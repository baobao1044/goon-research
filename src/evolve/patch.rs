//! Targeted modification (AutoSaddler): a bounded, typed patch language over bundle components.
//! A patch can never rewrite a whole file, never touch the verifier/policy/eval code, and every
//! operation must cite evidence. Phased scheduling keeps capability edits ahead of prose edits.
#![allow(dead_code)]

use crate::evolve::types::*;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// AutoSaddler patch taxonomy (9 subtypes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchKind {
    NewTool,
    ToolArgMod,
    ToolImplFix,
    ToolDescFix,
    PreToolUseHook,
    PromptRuleAdd,
    PromptRuleMod,
    InfraChange,
    LoopLogic,
    SkillAdd,
    SkillMod,
    MemoryPolicy,
    ContextPolicy,
}

/// Capability patches change what the harness can do; steering patches change how it is told to act.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchClass {
    Capability,
    Steering,
}

impl PatchKind {
    /// Every kind, in taxonomy order (used by scheduling and tests).
    pub fn all() -> [PatchKind; 13] {
        [
            PatchKind::NewTool,
            PatchKind::ToolArgMod,
            PatchKind::ToolImplFix,
            PatchKind::ToolDescFix,
            PatchKind::PreToolUseHook,
            PatchKind::PromptRuleAdd,
            PatchKind::PromptRuleMod,
            PatchKind::InfraChange,
            PatchKind::LoopLogic,
            PatchKind::SkillAdd,
            PatchKind::SkillMod,
            PatchKind::MemoryPolicy,
            PatchKind::ContextPolicy,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PatchKind::NewTool => "new_tool",
            PatchKind::ToolArgMod => "tool_arg_mod",
            PatchKind::ToolImplFix => "tool_impl_fix",
            PatchKind::ToolDescFix => "tool_desc_fix",
            PatchKind::PreToolUseHook => "pre_tool_use_hook",
            PatchKind::PromptRuleAdd => "prompt_rule_add",
            PatchKind::PromptRuleMod => "prompt_rule_mod",
            PatchKind::InfraChange => "infra_change",
            PatchKind::LoopLogic => "loop_logic",
            PatchKind::SkillAdd => "skill_add",
            PatchKind::SkillMod => "skill_mod",
            PatchKind::MemoryPolicy => "memory_policy",
            PatchKind::ContextPolicy => "context_policy",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match normalized.as_str() {
            "new_tool" => PatchKind::NewTool,
            "tool_arg_mod" => PatchKind::ToolArgMod,
            "tool_impl_fix" => PatchKind::ToolImplFix,
            "tool_desc_fix" => PatchKind::ToolDescFix,
            "pre_tool_use_hook" => PatchKind::PreToolUseHook,
            "prompt_rule_add" => PatchKind::PromptRuleAdd,
            "prompt_rule_mod" => PatchKind::PromptRuleMod,
            "infra_change" => PatchKind::InfraChange,
            "loop_logic" => PatchKind::LoopLogic,
            "skill_add" => PatchKind::SkillAdd,
            "skill_mod" => PatchKind::SkillMod,
            "memory_policy" => PatchKind::MemoryPolicy,
            "context_policy" => PatchKind::ContextPolicy,
            _ => return None,
        })
    }

    pub fn class(self) -> PatchClass {
        match self {
            PatchKind::NewTool | PatchKind::ToolArgMod | PatchKind::ToolImplFix | PatchKind::PreToolUseHook | PatchKind::InfraChange | PatchKind::LoopLogic => PatchClass::Capability,
            PatchKind::ToolDescFix | PatchKind::PromptRuleAdd | PatchKind::PromptRuleMod | PatchKind::SkillAdd | PatchKind::SkillMod => PatchClass::Steering,
            PatchKind::MemoryPolicy | PatchKind::ContextPolicy => PatchClass::Capability,
        }
    }

    pub fn is_structural(self) -> bool {
        matches!(self.class(), PatchClass::Capability)
    }
}

/// The only edit primitives an evolution agent may emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchAction {
    /// Append a `## heading` section at end of file.
    AppendSection,
    /// Replace the body between `<!-- id:target -->` and `<!-- /id:target -->` markers.
    ReplaceBlock,
    /// Add one bullet under a `## target` heading.
    InsertBullet,
    /// Set a scalar in a JSON policy component.
    SetParam,
    /// Create `skills/<name>.md`.
    AddSkill,
    /// Remove `skills/<name>.md`.
    RemoveSkill,
}

impl PatchAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PatchAction::AppendSection => "append_section",
            PatchAction::ReplaceBlock => "replace_block",
            PatchAction::InsertBullet => "insert_bullet",
            PatchAction::SetParam => "set_param",
            PatchAction::AddSkill => "add_skill",
            PatchAction::RemoveSkill => "remove_skill",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match normalized.as_str() {
            "append_section" => PatchAction::AppendSection,
            "replace_block" => PatchAction::ReplaceBlock,
            "insert_bullet" => PatchAction::InsertBullet,
            "set_param" => PatchAction::SetParam,
            "add_skill" => PatchAction::AddSkill,
            "remove_skill" => PatchAction::RemoveSkill,
            _ => return None,
        })
    }
}

/// One concrete edit operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchOp {
    pub action: PatchAction,
    pub component: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub content: String,
}

/// A named, evidence-backed change proposal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetedPatch {
    pub kind: PatchKind,
    pub rationale: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub ops: Vec<PatchOp>,
    #[serde(default)]
    pub prediction: Prediction,
}

impl TargetedPatch {
    pub fn class(&self) -> PatchClass {
        self.kind.class()
    }

