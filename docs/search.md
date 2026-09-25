# Search across all transcripts

A single search box at the top of the recording library searches every meeting's transcript and summary at once -- not just the 25 most recent recordings that the library's own local `#history-search` filter covers. It is backed by a SQLite [FTS5](https://www.sqlite.org/fts5.html) full-text index, entirely local: no transcript or summary text ever leaves the device for this feature.

## Index

`search.sqlite3` sits next to `recordings.sqlite3`, `transcription.sqlite3`, `diarization.sqlite3`, and `summary.sqlite3`. It is a **derived, rebuildable cache** -- those other databases and the transcript/summary files on disk remain the source of truth. If the file is missing, corrupt, or from an old/incompatible schema, it is dropped and rebuilt from scratch (`SearchStore::open_or_rebuild`) rather than blocking recording, transcription, diarization, or summarizing.

One row is indexed per:
- transcript segment (recording ID, segment index, start/end ms, speaker label, text),
- summary field (the narrative summary, each key point, each decision, each action item with its owner/due folded into the text), and
- the meeting's own title.

The FTS5 tokenizer is `unicode61 remove_diacritics 2`, so an accented query like "décision" matches "decision" and vice versa -- useful for French meetings searched with an unaccented keyboard, or the reverse. `prefix='2 3'` keeps 2-3 character prefix queries (as-you-type) fast without a full index scan.

## Keeping the index caught up

There is no event bus in this codebase for "a transcript just completed," so the index is kept fresh by:
- a **startup reconcile** pass, run once in a background thread so it never blocks the UI: for every finalized recording, it compares the recording's current transcript/summary checkpoint checksums against what the index last saw, and only re-indexes recordings whose checksum changed (including "never indexed"). A recording no longer reachable through `RecordingStore` (deleted) has its rows dropped.
- a light **periodic recheck** (every few seconds) of the same comparison, so a transcript or summary that finishes while the app is running gets indexed shortly afterward without needing a dedicated hook in `transcription.rs`/`diarization.rs`/`summary.rs`.
- **immediate** `reindex`/`remove` calls from `rename_recording` and `delete_recording` in `lib.rs`, so a rename's new title and a delete both show up (or disappear) in search results right away rather than waiting for the next recheck.

Every step here is best-effort: a recording with a missing, incomplete, or checksum-mismatched transcript/summary simply contributes fewer rows (at minimum its title) rather than erroring.

## Query handling

User input is never interpolated into FTS5's `MATCH` syntax directly. `search::build_match_query` treats every token -- a bare word or a `"quoted phrase"` -- as a literal FTS5 string (embedded `"` doubled per FTS5's own escaping rule), so characters a user might type that mean something to FTS5 (`OR`, `-`, `*`, parentheses, an unbalanced quote) are always literal text, never query syntax. The last token becomes a prefix match (`"word"*`) for as-you-type search; tokens are ANDed together.

Ranking uses SQLite's built-in `bm25()`. Because a common word can match tens of thousands of rows in a large library, the query runs in two phases: phase 1 ranks candidate rows by `bm25()` alone (cheap); only the rows that will actually be shown (a handful per matched meeting) get a `snippet()` built for them in phase 2, since `snippet()` is comparatively expensive. Results are grouped by meeting, each meeting ranked by its own best-scoring hit, each meeting keeping its own top few hits.

Snippets come back from `snippet()` with `\u{1}`/`\u{2}` marking matched spans; `search::render_snippet` HTML-escapes the plain text and wraps marked spans in `<mark>`, producing a string that is safe to insert as `innerHTML` directly -- the indexed text can never inject markup.

## Command and UI

`search_transcripts(query, limit)` returns meetings ranked by relevance, each with up to a few of its own best-ranked hits (kind, timestamp when applicable, speaker, and a pre-rendered HTML snippet). The library's search box (`#transcript-search-input`) debounces input by 150 ms, supports `Enter` to search immediately and `Escape` to clear, and shows empty/no-results states. Clicking a hit calls the existing `open_transcript` command for that meeting (opening at/near the specific timestamp is out of scope for now).

## Performance

A test builds a synthetic index of 2,000 meetings x 200 segments (400,000 rows) and measures query latency directly against `SearchStore` (see `search_tests.rs::query_latency_stays_small_at_synthetic_scale`). Measured on development hardware: a specific term that narrows to one meeting (~200 matching rows, the common case for a real search) took about **1.5 ms**; a deliberately worst-case common word matching roughly 1/15 of all rows (~27,000 matches) took about **110 ms**. The test asserts generous bounds (500 ms / 100 ms) to avoid flakiness on slower or shared CI hardware while still catching an accidental full scan or an `O(n^2)` regression, and prints the measured numbers.

Index size, measured directly (`search_tests.rs::probe_index_size_per_hour_of_meeting`, `cargo test --lib probe_index_size -- --ignored --nocapture`): a synthetic ~1-hour meeting (500 transcript segments plus a summary, 5 key points, 5 decisions, 5 action items -- 516 rows) added roughly **275 KB** to `search.sqlite3` at steady state (measured as the marginal cost of the 2nd through 20th such meeting, which amortizes away a single file's fixed SQLite/FTS5 overhead). That is well under 1% of that meeting's own audio storage (see [audio storage](transcription.md)) -- around 275 MB of `search.sqlite3` per 1,000 hours of indexed meetings.

## A note on delete performance

`search_fts`'s `recording_id` column is `UNINDEXED` (FTS5 does not build a secondary index over such columns), so deleting one recording's rows by `WHERE recording_id=?` would be a full table scan -- fine once, but `O(n)` per deleted/reindexed recording adds up to `O(n^2)` across a whole-library rebuild. A small side table, `row_recording(fts_rowid, recording_id)` with an ordinary B-tree index on `recording_id`, gives that lookup a real index: deletes go `row_recording` (indexed lookup) -> `search_fts` (rowid delete, which is fast against FTS5's own content table).
