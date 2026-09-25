# Manual recording — Phase 3

## Scope

This increment adds manual capture, local audio files, recording metadata, recent history, and recovery of interrupted sessions. Meeting detection and local Whisper/Markdown processing are separate workers; diarization and AppFlowy synchronization remain future increments. A saved recording is not an imported meeting.

## Design

A single Rust coordinator owns the recording lifecycle and native streams. Start and stop requests are serialized so the window and tray cannot create overlapping sessions. Start uses the exact saved source IDs; no default-device or same-name substitution is allowed. Missing or unsupported selections must produce an actionable error.

Audio callbacks must not perform disk or network I/O. A bounded handoff feeds background writing; dropped data is surfaced rather than silently reported as a clean recording. Each source is retained as a separate track at its native sample rate/channel configuration. This avoids lossy premature mixing and preserves provenance for later transcription. It does not establish speaker identity or guarantee sample-perfect synchronization across independent device clocks.

Recording identity and processing state belong in SQLite, not filenames alone. Capture output must remain recoverable if the app exits before finalization. Recovery must be repeatable and must never resume microphone capture automatically. Partial or failed sessions remain distinguishable from cleanly stopped recordings.

The WAV helper writes canonical PCM16 headers, checkpoints them at one-second intervals, and drains queued packets before finalizing. Timestamp gaps are represented as silence rather than removed from the timeline. Files use exclusive creation to avoid overwriting existing output. The current RIFF format has a 32-bit length limit; reaching the writer limit is an explicit failure with retained partial audio, not an unchecked wraparound. Long-duration recording and device-clock drift still need hardware soak testing.

Recovery verifies that each tracked WAV belongs directly to its recording directory, repairs stale header lengths and incomplete final PCM frames, and keeps corrupt metadata visible as an error. It does not invent conversation content or treat a recovered partial session as a cleanly completed recording.

The coordinator resolves all selected devices, persists a UUID recording intent, creates writers, builds all streams, and only then starts them. Stop and Quit drop the streams, drain writer queues, finalize headers, and commit metadata before completing. A failed metadata commit remains retryable and prevents a new session from silently hiding it.

Files live under the OS application-data directory:

```text
recordings/
  recordings.sqlite3
  meetings/
    YYYY-MM-DD_HH-mm-ss_meeting_title--<recording-uuid>/
      track-00.wav          # or track-00.flac once compressed (see below)
      track-01.wav
      transcript.json        # after local transcription
      transcript.md          # after local transcription
  <legacy-recording-uuid>/  # retained in place; never renamed
    track-00.wav
    track-01.wav
```

The SQLite recording ID, not a display name, selects a folder. A meeting title is best-effort Windows metadata only: it recognizes visible Meet and Slack meeting windows and does not adopt arbitrary window text or a browser tab title from a background tab. If no supported visible meeting title is available, the folder uses `Meeting`.

Input formats currently handled are float32/float64, signed16/signed32, and unsigned16; output files are normalized PCM16 at each device's native rate and channel count. No cross-device mixing or resampling is performed. A completely silent loopback endpoint may deliver no frames; that is explicitly flagged rather than certified as a good capture.

A driver-reported buffer discontinuity is recoverable: recording continues with a visible quality warning and the saved session is marked `interrupted` (potential gaps), not clean `recorded`. CPAL capture timestamps are evidence for the WAV frame timeline, but packet order remains authoritative when timestamps jitter or reset. A reset is re-anchored to the emitted cursor; a forward gap is retained only when it is consistent with elapsed recording time. Implausibly far-future timestamp jumps are rejected rather than allocating unbounded silence. Device loss, callback queue overflow, writer errors, and stalled microphone callbacks stop capture while retaining available audio. An input watchdog detects ten seconds without callbacks; ordinary silence still supplies microphone frames. This is not a universal OS sleep/wake integration.

Every stream starts behind frame 0 by its own device start-up latency (buffer prefill, USB isochronous fill time), and WASAPI routinely reports a discontinuity on the first buffer or two after `Start()` for exactly that reason. None of this has anything prior to be discontinuous with, so it is not counted: the frame timeline only starts treating a correction as a real gap once a packet has landed on schedule on its own (or, failing that, after a short bounded grace window), and `route_stream_error` mirrors the same rule for driver-reported Xruns. A short, clean recording with no real mid-stream loss now finalizes `recorded`; a genuine gap, reset, or overflow occurring after that point is still retained, counted, and surfaced exactly as before.

While recording, state polling exposes per-source peak/RMS dBFS telemetry, a short peak hold, clipping indication, and callback age. Metering uses finite scalar samples in the audio callback and atomics only; it does not retain samples, query history, write SQLite, or emit callback events. A source with no recent callback is reported stale/silent. These indicators diagnose capture delivery, not speech, meeting activity, recording quality, or speaker identity.

## Platform and privacy boundaries

- Windows inputs use WASAPI. A selected output uses loopback of that endpoint's mix, potentially including other applications and notifications; it is not application-isolated meeting audio.
- macOS inputs use Core Audio with microphone permission and a usage description. Native system audio needs a separate ScreenCaptureKit implementation; output selection is still unavailable.
- Linux inputs depend on ALSA and installed sound-server routing. System-output capture uses a PulseAudio/PipeWire monitor source selected as an output track (see [audio device management](audio-devices.md)); a dedicated thread performs blocking reads on that source and feeds the same WAV-writer/meter/timeline pipeline as CPAL tracks. Unlike WASAPI loopback, a quiet/idle monitor keeps delivering real (silent) frames rather than none, so a silent meeting is not itself flagged as a failed capture; the source disappearing or never opening still is.
- Device discovery and meeting detection do not record. Capture opens only after an explicit manual Start or approval of a detected meeting. The detector owns end-grace stopping only for approved meeting recordings; independently started manual recordings remain under user control. See [detection lifecycle](detection.md).
- No cloud calls, remote transcription requests, summaries, or uploads occur. An explicitly requested local transcription stays on-device. Audio remains in the application-owned recordings folder. Obtain any required participant consent before recording.

