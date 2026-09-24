use super::*;
use crate::{recording::Track, transcription_types::TranscriptSegment};
use std::sync::atomic::AtomicUsize;

struct Fixture {
    root: PathBuf,
    recordings: RecordingStore,
    store: TranscriptionStore,
    recording: Recording,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "meeting-transcription-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let store = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        fs::create_dir(root.join(&id)).unwrap();
        let recording = Recording {
            recording_id: id.clone(),
            started_at: 0,
            ended_at: Some(1000),
            duration_ms: 1000,
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
                file_path: root.join(&id).join("track-00.wav").display().to_string(),
                frames_written: 16000,
                dropped_samples: 0,
                error: None,
            }],
        };
        recordings.save(&recording).unwrap();
        Self {
            root,
            recordings,
            store,
            recording,
        }
    }
    fn model(&self) -> (String, String) {
        let hash = checksum(b"synthetic model fixture, never passed to Whisper");
        let dir = self.root.join("models");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{hash}.bin"));
        let bytes = b"synthetic model fixture, never passed to Whisper";
        fs::write(&path, bytes).unwrap();
        self.store
            .upsert_model(
                "base",
                &path.display().to_string(),
                &hash,
                bytes.len() as u64,
            )
            .unwrap();
        (path.display().to_string(), hash)
    }
    fn tick(&self, engine: &FakeEngine) {
        self.tick_with_key(engine, "base");
    }
    fn tick_with_key(&self, engine: &FakeEngine, key: &str) {
        let key = key.to_string();
        let settings: SettingsReader = Arc::new(move || Ok(TranscriptionSettings::for_model(key.clone())));
        tick(
            &self.root,
            &self.store,
            &self.recordings,
            &settings,
            engine,
            &AtomicBool::new(false),
        )
        .unwrap();
    }
    fn job(&self) -> StoredJob {
        self.store
            .get(&self.recording.recording_id)
            .unwrap()
            .unwrap()
    }
}
#[derive(Default)]
struct FakeEngine {
    calls: AtomicUsize,
    fail: AtomicBool,
}
impl TranscriptionEngine for FakeEngine {
    fn transcribe(
        &self,
        r: &TranscriptionRequest,
        _: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Transcript, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err("Synthetic decoder failure".into());
        }
        progress(50);
        Ok(Transcript {
            recording_id: r.recording.recording_id.clone(),
            model_key: r.model_key.clone(),
            model_hash: r.model_hash.clone(),
            warnings: vec![],
            languages_used: vec![],
            segments: vec![TranscriptSegment {
                start_ms: 0,
                end_ms: 900,
                text: "Hello, meeting.".into(),
                source_id: "mic".into(),
                source_name: "Desk microphone".into(),
            }],
        })
    }
}
#[test]
fn completed_job_is_not_transcribed_again_or_retried() {
    let f = Fixture::new();
    f.model();
    let engine = FakeEngine::default();
    f.tick(&engine);
    f.tick(&engine);
    f.store.retry(&f.recording.recording_id).unwrap();
    f.store.recover().unwrap();
    f.tick(&engine);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().attempts, 1);
    assert!(f.job().checksum.is_some());
    let md =
        fs::read_to_string(f.root.join(&f.recording.recording_id).join("transcript.md")).unwrap();
    assert!(md.contains("1970-01-01 00:00:00 UTC"));
    assert!(md.contains("Hello, meeting\\."));
}
#[test]
fn missing_model_waits_and_import_requeues_without_busy_retry() {
    let f = Fixture::new();
    let engine = FakeEngine::default();
    f.tick(&engine);
    f.tick(&engine);
    assert_eq!(f.job().status, "waiting_model");
    assert_eq!(f.job().attempts, 0);
    f.model();
    f.store.requeue_waiting("base").unwrap();
    f.tick(&engine);
    assert_eq!(f.job().status, "completed");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn failure_requires_explicit_retry_and_preserves_original_model() {
    let f = Fixture::new();
    let (path, hash) = f.model();
    let engine = FakeEngine::default();
    engine.fail.store(true, Ordering::SeqCst);
    f.tick(&engine);
    f.tick(&engine);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.job().status, "failed");
    f.store
        .upsert_model("base", "not-the-original-model", "other-hash", 1)
        .unwrap();
    engine.fail.store(false, Ordering::SeqCst);
    f.store.retry(&f.recording.recording_id).unwrap();
    f.tick(&engine);
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().model_path, Some(path));
    assert_eq!(f.job().model_hash, hash);
    assert_eq!(f.job().attempts, 2);
}
#[test]
fn restart_finishes_json_checkpoint_without_reinference() {
    let f = Fixture::new();
    let (path, hash) = f.model();
    let engine = FakeEngine::default();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "base", "").unwrap();
    assert!(f.store.claim(id, &path, &hash).unwrap());
    let transcript = Transcript {
        recording_id: id.clone(),
        model_key: "base".into(),
        model_hash: hash,
        segments: vec![],
        warnings: vec![],
        languages_used: vec![],
    };
    fs::write(
        f.root.join(id).join("transcript.json"),
        serde_json::to_vec(&transcript).unwrap(),
    )
    .unwrap();
    let restarted = TranscriptionStore::open(&f.root.join("transcription.sqlite3")).unwrap();
    restarted.recover().unwrap();
    f.tick(&engine);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.job().status, "completed");
    assert!(f.root.join(id).join("transcript.md").exists());
}
#[test]
fn corrupt_checkpoint_is_preserved_without_reinference() {
    let f = Fixture::new();
    f.model();
    let engine = FakeEngine::default();
    let json = f
        .root
        .join(&f.recording.recording_id)
        .join("transcript.json");
    fs::write(&json, b"partial-json").unwrap();
    f.tick(&engine);
    assert_eq!(f.job().status, "failed");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read(&json).unwrap(), b"partial-json");
}
#[test]
fn legacy_and_failed_capture_audio_is_never_enqueued() {
    let mut f = Fixture::new();
    f.recording.auto_transcribe = false;
    f.recordings.save(&f.recording).unwrap();
    let engine = FakeEngine::default();
    f.tick(&engine);
    assert!(f.store.all().unwrap().is_empty());
    f.recording.auto_transcribe = true;
    f.recording.status = "failed".into();
    f.recordings.save(&f.recording).unwrap();
    f.tick(&engine);
    assert!(f.store.all().unwrap().is_empty());
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
    let mut json = serde_json::to_value(&f.recording).unwrap();
    json.as_object_mut().unwrap().remove("auto_transcribe");
    assert!(
        !serde_json::from_value::<Recording>(json)
            .unwrap()
            .auto_transcribe
    );
}
#[test]
fn claim_transition_and_completion_are_guarded() {
    let f = Fixture::new();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "base", "").unwrap();
    f.store.enqueue(id, "tiny", "").unwrap();
    assert_eq!(f.store.all().unwrap().len(), 1);
    assert_eq!(f.job().model_key, "base");
    assert!(f.store.complete(id, "j", "m", "h").is_err());
    assert!(f.store.claim(id, "model", "hash").unwrap());
    assert!(!f.store.claim(id, "model", "hash").unwrap());
    f.store.complete(id, "j", "m", "h").unwrap();
    assert!(f.store.status(id, "failed", 0, None).is_err());
    assert!(f.store.complete(id, "j", "m", "h").is_err());
    assert!(f.store.retry(&uuid::Uuid::new_v4().to_string()).is_err());
}
#[test]
fn inhibited_service_and_failed_storage_do_not_run_engine() {
    let f = Fixture::new();
    f.model();
    let engine = Arc::new(FakeEngine::default());
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("tiny"))),
        engine.clone(),
        true,
    );
    assert!(service.state().inhibited);
    assert!(service.state().jobs.is_empty());
    assert!(service.retry(&f.recording.recording_id).is_err());
    service.shutdown().unwrap();
    let broken = f.root.join("not-a-directory");
    fs::write(&broken, b"x").unwrap();
    let service = TranscriptionService::with_inhibition(
        broken,
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine.clone(),
        false,
    );
    assert!(service.state().error.is_some());
    service.shutdown().unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn markdown_escapes_external_images_html_and_false_speaker_claims() {
    assert_eq!(
        literal("<img src='x'> ![x](https://x)"),
        "&lt;img src='x'&gt; \\!\\[x\\]\\(https://x\\)"
    );
    assert_eq!(utc_date(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(utc_date(951782400000), "2000-02-29 00:00:00 UTC");
    assert!(safe_id("../outside").is_err());
}
#[test]
fn bad_checkpoint_identity_and_model_paths_are_rejected() {
    let f = Fixture::new();
    let (path, hash) = f.model();
    let engine = FakeEngine::default();
    f.tick(&engine);
    assert!(managed_model(&f.root, "C:/outside.bin", &hash, None).is_err());
    assert!(managed_model(&f.root, &path, "../bad", None).is_err());
    let transcript = Transcript {
        recording_id: uuid::Uuid::new_v4().to_string(),
        model_key: "base".into(),
        model_hash: hash,
        segments: vec![],
        warnings: vec![],
        languages_used: vec![],
    };
    assert!(validate_transcript(&transcript, &f.job(), &f.recording).is_err());
}

#[test]
fn open_checks_integrity_and_missing_completed_output_never_requeues() {
    let f = Fixture::new();
    f.model();
    let engine = Arc::new(FakeEngine::default());
    f.tick(&engine);
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine.clone(),
        true,
    );
    let md = service.transcript_path(&f.recording.recording_id).unwrap();
    fs::write(&md, "User changed this Markdown").unwrap();
    assert!(service.transcript_path(&f.recording.recording_id).is_err());
    fs::remove_file(&md).unwrap();
    f.tick(&engine);
    assert!(service.transcript_path(&f.recording.recording_id).is_err());
    assert_eq!(f.job().status, "completed");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    service.shutdown().unwrap();
}

#[test]
fn finalized_query_includes_old_recordings_outside_recent_window() {
    let f = Fixture::new();
    for i in 1..=30 {
        let mut recording = f.recording.clone();
        recording.recording_id = uuid::Uuid::new_v4().to_string();
        recording.started_at = i;
        f.recordings.save(&recording).unwrap();
    }
    assert_eq!(f.recordings.recent().unwrap().len(), 25);
    assert_eq!(f.recordings.finalized_eligible().unwrap().len(), 31);
}

#[test]
fn managed_import_validates_hash_and_wakes_only_matching_jobs() {
    let f = Fixture::new();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        Arc::new(FakeEngine::default()),
        true,
    );
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "tiny", "").unwrap();
    f.store.status(id, "waiting_model", 0, None).unwrap();
    let mut header = [0u8; 48];
    header[..4].copy_from_slice(&0x67676d6cu32.to_le_bytes());
    header[20..24].copy_from_slice(&4i32.to_le_bytes());
    header[28..32].copy_from_slice(&384i32.to_le_bytes());
    header[36..40].copy_from_slice(&4i32.to_le_bytes());
    let model = f.root.join("synthetic-header.bin");
    fs::write(&model, header).unwrap();
    let imported = service.import_model(model.clone()).unwrap();
    assert_eq!(imported.key, "tiny");
    assert_eq!(f.job().status, "queued");
    assert!(!service.state().model.available); // selected base, not imported tiny
    assert_eq!(service.import_model(model).unwrap(), imported);
    service.shutdown().unwrap();
}

