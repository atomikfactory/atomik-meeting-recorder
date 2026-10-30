use super::*;
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
            action_items: vec![ActionItem {
                task: "Write design doc".into(),
                owner: Some("Alex".into()),
                due: Some("Friday".into()),
            }],
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
                start_ms: 13_000,
                end_ms: 40_000,
                text: "Sounds good.".into(),
            },
        ],
    }
}

#[test]
fn native_block_matches_the_documented_ai_meeting_schema() {
    let block = build_native_meeting_block(&sample_meeting());
    assert_eq!(block["type"], "ai_meeting");
    assert_eq!(block["data"]["title"], "Weekly sync");
    let children = block["children"].as_array().unwrap();
    let types: Vec<&str> = children
        .iter()
        .map(|c| c["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "ai_meeting_summary",
            "ai_meeting_notes",
            "ai_meeting_transcription"
        ]
    );
    let transcription = &children[2];
    let speakers = transcription["children"].as_array().unwrap();
    assert_eq!(speakers.len(), 2);
    assert_eq!(speakers[0]["type"], "ai_meeting_speaker");
    assert_eq!(speakers[0]["data"]["speaker_id"], "You");
    assert_eq!(speakers[0]["data"]["timestamp"], 0);
    assert_eq!(speakers[0]["data"]["end_timestamp"], 12);
    let paragraph = &speakers[0]["children"][0];
    assert_eq!(paragraph["type"], "paragraph");
    assert_eq!(paragraph["data"]["delta"][0]["insert"], "Let's kick off.");
}

#[test]
fn native_block_summary_children_include_all_summary_sections() {
    let block = build_native_meeting_block(&sample_meeting());
    let summary_block = &block["children"][0];
    assert_eq!(summary_block["type"], "ai_meeting_summary");
    let rendered = serde_json::to_string(&summary_block["children"]).unwrap();
    assert!(rendered.contains("Roadmap reviewed"));
    assert!(rendered.contains("Ship behind a flag"));
    assert!(rendered.contains("Write design doc"));
    assert!(rendered.contains("Alex"));
    assert!(rendered.contains("Who owns rollout?"));
}

#[test]
fn missing_summary_renders_a_placeholder_not_an_empty_block() {
    let mut meeting = sample_meeting();
    meeting.summary = None;
    let block = build_native_meeting_block(&meeting);
    let summary_children = block["children"][0]["children"].as_array().unwrap();
    assert_eq!(summary_children.len(), 1);
    assert!(summary_children[0]["data"]["delta"][0]["insert"]
        .as_str()
        .unwrap()
        .contains("No summary"));
}

#[test]
fn fallback_block_uses_only_standard_block_types() {
    let blocks = build_fallback_meeting_block(&sample_meeting());
    let array = blocks.as_array().unwrap();
    let types: Vec<&str> = array.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        vec!["heading", "paragraph", "toggle_list", "toggle_list"]
    );
    // No ai_meeting* type appears anywhere in the fallback tree.
    let rendered = serde_json::to_string(&blocks).unwrap();
    assert!(!rendered.contains("ai_meeting"));
    let transcript_toggle = &array[3];
    assert_eq!(
        transcript_toggle["data"]["delta"][0]["insert"],
        "Transcript"
    );
    let quotes = transcript_toggle["children"].as_array().unwrap();
    assert_eq!(quotes.len(), 2);
    assert_eq!(quotes[0]["type"], "quote");
    assert!(quotes[0]["data"]["delta"][0]["insert"]
        .as_str()
        .unwrap()
        .contains("You"));
}

#[test]
fn page_document_wraps_native_block_as_the_sole_child() {
    let document = build_page_document(&sample_meeting(), true);
    assert_eq!(document["type"], "page");
    let children = document["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["type"], "ai_meeting");
}

#[test]
fn page_document_wraps_fallback_blocks_as_top_level_children() {
    let document = build_page_document(&sample_meeting(), false);
    let children = document["children"].as_array().unwrap();
    assert_eq!(children.len(), 4);
    assert_eq!(children[0]["type"], "heading");
}

#[test]
fn french_utf8_and_special_characters_round_trip_through_the_delta() {
    let mut meeting = sample_meeting();
    meeting.title = "Réunion hebdomadaire".into();
    meeting.turns = vec![SpeakerTurn {
        speaker_label: "Vous".into(),
        start_ms: 0,
        end_ms: 5_000,
        text: "Résumé : \"c'était très clair\", n'est-ce pas ? Coût: 100% \\ garanti\nligne 2"
            .into(),
    }];
    meeting.summary = Some(SummaryOutput {
        title: "Réunion".into(),
        summary: "Discussion sur la stratégie à cinq ans.".into(),
        ..Default::default()
    });
    let native = build_native_meeting_block(&meeting);
    let serialized = serde_json::to_string(&native).unwrap();
    let reparsed: Value = serde_json::from_str(&serialized).unwrap();
    let text = &reparsed["children"][2]["children"][0]["children"][0]["data"]["delta"][0]["insert"];
    assert_eq!(
        text.as_str().unwrap(),
        "Résumé : \"c'était très clair\", n'est-ce pas ? Coût: 100% \\ garanti\nligne 2"
    );
    assert_eq!(reparsed["data"]["title"], "Réunion hebdomadaire");
}

#[test]
fn truncate_turns_keeps_whole_turns_within_the_budget() {
    let turns: Vec<SpeakerTurn> = (0..10)
        .map(|i| SpeakerTurn {
            speaker_label: "You".into(),
            start_ms: i * 1000,
            end_ms: i * 1000 + 500,
            text: "x".repeat(100),
        })
        .collect();
    let (kept, truncated) = truncate_turns(&turns, 350);
    assert!(truncated);
    assert_eq!(kept.len(), 3);
    let (kept_all, truncated_all) = truncate_turns(&turns, 10_000);
    assert!(!truncated_all);
    assert_eq!(kept_all.len(), 10);
}

#[test]
fn oversized_transcript_is_truncated_with_a_visible_note_in_both_formats() {
    let mut meeting = sample_meeting();
    meeting.turns = (0..10)
        .map(|i| SpeakerTurn {
            speaker_label: "You".into(),
            start_ms: i * 1000,
            end_ms: i * 1000 + 500,
            text: "x".repeat(MAX_TRANSCRIPT_CHARS / 5),
        })
        .collect();
    let native = build_native_meeting_block(&meeting);
    let native_rendered = serde_json::to_string(&native).unwrap();
    assert!(native_rendered.contains("Transcript truncated"));
    let fallback = build_fallback_meeting_block(&meeting);
    let fallback_rendered = serde_json::to_string(&fallback).unwrap();
    assert!(fallback_rendered.contains("Transcript truncated"));
}

#[test]
fn a_transcript_within_budget_is_never_truncated() {
    let meeting = sample_meeting();
    let native = build_native_meeting_block(&meeting);
    assert!(!serde_json::to_string(&native)
        .unwrap()
        .contains("Transcript truncated"));
}
