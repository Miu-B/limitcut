use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};

use crate::cli::BlurRegion;
use crate::error::{LimitcutError, Result};
use crate::ffmpeg::detect::EncoderConfig;

/// Fixed boxblur luma radius applied to every blur region.
const BLUR_LUMA_RADIUS: u32 = 10;

/// Parameters for the concatenation operation.
///
/// Groups the many arguments that `concatenate` needs so the function
/// signature stays clean and extensible.
pub struct ConcatParams<'a> {
    pub ffmpeg: &'a Path,
    pub pre: &'a Path,
    pub post: &'a Path,
    pub output: &'a Path,
    /// Duration of the pre-video up to the detected cut point (seconds).
    /// The pre-video is trimmed to this length via `-t`.
    pub cut_point_secs: f64,
    pub estimated_total_secs: f64,
    pub encoder: &'a EncoderConfig,
    pub blurs: &'a [BlurRegion],
    /// Fade-in duration in seconds (always applied, default 1.0).
    pub fadein: f64,
    /// Fade-out duration in seconds (always applied, default 1.0).
    pub fadeout: f64,
    /// Seconds of black screen before the fade-in begins (0.0 = none).
    /// The pre-video is NOT trimmed — these seconds are blacked out by the
    /// filter graph using `fade=t=in:st=black_hold:d=fadein`.
    pub black_hold: f64,
    /// Title text displayed during black-hold + fade-in period. Lines separated by '/'.
    pub title: Option<&'a str>,
    /// Seconds to seek into the pre-video before reading (0.0 = no seek).
    ///
    /// Used by the auto-trim feature: when `black_hold` exceeds
    /// `MAX_BLACK_HOLD`, the excess is converted into a pre-input seek so
    /// the output never starts with more than 4 seconds of black.
    pub pre_seek_secs: f64,
}

/// Result returned after a successful concat operation.
#[derive(Debug)]
pub struct ConcatResult {
    /// Wall-clock time taken for the ffmpeg encode.
    pub encode_time: std::time::Duration,
}

// ── Filter graph construction ─────────────────────────────────────────────────

/// Parameters that control fade/title behaviour in the filter graph.
///
/// Extracted from `ConcatParams` to keep `build_filter_complex` testable
/// without needing file paths, encoder configs, etc.
#[derive(Debug, Clone)]
pub struct FadeParams<'a> {
    /// Fade-in duration in seconds (always applied).
    pub fadein: f64,
    /// Fade-out duration in seconds (always applied).
    pub fadeout: f64,
    /// Seconds of the pre-video rendered as black before fade-in (0.0 = none).
    pub black_hold: f64,
    /// Title text displayed during black + fade-in period. Lines separated by '/'.
    pub title: Option<&'a str>,
    /// Total duration of the output video (needed for fade-out start time).
    pub total_duration_secs: f64,
}

impl Default for FadeParams<'_> {
    fn default() -> Self {
        Self {
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 0.0,
            title: None,
            total_duration_secs: 0.0,
        }
    }
}

