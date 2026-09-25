//! Full-text search index over transcripts and summaries, backed by SQLite
//! FTS5 in its own `search.sqlite3`. This database is a derived, rebuildable
//! cache: the transcript/summary/recording databases remain the source of
//! truth. If this file is missing, corrupt, or has a schema this build does
//! not recognize, it is dropped and rebuilt from scratch rather than
//! blocking any other feature -- see `SearchStore::open_or_rebuild`.
//!
//! Tokenizer: `unicode61 remove_diacritics 2`, so an accented query like
//! "décision" matches "decision" and vice versa. `prefix='2 3'` keeps
//! as-you-type prefix queries (2-3 character prefixes) fast without a
//! full table scan.

use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// One row queued for indexing: a transcript segment, a summary field, or a
/// meeting title. `seq` orders rows of the same `kind` within a recording
/// (segment index, or list index for key points/decisions/action items).
#[derive(Clone, Debug)]
pub struct IndexRow {
    pub kind: &'static str,
    pub seq: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub speaker: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct RecordingIndexState {
    pub transcript_checksum: Option<String>,
    pub summary_checksum: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub kind: String,
    pub seq: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub speaker: Option<String>,
    /// Snippet text with `\u{1}`/`\u{2}` marking the start/end of each
    /// matched span. Callers must HTML-escape the plain text between markers
    /// before rendering, then wrap marked spans in a highlight element --
    /// never render this string directly as HTML.
    pub snippet: String,
}

#[derive(Clone, Debug)]
pub struct MeetingHits {
    pub recording_id: String,
    pub title: String,
    pub started_at: i64,
    pub hits: Vec<SearchHit>,
}

const HIGHLIGHT_START: &str = "\u{1}";
const HIGHLIGHT_END: &str = "\u{2}";

/// Deletes every `search_fts` row belonging to `recording_id`, via the
/// `row_recording` side index rather than `search_fts`'s own UNINDEXED
/// `recording_id` column -- see the schema comment on `row_recording` in
/// `SearchStore::init` for why that distinction matters at scale.
fn delete_recording_rows(tx: &rusqlite::Transaction<'_>, recording_id: &str) -> Result<(), String> {
    tx.execute(
        "DELETE FROM search_fts WHERE rowid IN (SELECT fts_rowid FROM row_recording WHERE recording_id=?1)",
        params![recording_id],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM row_recording WHERE recording_id=?1",
        params![recording_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub struct SearchStore {
    c: Mutex<Connection>,
}

impl SearchStore {
    fn init(c: &Connection) -> Result<(), String> {
        c.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA synchronous=NORMAL;PRAGMA busy_timeout=5000;
             CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
               recording_id UNINDEXED,
               kind UNINDEXED,
               seq UNINDEXED,
               start_ms UNINDEXED,
               end_ms UNINDEXED,
               speaker UNINDEXED,
               text,
               tokenize='unicode61 remove_diacritics 2',
               prefix='2 3'
             );
             CREATE TABLE IF NOT EXISTS index_state(
               recording_id TEXT PRIMARY KEY NOT NULL,
               title TEXT NOT NULL,
               started_at INTEGER NOT NULL,
               transcript_checksum TEXT,
               summary_checksum TEXT,
               indexed_at INTEGER NOT NULL
             );
             -- `recording_id` on `search_fts` is UNINDEXED, so deleting a
             -- recording's rows by that column would be a full table scan
             -- (fine at small scale, quadratic across a whole-library
             -- reindex). This side table gives that lookup a real B-tree
             -- index, so deletes go through `search_fts`'s rowid instead --
             -- an ordinary primary-key delete against its content table.
             CREATE TABLE IF NOT EXISTS row_recording(
               fts_rowid INTEGER PRIMARY KEY,
               recording_id TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS row_recording_by_id ON row_recording(recording_id);",
        )
        .map_err(|e| e.to_string())
    }

    /// Opens the index, rebuilding it from scratch if it is missing, corrupt,
    /// or predates the current schema (an old build's leftover file, e.g.).
    /// A derived cache never has to be perfectly preserved: correctness of
    /// the underlying transcripts/summaries is unaffected either way.
    pub fn open_or_rebuild(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        match Self::open(path) {
            Ok(store) => Ok(store),
            Err(_) => {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let _ = fs::remove_file(format!("{}{suffix}", path.display()));
                }
                Self::open(path)
            }
        }
    }

    fn open(path: &Path) -> Result<Self, String> {
        if fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("search index must not be a symlink".into());
        }
        let c = Connection::open(path).map_err(|e| e.to_string())?;
        Self::init(&c)?;
        Ok(Self { c: Mutex::new(c) })
    }

    /// In-memory index, used by tests that do not need a file on disk.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let c = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::init(&c)?;
        Ok(Self { c: Mutex::new(c) })
    }

