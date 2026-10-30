//! Wire-level and parsing tests for `AtomikKbClient`, against the in-process
//! fake server (`atomik_kb_fake_server.rs`, loopback only). No real
//! atomik-kb, and never a real token: `FAKE_TOKEN` is obviously fake.
use super::*;
use crate::atomik_kb_credentials::FAKE_TOKEN;
use crate::atomik_kb_fake_server::FakeAtomikKbServer;

fn client(server: &FakeAtomikKbServer) -> AtomikKbClient {
    AtomikKbClient::with_host("127.0.0.1", server.port, server.token.clone()).unwrap()
}

#[test]
fn initialize_sends_initialize_then_the_initialized_notification() {
    let server = FakeAtomikKbServer::start();
    client(&server).initialize().unwrap();
    let calls = server.calls();
    let methods: Vec<&str> = calls.iter().map(|c| c.rpc_method.as_str()).collect();
    assert_eq!(methods, vec!["initialize", "notifications/initialized"]);
    assert!(calls.iter().all(|c| c.authorized));
}

#[test]
fn requests_carry_the_bearer_token_and_a_loopback_host_header() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    client.initialize().unwrap();
    client.list_spaces().unwrap();
    for call in server.calls() {
        assert!(call.authorized);
        assert_eq!(call.host, format!("127.0.0.1:{}", server.port));
    }
}

#[test]
fn list_spaces_and_list_pages_round_trip() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    client.initialize().unwrap();
    let spaces = client.list_spaces().unwrap();
    assert_eq!(
        spaces,
        vec![Space {
            id: server.space_id.clone(),
            name: "My Daily Meeting".into(),
            writable: true
        }]
    );
    assert!(client
        .list_pages(&server.space_id, None)
        .unwrap()
        .pages
        .is_empty());
    let page = server.seed_page(None, "My daily meetings");
    server.seed_page(Some(&page), "2026-09-25");
    let top = client.list_pages(&server.space_id, None).unwrap();
    assert_eq!(top.pages.len(), 1);
    assert_eq!(top.pages[0].title, "My daily meetings");
    assert!(top.pages[0].has_children);
    let children = client.list_pages(&server.space_id, Some(&page)).unwrap();
    assert_eq!(children.pages[0].title, "2026-09-25");
    assert!(!children.pages[0].has_children);
}

#[test]
fn create_page_returns_the_new_id_and_sends_title_parent_and_markdown() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    client.initialize().unwrap();
    let parent = client
        .create_page(&server.space_id, None, "My daily meetings", None)
        .unwrap();
    let id = client
        .create_page(
            &server.space_id,
            Some(&parent),
            "2026-09-25",
            Some("## Summary\n\nHello é"),
        )
        .unwrap();
    assert_eq!(server.child_ids(Some(&parent)), vec![id.clone()]);
    assert_eq!(
        server.markdown_of(&id).as_deref(),
        Some("## Summary\n\nHello é")
    );
    assert_eq!(
        server.markdown_of(&parent),
        None,
        "no markdown means an empty page"
    );
}

#[test]
fn titles_are_normalised_to_one_line_like_atomik_kb_does() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    client.initialize().unwrap();
    client
        .create_page(&server.space_id, None, "  Line one\n\tline  two  ", None)
        .unwrap();
    assert_eq!(
        server.child_titles(None),
        vec!["Line one line two".to_string()]
    );
    let list = client.list_pages(&server.space_id, None).unwrap();
    assert!(
        list.child_titled("Line one\nline two").is_some(),
        "lookup normalises the wanted title the same way"
    );
    assert!(
        list.child_titled("line one line two").is_none(),
        "but never matches case-insensitively or partially"
    );
}

#[test]
fn an_overlong_title_or_markdown_is_refused_locally_without_a_request() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    assert!(matches!(
        client.create_page(&server.space_id, None, &"x".repeat(201), None),
        Err(ClientError::Rejected(_))
    ));
    assert!(matches!(
        client.create_page(&server.space_id, None, "  \n ", None),
        Err(ClientError::Rejected(_))
    ));
    assert!(
        matches!(client.create_page(&server.space_id, None, "ok", Some(&"x".repeat(MAX_MARKDOWN_BYTES + 1))), Err(ClientError::Rejected(m)) if m.contains("256 KiB"))
    );
    assert!(server.calls().is_empty());
}

#[test]
fn a_wrong_token_is_unauthorized_not_transient() {
    let server = FakeAtomikKbServer::start();
    let client = AtomikKbClient::with_host("127.0.0.1", server.port, "kbm_wrong".into()).unwrap();
    let error = client.initialize().unwrap_err();
    assert!(
        matches!(&error, ClientError::Unauthorized(m) if m.contains("refused the token")),
        "{error}"
    );
    assert!(
        !error.to_string().contains("kbm_wrong"),
        "the token must never be echoed"
    );
}

