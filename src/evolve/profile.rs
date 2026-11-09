//! Composable harness profile: replaceable slots (tools, skills, context, memory, verify, JIT)
//! rather than one monolithic prompt (HarnessX composable primitives). Profiles are the
//! task-adaptive unit that JIT selection and evolution both operate on.
//!
//! A profile is a *safe* projection of a bundle: it may move context ratios, verify commands and
//! prompt notes, and it never turns on stub mode, approval modes or repositories. Everything that
//! changes the safety posture of a run stays outside this type on purpose.
#![allow(dead_code)]

use crate::evolve::bundle::HarnessContext;
use crate::evolve::types::*;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Context ratios must leave the harness room to work: too little reserve overflows the window,
/// too much starves the turn of conversation and file budget.
pub const RATIO_MIN: f64 = 0.05;
pub const RATIO_MAX: f64 = 0.9;
/// Memory notes become prompt text: keep the slot bounded whatever `memory.md` holds.
pub const MAX_MEMORY_NOTES: usize = 16;
pub const MAX_NOTE_CHARS: usize = 200;
/// This binary exposes no delegation knob, so the delegation slot is a constant projection
/// instead of an invented field. Diffing it therefore never reports a change.
pub const DELEGATION_DEFAULT: &str = "inherit";

/// Policy keys, most specific first. The canonical names mirror `config.json`; the short forms
/// exist because hand-written bundles tend to use them.
const RESERVE_KEYS: [&str; 4] = ["contextReserveRatio", "reserve", "reserveRatio", "context_reserve_ratio"];
const CONVERSATION_KEYS: [&str; 4] = ["contextConversationRatio", "conversation", "conversationRatio", "context_conversation_ratio"];
const FILES_KEYS: [&str; 4] = ["contextFilesRatio", "files", "filesRatio", "context_files_ratio"];
const VERIFY_KEYS: [&str; 3] = ["commands", "verifyCommands", "verify_commands"];
const JIT_KEYS: [&str; 2] = ["enabled", "jit"];

/// Replaceable slots of a harness profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Tools,
    Skills,
    Context,
    Memory,
    Verify,
    Delegation,
    Jit,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Tools => "tools",
            Slot::Skills => "skills",
            Slot::Context => "context",
            Slot::Memory => "memory",
            Slot::Verify => "verify",
            Slot::Delegation => "delegation",
            Slot::Jit => "jit",
        }
    }

    /// Stable render/diff order.
    pub fn all() -> [Slot; 7] {
        [Slot::Tools, Slot::Skills, Slot::Context, Slot::Memory, Slot::Verify, Slot::Delegation, Slot::Jit]
    }
}

/// A concrete, applicable harness configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessProfile {
    pub variant: Variant,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub context_reserve: Option<f64>,
    #[serde(default)]
    pub context_conversation: Option<f64>,
    #[serde(default)]
    pub context_files: Option<f64>,
    #[serde(default)]
    pub verify_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_repair_rounds: Option<u64>,
    #[serde(default)]
    pub memory_notes: Vec<String>,
    #[serde(default)]
    pub jit: bool,
}

impl Default for HarnessProfile {
    fn default() -> Self {
        Self {
            variant: Variant::Code,
            name: "lean".into(),
            tools: None,
            skills: vec![],
            context_reserve: None,
            context_conversation: None,
            context_files: None,
            verify_commands: vec![],
            verify_repair_rounds: None,
            memory_notes: vec![],
            jit: false,
        }
    }
}

impl HarnessProfile {
    /// Bundle-backed profile with the conventional name (`bundle:<variant>`).
    pub fn named(variant: Variant) -> Self {
        Self { variant, name: format!("bundle:{}", variant.as_str()), ..Self::default() }
    }

    /// True when the profile asks the runtime for nothing (safe to ignore).
    pub fn is_noop(&self) -> bool {
        self.context_reserve.is_none()
            && self.context_conversation.is_none()
            && self.context_files.is_none()
            && self.verify_commands.is_empty()
            && self.memory_notes.is_empty()
            && !self.jit
            && self.tools.is_none()
    }
}

