//! Real-socket tests for the desktop HTTP layer.
//!
//! Every test binds the actual `TcpListener` on an ephemeral port and drives it through a raw
//! `TcpStream`: the point of this module is the wire — status lines, headers, framing — not the
//! signatures of the helpers.
//!
//! Two limits are stated rather than hidden:
//!
//! - `POST /api/prompt` cannot spawn a real `goon` child from inside a test binary (it would re-run
//!   the test harness), so the spawn target is overridden through `CHILD_BIN_OVERRIDE` with a shell
//!   that exits immediately. That pins the SSE head, the framing and the terminal `exit` event; it
//!   does not pin the NDJSON relay, which is the same line loop the shell has always had and is
//!   covered by the `SseWriter` unit tests. A stub script cannot be used instead of a shell because
//!   `Command` on Windows does not execute `.cmd`/`.bat` directly.
//! - The event-log fixtures are written through `eventlog::EventLogWriter`, so the envelope shape is
//!   the real one, but the events are synthetic.

use super::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Server {
    port: u16,
    cwd: PathBuf,
    data_dir: PathBuf,
    _dir: tempfile::TempDir,
}

fn start() -> Server {
    start_with(|_, _| {})
}

fn start_with(prepare: impl FnOnce(&Path, &Path)) -> Server {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let cwd = dir.path().join("work");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    prepare(&cwd, &data_dir);
    let options = WebOptions { port: 0, open: false, cwd: cwd.clone(), data_dir: data_dir.clone(), passthrough: vec![] };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        run_with(options, |url| {
            let _ = sender.send(url.to_string());
        })
        .expect("serve");
    });
    let url = receiver.recv().expect("server url");
    let port = url.rsplit(':').next().expect("port").trim_end_matches('/').parse().expect("port number");
    Server { port, cwd, data_dir, _dir: dir }
}

/// Sends one complete request on its own connection and returns the raw response.
fn raw(server: &Server, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(15))).ok();
    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().ok();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).to_string()
}

fn get(server: &Server, path: &str) -> String {
    raw(server, &format!("GET {path} HTTP/1.1\r\nhost: localhost:{}\r\nconnection: close\r\n\r\n", server.port))
}

fn post(server: &Server, path: &str, body: &str, extra: &str) -> String {
    raw(server, &format!("POST {path} HTTP/1.1\r\nhost: localhost:{}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}", server.port, body.len()))
}

fn status(response: &str) -> u16 {
    response.lines().next().and_then(|line| line.split_whitespace().nth(1)).and_then(|code| code.parse().ok()).unwrap_or(0)
}

fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response.lines().take_while(|line| !line.trim().is_empty()).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn body(response: &str) -> &str {
    match response.find("\r\n\r\n") {
        Some(index) => &response[index + 4..],
        None => "",
    }
}

fn json_body(response: &str) -> Value {
    serde_json::from_str(body(response)).unwrap_or_else(|error| panic!("body is not JSON ({error}): {response}"))
}

/// Appends `count` events to `{data}/sessions/{id}.events.jsonl` through the real writer.
fn write_event_log(data_dir: &Path, id: &str, count: usize) {
    let path = crate::eventlog::event_log_path(data_dir, id);
    let mut writer = crate::eventlog::EventLogWriter::open(&path, Some(id), &[], Box::new(|_| {}));
    for index in 0..count {
        // `note` is not a streamed kind, so each append is exactly one envelope.
        writer.append(&json!({"type": "note", "text": format!("event {index}")}));
    }
    writer.close();
}

/// Reads exactly one response off a connection that stays open (used for keep-alive).
fn read_one(reader: &mut BufReader<TcpStream>) -> String {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read head") == 0 {
            break;
        }
        let blank = line == "\r\n" || line == "\n";
        head.push_str(&line);
        if blank {
            break;
        }
    }
    let mut length = 0usize;
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut payload).expect("read body");
    }
    head + &String::from_utf8_lossy(&payload)
}

// ---------------------------------------------------------------------------------------------
// Document and assets
// ---------------------------------------------------------------------------------------------

