//! The JSON endpoints of the desktop shell.
//!
//! Everything here is read-only over the same stores the CLI uses, except `cancel`, which signals
//! the live child run. The rule for every payload: it is safe to hand to a page, so no API key, no
//! `base_url` (it can carry a key in the query string on some gateways) and no absolute paths other
//! than the workspace/data directory the window is already showing.

use super::{RunRecord, ServerState};
use serde_json::{json, Value};
use std::sync::Arc;

/// How many envelopes `/api/sessions/:id/events` returns when the caller does not ask for a count.
pub const DEFAULT_EVENT_LIMIT: usize = 400;
/// The hard ceiling for that endpoint, whatever the caller asks for.
pub const MAX_EVENT_LIMIT: usize = 2000;
/// `read_event_log` loads the whole log; refuse absurd files instead of buffering them.
pub const MAX_EVENT_LOG_BYTES: u64 = 16 * 1024 * 1024;
/// `GET /api/runs` never returns more than this many entries.
pub const MAX_RUNS: usize = 20;

/// Session ids are `uuid`-shaped today; anything else is refused, which is what blocks traversal.
///
/// Restricting the alphabet to hex digits and `-` means `.`, `/`, `\`, `%` and every encoding trick
/// around them cannot survive to the filesystem join.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-') && id != "-"
}

/// Turns a callable route into a loggable label, so the session id never lands in a log line.
pub fn route_label(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        if rest.ends_with("/events") {
            return "/api/sessions/:id/events";
        }
    }
    path
}

/// The `:id` of `/api/sessions/:id/events`, or `None` when the path is something else.
pub fn session_events_id(path: &str) -> Option<&str> {
    path.strip_prefix("/api/sessions/")?.strip_suffix("/events")
}

/// `limit=N` from the query string, clamped to `1..=MAX_EVENT_LIMIT` with a default of 400.
pub fn parse_event_limit(query: &str) -> usize {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        if key != "limit" {
            continue;
        }
        if let Ok(parsed) = value.parse::<usize>() {
            return parsed.clamp(1, MAX_EVENT_LIMIT);
        }
    }
    DEFAULT_EVENT_LIMIT
}

/// `{"ok":true,"version","uptimeMs","runs"}`.
///
/// `runs` counts the runs the window remembers (the same list `/api/runs` returns), not the number
/// currently executing; `running` covers that and is what `/api/meta` has always reported.
pub fn health(state: &Arc<ServerState>) -> Value {
    let runs = state.runs.lock().map(|guard| guard.len()).unwrap_or(0);
    let uptime_ms = u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    json!({
        "ok": true,
        "version": crate::version::VERSION,
        "uptimeMs": uptime_ms,
        "runs": runs,
    })
}

/// `{"provider","model","effort","cwd","dataDir","eventLog","readOnly"}`.
///
/// Deliberately absent: `base_url` (a gateway URL may embed a key), `key_env`, and the key itself.
pub fn config(state: &Arc<ServerState>) -> Value {
    let loaded = crate::config::load_config(&Default::default()).ok();
    json!({
        "provider": loaded.as_ref().map(|config| config.provider.clone()).unwrap_or_default(),
        "model": loaded.as_ref().and_then(|config| config.model.clone()),
        "effort": loaded.as_ref().and_then(|config| config.effort.clone()),
        "cwd": state.options.cwd.to_string_lossy(),
        "dataDir": state.options.data_dir.to_string_lossy(),
        "eventLog": loaded.as_ref().map(|config| config.event_log).unwrap_or(false),
        "readOnly": false,
    })
}

