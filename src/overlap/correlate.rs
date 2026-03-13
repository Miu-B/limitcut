use std::path::Path;

use crate::error::{LimitcutError, Result};
use crate::ffmpeg::audio::{extract_pcm, CORRELATION_SAMPLE_RATE};
use crate::ffmpeg::probe::get_duration;

/// How many seconds of audio from the *end* of the pre-video to search.
///
/// The overlap is expected to be at most a few seconds, but we use 6 s to
/// give ample margin. Larger values increase CPU time linearly.
const SEARCH_WINDOW_SECS: f64 = 6.0;

/// How many seconds of audio from the *start* of the post-video to use as the
/// correlation needle. Must be shorter than the actual overlap.
const NEEDLE_DURATION_SECS: f64 = 0.5;

/// Minimum normalised cross-correlation score accepted as a valid match.
/// Below this threshold the audio is considered non-overlapping or silent.
const MIN_SCORE: f64 = 0.3;

/// The result of a successful overlap detection.
#[derive(Debug, Clone)]
pub struct CorrelationResult {
    /// Time (in seconds) at which to cut the pre-video.
    ///
    /// The pre-video should be trimmed to exactly this duration, then the
    /// post-video appended from its beginning to produce a seamless output.
    pub cut_point_secs: f64,
    /// Normalised cross-correlation score (0.0 – 1.0, higher = more confident).
    pub score: f64,
    /// Duration of the pre-video in seconds.
    pub pre_duration_secs: f64,
}

/// Detect the overlap between pre-video and post-video using audio
/// cross-correlation and return the cut point.
///
/// # Algorithm
///
/// 1. Probe the pre-video duration with ffprobe.
/// 2. Extract the **last `SEARCH_WINDOW_SECS`** of the pre-video audio (haystack).
/// 3. Extract the **first `NEEDLE_DURATION_SECS`** of the post-video audio (needle).
/// 4. Slide the needle over the haystack using normalised cross-correlation.
/// 5. The best-matching offset + the haystack start time gives the cut point.
///
/// # Errors
///
/// Returns [`LimitcutError::CorrelationScoreTooLow`] if the best score is below
/// [`MIN_SCORE`], which indicates the two clips do not share overlapping audio.
pub fn find_cut_point(
    ffmpeg: &Path,
    ffprobe: &Path,
    pre: &Path,
    post: &Path,
) -> Result<CorrelationResult> {
    let pre_duration = get_duration(ffprobe, pre)?;
    tracing::debug!("Pre-video duration: {:.3}s", pre_duration);

    // Extract haystack: the tail of the pre-video
    let tail_start = (pre_duration - SEARCH_WINDOW_SECS).max(0.0);
    let tail_duration = pre_duration - tail_start;

    tracing::debug!(
        "Extracting pre-video tail: {:.3}s → {:.3}s ({:.3}s window)",
        tail_start,
        pre_duration,
        tail_duration
    );
    let haystack = extract_pcm(ffmpeg, pre, tail_start, tail_duration)?;

    if haystack.is_empty() {
        return Err(LimitcutError::EmptyPreAudio);
    }

    // Extract needle: the head of the post-video
    tracing::debug!(
        "Extracting post-video head: 0.0s → {:.3}s (needle)",
        NEEDLE_DURATION_SECS
    );
    let needle = extract_pcm(ffmpeg, post, 0.0, NEEDLE_DURATION_SECS)?;

    if needle.is_empty() {
        return Err(LimitcutError::EmptyPostAudio);
    }

    let (offset_samples, score) = cross_correlate(&haystack, &needle)?;
    let offset_secs = offset_samples as f64 / CORRELATION_SAMPLE_RATE as f64;
    let cut_point_secs = tail_start + offset_secs;

    tracing::info!(
        "Correlation: score={:.4}, offset={:.3}s, cut_point={:.3}s (pre_duration={:.3}s)",
        score,
        offset_secs,
        cut_point_secs,
        pre_duration
    );

    // Sanity check: cut point must lie within the pre-video
    if cut_point_secs < 0.0 || cut_point_secs > pre_duration {
        return Err(LimitcutError::CutPointOutOfRange {
            cut: cut_point_secs,
            duration: pre_duration,
        });
    }

    Ok(CorrelationResult {
        cut_point_secs,
        score,
        pre_duration_secs: pre_duration,
    })
}

