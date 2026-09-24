use crate::{
    recording::{Recording, Track},
    recording_store::RecordingStore,
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "meeting-management-{label}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}
fn recording(id: &str, track: PathBuf, status: &str) -> Recording {
    Recording {
        recording_id: id.into(),
        started_at: 1,
        ended_at: Some(2),
        duration_ms: 1,
        status: status.into(),
        tracks: vec![Track {
            source_id: "mic".into(),
            name: "Mic".into(),
            direction: "input".into(),
            sample_rate: 16_000,
            channels: 1,
            file_path: track.display().to_string(),
            frames_written: 1,
            dropped_samples: 0,
            error: None,
        }],
        error: None,
        meeting_title: Some("Original".into()),
        storage_path: None,
        auto_transcribe: false,
        transcription_status: "idle".into(),
        appflowy_status: "idle".into(),
    }
}

#[test]
fn rename_persists_without_moving_or_changing_audio() {
    let root = root("rename");
    let dir = root.join("record-1");
    fs::create_dir(&dir).unwrap();
    let wav = dir.join("track.wav");
    let bytes = b"audio retained";
    fs::write(&wav, bytes).unwrap();
    let store = RecordingStore::open(&root).unwrap();
    store
        .save(&recording("record-1", wav.clone(), "recorded"))
        .unwrap();
    let renamed = store
        .rename("record-1", "  <Escaped title>  ".into())
        .unwrap();
    assert_eq!(renamed.meeting_title.as_deref(), Some("<Escaped title>"));
    assert_eq!(fs::read(&wav).unwrap(), bytes);
    assert!(dir.is_dir());
    assert_eq!(
        store.get_by_id("record-1").unwrap().unwrap().meeting_title,
        renamed.meeting_title
    );
    drop(store);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn tombstone_is_idempotent_hides_all_worker_queries_and_retains_audio() {
    let root = root("delete");
    let dir = root.join("record-2");
    fs::create_dir(&dir).unwrap();
    let wav = dir.join("track.wav");
    let bytes = b"retain";
    fs::write(&wav, bytes).unwrap();
    let store = RecordingStore::open(&root).unwrap();
    let record = recording("record-2", wav.clone(), "recorded");
    store.save(&record).unwrap();
    store.tombstone("record-2", 3).unwrap();
    store.tombstone("record-2", 4).unwrap();
    assert!(store.recent().unwrap().is_empty());
    assert!(store.get_by_id("record-2").unwrap().is_none());
    assert!(store.finalized_eligible().unwrap().is_empty());
    assert!(store.unfinished().unwrap().is_empty());
    assert_eq!(fs::read(&wav).unwrap(), bytes);
    assert!(
        store.save(&record).is_err(),
        "a tombstone must block stale-save resurrection"
    );
    drop(store);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_recordings_cannot_be_renamed_or_tombstoned() {
    let root = root("active");
    let dir = root.join("record-3");
    fs::create_dir(&dir).unwrap();
    let wav = dir.join("track.wav");
    fs::write(&wav, b"x").unwrap();
    let store = RecordingStore::open(&root).unwrap();
    store
        .save(&recording("record-3", wav, "recording"))
        .unwrap();
    assert!(store.rename("record-3", "Nope".into()).is_err());
    assert!(store.tombstone("record-3", 2).is_err());
    drop(store);
    let _ = fs::remove_dir_all(root);
}
