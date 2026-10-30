use super::*;
use crate::{
    appflowy_credentials::FakeCredentialStore, appflowy_fake_server::FakeAppFlowyServer,
    recording::Track, transcription_types::TranscriptSegment,
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
        let root =
            std::env::temp_dir().join(format!("meeting-appflowy-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = AppFlowyStore::open(&root.join("appflowy.sqlite3")).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording = Self::seed_recording(
            &recordings,
            &registry,
            &dir,
            &id,
            "Weekly sync",
            "Let's kick off.",
            "Sounds good.",
        );
        let credentials: SharedCredentialStore = Arc::new(FakeCredentialStore::new());
        Self {
            root,
            store,
            recordings,
            recording,
            credentials,
        }
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
                TranscriptSegment {
                    start_ms: 0,
                    end_ms: 1000,
                    text: line1.into(),
                    source_id: "mic".into(),
                    source_name: "Microphone".into(),
                },
                TranscriptSegment {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: line2.into(),
                    source_id: "mic".into(),
                    source_name: "Microphone".into(),
                },
            ],
        };
        let json = serde_json::to_vec_pretty(&transcript).unwrap();
        fs::write(dir.join("transcript.json"), &json).unwrap();
        registry
            .enqueue(id, "base", &transcript.model_hash)
            .unwrap();
        assert!(registry
            .claim(id, "unused-fake-whisper-model", &transcript.model_hash)
            .unwrap());
        registry
            .complete(
                id,
                &dir.join("transcript.json").display().to_string(),
                &dir.join("transcript.md").display().to_string(),
                &checksum(&json),
            )
            .unwrap();
        recording
    }
    fn add_second_meeting(&self, title: &str) -> Recording {
        let registry = TranscriptionStore::open(&self.root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        fs::create_dir(&dir).unwrap();
        Self::seed_recording(
            &self.recordings,
            &registry,
            &dir,
            &id,
            title,
            "Second meeting starts.",
            "Great, let's go.",
        )
    }
    fn sign_in(&self) {
        self.credentials
            .set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1")
            .unwrap();
        self.credentials
            .set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1")
            .unwrap();
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
        tick(
            &self.root,
            &self.store,
            &settings,
            &self.credentials,
            &AtomicBool::new(false),
        )
        .unwrap();
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

    let posts = server
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/page-view"))
        .count();
    assert_eq!(posts, 3, "root, date, and meeting pages are each one POST");
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "completed");

    // "My daily meetings" must land inside the default Space, never
    // directly under the workspace root -- that's the orphaned-page bug
    // this fix closes (AppFlowy's sidebar only shows Spaces and their
    // descendants).
    assert_eq!(
        server.child_names(&server.workspace_id),
        vec!["General".to_string()],
        "nothing should be created directly under the workspace root"
    );
    let root_names = server.child_names(&server.space_id);
    assert_eq!(root_names, vec!["My daily meetings".to_string()]);
    let root_id = fixture
        .store
        .get_page(&root_key(&server.workspace_id, &server.space_id))
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
    let date_names = server.child_names(&root_id);
    assert_eq!(date_names, vec![fixture.date()]);
    let date_page = fixture
        .store
        .get_page(&date_key(&server.workspace_id, &root_id, &fixture.date()))
        .unwrap()
        .unwrap();
    let meeting_names = server.child_names(date_page.remote_view_id.as_deref().unwrap());
    assert_eq!(meeting_names.len(), 1);
    assert!(meeting_names[0].contains("Weekly sync"));

    // The meeting page's content schema-checks against the native block tree.
    let meeting_key_str = meeting_key(&fixture.recording.recording_id, 1);
    let meeting_remote_id = fixture
        .store
        .get_page(&meeting_key_str)
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
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
    fixture
        .store
        .enqueue(&fixture.recording.recording_id)
        .unwrap();
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();

    let meeting_remote_id = fixture
        .store
        .get_page(&meeting_key(&fixture.recording.recording_id, 1))
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
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

    assert_eq!(
        posts_after_second - posts_after_first,
        1,
        "only the new meeting page should be created"
    );
    let root_id = fixture
        .store
        .get_page(&root_key(&server.workspace_id, &server.space_id))
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
    assert_eq!(
        server.child_names(&root_id),
        vec![fixture.date()],
        "no duplicate date page"
    );
}

