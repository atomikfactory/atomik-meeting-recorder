//! Platform-neutral logic for Linux system-output ("what the speakers play")
//! capture via PulseAudio/PipeWire's PulseAudio-compatible client protocol.
//!
//! Everything here is pure (no FFI, no I/O) so it runs under `cargo test` on
//! every platform, including Windows CI. The actual libpulse calls live in
//! `linux_pulse.rs`, which only compiles on `target_os = "linux"`.

/// Stable id scheme for a sink's monitor source, e.g. `pulse:monitor:alsa_output.pci-0000_00_1f.3.analog-stereo`.
/// Mirrors `audio_devices.rs`'s `"{native_id}:{input|output}"` scheme for CPAL
/// devices: a short fixed prefix plus the backend's own stable name, so a
/// driver/session restart that keeps the same sink name keeps the same id.
const PULSE_MONITOR_PREFIX: &str = "pulse:monitor:";

// These are only called from Linux-only code (`linux_pulse.rs` and
// `audio_devices.rs`'s Linux-gated enumeration), but stay in this
// platform-neutral module -- rather than behind `#[cfg(target_os = "linux")]`
// themselves -- so their pure logic keeps running under `cargo test` on every
// CI platform. `allow(dead_code)` only suppresses the (accurate, but
// intentional) unused-on-this-platform warning on non-Linux targets; their
// own tests below exercise them everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pulse_monitor_id(sink_name: &str) -> String {
    format!("{PULSE_MONITOR_PREFIX}{sink_name}")
}

/// Recovers the sink name from a monitor id produced by `pulse_monitor_id`,
/// or `None` if `id` is not one of ours (e.g. a CPAL device id).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_pulse_monitor_id(id: &str) -> Option<&str> {
    id.strip_prefix(PULSE_MONITOR_PREFIX)
        .filter(|name| !name.is_empty())
}

/// Stable id scheme for a PulseAudio/PipeWire input source (a microphone,
/// interface or webcam), e.g. `pulse:source:alsa_input.usb-046d_C922_Pro_Stream_Webcam-02.analog-stereo`.
/// Same shape as the monitor id above: the sound server's own stable source
/// name behind a fixed prefix.
const PULSE_SOURCE_PREFIX: &str = "pulse:source:";

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pulse_source_id(source_name: &str) -> String {
    format!("{PULSE_SOURCE_PREFIX}{source_name}")
}

/// Recovers the source name from an id produced by `pulse_source_id`, or
/// `None` if `id` is not one (a monitor id, a CPAL device id).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_pulse_source_id(id: &str) -> Option<&str> {
    id.strip_prefix(PULSE_SOURCE_PREFIX)
        .filter(|name| !name.is_empty())
}

/// One PulseAudio/PipeWire source as the server reports it, before deciding
/// whether it is worth listing as an input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct PulseSource {
    pub name: String,
    pub description: String,
    /// The source is a sink's monitor ("what the speakers play"); those are
    /// listed separately as system-output sources, not as inputs.
    pub is_monitor: bool,
    /// The server knows the active port has nothing plugged in (an empty
    /// line-in or headset jack). `false` when it does not know.
    pub unplugged: bool,
    pub is_default: bool,
    pub sample_rate: u32,
    pub channels: u16,
}

/// The sources worth offering as inputs: what the desktop's own sound
/// settings list. Monitors are left out (they are system-output sources) and
/// so are jacks with nothing plugged in. Unlike CPAL's ALSA inventory, which
/// lists every PCM plugin of every card (`hw:`, `plughw:`, `front:`,
/// `dsnoop:`, `surround*`...), the sound server already shows one source per
/// device profile, and opening it shares the device with other applications
/// instead of claiming the hardware exclusively.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pertinent_inputs(sources: Vec<PulseSource>) -> Vec<PulseSource> {
    sources
        .into_iter()
        .filter(|source| !source.is_monitor && !source.unplugged && source.channels > 0)
        .collect()
}

/// PulseAudio/ALSA expose a handful of routing pseudo-devices ("default",
/// "pulse", "sysdefault", "pipewire", and their `sysdefault:CARD=...`/
/// `front:CARD=...` siblings are real, but the bare aliases below just
/// redirect to whatever the sound server currently considers default). CPAL's
/// ALSA backend enumerates these alongside real hardware; listing them next
/// to the same hardware's Pulse monitor is confusing duplication, not a
/// distinct capture source, so device merging filters them out by exact name.
const ALSA_ROUTING_ALIASES: &[&str] = &["default", "pulse", "sysdefault", "pipewire"];

pub fn is_alsa_routing_alias(name: &str) -> bool {
    ALSA_ROUTING_ALIASES.contains(&name)
}

