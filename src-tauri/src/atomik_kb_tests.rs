use super::*;
use crate::{
    appflowy_credentials::FakeCredentialStore,
    atomik_kb_credentials::{FAKE_TOKEN, KEY_TOKEN},
    atomik_kb_fake_server::FakeAtomikKbServer,
    recording::Track,
    transcription::checksum,
    transcription_types::{Transcript, TranscriptSegment},
};

struct Fixture {
    root: PathBuf,
    store: AtomikKbStore,
    recordings: RecordingStore,
    recording: Recording,
    credentials: SharedCredentialStore,
}
impl Fixture {
    fn new() -> Self {
        Self::with_content("Weekly sync", "Let's kick off.", "Sounds good.")
    }
    fn with_content(title: &str, line1: &str, line2: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("meeting-atomik-kb-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = AtomikKbStore::open(&root.join("atomik_kb.sqlite3")).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let registry = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        fs::create_dir(&dir).unwrap();
        let recording =
            Self::seed_recording(&recordings, &registry, &dir, &id, title, line1, line2);
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
        self.credentials.set(KEY_TOKEN, FAKE_TOKEN).unwrap();
    }
    fn settings(&self, server: &FakeAtomikKbServer) -> AtomikKbSettings {
        let mut settings = AtomikKbSettings::for_test(server.port);
        // The fixture does not seed a completed summary job; the export must
        // still succeed from the transcript alone -- see `is_ready_for_export`.
        settings.summary_enabled = false;
        settings
    }
    fn tick_with(&self, settings: &AtomikKbSettings) {
        tick(
            &self.root,
            &self.store,
            settings,
            &self.credentials,
            &AtomicBool::new(false),
        )
        .unwrap();
    }
    fn tick(&self, server: &FakeAtomikKbServer) {
        self.tick_with(&self.settings(server));
    }
    fn export_recording(&self, server: &FakeAtomikKbServer, recording_id: &str) {
        self.store.enqueue(recording_id).unwrap();
        self.tick(server);
    }
    fn job(&self) -> AtomikKbJob {
        self.store
            .get_job(&self.recording.recording_id)
            .unwrap()
            .unwrap()
    }
    fn date(&self) -> String {
        local_date_string(self.recording.started_at)
    }
    fn service(
        &self,
        server: &FakeAtomikKbServer,
        enabled: bool,
        auto_export: bool,
    ) -> AtomikKbService {
        let port = server.port;
        AtomikKbService::with_inhibition(
            self.root.clone(),
            Arc::new(move || {
                let mut settings = AtomikKbSettings::for_test(port);
                settings.enabled = enabled;
                settings.auto_export = auto_export;
                settings.summary_enabled = false;
                Ok(settings)
            }),
            self.credentials.clone(),
            true,
        )
    }
}

/// Create-only, structurally: whatever happened, only the three allowlisted
/// tools were ever invoked, and never append/read/search.
fn assert_only_allowed_tools(server: &FakeAtomikKbServer) {
    for call in server.calls() {
        if let Some(tool) = &call.tool {
            assert!(
                ["list_spaces", "list_pages", "create_page"].contains(&tool.as_str()),
                "forbidden tool reached the server: {tool}"
            );
        } else {
            assert!(
                ["initialize", "notifications/initialized"].contains(&call.rpc_method.as_str()),
                "{call:?}"
            );
        }
    }
}

fn root_id(fixture: &Fixture, server: &FakeAtomikKbServer) -> String {
    fixture
        .store
        .get_page(&root_key(&server.space_id))
        .unwrap()
        .unwrap()
        .remote_page_id
        .unwrap()
}
fn date_page_id(fixture: &Fixture, server: &FakeAtomikKbServer) -> String {
    let root = root_id(fixture, server);
    fixture
        .store
        .get_page(&date_key(&root, &fixture.date()))
        .unwrap()
        .unwrap()
        .remote_page_id
        .unwrap()
}

#[test]
fn first_export_creates_root_date_and_meeting_with_exactly_three_creates() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    assert_eq!(
        server.creates(),
        3,
        "root, date, and meeting pages are each one create_page"
    );
    assert_eq!(
        server.child_titles(None),
        vec!["My daily meetings".to_string()],
        "the root lands at the top level of the space"
    );
    let root = root_id(&fixture, &server);
    assert_eq!(server.child_titles(Some(&root)), vec![fixture.date()]);
    let date = date_page_id(&fixture, &server);
    let meetings = server.child_titles(Some(&date));
    assert_eq!(meetings.len(), 1);
    assert!(
        meetings[0].ends_with("— Weekly sync") && meetings[0].len() > "— Weekly sync".len(),
        "{meetings:?}"
    );
    // `HH:mm — Title`
    assert_eq!(meetings[0].chars().nth(2), Some(':'));

