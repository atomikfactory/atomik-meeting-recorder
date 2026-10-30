//! Global recording hotkey: accelerator validation/normalization, the pure
//! idle/recording/awaiting-approval toggle decision, and a debounce guard --
//! all independent of Tauri/OS registration so they are cheaply unit tested.
//! Actual OS registration lives in `lib.rs` (via `tauri-plugin-global-shortcut`),
//! which calls `validate_accelerator` before ever asking the OS to register
//! anything.
use serde::Serialize;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// Recognized modifier tokens (case-insensitive), mapped to the canonical
/// spelling `tauri-plugin-global-shortcut` (via the `global-hotkey` crate)
/// parses. "Win"/"Windows"/"Meta" are not accepted by that parser, so they
/// are normalized to "Super" here rather than passed through verbatim.
const MODIFIER_ALIASES: &[(&str, &str)] = &[
    ("ALT", "Alt"),
    ("OPTION", "Alt"),
    ("CTRL", "Control"),
    ("CONTROL", "Control"),
    ("CMD", "Command"),
    ("COMMAND", "Command"),
    ("SUPER", "Super"),
    ("WIN", "Super"),
    ("WINDOWS", "Super"),
    ("META", "Super"),
    ("SHIFT", "Shift"),
    ("COMMANDORCONTROL", "CommandOrControl"),
    ("CMDORCTRL", "CommandOrControl"),
    ("CMDORCONTROL", "CommandOrControl"),
    ("COMMANDORCTRL", "CommandOrControl"),
];
/// Reserved OS/window-manager combinations that must never be captured or
/// accepted as this app's recording shortcut, checked after normalization
/// (so e.g. "Control+Alt+Delete" and "ctrl+alt+delete" are both caught).
const RESERVED_COMBOS: &[(&[&str], &str)] = &[
    (&["Alt", "Control"], "Delete"),
    (&["Alt"], "F4"),
    (&["Super"], "L"),
];
fn canonical_modifier(token: &str) -> Option<&'static str> {
    let upper = token.trim().to_ascii_uppercase();
    MODIFIER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == upper)
        .map(|(_, canonical)| *canonical)
}
fn canonical_key(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.chars().count() == 1 {
        return trimmed.to_ascii_uppercase();
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
        }
    }
}
/// Validates a user- or default-supplied accelerator string, returning its
/// canonical form (consistent modifier spelling, at least one modifier, and
/// exactly one trailing non-modifier key) or a message fit to show the user.
/// Does not attempt OS registration -- see `lib.rs::apply_hotkey_settings`.
pub fn validate_accelerator(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.split('+').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) || parts.len() < 2 {
        return Err(
            "The shortcut must include at least one modifier (e.g. Ctrl) plus a key.".into(),
        );
    }
    let (key_part, modifier_parts) = parts.split_last().expect("checked len >= 2 above");
    let mut modifiers = Vec::with_capacity(modifier_parts.len());
    for part in modifier_parts {
        match canonical_modifier(part) {
            Some(canonical) => {
                if !modifiers.contains(&canonical) {
                    modifiers.push(canonical);
                }
            }
            None => return Err(format!("Unrecognized modifier key: \"{part}\".")),
        }
    }
    if canonical_modifier(key_part).is_some() {
        return Err("The shortcut must end with a non-modifier key.".into());
    }
    let key = canonical_key(key_part);
    if key.is_empty() {
        return Err("The shortcut is missing its final key.".into());
    }
    let mut sorted_modifiers = modifiers.clone();
    sorted_modifiers.sort_unstable();
    for (reserved_modifiers, reserved_key) in RESERVED_COMBOS {
        let mut reserved_sorted: Vec<&str> = reserved_modifiers.to_vec();
        reserved_sorted.sort_unstable();
        if sorted_modifiers == reserved_sorted && key.eq_ignore_ascii_case(reserved_key) {
            return Err(format!(
                "\"{raw}\" is reserved by the operating system and cannot be used here."
            ));
        }
    }
    let mut canonical = modifiers;
    canonical.push(key.as_str());
    Ok(canonical.join("+"))
}

/// What pressing the global hotkey should do right now, decided purely from
/// meeting-detection's current phase -- no OS/Tauri dependency, so this is
/// unit tested directly. See `detection::DetectionState::phase` for the
/// possible phase strings; anything other than "recording" or
/// "awaiting_approval" is treated as idle (start a manual recording).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleAction {
    Start,
    Stop,
    Approve(String),
}
pub fn decide_toggle_action(phase: &str, approval_id: Option<&str>) -> ToggleAction {
    match phase {
        "recording" => ToggleAction::Stop,
        "awaiting_approval" => match approval_id {
            Some(id) => ToggleAction::Approve(id.to_string()),
            None => ToggleAction::Start,
        },
        _ => ToggleAction::Start,
    }
}

