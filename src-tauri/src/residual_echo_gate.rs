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

/// Minimum number of `far_end_only` (converged, near-end-free) blocks with
/// measurable reference energy required before `estimate_echo_coupling`
/// trusts a per-recording coupling ratio at all. Below this, there is too
/// little clean evidence of the echo path to extrapolate from, and the
/// unconverged-region check in `is_residual_echo_region` is skipped entirely
/// (falling back to the suppression-based checks alone).
const MIN_FAR_END_ONLY_BLOCKS_FOR_COUPLING: usize = 8;
/// Percentile (of per-block mic/reference ratios over far-end-only blocks)
/// used as the coupling estimate. The median, rather than a mean, so a
/// handful of outlier blocks (e.g. right at a talk-spurt boundary, where
/// `far_end_only` classification is noisiest) cannot skew the estimate that
/// every unconverged region gets compared against.
const COUPLING_PERCENTILE: f32 = 0.5;
/// A predicted echo level within this fractional margin of the observed raw
/// mic level still counts as "explained by predicted echo" -- the coupling
/// ratio is itself only a median-of-samples estimate, so a small margin
/// avoids being fooled by ordinary measurement noise into treating a region
/// as having *excess* (i.e. near-end) energy it does not really have.
const COUPLING_EXPLAINED_MARGIN: f32 = 0.5;
/// Energy above the coupling prediction is only treated as genuine
/// near-end evidence once it exceeds this fraction of the recording's own
/// near-end speech level -- a small overshoot is expected estimation noise,
/// not proof of near-end content.
const COUPLING_EXCESS_NEAR_END_FRACTION: f32 = 0.5;
/// Mic segments with this many words or fewer are eligible for the
/// text+timing tiny-segment rule below (see `is_tiny_far_end_fragment`).
const TINY_SEGMENT_MAX_WORDS: usize = 2;
/// Guards the tiny-segment rule against gating a genuine, if brief, piece of
/// double talk: the region's average mic level must not clearly look like
/// the user's own normal speech (i.e. must stay at or below the near-end
/// level estimate) for the rule to fire.
const TINY_SEGMENT_NEAR_END_FRACTION: f32 = 1.0;
/// Bounds `is_leading_alignment_dead_zone_fragment`'s "recording just
/// started" window: 8,000 samples at 16 kHz is 500 ms, comfortably above any
/// plausible acoustic+processing echo delay `echo_cancel::DelayEstimator`
/// would report, but far below a real few-second stretch of the user
/// speaking alone before the remote side joins in -- so this only ever
/// matches the alignment artifact the rule exists for, never a genuine
/// near-end-only opening.
const LEADING_DEAD_ZONE_MAX_SAMPLES: usize = 8_000;

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

/// Blocks from `blocks` whose sample span overlaps `[region_start, region_end)`.
fn overlapping_blocks(
    blocks: &[BlockEnergy],
    region_start: usize,
    region_end: usize,
) -> Vec<&BlockEnergy> {
    blocks
        .iter()
        .filter(|block| {
            let block_end = block.start_sample.saturating_add(block.len_samples);
            block.start_sample < region_end && block_end > region_start
        })
        .collect()
}

