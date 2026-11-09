//! JIT-Agent, downgraded to what a single binary can do: instead of training a generator,
//! select a profile from a small archive, and repair the selection when execution is unstable
//! (at most `MAX_JIT_REPAIRS` rounds, mirroring the paper's bound).
//!
//! Everything here is deterministic and model-free: classification is lexical, selection is a
//! pass-rate table over an append-only JSONL archive, repair is a fixed set of slot adjustments.
#![allow(dead_code)]

use crate::evolve::profile::HarnessProfile;
use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Repair budget: the paper keeps only trajectories executable within two rounds.
pub const MAX_JIT_REPAIRS: u32 = 2;

/// Fallback ratios used when a bundle leaves the context slot unset (they mirror `Config`).
const DEFAULT_CONVERSATION_RATIO: f64 = 0.45;
const DEFAULT_RESERVE_RATIO: f64 = 0.15;
/// Overflow repair: shrink conversation budget, floor it, and buy room back in the reserve.
const CONVERSATION_FLOOR: f64 = 0.15;
const CONVERSATION_SHRINK: f64 = 0.8;
const RESERVE_STEP: f64 = 0.05;
/// Prior confidence for an archive-less decision and its ceiling with a consistent archive.
const BASE_CONFIDENCE: f64 = 0.2;
const CONFIDENCE_FLOOR: f64 = 0.05;
const CONFIDENCE_CEILING: f64 = 0.95;
/// A decision about a profile the archive has not seen cannot be better than a weak prior.
const RATIO_CONFIDENCE_CEILING: f64 = 0.9;
/// Each repair is evidence the previous decision was wrong.
const REPAIR_CONFIDENCE_DECAY: f64 = 0.85;

/// Lexical keyword tables. Code keeps the default, so its list only needs the verbs that make a
/// task an edit rather than a question.
const OPS_KEYWORDS: &[&str] = &[
    "deploy",
    "deploys",
    "deployed",
    "deploying",
    "deployment",
    "rollout",
    "release",
    "releases",
    "shell",
    "bash",
    "zsh",
    "powershell",
    "systemd",
    "systemctl",
    "journalctl",
    "service",
    "services",
    "daemon",
    "cron",
    "crontab",
    "docker",
    "dockerfile",
    "compose",
    "podman",
    "kubernetes",
    "kubectl",
    "k8s",
    "helm",
    "terraform",
    "ansible",
    "nginx",
    "apache",
    "caddy",
    "iptables",
    "firewall",
    "ufw",
    "network",
    "networking",
    "dns",
    "proxy",
    "port",
    "ports",
    "socket",
    "server",
    "servers",
    "prod",
    "production",
    "staging",
    "infra",
    "infrastructure",
    "ops",
    "install",
    "uninstall",
    "upgrade",
    "restart",
    "reboot",
    "uptime",
    "monitoring",
    "prometheus",
    "grafana",
    "container",
    "containers",
    "mount",
    "disk",
    "ssh",
    "scp",
    "rsync",
    "triển khai",
    "cài đặt",
    "máy chủ",
    "hạ tầng",
];
const RESEARCH_KEYWORDS: &[&str] = &[
    "explain",
    "explains",
    "explained",
    "explaining",
    "explanation",
    "read",
    "reads",
    "reading",
    "analyze",
    "analyse",
    "analyzes",
    "analyses",
    "analyzed",
    "analysis",
    "review",
    "reviews",
    "reviewing",
    "reviewed",
    "summarize",
    "summarise",
    "summarizes",
    "summary",
    "summaries",
    "overview",
    "inspect",
    "inspects",
    "explore",
    "understand",
    "describe",
    "describes",
    "document",
    "documents",
    "documentation",
    "doc",
    "docs",
    "compare",
    "comparison",
    "difference",
    "differences",
    "why",
    "investigate",
    "research",
    "study",
    "learn",
    "explain",
    "giải thích",
    "phân tích",
    "tóm tắt",
    "đánh giá",
    "đọc",
];
const CODE_KEYWORDS: &[&str] = &[
    "fix",
    "fixes",
    "fixed",
    "fixing",
    "implement",
    "implements",
    "implemented",
    "implementing",
    "add",
    "adds",
    "added",
    "adding",
    "build",
    "builds",
    "building",
    "built",
    "test",
    "tests",
    "testing",
    "tested",
    "compile",
    "compiles",
    "compiling",
    "lint",
    "linting",
    "typecheck",
    "refactor",
    "refactoring",
    "patch",
    "patches",
    "write",
    "writes",
    "writing",
    "edit",
    "edits",
    "editing",
    "change",
    "changes",
    "changing",
    "update",
    "updates",
    "updating",
    "debug",
    "debugging",
    "bug",
    "bugs",
    "error",
    "errors",
    "fail",
    "fails",
    "failed",
    "failing",
    "code",
    "coding",
    "function",
    "module",
    "method",
    "class",
    "rename",
    "remove",
    "delete",
    "create",
    "generate",
    "commit",
    "sửa",
    "viết",
    "thêm",
    "tái cấu trúc",
    "kiểm thử",
    "xây dựng",
];

