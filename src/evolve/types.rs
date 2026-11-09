//! Frozen contract types for the harness-evolution subsystem (`src/evolve/`).
//!
//! The design follows six papers:
//! - arXiv:2604.25850 (AHE): component / experience / decision observability.
//! - arXiv:2606.14249 (HarnessX): composable, replaceable harness primitives.
//! - arXiv:2608.25593 (JIT-Agent): task-adaptive harness synthesis and repair.
//! - arXiv:2609.00006 (Anatomy): component-level design patterns of coding agents.
//! - arXiv:2605.13357 (Runtime Substrate): runtime responsibilities and state.
//! - arXiv:2608.23041 (AutoSaddler): diagnosis -> targeted patch -> verify -> durable update.
//!
//! Nothing here performs I/O. Every module in this subsystem consumes these types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Schema version of the on-disk evolution artifacts (bundles, manifests, DAG nodes).
pub const EVOLVE_SCHEMA: u64 = 1;
/// Workspace-relative root of the editable harness bundle.
pub const BUNDLE_DIR: &str = ".goon/harness";
/// Component paths are stable: the evolution agent addresses components by these names.
pub const SYSTEM_PROMPT_PATH: &str = "system.md";
pub const TOOL_GUIDANCE_PATH: &str = "tools/guidance.md";
pub const MIDDLEWARE_PATH: &str = "middleware.md";
pub const SKILLS_DIR: &str = "skills";
pub const SUBAGENTS_DIR: &str = "subagents";
pub const MEMORY_PATH: &str = "memory.md";
pub const VERIFY_POLICY_PATH: &str = "policy/verify.json";
pub const CONTEXT_POLICY_PATH: &str = "policy/context.json";
pub const JIT_POLICY_PATH: &str = "policy/jit.json";

/// RFC 3339 UTC timestamp with second precision (repo convention).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// First 16 hex chars of a sha256 digest: stable, human-typable id.
pub fn short_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(digest)[..16].to_string()
}

/// Deterministic id of a component map (sorted by path).
pub fn files_id(files: &BTreeMap<String, String>) -> String {
    let mut buffer = String::new();
    for (path, body) in files {
        buffer.push_str(path);
        buffer.push('\u{1f}');
        buffer.push_str(&short_hash(body.as_bytes()));
        buffer.push('\u{1e}');
    }
    short_hash(buffer.as_bytes())
}

/// Cheap token estimate used for budgets; matches the harness's conservative character heuristic.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Collapses volatile substrings (numbers, hashes, absolute paths) so equal failures cluster.
pub fn normalize_signature(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(160));
    let mut token = String::new();
    let push_token = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        let word = token.as_str();
        let digits = word.chars().filter(|c| c.is_ascii_digit()).count();
        let hexish = word.len() >= 6 && word.chars().all(|c| c.is_ascii_hexdigit()) && word.chars().any(|c| c.is_ascii_digit());
        if digits * 2 >= word.len() || hexish || word.contains('\\') || word.contains('/') {
            out.push_str("<var>");
        } else {
            out.push_str(&word.to_ascii_lowercase());
        }
        token.clear();
    };
    for ch in raw.chars() {
        if ch.is_whitespace() {
            push_token(&mut token, &mut out);
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            token.push(ch);
        }
    }
    push_token(&mut token, &mut out);
    let trimmed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > 160 {
        trimmed.chars().take(160).collect()
    } else {
        trimmed
    }
}

/// The seven harness component classes identified by AHE, plus the JIT policy slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    SystemPrompt,
    Skill,
    ToolGuidance,
    Middleware,
    SubagentRole,
    MemoryPolicy,
    VerifyPolicy,
    ContextPolicy,
    JitPolicy,
}