#[test]
fn unclaimed_job_follows_currently_selected_model_when_its_pinned_model_is_missing() {
    let f = Fixture::new();
    let (_, hash) = f.model(); // imports "base"
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "small", "").unwrap();
    let engine = FakeEngine::default();
    f.tick_with_key(&engine, "small");
    assert_eq!(f.job().status, "waiting_model");
    assert_eq!(f.job().model_key, "small");
    // Selecting "base" instead re-points the never-claimed job and lets it run.
    f.tick_with_key(&engine, "base");
    assert_eq!(f.job().status, "completed");
    assert_eq!(f.job().model_key, "base");
    assert_eq!(f.job().model_hash, hash);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn claimed_job_keeps_its_pinned_model_even_when_the_setting_changes() {
    let f = Fixture::new();
    let (path, hash) = f.model();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "base", "").unwrap();
    assert!(f.store.claim(id, &path, &hash).unwrap());
    // A different setting must not steal or re-pin an already-claimed job.
    f.store.repin_unclaimed("small", "unrelated-hash").unwrap();
    assert_eq!(f.job().model_key, "base");
    assert_eq!(f.job().model_hash, hash);
    assert_eq!(f.job().status, "transcribing");
}
#[test]
fn importing_the_only_available_model_wakes_a_job_once_the_setting_is_switched() {
    let f = Fixture::new();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "small", "").unwrap();
    let engine = FakeEngine::default();
    f.tick_with_key(&engine, "small");
    assert_eq!(f.job().status, "waiting_model");
    let selected = Arc::new(std::sync::Mutex::new("small".to_string()));
    let reader = selected.clone();
    let settings: SettingsReader =
        Arc::new(move || Ok(TranscriptionSettings::for_model(reader.lock().unwrap().clone())));
    let service =
        TranscriptionService::with_inhibition(f.root.clone(), settings, Arc::new(FakeEngine::default()), true);
    let mut header = [0u8; 48];
    header[..4].copy_from_slice(&0x67676d6cu32.to_le_bytes());
    header[20..24].copy_from_slice(&6i32.to_le_bytes());
    header[28..32].copy_from_slice(&512i32.to_le_bytes());
    header[36..40].copy_from_slice(&6i32.to_le_bytes());
    let model = f.root.join("synthetic-base-header.bin");
    fs::write(&model, header).unwrap();
    let info = service.import_model(model).unwrap();
    assert_eq!(info.key, "base");
    // Still selected "small", which is not imported: nothing changed yet.
    assert_eq!(f.job().status, "waiting_model");
    assert!(!service.has_model("small").unwrap());
    // lib.rs would now switch the saved setting to the imported size...
    *selected.lock().unwrap() = "base".into();
    // ...and repin so the job doesn't wait for the next worker tick.
    service.repin_to_current_model().unwrap();
    assert_eq!(f.job().status, "queued");
    assert_eq!(f.job().model_key, "base");
    f.tick_with_key(&FakeEngine::default(), "base");
    assert_eq!(f.job().status, "completed");
    service.shutdown().unwrap();
}
#[test]
fn worker_shutdown_cancels_owned_job_and_restart_can_reclaim_it() {
    struct BlockingEngine(AtomicBool);
    impl TranscriptionEngine for BlockingEngine {
        fn transcribe(
            &self,
            _: &TranscriptionRequest,
            cancelled: &AtomicBool,
            progress: &mut dyn FnMut(u8),
        ) -> Result<Transcript, String> {
            self.0.store(true, Ordering::Release);
            progress(10);
            while !cancelled.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            Err("Cancelled test inference".into())
        }
    }
    let f = Fixture::new();
    f.model();
    let engine = Arc::new(BlockingEngine(AtomicBool::new(false)));
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine.clone(),
        false,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !engine.0.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let started = engine.0.load(Ordering::Acquire);
    service.shutdown().unwrap();
    assert!(started);
    assert_eq!(f.job().status, "queued");
    let next = FakeEngine::default();
    f.tick(&next);
    assert_eq!(f.job().status, "completed");
}