const CODE_EDIT_KEYWORDS: &[&str] = &["fix", "fixes", "fixed", "implement", "add", "write", "refactor", "patch", "edit", "change", "update", "rename", "remove", "delete", "create", "generate"];
const CODE_DEBUG_KEYWORDS: &[&str] = &["debug", "debugging", "bug", "bugs", "error", "errors", "fail", "fails", "failed", "failing", "panic", "trace", "stacktrace"];
const RESEARCH_EXPLAIN_KEYWORDS: &[&str] = &["explain", "explains", "explained", "explaining", "explanation", "why", "understand", "how"];
const RESEARCH_SUMMARY_KEYWORDS: &[&str] = &["summarize", "summarise", "summarizes", "summary", "summaries", "overview", "document", "documentation", "describe"];
const RESEARCH_REVIEW_KEYWORDS: &[&str] =
    &["review", "reviews", "reviewing", "reviewed", "analyze", "analyse", "analyzes", "analyses", "analysis", "audit", "compare", "difference", "differences", "assess"];
const RESEARCH_READ_KEYWORDS: &[&str] = &["read", "reads", "reading", "inspect", "inspects", "open", "look", "explore", "check"];
const OPS_DEPLOY_KEYWORDS: &[&str] = &["deploy", "deploys", "deployed", "deploying", "deployment", "rollout", "release", "releases", "upgrade"];
const OPS_CONTAINER_KEYWORDS: &[&str] = &["docker", "dockerfile", "compose", "podman", "kubernetes", "kubectl", "k8s", "helm", "container", "containers"];
const OPS_SYSTEM_KEYWORDS: &[&str] = &["systemd", "systemctl", "journalctl", "service", "services", "daemon", "cron", "crontab", "restart", "reboot", "mount", "disk"];
const OPS_NETWORK_KEYWORDS: &[&str] = &["network", "networking", "dns", "proxy", "firewall", "iptables", "ufw", "port", "ports", "ssh", "scp", "rsync"];
const OPS_SHELL_KEYWORDS: &[&str] = &["shell", "bash", "zsh", "powershell", "script", "cmd"];

/// One archived (task class, profile, outcome) record: the cheap replacement for a bandit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JitRecord {
    pub created: String,
    pub variant: Variant,
    pub profile: String,
    pub task_class: String,
    pub passed: bool,
    pub tokens: u64,
    pub duration_ms: u64,
}

/// The decision handed to the harness for one task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JitDecision {
    pub variant: Variant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    pub profile: HarnessProfile,
    pub rationale: String,
    pub confidence: f64,
    pub repairs: u32,
    /// Task class the decision was planned for; the archive needs it and `plan` is the only place
    /// that can compute it. Kept serde-defaulted so older decisions stay readable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_class: String,
}

