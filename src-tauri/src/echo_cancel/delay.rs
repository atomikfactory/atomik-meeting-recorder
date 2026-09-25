//! Coarse device-latency and clock-drift alignment via GCC-PHAT
//! cross-correlation. Architecturally this plays the role of AEC3's matched-
//! filter delay estimator (see `../LICENSE-WEBRTC`), computed once globally
//! over the whole recording and then re-estimated every `BLOCK_SECONDS` so
//! the residual delay the adaptive filter (`filter.rs`) has to represent
//! stays small even though two independent USB audio devices can drift by
//! tens of ms over an hour, or (rarely) jump if a device resets mid-capture.
//! This offline two-stage split -- coarse alignment, then a short adaptive
//! filter -- is the main way this module can afford to be simpler than a
//! real-time AEC while still converging fast: a real-time system cannot
//! re-scan the whole file before starting.

use realfft::RealFftPlanner;
use rustfft::num_complex::Complex32;

const GLOBAL_SEARCH_MS: i64 = 1_000;
const LOCAL_SEARCH_MS: i64 = 150;
const BLOCK_SECONDS: f32 = 0.5;
const ANALYSIS_SECONDS: f32 = 0.75;
/// GCC-PHAT scores are normalized by FFT length before being compared
/// against this floor (see `gcc_phat_lag`), so it is comparable across the
/// differently-sized local and global searches. Below it, the local
/// (drift-tracking) search is treated as having lost the echo path -- e.g. a
/// genuine delay jump larger than the local search window, or a stretch with
/// no far-end energy to lock onto -- and a full global re-search is run for
/// that block instead.
///
/// On real (not synthetic-tone) recordings, broadband speech-band audio
/// through a real echo path normalizes to a much lower *typical* peak
/// GCC-PHAT score than the near-pure-tone fixtures in `tests.rs` suggest --
/// five real two-track recordings all showed well-tracked local blocks
/// (consistent, slowly-drifting shifts, matching a full global re-search
/// when one was forced) scoring anywhere from ~0.02 to ~0.13 block to block,
/// with a median around 0.07. A single block dipping under this floor is
/// therefore common and usually just score noise, not lost lock -- but it is
/// exactly what a genuine delay jump or a briefly far-end-silent stretch
/// also looks like on its first block, so the floor itself has to stay where
/// it is (lowering it let a wrong, periodicity-aliased local peak through
/// uncorrected on the synthetic pure-tone fixture in `recovers_a_known_
/// fixed_delay`). See `REACQUIRE_STREAK` for how the cost of that normal
/// score noise is avoided without relaxing the floor.
const REACQUIRE_SCORE_FLOOR: f32 = 0.03;
/// Number of *consecutive* blocks that must score under `REACQUIRE_SCORE_
/// FLOOR` before paying for an expensive full-range (`GLOBAL_SEARCH_MS`)
/// re-search. On real recordings a lone block dipping under the floor is
/// common (see there) and, when checked, its own local estimate almost
/// always already agreed with what the expensive reacquire would have found
/// -- broadband GCC-PHAT score is noisy enough block to block that isolated
/// dips are not a reliable signal of actually losing lock, only a sustained
/// run of them is. Requiring 2 in a row cut the reacquire rate on real
/// recordings from roughly two-thirds of all blocks to a small few percent
/// (see `echo_cancel::bench`), while still reacquiring within one extra
/// block of any genuine, sustained loss of lock (a real delay jump or a
/// silent stretch keeps scoring low on every subsequent block, not just
/// one).
const REACQUIRE_STREAK: u32 = 2;

pub struct DelayEstimator {
    planner: RealFftPlanner<f32>,
}

impl DelayEstimator {
    pub fn new() -> Self {
        Self {
            planner: RealFftPlanner::<f32>::new(),
        }
    }

