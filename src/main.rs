mod cli;
mod error;
mod ffmpeg;
mod overlap;

use std::process;

use anyhow::Context;
use clap::Parser;
use tracing::Level;

use cli::{default_output_path, default_preview_path, Args};
use error::LimitcutError;
use ffmpeg::{
    concat::{
        build_ffmpeg_command, concatenate, generate_blur_preview, print_command, ConcatParams,
    },
    detect::{detect_best_encoder, EncoderConfig, FfmpegBinaries},
    probe::get_duration,
};
use overlap::find_cut_point;

fn main() {
    let args = Args::parse();

    // Initialise tracing subscriber
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
            // Use exit code 1 for user/input errors, 2 for processing errors
            let code = exit_code(&e);
            eprintln!("error: {:#}", e);
            process::exit(code);
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    // ── 1. Validate inputs ────────────────────────────────────────────────

    if !args.pre_video.exists() {
        return Err(LimitcutError::InputNotFound(args.pre_video).into());
    }
    if !args.pre_video.is_file() {
        return Err(LimitcutError::InputNotAFile(args.pre_video).into());
    }
    if !args.post_video.exists() {
        return Err(LimitcutError::InputNotFound(args.post_video).into());
    }
    if !args.post_video.is_file() {
        return Err(LimitcutError::InputNotAFile(args.post_video).into());
    }

    let output = args
        .output
        .unwrap_or_else(|| default_output_path(&args.pre_video));

    if output.exists() && !args.overwrite {
        return Err(LimitcutError::OutputExists(output).into());
    }

    // Ensure the output directory exists
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create output directory: {}", parent.display())
            })?;
        }
    }

    // ── 2. Locate ffmpeg / ffprobe ────────────────────────────────────────

    let bins = FfmpegBinaries::locate()?;
    tracing::debug!("ffmpeg:  {}", bins.ffmpeg.display());
    tracing::debug!("ffprobe: {}", bins.ffprobe.display());

    // ── 2b. Blur preview (early exit) ─────────────────────────────────────

    if let Some(timestamp) = args.preview_blur {
        if args.blur.is_empty() {
            return Err(LimitcutError::PreviewBlurWithoutRegions.into());
        }

        let preview_path = default_preview_path(&args.pre_video);
        tracing::info!(
            "Generating blur preview at {:.1}s → {}",
            timestamp,
            preview_path.display()
        );

        generate_blur_preview(
            &bins.ffmpeg,
            &args.pre_video,
            &args.blur,
            timestamp,
            &preview_path,
        )?;

        println!("Blur preview saved: {}", preview_path.display());
        return Ok(());
    }

    // ── 3. Select encoder ─────────────────────────────────────────────────

    let encoder: EncoderConfig = match &args.encoder {
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

    // ── 4. Find the cut point via audio cross-correlation ─────────────────

    tracing::info!(
        "Analysing audio overlap between '{}' and '{}'…",
        args.pre_video.display(),
        args.post_video.display()
    );

    let result = find_cut_point(
        &bins.ffmpeg,
        &bins.ffprobe,
        &args.pre_video,
        &args.post_video,
    )
    .map_err(|e| {
        // Surface correlation errors with extra context
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

    // ── 5. Build and run the ffmpeg concat command ────────────────────────

    // Resolve fade parameters (always applied, defaults to 1.0s)
    let fadein = args.fadein.unwrap_or(1.0);
    let fadeout = args.fadeout.unwrap_or(1.0);
    let black_hold = args.black_hold.unwrap_or(0.0);

    // Validate: black_hold must fit within the pre-video (cut point)
    if black_hold > result.cut_point_secs {
        return Err(LimitcutError::BlackHoldExceedsCutPoint {
            hold: black_hold,
            cut: result.cut_point_secs,
        }
        .into());
    }

    // Total output duration ≈ pre cut point + full post-video length
    let post_duration = get_duration(&bins.ffprobe, &args.post_video).unwrap_or(0.0);
    let estimated_total = result.cut_point_secs + post_duration;

    tracing::info!(
        "Output: {} (estimated {:.0}s)",
        output.display(),
        estimated_total
    );

    let params = ConcatParams {
        ffmpeg: &bins.ffmpeg,
        pre: &args.pre_video,
        post: &args.post_video,
        output: &output,
        cut_point_secs: result.cut_point_secs,
        estimated_total_secs: estimated_total,
        encoder: &encoder,
        blurs: &args.blur,
        fadein,
        fadeout,
        black_hold,
        title: args.title.as_deref(),
    };

    if args.dry_run {
        let cmd = build_ffmpeg_command(&params);
        print_command(&cmd);
        return Ok(());
    }

    let concat_result = concatenate(&params)?;

    // ── 6. Report ─────────────────────────────────────────────────────────

    // Probe actual output duration
    let out_duration = get_duration(&bins.ffprobe, &output).unwrap_or(0.0);

    println!(
        "Done. Output: {} ({:.0}:{:02.0} min, encoded in {:.1}s)",
        output.display(),
        (out_duration / 60.0).floor(),
        out_duration % 60.0,
        concat_result.encode_time.as_secs_f64(),
    );

    Ok(())
}

/// Map an error to a shell exit code.
///
/// 1 — invalid user input (bad paths, bad args)
/// 2 — processing failure (ffmpeg error, correlation failure)
fn exit_code(err: &anyhow::Error) -> i32 {
    if let Some(
        LimitcutError::InputNotFound(_)
        | LimitcutError::InputNotAFile(_)
        | LimitcutError::OutputExists(_)
        | LimitcutError::InvalidBlurRegion { .. }
        | LimitcutError::PreviewBlurWithoutRegions
        | LimitcutError::BlackHoldExceedsCutPoint { .. },
    ) = err.downcast_ref::<LimitcutError>()
    {
        1
    } else {
        2
    }
}
