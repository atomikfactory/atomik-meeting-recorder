use super::*;
use std::io::Cursor;

// --- Framing -----------------------------------------------------------

#[test]
fn framing_round_trips_a_message() {
    let mut buf = Vec::new();
    write_message(&mut buf, b"{\"hello\":true}").unwrap();
    let mut cursor = Cursor::new(buf);
    let read = read_message(&mut cursor).unwrap();
    assert_eq!(read, b"{\"hello\":true}");
}

#[test]
fn framing_rejects_oversized_length_prefix_without_reading_body() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(MAX_MESSAGE_BYTES + 1).to_le_bytes());
    // No body at all -- an implementation that reads the length and then
    // tries to allocate/read the (nonexistent) body before checking the cap
    // would hang or error differently than expected.
    let mut cursor = Cursor::new(buf);
    match read_message(&mut cursor) {
        Err(FramingError::OversizedMessage(len)) => assert_eq!(len, MAX_MESSAGE_BYTES + 1),
        other => panic!("expected OversizedMessage, got {other:?}"),
    }
}

#[test]
fn framing_reports_eof_on_empty_stream() {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    assert!(matches!(read_message(&mut cursor), Err(FramingError::Eof)));
}

#[test]
fn framing_reports_eof_on_truncated_length_prefix() {
    let mut cursor = Cursor::new(vec![1u8, 2u8]); // only 2 of 4 length bytes
    assert!(matches!(read_message(&mut cursor), Err(FramingError::Eof)));
}

#[test]
fn framing_reports_eof_on_truncated_body() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(b"short"); // fewer than the declared 10 bytes
    let mut cursor = Cursor::new(buf);
    assert!(matches!(read_message(&mut cursor), Err(FramingError::Eof)));
}

#[test]
fn write_message_rejects_oversized_body() {
    let mut buf = Vec::new();
    let oversized = vec![0u8; MAX_MESSAGE_BYTES as usize + 1];
    assert!(matches!(
        write_message(&mut buf, &oversized),
        Err(FramingError::OversizedMessage(_))
    ));
}

// --- Schema validation ---------------------------------------------------

fn raw(platform: &str, state: &str, title: Option<&str>, tab_id: &str) -> RawMessage {
    RawMessage {
        platform: platform.into(),
        state: state.into(),
        title: title.map(String::from),
        tab_id: tab_id.into(),
    }
}

#[test]
fn validate_accepts_a_well_formed_meet_in_call_message() {
    let obs = validate(raw("meet", "in_call", Some("Weekly sync"), "abc123")).unwrap();
    assert_eq!(obs.platform, Platform::Meet);
    assert_eq!(obs.state, CallState::InCall);
    assert_eq!(obs.title.as_deref(), Some("Weekly sync"));
}

#[test]
fn validate_rejects_unknown_platform() {
    assert!(matches!(
        validate(raw("skype", "in_call", None, "abc")),
        Err(ValidationError::UnknownPlatform(_))
    ));
}

#[test]
fn validate_rejects_unknown_state() {
    assert!(matches!(
        validate(raw("meet", "on_hold", None, "abc")),
        Err(ValidationError::UnknownState(_))
    ));
}

#[test]
fn validate_rejects_empty_or_oversized_tab_id() {
    assert!(matches!(
        validate(raw("meet", "ended", None, "")),
        Err(ValidationError::TabIdInvalid)
    ));
    let long_id = "a".repeat(65);
    assert!(matches!(
        validate(raw("meet", "ended", None, &long_id)),
        Err(ValidationError::TabIdInvalid)
    ));
}

#[test]
fn validate_rejects_titles_over_the_length_cap() {
    let long_title = "x".repeat(MAX_TITLE_LEN + 1);
    assert!(matches!(
        validate(raw("meet", "in_call", Some(&long_title), "abc")),
        Err(ValidationError::TitleTooLong)
    ));
}

#[test]
fn validate_normalizes_a_blank_title_to_none() {
    let obs = validate(raw("meet", "in_call", Some("   "), "abc")).unwrap();
    assert_eq!(obs.title, None);
}

#[test]
fn unknown_json_fields_are_ignored_not_rejected() {
    // The extension may ship a newer field an older installed app doesn't
    // know about yet; the message must still parse.
    let json = r#"{"platform":"meet","state":"in_call","tab_id":"abc","extra_future_field":42}"#;
    let raw: RawMessage = serde_json::from_str(json).unwrap();
    assert!(validate(raw).is_ok());
}

