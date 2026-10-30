use super::*;
use crate::appflowy_blocks::SpeakerTurn;
use crate::summary_types::{ActionItem, SummaryOutput};

fn sample_meeting() -> MeetingExportData {
    MeetingExportData {
        title: "Weekly sync".into(),
        started_at_rfc3339: "2026-09-25T15:00:00Z".into(),
        summary: Some(SummaryOutput {
            title: "Weekly sync".into(),
            summary: "The team discussed the roadmap.".into(),
            key_points: vec!["Roadmap reviewed".into()],
            decisions: vec!["Ship behind a flag".into()],
            action_items: vec![
                ActionItem {
                    task: "Write design doc".into(),
                    owner: Some("Alex".into()),
                    due: Some("Friday".into()),
                },
                ActionItem {
                    task: "Book the room".into(),
                    owner: None,
                    due: None,
                },
            ],
            open_questions: vec!["Who owns rollout?".into()],
            language: "en".into(),
        }),
        turns: vec![
            SpeakerTurn {
                speaker_label: "You".into(),
                start_ms: 0,
                end_ms: 12_000,
                text: "Let's kick off.".into(),
            },
            SpeakerTurn {
                speaker_label: "Speaker 1".into(),
                start_ms: 3_725_000,
                end_ms: 3_740_000,
                text: "Sounds good.".into(),
            },
        ],
    }
}
fn build(meeting: &MeetingExportData) -> String {
    build_meeting_markdown(meeting, "2026-09-25 17:00")
}
fn turn(label: &str, ms: u64, text: String) -> SpeakerTurn {
    SpeakerTurn {
        speaker_label: label.into(),
        start_ms: ms,
        end_ms: ms + 1000,
        text,
    }
}

#[test]
fn renders_title_local_start_summary_sections_and_action_items_as_a_checklist() {
    let md = build(&sample_meeting());
    assert!(
        md.starts_with("**Weekly sync** · 2026-09-25 17:00 (local time)"),
        "{md}"
    );
    assert!(md.contains("## Summary\n\nThe team discussed the roadmap."));
    assert!(md.contains("### Key points\n\n- Roadmap reviewed"));
    assert!(md.contains("### Decisions\n\n- Ship behind a flag"));
    assert!(md.contains(
        "### Action items\n\n- [ ] Write design doc — Alex (due Friday)\n- [ ] Book the room\n"
    ));
    assert!(md.contains("### Open questions\n\n- Who owns rollout?"));
}

#[test]
fn transcript_turns_are_blockquotes_with_bold_timestamp_and_speaker_label() {
    let md = build(&sample_meeting());
    assert!(md.contains("## Transcript\n\n> **00:00:00 · You:** Let's kick off.\n\n> **01:02:05 · Speaker 1:** Sounds good."), "{md}");
}

#[test]
fn a_missing_summary_renders_a_placeholder_and_the_transcript_still_follows() {
    let mut meeting = sample_meeting();
    meeting.summary = None;
    let md = build(&meeting);
    assert!(md.contains("## Summary\n\nNo summary was generated for this meeting."));
    assert!(md.contains("## Transcript"));
    assert!(!md.contains("### Key points"));
}

#[test]
fn empty_sections_are_omitted() {
    let mut meeting = sample_meeting();
    let summary = meeting.summary.as_mut().unwrap();
    summary.decisions.clear();
    summary.open_questions.clear();
    let md = build(&meeting);
    assert!(!md.contains("### Decisions"));
    assert!(!md.contains("### Open questions"));
    assert!(md.contains("### Key points"));
}

#[test]
fn french_and_other_utf8_text_is_kept_verbatim() {
    let mut meeting = sample_meeting();
    meeting.title = "Réunion d'équipe — point hebdo".into();
    meeting.turns = vec![turn(
        "Speaker 1",
        5_000,
        "Très bien, à demain ! Ça marche : on décide d'envoyer l'offre à 17 h 30. 你好 🎉".into(),
    )];
    let md = build(&meeting);
    assert!(md.contains("**Réunion d'équipe — point hebdo**"), "{md}");
    assert!(md.contains("> **00:00:05 · Speaker 1:** Très bien, à demain ! Ça marche : on décide d'envoyer l'offre à 17 h 30. 你好 🎉"), "{md}");
}

#[test]
fn markdown_special_characters_in_user_text_are_escaped() {
    assert_eq!(escape_inline("*bold* and _it_"), "\\*bold\\* and \\_it\\_");
    assert_eq!(
        escape_inline("[link](http://x) <b>hi</b>"),
        "\\[link\\](http://x) \\<b\\>hi\\</b\\>"
    );
    assert_eq!(
        escape_inline("`code` ~strike~ a|b $5$ R&D back\\slash"),
        "\\`code\\` \\~strike\\~ a\\|b \\$5\\$ R\\&D back\\\\slash"
    );
    assert_eq!(escape_inline("issue #42"), "issue \\#42");
}

#[test]
fn text_that_would_start_a_block_is_neutralised_at_the_line_start() {
    assert_eq!(escape_inline("- not a list"), "\\- not a list");
    assert_eq!(escape_inline("+ nor this"), "\\+ nor this");
    assert_eq!(
        escape_inline("1. nor a numbered list"),
        "1\\. nor a numbered list"
    );
    assert_eq!(escape_inline("2) nor this"), "2\\) nor this");
    assert_eq!(escape_inline("# nor a heading"), "\\# nor a heading");
    assert_eq!(escape_inline("> nor a quote"), "\\> nor a quote");
    assert_eq!(escape_inline("---"), "\\---");
    assert_eq!(escape_inline("==="), "\\===");
    // Mid-line dashes and numbers are left alone.
    assert_eq!(
        escape_inline("a - b, 3 items, v1.2"),
        "a - b, 3 items, v1.2"
    );
}