impl ComponentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ComponentKind::SystemPrompt => "system_prompt",
            ComponentKind::Skill => "skill",
            ComponentKind::ToolGuidance => "tool_guidance",
            ComponentKind::Middleware => "middleware",
            ComponentKind::SubagentRole => "subagent_role",
            ComponentKind::MemoryPolicy => "memory_policy",
            ComponentKind::VerifyPolicy => "verify_policy",
            ComponentKind::ContextPolicy => "context_policy",
            ComponentKind::JitPolicy => "jit_policy",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "system_prompt" => ComponentKind::SystemPrompt,
            "skill" => ComponentKind::Skill,
            "tool_guidance" => ComponentKind::ToolGuidance,
            "middleware" => ComponentKind::Middleware,
            "subagent_role" => ComponentKind::SubagentRole,
            "memory_policy" => ComponentKind::MemoryPolicy,
            "verify_policy" => ComponentKind::VerifyPolicy,
            "context_policy" => ComponentKind::ContextPolicy,
            "jit_policy" => ComponentKind::JitPolicy,
            _ => return None,
        })
    }

    /// AHE ablation ordering: structural components (tools/middleware/memory) beat prompt prose.
    pub fn is_structural(self) -> bool {
        !matches!(self, ComponentKind::SystemPrompt | ComponentKind::Skill)
    }
}

/// Kind inferred from a bundle-relative path; `None` for unknown paths (rejected by `EditScope`).
pub fn component_kind_for_path(path: &str) -> Option<ComponentKind> {
    let path = path.trim_start_matches("./").trim_start_matches('/');
    if path == SYSTEM_PROMPT_PATH {
        return Some(ComponentKind::SystemPrompt);
    }
    if path == TOOL_GUIDANCE_PATH {
        return Some(ComponentKind::ToolGuidance);
    }
    if path == MIDDLEWARE_PATH {
        return Some(ComponentKind::Middleware);
    }
    if path == MEMORY_PATH {
        return Some(ComponentKind::MemoryPolicy);
    }
    if path == VERIFY_POLICY_PATH {
        return Some(ComponentKind::VerifyPolicy);
    }
    if path == CONTEXT_POLICY_PATH {
        return Some(ComponentKind::ContextPolicy);
    }
    if path == JIT_POLICY_PATH {
        return Some(ComponentKind::JitPolicy);
    }
    if let Some(rest) = path.strip_prefix("skills/") {
        return rest.ends_with(".md").then_some(ComponentKind::Skill);
    }
    if let Some(rest) = path.strip_prefix("subagents/") {
        return rest.ends_with(".md").then_some(ComponentKind::SubagentRole);
    }
    None
}

/// One editable component inside a bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEntry {
    pub kind: ComponentKind,
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl ComponentEntry {
    pub fn from_body(path: &str, body: &str) -> Option<Self> {
        let kind = component_kind_for_path(path)?;
        Some(Self { kind, id: path.to_string(), path: path.to_string(), sha256: short_hash(body.as_bytes()), bytes: body.len() as u64 })
    }
}

/// A point-in-time, revertible harness state: the AHE "file-level representation".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessBundle {
    pub schema: u64,
    pub id: String,
    pub parent: Option<String>,
    pub created: String,
    pub round: u32,
    pub variant: Variant,
    pub components: Vec<ComponentEntry>,
}

impl HarnessBundle {
    pub fn build(files: &BTreeMap<String, String>, parent: Option<&str>, round: u32, variant: Variant) -> Self {
        let components: Vec<ComponentEntry> = files.iter().filter_map(|(path, body)| ComponentEntry::from_body(path, body)).collect();
        Self { schema: EVOLVE_SCHEMA, id: files_id(files), parent: parent.map(str::to_string), created: now_rfc3339(), round, variant, components }
    }

    pub fn component(&self, path: &str) -> Option<&ComponentEntry> {
        self.components.iter().find(|entry| entry.path == path)
    }

    pub fn has_component(&self, path: &str) -> bool {
        self.component(path).is_some()
    }
}

/// Task-family routing (HarnessX variant isolation): one evolving harness per family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    #[default]
    Code,
    Research,
    Ops,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Code => "code",
            Variant::Research => "research",
            Variant::Ops => "ops",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw.to_ascii_lowercase().as_str() {
            "code" | "coding" | "edit" => Variant::Code,
            "research" | "read" | "explain" | "analysis" => Variant::Research,
            "ops" | "shell" | "deploy" | "infra" => Variant::Ops,
            _ => return None,
        })
    }
}

/// Evaluation split: `Train` drives diagnosis, `Dev` gates durability, `Test` is read once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Train,
    Dev,
    Test,
}

impl Split {
    pub fn as_str(self) -> &'static str {
        match self {
            Split::Train => "train",
            Split::Dev => "dev",
            Split::Test => "test",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "train" => Some(Split::Train),
            "dev" => Some(Split::Dev),
            "test" => Some(Split::Test),
            _ => None,
        }
    }
}