#[test]
fn document_carries_security_headers_and_csp() {
    let server = start();
    let page = get(&server, "/");
    assert_eq!(status(&page), 200, "{page}");
    assert_eq!(header(&page, "content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(header(&page, "x-content-type-options"), Some("nosniff"));
    assert_eq!(header(&page, "referrer-policy"), Some("no-referrer"));
    assert_eq!(header(&page, "content-security-policy"), Some(http::DOCUMENT_CSP));
    // The document policy must be the strict one, not a relaxed variant.
    assert!(header(&page, "content-security-policy").unwrap().contains("default-src 'none'"));
    assert!(header(&page, "content-security-policy").unwrap().contains("connect-src 'self'"));
    assert!(page.contains("id=\"intro\""), "intro markup is served");
    assert!(page.contains("id=\"transcript\""), "transcript markup is served");
    assert!(page.contains("/app.js"), "the UI script is referenced");
    let declared: usize = header(&page, "content-length").expect("content-length").parse().expect("length");
    assert_eq!(declared, body(&page).len(), "content-length must match the body");
}

#[test]
fn assets_are_served_with_etag_and_revalidate_to_304() {
    let server = start();
    let css = get(&server, "/app.css");
    assert_eq!(status(&css), 200, "{css}");
    assert_eq!(header(&css, "content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(header(&css, "cache-control"), Some("no-cache"));
    let etag = header(&css, "etag").expect("etag").to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "strong etag: {etag}");
    assert_eq!(header(&css, "content-length").expect("length").parse::<usize>().unwrap(), body(&css).len());

    let script = get(&server, "/app.js");
    assert_eq!(status(&script), 200, "{script}");
    assert_eq!(header(&script, "content-type"), Some("text/javascript; charset=utf-8"));
    assert!(header(&script, "etag").is_some());

    let revalidated = raw(&server, &format!("GET /app.css HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nif-none-match: {etag}\r\nconnection: close\r\n\r\n", server.port));
    assert_eq!(status(&revalidated), 304, "{revalidated}");
    assert_eq!(header(&revalidated, "etag"), Some(etag.as_str()));
    assert!(body(&revalidated).is_empty(), "a 304 has no body");
    assert!(header(&revalidated, "content-length").is_none(), "a 304 carries no length");

    // A weak validator is still a match, per RFC 9110.
    let weak = raw(&server, &format!("GET /app.css HTTP/1.1\r\nhost: localhost:{}\r\nif-none-match: W/{etag}\r\nconnection: close\r\n\r\n", server.port));
    assert_eq!(status(&weak), 304, "{weak}");

    // A stale validator gets the bytes again.
    let stale = raw(&server, &format!("GET /app.css HTTP/1.1\r\nhost: localhost:{}\r\nif-none-match: \"stale\"\r\nconnection: close\r\n\r\n", server.port));
    assert_eq!(status(&stale), 200, "{stale}");
    assert!(!body(&stale).is_empty());
}

#[test]
fn keep_alive_serves_two_requests_on_one_socket() {
    let server = start();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));

    stream.write_all(format!("GET /api/health HTTP/1.1\r\nhost: localhost:{}\r\n\r\n", server.port).as_bytes()).expect("write");
    let first = read_one(&mut reader);
    assert_eq!(status(&first), 200, "{first}");
    assert_eq!(header(&first, "connection"), Some("keep-alive"), "{first}");

    stream.write_all(format!("GET /api/config HTTP/1.1\r\nhost: localhost:{}\r\nconnection: close\r\n\r\n", server.port).as_bytes()).expect("write");
    let second = read_one(&mut reader);
    assert_eq!(status(&second), 200, "{second}");
    assert_eq!(header(&second, "connection"), Some("close"), "{second}");
}

// ---------------------------------------------------------------------------------------------
// API endpoints
// ---------------------------------------------------------------------------------------------

#[test]
fn health_reports_ok_version_uptime_and_runs() {
    let server = start();
    let response = get(&server, "/api/health");
    assert_eq!(status(&response), 200, "{response}");
    assert_eq!(header(&response, "cache-control"), Some("no-store"));
    let value = json_body(&response);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["version"], json!(crate::version::VERSION));
    assert!(value["uptimeMs"].is_u64(), "uptimeMs is a number: {value}");
    assert_eq!(value["runs"], json!(0));
}

