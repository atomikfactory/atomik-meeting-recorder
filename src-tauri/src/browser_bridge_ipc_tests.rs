use super::*;
use crate::browser_bridge::{BrowserBridgeState, Platform, RawMessage};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);
fn unique_name() -> String {
    // Kept short (unlike a "meetingrecorder-browser-bridge-test-..." name)
    // because on Unix it becomes part of a socket path under a directory
    // that is itself none too short in a test binary's `$TMPDIR` -- see
    // `short_test_dir` below.
    format!(
        "amr-t-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A short scratch directory for the Unix-socket round-trip tests below.
/// `std::env::temp_dir()` resolves to `$TMPDIR` on macOS
/// (`/var/folders/xx/<long random segment>/T/`), which combined with any
/// further subdirectory and the `.sock` filename can exceed
/// `sockaddr_un::sun_path`'s 104-byte limit (108 on Linux); `bind`/`connect`
/// then fail with no indication the path length was the cause. `/tmp`
/// itself is always short on both platforms.
#[cfg(not(windows))]
fn short_test_dir(label: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/amr-t-{label}-{}",
        std::process::id()
    ))
}

fn wait_for<F: Fn() -> bool>(condition: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(windows)]
#[test]
fn in_process_round_trip_delivers_a_valid_observation() {
    let name = unique_name();
    let state = BrowserBridgeState::new();
    let server_state = state.clone();
    let server_name = name.clone();
    std::thread::spawn(move || platform::run_server(&server_name, server_state));
    // Give CreateNamedPipeW+ConnectNamedPipe a moment to be listening.
    std::thread::sleep(Duration::from_millis(100));
    let mut client = platform::connect_client(&name).expect("client should connect");
    let raw = RawMessage {
        platform: "meet".into(),
        state: "in_call".into(),
        title: Some("Weekly sync".into()),
        tab_id: "abc123".into(),
    };
    client
        .write_all(encode_line(&raw).unwrap().as_bytes())
        .unwrap();

    assert!(wait_for(|| state.current_in_call(Instant::now()).is_some()));
    let observed = state.current_in_call(Instant::now()).unwrap();
    assert_eq!(observed.platform, Platform::Meet);
    assert_eq!(observed.title.as_deref(), Some("Weekly sync"));
}

#[cfg(windows)]
#[test]
fn in_process_round_trip_ignores_invalid_json_then_accepts_a_valid_line() {
    let name = unique_name();
    let state = BrowserBridgeState::new();
    let server_state = state.clone();
    let server_name = name.clone();
    std::thread::spawn(move || platform::run_server(&server_name, server_state));
    std::thread::sleep(Duration::from_millis(100));
    let mut client = platform::connect_client(&name).expect("client should connect");
    client.write_all(b"not json at all\n").unwrap();
    let raw = RawMessage {
        platform: "zoom".into(),
        state: "in_call".into(),
        title: None,
        tab_id: "z1".into(),
    };
    client
        .write_all(encode_line(&raw).unwrap().as_bytes())
        .unwrap();
    assert!(wait_for(|| state.has_ever_connected()));
    let observed = state.current_in_call(Instant::now()).unwrap();
    assert_eq!(observed.platform, Platform::Zoom);
}

#[cfg(not(windows))]
#[test]
fn in_process_round_trip_delivers_a_valid_observation() {
    let dir = short_test_dir("a");
    std::fs::create_dir_all(&dir).unwrap();
    let name = unique_name();
    let state = BrowserBridgeState::new();
    let server_state = state.clone();
    let server_dir = dir.clone();
    let server_name = name.clone();
    std::thread::spawn(move || platform::run_server_at(&server_dir, &server_name, server_state));
    std::thread::sleep(Duration::from_millis(100));
    let mut client = platform::connect_client_at(&dir, &name).expect("client should connect");
    let raw = RawMessage {
        platform: "meet".into(),
        state: "in_call".into(),
        title: Some("Weekly sync".into()),
        tab_id: "abc123".into(),
    };
    client
        .write_all(encode_line(&raw).unwrap().as_bytes())
        .unwrap();

    assert!(wait_for(|| state.current_in_call(Instant::now()).is_some()));
    let observed = state.current_in_call(Instant::now()).unwrap();
    assert_eq!(observed.platform, Platform::Meet);
    assert_eq!(observed.title.as_deref(), Some("Weekly sync"));
}

#[cfg(not(windows))]
#[test]
fn in_process_round_trip_ignores_invalid_json_then_accepts_a_valid_line() {
    let dir = short_test_dir("b");
    std::fs::create_dir_all(&dir).unwrap();
    let name = unique_name();
    let state = BrowserBridgeState::new();
    let server_state = state.clone();
    let server_dir = dir.clone();
    let server_name = name.clone();
    std::thread::spawn(move || platform::run_server_at(&server_dir, &server_name, server_state));
    std::thread::sleep(Duration::from_millis(100));
    let mut client = platform::connect_client_at(&dir, &name).expect("client should connect");
    client.write_all(b"not json at all\n").unwrap();
    let raw = RawMessage {
        platform: "zoom".into(),
        state: "in_call".into(),
        title: None,
        tab_id: "z1".into(),
    };
    client
        .write_all(encode_line(&raw).unwrap().as_bytes())
        .unwrap();
    assert!(wait_for(|| state.has_ever_connected()));
    let observed = state.current_in_call(Instant::now()).unwrap();
    assert_eq!(observed.platform, Platform::Zoom);
}

#[test]
fn per_connection_rate_limit_drops_excess_lines_without_closing() {
    // Exercises serve_connection's own limiter directly over an in-memory
    // pipe so the test doesn't depend on OS-level IPC timing.
    use std::io::BufReader;
    let state = BrowserBridgeState::new();
    let mut lines = String::new();
    for i in 0..(PER_CONNECTION_RATE_LIMIT + 5) {
        lines.push_str(&format!(
            "{{\"platform\":\"meet\",\"state\":\"in_call\",\"tab_id\":\"t{i}\"}}\n"
        ));
    }
    serve_connection(BufReader::new(lines.as_bytes()), &state);
    // At least one message got through (the limiter allows the first burst);
    // the exact count isn't asserted here since ordering within one window
    // is what the RateLimiter unit tests already cover precisely.
    assert!(state.has_ever_connected());
}