/// Ignores a hotkey press that follows another accepted press within
/// `window` -- guards against a single physical key-down producing multiple
/// OS-level events, or a user holding the combination down.
pub struct Debounce(Mutex<Option<Instant>>);
impl Default for Debounce {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}
impl Debounce {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn try_fire(&self, now: Instant, window: Duration) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(previous) = *last {
            if now.duration_since(previous) < window {
                return false;
            }
        }
        *last = Some(now);
        true
    }
}

/// Reported to the frontend (`get_hotkey_status`) so Settings can show
/// whether the saved shortcut actually registered with the OS -- e.g.
/// another application already owns it -- without blocking the rest of
/// `save_settings` on that failure.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HotkeyStatus {
    pub enabled: bool,
    pub hotkey: String,
    pub registered: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shortcut_is_valid_and_canonical() {
        assert_eq!(
            validate_accelerator("CommandOrControl+Alt+R").unwrap(),
            "CommandOrControl+Alt+R"
        );
    }

    #[test]
    fn accepts_case_insensitive_aliases_and_normalizes() {
        assert_eq!(validate_accelerator("ctrl+alt+r").unwrap(), "Control+Alt+R");
        assert_eq!(
            validate_accelerator("Option+shift+p").unwrap(),
            "Alt+Shift+P"
        );
        assert_eq!(
            validate_accelerator("win+shift+m").unwrap(),
            "Super+Shift+M"
        );
    }

    #[test]
    fn normalizes_named_keys() {
        assert_eq!(validate_accelerator("Ctrl+space").unwrap(), "Control+Space");
        assert_eq!(validate_accelerator("Ctrl+F1").unwrap(), "Control+F1");
    }

    #[test]
    fn rejects_modifier_only_combos() {
        assert!(validate_accelerator("Ctrl+Alt").is_err());
        assert!(validate_accelerator("Ctrl").is_err());
        assert!(validate_accelerator("").is_err());
    }

    #[test]
    fn rejects_unrecognized_modifier() {
        let error = validate_accelerator("Fn+R").unwrap_err();
        assert!(error.contains("Unrecognized"), "{error}");
    }

    #[test]
    fn rejects_reserved_combos_case_and_order_insensitively() {
        assert!(validate_accelerator("Ctrl+Alt+Delete").is_err());
        assert!(validate_accelerator("alt+ctrl+delete").is_err());
        assert!(validate_accelerator("Alt+F4").is_err());
        assert!(validate_accelerator("Win+L").is_err());
    }

    #[test]
    fn dedupes_repeated_modifiers() {
        assert_eq!(validate_accelerator("Ctrl+Ctrl+R").unwrap(), "Control+R");
    }

    // --- toggle decision ---------------------------------------------

    #[test]
    fn idle_phases_start_a_recording() {
        assert_eq!(decide_toggle_action("waiting", None), ToggleAction::Start);
        assert_eq!(
            decide_toggle_action("confirming", None),
            ToggleAction::Start
        );
        assert_eq!(
            decide_toggle_action("suppressed", None),
            ToggleAction::Start
        );
        assert_eq!(decide_toggle_action("error", None), ToggleAction::Start);
    }

    #[test]
    fn recording_phase_stops() {
        assert_eq!(decide_toggle_action("recording", None), ToggleAction::Stop);
    }

    #[test]
    fn awaiting_approval_with_an_id_approves_it() {
        assert_eq!(
            decide_toggle_action("awaiting_approval", Some("req-1")),
            ToggleAction::Approve("req-1".into())
        );
    }

    #[test]
    fn awaiting_approval_without_an_id_falls_back_to_start() {
        assert_eq!(
            decide_toggle_action("awaiting_approval", None),
            ToggleAction::Start
        );
    }

    // --- debounce ------------------------------------------------------

    #[test]
    fn debounce_ignores_a_repeat_within_the_window_and_allows_it_after() {
        let debounce = Debounce::new();
        let first = Instant::now();
        assert!(debounce.try_fire(first, Duration::from_secs(1)));
        let too_soon = first + Duration::from_millis(500);
        assert!(!debounce.try_fire(too_soon, Duration::from_secs(1)));
        let later = first + Duration::from_millis(1200);
        assert!(debounce.try_fire(later, Duration::from_secs(1)));
    }
}