    /// Component the patch mostly touches: the first op's component, or `(multi)`.
    pub fn primary_component(&self) -> String {
        let Some(first_op) = self.ops.first() else { return "(multi)".to_string() };
        let first = first_op.component.trim();
        let mixed = self.ops.iter().any(|other| other.component.trim() != first);
        if first.is_empty() || mixed {
            "(multi)".to_string()
        } else {
            first.to_string()
        }
    }
}

/// Write allowlist/denylist for evolution edits (AHE controllability).
#[derive(Clone, Debug)]
pub struct EditScope {
    pub allow_prefixes: Vec<String>,
    pub deny_prefixes: Vec<String>,
}

impl EditScope {
    pub fn default_for_bundle() -> Self {
        Self {
            allow_prefixes: vec![BUNDLE_DIR.to_string()],
            deny_prefixes: vec!["src/verify.rs".into(), "src/policy.rs".into(), "src/config.rs".into(), "eval/".into(), "tests/".into(), "sessions/".into()],
        }
    }

    /// Rejects absolute paths, traversal, denied prefixes and non-component files.
    ///
    /// Paths are accepted in either addressing form the bundle uses: workspace relative
    /// (`.goon/harness/system.md`) or bundle relative (`system.md`).
    pub fn check(&self, path: &str) -> Result<()> {
        let normalized = normalize_path(path)?;
        let (workspace_rel, component_rel) = self.resolve(&normalized)?;
        for deny in &self.deny_prefixes {
            let deny = normalize_prefix(deny);
            if deny.is_empty() {
                continue;
            }
            if under_prefix(&workspace_rel, &deny) || under_prefix(&component_rel, &deny) || under_prefix(&normalized, &deny) {
                bail!("edit scope: '{}' is under denied prefix '{}'", path.trim(), deny);
            }
        }
        if component_kind_for_path(&component_rel).is_none() && !is_policy_json(&component_rel) {
            bail!("edit scope: '{}' is not an editable harness component", path.trim());
        }
        Ok(())
    }

    /// Resolves an accepted path to `(workspace relative, component relative)`.
    fn resolve(&self, path: &str) -> Result<(String, String)> {
        for allow in &self.allow_prefixes {
            let allow = normalize_prefix(allow);
            if allow.is_empty() {
                continue;
            }
            if under_prefix(path, &allow) {
                let rel = path.strip_prefix(&allow).unwrap_or("").trim_start_matches('/').to_string();
                return Ok((path.to_string(), rel));
            }
        }
        // Bundle-relative addressing: the first allow root is treated as the bundle root.
        if let Some(allow) = self.allow_prefixes.iter().map(|allow| normalize_prefix(allow)).find(|allow| !allow.is_empty()) {
            return Ok((format!("{allow}/{path}"), path.to_string()));
        }
        bail!("edit scope: no allowlist configured, refusing every path")
    }
}

pub const MAX_OPS: usize = 8;
pub const MAX_GROWTH_BYTES: usize = 8192;
pub const MAX_REPLACED_RATIO: f64 = 0.5;
/// Hard cap of patches considered in one round (phased scheduling budget).
pub const MAX_PATCHES_PER_ROUND: usize = 3;

/// Report of one applied patch.
#[derive(Clone, Debug, Default)]
pub struct PatchReport {
    pub files_touched: usize,
    pub bytes_added: usize,
    pub ops_applied: usize,
}

/// Applies one patch to an in-memory component map; rejects out-of-scope or oversized edits.
pub fn apply(files: &mut BTreeMap<String, String>, patch: &TargetedPatch, scope: &EditScope) -> Result<PatchReport> {
    if patch.ops.len() > MAX_OPS {
        bail!("patch has {} ops, limit is {MAX_OPS}", patch.ops.len());
    }
    let mut staged = files.clone();
    let mut report = PatchReport::default();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for (index, op) in patch.ops.iter().enumerate() {
        let (key, before_len, after_len) = apply_op(&mut staged, op, scope).with_context(|| format!("op #{} ({})", index + 1, op.action.as_str()))?;
        touched.insert(key);
        if after_len > before_len {
            report.bytes_added += after_len - before_len;
        }
        report.ops_applied += 1;
        if report.bytes_added > MAX_GROWTH_BYTES {
            bail!("patch grows the bundle by {} bytes, limit is {MAX_GROWTH_BYTES}", report.bytes_added);
        }
    }
    report.files_touched = touched.len();
    *files = staged;
    Ok(report)
}

/// Applies a batch; later patches see earlier results and the whole batch is atomic.
pub fn apply_all(files: &mut BTreeMap<String, String>, patches: &[TargetedPatch], scope: &EditScope) -> Result<Vec<PatchReport>> {
    let mut staged = files.clone();
    let mut reports: Vec<PatchReport> = Vec::with_capacity(patches.len());
    for (index, patch) in patches.iter().enumerate() {
        let report = apply(&mut staged, patch, scope).with_context(|| format!("patch #{} ({}) failed", index + 1, patch.kind.as_str()))?;
        reports.push(report);
    }
    *files = staged;
    Ok(reports)
}

/// Parses model output (raw JSON array/object, fenced blocks tolerated) into patches.
pub fn parse(raw: &str) -> Result<Vec<TargetedPatch>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("patch payload is empty");
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut patches: Vec<TargetedPatch> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut parsed_any = false;
    for candidate in json_candidates(trimmed) {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        match collect_patches(&candidate) {
            Ok((found, notes)) => {
                parsed_any = true;
                patches.extend(found);
                skipped.extend(notes);
            }
            Err(err) => skipped.push(err.to_string()),
        }
        if !patches.is_empty() {
            // First candidate that yields patches wins: trailing prose may repeat the payload.
            break;
        }
    }
    if !patches.is_empty() {
        return Ok(patches);
    }
    if !parsed_any {
        bail!("model output has no JSON patch payload");
    }
    bail!("model output has no valid patch ({})", skipped.join("; "))
}

