# Meeting Recorder

A local-first desktop meeting recorder MVP with multi-source capture, a native tray, saved device selection, recording history, interrupted-file recovery, and local Whisper transcription. Windows meeting detection asks before recording and manages the end grace after approval.

## Start

Prerequisites: Node.js 22.12+ (Node 20.19+ also satisfies Vite's minimum), Rust stable, CMake, and libclang for Whisper's build-time bindings. Windows needs Microsoft C++ Build Tools and WebView2; Linux needs the WebKitGTK and related Tauri development packages; macOS needs Xcode Command Line Tools. See [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) and [transcription build/setup](docs/transcription.md).

```powershell
npm ci
npm run tauri -- dev
```

`scripts/dev.ps1` is an optional convenience launcher when a project-local Rust installation exists under `.tools`; it otherwise uses your normal PATH.

```powershell
.\scripts\dev.ps1
npm run build
```

## Available now

- Native tray, dashboard, settings, and single-instance lifecycle
- Microphone/interface discovery, input/output direction, default formats, and device errors
- Multiple saved audio sources, refresh, and retention of disconnected selections
- Windows WASAPI loopback capture; other platform limits are explicit
- Local SQLite preferences; no runtime cloud requests
- Manual start/stop from the window or tray, recording timer and active tray indicator
- Separate PCM16 WAV tracks, UUID recording IDs, SQLite metadata, and restart recovery
- Local Windows meeting hints, configurable confirmation/end grace, and approval before each detected recording
- Local Whisper model import, serial background transcription, retry/progress UI, timestamped Markdown, and crash-safe transcript checkpoints
- Optional local speaker-identification stage with separately imported segmentation and voice models; it never blocks transcription

To record, open Settings, select and save your sources, then use Start recording or approve a detected meeting. Meeting detection is enabled by default, but it never starts recording without your approval. Closing to the tray keeps recording on supported Windows/macOS setups; Quit finalizes local files. Audio stays local. Obtain participant consent before recording.

See [audio device management](docs/audio-devices.md) for platform differences and identity guarantees. Linux builds additionally require ALSA development headers (`libasound2-dev` and `pkg-config` on Ubuntu).

## Current limits

- Native meeting detection is Windows-only and heuristic; macOS/Linux retain manual recording
- Chrome/Firefox background-tab detection needs a later local-only browser extension
- Speaker identification is optional and labels uncertain turns as unknown; it does not infer names or merge identities across audio sources
- Whisper and diarization models must be imported explicitly; native diarization inference has not yet been verified
- Native system-output capture is Windows-only; macOS/Linux require exposed input/virtual devices for now
- Independent device clocks are not drift-corrected or mixed; each source retains its own track
- No AppFlowy connection or export
- No cloud requests, analytics, telemetry, or downloaded fonts

The dashboard labels unavailable capabilities directly. The existing automatic-recording preference now enables meeting detection and approval requests, honoring the user's ask-first preference. Settings and recording metadata persist locally; invalid updates do not replace valid saved preferences, and corrupt saved data is surfaced rather than reset. See [detection and approval behavior](docs/detection.md) and [recording behavior and limits](docs/recording.md), especially the difference between a clean recording and retained audio with gaps.

In Settings, choose a trusted Whisper GGML `.bin` model file and select the matching model size (multilingual `base` is the default). New recordings enter the processing queue after capture; missing models leave jobs waiting. Recordings created before this feature are never automatically transcribed. See [local transcription](docs/transcription.md) for supported formats, recovery, and current limitations.

See [the architecture notes](docs/architecture.md) for the current boundaries and future implementation plan.

## Build and verify

With Cargo on PATH:

```text
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --locked
npm run tauri -- build --debug --no-bundle
```

On Windows the executable is `src-tauri/target/debug/meeting-recorder.exe`. It loads packaged assets and does not require a running development server. This is an unsigned development build, not an installer. Native macOS/Linux verification is still required; a CI matrix is provided for those platforms.

`npm test` runs browser UI smoke tests (installed Microsoft Edge on Windows; Chromium elsewhere). For another browser, set `PW_CHANNEL`; install Chromium with `npx playwright install chromium` before selecting it. Browser preview via `npm run dev` is explicitly marked and disables desktop actions. It cannot record, persist settings, or open folders.

The optional Windows scripts `scripts/native-smoke.mjs`, `scripts/native-audio-smoke.mjs`, and `scripts/native-lifecycle.mjs --audio` test the actual desktop executable using WebView2's local debugging port (9223). Launch only a development test instance with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223`, `MEETING_RECORDER_DISABLE_AUTOMATION=1`, and `MEETING_RECORDER_DISABLE_TRANSCRIPTION=1`. The latter two inhibit automatic capture and all transcription job execution without changing saved preferences; scripts verify both safeguards before modifying settings. The lifecycle script sets them on restarted test processes. These scripts temporarily change preferences and the lifecycle test quits/restarts the app; they are not application startup behavior. Do not enable the debugging port for normal use. Audio checks enumerate hardware and save selections without opening streams.

See [manual acceptance checks](docs/manual-checks.md) for tray, permissions, and hardware checks. Recording and transcription have synthetic lifecycle, recovery, and idempotency tests. Remote-import idempotency remains future work; no AppFlowy requests are made.

`scripts/native-recording-smoke.mjs --allow-capture` is a separate, explicitly consented five-second Windows capture check. It selects the C922 webcam and default output on this development machine, retains the audio locally, and inspects headers only. Do not run it during a real recording or without microphone/system-audio consent. Synthetic tests do not open hardware.
