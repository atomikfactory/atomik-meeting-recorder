//! "Delete from disk" -- the destructive counterpart to the existing
//! `delete_recording` command (which only tombstones a recording out of the
//! list; its audio and folder stay on disk untouched). This module removes
//! the recording's whole meeting folder (moved to the OS Recycle Bin/Trash by
//! default, or permanently deleted) and its rows from every local store.
//!
//! It never calls AppFlowy and never removes the `appflowy_pages` journal: a
//! page already exported to AppFlowy stays exactly where it is; only the
//! local `appflowy_jobs` row (this app's own export bookkeeping) is removed.
//!
//! Each recording id is handled independently -- one being skipped (still
//! recording, a job actively running, an unresolvable folder) never blocks
//! any other id in the same batch from being deleted.

use crate::{
    appflowy_store::AppFlowyStore, audio_compress_store::CompressionStore,
    diarization_store::DiarizationStore, recording::Recording,
    recording_paths::resolved_recording_directory, recording_store::RecordingStore,
    summary_store::SummaryStore, transcription_store::TranscriptionStore,
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    RecycleBin,
    Permanent,
}
impl DeleteMode {
    /// Accepts the frontend's `"recycle_bin"` (also the default for an empty
    /// string) or `"permanent"`; anything else is rejected outright rather
    /// than silently defaulting, since this chooses between recoverable and
    /// irreversible deletion.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "recycle_bin" | "" => Ok(DeleteMode::RecycleBin),
            "permanent" => Ok(DeleteMode::Permanent),
            other => Err(format!("Unknown delete mode: {other}")),
        }
    }
}