fn legacy_fixture() -> Fixture {
    let mut fixture = Fixture::new();
    fixture.recording.auto_transcribe = false;
    fixture.recordings.save(&fixture.recording).unwrap();
    fixture
}

#[test]
fn manual_request_opts_only_the_selected_legacy_recording_into_the_queue() {
    let f = legacy_fixture();
    let mut other = f.recording.clone();
    other.recording_id = uuid::Uuid::new_v4().to_string();
    other.auto_transcribe = false;
    fs::create_dir(f.root.join(&other.recording_id)).unwrap();
    other.tracks[0].file_path = f
        .root
        .join(&other.recording_id)
        .join("track-00.wav")
        .display()
        .to_string();
    f.recordings.save(&other).unwrap();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        Arc::new(FakeEngine::default()),
        false,
    );
    service
        .transcribe_recording(&f.recording.recording_id)
        .unwrap();
    assert!(
        f.recordings
            .get_by_id(&f.recording.recording_id)
            .unwrap()
            .unwrap()
            .auto_transcribe
    );
    assert!(
        !f.recordings
            .get_by_id(&other.recording_id)
            .unwrap()
            .unwrap()
            .auto_transcribe
    );
    assert!(f.store.get(&f.recording.recording_id).unwrap().is_some());
    assert!(f.store.get(&other.recording_id).unwrap().is_none());
    service.shutdown().unwrap();
}

