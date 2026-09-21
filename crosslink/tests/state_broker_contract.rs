//! Broker contract integration tests against a local in-process HTTP stub.
//!
//! The stub speaks the broker v1 envelope contract (routes, typed errors, CAS,
//! read-back) over a real loopback socket, so the production
//! [`crosslink::state_broker::StateBrokerClient`] is exercised end to end —
//! request construction, auth header, envelope parsing, typed failures,
//! hydration, and reconciliation — without touching the deployed broker.
//!
//! Nothing here depends on the network beyond `127.0.0.1`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosslink::state_broker::{
    CommitRequest, ProjectStateTransport, StateBlob, StateBrokerClient, StateBrokerConfig,
};

const UUID: &str = "1d440dcf-bcbf-4d1a-987c-d5334568a716";
const TOKEN: &str = "stub-broker-token-abcdefghijklmnop";
const BASELINE: &str = "94fa0e38ae68c95e13d91226e74e6d4f6f1524dd";

// ── Stub broker ──────────────────────────────────────────────────────

struct StubState {
    project_uuid: String,
    token: String,
    head: Option<String>,
    message: Option<String>,
    files: BTreeMap<String, Vec<u8>>,
    /// Commit -> file snapshot, so historical commits stay readable the way the
    /// real broker's tree reads do.
    history: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    counter: u64,
    requests: Vec<String>,
    /// Force the next state/commit response to be a non-envelope body.
    broken_next_response: bool,
    /// Force the next error envelope to carry an unknown code.
    unknown_error_code: bool,
    /// Echo the bearer token in the next error envelope (redaction test).
    echo_token_next: bool,
    /// Return a non-retryable read-back mismatch for the next request.
    readback_mismatch_next: bool,
    /// Report this UUID (instead of the configured project) in whoami/state.
    project_uuid_override: Option<String>,
    /// Return `verified: false` on the next commit response even though the
    /// commit lands (broker read-back-disagreement shape).
    unverified_next: bool,
}

struct StubBroker {
    addr: SocketAddr,
    state: Arc<Mutex<StubState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl StubBroker {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub broker");
        let addr = listener.local_addr().expect("stub addr");
        let state = Arc::new(Mutex::new(StubState {
            project_uuid: UUID.to_string(),
            token: TOKEN.to_string(),
            head: None,
            message: None,
            files: BTreeMap::new(),
            history: BTreeMap::new(),
            counter: 0,
            requests: Vec::new(),
            broken_next_response: false,
            unknown_error_code: false,
            echo_token_next: false,
            readback_mismatch_next: false,
            project_uuid_override: None,
            unverified_next: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(mut stream) => handle_connection(&mut stream, &thread_state),
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn config(&self) -> StateBrokerConfig {
        StateBrokerConfig::new(self.base_url(), UUID, TOKEN, Duration::from_secs(5))
            .expect("stub config")
    }

    fn client(&self) -> StateBrokerClient {
        StateBrokerClient::new(self.config()).expect("stub client")
    }

    fn request_lines(&self) -> Vec<String> {
        self.lock().requests.clone()
    }

    fn seed<I, P, B>(&self, files: I)
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: AsRef<[u8]>,
    {
        let mut state = self.lock();
        for (path, bytes) in files {
            state.files.insert(path.into(), bytes.as_ref().to_vec());
        }
        state.counter += 1;
        let head = stub_commit_sha(&state, &[]);
        state.head = Some(head.clone());
        state.message = Some("seed\n\nProject-UUID: stub\nBroker: stub\n".to_string());
        let snapshot = state.files.clone();
        state.history.insert(head, snapshot);
    }

    fn set_broken_next_response(&self) {
        self.lock().broken_next_response = true;
    }

    fn set_unknown_error_code(&self) {
        self.lock().unknown_error_code = true;
    }

    /// Make the next authenticated response echo the bearer token in the error
    /// message and details (a misbehaving broker; the client must redact).
    fn set_echo_token_next(&self) {
        self.lock().echo_token_next = true;
    }

    /// Make the next response a non-retryable read-back mismatch (502).
    fn set_readback_mismatch_next(&self) {
        self.lock().readback_mismatch_next = true;
    }

    /// Report a different project UUID for the next whoami/state responses
    /// (backend/project identity binding test).
    fn set_project_uuid_override(&self, uuid: Option<&str>) {
        self.lock().project_uuid_override = uuid.map(str::to_string);
    }

    /// Land the next commit but report `verified: false` (the broker's
    /// read-back-disagreement shape).
    fn set_unverified_next(&self) {
        self.lock().unverified_next = true;
    }

    fn head(&self) -> Option<String> {
        self.lock().head.clone()
    }

    fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StubState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Drop for StubBroker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread can observe the stop flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(stream: &mut TcpStream, state: &Arc<Mutex<StubState>>) {
    let request = match read_request(stream) {
        Some(request) => request,
        None => return,
    };
    let (status, body) = route(&request, state);
    let payload = serde_json::to_vec(&body).unwrap_or_default();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

struct StubRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<StubRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut tmp).ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..read]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 128 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut tmp).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..read]);
    }

    let mut query = HashMap::new();
    for pair in raw_query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            query.insert(percent_decode(key), percent_decode(value));
        }
    }
    Some(StubRequest {
        method,
        path: raw_path,
        query,
        headers,
        body,
    })
}

