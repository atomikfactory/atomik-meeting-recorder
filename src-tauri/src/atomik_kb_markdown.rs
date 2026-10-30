//! Builds the Markdown body sent to atomik-kb at page-creation time. A pure
//! function of `MeetingExportData` (the same data AppFlowy's export is built
//! from, see `appflowy_blocks.rs`) -- no I/O, no network -- so it is
//! exhaustively unit tested without a fake server.
//!
//! atomik-kb converts the Markdown with its own importer (headings, bullets,
//! `- [ ]` task items, blockquotes; links and images stay text; `$...$` is
//! math). Everything that comes from a person or a model (titles, summary
//! text, speaker labels, transcript text) is therefore escaped before it is
//! embedded, so a transcript line can never turn into a heading, a list, a
//! code fence, a link or math.
use crate::appflowy_blocks::MeetingExportData;
use crate::summary_types::SummaryOutput;

/// Cap on the whole Markdown document, in characters. Same figure as
/// `appflowy_blocks::MAX_TRANSCRIPT_CHARS`: roughly 3-4 hours of conversation.
pub const MAX_MARKDOWN_CHARS: usize = 200_000;
/// Cap on the whole document in UTF-8 BYTES. atomik-kb refuses Markdown over
/// 256 KiB (262,144 bytes), and a character count alone does not bound that
/// (French accents are 2 bytes, CJK 3, emoji 4), so both caps apply and
/// whichever bites first wins. Kept a little under the server limit.
pub const MAX_MARKDOWN_BYTES: usize = 250_000;
/// Room kept free for the closing truncation note.
const NOTE_RESERVE: usize = 500;

