//! Durable local journal for AppFlowy export. Two tables:
//! `appflowy_pages` is the create-only idempotency ledger described in the
//! research pass's idempotency section -- one row per page this integration
//! has ever planned or created (root "My daily meetings", one per date, one
//! per meeting export), keyed by a deterministic local key so a retry after a
//! crash can tell "did I already create this" without re-deriving anything
//! from AppFlowy. `appflowy_jobs` is the export work queue, mirroring
//! `summary_store.rs`'s shape (a single SQLite table is the source of truth
//! for status/attempts, so a crash mid-export always resumes from a
//! consistent row).
use rusqlite::{params, Connection, OptionalExtension};
use std::{fs, path::Path, sync::Mutex};

/// `root:{workspace_id}:{space_view_id}` for "My daily meetings",
/// `date:{workspace_id}:{root_view_id}:{date}` for a date page, and
/// `meeting:{recording_id}:{export_index}` for a meeting page. Deterministic
/// so re-planning after a crash reuses the same key rather than minting a
/// new one and losing track of a page that may already exist.
pub fn root_key(workspace_id: &str, space_view_id: &str) -> String {
    format!("root:{workspace_id}:{space_view_id}")
}
pub fn date_key(workspace_id: &str, root_view_id: &str, date: &str) -> String {
    format!("date:{workspace_id}:{root_view_id}:{date}")
}
pub fn meeting_key(recording_id: &str, export_index: u32) -> String {
    format!("meeting:{recording_id}:{export_index}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageRecord {
    pub key: String,
    pub kind: String,
    pub parent_key: Option<String>,
    pub workspace_id: String,
    pub name: String,
    pub planned_view_id: String,
    pub remote_view_id: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppFlowyJob {
    pub recording_id: String,
    pub status: String,
    pub error: Option<String>,
    pub export_index: u32,
    #[cfg(test)]
    pub attempts: u32,
}

pub struct AppFlowyStore {
    c: Mutex<Connection>,
}
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}
/// Same shape as `summary_store::backoff_seconds`: 5s, 10s, 20s, ... capped
/// at 5 minutes.
fn backoff_seconds(attempts: u32) -> i64 {
    (5i64 * 2i64.pow(attempts.min(6))).min(300)
}

impl AppFlowyStore {
    pub fn open(p: &Path) -> Result<Self, String> {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err("appflowy database must not be a symlink".into());
        }
        let c = Connection::open(p).map_err(|e| e.to_string())?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;\
             CREATE TABLE IF NOT EXISTS appflowy_pages(\
                key TEXT PRIMARY KEY,kind TEXT NOT NULL,parent_key TEXT,workspace_id TEXT NOT NULL,\
                name TEXT NOT NULL,planned_view_id TEXT NOT NULL,remote_view_id TEXT,status TEXT NOT NULL);\
             CREATE TABLE IF NOT EXISTS appflowy_jobs(\
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
            workspace_id: row.get(3)?,
            name: row.get(4)?,
            planned_view_id: row.get(5)?,
            remote_view_id: row.get(6)?,
            status: row.get(7)?,
        })
    }
    pub fn get_page(&self, key: &str) -> Result<Option<PageRecord>, String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(
                "SELECT key,kind,parent_key,workspace_id,name,planned_view_id,remote_view_id,status FROM appflowy_pages WHERE key=?1",
                params![key],
                Self::row_to_page,
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    /// Returns the existing row for `key` if one is already planned/created;
    /// otherwise inserts a new `status='creating'` row with a freshly
    /// generated `planned_view_id` and returns that. Never overwrites an
    /// existing row -- idempotency without ever updating what a previous run
    /// already recorded.
    pub fn find_or_plan(
        &self,
        key: &str,
        kind: &str,
        parent_key: Option<&str>,
        workspace_id: &str,
        name: &str,
    ) -> Result<PageRecord, String> {
        if let Some(existing) = self.get_page(key)? {
            return Ok(existing);
        }
        let planned_view_id = uuid::Uuid::new_v4().to_string();
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "INSERT INTO appflowy_pages(key,kind,parent_key,workspace_id,name,planned_view_id,remote_view_id,status)\
                 VALUES(?1,?2,?3,?4,?5,?6,NULL,'creating')",
                params![key, kind, parent_key, workspace_id, name, planned_view_id],
            )
            .map_err(|e| e.to_string())?;
        self.get_page(key)?.ok_or_else(|| "page record vanished after insert".into())
    }
    /// Marks a page created (or adopted from the folder tree on retry).
    /// Idempotent: calling it twice with the same `remote_view_id` is fine.
    pub fn mark_created(&self, key: &str, remote_view_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE appflowy_pages SET remote_view_id=?2,status='created' WHERE key=?1",
                params![key, remote_view_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn row_to_job(row: &rusqlite::Row) -> rusqlite::Result<AppFlowyJob> {
        Ok(AppFlowyJob {
            recording_id: row.get(0)?,
            status: row.get(1)?,
            error: row.get(2)?,
            export_index: row.get(3)?,
            #[cfg(test)]
            attempts: row.get(4)?,
        })
    }
    pub fn get_job(&self, recording_id: &str) -> Result<Option<AppFlowyJob>, String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(
                "SELECT recording_id,status,error,export_index,attempts FROM appflowy_jobs WHERE recording_id=?1",
                params![recording_id],
                Self::row_to_job,
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all_jobs(&self) -> Result<Vec<AppFlowyJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut stmt = c
            .prepare("SELECT recording_id,status,error,export_index,attempts FROM appflowy_jobs")
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
    /// this on every completed-summary recording, so it must be safe to call
    /// repeatedly.
    pub fn enqueue(&self, recording_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "INSERT INTO appflowy_jobs(recording_id,status,export_index) VALUES(?1,'queued',1)\
                 ON CONFLICT(recording_id) DO NOTHING",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// A crashed/interrupted "exporting" job resumes as "queued" on startup,
    /// exactly like `summary_store::recover`.
    pub fn recover(&self) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute("UPDATE appflowy_jobs SET status='queued' WHERE status='exporting'", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// The next due job: queued, and (if it previously failed transiently)
    /// past its backoff deadline.
    pub fn next(&self) -> Result<Option<AppFlowyJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let now = now_ms();
        c.query_row(
            "SELECT recording_id,status,error,export_index,attempts FROM appflowy_jobs \
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
                "UPDATE appflowy_jobs SET status='exporting',attempts=attempts+1 WHERE recording_id=?1 AND status='queued'",
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
                "UPDATE appflowy_jobs SET status='completed',error=NULL WHERE recording_id=?1",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// `transient` schedules a backoff retry (network/5xx/401-after-refresh);
    /// otherwise the job is a permanent failure the user must retry manually
    /// (a 4xx validation error, surfaced verbatim in `error`).
    pub fn fail(&self, recording_id: &str, error: &str, transient: bool) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        if transient {
            let attempts: i64 = c
                .query_row("SELECT attempts FROM appflowy_jobs WHERE recording_id=?1", params![recording_id], |r| r.get(0))
                .unwrap_or(0);
            let retry_after = now_ms() + backoff_seconds(attempts.max(0) as u32) * 1000;
            c.execute(
                "UPDATE appflowy_jobs SET status='queued',error=?2,retry_after=?3 WHERE recording_id=?1",
                params![recording_id, error, retry_after],
            )
        } else {
            c.execute(
                "UPDATE appflowy_jobs SET status='failed',error=?2 WHERE recording_id=?1",
                params![recording_id, error],
            )
        }
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// User-triggered retry of a permanently-failed job.
    pub fn retry(&self, recording_id: &str) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE appflowy_jobs SET status='queued',error=NULL,retry_after=NULL WHERE recording_id=?1 AND status='failed'",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// "Export again": only valid once the previous export finished (either
    /// way); bumps `export_index` so the next tick creates a brand-new
    /// meeting page (never touching the old one) rather than resuming into
    /// the existing `appflowy_pages` row for the prior index.
    pub fn export_again(&self, recording_id: &str) -> Result<(), String> {
        let n = self
            .c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE appflowy_jobs SET status='queued',error=NULL,retry_after=NULL,export_index=export_index+1 \
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
#[path = "appflowy_store_tests.rs"]
mod tests;
