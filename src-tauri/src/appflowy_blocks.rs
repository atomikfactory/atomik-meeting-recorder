//! Builds the `page_data` JSON tree sent at page-creation time -- either the
//! native AppFlowy Web "AI Meeting Notes" block (`ai_meeting`/`ai_meeting_summary`/
//! `ai_meeting_notes`/`ai_meeting_transcription`/`ai_meeting_speaker`, per the
//! research pass's read of `AppFlowy-Web/src/application/types.ts` and
//! `AIMeetingBlock.tsx`) or a "standard blocks" fallback (heading + two
//! toggle lists + one quote per speaker turn) for clients that do not yet
//! render the native block. Both trees are pure functions of
//! `MeetingExportData` -- no I/O, no network -- so they are exhaustively unit
//! tested without a fake server.
use crate::summary_types::SummaryOutput;
use serde_json::{json, Value};

/// One turn of the transcript, already resolved to its presentation label
/// (`You` / `Speaker N` / `Remote`, see `transcription::presentation_label`)
/// by the caller -- this module has no diarization knowledge of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerTurn {
    pub speaker_label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct MeetingExportData {
    pub title: String,
    /// RFC 3339 UTC timestamp of the meeting's start, used only in block
    /// `data` fields (not for the page name, which the caller derives
    /// separately using local time -- see `appflowy.rs`).
    pub started_at_rfc3339: String,
    pub summary: Option<SummaryOutput>,
    pub turns: Vec<SpeakerTurn>,
}

/// A conservative cap on the transcript's rendered character count sent in
/// one `page_data` document. AppFlowy Cloud enforces no documented request
/// size limit (see the research pass), but since this integration can never
/// split a meeting's content across more than one page or append to a page
/// after creation (the hard create-only rule), an unbounded transcript could
/// still produce a request large enough to fail at the HTTP/proxy layer with
/// no way to recover other than discarding data. 200,000 characters is
/// roughly 3-4 hours of normal-paced conversation; a meeting beyond that is
/// truncated in the *exported* page only, with a note pointing back at the
/// always-complete local `transcript.json`/`summary.md`, which remain the
/// source of truth regardless of what was exported.
pub const MAX_TRANSCRIPT_CHARS: usize = 200_000;

fn text_run(text: &str) -> Value {
    json!([{"insert": text}])
}
fn paragraph(text: &str) -> Value {
    json!({"type": "paragraph", "data": {"delta": text_run(text)}, "children": []})
}
fn heading(level: u8, text: &str) -> Value {
    json!({"type": "heading", "data": {"level": level, "delta": text_run(text)}, "children": []})
}
fn bulleted(text: &str) -> Value {
    json!({"type": "bulleted_list", "data": {"delta": text_run(text)}, "children": []})
}
fn quote(text: &str) -> Value {
    json!({"type": "quote", "data": {"delta": text_run(text)}, "children": []})
}