    let meeting_page = fixture
        .store
        .get_page(&meeting_key(&fixture.recording.recording_id, 1))
        .unwrap()
        .unwrap();
    let markdown = server
        .markdown_of(meeting_page.remote_page_id.as_deref().unwrap())
        .unwrap();
    assert!(markdown.contains("**Weekly sync**"), "{markdown}");
    assert!(markdown.contains("## Transcript"));
    assert!(markdown.contains("> **00:00:00 · You:** Let's kick off."));
    assert!(markdown.contains("> **00:00:01 · You:** Sounds good."));
    assert_only_allowed_tools(&server);
}

#[test]
fn the_export_sends_initialize_before_any_tool_call_and_only_to_loopback() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let calls = server.calls();
    assert_eq!(calls[0].rpc_method, "initialize");
    assert_eq!(calls[1].rpc_method, "notifications/initialized");
    assert!(calls[2..].iter().all(|c| c.rpc_method == "tools/call"));
    assert!(calls
        .iter()
        .all(|c| c.authorized && c.host == format!("127.0.0.1:{}", server.port)));
}

#[test]
fn second_meeting_same_day_only_creates_the_meeting_page() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let after_first = server.creates();

    let second = fixture.add_second_meeting("Design review");
    fixture.export_recording(&server, &second.recording_id);

    assert_eq!(
        server.creates() - after_first,
        1,
        "only the new meeting page should be created"
    );
    let root = root_id(&fixture, &server);
    assert_eq!(
        server.child_titles(Some(&root)),
        vec![fixture.date()],
        "no duplicate date page"
    );
    assert_eq!(
        server
            .child_titles(Some(&date_page_id(&fixture, &server)))
            .len(),
        2
    );
}

#[test]
fn an_existing_daily_meetings_page_is_found_and_reused() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    let existing = server.seed_page(None, "My daily meetings");
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);

    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    assert_eq!(
        server.creates(),
        2,
        "only date + meeting; the root was adopted"
    );
    assert_eq!(
        server.child_titles(None),
        vec!["My daily meetings".to_string()],
        "no second root page appeared"
    );
    assert_eq!(root_id(&fixture, &server), existing);
}

#[test]
fn crash_between_page_creation_and_journal_update_is_recovered_on_retry() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    // Simulate: the root page was already created in a previous run, but the
    // process crashed before `mark_created`, so the journal shows "creating".
    fixture
        .store
        .find_or_plan(
            &root_key(&server.space_id),
            "root",
            None,
            &server.space_id,
            ROOT_PAGE_TITLE,
        )
        .unwrap();
    let created_before_crash = server.seed_page(None, "My daily meetings");

    fixture.export_recording(&server, &fixture.recording.recording_id);

    assert_eq!(
        server.creates(),
        2,
        "the retry adopts the existing page instead of creating a second one"
    );
    assert_eq!(
        server.child_titles(None),
        vec!["My daily meetings".to_string()]
    );
    assert_eq!(root_id(&fixture, &server), created_before_crash);
}