/// Archive aggregate for one profile within one `(variant, task_class)` bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveChoice {
    pub profile: String,
    pub samples: usize,
    pub pass_rate: f64,
    pub avg_tokens: u64,
}

/// Classifies a task prompt into a variant (lexical, deterministic, no model call).
///
/// Ties resolve `ops` > `research` > `code`: an ops word is the most specific signal, a question
/// word beats a generic verb ("review this patch"), and code is the fallback the spec asks for.
pub fn classify_task(task: &str) -> Variant {
    let lowered = task.to_lowercase();
    let tokens = tokens_of(&lowered);
    let ops = hits(&lowered, &tokens, OPS_KEYWORDS);
    let research = hits(&lowered, &tokens, RESEARCH_KEYWORDS);
    let code = hits(&lowered, &tokens, CODE_KEYWORDS);
    if ops > 0 && ops >= research && ops >= code {
        Variant::Ops
    } else if research > 0 && research >= code {
        Variant::Research
    } else {
        Variant::Code
    }
}

/// Coarse task class used for archive lookup (`code`, `research`, `ops`) plus a short sub-class
/// (`code:edit`, `research:explain`, `ops:deploy`), stable for a given prompt.
pub fn task_class(task: &str) -> String {
    let variant = classify_task(task);
    let lowered = task.to_lowercase();
    let tokens = tokens_of(&lowered);
    let sub = match variant {
        Variant::Code => {
            if hits(&lowered, &tokens, CODE_EDIT_KEYWORDS) > 0 {
                "edit"
            } else if hits(&lowered, &tokens, CODE_DEBUG_KEYWORDS) > 0 {
                "debug"
            } else if hits(&lowered, &tokens, &["test", "tests", "testing", "build", "builds", "compile", "lint", "typecheck", "verify", "ci"]) > 0 {
                "test"
            } else {
                "edit"
            }
        }
        Variant::Research => {
            if hits(&lowered, &tokens, RESEARCH_EXPLAIN_KEYWORDS) > 0 {
                "explain"
            } else if hits(&lowered, &tokens, RESEARCH_SUMMARY_KEYWORDS) > 0 {
                "summary"
            } else if hits(&lowered, &tokens, RESEARCH_REVIEW_KEYWORDS) > 0 {
                "review"
            } else if hits(&lowered, &tokens, RESEARCH_READ_KEYWORDS) > 0 {
                "read"
            } else {
                "explain"
            }
        }
        Variant::Ops => {
            // Shell is the default ops sub-class: a shell task with no more specific cue.
            if hits(&lowered, &tokens, OPS_DEPLOY_KEYWORDS) > 0 {
                "deploy"
            } else if hits(&lowered, &tokens, OPS_CONTAINER_KEYWORDS) > 0 {
                "container"
            } else if hits(&lowered, &tokens, OPS_SYSTEM_KEYWORDS) > 0 {
                "system"
            } else if hits(&lowered, &tokens, OPS_NETWORK_KEYWORDS) > 0 {
                "network"
            } else {
                "shell"
            }
        }
    };
    format!("{}:{sub}", variant.as_str())
}

fn tokens_of(lowered: &str) -> BTreeSet<String> {
    lowered.split(|ch: char| !ch.is_alphanumeric()).filter(|token| !token.is_empty()).map(str::to_string).collect()
}

/// Counts distinct keyword hits: phrases (whitespace or non-ASCII) match as substrings, plain
/// ASCII words match whole tokens so "prefix" never counts as "fix".
fn hits(lowered: &str, tokens: &BTreeSet<String>, keywords: &[&str]) -> usize {
    keywords.iter().filter(|keyword| if keyword.contains(char::is_whitespace) || !keyword.is_ascii() { lowered.contains(**keyword) } else { tokens.contains(**keyword) }).count()
}

