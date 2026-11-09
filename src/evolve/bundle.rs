//! Harness bundle: the file-level, revertible representation of every editable component
//! (AHE component observability). Bundle paths are fixed by `types::component_kind_for_path`.
#![allow(dead_code)]

use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// What changed between two bundles, component by component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Added,
    Modified,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComponentDiff {
    pub path: String,
    pub kind: ComponentKind,
    pub status: DiffStatus,
}

/// Resolved, prompt-ready view of a bundle (what the harness actually consumes).
#[derive(Clone, Debug, Default)]
pub struct HarnessContext {
    pub system_append: String,
    pub skills: Vec<(String, String)>,
    pub tool_guidance: String,
    pub middleware: Vec<String>,
    pub memory_notes: Vec<String>,
    pub verify_commands: Vec<String>,
    pub context_reserve: Option<f64>,
    pub context_conversation: Option<f64>,
    pub context_files: Option<f64>,
    pub jit_enabled: Option<bool>,
}

const SYSTEM_CODE: &str = "\
# goon harness: code

You are goon, a terminal coding harness working inside the user's repository.

## Loop
- Read the target files before editing; prefer the smallest correct diff.
- Make one focused change per turn, then verify it.
- When verification fails, read the failure text and fix the cause, not the symptom.

## Rules
- Never rewrite a file you have not read in this session.
- Keep comments short and only where the code is not self-evident.
- Report what you changed and what you did not test.
";

const SYSTEM_RESEARCH: &str = "\
# goon harness: research

You are goon, a terminal research harness answering questions from evidence.

## Loop
- Restate the question, list the evidence that would answer it, then collect it.
- Quote the exact source for every claim; label inference as inference.
- Stop when the evidence is sufficient; do not pad.

## Rules
- Never present an unverified guess as a fact.
- Prefer primary sources over summaries.
- Lead with the conclusion, then the supporting detail.
";

const SYSTEM_OPS: &str = "\
# goon harness: ops

You are goon, a terminal operations harness changing live state.

## Loop
- Inspect the current state before changing it (read the config, list the target).
- Run the least destructive command that proves the next step.
- Dry run, then the real run, then a verification read.

## Rules
- Never run an irreversible command without stating the blast radius.
- Keep every change reversible and record the rollback command.
- Quote command output verbatim when reporting a failure.
";

const TOOL_GUIDANCE: &str = "\
# Tool guidance

- Read before you write: call the read tool on a file before editing it.
- Prefer targeted edits over full rewrites; keep the diff reviewable.
- One tool call per intent; do not repeat a failing call unchanged.
- When a tool errors, read the error text and change the input, not the goal.
- Bound output: use ranges and limits instead of dumping whole logs.
";

const MIDDLEWARE: &str = "\
# Middleware directives

- Before an edit, confirm the file was read in this session.
- After a batch of edits, run the commands in `policy/verify.json`.
- On a verify failure, feed the first error back before trying another patch.
- Do not retry a denied tool call with different arguments for the same intent.
";

const MEMORY: &str = "\
# Memory notes

- Re-read any file that changed since your last read of it.
- Prefer the repository's own conventions over general style advice.
- Record a fact once; drop notes the code contradicts.
";

fn seed_skills(variant: Variant) -> Vec<(&'static str, &'static str)> {
    match variant {
        Variant::Code => vec![
            (
                "skills/read-before-write.md",
                "# Read before write\n\n- Read a file, or the region you intend to change, before editing it.\n- Re-read after any external change; stale contents cause destructive edits.\n",
            ),
            (
                "skills/verify-first.md",
                "# Verify first\n\n- Run the narrowest check that would fail if the change is wrong.\n- Report the command and its result; do not claim a pass you did not observe.\n",
            ),
        ],
        Variant::Research => vec![
            ("skills/cite-sources.md", "# Cite sources\n\n- Every factual claim carries the file, line, or URL it came from.\n- Separate quoted evidence from your own inference.\n"),
            (
                "skills/primary-sources.md",
                "# Prefer primary sources\n\n- Read the source itself before trusting a summary or a search snippet.\n- Note the version or commit you read; conclusions drift with it.\n",
            ),
        ],
        Variant::Ops => vec![
            (
                "skills/dry-run-first.md",
                "# Dry run first\n\n- Prefer a dry run or `--plan` mode before the real command.\n- State the target and the blast radius before running anything destructive.\n",
            ),
            ("skills/rollback-plan.md", "# Rollback plan\n\n- Write the rollback command before making the change.\n- Verify the change with a read, not with the absence of an error.\n"),
        ],
    }
}

