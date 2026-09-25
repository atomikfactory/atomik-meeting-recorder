use crate::{diarization_types::DiarizationOutput, transcription_types::Transcript};
use std::collections::{BTreeMap, HashMap};

/// A Whisper segment is labelled with the speaker whose diarization turns
/// cover the largest share of it once that share reaches this fraction.
/// Chosen (per docs/diarization.md) to match Whisper's segment granularity --
/// a segment can straddle a turn boundary by a word or two even when it is
/// unambiguously one speaker's line -- rather than the much stricter 80%
/// "near-total coverage" this used to require, which left most segments on a
/// track with more than one detected speaker unlabeled (falling back to
/// "Remote") even when one speaker plainly dominated.
const MIN_COVERAGE_SHARE: u64 = 50;
/// A turn confidence at or below this is treated as "genuinely low" and
/// blocks labeling its speaker even if it otherwise has majority coverage.
/// sherpa-onnx's real per-turn confidences on multi-speaker audio run well
/// above this; only a real misassignment risk (as opposed to the sentinel
/// "confidence unavailable" case -- see `normalize_confidence` in
/// `diarization_engine.rs`, which is already reported as `None` and is
/// deliberately *not* treated as low here) scores this low.
const LOW_CONFIDENCE_REJECT: f32 = 0.3;

pub fn align_speakers(t: &Transcript, d: &DiarizationOutput) -> Result<Vec<Option<u32>>, String> {
    if t.recording_id != d.recording_id {
        return Err("Transcript and diarization recording IDs differ.".into());
    }
    if t.segments.iter().any(|s| s.end_ms < s.start_ms) {
        return Err("Invalid transcript timestamps.".into());
    }
    for x in &d.turns {
        if x.end_ms < x.start_ms
            || x.confidence
                .is_some_and(|c| !c.is_finite() || !(0.0..=1.0).contains(&c))
        {
            return Err("Invalid diarization turn.".into());
        }
    }
    let mut raw = vec![None; t.segments.len()];
    for (i, s) in t.segments.iter().enumerate() {
        let n = s.end_ms.saturating_sub(s.start_ms);
        if n == 0 {
            continue;
        }
        // Per-speaker overlap intervals with this segment, plus whether any
        // contributing turn has a confidence explicitly known to be low. A
        // turn with *no* confidence (the sherpa-onnx single-speaker-cluster
        // sentinel, already normalized to `None`) is treated as "uncertain
        // but not disqualifying" -- otherwise every segment on a
        // single-speaker track would stay unlabeled forever, since a lone
        // speaker never gets a real confidence score at all.
        let mut m: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
        let mut low_confidence: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for x in d.turns.iter().filter(|x| x.source_id == s.source_id) {
            let (a, b) = (s.start_ms.max(x.start_ms), s.end_ms.min(x.end_ms));
            if b > a {
                m.entry(x.speaker).or_default().push((a, b));
                if x.confidence.is_some_and(|c| c <= LOW_CONFIDENCE_REJECT) {
                    low_confidence.insert(x.speaker);
                }
            }
        }
        if m.is_empty() {
            continue;
        }
        let mut coverage: Vec<(u32, u64)> =
            m.iter().map(|(speaker, v)| (*speaker, merged_coverage_ms(v))).collect();
        coverage.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let (winner, winner_ms) = coverage[0];
        let runner_up_ms = coverage.get(1).map(|(_, ms)| *ms).unwrap_or(0);
        let has_majority = winner_ms * 100 >= n * MIN_COVERAGE_SHARE;
        let unambiguous = winner_ms > runner_up_ms;
        if has_majority && unambiguous && !low_confidence.contains(&winner) {
            raw[i] = Some((s.source_id.clone(), winner, s.start_ms))
        }
    }
    let mut keys: Vec<_> = raw.iter().flatten().cloned().collect();
    keys.sort_by_key(|x| (x.2, x.0.clone(), x.1));
    let mut map = HashMap::new();
    for (s, p, _) in keys {
        let next = map.len() as u32 + 1;
        map.entry((s, p)).or_insert(next);
    }
    Ok(raw
        .into_iter()
        .map(|x| x.map(|(s, p, _)| map[&(s, p)]))
        .collect())
}

