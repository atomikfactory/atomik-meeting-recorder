//! Integration tests for the search index: reconcile (missing/stale/deleted),
//! corrupt-index rebuild, and query performance at scale. Query escaping,
//! tokenizer/diacritics, ranking, and snippet rendering are unit-tested next
//! to the code they cover (`search.rs`, `search_store.rs`).
use crate::{
    diarization_store::{DiarizationStore, ModelPair},
    diarization_types::{DiarizationOutput, SpeakerTurn},
    recording::{Recording, Track},
    recording_paths::create_recording_directory,
    recording_store::RecordingStore,
    search::{reconcile_for_test, reconcile_for_test_with_roots},
    search_store::{IndexRow, RecordingIndexState, SearchStore},
    summary::SummaryCheckpoint,
    summary_store::SummaryStore,
    summary_types::SummaryOutput,
    transcription::checksum,
    transcription_store::TranscriptionStore,
    transcription_types::{Transcript, TranscriptSegment},
};
use std::{fs, path::PathBuf, time::Instant};

fn test_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "meeting-search-test-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

struct Fixture {
    root: PathBuf,
    recordings: RecordingStore,
    id: String,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let root = test_root(name);
        let recordings = RecordingStore::open(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording = Recording {
            recording_id: id.clone(),
            started_at: 10,
            ended_at: Some(2000),
            duration_ms: 2000,
            status: "recorded".into(),
            meeting_title: Some("Weekly sync".into()),
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            error: None,
            transcription_status: "pending".into(),
            appflowy_status: "pending".into(),
            tracks: vec![Track {
                source_id: "mic".into(),
                name: "Microphone".into(),
                direction: "input".into(),
                sample_rate: 16000,
                channels: 1,
                file_path: dir.join("track-00.wav").display().to_string(),
                frames_written: 32000,
                dropped_samples: 0,
                error: None,
            }],
        };
        recordings.save(&recording).unwrap();
        Self {
            root,
            recordings,
            id,
        }
    }
    fn complete_transcript(&self, text: &str) -> String {
        let dir = self.root.join(&self.id);
        let transcript = Transcript {
            recording_id: self.id.clone(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec!["en".into()],
            local_speaker_name: Some("You".into()),
            segments: vec![TranscriptSegment {
                start_ms: 0,
                end_ms: 1000,
                text: text.into(),
                source_id: "mic".into(),
                source_name: "Microphone".into(),
            }],
        };
        let bytes = serde_json::to_vec_pretty(&transcript).unwrap();
        let json = dir.join("transcript.json");
        fs::write(&json, &bytes).unwrap();
        let registry = TranscriptionStore::open(&self.root.join("transcription.sqlite3")).unwrap();
        registry
            .enqueue(&self.id, "base", &transcript.model_hash)
            .unwrap();
        assert!(registry
            .claim(&self.id, "unused", &transcript.model_hash)
            .unwrap());
        registry
            .complete(
                &self.id,
                &json.display().to_string(),
                &dir.join("transcript.md").display().to_string(),
                &checksum(&bytes),
            )
            .unwrap();
        checksum(&bytes)
    }
    fn complete_summary(&self, parent_checksum: &str, summary_text: &str) {
        let dir = self.root.join(&self.id);
        let checkpoint = SummaryCheckpoint {
            recording_id: self.id.clone(),
            parent_checksum: parent_checksum.to_string(),
            model: "qwen3:8b".into(),
            output: SummaryOutput {
                title: "Weekly sync".into(),
                summary: summary_text.into(),
                key_points: vec!["Ship on Friday".into()],
                decisions: vec![],
                action_items: vec![],
                open_questions: vec![],
                language: "en".into(),
            },
        };
        let bytes = serde_json::to_vec_pretty(&checkpoint).unwrap();
        fs::write(dir.join("summary.json"), &bytes).unwrap();
        let store = SummaryStore::open(&self.root.join("summary.sqlite3")).unwrap();
        store.enqueue(&self.id, parent_checksum).unwrap();
        assert!(store.claim(&self.id, "qwen3:8b").unwrap());
        store
            .complete(&self.id, &checksum(&bytes), "Weekly sync")
            .unwrap();
    }
    fn delete(&self) {
        self.recordings.tombstone(&self.id, 999).unwrap();
    }
}

#[test]
fn reconcile_indexes_a_never_indexed_recording() {
    let f = Fixture::new("missing");
    f.complete_transcript("We reached a decision about the budget.");
    let index = SearchStore::open_in_memory().unwrap();
    reconcile_for_test(&f.root, &index);
    let hits = index.search("\"budget\"", 10, 3).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].recording_id, f.id);
    assert_eq!(hits[0].title, "Weekly sync");
}

