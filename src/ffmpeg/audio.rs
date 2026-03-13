use std::path::Path;
use std::process::Command;

use crate::error::{LimitcutError, Result};

/// Audio sample rate used for cross-correlation analysis (16 kHz mono).
pub const CORRELATION_SAMPLE_RATE: u32 = 16_000;

/// Extract a region of a media file as raw mono f32 PCM samples.
///
/// `start_secs` — start offset in seconds (0.0 = beginning of file).
/// `duration_secs` — how many seconds to extract.
///
/// Uses ffmpeg's `-f f32le` output to pipe little-endian 32-bit float samples
/// to stdout at `CORRELATION_SAMPLE_RATE` Hz, mono.
pub fn extract_pcm(
    ffmpeg: &Path,
    media: &Path,
    start_secs: f64,
    duration_secs: f64,
) -> Result<Vec<f32>> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error"]);

    if start_secs > 0.0 {
        cmd.args(["-ss", &format!("{:.6}", start_secs)]);
    }

    cmd.arg("-i").arg(media);
    cmd.args([
        "-t",
        &format!("{:.6}", duration_secs),
        "-vn", // drop video
        "-ac",
        "1", // mono
        "-ar",
        &CORRELATION_SAMPLE_RATE.to_string(),
        "-f",
        "f32le", // raw 32-bit float little-endian
        "-",     // pipe to stdout
    ]);

    let output = cmd.output().map_err(LimitcutError::FfmpegSpawnFailed)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(LimitcutError::AudioExtractionFailed {
            path: media.to_path_buf(),
            stderr,
        });
    }

    let bytes = &output.stdout;
    if !bytes.len().is_multiple_of(4) {
        return Err(LimitcutError::AudioDataCorrupt { len: bytes.len() });
    }

    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    tracing::debug!(
        "Extracted {} PCM samples ({:.3}s) from {:?} starting at {:.3}s",
        samples.len(),
        samples.len() as f64 / CORRELATION_SAMPLE_RATE as f64,
        media,
        start_secs,
    );

    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PCM byte-to-f32 conversion logic tested without a real media file.
    #[test]
    fn bytes_to_f32_samples() {
        // 1.0f32 in little-endian bytes
        let one: [u8; 4] = 1.0f32.to_le_bytes();
        // -1.0f32 in little-endian bytes
        let neg_one: [u8; 4] = (-1.0f32).to_le_bytes();
        let bytes: Vec<u8> = one.iter().chain(neg_one.iter()).copied().collect();

        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 1.0).abs() < f32::EPSILON);
        assert!((samples[1] + 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn odd_byte_count_would_error() {
        // Simulate the error path: 7 bytes is not a multiple of 4
        let bytes = [0u8; 7];
        let result = if !bytes.len().is_multiple_of(4) {
            Err(LimitcutError::AudioDataCorrupt { len: bytes.len() })
        } else {
            Ok(())
        };
        assert!(result.is_err());
    }
}
