//! Real, end-to-end verification of the adaptive residual-echo gate
//! (`residual_echo_gate.rs`) against real two-track recordings, run through
//! the exact production path (`LocalWhisperEngine::transcribe`, with
//! acoustic echo cancellation and text-level dedup both enabled) rather than
//! any synthetic fixture. Not part of the default suite: `#[ignore]`d and
//! env-gated, since it depends on a real Whisper model file and private
//! recording audio neither of which this repository ships.
//!
//! Run explicitly, e.g.:
//!   RESIDUAL_ECHO_REAL_MODEL="<path to a ggml Whisper model, e.g. turbo>" \
//!   RESIDUAL_ECHO_REAL_RECORDINGS_DIR="<dir of recording-id subfolders, each containing track-00.wav/track-01.wav>" \
//!     cargo test --release --features cuda --manifest-path src-tauri/Cargo.toml --locked \
//!       -- --ignored --nocapture real_recordings_residual_echo_gate_eval
//!
//! Each subfolder's name must be a valid UUID (used directly as the
//! recording id) and contain exactly two WAV tracks. Track direction
//! (microphone/input vs loopback/output) is not recoverable from the WAV
//! files alone, so this test assumes the convention this recorder always
//! used when these fixtures were captured: `track-00.wav` is the microphone
//! (input) and `track-01.wav` is the loopback (output). If a recording's
//! printed mic lines look like the remote side instead, that assumption is
//! wrong for that fixture and the two tracks should be swapped.
use crate::echo_cancel;
use crate::recording::{Recording, Track};
use crate::residual_echo_gate;
use crate::transcription_engine::{
    inspect_model, load_wav_16k_mono, LocalWhisperEngine, TranscriptionEngine,
};
use crate::transcription_types::TranscriptionRequest;
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

/// Diagnostic (not an acceptance check): dumps the AEC block trace for one
/// recording's very start, to explain why a specific mic segment there was or
/// was not gated. Run directly against a single recording folder with
/// RESIDUAL_ECHO_REAL_DEBUG_DIR set to it.
#[test]
#[ignore]
fn real_recording_debug_block_trace() {
    let Ok(dir) = std::env::var("RESIDUAL_ECHO_REAL_DEBUG_DIR") else {
        eprintln!("skipping: set RESIDUAL_ECHO_REAL_DEBUG_DIR to one recording folder");
        return;
    };
    let dir = PathBuf::from(dir);
    let mic = load_wav_16k_mono(&dir.join("track-00.wav"), 3600).expect("mic decode");
    let reference = load_wav_16k_mono(&dir.join("track-01.wav"), 3600).expect("reference decode");
    let (_, stats) = echo_cancel::cancel_echo(&mic, &reference, 16_000);
    println!(
        "applied={} delay_ms={:?} erle_db={:?} blocks={}",
        stats.applied,
        stats.estimated_delay_ms,
        stats.erle_db,
        stats.blocks.len()
    );
    let coupling = residual_echo_gate::estimate_echo_coupling(&stats.blocks);
    let near_end = residual_echo_gate::derive_near_end_level_rms(&stats.blocks);
    println!("coupling={coupling:?} near_end_level_rms={near_end}");
    for block in stats.blocks.iter().take(45) {
        println!(
            "  start={:>7} len={:>5} far_end_active={:>5} far_end_only={:>5} mic_rms={:.5} residual_rms={:.5} reference_rms={:.5}",
            block.start_sample,
            block.len_samples,
            block.far_end_active,
            block.far_end_only,
            block.mic_rms,
            block.residual_rms,
            block.reference_rms,
        );
    }
    let region_gated =
        residual_echo_gate::is_residual_echo_region(&stats.blocks, 0, 13_440, near_end, coupling);
    let tiny = residual_echo_gate::is_tiny_far_end_fragment(&stats.blocks, 0, 13_440, near_end, 1);
    println!("region[0,13440) gated={region_gated} tiny_fragment={tiny}");
}

