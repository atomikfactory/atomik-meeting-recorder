//! SQLite processing journal. Completion and artifact identifiers commit together.
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct StoredJob {
    pub recording_id: String,
    pub status: String,
    pub model_key: String,
    pub model_hash: String,
    pub model_path: Option<String>,
    pub progress: u8,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
    pub markdown_path: Option<String>,
    pub checksum: Option<String>,
    pub attempts: u32,
    pub manual: bool,
}
pub struct TranscriptionStore {
    conn: Mutex<Connection>,
}
#[derive(Clone, Debug)]
pub struct RestartIntent {
    pub recording_id: String,
    pub model_key: String,
    pub model_hash: String,
    pub archive_id: String,
}
const FIELDS: &str = "recording_id,status,model_key,model_hash,model_path,progress,error,transcript_path,markdown_path,checksum,attempts,manual";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredJob> {
    Ok(StoredJob {
        recording_id: r.get(0)?,
        status: r.get(1)?,
        model_key: r.get(2)?,
        model_hash: r.get(3)?,
        model_path: r.get(4)?,
        progress: r.get(5)?,
        error: r.get(6)?,
        transcript_path: r.get(7)?,
        markdown_path: r.get(8)?,
        checksum: r.get(9)?,
        attempts: r.get(10)?,
        manual: r.get(11)?,
    })
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl TranscriptionStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(err)?;
        }
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Transcription database must not be a symlink.".into());
        }
        let connection = Connection::open(path).map_err(err)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(err)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS jobs(recording_id TEXT PRIMARY KEY,status TEXT NOT NULL,
            model_key TEXT NOT NULL,model_hash TEXT NOT NULL,model_path TEXT,
            progress INTEGER NOT NULL CHECK(progress BETWEEN 0 AND 100),error TEXT,
            transcript_path TEXT,markdown_path TEXT,checksum TEXT,attempts INTEGER NOT NULL DEFAULT 0,
            manual INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS models(model_key TEXT PRIMARY KEY,path TEXT NOT NULL,hash TEXT NOT NULL,bytes INTEGER NOT NULL);").map_err(err)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS transcription_restarts(recording_id TEXT PRIMARY KEY NOT NULL,model_key TEXT NOT NULL,model_hash TEXT NOT NULL,archive_id TEXT NOT NULL);").map_err(err)?;
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(jobs)")
            .map_err(err)?
            .query_map([], |r| r.get(1))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?;
        if !columns.iter().any(|c| c == "model_path") {
            connection
                .execute_batch("ALTER TABLE jobs ADD COLUMN model_path TEXT;")
                .map_err(err)?;
        }
        if !columns.iter().any(|c| c == "attempts") {
            connection
                .execute_batch("ALTER TABLE jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;")
                .map_err(err)?;
        }
        if !columns.iter().any(|c| c == "manual") {
            connection
                .execute_batch("ALTER TABLE jobs ADD COLUMN manual INTEGER NOT NULL DEFAULT 0;")
                .map_err(err)?;
        }
        Ok(Self {
            conn: Mutex::new(connection),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "Transcription database lock failed.".into())
    }
    pub fn recover(&self) -> Result<(), String> {
        self.lock()?.execute("UPDATE jobs SET status='queued',progress=0,error='Interrupted; queued for recovery.' WHERE status='transcribing'", []).map_err(err)?;
        Ok(())
    }
    pub fn upsert_model(
        &self,
        key: &str,
        path: &str,
        hash: &str,
        bytes: u64,
    ) -> Result<(), String> {
        let bytes = i64::try_from(bytes).map_err(err)?;
        self.lock()?.execute("INSERT INTO models(model_key,path,hash,bytes) VALUES(?1,?2,?3,?4) ON CONFLICT(model_key) DO UPDATE SET path=excluded.path,hash=excluded.hash,bytes=excluded.bytes", params![key,path,hash,bytes]).map_err(err)?;
        Ok(())
    }
    pub fn model(&self, key: &str) -> Result<Option<(String, String, u64)>, String> {
        self.lock()?
            .query_row(
                "SELECT path,hash,bytes FROM models WHERE model_key=?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(err)
    }
    pub fn enqueue(&self, id: &str, key: &str, hash: &str) -> Result<(), String> {
        self.enqueue_with_intent(id, key, hash, false)
    }
    pub fn enqueue_manual(&self, id: &str, key: &str, hash: &str) -> Result<(), String> {
        self.enqueue_with_intent(id, key, hash, true)
    }
    fn enqueue_with_intent(
        &self,
        id: &str,
        key: &str,
        hash: &str,
        manual: bool,
    ) -> Result<(), String> {
        if uuid::Uuid::parse_str(id).map_err(err)?.to_string() != id {
            return Err("Invalid recording identity.".into());
        }
        self.lock()?.execute("INSERT INTO jobs(recording_id,status,model_key,model_hash,progress,manual) VALUES(?1,'queued',?2,?3,0,?4) ON CONFLICT(recording_id) DO NOTHING",params![id,key,hash,manual]).map_err(err)?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<StoredJob>, String> {
        self.lock()?
            .query_row(
                &format!("SELECT {FIELDS} FROM jobs WHERE recording_id=?1"),
                params![id],
                row,
            )
            .optional()
            .map_err(err)
    }
    pub fn next(&self) -> Result<Option<StoredJob>, String> {
        self.lock()?
            .query_row(
                &format!("SELECT {FIELDS} FROM jobs WHERE status='queued' ORDER BY rowid LIMIT 1"),
                [],
                row,
            )
            .optional()
            .map_err(err)
    }
    pub fn all(&self) -> Result<Vec<StoredJob>, String> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(&format!("SELECT {FIELDS} FROM jobs ORDER BY rowid DESC"))
            .map_err(err)?;
        let rows = statement.query_map([], row).map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)
    }
    pub fn claim(&self, id: &str, path: &str, hash: &str) -> Result<bool, String> {
        Ok(self.lock()?.execute("UPDATE jobs SET status='transcribing',progress=0,error=NULL,model_path=?2,model_hash=?3,attempts=attempts+1 WHERE recording_id=?1 AND status='queued' AND (model_path IS NULL OR (model_path=?2 AND model_hash=?3))",params![id,path,hash]).map_err(err)? == 1)
    }
    pub fn status(
        &self,
        id: &str,
        status: &str,
        progress: u8,
        error: Option<&str>,
    ) -> Result<(), String> {
        let predecessor = match status {
            "waiting_model" => "queued",
            "queued" | "failed" | "transcribing" => "transcribing",
            _ => return Err("Invalid job transition.".into()),
        };
        if progress > 100 {
            return Err("Invalid progress.".into());
        }
        let changed = self.lock()?.execute("UPDATE jobs SET status=?2,progress=?3,error=?4 WHERE recording_id=?1 AND status=?5",params![id,status,progress,error,predecessor]).map_err(err)?;
        if changed != 1 {
            return Err("Job ownership changed; transition refused.".into());
        }
        Ok(())
    }
    pub fn complete(&self, id: &str, json: &str, md: &str, hash: &str) -> Result<(), String> {
        let changed = self.lock()?.execute("UPDATE jobs SET status='completed',progress=100,error=NULL,transcript_path=?2,markdown_path=?3,checksum=?4 WHERE recording_id=?1 AND status='transcribing'",params![id,json,md,hash]).map_err(err)?;
        if changed != 1 {
            return Err("Cannot complete an unclaimed job.".into());
        }
        Ok(())
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        let connection = self.lock()?;
        let status: String = connection
            .query_row(
                "SELECT status FROM jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?
            .ok_or("Unknown transcription job.")?;
        if matches!(status.as_str(), "completed" | "queued" | "transcribing") {
            return Ok(());
        }
        if !matches!(status.as_str(), "failed" | "waiting_model") {
            return Err("Invalid retry state.".into());
        }
        connection.execute("UPDATE jobs SET status='queued',progress=0,error=NULL WHERE recording_id=?1 AND status IN ('failed','waiting_model')",params![id]).map_err(err)?;
        Ok(())
    }
    pub fn retry_manual(&self, id: &str) -> Result<(), String> {
        let changed = self.lock()?.execute("UPDATE jobs SET status='queued',progress=0,error=NULL,manual=1 WHERE recording_id=?1 AND status IN ('failed','waiting_model')", params![id]).map_err(err)?;
        if changed == 1 {
            Ok(())
        } else {
            Err("Transcription is already queued or running; it was not retried.".into())
        }
    }
    pub fn requeue_waiting(&self, key: &str) -> Result<(), String> {
        self.lock()?.execute("UPDATE jobs SET status='queued',progress=0,error=NULL WHERE status='waiting_model' AND model_key=?1",params![key]).map_err(err)?;
        Ok(())
    }
    /// A job that has never been claimed (no pinned `model_path`) still tracks the
    /// currently selected model, so switching the setting or importing a model that
    /// matches it wakes those jobs without touching anything already claimed/retried.
    pub fn repin_unclaimed(&self, key: &str, hash: &str) -> Result<(), String> {
        self.lock()?.execute("UPDATE jobs SET status='queued',model_key=?1,model_hash=?2,progress=0,error=NULL WHERE model_path IS NULL AND (status='waiting_model' OR (status='queued' AND (model_key<>?1 OR model_hash<>?2)))",params![key,hash]).map_err(err)?;
        Ok(())
    }
    pub fn model_keys(&self) -> Result<Vec<String>, String> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT model_key FROM models ORDER BY model_key")
            .map_err(err)?;
        let rows = statement.query_map([], |r| r.get(0)).map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)
    }
    /// A deletion is allowed to remove dormant/completed journal rows, but never an
    /// in-flight inference claim.
    pub fn remove_if_idle(&self, id: &str) -> Result<(), String> {
        let connection = self.lock()?;
        let status: Option<String> = connection
            .query_row(
                "SELECT status FROM jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        if status.as_deref() == Some("transcribing") {
            return Err("Transcription is in progress; wait for it to finish before deleting this recording.".into());
        }
        connection
            .execute("DELETE FROM jobs WHERE recording_id=?1", params![id])
            .map_err(err)?;
        connection
            .execute(
                "DELETE FROM transcription_restarts WHERE recording_id=?1",
                params![id],
            )
            .map_err(err)?;
        Ok(())
    }
    pub fn begin_restart(&self, id: &str, key: &str, hash: &str) -> Result<RestartIntent, String> {
        let archive_id = uuid::Uuid::new_v4().to_string();
        let connection = self.lock()?;
        let status: Option<String> = connection
            .query_row(
                "SELECT status FROM jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        if !matches!(
            status.as_deref(),
            Some("completed" | "failed" | "waiting_model")
        ) {
            return Err("Transcription is already queued or running; it was not restarted.".into());
        }
        connection.execute("INSERT INTO transcription_restarts(recording_id,model_key,model_hash,archive_id) VALUES(?1,?2,?3,?4) ON CONFLICT(recording_id) DO NOTHING", params![id,key,hash,archive_id]).map_err(err)?;
        let intent = connection.query_row("SELECT recording_id,model_key,model_hash,archive_id FROM transcription_restarts WHERE recording_id=?1", params![id], |r| Ok(RestartIntent { recording_id:r.get(0)?, model_key:r.get(1)?, model_hash:r.get(2)?, archive_id:r.get(3)? })).map_err(err)?;
        Ok(intent)
    }
    pub fn restart_intents(&self) -> Result<Vec<RestartIntent>, String> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT recording_id,model_key,model_hash,archive_id FROM transcription_restarts ORDER BY rowid")
            .map_err(err)?;
        let rows = statement
            .query_map([], |r| {
                Ok(RestartIntent {
                    recording_id: r.get(0)?,
                    model_key: r.get(1)?,
                    model_hash: r.get(2)?,
                    archive_id: r.get(3)?,
                })
            })
            .map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)
    }
    pub fn has_restart_intent(&self, id: &str) -> Result<bool, String> {
        self.lock()?
            .query_row(
                "SELECT 1 FROM transcription_restarts WHERE recording_id=?1",
                params![id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(err)
    }
    pub fn finish_restart(&self, id: &str, key: &str, hash: &str) -> Result<(), String> {
        let mut connection = self.lock()?;
        let tx = connection.transaction().map_err(err)?;
        let changed = tx.execute("UPDATE jobs SET status='queued',model_key=?2,model_hash=?3,model_path=NULL,manual=1,progress=0,error=NULL,transcript_path=NULL,markdown_path=NULL,checksum=NULL WHERE recording_id=?1 AND status IN ('completed','failed','waiting_model')", params![id,key,hash]).map_err(err)?;
        if changed != 1 {
            return Err("Transcription is already queued or running; it was not restarted.".into());
        }
        tx.execute(
            "DELETE FROM transcription_restarts WHERE recording_id=?1",
            params![id],
        )
        .map_err(err)?;
        tx.commit().map_err(err)
    }
}
