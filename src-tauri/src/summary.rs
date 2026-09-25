//! Optional, durable local meeting summaries via a local Ollama server. Mirrors
//! `diarization.rs`'s shape: a SQLite job journal, a serial background worker
//! sharing `transcription::processing_guard` with Whisper/diarization so a
//! summary is never generated while the GPU is busy transcribing, crash-safe
//! checkpointed JSON/Markdown outputs, and retry/backoff for a down Ollama.
use crate::{
    recording::Recording,
    recording_store::RecordingStore,
    summary_engine::{SummaryEngine, SummaryRequest},
    summary_store::{SummaryJob, SummaryStore},
    summary_types::SummaryOutput,
    transcription::{
        atomic_new, checksum, output_path, processing_guard, read_checkpoint, recording_directory,
        validate_transcript,
    },
    transcription_store::TranscriptionStore,
    transcription_types::Transcript,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::fs;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// The subset of app settings the summary worker needs on each tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummarySettings {
    pub enabled: bool,
    pub ollama_url: String,
    pub model: String,
}
#[cfg(test)]
impl SummarySettings {
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            enabled: true,
            ollama_url: "http://127.0.0.1:11434".into(),
            model: model.into(),
        }
    }
}
type SettingsReader = Arc<dyn Fn() -> Result<SummarySettings, String> + Send + Sync>;