#[test]
fn a_meeting_page_created_before_a_crash_is_adopted_not_duplicated() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let date = date_page_id(&fixture, &server);
    assert_eq!(server.creates(), 3);
    // Crash after the remote create but before the journal row was updated:
    // wipe the meeting's journal link by recreating the store without it.
    let meeting_title = server.child_titles(Some(&date)).remove(0);
    let fresh = AtomikKbStore::open(&fixture.root.join("fresh.sqlite3")).unwrap();
    let root = root_id(&fixture, &server);
    fresh
        .find_or_plan(
            &root_key(&server.space_id),
            "root",
            None,
            &server.space_id,
            ROOT_PAGE_TITLE,
        )
        .unwrap();
    fresh
        .mark_created(&root_key(&server.space_id), &root)
        .unwrap();
    fresh
        .find_or_plan(
            &date_key(&root, &fixture.date()),
            "date",
            None,
            &server.space_id,
            &fixture.date(),
        )
        .unwrap();
    fresh
        .mark_created(&date_key(&root, &fixture.date()), &date)
        .unwrap();
    fresh.enqueue(&fixture.recording.recording_id).unwrap();
    tick(
        &fixture.root,
        &fresh,
        &fixture.settings(&server),
        &fixture.credentials,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        fresh
            .get_job(&fixture.recording.recording_id)
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        server.creates(),
        3,
        "adopted by exact title; nothing new created"
    );
    assert_eq!(server.child_titles(Some(&date)), vec![meeting_title]);
}

#[test]
fn re_running_a_finished_export_creates_nothing_new() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(server.creates(), 3);
    // Re-run: the job is queued again (as after a crash right before it was
    // marked completed) -- every page is already in the journal.
    fixture
        .store
        .force_status(&fixture.recording.recording_id, "queued")
        .unwrap();
    fixture.tick(&server);
    assert_eq!(fixture.job().status, "completed");
    assert_eq!(server.creates(), 3, "no duplicate pages on a re-run");
    // And a plain re-enqueue of a completed job is a no-op.
    fixture
        .store
        .enqueue(&fixture.recording.recording_id)
        .unwrap();
    fixture.tick(&server);
    assert_eq!(server.creates(), 3);
    assert_only_allowed_tools(&server);
}

#[test]
fn two_meetings_with_the_same_title_and_minute_get_two_pages() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let twin = fixture.add_second_meeting("Weekly sync");
    fixture.export_recording(&server, &twin.recording_id);
    let titles = server.child_titles(Some(&date_page_id(&fixture, &server)));
    assert_eq!(
        titles.len(),
        2,
        "the second meeting must not adopt the first one's page: {titles:?}"
    );
    assert_eq!(titles[0], titles[1]);
}

#[test]
fn a_space_that_is_not_shared_fails_the_job_and_creates_nothing() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start_with_space("Some Other Space", true);
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(job.status, "failed");
    let error = job.error.unwrap();
    assert!(
        error.contains("No space named \"My Daily Meeting\""),
        "{error}"
    );
    assert!(
        error.contains("Some Other Space"),
        "lists what it can see: {error}"
    );
    assert_eq!(
        server.creates(),
        0,
        "never falls back to another space, never creates one"
    );
    assert_only_allowed_tools(&server);
}

#[test]
fn space_names_match_case_insensitively_and_exactly() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start_with_space("my daily MEETING", true);
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );

    let partial = Fixture::new();
    let server = FakeAtomikKbServer::start_with_space("My Daily Meeting Archive", true);
    partial.sign_in();
    partial.export_recording(&server, &partial.recording.recording_id);
    assert_eq!(
        partial.job().status,
        "failed",
        "a partial name is not a match"
    );
    assert_eq!(server.creates(), 0);
}

#[test]
fn when_two_spaces_share_the_name_the_writable_one_is_used() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start_with_space("My Daily Meeting", false);
    let writable = server.add_space("my daily meeting", true);
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    assert!(
        fixture
            .store
            .get_page(&root_key(&writable))
            .unwrap()
            .is_some(),
        "journaled under the writable space"
    );
    assert!(fixture
        .store
        .get_page(&root_key(&server.space_id))
        .unwrap()
        .is_none());
}

#[test]
fn a_space_with_no_grant_at_all_is_reported_as_not_found() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start_with_no_space();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(fixture.job().status, "failed");
    assert!(fixture
        .job()
        .error
        .unwrap()
        .contains("grant this client write access"));
    assert_eq!(server.creates(), 0);
}

