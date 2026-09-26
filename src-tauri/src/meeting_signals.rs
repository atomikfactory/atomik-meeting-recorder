//! Local, read-only meeting hints for the detection controller.
//!
//! This module never opens an audio stream, camera, browser profile, or network
//! connection. Windows-only COM work runs on this detector's worker thread.

use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SignalSnapshot {
    pub supported: bool,
    /// Stable provider key, never a title, process ID, or other transient value.
    pub meeting: Option<String>,
    pub detail: String,
    /// `Some` means the observation is incomplete and must not be treated as absence.
    pub error: Option<String>,
}

pub trait MeetingDetector {
    fn sample(&mut self) -> SignalSnapshot;
}

pub struct NativeMeetingDetector {
    #[cfg(target_os = "windows")]
    requests: Option<std::sync::mpsc::SyncSender<WorkerRequest>>,
}

impl NativeMeetingDetector {
    /// Starts the dedicated worker only. In particular, this does not initialize
    /// COM or touch audio devices on the caller/UI thread.
    pub fn new() -> Self {
        #[cfg(target_os = "windows")]
        {
            let (requests, receiver) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::Builder::new()
                .name("meeting-signal-worker".into())
                .spawn(move || worker_loop(receiver))
                .ok();
            // Failure remains observable as an unknown sample instead of panicking
            // during application setup.
            Self {
                requests: worker.map(|_| requests),
            }
        }
        #[cfg(not(target_os = "windows"))]
        Self {}
    }
}

impl Default for NativeMeetingDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingDetector for NativeMeetingDetector {
    fn sample(&mut self) -> SignalSnapshot {
        #[cfg(target_os = "windows")]
        {
            let (reply, response) = std::sync::mpsc::sync_channel(1);
            let Some(requests) = &self.requests else {
                return unknown("Native meeting detection worker could not start.");
            };
            if requests.try_send(WorkerRequest::Sample(reply)).is_err() {
                return unknown("Native meeting detection worker is busy or unavailable.");
            }
            response.recv_timeout(std::time::Duration::from_millis(750)).unwrap_or_else(|_| {
                unknown("Native meeting detection did not finish within its bounded sampling interval.")
            })
        }
        #[cfg(not(target_os = "windows"))]
        unsupported()
    }
}

#[cfg(not(target_os = "windows"))]
fn unsupported() -> SignalSnapshot {
    SignalSnapshot {
        supported: false,
        meeting: None,
        detail: "Native meeting detection is currently supported only on Windows.".into(),
        error: None,
    }
}

#[cfg(target_os = "windows")]
fn unknown(message: impl Into<String>) -> SignalSnapshot {
    SignalSnapshot {
        supported: cfg!(target_os = "windows"),
        meeting: None,
        detail: "Native meeting detection could not complete this sample.".into(),
        error: Some(message.into()),
    }
}

#[cfg(target_os = "windows")]
enum WorkerRequest {
    Sample(std::sync::mpsc::SyncSender<SignalSnapshot>),
}

