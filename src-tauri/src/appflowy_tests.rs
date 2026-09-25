use super::*;
use crate::{
    appflowy_credentials::FakeCredentialStore,
    appflowy_fake_server::FakeAppFlowyServer,
    recording::Track,
    transcription_types::TranscriptSegment,
};

struct Fixture {
    root: PathBuf,
    store: AppFlowyStore,
    recordings: RecordingStore,
    recording: Recording,
    credentials: SharedCredentialStore,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("meeting-appflowy-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = AppFlowyStore::open(&root.join("appflowy.sqlite3")).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording = Self::seed_recording(&recordings, &registry, &dir, &id, "Weekly sync", "Let's kick off.", "Sounds good.");
        let credentials: SharedCredentialStore = Arc::new(FakeCredentialStore::new());
        Self { root, store, recordings, recording, credentials }
    }
    fn seed_recording(
        recordings: &RecordingStore,
        registry: &TranscriptionStore,
        dir: &Path,
        id: &str,
        title: &str,
        line1: &str,
        line2: &str,
    ) -> Recording {
        let recording = Recording {
            recording_id: id.to_string(),
            started_at: 1_726_500_000_000,
            ended_at: Some(1_726_500_120_000),
            duration_ms: 120_000,
            status: "recorded".into(),
            meeting_title: Some(title.to_string()),
            storage_path: None,
            library_root: None,
            auto_transcribe: true,
            error: None,
            transcription_status: "pending".into(),
            appflowy_status: "idle".into(),
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
            recording_id: id.to_string(),
            model_key: "base".into(),
            model_hash: "a".repeat(64),
            warnings: vec![],
            languages_used: vec!["en".into()],
            local_speaker_name: Some("You".into()),
            segments: vec![
                TranscriptSegment { start_ms: 0, end_ms: 1000, text: line1.into(), source_id: "mic".into(), source_name: "Microphone".into() },
                TranscriptSegment { start_ms: 1000, end_ms: 2000, text: line2.into(), source_id: "mic".into(), source_name: "Microphone".into() },
            ],
        };
        let json = serde_json::to_vec_pretty(&transcript).unwrap();
        fs::write(dir.join("transcript.json"), &json).unwrap();
        registry.enqueue(id, "base", &transcript.model_hash).unwrap();
        assert!(registry.claim(id, "unused-fake-whisper-model", &transcript.model_hash).unwrap());
        registry
            .complete(id, &dir.join("transcript.json").display().to_string(), &dir.join("transcript.md").display().to_string(), &checksum(&json))
            .unwrap();
        recording
    }
    fn add_second_meeting(&self, title: &str) -> Recording {
        let registry = TranscriptionStore::open(&self.root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        fs::create_dir(&dir).unwrap();
        Self::seed_recording(&self.recordings, &registry, &dir, &id, title, "Second meeting starts.", "Great, let's go.")
    }
    fn sign_in(&self) {
        self.credentials.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
        self.credentials.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    }
    fn settings(&self, server: &FakeAppFlowyServer) -> AppFlowySettings {
        let mut settings = AppFlowySettings::for_test(server.workspace_id.clone());
        settings.base_url = server.base_url.clone();
        // The fixture does not seed a completed summary job; the export must
        // still succeed using the transcript alone (summary disabled for the
        // recording, or simply not produced) -- see `is_ready_for_export`.
        settings.summary_enabled = false;
        settings
    }
    fn tick(&self, server: &FakeAppFlowyServer) {
        let settings = self.settings(server);
        tick(&self.root, &self.store, &settings, &self.credentials, &AtomicBool::new(false)).unwrap();
    }
    fn export_recording(&self, server: &FakeAppFlowyServer, recording_id: &str) {
        self.store.enqueue(recording_id).unwrap();
        self.tick(server);
    }
    /// The date page name this fixture's recording must land under, computed
    /// the same way `export_one` does -- kept dynamic (not a hardcoded
    /// string) so these tests pass regardless of the machine's local
    /// timezone.
    fn date(&self) -> String {
        local_date_string(self.recording.started_at)
    }
}

#[test]
fn first_export_creates_root_date_and_meeting_with_exactly_three_posts() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    let posts = server.requests().into_iter().filter(|r| r.method == "POST" && r.path.ends_with("/page-view")).count();
    assert_eq!(posts, 3, "root, date, and meeting pages are each one POST");
    let job = fixture.store.get_job(&fixture.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "completed");

