# Audio device management — Phase 2

Phase 2 added device discovery and saved source selection. Phase 3 now provides [manual recording](recording.md), but listing a device or saving a source still does not start an audio stream or establish microphone permission for capture.

## Device identity and selection

Use the native backend's CPAL DeviceId rather than a friendly name or enumeration index. Friendly names are display labels and can repeat. Save a direction-qualified selection for duplex devices so an input is distinct from its output. Refresh must never substitute another device because it shares a name or becomes the OS default.

Existing preferences migrate with an empty source selection. A user may select multiple available inputs. The default-device marker helps identify the system's current choice, but does not automatically change a saved selection. Selected devices which disappear remain visible as unavailable and can be removed explicitly. Driver reinstalls, OS changes, or some Linux device naming schemes can change native IDs; those cases require reselection rather than guessed identity.

Device information should distinguish input/output, backend, OS default status, and default sample rate/channel format. A default format query is not an active recording sample rate and does not prove that opening the device will succeed. Per-device query failures should be visible without discarding the rest of the inventory.

## Platform boundaries

| Platform | Inventory backend | Source choices and limits |
| --- | --- | --- |
| Windows | WASAPI | Inputs include microphones and interfaces exposed by Windows drivers. Output endpoints can be selected as system-loopback sources. Recording loopback captures the endpoint mix, not necessarily only the meeting app. ASIO-only capabilities are outside this MVP. |
| macOS | Core Audio | Select exposed input devices and interfaces. Output enumeration does not provide system capture; a ScreenCaptureKit adapter and its OS permissions remain separate work. Explicitly configured virtual input devices may appear as inputs. |
| Linux | ALSA (inputs) + PulseAudio/PipeWire client protocol (system output) | ALSA exposes microphones and interfaces as inputs. Each PulseAudio/PipeWire sink's monitor source is listed separately as a selectable output ("`<sink> (monitor)`"), backed by `libpulse-binding`/`libpulse-simple-binding` rather than CPAL -- CPAL's ALSA backend has no monitor-source concept. ALSA's `default`/`pulse`/`sysdefault`/`pipewire` routing pseudo-devices are filtered out of the ALSA list entirely, since they just redirect to whatever the sound server considers default and would otherwise appear as confusing duplicates of real hardware (and, for output, of that hardware's own monitor entry). |

A Linux monitor source's id is `pulse:monitor:<sink name>` -- a fixed prefix plus PulseAudio/PipeWire's own stable sink name, mirroring the `<native id>:input`/`:output` scheme CPAL devices already use above. It survives restarts and a sound-server reconnect as long as the sink keeps its name; a monitor whose sink has disappeared shows up as unavailable-but-selected exactly like a disconnected CPAL device, using the same generic id-not-found handling in the frontend. PulseAudio was chosen over a PipeWire-native client because `pipewire-pulse` serves the identical protocol on every modern distro, so one implementation covers both sound servers without an extra native dependency; JACK was not considered since it is not a default audio path on the target distros. Microphones remain ALSA/CPAL-only on Linux -- PipeWire's clock model already lets ALSA (via `pipewire-alsa`) and the monitor's PulseAudio/PipeWire path observe the same underlying graph, and each track already carries its own independent `FrameTimeline` (see [recording](recording.md)), so mixing backends across a recording's tracks is no different from mixing CPAL backends across OSes elsewhere in this app.

Virtual cable names can be counterintuitive: a device named “CABLE Output” may be an input to this app. Trust the backend's direction, not words in the name.

Enumeration is local. Format queries may contact the OS audio service or driver; they do not play or record audio. Device changes are refreshed while the window is visible, with explicit refresh available. Tray/background recording will need capture-engine disconnection handling in its own phase.

## Verification targets

- Enumerate actual hardware and compare endpoint directions with the OS.
- Select more than one input; save, reload and restart without losing IDs.
- Save ordinary preferences without overwriting a newer audio selection.
- Refresh while editing ordinary preferences; keep unsaved controls intact.
- Simulate unplug/replug and enumeration failure; preserve selected and draft devices without automatic fallback.
- Reject duplicate, unknown, or unsupported selections at the Rust boundary.
- Preserve selections and settings when saving fails.
- Use explicit mocks for hardware scenarios in automated tests; document real hotplug testing separately.

## Primary references

- [CPAL DeviceId persistence](https://docs.rs/cpal/latest/cpal/struct.DeviceId.html)
- [Device descriptions and direction](https://docs.rs/cpal/latest/cpal/device_description/struct.DeviceDescription.html)
- [Device format queries](https://docs.rs/cpal/latest/cpal/traits/trait.DeviceTrait.html)
- [WASAPI loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
- [PipeWire stream properties](https://docs.pipewire.org/devel/group__pw__keys.html)
- [PipeWire's PulseAudio compatibility (`pipewire-pulse`)](https://docs.pipewire.org/page_module_protocol_pulse.html)
- [libpulse-binding / libpulse-simple-binding](https://docs.rs/libpulse-binding/latest/libpulse_binding/)
- [PulseAudio monitor sources](https://www.freedesktop.org/wiki/Software/PulseAudio/Documentation/User/Modules/#module-loopback)
