# Meeting Recorder implementation plan

## Inspection — 2026-09-23

The workspace was empty (including no Git repository or AGENTS.md). No application shell, UI, database, build configuration, or existing code needed preservation. The development host is Windows with Node 25.5.0, npm 11.8.0, Git, CMake, FFmpeg, Visual Studio Build Tools 2019 (C++ tools), and WebView2 148. Rust was absent from PATH and the usual user installation directory. Rust 1.98.1, rustfmt, and Clippy were subsequently installed inside `.tools/` without changing the system PATH; they are not application runtime dependencies. Target desktop platforms remain Windows, macOS, and Linux; only Windows can be exercised on this host.

## Architecture

Tauri 2 owns the native tray, window lifecycle, single instance, paths, and IPC. Rust owns settings, eventual capture coordination, durable jobs, and integrations. A small vanilla TypeScript/Vite frontend renders snapshots and invokes commands; no frontend framework or bundled browser is necessary for this MVP. System fonts and local vector assets avoid runtime CDN requests.

SQLite through rusqlite with bundled SQLite starts with transactional settings. Later migrations add recordings and processing jobs. Keep dependencies and interfaces small; add engine traits when implementations exist rather than building a general plugin framework now.

Phase 2 uses CPAL 0.18.2 behind `AudioDeviceInventory` for native endpoint discovery. Blocking driver queries run off the UI thread and are serialized. Direction-qualified native IDs and display metadata persist in SQLite; older settings deserialize with an empty selection. Dedicated source saves validate the current inventory and preserve other preferences transactionally. No capture stream is opened. See [device architecture and platform limits](audio-devices.md).

Phase 3 adds `RecordingService`, a single-thread-owned capture coordinator with replaceable `AudioCapture`/device/stream boundaries and synthetic implementations in tests. Native streams feed bounded queues, separate WAV writer workers, and transactional `RecordingStore` records. CPAL supplies Windows output loopback; unsupported output capture on other platforms remains explicit. The current implementation uses checkpointed per-source WAV files, not chunk assembly or mixed audio. See [manual capture design](recording.md).

Phases 4/5 add local Windows meeting hints and a persisted detection lifecycle. The user subsequently chose **ask before each detected recording**, so automatic detection produces an approval request rather than unattended capture. The user also approved a local-only Chrome/Firefox extension for a later increment; this is not installed yet. See [detection lifecycle](detection.md) and [platform signal limits](detection-platforms.md). Earlier phase descriptions below are historical delivery boundaries, not current capability claims.

Phase 1 is an application shell. It must never display a simulated recording as real, claim to be monitoring hardware, or fabricate meeting history. Automatic recording is the saved default preference, but capture and detection remain unavailable until their phases land. Start/Stop are present in the tray with truthful availability. Settings, navigation, folder opening, single-instance behavior, and tray lifecycle are real.

## Delivery sequence and acceptance

Phase 6 now implements local CPU Whisper through `TranscriptionEngine`, a single background `TranscriptionService`, and an independent durable SQLite job/model registry. Basic source-labelled Markdown was brought forward from Phase 8 so each successful transcription produces a usable local document. Actual diarization and AppFlowy Cloud remain unimplemented. See [transcription design and limitations](transcription.md). Prior phase descriptions below remain historical boundaries.

1. **Shell:** dashboard, settings, tray icon/menu, local settings persistence, clear capability states, close-to-tray and platform fallback. Build frontend and Windows executable; test persistence, validation, and unavailable recording behavior.
2. **Devices:** evaluate current CPAL for microphone/interface enumeration and input capture; list direction, availability, native sample rates and stable device identity where provided. Add refresh/hotplug reconciliation, preserving missing selections rather than silently substituting hardware.
3. **Manual recording:** one coordinator controls capture sessions. UUID recording IDs, SQLite metadata, bounded audio queues, per-source recoverable chunks and final WAV assembly. Keep device callbacks free of disk/network work. Detect dropped frames and disconnections. Never label an empty or broken capture complete.
4. **Detection/lifecycle:** local modular signals and a testable clock-driven debounce state machine. Positive meeting evidence plus capture/session activity; process presence alone is insufficient. Configurable start delay and end grace; own audio sessions excluded. Manual recordings do not stop because automatic meeting evidence vanishes. User stop suppresses immediate auto-restart for the same meeting. Suspend/resume reconciles stale signals and capture handles.
5. **Transcription:** local whisper-rs/whisper.cpp behind a replaceable engine; run outside the UI thread with cancellation and bounded concurrency. Start with configurable multilingual `base`, allow larger local models. Explicit model installation, source/license/checksum metadata, no silent model downloads or audio upload. Resample a derived stream to the model format while retaining source audio.
6. **Diarization and Markdown:** evaluate Sherpa ONNX segmentation/embedding/clustering independently of transcription. Model licensing, CPU/memory cost and overlap quality need validation. Generic numbered speakers only where defensible; uncertain turns stay explicitly unknown. A microphone/system channel is an audio source, not proof of a speaker identity. Write timestamped verbatim Markdown atomically, with recording ID, time zone, sources and duration; no summaries.
7. **AppFlowy Cloud (user-selected):** verify supported Cloud API capabilities, then build the requested root/date/meeting hierarchy. Keep Markdown indefinitely on sync failure. Explicitly configure destination and authentication before any transcript leaves the device. Store credentials in the OS credential store, not SQLite settings or logs.
8. **Hardening:** crash/retry tests, sleep/wake, hotplug, corrupt capture recovery, OS-native packaging and manual tests on all three targets.

