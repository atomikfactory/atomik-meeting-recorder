//! Double-talk detection and residual echo suppression: the two stages AEC3
//! runs around its linear filter (`filter.rs`) to handle what the linear
//! model alone cannot -- deciding when *not* to adapt, and cleaning up the
//! echo the (imperfectly converged, imperfectly linear) filter leaves behind.
//! Both are deliberately simple energy-ratio heuristics rather than AEC3's
//! coherence-based double-talk detector or its trained residual-echo model;
//! see `../LICENSE-WEBRTC`.

/// Below this ratio of mic energy to predicted echo energy, a block is
/// classed as far-end-only: safe to adapt at full speed and to count toward
/// the ERLE estimate.
const FAR_END_RATIO: f32 = 2.0;
/// Above this ratio, a block is classed as near-end (double talk, or the mic
/// speaker not the far end): adaptation is slowed to a crawl rather than
/// stopped outright, so slow echo-path drift is still tracked through a long
/// double-talk stretch.
const NEAR_END_RATIO: f32 = 6.0;
/// Never fully freeze -- a small residual step keeps the filter from going
/// stale during long double talk.
const MIN_STEP_SCALE: f32 = 0.05;

/// A coarse, energy-ratio double-talk detector (Geigel-family: comparing mic
/// energy against the filter's own echo-path prediction rather than the raw
/// reference, which is more robust to echo-path gain than the classic
/// Geigel's fixed threshold on |reference|).
pub struct DoubleTalkDetector {
    smoothed_ratio: f32,
}

impl DoubleTalkDetector {
    pub fn new() -> Self {
        Self {
            smoothed_ratio: 0.0,
        }
    }

    /// Returns the step-size multiplier in `[MIN_STEP_SCALE, 1.0]` for this
    /// block, and whether it should be counted as "far-end-only" for ERLE
    /// purposes (a stricter, unsmoothed classification so a handful of noisy
    /// blocks around a talk-spurt boundary don't skew the reported ERLE).
    pub fn classify(&mut self, mic_energy: f32, echo_estimate_energy: f32) -> (f32, bool) {
        let ratio = if echo_estimate_energy > 1e-9 {
            mic_energy / echo_estimate_energy
        } else if mic_energy > 1e-9 {
            f32::MAX
        } else {
            1.0
        };
        self.smoothed_ratio = 0.7 * self.smoothed_ratio + 0.3 * ratio.min(1.0e6);
        let step_scale = if self.smoothed_ratio <= FAR_END_RATIO {
            1.0
        } else if self.smoothed_ratio >= NEAR_END_RATIO {
            MIN_STEP_SCALE
        } else {
            1.0 - (self.smoothed_ratio - FAR_END_RATIO) / (NEAR_END_RATIO - FAR_END_RATIO)
        };
        let far_end_only = ratio <= FAR_END_RATIO;
        (step_scale, far_end_only)
    }
}

impl Default for DoubleTalkDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Fraction of the predicted echo assumed to leak through the linear filter
/// as residual (imperfect convergence, nonlinearities the linear model can't
/// represent). Larger values suppress more aggressively.
const LEAKAGE: f32 = 0.2;
/// Suppression floor when the block looks purely far-end: leave room for the
/// suppressor to meaningfully clean up the linear filter's leftovers.
const FAR_END_FLOOR: f32 = 0.12;
/// Suppression floor as a block looks more like double talk: the near end's
/// own voice must never be gated toward silence, which the project brief
/// calls the worst failure mode, so the floor rises the less confident this
/// module is that a loud block is "just" unconverged echo.
const NEAR_END_FLOOR: f32 = 0.45;

/// Residual echo suppression gain for one block, applied to the linear
/// filter's error signal. `step_scale` (from `DoubleTalkDetector::classify`)
/// stands in for how likely this block is to be genuine double talk: at
/// `step_scale` near 0 (near-end speech), both the assumed leak and the
/// comfort floor relax to protect the near end's own voice; near 1 (far-end
/// only), the floor tightens so real residual echo is still cleaned up.
pub fn residual_gain(echo_estimate_energy: f32, error_energy: f32, step_scale: f32) -> f32 {
    if error_energy <= 1e-12 {
        return 1.0;
    }
    let step_scale = step_scale.clamp(0.0, 1.0);
    let assumed_leak = echo_estimate_energy * LEAKAGE * step_scale.max(MIN_STEP_SCALE);
    let gain = 1.0 - (assumed_leak / (error_energy + assumed_leak)).sqrt();
    let floor = NEAR_END_FLOOR + (FAR_END_FLOOR - NEAR_END_FLOOR) * step_scale;
    gain.clamp(floor, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn far_end_dominant_energy_adapts_at_full_speed() {
        let mut dtd = DoubleTalkDetector::new();
        let (scale, far_end_only) = dtd.classify(1.0, 1.0);
        assert_eq!(scale, 1.0);
        assert!(far_end_only);
    }

    #[test]
    fn near_end_dominant_energy_slows_adaptation_but_never_stops_it() {
        let mut dtd = DoubleTalkDetector::new();
        for _ in 0..10 {
            dtd.classify(20.0, 0.1);
        }
        let (scale, far_end_only) = dtd.classify(20.0, 0.1);
        assert!(scale < 0.2, "expected near-frozen adaptation, got {scale}");
        assert!(scale >= MIN_STEP_SCALE);
        assert!(!far_end_only);
    }

    #[test]
    fn suppression_never_gates_below_the_comfort_floor() {
        let gain = residual_gain(1000.0, 0.01, 1.0);
        assert!(gain >= FAR_END_FLOOR);
    }
}
