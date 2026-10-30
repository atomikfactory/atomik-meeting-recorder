use super::*;
use crate::recording::Track;
use crate::recording_paths::create_recording_directory;
use std::{
    cell::RefCell,
    sync::Mutex as StdMutex,
    time::{SystemTime, UNIX_EPOCH},
};

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "meeting-recorder-delete-{name}-{}-{nonce}",
        std::process::id()
    ))
}

fn hash64() -> String {
    "a".repeat(64)
}

/// Never touches a real Recycle Bin/Trash: just records which paths would
/// have been sent there.
#[derive(Default)]
struct FakeTrash {
    sent: StdMutex<RefCell<Vec<PathBuf>>>,
}
impl FakeTrash {
    fn sent_paths(&self) -> Vec<PathBuf> {
        self.sent.lock().unwrap().borrow().clone()
    }
}
impl TrashBackend for FakeTrash {
    fn send_to_trash(&self, path: &Path) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .borrow_mut()
            .push(path.to_path_buf());
        // A real trash call also removes the folder; simulate that so callers
        // observing the filesystem see the same end state either mode leaves.
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    }
}

struct Fixture {
    root: PathBuf,
    recordings: RecordingStore,
    transcription: TranscriptionStore,
    diarization: DiarizationStore,
    summary: SummaryStore,
    compression: CompressionStore,
    appflowy: AppFlowyStore,
    atomik_kb: crate::atomik_kb_store::AtomikKbStore,
    speaker_names: crate::speaker_names::SpeakerNameStore,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let root = temp_root(name);
        std::fs::create_dir_all(&root).unwrap();
        let recordings = RecordingStore::open(&root).unwrap();
        let transcription = TranscriptionStore::open(&root.join("transcription.sqlite3")).unwrap();
        let diarization = DiarizationStore::open(&root.join("diarization.sqlite3")).unwrap();
        let summary = SummaryStore::open(&root.join("summary.sqlite3")).unwrap();
        let compression = CompressionStore::open(&root.join("compression.sqlite3")).unwrap();
        let appflowy = AppFlowyStore::open(&root.join("appflowy.sqlite3")).unwrap();
        let atomik_kb =
            crate::atomik_kb_store::AtomikKbStore::open(&root.join("atomik_kb.sqlite3")).unwrap();
        let speaker_names =
            crate::speaker_names::SpeakerNameStore::open(crate::speaker_names::store_path(&root))
                .unwrap();
        Self {
            root,
            recordings,
            transcription,
            diarization,
            summary,
            compression,
            appflowy,
            atomik_kb,
            speaker_names,
        }
    }
    fn stores(&self) -> DeleteStores<'_> {
        DeleteStores {
            recordings: &self.recordings,
            transcription: &self.transcription,
            diarization: &self.diarization,
            summary: &self.summary,
            compression: &self.compression,
            appflowy: &self.appflowy,
            atomik_kb: &self.atomik_kb,
            speaker_names: &self.speaker_names,
        }
    }
    /// Creates a real, finished recording folder (one track WAV) under this
    /// fixture's root and saves its metadata, exactly like a real recording.
    fn new_recording(&self) -> (String, PathBuf) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut rec = Recording {
            recording_id: id.clone(),
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_001_000),
            duration_ms: 1_000,
            status: "recorded".into(),
            tracks: vec![Track {
                source_id: "s".into(),
                name: "mic".into(),
                direction: "input".into(),
                sample_rate: 16_000,
                channels: 1,
                file_path: String::new(),
                frames_written: 10,
                dropped_samples: 0,
                error: None,
            }],
            error: None,
            meeting_title: Some("Standup".into()),
            storage_path: None,
            library_root: None,
            auto_transcribe: false,
            transcription_status: "not_started".into(),
            appflowy_status: "not_started".into(),
        };
        let dir = create_recording_directory(&self.root, &mut rec).unwrap();
        std::fs::write(dir.join("track-00.wav"), [0u8; 100]).unwrap();
        self.recordings.begin(&rec).unwrap();
        self.recordings.finish(&rec).unwrap();
        (id, dir)
    }
}