#[test]
#[ignore]
fn real_recordings_residual_echo_gate_eval() {
    let Ok(model_path) = std::env::var("RESIDUAL_ECHO_REAL_MODEL") else {
        eprintln!(
            "skipping: set RESIDUAL_ECHO_REAL_MODEL and RESIDUAL_ECHO_REAL_RECORDINGS_DIR to run \
             this against a real Whisper model and real recordings"
        );
        return;
    };
    let recordings_dir = std::env::var("RESIDUAL_ECHO_REAL_RECORDINGS_DIR")
        .expect("set RESIDUAL_ECHO_REAL_RECORDINGS_DIR to a directory of recording-id subfolders");
    let model_path = PathBuf::from(model_path);
    let model = inspect_model(&model_path).expect("model must be a valid ggml Whisper file");
    println!(
        "\n=== Real residual-echo-gate evaluation (model: {}) ===",
        model.key
    );

    let mut entries: Vec<_> = fs::read_dir(&recordings_dir)
        .expect("RESIDUAL_ECHO_REAL_RECORDINGS_DIR must exist")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    assert!(
        !entries.is_empty(),
        "no recording subfolders found under {recordings_dir}"
    );

    let engine = LocalWhisperEngine::new();
    let recordings_root = PathBuf::from(&recordings_dir);
    let cancelled = AtomicBool::new(false);

    for entry in &entries {
        let folder = entry.path();
        let recording_id = folder.file_name().unwrap().to_string_lossy().to_string();
        if uuid::Uuid::parse_str(&recording_id).is_err() {
            println!("--- {recording_id}: not a UUID folder name, skipping ---");
            continue;
        }
        let track_00 = folder.join("track-00.wav");
        let track_01 = folder.join("track-01.wav");
        if !track_00.is_file() || !track_01.is_file() {
            println!("--- {recording_id}: missing track-00.wav/track-01.wav, skipping ---");
            continue;
        }
        let recording = Recording {
            recording_id: recording_id.clone(),
            started_at: 0,
            ended_at: Some(0),
            duration_ms: 0,
            status: "recorded".into(),
            tracks: vec![
                Track {
                    source_id: "mic:input".into(),
                    name: "Microphone".into(),
                    direction: "input".into(),
                    sample_rate: 16_000,
                    channels: 1,
                    file_path: track_00.to_string_lossy().into_owned(),
                    frames_written: 1,
                    dropped_samples: 0,
                    error: None,
                },
                Track {
                    source_id: "speakers:output".into(),
                    name: "Speakers".into(),
                    direction: "output".into(),
                    sample_rate: 16_000,
                    channels: 1,
                    file_path: track_01.to_string_lossy().into_owned(),
                    frames_written: 1,
                    dropped_samples: 0,
                    error: None,
                },
            ],
            error: None,
            meeting_title: None,
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            transcription_status: "pending".into(),
            appflowy_status: "none".into(),
        };
        let request = TranscriptionRequest {
            recording,
            recordings_root: recordings_root.clone(),
            extra_library_roots: Vec::new(),
            model_path: model_path.clone(),
            model_key: model.key.clone(),
            model_hash: model.sha256.clone(),
            language: None,
            echo_cancellation: true,
            dedupe_echo_segments: true,
            local_speaker_name: "You".into(),
        };
        let started = std::time::Instant::now();
        let result = engine.transcribe(&request, &cancelled, &mut |_p| {});
        let elapsed = started.elapsed();
        match result {
            Ok(transcript) => {
                println!(
                    "\n--- {recording_id} ({} segments in {:.1}s, backend {}) ---",
                    transcript.segments.len(),
                    elapsed.as_secs_f64(),
                    engine.backend_label(),
                );
                if !transcript.warnings.is_empty() {
                    println!("  warnings: {:?}", transcript.warnings);
                }
                let mut mic_lines = 0;
                for segment in &transcript.segments {
                    let is_mic = segment.source_id == "mic:input";
                    if is_mic {
                        mic_lines += 1;
                    }
                    let who = if is_mic { "You" } else { "Remote" };
                    println!(
                        "    [{:>8}-{:>8}ms] {who}: {}",
                        segment.start_ms, segment.end_ms, segment.text
                    );
                }
                println!("  mic (You) lines kept: {mic_lines}");
            }
            Err(error) => {
                println!("--- {recording_id}: transcription FAILED: {error} ---");
            }
        }
    }
}
