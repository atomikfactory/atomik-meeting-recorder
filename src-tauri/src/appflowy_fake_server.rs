//! An in-process fake AppFlowy Cloud server (GoTrue + workspace/folder/page
//! routes only) used by `appflowy_client.rs`'s wire-level tests and
//! `appflowy.rs`'s export-orchestration tests. No real network, no real
//! AppFlowy server, ever -- this only proves this codebase's own client and
//! service speak the documented wire protocol and only ever call the
//! documented create-only routes; see `appflowy_allowlist_tests.rs` for the
//! (separate, socket-free) proof that anything else is refused before it
//! could reach a server like this one.
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
struct FakeNode {
    name: String,
    is_space: bool,
    children: Vec<String>,
}
struct FakeState {
    nodes: HashMap<String, FakeNode>,
    page_data: HashMap<String, serde_json::Value>,
    /// Models AppFlowy-Cloud's real `create_document_page` semantics: the
    /// document collab is stored under
    /// `collab_id.unwrap_or(Uuid::new_v4())`, the *page* (view) under
    /// `view_id.unwrap_or(collab_id)` -- independently. `GET
    /// .../page-view/{view_id}` (the real client-facing "does this page's
    /// document actually open" check) only succeeds when a collab was really
    /// stored under that exact `view_id`, i.e. when the request's
    /// `collab_id` equalled its `view_id`. A request that omits `collab_id`
    /// (the pre-fix bug this fake reproduces) stores the collab under a
    /// fresh random id the caller never learns, so this map records `false`
    /// for that `view_id` -- "Collab not found" on lookup, exactly like the
    /// real server.
    collab_ok: HashMap<String, bool>,
    workspace_id: String,
    access_token: String,
    refresh_token: String,
    otp_email: Option<String>,
    otp_code: String,
    refresh_count: u32,
    fail_next_page_view: Option<(u16, String)>,
    /// When set, a newly-created page is stored (so `page_data_for` and a
    /// later `create_page` retry still see it) but never linked into its
    /// parent's `children` list -- simulating a server that accepted a page
    /// creation yet doesn't (yet) list it under its parent, to exercise
    /// `appflowy.rs`'s post-create verification failing the job instead of
    /// reporting success.
    hide_created_children: bool,
    /// When set, every subsequently created page is stored with no working
    /// collab (`collab_ok=false`) regardless of what `collab_id` the request
    /// sent -- simulates a server-side failure to persist the document
    /// collab, to exercise `appflowy.rs`'s post-create content-load
    /// verification failing the job instead of reporting success.
    force_missing_collab: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
}