#[test]
fn config_exposes_only_safe_fields() {
    let server = start();
    let response = get(&server, "/api/config");
    assert_eq!(status(&response), 200, "{response}");
    let value = json_body(&response);
    for key in ["provider", "model", "effort", "cwd", "dataDir", "eventLog", "readOnly"] {
        assert!(value.get(key).is_some(), "missing {key}: {value}");
    }
    assert_eq!(value["readOnly"], json!(false));
    assert_eq!(value["cwd"], json!(server.cwd.to_string_lossy()));
    assert_eq!(value["dataDir"], json!(server.data_dir.to_string_lossy()));
    assert!(value["eventLog"].is_boolean());
    // Nothing that could carry a credential may appear, in the keys or in the values.
    let text = body(&response).to_ascii_lowercase();
    for needle in ["base_url", "baseurl", "apikey", "api_key", "key_env", "authorization", "bearer", "sk-", "http://", "https://", "token"] {
        assert!(!text.contains(needle), "config leaked `{needle}`: {text}");
    }
}

#[test]
fn session_events_returns_the_tail_with_truncation() {
    let id = "0123456789abcdef0123456789abcdef";
    let server = start_with(|_, data_dir| write_event_log(data_dir, id, 5));

    let full = get(&server, &format!("/api/sessions/{id}/events"));
    assert_eq!(status(&full), 200, "{full}");
    let value = json_body(&full);
    assert_eq!(value["id"], json!(id));
    assert_eq!(value["truncated"], json!(false));
    let events = value["events"].as_array().expect("events array");
    assert_eq!(events.len(), 5);
    // An envelope is what `EventEnvelope::to_value` produces.
    for key in ["v", "seq", "at", "sessionId", "type", "event"] {
        assert!(events[0].get(key).is_some(), "envelope missing {key}: {}", events[0]);
    }
    assert_eq!(events[4]["seq"], json!(5));

    let tail = json_body(&get(&server, &format!("/api/sessions/{id}/events?limit=2")));
    let tail_events = tail["events"].as_array().expect("tail");
    assert_eq!(tail_events.len(), 2);
    assert_eq!(tail["truncated"], json!(true));
    assert_eq!(tail_events[0]["seq"], json!(4), "the tail is returned, newest last");
    assert_eq!(tail_events[1]["seq"], json!(5));

    // An absurd limit is clamped rather than believed.
    let clamped = json_body(&get(&server, &format!("/api/sessions/{id}/events?limit=999999")));
    assert_eq!(clamped["events"].as_array().unwrap().len(), 5);
    let zero = json_body(&get(&server, &format!("/api/sessions/{id}/events?limit=0")));
    assert_eq!(zero["events"].as_array().unwrap().len(), 1, "limit is clamped to at least 1");
}

#[test]
fn session_events_rejects_invalid_ids_and_missing_sessions() {
    let server = start();
    for path in [
        "/api/sessions/../../windows/win.ini/events",
        "/api/sessions/..%2f..%2fetc/events",
        "/api/sessions/..%5c..%5cboot.ini/events",
        "/api/sessions/has%20space/events",
        "/api/sessions/.hidden/events",
    ] {
        let response = get(&server, path);
        assert_eq!(status(&response), 400, "{path} must be refused: {response}");
        assert!(body(&response).contains("invalid session id"), "{response}");
    }
    // A well-formed id with no log behind it is a 404, not a 400.
    let missing = get(&server, "/api/sessions/ffffffffffffffffffffffffffffffff/events");
    assert_eq!(status(&missing), 404, "{missing}");
    assert!(body(&missing).contains("session not found"), "{missing}");
}

#[test]
fn runs_is_an_empty_array_before_any_prompt() {
    let server = start();
    let response = get(&server, "/api/runs");
    assert_eq!(status(&response), 200, "{response}");
    assert_eq!(json_body(&response), json!([]));
}

#[test]
fn sessions_listing_still_answers() {
    let server = start();
    let response = get(&server, "/api/sessions");
    assert_eq!(status(&response), 200, "{response}");
    assert!(json_body(&response).is_array());
    let meta = get(&server, "/api/meta");
    assert_eq!(status(&meta), 200, "{meta}");
    assert_eq!(json_body(&meta)["version"], json!(crate::version::VERSION));
}

#[test]
fn cancel_without_a_run_reports_honestly() {
    let server = start();
    let response = post(&server, "/api/cancel", "", "");
    assert_eq!(status(&response), 200, "{response}");
    assert!(body(&response).contains("no run in progress"), "{response}");
}