fn route(request: &StubRequest, state: &Arc<Mutex<StubState>>) -> (u16, serde_json::Value) {
    let mut guard = match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.requests.push(format!(
        "{} {}?{:?}",
        request.method, request.path, request.query
    ));

    if guard.broken_next_response {
        guard.broken_next_response = false;
        return (200, serde_json::json!({ "unexpected": "not an envelope" }));
    }

    if request.path == "/v1/health" {
        return ok(
            "health",
            serde_json::json!({
                "status": "ok",
                "service": "crosslink-state-broker",
                "version": "0.1.0-test",
                "time": "2026-09-21T00:00:00.000Z",
            }),
        );
    }

    let expected_auth = format!("Bearer {}", guard.token);
    if request.headers.get("authorization") != Some(&expected_auth) {
        return (
            401,
            error_envelope(
                "unauthorized",
                "missing or invalid broker credential",
                false,
            ),
        );
    }

    if guard.readback_mismatch_next {
        guard.readback_mismatch_next = false;
        return (
            502,
            serde_json::json!({
                "ok": false,
                "operation": "state.commit",
                "request_id": "stub",
                "error": {
                    "code": "upstream_error",
                    "message": "state commit landed but read-back verification failed; reconcile before retrying the write",
                    "retryable": false,
                    "details": {
                        "commit": "a".repeat(40),
                        "failed_paths": ["a.json"],
                    },
                },
            }),
        );
    }

    if guard.echo_token_next {
        guard.echo_token_next = false;
        return (
            502,
            serde_json::json!({
                "ok": false,
                "operation": "test",
                "request_id": "stub",
                "error": {
                    "code": "upstream_error",
                    "message": format!("backend echo Bearer {}", guard.token),
                    "retryable": true,
                    "details": {"echo": guard.token},
                },
            }),
        );
    }

    let reported_uuid = guard
        .project_uuid_override
        .clone()
        .unwrap_or_else(|| guard.project_uuid.clone());

    if request.path == "/v1/whoami" {
        return ok(
            "whoami",
            serde_json::json!({
                "token_id": "token-1",
                "project_uuid": reported_uuid,
                "scopes": ["state:read", "state:write"],
            }),
        );
    }

    let prefix = format!("/v1/projects/{}/state", guard.project_uuid);
    if !request.path.starts_with(&prefix) {
        return (404, error_envelope("not_found", "unknown operation", false));
    }
    let tail = &request.path[prefix.len()..];

    if guard.unknown_error_code {
        guard.unknown_error_code = false;
        return (
            500,
            serde_json::json!({
                "ok": false,
                "operation": "test",
                "request_id": "stub",
                "error": {"code": "surprise_code", "message": "unknown code", "retryable": false},
            }),
        );
    }

    match tail {
        "" => {
            if request.method != "GET" {
                return (405, error_envelope("method_not_allowed", "GET only", false));
            }
            ok("state.read", state_result(&guard, &reported_uuid))
        }
        "/blob" => {
            let Some(path) = request.query.get("path") else {
                return (400, error_envelope("invalid_input", "path required", false));
            };
            if !stub_path_ok(path) {
                return (400, error_envelope("invalid_input", "invalid path", false));
            }
            let head = guard.head.clone().unwrap_or_default();
            let requested = request.query.get("ref").cloned();
            let (commit_at, snapshot) = match requested.as_deref() {
                None => (head.clone(), guard.history.get(&head)),
                Some(reference)
                    if reference == state_ref(&guard.project_uuid)
                        || reference == state_branch(&guard.project_uuid)
                        || reference == "state" =>
                {
                    (head.clone(), guard.history.get(&head))
                }
                Some(commit) => (commit.to_string(), guard.history.get(commit)),
            };
            let Some(snapshot) = snapshot else {
                return (404, error_envelope("not_found", "no such ref", false));
            };
            let Some(bytes) = snapshot.get(path) else {
                return (
                    404,
                    error_envelope("not_found", "state file not found", false),
                );
            };
            ok(
                "state.hydrate",
                serde_json::json!({
                    "path": path,
                    "ref": requested.unwrap_or_else(|| state_ref(&guard.project_uuid)),
                    // The broker reports the commit the blob was read at, not
                    // necessarily the head (historical reads must be faithful).
                    "commit": commit_at,
                    "blob_sha": pseudo_sha(bytes),
                    "sha256": sha256_hex(bytes),
                    "size": bytes.len(),
                    "content_base64": base64_encode(bytes),
                }),
            )
        }
        "/verify" => {
            let Some(commit) = request.query.get("commit") else {
                return (
                    400,
                    error_envelope("invalid_input", "commit required", false),
                );
            };
            if !stub_sha_ok(commit) {
                return (
                    400,
                    error_envelope("invalid_input", "commit must be a sha", false),
                );
            }
            let Some(snapshot) = guard.history.get(commit) else {
                return (404, error_envelope("not_found", "unknown commit", false));
            };
            let paths = request
                .query
                .get("paths")
                .map(|value| value.split(',').map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_default();
            if paths.is_empty() || paths.len() > 32 {
                return (
                    400,
                    error_envelope("invalid_input", "paths must be 1..=32", false),
                );
            }
            let mut seen = std::collections::HashSet::new();
            for path in &paths {
                if !stub_path_ok(path) || !seen.insert(path.clone()) {
                    return (
                        400,
                        error_envelope("invalid_input", "invalid or duplicate path", false),
                    );
                }
            }
            let entries: Vec<serde_json::Value> = paths
                .iter()
                .map(|path| match snapshot.get(path) {
                    Some(bytes) => serde_json::json!({
                        "path": path,
                        "present": true,
                        "blob_sha": pseudo_sha(bytes),
                        "sha256": sha256_hex(bytes),
                        "size": bytes.len(),
                    }),
                    None => serde_json::json!({
                        "path": path,
                        "present": false,
                        "blob_sha": null,
                        "sha256": null,
                        "size": null,
                    }),
                })
                .collect();
            ok(
                "state.verify",
                serde_json::json!({
                    "commit": commit,
                    "entries": entries,
                }),
            )
        }
        "/commit" => {
            if request.method != "POST" {
                return (
                    405,
                    error_envelope("method_not_allowed", "POST only", false),
                );
            }
            let body: serde_json::Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(_) => return (400, error_envelope("invalid_input", "bad JSON body", false)),
            };
            if let Err(reason) = stub_validate_commit_body(&body) {
                return (400, error_envelope("invalid_input", &reason, false));
            }
            let expected_head = body
                .get("expected_head")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let observed = guard
                .head
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String);
            let expected_matches = match (&expected_head, &observed) {
                (serde_json::Value::Null, serde_json::Value::Null) => true,
                (serde_json::Value::String(expected), serde_json::Value::String(observed)) => {
                    expected == observed
                }
                _ => false,
            };
            if !expected_matches {
                return (
                    409,
                    serde_json::json!({
                        "ok": false,
                        "operation": "state.commit",
                        "request_id": "stub",
                        "error": {
                            "code": "stale_state",
                            "message": "expected state head does not match the observed state head; no write was performed",
                            "retryable": true,
                            "details": {
                                "ref": state_ref(&guard.project_uuid),
                                "expected_head": expected_head,
                                "observed_head": observed,
                            },
                        },
                    }),
                );
            }
            let message = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let op_id = body.get("op_id").and_then(|v| v.as_str());
            let files = body
                .get("files")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if files.is_empty() {
                return (
                    400,
                    error_envelope("invalid_input", "files required", false),
                );
            }

            let previous_head = guard.head.clone();
            let verified_flag = if guard.unverified_next {
                guard.unverified_next = false;
                false
            } else {
                true
            };
            let mut verified = Vec::new();
            for file in &files {
                let path = file
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let content = file
                    .get("content_base64")
                    .and_then(|v| v.as_str())
                    .map(|value| base64_decode(value).unwrap_or_default())
                    .unwrap_or_default();
                guard.files.insert(path.clone(), content.clone());
                verified.push(serde_json::json!({
                    "path": path,
                    "blob_sha": pseudo_sha(&content),
                    "sha256": sha256_hex(&content),
                    "size": content.len(),
                    "verified": verified_flag,
                }));
            }
            guard.counter += 1;
            let commit = stub_commit_sha(&guard, &files);
            let mut trailers = vec![
                format!("Project-UUID: {}", guard.project_uuid),
                "Broker: crosslink-state-broker/0.1.0-test".to_string(),
            ];
            if let Some(op_id) = op_id {
                trailers.push(format!("Broker-Op: {op_id}"));
            }
            let full_message = format!("{}\n\n{}\n", message.trim_end(), trailers.join("\n"));
            guard.head = Some(commit.clone());
            guard.message = Some(full_message.clone());
            let snapshot = guard.files.clone();
            guard.history.insert(commit.clone(), snapshot);
            ok(
                "state.commit",
                serde_json::json!({
                    "ref": state_ref(&guard.project_uuid),
                    "commit": commit,
                    "previous_head": previous_head,
                    "head_after": commit,
                    "message": full_message,
                    "op_id": op_id,
                    "files": verified,
                    "verified": verified_flag,
                }),
            )
        }
        _ => (404, error_envelope("not_found", "unknown operation", false)),
    }
}