pub struct FakeAppFlowyServer {
    pub base_url: String,
    pub workspace_id: String,
    /// The id of the default "General" Space seeded by `start()` /
    /// `start_with_existing_top_level_page()` -- the destination
    /// `resolve_root_parent` should land on when settings don't pin an
    /// explicit `parent_view_id`.
    pub space_id: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for FakeAppFlowyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Nudge the accept() loop so it observes the stop flag promptly.
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl FakeAppFlowyServer {
    /// A fresh server with one Space ("General", so it's picked as the
    /// default destination) and no "My daily meetings" page yet, with a
    /// valid initial session (`access-1`/`refresh-1`).
    pub fn start() -> Self {
        Self::start_with_spaces(&["General"])
    }
    /// A fresh server whose workspace root has exactly the given Spaces (in
    /// order), none of them named "General" unless included -- for testing
    /// the "first space by order" fallback and multi-space workspaces. The
    /// first Space's id is `self.space_id`.
    pub fn start_with_spaces(space_names: &[&str]) -> Self {
        let workspace_id = "workspace-1".to_string();
        let mut nodes = HashMap::new();
        let mut space_ids = Vec::new();
        for (i, name) in space_names.iter().enumerate() {
            let id = format!("space-{i}");
            nodes.insert(id.clone(), FakeNode { name: name.to_string(), is_space: true, children: vec![] });
            space_ids.push(id);
        }
        nodes.insert(workspace_id.clone(), FakeNode { name: "Workspace".into(), is_space: false, children: space_ids.clone() });
        let mut server = Self::start_with_state(FakeState {
            nodes,
            page_data: HashMap::new(),
            collab_ok: HashMap::new(),
            fail_next_page_view: None,
            workspace_id,
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            otp_email: None,
            otp_code: "123456".into(),
            refresh_count: 0,
            hide_created_children: false,
            force_missing_collab: false,
        });
        server.space_id = space_ids.first().cloned().unwrap_or_default();
        server
    }
    /// A server whose workspace has no Space at all -- exercises the "no
    /// space found" failure path.
    pub fn start_with_no_space() -> Self {
        Self::start_with_spaces(&[])
    }
    /// A server that already has a page named `name` (e.g. "My daily
    /// meetings") directly under the default "General" Space, simulating one
    /// the user created themselves.
    pub fn start_with_existing_top_level_page(name: &str) -> Self {
        let server = Self::start();
        server.seed_existing_page(&server.space_id, "existing-daily-meetings", name);
        server
    }
    fn start_with_state(state: FakeState) -> Self {
        let workspace_id = state.workspace_id.clone();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(false).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let (requests_bg, state_bg, stop_bg) = (requests.clone(), state.clone(), stop.clone());
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_bg.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                handle_connection(&mut stream, &requests_bg, &state_bg);
            }
        });
        Self { base_url: format!("http://{addr}"), workspace_id, space_id: String::new(), requests, state, stop, handle: Some(handle) }
    }
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
    pub fn requests_matching(&self, method: &str, path_contains: &str) -> usize {
        self.requests().iter().filter(|r| r.method == method && r.path.contains(path_contains)).count()
    }
    pub fn otp_code(&self) -> String {
        self.state.lock().unwrap().otp_code.clone()
    }
    pub fn otp_email_requested(&self) -> Option<String> {
        self.state.lock().unwrap().otp_email.clone()
    }
    /// Forces the currently valid access token to something the client does
    /// not have, so its next authenticated call gets a 401 and must refresh.
    pub fn invalidate_current_access_token(&self) {
        let mut state = self.state.lock().unwrap();
        state.access_token = format!("rotated-{}", uuid::Uuid::new_v4());
    }
    pub fn refresh_count(&self) -> u32 {
        self.state.lock().unwrap().refresh_count
    }
    /// The `page_data` body a `create_page` call sent for `view_id`, if any
    /// -- lets a test schema-check exactly what was sent over the wire.
    pub fn page_data_for(&self, view_id: &str) -> Option<serde_json::Value> {
        self.state.lock().unwrap().page_data.get(view_id).cloned()
    }
    /// Every top-level child name under the workspace root, for asserting
    /// "no duplicate root/date page was created".
    pub fn child_names(&self, parent_id: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.nodes.get(parent_id).map(|n| n.children.iter().filter_map(|c| s.nodes.get(c)).map(|n| n.name.clone()).collect()).unwrap_or_default()
    }
    /// Simulates a page having been created on the server in a previous run
    /// whose local journal update never landed (a crash between the two).
    /// Makes the next `create_page` call fail with `status`/`message` (and
    /// only that one call -- resets itself), to exercise transient (5xx) vs
    /// permanent (4xx) failure handling in `appflowy.rs`.
    pub fn fail_next_page_view(&self, status: u16, message: &str) {
        self.state.lock().unwrap().fail_next_page_view = Some((status, message.to_string()));
    }
    /// Seeds an existing page with a working document collab (the normal
    /// case: a page a user created themselves, or one this integration
    /// created correctly).
    pub fn seed_existing_page(&self, parent_id: &str, id: &str, name: &str) {
        self.seed_existing_page_with_collab(parent_id, id, name, true);
    }
    /// Seeds an existing page whose document collab is missing -- `GET
    /// .../page-view/{id}` 404s "Collab not found" even though the page is
    /// listed under its parent, exactly like the three broken pages left
    /// behind on the user's real account by the pre-fix bug. Used to prove
    /// name-match adoption skips a page whose content doesn't actually load.
    pub fn seed_broken_existing_page(&self, parent_id: &str, id: &str, name: &str) {
        self.seed_existing_page_with_collab(parent_id, id, name, false);
    }
    fn seed_existing_page_with_collab(&self, parent_id: &str, id: &str, name: &str, collab_ok: bool) {
        let mut state = self.state.lock().unwrap();
        state.nodes.insert(id.to_string(), FakeNode { name: name.to_string(), is_space: false, children: vec![] });
        state.collab_ok.insert(id.to_string(), collab_ok);
        if let Some(parent) = state.nodes.get_mut(parent_id) {
            parent.children.push(id.to_string());
        }
    }
    /// After this call, every subsequent `create_page` succeeds (the page is
    /// stored) but is not linked into its parent's children -- see
    /// `FakeState::hide_created_children`.
    pub fn omit_created_children(&self) {
        self.state.lock().unwrap().hide_created_children = true;
    }
    /// After this call, every subsequently created page has no working
    /// document collab -- see `FakeState::force_missing_collab`.
    pub fn force_missing_collab(&self) {
        self.state.lock().unwrap().force_missing_collab = true;
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
        let Some(header_end) = find_header_end(&buffer) else { continue };
        let content_length = content_length_of(&buffer[..header_end]);
        if buffer.len() >= header_end + 4 + content_length {
            break;
        }
    }
    buffer
}
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}
fn content_length_of(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
fn respond(stream: &mut TcpStream, status: &str, body: &serde_json::Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let mut parts = line.splitn(2, ':');
        let key = parts.next()?.trim();
        if key.eq_ignore_ascii_case(name) {
            Some(parts.next()?.trim())
        } else {
            None
        }
    })
}