#[test]
fn a_read_only_space_fails_the_job_with_the_grant_instructions() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start_with_space("My Daily Meeting", false);
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(job.status, "failed");
    let error = job.error.unwrap();
    assert!(
        error.contains("read-only") && error.contains("Settings → MCP"),
        "{error}"
    );
    assert_eq!(
        server.creates(),
        0,
        "never even attempts a write it knows is denied"
    );
}

#[test]
fn a_tool_level_denial_on_create_fails_the_job_permanently_with_a_grant_hint() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    // The listing says writable, but the grant was revoked in between.
    server.fail_next_create_with_tool_error("This client has no write access to that space");
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(job.status, "failed");
    assert!(
        job.error
            .as_deref()
            .unwrap()
            .contains("grant this client write access"),
        "{:?}",
        job.error
    );
    // Not automatically retried.
    fixture.tick(&server);
    assert_eq!(fixture.job().status, "failed");
}

#[test]
fn a_refused_token_is_permanent_until_fixed_and_never_echoes_the_token() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture
        .credentials
        .set(KEY_TOKEN, "kbm_wrong_token_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .unwrap();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(
        job.status, "failed",
        "a 401 must not be retried with backoff"
    );
    let error = job.error.unwrap();
    assert!(error.contains("refused the token"), "{error}");
    assert!(!error.contains("kbm_"), "{error}");
    // Fixing the token and retrying works.
    fixture.sign_in();
    fixture
        .store
        .retry(&fixture.recording.recording_id)
        .unwrap();
    fixture.tick(&server);
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
}

#[test]
fn a_missing_token_fails_permanently_with_setup_instructions() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(job.status, "failed");
    assert!(job.error.unwrap().contains("No atomik-kb token"));
    assert!(server.calls().is_empty());
}

#[test]
fn a_closed_atomik_kb_is_a_transient_failure_retried_with_backoff() {
    let fixture = Fixture::new();
    let port = FakeAtomikKbServer::start().port; // dropped immediately: nothing listens
    fixture.sign_in();
    let mut settings = AtomikKbSettings::for_test(port);
    settings.summary_enabled = false;
    fixture
        .store
        .enqueue(&fixture.recording.recording_id)
        .unwrap();
    fixture.tick_with(&settings);
    let job = fixture.job();
    assert_eq!(
        job.status, "queued",
        "connection refused must stay queued for retry, not become permanently failed"
    );
    assert!(
        job.error.as_deref().unwrap().contains("not reachable"),
        "{:?}",
        job.error
    );
    assert_eq!(job.attempts, 1);
    // Backoff: not immediately due again.
    assert!(fixture.store.next().unwrap().is_none());
}

#[test]
fn a_transient_server_error_is_queued_and_a_permanent_rejection_is_not_retried() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    server.fail_next_create_with_http(500);
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(fixture.job().status, "queued");

    let rejected = Fixture::new();
    let server = FakeAtomikKbServer::start();
    rejected.sign_in();
    server.fail_next_create_with_http(422);
    rejected.export_recording(&server, &rejected.recording.recording_id);
    assert_eq!(rejected.job().status, "failed");
    assert!(rejected.job().error.unwrap().contains("422"));
    rejected.tick(&server);
    assert_eq!(rejected.job().status, "failed");
}

#[test]
fn a_page_the_server_does_not_list_after_creating_fails_the_job() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    server.omit_created_children();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let job = fixture.job();
    assert_eq!(
        job.status, "failed",
        "never report success for a page atomik-kb doesn't list"
    );
    assert!(
        job.error.as_deref().unwrap().contains("not visible"),
        "{:?}",
        job.error
    );
    assert_eq!(
        fixture
            .store
            .get_page(&root_key(&server.space_id))
            .unwrap()
            .unwrap()
            .status,
        "creating",
        "an unverified page is never journaled as created"
    );
}

#[test]
fn a_large_parent_whose_listing_is_cut_still_verifies_a_new_page() {
    // atomik-kb lists at most 200 children. With the listing capped, the new
    // date page falls past the cut; it is confirmed through its own children
    // instead of failing a legitimate export.
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let root = server.seed_page(None, "My daily meetings");
    for day in 1..=5 {
        server.seed_page(Some(&root), &format!("2020-01-0{day}"));
    }
    server.cap_list_pages(2);
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    assert_eq!(server.creates(), 2, "date + meeting");
}