    /// Returns a reference buffer the same length as `mic`, aligned to the
    /// estimated delay of `reference` relative to `mic`, plus the first
    /// block's estimated delay in milliseconds (for reporting/tests). The
    /// delay is re-estimated every `BLOCK_SECONDS`, and the shift actually
    /// applied is *linearly interpolated* between consecutive estimates
    /// (fractional, sub-sample shifts included) rather than held constant
    /// for each block: a constant-rate clock drift is linear in time by
    /// definition, so this corrects it essentially exactly, leaving the
    /// adaptive filter with only the (small, genuinely non-linear) residual
    /// to track. Holding the shift piecewise-constant instead would leave a
    /// sawtooth of up to a full block's worth of drift for the filter to
    /// re-absorb every `BLOCK_SECONDS` -- and broadband audio decorrelates
    /// fast under even a few samples of sub-sample misalignment, so that
    /// residual would matter far more than it might sound.
    pub fn align(&mut self, mic: &[f32], reference: &[f32], sample_rate: u32) -> (Vec<f32>, f32) {
        if mic.is_empty() || reference.is_empty() || sample_rate == 0 {
            return (vec![0.0; mic.len()], 0.0);
        }
        let global_shift = self.estimate(mic, reference, sample_rate, 0, 0, GLOBAL_SEARCH_MS).0;
        let block_len = ((sample_rate as f32 * BLOCK_SECONDS) as usize).max(1);

        // Pass 1: one delay estimate anchored at the start of each block.
        let mut anchors: Vec<usize> = Vec::new();
        let mut shifts: Vec<f64> = Vec::new();
        let mut shift = global_shift;
        let mut start = 0usize;
        let mut low_score_streak = 0u32;
        while start < mic.len() {
            let (local_shift, local_score) =
                self.estimate(mic, reference, sample_rate, start, shift, LOCAL_SEARCH_MS);
            shift = if local_score >= REACQUIRE_SCORE_FLOOR {
                low_score_streak = 0;
                local_shift
            } else {
                low_score_streak += 1;
                // The very first block has no established lock yet to trust
                // a lone low score against (there is no "previous, presumably
                // still-correct shift" to fall back on the way there is once
                // tracking is under way) -- always reacquire immediately for
                // it, same as every block used to, rather than risking a
                // periodicity-aliased local peak going unchecked from the
                // very start of the recording.
                if start > 0 && low_score_streak < REACQUIRE_STREAK {
                    // A single block dipping under the floor is common score
                    // noise on real recordings (see `REACQUIRE_STREAK`):
                    // trust this block's own local estimate rather than
                    // paying for a full re-search that would almost always
                    // just agree with it anyway.
                    local_shift
                } else {
                    // A *sustained* run of low scores: drift tracking really
                    // has lost lock (or a delay jump bigger than the local
                    // window), so it is worth paying for a full-range search.
                    let (reacquired, reacquired_score) =
                        self.estimate(mic, reference, sample_rate, start, 0, GLOBAL_SEARCH_MS);
                    if reacquired_score < REACQUIRE_SCORE_FLOOR {
                        // Neither search found anything to lock onto (e.g. a
                        // stretch where the far end is silent): holding the
                        // last known-good shift is far safer than anchoring
                        // this block's essentially-random noise-fit
                        // estimate, which would otherwise corrupt the linear
                        // interpolation through this whole stretch,
                        // including at its edges where real signal resumes.
                        shift
                    } else {
                        reacquired
                    }
                }
            };
            anchors.push(start);
            shifts.push(shift as f64);
            start += block_len;
        }
        debug_assert!(!shifts.is_empty());
        // A trailing anchor at the end lets the final block ramp linearly
        // too, instead of holding flat right up to EOF.
        anchors.push(mic.len());
        shifts.push(*shifts.last().expect("at least one block anchor"));

        // Pass 2: fractionally-interpolated resample of `reference` using
        // the shift linearly interpolated between each pair of anchors.
        let mut aligned = vec![0.0f32; mic.len()];
        for segment in 0..anchors.len() - 1 {
            let (t0, t1) = (anchors[segment], anchors[segment + 1]);
            let (s0, s1) = (shifts[segment], shifts[segment + 1]);
            let span = (t1 - t0).max(1) as f64;
            for (i, out) in aligned[t0..t1].iter_mut().enumerate() {
                let frac = i as f64 / span;
                let shift = s0 + (s1 - s0) * frac;
                *out = interpolate(reference, (t0 + i) as f64 - shift);
            }
        }
        let first_shift_ms = shifts[0] as f32 * 1000.0 / sample_rate as f32;
        (aligned, first_shift_ms)
    }

    /// GCC-PHAT delay estimate: the shift `d` (samples) such that
    /// `reference[t - d]` best predicts `mic[t]` for `t` near `probe_start`,
    /// searched within `search_ms` of `center_shift`. Also returns the raw
    /// (unnormalized) peak correlation score, comparable only against other
    /// calls with the same analysis window length.
    fn estimate(
        &mut self,
        mic: &[f32],
        reference: &[f32],
        sample_rate: u32,
        probe_start: usize,
        center_shift: i64,
        search_ms: i64,
    ) -> (i64, f32) {
        let window = ((sample_rate as f32 * ANALYSIS_SECONDS) as usize)
            .min(mic.len().saturating_sub(probe_start));
        if window < 16 {
            return (center_shift, 0.0);
        }
        let sig = &mic[probe_start..probe_start + window];
        let max_shift = ((search_ms * i64::from(sample_rate)) / 1000).max(1);
        let ref_abs_start = probe_start as i64 - center_shift - max_shift;
        let ref_len = (2 * max_shift) as usize + window;
        let mut ref_snip = vec![0.0f32; ref_len];
        for (i, sample) in ref_snip.iter_mut().enumerate() {
            let idx = ref_abs_start + i as i64;
            if idx >= 0 && (idx as usize) < reference.len() {
                *sample = reference[idx as usize];
            }
        }
        let (lag, score) = self.gcc_phat_lag(sig, &ref_snip);
        // sig[i] is compared against ref_snip[lag + i], whose absolute index
        // is ref_abs_start + lag + i = (probe_start - center_shift -
        // max_shift) + lag + i. We want that to equal probe_start + i - d,
        // so d = center_shift + max_shift - lag.
        (center_shift + max_shift - lag as i64, score)
    }

