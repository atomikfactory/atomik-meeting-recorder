//! Real, native verification of the "name speakers once" voice-profile
//! feature: named speaker centroids built from one real meeting, matched
//! against two more recordings from the same series and one unrelated
//! recording (a false-positive check). Not part of the default suite:
//! `#[ignore]`d and env-gated, exactly like `diarization_real_tests.rs`, so
//! CI and normal `cargo test` runs never depend on locally-imported model
//! files or private recording audio. No recording audio or transcript is
//! committed -- only the four recording IDs the user identified as being
//! from the same "Sprint Ceremonies" series (three) plus one unrelated
//! French recording (the false-positive check), read from local paths given
//! entirely through environment variables.
//!
//! Run explicitly, e.g.:
//!   VOICE_PROFILES_REAL=1 \
//!   VOICE_PROFILES_REAL_SEGMENTATION_MODEL="<path to pyannote segmentation model.onnx>" \
//!   VOICE_PROFILES_REAL_EMBEDDING_MODEL="<path to nemo_en_titanet_small.onnx>" \
//!   VOICE_PROFILES_REAL_RECORDINGS_METADATA_ROOT="<app data recordings folder containing recordings.sqlite3>" \
//!   VOICE_PROFILES_REAL_AUDIO_DIR="<dir of subfolders named by recording id, each containing that recording's track-*.wav files>" \
//!     cargo test --release --manifest-path src-tauri/Cargo.toml --locked -- --ignored real_voice_profiles
//!
//! Reads the raw `recordings` row for each ID directly (bypassing the normal
//! tombstone filter): a recording used for this kind of one-off local
//! verification may well have already been removed from the visible library
//! while its metadata and audio remain on disk for inspection.
use crate::{
    diarization_engine::create_diarizer,
    transcription_engine::load_wav_16k_mono,
    voice_profiles::{
        candidate_scores, speaker_centroid, ClusterCentroid, EmbeddingExtractor,
        SherpaEmbeddingExtractor, SpeakerSamples, MIN_TURN_SECONDS,
    },
};
use std::{collections::BTreeMap, path::PathBuf};

const MAX_SOURCE_SECONDS: u64 = 30 * 60;
/// Same three "Sprint Ceremonies" recordings the user identified, plus one
/// unrelated French recording used only as a false-positive check.
const NAMING_RECORDING: &str = "d3863b42-4868-4283-b89b-14a8fe24a313";
const OTHER_RECORDINGS: [&str; 3] = [
    "f873f721-a299-4cce-912e-4af478d829ad",
    "3c9b5271-dccd-4a9e-8065-07d6ba4896d5",
    "cd574cf7-0d36-4471-8748-53062196dfe6",
];
/// Placeholder names for the naming recording's detected speakers, in the
/// order sherpa-onnx's clustering assigns speaker numbers (1-indexed).
const PLACEHOLDER_NAMES: [&str; 3] = ["Alice", "Bob", "Carol"];

#[cfg(windows)]
fn ensure_sherpa_dlls_beside_test_binary() {
    let exe = std::env::current_exe().expect("current_exe must resolve");
    let exe_dir = exe.parent().expect("exe has a parent directory");
    let profile_dir = exe_dir.parent().expect("deps/ has a parent directory");
    let Ok(entries) = std::fs::read_dir(profile_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let dest = exe_dir.join(path.file_name().unwrap());
        if !dest.exists() {
            let _ = std::fs::copy(&path, &dest);
        }
    }
}
#[cfg(not(windows))]
fn ensure_sherpa_dlls_beside_test_binary() {}

