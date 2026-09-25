//! Optional, durable speaker attribution. Never changes the Whisper checkpoint.
use crate::{
    diarization_engine::DiarizationEngine,
    diarization_store::{DiarizationStore, ModelPair, SpeakerJob},
    diarization_types::{DiarizationOutput, DiarizationRequest},
    recording::Recording,
    recording_store::RecordingStore,
    speaker_alignment::align_speakers,
    transcription::{
        atomic_new, checksum, markdown_document_labeled, output_path, processing_guard,
        read_checkpoint, recording_directory, reject_symlink, validate_transcript,
    },
    transcription_engine::hash_model,
    transcription_store::TranscriptionStore,
    transcription_types::{ModelInfo, Transcript},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

type SettingsReader = Arc<dyn Fn() -> Result<bool, String> + Send + Sync>;
const KINDS: [&str; 2] = ["segmentation", "embedding"];
#[derive(Serialize)]
pub struct ModelStatus {
    kind: String,
    available: bool,
    error: Option<String>,
}
#[derive(Serialize)]
pub struct JobSummary {
    recording_id: String,
    status: String,
    progress: u8,
    error: Option<String>,
    speaker_count: Option<u32>,
}
#[derive(Serialize)]
pub struct DiarizationState {
    enabled: bool,
    inhibited: bool,
    models: Vec<ModelStatus>,
    jobs: Vec<JobSummary>,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct SpeakerCheckpoint {
    recording_id: String,
    parent_checksum: String,
    models: ModelPair,
    output: DiarizationOutput,
    labels: Vec<Option<u32>>,
}

pub struct DiarizationService {
    root: PathBuf,
    stores: Option<(Arc<DiarizationStore>, Arc<TranscriptionStore>)>,
    settings: SettingsReader,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
}
impl DiarizationService {
    pub fn new(
        root: PathBuf,
        settings: SettingsReader,
        engine: Arc<dyn DiarizationEngine>,
    ) -> Self {
        let inhibited = [
            "MEETING_RECORDER_DISABLE_TRANSCRIPTION",
            "MEETING_RECORDER_DISABLE_DIARIZATION",
        ]
        .iter()
        .any(|key| std::env::var(key).ok().as_deref() == Some("1"));
        Self::with_inhibition(root, settings, engine, inhibited)
    }
    fn with_inhibition(
        root: PathBuf,
        settings: SettingsReader,
        engine: Arc<dyn DiarizationEngine>,
        inhibited: bool,
    ) -> Self {
        let error = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let stores = match DiarizationStore::open(&root.join("diarization.sqlite3")).and_then(|d| {
            TranscriptionStore::open(&root.join("transcription.sqlite3"))
                .map(|t| (Arc::new(d), Arc::new(t)))
        }) {
            Ok(stores) => Some(stores),
            Err(e) => {
                set_error(&error, Some(e));
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some((d, t)) = stores.clone() {
                let (r, s, c, e) = (
                    root.clone(),
                    settings.clone(),
                    cancel.clone(),
                    error.clone(),
                );
                match thread::Builder::new().name("local-speakers".into()).spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let recordings = match processing_guard().lock()
                            .map_err(|_| "Processing guard failed.".to_string())
                            .and_then(|_guard| d.recover())
                            .and_then(|()| RecordingStore::open(&r)) {
                            Ok(value) => value, Err(err) => { set_error(&e, Some(err)); return; }
                        };
                        while !c.load(Ordering::Acquire) {
                            let result = s().and_then(|enabled| {
                                if enabled {
                                    let _guard = processing_guard().lock().map_err(|_| "Processing guard failed.".to_string())?;
                                    tick(&r, &d, &t, &recordings, engine.as_ref(), &c)
                                } else { Ok(()) }
                            });
                            set_error(&e, result.err());
                            for _ in 0..10 {
                                if c.load(Ordering::Acquire) { break; }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }));
                    if result.is_err() { set_error(&e, Some("Speaker worker stopped unexpectedly. Restart to recover its job.".into())); }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start speaker worker: {e}"))),
                }
            }
        }
        Self {
            root,
            stores,
            settings,
            cancel,
            worker: Mutex::new(worker),
            error,
            inhibited,
        }
    }
    fn stores(&self) -> Result<(&DiarizationStore, &TranscriptionStore), String> {
        self.stores
            .as_ref()
            .map(|(d, t)| (d.as_ref(), t.as_ref()))
            .ok_or_else(|| {
                "Speaker storage is unavailable. Original transcripts remain local.".into()
            })
    }
    pub fn state(&self) -> DiarizationState {
        let mut error = self.error.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let enabled = (self.settings)().unwrap_or_else(|e| {
            error = Some(e);
            false
        });
        let mut models = Vec::new();
        let mut jobs = Vec::new();
        for kind in KINDS {
            let result = self
                .stores()
                .and_then(|(_, t)| registered_model(&self.root, t, kind));
            models.push(ModelStatus {
                kind: kind.into(),
                available: matches!(&result, Ok(Some(_))),
                error: result.err(),
            });
        }
        match self.stores().and_then(|(d, _)| d.all()) {
            Ok(saved) => {
                jobs = saved
                    .into_iter()
                    .map(|j| JobSummary {
                        recording_id: j.recording_id,
                        status: j.status,
                        progress: j.progress,
                        error: j.error,
                        speaker_count: j.speaker_count,
                    })
                    .collect()
            }
            Err(e) => error = Some(e),
        }
        DiarizationState {
            enabled,
            inhibited: self.inhibited,
            models,
            jobs,
            error,
        }
    }
    pub fn import_model(&self, kind: &str, source: PathBuf) -> Result<ModelInfo, String> {
        validate_kind(kind)?;
        if self.cancel.load(Ordering::Acquire) {
            return Err("Speaker processing is shutting down.".into());
        }
        let (store, registry) = self.stores()?;
        reject_symlink(&source)?;
        let metadata = fs::metadata(&source).map_err(err)?;
        if !metadata.is_file()
            || !(4096..=512 * 1024 * 1024).contains(&metadata.len())
            || source.extension().and_then(|x| x.to_str()) != Some("onnx")
        {
            return Err("Choose a trusted ONNX model between 4 KiB and 512 MiB.".into());
        }
        let hash = hash_model(&source)?;
        let root = self.root.canonicalize().map_err(err)?;
        let models = root.join("models");
        reject_symlink(&models)?;
        fs::create_dir_all(&models).map_err(err)?;
        let target = output_path(&models, &format!("{hash}.onnx"))?;
        if !target.exists() {
            let tmp = models.join(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| {
                let input = File::open(&source).map_err(err)?;
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .map_err(err)?;
                let copied =
                    std::io::copy(&mut input.take(metadata.len() + 1), &mut out).map_err(err)?;
                out.sync_all().map_err(err)?;
                drop(out);
                if copied != metadata.len() || hash_model(&tmp)? != hash {
                    return Err("Model changed during import.".into());
                }
                fs::rename(&tmp, &target).map_err(err)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            result?;
        }
        if hash_model(&target)? != hash {
            return Err("Stored model checksum mismatch; file preserved.".into());
        }
        let key = format!("diarization_{kind}");
        registry.upsert_model(&key, &target.display().to_string(), &hash, metadata.len())?;
        store.wake_waiting()?;
        Ok(ModelInfo {
            key,
            sha256: hash,
            bytes: metadata.len(),
        })
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Speaker processing is disabled in this session.".into());
        }
        self.stores()?.0.retry(id)
    }
    pub fn preferred_transcript_path(&self, id: &str) -> Result<Option<PathBuf>, String> {
        // Failure of the optional stage must not hide a usable baseline transcript.
        let Some((store, registry)) = &self.stores else {
            return Ok(None);
        };
        let Some(job) = store.get(id)? else {
            return Ok(None);
        };
        if job.status != "completed" {
            return Ok(None);
        }
        let recordings = RecordingStore::open(&self.root)?;
        let (recording, transcript, directory) =
            load_parent(&self.root, registry, &recordings, &job)?;
        let json = output_path(&directory, "speakers.json")?;
        let bytes = read_checkpoint(&json)?;
        if job.output_hash.as_deref() != Some(checksum(&bytes).as_str()) {
            return Err("Speaker checkpoint checksum mismatch; files preserved.".into());
        }
        let checkpoint = validate_checkpoint(&bytes, &job, &recording, &transcript)?;
        let md = output_path(&directory, "transcript-speakers.md")?;
        if read_checkpoint(&md)?
            != speaker_markdown(&recording, &transcript, &checkpoint).as_bytes()
        {
            return Err("Speaker Markdown was modified or damaged; open the recordings folder to inspect preserved files.".into());
        }
        Ok(Some(md))
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self
            .worker
            .lock()
            .map_err(|_| "Speaker worker lock failed.")?
            .take()
        {
            handle
                .join()
                .map_err(|_| "Speaker worker could not shut down.")?;
        }
        Ok(())
    }
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn set_error(slot: &Mutex<Option<String>>, value: Option<String>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = value;
}
pub(crate) fn validate_kind(kind: &str) -> Result<(), String> {
    if KINDS.contains(&kind) {
        Ok(())
    } else {
        Err("Unknown speaker model kind.".into())
    }
}
fn managed_model(
    root: &Path,
    path: &str,
    hash: &str,
    bytes: Option<u64>,
) -> Result<PathBuf, String> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("Invalid speaker model checksum.".into());
    }
    let models = root.canonicalize().map_err(err)?.join("models");
    reject_symlink(&models)?;
    let expected = output_path(&models, &format!("{hash}.onnx"))?;
    if Path::new(path) != expected {
        return Err("Speaker model is outside managed storage.".into());
    }
    let metadata = fs::metadata(&expected)
        .map_err(|_| "Imported speaker model is missing; import it again.")?;
    if !metadata.is_file() || bytes.is_some_and(|b| b != metadata.len()) {
        return Err("Speaker model size changed.".into());
    }
    Ok(expected)
}
fn registered_model(
    root: &Path,
    registry: &TranscriptionStore,
    kind: &str,
) -> Result<Option<(String, String)>, String> {
    registry
        .model(&format!("diarization_{kind}"))?
        .map(|(path, hash, bytes)| {
            managed_model(root, &path, &hash, Some(bytes))?;
            Ok((path, hash))
        })
        .transpose()
}
fn model_pair(root: &Path, registry: &TranscriptionStore) -> Result<Option<ModelPair>, String> {
    let a = registered_model(root, registry, "segmentation")?;
    let b = registered_model(root, registry, "embedding")?;
    Ok(a.zip(b).map(
        |((segmentation_path, segmentation_hash), (embedding_path, embedding_hash))| ModelPair {
            segmentation_path,
            segmentation_hash,
            embedding_path,
            embedding_hash,
        },
    ))
}
fn load_parent(
    root: &Path,
    registry: &TranscriptionStore,
    recordings: &RecordingStore,
    job: &SpeakerJob,
) -> Result<(Recording, Transcript, PathBuf), String> {
    let parent = registry
        .get(&job.recording_id)?
        .ok_or("Whisper job is missing.")?;
    if parent.status != "completed"
        || parent.checksum.as_deref() != Some(job.parent_checksum.as_str())
    {
        return Err("Whisper checkpoint changed or is incomplete.".into());
    }
    let recording = recordings
        .get_by_id(&job.recording_id)?
        .ok_or("Recording metadata is unavailable.")?;
    if !recording.auto_transcribe
        || !matches!(recording.status.as_str(), "recorded" | "interrupted")
    {
        return Err("Recording is not eligible for speaker processing.".into());
    }
    let directory = recording_directory(root, &job.recording_id)?;
    let json = output_path(&directory, "transcript.json")?;
    if parent.transcript_path.as_deref() != Some(json.to_string_lossy().as_ref()) {
        return Err("Whisper checkpoint path is invalid.".into());
    }
    let bytes = read_checkpoint(&json)?;
    if checksum(&bytes) != job.parent_checksum {
        return Err("Whisper checkpoint checksum mismatch; file preserved.".into());
    }
    let transcript = serde_json::from_slice(&bytes).map_err(err)?;
    validate_transcript(&transcript, &parent, &recording)?;
    Ok((recording, transcript, directory))
}
fn validate_output(output: &DiarizationOutput, recording: &Recording) -> Result<(), String> {
    if output.recording_id != recording.recording_id
        || output.warnings.len() > 32
        || output.warnings.iter().any(|s| s.len() > 512)
    {
        return Err("Invalid speaker result identity or warnings.".into());
    }
    for turn in &output.turns {
        if turn.end_ms < turn.start_ms
            || turn.end_ms > recording.duration_ms.saturating_add(30_000)
            || !recording
                .tracks
                .iter()
                .any(|t| t.source_id == turn.source_id)
            || turn
                .confidence
                .is_some_and(|c| !c.is_finite() || !(0.0..=1.0).contains(&c))
        {
            return Err("Invalid speaker result source, timestamp, or confidence.".into());
        }
    }
    Ok(())
}
fn validate_checkpoint(
    bytes: &[u8],
    job: &SpeakerJob,
    recording: &Recording,
    transcript: &Transcript,
) -> Result<SpeakerCheckpoint, String> {
    let checkpoint: SpeakerCheckpoint = serde_json::from_slice(bytes).map_err(err)?;
    if checkpoint.recording_id != job.recording_id
        || checkpoint.parent_checksum != job.parent_checksum
        || Some(&checkpoint.models) != job.models.as_ref()
    {
        return Err("Speaker checkpoint identity or model pins changed; file preserved.".into());
    }
    validate_output(&checkpoint.output, recording)?;
    if align_speakers(transcript, &checkpoint.output)? != checkpoint.labels {
        return Err("Speaker checkpoint labels do not match their source turns.".into());
    }
    Ok(checkpoint)
}
fn speaker_markdown(
    recording: &Recording,
    transcript: &Transcript,
    checkpoint: &SpeakerCheckpoint,
) -> String {
    let mut labelled = transcript.clone();
    labelled
        .warnings
        .extend(checkpoint.output.warnings.iter().cloned());
    markdown_document_labeled(recording, &labelled, Some(&checkpoint.labels))
}
/// Best-effort lookup of a recording's completed local speaker-identification
/// labels (see `SpeakerCheckpoint`), for embedding remote speaker numbers into
/// the summary prompt (see `summary.rs`). Returns `None` on anything short of
/// a fully validated, completed checkpoint matching this exact transcript --
/// diarization is optional and must never block or corrupt a summary. If
/// diarization finishes only *after* a summary was already generated, this
/// simply means that summary predates the labels; the user can regenerate it
/// (see `regenerate_summary`) to pick them up. We do not auto-regenerate: a
/// summary rerun is an expensive Ollama call better left to explicit intent.
pub(crate) fn read_completed_speaker_labels(
    root: &Path,
    recording: &Recording,
    transcript: &Transcript,
) -> Option<Vec<Option<u32>>> {
    let store = DiarizationStore::open(&root.join("diarization.sqlite3")).ok()?;
    let job = store.get(&recording.recording_id).ok().flatten()?;
    if job.status != "completed" {
        return None;
    }
    let directory = recording_directory(root, &recording.recording_id).ok()?;
    let json = output_path(&directory, "speakers.json").ok()?;
    let bytes = read_checkpoint(&json).ok()?;
    if job.output_hash.as_deref() != Some(checksum(&bytes).as_str()) {
        return None;
    }
    validate_checkpoint(&bytes, &job, recording, transcript)
        .ok()
        .map(|checkpoint| checkpoint.labels)
}

fn tick(
    root: &Path,
    store: &DiarizationStore,
    registry: &TranscriptionStore,
    recordings: &RecordingStore,
    engine: &dyn DiarizationEngine,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for parent in registry
        .all()?
        .into_iter()
        .filter(|j| j.status == "completed")
    {
        if recordings
            .get_by_id(&parent.recording_id)?
            .is_some_and(|r| {
                r.auto_transcribe && matches!(r.status.as_str(), "recorded" | "interrupted")
            })
        {
            store.enqueue(
                &parent.recording_id,
                parent
                    .checksum
                    .as_deref()
                    .ok_or("Completed Whisper job has no checksum.")?,
            )?;
        }
    }
    let Some(mut job) = store.next()? else {
        return Ok(());
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    if registry.has_restart_intent(&job.recording_id)? {
        store.remove_if_idle(&job.recording_id)?;
        return Ok(());
    }
    if recordings.get_by_id(&job.recording_id)?.is_none() {
        store.remove_if_idle(&job.recording_id)?;
        return Ok(());
    }
    let models = match job.models.clone().map(Ok).unwrap_or_else(|| {
        model_pair(root, registry)
            .and_then(|p| p.ok_or_else(|| "Choose both local speaker models in Settings.".into()))
    }) {
        Ok(models) => models,
        Err(e) => {
            store.status(&job.recording_id, "waiting_models", 0, Some(&e))?;
            return Ok(());
        }
    };
    if !store.claim(&job.recording_id, &models)? {
        return Ok(());
    }
    job.models = Some(models.clone());
    let result = (|| {
        let (recording, transcript, directory) = load_parent(root, registry, recordings, &job)?;
        let json = output_path(&directory, "speakers.json")?;
        let md = output_path(&directory, "transcript-speakers.md")?;
        let (checkpoint, bytes) = if json.exists() {
            let bytes = read_checkpoint(&json)?;
            (
                validate_checkpoint(&bytes, &job, &recording, &transcript)?,
                bytes,
            )
        } else {
            if md.exists() {
                return Err(
                    "Speaker Markdown exists without its checkpoint; file preserved.".into(),
                );
            }
            let segmentation_model = managed_model(
                root,
                &models.segmentation_path,
                &models.segmentation_hash,
                None,
            )?;
            let embedding_model =
                managed_model(root, &models.embedding_path, &models.embedding_hash, None)?;
            if hash_model(&segmentation_model)? != models.segmentation_hash
                || hash_model(&embedding_model)? != models.embedding_hash
            {
                return Err("Speaker model checksum mismatch.".into());
            }
            let request = DiarizationRequest {
                recording: recording.clone(),
                recordings_root: root.to_owned(),
                segmentation_model,
                embedding_model,
                segmentation_hash: models.segmentation_hash.clone(),
                embedding_hash: models.embedding_hash.clone(),
            };
            let mut progress_error = None;
            let mut progress = |p: u8| {
                if let Err(e) = store.status(&job.recording_id, "diarizing", p.min(99), None) {
                    progress_error = Some(e);
                }
            };
            let output = engine.diarize(&request, cancel, &mut progress)?;
            if let Some(e) = progress_error {
                return Err(e);
            }
            if cancel.load(Ordering::Acquire) {
                return Err("Speaker processing cancelled.".into());
            }
            validate_output(&output, &recording)?;
            let labels = align_speakers(&transcript, &output)?;
            let checkpoint = SpeakerCheckpoint {
                recording_id: job.recording_id.clone(),
                parent_checksum: job.parent_checksum.clone(),
                models,
                output,
                labels,
            };
            let bytes = serde_json::to_vec_pretty(&checkpoint).map_err(err)?;
            atomic_new(&json, &bytes)?;
            (checkpoint, bytes)
        };
        let markdown = speaker_markdown(&recording, &transcript, &checkpoint);
        if md.exists() {
            if read_checkpoint(&md)? != markdown.as_bytes() {
                return Err("Existing speaker Markdown differs; file preserved.".into());
            }
        } else {
            atomic_new(&md, markdown.as_bytes())?;
        }
        let count = checkpoint
            .labels
            .iter()
            .flatten()
            .collect::<HashSet<_>>()
            .len() as u32;
        store.complete(&job.recording_id, &checksum(&bytes), count)
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

#[cfg(test)]
#[path = "diarization_tests.rs"]
mod tests;
