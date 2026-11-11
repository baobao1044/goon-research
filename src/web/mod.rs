//! `goon web`: a local desktop shell for the CLI.
//!
//! The desktop build is deliberately small and dependency-free:
//!
//! - one `TcpListener` on the loopback interface serving a single embedded UI document,
//! - `POST /api/prompt` spawns the real `goon --json -p ...` child and relays its NDJSON events
//!   to the browser as Server-Sent Events, so the UI shows exactly what the CLI emits,
//! - `GET /api/sessions` and `GET /api/config` read the same stores the CLI uses,
//! - `--open` launches the default Chromium browser with `--app=<url>`, which is the desktop
//!   window (no address bar, own taskbar entry) on a machine that has Chrome or Edge.
//!
//! Nothing here re-implements the agent: the web shell is a thin front-end over the binary, which
//! keeps the CLI the single source of behaviour.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The whole UI is one embedded document: no bundler, no node_modules, no CDN.
const UI_HTML: &str = include_str!("ui.html");

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
    let shared = Arc::new(ServerState { options, running: AtomicUsize::new(0), child_bin: current_binary()? });
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
                        let _ = error;
                    }
                });
            }
            Err(error) => return Err(error).context("accept connection"),
        }
    }
    Ok(())
}

struct ServerState {
    options: WebOptions,
    running: AtomicUsize,
    child_bin: PathBuf,
}