fn seed_subagents(variant: Variant) -> Vec<(String, String)> {
    let focus = match variant {
        Variant::Code => "Focus on the diff and the tests that should cover it.",
        Variant::Research => "Focus on the citations and the claims they support.",
        Variant::Ops => "Focus on the target state, the rollback, and the verification read.",
    };
    vec![
        ("subagents/scout.md".to_string(), "# Scout\n\nRead-only reconnaissance: locate the relevant files, commands, and existing conventions. Return a short map, never an edit.\n".to_string()),
        (
            "subagents/reviewer.md".to_string(),
            format!("# Reviewer\n\nAdversarial review of the completed work: check the requirement, the diff, and the missing verification. Reject on evidence, not on taste. {focus}\n"),
        ),
    ]
}

fn seed_verify_policy(variant: Variant) -> String {
    let commands = match variant {
        Variant::Code => "  \"commands\": [\"cargo check --lib\", \"cargo test --lib\"]",
        Variant::Research | Variant::Ops => "  \"commands\": []",
    };
    format!("{{\n  \"version\": 1,\n{commands}\n}}\n")
}

fn seed_context_policy(variant: Variant) -> String {
    let (reserve, conversation, files) = match variant {
        Variant::Code => (0.12, 0.54, 0.34),
        Variant::Research => (0.10, 0.60, 0.30),
        Variant::Ops => (0.15, 0.60, 0.25),
    };
    format!("{{\n  \"version\": 1,\n  \"reserve\": {reserve},\n  \"conversation\": {conversation},\n  \"files\": {files}\n}}\n")
}

/// Seed bundle for a variant (`goon evolve init`).
pub fn seed_files(variant: Variant) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let system = match variant {
        Variant::Code => SYSTEM_CODE,
        Variant::Research => SYSTEM_RESEARCH,
        Variant::Ops => SYSTEM_OPS,
    };
    files.insert(SYSTEM_PROMPT_PATH.to_string(), system.to_string());
    files.insert(TOOL_GUIDANCE_PATH.to_string(), TOOL_GUIDANCE.to_string());
    files.insert(MIDDLEWARE_PATH.to_string(), MIDDLEWARE.to_string());
    files.insert(MEMORY_PATH.to_string(), MEMORY.to_string());
    files.insert(VERIFY_POLICY_PATH.to_string(), seed_verify_policy(variant));
    files.insert(CONTEXT_POLICY_PATH.to_string(), seed_context_policy(variant));
    files.insert(JIT_POLICY_PATH.to_string(), "{\n  \"version\": 1,\n  \"enabled\": true\n}\n".to_string());
    for (path, body) in seed_skills(variant) {
        files.insert(path.to_string(), body.to_string());
    }
    for (path, body) in seed_subagents(variant) {
        files.insert(path, body);
    }
    files
}

