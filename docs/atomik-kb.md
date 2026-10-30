# Atomik KB export

Optional, off-by-default export of a meeting's transcript and summary to
atomik-kb, a local-first knowledge base running on the
same computer. A sibling of the [AppFlowy export](appflowy.md) with the same
shape and the same guarantees. Audio never leaves this device, and neither
does anything else: atomik-kb is reached over loopback only. This document
covers setup, exactly what is sent, the create-only guarantee and how it is
enforced, the page structure, idempotency, and known limitations.

## Setup

1. In **atomik-kb**: Settings → MCP → turn the MCP server on, then **add a
   client** (any name, e.g. "Meeting Recorder"). Copy the token **once** -- it
   is shown only when the client is created (`kbm_` followed by 43
   characters).
2. Still in atomik-kb's MCP settings, **grant that client write access** to
   the space the meetings should go to. The default is a space named
   **My Daily Meeting**; create it in atomik-kb first (this app never creates
   a space).
3. In this app: Settings → Atomik KB export → check **Export meetings to
   Atomik KB**.
4. Set the **port** to the one shown in atomik-kb's MCP settings (default
   `47811`) and the **space name** (default `My Daily Meeting`, matched
   case-insensitively and exactly).
5. Paste the token and click **Save token**. It goes straight into the OS
   credential store (Windows Credential Manager, macOS Keychain, or Linux
   Secret Service via the `keyring` crate) -- never into SQLite, the settings
   file, or logs -- and the field is cleared. **Clear token** removes it.
6. Click **Test Atomik KB connection**. It runs `initialize` + `list_spaces` only (it
   never writes) and reports either "space found and writable" or the precise
   problem: token refused, atomik-kb not reachable, space not shared with the
   client, or space read-only. It tests the values currently in the form, so
   you can try a port or space name before saving.
