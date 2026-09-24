//! Local meeting hints request consent; only a fresh, approved request can capture.
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct SignalSnapshot {
    pub supported: bool,
    pub meeting: Option<String>,
    pub detail: String,
    pub error: Option<String>,
}
pub trait MeetingDetector: Send {
    fn sample(&mut self) -> SignalSnapshot;
}
pub trait RecordingBoundary: Send + Sync {
    fn active_id(&self) -> Option<String>;
    fn start(&self) -> Result<String, String>;
    fn stop(&self, expected_id: &str) -> Result<(), String>;
}
#[derive(Clone, Copy, Debug)]
pub struct DetectionSettings {
    pub enabled: bool,
    pub start: Duration,
    pub grace: Duration,
}
impl DetectionSettings {
    pub fn new(enabled: bool, start: u16, grace: u16) -> Self {
        Self {
            enabled,
            start: Duration::from_secs(start.min(60) as u64),
            grace: Duration::from_secs(grace.clamp(5, 300) as u64),
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct DetectionState {
    pub supported: bool,
    pub enabled: bool,
    pub phase: String,
    pub provider: Option<String>,
    pub detail: String,
    pub error: Option<String>,
    pub seconds_remaining: Option<u64>,
    pub automation_inhibited: bool,
    pub approval_id: Option<String>,
}
impl Default for DetectionState {
    fn default() -> Self {
        Self {
            supported: false,
            enabled: false,
            phase: "waiting".into(),
            provider: None,
            detail: "Checking local meeting detection availability.".into(),
            error: None,
            seconds_remaining: None,
            automation_inhibited: false,
            approval_id: None,
        }
    }
}
struct DetectionJournal {
    conn: Connection,
}
impl DetectionJournal {
    fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS detection_latch (id INTEGER PRIMARY KEY CHECK(id=1), latched INTEGER NOT NULL CHECK(latched IN (0,1)));") .map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }
    fn latched(&self) -> Result<bool, String> {
        let value = self
            .conn
            .query_row("SELECT latched FROM detection_latch WHERE id=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("Invalid detection latch; saved data retained.".into()),
        }
    }
    fn set(&self, value: bool) -> Result<(), String> {
        self.conn.execute("INSERT INTO detection_latch(id,latched) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET latched=excluded.latched", params![value as i64]).map_err(|e| e.to_string())?;
        Ok(())
    }
}
// One lock serializes decisions, persistence and ALL manual/automatic effects.
struct Core {
    state: DetectionState,
    journal: Option<DetectionJournal>,
    journal_error: Option<String>,
    persisted_latch: bool,
    latched: bool,
    owned_id: Option<String>,
    operation_error: Option<String>,
    provider: Option<String>,
    present_since: Option<Duration>,
    absent_since: Option<Duration>,
    unknown_since: Option<Duration>,
    last_sample: Option<Duration>,
    approved: bool,
    pending_stop: bool,
}
impl Core {
    fn new(path: &Path, inhibited: bool) -> Self {
        let opened = DetectionJournal::open(path).and_then(|j| j.latched().map(|l| (j, l)));
        let (journal, latched, journal_error) = match opened {
            Ok((journal, latched)) => (Some(journal), latched, None),
            Err(e) => (
                None,
                true,
                Some(format!(
                    "Automatic requests disabled: detection journal unavailable: {e}"
                )),
            ),
        };
        Self {
            state: DetectionState {
                automation_inhibited: inhibited,
                ..Default::default()
            },
            journal,
            journal_error,
            persisted_latch: latched,
            latched,
            owned_id: None,
            operation_error: None,
            provider: None,
            present_since: None,
            absent_since: None,
            unknown_since: None,
            last_sample: None,
            approved: false,
            pending_stop: false,
        }
    }
    fn cancel_request(&mut self) {
        self.state.approval_id = None;
        self.approved = false;
    }
    fn reset_evidence(&mut self) {
        self.cancel_request();
        self.present_since = None;
        self.absent_since = None;
        self.unknown_since = None;
        self.provider = None;
    }
    fn persist(&mut self, value: bool) -> Result<(), String> {
        if let Some(error) = &self.journal_error {
            return Err(error.clone());
        }
        if self.persisted_latch == value {
            self.latched = value;
            return Ok(());
        }
        let result = self
            .journal
            .as_ref()
            .ok_or_else(|| "Detection journal unavailable".to_string())
            .and_then(|j| j.set(value));
        match result {
            Ok(()) => {
                self.persisted_latch = value;
                self.latched = value;
                Ok(())
            }
            Err(e) => {
                let error =
                    format!("Automatic requests disabled: could not save meeting latch: {e}");
                self.journal_error = Some(error.clone());
                self.latched = true;
                Err(error)
            }
        }
    }
    fn phase(&mut self, phase: &str, detail: impl Into<String>) {
        self.state.phase = phase.into();
        self.state.detail = detail.into();
    }
    fn stop_owned(&mut self, recorder: &dyn RecordingBoundary) -> bool {
        let Some(id) = self.owned_id.clone() else {
            self.pending_stop = false;
            return true;
        };
        if recorder.active_id().as_deref() != Some(&id) {
            self.owned_id = None;
            self.pending_stop = false;
            return true;
        }
        match recorder.stop(&id) {
            Ok(()) => {
                self.owned_id = None;
                self.pending_stop = false;
                true
            }
            Err(e) => {
                self.pending_stop = true;
                self.operation_error = Some(e.clone());
                self.state.error = Some(e);
                self.phase("error", "Could not stop the meeting recording. Retrying; manual Stop remains available.");
                false
            }
        }
    }
    /// Fake-clock friendly step. Returns true only for a newly created prompt.
    fn step(
        &mut self,
        now: Duration,
        mut sample: SignalSnapshot,
        settings: Result<DetectionSettings, String>,
        recorder: &dyn RecordingBoundary,
    ) -> bool {
        self.state.seconds_remaining = None;
        self.state.supported = sample.supported;
        self.state.provider = sample.meeting.clone();
        self.state.error = self.operation_error.clone();
        let gap = self
            .last_sample
            .is_some_and(|last| now.saturating_sub(last) > Duration::from_secs(5) || now < last);
        self.last_sample = Some(now);
        if gap {
            self.reset_evidence();
            if self.owned_id.is_some() {
                self.pending_stop = true;
                self.operation_error = Some(
                    "Meeting observations were interrupted; recording stopped conservatively."
                        .into(),
                );
            }
        }
        let active = recorder.active_id();
        if self.owned_id.is_some() && self.owned_id != active {
            self.owned_id = None;
            self.pending_stop = false;
            self.latched = true;
            self.operation_error = Some("The meeting recording ended unexpectedly. Check recent recordings before starting manually.".into());
        }
        if self.pending_stop && !self.stop_owned(recorder) {
            return false;
        }
        let configured = match settings {
            Ok(s) => s,
            Err(error) => {
                self.reset_evidence();
                self.state.enabled = false;
                self.stop_owned(recorder);
                self.state.error = Some(error);
                self.phase(
                    "error",
                    "Could not read detection settings. Automatic requests are paused.",
                );
                return false;
            }
        };
        self.state.enabled = configured.enabled && !self.state.automation_inhibited;
        if !self.state.enabled {
            self.reset_evidence();
            if !self.stop_owned(recorder) {
                return false;
            }
            self.phase(
                "disabled",
                if self.state.automation_inhibited {
                    "Meeting requests are disabled for this development test session."
                } else {
                    "Meeting detection is off. Start a recording manually whenever you need it."
                },
            );
            return false;
        }
        if let Some(error) = self.journal_error.clone() {
            self.reset_evidence();
            self.stop_owned(recorder);
            self.state.error = Some(error);
            self.phase("error", "Meeting requests are paused to preserve duplicate protection. Manual recording remains available.");
            return false;
        }
        if !sample.supported {
            self.reset_evidence();
            if !self.stop_owned(recorder) {
                return false;
            }
            self.phase(
                "unsupported",
                "Native meeting detection is not available on this platform. Use manual recording.",
            );
            return false;
        }
        if gap {
            sample.error =
                Some("Meeting observations were interrupted; waiting for fresh evidence.".into());
        }
        if let Some(error) = sample.error {
            self.cancel_request();
            self.present_since = None;
            self.absent_since = None;
            self.provider = None;
            let since = *self.unknown_since.get_or_insert(now);
            if now.saturating_sub(since) >= configured.grace && !self.stop_owned(recorder) {
                return false;
            }
            self.state.error = Some(error);
            self.phase(
                "error",
                "Meeting activity is unknown. No new recording can start until detection recovers.",
            );
            return false;
        }
        self.unknown_since = None;
        if let Some(provider) = sample.meeting {
            self.absent_since = None;
            if self.owned_id.is_some() {
                self.phase(
                    "recording",
                    "Recording this approved meeting. Stop is always available.",
                );
                return false;
            }
            if active.is_some() {
                self.cancel_request();
                self.phase(
                    "recording",
                    "Manual recording is active. Meeting detection will not stop it.",
                );
                return false;
            }
            if self.provider.as_ref() != Some(&provider) {
                self.cancel_request();
                self.present_since = Some(now);
                self.provider = Some(provider);
            }
            if self.latched {
                self.cancel_request();
                self.state.error = self.operation_error.clone();
                self.phase("suppressed", "This meeting has already been handled. Waiting for it to end before asking again.");
                return false;
            }
            if self.state.approval_id.is_some() {
                if self.approved {
                    self.cancel_request();
                    if let Err(error) = self.persist(true) {
                        self.state.error = Some(error);
                        self.phase(
                            "error",
                            "Could not safely save recording intent. No recording was started.",
                        );
                        return false;
                    }
                    match recorder.start() {
                        Ok(id) => {
                            self.owned_id = Some(id);
                            self.operation_error = None;
                            self.phase(
                                "recording",
                                "Recording this approved meeting. Stop is always available.",
                            );
                        }
                        Err(error) => {
                            self.operation_error = Some(error.clone());
                            self.state.error = Some(error);
                            self.phase("suppressed", "Recording could not start. Fix the source or permission issue, then use manual Start.");
                        }
                    }
                } else {
                    self.phase(
                        "awaiting_approval",
                        format!("{} No audio is recorded until you approve.", sample.detail),
                    );
                }
                return false;
            }
            let since = *self.present_since.get_or_insert(now);
            let elapsed = now.saturating_sub(since);
            if elapsed >= configured.start {
                self.state.approval_id = Some(uuid::Uuid::new_v4().to_string());
                self.phase(
                    "awaiting_approval",
                    format!("{} No audio is recorded until you approve.", sample.detail),
                );
                return true;
            }
            self.phase("confirming", sample.detail);
            let remaining = configured.start.saturating_sub(elapsed);
            self.state.seconds_remaining =
                Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0));
            return false;
        }
        self.cancel_request();
        self.present_since = None;
        self.provider = None;
        let since = *self.absent_since.get_or_insert(now);
        let elapsed = now.saturating_sub(since);
        if elapsed >= configured.grace && self.latched {
            if !self.stop_owned(recorder) {
                return false;
            }
            if let Err(error) = self.persist(false) {
                self.state.error = Some(error);
                self.phase("error", "Could not rearm meeting detection safely.");
                return false;
            }
            self.operation_error = None;
            self.state.error = None;
        }
        if self.owned_id.is_some() {
            self.phase(
                "grace",
                "Meeting activity has disappeared. Waiting before ending this recording.",
            );
            let remaining = configured.grace.saturating_sub(elapsed);
            self.state.seconds_remaining =
                Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0));
        } else if recorder.active_id().is_some() {
            self.phase(
                "recording",
                "Manual recording is active. Meeting detection will not stop it.",
            );
        } else if self.latched {
            self.phase(
                "suppressed",
                "Waiting for a full quiet grace period before asking about another meeting.",
            );
        } else {
            self.phase(
                "waiting",
                "Waiting for likely meeting activity. Detection does not capture audio.",
            );
        }
        false
    }
    fn approve(&mut self, id: &str) -> Result<(), String> {
        if self.state.phase != "awaiting_approval"
            || self.state.approval_id.as_deref() != Some(id)
            || self.approved
        {
            return Err("This meeting approval has expired or was already handled.".into());
        }
        // Consumption occurs on the NEXT fresh, positive, same-provider sample.
        self.approved = true;
        self.phase(
            "approval_received",
            "Approval received. Rechecking meeting activity before opening audio devices.",
        );
        Ok(())
    }
    fn dismiss(&mut self, id: &str) -> Result<(), String> {
        if self.state.phase != "awaiting_approval" || self.state.approval_id.as_deref() != Some(id)
        {
            return Err("This meeting approval has expired.".into());
        }
        self.cancel_request();
        self.latched = true;
        self.phase(
            "suppressed",
            "You skipped this meeting. We will not ask again until it ends.",
        );
        self.persist(true)
    }
    fn manual_start(&mut self, recorder: &dyn RecordingBoundary) -> Result<(), String> {
        if recorder.active_id().is_some() {
            return Err("A recording is already in progress.".into());
        }
        self.reset_evidence();
        self.latched = true;
        // A broken detection journal must not prevent explicit manual capture.
        if !self.state.automation_inhibited {
            let _ = self.persist(true);
        }
        recorder.start()?;
        self.owned_id = None;
        self.operation_error = None;
        self.phase(
            "recording",
            "Manual recording is active. Meeting detection will not stop it.",
        );
        Ok(())
    }
    fn manual_stop(&mut self, recorder: &dyn RecordingBoundary) -> Result<(), String> {
        let id = recorder
            .active_id()
            .ok_or_else(|| "No recording is in progress.".to_string())?;
        self.reset_evidence();
        self.latched = true;
        // Stopping audio is never blocked by a detector-journal error.
        if !self.state.automation_inhibited {
            let _ = self.persist(true);
        }
        recorder.stop(&id)?;
        self.owned_id = None;
        self.pending_stop = false;
        self.phase(
            "suppressed",
            "Recording stopped. Waiting for this meeting to end before asking again.",
        );
        Ok(())
    }
}
type SettingsReader = Arc<dyn Fn() -> Result<DetectionSettings, String> + Send + Sync>;
#[derive(Clone)]
pub struct DetectionService {
    core: Arc<Mutex<Core>>,
    recorder: Arc<dyn RecordingBoundary>,
    stopped: Arc<AtomicBool>,
}
impl DetectionService {
    pub fn new<D: MeetingDetector + 'static>(
        path: PathBuf,
        mut detector: D,
        settings: SettingsReader,
        recorder: Arc<dyn RecordingBoundary>,
        on_prompt: Arc<dyn Fn(DetectionState) + Send + Sync>,
    ) -> Self {
        let inhibited = std::env::var("MEETING_RECORDER_DISABLE_AUTOMATION")
            .ok()
            .as_deref()
            == Some("1");
        let service = Self {
            core: Arc::new(Mutex::new(Core::new(&path, inhibited))),
            recorder,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let worker = service.clone();
        let result = thread::Builder::new()
            .name("meeting-detection".into())
            .spawn(move || {
                let epoch = Instant::now();
                while !worker.stopped.load(Ordering::Acquire) {
                    let sample = detector.sample();
                    {
                        let mut core = worker.core.lock().unwrap_or_else(|p| p.into_inner());
                        if worker.stopped.load(Ordering::Acquire) {
                            break;
                        }
                        if core.step(
                            epoch.elapsed(),
                            sample,
                            settings(),
                            worker.recorder.as_ref(),
                        ) {
                            on_prompt(core.state.clone());
                        }
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            });
        if let Err(error) = result {
            if let Ok(mut core) = service.core.lock() {
                core.journal_error = Some(format!("Could not start meeting detector: {error}"));
                core.state.error = core.journal_error.clone();
                core.phase(
                    "error",
                    "Meeting requests unavailable; manual recording remains available.",
                );
            }
        }
        service
    }
    pub fn state(&self) -> DetectionState {
        self.core
            .lock()
            .map(|c| c.state.clone())
            .unwrap_or_else(|_| DetectionState {
                phase: "error".into(),
                error: Some("Detection state lock failed.".into()),
                ..Default::default()
            })
    }
    pub fn approve_pending(&self, id: &str) -> Result<(), String> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| "Detection state lock failed")?;
        if self.stopped.load(Ordering::Acquire) {
            return Err("Application is shutting down.".into());
        }
        core.approve(id)
    }
    pub fn dismiss_pending(&self, id: &str) -> Result<(), String> {
        self.core
            .lock()
            .map_err(|_| "Detection state lock failed")?
            .dismiss(id)
    }
    pub fn manual_start(&self) -> Result<(), String> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| "Detection state lock failed")?;
        if self.stopped.load(Ordering::Acquire) {
            return Err("Application is shutting down.".into());
        }
        core.manual_start(self.recorder.as_ref())
    }
    pub fn manual_stop(&self) -> Result<(), String> {
        self.core
            .lock()
            .map_err(|_| "Detection state lock failed")?
            .manual_stop(self.recorder.as_ref())
    }
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        // Barrier waits for effects; the worker rechecks the flag under this lock.
        // The caller runs off the UI thread.
        if let Ok(mut core) = self.core.lock() {
            core.reset_evidence();
            core.state.enabled = false;
            core.phase(
                "disabled",
                "Meeting requests stopped for application shutdown.",
            );
        }
    }
}
#[cfg(test)]
#[path = "detection_tests.rs"]
mod tests;