/// Build the ffmpeg `-filter_complex` argument for a two-input concat with
/// optional blur regions and always-on fade-in/fade-out effects.
///
/// The pipeline always uses 2 inputs (0=pre, 1=post) and the stages are:
///
/// 1. Concat: `[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1`
/// 2. Blur regions (split→crop→boxblur→overlay chain)
/// 3. Video fade-in (`fade=t=in:st=hold:d=fadein` — blacks out before `st`)
///    and fade-out, plus audio silence/fade filters
/// 4. Drawtext title overlay (visible during hold, fades out during fade-in)
pub fn build_filter_complex(blurs: &[BlurRegion], fade: &FadeParams) -> String {
    let hold = fade.black_hold;
    let fadein = fade.fadein;
    let fadeout = fade.fadeout;

    // ── Stage 1: Concat ───────────────────────────────────────────────────

    let has_title = fade.title.is_some();
    // We always have fade, so we always need post-processing
    let needs_post_processing = true;

    let (concat_v_label, concat_a_label) = if needs_post_processing {
        ("cv0".to_owned(), "araw".to_owned())
    } else {
        ("v".to_owned(), "a".to_owned())
    };

    let mut parts: Vec<String> = Vec::new();

    // Always 2-input concat (no lavfi black source)
    parts.push(format!(
        "[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[{concat_v}][{concat_a}]",
        concat_v = concat_v_label,
        concat_a = concat_a_label,
    ));

    // ── Stage 2: Blur regions ─────────────────────────────────────────────

    let mut current_v = concat_v_label;

    for (i, blur) in blurs.iter().enumerate() {
        let output_label = format!("cv{}", i + 1);
        let main = format!("main{}", i);
        let blurred = format!("blurred{}", i);

        parts.push(format!(
            "[{input}]split[{main}][blur{i}];\
             [blur{i}]crop={w}:{h}:{x}:{y},boxblur={r}[{blurred}];\
             [{main}][{blurred}]overlay={x}:{y}[{output}]",
            input = current_v,
            main = main,
            i = i,
            blurred = blurred,
            w = blur.width,
            h = blur.height,
            x = blur.x,
            y = blur.y,
            r = BLUR_LUMA_RADIUS,
            output = output_label,
        ));

        current_v = output_label;
    }

    // ── Stage 3: Video & audio fade ───────────────────────────────────────

    let current_a = concat_a_label;

    // Video fade-in: fade=t=in:st=hold:d=fadein
    // This automatically blacks out all frames before st=hold.
    let mut vfade_parts: Vec<String> = Vec::new();
    let mut afade_parts: Vec<String> = Vec::new();

    // Video fade-in
    vfade_parts.push(format!(
        "fade=t=in:st={st:.6}:d={d:.6}",
        st = hold,
        d = fadein,
    ));

    // Audio: silence [0, max(0, hold-fadein)], then afade from max(0, hold-fadein) for fadein
    // so audio reaches full volume at t=hold (just as video starts fading in).
    let audio_fade_start = (hold - fadein).max(0.0);
    if audio_fade_start > 0.0 {
        // Silence the audio before the fade begins
        afade_parts.push(format!(
            "volume=enable='between(t,0,{end:.6})':volume=0",
            end = audio_fade_start,
        ));
    }
    afade_parts.push(format!(
        "afade=t=in:st={st:.6}:d={d:.6}",
        st = audio_fade_start,
        d = fadein,
    ));

    // Video & audio fade-out
    let total = fade.total_duration_secs;
    let fadeout_start = (total - fadeout).max(0.0);
    vfade_parts.push(format!(
        "fade=t=out:st={st:.6}:d={d:.6}",
        st = fadeout_start,
        d = fadeout,
    ));
    afade_parts.push(format!(
        "afade=t=out:st={st:.6}:d={d:.6}",
        st = fadeout_start,
        d = fadeout,
    ));

    let next_v = if has_title {
        "vfaded".to_owned()
    } else {
        "v".to_owned()
    };
    parts.push(format!(
        "[{input}]{filters}[{output}]",
        input = current_v,
        filters = vfade_parts.join(","),
        output = next_v,
    ));
    current_v = next_v;

    parts.push(format!(
        "[{input}]{filters}[a]",
        input = current_a,
        filters = afade_parts.join(","),
    ));

    // ── Stage 4: Title drawtext ───────────────────────────────────────────

    if let Some(title_text) = fade.title {
        let lines: Vec<&str> = title_text.split('/').collect();
        let line_count = lines.len() as i32;
        let font_size = 48;
        let line_spacing = 16;
        let total_text_height = line_count * font_size + (line_count - 1) * line_spacing;

        let mut drawtext_chain: Vec<String> = Vec::new();

        for (li, line) in lines.iter().enumerate() {
            let li = li as i32;
            let y_offset = -(total_text_height / 2) + li * (font_size + line_spacing);

            let escaped = escape_drawtext(line.trim());

            // Alpha expression: full opacity during [0, hold], linear fade 1→0 during [hold, hold+fadein]
            // After hold+fadein, alpha=0 (title gone). If hold=0, title immediately starts fading.
            let alpha_expr = if hold > 0.0 {
                format!(
                    "if(lt(t\\,{hold:.6})\\,1\\,max(0\\,1-(t-{hold:.6})/{fadein:.6}))",
                    hold = hold,
                    fadein = fadein,
                )
            } else {
                format!("max(0\\,1-t/{fadein:.6})", fadein = fadein,)
            };

            drawtext_chain.push(format!(
                "drawtext=text='{text}':\
                 x=(w-tw)/2:y=(h/2)+{y_off}-(th/2):\
                 fontsize={fs}:fontcolor=white:\
                 alpha='{alpha}'",
                text = escaped,
                y_off = y_offset,
                fs = font_size,
                alpha = alpha_expr,
            ));
        }

        parts.push(format!(
            "[{input}]{filters}[v]",
            input = current_v,
            filters = drawtext_chain.join(","),
        ));
    }

    parts.join(";")
}