#[test]
fn reconcile_reindexes_when_transcript_checksum_is_stale() {
    let f = Fixture::new("stale");
    let checksum = f.complete_transcript("Original wording about the roadmap.");
    let index = SearchStore::open_in_memory().unwrap();
    // Seed the index with a state row claiming a different (stale) checksum,
    // and a row that should be replaced.
    index
        .replace_recording(
            &f.id,
            "Weekly sync",
            10,
            &RecordingIndexState {
                transcript_checksum: Some(format!("not-{checksum}")),
                summary_checksum: None,
            },
            &[IndexRow {
                kind: "segment",
                seq: 0,
                start_ms: Some(0),
                end_ms: Some(1000),
                speaker: Some("You".into()),
                text: "stale text nobody should find".into(),
            }],
        )
        .unwrap();
    reconcile_for_test(&f.root, &index);
    assert!(index.search("\"stale\"", 10, 3).unwrap().is_empty());
    assert_eq!(index.search("\"roadmap\"", 10, 3).unwrap().len(), 1);
}

#[test]
fn reconcile_removes_rows_for_a_deleted_recording() {
    let f = Fixture::new("deleted");
    f.complete_transcript("Notes before deletion.");
    let index = SearchStore::open_in_memory().unwrap();
    reconcile_for_test(&f.root, &index);
    assert_eq!(index.search("\"deletion\"", 10, 3).unwrap().len(), 1);
    f.delete();
    reconcile_for_test(&f.root, &index);
    assert!(index.search("\"deletion\"", 10, 3).unwrap().is_empty());
    assert!(index.indexed_ids().unwrap().is_empty());
}

#[test]
fn reconcile_indexes_summary_fields_once_available() {
    let f = Fixture::new("summary");
    let checksum = f.complete_transcript("Let's talk about the launch plan.");
    let index = SearchStore::open_in_memory().unwrap();
    reconcile_for_test(&f.root, &index);
    assert!(index.search("\"quarterly\"", 10, 3).unwrap().is_empty());
    f.complete_summary(&checksum, "We agreed on the quarterly launch plan.");
    reconcile_for_test(&f.root, &index);
    let hits = index.search("\"quarterly\"", 10, 3).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].hits.iter().any(|h| h.kind == "summary"));
    assert!(index.search("\"Ship\"*", 10, 3).unwrap()[0]
        .hits
        .iter()
        .any(|h| h.kind == "key_point"));
}

#[test]
fn corrupt_index_file_is_dropped_and_rebuilt() {
    let root = test_root("corrupt");
    let path = root.join("search.sqlite3");
    fs::write(&path, b"not a sqlite database at all").unwrap();
    let store = SearchStore::open_or_rebuild(&path).expect("rebuild should succeed");
    store
        .replace_recording(
            "r1",
            "Meeting",
            1,
            &RecordingIndexState::default(),
            &[IndexRow {
                kind: "segment",
                seq: 0,
                start_ms: Some(0),
                end_ms: Some(1000),
                speaker: None,
                text: "it works after rebuild".into(),
            }],
        )
        .unwrap();
    assert_eq!(store.search("\"works\"", 10, 3).unwrap().len(), 1);
}

