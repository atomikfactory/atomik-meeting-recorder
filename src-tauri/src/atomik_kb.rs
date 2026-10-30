//! Optional, durable atomik-kb export -- a sibling of `appflowy.rs`. Mirrors
//! its shape: a SQLite job journal (`atomik_kb_store.rs`), a serial background
//! worker, and retry/backoff for an unreachable server. Every request goes
//! through `atomik_kb_client::AtomikKbClient`, the only type in this codebase
//! allowed to open a socket to atomik-kb, and that client can only create
//! pages, never update or delete anything (see its module docs and
//! `atomik_kb_allowlist_tests.rs`).
use crate::{
    appflowy::{diarization_settled, load_meeting_data, local_date_string, summary_settled},
    appflowy_credentials::SharedCredentialStore,
    atomik_kb_client::{self, AtomikKbClient, ClientError, Space},
    atomik_kb_credentials,
    atomik_kb_markdown::build_meeting_markdown,
    atomik_kb_store::{date_key, meeting_key, root_key, AtomikKbJob, AtomikKbStore},
    diarization_store::DiarizationStore,
    recording::Recording,
    recording_store::RecordingStore,
    summary_store::SummaryStore,
    transcription_store::TranscriptionStore,
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

/// Title of the top-level page the date pages live under.
pub const ROOT_PAGE_TITLE: &str = "My daily meetings";
/// Default atomik-kb space meetings are exported to.
pub const DEFAULT_SPACE_NAME: &str = "My Daily Meeting";

/// The subset of app settings the export worker needs on each tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomikKbSettings {
    pub enabled: bool,
    pub auto_export: bool,
    pub port: u16,
    pub space_name: String,
    pub diarization_enabled: bool,
    pub summary_enabled: bool,
    pub local_speaker_name: String,
    pub extra_library_roots: Vec<PathBuf>,
}
#[cfg(test)]
impl AtomikKbSettings {
    pub fn for_test(port: u16) -> Self {
        Self {
            enabled: true,
            auto_export: true,
            port,
            space_name: DEFAULT_SPACE_NAME.into(),
            diarization_enabled: false,
            summary_enabled: true,
            local_speaker_name: "You".into(),
            extra_library_roots: Vec::new(),
        }
    }
}
impl AtomikKbSettings {
    fn default_disabled() -> Self {
        Self {
            enabled: false,
            auto_export: false,
            port: atomik_kb_client::DEFAULT_PORT,
            space_name: DEFAULT_SPACE_NAME.into(),
            diarization_enabled: false,
            summary_enabled: true,
            local_speaker_name: "You".into(),
            extra_library_roots: Vec::new(),
        }
    }
}
type SettingsReader = Arc<dyn Fn() -> Result<AtomikKbSettings, String> + Send + Sync>;

#[derive(Serialize, Clone, Debug)]
pub struct JobSummary {
    pub recording_id: String,
    pub status: String,
    pub error: Option<String>,
    pub export_index: u32,
}
/// One row of the local create-only journal, surfaced read-only in Settings
/// as "Exported pages". This app itself never moves, renames, or deletes
/// anything it lists here.
#[derive(Serialize, Clone, Debug)]
pub struct PageInfo {
    pub key: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub page_id: Option<String>,
}
#[derive(Serialize)]
pub struct AtomikKbState {
    pub enabled: bool,
    pub auto_export: bool,
    /// A token is saved in the OS credential store (never the token itself).
    pub has_token: bool,
    pub port: u16,
    pub space_name: String,
    pub jobs: Vec<JobSummary>,
    /// Set by `MEETING_RECORDER_DISABLE_ATOMIK_KB=1` (used by this crate's own
    /// verification harnesses); the worker never runs while this is true.
    pub inhibited: bool,
    pub error: Option<String>,
}
/// Result of Settings' "Test connection": initialize + list_spaces only.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ConnectionTest {
    pub ok: bool,
    pub message: String,
    pub space_found: bool,
    pub writable: bool,
}

