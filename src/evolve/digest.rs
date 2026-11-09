//! Experience observability: distills raw session trajectories into a layered, drill-down
//! evidence corpus (AHE pillar 2). L0 = raw NDJSON event log (never rewritten); L1 = per-task
//! digest; L2 = failure clusters; L3 = actionable evidence items.
#![allow(dead_code)]

use crate::eventlog::{read_event_log, EventEnvelope};
use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Bounds for one digest pass; the corpus must stay cheap to read on a phone.
#[derive(Clone, Debug)]
pub struct DigestOptions {
    pub max_sessions: usize,
    pub max_tasks: usize,
    pub max_tokens: u64,
}

impl Default for DigestOptions {
    fn default() -> Self {
        Self { max_sessions: 40, max_tasks: 20, max_tokens: 6000 }
    }
}

/// L1 digest of a single task attempt.
#[derive(Clone, Debug, Default)]
pub struct TaskDigest {
    pub task_id: String,
    pub session_id: String,
    pub passed: bool,
    pub failure: Option<FailureSignal>,
    pub steps: u32,
    pub tool_errors: Vec<String>,
    pub verify_issues: Vec<String>,
    pub key_events: Vec<String>,
    pub tokens: u64,
}

const EXCERPT_LIMIT: usize = 180;
const DETAIL_LIMIT: usize = 20;
const PROMPT_TASKS: usize = 4;
const PROMPT_SESSIONS: usize = 4;

/// Reads `{data}/sessions/*.events.jsonl` and produces (corpus, per-task digests).
pub fn digest_sessions(sessions_dir: &Path, opts: &DigestOptions) -> Result<(EvidenceCorpus, Vec<TaskDigest>)> {
    let paths = event_log_paths(sessions_dir)?;
    let session_limit = if opts.max_sessions == 0 { usize::MAX } else { opts.max_sessions };
    let task_limit = if opts.max_tasks == 0 { usize::MAX } else { opts.max_tasks };
    let token_budget = if opts.max_tokens == 0 { u64::MAX } else { opts.max_tokens };

    let mut corpus = EvidenceCorpus::empty();
    let mut truncated = paths.len() > session_limit;
    let mut digests: BTreeMap<String, TaskDigest> = BTreeMap::new();
    let mut clusters: BTreeMap<(String, String), Cluster> = BTreeMap::new();

    for path in paths.iter().take(session_limit) {
        match read_event_log(path) {
            Ok(log) if !log.envelopes.is_empty() => {
                // A malformed final line that ends with a newline is a complete corrupt line
                // (skip the log); only an unterminated tail is a torn append worth keeping.
                let newline_terminated = std::fs::read_to_string(path).map(|raw| raw.ends_with('\n')).unwrap_or(true);
                if log.truncated && newline_terminated {
                    corpus.sessions_skipped += 1;
                } else {
                    corpus.sessions_scanned += 1;
                    scan_session(&log.envelopes, &mut clusters, &mut digests);
                }
            }
            // A missing/torn/corrupt log contributes nothing but is counted, never fatal.
            Ok(_) | Err(_) => corpus.sessions_skipped += 1,
        }
    }

    let mut digests: Vec<TaskDigest> = digests.into_values().collect();
    if digests.len() > task_limit {
        digests.truncate(task_limit);
        truncated = true;
    }

    let mut items: Vec<EvidenceItem> = clusters.into_values().map(cluster_item).collect();
    items.sort_by(|a, b| b.occurrences.cmp(&a.occurrences).then_with(|| a.signature.cmp(&b.signature)));
    let mut kept: Vec<EvidenceItem> = Vec::new();
    let mut tokens = 0u64;
    for item in items {
        let cost = item_cost(&item);
        if tokens.saturating_add(cost) > token_budget {
            truncated = true;
            break;
        }
        tokens = tokens.saturating_add(cost);
        kept.push(item);
    }
    corpus.tokens_estimate = tokens;
    corpus.items = kept;
    corpus.truncated = truncated;
    Ok((corpus, digests))
}

