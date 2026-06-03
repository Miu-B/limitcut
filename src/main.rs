mod cli;
mod config;
mod discord;
mod error;
mod ffmpeg;
mod instapost;
mod json_input;
mod overlap;

use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing::Level;

use cli::{default_output_filename, default_output_path, default_preview_path, Args, BlurRegion};
use config::{Config, DiscordVideoMode};
use discord::upload_instapost;
use error::LimitcutError;
use ffmpeg::{
    concat::{
        build_ffmpeg_command, build_single_input_ffmpeg_command, concatenate,
        generate_blur_preview, print_command, transcode_single, ConcatParams, SingleInputParams,
    },
    detect::{detect_best_encoder, EncoderConfig, FfmpegBinaries},
    probe::get_duration,
};
use instapost::{
    derive_output_path as derive_instapost_output_path, list_instapost_json_files,
    load_and_validate as load_and_validate_instapost, move_to_status_dir, write_failure_marker,
};
use json_input::{derive_output_path, load_and_validate};
use overlap::find_cut_point;

/// Maximum seconds of black-hold before the auto-trim kicks in.
///
/// When the user requests a `--black-hold` longer than this, the excess is
/// converted into a pre-input seek (`-ss`) so the output never starts with
/// more than `MAX_BLACK_HOLD` seconds of black screen.
const MAX_BLACK_HOLD: f64 = 4.0;
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
struct InstapostVideoEffects {
    fadein: f64,
    fadeout: f64,
    black_hold: f64,
}

