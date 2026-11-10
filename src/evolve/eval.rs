//! Evaluation harness: runs the real CLI (this same binary) on a task set, with a bundle
//! applied, and produces a `RunSummary`. Stub mode keeps the loop fully offline and
//! deterministic for tests; the same code path drives a live provider.
//!
//! Judging is deliberately boring so a score cannot drift: the child must exit 0, print the
//! expected substring on stdout when the task declares one, and pass every verify command.
//! Each task runs in its own throwaway workspace under the system temp directory.
#![allow(dead_code)]

use crate::evolve::types::*;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Fallback timeout when a task carries none (mirrors the serde default).
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// `detail` is surfaced in prompts and manifests: keep it to one short line.
pub const DETAIL_MAX_CHARS: usize = 300;

/// One evaluation task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalTask {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    #[serde(default)]
    pub verify: Vec<String>,
    /// FrontierCode-style grading: blocking criteria must all pass, the rest are weighted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<Rubric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Variant>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// One graded requirement: a shell check that must exit 0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    #[serde(default = "unit_weight")]
    pub weight: f64,
    pub command: String,
}

fn unit_weight() -> f64 {
    1.0
}

/// How a task is graded, mirroring FrontierCode's two-tier rubric.
///
/// A failing blocker caps the task at zero (like a change-requested review); otherwise the score
/// is the weighted fraction of passing criteria. Without a rubric the task is pass/fail on
/// `expect` + `verify`, which is what the other eval modes use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rubric {
    /// Requirements that are so central that failing one means "not mergeable".
    #[serde(default)]
    pub blockers: Vec<Criterion>,
    /// Quality requirements that contribute weight to the final score.
    #[serde(default)]
    pub criteria: Vec<Criterion>,
}

impl Rubric {
    pub fn is_empty(&self) -> bool {
        self.blockers.is_empty() && self.criteria.is_empty()
    }

    pub fn validate(&self, task_id: &str) -> Result<()> {
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for criterion in self.blockers.iter().chain(self.criteria.iter()) {
            if criterion.id.trim().is_empty() {
                bail!("task `{task_id}` has a rubric entry with an empty id");
            }
            if !seen.insert(criterion.id.as_str()) {
                bail!("task `{task_id}` has a duplicate rubric id `{}`", criterion.id);
            }
            if criterion.command.trim().is_empty() {
                bail!("task `{task_id}` rubric `{}` has an empty command", criterion.id);
            }
            if !criterion.weight.is_finite() || criterion.weight < 0.0 {
                bail!("task `{task_id}` rubric `{}` has an invalid weight", criterion.id);
            }
        }
        Ok(())
    }

    pub fn total_weight(&self) -> f64 {
        self.criteria.iter().map(|criterion| criterion.weight).sum()
    }
}

/// Outcome of grading one task against its rubric.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RubricScore {
    /// Weighted fraction in `0..=1`; `0.0` when any blocker failed.
    pub score: f64,
    pub failed_blockers: Vec<String>,
    pub passed_criteria: Vec<String>,
    pub failed_criteria: Vec<String>,
}

impl RubricScore {
    pub fn passed(&self) -> bool {
        self.failed_blockers.is_empty()
    }
}

/// A split's worth of tasks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalSet {
    pub split: Split,
    pub tasks: Vec<EvalTask>,
}

impl Default for EvalSet {
    /// `Split` has no `Default` in the frozen contract, so an empty train set is the neutral value.
    fn default() -> Self {
        Self { split: Split::Train, tasks: vec![] }
    }
}

/// Executes one eval pass.
#[derive(Clone, Debug)]
pub struct EvalOptions {
    pub goon_bin: PathBuf,
    /// Workspace the eval set belongs to (the run itself happens in a per-task temp dir).
    pub workspace: PathBuf,
    pub bundle: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub stub: bool,
    pub max_tasks: usize,
    pub timeout_ms: u64,
    pub variant: Option<Variant>,
    /// How many tasks run at once (clamped to 1..=8).
    pub concurrency: u32,
    /// Extra attempts for infrastructural failures (timeout / unparsable output).
    pub retries: u32,
}

