//! Durable local journal for atomik-kb export, mirroring `appflowy_store.rs`
//! (minus its legacy-generation machinery, which this integration has never
//! needed). Two tables: `atomik_kb_pages` is the create-only idempotency
//! ledger -- one row per page this integration has ever planned or created
//! (root "My daily meetings", one per date, one per meeting export), keyed by
//! a deterministic local key so a retry after a crash can tell "did I already
//! create this" without re-deriving anything from the knowledge base.
//! `atomik_kb_jobs` is the export work queue (a single SQLite table is the
//! source of truth for status/attempts, so a crash mid-export always resumes
//! from a consistent row).
use rusqlite::{params, Connection, OptionalExtension};
use std::{fs, path::Path, sync::Mutex};

/// `root:{space_id}` for "My daily meetings", `date:{root_page_id}:{date}`
/// for a date page, and `meeting:{recording_id}:{export_index}` for a meeting
/// page. Deterministic so re-planning after a crash reuses the same key
/// rather than minting a new one and losing track of a page that may exist.
pub fn root_key(space_id: &str) -> String {
    format!("root:{space_id}")
}
pub fn date_key(root_page_id: &str, date: &str) -> String {
    format!("date:{root_page_id}:{date}")
}
pub fn meeting_key(recording_id: &str, export_index: u32) -> String {
    format!("meeting:{recording_id}:{export_index}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageRecord {
    pub key: String,
    pub kind: String,
    pub parent_key: Option<String>,
    pub space_id: String,
    pub name: String,
    /// atomik-kb assigns page ids itself, so there is nothing to plan up
    /// front; `None` until the page is created (or adopted by exact title).
    pub remote_page_id: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AtomikKbJob {
    pub recording_id: String,
    pub status: String,
    pub error: Option<String>,
    pub export_index: u32,
    #[cfg(test)]
    pub attempts: u32,
}

pub struct AtomikKbStore {
    c: Mutex<Connection>,
}
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
/// Same shape as `appflowy_store::backoff_seconds`: 5s, 10s, 20s, ... capped
/// at 5 minutes.
fn backoff_seconds(attempts: u32) -> i64 {
    (5i64 * 2i64.pow(attempts.min(6))).min(300)
}

impl AtomikKbStore {
    pub fn open(p: &Path) -> Result<Self, String> {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("atomik_kb database must not be a symlink".into());
        }
        let c = Connection::open(p).map_err(|e| e.to_string())?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;\
             CREATE TABLE IF NOT EXISTS atomik_kb_pages(\
                key TEXT PRIMARY KEY,kind TEXT NOT NULL,parent_key TEXT,space_id TEXT NOT NULL,\
                name TEXT NOT NULL,remote_page_id TEXT,status TEXT NOT NULL);\
             CREATE TABLE IF NOT EXISTS atomik_kb_jobs(\
                recording_id TEXT PRIMARY KEY,status TEXT NOT NULL,error TEXT,\
                export_index INTEGER NOT NULL DEFAULT 1,attempts INTEGER NOT NULL DEFAULT 0,retry_after INTEGER);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { c: Mutex::new(c) })
    }

    fn row_to_page(row: &rusqlite::Row) -> rusqlite::Result<PageRecord> {
        Ok(PageRecord {
            key: row.get(0)?,
            kind: row.get(1)?,
            parent_key: row.get(2)?,
            space_id: row.get(3)?,
            name: row.get(4)?,
            remote_page_id: row.get(5)?,
            status: row.get(6)?,
        })
    }
    /// Every page this integration has ever planned or created, for the
    /// read-only "Exported pages" list in Settings.
    pub fn all_pages(&self) -> Result<Vec<PageRecord>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut stmt = c
            .prepare("SELECT key,kind,parent_key,space_id,name,remote_page_id,status FROM atomik_kb_pages ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], Self::row_to_page)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }
    pub fn get_page(&self, key: &str) -> Result<Option<PageRecord>, String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(
                "SELECT key,kind,parent_key,space_id,name,remote_page_id,status FROM atomik_kb_pages WHERE key=?1",
                params![key],
                Self::row_to_page,
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    /// Returns the existing row for `key` if one is already planned/created;
    /// otherwise inserts a new `status='creating'` row and returns that.
    /// Never overwrites an existing row -- idempotency without ever updating
    /// what a previous run already recorded.
    pub fn find_or_plan(
        &self,
        key: &str,
        kind: &str,
        parent_key: Option<&str>,
        space_id: &str,
        name: &str,
    ) -> Result<PageRecord, String> {
        if let Some(existing) = self.get_page(key)? {
            return Ok(existing);
        }
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "INSERT INTO atomik_kb_pages(key,kind,parent_key,space_id,name,remote_page_id,status) VALUES(?1,?2,?3,?4,?5,NULL,'creating')",
                params![key, kind, parent_key, space_id, name],
            )
            .map_err(|e| e.to_string())?;
        self.get_page(key)?
            .ok_or_else(|| "page record vanished after insert".into())
    }
    /// True if some journal row already owns `remote_page_id`. Used so a
    /// name match is never adopted by a second row: two meetings with the
    /// same title in the same minute must get two pages, not share one.
    pub fn is_page_recorded(&self, remote_page_id: &str) -> Result<bool, String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(
                "SELECT 1 FROM atomik_kb_pages WHERE remote_page_id=?1 LIMIT 1",
                params![remote_page_id],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
            .map_err(|e| e.to_string())
    }
    /// Marks a page created (or adopted by exact title on retry).
    /// Idempotent: calling it twice with the same `remote_page_id` is fine.
    pub fn mark_created(&self, key: &str, remote_page_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_pages SET remote_page_id=?2,status='created' WHERE key=?1",
                params![key, remote_page_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn row_to_job(row: &rusqlite::Row) -> rusqlite::Result<AtomikKbJob> {
        Ok(AtomikKbJob {
            recording_id: row.get(0)?,
            status: row.get(1)?,
            error: row.get(2)?,
            export_index: row.get(3)?,
            #[cfg(test)]
            attempts: row.get(4)?,
        })
    }
    pub fn get_job(&self, recording_id: &str) -> Result<Option<AtomikKbJob>, String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(
                "SELECT recording_id,status,error,export_index,attempts FROM atomik_kb_jobs WHERE recording_id=?1",
                params![recording_id],
                Self::row_to_job,
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all_jobs(&self) -> Result<Vec<AtomikKbJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut stmt = c
            .prepare("SELECT recording_id,status,error,export_index,attempts FROM atomik_kb_jobs")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], Self::row_to_job)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }
    /// Queues a meeting for export if it is not already queued/exporting/completed.
    /// A no-op (not an error) if a job already exists -- the trigger tick calls
    /// this on every ready recording, so it must be safe to call repeatedly.
    pub fn enqueue(&self, recording_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "INSERT INTO atomik_kb_jobs(recording_id,status,export_index) VALUES(?1,'queued',1) ON CONFLICT(recording_id) DO NOTHING",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// A crashed/interrupted "exporting" job resumes as "queued" on startup.
    pub fn recover(&self) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_jobs SET status='queued' WHERE status='exporting'",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// The next due job: queued, and (if it previously failed transiently)
    /// past its backoff deadline.
    pub fn next(&self) -> Result<Option<AtomikKbJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let now = now_ms();
        c.query_row(
            "SELECT recording_id,status,error,export_index,attempts FROM atomik_kb_jobs \
             WHERE status='queued' AND (retry_after IS NULL OR retry_after<=?1) LIMIT 1",
            params![now],
            Self::row_to_job,
        )
        .optional()
        .map_err(|e| e.to_string())
    }
    pub fn claim(&self, recording_id: &str) -> Result<bool, String> {
        Ok(self
            .c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_jobs SET status='exporting',attempts=attempts+1 WHERE recording_id=?1 AND status='queued'",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?
            == 1)
    }
    pub fn complete(&self, recording_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_jobs SET status='completed',error=NULL WHERE recording_id=?1",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// `transient` schedules a backoff retry (atomik-kb closed/locked, 5xx);
    /// otherwise the job is a permanent failure the user must retry manually
    /// (a bad token, a missing grant, a tool-level refusal), surfaced
    /// verbatim in `error`.
    pub fn fail(&self, recording_id: &str, error: &str, transient: bool) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        if transient {
            let attempts: i64 = c.query_row("SELECT attempts FROM atomik_kb_jobs WHERE recording_id=?1", params![recording_id], |r| r.get(0)).unwrap_or(0);
            let retry_after = now_ms() + backoff_seconds(attempts.max(0) as u32) * 1000;
            c.execute("UPDATE atomik_kb_jobs SET status='queued',error=?2,retry_after=?3 WHERE recording_id=?1", params![recording_id, error, retry_after])
        } else {
            c.execute("UPDATE atomik_kb_jobs SET status='failed',error=?2 WHERE recording_id=?1", params![recording_id, error])
        }
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// User-triggered retry of a permanently-failed job.
    pub fn retry(&self, recording_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute("UPDATE atomik_kb_jobs SET status='queued',error=NULL,retry_after=NULL WHERE recording_id=?1 AND status='failed'", params![recording_id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// A deletion is allowed to remove a dormant/completed/failed job row, but
    /// never one whose export is actively in flight. Deliberately leaves
    /// `atomik_kb_pages` (the create-only idempotency ledger) untouched: an
    /// exported page already living in atomik-kb is never modified or removed
    /// by deleting the local recording.
    pub fn remove_job_if_idle(&self, recording_id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let status: Option<String> = c
            .query_row(
                "SELECT status FROM atomik_kb_jobs WHERE recording_id=?1",
                params![recording_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if status.as_deref() == Some("exporting") {
            return Err("An atomik-kb export is in progress; wait for it to finish before deleting this recording.".into());
        }
        c.execute(
            "DELETE FROM atomik_kb_jobs WHERE recording_id=?1",
            params![recording_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// "Export again": only valid once the previous export finished (either
    /// way); bumps `export_index` so the next tick creates a brand-new
    /// meeting page (never touching the old one) rather than resuming into
    /// the existing `atomik_kb_pages` row for the prior index.
    pub fn export_again(&self, recording_id: &str) -> Result<(), String> {
        let n = self
            .c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_jobs SET status='queued',error=NULL,retry_after=NULL,export_index=export_index+1 \
                 WHERE recording_id=?1 AND status IN('completed','failed')",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("A meeting can only be exported again once its current export has finished.".into())
        }
    }
}

#[cfg(test)]
impl AtomikKbStore {
    /// Test-only: forces a job's status, e.g. to simulate a crash after the
    /// pages were journaled but before the job was marked completed.
    pub fn force_status(&self, recording_id: &str, status: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE atomik_kb_jobs SET status=?2,retry_after=NULL WHERE recording_id=?1",
                params![recording_id, status],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "atomik_kb_store_tests.rs"]
mod tests;