#[test]
fn an_existing_daily_meetings_page_is_found_and_reused() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start_with_existing_top_level_page("My daily meetings");
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    // Only 2 POSTs: date + meeting -- the root page was never created.
    assert_eq!(server.requests_matching("POST", "/page-view"), 2);
    assert_eq!(
        server.child_names(&server.space_id),
        vec!["My daily meetings".to_string()],
        "no second root page appeared"
    );
}

#[test]
fn crash_between_page_creation_and_journal_update_is_recovered_on_retry() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    // Simulate: the root page was already created on the server in a
    // previous run, but the process crashed before `mark_created` ran, so
    // the local journal still shows "creating" with no remote id.
    fixture
        .store
        .find_or_plan(
            &root_key(&server.workspace_id, &server.space_id),
            "root",
            None,
            &server.workspace_id,
            "My daily meetings",
        )
        .unwrap();
    server.seed_existing_page(
        &server.space_id,
        "root-created-before-crash",
        "My daily meetings",
    );

    fixture.export_recording(&server, &fixture.recording.recording_id);

    // The retry must have adopted the existing page rather than creating a
    // second one: only 2 POSTs (date + meeting), and the journal now points
    // at the pre-existing remote id.
    assert_eq!(server.requests_matching("POST", "/page-view"), 2);
    assert_eq!(
        server.child_names(&server.space_id),
        vec!["My daily meetings".to_string()]
    );
    let root_record = fixture
        .store
        .get_page(&root_key(&server.workspace_id, &server.space_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        root_record.remote_view_id.as_deref(),
        Some("root-created-before-crash")
    );
}

#[test]
fn stale_orphan_root_keyed_under_the_workspace_root_is_never_reused() {
    // Simulates a journal left over from the pre-fix version of this
    // integration, which planned/created "My daily meetings" directly under
    // the workspace root (parent = workspace id) -- exactly the orphaned
    // page from the real bug this fix closes. That row's key embeds the old
    // parent, so it is a different key from the one the fixed
    // `resolve_root_parent` now plans under (parent = the Space): the stale
    // row must be left untouched and a fresh root created under the Space.
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let stale_key = root_key(&server.workspace_id, &server.workspace_id);
    fixture
        .store
        .find_or_plan(
            &stale_key,
            "root",
            None,
            &server.workspace_id,
            "My daily meetings",
        )
        .unwrap();
    server.seed_existing_page(
        &server.workspace_id,
        "orphaned-under-workspace-root",
        "My daily meetings",
    );

    fixture.export_recording(&server, &fixture.recording.recording_id);

    // A brand-new root was created under the Space -- 3 POSTs, not 2.
    assert_eq!(server.requests_matching("POST", "/page-view"), 3);
    assert_eq!(
        server.child_names(&server.space_id),
        vec!["My daily meetings".to_string()],
        "the new root must be visible under the Space"
    );
    // The pre-existing orphaned page is left exactly where it was (never
    // touched, moved, or deleted) -- only "General" is a genuinely new
    // sibling from server setup, and no *new* page was added here.
    assert_eq!(
        server.child_names(&server.workspace_id),
        vec!["General".to_string(), "My daily meetings".to_string()]
    );
    let fresh_root = fixture
        .store
        .get_page(&root_key(&server.workspace_id, &server.space_id))
        .unwrap()
        .unwrap();
    assert_ne!(
        fresh_root.remote_view_id.as_deref(),
        Some("orphaned-under-workspace-root"),
        "the fresh root must not adopt the orphaned page"
    );
    let stale = fixture.store.get_page(&stale_key).unwrap().unwrap();
    assert_eq!(
        stale.status, "creating",
        "the stale orphan row is left exactly as it was -- never touched, never adopted"
    );
}

#[test]
fn explicit_parent_view_id_is_always_honoured_even_with_spaces_present() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start_with_spaces(&["General", "Engineering"]);
    fixture.sign_in();
    let mut settings = fixture.settings(&server);
    settings.parent_view_id = Some("space-1".to_string()); // "Engineering", not the default
    fixture
        .store
        .enqueue(&fixture.recording.recording_id)
        .unwrap();
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(
        server.child_names("space-1"),
        vec!["My daily meetings".to_string()]
    );
    assert!(
        server.child_names(&server.space_id).is_empty(),
        "the default Space must be untouched"
    );
}

#[test]
fn first_space_is_used_when_no_space_is_named_general() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start_with_spaces(&["Marketing", "Engineering"]);
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(
        server.child_names("space-0"),
        vec!["My daily meetings".to_string()],
        "Marketing (first by order) is picked, since none is named General"
    );
}

