//! Signal-level gate against residual echo: mic audio that acoustic echo
//! cancellation (`echo_cancel.rs`) reduced but did not fully remove, which
//! Whisper then mishears as invented local-user speech (see
//! docs/transcription.md and the real d3863b42 recording this was written
//! against -- residual echo lines like "I can leave here and I mentioned the
//! 21st Thursday" time-aligned with, but textually different from, the
//! actual remote speech). `echo_dedup.rs`'s text-level comparison misses
//! these because the misheard text does not resemble the remote line closely
//! enough to clear its similarity threshold; this module instead looks at
//! the *signal* AEC left behind, before Whisper ever sees it, using the
//! per-block energy trace `echo_cancel::cancel_echo` now records.
use crate::echo_cancel::BlockEnergy;

/// A far-end-active region only counts as "mostly far-end" (and therefore a
/// residual-echo candidate at all) once this fraction of its blocks had
/// far-end signal present. A region far-end was active for only a sliver of
/// is predominantly the user's own independent speech, so it must never be
/// gated regardless of its energy.
const FAR_END_MAJORITY_FRACTION: f32 = 0.5;
/// Residual energy at or below this fraction of the recording's derived
/// near-end speech level is treated as "well below typical near-end speech" --
/// i.e. AEC left behind only echo tail, not the user's own voice. Calibrated
/// against this AEC's own real-world behavior (see docs/transcription.md and
/// `echo_cancel::bench`): on a short recording the two-pass adaptive filter
/// often has not fully converged, so residual echo commonly sits well above
/// a strict "near silence" bar (e.g. 0.35x) while still being clearly not the
/// user's own voice -- 0.6x still requires the residual to be meaningfully
/// quieter than typical near-end speech, just not vanishingly so.
const RESIDUAL_BELOW_NEAR_END_FRACTION: f32 = 0.6;
/// An echo-return-loss-enhancement-like suppression this large (mic energy
/// down to residual energy) is treated as evidence AEC removed most of the
/// block's energy, i.e. there was mostly echo there to remove. 10 dB (rather
/// than a stricter 15+ dB) matches the ERLE this AEC typically achieves on a
/// short, imperfectly-converged real recording.
const HIGH_SUPPRESSION_DB: f32 = 10.0;
/// Even at high suppression, a residual anywhere close to the near-end level
/// itself is still audible near-end-level speech -- real double talk can show
/// a large absolute dB drop (from a loud mixed-in echo) while still leaving
/// the user's own voice fully intact in the residual -- so the
/// high-suppression branch also requires the residual to be under the
/// near-end reference by a real margin, not merely equal to or below it.
const RESIDUAL_AT_OR_BELOW_NEAR_END_FRACTION: f32 = 0.7;
/// A conservative absolute near-end speech level (linear RMS, matching the
/// `[-1.0, 1.0]` normalized PCM used throughout this codebase) used when a
/// recording has no far-end-silent stretch to measure a real one from, or
/// when the user spoke little enough that far-end-silent stretches are
/// themselves near-total silence rather than actual near-end speech --
/// roughly -34 dBFS, comfortably below normal conversational speech but
/// above a typical residual-echo/background-noise floor, so it does not
/// itself masquerade as "near-end speech" for gating purposes.
const CONSERVATIVE_NEAR_END_RMS: f32 = 0.02;
/// Minimum number of far-end-silent blocks required before trusting a
/// measured near-end level over the conservative absolute floor above.
const MIN_FAR_END_SILENT_BLOCKS_FOR_ESTIMATE: usize = 10;
/// Percentile (of far-end-silent blocks' mic RMS) used as the near-end speech
/// level estimate. Not the median: most far-end-silent audio is itself quiet
/// (the user not currently talking), so a fairly high percentile is needed to
/// land on what the user's own speech actually sounds like rather than their
/// background noise floor.
const NEAR_END_LEVEL_PERCENTILE: f32 = 0.75;

