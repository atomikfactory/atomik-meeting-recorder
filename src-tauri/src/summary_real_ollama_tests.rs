//! Real-Ollama verification. Not part of the default suite: `#[ignore]`d and
//! env-gated so CI and normal `cargo test` runs never depend on a running
//! Ollama server. Reads `transcript.json` files from a user-provided
//! directory -- never the repo, never `%APPDATA%` directly -- so no private
//! recording data is ever bundled or committed.
//!
//! Run explicitly, e.g. against local scratch copies of real transcripts:
//!   SUMMARY_REAL_OLLAMA=1 SUMMARY_REAL_TRANSCRIPTS_DIR="<dir of subfolders each with a transcript.json>" \
//!     cargo test --manifest-path src-tauri/Cargo.toml --locked -- --ignored real_ollama_summarizes
//!
//! Optional: SUMMARY_REAL_MODEL (default qwen3:8b), SUMMARY_REAL_OLLAMA_URL
//! (default http://127.0.0.1:11434).
use crate::{
    recording::{Recording, Track},
    summary_engine::{OllamaSummaryEngine, SummaryEngine, SummaryRequest},
    transcription_types::Transcript,
};
use std::{fs, sync::atomic::AtomicBool, time::Instant};

fn synthetic_recording(transcript: &Transcript) -> Recording {
    let mut tracks: Vec<Track> = Vec::new();
    for segment in &transcript.segments {
        if !tracks.iter().any(|t| t.source_id == segment.source_id) {
            let direction = if segment.source_name.to_lowercase().contains("speaker")
                || segment.source_name.to_lowercase().contains("system")
                || segment.source_name.to_lowercase().contains("loopback")
            {
                "output"
            } else {
                "input"
            };
            tracks.push(Track {
                source_id: segment.source_id.clone(),
                name: segment.source_name.clone(),
                direction: direction.into(),
                sample_rate: 16_000,
                channels: 1,
                file_path: String::new(),
                frames_written: 1,
                dropped_samples: 0,
                error: None,
            });
        }
    }
    let duration_ms = transcript
        .segments
        .iter()
        .map(|s| s.end_ms)
        .max()
        .unwrap_or(0);
    Recording {
        recording_id: transcript.recording_id.clone(),
        started_at: 0,
        ended_at: Some(duration_ms),
        duration_ms,
        status: "recorded".into(),
        tracks,
        error: None,
        meeting_title: None,
        storage_path: None,
        library_root: None,
        auto_transcribe: true,
        transcription_status: "completed".into(),
        appflowy_status: "idle".into(),
    }
}

#[test]
#[ignore]
fn real_ollama_summarizes_copied_transcripts() {
    let Ok(_) = std::env::var("SUMMARY_REAL_OLLAMA") else {
        eprintln!("skipping: set SUMMARY_REAL_OLLAMA=1 and SUMMARY_REAL_TRANSCRIPTS_DIR to run this against a real Ollama server");
        return;
    };
    let dir = std::env::var("SUMMARY_REAL_TRANSCRIPTS_DIR")
        .expect("set SUMMARY_REAL_TRANSCRIPTS_DIR to a directory of subfolders, each containing a transcript.json");
    let ollama_url = std::env::var("SUMMARY_REAL_OLLAMA_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let model = std::env::var("SUMMARY_REAL_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
    let engine = OllamaSummaryEngine::new();
    engine
        .test_connection(&ollama_url)
        .expect("Ollama must be reachable at SUMMARY_REAL_OLLAMA_URL for this real-run test");

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("SUMMARY_REAL_TRANSCRIPTS_DIR must exist")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    assert!(
        !entries.is_empty(),
        "no transcript subfolders found under {dir}"
    );

    println!("\n=== Real Ollama summary verification ({model} @ {ollama_url}) ===");
    for entry in entries {
        let folder = entry.path();
        let json_path = folder.join("transcript.json");
        if !json_path.exists() {
            continue;
        }
        let name = folder.file_name().unwrap().to_string_lossy().to_string();
        let bytes = fs::read(&json_path).unwrap();
        let transcript: Transcript = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{name}: transcript.json did not parse: {e}"));
        let recording = synthetic_recording(&transcript);
        let request = SummaryRequest {
            recording: recording.clone(),
            local_speaker_name: transcript
                .local_speaker_name
                .clone()
                .unwrap_or_else(|| "You".into()),
            remote_speaker_labels: None,
            names: Default::default(),
            transcript: transcript.clone(),
            ollama_url: ollama_url.clone(),
            model: model.clone(),
        };
        let started = Instant::now();
        let mut progress_calls = Vec::new();
        let mut progress = |p: u8| progress_calls.push(p);
        let result = engine.summarize(&request, &AtomicBool::new(false), &mut progress);
        let elapsed = started.elapsed();
        match &result {
            Ok(output) => {
                assert!(output.is_meaningful(), "{name}: summary was empty");
                let summary_md = render_markdown(&recording, &model, output);
                let out_path = folder.join("summary.md");
                let _ = fs::write(&out_path, &summary_md);
                let out_json = folder.join("summary.json");
                let _ = fs::write(&out_json, serde_json::to_vec_pretty(output).unwrap());
                println!(
                    "\n--- {name} ---\nsegments: {} | wall time: {:.1}s | JSON validated: yes | title: {}",
                    transcript.segments.len(),
                    elapsed.as_secs_f64(),
                    output.title
                );
                if name.contains("d3863b42") || name.contains("cd574cf7") {
                    println!(
                        "### Generated summary.md for {name} ###\n{summary_md}\n### end {name} ###"
                    );
                }
            }
            Err(error) => {
                println!(
                    "\n--- {name} ---\nsegments: {} | wall time: {:.1}s | FAILED: {error}",
                    transcript.segments.len(),
                    elapsed.as_secs_f64()
                );
                panic!("{name}: real Ollama summarization failed: {error}");
            }
        }
    }
}

fn render_markdown(
    recording: &Recording,
    model: &str,
    output: &crate::summary_types::SummaryOutput,
) -> String {
    let mut md = format!(
        "# {}\n\n- Recording ID: `{}`\n- Model: {}\n- Language: {}\n\n## Summary\n\n{}\n\n",
        output.title, recording.recording_id, model, output.language, output.summary
    );
    if !output.key_points.is_empty() {
        md.push_str("## Key points\n\n");
        for p in &output.key_points {
            md.push_str(&format!("- {p}\n"));
        }
        md.push('\n');
    }
    if !output.decisions.is_empty() {
        md.push_str("## Decisions\n\n");
        for d in &output.decisions {
            md.push_str(&format!("- {d}\n"));
        }
        md.push('\n');
    }
    if !output.action_items.is_empty() {
        md.push_str("## Action items\n\n");
        for item in &output.action_items {
            md.push_str(&format!(
                "- {}{}{}\n",
                item.task,
                item.owner
                    .as_deref()
                    .map(|o| format!(" — {o}"))
                    .unwrap_or_default(),
                item.due
                    .as_deref()
                    .map(|d| format!(" (due {d})"))
                    .unwrap_or_default(),
            ));
        }
        md.push('\n');
    }
    if !output.open_questions.is_empty() {
        md.push_str("## Open questions\n\n");
        for q in &output.open_questions {
            md.push_str(&format!("- {q}\n"));
        }
    }
    md
}
