//! Text-level duplicate cleanup: a safety net run after transcription, on top
//! of (not instead of) the signal-level acoustic echo cancellation in
//! `echo_cancel/mod.rs`. AEC reduces, but does not always fully remove, a remote
//! participant's speech leaking from the loopback track into the microphone
//! track -- and a recording with `echo_cancellation` turned off, or where AEC
//! itself was skipped for a track (see `transcription_engine::
//! apply_echo_cancellation`), gets no signal-level help at all. Comparing the
//! transcribed text of every microphone segment against every loopback
//! segment and dropping a microphone segment that is clearly just an echo of
//! one is a coarse but robust last line of defense: it only ever removes a
//! microphone line, never a loopback line, and only when a loopback line
//! covering (about) the same time window says (about) the same words.
use crate::transcription_types::TranscriptSegment;

/// A mic segment counts as overlapping a loopback segment if their time
/// ranges are within this many milliseconds of each other -- generous enough
/// to absorb encoder/VAD chunk-boundary slop and the small residual delay
/// AEC's delay estimator leaves behind (see `echo_cancel::delay`), without
/// being so wide that it starts matching unrelated nearby lines.
const OVERLAP_WINDOW_MS: u64 = 1_500;
/// Normalized word (Jaccard) similarity at or above which a mic segment is
/// considered a clear echo duplicate of an overlapping loopback segment
/// rather than the user's own, independent speech. Deliberately well above
/// 0.5 so a double-talk line (the remote line's words plus the user's own)
/// is *kept*: mixing in even a modest amount of the user's own speech should
/// dilute the overlap below this threshold, since only an (almost) verbatim
/// match is safe to drop outright.
const DUPLICATE_SIMILARITY_THRESHOLD: f32 = 0.6;
/// A much lower similarity bar used only for a mic segment that (a) lies
/// inside an overlapping loopback segment's time span and (b) has weak
/// post-AEC energy consistent with residual echo rather than real near-end
/// speech (see `residual_echo_gate::overlaps_weak_far_end_residual`). This
/// catches the case the strict `DUPLICATE_SIMILARITY_THRESHOLD` above cannot:
/// Whisper *mishearing* residual echo as different words entirely (e.g. "I
/// can leave here and I mentioned the 21st Thursday" for a remote line about
/// "scrape the first information into the Google Drive"), where textual
/// overlap is real but partial rather than near-verbatim. Deliberately not
/// applied to every mic segment -- only ones already flagged as acoustically
/// weak-and-far-end-dominated -- so a genuinely independent, low-volume
/// near-end line is never at risk from this looser bar.
const PARTIAL_SIMILARITY_THRESHOLD: f32 = 0.3;
/// Slack allowed when checking that a mic segment's time span lies inside an
/// overlapping loopback segment's span (see `PARTIAL_SIMILARITY_THRESHOLD`),
/// matching the general timing slop `OVERLAP_WINDOW_MS` already accounts for
/// elsewhere in this module, just tighter: this is a containment check, not
/// just "somewhere nearby".
const CONTAINMENT_SLACK_MS: u64 = 300;

/// Removes microphone-track segments from `segments` that are clear echo
/// duplicates of an overlapping loopback-track segment, or (when
/// `weak_mic_energy[i]` flags `segments[i]` as acoustically weak, far-end-
/// dominated residual -- see `residual_echo_gate::overlaps_weak_far_end_
/// residual`) a much looser partial-overlap match (see
/// `PARTIAL_SIMILARITY_THRESHOLD`), catching a misheard-echo mic line a pure
/// text comparison alone would miss. Returns the filtered list (in the
/// original order) and how many were removed (recorded in transcript
/// processing notes by the caller). `directions[i]` is the track direction
/// ("input" or "output", matching `recording::Track::direction`) that
/// produced `segments[i]`; all three slices/vectors must be the same length.
/// Deterministic: iterates in a fixed order and never mutates surviving
/// segments, so it is stable to re-run and safe for checkpoint recovery.
pub fn dedupe_echo_segments_with_energy(
    segments: Vec<TranscriptSegment>,
    directions: &[&str],
    weak_mic_energy: &[bool],
) -> (Vec<TranscriptSegment>, usize) {
    debug_assert_eq!(segments.len(), directions.len());
    debug_assert_eq!(segments.len(), weak_mic_energy.len());
    let keep = echo_duplicate_keep_mask(&segments, directions, weak_mic_energy);
    let removed = keep.iter().filter(|k| !**k).count();
    let kept = segments
        .into_iter()
        .zip(keep)
        .filter_map(|(segment, keep)| keep.then_some(segment))
        .collect();
    (kept, removed)
}