/// Writes `analysis/<task>.md`, `analysis/overview.md` plus the corpus JSON.
pub fn write_digest(dir: &Path, corpus: &EvidenceCorpus, digests: &[TaskDigest]) -> Result<()> {
    crate::config::mkdir_private(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let analysis = dir.join("analysis");
    crate::config::mkdir_private(&analysis).with_context(|| format!("cannot create {}", analysis.display()))?;
    for digest in digests {
        let path = analysis.join(format!("{}.md", crate::evolve::store::safe_name(&digest.task_id)));
        crate::config::atomic_write(&path, render_task_markdown(digest).as_bytes()).with_context(|| format!("cannot write {}", path.display()))?;
    }
    let path = analysis.join("overview.md");
    crate::config::atomic_write(&path, render_overview(corpus, digests).as_bytes()).with_context(|| format!("cannot write {}", path.display()))?;
    let body = serde_json::to_string_pretty(corpus).context("cannot encode evidence corpus")?;
    let path = dir.join("corpus.json");
    crate::config::atomic_write(&path, format!("{body}\n").as_bytes()).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Agent-facing rendering of the corpus: bounded, ranked, with session drill-down ids.
pub fn corpus_prompt(corpus: &EvidenceCorpus, max_items: usize, max_tokens: u64) -> String {
    let item_limit = if max_items == 0 { usize::MAX } else { max_items };
    let token_budget = if max_tokens == 0 { u64::MAX } else { max_tokens };
    if corpus.items.is_empty() {
        return "## Failure evidence\n\nNo failure evidence recorded yet.\n".to_string();
    }
    let mut out = String::new();
    out.push_str("## Failure evidence\n\n");
    out.push_str(&format!(
        "sessions: {} scanned / {} skipped; occurrences: {}; items: {}{}\n\n",
        corpus.sessions_scanned,
        corpus.sessions_skipped,
        corpus.total_occurrences(),
        corpus.items.len(),
        if corpus.truncated { " (truncated)" } else { "" },
    ));
    let mut used = estimate_tokens(&out);
    let mut emitted = 0usize;
    for item in corpus.top(item_limit) {
        let block = render_item(item);
        let cost = estimate_tokens(&block);
        if used.saturating_add(cost) > token_budget {
            break;
        }
        out.push_str(&block);
        used = used.saturating_add(cost);
        emitted += 1;
    }
    if emitted == 0 {
        out.push_str("(no evidence item fits the token budget)\n");
    }
    if emitted < corpus.items.len() {
        out.push_str("\n(truncated: request more budget for the full corpus)\n");
    }
    out
}

/// Default session directory for a data dir.
pub fn sessions_dir_for(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// Convenience: digest into a store-owned directory.
pub fn digest_into(store: &HarnessStore, sessions_dir: &Path, opts: &DigestOptions) -> Result<(EvidenceCorpus, Vec<TaskDigest>)> {
    let (corpus, digests) = digest_sessions(sessions_dir, opts)?;
    store.ensure()?;
    write_digest(&store.digest_dir(), &corpus, &digests)?;
    Ok((corpus, digests))
}

/// L2 cluster: one normalized failure class with the evidence that produced it.
struct Cluster {
    signal: FailureSignal,
    signature: String,
    occurrences: usize,
    tasks: BTreeSet<String>,
    sessions: BTreeSet<String>,
    excerpt: String,
    component_hint: Option<String>,
}

impl Cluster {
    fn new(signal: FailureSignal, signature: String) -> Self {
        Self { signal, signature, occurrences: 0, tasks: BTreeSet::new(), sessions: BTreeSet::new(), excerpt: String::new(), component_hint: None }
    }
}

/// Working set for one task inside one session (folded into `TaskDigest` at the end).
#[derive(Default)]
struct TaskAccumulator {
    session_id: String,
    steps: u32,
    tool_calls: u32,
    tokens: u64,
    tool_errors: Vec<String>,
    verify_issues: Vec<String>,
    key_events: Vec<String>,
    failure: Option<FailureSignal>,
    completed_ok: bool,
    verify_passed: bool,
    verify_failed: bool,
}

impl TaskAccumulator {
    fn new(session_id: &str) -> Self {
        Self { session_id: session_id.to_string(), ..Default::default() }
    }

    fn finish(self, task_id: &str) -> TaskDigest {
        // A failed verification is the task-level verdict: it outranks intermediate tool errors.
        let mut failure = if self.verify_failed { Some(FailureSignal::VerifyFail) } else { self.failure };
        let passed = failure.is_none() && (self.completed_ok || self.verify_passed);
        // A run that ended without any completion marker still failed; without this the session
        // would be counted but contribute no evidence, which is how a real failure goes missing.
        if !passed && failure.is_none() {
            failure = Some(FailureSignal::Other("task ended without a completion marker".to_string()));
        }
        let mut key_events = self.key_events;
        if self.tool_calls > 0 {
            // `TaskDigest` has no dedicated counter; the L1 tool-call count rides here.
            key_events.push(format!("tool_calls={}", self.tool_calls));
        }
        let steps = if self.steps > 0 { self.steps } else { self.tool_calls };
        TaskDigest {
            task_id: task_id.to_string(),
            session_id: self.session_id,
            passed,
            failure,
            steps,
            tool_errors: self.tool_errors,
            verify_issues: self.verify_issues,
            key_events,
            tokens: self.tokens,
        }
    }
}

fn scan_session(envelopes: &[EventEnvelope], clusters: &mut BTreeMap<(String, String), Cluster>, digests: &mut BTreeMap<String, TaskDigest>) {
    let session_id = envelopes.first().map(|envelope| envelope.session_id.clone()).unwrap_or_default();
    let mut accumulators: BTreeMap<String, TaskAccumulator> = BTreeMap::new();
    let mut current = session_id.clone();
    for envelope in envelopes {
        let event = &envelope.event;
        let kind = if envelope.kind.is_empty() { event.get("type").and_then(Value::as_str).unwrap_or_default() } else { envelope.kind.as_str() };
        if kind == "task_started" || kind == "task_start" {
            if let Some(task_id) = task_id_from_event(event) {
                current = task_id;
            }
        }
        let accumulator = accumulators.entry(current.clone()).or_insert_with(|| TaskAccumulator::new(&session_id));
        match kind {
            "task_started" => accumulator.key_events.push("task_started".to_string()),
            "task_start" => accumulator.key_events.push("task_start".to_string()),
            "usage" => {
                accumulator.steps = accumulator.steps.saturating_add(1);
                accumulator.tokens = accumulator.tokens.saturating_add(usage_tokens(event));
            }
            "tool" => match event.get("state").and_then(Value::as_str).unwrap_or_default() {
                "start" => accumulator.tool_calls = accumulator.tool_calls.saturating_add(1),
                "error" => {
                    let raw = event_text(event).unwrap_or_else(|| {
                        let name = event.get("name").and_then(Value::as_str).unwrap_or("tool");
                        format!("{name} failed")
                    });
                    accumulator.tool_errors.push(raw.clone());
                    let signal = record_failure(clusters, &raw, &current, &session_id);
                    if accumulator.failure.is_none() {
                        accumulator.failure = Some(signal);
                    }
                }
                _ => {}
            },
            "verify_result" => {
                let status = event.get("status").and_then(Value::as_str).unwrap_or_default();
                let reason = event.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                match status {
                    "pass" => accumulator.verify_passed = true,
                    "fail" => {
                        accumulator.verify_failed = true;
                        let mut texts = issue_texts(event.get("errors"));
                        if texts.is_empty() {
                            texts.push(if reason.is_empty() { "verify failed".to_string() } else { reason });
                        }
                        for text in texts {
                            accumulator.verify_issues.push(text.clone());
                            let signal = record_failure(clusters, &text, &current, &session_id);
                            if accumulator.failure.is_none() {
                                accumulator.failure = Some(signal);
                            }
                        }
                    }
                    "warn" => accumulator.verify_issues.extend(issue_texts(event.get("errors"))),
                    _ => {
                        if !reason.is_empty() {
                            accumulator.key_events.push(format!("verify {status}: {reason}"));
                        }
                    }
                }
            }
            "task_completed" => {
                let outcome = event.get("outcome").and_then(Value::as_str).or_else(|| event.get("status").and_then(Value::as_str)).unwrap_or_default();
                let lower = outcome.to_ascii_lowercase();
                if lower.is_empty() || matches!(lower.as_str(), "done" | "completed" | "complete" | "success" | "ok" | "pass" | "passed") {
                    accumulator.completed_ok = true;
                    accumulator.key_events.push("task_completed".to_string());
                } else {
                    // The outcome marker duplicates the real cause (tool error, verify fail),
                    // so it annotates the digest instead of opening its own cluster.
                    let raw = event_text(event).unwrap_or_else(|| format!("task completed with outcome {outcome}"));
                    accumulator.key_events.push(format!("task_completed: {outcome}"));
                    if accumulator.failure.is_none() {
                        accumulator.failure = Some(FailureSignal::classify(&raw));
                    }
                }
            }
            "error" | "abort" | "fatal" => {
                let raw = event_text(event).unwrap_or_else(|| format!("{kind} event"));
                accumulator.key_events.push(format!("{kind}: {}", excerpt(&raw)));
                let signal = record_failure(clusters, &raw, &current, &session_id);
                if accumulator.failure.is_none() {
                    accumulator.failure = Some(signal);
                }
            }
            "done" => accumulator.key_events.push("done".to_string()),
            _ => {}
        }
    }
    for (task_id, accumulator) in accumulators {
        let digest = accumulator.finish(&task_id);
        // Ensure every failing task leaves a cluster even when the failure was inferred at finish
        // time (missing completion marker) rather than recorded from a single event.
        if let Some(signal) = digest.failure.as_ref() {
            let text = match signal {
                crate::evolve::types::FailureSignal::Other(raw) => raw.clone(),
                other => other.as_str().to_string(),
            };
            let has_cluster = clusters.keys().any(|(kind, _)| kind == signal.as_str());
            if !has_cluster {
                record_failure(clusters, &text, &task_id, &session_id);
            }
        }
        match digests.get_mut(&task_id) {
            Some(existing) => merge_digest(existing, digest),
            None => {
                digests.insert(task_id, digest);
            }
        }
    }
}

fn merge_digest(target: &mut TaskDigest, incoming: TaskDigest) {
    target.steps = target.steps.saturating_add(incoming.steps);
    target.tokens = target.tokens.saturating_add(incoming.tokens);
    target.passed = target.passed && incoming.passed;
    if target.failure.is_none() {
        target.failure = incoming.failure;
    }
    if target.session_id.is_empty() {
        target.session_id = incoming.session_id;
    }
    target.tool_errors.extend(incoming.tool_errors);
    target.tool_errors.sort();
    target.tool_errors.dedup();
    target.verify_issues.extend(incoming.verify_issues);
    target.verify_issues.sort();
    target.verify_issues.dedup();
    target.key_events.extend(incoming.key_events);
    target.key_events.sort();
    target.key_events.dedup();
}

/// Records one normalized failure into the L2 cluster map and returns its signal class.
fn record_failure(clusters: &mut BTreeMap<(String, String), Cluster>, raw: &str, task_id: &str, session_id: &str) -> FailureSignal {
    let signal = FailureSignal::classify(raw);
    let mut signature = normalize_signature(raw);
    if signature.is_empty() {
        signature = short_hash(raw.as_bytes());
    }
    let key = (signal.as_str().to_string(), signature.clone());
    let cluster = clusters.entry(key).or_insert_with(|| Cluster::new(signal.clone(), signature));
    cluster.occurrences = cluster.occurrences.saturating_add(1);
    if !task_id.is_empty() {
        cluster.tasks.insert(task_id.to_string());
    }
    if !session_id.is_empty() {
        cluster.sessions.insert(session_id.to_string());
    }
    if cluster.excerpt.is_empty() {
        cluster.excerpt = excerpt(raw);
    }
    if cluster.component_hint.is_none() {
        cluster.component_hint = component_hint_for(raw);
    }
    cluster.signal.clone()
}

fn cluster_item(cluster: Cluster) -> EvidenceItem {
    EvidenceItem {
        id: short_hash(format!("{}:{}", cluster.signal.as_str(), cluster.signature).as_bytes()),
        tier: 3,
        signature: cluster.signature,
        component_hint: cluster.component_hint,
        signal: cluster.signal,
        occurrences: cluster.occurrences,
        tasks: cluster.tasks.into_iter().collect(),
        sessions: cluster.sessions.into_iter().collect(),
        excerpt: cluster.excerpt,
    }
}

fn item_cost(item: &EvidenceItem) -> u64 {
    estimate_tokens(&item.signature) + estimate_tokens(&item.excerpt) + 16 + (item.tasks.len() as u64 + item.sessions.len() as u64) * 4
}

/// Suggests the bundle component most likely responsible for a failure.
fn component_hint_for(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let table: [(&str, &str); 15] = [
        ("context length", CONTEXT_POLICY_PATH),
        ("context window", CONTEXT_POLICY_PATH),
        ("compaction", CONTEXT_POLICY_PATH),
        ("budget", JIT_POLICY_PATH),
        ("jit", JIT_POLICY_PATH),
        ("verify", VERIFY_POLICY_PATH),
        ("typecheck", VERIFY_POLICY_PATH),
        ("lint", VERIFY_POLICY_PATH),
        ("test failed", VERIFY_POLICY_PATH),
        ("memory", MEMORY_PATH),
        ("middleware", MIDDLEWARE_PATH),
        ("hook", MIDDLEWARE_PATH),
        ("skill", SKILLS_DIR),
        ("subagent", SUBAGENTS_DIR),
        ("delegat", SUBAGENTS_DIR),
    ];
    for (needle, path) in table {
        if lower.contains(needle) {
            return Some(path.to_string());
        }
    }
    if lower.contains("prompt") || lower.contains("system.md") {
        return Some(SYSTEM_PROMPT_PATH.to_string());
    }
    if lower.contains("tool") || lower.contains("argument") || lower.contains("schema") {
        return Some(TOOL_GUIDANCE_PATH.to_string());
    }
    None
}

fn render_item(item: &EvidenceItem) -> String {
    let mut out = String::new();
    out.push_str(&format!("### {} x{} - {}\n", item.signal.as_str(), item.occurrences, item.signature));
    if let Some(hint) = &item.component_hint {
        out.push_str(&format!("- component: `{hint}`\n"));
    }
    if !item.tasks.is_empty() {
        out.push_str(&format!("- tasks: {}\n", joined(&item.tasks, PROMPT_TASKS)));
    }
    if !item.sessions.is_empty() {
        // Session ids are the drill-down handle: `goon sessions show <id>`.
        out.push_str(&format!("- sessions: {}\n", joined(&item.sessions, PROMPT_SESSIONS)));
    }
    if !item.excerpt.is_empty() {
        out.push_str(&format!("- excerpt: {}\n", item.excerpt));
    }
    out.push('\n');
    out
}

fn joined(values: &[String], limit: usize) -> String {
    let mut shown: Vec<&str> = values.iter().take(limit).map(String::as_str).collect();
    if values.len() > limit {
        shown.push("...");
    }
    shown.join(", ")
}

fn render_task_markdown(digest: &TaskDigest) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Task `{}`\n\n", digest.task_id));
    out.push_str(&format!("- session: `{}`\n", digest.session_id));
    out.push_str(&format!("- passed: {}\n", digest.passed));
    out.push_str(&format!("- failure: {}\n", digest.failure.as_ref().map(FailureSignal::as_str).unwrap_or("none")));
    out.push_str(&format!("- steps: {}\n- tokens: {}\n", digest.steps, digest.tokens));
    push_details(&mut out, "Tool errors", &digest.tool_errors);
    push_details(&mut out, "Verify issues", &digest.verify_issues);
    push_details(&mut out, "Key events", &digest.key_events);
    out
}

fn push_details(out: &mut String, title: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title}\n\n"));
    for value in values.iter().take(DETAIL_LIMIT) {
        out.push_str(&format!("- {value}\n"));
    }
    if values.len() > DETAIL_LIMIT {
        out.push_str(&format!("- (+{} more)\n", values.len() - DETAIL_LIMIT));
    }
}

fn render_overview(corpus: &EvidenceCorpus, digests: &[TaskDigest]) -> String {
    let passed = digests.iter().filter(|digest| digest.passed).count();
    let mut out = String::new();
    out.push_str("# Experience digest\n\n");
    out.push_str(&format!("- generated: {}\n", corpus.created));
    out.push_str(&format!("- sessions: {} scanned, {} skipped\n", corpus.sessions_scanned, corpus.sessions_skipped));
    out.push_str(&format!("- tasks: {} ({} passed)\n", digests.len(), passed));
    out.push_str(&format!("- occurrences: {}\n", corpus.total_occurrences()));
    out.push_str(&format!("- evidence items: {}\n", corpus.items.len()));
    out.push_str(&format!("- tokens: {}\n", corpus.tokens_estimate));
    out.push_str(&format!("- truncated: {}\n", corpus.truncated));
    if !corpus.items.is_empty() {
        out.push_str("\n## Top evidence\n\n| signal | occurrences | component | signature |\n| --- | --- | --- | --- |\n");
        for item in corpus.top(10) {
            out.push_str(&format!("| {} | {} | {} | `{}` |\n", item.signal.as_str(), item.occurrences, item.component_hint.as_deref().unwrap_or("-"), item.signature.replace('|', "/")));
        }
    }
    if !digests.is_empty() {
        out.push_str("\n## Tasks\n\n| task | passed | steps | tokens | failure |\n| --- | --- | --- | --- | --- |\n");
        for digest in digests.iter().take(DETAIL_LIMIT) {
            out.push_str(&format!("| `{}` | {} | {} | {} | {} |\n", digest.task_id, digest.passed, digest.steps, digest.tokens, digest.failure.as_ref().map(FailureSignal::as_str).unwrap_or("-"),));
        }
    }
    out
}

fn event_log_paths(sessions_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(anyhow::Error::new(error).context(format!("cannot list {}", sessions_dir.display()))),
    };
    let mut logs: Vec<(u128, String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read an entry of {}", sessions_dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
        if !name.ends_with(".events.jsonl") {
            continue;
        }
        let modified = entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|delta| delta.as_millis()).unwrap_or(0);
        logs.push((modified, name, path));
    }
    // Newest first so `max_sessions` keeps the freshest evidence; the name breaks ties deterministically.
    logs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(logs.into_iter().map(|(_, _, path)| path).collect())
}

