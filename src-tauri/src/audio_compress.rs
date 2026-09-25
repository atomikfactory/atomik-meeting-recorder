//! Optional, durable lossless compression of a finished recording's WAV tracks
//! to FLAC. Mirrors `summary.rs`'s shape: a SQLite job journal, a serial
//! background worker sharing `transcription::processing_guard` with
//! Whisper/diarization so encoding never competes with them for CPU, and
//! crash-safe track swaps.
//!
//! # Safety argument
//!
//! For each track this never touches the original WAV in a way that could
//! lose audio:
//!
//! 1. Encode PCM16 read from the WAV to a brand-new temp file in the same
//!    directory (`audio_file::encode_flac_new`, which refuses to overwrite
//!    anything and `fsync`s before returning).
//! 2. Decode that temp file back and compare every sample against the
//!    original WAV's samples for exact equality (`audio_file::read_flac_pcm16`).
//!    Any mismatch, or any error at any step so far, deletes the temp file
//!    and reports a job error; the WAV is never touched.
//! 3. Only once verified: atomically rename the temp file onto its final
//!    `.flac` name (same directory/filesystem, so this is a single directory
//!    entry update, not a copy).
//! 4. Update the recording's metadata row for that track's `file_path` in one
//!    SQLite transaction (`RecordingStore::save`, already transactional).
//! 5. Only now delete the original WAV.
//!
//! A crash at any point leaves exactly one of two consistent states: the
//! metadata still names the WAV (steps 3/4 not yet visible -- the `.flac`,
//! if present on disk, is already fully verified content, and a leftover
//! `.tmp-*` file is a pure orphan), or the metadata already names the `.flac`
//! (step 5 just needs to finish deleting the now-redundant WAV). `recover()`
//! below restores either case to steady state on startup, and never deletes
//! a WAV unless a verified `.flac` is (or becomes) the one the metadata
//! names.
use crate::{
    audio_compress_store::CompressionStore,
    audio_file,
    diarization_store::DiarizationStore,
    recording::Recording,
    recording_paths::resolved_recording_directory,
    recording_store::RecordingStore,
    transcription::processing_guard,
    transcription_store::TranscriptionStore,
};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionSettings {
    pub enabled: bool,
    /// Extra known library roots (the currently configured recordings folder
    /// and any it replaced) so a recording created under a custom folder can
    /// still be located; see `recording_paths::resolved_recording_directory`.
    pub extra_library_roots: Vec<PathBuf>,
}
type SettingsReader = Arc<dyn Fn() -> Result<CompressionSettings, String> + Send + Sync>;

#[derive(Serialize)]
pub struct JobSummary {
    pub recording_id: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
}
#[derive(Serialize)]
pub struct CompressionState {
    pub enabled: bool,
    pub inhibited: bool,
    pub jobs: Vec<JobSummary>,
    pub error: Option<String>,
}