fn timestamp_label(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Truncates `turns` so their combined text stays within `max_chars`,
/// returning the kept turns and whether anything was cut. Keeps whole turns
/// only -- never cuts mid-utterance -- so a kept turn's text is always
/// exactly what was said.
pub fn truncate_turns(turns: &[SpeakerTurn], max_chars: usize) -> (Vec<SpeakerTurn>, bool) {
    let mut kept = Vec::new();
    let mut used = 0usize;
    for turn in turns {
        let len = turn.text.len();
        if used + len > max_chars && !kept.is_empty() {
            return (kept, true);
        }
        used += len;
        kept.push(turn.clone());
    }
    (kept, false)
}

fn summary_blocks(summary: &SummaryOutput) -> Vec<Value> {
    let mut blocks = vec![paragraph(&summary.summary)];
    if !summary.key_points.is_empty() {
        blocks.push(heading(3, "Key points"));
        blocks.extend(summary.key_points.iter().map(|p| bulleted(p)));
    }
    if !summary.decisions.is_empty() {
        blocks.push(heading(3, "Decisions"));
        blocks.extend(summary.decisions.iter().map(|d| bulleted(d)));
    }
    if !summary.action_items.is_empty() {
        blocks.push(heading(3, "Action items"));
        blocks.extend(summary.action_items.iter().map(|item| {
            let owner = item
                .owner
                .as_deref()
                .map(|o| format!(" — {o}"))
                .unwrap_or_default();
            let due = item
                .due
                .as_deref()
                .map(|d| format!(" (due {d})"))
                .unwrap_or_default();
            bulleted(&format!("{}{owner}{due}", item.task))
        }));
    }
    if !summary.open_questions.is_empty() {
        blocks.push(heading(3, "Open questions"));
        blocks.extend(summary.open_questions.iter().map(|q| bulleted(q)));
    }
    if blocks.is_empty() {
        blocks.push(paragraph("No summary was generated for this meeting."));
    }
    blocks
}

fn truncation_note(kept: usize, total: usize) -> String {
    format!(
        "Transcript truncated to the first {kept} of {total} turns for this export; the complete transcript remains available locally in this recording's transcript.json and summary.md."
    )
}

/// Builds the native `ai_meeting` block tree (see module docs for schema
/// sources). `speaker_info_map` is left unset -- speaker labels are already
/// human-readable presentation labels (`You`/`Speaker 1`/`Remote`), so no
/// separate id->name mapping is needed.
pub fn build_native_meeting_block(meeting: &MeetingExportData) -> Value {
    let (turns, truncated) = truncate_turns(&meeting.turns, MAX_TRANSCRIPT_CHARS);
    let summary_children: Vec<Value> = match &meeting.summary {
        Some(summary) => summary_blocks(summary),
        None => vec![paragraph("No summary was generated for this meeting.")],
    };
    let mut speaker_blocks: Vec<Value> = turns
        .iter()
        .map(|turn| {
            json!({
                "type": "ai_meeting_speaker",
                "data": {
                    "speaker_id": turn.speaker_label,
                    "timestamp": turn.start_ms / 1000,
                    "end_timestamp": turn.end_ms / 1000,
                },
                "children": [paragraph(&turn.text)],
            })
        })
        .collect();
    if truncated {
        speaker_blocks.push(json!({
            "type": "ai_meeting_speaker",
            "data": {"speaker_id": "Note", "timestamp": 0, "end_timestamp": 0},
            "children": [paragraph(&truncation_note(turns.len(), meeting.turns.len()))],
        }));
    }
    json!({
        "type": "ai_meeting",
        "data": {
            "title": meeting.title,
            "date": meeting.started_at_rfc3339,
            "meeting_source": "meeting-transcript-recorder",
            "meeting_app_name": "meeting-transcript-recorder",
            "show_notes_directly": false,
        },
        "children": [
            {"type": "ai_meeting_summary", "data": {}, "children": summary_children},
            {"type": "ai_meeting_notes", "data": {}, "children": [paragraph("No additional manual notes.")]},
            {"type": "ai_meeting_transcription", "data": {}, "children": speaker_blocks},
        ],
    })
}

/// Standard-blocks fallback: a heading, two collapsible toggle lists
/// ("Summary", "Transcript"), and one quote block per speaker turn -- built
/// only from block types documented as long-standing in every AppFlowy
/// client (see the research pass), for a client that does not yet render
/// `ai_meeting*`.
pub fn build_fallback_meeting_block(meeting: &MeetingExportData) -> Value {
    let (turns, truncated) = truncate_turns(&meeting.turns, MAX_TRANSCRIPT_CHARS);
    let summary_children: Vec<Value> = match &meeting.summary {
        Some(summary) => summary_blocks(summary),
        None => vec![paragraph("No summary was generated for this meeting.")],
    };
    let mut transcript_children: Vec<Value> = turns
        .iter()
        .map(|turn| {
            quote(&format!(
                "[{}] {}: {}",
                timestamp_label(turn.start_ms),
                turn.speaker_label,
                turn.text
            ))
        })
        .collect();
    if truncated {
        transcript_children.push(paragraph(&truncation_note(
            turns.len(),
            meeting.turns.len(),
        )));
    }
    json!([
        heading(1, &meeting.title),
        paragraph(&meeting.started_at_rfc3339),
        {"type": "toggle_list", "data": {"collapsed": false, "delta": text_run("Summary")}, "children": summary_children},
        {"type": "toggle_list", "data": {"collapsed": true, "delta": text_run("Transcript")}, "children": transcript_children},
    ])
}

/// Wraps a meeting block (native or fallback) in the page document root, the
/// exact shape passed as `page_data` to `AppFlowyClient::create_page`.
pub fn build_page_document(meeting: &MeetingExportData, use_native_block: bool) -> Value {
    let children = if use_native_block {
        vec![build_native_meeting_block(meeting)]
    } else {
        build_fallback_meeting_block(meeting)
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    json!({"type": "page", "data": {}, "children": children})
}

#[cfg(test)]
#[path = "appflowy_blocks_tests.rs"]
mod tests;