#[test]
fn manual_double_click_deduplicates_and_completed_job_is_a_noop() {
    let f = legacy_fixture();
    let engine = Arc::new(FakeEngine::default());
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine.clone(),
        false,
    );
    let id = &f.recording.recording_id;
    service.transcribe_recording(id).unwrap();
    service.transcribe_recording(id).unwrap();
    assert_eq!(f.store.all().unwrap().len(), 1);
    service.shutdown().unwrap();

    f.model();
    f.store.requeue_waiting("base").unwrap();
    f.tick(engine.as_ref());
    assert_eq!(f.job().status, "completed");
    let calls = engine.calls.load(Ordering::SeqCst);
    let completed = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine.clone(),
        false,
    );
    completed.transcribe_recording(id).unwrap();
    completed.shutdown().unwrap();
    assert_eq!(f.job().status, "completed");
    assert_eq!(engine.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn manual_request_is_inhibited_without_mutating_metadata_or_jobs() {
    let f = legacy_fixture();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        Arc::new(FakeEngine::default()),
        true,
    );
    assert!(service
        .transcribe_recording(&f.recording.recording_id)
        .is_err());
    assert!(
        !f.recordings
            .get_by_id(&f.recording.recording_id)
            .unwrap()
            .unwrap()
            .auto_transcribe
    );
    assert!(f.store.all().unwrap().is_empty());
    service.shutdown().unwrap();
}