fn instapost_video_effects() -> InstapostVideoEffects {
    InstapostVideoEffects {
        fadein: 0.0,
        fadeout: 0.0,
        black_hold: 0.0,
    }
}

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
            &args.json_dir,
            &args.instapost,
            &args.watch_instapost,
        ),
        (Some(_), Some(_), None, None, None, None)
            | (None, None, Some(_), None, None, None)
            | (None, None, None, Some(_), None, None)
            | (None, None, None, None, Some(_), None)
            | (None, None, None, None, None, Some(_))
    ) || (args.pre_video.is_some()
        && args.post_video.is_none()
        && args.json.is_none()
        && args.json_dir.is_none()
        && args.instapost.is_none()
        && args.watch_instapost.is_none()
        && args.preview_blur.is_some());

    if !valid {
        return Err(LimitcutError::InvalidInputMode(
            "provide PRE_VIDEO and POST_VIDEO (or use --preview-blur with just PRE_VIDEO), or --json, or --json-dir, or --instapost, or --watch-instapost",
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
        &args.instapost,
        &args.watch_instapost,
    ) {
        (Some(pre), None, None, None, None, None) => {
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
                    "provide PRE_VIDEO and POST_VIDEO (or use --preview-blur with just PRE_VIDEO), or --json, or --json-dir, or --instapost, or --watch-instapost",
                )
                .into())
            }
        }
        (Some(pre), Some(post), None, None, None, None) => {
            let output = resolve_normal_output_path(&args, &config, pre);
            let title = args.title.as_deref();
            run_single(pre, post, &output, title, &args, &config, &bins)
        }
        (None, None, Some(json_path), None, None, None) => {
            run_from_json(json_path, &args, &config, &bins, false)
        }
        (None, None, None, Some(json_dir), None, None) => {
            run_from_json_dir(json_dir, &args, &config, &bins)
        }
        (None, None, None, None, Some(instapost_path), None) => {
            run_from_instapost(instapost_path, &args, &config, &bins)
        }
        (None, None, None, None, None, Some(watch_dir)) => {
            watch_instapost_dir(watch_dir, &args, &config, &bins)
        }
        _ => Err(LimitcutError::InvalidInputMode(
            "provide PRE_VIDEO and POST_VIDEO, or --json, or --json-dir, or --instapost, or --watch-instapost",
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
            Some(user) => format!("{base}/{user}"),
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

fn run_from_instapost(
    json_path: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    match process_instapost(json_path, args, config, bins) {
        Ok(()) => {
            if instapost_should_consume(args) {
                move_instapost_to_done(json_path)?;
            }
            Ok(())
        }
        Err(err) => {
            if instapost_should_consume(args) {
                move_instapost_to_failed(json_path, &err)?;
            }
            Err(err)
        }
    }
}

fn process_instapost(
    json_path: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    let validated = load_and_validate_instapost(json_path)?;
    let output = resolve_instapost_output_path(args, config, &validated);

    tracing::info!(
        "Processing InstaPost {} → {}",
        validated.json_path.display(),
        output.display()
    );
    tracing::info!(
        "InstaPost video mode applies blur only; fade-in, fade-out, black-hold, and title overlay are ignored"
    );

    run_single_input(&validated.replay_buffer, &output, args, config, bins)?;

    if let Some(webhook_url) = instapost_discord_webhook(args, config) {
        tracing::info!(
            "Uploading InstaPost output to Discord webhook ({})",
            discord_video_mode_label(discord_video_mode(config))
        );
        upload_instapost(webhook_url, &output, &validated)?;
    }

    Ok(())
}

fn watch_instapost_dir(
    watch_dir: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    // Validate eagerly so user-facing errors are immediate.
    let _ = list_instapost_json_files(watch_dir)?;

    tracing::info!(
        "Watching InstaPost directory: {} (polling every {:.0}s)",
        watch_dir.display(),
        WATCH_POLL_INTERVAL.as_secs_f64()
    );

    loop {
        let pending = list_instapost_json_files(watch_dir)?;
        if pending.is_empty() {
            thread::sleep(WATCH_POLL_INTERVAL);
            continue;
        }

        for json_path in pending {
            handle_watched_instapost(&json_path, args, config, bins)?;
        }
    }
}

fn handle_watched_instapost(
    json_path: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    match process_instapost(json_path, args, config, bins) {
        Ok(()) => {
            if instapost_should_consume(args) {
                move_instapost_to_done(json_path)?;
            }
        }
        Err(err) => {
            eprintln!("Failed {}: {:#}", json_path.display(), err);
            if instapost_should_consume(args) {
                move_instapost_to_failed(json_path, &err)?;
            }
        }
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

    ensure_output_path(output, args.overwrite || config.overwrite.unwrap_or(false))?;

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

fn run_single_input(
    input_video: &Path,
    output: &Path,
    args: &Args,
    config: &Config,
    bins: &FfmpegBinaries,
) -> anyhow::Result<()> {
    validate_video_input(input_video)?;

    let blurs = resolve_blurs(args, config);

    if let Some(timestamp) = args.preview_blur {
        if blurs.is_empty() {
            return Err(LimitcutError::PreviewBlurWithoutRegions.into());
        }

        let preview_path = default_preview_path(input_video);
        tracing::info!(
            "Generating blur preview at {:.1}s → {}",
            timestamp,
            preview_path.display()
        );

        generate_blur_preview(&bins.ffmpeg, input_video, &blurs, timestamp, &preview_path)?;
        println!("Blur preview saved: {}", preview_path.display());
        return Ok(());
    }

    ensure_output_path(output, args.overwrite || config.overwrite.unwrap_or(false))?;

    let video_mode = discord_video_mode(config);
    let encoder = resolve_instapost_encoder(args, config, video_mode);
    let effects = instapost_video_effects();

    let input_duration = get_duration(&bins.ffprobe, input_video).unwrap_or(0.0);
    let input_seek = 0.0;
    let estimated_total = input_duration.max(0.0);

    tracing::info!(
        "Discord video mode: {}",
        discord_video_mode_label(video_mode)
    );
    tracing::info!("Using encoder: {}", encoder.display_name);

    tracing::info!(
        "Processing single input {} → {} (estimated {:.0}s)",
        input_video.display(),
        output.display(),
        estimated_total
    );

    let params = SingleInputParams {
        ffmpeg: &bins.ffmpeg,
        input: input_video,
        output,
        estimated_total_secs: estimated_total,
        encoder: &encoder,
        blurs: &blurs,
        fadein: effects.fadein,
        fadeout: effects.fadeout,
        black_hold: effects.black_hold,
        title: None,
        input_seek_secs: input_seek,
        output_height: Some(discord_output_height(video_mode)),
    };

    if args.dry_run {
        let cmd = build_single_input_ffmpeg_command(&params);
        print_command(&cmd);
        return Ok(());
    }

    let transcode_result = transcode_single(&params)?;
    let out_duration = get_duration(&bins.ffprobe, output).unwrap_or(0.0);

    println!(
        "Done. Output: {} ({:.0}:{:02.0} min, encoded in {:.1}s)",
        output.display(),
        (out_duration / 60.0).floor(),
        out_duration % 60.0,
        transcode_result.encode_time.as_secs_f64(),
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

fn resolve_instapost_output_path(
    args: &Args,
    config: &Config,
    validated: &instapost::ValidatedInstapostJson,
) -> PathBuf {
    if let Some(ref explicit) = args.output {
        return explicit.clone();
    }

    let root = if let Some(ref dir) = args.output_dir {
        dir.clone()
    } else if let Some(ref dir) = config.output_dir {
        dir.clone()
    } else {
        validated
            .json_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };

    derive_instapost_output_path(validated, &root)
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

fn resolve_instapost_encoder(
    args: &Args,
    config: &Config,
    video_mode: DiscordVideoMode,
) -> EncoderConfig {
    if args.encoder.is_some() || config.encoder.is_some() {
        tracing::warn!(
            "InstaPost ignores encoder overrides and uses a Discord-oriented encode profile"
        );
    }

    match video_mode {
        DiscordVideoMode::P1080 => EncoderConfig::discord_1080p(),
        DiscordVideoMode::P720 => EncoderConfig::discord_720p(),
    }
}

fn discord_webhook_url(config: &Config) -> Option<&str> {
    if config.discord_enabled.unwrap_or(false) {
        config.discord_webhook_url.as_deref()
    } else {
        None
    }
}

fn instapost_should_consume(args: &Args) -> bool {
    args.preview_blur.is_none() && !args.dry_run
}

fn move_instapost_to_done(json_path: &Path) -> anyhow::Result<()> {
    let moved = move_to_status_dir(json_path, "done")?;
    tracing::info!("Moved descriptor to {}", moved.display());
    Ok(())
}

fn move_instapost_to_failed(json_path: &Path, err: &anyhow::Error) -> anyhow::Result<()> {
    let moved = move_to_status_dir(json_path, "failed")
        .with_context(|| format!("Failed to move {} into failed/", json_path.display()))?;
    let marker = write_failure_marker(&moved, err)?;
    tracing::warn!(
        "Moved failed descriptor to {} and wrote {}",
        moved.display(),
        marker.display()
    );
    Ok(())
}

fn discord_video_mode(config: &Config) -> DiscordVideoMode {
    config.discord_video_mode.unwrap_or(DiscordVideoMode::P720)
}

fn discord_video_mode_label(mode: DiscordVideoMode) -> &'static str {
    match mode {
        DiscordVideoMode::P1080 => "1080p",
        DiscordVideoMode::P720 => "720p",
    }
}

fn discord_output_height(mode: DiscordVideoMode) -> u32 {
    match mode {
        DiscordVideoMode::P1080 => 1080,
        DiscordVideoMode::P720 => 720,
    }
}

fn instapost_discord_webhook<'a>(args: &Args, config: &'a Config) -> Option<&'a str> {
    if args.preview_blur.is_some() || args.dry_run {
        None
    } else {
        discord_webhook_url(config)
    }
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
        | LimitcutError::InstapostWatchDirNotFound(_)
        | LimitcutError::InstapostWatchDirNotADir(_)
        | LimitcutError::NoJsonFilesFound(_)
        | LimitcutError::JsonDirReadFailed { .. }
        | LimitcutError::InstapostWatchDirReadFailed { .. }
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

    fn instapost_args() -> Args {
        Args::parse_from(["limitcut", "--instapost", "instapost.json"])
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
    fn resolve_instapost_output_path_prefers_explicit_then_cli_dir_then_config_then_json_parent() {
        let validated = instapost::ValidatedInstapostJson {
            json_path: PathBuf::from("meta/instapost.json"),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-06-02T19:07:02+02:00").unwrap(),
            replay_buffer: PathBuf::from("meta/Replay.mkv"),
            job: Some("RPR".to_owned()),
            encounter: Some("Solution Nine".to_owned()),
            territory_name: None,
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: None,
        };

        let args = Args::parse_from([
            "limitcut",
            "--instapost",
            "meta/instapost.json",
            "-o",
            "out.mp4",
        ]);
        assert_eq!(
            resolve_instapost_output_path(&args, &Config::default(), &validated),
            PathBuf::from("out.mp4")
        );

        let args = Args::parse_from([
            "limitcut",
            "--instapost",
            "meta/instapost.json",
            "--output-dir",
            "/tmp/out",
        ]);
        assert_eq!(
            resolve_instapost_output_path(&args, &Config::default(), &validated),
            PathBuf::from("/tmp/out/instapost/2026-06-02/Solution_Nine/RPR/19-07-02.mp4")
        );

        let cfg = Config {
            output_dir: Some(PathBuf::from("/cfg/out")),
            ..Config::default()
        };
        assert_eq!(
            resolve_instapost_output_path(&instapost_args(), &cfg, &validated),
            PathBuf::from("/cfg/out/instapost/2026-06-02/Solution_Nine/RPR/19-07-02.mp4")
        );

        assert_eq!(
            resolve_instapost_output_path(&instapost_args(), &Config::default(), &validated),
            PathBuf::from("meta/instapost/2026-06-02/Solution_Nine/RPR/19-07-02.mp4")
        );
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
            instapost: None,
            watch_instapost: None,
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
            instapost: None,
            watch_instapost: None,
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
            instapost: None,
            watch_instapost: None,
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
    fn exit_code_marks_instapost_watch_validation_as_user_error() {
        let err = anyhow::Error::new(LimitcutError::InstapostWatchDirNotFound(PathBuf::from(
            "missing-dir",
        )));
        assert_eq!(exit_code(&err), 1);
    }

    #[test]
    fn exit_code_marks_processing_failures_as_processing_error() {
        let err = anyhow::Error::new(LimitcutError::ConcatFailed {
            stderr: "boom".to_owned(),
        });
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn discord_webhook_url_requires_enable_flag() {
        let disabled = Config {
            discord_webhook_url: Some("https://discord.example/test".to_owned()),
            discord_enabled: Some(false),
            ..Config::default()
        };
        assert!(discord_webhook_url(&disabled).is_none());

        let enabled = Config {
            discord_webhook_url: Some("https://discord.example/test".to_owned()),
            discord_enabled: Some(true),
            ..Config::default()
        };
        assert_eq!(
            discord_webhook_url(&enabled),
            Some("https://discord.example/test")
        );
    }

    #[test]
    fn discord_video_mode_defaults_to_720p() {
        assert_eq!(discord_video_mode(&Config::default()), DiscordVideoMode::P720);

        let config = Config {
            discord_video_mode: Some(DiscordVideoMode::P1080),
            ..Config::default()
        };
        assert_eq!(discord_video_mode(&config), DiscordVideoMode::P1080);
        assert_eq!(discord_video_mode_label(DiscordVideoMode::P720), "720p");
        assert_eq!(discord_output_height(DiscordVideoMode::P1080), 1080);
    }

    #[test]
    fn instapost_encoder_uses_discord_profile() {
        let default = resolve_instapost_encoder(&instapost_args(), &Config::default(), DiscordVideoMode::P720);
        assert_eq!(default.name, "libx264");
        assert_eq!(default.display_name, "libx264 (Discord 720p profile)");

        let config = Config {
            encoder: Some("h264_nvenc".to_owned()),
            ..Config::default()
        };
        let with_override = resolve_instapost_encoder(&instapost_args(), &config, DiscordVideoMode::P1080);
        assert_eq!(with_override.name, "libx264");
        assert_eq!(with_override.display_name, "libx264 (Discord 1080p profile)");
    }

    #[test]
    fn instapost_video_effects_disable_fades_and_black_hold() {
        assert_eq!(
            instapost_video_effects(),
            InstapostVideoEffects {
                fadein: 0.0,
                fadeout: 0.0,
                black_hold: 0.0,
            }
        );
    }

    #[test]
    fn instapost_discord_upload_skips_preview_and_dry_run() {
        let config = Config {
            discord_webhook_url: Some("https://discord.example/test".to_owned()),
            discord_enabled: Some(true),
            ..Config::default()
        };

        let preview_args = Args::parse_from([
            "limitcut",
            "--instapost",
            "instapost.json",
            "--blur",
            "0:0:10:10",
            "--preview-blur",
        ]);
        assert!(instapost_discord_webhook(&preview_args, &config).is_none());

        let dry_run_args = Args::parse_from(["limitcut", "--instapost", "instapost.json", "--dry-run"]);
        assert!(instapost_discord_webhook(&dry_run_args, &config).is_none());

        let normal_args = instapost_args();
        assert_eq!(
            instapost_discord_webhook(&normal_args, &config),
            Some("https://discord.example/test")
        );
    }

    #[test]
    fn instapost_consumption_skips_preview_and_dry_run() {
        let preview_args = Args::parse_from([
            "limitcut",
            "--instapost",
            "instapost.json",
            "--blur",
            "0:0:10:10",
            "--preview-blur",
        ]);
        assert!(!instapost_should_consume(&preview_args));

        let dry_run_args = Args::parse_from(["limitcut", "--instapost", "instapost.json", "--dry-run"]);
        assert!(!instapost_should_consume(&dry_run_args));

        assert!(instapost_should_consume(&instapost_args()));
    }
}