/// Picks the best archived profile for a `(variant, task_class)` bucket: highest pass rate, then
/// fewest tokens, then name (stable). `None` when the archive holds no usable sample.
pub fn best_profile(records: &[JitRecord], variant: Variant, task_class: &str) -> Option<ArchiveChoice> {
    let mut buckets: BTreeMap<&str, (usize, usize, u64)> = BTreeMap::new();
    for record in records.iter().filter(|record| record.variant == variant && (record.task_class == task_class || record.task_class.is_empty())) {
        let entry = buckets.entry(record.profile.as_str()).or_insert((0, 0, 0));
        entry.0 += 1;
        if record.passed {
            entry.1 += 1;
        }
        entry.2 = entry.2.saturating_add(record.tokens);
    }
    let mut choices: Vec<ArchiveChoice> = buckets
        .into_iter()
        .filter(|(name, (samples, _, _))| !name.is_empty() && *samples > 0)
        .map(|(name, (samples, passed, tokens))| ArchiveChoice { profile: name.to_string(), samples, pass_rate: passed as f64 / samples as f64, avg_tokens: tokens / samples as u64 })
        .collect();
    choices.sort_by(|a, b| b.pass_rate.total_cmp(&a.pass_rate).then_with(|| a.avg_tokens.cmp(&b.avg_tokens)).then_with(|| a.profile.cmp(&b.profile)));
    choices.into_iter().next()
}

/// Plans a harness for a task from the archive and the current bundle.
pub struct JitPlanner {
    store: HarnessStore,
}

impl JitPlanner {
    pub fn new(store: HarnessStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &HarnessStore {
        &self.store
    }

    /// Resolves the bundle files to plan from: an explicit map wins, otherwise the newest snapshot.
    /// The returned id is the content id when it is known, so a decision is always reproducible.
    fn resolve_files(&self, files: Option<&BTreeMap<String, String>>) -> (Option<BTreeMap<String, String>>, Option<String>) {
        if let Some(files) = files {
            return (Some(files.clone()), Some(files_id(files)));
        }
        match self.store.latest_bundle() {
            Ok(Some(bundle)) => {
                let components = self.store.component_files(&bundle.id).ok();
                (components, Some(bundle.id))
            }
            _ => (None, None),
        }
    }

    pub fn plan(&self, task: &str, files: Option<&BTreeMap<String, String>>) -> Result<JitDecision> {
        let variant = classify_task(task);
        let class = task_class(task);
        let (resolved, bundle) = self.resolve_files(files);

        let (mut profile, source) = match resolved.as_ref() {
            Some(resolved) => match crate::evolve::profile::from_files(resolved, variant) {
                Ok(profile) => (profile, bundle.as_deref().map(|id| format!("bundle {id}")).unwrap_or_else(|| "bundle".to_string())),
                Err(error) => (default_profile(variant), format!("defaults (bundle policy unusable: {error})")),
            },
            None => (default_profile(variant), "defaults (no bundle)".to_string()),
        };
        profile.variant = variant;

        let records = self.records().unwrap_or_default();
        let choice = best_profile(&records, variant, &class);
        let (rationale, confidence) = match &choice {
            None => (format!("jit: {} from {source}; no archive samples for this class yet, using profile `{}` (low confidence)", class, profile.name), BASE_CONFIDENCE),
            Some(choice) if choice.profile == profile.name => {
                let sample_bonus = 0.05 * choice.samples.saturating_sub(1).min(3) as f64;
                let confidence = (0.35 + 0.5 * choice.pass_rate + sample_bonus).clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING);
                (
                    format!("jit: {} from {source}; profile `{}` archived {} sample(s), pass_rate={:.2}, avg_tokens={}", class, choice.profile, choice.samples, choice.pass_rate, choice.avg_tokens),
                    confidence,
                )
            }
            Some(choice) => {
                let confidence = (0.25 + 0.3 * choice.pass_rate).clamp(CONFIDENCE_FLOOR, RATIO_CONFIDENCE_CEILING);
                (
                    format!(
                        "jit: {} from {source}; archived best `{}` ({} sample(s), pass_rate={:.2}, avg_tokens={}) is not materialized here, keeping `{}`",
                        class, choice.profile, choice.samples, choice.pass_rate, choice.avg_tokens, profile.name
                    ),
                    confidence,
                )
            }
        };

        Ok(JitDecision { variant, bundle, profile, rationale, confidence, repairs: 0, task_class: class })
    }

