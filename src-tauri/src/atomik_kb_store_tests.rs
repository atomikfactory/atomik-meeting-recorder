use super::*;

fn store() -> AtomikKbStore {
    let dir = std::env::temp_dir().join(format!(
        "meeting-atomik-kb-store-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    AtomikKbStore::open(&dir.join("atomik_kb.sqlite3")).unwrap()
}

#[test]
fn find_or_plan_creates_once_and_is_stable_on_retry() {
    let store = store();
    let key = root_key("space1");
    let first = store
        .find_or_plan(&key, "root", None, "space1", "My daily meetings")
        .unwrap();
    assert_eq!(first.status, "creating");
    assert!(first.remote_page_id.is_none());
    let second = store
        .find_or_plan(&key, "root", None, "space1", "My daily meetings")
        .unwrap();
    assert_eq!(
        first, second,
        "a retry must reuse the row, not plan a new one"
    );
    assert_eq!(store.all_pages().unwrap().len(), 1);
}

#[test]
fn mark_created_is_idempotent_and_find_or_plan_then_returns_the_remote_id() {
    let store = store();
    let key = root_key("space1");
    store
        .find_or_plan(&key, "root", None, "space1", "My daily meetings")
        .unwrap();
    store.mark_created(&key, "page-1").unwrap();
    store.mark_created(&key, "page-1").unwrap();
    let page = store
        .find_or_plan(&key, "root", None, "space1", "My daily meetings")
        .unwrap();
    assert_eq!(page.status, "created");
    assert_eq!(page.remote_page_id.as_deref(), Some("page-1"));
}

#[test]
fn all_pages_lists_every_planned_and_created_page_in_order() {
    let store = store();
    let root = root_key("space1");
    let date = date_key("root-remote", "2026-09-26");
    store
        .find_or_plan(&root, "root", None, "space1", "My daily meetings")
        .unwrap();
    store.mark_created(&root, "root-remote").unwrap();
    store
        .find_or_plan(&date, "date", Some(&root), "space1", "2026-09-26")
        .unwrap();
    let pages = store.all_pages().unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].key, root);
    assert_eq!(pages[0].remote_page_id.as_deref(), Some("root-remote"));
    assert_eq!(pages[1].key, date);
    assert!(pages[1].remote_page_id.is_none());
}

#[test]
fn job_lifecycle_enqueue_claim_complete() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.enqueue("rec-1").unwrap();
    assert_eq!(store.all_jobs().unwrap().len(), 1);
    assert_eq!(store.next().unwrap().unwrap().recording_id, "rec-1");
    assert!(store.claim("rec-1").unwrap());
    assert!(
        !store.claim("rec-1").unwrap(),
        "cannot claim an already-exporting job twice"
    );
    store.complete("rec-1").unwrap();
    assert_eq!(store.get_job("rec-1").unwrap().unwrap().status, "completed");
}

#[test]
fn transient_failure_schedules_backoff_and_is_not_immediately_retried() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    store
        .fail("rec-1", "atomik-kb is not reachable", true)
        .unwrap();
    let job = store.get_job("rec-1").unwrap().unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(job.error.as_deref(), Some("atomik-kb is not reachable"));
    assert!(store.next().unwrap().is_none());
}

#[test]
fn permanent_failure_requires_manual_retry() {
    let store = store();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    store.fail("rec-1", "token refused", false).unwrap();
    assert_eq!(store.get_job("rec-1").unwrap().unwrap().status, "failed");
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
    store.recover().unwrap();
    assert_eq!(store.get_job("rec-1").unwrap().unwrap().status, "queued");
}

#[test]
fn deleting_a_recording_removes_its_idle_job_but_never_the_page_journal() {
    let store = store();
    let key = meeting_key("rec-1", 1);
    store
        .find_or_plan(&key, "meeting", None, "space1", "09:00 — Sync")
        .unwrap();
    store.mark_created(&key, "page-1").unwrap();
    store.enqueue("rec-1").unwrap();
    store.claim("rec-1").unwrap();
    assert!(
        store.remove_job_if_idle("rec-1").is_err(),
        "an in-flight export blocks deletion"
    );
    store.complete("rec-1").unwrap();
    store.remove_job_if_idle("rec-1").unwrap();
    assert!(store.get_job("rec-1").unwrap().is_none());
    assert_eq!(
        store
            .get_page(&key)
            .unwrap()
            .unwrap()
            .remote_page_id
            .as_deref(),
        Some("page-1")
    );
}

#[test]
fn page_and_meeting_keys_are_deterministic_across_calls() {
    assert_eq!(root_key("s1"), root_key("s1"));
    assert_ne!(root_key("s1"), root_key("s2"));
    assert_eq!(
        date_key("root1", "2026-09-25"),
        date_key("root1", "2026-09-25")
    );
    assert_eq!(meeting_key("rec-1", 1), meeting_key("rec-1", 1));
    assert_ne!(meeting_key("rec-1", 1), meeting_key("rec-1", 2));
}
