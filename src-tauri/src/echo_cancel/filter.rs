//! Partitioned block frequency-domain adaptive filter (PBFDAF / "MDF"): the
//! linear echo-path model at the heart of the canceller. Architecturally this
//! plays the role of AEC3's refined adaptive filter (see `../LICENSE-WEBRTC`
//! for what "architecturally" means here); AEC3's separate low-latency
//! "coarse" filter is not ported -- one filter, sized for the whole target
//! echo-path length, is simpler and was sufficient to hit the ERLE target in
//! `echo_cancel_tests.rs`.
//!
//! One real FFT of size `2*block_len` is computed per input block and reused,
//! aged, across all partitions (the standard MDF trick: partition `m`'s
//! frequency-domain reference block is simply the block computed `m` blocks
//! ago), so cost stays O(M) complex multiplies per bin per block rather than
//! O(M) FFTs. Every scratch buffer is preallocated in `new` and reused by
//! `estimate`/`adapt`; no per-block heap allocation on the hot path.

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex32;
use std::{collections::VecDeque, sync::Arc};

/// Target linear filter length. WASAPI-loopback-to-USB-mic echo paths are
/// short (electrical/software loopback plus at most a little room
/// reflection off nearby surfaces near the mic), so 300 ms is generous;
/// widening this only costs CPU, not correctness, once the coarse delay
/// alignment in `delay.rs` has already removed the gross device latency.
const FILTER_MS: u32 = 300;
/// Adaptive-filter block period. 10 ms is the conventional real-time AEC
/// block size, but this canceller is offline: doubling it to 20 ms roughly
/// quarters the dominant per-block cost (the gradient-constraint FFT pair in
/// `adapt`, paid once per *partition* per block -- see there -- and
/// `partitions = FILTER_MS / BLOCK_MS` scales down with it too), for a
/// combined ~4x speedup, at the cost of halving the double-talk detector's
/// and the adaptive filter's time resolution. That resolution loss is safe
/// here because `delay.rs` already removes essentially all linear drift
/// before this filter ever sees the signal, leaving only short-timescale,
/// non-linear residual for it to track, and because both passes replay the
/// whole recording (see `cancel_echo`), so a slower-reacting double-talk
/// detector still converges well before the audio it is judging matters.
const BLOCK_MS: u32 = 20;
/// Per-block leak applied to every filter weight before its update, so a
/// long run of unlucky updates cannot let it drift away indefinitely.
const WEIGHT_LEAK: f32 = 0.99995;
/// Hard backstop on a partition weight's per-bin magnitude; a converged
/// filter representing a physically plausible echo path should never
/// approach this. See `adapt`.
const MAX_WEIGHT_MAGNITUDE: f32 = 6.0;

#[derive(Clone, Copy, Debug)]
pub struct AecConfig {
    pub sample_rate: u32,
    pub block_len: usize,
    pub partitions: usize,
    pub mu: f32,
    pub eps: f32,
}

impl AecConfig {
    pub fn for_sample_rate(sample_rate: u32) -> Self {
        let block_len = ((sample_rate as u64 * u64::from(BLOCK_MS)) / 1000).max(16) as usize;
        let partitions =
            (((sample_rate as u64 * u64::from(FILTER_MS)) / 1000) as usize / block_len).max(4);
        Self {
            sample_rate,
            block_len,
            partitions,
            // NLMS step size; per-bin normalization by that partition's own
            // power keeps this stable across widely different input levels,
            // and `adapt` further divides it across active partitions (see
            // there), so this can stay a fairly assertive base step.
            mu: 0.5,
            eps: 1e-6,
        }
    }
}

struct HistEntry {
    spectrum: Vec<Complex32>,
    power: Vec<f32>,
}

pub struct PartitionedFilter {
    n: usize,
    mu: f32,
    eps: f32,
    fft_fwd: Arc<dyn RealToComplex<f32>>,
    fft_inv: Arc<dyn ComplexToReal<f32>>,
    weights: Vec<Vec<Complex32>>,
    history: VecDeque<HistEntry>,
    max_partitions: usize,
    prev_ref_block: Vec<f32>,
    // Reused scratch buffers (avoid per-block/per-partition allocation).
    scratch_time: Vec<f32>,
    scratch_freq: Vec<Complex32>,
    scratch_time_b: Vec<f32>,
    scratch_freq_b: Vec<Complex32>,
    echo_freq: Vec<Complex32>,
    total_power: Vec<f32>,
}

