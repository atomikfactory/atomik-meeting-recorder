//! An in-process fake atomik-kb MCP server (`POST /mcp`, JSON-RPC 2.0 over
//! loopback HTTP) used by `atomik_kb_client.rs`'s wire-level tests and
//! `atomik_kb.rs`'s export-orchestration tests. No real network, no real
//! knowledge base, ever. It models the real server's observable behaviour
//! (bearer-token auth with 401, per-space read/write grants with `isError`
//! results, titles stored in single-line form, the exact text formats of
//! `list_spaces` / `list_pages` / `create_page`) so this codebase's own
//! client and service are proven against the documented wire protocol; see
//! `atomik_kb_allowlist_tests.rs` for the (separate, socket-free) proof that
//! anything else is refused before it could reach a server like this one.
use crate::atomik_kb_client::one_line;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

#[derive(Clone, Debug)]
struct FakeSpace {
    id: String,
    name: String,
    write: bool,
    /// False simulates a space the client has no grant for at all: it is not
    /// listed by `list_spaces` and every tool on it is denied.
    granted: bool,
}
#[derive(Clone, Debug)]
struct FakePage {
    space_id: String,
    parent: Option<String>,
    title: String,
    markdown: Option<String>,
    /// Stored but never listed, simulating a server that accepted a create
    /// without (yet) showing the page under its parent.
    hidden: bool,
}
struct FakeState {
    token: String,
    spaces: Vec<FakeSpace>,
    pages: HashMap<String, FakePage>,
    order: Vec<String>,
    hide_created: bool,
    fail_next_create_text: Option<String>,
    fail_next_create_http: Option<u16>,
    /// Repeated, so `list_pages` mimics the real "... and N more" cut-off.
    list_cap: Option<usize>,
}

/// One request as it reached the server, for asserting what was (never) sent.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub rpc_method: String,
    pub tool: Option<String>,
    pub authorized: bool,
    pub host: String,
}

pub struct FakeAtomikKbServer {
    pub port: u16,
    pub token: String,
    /// The id of the default "My Daily Meeting" space.
    pub space_id: String,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for FakeAtomikKbServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Nudge the accept() loop so it observes the stop flag promptly.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl FakeAtomikKbServer {
    /// One writable space named "My Daily Meeting" and no pages yet.
    pub fn start() -> Self {
        Self::start_with_space("My Daily Meeting", true)
    }
    pub fn start_with_space(name: &str, write: bool) -> Self {
        let space_id = uuid::Uuid::new_v4().to_string();
        let token = crate::atomik_kb_credentials::FAKE_TOKEN.to_string();
        let state = FakeState {
            token: token.clone(),
            spaces: vec![FakeSpace {
                id: space_id.clone(),
                name: name.into(),
                write,
                granted: true,
            }],
            pages: HashMap::new(),
            order: Vec::new(),
            hide_created: false,
            fail_next_create_text: None,
            fail_next_create_http: None,
            list_cap: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let (calls_bg, state_bg, stop_bg) = (calls.clone(), state.clone(), stop.clone());
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_bg.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                handle_connection(&mut stream, &calls_bg, &state_bg);
            }
        });
        Self {
            port,
            token,
            space_id,
            calls,
            state,
            stop,
            handle: Some(handle),
        }
    }
    /// A server with no space the client may use at all.
    pub fn start_with_no_space() -> Self {
        let server = Self::start();
        server.state.lock().unwrap().spaces.clear();
        server
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
    pub fn tool_calls(&self, tool: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.tool.as_deref() == Some(tool))
            .count()
    }
    pub fn creates(&self) -> usize {
        self.tool_calls("create_page")
    }
    /// Titles of the visible children of `parent` (`None` = the space's top level).
    pub fn child_titles(&self, parent: Option<&str>) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.order
            .iter()
            .filter_map(|id| s.pages.get(id))
            .filter(|p| !p.hidden && p.parent.as_deref() == parent)
            .map(|p| p.title.clone())
            .collect()
    }
    pub fn child_ids(&self, parent: Option<&str>) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.order
            .iter()
            .filter(|id| {
                s.pages
                    .get(*id)
                    .is_some_and(|p| !p.hidden && p.parent.as_deref() == parent)
            })
            .cloned()
            .collect()
    }
    pub fn markdown_of(&self, page_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .pages
            .get(page_id)
            .and_then(|p| p.markdown.clone())
    }
    /// Simulates a page that already exists (a user's own, or one created in a
    /// previous run whose local journal update never landed).
    pub fn seed_page(&self, parent: Option<&str>, title: &str) -> String {
        let mut s = self.state.lock().unwrap();
        let space_id = s.spaces.first().map(|sp| sp.id.clone()).unwrap_or_default();
        let id = uuid::Uuid::new_v4().to_string();
        s.pages.insert(
            id.clone(),
            FakePage {
                space_id,
                parent: parent.map(str::to_string),
                title: one_line(title),
                markdown: None,
                hidden: false,
            },
        );
        s.order.push(id.clone());
        id
    }
    pub fn add_space(&self, name: &str, write: bool) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.state.lock().unwrap().spaces.push(FakeSpace {
            id: id.clone(),
            name: name.into(),
            write,
            granted: true,
        });
        id
    }
    /// After this call, every subsequently created page is stored but never
    /// listed under its parent.
    pub fn omit_created_children(&self) {
        self.state.lock().unwrap().hide_created = true;
    }
    /// The next `create_page` answers a tool-level `isError` result.
    pub fn fail_next_create_with_tool_error(&self, text: &str) {
        self.state.lock().unwrap().fail_next_create_text = Some(text.to_string());
    }
    /// The next `create_page` answers this HTTP status instead.
    pub fn fail_next_create_with_http(&self, status: u16) {
        self.state.lock().unwrap().fail_next_create_http = Some(status);
    }
    /// `list_pages` shows at most `cap` children, then "... and N more".
    pub fn cap_list_pages(&self, cap: usize) {
        self.state.lock().unwrap().list_cap = Some(cap);
    }
}

