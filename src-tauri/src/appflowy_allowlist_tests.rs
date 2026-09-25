//! Proves every forbidden AppFlowy call is refused by `allowlist::check`
//! itself -- a pure function with no I/O -- so this test suite never opens a
//! socket to prove the refusal. `appflowy_fake_server_tests.rs` separately
//! proves the *allowed* calls actually reach a server correctly; this file
//! only proves the hard safety rule's negative space.
use super::allowlist::{check, Method};

#[test]
fn every_mutating_or_deleting_page_view_route_is_refused() {
    // The only allowed page-view calls: GET a single page, POST to create.
    assert!(check(Method::Get, "/api/workspace/w1/page-view/v1").is_ok());
    assert!(check(Method::Post, "/api/workspace/w1/page-view").is_ok());
    // Everything that mutates, appends to, or deletes an existing page is
    // refused, under both methods a REST API might plausibly use for it (the
    // real AppFlowy-Cloud routes are POST/PATCH; GET is checked too as a
    // defence against a future route-table typo).
    for path in [
        "/api/workspace/w1/page-view/v1",       // PATCH UpdatePageParams in the real API
        "/api/workspace/w1/page-view/v1/update-name",
        "/api/workspace/w1/page-view/v1/update-icon",
        "/api/workspace/w1/page-view/v1/append-block",
        "/api/workspace/w1/page-view/v1/publish",
        "/api/workspace/w1/page-view/v1/unpublish",
        "/api/workspace/w1/page-view/v1/move",
        "/api/workspace/w1/page-view/v1/trash",
    ] {
        assert!(check(Method::Post, path).is_err(), "expected POST {path} to be refused");
        if path != "/api/workspace/w1/page-view/v1" {
            assert!(check(Method::Get, path).is_err(), "expected GET {path} to be refused");
        }
    }
    // Listing (not fetching one page) or creating without a body-less name
    // are not modeled either.
    assert!(check(Method::Get, "/api/workspace/w1/page-view").is_err());
}

#[test]
fn move_and_trash_routes_are_refused() {
    for path in [
        "/api/workspace/w1/page-view/v1/move",
        "/api/workspace/w1/page-view/v1/trash",
        "/api/workspace/w1/trash",
        "/api/workspace/w1/page-view/v1/favorite",
    ] {
        assert!(check(Method::Post, path).is_err(), "{path}");
        assert!(check(Method::Get, path).is_err(), "{path}");
    }
}

#[test]
fn password_grant_is_not_a_distinct_allowed_route_but_shares_the_token_path() {
    // Refresh-grant and password-grant both post to `/gotrue/token`; this
    // client's `refresh()` method only ever sends a refresh-token body (see
    // `appflowy_client.rs`), so password grant is refused not at the route
    // level but by construction -- there is no method on `AppFlowyClient`
    // that can send a password. The route itself must still be reachable for
    // the refresh flow.
    assert!(check(Method::Post, "/gotrue/token").is_ok());
}

#[test]
fn admin_routes_are_refused() {
    for path in ["/gotrue/admin/users", "/gotrue/admin/generate_link"] {
        assert!(check(Method::Post, path).is_err(), "{path}");
        assert!(check(Method::Get, path).is_err(), "{path}");
    }
}

#[test]
fn gotrue_signup_and_logout_are_refused() {
    for path in ["/gotrue/signup", "/gotrue/logout", "/gotrue/user"] {
        assert!(check(Method::Post, path).is_err(), "{path}");
        assert!(check(Method::Get, path).is_err(), "{path}");
    }
}

#[test]
fn oauth_authorize_route_is_refused() {
    assert!(check(Method::Get, "/gotrue/authorize").is_err());
}

#[test]
fn allowed_routes_are_exactly_the_documented_set() {
    assert!(check(Method::Post, "/gotrue/otp").is_ok());
    assert!(check(Method::Post, "/gotrue/verify").is_ok());
    assert!(check(Method::Post, "/gotrue/token").is_ok());
    assert!(check(Method::Get, "/api/workspace").is_ok());
    assert!(check(Method::Get, "/api/workspace/w1/folder").is_ok());
    assert!(check(Method::Get, "/api/workspace/w1/page-view/v1").is_ok());
    assert!(check(Method::Post, "/api/workspace/w1/page-view").is_ok());
}

#[test]
fn a_path_pattern_never_matches_a_prefix_or_extra_segment() {
    // Guards against a sloppy `starts_with`-style matcher that would let
    // "/api/workspace/w1/page-view/v1/append-block" slip through because it
    // shares a prefix with the allowed "/api/workspace/w1/page-view/v1" GET.
    assert!(check(Method::Get, "/api/workspace/w1/page-view/v1/append-block").is_err());
    assert!(check(Method::Get, "/api/workspace/w1/page-view/v1/extra/segment").is_err());
    assert!(check(Method::Post, "/api/workspace/w1/page-view/extra").is_err());
}