    /// Returns a repaired decision when the signal indicates instability, or `None` at the cap.
    pub fn repair(&self, decision: &JitDecision, signal: &FailureSignal) -> Result<Option<JitDecision>> {
        if decision.repairs >= MAX_JIT_REPAIRS {
            return Ok(None);
        }
        let repairs = decision.repairs + 1;
        let mut next = decision.clone();
        next.repairs = repairs;
        let note = match signal {
            FailureSignal::ContextOverflow => {
                let conversation = next.profile.context_conversation.unwrap_or(DEFAULT_CONVERSATION_RATIO);
                let reserve = next.profile.context_reserve.unwrap_or(DEFAULT_RESERVE_RATIO);
                let shrunk = crate::evolve::profile::clamp_ratio((conversation * CONVERSATION_SHRINK).max(CONVERSATION_FLOOR));
                let grown = crate::evolve::profile::clamp_ratio(reserve + RESERVE_STEP);
                next.profile.context_conversation = Some(shrunk);
                next.profile.context_reserve = Some(grown);
                format!("context_overflow -> conversation {conversation:.2}->{shrunk:.2}, reserve {reserve:.2}->{grown:.2}")
            }
            FailureSignal::VerifyFail => {
                if next.profile.verify_commands.is_empty() {
                    next.profile.verify_commands = self.bundle_verify_commands(next.bundle.as_deref());
                }
                next.profile.jit = true;
                format!("verify_fail -> {} verify command(s) staged, jit enabled", next.profile.verify_commands.len())
            }
            // A tool that fails or returns a payload the harness cannot use is one instability class.
            FailureSignal::ToolError | FailureSignal::ParseError => {
                next.profile.tools = None;
                push_note(&mut next.profile.memory_notes, "jit: tool errors or unparseable tool output observed; keep the full toolset and re-read tool guidance before the next attempt");
                format!("{} -> tools kept full, tool-guidance note added", signal.as_str())
            }
            FailureSignal::NoAnswer => {
                push_note(&mut next.profile.memory_notes, "jit: answers came back empty; middleware must restate the final answer before finishing");
                next.profile.jit = true;
                "no_answer -> middleware note added".to_string()
            }
            FailureSignal::Other(raw) if is_no_match(raw) => {
                next.profile.tools = None;
                push_note(&mut next.profile.memory_notes, "jit: no matching capability for this task shape; restate the plan and check tool guidance");
                next.profile.jit = true;
                "no_match -> tools kept full, capability note added, jit enabled".to_string()
            }
            _ => return Ok(None),
        };
        next.confidence = (decision.confidence * REPAIR_CONFIDENCE_DECAY).clamp(0.0, CONFIDENCE_CEILING);
        next.rationale = format!("{}; repair #{repairs}: {note}", decision.rationale);
        Ok(Some(next))
    }

