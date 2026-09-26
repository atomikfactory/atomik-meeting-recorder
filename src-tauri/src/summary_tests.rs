use super::*;
use crate::{recording::Track, summary_types::ActionItem, transcription_types::TranscriptSegment};
use std::sync::atomic::AtomicUsize;

struct Fixture {
    root: PathBuf,
    store: SummaryStore,
    registry: TranscriptionStore,
    recordings: RecordingStore,
    recording: Recording,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("meeting-summary-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = SummaryStore::open(&root.join("summary.sqlite3")).unwrap();
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
        let transcript = Transcript {
            recording_id: id.clone(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec!["en".into()],
            local_speaker_name: Some("You".into()),
            segments: vec![
                TranscriptSegment {
                    start_ms: 0,
                    end_ms: 1000,
                    text: "Let's ship the *release* on Friday.".into(),
                    source_id: "mic".into(),
                    source_name: "Microphone".into(),
                },
                TranscriptSegment {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: "Sounds good, I will write the notes.".into(),
                    source_id: "mic".into(),
                    source_name: "Microphone".into(),
                },
            ],
        };
        let json = serde_json::to_vec_pretty(&transcript).unwrap();
        fs::write(dir.join("transcript.json"), &json).unwrap();
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
    fn tick(&self, engine: &FakeEngine) {
        let settings = SummarySettings::for_model("qwen3:8b");
        tick(
            &self.root,
            &self.store,
            &self.registry,
            &self.recordings,
            engine,
            &settings,
            &AtomicBool::new(false),
        )
        .unwrap();
    }
    fn job(&self) -> SummaryJob {
        self.store
            .get(&self.recording.recording_id)
            .unwrap()
            .unwrap()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(&self.recording.recording_id).join(name)
    }
    fn service(&self) -> SummaryService {
        SummaryService::with_inhibition(
            self.root.clone(),
            Arc::new(|| Ok(SummarySettings::for_model("qwen3:8b"))),
            Arc::new(FakeEngine::default()),
            true,
        )
    }
}
#[derive(Default)]
struct FakeEngine {
    calls: AtomicUsize,
    fail: AtomicBool,
    transient: AtomicBool,
    cancel: AtomicBool,
}
fn fake_output() -> SummaryOutput {
    SummaryOutput {
        title: "Release planning".into(),
        summary: "The team agreed to ship the release on Friday.".into(),
        key_points: vec!["Ship on Friday".into()],
        decisions: vec!["Release date confirmed".into()],
        action_items: vec![ActionItem {
            task: "Write release notes".into(),
            owner: None,
            due: None,
        }],
        open_questions: vec![],
        language: "en".into(),
    }
}
impl SummaryEngine for FakeEngine {
    fn summarize(
        &self,
        _request: &SummaryRequest,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<SummaryOutput, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.cancel.load(Ordering::SeqCst) {
            cancel.store(true, Ordering::SeqCst);
            return Err("Cancelled fake engine".into());
        }
        if self.transient.load(Ordering::SeqCst) {
            return Err("Ollama is not reachable at the configured URL.".into());
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err("Synthetic model failure".into());
        }
        progress(40);
        Ok(fake_output())
    }
    fn list_models(&self, _url: &str) -> Result<Vec<String>, String> {
        Ok(vec!["qwen3:8b".into()])
    }
    fn test_connection(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
}
#[test]
fn completes_once_writes_markdown_and_preserves_transcript() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    let original = fs::read(f.path("transcript.json")).unwrap();
    f.tick(&e);
    f.tick(&e);
    f.store.retry(&f.recording.recording_id).unwrap();
    f.store.recover().unwrap();
    f.tick(&e);
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().title.as_deref(), Some("Release planning"));
    assert_eq!(fs::read(f.path("transcript.json")).unwrap(), original);
    let md = fs::read_to_string(f.path("summary.md")).unwrap();
    assert!(md.contains("Release planning"));
    assert!(md.contains("Write release notes"));
    assert_eq!(
        f.service()
            .summary_path(&f.recording.recording_id)
            .unwrap(),
        f.path("summary.md")
    );
}
#[test]
fn ollama_down_waits_with_backoff_instead_of_failing() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    e.transient.store(true, Ordering::SeqCst);
    f.tick(&e);
    f.tick(&e);
    assert_eq!(f.job().status, "waiting_ollama");
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    // Backoff has not elapsed: another tick does not retry immediately.
    f.tick(&e);
    assert_eq!(f.job().status, "waiting_ollama");
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    // An explicit wake (e.g. the user fixed the URL) retries right away.
    f.store.wake_waiting().unwrap();
    e.transient.store(false, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
}
#[test]
fn failure_retry_recovers() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    e.fail.store(true, Ordering::SeqCst);
    f.tick(&e);
    f.tick(&e);
    assert_eq!(f.job().status, "failed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    f.store.retry(&f.recording.recording_id).unwrap();
    e.fail.store(false, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
}
#[test]
fn cancellation_requeues_without_committing_artifacts() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    e.cancel.store(true, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "queued");
    assert!(!f.path("summary.json").exists());
    e.cancel.store(false, Ordering::SeqCst);
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
}
#[test]
fn legacy_recordings_are_never_enqueued() {
    let f = Fixture::new();
    let mut recording = f.recording.clone();
    recording.auto_transcribe = false;
    f.recordings.save(&recording).unwrap();
    let e = FakeEngine::default();
    f.tick(&e);
    assert!(f.store.all().unwrap().is_empty());
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn restart_invalidation_removes_idle_job_and_requires_new_checksum() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    // Simulate what `restart_transcription` does: remove the idle summary job,
    // then land a new Whisper checkpoint under a new checksum. The recording
    // must be re-summarized, not silently kept under the old checkpoint.
    f.store.remove_if_idle(&f.recording.recording_id).unwrap();
    let new_hash = "b".repeat(64);
    f.registry
        .begin_restart(&f.recording.recording_id, "base", &new_hash)
        .unwrap();
    f.registry
        .finish_restart(&f.recording.recording_id, "base", &new_hash)
        .unwrap();
    let new_transcript = Transcript {
        recording_id: f.recording.recording_id.clone(),
        model_key: "base".into(),
        model_hash: new_hash.clone(),
        warnings: vec![],
        languages_used: vec!["en".into()],
        local_speaker_name: Some("You".into()),
        segments: vec![TranscriptSegment {
            start_ms: 0,
            end_ms: 500,
            text: "A different meeting entirely.".into(),
            source_id: "mic".into(),
            source_name: "Microphone".into(),
        }],
    };
    let bytes = serde_json::to_vec_pretty(&new_transcript).unwrap();
    fs::remove_file(f.path("transcript.json")).unwrap();
    fs::remove_file(f.path("summary.json")).unwrap();
    fs::remove_file(f.path("summary.md")).unwrap();
    fs::write(f.path("transcript.json"), &bytes).unwrap();
    assert!(f
        .registry
        .claim(&f.recording.recording_id, "unused-fake-whisper-model", &new_hash)
        .unwrap());
    f.registry
        .complete(
            &f.recording.recording_id,
            &f.path("transcript.json").display().to_string(),
            &f.path("transcript.md").display().to_string(),
            &checksum(&bytes),
        )
        .unwrap();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
    let md = fs::read_to_string(f.path("summary.md")).unwrap();
    assert!(md.contains("Release planning"));
}
#[test]
fn inhibited_service_makes_no_claims() {
    let f = Fixture::new();
    let engine = Arc::new(FakeEngine::default());
    let service = SummaryService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(SummarySettings::for_model("qwen3:8b"))),
        engine.clone(),
        true,
    );
    assert!(service.state().inhibited);
    assert!(service.state().jobs.is_empty());
    assert!(service.retry(&f.recording.recording_id).is_err());
    service.shutdown().unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn regenerate_reruns_with_current_model() {
    let f = Fixture::new();
    let e = FakeEngine::default();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    f.store.regenerate(&f.recording.recording_id).unwrap();
    // A regenerate keeps the same checkpoint content (the fake engine is
    // deterministic); the exercise is that the job transitions cleanly again.
    fs::remove_file(f.path("summary.json")).unwrap();
    fs::remove_file(f.path("summary.md")).unwrap();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
}
#[test]
fn queue_enqueues_a_first_summary_job_for_a_completed_transcript() {
    let f = Fixture::new();
    assert!(f.store.get(&f.recording.recording_id).unwrap().is_none());
    f.service().queue(&f.recording.recording_id).unwrap();
    assert_eq!(f.job().status, "queued");
}
#[test]
fn queue_is_idempotent_and_never_resets_an_existing_job() {
    let f = Fixture::new();
    let service = f.service();
    service.queue(&f.recording.recording_id).unwrap();
    service.queue(&f.recording.recording_id).unwrap();
    assert_eq!(f.store.all().unwrap().len(), 1, "must not create a duplicate row");
    let e = FakeEngine::default();
    f.tick(&e);
    assert_eq!(f.job().status, "completed");
    // Calling queue again once the job has already completed must not reset
    // or re-run it -- same idempotency guarantee as the automatic pipeline's
    // `store.enqueue` call in `tick`.
    service.queue(&f.recording.recording_id).unwrap();
    assert_eq!(f.job().status, "completed");
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn queue_refuses_a_recording_without_a_completed_transcript() {
    let root = std::env::temp_dir().join(format!("meeting-summary-queue-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    // Enqueued but never claimed/completed -- transcription itself has not
    // finished yet, so there is nothing to summarize.
    registry.enqueue(&id, "base", &"a".repeat(64)).unwrap();
    let service = SummaryService::with_inhibition(
        root.clone(),
        Arc::new(|| Ok(SummarySettings::for_model("qwen3:8b"))),
        Arc::new(FakeEngine::default()),
        true,
    );
    let error = service.queue(&id).unwrap_err();
    assert!(error.contains("not completed"), "{error}");
    drop(service);
    drop(registry);
    let _ = fs::remove_dir_all(&root);
}
#[test]
fn queue_refuses_an_unknown_recording() {
    let f = Fixture::new();
    let error = f.service().queue("00000000-0000-0000-0000-000000000000").unwrap_err();
    assert!(error.contains("no transcript"), "{error}");
}
#[test]
fn queue_reports_a_clear_error_and_makes_no_row_when_summaries_are_disabled() {
    let f = Fixture::new();
    let service = SummaryService::with_inhibition(
        f.root.clone(),
        Arc::new(|| {
            Ok(SummarySettings {
                enabled: false,
                ..SummarySettings::for_model("qwen3:8b")
            })
        }),
        Arc::new(FakeEngine::default()),
        true,
    );
    let error = service.queue(&f.recording.recording_id).unwrap_err();
    assert_eq!(error, "Summaries are turned off in Settings.");
    assert!(f.store.get(&f.recording.recording_id).unwrap().is_none());
}
