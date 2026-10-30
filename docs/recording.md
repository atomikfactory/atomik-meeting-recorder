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

Databases and imported models always live under the OS application-data directory (never affected by the configurable recordings folder below):

```text
recordings/
  recordings.sqlite3
  transcription.sqlite3 · diarization.sqlite3 · summary.sqlite3 · compression.sqlite3 · search.sqlite3
  models/                 # imported Whisper/diarization models
  meetings/                # the default location for meeting folders -- see below
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

## Storage: a configurable recordings folder

**Settings → Recordings folder** lets the user choose where meeting folders (audio tracks, `transcript.json`/`.md`, `summary.json`/`.md`) are stored -- typically a bigger data drive or a synced folder. Databases and imported models are deliberately *not* affected by this setting and always stay under the app data directory: SQLite on a possibly-removable or synced folder risks corruption from partial syncs or surprise unmounts, while the databases are small and not something a user needs on another drive.

`Settings.recordings_folder: Option<String>` is `None` by default (meaning the location above, unchanged from before this feature). Setting it is a two-step, explicit action, never implicit:

1. **Change…** opens the OS folder picker (`tauri-plugin-dialog`) and validates the choice before saving: an absolute path, real or creatable, actually writable (a probe file is written and removed), not a symlink/junction, not inside the application's own install directory, and not the root of a drive/filesystem. **New recordings go to the chosen folder immediately** -- this alone does not touch any existing recording. **Reset to default** clears the setting back to `None`, immediately, the same way. Both remember the folder they replaced (`legacy_library_roots`, capped and de-duplicated) purely so recordings under it remain resolvable.
2. **Move existing recordings here** is a separate, explicit action (`move_recordings_to_configured_folder`) that copies each finalized (`recorded`/`interrupted`, never active) meeting folder to the new root one at a time, crash-safely (`library_move.rs`): copy the whole folder → verify every file byte-for-byte (size + SHA-256) against the source → update that one recording's metadata in a single database write → only then delete the source folder. Nothing is ever deleted before its copy is verified. A small on-disk journal (one JSON file per in-flight recording under `library_move_journal/`) makes an interrupted move resumable: a crash mid-copy simply redoes the copy from scratch (the source was never touched yet), while a crash after verification redoes only the now-idempotent metadata update and delete. The whole action takes the same processing guard used by transcription/diarization/compression, so it refuses to run during an active recording or while another job is running, and does not compete with them for I/O. A recording made before this feature (no `storage_path`, i.e. the flat legacy `<uuid>/` layout) cannot be moved automatically and is reported as such.

Every place that resolves a recording's folder -- transcription, diarization, summarization, compression, the search indexer (including its summary-text and speaker-label enrichment), "Open folder", and crash recovery -- validates a recording's saved library root against the *known* roots (the default location, the currently configured folder, and any folder it replaced) rather than trusting an arbitrary path from the database; the existing symlink/parent/basename checks in `recording_paths::resolved_recording_directory` still apply on top of that. If the configured folder becomes unreachable (its drive unplugged, a network share dropped), starting a new recording fails with a clear error rather than silently falling back to the default, and existing recordings that live under it simply show that folder's operations (open, transcribe, ...) as unavailable -- their database rows, and the folder's own files, are never touched or forgotten.

## Global recording shortcut

A single OS-wide keyboard shortcut toggles recording, registered from Rust (`tauri-plugin-global-shortcut`) rather than the webview, so it works even while the main window is hidden in the tray. Default: **Ctrl+Alt+R** (`CommandOrControl+Alt+R`). Change it in **Settings → Global recording shortcut**: click the shortcut button, then press the new combination (Escape cancels; Backspace clears it), or turn the feature off entirely with the checkbox next to it. Saving re-registers (or unregisters) the shortcut immediately -- no restart needed.

Pressing the shortcut looks at meeting detection's current state and does exactly what the Start/Stop button or tray menu item would do in that state:

- **Idle** (nothing recording, no meeting awaiting approval) -- starts a manual recording with the currently saved sources, the same path as Start. If no sources are saved (or another start error occurs), the window is brought forward and the same error the Start button would show is displayed.
- **Recording** -- stops and finalizes, the same path as Stop.
- **A detected meeting is awaiting your approval** -- pressing the shortcut approves it (starts that recording) instead of starting an unrelated manual one.

Repeated presses within one second of an accepted press are ignored, so a single physical key-down that the OS reports more than once (or a user holding the combination down) cannot fire the toggle twice.

The accelerator must include at least one modifier (Ctrl/Alt/Shift/Win, or Cmd on macOS) plus exactly one non-modifier key, and a small set of OS/window-manager reserved combinations (Ctrl+Alt+Delete, Alt+F4, Win+L) are always rejected. If the OS refuses to register the shortcut -- most commonly because another application already owns it -- Settings shows that error text directly under the shortcut field; every other saved preference is unaffected.

**Linux/Wayland caveat**: `tauri-plugin-global-shortcut` is built on OS global-hotkey APIs that X11 supports directly; Wayland compositors frequently do not expose a portal for arbitrary global shortcuts. Where registration is unsupported, it fails the same way a shortcut conflict does -- reported as a clear error in Settings, not silently ignored -- and manual recording from the window or tray remains fully available either way.

## Meeting management and status meaning

**Rename** changes only the saved display title. It never renames a recording folder, moves WAV files, changes the stable recording ID, or invalidates a transcript checksum.

**Remove** hides the finalized recording from in-app history and worker selection. Its locally retained folder, WAV tracks, and metadata are deliberately not deleted, and this version has no in-app restore action. The confirmation wording therefore says that audio remains recoverable rather than promising deletion.

Capture status and transcript status are separate. A recording can be `interrupted` because a device reported a discontinuity or timestamp adjustment while still having a completed, locally generated transcript. Conversely, a failed capture can retain usable audio; **Transcribe** is offered only when finalized tracks contain retained frames. Meter levels show callback delivery and amplitude, not speech, transcript success, or meeting presence.

**Retry transcription** retries a failed job with its previously pinned model. **Restart transcription** is an explicit replacement of a completed or dormant transcript: it uses the currently selected local model, archives the previous transcript outputs first, and does not capture audio again. A queued or running transcription cannot be restarted concurrently.

To choose a model, open **Settings**, select the intended **Whisper model** key, select **Choose local Whisper model**, choose a local GGML `.bin` file, then save the settings. During development, a public base model can optionally be kept under the git-ignored `.tools/public-whisper/` directory and chosen explicitly from there. It is not imported or downloaded automatically. If a selected recording is waiting for a model, importing that matching model wakes its queued job.

Automated synthetic and mocked tests are distinct from hardware acceptance. Physical unplug, sleep/wake, long-duration drift, and macOS/Linux permission behavior require native verification. See [verification results](verification.md) for checks actually run.

## Deleting meetings: list vs disk

The library supports two, deliberately different, kinds of deletion, selectable per-meeting or in bulk over a multi-select:

- **Remove from list** (`delete_recording`) is the soft delete described above: it hides the meeting from history and worker selection, but its folder, WAV tracks, transcript, summary, and any AppFlowy export stay exactly as they are on disk. Use this when you just want the meeting out of the way.
- **Delete from disk** (`delete_recordings_from_disk`, bulk-only) is destructive: it removes the meeting's whole folder from disk -- moved to the OS Recycle Bin/Trash by default (recoverable through the OS, via the [`trash`](https://crates.io/crates/trash) crate, MIT/Apache-2.0), or permanently deleted when "Delete permanently" is chosen (irreversible; the confirmation dialog requires typing `DELETE` or the exact number of meetings being deleted before this mode can be confirmed). Either way, it also removes the meeting's rows from every local store: recordings, transcription, diarization, summary, compression, and the local AppFlowy job queue.

Delete from disk refuses (and reports as skipped, never as a batch-wide error) any meeting that is currently being captured, or has a transcription/diarization/summarization/compression/AppFlowy-export job actively running for it right now; a queued, completed, or failed job never blocks it. Each meeting's folder is resolved through the same known-library-roots validation used everywhere else a recording's folder is touched (canonicalized, symlink/junction-rejecting, confirmed to be a direct child of a recognized recordings library) before anything is touched, so a tampered or stale path can never cause deletion outside a recording's own folder. The recording is tombstoned in the database *before* its folder is touched, reusing the same tombstone mechanism "Remove from list" already relies on for crash safety: a crash mid-deletion leaves at worst a folder that failed to be removed, never a database row pointing at audio that may already be gone, and startup recovery only ever repairs *unfinished* (still-recording) rows, so a tombstoned meeting is never resurrected.

**What is never touched:** Delete from disk never calls AppFlowy over the network, and never removes a row from the AppFlowy pages journal (the create-only ledger of pages this app has already exported). A meeting page already exported to AppFlowy stays exactly where it is in AppFlowy; only this app's own local bookkeeping for that export job is removed.

## Primary references

- [CPAL 0.18.2 device/stream API](https://docs.rs/cpal/0.18.2/cpal/traits/trait.DeviceTrait.html)
- [WASAPI loopback limitations](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [Apple microphone usage description](https://developer.apple.com/documentation/bundleresources/information-property-list/nsmicrophoneusagedescription)

The pinned CPAL WASAPI implementation also documents transparent loopback when an output device is opened as an input stream. Capture details and test results should be read against the locked dependency version, not assumed from another platform's behavior.