#[test]
fn unknown_route_is_404_and_known_route_with_wrong_verb_is_405() {
    let server = start();
    let missing = get(&server, "/nope");
    assert_eq!(status(&missing), 404, "{missing}");
    let wrong = get(&server, "/api/prompt");
    assert_eq!(status(&wrong), 405, "{wrong}");
    assert_eq!(header(&wrong, "allow"), Some("GET, POST"));
    let also_wrong = post(&server, "/api/health", "", "");
    assert_eq!(status(&also_wrong), 405, "{also_wrong}");
}

// ---------------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------------

#[test]
fn host_guard_blocks_dns_rebinding() {
    let server = start();
    let rebound = raw(&server, "GET /api/health HTTP/1.1\r\nhost: evil.example:80\r\nconnection: close\r\n\r\n");
    assert_eq!(status(&rebound), 403, "{rebound}");
    let wrong_port = raw(&server, "GET /api/health HTTP/1.1\r\nhost: localhost:1\r\nconnection: close\r\n\r\n");
    assert_eq!(status(&wrong_port), 403, "{wrong_port}");
    let bare = raw(&server, "GET /api/health HTTP/1.1\r\nconnection: close\r\n\r\n");
    assert_eq!(status(&bare), 403, "a missing host header is refused: {bare}");
    let good = raw(&server, &format!("GET /api/health HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nconnection: close\r\n\r\n", server.port));
    assert_eq!(status(&good), 200, "{good}");
}

#[test]
fn origin_and_referer_guard_blocks_cross_site_posts() {
    let server = start();
    let port = server.port;
    let cross = post(&server, "/api/cancel", "", "origin: http://evil.example\r\n");
    assert_eq!(status(&cross), 403, "{cross}");
    let nulled = post(&server, "/api/cancel", "", "origin: null\r\n");
    assert_eq!(status(&nulled), 403, "{nulled}");
    let bad_referer = post(&server, "/api/cancel", "", "referer: https://evil.example/goon\r\n");
    assert_eq!(status(&bad_referer), 403, "{bad_referer}");
    let good = post(&server, "/api/cancel", "", &format!("origin: http://127.0.0.1:{port}\r\n"));
    assert_eq!(status(&good), 200, "{good}");
    let good_localhost = post(&server, "/api/cancel", "", &format!("referer: http://localhost:{port}/app.js\r\n"));
    assert_eq!(status(&good_localhost), 200, "{good_localhost}");
    // No Origin at all means a local, non-browser client; browsers always send one cross-site.
    let bare = post(&server, "/api/cancel", "", "");
    assert_eq!(status(&bare), 200, "{bare}");
}

// ---------------------------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------------------------

#[test]
fn oversized_body_is_refused_with_413_before_it_is_read() {
    let server = start();
    let response = raw(&server, &format!("POST /api/prompt HTTP/1.1\r\nhost: localhost:{}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", server.port, http::MAX_BODY_BYTES + 1));
    assert_eq!(status(&response), 413, "{response}");
    assert!(body(&response).contains("body too large"), "{response}");
}

#[test]
fn oversized_headers_are_refused_with_431() {
    let server = start();
    let mut request = format!("GET /api/health HTTP/1.1\r\nhost: localhost:{}\r\n", server.port);
    let pad = "a".repeat(200);
    while request.len() < http::MAX_HEADER_BYTES + 4096 {
        request.push_str("x-pad: ");
        request.push_str(&pad);
        request.push_str("\r\n");
    }
    request.push_str("connection: close\r\n\r\n");
    let response = raw(&server, &request);
    assert_eq!(status(&response), 431, "{response}");
}

#[test]
fn oversized_request_line_is_refused_with_414() {
    let server = start();
    let path = format!("/{}", "a".repeat(http::MAX_REQUEST_LINE + 16));
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let _ = stream.write_all(format!("GET {path} HTTP/1.1\r\nhost: localhost:{}\r\nconnection: close\r\n\r\n", server.port).as_bytes());
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    let response = String::from_utf8_lossy(&out).to_string();
    assert_eq!(status(&response), 414, "{response}");
}

#[test]
fn duplicate_and_invalid_framing_headers_are_refused() {
    let server = start();
    let duplicated = raw(&server, &format!("POST /api/prompt HTTP/1.1\r\nhost: localhost:{}\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\n{{}}", server.port));
    assert_eq!(status(&duplicated), 400, "{duplicated}");
    let chunked = raw(&server, &format!("POST /api/prompt HTTP/1.1\r\nhost: localhost:{}\r\ntransfer-encoding: chunked\r\n\r\n", server.port));
    assert_eq!(status(&chunked), 400, "chunked framing is refused rather than guessed: {chunked}");
}