pub struct AtomikKbService {
    store: Option<Arc<AtomikKbStore>>,
    settings: SettingsReader,
    credentials: SharedCredentialStore,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    error: Arc<Mutex<Option<String>>>,
    inhibited: bool,
}
impl AtomikKbService {
    pub fn new(
        root: PathBuf,
        settings: SettingsReader,
        credentials: SharedCredentialStore,
    ) -> Self {
        let inhibited = std::env::var("MEETING_RECORDER_DISABLE_ATOMIK_KB")
            .ok()
            .as_deref()
            == Some("1");
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
        let store = match AtomikKbStore::open(&root.join("atomik_kb.sqlite3")) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                set_error(&error, Some(e));
                None
            }
        };
        let mut worker = None;
        if !inhibited {
            if let Some(store) = store.clone() {
                let (r, settings_reader, c, e, creds) = (
                    root.clone(),
                    settings.clone(),
                    cancel.clone(),
                    error.clone(),
                    credentials.clone(),
                );
                match thread::Builder::new().name("atomik-kb-export".into()).spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(err) = store.recover() {
                            set_error(&e, Some(err));
                            return;
                        }
                        while !c.load(Ordering::Acquire) {
                            let result = settings_reader().and_then(|settings| if settings.enabled { tick(&r, &store, &settings, &creds, &c) } else { Ok(()) });
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
                        set_error(&e, Some("atomik-kb export worker stopped unexpectedly. Restart to resume it.".into()));
                    }
                }) {
                    Ok(handle) => worker = Some(handle),
                    Err(e) => set_error(&error, Some(format!("Could not start atomik-kb export worker: {e}"))),
                }
            }
        }
        Self {
            store,
            settings,
            credentials,
            cancel,
            worker: Mutex::new(worker),
            error,
            inhibited,
        }
    }
    fn store(&self) -> Result<&AtomikKbStore, String> {
        self.store
            .as_deref()
            .ok_or_else(|| "atomik-kb export storage is unavailable.".into())
    }
    pub fn state(&self) -> AtomikKbState {
        let mut error = self.error.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let settings = (self.settings)().unwrap_or_else(|e| {
            error = Some(e);
            AtomikKbSettings::default_disabled()
        });
        let has_token = atomik_kb_credentials::has_token(&self.credentials).unwrap_or(false);
        let jobs = match self.store().and_then(|s| s.all_jobs()) {
            Ok(jobs) => jobs
                .into_iter()
                .map(|j| JobSummary {
                    recording_id: j.recording_id,
                    status: j.status,
                    error: j.error,
                    export_index: j.export_index,
                })
                .collect(),
            Err(e) => {
                error = Some(e);
                Vec::new()
            }
        };
        AtomikKbState {
            enabled: settings.enabled,
            auto_export: settings.auto_export,
            has_token,
            port: settings.port,
            space_name: settings.space_name,
            jobs,
            inhibited: self.inhibited,
            error,
        }
    }
    /// Saves the token to the OS credential store only (shape-checked first).
    pub fn save_token(&self, token: &str) -> Result<(), String> {
        atomik_kb_credentials::save_token(&self.credentials, token)
    }
    pub fn clear_token(&self) -> Result<(), String> {
        atomik_kb_credentials::clear_token(&self.credentials)
    }
    /// `initialize` + `list_spaces`, read-only: reports whether the configured
    /// space is shared with this client and writable, or the precise problem.
    /// `port`/`space_name` override the saved settings so the form's unsaved
    /// values can be tested before Save.
    pub fn test_connection(
        &self,
        port: Option<u16>,
        space_name: Option<String>,
    ) -> Result<ConnectionTest, String> {
        let settings = (self.settings)()?;
        let port = port.unwrap_or(settings.port);
        let space_name = space_name.unwrap_or(settings.space_name);
        atomik_kb_client::validate_port(port)?;
        atomik_kb_client::validate_space_name(&space_name)?;
        Ok(check_connection(port, &space_name, &self.credentials))
    }
    /// Read-only: every page this integration has ever planned/created, for
    /// the Settings "Exported pages" list.
    pub fn list_pages(&self) -> Result<Vec<PageInfo>, String> {
        Ok(self
            .store()?
            .all_pages()?
            .into_iter()
            .map(|p| PageInfo {
                key: p.key,
                kind: p.kind,
                name: p.name,
                status: p.status,
                page_id: p.remote_page_id,
            })
            .collect())
    }
    /// Manual per-meeting export, independent of the `auto_export` toggle
    /// (still requires `enabled`, checked here so the button gives an
    /// immediate, clear error).
    pub fn export_now(&self, recording_id: &str) -> Result<(), String> {
        if !(self.settings)()?.enabled {
            return Err("atomik-kb export is turned off in Settings.".into());
        }
        self.store()?.enqueue(recording_id)
    }
    /// Creates a brand-new meeting page rather than reusing the one from a
    /// previous export -- never modifies the prior page. Requires the
    /// caller to have already obtained the user's explicit confirmation.
    pub fn export_again(&self, recording_id: &str, confirm: bool) -> Result<(), String> {
        if !confirm {
            return Err("Exporting again requires confirmation.".into());
        }
        if !(self.settings)()?.enabled {
            return Err("atomik-kb export is turned off in Settings.".into());
        }
        self.store()?.export_again(recording_id)
    }
    pub fn retry(&self, recording_id: &str) -> Result<(), String> {
        self.store()?.retry(recording_id)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self
            .worker
            .lock()
            .map_err(|_| "atomik-kb worker lock failed.")?
            .take()
        {
            handle
                .join()
                .map_err(|_| "atomik-kb export worker could not shut down.")?;
        }
        Ok(())
    }
}
fn set_error(slot: &Mutex<Option<String>>, value: Option<String>) {
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = value;
}

