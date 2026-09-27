use super::*;
use crate::{
    diarization_types::SpeakerTurn, recording::Track, transcription::markdown_document,
    transcription_types::TranscriptSegment,
};
use std::sync::atomic::AtomicUsize;

struct Fixture {
    root: PathBuf,
    store: DiarizationStore,
    registry: TranscriptionStore,
    recordings: RecordingStore,
    recording: Recording,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("meeting-speakers-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = DiarizationStore::open(&root.join("diarization.sqlite3")).unwrap();
        let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording = Recording {
            recording_id: id.clone(),
            started_at: 0,
            ended_at: Some(2000),
            duration_ms: 2000,
            status: "recorded".into(),
            meeting_title: None,
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            error: None,
            transcription_status: "pending".into(),
            appflowy_status: "pending".into(),
            tracks: vec![Track {
                source_id: "loopback".into(),
                name: "Meeting audio (loopback)".into(),
                direction: "output".into(),
                sample_rate: 16000,
                channels: 1,
                file_path: dir.join("track-00.wav").display().to_string(),
                frames_written: 32000,
                dropped_samples: 0,
                error: None,
            }],
        };
        recordings.save(&recording).unwrap();
        let transcript = Transcript {
            recording_id: id.clone(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec![],
            local_speaker_name: Some("You".into()),
            segments: vec![
                TranscriptSegment {
                    start_ms: 0,
                    end_ms: 1000,
                    text: "Literal *words* <img src=x>.".into(),
                    source_id: "loopback".into(),
                    source_name: "Meeting audio (loopback)".into(),
                },
                TranscriptSegment {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: "An uncertain passage.".into(),
                    source_id: "loopback".into(),
                    source_name: "Meeting audio (loopback)".into(),
                },
            ],
        };
        let json = serde_json::to_vec_pretty(&transcript).unwrap();
        fs::write(dir.join("transcript.json"), &json).unwrap();
        fs::write(
            dir.join("transcript.md"),
            markdown_document(&recording, &transcript),
        )
        .unwrap();
        registry
            .enqueue(&id, "base", &transcript.model_hash)
            .unwrap();
        assert!(registry
            .claim(&id, "unused-fake-whisper-model", &transcript.model_hash)
            .unwrap());
        registry
            .complete(
                &id,
                &dir.join("transcript.json").display().to_string(),
                &dir.join("transcript.md").display().to_string(),
                &checksum(&json),
            )
            .unwrap();
        Self {
            root,
            store,
            registry,
            recordings,
            recording,
        }
    }
    fn models(&self) -> ModelPair {
        let models = self.root.join("models");
        fs::create_dir_all(&models).unwrap();
        for (i, kind) in KINDS.iter().enumerate() {
            let bytes = vec![i as u8; 4096];
            let hash = checksum(&bytes);
            let path = models.join(format!("{hash}.onnx"));
            fs::write(&path, &bytes).unwrap();
            self.registry
                .upsert_model(
                    &format!("diarization_{kind}"),
                    &path.display().to_string(),
                    &hash,
                    4096,
                )
                .unwrap();
        }
        model_pair(&self.root, &self.registry).unwrap().unwrap()
    }
    fn tick(&self, engine: &FakeEngine) {
        tick(
            &self.root,
            &[],
            &self.store,
            &self.registry,
            &self.recordings,
            engine,
            &AtomicBool::new(false),
            false,
            &real_embedding_extractor,
        )
        .unwrap();
    }
    fn job(&self) -> SpeakerJob {
        self.store
            .get(&self.recording.recording_id)
            .unwrap()
            .unwrap()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(&self.recording.recording_id).join(name)
    }
    fn service(&self) -> DiarizationService {
        DiarizationService::with_inhibition(
            self.root.clone(),
            Arc::new(|| {
                Ok(crate::diarization::DiarizationSettings {
                    enabled: true,
                    recognize_known_voices: false,
                    extra_library_roots: Vec::new(),
                })
            }),
            Arc::new(FakeEngine::default()),
            true,
        )
    }
}
#[derive(Default)]
struct FakeEngine {
    calls: AtomicUsize,
    fail: AtomicBool,
    cancel: AtomicBool,
}
impl DiarizationEngine for FakeEngine {
    fn diarize(
        &self,
        r: &DiarizationRequest,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<DiarizationOutput, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.cancel.load(Ordering::SeqCst) {
            cancel.store(true, Ordering::SeqCst);
            return Err("Cancelled fake engine".into());
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err("Synthetic model failure".into());
        }
        progress(40);
        Ok(DiarizationOutput {
            recording_id: r.recording.recording_id.clone(),
            warnings: vec![],
            turns: vec![
                SpeakerTurn {
                    source_id: "loopback".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 7,
                    confidence: Some(0.95),
                },
                SpeakerTurn {
                    source_id: "loopback".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: 8,
                    confidence: Some(0.2),
                },
            ],
        })
    }
}
#[test]
fn completes_once_preserves_whisper_and_escapes_literal_text() {
    let f = Fixture::new();
    f.models();
    let e = FakeEngine::default();
    let original = fs::read(f.path("transcript.json")).unwrap();
    let baseline = fs::read(f.path("transcript.md")).unwrap();
    f.tick(&e);
    f.tick(&e);
    f.store.retry(&f.recording.recording_id).unwrap();
    f.store.recover().unwrap();
    f.tick(&e);
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().speaker_count, Some(1));
    assert_eq!(fs::read(f.path("transcript.json")).unwrap(), original);
    assert_eq!(fs::read(f.path("transcript.md")).unwrap(), baseline);
    let md = fs::read_to_string(f.path("transcript-speakers.md")).unwrap();
    assert!(md.contains("Speaker 1"));
    assert!(md.contains("Remote"));
    assert!(md.contains("Literal \\*words\\* &lt;img src=x&gt;\\."));
    assert!(!md.contains("No summarization or diarization has been applied"));
    assert_eq!(
        f.registry
            .get(&f.recording.recording_id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    assert_eq!(
        f.service()
            .preferred_transcript_path(&f.recording.recording_id)
            .unwrap(),
        Some(f.path("transcript-speakers.md"))
    );
}
#[test]
fn missing_models_wait_without_reading_audio_or_hiding_transcript() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    f.tick(&e);
    f.tick(&e);
    assert_eq!(f.job().status, "waiting_models");
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    assert!(f
        .service()
        .preferred_transcript_path(&f.recording.recording_id)
        .unwrap()
        .is_none());
    assert!(f.path("transcript.md").exists());
    f.models();
    f.store.wake_waiting().unwrap();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
}
#[test]
fn failure_retry_keeps_model_pins_and_does_not_touch_whisper() {
    let f = Fixture::new();
    let pins = f.models();
    let e = FakeEngine::default();
    e.fail.store(true, Ordering::SeqCst);
    f.tick(&e);
    f.tick(&e);
    assert_eq!(f.job().status, "failed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    f.registry
        .upsert_model(
            "diarization_embedding",
            "nonexistent-new-model",
            &"b".repeat(64),
            4096,
        )
        .unwrap();
    f.store.retry(&f.recording.recording_id).unwrap();
    e.fail.store(false, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().models, Some(pins));
    assert_eq!(
        f.registry
            .get(&f.recording.recording_id)
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
}
#[test]
fn restart_after_json_checkpoint_finishes_without_inference_or_models() {
    let f = Fixture::new();
    let pins = f.models();
    let parent = f.registry.get(&f.recording.recording_id).unwrap().unwrap();
    f.store
        .enqueue(
            &f.recording.recording_id,
            parent.checksum.as_deref().unwrap(),
        )
        .unwrap();
    f.store.claim(&f.recording.recording_id, &pins).unwrap();
    let output = DiarizationOutput {
        recording_id: f.recording.recording_id.clone(),
        turns: vec![],
        warnings: vec![],
    };
    let checkpoint = SpeakerCheckpoint {
        recording_id: f.recording.recording_id.clone(),
        parent_checksum: parent.checksum.unwrap(),
        models: pins.clone(),
        output,
        labels: vec![None, None],
    };
    fs::write(
        f.path("speakers.json"),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();
    fs::remove_file(&pins.embedding_path).unwrap();
    fs::remove_file(&pins.segmentation_path).unwrap();
    let reopened = DiarizationStore::open(&f.root.join("diarization.sqlite3")).unwrap();
    reopened.recover().unwrap();
    let e = FakeEngine::default();
    f.tick(&e);
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.job().status, "completed");
    assert!(f.path("transcript-speakers.md").exists());
}
#[test]
fn corrupt_checkpoint_is_preserved_without_new_inference() {
    let f = Fixture::new();
    f.models();
    fs::write(f.path("speakers.json"), b"broken json").unwrap();
    let e = FakeEngine::default();
    f.tick(&e);
    f.store.retry(&f.recording.recording_id).unwrap();
    f.tick(&e);
    assert_eq!(f.job().status, "failed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read(f.path("speakers.json")).unwrap(), b"broken json");
}
#[test]
fn corrupted_completed_artifact_is_reported_never_regenerated() {
    let f = Fixture::new();
    f.models();
    let e = FakeEngine::default();
    f.tick(&e);
    fs::write(f.path("transcript-speakers.md"), b"user edited note").unwrap();
    assert!(f
        .service()
        .preferred_transcript_path(&f.recording.recording_id)
        .is_err());
    f.tick(&e);
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(f.path("transcript-speakers.md")).unwrap(),
        b"user edited note"
    );
}
#[test]
fn legacy_recordings_are_never_enqueued() {
    let f = Fixture::new();
    f.models();
    let mut recording = f.recording.clone();
    recording.auto_transcribe = false;
    f.recordings.save(&recording).unwrap();
    let e = FakeEngine::default();
    f.tick(&e);
    assert!(f.store.all().unwrap().is_empty());
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn cancellation_requeues_without_committing_artifacts() {
    let f = Fixture::new();
    f.models();
    let e = FakeEngine::default();
    e.cancel.store(true, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "queued");
    assert!(!f.path("speakers.json").exists());
    e.cancel.store(false, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
}
#[test]
fn model_import_is_local_pinned_and_rejects_replacement_corruption() {
    let f = Fixture::new();
    let service = f.service();
    let source = f.root.join("fixture.onnx");
    fs::write(&source, vec![3u8; 4096]).unwrap();
    assert!(service.import_model("unknown", source.clone()).is_err());
    let info = service
        .import_model("segmentation", source.clone())
        .unwrap();
    assert_eq!(info.key, "diarization_segmentation");
    let copy = f.root.join("models").join(format!("{}.onnx", info.sha256));
    fs::write(&copy, vec![4u8; 4096]).unwrap();
    assert!(service.import_model("segmentation", source).is_err());
    assert_eq!(fs::read(copy).unwrap(), vec![4u8; 4096]);
}
#[test]
fn inhibited_service_makes_no_claims() {
    let f = Fixture::new();
    f.models();
    let engine = Arc::new(FakeEngine::default());
    let service = DiarizationService::with_inhibition(
        f.root.clone(),
        Arc::new(|| {
            Ok(crate::diarization::DiarizationSettings {
                enabled: true,
                recognize_known_voices: false,
                extra_library_roots: Vec::new(),
            })
        }),
        engine.clone(),
        true,
    );
    assert!(service.state().inhibited);
    assert!(service.state().jobs.is_empty());
    assert!(service.retry(&f.recording.recording_id).is_err());
    service.shutdown().unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}

/// Automatic post-diarization recognition ("Recognize known voices in new
/// meetings"). Self-contained fixture (rather than reusing `Fixture` above):
/// it needs a longer recording with real, decodable WAV audio on the
/// loopback track (>= voice_profiles::MIN_TOTAL_SPEECH_SECONDS per speaker)
/// so `voice_profiles::recognize_speakers` has something to load and embed,
/// which none of the other tests in this file require.
mod recognition {
    use super::*;
    use crate::voice_profiles::{self, EmbeddingExtractor, VoiceProfileStore};

    /// A deterministic stand-in for the real sherpa-onnx embedding model:
    /// the "embedding" is just the turn's first PCM sample plus a zero, so
    /// two turns built from distinguishable constant-amplitude audio (see
    /// `write_wav` below) land as near-opposite unit vectors after
    /// `voice_profiles::centroid`'s L2 normalization -- trivially separable
    /// without needing a real model file.
    struct FixedExtractor;
    impl EmbeddingExtractor for FixedExtractor {
        fn model_hash(&self) -> String {
            "fake-embedding-hash".into()
        }
        fn extract(&self, samples: &[f32]) -> Result<Vec<f32>, String> {
            samples
                .first()
                .map(|s| vec![*s, 0.0])
                .ok_or_else(|| "empty".to_string())
        }
    }
    fn fake_extractor_factory(
        _path: &Path,
        _hash: &str,
    ) -> Result<Box<dyn EmbeddingExtractor>, String> {
        Ok(Box::new(FixedExtractor))
    }

    fn write_wav(path: &Path, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            writer.write_sample(*s).unwrap();
        }
        writer.finalize().unwrap();
    }

    struct RecognitionFixture {
        root: PathBuf,
        store: DiarizationStore,
        registry: TranscriptionStore,
        recordings: RecordingStore,
        recording: Recording,
    }
    impl RecognitionFixture {
        /// Builds a 12-second recording with two 6-second speaker turns on
        /// its loopback track, each well above MIN_TURN_SECONDS (1s) and
        /// MIN_TOTAL_SPEECH_SECONDS (5s) alone, with distinguishable
        /// constant-amplitude audio: the first half is a positive-amplitude
        /// tone ("speaker 1"), the second half negative ("speaker 2").
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("meeting-speakers-recognition-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let store = DiarizationStore::open(&root.join("diarization.sqlite3")).unwrap();
            let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
            let recordings = RecordingStore::open(&root).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let dir = root.join(&id);
            fs::create_dir(&dir).unwrap();
            let wav_path = dir.join("track-00.wav");
            let mut samples = vec![16_000i16; 96_000];
            samples.extend(std::iter::repeat_n(-16_000i16, 96_000));
            write_wav(&wav_path, &samples);
            let recording = Recording {
                recording_id: id.clone(),
                started_at: 0,
                ended_at: Some(12_000),
                duration_ms: 12_000,
                status: "recorded".into(),
                meeting_title: None,
                storage_path: None,
                library_root: None,
                auto_transcribe: true,
                error: None,
                transcription_status: "pending".into(),
                appflowy_status: "pending".into(),
                tracks: vec![Track {
                    source_id: "loopback".into(),
                    name: "Meeting audio (loopback)".into(),
                    direction: "output".into(),
                    sample_rate: 16000,
                    channels: 1,
                    file_path: wav_path.display().to_string(),
                    frames_written: 192_000,
                    dropped_samples: 0,
                    error: None,
                }],
            };
            recordings.save(&recording).unwrap();
            let transcript = Transcript {
                recording_id: id.clone(),
                model_key: "base".into(),
                model_hash: "a".repeat(64),
                warnings: vec![],
                languages_used: vec![],
                local_speaker_name: Some("You".into()),
                segments: vec![
                    TranscriptSegment {
                        start_ms: 0,
                        end_ms: 6000,
                        text: "First half.".into(),
                        source_id: "loopback".into(),
                        source_name: "Meeting audio (loopback)".into(),
                    },
                    TranscriptSegment {
                        start_ms: 6000,
                        end_ms: 12000,
                        text: "Second half.".into(),
                        source_id: "loopback".into(),
                        source_name: "Meeting audio (loopback)".into(),
                    },
                ],
            };
            let json = serde_json::to_vec_pretty(&transcript).unwrap();
            fs::write(dir.join("transcript.json"), &json).unwrap();
            fs::write(
                dir.join("transcript.md"),
                markdown_document(&recording, &transcript),
            )
            .unwrap();
            registry
                .enqueue(&id, "base", &transcript.model_hash)
                .unwrap();
            assert!(registry
                .claim(&id, "unused-fake-whisper-model", &transcript.model_hash)
                .unwrap());
            registry
                .complete(
                    &id,
                    &dir.join("transcript.json").display().to_string(),
                    &dir.join("transcript.md").display().to_string(),
                    &checksum(&json),
                )
                .unwrap();
            Self {
                root,
                store,
                registry,
                recordings,
                recording,
            }
        }
        fn models(&self) {
            let models = self.root.join("models");
            fs::create_dir_all(&models).unwrap();
            for (i, kind) in KINDS.iter().enumerate() {
                let bytes = vec![i as u8; 4096];
                let hash = checksum(&bytes);
                let path = models.join(format!("{hash}.onnx"));
                fs::write(&path, &bytes).unwrap();
                self.registry
                    .upsert_model(
                        &format!("diarization_{kind}"),
                        &path.display().to_string(),
                        &hash,
                        4096,
                    )
                    .unwrap();
            }
        }
        fn tick(&self, engine: &FakeSpeakersEngine, recognize_known_voices: bool) {
            tick(
                &self.root,
                &[],
                &self.store,
                &self.registry,
                &self.recordings,
                engine,
                &AtomicBool::new(false),
                recognize_known_voices,
                &fake_extractor_factory,
            )
            .unwrap();
        }
        fn names(&self) -> Vec<crate::speaker_names::NamedLabel> {
            crate::speaker_names::SpeakerNameStore::open(crate::speaker_names::store_path(&self.root))
                .unwrap()
                .list(&self.recording.recording_id)
                .unwrap()
        }
    }

    #[derive(Default)]
    struct FakeSpeakersEngine;
    impl DiarizationEngine for FakeSpeakersEngine {
        fn diarize(
            &self,
            r: &DiarizationRequest,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(u8),
        ) -> Result<DiarizationOutput, String> {
            Ok(DiarizationOutput {
                recording_id: r.recording.recording_id.clone(),
                warnings: vec![],
                turns: vec![
                    SpeakerTurn {
                        source_id: "loopback".into(),
                        start_ms: 0,
                        end_ms: 6000,
                        speaker: 1,
                        confidence: Some(0.95),
                    },
                    SpeakerTurn {
                        source_id: "loopback".into(),
                        start_ms: 6000,
                        end_ms: 12000,
                        speaker: 2,
                        confidence: Some(0.95),
                    },
                ],
            })
        }
    }

    /// The embedding that `FixedExtractor` would derive (after
    /// `voice_profiles::centroid`'s L2 normalization) from the first
    /// half's positive-amplitude tone: any positive first sample
    /// normalizes to exactly `[1.0, 0.0]`.
    fn alice_embedding() -> Vec<f32> {
        vec![1.0, 0.0]
    }

    #[test]
    fn auto_recognition_names_a_known_voice_after_diarization_completes() {
        let f = RecognitionFixture::new();
        f.models();
        VoiceProfileStore::open(voice_profiles::store_path(&f.root))
            .unwrap()
            .create("Alice", &alice_embedding(), "fake-embedding-hash", 20.0)
            .unwrap();
        f.tick(&FakeSpeakersEngine, true);
        let names = f.names();
        let speaker1 = names.iter().find(|n| n.label == "Speaker 1").unwrap();
        assert_eq!(speaker1.name, "Alice");
        assert!(speaker1.auto_identified);
        // Speaker 2's tone is the opposite direction -- no matching
        // profile, so it stays unnamed.
        assert!(names.iter().all(|n| n.label != "Speaker 2"));
        let md = fs::read_to_string(
            f.root.join(&f.recording.recording_id).join("transcript-speakers.md"),
        )
        .unwrap();
        assert!(md.contains("Alice"));
    }

    #[test]
    fn auto_recognition_does_nothing_when_the_setting_is_off() {
        let f = RecognitionFixture::new();
        f.models();
        VoiceProfileStore::open(voice_profiles::store_path(&f.root))
            .unwrap()
            .create("Alice", &alice_embedding(), "fake-embedding-hash", 20.0)
            .unwrap();
        f.tick(&FakeSpeakersEngine, false);
        assert!(f.names().is_empty());
    }

    #[test]
    fn auto_recognition_never_overwrites_a_name_the_user_already_set() {
        let f = RecognitionFixture::new();
        f.models();
        VoiceProfileStore::open(voice_profiles::store_path(&f.root))
            .unwrap()
            .create("Alice", &alice_embedding(), "fake-embedding-hash", 20.0)
            .unwrap();
        crate::speaker_names::SpeakerNameStore::open(crate::speaker_names::store_path(&f.root))
            .unwrap()
            .set(&f.recording.recording_id, "Speaker 1", "Custom Name", false)
            .unwrap();
        f.tick(&FakeSpeakersEngine, true);
        let names = f.names();
        let speaker1 = names.iter().find(|n| n.label == "Speaker 1").unwrap();
        assert_eq!(speaker1.name, "Custom Name");
        assert!(!speaker1.auto_identified);
    }
}
