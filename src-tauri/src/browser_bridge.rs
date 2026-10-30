//! Local-only bridge from the optional browser extension (`browser-extension/`)
//! to meeting detection. Two roles share this module:
//!
//! - The **host** role runs inside the *same executable*, invoked as
//!   `meeting-recorder --native-messaging-host` by the browser. It speaks
//!   Chrome/Firefox's native-messaging stdio protocol (a 4-byte little-endian
//!   length prefix followed by that many bytes of UTF-8 JSON) on stdin/stdout,
//!   validates and rate-limits each message, and forwards it to...
//! - The **server**, a background thread inside the main running app that
//!   listens on a local, per-user IPC channel (a Windows named pipe or a Unix
//!   domain socket) and updates `BrowserBridgeState`, which `detection.rs`
//!   consults each sample.
//!
//! No network socket is ever opened; both transports are local-only and
//! restricted to the current OS user. See docs/browser-extension.md.

use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Native messaging's own cap is 1 MiB; we need at most a few hundred bytes
/// per message, so anything beyond this is already a protocol violation.
pub const MAX_MESSAGE_BYTES: u32 = 8 * 1024;
const MAX_TITLE_LEN: usize = 200;
const MAX_TAB_ID_LEN: usize = 64;
/// One heartbeat every 15s from the extension; three missed heartbeats
/// (45s) before the app treats an "in_call" observation as stale and expires
/// it, in case the browser or the extension's tab disappeared without a
/// clean "ended" message (e.g. a browser crash).
pub const STALE_AFTER: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Meet,
    Teams,
    Zoom,
}
impl Platform {
    pub fn provider_key(self) -> &'static str {
        match self {
            Platform::Meet => "browser_meet",
            Platform::Teams => "browser_teams",
            Platform::Zoom => "browser_zoom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    InCall,
    Ended,
}

/// The wire schema. Unknown top-level fields are ignored (`serde`'s default
/// behavior without `deny_unknown_fields`) rather than rejected, so a newer
/// extension can add fields without breaking an older installed app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawMessage {
    pub platform: String,
    pub state: String,
    #[serde(default)]
    pub title: Option<String>,
    pub tab_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub platform: Platform,
    pub state: CallState,
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum ValidationError {
    UnknownPlatform(String),
    UnknownState(String),
    TitleTooLong,
    TabIdInvalid,
}
impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::UnknownPlatform(p) => write!(f, "unrecognized platform: {p}"),
            ValidationError::UnknownState(s) => write!(f, "unrecognized state: {s}"),
            ValidationError::TitleTooLong => write!(f, "title exceeds {MAX_TITLE_LEN} characters"),
            ValidationError::TabIdInvalid => write!(f, "tab_id is missing, empty, or too long"),
        }
    }
}

/// Validates and normalizes a raw, already-deserialized message. Separate
/// from JSON parsing so a malformed-but-parseable message (e.g. an unknown
/// platform) can be rejected without terminating the connection.
pub fn validate(raw: RawMessage) -> Result<Observation, ValidationError> {
    let platform = match raw.platform.as_str() {
        "meet" => Platform::Meet,
        "teams" => Platform::Teams,
        "zoom" => Platform::Zoom,
        other => return Err(ValidationError::UnknownPlatform(other.to_string())),
    };
    let state = match raw.state.as_str() {
        "in_call" => CallState::InCall,
        "ended" => CallState::Ended,
        other => return Err(ValidationError::UnknownState(other.to_string())),
    };
    if raw.tab_id.is_empty() || raw.tab_id.len() > MAX_TAB_ID_LEN {
        return Err(ValidationError::TabIdInvalid);
    }
    let title = match raw.title {
        Some(t) if t.chars().count() > MAX_TITLE_LEN => return Err(ValidationError::TitleTooLong),
        Some(t) if t.trim().is_empty() => None,
        other => other,
    };
    Ok(Observation {
        platform,
        state,
        title,
    })
}

// ---------------------------------------------------------------------
// Length-prefixed framing (native messaging's own stdio protocol)
// ---------------------------------------------------------------------

#[derive(Debug)]
pub enum FramingError {
    Eof,
    OversizedMessage(u32),
    Io(String),
}
impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::Eof => write!(f, "connection closed"),
            FramingError::OversizedMessage(len) => {
                write!(
                    f,
                    "message of {len} bytes exceeds the {MAX_MESSAGE_BYTES}-byte cap"
                )
            }
            FramingError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

/// Reads one length-prefixed message. Rejects an oversized length *before*
/// attempting to read the body, so a hostile or corrupt sender cannot force
/// an unbounded allocation.
pub fn read_message<R: Read>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FramingError::Eof),
        Err(e) => return Err(FramingError::Io(e.to_string())),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(FramingError::OversizedMessage(len));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => FramingError::Eof,
        _ => FramingError::Io(e.to_string()),
    })?;
    Ok(body)
}