/// Estimates the echo path's coupling for a recording: the ratio between the
/// far-end (loopback) level and the echo level actually observed in the raw
/// mic, measured only over blocks `far_end_only` marked as converged and
/// near-end-free (see `BlockEnergy::far_end_only`). Once known, this ratio
/// lets `is_residual_echo_region` predict the *expected* echo level for any
/// far-end-active block -- including one AEC has not converged on yet, where
/// the achieved suppression alone would badly underestimate how much of the
/// block's energy is really just echo. Returns `None` when there is not
/// enough clean far-end-only audio to trust an estimate (see
/// `MIN_FAR_END_ONLY_BLOCKS_FOR_COUPLING`).
pub fn estimate_echo_coupling(blocks: &[BlockEnergy]) -> Option<f32> {
    let mut ratios: Vec<f32> = blocks
        .iter()
        .filter(|b| b.far_end_only && b.reference_rms > 1e-6)
        .map(|b| b.mic_rms / b.reference_rms)
        .filter(|ratio| ratio.is_finite() && *ratio >= 0.0)
        .collect();
    if ratios.len() < MIN_FAR_END_ONLY_BLOCKS_FOR_COUPLING {
        return None;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = (((ratios.len() - 1) as f32) * COUPLING_PERCENTILE) as usize;
    Some(ratios[index.min(ratios.len() - 1)])
}

/// Whether the mic samples `[region_start, region_end)` (sample indices into
/// the same 16 kHz buffer `blocks` was recorded against) should be treated as
/// residual echo -- and therefore never sent to Whisper -- rather than real
/// near-end speech. `near_end_level_rms` should come from
/// `derive_near_end_level_rms` and `echo_coupling` from
/// `estimate_echo_coupling`, both for the same track.
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
    echo_coupling: Option<f32>,
) -> bool {
    if blocks.is_empty() || region_end <= region_start {
        return false;
    }
    let overlapping = overlapping_blocks(blocks, region_start, region_end);
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
    // Unconverged-region check: predict what this block's *raw* mic energy
    // should look like purely from echo (far-end level x this recording's own
    // measured coupling), independent of however much or little AEC actually
    // suppressed here. This is what lets a region AEC has not converged on
    // yet -- real suppression well under `HIGH_SUPPRESSION_DB` -- still be
    // recognized as residual echo, as long as the raw mic energy is fully
    // explainable by the predicted echo and there's no sign of extra
    // (near-end) energy on top of it.
    let explained_by_predicted_echo = echo_coupling.is_some_and(|coupling| {
        let avg_reference_rms = mean(active.iter().map(|b| b.reference_rms));
        if avg_reference_rms <= 0.0 {
            return false;
        }
        let predicted_echo_rms = avg_reference_rms * coupling;
        if predicted_echo_rms <= 0.0 {
            return false;
        }
        let within_margin = avg_mic_rms <= predicted_echo_rms * (1.0 + COUPLING_EXPLAINED_MARGIN);
        let excess = (avg_mic_rms - predicted_echo_rms).max(0.0);
        let no_near_end_excess = excess <= near_end_level_rms * COUPLING_EXCESS_NEAR_END_FRACTION;
        within_margin && no_near_end_excess
    });
    well_below_near_end || high_suppression || explained_by_predicted_echo
}

/// Text+timing rule for tiny mic segments (see module docs, "problem 1a"
/// follow-up): a mic segment of `word_count` words at or below
/// `TINY_SEGMENT_MAX_WORDS`, fully inside a far-end speech span (every
/// overlapping block had far-end active, not just a majority), with no sign
/// of near-end evidence (average mic level does not clearly look like the
/// user's own normal speech), is dropped regardless of how much AEC
/// suppressed there. This exists for exactly the shortest hallucinated
/// fragments (e.g. a single spurious "and") that a majority-based region
/// check can be too coarse to catch on its own, while the near-end-level
/// guard keeps it from ever gating a genuine brief interjection (real double
/// talk that happens to be at normal speaking volume still passes).
pub fn is_tiny_far_end_fragment(
    blocks: &[BlockEnergy],
    region_start: usize,
    region_end: usize,
    near_end_level_rms: f32,
    word_count: usize,
) -> bool {
    if word_count == 0 || word_count > TINY_SEGMENT_MAX_WORDS {
        return false;
    }
    if blocks.is_empty() || region_end <= region_start {
        return false;
    }
    let overlapping = overlapping_blocks(blocks, region_start, region_end);
    if overlapping.is_empty() {
        return false;
    }
    let fully_far_end = overlapping.iter().all(|b| b.far_end_active);
    if !fully_far_end {
        return false;
    }
    let avg_mic_rms = mean(overlapping.iter().map(|b| b.mic_rms));
    avg_mic_rms <= near_end_level_rms * TINY_SEGMENT_NEAR_END_FRACTION
}