/// The `window.__goon_meta` shaped payload the first UI used; kept because the same document still
/// reads it and it carries nothing the newer endpoints do not.
pub fn meta(state: &Arc<ServerState>) -> Value {
    let loaded = crate::config::load_config(&Default::default()).ok();
    json!({
        "version": crate::version::VERSION,
        "cwd": state.options.cwd.to_string_lossy(),
        "dataDir": state.options.data_dir.to_string_lossy(),
        "model": loaded.as_ref().and_then(|config| config.model.clone()),
        "provider": loaded.as_ref().map(|config| config.provider.clone()),
        "running": state.running.load(std::sync::atomic::Ordering::SeqCst),
    })
}

/// Recent runs, newest first: `[{"id","prompt","startedAt","durationMs","exitCode","cancelled"}]`.
///
/// The array is the response body itself, not an object wrapping it.
pub fn runs(state: &Arc<ServerState>) -> Value {
    let Ok(guard) = state.runs.lock() else { return Value::Array(Vec::new()) };
    Value::Array(guard.iter().rev().map(RunRecord::to_value).collect())
}

/// Session summaries, newest first, capped like the CLI's own listing.
///
/// `hasEvents` tells the window whether replay is possible at all: event logging is opt-in, so a
/// session recorded without `--event-log` has no log to walk, and the UI must say that instead of
/// showing an empty transcript.
pub fn sessions(state: &Arc<ServerState>) -> Value {
    let dir = state.options.data_dir.join("sessions");
    match crate::session::SessionStore::list(&dir) {
        Ok(list) => Value::Array(
            list.iter()
                .take(50)
                .map(|session| {
                    let has_events = crate::eventlog::event_log_path(&state.options.data_dir, &session.id).is_file();
                    json!({"id": session.id, "title": session.title, "updatedAt": session.updated_at, "cwd": session.cwd, "model": session.model, "hasEvents": has_events})
                })
                .collect(),
        ),
        Err(error) => json!({"error": error.to_string()}),
    }
}

/// `{"id","events":[...envelopes...],"truncated":bool}` for one session's event log.
///
/// The tail is returned: a UI wants the newest events first, and `truncated` tells it that older
/// ones were dropped. A torn final line (the CLI was killed mid-append) is reported the same way
/// rather than as an error, which matches `goon replay`.
pub fn session_events(state: &Arc<ServerState>, id: &str, limit: usize) -> (u16, Value) {
    let path = crate::eventlog::event_log_path(&state.options.data_dir, id);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return (404, json!({"error": "session not found", "id": id}));
    };
    if metadata.len() > MAX_EVENT_LOG_BYTES {
        return (413, json!({"error": "event log is too large for the desktop shell", "id": id}));
    }
    let Ok(read) = crate::eventlog::read_event_log(&path) else {
        return (500, json!({"error": "cannot read the session event log", "id": id}));
    };
    let total = read.envelopes.len();
    let truncated = read.truncated || total > limit;
    let start = total.saturating_sub(limit);
    let events: Vec<Value> = read.envelopes.iter().skip(start).map(crate::eventlog::EventEnvelope::to_value).collect();
    (200, json!({"id": id, "events": events, "truncated": truncated}))
}

/// Interrupts the live child run, if any.
///
/// The CLI already treats termination as a clean cancellation (it checkpoints and records terminal
/// tool results), so killing the child is the correct stop signal rather than a special protocol.
pub fn cancel(state: &Arc<ServerState>) -> Value {
    let pid = state.child_pid.lock().map(|guard| *guard).unwrap_or(None);
    let Some(pid) = pid else {
        return json!({"ok": false, "reason": "no run in progress"});
    };
    match terminate(pid) {
        Ok(()) => {
            state.mark_cancelled();
            json!({"ok": true, "pid": pid})
        }
        Err(error) => json!({"ok": false, "reason": error.to_string()}),
    }
}

#[cfg(windows)]
fn terminate(pid: u32) -> anyhow::Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| anyhow::anyhow!("run taskkill: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("taskkill exited with {status}")
    }
}

#[cfg(not(windows))]
fn terminate(pid: u32) -> anyhow::Result<()> {
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| anyhow::anyhow!("run kill: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("kill exited with {status}")
    }
}
