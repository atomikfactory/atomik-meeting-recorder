//! The ONLY module in this codebase allowed to talk to atomik-kb's local MCP
//! server. It speaks MCP over Streamable HTTP (JSON-RPC 2.0, one `POST
//! http://127.0.0.1:<port>/mcp` endpoint) and exposes exactly four
//! operations: `initialize` (with its `notifications/initialized` follow-up),
//! `list_spaces`, `list_pages`, and `create_page` (with its full Markdown
//! content set at creation). There is no `append_to_page`, `read_page`,
//! `search`, update, rename, move or delete method anywhere on this type --
//! the hard safety rule ("create only, never update or delete anything in the
//! knowledge base") is enforced structurally by the absence of those methods,
//! and defensively by `allowlist::check`, which every outgoing JSON-RPC
//! message passes through *before* a socket is opened.
//! `atomik_kb_allowlist_tests.rs` proves every forbidden method/tool is
//! refused without any network I/O.
//!
//! Loopback only: the host is a fixed `127.0.0.1` (the server itself refuses
//! any other `Host` header and binds loopback only), `validate_host` refuses
//! anything else, and plain http is therefore safe -- the token never leaves
//! this machine.
use crate::{appflowy_credentials::SharedCredentialStore, atomik_kb_credentials::KEY_TOKEN};
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// atomik-kb's default MCP port (user-configurable in its Settings -> MCP).
pub const DEFAULT_PORT: u16 = 47811;
/// The protocol revision this client speaks (the server negotiates down).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Hard limits of the `create_page` tool, mirrored here so an over-long
/// request is refused locally instead of by the server.
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_MARKDOWN_BYTES: usize = 256 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// atomik-kb refuses request bodies over 1 MiB.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Every message this client is capable of sending must pass this check. It
/// is a fixed table, NOT derived from anything a caller could influence, so a
/// bug elsewhere in this codebase cannot smuggle a forbidden tool through.
mod allowlist {
    /// JSON-RPC methods: the lifecycle pair plus `tools/call`.
    const METHODS: &[&str] = &["initialize", "notifications/initialized", "tools/call"];
    /// The only tools ever called. Notably absent: `append_to_page`,
    /// `read_page`, `search`.
    const TOOLS: &[&str] = &["list_spaces", "list_pages", "create_page"];
    #[cfg(test)]
    pub const METHODS_COUNT: usize = METHODS.len();
    #[cfg(test)]
    pub const TOOLS_COUNT: usize = TOOLS.len();
    /// `tool` is only meaningful (and mandatory) for `tools/call`.
    pub fn check(method: &str, tool: Option<&str>) -> Result<(), String> {
        let refused = || {
            Err(format!(
                "Refused to send {method}{} to atomik-kb: not on the create-only allowlist. This is a bug -- report it rather than working around it.",
                tool.map(|t| format!(" ({t})")).unwrap_or_default()
            ))
        };
        if !METHODS.contains(&method) {
            return refused();
        }
        match (method, tool) {
            ("tools/call", Some(t)) if TOOLS.contains(&t) => Ok(()),
            ("tools/call", _) => refused(),
            (_, None) => Ok(()),
            (_, Some(_)) => refused(),
        }
    }
}

/// Only the IPv4 loopback literal or `localhost` -- the two `Host` values
/// atomik-kb accepts. Anything else (a LAN address, a hostname, a URL) is
/// refused before a client exists.
pub fn validate_host(host: &str) -> Result<(), String> {
    if host == "127.0.0.1" || host == "localhost" {
        Ok(())
    } else {
        Err("atomik-kb is only reachable on this computer (127.0.0.1); a remote host is never allowed.".into())
    }
}
pub fn validate_port(port: u16) -> Result<(), String> {
    if port == 0 {
        Err("The atomik-kb port must be between 1 and 65535.".into())
    } else {
        Ok(())
    }
}
/// A space name is matched, not sent as free text, but keep it sane.
pub fn validate_space_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        Err("The atomik-kb space name must be 1-200 plain-text characters.".into())
    } else {
        Ok(())
    }
}

/// A server error worth distinguishing: a bad token is permanent until the
/// user fixes it; a refused connection (atomik-kb closed, locked, or MCP off)
/// and 5xx are retried with backoff by the caller (`atomik_kb.rs`).
#[derive(Debug, PartialEq)]
pub enum ClientError {
    /// 401 (or no token saved): permanent until the user pastes a new token.
    Unauthorized(String),
    /// A tool-level or protocol-level refusal the server explains; not
    /// retried automatically.
    Rejected(String),
    /// Connection refused / timeout / 5xx; safe to retry with backoff.
    Transient(String),
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unauthorized(m) | ClientError::Rejected(m) | ClientError::Transient(m) => {
                write!(f, "{m}")
            }
        }
    }
}
impl std::error::Error for ClientError {}

const NOT_RUNNING: &str = "atomik-kb is not reachable. Open it and unlock it, and check Settings → MCP is on and the port matches.";
const TOKEN_REFUSED: &str = "atomik-kb refused the token. Paste a new one in Settings (atomik-kb: Settings → MCP → add client).";