#[test]
fn hostile_transcript_text_cannot_break_out_of_its_quote_or_inject_blocks() {
    let mut meeting = sample_meeting();
    meeting.turns = vec![turn("You", 0, "first line\n\n# Injected heading\n- injected bullet\n```\ncode\n```\n---\n[x](javascript:alert(1))".into())];
    let md = build(&meeting);
    let transcript = md.split("## Transcript\n\n").nth(1).unwrap();
    assert_eq!(
        transcript.lines().filter(|l| !l.is_empty()).count(),
        1,
        "newlines collapse into one quoted line: {transcript}"
    );
    assert!(transcript.starts_with("> **00:00:00 · You:** first line"));
    assert!(transcript.contains("\\# Injected heading"));
    assert!(transcript.contains("\\`\\`\\`"));
    assert!(transcript.contains("\\[x\\]"));
}

#[test]
fn a_hostile_speaker_label_and_summary_text_are_escaped_too() {
    let mut meeting = sample_meeting();
    meeting.turns = vec![turn("Jean_Luc *the* [boss]", 0, "Bonjour".into())];
    meeting.summary.as_mut().unwrap().summary =
        "# Fake heading\n\n- fake bullet\nsecond line".into();
    meeting.summary.as_mut().unwrap().key_points = vec!["- nested?".into()];
    let md = build(&meeting);
    assert!(
        md.contains("**00:00:00 · Jean\\_Luc \\*the\\* \\[boss\\]:**"),
        "{md}"
    );
    assert!(
        md.contains("## Summary\n\n\\# Fake heading\n\n\\- fake bullet\nsecond line"),
        "{md}"
    );
    assert!(md.contains("- \\- nested?"), "{md}");
}

#[test]
fn a_long_transcript_is_capped_by_characters_with_a_visible_note() {
    let mut meeting = sample_meeting();
    meeting.turns = (0..5_000)
        .map(|i| {
            turn(
                "Speaker 1",
                i * 1000,
                format!("Turn number {i}: {}", "word ".repeat(30)),
            )
        })
        .collect();
    let md = build(&meeting);
    assert!(
        md.chars().count() <= MAX_MARKDOWN_CHARS,
        "{} chars",
        md.chars().count()
    );
    assert!(md.len() <= MAX_MARKDOWN_BYTES);
    assert!(
        md.contains("Transcript truncated to the first "),
        "a visible truncation note must be present"
    );
    assert!(md.contains("of 5000 turns"));
    assert!(md.contains("Turn number 0:"), "the first turns are kept");
    assert!(
        !md.contains("Turn number 4999:"),
        "the tail is the part that is dropped"
    );
}

#[test]
fn the_cap_counts_utf8_bytes_not_just_characters() {
    // 3 bytes per character: 100 turns of 1,000 characters is 100,000
    // characters (under the character cap) but 300,000+ bytes -- over what
    // atomik-kb accepts (256 KiB) -- so the byte cap must be what bites.
    let mut meeting = sample_meeting();
    meeting.turns = (0..100)
        .map(|i| turn("You", i * 1000, "あ".repeat(1_000)))
        .collect();
    let md = build(&meeting);
    assert!(md.chars().count() < MAX_MARKDOWN_CHARS);
    assert!(md.len() <= MAX_MARKDOWN_BYTES, "{} bytes", md.len());
    assert!(md.len() < 256 * 1024);
    assert!(md.contains("Transcript truncated to the first "));
}

#[test]
fn the_kept_turns_are_a_whole_prefix_and_never_cut_mid_turn() {
    let mut meeting = sample_meeting();
    meeting.turns = (0..2_000)
        .map(|i| turn("You", i * 1000, format!("END{i} {}", "é".repeat(400))))
        .collect();
    let md = build(&meeting);
    let kept = md.matches("> **").count();
    assert!(kept > 10 && kept < 2_000, "{kept}");
    for i in 0..kept {
        assert!(md.contains(&format!("END{i} ")), "turn {i} must be present");
    }
    assert!(md.contains(&format!("the first {kept} of 2000 turns")));
}

#[test]
fn one_oversized_turn_is_clipped_rather_than_dropping_the_whole_transcript() {
    let mut meeting = sample_meeting();
    meeting.turns = vec![turn("You", 0, "x".repeat(400_000))];
    let md = build(&meeting);
    assert!(md.len() <= MAX_MARKDOWN_BYTES);
    assert!(md.chars().count() <= MAX_MARKDOWN_CHARS);
    assert!(md.contains("> **00:00:00 · You:** xxxx"));
    assert!(md.contains("Transcript truncated"));
}

#[test]
fn an_enormous_summary_still_respects_both_caps() {
    let mut meeting = sample_meeting();
    meeting.summary.as_mut().unwrap().summary = "ü".repeat(300_000);
    let md = build(&meeting);
    assert!(md.len() <= MAX_MARKDOWN_BYTES);
    assert!(md.chars().count() <= MAX_MARKDOWN_CHARS);
    assert!(md.is_char_boundary(md.len()));
}

#[test]
fn a_meeting_under_the_caps_has_no_truncation_note() {
    assert!(!build(&sample_meeting()).contains("truncated"));
}

#[test]
fn turns_with_no_text_are_skipped() {
    let mut meeting = sample_meeting();
    meeting.turns.push(turn("You", 9_000, "   \n ".into()));
    assert_eq!(build(&meeting).matches("> **").count(), 2);
}
