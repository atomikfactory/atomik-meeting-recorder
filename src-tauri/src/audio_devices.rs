use crate::system_output;
use cpal::traits::{DeviceTrait, HostTrait};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioDeviceDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: Option<String>,
    pub name: String,
    pub direction: AudioDeviceDirection,
    pub is_default: bool,
    pub default_format: Option<String>,
    pub selectable: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct AudioInventorySnapshot {
    pub backend: String,
    pub devices: Vec<AudioDevice>,
    pub errors: Vec<String>,
}

/// A deliberately small boundary around the platform enumeration. Capture is not
/// part of this interface; it is only inventory for preference selection.
pub trait AudioDeviceInventory: Send + Sync {
    fn snapshot(&self) -> AudioInventorySnapshot;
}

pub struct CpalAudioDeviceInventory;

impl AudioDeviceInventory for CpalAudioDeviceInventory {
    fn snapshot(&self) -> AudioInventorySnapshot {
        #[cfg(target_os = "linux")]
        let pulse_error = match pulse_snapshot() {
            Ok(snapshot) => return snapshot,
            Err(error) => error,
        };
        let host = cpal::default_host();
        let backend = format!("{:?}", host.id());
        let mut snapshot = AudioInventorySnapshot {
            backend,
            ..Default::default()
        };
        collect(&host, &mut snapshot);
        #[cfg(target_os = "linux")]
        snapshot.errors.push(format!(
            "Could not list PulseAudio/PipeWire devices, so raw ALSA devices are listed instead and system output cannot be recorded: {pulse_error}"
        ));
        snapshot
    }
}

/// The Linux inventory, from the PulseAudio/PipeWire server: its input
/// sources (microphones, interfaces, webcams; see
/// `system_output::pertinent_inputs`) and each sink's monitor ("what the
/// speakers play"). This is the list the desktop's own sound settings show,
/// and capturing through the server shares a device with the meeting app
/// instead of claiming it exclusively, as opening an ALSA `hw:` device would.
///
/// An error when no server answers (neither pipewire-pulse nor pulseaudio
/// running): the caller then falls back to CPAL's ALSA inventory and shows
/// the reason as an inventory error.
#[cfg(target_os = "linux")]
fn pulse_snapshot() -> Result<AudioInventorySnapshot, String> {
    let inventory = crate::linux_pulse::enumerate()?;
    let mut devices = Vec::new();
    for source in system_output::pertinent_inputs(inventory.sources) {
        devices.push(AudioDevice {
            id: Some(system_output::pulse_source_id(&source.name)),
            name: source.description,
            direction: AudioDeviceDirection::Input,
            is_default: source.is_default,
            default_format: Some(format!(
                "{} channels · {} Hz",
                source.channels, source.sample_rate
            )),
            selectable: true,
            unavailable_reason: None,
        });
    }
    for sink in inventory.sinks {
        devices.push(AudioDevice {
            id: Some(system_output::pulse_monitor_id(&sink.sink_name)),
            name: format!("{} (monitor)", sink.description),
            direction: AudioDeviceDirection::Output,
            is_default: sink.is_default,
            default_format: Some(format!(
                "{} channels · {} Hz",
                sink.channels, sink.sample_rate
            )),
            selectable: true,
            unavailable_reason: None,
        });
    }
    Ok(AudioInventorySnapshot {
        backend: "PulseAudio/PipeWire".into(),
        devices,
        errors: Vec::new(),
    })
}

