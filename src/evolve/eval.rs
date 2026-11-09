//! Evaluation harness: runs the real CLI (this same binary) on a task set, with a bundle
//! applied, and produces a `RunSummary`. Stub mode keeps the loop fully offline and
//! deterministic for tests; the same code path drives a live provider.
//!
//! Judging is deliberately boring so a score cannot drift: the child must exit 0, print the
//! expected substring on stdout when the task declares one, and pass every verify command.
//! Each task runs in its own throwaway workspace under the system temp directory.
#![allow(dead_code)]

use crate::evolve::types::*;
use anyhow::{anyhow, Result};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Variant>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
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

/// Runs all tasks, judging each by exit status, expected substring and verify commands.
pub async fn run(set: &EvalSet, opts: &EvalOptions) -> Result<RunSummary> {
    let limit = if opts.max_tasks == 0 { set.tasks.len() } else { opts.max_tasks.min(set.tasks.len()) };
    let mut outcomes = Vec::with_capacity(limit);
    for task in set.tasks.iter().take(limit) {
        outcomes.push(run_task(task, opts).await);
    }
    Ok(RunSummary::new(set.split, opts.bundle.clone(), opts.variant, outcomes))
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
            score: if judgement.passed { 1.0 } else { 0.0 },
            tokens: 0,
            duration_ms,
            failure: judgement.failure,
            session_id: None,
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

    let child = command.spawn().map_err(|error| anyhow!("spawn {}: {error}", opts.goon_bin.display()))?;
    // On timeout the future is dropped, which drops the child: `kill_on_drop` then reaps it.
    let finished = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await;
    let (exit_ok, stdout, stderr, timed_out) = match finished {
        Ok(Ok(output)) => (output.status.success(), String::from_utf8_lossy(&output.stdout).to_string(), String::from_utf8_lossy(&output.stderr).to_string(), false),
        Ok(Err(error)) => return Err(anyhow!("wait {}: {error}", opts.goon_bin.display())),
        Err(_) => (false, String::new(), String::new(), true),
    };

    let verify_ok = if timed_out { false } else { verify_all(&task.verify, root, timeout_ms).await? };
    let ok = passed(exit_ok, task.expect.as_deref(), &stdout, verify_ok);
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
    Ok(Judgement { passed: ok, failure, detail })
}

fn effective_timeout(task: &EvalTask, opts: &EvalOptions) -> u64 {
    let task_ms = if task.timeout_ms == 0 { DEFAULT_TIMEOUT_MS } else { task.timeout_ms };
    let budget = if opts.timeout_ms == 0 { task_ms } else { task_ms.min(opts.timeout_ms) };
    budget.max(1)
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
    let (program, args) = shell_command(command);
    let child = tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| anyhow!("spawn verify `{command}`: {error}"))?;
    match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output.status.success()),
        // A verify command that cannot be waited for simply did not pass.
        Ok(Err(_)) | Err(_) => Ok(false),
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
        EvalTask { id: id.into(), prompt: prompt.into(), files: BTreeMap::new(), expect: None, verify: vec![], variant: None, timeout_ms: 30_000 }
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