/// Loads `eval/tasks.json` (`{"split": {"tasks": [...]}}` or `{"tasks": [...]}`).
pub fn load(path: &Path, split: Split) -> Result<EvalSet> {
    let body = std::fs::read_to_string(path).map_err(|error| anyhow!("read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&body).map_err(|error| anyhow!("{}: invalid JSON ({error})", path.display()))?;
    let object = value.as_object().ok_or_else(|| anyhow!("{}: expected a JSON object", path.display()))?;

    let mut selected = split;
    let mut tasks_value = object.get("tasks");
    match object.get("split") {
        Some(Value::Object(inner)) => {
            if let Some(name) = inner.get("split").and_then(Value::as_str) {
                selected = parse_split(name, path)?;
            }
            if tasks_value.is_none() {
                tasks_value = inner.get("tasks");
            }
        }
        Some(Value::String(name)) => selected = parse_split(name, path)?,
        Some(_) => return Err(anyhow!("{}: `split` must be a string or an object", path.display())),
        None => {}
    }

    let tasks_value = tasks_value.ok_or_else(|| anyhow!("{}: eval set must contain a tasks array", path.display()))?;
    let tasks: Vec<EvalTask> = serde_json::from_value(tasks_value.clone()).map_err(|error| anyhow!("{}: invalid tasks ({error})", path.display()))?;

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, task) in tasks.iter().enumerate() {
        let id = task.id.trim();
        if id.is_empty() {
            return Err(anyhow!("{}: task at index {index} has an empty id", path.display()));
        }
        if !seen.insert(id.to_string()) {
            return Err(anyhow!("{}: duplicate task id `{id}`", path.display()));
        }
        if task.prompt.trim().is_empty() {
            return Err(anyhow!("{}: task `{id}` has an empty prompt", path.display()));
        }
        if let Some(rubric) = &task.rubric {
            rubric.validate(id)?;
        }
    }
    Ok(EvalSet { split: selected, tasks })
}

fn parse_split(raw: &str, path: &Path) -> Result<Split> {
    let normalized = raw.trim().to_ascii_lowercase();
    Split::parse(&normalized).ok_or_else(|| anyhow!("{}: unknown split `{raw}`", path.display()))
}

/// Writes a runnable starter task set.
pub fn write_template(path: &Path) -> Result<()> {
    let template = serde_json::json!({
        "split": "train",
        "tasks": [
            {
                "id": "edit-readme",
                "prompt": "Rewrite README.md so it contains the single line: hello goon.",
                "files": {"README.md": "old text\n"},
                "verify": ["echo ok"]
            },
            {
                "id": "explain-sample",
                "prompt": "Explain in one short sentence what src/sample.txt contains.",
                "files": {"src/sample.txt": "alpha\n"},
                "variant": "research"
            },
            {
                "id": "report-tree",
                "prompt": "List the workspace files and report how many there are.",
                "files": {"data/notes.txt": "one\ntwo\n"},
                "verify": ["echo ok"],
                "timeout_ms": 60000
            }
        ]
    });
    let body = serde_json::to_string_pretty(&template)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| anyhow!("create {}: {error}", parent.display()))?;
        }
    }
    std::fs::write(path, format!("{body}\n")).map_err(|error| anyhow!("write {}: {error}", path.display()))?;
    Ok(())
}

/// Runs all tasks concurrently (bounded by `EvalOptions::concurrency`), judging each by exit
/// status, expected substring and verify commands.
///
/// Order of `outcomes` matches the task list so two runs of the same set are comparable.
pub async fn run(set: &EvalSet, opts: &EvalOptions) -> Result<RunSummary> {
    use futures_util::stream::{self, StreamExt};
    let limit = if opts.max_tasks == 0 { set.tasks.len() } else { opts.max_tasks.min(set.tasks.len()) };
    let concurrency = opts.concurrency.clamp(1, 8) as usize;
    let selected: Vec<&EvalTask> = set.tasks.iter().take(limit).collect();
    let mut outcomes: Vec<(usize, TaskOutcome)> =
        stream::iter(selected.into_iter().enumerate()).map(|(index, task)| async move { (index, run_task_with_retry(task, opts).await) }).buffer_unordered(concurrency).collect().await;
    outcomes.sort_by_key(|(index, _)| *index);
    Ok(RunSummary::new(set.split, opts.bundle.clone(), opts.variant, outcomes.into_iter().map(|(_, outcome)| outcome).collect()))
}

/// Retries a task once when the failure is infrastructural (timeout or unparsable output) rather
/// than a real wrong answer: a flaky model run should not silently change a round's score.
async fn run_task_with_retry(task: &EvalTask, opts: &EvalOptions) -> TaskOutcome {
    let first = run_task(task, opts).await;
    if first.passed || opts.retries == 0 || !retryable(first.failure.as_ref()) {
        return first;
    }
    let mut second = run_task(task, opts).await;
    // Keep the better result: a retry can only rescue a flaky run, never hide a real failure.
    if second.passed {
        second.detail = None;
        return second;
    }
    second.duration_ms = first.duration_ms.saturating_add(second.duration_ms);
    second
}

/// Only infrastructure failures are retryable; a wrong answer or a failed verify is the result.
fn retryable(failure: Option<&FailureSignal>) -> bool {
    matches!(failure, Some(FailureSignal::Timeout) | Some(FailureSignal::ParseError) | Some(FailureSignal::ContextOverflow))
}

/// Judge one task result (public so tests and engine agree on the rule).
pub fn passed(exit_ok: bool, expect: Option<&str>, stdout: &str, verify_ok: bool) -> bool {
    if !exit_ok || !verify_ok {
        return false;
    }
    match expect {
        Some(needle) => needle.is_empty() || stdout.contains(needle),
        None => true,
    }
}

/// What one judged task produced before it is turned into a `TaskOutcome`.
struct Judgement {
    passed: bool,
    failure: Option<FailureSignal>,
    detail: Option<String>,
    tokens: u64,
    session_id: Option<String>,
    /// Weighted rubric score in `0..=1` when the task carries a rubric.
    score: Option<f64>,
}