/// Clamps a context ratio into the applicable band; `NaN` collapses to the floor.
pub fn clamp_ratio(value: f64) -> f64 {
    if value.is_nan() {
        RATIO_MIN
    } else {
        value.clamp(RATIO_MIN, RATIO_MAX)
    }
}

/// Builds a profile from a resolved bundle context plus the task variant.
pub fn from_context(ctx: &HarnessContext, variant: Variant) -> HarnessProfile {
    let mut profile = HarnessProfile::named(variant);
    profile.skills = ctx.skills.iter().map(|(name, _)| normalize_skill_name(name)).filter(|name| !name.is_empty()).collect();
    profile.skills.sort();
    profile.skills.dedup();
    profile.context_reserve = ctx.context_reserve.filter(|value| value.is_finite());
    profile.context_conversation = ctx.context_conversation.filter(|value| value.is_finite());
    profile.context_files = ctx.context_files.filter(|value| value.is_finite());
    profile.verify_commands = clean_commands(&ctx.verify_commands);
    profile.verify_repair_rounds = ctx.verify_repair_rounds;
    profile.memory_notes = clean_notes(&ctx.memory_notes);
    profile.jit = ctx.jit_enabled.unwrap_or(false);
    profile
}

/// Builds a profile directly from bundle files: policy JSON plus component presence.
pub fn from_files(files: &BTreeMap<String, String>, variant: Variant) -> Result<HarnessProfile> {
    let mut profile = HarnessProfile::named(variant);
    profile.skills = skill_names(files);
    if let Some(body) = files.get(CONTEXT_POLICY_PATH) {
        let policy = parse_object(body, CONTEXT_POLICY_PATH)?;
        profile.context_reserve = number_in(&policy, &RESERVE_KEYS);
        profile.context_conversation = number_in(&policy, &CONVERSATION_KEYS);
        profile.context_files = number_in(&policy, &FILES_KEYS);
    }
    if let Some(body) = files.get(VERIFY_POLICY_PATH) {
        profile.verify_commands = parse_verify_commands(body, VERIFY_POLICY_PATH)?;
        profile.verify_repair_rounds =
            serde_json::from_str::<serde_json::Value>(body).ok().and_then(|value| value.get("maxRepairRounds").or_else(|| value.get("max_repair_rounds")).and_then(serde_json::Value::as_u64));
    }
    if let Some(body) = files.get(JIT_POLICY_PATH) {
        profile.jit = parse_jit_enabled(body, JIT_POLICY_PATH)?;
    }
    if let Some(body) = files.get(MEMORY_PATH) {
        profile.memory_notes = memory_notes(body);
    }
    Ok(profile)
}

/// Applies a profile onto the runtime config that the harness actually reads.
///
/// Only the three context ratios and `verify.commands` are touched: never `stub`, never
/// `approval_mode`, never `rag_enabled`/`memory_enabled` (profiles carry no such field, so an
/// unchanged profile leaves them unchanged). `None` values mean "inherit", not "zero".
pub fn apply_to_config(profile: &HarnessProfile, config: &mut crate::config::Config) {
    if let Some(value) = profile.context_reserve.filter(|value| value.is_finite()) {
        config.context_reserve_ratio = clamp_ratio(value);
    }
    if let Some(value) = profile.context_conversation.filter(|value| value.is_finite()) {
        config.context_conversation_ratio = clamp_ratio(value);
    }
    if let Some(value) = profile.context_files.filter(|value| value.is_finite()) {
        config.context_files_ratio = clamp_ratio(value);
    }
    let commands = clean_commands(&profile.verify_commands);
    if !commands.is_empty() {
        // A bundle that declares verify commands means "verify this", so it also enables the
        // stage: otherwise the declaration would be silently inert.
        config.verify.enabled = true;
        config.verify.commands = commands;
    }
    if let Some(rounds) = profile.verify_repair_rounds.filter(|rounds| *rounds > 0) {
        config.verify.max_repair_rounds = rounds;
    }
}

/// Slot-level diff between two profiles: `(slot, before, after)` for changed slots only.
pub fn slot_diff(before: &HarnessProfile, after: &HarnessProfile) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for slot in Slot::all() {
        let before_value = slot_value(before, slot);
        let after_value = slot_value(after, slot);
        if before_value != after_value {
            out.push((slot.as_str().to_string(), before_value, after_value));
        }
    }
    out
}