#[test]
fn export_again_requires_confirmation_and_creates_a_new_suffixed_page() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let original = fixture
        .store
        .get_page(&meeting_key(&fixture.recording.recording_id, 1))
        .unwrap()
        .unwrap()
        .remote_page_id
        .unwrap();
    let original_markdown = server.markdown_of(&original).unwrap();

    let service = fixture.service(&server, true, false);
    assert!(
        service
            .export_again(&fixture.recording.recording_id, false)
            .is_err(),
        "must require confirmation"
    );
    assert_eq!(
        fixture.job().export_index,
        1,
        "a refused request changes nothing"
    );
    service
        .export_again(&fixture.recording.recording_id, true)
        .unwrap();
    fixture.tick(&server);

    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    let titles = server.child_titles(Some(&date_page_id(&fixture, &server)));
    assert_eq!(
        titles.len(),
        2,
        "the original meeting page must still exist alongside the new one"
    );
    assert!(titles[1].ends_with("(2)") && !titles[0].ends_with("(2)"));
    assert_eq!(
        server.markdown_of(&original).unwrap(),
        original_markdown,
        "the original page's content is untouched"
    );
}

#[test]
fn export_again_twice_adds_suffix_2_then_3_and_never_touches_earlier_pages() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let date = date_page_id(&fixture, &server);
    let original_ids = server.child_ids(Some(&date));
    let service = fixture.service(&server, true, false);

    service
        .export_again(&fixture.recording.recording_id, true)
        .unwrap();
    fixture.tick(&server);
    service
        .export_again(&fixture.recording.recording_id, true)
        .unwrap();
    fixture.tick(&server);

    let titles = server.child_titles(Some(&date));
    assert_eq!(titles.len(), 3, "{titles:?}");
    assert!(titles[0].ends_with("Weekly sync"));
    assert!(titles[1].ends_with("Weekly sync (2)"));
    assert!(titles[2].ends_with("Weekly sync (3)"));
    assert_eq!(
        &server.child_ids(Some(&date))[..1],
        &original_ids[..],
        "the original page is the same page, untouched"
    );
    assert_eq!(
        server.creates(),
        5,
        "root + date + 3 meeting pages; nothing else"
    );
    assert_only_allowed_tools(&server);
}

#[test]
fn a_long_title_is_cut_so_the_whole_page_title_fits_with_its_suffix() {
    let long = "Réunion ".repeat(60);
    for index in [1, 2, 12] {
        let title = meeting_page_title(1_726_500_000_000, &long, index);
        assert!(
            title.chars().count() <= 200,
            "{} chars",
            title.chars().count()
        );
        if index > 1 {
            assert!(title.ends_with(&format!(" ({index})")), "{title}");
        }
        assert!(title.contains(" — Réunion"));
    }
    let short = meeting_page_title(1_726_500_000_000, "Weekly\nsync", 1);
    assert!(short.ends_with("— Weekly sync"), "{short}");
}

#[test]
fn french_titles_and_transcripts_arrive_intact() {
    let fixture = Fixture::with_content(
        "Réunion d'équipe — décisions",
        "Très bien, on décide d'envoyer l'offre à 17 h 30 !",
        "D'accord. Ça marche : à demain, ça roule 🎉",
    );
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
    let date = date_page_id(&fixture, &server);
    assert!(server.child_titles(Some(&date))[0].ends_with("— Réunion d'équipe — décisions"));
    let page = fixture
        .store
        .get_page(&meeting_key(&fixture.recording.recording_id, 1))
        .unwrap()
        .unwrap();
    let markdown = server
        .markdown_of(page.remote_page_id.as_deref().unwrap())
        .unwrap();
    assert!(markdown.contains("**Réunion d'équipe — décisions**"));
    assert!(markdown.contains("Très bien, on décide d'envoyer l'offre à 17 h 30 !"));
    assert!(markdown.contains("D'accord. Ça marche : à demain, ça roule 🎉"));
}

