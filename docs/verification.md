# Verification guide

This document describes how the application is verified, what each layer covers, and the known limits. Test counts change as the suite grows, so run the commands below for current numbers rather than relying on figures recorded here.

## Automated checks

| Check | Command |
| --- | --- |
| TypeScript and production frontend build | `npm run build` |
| Browser UI tests (Playwright; installed Edge on Windows, Chromium elsewhere) | `npm test` |
| Browser extension logic tests | `npm run test:extension` |
| Rust unit and integration tests | `cargo test --locked` in `src-tauri` |
| Clippy, all targets (kept at zero warnings) | `cargo clippy --all-targets --locked` in `src-tauri` |
| Formatting | `cargo fmt --check` in `src-tauri` |
| Windows packaged debug build, no installer | `npm run tauri -- build --debug --no-bundle` |

The build uses locked npm and Cargo dependencies. The packaged debug executable (`src-tauri/target/debug/meeting-recorder.exe` on Windows) is unsigned, embeds the frontend assets, and depends on the DLLs beside it in the target directory, so it is not a standalone distributable. The native smoke scripts write screenshots to `artifacts/` and Playwright writes to `test-results/`; both are git-ignored.

Tests that need real hardware, models, or local services (native diarization inference, real Ollama summaries, voice profiles, a Whisper model) are `#[ignore]`d and gated by environment variables, so `cargo test` never depends on them. Each such test file documents how to opt in.

## What the automated suites cover

### Application shell

- Packaged native UI and Rust IPC, tray creation, close-to-tray, second launch restoring the existing window, and settings persistence across a WebView reload and a full process restart.
- Invalid settings never replace saved preferences; corrupt SQLite bytes are preserved and a failed open can be retried.
- UI initialization failure, retry, failed save, and error escaping (with mocked IPC); browser preview disables desktop mutations; the 780 px layout has no horizontal overflow.

### Device management

- Enumeration of real endpoints (inputs and outputs, with directions matching the OS), default formats, multi-source selection, and stable IDs across refreshes. These checks query default formats and persist selections only; no capture stream is opened.
- Duplicate or unknown source rejection; stale preferences cannot erase sources; selections survive a full restart.
- Simulated device disconnect/reconnect and inventory failure preserve draft names, selections, focus, and other unsaved preferences; a failed selection save can be retried with the draft retained; an unchanged inventory refresh keeps the existing controls rather than replacing the DOM.
- Escaped device names and IDs; unsupported outputs shown as disabled.

### Recording

Synthetic tests drive the real coordinator with fake capture back ends:

- Two-source start and stop; duplicate start, empty selection, and partial second-device failure (no overlapping sessions, built handles released).
- Graceful shutdown drains writers and completed history survives reopening.
- Stream failure, dropped samples, and malformed packet or writer failure finalize the capture as failed rather than clean.
- A recoverable driver discontinuity (WASAPI `Xrun`) lets capture continue with a quality warning and marks the saved session `interrupted`; fatal stream or device errors stop the capture.
- Bounded WAV writer drain, silence gaps, the RIFF size cap (a recording at the cap stops with a retained partial file), exclusive file creation, and idle checkpoints.
- WAV stale-header and partial-tail repair, repeated recovery, SQLite recovery with isolation of corrupted rows, preserved invalid database bytes, and preservation after a failed write.
- Clock and meter behavior: timestamp jitter, epoch re-anchoring, forward-gap retention, late loopback callbacks, long drift, bounded future jumps, finite and stale meter values, zero-time callback heartbeats, peak expiry, per-source mapping, and clearing levels on stop.
- Recording library: rename, remove, and restart confirmation behavior, retention wording, separate audio-quality and transcript states, and pending-dialog guards.

### Detection

All capture effects in the detection tests are mocks; detection alone never opens capture streams, and every detected session requires a current approval token.

- Debounce, per-request approval, and stale or duplicate token rejection.
- Brief signal loss, end grace, and unknown-detector timeout; only owned meeting recordings are stopped.
- Manual stop and dismiss suppression, including the durable restart latch and re-opening after clearing it.
- Settings or journal read/write failures make capture requests fail safely while explicit manual capture remains available.
- Failed start and failed stop retry (no repeated start attempts; a failed stop retains the owned recording ID), device or engine failure reconciliation, replacement manual sessions, observation gaps, and the shutdown barrier (no late start after shutdown).
- A read-only native sample test, run explicitly, enumerates session, process, and window metadata and returns a normalized snapshot. It does not open audio or camera streams or read browser profiles.

