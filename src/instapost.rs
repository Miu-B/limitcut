use std::path::{Path, PathBuf};

use anyhow::Error;
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use crate::error::{LimitcutError, Result};
use crate::json_input::normalize_encounter_name;

#[derive(Debug, Deserialize)]
struct InstapostJson {
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    replay_buffer: Option<String>,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    encounter: Option<String>,
    #[serde(default)]
    territory_name: Option<String>,
    #[serde(default)]
    territory_type: Option<u32>,
    #[serde(default)]
    is_in_combat: Option<bool>,
    #[serde(default)]
    player_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ValidatedInstapostJson {
    pub json_path: PathBuf,
    pub started_at: DateTime<FixedOffset>,
    pub replay_buffer: PathBuf,
    pub job: Option<String>,
    pub encounter: Option<String>,
    pub territory_name: Option<String>,
    pub territory_type: Option<u32>,
    #[allow(dead_code)]
    pub is_in_combat: Option<bool>,
    pub player_name: Option<String>,
}

impl ValidatedInstapostJson {
    pub fn display_label(&self) -> String {
        self.encounter
            .as_ref()
            .cloned()
            .or_else(|| self.territory_name.as_ref().cloned())
            .or_else(|| self.territory_type.map(|id| format!("territory-{id}")))
            .unwrap_or_else(|| "territory-unknown".to_owned())
    }

    pub fn default_title(&self) -> String {
        let label = self.display_label();
        if let Some(job) = &self.job {
            format!("{label}/{job} POV")
        } else {
            format!("{label} POV")
        }
    }
}

pub fn load_and_validate(path: &Path) -> Result<ValidatedInstapostJson> {
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

    let raw: InstapostJson =
        serde_json::from_str(&contents).map_err(|e| LimitcutError::JsonParseFailed {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let started_at_raw = required_non_empty(raw.started_at, path, "started_at")?;
    let replay_buffer_raw = required_non_empty(raw.replay_buffer, path, "replay_buffer")?;

    let started_at = DateTime::parse_from_rfc3339(&started_at_raw).map_err(|_| {
        LimitcutError::JsonInvalidDatetime {
            path: path.to_path_buf(),
            value: started_at_raw.clone(),
        }
    })?;

    let json_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let replay_buffer = json_dir.join(&replay_buffer_raw);

    if !replay_buffer.exists() {
        return Err(LimitcutError::JsonVideoNotFound {
            json: path.to_path_buf(),
            video: replay_buffer,
        });
    }
    if !replay_buffer.is_file() {
        return Err(LimitcutError::InputNotAFile(replay_buffer));
    }

    Ok(ValidatedInstapostJson {
        json_path: path.to_path_buf(),
        started_at,
        replay_buffer,
        job: optional_non_empty(raw.job),
        encounter: optional_non_empty(raw.encounter),
        territory_name: optional_non_empty(raw.territory_name),
        territory_type: raw.territory_type,
        is_in_combat: raw.is_in_combat,
        player_name: optional_non_empty(raw.player_name),
    })
}

pub fn derive_output_path(json: &ValidatedInstapostJson, base_dir: &Path) -> PathBuf {
    let date_dir = json.started_at.format("%Y-%m-%d").to_string();
    let label_dir = normalized_output_label(json);
    let file_name = format!("{}.mp4", json.started_at.format("%H-%M-%S"));

    let output = base_dir.join("instapost").join(date_dir).join(label_dir);
    match json.job.as_deref().map(normalize_encounter_name) {
        Some(job_dir) if !job_dir.is_empty() => output.join(job_dir).join(file_name),
        _ => output.join(file_name),
    }
}

fn normalized_output_label(json: &ValidatedInstapostJson) -> String {
    let primary = normalize_encounter_name(&json.display_label());
    if !primary.is_empty() {
        return primary;
    }

    json.territory_type
        .map(|id| format!("territory-{id}"))
        .unwrap_or_else(|| "territory-unknown".to_owned())
}

pub fn is_instapost_json_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    file_name.starts_with("instapost_") && file_name.ends_with(".json")
}

pub fn list_instapost_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(LimitcutError::InstapostWatchDirNotFound(dir.to_path_buf()));
    }
    if !dir.is_dir() {
        return Err(LimitcutError::InstapostWatchDirNotADir(dir.to_path_buf()));
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| LimitcutError::InstapostWatchDirReadFailed {
            path: dir.to_path_buf(),
            source,
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| is_instapost_json_path(path))
        .collect();

    files.sort();
    Ok(files)
}

pub fn move_to_status_dir(path: &Path, status_dir_name: &str) -> anyhow::Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let status_dir = parent.join(status_dir_name);
    std::fs::create_dir_all(&status_dir)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("missing file name for {}", path.display()))?;

    let mut target = status_dir.join(file_name);
    if target.exists() {
        target = unique_status_path(&status_dir, file_name);
    }

    std::fs::rename(path, &target)?;
    Ok(target)
}

