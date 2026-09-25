# AppFlowy Cloud export

Optional, off-by-default export of a meeting's transcript and summary to an
AppFlowy Cloud workspace. Audio never leaves this device. This document
covers setup, exactly what is sent, the create-only guarantee and how it is
enforced, the page structure, idempotency, and known limitations.

## Setup

1. Settings → AppFlowy export → check **Export meetings to AppFlowy**.
2. Enter your email and click **Send sign-in code**; AppFlowy Cloud emails a
   one-time 6-digit code (magic-link/OTP -- this app never asks for or
   transmits your AppFlowy account password).
3. Enter the code and click **Verify code**. The access/refresh tokens are
   stored in the OS credential store (Windows Credential Manager, macOS
   Keychain, or Linux Secret Service via the `keyring` crate) -- never in
   SQLite, the settings file, or logs.
4. Pick a workspace from the dropdown.
5. Optionally check **Export automatically** (off by default -- see
   [Privacy defaults](#privacy-defaults)). Otherwise use the **Export to
   AppFlowy** button that appears on each eligible meeting.
6. Optionally point **AppFlowy server** at a self-hosted deployment instead
   of the default `https://beta.appflowy.cloud` (https required, except a
   loopback address, which only this codebase's own tests use).

## What is sent

Only the meeting's **transcript text and summary text** -- never audio,
never a link back to the local audio file. Specifically, the meeting page's
content is built entirely from:

- The recording's title and local start time.
- `summary.json` (if the local-summary feature produced one): summary, key
  points, decisions, action items with owners, open questions.
- `transcript.json` plus any completed speaker-identification labels:
  speaker turns labeled `You` / `Speaker N` / `Remote` with timestamps (see
  `transcription::presentation_label`).

The destination (workspace › "My daily meetings" › date) is shown in
Settings before the first export.

## Privacy defaults

- `appflowy_enabled` defaults to `false`: nothing about a recording leaves
  this device until the feature is explicitly turned on.
- `appflowy_auto_export` defaults to `false` even after the feature is
  enabled -- a second, separate opt-in. Manual per-meeting export works
  regardless of this setting.
- The export trigger waits for the transcript to be complete, and (if
  enabled) for local summarizing and speaker identification to finish first,
  so labels and the summary are final before anything is sent.

## Create-only guarantee, and how it is enforced

**Hard rule: this integration creates pages in AppFlowy; it never updates,
renames, moves, or deletes anything.** This is enforced structurally, not by
convention, in `src-tauri/src/appflowy_client.rs`:

1. **One client, six methods.** `AppFlowyClient` is the only type in this
   codebase allowed to open a socket to AppFlowy. It exposes exactly:
   `request_otp`, `verify_otp` (and an internal token refresh), `sign_out`,
   `list_workspaces`, `get_folder`, and `create_page`. There is no
   `update_page`, `delete_page`, `move_page`, `rename_page`, `publish`, or
   `append_block` method anywhere -- the forbidden actions have no code path
   that could call them.
2. **A hard-coded allowlist checked before any socket opens.** Every
   request, regardless of caller, passes through `allowlist::check(method,
   path)` first. The allowlist is a fixed table, not derived from any
   argument a caller could influence:
   - `POST /gotrue/otp`
   - `POST /gotrue/verify`
   - `POST /gotrue/token` (refresh grant only -- no code path ever sends a
     password grant)
   - `GET /api/workspace`
   - `GET /api/workspace/{workspace_id}/folder`
   - `GET /api/workspace/{workspace_id}/page-view/{view_id}`
   - `POST /api/workspace/{workspace_id}/page-view`

   Anything else -- `update-name`, `update-icon`, `append-block`, `publish`,
   `unpublish`, `move`, `trash`, the combined `PATCH` page-view route, GoTrue
   `admin/*`, `signup`, `logout`, `authorize` -- is refused with an error
   before a `TcpStream`/TLS handshake is ever attempted.
   `appflowy_allowlist_tests.rs` proves every one of these forbidden
   (method, route) combinations is refused, asserting purely against the
   allowlist function itself so the test suite never opens a socket to prove
   it.
3. **Content is set at creation, never appended.** `create_page` takes the
   page's full `page_data` document as an argument to the same call that
   creates the page. There is no follow-up call that appends blocks to an
   existing page -- appending to a page the integration did not just create
   in this run would count as modifying it.
4. **Read-before-write idempotency, not update-on-conflict.** Before
   creating any page, the local SQLite journal (`appflowy_store.rs`) is
   checked; if it already has a `created` record, that page id is reused
   outright. Otherwise the parent's folder tree is re-read via `get_folder`
   and matched **by exact name** -- this is how an existing "My daily
   meetings" page the user created themselves is found and reused, and how a
   crash between a successful remote create and the local journal update is
   recovered (the retry adopts the already-created page instead of creating
   a duplicate). If a page already exists, it is left completely alone, even
   if its content looks incomplete -- this integration never "fixes" a page
   it did not just create.

## Page structure

```
Workspace
└── My daily meetings                (created once, top level of the workspace/space)
    └── 2026-09-25                   (created once per date)
        └── 14:00 — Weekly sync      (one per meeting, local start time)
```

Each meeting page's content is one of two formats, controlled by the **Use
the native AI Meeting Notes block** setting (default: on):

- **Native** (`appflowy_use_native_block: true`): the `ai_meeting` block
  documented by AppFlowy Web's editor (`ai_meeting` → `ai_meeting_summary` /
  `ai_meeting_notes` / `ai_meeting_transcription` → `ai_meeting_speaker`
  turns with `speaker_id`/`timestamp`/`end_timestamp`, each holding
  paragraph blocks with Quill-delta text). This renders with the tabbed
  Summary/Transcript UI in AppFlowy Web and any client that has shipped this
  feature.
- **Fallback** (`appflowy_use_native_block: false`): a heading with the
  meeting title, a metadata paragraph, and two `toggle_list` blocks
  ("Summary", "Transcript") built entirely from long-standing standard block
  types (`heading`, `paragraph`, `bulleted_list`, `quote`, `toggle_list`).
  Transcript turns render as one `quote` block per turn. Use this if a
  client renders the native block blank -- as of this integration's
  research pass, the `ai_meeting*` block types exist in AppFlowy Web but not
  yet in the open-source Flutter desktop client.

Both formats are built by pure functions in `appflowy_blocks.rs`, unit
tested against the documented schema, including French/UTF-8 content and
Quill-delta escaping of special characters.

## Idempotency

See [Create-only guarantee](#create-only-guarantee-and-how-it-is-enforced)
above for the mechanism. In summary: a meeting auto-exports (or is manually
exported) at most once, tracked by a durable job row per recording in
`appflowy_store.rs`. A manual **Export again** action creates a **new**
meeting page with a numeric suffix (`HH:mm — Title (2)`, `(3)`, ...) only
after the user confirms in the dashboard -- the prior page is never touched.

## Limitations

- The native `ai_meeting` block currently renders correctly in AppFlowy Web
  (and any client that has shipped this feature); it may render as a blank
  or "unsupported block" placeholder elsewhere. Use the standard-blocks
  fallback setting if that happens.
- A single meeting's exported page content is capped at
  `appflowy_blocks::MAX_TRANSCRIPT_CHARS` (200,000 characters, roughly
  3-4 hours of conversation) because the create-only rule forbids splitting
  a meeting across multiple pages or appending more content later. A
  transcript beyond that limit is truncated in the *exported* page with a
  visible note; the local `transcript.json`/`summary.md` remain the complete
  source of truth regardless.
- No dedicated "find page by name" API exists on AppFlowy Cloud; existing-page
  lookups always read the parent's folder tree and match by exact name (see
  the research notes in `docs/architecture.md`'s AppFlowy section).
- The `Client-Version` header (`appflowy_client::CLIENT_VERSION`) is pinned
  to `0.9.64`, the newest AppFlowy Cloud release tag observed during this
  integration's research pass; a future server may require a newer value.

## Testing

All AppFlowy tests run against an in-process fake HTTP server
(`appflowy_fake_server.rs`, bound to `127.0.0.1:0`) or purely in-memory --
**no test in this codebase ever makes a real request to `beta.appflowy.cloud`
or any other real AppFlowy server.** Coverage includes: the allowlist
refusal tests described above; first-export page creation (root/date/meeting,
schema-checked `page_data`) in both block formats; a second same-day meeting
reusing the existing date page; an existing "My daily meetings" page being
found and reused; crash-then-retry recovery; 401-triggered token refresh;
the OTP sign-in flow; permanent (4xx) vs. transient (5xx/network) failure
handling; disabled/auto-export gating; "Export again" confirmation and
suffixing; and French-language UTF-8 content. `tests/appflowy.spec.ts`
covers the Settings sign-in flow and the per-meeting export button with
mocked IPC.