Windows detection is heuristic: muted or background meetings can be missed, unrelated browser microphone use can coincide with a Meet window, and Slack microphone use is not necessarily a huddle. macOS and Linux report detection as unsupported; manual capture remains available. See [detection behavior](detection.md) and [platform limits](detection-platforms.md).

### Transcription and meetings

Isolated metadata, model, transcript, and worker fakes cover selected legacy-recording opt-in, duplicate manual requests, completed-job no-ops, disabled-session non-mutation, invalid, active, or failed recording rejection, waiting-for-model behavior, pinned retry models, and `meetings` storage checkpoint recovery. A public speech fixture can be run through a real local Whisper model as an opt-in test; real meeting audio quality and long-duration performance are not covered by automated tests.

## Native smoke scripts (Windows)

`scripts/native-smoke.mjs`, `scripts/native-audio-smoke.mjs`, and `scripts/native-lifecycle.mjs` exercise the actual desktop executable through WebView2's local debugging port. Launch only a development test instance, as described in the [README](../README.md), with `MEETING_RECORDER_DISABLE_AUTOMATION=1` and `MEETING_RECORDER_DISABLE_TRANSCRIPTION=1` so no automatic capture or transcription runs. The scripts verify both safeguards before changing settings and restore saved preferences afterwards. Do not run them while a real recording is in progress.

What they verify:

- Packaged native shell and settings IPC, tray creation, empty-source rejection, and expired or dismissed approval rejection (recording state and history unchanged), without opening streams.
- Close-to-tray, single-instance restore, and a full restart that retains settings, history, and device selections without resuming capture.
- Real endpoint inventory (typically a mix of physical inputs, outputs, and any virtual audio cable the OS exposes), default formats, multi-source save, duplicate and unknown source rejection, and persistence across restart. Streams are not opened.

## Consented hardware capture check

`scripts/native-recording-smoke.mjs --allow-capture` records for about five seconds from one input device and the default output (loopback). It requires explicit consent for microphone and system-audio capture, refuses to run if another recording is active, and inspects only the 44-byte WAV headers and file metadata; the audio is never played, transcribed, or uploaded. Pick the input with `--mic <name-substring>` or `MIC_NAME`; otherwise it uses the default input. It writes a screenshot to `artifacts/`.

Pass criteria:

- The session lasts roughly five seconds (the script asserts 4 to 6.5 seconds) rather than stopping early.
- A second start is rejected while recording; stopping finalizes both WAV files and each RIFF length matches its actual file size.
- The microphone track reports frames written, and exactly one history entry exists for the recording ID, including after a UI reload.
- Source selections are restored afterwards.

A sample result from one such run on Windows, using a webcam microphone and the default USB audio interface output at 48 kHz stereo:

| Source | File size | Timeline frames | Queue-dropped samples | Device discontinuities |
| --- | --- | --- | --- | --- |
| Webcam microphone | 961,268 bytes | 240,306 | 0 | 1 |
| Default speaker output (loopback) | 961,044 bytes | 240,250 | 0 | 1 |

The discontinuities did not stop capture. The session is correctly marked `interrupted` with a possible-gap warning per source; that is not a claim of gap-free audio.

This check also exposed a real defect: an earlier implementation treated WASAPI buffer-discontinuity (`Xrun`) warnings as fatal and stopped after 44 ms. The failed session was retained with its stable ID and error metadata rather than falsely marked recorded. The fix (continue on recoverable discontinuities, surface a warning, mark the session `interrupted`) is covered by a synthetic regression test.

## Known limits

- Not verified: physical device unplug and replug, actual OS sleep/wake, forced process kill during hardware capture, long-duration soak runs, and native macOS or Linux execution. macOS has a microphone usage description and the Linux build keeps its ALSA dependencies, but that does not establish working native capture on those platforms.
- No CI is configured in this repository; cross-platform builds and tests must be run locally.
- Unsupported macOS and Linux output capture is explicitly disabled; their input devices can still be selected.
- Real-call acceptance of detection, tray interaction through the actual OS menus, OS folder opening, and native accessibility checks require the manual runs in [manual acceptance checks](manual-checks.md).
- Cross-device drift correction, RF64 or chunk rollover for recordings past the WAV size cap, and live AppFlowy Cloud behavior (as opposed to a fake server) are not covered.
- A Windows debug build cannot replace `meeting-recorder.exe` while an instance of it is running; quit the app before rebuilding.