/// Per-edit verdict computed at the start of the next round (AHE decision observability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchedVerdict {
    Pending,
    Confirmed,
    Partial,
    Refuted,
    Regressed,
}

impl PatchedVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            PatchedVerdict::Pending => "pending",
            PatchedVerdict::Confirmed => "confirmed",
            PatchedVerdict::Partial => "partial",
            PatchedVerdict::Refuted => "refuted",
            PatchedVerdict::Regressed => "regressed",
        }
    }
}

/// A pointer back to the raw trajectory that motivates an edit (drill-down evidence).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub session_id: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<u64>,
    pub signal: String,
}

impl EvidenceRef {
    pub fn new(session_id: &str, task_id: &str, signal: &str) -> Self {
        Self { session_id: session_id.to_string(), task_id: task_id.to_string(), event_seq: None, signal: signal.to_string() }
    }
}

/// Normalized failure classes used for clustering and JIT repair triggers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSignal {
    ToolError,
    VerifyFail,
    NoAnswer,
    BudgetExceeded,
    Timeout,
    ApprovalDenied,
    ParseError,
    ContextOverflow,
    Other(String),
}

impl FailureSignal {
    pub fn as_str(&self) -> &str {
        match self {
            FailureSignal::ToolError => "tool_error",
            FailureSignal::VerifyFail => "verify_fail",
            FailureSignal::NoAnswer => "no_answer",
            FailureSignal::BudgetExceeded => "budget_exceeded",
            FailureSignal::Timeout => "timeout",
            FailureSignal::ApprovalDenied => "approval_denied",
            FailureSignal::ParseError => "parse_error",
            FailureSignal::ContextOverflow => "context_overflow",
            FailureSignal::Other(raw) => raw.as_str(),
        }
    }

    /// Classifies a free-form failure text emitted by the harness or verify stages.
    pub fn classify(raw: &str) -> Self {
        let lower = raw.to_ascii_lowercase();
        let has = |needle: &str| lower.contains(needle);
        if has("context length") || has("context window") || has("token limit") || has("compaction") {
            FailureSignal::ContextOverflow
        } else if has("timed out") || has("timeout") || has("deadline") {
            FailureSignal::Timeout
        } else if has("budget") || has("max-cost") || has("max_cost") {
            FailureSignal::BudgetExceeded
        } else if has("denied") || has("permission") {
            FailureSignal::ApprovalDenied
        } else if has("json") || has("parse") || has("schema") || has("invalid tool") {
            FailureSignal::ParseError
        } else if has("no answer") || has("empty answer") || has("missing answer") {
            FailureSignal::NoAnswer
        } else if has("verify") || has("typecheck") || has("lint") || has("test failed") {
            FailureSignal::VerifyFail
        } else if has("tool") || has("enoent") || has("not found") || has("exit code") {
            FailureSignal::ToolError
        } else {
            FailureSignal::Other(normalize_signature(raw))
        }
    }
}

/// One consolidated evidence item: the layer an evolving agent actually consumes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub id: String,
    pub tier: u8,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_hint: Option<String>,
    pub signal: FailureSignal,
    pub occurrences: usize,
    pub tasks: Vec<String>,
    pub sessions: Vec<String>,
    pub excerpt: String,
}

/// Layered evidence corpus (AHE experience observability): L1 step index, L2 clusters, L3 items.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EvidenceCorpus {
    pub schema: u64,
    pub created: String,
    pub bundle: Option<String>,
    pub sessions_scanned: usize,
    pub sessions_skipped: usize,
    pub items: Vec<EvidenceItem>,
    pub tokens_estimate: u64,
    pub truncated: bool,
}

impl EvidenceCorpus {
    pub fn empty() -> Self {
        Self { schema: EVOLVE_SCHEMA, created: now_rfc3339(), ..Default::default() }
    }

    pub fn total_occurrences(&self) -> usize {
        self.items.iter().map(|item| item.occurrences).sum()
    }

    pub fn top(&self, limit: usize) -> Vec<&EvidenceItem> {
        let mut items: Vec<&EvidenceItem> = self.items.iter().collect();
        items.sort_by(|a, b| b.occurrences.cmp(&a.occurrences).then_with(|| a.signature.cmp(&b.signature)));
        items.truncate(limit);
        items
    }
}