/// Fair-internet-use detection, mirroring FrontierCode's second safeguard.
///
/// A run is flagged when its shell commands touch a solution-bearing source: an upstream commit
/// or PR diff, a raw file from the project's own repository, or a package install of the project
/// under test. Documentation lookups and search queries are deliberately allowed, because the
/// benchmark is about engineering work, not about pretending the internet does not exist.
pub fn unfair_internet_use(command: &str, project: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let project = project.trim().to_ascii_lowercase();
    let reads_diff = lower.contains("/pull/")
        || lower.contains("refs/pull/")
        || lower.contains("/commit/")
        || lower.contains("patch-diff")
        || lower.contains(".diff?")
        || lower.ends_with(".diff")
        || lower.contains(".diff ")
        || lower.contains(".patch")
        || (lower.contains("gh pr") && lower.contains("checkout"));
    let on_github = lower.contains("github.com") || lower.contains("gitlab.com") || lower.contains("raw.githubusercontent");
    let pulls_solution = project.len() >= 3
        && lower.contains(&project)
        && (lower.contains("git clone") || lower.contains("npm install") || lower.contains("pip install") || lower.contains("cargo add") || lower.contains("yarn add"));
    // A pull-request ref or a diff URL is solution-bearing on its own: `refs/pull/9/head` needs
    // no host name to be the upstream fix.
    let decisive = lower.contains("refs/pull/") || (lower.contains("gh pr") && lower.contains("checkout"));
    (reads_diff && (on_github || lower.contains("http") || lower.contains("url"))) || decisive || pulls_solution
}

/// Scans the child's own event log for a flagged shell command.
fn scan_fair_use(data_base: Option<&Path>, before: &std::collections::BTreeSet<String>, project: &str) -> Option<String> {
    let dir = match data_base {
        Some(base) => base.join("goon").join("sessions"),
        None => crate::config::data_dir().join("sessions"),
    };
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".events.jsonl") || before.contains(&name) {
            continue;
        }
        let Ok(log) = crate::eventlog::read_event_log(&entry.path()) else { continue };
        for envelope in &log.envelopes {
            let event = &envelope.event;
            if event.get("type").and_then(serde_json::Value::as_str) != Some("tool") {
                continue;
            }
            if event.get("name").and_then(serde_json::Value::as_str) != Some("bash") {
                continue;
            }
            let summary = event.get("meta").and_then(|meta| meta.get("summary")).and_then(serde_json::Value::as_str).unwrap_or("");
            if unfair_internet_use(summary, project) {
                return Some(summary.to_string());
            }
        }
    }
    None
}
async fn grade(rubric: &Rubric, root: &Path, timeout_ms: u64) -> Result<RubricScore> {
    let mut score = RubricScore::default();
    for blocker in &rubric.blockers {
        if !verify_one(&blocker.command, root, timeout_ms).await? {
            score.failed_blockers.push(blocker.id.clone());
        }
    }
    let total = rubric.total_weight();
    let mut earned = 0.0;
    for criterion in &rubric.criteria {
        if verify_one(&criterion.command, root, timeout_ms).await? {
            earned += criterion.weight;
            score.passed_criteria.push(criterion.id.clone());
        } else {
            score.failed_criteria.push(criterion.id.clone());
        }
    }
    // A blocker failure is a change-requested review: the weighted score does not matter.
    score.score = if !score.failed_blockers.is_empty() {
        0.0
    } else if total > 0.0 {
        (earned / total).clamp(0.0, 1.0)
    } else {
        1.0
    };
    Ok(score)
}

async fn run_task(task: &EvalTask, opts: &EvalOptions) -> TaskOutcome {
    let started = Instant::now();
    let root = std::env::temp_dir().join(format!("goon-eval-{}", uuid::Uuid::new_v4()));
    let judgement = judge_task(task, opts, &root).await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let _ = std::fs::remove_dir_all(&root);
    match judgement {
        Ok(judgement) => TaskOutcome {
            task_id: task.id.clone(),
            passed: judgement.passed,
            // A rubric score is the graded value; pass/fail tasks keep the 0/1 convention.
            score: judgement.score.unwrap_or(if judgement.passed { 1.0 } else { 0.0 }),
            tokens: judgement.tokens,
            duration_ms,
            failure: judgement.failure,
            session_id: judgement.session_id,
            detail: judgement.detail,
        },
        Err(error) => {
            let message = error.to_string();
            TaskOutcome {
                task_id: task.id.clone(),
                passed: false,
                score: 0.0,
                tokens: 0,
                duration_ms,
                failure: Some(FailureSignal::classify(&message)),
                session_id: None,
                detail: Some(truncate_chars(&message, DETAIL_MAX_CHARS)),
            }
        }
    }
}

