# Browser extension (local-only Meet/Teams/Zoom detection)

An optional, local-only browser extension (`browser-extension/`) lets meeting
detection see Google Meet calls in **background tabs** -- something the
native Windows heuristic (see [detection-platforms.md](detection-platforms.md))
cannot do -- and extends detection to Linux and macOS for the browsers it
covers. Meet is the priority; Microsoft Teams web (`teams.microsoft.com`,
`teams.live.com`) and the Zoom web client (`app.zoom.us/wc`) use the same
mechanism with lighter heuristics. It never records anything by itself: every
detected meeting still goes through the app's existing ask-first approval
flow (see [detection.md](detection.md)).

## What it sends, and what it never sends

On a state change (and a heartbeat every 15s while in a call), the extension
sends exactly:

```json
{ "platform": "meet", "state": "in_call", "title": "Weekly sync", "tab_id": "3f9a2b1c7e4d5061" }
```

- `platform`: `"meet"`, `"teams"`, or `"zoom"`.
- `state`: `"in_call"` or `"ended"`.
- `title`: a best-effort meeting title, cleaned of the platform's window-title
  decoration (mirrors `src-tauri/src/meeting_title.rs`'s approach), or `null`.
- `tab_id`: an **opaque, per-install, non-reversible** token -- a SHA-256 hash
  of the real tab id salted with a random value generated once and stored in
  the extension's own local storage (`browser-extension/tab-id.js`). The real
  browser tab id, the tab's URL (which for Meet embeds the meeting code), page
  content, and participant names are never read for this purpose and never
  sent.

The extension requests only `nativeMessaging` and `storage` permissions, plus
host permissions for the four meeting domains above -- no `<all_urls>`, no
analytics, no remote code, no network requests of its own.

## How it reaches the app: native messaging, not a network port

The extension talks to the app the way browser extensions are meant to talk
to local native software: [native messaging](https://developer.chrome.com/docs/apps/nativeMessaging)
over stdio, with no listening network socket anywhere in the picture.

The **host** is this app's own executable, invoked by the browser as
`meeting-recorder --native-messaging-host` (see `main.rs`). A separate helper
binary was considered and rejected: it would double the artifacts to build,
sign, and register for no benefit, since the flag path shares all of the
framing/validation/rate-limiting code (`src-tauri/src/browser_bridge.rs`) with
the rest of the app and starts in milliseconds.

The host cannot talk to the already-running app's in-memory state directly
(it is a separate OS process each time the browser starts it), so it forwards
each validated message over a second, local-only channel to the running app:

- **Windows**: a named pipe (`\\.\pipe\meetingrecorder-browser-bridge`) created
  with an explicit security descriptor (`D:(A;;GA;;;OW)`) granting access to
  the pipe's owner only -- no other local account, not even other admins.
- **Linux/macOS**: a Unix domain socket under `$XDG_RUNTIME_DIR` (falling back
  to `$TMPDIR`, then `/tmp`), created with `0600` permissions.

Both directions are validated independently: the native-messaging host
validates and rate-limits before forwarding, and the app's IPC server
validates and rate-limits again on receipt (`browser_bridge_ipc.rs`), so a
compromised host process gains nothing beyond what the wire schema already
allows.

### Framing and limits

Native messaging's own stdio protocol is a 4-byte little-endian length prefix
followed by that many bytes of UTF-8 JSON, with a 1&nbsp;MiB cap from either
side. This app's schema needs at most a few hundred bytes, so
`browser_bridge::MAX_MESSAGE_BYTES` (8&nbsp;KiB) is enforced well below that,
and an oversized length is rejected *before* attempting to read the body.
Unknown top-level JSON fields are ignored rather than rejected, so a newer
extension version can add fields without breaking an older installed app.
Both the host and the app's IPC server independently rate-limit (a fixed
window, generous for one heartbeat/15s across a handful of tabs).

### Staleness

An `in_call` observation is treated as fresh for 45 seconds (three missed
15s heartbeats) after the last message; past that, detection treats it as
absent even without an explicit `"ended"` message, so a browser crash or a
killed tab does not leave the app waiting forever for a call that already
ended (see `browser_bridge::STALE_AFTER`).

## Registration

Nothing is registered at startup. In Settings > Browser extension:

- **Connect browser extension** writes a native-messaging host manifest
  (pointing at this app's own executable, with `--native-messaging-host`
  implied by the host's own invocation contract) for Chrome, Edge, and
  Firefox, in each browser's per-user native-messaging-hosts location, and
  (Windows only) a matching `HKEY_CURRENT_USER` registry entry. Every path is
  computed deterministically from the browser and the home directory
  (`src-tauri/src/browser_registration.rs`), so...
- **Disconnect** recomputes those same paths/keys and removes exactly them --
  nothing else -- and is always available and safe to click even after a
  partial or already-undone connect.

The manifests pin this extension specifically: the Chrome/Edge manifest's
`allowed_origins` lists the extension's committed, stable ID
(`browser-extension/manifest.json`'s `key`, derived by
`scripts/generate-extension-key.mjs`); the Firefox manifest's
`allowed_extensions` lists the `browser_specific_settings.gecko.id` from
`browser-extension/manifest.firefox.json`. No other extension can use this
host.

## Installing the extension in your browser

Registering the host does not install the extension itself -- browsers only
allow that from their own store, or as an explicit unpacked/temporary load
during development. Click **Open extension folder** in Settings, then:

- **Chrome / Edge**: open `chrome://extensions` (or `edge://extensions`),
  enable *Developer mode*, click *Load unpacked*, and select the opened
  folder (it uses `manifest.json`).
- **Firefox**: open `about:debugging#/runtime/this-firefox`, click *Load
  Temporary Add-on…*, and select `manifest.firefox.json` in the opened
  folder. Firefox temporary add-ons are removed when Firefox restarts, so
  this step is unfortunately needed again after every Firefox restart
  until the extension is signed and published -- see Limitations below.

## Detection integration

`src-tauri/src/lib.rs`'s `NativeDetectorAdapter` merges the extension's
signal with the existing Windows-only heuristic:

- A fresh `in_call` observation is treated as **high confidence** and takes
  priority over the native heuristic, with the extension's own title
  preferred over the window-title fallback (`meeting_title::best_effort_title`)
  when a recording starts.
- Once the observation ends or goes stale, native detection (if any) resumes
  deciding.
- If native detection is unsupported on this platform (macOS/Linux today) but
  the extension has connected at least once, detection reports itself
  **supported** anyway, purely from the extension's signal -- this is what
  extends Meet/Teams/Zoom detection to Linux and macOS.

Every existing safeguard still applies unchanged: debounce, ask-first
approval, end grace, the SQLite duplicate-episode latch, and "never record
without approval" (see [detection.md](detection.md)). The extension only ever
supplies a *signal*; it has no path to start or stop a recording directly.

## Testing

Three layers, from fastest/narrowest to slowest/broadest:

- **Pure logic** (`npm run test:extension`, `node --test
  browser-extension/test/*.test.mjs`): `content-logic.js`'s `decideState`/
  `cleanTitle` and `tab-id.js`'s `opaqueTabId`, exercised directly with
  hand-derived signal snapshots. No browser involved.
- **Settings UI** (`npm test`, `tests/browser-extension.spec.ts`): the app's
  own Settings page (Connect/Disconnect, status, folder button), driven
  through a mocked `__TAURI_INTERNALS__.invoke` -- the extension itself is
  never loaded.
- **Real-browser runtime** (`npm run test:extension:browser`,
  `tests-extension/runtime.spec.ts`): loads the actual, unpacked extension
  into a real Chromium via Playwright's `launchPersistentContext` with
  `--load-extension`, against small local Meet fixture pages (`browser-extension/test/fixtures/*.html`,
  served from a throwaway local HTTP server -- never the real
  meet.google.com). This is the only layer that exercises
  `content-runtime.js`'s DOM polling/heartbeat timers and `background.js`'s
  message relay in a real page/service-worker environment, and it caught a
  real bug: content scripts silently loaded as classic (non-module) scripts
  despite `"type": "module"` in `content_scripts` (not a real manifest key),
  so `content-meet.js`/`content-teams.js`/`content-zoom.js`'s static `import`
  threw at runtime in an actual browser. The fix (dynamic `import()` via
  `chrome.runtime.getURL`, plus the now-required `web_accessible_resources`
  entry) is what's shipped in `manifest.json`/`manifest.firefox.json` today.

  This test never touches the real native-messaging host or a real browser
  profile: `scripts/build-extension-test.mjs` copies `browser-extension/`
  into a gitignored `test-results/extension-under-test/` and, only in that
  copy, (1) adds one match pattern scoped to `http://127.0.0.1:<port>/` --
  the port the test's own fixture server is listening on -- to
  `host_permissions`/`content_scripts`/`web_accessible_resources`, (2)
  prepends a test-only shim that stubs `chrome.runtime.connectNative` and
  captures every message the extension would have sent into
  `chrome.storage.session` (read back by the test via the extension's
  service worker), and (3) shortens `content-runtime.js`'s poll/heartbeat
  intervals so the test doesn't wait out the real 15s cadence. The shipped
  manifests and `background.js`/`content-runtime.js` are never modified.

  Requires Chromium (`npx playwright install chromium`; the rest of this
  repo's Playwright tests use the `msedge`/system-`chromium` channel, which
  doesn't support `--load-extension`). The test skips itself with a clear
  message, rather than failing, if Chromium isn't installed or a persistent
  context with an extension can't be launched. It isn't part of `npm test`
  for the same reason: it needs its own browser binary and is slower, so
  it's run separately (and, in CI, only where that binary is available).

## Limitations

- **Selector fragility**: the in-call/lobby/ended heuristics
  (`browser-extension/content-logic.js`) key off `aria-label` text (locale-
  dependent; English patterns are what's implemented) and a small number of
  DOM signals, not a private/undocumented API. A Meet/Teams/Zoom UI change
  can break detection until the patterns are updated; this mirrors the
  native Windows heuristic's own documented fragility.
- **Unsigned Firefox add-on**: this repository ships the extension
  unsigned. Firefox only allows an unsigned add-on to run as a *temporary*
  load (`about:debugging`), which does not persist across a Firefox restart.
  Persistent installation on release Firefox requires submitting the
  extension to Mozilla for signing (or running Firefox Developer Edition/
  Nightly with signature enforcement disabled) -- not done here.
- **Pinned Chromium ID, but unsigned**: the `key` in `manifest.json` fixes
  the extension's ID across reloads, but Chrome still shows an "unpacked
  extensions" warning banner and disables it after each browser restart
  unless developer mode stays on; a Chrome Web Store listing (out of scope
  here) would remove that friction.
- **Best-effort heartbeat/staleness**: a browser or OS suspend that pauses
  timers can delay the 15s heartbeat; the 45s staleness window absorbs
  ordinary jitter but not an extended suspend, in which case detection may
  briefly show the meeting as ended before the next real observation.
- Only the browsers/platforms above are covered; other browsers (Safari,
  Brave's extension store variant, etc.) are not addressed by this manifest
  set, though Brave/Vivaldi/other Chromium browsers can generally load the
  same Chrome manifest as an unpacked extension.