At each increment: compile, run relevant business-logic tests, document hardware checks still outstanding. Phase 1 tests cannot establish future recording/import idempotency.

## Durable processing and duplicate protection design

Persist recording_id, UTC start/end plus meeting time zone, duration, source IDs/configurations, source files, status, transcription status/hash, Markdown path, destination workspace/date-page/note IDs, attempts, retry time, and error classification. Keep capture state independent from processing jobs so a new meeting can record while a previous meeting transcribes or syncs.

Use unique database constraints for `(recording_id, job_kind, engine_configuration_hash)` and `(destination, recording_id)` and conditional transactional job claims. Restart recovery reclaims abandoned work, repairs/finalizes available chunks, and resumes the earliest unfinished stage. Do not retranscribe an already verified output. Write temporary transcript output, flush/rename, then update metadata; reconcile an output committed before a database crash using embedded IDs and hashes.

An HTTP create followed by a crash cannot be made exactly-once solely by saving the returned page ID locally. Before implementing automatic import, verify server support for client-selected IDs/idempotency keys or a queryable stable recording marker. Persist an import intent before the request. On an ambiguous timeout, reconcile remotely before attempting another create. If the supported API cannot prove whether a create succeeded, mark it for reconciliation instead of blindly repeating it. Use the recording's local start date, not retry date, for the date page. Apply equivalent reconciliation to date-page creation and account for other clients.

## Platform constraints

| Area | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Input capture | WASAPI/CPAL and desktop microphone privacy settings | Core Audio/CPAL, microphone usage description and permission | ALSA/CPAL or sound-server adapter; device/session permissions |
| System audio | WASAPI shared-mode output loopback; captures chosen endpoint mix, not necessarily one meeting | ScreenCaptureKit native adapter, supported OS version and screen/system-audio permission; older systems need explicitly configured virtual input | PipeWire or PulseAudio monitor sources; availability depends on sound server/session policy; ALSA alone is not universal desktop loopback |
| Multiple sources | Separate clocks, drift correction/resampling and aligned chunks required | Same; do not assume aggregate-device support | Same; routes may disappear/reappear |
| Meeting signals | Investigate supported audio-session/process APIs; don't infer actual calls from a running browser | Permissions and sandboxing restrict observing other apps; no universal other-app mic/camera API assumed | PipeWire/session permissions and Wayland restrict observation |
| Browser identity | Chrome/Firefox process names cannot establish an active Meet call | Same | Same; Wayland may also prevent window-title inspection |
| Tray | Native tray, hide window only with usable tray | Main-thread menu-bar icon; Phase 1 uses a colored icon, so light/dark appearance still needs native testing | AppIndicator support and desktop extension may be needed; tray creation success does not guarantee visible host. Phase 1 closes/exits instead of hiding on Linux |

Browser-specific detection may need an explicitly installed extension/native-messaging adapter. Treat that as a later supported signal, not hidden browsing-history access. Camera activity observation must not open a camera to infer another app's use. Do not request camera permission merely for the shell. Unsupported detectors report capability limitations and retain manual recording as the fallback once capture exists.

## AppFlowy findings and unresolved contract

The user confirmed **AppFlowy Cloud** as the integration target. Synchronize completed transcripts to `My Day To Day Meetings / YYYY-MM-DD / Meeting HH:mm`. Audio capture, Whisper transcription, and diarization stay local; do not upload audio or request cloud summarization. Workspace selection and authentication will be configured when the integration is implemented. The supported API contract for AI meeting notes and import reconciliation still needs verification.

Official AppFlowy Cloud OpenAPI documentation exposes authenticated workspace/folder APIs. The official server source also defines page-view create/read/update and append-block routes. These facts establish a plausible Cloud/self-hosted integration path; they do not establish a stable public contract for the specific AI meeting-note type or guarantee API availability on every server version.

No supported local-only desktop automation API or official MCP path has been verified in this inspection. Third-party MCP servers are not a basis for silently using private APIs. Do not write directly into AppFlowy's local databases, automate its GUI, or invoke cloud AI summarization. If the supported deployment API only supports ordinary document pages, disclose that difference before substituting them for the requested AI meeting notes. For local-only deployments, retain Markdown and document supported manual import until a supported automatic interface is verified.

## Primary sources checked

- [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) and [system tray](https://v2.tauri.app/learn/system-tray/)
- [CPAL platform support](https://docs.rs/cpal/latest/cpal/)
- [Microsoft WASAPI loopback](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording) and [device invalidation](https://learn.microsoft.com/en-us/windows/win32/coreaudio/wasapi)
- [Apple ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
- [PipeWire stream properties](https://docs.pipewire.org/devel/group__pw__keys.html)
- [whisper-rs API](https://docs.rs/whisper-rs/latest/whisper_rs/) (upstream has moved to Codeberg; use published crate provenance when pinning)
- [Sherpa ONNX diarization](https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/index.html) and [official Rust example](https://github.com/k2-fsa/sherpa-onnx/blob/master/rust-api-examples/examples/offline_speaker_diarization.rs)
- [AppFlowy OpenAPI](https://github.com/AppFlowy-IO/AppFlowy-Docs/blob/main/documentation/appflowy-cloud/openapi/README.md), [official server routes](https://github.com/AppFlowy-IO/AppFlowy-Cloud/blob/main/src/api/workspace.rs), and [editor import support](https://github.com/AppFlowy-IO/appflowy-editor/blob/main/documentation/importing.md)

Future dependencies are candidates, not claims of working integrations. Recheck versions and deployed contracts when implementing each phase; lock resolved versions for reproducible builds.