async fn judge_task(task: &EvalTask, opts: &EvalOptions, root: &Path) -> Result<Judgement> {
    std::fs::create_dir_all(root).map_err(|error| anyhow!("create {}: {error}", root.display()))?;
    write_task_files(root, &task.files)?;
    let timeout_ms = effective_timeout(task, opts);

    let mut command = goon_command(&opts.goon_bin);
    command
        .arg("--yes")
        // Experience observability reads event logs, so every eval run must write one.
        .arg("--event-log")
        .arg(format!("--prompt={}", task.prompt))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GOON_TASK_ID", task.id.trim());
    if opts.stub {
        command.arg("--stub");
    }
    if let Some(bundle) = opts.bundle.as_deref().filter(|bundle| !bundle.is_empty()) {
        command.env("GOON_HARNESS_BUNDLE", bundle);
    }
    if let Some(data_dir) = opts.data_dir.as_deref() {
        command.env("GOON_DATA_DIR", data_dir);
    }

    let sessions_before = session_logs(opts.data_dir.as_deref());

    let child = command.spawn().map_err(|error| anyhow!("spawn {}: {error}", opts.goon_bin.display()))?;
    // On timeout the future is dropped, which drops the child: `kill_on_drop` then reaps it.
    let finished = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await;
    let (exit_ok, stdout, stderr, timed_out) = match finished {
        Ok(Ok(output)) => (output.status.success(), String::from_utf8_lossy(&output.stdout).to_string(), String::from_utf8_lossy(&output.stderr).to_string(), false),
        Ok(Err(error)) => return Err(anyhow!("wait {}: {error}", opts.goon_bin.display())),
        Err(_) => (false, String::new(), String::new(), true),
    };
    // Cost accounting: the child's own event log is the only place its usage lands.
    let (tokens, session_id) = session_usage(opts.data_dir.as_deref(), &sessions_before, task.id.trim());
    // FrontierCode safeguard 2: a run that consulted a solution-bearing source scores zero.
    let flagged = scan_fair_use(opts.data_dir.as_deref(), &sessions_before, &task.id);

    let verify_ok = if timed_out { false } else { verify_all(&task.verify, root, timeout_ms).await? };
    // A rubric replaces the pass/fail verdict with a weighted score (blockers cap it at zero).
    let graded = match &task.rubric {
        Some(rubric) if !rubric.is_empty() && !timed_out => Some(grade(rubric, root, timeout_ms).await?),
        _ => None,
    };
    let ok = match &graded {
        Some(score) => score.passed() && (verify_ok || task.verify.is_empty()),
        None => passed(exit_ok, task.expect.as_deref(), &stdout, verify_ok),
    };
    let failure = if ok {
        None
    } else if timed_out {
        // A timeout is the one failure the child cannot report in its own output.
        Some(FailureSignal::Timeout)
    } else {
        Some(FailureSignal::classify(&format!("{stderr}\n{stdout}")))
    };
    let detail = if ok {
        None
    } else if timed_out {
        Some(format!("timed out after {timeout_ms} ms"))
    } else {
        detail_of(&stderr, &stdout)
    };
    if let Some(command) = flagged {
        // Zero the run: a valid answer obtained from the upstream fix is not a solve.
        return Ok(Judgement {
            passed: false,
            failure: Some(FailureSignal::Other("unfair_internet_use".to_string())),
            detail: Some(format!("flagged: consulted a solution-bearing source ({command})")),
            tokens,
            session_id,
            score: Some(0.0),
        });
    }
    Ok(Judgement { passed: ok, failure, detail, tokens, session_id, score: graded.as_ref().map(|score| score.score) })
}

/// Session log file names present before a child runs (`{data}/goon/sessions/*.events.jsonl`).
fn session_logs(data_base: Option<&Path>) -> std::collections::BTreeSet<String> {
    let dir = match data_base {
        Some(base) => base.join("goon").join("sessions"),
        None => crate::config::data_dir().join("sessions"),
    };
    let mut names = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".events.jsonl") {
                names.insert(name);
            }
        }
    }
    names
}

/// Token total and session id of the child run that appeared after `before`.
///
/// Bounded on purpose: only new logs are read, and only their `usage` events are summed.
fn session_usage(data_base: Option<&Path>, before: &std::collections::BTreeSet<String>, task_id: &str) -> (u64, Option<String>) {
    let dir = match data_base {
        Some(base) => base.join("goon").join("sessions"),
        None => crate::config::data_dir().join("sessions"),
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return (0, None) };
    let mut candidate: Option<(bool, u64, String)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".events.jsonl") || before.contains(&name) {
            continue;
        }
        let session_id = name.trim_end_matches(".events.jsonl").to_string();
        let mut tokens = 0u64;
        let mut matches_task = false;
        if let Ok(log) = crate::eventlog::read_event_log(&entry.path()) {
            for envelope in &log.envelopes {
                let event = &envelope.event;
                match event.get("type").and_then(serde_json::Value::as_str) {
                    Some("task_start") => {
                        matches_task |= event.get("taskId").and_then(serde_json::Value::as_str) == Some(task_id);
                    }
                    Some("usage") => {
                        let total = event
                            .get("total")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_else(|| event.get("input").and_then(serde_json::Value::as_u64).unwrap_or(0) + event.get("output").and_then(serde_json::Value::as_u64).unwrap_or(0));
                        tokens = tokens.saturating_add(total);
                    }
                    _ => {}
                }
            }
        }
        // A log that names this task wins; otherwise the last one seen is kept.
        let replace = match &candidate {
            None => true,
            Some((best_matches, _, _)) => matches_task && !best_matches,
        };
        if replace {
            candidate = Some((matches_task, tokens, session_id));
        }
    }
    match candidate {
        Some((_, tokens, session_id)) => (tokens, Some(session_id)),
        None => (0, None),
    }
}

