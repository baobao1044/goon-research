//! `goon web`: a local desktop shell for the CLI.
//!
//! The desktop build is deliberately small and dependency-free:
//!
//! - one `TcpListener` on the loopback interface serving a single embedded UI document,
//! - `POST /api/prompt` spawns the real `goon --json -p ...` child and relays its NDJSON events
//!   to the browser as Server-Sent Events, so the UI shows exactly what the CLI emits,
//! - the `/api/*` endpoints read the same stores the CLI uses,
//! - `--open` launches the default Chromium browser with `--app=<url>`, which is the desktop
//!   window (no address bar, own taskbar entry) on a machine that has Chrome or Edge.
//!
//! Nothing here re-implements the agent: the web shell is a thin front-end over the binary, which
//! keeps the CLI the single source of behaviour.
//!
//! # Threat model
//!
//! This is a browser-reachable process that can spawn an agent with file-write tools, so the HTTP
//! layer refuses the two ways a remote page can reach a loopback server:
//!
//! - **DNS rebinding** — the `Host` header must be `127.0.0.1:<port>` or `localhost:<port>`; a
//!   rebound hostname arrives with the attacker's domain in `Host` and gets `403`.
//! - **Cross-site request forgery** — a `POST` whose `Origin`/`Referer` is present but not one of
//!   those two origins gets `403`. Absent `Origin` is accepted: browsers always send it on a
//!   cross-site `POST`, so its absence means a local, non-browser client.
//!
//! Every request is also bounded (see [`http`]), so a local or rebound peer cannot make the shell
//! allocate without limit. None of this is a substitute for not exposing the port: the guard is
//! "loopback plus same-origin", not an authentication layer.
//!
//! # Module map
//!
//! - [`http`] — bounded request parsing, response writing, status reasons.
//! - [`api`] — the JSON endpoints and their payload shapes.
//! - [`sse`] — the prompt stream: child process, keep-alive comments, exit event.

mod api;
mod http;
mod sse;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// The whole UI is one embedded document: no bundler, no node_modules, no CDN.
const UI_HTML: &str = include_str!("ui.html");
const UI_CSS: &str = include_str!("ui.css");
const UI_JS: &str = include_str!("ui.js");

/// A kept-alive socket is reused for a bounded number of requests, then closed. Browsers pipeline a
/// document plus its assets; nothing legitimate needs more than this in one connection.
const MAX_REQUESTS_PER_CONNECTION: usize = 64;

/// Options for one desktop session.
pub struct WebOptions {
    pub port: u16,
    pub open: bool,
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    /// Extra CLI arguments forwarded to every child run (`--model`, `--effort`, ...).
    pub passthrough: Vec<String>,
}

impl Default for WebOptions {
    fn default() -> Self {
        Self { port: 0, open: true, cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")), data_dir: crate::config::data_dir(), passthrough: Vec::new() }
    }
}

/// Starts the server and blocks until the process is killed.
pub fn run(options: WebOptions) -> Result<()> {
    run_with(options, |_| {})
}

/// Starts the server, hands the bound URL to `on_ready`, then serves forever.
///
/// Split out so tests can bind an ephemeral port and drive real HTTP requests without opening a
/// browser window.
pub fn run_with<F: FnOnce(&str)>(options: WebOptions, on_ready: F) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", options.port)).with_context(|| format!("bind 127.0.0.1:{}", options.port))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");
    let shared = Arc::new(ServerState {
        options,
        port,
        started: Instant::now(),
        running: AtomicUsize::new(0),
        child_bin: current_binary()?,
        child_pid: Mutex::new(None),
        runs: Mutex::new(VecDeque::new()),
        run_seq: AtomicU64::new(0),
    });
    on_ready(&url);
    if shared.options.open {
        if let Err(error) = open_window(&url) {
            crate::cli::stderr_write(&format!("goon web: could not open a window ({error}); open {url} manually\n"));
        }
    }
    crate::cli::stderr_write(&format!("goon web: listening on {url} (Ctrl+C to stop)\n"));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || {
                    if let Err(error) = handle(stream, &state) {
                        // A client that walked away mid-response is normal; anything else is worth
                        // one line. Neither path ever includes a prompt or a response body.
                        if !is_disconnect_error(&error) {
                            crate::cli::stderr_write(&format!("goon web: {error}\n"));
                        }
                    }
                });
            }
            Err(error) => return Err(error).context("accept connection"),
        }
    }
    Ok(())
}

/// One in-memory record of a prompt run, shaped exactly like `GET /api/runs` entries.
pub struct RunRecord {
    pub id: String,
    pub prompt: String,
    pub started_at: String,
    pub started: Instant,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
}