pub fn write_failure_marker(moved_json_path: &Path, err: &Error) -> anyhow::Result<PathBuf> {
    let parent = moved_json_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = moved_json_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("missing file name for {}", moved_json_path.display()))?
        .to_string_lossy();
    let marker_path = parent.join(format!("{file_name}.error.txt"));
    std::fs::write(&marker_path, format!("{:#}\n", err))?;
    Ok(marker_path)
}

fn unique_status_path(status_dir: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let file_name = file_name.to_string_lossy();
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_owned(), Some(ext.to_owned())),
        None => (file_name.to_string(), None),
    };

    for index in 1.. {
        let candidate = match &ext {
            Some(ext) => status_dir.join(format!("{stem}.{index}.{ext}")),
            None => status_dir.join(format!("{stem}.{index}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("infinite iterator returned no unique path")
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

fn optional_non_empty(value: Option<String>) -> Option<String> {
    value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn write_file(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn load_and_validate_success() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("instapost_20260602_190702.json");
        write_file(&dir.path().join("Replay_2026-06-02_19-07-02.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-06-02T19:07:02.8897222+02:00",
  "replay_buffer": "Replay_2026-06-02_19-07-02.mkv",
  "job": "RPR",
  "encounter": "The Futures Rewritten",
  "territory_name": "Solution Nine",
  "territory_type": 1185,
  "is_in_combat": false,
  "player_name": "M'iu Bittermoon"
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(validated.job, Some("RPR".to_owned()));
        assert_eq!(validated.encounter, Some("The Futures Rewritten".to_owned()));
        assert_eq!(validated.territory_name, Some("Solution Nine".to_owned()));
        assert_eq!(validated.territory_type, Some(1185));
        assert_eq!(validated.player_name, Some("M'iu Bittermoon".to_owned()));
        assert_eq!(
            validated.replay_buffer,
            dir.path().join("Replay_2026-06-02_19-07-02.mkv")
        );
    }

    #[test]
    fn load_and_validate_resolves_replay_path_relative_to_json_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("obs");
        std::fs::create_dir_all(&nested).unwrap();
        let json_path = nested.join("instapost_20260602_190702.json");
        write_file(&nested.join("Replay_2026-06-02_19-07-02.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-06-02T19:07:02.8897222+02:00",
  "replay_buffer": "Replay_2026-06-02_19-07-02.mkv",
  "territory_type": 1185,
  "is_in_combat": false
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(
            validated.replay_buffer,
            nested.join("Replay_2026-06-02_19-07-02.mkv")
        );
    }

    #[test]
    fn load_and_validate_accepts_null_encounter() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("instapost_20260602_190702.json");
        write_file(&dir.path().join("Replay.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-06-02T19:07:02.8897222+02:00",
  "replay_buffer": "Replay.mkv",
  "encounter": null,
  "territory_name": "Solution Nine",
  "territory_type": 1185,
  "is_in_combat": false
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(validated.encounter, None);
        assert_eq!(validated.display_label(), "Solution Nine");
    }

    #[test]
    fn load_and_validate_accepts_null_territory_name() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("instapost_20260602_190702.json");
        write_file(&dir.path().join("Replay.mkv"), "video");
        write_file(
            &json_path,
            r#"{
  "started_at": "2026-06-02T19:07:02.8897222+02:00",
  "replay_buffer": "Replay.mkv",
  "encounter": null,
  "territory_name": null,
  "territory_type": 1185,
  "is_in_combat": false
}"#,
        );

        let validated = load_and_validate(&json_path).unwrap();
        assert_eq!(validated.territory_name, None);
        assert_eq!(validated.display_label(), "territory-1185");
    }

    #[test]
    fn display_label_prefers_encounter_then_territory_then_id() {
        let started_at = DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap();
        let base = ValidatedInstapostJson {
            json_path: PathBuf::from("instapost.json"),
            started_at,
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: Some("RPR".to_owned()),
            encounter: Some("The Futures Rewritten".to_owned()),
            territory_name: Some("Solution Nine".to_owned()),
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: Some("M'iu Bittermoon".to_owned()),
        };
        assert_eq!(base.display_label(), "The Futures Rewritten");

        let no_encounter = ValidatedInstapostJson {
            encounter: None,
            ..base.clone()
        };
        assert_eq!(no_encounter.display_label(), "Solution Nine");

        let id_fallback = ValidatedInstapostJson {
            encounter: None,
            territory_name: None,
            ..base
        };
        assert_eq!(id_fallback.display_label(), "territory-1185");
    }

    #[test]
    fn derive_output_path_uses_instapost_archive_layout() {
        let json = ValidatedInstapostJson {
            json_path: PathBuf::from("/tmp/instapost.json"),
            started_at: DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap(),
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: Some("RPR".to_owned()),
            encounter: Some("The Futures Rewritten".to_owned()),
            territory_name: Some("Solution Nine".to_owned()),
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: Some("M'iu Bittermoon".to_owned()),
        };

        let output = derive_output_path(&json, Path::new("/archive"));
        assert_eq!(
            output,
            PathBuf::from("/archive/instapost/2026-06-02/The_Futures_Rewritten/RPR/19-07-02.mp4")
        );
    }

    #[test]
    fn derive_output_path_falls_back_when_label_normalizes_empty() {
        let json = ValidatedInstapostJson {
            json_path: PathBuf::from("/tmp/instapost.json"),
            started_at: DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap(),
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: Some("<>".to_owned()),
            encounter: Some("<>:\"|?*///".to_owned()),
            territory_name: None,
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: None,
        };

        let output = derive_output_path(&json, Path::new("/archive"));
        assert_eq!(
            output,
            PathBuf::from("/archive/instapost/2026-06-02/territory-1185/19-07-02.mp4")
        );
    }

    #[test]
    fn default_title_omits_missing_job_cleanly() {
        let json = ValidatedInstapostJson {
            json_path: PathBuf::from("instapost.json"),
            started_at: DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap(),
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: None,
            encounter: Some("Solution Nine".to_owned()),
            territory_name: None,
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: None,
        };

        assert_eq!(json.default_title(), "Solution Nine POV");
    }

    #[test]
    fn instapost_path_matcher_ignores_tmp_files() {
        let dir = tempdir().unwrap();
        let json = dir.path().join("instapost_20260602_190702.json");
        let tmp = dir.path().join("instapost_20260602_190702.json.tmp");
        write_file(&json, "{}");
        write_file(&tmp, "{}");

        assert!(is_instapost_json_path(&json));
        assert!(!is_instapost_json_path(&tmp));
    }

    #[test]
    fn list_instapost_json_files_processes_backlog_in_sorted_order() {
        let dir = tempdir().unwrap();
        write_file(&dir.path().join("instapost_20260602_190703.json"), "{}");
        write_file(&dir.path().join("instapost_20260602_190701.json"), "{}");
        write_file(&dir.path().join("instapost_20260602_190702.json.tmp"), "{}");
        write_file(&dir.path().join("pull_20260602_190702.json"), "{}");

        let files = list_instapost_json_files(dir.path()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            names,
            vec![
                "instapost_20260602_190701.json",
                "instapost_20260602_190703.json",
            ]
        );
    }

    #[test]
    fn move_to_status_dir_moves_successful_json_to_done() {
        let dir = tempdir().unwrap();
        let json = dir.path().join("instapost_20260602_190702.json");
        write_file(&json, "{}");

        let moved = move_to_status_dir(&json, "done").unwrap();
        assert_eq!(
            moved,
            dir.path().join("done/instapost_20260602_190702.json")
        );
        assert!(!json.exists());
        assert!(moved.exists());
    }

    #[test]
    fn write_failure_marker_preserves_error_text() {
        let dir = tempdir().unwrap();
        let moved = dir.path().join("failed/instapost_20260602_190702.json");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        write_file(&moved, "{}");

        let marker = write_failure_marker(&moved, &anyhow::anyhow!("boom")).unwrap();
        let contents = std::fs::read_to_string(marker).unwrap();
        assert!(contents.contains("boom"));
    }
}