impl PartitionedFilter {
    pub fn new(config: &AecConfig) -> Self {
        debug_assert!(config.sample_rate > 0, "AecConfig::sample_rate must be positive");
        let n = config.block_len;
        let fft_len = 2 * n;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(fft_len);
        let fft_inv = planner.plan_fft_inverse(fft_len);
        let bins = n + 1;
        Self {
            n,
            mu: config.mu,
            eps: config.eps,
            fft_fwd,
            fft_inv,
            weights: (0..config.partitions)
                .map(|_| vec![Complex32::new(0.0, 0.0); bins])
                .collect(),
            history: VecDeque::with_capacity(config.partitions),
            max_partitions: config.partitions,
            prev_ref_block: vec![0.0; n],
            scratch_time: vec![0.0; fft_len],
            scratch_freq: vec![Complex32::new(0.0, 0.0); bins],
            scratch_time_b: vec![0.0; fft_len],
            scratch_freq_b: vec![Complex32::new(0.0, 0.0); bins],
            echo_freq: vec![Complex32::new(0.0, 0.0); bins],
            total_power: vec![0.0; bins],
        }
    }

    /// Feeds one new `n`-sample reference block, ages the frequency-domain
    /// history, and returns this block's `n`-sample linear echo estimate.
    /// Must be called exactly once per block, in order, before `adapt`.
    pub fn estimate(&mut self, ref_block: &[f32]) -> Vec<f32> {
        debug_assert_eq!(ref_block.len(), self.n);
        self.scratch_time[..self.n].copy_from_slice(&self.prev_ref_block);
        self.scratch_time[self.n..].copy_from_slice(ref_block);
        self.prev_ref_block.copy_from_slice(ref_block);

        // Recycle the oldest entry's buffers once the history is full instead
        // of allocating a fresh `Vec` (and dropping the displaced one) on
        // every block: at steady state this is the only per-block allocation
        // `estimate` would otherwise make, over the tens of thousands of
        // blocks a long recording's two passes add up to.
        let mut entry = if self.history.len() >= self.max_partitions {
            self.history.pop_back().expect("max_partitions > 0")
        } else {
            HistEntry {
                spectrum: vec![Complex32::new(0.0, 0.0); self.n + 1],
                power: vec![0.0; self.n + 1],
            }
        };
        self.fft_fwd
            .process(&mut self.scratch_time, &mut entry.spectrum)
            .expect("fixed-size forward FFT");
        clean_real_edges(&mut entry.spectrum);
        for (power, bin) in entry.power.iter_mut().zip(entry.spectrum.iter()) {
            *power = bin.norm_sqr();
        }
        self.history.push_front(entry);

        for bin in self.echo_freq.iter_mut() {
            *bin = Complex32::new(0.0, 0.0);
        }
        for (weight, entry) in self.weights.iter().zip(self.history.iter()) {
            for k in 0..self.echo_freq.len() {
                self.echo_freq[k] += weight[k] * entry.spectrum[k];
            }
        }
        clean_real_edges(&mut self.echo_freq);
        self.fft_inv
            .process(&mut self.echo_freq, &mut self.scratch_time)
            .expect("fixed-size inverse FFT");
        // Overlap-save: only the second half of the 2N circular convolution
        // is the valid (unaliased) linear-convolution result.
        let scale = 1.0 / (2 * self.n) as f32;
        self.scratch_time[self.n..]
            .iter()
            .map(|v| v * scale)
            .collect()
    }