/// Escape text for ffmpeg's drawtext filter.
///
/// Characters that have special meaning in ffmpeg filter expressions
/// need to be escaped with a backslash.
fn escape_drawtext(text: &str) -> String {
    text.replace('\\', "\\\\\\\\")
        .replace('\'', "'\\\\\\''")
        .replace(':', "\\:")
        .replace('%', "\\%")
}

// ── Command construction ──────────────────────────────────────────────────────

/// Build the ffmpeg `Command` for a two-input concat with always-on
/// fade-in/fade-out and optional title overlay.
///
/// Only two inputs are used: 0=pre (optionally seeked + trimmed to cut
/// point), 1=post (full). When `pre_seek_secs > 0`, the pre-video input
/// is seeked by that amount and `-t` is reduced accordingly so the stream
/// still ends at the original cut point.
///
/// This is shared by both the real encode path and `--dry-run`, so the
/// printed command always matches what would actually be executed.
pub fn build_ffmpeg_command(params: &ConcatParams) -> Command {
    let fade = FadeParams {
        fadein: params.fadein,
        fadeout: params.fadeout,
        black_hold: params.black_hold,
        title: params.title,
        total_duration_secs: params.estimated_total_secs,
    };

    let filter_complex = build_filter_complex(params.blurs, &fade);

    let mut cmd = Command::new(params.ffmpeg);
    cmd.args(["-hide_banner", "-y"]);

    // Input 0: pre-video, optionally seeked and trimmed to the cut point.
    // When pre_seek_secs > 0, we seek into the pre-video and shorten -t
    // so the stream still ends at the original cut point.
    if params.pre_seek_secs > 0.0 {
        cmd.args(["-ss", &format!("{:.6}", params.pre_seek_secs)]);
    }
    let pre_duration = params.cut_point_secs - params.pre_seek_secs;
    cmd.args(["-t", &format!("{:.6}", pre_duration)]);
    cmd.arg("-i").arg(params.pre);

    // Input 1: full post-video
    cmd.arg("-i").arg(params.post);

    // Filter graph
    cmd.args([
        "-filter_complex",
        &filter_complex,
        "-map",
        "[v]",
        "-map",
        "[a]",
    ]);

    // Video encoder + quality
    cmd.args(["-c:v", &params.encoder.name]);
    for arg in &params.encoder.quality_args {
        cmd.arg(arg);
    }

    // Audio codec
    cmd.args(["-c:a", "aac", "-b:a", "192k"]);

    // Output
    cmd.arg(params.output);

    cmd
}

// ── Progress bar ──────────────────────────────────────────────────────────────

fn make_progress_bar() -> ProgressBar {
    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>3}% {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );
    pb
}

/// Parse a percentage value out of an ffmpeg progress line.
///
/// ffmpeg writes progress to stderr as key=value pairs, e.g.:
/// ```text
/// out_time_ms=10000000
/// progress=continue
/// ```
/// We watch for `progress=` lines and use elapsed time vs total duration to
/// estimate percent. Alternatively we parse `out_time_ms` directly.
fn parse_out_time_ms(line: &str) -> Option<u64> {
    let stripped = line.trim().strip_prefix("out_time_ms=")?;
    stripped.parse::<u64>().ok()
}

