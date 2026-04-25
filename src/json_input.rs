use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use crate::error::{LimitcutError, Result};

#[derive(Debug, Deserialize)]
struct PullToObsJson {
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    encounter: Option<String>,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    recording: Option<String>,
    #[serde(default)]
    replay_buffer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ValidatedJson {
    pub json_path: PathBuf,
    pub started_at: DateTime<FixedOffset>,
    pub encounter: String,
    pub job: Option<String>,
    pub recording: PathBuf,
    pub replay_buffer: PathBuf,
}

pub fn load_and_validate(path: &Path) -> Result<ValidatedJson> {
    if !path.exists() {
        return Err(LimitcutError::JsonNotFound(path.to_path_buf()));
    }
    if !path.is_file() {
        return Err(LimitcutError::JsonNotAFile(path.to_path_buf()));
    }

    let contents = std::fs::read_to_string(path).map_err(|e| LimitcutError::JsonParseFailed {
        path: path.to_path_buf(),
        message: format!("could not read file: {e}"),
    })?;

    let raw: PullToObsJson =
        serde_json::from_str(&contents).map_err(|e| LimitcutError::JsonParseFailed {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let started_at_raw = required_non_empty(raw.started_at, path, "started_at")?;
    let encounter = required_non_empty(raw.encounter, path, "encounter")?;
    let recording_raw = required_non_empty(raw.recording, path, "recording")?;
    let replay_buffer_raw = required_non_empty(raw.replay_buffer, path, "replay_buffer")?;

    let started_at = DateTime::parse_from_rfc3339(&started_at_raw).map_err(|_| {
        LimitcutError::JsonInvalidDatetime {
            path: path.to_path_buf(),
            value: started_at_raw.clone(),
        }
    })?;

    if normalize_encounter_name(&encounter).is_empty() {
        return Err(LimitcutError::JsonEncounterNameEmpty {
            path: path.to_path_buf(),
        });
    }

    let json_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let recording = json_dir.join(&recording_raw);
    let replay_buffer = json_dir.join(&replay_buffer_raw);

    if !recording.exists() {
        return Err(LimitcutError::JsonVideoNotFound {
            json: path.to_path_buf(),
            video: recording,
        });
    }
    if !recording.is_file() {
        return Err(LimitcutError::InputNotAFile(recording));
    }
    if !replay_buffer.exists() {
        return Err(LimitcutError::JsonVideoNotFound {
            json: path.to_path_buf(),
            video: replay_buffer,
        });
    }
    if !replay_buffer.is_file() {
        return Err(LimitcutError::InputNotAFile(replay_buffer));
    }

    let job = raw
        .job
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    Ok(ValidatedJson {
        json_path: path.to_path_buf(),
        started_at,
        encounter,
        job,
        recording,
        replay_buffer,
    })
}

pub fn normalize_encounter_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_sep = false;

    for ch in name.trim().chars() {
        let next = match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => None,
            c if c.is_whitespace() => Some('_'),
            c if c.is_control() => None,
            c => Some(c),
        };

        if let Some(ch) = next {
            if ch == '_' {
                if !last_was_sep {
                    normalized.push('_');
                }
                last_was_sep = true;
            } else {
                normalized.push(ch);
                last_was_sep = false;
            }
        }
    }

    normalized.trim_matches('_').to_owned()
}

pub fn derive_output_path(json: &ValidatedJson, base_dir: &Path) -> PathBuf {
    let date_dir = json.started_at.format("%Y-%m-%d").to_string();
    let encounter_dir = normalize_encounter_name(&json.encounter);
    let file_name = format!("{}.mp4", json.started_at.format("%H-%M-%S"));
    if let Some(ref job) = json.job {
        base_dir
            .join(date_dir)
            .join(encounter_dir)
            .join(job)
            .join(file_name)
    } else {
        base_dir.join(date_dir).join(encounter_dir).join(file_name)
    }
}

fn required_non_empty(value: Option<String>, path: &Path, field: &'static str) -> Result<String> {
    let value = value.ok_or_else(|| LimitcutError::JsonMissingField {
        path: path.to_path_buf(),
        field,
    })?;

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LimitcutError::JsonEmptyField {
            path: path.to_path_buf(),
            field,
        });
    }

    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn write_file(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn normalize_encounter_replaces_spaces_and_invalid_chars() {
        assert_eq!(
            normalize_encounter_name("Deltascape V1.0"),
            "Deltascape_V1.0"
        );
        assert_eq!(
            normalize_encounter_name("Boss: Name/Phase 1"),
            "Boss_NamePhase_1"
        );
    }

    #[test]
    fn normalize_encounter_collapses_separators() {
        assert_eq!(normalize_encounter_name("  A   B  "), "A_B");
        assert_eq!(normalize_encounter_name("___A___"), "A");
    }

    #[test]
    fn derive_output_path_uses_encounter_subdir_and_timestamp() {
        let json = ValidatedJson {
            json_path: PathBuf::from("/tmp/input.json"),
            started_at: DateTime::parse_from_rfc3339("2026-04-24T07:55:31.746842+02:00").unwrap(),
            encounter: "Deltascape V1.0".to_owned(),
            job: None,
            recording: PathBuf::from("recording.mkv"),
            replay_buffer: PathBuf::from("replay.mkv"),
        };

        let output = derive_output_path(&json, Path::new("/out"));
        assert_eq!(
            output,
            PathBuf::from("/out/2026-04-24/Deltascape_V1.0/07-55-31.mp4")
        );
    }

    #[test]
    fn derive_output_path_includes_job_subfolder() {
        let json = ValidatedJson {
            json_path: PathBuf::from("/tmp/input.json"),
            started_at: DateTime::parse_from_rfc3339("2026-04-24T07:55:31.746842+02:00").unwrap(),
            encounter: "Deltascape V1.0".to_owned(),
            job: Some("BLM".to_owned()),
            recording: PathBuf::from("recording.mkv"),
            replay_buffer: PathBuf::from("replay.mkv"),
        };

        let output = derive_output_path(&json, Path::new("/out"));
        assert_eq!(
            output,
            PathBuf::from("/out/2026-04-24/Deltascape_V1.0/BLM/07-55-31.mp4")
        );
    }

    #[test]
    fn load_and_validate_success() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(&dir.path().join("full.mkv"), "video");
        write_file(&dir.path().join("pre.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "Deltascape V1.0",
  "job": "BLM",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(validated.encounter, "Deltascape V1.0");
        assert_eq!(validated.job, Some("BLM".to_owned()));
        assert_eq!(validated.recording, dir.path().join("full.mkv"));
        assert_eq!(validated.replay_buffer, dir.path().join("pre.mkv"));
    }

    #[test]
    fn load_and_validate_success_without_job() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(&dir.path().join("full.mkv"), "video");
        write_file(&dir.path().join("pre.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "Deltascape V1.0",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(validated.encounter, "Deltascape V1.0");
        assert!(validated.job.is_none());
        assert_eq!(validated.recording, dir.path().join("full.mkv"));
        assert_eq!(validated.replay_buffer, dir.path().join("pre.mkv"));
    }

    #[test]
    fn load_and_validate_missing_field_errors() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "Deltascape V1.0",
  "recording": "full.mkv"
}"#,
        );

        let err = load_and_validate(&json_path).unwrap_err();
        assert!(matches!(
            err,
            LimitcutError::JsonMissingField {
                field: "replay_buffer",
                ..
            }
        ));
    }

    #[test]
    fn load_and_validate_empty_field_errors() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "   ",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let err = load_and_validate(&json_path).unwrap_err();
        assert!(matches!(
            err,
            LimitcutError::JsonEmptyField {
                field: "encounter",
                ..
            }
        ));
    }

    #[test]
    fn load_and_validate_invalid_datetime_errors() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(&dir.path().join("full.mkv"), "video");
        write_file(&dir.path().join("pre.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "not-a-date",
  "encounter": "Deltascape V1.0",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let err = load_and_validate(&json_path).unwrap_err();
        assert!(matches!(err, LimitcutError::JsonInvalidDatetime { .. }));
    }

    #[test]
    fn load_and_validate_missing_video_errors() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(&dir.path().join("pre.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "Deltascape V1.0",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let err = load_and_validate(&json_path).unwrap_err();
        assert!(matches!(err, LimitcutError::JsonVideoNotFound { .. }));
    }

    #[test]
    fn load_and_validate_empty_normalized_name_errors() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("pull.json");
        write_file(&dir.path().join("full.mkv"), "video");
        write_file(&dir.path().join("pre.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-04-24T07:55:31.746842+02:00",
  "encounter": "<>:\"|?*///",
  "recording": "full.mkv",
  "replay_buffer": "pre.mkv"
}"#,
        );

        let err = load_and_validate(&json_path).unwrap_err();
        assert!(matches!(err, LimitcutError::JsonEncounterNameEmpty { .. }));
    }
}