impl RunRecord {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "prompt": self.prompt,
            "startedAt": self.started_at,
            "durationMs": self.duration_ms,
            "exitCode": self.exit_code,
            "cancelled": self.cancelled,
        })
    }
}

struct ServerState {
    options: WebOptions,
    /// The actually bound port; the `Host` guard compares against this, so `--port 0` still works.
    port: u16,
    started: Instant,
    running: AtomicUsize,
    child_bin: PathBuf,
    /// PID of the live child run, so the UI's Stop button can actually interrupt it.
    child_pid: Mutex<Option<u32>>,
    runs: Mutex<VecDeque<RunRecord>>,
    run_seq: AtomicU64,
}

impl ServerState {
    /// Registers a run before the child produces anything, so `/api/runs` shows it while it runs.
    fn begin_run(&self, prompt: &str) -> String {
        let sequence = self.run_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("run-{sequence}");
        let record = RunRecord { id: id.clone(), prompt: prompt.to_string(), started_at: crate::jsutil::now_iso(), started: Instant::now(), duration_ms: None, exit_code: None, cancelled: false };
        if let Ok(mut guard) = self.runs.lock() {
            while guard.len() >= api::MAX_RUNS {
                guard.pop_front();
            }
            guard.push_back(record);
        }
        id
    }

    /// Records the outcome. Returns whether the run was cancelled, for the exit event.
    fn finish_run(&self, id: &str, exit_code: Option<i32>) -> bool {
        let Ok(mut guard) = self.runs.lock() else { return false };
        let Some(record) = guard.iter_mut().rev().find(|record| record.id == id) else { return false };
        record.exit_code = exit_code;
        record.duration_ms = Some(u64::try_from(record.started.elapsed().as_millis()).unwrap_or(u64::MAX));
        record.cancelled
    }

    /// Flags the newest unfinished run as cancelled; `/api/cancel` calls this after signalling.
    fn mark_cancelled(&self) {
        if let Ok(mut guard) = self.runs.lock() {
            if let Some(record) = guard.iter_mut().rev().find(|record| record.exit_code.is_none()) {
                record.cancelled = true;
            }
        }
    }
}

fn current_binary() -> Result<PathBuf> {
    // Tests cannot point `WebOptions` at a stub binary (the struct shape is the CLI's contract), so
    // the one seam lives here and only exists in a test build.
    #[cfg(test)]
    if let Some(path) = CHILD_BIN_OVERRIDE.lock().ok().and_then(|guard| guard.clone()) {
        return Ok(path);
    }
    std::env::current_exe().context("resolve the goon binary for child runs")
}

#[cfg(test)]
pub(crate) static CHILD_BIN_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

// ---------------------------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------------------------

fn handle(stream: TcpStream, state: &Arc<ServerState>) -> Result<()> {
    stream.set_nodelay(true).ok();
    // A stalled peer must not pin a thread forever; reads that time out are treated as a close.
    stream.set_read_timeout(Some(http::READ_TIMEOUT)).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        let request = match http::read_request(&mut reader) {
            Ok(http::Incoming::Empty) => return Ok(()),
            Ok(http::Incoming::Rejected { status, message }) => {
                let body = format!("{message}\n");
                let _ = http::write_text(&mut stream, status, &body, false);
                log_request("-", "-", status);
                return Ok(());
            }
            Ok(http::Incoming::Request(request)) => request,
            Err(error) if is_io_disconnect(&error) => return Ok(()),
            Err(error) => return Err(error).context("read request"),
        };
        let (status, keep_alive) = dispatch(&mut stream, state, &request)?;
        log_request(&request.method, api::route_label(request.path()), status);
        if !keep_alive {
            return Ok(());
        }
    }
    Ok(())
}