fn effective_timeout(task: &EvalTask, opts: &EvalOptions) -> u64 {
    // The task author knows how long the task needs; the global value is only a fallback for
    // tasks that declare none. Clamping to the global default silently starved longer tasks.
    let task_ms = if task.timeout_ms == 0 { 0 } else { task.timeout_ms };
    if task_ms > 0 {
        return task_ms.max(1);
    }
    let opts_ms = if opts.timeout_ms == 0 { DEFAULT_TIMEOUT_MS } else { opts.timeout_ms };
    opts_ms.max(1)
}

fn detail_of(stderr: &str, stdout: &str) -> Option<String> {
    let source = if stderr.trim().is_empty() { stdout } else { stderr };
    let text = source.trim();
    if text.is_empty() {
        None
    } else {
        Some(truncate_chars(text, DETAIL_MAX_CHARS))
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        collapsed
    } else {
        collapsed.chars().take(limit).collect()
    }
}

/// Writes a task's files into the throwaway workspace, refusing paths that escape it.
fn write_task_files(root: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    for (relative, body) in files {
        let path = relative_path(relative)?;
        let target = root.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| anyhow!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, body).map_err(|error| anyhow!("write {}: {error}", target.display()))?;
    }
    Ok(())
}

fn relative_path(raw: &str) -> Result<PathBuf> {
    let normalized = raw.trim().replace('\\', "/");
    let path = Path::new(&normalized);
    if normalized.is_empty() || path.is_absolute() {
        return Err(anyhow!("eval task file path must be relative: `{raw}`"));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(anyhow!("eval task file path must stay inside the workspace: `{raw}`")),
            Component::Prefix(_) | Component::RootDir => return Err(anyhow!("eval task file path must be relative: `{raw}`")),
            _ => {}
        }
    }
    Ok(path.to_path_buf())
}

