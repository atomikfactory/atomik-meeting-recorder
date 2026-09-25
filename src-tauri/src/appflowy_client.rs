//! The ONLY module in this codebase allowed to talk to an AppFlowy Cloud
//! server. It exposes exactly six operations: request an OTP, verify an OTP,
//! refresh the session, list workspaces, read a folder tree, and create a
//! page (with its full content set at creation). There is no update, delete,
//! move, rename, publish, or append-block method anywhere on this type --
//! the hard safety rule ("create only, never update or delete anything in
//! AppFlowy") is enforced structurally by the absence of those methods, and
//! defensively by `allowlist::check`, which every outgoing request passes
//! through *before* a socket is opened. `allowlist_tests.rs` proves every
//! forbidden (method, path) combination is refused without any network I/O.
use crate::appflowy_credentials::{SharedCredentialStore, KEY_ACCESS_TOKEN, KEY_EMAIL, KEY_REFRESH_TOKEN};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// AppFlowy Cloud enforces a `Client-Version` compatibility window on
/// `/api/workspace/*` routes (see `libs/client-api/src/http.rs` in
/// AppFlowy-Cloud, which clamps requests against `min_version`/`max_version`).
/// The research pass behind this integration found the currently-enforced
/// range reported as `0.6.7 <= version < 1.0.0` (unverified against source
/// directly; the newest tag actually observed on `main` was `0.9.64`). Pin to
/// that observed tag rather than inventing a version; bump this constant if a
/// server starts rejecting it.
pub const CLIENT_VERSION: &str = "0.9.64";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Every request this client is capable of issuing must match one of these
/// (method, path-pattern) tuples. `{}` in a pattern matches exactly one
/// non-empty path segment. This is intentionally hard-coded and NOT derived
/// from caller input, so a bug elsewhere in this codebase cannot smuggle a
/// forbidden route/method through -- there is no code path that lets a
/// caller supply an arbitrary method or an arbitrary path template.
mod allowlist {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Method {
        Get,
        Post,
    }
    const ALLOWED: &[(Method, &str)] = &[
        (Method::Post, "/gotrue/otp"),
        (Method::Post, "/gotrue/verify"),
        (Method::Post, "/gotrue/token"),
        (Method::Get, "/api/workspace"),
        (Method::Get, "/api/workspace/{}/folder"),
        (Method::Get, "/api/workspace/{}/page-view/{}"),
        (Method::Post, "/api/workspace/{}/page-view"),
    ];
    fn path_matches(pattern: &str, path: &str) -> bool {
        let pattern_segments: Vec<&str> = pattern.split('/').collect();
        let path_segments: Vec<&str> = path.split('/').collect();
        if pattern_segments.len() != path_segments.len() {
            return false;
        }
        pattern_segments
            .iter()
            .zip(path_segments.iter())
            .all(|(p, s)| if *p == "{}" { !s.is_empty() } else { p == s })
    }
    /// Checked before any socket is opened. `path` must not include a query
    /// string (query parameters, e.g. `?depth=`, are never part of the
    /// allowlist match and never affect it either way).
    pub fn check(method: Method, path: &str) -> Result<(), String> {
        if ALLOWED.iter().any(|(m, pattern)| *m == method && path_matches(pattern, path)) {
            Ok(())
        } else {
            Err(format!(
                "Refused to call {method:?} {path}: not on the AppFlowy create-only allowlist. This is a bug -- report it rather than working around it."
            ))
        }
    }
}
use allowlist::Method;

/// A page-view layout, matching AppFlowy Cloud's `ViewLayout` repr(u8).
/// Only `Document` is ever used by this integration.
pub const LAYOUT_DOCUMENT: u8 = 0;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub workspace_name: Option<String>,
}
#[derive(Debug, Deserialize)]
struct WorkspaceListResponse {
    #[serde(default)]
    data: Vec<Workspace>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FolderView {
    pub view_id: String,
    #[serde(default)]
    pub parent_view_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_space: bool,
    #[serde(default)]
    pub children: Vec<FolderView>,
}
impl FolderView {
    /// Exact-name child lookup -- the only "does X already exist" mechanism
    /// this integration uses (see module docs); never a fuzzy/partial match,
    /// since a near-miss must never be silently adopted or duplicated.
    pub fn child_named(&self, name: &str) -> Option<&FolderView> {
        self.children.iter().find(|c| c.name == name)
    }
}
#[derive(Debug, Deserialize)]
struct FolderResponse {
    data: FolderView,
}

#[derive(Debug, Serialize)]
struct CreatePageParams<'a> {
    parent_view_id: &'a str,
    layout: u8,
    name: &'a str,
    page_data: &'a serde_json::Value,
    view_id: &'a str,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub view_id: String,
}
#[derive(Debug, Deserialize)]
struct PageResponse {
    data: Page,
}