/// The keep/drop decision for every segment, same length and order as
/// `segments`. Split out from `dedupe_echo_segments` so tests can assert on
/// exactly which indices were dropped without reconstructing them from the
/// filtered output.
fn echo_duplicate_keep_mask(
    segments: &[TranscriptSegment],
    directions: &[&str],
    weak_mic_energy: &[bool],
) -> Vec<bool> {
    (0..segments.len())
        .map(|i| {
            // Only a microphone (input) segment can be dropped as an echo
            // duplicate; a loopback (output) segment is never removed, and
            // any other direction (there is none today, but this is not the
            // place to assume that stays true) is left alone too.
            if directions[i] != "input" {
                return true;
            }
            let seg = &segments[i];
            let weak = weak_mic_energy.get(i).copied().unwrap_or(false);
            let is_duplicate = segments.iter().enumerate().any(|(j, other)| {
                if i == j || directions[j] != "output" || !overlaps_within_window(seg, other) {
                    return false;
                }
                let similarity = word_similarity(&seg.text, &other.text);
                if similarity >= DUPLICATE_SIMILARITY_THRESHOLD {
                    return true;
                }
                weak && contained_within(seg, other) && similarity >= PARTIAL_SIMILARITY_THRESHOLD
            });
            !is_duplicate
        })
        .collect()
}

/// True when `inner`'s time span lies inside `outer`'s, within
/// `CONTAINMENT_SLACK_MS` slack on either edge.
fn contained_within(inner: &TranscriptSegment, outer: &TranscriptSegment) -> bool {
    inner.start_ms.saturating_add(CONTAINMENT_SLACK_MS) >= outer.start_ms
        && inner.end_ms <= outer.end_ms.saturating_add(CONTAINMENT_SLACK_MS)
}

fn overlaps_within_window(a: &TranscriptSegment, b: &TranscriptSegment) -> bool {
    a.start_ms <= b.end_ms.saturating_add(OVERLAP_WINDOW_MS)
        && b.start_ms <= a.end_ms.saturating_add(OVERLAP_WINDOW_MS)
}

/// Normalized word-level (Jaccard) similarity of `a` and `b`: lowercase,
/// diacritic-folded, punctuation-stripped tokens, compared as sets (word
/// order is deliberately ignored -- this is meant to catch a near-verbatim
/// echo, not to be a general text-similarity metric). `0.0` if either side
/// has no words after normalization.
fn word_similarity(a: &str, b: &str) -> f32 {
    let words_a = normalized_words(a);
    let words_b = normalized_words(b);
    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }
    let set_a: std::collections::HashSet<&str> = words_a.iter().map(String::as_str).collect();
    let set_b: std::collections::HashSet<&str> = words_b.iter().map(String::as_str).collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count().max(1);
    intersection as f32 / union as f32
}