#[test]
fn manual_request_rejects_active_empty_failed_and_unknown_recordings() {
    let f = legacy_fixture();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        Arc::new(FakeEngine::default()),
        false,
    );
    for status in ["recording", "failed"] {
        let mut recording = f.recording.clone();
        recording.status = status.into();
        if status == "failed" {
            recording.tracks[0].frames_written = 0;
        }
        f.recordings.save(&recording).unwrap();
        assert!(service
            .transcribe_recording(&recording.recording_id)
            .is_err());
        assert!(f.store.get(&recording.recording_id).unwrap().is_none());
    }
    assert!(service
        .transcribe_recording(&uuid::Uuid::new_v4().to_string())
        .is_err());
    service.shutdown().unwrap();
}

#[test]
fn retained_failed_capture_requires_explicit_manual_queue_and_never_auto_opts_in() {
    let mut f = legacy_fixture();
    f.recording.status = "failed".into();
    f.recording.error = Some("writer failed after retained audio".into());
    f.recordings.save(&f.recording).unwrap();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        Arc::new(FakeEngine::default()),
        false,
    );
    service
        .transcribe_recording(&f.recording.recording_id)
        .unwrap();
    let job = f.store.get(&f.recording.recording_id).unwrap().unwrap();
    assert!(job.manual);
    assert!(
        !f.recordings
            .get_by_id(&f.recording.recording_id)
            .unwrap()
            .unwrap()
            .auto_transcribe
    );
    service.shutdown().unwrap();
}

#[test]
fn manual_waiting_model_request_is_idempotent_until_import() {
    let f = legacy_fixture();
    let engine = Arc::new(FakeEngine::default());
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("tiny"))),
        engine,
        false,
    );
    let id = &f.recording.recording_id;
    service.transcribe_recording(id).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while f.job().status != "waiting_model" && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.job().status, "waiting_model");
    service.transcribe_recording(id).unwrap();
    assert_eq!(f.job().status, "waiting_model");
    service.shutdown().unwrap();
}

#[test]
fn manual_failed_job_retry_opts_in_legacy_row_without_replacing_its_model_pin() {
    let f = legacy_fixture();
    let (path, hash) = f.model();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "base", "").unwrap();
    assert!(f.store.claim(id, &path, &hash).unwrap());
    f.store
        .status(id, "failed", 0, Some("synthetic failure"))
        .unwrap();
    let engine = Arc::new(FakeEngine::default());
    engine.fail.store(true, Ordering::SeqCst);
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("base"))),
        engine,
        false,
    );
    service.transcribe_recording(id).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while f.job().status != "failed" && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.job().status, "failed");
    assert!(f.recordings.get_by_id(id).unwrap().unwrap().auto_transcribe);
    assert_eq!(f.job().model_path.as_deref(), Some(path.as_str()));
    assert_eq!(f.job().model_hash, hash);
    service.shutdown().unwrap();
}