    /// Full (non-circular-wraparound) GCC-PHAT cross-correlation of `sig`
    /// against every offset of `sig` within `ref_snip` (`ref_snip` must be at
    /// least as long as `sig`). Returns the best offset and its score.
    fn gcc_phat_lag(&mut self, sig: &[f32], ref_snip: &[f32]) -> (usize, f32) {
        let fft_len = (sig.len() + ref_snip.len()).next_power_of_two();
        let r2c = self.planner.plan_fft_forward(fft_len);
        let c2r = self.planner.plan_fft_inverse(fft_len);
        let mut a = r2c.make_input_vec();
        let mut b = r2c.make_input_vec();
        a[..ref_snip.len()].copy_from_slice(ref_snip);
        b[..sig.len()].copy_from_slice(sig);
        let mut fa = r2c.make_output_vec();
        let mut fb = r2c.make_output_vec();
        r2c.process(&mut a, &mut fa).expect("fixed-size forward FFT");
        r2c.process(&mut b, &mut fb).expect("fixed-size forward FFT");
        let mut cross: Vec<Complex32> = fa
            .iter()
            .zip(fb.iter())
            .map(|(x, y)| x * y.conj())
            .collect();
        // Plain PHAT (divide by |cross|) assumes a roughly flat spectrum; on
        // a spectrum with near-silent bins (e.g. a narrowband signal, or
        // simply the FFT bins between two speech formants) it instead
        // amplifies whatever floating-point noise happens to be in the
        // near-zero bins up to unit magnitude, swamping the real peak. Floor
        // the divisor at a small fraction of the spectrum's own mean
        // magnitude instead of an absolute epsilon, a standard regularized
        // PHAT variant, so quiet bins are suppressed rather than amplified.
        let mean_mag: f32 =
            cross.iter().map(|c| c.norm()).sum::<f32>() / cross.len().max(1) as f32;
        let floor = (mean_mag * 1e-2).max(1e-12);
        for c in cross.iter_mut() {
            let mag = c.norm();
            *c /= mag.max(floor);
        }
        let mut corr = c2r.make_output_vec();
        c2r.process(&mut cross, &mut corr)
            .expect("fixed-size inverse FFT");
        // realfft's forward+inverse pair scales by `fft_len`; divide it back
        // out so scores are comparable across the differently-sized local
        // and global searches (see `REACQUIRE_SCORE_FLOOR`).
        let norm = 1.0 / fft_len as f32;
        let valid_len = (ref_snip.len() - sig.len() + 1).min(corr.len());
        let (best_index, best_value) = corr[..valid_len]
            .iter()
            .enumerate()
            .fold((0usize, f32::MIN), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
        (best_index, best_value * norm)
    }
}

impl Default for DelayEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear-interpolated sample of `reference` at fractional index `pos`;
/// out-of-range reads (before the start or past the end) are treated as
/// silence rather than erroring, matching `reference`'s own zero-padding.
fn interpolate(reference: &[f32], pos: f64) -> f32 {
    let base = pos.floor();
    let frac = (pos - base) as f32;
    let read = |index: f64| -> f32 {
        if index >= 0.0 && (index as usize) < reference.len() {
            reference[index as usize]
        } else {
            0.0
        }
    };
    let a = read(base);
    let b = read(base + 1.0);
    a + (b - a) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_known_fixed_delay() {
        let sample_rate = 16_000u32;
        let seconds = 6.0f32;
        let total = (sample_rate as f32 * seconds) as usize;
        // Two non-harmonically-related tones (1150/317 is not a small integer
        // ratio) rather than e.g. 300+1200 Hz: exact harmonics make the
        // combined waveform perfectly periodic at the fundamental, which
        // gives GCC-PHAT a whole comb of equally-good aliased correlation
        // peaks (real speech never does this) and is a fixture artifact, not
        // something the estimator needs to handle.
        let reference: Vec<f32> = (0..total)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                0.3 * (2.0 * std::f32::consts::PI * 317.0 * t).sin()
                    + 0.15 * (2.0 * std::f32::consts::PI * 1150.0 * t).sin()
            })
            .collect();
        let true_delay = (0.083 * sample_rate as f32) as usize; // 83ms
        let mut mic = vec![0.0f32; total];
        mic[true_delay..total].copy_from_slice(&reference[..total - true_delay]);
        let mut estimator = DelayEstimator::new();
        let (_, delay_ms) = estimator.align(&mic, &reference, sample_rate);
        let expected_ms = true_delay as f32 * 1000.0 / sample_rate as f32;
        assert!(
            (delay_ms - expected_ms).abs() < 3.0,
            "expected ~{expected_ms}ms, got {delay_ms}ms"
        );
    }
}