/// Run an ffmpeg command, streaming progress to an `indicatif` progress bar.
///
/// `total_duration_secs` is used to convert `out_time_ms` into a percentage.
fn run_ffmpeg_with_progress(mut cmd: Command, total_duration_secs: f64) -> Result<()> {
    use std::io::{BufRead, BufReader};

    // Ask ffmpeg to write machine-readable progress to stderr
    cmd.args(["-progress", "pipe:2", "-nostats"]);
    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::null());

    let pb = make_progress_bar();
    pb.set_message("encoding…");

    let mut child = cmd.spawn().map_err(LimitcutError::FfmpegSpawnFailed)?;

    // Capture stderr lines from ffmpeg
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| LimitcutError::ConcatFailed {
            stderr: "failed to capture ffmpeg stderr".to_owned(),
        })?;
    let reader = BufReader::new(stderr);
    let mut stderr_lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        stderr_lines.push(line.clone());

        if let Some(out_ms) = parse_out_time_ms(&line) {
            let elapsed_secs = out_ms as f64 / 1_000_000.0;
            if total_duration_secs > 0.0 {
                let pct = ((elapsed_secs / total_duration_secs) * 100.0).min(100.0) as u64;
                pb.set_position(pct);
            }
        }
    }

    let status = child.wait().map_err(LimitcutError::FfmpegSpawnFailed)?;

    pb.finish_and_clear();

    if !status.success() {
        // Collect the full stderr output for a helpful error message.
        // Filter out pure progress lines to keep the message readable.
        let error_lines: Vec<&str> = stderr_lines
            .iter()
            .map(String::as_str)
            .filter(|l| {
                !l.starts_with("frame=")
                    && !l.starts_with("fps=")
                    && !l.starts_with("out_time")
                    && !l.starts_with("progress=")
                    && !l.starts_with("speed=")
                    && !l.starts_with("bitrate=")
                    && !l.starts_with("total_size=")
                    && !l.starts_with("dup_frames=")
                    && !l.starts_with("drop_frames=")
            })
            .collect();
        return Err(LimitcutError::ConcatFailed {
            stderr: error_lines.join("\n"),
        });
    }

    Ok(())
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Concatenate a pre-video (trimmed at `cut_point_secs`) and a full post-video
/// into a single output MP4.
///
/// Optionally applies blur regions to the output video.
pub fn concatenate(params: &ConcatParams) -> Result<ConcatResult> {
    let cmd = build_ffmpeg_command(params);

    let start = Instant::now();
    run_ffmpeg_with_progress(cmd, params.estimated_total_secs)?;
    let encode_time = start.elapsed();

    Ok(ConcatResult { encode_time })
}

/// Build a single-input blur filter graph for preview mode.
///
/// Unlike [`build_filter_complex`] (which starts from a two-input concat),
/// this starts from `[0:v]` directly:
/// ```text
/// [0:v]split[main0][blur0];[blur0]crop=W:H:X:Y,boxblur=10[blurred0];[main0][blurred0]overlay=X:Y[v]
/// ```
///
/// # Panics
///
/// Panics if `blurs` is empty — the caller must validate this beforehand.
pub fn build_blur_preview_filter(blurs: &[BlurRegion]) -> String {
    assert!(
        !blurs.is_empty(),
        "blur preview requires at least one region"
    );

    let mut parts = Vec::new();

    for (i, blur) in blurs.iter().enumerate() {
        let input_label = if i == 0 {
            "0:v".to_owned()
        } else {
            format!("cv{}", i)
        };
        let output_label = if i == blurs.len() - 1 {
            "v".to_owned()
        } else {
            format!("cv{}", i + 1)
        };
        let main = format!("main{}", i);
        let blurred = format!("blurred{}", i);

        parts.push(format!(
            "[{input}]split[{main}][blur{i}];\
             [blur{i}]crop={w}:{h}:{x}:{y},boxblur={r}[{blurred}];\
             [{main}][{blurred}]overlay={x}:{y}[{output}]",
            input = input_label,
            main = main,
            i = i,
            blurred = blurred,
            w = blur.width,
            h = blur.height,
            x = blur.x,
            y = blur.y,
            r = BLUR_LUMA_RADIUS,
            output = output_label,
        ));
    }

    parts.join(";")
}