fn read_full_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let content_length = String::from_utf8_lossy(&buffer[..header_end])
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= header_end + 4 + content_length {
            break;
        }
    }
    buffer
}
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}
fn respond_status(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
fn rpc_result(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}
fn rpc_error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
fn tool_result(id: &Value, text: String, is_error: bool) -> String {
    rpc_result(
        id,
        json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
    )
}

fn handle_connection(
    stream: &mut TcpStream,
    calls: &Arc<Mutex<Vec<RecordedCall>>>,
    state: &Arc<Mutex<FakeState>>,
) {
    let raw = read_full_request(stream);
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let request_line = headers.lines().next().unwrap_or("").to_string();
    let body_start = (header_end + 4).min(raw.len());
    let message: Value = serde_json::from_slice(&raw[body_start..]).unwrap_or(Value::Null);
    let rpc_method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let tool = (rpc_method == "tools/call").then(|| {
        params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    let mut s = state.lock().unwrap();
    let authorized =
        header_value(&headers, "Authorization") == Some(format!("Bearer {}", s.token).as_str());
    calls.lock().unwrap().push(RecordedCall {
        rpc_method: rpc_method.clone(),
        tool: tool.clone(),
        authorized,
        host: header_value(&headers, "Host").unwrap_or("").to_string(),
    });
    if !request_line.starts_with("POST /mcp ") {
        respond_status(stream, "404 Not Found", "{}");
        return;
    }
    if !authorized {
        respond_status(stream, "401 Unauthorized", "{}");
        return;
    }
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    match rpc_method.as_str() {
        "initialize" => respond_status(
            stream,
            "200 OK",
            &rpc_result(
                &id,
                json!({"protocolVersion": "2025-06-18", "serverInfo": {"name": "atomik-kb", "version": "test"}, "capabilities": {"tools": {"listChanged": false}}}),
            ),
        ),
        "notifications/initialized" => respond_status(stream, "202 Accepted", ""),
        "tools/call" => {
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let tool_name = tool.unwrap_or_default();
            if tool_name == "create_page" {
                if let Some(status) = s.fail_next_create_http.take() {
                    respond_status(stream, &format!("{status} Injected Failure"), "{}");
                    return;
                }
            }
            let (text, is_error) = match tool_name.as_str() {
                "list_spaces" => (list_spaces_text(&s), false),
                "list_pages" => run_list_pages(&s, &arguments),
                "create_page" => run_create_page(&mut s, &arguments),
                other => {
                    respond_status(
                        stream,
                        "200 OK",
                        &rpc_error(&id, -32602, &format!("unknown tool {other}")),
                    );
                    return;
                }
            };
            respond_status(stream, "200 OK", &tool_result(&id, text, is_error));
        }
        _ => respond_status(
            stream,
            "200 OK",
            &rpc_error(&id, -32601, "method not found"),
        ),
    }
}

fn list_spaces_text(s: &FakeState) -> String {
    let granted: Vec<&FakeSpace> = s.spaces.iter().filter(|sp| sp.granted).collect();
    if granted.is_empty() {
        return "No spaces are shared with this client yet. The person can grant access in Settings.".into();
    }
    let mut out = format!("Spaces you can access ({}):", granted.len());
    for space in granted {
        let pages = s.pages.values().filter(|p| p.space_id == space.id).count();
        let access = if space.write {
            "read and write"
        } else {
            "read only"
        };
        out.push_str(&format!(
            "\n- {} (spaceId: {}, {} page{}): {access}",
            space.name,
            space.id,
            pages,
            if pages == 1 { "" } else { "s" }
        ));
    }
    out
}
fn find_space<'a>(s: &'a FakeState, id: &str) -> Option<&'a FakeSpace> {
    s.spaces.iter().find(|sp| sp.id == id && sp.granted)
}
fn run_list_pages(s: &FakeState, arguments: &Value) -> (String, bool) {
    let space_id = arguments
        .get("spaceId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(space) = find_space(s, space_id) else {
        return ("This client has no read access to that space".into(), true);
    };
    let parent = arguments
        .get("parentId")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let heading = match parent {
        None => format!("Top-level pages in \"{}\"", space.name),
        Some(parent_id) => match s.pages.get(parent_id) {
            Some(page) if page.space_id == space.id => format!("Subpages of \"{}\"", page.title),
            _ => return ("Page not found".into(), true),
        },
    };
    let children: Vec<(&String, &FakePage)> = s
        .order
        .iter()
        .filter_map(|id| s.pages.get(id).map(|p| (id, p)))
        .filter(|(_, p)| !p.hidden && p.space_id == space.id && p.parent.as_deref() == parent)
        .collect();
    if children.is_empty() {
        return (format!("{heading}: none."), false);
    }
    let cap = s.list_cap.unwrap_or(200);
    let mut out = format!("{heading} ({}):", children.len());
    for (id, page) in children.iter().take(cap) {
        let subpages = s
            .pages
            .values()
            .filter(|p| !p.hidden && p.parent.as_deref() == Some(id.as_str()))
            .count();
        out.push_str(&format!("\n- {} (pageId: {id}", page.title));
        if subpages > 0 {
            out.push_str(&format!(
                ", {subpages} subpage{}",
                if subpages == 1 { "" } else { "s" }
            ));
        }
        out.push(')');
    }
    if children.len() > cap {
        out.push_str(&format!(
            "\n... and {} more. Use search to find a particular page.",
            children.len() - cap
        ));
    }
    (out, false)
}
fn run_create_page(s: &mut FakeState, arguments: &Value) -> (String, bool) {
    if let Some(text) = s.fail_next_create_text.take() {
        return (text, true);
    }
    let space_id = arguments
        .get("spaceId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(space) = find_space(s, &space_id).cloned() else {
        return ("This client has no write access to that space".into(), true);
    };
    if !space.write {
        return ("This client has no write access to that space".into(), true);
    }
    let parent = arguments
        .get("parentId")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    if let Some(parent_id) = &parent {
        if !s
            .pages
            .get(parent_id)
            .is_some_and(|p| p.space_id == space.id)
        {
            return ("Parent page not found".into(), true);
        }
    }
    let title = one_line(arguments.get("title").and_then(Value::as_str).unwrap_or(""));
    if title.is_empty() || title.chars().count() > 200 {
        return (
            "create_page: title must be 1 to 200 characters".into(),
            true,
        );
    }
    let markdown = arguments
        .get("markdown")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let id = uuid::Uuid::new_v4().to_string();
    let hidden = s.hide_created;
    s.pages.insert(
        id.clone(),
        FakePage {
            space_id: space.id.clone(),
            parent,
            title: title.clone(),
            markdown,
            hidden,
        },
    );
    s.order.push(id.clone());
    (
        format!(
            "Created page \"{title}\" (pageId: {id}) in space \"{}\" (spaceId: {}).",
            space.name, space.id
        ),
        false,
    )
}