#[test]
fn markdown_special_characters_in_a_transcript_are_escaped_in_the_sent_page() {
    let fixture = Fixture::with_content(
        "Sync",
        "# Not a heading\n- not a bullet",
        "Use *stars* and [links](http://x) and `ticks`.",
    );
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let page = fixture
        .store
        .get_page(&meeting_key(&fixture.recording.recording_id, 1))
        .unwrap()
        .unwrap();
    let markdown = server
        .markdown_of(page.remote_page_id.as_deref().unwrap())
        .unwrap();
    assert!(
        markdown.contains("\\# Not a heading - not a bullet"),
        "{markdown}"
    );
    assert!(
        markdown.contains("Use \\*stars\\* and \\[links\\](http://x) and \\`ticks\\`."),
        "{markdown}"
    );
}

#[test]
fn export_is_never_enqueued_automatically_when_auto_export_is_off() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let mut settings = fixture.settings(&server);
    settings.auto_export = false;
    fixture.tick_with(&settings);
    assert!(fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .is_none());
    assert!(
        server.calls().is_empty(),
        "nothing is sent unless exporting was requested"
    );
}

#[test]
fn auto_export_enqueues_a_ready_recording_and_exports_it() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.tick(&server); // settings(): auto_export on, summary off
    assert_eq!(
        fixture.job().status,
        "completed",
        "{:?}",
        fixture.job().error
    );
}

#[test]
fn auto_export_waits_for_the_summary_when_summaries_are_enabled() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let mut settings = fixture.settings(&server);
    settings.summary_enabled = true;
    fixture.tick_with(&settings);
    assert!(
        fixture
            .store
            .get_job(&fixture.recording.recording_id)
            .unwrap()
            .is_none(),
        "no summary job yet: wait"
    );
    let summaries = SummaryStore::open(&fixture.root.join("summary.sqlite3")).unwrap();
    summaries
        .enqueue(&fixture.recording.recording_id, &"c".repeat(64))
        .unwrap();
    fixture.tick_with(&settings);
    assert!(
        fixture
            .store
            .get_job(&fixture.recording.recording_id)
            .unwrap()
            .is_none(),
        "summary still queued: wait"
    );
    assert!(server.calls().is_empty());
}

#[test]
fn auto_export_proceeds_when_speaker_models_are_not_imported() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
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
    settings.diarization_enabled = true;
    fixture.tick_with(&settings);
    assert!(
        fixture
            .store
            .get_job(&recording.recording_id)
            .unwrap()
            .is_none(),
        "queued speaker identification: wait"
    );
    diarization
        .status(&recording.recording_id, "waiting_models", 0, None)
        .unwrap();
    fixture.tick_with(&settings);
    fixture.tick_with(&settings);
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

#[test]
fn manual_export_works_even_with_auto_export_off() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let service = fixture.service(&server, true, false);
    service.export_now(&fixture.recording.recording_id).unwrap();
    let mut settings = fixture.settings(&server);
    settings.auto_export = false;
    fixture.tick_with(&settings);
    assert_eq!(fixture.job().status, "completed");
}

#[test]
fn manual_export_and_export_again_are_refused_when_the_feature_is_disabled() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    let service = fixture.service(&server, false, false);
    let error = service
        .export_now(&fixture.recording.recording_id)
        .unwrap_err();
    assert!(error.contains("turned off"), "{error}");
    assert!(service
        .export_again(&fixture.recording.recording_id, true)
        .unwrap_err()
        .contains("turned off"));
    assert!(fixture
        .store
        .get_job(&fixture.recording.recording_id)
        .unwrap()
        .is_none());
}

#[test]
fn the_worker_never_runs_when_inhibited() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let service = fixture.service(&server, true, true); // inhibited = true
    assert!(service.state().inhibited);
    std::thread::sleep(Duration::from_millis(300));
    assert!(server.calls().is_empty());
}

