//! Per-recording speaker display names ("name speakers once" feature).
//!
//! Storage choice: a small SQLite table in the app-state directory (like
//! `diarization_store.rs`/`appflowy_store.rs`), not a checkpointed JSON file
//! next to the transcript. Reasons:
//! - It matches every other piece of small per-recording pipeline metadata in
//!   this app (`speaker_jobs`, `summary_jobs`, `appflowy_exports`), so there is
//!   one storage pattern to reason about rather than two.
//! - A rename is a small, frequent, structured write (one row keyed by
//!   `(recording_id, label)`); SQLite's atomic `INSERT ... ON CONFLICT`
//!   gives crash safety for free, where a hand-rolled checkpointed JSON file
//!   would need its own tmp-file-plus-rename dance for what is otherwise a
//!   single `UPDATE`.
//! - It can be queried across every recording at once (needed later for
//!   voice-profile bookkeeping and for any future "rename this person
//!   everywhere" feature) without opening one file per recording folder.
//! - It lives in the app-state directory, never in the (possibly
//!   user-relocated, possibly removable) recordings folder, so it survives a
//!   recordings-folder move independently of the library files it labels.
//!
//! Crash safety: `PRAGMA journal_mode=WAL` plus `synchronous=FULL` (same as
//! `diarization_store.rs`) means a set-name call either fully lands or fully
//! does not; there is no partially-written state to recover from.
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Longest accepted display name -- generous for a real name while keeping
/// rendered Markdown/JSON/prompt output bounded (matches the existing
/// `store::MAX_LOCAL_SPEAKER_NAME_LEN` convention for "You").
pub const MAX_NAME_LEN: usize = 60;

pub struct SpeakerNameStore {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NamedLabel {
    pub label: String,
    pub name: String,
    pub auto_identified: bool,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Trims and validates a user-supplied display name: 1-60 non-control
/// characters once trimmed. Escaping for a particular output format (HTML,
/// Markdown, JSON) is the renderer's job, not this store's -- this only
/// guards against empty/oversized/control-character input reaching storage.
pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_NAME_LEN
        || trimmed.chars().any(|c| c.is_control())
    {
        return Err(format!(
            "Speaker names must be 1-{MAX_NAME_LEN} plain-text characters."
        ));
    }
    Ok(trimmed)
}

fn valid_recording_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 64 && !label.chars().any(|c| c.is_control())
}