// ---------------------------------------------------------------------------------------------
// Prompt stream
// ---------------------------------------------------------------------------------------------

/// The spawn target used by the SSE test: a shell that exits immediately, so the assertions are
/// about the stream we write, not about a real agent run.
fn stub_shell() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").ok().map(PathBuf::from).or_else(|| Some(PathBuf::from("cmd.exe")))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from("/bin/sh"))
    }
}

#[test]
fn prompt_requires_text() {
    let server = start();
    let response = post(&server, "/api/prompt", "{}", "");
    assert_eq!(status(&response), 400, "{response}");
    assert!(body(&response).contains("prompt is required"), "{response}");
}

#[test]
fn prompt_stream_uses_sse_headers_and_ends_with_an_exit_event() {
    let Some(shell) = stub_shell() else { return };
    if let Ok(mut guard) = CHILD_BIN_OVERRIDE.lock() {
        *guard = Some(shell);
    }
    let server = start();
    // From here on the state is already captured by the running server, so other tests are not
    // affected by the override any more.
    if let Ok(mut guard) = CHILD_BIN_OVERRIDE.lock() {
        *guard = None;
    }
    let response = post(&server, "/api/prompt", r#"{"prompt":"hello"}"#, "");
    assert_eq!(status(&response), 200, "{response}");
    assert_eq!(header(&response, "content-type"), Some("text/event-stream"));
    assert_eq!(header(&response, "cache-control"), Some("no-store"));
    assert_eq!(header(&response, "x-accel-buffering"), Some("no"));
    assert_eq!(header(&response, "x-content-type-options"), Some("nosniff"));
    assert_eq!(header(&response, "connection"), Some("close"), "a stream is never kept alive");
    assert!(header(&response, "content-length").is_none(), "a stream is not length-delimited");

    let last = body(&response).lines().rfind(|line| line.starts_with("data: ")).expect("at least one data frame");
    let event: Value = serde_json::from_str(last.trim_start_matches("data: ")).expect("exit event is JSON");
    assert_eq!(event["type"], json!("exit"));
    assert!(event["code"].is_i64() || event["code"].is_u64(), "exit carries a code: {event}");
    assert!(event["cancelled"].is_boolean(), "exit says whether it was cancelled: {event}");

    // The run that just finished is in the history with the same shape `/api/runs` promises.
    let runs = json_body(&get(&server, "/api/runs"));
    let entries = runs.as_array().expect("/api/runs is a bare array");
    assert_eq!(entries.len(), 1, "{runs}");
    assert_eq!(entries[0]["prompt"], json!("hello"));
    assert!(entries[0]["id"].as_str().map(|id| id.starts_with("run-")).unwrap_or(false), "{runs}");
    assert!(entries[0]["startedAt"].is_string(), "{runs}");
    assert!(entries[0]["durationMs"].is_u64(), "a finished run has a duration: {runs}");
    assert!(entries[0]["exitCode"].is_i64() || entries[0]["exitCode"].is_u64(), "{runs}");
    assert_eq!(entries[0]["cancelled"], json!(false));
    // And health counts it.
    let health = json_body(&get(&server, "/api/health"));
    assert_eq!(health["runs"], json!(1), "{health}");
}

// ---------------------------------------------------------------------------------------------
// Unit tests for the pieces a socket test cannot reach
// ---------------------------------------------------------------------------------------------

#[test]
fn sse_frames_carry_one_data_line_and_pings_are_comments() {
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = sse::SseWriter::new(&mut buffer);
        writer.send("{\"type\":\"text\",\"text\":\"hi\"}").expect("send");
        writer.ping().expect("ping");
        writer.send("{\"type\":\"exit\",\"code\":0}").expect("send");
    }
    let text = String::from_utf8(buffer).expect("utf8");
    assert_eq!(text, "data: {\"type\":\"text\",\"text\":\"hi\"}\n\n: ping\n\ndata: {\"type\":\"exit\",\"code\":0}\n\n");
    // A ping is a comment: no `data:` prefix means no client-visible event.
    for frame in text.split("\n\n").filter(|frame| !frame.is_empty()) {
        assert!(frame.starts_with("data: ") || frame.starts_with(": "), "unexpected frame: {frame}");
    }
}