/// Rendered value of one slot, used by both `slot_diff` and `render`.
pub fn slot_value(profile: &HarnessProfile, slot: Slot) -> String {
    match slot {
        Slot::Tools => match &profile.tools {
            None => "full".to_string(),
            Some(tools) if tools.is_empty() => "none".to_string(),
            Some(tools) => tools.join(","),
        },
        Slot::Skills => join_or_dash(&profile.skills),
        Slot::Context => format!("reserve={},conversation={},files={}", ratio_text(profile.context_reserve), ratio_text(profile.context_conversation), ratio_text(profile.context_files)),
        Slot::Memory => join_or_dash(&profile.memory_notes),
        Slot::Verify => join_or_dash(&profile.verify_commands),
        Slot::Delegation => DELEGATION_DEFAULT.to_string(),
        Slot::Jit => if profile.jit { "on" } else { "off" }.to_string(),
    }
}

/// Human/agent-readable rendering used in JIT decisions and manifests.
pub fn render(profile: &HarnessProfile) -> String {
    let mut out = format!("**harness profile** `{}` ({})\n", profile.name, profile.variant.as_str());
    for slot in Slot::all() {
        out.push_str(&format!("- {}: {}\n", slot.as_str(), slot_value(profile, slot)));
    }
    out
}

/// Lenient helper for callers that only need the verify commands (`JIT` repair staging).
pub fn verify_commands_from_files(files: &BTreeMap<String, String>) -> Vec<String> {
    files.get(VERIFY_POLICY_PATH).map(|body| parse_verify_commands(body, VERIFY_POLICY_PATH).unwrap_or_default()).unwrap_or_default()
}

/// Skill names implied by `skills/*.md` presence, sorted for determinism.
pub fn skill_names(files: &BTreeMap<String, String>) -> Vec<String> {
    // Only markdown files are skill components; anything else under skills/ is ignored.
    let mut names: Vec<String> = files
        .keys()
        .filter(|path| path.starts_with("skills/") && path.ends_with(".md"))
        .filter_map(|path| path.strip_prefix("skills/").map(normalize_skill_name))
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Memory notes carried by `memory.md` bullets (headings, prose and code fences are ignored).
pub fn memory_notes_from_files(files: &BTreeMap<String, String>) -> Vec<String> {
    files.get(MEMORY_PATH).map(|body| memory_notes(body)).unwrap_or_default()
}

fn normalize_skill_name(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches("./").trim_start_matches('/');
    let without_dir = trimmed.strip_prefix("skills/").unwrap_or(trimmed);
    let without_ext = without_dir.strip_suffix(".md").unwrap_or(without_dir);
    without_ext.trim_matches('/').to_string()
}

fn clean_commands(commands: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for command in commands {
        let command = command.trim();
        if !command.is_empty() && !out.iter().any(|seen| seen == command) {
            out.push(command.to_string());
        }
    }
    out
}

fn clean_notes(notes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for note in notes {
        let note = flatten(note);
        if !note.is_empty() && !out.iter().any(|seen| seen == &note) {
            out.push(note);
        }
        if out.len() >= MAX_MEMORY_NOTES {
            break;
        }
    }
    out
}

fn memory_notes(body: &str) -> Vec<String> {
    let mut notes: Vec<String> = vec![];
    let mut in_fence = false;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bullet = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ "));
        if let Some(bullet) = bullet {
            let note = flatten(bullet);
            if !note.is_empty() && !notes.iter().any(|seen| seen == &note) {
                notes.push(note);
            }
        }
        if notes.len() >= MAX_MEMORY_NOTES {
            break;
        }
    }
    notes
}

fn flatten(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_NOTE_CHARS {
        collapsed.chars().take(MAX_NOTE_CHARS).collect()
    } else {
        collapsed
    }
}

fn parse_object(body: &str, path: &str) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(body).map_err(|error| anyhow!("{path}: invalid JSON ({error})"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!("{path}: expected a JSON object")),
    }
}

