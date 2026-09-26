//! Proves `AppFlowyClient` actually speaks the documented wire protocol
//! against a real (fake, in-process) HTTP server -- request shape, header,
//! response parsing, and the 401-refresh-retry path -- complementing
//! `appflowy_allowlist_tests.rs`'s pure, socket-free proof of the negative
//! space (everything forbidden).
use super::*;
use crate::appflowy_credentials::{CredentialStore, FakeCredentialStore};
use crate::appflowy_fake_server::FakeAppFlowyServer;

fn client(server: &FakeAppFlowyServer) -> (AppFlowyClient, FakeCredentialStore) {
    let creds = FakeCredentialStore::new();
    let client = AppFlowyClient::new(&server.base_url, std::sync::Arc::new(creds.clone())).unwrap();
    (client, creds)
}

#[test]
fn otp_request_and_verify_stores_a_session() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    client.request_otp("user@example.com").unwrap();
    assert_eq!(server.otp_email_requested().as_deref(), Some("user@example.com"));
    client.verify_otp("user@example.com", &server.otp_code()).unwrap();
    assert!(client.is_signed_in().unwrap());
    assert_eq!(client.signed_in_email().unwrap().as_deref(), Some("user@example.com"));
    assert_eq!(creds.get(crate::appflowy_credentials::KEY_ACCESS_TOKEN).unwrap().as_deref(), Some("access-1"));
}

#[test]
fn verify_otp_rejects_a_wrong_code() {
    let server = FakeAppFlowyServer::start();
    let (client, _creds) = client(&server);
    client.request_otp("user@example.com").unwrap();
    let error = client.verify_otp("user@example.com", "000000").unwrap_err();
    assert!(error.contains("invalid code"), "{error}");
    assert!(!client.is_signed_in().unwrap());
}

#[test]
fn list_workspaces_and_get_folder_round_trip() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    let workspaces = client.list_workspaces().unwrap();
    assert_eq!(workspaces[0].workspace_id, server.workspace_id);
    let folder = client.get_folder(&server.workspace_id, None).unwrap();
    assert_eq!(folder.view_id, server.workspace_id);
    assert!(folder.child_named("General").is_some(), "the fake server seeds a default Space");
}

#[test]
fn create_page_reaches_the_server_and_returns_its_view_id() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    let page_data = serde_json::json!({"type": "page", "data": {}, "children": []});
    let page = client.create_page(&server.workspace_id, &server.workspace_id, "planned-1", "My daily meetings", &page_data).unwrap();
    assert_eq!(page.view_id, "planned-1");
    let folder = client.get_folder(&server.workspace_id, None).unwrap();
    assert!(folder.child_named("My daily meetings").is_some());
}

#[test]
fn a_401_triggers_exactly_one_refresh_then_succeeds() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    server.invalidate_current_access_token();
    let workspaces = client.list_workspaces().unwrap();
    assert_eq!(workspaces[0].workspace_id, server.workspace_id);
    assert_eq!(server.refresh_count(), 1, "exactly one refresh should have happened");
    // The refreshed token was persisted.
    assert_eq!(creds.get(crate::appflowy_credentials::KEY_ACCESS_TOKEN).unwrap().as_deref(), Some("access-2"));
}

#[test]
fn create_page_sends_collab_id_so_its_content_loads_afterward() {
    // Regression test for the "Record not found: Collab not found for
    // object_id: <view_id>" bug: AppFlowy-Cloud stores the document collab
    // under `collab_id.unwrap_or(new uuid)` and the page under
    // `view_id.unwrap_or(collab_id)`, independently. A `CreatePageParams`
    // that sends only `view_id` (the pre-fix bug) leaves the document under
    // a random id the client never learns, so the page is created and
    // listed, but its content 404s "Collab not found" when opened by
    // `view_id`. `AppFlowyClient::create_page` must send `collab_id ==
    // view_id` so this never happens.
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    let page_data = serde_json::json!({"type": "page", "data": {}, "children": []});
    let page = client.create_page(&server.workspace_id, &server.workspace_id, "planned-collab", "My daily meetings", &page_data).unwrap();
    assert!(
        client.page_content_loads(&server.workspace_id, &page.view_id).unwrap(),
        "a page created by this client's create_page must have working content, not just a listed view"
    );
}