#[test]
fn recycle_bin_mode_sends_folder_to_trash_backend_and_removes_all_rows() {
    let fx = Fixture::new("recycle");
    let (id, dir) = fx.new_recording();
    fx.transcription.enqueue(&id, "base", &hash64()).unwrap();
    fx.appflowy.enqueue(&id).unwrap();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        std::slice::from_ref(&id),
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(summary.results[0].deleted, "{:?}", summary.results[0]);
    assert_eq!(trash.sent_paths(), vec![dir.clone()]);
    assert!(!dir.exists());
    assert!(fx.recordings.recent().unwrap().is_empty());
    assert!(fx.transcription.get(&id).unwrap().is_none());
    assert!(fx.appflowy.get_job(&id).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn permanent_mode_never_calls_the_trash_backend() {
    let fx = Fixture::new("permanent");
    let (id, dir) = fx.new_recording();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        &[id],
        DeleteMode::Permanent,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(summary.results[0].deleted);
    assert!(
        trash.sent_paths().is_empty(),
        "permanent mode must not use the recycle bin"
    );
    assert!(!dir.exists());
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn refuses_a_recording_that_is_currently_being_captured() {
    let fx = Fixture::new("active");
    let (id, dir) = fx.new_recording();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        std::slice::from_ref(&id),
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        Some(id.as_str()),
        &trash,
    );
    assert!(!summary.results[0].deleted);
    assert!(summary.results[0]
        .skipped_reason
        .as_deref()
        .unwrap()
        .contains("currently"));
    assert!(dir.exists());
    assert!(trash.sent_paths().is_empty());
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn refuses_recordings_with_an_actively_running_job() {
    for job in [
        "transcribing",
        "diarizing",
        "summarizing",
        "compressing",
        "exporting",
    ] {
        let fx = Fixture::new(&format!("job-{job}"));
        let (id, dir) = fx.new_recording();
        match job {
            "transcribing" => {
                fx.transcription.enqueue(&id, "base", &hash64()).unwrap();
                assert!(fx
                    .transcription
                    .claim(&id, "/models/base.bin", &hash64())
                    .unwrap());
            }
            "diarizing" => {
                fx.diarization.enqueue(&id, &hash64()).unwrap();
                let pair = crate::diarization_store::ModelPair {
                    segmentation_path: "/seg".into(),
                    segmentation_hash: hash64(),
                    embedding_path: "/emb".into(),
                    embedding_hash: hash64(),
                };
                assert!(fx.diarization.claim(&id, &pair).unwrap());
            }
            "summarizing" => {
                fx.summary.enqueue(&id, &hash64()).unwrap();
                assert!(fx.summary.claim(&id, "qwen3:8b").unwrap());
            }
            "compressing" => {
                fx.compression.enqueue(&id).unwrap();
                assert!(fx.compression.claim(&id).unwrap());
            }
            "exporting" => {
                fx.appflowy.enqueue(&id).unwrap();
                assert!(fx.appflowy.claim(&id).unwrap());
            }
            _ => unreachable!(),
        }
        let trash = FakeTrash::default();
        let summary = delete_recordings_from_disk(
            std::slice::from_ref(&id),
            DeleteMode::RecycleBin,
            &fx.root,
            &[],
            &fx.stores(),
            None,
            &trash,
        );
        assert!(!summary.results[0].deleted, "job={job}");
        assert!(
            dir.exists(),
            "job={job}: folder must survive a refused delete"
        );
        assert!(trash.sent_paths().is_empty(), "job={job}");
        let _ = std::fs::remove_dir_all(&fx.root);
    }
}

#[test]
fn a_recording_under_an_unrecognized_root_is_skipped_not_deleted() {
    let fx = Fixture::new("outside-a");
    let untracked_root = temp_root("outside-b");
    std::fs::create_dir_all(&untracked_root).unwrap();
    let mut rec = Recording {
        recording_id: uuid::Uuid::new_v4().to_string(),
        started_at: 1,
        ended_at: Some(1),
        duration_ms: 0,
        status: "recorded".into(),
        tracks: vec![],
        error: None,
        meeting_title: None,
        storage_path: None,
        library_root: None,
        auto_transcribe: false,
        transcription_status: "not_started".into(),
        appflowy_status: "not_started".into(),
    };
    // Created under a root never registered as the default or an extra
    // (known) root -- its `library_root` claim must not be trusted.
    let dir = create_recording_directory(&untracked_root, &mut rec).unwrap();
    fx.recordings.begin(&rec).unwrap();
    fx.recordings.finish(&rec).unwrap();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        &[rec.recording_id.clone()],
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(!summary.results[0].deleted);
    assert!(dir.exists());
    assert!(trash.sent_paths().is_empty());
    let _ = std::fs::remove_dir_all(&fx.root);
    let _ = std::fs::remove_dir_all(&untracked_root);
}

#[test]
fn a_symlinked_recording_folder_is_never_touched() {
    let fx = Fixture::new("symlink");
    let (id, dir) = fx.new_recording();
    let real_elsewhere = temp_root("symlink-target");
    std::fs::rename(&dir, &real_elsewhere).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_elsewhere, &dir).unwrap();
    #[cfg(windows)]
    {
        // Directory junction via the standard library's symlink_dir (requires
        // no elevated privileges on modern Windows for junction-equivalent
        // dir symlinks in dev mode; if creation fails, e.g. no permission in
        // this environment, skip rather than false-fail an unrelated case.
        if std::os::windows::fs::symlink_dir(&real_elsewhere, &dir).is_err() {
            let _ = std::fs::remove_dir_all(&fx.root);
            let _ = std::fs::remove_dir_all(&real_elsewhere);
            return;
        }
    }
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        &[id],
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(!summary.results[0].deleted);
    assert!(trash.sent_paths().is_empty());
    assert!(
        real_elsewhere.exists(),
        "the real folder behind the symlink must survive"
    );
    let _ = std::fs::remove_file(&dir).or_else(|_| std::fs::remove_dir(&dir));
    let _ = std::fs::remove_dir_all(&fx.root);
    let _ = std::fs::remove_dir_all(&real_elsewhere);
}

#[test]
fn the_appflowy_pages_journal_is_never_touched_only_the_job_row() {
    let fx = Fixture::new("appflowy-journal");
    let (id, dir) = fx.new_recording();
    fx.appflowy.enqueue(&id).unwrap();
    fx.appflowy
        .find_or_plan("meeting:test:1", "meeting", None, "ws", "Standup")
        .unwrap();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        std::slice::from_ref(&id),
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(summary.results[0].deleted);
    assert!(!dir.exists());
    assert!(
        fx.appflowy.get_job(&id).unwrap().is_none(),
        "job row must be removed"
    );
    assert!(
        fx.appflowy.get_page("meeting:test:1").unwrap().is_some(),
        "the pages journal (already-exported pages) must never be touched by a local delete"
    );
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn the_atomik_kb_journal_is_never_touched_only_the_job_row_and_an_active_export_blocks_deletion() {
    let fx = Fixture::new("atomik-kb-journal");
    let (id, dir) = fx.new_recording();
    fx.atomik_kb.enqueue(&id).unwrap();
    fx.atomik_kb
        .find_or_plan("meeting:test:1", "meeting", None, "space", "Standup")
        .unwrap();
    assert!(fx.atomik_kb.claim(&id).unwrap());
    let trash = FakeTrash::default();
    let blocked = delete_recordings_from_disk(
        std::slice::from_ref(&id),
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(
        !blocked.results[0].deleted,
        "an in-flight atomik-kb export blocks deletion"
    );
    assert!(dir.exists());
    fx.atomik_kb.complete(&id).unwrap();
    let summary = delete_recordings_from_disk(
        std::slice::from_ref(&id),
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(summary.results[0].deleted, "{:?}", summary.results[0]);
    assert!(
        fx.atomik_kb.get_job(&id).unwrap().is_none(),
        "job row must be removed"
    );
    assert!(
        fx.atomik_kb.get_page("meeting:test:1").unwrap().is_some(),
        "the pages journal must never be touched by a local delete"
    );
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn tombstone_is_set_before_the_folder_is_removed_crash_safety() {
    // Simulates a crash between the tombstone write and the folder removal:
    // once tombstoned, the recording is already gone from every listing, so
    // a follow-up run never re-exposes or re-processes it even if the folder
    // still happens to be on disk.
    let fx = Fixture::new("tombstone");
    let (id, dir) = fx.new_recording();
    fx.recordings.tombstone(&id, 123).unwrap();
    assert!(fx.recordings.recent().unwrap().is_empty());
    assert!(dir.exists(), "tombstoning alone must not touch the folder");
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn unknown_recording_ids_are_reported_as_skipped_and_do_not_error_out_the_batch() {
    let fx = Fixture::new("unknown");
    let (id, dir) = fx.new_recording();
    let missing = uuid::Uuid::new_v4().to_string();
    let trash = FakeTrash::default();
    let summary = delete_recordings_from_disk(
        &[missing.clone(), id.clone()],
        DeleteMode::RecycleBin,
        &fx.root,
        &[],
        &fx.stores(),
        None,
        &trash,
    );
    assert!(!summary.results[0].deleted);
    assert!(summary.results[1].deleted);
    assert!(!dir.exists());
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn parses_delete_mode() {
    assert_eq!(
        DeleteMode::parse("recycle_bin").unwrap(),
        DeleteMode::RecycleBin
    );
    assert_eq!(DeleteMode::parse("").unwrap(), DeleteMode::RecycleBin);
    assert_eq!(
        DeleteMode::parse("permanent").unwrap(),
        DeleteMode::Permanent
    );
    assert!(DeleteMode::parse("delete-forever").is_err());
}