/// Writes one length-prefixed message (host -> browser direction). Not
/// currently called in production -- the host is fire-and-forget toward the
/// extension -- but kept alongside `read_message` as the framing's other
/// half, exercised directly by `browser_bridge_tests.rs`'s round-trip test.
#[cfg(test)]
pub fn write_message<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), FramingError> {
    if body.len() > MAX_MESSAGE_BYTES as usize {
        return Err(FramingError::OversizedMessage(body.len() as u32));
    }
    writer
        .write_all(&(body.len() as u32).to_le_bytes())
        .map_err(|e| FramingError::Io(e.to_string()))?;
    writer
        .write_all(body)
        .map_err(|e| FramingError::Io(e.to_string()))?;
    writer.flush().map_err(|e| FramingError::Io(e.to_string()))
}

// ---------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------

/// A simple fixed-window limiter: at most `limit` accepted calls per
/// `window`. Generous enough for one heartbeat/15s across a handful of tabs,
/// tight enough to bound a misbehaving or hostile sender.
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    timestamps: VecDeque<Instant>,
}
impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            timestamps: VecDeque::new(),
        }
    }
    /// Returns true if this call is within the limit (and records it).
    pub fn allow(&mut self, now: Instant) -> bool {
        while let Some(&front) = self.timestamps.front() {
            if now.saturating_duration_since(front) > self.window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        if self.timestamps.len() >= self.limit {
            return false;
        }
        self.timestamps.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------
// Shared state consulted by detection.rs
// ---------------------------------------------------------------------

#[derive(Clone)]
struct StampedObservation {
    observation: Observation,
    at: Instant,
}
#[derive(Clone, Default)]
pub struct BrowserBridgeState(Arc<Mutex<Inner>>);
#[derive(Default)]
struct Inner {
    last: Option<StampedObservation>,
    ever_connected: bool,
    last_message_at: Option<Instant>,
}
impl BrowserBridgeState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record(&self, observation: Observation) {
        let mut inner = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        inner.ever_connected = true;
        inner.last_message_at = Some(now);
        inner.last = Some(StampedObservation {
            observation,
            at: now,
        });
    }
    /// `has_ever_connected` distinguishes "no browser extension has ever
    /// registered a signal" (native detection's own `supported` should
    /// stand) from "the extension is connected but currently sees no call"
    /// (this platform should report `supported: true` even where the native
    /// heuristic itself is unavailable, e.g. Linux -- see detection.rs).
    pub fn has_ever_connected(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ever_connected
    }
    pub fn last_message_at(&self) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_message_at
    }
    /// A fresh `InCall` observation, or `None` if there is none / it has
    /// gone stale (no heartbeat within `STALE_AFTER`).
    pub fn current_in_call(&self, now: Instant) -> Option<Observation> {
        let inner = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let stamped = inner.last.as_ref()?;
        if stamped.observation.state != CallState::InCall {
            return None;
        }
        if now.saturating_duration_since(stamped.at) > STALE_AFTER {
            return None;
        }
        Some(stamped.observation.clone())
    }
    /// The most recent title reported for a still-fresh in-call observation,
    /// used by `recording.rs`'s title provider to prefer the extension's
    /// title over the window-title heuristic.
    pub fn current_title(&self) -> Option<String> {
        self.current_in_call(Instant::now())?.title
    }
}

// ---------------------------------------------------------------------
// Wiring: the app-side server and the `--native-messaging-host` client
// ---------------------------------------------------------------------

/// Where the Unix-domain socket lives. Deliberately independent of Tauri's
/// app-data path (which the standalone `--native-messaging-host` process
/// invocation has no easy access to without booting a Tauri context): both
/// the server (started from `lib.rs::run`) and the host process (started
/// fresh by the browser) call this same function, so they always agree.
/// `XDG_RUNTIME_DIR` is already per-user and mode 0700 on every common
/// Linux desktop; it is the correct home for a short-lived local socket.
///
/// macOS never sets `XDG_RUNTIME_DIR`, and its `$TMPDIR`
/// (`/var/folders/xx/<long random segment>/T/`) is long enough that a
/// socket path built under it can exceed `sockaddr_un::sun_path`'s 104-byte
/// limit (108 on Linux) -- `bind`/`connect` then fail outright with no
/// indication the path was the problem. `short_private_socket_dir` is a
/// short, fixed, per-user fallback that avoids this regardless of `$TMPDIR`.
#[cfg(not(windows))]
pub fn default_socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(dir) = short_private_socket_dir() {
        return dir;
    }
    PathBuf::from("/tmp")
}

