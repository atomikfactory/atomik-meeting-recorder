//! Acoustic echo cancellation (AEC) for a recording made through loudspeakers
//! instead of headphones, where every remote participant's audio the
//! loopback track captures also leaks into the local microphone track and
//! shows up a second time in the transcript (see docs/transcription.md).
//! Runs entirely offline, in memory, on already-decoded PCM before the
//! silence pre-pass and Whisper in `transcription_engine.rs`; the recorded
//! WAV files on disk are never touched.
//!
//! This is an independent Rust implementation whose architecture -- a delay
//! estimator, a partitioned frequency-domain adaptive filter, a double-talk
//! detector, and a residual echo suppressor with a comfort floor -- is
//! modeled on the publicly documented design of WebRTC's AEC3. It is not a
//! line-by-line port; see `LICENSE-WEBRTC` for attribution and
//! docs/transcription.md for what is simplified.
mod delay;
mod filter;
mod suppressor;

use delay::DelayEstimator;
use filter::{AecConfig, PartitionedFilter};
use suppressor::{residual_gain, DoubleTalkDetector};

/// Below this RMS, `reference` is treated as silence: cancelling against it
/// would be a costly no-op (and dividing by near-zero energy anywhere in the
/// pipeline would be numerically fragile), so `mic` is returned unchanged.
const SILENT_REFERENCE_RMS: f32 = 1e-5;
/// A gentle single-pole DC/rumble high-pass, well below adult speech
/// fundamentals, applied to both signals before alignment/filtering so a DC
/// offset or sub-audible drift in either track cannot bias delay estimation
/// or the adaptive filter's power normalization.
const HIGH_PASS_HZ: f32 = 60.0;
/// A reference block whose RMS is below this fraction of the loudest
/// reference block seen anywhere in the recording is treated as having no
/// far-end signal at all, regardless of what the linear filter's own echo
/// estimate or the double-talk detector's energy-ratio heuristic say about
/// it (see `far_end_active_mask`). This is a hard backstop for near-end
/// preservation: a block with no far-end energy has nothing legitimate to
/// cancel, so any subtraction the filter still produces for it (e.g. from
/// weights that overfit a coincidental correlation, or a double-talk
/// classification an unusually loud, ratio-inflating echo estimate fooled)
/// is necessarily wrong, and is skipped outright rather than trusted. -26 dB
/// below the recording's own loudest reference activity comfortably clears
/// normal quiet-vs-loud speech dynamics while still catching genuine
/// silence/near-silence stretches.
const FAR_END_GATE_RATIO: f32 = 0.05;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EchoCancelStats {
    pub applied: bool,
    pub estimated_delay_ms: Option<f32>,
    /// Echo return loss enhancement, in dB, measured only over blocks this
    /// pass classified as far-end-only (see `DoubleTalkDetector`). `None`
    /// when too little far-end-only audio was seen to measure it (e.g. an
    /// input track that is mostly double talk, or has almost no matching
    /// echo at all).
    pub erle_db: Option<f32>,
}

/// Cancels acoustic echo from `mic` using `reference` (already summed, if
/// there were several loopback tracks) as the far-end signal. Both must
/// already be decoded to mono `f32` PCM at `sample_rate` -- this module does
/// no file I/O and no resampling. Returns a new buffer the same length as
/// `mic`; `mic` is never mutated in place. A silent or empty reference (or an
/// empty mic) is a documented no-op that returns `mic` unchanged.
pub fn cancel_echo(mic: &[f32], reference: &[f32], sample_rate: u32) -> (Vec<f32>, EchoCancelStats) {
    if mic.is_empty() || reference.is_empty() || sample_rate == 0 || is_silent(reference) {
        return (mic.to_vec(), EchoCancelStats::default());
    }
    let mic_hp = high_pass(mic, sample_rate);
    let mut reference_hp = high_pass(reference, sample_rate);
    reference_hp.resize(mic_hp.len(), 0.0);

    let (aligned_reference, delay_ms) =
        DelayEstimator::new().align(&mic_hp, &reference_hp, sample_rate);

    let config = AecConfig::for_sample_rate(sample_rate);
    let n = config.block_len;
    let mut linear_filter = PartitionedFilter::new(&config);
    let far_end_active = far_end_active_mask(&aligned_reference, n);

    // Pass 1: adapt over the whole recording at full speed, ignoring the
    // double-talk detector, and discard the output. This is the offline
    // trick a real-time canceller cannot use: a real-time double-talk
    // detector must protect the near end from block one, when the filter's
    // own echo estimate (what the detector compares the mic against) is
    // still all zero -- exactly when it is least trustworthy. Here, because
    // this pass's output is thrown away, it can afford to just adapt
    // through any double talk (near-end contributions are uncorrelated with
    // the reference and average out over the recording) and hand pass 2 an
    // already-converged filter.
    run_pass(&mut linear_filter, &mic_hp, &aligned_reference, n, None, false, &far_end_active);

    // Pass 2: replay from the start with the converged filter (continuing to
    // adapt, now double-talk aware since the echo estimate is trustworthy
    // from the first block, to track any residual drift within the
    // recording), this time keeping the output and the ERLE statistics.
    let mut output = Vec::with_capacity(mic_hp.len());
    let stats = run_pass(
        &mut linear_filter,
        &mic_hp,
        &aligned_reference,
        n,
        Some(&mut output),
        true,
        &far_end_active,
    );
    output.truncate(mic.len());

    (
        output,
        EchoCancelStats {
            applied: true,
            estimated_delay_ms: Some(delay_ms),
            erle_db: stats.erle_db(),
        },
    )
}