#[derive(Serialize)]
pub struct JobSummary {
    pub recording_id: String,
    pub status: String,
    pub progress: u8,
    pub error: Option<String>,
    pub title: Option<String>,
}
#[derive(Serialize)]
pub struct SummaryState {
    pub enabled: bool,
    pub inhibited: bool,
    pub jobs: Vec<JobSummary>,
    pub error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct SummaryCheckpoint {
    recording_id: String,
    parent_checksum: String,
    model: String,
    output: SummaryOutput,
}

pub struct SummaryService {
    root: PathBuf,
    stores: Option<(Arc<SummaryStore>, Arc<TranscriptionStore>)>,
    settings: SettingsReader,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
}
impl SummaryService {
    pub fn new(root: PathBuf, settings: SettingsReader, engine: Arc<dyn SummaryEngine>) -> Self {
        let inhibited = [
            "MEETING_RECORDER_DISABLE_TRANSCRIPTION",
            "MEETING_RECORDER_DISABLE_SUMMARY",
        ]
        .iter()
        .any(|key| std::env::var(key).ok().as_deref() == Some("1"));
        Self::with_inhibition(root, settings, engine, inhibited)
    }
    pub(crate) fn with_inhibition(
        root: PathBuf,
        settings: SettingsReader,
        engine: Arc<dyn SummaryEngine>,
        inhibited: bool,
    ) -> Self {
        let error = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let stores = match SummaryStore::open(&root.join("summary.sqlite3")).and_then(|s| {
            TranscriptionStore::open(&root.join("transcription.sqlite3"))
                .map(|t| (Arc::new(s), Arc::new(t)))
        }) {
            Ok(stores) => Some(stores),
            Err(e) => {
                set_error(&error, Some(e));
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some((s, t)) = stores.clone() {
                let (r, settings_reader, c, e, worker_engine) = (
                    root.clone(),
                    settings.clone(),
                    cancel.clone(),
                    error.clone(),
                    engine.clone(),
                );
                match thread::Builder::new().name("local-summary".into()).spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let recordings = match processing_guard().lock()
                            .map_err(|_| "Processing guard failed.".to_string())
                            .and_then(|_guard| s.recover())
                            .and_then(|()| RecordingStore::open(&r)) {
                            Ok(value) => value, Err(err) => { set_error(&e, Some(err)); return; }
                        };
                        while !c.load(Ordering::Acquire) {
                            let result = settings_reader().and_then(|settings| {
                                if settings.enabled {
                                    let _guard = processing_guard().lock().map_err(|_| "Processing guard failed.".to_string())?;
                                    tick(&r, &s, &t, &recordings, worker_engine.as_ref(), &settings, &c)
                                } else { Ok(()) }
                            });
                            set_error(&e, result.err());
                            for _ in 0..10 {
                                if c.load(Ordering::Acquire) { break; }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }));
                    if result.is_err() { set_error(&e, Some("Summary worker stopped unexpectedly. Restart to recover its job.".into())); }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start summary worker: {e}"))),
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
    fn stores(&self) -> Result<(&SummaryStore, &TranscriptionStore), String> {
        self.stores
            .as_ref()
            .map(|(s, t)| (s.as_ref(), t.as_ref()))
            .ok_or_else(|| "Summary storage is unavailable. Transcripts remain local.".into())
    }
    pub fn state(&self) -> SummaryState {
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
                        title: j.title,
                    })
                    .collect()
            }
            Err(e) => error = Some(e),
        }
        SummaryState {
            enabled,
            inhibited: self.inhibited,
            jobs,
            error,
        }
    }
    pub fn retry(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Summarizing is disabled in this session.".into());
        }
        self.stores()?.0.retry(id)
    }
    pub fn regenerate(&self, id: &str) -> Result<(), String> {
        if self.inhibited || self.cancel.load(Ordering::Acquire) {
            return Err("Summarizing is disabled in this session.".into());
        }
        self.stores()?.0.regenerate(id)
    }
    /// The user changed a setting that could unblock waiting jobs (enabled the
    /// feature, or fixed the URL/model); wake them immediately rather than
    /// waiting out their backoff.
    pub fn wake_waiting(&self) -> Result<(), String> {
        self.stores()?.0.wake_waiting()
    }
    pub fn summary_path(&self, id: &str) -> Result<PathBuf, String> {
        let (store, registry) = self.stores()?;
        let job = store.get(id)?.ok_or("Unknown summary job.")?;
        if job.status != "completed" {
            return Err("Summary is not complete.".into());
        }
        let recordings = RecordingStore::open(&self.root)?;
        let (recording, transcript, directory) = load_parent(&self.root, registry, &recordings, &job)?;
        let json = output_path(&directory, "summary.json")?;
        let bytes = read_checkpoint(&json)?;
        if job.output_hash.as_deref() != Some(checksum(&bytes).as_str()) {
            return Err("Summary checkpoint checksum mismatch; files preserved.".into());
        }
        let checkpoint = validate_checkpoint(&bytes, &job, &recording, &transcript)?;
        let md = output_path(&directory, "summary.md")?;
        if read_checkpoint(&md)? != summary_markdown(&recording, &checkpoint).as_bytes() {
            return Err("Summary Markdown was modified or damaged; open the recordings folder to inspect preserved files.".into());
        }
        Ok(md)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self
            .worker
            .lock()
            .map_err(|_| "Summary worker lock failed.")?
            .take()
        {
            handle
                .join()
                .map_err(|_| "Summary worker could not shut down.")?;
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
fn load_parent(
    root: &Path,
    registry: &TranscriptionStore,
    recordings: &RecordingStore,
    job: &SummaryJob,
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
        return Err("Recording is not eligible for summarizing.".into());
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
fn validate_checkpoint(
    bytes: &[u8],
    job: &SummaryJob,
    recording: &Recording,
    transcript: &Transcript,
) -> Result<SummaryCheckpoint, String> {
    let checkpoint: SummaryCheckpoint = serde_json::from_slice(bytes).map_err(err)?;
    if checkpoint.recording_id != job.recording_id
        || checkpoint.parent_checksum != job.parent_checksum
        || Some(&checkpoint.model) != job.model.as_ref()
        || checkpoint.recording_id != recording.recording_id
        || !checkpoint.output.is_meaningful()
    {
        return Err("Summary checkpoint identity, model pin, or content is invalid; file preserved.".into());
    }
    // The checkpoint's summary output is independent of segment-level content, but
    // must still describe this exact transcript, not a stale/foreign one.
    if transcript.recording_id != recording.recording_id {
        return Err("Summary checkpoint transcript identity mismatch.".into());
    }
    Ok(checkpoint)
}
fn literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('*', "\\*").replace('_', "\\_")
}
fn summary_markdown(recording: &Recording, checkpoint: &SummaryCheckpoint) -> String {
    let output = &checkpoint.output;
    let mut markdown = format!(
        "# {}\n\n- Recording ID: `{}`\n- Model: {}\n- Language: {}\n\n## Summary\n\n{}\n\n",
        literal(&output.title),
        recording.recording_id,
        literal(&checkpoint.model),
        literal(&output.language),
        output.summary,
    );
    let section = |title: &str, items: &[String]| -> String {
        if items.is_empty() {
            return String::new();
        }
        let mut s = format!("## {title}\n\n");
        for item in items {
            s.push_str(&format!("- {}\n", literal(item)));
        }
        s.push('\n');
        s
    };
    markdown.push_str(&section("Key points", &output.key_points));
    markdown.push_str(&section("Decisions", &output.decisions));
    if !output.action_items.is_empty() {
        markdown.push_str("## Action items\n\n");
        for item in &output.action_items {
            markdown.push_str(&format!(
                "- {}{}{}\n",
                literal(&item.task),
                item.owner.as_deref().map(|o| format!(" — {}", literal(o))).unwrap_or_default(),
                item.due.as_deref().map(|d| format!(" (due {})", literal(d))).unwrap_or_default(),
            ));
        }
        markdown.push('\n');
    }
    markdown.push_str(&section("Open questions", &output.open_questions));
    markdown.push_str("---\n\nGenerated locally by an Ollama model from this recording's transcript. Names, owners, and dates are only as reliable as the transcript; verify anything important.\n");
    markdown
}
fn tick(
    root: &Path,
    store: &SummaryStore,
    registry: &TranscriptionStore,
    recordings: &RecordingStore,
    engine: &dyn SummaryEngine,
    settings: &SummarySettings,
    cancel: &AtomicBool,
) -> Result<(), String> {
    store.wake_due()?;
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
    if !store.claim(&job.recording_id, &settings.model)? {
        return Ok(());
    }
    job.model = Some(settings.model.clone());
    let result: Result<(), String> = (|| {
        let (recording, transcript, directory) = load_parent(root, registry, recordings, &job)?;
        let json = output_path(&directory, "summary.json")?;
        let md = output_path(&directory, "summary.md")?;
        let (checkpoint, bytes) = if json.exists() {
            let bytes = read_checkpoint(&json)?;
            (
                validate_checkpoint(&bytes, &job, &recording, &transcript)?,
                bytes,
            )
        } else {
            if md.exists() {
                return Err("Summary Markdown exists without its checkpoint; file preserved.".into());
            }
            let request = SummaryRequest {
                recording: recording.clone(),
                transcript: transcript.clone(),
                ollama_url: settings.ollama_url.clone(),
                model: settings.model.clone(),
            };
            let mut progress_error = None;
            let mut progress = |p: u8| {
                if let Err(e) = store.status(&job.recording_id, "summarizing", p.min(99), None) {
                    progress_error = Some(e);
                }
            };
            let output = match engine.summarize(&request, cancel, &mut progress) {
                Ok(output) => output,
                Err(e) if is_transient_ollama_error(&e) => {
                    store.wait_for_ollama(&job.recording_id, &e)?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if let Some(e) = progress_error {
                return Err(e);
            }
            if cancel.load(Ordering::Acquire) {
                return Err("Summarizing cancelled; no result was applied.".into());
            }
            let checkpoint = SummaryCheckpoint {
                recording_id: job.recording_id.clone(),
                parent_checksum: job.parent_checksum.clone(),
                model: settings.model.clone(),
                output,
            };
            let bytes = serde_json::to_vec_pretty(&checkpoint).map_err(err)?;
            atomic_new(&json, &bytes)?;
            (checkpoint, bytes)
        };
        let markdown = summary_markdown(&recording, &checkpoint);
        if md.exists() {
            if read_checkpoint(&md)? != markdown.as_bytes() {
                return Err("Existing summary Markdown differs; file preserved.".into());
            }
        } else {
            atomic_new(&md, markdown.as_bytes())?;
        }
        store.complete(&job.recording_id, &checksum(&bytes), &checkpoint.output.title)
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
/// A server that is down, or a model that is not pulled yet, is expected to
/// resolve itself once the user starts Ollama or runs `ollama pull`. Treat it
/// as "waiting", not a permanent failure that needs a manual retry click.
fn is_transient_ollama_error(message: &str) -> bool {
    message.contains("not reachable") || message.contains("does not have the model")
}
#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