7. Optionally check **Export to Atomik KB automatically** (off by default --
   see [Privacy defaults](#privacy-defaults)). Otherwise use the **Export to
   Atomik KB** button that appears on each eligible meeting.

The atomik-kb app must be **open and unlocked** for an export to run (its MCP
server stops when it is locked or closed). If it is not, the export waits and
retries with backoff -- see [Failure handling](#failure-handling).

## What is sent

Only the meeting's **transcript text and summary text** -- never audio, never
a link back to the local audio file. The page content is built entirely from
the same data the AppFlowy export uses (`appflowy::load_meeting_data`):

- The recording's title and local start time.
- `summary.json` (if the local-summary feature produced one): summary, key
  points, decisions, action items with owners (as `- [ ]` tasks), open
  questions.
- `transcript.json` plus any completed speaker-identification labels and
  names: one blockquote per turn, `> **00:12:05 · Speaker 1:** text`, labeled
  `You` / `Speaker N` / a name you gave / `Remote` (see
  `transcription::presentation_label`).

It is sent as **Markdown** built by pure functions in `atomik_kb_markdown.rs`.
Everything that came from a person or a model is Markdown-escaped, so a
transcript line can never become a heading, a list, a code fence, a link or
math in atomik-kb. Newlines inside a turn collapse to spaces. The whole
document is capped at 200,000 characters **and** 250,000 UTF-8 bytes
(atomik-kb refuses more than 256 KiB; French accents are 2 bytes, CJK 3, emoji
4, so both are counted); whole turns are kept from the start and a visible
note marks the cut. The local `transcript.json` / `summary.md` stay complete.

## Privacy defaults

- `atomik_kb_enabled` defaults to `false`: nothing about a recording leaves
  the app until the feature is explicitly turned on.
- `atomik_kb_auto_export` defaults to `false` even after the feature is
  enabled -- a second, separate opt-in. Manual per-meeting export works
  regardless of this setting.
- The export trigger waits for the transcript to be complete, and (if
  enabled) for local summarizing and speaker identification to finish first,
  so labels and the summary are final before anything is sent.
- **Loopback only.** The host is a fixed `127.0.0.1` -- there is no host
  setting, `atomik_kb_client::validate_host` accepts nothing but
  `127.0.0.1`/`localhost`, and atomik-kb itself binds loopback only and
  refuses any other `Host`. Plain http is therefore safe: the bearer token
  never leaves the machine.
- Setting `MEETING_RECORDER_DISABLE_ATOMIK_KB=1` before launch stops the
  export worker for that process (like `MEETING_RECORDER_DISABLE_APPFLOWY=1`),
  without changing saved preferences.

## Create-only guarantee, and how it is enforced

**Hard rule: this integration creates pages in atomik-kb; it never appends
to, updates, renames, moves, or deletes anything.** atomik-kb's MCP server
does offer `append_to_page`; this app never calls it. Enforced structurally in
`src-tauri/src/atomik_kb_client.rs`:

1. **One client, four operations.** `AtomikKbClient` is the only type in this
   codebase allowed to open a socket to atomik-kb. It exposes exactly
   `initialize` (plus the `notifications/initialized` the protocol requires),
   `list_spaces`, `list_pages` and `create_page`. There is no `append_to_page`,
   `read_page`, `search`, update, delete, move or rename method anywhere.
2. **A hard-coded allowlist checked before any socket opens.** Every
   JSON-RPC message passes `allowlist::check(method, tool)` first. The table is
   fixed, not derived from anything a caller could influence: the methods
   `initialize`, `notifications/initialized` and `tools/call`, and -- for
   `tools/call` -- only the tools `list_spaces`, `list_pages`, `create_page`.
   Everything else (including `append_to_page`, `read_page`, `search`,
   `tools/list`, `resources/*`, `prompts/*`) is refused with an error before a
   connection is attempted. `atomik_kb_allowlist_tests.rs` proves it against the
   allowlist function itself, checks the table still has exactly 3 methods and
   3 tools, and proves through the client that a refused call never reaches the
   in-process fake server.
3. **Content is set at creation, never appended.** `create_page` takes the
   page's full Markdown in the same call that creates it. There is no
   follow-up call that adds to a page.
4. **Read-before-write idempotency, not update-on-conflict.** See
   [Idempotency](#idempotency).

The token only ever goes into an `Authorization: Bearer` header on those
requests; it is never logged and never echoed in an error message.

## Page structure

```
Space ("My Daily Meeting", chosen by name)
└── My daily meetings              (created once, top level of the space)
    └── 2026-09-25                 (created once per date)
        └── 14:00 — Weekly sync    (one per meeting, local start time)
```

Titles are normalised to atomik-kb's own single-line form and capped at 200
characters (a long meeting title is cut with an ellipsis, never the ` (2)`
suffix).

### Which space

The configured space name is resolved on every export through `list_spaces`
by a case-insensitive **exact** match. If several spaces share the name, a
writable one is preferred. If no space matches, or the match is read-only for
this client, the job **fails with a clear message** (naming the spaces the
client can see, and where to grant write access). It never creates a space and
never falls back to another one.

### Verification after create

After creating each page (root, date, or meeting -- never when *reusing* an
existing one), the parent's children are listed again to confirm the new page
is really there. If it is not, the job is marked **failed** instead of
"completed". atomik-kb lists at most 200 children and ends a longer list with
"... and N more"; when the parent is that large the new page may fall past the
cut, and is then confirmed by listing the new page's own children instead
(which succeeds only if the page exists in that space).

## Idempotency

A meeting auto-exports (or is manually exported) at most once, tracked by a
durable job row per recording in `atomik_kb_store.rs` (`atomik_kb.sqlite3`).
Before creating any page:

1. The local journal (`atomik_kb_pages`) is checked; a row already `created`
   is reused outright.
2. Otherwise the parent's children are re-read via `list_pages` and an
   **exact-title** child is adopted (titles compared in atomik-kb's single-line
   form; never fuzzy). This is how an existing "My daily meetings" page you
   made yourself is found and reused, and how a crash between a successful
   create and the journal update is recovered. A page another journal row
   already owns is never adopted a second time, so two meetings with the same
   title in the same minute get two pages.
3. Only then is a page created.

A page that already exists is left completely alone, even if its content looks
incomplete -- this integration never "fixes" a page it did not just create.
A manual **Export again** (confirmed in the dashboard) creates a **new**
meeting page with a numeric suffix (`HH:mm — Title (2)`, `(3)`, ...); the
earlier page is never touched.

## Failure handling

| Situation | Behaviour |
| --- | --- |
| Connection refused / timeout / HTTP 5xx (atomik-kb closed, locked, MCP off) | **Transient**: the job stays queued and retries with backoff (5 s doubling to 5 min). The meeting row shows the message. |
| HTTP 401, or no token saved | **Permanent** until you fix it: the job fails with "refused the token" / "No atomik-kb token is saved"; save a new token and use **Retry**. |
| Space not found or read-only | **Permanent**: clear message, nothing created. |
| Tool-level `isError` (e.g. no write grant) | **Permanent**: atomik-kb's own sentence plus how to grant write access. |
| A created page not listed afterwards | **Permanent**: marked failed, not completed. |

## Limitations

- atomik-kb has no web URL for a page, so there is no "Open" button; the
  Settings **Exported pages** list (read straight from the local journal) shows
  what was created, read-only.
- A journal entry is trusted without re-checking it. If you delete or move a
  page this app created, later exports still use its remembered id and fail
  (permanently, with atomik-kb's message) when it is gone; this app never
  recreates or repairs anything.
- Crash-recovery adoption looks at the first 200 children of a parent. If a
  parent has more (e.g. more than 200 date pages under "My daily meetings") and
  the local journal was lost at the same time as a crash, a duplicate-titled
  page can be created instead of the existing one being adopted.
- A single meeting's page is capped (see [What is sent](#what-is-sent))
  because the create-only rule forbids splitting a meeting across pages or
  appending later.
- atomik-kb converts the Markdown with its own importer: links and images stay
  text, and `- [ ]` task items become whatever that importer produces.
- Only the Windows credential store was exercised during development; macOS
  Keychain and Linux Secret Service use the same `keyring` code path as the
  AppFlowy integration.

## Testing

All atomik-kb tests run against an in-process fake MCP server
(`atomik_kb_fake_server.rs`, bound to `127.0.0.1:0`, implementing
`initialize` / `list_spaces` / `list_pages` / `create_page` with bearer-token
auth, per-space write grants, `isError` results, single-line titles and the
real server's text formats) or purely in memory -- **no test ever makes a real
request to a real atomik-kb, and no test contains a real token** (tests use
the obviously fake `kbm_TEST_FAKE_TOKEN_...`). Coverage: the allowlist refusal
tests; first-export root/date/meeting creation; a second same-day meeting
reusing the date page; an existing root being adopted; crash-then-retry
adoption; no duplicates on a re-run or for same-title meetings; space not
found / partial name / read-only / revoked-after-listing; 401 (permanent) vs
refused connection and 5xx (transient); post-create verification, including a
capped listing; Export-again suffixing; auto-export gating on summary and
speaker identification; the `list_spaces` / `list_pages` / `create_page` text
parsers (parenthesised names, noise, truncation, look-alike markers);
loopback-only validation; the Markdown builder (French/UTF-8, escaping and
block-injection resistance, character and byte caps); and the credential
store round trip. `tests/atomikKb.spec.ts` covers the Settings section
(password field, token save/clear, test connection with unsaved values,
polling not clobbering edits), the per-meeting buttons and status, and the
exported-pages list, all with mocked IPC.