#[test]
fn a_workspace_with_no_space_fails_the_job_with_a_clear_message() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start_with_no_space();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "failed");
    assert!(
        job.error
            .as_deref()
            .unwrap_or_default()
            .contains("No space found"),
        "{:?}",
        job.error
    );
    assert!(
        server.requests_matching("POST", "/page-view") == 0,
        "nothing should ever be created without a resolved space"
    );
}

#[test]
fn a_page_the_server_does_not_list_after_creating_fails_the_job_instead_of_reporting_success() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.omit_created_children();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        job.status, "failed",
        "the export must never report success for a page AppFlowy doesn't actually list"
    );
    assert!(
        job.error
            .as_deref()
            .unwrap_or_default()
            .contains("not visible"),
        "{:?}",
        job.error
    );
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
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .is_none());
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
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
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
    let error = service
        .export_now(&fixture.recording.recording_id)
        .unwrap_err();
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
    assert!(
        service
            .export_again(&fixture.recording.recording_id, false)
            .is_err(),
        "must require confirmation"
    );
    service
        .export_again(&fixture.recording.recording_id, true)
        .unwrap();
    let settings = fixture.settings(&server);
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();

    let date_id = fixture
        .store
        .get_page(&date_key(
            &server.workspace_id,
            &fixture
                .store
                .get_page(&root_key(&server.workspace_id, &server.space_id))
                .unwrap()
                .unwrap()
                .remote_view_id
                .unwrap(),
            &fixture.date(),
        ))
        .unwrap()
        .unwrap()
        .remote_view_id
        .unwrap();
    let meeting_names = server.child_names(&date_id);
    assert_eq!(
        meeting_names.len(),
        2,
        "the original meeting page must still exist alongside the new one"
    );
    assert!(meeting_names.iter().any(|n| n.ends_with("(2)")));
    // The original page's own name is unchanged -- only a new page was added.
    assert!(meeting_names
        .iter()
        .any(|n| n.contains("Weekly sync") && !n.contains("(2)")));
}

#[test]
fn a_permanently_rejected_export_surfaces_the_server_message_and_does_not_auto_retry() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.fail_next_page_view(422, "Meeting name is invalid");
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "failed");
    assert!(job
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Meeting name is invalid"));
    // Not automatically retried: another tick makes no further progress.
    fixture.tick(&server);
    let job_again = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job_again.status, "failed");
}

#[test]
fn a_transient_failure_is_queued_for_backoff_retry_not_marked_permanently_failed() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.fail_next_page_view(500, "temporary outage");
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        job.status, "queued",
        "a transient failure must remain queued for backoff, not become permanently failed"
    );
    assert!(job.error.as_deref().unwrap_or_default().contains("outage") || job.error.is_some());
}

#[test]
fn optional_diarization_never_blocks_auto_export_forever() {
    assert!(!super::diarization_settled(None));
    assert!(!super::diarization_settled(Some("queued")));
    assert!(!super::diarization_settled(Some("diarizing")));
    assert!(super::diarization_settled(Some("completed")));
    assert!(super::diarization_settled(Some("failed")));
    assert!(super::diarization_settled(Some("waiting_models")));
}

#[test]
fn a_failed_summary_exports_the_transcript_alone() {
    assert!(!super::summary_settled(None));
    assert!(!super::summary_settled(Some("queued")));
    assert!(!super::summary_settled(Some("summarizing")));
    assert!(!super::summary_settled(Some("waiting_ollama")));
    assert!(super::summary_settled(Some("completed")));
    assert!(super::summary_settled(Some("failed")));
}