/// Reads `.goon/harness/**` from a workspace; unknown paths are ignored.
pub fn load_files(workspace: &Path) -> Result<BTreeMap<String, String>> {
    let root = crate::evolve::bundle_dir(workspace);
    let mut files = BTreeMap::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!("cannot list {}", dir.display())));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read an entry of {}", dir.display()))?;
            let path = entry.path();
            let meta = entry.metadata().with_context(|| format!("cannot stat {}", path.display()))?;
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&root) else { continue };
            let normalized = relative.components().map(|part| part.as_os_str().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>().join("/");
            if !is_component_path(&normalized) {
                continue;
            }
            // Non-UTF-8 component bodies are unusable for prompt assembly: skip them.
            if let Ok(body) = std::fs::read_to_string(&path) {
                files.insert(normalized, body);
            }
        }
    }
    Ok(files)
}

/// Writes only known component paths back into `.goon/harness/**`.
pub fn write_files(workspace: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    let root = crate::evolve::bundle_dir(workspace);
    crate::config::mkdir_private(&root).with_context(|| format!("cannot create {}", root.display()))?;
    let base = root.canonicalize().with_context(|| format!("cannot resolve {}", root.display()))?;
    for (path, body) in files {
        if !is_component_path(path) {
            bail!("Refusing to write unknown component path {path}.");
        }
        let Some(relative) = crate::evolve::store::safe_relative(path) else {
            bail!("Refusing unsafe component path {path}.");
        };
        let target = root.join(relative);
        let parent = match target.parent() {
            Some(parent) => parent.to_path_buf(),
            None => bail!("Refusing component path {path} without a parent directory."),
        };
        crate::config::mkdir_private(&parent).with_context(|| format!("cannot create {}", parent.display()))?;
        // Resolve after creation so a symlinked or `..`-spliced path cannot escape the bundle dir.
        let resolved = parent.canonicalize().with_context(|| format!("cannot resolve {}", parent.display()))?;
        if !resolved.starts_with(&base) {
            bail!("Refusing component path {path} outside the bundle directory.");
        }
        crate::config::atomic_write(&target, body.as_bytes()).with_context(|| format!("cannot write {}", target.display()))?;
    }
    Ok(())
}

/// Persists a content-addressed snapshot and returns its manifest.
pub fn snapshot(store: &HarnessStore, files: &BTreeMap<String, String>, parent: Option<&str>, round: u32, variant: Variant) -> Result<HarnessBundle> {
    let bundle = HarnessBundle::build(files, parent, round, variant);
    store.ensure()?;
    store.save_component_files(&bundle.id, files)?;
    store.save_bundle(&bundle)?;
    Ok(bundle)
}

/// Component-level diff between two file maps, sorted by path.
pub fn diff(base: &BTreeMap<String, String>, candidate: &BTreeMap<String, String>) -> Vec<ComponentDiff> {
    let mut paths: Vec<&String> = base.keys().collect();
    paths.extend(candidate.keys());
    paths.sort();
    paths.dedup();
    let mut diffs = Vec::new();
    for path in paths {
        let Some(kind) = component_kind_for_path(path) else { continue };
        let before = base.get(path);
        let after = candidate.get(path);
        let status = match (before, after) {
            (None, Some(_)) => DiffStatus::Added,
            (Some(_), None) => DiffStatus::Removed,
            (Some(before), Some(after)) if before != after => DiffStatus::Modified,
            _ => continue,
        };
        diffs.push(ComponentDiff { path: path.clone(), kind, status });
    }
    diffs
}