/// A short (well under any `sun_path` limit), per-user, 0700 directory under
/// `/tmp` used when `XDG_RUNTIME_DIR` is unset -- always true on macOS, and
/// the fallback path when a Linux session manager doesn't set it either.
/// `/tmp` is world-writable, so another local user could otherwise
/// pre-create `dir` (as a symlink elsewhere, or a directory they own) ahead
/// of us; refuse to reuse anything that isn't already exactly ours with
/// exactly these permissions rather than silently trusting or widening it.
#[cfg(not(windows))]
fn short_private_socket_dir() -> Option<std::path::PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = std::path::PathBuf::from(format!("/tmp/atomik-mr-{uid}"));
    ensure_private_dir(&dir, uid).then_some(dir)
}

/// Creates `dir` mode 0700 if it doesn't exist, then confirms it is a
/// directory owned by `uid` with exactly mode 0700 (whether just-created or
/// pre-existing). Returns `false` -- refusing to use `dir` -- for anything
/// else: owned by another user, wider permissions, a symlink, a plain file,
/// or a directory that could not be created. Split out from
/// `short_private_socket_dir` so tests can point it at a scratch path
/// instead of the real per-uid `/tmp` directory.
#[cfg(not(windows))]
fn ensure_private_dir(dir: &std::path::Path, uid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let is_exclusively_ours = |meta: &std::fs::Metadata| {
        meta.is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o777 == 0o700
    };
    if std::fs::symlink_metadata(dir).is_err() {
        if std::fs::create_dir(dir).is_err() {
            return false;
        }
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // Always re-check (rather than trusting a fresh `create_dir`): another
    // process could have raced us between creation and the permission
    // change, or the path could have pre-existed under another user's
    // control (this directory lives under the world-writable `/tmp`).
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => is_exclusively_ours(&meta),
        Err(_) => false,
    }
}

/// Starts the background thread that accepts connections from the
/// native-messaging host process and feeds validated observations into
/// `state`. Best-effort: a failure here disables the browser signal source
/// but never the rest of the app (mirrors `DetectionService::new`'s own
/// tolerance of a failed worker thread).
pub fn spawn_server(state: BrowserBridgeState) {
    let result = std::thread::Builder::new()
        .name("browser-bridge-server".into())
        .spawn(move || {
            #[cfg(windows)]
            crate::browser_bridge_ipc::platform::run_server(
                crate::browser_bridge_ipc::ENDPOINT_NAME,
                state,
            );
            #[cfg(not(windows))]
            crate::browser_bridge_ipc::platform::run_server_at(
                &default_socket_dir(),
                crate::browser_bridge_ipc::ENDPOINT_NAME,
                state,
            );
        });
    if let Err(error) = result {
        eprintln!("Could not start browser-bridge server thread: {error}");
    }
}

/// Entry point for `meeting-recorder --native-messaging-host`, invoked by
/// the browser per the registered host manifest. Reads native-messaging
/// framed JSON from stdin, validates and rate-limits it, and forwards valid
/// observations to the running app over local IPC. Returns a process exit
/// code (0 on a clean stdin close, matching what Chrome/Firefox expect when
/// the extension's port disconnects).
pub fn run_native_messaging_host() -> i32 {
    use crate::browser_bridge_ipc::{encode_line, platform, ENDPOINT_NAME};
    let mut limiter = RateLimiter::new(20, Duration::from_secs(10));
    let stdin = io::stdin();
    let mut input = stdin.lock();
    #[cfg(windows)]
    let mut client = platform::connect_client(ENDPOINT_NAME);
    #[cfg(not(windows))]
    let mut client = platform::connect_client_at(&default_socket_dir(), ENDPOINT_NAME);
    loop {
        let body = match read_message(&mut input) {
            Ok(body) => body,
            Err(FramingError::Eof) => return 0,
            Err(_) => return 1,
        };
        if !limiter.allow(Instant::now()) {
            continue;
        }
        let Ok(raw) = serde_json::from_slice::<RawMessage>(&body) else {
            continue;
        };
        if validate(raw.clone()).is_err() {
            continue;
        }
        if client.is_none() {
            #[cfg(windows)]
            {
                client = platform::connect_client(ENDPOINT_NAME);
            }
            #[cfg(not(windows))]
            {
                client = platform::connect_client_at(&default_socket_dir(), ENDPOINT_NAME);
            }
        }
        if let Some(stream) = client.as_mut() {
            let sent = encode_line(&raw)
                .ok()
                .is_some_and(|line| stream.write_all(line.as_bytes()).is_ok());
            if !sent {
                client = None;
            }
        }
        // No connected app is not fatal: keep draining stdin so the browser
        // does not see a broken pipe while the app is merely starting up.
    }
}

#[cfg(test)]
#[path = "browser_bridge_tests.rs"]
mod tests;