fn ok(operation: &str, result: serde_json::Value) -> (u16, serde_json::Value) {
    (
        200,
        serde_json::json!({
            "ok": true,
            "operation": operation,
            "request_id": "stub",
            "result": result,
            "provenance": {"backend_repository": "stub/crosslink-state"},
        }),
    )
}

fn error_envelope(code: &str, message: &str, retryable: bool) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "operation": "test",
        "request_id": "stub",
        "error": {"code": code, "message": message, "retryable": retryable},
    })
}

fn state_result(state: &StubState, reported_uuid: &str) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = state
        .files
        .iter()
        .map(|(path, bytes)| {
            serde_json::json!({
                "path": path,
                "blob_sha": pseudo_sha(bytes),
                "size": bytes.len(),
            })
        })
        .collect();
    let head = state.head.as_ref().map(|commit| {
        serde_json::json!({
            "commit": commit,
            "message": state.message.clone().unwrap_or_default(),
            "committed_at": "2026-09-21T00:00:00.000Z",
        })
    });
    serde_json::json!({
        "project": {
            "uuid": reported_uuid,
            "slug": "codex-build",
            "source_repository": "https://example.invalid/codex-build",
        },
        "backend_repository": "rock-solid-sites/crosslink-state",
        "state": {
            "ref": state_ref(reported_uuid),
            "exists": state.head.is_some(),
            "head": head,
            "entries": entries,
        },
        "baseline": {
            "ref": "refs/heads/main",
            "expected_commit": BASELINE,
            "observed_commit": BASELINE,
            "matches": true,
            "error": null,
        },
        "registry": {
            "source_ref": "refs/heads/main",
            "commit": null,
            "present": true,
            "entry": {"id": state.project_uuid},
            "error": null,
        },
    })
}