/// Parses `policy/*.json` components into typed overrides; invalid JSON falls back to defaults.
pub fn context(files: &BTreeMap<String, String>) -> HarnessContext {
    let mut ctx = HarnessContext::default();
    if let Some(body) = files.get(SYSTEM_PROMPT_PATH) {
        ctx.system_append = body.trim().to_string();
    }
    if let Some(body) = files.get(TOOL_GUIDANCE_PATH) {
        ctx.tool_guidance = body.trim().to_string();
    }
    ctx.middleware = directive_lines(files.get(MIDDLEWARE_PATH));
    ctx.memory_notes = bullet_lines(files.get(MEMORY_PATH));
    for (path, body) in files {
        if let Some(name) = path.strip_prefix("skills/").and_then(|rest| rest.strip_suffix(".md")) {
            if !name.is_empty() {
                ctx.skills.push((name.to_string(), body.clone()));
            }
        }
    }

    if let Some(policy) = parse_json(files.get(VERIFY_POLICY_PATH)) {
        let source = lookup(&policy, &["commands", "verifyCommands", "verify_commands"]).unwrap_or(&policy);
        if let Some(commands) = string_list(source) {
            ctx.verify_commands = commands;
        }
    }
    if let Some(policy) = parse_json(files.get(CONTEXT_POLICY_PATH)) {
        ctx.context_reserve = fraction(&policy, &["reserve", "contextReserveRatio", "reserveRatio", "context_reserve_ratio"]);
        ctx.context_conversation = fraction(&policy, &["conversation", "contextConversationRatio", "conversationRatio", "context_conversation_ratio"]);
        ctx.context_files = fraction(&policy, &["files", "contextFilesRatio", "filesRatio", "context_files_ratio"]);
    }
    if let Some(policy) = parse_json(files.get(JIT_POLICY_PATH)) {
        ctx.jit_enabled = match policy {
            Value::Bool(enabled) => Some(enabled),
            _ => boolean(&policy, &["enabled", "jit"]),
        };
    }
    ctx
}

/// Restores a snapshot's files into the workspace bundle directory.
pub fn apply_to_workspace(store: &HarnessStore, id: &str, workspace: &Path) -> Result<()> {
    let files = store.component_files(id)?;
    if files.is_empty() {
        bail!("Bundle {id} has no component files to restore.");
    }
    let root = crate::evolve::bundle_dir(workspace);
    let existing = load_files(workspace)?;
    for path in existing.keys() {
        if files.contains_key(path) {
            continue;
        }
        let Some(relative) = crate::evolve::store::safe_relative(path) else { continue };
        let target = root.join(relative);
        if !target.starts_with(&root) {
            continue;
        }
        std::fs::remove_file(&target).with_context(|| format!("cannot remove stale component {}", target.display()))?;
    }
    write_files(workspace, &files)
}

/// True when a path is a known component slot (guards every write).
pub fn is_component_path(path: &str) -> bool {
    component_kind_for_path(path).is_some()
}

fn parse_json(body: Option<&String>) -> Option<Value> {
    serde_json::from_str::<Value>(body?).ok()
}

fn lookup<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}

/// Accepts a JSON array of commands, an array of argv arrays, or one bare string.
fn string_list(value: &Value) -> Option<Vec<String>> {
    let mut commands = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(text) if !text.trim().is_empty() => commands.push(text.trim().to_string()),
                    Value::Array(parts) => {
                        let joined = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
                        if !joined.trim().is_empty() {
                            commands.push(joined.trim().to_string());
                        }
                    }
                    _ => {}
                }
            }
            Some(commands)
        }
        Value::String(text) if !text.trim().is_empty() => Some(vec![text.trim().to_string()]),
        _ => None,
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(inner) => inner.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Ratios may be written as `0.12` or as a percentage (`12`); anything else is ignored.
fn fraction(value: &Value, keys: &[&str]) -> Option<f64> {
    let raw = number(lookup(value, keys)?)?;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let normalized = if raw > 1.0 { raw / 100.0 } else { raw };
    (normalized > 0.0 && normalized <= 1.0).then_some(normalized)
}

