//! Durable compression job journal. Mirrors `summary_store.rs`/`diarization_store.rs`'s
//! shape: a single SQLite table is the source of truth for status/progress, so a
//! crash mid-compression always resumes from a consistent row. Unlike those stores,
//! there is no parent checkpoint to pin against -- eligibility is re-derived each
//! worker tick from the recording/transcription/diarization stores directly (see
//! `audio_compress.rs::tick`) -- so this store only tracks this stage's own state.
use rusqlite::{params, Connection, OptionalExtension};
use std::{fs, path::Path, sync::Mutex};

#[derive(Clone, Debug)]
pub struct CompressionJob {
    pub recording_id: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
    #[cfg(test)]
    pub attempts: u32,
}
pub struct CompressionStore {
    c: Mutex<Connection>,
}
fn valid_id(x: &str) -> bool {
    uuid::Uuid::parse_str(x).is_ok_and(|id| id.to_string() == x)
}
impl CompressionStore {
    pub fn open(p: &Path) -> Result<Self, String> {
        if let Some(x) = p.parent() {
            fs::create_dir_all(x).map_err(|e| e.to_string())?
        }
        if fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("compression database must not be a symlink".into());
        }
        let c = Connection::open(p).map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;CREATE TABLE IF NOT EXISTS compression_jobs(recording_id TEXT PRIMARY KEY,status TEXT NOT NULL,progress INTEGER NOT NULL CHECK(progress BETWEEN 0 AND 100),error TEXT,bytes_before INTEGER,bytes_after INTEGER,attempts INTEGER NOT NULL DEFAULT 0);").map_err(|e|e.to_string())?;
        Ok(Self { c: Mutex::new(c) })
    }
    pub fn enqueue(&self, id: &str) -> Result<(), String> {
        if !valid_id(id) {
            return Err("Invalid recording ID.".into());
        }
        self.c.lock().map_err(|_| "store lock")?.execute("INSERT INTO compression_jobs(recording_id,status,progress)VALUES(?1,'queued',0) ON CONFLICT(recording_id) DO NOTHING",params![id]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn recover(&self) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE compression_jobs SET status='queued',progress=0 WHERE status='compressing'",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn claim(&self, id: &str) -> Result<bool, String> {
        Ok(self.c.lock().map_err(|_|"store lock")?.execute("UPDATE compression_jobs SET status='compressing',progress=0,attempts=attempts+1 WHERE recording_id=?1 AND status='queued'",params![id]).map_err(|e|e.to_string())?==1)
    }
    pub fn status(&self, id: &str, s: &str, p: u8, e: Option<&str>) -> Result<(), String> {
        let n=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE compression_jobs SET status=?2,progress=?3,error=?4 WHERE recording_id=?1 AND status='compressing' AND ?2 IN('queued','failed','compressing')",params![id,s,p as i64,e]).map_err(|e|e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Invalid compression job transition.".into())
        }
    }
    pub fn complete(&self, id: &str, bytes_before: u64, bytes_after: u64) -> Result<(), String> {
        let x=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE compression_jobs SET status='completed',progress=100,error=NULL,bytes_before=?2,bytes_after=?3 WHERE recording_id=?1 AND status='compressing'",params![id,bytes_before as i64,bytes_after as i64]).map_err(|e|e.to_string())?;
        if x == 1 {
            Ok(())
        } else {
            Err("Invalid completion transition.".into())
        }
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let n=c.execute("UPDATE compression_jobs SET status='queued',progress=0,error=NULL WHERE recording_id=?1 AND status='failed'",params![id]).map_err(|e|e.to_string())?;
        if n>0||c.query_row("SELECT 1 FROM compression_jobs WHERE recording_id=?1 AND status IN('completed','queued','compressing')",params![id],|_|Ok(())).optional().map_err(|e|e.to_string())?.is_some(){Ok(())}else{Err("Unknown compression job.".into())}
    }
    pub fn remove_if_idle(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let status: Option<String> = c
            .query_row(
                "SELECT status FROM compression_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if status.as_deref() == Some("compressing") {
            return Err("Compression is in progress; wait for it to finish before deleting this recording.".into());
        }
        c.execute(
            "DELETE FROM compression_jobs WHERE recording_id=?1",
            params![id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<CompressionJob>, String> {
        self.row(" WHERE recording_id=?1", params![id])
    }
    pub fn next(&self) -> Result<Option<CompressionJob>, String> {
        self.row(" WHERE status='queued' ORDER BY rowid LIMIT 1", [])
    }
    fn row<P: rusqlite::Params>(&self, w: &str, p: P) -> Result<Option<CompressionJob>, String> {
        let q = format!(
            "SELECT recording_id,status,progress,error,bytes_before,bytes_after,attempts FROM compression_jobs{w}"
        );
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(&q, p, |r| {
                Ok(CompressionJob {
                    recording_id: r.get(0)?,
                    status: r.get(1)?,
                    progress: r.get(2)?,
                    error: r.get(3)?,
                    bytes_before: r
                        .get::<_, Option<i64>>(4)?
                        .map(|v| v.max(0) as u64),
                    bytes_after: r
                        .get::<_, Option<i64>>(5)?
                        .map(|v| v.max(0) as u64),
                    #[cfg(test)]
                    attempts: r.get(6)?,
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all(&self) -> Result<Vec<CompressionJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut s = c
            .prepare("SELECT recording_id FROM compression_jobs ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let ids = s
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(s);
        drop(c);
        ids.into_iter()
            .map(|id| self.get(&id)?.ok_or("job disappeared".into()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id() -> String {
        "123e4567-e89b-12d3-a456-426614174000".into()
    }
    fn store() -> CompressionStore {
        CompressionStore::open(
            &std::env::temp_dir().join(format!("compression-store-{}.db", uuid::Uuid::new_v4())),
        )
        .unwrap()
    }
    #[test]
    fn rejects_invalid_id() {
        let s = store();
        assert!(s.enqueue("bad").is_err());
    }
    #[test]
    fn claim_pins_once_and_survives_recovery() {
        let s = store();
        s.enqueue(&id()).unwrap();
        assert!(s.claim(&id()).unwrap());
        assert!(!s.claim(&id()).unwrap());
        s.recover().unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "queued");
        assert_eq!(j.attempts, 1);
    }
    #[test]
    fn completed_is_immutable_and_retry_noop() {
        let s = store();
        s.enqueue(&id()).unwrap();
        s.claim(&id()).unwrap();
        s.complete(&id(), 1000, 400).unwrap();
        assert!(s.complete(&id(), 1000, 400).is_err());
        s.retry(&id()).unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "completed");
        assert_eq!(j.bytes_before, Some(1000));
        assert_eq!(j.bytes_after, Some(400));
    }
    #[test]
    fn failed_status_is_recorded_and_retryable() {
        let s = store();
        s.enqueue(&id()).unwrap();
        s.claim(&id()).unwrap();
        s.status(&id(), "failed", 0, Some("verification failed")).unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "failed");
        assert_eq!(j.error.as_deref(), Some("verification failed"));
        s.retry(&id()).unwrap();
        assert_eq!(s.get(&id()).unwrap().unwrap().status, "queued");
    }
    #[test]
    fn remove_if_idle_rejects_in_progress_job() {
        let s = store();
        s.enqueue(&id()).unwrap();
        s.claim(&id()).unwrap();
        assert!(s.remove_if_idle(&id()).is_err());
        s.recover().unwrap();
        s.remove_if_idle(&id()).unwrap();
        assert!(s.get(&id()).unwrap().is_none());
    }
    #[test]
    fn all_lists_every_job() {
        let s = store();
        s.enqueue(&id()).unwrap();
        assert_eq!(s.all().unwrap().len(), 1);
    }
}