pub struct CompressionService {
    stores: Option<(Arc<CompressionStore>, Arc<RecordingStore>)>,
    settings: SettingsReader,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
}
impl CompressionService {
    pub fn new(root: PathBuf, settings: SettingsReader) -> Self {
        let inhibited = [
            "MEETING_RECORDER_DISABLE_TRANSCRIPTION",
            "MEETING_RECORDER_DISABLE_COMPRESSION",
        ]
        .iter()
        .any(|key| std::env::var(key).ok().as_deref() == Some("1"));
        Self::with_inhibition(root, settings, inhibited)
    }
    pub(crate) fn with_inhibition(
        root: PathBuf,
        settings: SettingsReader,
        inhibited: bool,
    ) -> Self {
        let error = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let stores = match CompressionStore::open(&root.join("compression.sqlite3"))
            .and_then(|s| RecordingStore::open(&root).map(|r| (Arc::new(s), Arc::new(r))))
        {
            Ok(stores) => Some(stores),
            Err(e) => {
                set_error(&error, Some(e));
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some((s, r)) = stores.clone() {
                let (root, settings_reader, c, e) =
                    (root.clone(), settings.clone(), cancel.clone(), error.clone());
                match thread::Builder::new().name("audio-compress".into()).spawn(move || {
                    lower_thread_priority();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let startup_extra_roots = settings_reader()
                            .map(|s| s.extra_library_roots)
                            .unwrap_or_default();
                        if let Err(err) = processing_guard()
                            .lock()
                            .map_err(|_| "Processing guard failed.".to_string())
                            .and_then(|_guard| s.recover())
                            .and_then(|()| recover_tracks(&root, &startup_extra_roots, &r))
                        {
                            set_error(&e, Some(err));
                            return;
                        }
                        while !c.load(Ordering::Acquire) {
                            let result = settings_reader().and_then(|settings| {
                                if settings.enabled {
                                    let _guard = processing_guard()
                                        .lock()
                                        .map_err(|_| "Processing guard failed.".to_string())?;
                                    tick(&root, &settings.extra_library_roots, &s, &r, &c)
                                } else {
                                    Ok(())
                                }
                            });
                            set_error(&e, result.err());
                            for _ in 0..10 {
                                if c.load(Ordering::Acquire) {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }));
                    if result.is_err() {
                        set_error(
                            &e,
                            Some("Audio compression worker stopped unexpectedly. Restart to recover its job.".into()),
                        );
                    }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start audio compression worker: {e}"))),
                }
            }
        }
        Self {
            stores,
            settings,
            cancel,
            worker: Mutex::new(worker),
            error,
            inhibited,
        }
    }
    fn stores(&self) -> Result<(&CompressionStore, &RecordingStore), String> {
        self.stores
            .as_ref()
            .map(|(s, r)| (s.as_ref(), r.as_ref()))
            .ok_or_else(|| "Compression storage is unavailable.".into())
    }
    pub fn state(&self) -> CompressionState {
        let mut error = self.error.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let enabled = (self.settings)().map(|s| s.enabled).unwrap_or_else(|e| {
            error = Some(e);
            false
        });
        let mut jobs = Vec::new();
        match self.stores().and_then(|(s, _)| s.all()) {
            Ok(saved) => {
                jobs = saved
                    .into_iter()
                    .map(|j| JobSummary {
                        recording_id: j.recording_id,
                        status: j.status,
                        progress: j.progress,
                        error: j.error,
                        bytes_before: j.bytes_before,
                        bytes_after: j.bytes_after,
                    })
                    .collect()
            }
            Err(e) => error = Some(e),
        }
        CompressionState {
            enabled,
            inhibited: self.inhibited,
            jobs,
            error,
        }
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Audio compression is disabled in this session.".into());
        }
        self.stores()?.0.retry(id)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self
            .worker
            .lock()
            .map_err(|_| "Audio compression worker lock failed.")?
            .take()
        {
            handle
                .join()
                .map_err(|_| "Audio compression worker could not shut down.")?;
        }
        Ok(())
    }
}
fn set_error(slot: &Mutex<Option<String>>, value: Option<String>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = value;
}

/// Best-effort: lets the OS scheduler favor Whisper/diarization (and the UI)
/// over this background encode/verify work when they compete for CPU. Not
/// load-bearing -- `processing_guard` already prevents them from running at
/// the same time -- but this also helps the encode/verify work stay out of
/// the way of anything else on the machine while a recording is active.
#[cfg(windows)]
fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}
#[cfg(not(windows))]
fn lower_thread_priority() {}

fn transcription_settled(root: &Path, id: &str) -> Result<bool, String> {
    let store = TranscriptionStore::open(&root.join("transcription.sqlite3"))?;
    Ok(store
        .get(id)?
        .is_some_and(|job| matches!(job.status.as_str(), "completed" | "failed")))
}
fn diarization_settled(root: &Path, id: &str) -> Result<bool, String> {
    let store = DiarizationStore::open(&root.join("diarization.sqlite3"))?;
    Ok(match store.get(id)? {
        None => true,
        Some(job) => matches!(job.status.as_str(), "completed" | "failed"),
    })
}