    /// Appends one archive record (durable, append-only).
    pub fn record(&self, decision: &JitDecision, outcome: &TaskOutcome) -> Result<()> {
        let profile = if decision.profile.name.is_empty() { decision.variant.as_str().to_string() } else { decision.profile.name.clone() };
        let task_class = if decision.task_class.is_empty() { format!("{}:unknown", decision.variant.as_str()) } else { decision.task_class.clone() };
        let record = JitRecord { created: now_rfc3339(), variant: decision.variant, profile, task_class, passed: outcome.passed, tokens: outcome.tokens, duration_ms: outcome.duration_ms };
        let path = self.store.jit_archive();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| anyhow!("create {}: {error}", parent.display()))?;
            }
        }
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|error| anyhow!("open {}: {error}", path.display()))?;
        file.write_all(line.as_bytes()).map_err(|error| anyhow!("append {}: {error}", path.display()))?;
        file.flush().map_err(|error| anyhow!("flush {}: {error}", path.display()))?;
        Ok(())
    }

    /// Reads the archive back, dropping unparseable lines (a torn append must not hide history).
    pub fn records(&self) -> Result<Vec<JitRecord>> {
        let path = self.store.jit_archive();
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(_) => return Ok(vec![]),
        };
        Ok(body.lines().filter_map(|line| serde_json::from_str::<JitRecord>(line.trim()).ok()).collect())
    }

    fn bundle_verify_commands(&self, bundle: Option<&str>) -> Vec<String> {
        let Some(bundle) = bundle else { return vec![] };
        match self.store.component_files(bundle) {
            Ok(files) => crate::evolve::profile::verify_commands_from_files(&files),
            Err(_) => vec![],
        }
    }
}

/// Default `lean` profile for a variant: no ratio overrides, so applying it changes nothing.
fn default_profile(variant: Variant) -> HarnessProfile {
    HarnessProfile { variant, ..HarnessProfile::default() }
}

fn push_note(notes: &mut Vec<String>, note: &str) {
    if !notes.iter().any(|existing| existing == note) {
        notes.push(note.to_string());
    }
}

