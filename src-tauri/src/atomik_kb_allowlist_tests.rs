//! Proves every forbidden atomik-kb call is refused by `allowlist::check`
//! itself -- a pure function with no I/O -- so this test suite never opens a
//! socket to prove the refusal. `atomik_kb_client_tests.rs` separately proves
//! the *allowed* calls reach a server correctly; this file only proves the
//! hard safety rule's negative space (create-only: never append to, update,
//! rename, move or delete anything).
use super::allowlist::{check, METHODS_COUNT, TOOLS_COUNT};
use super::*;
use crate::atomik_kb_fake_server::FakeAtomikKbServer;

#[test]
fn the_four_documented_operations_are_allowed() {
    assert!(check("initialize", None).is_ok());
    assert!(check("notifications/initialized", None).is_ok());
    assert!(check("tools/call", Some("list_spaces")).is_ok());
    assert!(check("tools/call", Some("list_pages")).is_ok());
    assert!(check("tools/call", Some("create_page")).is_ok());
}

#[test]
fn append_and_every_other_tool_atomik_kb_offers_is_refused() {
    for tool in [
        "append_to_page",
        "read_page",
        "search",
        "update_page",
        "delete_page",
        "move_page",
        "rename_page",
        "trash_page",
        "restore_page",
        "create_space",
        "delete_space",
        "set_grants",
        "",
    ] {
        assert!(
            check("tools/call", Some(tool)).is_err(),
            "expected tools/call {tool:?} to be refused"
        );
    }
    // A tools/call must always name a tool.
    assert!(check("tools/call", None).is_err());
    // Case and prefix tricks do not slip through.
    for tool in [
        "Create_Page",
        "create_page ",
        "create_page2",
        "list_pages/x",
        " list_spaces",
    ] {
        assert!(check("tools/call", Some(tool)).is_err(), "{tool:?}");
    }
}

#[test]
fn only_the_lifecycle_pair_and_tools_call_are_allowed_methods() {
    for method in [
        "tools/list",
        "resources/read",
        "resources/list",
        "prompts/get",
        "prompts/list",
        "ping",
        "completion/complete",
        "logging/setLevel",
        "sampling/createMessage",
        "notifications/cancelled",
        "notifications/roots/list_changed",
        "",
        "Initialize",
    ] {
        assert!(check(method, None).is_err(), "{method:?}");
        assert!(check(method, Some("create_page")).is_err(), "{method:?}");
    }
    // A tool name only belongs with tools/call.
    assert!(check("initialize", Some("create_page")).is_err());
    assert!(check("notifications/initialized", Some("list_spaces")).is_err());
}

#[test]
fn the_allowlist_holds_exactly_three_methods_and_three_tools() {
    assert_eq!(
        METHODS_COUNT, 3,
        "initialize, notifications/initialized, tools/call"
    );
    assert_eq!(TOOLS_COUNT, 3, "list_spaces, list_pages, create_page -- adding a tool must be a deliberate, reviewed change");
}

#[test]
fn a_refused_call_through_the_client_never_opens_a_socket() {
    let server = FakeAtomikKbServer::start();
    let client = AtomikKbClient::with_host("127.0.0.1", server.port, server.token.clone()).unwrap();
    for tool in ["append_to_page", "read_page", "search", "delete_page"] {
        let error = client
            .call_tool(tool, serde_json::json!({"pageId": "p", "markdown": "x"}))
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::Rejected(m) if m.contains("allowlist")),
            "{error}"
        );
    }
    let error = client
        .request("tools/list", None, serde_json::json!({}))
        .unwrap_err();
    assert!(
        matches!(&error, ClientError::Rejected(m) if m.contains("allowlist")),
        "{error}"
    );
    assert!(
        server.calls().is_empty(),
        "no request may reach the server: {:?}",
        server.calls()
    );
}

#[test]
fn the_client_has_no_public_method_for_anything_but_the_four_operations() {
    // Structural check on this module's own source: the only `call_tool`
    // call sites are the three allowlisted tools.
    let source = include_str!("atomik_kb_client.rs");
    let production = source
        .split("#[cfg(test)]\n#[path = \"atomik_kb_allowlist_tests.rs\"]")
        .next()
        .unwrap();
    let call_sites: Vec<&str> = production
        .match_indices("self.call_tool(\"")
        .map(|(i, _)| &production[i + 16..i + 16 + 12])
        .collect();
    assert_eq!(call_sites.len(), 3, "{call_sites:?}");
    assert!(call_sites.iter().any(|s| s.starts_with("list_spaces")));
    assert!(call_sites.iter().any(|s| s.starts_with("list_pages")));
    assert!(call_sites.iter().any(|s| s.starts_with("create_page")));
    assert!(!production.contains("self.call_tool(\"append"));
}
