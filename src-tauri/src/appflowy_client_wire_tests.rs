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
    assert!(folder.children.is_empty());
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