fn is_no_match(raw: &str) -> bool {
    let lowered = raw.to_lowercase();
    lowered.contains("match") || lowered.contains("nomatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(repairs: u32) -> JitDecision {
        JitDecision {
            variant: Variant::Code,
            bundle: None,
            profile: HarnessProfile::named(Variant::Code),
            rationale: "jit: code:edit test decision".into(),
            confidence: 0.5,
            repairs,
            task_class: "code:edit".into(),
        }
    }

    fn outcome(task_id: &str, passed: bool, tokens: u64) -> TaskOutcome {
        TaskOutcome { task_id: task_id.into(), passed, score: if passed { 1.0 } else { 0.0 }, tokens, duration_ms: 7, failure: None, session_id: None, detail: None }
    }

    #[test]
    fn classify_task_is_lexical_and_deterministic() {
        assert_eq!(classify_task("fix the failing test in src/evolve/profile.rs"), Variant::Code);
        assert_eq!(classify_task("implement caching for the provider"), Variant::Code);
        assert_eq!(classify_task("explain how the harness loop works"), Variant::Research);
        assert_eq!(classify_task("read the file and summarize the parser"), Variant::Research);
        assert_eq!(classify_task("review this patch"), Variant::Research);
        assert_eq!(classify_task("deploy the docker service to production"), Variant::Ops);
        assert_eq!(classify_task("systemd unit keeps restarting"), Variant::Ops);
        assert_eq!(classify_task("sửa lỗi này giúp tôi"), Variant::Code);
        assert_eq!(classify_task("giải thích đoạn code này"), Variant::Research);
        assert_eq!(classify_task(""), Variant::Code);
        assert_eq!(classify_task("prefixture builds"), Variant::Code);
    }

    #[test]
    fn task_class_is_stable_and_subclassed() {
        assert_eq!(task_class("fix the failing test"), "code:edit");
        assert_eq!(task_class("debug the panic in the parser"), "code:debug");
        assert_eq!(task_class("run the unit tests"), "code:test");
        assert_eq!(task_class("explain the borrow checker"), "research:explain");
        assert_eq!(task_class("summarize the design doc"), "research:summary");
        assert_eq!(task_class("review these changes"), "research:review");
        assert_eq!(task_class("deploy the release"), "ops:deploy");
        assert_eq!(task_class("restart the systemd service"), "ops:system");
        // Same prompt, same class: the archive lookup depends on it.
        assert_eq!(task_class("fix the failing test"), task_class("fix the failing test"));
    }

    #[test]
    fn repair_stops_at_the_cap() {
        let store = HarnessStore::new(std::env::temp_dir().join("goon-jit-test-cap"));
        let planner = JitPlanner::new(store);
        let first = planner.repair(&decision(0), &FailureSignal::VerifyFail).expect("repair 1").expect("some");
        assert_eq!(first.repairs, 1);
        assert!(first.profile.jit);
        assert!(first.rationale.contains("repair #1"));

        let second = planner.repair(&first, &FailureSignal::VerifyFail).expect("repair 2").expect("some");
        assert_eq!(second.repairs, MAX_JIT_REPAIRS);
        assert!(second.rationale.contains("repair #2"));

        assert!(planner.repair(&second, &FailureSignal::VerifyFail).expect("repair 3").is_none());
        assert!(planner.repair(&decision(MAX_JIT_REPAIRS), &FailureSignal::ContextOverflow).expect("cap").is_none());
    }

    #[test]
    fn repair_shrinks_context_and_raises_reserve() {
        let planner = JitPlanner::new(HarnessStore::new(std::env::temp_dir().join("goon-jit-test-context")));
        let mut base = decision(0);
        base.profile.context_conversation = Some(0.45);
        base.profile.context_reserve = Some(0.15);
        let repaired = planner.repair(&base, &FailureSignal::ContextOverflow).expect("repair").expect("some");
        let conversation = repaired.profile.context_conversation.expect("conversation");
        assert!((conversation - 0.36).abs() < 1e-9, "conversation was {conversation}");
        assert!((repaired.profile.context_reserve.expect("reserve") - 0.2).abs() < 1e-9);
        assert!(repaired.confidence < base.confidence);

        let mut tight = decision(0);
        tight.profile.context_conversation = Some(0.16);
        let floored = planner.repair(&tight, &FailureSignal::ContextOverflow).expect("repair").expect("some");
        assert_eq!(floored.profile.context_conversation, Some(CONVERSATION_FLOOR));
    }

    #[test]
    fn repair_keeps_toolset_full_and_notes_instability() {
        let planner = JitPlanner::new(HarnessStore::new(std::env::temp_dir().join("goon-jit-test-tools")));
        let mut base = decision(0);
        base.profile.tools = Some(vec!["shell".into(), "read".into()]);
        let repaired = planner.repair(&base, &FailureSignal::ToolError).expect("repair").expect("some");
        assert!(repaired.profile.tools.is_none());
        assert_eq!(repaired.profile.memory_notes.len(), 1);
        assert!(repaired.profile.memory_notes[0].contains("tool guidance"));

        // A repeated repair must not duplicate the note.
        let again = planner.repair(&repaired, &FailureSignal::ParseError).expect("repair").expect("some");
        assert_eq!(again.profile.memory_notes, repaired.profile.memory_notes);

        let no_answer = planner.repair(&base, &FailureSignal::NoAnswer).expect("repair").expect("some");
        assert!(no_answer.profile.memory_notes.iter().any(|note| note.contains("middleware")));
        assert!(no_answer.profile.jit);

        let no_match = planner.repair(&base, &FailureSignal::Other("no_match: capability".into())).expect("repair").expect("some");
        assert!(no_match.profile.memory_notes.iter().any(|note| note.contains("no matching capability")));

        // Signals with no targeted repair are left alone.
        assert!(planner.repair(&base, &FailureSignal::Timeout).expect("timeout").is_none());
        assert!(planner.repair(&base, &FailureSignal::ApprovalDenied).expect("denied").is_none());
    }

    #[test]
    fn archive_records_round_trip_and_skip_corrupt_lines() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let planner = JitPlanner::new(HarnessStore::new(dir.path().join("state")));
        let first = decision(0);
        planner.record(&first, &outcome("task-a", true, 120)).expect("record a");
        planner.record(&first, &outcome("task-b", false, 80)).expect("record b");

        assert!(planner.store().jit_archive().exists());
        {
            use std::io::Write;
            let mut handle = std::fs::OpenOptions::new().append(true).open(planner.store().jit_archive()).expect("open archive");
            handle.write_all(b"{ this is not json \n").expect("append corrupt line");
        }

        let records = planner.records().expect("records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].task_class, "code:edit");
        assert_eq!(records[0].variant, Variant::Code);
        assert!(records[0].passed);
        assert_eq!(records[0].tokens, 120);
        assert!(!records[1].passed);

        // A missing archive is empty history, not an error.
        let empty = JitPlanner::new(HarnessStore::new(dir.path().join("missing")));
        assert!(empty.records().expect("missing archive").is_empty());
    }

    #[test]
    fn best_profile_prefers_pass_rate_then_tokens() {
        let base = |profile: &str, passed: bool, tokens: u64, class: &str| JitRecord {
            created: now_rfc3339(),
            variant: Variant::Code,
            profile: profile.into(),
            task_class: class.into(),
            passed,
            tokens,
            duration_ms: 1,
        };
        let records = vec![
            base("low", true, 10, "code:edit"),
            base("low", false, 10, "code:edit"),
            base("cheap", true, 20, "code:edit"),
            base("pricey", true, 90, "code:edit"),
            base("other-class", true, 1, "code:test"),
            JitRecord { variant: Variant::Research, ..base("research-only", true, 1, "code:edit") },
        ];
        let choice = best_profile(&records, Variant::Code, "code:edit").expect("choice");
        assert_eq!(choice.profile, "cheap");
        assert_eq!(choice.samples, 1);
        assert_eq!(choice.pass_rate, 1.0);
        assert_eq!(choice.avg_tokens, 20);

        let tied = best_profile(&records, Variant::Code, "code:edit");
        assert_eq!(tied.expect("tie").avg_tokens, 20);

        assert!(best_profile(&records, Variant::Ops, "ops:deploy").is_none());
    }

    #[test]
    fn plan_uses_explicit_files_without_store() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let planner = JitPlanner::new(HarnessStore::new(dir.path().join("state")));
        let mut files = BTreeMap::new();
        files.insert("policy/context.json".to_string(), r#"{"conversation":0.62,"reserve":0.12}"#.to_string());
        files.insert("policy/jit.json".to_string(), r#"{"enabled":true}"#.to_string());

        let planned = planner.plan("fix the failing test", Some(&files)).expect("plan");
        assert_eq!(planned.variant, Variant::Code);
        assert_eq!(planned.task_class, "code:edit");
        assert_eq!(planned.bundle.as_deref(), Some(files_id(&files).as_str()));
        assert_eq!(planned.profile.name, "bundle:code");
        assert_eq!(planned.profile.context_conversation, Some(0.62));
        assert!(planned.profile.jit);
        assert_eq!(planned.repairs, 0);
        assert!(planned.rationale.contains("no archive samples"));
        assert!(planned.confidence > 0.0 && planned.confidence <= 1.0);

        // Archive is empty, so confidence stays in the low band.
        assert!(planned.confidence <= 0.3);
    }

    #[test]
    fn plan_reports_archive_support_for_the_current_profile() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let planner = JitPlanner::new(HarnessStore::new(dir.path().join("state")));
        let decision = JitDecision {
            variant: Variant::Code,
            bundle: Some("bundle-x".into()),
            profile: HarnessProfile::named(Variant::Code),
            rationale: "seed".into(),
            confidence: 0.5,
            repairs: 0,
            task_class: "code:edit".into(),
        };
        planner.record(&decision, &outcome("t1", true, 100)).expect("rec 1");
        planner.record(&decision, &outcome("t2", true, 100)).expect("rec 2");

        let mut files = BTreeMap::new();
        files.insert("policy/context.json".to_string(), r#"{"conversation":0.5}"#.to_string());
        let planned = planner.plan("fix the failing test", Some(&files)).expect("plan");
        assert!(planned.rationale.contains("2 sample(s)"));
        assert!(planned.rationale.contains("pass_rate=1.00"));
        assert!(planned.confidence > 0.5);
        assert!(planned.confidence <= CONFIDENCE_CEILING);
    }
}
