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

/// Regenerates a recording's presentation Markdown (`transcript.md`, and
/// `transcript-speakers.md` if speaker identification has completed) from
/// their already-durable checkpoints (`transcript.json`/`speakers.json`) so
/// they reflect the current name overlay -- without re-running Whisper or
/// diarization. Called after a rename; also safe to call when nothing
/// changed (it simply rewrites the same bytes). Never touches the
/// checkpoints themselves.
pub fn regenerate_presentation(
    root: &Path,
    extra_roots: &[PathBuf],
    recording_id: &str,
) -> Result<(), String> {
    use crate::{
        diarization::{read_completed_checkpoint, speaker_markdown},
        recording_store::RecordingStore,
        transcription::{
            atomic_replace, checksum, markdown_document_labeled, output_path, read_checkpoint,
            recording_directory, validate_transcript,
        },
        transcription_store::TranscriptionStore,
        transcription_types::Transcript,
    };
    let recordings = RecordingStore::open(root)?;
    let recording = recordings
        .get_by_id(recording_id)?
        .ok_or("Recording metadata is unavailable.")?;
    let registry = TranscriptionStore::open(&root.join("transcription.sqlite3"))?;
    let job = registry
        .get(recording_id)?
        .ok_or("This recording has no transcript yet.")?;
    if job.status != "completed" {
        return Err("Transcript is not complete yet.".into());
    }
    let directory = recording_directory(root, extra_roots, recording_id)?;
    let json = output_path(&directory, "transcript.json")?;
    let bytes = read_checkpoint(&json)?;
    if Some(checksum(&bytes)) != job.checksum {
        return Err("Transcript checkpoint checksum mismatch; files preserved.".into());
    }
    let transcript: Transcript = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    validate_transcript(&transcript, &job, &recording)?;
    let names = names_for(root, recording_id);
    let md = output_path(&directory, "transcript.md")?;
    atomic_replace(
        &md,
        markdown_document_labeled(&recording, &transcript, None, Some(&names)).as_bytes(),
    )?;
    if let Some(checkpoint) = read_completed_checkpoint(root, extra_roots, &recording, &transcript)
    {
        let speakers_md = output_path(&directory, "transcript-speakers.md")?;
        if speakers_md.exists() {
            atomic_replace(
                &speakers_md,
                speaker_markdown(&recording, &transcript, &checkpoint, Some(&names)).as_bytes(),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recording::{Recording, Track},
        recording_store::RecordingStore,
        transcription::{checksum as transcript_checksum, markdown_document},
        transcription_store::TranscriptionStore,
        transcription_types::{Transcript, TranscriptSegment},
    };

    fn fixture() -> (PathBuf, Recording) {
        let root =
            std::env::temp_dir().join(format!("speaker-names-regen-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording = Recording {
            recording_id: id.clone(),
            started_at: 0,
            ended_at: Some(1000),
            duration_ms: 1000,
            status: "recorded".into(),
            meeting_title: None,
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            error: None,
            transcription_status: "pending".into(),
            appflowy_status: "pending".into(),
            tracks: vec![Track {
                source_id: "mic".into(),
                name: "Microphone".into(),
                direction: "input".into(),
                sample_rate: 16000,
                channels: 1,
                file_path: dir.join("track-00.wav").display().to_string(),
                frames_written: 16000,
                dropped_samples: 0,
                error: None,
            }],
        };
        recordings.save(&recording).unwrap();
        let transcript = Transcript {
            recording_id: id.clone(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec![],
            local_speaker_name: Some("You".into()),
            segments: vec![TranscriptSegment {
                start_ms: 0,
                end_ms: 1000,
                text: "hello".into(),
                source_id: "mic".into(),
                source_name: "Microphone".into(),
            }],
        };
        let json = serde_json::to_vec_pretty(&transcript).unwrap();
        fs::write(dir.join("transcript.json"), &json).unwrap();
        fs::write(
            dir.join("transcript.md"),
            markdown_document(&recording, &transcript),
        )
        .unwrap();
        registry
            .enqueue(&id, "base", &transcript.model_hash)
            .unwrap();
        assert!(registry
            .claim(&id, "unused-fake-model", &transcript.model_hash)
            .unwrap());
        registry
            .complete(
                &id,
                &dir.join("transcript.json").display().to_string(),
                &dir.join("transcript.md").display().to_string(),
                &transcript_checksum(&json),
            )
            .unwrap();
        (root, recording)
    }

    #[test]
    fn regenerate_presentation_rewrites_transcript_markdown_with_the_new_name() {
        let (root, recording) = fixture();
        let names = SpeakerNameStore::open(store_path(&root)).unwrap();
        names
            .set(&recording.recording_id, "You", "Ben", false)
            .unwrap();
        regenerate_presentation(&root, &[], &recording.recording_id).unwrap();
        let md =
            fs::read_to_string(root.join(&recording.recording_id).join("transcript.md")).unwrap();
        assert!(md.contains("Ben:"));
        assert!(!md.contains("You:"));
    }

    #[test]
    fn regenerate_presentation_is_idempotent_and_crash_safe_to_repeat() {
        let (root, recording) = fixture();
        let names = SpeakerNameStore::open(store_path(&root)).unwrap();
        names
            .set(&recording.recording_id, "You", "Ben", false)
            .unwrap();
        regenerate_presentation(&root, &[], &recording.recording_id).unwrap();
        // Calling it again (as if a crash happened right after the first
        // regeneration and the caller retries) must not error just because
        // the file already exists with the same content.
        regenerate_presentation(&root, &[], &recording.recording_id).unwrap();
        let md =
            fs::read_to_string(root.join(&recording.recording_id).join("transcript.md")).unwrap();
        assert!(md.contains("Ben:"));
    }

    fn store() -> SpeakerNameStore {
        SpeakerNameStore::open(
            std::env::temp_dir().join(format!("speaker-names-{}.db", uuid::Uuid::new_v4())),
        )
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
        let path =
            std::env::temp_dir().join(format!("speaker-names-reopen-{}.db", uuid::Uuid::new_v4()));
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
        let root =
            std::env::temp_dir().join(format!("speaker-names-root-{}", uuid::Uuid::new_v4()));
        assert!(names_for(&root, "rec-1").is_empty());
    }

    #[test]
    fn names_changed_since_compares_against_the_summary_timestamp() {
        let root =
            std::env::temp_dir().join(format!("speaker-names-changed-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SpeakerNameStore::open(store_path(&root)).unwrap();
        store.set("rec-1", "Speaker 1", "Alice", false).unwrap();
        assert!(names_changed_since(&root, "rec-1", 0));
        assert!(!names_changed_since(&root, "rec-1", now() + 3600));
    }
}
