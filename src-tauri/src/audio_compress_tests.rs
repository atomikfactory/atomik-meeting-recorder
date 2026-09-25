use super::*;
use crate::recording::Track;
use std::sync::atomic::AtomicBool;

struct Fixture {
    root: PathBuf,
    store: CompressionStore,
    recordings: RecordingStore,
    dir: PathBuf,
    recording: Recording,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "meeting-audio-compress-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = CompressionStore::open(&root.join("compression.sqlite3")).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let wav_path = dir.join("track-00.wav");
        write_wav(&wav_path, 8_000, 1, &test_samples(4_000));
        let recording = Recording {
            recording_id: id.clone(),
            started_at: 0,
            ended_at: Some(500),
            duration_ms: 500,
            status: "recorded".into(),
            meeting_title: None,
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
                sample_rate: 8_000,
                channels: 1,
                file_path: wav_path.display().to_string(),
                frames_written: 4_000,
                dropped_samples: 0,
                error: None,
            }],
        };
        recordings.save(&recording).unwrap();
        Self {
            root,
            store,
            recordings,
            dir,
            recording,
        }
    }
    /// Marks transcription completed (so the recording becomes eligible) and
    /// leaves diarization untouched (no job -> "disabled/never needed").
    fn mark_transcription_completed(&self) {
        let registry =
            TranscriptionStore::open(&self.root.join("transcription.sqlite3")).unwrap();
        registry
            .enqueue(&self.recording.recording_id, "base", &"a".repeat(64))
            .unwrap();
        assert!(registry
            .claim(&self.recording.recording_id, "unused-model-path", &"a".repeat(64))
            .unwrap());
        let json = self.dir.join("transcript.json");
        fs::write(&json, b"{}").unwrap();
        registry
            .complete(
                &self.recording.recording_id,
                &json.display().to_string(),
                &self.dir.join("transcript.md").display().to_string(),
                &"b".repeat(64),
            )
            .unwrap();
    }
    fn track_path(&self) -> PathBuf {
        PathBuf::from(&self.recordings
            .get_by_id(&self.recording.recording_id)
            .unwrap()
            .unwrap()
            .tracks[0]
            .file_path)
    }
}
fn test_samples(len: usize) -> Vec<i16> {
    (0..len)
        .map(|i| ((i as f32 * 0.07).sin() * 16_000.0) as i16)
        .collect()
}
fn write_wav(path: &Path, sample_rate: u32, channels: u16, samples: &[i16]) {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for sample in samples {
        writer.write_sample(*sample).unwrap();
    }
    writer.finalize().unwrap();
}
fn cleanup(fixture: &Fixture) {
    let _ = fs::remove_dir_all(&fixture.root);
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn ineligible_recording_is_not_enqueued() {
    let f = Fixture::new();
    // Transcription never marked completed.
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    assert!(f.store.all().unwrap().is_empty());
    assert!(f.track_path().extension().unwrap() == "wav");
    cleanup(&f);
}

#[test]
fn pending_diarization_blocks_compression() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let speakers = DiarizationStore::open(&f.root.join("diarization.sqlite3")).unwrap();
    speakers
        .enqueue(&f.recording.recording_id, &"b".repeat(64))
        .unwrap();
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    assert!(f.store.all().unwrap().is_empty());
    cleanup(&f);
}

#[test]
fn completed_diarization_unblocks_compression() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let speakers = DiarizationStore::open(&f.root.join("diarization.sqlite3")).unwrap();
    speakers
        .enqueue(&f.recording.recording_id, &"b".repeat(64))
        .unwrap();
    let models = crate::diarization_store::ModelPair {
        segmentation_path: "seg".into(),
        segmentation_hash: "a".repeat(64),
        embedding_path: "emb".into(),
        embedding_hash: "b".repeat(64),
    };
    assert!(speakers
        .claim(&f.recording.recording_id, &models)
        .unwrap());
    speakers
        .complete(&f.recording.recording_id, &"c".repeat(64), 1)
        .unwrap();
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    let job = f.store.get(&f.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "completed");
    cleanup(&f);
}

#[test]
fn end_to_end_compresses_verifies_and_swaps_metadata() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    let job = f.store.get(&f.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "completed");
    assert!(job.bytes_after.unwrap() > 0);
    assert!(job.bytes_after.unwrap() <= job.bytes_before.unwrap());
    let track_path = f.track_path();
    assert_eq!(track_path.extension().unwrap(), "flac");
    assert!(track_path.exists());
    assert!(!track_path.with_extension("wav").exists());
    let (_, decoded) = audio_file::read_flac_pcm16(&track_path).unwrap();
    assert_eq!(decoded, test_samples(4_000));
    cleanup(&f);
}