#[test]
fn a_closed_server_is_transient() {
    let port = {
        let server = FakeAtomikKbServer::start();
        server.port
    };
    let client = AtomikKbClient::with_host("127.0.0.1", port, FAKE_TOKEN.into()).unwrap();
    let error = client.initialize().unwrap_err();
    assert!(
        matches!(&error, ClientError::Transient(m) if m.contains("not reachable")),
        "{error}"
    );
}

#[test]
fn a_server_error_status_is_transient_and_a_client_error_is_permanent() {
    let server = FakeAtomikKbServer::start();
    let client = client(&server);
    client.initialize().unwrap();
    server.fail_next_create_with_http(503);
    assert!(matches!(
        client.create_page(&server.space_id, None, "A", None),
        Err(ClientError::Transient(_))
    ));
    server.fail_next_create_with_http(422);
    assert!(
        matches!(client.create_page(&server.space_id, None, "A", None), Err(ClientError::Rejected(m)) if m.contains("422"))
    );
}

#[test]
fn a_tool_level_denial_surfaces_the_server_text_and_a_grant_hint() {
    let server = FakeAtomikKbServer::start_with_space("My Daily Meeting", false);
    let client = client(&server);
    client.initialize().unwrap();
    let error = client
        .create_page(&server.space_id, None, "A", None)
        .unwrap_err();
    let ClientError::Rejected(message) = error else {
        panic!("expected Rejected")
    };
    assert!(message.contains("no write access"), "{message}");
    assert!(message.contains("Settings → MCP"), "{message}");
    assert!(server.child_titles(None).is_empty());
}

#[test]
fn a_missing_token_is_unauthorized_with_setup_instructions() {
    use crate::appflowy_credentials::FakeCredentialStore;
    let credentials: SharedCredentialStore = std::sync::Arc::new(FakeCredentialStore::new());
    let error = AtomikKbClient::new(47811, &credentials).err().unwrap();
    assert!(
        matches!(&error, ClientError::Unauthorized(m) if m.contains("No atomik-kb token")),
        "{error}"
    );
}

#[test]
fn the_client_reads_the_token_from_the_credential_store() {
    use crate::appflowy_credentials::FakeCredentialStore;
    let server = FakeAtomikKbServer::start();
    let credentials: SharedCredentialStore = std::sync::Arc::new(FakeCredentialStore::new());
    credentials.set(KEY_TOKEN, FAKE_TOKEN).unwrap();
    AtomikKbClient::new(server.port, &credentials)
        .unwrap()
        .initialize()
        .unwrap();
}

#[test]
fn only_loopback_hosts_and_real_ports_are_accepted() {
    assert!(validate_host("127.0.0.1").is_ok());
    assert!(validate_host("localhost").is_ok());
    for host in [
        "192.168.1.10",
        "10.0.0.1",
        "0.0.0.0",
        "example.com",
        "kb.local",
        "127.0.0.1.evil.com",
        "http://127.0.0.1",
        "127.0.0.1:47811",
        "::1",
        "[::1]",
        "",
    ] {
        assert!(validate_host(host).is_err(), "{host:?} must be refused");
        assert!(
            AtomikKbClient::with_host(host, 47811, "t".into()).is_err(),
            "{host:?}"
        );
    }
    assert!(validate_port(1).is_ok());
    assert!(validate_port(65535).is_ok());
    assert!(validate_port(DEFAULT_PORT).is_ok());
    assert!(validate_port(0).is_err());
}

#[test]
fn space_names_are_validated() {
    assert!(validate_space_name("My Daily Meeting").is_ok());
    assert!(validate_space_name("Réunions — 2026").is_ok());
    assert!(validate_space_name("").is_err());
    assert!(validate_space_name("   ").is_err());
    assert!(validate_space_name("a\nb").is_err());
    assert!(validate_space_name(&"x".repeat(201)).is_err());
}

#[test]
fn parse_spaces_reads_the_real_list_spaces_text() {
    let text = "Spaces you can access (3):\n\
                - My Daily Meeting (spaceId: 11111111-1111-1111-1111-111111111111, 12 pages): read and write\n\
                - Archive (spaceId: 22222222-2222-2222-2222-222222222222, 1 page): read only\n\
                - Notes (draft) (v2) (spaceId: 33333333-3333-3333-3333-333333333333, 0 pages): read and write";
    let spaces = parse_spaces(text);
    assert_eq!(spaces.len(), 3);
    assert_eq!(
        spaces[0],
        Space {
            id: "11111111-1111-1111-1111-111111111111".into(),
            name: "My Daily Meeting".into(),
            writable: true
        }
    );
    assert_eq!(
        spaces[1],
        Space {
            id: "22222222-2222-2222-2222-222222222222".into(),
            name: "Archive".into(),
            writable: false
        }
    );
    assert_eq!(
        spaces[2].name, "Notes (draft) (v2)",
        "parentheses in a name must not confuse the parser"
    );
    assert!(spaces[2].writable);
}