fn parse_verify_commands(body: &str, path: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body).map_err(|error| anyhow!("{path}: invalid JSON ({error})"))?;
    match &value {
        Value::Array(_) => Ok(clean_commands(&strings(&value))),
        Value::Object(map) => Ok(clean_commands(&lookup(map, &VERIFY_KEYS).map(strings).unwrap_or_default())),
        _ => Err(anyhow!("{path}: expected a JSON object or array")),
    }
}

fn parse_jit_enabled(body: &str, path: &str) -> Result<bool> {
    let value: Value = serde_json::from_str(body).map_err(|error| anyhow!("{path}: invalid JSON ({error})"))?;
    match &value {
        Value::Bool(enabled) => Ok(*enabled),
        Value::Object(map) => Ok(lookup(map, &JIT_KEYS).and_then(bool_of).unwrap_or(false)),
        _ => Err(anyhow!("{path}: expected a JSON object or boolean")),
    }
}

fn lookup<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| map.get(*key))
}

fn number_in(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    let value = lookup(map, keys)?;
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    number.is_finite().then_some(number)
}

fn bool_of(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_u64().map(|value| value != 0),
        _ => None,
    }
}

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(|item| item.as_str()).map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect(),
        Value::String(single) => {
            let single = single.trim();
            if single.is_empty() {
                vec![]
            } else {
                vec![single.to_string()]
            }
        }
        _ => vec![],
    }
}