fn classify(error: ureq::Error) -> ClientError {
    match error {
        ureq::Error::Status(401, _) => ClientError::Unauthorized(TOKEN_REFUSED.into()),
        ureq::Error::Status(code, response) if (400..500).contains(&code) => {
            let body = response.into_string().unwrap_or_default();
            let body = body.chars().take(200).collect::<String>();
            ClientError::Rejected(format!(
                "atomik-kb rejected the request (HTTP {code}){}",
                if body.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", body.trim())
                }
            ))
        }
        ureq::Error::Status(code, _) => {
            ClientError::Transient(format!("atomik-kb returned HTTP {code}."))
        }
        ureq::Error::Transport(_) => ClientError::Transient(NOT_RUNNING.into()),
    }
}

/// One space as `list_spaces` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub writable: bool,
}
/// One child as `list_pages` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageEntry {
    pub id: String,
    pub title: String,
    pub has_children: bool,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageList {
    pub pages: Vec<PageEntry>,
    /// atomik-kb lists at most 200 children and ends with "... and N more";
    /// a name lookup in a truncated list cannot prove a page is absent.
    pub truncated: bool,
}
impl PageList {
    /// Exact-title child lookup -- the only "does X already exist" mechanism
    /// this integration uses; never a fuzzy/partial match, since a near-miss
    /// must never be silently adopted or duplicated. Titles are compared in
    /// atomik-kb's own single-line form (`one_line`).
    pub fn child_titled(&self, title: &str) -> Option<&PageEntry> {
        let wanted = one_line(title);
        self.pages.iter().find(|p| p.title == wanted)
    }
}

/// Mirrors atomik-kb's own `one_line`: control characters and runs of
/// whitespace become one space, the ends are trimmed. Page titles are stored
/// and listed in this form, so lookups and creates must use it too.
pub fn one_line(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take_id(rest: &str) -> Option<String> {
    let id: String = rest
        .chars()
        .take_while(|c| !matches!(c, ',' | ')') && !c.is_whitespace())
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Parses `list_spaces` text: a header, then lines like
/// `- Name (spaceId: <id>, 3 pages): read and write` (or `read only`). The
/// space name may itself contain parentheses, so the LAST `(spaceId: ` on the
/// line is the marker. Unknown lines (the "no spaces shared" sentence, a
/// future header) are ignored rather than failing.
pub fn parse_spaces(text: &str) -> Vec<Space> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("- ")?;
            let at = line.rfind("(spaceId: ")?;
            let name = line[..at].trim().to_string();
            let after = &line[at + "(spaceId: ".len()..];
            let id = take_id(after)?;
            let access = after
                .split_once("):")
                .map(|(_, a)| a.to_ascii_lowercase())
                .unwrap_or_default();
            let writable = access.contains("write") && !access.contains("read only");
            Some(Space { id, name, writable })
        })
        .collect()
}

/// Parses `list_pages` text: `- Title (pageId: <id>[, N subpages])` per
/// child; "...: none." for an empty list; "... and N more" when cut at 200.
pub fn parse_pages(text: &str) -> PageList {
    let mut list = PageList::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("... and ") && line.contains(" more") {
            list.truncated = true;
            continue;
        }
        let Some(entry) = line.strip_prefix("- ") else {
            continue;
        };
        let Some(at) = entry.rfind("(pageId: ") else {
            continue;
        };
        let after = &entry[at + "(pageId: ".len()..];
        let Some(id) = take_id(after) else { continue };
        list.pages.push(PageEntry {
            id,
            title: entry[..at].trim().to_string(),
            has_children: after.contains("subpage"),
        });
    }
    list
}

/// Parses the `create_page` result,
/// `Created page "T" (pageId: <id>) in space "S" (spaceId: <id>).`. Titles and
/// space names are user text and may contain look-alike markers, so only a
/// `(pageId: <id>)` that is immediately followed by ` in space "` counts, and
/// the LAST such marker wins (the real id comes after the title). The caller
/// additionally confirms the id is listed under its parent.
pub fn parse_created_page_id(text: &str) -> Option<String> {
    let marker = "(pageId: ";
    let mut found = None;
    let mut from = 0;
    while let Some(at) = text[from..].find(marker) {
        let start = from + at + marker.len();
        if let Some(id) = take_id(&text[start..]) {
            if text[start + id.len()..].starts_with(") in space \"") {
                found = Some(id);
            }
        }
        from = start;
    }
    found
}