/// Escapes text for embedding in a single line of Markdown. Backslash-escapes
/// every character the importer could read as inline syntax (`\ ` * _ [ ] < >
/// ~ | $ & #`), and neutralises a line-start that would open a block (`-`,
/// `+`, `=` runs, `1.` / `1)`). Newlines are the caller's concern (see
/// `one_line_text`).
pub fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '~' | '|' | '$' | '&' | '#'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    escape_line_start(out)
}
/// A line that starts with a bullet/rule/setext marker or `N.` / `N)` would
/// open a list, rule or heading; escape the one character that does it.
fn escape_line_start(line: String) -> String {
    let mut chars = line.char_indices();
    match chars.next() {
        Some((_, c @ ('-' | '+' | '='))) => format!("\\{c}{}", &line[1..]),
        Some((_, c)) if c.is_ascii_digit() => {
            let digits = line.chars().take_while(char::is_ascii_digit).count();
            match line[digits..].chars().next() {
                Some(m @ ('.' | ')')) => format!("{}\\{m}{}", &line[..digits], &line[digits + 1..]),
                _ => line,
            }
        }
        _ => line,
    }
}
/// Collapses every run of whitespace (newlines included) to one space, so
/// user text can never start a new block inside a quote or bullet.
fn one_line_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn escaped_line(text: &str) -> String {
    escape_inline(&one_line_text(text))
}
/// Multi-line text (the summary paragraph): each line is escaped on its own
/// and stripped of leading indentation (four spaces would be a code block);
/// blank lines still separate paragraphs.
fn escaped_block(text: &str) -> String {
    text.lines()
        .map(|l| escape_inline(l.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn timestamp_label(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn push_list(out: &mut String, heading: &str, items: Vec<String>) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("### {heading}\n\n"));
    for item in items {
        out.push_str(&format!("{item}\n"));
    }
    out.push('\n');
}
fn summary_markdown(summary: &SummaryOutput) -> String {
    let mut out = format!("{}\n\n", escaped_block(&summary.summary));
    push_list(
        &mut out,
        "Key points",
        summary
            .key_points
            .iter()
            .map(|p| format!("- {}", escaped_line(p)))
            .collect(),
    );
    push_list(
        &mut out,
        "Decisions",
        summary
            .decisions
            .iter()
            .map(|d| format!("- {}", escaped_line(d)))
            .collect(),
    );
    push_list(
        &mut out,
        "Action items",
        summary
            .action_items
            .iter()
            .map(|item| {
                let owner = item
                    .owner
                    .as_deref()
                    .map(|o| format!(" — {}", escaped_line(o)))
                    .unwrap_or_default();
                let due = item
                    .due
                    .as_deref()
                    .map(|d| format!(" (due {})", escaped_line(d)))
                    .unwrap_or_default();
                format!("- [ ] {}{owner}{due}", escaped_line(&item.task))
            })
            .collect(),
    );
    push_list(
        &mut out,
        "Open questions",
        summary
            .open_questions
            .iter()
            .map(|q| format!("- {}", escaped_line(q)))
            .collect(),
    );
    out
}

/// Longest prefix of `text` within both budgets, cut on a character boundary.
fn clamp(text: &str, max_chars: usize, max_bytes: usize) -> &str {
    let mut end = 0;
    for (count, (i, c)) in text.char_indices().enumerate() {
        if count >= max_chars || i + c.len_utf8() > max_bytes {
            break;
        }
        end = i + c.len_utf8();
    }
    &text[..end]
}

fn truncation_note(kept: usize, total: usize, last_cut_short: bool) -> String {
    format!(
        "> *Transcript truncated to the first {kept} of {total} turns{} for this export; the complete transcript remains available locally in this recording's transcript.json and summary.md.*\n",
        if last_cut_short { " (the last one cut short)" } else { "" }
    )
}

/// The whole page body: a metadata line (title and local start time), the
/// summary sections (key points, decisions, action items with owners as
/// `- [ ]` tasks, open questions), then one blockquote per transcript turn
/// with a bold timestamp and the presentation speaker label. `started_local`
/// is the meeting's local start time, formatted by the caller. The document
/// never exceeds `MAX_MARKDOWN_CHARS` characters or `MAX_MARKDOWN_BYTES`
/// bytes: whole turns are kept from the start, the rest is dropped with a
/// visible note (the local transcript.json/summary.md stay complete).
pub fn build_meeting_markdown(meeting: &MeetingExportData, started_local: &str) -> String {
    let summary = match &meeting.summary {
        Some(summary) => summary_markdown(summary),
        None => "No summary was generated for this meeting.\n\n".to_string(),
    };
    let head = format!(
        "**{}** · {} (local time)\n\n## Summary\n\n{summary}## Transcript\n\n",
        escaped_line(&meeting.title),
        escape_inline(started_local)
    );
    let max_chars = MAX_MARKDOWN_CHARS - NOTE_RESERVE;
    let max_bytes = MAX_MARKDOWN_BYTES - NOTE_RESERVE;
    if head.chars().count() > max_chars || head.len() > max_bytes {
        let mut out = clamp(&head, max_chars, max_bytes).to_string();
        out.push_str("…\n\n> *The summary was cut short and the transcript left out of this export because the page would be too large; the complete transcript and summary remain available locally in this recording's transcript.json and summary.md.*\n");
        return out;
    }
    let mut out = head;
    let (mut used_chars, mut used_bytes) = (out.chars().count(), out.len());
    let turns: Vec<_> = meeting
        .turns
        .iter()
        .filter(|t| !t.text.trim().is_empty())
        .collect();
    let (mut kept, mut cut_short) = (0, false);
    for turn in &turns {
        let line = format!(
            "> **{} · {}:** {}\n\n",
            timestamp_label(turn.start_ms),
            escaped_line(&turn.speaker_label),
            escaped_line(&turn.text)
        );
        let (chars, bytes) = (line.chars().count(), line.len());
        if used_chars + chars <= max_chars && used_bytes + bytes <= max_bytes {
            out.push_str(&line);
            used_chars += chars;
            used_bytes += bytes;
            kept += 1;
            continue;
        }
        if kept == 0 {
            // Never drop a transcript wholesale because its first turn is huge.
            let room = clamp(
                &line,
                max_chars.saturating_sub(used_chars + 3),
                max_bytes.saturating_sub(used_bytes + 3),
            );
            out.push_str(room.trim_end());
            out.push_str("…\n\n");
            kept = 1;
            cut_short = true;
        }
        break;
    }
    if kept < turns.len() || cut_short {
        out.push_str(&truncation_note(kept, turns.len(), cut_short));
    }
    out
}

#[cfg(test)]
#[path = "atomik_kb_markdown_tests.rs"]
mod tests;