fn tick(
    root: &Path,
    extra_roots: &[PathBuf],
    store: &CompressionStore,
    recordings: &RecordingStore,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for recording in recordings.finalized_eligible()? {
        if transcription_settled(root, &recording.recording_id)?
            && diarization_settled(root, &recording.recording_id)?
        {
            store.enqueue(&recording.recording_id)?;
        }
    }
    let Some(job) = store.next()? else {
        return Ok(());
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let Some(recording) = recordings.get_by_id(&job.recording_id)? else {
        store.remove_if_idle(&job.recording_id)?;
        return Ok(());
    };
    if !store.claim(&job.recording_id)? {
        return Ok(());
    }
    let result = compress_recording(root, extra_roots, recording, recordings, cancel);
    match result {
        Ok((bytes_before, bytes_after)) => {
            store.complete(&job.recording_id, bytes_before, bytes_after)?;
        }
        Err(e) => {
            store.status(
                &job.recording_id,
                if cancel.load(Ordering::Acquire) {
                    "queued"
                } else {
                    "failed"
                },
                0,
                Some(&e),
            )?;
        }
    }
    Ok(())
}

/// Compresses every WAV track of `recording` that is not already FLAC,
/// skipping ones that are (so a resumed job after a crash is idempotent).
/// Returns the recording's total original and final audio bytes, computed
/// from durable metadata (`frames_written`) and current on-disk sizes rather
/// than anything that could have been lost mid-job.
fn compress_recording(
    root: &Path,
    extra_roots: &[PathBuf],
    mut recording: Recording,
    recordings: &RecordingStore,
    cancel: &AtomicBool,
) -> Result<(u64, u64), String> {
    let directory = resolved_recording_directory(root, extra_roots, &recording)?;
    for index in 0..recording.tracks.len() {
        if cancel.load(Ordering::Acquire) {
            return Err("Compression cancelled; no result was applied.".into());
        }
        let path = PathBuf::from(&recording.tracks[index].file_path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "A recording track is unavailable.".to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("A recording track must not be a symbolic link.".into());
        }
        let canonical = fs::canonicalize(&path)
            .map_err(|_| "A recording track is unavailable.".to_string())?;
        if canonical.parent() != Some(directory.as_path()) {
            return Err("A recording track is outside its recording directory.".into());
        }
        if audio_file::is_flac_path(&canonical) {
            continue;
        }
        let flac_path = canonical.with_extension("flac");
        if flac_path.exists() {
            return Err(
                "A FLAC file already exists for this track without updated metadata; files preserved."
                    .into(),
            );
        }
        let (info, samples) = audio_file::read_wav_pcm16(&canonical)?;
        let track = &recording.tracks[index];
        if info.channels != track.channels || info.sample_rate != track.sample_rate {
            return Err("Track audio format does not match its recorded metadata.".into());
        }
        // FLAC's format minimum block size (32 samples) is shorter than any
        // real meeting recording, but keeps genuinely tiny/degenerate tracks
        // (e.g. a source that opened and immediately failed) on WAV rather
        // than forcing a workaround that would compromise bit-exactness.
        let frames = samples.len() / usize::from(info.channels.max(1));
        if frames < 32 {
            continue;
        }
        let tmp_path = canonical.with_extension(format!("flac.tmp-{}", Uuid::new_v4()));
        if let Err(e) = audio_file::encode_flac_new(&tmp_path, info.sample_rate, info.channels, &samples) {
            let _ = audio_file::remove_orphan(&tmp_path);
            return Err(e);
        }
        match audio_file::read_flac_pcm16(&tmp_path) {
            Ok((decoded_info, decoded_samples))
                if decoded_samples == samples
                    && decoded_info.sample_rate == info.sample_rate
                    && decoded_info.channels == info.channels =>
            {
                // Verified bit-exact; fall through to the atomic rename below.
            }
            Ok(_) => {
                let _ = audio_file::remove_orphan(&tmp_path);
                return Err(
                    "FLAC verification did not exactly match the original audio; the WAV was kept."
                        .into(),
                );
            }
            Err(e) => {
                let _ = audio_file::remove_orphan(&tmp_path);
                return Err(format!("FLAC verification failed; the WAV was kept: {e}"));
            }
        }
        fs::rename(&tmp_path, &flac_path)
            .map_err(|e| format!("Could not finalize the compressed track: {e}"))?;
        recording.tracks[index].file_path = flac_path.to_string_lossy().into_owned();
        recordings.save(&recording)?;
        // Best-effort: if this fails or the process crashes here, `recover()`
        // finishes the delete on the next startup (metadata already points
        // at the verified FLAC, so the WAV is strictly redundant).
        let _ = fs::remove_file(&canonical);
    }
    let bytes_before: u64 = recording
        .tracks
        .iter()
        .map(|t| {
            t.frames_written
                .saturating_mul(u64::from(t.channels))
                .saturating_mul(2)
        })
        .sum();
    let bytes_after: u64 = recording
        .tracks
        .iter()
        .filter_map(|t| fs::metadata(&t.file_path).ok())
        .map(|m| m.len())
        .sum();
    Ok((bytes_before, bytes_after))
}

/// Startup recovery: removes orphaned encode temp files, and finishes any
/// track swap that a crash interrupted between the verified rename and the
/// WAV delete (see the module doc comment for why this is always safe).
pub(crate) fn recover_tracks(
    root: &Path,
    extra_roots: &[PathBuf],
    recordings: &RecordingStore,
) -> Result<(), String> {
    for mut recording in recordings.finalized_eligible()? {
        let Ok(directory) = resolved_recording_directory(root, extra_roots, &recording) else {
            continue;
        };
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().contains(".flac.tmp-") {
                    let _ = audio_file::remove_orphan(&entry.path());
                }
            }
        }
        let mut changed = false;
        for track in recording.tracks.iter_mut() {
            let named = PathBuf::from(&track.file_path);
            let Some(stem_path) = named.file_stem().map(|stem| directory.join(stem)) else {
                continue;
            };
            let wav_path = stem_path.with_extension("wav");
            let flac_path = stem_path.with_extension("flac");
            let wav_exists = wav_path.is_file();
            let flac_exists = flac_path.is_file();
            let named_is_flac = audio_file::is_flac_path(&named);
            match (named_is_flac, wav_exists, flac_exists) {
                (false, true, true) => {
                    // Verified rename already happened; metadata update and/or
                    // WAV delete did not. The FLAC is trusted (see module doc).
                    track.file_path = flac_path.to_string_lossy().into_owned();
                    changed = true;
                    let _ = fs::remove_file(&wav_path);
                }
                (true, true, true) => {
                    // Metadata already points at the FLAC; only the delete
                    // was interrupted.
                    let _ = fs::remove_file(&wav_path);
                }
                (true, false, true) => {} // steady state after compression
                (true, true, false) => {
                    // The FLAC referenced by metadata is missing but the WAV
                    // survives: fall back to the WAV rather than lose audio.
                    track.file_path = wav_path.to_string_lossy().into_owned();
                    changed = true;
                }
                _ => {} // (false, true, false): steady state before compression;
                        // (false/_, false, false): nothing recoverable here --
                        // left for the existing missing-track error paths.
            }
        }
        if changed {
            recordings.save(&recording)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "audio_compress_tests.rs"]
mod tests;