/// A monitor source delivers interleaved samples in one PulseAudio-native
/// sample format. This app's downstream pipeline (AudioPacket, WAV writer,
/// capture meter) works in `f32` throughout, exactly like CPAL's callback
/// conversion in `recording.rs`, so capture requests `FLOAT32LE` from the
/// server and this is just a byte-to-sample reinterpretation, not a format
/// conversion. Kept as a free function so its edge cases (truncated tail
/// bytes from a short read) are covered without needing a live server.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn f32le_bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

/// Number of complete interleaved frames represented by `sample_count`
/// samples at `channels` channels. Returns 0 for a malformed (zero-channel)
/// format instead of dividing by zero.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn frames_in(sample_count: usize, channels: u16) -> u64 {
    if channels == 0 {
        return 0;
    }
    (sample_count / usize::from(channels)) as u64
}

/// PulseAudio's `bytes_per_second` sizing for a target fragment latency, used
/// to size the simple-API stream's `maxlength`/`tlength`-equivalent buffer
/// attributes for low-latency capture. Kept pure/testable rather than reading
/// `pa_sample_spec` fields inline at the call site.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn bytes_per_fragment(sample_rate: u32, channels: u16, fragment_ms: u32) -> u32 {
    let bytes_per_frame = u32::from(channels) * 4; // FLOAT32LE
    let frames = (u64::from(sample_rate) * u64::from(fragment_ms) / 1000) as u32;
    frames.saturating_mul(bytes_per_frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_id_round_trips_a_sink_name() {
        let id = pulse_monitor_id("alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(
            id,
            "pulse:monitor:alsa_output.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(
            parse_pulse_monitor_id(&id),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
    }

    #[test]
    fn parse_rejects_non_monitor_ids() {
        assert_eq!(parse_pulse_monitor_id("hw:0,0:output"), None);
        assert_eq!(parse_pulse_monitor_id("pulse:monitor:"), None);
        assert_eq!(parse_pulse_monitor_id(""), None);
    }

    #[test]
    fn source_id_round_trips_and_is_distinct_from_a_monitor_id() {
        let name = "alsa_input.usb-046d_C922_Pro_Stream_Webcam-02.analog-stereo";
        let id = pulse_source_id(name);
        assert_eq!(id, format!("pulse:source:{name}"));
        assert_eq!(parse_pulse_source_id(&id), Some(name));
        assert_eq!(parse_pulse_monitor_id(&id), None);
        assert_eq!(parse_pulse_source_id(&pulse_monitor_id(name)), None);
        assert_eq!(parse_pulse_source_id("pulse:source:"), None);
        assert_eq!(parse_pulse_source_id("hw:CARD=Webcam,DEV=0:input"), None);
    }

    fn source(name: &str, is_monitor: bool, unplugged: bool) -> PulseSource {
        PulseSource {
            name: name.into(),
            description: name.into(),
            is_monitor,
            unplugged,
            is_default: false,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn pertinent_inputs_drop_monitors_and_unplugged_jacks_and_keep_order() {
        let kept = pertinent_inputs(vec![
            source("webcam", false, false),
            source("speakers.monitor", true, false),
            source("line-in", false, true),
            source("interface", false, false),
            PulseSource {
                channels: 0,
                ..source("broken", false, false)
            },
        ]);
        let names: Vec<_> = kept.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["webcam", "interface"]);
    }

    #[test]
    fn routing_aliases_are_recognized_and_hardware_is_not() {
        for alias in ["default", "pulse", "sysdefault", "pipewire"] {
            assert!(is_alsa_routing_alias(alias));
        }
        assert!(!is_alsa_routing_alias("sysdefault:CARD=PCH"));
        assert!(!is_alsa_routing_alias("hw:0,0"));
    }

    #[test]
    fn f32le_conversion_matches_manual_bytes_and_ignores_a_truncated_tail() {
        let mut bytes = 1.0f32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
        bytes.push(0xAA); // short trailing byte from a mid-fragment read
        let samples = f32le_bytes_to_samples(&bytes);
        assert_eq!(samples, vec![1.0, -0.5]);
    }

    #[test]
    fn frame_count_divides_by_channel_count_and_avoids_div_by_zero() {
        assert_eq!(frames_in(4, 2), 2);
        assert_eq!(frames_in(3, 2), 1); // trailing partial frame dropped
        assert_eq!(frames_in(4, 0), 0);
    }

    #[test]
    fn fragment_sizing_matches_rate_channels_and_duration() {
        // 48kHz stereo f32, 20ms fragment: 48000 * 0.02 = 960 frames * 8 bytes.
        assert_eq!(bytes_per_fragment(48_000, 2, 20), 960 * 8);
        assert_eq!(bytes_per_fragment(44_100, 1, 10), 441 * 4);
    }
}