#[test]
fn query_latency_stays_small_at_synthetic_scale() {
    let store = SearchStore::open_in_memory().unwrap();
    let words = [
        "budget",
        "roadmap",
        "decision",
        "launch",
        "customer",
        "release",
        "meeting",
        "action",
        "review",
        "planning",
        "engineering",
        "design",
        "marketing",
        "hiring",
        "vendor",
    ];
    let build_start = Instant::now();
    for meeting in 0..2000 {
        let mut rows = Vec::with_capacity(200);
        for seg in 0..200 {
            let text = format!(
                "{} {} discussion segment {} for meeting {}",
                words[(meeting * 7 + seg) % words.len()],
                words[(meeting + seg * 3) % words.len()],
                seg,
                meeting
            );
            rows.push(IndexRow {
                kind: "segment",
                seq: seg as i64,
                start_ms: Some(seg as i64 * 1000),
                end_ms: Some(seg as i64 * 1000 + 800),
                speaker: Some("You".into()),
                text,
            });
        }
        store
            .replace_recording(
                &format!("meeting-{meeting}"),
                &format!("Meeting {meeting}"),
                meeting as i64,
                &RecordingIndexState::default(),
                &rows,
            )
            .unwrap();
    }
    println!(
        "index build took: {:?} for 2000 meetings x 200 segments",
        build_start.elapsed()
    );

    // Worst case: a common word present in roughly 1/15 of all 400,000 rows
    // (~27,000 matches). Warm up (page cache, query plan) before timing.
    store.search("\"budget\"*", 20, 5).unwrap();
    let start = Instant::now();
    let results = store.search("\"budget\"*", 20, 5).unwrap();
    let common_elapsed = start.elapsed();
    assert!(!results.is_empty());
    println!("search perf (common term, ~27k matches): {common_elapsed:?}");
    // Generous bound to avoid flakiness on slow/shared CI hardware while
    // still catching an accidental full scan or an O(n^2) regression.
    assert!(
        common_elapsed.as_millis() < 500,
        "query took {common_elapsed:?}, expected well under 500ms"
    );

    // Typical case: a specific term that narrows to one meeting's ~200
    // segments (every row embeds its own meeting number), closer to what a
    // real search box query looks like.
    store.search("\"1234\"", 20, 5).unwrap();
    let start = Instant::now();
    let narrow_results = store.search("\"1234\"", 20, 5).unwrap();
    let narrow_elapsed = start.elapsed();
    assert_eq!(narrow_results.len(), 1);
    println!("search perf (specific term, ~200 matches): {narrow_elapsed:?}");
    assert!(
        narrow_elapsed.as_millis() < 100,
        "query took {narrow_elapsed:?}, expected well under 100ms"
    );
}

#[test]
#[ignore]
fn probe_index_size_per_hour_of_meeting() {
    // Not part of the regular suite: a one-off measurement to ground the
    // "index size per hour of meeting" estimate in docs/search.md. A typical
    // one-hour meeting transcribes to roughly 400-600 segments (a few
    // seconds of speech each); use 500 as a representative middle estimate,
    // plus a modest summary (a paragraph, five key points/decisions/action
    // items). Run with: cargo test --lib probe_index_size -- --ignored --nocapture
    let root = test_root("size-probe");
    let path = root.join("search.sqlite3");
    let store = SearchStore::open_or_rebuild(&path).unwrap();
    let mut rows = Vec::new();
    for seg in 0..500 {
        rows.push(IndexRow {
            kind: "segment",
            seq: seg,
            start_ms: Some(seg * 7000),
            end_ms: Some(seg * 7000 + 4000),
            speaker: Some("You".into()),
            text: "Let's make sure the release notes are ready before Friday and that everyone reviews the staging deploy checklist.".into(),
        });
    }
    rows.push(IndexRow { kind: "summary", seq: 0, start_ms: None, end_ms: None, speaker: None, text: "The team agreed on the Friday release timeline and reviewed the staging deploy checklist, flagging two open risks around the payments integration.".into() });
    for i in 0..5 {
        rows.push(IndexRow {
            kind: "key_point",
            seq: i,
            start_ms: None,
            end_ms: None,
            speaker: None,
            text: "Payments integration needs one more review pass before Friday.".into(),
        });
        rows.push(IndexRow {
            kind: "decision",
            seq: i,
            start_ms: None,
            end_ms: None,
            speaker: None,
            text: "Ship on Friday unless the payments review finds a blocker.".into(),
        });
        rows.push(IndexRow {
            kind: "action_item",
            seq: i,
            start_ms: None,
            end_ms: None,
            speaker: None,
            text: "Review the staging deploy checklist — Ben — Thursday".into(),
        });
    }
    store
        .replace_recording(
            "one-hour-meeting-0",
            "Weekly release sync",
            0,
            &RecordingIndexState::default(),
            &rows,
        )
        .unwrap();
    drop(store);
    let one_meeting_bytes = fs::metadata(&path).unwrap().len();
    println!(
        "search.sqlite3 for a single ~1-hour meeting ({} rows): {} bytes ({:.1} KB) -- inflated by fixed SQLite/FTS5 file overhead, not the steady-state marginal cost",
        rows.len(),
        one_meeting_bytes,
        one_meeting_bytes as f64 / 1024.0
    );
    // Steady-state marginal cost: add 19 more identical-shaped meetings (20
    // "hours" total) and divide by 19, which amortizes away the fixed
    // per-file overhead measured above.
    let store = SearchStore::open_or_rebuild(&path).unwrap();
    for meeting in 1..20 {
        store
            .replace_recording(
                &format!("one-hour-meeting-{meeting}"),
                "Weekly release sync",
                meeting,
                &RecordingIndexState::default(),
                &rows,
            )
            .unwrap();
    }
    drop(store);
    let twenty_meetings_bytes = fs::metadata(&path).unwrap().len();
    let marginal_per_hour = (twenty_meetings_bytes - one_meeting_bytes) as f64 / 19.0;
    println!(
        "search.sqlite3 for 20 such meetings: {} bytes ({:.1} KB); marginal cost per additional hour of meeting: {:.1} KB",
        twenty_meetings_bytes,
        twenty_meetings_bytes as f64 / 1024.0,
        marginal_per_hour / 1024.0
    );
}

