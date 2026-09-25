//! Manual, real end-to-end verification: turbo Whisper (optionally CUDA) +
//! AEC + echo-dedup + real sherpa-onnx speaker identification + a real local
//! Ollama summary, chained exactly as the app's own worker pipeline would run
//! them (see `transcription.rs`/`diarization.rs`/`summary.rs`), but driven
//! directly against copied track `.wav` files instead of the app's sqlite job
//! queues -- this is a one-off verification harness, not a shipped feature.
//! `#[ignore]`d and env-gated so CI and normal `cargo test` runs never depend
//! on a local Whisper model, licensed diarization models, or a running Ollama
//! server, and no private recording data is ever bundled or committed.
//!
//! Run explicitly, e.g. (PowerShell, from a CUDA release build):
//!   $env:SPEAKERS_E2E = "1"
//!   $env:SPEAKERS_E2E_WHISPER_MODEL = "<path to ggml turbo model>"
//!   $env:SPEAKERS_E2E_SEGMENTATION_MODEL = "<path to pyannote segmentation model.onnx>"
//!   $env:SPEAKERS_E2E_EMBEDDING_MODEL = "<path to nemo_en_titanet_small.onnx>"
//!   $env:SPEAKERS_E2E_RECORDING_DIR = "<dir containing this one recording's track-*.wav files>"
//!   $env:SPEAKERS_E2E_OUTPUT_DIR = "<dir to write transcript.md/summary.md into>"
//!   cargo test --release --features cuda --manifest-path src-tauri/Cargo.toml --locked \
//!     -- --ignored real_end_to_end_speakers_pipeline --nocapture
//!
//! Optional: `SPEAKERS_E2E_LOCAL_NAME` (default "You"), `SPEAKERS_E2E_OLLAMA_MODEL`
//! (default qwen3:8b), `SPEAKERS_E2E_OLLAMA_URL` (default http://127.0.0.1:11434).
//! Track directions are guessed from filename/content: exactly two
//! `track-*.wav` files are expected, and the one with a WASAPI/loopback-style
//! name or (as a fallback) the second alphabetically is treated as the output
//! (remote/loopback) track; override with `SPEAKERS_E2E_INPUT_TRACK` /
//! `SPEAKERS_E2E_OUTPUT_TRACK` (file names) if that guess is wrong for a given
//! recording.
use crate::{
    diarization_engine::create_diarizer,
    diarization_types::{DiarizationOutput, SpeakerTurn},
    recording::{Recording, Track},
    speaker_alignment::align_speakers,
    summary::SummaryCheckpoint,
    summary_engine::{OllamaSummaryEngine, SummaryEngine, SummaryRequest},
    transcription::markdown_document_labeled,
    transcription_engine::{inspect_model, load_wav_16k_mono, LocalWhisperEngine, TranscriptionEngine},
    transcription_types::TranscriptionRequest,
};
use std::{fs, path::PathBuf, sync::atomic::AtomicBool, time::Instant};

const MAX_SECONDS: u64 = 30 * 60;

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

