mod audio_devices;
mod capture_meter;
mod detection;
mod diarization;
mod echo_cancel;
mod echo_dedup;
mod diarization_engine;
mod diarization_store;
mod diarization_types;
#[cfg(target_os = "linux")]
mod linux_pulse;
#[cfg(test)]
mod meeting_management_tests;
mod meeting_signals;
mod meeting_title;
mod ollama_client;
mod recording;
mod recording_paths;
mod recording_store;
mod recording_wav;
mod speaker_alignment;
mod store;
mod summary;
mod summary_engine;
mod summary_store;
mod summary_types;
mod system_output;
mod transcription;
mod transcription_engine;
mod transcription_store;
mod transcription_types;

use audio_devices::{
    AudioDeviceDirection, AudioDeviceInventory, AudioInventorySnapshot, CpalAudioDeviceInventory,
};
use detection::{
    DetectionService, DetectionSettings, MeetingDetector, RecordingBoundary, SignalSnapshot,
};
use diarization::DiarizationService;
use diarization_store::DiarizationStore;
use meeting_signals::{MeetingDetector as NativeDetectorTrait, NativeMeetingDetector};
use recording::{RecordingService, RecordingState};
use recording_paths::{library_directory, resolved_recording_directory};
use recording_store::RecordingStore;
use serde::Serialize;
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use store::{AudioSourceDirection, AudioSourceSelection, Settings, SettingsStorage};
use summary::{SummaryService, SummarySettings};
use summary_engine::SummaryEngine;
use summary_store::SummaryStore;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_single_instance::init as single_instance;
use transcription::{try_processing_guard, TranscriptionService, TranscriptionSettings};
use transcription_store::TranscriptionStore;

pub fn run_diarization_worker(
    request: &std::path::Path,
    result: &std::path::Path,
) -> Result<(), String> {
    diarization_engine::run_worker(request, result)
}

#[derive(Default, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Lifecycle {
    #[default]
    Idle,
    Detecting,
    Recording,
    Processing,
    Failed,
}
#[derive(Default)]
struct TrayStatus(Mutex<TrayAvailability>);
/// 0 permits a new shutdown attempt, 1 is finalizing recordings, and 2 lets
/// Tauri's final ExitRequested event through.
#[derive(Clone)]
struct ExitGate(Arc<AtomicU8>);
const EXIT_NORMAL: u8 = 0;
const EXIT_SHUTTING_DOWN: u8 = 1;
const EXIT_ALLOWED: u8 = 2;
#[derive(Clone)]
struct TrayControls {
    tray: tauri::tray::TrayIcon,
    start: MenuItem<tauri::Wry>,
    stop: MenuItem<tauri::Wry>,
}
#[derive(Clone)]
struct AudioScanGate(Arc<Mutex<()>>);
#[derive(Default)]
struct ModelImportGate(Mutex<()>);
impl Default for AudioScanGate {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(())))
    }
}
#[derive(Default, Clone)]
struct TrayAvailability {
    available: bool,
    error: Option<String>,
}
#[derive(Serialize)]
struct AppState {
    status: Lifecycle,
    recording_supported: bool,
    detection_supported: bool,
    appflowy_connected: bool,
    recordings_folder: String,
    tray_available: bool,
    tray_error: Option<String>,
}
#[derive(Serialize)]
struct ImportOutcome {
    #[serde(flatten)]
    info: transcription_types::ModelInfo,
    /// Set when the saved Whisper model setting was auto-switched to the size just
    /// imported, because the previously selected size was not available.
    switched_to: Option<String>,
}
fn whisper_model_key(model: &store::WhisperModel) -> &'static str {
    match model {
        store::WhisperModel::Tiny => "tiny",
        store::WhisperModel::Base => "base",
        store::WhisperModel::Small => "small",
        store::WhisperModel::Medium => "medium",
        store::WhisperModel::Large => "large",
        store::WhisperModel::Turbo => "turbo",
    }
}
fn whisper_model_from_key(key: &str) -> Option<store::WhisperModel> {
    Some(match key {
        "tiny" => store::WhisperModel::Tiny,
        "base" => store::WhisperModel::Base,
        "small" => store::WhisperModel::Small,
        "medium" => store::WhisperModel::Medium,
        "large" => store::WhisperModel::Large,
        "turbo" => store::WhisperModel::Turbo,
        _ => return None,
    })
}
/// A user who imports a model just wants it to work: if their currently selected
/// size isn't imported, switch the saved setting to the size just imported. Never
/// overrides a selection that is already satisfied.
fn switch_to_imported_model_if_unavailable(
    settings: &Mutex<SettingsStorage>,
    transcription: &TranscriptionService,
    imported_key: &str,
) -> Result<Option<String>, String> {
    let Some(new_model) = whisper_model_from_key(imported_key) else {
        return Ok(None);
    };
    let mut storage = settings.lock().map_err(app_error)?;
    let mut current = storage.load().map_err(app_error)?;
    if transcription.has_model(whisper_model_key(&current.whisper_model))? {
        return Ok(None);
    }
    current.whisper_model = new_model;
    storage.save(current).map_err(app_error)?;
    Ok(Some(imported_key.to_string()))
}

