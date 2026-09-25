//! Local IPC transport between the native-messaging host process
//! (`meeting-recorder --native-messaging-host`, spawned by the browser) and
//! the already-running app process. No network socket is used on any
//! platform. Messages are newline-delimited JSON `RawMessage` values
//! (see `browser_bridge.rs`); each line is validated and rate-limited
//! independently by the server, so a compromised or buggy host process gains
//! nothing beyond what the wire schema already allows.

use crate::browser_bridge::{validate, BrowserBridgeState, RateLimiter, RawMessage};
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

/// Fixed local endpoint name. On Windows this becomes a named pipe path; on
/// Unix it is resolved to a socket file under the caller-supplied directory
/// (the app's own data directory in production, a temp directory in tests).
pub const ENDPOINT_NAME: &str = "meetingrecorder-browser-bridge";

const PER_CONNECTION_RATE_LIMIT: usize = 20;
const PER_CONNECTION_RATE_WINDOW: Duration = Duration::from_secs(10);
const MAX_LINE_BYTES: usize = 8 * 1024;

/// Reads and applies newline-delimited `RawMessage`s from one connection
/// until it closes. Shared by every platform's server-side accept loop.
fn serve_connection<R: BufRead>(mut reader: R, state: &BrowserBridgeState) {
    let mut limiter = RateLimiter::new(PER_CONNECTION_RATE_LIMIT, PER_CONNECTION_RATE_WINDOW);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line);
        match read {
            Ok(0) => break, // clean EOF
            Ok(n) if n > MAX_LINE_BYTES => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !limiter.allow(Instant::now()) {
            continue; // drop silently; the connection stays open
        }
        let Ok(raw) = serde_json::from_str::<RawMessage>(trimmed) else {
            continue;
        };
        if let Ok(observation) = validate(raw) {
            state.record(observation);
        }
    }
}

/// Encodes one already-validated observation as the newline-delimited JSON
/// line the server expects. Used by the native-messaging host (the IPC
/// client) after it validates a browser message.
pub fn encode_line(raw: &RawMessage) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(raw)?;
    s.push('\n');
    Ok(s)
}

#[cfg(windows)]
pub mod platform {
    use super::*;
    use std::fs::File;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    fn pipe_path(name: &str) -> Vec<u16> {
        format!(r"\\.\pipe\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A security descriptor granting full access to the pipe's creator
    /// (owner) only -- no other local user, admin group, or "Everyone"
    /// entry -- restricting the pipe to the current OS user, as required for
    /// a local trust boundary between the browser and this app.
    fn owner_only_security_attributes() -> windows::core::Result<SECURITY_ATTRIBUTES> {
        // "D:(A;;GA;;;OW)" = DACL, Allow Generic-All to the Owner SID.
        // Windows copies this string into its own heap-allocated descriptor,
        // so the local `sddl` buffer need not outlive this call.
        let sddl: Vec<u16> = "D:(A;;GA;;;OW)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut psd = windows::Win32::Security::PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?;
        }
        Ok(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            // Intentionally leaked for the process lifetime (LocalAlloc'd by
            // Windows inside ConvertStringSecurityDescriptorToSecurityDescriptorW);
            // the pipe server loop runs for as long as the process does.
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        })
    }

    /// Accepts connections forever, handling each on its own thread. Runs on
    /// a dedicated background thread started by `browser_bridge::spawn_server`.
    pub fn run_server(name: &str, state: BrowserBridgeState) {
        let wide_path = pipe_path(name);
        loop {
            let Ok(security_attributes) = owner_only_security_attributes() else {
                return; // Could not build the ACL; refuse to open an unrestricted pipe.
            };
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(wide_path.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    Some(&security_attributes),
                )
            };
            if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
                return;
            }
            let connected = unsafe { ConnectNamedPipe(handle, None) };
            if connected.is_err() {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                continue;
            }
            let state = state.clone();
            // HANDLE (a raw *mut c_void) is not Send; move it across as an
            // isize and reconstruct it on the worker thread instead.
            let raw_handle = handle.0 as isize;
            std::thread::spawn(move || {
                let file = unsafe { File::from_raw_handle(raw_handle as *mut _) };
                serve_connection(BufReader::new(file), &state);
            });
        }
    }

    /// Connects as a client (used by the native-messaging host process) and
    /// returns a writable handle. Retries briefly: the app may not have
    /// finished starting its server yet.
    pub fn connect_client(name: &str) -> Option<File> {
        let wide_path = pipe_path(name);
        for attempt in 0..10 {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide_path.as_ptr()),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    Some(HANDLE::default()),
                )
            };
            if let Ok(handle) = handle {
                if !handle.is_invalid() {
                    return Some(unsafe { File::from_raw_handle(handle.0 as *mut _) });
                }
            }
            if attempt < 9 {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        None
    }
}

#[cfg(not(windows))]
pub mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    fn socket_path(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.sock"))
    }

    /// Accepts connections forever under `dir` (the app's own data
    /// directory in production; a temp directory in tests), handling each on
    /// its own thread. The socket file is created with 0600 permissions --
    /// only the owning user can connect.
    pub fn run_server_at(dir: &Path, name: &str, state: BrowserBridgeState) {
        let path = socket_path(dir, name);
        let _ = std::fs::remove_file(&path); // clear a stale socket from a prior crash
        let Ok(listener) = UnixListener::bind(&path) else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        for stream in listener.incoming().flatten() {
            let state = state.clone();
            std::thread::spawn(move || {
                serve_connection(BufReader::new(stream), &state);
            });
        }
    }

    pub fn connect_client_at(dir: &Path, name: &str) -> Option<UnixStream> {
        let path = socket_path(dir, name);
        for attempt in 0..10 {
            if let Ok(stream) = UnixStream::connect(&path) {
                return Some(stream);
            }
            if attempt < 9 {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "browser_bridge_ipc_tests.rs"]
mod tests;