/// Case-insensitive exact name match -- never partial, never "the first
/// space". When several spaces share the name, a writable one is preferred.
fn find_space<'a>(spaces: &'a [Space], name: &str) -> Option<&'a Space> {
    let wanted = name.trim().to_lowercase();
    let mut matches = spaces
        .iter()
        .filter(|s| s.name.trim().to_lowercase() == wanted);
    let first = matches.next()?;
    if first.writable {
        return Some(first);
    }
    matches.find(|s| s.writable).or(Some(first))
}
fn visible_space_names(spaces: &[Space]) -> String {
    if spaces.is_empty() {
        return "none".into();
    }
    spaces
        .iter()
        .take(10)
        .map(|s| format!("\"{}\"", s.name))
        .collect::<Vec<_>>()
        .join(", ")
}
fn space_not_found_message(name: &str, spaces: &[Space]) -> String {
    format!(
        "No space named \"{name}\" is shared with this client (spaces it can see: {}). Create it in atomik-kb and grant this client write access in Settings → MCP; this app never creates spaces and never exports to another one.",
        visible_space_names(spaces)
    )
}
fn space_read_only_message(name: &str) -> String {
    format!("The space \"{name}\" is read-only for this client. In atomik-kb: Settings → MCP, grant this client write access to it.")
}

fn check_connection(
    port: u16,
    space_name: &str,
    credentials: &SharedCredentialStore,
) -> ConnectionTest {
    let fail = |message: String| ConnectionTest {
        ok: false,
        message,
        space_found: false,
        writable: false,
    };
    let result = AtomikKbClient::new(port, credentials).and_then(|client| {
        client.initialize()?;
        client.list_spaces()
    });
    match result {
        Err(e) => fail(e.to_string()),
        Ok(spaces) => match find_space(&spaces, space_name) {
            None => ConnectionTest {
                ok: false,
                message: space_not_found_message(space_name, &spaces),
                space_found: false,
                writable: false,
            },
            Some(space) if !space.writable => ConnectionTest {
                ok: false,
                message: space_read_only_message(&space.name),
                space_found: true,
                writable: false,
            },
            Some(space) => ConnectionTest {
                ok: true,
                message: format!(
                    "Connected. The space \"{}\" was found and is writable.",
                    space.name
                ),
                space_found: true,
                writable: true,
            },
        },
    }
}

