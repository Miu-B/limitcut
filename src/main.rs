mod cli;
mod config;
mod error;
mod ffmpeg;
mod json_input;
mod overlap;

use std::path::{Path, PathBuf};
use std::process;

use anyhow::Context;
use clap::Parser;
use tracing::Level;

use cli::{default_output_filename, default_output_path, default_preview_path, Args, BlurRegion};
use config::Config;
use error::LimitcutError;
use ffmpeg::{
    concat::{
        build_ffmpeg_command, concatenate, generate_blur_preview, print_command, ConcatParams,
    },
    detect::{detect_best_encoder, EncoderConfig, FfmpegBinaries},
    probe::get_duration,
};
use json_input::{derive_output_path, load_and_validate};
use overlap::find_cut_point;

/// Maximum seconds of black-hold before the auto-trim kicks in.
///
/// When the user requests a `--black-hold` longer than this, the excess is
/// converted into a pre-input seek (`-ss`) so the output never starts with
/// more than `MAX_BLACK_HOLD` seconds of black screen.
const MAX_BLACK_HOLD: f64 = 4.0;

fn main() {
    let args = Args::parse();

    let log_level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .without_time()
        .init();

    match run(args) {
        Ok(()) => {}
        Err(e) => {
            let code = exit_code(&e);
            eprintln!("error: {:#}", e);
            process::exit(code);
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let valid = matches!(
        (
            &args.pre_video,
            &args.post_video,
            &args.json,
            &args.json_dir
        ),
        (Some(_), Some(_), None, None) | (None, None, Some(_), None) | (None, None, None, Some(_))
    ) || (args.pre_video.is_some()
        && args.post_video.is_none()
        && args.json.is_none()
        && args.json_dir.is_none()
        && args.preview_blur.is_some());

    if !valid {
        return Err(LimitcutError::InvalidInputMode(
            "provide PRE_VIDEO and POST_VIDEO (or use --preview-blur with just PRE_VIDEO), or --json, or --json-dir",
        )
        .into());
    }

    let config = Config::load()?;
    let bins = FfmpegBinaries::locate()?;
    tracing::debug!("ffmpeg:  {}", bins.ffmpeg.display());
    tracing::debug!("ffprobe: {}", bins.ffprobe.display());

    match (
        &args.pre_video,
        &args.post_video,
        &args.json,
        &args.json_dir,
    ) {
        (Some(pre), None, None, None) => {
            if let Some(timestamp) = args.preview_blur {
                validate_video_input(pre)?;
                let blurs = resolve_blurs(&args, &config);
                if blurs.is_empty() {
                    return Err(LimitcutError::PreviewBlurWithoutRegions.into());
                }
                let preview_path = default_preview_path(pre);
                generate_blur_preview(&bins.ffmpeg, pre, &blurs, timestamp, &preview_path)?;
                println!("Blur preview saved: {}", preview_path.display());
                Ok(())
            } else {
                Err(LimitcutError::InvalidInputMode(
                    "provide PRE_VIDEO and POST_VIDEO (or use --preview-blur with just PRE_VIDEO), or --json, or --json-dir",
                )
                .into())
            }
        }
        (Some(pre), Some(post), None, None) => {
            let output = resolve_normal_output_path(&args, &config, pre);
            let title = args.title.as_deref();
            run_single(pre, post, &output, title, &args, &config, &bins)
        }
        (None, None, Some(json_path), None) => {
            run_from_json(json_path, &args, &config, &bins, false)
        }
        (None, None, None, Some(json_dir)) => run_from_json_dir(json_dir, &args, &config, &bins),
        _ => Err(LimitcutError::InvalidInputMode(
            "provide either PRE_VIDEO POST_VIDEO, or --json, or --json-dir",
        )
        .into()),
    }
}

fn run_from_json(
    json_path: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
    auto_title: bool,
) -> anyhow::Result<()> {
    let validated = load_and_validate(json_path)?;
    let base_output_dir = resolve_json_output_root(args, config, json_path);
    let output = derive_output_path(&validated, &base_output_dir);

    let generated_title;
    let title = if auto_title {
        let base = if let Some(ref job) = validated.job {
            format!("{}/{} POV", validated.encounter, job)
        } else {
            validated.encounter.clone()
        };
        generated_title = match args.title.as_deref() {
            Some(user) => format!("{}/{}", base, user),
            None => base,
        };
        Some(generated_title.as_str())
    } else {
        args.title.as_deref().or(Some(validated.encounter.as_str()))
    };

    tracing::info!(
        "Processing JSON {} → {}",
        validated.json_path.display(),
        output.display()
    );

    run_single(
        &validated.replay_buffer,
        &validated.recording,
        &output,
        title,
        args,
        config,
        bins,
    )
}

fn run_from_json_dir(
    json_dir: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    if !json_dir.exists() {
        return Err(LimitcutError::JsonDirNotFound(json_dir.to_path_buf()).into());
    }
    if !json_dir.is_dir() {
        return Err(LimitcutError::JsonDirNotADir(json_dir.to_path_buf()).into());
    }

    let mut json_files: Vec<PathBuf> = std::fs::read_dir(json_dir)
        .map_err(|source| LimitcutError::JsonDirReadFailed {
            path: json_dir.to_path_buf(),
            source,
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();

    json_files.sort();

    if json_files.is_empty() {
        return Err(LimitcutError::NoJsonFilesFound(json_dir.to_path_buf()).into());
    }

    let mut succeeded = 0usize;
    let mut failures: Vec<(PathBuf, anyhow::Error)> = Vec::new();

    for json_path in json_files {
        match run_from_json(&json_path, args, config, bins, true) {
            Ok(()) => succeeded += 1,
            Err(err) => {
                eprintln!("Skipping {}: {:#}", json_path.display(), err);
                failures.push((json_path, err));
            }
        }
    }

    println!(
        "Batch complete: {}/{} succeeded",
        succeeded,
        succeeded + failures.len()
    );

    if failures.is_empty() {
        return Ok(());
    }

    for (path, err) in &failures {
        eprintln!("Failed {}: {:#}", path.display(), err);
    }

    Ok(())
}

fn run_single(
    pre_video: &Path,
    post_video: &Path,
    output: &Path,
    title: Option<&str>,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    validate_video_input(pre_video)?;
    validate_video_input(post_video)?;

    ensure_output_path(output, args.overwrite || config.overwrite.unwrap_or(false))?;

    let blurs = resolve_blurs(args, config);

    if let Some(timestamp) = args.preview_blur {
        if blurs.is_empty() {
            return Err(LimitcutError::PreviewBlurWithoutRegions.into());
        }

        let preview_path = default_preview_path(pre_video);
        tracing::info!(
            "Generating blur preview at {:.1}s → {}",
            timestamp,
            preview_path.display()
        );

        generate_blur_preview(&bins.ffmpeg, pre_video, &blurs, timestamp, &preview_path)?;
        println!("Blur preview saved: {}", preview_path.display());
        return Ok(());
    }

    let encoder = resolve_encoder(args, config, bins)?;

    tracing::info!(
        "Analysing audio overlap between '{}' and '{}'…",
        pre_video.display(),
        post_video.display()
    );

    let result =
        find_cut_point(&bins.ffmpeg, &bins.ffprobe, pre_video, post_video).map_err(|e| {
            anyhow::anyhow!(
                "{}\n\nHint: make sure both videos share overlapping audio. \
                 The end of PRE_VIDEO and the start of POST_VIDEO must cover \
                 the same audio segment.",
                e
            )
        })?;

    tracing::info!(
        "Cut point detected: {:.3}s (correlation score: {:.4}, pre-video duration: {:.3}s)",
        result.cut_point_secs,
        result.score,
        result.pre_duration_secs,
    );

    let fadein = args.fadein.or(config.fadein).unwrap_or(1.0);
    let fadeout = args.fadeout.or(config.fadeout).unwrap_or(1.0);
    let black_hold = args.black_hold.or(config.black_hold).unwrap_or(0.0);

    if black_hold > result.cut_point_secs {
        return Err(LimitcutError::BlackHoldExceedsCutPoint {
            hold: black_hold,
            cut: result.cut_point_secs,
        }
        .into());
    }

    let (pre_seek, effective_hold) = if black_hold > MAX_BLACK_HOLD {
        let excess = black_hold - MAX_BLACK_HOLD;
        tracing::info!(
            "Auto-trimming black hold: {:.1}s → {:.1}s (seeking {:.1}s into pre-video)",
            black_hold,
            MAX_BLACK_HOLD,
            excess,
        );
        (excess, MAX_BLACK_HOLD)
    } else {
        (0.0, black_hold)
    };

    let post_duration = get_duration(&bins.ffprobe, post_video).unwrap_or(0.0);
    let estimated_total = (result.cut_point_secs - pre_seek) + post_duration;

    tracing::info!(
        "Output: {} (estimated {:.0}s)",
        output.display(),
        estimated_total
    );

    let params = ConcatParams {
        ffmpeg: &bins.ffmpeg,
        pre: pre_video,
        post: post_video,
        output,
        cut_point_secs: result.cut_point_secs,
        estimated_total_secs: estimated_total,
        encoder: &encoder,
        blurs: &blurs,
        fadein,
        fadeout,
        black_hold: effective_hold,
        title,
        pre_seek_secs: pre_seek,
    };

    if args.dry_run {
        let cmd = build_ffmpeg_command(&params);
        print_command(&cmd);
        return Ok(());
    }

    let concat_result = concatenate(&params)?;
    let out_duration = get_duration(&bins.ffprobe, output).unwrap_or(0.0);

    println!(
        "Done. Output: {} ({:.0}:{:02.0} min, encoded in {:.1}s)",
        output.display(),
        (out_duration / 60.0).floor(),
        out_duration % 60.0,
        concat_result.encode_time.as_secs_f64(),
    );

    Ok(())
}

fn validate_video_input(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Err(LimitcutError::InputNotFound(path.to_path_buf()).into());
    }
    if !path.is_file() {
        return Err(LimitcutError::InputNotAFile(path.to_path_buf()).into());
    }
    Ok(())
}

fn ensure_output_path(output: &Path, overwrite: bool) -> anyhow::Result<()> {
    if output.exists() && !overwrite {
        return Err(LimitcutError::OutputExists(output.to_path_buf()).into());
    }

    if let Some(parent) = output.parent() {
        if parent.exists() && !parent.is_dir() {
            return Err(LimitcutError::OutputDirNotADirectory(parent.to_path_buf()).into());
        }

        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create output directory: {}", parent.display())
            })?;
        }
    }

    Ok(())
}