#[test]
fn transcript_outputs_follow_validated_new_storage_metadata() {
    let mut f = Fixture::new();
    let basename = format!("2026-09-24_12-00-00_Meeting--{}", f.recording.recording_id);
    let directory = f.root.join("meetings").join(&basename);
    fs::create_dir_all(&directory).unwrap();
    f.recording.storage_path = Some(format!("meetings/{basename}"));
    f.recordings.save(&f.recording).unwrap();
    f.model();
    let engine = FakeEngine::default();
    f.tick(&engine);
    assert_eq!(
        recording_directory(&f.root, &f.recording.recording_id).unwrap(),
        directory.canonicalize().unwrap()
    );
    assert!(directory.join("transcript.json").is_file());
    assert!(directory.join("transcript.md").is_file());
    assert!(!f
        .root
        .join(&f.recording.recording_id)
        .join("transcript.json")
        .exists());
}

#[test]
fn new_storage_checkpoint_recovers_without_reinference_after_restart() {
    let mut f = Fixture::new();
    let basename = format!("2026-09-24_12-00-00_Meeting--{}", f.recording.recording_id);
    let directory = f.root.join("meetings").join(&basename);
    fs::create_dir_all(&directory).unwrap();
    f.recording.storage_path = Some(format!("meetings/{basename}"));
    f.recordings.save(&f.recording).unwrap();
    let (path, hash) = f.model();
    let id = &f.recording.recording_id;
    f.store.enqueue(id, "base", "").unwrap();
    assert!(f.store.claim(id, &path, &hash).unwrap());
    let transcript = Transcript {
        recording_id: id.clone(),
        model_key: "base".into(),
        model_hash: hash,
        segments: vec![],
        warnings: vec![],
        languages_used: vec![],
    };
    fs::write(
        directory.join("transcript.json"),
        serde_json::to_vec(&transcript).unwrap(),
    )
    .unwrap();
    let restarted = TranscriptionStore::open(&f.root.join("transcription.sqlite3")).unwrap();
    restarted.recover().unwrap();
    let engine = FakeEngine::default();
    f.tick(&engine);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.job().status, "completed");
    assert!(directory.join("transcript.md").is_file());
}

#[test]
fn explicit_restart_archives_completed_outputs_and_refuses_a_duplicate_request() {
    let f = Fixture::new();
    f.model();
    let engine = Arc::new(FakeEngine::default());
    f.tick(engine.as_ref());
    let guard = processing_guard().lock().unwrap();
    let service = TranscriptionService::with_inhibition(
        f.root.clone(),
        Arc::new(|| Ok(TranscriptionSettings::for_model("tiny"))),
        engine,
        false,
    );
    let id = &f.recording.recording_id;
    service.restart_transcription(id).unwrap();
    assert!(service.restart_transcription(id).is_err());
    let archives = fs::read_dir(f.root.join(id).join("transcript-archive"))
        .unwrap()
        .count();
    assert_eq!(archives, 1);
    drop(guard);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while f.job().status == "queued" && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.job().status, "waiting_model");
    assert_eq!(f.job().model_key, "tiny");
    assert!(f.job().manual);
    service.shutdown().unwrap();
}

#[test]
fn tombstoned_pending_restart_is_discarded_without_touching_retained_outputs() {
    let f = Fixture::new();
    let (_, hash) = f.model();
    let engine = FakeEngine::default();
    f.tick(&engine);
    let id = &f.recording.recording_id;
    f.store.begin_restart(id, "base", &hash).unwrap();
    f.recordings.tombstone(id, 1).unwrap();
    f.tick(&engine);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert!(f.store.restart_intents().unwrap().is_empty());
    assert!(f.root.join(id).join("transcript.json").is_file());
}

#[test]
fn pending_restart_replays_a_partial_archive_before_any_new_inference() {
    let f = Fixture::new();
    let (path, hash) = f.model();
    let engine = FakeEngine::default();
    f.tick(&engine);
    let id = &f.recording.recording_id;
    let intent = f.store.begin_restart(id, "base", &hash).unwrap();
    let directory = f.root.join(id);
    let archive = directory
        .join("transcript-archive")
        .join(format!("restart-{}", intent.archive_id));
    fs::create_dir_all(&archive).unwrap();
    fs::rename(
        directory.join("transcript.json"),
        archive.join("transcript.json"),
    )
    .unwrap();
    f.tick(&engine);
    assert!(archive.join("transcript.json").is_file());
    assert!(archive.join("transcript.md").is_file());
    assert!(f.store.restart_intents().unwrap().is_empty());
    assert!(engine.calls.load(Ordering::SeqCst) >= 2);
    let _ = path;
}
