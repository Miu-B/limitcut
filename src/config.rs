use std::path::PathBuf;

use serde::de::Error as _;
use serde::Deserialize;

use crate::cli::BlurRegion;
use crate::error::LimitcutError;

/// User configuration loaded from the config file.
///
/// Every field is optional — missing keys simply fall back to the built-in
/// defaults. CLI flags always override config values.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// H.264 encoder to use (e.g. "nvenc", "libx264").
    pub encoder: Option<String>,
    /// Fade-in duration in seconds.
    pub fadein: Option<f64>,
    /// Fade-out duration in seconds.
    pub fadeout: Option<f64>,
    /// Seconds of black screen before the fade-in begins.
    pub black_hold: Option<f64>,
    /// Always overwrite the output file without prompting.
    pub overwrite: Option<bool>,
    /// Default output directory.
    ///
    /// Equivalent to `--output-dir` on the CLI. In normal mode the filename is
    /// auto-generated from the pre-video. In JSON mode encounter subfolders are
    /// created under this directory.
    pub output_dir: Option<PathBuf>,
    /// Blur regions applied to the output video.
    #[serde(default)]
    pub blur: Vec<BlurRegion>,
}

/// Default config file template written on first run.
///
/// All keys are commented out so the user starts from the built-in defaults
/// and can selectively enable what they need.
const DEFAULT_CONFIG_TEMPLATE: &str = "\
# limitcut configuration file
# All keys are optional. CLI flags always override these values.

# H.264 encoder. Options: nvenc, vaapi, videotoolbox, libx264
# If not set, the best available encoder is auto-detected.
# encoder = \"libx264\"

# Fade-in / fade-out durations in seconds (default: 1.0 each).
# fadein = 1.0
# fadeout = 1.0

# Seconds of black screen before the fade-in begins (default: 0).
# black_hold = 0.0

# Always overwrite the output file without prompting (default: false).
# overwrite = false

# Default output directory. This is the same as passing --output-dir.
# In normal mode, the filename is auto-generated from the pre-video
# (e.g. \"prepull_combined.mp4\"). In JSON mode, output is organised as
# `<dir>/YYYY-MM-DD/<encounter>/<job>/HH-MM-SS.mp4`. If not set, output
# goes next to the pre-video (normal mode) or next to the JSON file (JSON mode).
# output_dir = \"/home/user/Videos\"

# Blur regions applied to the output video. Repeat this block for
# each region. CLI --blur flags replace all config blurs when present.
# [[blur]]
# x = 0
# y = 0
# width = 100
# height = 100
";

impl Config {
    /// Platform-specific path to the config file.
    ///
    /// | Platform | Path |
    /// |---|---|
    /// | Linux | `~/.config/limitcut/config.toml` |
    /// | macOS | `~/Library/Application Support/limitcut/config.toml` |
    /// | Windows | `%APPDATA%\\limitcut\\config.toml` |
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("limitcut").join("config.toml"))
    }

    /// Load the config file, creating a default template if none exists.
    ///
    /// - If the config file doesn't exist, a commented-out template is
    ///   written and an info message is printed. The returned config has
    ///   all fields at their defaults (i.e. `None` / empty).
    /// - If the config file exists but is malformed, a hard error is
    ///   returned so the user knows to fix it.
    /// - If the platform config directory can't be determined, the config
    ///   is silently skipped (returns defaults).
    pub fn load() -> Result<Self, LimitcutError> {
        let path = match Self::path() {
            Some(p) => p,
            None => {
                tracing::debug!("Could not determine config directory; skipping config file");
                return Ok(Self::default());
            }
        };

        if !path.exists() {
            Self::write_default(&path);
            return Ok(Self::default());
        }

        let contents =
            std::fs::read_to_string(&path).map_err(|e| LimitcutError::ConfigParseError {
                path: path.clone(),
                source: toml::de::Error::custom(format!(
                    "could not read file: {} ({})",
                    path.display(),
                    e
                )),
            })?;

        let config: Config =
            toml::from_str(&contents).map_err(|e| LimitcutError::ConfigParseError {
                path: path.clone(),
                source: e,
            })?;

        tracing::debug!("Loaded config from {}", path.display());
        Ok(config)
    }

    /// Write the default config template to disk.
    ///
    /// Silently does nothing if the directory can't be created or the file
    /// can't be written — this is a best-effort convenience, not critical.
    fn write_default(path: &PathBuf) {
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                tracing::debug!("Could not create config directory: {}", parent.display());
                return;
            }
        }

        match std::fs::write(path, DEFAULT_CONFIG_TEMPLATE) {
            Ok(()) => {
                eprintln!(
                    "Config file created: {} — edit it to set your defaults.",
                    path.display()
                );
            }
            Err(e) => {
                tracing::debug!("Could not write default config: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_all_none() {
        let config = Config::default();
        assert!(config.encoder.is_none());
        assert!(config.fadein.is_none());
        assert!(config.fadeout.is_none());
        assert!(config.black_hold.is_none());
        assert!(config.overwrite.is_none());
        assert!(config.output_dir.is_none());
        assert!(config.blur.is_empty());
    }

    #[test]
    fn parse_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.encoder.is_none());
        assert!(config.blur.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
            encoder = "nvenc"
            fadein = 1.5
            fadeout = 2.0
            black_hold = 3.0
            overwrite = true
            output_dir = "/home/user/Videos"

            [[blur]]
            x = 0
            y = 840
            width = 480
            height = 200

            [[blur]]
            x = 1400
            y = 0
            width = 480
            height = 60
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.encoder, Some("nvenc".to_owned()));
        assert_eq!(config.fadein, Some(1.5));
        assert_eq!(config.fadeout, Some(2.0));
        assert_eq!(config.black_hold, Some(3.0));
        assert_eq!(config.overwrite, Some(true));
        assert_eq!(config.output_dir, Some(PathBuf::from("/home/user/Videos")));
        assert_eq!(config.blur.len(), 2);
        assert_eq!(
            config.blur[0],
            BlurRegion {
                x: 0,
                y: 840,
                width: 480,
                height: 200
            }
        );
        assert_eq!(
            config.blur[1],
            BlurRegion {
                x: 1400,
                y: 0,
                width: 480,
                height: 60
            }
        );
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"
            fadein = 2.0
            overwrite = true
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.fadein, Some(2.0));
        assert_eq!(config.overwrite, Some(true));
        assert!(config.encoder.is_none());
        assert!(config.fadeout.is_none());
        assert!(config.blur.is_empty());
    }

    #[test]
    fn parse_single_blur() {
        let toml = r#"
            [[blur]]
            x = 100
            y = 200
            width = 300
            height = 150
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.blur.len(), 1);
        assert_eq!(
            config.blur[0],
            BlurRegion {
                x: 100,
                y: 200,
                width: 300,
                height: 150
            }
        );
    }

    #[test]
    fn parse_default_template_is_valid_toml() {
        // The commented-out template must parse as valid TOML (all lines are comments)
        let config: Config = toml::from_str(DEFAULT_CONFIG_TEMPLATE).unwrap();
        assert!(config.encoder.is_none());
        assert!(config.blur.is_empty());
    }

    #[test]
    fn parse_malformed_toml_errors() {
        let result = toml::from_str::<Config>("encoder = [invalid");
        assert!(result.is_err());
    }

    #[test]
    fn config_path_is_some() {
        // On any real system, dirs::config_dir() should return Some
        // This test may fail in extremely sandboxed environments
        let path = Config::path();
        if let Some(p) = &path {
            assert!(p.ends_with("limitcut/config.toml"));
        }
    }
}