// ── Helpers ──────────────────────────────────────────────────────────

fn state_ref(uuid: &str) -> String {
    format!("refs/heads/projects/{uuid}/state")
}

fn state_branch(uuid: &str) -> String {
    format!("projects/{uuid}/state")
}

fn stub_commit_sha(state: &StubState, files: &[serde_json::Value]) -> String {
    let mut seed = format!("{}:{}", state.project_uuid, state.counter);
    for path in state.files.keys() {
        seed.push(':');
        seed.push_str(path);
    }
    for file in files {
        seed.push('#');
        seed.push_str(&file.to_string());
    }
    pseudo_sha(seed.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    crosslink::state_broker::digest::sha256_hex(bytes)
}

fn pseudo_sha(bytes: &[u8]) -> String {
    crosslink::state_broker::digest::pseudo_git_sha(bytes)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

// ── Independent contract rules (reimplemented here so the stub can reject what
// the real broker rejects, independent of the client's own validators) ──────

fn stub_sha_ok(value: &str) -> bool {
    value.len() == 40
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn stub_path_ok(path: &str) -> bool {
    if path.is_empty() || path.len() > 256 || path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    if path.contains('\\') || path.contains('\0') {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > 16 {
        return false;
    }
    segments.iter().all(|segment| {
        if segment.is_empty() || segment.len() > 64 || *segment == "." || *segment == ".." {
            return false;
        }
        let mut chars = segment.chars();
        let first = chars.next().unwrap_or('!');
        if !(first.is_ascii_alphanumeric() || first == '.' || first == '_') {
            return false;
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    })
}

fn stub_message_ok(message: &str) -> bool {
    if message.trim().is_empty() || message.chars().count() > 512 {
        return false;
    }
    if message
        .chars()
        .any(|c| c == '\u{7f}' || ('\u{0}'..='\u{1f}').contains(&c))
    {
        return false;
    }
    let trimmed = message.trim();
    // Byte-prefix comparison, never a `&str` byte slice: a multi-byte prefix
    // must compare unequal, not panic (matches the broker's TS regex).
    !["Project-UUID:", "Broker:", "Broker-Op:"]
        .iter()
        .any(|trailer| {
            trimmed.len() >= trailer.len()
                && trimmed.as_bytes()[..trailer.len()].eq_ignore_ascii_case(trailer.as_bytes())
        })
}

fn stub_op_id_ok(op_id: &str) -> bool {
    !op_id.is_empty()
        && op_id.len() <= 128
        && op_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Broker-equivalent commit-body validation (`state.ts::validateCommitInput`).
fn stub_validate_commit_body(body: &serde_json::Value) -> Result<(), String> {
    let Some(object) = body.as_object() else {
        return Err("request body must be a JSON object".to_string());
    };
    if !object.contains_key("expected_head") {
        return Err("expected_head is required (use null to expect no state ref)".to_string());
    }
    match object.get("expected_head") {
        Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(sha)) if stub_sha_ok(sha) => {}
        _ => return Err("expected_head must be null or a commit sha".to_string()),
    }
    let Some(message) = object.get("message").and_then(|value| value.as_str()) else {
        return Err("message is required".to_string());
    };
    if !stub_message_ok(message) {
        return Err("invalid message".to_string());
    }
    if let Some(op_id) = object.get("op_id") {
        if !op_id.is_null() {
            match op_id.as_str() {
                Some(op_id) if stub_op_id_ok(op_id) => {}
                _ => return Err("invalid op_id".to_string()),
            }
        }
    }
    let Some(files) = object.get("files").and_then(|value| value.as_array()) else {
        return Err("files must be an array".to_string());
    };
    if files.is_empty() || files.len() > 32 {
        return Err("files must contain 1-32 entries".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut total: usize = 0;
    for file in files {
        let Some(path) = file.get("path").and_then(|value| value.as_str()) else {
            return Err("each file needs a path".to_string());
        };
        if !stub_path_ok(path) || !seen.insert(path.to_string()) {
            return Err("invalid or duplicate path".to_string());
        }
        let Some(encoded) = file.get("content_base64").and_then(|value| value.as_str()) else {
            return Err("content_base64 is required".to_string());
        };
        let decoded =
            base64_decode(encoded).ok_or_else(|| "content_base64 must be base64".to_string())?;
        if base64_encode(&decoded) != encoded {
            return Err("content_base64 must be strict base64".to_string());
        }
        if decoded.is_empty() || decoded.len() > 256 * 1024 {
            return Err("file size must be 1-262144 bytes".to_string());
        }
        total += decoded.len();
        if total > 1024 * 1024 {
            return Err("total commit size must not exceed 1048576 bytes".to_string());
        }
    }
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ── Tests ────────────────────────────────────────────────────────────

#[test]
fn state_and_whoami_round_trip_through_real_http() {
    let broker = StubBroker::start();
    broker.seed(vec![
        ("issues/abc.json", br#"{"title":"one"}"#.to_vec()),
        (
            "meta/counters.json",
            br#"{"next_display_id":2,"next_comment_id":1}"#.to_vec(),
        ),
    ]);
    let client = broker.client();

    let health = client.health().expect("health");
    assert_eq!(health.status, "ok");
    assert_eq!(health.service, "crosslink-state-broker");

    let who = client.whoami().expect("whoami");
    assert_eq!(who.token_id, "token-1");
    assert_eq!(who.project_uuid, UUID);
    assert_eq!(who.scopes, vec!["state:read", "state:write"]);

    let state = client.read_state().expect("state");
    assert_eq!(state.project.uuid, UUID);
    assert_eq!(state.backend_repository, "rock-solid-sites/crosslink-state");
    assert_eq!(state.state.state_ref, state_ref(UUID));
    assert!(state.state.exists);
    assert_eq!(state.state.entries.len(), 2);
    assert_eq!(
        state.state.entries[0].path, "issues/abc.json",
        "inventory is path-sorted"
    );
    assert!(state.baseline.matches);
    assert_eq!(state.baseline.observed_commit.as_deref(), Some(BASELINE));
    assert_eq!(client.current_head().unwrap(), broker.head());
}

#[test]
fn blob_read_verifies_digests_and_hydrates_a_disposable_projection() {
    let broker = StubBroker::start();
    broker.seed(vec![
        ("issues/abc.json", br#"{"title":"one"}"#.to_vec()),
        (
            "meta/counters.json",
            br#"{"next_display_id":2,"next_comment_id":1}"#.to_vec(),
        ),
    ]);
    let client = broker.client();

    let blob: StateBlob = client
        .read_blob("issues/abc.json", None)
        .expect("blob read");
    assert_eq!(blob.bytes().unwrap(), br#"{"title":"one"}"#);
    assert_eq!(blob.sha256, sha256_hex(br#"{"title":"one"}"#));
    assert_eq!(
        String::from_utf8(blob.bytes().unwrap()).unwrap(),
        r#"{"title":"one"}"#
    );

    let entries = client
        .verify(
            &broker.head().unwrap(),
            &["issues/abc.json".to_string(), "missing.json".to_string()],
        )
        .expect("verify")
        .entries;
    assert!(entries[0].is_verified());
    assert!(!entries[1].present);

    let dir = tempfile::tempdir().unwrap();
    let projection = dir.path().join("state-projection");
    let report = client.hydrate_into(&projection, None).expect("hydrate");
    assert_eq!(report.files.len(), 2);
    assert_eq!(
        std::fs::read(projection.join("issues/abc.json")).unwrap(),
        br#"{"title":"one"}"#
    );
    // The projection is identity-bound and fresh at the durable head.
    let marker = client
        .verify_projection(&projection)
        .expect("fresh projection");
    assert_eq!(marker.project_uuid, UUID);
    assert_eq!(marker.head_commit, broker.head().unwrap());

    // It is disposable: deleting it loses nothing; the durable head is still
    // the broker's.
    std::fs::remove_dir_all(&projection).unwrap();
    assert_eq!(client.current_head().unwrap(), broker.head());
}

/// A projection that no longer matches the durable head must be refused by the
/// freshness gate (stale projections cannot masquerade as current).
#[test]
fn stale_projection_is_refused_over_http() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"one".to_vec())]);
    let client = broker.client();
    let dir = tempfile::tempdir().unwrap();
    let projection = dir.path().join("state-projection");
    client.hydrate_into(&projection, None).expect("hydrate");
    client.verify_projection(&projection).expect("fresh");

    // The durable head moves: the projection is stale.
    client
        .commit(&CommitRequest::single(
            "b.json",
            b"two".to_vec(),
            broker.head(),
            "advance",
            None,
        ))
        .expect("commit");
    let error = client.verify_projection(&projection).unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::LocalIo
    );
    assert!(error.message().contains("stale"), "{}", error.message());

    // Re-hydrating restores freshness.
    client.hydrate_into(&projection, None).expect("rehydrate");
    let marker = client.verify_projection(&projection).expect("fresh again");
    assert_eq!(marker.head_commit, broker.head().unwrap());
}

#[test]
fn verify_and_blob_read_support_historical_commits() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"one".to_vec())]);
    let client = broker.client();
    let first = broker.head().unwrap();

    let second = client
        .commit(&CommitRequest::single(
            "b.json",
            b"two".to_vec(),
            Some(first.clone()),
            "add b",
            None,
        ))
        .expect("second commit")
        .commit;
    assert_eq!(broker.head().as_deref(), Some(second.as_str()));

    // Historical verify: b.json did not exist at the first commit.
    let entries = client
        .verify(&first, &["a.json".to_string(), "b.json".to_string()])
        .expect("verify at historical commit")
        .entries;
    assert!(entries[0].is_verified());
    assert!(!entries[1].present);

    // Historical blob read.
    let blob = client
        .read_blob("a.json", Some(&first))
        .expect("blob at historical commit");
    assert_eq!(
        blob.commit, first,
        "the blob must report the commit it was read at"
    );
    assert_eq!(blob.bytes().unwrap(), b"one");
    let at_head = client.read_blob("b.json", None).expect("blob at head");
    assert_eq!(at_head.bytes().unwrap(), b"two");

    // Unknown commit stays a typed not_found.
    let missing = client
        .verify(&"c".repeat(40), &["a.json".to_string()])
        .unwrap_err();
    assert_eq!(
        missing.code(),
        crosslink::state_broker::BrokerErrorCode::NotFound
    );
}

#[test]
fn commit_bootstrap_conflict_and_reconciled_retry() {
    let broker = StubBroker::start();
    let client = broker.client();

    // Bootstrap commit: expected_head = null.
    let first = client
        .commit(&CommitRequest::single(
            "checkpoints/first.json",
            br#"{"phase":"first"}"#.to_vec(),
            None,
            "checkpoint: first",
            Some("op-first".to_string()),
        ))
        .expect("bootstrap commit");
    assert!(first.verified);
    assert_eq!(first.previous_head, None);
    assert_eq!(
        broker.file("checkpoints/first.json").unwrap(),
        br#"{"phase":"first"}"#
    );
    let first_head = first.commit.clone();

    // Stale CAS: caller still believes the ref does not exist.
    let error = client
        .commit(&CommitRequest::single(
            "checkpoints/second.json",
            br#"{"phase":"second"}"#.to_vec(),
            None,
            "checkpoint: second",
            Some("op-second".to_string()),
        ))
        .unwrap_err();
    assert!(error.is_stale_state());
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::StaleState
    );
    assert_eq!(error.http_status(), Some(409));
    let observed = error
        .details()
        .and_then(|details| details.get("observed_head"))
        .and_then(|value| value.as_str());
    assert_eq!(observed, Some(first_head.as_str()));
    assert_eq!(broker.head().as_deref(), Some(first_head.as_str()));
    assert_eq!(
        broker.file("checkpoints/second.json"),
        None,
        "nothing written"
    );

    // Reconciled CAS: re-read the head and retry through commit_cas.
    let request = CommitRequest::single(
        "checkpoints/second.json",
        br#"{"phase":"second"}"#.to_vec(),
        Some(first_head.clone()),
        "checkpoint: second",
        Some("op-second".to_string()),
    );
    let resolution = client.commit_cas(&request, 1).expect("cas retry");
    assert!(matches!(
        resolution,
        crosslink::state_broker::CasResolution::Applied { .. }
    ));
    assert!(resolution.is_verified());
    assert_ne!(resolution.commit(), Some(first_head.as_str()));

    // Idempotent replay: pretend the landed write's response was lost and the
    // caller retries with the pre-write head. The broker reports stale_state;
    // commit_cas must recognize our own Broker-Op trailer and not write again.
    let before_replay = broker.head();
    let replay = client
        .commit_cas(
            &CommitRequest::single(
                "checkpoints/second.json",
                br#"{"phase":"second"}"#.to_vec(),
                Some(first_head),
                "checkpoint: second",
                Some("op-second".to_string()),
            ),
            1,
        )
        .expect("idempotent replay");
    assert!(matches!(
        replay,
        crosslink::state_broker::CasResolution::AlreadyApplied { .. }
    ));
    assert!(replay.is_verified());
    assert_eq!(
        broker.head(),
        before_replay,
        "replay must not move the head"
    );
}

#[test]
fn token_never_leaks_into_errors_logs_or_urls() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();

    // A misbehaving broker that echoes the token in its error envelope must
    // not leak it into client errors.
    let config = broker.config();
    broker.set_echo_token_next();
    let error = client.read_state().unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::UpstreamError
    );
    let rendered = format!("{error} {error:?}");
    assert!(
        !rendered.contains(TOKEN),
        "error must be redacted: {rendered}"
    );
    assert!(error.message().contains("[redacted]"));
    assert!(!error.details().unwrap().to_string().contains(TOKEN));

    // Client Debug output must not contain the token either.
    assert!(!format!("{client:?}").contains(TOKEN));
    assert!(!format!("{config:?}").contains(TOKEN));

    // Real requests carry the token only in the Authorization header, never in
    // the request target recorded by the stub.
    for line in broker.request_lines() {
        assert!(
            !line.contains(TOKEN),
            "token must not appear in request URLs: {line}"
        );
    }

    // A wrong token produces the typed unauthorized failure.
    let wrong = StateBrokerClient::new(
        StateBrokerConfig::new(
            broker.base_url(),
            UUID,
            "wrong-token-abcdefghijklmnop",
            Duration::from_secs(5),
        )
        .unwrap(),
    )
    .unwrap();
    let error = wrong.whoami().unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::Unauthorized
    );
    assert_eq!(error.http_status(), Some(401));
}