#[test]
fn state_reports_token_presence_settings_and_jobs_but_never_the_token() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    let service = fixture.service(&server, true, false);
    assert!(!service.state().has_token);
    service.save_token(FAKE_TOKEN).unwrap();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    let state = service.state();
    assert!(state.has_token && state.enabled && !state.auto_export);
    assert_eq!(state.port, server.port);
    assert_eq!(state.space_name, "My Daily Meeting");
    assert_eq!(state.jobs.len(), 1);
    assert_eq!(state.jobs[0].status, "completed");
    let json = serde_json::to_string(&state).unwrap();
    assert!(
        !json.contains("kbm_"),
        "the token must never be serialised into state: {json}"
    );
    let pages = service.list_pages().unwrap();
    assert_eq!(
        pages.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(),
        vec!["root", "date", "meeting"]
    );
    assert!(pages
        .iter()
        .all(|p| p.status == "created" && p.page_id.is_some()));
    service.clear_token().unwrap();
    assert!(!service.state().has_token);
}

#[test]
fn saving_a_malformed_token_is_refused() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    let service = fixture.service(&server, true, false);
    assert!(service.save_token("not-a-token").is_err());
    assert!(!service.state().has_token);
}

#[test]
fn test_connection_reports_a_writable_space() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    let outcome = fixture
        .service(&server, true, false)
        .test_connection(None, None)
        .unwrap();
    assert_eq!(
        outcome,
        ConnectionTest {
            ok: true,
            message: "Connected. The space \"My Daily Meeting\" was found and is writable.".into(),
            space_found: true,
            writable: true
        }
    );
    assert_eq!(server.creates(), 0, "testing never writes");
    assert_only_allowed_tools(&server);
}

#[test]
fn test_connection_reports_each_precise_problem() {
    let fixture = Fixture::new();
    let service_for = |server: &FakeAtomikKbServer| fixture.service(server, true, false);

    let read_only = FakeAtomikKbServer::start_with_space("My Daily Meeting", false);
    fixture.sign_in();
    let outcome = service_for(&read_only).test_connection(None, None).unwrap();
    assert!(!outcome.ok && outcome.space_found && !outcome.writable);
    assert!(outcome.message.contains("read-only"), "{}", outcome.message);

    let other = FakeAtomikKbServer::start_with_space("Elsewhere", true);
    let outcome = service_for(&other).test_connection(None, None).unwrap();
    assert!(!outcome.ok && !outcome.space_found);
    assert!(
        outcome
            .message
            .contains("No space named \"My Daily Meeting\"")
            && outcome.message.contains("Elsewhere")
    );
    // The unsaved form value can be tested before saving.
    let outcome = service_for(&other)
        .test_connection(None, Some("elsewhere".into()))
        .unwrap();
    assert!(outcome.ok, "{}", outcome.message);

    fixture
        .credentials
        .set(KEY_TOKEN, "kbm_wrong_token_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .unwrap();
    let outcome = service_for(&other).test_connection(None, None).unwrap();
    assert!(
        !outcome.ok && outcome.message.contains("refused the token"),
        "{}",
        outcome.message
    );

    fixture.credentials.delete(KEY_TOKEN).unwrap();
    let outcome = service_for(&other).test_connection(None, None).unwrap();
    assert!(
        !outcome.ok && outcome.message.contains("No atomik-kb token"),
        "{}",
        outcome.message
    );

    fixture.sign_in();
    let closed_port = FakeAtomikKbServer::start().port;
    let outcome = service_for(&other)
        .test_connection(Some(closed_port), None)
        .unwrap();
    assert!(
        !outcome.ok && outcome.message.contains("not reachable"),
        "{}",
        outcome.message
    );
}

#[test]
fn test_connection_validates_its_overrides() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    let service = fixture.service(&server, true, false);
    assert!(service.test_connection(Some(0), None).is_err());
    assert!(service.test_connection(None, Some("  ".into())).is_err());
}

#[test]
fn the_token_is_never_written_to_the_journal_or_the_settings_it_is_derived_from() {
    let fixture = Fixture::new();
    let server = FakeAtomikKbServer::start();
    fixture.sign_in();
    fixture.export_recording(&server, &fixture.recording.recording_id);
    for entry in std::fs::read_dir(&fixture.root).unwrap().flatten() {
        let path = entry.path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(FAKE_TOKEN),
                "{} contains the token",
                path.display()
            );
        }
    }
}