#[test]
fn missing_required_field_fails_to_parse() {
    let json = r#"{"platform":"meet","state":"in_call"}"#; // no tab_id
    assert!(serde_json::from_str::<RawMessage>(json).is_err());
}

// --- Rate limiting -------------------------------------------------------

#[test]
fn rate_limiter_allows_up_to_the_limit_then_rejects() {
    let mut limiter = RateLimiter::new(3, Duration::from_secs(10));
    let now = Instant::now();
    assert!(limiter.allow(now));
    assert!(limiter.allow(now));
    assert!(limiter.allow(now));
    assert!(!limiter.allow(now));
}

#[test]
fn rate_limiter_recovers_once_the_window_elapses() {
    let mut limiter = RateLimiter::new(1, Duration::from_secs(10));
    let now = Instant::now();
    assert!(limiter.allow(now));
    assert!(!limiter.allow(now + Duration::from_secs(1)));
    assert!(limiter.allow(now + Duration::from_secs(11)));
}

// --- BrowserBridgeState (staleness / freshness) --------------------------

#[test]
fn state_reports_no_current_call_before_any_message() {
    let state = BrowserBridgeState::new();
    assert!(!state.has_ever_connected());
    assert!(state.current_in_call(Instant::now()).is_none());
}

#[test]
fn state_reports_a_fresh_in_call_observation() {
    let state = BrowserBridgeState::new();
    state.record(Observation {
        platform: Platform::Meet,
        state: CallState::InCall,
        title: Some("Standup".into()),
    });
    assert!(state.has_ever_connected());
    let now = Instant::now();
    let current = state.current_in_call(now).unwrap();
    assert_eq!(current.platform, Platform::Meet);
    assert_eq!(current.title.as_deref(), Some("Standup"));
}

#[test]
fn state_expires_a_stale_in_call_observation() {
    let state = BrowserBridgeState::new();
    state.record(Observation {
        platform: Platform::Meet,
        state: CallState::InCall,
        title: None,
    });
    let far_future = Instant::now() + STALE_AFTER + Duration::from_secs(1);
    assert!(state.current_in_call(far_future).is_none());
}

// sockaddr_un::sun_path is 104 bytes on macOS (108 on Linux), including the
// trailing NUL Rust adds when binding/connecting; leave headroom below the
// tighter macOS limit rather than cutting it exactly at either.
#[cfg(not(windows))]
const SUN_PATH_SAFE_LIMIT: usize = 100;

#[cfg(not(windows))]
#[test]
fn generated_socket_paths_stay_under_the_sun_path_limit() {
    // Mirrors browser_bridge_ipc::platform::socket_path's `dir.join(name +
    // ".sock")` using short_private_socket_dir() directly -- the fallback
    // `default_socket_dir()` takes on macOS, where `XDG_RUNTIME_DIR` is
    // never set -- rather than mutating the process-wide `XDG_RUNTIME_DIR`
    // env var, which parallel test threads also read.
    let dir =
        short_private_socket_dir().expect("short_private_socket_dir should succeed under /tmp");
    let path = dir.join(format!("{}.sock", crate::browser_bridge_ipc::ENDPOINT_NAME));
    let len = path.as_os_str().len();
    assert!(
        len < SUN_PATH_SAFE_LIMIT,
        "socket path {path:?} is {len} bytes, at or over the sun_path limit"
    );
}

#[cfg(not(windows))]
#[test]
fn ensure_private_dir_creates_and_accepts_a_fresh_0700_directory() {
    let uid = unsafe { libc::getuid() };
    let dir =
        std::env::temp_dir().join(format!("amr-test-fresh-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ensure_private_dir(&dir, uid));
    assert!(
        ensure_private_dir(&dir, uid),
        "re-checking an already-ours dir must still accept it"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(not(windows))]
#[test]
fn ensure_private_dir_refuses_a_directory_it_does_not_exclusively_own() {
    use std::os::unix::fs::PermissionsExt;
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!(
        "amr-test-unsafe-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    // World-writable/readable, not the required 0700: another local user
    // could have pre-created this under a shared /tmp.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        !ensure_private_dir(&dir, uid),
        "a pre-existing directory with the wrong permissions must be refused, not silently reused"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn state_treats_ended_as_no_current_call_immediately() {
    let state = BrowserBridgeState::new();
    state.record(Observation {
        platform: Platform::Meet,
        state: CallState::InCall,
        title: None,
    });
    state.record(Observation {
        platform: Platform::Meet,
        state: CallState::Ended,
        title: None,
    });
    assert!(state.current_in_call(Instant::now()).is_none());
}