#[test]
fn already_compressed_track_is_skipped_idempotently() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    // Re-running compress_recording directly (as a resumed/crashed job would)
    // must be a no-op that neither errors nor touches the FLAC again.
    let recording = f.recordings.get_by_id(&f.recording.recording_id).unwrap().unwrap();
    let (before, after) = compress_recording(&f.root, &[], recording, &f.recordings, &cancel()).unwrap();
    assert!(after > 0 && after <= before);
    cleanup(&f);
}

#[test]
fn existing_flac_without_metadata_update_is_left_untouched() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    // Simulate a stray .flac left by something else at the target path.
    let flac_path = f.dir.join("track-00.flac");
    fs::write(&flac_path, b"not a real flac").unwrap();
    tick(&f.root, &[], &f.store, &f.recordings, &cancel()).unwrap();
    let job = f.store.get(&f.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.unwrap().contains("without updated metadata"));
    // The original WAV must still be present and referenced.
    assert!(f.track_path().extension().unwrap() == "wav");
    assert!(f.track_path().exists());
    cleanup(&f);
}

#[test]
fn cancellation_mid_job_requeues_without_partial_loss() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let recording = f.recordings.get_by_id(&f.recording.recording_id).unwrap().unwrap();
    let cancelled = AtomicBool::new(true);
    let result = compress_recording(&f.root, &[], recording, &f.recordings, &cancelled);
    assert!(result.is_err());
    // Nothing was swapped: the WAV must still be the referenced, existing file.
    assert!(f.track_path().extension().unwrap() == "wav");
    assert!(f.track_path().exists());
    cleanup(&f);
}

#[test]
fn disabled_setting_is_reflected_in_state_without_enqueueing() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let settings: SettingsReader = Arc::new(|| Ok(CompressionSettings { enabled: false, extra_library_roots: Vec::new() }));
    let service =
        CompressionService::with_inhibition(f.root.clone(), settings, true);
    let state = service.state();
    assert!(!state.enabled);
    assert!(state.jobs.is_empty());
    cleanup(&f);
}

#[test]
fn recovery_removes_orphan_temp_files() {
    let f = Fixture::new();
    let orphan = f.dir.join("track-00.flac.tmp-deadbeef");
    fs::write(&orphan, b"partial").unwrap();
    recover_tracks(&f.root, &[], &f.recordings).unwrap();
    assert!(!orphan.exists());
    // Untouched otherwise.
    assert!(f.track_path().exists());
    cleanup(&f);
}

#[test]
fn recovery_finishes_an_interrupted_swap_when_both_files_are_present() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    // Manually reach the "verified rename happened, metadata/delete did not"
    // state that a crash between steps 3 and 5 would leave.
    let (info, samples) = audio_file::read_wav_pcm16(&f.track_path()).unwrap();
    let flac_path = f.dir.join("track-00.flac");
    audio_file::encode_flac_new(&flac_path, info.sample_rate, info.channels, &samples).unwrap();
    assert!(f.track_path().exists()); // WAV still present too
    recover_tracks(&f.root, &[], &f.recordings).unwrap();
    let track_path = f.track_path();
    assert_eq!(track_path.extension().unwrap(), "flac");
    assert!(!track_path.with_extension("wav").exists());
    cleanup(&f);
}

#[test]
fn recovery_finishes_a_lagging_wav_delete_when_metadata_already_points_at_flac() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let (info, samples) = audio_file::read_wav_pcm16(&f.track_path()).unwrap();
    let flac_path = f.dir.join("track-00.flac");
    audio_file::encode_flac_new(&flac_path, info.sample_rate, info.channels, &samples).unwrap();
    let mut recording = f.recordings.get_by_id(&f.recording.recording_id).unwrap().unwrap();
    recording.tracks[0].file_path = flac_path.display().to_string();
    f.recordings.save(&recording).unwrap();
    assert!(f.dir.join("track-00.wav").exists());
    recover_tracks(&f.root, &[], &f.recordings).unwrap();
    assert!(!f.dir.join("track-00.wav").exists());
    assert!(f.track_path().exists());
    cleanup(&f);
}

#[test]
fn recovery_falls_back_to_wav_when_metadata_flac_is_missing() {
    let f = Fixture::new();
    f.mark_transcription_completed();
    let mut recording = f.recordings.get_by_id(&f.recording.recording_id).unwrap().unwrap();
    let missing_flac = f.dir.join("track-00.flac");
    recording.tracks[0].file_path = missing_flac.display().to_string();
    f.recordings.save(&recording).unwrap();
    recover_tracks(&f.root, &[], &f.recordings).unwrap();
    let track_path = f.track_path();
    assert_eq!(track_path.extension().unwrap(), "wav");
    assert!(track_path.exists());
    cleanup(&f);
}
