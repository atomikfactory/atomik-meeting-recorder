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
5. Optionally pick a **Destination** -- a Space, or a page inside one -- from
   the dropdown below it; the chosen path is shown as
   "Workspace › Space › My daily meetings". Leaving it as **Automatic
   (General space)** (the default) makes "My daily meetings" land in the
   workspace's "General" Space if one exists, or its first Space otherwise
   (see [Where pages are created](#where-pages-are-created) below).
6. Optionally check **Export automatically** (off by default -- see
   [Privacy defaults](#privacy-defaults)). Otherwise use the **Export to
   AppFlowy** button that appears on each eligible meeting.
7. Optionally point **AppFlowy server** at a self-hosted deployment instead
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

## The "Record not found" bug and how it was fixed

A confirmed bug on a real account: pages this integration created appeared
correctly in the AppFlowy sidebar, but opening one failed with
`[Document lifecycle]: failed to init document: Record not found: Collab not
found for object_id: <the page's own view_id>`.

**Root cause.** AppFlowy-Cloud's `create_document_page`
(`src/biz/workspace/page_view.rs`) stores the page's *document collab* under
`collab_id.unwrap_or(Uuid::new_v4())` and the *page itself* under
`view_id.unwrap_or(collab_id)` -- two independent ids. This integration's
`CreatePageParams` sent `view_id` but never `collab_id`, so the server always
generated a random collab id that never matched the `view_id` the page was
listed and later opened under -- the document was, in effect, unreachable
from the moment it was created, even though page creation itself reported
success and the page showed up in the folder tree.

**The fix.** Every `create_page` call now sends `collab_id` equal to the same
`view_id` it sends, so the document is always stored under the exact id used
to open it. `appflowy_fake_server.rs` models the real server's semantics
(collab stored under `collab_id`, page under `view_id`, independently) so
this codebase's own tests reproduce the bug for the old request shape and
prove the fix for the new one. After creating any page (root, date, or
meeting), and before adopting any existing page found by name or by the local
journal, `AppFlowyClient::page_content_loads` calls the existing read-only
`GET .../page-view/{view_id}` route to confirm the page's document actually
opens -- not just that the page is listed. A page whose content 404s
("Collab not found") is never trusted: after creation it fails the export
job with a clear message instead of reporting success; when found by name or
journal, it is skipped in favor of creating a fresh page (which can mean a
duplicate-named page appears alongside the broken one -- see
[Limitations](#limitations)).

**Legacy pages are never reused.** Every local journal row
(`appflowy_pages` in `appflowy_store.rs`) is stamped with a `generation`
number; a row from before this fix defaults to generation 1 via the
database migration, while everything this version plans or creates is
generation 2 (`AppFlowyStore::CURRENT_GENERATION`). A legacy row is never
handed back as "already created" (a fresh row is planned instead, under a
distinct local key), and a legacy `view_id` is excluded from name-based
adoption even if a page with the expected name exists in AppFlowy -- so a
meeting export can never silently attach itself to a page from before the
fix. Settings' **Previously created pages** list marks each legacy row
"created by an earlier version -- content may not open; you can delete it in
AppFlowy" (see [Open in AppFlowy / previously created
pages](#open-in-appflowy--previously-created-pages)); this app itself never
deletes anything, so a broken page from an older version stays in AppFlowy
until the user removes it there. A meeting export job that already
`completed` against a page later marked legacy is **not** automatically
re-exported -- only the existing, confirmation-gated **Export again** action
creates a fresh page.

## Page structure

```
Workspace
└── General (or another Space)       (a Space -- see below)
    └── My daily meetings             (created once, top level of the Space)
        └── 2026-09-25                (created once per date)
            └── 14:00 — Weekly sync   (one per meeting, local start time)
```

### Where pages are created

AppFlowy's own sidebar only ever shows **Spaces** (`FolderView.is_space`) and
their descendants -- a page created directly under the workspace root itself
is never shown there, even though it really was created and the API reports
success. An earlier version of this integration did exactly that (it used
`settings.parent_view_id.unwrap_or(workspace_id)`, so with no destination
chosen "My daily meetings" landed directly under the workspace root), which
is why an export could report success while nothing appeared to the user.

The destination is resolved once per export, in this order:

1. **An explicit Destination** chosen in Settings (`appflowy_parent_view_id`)
   is always honoured as-is -- it can be a Space or a page inside one.
2. Otherwise, the workspace's own folder tree is read (read-only) and a
   Space is picked: one named "General" (case-insensitive) if any exists,
   else the first Space in the workspace's own order.
3. If the workspace has **no Space at all**, the export fails with a clear
   message ("No space found in this AppFlowy workspace; choose a parent in
   Settings") rather than falling back to the invisible workspace root.

After creating each page (root, date, or meeting -- never when *reusing* an
existing one, which is already known-visible), the parent's folder tree is
read back once more to confirm the new page is actually listed as a child.
If it isn't, the export is marked **failed** with a clear message instead of
"completed" -- this app never reports success for a page the user cannot
find. Both checks are read-only (`get_folder`); neither adds a new route to
the create-only allowlist below.

### Open in AppFlowy / previously created pages

A completed export shows an **Open in AppFlowy** button that opens the
created meeting page in your system browser (see
[URL format](#appflowy-web-url-format) below). Settings also lists
**Previously created pages** -- every page this integration has ever
planned or created, read straight from the local journal, each with an
**Open** link. This is read-only and lets you find (and, if you want,
manually move in AppFlowy) a page left behind by an older version of this
app, including one orphaned directly under the workspace root by the bug
described above -- this app itself never moves, renames, or deletes
anything listed here.

### AppFlowy Web URL format

"Open in AppFlowy" and the previously-created-pages list both link to
`appflowy_client::web_page_url(base_url, workspace_id, view_id)`. This is an
**assumption**, not independently confirmed against AppFlowy's source: for
the official cloud, the web app is understood to live at a different
domain (`https://appflowy.com/app/<workspace_id>/<view_id>`) than the
`beta.appflowy.cloud` API/auth host this integration talks to; for a
self-hosted deployment, the web app is assumed to be served from the same
configured base URL under `/app/...` (the typical single-domain,
nginx-reverse-proxied self-host layout), since a self-host has no separate
published web domain to reference instead. If this assumption is wrong for
a given deployment, only that one function needs to change. Opening a URL is
restricted, defense-in-depth, to `https://` and to either the AppFlowy Web
public domain or the host of the configured AppFlowy server -- never an
arbitrary caller-supplied URL.

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

- Pages created by a version of this app before the Space-resolution fix
  described in [Where pages are created](#where-pages-are-created) may be
  hidden under the workspace root, invisible in AppFlowy's own sidebar. Use
  Settings' **Previously created pages** list to open one directly and, if
  you want, move it into a Space yourself -- this app never moves anything
  on its own.
- Pages created by a version of this app before the `collab_id` fix
  described in [The "Record not found" bug](#the-record-not-found-bug-and-how-it-was-fixed)
  may fail to open at all ("Record not found: Collab not found"). Because
  this app never deletes or updates anything it created, these pages are
  left exactly where they are; Settings' **Previously created pages** list
  marks each one so you can find and, if you want, delete it yourself in
  AppFlowy. Since a legacy page is never reused as a parent, a later export
  of the same meeting (or a meeting on the same date) creates a fresh,
  working root/date/meeting page rather than adding more content under the
  broken one -- you may see a duplicate-named "My daily meetings" or date
  page in AppFlowy as a result; the working one is whatever this app's
  Settings/dashboard links to going forward.
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
refusal tests described above, plus an assertion that the allowlist table
still holds exactly the seven documented routes; first-export page creation
(root/date/meeting, schema-checked `page_data`) in both block formats; a
second same-day meeting reusing the existing date page; an existing "My
daily meetings" page being found and reused; crash-then-retry recovery; a
stale journal root keyed under the workspace root (the pre-fix orphan shape)
never being reused, with a fresh root created under the Space instead;
Space resolution (a Space named "General" preferred, the first Space by
order otherwise, an explicit `parent_view_id` always honoured, and a
workspace with no Space failing the job with a clear message); post-create
verification failing the job when the fake server accepts a create but
doesn't list the new page under its parent; post-create *content* verification
failing the job when a created page's document collab doesn't load; a
page-view create request that omits `collab_id` reproducing the "Record not
found: Collab not found" bug against the fake server, and the fixed request
(`collab_id == view_id`) resolving it; a legacy (pre-fix generation) root row
never being adopted, with a fresh, working root planned and created instead;
a name-matched page whose content fails to load being skipped in favor of a
fresh page; 401-triggered token refresh; the OTP sign-in flow; permanent
(4xx) vs. transient (5xx/network) failure handling; disabled/auto-export
gating; "Export again" confirmation and suffixing; and French-language UTF-8
content. `tests/appflowy.spec.ts` covers the Settings sign-in flow, the
per-meeting export button, the Destination picker (including surviving
background polling), the "Open in AppFlowy" button, and the
previously-created-pages list (escaped names, its Open links, and the
legacy-page label), all with mocked IPC.