/// Derives a per-recording estimate of "what does this user's own speech
/// sound like, post rejecting far-end-active blocks" from blocks where the
/// far end was silent (see module docs). Falls back to a conservative
/// absolute floor when there is not enough far-end-silent audio to measure
/// one (e.g. the remote side spoke throughout almost the whole recording).
pub fn derive_near_end_level_rms(blocks: &[BlockEnergy]) -> f32 {
    let mut silent_levels: Vec<f32> = blocks
        .iter()
        .filter(|block| !block.far_end_active)
        .map(|block| block.mic_rms)
        .collect();
    if silent_levels.len() < MIN_FAR_END_SILENT_BLOCKS_FOR_ESTIMATE {
        return CONSERVATIVE_NEAR_END_RMS;
    }
    silent_levels.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = (((silent_levels.len() - 1) as f32) * NEAR_END_LEVEL_PERCENTILE) as usize;
    silent_levels[index.min(silent_levels.len() - 1)].max(CONSERVATIVE_NEAR_END_RMS)
}

/// Whether the mic samples `[region_start, region_end)` (sample indices into
/// the same 16 kHz buffer `blocks` was recorded against) should be treated as
/// residual echo -- and therefore never sent to Whisper -- rather than real
/// near-end speech. `near_end_level_rms` should come from
/// `derive_near_end_level_rms` for the same track.
///
/// Deliberately conservative: any region where far-end was not active for a
/// majority of its blocks is real near-end candidate speech and is always
/// kept (double talk, and any region with no matching `blocks` at all --
/// e.g. AEC was never applied -- passes straight through).
pub fn is_residual_echo_region(
    blocks: &[BlockEnergy],
    region_start: usize,
    region_end: usize,
    near_end_level_rms: f32,
) -> bool {
    if blocks.is_empty() || region_end <= region_start {
        return false;
    }
    let overlapping: Vec<&BlockEnergy> = blocks
        .iter()
        .filter(|block| {
            let block_end = block.start_sample.saturating_add(block.len_samples);
            block.start_sample < region_end && block_end > region_start
        })
        .collect();
    if overlapping.is_empty() {
        return false;
    }
    let far_end_active_count = overlapping.iter().filter(|b| b.far_end_active).count();
    let far_end_fraction = far_end_active_count as f32 / overlapping.len() as f32;
    if far_end_fraction < FAR_END_MAJORITY_FRACTION {
        // Predominantly a near-end-only stretch: never a residual-echo
        // candidate, regardless of level -- this is the near-end
        // preservation guarantee.
        return false;
    }
    let active: Vec<&&BlockEnergy> = overlapping.iter().filter(|b| b.far_end_active).collect();
    let avg_mic_rms = mean(active.iter().map(|b| b.mic_rms));
    let avg_residual_rms = mean(active.iter().map(|b| b.residual_rms));
    if avg_mic_rms <= 0.0 {
        return false;
    }
    let well_below_near_end =
        avg_residual_rms <= near_end_level_rms * RESIDUAL_BELOW_NEAR_END_FRACTION;
    let suppression_db = if avg_residual_rms > 1e-9 {
        20.0 * (avg_mic_rms / avg_residual_rms).log10()
    } else {
        f32::INFINITY
    };
    let high_suppression = suppression_db > HIGH_SUPPRESSION_DB
        && avg_residual_rms <= near_end_level_rms * RESIDUAL_AT_OR_BELOW_NEAR_END_FRACTION;
    well_below_near_end || high_suppression
}

/// A looser signal-only check than `is_residual_echo_region`, used as one
/// half of the segment-level post-Whisper filter in `transcription_engine`
/// (problem 1b): true when far end was active for a majority of the
/// segment's overlapping blocks and the average residual left behind is no
/// more than half the near-end level -- on its own not damning enough to
/// drop a segment (a genuinely quiet near-end word could register this way
/// too), but combined with whisper.cpp's own low confidence in the same
/// segment (see the threshold constants in `transcription_engine`) it is.
pub fn overlaps_weak_far_end_residual(
    blocks: &[BlockEnergy],
    region_start: usize,
    region_end: usize,
    near_end_level_rms: f32,
) -> bool {
    const WEAK_RESIDUAL_FRACTION: f32 = 0.5;
    if blocks.is_empty() || region_end <= region_start {
        return false;
    }
    let overlapping: Vec<&BlockEnergy> = blocks
        .iter()
        .filter(|block| {
            let block_end = block.start_sample.saturating_add(block.len_samples);
            block.start_sample < region_end && block_end > region_start
        })
        .collect();
    if overlapping.is_empty() {
        return false;
    }
    let far_end_active_count = overlapping.iter().filter(|b| b.far_end_active).count();
    let far_end_fraction = far_end_active_count as f32 / overlapping.len() as f32;
    if far_end_fraction < FAR_END_MAJORITY_FRACTION {
        return false;
    }
    let active: Vec<&&BlockEnergy> = overlapping.iter().filter(|b| b.far_end_active).collect();
    let avg_residual_rms = mean(active.iter().map(|b| b.residual_rms));
    avg_residual_rms <= near_end_level_rms * WEAK_RESIDUAL_FRACTION
}

