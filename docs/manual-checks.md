# Desktop acceptance checks

These are manual acceptance checks, not a claim that every platform has passed. Manual capture is Phase 3; meeting detection and approval-driven lifecycle are Phases 4/5. Transcription and network integration remain later work.

## Shell — Windows, macOS, Linux

- Launch the desktop build: dashboard appears with actual local history (or an empty state), manual capture controls, and truthful detection/transcription capability/status. AppFlowy remains disconnected.
- Change detection requests, delays, and local model preference. Save, quit completely, relaunch, and verify persistence. Defaults should enable detection requests, but every detected recording still requires approval.
- Use the tray's Open application and Settings actions, including after the window has been minimized. Verify the window is restored and focused.
- With no selected sources, Start must reject without opening streams. With explicit capture consent and saved sources, window and tray Start/Stop must operate the same session. Stop is disabled when idle; repeated Start must not create another recording.
- Open recordings folder. Confirm it opens the app-owned local folder without interpreting its path as a shell command.
- Close the window on supported Windows/macOS tray setups: process remains running and the tray can restore the window. Quit exits completely.
- On Linux without a visible AppIndicator host, ensure closing never leaves the only window irretrievably hidden. There is no in-window Quit button; use the tray's Quit action (the only way to exit while the tray keeps the app running in the background). Also check Linux with a visible tray host.
- Launch a second instance: existing window should be restored instead of a second background instance.
- Test at the minimum supported window size and 125% / 150% display scaling. Verify keyboard focus, settings labels, save feedback, and no clipped controls. Check reduced-motion preference.
- Simulate a read-only settings directory and invalid saved settings in an isolated test profile. Surface the error, preserve the original data, and do not enable saving default values over unreadable settings.
- Verify the colored tray icon on macOS light/dark menu bars and the Windows icon at native tray size.

## Audio device management

- Check each listed endpoint's input/output direction against OS audio settings, including virtual cables with misleading names.
- Check default-format sample rate and channels against the OS; no recording should start during refresh or selection.
- Save multiple inputs and an eligible system-audio source, quit, relaunch, and verify the same IDs remain selected.
- Change an ordinary preference while a device refresh occurs. Verify the draft and keyboard focus survive.
- Unplug a selected USB interface. Verify it remains listed as unavailable; plug it back in and verify it returns without selecting another device.
- Refresh during an audio-service failure or device permission failure. Keep existing selections and show a useful error.
- Check outputs on macOS/Linux are not presented as working loopback capture.
- See [device management details](audio-devices.md) for backend-specific limits.

## Privacy checks

- Device refresh and detection must not open microphone streams. Capture requires explicit Start or approval of a current detected-meeting request; no camera permission is needed. Dismissing or allowing a request to expire must not record.
- No runtime calls to analytics, fonts, model providers, or AppFlowy. Development dependency downloads and Vite's local development connection are separate from application runtime traffic.
- Browser preview clearly identifies itself and cannot persist desktop settings or open folders.

## Future phase checks

Capture acceptance must include device removal mid-session, default-device change, different sample rates/clocks, long meetings, sleep/wake, and process kill before WAV finalization. Check that partial audio remains recoverable and devices never switch silently. See [recording design and acceptance](recording.md) and the actual [verification results](verification.md). Do not confuse synthetic recovery tests with these hardware checks.

Detection acceptance needs real Slack and Chrome/Firefox calls as well as non-call microphone use. Check confirmation cancellation, explicit approval, stale-request rejection, grace expiry, manual stop/dismiss suppression across restart, and manual recordings without meeting signals. Keep synthetic clock/detector tests distinct from live-call verification. Use `MEETING_RECORDER_DISABLE_AUTOMATION=1` for ordinary native smoke tests. Do not create real capture sessions without separate consent.

Pipeline tests need mocked engines/destination: unique job claims, restart during each stage, transcribed output retained after sync failure, bounded retries, ambiguous remote-create results, no duplicate notes/date pages, corrupt input, missing models, and deterministic Markdown with no summaries or invented identities.

Run actual recording/Whisper/diarization/integration acceptance only when those implementations exist. Shell checks cannot stand in for those tests.