pub struct AtomikKbClient {
    url: String,
    token: String,
    agent: ureq::Agent,
    next_id: AtomicU64,
}
impl AtomikKbClient {
    /// Builds a client for the fixed loopback host, reading the token from the
    /// OS credential store. A missing token is `Unauthorized`, so a job fails
    /// permanently with an actionable message rather than retrying forever.
    pub fn new(port: u16, credentials: &SharedCredentialStore) -> Result<Self, ClientError> {
        let token = credentials
            .get(KEY_TOKEN)
            .map_err(ClientError::Rejected)?
            .ok_or_else(|| ClientError::Unauthorized("No atomik-kb token is saved. Paste one in Settings (atomik-kb: Settings → MCP → add client).".into()))?;
        Self::with_host("127.0.0.1", port, token).map_err(ClientError::Rejected)
    }
    pub(crate) fn with_host(host: &str, port: u16, token: String) -> Result<Self, String> {
        validate_host(host)?;
        validate_port(port)?;
        Ok(Self {
            url: format!("http://{host}:{port}/mcp"),
            token,
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(READ_TIMEOUT)
                .timeout_write(Duration::from_secs(30))
                .build(),
            next_id: AtomicU64::new(1),
        })
    }

    /// Sends one JSON-RPC message. Allowlist first; only then a socket.
    fn post(
        &self,
        method: &str,
        tool: Option<&str>,
        message: &Value,
    ) -> Result<Option<Value>, ClientError> {
        allowlist::check(method, tool).map_err(ClientError::Rejected)?;
        let body = message.to_string();
        if body.len() > MAX_BODY_BYTES {
            return Err(ClientError::Rejected(
                "The request is larger than atomik-kb accepts (1 MiB).".into(),
            ));
        }
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&body)
            .map_err(classify)?;
        if response.status() == 202 {
            return Ok(None);
        }
        let parsed: Value = response
            .into_json()
            .map_err(|e| ClientError::Rejected(format!("Unreadable atomik-kb response: {e}")))?;
        if let Some(error) = parsed.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(ClientError::Rejected(format!(
                "atomik-kb refused the request: {}",
                message.chars().take(200).collect::<String>()
            )));
        }
        Ok(Some(parsed))
    }
    fn request(
        &self,
        method: &str,
        tool: Option<&str>,
        params: Value,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.post(method, tool, &message)?
            .and_then(|v| v.get("result").cloned())
            .ok_or_else(|| ClientError::Rejected("atomik-kb sent an empty reply.".into()))
    }
    /// Runs one allowlisted tool and returns its text; `isError` results
    /// become `Rejected` with the server's own sentence (plus a hint when the
    /// cause is a missing write grant).
    fn call_tool(&self, tool: &str, arguments: Value) -> Result<String, ClientError> {
        let result = self.request(
            "tools/call",
            Some(tool),
            json!({"name": tool, "arguments": arguments}),
        )?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let mut message = format!("atomik-kb: {}", text.chars().take(300).collect::<String>());
            if text.contains("no write access") {
                message.push_str(" -- in atomik-kb: Settings → MCP, grant this client write access to the space.");
            }
            return Err(ClientError::Rejected(message));
        }
        Ok(text)
    }

    /// `initialize`, then the `notifications/initialized` the protocol
    /// requires before any tool call.
    pub fn initialize(&self) -> Result<(), ClientError> {
        self.request(
            "initialize",
            None,
            json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "atomik-meeting-recorder", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        self.post(
            "notifications/initialized",
            None,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )?;
        Ok(())
    }
    pub fn list_spaces(&self) -> Result<Vec<Space>, ClientError> {
        Ok(parse_spaces(&self.call_tool("list_spaces", json!({}))?))
    }
    pub fn list_pages(
        &self,
        space_id: &str,
        parent_id: Option<&str>,
    ) -> Result<PageList, ClientError> {
        let mut arguments = json!({"spaceId": space_id});
        if let Some(parent) = parent_id {
            arguments["parentId"] = json!(parent);
        }
        Ok(parse_pages(&self.call_tool("list_pages", arguments)?))
    }
    /// Creates a new page with its full content set at creation -- there is
    /// no follow-up call that appends to it. This is the only method in this
    /// entire module that mutates atomik-kb state.
    pub fn create_page(
        &self,
        space_id: &str,
        parent_id: Option<&str>,
        title: &str,
        markdown: Option<&str>,
    ) -> Result<String, ClientError> {
        let title = one_line(title);
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(ClientError::Rejected(format!(
                "A page title must be 1 to {MAX_TITLE_CHARS} characters."
            )));
        }
        if markdown.is_some_and(|m| m.len() > MAX_MARKDOWN_BYTES) {
            return Err(ClientError::Rejected(
                "The page content is larger than atomik-kb accepts (256 KiB).".into(),
            ));
        }
        let mut arguments = json!({"spaceId": space_id, "title": title});
        if let Some(parent) = parent_id {
            arguments["parentId"] = json!(parent);
        }
        if let Some(markdown) = markdown.filter(|m| !m.is_empty()) {
            arguments["markdown"] = json!(markdown);
        }
        let text = self.call_tool("create_page", arguments)?;
        parse_created_page_id(&text).ok_or_else(|| {
            ClientError::Rejected(
                "atomik-kb created the page but its id could not be read from the reply.".into(),
            )
        })
    }
}

#[cfg(test)]
#[path = "atomik_kb_allowlist_tests.rs"]
mod allowlist_tests;

#[cfg(test)]
#[path = "atomik_kb_client_tests.rs"]
mod client_tests;