/// Slide `needle` over `haystack` using normalised cross-correlation.
///
/// Returns `(best_offset_samples, best_score)` where `best_offset_samples` is
/// the index in `haystack` where `needle` best aligns.
///
/// The normalisation by `sqrt(hay_energy) * sqrt(needle_energy)` makes the
/// score independent of absolute volume levels, which differ between an OBS
/// replay buffer and a fresh recording.
///
/// # Errors
///
/// - [`LimitcutError::NeedleTooLong`] if `needle` is longer than `haystack`.
/// - [`LimitcutError::SilentAudio`] if the needle has no energy (all zeros).
/// - [`LimitcutError::CorrelationScoreTooLow`] if the best score < `MIN_SCORE`.
pub fn cross_correlate(haystack: &[f32], needle: &[f32]) -> Result<(usize, f64)> {
    if needle.len() > haystack.len() {
        return Err(LimitcutError::NeedleTooLong {
            needle_len: needle.len(),
            haystack_len: haystack.len(),
        });
    }

    let needle_energy: f64 = needle.iter().map(|&s| (s as f64).powi(2)).sum();
    if needle_energy < 1e-10 {
        return Err(LimitcutError::SilentAudio);
    }

    let max_offset = haystack.len() - needle.len();
    let mut best_offset = 0usize;
    let mut best_score = f64::NEG_INFINITY;

    for offset in 0..=max_offset {
        let window = &haystack[offset..offset + needle.len()];

        let mut dot = 0.0f64;
        let mut hay_energy = 0.0f64;
        for (&h, &n) in window.iter().zip(needle.iter()) {
            let hf = h as f64;
            let nf = n as f64;
            dot += hf * nf;
            hay_energy += hf * hf;
        }

        let score = if hay_energy > 1e-10 {
            dot / (hay_energy.sqrt() * needle_energy.sqrt())
        } else {
            0.0
        };

        if score > best_score {
            best_score = score;
            best_offset = offset;
        }
    }

    if best_score < MIN_SCORE {
        return Err(LimitcutError::CorrelationScoreTooLow {
            score: best_score,
            threshold: MIN_SCORE,
        });
    }

    Ok((best_offset, best_score))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    // ── Helper: generate a sine wave ──────────────────────────────────────

    fn sine_wave(freq_hz: f32, sample_rate: u32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|i| (2.0 * PI * freq_hz * i as f32 / sample_rate as f32).sin())
            .collect()
    }

    // ── cross_correlate ───────────────────────────────────────────────────

    #[test]
    fn identical_signals_peak_at_zero() {
        let signal = sine_wave(440.0, 16_000, 1_600);
        let (offset, score) = cross_correlate(&signal, &signal).unwrap();
        assert_eq!(offset, 0, "identical signals should match at offset 0");
        assert!(score > 0.99, "score should be near 1.0, got {}", score);
    }

    #[test]
    fn shifted_signal_finds_correct_offset() {
        // Haystack: 2000-sample sine, needle: 800 samples taken from offset 300
        let haystack = sine_wave(440.0, 16_000, 2_000);
        let needle = haystack[300..1100].to_vec();
        let (offset, score) = cross_correlate(&haystack, &needle).unwrap();
        assert_eq!(
            offset, 300,
            "should find needle at offset 300, got {}",
            offset
        );
        assert!(score > 0.99, "score should be near 1.0, got {}", score);
    }

    #[test]
    fn noisy_shift_finds_approximate_offset() {
        // Use a frequency sweep (chirp) instead of a pure sine — a chirp is non-periodic
        // so the correlation has a unique global peak even with a tiny amount of noise.
        let haystack: Vec<f32> = (0..3_000usize)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                // Linear chirp from 200 Hz to 800 Hz over the haystack
                let freq = 200.0 + 600.0 * t;
                (2.0 * std::f32::consts::PI * freq * t).sin()
            })
            .collect();

        let true_offset = 500usize;
        let mut needle: Vec<f32> = haystack[true_offset..true_offset + 800].to_vec();

        // Add tiny deterministic noise (amplitude ≪ 1.0)
        for (i, s) in needle.iter_mut().enumerate() {
            let noise = ((i % 7) as f32 - 3.0) * 0.001;
            *s += noise;
        }

        let (offset, _score) = cross_correlate(&haystack, &needle).unwrap();
        assert_eq!(offset, true_offset);
    }

    #[test]
    fn silent_needle_returns_error() {
        let haystack = sine_wave(440.0, 16_000, 1_600);
        let silent_needle = vec![0.0f32; 400];
        let err = cross_correlate(&haystack, &silent_needle).unwrap_err();
        assert!(
            matches!(err, LimitcutError::SilentAudio),
            "expected SilentAudio, got {:?}",
            err
        );
    }

    #[test]
    fn needle_longer_than_haystack_returns_error() {
        let haystack = vec![1.0f32; 10];
        let needle = vec![1.0f32; 20];
        let err = cross_correlate(&haystack, &needle).unwrap_err();
        assert!(
            matches!(err, LimitcutError::NeedleTooLong { .. }),
            "expected NeedleTooLong, got {:?}",
            err
        );
    }

    #[test]
    fn low_score_returns_error() {
        // Haystack: pseudo-random noise, needle: a clean sine at a different frequency.
        // These share no structure so the correlation score will be below MIN_SCORE.
        let random_haystack: Vec<f32> = (0..3200usize)
            .map(|i| {
                // deterministic pseudo-random via Knuth multiplicative hash
                (i.wrapping_mul(2_654_435_761) >> 16) as f32 / 65_536.0 - 0.5
            })
            .collect();
        let sine_needle = sine_wave(100.0, 16_000, 800);
        let err = cross_correlate(&random_haystack, &sine_needle).unwrap_err();
        assert!(
            matches!(err, LimitcutError::CorrelationScoreTooLow { .. }),
            "expected CorrelationScoreTooLow, got {:?}",
            err
        );
    }

    #[test]
    fn single_sample_needle() {
        // Edge case: needle of length 1
        let haystack: Vec<f32> = vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let needle: Vec<f32> = vec![1.0];
        let (offset, score) = cross_correlate(&haystack, &needle).unwrap();
        assert_eq!(offset, 3);
        assert!(score > 0.99);
    }

    #[test]
    fn same_length_haystack_and_needle() {
        // Only one possible offset: 0
        let signal = sine_wave(440.0, 16_000, 800);
        let (offset, score) = cross_correlate(&signal, &signal).unwrap();
        assert_eq!(offset, 0);
        assert!(score > 0.99);
    }

    // ── cut_point_secs derivation ─────────────────────────────────────────

    #[test]
    fn cut_point_formula() {
        // If tail_start = 4.0s, offset = 0.5s → cut_point = 4.5s
        let tail_start = 4.0f64;
        let offset_samples = (0.5 * CORRELATION_SAMPLE_RATE as f64) as usize;
        let offset_secs = offset_samples as f64 / CORRELATION_SAMPLE_RATE as f64;
        let cut = tail_start + offset_secs;
        assert!(
            (cut - 4.5).abs() < 0.001,
            "cut should be ~4.5s, got {}",
            cut
        );
    }
}