#[test]
#[ignore]
fn real_end_to_end_speakers_pipeline() {
    let Ok(_) = std::env::var("SPEAKERS_E2E") else {
        eprintln!(
            "skipping: set SPEAKERS_E2E=1 and the SPEAKERS_E2E_* variables documented at the \
             top of speakers_end_to_end_tests.rs to run the full real pipeline"
        );
        return;
    };
    let whisper_model = env_path("SPEAKERS_E2E_WHISPER_MODEL");
    let segmentation_model = env_path("SPEAKERS_E2E_SEGMENTATION_MODEL");
    let embedding_model = env_path("SPEAKERS_E2E_EMBEDDING_MODEL");
    let recording_dir = env_path("SPEAKERS_E2E_RECORDING_DIR");
    let output_dir = env_path("SPEAKERS_E2E_OUTPUT_DIR");
    fs::create_dir_all(&output_dir).expect("output dir must be creatable");
    let local_name = std::env::var("SPEAKERS_E2E_LOCAL_NAME").unwrap_or_else(|_| "You".into());
    let ollama_model = std::env::var("SPEAKERS_E2E_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
    let ollama_url = std::env::var("SPEAKERS_E2E_OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    // A plain file copy loses the recorder's own metadata, so real
    // started_at/duration_ms (rather than the 1970/00:00:00 this harness used
    // to hardcode into every rendered header) can optionally be supplied from
    // the source recording's own sqlite row or its previously-rendered
    // transcript.md header.
    let started_at: u64 = std::env::var("SPEAKERS_E2E_STARTED_AT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let duration_ms: u64 = std::env::var("SPEAKERS_E2E_DURATION_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut wavs: Vec<PathBuf> = fs::read_dir(&recording_dir)
        .expect("SPEAKERS_E2E_RECORDING_DIR must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|x| x.to_str()) == Some("wav")
                && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("track-"))
        })
        .collect();
    wavs.sort();
    assert_eq!(wavs.len(), 2, "expected exactly two track-*.wav files in {}", recording_dir.display());

    let (input_wav, output_wav) = match (
        std::env::var("SPEAKERS_E2E_INPUT_TRACK").ok(),
        std::env::var("SPEAKERS_E2E_OUTPUT_TRACK").ok(),
    ) {
        (Some(i), Some(o)) => (recording_dir.join(i), recording_dir.join(o)),
        // No reliable metadata survives a plain file copy, so this is a
        // best-effort guess for the harness only, not production logic: the
        // app always knows real track direction from `Recording.tracks`.
        _ => (wavs[0].clone(), wavs[1].clone()),
    };
    println!("Guessed input (mic) track: {}", input_wav.display());
    println!("Guessed output (loopback) track: {}", output_wav.display());

    // `Recording.recording_id` must be a valid UUID (see `validated_recording_tracks`):
    // reuse the recording directory's own name when it already is one (as our
    // copied scratch recordings are named), otherwise mint a fresh one.
    let recording_id = recording_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| uuid::Uuid::parse_str(n).ok())
        .unwrap_or_else(uuid::Uuid::new_v4)
        .to_string();

    let recording = Recording {
        recording_id: recording_id.clone(),
        started_at,
        ended_at: Some(started_at.saturating_add(duration_ms)),
        duration_ms,
        status: "recorded".into(),
        meeting_title: None,
        storage_path: None,
        auto_transcribe: true,
        error: None,
        transcription_status: "completed".into(),
        appflowy_status: "idle".into(),
        tracks: vec![
            Track {
                source_id: "mic".into(),
                name: "Microphone".into(),
                direction: "input".into(),
                sample_rate: 48_000,
                channels: 2,
                file_path: input_wav.to_string_lossy().into(),
                frames_written: 1,
                dropped_samples: 0,
                error: None,
            },
            Track {
                source_id: "loopback".into(),
                name: "Speakers (loopback)".into(),
                direction: "output".into(),
                sample_rate: 48_000,
                channels: 2,
                file_path: output_wav.to_string_lossy().into(),
                frames_written: 1,
                dropped_samples: 0,
                error: None,
            },
        ],
    };

    // --- 1. Transcription: turbo Whisper + AEC + echo-dedup ---
    let info = inspect_model(&whisper_model).expect("whisper model must be valid");
    let request = TranscriptionRequest {
        recording: recording.clone(),
        recordings_root: recording_dir.parent().unwrap_or(&recording_dir).to_owned(),
        model_path: whisper_model,
        model_key: info.key,
        model_hash: info.sha256,
        language: None,
        echo_cancellation: true,
        dedupe_echo_segments: true,
        local_speaker_name: local_name.clone(),
    };
    let engine = LocalWhisperEngine::new();
    let started = Instant::now();
    let transcript = engine
        .transcribe(&request, &AtomicBool::new(false), &mut |_| {})
        .expect("transcription must succeed");
    println!(
        "Transcription: backend={} | {} segments | {:.1}s wall time",
        engine.backend_label(),
        transcript.segments.len(),
        started.elapsed().as_secs_f64()
    );

    // --- 2. Speaker identification: real sherpa-onnx on the loopback track only ---
    let samples = load_wav_16k_mono(&output_wav, MAX_SECONDS).expect("loopback track must decode");
    let diarizer = create_diarizer(&segmentation_model, &embedding_model).expect("real models must initialize");
    let diar_started = Instant::now();
    let result = diarizer.process(&samples).expect("native diarization must produce a result");
    let diar_elapsed = diar_started.elapsed();
    let segments = result.sort_by_start_time();
    println!(
        "Speaker identification: {:.1}s audio in {:.2}s ({:.3}x real time) | {} speakers | {} segments",
        samples.len() as f64 / 16_000.0,
        diar_elapsed.as_secs_f64(),
        diar_elapsed.as_secs_f64() / (samples.len() as f64 / 16_000.0).max(0.001),
        segments.iter().map(|s| s.speaker).collect::<std::collections::BTreeSet<_>>().len(),
        segments.len(),
    );
    let turns: Vec<SpeakerTurn> = segments
        .iter()
        .map(|s| SpeakerTurn {
            source_id: "loopback".into(),
            start_ms: (s.start as f64 * 1000.0).round() as u64,
            end_ms: (s.end as f64 * 1000.0).round() as u64,
            speaker: s.speaker as u32,
            confidence: s.confidence.is_finite().then_some(s.confidence).filter(|c| (0.0..=1.0).contains(c)),
        })
        .collect();
    let diarization_output = DiarizationOutput {
        recording_id: recording_id.clone(),
        turns,
        warnings: vec![],
    };
    let labels = align_speakers(&transcript, &diarization_output).expect("alignment must succeed");

    // --- 3. Render transcript.md exactly as the app would (with speaker labels) ---
    let mut labeled_transcript = transcript.clone();
    labeled_transcript.local_speaker_name = Some(local_name.clone());
    let transcript_md = markdown_document_labeled(&recording, &labeled_transcript, Some(&labels));
    let transcript_path = output_dir.join("transcript.md");
    fs::write(&transcript_path, &transcript_md).expect("must write transcript.md");
    println!("Wrote {}", transcript_path.display());

    // --- 4. Summary: real local Ollama, using the new You/Speaker N labels ---
    let summary_engine = OllamaSummaryEngine::new();
    summary_engine
        .test_connection(&ollama_url)
        .expect("Ollama must be reachable for this real-run test");
    let summary_request = SummaryRequest {
        recording: recording.clone(),
        transcript: labeled_transcript,
        ollama_url,
        model: ollama_model.clone(),
        local_speaker_name: local_name,
        remote_speaker_labels: Some(labels),
    };
    let summary_started = Instant::now();
    let mut progress_calls = Vec::new();
    let output = summary_engine
        .summarize(&summary_request, &AtomicBool::new(false), &mut |p| progress_calls.push(p))
        .expect("real Ollama summarization must succeed");
    println!(
        "Summary: {:.1}s wall time | title: {}",
        summary_started.elapsed().as_secs_f64(),
        output.title
    );
    let checkpoint = SummaryCheckpoint {
        recording_id,
        parent_checksum: "e2e-harness".into(),
        model: ollama_model,
        output,
    };
    let summary_md = crate::summary::summary_markdown(&recording, &checkpoint);
    let summary_path = output_dir.join("summary.md");
    fs::write(&summary_path, &summary_md).expect("must write summary.md");
    println!("Wrote {}", summary_path.display());

    println!("\n### transcript.md ###\n{transcript_md}\n### end transcript.md ###");
    println!("\n### summary.md ###\n{summary_md}\n### end summary.md ###");
}
