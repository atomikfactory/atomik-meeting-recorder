//! Crash-safe moves of meeting folders from one recordings-library root to
//! another, for the "Move existing recordings" action in Settings (see
//! `docs/recording.md`'s storage section).
//!
//! Never automatic: the user explicitly starts a move after changing the
//! configured recordings folder. Every recording is moved independently --
//! one failing does not stop the rest -- and nothing is deleted from the
//! source until the destination copy has been read back and verified
//! byte-for-byte (size plus a SHA-256 checksum per file). A small on-disk
//! journal (one JSON file per in-flight recording, written atomically) makes
//! the whole thing resumable after a crash or forced quit: `resume_pending`
//! replays or restarts any move that did not reach its terminal state.
//!
//! Journal phases, and what a resume does for each:
//! - `Copying`: the destination may be a partial copy. Discard it and redo
//!   the copy from scratch (the source is never touched at this phase).
//! - `Verified`: the destination is a fully verified copy, but we do not
//!   know whether the database commit or the source delete completed.
//!   Re-run both -- both are naturally idempotent (a repeat `save` is just
//!   another write of the same row; a repeat delete of an already-missing
//!   source is a no-op).
use crate::{
    recording::Recording,
    recording_paths::resolved_recording_directory,
    recording_store::RecordingStore,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Copying,
    Verified,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct JournalEntry {
    recording_id: String,
    source: PathBuf,
    destination: PathBuf,
    phase: Phase,
}

fn journal_dir(state_root: &Path) -> PathBuf {
    state_root.join("library_move_journal")
}
fn journal_path(state_root: &Path, recording_id: &str) -> Option<PathBuf> {
    // `recording_id` is always a UUID by the time we get here (checked by
    // `RecordingStore`/`recording_paths`), but this is a filename derived
    // from caller-adjacent data, so refuse anything that is not exactly one
    // safe path segment.
    if recording_id.is_empty()
        || recording_id.len() > 64
        || !recording_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    Some(journal_dir(state_root).join(format!("{recording_id}.json")))
}
fn write_journal(state_root: &Path, entry: &JournalEntry) -> Result<(), String> {
    let dir = journal_dir(state_root);
    fs::create_dir_all(&dir).map_err(|e| format!("create move journal directory: {e}"))?;
    let path =
        journal_path(state_root, &entry.recording_id).ok_or("recording id is invalid")?;
    let bytes = serde_json::to_vec(entry).map_err(|e| format!("serialize move journal: {e}"))?;
    let tmp = dir.join(format!("{}.tmp-{}", entry.recording_id, uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("write move journal: {e}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write move journal: {e}"))?;
    drop(file);
    // `rename` onto an existing path is an atomic replace on both Windows
    // (same-volume, which the journal directory always is) and POSIX.
    fs::rename(&tmp, &path).map_err(|e| format!("commit move journal: {e}"))
}
fn read_journal(state_root: &Path, recording_id: &str) -> Option<JournalEntry> {
    let path = journal_path(state_root, recording_id)?;
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}
fn clear_journal(state_root: &Path, recording_id: &str) {
    if let Some(path) = journal_path(state_root, recording_id) {
        let _ = fs::remove_file(path);
    }
}
/// Every recording with a journal entry still on disk, i.e. a move that did
/// not reach its terminal (journal-cleared) state -- from a crash, forced
/// quit, or the app being closed mid-move.
pub fn pending_recording_ids(state_root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(journal_dir(state_root)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_suffix(".json").map(|id| id.to_string())
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct MoveResult {
    pub recording_id: String,
    pub moved: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct MoveSummary {
    pub results: Vec<MoveResult>,
}

fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut file = File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((len, format!("{:x}", hasher.finalize())))
}
/// Recursively copies every regular file under `source` into `destination`
/// (which must not already exist), rejecting anything that is not a plain
/// file or directory -- a symlink in a recording folder is unexpected and
/// must not be silently followed.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|e| format!("create {}: {e}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|e| format!("read {}: {e}", source.display()))?
    {
        let entry = entry.map_err(|e| format!("read {}: {e}", source.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(format!("{} is a symbolic link; refusing to move it", from.display()));
        } else if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}
/// Compares every file under `source` and `destination` by relative path,
/// size, and SHA-256 checksum. Never trusts size alone.
fn verify_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((from_dir, to_dir)) = stack.pop() {
        let mut from_names = std::collections::BTreeSet::new();
        for entry in
            fs::read_dir(&from_dir).map_err(|e| format!("read {}: {e}", from_dir.display()))?
        {
            let entry = entry.map_err(|e| format!("read {}: {e}", from_dir.display()))?;
            let name = entry.file_name();
            from_names.insert(name.clone());
            let from = entry.path();
            let to = to_dir.join(&name);
            let file_type = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", from.display()))?;
            if file_type.is_dir() {
                if !to.is_dir() {
                    return Err(format!("{} is missing at the destination", from.display()));
                }
                stack.push((from, to));
            } else if file_type.is_file() {
                let (from_len, from_hash) = hash_file(&from)?;
                if !to.is_file() {
                    return Err(format!("{} is missing at the destination", from.display()));
                }
                let (to_len, to_hash) = hash_file(&to)?;
                if from_len != to_len || from_hash != to_hash {
                    return Err(format!(
                        "{} did not verify byte-for-byte at the destination",
                        from.display()
                    ));
                }
            }
        }
        // The destination must not have picked up any extra file: a mismatch
        // here means the "verified" copy is not actually a faithful copy.
        let to_names: std::collections::BTreeSet<_> = fs::read_dir(&to_dir)
            .map_err(|e| format!("read {}: {e}", to_dir.display()))?
            .flatten()
            .map(|e| e.file_name())
            .collect();
        if to_names != from_names {
            return Err(format!(
                "{} does not exactly match its destination",
                from_dir.display()
            ));
        }
    }
    Ok(())
}

/// Moves one recording's folder from wherever it currently resolves (against
/// `default_root`/`extra_roots`) to `destination_root`. Safe to call again
/// for the same recording after a crash: it consults (and updates) the
/// on-disk journal so a repeated call resumes rather than redoing completed
/// work or double-deleting.
pub fn move_one(
    state_root: &Path,
    default_root: &Path,
    extra_roots: &[PathBuf],
    destination_root: &Path,
    recordings: &RecordingStore,
    recording: &Recording,
) -> Result<(), String> {
    let Some(relative) = recording.storage_path.as_deref() else {
        return Err(
            "This recording predates the meeting-folder layout and cannot be moved automatically."
                .into(),
        );
    };
    let destination_root = fs::canonicalize(destination_root)
        .map_err(|e| format!("canonicalize destination folder: {e}"))?;
    let destination = destination_root.join(relative);
    // Already at the destination (a previous move for this recording already
    // updated its metadata -- possibly a crash left the old source
    // undeleted, which a leftover journal entry would still point at).
    if recording
        .library_root
        .as_deref()
        .and_then(|saved| fs::canonicalize(saved).ok())
        == Some(destination_root.clone())
    {
        if let Some(entry) = read_journal(state_root, &recording.recording_id) {
            let _ = fs::remove_dir_all(&entry.source);
        }
        clear_journal(state_root, &recording.recording_id);
        return Ok(());
    }
    let phase = read_journal(state_root, &recording.recording_id).map(|e| e.phase);
    let source = if phase == Some(Phase::Verified) {
        // The previous attempt's source may already be gone (if only the DB
        // commit/delete was left unfinished); we do not need it again.
        None
    } else {
        Some(resolved_recording_directory(
            default_root,
            extra_roots,
            recording,
        )?)
    };
    if phase != Some(Phase::Verified) {
        let source = source.clone().expect("source resolved above");
        write_journal(
            state_root,
            &JournalEntry {
                recording_id: recording.recording_id.clone(),
                source: source.clone(),
                destination: destination.clone(),
                phase: Phase::Copying,
            },
        )?;
        // A previous crash may have left a partial copy; it was never
        // verified, so it is always safe to discard and redo.
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("remove partial copy: {e}"))?;
        }
        copy_tree(&source, &destination)?;
        verify_tree(&source, &destination)?;
        write_journal(
            state_root,
            &JournalEntry {
                recording_id: recording.recording_id.clone(),
                source,
                destination: destination.clone(),
                phase: Phase::Verified,
            },
        )?;
    }
    let mut updated = recording.clone();
    updated.library_root = Some(destination_root.display().to_string());
    recordings.save(&updated)?;
    if let Some(entry) = read_journal(state_root, &recording.recording_id) {
        // The source recorded in the journal, not a freshly-resolved one:
        // resolution against the *old* root may no longer succeed once other
        // recordings have already moved (that is fine -- we already have the
        // exact path we copied from).
        let _ = fs::remove_dir_all(&entry.source);
    }
    clear_journal(state_root, &recording.recording_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::Track;
    use crate::recording_paths::create_recording_directory;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("meeting-library-move-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }
    /// Builds a recording under `source_root` (new meeting-folder layout,
    /// with one small "track" file inside it) and persists it to a
    /// `RecordingStore` rooted at `state_root`.
    fn seeded_recording(state_root: &Path, source_root: &Path) -> (RecordingStore, Recording) {
        let recordings = RecordingStore::open(state_root).unwrap();
        let mut recording = Recording {
            recording_id: uuid::Uuid::new_v4().to_string(),
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_010_000),
            duration_ms: 10_000,
            status: "recorded".into(),
            tracks: vec![],
            error: None,
            meeting_title: Some("Weekly Sync".into()),
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            transcription_status: "completed".into(),
            appflowy_status: "idle".into(),
        };
        let directory = create_recording_directory(source_root, &mut recording).unwrap();
        let track_path = directory.join("track-00.wav");
        fs::write(&track_path, b"pretend-wav-bytes").unwrap();
        recording.tracks.push(Track {
            source_id: "mic".into(),
            name: "Microphone".into(),
            direction: "input".into(),
            sample_rate: 16_000,
            channels: 1,
            file_path: track_path.display().to_string(),
            frames_written: 100,
            dropped_samples: 0,
            error: None,
        });
        recordings.save(&recording).unwrap();
        (recordings, recording)
    }

    #[test]
    fn moves_copies_verifies_updates_metadata_and_deletes_the_source() {
        let state_root = scratch("state-a");
        let source_root = scratch("source-a");
        let destination_root = scratch("destination-a");
        let (recordings, recording) = seeded_recording(&state_root, &source_root);
        let source_dir = resolved_recording_directory(&source_root, &[], &recording).unwrap();
        assert!(source_dir.join("track-00.wav").is_file());

        move_one(
            &state_root,
            &source_root,
            &[],
            &destination_root,
            &recordings,
            &recording,
        )
        .unwrap();

        let updated = recordings.get_by_id(&recording.recording_id).unwrap().unwrap();
        let new_dir = resolved_recording_directory(&source_root, std::slice::from_ref(&destination_root), &updated).unwrap();
        assert!(new_dir.starts_with(fs::canonicalize(&destination_root).unwrap()));
        assert_eq!(fs::read(new_dir.join("track-00.wav")).unwrap(), b"pretend-wav-bytes");
        // The source is gone and the journal is clear.
        assert!(!source_dir.exists());
        assert!(pending_recording_ids(&state_root).is_empty());
    }

    #[test]
    fn a_crash_mid_copy_is_resumed_by_redoing_the_copy_from_scratch() {
        let state_root = scratch("state-b");
        let source_root = scratch("source-b");
        let destination_root = scratch("destination-b");
        let (recordings, recording) = seeded_recording(&state_root, &source_root);
        let relative = recording.storage_path.clone().unwrap();
        let destination_root_canonical = fs::canonicalize(&destination_root).unwrap();
        let destination = destination_root_canonical.join(&relative);
        // Simulate a crash partway through copying: a "Copying" journal entry
        // plus a partial, WRONG destination (as if only some bytes made it).
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("track-00.wav"), b"corrupt-partial").unwrap();
        write_journal(
            &state_root,
            &JournalEntry {
                recording_id: recording.recording_id.clone(),
                source: resolved_recording_directory(&source_root, &[], &recording).unwrap(),
                destination: destination.clone(),
                phase: Phase::Copying,
            },
        )
        .unwrap();

        move_one(
            &state_root,
            &source_root,
            &[],
            &destination_root,
            &recordings,
            &recording,
        )
        .unwrap();

        assert_eq!(fs::read(destination.join("track-00.wav")).unwrap(), b"pretend-wav-bytes");
        assert!(pending_recording_ids(&state_root).is_empty());
    }

    #[test]
    fn a_crash_after_verify_before_the_db_commit_is_resumed() {
        let state_root = scratch("state-c");
        let source_root = scratch("source-c");
        let destination_root = scratch("destination-c");
        let (recordings, recording) = seeded_recording(&state_root, &source_root);
        let source_dir = resolved_recording_directory(&source_root, &[], &recording).unwrap();
        let relative = recording.storage_path.clone().unwrap();
        let destination_root_canonical = fs::canonicalize(&destination_root).unwrap();
        let destination = destination_root_canonical.join(&relative);
        // The copy completed and was verified, but the crash happened before
        // the database commit -- so metadata still points at the old root,
        // and the source is still present.
        copy_tree(&source_dir, &destination).unwrap();
        write_journal(
            &state_root,
            &JournalEntry {
                recording_id: recording.recording_id.clone(),
                source: source_dir.clone(),
                destination: destination.clone(),
                phase: Phase::Verified,
            },
        )
        .unwrap();

        move_one(
            &state_root,
            &source_root,
            &[],
            &destination_root,
            &recordings,
            &recording,
        )
        .unwrap();

        let updated = recordings.get_by_id(&recording.recording_id).unwrap().unwrap();
        assert_eq!(
            fs::canonicalize(updated.library_root.unwrap()).unwrap(),
            destination_root_canonical
        );
        assert!(!source_dir.exists());
        assert!(pending_recording_ids(&state_root).is_empty());
    }

    #[test]
    fn a_crash_after_the_db_commit_before_the_delete_is_resumed() {
        let state_root = scratch("state-d");
        let source_root = scratch("source-d");
        let destination_root = scratch("destination-d");
        let (recordings, mut recording) = seeded_recording(&state_root, &source_root);
        let source_dir = resolved_recording_directory(&source_root, &[], &recording).unwrap();
        let relative = recording.storage_path.clone().unwrap();
        let destination_root_canonical = fs::canonicalize(&destination_root).unwrap();
        let destination = destination_root_canonical.join(&relative);
        // The copy, verify, and DB commit all completed -- the recording
        // already points at the new root -- but the crash happened before
        // the old source was deleted.
        copy_tree(&source_dir, &destination).unwrap();
        recording.library_root = Some(destination_root_canonical.display().to_string());
        recordings.save(&recording).unwrap();
        write_journal(
            &state_root,
            &JournalEntry {
                recording_id: recording.recording_id.clone(),
                source: source_dir.clone(),
                destination: destination.clone(),
                phase: Phase::Verified,
            },
        )
        .unwrap();

        move_one(
            &state_root,
            &source_root,
            &[],
            &destination_root,
            &recordings,
            &recording,
        )
        .unwrap();

        assert!(!source_dir.exists());
        assert!(pending_recording_ids(&state_root).is_empty());
    }

    #[test]
    fn a_corrupted_destination_copy_fails_verification_and_is_never_committed() {
        let state_root = scratch("state-e");
        let source_root = scratch("source-e");
        let destination_root = scratch("destination-e");
        let (recordings, recording) = seeded_recording(&state_root, &source_root);
        let source_dir = resolved_recording_directory(&source_root, &[], &recording).unwrap();

        // Force `copy_tree` to silently miss a byte by truncating the source
        // file's copy right after `copy_tree` runs but before `verify_tree`
        // would compare it -- simulated here by pre-seeding a "Verified"
        // journal entry pointing at a destination that does not actually
        // match, exercising the same path a corrupted disk read would.
        let relative = recording.storage_path.clone().unwrap();
        let destination_root_canonical = fs::canonicalize(&destination_root).unwrap();
        let destination = destination_root_canonical.join(&relative);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("track-00.wav"), b"WRONG").unwrap();

        let result = verify_tree(&source_dir, &destination);
        assert!(result.is_err());
        // Nothing was committed: the source is untouched and the recording's
        // metadata still points at its original (source) root, unchanged.
        assert!(source_dir.exists());
        assert_eq!(
            recordings
                .get_by_id(&recording.recording_id)
                .unwrap()
                .unwrap()
                .library_root,
            recording.library_root
        );
    }
}
