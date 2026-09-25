use super::*;
use crate::{
    diarization_types::SpeakerTurn, recording::Track, transcription_types::TranscriptSegment,
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
            auto_transcribe: true,
            error: None,
            transcription_status: "pending".into(),
            appflowy_status: "pending".into(),
            tracks: vec![Track {
                source_id: "mic".into(),
                name: "Desk microphone".into(),
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
        let transcript = Transcript {
            recording_id: id.clone(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec![],
            segments: vec![
                TranscriptSegment {
                    start_ms: 0,
                    end_ms: 1000,
                    text: "Literal *words* <img src=x>.".into(),
                    source_id: "mic".into(),
                    source_name: "Desk microphone".into(),
                },
                TranscriptSegment {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: "An uncertain passage.".into(),
                    source_id: "mic".into(),
                    source_name: "Desk microphone".into(),
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
            &self.store,
            &self.registry,
            &self.recordings,
            engine,
            &AtomicBool::new(false),
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
            Arc::new(|| Ok(true)),
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
                    source_id: "mic".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 7,
                    confidence: Some(0.95),
                },
                SpeakerTurn {
                    source_id: "mic".into(),
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
    assert!(md.contains("Speaker unknown"));
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
        Arc::new(|| Ok(true)),
        engine.clone(),
        true,
    );
    assert!(service.state().inhibited);
    assert!(service.state().jobs.is_empty());
    assert!(service.retry(&f.recording.recording_id).is_err());
    service.shutdown().unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}
