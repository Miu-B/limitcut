use std::path::PathBuf;

use thiserror::Error;

/// Top-level error type for limitcut.
///
/// All processing errors are distinct variants so callers can match on them
/// and tools like `main` can assign appropriate exit codes.
#[derive(Debug, Error)]
pub enum LimitcutError {
    // ── Input validation ──────────────────────────────────────────────────
    #[error("Input file not found: {0}")]
    InputNotFound(PathBuf),

    #[error("Input path is not a file: {0}")]
    InputNotAFile(PathBuf),

    #[error("Output path already exists (use --overwrite to replace): {0}")]
    OutputExists(PathBuf),

    #[error("Invalid blur region '{input}': expected x:y:w:h with non-negative integers")]
    InvalidBlurRegion { input: String },

    #[error("--preview-blur requires at least one --blur region")]
    PreviewBlurWithoutRegions,

    #[error(
        "--black-hold ({hold:.1}s) exceeds the pre-video cut point ({cut:.1}s). \
         Use a shorter --black-hold value."
    )]
    BlackHoldExceedsCutPoint { hold: f64, cut: f64 },

    #[error("Failed to probe video resolution from {path}: {stderr}")]
    #[allow(dead_code)]
    ResolutionProbeFailed { path: PathBuf, stderr: String },

    #[error("Failed to parse video resolution from ffprobe output '{raw}'")]
    #[allow(dead_code)]
    ResolutionParseFailed { raw: String },

    // ── FFmpeg / FFprobe ──────────────────────────────────────────────────
    #[error("ffmpeg not found in PATH. Install ffmpeg to use limitcut.")]
    FfmpegNotFound,

    #[error("ffprobe not found in PATH. Install ffmpeg (which includes ffprobe) to use limitcut.")]
    FfprobeNotFound,

    #[error("ffprobe failed on {path}: {stderr}")]
    FfprobeFailed { path: PathBuf, stderr: String },

    #[error("Failed to parse duration from ffprobe output '{raw}': {source}")]
    DurationParseFailed {
        raw: String,
        #[source]
        source: std::num::ParseFloatError,
    },

    #[error("Failed to extract audio PCM from {path}: {stderr}")]
    AudioExtractionFailed { path: PathBuf, stderr: String },

    #[error("Audio data length {len} is not a multiple of 4 (corrupt PCM output)")]
    AudioDataCorrupt { len: usize },

    #[error("FFmpeg concat failed:\n{stderr}")]
    ConcatFailed { stderr: String },

    #[error("FFmpeg process could not be spawned: {0}")]
    FfmpegSpawnFailed(#[source] std::io::Error),

    // ── Correlation ───────────────────────────────────────────────────────
    #[error(
        "No audio overlap detected (correlation score {score:.3} < threshold {threshold:.3}). \
         The two videos may not overlap, or both clips are silent."
    )]
    CorrelationScoreTooLow { score: f64, threshold: f64 },

    #[error("Audio clip is silent — cannot correlate silent audio")]
    SilentAudio,

    #[error(
        "Needle ({needle_len} samples) is longer than haystack ({haystack_len} samples). \
         The post-video head window is larger than the pre-video tail window."
    )]
    NeedleTooLong {
        needle_len: usize,
        haystack_len: usize,
    },

    #[error("Correlation produced an out-of-range cut point: {cut:.3}s (video duration: {duration:.3}s)")]
    CutPointOutOfRange { cut: f64, duration: f64 },

    #[error("Pre-video audio is empty — file may have no audio track")]
    EmptyPreAudio,

    #[error("Post-video audio is empty — file may have no audio track")]
    EmptyPostAudio,
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, LimitcutError>;