#[test]
fn parse_spaces_tolerates_noise_and_the_no_spaces_sentence() {
    assert!(parse_spaces(
        "No spaces are shared with this client yet. The person can grant access in Settings."
    )
    .is_empty());
    assert!(parse_spaces("").is_empty());
    let noisy = "Some future header\n\n  - Padded (spaceId: abc-1): read and write  \nrandom line\n- no id here\n- Trailing (spaceId: abc-2)";
    let spaces = parse_spaces(noisy);
    assert_eq!(
        spaces
            .iter()
            .map(|s| (s.name.as_str(), s.id.as_str(), s.writable))
            .collect::<Vec<_>>(),
        vec![("Padded", "abc-1", true), ("Trailing", "abc-2", false)]
    );
    // A tricky name that mentions the marker itself: the last marker wins.
    let sneaky = parse_spaces("- Evil (spaceId: fake) name (spaceId: real-id, 2 pages): read only");
    assert_eq!(sneaky[0].id, "real-id");
    assert_eq!(sneaky[0].name, "Evil (spaceId: fake) name");
    assert!(!sneaky[0].writable);
    // Unicode names survive.
    assert_eq!(
        parse_spaces("- Réunions 你好 (spaceId: u-1, 0 pages): read and write")[0].name,
        "Réunions 你好"
    );
}

#[test]
fn parse_pages_reads_children_subpage_counts_none_and_truncation() {
    let text = "Top-level pages in \"My Daily Meeting\" (3):\n- My daily meetings (pageId: aaaa-1, 4 subpages)\n- (untitled) (pageId: aaaa-2)\n- Plan (v2) (pageId: aaaa-3, 1 subpage)";
    let list = parse_pages(text);
    assert!(!list.truncated);
    assert_eq!(list.pages.len(), 3);
    assert_eq!(
        list.pages[0],
        PageEntry {
            id: "aaaa-1".into(),
            title: "My daily meetings".into(),
            has_children: true
        }
    );
    assert!(!list.pages[1].has_children);
    assert_eq!(list.pages[2].title, "Plan (v2)");
    assert!(list.pages[2].has_children);

    assert!(parse_pages("Top-level pages in \"X\": none.")
        .pages
        .is_empty());
    assert!(parse_pages("Subpages of \"2026-09-25\": none.")
        .pages
        .is_empty());

    let cut = parse_pages("Top-level pages in \"X\" (250):\n- A (pageId: a1)\n... and 50 more. Use search to find a particular page.");
    assert!(cut.truncated);
    assert_eq!(cut.pages.len(), 1);
}

#[test]
fn parse_pages_ignores_lines_without_a_page_id() {
    let list = parse_pages(
        "Top-level pages in \"X\" (2):\n- Broken line\n- Fine (pageId: f-1)\nfree text",
    );
    assert_eq!(list.pages.len(), 1);
    assert_eq!(list.pages[0].id, "f-1");
}

#[test]
fn parse_created_page_id_reads_the_real_create_page_text() {
    let text = "Created page \"2026-09-25\" (pageId: 5e2f6a40-0000-4000-8000-123456789abc) in space \"My Daily Meeting\" (spaceId: 11111111-1111-1111-1111-111111111111).";
    assert_eq!(
        parse_created_page_id(text).as_deref(),
        Some("5e2f6a40-0000-4000-8000-123456789abc")
    );
    // A title or space name that imitates the marker cannot hijack the id.
    let tricky = "Created page \"x (pageId: bogus) in space \" (pageId: real-1) in space \"Y (pageId: also-bogus)\" (spaceId: s-1).";
    assert_eq!(parse_created_page_id(tricky).as_deref(), Some("real-1"));
    assert_eq!(parse_created_page_id("Something else entirely"), None);
    assert_eq!(parse_created_page_id(""), None);
}

#[test]
fn one_line_matches_atomik_kbs_normalisation() {
    assert_eq!(one_line("  a\n\nb\t c  "), "a b c");
    assert_eq!(one_line("09:00 — Réunion\u{0007}x"), "09:00 — Réunion x");
    assert_eq!(one_line(""), "");
}
