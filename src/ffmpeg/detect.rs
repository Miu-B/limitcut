use std::process::Command;

use crate::error::{LimitcutError, Result};

/// Supported hardware encoders in priority order (name, display label).
const HW_ENCODERS: &[(&str, &str)] = &[
    ("h264_nvenc", "NVIDIA NVENC"),
    ("h264_vaapi", "VAAPI (AMD/Intel)"),
    ("h264_videotoolbox", "VideoToolbox (macOS)"),
];

/// Software fallback encoder.
const SW_ENCODER: &str = "libx264";

/// Paths to the ffmpeg and ffprobe binaries located on this system.
#[derive(Debug, Clone)]
pub struct FfmpegBinaries {
    /// Absolute path to the `ffmpeg` binary.
    pub ffmpeg: std::path::PathBuf,
    /// Absolute path to the `ffprobe` binary.
    pub ffprobe: std::path::PathBuf,
}

impl FfmpegBinaries {
    /// Locate both binaries using `which`, returning distinct errors for each.
    pub fn locate() -> Result<Self> {
        let ffmpeg = which::which("ffmpeg").map_err(|_| LimitcutError::FfmpegNotFound)?;
        let ffprobe = which::which("ffprobe").map_err(|_| LimitcutError::FfprobeNotFound)?;
        Ok(Self { ffmpeg, ffprobe })
    }
}

/// H.264 encoder configuration including its quality CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// The ffmpeg encoder name (e.g. `h264_nvenc`).
    pub name: String,
    /// Human-readable label for display.
    pub display_name: String,
    /// Quality arguments appended after `-c:v <name>`.
    pub quality_args: Vec<String>,
}

impl EncoderConfig {
    pub fn libx264() -> Self {
        Self {
            name: SW_ENCODER.to_owned(),
            display_name: "libx264 (CPU)".to_owned(),
            quality_args: vec![
                "-crf".to_owned(),
                "18".to_owned(),
                "-preset".to_owned(),
                "medium".to_owned(),
            ],
        }
    }

    pub fn nvenc() -> Self {
        Self {
            name: "h264_nvenc".to_owned(),
            display_name: "NVIDIA NVENC".to_owned(),
            quality_args: vec![
                "-cq".to_owned(),
                "18".to_owned(),
                "-preset".to_owned(),
                "p4".to_owned(),
            ],
        }
    }

    pub fn vaapi() -> Self {
        Self {
            name: "h264_vaapi".to_owned(),
            display_name: "VAAPI (AMD/Intel)".to_owned(),
            quality_args: vec!["-qp".to_owned(), "18".to_owned()],
        }
    }

    pub fn videotoolbox() -> Self {
        Self {
            name: "h264_videotoolbox".to_owned(),
            display_name: "VideoToolbox (macOS)".to_owned(),
            quality_args: vec!["-q:v".to_owned(), "65".to_owned()],
        }
    }

    /// Build an `EncoderConfig` from a canonical encoder name string.
    ///
    /// Accepts the full ffmpeg names. Falls back to `libx264` for any unknown name.
    pub fn from_name(name: &str) -> Self {
        match name {
            "h264_nvenc" => Self::nvenc(),
            "h264_vaapi" => Self::vaapi(),
            "h264_videotoolbox" => Self::videotoolbox(),
            _ => Self::libx264(),
        }
    }
}

