# Automatic recording lifecycle

This increment connects local meeting hints to the existing recording coordinator. It does not implement transcription, Markdown, or AppFlowy synchronization. Manual Start and Stop remain available independently of detector support.

## Behavior

Per the user's preference, meeting detection is enabled by default but **each detected recording requires approval**. The existing `auto_record` preference enables detection and recording requests; it no longer means unattended capture. Choose and save audio sources before using it. A supported meeting hint must persist for the configured start debounce before the app asks “Record this meeting?”. No capture starts until approval. Dismissing the request suppresses it for the current episode. The detector does not capture microphone samples to decide whether another application is using a microphone.

After an approved recording starts, disappearance of meeting evidence starts the end grace rather than stopping immediately. A returning hint cancels the end countdown. Approval is tied to the current request, not reusable permission for future meetings; stale or cancelled requests must not open devices.

The dashboard distinguishes waiting, confirmation, approval requests, meeting capture, end grace, suppression, and detector failure. A manual recording is not owned by the detector and must not be stopped because meeting evidence disappears. Manual Stop suppresses another request until the current episode has been absent for the grace period. Obtain participant consent before approving recording; local processing is not a substitute for consent.

## Safety and persistence

The detection service uses a clock-driven state machine and serializes manual/automatic capture effects. The recording coordinator retains its independent single-session guard. A separate SQLite `detection.sqlite3` journal under the recordings directory persists the meeting-episode latch before an automatic start. The latch is conservative: a crash between committing intent and starting capture may suppress a recording, but must not cause a duplicate automatic recording on restart. Manual Start remains the escape hatch.

Detector errors are unknown observations, not evidence of an ended meeting. Confirmation must restart after uncertainty. Repeated unknown observations are bounded for an automatically owned capture, while manual capture remains under user control. Recovered episodes require a verified absence before automatic rearming. Abrupt gaps between observations require fresh evidence; synthetic time-gap tests do not establish native suspend/resume behavior on every OS.

When a meeting finishes, WAV finalization and recording metadata persistence run, then the independent local transcription worker may queue the recording when its explicit `auto_transcribe` flag is enabled. The application then becomes ready to detect another episode. AppFlowy import remains separate; saved audio is not advertised as transcribed until its durable job completes or as imported at any time in this increment.

## Platform boundaries

See [native signals and limitations](detection-platforms.md). Windows meeting signals are heuristics, not authoritative conferencing-provider call events. A running process alone does not produce a request. Browser detection cannot reliably observe background tabs without an additional supported browser integration. The user has accepted a local-only Chrome/Firefox extension as a future increment; this release does not install or implement one. No browser profile, browsing history, camera stream, or audio payload is inspected by detection. Normal application runtime makes no network calls.

## Development verification

Set `MEETING_RECORDER_DISABLE_AUTOMATION=1` **before launching** a test app. It disables automatic capture for that process without changing the saved default. Native smoke scripts assert `automation_inhibited` before changing preferences, and lifecycle-test relaunches inherit the safeguard. This variable does not disable explicit manual recording; the separate hardware recording script still requires fresh, bounded user consent.

Rust lifecycle tests use synthetic signals/effects and local test databases. Browser tests mock IPC; they cannot validate OS detection. Any real-call acceptance requires an explicitly planned recording session. Run no additional microphone capture simply to test detection.