/// Diarizes one recording's loopback track(s) and returns, for each detected
/// speaker number, its usable turns' audio (see `SpeakerSamples`).
fn diarize_recording(
    metadata_root: &std::path::Path,
    audio_dir: &std::path::Path,
    recording_id: &str,
    segmentation_model: &std::path::Path,
    embedding_model: &std::path::Path,
) -> Vec<SpeakerSamples> {
    // Reads the raw row directly rather than through `RecordingStore::get_by_id`,
    // which hides tombstoned (soft-deleted-from-the-library) recordings -- these
    // verification recordings may well have been cleaned up from the visible
    // library already while their metadata/audio still exist for this
    // read-only check.
    let connection = rusqlite::Connection::open(metadata_root.join("recordings.sqlite3"))
        .unwrap_or_else(|e| panic!("could not open recordings metadata at {metadata_root:?}: {e}"));
    let payload: String = connection
        .query_row(
            "SELECT payload FROM recordings WHERE recording_id=?1",
            [recording_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("recording {recording_id} is not in this metadata store: {e}"));
    let recording: crate::recording::Recording = serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("recording {recording_id} metadata did not parse: {e}"));
    let output_tracks: Vec<_> = recording
        .tracks
        .iter()
        .filter(|t| t.direction == "output")
        .collect();
    assert!(
        !output_tracks.is_empty(),
        "recording {recording_id} has no output (loopback) track"
    );
    let mut samples = Vec::new();
    for track in output_tracks {
        let file_name = std::path::Path::new(&track.file_path)
            .file_name()
            .unwrap_or_else(|| panic!("track has no file name: {}", track.file_path));
        let path = audio_dir.join(recording_id).join(file_name);
        assert!(path.is_file(), "expected audio file at {path:?}");
        let track_samples = load_wav_16k_mono(&path, MAX_SOURCE_SECONDS)
            .unwrap_or_else(|e| panic!("could not decode {path:?}: {e}"));
        if track_samples.is_empty() {
            continue;
        }
        let diarizer = create_diarizer(segmentation_model, embedding_model)
            .expect("real models must initialize a diarizer");
        let result = diarizer
            .process(&track_samples)
            .unwrap_or_else(|| panic!("native diarization returned nothing for {path:?}"));
        for segment in result.sort_by_start_time() {
            let start_sample = (segment.start.max(0.0) * 16_000.0) as usize;
            let end_sample = ((segment.end.max(0.0) * 16_000.0) as usize).min(track_samples.len());
            if end_sample <= start_sample {
                continue;
            }
            let seconds = segment.end as f64 - segment.start as f64;
            if seconds < MIN_TURN_SECONDS {
                continue;
            }
            samples.push(SpeakerSamples {
                speaker: segment.speaker as u32,
                seconds,
                samples: track_samples[start_sample..end_sample].to_vec(),
            });
        }
    }
    samples
}

