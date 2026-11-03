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
| Linux | PulseAudio/PipeWire client protocol (inputs and system output); ALSA only as a fallback | Inputs are the sound server's sources: one entry per microphone, interface or webcam, as the desktop's own sound settings list them. Monitor sources are not inputs, and a source whose active port the server reports as unplugged (an empty line-in jack) is hidden. Each sink's monitor source is listed separately as a selectable output ("`<sink> (monitor)`"). Both are backed by `libpulse-binding`/`libpulse-simple-binding` rather than CPAL. When no PulseAudio/PipeWire server answers, the list falls back to CPAL's ALSA inputs, without ALSA's `default`/`pulse`/`sysdefault`/`pipewire` routing aliases, and says why; ALSA outputs are never listed, since none is a loopback source. |

On Linux an input's id is `pulse:source:<source name>` and a monitor's id is `pulse:monitor:<sink name>`: a fixed prefix plus PulseAudio/PipeWire's own stable name, mirroring the `<native id>:input`/`:output` scheme CPAL devices use elsewhere. The id survives restarts and a sound-server reconnect as long as the name stays the same. A source that disappears shows up as unavailable-but-selected, exactly like a disconnected CPAL device, through the same id-not-found handling in the frontend.

Inputs used to come from CPAL's ALSA backend, and that caused two problems. First, ALSA lists every PCM plugin of every card (`hw:`, `plughw:`, `front:`, `dsnoop:`, `surround*`...), so one webcam appeared many times. Second, opening a raw `hw:` device claims the card exclusively: it fails ("device is temporarily busy") while PipeWire or another app is using it, and while it is held, the meeting app cannot use that microphone. Recording through the sound server shares the device instead. A selection saved with an old ALSA id shows as unavailable and has to be chosen again from the new list.

PulseAudio was chosen over a PipeWire-native client because `pipewire-pulse` serves the identical protocol on every modern distro, so one implementation covers both sound servers without an extra native dependency. JACK was not considered, since it is not a default audio path on the target distros. Each track carries its own independent `FrameTimeline` (see [recording](recording.md)).

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