fn boolean(value: &Value, keys: &[&str]) -> Option<bool> {
    match lookup(value, keys)? {
        Value::Bool(flag) => Some(*flag),
        Value::Number(inner) => inner.as_f64().map(|number| number != 0.0),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn bullet_lines(body: Option<&String>) -> Vec<String> {
    let Some(body) = body else { return Vec::new() };
    body.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let text = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")).unwrap_or("");
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect()
}

fn directive_lines(body: Option<&String>) -> Vec<String> {
    let Some(body) = body else { return Vec::new() };
    body.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let text = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).unwrap_or(trimmed).trim();
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn variants() -> [Variant; 3] {
        [Variant::Code, Variant::Research, Variant::Ops]
    }

    #[test]
    fn seed_covers_every_component_slot() {
        for variant in variants() {
            let files = seed_files(variant);
            let kinds: BTreeSet<ComponentKind> = files.keys().filter_map(|path| component_kind_for_path(path)).collect();
            for kind in [
                ComponentKind::SystemPrompt,
                ComponentKind::Skill,
                ComponentKind::ToolGuidance,
                ComponentKind::Middleware,
                ComponentKind::SubagentRole,
                ComponentKind::MemoryPolicy,
                ComponentKind::VerifyPolicy,
                ComponentKind::ContextPolicy,
                ComponentKind::JitPolicy,
            ] {
                assert!(kinds.contains(&kind), "{} missing {:?}", variant.as_str(), kind);
            }
            for path in [VERIFY_POLICY_PATH, CONTEXT_POLICY_PATH, JIT_POLICY_PATH] {
                let body = files.get(path).unwrap();
                let parsed: Value = serde_json::from_str(body).unwrap();
                assert!(parsed.is_object(), "{path} must be a JSON object");
            }
            // Seeding is deterministic.
            assert_eq!(seed_files(variant), files);
            assert_eq!(HarnessBundle::build(&files, None, 0, variant).id, files_id(&files));
        }
    }

    #[test]
    fn write_then_load_roundtrip_ignores_unknown_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let files = seed_files(Variant::Code);
        write_files(workspace, &files).unwrap();
        assert_eq!(load_files(workspace).unwrap(), files);

        // Unknown slots and non-markdown files are invisible to the bundle.
        let harness = crate::evolve::bundle_dir(workspace);
        std::fs::write(harness.join("notes.txt"), "ignore me").unwrap();
        std::fs::create_dir_all(harness.join("skills")).unwrap();
        std::fs::write(harness.join("skills").join("raw.txt"), "ignore me too").unwrap();
        assert_eq!(load_files(workspace).unwrap(), files);

        // Missing bundle directory is an empty bundle, not an error.
        let empty = tempfile::tempdir().unwrap();
        assert!(load_files(empty.path()).unwrap().is_empty());
    }

    #[test]
    fn write_files_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut hostile = BTreeMap::new();
        hostile.insert("../escape.md".to_string(), "nope".to_string());
        assert!(write_files(&workspace, &hostile).is_err());
        let mut hostile = BTreeMap::new();
        hostile.insert("skills/../../escape.md".to_string(), "nope".to_string());
        assert!(write_files(&workspace, &hostile).is_err());
        let mut hostile = BTreeMap::new();
        hostile.insert("/etc/system.md".to_string(), "nope".to_string());
        assert!(write_files(&workspace, &hostile).is_err());
        let mut hostile = BTreeMap::new();
        hostile.insert("skills\\windows.md".to_string(), "nope".to_string());
        assert!(write_files(&workspace, &hostile).is_err());
        let mut unknown = BTreeMap::new();
        unknown.insert("eval/tasks.json".to_string(), "nope".to_string());
        assert!(write_files(&workspace, &unknown).is_err());
        assert!(!dir.path().join("escape.md").exists());
        assert!(!workspace.parent().unwrap().join("escape.md").exists());
    }

    #[test]
    fn diff_is_sorted_and_classified() {
        let mut base = BTreeMap::new();
        base.insert("system.md".to_string(), "one".to_string());
        base.insert("middleware.md".to_string(), "hook".to_string());
        base.insert("skills/gone.md".to_string(), "old".to_string());
        let mut candidate = BTreeMap::new();
        candidate.insert("system.md".to_string(), "two".to_string());
        candidate.insert("middleware.md".to_string(), "hook".to_string());
        candidate.insert("skills/new.md".to_string(), "fresh".to_string());
        let diffs = diff(&base, &candidate);
        let summary: Vec<(String, DiffStatus)> = diffs.iter().map(|entry| (entry.path.clone(), entry.status)).collect();
        assert_eq!(summary, vec![("skills/gone.md".to_string(), DiffStatus::Removed), ("skills/new.md".to_string(), DiffStatus::Added), ("system.md".to_string(), DiffStatus::Modified),]);
        assert_eq!(diffs[2].kind, ComponentKind::SystemPrompt);
        assert!(diff(&base, &base).is_empty());
    }

    #[test]
    fn context_parses_policies_and_tolerates_garbage() {
        let files = seed_files(Variant::Code);
        let ctx = context(&files);
        assert_eq!(ctx.verify_commands, vec!["cargo check --lib".to_string(), "cargo test --lib".to_string()]);
        assert_eq!(ctx.context_reserve, Some(0.12));
        assert_eq!(ctx.context_conversation, Some(0.54));
        assert_eq!(ctx.context_files, Some(0.34));
        assert_eq!(ctx.jit_enabled, Some(true));
        assert!(ctx.system_append.starts_with("# goon harness: code"));
        assert!(!ctx.tool_guidance.is_empty());
        assert!(!ctx.middleware.is_empty());
        assert!(!ctx.memory_notes.is_empty());
        let skills: Vec<&str> = ctx.skills.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(skills, vec!["read-before-write", "verify-first"]);

        // Percent form is normalised, and broken JSON falls back to defaults without panicking.
        let mut odd = BTreeMap::new();
        odd.insert(CONTEXT_POLICY_PATH.to_string(), "{\"reserve\":12,\"conversation\":\"62\",\"files\":\"junk\"}".to_string());
        odd.insert(VERIFY_POLICY_PATH.to_string(), "{ not json".to_string());
        odd.insert(JIT_POLICY_PATH.to_string(), "false".to_string());
        let ctx = context(&odd);
        assert_eq!(ctx.context_reserve, Some(0.12));
        assert_eq!(ctx.context_conversation, Some(0.62));
        assert_eq!(ctx.context_files, None);
        assert!(ctx.verify_commands.is_empty());
        assert_eq!(ctx.jit_enabled, Some(false));
        assert!(ctx.system_append.is_empty());

        // Bare array verify policy is accepted.
        let mut listed = BTreeMap::new();
        listed.insert(VERIFY_POLICY_PATH.to_string(), "[\"cargo check\"]".to_string());
        assert_eq!(context(&listed).verify_commands, vec!["cargo check".to_string()]);
    }

    #[test]
    fn snapshot_and_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::new(dir.path().join("state"));
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let seed = seed_files(Variant::Ops);
        write_files(&workspace, &seed).unwrap();

        let first = snapshot(&store, &seed, None, 0, Variant::Ops).unwrap();
        assert_eq!(first.id, files_id(&seed));
        assert_eq!(first.round, 0);
        assert!(first.parent.is_none());
        let again = snapshot(&store, &seed, None, 0, Variant::Ops).unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(store.list_bundles().unwrap(), vec![first.id.clone()]);
        assert_eq!(store.component_files(&first.id).unwrap(), seed);

        // The workspace drifts, then the snapshot is restored exactly.
        let mut drifted = seed.clone();
        drifted.insert(SYSTEM_PROMPT_PATH.to_string(), "tampered".to_string());
        drifted.insert("skills/extra.md".to_string(), "stale skill".to_string());
        write_files(&workspace, &drifted).unwrap();
        assert_eq!(load_files(&workspace).unwrap(), drifted);

        apply_to_workspace(&store, &first.id, &workspace).unwrap();
        assert_eq!(load_files(&workspace).unwrap(), seed);
        assert!(!crate::evolve::bundle_dir(&workspace).join("skills").join("extra.md").exists());
        assert!(apply_to_workspace(&store, "missing-id", &workspace).is_err());
    }
}