#[derive(Default)]
struct PassStats {
    far_end_mic_energy: f64,
    far_end_output_energy: f64,
}
impl PassStats {
    fn erle_db(&self) -> Option<f32> {
        (self.far_end_output_energy > 1e-9 && self.far_end_mic_energy > 0.0)
            .then(|| 10.0 * (self.far_end_mic_energy / self.far_end_output_energy).log10() as f32)
    }
}

fn run_pass(
    filter: &mut PartitionedFilter,
    mic: &[f32],
    reference: &[f32],
    n: usize,
    mut output: Option<&mut Vec<f32>>,
    respect_double_talk: bool,
    far_end_active: &[bool],
) -> PassStats {
    let mut dtd = DoubleTalkDetector::new();
    let mut stats = PassStats::default();
    let mut mic_block = vec![0f32; n];
    let mut ref_block = vec![0f32; n];
    let mut start = 0usize;
    let mut block_index = 0usize;
    while start < mic.len() {
        let end = (start + n).min(mic.len());
        let len = end - start;
        mic_block[..len].copy_from_slice(&mic[start..end]);
        mic_block[len..].fill(0.0);
        ref_block[..len].copy_from_slice(&reference[start..end]);
        ref_block[len..].fill(0.0);

        let echo_estimate = filter.estimate(&ref_block);
        // A block with (essentially) no far-end signal anywhere near it has
        // nothing legitimate to cancel: pass it through unchanged instead of
        // trusting whatever the linear filter's echo estimate says (see
        // `FAR_END_GATE_RATIO`). Still feed this block into the filter's
        // history via `estimate` above (needed for correct overlap-save
        // convolution on later blocks), just not into `adapt` -- there is no
        // real echo path information in silence to learn from.
        if !far_end_active.get(block_index).copied().unwrap_or(true) {
            if let Some(output) = output.as_deref_mut() {
                output.extend_from_slice(&mic_block[..len]);
            }
            start = end;
            block_index += 1;
            continue;
        }
        let error: Vec<f32> = mic_block
            .iter()
            .zip(&echo_estimate)
            .map(|(m, e)| m - e)
            .collect();
        let mic_energy = mic_block.iter().map(|v| v * v).sum::<f32>();
        let echo_energy = echo_estimate.iter().map(|v| v * v).sum::<f32>();
        let error_energy = error.iter().map(|v| v * v).sum::<f32>();
        let (classified_step, far_end_only) = dtd.classify(mic_energy, echo_energy);
        // Pass 1 still slows down (rather than fully ignores) the detector
        // during clear double talk -- a loud near end injects real gradient
        // noise, not just a fairness concern -- but never freezes as hard as
        // pass 2 does, since early on (echo estimate still near zero) the
        // detector's own input is not yet trustworthy either.
        let step_scale = if respect_double_talk {
            classified_step
        } else {
            classified_step.max(0.3)
        };
        filter.adapt(&error, step_scale);

        let gain = residual_gain(echo_energy, error_energy, classified_step);
        if far_end_only {
            stats.far_end_mic_energy += f64::from(mic_energy);
            stats.far_end_output_energy += f64::from(error_energy * gain * gain);
        }
        if let Some(output) = output.as_deref_mut() {
            output.extend(error[..len].iter().map(|v| v * gain));
        }
        start = end;
        block_index += 1;
    }
    stats
}

/// Marks every `n`-sample block of `reference` as having far-end signal
/// present or not, relative to the loudest block anywhere in the recording
/// (see `FAR_END_GATE_RATIO`). Computed once, offline, over the whole
/// (already delay-aligned) reference -- the two-pass trick `cancel_echo`
/// already relies on elsewhere -- rather than any online/adaptive floor, so
/// it cannot itself be fooled by a stretch of double talk or a slow-moving
/// noise floor.
fn far_end_active_mask(reference: &[f32], n: usize) -> Vec<bool> {
    if reference.is_empty() || n == 0 {
        return Vec::new();
    }
    let mut block_rms = Vec::with_capacity(reference.len() / n + 1);
    let mut start = 0usize;
    while start < reference.len() {
        let end = (start + n).min(reference.len());
        let energy: f32 = reference[start..end].iter().map(|v| v * v).sum();
        block_rms.push((energy / (end - start).max(1) as f32).sqrt());
        start = end;
    }
    let peak = block_rms.iter().cloned().fold(0.0f32, f32::max);
    let gate = (peak * FAR_END_GATE_RATIO).max(SILENT_REFERENCE_RMS);
    block_rms.into_iter().map(|rms| rms >= gate).collect()
}

fn is_silent(samples: &[f32]) -> bool {
    let energy: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    ((energy / samples.len().max(1) as f64).sqrt() as f32) < SILENT_REFERENCE_RMS
}

/// A single-pole (RC) high-pass filter, `y[n] = a*(y[n-1] + x[n] - x[n-1])`
/// with `a` set from `HIGH_PASS_HZ`. Simple on purpose: this is DC/rumble
/// removal, not a spectral shaping stage.
fn high_pass(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let dt = 1.0 / sample_rate as f32;
    let rc = 1.0 / (2.0 * std::f32::consts::PI * HIGH_PASS_HZ);
    let alpha = rc / (rc + dt);
    let mut out = Vec::with_capacity(samples.len());
    let mut prev_in = 0.0f32;
    let mut prev_out = 0.0f32;
    for &sample in samples {
        let y = alpha * (prev_out + sample - prev_in);
        out.push(y);
        prev_in = sample;
        prev_out = y;
    }
    out
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bench.rs"]
mod bench;