#[test]
fn readback_mismatch_on_a_write_is_reconcile_required() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();

    // On a *read*, an upstream error is still an upstream error (non-retryable
    // when the broker says so).
    broker.set_readback_mismatch_next();
    let error = client.read_state().unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::UpstreamError
    );
    assert!(
        !error.retryable(),
        "an explicit retryable=false must not be overridden by the code default"
    );

    // On a *write*, the same shape means the commit may have landed: it must be
    // an explicit reconcile-required outcome, never an ordinary error.
    broker.set_readback_mismatch_next();
    let error = client
        .commit(&CommitRequest::single(
            "b.json",
            b"{}".to_vec(),
            broker.head(),
            "ambiguous write",
            Some("op-ambiguous".to_string()),
        ))
        .unwrap_err();
    assert!(error.is_reconcile_required(), "{error:?}");
    assert_eq!(error.reconcile_reason(), Some("readback_mismatch"));
    assert!(!error.retryable(), "never blind-retry a write");
    assert_eq!(
        error
            .details()
            .and_then(|details| details.get("op_id"))
            .and_then(|value| value.as_str()),
        Some("op-ambiguous")
    );

    // Control: stale_state stays retryable, and a definite rejection is not
    // reclassified as ambiguity.
    let stale = client
        .commit(&CommitRequest::single(
            "a.json",
            b"{}".to_vec(),
            None,
            "control commit",
            None,
        ))
        .unwrap_err();
    assert!(stale.is_stale_state());
    assert!(stale.retryable());
}