/// Result of one evaluated task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub task_id: String,
    pub passed: bool,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Aggregate of one evaluation pass over one split (HarnessX "seesaw" input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub schema: u64,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Variant>,
    pub split: Split,
    pub created: String,
    pub outcomes: Vec<TaskOutcome>,
}

impl RunSummary {
    pub fn new(split: Split, bundle: Option<String>, variant: Option<Variant>, outcomes: Vec<TaskOutcome>) -> Self {
        let id = short_hash(format!("{}:{}:{}", split.as_str(), bundle.clone().unwrap_or_default(), outcomes.len()).as_bytes());
        Self { schema: EVOLVE_SCHEMA, run_id: id, bundle, variant, split, created: now_rfc3339(), outcomes }
    }

    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|outcome| outcome.passed).count()
    }

    pub fn total(&self) -> usize {
        self.outcomes.len()
    }

    pub fn pass_rate(&self) -> f64 {
        if self.outcomes.is_empty() {
            0.0
        } else {
            self.passed() as f64 / self.outcomes.len() as f64
        }
    }

    pub fn passed_ids(&self) -> std::collections::BTreeSet<String> {
        self.outcomes.iter().filter(|outcome| outcome.passed).map(|outcome| outcome.task_id.clone()).collect()
    }

    pub fn failed_ids(&self) -> std::collections::BTreeSet<String> {
        self.outcomes.iter().filter(|outcome| !outcome.passed).map(|outcome| outcome.task_id.clone()).collect()
    }

    pub fn tokens(&self) -> u64 {
        self.outcomes.iter().map(|outcome| outcome.tokens).sum()
    }

    pub fn outcome(&self, task_id: &str) -> Option<&TaskOutcome> {
        self.outcomes.iter().find(|outcome| outcome.task_id == task_id)
    }
}

/// Compact score carried by an EvoDAG node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub passed: usize,
    pub total: usize,
}

impl Score {
    pub fn from_run(run: &RunSummary) -> Self {
        Self { passed: run.passed(), total: run.total() }
    }

    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f64 / self.total as f64
        }
    }
}

/// AutoSaddler EvoDAG node: harness snapshot + lessons + measured scores.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DagNode {
    pub id: String,
    pub bundle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub round: u32,
    pub train: Score,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<Score>,
    #[serde(default)]
    pub lessons: Vec<String>,
    pub created: String,
}

impl DagNode {
    pub fn new(bundle: &str, parent: Option<&str>, round: u32, train: Score, dev: Option<Score>, lessons: Vec<String>) -> Self {
        Self {
            id: short_hash(format!("{bundle}:{round}:{}", parent.unwrap_or("-")).as_bytes()),
            bundle: bundle.to_string(),
            parent: parent.map(str::to_string),
            round,
            train,
            dev,
            lessons,
            created: now_rfc3339(),
        }
    }
}

/// Decision observability payload: every edit must state what it expects to fix and what may break.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    #[serde(default)]
    pub expected_fix: Vec<String>,
    #[serde(default)]
    pub at_risk: Vec<String>,
    #[serde(default)]
    pub expected_gain_pp: f64,
    #[serde(default)]
    pub confidence: f64,
}

/// One manifest entry = one falsifiable contract about one component.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    pub round: u32,
    pub component: String,
    pub component_kind: ComponentKind,
    pub patch_kind: crate::evolve::patch::PatchKind,
    pub root_cause: String,
    pub fix: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    pub prediction: Prediction,
    pub verdict: PatchedVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_gain_pp: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One evolution round's durable record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeManifest {
    pub schema: u64,
    pub round: u32,
    pub base: String,
    pub candidate: String,
    pub created: String,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    pub before: Score,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_before: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_after: Option<Score>,
    pub entries: Vec<ManifestEntry>,
}

impl ChangeManifest {
    pub fn verdicts(&self) -> BTreeMap<String, PatchedVerdict> {
        self.entries.iter().map(|entry| (entry.id.clone(), entry.verdict)).collect()
    }
}

/// Acceptance decision (HarnessX seesaw + AutoSaddler dev gate).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    Accept,
    Reject { reason: String },
}

impl GateDecision {
    pub fn accepted(&self) -> bool {
        matches!(self, GateDecision::Accept)
    }