fn mean(values: impl Iterator<Item = f32> + Clone) -> f32 {
    let count = values.clone().count();
    if count == 0 {
        return 0.0;
    }
    values.sum::<f32>() / count as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start: usize, len: usize, far_end_active: bool, mic_rms: f32, residual_rms: f32) -> BlockEnergy {
        BlockEnergy {
            start_sample: start,
            len_samples: len,
            far_end_active,
            mic_rms,
            residual_rms,
        }
    }

    #[test]
    fn near_end_only_region_is_never_gated_even_at_low_energy() {
        // Far end never active in this region: whatever the (low) energy is,
        // this must be preserved -- it can only be near-end audio (or
        // silence), never residual echo.
        let blocks = vec![
            block(0, 160, false, 0.001, 0.001),
            block(160, 160, false, 0.001, 0.001),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn far_end_active_with_heavily_suppressed_residual_is_gated() {
        // Loud far end, AEC crushed it to near nothing: this is the
        // synthetic residual-echo case -- far below the derived near-end
        // level, so it must be gated.
        let blocks = vec![
            block(0, 160, true, 0.2, 0.0005),
            block(160, 160, true, 0.2, 0.0006),
        ];
        assert!(is_residual_echo_region(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn double_talk_with_residual_near_the_near_end_level_is_kept() {
        // Far end active, but the residual left behind is comparable to the
        // user's own normal speech level: real double talk, must survive.
        let blocks = vec![
            block(0, 160, true, 0.22, 0.018),
            block(160, 160, true, 0.21, 0.019),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn mixed_region_mostly_near_end_is_kept() {
        // Far end only active for a minority of the region's blocks: treat
        // the whole region as near-end candidate speech.
        let blocks = vec![
            block(0, 160, false, 0.02, 0.02),
            block(160, 160, false, 0.02, 0.02),
            block(320, 160, true, 0.2, 0.0005),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 480, 0.02));
    }

    #[test]
    fn empty_trace_is_never_gated() {
        // No AEC trace at all (e.g. AEC was not applied to this track):
        // nothing to gate on, so always keep.
        assert!(!is_residual_echo_region(&[], 0, 320, 0.02));
    }

    #[test]
    fn region_outside_all_blocks_is_kept() {
        let blocks = vec![block(0, 160, true, 0.2, 0.0005)];
        assert!(!is_residual_echo_region(&blocks, 10_000, 10_320, 0.02));
    }

    #[test]
    fn near_end_level_prefers_a_measured_estimate_over_the_conservative_floor() {
        let mut blocks = Vec::new();
        for _ in 0..20 {
            blocks.push(block(0, 160, false, 0.05, 0.05));
        }
        let level = derive_near_end_level_rms(&blocks);
        assert!(
            (level - 0.05).abs() < 1e-6,
            "expected the measured far-end-silent level, got {level}"
        );
    }

    #[test]
    fn weak_far_end_residual_flags_a_low_residual_span() {
        let blocks = vec![
            block(0, 160, true, 0.2, 0.005),
            block(160, 160, true, 0.2, 0.006),
        ];
        assert!(overlaps_weak_far_end_residual(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn weak_far_end_residual_does_not_flag_near_end_level_residual() {
        let blocks = vec![
            block(0, 160, true, 0.2, 0.02),
            block(160, 160, true, 0.2, 0.021),
        ];
        assert!(!overlaps_weak_far_end_residual(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn weak_far_end_residual_ignores_predominantly_near_end_spans() {
        let blocks = vec![
            block(0, 160, false, 0.01, 0.01),
            block(160, 160, false, 0.01, 0.01),
        ];
        assert!(!overlaps_weak_far_end_residual(&blocks, 0, 320, 0.02));
    }

    #[test]
    fn near_end_level_falls_back_to_the_conservative_floor_when_far_end_almost_always_active() {
        let mut blocks = Vec::new();
        for _ in 0..20 {
            blocks.push(block(0, 160, true, 0.3, 0.01));
        }
        assert_eq!(derive_near_end_level_rms(&blocks), CONSERVATIVE_NEAR_END_RMS);
    }
}