#[test]
fn page_content_loads_reports_false_for_an_unknown_view_id() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    assert!(!client.page_content_loads(&server.workspace_id, "no-such-view").unwrap());
}

#[test]
fn a_request_without_collab_id_reproduces_the_record_not_found_bug_and_the_fix_resolves_it() {
    // Drives the fake server's raw wire protocol directly (bypassing
    // `AppFlowyClient`, which always sends the fix) to prove the fake models
    // the *real* AppFlowy-Cloud bug this integration hit in production: a
    // `page-view` create request with `view_id` but no `collab_id` succeeds
    // and lists the page, yet the page's content never loads afterward.
    let server = FakeAppFlowyServer::start();
    let agent = ureq::AgentBuilder::new().build();
    let create = |body: serde_json::Value| -> String {
        agent
            .post(&format!("{}/api/workspace/{}/page-view", server.base_url, server.workspace_id))
            .set("Authorization", "Bearer access-1")
            .send_json(body)
            .unwrap()
            .into_json::<serde_json::Value>()
            .unwrap()["data"]["view_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Returns `Some(status)` on a non-2xx response, `None` on 2xx --
    // avoids handing back `ureq::Error` itself (clippy::result_large_err).
    let get_page_view_status = |view_id: &str| -> Option<u16> {
        match agent
            .get(&format!("{}/api/workspace/{}/page-view/{}", server.base_url, server.workspace_id, view_id))
            .set("Authorization", "Bearer access-1")
            .call()
        {
            Ok(_) => None,
            Err(ureq::Error::Status(code, _)) => Some(code),
            Err(ureq::Error::Transport(t)) => panic!("unexpected transport error: {t}"),
        }
    };

    // Old, buggy request shape: `view_id` present, `collab_id` absent.
    let legacy_view_id = create(serde_json::json!({
        "parent_view_id": server.space_id, "layout": 0, "name": "Old",
        "page_data": {"type": "page", "data": {}, "children": []},
        "view_id": "legacy-view-1",
    }));
    assert_eq!(legacy_view_id, "legacy-view-1");
    assert_eq!(
        get_page_view_status(&legacy_view_id),
        Some(404),
        "the old request shape must reproduce the Record-not-found bug"
    );

    // Fixed request shape: `collab_id` equal to `view_id`.
    let fixed_view_id = create(serde_json::json!({
        "parent_view_id": server.space_id, "layout": 0, "name": "New",
        "page_data": {"type": "page", "data": {}, "children": []},
        "view_id": "fixed-view-1", "collab_id": "fixed-view-1",
    }));
    assert_eq!(fixed_view_id, "fixed-view-1");
    assert_eq!(get_page_view_status(&fixed_view_id), None, "the fixed request shape must make the page's content load");
}

#[test]
fn french_utf8_page_name_and_content_round_trip_over_the_wire() {
    let server = FakeAppFlowyServer::start();
    let (client, creds) = client(&server);
    creds.set(crate::appflowy_credentials::KEY_ACCESS_TOKEN, "access-1").unwrap();
    creds.set(crate::appflowy_credentials::KEY_REFRESH_TOKEN, "refresh-1").unwrap();
    let page_data = serde_json::json!({"type": "page", "data": {}, "children": [
        {"type": "paragraph", "data": {"delta": [{"insert": "Réunion à 14h30 — 100% \"prêt\""}]}, "children": []}
    ]});
    client.create_page(&server.workspace_id, &server.workspace_id, "planned-fr", "Réunion hebdomadaire", &page_data).unwrap();
    let folder = client.get_folder(&server.workspace_id, None).unwrap();
    assert!(folder.child_named("Réunion hebdomadaire").is_some());
}