#[derive(Debug, Serialize)]
struct OtpRequest<'a> {
    email: &'a str,
    should_create_user: bool,
}
#[derive(Debug, Serialize)]
struct VerifyOtpRequest<'a> {
    email: &'a str,
    token: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
}
#[derive(Debug, Serialize)]
struct RefreshTokenRequest<'a> {
    refresh_token: &'a str,
}
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A server error worth distinguishing: 401 triggers exactly one refresh +
/// retry; other 4xx are permanent (surfaced verbatim to the user); network
/// errors and 5xx are retried with backoff by the caller (`appflowy.rs`).
#[derive(Debug)]
pub enum ClientError {
    Unauthorized,
    /// A 4xx the server explains; not retried automatically.
    Rejected(String),
    /// Network/5xx; safe to retry with backoff.
    Transient(String),
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unauthorized => write!(f, "AppFlowy session expired."),
            ClientError::Rejected(m) | ClientError::Transient(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for ClientError {}

fn classify(error: ureq::Error) -> ClientError {
    match error {
        ureq::Error::Status(401, _) => ClientError::Unauthorized,
        ureq::Error::Status(code, response) if (400..500).contains(&code) => {
            let message = response
                .into_string()
                .unwrap_or_else(|_| "invalid response body".into());
            ClientError::Rejected(format!("AppFlowy rejected the request (HTTP {code}): {message}"))
        }
        ureq::Error::Status(code, _) => {
            ClientError::Transient(format!("AppFlowy returned HTTP {code}."))
        }
        ureq::Error::Transport(_) => {
            ClientError::Transient("AppFlowy Cloud is not reachable right now.".into())
        }
    }
}

/// Validates the configured base URL: https, except loopback (http allowed
/// only for `127.0.0.1`/`localhost`, and only so the fake server in this
/// crate's own tests can use plain HTTP -- a real deployment is always TLS).
pub fn validate_base_url(url: &str) -> Result<(), String> {
    if let Some(rest) = url.strip_prefix("https://") {
        if rest.is_empty() {
            return Err("AppFlowy base URL is missing a host.".into());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if host == "127.0.0.1" || host == "localhost" || host == "::1" {
            return Ok(());
        }
        return Err("AppFlowy base URL must use https:// (plain http:// is only allowed for a loopback test server).".into());
    }
    Err("AppFlowy base URL must start with https:// or http://.".into())
}

pub struct AppFlowyClient {
    base_url: String,
    agent: ureq::Agent,
    credentials: SharedCredentialStore,
}
impl AppFlowyClient {
    pub fn new(base_url: &str, credentials: SharedCredentialStore) -> Result<Self, String> {
        validate_base_url(base_url)?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(READ_TIMEOUT)
                .timeout_write(Duration::from_secs(30))
                .build(),
            credentials,
        })
    }

    fn store_session(&self, tokens: &TokenResponse) -> Result<(), String> {
        self.credentials.set(KEY_ACCESS_TOKEN, &tokens.access_token)?;
        self.credentials.set(KEY_REFRESH_TOKEN, &tokens.refresh_token)
    }

    /// Sends the one-time-passcode email. Never handles a password (see
    /// `docs/appflowy.md` for why: magic-link/OTP is the only auth flow this
    /// client implements).
    pub fn request_otp(&self, email: &str) -> Result<(), String> {
        allowlist::check(Method::Post, "/gotrue/otp")?;
        self.agent
            .post(&format!("{}/gotrue/otp", self.base_url))
            .set("Client-Version", CLIENT_VERSION)
            .send_json(OtpRequest { email, should_create_user: true })
            .map_err(|e| classify(e).to_string())?;
        self.credentials.set(KEY_EMAIL, email)?;
        Ok(())
    }
    /// Verifies the 6-digit code the user received by email and stores the
    /// resulting session in the credential store.
    pub fn verify_otp(&self, email: &str, code: &str) -> Result<(), String> {
        allowlist::check(Method::Post, "/gotrue/verify")?;
        let response = self
            .agent
            .post(&format!("{}/gotrue/verify", self.base_url))
            .set("Client-Version", CLIENT_VERSION)
            .send_json(VerifyOtpRequest { email, token: code, kind: "magiclink" })
            .map_err(|e| classify(e).to_string())?;
        let tokens: TokenResponse = response.into_json().map_err(err)?;
        self.store_session(&tokens)?;
        self.credentials.set(KEY_EMAIL, email)
    }
    pub fn signed_in_email(&self) -> Result<Option<String>, String> {
        self.credentials.get(KEY_EMAIL)
    }
    pub fn is_signed_in(&self) -> Result<bool, String> {
        Ok(self.credentials.get(KEY_ACCESS_TOKEN)?.is_some())
    }
    /// Deletes only this app's own credential-store entries.
    pub fn sign_out(&self) -> Result<(), String> {
        for key in [KEY_ACCESS_TOKEN, KEY_REFRESH_TOKEN, KEY_EMAIL] {
            self.credentials.delete(key)?;
        }
        Ok(())
    }
    fn refresh(&self) -> Result<String, String> {
        allowlist::check(Method::Post, "/gotrue/token")?;
        let refresh_token = self
            .credentials
            .get(KEY_REFRESH_TOKEN)?
            .ok_or("Not signed in to AppFlowy.")?;
        let response = self
            .agent
            .post(&format!("{}/gotrue/token?grant_type=refresh_token", self.base_url))
            .set("Client-Version", CLIENT_VERSION)
            .send_json(RefreshTokenRequest { refresh_token: &refresh_token })
            .map_err(|e| classify(e).to_string())?;
        let tokens: TokenResponse = response.into_json().map_err(err)?;
        self.store_session(&tokens)?;
        Ok(tokens.access_token)
    }
    fn access_token(&self) -> Result<String, String> {
        self.credentials
            .get(KEY_ACCESS_TOKEN)?
            .ok_or_else(|| "Not signed in to AppFlowy.".to_string())
    }

    /// Runs one authenticated GET, refreshing the access token exactly once
    /// on a 401 before giving up.
    // ureq's own `Result<_, ureq::Error>` is what the inner `attempt` closure
    // returns before this method narrows it to `ClientError`; boxing it would
    // just move the same size complaint into ureq's own API.
    #[allow(clippy::result_large_err)]
    fn get_authed(&self, method: Method, path: &str) -> Result<ureq::Response, ClientError> {
        allowlist::check(method, path).map_err(ClientError::Rejected)?;
        let token = self.access_token().map_err(ClientError::Rejected)?;
        let attempt = |token: &str| {
            self.agent
                .get(&format!("{}{}", self.base_url, path))
                .set("Client-Version", CLIENT_VERSION)
                .set("Authorization", &format!("Bearer {token}"))
                .call()
        };
        match attempt(&token) {
            Ok(response) => Ok(response),
            Err(e) => match classify(e) {
                ClientError::Unauthorized => {
                    let refreshed = self.refresh().map_err(ClientError::Rejected)?;
                    attempt(&refreshed).map_err(classify)
                }
                other => Err(other),
            },
        }
    }
    #[allow(clippy::result_large_err)]
    fn post_authed_json(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<ureq::Response, ClientError> {
        allowlist::check(Method::Post, path).map_err(ClientError::Rejected)?;
        let token = self.access_token().map_err(ClientError::Rejected)?;
        let attempt = |token: &str| {
            self.agent
                .post(&format!("{}{}", self.base_url, path))
                .set("Client-Version", CLIENT_VERSION)
                .set("Authorization", &format!("Bearer {token}"))
                .send_json(serde_json::to_value(body).unwrap_or_default())
        };
        match attempt(&token) {
            Ok(response) => Ok(response),
            Err(e) => match classify(e) {
                ClientError::Unauthorized => {
                    let refreshed = self.refresh().map_err(ClientError::Rejected)?;
                    attempt(&refreshed).map_err(classify)
                }
                other => Err(other),
            },
        }
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, ClientError> {
        let response = self.get_authed(Method::Get, "/api/workspace")?;
        let parsed: WorkspaceListResponse = response
            .into_json()
            .map_err(|e| ClientError::Rejected(format!("Unreadable workspace list: {e}")))?;
        Ok(parsed.data)
    }
    /// `depth=4` covers workspace -> "My daily meetings" -> date -> meeting,
    /// the full hierarchy this integration ever creates.
    #[allow(clippy::result_large_err)]
    pub fn get_folder(&self, workspace_id: &str, root_view_id: Option<&str>) -> Result<FolderView, ClientError> {
        let path = format!("/api/workspace/{workspace_id}/folder");
        let query = match root_view_id {
            Some(id) => format!("?depth=4&root_view_id={id}"),
            None => "?depth=4".to_string(),
        };
        allowlist::check(Method::Get, &path).map_err(ClientError::Rejected)?;
        let token = self.access_token().map_err(ClientError::Rejected)?;
        let attempt = |token: &str| {
            self.agent
                .get(&format!("{}{}{}", self.base_url, path, query))
                .set("Client-Version", CLIENT_VERSION)
                .set("Authorization", &format!("Bearer {token}"))
                .call()
        };
        let response = match attempt(&token) {
            Ok(response) => response,
            Err(e) => match classify(e) {
                ClientError::Unauthorized => {
                    let refreshed = self.refresh().map_err(ClientError::Rejected)?;
                    attempt(&refreshed).map_err(classify)?
                }
                other => return Err(other),
            },
        };
        let parsed: FolderResponse = response
            .into_json()
            .map_err(|e| ClientError::Rejected(format!("Unreadable folder tree: {e}")))?;
        Ok(parsed.data)
    }
    /// Creates a new page with its full content set at creation
    /// (`page_data`) -- there is no follow-up call that appends to it. This
    /// is the only method in this entire module that mutates AppFlowy state.
    pub fn create_page(
        &self,
        workspace_id: &str,
        parent_view_id: &str,
        view_id: &str,
        name: &str,
        page_data: &serde_json::Value,
    ) -> Result<Page, ClientError> {
        let path = format!("/api/workspace/{workspace_id}/page-view");
        let response = self.post_authed_json(
            &path,
            &CreatePageParams { parent_view_id, layout: LAYOUT_DOCUMENT, name, page_data, view_id },
        )?;
        let parsed: PageResponse = response
            .into_json()
            .map_err(|e| ClientError::Rejected(format!("Unreadable create-page response: {e}")))?;
        Ok(parsed.data)
    }
}

#[cfg(test)]
#[path = "appflowy_allowlist_tests.rs"]
mod allowlist_tests;

#[cfg(test)]
#[path = "appflowy_client_wire_tests.rs"]
mod wire_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base_url_requires_https_except_loopback() {
        assert!(validate_base_url("https://beta.appflowy.cloud").is_ok());
        assert!(validate_base_url("http://127.0.0.1:9999").is_ok());
        assert!(validate_base_url("http://localhost:9999").is_ok());
        assert!(validate_base_url("http://example.com").is_err());
        assert!(validate_base_url("http://beta.appflowy.cloud").is_err());
        assert!(validate_base_url("ftp://beta.appflowy.cloud").is_err());
        assert!(validate_base_url("https://").is_err());
    }
    #[test]
    fn folder_view_finds_exact_name_match_only() {
        let root = FolderView {
            view_id: "root".into(),
            parent_view_id: None,
            name: "Workspace".into(),
            is_space: true,
            children: vec![FolderView {
                view_id: "daily".into(),
                parent_view_id: Some("root".into()),
                name: "My daily meetings".into(),
                is_space: false,
                children: vec![],
            }],
        };
        assert!(root.child_named("My daily meetings").is_some());
        assert!(root.child_named("my daily meetings").is_none());
        assert!(root.child_named("My daily meeting").is_none());
    }
}
