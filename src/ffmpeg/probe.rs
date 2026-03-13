use std::path::Path;
use std::process::Command;

use crate::error::{LimitcutError, Result};

/// Return the duration of a media file in seconds using `ffprobe`.
pub fn get_duration(ffprobe: &Path, media: &Path) -> Result<f64> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(media)
        .output()
        .map_err(LimitcutError::FfmpegSpawnFailed)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(LimitcutError::FfprobeFailed {
            path: media.to_path_buf(),
            stderr,
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    raw.parse::<f64>()
        .map_err(|source| LimitcutError::DurationParseFailed { raw, source })
}

#[cfg(test)]
mod tests {
    /// Duration parsing logic tested in isolation (no real ffprobe needed).
    #[test]
    fn parse_valid_duration() {
        let raw = "123.456000";
        let parsed: f64 = raw.trim().parse().unwrap();
        assert!((parsed - 123.456).abs() < 0.001);
    }

    #[test]
    fn parse_integer_duration() {
        let raw = "60\n";
        let parsed: f64 = raw.trim().parse().unwrap();
        assert!((parsed - 60.0).abs() < 0.001);
    }

    #[test]
    fn parse_invalid_duration_fails() {
        let raw = "N/A";
        let result: std::result::Result<f64, _> = raw.trim().parse();
        assert!(result.is_err());
    }
}
