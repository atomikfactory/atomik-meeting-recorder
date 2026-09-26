//! Optional, durable AppFlowy Cloud export. Mirrors `summary.rs`'s shape: a
//! SQLite job journal (`appflowy_store.rs`), a serial background worker, and
//! retry/backoff for a down or rate-limiting server. The one thing this
//! module does NOT mirror from `summary.rs` is direct network access -- every
//! AppFlowy request goes through `appflowy_client::AppFlowyClient`, the only
//! type in this codebase allowed to open a socket to AppFlowy, and that
//! client can only create pages, never update or delete anything (see its
//! module docs and `appflowy_allowlist_tests.rs`).
use crate::{
    appflowy_blocks::{build_page_document, MeetingExportData, SpeakerTurn},
    appflowy_client::{AppFlowyClient, ClientError, FolderView, Workspace},
    appflowy_credentials::SharedCredentialStore,
    appflowy_store::{date_key, meeting_key, root_key, AppFlowyJob, AppFlowyStore},
    diarization_store::DiarizationStore,
    recording::Recording,
    recording_store::RecordingStore,
    summary_store::SummaryStore,
    transcription::{checksum, output_path, presentation_label, read_checkpoint, recording_directory, validate_transcript},
    transcription_store::TranscriptionStore,
    transcription_types::Transcript,
};
use chrono::{Local, TimeZone};
use serde::Serialize;
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

/// The subset of app settings the export worker needs on each tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppFlowySettings {
    pub enabled: bool,
    pub auto_export: bool,
    pub base_url: String,
    pub use_native_block: bool,
    pub workspace_id: Option<String>,
    /// `None` means the workspace's own top level.
    pub parent_view_id: Option<String>,
    pub diarization_enabled: bool,
    pub summary_enabled: bool,
    pub local_speaker_name: String,
    pub extra_library_roots: Vec<PathBuf>,
}
#[cfg(test)]
impl AppFlowySettings {
    pub fn for_test(workspace_id: impl Into<String>) -> Self {
        Self {
            enabled: true,
            auto_export: true,
            base_url: "https://beta.appflowy.cloud".into(),
            use_native_block: true,
            workspace_id: Some(workspace_id.into()),
            parent_view_id: None,
            diarization_enabled: false,
            summary_enabled: true,
            local_speaker_name: "You".into(),
            extra_library_roots: Vec::new(),
        }
    }
}
type SettingsReader = Arc<dyn Fn() -> Result<AppFlowySettings, String> + Send + Sync>;

#[derive(Serialize, Clone, Debug)]
pub struct JobSummary {
    pub recording_id: String,
    pub status: String,
    pub error: Option<String>,
    pub export_index: u32,
}
#[derive(Serialize)]
pub struct AppFlowyState {
    pub enabled: bool,
    pub auto_export: bool,
    pub connected: bool,
    pub email: Option<String>,
    pub base_url: String,
    pub jobs: Vec<JobSummary>,
    /// Set by `MEETING_RECORDER_DISABLE_APPFLOWY=1` (used by this crate's own
    /// verification harnesses); the worker never runs while this is true.
    pub inhibited: bool,
    pub error: Option<String>,
}