/// Orders patches for a round: capability before steering, hard cap per round.
pub fn phased(patches: &[TargetedPatch], round: u32, max_rounds: u32, phased: bool) -> Vec<usize> {
    let limit = MAX_PATCHES_PER_ROUND;
    if !phased {
        return (0..patches.len().min(limit)).collect();
    }
    let mut capability: Vec<usize> = Vec::new();
    let mut steering: Vec<usize> = Vec::new();
    for (index, patch) in patches.iter().enumerate() {
        match patch.class() {
            PatchClass::Capability => {
                if capability.len() < limit {
                    capability.push(index);
                }
            }
            PatchClass::Steering => {
                if steering.len() < limit {
                    steering.push(index);
                }
            }
        }
    }
    // Early rounds may only change capability (structure first, prose later).
    if round <= max_rounds / 2 {
        return capability;
    }
    capability.extend(steering);
    capability.truncate(limit);
    capability
}

/// Serializes patches back to JSON (for manifests and dry-run output).
pub fn to_json(patches: &[TargetedPatch]) -> String {
    serde_json::to_string_pretty(patches).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------------------------
// path scoping
// ---------------------------------------------------------------------------------------------

/// Normalizes a relative path: `\` -> `/`, drops `.` segments, rejects absolute and traversal.
fn normalize_path(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("edit scope: empty path");
    }
    let slashed = raw.replace('\\', "/");
    if slashed.starts_with('/') {
        bail!("edit scope: absolute path rejected: {raw}");
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in slashed.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("edit scope: path traversal rejected: {raw}"),
            other => {
                if other.contains(':') {
                    bail!("edit scope: drive-qualified path rejected: {raw}");
                }
                parts.push(other);
            }
        }
    }
    if parts.is_empty() {
        bail!("edit scope: empty path");
    }
    Ok(parts.join("/"))
}

/// Normalizes a prefix, keeping an unusable prefix as an empty string.
fn normalize_prefix(raw: &str) -> String {
    match normalize_path(raw) {
        Ok(prefix) => prefix,
        Err(_) => raw.replace('\\', "/").trim_matches('/').to_string(),
    }
}

/// True when `path` equals `prefix` or lives inside it.
fn under_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    if path == prefix {
        return true;
    }
    path.strip_prefix(prefix).map(|rest| rest.starts_with('/')).unwrap_or(false)
}

/// Any `policy/*.json` file is a valid policy component even without a fixed kind.
fn is_policy_json(path: &str) -> bool {
    match path.strip_prefix("policy/") {
        Some(rest) => rest.ends_with(".json") && rest.len() > ".json".len(),
        None => false,
    }
}

/// Bundle-relative form of an already accepted path.
fn bundle_rel(raw: &str) -> Result<String> {
    let normalized = normalize_path(raw)?;
    let prefix = format!("{BUNDLE_DIR}/");
    Ok(normalized.strip_prefix(&prefix).unwrap_or(&normalized).to_string())
}

/// Key used by the component map, accepting either addressing form.
fn component_key(files: &BTreeMap<String, String>, raw: &str, scope: &EditScope) -> Result<String> {
    if raw.trim().is_empty() {
        bail!("op has no component");
    }
    scope.check(raw)?;
    let rel = bundle_rel(raw)?;
    if files.contains_key(&rel) {
        return Ok(rel);
    }
    let prefixed = format!("{BUNDLE_DIR}/{rel}");
    if files.contains_key(&prefixed) {
        return Ok(prefixed);
    }
    bail!("component not found in bundle: '{}'", raw.trim())
}