/// A recording that lives under a *custom* library root (not the default
/// `default_root` that also holds the databases) must still have its
/// transcript, summary, and speaker labels indexed and searchable -- the
/// same as any other recording. This exercises the "known library roots"
/// plumbing all the way through `search.rs`'s own lookups
/// (`completed_transcript`/`recording_directory`) and the two enrichment
/// helpers it calls into, `summary::read_completed_summary` and
/// `diarization::read_completed_speaker_labels`.
#[test]
fn reconcile_indexes_summary_and_speaker_labels_for_a_recording_under_a_custom_library_root() {
    let default_root = test_root("custom-root-default");
    let custom_root = test_root("custom-root-library");
    let recordings = RecordingStore::open(&default_root).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut recording = Recording {
        recording_id: id.clone(),
        started_at: 10,
        ended_at: Some(2000),
        duration_ms: 2000,
        status: "recorded".into(),
        meeting_title: Some("Weekly sync".into()),
        storage_path: None,
        library_root: None,
        auto_transcribe: true,
        error: None,
        transcription_status: "pending".into(),
        appflowy_status: "pending".into(),
        tracks: vec![Track {
            // A loopback/output track: only remote (output) segments get
            // numbered "Speaker N" -- see `transcription::presentation_label`.
            source_id: "loop".into(),
            name: "Zoom".into(),
            direction: "output".into(),
            sample_rate: 16000,
            channels: 1,
            file_path: String::new(),
            frames_written: 32000,
            dropped_samples: 0,
            error: None,
        }],
    };
    // Places the recording's folder under `custom_root`, not `default_root`,
    // and records that in `recording.library_root`.
    let directory = create_recording_directory(&custom_root, &mut recording).unwrap();
    recording.tracks[0].file_path = directory.join("track-00.wav").display().to_string();
    recordings.save(&recording).unwrap();

    // Transcript: one segment on the output track.
    let transcript = Transcript {
        recording_id: id.clone(),
        model_key: "base".into(),
        model_hash: "a".repeat(64),
        warnings: vec![],
        languages_used: vec!["en".into()],
        local_speaker_name: Some("You".into()),
        segments: vec![TranscriptSegment {
            start_ms: 0,
            end_ms: 1000,
            text: "Let's talk about the northbound roadmap.".into(),
            source_id: "loop".into(),
            source_name: "Zoom".into(),
        }],
    };
    let transcript_bytes = serde_json::to_vec_pretty(&transcript).unwrap();
    fs::write(directory.join("transcript.json"), &transcript_bytes).unwrap();
    let transcript_checksum = checksum(&transcript_bytes);
    let registry = TranscriptionStore::open(&default_root.join("transcription.sqlite3")).unwrap();
    registry
        .enqueue(&id, "base", &transcript.model_hash)
        .unwrap();
    assert!(registry
        .claim(&id, "unused", &transcript.model_hash)
        .unwrap());
    registry
        .complete(
            &id,
            &directory.join("transcript.json").display().to_string(),
            &directory.join("transcript.md").display().to_string(),
            &transcript_checksum,
        )
        .unwrap();

    // Speaker identification: one turn spanning the whole segment, high
    // confidence, single speaker -- `align_speakers` renumbers by first
    // appearance starting at 1, so this labels the segment `Some(1)`
    // ("Speaker 1").
    let models = ModelPair {
        segmentation_path: "seg".into(),
        segmentation_hash: "a".repeat(64),
        embedding_path: "emb".into(),
        embedding_hash: "b".repeat(64),
    };
    let output = DiarizationOutput {
        recording_id: id.clone(),
        turns: vec![SpeakerTurn {
            source_id: "loop".into(),
            start_ms: 0,
            end_ms: 1000,
            speaker: 0,
            confidence: Some(0.99),
        }],
        warnings: vec![],
    };
    // `diarization::SpeakerCheckpoint` is private to that module; its JSON
    // shape (checked by `diarization::validate_checkpoint`) is reproduced
    // here directly rather than reaching into a private type.
    let checkpoint_bytes = serde_json::to_vec(&serde_json::json!({
        "recording_id": id,
        "parent_checksum": transcript_checksum,
        "models": models,
        "output": output,
        "labels": [1],
    }))
    .unwrap();
    fs::write(directory.join("speakers.json"), &checkpoint_bytes).unwrap();
    let checkpoint_checksum = checksum(&checkpoint_bytes);
    let speakers = DiarizationStore::open(&default_root.join("diarization.sqlite3")).unwrap();
    speakers.enqueue(&id, &transcript_checksum).unwrap();
    assert!(speakers.claim(&id, &models).unwrap());
    speakers.complete(&id, &checkpoint_checksum, 1).unwrap();

    // Summary.
    let summary_checkpoint = SummaryCheckpoint {
        recording_id: id.clone(),
        parent_checksum: transcript_checksum.clone(),
        model: "qwen3:8b".into(),
        output: SummaryOutput {
            title: "Weekly sync".into(),
            summary: "The team aligned on the northbound roadmap for next quarter.".into(),
            key_points: vec!["Next quarter starts in January.".into()],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            language: "en".into(),
        },
    };
    let summary_bytes = serde_json::to_vec_pretty(&summary_checkpoint).unwrap();
    fs::write(directory.join("summary.json"), &summary_bytes).unwrap();
    let summary_checksum = checksum(&summary_bytes);
    let summaries = SummaryStore::open(&default_root.join("summary.sqlite3")).unwrap();
    summaries.enqueue(&id, &transcript_checksum).unwrap();
    assert!(summaries.claim(&id, "qwen3:8b").unwrap());
    summaries
        .complete(&id, &summary_checksum, "Weekly sync")
        .unwrap();

    let index = SearchStore::open_in_memory().unwrap();
    // Only listing `custom_root` as a known extra root makes it resolvable
    // at all; without it, reconcile would silently skip every part of this
    // recording -- proving the reconcile pass here actually depends on it,
    // not merely tolerates its absence.
    reconcile_for_test(&default_root, &index);
    assert!(index.search("\"roadmap\"", 10, 3).unwrap().is_empty());
    assert!(index.search("\"aligned\"", 10, 3).unwrap().is_empty());

    reconcile_for_test_with_roots(&default_root, std::slice::from_ref(&custom_root), &index);

    let transcript_hits = index.search("\"roadmap\"", 10, 3).unwrap();
    assert_eq!(transcript_hits.len(), 1);
    assert_eq!(transcript_hits[0].recording_id, id);
    assert!(transcript_hits[0]
        .hits
        .iter()
        .any(|h| h.kind == "segment" && h.speaker.as_deref() == Some("Speaker 1")));

    let summary_hits = index.search("\"aligned\"", 10, 3).unwrap();
    assert_eq!(summary_hits.len(), 1);
    assert!(summary_hits[0].hits.iter().any(|h| h.kind == "summary"));
}