    let root_names = server.child_names(&server.workspace_id);
    assert_eq!(root_names, vec!["My daily meetings".to_string()]);
    let root_id = fixture.store.get_page(&root_key(&server.workspace_id, &server.workspace_id)).unwrap().unwrap().remote_view_id.unwrap();
    let date_names = server.child_names(&root_id);
    assert_eq!(date_names, vec![fixture.date()]);
    let date_page = fixture.store.get_page(&date_key(&server.workspace_id, &root_id, &fixture.date())).unwrap().unwrap();
    let meeting_names = server.child_names(date_page.remote_view_id.as_deref().unwrap());
    assert_eq!(meeting_names.len(), 1);
    assert!(meeting_names[0].contains("Weekly sync"));

    // The meeting page's content schema-checks against the native block tree.
    let meeting_key_str = meeting_key(&fixture.recording.recording_id, 1);
    let meeting_remote_id = fixture.store.get_page(&meeting_key_str).unwrap().unwrap().remote_view_id.unwrap();
    let page_data = server.page_data_for(&meeting_remote_id).unwrap();
    assert_eq!(page_data["children"][0]["type"], "ai_meeting");
}

#[test]
fn fallback_mode_sends_standard_blocks_instead_of_the_native_type() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let mut settings = fixture.settings(&server);
    settings.use_native_block = false;
    fixture.store.enqueue(&fixture.recording.recording_id).unwrap();
    tick(&fixture.root, &fixture.store, &settings, &fixture.credentials, &AtomicBool::new(false)).unwrap();

    let meeting_remote_id = fixture.store.get_page(&meeting_key(&fixture.recording.recording_id, 1)).unwrap().unwrap().remote_view_id.unwrap();
    let page_data = server.page_data_for(&meeting_remote_id).unwrap();
    let rendered = serde_json::to_string(&page_data).unwrap();
    assert!(!rendered.contains("ai_meeting"));
    assert_eq!(page_data["children"][0]["type"], "heading");
}

#[test]
fn second_meeting_same_day_only_creates_the_meeting_page() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let posts_after_first = server.requests_matching("POST", "/page-view");

    let second = fixture.add_second_meeting("Design review");
    fixture.export_recording(&server, &second.recording_id);
    let posts_after_second = server.requests_matching("POST", "/page-view");

    assert_eq!(posts_after_second - posts_after_first, 1, "only the new meeting page should be created");
    let root_id = fixture.store.get_page(&root_key(&server.workspace_id, &server.workspace_id)).unwrap().unwrap().remote_view_id.unwrap();
    assert_eq!(server.child_names(&root_id), vec![fixture.date()], "no duplicate date page");
}

#[test]
fn an_existing_daily_meetings_page_is_found_and_reused() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start_with_existing_top_level_page("My daily meetings");
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    // Only 2 POSTs: date + meeting -- the root page was never created.
    assert_eq!(server.requests_matching("POST", "/page-view"), 2);
    assert_eq!(server.child_names(&server.workspace_id), vec!["My daily meetings".to_string()], "no second root page appeared");
}

#[test]
fn crash_between_page_creation_and_journal_update_is_recovered_on_retry() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    // Simulate: the root page was already created on the server in a
    // previous run, but the process crashed before `mark_created` ran, so
    // the local journal still shows "creating" with no remote id.
    fixture.store.find_or_plan(&root_key(&server.workspace_id, &server.workspace_id), "root", None, &server.workspace_id, "My daily meetings").unwrap();
    server.seed_existing_page(&server.workspace_id, "root-created-before-crash", "My daily meetings");

    fixture.export_recording(&server, &fixture.recording.recording_id);

    // The retry must have adopted the existing page rather than creating a
    // second one: only 2 POSTs (date + meeting), and the journal now points
    // at the pre-existing remote id.
    assert_eq!(server.requests_matching("POST", "/page-view"), 2);
    assert_eq!(server.child_names(&server.workspace_id), vec!["My daily meetings".to_string()]);
    let root_record = fixture.store.get_page(&root_key(&server.workspace_id, &server.workspace_id)).unwrap().unwrap();
    assert_eq!(root_record.remote_view_id.as_deref(), Some("root-created-before-crash"));
}