/// Key for a file that does not exist yet, matching the map's addressing convention.
fn new_key(files: &BTreeMap<String, String>, rel: &str) -> String {
    let prefix = format!("{BUNDLE_DIR}/");
    if files.keys().any(|key| key.starts_with(&prefix)) {
        format!("{prefix}{rel}")
    } else {
        rel.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------------------------

/// Applies one op, returning `(key, bytes before, bytes after)`.
fn apply_op(files: &mut BTreeMap<String, String>, op: &PatchOp, scope: &EditScope) -> Result<(String, usize, usize)> {
    match op.action {
        PatchAction::AddSkill => add_skill(files, op, scope),
        PatchAction::RemoveSkill => remove_skill(files, op, scope),
        PatchAction::SetParam => set_param(files, op, scope),
        PatchAction::AppendSection | PatchAction::ReplaceBlock | PatchAction::InsertBullet => {
            let key = component_key(files, &op.component, scope)?;
            let body = files.get(&key).cloned().unwrap_or_default();
            let edited = match op.action {
                PatchAction::AppendSection => append_section(&body, op.target.as_deref().unwrap_or(""), &op.content)?,
                PatchAction::ReplaceBlock => replace_block(&body, op.target.as_deref().unwrap_or(""), &op.content)?,
                PatchAction::InsertBullet => insert_bullet(&body, op.target.as_deref().unwrap_or(""), &op.content)?,
                _ => unreachable!("guarded by the outer match"),
            };
            let before = body.len();
            let after = edited.len();
            files.insert(key.clone(), edited);
            Ok((key, before, after))
        }
    }
}

fn append_section(body: &str, target: &str, content: &str) -> Result<String> {
    let target = target.trim();
    if target.is_empty() {
        bail!("append_section requires a target heading");
    }
    if content.trim().is_empty() {
        bail!("append_section requires content");
    }
    let mut out = body.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("## ");
    out.push_str(target);
    out.push_str("\n\n");
    out.push_str(content.trim());
    out.push('\n');
    Ok(out)
}

fn replace_block(body: &str, target: &str, content: &str) -> Result<String> {
    let target = target.trim();
    if target.is_empty() {
        bail!("replace_block requires a target block id");
    }
    let open = format!("<!-- id:{target} -->");
    let close = format!("<!-- /id:{target} -->");
    let start = body.find(&open).ok_or_else(|| anyhow!("missing block marker '{open}'"))?;
    let inner_start = start + open.len();
    let tail = &body[inner_start..];
    let end = tail.find(&close).ok_or_else(|| anyhow!("missing block marker '{close}'"))?;
    let replaced = &tail[..end];
    if replaced.len() as f64 > body.len() as f64 * MAX_REPLACED_RATIO {
        bail!("replace_block rewrites {} of {} bytes (limit {:.0}%)", replaced.len(), body.len(), MAX_REPLACED_RATIO * 100.0);
    }
    let mut out = String::with_capacity(body.len() + content.len());
    out.push_str(&body[..inner_start]);
    out.push('\n');
    let payload = content.trim_matches('\n');
    if !payload.is_empty() {
        out.push_str(payload.trim_end());
        out.push('\n');
    }
    out.push_str(&tail[end..]);
    Ok(out)
}

fn insert_bullet(body: &str, target: &str, content: &str) -> Result<String> {
    let target = target.trim();
    if target.is_empty() {
        bail!("insert_bullet requires a target heading");
    }
    if content.trim().is_empty() {
        bail!("insert_bullet requires content");
    }
    let bullet = format!("- {}", content.trim());
    let heading = format!("## {target}");
    let Some(position) = body.find(&heading) else {
        // No heading yet: create it, then the bullet under it.
        let mut out = body.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&heading);
        out.push_str("\n\n");
        out.push_str(&bullet);
        out.push('\n');
        return Ok(out);
    };
    let line_end = body[position..].find('\n').map(|offset| position + offset + 1).unwrap_or(body.len());
    let rest = &body[line_end..];
    let boundary = rest.find("\n## ");
    let (section, suffix) = match boundary {
        Some(offset) => (&rest[..offset], Some(&rest[offset..])),
        None => (rest, None),
    };
    let trimmed_section = section.trim_end();
    let mut out = String::with_capacity(body.len() + bullet.len() + 8);
    out.push_str(&body[..line_end]);
    if !trimmed_section.is_empty() {
        out.push_str(trimmed_section);
        out.push('\n');
    }
    out.push_str(&bullet);
    out.push('\n');
    if let Some(suffix) = suffix {
        out.push('\n');
        out.push_str(suffix.trim_start_matches('\n'));
    }
    Ok(out)
}

fn add_skill(files: &mut BTreeMap<String, String>, op: &PatchOp, scope: &EditScope) -> Result<(String, usize, usize)> {
    if op.content.trim().is_empty() {
        bail!("add_skill requires skill content");
    }
    let rel = skill_rel(op)?;
    scope.check(&rel)?;
    let key = new_key(files, &rel);
    if files.contains_key(&key) {
        bail!("skill already exists: {rel}");
    }
    let mut body = op.content.trim().to_string();
    body.push('\n');
    let after = body.len();
    files.insert(key.clone(), body);
    Ok((key, 0, after))
}

fn remove_skill(files: &mut BTreeMap<String, String>, op: &PatchOp, scope: &EditScope) -> Result<(String, usize, usize)> {
    let rel = skill_rel(op)?;
    scope.check(&rel)?;
    let key = if files.contains_key(&rel) { rel.clone() } else { format!("{BUNDLE_DIR}/{rel}") };
    let before = files.remove(&key).map(|body| body.len()).ok_or_else(|| anyhow!("skill not found: {rel}"))?;
    Ok((key, before, 0))
}

/// `skills/<name>.md` for an add/remove skill op.
fn skill_rel(op: &PatchOp) -> Result<String> {
    let raw = match op.target.as_deref().map(str::trim).filter(|target| !target.is_empty()) {
        Some(target) => target,
        None => op.component.trim(),
    };
    if raw.is_empty() {
        bail!("skill op requires a target skill name");
    }
    let normalized = normalize_path(raw)?;
    let prefix = format!("{SKILLS_DIR}/");
    let stem = normalized.strip_prefix(&prefix).unwrap_or(&normalized);
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    if stem.is_empty() || stem.contains('/') {
        bail!("skill name must be a single segment, got '{raw}'");
    }
    Ok(format!("{prefix}{stem}.md"))
}

fn set_param(files: &mut BTreeMap<String, String>, op: &PatchOp, scope: &EditScope) -> Result<(String, usize, usize)> {
    let key = component_key(files, &op.component, scope)?;
    let rel = bundle_rel(&key)?;
    let policy_kind = matches!(component_kind_for_path(&rel), Some(ComponentKind::VerifyPolicy | ComponentKind::ContextPolicy | ComponentKind::JitPolicy));
    if !policy_kind && !is_policy_json(&rel) {
        bail!("set_param requires a JSON policy component, got '{rel}'");
    }
    let target = op.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        bail!("set_param requires a target key path");
    }
    let raw = files.get(&key).cloned().unwrap_or_default();
    let mut root: Value = if raw.trim().is_empty() { Value::Object(Map::new()) } else { serde_json::from_str(&raw).with_context(|| format!("policy component '{rel}' is not valid JSON"))? };
    if !root.is_object() {
        bail!("policy component '{rel}' must be a JSON object");
    }
    let value = match serde_json::from_str::<Value>(&op.content) {
        Ok(value) => value,
        Err(_) => Value::String(op.content.clone()),
    };
    let segments: Vec<&str> = target.split('.').map(str::trim).collect();
    if segments.len() > 32 {
        bail!("set_param target '{target}' is too deep");
    }
    set_json_path(&mut root, &segments, value)?;
    let rendered = format!("{}\n", serde_json::to_string_pretty(&root)?);
    let before = raw.len();
    let after = rendered.len();
    files.insert(key.clone(), rendered);
    Ok((key, before, after))
}