    pub fn reject(reason: impl Into<String>) -> Self {
        GateDecision::Reject { reason: reason.into() }
    }
}

/// Runtime configuration for the evolution subsystem (`[evolve]` in config.json).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvolveConfig {
    pub enabled: bool,
    #[serde(default)]
    pub rounds: u32,
    #[serde(default = "one")]
    pub k: u32,
    #[serde(default)]
    pub max_tasks: usize,
    #[serde(default)]
    pub token_budget: u64,
    #[serde(default = "yes")]
    pub phased_patches: bool,
    #[serde(default = "yes")]
    pub require_dev_gate: bool,
    #[serde(default = "yes")]
    pub jit: bool,
    #[serde(default = "max_regress")]
    pub max_regress: usize,
    #[serde(default)]
    pub min_gain_pp: f64,
    #[serde(default)]
    pub eval: Option<PathBuf>,
    /// Active bundle id or snapshot path applied to a normal run (`--bundle` / `GOON_HARNESS_BUNDLE`).
    #[serde(default)]
    pub bundle: Option<String>,
}

fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn max_regress() -> usize {
    1
}

impl Default for EvolveConfig {
    fn default() -> Self {
        Self { enabled: false, rounds: 1, k: 1, max_tasks: 10, token_budget: 0, phased_patches: true, require_dev_gate: true, jit: true, max_regress: 1, min_gain_pp: 0.0, eval: None, bundle: None }
    }
}

/// Root of the evolution state directory under the CLI data dir.
pub fn default_root(data_dir: &Path) -> PathBuf {
    data_dir.join("harness")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_collapses_volatiles() {
        assert_eq!(normalize_signature("Error at /tmp/x/1234.rs:55"), "error at <var>");
        let a = normalize_signature("test failed: 42 expected");
        let b = normalize_signature("test failed: 43 expected");
        assert_eq!(a, b);
    }

    #[test]
    fn component_kinds_resolve_from_paths() {
        assert_eq!(component_kind_for_path("system.md"), Some(ComponentKind::SystemPrompt));
        assert_eq!(component_kind_for_path("skills/rust.md"), Some(ComponentKind::Skill));
        assert_eq!(component_kind_for_path("skills/rust.txt"), None);
        assert_eq!(component_kind_for_path("policy/verify.json"), Some(ComponentKind::VerifyPolicy));
        assert_eq!(component_kind_for_path("eval/tasks.json"), None);
    }

    #[test]
    fn bundle_id_is_deterministic_and_order_insensitive() {
        let mut a = BTreeMap::new();
        a.insert("system.md".to_string(), "s".to_string());
        a.insert("middleware.md".to_string(), "m".to_string());
        let mut b = BTreeMap::new();
        b.insert("middleware.md".to_string(), "m".to_string());
        b.insert("system.md".to_string(), "s".to_string());
        assert_eq!(files_id(&a), files_id(&b));
        let bundle = HarnessBundle::build(&a, None, 0, Variant::Code);
        assert_eq!(bundle.components.len(), 2);
        assert!(bundle.has_component("system.md"));
    }

    #[test]
    fn run_summary_counts() {
        let outcomes = vec![
            TaskOutcome { task_id: "a".into(), passed: true, score: 1.0, tokens: 10, duration_ms: 1, failure: None, session_id: None, detail: None },
            TaskOutcome { task_id: "b".into(), passed: false, score: 0.0, tokens: 20, duration_ms: 2, failure: Some(FailureSignal::ToolError), session_id: None, detail: None },
        ];
        let run = RunSummary::new(Split::Train, Some("x".into()), Some(Variant::Code), outcomes);
        assert_eq!(run.passed(), 1);
        assert_eq!(run.total(), 2);
        assert_eq!(run.tokens(), 30);
        assert!(run.passed_ids().contains("a"));
        assert!(run.failed_ids().contains("b"));
    }

    #[test]
    fn failure_signal_classifies() {
        assert_eq!(FailureSignal::classify("context length exceeded"), FailureSignal::ContextOverflow);
        assert_eq!(FailureSignal::classify("verify failed: typecheck"), FailureSignal::VerifyFail);
        assert_eq!(FailureSignal::classify("permission denied"), FailureSignal::ApprovalDenied);
    }
}