/// Routes one parsed request. Returns the status to log and whether the socket stays open.
fn dispatch(stream: &mut TcpStream, state: &Arc<ServerState>, request: &http::Request) -> Result<(u16, bool)> {
    // Guard 1: Host. Without it, any hostname that resolves to 127.0.0.1 (DNS rebinding) is a
    // same-origin page as far as the browser is concerned.
    if !host_allowed(request.header("host"), state.port) {
        return reject(stream, 403, "host is not the local desktop window");
    }
    // Guard 2: Origin/Referer on state-changing requests.
    if request.method != "GET" && !origin_allowed(request.header("origin"), request.header("referer"), state.port) {
        return reject(stream, 403, "origin is not the local desktop window");
    }
    let keep = request.wants_keep_alive();
    let path = request.path();
    match (request.method.as_str(), path) {
        ("GET", "/") => {
            http::write_response(stream, 200, "text/html; charset=utf-8", UI_HTML.as_bytes(), &[("cache-control", "no-store"), ("content-security-policy", http::DOCUMENT_CSP)], keep)?;
            Ok((200, keep))
        }
        ("GET", "/app.css") => Ok((asset(stream, UI_CSS.as_bytes(), "text/css; charset=utf-8", &CSS_ETAG, request.header("if-none-match"), keep)?, keep)),
        ("GET", "/app.js") => Ok((asset(stream, UI_JS.as_bytes(), "text/javascript; charset=utf-8", &JS_ETAG, request.header("if-none-match"), keep)?, keep)),
        ("GET", "/api/health") => {
            reply_json(stream, 200, &api::health(state), keep)?;
            Ok((200, keep))
        }
        ("GET", "/api/config") => {
            reply_json(stream, 200, &api::config(state), keep)?;
            Ok((200, keep))
        }
        ("GET", "/api/meta") => {
            reply_json(stream, 200, &api::meta(state), keep)?;
            Ok((200, keep))
        }
        ("GET", "/api/runs") => {
            reply_json(stream, 200, &api::runs(state), keep)?;
            Ok((200, keep))
        }
        ("GET", "/api/sessions") => {
            reply_json(stream, 200, &api::sessions(state), keep)?;
            Ok((200, keep))
        }
        ("GET", _) if api::session_events_id(path).is_some() => {
            let Some(id) = api::session_events_id(path) else { return not_found(stream) };
            if !api::valid_session_id(id) {
                return reject(stream, 400, "invalid session id");
            }
            let (status, payload) = api::session_events(state, id, api::parse_event_limit(request.query()));
            reply_json(stream, status, &payload, keep)?;
            Ok((status, keep))
        }
        ("POST", "/api/prompt") => {
            let status = prompt(stream, state, &request.body)?;
            Ok((status, false))
        }
        ("POST", "/api/cancel") => {
            reply_json(stream, 200, &api::cancel(state), false)?;
            Ok((200, false))
        }
        // A known path with the wrong verb is a client bug, not a missing route.
        (_, "/" | "/app.css" | "/app.js" | "/api/health" | "/api/config" | "/api/meta" | "/api/runs" | "/api/sessions" | "/api/prompt" | "/api/cancel") => {
            http::write_response(stream, 405, "text/plain; charset=utf-8", b"method not allowed\n", &[("allow", "GET, POST"), ("cache-control", "no-store")], false)?;
            Ok((405, false))
        }
        _ => not_found(stream),
    }
}

fn not_found(stream: &mut TcpStream) -> Result<(u16, bool)> {
    http::write_text(stream, 404, "not found\n", false)?;
    Ok((404, false))
}

/// Runs one prompt and streams the child's NDJSON events as SSE.
fn prompt(stream: &mut TcpStream, state: &Arc<ServerState>, body: &[u8]) -> Result<u16> {
    let request: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let text = request.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        reply_json(stream, 400, &json!({"error": "prompt is required"}), false)?;
        return Ok(400);
    }
    if state.running.fetch_add(1, Ordering::SeqCst) > 0 {
        state.running.fetch_sub(1, Ordering::SeqCst);
        reply_json(stream, 409, &json!({"error": "a run is already in progress in this window"}), false)?;
        return Ok(409);
    }
    let result = sse::stream_events(stream, state, &text);
    state.running.fetch_sub(1, Ordering::SeqCst);
    result.map(|()| 200)
}

// ---------------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------------

/// The `Host` header must name the exact loopback endpoint the listener bound.
fn host_allowed(header: Option<&str>, port: u16) -> bool {
    let Some(value) = header else { return false };
    let value = value.trim();
    value.eq_ignore_ascii_case(&format!("127.0.0.1:{port}")) || value.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

/// Only the two loopback origins are same-window; anything else (including `null`) is cross-site.
fn origin_allowed(origin: Option<&str>, referer: Option<&str>, port: u16) -> bool {
    if let Some(origin) = origin {
        return is_loopback_origin(origin, port);
    }
    match referer {
        Some(referer) => is_loopback_origin(origin_of(referer), port),
        None => true,
    }
}

fn is_loopback_origin(value: &str, port: u16) -> bool {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.eq_ignore_ascii_case("null") {
        return false;
    }
    trimmed.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}")) || trimmed.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
}

/// `scheme://authority` of a URL, without pulling in a parser for one comparison.
fn origin_of(url: &str) -> &str {
    let Some(scheme_end) = url.find("://") else { return url };
    let start = scheme_end + 3;
    match url[start..].find('/') {
        Some(offset) => &url[..start + offset],
        None => url,
    }
}

// ---------------------------------------------------------------------------------------------
// Assets and responses
// ---------------------------------------------------------------------------------------------

static CSS_ETAG: OnceLock<String> = OnceLock::new();
static JS_ETAG: OnceLock<String> = OnceLock::new();