/// Recursive object walk: creates missing intermediate objects, then writes the leaf.
fn set_json_path(node: &mut Value, segments: &[&str], value: Value) -> Result<()> {
    let Some((head, tail)) = segments.split_first() else { bail!("set_param requires a target key path") };
    if head.is_empty() {
        bail!("set_param target has an empty segment");
    }
    let map = node.as_object_mut().ok_or_else(|| anyhow!("set_param target segment '{head}' is not a JSON object"))?;
    if tail.is_empty() {
        map.insert((*head).to_string(), value);
        return Ok(());
    }
    let child = map.entry((*head).to_string()).or_insert_with(|| Value::Object(Map::new()));
    set_json_path(child, tail, value)
}

// ---------------------------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------------------------

/// Candidate JSON payloads, most specific first.
fn json_candidates(text: &str) -> Vec<String> {
    let mut candidates: Vec<String> = vec![text.to_string()];
    for block in fenced_blocks(text) {
        let block = block.trim().to_string();
        if !block.is_empty() {
            candidates.push(block);
        }
    }
    for span in json_spans(text) {
        candidates.push(span);
    }
    candidates
}

/// Contents of every ```json fenced block (plain fences are accepted too).
fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut open: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            match open.take() {
                Some((language, body)) => {
                    if language.is_empty() || language.eq_ignore_ascii_case("json") {
                        blocks.push(body);
                    }
                }
                None => open = Some((info.trim().to_string(), String::new())),
            }
            continue;
        }
        if let Some((_, body)) = open.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    blocks
}

