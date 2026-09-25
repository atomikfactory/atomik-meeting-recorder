use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, sync::Mutex};
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ModelPair {
    pub segmentation_path: String,
    pub segmentation_hash: String,
    pub embedding_path: String,
    pub embedding_hash: String,
}
#[derive(Clone, Debug)]
pub struct SpeakerJob {
    pub recording_id: String,
    pub parent_checksum: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub models: Option<ModelPair>,
    pub output_hash: Option<String>,
    pub speaker_count: Option<u32>,
    #[cfg(test)]
    pub attempts: u32,
}
pub struct DiarizationStore {
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
impl DiarizationStore {
    pub fn open(p: &Path) -> Result<Self, String> {
        if let Some(x) = p.parent() {
            fs::create_dir_all(x).map_err(|e| e.to_string())?
        }
        if fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("diarization database must not be a symlink".into());
        }
        let c = Connection::open(p).map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;CREATE TABLE IF NOT EXISTS speaker_jobs(recording_id TEXT PRIMARY KEY,parent_checksum TEXT NOT NULL,status TEXT NOT NULL,progress INTEGER NOT NULL CHECK(progress BETWEEN 0 AND 100),error TEXT,models TEXT,output_hash TEXT,speaker_count INTEGER,attempts INTEGER NOT NULL DEFAULT 0);").map_err(|e|e.to_string())?;
        Ok(Self { c: Mutex::new(c) })
    }
    pub fn enqueue(&self, id: &str, h: &str) -> Result<(), String> {
        if !valid_id(id) || !valid_hash(h) {
            return Err("Invalid recording ID or parent checksum.".into());
        }
        let c = self.c.lock().map_err(|_| "store lock")?;
        let old: Option<String> = c
            .query_row(
                "SELECT parent_checksum FROM speaker_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match old {
            Some(x) if x != h => {
                Err("Transcript checksum changed; create a new speaker job.".into())
            }
            Some(_) => Ok(()),
            None => {
                c.execute("INSERT INTO speaker_jobs(recording_id,parent_checksum,status,progress)VALUES(?1,?2,'queued',0)",params![id,h]).map_err(|e|e.to_string())?;
                Ok(())
            }
        }
    }
    pub fn recover(&self) -> Result<(), String> {
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .execute(
                "UPDATE speaker_jobs SET status='queued',progress=0 WHERE status='diarizing'",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn wake_waiting(&self) -> Result<(), String> {
        self.c.lock().map_err(|_|"store lock")?.execute("UPDATE speaker_jobs SET status='queued',progress=0,error=NULL WHERE status='waiting_models'",[]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn claim(&self, id: &str, m: &ModelPair) -> Result<bool, String> {
        if m.segmentation_path.is_empty()
            || m.embedding_path.is_empty()
            || !valid_hash(&m.segmentation_hash)
            || !valid_hash(&m.embedding_hash)
        {
            return Err("Invalid pinned speaker models.".into());
        }
        let j = serde_json::to_string(m).map_err(|e| e.to_string())?;
        Ok(self.c.lock().map_err(|_|"store lock")?.execute("UPDATE speaker_jobs SET status='diarizing',progress=0,models=?2,attempts=attempts+1 WHERE recording_id=?1 AND status='queued' AND (models IS NULL OR models=?2)",params![id,j]).map_err(|e|e.to_string())?==1)
    }
    pub fn status(&self, id: &str, s: &str, p: u8, e: Option<&str>) -> Result<(), String> {
        let n=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE speaker_jobs SET status=?2,progress=?3,error=?4 WHERE recording_id=?1 AND ((status='queued' AND ?2='waiting_models') OR (status='diarizing' AND ?2 IN('queued','failed','diarizing')))",params![id,s,p as i64,e]).map_err(|e|e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Invalid speaker job transition.".into())
        }
    }
    pub fn complete(&self, id: &str, h: &str, n: u32) -> Result<(), String> {
        if !valid_hash(h) {
            return Err("Invalid speaker output checksum.".into());
        }
        let x=self.c.lock().map_err(|_|"store lock")?.execute("UPDATE speaker_jobs SET status='completed',progress=100,error=NULL,output_hash=?2,speaker_count=?3 WHERE recording_id=?1 AND status='diarizing'",params![id,h,n as i64]).map_err(|e|e.to_string())?;
        if x == 1 {
            Ok(())
        } else {
            Err("Invalid completion transition.".into())
        }
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let n=c.execute("UPDATE speaker_jobs SET status='queued',progress=0,error=NULL WHERE recording_id=?1 AND status IN('failed','waiting_models')",params![id]).map_err(|e|e.to_string())?;
        if n>0||c.query_row("SELECT 1 FROM speaker_jobs WHERE recording_id=?1 AND status IN('completed','queued','diarizing')",params![id],|_|Ok(())).optional().map_err(|e|e.to_string())?.is_some(){Ok(())}else{Err("Unknown speaker job.".into())}
    }
    pub fn remove_if_idle(&self, id: &str) -> Result<(), String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let status: Option<String> = c
            .query_row(
                "SELECT status FROM speaker_jobs WHERE recording_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if status.as_deref() == Some("diarizing") {
            return Err("Speaker identification is in progress; wait for it to finish before deleting this recording.".into());
        }
        c.execute(
            "DELETE FROM speaker_jobs WHERE recording_id=?1",
            params![id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<SpeakerJob>, String> {
        self.row(" WHERE recording_id=?1", params![id])
    }
    pub fn next(&self) -> Result<Option<SpeakerJob>, String> {
        self.row(" WHERE status='queued' ORDER BY rowid LIMIT 1", [])
    }
    fn row<P: rusqlite::Params>(&self, w: &str, p: P) -> Result<Option<SpeakerJob>, String> {
        let q=format!("SELECT recording_id,parent_checksum,status,progress,error,models,output_hash,speaker_count,attempts FROM speaker_jobs{w}");
        self.c
            .lock()
            .map_err(|_| "store lock")?
            .query_row(&q, p, |r| {
                let m: Option<String> = r.get(5)?;
                Ok(SpeakerJob {
                    recording_id: r.get(0)?,
                    parent_checksum: r.get(1)?,
                    status: r.get(2)?,
                    progress: r.get(3)?,
                    error: r.get(4)?,
                    models: m
                        .map(|x| serde_json::from_str(&x))
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    output_hash: r.get(6)?,
                    speaker_count: r.get(7)?,
                    #[cfg(test)]
                    attempts: r.get(8)?,
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all(&self) -> Result<Vec<SpeakerJob>, String> {
        let c = self.c.lock().map_err(|_| "store lock")?;
        let mut s = c
            .prepare("SELECT recording_id FROM speaker_jobs ORDER BY rowid")
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
    fn pair() -> ModelPair {
        ModelPair {
            segmentation_path: "s".into(),
            segmentation_hash: hash(),
            embedding_path: "e".into(),
            embedding_hash: hash(),
        }
    }
    fn store() -> DiarizationStore {
        DiarizationStore::open(
            &std::env::temp_dir().join(format!("speaker-store-{}.db", uuid::Uuid::new_v4())),
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
        assert!(s.claim(&id(), &pair()).unwrap());
        assert!(!s.claim(&id(), &pair()).unwrap());
        s.recover().unwrap();
        let j = s.get(&id()).unwrap().unwrap();
        assert_eq!(j.status, "queued");
        assert_eq!(j.models, Some(pair()));
        assert_eq!(j.attempts, 1);
    }
    #[test]
    fn completed_is_immutable_and_retry_noop() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.claim(&id(), &pair()).unwrap();
        s.complete(&id(), &hash(), 2).unwrap();
        assert!(s.complete(&id(), &hash(), 2).is_err());
        s.retry(&id()).unwrap();
        assert_eq!(s.get(&id()).unwrap().unwrap().status, "completed");
    }
    #[test]
    fn waiting_wakes_and_all_is_complete() {
        let s = store();
        s.enqueue(&id(), &hash()).unwrap();
        s.status(&id(), "waiting_models", 0, None).unwrap();
        s.wake_waiting().unwrap();
        assert_eq!(s.next().unwrap().unwrap().recording_id, id());
        assert_eq!(s.all().unwrap().len(), 1);
    }
    #[test]
    fn invalid_transitions_and_unknown_retry_fail() {
        let s = store();
        assert!(s.retry(&id()).is_err());
        s.enqueue(&id(), &hash()).unwrap();
        assert!(s.complete(&id(), &hash(), 1).is_err());
    }
}
