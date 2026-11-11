//! The one streaming endpoint: `POST /api/prompt` relays the child CLI's NDJSON events as
//! Server-Sent Events.
//!
//! Two details make this usable in a window that stays open for hours:
//!
//! - the child's stdout is drained on its own thread into a channel, so the writer can emit a
//!   `: ping` comment every 15 s while the agent is thinking instead of sitting in a blocking read
//!   (proxies and browsers both drop an idle stream, and the UI cannot tell a stall from a hang),
//! - the child's stderr is drained on a second thread, because a chatty child that fills its stderr
//!   pipe while we are blocked in `wait()` would deadlock the stream.
//!
//! The last event is always `{"type":"exit","code":N,...}` so the UI has one unambiguous terminator.

use super::{http, ServerState};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// Idle time after which a comment line goes out to keep the stream warm.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// The stderr tail echoed in the exit event is clamped to this many characters.
pub const MAX_STDERR_CHARS: usize = 4000;

/// Writes SSE frames. Generic over the sink so the framing is unit-testable without a socket.
pub struct SseWriter<W: Write> {
    out: W,
}

impl<W: Write> SseWriter<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    /// One `data:` frame carrying one already-serialised JSON line.
    pub fn send(&mut self, data: &str) -> std::io::Result<()> {
        self.out.write_all(b"data: ")?;
        self.out.write_all(data.as_bytes())?;
        self.out.write_all(b"\n\n")?;
        self.out.flush()
    }

    /// A comment line: legal SSE that carries no event, ignored by every client.
    pub fn ping(&mut self) -> std::io::Result<()> {
        self.out.write_all(b": ping\n\n")?;
        self.out.flush()
    }
}

/// The SSE response head: streaming, never cached, never buffered by a reverse proxy.
pub fn write_stream_head(stream: &mut TcpStream) -> std::io::Result<()> {
    http::write_head(stream, 200, Some("text/event-stream"), &[("cache-control", "no-store"), ("x-accel-buffering", "no")], None, false)?;
    stream.flush()
}

/// Stops a run nobody is listening to, clears the live PID and records it as finished.
fn abandon(state: &Arc<ServerState>, child: &mut std::process::Child, run_id: &str) {
    let _ = child.kill();
    let _ = child.wait();
    if let Ok(mut guard) = state.child_pid.lock() {
        *guard = None;
    }
    state.finish_run(run_id, None);
}

/// Runs one prompt and streams the child's events. Returns after the final `exit` frame.
///
/// The child keeps running if the window goes away: the run's work (edits, commits, checkpoints) is
/// not something to throw away because a tab was closed, and the CLI records terminal results for
/// every tool it started. Only the socket is abandoned.
pub fn stream_events(stream: &mut TcpStream, state: &Arc<ServerState>, prompt: &str) -> Result<()> {
    let mut command = Command::new(&state.child_bin);
    command.arg("--json").arg("--yes").arg("-p").arg(prompt).current_dir(&state.options.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for extra in &state.options.passthrough {
        command.arg(extra);
    }
    let mut child = command.spawn().with_context(|| format!("spawn {}", state.child_bin.display()))?;
    let run_id = state.begin_run(prompt);
    if let Ok(mut guard) = state.child_pid.lock() {
        *guard = Some(child.id());
    }
    let Some(stdout) = child.stdout.take() else {
        abandon(state, &mut child, &run_id);
        return Err(anyhow!("child has no stdout"));
    };
    let stderr = child.stderr.take();
    // If the window went away before the stream even started, there is nobody to consume the run:
    // stop it and record it, instead of leaving a live child and a permanently "running" record.
    if let Err(error) = write_stream_head(stream) {
        abandon(state, &mut child, &run_id);
        return Err(error).context("write the event stream head");
    }

    let (sender, receiver) = mpsc::channel::<String>();
    let stdout_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let stderr_reader = stderr.map(|stderr| {
        std::thread::spawn(move || {
            let mut tail: Vec<String> = Vec::new();
            let mut bytes = 0usize;
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                bytes += line.len();
                tail.push(line);
                if bytes > 4 * MAX_STDERR_CHARS * 4 {
                    // Keep the last chunk only; the tail is all the exit event shows anyway.
                    let drop_to = tail.len() / 2;
                    tail.drain(..drop_to);
                    bytes = tail.iter().map(String::len).sum();
                }
            }
            tail.join("\n")
        })
    });

    let mut writer = SseWriter::new(&mut *stream);
    let mut client_gone = false;
    loop {
        match receiver.recv_timeout(KEEP_ALIVE) {
            Ok(line) => {
                // Keep draining even after the client leaves: the child writes into a pipe we own,
                // and a full pipe would block it forever.
                if !client_gone && writer.send(&line).is_err() {
                    client_gone = true;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !client_gone && writer.ping().is_err() {
                    client_gone = true;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = stdout_reader.join();

    let waited = child.wait();
    if let Ok(mut guard) = state.child_pid.lock() {
        *guard = None;
    }
    let status = match waited {
        Ok(status) => status,
        Err(error) => {
            // The run's exit code is unknowable now, but the record must not stay "running".
            state.finish_run(&run_id, None);
            return Err(error).context("wait for the child run");
        }
    };
    let stderr_tail = stderr_reader.map(|handle| handle.join().unwrap_or_default()).unwrap_or_default();
    let cancelled = state.finish_run(&run_id, status.code());

    let mut note = json!({"type": "exit", "code": status.code().unwrap_or(-1), "cancelled": cancelled});
    if !stderr_tail.is_empty() {
        note["stderr"] = Value::String(crate::jsutil::take_chars(&stderr_tail, MAX_STDERR_CHARS).to_string());
    }
    if !client_gone {
        let _ = writer.send(&crate::jsutil::stringify(&note));
    }
    Ok(())
}