fn app_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn recordings_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(app_error)?
        .join("recordings");
    Ok(path)
}
fn recordings_library(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    library_directory(&recordings_dir(app)?)
}
struct NativeDetectorAdapter(NativeMeetingDetector);
impl MeetingDetector for NativeDetectorAdapter {
    fn sample(&mut self) -> SignalSnapshot {
        let s = NativeDetectorTrait::sample(&mut self.0);
        SignalSnapshot {
            supported: s.supported,
            meeting: s.meeting,
            detail: s.detail,
            error: s.error,
        }
    }
}
fn detection_settings(app: &tauri::AppHandle) -> Result<DetectionSettings, String> {
    let settings = app
        .state::<Mutex<SettingsStorage>>()
        .lock()
        .map_err(app_error)?
        .load()
        .map_err(app_error)?;
    Ok(DetectionSettings::new(
        settings.auto_record,
        settings.start_debounce_seconds,
        settings.end_grace_seconds,
    ))
}
struct AppRecordingBoundary(tauri::AppHandle);
impl RecordingBoundary for AppRecordingBoundary {
    fn active_id(&self) -> Option<String> {
        self.0
            .state::<RecordingService>()
            .state()
            .active
            .map(|recording| recording.recording_id)
    }
    fn start(&self) -> Result<String, String> {
        let sources = self
            .0
            .state::<Mutex<SettingsStorage>>()
            .lock()
            .map_err(app_error)?
            .load()
            .map_err(app_error)?
            .audio_sources;
        self.0.state::<RecordingService>().start(sources)?;
        self.active_id().ok_or_else(|| {
            "Recording stopped before startup could be confirmed; check recent recordings.".into()
        })
    }
    fn stop(&self, expected_id: &str) -> Result<(), String> {
        match self.active_id() {
            Some(active) if active == expected_id => self.0.state::<RecordingService>().stop(),
            Some(_) => Err("The recording changed; refusing to stop another session.".into()),
            None => Ok(()),
        }
    }
}