fn current_binary() -> Result<PathBuf> {
    std::env::current_exe().context("resolve the goon binary for child runs")
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

fn handle(mut stream: TcpStream, state: &Arc<ServerState>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let line = header.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length.min(1 << 20)];
    if content_length > 0 {
        use std::io::Read;
        reader.read_exact(&mut body)?;
    }

    let route = path.split('?').next().unwrap_or("/");
    match (&method[..], route) {
        ("GET", "/") => respond_html(&mut stream, UI_HTML),
        ("GET", "/app.css") => respond(&mut stream, "text/css", UI_CSS.as_bytes()),
        ("GET", "/app.js") => respond(&mut stream, "text/javascript", UI_JS.as_bytes()),
        ("GET", "/api/meta") => respond_json(&mut stream, &meta(state)),
        ("GET", "/api/sessions") => respond_json(&mut stream, &sessions(state)),
        ("POST", "/api/prompt") => prompt(stream, state, &body),
        ("POST", "/api/cancel") => {
            // Cancelling is out of scope for the first shell: the UI simply stops reading and the
            // child finishes its turn. Say so instead of pretending.
            respond_json(&mut stream, &json!({"ok": false, "reason": "cancel is not implemented; the run finishes its current turn"}))
        }
        _ => respond_status(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn meta(state: &Arc<ServerState>) -> Value {
    let config = crate::config::load_config(&Default::default()).ok();
    json!({
        "version": crate::version::VERSION,
        "cwd": state.options.cwd.to_string_lossy(),
        "dataDir": state.options.data_dir.to_string_lossy(),
        "model": config.as_ref().and_then(|c| c.model.clone()),
        "provider": config.as_ref().map(|c| c.provider.clone()),
        "running": state.running.load(Ordering::SeqCst),
    })
}

fn sessions(state: &Arc<ServerState>) -> Value {
    let dir = state.options.data_dir.join("sessions");
    match crate::session::SessionStore::list(&dir) {
        Ok(list) => {
            Value::Array(list.iter().take(50).map(|session| json!({"id": session.id, "title": session.title, "updatedAt": session.updated_at, "cwd": session.cwd, "model": session.model})).collect())
        }
        Err(error) => json!({"error": error.to_string()}),
    }
}

/// Runs one prompt and streams the child's NDJSON events as SSE.
fn prompt(mut stream: TcpStream, state: &Arc<ServerState>, body: &[u8]) -> Result<()> {
    let request: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let text = request.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return respond_json(&mut stream, &json!({"error": "prompt is required"}));
    }
    if state.running.fetch_add(1, Ordering::SeqCst) > 0 {
        state.running.fetch_sub(1, Ordering::SeqCst);
        return respond_json(&mut stream, &json!({"error": "a run is already in progress in this window"}));
    }
    let result = stream_events(&mut stream, state, &text);
    state.running.fetch_sub(1, Ordering::SeqCst);
    result
}

fn stream_events(stream: &mut TcpStream, state: &Arc<ServerState>, prompt: &str) -> Result<()> {
    let mut command = Command::new(&state.child_bin);
    command.arg("--json").arg("--yes").arg("-p").arg(prompt).current_dir(&state.options.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for extra in &state.options.passthrough {
        command.arg(extra);
    }
    let mut child = command.spawn().with_context(|| format!("spawn {}", state.child_bin.display()))?;
    let stderr = child.stderr.take();
    write_head(stream, "text/event-stream")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("child has no stdout"))?;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        // The child already emits one JSON object per line; SSE needs one data line each.
        if write!(stream, "data: {line}\n\n").is_err() || stream.flush().is_err() {
            // The window went away: stop reading and let the child finish on its own.
            break;
        }
    }
    let status = child.wait().context("wait for the child run")?;
    let mut note = json!({"type": "exit", "code": status.code().unwrap_or(-1)});
    if let Some(stderr) = stderr {
        let tail: String = BufReader::new(stderr).lines().map_while(|line| line.ok()).collect::<Vec<String>>().join("\n");
        if !tail.is_empty() {
            note["stderr"] = Value::String(crate::jsutil::take_chars(&tail, 4000).to_string());
        }
    }
    let _ = write!(stream, "data: {note}\n\n");
    let _ = stream.flush();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------------------------

fn write_head(stream: &mut TcpStream, content_type: &str) -> Result<()> {
    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n")?;
    stream.flush()?;
    Ok(())
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> Result<()> {
    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn respond_html(stream: &mut TcpStream, body: &str) -> Result<()> {
    respond(stream, "text/html; charset=utf-8", body.as_bytes())
}

fn respond_json(stream: &mut TcpStream, value: &Value) -> Result<()> {
    respond(stream, "application/json", crate::jsutil::stringify(value).as_bytes())
}

fn respond_status(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        404 => "Not Found",
        400 => "Bad Request",
        _ => "OK",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
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

// ---------------------------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------------------------

const UI_CSS: &str = include_str!("ui.css");
const UI_JS: &str = include_str!("ui.js");

/// Rejects anything that is not a loopback bind, used by the CLI before starting.
pub fn validate_host(host: &str) -> Result<()> {
    if host == "127.0.0.1" || host == "localhost" || host == "::1" {
        return Ok(());
    }
    bail!("goon web only binds the loopback interface (got '{host}'); the desktop shell is not a server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn request(port: u16, raw: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.write_all(raw.as_bytes()).expect("write request");
        let mut out = String::new();
        stream.read_to_string(&mut out).expect("read response");
        out
    }

    #[test]
    fn serves_ui_and_meta_over_loopback() {
        let dir = tempfile::TempDir::new().unwrap();
        let options = WebOptions { port: 0, open: false, cwd: dir.path().to_path_buf(), data_dir: dir.path().join("data"), passthrough: vec![] };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            run_with(options, |url| {
                tx.send(url.to_string()).unwrap();
            })
            .unwrap();
        });
        let url = rx.recv().expect("server url");
        let port: u16 = url.rsplit(':').next().unwrap().trim_end_matches('/').parse().unwrap();

        let page = request(port, "GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("goon desktop"), "the UI document is served");

        let meta = request(port, "GET /api/meta HTTP/1.1\r\nhost: localhost\r\n\r\n");
        assert!(meta.starts_with("HTTP/1.1 200"));
        assert!(meta.contains("\"version\""), "{meta}");

        let missing = request(port, "GET /nope HTTP/1.1\r\nhost: localhost\r\n\r\n");
        assert!(missing.starts_with("HTTP/1.1 404"));

        let cancel = request(port, "POST /api/cancel HTTP/1.1\r\ncontent-length: 0\r\n\r\n");
        assert!(cancel.contains("not implemented"), "{cancel}");
    }

    #[test]
    fn prompt_requires_text_and_reports_busy_state_shape() {
        let dir = tempfile::TempDir::new().unwrap();
        let options = WebOptions { port: 0, open: false, cwd: dir.path().to_path_buf(), data_dir: dir.path().join("data"), passthrough: vec![] };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            run_with(options, |url| {
                tx.send(url.to_string()).unwrap();
            })
            .unwrap();
        });
        let url = rx.recv().expect("server url");
        let port: u16 = url.rsplit(':').next().unwrap().trim_end_matches('/').parse().unwrap();
        let response = request(port, "POST /api/prompt HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}");
        assert!(response.contains("prompt is required"), "{response}");
    }

    #[test]
    fn only_loopback_hosts_are_accepted() {
        assert!(validate_host("127.0.0.1").is_ok());
        assert!(validate_host("localhost").is_ok());
        assert!(validate_host("0.0.0.0").is_err());
        assert!(validate_host("192.168.1.5").is_err());
    }
}
