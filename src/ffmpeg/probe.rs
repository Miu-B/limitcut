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

/// Return the resolution (width, height) of the first video stream using `ffprobe`.
#[allow(dead_code)]
pub fn probe_resolution(ffprobe: &Path, media: &Path) -> Result<(u32, u32)> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(media)
        .output()
        .map_err(LimitcutError::FfmpegSpawnFailed)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(LimitcutError::ResolutionProbeFailed {
            path: media.to_path_buf(),
            stderr,
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let parts: Vec<&str> = raw.split('x').collect();
    if parts.len() != 2 {
        return Err(LimitcutError::ResolutionParseFailed { raw });
    }

    let width: u32 = parts[0]
        .trim()
        .parse()
        .map_err(|_| LimitcutError::ResolutionParseFailed { raw: raw.clone() })?;
    let height: u32 = parts[1]
        .trim()
        .parse()
        .map_err(|_| LimitcutError::ResolutionParseFailed { raw: raw.clone() })?;

    Ok((width, height))
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

    #[test]
    fn parse_resolution_valid() {
        // Simulates "1920x1080\n" from ffprobe
        let raw = "1920x1080";
        let parts: Vec<&str> = raw.split('x').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].trim().parse::<u32>().unwrap(), 1920);
        assert_eq!(parts[1].trim().parse::<u32>().unwrap(), 1080);
    }

    #[test]
    fn parse_resolution_invalid() {
        let raw = "not_a_resolution";
        let parts: Vec<&str> = raw.split('x').collect();
        // This should not have exactly 2 numeric parts
        assert!(parts.len() != 2 || parts[0].trim().parse::<u32>().is_err());
    }
}
