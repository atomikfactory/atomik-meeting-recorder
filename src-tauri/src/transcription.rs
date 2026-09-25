//! Durable local transcription, independent of the capture coordinator.
use crate::{
    diarization_store::DiarizationStore,
    recording::Recording,
    recording_paths::resolved_recording_directory,
    recording_store::RecordingStore,
    summary_store::SummaryStore,
    transcription_engine::{hash_model, inspect_model, TranscriptionEngine},
    transcription_store::{StoredJob, TranscriptionStore},
    transcription_types::{ModelInfo, Transcript, TranscriptSegment, TranscriptionRequest},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    thread,
    time::Duration,
};

/// The subset of app settings the transcription worker needs on each tick.
/// Read fresh at the start of every job (never pinned into a job row, unlike
/// the model itself) so a language change takes effect on the next run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptionSettings {
    pub model_key: String,
    /// "auto" or a lowercase ISO 639-1 code; see `store::Settings::transcription_language`.
    pub language: String,
    /// See `store::Settings::echo_cancellation`.
    pub echo_cancellation: bool,
    /// See `store::Settings::dedupe_echo_segments`.
    pub dedupe_echo_segments: bool,
    /// See `store::Settings::local_speaker_name`.
    pub local_speaker_name: String,
}
impl TranscriptionSettings {
    /// Convenience for tests and fixtures that only care about the model size.
    #[cfg(test)]
    pub fn for_model(model_key: impl Into<String>) -> Self {
        Self {
            model_key: model_key.into(),
            language: "auto".into(),
            echo_cancellation: true,
            dedupe_echo_segments: true,
            local_speaker_name: "You".into(),
        }
    }
}
type SettingsReader = Arc<dyn Fn() -> Result<TranscriptionSettings, String> + Send + Sync>;
static PROCESSING_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes mutable processing artifacts across the transcription and speaker workers.
/// Commands use `try_lock` so the UI is never stalled behind inference.
pub(crate) fn processing_guard() -> &'static Mutex<()> {
    PROCESSING_GUARD.get_or_init(|| Mutex::new(()))
}
pub(crate) fn try_processing_guard() -> Result<MutexGuard<'static, ()>, String> {
    processing_guard()
        .try_lock()
        .map_err(|_| "Processing is busy; wait for the current job to finish.".into())
}
#[derive(Clone, Serialize)]
pub struct ModelStatus {
    pub key: String,
    pub available: bool,
    pub path: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct JobSummary {
    pub recording_id: String,
    pub model_key: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub markdown_path: Option<String>,
    pub attempts: u32,
}
#[derive(Clone, Serialize)]
pub struct TranscriptionState {
    pub inhibited: bool,
    pub model: ModelStatus,
    pub jobs: Vec<JobSummary>,
    pub imported_models: Vec<String>,
    pub error: Option<String>,
    /// Human-readable transcription backend, e.g. "cpu (AVX2)" or "cuda".
    pub backend: String,
}
pub struct TranscriptionService {
    root: PathBuf,
    store: Option<Arc<TranscriptionStore>>,
    settings: SettingsReader,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
    engine: Arc<dyn TranscriptionEngine>,
}
impl TranscriptionService {
    pub fn new(
        root: PathBuf,
        settings: SettingsReader,
        engine: Arc<dyn TranscriptionEngine>,
    ) -> Self {
        let inhibited = std::env::var("MEETING_RECORDER_DISABLE_TRANSCRIPTION")
            .ok()
            .as_deref()
            == Some("1");
        Self::with_inhibition(root, settings, engine, inhibited)
    }
    pub(crate) fn with_inhibition(
        root: PathBuf,
        settings: SettingsReader,
        engine: Arc<dyn TranscriptionEngine>,
        inhibited: bool,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let store = match TranscriptionStore::open(&root.join("transcription.sqlite3")) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                set_error(
                    &error,
                    Some(format!("Transcription storage unavailable: {e}")),
                );
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some(store) = store.clone() {
                let (r, s, c, e, worker_engine) = (
                    root.clone(),
                    settings.clone(),
                    cancel.clone(),
                    error.clone(),
                    engine.clone(),
                );
                match thread::Builder::new().name("local-transcription".into()).spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_worker(r, store, s, worker_engine, c, e.clone())));
                    if result.is_err() { set_error(&e, Some("Transcription worker stopped unexpectedly. Restart to recover its job.".into())); }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start transcription worker: {e}"))),
                }
            }
        }
        Self {
            root,
            store,
            settings,
            cancel,
            worker: Mutex::new(worker),
            error,
            inhibited,
            engine,
        }
    }
    fn store(&self) -> Result<&TranscriptionStore, String> {
        self.store.as_deref().ok_or_else(|| {
            "Transcription storage is unavailable; local capture remains available.".into()
        })
    }
    pub fn state(&self) -> TranscriptionState {
        let mut error = self.error.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let key = match (self.settings)() {
            Ok(settings) => settings.model_key,
            Err(e) => {
                error = Some(e);
                String::new()
            }
        };
        let mut model = ModelStatus {
            key: key.clone(),
            available: false,
            path: None,
            error: None,
        };
        let mut jobs = Vec::new();
        let mut imported_models = Vec::new();
        if let Some(store) = &self.store {
            match store.model_keys() {
                Ok(keys) => imported_models = keys,
                Err(e) => error = Some(e),
            }
            match store.model(&key) {
                Ok(Some((path, hash, bytes))) => {
                    match managed_model(&self.root, &path, &hash, Some(bytes)) {
                        Ok(_) => {
                            model.available = true;
                            model.path = Some(path);
                        }
                        Err(e) => model.error = Some(e),
                    }
                }
                Ok(None) => {}
                Err(e) => error = Some(e),
            }
            match store.all() {
                Ok(saved) => match RecordingStore::open(&self.root) {
                    Ok(recordings) => {
                        jobs = saved
                            .into_iter()
                            .filter(|j| {
                                recordings
                                    .get_by_id(&j.recording_id)
                                    .ok()
                                    .flatten()
                                    .is_some()
                            })
                            .map(|j| JobSummary {
                                recording_id: j.recording_id,
                                model_key: j.model_key,
                                status: j.status,
                                progress: j.progress,
                                error: j.error,
                                markdown_path: j.markdown_path,
                                attempts: j.attempts,
                            })
                            .collect()
                    }
                    Err(e) => error = Some(e),
                },
                Err(e) => error = Some(e),
            }
        }
        TranscriptionState {
            inhibited: self.inhibited,
            model,
            jobs,
            imported_models,
            error,
            backend: self.engine.backend_label(),
        }
    }
    pub fn import_model(&self, source: PathBuf) -> Result<ModelInfo, String> {
        if self.cancel.load(Ordering::Acquire) {
            return Err("Transcription is shutting down.".into());
        }
        let store = self.store()?;
        reject_symlink(&source)?;
        let info = inspect_model(&source)?;
        let root = self.root.canonicalize().map_err(io_error)?;
        let models = root.join("models");
        reject_symlink(&models)?;
        fs::create_dir_all(&models).map_err(io_error)?;
        let target = models.join(format!("{}.bin", info.sha256));
        reject_symlink(&target)?;
        if !target.exists() {
            let tmp = models.join(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| {
                let input = File::open(&source).map_err(io_error)?;
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .map_err(io_error)?;
                let copied =
                    std::io::copy(&mut input.take(info.bytes + 1), &mut out).map_err(io_error)?;
                out.sync_all().map_err(io_error)?;
                drop(out);
                if copied != info.bytes || hash_model(&tmp)? != info.sha256 {
                    return Err(
                        "Model changed while being imported; select the original file again."
                            .into(),
                    );
                }
                fs::rename(&tmp, &target).map_err(io_error)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            result?;
        }
        let target = managed_model(
            &root,
            &target.display().to_string(),
            &info.sha256,
            Some(info.bytes),
        )?;
        if hash_model(&target)? != info.sha256 {
            return Err(
                "Stored model checksum mismatch; existing file was not overwritten.".into(),
            );
        }
        store.upsert_model(
            &info.key,
            &target.display().to_string(),
            &info.sha256,
            info.bytes,
        )?;
        store.requeue_waiting(&info.key)?;
        self.repin_to_current_model()?;
        Ok(info)
    }
    /// True when the given model size has already been imported.
    pub fn has_model(&self, key: &str) -> Result<bool, String> {
        Ok(self.store()?.model(key)?.is_some())
    }
    /// Re-points every never-claimed job at the currently selected model, if that
    /// model is available. Called after import and once per worker tick so a job
    /// enqueued under one size follows the setting until it is actually claimed.
    pub(crate) fn repin_to_current_model(&self) -> Result<(), String> {
        let store = self.store()?;
        let key = (self.settings)()?.model_key;
        if let Some((_, hash, _)) = store.model(&key)? {
            store.repin_unclaimed(&key, &hash)?;
        }
        Ok(())
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Transcription is disabled in this session.".into());
        }
        safe_id(id)?;
        self.store()?.retry(id)
    }
    /// Explicit user intent. Unlike the background scan this may opt a legacy
    /// recording into transcription, but only after validating its durable row.
    pub fn transcribe_recording(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Transcription is disabled in this session.".into());
        }
        safe_id(id)?;
        let key = (self.settings)()?.model_key;
        let recordings = RecordingStore::open(&self.root)?;
        let mut recording = recordings
            .get_by_id(id)?
            .ok_or("Recording metadata is unavailable.")?;
        if !matches!(
            recording.status.as_str(),
            "recorded" | "interrupted" | "failed"
        ) || recording.tracks.is_empty()
            || recording
                .tracks
                .iter()
                .all(|track| track.frames_written == 0)
        {
            return Err("Only finalized recordings with retained audio can be transcribed.".into());
        }
        let store = self.store()?;
        // Existing completed/pending jobs remain authoritative and keep pins.
        if let Some(job) = store.get(id)? {
            if matches!(
                job.status.as_str(),
                "completed" | "queued" | "transcribing" | "waiting_model"
            ) {
                return Ok(());
            }
            // A retry is explicit user intent. Ensure the selected legacy row remains
            // eligible before releasing its existing, pinned job back to the worker.
            if recording.status != "failed" && !recording.auto_transcribe {
                recording.auto_transcribe = true;
                recordings.save(&recording)?;
            }
            return if recording.status == "failed" {
                store.retry_manual(id)
            } else {
                store.retry(id)
            };
        }
        if recording.status != "failed" && !recording.auto_transcribe {
            recording.auto_transcribe = true;
            recordings.save(&recording)?;
        }
        let hash = store
            .model(&key)?
            .map(|(_, hash, _)| hash)
            .unwrap_or_default();
        if recording.status == "failed" {
            store.enqueue_manual(id, &key, &hash)
        } else {
            store.enqueue(id, &key, &hash)
        }
    }
    /// Explicitly replaces a dormant transcript with one using the currently selected
    /// model. The old artifacts are retained under a unique archive directory first.
    pub fn restart_transcription(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Transcription is disabled in this session.".into());
        }
        safe_id(id)?;
        let recordings = RecordingStore::open(&self.root)?;
        let recording = recordings
            .get_by_id(id)?
            .ok_or("Recording metadata is unavailable or was deleted.")?;
        if !matches!(
            recording.status.as_str(),
            "recorded" | "interrupted" | "failed"
        ) || recording
            .tracks
            .iter()
            .all(|track| track.frames_written == 0)
        {
            return Err("Only retained finalized audio can be transcribed.".into());
        }
        let store = self.store()?;
        let job = store
            .get(id)?
            .ok_or("There is no completed transcription to restart.")?;
        if !matches!(
            job.status.as_str(),
            "completed" | "failed" | "waiting_model"
        ) {
            return Err("Transcription is already queued or running; it was not restarted.".into());
        }
        let key = (self.settings)()?.model_key;
        let hash = store
            .model(&key)?
            .map(|(_, hash, _)| hash)
            .unwrap_or_default();
        let intent = store.begin_restart(id, &key, &hash)?;
        archive_outputs(
            &recording_directory(&self.root, id)?,
            id,
            &intent.archive_id,
        )?;
        DiarizationStore::open(&self.root.join("diarization.sqlite3"))?.remove_if_idle(id)?;
        SummaryStore::open(&self.root.join("summary.sqlite3"))?.remove_if_idle(id)?;
        store.finish_restart(id, &intent.model_key, &intent.model_hash)
    }
    pub fn transcript_path(&self, id: &str) -> Result<PathBuf, String> {
        let j = self.store()?.get(id)?.ok_or("Unknown transcription job.")?;
        if j.status != "completed" {
            return Err("Transcript is not complete.".into());
        }
        let directory = recording_directory(&self.root, id)?;
        let json = output_path(&directory, "transcript.json")?;
        let markdown = output_path(&directory, "transcript.md")?;
        if j.transcript_path.as_deref() != Some(json.to_string_lossy().as_ref())
            || j.markdown_path.as_deref() != Some(markdown.to_string_lossy().as_ref())
        {
            return Err("Stored transcript paths do not match this recording.".into());
        }
        let bytes = read_checkpoint(&json)?;
        if Some(checksum(&bytes)).as_deref() != j.checksum.as_deref() {
            return Err(
                "Transcript checkpoint checksum mismatch; preserved files were not overwritten."
                    .into(),
            );
        }
        let transcript: Transcript = serde_json::from_slice(&bytes).map_err(io_error)?;
        let recording = RecordingStore::open(&self.root)?
            .get_by_id(id)?
            .ok_or("Recording metadata is unavailable.")?;
        validate_transcript(&transcript, &j, &recording)?;
        if read_checkpoint(&markdown)? != markdown_document(&recording, &transcript).as_bytes() {
            return Err("Markdown differs from its saved transcript. It remains on disk; open the recordings folder to inspect it.".into());
        }
        Ok(markdown)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self
            .worker
            .lock()
            .map_err(|_| "Transcription worker lock failed.")?
            .take()
        {
            handle
                .join()
                .map_err(|_| "Transcription worker could not shut down.")?;
        }
        Ok(())
    }
}
fn set_error(slot: &Mutex<Option<String>>, value: Option<String>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = value;
}
fn io_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
pub(crate) fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn safe_id(id: &str) -> Result<(), String> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| "Invalid recording identity.")?;
    if uuid.to_string() != id {
        return Err("Recording identity must be a canonical UUID.".into());
    }
    Ok(())
}
pub(crate) fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err("Linked recording/model paths are not supported.".into())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}
pub(crate) fn recording_directory(root: &Path, id: &str) -> Result<PathBuf, String> {
    safe_id(id)?;
    let recording = RecordingStore::open(root)?
        .get_by_id(id)?
        .ok_or_else(|| "Recording was not found.".to_string())?;
    resolved_recording_directory(root, &recording)
}
pub(crate) fn output_path(directory: &Path, name: &str) -> Result<PathBuf, String> {
    let path = directory.join(name);
    reject_symlink(&path)?;
    if path.exists() && (!path.is_file() || path.canonicalize().map_err(io_error)? != path) {
        return Err("Invalid transcript output path.".into());
    }
    Ok(path)
}
fn archive_outputs(directory: &Path, id: &str, archive_id: &str) -> Result<(), String> {
    let parsed =
        uuid::Uuid::parse_str(archive_id).map_err(|_| "Restart archive identity is invalid.")?;
    if parsed.to_string() != archive_id {
        return Err("Restart archive identity is invalid.".into());
    }
    let archive_root = directory.join("transcript-archive");
    reject_symlink(&archive_root)?;
    fs::create_dir_all(&archive_root).map_err(io_error)?;
    let archive_root = archive_root.canonicalize().map_err(io_error)?;
    if archive_root.parent() != Some(directory) {
        return Err("Transcript archive is outside the recording directory.".into());
    }
    let archive = archive_root.join(format!("restart-{archive_id}"));
    if archive.exists() {
        reject_symlink(&archive)?;
        if !archive.is_dir() {
            return Err("Transcript archive path is invalid.".into());
        }
    } else {
        fs::create_dir(&archive).map_err(io_error)?;
    }
    let archive = archive.canonicalize().map_err(io_error)?;
    if archive.parent() != Some(archive_root.as_path()) {
        return Err("Transcript archive escaped its owned directory.".into());
    }
    let intent = archive.join("restart-intent.json");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "recording_id": id,
        "state": "archiving"
    }))
    .map_err(io_error)?;
    if !intent.exists() {
        atomic_new(&intent, &bytes)?;
    }
    for name in [
        "transcript.json",
        "transcript.md",
        "speakers.json",
        "transcript-speakers.md",
        "summary.json",
        "summary.md",
    ] {
        let source = output_path(directory, name)?;
        let destination = archive.join(name);
        reject_symlink(&destination)?;
        if source.exists() && destination.exists() {
            return Err(
                "Restart archive has conflicting output files; nothing was overwritten.".into(),
            );
        }
        if source.exists() {
            fs::rename(&source, &destination).map_err(io_error)?;
        }
    }
    let complete = archive.join("archive-complete.json");
    if !complete.exists() {
        atomic_new(
            &complete,
            &serde_json::to_vec(&serde_json::json!({"recording_id": id, "state": "archived"}))
                .map_err(io_error)?,
        )?;
    }
    Ok(())
}
fn recover_restart_intents(
    root: &Path,
    store: &TranscriptionStore,
    recordings: &RecordingStore,
) -> Result<(), String> {
    for intent in store.restart_intents()? {
        let Some(recording) = recordings.get_by_id(&intent.recording_id)? else {
            // A tombstone is terminal. Removing the idle journal row prevents a hidden
            // restart from being revived after a crash.
            store.remove_if_idle(&intent.recording_id)?;
            continue;
        };
        archive_outputs(
            &resolved_recording_directory(root, &recording)?,
            &intent.recording_id,
            &intent.archive_id,
        )?;
        DiarizationStore::open(&root.join("diarization.sqlite3"))?
            .remove_if_idle(&intent.recording_id)?;
        SummaryStore::open(&root.join("summary.sqlite3"))?.remove_if_idle(&intent.recording_id)?;
        store.finish_restart(&intent.recording_id, &intent.model_key, &intent.model_hash)?;
    }
    Ok(())
}
fn managed_model(
    root: &Path,
    path: &str,
    hash: &str,
    bytes: Option<u64>,
) -> Result<PathBuf, String> {
    if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid model checksum.".into());
    }
    let root = root.canonicalize().map_err(io_error)?;
    let models = root.join("models");
    reject_symlink(&models)?;
    let expected = models.join(format!("{hash}.bin"));
    reject_symlink(&expected)?;
    if Path::new(path) != expected {
        return Err("Model registry points outside its managed location.".into());
    }
    let metadata = fs::metadata(&expected)
        .map_err(|_| "Imported model is missing; import it again.".to_string())?;
    if !metadata.is_file() || bytes.is_some_and(|size| size != metadata.len()) {
        return Err("Imported model size changed; import it again.".into());
    }
    if expected.canonicalize().map_err(io_error)? != expected {
        return Err("Model path is redirected.".into());
    }
    Ok(expected)
}
fn run_worker(
    root: PathBuf,
    store: Arc<TranscriptionStore>,
    settings: SettingsReader,
    engine: Arc<dyn TranscriptionEngine>,
    cancel: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
) {
    let recordings = match processing_guard()
        .lock()
        .map_err(|_| "Processing guard failed.".to_string())
        .and_then(|_guard| store.recover())
        .and_then(|()| RecordingStore::open(&root))
    {
        Ok(recordings) => recordings,
        Err(e) => {
            set_error(&error, Some(e));
            return;
        }
    };
    while !cancel.load(Ordering::Acquire) {
        let result = processing_guard()
            .lock()
            .map_err(|_| "Processing guard failed.".to_string())
            .and_then(|_guard| {
                tick(
                    &root,
                    &store,
                    &recordings,
                    &settings,
                    engine.as_ref(),
                    &cancel,
                )
            });
        set_error(&error, result.err());
        for _ in 0..10 {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
fn tick(
    root: &Path,
    store: &TranscriptionStore,
    recordings: &RecordingStore,
    settings: &SettingsReader,
    engine: &dyn TranscriptionEngine,
    cancel: &AtomicBool,
) -> Result<(), String> {
    recover_restart_intents(root, store, recordings)?;
    let current_settings = settings()?;
    let key = current_settings.model_key;
    for recording in recordings.finalized_eligible()? {
        if recording.auto_transcribe {
            store.enqueue(&recording.recording_id, &key, "")?;
        }
    }
    // A job that was never claimed still tracks the currently selected model, so a
    // setting change (or an import that finally matches it) wakes it here too.
    if let Some((_, hash, _)) = store.model(&key)? {
        store.repin_unclaimed(&key, &hash)?;
    }
    let Some(mut job) = store.next()? else {
        return Ok(());
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    // A tombstone can survive a crash before its job rows are removed. Do not inspect
    // a model or a checkpoint for that recording; drop the dormant row instead.
    let recording = match recordings.get_by_id(&job.recording_id)? {
        Some(recording) => recording,
        None => {
            store.remove_if_idle(&job.recording_id)?;
            return Ok(());
        }
    };
    let model = if let Some(path) = &job.model_path {
        Some((path.clone(), job.model_hash.clone(), None))
    } else {
        store
            .model(&job.model_key)?
            .map(|(p, h, b)| (p, h, Some(b)))
    };
    let Some((model_path, hash, size)) = model else {
        store.status(
            &job.recording_id,
            "waiting_model",
            0,
            Some(&format!(
                "Whisper model '{}' is not imported. Import it or select an imported model in Settings.",
                job.model_key
            )),
        )?;
        return Ok(());
    };
    if !store.claim(&job.recording_id, &model_path, &hash)? {
        return Ok(());
    }
    job.model_path = Some(model_path.clone());
    job.model_hash = hash.clone();
    let result = (|| {
        let automatic = recording.auto_transcribe
            && matches!(recording.status.as_str(), "recorded" | "interrupted");
        let explicitly_requested = job.manual
            && matches!(
                recording.status.as_str(),
                "recorded" | "interrupted" | "failed"
            )
            && recording
                .tracks
                .iter()
                .any(|track| track.frames_written > 0);
        if !automatic && !explicitly_requested {
            return Err("This recording is not eligible for transcription. A failed capture must be explicitly requested and retain audio.".into());
        }
        let directory = recording_directory(root, &job.recording_id)?;
        let json = output_path(&directory, "transcript.json")?;
        let md = output_path(&directory, "transcript.md")?;
        // Recover a JSON rename that succeeded before the database commit.
        let (transcript, bytes) = if json.exists() {
            let bytes = read_checkpoint(&json)?;
            let transcript: Transcript = serde_json::from_slice(&bytes).map_err(io_error)?;
            validate_transcript(&transcript, &job, &recording)?;
            (transcript, bytes)
        } else {
            if md.exists() {
                return Err(
                    "Markdown exists without its checkpoint; preserved for inspection.".into(),
                );
            }
            let path = managed_model(root, &model_path, &hash, size)?;
            let request = TranscriptionRequest {
                recording: recording.clone(),
                recordings_root: root.to_owned(),
                model_path: path,
                model_key: job.model_key.clone(),
                model_hash: hash,
                // "auto" means per-track detection inside the engine, not a literal
                // language tag, so it is not forwarded to Whisper as one.
                language: (current_settings.language != "auto")
                    .then(|| current_settings.language.clone()),
                echo_cancellation: current_settings.echo_cancellation,
                dedupe_echo_segments: current_settings.dedupe_echo_segments,
                local_speaker_name: current_settings.local_speaker_name.clone(),
            };
            let mut progress_error = None;
            let mut progress = |value: u8| {
                if let Err(e) = store.status(&job.recording_id, "transcribing", value.min(99), None)
                {
                    progress_error = Some(e);
                }
            };
            let transcript = engine.transcribe(&request, cancel, &mut progress)?;
            if let Some(e) = progress_error {
                return Err(e);
            }
            validate_transcript(&transcript, &job, &recording)?;
            let bytes = serde_json::to_vec_pretty(&transcript).map_err(io_error)?;
            atomic_new(&json, &bytes)?;
            (transcript, bytes)
        };
        let markdown = markdown_document(&recording, &transcript);
        if md.exists() {
            if read_checkpoint(&md)? != markdown.as_bytes() {
                return Err(
                    "Existing Markdown differs from the checkpoint; preserved for inspection."
                        .into(),
                );
            }
        } else {
            atomic_new(&md, markdown.as_bytes())?;
        }
        store.complete(
            &job.recording_id,
            &json.display().to_string(),
            &md.display().to_string(),
            &checksum(&bytes),
        )
    })();
    if let Err(e) = result {
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
    Ok(())
}
pub(crate) fn read_checkpoint(path: &Path) -> Result<Vec<u8>, String> {
    reject_symlink(path)?;
    let file = File::open(path).map_err(io_error)?;
    const MAX: u64 = 64 * 1024 * 1024;
    if file.metadata().map_err(io_error)?.len() > MAX {
        return Err("Transcript checkpoint exceeds the size limit.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX {
        return Err("Transcript checkpoint exceeds the size limit.".into());
    }
    Ok(bytes)
}
pub(crate) fn atomic_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    reject_symlink(path)?;
    if path.exists() {
        return Err("Refusing to replace existing transcript output.".into());
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        fs::rename(&tmp, path).map_err(io_error)?;
        #[cfg(unix)]
        File::open(path.parent().ok_or("Missing output parent.")?)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
pub(crate) fn validate_transcript(
    t: &Transcript,
    job: &StoredJob,
    recording: &Recording,
) -> Result<(), String> {
    if t.recording_id != recording.recording_id
        || t.recording_id != job.recording_id
        || t.model_key != job.model_key
        || t.model_hash != job.model_hash
    {
        return Err("Transcript checkpoint identity or model does not match its job.".into());
    }
    for segment in &t.segments {
        if segment.end_ms < segment.start_ms
            || segment.end_ms > recording.duration_ms.saturating_add(30_000)
            || !recording.tracks.iter().any(|track| {
                track.source_id == segment.source_id && track.name == segment.source_name
            })
        {
            return Err(
                "Transcript checkpoint contains invalid source or timestamp metadata.".into(),
            );
        }
    }
    Ok(())
}
fn literal(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '!' | '#' | '+' | '-' | '.' | '|' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' | '\r' => escaped.push(' '),
            c if c.is_control() => {}
            c => escaped.push(c),
        }
    }
    escaped
}
fn timestamp(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
// Gregorian civil date from Unix days, independent of host timezone at recovery.
fn utc_date(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
/// Default label for output-track (remote/loopback) speech in Markdown and the
/// summary prompt, used whenever speaker identification has not resolved a
/// specific, stable speaker number for a turn (or has not run at all).
pub(crate) const DEFAULT_REMOTE_LABEL: &str = "Remote";

/// Presentation label for one transcript segment: the local user's own name
/// for an input (microphone) track, "Speaker N" (or the default remote
/// label) for an output (loopback) track, or the segment's recorded source
/// name as a fallback when its track cannot be resolved (e.g. a track later
/// removed from `recording.tracks`). Purely a display concern --
/// `segment.source_id`/`source_name` are never mutated, keeping
/// `transcript.json` a stable historical record independent of presentation.
pub(crate) fn presentation_label(
    recording: &Recording,
    segment: &TranscriptSegment,
    local_speaker_name: &str,
    remote_speaker: Option<u32>,
) -> String {
    match recording
        .tracks
        .iter()
        .find(|track| track.source_id == segment.source_id)
        .map(|track| track.direction.as_str())
    {
        Some("input") => local_speaker_name.to_string(),
        Some("output") => remote_speaker
            .map(|n| format!("Speaker {n}"))
            .unwrap_or_else(|| DEFAULT_REMOTE_LABEL.to_string()),
        _ => segment.source_name.clone(),
    }
}

pub(crate) fn markdown_document(recording: &Recording, t: &Transcript) -> String {
    markdown_document_labeled(recording, t, None)
}

/// As `markdown_document`, optionally overlaying per-segment remote speaker
/// numbers (parallel to `t.segments`; `None` entries mean undecided) from a
/// completed local speaker-identification checkpoint (see `diarization.rs`).
/// A pure function of `(recording, t, remote_labels)`, so the diarization
/// companion Markdown stays checkpoint-safe just like the base transcript.
pub(crate) fn markdown_document_labeled(
    recording: &Recording,
    t: &Transcript,
    remote_labels: Option<&[Option<u32>]>,
) -> String {
    let local_speaker_name = t.local_speaker_name.as_deref().unwrap_or("You");
    let note = if remote_labels.is_some() {
        format!(
            "Labels are approximate. \"{}\" is your own microphone track. Remote turns are grouped into Speaker 1, Speaker 2, and so on by voice similarity within each recorded source, or \"{}\" when uncertain; these are not named identities and are not comparable across recordings. No summary was regenerated for this file.",
            literal(local_speaker_name),
            DEFAULT_REMOTE_LABEL
        )
    } else {
        format!(
            "\"{}\" is your own microphone track; other tracks are labeled \"{}\". These reflect local/remote roles, not per person identities, unless speaker identification has run.",
            literal(local_speaker_name),
            DEFAULT_REMOTE_LABEL
        )
    };
    let mut output = format!("# Meeting — {}\n\n- Recording ID: `{}`\n- Duration: {}\n- Model: {}\n- Audio sources: {}\n\n{}\n\n", utc_date(recording.started_at), recording.recording_id, timestamp(recording.duration_ms), literal(&t.model_key), recording.tracks.iter().map(|track| literal(&track.name)).collect::<Vec<_>>().join(", "), note);
    if t.segments.is_empty() {
        output.push_str("No speech segments were returned by the local engine.\n");
    }
    for (i, segment) in t.segments.iter().enumerate() {
        let remote_speaker = remote_labels.and_then(|labels| labels.get(i).copied().flatten());
        let label = presentation_label(recording, segment, local_speaker_name, remote_speaker);
        output.push_str(&format!(
            "**[{}–{}] {}:** {}\n\n",
            timestamp(segment.start_ms),
            timestamp(segment.end_ms),
            literal(&label),
            literal(&segment.text)
        ));
    }
    let capture_warning = recording.status == "interrupted"
        || recording
            .tracks
            .iter()
            .any(|track| track.error.is_some() || track.dropped_samples > 0);
    if capture_warning || !t.warnings.is_empty() {
        output.push_str("## Processing notes\n\n");
        if capture_warning {
            output.push_str("- The recording was interrupted or contains capture gaps. Retained audio may be incomplete.\n");
        }
        for warning in &t.warnings {
            output.push_str(&format!("- {}\n", literal(warning)));
        }
    }
    output
}

#[cfg(test)]
#[path = "transcription_tests.rs"]
mod tests;
