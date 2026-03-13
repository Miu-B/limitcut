use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;

use crate::error::{LimitcutError, Result};

/// Limitcut — seamlessly combine two overlapping video recordings into one MP4.
///
/// Given a PRE_VIDEO (e.g. a replay buffer clip) and a POST_VIDEO (e.g. the
/// full encounter recording), limitcut uses audio cross-correlation to find
/// the exact overlap point, then uses ffmpeg to produce a single seamless MP4.
///
/// Both videos must share a common audio segment — the end of PRE_VIDEO and
/// the start of POST_VIDEO must overlap. limitcut will error if no audio
/// overlap is detected.
#[derive(Debug, Parser)]
#[command(
    name = "limitcut",
    version,
    author,
    about,
    long_about = None,
    after_help = "EXAMPLES:\n  limitcut prepull.mkv pull.mkv\n  limitcut prepull.mkv pull.mkv -o combined.mp4\n  limitcut prepull.mkv pull.mkv --blur 0:840:480:200 --blur 1400:0:480:60\n  limitcut prepull.mkv pull.mkv --blur 0:840:480:200 --preview-blur\n  limitcut prepull.mkv pull.mkv --encoder libx264 --dry-run"
)]
pub struct Args {
    /// The first recording (will be trimmed at the detected cut point).
    ///
    /// This is the shorter clip that contains footage *before* the main event
    /// starts — e.g. a replay buffer clip saved just before a boss pull.
    pub pre_video: PathBuf,

    /// The second recording (appended in full after the cut point).
    ///
    /// This is the main recording that starts slightly before the cut point
    /// and continues through the entire event.
    pub post_video: PathBuf,

    /// Output file path.
    ///
    /// Defaults to the pre-video filename with `_combined.mp4` appended in
    /// the same directory as the pre-video.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Overwrite the output file if it already exists.
    #[arg(long, default_value_t = false)]
    pub overwrite: bool,

    /// H.264 encoder to use for the output.
    ///
    /// If not specified, limitcut auto-detects the best available encoder in
    /// this order: h264_nvenc (NVIDIA) → h264_vaapi (AMD/Intel) →
    /// h264_videotoolbox (macOS) → libx264 (CPU fallback).
    ///
    /// Valid values: nvenc, vaapi, videotoolbox, libx264
    #[arg(long, value_name = "ENCODER", value_parser = parse_encoder_name)]
    pub encoder: Option<String>,

    /// Blur a rectangular region of the output video.
    ///
    /// Format: x:y:width:height (pixel coordinates, top-left origin).
    /// This flag can be repeated to blur multiple regions.
    ///
    /// Example: --blur 0:840:480:200 --blur 1400:0:480:60
    #[arg(long, value_name = "x:y:w:h", value_parser = BlurRegion::from_str)]
    pub blur: Vec<BlurRegion>,

    /// Generate a single JPEG frame with blur regions applied, then exit.
    ///
    /// Accepts an optional timestamp in seconds to seek to (default: 1.0s).
    /// The preview is saved as `<pre_video>_blur_preview.jpg` alongside the
    /// input file. Requires at least one --blur region.
    ///
    /// Examples:
    ///   --preview-blur                  (grab frame at 1.0s)
    ///   --preview-blur 12.5             (grab frame at 12.5s)
    #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "1.0")]
    pub preview_blur: Option<f64>,

    /// Print the ffmpeg command that would be executed, then exit without running it.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Enable verbose debug logging.
    #[arg(short, long, default_value_t = false)]
    pub verbose: bool,
}

/// A rectangular region of the video to blur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlurRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl FromStr for BlurRegion {
    type Err = LimitcutError;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 4 {
            return Err(LimitcutError::InvalidBlurRegion {
                input: s.to_owned(),
            });
        }

        let parse = |part: &str| -> std::result::Result<u32, _> { part.trim().parse::<u32>() };

        match (
            parse(parts[0]),
            parse(parts[1]),
            parse(parts[2]),
            parse(parts[3]),
        ) {
            (Ok(x), Ok(y), Ok(w), Ok(h)) => Ok(BlurRegion {
                x,
                y,
                width: w,
                height: h,
            }),
            _ => Err(LimitcutError::InvalidBlurRegion {
                input: s.to_owned(),
            }),
        }
    }
}

