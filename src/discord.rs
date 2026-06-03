use std::path::Path;

use anyhow::Context;
use reqwest::blocking::{multipart, Client};
use serde_json::json;

use crate::error::LimitcutError;
use crate::instapost::ValidatedInstapostJson;

const REDACTED_WEBHOOK_TARGET: &str = "configured Discord webhook";

pub fn format_instapost_message(json_meta: &ValidatedInstapostJson) -> String {
    let mut lines = vec![format!("**{}**", json_meta.default_title())];

    if let Some(player_name) = &json_meta.player_name {
        lines.push(format!("Player: {player_name}"));
    }

    lines.join("\n")
}

pub fn upload_instapost(
    webhook_url: &str,
    mp4_path: &Path,
    json_meta: &ValidatedInstapostJson,
) -> anyhow::Result<()> {
    let filename = mp4_path
        .file_name()
        .context("upload path is missing a file name")?
        .to_string_lossy()
        .into_owned();

    let file_bytes = std::fs::read(mp4_path)
        .with_context(|| format!("Failed to read upload file: {}", mp4_path.display()))?;

    let payload = json!({
        "content": format_instapost_message(json_meta),
    });

    let form = multipart::Form::new()
        .text("payload_json", payload.to_string())
        .part(
            "files[0]",
            multipart::Part::bytes(file_bytes)
                .file_name(filename)
                .mime_str("video/mp4")?,
        );

    let response = Client::new()
        .post(webhook_url)
        .multipart(form)
        .send()
        .map_err(|source| LimitcutError::DiscordUploadFailed {
            url: REDACTED_WEBHOOK_TARGET.to_owned(),
            message: source.to_string(),
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(LimitcutError::DiscordUploadFailed {
            url: REDACTED_WEBHOOK_TARGET.to_owned(),
            message: format!("HTTP {}: {}", status, body.trim()),
        }
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::DateTime;

    use super::*;

    #[test]
    fn format_message_includes_basic_metadata() {
        let json = ValidatedInstapostJson {
            json_path: PathBuf::from("instapost.json"),
            started_at: DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap(),
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: Some("RPR".to_owned()),
            encounter: Some("The Futures Rewritten".to_owned()),
            territory_name: Some("Solution Nine".to_owned()),
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: Some("M'iu Bittermoon".to_owned()),
        };

        let message = format_instapost_message(&json);
        assert!(message.contains("**The Futures Rewritten/RPR POV**"));
        assert!(message.contains("Player: M'iu Bittermoon"));
        assert!(!message.contains("Started:"));
        assert!(!message.contains("Combat:"));
    }

    #[test]
    fn format_message_omits_player_line_when_missing() {
        let json = ValidatedInstapostJson {
            json_path: PathBuf::from("instapost.json"),
            started_at: DateTime::parse_from_rfc3339("2026-06-02T19:07:02.8897222+02:00").unwrap(),
            replay_buffer: PathBuf::from("Replay.mkv"),
            job: None,
            encounter: Some("Tuliyollal".to_owned()),
            territory_name: None,
            territory_type: Some(1185),
            is_in_combat: Some(false),
            player_name: None,
        };

        let message = format_instapost_message(&json);
        assert_eq!(message, "**Tuliyollal POV**");
    }

    #[test]
    fn discord_errors_use_redacted_target() {
        let err = LimitcutError::DiscordUploadFailed {
            url: REDACTED_WEBHOOK_TARGET.to_owned(),
            message: "boom".to_owned(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains(REDACTED_WEBHOOK_TARGET));
        assert!(!rendered.contains("discord.com/api/webhooks"));
    }
}