#[tauri::command]
fn get_settings(store: State<'_, Mutex<SettingsStorage>>) -> Result<Settings, String> {
    store.lock().map_err(app_error)?.load().map_err(app_error)
}
#[tauri::command]
fn save_settings(
    settings: Settings,
    store: State<'_, Mutex<SettingsStorage>>,
    summary: State<'_, SummaryService>,
) -> Result<Settings, String> {
    // General preferences must not be an escape hatch around validated source
    // selection. Keep the persisted selection until the dedicated command runs.
    let mut storage = store.lock().map_err(app_error)?;
    let existing = storage.load().map_err(app_error)?;
    let saved = storage.save(Settings {
        audio_sources: existing.audio_sources,
        ..settings
    }).map_err(app_error)?;
    // A user who just enabled summaries, or fixed the URL/model, expects a
    // waiting job to resume immediately rather than sit out its backoff.
    let _ = summary.wake_waiting();
    Ok(saved)
}
#[tauri::command]
async fn get_audio_devices(
    gate: State<'_, AudioScanGate>,
) -> Result<AudioInventorySnapshot, String> {
    let gate = gate.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        CpalAudioDeviceInventory.snapshot()
    })
    .await
    .map_err(app_error)
}
#[tauri::command]
async fn save_audio_selection(
    source_ids: Vec<String>,
    store: State<'_, Mutex<SettingsStorage>>,
    gate: State<'_, AudioScanGate>,
) -> Result<Settings, String> {
    let gate = gate.0.clone();
    let inventory = tauri::async_runtime::spawn_blocking(move || {
        let _guard = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        CpalAudioDeviceInventory.snapshot()
    })
    .await
    .map_err(app_error)?;
    let mut storage = store.lock().map_err(app_error)?;
    let current = storage.load().map_err(app_error)?;
    let sources = normalize_selection(&inventory, &current.audio_sources, source_ids)?;
    storage
        .save(Settings {
            audio_sources: sources,
            ..current
        })
        .map_err(app_error)
}
fn normalize_selection(
    inventory: &AudioInventorySnapshot,
    previous: &[AudioSourceSelection],
    source_ids: Vec<String>,
) -> Result<Vec<AudioSourceSelection>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut sources = Vec::with_capacity(source_ids.len());
    for id in source_ids {
        if !seen.insert(id.clone()) {
            return Err("Each audio source can only be selected once.".into());
        }
        if let Some(device) = inventory
            .devices
            .iter()
            .find(|device| device.id.as_deref() == Some(id.as_str()) && device.selectable)
        {
            sources.push(AudioSourceSelection {
                id,
                name: device.name.clone(),
                direction: match device.direction {
                    AudioDeviceDirection::Input => AudioSourceDirection::Input,
                    AudioDeviceDirection::Output => AudioSourceDirection::Output,
                },
            });
        } else if let Some(previous) = previous.iter().find(|source| source.id == id) {
            sources.push(previous.clone());
        } else {
            return Err("An audio source is no longer available. Refresh the device list and choose a listed source.".into());
        }
    }
    Ok(sources)
}
#[tauri::command]
async fn get_app_state(
    app: tauri::AppHandle,
    recording: State<'_, RecordingService>,
    tray: State<'_, TrayStatus>,
) -> Result<AppState, String> {
    let tray = tray.0.lock().map_err(app_error)?.clone();
    let detector = app.state::<DetectionService>().inner().clone();
    let detection = tauri::async_runtime::spawn_blocking(move || detector.state())
        .await
        .map_err(app_error)?;
    Ok(AppState {
        status: match recording.state().status.as_str() {
            "recording" => Lifecycle::Recording,
            "processing" => Lifecycle::Processing,
            "failed" => Lifecycle::Failed,
            _ if detection.enabled && detection.supported => Lifecycle::Detecting,
            _ => Lifecycle::Idle,
        },
        recording_supported: true,
        detection_supported: detection.supported,
        appflowy_connected: false,
        recordings_folder: recordings_library(&app)?.display().to_string(),
        tray_available: tray.available,
        tray_error: tray.error,
    })
}
#[tauri::command]
async fn get_detection_state(
    detection: State<'_, DetectionService>,
) -> Result<detection::DetectionState, String> {
    let service = detection.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.state())
        .await
        .map_err(app_error)
}
#[tauri::command]
async fn confirm_detected_recording(
    approval_id: String,
    detection: State<'_, DetectionService>,
) -> Result<(), String> {
    let service = detection.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.approve_pending(&approval_id))
        .await
        .map_err(app_error)?
}
#[tauri::command]
async fn dismiss_detected_recording(
    approval_id: String,
    detection: State<'_, DetectionService>,
) -> Result<(), String> {
    let service = detection.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.dismiss_pending(&approval_id))
        .await
        .map_err(app_error)?
}
#[tauri::command]
async fn get_recording_state(
    recording: State<'_, RecordingService>,
) -> Result<RecordingState, String> {
    Ok(recording.state())
}
#[tauri::command]
async fn get_transcription_state(
    app: tauri::AppHandle,
) -> Result<transcription::TranscriptionState, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<TranscriptionService>().state())
        .await
        .map_err(app_error)
}
#[tauri::command]
async fn import_whisper_model(app: tauri::AppHandle) -> Result<Option<ImportOutcome>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let gate = app.state::<ModelImportGate>();
        let _guard = gate
            .0
            .try_lock()
            .map_err(|_| "A model import is already open.".to_string())?;
        if app.state::<ExitGate>().0.load(Ordering::Acquire) != EXIT_NORMAL {
            return Err("The application is shutting down.".into());
        }
        let selected = app
            .dialog()
            .file()
            .set_title("Import a local Whisper GGML model")
            .add_filter("Whisper GGML model", &["bin"])
            .blocking_pick_file();
        let Some(selected) = selected else {
            return Ok(None);
        };
        let source = selected.into_path().map_err(app_error)?;
        let transcription = app.state::<TranscriptionService>();
        let info = transcription.import_model(source)?;
        let switched_to = switch_to_imported_model_if_unavailable(
            app.state::<Mutex<SettingsStorage>>().inner(),
            transcription.inner(),
            &info.key,
        )?;
        if switched_to.is_some() {
            transcription.repin_to_current_model()?;
        }
        Ok(Some(ImportOutcome { info, switched_to }))
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn retry_transcription(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_processing_guard()?;
        app.state::<TranscriptionService>().retry(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn transcribe_recording(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_processing_guard()?;
        app.state::<TranscriptionService>()
            .transcribe_recording(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn restart_transcription(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_processing_guard()?;
        let root = recordings_dir(&app)?;
        let speakers = DiarizationStore::open(&root.join("diarization.sqlite3"))?;
        if speakers
            .get(&recording_id)?
            .is_some_and(|job| job.status == "diarizing")
        {
            return Err("Speaker identification is in progress; wait for it to finish before restarting transcription.".into());
        }
        let summaries = SummaryStore::open(&root.join("summary.sqlite3"))?;
        if summaries
            .get(&recording_id)?
            .is_some_and(|job| job.status == "summarizing")
        {
            return Err("Summarizing is in progress; wait for it to finish before restarting transcription.".into());
        }
        app.state::<TranscriptionService>()
            .restart_transcription(&recording_id)?;
        speakers.remove_if_idle(&recording_id)?;
        summaries.remove_if_idle(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn rename_recording(
    recording_id: String,
    title: String,
    app: tauri::AppHandle,
    recording: State<'_, RecordingService>,
) -> Result<recording::Recording, String> {
    let service = recording.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_processing_guard()?;
        if matches!(service.state().status.as_str(), "recording" | "processing") {
            return Err("Stop the active recording before renaming it.".into());
        }
        let updated = RecordingStore::open(&recordings_dir(&app)?)?.rename(&recording_id, title)?;
        service.refresh_history()?;
        Ok(updated)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn delete_recording(
    recording_id: String,
    app: tauri::AppHandle,
    recording: State<'_, RecordingService>,
) -> Result<(), String> {
    let service = recording.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_processing_guard()?;
        if matches!(service.state().status.as_str(), "recording" | "processing") {
            return Err("Stop the active recording before deleting it.".into());
        }
        let root = recordings_dir(&app)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // Tombstone first: a crash after this point may leave artifacts behind, but
        // no worker will expose or process this recording again.
        RecordingStore::open(&root)?.tombstone(&recording_id, now)?;
        TranscriptionStore::open(&root.join("transcription.sqlite3"))?
            .remove_if_idle(&recording_id)?;
        DiarizationStore::open(&root.join("diarization.sqlite3"))?.remove_if_idle(&recording_id)?;
        SummaryStore::open(&root.join("summary.sqlite3"))?.remove_if_idle(&recording_id)?;
        service.refresh_history()
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn get_diarization_state(
    app: tauri::AppHandle,
) -> Result<diarization::DiarizationState, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<DiarizationService>().state())
        .await
        .map_err(app_error)
}
#[tauri::command]
async fn import_diarization_model(
    kind: String,
    app: tauri::AppHandle,
) -> Result<Option<transcription_types::ModelInfo>, String> {
    diarization::validate_kind(&kind)?;
    tauri::async_runtime::spawn_blocking(move || {
        let gate = app.state::<ModelImportGate>();
        let _guard = gate
            .0
            .try_lock()
            .map_err(|_| "A model import is already open.")?;
        if app.state::<ExitGate>().0.load(Ordering::Acquire) != EXIT_NORMAL {
            return Err("The application is shutting down.".into());
        }
        let selected = app
            .dialog()
            .file()
            .set_title(format!("Import a local {kind} ONNX model"))
            .add_filter("Speaker ONNX model", &["onnx"])
            .blocking_pick_file();
        let Some(selected) = selected else {
            return Ok(None);
        };
        app.state::<DiarizationService>()
            .import_model(&kind, selected.into_path().map_err(app_error)?)
            .map(Some)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn retry_diarization(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<DiarizationService>().retry(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn open_transcript(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = match app
            .state::<DiarizationService>()
            .preferred_transcript_path(&recording_id)?
        {
            Some(path) => path,
            None => app
                .state::<TranscriptionService>()
                .transcript_path(&recording_id)?,
        };
        open_file(&path)
    })
    .await
    .map_err(app_error)?
}
fn open_file(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path).spawn().map_err(app_error)?;
    Ok(())
}
#[tauri::command]
async fn get_summary_state(app: tauri::AppHandle) -> Result<summary::SummaryState, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<SummaryService>().state())
        .await
        .map_err(app_error)
}
#[tauri::command]
async fn list_summary_models(ollama_url: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        summary_engine::OllamaSummaryEngine::new().list_models(&ollama_url)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn test_ollama_connection(ollama_url: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        summary_engine::OllamaSummaryEngine::new().test_connection(&ollama_url)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn retry_summary(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<SummaryService>().retry(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn regenerate_summary(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<SummaryService>().regenerate(&recording_id)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn open_summary(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = app.state::<SummaryService>().summary_path(&recording_id)?;
        open_file(&path)
    })
    .await
    .map_err(app_error)?
}
#[tauri::command]
async fn start_recording(
    app: tauri::AppHandle,
    recording: State<'_, RecordingService>,
    detection: State<'_, DetectionService>,
) -> Result<RecordingState, String> {
    show(&app);
    let service = detection.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.manual_start())
        .await
        .map_err(app_error)??;
    Ok(recording.state())
}
#[tauri::command]
async fn quit_application(
    app: tauri::AppHandle,
    recording: State<'_, RecordingService>,
    exit_gate: State<'_, ExitGate>,
) -> Result<(), String> {
    request_exit(&app, recording.inner().clone(), exit_gate.inner());
    Ok(())
}
#[tauri::command]
async fn stop_recording(
    recording: State<'_, RecordingService>,
    detection: State<'_, DetectionService>,
) -> Result<RecordingState, String> {
    let service = detection.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.manual_stop())
        .await
        .map_err(app_error)??;
    Ok(recording.state())
}
fn open_folder(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map_err(app_error)?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map_err(app_error)?;
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map_err(app_error)?;
    Ok(())
}
#[tauri::command]
fn open_recordings_folder(app: tauri::AppHandle) -> Result<(), String> {
    open_folder(&recordings_library(&app)?)
}
#[tauri::command]
async fn open_recording_folder(recording_id: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let id = uuid::Uuid::parse_str(&recording_id).map_err(|_| "Invalid recording identity.")?;
        if id.to_string() != recording_id {
            return Err("Recording identity must be a canonical UUID.".into());
        }
        let root = recordings_dir(&app)?;
        let recording = RecordingStore::open(&root)?
            .get_by_id(&recording_id)?
            .ok_or("Recording metadata is unavailable.")?;
        let directory = resolved_recording_directory(&root, &recording)?;
        open_folder(&directory)
    })
    .await
    .map_err(app_error)?
}
fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn request_exit(app: &tauri::AppHandle, service: RecordingService, gate: &ExitGate) {
    if !try_begin_exit(&gate.0) {
        return;
    }
    let app = app.clone();
    let gate = gate.clone();
    // The state outlives the application event loop. Keep all blocking writer
    // finalization off Tauri's event/UI thread.
    thread::spawn(move || {
        if let Some(detection) = app.try_state::<DetectionService>() {
            detection.shutdown();
        }
        let result = service
            .shutdown()
            .and_then(|()| app.state::<TranscriptionService>().shutdown())
            .and_then(|()| app.state::<DiarizationService>().shutdown())
            .and_then(|()| app.state::<SummaryService>().shutdown());
        match result {
            Ok(()) => {
                gate.0.store(EXIT_ALLOWED, Ordering::Release);
                app.exit(0);
            }
            Err(error) => {
                gate.0.store(EXIT_NORMAL, Ordering::Release);
                show(&app);
                let _ = app.emit(
                    "recording-failed",
                    format!("Could not finish recording before exit: {error}"),
                );
            }
        }
    });
}
fn try_begin_exit(gate: &AtomicU8) -> bool {
    gate.compare_exchange(
        EXIT_NORMAL,
        EXIT_SHUTTING_DOWN,
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .is_ok()
}

fn start_tray_status_updater(app: tauri::AppHandle) {
    thread::spawn(move || {
        let idle_icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png")).ok();
        let active_icon = idle_icon.as_ref().map(|icon| {
            let mut pixels = icon.rgba().to_vec();
            // Recolor the teal background; keep the original crisp microphone silhouette.
            for pixel in pixels.as_chunks_mut::<4>().0 {
                if pixel[1] > pixel[0].saturating_add(25) && pixel[0] < 100 {
                    pixel[0] = 190;
                    pixel[1] = 48;
                    pixel[2] = 55;
                }
            }
            tauri::image::Image::new_owned(pixels, icon.width(), icon.height())
        });
        let mut previous = String::new();
        loop {
            let state = app.state::<RecordingService>().state();
            let active = state.active.is_some();
            let key = format!("{}:{active}", state.status);
            if key != previous {
                previous = key;
                if let Some(controls) = app.try_state::<TrayControls>() {
                    let recording = active || state.status == "recording";
                    let processing = state.status == "processing";
                    let _ = controls.start.set_enabled(!recording && !processing);
                    let _ = controls.stop.set_enabled(recording);
                    let tooltip = if recording {
                        "Meeting Recorder — Recording"
                    } else if processing {
                        "Meeting Recorder — Saving recording"
                    } else {
                        "Meeting Recorder — Ready"
                    };
                    let _ = controls.tray.set_tooltip(Some(tooltip));
                    let _ = controls.tray.set_icon(if recording {
                        active_icon.clone()
                    } else {
                        idle_icon.clone()
                    });
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
    });
}

fn install_tray(app: &tauri::App) -> Result<TrayControls, tauri::Error> {
    let open = MenuItem::with_id(app, "open", "Open application", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", "Start recording", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Stop recording", false, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "Open recordings folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &start, &stop, &settings, &folder, &quit])?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;
    let tray = TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show(app),
            "settings" => {
                show(app);
                let _ = app.emit("navigate-settings", ());
            }
            "start" => {
                show(app);
                let app = app.clone();
                std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
                    match app.state::<DetectionService>().manual_start() {
                        Ok(()) => {
                            let _ = app.emit("recording-started", ());
                        }
                        Err(error) => {
                            let _ = app.emit("recording-failed", error);
                        }
                    }
                }));
            }
            "stop" => {
                let app = app.clone();
                std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
                    if let Err(error) = app.state::<DetectionService>().manual_stop() {
                        let _ = app.emit("recording-failed", error);
                    }
                }));
            }
            "folder" => {
                let app = app.clone();
                std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
                    if let Err(error) = open_recordings_folder(app.clone()) {
                        let _ = app.emit("recording-failed", error);
                    }
                }));
            }
            "quit" => {
                let service = app.state::<RecordingService>().inner().clone();
                let gate = app.state::<ExitGate>();
                request_exit(app, service, gate.inner());
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(TrayControls { tray, start, stop })
}

pub fn run() {
    let app = tauri::Builder::default().plugin(single_instance(|app, _, _| show(app))).plugin(tauri_plugin_dialog::init()).setup(|app| {
    let database = app.path().app_config_dir()?.join("settings.sqlite3");
    let recording_dir = recordings_dir(app.handle()).map_err(std::io::Error::other)?;
    app.manage(Mutex::new(SettingsStorage::new(database))); app.manage(RecordingService::new(recording_dir.clone())); app.manage(TrayStatus::default()); app.manage(AudioScanGate::default()); app.manage(ExitGate(Arc::new(AtomicU8::new(EXIT_NORMAL))));
    let detection_app = app.handle().clone();
    let prompt_app = app.handle().clone();
    let detection = DetectionService::new(recording_dir.join("detection.sqlite3"), NativeDetectorAdapter(NativeMeetingDetector::new()), Arc::new(move || detection_settings(&detection_app)), Arc::new(AppRecordingBoundary(app.handle().clone())), Arc::new(move |snapshot| {
        show(&prompt_app);
        let _ = prompt_app.emit("detection-state", snapshot);
    }));
    app.manage(detection);
    app.manage(ModelImportGate::default());
    let transcription_app = app.handle().clone();
    app.manage(TranscriptionService::new(recording_dir.clone(), Arc::new(move || {
        let settings = transcription_app.state::<Mutex<SettingsStorage>>().lock().map_err(app_error)?.load().map_err(app_error)?;
        Ok(TranscriptionSettings {
            model_key: whisper_model_key(&settings.whisper_model).into(),
            language: settings.transcription_language.clone(),
            echo_cancellation: settings.echo_cancellation,
            dedupe_echo_segments: settings.dedupe_echo_segments,
        })
    }), Arc::new(transcription_engine::LocalWhisperEngine::new())));
    let speaker_app = app.handle().clone();
    app.manage(DiarizationService::new(recording_dir.clone(), Arc::new(move || {
        Ok(speaker_app.state::<Mutex<SettingsStorage>>().lock().map_err(app_error)?.load().map_err(app_error)?.diarization_enabled)
    }), Arc::new(diarization_engine::LocalDiarizationEngine::new())));
    let summary_app = app.handle().clone();
    app.manage(SummaryService::new(recording_dir.clone(), Arc::new(move || {
        let settings = summary_app.state::<Mutex<SettingsStorage>>().lock().map_err(app_error)?.load().map_err(app_error)?;
        Ok(SummarySettings { enabled: settings.summary_enabled, ollama_url: settings.ollama_url, model: settings.summary_model })
    }), Arc::new(summary_engine::OllamaSummaryEngine::new())));
    let availability = match install_tray(app) {
        Ok(controls) => {
            app.manage(controls);
            start_tray_status_updater(app.handle().clone());
            TrayAvailability { available: true, error: None }
        },
        Err(error) => TrayAvailability { available: false, error: Some(format!("System tray is unavailable. Closing this window will exit the app. ({error})")) },
    };
    *app.state::<TrayStatus>().0.lock().map_err(|_| std::io::Error::other("Tray status lock poisoned"))? = availability;
    Ok(())
}).on_window_event(|window, event| {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        let tray_available = window.state::<TrayStatus>().0.lock().map(|status| status.available).unwrap_or(false);
        #[cfg(not(target_os = "linux"))]
        if tray_available {
            api.prevent_close();
            let _ = window.hide();
            return;
        }
        // Keep the window alive until background finalization has succeeded;
        // a failed shutdown returns the user to a visible, retryable app.
        api.prevent_close();
        let app = window.app_handle().clone();
        let service = app.state::<RecordingService>().inner().clone();
        let gate = app.state::<ExitGate>();
        request_exit(&app, service, gate.inner());
    }
}).invoke_handler(tauri::generate_handler![get_settings, save_settings, get_audio_devices, save_audio_selection, get_app_state, get_recording_state, get_detection_state, get_transcription_state, import_whisper_model, retry_transcription, transcribe_recording, restart_transcription, rename_recording, delete_recording, get_diarization_state, import_diarization_model, retry_diarization, open_transcript, get_summary_state, list_summary_models, test_ollama_connection, retry_summary, regenerate_summary, open_summary, confirm_detected_recording, dismiss_detected_recording, start_recording, stop_recording, open_recordings_folder, open_recording_folder, quit_application]).build(tauri::generate_context!()).expect("failed to build Meeting Recorder");
    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            let gate = handle.state::<ExitGate>();
            if gate.0.load(Ordering::Acquire) != EXIT_ALLOWED {
                api.prevent_exit();
                request_exit(
                    handle,
                    handle.state::<RecordingService>().inner().clone(),
                    gate.inner(),
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_normalization_rejects_duplicates_and_retains_missing_in_order() {
        let inventory = audio_devices::fixture_snapshot();
        let previous = vec![AudioSourceSelection {
            id: "gone".into(),
            name: "Unplugged mic".into(),
            direction: AudioSourceDirection::Input,
        }];
        assert!(normalize_selection(
            &inventory,
            &previous,
            vec!["input-a".into(), "input-a".into()]
        )
        .is_err());
        assert!(normalize_selection(&inventory, &previous, vec!["unknown".into()]).is_err());
        let selected =
            normalize_selection(&inventory, &previous, vec!["gone".into(), "input-a".into()])
                .unwrap();
        assert_eq!(selected[0].id, "gone");
        assert_eq!(selected[1].name, "Desk microphone");
    }
    #[test]
    fn exit_gate_allows_one_finalization_attempt_and_can_retry_after_error() {
        let gate = AtomicU8::new(EXIT_NORMAL);
        assert!(try_begin_exit(&gate));
        assert!(!try_begin_exit(&gate));
        gate.store(EXIT_NORMAL, Ordering::Release);
        assert!(try_begin_exit(&gate));
        gate.store(EXIT_ALLOWED, Ordering::Release);
        assert!(!try_begin_exit(&gate));
    }
    fn lib_test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("meeting-lib-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn synthetic_base_model(root: &std::path::Path) -> std::path::PathBuf {
        let mut header = [0u8; 48];
        header[..4].copy_from_slice(&0x67676d6cu32.to_le_bytes());
        header[20..24].copy_from_slice(&6i32.to_le_bytes());
        header[28..32].copy_from_slice(&512i32.to_le_bytes());
        header[36..40].copy_from_slice(&6i32.to_le_bytes());
        let path = root.join("synthetic-base-header.bin");
        std::fs::write(&path, header).unwrap();
        path
    }
    #[test]
    fn importing_a_model_switches_an_unavailable_selected_setting() {
        let root = lib_test_root();
        let settings = Mutex::new(SettingsStorage::new(root.join("settings.sqlite3")));
        {
            let mut storage = settings.lock().unwrap();
            let mut current = storage.load().unwrap();
            current.whisper_model = store::WhisperModel::Small;
            storage.save(current).unwrap();
        }
        let service = TranscriptionService::with_inhibition(
            root.clone(),
            Arc::new(|| Ok(TranscriptionSettings::for_model("small"))),
            Arc::new(transcription_engine::LocalWhisperEngine::new()),
            true,
        );
        // "small" has not been imported: switching to the just-imported "base" fixes it.
        let switched =
            switch_to_imported_model_if_unavailable(&settings, &service, "base").unwrap();
        assert_eq!(switched, Some("base".into()));
        assert_eq!(
            settings.lock().unwrap().load().unwrap().whisper_model,
            store::WhisperModel::Base
        );
        service.shutdown().unwrap();
    }
    #[test]
    fn switch_is_a_no_op_when_the_selected_model_is_already_available() {
        let root = lib_test_root();
        let settings = Mutex::new(SettingsStorage::new(root.join("settings.sqlite3")));
        let service = TranscriptionService::with_inhibition(
            root.clone(),
            Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
            Arc::new(transcription_engine::LocalWhisperEngine::new()),
            true,
        );
        service.import_model(synthetic_base_model(&root)).unwrap();
        let switched =
            switch_to_imported_model_if_unavailable(&settings, &service, "base").unwrap();
        assert_eq!(switched, None);
        assert_eq!(
            settings.lock().unwrap().load().unwrap().whisper_model,
            store::WhisperModel::Base
        );
        service.shutdown().unwrap();
    }
}
