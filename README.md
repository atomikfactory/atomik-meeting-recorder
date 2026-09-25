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
- Windows WASAPI loopback capture, and Linux PulseAudio/PipeWire monitor-source capture of system output; other platform limits are explicit
- Local SQLite preferences; no runtime cloud requests
- Manual start/stop from the window or tray, recording timer and active tray indicator
- Separate PCM16 WAV tracks, UUID recording IDs, SQLite metadata, and restart recovery
- Local Windows meeting hints, configurable confirmation/end grace, and approval before each detected recording
- Optional local-only browser extension (Chrome/Edge/Firefox) for Google Meet, Microsoft Teams web, and Zoom web detection, including background tabs, connected via native messaging (no network port) from Settings; extends meeting detection to Linux/macOS for the platforms it covers. Same ask-first approval as native detection; see [browser extension](docs/browser-extension.md)
- Local Whisper model import (tiny/base/small/medium/large/**turbo**), serial background transcription, retry/progress UI, timestamped Markdown, and crash-safe transcript checkpoints. **large-v3-turbo is recommended**, especially quantized (e.g. `ggml-large-v3-turbo-q5_0.bin`, ~574 MB from the [whisper.cpp Hugging Face repo](https://huggingface.co/ggerganov/whisper.cpp)); import it in Settings, no in-app downloads. Waiting (not-yet-claimed) jobs follow the currently selected model size
- Transcription language setting: detect automatically, or pin one of several supported languages
- Silence skipping and anti-hallucination decode settings, reducing wasted decode time and repeated/hallucinated Whisper text on near-silent audio
- Optional offline acoustic echo cancellation of the microphone against loopback (an independent Rust implementation modeled on WebRTC AEC3's design, not a binding to it -- see `src-tauri/src/echo_cancel/LICENSE-WEBRTC` for the BSD-3 attribution this required), plus a separate text-level duplicate-line cleanup pass after transcription; both toggle independently in Settings
- x86-64-v3 (AVX2) CPU baseline for local transcription, and an optional CUDA build for GPU transcription (see Build and verify below)
- Optional local speaker-identification stage with separately imported segmentation and voice models (verified against real audio; see [diarization](docs/diarization.md#verified-with-real-audio)); it never blocks transcription and runs only on remote (loopback) audio, labeling distinct voices "Speaker 1", "Speaker 2", …
- Transcript Markdown and summaries label your own microphone lines with a name you choose in Settings (default "You") and remote lines "Speaker N"/"Remote", instead of raw device names; see [speaker labels](docs/transcription.md#speaker-labels)
- Optional local meeting summaries (title, key points, decisions, action items, open questions) via a local [Ollama](https://ollama.com) server, loopback-only so summaries never leave this device, with section headings localized to the summary's language (English/French so far); see [local summaries](docs/summary.md)
- Optional lossless FLAC compression of a finished recording's WAV tracks once transcription and speaker identification no longer need the raw audio, verified bit-exact before the WAV is ever removed; see [recording behavior](docs/recording.md#storage-lossless-flac-compression-of-finished-audio)
- Search across every recorded meeting's transcript and summary at once, from a search box at the top of the library, backed by a local SQLite FTS5 index (diacritic-insensitive, as-you-type prefix matching, highlighted snippets); see [search](docs/search.md)
- Configurable recordings folder in Settings (e.g. a bigger data drive): a validated OS folder picker, "Reset to default", and "Open folder"; new recordings go there immediately, and an explicit "Move existing recordings here" action copies-verifies-commits-deletes each one crash-safely. Databases and imported models always stay in the app's own data folder; see [recording behavior](docs/recording.md#storage-a-configurable-recordings-folder)
- Optional AppFlowy Cloud export of a meeting's transcript and summary (never audio), off by default, email-code sign-in, building `My daily meetings / <date> / <time — title>`; strictly create-only by construction (a hard-coded allowlist refuses every update/delete/append route before any request is sent) with idempotent retries and a manual "Export again" that always creates a new page; see [AppFlowy export](docs/appflowy.md)

To record, open Settings, select and save your sources, then use Start recording or approve a detected meeting. Meeting detection is enabled by default, but it never starts recording without your approval. Closing to the tray keeps recording on supported Windows/macOS setups; Quit finalizes local files. Audio stays local. Obtain participant consent before recording.

See [audio device management](docs/audio-devices.md) for platform differences and identity guarantees. Linux builds additionally require ALSA development headers, libpulse's development headers, and pkg-config (`libasound2-dev libpulse-dev pkg-config` on Ubuntu) -- system-output capture there talks to PulseAudio/PipeWire's `pipewire-pulse` compatibility layer at runtime (`libpulse.so.0`, present on essentially every desktop distro).

## Current limits

- Native (non-browser) meeting detection is Windows-only and heuristic; macOS/Linux retain manual recording unless the browser extension is connected for a covered meeting platform
- The browser extension covers Google Meet, Microsoft Teams web, and Zoom web only; it must be connected from Settings and then loaded unpacked/temporarily in the browser (unsigned Firefox add-ons don't persist across a Firefox restart -- see [browser extension limitations](docs/browser-extension.md#limitations))
- Speaker identification is optional, runs only on remote (loopback) audio, and labels an uncertain turn "Remote" rather than guessing; it does not infer real names or merge identities across audio sources or recordings
- Whisper and diarization models must be imported explicitly; native diarization inference has been verified on Windows against real recordings, but not yet on macOS/Linux
- Native system-output capture is Windows- and Linux-native (WASAPI loopback; PulseAudio/PipeWire monitor sources); macOS still requires exposed input/virtual devices until a ScreenCaptureKit adapter is added. The Linux path is compile-checked and unit-tested in CI but not yet verified against a real PulseAudio/PipeWire session
- Independent device clocks are not mixed; each source retains its own track. Acoustic echo cancellation now estimates and corrects for delay/drift between the microphone and loopback clocks when cancelling echo, but this does not merge or align the tracks themselves
- AppFlowy export sends the native "AI Meeting Notes" block, which currently renders in AppFlowy Web (and any client that has shipped it); a standard-blocks fallback setting exists for clients that render it blank. A single meeting's exported content is capped (very long transcripts are truncated in the exported page only; the local transcript/summary files are always complete)
- No cloud requests, analytics, telemetry, or downloaded fonts, other than the AppFlowy Cloud requests this integration makes once a user explicitly enables and signs in to it

The dashboard labels unavailable capabilities directly. The existing automatic-recording preference now enables meeting detection and approval requests, honoring the user's ask-first preference. Settings and recording metadata persist locally; invalid updates do not replace valid saved preferences, and corrupt saved data is surfaced rather than reset. See [detection and approval behavior](docs/detection.md) and [recording behavior and limits](docs/recording.md), especially the difference between a clean recording and retained audio with gaps.

In Settings, choose a trusted Whisper GGML `.bin` model file and select the matching model size (multilingual `base` is the default). New recordings enter the processing queue after capture; missing models leave jobs waiting. Recordings created before this feature are never automatically transcribed. See [local transcription](docs/transcription.md) for supported formats, recovery, and current limitations.

See [the architecture notes](docs/architecture.md) for the current boundaries and future implementation plan.

## Build and verify

With Cargo on PATH:

```text
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --locked
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked
npm run tauri -- build --debug --no-bundle
```

`cargo clippy --all-targets --locked` is kept at zero warnings. On Windows the executable is `src-tauri/target/debug/meeting-recorder.exe`. It loads packaged assets and does not require a running development server. This is an unsigned development build, not an installer. Native macOS/Linux verification is still required; a CI matrix is provided for those platforms.

This repo also supports a project-local Rust toolchain under `.tools` (Cargo, rustup, and libclang), set up so contributors don't need a system-wide install; `scripts/dev.ps1` discovers it automatically and falls back to your normal `PATH` otherwise.

### Optional GPU (CUDA) build

`npm run tauri:cuda` (dev) or `npm run tauri:build:cuda` (release), or `scripts/dev.ps1 -Cuda`, build with GPU (CUDA) Whisper transcription instead of CPU-only. This needs the CUDA Toolkit (12.x or 13.x, with Visual Studio integration on Windows) installed separately from the driver; it is not part of default builds, so CI and non-NVIDIA machines are unaffected. At runtime, a CUDA build needs the CUDA runtime DLLs (`cudart64_*.dll`, `cublas64_*.dll`, ...) reachable, either via the Toolkit's `bin` on `PATH` or copied beside the built executable. See [local transcription](docs/transcription.md) for the CPU-vs-GPU wall-time comparison and fallback behavior when no compatible GPU is found.

#### Packaging a Windows CUDA installer or portable build

`npm run tauri:build:cuda` (`tauri build --features cuda --config src-tauri/tauri.cuda.conf.json`) produces a self-contained NSIS installer at `src-tauri/target/release/bundle/nsis/Meeting Recorder_<version>_x64-setup.exe`. Windows bundling is restricted to `["nsis"]` (`src-tauri/tauri.windows.conf.json`) since that is the target that builds cleanly here; macOS/Linux bundling is untouched. `src-tauri/build.rs` copies the CUDA redistributable DLLs (`cudart64_13.dll`, `cublas64_13.dll`, `cublasLt64_13.dll` — the last one is large, ~470 MB) beside the built executable when the `cuda` feature is on; `tauri.cuda.conf.json` and `tauri.windows.conf.json` together list every DLL the release exe loads (those three plus `onnxruntime.dll`, `onnxruntime_providers_shared.dll`, `sherpa-onnx-c-api.dll`, `sherpa-onnx-cxx-api.dll`) as bundle resources so the installer carries them into the install root next to the exe. `nvcuda.dll` is deliberately never bundled — it ships with the NVIDIA display driver and must already be present on any CUDA-capable machine. `cudart`, `cublas`, and `cublasLt` are redistributable under NVIDIA's CUDA EULA; see NVIDIA's license terms for the CUDA Toolkit for the current redistribution list. The default NSIS `installMode` (per-user, no admin) and Tauri's default WebView2 bootstrapper install mode are both left as-is.

`scripts/package-portable.ps1` (optionally `-Build` to also run the CUDA release build first) assembles a portable, no-installer folder at `dist-portable/MeetingRecorder-<version>-cuda-portable/` (the exe plus the same DLL set, and a `README-PORTABLE.txt` listing requirements: an NVIDIA driver supporting CUDA 13, and the WebView2 Runtime already present on Windows 10/11) and zips it to `dist-portable/MeetingRecorder-<version>-cuda-portable.zip`. `dist-portable/` is git-ignored. The portable folder is self-contained, but application data (recordings, transcripts, settings) still lands in `%APPDATA%\com.local.meetingrecorder` regardless of where the exe runs from — this is not a portable-data mode.

Both artifacts are unsigned, so Windows SmartScreen shows an unrecognized-publisher warning on first run ("More info" → "Run anyway"); this is expected without a code-signing certificate. The CUDA build only benefits NVIDIA GPUs — it silently falls back to the CPU path elsewhere (see above).

`npm test` runs browser UI smoke tests (installed Microsoft Edge on Windows; Chromium elsewhere). For another browser, set `PW_CHANNEL`; install Chromium with `npx playwright install chromium` before selecting it. Browser preview via `npm run dev` is explicitly marked and disables desktop actions. It cannot record, persist settings, or open folders. `npm run test:extension` runs the browser extension's own dependency-free `node --test` suite (`browser-extension/test/`) against its pure detection/title/tab-id logic.

The optional Windows scripts `scripts/native-smoke.mjs`, `scripts/native-audio-smoke.mjs`, and `scripts/native-lifecycle.mjs --audio` test the actual desktop executable using WebView2's local debugging port (9223). Launch only a development test instance with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223`, `MEETING_RECORDER_DISABLE_AUTOMATION=1`, and `MEETING_RECORDER_DISABLE_TRANSCRIPTION=1`. The latter two inhibit automatic capture and all transcription job execution without changing saved preferences; scripts verify both safeguards before modifying settings. The lifecycle script sets them on restarted test processes. These scripts temporarily change preferences and the lifecycle test quits/restarts the app; they are not application startup behavior. Do not enable the debugging port for normal use. Audio checks enumerate hardware and save selections without opening streams.

See [manual acceptance checks](docs/manual-checks.md) for tray, permissions, and hardware checks. Recording and transcription have synthetic lifecycle, recovery, and idempotency tests. Remote-import idempotency remains future work; no AppFlowy requests are made.

`scripts/native-recording-smoke.mjs --allow-capture` is a separate, explicitly consented five-second Windows capture check. It selects the C922 webcam and default output on this development machine, retains the audio locally, and inspects headers only. Do not run it during a real recording or without microphone/system-audio consent. Synthetic tests do not open hardware.