#[test]
#[ignore]
fn real_voice_profiles_identify_known_speakers_across_meetings() {
    ensure_sherpa_dlls_beside_test_binary();
    let Ok(_) = std::env::var("VOICE_PROFILES_REAL") else {
        eprintln!(
            "skipping: set VOICE_PROFILES_REAL=1, VOICE_PROFILES_REAL_SEGMENTATION_MODEL, \
             VOICE_PROFILES_REAL_EMBEDDING_MODEL, VOICE_PROFILES_REAL_RECORDINGS_METADATA_ROOT, \
             and VOICE_PROFILES_REAL_AUDIO_DIR to run this against real sherpa-onnx models and recordings"
        );
        return;
    };
    let segmentation_model = PathBuf::from(
        std::env::var("VOICE_PROFILES_REAL_SEGMENTATION_MODEL")
            .expect("set VOICE_PROFILES_REAL_SEGMENTATION_MODEL"),
    );
    let embedding_model = PathBuf::from(
        std::env::var("VOICE_PROFILES_REAL_EMBEDDING_MODEL")
            .expect("set VOICE_PROFILES_REAL_EMBEDDING_MODEL"),
    );
    let metadata_root = PathBuf::from(
        std::env::var("VOICE_PROFILES_REAL_RECORDINGS_METADATA_ROOT")
            .expect("set VOICE_PROFILES_REAL_RECORDINGS_METADATA_ROOT"),
    );
    let audio_dir = PathBuf::from(
        std::env::var("VOICE_PROFILES_REAL_AUDIO_DIR").expect("set VOICE_PROFILES_REAL_AUDIO_DIR"),
    );

    println!("\n=== Real voice-profile identification verification ===");
    println!("--- Naming pass: {NAMING_RECORDING} ---");
    let naming_samples = diarize_recording(
        &metadata_root,
        &audio_dir,
        NAMING_RECORDING,
        &segmentation_model,
        &embedding_model,
    );
    let mut speakers: Vec<u32> = naming_samples.iter().map(|s| s.speaker).collect();
    speakers.sort_unstable();
    speakers.dedup();
    println!("detected {} speaker(s): {:?}", speakers.len(), speakers);

    let extractor = SherpaEmbeddingExtractor::create(&embedding_model, "real-verification")
        .expect("embedding extractor must initialize");
    let mut profiles = Vec::new();
    for (index, speaker) in speakers.iter().enumerate() {
        let name = PLACEHOLDER_NAMES
            .get(index)
            .copied()
            .unwrap_or("Unnamed");
        match speaker_centroid(&extractor, &naming_samples, *speaker) {
            Ok((embedding, total_seconds)) => {
                println!(
                    "  speaker {speaker} -> \"{name}\" ({total_seconds:.1}s of usable speech)"
                );
                profiles.push((
                    name.to_string(),
                    crate::voice_profiles::VoiceProfile {
                        id: format!("test-{speaker}"),
                        name: name.to_string(),
                        embedding,
                        model_hash: extractor.model_hash(),
                        sample_seconds: total_seconds,
                        created_at: 0,
                        updated_at: 0,
                    },
                ));
            }
            Err(error) => {
                println!("  speaker {speaker}: {error}");
            }
        }
    }
    assert!(
        !profiles.is_empty(),
        "the naming recording produced no usable voice profile; cannot continue"
    );
    let profile_list: Vec<_> = profiles.iter().map(|(_, p)| p.clone()).collect();

    println!("\n--- Recognition pass ---");
    println!("recording,speaker,best_profile,best_score,second_best_score");
    for recording_id in OTHER_RECORDINGS {
        let samples = diarize_recording(
            &metadata_root,
            &audio_dir,
            recording_id,
            &segmentation_model,
            &embedding_model,
        );
        let mut speakers: Vec<u32> = samples.iter().map(|s| s.speaker).collect();
        speakers.sort_unstable();
        speakers.dedup();
        let mut clusters: Vec<ClusterCentroid> = Vec::new();
        for speaker in speakers {
            if let Ok((embedding, _seconds)) = speaker_centroid(&extractor, &samples, speaker) {
                clusters.push(ClusterCentroid { speaker, embedding });
            }
        }
        let scored = candidate_scores(&clusters, &profile_list);
        let mut by_speaker: BTreeMap<u32, &crate::voice_profiles::MatchCandidate> = BTreeMap::new();
        for candidate in &scored {
            by_speaker.insert(candidate.speaker, candidate);
        }
        for (speaker, candidate) in &by_speaker {
            println!(
                "{recording_id},{speaker},{},{:.4},{:.4}",
                candidate.profile_name, candidate.best_score, candidate.second_best_score
            );
        }
        if recording_id == "cd574cf7-0d36-4471-8748-53062196dfe6" {
            for candidate in &scored {
                if candidate.best_score >= crate::voice_profiles::DEFAULT_MATCH_THRESHOLD
                    && candidate.best_score - candidate.second_best_score
                        >= crate::voice_profiles::DEFAULT_MATCH_MARGIN
                {
                    println!(
                        "    NOTE: the unrelated French recording scored a passing match against \"{}\" \
                         ({:.3}) -- treat this as a false positive when tuning the default threshold/margin.",
                        candidate.profile_name, candidate.best_score
                    );
                }
            }
        }
    }
}