fn etag_for(bytes: &[u8], slot: &'static OnceLock<String>) -> &'static str {
    slot.get_or_init(|| format!("\"{}\"", sha256_hex(bytes)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Weak and strong forms both count, and `*` matches anything (RFC 9110 §13.1.2).
fn etag_matches(header: &str, etag: &str) -> bool {
    header.split(',').map(str::trim).any(|candidate| {
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate == "*" || candidate == etag
    })
}

/// Serves an embedded asset with a strong ETag, answering `304` when the browser already has it.
///
/// The digest is computed once per process and cached, so a revalidated asset costs one header
/// comparison and no body write.
fn asset(stream: &mut TcpStream, body: &[u8], content_type: &str, slot: &'static OnceLock<String>, if_none_match: Option<&str>, keep_alive: bool) -> Result<u16> {
    let etag = etag_for(body, slot);
    // `no-cache` means "store it, but revalidate": the 304 path is exercised instead of a blind
    // re-download, and a rebuilt binary is picked up immediately.
    let extra = [("cache-control", "no-cache"), ("etag", etag)];
    if let Some(header) = if_none_match {
        if etag_matches(header, etag) {
            http::write_head(stream, 304, None, &extra, None, keep_alive)?;
            stream.flush()?;
            return Ok(304);
        }
    }
    http::write_response(stream, 200, content_type, body, &extra, keep_alive)?;
    Ok(200)
}

fn reply_json(stream: &mut TcpStream, status: u16, value: &Value, keep_alive: bool) -> Result<()> {
    let body = crate::jsutil::stringify(value);
    http::write_response(stream, status, "application/json; charset=utf-8", body.as_bytes(), &[("cache-control", "no-store")], keep_alive)?;
    Ok(())
}

fn reject(stream: &mut TcpStream, status: u16, message: &str) -> Result<(u16, bool)> {
    reply_json(stream, status, &json!({"error": message}), false)?;
    Ok((status, false))
}

/// One line per request: method, route and status. Never the prompt, never a response body.
fn log_request(method: &str, route: &str, status: u16) {
    crate::cli::stderr_write(&format!("goon web: {method} {route} {status}\n"));
}

fn is_io_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe
    )
}

fn is_disconnect_error(error: &anyhow::Error) -> bool {
    // Contexts wrap the io error, so walk the chain instead of only looking at the top.
    error.chain().filter_map(|cause| cause.downcast_ref::<std::io::Error>()).any(is_io_disconnect)
}

// ---------------------------------------------------------------------------------------------
// Desktop window
// ---------------------------------------------------------------------------------------------

/// Opens the UI as a Chromium app window, which is what makes it look like a desktop app.
///
/// Tries Chrome, then Edge, then falls back to the default browser (`ShellExecute`), so the shell
/// still works on a machine without either installed.
fn open_window(url: &str) -> Result<()> {
    for candidate in browser_candidates() {
        if !candidate.exists() {
            continue;
        }
        let mut command = Command::new(&candidate);
        command.arg(format!("--app={url}")).arg("--window-size=1280,860").arg("--no-first-run").arg("--no-default-browser-check");
        // A dedicated profile keeps the app window from stealing the normal browsing session.
        command.arg(format!("--user-data-dir={}", app_profile_dir()?.display()));
        if command.spawn().is_ok() {
            return Ok(());
        }
    }
    open_default_browser(url)
}

fn browser_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(root) = std::env::var(base) {
                out.push(Path::new(&root).join("Google/Chrome/Application/chrome.exe"));
                out.push(Path::new(&root).join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
        out.push(PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
    } else {
        out.push(PathBuf::from("/usr/bin/google-chrome"));
        out.push(PathBuf::from("/usr/bin/chromium"));
        out.push(PathBuf::from("/usr/bin/microsoft-edge"));
    }
    out
}

fn app_profile_dir() -> Result<PathBuf> {
    let dir = crate::config::data_dir().join("desktop-profile");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

#[cfg(windows)]
fn open_default_browser(url: &str) -> Result<()> {
    // `start` is a cmd.exe builtin, so it needs the shell rather than CreateProcess.
    Command::new("cmd").args(["/C", "start", "", url]).spawn().map(|_| ()).context("launch the default browser")
}

#[cfg(target_os = "macos")]
fn open_default_browser(url: &str) -> Result<()> {
    Command::new("open").arg(url).spawn().map(|_| ()).context("open the default browser")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_default_browser(url: &str) -> Result<()> {
    Command::new("xdg-open").arg(url).spawn().map(|_| ()).context("open the default browser")
}

/// Rejects anything that is not a loopback bind, used by the CLI before starting.
pub fn validate_host(host: &str) -> Result<()> {
    if host == "127.0.0.1" || host == "localhost" || host == "::1" {
        return Ok(());
    }
    bail!("goon web only binds the loopback interface (got '{host}'); the desktop shell is not a server")
}