/// Total milliseconds covered by `v` (a speaker's overlap intervals with one
/// transcript segment) after merging overlapping/adjacent ranges, so a
/// speaker whose turns were split into several short diarization turns is
/// not penalized relative to one long turn covering the same span.
fn merged_coverage_ms(v: &[(u64, u64)]) -> u64 {
    let mut x = v.to_vec();
    x.sort();
    let (mut a, mut b, mut total) = (0u64, 0u64, 0u64);
    for (l, r) in x {
        if a == b {
            a = l;
            b = r;
        } else if l <= b {
            b = b.max(r);
        } else {
            total += b - a;
            a = l;
            b = r;
        }
    }
    total + b.saturating_sub(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diarization_types::{DiarizationOutput, SpeakerTurn},
        transcription_types::{Transcript, TranscriptSegment},
    };
    fn t() -> Transcript {
        Transcript {
            recording_id: "r".into(),
            model_key: "m".into(),
            model_hash: "h".into(),
            warnings: vec![],
            languages_used: vec![],
            local_speaker_name: Some("You".into()),
            segments: vec![seg(0, 100, "a"), seg(100, 200, "a"), seg(200, 300, "a")],
        }
    }
    fn seg(a: u64, b: u64, src: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms: a,
            end_ms: b,
            text: "words".into(),
            source_id: src.into(),
            source_name: src.into(),
        }
    }
    fn turn(a: u64, b: u64, p: u32) -> SpeakerTurn {
        SpeakerTurn {
            source_id: "a".into(),
            start_ms: a,
            end_ms: b,
            speaker: p,
            confidence: Some(0.9),
        }
    }
    fn d(v: Vec<SpeakerTurn>) -> DiarizationOutput {
        DiarizationOutput {
            recording_id: "r".into(),
            turns: v,
            warnings: vec![],
        }
    }
    #[test]
    fn repeated_is_stable() {
        assert_eq!(
            align_speakers(
                &t(),
                &d(vec![turn(0, 100, 7), turn(100, 200, 8), turn(200, 300, 7)])
            )
            .unwrap(),
            vec![Some(1), Some(2), Some(1)]
        )
    }
    #[test]
    fn duplicate_union_not_inflated() {
        // Two identical, fully-overlapping 40ms turns for the same speaker
        // must merge to 40ms of coverage (40% of the 100ms segment), not
        // double-count to 80ms -- so this stays below the 50% labeling
        // threshold and the segment is left unlabeled.
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 40, 1), turn(0, 40, 1)])).unwrap()[0],
            None
        )
    }
    #[test]
    fn unknown_confidence_does_not_block_a_majority_label() {
        // The sherpa-onnx sentinel for "confidence unavailable" (e.g. only
        // one speaker cluster was ever found on this track) is already
        // normalized to `None` upstream; a turn with unknown confidence must
        // still be labeled when it otherwise has clear majority coverage --
        // otherwise every segment on a single-speaker track stays "Remote"
        // forever, which was the observed real-world bug.
        let mut x = turn(0, 100, 1);
        x.confidence = None;
        assert_eq!(align_speakers(&t(), &d(vec![x])).unwrap()[0], Some(1));
    }
    #[test]
    fn a_real_low_confidence_turn_still_blocks_labeling() {
        let mut x = turn(0, 100, 1);
        x.confidence = Some(0.1);
        assert_eq!(align_speakers(&t(), &d(vec![x])).unwrap()[0], None);
    }
    #[test]
    fn a_small_competing_turn_no_longer_blocks_the_clear_majority() {
        // Speaker 1 covers the whole 100ms segment; speaker 2 only overlaps
        // an 11ms sliver. The old rule rejected the label outright whenever
        // *any* other speaker exceeded 10% coverage; the fix instead picks
        // whichever speaker has the largest (here, overwhelming) share.
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 100, 1), turn(0, 11, 2)])).unwrap()[0],
            Some(1)
        )
    }
    #[test]
    fn a_near_even_split_between_two_speakers_stays_unlabeled() {
        // Neither speaker reaches a clear majority (each covers half), so
        // the segment is genuinely ambiguous and must fall back to unlabeled
        // ("Remote") rather than guessing.
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 50, 1), turn(50, 100, 2)])).unwrap()[0],
            None
        )
    }
    #[test]
    fn fifty_percent_coverage_is_enough_to_label() {
        // Exactly half the segment covered by one speaker, with no
        // competitor at all, meets the documented "~50%" bar.
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 50, 1)])).unwrap()[0],
            Some(1)
        )
    }
    #[test]
    fn malformed_nan_rejected() {
        let mut x = turn(0, 100, 1);
        x.confidence = Some(f32::NAN);
        assert!(align_speakers(&t(), &d(vec![x])).is_err())
    }
    #[test]
    fn extra_source_turns_are_ignored() {
        let mut extra = turn(0, 100, 99);
        extra.source_id = "unrepresented-track".into();
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 100, 1), extra])).unwrap()[0],
            Some(1)
        );
    }
    #[test]
    fn single_speaker_track_labels_every_segment_speaker_one() {
        // Reproduces the reported bug: a track where diarization only ever
        // found one speaker (so every turn's confidence is the sherpa-onnx
        // sentinel, normalized to `None`) used to leave every segment
        // unlabeled ("Remote" fallback) instead of consistently "Speaker 1".
        let mut turns = vec![turn(0, 100, 3), turn(100, 200, 3), turn(200, 300, 3)];
        for turn in &mut turns {
            turn.confidence = None;
        }
        assert_eq!(
            align_speakers(&t(), &d(turns)).unwrap(),
            vec![Some(1), Some(1), Some(1)]
        );
    }
}