/// Generate a single-frame JPEG preview with blur regions applied.
///
/// Seeks to `timestamp_secs` in the video, applies the blur filter graph,
/// and writes one frame to `output`.
pub fn generate_blur_preview(
    ffmpeg: &Path,
    video: &Path,
    blurs: &[BlurRegion],
    timestamp_secs: f64,
    output: &Path,
) -> Result<()> {
    let filter = build_blur_preview_filter(blurs);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-y"]);

    // Seek before input for fast keyframe-based seeking
    cmd.args(["-ss", &format!("{:.6}", timestamp_secs)]);
    cmd.arg("-i").arg(video);

    cmd.args(["-filter_complex", &filter, "-map", "[v]"]);
    cmd.args(["-frames:v", "1", "-q:v", "2"]);
    cmd.arg(output);

    let proc_output = cmd.output().map_err(LimitcutError::FfmpegSpawnFailed)?;

    if !proc_output.status.success() {
        let stderr = String::from_utf8_lossy(&proc_output.stderr).into_owned();
        return Err(LimitcutError::ConcatFailed { stderr });
    }

    Ok(())
}

/// Print the ffmpeg command as a shell-pasteable string (for --dry-run).
pub fn print_command(cmd: &Command) {
    let program = cmd.get_program().to_string_lossy();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| {
            let s = a.to_string_lossy();
            // Quote arguments that contain spaces
            if s.contains(' ') {
                format!("\"{}\"", s)
            } else {
                s.into_owned()
            }
        })
        .collect();
    println!("{} {}", program, args.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BlurRegion;

    /// Default fade params (fadein=1, fadeout=1, no hold, no title).
    fn default_fade() -> FadeParams<'static> {
        FadeParams {
            total_duration_secs: 60.0,
            ..Default::default()
        }
    }

    // ── Filter graph construction ─────────────────────────────────────────

    #[test]
    fn filter_no_blur_default_fades() {
        let filter = build_filter_complex(&[], &default_fade());

        // Always 2-input concat
        assert!(filter.contains("[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1"));
        // Video fade-in at st=0 (no hold), d=1
        assert!(filter.contains("fade=t=in:st=0.000000:d=1.000000"));
        // Video fade-out at st=59
        assert!(filter.contains("fade=t=out:st=59.000000:d=1.000000"));
        // Audio fade-in from st=0 (no hold, so no volume silence)
        assert!(filter.contains("afade=t=in:st=0.000000:d=1.000000"));
        // Audio fade-out
        assert!(filter.contains("afade=t=out:st=59.000000:d=1.000000"));
        // No volume silence (hold=0, hold-fadein < 0)
        assert!(!filter.contains("volume="));
        // Final labels
        assert!(filter.contains("[v]"));
        assert!(filter.contains("[a]"));
    }

    #[test]
    fn filter_single_blur() {
        let blurs = vec![BlurRegion {
            x: 0,
            y: 840,
            width: 480,
            height: 200,
        }];
        let filter = build_filter_complex(&blurs, &default_fade());

        // Must start with the concat producing [cv0][araw]
        assert!(filter.contains("concat=n=2:v=1:a=1[cv0][araw]"));
        // Must have a split from [cv0]
        assert!(filter.contains("[cv0]split[main0][blur0]"));
        // Must crop with correct dimensions
        assert!(filter.contains("crop=480:200:0:840"));
        // Must apply boxblur
        assert!(filter.contains(&format!("boxblur={}", BLUR_LUMA_RADIUS)));
        // Must overlay at correct position
        assert!(filter.contains("overlay=0:840"));
        // Fade filters present after blur
        assert!(filter.contains("fade=t=in"));
        assert!(filter.contains("fade=t=out"));
        // Final labels
        assert!(filter.contains("[v]"));
        assert!(filter.contains("[a]"));
    }

    #[test]
    fn filter_two_blurs() {
        let blurs = vec![
            BlurRegion {
                x: 0,
                y: 840,
                width: 480,
                height: 200,
            },
            BlurRegion {
                x: 1400,
                y: 0,
                width: 480,
                height: 60,
            },
        ];
        let filter = build_filter_complex(&blurs, &default_fade());

        // First blur: input=[cv0], output=[cv1]
        assert!(filter.contains("[cv0]split[main0][blur0]"));
        assert!(filter.contains("[cv1]"));

        // Second blur: input=[cv1], output=[cv2]
        assert!(filter.contains("[cv1]split[main1][blur1]"));
        // Fade applied after blur chain
        assert!(filter.contains("fade=t=in"));
        assert!(filter.contains("[v]"));
    }

    #[test]
    fn filter_three_blurs_correct_chain() {
        let blurs: Vec<BlurRegion> = (0..3)
            .map(|i| BlurRegion {
                x: i * 100,
                y: 0,
                width: 50,
                height: 50,
            })
            .collect();
        let filter = build_filter_complex(&blurs, &default_fade());

        // Intermediate labels cv1, cv2 must appear
        assert!(filter.contains("cv1"));
        assert!(filter.contains("cv2"));
        // Final output is [v]
        assert!(filter.contains("[v]"));
    }

    // ── Fade filter construction ──────────────────────────────────────────

    #[test]
    fn filter_custom_fadein() {
        let fade = FadeParams {
            fadein: 2.5,
            total_duration_secs: 60.0,
            ..Default::default()
        };
        let filter = build_filter_complex(&[], &fade);

        // Always 2-input concat (no lavfi inputs)
        assert!(filter.contains("concat=n=2"));
        assert!(!filter.contains("concat=n=3"));
        // Video fade-in at st=0 (no hold), d=2.5
        assert!(filter.contains("fade=t=in:st=0.000000:d=2.500000"));
        // Audio fade-in from st=0, d=2.5
        assert!(filter.contains("afade=t=in:st=0.000000:d=2.500000"));
        // No volume silence (hold-fadein < 0)
        assert!(!filter.contains("volume="));
    }

    #[test]
    fn filter_custom_fadeout() {
        let fade = FadeParams {
            fadeout: 2.0,
            total_duration_secs: 60.0,
            ..Default::default()
        };
        let filter = build_filter_complex(&[], &fade);

        // Video fade-out starting at 58s
        assert!(filter.contains("fade=t=out:st=58.000000:d=2.000000"));
        // Audio fade-out
        assert!(filter.contains("afade=t=out:st=58.000000:d=2.000000"));
    }

    #[test]
    fn filter_with_black_hold() {
        let fade = FadeParams {
            fadein: 2.0,
            fadeout: 1.0,
            black_hold: 10.0,
            total_duration_secs: 60.0,
            ..Default::default()
        };
        let filter = build_filter_complex(&[], &fade);

        // 2-input concat (no lavfi)
        assert!(filter.contains("concat=n=2"));
        assert!(!filter.contains("concat=n=3"));
        // Video fade-in starts at hold=10
        assert!(filter.contains("fade=t=in:st=10.000000:d=2.000000"));
        // Audio: silence [0, hold-fadein=8], then afade from 8 for 2s
        assert!(filter.contains("volume=enable='between(t,0,8.000000)':volume=0"));
        assert!(filter.contains("afade=t=in:st=8.000000:d=2.000000"));
    }

    #[test]
    fn filter_black_hold_less_than_fadein() {
        // hold=0.5, fadein=2.0 → hold-fadein=-1.5 → clamped to 0
        let fade = FadeParams {
            fadein: 2.0,
            fadeout: 1.0,
            black_hold: 0.5,
            total_duration_secs: 60.0,
            ..Default::default()
        };
        let filter = build_filter_complex(&[], &fade);

        // Video fade-in starts at hold=0.5
        assert!(filter.contains("fade=t=in:st=0.500000:d=2.000000"));
        // Audio: no silence period (hold-fadein < 0), afade from st=0
        assert!(!filter.contains("volume="));
        assert!(filter.contains("afade=t=in:st=0.000000:d=2.000000"));
    }

    #[test]
    fn filter_fadein_fadeout_and_title() {
        let fade = FadeParams {
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 2.0,
            title: Some("Boss Name/Mythic Kill"),
            total_duration_secs: 63.0,
        };
        let filter = build_filter_complex(&[], &fade);

        // 2-input concat
        assert!(filter.contains("concat=n=2"));
        // Video fade-in starts at hold=2
        assert!(filter.contains("fade=t=in:st=2.000000:d=1.000000"));
        // Video fade-out
        assert!(filter.contains("fade=t=out:st=62.000000:d=1.000000"));
        // Audio silence [0, hold-fadein=1]
        assert!(filter.contains("volume=enable='between(t,0,1.000000)':volume=0"));
        // Audio fade-in from st=1
        assert!(filter.contains("afade=t=in:st=1.000000:d=1.000000"));
        // Title drawtext with two lines
        assert!(filter.contains("drawtext=text='Boss Name'"));
        assert!(filter.contains("drawtext=text='Mythic Kill'"));
        // Title uses alpha expression (not enable/between)
        assert!(
            filter.contains("alpha='if(lt(t\\,2.000000)\\,1\\,max(0\\,1-(t-2.000000)/1.000000))'")
        );
    }

    #[test]
    fn filter_title_no_hold() {
        let fade = FadeParams {
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 0.0,
            title: Some("My Title"),
            total_duration_secs: 60.0,
        };
        let filter = build_filter_complex(&[], &fade);

        // Title uses simplified alpha expression (no hold)
        assert!(filter.contains("alpha='max(0\\,1-t/1.000000)'"));
    }

    #[test]
    fn filter_fadein_and_blur() {
        let blurs = vec![BlurRegion {
            x: 0,
            y: 840,
            width: 480,
            height: 200,
        }];
        let fade = FadeParams {
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 0.0,
            total_duration_secs: 61.0,
            title: None,
        };
        let filter = build_filter_complex(&blurs, &fade);

        // 2-input concat
        assert!(filter.contains("concat=n=2"));
        // Blur chain
        assert!(filter.contains("split[main0][blur0]"));
        assert!(filter.contains("crop=480:200:0:840"));
        // Fade filters after blur
        assert!(filter.contains("fade=t=in"));
        // Final labels
        assert!(filter.contains("[v]"));
        assert!(filter.contains("[a]"));
    }

    #[test]
    fn filter_title_escapes_special_chars() {
        let fade = FadeParams {
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 0.0,
            title: Some("Hello: World"),
            total_duration_secs: 61.0,
        };
        let filter = build_filter_complex(&[], &fade);

        // Colon should be escaped
        assert!(filter.contains("Hello\\: World"));
    }

    // ── parse_out_time_ms ─────────────────────────────────────────────────

    #[test]
    fn parse_progress_out_time() {
        assert_eq!(parse_out_time_ms("out_time_ms=5000000"), Some(5_000_000));
        assert_eq!(parse_out_time_ms("out_time_ms=0"), Some(0));
    }

    #[test]
    fn parse_progress_other_line() {
        assert_eq!(parse_out_time_ms("frame=100"), None);
        assert_eq!(parse_out_time_ms("progress=continue"), None);
        assert_eq!(parse_out_time_ms(""), None);
    }

    #[test]
    fn parse_progress_invalid_value() {
        assert_eq!(parse_out_time_ms("out_time_ms=N/A"), None);
    }

    // ── Blur preview filter construction ──────────────────────────────────

    #[test]
    fn preview_filter_single_blur() {
        let blurs = vec![BlurRegion {
            x: 100,
            y: 200,
            width: 300,
            height: 150,
        }];
        let filter = build_blur_preview_filter(&blurs);

        // Must start from [0:v] (single input, no concat)
        assert!(filter.starts_with("[0:v]split"));
        // Must crop with correct dimensions
        assert!(filter.contains("crop=300:150:100:200"));
        // Must apply boxblur
        assert!(filter.contains(&format!("boxblur={}", BLUR_LUMA_RADIUS)));
        // Must overlay at correct position
        assert!(filter.contains("overlay=100:200"));
        // Final label must be [v]
        assert!(filter.ends_with("[v]"));
        // Must NOT contain concat or audio labels
        assert!(!filter.contains("concat"));
        assert!(!filter.contains("[a]"));
    }

    #[test]
    fn preview_filter_two_blurs() {
        let blurs = vec![
            BlurRegion {
                x: 0,
                y: 840,
                width: 480,
                height: 200,
            },
            BlurRegion {
                x: 1400,
                y: 0,
                width: 480,
                height: 60,
            },
        ];
        let filter = build_blur_preview_filter(&blurs);

        // First blur: input=[0:v], output=[cv1]
        assert!(filter.contains("[0:v]split[main0][blur0]"));
        assert!(filter.contains("overlay=0:840[cv1]"));

        // Second blur: input=[cv1], final output=[v]
        assert!(filter.contains("[cv1]split[main1][blur1]"));
        assert!(filter.ends_with("[v]"));
    }

    #[test]
    fn preview_filter_three_blurs_chain() {
        let blurs: Vec<BlurRegion> = (0..3)
            .map(|i| BlurRegion {
                x: i * 100,
                y: 0,
                width: 50,
                height: 50,
            })
            .collect();
        let filter = build_blur_preview_filter(&blurs);

        // Starts from [0:v]
        assert!(filter.starts_with("[0:v]split"));
        // Intermediate labels cv1, cv2
        assert!(filter.contains("[cv1]"));
        assert!(filter.contains("[cv2]"));
        // Final output is [v]
        assert!(filter.ends_with("[v]"));
    }

    #[test]
    #[should_panic(expected = "blur preview requires at least one region")]
    fn preview_filter_empty_blurs_panics() {
        build_blur_preview_filter(&[]);
    }

    // ── Drawtext escape ───────────────────────────────────────────────────

    #[test]
    fn escape_drawtext_colons() {
        assert_eq!(escape_drawtext("Hello: World"), "Hello\\: World");
    }

    #[test]
    fn escape_drawtext_percent() {
        assert_eq!(escape_drawtext("100%"), "100\\%");
    }

    #[test]
    fn escape_drawtext_plain() {
        assert_eq!(escape_drawtext("Hello World"), "Hello World");
    }

    // ── build_ffmpeg_command ───────────────────────────────────────────────

    /// Helper to build a ConcatParams for command construction tests.
    fn test_concat_params(cut_point_secs: f64, encoder: &EncoderConfig) -> ConcatParams<'_> {
        ConcatParams {
            ffmpeg: std::path::Path::new("/usr/bin/ffmpeg"),
            pre: std::path::Path::new("pre.mkv"),
            post: std::path::Path::new("post.mkv"),
            output: std::path::Path::new("out.mp4"),
            cut_point_secs,
            estimated_total_secs: cut_point_secs + 300.0,
            encoder,
            blurs: &[],
            fadein: 1.0,
            fadeout: 1.0,
            black_hold: 0.0,
            title: None,
            pre_seek_secs: 0.0,
        }
    }

    #[test]
    fn cmd_basic_two_inputs() {
        let enc = EncoderConfig::from_name("libx264");
        let params = test_concat_params(25.0, &enc);
        let cmd = build_ffmpeg_command(&params);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // No -ss (pre-video is never seeked)
        assert!(!args.contains(&"-ss".to_owned()));
        // -t trims the pre-video to cut point
        assert!(args.contains(&"-t".to_owned()));
        let t_pos = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t_pos + 1], "25.000000");
    }

    #[test]
    fn cmd_black_hold_does_not_seek() {
        let enc = EncoderConfig::from_name("libx264");
        let mut params = test_concat_params(25.0, &enc);
        params.black_hold = 10.0;
        params.fadein = 2.0;
        let cmd = build_ffmpeg_command(&params);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // Still no -ss — black_hold is handled purely by the filter graph
        // (the auto-trim logic lives in main.rs, not in the command builder)
        assert!(!args.contains(&"-ss".to_owned()));
        // -t is still the full cut point
        let t_pos = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t_pos + 1], "25.000000");
    }

    #[test]
    fn cmd_pre_seek_adds_ss_and_reduces_t() {
        let enc = EncoderConfig::from_name("libx264");
        let mut params = test_concat_params(25.0, &enc);
        params.pre_seek_secs = 8.0; // e.g. black_hold=12 → trimmed to 4, seek=8
        params.black_hold = 4.0;
        let cmd = build_ffmpeg_command(&params);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // -ss must be present before -i
        let ss_pos = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[ss_pos + 1], "8.000000");

        // -t must be cut_point - pre_seek = 25 - 8 = 17
        let t_pos = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t_pos + 1], "17.000000");

        // -ss comes before -t which comes before -i
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        assert!(ss_pos < t_pos);
        assert!(t_pos < i_pos);
    }

    #[test]
    fn cmd_pre_seek_zero_no_ss() {
        let enc = EncoderConfig::from_name("libx264");
        let mut params = test_concat_params(30.0, &enc);
        params.pre_seek_secs = 0.0;
        params.black_hold = 3.0; // under MAX_BLACK_HOLD, no seek
        let cmd = build_ffmpeg_command(&params);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // No -ss when pre_seek is 0
        assert!(!args.contains(&"-ss".to_owned()));
        // -t is full cut point
        let t_pos = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t_pos + 1], "30.000000");
    }
}