    /// Updates the filter weights from this block's error signal (mic minus
    /// the echo estimate just returned by `estimate`, after any suppression
    /// stage the caller applies is irrelevant here -- adaptation always uses
    /// the raw linear-filter error). `mu_scale` in `[0, 1]` scales the base
    /// step size; callers pass a small but nonzero value during detected
    /// double talk rather than fully freezing, so the filter can still track
    /// slow echo-path drift while the near end is talking.
    pub fn adapt(&mut self, error: &[f32], mu_scale: f32) {
        debug_assert_eq!(error.len(), self.n);
        if mu_scale <= 0.0 || self.history.is_empty() {
            return;
        }
        // Gradient-constraint input: zero-pad the error into the *first*
        // half of the 2N window (the constraint below then zeroes the
        // opposite half of each partition's time-domain gradient), matching
        // the overlap-save convolution structure used in `estimate`.
        for v in self.scratch_time.iter_mut().take(self.n) {
            *v = 0.0;
        }
        self.scratch_time[self.n..].copy_from_slice(error);
        self.fft_fwd
            .process(&mut self.scratch_time, &mut self.scratch_freq)
            .expect("fixed-size forward FFT");
        clean_real_edges(&mut self.scratch_freq);
        // Normalize every partition's update by the *total* reference power
        // at that bin summed across the whole filter memory, not just that
        // partition's own single-block instantaneous power. A lone block's
        // power is a noisy estimate for broadband input (it can be
        // momentarily near zero at a bin that is, on average, far from
        // quiet), which otherwise drives that bin's step size sky-high for
        // one block at a time and never lets it settle; summing across all
        // `history.len()` blocks currently held is a much steadier
        // denominator, roughly averaging power over the whole filter length.
        // This also spreads the (shared-error) step across partitions, so
        // the effective loop gain does not scale with the partition count.
        for bin in self.total_power.iter_mut() {
            *bin = self.eps;
        }
        for entry in &self.history {
            for k in 0..self.total_power.len() {
                self.total_power[k] += entry.power[k];
            }
        }
        let step = self.mu * mu_scale;
        for (weight, entry) in self.weights.iter_mut().zip(self.history.iter()) {
            for k in 0..self.scratch_freq_b.len() {
                let grad = entry.spectrum[k].conj() * self.scratch_freq[k];
                self.scratch_freq_b[k] = grad / self.total_power[k] * step;
            }
            clean_real_edges(&mut self.scratch_freq_b);
            // Constrain the gradient to a causal N-tap impulse response
            // before applying it, so repeated updates cannot let a
            // partition's effective filter alias/wrap around the 2N block
            // (the classic PBFDAF "gradient constraint").
            self.fft_inv
                .process(&mut self.scratch_freq_b, &mut self.scratch_time_b)
                .expect("fixed-size inverse FFT");
            let scale = 1.0 / (2 * self.n) as f32;
            for v in self.scratch_time_b.iter_mut().take(self.n) {
                *v *= scale;
            }
            for v in self.scratch_time_b[self.n..].iter_mut() {
                *v = 0.0;
            }
            self.fft_fwd
                .process(&mut self.scratch_time_b, &mut self.scratch_freq_b)
                .expect("fixed-size forward FFT");
            clean_real_edges(&mut self.scratch_freq_b);
            for k in 0..weight.len() {
                // A tiny leak bleeds off any energy an unlucky sequence of
                // updates accumulates, and the hard magnitude clamp is a
                // last-resort backstop against runaway divergence (e.g. a
                // pathological, near-silent-but-not-quite input driving the
                // per-bin power normalization close to `eps`) rather than a
                // tuning parameter -- it should not normally bind.
                let updated = weight[k] * WEIGHT_LEAK + self.scratch_freq_b[k];
                let magnitude = updated.norm();
                weight[k] = if magnitude > MAX_WEIGHT_MAGNITUDE {
                    updated * (MAX_WEIGHT_MAGNITUDE / magnitude)
                } else {
                    updated
                };
            }
            clean_real_edges(weight);
        }
    }

    pub fn block_len(&self) -> usize {
        self.n
    }
}

/// realfft's inverse (complex-to-real) transform requires the DC and Nyquist
/// bins to be exactly real; a fresh forward (real-to-complex) transform
/// guarantees that, but the spectra here are built up through elementwise
/// complex arithmetic (sums of products, divisions), which does not
/// preserve an exact-zero imaginary part at those two bins even when it
/// started at exactly `0.0`. Clearing them defensively before every inverse
/// transform is cheap and avoids depending on that not happening.
fn clean_real_edges(spectrum: &mut [Complex32]) {
    if let Some(first) = spectrum.first_mut() {
        first.im = 0.0;
    }
    if let Some(last) = spectrum.last_mut() {
        last.im = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pure_delay_converges_to_low_residual_energy() {
        let sample_rate = 16_000u32;
        let config = AecConfig::for_sample_rate(sample_rate);
        let mut filter = PartitionedFilter::new(&config);
        let n = filter.block_len();
        // Reference: filtered noise-ish signal (sum of a few tones so the
        // spectrum isn't a single line, which would make the filter's
        // problem degenerate).
        let total_blocks = 400;
        let make = |i: usize| -> f32 {
            let t = i as f32 / sample_rate as f32;
            0.2 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                + 0.1 * (2.0 * std::f32::consts::PI * 850.0 * t).sin()
        };
        let echo_gain = 0.6;
        let delay = 5usize; // samples, well inside one partition
        let mut last_error_energy = 0.0f32;
        let mut last_mic_energy = 0.0f32;
        for block in 0..total_blocks {
            let mut ref_block = vec![0.0f32; n];
            let mut mic_block = vec![0.0f32; n];
            for i in 0..n {
                let index = block * n + i;
                ref_block[i] = make(index);
                let echo_index = index as i64 - delay as i64;
                let echo = if echo_index >= 0 {
                    make(echo_index as usize) * echo_gain
                } else {
                    0.0
                };
                mic_block[i] = echo;
            }
            let echo_estimate = filter.estimate(&ref_block);
            let error: Vec<f32> = mic_block
                .iter()
                .zip(&echo_estimate)
                .map(|(m, e)| m - e)
                .collect();
            if block >= total_blocks - 5 {
                last_error_energy += error.iter().map(|v| v * v).sum::<f32>();
                last_mic_energy += mic_block.iter().map(|v| v * v).sum::<f32>();
            }
            filter.adapt(&error, 1.0);
        }
        let erle = 10.0 * (last_mic_energy / last_error_energy.max(1e-12)).log10();
        assert!(erle > 20.0, "ERLE too low after convergence: {erle} dB");
    }
}