/// A recording is ready for export once its transcript is complete and (if
/// enabled) both diarization and summarizing have also finished, so speaker
/// labels and the summary are final before anything is sent.
fn is_ready_for_export(
    root: &Path,
    settings: &AtomikKbSettings,
    recording: &Recording,
) -> Result<bool, String> {
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

fn tick(
    root: &Path,
    store: &AtomikKbStore,
    settings: &AtomikKbSettings,
    credentials: &SharedCredentialStore,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if settings.auto_export {
        let recordings = RecordingStore::open(root)?;
        let transcripts = TranscriptionStore::open(&root.join("transcription.sqlite3"))?;
        for parent in transcripts
            .all()?
            .into_iter()
            .filter(|j| j.status == "completed")
        {
            let Some(recording) = recordings.get_by_id(&parent.recording_id)? else {
                continue;
            };
            if !recording.auto_transcribe
                || !matches!(recording.status.as_str(), "recorded" | "interrupted")
            {
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

/// `(message, transient)`. Transient: retried with backoff (atomik-kb closed,
/// locked or MCP off; 5xx). Permanent: surfaced for the user to fix and retry.
type ExportError = (String, bool);

fn classify_client_error(error: ClientError) -> ExportError {
    match error {
        ClientError::Transient(m) => (m, true),
        ClientError::Rejected(m) | ClientError::Unauthorized(m) => (m, false),
    }
}

/// The fixed inputs `resolve_page` needs for one root/date/meeting page,
/// bundled to keep the function's argument count reasonable.
struct PagePlan<'a> {
    space_id: &'a str,
    key: &'a str,
    kind: &'a str,
    parent_key: Option<&'a str>,
    parent_page_id: Option<&'a str>,
    name: &'a str,
    markdown: Option<&'a str>,
}

/// Resolves (finding or creating) one page in the root -> date -> meeting
/// chain. Read-before-write: a page already marked `created` in the local
/// journal is trusted outright; otherwise the parent's children are re-read
/// and an exact-title child is adopted (covers both a user's pre-existing page
/// and a crash after a successful remote create but before the journal was
/// updated) -- only creating a new page when none exists.
///
/// After creating a brand-new page (not when adopting an existing one -- that
/// page is already known-visible), the parent's children are re-read once more
/// to confirm atomik-kb actually lists the new page; if not, the job is failed
/// with a clear message instead of reporting "completed" for a page the user
/// cannot find. atomik-kb cuts a listing at 200 children, so when the parent is
/// that large the new page may legitimately fall past the cut; it is then
/// confirmed by listing the new page's own (empty) children instead, which
/// succeeds only if the page exists in that space.
fn resolve_page(
    client: &AtomikKbClient,
    store: &AtomikKbStore,
    plan: PagePlan,
) -> Result<String, ExportError> {
    let PagePlan {
        space_id,
        key,
        kind,
        parent_key,
        parent_page_id,
        name,
        markdown,
    } = plan;
    let record = store
        .find_or_plan(key, kind, parent_key, space_id, name)
        .map_err(|e| (e, false))?;
    if let Some(remote_id) = record.remote_page_id {
        return Ok(remote_id);
    }
    let existing = client
        .list_pages(space_id, parent_page_id)
        .map_err(classify_client_error)?;
    if let Some(found) = existing.child_titled(name) {
        let id = found.id.clone();
        // A page another journal row already owns (e.g. an earlier meeting
        // with the identical title and minute) is not this page's to adopt.
        if !store.is_page_recorded(&id).map_err(|e| (e, false))? {
            store
                .mark_created(&record.key, &id)
                .map_err(|e| (e, false))?;
            return Ok(id);
        }
    }
    let page_id = client
        .create_page(space_id, parent_page_id, name, markdown)
        .map_err(classify_client_error)?;
    let verify = client
        .list_pages(space_id, parent_page_id)
        .map_err(classify_client_error)?;
    let visible = verify.pages.iter().any(|p| p.id == page_id)
        || (verify.truncated && client.list_pages(space_id, Some(&page_id)).is_ok());
    if !visible {
        return Err((
            format!("atomik-kb accepted \"{name}\" but it is not visible under its parent afterward; the export was marked failed rather than reported as completed for a page you would not be able to find."),
            false,
        ));
    }
    store
        .mark_created(&record.key, &page_id)
        .map_err(|e| (e, false))?;
    Ok(page_id)
}

/// Resolves the configured space by case-insensitive exact name. Never
/// creates a space and never falls back to another one.
fn resolve_space(client: &AtomikKbClient, space_name: &str) -> Result<Space, ExportError> {
    let spaces = client.list_spaces().map_err(classify_client_error)?;
    match find_space(&spaces, space_name) {
        None => Err((space_not_found_message(space_name, &spaces), false)),
        Some(space) if !space.writable => Err((space_read_only_message(&space.name), false)),
        Some(space) => Ok(space.clone()),
    }
}

/// `HH:mm — Title` (local start time), with ` (2)`, ` (3)` ... for "Export
/// again". Normalised to atomik-kb's single-line form and cut so the whole
/// title, suffix included, fits the 200-character limit.
pub(crate) fn meeting_page_title(started_at_ms: u64, title: &str, export_index: u32) -> String {
    let local = Local
        .timestamp_millis_opt(started_at_ms as i64)
        .single()
        .unwrap_or_else(Local::now);
    let suffix = if export_index <= 1 {
        String::new()
    } else {
        format!(" ({export_index})")
    };
    let base = atomik_kb_client::one_line(&format!("{} — {}", local.format("%H:%M"), title));
    let room = atomik_kb_client::MAX_TITLE_CHARS - suffix.chars().count();
    let base = if base.chars().count() > room {
        let cut: String = base.chars().take(room - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        base
    };
    format!("{base}{suffix}")
}
fn local_start_label(started_at_ms: u64) -> String {
    Local
        .timestamp_millis_opt(started_at_ms as i64)
        .single()
        .unwrap_or_else(Local::now)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn export_one(
    root: &Path,
    store: &AtomikKbStore,
    settings: &AtomikKbSettings,
    credentials: &SharedCredentialStore,
    job: &AtomikKbJob,
) -> Result<(), ExportError> {
    atomik_kb_client::validate_port(settings.port).map_err(|e| (e, false))?;
    // Local data first: a broken checkpoint fails before any network I/O.
    let (recording, meeting) = load_meeting_data(
        root,
        &settings.extra_library_roots,
        &settings.local_speaker_name,
        &job.recording_id,
    )
    .map_err(|e| (e, false))?;
    let client = AtomikKbClient::new(settings.port, credentials).map_err(classify_client_error)?;
    client.initialize().map_err(classify_client_error)?;
    let space = resolve_space(&client, &settings.space_name)?;

    let root_key = root_key(&space.id);
    let root_page_id = resolve_page(
        &client,
        store,
        PagePlan {
            space_id: &space.id,
            key: &root_key,
            kind: "root",
            parent_key: None,
            parent_page_id: None,
            name: ROOT_PAGE_TITLE,
            markdown: None,
        },
    )?;
    let date = local_date_string(recording.started_at);
    let date_key = date_key(&root_page_id, &date);
    let date_page_id = resolve_page(
        &client,
        store,
        PagePlan {
            space_id: &space.id,
            key: &date_key,
            kind: "date",
            parent_key: Some(&root_key),
            parent_page_id: Some(&root_page_id),
            name: &date,
            markdown: None,
        },
    )?;
    let markdown = build_meeting_markdown(&meeting, &local_start_label(recording.started_at));
    let meeting_title = meeting_page_title(recording.started_at, &meeting.title, job.export_index);
    resolve_page(
        &client,
        store,
        PagePlan {
            space_id: &space.id,
            key: &meeting_key(&job.recording_id, job.export_index),
            kind: "meeting",
            parent_key: Some(&date_key),
            parent_page_id: Some(&date_page_id),
            name: &meeting_title,
            markdown: Some(&markdown),
        },
    )?;
    store.complete(&job.recording_id).map_err(|e| (e, false))
}

#[cfg(test)]
#[path = "atomik_kb_tests.rs"]
mod tests;
