//! Opt-in integration coverage confirming that transcribing a FLAC copy of a
//! track (as `audio_compress.rs` would leave it after compression) produces
//! the exact same transcript text as transcribing the original WAV. Mirrors
//! `transcription_engine::tests::transcribes_explicit_public_fixture`'s
//! pattern: reads only the model and public fixture explicitly named by the
//! caller, no discovery or download.
use crate::{
    audio_file,
    recording::{Recording, Track},
    transcription_engine::{inspect_model, LocalWhisperEngine, TranscriptionEngine},
    transcription_types::TranscriptionRequest,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "meeting-recorder-flac-parity-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn request(
    root: &Path,
    recording_id: String,
    track_path: &Path,
    model_path: PathBuf,
    model_key: String,
    model_hash: String,
) -> TranscriptionRequest {
    TranscriptionRequest {
        recording: Recording {
            recording_id,
            started_at: 0,
            ended_at: Some(0),
            duration_ms: 0,
            status: "complete".into(),
            tracks: vec![Track {
                source_id: "public_fixture".into(),
                name: "Public fixture".into(),
                direction: "input".into(),
                sample_rate: 16_000,
                channels: 1,
                file_path: track_path.to_string_lossy().into(),
                frames_written: 0,
                dropped_samples: 0,
                error: None,
            }],
            error: None,
            meeting_title: None,
            storage_path: None,
            library_root: None,
            auto_transcribe: false,
            transcription_status: "idle".into(),
            appflowy_status: "idle".into(),
        },
        recordings_root: root.to_path_buf(),
        extra_library_roots: Vec::new(),
        model_path,
        model_key,
        model_hash,
        language: None,
        echo_cancellation: false,
        dedupe_echo_segments: false,
        local_speaker_name: "You".into(),
    }
}

#[test]
#[ignore = "set WHISPER_TEST_MODEL and WHISPER_TEST_AUDIO to explicit public fixture paths"]
fn flac_and_wav_produce_identical_transcripts_with_a_real_model() {
    let model_path =
        std::env::var("WHISPER_TEST_MODEL").expect("Explicitly select a local test model");
    let audio_path =
        std::env::var("WHISPER_TEST_AUDIO").expect("Explicitly select public speech audio");
    let info = inspect_model(Path::new(&model_path)).expect("explicit model must be valid");

    let wav_id = uuid::Uuid::new_v4().to_string();
    let wav_root = temp_dir("wav");
    let wav_dir = wav_root.join(&wav_id);
    fs::create_dir_all(&wav_dir).unwrap();
    let wav_path = wav_dir.join("public.wav");
    fs::copy(&audio_path, &wav_path).expect("explicit public audio must be readable");

    let flac_id = uuid::Uuid::new_v4().to_string();
    let flac_root = temp_dir("flac");
    let flac_dir = flac_root.join(&flac_id);
    fs::create_dir_all(&flac_dir).unwrap();
    let (wav_info, samples) = audio_file::read_wav_pcm16(&wav_path).unwrap();
    let flac_path = flac_dir.join("public.flac");
    audio_file::encode_flac_new(
        &flac_path,
        wav_info.sample_rate,
        wav_info.channels,
        &samples,
    )
    .unwrap();

    let cancelled = AtomicBool::new(false);
    let engine = LocalWhisperEngine::new();
    let wav_transcript = engine
        .transcribe(
            &request(
                &wav_root,
                wav_id,
                &wav_path,
                PathBuf::from(&model_path),
                info.key.clone(),
                info.sha256.clone(),
            ),
            &cancelled,
            &mut |_| {},
        )
        .expect("WAV fixture must transcribe locally");
    let flac_transcript = engine
        .transcribe(
            &request(
                &flac_root,
                flac_id,
                &flac_path,
                PathBuf::from(&model_path),
                info.key.clone(),
                info.sha256.clone(),
            ),
            &cancelled,
            &mut |_| {},
        )
        .expect("FLAC fixture must transcribe locally");

    let wav_text: Vec<&str> = wav_transcript
        .segments
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    let flac_text: Vec<&str> = flac_transcript
        .segments
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    assert!(
        !wav_text.is_empty(),
        "Public speech fixture must yield text"
    );
    assert_eq!(
        wav_text, flac_text,
        "Transcribing a FLAC copy must produce the exact same text as the original WAV"
    );
    println!(
        "WAV and FLAC transcripts matched exactly: {} segment(s)",
        wav_text.len()
    );
}