fn ratio_text(value: Option<f64>) -> String {
    value.map(|number| format!("{number:.2}")).unwrap_or_else(|| "-".to_string())
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(variant: Variant, name: &str) -> HarnessProfile {
        HarnessProfile { variant, name: name.to_string(), ..HarnessProfile::default() }
    }

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(path, body)| (path.to_string(), body.to_string())).collect()
    }

    #[test]
    fn apply_clamps_ratios_and_leaves_safety_knobs_alone() {
        let mut config = crate::config::Config::default();
        let stub_before = config.stub;
        let approval_before = config.approval_mode.clone();
        let rag_before = config.rag_enabled;
        let memory_before = config.memory_enabled;
        let files_before = config.context_files_ratio;
        let commands_before = config.verify.commands.clone();

        let mut applied = profile(Variant::Code, "clamped");
        applied.context_reserve = Some(4.0);
        applied.context_conversation = Some(0.0);
        applied.context_files = Some(f64::NAN);
        applied.verify_commands = vec!["cargo test".into(), "  ".into(), "cargo test".into()];
        applied.memory_notes = vec!["note".into()];
        apply_to_config(&applied, &mut config);

        assert_eq!(config.context_reserve_ratio, RATIO_MAX);
        assert_eq!(config.context_conversation_ratio, RATIO_MIN);
        // NaN is not a request: inherit the configured value.
        assert_eq!(config.context_files_ratio, files_before);
        assert_eq!(config.verify.commands, vec!["cargo test".to_string()]);
        assert_eq!(config.stub, stub_before);
        assert_eq!(config.approval_mode, approval_before);
        assert_eq!(config.rag_enabled, rag_before);
        assert_eq!(config.memory_enabled, memory_before);

        // An empty profile must not clear anything.
        apply_to_config(&profile(Variant::Code, "empty"), &mut config);
        assert_eq!(config.verify.commands, vec!["cargo test".to_string()]);
        assert_ne!(commands_before, config.verify.commands);
    }

    #[test]
    fn slot_diff_reports_only_changed_slots() {
        let before = profile(Variant::Code, "a");
        let mut after = before.clone();
        after.tools = Some(vec!["shell".into()]);
        after.skills = vec!["rust".into()];
        after.context_conversation = Some(0.3);
        after.verify_commands = vec!["cargo test".into()];
        after.jit = true;

        let diff = slot_diff(&before, &after);
        let slots: Vec<&str> = diff.iter().map(|(slot, _, _)| slot.as_str()).collect();
        assert_eq!(slots, vec!["tools", "skills", "context", "verify", "jit"]);

        let tools = diff.iter().find(|(slot, _, _)| slot == "tools").expect("tools diff");
        assert_eq!(tools.1, "full");
        assert_eq!(tools.2, "shell");
        let context = diff.iter().find(|(slot, _, _)| slot == "context").expect("context diff");
        assert_eq!(context.1, "reserve=-,conversation=-,files=-");
        assert_eq!(context.2, "reserve=-,conversation=0.30,files=-");
        assert!(slot_diff(&after, &after).is_empty());
    }

    #[test]
    fn from_files_reads_policies_and_component_presence() {
        let bundle = files(&[
            ("policy/context.json", r#"{"contextReserveRatio":"0.2","conversation":0.5}"#),
            ("policy/verify.json", r#"{"commands":["cargo test",""]}"#),
            ("policy/jit.json", r#"{"enabled":true}"#),
            ("skills/rust.md", "rust rules"),
            ("skills/deep/nested.md", "nested"),
            ("skills/notes.txt", "ignored"),
            ("memory.md", "- first note\n# heading\n- second note\n```\n- not a note\n```\n- first note\n"),
        ]);
        let parsed = from_files(&bundle, Variant::Research).expect("parse profile");
        assert_eq!(parsed.name, "bundle:research");
        assert_eq!(parsed.variant, Variant::Research);
        assert_eq!(parsed.context_reserve, Some(0.2));
        assert_eq!(parsed.context_conversation, Some(0.5));
        assert_eq!(parsed.context_files, None);
        assert_eq!(parsed.verify_commands, vec!["cargo test".to_string()]);
        assert_eq!(parsed.skills, vec!["deep/nested".to_string(), "rust".to_string()]);
        assert_eq!(parsed.memory_notes, vec!["first note".to_string(), "second note".to_string()]);
        assert!(parsed.jit);
        assert!(parsed.tools.is_none());
    }

    #[test]
    fn from_files_is_strict_and_helpers_are_lenient() {
        let broken = files(&[("policy/context.json", "{not json")]);
        assert!(from_files(&broken, Variant::Code).is_err());

        let wrong_shape = files(&[("policy/verify.json", r#"{"commands":"cargo test"}"#)]);
        assert_eq!(from_files(&wrong_shape, Variant::Code).expect("string command").verify_commands, vec!["cargo test".to_string()]);

        let bare_array = files(&[("policy/verify.json", r#"["echo ok","echo ok"]"#)]);
        assert_eq!(verify_commands_from_files(&bare_array), vec!["echo ok".to_string()]);

        let broken_verify = files(&[("policy/verify.json", "nope")]);
        assert!(verify_commands_from_files(&broken_verify).is_empty());
        assert!(memory_notes_from_files(&files(&[("memory.md", "no bullets here")])).is_empty());
    }

    #[test]
    fn from_context_maps_resolved_components() {
        let ctx = HarnessContext {
            system_append: "be terse".into(),
            skills: vec![("skills/rust.md".into(), "body".into())],
            tool_guidance: "guidance".into(),
            middleware: vec![],
            memory_notes: vec!["remember the build takes minutes".into()],
            verify_commands: vec!["cargo check --lib".into()],
            context_reserve: Some(0.2),
            context_conversation: Some(0.4),
            context_files: None,
            jit_enabled: Some(true),
            ..HarnessContext::default()
        };
        let parsed = from_context(&ctx, Variant::Code);
        assert_eq!(parsed.name, "bundle:code");
        assert_eq!(parsed.skills, vec!["rust".to_string()]);
        assert_eq!(parsed.context_reserve, Some(0.2));
        assert_eq!(parsed.context_conversation, Some(0.4));
        assert_eq!(parsed.context_files, None);
        assert_eq!(parsed.verify_commands, vec!["cargo check --lib".to_string()]);
        assert_eq!(parsed.memory_notes, vec!["remember the build takes minutes".to_string()]);
        assert!(parsed.jit);
        assert!(!parsed.is_noop());
    }

    #[test]
    fn render_is_short_markdown() {
        let mut rendered_profile = profile(Variant::Ops, "bundle:ops");
        rendered_profile.jit = true;
        rendered_profile.context_files = Some(0.25);
        let text = render(&rendered_profile);
        assert!(text.contains("bundle:ops"));
        assert!(text.contains("(ops)"));
        assert!(text.contains("- tools: full"));
        assert!(text.contains("- context: reserve=-,conversation=-,files=0.25"));
        assert!(text.contains("- delegation: inherit"));
        assert!(text.contains("- jit: on"));
        assert!(text.lines().count() <= 9);
    }
}