fn normalized_words(text: &str) -> Vec<String> {
    text.chars()
        .map(fold_diacritic)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Folds a common accented Latin letter to its unaccented ASCII base and
/// lowercases it, so e.g. French "café" and "Cafe" compare as the same word.
/// Covers the Latin-1 Supplement / common Latin Extended-A letters used by
/// French, Spanish, Portuguese, Italian, and German; anything outside that
/// (already-ASCII, or another script entirely) passes through
/// `to_ascii_lowercase`/unchanged respectively.
fn fold_diacritic(c: char) -> char {
    let lower = c.to_lowercase().next().unwrap_or(c);
    match lower {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'ç' | 'ć' | 'č' => 'c',
        'ñ' | 'ń' => 'n',
        'ß' => 's',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only convenience matching this module's pre-energy-flag API: no
    /// segment is acoustically flagged weak, so only the strict, near-verbatim
    /// match applies -- exactly the original `dedupe_echo_segments` behavior.
    fn dedupe_echo_segments(
        segments: Vec<TranscriptSegment>,
        directions: &[&str],
    ) -> (Vec<TranscriptSegment>, usize) {
        let weak_mic_energy = vec![false; segments.len()];
        dedupe_echo_segments_with_energy(segments, directions, &weak_mic_energy)
    }

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms,
            text: text.into(),
            source_id: "source".into(),
            source_name: "Source".into(),
        }
    }

    #[test]
    fn weak_energy_mic_line_contained_in_a_remote_line_is_removed_on_partial_overlap() {
        // Reproduces the reported residual-echo hallucination: the mic line
        // is textually quite different from the overlapping remote line
        // (word (Jaccard) similarity here is 2/6 = 0.333: above the loose
        // partial-overlap bar but well below the strict near-verbatim one),
        // but it lies fully inside the remote line's time span and is
        // flagged as acoustically weak/far-end residual, so the looser bar
        // applies.
        let segments = vec![
            segment(3_000, 11_000, "one two three four"),
            segment(5_000, 9_000, "one two five six"),
        ];
        let (kept, removed) =
            dedupe_echo_segments_with_energy(segments, &["output", "input"], &[false, true]);
        assert_eq!(
            removed, 1,
            "weak, contained, partially-overlapping mic line should be dropped"
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source_id, "source");
    }

    #[test]
    fn a_mic_line_not_flagged_weak_is_not_dropped_by_the_looser_bar() {
        // Same partial overlap as above, but the acoustic energy flag says
        // this is not weak/far-end residual -- so the strict bar still
        // applies, and the line survives.
        let segments = vec![
            segment(3_000, 11_000, "one two three four"),
            segment(5_000, 9_000, "one two five six"),
        ];
        let (kept, removed) =
            dedupe_echo_segments_with_energy(segments, &["output", "input"], &[false, false]);
        assert_eq!(removed, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_weak_mic_line_not_contained_in_the_remote_span_is_not_dropped() {
        // Weak energy alone is not enough: the mic segment must also lie
        // inside the overlapping remote segment's time span.
        let segments = vec![
            segment(0, 2_000, "one two three four"),
            segment(1_000, 3_000, "one two five six"),
        ];
        let (kept, removed) =
            dedupe_echo_segments_with_energy(segments, &["output", "input"], &[false, true]);
        assert_eq!(removed, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn exact_duplicate_mic_line_is_removed() {
        let segments = vec![
            segment(0, 3000, "completing the code merge and migration"),
            segment(200, 3200, "completing the code merge and migration"),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source_id, "source"); // the surviving (output) line
    }

    #[test]
    fn near_duplicate_with_minor_wording_diff_is_removed() {
        let segments = vec![
            segment(0, 3000, "with those key brands in the list there"),
            segment(
                50,
                6800,
                "with those key brands in the list there, completing",
            ),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 1, "near-duplicate wording should still be dropped");
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn partial_overlap_double_talk_line_is_kept() {
        // Mic line mixes the remote line's words with a chunk of the user's
        // own speech: overlap with the loopback line alone should not clear
        // the similarity threshold.
        let segments = vec![
            segment(0, 2000, "stuff boring"),
            segment(
                0,
                6000,
                "stuff boring wait I think we should also review the budget numbers before the call ends",
            ),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 0, "double talk must never be dropped");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn user_only_line_with_no_similar_loopback_is_kept() {
        let segments = vec![
            segment(0, 2000, "let's start the meeting"),
            segment(100, 2200, "can everyone hear me okay"),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn time_far_similar_line_is_kept() {
        // Same words, but 10 seconds apart: far outside the overlap window,
        // so this is coincidence (or a genuinely repeated phrase), not echo.
        let segments = vec![
            segment(0, 2000, "let's take a five minute break"),
            segment(12_000, 14_000, "let's take a five minute break"),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn french_accented_duplicate_is_removed_diacritic_insensitively() {
        let segments = vec![
            segment(0, 2000, "Le café est prêt à être servi"),
            segment(100, 2100, "le cafe est pret a etre servi"),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "input"]);
        assert_eq!(removed, 1);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_loopback_segment_is_never_removed_even_if_it_duplicates_another_loopback_segment() {
        let segments = vec![
            segment(0, 2000, "hello everyone"),
            segment(100, 2100, "hello everyone"),
        ];
        let (kept, removed) = dedupe_echo_segments(segments, &["output", "output"]);
        assert_eq!(removed, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn word_similarity_is_symmetric_and_diacritic_insensitive() {
        assert!((word_similarity("hello world", "hello world") - 1.0).abs() < 1e-6);
        assert_eq!(word_similarity("", "anything"), 0.0);
        assert!(word_similarity("café", "cafe") > 0.99);
    }
}