/// Validate and normalise an encoder name supplied on the CLI.
fn parse_encoder_name(s: &str) -> std::result::Result<String, String> {
    match s {
        "nvenc" | "h264_nvenc" => Ok("h264_nvenc".to_owned()),
        "vaapi" | "h264_vaapi" => Ok("h264_vaapi".to_owned()),
        "videotoolbox" | "h264_videotoolbox" => Ok("h264_videotoolbox".to_owned()),
        "libx264" | "x264" => Ok("libx264".to_owned()),
        other => Err(format!(
            "unknown encoder '{}'. Valid values: nvenc, vaapi, videotoolbox, libx264",
            other
        )),
    }
}

/// Derive the default output path from the pre-video path.
///
/// Strips the extension and appends `_combined.mp4` in the same directory.
pub fn default_output_path(pre_video: &std::path::Path) -> PathBuf {
    let stem = pre_video
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("output"))
        .to_string_lossy();
    let filename = format!("{}_combined.mp4", stem);
    match pre_video.parent() {
        Some(parent) if parent != std::path::Path::new("") => parent.join(filename),
        _ => PathBuf::from(filename),
    }
}

/// Derive the default blur preview path from the pre-video path.
///
/// Strips the extension and appends `_blur_preview.jpg` in the same directory.
pub fn default_preview_path(pre_video: &std::path::Path) -> PathBuf {
    let stem = pre_video
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("output"))
        .to_string_lossy();
    let filename = format!("{}_blur_preview.jpg", stem);
    match pre_video.parent() {
        Some(parent) if parent != std::path::Path::new("") => parent.join(filename),
        _ => PathBuf::from(filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── BlurRegion parsing ────────────────────────────────────────────────

    #[test]
    fn blur_region_valid() {
        let r: BlurRegion = "10:20:300:150".parse().unwrap();
        assert_eq!(
            r,
            BlurRegion {
                x: 10,
                y: 20,
                width: 300,
                height: 150
            }
        );
    }

    #[test]
    fn blur_region_zeros() {
        let r: BlurRegion = "0:0:1920:1080".parse().unwrap();
        assert_eq!(
            r,
            BlurRegion {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn blur_region_too_few_parts() {
        let err = "10:20:300".parse::<BlurRegion>();
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("x:y:w:h"));
    }

    #[test]
    fn blur_region_too_many_parts() {
        let err = "10:20:300:150:5".parse::<BlurRegion>();
        assert!(err.is_err());
    }

    #[test]
    fn blur_region_negative_rejected() {
        // u32 parse will fail on a negative number
        let err = "10:-20:300:150".parse::<BlurRegion>();
        assert!(err.is_err());
    }

    #[test]
    fn blur_region_non_numeric() {
        let err = "x:y:w:h".parse::<BlurRegion>();
        assert!(err.is_err());
    }

    #[test]
    fn blur_region_whitespace_trimmed() {
        let r: BlurRegion = " 10 : 20 : 300 : 150 ".parse().unwrap();
        assert_eq!(
            r,
            BlurRegion {
                x: 10,
                y: 20,
                width: 300,
                height: 150
            }
        );
    }

    // ── Encoder name parsing ──────────────────────────────────────────────

    #[test]
    fn encoder_short_aliases() {
        assert_eq!(parse_encoder_name("nvenc").unwrap(), "h264_nvenc");
        assert_eq!(parse_encoder_name("vaapi").unwrap(), "h264_vaapi");
        assert_eq!(
            parse_encoder_name("videotoolbox").unwrap(),
            "h264_videotoolbox"
        );
        assert_eq!(parse_encoder_name("x264").unwrap(), "libx264");
    }

    #[test]
    fn encoder_full_names() {
        assert_eq!(parse_encoder_name("h264_nvenc").unwrap(), "h264_nvenc");
        assert_eq!(parse_encoder_name("libx264").unwrap(), "libx264");
    }

    #[test]
    fn encoder_unknown_rejected() {
        let err = parse_encoder_name("h265_nvenc");
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("unknown encoder"));
    }

    // ── Default output path ───────────────────────────────────────────────

    #[test]
    fn default_output_same_dir() {
        let p = default_output_path(std::path::Path::new("/recordings/prepull.mkv"));
        assert_eq!(p, PathBuf::from("/recordings/prepull_combined.mp4"));
    }

    #[test]
    fn default_output_no_extension() {
        let p = default_output_path(std::path::Path::new("/recordings/myvideo"));
        assert_eq!(p, PathBuf::from("/recordings/myvideo_combined.mp4"));
    }

    #[test]
    fn default_output_relative_path() {
        let p = default_output_path(std::path::Path::new("prepull.mkv"));
        assert_eq!(p, PathBuf::from("prepull_combined.mp4"));
    }

    // ── CLI parse integration ─────────────────────────────────────────────

    #[test]
    fn cli_minimal_args() {
        let args = Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv"]).unwrap();
        assert_eq!(args.pre_video, PathBuf::from("pre.mkv"));
        assert_eq!(args.post_video, PathBuf::from("post.mkv"));
        assert!(args.output.is_none());
        assert!(args.blur.is_empty());
        assert!(!args.dry_run);
        assert!(!args.verbose);
    }

    #[test]
    fn cli_with_output() {
        let args =
            Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv", "-o", "out.mp4"]).unwrap();
        assert_eq!(args.output, Some(PathBuf::from("out.mp4")));
    }

    #[test]
    fn cli_multiple_blur_regions() {
        let args = Args::try_parse_from([
            "limitcut",
            "pre.mkv",
            "post.mkv",
            "--blur",
            "0:840:480:200",
            "--blur",
            "1400:0:480:60",
        ])
        .unwrap();
        assert_eq!(args.blur.len(), 2);
        assert_eq!(
            args.blur[0],
            BlurRegion {
                x: 0,
                y: 840,
                width: 480,
                height: 200
            }
        );
        assert_eq!(
            args.blur[1],
            BlurRegion {
                x: 1400,
                y: 0,
                width: 480,
                height: 60
            }
        );
    }

    #[test]
    fn cli_encoder_short_alias() {
        let args = Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv", "--encoder", "nvenc"])
            .unwrap();
        assert_eq!(args.encoder, Some("h264_nvenc".to_owned()));
    }

    #[test]
    fn cli_dry_run_and_verbose() {
        let args =
            Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv", "--dry-run", "--verbose"])
                .unwrap();
        assert!(args.dry_run);
        assert!(args.verbose);
    }

    #[test]
    fn cli_missing_args_fails() {
        let result = Args::try_parse_from(["limitcut"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_invalid_encoder_fails() {
        let result = Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv", "--encoder", "h265"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_invalid_blur_fails() {
        let result =
            Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv", "--blur", "notvalid"]);
        assert!(result.is_err());
    }

    // ── Preview blur CLI ──────────────────────────────────────────────────

    #[test]
    fn cli_preview_blur_flag_only() {
        let args = Args::try_parse_from([
            "limitcut",
            "pre.mkv",
            "post.mkv",
            "--blur",
            "0:0:100:100",
            "--preview-blur",
        ])
        .unwrap();
        assert_eq!(args.preview_blur, Some(1.0));
    }

    #[test]
    fn cli_preview_blur_with_timestamp() {
        let args = Args::try_parse_from([
            "limitcut",
            "pre.mkv",
            "post.mkv",
            "--blur",
            "0:0:100:100",
            "--preview-blur",
            "12.5",
        ])
        .unwrap();
        assert_eq!(args.preview_blur, Some(12.5));
    }

    #[test]
    fn cli_preview_blur_not_set() {
        let args = Args::try_parse_from(["limitcut", "pre.mkv", "post.mkv"]).unwrap();
        assert!(args.preview_blur.is_none());
    }

    // ── Default preview path ──────────────────────────────────────────────

    #[test]
    fn default_preview_same_dir() {
        let p = default_preview_path(std::path::Path::new("/recordings/prepull.mkv"));
        assert_eq!(p, PathBuf::from("/recordings/prepull_blur_preview.jpg"));
    }

    #[test]
    fn default_preview_no_extension() {
        let p = default_preview_path(std::path::Path::new("/recordings/myvideo"));
        assert_eq!(p, PathBuf::from("/recordings/myvideo_blur_preview.jpg"));
    }

    #[test]
    fn default_preview_relative_path() {
        let p = default_preview_path(std::path::Path::new("prepull.mkv"));
        assert_eq!(p, PathBuf::from("prepull_blur_preview.jpg"));
    }
}