/// Injectable so tests never touch a real OS Recycle Bin/Trash.
pub trait TrashBackend {
    fn send_to_trash(&self, path: &Path) -> Result<(), String>;
}
pub struct OsTrash;
impl TrashBackend for OsTrash {
    fn send_to_trash(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|e| format!("move to recycle bin: {e}"))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DeleteOutcome {
    pub recording_id: String,
    pub deleted: bool,
    pub skipped_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DeleteSummary {
    pub results: Vec<DeleteOutcome>,
}

/// Every store a recording's rows must be removed from. All borrowed: the
/// caller (the Tauri command) owns them and additionally removes the search
/// index row and refreshes the recording service's history afterward, since
/// those two are not plain SQLite stores this module can borrow generically.
pub struct DeleteStores<'a> {
    pub recordings: &'a RecordingStore,
    pub transcription: &'a TranscriptionStore,
    pub diarization: &'a DiarizationStore,
    pub summary: &'a SummaryStore,
    pub compression: &'a CompressionStore,
    pub appflowy: &'a AppFlowyStore,
    pub speaker_names: &'a crate::speaker_names::SpeakerNameStore,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn skip(id: &str, reason: impl Into<String>) -> DeleteOutcome {
    DeleteOutcome {
        recording_id: id.to_string(),
        deleted: false,
        skipped_reason: Some(reason.into()),
    }
}

/// Refuses a recording whose transcription/diarization/summary/compression/
/// AppFlowy-export job is actively running right now. A job that is merely
/// queued, completed, or failed does not block deletion.
fn active_job_reason(id: &str, stores: &DeleteStores) -> Result<(), String> {
    if stores
        .transcription
        .get(id)
        .map_err(|e| format!("Could not read transcription status: {e}"))?
        .is_some_and(|job| job.status == "transcribing")
    {
        return Err("Transcription is in progress.".into());
    }
    if stores
        .diarization
        .get(id)
        .map_err(|e| format!("Could not read speaker identification status: {e}"))?
        .is_some_and(|job| job.status == "diarizing")
    {
        return Err("Speaker identification is in progress.".into());
    }
    if stores
        .summary
        .get(id)
        .map_err(|e| format!("Could not read summary status: {e}"))?
        .is_some_and(|job| job.status == "summarizing")
    {
        return Err("Summarizing is in progress.".into());
    }
    if stores
        .compression
        .get(id)
        .map_err(|e| format!("Could not read compression status: {e}"))?
        .is_some_and(|job| job.status == "compressing")
    {
        return Err("Audio compression is in progress.".into());
    }
    if stores
        .appflowy
        .get_job(id)
        .map_err(|e| format!("Could not read AppFlowy export status: {e}"))?
        .is_some_and(|job| job.status == "exporting")
    {
        return Err("An AppFlowy export is in progress.".into());
    }
    Ok(())
}

fn delete_one(
    id: &str,
    mode: DeleteMode,
    default_root: &Path,
    extra_roots: &[PathBuf],
    stores: &DeleteStores,
    active_recording_id: Option<&str>,
    trash: &dyn TrashBackend,
) -> DeleteOutcome {
    if active_recording_id == Some(id) {
        return skip(id, "This recording is currently being captured.");
    }
    let recording: Recording = match stores.recordings.get_by_id(id) {
        Ok(Some(r)) => r,
        Ok(None) => return skip(id, "Recording was not found."),
        Err(e) => return skip(id, format!("Could not read recording metadata: {e}")),
    };
    if matches!(recording.status.as_str(), "recording" | "processing") {
        return skip(id, "This recording is currently being captured.");
    }
    if let Err(reason) = active_job_reason(id, stores) {
        return skip(id, reason);
    }
    // Resolves through the same known-roots validation used everywhere else a
    // recording's folder is touched: canonicalized, symlink/junction-checked,
    // and confirmed to be a direct child of a recognized recordings library.
    // A recording whose folder cannot be safely resolved this way is left
    // completely alone rather than guessed at.
    let folder = match resolved_recording_directory(default_root, extra_roots, &recording) {
        Ok(dir) => dir,
        Err(e) => return skip(id, format!("Could not locate this recording's folder safely: {e}")),
    };

    // Tombstone first (the same crash-safety ordering as `delete_recording`):
    // once this succeeds, no worker will claim or expose this recording
    // again, so a crash mid-deletion below leaves at worst a dangling folder
    // (harmless -- nothing still points at it) rather than a row pointing at
    // audio that may already be gone. Startup recovery only ever repairs
    // *unfinished* recordings, so a tombstoned row is never resurrected.
    let now = now_ms();
    if let Err(e) = stores.recordings.tombstone(id, now) {
        return skip(id, format!("Could not mark this recording for deletion: {e}"));
    }

    let remove_result = match mode {
        DeleteMode::RecycleBin => trash.send_to_trash(&folder),
        DeleteMode::Permanent => {
            std::fs::remove_dir_all(&folder).map_err(|e| format!("permanently delete folder: {e}"))
        }
    };
    if let Err(e) = remove_result {
        // The tombstone already hid the recording; leaving the DB rows in
        // place for the other stores means a future run can still see the
        // job history if the folder removal is retried. Report the failure
        // rather than silently pretending success.
        return skip(id, format!("Could not delete this recording's files: {e}"));
    }

    // Best-effort: a row here is idle by construction (the active-job check
    // above already refused anything actively running), but each store
    // independently re-checks before deleting its own row, so a job that
    // started between the check and here is still refused rather than
    // silently dropped -- it just means this recording's folder is already
    // gone while its job history briefly remains, resolved by simply retrying
    // the delete once the job finishes.
    let mut leftovers = Vec::new();
    if let Err(e) = stores.transcription.remove_if_idle(id) {
        leftovers.push(format!("transcription: {e}"));
    }
    if let Err(e) = stores.diarization.remove_if_idle(id) {
        leftovers.push(format!("speaker identification: {e}"));
    }
    if let Err(e) = stores.summary.remove_if_idle(id) {
        leftovers.push(format!("summary: {e}"));
    }
    if let Err(e) = stores.compression.remove_if_idle(id) {
        leftovers.push(format!("compression: {e}"));
    }
    if let Err(e) = stores.appflowy.remove_job_if_idle(id) {
        leftovers.push(format!("AppFlowy export: {e}"));
    }
    // Best-effort: per-recording speaker names are pure presentation
    // metadata with no job state of its own, so it is simply cleared rather
    // than gated on an "idle" check like the pipeline stores above.
    let _ = stores.speaker_names.delete_recording(id);
    DeleteOutcome {
        recording_id: id.to_string(),
        deleted: true,
        skipped_reason: (!leftovers.is_empty()).then(|| {
            format!(
                "Files deleted; some job history could not be cleared yet: {}",
                leftovers.join("; ")
            )
        }),
    }
}

/// Deletes each requested recording's folder from disk and its rows from
/// every local store. `active_recording_id` is the currently
/// recording/processing recording, if any (refused outright, never
/// deleted). Never touches the AppFlowy `appflowy_pages` journal and never
/// calls AppFlowy itself.
pub fn delete_recordings_from_disk(
    recording_ids: &[String],
    mode: DeleteMode,
    default_root: &Path,
    extra_roots: &[PathBuf],
    stores: &DeleteStores,
    active_recording_id: Option<&str>,
    trash: &dyn TrashBackend,
) -> DeleteSummary {
    let results = recording_ids
        .iter()
        .map(|id| delete_one(id, mode, default_root, extra_roots, stores, active_recording_id, trash))
        .collect();
    DeleteSummary { results }
}

#[cfg(test)]
#[path = "recording_delete_tests.rs"]
mod tests;