#[test]
fn request_parser_enforces_its_limits() {
    // A well-formed request round-trips.
    let input = b"GET /api/health?limit=7 HTTP/1.1\r\nHost: localhost:1\r\nContent-Length: 0\r\n\r\n";
    match http::read_request(&mut std::io::Cursor::new(input.to_vec())).expect("read") {
        http::Incoming::Request(request) => {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path(), "/api/health");
            assert_eq!(request.query(), "limit=7");
            assert_eq!(request.header("host"), Some("localhost:1"));
            assert!(request.wants_keep_alive());
        }
        _ => panic!("expected a parsed request"),
    }

    let huge_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(http::MAX_REQUEST_LINE + 8));
    match http::read_request(&mut std::io::Cursor::new(huge_line.into_bytes())).expect("read") {
        http::Incoming::Rejected { status, .. } => assert_eq!(status, 414),
        _ => panic!("expected a rejection"),
    }

    let huge_headers = format!("GET / HTTP/1.1\r\nx-pad: {}\r\n\r\n", "a".repeat(http::MAX_HEADER_BYTES + 8));
    match http::read_request(&mut std::io::Cursor::new(huge_headers.into_bytes())).expect("read") {
        http::Incoming::Rejected { status, .. } => assert_eq!(status, 431),
        _ => panic!("expected a rejection"),
    }

    let huge_body = format!("POST /api/prompt HTTP/1.1\r\ncontent-length: {}\r\n\r\n", http::MAX_BODY_BYTES + 1);
    match http::read_request(&mut std::io::Cursor::new(huge_body.into_bytes())).expect("read") {
        http::Incoming::Rejected { status, .. } => assert_eq!(status, 413),
        _ => panic!("expected a rejection"),
    }

    // Closed connection reads as empty rather than as an error.
    match http::read_request(&mut std::io::Cursor::new(Vec::new())).expect("read") {
        http::Incoming::Empty => {}
        _ => panic!("expected an empty read"),
    }
}

#[test]
fn host_origin_and_etag_matchers_behave() {
    assert!(host_allowed(Some("127.0.0.1:4321"), 4321));
    assert!(host_allowed(Some("LOCALHOST:4321"), 4321));
    assert!(!host_allowed(Some("localhost:4322"), 4321));
    assert!(!host_allowed(Some("evil.example:4321"), 4321));
    assert!(!host_allowed(None, 4321));

    assert!(origin_allowed(Some("http://127.0.0.1:4321"), None, 4321));
    assert!(origin_allowed(Some("http://localhost:4321/"), None, 4321));
    assert!(!origin_allowed(Some("http://evil.example"), None, 4321));
    assert!(!origin_allowed(Some("null"), None, 4321));
    assert!(origin_allowed(None, Some("http://localhost:4321/app.js"), 4321));
    assert!(!origin_allowed(None, Some("http://evil.example/app.js"), 4321));
    assert!(origin_allowed(None, None, 4321));
    assert_eq!(origin_of("http://127.0.0.1:1/a/b"), "http://127.0.0.1:1");
    assert_eq!(origin_of("http://127.0.0.1:1"), "http://127.0.0.1:1");

    assert!(etag_matches("\"abc\"", "\"abc\""));
    assert!(etag_matches("W/\"abc\"", "\"abc\""));
    assert!(etag_matches("\"x\", \"abc\"", "\"abc\""));
    assert!(etag_matches("*", "\"abc\""));
    assert!(!etag_matches("\"nope\"", "\"abc\""));
}

#[test]
fn session_id_validation_is_strict() {
    assert!(api::valid_session_id("0123456789abcdef0123456789abcdef"));
    assert!(api::valid_session_id("abc-DEF-123"));
    assert!(!api::valid_session_id(""));
    assert!(!api::valid_session_id("-"));
    assert!(!api::valid_session_id(".."));
    assert!(!api::valid_session_id("a/b"));
    assert!(!api::valid_session_id("a\\b"));
    assert!(!api::valid_session_id("a.b"));
    assert!(!api::valid_session_id("a b"));
    assert!(!api::valid_session_id("a%2fb"));
    assert!(!api::valid_session_id(&"a".repeat(65)));
}

#[test]
fn only_loopback_hosts_are_accepted() {
    assert!(validate_host("127.0.0.1").is_ok());
    assert!(validate_host("localhost").is_ok());
    assert!(validate_host("::1").is_ok());
    assert!(validate_host("0.0.0.0").is_err());
    assert!(validate_host("192.168.1.5").is_err());
}