## Acceptance checks

1. With no selected sources, Start rejects without creating a recording.
2. Start opens every selected source or reports failure; a second start cannot create another session.
3. Stop drains/finalizes audio, saves metadata, and returns to idle. Recent history reflects actual output.
4. Tray and window share the same active session; closing to tray continues capture, while Quit finalizes it.
5. Unplugging a selected device or a writer failure retains available audio and reports an interruption/failure.
6. Restart repairs unfinished audio once and preserves its recording ID. Recovery does not open capture; meeting detection still requires a fresh approval, and a persisted episode latch suppresses duplicate requests for a previously handled meeting.
7. Different sample rates, silent system audio, buffer gaps, and long durations do not silently produce a misleading clean result.
8. UI polling preserves unsaved settings, keeps errors escaped, and does not claim transcription or AppFlowy success.

Recent history is intentionally limited to the 25 newest rows; it is not an audio-discovery scan. Older recordings remain indexed and remain in their original folders, so **Open folder** on a selected history row always resolves that row's durable metadata rather than guessing from a title.

Manual transcription is available per finalized recording. Selecting **Transcribe** opts in only that retained recording (including a legacy recording whose `auto_transcribe` flag was absent or false); it queues local work with the selected model and never starts capture, downloads a model, or reads unrelated recordings. Completed and pending jobs remain immutable/idempotent, while an explicit retry retains its pinned model.

## Storage: lossless FLAC compression of finished audio

Once a recording is settled (`recorded` or `interrupted`, never while active) and every pipeline stage that still needs the raw audio has itself settled -- transcription `completed`/`failed`, and speaker identification `completed`/`failed`/disabled -- a low-priority background stage (`audio_compress.rs`) losslessly re-encodes each WAV track to FLAC (`flacenc` for encoding, `claxon` for decoding; pure Rust, no system libraries) and removes the now-redundant WAV. It shares the same serialized processing guard as Whisper/diarization, so it never competes with them for CPU, and it is controlled by the **Compress finished recordings to FLAC** setting (default on).

This is intentionally conservative about safety, since it deletes audio:

1. Encode to a brand-new temp file in the same directory as the WAV, then `fsync` it.
2. Decode that temp file back and compare every sample against the original WAV for exact equality. Any mismatch, or any error, deletes the temp file and records a job error -- the WAV is never touched.
3. Only once verified bit-exact: atomically rename the temp file onto its final `.flac` name.
4. Update the recording's metadata (`file_path` for that track) in one SQLite transaction.
5. Only now delete the original WAV.

A crash at any point leaves either the WAV still referenced (with, at worst, an orphaned, harmless temp file) or the FLAC already referenced (with, at worst, a redundant WAV still on disk); startup recovery removes orphan temp files and finishes whichever half of the swap did not complete, and never deletes a WAV unless a verified FLAC is the one the metadata already names. A track shorter than FLAC's own minimum block size (32 samples -- far below any real meeting audio) is left as WAV rather than worked around. Existing recordings are picked up the same way new ones are, on the same eligibility scan.

Every audio reader -- transcription decode, speaker identification, restart, and this stage's own verification -- goes through a small shared `audio_file` abstraction that accepts either format transparently, so compression is invisible to the rest of the pipeline. The dashboard/settings page shows a running "Audio on disk" total, and each library card shows its format and size once compressed (e.g. "FLAC · 12.3 MB (saved 51%)").

## Meeting management and status meaning

**Rename** changes only the saved display title. It never renames a recording folder, moves WAV files, changes the stable recording ID, or invalidates a transcript checksum.

**Remove** hides the finalized recording from in-app history and worker selection. Its locally retained folder, WAV tracks, and metadata are deliberately not deleted, and this version has no in-app restore action. The confirmation wording therefore says that audio remains recoverable rather than promising deletion.

Capture status and transcript status are separate. A recording can be `interrupted` because a device reported a discontinuity or timestamp adjustment while still having a completed, locally generated transcript. Conversely, a failed capture can retain usable audio; **Transcribe** is offered only when finalized tracks contain retained frames. Meter levels show callback delivery and amplitude, not speech, transcript success, or meeting presence.

**Retry transcription** retries a failed job with its previously pinned model. **Restart transcription** is an explicit replacement of a completed or dormant transcript: it uses the currently selected local model, archives the previous transcript outputs first, and does not capture audio again. A queued or running transcription cannot be restarted concurrently.

To choose a model, open **Settings**, select the intended **Whisper model** key, select **Choose local Whisper model**, choose a local GGML `.bin` file, then save the settings. The optional public base model can be chosen explicitly from `.tools/public-whisper/ggml-base.bin` during development. It is not imported or downloaded automatically. If a selected recording is waiting for a model, importing that matching model wakes its queued job.

Automated synthetic and mocked tests are distinct from hardware acceptance. Physical unplug, sleep/wake, long-duration drift, and macOS/Linux permission behavior require native verification. See [verification results](verification.md) for checks actually run.

## Primary references

- [CPAL 0.18.2 device/stream API](https://docs.rs/cpal/0.18.2/cpal/traits/trait.DeviceTrait.html)
- [WASAPI loopback limitations](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [Apple microphone usage description](https://developer.apple.com/documentation/bundleresources/information-property-list/nsmicrophoneusagedescription)

The pinned CPAL WASAPI implementation also documents transparent loopback when an output device is opened as an input stream. Capture details and test results should be read against the locked dependency version, not assumed from another platform's behavior.