fn task_id_from_event(event: &Value) -> Option<String> {
    for key in ["taskId", "task_id"] {
        match event.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => return Some(text.trim().to_string()),
            Some(Value::Number(number)) => return Some(number.to_string()),
            _ => {}
        }
    }
    None
}

fn usage_tokens(event: &Value) -> u64 {
    if let Some(total) = event.get("total").and_then(Value::as_u64) {
        return total;
    }
    event.get("input").and_then(Value::as_u64).unwrap_or(0).saturating_add(event.get("output").and_then(Value::as_u64).unwrap_or(0))
}

fn event_text(event: &Value) -> Option<String> {
    if let Some(text) = text_field(event) {
        return Some(text);
    }
    event.get("meta").and_then(text_field)
}

fn text_field(value: &Value) -> Option<String> {
    for key in ["message", "reason", "detail", "summary", "error", "outputPreview"] {
        if let Some(Value::String(text)) = value.get(key) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn issue_texts(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else { return Vec::new() };
    let mut texts = Vec::new();
    for item in items {
        match item {
            Value::String(text) if !text.trim().is_empty() => texts.push(text.trim().to_string()),
            Value::Object(map) => {
                for key in ["message", "reason", "detail"] {
                    if let Some(Value::String(text)) = map.get(key) {
                        if !text.trim().is_empty() {
                            texts.push(text.trim().to_string());
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    texts
}

fn excerpt(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= EXCERPT_LIMIT {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(EXCERPT_LIMIT).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(dir: &Path, session: &str, events: &[Value]) -> PathBuf {
        let mut body = String::new();
        for (index, event) in events.iter().enumerate() {
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("event");
            let envelope = serde_json::json!({
                "v": 1,
                "seq": (index + 1) as u64,
                "at": "2026-01-01T00:00:00Z",
                "sessionId": session,
                "type": kind,
                "event": event,
            });
            body.push_str(&serde_json::to_string(&envelope).unwrap());
            body.push('\n');
        }
        let path = dir.join(format!("{session}.events.jsonl"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn failing_events(task_id: &str, expected: u32) -> Vec<Value> {
        vec![
            serde_json::json!({"type": "task_started", "taskId": task_id, "task": "fix the build"}),
            serde_json::json!({"type": "tool", "name": "read_file", "state": "start", "meta": {"summary": "read src/lib.rs"}}),
            serde_json::json!({"type": "usage", "input": 100, "output": 20, "total": 120}),
            serde_json::json!({"type": "tool", "name": "edit_file", "state": "error", "meta": {"summary": "edit_file failed: no such file src/missing.rs"}}),
            serde_json::json!({"type": "verify_result", "status": "fail", "reason": "cargo test failed", "errors": [{"message": format!("test failed: expected {expected} got 3")}], "warnings": []}),
            serde_json::json!({"type": "task_completed", "outcome": "fail"}),
        ]
    }

    fn passing_events(task_id: &str) -> Vec<Value> {
        vec![
            serde_json::json!({"type": "task_started", "taskId": task_id, "task": "add a flag"}),
            serde_json::json!({"type": "usage", "input": 50, "output": 10, "total": 60}),
            serde_json::json!({"type": "tool", "name": "read_file", "state": "start", "meta": {"summary": "read"}}),
            serde_json::json!({"type": "verify_result", "status": "pass", "reason": "ok", "errors": [], "warnings": []}),
            serde_json::json!({"type": "task_completed", "outcome": "done"}),
        ]
    }

    #[test]
    fn digests_one_session_into_l1_and_l3() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), "sess-a", &failing_events("task-a", 2));
        let (corpus, digests) = digest_sessions(dir.path(), &DigestOptions::default()).unwrap();
        assert_eq!(corpus.sessions_scanned, 1);
        assert_eq!(corpus.sessions_skipped, 0);
        assert_eq!(digests.len(), 1);
        let digest = &digests[0];
        assert_eq!(digest.task_id, "task-a");
        assert_eq!(digest.session_id, "sess-a");
        assert!(!digest.passed);
        assert_eq!(digest.failure, Some(FailureSignal::VerifyFail));
        assert_eq!(digest.steps, 1);
        assert_eq!(digest.tokens, 120);
        assert_eq!(digest.tool_errors.len(), 1);
        assert!(digest.verify_issues.iter().any(|issue| issue.contains("test failed")));
        assert!(digest.key_events.iter().any(|event| event == "tool_calls=1"));
        assert!(!corpus.items.is_empty());
        assert!(corpus.total_occurrences() >= 2);
        let verify_item = corpus.items.iter().find(|item| item.signal == FailureSignal::VerifyFail).unwrap();
        assert_eq!(verify_item.tier, 3);
        assert!(!verify_item.excerpt.is_empty());
        assert!(verify_item.sessions.contains(&"sess-a".to_string()));
    }

    #[test]
    fn sessions_without_failures_pass_and_cluster_by_signature() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), "sess-ok", &passing_events("task-ok"));
        write_log(dir.path(), "sess-1", &failing_events("task-1", 2));
        write_log(dir.path(), "sess-2", &failing_events("task-2", 7));
        let (corpus, digests) = digest_sessions(dir.path(), &DigestOptions::default()).unwrap();
        assert_eq!(corpus.sessions_scanned, 3);
        let passing = digests.iter().find(|digest| digest.task_id == "task-ok").unwrap();
        assert!(passing.passed);
        assert!(passing.failure.is_none());

        // Volatile numbers collapse: one cluster, two sessions, two tasks.
        let verify_items: Vec<&EvidenceItem> = corpus.items.iter().filter(|item| item.signal == FailureSignal::VerifyFail).collect();
        assert_eq!(verify_items.len(), 1, "expected one verify cluster, got {verify_items:?}");
        let item = verify_items[0];
        assert_eq!(item.occurrences, 2);
        assert_eq!(item.sessions.len(), 2);
        assert_eq!(item.tasks.len(), 2);
        assert!(item.component_hint.as_deref() == Some(VERIFY_POLICY_PATH));
        assert_eq!(item.id, short_hash(format!("verify_fail:{}", item.signature).as_bytes()));
    }

    #[test]
    fn skips_broken_logs_but_keeps_torn_tails() {
        let dir = tempfile::tempdir().unwrap();
        let torn = write_log(dir.path(), "sess-torn", &passing_events("task-ok"));
        let mut body = std::fs::read_to_string(&torn).unwrap();
        body.push_str("{\"v\":1,\"seq\":99");
        std::fs::write(&torn, body).unwrap();
        std::fs::write(dir.path().join("sess-broken.events.jsonl"), "{\"v\":1,\"seq\":1,\"at\":\"x\",\"sessionId\":\"sess-broken\",\"type\":\"usage\",\"event\":{}}\nnot json\n").unwrap();

        let (corpus, digests) = digest_sessions(dir.path(), &DigestOptions::default()).unwrap();
        assert_eq!(corpus.sessions_scanned, 1);
        assert_eq!(corpus.sessions_skipped, 1);
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].session_id, "sess-torn");
        // Missing directory is an empty digest, not an error.
        let empty = digest_sessions(&dir.path().join("nope"), &DigestOptions::default()).unwrap();
        assert!(empty.0.items.is_empty());
        assert!(empty.1.is_empty());
    }

    #[test]
    fn respects_budgets_and_marks_truncation() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), "sess-1", &failing_events("task-1", 2));
        write_log(dir.path(), "sess-2", &failing_events("task-2", 7));

        let tight_sessions = DigestOptions { max_sessions: 1, max_tasks: 0, max_tokens: 0 };
        let (corpus, _) = digest_sessions(dir.path(), &tight_sessions).unwrap();
        assert_eq!(corpus.sessions_scanned, 1);
        assert!(corpus.truncated);

        let tight_tokens = DigestOptions { max_sessions: 0, max_tasks: 0, max_tokens: 12 };
        let (corpus, _) = digest_sessions(dir.path(), &tight_tokens).unwrap();
        assert!(corpus.truncated);
        assert!(corpus.tokens_estimate <= 12);
        assert!(corpus.items.len() < 2);

        let tight_tasks = DigestOptions { max_sessions: 0, max_tasks: 1, max_tokens: 0 };
        let (corpus, digests) = digest_sessions(dir.path(), &tight_tasks).unwrap();
        assert_eq!(digests.len(), 1);
        assert!(corpus.truncated);

        let unbounded = DigestOptions { max_sessions: 0, max_tasks: 0, max_tokens: 0 };
        let (corpus, digests) = digest_sessions(dir.path(), &unbounded).unwrap();
        assert_eq!(digests.len(), 2);
        assert!(!corpus.truncated);
        assert_eq!(corpus.items.len(), 2);
    }

    #[test]
    fn corpus_prompt_is_ranked_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), "sess-1", &failing_events("task-1", 2));
        write_log(dir.path(), "sess-2", &failing_events("task-2", 7));
        write_log(dir.path(), "sess-3", &passing_events("task-ok"));
        let (corpus, digests) = digest_sessions(dir.path(), &DigestOptions::default()).unwrap();

        let full = corpus_prompt(&corpus, 10, 0);
        assert!(full.contains("sessions: 3 scanned"));
        for item in &corpus.items {
            assert!(full.contains(&item.signature), "prompt should list {}", item.signature);
            assert!(full.contains(&item.sessions[0]));
        }

        let single = corpus_prompt(&corpus, 1, 0);
        assert!(single.contains(&corpus.top(1)[0].signature));
        assert!(single.contains("truncated"));

        let starved = corpus_prompt(&corpus, 0, 6);
        assert!(starved.contains("token budget") || starved.contains("truncated"));

        let empty = EvidenceCorpus::empty();
        assert!(corpus_prompt(&empty, 5, 100).contains("No failure evidence"));

        // L2 digests render to disk with an overview and a JSON corpus.
        let out = dir.path().join("reports");
        write_digest(&out, &corpus, &digests).unwrap();
        assert!(out.join("analysis").join("overview.md").is_file());
        assert!(out.join("analysis").join("task-1.md").is_file());
        let overview = std::fs::read_to_string(out.join("analysis").join("overview.md")).unwrap();
        assert!(overview.contains("Experience digest"));
        assert!(overview.contains("Top evidence"));
        let stored: EvidenceCorpus = serde_json::from_str(&std::fs::read_to_string(out.join("corpus.json")).unwrap()).unwrap();
        assert_eq!(stored.items.len(), corpus.items.len());
    }

    #[test]
    fn digest_into_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        write_log(&sessions, "sess-1", &failing_events("task-1", 2));
        let store = HarnessStore::new(dir.path().join("state"));
        let (corpus, digests) = digest_into(&store, &sessions, &DigestOptions::default()).unwrap();
        assert_eq!(corpus.sessions_scanned, 1);
        assert_eq!(digests.len(), 1);
        let stored = store.load_evidence("corpus").unwrap();
        assert_eq!(stored.items.len(), corpus.items.len());
        assert!(store.digest_dir().join("analysis").join("overview.md").is_file());
        assert_eq!(sessions_dir_for(dir.path()), dir.path().join("sessions"));
    }
}