    pub fn state(&self, recording_id: &str) -> Result<Option<RecordingIndexState>, String> {
        self.c
            .lock()
            .map_err(|_| "search index lock")?
            .query_row(
                "SELECT transcript_checksum, summary_checksum FROM index_state WHERE recording_id=?1",
                params![recording_id],
                |r| {
                    Ok(RecordingIndexState {
                        transcript_checksum: r.get(0)?,
                        summary_checksum: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn indexed_ids(&self) -> Result<Vec<String>, String> {
        let c = self.c.lock().map_err(|_| "search index lock")?;
        let mut stmt = c
            .prepare("SELECT recording_id FROM index_state")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(ids)
    }

    /// Atomically replaces every indexed row for one recording. Safe to call
    /// repeatedly (e.g. from the startup reconcile): old rows are deleted
    /// first inside the same transaction.
    pub fn replace_recording(
        &self,
        recording_id: &str,
        title: &str,
        started_at: i64,
        state: &RecordingIndexState,
        rows: &[IndexRow],
    ) -> Result<(), String> {
        let mut c = self.c.lock().map_err(|_| "search index lock")?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        delete_recording_rows(&tx, recording_id)?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO search_fts(recording_id,kind,seq,start_ms,end_ms,speaker,text) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                )
                .map_err(|e| e.to_string())?;
            let mut track_rowid = tx
                .prepare("INSERT INTO row_recording(fts_rowid,recording_id) VALUES(?1,?2)")
                .map_err(|e| e.to_string())?;
            for row in rows {
                if row.text.trim().is_empty() {
                    continue;
                }
                insert
                    .execute(params![
                        recording_id,
                        row.kind,
                        row.seq,
                        row.start_ms,
                        row.end_ms,
                        row.speaker,
                        row.text
                    ])
                    .map_err(|e| e.to_string())?;
                track_rowid
                    .execute(params![tx.last_insert_rowid(), recording_id])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.execute(
            "INSERT INTO index_state(recording_id,title,started_at,transcript_checksum,summary_checksum,indexed_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(recording_id) DO UPDATE SET title=excluded.title, started_at=excluded.started_at,
               transcript_checksum=excluded.transcript_checksum, summary_checksum=excluded.summary_checksum,
               indexed_at=excluded.indexed_at",
            params![
                recording_id,
                title,
                started_at,
                state.transcript_checksum,
                state.summary_checksum,
                now_ms()
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn remove_recording(&self, recording_id: &str) -> Result<(), String> {
        let mut c = self.c.lock().map_err(|_| "search index lock")?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        delete_recording_rows(&tx, recording_id)?;
        tx.execute(
            "DELETE FROM index_state WHERE recording_id=?1",
            params![recording_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Runs `query` (already built into safe FTS5 MATCH syntax -- see
    /// `search::build_match_query`) and groups hits by meeting, ranked by
    /// each meeting's best (lowest/most relevant) bm25 score. `limit` bounds
    /// the number of meetings returned; each meeting keeps at most
    /// `hits_per_meeting` of its own best-ranked hits.
    ///
    /// Two-phase on purpose: a common word can match tens of thousands of
    /// rows, and `snippet()` is not cheap (it re-tokenizes and formats text
    /// per row). Phase 1 ranks candidates by `bm25()` alone, cheaply, and
    /// only the handful that will actually be shown ever have a snippet
    /// built for them in phase 2.
    pub fn search(
        &self,
        match_query: &str,
        limit: usize,
        hits_per_meeting: usize,
    ) -> Result<Vec<MeetingHits>, String> {
        let c = self.c.lock().map_err(|_| "search index lock")?;
        // Enough candidates to have a good chance of filling `limit` distinct
        // meetings even if the best-ranked rows cluster into few meetings,
        // capped so a very common term still costs a bounded amount of work.
        let candidate_pool = (limit.saturating_mul(hits_per_meeting).saturating_mul(8))
            .clamp(200, 5000) as i64;
        let mut candidates_stmt = c
            .prepare(
                "SELECT rowid, recording_id FROM search_fts WHERE search_fts MATCH ?1
                 ORDER BY bm25(search_fts) LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let candidates: Vec<(i64, String)> = candidates_stmt
            .query_map(params![match_query, candidate_pool], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(candidates_stmt);

        // Candidates arrived best-rank-first; keep each meeting's first
        // `hits_per_meeting` rowids and the order meetings first appeared in.
        let mut order: Vec<String> = Vec::new();
        let mut rowids_by_meeting: std::collections::HashMap<String, Vec<i64>> =
            std::collections::HashMap::new();
        for (rowid, recording_id) in candidates {
            let entry = rowids_by_meeting.entry(recording_id.clone()).or_insert_with(|| {
                order.push(recording_id);
                Vec::new()
            });
            if entry.len() < hits_per_meeting {
                entry.push(rowid);
            }
        }
        order.truncate(limit);
        let chosen_rowids: Vec<i64> = order
            .iter()
            .flat_map(|id| rowids_by_meeting[id].iter().copied())
            .collect();
        if chosen_rowids.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: only now pay for `snippet()`, and only for rows that will
        // actually be shown. Snippet column index 6 is `text` (0-based
        // across the 7 declared columns).
        let placeholders = chosen_rowids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT s.rowid, s.recording_id, st.title, st.started_at, s.kind, s.seq, s.start_ms, s.end_ms, s.speaker,
                    snippet(search_fts, 6, '{HIGHLIGHT_START}', '{HIGHLIGHT_END}', '…', 12) AS snip
             FROM search_fts s
             JOIN index_state st ON st.recording_id = s.recording_id
             WHERE s.rowid IN ({placeholders})"
        );
        let mut detail_stmt = c.prepare(&sql).map_err(|e| e.to_string())?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            chosen_rowids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let mut by_rowid: std::collections::HashMap<i64, (String, String, i64, SearchHit)> =
            std::collections::HashMap::new();
        let rows = detail_stmt
            .query_map(params_dyn.as_slice(), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, String>(9)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        for (rowid, recording_id, title, started_at, kind, seq, start_ms, end_ms, speaker, snip) in
            rows
        {
            by_rowid.insert(
                rowid,
                (
                    recording_id,
                    title,
                    started_at,
                    SearchHit {
                        kind,
                        seq,
                        start_ms,
                        end_ms,
                        speaker,
                        snippet: snip,
                    },
                ),
            );
        }

        let mut results = Vec::with_capacity(order.len());
        for recording_id in order {
            let Some(rowids) = rowids_by_meeting.get(&recording_id) else {
                continue;
            };
            let mut hits = Vec::with_capacity(rowids.len());
            let mut title = String::new();
            let mut started_at = 0i64;
            for rowid in rowids {
                if let Some((t, ti, sa, hit)) = by_rowid.remove(rowid) {
                    debug_assert_eq!(t, recording_id);
                    title = ti;
                    started_at = sa;
                    hits.push(hit);
                }
            }
            if !hits.is_empty() {
                results.push(MeetingHits {
                    recording_id,
                    title,
                    started_at,
                    hits,
                });
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &'static str, seq: i64, text: &str) -> IndexRow {
        IndexRow {
            kind,
            seq,
            start_ms: Some(seq * 1000),
            end_ms: Some(seq * 1000 + 500),
            speaker: Some("You".into()),
            text: text.into(),
        }
    }

    #[test]
    fn diacritics_are_folded_both_directions() {
        let s = SearchStore::open_in_memory().unwrap();
        s.replace_recording(
            "r1",
            "Weekly sync",
            1,
            &RecordingIndexState::default(),
            &[row("segment", 0, "We reached a d\u{e9}cision today")],
        )
        .unwrap();
        assert_eq!(s.search("\"decision\"", 10, 3).unwrap().len(), 1);
        s.replace_recording(
            "r2",
            "Weekly sync 2",
            2,
            &RecordingIndexState::default(),
            &[row("segment", 0, "We reached a decision today")],
        )
        .unwrap();
        assert_eq!(s.search("\"d\u{e9}cision\"", 10, 3).unwrap().len(), 2);
    }

    #[test]
    fn replace_recording_drops_stale_rows() {
        let s = SearchStore::open_in_memory().unwrap();
        s.replace_recording(
            "r1",
            "Meeting",
            1,
            &RecordingIndexState::default(),
            &[row("segment", 0, "alpha term")],
        )
        .unwrap();
        assert_eq!(s.search("\"alpha\"*", 10, 3).unwrap().len(), 1);
        s.replace_recording(
            "r1",
            "Meeting",
            1,
            &RecordingIndexState::default(),
            &[row("segment", 0, "beta term")],
        )
        .unwrap();
        assert!(s.search("\"alpha\"*", 10, 3).unwrap().is_empty());
        assert_eq!(s.search("\"beta\"*", 10, 3).unwrap().len(), 1);
    }

    #[test]
    fn remove_recording_clears_rows_and_state() {
        let s = SearchStore::open_in_memory().unwrap();
        s.replace_recording(
            "r1",
            "Meeting",
            1,
            &RecordingIndexState::default(),
            &[row("segment", 0, "alpha term")],
        )
        .unwrap();
        s.remove_recording("r1").unwrap();
        assert!(s.search("\"alpha\"*", 10, 3).unwrap().is_empty());
        assert!(s.state("r1").unwrap().is_none());
        assert!(s.indexed_ids().unwrap().is_empty());
    }

    #[test]
    fn ranking_prefers_more_relevant_meeting() {
        let s = SearchStore::open_in_memory().unwrap();
        s.replace_recording(
            "sparse",
            "Sparse",
            1,
            &RecordingIndexState::default(),
            &[row("segment", 0, "budget mentioned once in passing")],
        )
        .unwrap();
        s.replace_recording(
            "dense",
            "Dense",
            2,
            &RecordingIndexState::default(),
            &[
                row("segment", 0, "budget budget budget review"),
                row("segment", 1, "the budget is the main topic"),
            ],
        )
        .unwrap();
        let results = s.search("\"budget\"", 10, 3).unwrap();
        assert_eq!(results[0].recording_id, "dense");
    }

    #[test]
    fn hits_per_meeting_is_capped_and_meetings_ordered_by_best_rank() {
        let s = SearchStore::open_in_memory().unwrap();
        s.replace_recording(
            "r1",
            "Meeting one",
            1,
            &RecordingIndexState::default(),
            &[
                row("segment", 0, "alpha one"),
                row("segment", 1, "alpha two"),
                row("segment", 2, "alpha three"),
            ],
        )
        .unwrap();
        let results = s.search("\"alpha\"*", 10, 2).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hits.len(), 2);
    }
}