/// A success envelope carrying `verified: false` is not an ordinary success:
/// the write may have landed, so it must be reconcile-required, and
/// `commit_cas` may resolve it to `AlreadyApplied` only by independently
/// verifying content.
#[test]
fn verified_false_is_never_an_ordinary_success() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();
    let base = broker.head().unwrap();

    broker.set_unverified_next();
    let error = client
        .commit(&CommitRequest::single(
            "b.json",
            b"two".to_vec(),
            Some(base.clone()),
            "write with unverified read-back",
            Some("op-unverified".to_string()),
        ))
        .unwrap_err();
    assert!(error.is_reconcile_required(), "{error:?}");
    assert_eq!(error.reconcile_reason(), Some("verified_false"));
    assert!(!error.retryable());
    // The stub still applied the commit (the broker's read-back disagreed, not
    // the ref update).
    let landed = broker.head().unwrap();
    assert_ne!(landed, base);

    // commit_cas reconciles by op id and content: the payload is there, so the
    // verdict is a verified AlreadyApplied, and no second write happens.
    let resolution = client
        .commit_cas(
            &CommitRequest::single(
                "b.json",
                b"two".to_vec(),
                Some(base),
                "write with unverified read-back",
                Some("op-unverified".to_string()),
            ),
            1,
        )
        .expect("reconciled verdict");
    assert!(
        matches!(
            resolution,
            crosslink::state_broker::CasResolution::AlreadyApplied { .. }
        ),
        "{resolution:?}"
    );
    assert!(resolution.is_verified());
    assert_eq!(broker.head(), Some(landed));
}