fn folder_json(nodes: &HashMap<String, FakeNode>, id: &str) -> serde_json::Value {
    let node = nodes.get(id);
    let (name, is_space, children) = match node {
        Some(n) => (n.name.clone(), n.is_space, n.children.clone()),
        None => (String::new(), false, vec![]),
    };
    serde_json::json!({
        "view_id": id,
        "parent_view_id": serde_json::Value::Null,
        "name": name,
        "is_space": is_space,
        "children": children.iter().map(|c| folder_json(nodes, c)).collect::<Vec<_>>(),
    })
}

fn handle_connection(stream: &mut TcpStream, requests: &Arc<Mutex<Vec<RecordedRequest>>>, state: &Arc<Mutex<FakeState>>) {
    let raw = read_full_request(stream);
    let text = String::from_utf8_lossy(&raw);
    let Some(request_line) = text.lines().next() else { return };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let full_path = parts.next().unwrap_or("").to_string();
    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (full_path.clone(), String::new()),
    };
    requests.lock().unwrap().push(RecordedRequest { method: method.clone(), path: path.clone() });
    let header_end = find_header_end(&raw).unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body_start = header_end.saturating_add(4).min(raw.len());
    let body_bytes = &raw[body_start..];
    let body: serde_json::Value = serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
    let auth = header_value(&headers, "Authorization").unwrap_or("").to_string();

    let mut s = state.lock().unwrap();
    match (method.as_str(), path.as_str()) {
        ("POST", "/gotrue/otp") => {
            s.otp_email = body.get("email").and_then(|v| v.as_str()).map(str::to_string);
            respond(stream, "200 OK", &serde_json::json!({}));
        }
        ("POST", "/gotrue/verify") => {
            let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("");
            if token == s.otp_code {
                respond(stream, "200 OK", &serde_json::json!({"access_token": s.access_token, "refresh_token": s.refresh_token}));
            } else {
                respond(stream, "400 Bad Request", &serde_json::json!({"message": "invalid code"}));
            }
        }
        ("POST", "/gotrue/token") => {
            s.refresh_count += 1;
            s.access_token = format!("access-{}", s.refresh_count + 1);
            s.refresh_token = format!("refresh-{}", s.refresh_count + 1);
            respond(stream, "200 OK", &serde_json::json!({"access_token": s.access_token, "refresh_token": s.refresh_token}));
        }
        ("GET", "/api/workspace") => {
            if !check_auth(&auth, &s.access_token, stream) {
                return;
            }
            respond(stream, "200 OK", &serde_json::json!({"data": [{"workspace_id": s.workspace_id, "workspace_name": "Test Workspace"}]}));
        }
        (method, p) if method == "GET" && p.ends_with("/folder") => {
            if !check_auth(&auth, &s.access_token, stream) {
                return;
            }
            let query_map = parse_query(&query);
            let root = query_map.get("root_view_id").cloned().unwrap_or_else(|| s.workspace_id.clone());
            respond(stream, "200 OK", &serde_json::json!({"data": folder_json(&s.nodes, &root)}));
        }
        ("POST", p) if p.ends_with("/page-view") => {
            if !check_auth(&auth, &s.access_token, stream) {
                return;
            }
            if let Some((status, message)) = s.fail_next_page_view.take() {
                respond(stream, &format!("{status} Injected Failure"), &serde_json::json!({"message": message}));
                return;
            }
            let parent_id = body.get("parent_view_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            // Real `create_document_page` semantics (AppFlowy-Cloud
            // `src/biz/workspace/page_view.rs`): the document collab is
            // stored under `collab_id.unwrap_or(Uuid::new_v4())`, and the
            // page under `view_id.unwrap_or(collab_id)`. A request that
            // omits `collab_id` (the pre-fix bug) gets a collab stored under
            // a random id that never equals `view_id`, so its content never
            // loads by `view_id` afterward.
            let collab_id = body.get("collab_id").and_then(|v| v.as_str()).map(str::to_string);
            let view_id = body
                .get("view_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| collab_id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let effective_collab_id = collab_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !s.nodes.contains_key(&parent_id) {
                respond(stream, "404 Not Found", &serde_json::json!({"message": "parent not found"}));
                return;
            }
            let hide = s.hide_created_children;
            let collab_ok = !s.force_missing_collab && effective_collab_id == view_id;
            s.nodes.insert(view_id.clone(), FakeNode { name, is_space: false, children: vec![] });
            s.collab_ok.insert(view_id.clone(), collab_ok);
            if !hide {
                if let Some(parent) = s.nodes.get_mut(&parent_id) {
                    parent.children.push(view_id.clone());
                }
            }
            if let Some(page_data) = body.get("page_data") {
                s.page_data.insert(view_id.clone(), page_data.clone());
            }
            respond(stream, "200 OK", &serde_json::json!({"data": {"view_id": view_id}}));
        }
        (method, p) if method == "GET" && p.contains("/page-view/") => {
            if !check_auth(&auth, &s.access_token, stream) {
                return;
            }
            let view_id = p.rsplit('/').next().unwrap_or("");
            if !s.nodes.contains_key(view_id) {
                respond(stream, "404 Not Found", &serde_json::json!({"message": "Page not found"}));
                return;
            }
            if s.collab_ok.get(view_id).copied().unwrap_or(false) {
                respond(stream, "200 OK", &serde_json::json!({"data": {"view_id": view_id}}));
            } else {
                respond(
                    stream,
                    "404 Not Found",
                    &serde_json::json!({"message": format!("Record not found: Collab not found for object_id: {view_id}")}),
                );
            }
        }
        _ => {
            respond(stream, "404 Not Found", &serde_json::json!({"message": format!("unhandled fake route {method} {path}")}));
        }
    }
}
fn check_auth(auth: &str, expected_token: &str, stream: &mut TcpStream) -> bool {
    if auth == format!("Bearer {expected_token}") {
        true
    } else {
        respond(stream, "401 Unauthorized", &serde_json::json!({"message": "unauthorized"}));
        false
    }
}
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