/// Balanced `{...}` / `[...]` spans, string-aware.
fn json_spans(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<String> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if matches!(chars[index], '[' | '{') {
            if let Some(end) = matching_bracket(&chars, index) {
                spans.push(chars[index..=end].iter().collect());
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    spans
}

fn matching_bracket(chars: &[char], start: usize) -> Option<usize> {
    let open = chars[start];
    let close = if open == '[' { ']' } else { '}' };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in chars[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *ch == '\\' {
                escaped = true;
            } else if *ch == '"' {
                in_string = false;
            }
            continue;
        }
        match *ch {
            '"' => in_string = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Converts one JSON payload into patches, collecting per-entry skip reasons.
fn collect_patches(text: &str) -> Result<(Vec<TargetedPatch>, Vec<String>)> {
    let value: Value = serde_json::from_str(text).context("not JSON")?;
    let items: Vec<Value> = match value {
        Value::Array(items) => items,
        Value::Object(map) => match map.get("patches") {
            Some(Value::Array(items)) => items.clone(),
            Some(other) => bail!("'patches' must be an array, got {}", json_kind(other)),
            None => {
                if map.contains_key("kind") || map.contains_key("ops") || map.contains_key("action") {
                    vec![Value::Object(map)]
                } else {
                    bail!("object has neither 'patches' nor patch fields");
                }
            }
        },
        other => bail!("expected a JSON array or object, got {}", json_kind(&other)),
    };
    let mut patches: Vec<TargetedPatch> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match patch_from_value(&item) {
            Ok(Some(patch)) => patches.push(patch),
            Ok(None) => skipped.push(format!("patch #{} skipped: unknown kind", index + 1)),
            Err(err) => skipped.push(format!("patch #{} skipped: {err}", index + 1)),
        }
    }
    Ok((patches, skipped))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Default kind for a patch that omitted `kind`.
fn default_kind() -> PatchKind {
    PatchKind::PromptRuleAdd
}

fn patch_from_value(value: &Value) -> Result<Option<TargetedPatch>> {
    let map = value.as_object().ok_or_else(|| anyhow!("not a JSON object"))?;
    let kind = match map.get("kind").and_then(Value::as_str).map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => match PatchKind::parse(raw) {
            Some(kind) => kind,
            None => return Ok(None),
        },
        None => default_kind(),
    };
    let rationale = map.get("rationale").and_then(Value::as_str).or_else(|| map.get("root_cause").and_then(Value::as_str)).unwrap_or("").trim().to_string();
    let evidence = parse_evidence(map.get("evidence"));
    let prediction = map.get("prediction").and_then(|value| serde_json::from_value::<Prediction>(value.clone()).ok()).unwrap_or_default();
    let ops_raw = map.get("ops").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut ops: Vec<PatchOp> = Vec::new();
    let mut skipped_ops = 0usize;
    for op_value in ops_raw {
        match parse_op(&op_value) {
            Ok(Some(op)) => ops.push(op),
            Ok(None) => skipped_ops += 1,
            Err(_) => skipped_ops += 1,
        }
    }
    if ops.is_empty() {
        return Err(anyhow!("{} unusable op(s), none left", skipped_ops.max(1)));
    }
    Ok(Some(TargetedPatch { kind, rationale, evidence, ops, prediction }))
}

fn parse_op(value: &Value) -> Result<Option<PatchOp>> {
    let map = value.as_object().ok_or_else(|| anyhow!("op is not a JSON object"))?;
    let action_raw = map.get("action").and_then(Value::as_str).unwrap_or("");
    let Some(action) = PatchAction::parse(action_raw) else {
        return Ok(None);
    };
    let component =
        map.get("component").and_then(Value::as_str).or_else(|| map.get("path").and_then(Value::as_str)).or_else(|| map.get("file").and_then(Value::as_str)).unwrap_or("").trim().to_string();
    if component.is_empty() && !matches!(action, PatchAction::AddSkill | PatchAction::RemoveSkill) {
        bail!("op '{}' has no component", action.as_str());
    }
    let target = map.get("target").and_then(Value::as_str).map(str::trim).filter(|target| !target.is_empty()).map(str::to_string);
    let content = map.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Some(PatchOp { action, component, target, content }))
}

fn parse_evidence(value: Option<&Value>) -> Vec<EvidenceRef> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut evidence: Vec<EvidenceRef> = Vec::new();
    for item in items {
        if let Ok(reference) = serde_json::from_value::<EvidenceRef>(item.clone()) {
            evidence.push(reference);
            continue;
        }
        if let Some(task) = item.as_str().map(str::trim).filter(|task| !task.is_empty()) {
            evidence.push(EvidenceRef::new("", task, ""));
        }
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        files.insert("system.md".to_string(), "# System\n\nBe careful and concise.\n".to_string());
        files.insert("skills/rust.md".to_string(), "# Rust\n\nPrefer cargo.\n".to_string());
        files.insert("policy/verify.json".to_string(), "{\n  \"verify\": {\n    \"timeout_ms\": 1000\n  }\n}".to_string());
        files
    }

    fn op(action: PatchAction, component: &str, target: Option<&str>, content: &str) -> PatchOp {
        PatchOp { action, component: component.to_string(), target: target.map(str::to_string), content: content.to_string() }
    }

    fn patch(kind: PatchKind, ops: Vec<PatchOp>) -> TargetedPatch {
        TargetedPatch { kind, rationale: "because evidence".to_string(), evidence: vec![EvidenceRef::new("s1", "t1", "tool_error")], ops, prediction: Prediction::default() }
    }

    #[test]
    fn patch_kind_round_trips_and_classifies() {
        for kind in PatchKind::all() {
            assert_eq!(PatchKind::parse(kind.as_str()), Some(kind), "{}", kind.as_str());
        }
        assert_eq!(PatchKind::parse("Append-Section"), None);
        assert_eq!(PatchKind::parse("tool-impl-fix"), Some(PatchKind::ToolImplFix));
        assert_eq!(PatchKind::all().len(), 13);
        assert_eq!(PatchKind::NewTool.class(), PatchClass::Capability);
        assert_eq!(PatchKind::PromptRuleAdd.class(), PatchClass::Steering);
        assert!(PatchKind::MemoryPolicy.is_structural());
        assert!(!PatchKind::SkillAdd.is_structural());
        for action in [PatchAction::AppendSection, PatchAction::ReplaceBlock, PatchAction::InsertBullet, PatchAction::SetParam, PatchAction::AddSkill, PatchAction::RemoveSkill] {
            assert_eq!(PatchAction::parse(action.as_str()), Some(action));
        }
    }

    #[test]
    fn append_section_adds_heading_and_content() {
        let mut files = batch();
        let scope = EditScope::default_for_bundle();
        let report = apply(&mut files, &patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::AppendSection, "system.md", Some("Tool use"), "Call `read` before `edit`.")]), &scope).expect("apply");
        let body = files.get("system.md").expect("system.md");
        assert!(body.contains("## Tool use"));
        assert!(body.contains("Call `read` before `edit`."));
        assert_eq!(report.ops_applied, 1);
        assert_eq!(report.files_touched, 1);
        assert!(report.bytes_added > 0);
    }

    #[test]
    fn replace_block_requires_markers_and_is_bounded() {
        let scope = EditScope::default_for_bundle();
        let mut files = BTreeMap::new();
        let body = format!("intro\n<!-- id:rules -->\nold\n<!-- /id:rules -->\n{}", "filler text. ".repeat(40));
        files.insert("system.md".to_string(), body);
        let patch_ok = patch(PatchKind::PromptRuleMod, vec![op(PatchAction::ReplaceBlock, "system.md", Some("rules"), "new rule")]);
        apply(&mut files, &patch_ok, &scope).expect("replace");
        let body = files.get("system.md").expect("system.md");
        assert!(body.contains("new rule"));
        assert!(!body.contains("\nold\n"));
        assert!(body.contains("<!-- id:rules -->"));
        assert!(body.contains("<!-- /id:rules -->"));

        let missing = patch(PatchKind::PromptRuleMod, vec![op(PatchAction::ReplaceBlock, "system.md", Some("absent"), "x")]);
        let err = apply(&mut files, &missing, &scope).expect_err("missing marker must fail");
        assert!(format!("{err:#}").contains("missing block marker"));

        // Replacing most of the file is rejected: no whole-file rewrites.
        let mut small = BTreeMap::new();
        small.insert("system.md".to_string(), format!("<!-- id:big -->\n{}\n<!-- /id:big -->\n", "x".repeat(200)));
        let huge = patch(PatchKind::PromptRuleMod, vec![op(PatchAction::ReplaceBlock, "system.md", Some("big"), "y")]);
        let err = apply(&mut small, &huge, &scope).expect_err("ratio guard must fail");
        assert!(format!("{err:#}").contains("rewrites"));
        assert!(small["system.md"].contains(&"x".repeat(200)), "rejected patch must not mutate input");
    }

    #[test]
    fn insert_bullet_appends_under_heading_or_creates_it() {
        let scope = EditScope::default_for_bundle();
        let mut files = BTreeMap::new();
        files.insert("system.md".to_string(), "# System\n\n## Rules\n\n- existing\n\n## Other\n\nkeep me\n".to_string());
        let op_existing = op(PatchAction::InsertBullet, "system.md", Some("Rules"), "read before edit");
        apply(&mut files, &patch(PatchKind::PromptRuleAdd, vec![op_existing]), &scope).expect("insert");
        let body = files.get("system.md").expect("system.md");
        let rules_at = body.find("## Rules").expect("rules heading");
        let other_at = body.find("## Other").expect("other heading");
        let bullet_at = body.find("- read before edit").expect("bullet");
        assert!(bullet_at > rules_at && bullet_at < other_at, "bullet must stay inside its section: {body}");
        assert!(body.contains("- existing"));
        assert!(body.contains("keep me"));

        let op_new = op(PatchAction::InsertBullet, "system.md", Some("Style"), "no emoji");
        apply(&mut files, &patch(PatchKind::PromptRuleAdd, vec![op_new]), &scope).expect("create heading");
        let body = files.get("system.md").expect("system.md");
        assert!(body.contains("## Style"));
        assert!(body.contains("- no emoji"));
    }

    #[test]
    fn set_param_creates_nested_keys_and_types() {
        let scope = EditScope::default_for_bundle();
        let mut files = batch();
        let nested = op(PatchAction::SetParam, "policy/verify.json", Some("verify.retries"), "3");
        apply(&mut files, &patch(PatchKind::MemoryPolicy, vec![nested]), &scope).expect("set nested");
        let deep = op(PatchAction::SetParam, "policy/context.json", Some("budget.window.strategy"), "compact");
        files.insert("policy/context.json".to_string(), "{}".to_string());
        apply(&mut files, &patch(PatchKind::ContextPolicy, vec![deep]), &scope).expect("create policy");
        let verify: Value = serde_json::from_str(files.get("policy/verify.json").expect("verify policy")).expect("policy json");
        assert_eq!(verify["verify"]["retries"], serde_json::json!(3));
        assert_eq!(verify["verify"]["timeout_ms"], serde_json::json!(1000));
        let context: Value = serde_json::from_str(files.get("policy/context.json").expect("context policy")).expect("policy json");
        assert_eq!(context["budget"]["window"]["strategy"], serde_json::json!("compact"));

        let wrong = patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::SetParam, "system.md", Some("a.b"), "1")]);
        let err = apply(&mut files, &wrong, &scope).expect_err("set_param on markdown must fail");
        assert!(format!("{err:#}").contains("JSON policy component"));
    }

    #[test]
    fn add_and_remove_skill() {
        let scope = EditScope::default_for_bundle();
        let mut files = batch();
        apply(&mut files, &patch(PatchKind::SkillAdd, vec![op(PatchAction::AddSkill, "", Some("python"), "Use ruff.")]), &scope).expect("add skill");
        assert!(files.get("skills/python.md").expect("skill").contains("Use ruff."));
        let duplicate = patch(PatchKind::SkillAdd, vec![op(PatchAction::AddSkill, "", Some("python"), "again")]);
        assert!(apply(&mut files, &duplicate, &scope).is_err(), "duplicate add_skill must fail");

        apply(&mut files, &patch(PatchKind::SkillMod, vec![op(PatchAction::RemoveSkill, "skills/python.md", None, "")]), &scope).expect("remove skill");
        assert!(!files.contains_key("skills/python.md"));
        let missing = patch(PatchKind::SkillMod, vec![op(PatchAction::RemoveSkill, "skills/python.md", None, "")]);
        assert!(apply(&mut files, &missing, &scope).is_err(), "removing a missing skill must fail");
    }

    #[test]
    fn scope_rejects_escapes_and_outsiders() {
        let scope = EditScope::default_for_bundle();
        for bad in ["/etc/passwd", "C:\\Windows\\win.ini", "../escape.md", ".goon/harness/../../escape.md", "", "  "] {
            assert!(scope.check(bad).is_err(), "must reject {bad:?}");
        }
        assert!(scope.check("eval/tasks.json").is_err(), "eval/ is denied");
        assert!(scope.check("src/verify.rs").is_err(), "verifier source is denied");
        assert!(scope.check("src/main.rs").is_err(), "outside the allowlist");
        assert!(scope.check("notes.txt").is_err(), "unknown component kind");
        assert!(scope.check("system.md").is_ok());
        assert!(scope.check("./system.md").is_ok());
        assert!(scope.check(".goon\\harness\\skills\\rust.md").is_ok(), "windows separators are normalized");
        assert!(scope.check("policy/custom.json").is_ok(), "policy JSON is addressable");
    }

    #[test]
    fn apply_rejects_too_many_ops_and_unbounded_growth() {
        let scope = EditScope::default_for_bundle();
        let mut files = batch();
        let ops: Vec<PatchOp> = (0..=MAX_OPS).map(|index| op(PatchAction::AppendSection, "system.md", Some(&format!("s{index}")), "x")).collect();
        let err = apply(&mut files, &patch(PatchKind::PromptRuleAdd, ops), &scope).expect_err("op cap");
        assert!(format!("{err:#}").contains("limit is 8"));

        let huge = "a".repeat(MAX_GROWTH_BYTES + 1);
        let err = apply(&mut files, &patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::AppendSection, "system.md", Some("big"), &huge)]), &scope).expect_err("growth cap");
        assert!(format!("{err:#}").contains("limit is 8192"));
        assert_eq!(files["system.md"], "# System\n\nBe careful and concise.\n", "failed patch leaves files untouched");
    }

    #[test]
    fn apply_all_is_atomic_on_failure() {
        let scope = EditScope::default_for_bundle();
        let mut files = batch();
        let before = files.clone();
        let good = patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::AppendSection, "system.md", Some("Good"), "ok")]);
        let bad = patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::AppendSection, "missing.md", Some("Bad"), "nope")]);
        let err = apply_all(&mut files, &[good, bad], &scope).expect_err("batch must fail");
        assert!(format!("{err:#}").contains("patch #2"));
        assert_eq!(files, before, "batch failure must not leave partial edits");

        let good = patch(PatchKind::PromptRuleMod, vec![op(PatchAction::AppendSection, "system.md", Some("A"), "one")]);
        let also_good = patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::InsertBullet, "system.md", Some("A"), "two")]);
        let reports = apply_all(&mut files, &[good, also_good], &scope).expect("batch applies");
        assert_eq!(reports.len(), 2);
        let body = files.get("system.md").expect("system.md");
        assert!(body.contains("## A"));
        assert!(body.contains("- two"));
    }

    #[test]
    fn parse_accepts_array_object_and_fenced_json() {
        let array = r#"[
            {"kind":"prompt_rule_add","rationale":"errors cluster","ops":[{"action":"append_section","component":"system.md","target":"Rules","content":"be brief"}],
             "prediction":{"expected_fix":["t1"]},"evidence":[{"session_id":"s","task_id":"t1","signal":"tool_error"}]}
        ]"#;
        let parsed = parse(array).expect("array");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, PatchKind::PromptRuleAdd);
        assert_eq!(parsed[0].ops.len(), 1);
        assert_eq!(parsed[0].prediction.expected_fix, vec!["t1".to_string()]);
        assert_eq!(parsed[0].evidence.len(), 1);

        let wrapped = r#"{"patches":[{"kind":"skill_add","ops":[{"action":"add_skill","target":"go","content":"go fast"}]}]}"#;
        assert_eq!(parse(wrapped).expect("object").len(), 1);

        let fenced = "Here are the patches:\n```json\n[{\"kind\":\"memory_policy\",\"ops\":[{\"action\":\"set_param\",\"component\":\"policy/verify.json\",\"target\":\"verify.x\",\"content\":\"1\"}]}]\n```\nDone.";
        assert_eq!(parse(fenced).expect("fenced").len(), 1);

        // Missing fields fall back to defaults, evidence may be empty.
        let sparse = r#"[{"ops":[{"action":"append_section","component":"system.md","target":"T","content":"c"}]}]"#;
        let parsed = parse(sparse).expect("defaults");
        assert_eq!(parsed[0].kind, default_kind());
        assert!(parsed[0].evidence.is_empty());
        assert_eq!(parsed[0].ops.len(), 1);
    }

    #[test]
    fn parse_skips_unknown_kinds_but_reports_when_nothing_is_left() {
        let mixed = r#"[{"kind":"time_travel","ops":[{"action":"append_section","component":"system.md","target":"T","content":"c"}]},
                         {"kind":"prompt_rule_add","ops":[{"action":"append_section","component":"system.md","target":"T","content":"c"}]}]"#;
        let parsed = parse(mixed).expect("one valid patch survives");
        assert_eq!(parsed.len(), 1);

        let only_unknown = r#"[{"kind":"time_travel","ops":[{"action":"append_section","component":"system.md","target":"T","content":"c"}]}]"#;
        let err = parse(only_unknown).expect_err("no valid patch");
        assert!(format!("{err:#}").contains("unknown kind"));

        for garbage in ["", "   ", "no json here at all", "{ broken", "[]", "{\"patches\":[]}"] {
            assert!(parse(garbage).is_err(), "garbage input must error, not panic: {garbage:?}");
        }
        // Nested braces inside strings must not confuse the span scanner.
        let noisy = "note {not json} then {\"patches\":[{\"kind\":\"skill_mod\",\"ops\":[{\"action\":\"remove_skill\",\"target\":\"rust\"}]}]}";
        assert_eq!(parse(noisy).expect("span").len(), 1);
    }

    #[test]
    fn phased_orders_capability_first_and_caps_per_round() {
        let capability = |method: PatchKind| patch(method, vec![op(PatchAction::AppendSection, "system.md", Some("C"), "c")]);
        let steering = |method: PatchKind| patch(method, vec![op(PatchAction::AppendSection, "system.md", Some("S"), "s")]);
        let patches = vec![
            steering(PatchKind::PromptRuleAdd),
            capability(PatchKind::ToolImplFix),
            steering(PatchKind::SkillAdd),
            capability(PatchKind::MemoryPolicy),
            capability(PatchKind::LoopLogic),
            steering(PatchKind::PromptRuleMod),
        ];
        assert_eq!(phased(&patches, 1, 6, true), vec![1, 3, 4], "early rounds: capability only, capped");
        assert_eq!(phased(&patches, 5, 6, true), vec![1, 3, 4], "late rounds: capability first, capped");
        let mut later = vec![steering(PatchKind::PromptRuleAdd), capability(PatchKind::ToolImplFix), capability(PatchKind::InfraChange)];
        assert_eq!(phased(&later, 4, 6, true), vec![1, 2, 0], "late rounds keep capability ahead of steering");
        assert_eq!(phased(&patches, 1, 6, false), vec![0, 1, 2], "disabled keeps original order");
        later.clear();
        assert!(phased(&later, 1, 6, true).is_empty());
        assert!(phased(&[], 1, 1, false).is_empty());
    }

    #[test]
    fn to_json_is_pretty_and_round_trips() {
        let patches = vec![patch(PatchKind::PromptRuleAdd, vec![op(PatchAction::AppendSection, "system.md", Some("T"), "c")])];
        let encoded = to_json(&patches);
        assert!(encoded.contains("\n"), "pretty output");
        let decoded: Vec<TargetedPatch> = serde_json::from_str(&encoded).expect("round trip");
        assert_eq!(decoded, patches);
    }
}