fn collect(host: &cpal::Host, snapshot: &mut AudioInventorySnapshot) {
    let default_input = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host.devices();
    let devices = match devices {
        Ok(devices) => devices,
        Err(error) => {
            snapshot
                .errors
                .push(format!("Could not enumerate audio devices: {error}"));
            return;
        }
    };
    for device in devices {
        let description = match device.description() {
            Ok(description) => description,
            Err(error) => {
                snapshot.errors.push(format!(
                    "Could not read an audio device description: {error}"
                ));
                continue;
            }
        };
        // On Linux this is only the fallback when no PulseAudio/PipeWire
        // server answers; an ALSA output is never a loopback source, so only
        // inputs are listed there.
        let directions = [
            (AudioDeviceDirection::Input, description.supports_input()),
            (
                AudioDeviceDirection::Output,
                description.supports_output() && !cfg!(target_os = "linux"),
            ),
        ];
        if !directions.iter().any(|(_, supported)| *supported) {
            if cfg!(target_os = "linux") && description.supports_output() {
                continue;
            }
            snapshot
                .errors
                .push("Audio backend reported a device with no source direction.".into());
            continue;
        }
        let name = description.name().to_string();
        // ALSA's "default"/"pulse"/"sysdefault"/"pipewire" pseudo-devices just
        // redirect to whichever real device the sound server currently
        // considers default; listing them next to that same hardware (and,
        // for output, next to its Pulse monitor below) is confusing
        // duplication rather than a distinct capture source.
        if cfg!(target_os = "linux") && system_output::is_alsa_routing_alias(&name) {
            continue;
        }
        // DeviceId is supplied by CPAL's backend and is intentionally not derived
        // from a mutable display name or enumeration index.
        let native_id = device.id().ok().map(|id| id.to_string());
        for (direction, supported) in directions {
            if supported {
                let format_result = match direction {
                    AudioDeviceDirection::Input => device.default_input_config(),
                    AudioDeviceDirection::Output => device.default_output_config(),
                };
                let (default_format, format_error) = match format_result {
                    Ok(config) => (
                        Some(format!(
                            "{} channels · {} Hz",
                            config.channels(),
                            config.sample_rate()
                        )),
                        None,
                    ),
                    Err(error) => (
                        None,
                        Some(format!(
                            "Could not read this device's default format: {error}"
                        )),
                    ),
                };
                // Direction is part of the identity: a duplex endpoint creates two
                // unambiguous selectable source records without using its display name.
                let suffix = match direction {
                    AudioDeviceDirection::Input => "input",
                    AudioDeviceDirection::Output => "output",
                };
                let id = native_id.as_ref().map(|id| format!("{id}:{suffix}"));
                let base_reason = if id.is_none() {
                    Some("This device has no stable native endpoint ID, so it cannot be selected safely.".into())
                } else {
                    format_error
                };
                let (selectable, unavailable_reason) = match direction {
                    AudioDeviceDirection::Input => (id.is_some() && base_reason.is_none(), base_reason),
                    AudioDeviceDirection::Output if cfg!(target_os = "windows") => {
                        (id.is_some() && base_reason.is_none(), base_reason)
                    }
                    AudioDeviceDirection::Output => (
                        false,
                        Some(
                            "System-audio capture is not available here yet. Choose a virtual input until ScreenCaptureKit support is added."
                                .into(),
                        ),
                    ),
                };
                let is_default = native_id.is_some()
                    && match direction {
                        AudioDeviceDirection::Input => {
                            native_id.as_deref() == default_input.as_deref()
                        }
                        AudioDeviceDirection::Output => {
                            native_id.as_deref() == default_output.as_deref()
                        }
                    };
                snapshot.devices.push(AudioDevice {
                    id,
                    name: name.clone(),
                    direction,
                    is_default,
                    default_format,
                    selectable,
                    unavailable_reason,
                });
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_snapshot() -> AudioInventorySnapshot {
    AudioInventorySnapshot {
        backend: "test".into(),
        devices: vec![
            AudioDevice {
                id: Some("input-a".into()),
                name: "Desk microphone".into(),
                direction: AudioDeviceDirection::Input,
                is_default: true,
                default_format: Some("2 channels · 48000 Hz".into()),
                selectable: true,
                unavailable_reason: None,
            },
            AudioDevice {
                id: Some("output-a".into()),
                name: "Speakers".into(),
                direction: AudioDeviceDirection::Output,
                is_default: false,
                default_format: Some("2 channels · 48000 Hz".into()),
                selectable: cfg!(target_os = "windows"),
                unavailable_reason: None,
            },
        ],
        errors: vec![],
    }
}