/// The same path changed by a competing writer must be refused over real HTTP,
/// not clobbered by the CAS rebase.
#[test]
fn same_path_rebase_is_refused_through_the_client() {
    let broker = StubBroker::start();
    broker.seed(vec![("shared/counter.json", b"{\"n\":1}".to_vec())]);
    let client = broker.client();
    let base = broker.head().unwrap();

    // A competing writer advances the same path.
    client
        .commit(&CommitRequest::single(
            "shared/counter.json",
            b"{\"n\":2}".to_vec(),
            Some(base.clone()),
            "competing update",
            Some("op-theirs".to_string()),
        ))
        .expect("competing commit");

    let resolution = client
        .commit_cas(
            &CommitRequest::single(
                "shared/counter.json",
                b"{\"n\":3}".to_vec(),
                Some(base),
                "our update",
                Some("op-ours".to_string()),
            ),
            1,
        )
        .expect("verdict");
    match &resolution {
        crosslink::state_broker::CasResolution::ReconcileRequired { reason, .. } => {
            assert!(
                matches!(
                    reason,
                    crosslink::state_broker::ReconcileReason::OverlappingPaths { .. }
                ),
                "{reason:?}"
            );
        }
        other => panic!("expected ReconcileRequired, got {other:?}"),
    }
    assert_eq!(
        broker.file("shared/counter.json").unwrap(),
        b"{\"n\":2}".to_vec(),
        "the competing writer's bytes must survive"
    );
}