async fn verify_all(commands: &[String], root: &Path, timeout_ms: u64) -> Result<bool> {
    for command in commands.iter().map(|command| command.trim()).filter(|command| !command.is_empty()) {
        if !verify_one(command, root, timeout_ms).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn verify_one(command: &str, root: &Path, timeout_ms: u64) -> Result<bool> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = crate::process::RunOptions { cwd: root, cancel: &cancel, timeout_ms, env: None, output_limit_bytes: Some(200_000), hard_output_bytes: Some(400_000) };
    // `run_shell` keeps the command line opaque on Windows; re-quoting it as an argv element
    // mangles embedded quotes and can turn a failing verify into a passing one.
    match crate::process::run_shell(command, options).await {
        Ok(result) => Ok(result.code == 0),
        // A verify command that cannot be run simply did not pass.
        Err(_) => Ok(false),
    }
}

/// A verify command is a shell line, never an argv vector: that is the only portable contract.
#[cfg(windows)]
fn shell_command(command: &str) -> (String, Vec<String>) {
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    (shell, vec!["/C".to_string(), command.to_string()])
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> (String, Vec<String>) {
    ("sh".to_string(), vec!["-c".to_string(), command.to_string()])
}

/// Builds the child command, routing Windows script stand-ins through the command processor.
fn goon_command(bin: &Path) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let name = bin.to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".cmd") || name.ends_with(".bat") {
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            let mut command = tokio::process::Command::new(shell);
            command.arg("/C").arg(bin);
            return command;
        }
    }
    tokio::process::Command::new(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, body).expect("write file");
    }

    fn task(id: &str, prompt: &str) -> EvalTask {
        EvalTask { id: id.into(), prompt: prompt.into(), files: BTreeMap::new(), expect: None, verify: vec![], rubric: None, variant: None, timeout_ms: 30_000 }
    }

    #[test]
    fn load_accepts_both_shapes() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let flat = dir.path().join("flat.json");
        write_file(&flat, r#"{"split":"test","tasks":[{"id":"a","prompt":"do a thing"},{"id":"b","prompt":"do b"}]}"#);
        let loaded = load(&flat, Split::Train).expect("flat set");
        assert_eq!(loaded.split, Split::Test);
        assert_eq!(loaded.tasks.len(), 2);
        assert_eq!(loaded.tasks[0].id, "a");
        assert_eq!(loaded.tasks[0].timeout_ms, DEFAULT_TIMEOUT_MS);

        let nested = dir.path().join("nested.json");
        write_file(&nested, r#"{"split":{"tasks":[{"id":"dev-1","prompt":"p","files":{"src/a.txt":"x"},"expect":"ok","verify":["echo ok"],"variant":"ops"}]}}"#);
        let loaded = load(&nested, Split::Dev).expect("nested set");
        // No split name inside the object keeps the caller's default.
        assert_eq!(loaded.split, Split::Dev);
        assert_eq!(loaded.tasks[0].files.get("src/a.txt").map(String::as_str), Some("x"));
        assert_eq!(loaded.tasks[0].expect.as_deref(), Some("ok"));
        assert_eq!(loaded.tasks[0].verify, vec!["echo ok".to_string()]);
        assert_eq!(loaded.tasks[0].variant, Some(Variant::Ops));
    }

    #[test]
    fn load_rejects_bad_sets() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cases = [
            ("duplicates.json", r#"{"tasks":[{"id":"a","prompt":"p"},{"id":"a","prompt":"q"}]}"#),
            ("empty-id.json", r#"{"tasks":[{"id":"  ","prompt":"p"}]}"#),
            ("empty-prompt.json", r#"{"tasks":[{"id":"a","prompt":" "}]}"#),
            ("no-tasks.json", r#"{"split":"train"}"#),
            ("bad-split.json", r#"{"split":"nope","tasks":[]}"#),
            ("not-object.json", r#"[]"#),
            ("broken.json", "{ nope"),
        ];
        for (name, body) in cases {
            let path = dir.path().join(name);
            write_file(&path, body);
            assert!(load(&path, Split::Train).is_err(), "{name} should not load");
        }
        assert!(load(&dir.path().join("missing.json"), Split::Train).is_err());
    }

    #[test]
    fn template_is_written_and_loads() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("eval/tasks.json");
        write_template(&path).expect("write template");
        let loaded = load(&path, Split::Dev).expect("load template");
        assert_eq!(loaded.split, Split::Train);
        assert_eq!(loaded.tasks.len(), 3);
        let ids: BTreeSet<&str> = loaded.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(loaded.tasks.iter().any(|task| task.variant == Some(Variant::Research)));
        assert!(loaded.tasks.iter().all(|task| !task.prompt.trim().is_empty()));
        assert!(loaded.tasks.iter().any(|task| !task.verify.is_empty()));
        // Overwriting an existing template is allowed.
        write_template(&path).expect("rewrite template");
    }

    #[test]
    fn passed_requires_exit_expect_and_verify() {
        assert!(passed(true, None, "", true));
        assert!(passed(true, Some("ok"), "all ok", true));
        assert!(passed(true, Some(""), "anything", true));
        assert!(!passed(false, None, "ok", true));
        assert!(!passed(true, None, "ok", false));
        assert!(!passed(true, Some("absent"), "all ok", true));
        assert!(!passed(false, Some("ok"), "ok", false));
    }

    #[test]
    fn relative_paths_cannot_escape_the_workspace() {
        assert!(relative_path("src/lib.rs").is_ok());
        assert!(relative_path("src\\lib.rs").is_ok());
        assert!(relative_path("../escape.txt").is_err());
        assert!(relative_path("/etc/passwd").is_err());
        assert!(relative_path("").is_err());
        assert!(relative_path("src/../../escape.txt").is_err());
    }

    // Scripted stand-ins: the eval harness must be testable without a provider or a built CLI.
    #[cfg(unix)]
    const OK_SCRIPT: &str = "#!/bin/sh\nprintf 'marker %s\\n' \"$GOON_TASK_ID\"\n";
    #[cfg(unix)]
    const FAIL_SCRIPT: &str = "#!/bin/sh\necho 'verify failed: typecheck' >&2\nexit 1\n";
    #[cfg(unix)]
    const STUB_NAME: &str = "stub-goon.sh";
    #[cfg(unix)]
    const FAIL_VERIFY: &str = "exit 3";
    #[cfg(windows)]
    const OK_SCRIPT: &str = "@echo off\r\necho marker %GOON_TASK_ID%\r\nexit /b 0\r\n";
    #[cfg(windows)]
    const FAIL_SCRIPT: &str = "@echo off\r\necho verify failed: typecheck 1>&2\r\nexit /b 1\r\n";
    #[cfg(windows)]
    const STUB_NAME: &str = "stub-goon.cmd";
    #[cfg(windows)]
    const FAIL_VERIFY: &str = "exit /b 3";

    fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }
        path
    }

    fn options(dir: &Path, script: PathBuf) -> EvalOptions {
        EvalOptions {
            goon_bin: script,
            workspace: dir.to_path_buf(),
            bundle: Some("bundle-test".into()),
            data_dir: Some(dir.join("data")),
            stub: true,
            max_tasks: 8,
            timeout_ms: 0,
            variant: Some(Variant::Code),
            concurrency: 1,
            // Retries make a scripted test non-deterministic; the retry policy has its own test.
            retries: 0,
        }
    }

    #[tokio::test]
    async fn run_judges_a_passing_task() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let mut eval_task = task("env-task", "report the task id");
        eval_task.files.insert("notes.txt".into(), "hi".into());
        eval_task.expect = Some("marker env-task".into());
        eval_task.verify = vec!["echo ok".into()];
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task] };

        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        assert_eq!(summary.split, Split::Train);
        assert_eq!(summary.total(), 1);
        assert_eq!(summary.passed(), 1, "outcomes: {:?}", summary.outcomes);
        assert_eq!(summary.bundle.as_deref(), Some("bundle-test"));
        assert_eq!(summary.variant, Some(Variant::Code));
        let outcome = &summary.outcomes[0];
        assert_eq!(outcome.task_id, "env-task");
        assert!(outcome.passed);
        assert!(outcome.failure.is_none());
        assert!(outcome.detail.is_none());
        assert!(outcome.session_id.is_none());
        assert_eq!(outcome.tokens, 0);
        assert_eq!(outcome.score, 1.0);
    }

    #[tokio::test]
    async fn concurrency_preserves_task_order_and_task_timeout_is_not_clamped() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let names = ["a", "b", "c", "d"];
        let tasks: Vec<EvalTask> = names.iter().map(|name| task(name, "report the task id")).collect();
        let set = EvalSet { split: Split::Train, tasks };
        let mut opts = options(dir.path(), script);
        opts.concurrency = 4;
        let summary = run(&set, &opts).await.expect("run");
        let observed: Vec<&str> = summary.outcomes.iter().map(|outcome| outcome.task_id.as_str()).collect();
        assert_eq!(observed, names, "concurrent runs must still report in task order");
        assert_eq!(summary.passed(), 4);

        // A task that declares a long timeout keeps it; the global value is only a fallback.
        let mut eval_task = task("slow", "report the task id");
        eval_task.timeout_ms = 240_000;
        let mut short_cap = options(dir.path(), write_stub(dir.path(), STUB_NAME, OK_SCRIPT));
        short_cap.timeout_ms = 60_000;
        assert_eq!(effective_timeout(&eval_task, &short_cap), 240_000);
        eval_task.timeout_ms = 0;
        assert_eq!(effective_timeout(&eval_task, &short_cap), 60_000);
    }

    #[test]
    fn rubric_validates_ids_commands_and_weights() {
        let ok = Rubric { blockers: vec![Criterion { id: "b".into(), weight: 1.0, command: "true".into() }], criteria: vec![Criterion { id: "c".into(), weight: 2.0, command: "true".into() }] };
        assert!(ok.validate("t").is_ok());
        assert_eq!(ok.total_weight(), 2.0);

        let duplicate =
            Rubric { criteria: vec![Criterion { id: "c".into(), weight: 1.0, command: "true".into() }, Criterion { id: "c".into(), weight: 1.0, command: "true".into() }], blockers: vec![] };
        assert!(duplicate.validate("t").is_err());
        let empty_command = Rubric { blockers: vec![], criteria: vec![Criterion { id: "c".into(), weight: 1.0, command: "  ".into() }] };
        assert!(empty_command.validate("t").is_err());
        let bad_weight = Rubric { blockers: vec![], criteria: vec![Criterion { id: "c".into(), weight: -1.0, command: "true".into() }] };
        assert!(bad_weight.validate("t").is_err());
    }

    #[test]
    fn fair_use_allows_documentation_and_flags_solution_sources() {
        // Allowed: reading docs, searching for an error message.
        assert!(!unfair_internet_use("curl https://developer.mozilla.org/en-US/docs/Web/JavaScript", "msw"));
        assert!(!unfair_internet_use("npm view msw version", "msw"));
        // Flagged: opening the upstream PR diff, fetching a patch, installing the project under test.
        assert!(unfair_internet_use("curl https://github.com/mswjs/msw/pull/1234.diff", "msw"));
        assert!(unfair_internet_use("git fetch origin refs/pull/9/head", "msw"));
        assert!(unfair_internet_use("npm install msw@latest", "msw"));
        assert!(unfair_internet_use("pip install requests==2.0", "requests"));
    }

    #[test]
    fn mean_score_equals_pass_rate_for_pass_fail_sets() {
        let outcomes = vec![
            TaskOutcome { task_id: "a".into(), passed: true, score: 1.0, tokens: 0, duration_ms: 0, failure: None, session_id: None, detail: None },
            TaskOutcome { task_id: "b".into(), passed: false, score: 0.0, tokens: 0, duration_ms: 0, failure: None, session_id: None, detail: None },
        ];
        let run = RunSummary::new(Split::Train, None, None, outcomes);
        assert!((run.pass_rate() - run.mean_score()).abs() < 1e-9);
        // A graded set separates them: pass/fail can be 1/1 while the score is lower.
        let graded = vec![
            TaskOutcome { task_id: "a".into(), passed: true, score: 1.0, tokens: 0, duration_ms: 0, failure: None, session_id: None, detail: None },
            TaskOutcome { task_id: "b".into(), passed: true, score: 0.5, tokens: 0, duration_ms: 0, failure: None, session_id: None, detail: None },
        ];
        let run = RunSummary::new(Split::Train, None, None, graded);
        assert_eq!(run.pass_rate(), 1.0);
        assert!((run.mean_score() - 0.75).abs() < 1e-9);
    }

    #[tokio::test]
    async fn rubric_grades_weighted_criteria_and_blockers_cap_the_score() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let mut eval_task = task("graded", "report the task id");
        eval_task.rubric = Some(Rubric {
            blockers: vec![Criterion { id: "blocker".into(), weight: 1.0, command: "echo ok".into() }],
            criteria: vec![Criterion { id: "cheap".into(), weight: 1.0, command: "echo ok".into() }, Criterion { id: "expensive".into(), weight: 3.0, command: FAIL_VERIFY.into() }],
        });
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task.clone()] };
        let summary = run(&set, &options(dir.path(), script.clone())).await.expect("run");
        let outcome = &summary.outcomes[0];
        assert!(outcome.passed, "blocker passed, so the task is mergeable");
        assert!((outcome.score - 0.25).abs() < 1e-9, "1 of 4 weight passed, got {}", outcome.score);
        assert!((summary.mean_score() - 0.25).abs() < 1e-9);

        // A failing blocker caps the score at zero even when every criterion passes.
        eval_task.rubric = Some(Rubric {
            blockers: vec![Criterion { id: "blocker".into(), weight: 1.0, command: FAIL_VERIFY.into() }],
            criteria: vec![Criterion { id: "cheap".into(), weight: 1.0, command: "echo ok".into() }],
        });
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task] };
        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        assert!(!summary.outcomes[0].passed);
        assert_eq!(summary.outcomes[0].score, 0.0);
    }

    #[test]
    fn load_rejects_invalid_rubrics() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("bad.json");
        std::fs::write(&path, r#"{"split":"train","tasks":[{"id":"t","prompt":"p","rubric":{"blockers":[{"id":"a","command":"true"},{"id":"a","command":"true"}]}}]}"#).unwrap();
        let err = load(&path, Split::Train).expect_err("duplicate rubric id");
        assert!(format!("{err:#}").contains("duplicate rubric id"));
    }

    #[tokio::test]
    async fn run_judges_a_failing_task() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, FAIL_SCRIPT);
        let set = EvalSet { split: Split::Dev, tasks: vec![task("fails", "break something")] };

        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        assert_eq!(summary.split, Split::Dev);
        let outcome = &summary.outcomes[0];
        assert!(!outcome.passed);
        assert_eq!(outcome.failure, Some(FailureSignal::VerifyFail));
        assert!(outcome.detail.as_deref().unwrap_or_default().contains("verify failed"));
        assert_eq!(outcome.score, 0.0);
    }

    #[tokio::test]
    async fn retry_policy_covers_infrastructure_not_answers() {
        // Infrastructure failures are retryable; a wrong answer or failed verify is the result.
        assert!(retryable(Some(&FailureSignal::Timeout)));
        assert!(retryable(Some(&FailureSignal::ParseError)));
        assert!(retryable(Some(&FailureSignal::ContextOverflow)));
        assert!(!retryable(Some(&FailureSignal::VerifyFail)));
        assert!(!retryable(Some(&FailureSignal::ToolError)));
        assert!(!retryable(None));

        // A passing task is never retried, and retries: 0 disables the path entirely.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let mut opts = options(dir.path(), script);
        opts.retries = 1;
        let outcome = run_task_with_retry(&task("ok", "report the task id"), &opts).await;
        assert!(outcome.passed);
        assert!(outcome.duration_ms > 0);
    }

    #[tokio::test]
    async fn run_requires_verify_commands_to_pass() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let mut eval_task = task("verify-fails", "report the task id");
        eval_task.verify = vec![FAIL_VERIFY.to_string()];
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task] };

        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        let outcome = &summary.outcomes[0];
        assert!(!outcome.passed, "a failing verify command must fail the task");
        assert_eq!(outcome.score, 0.0);
    }

    #[tokio::test]
    async fn run_reports_an_unsafe_task_file_without_running_it() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let mut eval_task = task("escape", "try to write outside");
        eval_task.files.insert("../escape.txt".into(), "nope".into());
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task] };

        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        let outcome = &summary.outcomes[0];
        assert!(!outcome.passed);
        assert!(outcome.detail.as_deref().unwrap_or_default().contains("must stay inside the workspace"));
        assert!(!dir.path().parent().map(|parent| parent.join("escape.txt").exists()).unwrap_or(false));
    }

    #[tokio::test]
    async fn run_stops_at_max_tasks() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, OK_SCRIPT);
        let set = EvalSet { split: Split::Train, tasks: vec![task("one", "a"), task("two", "b"), task("three", "c")] };
        let mut opts = options(dir.path(), script);
        opts.max_tasks = 2;

        let summary = run(&set, &opts).await.expect("run");
        assert_eq!(summary.total(), 2);
        assert_eq!(summary.outcomes[1].task_id, "two");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_kills_a_task_that_times_out() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let script = write_stub(dir.path(), STUB_NAME, "#!/bin/sh\nsleep 10\n");
        let mut eval_task = task("slow", "take forever");
        eval_task.timeout_ms = 400;
        let set = EvalSet { split: Split::Train, tasks: vec![eval_task] };

        let summary = run(&set, &options(dir.path(), script)).await.expect("run");
        let outcome = &summary.outcomes[0];
        assert!(!outcome.passed);
        assert_eq!(outcome.failure, Some(FailureSignal::Timeout));
        assert!(outcome.detail.as_deref().unwrap_or_default().contains("timed out"));
        assert!(outcome.duration_ms < 8_000, "timeout must not wait for the child");
    }
}
