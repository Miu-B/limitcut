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
    pub cut_point_secs: f64,
    pub estimated_total_secs: f64,
    pub encoder: &'a EncoderConfig,
    pub blurs: &'a [BlurRegion],
}

/// Result returned after a successful concat operation.
#[derive(Debug)]
pub struct ConcatResult {
    /// Wall-clock time taken for the ffmpeg encode.
    pub encode_time: std::time::Duration,
}

// ── Filter graph construction ─────────────────────────────────────────────────

/// Build the ffmpeg `-filter_complex` argument for a two-input concat with
/// optional blur regions applied to the combined video stream.
///
/// Without blur regions:
/// ```text
/// [0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[v][a]
/// ```
///
/// With N blur regions, the video stream `[cv]` is piped through N successive
/// split→crop→boxblur→overlay chains before being labelled `[v]`:
/// ```text
/// [0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[cv][a];
/// [cv]split[main0][blur0];[blur0]crop=W:H:X:Y,boxblur=10[blurred0];[main0][blurred0]overlay=X:Y[cv1];
/// [cv1]split[main1][blur1];...overlay=X:Y[v]
/// ```
pub fn build_filter_complex(blurs: &[BlurRegion]) -> String {
    if blurs.is_empty() {
        return "[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[v][a]".to_owned();
    }

    // The concat produces [cv][a]; we'll chain blur transforms through [cv0], [cv1], …
    let mut parts = vec!["[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[cv0][a]".to_owned()];

    for (i, blur) in blurs.iter().enumerate() {
        let input_label = format!("cv{}", i);
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

// ── Command construction ──────────────────────────────────────────────────────

/// Build the ffmpeg `Command` for a two-input concat.
///
/// This is shared by both the real encode path and `--dry-run`, so the
/// printed command always matches what would actually be executed.
pub fn build_ffmpeg_command(params: &ConcatParams) -> Command {
    let filter_complex = build_filter_complex(params.blurs);

    let mut cmd = Command::new(params.ffmpeg);
    cmd.args(["-hide_banner", "-y"]);

    // Input 0: pre-video trimmed to the cut point
    cmd.args(["-t", &format!("{:.6}", params.cut_point_secs)]);
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

    // ── Filter graph construction ─────────────────────────────────────────

    #[test]
    fn filter_no_blur() {
        let filter = build_filter_complex(&[]);
        assert_eq!(filter, "[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[v][a]");
    }

    #[test]
    fn filter_single_blur() {
        let blurs = vec![BlurRegion {
            x: 0,
            y: 840,
            width: 480,
            height: 200,
        }];
        let filter = build_filter_complex(&blurs);

        // Must start with the concat producing [cv0][a]
        assert!(filter.contains("concat=n=2:v=1:a=1[cv0][a]"));
        // Must have a split from [cv0]
        assert!(filter.contains("[cv0]split[main0][blur0]"));
        // Must crop with correct dimensions
        assert!(filter.contains("crop=480:200:0:840"));
        // Must apply boxblur
        assert!(filter.contains(&format!("boxblur={}", BLUR_LUMA_RADIUS)));
        // Must overlay at correct position
        assert!(filter.contains("overlay=0:840"));
        // Final label must be [v]
        assert!(filter.ends_with("[v]"));
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
        let filter = build_filter_complex(&blurs);

        // First blur: input=[cv0], output=[cv1]
        assert!(filter.contains("[cv0]split[main0][blur0]"));
        assert!(filter.contains("[cv1]"));

        // Second blur: input=[cv1], final output=[v]
        assert!(filter.contains("[cv1]split[main1][blur1]"));
        assert!(filter.ends_with("[v]"));
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
        let filter = build_filter_complex(&blurs);

        // Intermediate labels cv1, cv2 must appear
        assert!(filter.contains("cv1"));
        assert!(filter.contains("cv2"));
        // Final output is [v]
        assert!(filter.ends_with("[v]"));
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
}