#[cfg(target_os = "windows")]
fn worker_loop(receiver: std::sync::mpsc::Receiver<WorkerRequest>) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    // The COM apartment and all COM interface lifetimes stay on this worker.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };
    while let Ok(WorkerRequest::Sample(reply)) = receiver.recv() {
        let snapshot = match &initialized {
            Ok(()) => sample_windows(),
            Err(error) => unknown(format!("Windows COM initialization failed: {error}")),
        };
        let _ = reply.send(snapshot);
    }
    if initialized.is_ok() {
        unsafe { CoUninitialize() };
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessObservation {
    executable: String,
    has_meet_window: bool,
}

#[cfg(target_os = "windows")]
fn sample_windows() -> SignalSnapshot {
    match enumerate_active_capture_processes().and_then(|mut processes| {
        annotate_visible_meet_windows(&mut processes)?;
        Ok(classify(&processes))
    }) {
        Ok(snapshot) => snapshot,
        Err(error) => unknown(error),
    }
}

#[cfg(target_os = "windows")]
fn classify(processes: &[ProcessObservation]) -> SignalSnapshot {
    if processes
        .iter()
        .any(|process| process.executable == "slack.exe")
    {
        return SignalSnapshot {
            supported: true,
            meeting: Some("slack".into()),
            detail: "An active microphone session is attributed to Slack.".into(),
            error: None,
        };
    }
    if processes
        .iter()
        .any(|process| process.executable == "chrome.exe" && process.has_meet_window)
    {
        return SignalSnapshot {
            supported: true,
            meeting: Some("google_meet_chrome".into()),
            detail:
                "An active Chrome microphone session coincides with a visible Google Meet window."
                    .into(),
            error: None,
        };
    }
    if processes
        .iter()
        .any(|process| process.executable == "firefox.exe" && process.has_meet_window)
    {
        return SignalSnapshot {
            supported: true,
            meeting: Some("google_meet_firefox".into()),
            detail:
                "An active Firefox microphone session coincides with a visible Google Meet window."
                    .into(),
            error: None,
        };
    }
    SignalSnapshot {
        supported: true,
        meeting: None,
        detail: "No supported meeting hint is currently visible.".into(),
        error: None,
    }
}

#[cfg(target_os = "windows")]
fn enumerate_active_capture_processes() -> Result<Vec<ProcessObservation>, String> {
    use std::collections::HashSet;
    use windows::{
        core::Interface,
        Win32::{
            Media::Audio::{
                eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
                IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
            },
            System::Com::{CoCreateInstance, CLSCTX_ALL},
        },
    };

    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|error| {
            format!("Could not create the Windows audio-device enumerator: {error}")
        })?
    };
    let devices = unsafe {
        enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .map_err(|error| format!("Could not enumerate active microphone endpoints: {error}"))?
    };
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let count = unsafe { devices.GetCount() }
        .map_err(|error| format!("Could not count active microphone endpoints: {error}"))?;
    for device_index in 0..count {
        let device = unsafe { devices.Item(device_index) }
            .map_err(|error| format!("Could not read an active microphone endpoint: {error}"))?;
        let manager: IAudioSessionManager2 =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|error| {
                format!("Could not inspect a microphone endpoint's sessions: {error}")
            })?;
        let sessions = unsafe { manager.GetSessionEnumerator() }
            .map_err(|error| format!("Could not enumerate microphone sessions: {error}"))?;
        let session_count = unsafe { sessions.GetCount() }
            .map_err(|error| format!("Could not count microphone sessions: {error}"))?;
        for session_index in 0..session_count {
            let control = unsafe { sessions.GetSession(session_index) }
                .map_err(|error| format!("Could not inspect a microphone session: {error}"))?;
            if unsafe { control.GetState() }
                .map_err(|error| format!("Could not read a microphone session state: {error}"))?
                != AudioSessionStateActive
            {
                continue;
            }
            let control: IAudioSessionControl2 = control
                .cast()
                .map_err(|error| format!("Could not attribute a microphone session: {error}"))?;
            let process_id = unsafe { control.GetProcessId() }
                .map_err(|error| format!("Could not read a microphone session owner: {error}"))?;
            // Do not allow this recorder's own input monitoring/recording to become a hint.
            if process_id == 0 || process_id == std::process::id() || !seen.insert(process_id) {
                continue;
            }
            result.push(ProcessObservation {
                executable: executable_name(process_id)?,
                has_meet_window: false,
            });
        }
    }
    Ok(result)
}