pub struct AppFlowyService {
    store: Option<Arc<AppFlowyStore>>,
    settings: SettingsReader,
    credentials: SharedCredentialStore,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
}
impl AppFlowyService {
    pub fn new(root: PathBuf, settings: SettingsReader, credentials: SharedCredentialStore) -> Self {
        let inhibited = std::env::var("MEETING_RECORDER_DISABLE_APPFLOWY").ok().as_deref() == Some("1");
        Self::with_inhibition(root, settings, credentials, inhibited)
    }
    pub(crate) fn with_inhibition(
        root: PathBuf,
        settings: SettingsReader,
        credentials: SharedCredentialStore,
        inhibited: bool,
    ) -> Self {
        let error = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let store = match AppFlowyStore::open(&root.join("appflowy.sqlite3")) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                set_error(&error, Some(e));
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some(store) = store.clone() {
                let (r, settings_reader, c, e, creds) = (root.clone(), settings.clone(), cancel.clone(), error.clone(), credentials.clone());
                match thread::Builder::new().name("appflowy-export".into()).spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(err) = store.recover() {
                            set_error(&e, Some(err));
                            return;
                        }
                        while !c.load(Ordering::Acquire) {
                            let result = settings_reader().and_then(|settings| {
                                if settings.enabled {
                                    tick(&r, &store, &settings, &creds, &c)
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
                        set_error(&e, Some("AppFlowy export worker stopped unexpectedly. Restart to resume it.".into()));
                    }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start AppFlowy export worker: {e}"))),
                }
            }
        }
        Self { store, settings, credentials, cancel, worker: Mutex::new(worker), error, inhibited }
    }
    fn store(&self) -> Result<&AppFlowyStore, String> {
        self.store.as_deref().ok_or_else(|| "AppFlowy export storage is unavailable.".into())
    }
    fn client(&self, settings: &AppFlowySettings) -> Result<AppFlowyClient, String> {
        AppFlowyClient::new(&settings.base_url, self.credentials.clone())
    }
    pub fn state(&self) -> AppFlowyState {
        let mut error = self.error.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let settings = (self.settings)().unwrap_or_else(|e| {
            error = Some(e);
            AppFlowySettings::default_disabled()
        });
        let connected = self.client(&settings).and_then(|c| c.is_signed_in()).unwrap_or(false);
        let email = self.client(&settings).and_then(|c| c.signed_in_email()).unwrap_or(None);
        let jobs = match self.store().and_then(|s| s.all_jobs()) {
            Ok(jobs) => jobs
                .into_iter()
                .map(|j| JobSummary { recording_id: j.recording_id, status: j.status, error: j.error, export_index: j.export_index })
                .collect(),
            Err(e) => {
                error = Some(e);
                Vec::new()
            }
        };
        AppFlowyState { enabled: settings.enabled, auto_export: settings.auto_export, connected, email, base_url: settings.base_url, jobs, inhibited: self.inhibited, error }
    }
    pub fn request_otp(&self, email: &str) -> Result<(), String> {
        let settings = (self.settings)()?;
        self.client(&settings)?.request_otp(email)
    }
    pub fn verify_otp(&self, email: &str, code: &str) -> Result<(), String> {
        let settings = (self.settings)()?;
        self.client(&settings)?.verify_otp(email, code)
    }
    pub fn sign_out(&self) -> Result<(), String> {
        let settings = (self.settings)()?;
        self.client(&settings)?.sign_out()
    }
    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, String> {
        let settings = (self.settings)()?;
        self.client(&settings)?.list_workspaces().map_err(|e| e.to_string())
    }
    /// Manual per-meeting export, independent of the `auto_export` toggle
    /// (still requires `enabled`, checked here rather than only in the
    /// worker so the button gives an immediate, clear error).
    pub fn export_now(&self, recording_id: &str) -> Result<(), String> {
        let settings = (self.settings)()?;
        if !settings.enabled {
            return Err("AppFlowy export is turned off in Settings.".into());
        }
        self.store()?.enqueue(recording_id)
    }
    /// Creates a brand-new meeting page rather than reusing the one from a
    /// previous export -- never modifies the prior page. Requires the
    /// caller to have already obtained the user's explicit confirmation
    /// (`confirm` is not itself the confirmation UI; the Settings/dashboard
    /// layer must ask first).
    pub fn export_again(&self, recording_id: &str, confirm: bool) -> Result<(), String> {
        if !confirm {
            return Err("Exporting again requires confirmation.".into());
        }
        let settings = (self.settings)()?;
        if !settings.enabled {
            return Err("AppFlowy export is turned off in Settings.".into());
        }
        self.store()?.export_again(recording_id)
    }
    pub fn retry(&self, recording_id: &str) -> Result<(), String> {
        self.store()?.retry(recording_id)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self.worker.lock().map_err(|_| "AppFlowy worker lock failed.")?.take() {
            handle.join().map_err(|_| "AppFlowy export worker could not shut down.")?;
        }
        Ok(())
    }
}
impl AppFlowySettings {
    fn default_disabled() -> Self {
        Self {
            enabled: false,
            auto_export: false,
            base_url: "https://beta.appflowy.cloud".into(),
            use_native_block: true,
            workspace_id: None,
            parent_view_id: None,
            diarization_enabled: false,
            summary_enabled: true,
            local_speaker_name: "You".into(),
            extra_library_roots: Vec::new(),
        }
    }
}
fn set_error(slot: &Mutex<Option<String>>, value: Option<String>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = value;
}

/// A recording is ready for export once its transcript is complete and (if
/// enabled) both diarization and summarizing have also finished, so speaker
/// labels and the summary are final before anything is sent.
fn is_ready_for_export(root: &Path, settings: &AppFlowySettings, recording: &Recording) -> Result<bool, String> {
    if settings.diarization_enabled && recording.tracks.iter().any(|t| t.direction == "output") {
        let status = DiarizationStore::open(&root.join("diarization.sqlite3"))?
            .get(&recording.recording_id)?
            .map(|j| j.status);
        if !diarization_settled(status.as_deref()) {
            return Ok(false);
        }
    }
    if settings.summary_enabled {
        let status = SummaryStore::open(&root.join("summary.sqlite3"))?
            .get(&recording.recording_id)?
            .map(|j| j.status);
        if !summary_settled(status.as_deref()) {
            return Ok(false);
        }
    }
    Ok(true)
}
/// Speaker labels are optional: only a job that can still finish is worth
/// waiting for. Missing models or a failed job export without labels rather
/// than holding the meeting back forever.
fn diarization_settled(status: Option<&str>) -> bool {
    !matches!(status, None | Some("queued" | "diarizing"))
}
/// A summary still pending (queued, running or waiting for Ollama to come
/// back) delays export; a failed summary exports the transcript alone.
fn summary_settled(status: Option<&str>) -> bool {
    matches!(status, Some("completed" | "failed"))
}

fn tick(
    root: &Path,
    store: &AppFlowyStore,
    settings: &AppFlowySettings,
    credentials: &SharedCredentialStore,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if settings.auto_export {
        let recordings = RecordingStore::open(root)?;
        let transcripts = TranscriptionStore::open(&root.join("transcription.sqlite3"))?;
        for parent in transcripts.all()?.into_iter().filter(|j| j.status == "completed") {
            let Some(recording) = recordings.get_by_id(&parent.recording_id)? else { continue };
            if !recording.auto_transcribe || !matches!(recording.status.as_str(), "recorded" | "interrupted") {
                continue;
            }
            if store.get_job(&recording.recording_id)?.is_some() {
                continue;
            }
            if is_ready_for_export(root, settings, &recording)? {
                store.enqueue(&recording.recording_id)?;
            }
        }
    }
    let Some(job) = store.next()? else {
        return Ok(());
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    if !store.claim(&job.recording_id)? {
        return Ok(());
    }
    if let Err((message, transient)) = export_one(root, store, settings, credentials, &job) {
        store.fail(&job.recording_id, &message, transient)?;
    }
    Ok(())
}

type ExportError = (String, bool);

fn classify_client_error(error: ClientError) -> ExportError {
    match error {
        ClientError::Transient(m) => (m, true),
        ClientError::Rejected(m) => (m, false),
        ClientError::Unauthorized => ("AppFlowy session expired; please sign in again.".into(), false),
    }
}

/// The fixed inputs `resolve_page` needs for one root/date/meeting page,
/// bundled to keep the function's argument count reasonable.
struct PagePlan<'a> {
    workspace_id: &'a str,
    key: &'a str,
    kind: &'a str,
    parent_remote_id: &'a str,
    name: &'a str,
    page_data: &'a serde_json::Value,
}

/// Resolves (finding or creating) one page in the root -> date -> meeting
/// chain. Read-before-write: a page already marked `created` locally is
/// trusted outright; otherwise the parent's folder tree is re-read and an
/// exact-name child is adopted if found (covers both a user's pre-existing
/// page and a crash after a successful remote create but before the local
/// journal was updated) -- only creating a new page when neither is true.
fn resolve_page(client: &AppFlowyClient, store: &AppFlowyStore, plan: PagePlan) -> Result<String, ExportError> {
    let PagePlan { workspace_id, key, kind, parent_remote_id, name, page_data } = plan;
    let record = store.find_or_plan(key, kind, Some(parent_remote_id), workspace_id, name).map_err(|e| (e, false))?;
    if let Some(remote_id) = record.remote_view_id {
        return Ok(remote_id);
    }
    let folder: FolderView = client.get_folder(workspace_id, Some(parent_remote_id)).map_err(classify_client_error)?;
    if let Some(existing) = folder.child_named(name) {
        let remote_id = existing.view_id.clone();
        store.mark_created(key, &remote_id).map_err(|e| (e, false))?;
        return Ok(remote_id);
    }
    let page = client
        .create_page(workspace_id, parent_remote_id, &record.planned_view_id, name, page_data)
        .map_err(classify_client_error)?;
    store.mark_created(key, &page.view_id).map_err(|e| (e, false))?;
    Ok(page.view_id)
}

fn empty_document() -> serde_json::Value {
    serde_json::json!({"type": "page", "data": {}, "children": []})
}

fn meeting_page_name(started_at_ms: u64, title: &str, export_index: u32) -> String {
    let local = Local
        .timestamp_millis_opt(started_at_ms as i64)
        .single()
        .unwrap_or_else(Local::now);
    let base = format!("{} — {}", local.format("%H:%M"), title);
    if export_index <= 1 {
        base
    } else {
        format!("{base} ({export_index})")
    }
}
fn local_date_string(started_at_ms: u64) -> String {
    Local
        .timestamp_millis_opt(started_at_ms as i64)
        .single()
        .unwrap_or_else(Local::now)
        .format("%Y-%m-%d")
        .to_string()
}

fn export_one(
    root: &Path,
    store: &AppFlowyStore,
    settings: &AppFlowySettings,
    credentials: &SharedCredentialStore,
    job: &AppFlowyJob,
) -> Result<(), ExportError> {
    let client = AppFlowyClient::new(&settings.base_url, credentials.clone()).map_err(|e| (e, false))?;
    let workspace_id = settings.workspace_id.clone().ok_or_else(|| ("AppFlowy workspace is not selected.".to_string(), false))?;
    let root_parent_id = settings.parent_view_id.clone().unwrap_or_else(|| workspace_id.clone());
    let (recording, meeting) = load_meeting_export_data(root, settings, &job.recording_id).map_err(|e| (e, false))?;

    let root_page_id = resolve_page(
        &client,
        store,
        PagePlan {
            workspace_id: &workspace_id,
            key: &root_key(&workspace_id, &root_parent_id),
            kind: "root",
            parent_remote_id: &root_parent_id,
            name: "My daily meetings",
            page_data: &empty_document(),
        },
    )?;
    let date = local_date_string(recording.started_at);
    let date_page_id = resolve_page(
        &client,
        store,
        PagePlan {
            workspace_id: &workspace_id,
            key: &date_key(&workspace_id, &root_page_id, &date),
            kind: "date",
            parent_remote_id: &root_page_id,
            name: &date,
            page_data: &empty_document(),
        },
    )?;
    let page_data = build_page_document(&meeting, settings.use_native_block);
    let meeting_name = meeting_page_name(recording.started_at, &meeting.title, job.export_index);
    resolve_page(
        &client,
        store,
        PagePlan {
            workspace_id: &workspace_id,
            key: &meeting_key(&job.recording_id, job.export_index),
            kind: "meeting",
            parent_remote_id: &date_page_id,
            name: &meeting_name,
            page_data: &page_data,
        },
    )?;
    store.complete(&job.recording_id).map_err(|e| (e, false))
}

fn load_meeting_export_data(
    root: &Path,
    settings: &AppFlowySettings,
    recording_id: &str,
) -> Result<(Recording, MeetingExportData), String> {
    let recordings = RecordingStore::open(root)?;
    let recording = recordings.get_by_id(recording_id)?.ok_or("Recording metadata is unavailable.")?;
    let registry = TranscriptionStore::open(&root.join("transcription.sqlite3"))?;
    let parent = registry.get(recording_id)?.ok_or("Whisper job is missing.")?;
    if parent.status != "completed" {
        return Err("Transcript is not complete.".into());
    }
    let directory = recording_directory(root, &settings.extra_library_roots, recording_id)?;
    let json_path = output_path(&directory, "transcript.json")?;
    let bytes = read_checkpoint(&json_path)?;
    if Some(checksum(&bytes)) != parent.checksum {
        return Err("Whisper checkpoint checksum mismatch; file preserved.".into());
    }
    let transcript: Transcript = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    validate_transcript(&transcript, &parent, &recording)?;
    let local_speaker_name = transcript.local_speaker_name.clone().unwrap_or_else(|| settings.local_speaker_name.clone());
    let labels = crate::diarization::read_completed_speaker_labels(root, &settings.extra_library_roots, &recording, &transcript);
    let turns: Vec<SpeakerTurn> = transcript
        .segments
        .iter()
        .enumerate()
        .map(|(i, segment)| {
            let remote_speaker = labels.as_ref().and_then(|l| l.get(i).copied().flatten());
            let speaker_label = presentation_label(&recording, segment, &local_speaker_name, remote_speaker);
            SpeakerTurn { speaker_label, start_ms: segment.start_ms, end_ms: segment.end_ms, text: segment.text.clone() }
        })
        .collect();
    let summary = crate::summary::read_completed_summary(root, &settings.extra_library_roots, recording_id).map(|(_, output)| output);
    let title = recording.meeting_title.clone().unwrap_or_else(|| "Meeting".into());
    let started_at_rfc3339 = Local
        .timestamp_millis_opt(recording.started_at as i64)
        .single()
        .unwrap_or_else(Local::now)
        .with_timezone(&chrono::Utc)
        .to_rfc3339();
    Ok((recording, MeetingExportData { title, started_at_rfc3339, summary, turns }))
}

#[cfg(test)]
#[path = "appflowy_tests.rs"]
mod tests;