fn resolve_normal_output_path(args: &Args, config: &Config, pre_video: &Path) -> PathBuf {
    if let Some(ref explicit) = args.output {
        return explicit.clone();
    }

    if let Some(ref dir) = args.output_dir {
        return dir.join(default_output_filename(pre_video));
    }

    if let Some(ref dir) = config.output_dir {
        return dir.join(default_output_filename(pre_video));
    }

    default_output_path(pre_video)
}

fn resolve_json_output_root(args: &Args, config: &Config, json_path: &Path) -> PathBuf {
    if let Some(ref dir) = args.output_dir {
        return dir.clone();
    }

    if let Some(ref dir) = config.output_dir {
        return dir.clone();
    }

    json_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn resolve_blurs(args: &Args, config: &Config) -> Vec<BlurRegion> {
    if args.blur.is_empty() {
        config.blur.clone()
    } else {
        args.blur.clone()
    }
}

fn resolve_encoder(
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<EncoderConfig> {
    let encoder_name = args.encoder.clone().or(config.encoder.clone());
    let encoder = match &encoder_name {
        Some(name) => {
            let enc = EncoderConfig::from_name(name);
            tracing::info!("Using encoder: {} (user override)", enc.display_name);
            enc
        }
        None => {
            let enc = detect_best_encoder(&bins.ffmpeg)?;
            tracing::info!("Using encoder: {} (auto-detected)", enc.display_name);
            enc
        }
    };

    Ok(encoder)
}

/// Map an error to a shell exit code.
///
/// 1 — invalid user input (bad paths, bad args)
/// 2 — processing failure (ffmpeg error, correlation failure)
fn exit_code(err: &anyhow::Error) -> i32 {
    if let Some(
        LimitcutError::ConfigParseError { .. }
        | LimitcutError::InputNotFound(_)
        | LimitcutError::InputNotAFile(_)
        | LimitcutError::OutputExists(_)
        | LimitcutError::OutputDirNotADirectory(_)
        | LimitcutError::InvalidInputMode(_)
        | LimitcutError::InvalidBlurRegion { .. }
        | LimitcutError::PreviewBlurWithoutRegions
        | LimitcutError::BlackHoldExceedsCutPoint { .. }
        | LimitcutError::JsonNotFound(_)
        | LimitcutError::JsonNotAFile(_)
        | LimitcutError::JsonDirNotFound(_)
        | LimitcutError::JsonDirNotADir(_)
        | LimitcutError::NoJsonFilesFound(_)
        | LimitcutError::JsonDirReadFailed { .. }
        | LimitcutError::JsonParseFailed { .. }
        | LimitcutError::JsonMissingField { .. }
        | LimitcutError::JsonEmptyField { .. }
        | LimitcutError::JsonInvalidDatetime { .. }
        | LimitcutError::JsonVideoNotFound { .. }
        | LimitcutError::JsonEncounterNameEmpty { .. },
    ) = err.downcast_ref::<LimitcutError>()
    {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn base_args() -> Args {
        Args::parse_from(["limitcut", "--json", "pull.json"])
    }

    #[test]
    fn resolve_normal_output_path_uses_cli_output_dir() {
        let args = Args::parse_from([
            "limitcut",
            "pre.mkv",
            "post.mkv",
            "--output-dir",
            "/tmp/out",
        ]);
        let output = resolve_normal_output_path(&args, &Config::default(), Path::new("pre.mkv"));
        assert_eq!(output, PathBuf::from("/tmp/out/pre_combined.mp4"));
    }

    #[test]
    fn resolve_normal_output_path_prefers_explicit_output() {
        let args = Args::parse_from(["limitcut", "pre.mkv", "post.mkv", "-o", "out.mp4"]);
        let output = resolve_normal_output_path(&args, &Config::default(), Path::new("pre.mkv"));
        assert_eq!(output, PathBuf::from("out.mp4"));
    }

    #[test]
    fn resolve_json_output_root_prefers_cli_dir_then_config_then_json_parent() {
        let args = Args::parse_from([
            "limitcut",
            "--json",
            "meta/pull.json",
            "--output-dir",
            "/tmp/out",
        ]);
        let root = resolve_json_output_root(&args, &Config::default(), Path::new("meta/pull.json"));
        assert_eq!(root, PathBuf::from("/tmp/out"));

        let cfg = Config {
            output_dir: Some(PathBuf::from("/cfg/out")),
            ..Config::default()
        };
        let root = resolve_json_output_root(&base_args(), &cfg, Path::new("meta/pull.json"));
        assert_eq!(root, PathBuf::from("/cfg/out"));

        let root = resolve_json_output_root(
            &base_args(),
            &Config::default(),
            Path::new("meta/pull.json"),
        );
        assert_eq!(root, PathBuf::from("meta"));
    }

    #[test]
    fn ensure_output_path_rejects_parent_file() {
        let dir = tempdir().unwrap();
        let parent_file = dir.path().join("not_a_dir");
        std::fs::write(&parent_file, "x").unwrap();

        let err = ensure_output_path(&parent_file.join("out.mp4"), false).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<LimitcutError>(),
            Some(LimitcutError::OutputDirNotADirectory(_))
        ));
    }

    #[test]
    fn validate_video_input_rejects_missing_file() {
        let err = validate_video_input(Path::new("/does/not/exist.mkv")).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<LimitcutError>(),
            Some(LimitcutError::InputNotFound(_))
        ));
    }

    #[test]
    fn run_rejects_invalid_mixed_input_modes() {
        let args = Args {
            pre_video: Some(PathBuf::from("pre.mkv")),
            post_video: Some(PathBuf::from("post.mkv")),
            json: Some(PathBuf::from("pull.json")),
            json_dir: None,
            output: None,
            output_dir: None,
            overwrite: false,
            encoder: None,
            blur: Vec::new(),
            preview_blur: None,
            dry_run: false,
            fadein: None,
            fadeout: None,
            black_hold: None,
            title: None,
            verbose: false,
        };

        let err = run(args).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<LimitcutError>(),
            Some(LimitcutError::InvalidInputMode(_))
        ));
    }

    #[test]
    fn run_rejects_single_video_without_preview_blur() {
        let args = Args {
            pre_video: Some(PathBuf::from("pre.mkv")),
            post_video: None,
            json: None,
            json_dir: None,
            output: None,
            output_dir: None,
            overwrite: false,
            encoder: None,
            blur: vec![],
            preview_blur: None,
            dry_run: false,
            fadein: None,
            fadeout: None,
            black_hold: None,
            title: None,
            verbose: false,
        };

        let err = run(args).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<LimitcutError>(),
            Some(LimitcutError::InvalidInputMode(_))
        ));
    }

    #[test]
    fn run_preview_blur_single_video_enters_preview_path() {
        let args = Args {
            pre_video: Some(PathBuf::from("pre.mkv")),
            post_video: None,
            json: None,
            json_dir: None,
            output: None,
            output_dir: None,
            overwrite: false,
            encoder: None,
            blur: vec![BlurRegion {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }],
            preview_blur: Some(1.0),
            dry_run: false,
            fadein: None,
            fadeout: None,
            black_hold: None,
            title: None,
            verbose: false,
        };

        let err = run(args).unwrap_err();
        assert!(!matches!(
            err.downcast_ref::<LimitcutError>(),
            Some(LimitcutError::InvalidInputMode(_))
        ));
    }

    #[test]
    fn exit_code_marks_json_validation_as_user_error() {
        let err = anyhow::Error::new(LimitcutError::JsonNotFound(PathBuf::from("missing.json")));
        assert_eq!(exit_code(&err), 1);
    }

    #[test]
    fn exit_code_marks_processing_failures_as_processing_error() {
        let err = anyhow::Error::new(LimitcutError::ConcatFailed {
            stderr: "boom".to_owned(),
        });
        assert_eq!(exit_code(&err), 2);
    }
}