#[cfg(target_os = "windows")]
fn executable_name(process_id: u32) -> Result<String, String> {
    use windows::{
        core::PWSTR,
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map_err(|error| {
            format!("Could not inspect an active microphone-session process: {error}")
        })?;
    let mut path = [0u16; 32_768];
    let mut length = path.len() as u32;
    let query = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    query.map_err(|error| {
        format!("Could not read an active microphone-session executable: {error}")
    })?;
    let full_path = String::from_utf16_lossy(&path[..length as usize]);
    full_path
        .rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
        .ok_or_else(|| "Could not normalize an active microphone-session executable.".into())
}

#[cfg(target_os = "windows")]
fn annotate_visible_meet_windows(processes: &mut [ProcessObservation]) -> Result<(), String> {
    use std::collections::HashMap;
    use windows::{
        core::BOOL,
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{
                EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
            },
        },
    };

    let active_browser_families: HashMap<String, bool> = processes
        .iter()
        .filter_map(|process| {
            matches!(process.executable.as_str(), "chrome.exe" | "firefox.exe")
                .then_some((process.executable.clone(), false))
        })
        .collect();
    if active_browser_families.is_empty() {
        return Ok(());
    }
    struct WindowScan {
        active_browser_families: HashMap<String, bool>,
        error: Option<String>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let scan = unsafe { &mut *(lparam.0 as *mut WindowScan) };
        if unsafe { IsWindowVisible(hwnd) } == false {
            return true.into();
        }
        let mut title = [0u16; 512];
        let length = unsafe { GetWindowTextW(hwnd, &mut title) };
        if length <= 0
            || !is_google_meet_title(&String::from_utf16_lossy(&title[..length as usize]))
        {
            return true.into();
        }
        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        match executable_name(process_id) {
            Ok(executable) => {
                if let Some(meets) = scan.active_browser_families.get_mut(&executable) {
                    *meets = true;
                }
            }
            Err(error) => scan.error = Some(error),
        }
        true.into()
    }
    let mut scan = WindowScan {
        active_browser_families,
        error: None,
    };
    unsafe { EnumWindows(Some(visit), LPARAM((&mut scan as *mut WindowScan) as isize)) }
        .map_err(|error| format!("Could not enumerate visible application windows: {error}"))?;
    if let Some(error) = scan.error {
        return Err(error);
    }
    for process in processes {
        process.has_meet_window = scan
            .active_browser_families
            .get(&process.executable)
            .copied()
            .unwrap_or(false);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn is_google_meet_title(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    // The microphone session establishes browser media use; this title check adds
    // a separate, visible Google Meet signal. Keep the phrases intentionally narrow.
    title.contains("google meet") || title.starts_with("meet - ")
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn classifier_requires_both_browser_mic_and_meet_window() {
        let browser_only = [ProcessObservation {
            executable: "chrome.exe".into(),
            has_meet_window: false,
        }];
        assert_eq!(classify(&browser_only).meeting, None);
        let meet = [ProcessObservation {
            executable: "firefox.exe".into(),
            has_meet_window: true,
        }];
        assert_eq!(
            classify(&meet).meeting.as_deref(),
            Some("google_meet_firefox")
        );
    }

    #[test]
    fn slack_microphone_hint_wins_over_browser_hint() {
        let observations = [
            ProcessObservation {
                executable: "chrome.exe".into(),
                has_meet_window: true,
            },
            ProcessObservation {
                executable: "slack.exe".into(),
                has_meet_window: false,
            },
        ];
        assert_eq!(classify(&observations).meeting.as_deref(), Some("slack"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "read-only native sample; run explicitly on a Windows desktop"]
    fn native_sample_is_normalized_and_never_exposes_process_details() {
        let mut detector = NativeMeetingDetector::new();
        let snapshot = detector.sample();
        println!("Normalized read-only detection sample: {snapshot:?}");
        assert!(snapshot.supported);
        assert!(
            snapshot.error.is_none(),
            "Native sampling failed: {:?}",
            snapshot.error
        );
        assert!(snapshot.detail.len() < 256);
        assert!(snapshot.error.as_deref().unwrap_or("").len() < 512);
        assert!(!snapshot.detail.contains('\\'));
        assert!(snapshot.meeting.as_deref().is_none_or(|key| {
            matches!(key, "slack" | "google_meet_chrome" | "google_meet_firefox")
        }));
    }
}