/// A narrow, text/timing-only rule for one specific artifact the signal-based
/// checks above cannot see: the "alignment dead zone" at the very start of a
/// recording, before the delay-compensated far-end reference has any samples
/// to compare the mic against at all (see `echo_cancel::cancel_echo`'s delay
/// alignment). Whatever real far-end echo leaked into the mic during that
/// window has no matching reference sample to predict it from -- the true
/// preceding far-end audio, if any, was simply never captured -- so neither
/// `is_residual_echo_region`'s suppression/coupling checks nor
/// `is_tiny_far_end_fragment`'s "fully inside far-end-active blocks" check
/// can ever fire there (the dead zone is far-end-*inactive* by construction).
/// Whisper occasionally mishears the adaptive filter's transient response in
/// that window as a short spurious word (e.g. a lone "and"). Deliberately
/// narrow to keep this safe: only a segment starting at the very first
/// sample of the track, only `TINY_SEGMENT_MAX_WORDS`-or-fewer words, only
/// fully inside a short (`LEADING_DEAD_ZONE_MAX_SAMPLES`-bounded) leading
/// run before the first far-end-active block anywhere in the trace, and only
/// when the recording actually goes on to have far-end activity (a track
/// with no far-end activity at all is never touched). A genuine near-end
/// utterance that happens to open a recording within that same half-second
/// window is the one real risk this rule accepts, in exchange for not
/// hallucinating a word from acoustic startup transients every such
/// recording would otherwise carry.
pub fn is_leading_alignment_dead_zone_fragment(
    blocks: &[BlockEnergy],
    region_start: usize,
    region_end: usize,
    word_count: usize,
) -> bool {
    if word_count == 0 || word_count > TINY_SEGMENT_MAX_WORDS {
        return false;
    }
    let Some(first_block) = blocks.first() else {
        return false;
    };
    if region_start != first_block.start_sample {
        return false;
    }
    let Some(first_active) = blocks.iter().find(|b| b.far_end_active) else {
        return false;
    };
    if first_active
        .start_sample
        .saturating_sub(first_block.start_sample)
        > LEADING_DEAD_ZONE_MAX_SAMPLES
    {
        return false;
    }
    region_end <= first_active.start_sample
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

    fn block(
        start: usize,
        len: usize,
        far_end_active: bool,
        mic_rms: f32,
        residual_rms: f32,
    ) -> BlockEnergy {
        block_full(
            start,
            len,
            far_end_active,
            mic_rms,
            residual_rms,
            0.0,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn block_full(
        start: usize,
        len: usize,
        far_end_active: bool,
        mic_rms: f32,
        residual_rms: f32,
        reference_rms: f32,
        far_end_only: bool,
    ) -> BlockEnergy {
        BlockEnergy {
            start_sample: start,
            len_samples: len,
            far_end_active,
            mic_rms,
            residual_rms,
            reference_rms,
            far_end_only,
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
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02, None));
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
        assert!(is_residual_echo_region(&blocks, 0, 320, 0.02, None));
    }

    #[test]
    fn double_talk_with_residual_near_the_near_end_level_is_kept() {
        // Far end active, but the residual left behind is comparable to the
        // user's own normal speech level: real double talk, must survive.
        let blocks = vec![
            block(0, 160, true, 0.22, 0.018),
            block(160, 160, true, 0.21, 0.019),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02, None));
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
        assert!(!is_residual_echo_region(&blocks, 0, 480, 0.02, None));
    }

    #[test]
    fn empty_trace_is_never_gated() {
        // No AEC trace at all (e.g. AEC was not applied to this track):
        // nothing to gate on, so always keep.
        assert!(!is_residual_echo_region(&[], 0, 320, 0.02, None));
    }

    #[test]
    fn region_outside_all_blocks_is_kept() {
        let blocks = vec![block(0, 160, true, 0.2, 0.0005)];
        assert!(!is_residual_echo_region(
            &blocks, 10_000, 10_320, 0.02, None
        ));
    }

    #[test]
    fn double_talk_is_kept_even_with_a_coupling_estimate_present() {
        // Same double-talk scenario as above, but now also carrying a
        // coupling estimate -- must still pass: the near-end residual/level
        // checks are unaffected by whether a coupling estimate exists.
        let blocks = vec![
            block(0, 160, true, 0.22, 0.018),
            block(160, 160, true, 0.21, 0.019),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02, Some(1.0)));
    }

    #[test]
    fn near_end_only_speech_is_kept_with_coupling_present() {
        let blocks = vec![
            block(0, 160, false, 0.03, 0.03),
            block(160, 160, false, 0.03, 0.03),
        ];
        assert!(!is_residual_echo_region(&blocks, 0, 320, 0.02, Some(1.0)));
    }

    #[test]
    fn unconverged_echo_region_is_gated_by_predicted_echo_even_at_low_suppression() {
        // Only ~6.5 dB of suppression here (well under HIGH_SUPPRESSION_DB),
        // mirroring the real d3863b42 unconverged region from the task -- but
        // the raw mic energy matches exactly what this recording's own
        // measured coupling (0.9 mic-per-reference, from the far_end_only
        // blocks below) predicts for this reference level, with no excess:
        // this must be recognized as residual echo despite the AEC pass
        // barely having suppressed it yet.
        let mut blocks = Vec::new();
        // Converged, far-end-only stretch establishing coupling ~= 0.9.
        for _ in 0..12 {
            blocks.push(block_full(0, 160, true, 0.09, 0.01, 0.1, true));
        }
        // Unconverged region: reference RMS 0.2 -> predicted echo ~0.18,
        // raw mic RMS is 0.19 (matches prediction), suppression is weak
        // (residual 0.09, only ~6.5 dB) -- old fixed-threshold logic alone
        // would keep this; the coupling-based check must gate it.
        let region_start = blocks.len() * 160;
        blocks.push(block_full(region_start, 160, true, 0.19, 0.09, 0.2, false));
        blocks.push(block_full(
            region_start + 160,
            160,
            true,
            0.19,
            0.09,
            0.2,
            false,
        ));
        let near_end_level_rms = derive_near_end_level_rms(&blocks);
        let coupling = estimate_echo_coupling(&blocks).expect("enough far_end_only blocks");
        assert!(
            is_residual_echo_region(
                &blocks,
                region_start,
                region_start + 320,
                near_end_level_rms,
                Some(coupling)
            ),
            "expected the unconverged region to be gated via predicted echo (coupling {coupling})"
        );
    }

    #[test]
    fn genuine_near_end_energy_on_top_of_predicted_echo_is_not_gated() {
        // Same coupling setup as above, but this region's raw mic energy is
        // well above what the predicted echo alone explains -- real
        // near-end speech mixed with unconverged echo (double talk) -- so it
        // must NOT be gated even though a coupling estimate exists.
        let mut blocks = Vec::new();
        for _ in 0..12 {
            blocks.push(block_full(0, 160, true, 0.09, 0.01, 0.1, true));
        }
        let region_start = blocks.len() * 160;
        // Predicted echo for reference 0.2 is ~0.18; actual mic is 0.5,
        // far beyond the coupling-explained margin and near-end excess floor.
        blocks.push(block_full(region_start, 160, true, 0.5, 0.3, 0.2, false));
        blocks.push(block_full(
            region_start + 160,
            160,
            true,
            0.5,
            0.3,
            0.2,
            false,
        ));
        let near_end_level_rms = derive_near_end_level_rms(&blocks);
        let coupling = estimate_echo_coupling(&blocks);
        assert!(!is_residual_echo_region(
            &blocks,
            region_start,
            region_start + 320,
            near_end_level_rms,
            coupling
        ));
    }

    #[test]
    fn tiny_word_fragment_fully_inside_far_end_speech_is_gated() {
        // A one-word mic segment fully overlapping active far-end blocks,
        // with mic energy at/below the near-end level -- exactly the "You:
        // and" hallucination shape from the task -- must be dropped.
        let blocks = vec![
            block(0, 160, true, 0.015, 0.01),
            block(160, 160, true, 0.014, 0.009),
        ];
        assert!(is_tiny_far_end_fragment(&blocks, 0, 320, 0.02, 1));
    }

    #[test]
    fn tiny_word_rule_does_not_fire_past_the_word_limit() {
        let blocks = vec![
            block(0, 160, true, 0.015, 0.01),
            block(160, 160, true, 0.014, 0.009),
        ];
        assert!(!is_tiny_far_end_fragment(&blocks, 0, 320, 0.02, 3));
    }

    #[test]
    fn tiny_word_rule_does_not_fire_when_far_end_silent_for_part_of_the_region() {
        // Mic-only segment (far end silent for part of it): must never be
        // gated by the tiny-word rule, regardless of word count.
        let blocks = vec![
            block(0, 160, false, 0.03, 0.03),
            block(160, 160, true, 0.03, 0.03),
        ];
        assert!(!is_tiny_far_end_fragment(&blocks, 0, 320, 0.02, 1));
    }

    #[test]
    fn tiny_word_rule_does_not_fire_on_genuine_loud_double_talk() {
        // Short but at normal near-end speaking volume: real double talk
        // (e.g. a brief "Well" or "Stop"), must survive despite being fully
        // inside a far-end-active span and only one word.
        let blocks = vec![
            block(0, 160, true, 0.05, 0.045),
            block(160, 160, true, 0.05, 0.045),
        ];
        assert!(!is_tiny_far_end_fragment(&blocks, 0, 320, 0.02, 1));
    }

    #[test]
    fn coupling_estimate_needs_enough_far_end_only_blocks() {
        let blocks = vec![block_full(0, 160, true, 0.09, 0.01, 0.1, true)];
        assert!(estimate_echo_coupling(&blocks).is_none());
    }

    #[test]
    fn leading_dead_zone_fragment_is_gated_when_short_and_at_the_very_start() {
        // Mirrors the real d3863b42 shape: a short leading run of
        // far-end-inactive blocks (alignment dead zone) immediately followed
        // by real far-end activity.
        let blocks = vec![
            block(0, 320, false, 0.0, 0.0),
            block(320, 320, false, 0.13, 0.13),
            block(640, 320, false, 0.10, 0.10),
            block(960, 320, true, 0.05, 0.02),
        ];
        assert!(is_leading_alignment_dead_zone_fragment(&blocks, 0, 960, 1));
    }

    #[test]
    fn leading_dead_zone_rule_does_not_fire_mid_recording() {
        let blocks = vec![
            block(0, 320, false, 0.0, 0.0),
            block(320, 320, true, 0.05, 0.02),
            block(640, 320, false, 0.13, 0.13),
            block(960, 320, false, 0.10, 0.10),
        ];
        // Region does not start at sample 0 of the track: never the leading
        // dead zone, regardless of anything else.
        assert!(!is_leading_alignment_dead_zone_fragment(
            &blocks, 640, 1280, 1
        ));
    }

    #[test]
    fn leading_dead_zone_rule_requires_a_short_gap() {
        // A long leading far-end-inactive stretch (tens of seconds, scaled
        // down here) looks like a genuine near-end-only opening, not an
        // alignment artifact -- must never fire.
        let blocks = vec![
            block(0, LEADING_DEAD_ZONE_MAX_SAMPLES + 1_000, false, 0.1, 0.1),
            block(LEADING_DEAD_ZONE_MAX_SAMPLES + 1_000, 320, true, 0.05, 0.02),
        ];
        assert!(!is_leading_alignment_dead_zone_fragment(
            &blocks,
            0,
            LEADING_DEAD_ZONE_MAX_SAMPLES + 1_000,
            1
        ));
    }

    #[test]
    fn leading_dead_zone_rule_requires_far_end_activity_somewhere() {
        let blocks = vec![
            block(0, 320, false, 0.1, 0.1),
            block(320, 320, false, 0.1, 0.1),
        ];
        assert!(!is_leading_alignment_dead_zone_fragment(&blocks, 0, 320, 1));
    }

    #[test]
    fn leading_dead_zone_rule_respects_the_word_limit() {
        let blocks = vec![
            block(0, 320, false, 0.13, 0.13),
            block(320, 320, true, 0.05, 0.02),
        ];
        assert!(!is_leading_alignment_dead_zone_fragment(&blocks, 0, 320, 3));
    }

    #[test]
    fn coupling_estimate_ignores_double_talk_blocks() {
        let mut blocks = Vec::new();
        for _ in 0..12 {
            blocks.push(block_full(0, 160, true, 0.09, 0.01, 0.1, true));
        }
        // A double-talk block with a wildly different mic/reference ratio
        // must not skew the estimate.
        blocks.push(block_full(0, 160, true, 5.0, 4.0, 0.1, false));
        let coupling = estimate_echo_coupling(&blocks).expect("enough far_end_only blocks");
        assert!((coupling - 0.9).abs() < 1e-3, "got {coupling}");
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
        assert_eq!(
            derive_near_end_level_rms(&blocks),
            CONSERVATIVE_NEAR_END_RMS
        );
    }
}
