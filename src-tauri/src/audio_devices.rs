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
        let host = cpal::default_host();
        let backend = format!("{:?}", host.id());
        let mut snapshot = AudioInventorySnapshot {
            backend,
            ..Default::default()
        };
        collect(&host, &mut snapshot);
        snapshot
    }
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
        let directions = [
            (AudioDeviceDirection::Input, description.supports_input()),
            (AudioDeviceDirection::Output, description.supports_output()),
        ];
        if !directions.iter().any(|(_, supported)| *supported) {
            snapshot
                .errors
                .push("Audio backend reported a device with no source direction.".into());
            continue;
        }
        let name = description.name().to_string();
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
                let (selectable, unavailable_reason) = match direction { AudioDeviceDirection::Input => (id.is_some() && base_reason.is_none(), base_reason), AudioDeviceDirection::Output if cfg!(target_os = "windows") => (id.is_some() && base_reason.is_none(), base_reason), AudioDeviceDirection::Output => (false, Some(if cfg!(target_os = "macos") { "System-audio capture is not available here yet. Choose a virtual input until ScreenCaptureKit support is added." } else { "System-audio capture is not available here yet. Choose a monitor or virtual input exposed by your Linux audio adapter." }.into())) };
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
