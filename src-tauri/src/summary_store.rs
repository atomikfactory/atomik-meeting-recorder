//! Durable local summary job journal. Mirrors `diarization_store.rs`'s shape:
//! a single SQLite table is the source of truth for status/progress/pins, so a
//! crash mid-summary always resumes from a consistent row rather than a
//! half-written checkpoint.
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct SummaryJob {
    pub recording_id: String,
    pub parent_checksum: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub model: Option<String>,
    pub output_hash: Option<String>,
    pub title: Option<String>,
    #[cfg(test)]
    pub attempts: u32,
}
pub struct SummaryStore {
    c: Mutex<Connection>,
}
fn valid_id(x: &str) -> bool {
    uuid::Uuid::parse_str(x).is_ok_and(|id| id.to_string() == x)
}
fn valid_hash(x: &str) -> bool {
    x.len() == 64
        && x.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
/// Exponential backoff capped at 5 minutes: 5s, 10s, 20s, 40s, 80s, 160s, 300s...
fn backoff_seconds(attempts: u32) -> i64 {
    (5i64 * 2i64.pow(attempts.min(6))).min(300)
}
impl SummaryStore {
    pub fn open(p: &Path) -> Result<Self, String> {
        if let Some(x) = p.parent() {
            fs::create_dir_all(x).map_err(|e| e.to_string())?
        }
        if fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("summary database must not be a symlink".into());
        }
        let c = Connection::open(p).map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;CREATE TABLE IF NOT EXISTS summary_jobs(recording_id TEXT PRIMARY KEY,parent_checksum TEXT NOT NULL,status TEXT NOT NULL,progress INTEGER NOT NULL CHECK(progress BETWEEN 0 AND 100),error TEXT,model TEXT,output_hash TEXT,title TEXT,retry_after INTEGER,attempts INTEGER NOT NULL DEFAULT 0);").map_err(|e|e.to_string())?;
        Ok(Self { c: Mutex::new(c) })
    }
    pub fn enqueue(&self, id: &str, h: &str) -> Result<(), String> {
        if !valid_id(id) || !valid_hash(h) {
            return Err("Invalid recording ID or parent checksum.".into());
        }
        let c = self.c.lock().map_err(|_| "store lock")?;
        let old: Option<String> = c
            .query_row(
                "SELECT parent_checksum FROM summary_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match old {
            Some(x) if x != h => Err("Transcript checksum changed; create a new summary job.".into()),
            Some(_) => Ok(()),
            None => {
                c.execute("INSERT INTO summary_jobs(recording_id,parent_checksum,status,progress)VALUES(?1,?2,'queued',0)",params![id,h]).map_err(|e|e.to_string())?;
                Ok(())
            }
        }
    }
    pub fn recover(&self) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE summary_jobs SET status='queued',progress=0 WHERE status='summarizing'",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn wake_waiting(&self) -> Result<(), String> {
        self.c.lock().map_err(|_|"store lock")?.execute("UPDATE summary_jobs SET status='queued',progress=0,error=NULL WHERE status='waiting_ollama'",[]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn claim(&self, id: &str, model: &str) -> Result<bool, String> {
        if model.trim().is_empty() {
            return Err("Invalid summary model.".into());
        }
        Ok(self.c.lock().map_err(|_|"store lock")?.execute("UPDATE summary_jobs SET status='summarizing',progress=0,model=?2,attempts=attempts+1 WHERE recording_id=?1 AND status='queued' AND (model IS NULL OR model=?2)",params![id,model]).map_err(|e|e.to_string())?==1)
    }
    pub fn status(&self, id: &str, s: &str, p: u8, e: Option<&str>) -> Result<(), String> {
        let n=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE summary_jobs SET status=?2,progress=?3,error=?4 WHERE recording_id=?1 AND ((status='queued' AND ?2='waiting_ollama') OR (status='summarizing' AND ?2 IN('queued','failed','summarizing')))",params![id,s,p as i64,e]).map_err(|e|e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Invalid summary job transition.".into())
        }
    }
    /// Ollama being unreachable or missing a model is transient: back off instead
    /// of failing permanently, so the job resumes on its own once Ollama is up.
    pub fn wait_for_ollama(&self, id: &str, error: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let attempts: i64 = c
            .query_row(
                "SELECT attempts FROM summary_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let retry_after = now_ms() + backoff_seconds(attempts.max(0) as u32) * 1000;
        let n = c.execute("UPDATE summary_jobs SET status='waiting_ollama',progress=0,error=?2,retry_after=?3 WHERE recording_id=?1 AND status='summarizing'", params![id, error, retry_after]).map_err(|e| e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Invalid summary job transition.".into())
        }
    }
    /// Requeues every `waiting_ollama` job whose backoff has elapsed. Called once
    /// per worker tick so a down Ollama server is retried automatically.
    pub fn wake_due(&self) -> Result<(), String> {
        self.c.lock().map_err(|_| "store lock")?.execute("UPDATE summary_jobs SET status='queued',progress=0,error=NULL,retry_after=NULL WHERE status='waiting_ollama' AND retry_after IS NOT NULL AND retry_after<=?1", params![now_ms()]).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn complete(&self, id: &str, h: &str, title: &str) -> Result<(), String> {
        if !valid_hash(h) {
            return Err("Invalid summary output checksum.".into());
        }
        let x=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE summary_jobs SET status='completed',progress=100,error=NULL,output_hash=?2,title=?3 WHERE recording_id=?1 AND status='summarizing'",params![id,h,title]).map_err(|e|e.to_string())?;
        if x == 1 {
            Ok(())
        } else {
            Err("Invalid completion transition.".into())
        }
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let n=c.execute("UPDATE summary_jobs SET status='queued',progress=0,error=NULL WHERE recording_id=?1 AND status IN('failed','waiting_ollama')",params![id]).map_err(|e|e.to_string())?;
        if n>0||c.query_row("SELECT 1 FROM summary_jobs WHERE recording_id=?1 AND status IN('completed','queued','summarizing')",params![id],|_|Ok(())).optional().map_err(|e|e.to_string())?.is_some(){Ok(())}else{Err("Unknown summary job.".into())}
    }
    /// Explicit user request to redo a completed summary with the current model.
    pub fn regenerate(&self, id: &str) -> Result<(), String> {
        let n=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE summary_jobs SET status='queued',progress=0,error=NULL,model=NULL,output_hash=NULL WHERE recording_id=?1 AND status IN('completed','failed','waiting_ollama')",params![id]).map_err(|e|e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Summarizing is already queued or running; it was not regenerated.".into())
        }
    }
    pub fn remove_if_idle(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let status: Option<String> = c
            .query_row(
                "SELECT status FROM summary_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if status.as_deref() == Some("summarizing") {
            return Err("Summarizing is in progress; wait for it to finish before deleting this recording.".into());
        }
        c.execute(
            "DELETE FROM summary_jobs WHERE recording_id=?1",
            params![id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<SummaryJob>, String> {
        self.row(" WHERE recording_id=?1", params![id])
    }
    pub fn next(&self) -> Result<Option<SummaryJob>, String> {
        self.row(" WHERE status='queued' ORDER BY rowid LIMIT 1", [])
    }
    fn row<P: rusqlite::Params>(&self, w: &str, p: P) -> Result<Option<SummaryJob>, String> {
        let q=format!("SELECT recording_id,parent_checksum,status,progress,error,model,output_hash,title,attempts FROM summary_jobs{w}");
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(&q, p, |r| {
                Ok(SummaryJob {
                    recording_id: r.get(0)?,
                    parent_checksum: r.get(1)?,
                    status: r.get(2)?,
                    progress: r.get(3)?,
                    error: r.get(4)?,
                    model: r.get(5)?,
                    output_hash: r.get(6)?,
                    title: r.get(7)?,
                    #[cfg(test)]
                    attempts: r.get(8)?,
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all(&self) -> Result<Vec<SummaryJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut s = c
            .prepare("SELECT recording_id FROM summary_jobs ORDER BY rowid")
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
    fn hash() -> String {
        "a".repeat(64)
    }
    fn store() -> SummaryStore {
        SummaryStore::open(
            &std::env::temp_dir().join(format!("summary-store-{}.db", uuid::Uuid::new_v4())),
        )
        .unwrap()
    }
    #[test]
    fn validates_parent_and_uuid() {
        let s = store();
        assert!(s.enqueue("bad", &hash()).is_err());
        s.enqueue(&id(), &hash()).unwrap();
        assert!(s.enqueue(&id(), &"b".repeat(64)).is_err());
    }
    #[test]
    fn claim_pins_once_and_survives_recovery() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        assert!(s.claim(&id(), "qwen3:8b").unwrap());
        assert!(!s.claim(&id(), "qwen3:8b").unwrap());
        s.recover().unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "queued");
        assert_eq!(j.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(j.attempts, 1);
    }
    #[test]
    fn completed_is_immutable_and_retry_noop() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.claim(&id(), "qwen3:8b").unwrap();
        s.complete(&id(), &hash(), "Weekly sync").unwrap();
        assert!(s.complete(&id(), &hash(), "Weekly sync").is_err());
        s.retry(&id()).unwrap();
        assert_eq!(s.get(&id()).unwrap().unwrap().status, "completed");
    }
    #[test]
    fn waiting_wakes_and_all_is_complete() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.status(&id(), "waiting_ollama", 0, None).unwrap();
        s.wake_waiting().unwrap();
        assert_eq!(s.next().unwrap().unwrap().recording_id, id());
        assert_eq!(s.all().unwrap().len(), 1);
    }
    #[test]
    fn invalid_transitions_and_unknown_retry_fail() {
        let s = store();
        assert!(s.retry(&id()).is_err());
        s.enqueue(&id(), &hash()).unwrap();
        assert!(s.complete(&id(), &hash(), "Weekly sync").is_err());
    }
    #[test]
    fn regenerate_requeues_completed_and_rejects_in_progress() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.claim(&id(), "qwen3:8b").unwrap();
        s.complete(&id(), &hash(), "Weekly sync").unwrap();
        s.regenerate(&id()).unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "queued");
        assert!(j.model.is_none());
        s.claim(&id(), "qwen3:8b").unwrap();
        assert!(s.regenerate(&id()).is_err());
    }
    #[test]
    fn wait_for_ollama_backs_off_and_wake_due_respects_it() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.claim(&id(), "qwen3:8b").unwrap();
        s.wait_for_ollama(&id(), "Ollama is not reachable.").unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "waiting_ollama");
        assert_eq!(j.error.as_deref(), Some("Ollama is not reachable."));
        // Not due yet (backoff is seconds in the future): wake_due is a no-op.
        s.wake_due().unwrap();
        assert_eq!(s.get(&id()).unwrap().unwrap().status, "waiting_ollama");
        // Explicit wake (e.g. after a settings change) still requeues immediately.
        s.wake_waiting().unwrap();
        assert_eq!(s.get(&id()).unwrap().unwrap().status, "queued");
    }
    #[test]
    fn remove_if_idle_rejects_in_progress_job() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.claim(&id(), "qwen3:8b").unwrap();
        assert!(s.remove_if_idle(&id()).is_err());
        s.recover().unwrap();
        s.remove_if_idle(&id()).unwrap();
        assert!(s.get(&id()).unwrap().is_none());
    }
}