#[test]
fn a_legacy_root_row_is_never_adopted_and_a_fresh_working_root_is_created() {
    // Reproduces the shape of the user's real, broken account: "My daily
    // meetings" was created by a version predating the `collab_id` fix
    // (generation 1), and a same-named page really exists in AppFlowy --
    // exactly what a name-match adoption would otherwise pick up. It must be
    // left completely alone, and a fresh, working root created instead. The
    // same mechanism (`AppFlowyStore::find_or_plan` refusing to hand back a
    // legacy row, `is_legacy_view_id` excluding it from name adoption) is
    // shared by the date and meeting levels -- see
    // `appflowy_store_tests.rs` for generic, non-root-specific proof of it.
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let root_key_str = root_key(&server.workspace_id, &server.space_id);
    fixture
        .store
        .insert_legacy_row(
            &root_key_str,
            "root",
            &server.workspace_id,
            "My daily meetings",
            "legacy-root-id",
            1,
        )
        .unwrap();
    server.seed_existing_page(&server.space_id, "legacy-root-id", "My daily meetings");

    fixture.export_recording(&server, &fixture.recording.recording_id);

    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "completed", "{:?}", job.error);
    // Fresh root + date + meeting: 3 creates, none of them reusing the
    // legacy root id.
    assert_eq!(server.requests_matching("POST", "/page-view"), 3);
    let space_children = server.child_names(&server.space_id);
    assert_eq!(
        space_children,
        vec![
            "My daily meetings".to_string(),
            "My daily meetings".to_string()
        ],
        "the legacy root is left in place; a second, working root is created alongside it"
    );

    let legacy_row = fixture.store.get_page(&root_key_str).unwrap().unwrap();
    assert_eq!(
        legacy_row.remote_view_id.as_deref(),
        Some("legacy-root-id"),
        "the legacy row itself must be untouched"
    );
    assert_eq!(
        legacy_row.status, "created",
        "never touched, moved, or re-marked"
    );

    let all_pages = fixture.store.all_pages().unwrap();
    let fresh_root_row = all_pages
        .iter()
        .find(|p| p.kind == "root" && p.remote_view_id.as_deref() != Some("legacy-root-id"))
        .unwrap();
    assert!(!fresh_root_row.is_legacy());
    assert_ne!(
        fresh_root_row.key, root_key_str,
        "the fresh root must be planned under a different local key"
    );
}

#[test]
fn a_name_matched_page_whose_content_fails_to_load_is_skipped_in_favor_of_a_fresh_page() {
    // A page named "My daily meetings" really exists under the Space (not
    // tracked in our own journal at all -- e.g. left over from some other
    // cause), but its document collab is missing, so opening it in AppFlowy
    // would show "Record not found". Adoption-by-name must not trust it.
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.seed_broken_existing_page(&server.space_id, "broken-root-id", "My daily meetings");

    fixture.export_recording(&server, &fixture.recording.recording_id);

    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "completed", "{:?}", job.error);
    assert_eq!(
        server.requests_matching("POST", "/page-view"),
        3,
        "root/date/meeting all created fresh rather than adopting the broken page"
    );
    let space_children = server.child_names(&server.space_id);
    assert_eq!(
        space_children.len(),
        2,
        "the broken page is left in place alongside a fresh, working duplicate"
    );
    assert!(space_children
        .iter()
        .all(|n| n.as_str() == "My daily meetings"));
    let root_record = fixture
        .store
        .get_page(&root_key(&server.workspace_id, &server.space_id))
        .unwrap()
        .unwrap();
    assert_ne!(
        root_record.remote_view_id.as_deref(),
        Some("broken-root-id"),
        "the broken page must never be adopted"
    );
}

#[test]
fn a_page_whose_content_fails_to_load_right_after_creation_fails_the_job() {
    // Guards the "Record not found: Collab not found" bug even for a
    // brand-new page: creating and listing it successfully is not enough --
    // its content must actually open before the export is reported as
    // completed.
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    server.force_missing_collab();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        job.status, "failed",
        "the export must never report success for a page whose content does not load"
    );
    assert!(
        job.error
            .as_deref()
            .unwrap_or_default()
            .contains("content failed to load"),
        "{:?}",
        job.error
    );
}

#[test]
fn auto_export_proceeds_when_speaker_models_are_not_imported() {
    let fixture = Fixture::new();
    let server = FakeAppFlowyServer::start();
    fixture.sign_in();
    let mut recording = fixture.recording.clone();
    let mut loopback = recording.tracks[0].clone();
    loopback.source_id = "speakers".into();
    loopback.name = "Speakers".into();
    loopback.direction = "output".into();
    recording.tracks.push(loopback);
    fixture.recordings.save(&recording).unwrap();
    let diarization = DiarizationStore::open(&fixture.root.join("diarization.sqlite3")).unwrap();
    diarization
        .enqueue(&recording.recording_id, &"b".repeat(64))
        .unwrap();
    let mut settings = fixture.settings(&server);
    settings.auto_export = true;
    settings.diarization_enabled = true;
    // Still queued: a job that can finish is worth waiting for.
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(fixture
        .store
        .get_job(&recording.recording_id)
        .unwrap()
        .is_none());
    // Models never imported: waiting forever would hide the meeting, so export without labels.
    diarization
        .status(&recording.recording_id, "waiting_models", 0, None)
        .unwrap();
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    tick(
        &fixture.root,
        &fixture.store,
        &settings,
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        fixture
            .store
            .get_job(&recording.recording_id)
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
}
