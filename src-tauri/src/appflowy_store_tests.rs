use super::*;

fn store() -> AppFlowyStore {
    let dir = std::env::temp_dir().join(format!(
        "meeting-appflowy-store-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    AppFlowyStore::open(&dir.join("appflowy.sqlite3")).unwrap()
}

#[test]
fn find_or_plan_creates_once_and_is_stable_on_retry() {
    let store = store();
    let key = root_key("w1", "space1");
    let first = store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    assert_eq!(first.status, "creating");
    let second = store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    assert_eq!(
        first.planned_view_id, second.planned_view_id,
        "retry must reuse the planned id, not mint a new one"
    );
    assert_eq!(second.status, "creating");
}

#[test]
fn mark_created_is_idempotent() {
    let store = store();
    let key = root_key("w1", "space1");
    store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    store.mark_created(&key, "remote-id-1").unwrap();
    store.mark_created(&key, "remote-id-1").unwrap();
    let page = store.get_page(&key).unwrap().unwrap();
    assert_eq!(page.status, "created");
    assert_eq!(page.remote_view_id.as_deref(), Some("remote-id-1"));
}

#[test]
fn all_pages_lists_every_planned_and_created_page() {
    let store = store();
    let root = root_key("w1", "space1");
    let date = date_key("w1", "root-remote", "2026-09-26");
    store
        .find_or_plan(&root, "root", None, "w1", "My daily meetings")
        .unwrap();
    store.mark_created(&root, "root-remote").unwrap();
    store
        .find_or_plan(&date, "date", Some(&root), "w1", "2026-09-26")
        .unwrap();
    let pages = store.all_pages().unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].key, root);
    assert_eq!(pages[0].remote_view_id.as_deref(), Some("root-remote"));
    assert_eq!(pages[1].key, date);
    assert!(pages[1].remote_view_id.is_none());
}

#[test]
fn job_lifecycle_enqueue_claim_complete() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    // Re-enqueueing an already-queued job is a no-op, not a duplicate/error.
    store.enqueue("rec-1").unwrap();
    assert_eq!(store.all_jobs().unwrap().len(), 1);
    let next = store.next().unwrap().unwrap();
    assert_eq!(next.recording_id, "rec-1");
    assert!(store.claim("rec-1").unwrap());
    assert!(
        !store.claim("rec-1").unwrap(),
        "cannot claim an already-exporting job twice"
    );
    store.complete("rec-1").unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "completed");
}

#[test]
fn transient_failure_schedules_backoff_and_is_not_immediately_retried() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    store.fail("rec-1", "network error", true).unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(job.error.as_deref(), Some("network error"));
    // Backoff means it's not immediately due again.
    assert!(store.next().unwrap().is_none());
}

#[test]
fn permanent_failure_requires_manual_retry() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    store
        .fail(
            "rec-1",
            "AppFlowy rejected the request (HTTP 422): bad name",
            false,
        )
        .unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "failed");
    assert!(store.next().unwrap().is_none());
    store.retry("rec-1").unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "queued");
    assert!(job.error.is_none());
}

#[test]
fn export_again_bumps_the_index_and_requires_a_finished_job() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    // Cannot export-again a job that is still queued/exporting.
    assert!(store.export_again("rec-1").is_err());
    store.claim("rec-1").unwrap();
    store.complete("rec-1").unwrap();
    store.export_again("rec-1").unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.export_index, 2);
    assert_eq!(job.status, "queued");
}

#[test]
fn recover_resumes_interrupted_exports_as_queued() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    // Simulate a crash mid-export: process restarts, recover() runs.
    store.recover().unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "queued");
}

#[test]
fn find_or_plan_stamps_the_current_generation() {
    let store = store();
    let key = root_key("w1", "space1");
    let record = store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    assert_eq!(record.generation, CURRENT_GENERATION);
    assert!(!record.is_legacy());
}

#[test]
fn a_legacy_row_is_never_reused_and_a_fresh_row_is_planned_instead() {
    let store = store();
    let key = root_key("w1", "space1");
    // Simulate a row left behind by a version predating a generation bump
    // (e.g. the pre-fix `collab_id` bug) -- occupies the same deterministic
    // key a fresh export would otherwise reuse.
    store
        .insert_legacy_row(
            &key,
            "root",
            "w1",
            "My daily meetings",
            "legacy-remote-id",
            1,
        )
        .unwrap();

    let record = store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    assert_ne!(
        record.key, key,
        "a fresh row must be planned under a different key, never reusing the legacy row's key"
    );
    assert_eq!(record.generation, CURRENT_GENERATION);
    assert!(
        record.remote_view_id.is_none(),
        "the fresh row must not inherit the legacy row's remote id"
    );

    // The legacy row itself is left completely untouched.
    let legacy = store.get_page(&key).unwrap().unwrap();
    assert_eq!(legacy.remote_view_id.as_deref(), Some("legacy-remote-id"));
    assert_eq!(legacy.generation, 1);

    // Retrying is stable: the same fresh key is reused, not a third row.
    let retry = store
        .find_or_plan(&key, "root", None, "w1", "My daily meetings")
        .unwrap();
    assert_eq!(retry.key, record.key);
}

#[test]
fn is_legacy_view_id_reflects_the_generation_of_the_row_that_recorded_it() {
    let store = store();
    let legacy_key = root_key("w1", "space1");
    store
        .insert_legacy_row(
            &legacy_key,
            "root",
            "w1",
            "My daily meetings",
            "legacy-remote-id",
            1,
        )
        .unwrap();
    assert!(store.is_legacy_view_id("legacy-remote-id").unwrap());

    let date_key_str = date_key("w1", "root-remote", "2026-09-26");
    store
        .find_or_plan(&date_key_str, "date", None, "w1", "2026-09-26")
        .unwrap();
    store
        .mark_created(&date_key_str, "fresh-remote-id")
        .unwrap();
    assert!(!store.is_legacy_view_id("fresh-remote-id").unwrap());
    assert!(!store.is_legacy_view_id("never-recorded").unwrap());
}

#[test]
fn page_and_meeting_keys_are_deterministic_across_calls() {
    assert_eq!(root_key("w1", "s1"), root_key("w1", "s1"));
    assert_eq!(
        date_key("w1", "root1", "2026-09-25"),
        date_key("w1", "root1", "2026-09-25")
    );
    assert_eq!(meeting_key("rec-1", 1), meeting_key("rec-1", 1));
    assert_ne!(meeting_key("rec-1", 1), meeting_key("rec-1", 2));
}
