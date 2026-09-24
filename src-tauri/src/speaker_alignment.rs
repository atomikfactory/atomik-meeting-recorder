use crate::{diarization_types::DiarizationOutput, transcription_types::Transcript};
use std::collections::{BTreeMap, HashMap};
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
        let mut m: BTreeMap<u32, Vec<(u64, u64, bool)>> = BTreeMap::new();
        for x in d.turns.iter().filter(|x| x.source_id == s.source_id) {
            let (a, b) = (s.start_ms.max(x.start_ms), s.end_ms.min(x.end_ms));
            if b > a {
                m.entry(x.speaker).or_default().push((
                    a,
                    b,
                    x.confidence.is_some_and(|c| c >= 0.65),
                ))
            }
        }
        let good: Vec<_> = m
            .iter()
            .filter(|(_, v)| {
                let (c, g) = u(v);
                c * 100 >= n * 80 && g
            })
            .collect();
        if good.len() == 1
            && !m
                .iter()
                .filter(|(k, _)| *k != good[0].0)
                .any(|(_, v)| u(v).0 * 100 > n * 10)
        {
            raw[i] = Some((s.source_id.clone(), *good[0].0, s.start_ms))
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
fn u(v: &Vec<(u64, u64, bool)>) -> (u64, bool) {
    let mut x = v.clone();
    x.sort();
    let (mut a, mut b, mut n, mut g) = (0, 0, 0, true);
    for (l, r, q) in x {
        if a == b {
            a = l;
            b = r;
            g &= q
        } else if l <= b {
            b = b.max(r);
            g &= q
        } else {
            n += b - a;
            a = l;
            b = r;
            g &= q
        }
    }
    (n + b.saturating_sub(a), g)
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
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 50, 1), turn(0, 50, 1)])).unwrap()[0],
            None
        )
    }
    #[test]
    fn confidence_and_competition_unknown() {
        let mut x = turn(0, 100, 1);
        x.confidence = None;
        assert_eq!(align_speakers(&t(), &d(vec![x])).unwrap()[0], None);
        assert_eq!(
            align_speakers(&t(), &d(vec![turn(0, 100, 1), turn(0, 11, 2)])).unwrap()[0],
            None
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
}