/// Non-ASCII commit messages must not panic the client or the broker's
/// validator (the byte-slice panic class), and must round-trip.
#[test]
fn non_ascii_commit_messages_round_trip() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();

    for (message, path) in [
        ("éééé", "b.json"),
        ("€€€ broker note", "c.json"),
        ("日本語のメモ", "d.json"),
    ] {
        let outcome = client
            .commit(&CommitRequest::single(
                path,
                b"{}".to_vec(),
                broker.head(),
                message,
                None,
            ))
            .unwrap_or_else(|e| panic!("message {message:?} must be accepted: {e}"));
        assert!(outcome.verified);
    }
}

/// The broker's reported project identity is bound to the configuration: a
/// broker answering for another project is a hard error, not data.
#[test]
fn broker_project_identity_mismatch_is_a_hard_error() {
    const OTHER_UUID: &str = "2a551ed0-cdc0-4e2b-a98d-e6445679b827";
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();

    // Baseline: identity matches.
    assert_eq!(client.read_state().unwrap().project.uuid, UUID);

    broker.set_project_uuid_override(Some(OTHER_UUID));
    let error = client.read_state().unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");
    assert!(!error.retryable());
    let error = client.whoami().unwrap_err();
    assert!(error.is_identity_mismatch(), "{error:?}");

    broker.set_project_uuid_override(None);
    assert_eq!(client.read_state().unwrap().project.uuid, UUID);
}

#[test]
fn protocol_violations_are_typed_and_local_validation_skips_the_network() {
    let broker = StubBroker::start();
    broker.seed(vec![("a.json", b"{}".to_vec())]);
    let client = broker.client();
    let before = broker.request_lines().len();

    // Local rejection: no request must be sent.
    let error = client.read_blob("../escape", None).unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::InvalidInput
    );
    let error = client
        .commit(&CommitRequest::single(
            "a.json",
            b"{}".to_vec(),
            Some("not-a-sha".to_string()),
            "bad head",
            None,
        ))
        .unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::InvalidInput
    );
    assert_eq!(
        broker.request_lines().len(),
        before,
        "no network on local rejection"
    );

    // Non-envelope response → protocol error.
    broker.set_broken_next_response();
    let error = client.read_state().unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::Protocol
    );

    // Unknown error code → protocol error (never silently mapped).
    broker.set_unknown_error_code();
    let error = client.read_state().unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::Protocol
    );
    assert!(error.message().contains("surprise_code"));
}

#[test]
fn non_loopback_plain_http_is_refused_by_config() {
    let error = StateBrokerConfig::new(
        "http://broker.example.workers.dev",
        UUID,
        TOKEN,
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert_eq!(
        error.code(),
        crosslink::state_broker::BrokerErrorCode::Configuration
    );
}