impl SpeakerNameStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("speaker names database must not be a symlink".into());
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;\
             CREATE TABLE IF NOT EXISTS speaker_names(\
                recording_id TEXT NOT NULL,\
                label TEXT NOT NULL,\
                name TEXT NOT NULL,\
                auto_identified INTEGER NOT NULL DEFAULT 0,\
                updated_at INTEGER NOT NULL,\
                PRIMARY KEY(recording_id,label)\
             );",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    /// Sets (or renames) the display name for one label in one recording.
    /// `auto_identified` marks a name that voice-profile recognition assigned
    /// automatically rather than one the user typed; a later user edit always
    /// calls this again with `auto_identified = false`, so a correction is
    /// simply the newest row -- there is no merge of two different names.
    pub fn set(
        &self,
        recording_id: &str,
        label: &str,
        name: &str,
        auto_identified: bool,
    ) -> Result<(), String> {
        if !valid_recording_id(recording_id) || !valid_label(label) {
            return Err("Invalid recording ID or speaker label.".into());
        }
        let name = validate_name(name)?;
        self.conn
            .execute(
                "INSERT INTO speaker_names(recording_id,label,name,auto_identified,updated_at) \
                 VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(recording_id,label) DO UPDATE SET name=excluded.name, \
                 auto_identified=excluded.auto_identified, updated_at=excluded.updated_at",
                params![recording_id, label, name, auto_identified as i64, now()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Removes a per-recording name, reverting that label to its default
    /// presentation (e.g. back to plain "Speaker 2").
    pub fn delete(&self, recording_id: &str, label: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM speaker_names WHERE recording_id=?1 AND label=?2",
                params![recording_id, label],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Removes every name set for a recording, e.g. when the recording itself
    /// is deleted (see `recording_delete.rs`).
    pub fn delete_recording(&self, recording_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM speaker_names WHERE recording_id=?1",
                params![recording_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list(&self, recording_id: &str) -> Result<Vec<NamedLabel>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT label,name,auto_identified FROM speaker_names WHERE recording_id=?1 ORDER BY label",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![recording_id], |r| {
                Ok(NamedLabel {
                    label: r.get(0)?,
                    name: r.get(1)?,
                    auto_identified: r.get::<_, i64>(2)? != 0,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// Most recent name-change timestamp (unix seconds) for a recording, or
    /// `None` if nothing has been named yet. Used to decide whether a
    /// generated summary predates the current names (see
    /// `names_changed_since` below).
    pub fn last_changed(&self, recording_id: &str) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT MAX(updated_at) FROM speaker_names WHERE recording_id=?1",
                params![recording_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
            .map(Option::flatten)
    }
}

/// Best-effort label -> display name map for presentation (Markdown, search
/// index, summary prompt, AppFlowy export). Returns an empty map on any
/// failure (missing database, closed recording, ...) rather than propagating
/// an error: naming is a purely cosmetic overlay and must never block or
/// corrupt an otherwise-working pipeline stage, mirroring
/// `diarization::read_completed_speaker_labels`'s "optional stage" philosophy.
pub fn names_for(root: &Path, recording_id: &str) -> HashMap<String, String> {
    (|| -> Result<HashMap<String, String>, String> {
        let store = SpeakerNameStore::open(store_path(root))?;
        Ok(store
            .list(recording_id)?
            .into_iter()
            .map(|n| (n.label, n.name))
            .collect())
    })()
    .unwrap_or_default()
}

/// `true` if any speaker name for this recording was set/changed after
/// `summary_generated_at` (unix seconds) -- drives the "Speaker names
/// changed -- Regenerate summary to use them" hint. Never errors outward:
/// on any storage problem this conservatively reports "unchanged" so the
/// hint does not spuriously nag.
pub fn names_changed_since(root: &Path, recording_id: &str, summary_generated_at: i64) -> bool {
    SpeakerNameStore::open(store_path(root))
        .ok()
        .and_then(|store| store.last_changed(recording_id).ok().flatten())
        .is_some_and(|changed_at| changed_at > summary_generated_at)
}

pub fn store_path(root: &Path) -> PathBuf {
    root.join("speaker_names.sqlite3")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SpeakerNameStore {
        SpeakerNameStore::open(std::env::temp_dir().join(format!(
            "speaker-names-{}.db",
            uuid::Uuid::new_v4()
        )))
        .unwrap()
    }

    #[test]
    fn set_and_list_round_trip() {
        let s = store();
        s.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        s.set("rec-1", "Speaker 2", "Bob", true).unwrap();
        let mut list = s.list("rec-1").unwrap();
        list.sort_by(|a, b| a.label.cmp(&b.label));
        assert_eq!(
            list,
            vec![
                NamedLabel {
                    label: "Speaker 1".into(),
                    name: "Alice".into(),
                    auto_identified: false
                },
                NamedLabel {
                    label: "Speaker 2".into(),
                    name: "Bob".into(),
                    auto_identified: true
                },
            ]
        );
    }

    #[test]
    fn set_again_overwrites_and_clears_auto_identified() {
        let s = store();
        s.set("rec-1", "Speaker 1", "Alice", true).unwrap();
        s.set("rec-1", "Speaker 1", "Alicia", false).unwrap();
        let list = s.list("rec-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Alicia");
        assert!(!list[0].auto_identified);
    }

    #[test]
    fn trims_and_rejects_blank_or_oversized_names() {
        let s = store();
        s.set("rec-1", "Speaker 1", "  Alice  ", false).unwrap();
        assert_eq!(s.list("rec-1").unwrap()[0].name, "Alice");
        assert!(s.set("rec-1", "Speaker 1", "   ", false).is_err());
        assert!(s
            .set("rec-1", "Speaker 1", &"x".repeat(MAX_NAME_LEN + 1), false)
            .is_err());
        // A rejected update must not clobber the previously saved value.
        assert_eq!(s.list("rec-1").unwrap()[0].name, "Alice");
    }

    #[test]
    fn delete_reverts_one_label_without_touching_others() {
        let s = store();
        s.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        s.set("rec-1", "Speaker 2", "Bob", false).unwrap();
        s.delete("rec-1", "Speaker 1").unwrap();
        let list = s.list("rec-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "Speaker 2");
    }

    #[test]
    fn delete_recording_clears_everything_for_that_recording_only() {
        let s = store();
        s.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        s.set("rec-2", "Speaker 1", "Carol", false).unwrap();
        s.delete_recording("rec-1").unwrap();
        assert!(s.list("rec-1").unwrap().is_empty());
        assert_eq!(s.list("rec-2").unwrap().len(), 1);
    }

    #[test]
    fn names_persist_after_reopen_crash_safety() {
        let path = std::env::temp_dir().join(format!("speaker-names-reopen-{}.db", uuid::Uuid::new_v4()));
        {
            let s = SpeakerNameStore::open(&path).unwrap();
            s.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        }
        let reopened = SpeakerNameStore::open(&path).unwrap();
        assert_eq!(reopened.list("rec-1").unwrap()[0].name, "Alice");
    }

    #[test]
    fn last_changed_tracks_the_newest_set_call() {
        let s = store();
        assert_eq!(s.last_changed("rec-1").unwrap(), None);
        s.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        assert!(s.last_changed("rec-1").unwrap().is_some());
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let s = store();
        assert!(s.set("../escape", "Speaker 1", "Alice", false).is_err());
        assert!(s.set("rec-1", "", "Alice", false).is_err());
    }

    #[test]
    fn names_for_is_empty_when_nothing_was_set_or_store_is_unreachable() {
        let root = std::env::temp_dir().join(format!("speaker-names-root-{}", uuid::Uuid::new_v4()));
        assert!(names_for(&root, "rec-1").is_empty());
    }

    #[test]
    fn names_changed_since_compares_against_the_summary_timestamp() {
        let root = std::env::temp_dir().join(format!("speaker-names-changed-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SpeakerNameStore::open(store_path(&root)).unwrap();
        store.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        assert!(names_changed_since(&root, "rec-1", 0));
        assert!(!names_changed_since(&root, "rec-1", now() + 3600));
    }
}