/// Auto-detect the best available H.264 encoder on this machine.
///
/// Probes in order: NVENC → VAAPI → VideoToolbox → libx264.
/// Returns `libx264` if no hardware encoder is listed in `ffmpeg -encoders`.
pub fn detect_best_encoder(ffmpeg: &std::path::Path) -> Result<EncoderConfig> {
    let output = Command::new(ffmpeg)
        .args(["-encoders", "-hide_banner"])
        .output()
        .map_err(LimitcutError::FfmpegSpawnFailed)?;

    let text = String::from_utf8_lossy(&output.stdout);

    for (enc_name, _label) in HW_ENCODERS {
        // FFmpeg encoder list format: " V....D h264_nvenc  NVIDIA NVENC H.264 encoder"
        // Match the name surrounded by whitespace to avoid partial matches.
        let padded = format!(" {} ", enc_name);
        let eol = format!(" {}\n", enc_name);
        if text.contains(&padded) || text.contains(&eol) {
            tracing::debug!("Selected hardware encoder: {}", enc_name);
            return Ok(EncoderConfig::from_name(enc_name));
        }
    }

    tracing::debug!("No hardware encoder found, falling back to libx264");
    Ok(EncoderConfig::libx264())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── EncoderConfig constructors ────────────────────────────────────────

    #[test]
    fn libx264_config() {
        let enc = EncoderConfig::libx264();
        assert_eq!(enc.name, "libx264");
        assert!(enc.quality_args.contains(&"-crf".to_owned()));
        assert!(enc.quality_args.contains(&"18".to_owned()));
        assert!(enc.quality_args.contains(&"-preset".to_owned()));
        assert!(enc.quality_args.contains(&"medium".to_owned()));
    }

    #[test]
    fn nvenc_config() {
        let enc = EncoderConfig::nvenc();
        assert_eq!(enc.name, "h264_nvenc");
        assert!(enc.quality_args.contains(&"-cq".to_owned()));
        assert!(enc.quality_args.contains(&"18".to_owned()));
    }

    #[test]
    fn vaapi_config() {
        let enc = EncoderConfig::vaapi();
        assert_eq!(enc.name, "h264_vaapi");
        assert!(enc.quality_args.contains(&"-qp".to_owned()));
    }

    #[test]
    fn videotoolbox_config() {
        let enc = EncoderConfig::videotoolbox();
        assert_eq!(enc.name, "h264_videotoolbox");
        assert!(enc.quality_args.contains(&"-q:v".to_owned()));
    }

    #[test]
    fn from_name_known() {
        assert_eq!(EncoderConfig::from_name("h264_nvenc").name, "h264_nvenc");
        assert_eq!(EncoderConfig::from_name("h264_vaapi").name, "h264_vaapi");
        assert_eq!(
            EncoderConfig::from_name("h264_videotoolbox").name,
            "h264_videotoolbox"
        );
        assert_eq!(EncoderConfig::from_name("libx264").name, "libx264");
    }

    #[test]
    fn from_name_unknown_falls_back_to_libx264() {
        let enc = EncoderConfig::from_name("totally_unknown");
        assert_eq!(enc.name, "libx264");
    }

    // ── detect_best_encoder logic (unit-tested via mocked encoder text) ───
    //
    // We can't call the real detect_best_encoder without ffmpeg, but we can
    // test the string-matching logic in isolation.

    fn encoder_present_in_text(text: &str, enc_name: &str) -> bool {
        let padded = format!(" {} ", enc_name);
        let eol = format!(" {}\n", enc_name);
        text.contains(&padded) || text.contains(&eol)
    }

    #[test]
    fn encoder_detection_finds_nvenc() {
        let fake_output = " V....D h264_nvenc           NVIDIA NVENC H.264 encoder\n";
        assert!(encoder_present_in_text(fake_output, "h264_nvenc"));
        assert!(!encoder_present_in_text(fake_output, "h264_vaapi"));
    }

    #[test]
    fn encoder_detection_finds_vaapi() {
        let fake_output = " V....D h264_vaapi           H.264/AVC (VAAPI)\n";
        assert!(encoder_present_in_text(fake_output, "h264_vaapi"));
        assert!(!encoder_present_in_text(fake_output, "h264_nvenc"));
    }

    #[test]
    fn encoder_detection_no_hw_encoder() {
        let fake_output = " V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC\n";
        assert!(!encoder_present_in_text(fake_output, "h264_nvenc"));
        assert!(!encoder_present_in_text(fake_output, "h264_vaapi"));
        assert!(!encoder_present_in_text(fake_output, "h264_videotoolbox"));
    }

    #[test]
    fn encoder_detection_no_partial_match() {
        // Ensure "h264_nvenc_extra" doesn't match "h264_nvenc"
        let fake_output = " V....D h264_nvenc_extra  something\n";
        assert!(!encoder_present_in_text(fake_output, "h264_nvenc"));
    }
}
