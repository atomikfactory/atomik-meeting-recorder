use super::*;

fn store() -> AppFlowyStore {
    let dir = std::env::temp_dir().join(format!("meeting-appflowy-store-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    AppFlowyStore::open(&dir.join("appflowy.sqlite3")).unwrap()
}

#[test]
fn find_or_plan_creates_once_and_is_stable_on_retry() {
    let store = store();
    let key = root_key("w1", "space1");
    let first = store.find_or_plan(&key, "root", None, "w1", "My daily meetings").unwrap();
    assert_eq!(first.status, "creating");
    let second = store.find_or_plan(&key, "root", None, "w1", "My daily meetings").unwrap();
    assert_eq!(first.planned_view_id, second.planned_view_id, "retry must reuse the planned id, not mint a new one");
    assert_eq!(second.status, "creating");
}

#[test]
fn mark_created_is_idempotent() {
    let store = store();
    let key = root_key("w1", "space1");
    store.find_or_plan(&key, "root", None, "w1", "My daily meetings").unwrap();
    store.mark_created(&key, "remote-id-1").unwrap();
    store.mark_created(&key, "remote-id-1").unwrap();
    let page = store.get_page(&key).unwrap().unwrap();
    assert_eq!(page.status, "created");
    assert_eq!(page.remote_view_id.as_deref(), Some("remote-id-1"));
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
    assert!(!store.claim("rec-1").unwrap(), "cannot claim an already-exporting job twice");
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
    store.fail("rec-1", "AppFlowy rejected the request (HTTP 422): bad name", false).unwrap();
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
fn page_and_meeting_keys_are_deterministic_across_calls() {
    assert_eq!(root_key("w1", "s1"), root_key("w1", "s1"));
    assert_eq!(date_key("w1", "root1", "2026-09-25"), date_key("w1", "root1", "2026-09-25"));
    assert_eq!(meeting_key("rec-1", 1), meeting_key("rec-1", 1));
    assert_ne!(meeting_key("rec-1", 1), meeting_key("rec-1", 2));
}
