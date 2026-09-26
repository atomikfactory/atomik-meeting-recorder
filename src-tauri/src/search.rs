//! Search across every recording's transcript and summary. Backed by
//! `search_store.rs`'s SQLite FTS5 index. This module owns:
//!
//! - safe FTS5 `MATCH` query construction from arbitrary user text
//!   (`build_match_query`),
//! - turning a recording's transcript/summary into indexable rows
//!   (`gather_index_data`), and
//! - `SearchService`, which keeps the index caught up: a one-time startup
//!   reconcile pass plus a light periodic recheck (both off the UI thread),
//!   with `reindex`/`remove` for the immediate cases the app already knows
//!   about (rename, delete).
//!
//! The index is a derived cache. Every failure here degrades to "search
//! returns nothing useful yet" -- it never blocks recording, transcription,
//! diarization, or summarizing.

use crate::{
    diarization::read_completed_speaker_labels,
    recording::Recording,
    recording_store::RecordingStore,
    search_store::{IndexRow, MeetingHits, RecordingIndexState, SearchStore},
    summary::read_completed_summary,
    transcription::{
        checksum, output_path, presentation_label, read_checkpoint, recording_directory,
        validate_transcript,
    },
    transcription_store::TranscriptionStore,
    transcription_types::Transcript,
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
const MAX_QUERY_CHARS: usize = 200;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
const HITS_PER_MEETING: usize = 5;

#[derive(Clone, Serialize)]
pub struct SearchHit {
    pub kind: String,
    pub seq: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub speaker: Option<String>,
    /// Pre-rendered, HTML-safe snippet: plain text is escaped and matched
    /// spans are wrapped in `<mark>`. Safe to insert directly into the DOM.
    pub snippet_html: String,
}
#[derive(Clone, Serialize)]
pub struct SearchMeeting {
    pub recording_id: String,
    pub title: String,
    pub started_at: i64,
    pub hits: Vec<SearchHit>,
}
#[derive(Clone, Serialize)]
pub struct SearchResults {
    pub meetings: Vec<SearchMeeting>,
    pub took_ms: u64,
}

/// Extra known library roots (the currently configured recordings folder and
/// any it replaced), read fresh so a folder change takes effect on the next
/// reconcile pass without restarting the app.
pub type LibraryRootsProvider = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

pub struct SearchService {
    root: PathBuf,
    store: Arc<Mutex<Option<SearchStore>>>,
    library_roots: LibraryRootsProvider,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl SearchService {
    pub fn with_library_roots(root: PathBuf, library_roots: LibraryRootsProvider) -> Self {
        let store = Arc::new(Mutex::new(
            SearchStore::open_or_rebuild(&root.join("search.sqlite3")).ok(),
        ));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = {
            let (root, store, cancel, library_roots) =
                (root.clone(), store.clone(), cancel.clone(), library_roots.clone());
            thread::Builder::new()
                .name("search-reconcile".into())
                .spawn(move || {
                    // First pass runs immediately (still off the UI thread); later
                    // passes are cheap because only changed recordings do real work.
                    while !cancel.load(std::sync::atomic::Ordering::Acquire) {
                        reconcile(&root, &library_roots(), &store);
                        let start = Instant::now();
                        while start.elapsed() < RECONCILE_INTERVAL {
                            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                })
                .ok()
        };
        Self {
            root,
            store,
            library_roots,
            cancel,
            worker: Mutex::new(worker),
        }
    }

    /// Immediately refreshes one recording's index rows (e.g. right after a
    /// rename, so the title used in results updates without waiting for the
    /// next reconcile tick). Never fails the caller: indexing is best-effort.
    pub fn reindex(&self, recording_id: &str) {
        let Ok(store_guard) = self.store.lock() else {
            return;
        };
        let Some(store) = store_guard.as_ref() else {
            return;
        };
        if let Ok(Some(recording)) = RecordingStore::open(&self.root).and_then(|r| r.get_by_id(recording_id)) {
            let _ = index_one(&self.root, &(self.library_roots)(), store, &recording);
        } else {
            let _ = store.remove_recording(recording_id);
        }
    }

    /// Immediately drops one recording from the index (e.g. right after delete).
    pub fn remove(&self, recording_id: &str) {
        if let Ok(store_guard) = self.store.lock() {
            if let Some(store) = store_guard.as_ref() {
                let _ = store.remove_recording(recording_id);
            }
        }
    }

    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<SearchResults, String> {
        let started = Instant::now();
        let query = query.chars().take(MAX_QUERY_CHARS).collect::<String>();
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let store_guard = self.store.lock().map_err(|_| "search index lock")?;
        let Some(store) = store_guard.as_ref() else {
            return Ok(SearchResults {
                meetings: Vec::new(),
                took_ms: started.elapsed().as_millis() as u64,
            });
        };
        let Some(match_query) = build_match_query(&query) else {
            return Ok(SearchResults {
                meetings: Vec::new(),
                took_ms: started.elapsed().as_millis() as u64,
            });
        };
        let meetings = store
            .search(&match_query, limit, HITS_PER_MEETING)?
            .into_iter()
            .map(render_meeting)
            .collect();
        Ok(SearchResults {
            meetings,
            took_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub fn shutdown(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
        }
    }
}

fn render_meeting(meeting: MeetingHits) -> SearchMeeting {
    SearchMeeting {
        recording_id: meeting.recording_id,
        title: meeting.title,
        started_at: meeting.started_at,
        hits: meeting
            .hits
            .into_iter()
            .map(|hit| SearchHit {
                kind: hit.kind,
                seq: hit.seq,
                start_ms: hit.start_ms,
                end_ms: hit.end_ms,
                speaker: hit.speaker,
                snippet_html: render_snippet(&hit.snippet),
            })
            .collect(),
    }
}

/// Turns an FTS5 `snippet()` result (marked with `\u{1}`/`\u{2}` around each
/// matched span) into HTML: plain runs are entity-escaped, matched runs are
/// escaped the same way and wrapped in `<mark>`. The output is safe to set
/// as `innerHTML` -- the source text can never inject markup.
fn render_snippet(marked: &str) -> String {
    let mut html = String::with_capacity(marked.len() + 16);
    let mut in_mark = false;
    let mut buf = String::new();
    let flush = |buf: &mut String, html: &mut String, in_mark: bool| {
        if buf.is_empty() {
            return;
        }
        let escaped = html_escape(buf);
        if in_mark {
            html.push_str("<mark>");
            html.push_str(&escaped);
            html.push_str("</mark>");
        } else {
            html.push_str(&escaped);
        }
        buf.clear();
    };
    for ch in marked.chars() {
        match ch {
            '\u{1}' => {
                flush(&mut buf, &mut html, in_mark);
                in_mark = true;
            }
            '\u{2}' => {
                flush(&mut buf, &mut html, in_mark);
                in_mark = false;
            }
            _ => buf.push(ch),
        }
    }
    flush(&mut buf, &mut html, in_mark);
    html
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Builds a safe FTS5 `MATCH` argument from arbitrary, untrusted user text.
/// Every token -- quoted phrase or bare word -- is emitted as a quoted FTS5
/// string literal (embedded `"` doubled per FTS5's escaping rule), so raw
/// operators the user types (`OR`, `-`, `*`, parentheses, an unbalanced `"`)
/// are always treated as literal text, never as query syntax. The final
/// token is turned into a prefix match (`"word"*`) for as-you-type search;
/// tokens are ANDed together. Returns `None` for empty/whitespace-only input.
pub(crate) fn build_match_query(input: &str) -> Option<String> {
    enum Term {
        Word(String),
        Phrase(String),
    }
    let mut terms = Vec::new();
    let mut word = String::new();
    let mut in_quotes = false;
    let mut phrase = String::new();
    for ch in input.chars() {
        if ch == '"' {
            if in_quotes {
                if !phrase.trim().is_empty() {
                    terms.push(Term::Phrase(phrase.clone()));
                }
                phrase.clear();
                in_quotes = false;
            } else {
                if !word.is_empty() {
                    terms.push(Term::Word(word.clone()));
                    word.clear();
                }
                in_quotes = true;
            }
        } else if in_quotes {
            phrase.push(ch);
        } else if ch.is_whitespace() {
            if !word.is_empty() {
                terms.push(Term::Word(word.clone()));
                word.clear();
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        terms.push(Term::Word(word));
    }
    // An unbalanced closing quote never arrived: treat whatever was captured
    // as a literal phrase rather than silently dropping it.
    if in_quotes && !phrase.trim().is_empty() {
        terms.push(Term::Phrase(phrase));
    }
    if terms.is_empty() {
        return None;
    }
    let n = terms.len();
    let escape = |s: &str| s.replace('"', "\"\"");
    let parts: Vec<String> = terms
        .into_iter()
        .enumerate()
        .map(|(i, term)| {
            let last = i + 1 == n;
            let text = match &term {
                Term::Word(w) => escape(w),
                Term::Phrase(p) => escape(p.trim()),
            };
            if last {
                format!("\"{text}\"*")
            } else {
                format!("\"{text}\"")
            }
        })
        .collect();
    Some(parts.join(" AND "))
}

/// Reconciles the index against durable state: drops rows for recordings
/// that no longer exist (deleted, or a corrupt store dropped their row), and
/// reindexes any recording whose transcript or summary checksum has changed
/// since it was last indexed (including "never indexed"). Cheap when nothing
/// changed: each recording costs one small-row lookup per store, not a
/// re-read of transcript/summary bytes.
fn reconcile(root: &Path, extra_roots: &[PathBuf], store: &Mutex<Option<SearchStore>>) {
    let Ok(mut guard) = store.lock() else { return };
    if guard.is_none() {
        *guard = SearchStore::open_or_rebuild(&root.join("search.sqlite3")).ok();
    }
    if let Some(store) = guard.as_ref() {
        reconcile_store(root, extra_roots, store);
    }
}

/// The reconcile pass shared by the background worker (`reconcile`, above,
/// which owns opening/rebuilding the store) and tests (`reconcile_for_test`,
/// which own a `SearchStore` directly): drop rows for recordings that no
/// longer exist, then (re)index anything whose transcript/summary checksum
/// changed since it was last indexed, including "never indexed".
fn reconcile_store(root: &Path, extra_roots: &[PathBuf], store: &SearchStore) {
    let Ok(recordings) = RecordingStore::open(root) else {
        return;
    };
    if let Ok(indexed_ids) = store.indexed_ids() {
        for id in indexed_ids {
            if recordings.get_by_id(&id).ok().flatten().is_none() {
                let _ = store.remove_recording(&id);
            }
        }
    }
    let Ok(eligible) = recordings.finalized_eligible() else {
        return;
    };
    for recording in eligible {
        let _ = index_one(root, extra_roots, store, &recording);
    }
}

/// Exposes one reconcile pass directly against a caller-owned `SearchStore`,
/// for integration tests that want to assert on index contents without
/// spinning up `SearchService`'s background thread and file-backed index.
#[cfg(test)]
pub(crate) fn reconcile_for_test(root: &Path, store: &SearchStore) {
    reconcile_store(root, &[], store);
}
/// As `reconcile_for_test`, but with extra known library roots -- for tests
/// that index a recording living under a custom (non-default) folder.
#[cfg(test)]
pub(crate) fn reconcile_for_test_with_roots(root: &Path, extra_roots: &[PathBuf], store: &SearchStore) {
    reconcile_store(root, extra_roots, store);
}

fn index_one(
    root: &Path,
    extra_roots: &[PathBuf],
    store: &SearchStore,
    recording: &Recording,
) -> Result<(), String> {
    let (title, started_at, state, rows) = gather_index_data(root, extra_roots, recording);
    let previous = store.state(&recording.recording_id)?;
    let unchanged = previous
        .map(|p| {
            p.transcript_checksum == state.transcript_checksum
                && p.summary_checksum == state.summary_checksum
        })
        .unwrap_or(false);
    if unchanged {
        return Ok(());
    }
    store.replace_recording(&recording.recording_id, &title, started_at, &state, &rows)
}

/// Builds every indexable row for one recording, plus the checksums that
/// let `index_one` skip recordings that have not actually changed. Never
/// errors: a transcript or summary that is missing, incomplete, or fails
/// validation simply contributes no rows for that part -- the recording
/// (at minimum its title) is still indexed.
fn gather_index_data(
    root: &Path,
    extra_roots: &[PathBuf],
    recording: &Recording,
) -> (String, i64, RecordingIndexState, Vec<IndexRow>) {
    let title = recording
        .meeting_title
        .clone()
        .unwrap_or_else(|| "Meeting".into());
    let started_at = recording.started_at as i64;
    let mut rows = vec![IndexRow {
        kind: "title",
        seq: 0,
        start_ms: None,
        end_ms: None,
        speaker: None,
        text: title.clone(),
    }];
    let mut state = RecordingIndexState::default();

    if let Some(transcript) = completed_transcript(root, extra_roots, recording) {
        state.transcript_checksum = Some(transcript.0.clone());
        let labels = read_completed_speaker_labels(root, extra_roots, recording, &transcript.1);
        let local_speaker_name = transcript.1.local_speaker_name.as_deref().unwrap_or("You");
        let names = crate::speaker_names::names_for(root, &recording.recording_id);
        for (i, segment) in transcript.1.segments.iter().enumerate() {
            let remote_speaker = labels.as_ref().and_then(|l| l.get(i).copied().flatten());
            let speaker = presentation_label(recording, segment, local_speaker_name, remote_speaker, Some(&names));
            rows.push(IndexRow {
                kind: "segment",
                seq: i as i64,
                start_ms: Some(segment.start_ms as i64),
                end_ms: Some(segment.end_ms as i64),
                speaker: Some(speaker),
                text: segment.text.clone(),
            });
        }
    }

    if let Some((hash, summary)) = read_completed_summary(root, extra_roots, &recording.recording_id) {
        state.summary_checksum = Some(hash);
        if !summary.summary.trim().is_empty() {
            rows.push(IndexRow {
                kind: "summary",
                seq: 0,
                start_ms: None,
                end_ms: None,
                speaker: None,
                text: summary.summary.clone(),
            });
        }
        for (i, point) in summary.key_points.iter().enumerate() {
            rows.push(IndexRow {
                kind: "key_point",
                seq: i as i64,
                start_ms: None,
                end_ms: None,
                speaker: None,
                text: point.clone(),
            });
        }
        for (i, decision) in summary.decisions.iter().enumerate() {
            rows.push(IndexRow {
                kind: "decision",
                seq: i as i64,
                start_ms: None,
                end_ms: None,
                speaker: None,
                text: decision.clone(),
            });
        }
        for (i, item) in summary.action_items.iter().enumerate() {
            let mut text = item.task.clone();
            if let Some(owner) = &item.owner {
                text.push_str(" — ");
                text.push_str(owner);
            }
            if let Some(due) = &item.due {
                text.push_str(" — ");
                text.push_str(due);
            }
            rows.push(IndexRow {
                kind: "action_item",
                seq: i as i64,
                start_ms: None,
                end_ms: None,
                speaker: None,
                text,
            });
        }
    }

    (title, started_at, state, rows)
}

/// A validated, completed transcript for this exact recording, plus its
/// checkpoint checksum. `None` for anything short of that (not transcribed
/// yet, checksum mismatch, corrupt checkpoint) -- the caller treats it the
/// same as "nothing to index yet".
fn completed_transcript(
    root: &Path,
    extra_roots: &[PathBuf],
    recording: &Recording,
) -> Option<(String, Transcript)> {
    let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).ok()?;
    let job = registry.get(&recording.recording_id).ok().flatten()?;
    if job.status != "completed" {
        return None;
    }
    let directory = recording_directory(root, extra_roots, &recording.recording_id).ok()?;
    let json = output_path(&directory, "transcript.json").ok()?;
    if job.transcript_path.as_deref() != Some(json.to_string_lossy().as_ref()) {
        return None;
    }
    let bytes = read_checkpoint(&json).ok()?;
    let hash = checksum(&bytes);
    if Some(hash.as_str()) != job.checksum.as_deref() {
        return None;
    }
    let transcript: Transcript = serde_json::from_slice(&bytes).ok()?;
    validate_transcript(&transcript, &job, recording).ok()?;
    Some((hash, transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_become_anded_prefix_terms() {
        assert_eq!(
            build_match_query("budget review").unwrap(),
            "\"budget\" AND \"review\"*"
        );
    }

    #[test]
    fn quoted_phrase_is_kept_as_one_term() {
        assert_eq!(
            build_match_query("\"quarterly budget\" review").unwrap(),
            "\"quarterly budget\" AND \"review\"*"
        );
    }

    #[test]
    fn adjacent_quoted_phrases_stay_separate_terms() {
        // Each `"..."` group closes its own phrase; back-to-back quotes open
        // a fresh phrase rather than being read as one escaped literal.
        let q = build_match_query("say \"hi\"\"there\"").unwrap();
        assert_eq!(q, "\"say\" AND \"hi\" AND \"there\"*");
    }

    #[test]
    fn adversarial_quote_runs_never_panic_or_produce_bare_syntax() {
        for input in ["\"\"\"\"\"", "\"\"", "\"a\"\"\"b\"", "\"\"\""] {
            if let Some(q) = build_match_query(input) {
                for part in q.split(" AND ") {
                    assert!(part.starts_with('"'), "part {part:?} from {input:?} is unquoted");
                }
            }
        }
    }

    #[test]
    fn operators_and_symbols_never_produce_bare_syntax() {
        for input in ["OR", "a OR b", "foo*bar", "(foo)", "foo-bar", "AND NOT x"] {
            let q = build_match_query(input).unwrap();
            // Every top-level term is wrapped in double quotes; none of the
            // original operator characters appear outside quotes.
            for part in q.split(" AND ") {
                assert!(part.starts_with('"'), "part {part:?} from {input:?} is unquoted");
            }
        }
    }

    #[test]
    fn unbalanced_quote_is_captured_to_end_not_dropped() {
        let q = build_match_query("meeting \"about the budget").unwrap();
        assert_eq!(q, "\"meeting\" AND \"about the budget\"*");
    }

    #[test]
    fn empty_or_whitespace_query_has_no_match() {
        assert!(build_match_query("").is_none());
        assert!(build_match_query("   ").is_none());
        assert!(build_match_query("\"\"").is_none());
    }

    #[test]
    fn snippet_rendering_escapes_html_and_wraps_marks() {
        let marked = format!("<script>{}alert{}</script>", '\u{1}', '\u{2}');
        let html = render_snippet(&marked);
        assert_eq!(html, "&lt;script&gt;<mark>alert</mark>&lt;/script&gt;");
    }

    #[test]
    fn snippet_rendering_handles_no_markers() {
        assert_eq!(render_snippet("plain text"), "plain text");
    }
}