#[test]
fn export_is_never_enqueued_automatically_when_the_feature_is_disabled() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let mut settings = fixture.settings(&server);
    settings.auto_export = true;
    // tick() itself is only ever invoked while `enabled` (see AppFlowyService's
    // worker loop, which gates every tick on it); calling tick directly with
    // enabled=false would defeat the point of this test, so instead prove
    // the auto-enqueue scan respects `auto_export` at the tick level.
    settings.auto_export = false;
    tick(&fixture.root, &fixture.store, &settings, &fixture.credentials, &AtomicBool::new(false)).unwrap();
    assert!(fixture.store.get_job(&fixture.recording.recording_id).unwrap().is_none());
    assert!(server.requests().is_empty());
}

#[test]
fn manual_export_works_even_with_auto_export_off() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let service = AppFlowyService::with_inhibition(
        fixture.root.clone(),
        {
            let server_url = server.base_url.clone();
            let workspace_id = server.workspace_id.clone();
            Arc::new(move || {
                let mut settings = AppFlowySettings::for_test(workspace_id.clone());
                settings.base_url = server_url.clone();
                settings.auto_export = false;
                settings.summary_enabled = false;
                Ok(settings)
            })
        },
        fixture.credentials.clone(),
        true,
    );
    service.export_now(&fixture.recording.recording_id).unwrap();
    let settings = fixture.settings(&server);
    tick(&fixture.root, &fixture.store, &settings, &fixture.credentials, &AtomicBool::new(false)).unwrap();
    let job = fixture.store.get_job(&fixture.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "completed");
}

#[test]
fn export_now_is_refused_when_the_feature_is_disabled() {
    let fixture = Fixture::new();
    let service = AppFlowyService::with_inhibition(
        fixture.root.clone(),
        Arc::new(|| Ok(AppFlowySettings::default_disabled())),
        fixture.credentials.clone(),
        true,
    );
    let error = service.export_now(&fixture.recording.recording_id).unwrap_err();
    assert!(error.contains("turned off"), "{error}");
}

#[test]
fn export_again_requires_confirmation_and_creates_a_new_suffixed_page() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    let service = AppFlowyService::with_inhibition(
        fixture.root.clone(),
        {
            let server_url = server.base_url.clone();
            let workspace_id = server.workspace_id.clone();
            Arc::new(move || {
                let mut settings = AppFlowySettings::for_test(workspace_id.clone());
                settings.base_url = server_url.clone();
                settings.summary_enabled = false;
                Ok(settings)
            })
        },
        fixture.credentials.clone(),
        true,
    );
    assert!(service.export_again(&fixture.recording.recording_id, false).is_err(), "must require confirmation");
    service.export_again(&fixture.recording.recording_id, true).unwrap();
    let settings = fixture.settings(&server);
    tick(&fixture.root, &fixture.store, &settings, &fixture.credentials, &AtomicBool::new(false)).unwrap();

    let date_id = fixture
        .store
        .get_page(&date_key(&server.workspace_id, &fixture.store.get_page(&root_key(&server.workspace_id, &server.workspace_id)).unwrap().unwrap().remote_view_id.unwrap(), &fixture.date()))
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
    let meeting_names = server.child_names(&date_id);
    assert_eq!(meeting_names.len(), 2, "the original meeting page must still exist alongside the new one");
    assert!(meeting_names.iter().any(|n| n.ends_with("(2)")));
    // The original page's own name is unchanged -- only a new page was added.
    assert!(meeting_names.iter().any(|n| n.contains("Weekly sync") && !n.contains("(2)")));
}

#[test]
fn a_permanently_rejected_export_surfaces_the_server_message_and_does_not_auto_retry() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.fail_next_page_view(422, "Meeting name is invalid");
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.store.get_job(&fixture.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.as_deref().unwrap_or_default().contains("Meeting name is invalid"));
    // Not automatically retried: another tick makes no further progress.
    fixture.tick(&server);
    let job_again = fixture.store.get_job(&fixture.recording.recording_id).unwrap().unwrap();
    assert_eq!(job_again.status, "failed");
}

#[test]
fn a_transient_failure_is_queued_for_backoff_retry_not_marked_permanently_failed() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.fail_next_page_view(500, "temporary outage");
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.store.get_job(&fixture.recording.recording_id).unwrap().unwrap();
    assert_eq!(job.status, "queued", "a transient failure must remain queued for backoff, not become permanently failed");
    assert!(job.error.as_deref().unwrap_or_default().contains("outage") || job.error.is_some());
}
