//! Decision observability: every round writes a manifest pairing edits with self-declared
//! predictions, and the next round turns those predictions into verdicts (AHE pillar 3).
//! Also owns the acceptance gate: no regressions on already-solved tasks (HarnessX seesaw)
//! plus an optional held-out dev gate for durability (AutoSaddler).
#![allow(dead_code)]

use crate::evolve::patch::{PatchOp, TargetedPatch};
use crate::evolve::types::*;
use anyhow::Result;
use std::collections::BTreeSet;

/// Gate configuration for one acceptance decision.
#[derive(Clone, Debug)]
pub struct AcceptanceGate {
    pub solved: BTreeSet<String>,
    pub max_regress: usize,
    pub min_gain_pp: f64,
    pub require_dev: bool,
}

impl Default for AcceptanceGate {
    fn default() -> Self {
        Self { solved: BTreeSet::new(), max_regress: 1, min_gain_pp: 0.0, require_dev: false }
    }
}

/// Compares before/after runs (and optional dev runs) and decides accept/reject.
pub fn decide(before: &RunSummary, after: &RunSummary, dev: Option<(&RunSummary, &RunSummary)>, gate: &AcceptanceGate) -> GateDecision {
    let before_rate = before.pass_rate();
    let after_rate = after.pass_rate();
    let gain_pp = (after_rate - before_rate) * 100.0;

    // (0) An unmeasured edit is never durable: without tasks there is nothing to falsify.
    if before.total() == 0 || after.total() == 0 {
        return GateDecision::reject(format!("no tasks measured (before {} / after {}): refusing an unverifiable harness change", before.total(), after.total()));
    }

    // (a) Any drop in train pass rate is a hard reject: the seesaw must not tip backwards.
    if after_rate < before_rate {
        return GateDecision::reject(format!("train pass rate dropped {:.1}pp ({}/{}) -> ({}/{})", -gain_pp, before.passed(), before.total(), after.passed(), after.total()));
    }

    // (b) Solved tasks that stopped passing: the seesaw's per-task guarantee.
    let lost: Vec<String> = gate.solved.iter().filter(|task| passed_in(before, task) && !passed_in(after, task)).cloned().collect();
    if lost.len() > gate.max_regress {
        return GateDecision::reject(format!("lost {} previously solved task(s) (limit {}): {}", lost.len(), gate.max_regress, preview(&lost, 3)));
    }

    // (c) Measured gain must clear the configured bar.
    if gain_pp < gate.min_gain_pp {
        return GateDecision::reject(format!("gain {:.1}pp below required {:.1}pp", gain_pp, gate.min_gain_pp));
    }

    // (c2) A round that fixed nothing is not an improvement, even when nothing regressed.
    let fixed: Vec<String> = after.passed_ids().difference(&before.passed_ids()).cloned().collect();
    let still_failing = after.failed_ids();
    let untouched = still_failing.intersection(&before.failed_ids()).count();
    if fixed.is_empty() && untouched == before.failed_ids().len() && untouched > 0 {
        return GateDecision::reject("no task was fixed: the edit did not change any outcome".to_string());
    }

    // (d) Durability gate: an edit that only holds on the train split is not durable.
    if gate.require_dev {
        if let Some((dev_before, dev_after)) = dev {
            if dev_after.pass_rate() < dev_before.pass_rate() {
                let dev_gain = (dev_after.pass_rate() - dev_before.pass_rate()) * 100.0;
                return GateDecision::reject(format!("dev pass rate dropped {:.1}pp ({}/{}) -> ({}/{})", -dev_gain, dev_before.passed(), dev_before.total(), dev_after.passed(), dev_after.total()));
            }
        }
    }

    GateDecision::Accept
}

/// Builds the durable manifest for a round from patches, digests and measured runs.
pub fn build(
    round: u32,
    base: &HarnessBundle,
    candidate: &HarnessBundle,
    patches: &[TargetedPatch],
    before: &RunSummary,
    after: Option<&RunSummary>,
    dev: Option<(&RunSummary, &RunSummary)>,
    decision: &GateDecision,
) -> ChangeManifest {
    let mut entries: Vec<ManifestEntry> = patches.iter().map(|patch| entry_for(round, patch)).collect();
    entries.sort_by(|left, right| left.component.cmp(&right.component).then_with(|| left.id.cmp(&right.id)));
    ChangeManifest {
        schema: EVOLVE_SCHEMA,
        round,
        base: base.id.clone(),
        candidate: candidate.id.clone(),
        created: now_rfc3339(),
        accepted: decision.accepted(),
        reject_reason: match decision {
            GateDecision::Accept => None,
            GateDecision::Reject { reason } => Some(reason.clone()),
        },
        before: Score::from_run(before),
        after: after.map(Score::from_run),
        dev_before: dev.map(|(dev_before, _)| Score::from_run(dev_before)),
        dev_after: dev.map(|(_, dev_after)| Score::from_run(dev_after)),
        entries,
    }
}

/// Turns the previous round's predictions into Confirmed/Partial/Refuted/Regressed verdicts.
pub fn verify_predictions(manifest: &mut ChangeManifest, before: &RunSummary, after: &RunSummary) -> Result<Vec<(String, PatchedVerdict)>> {
    let gain_pp = round2((after.pass_rate() - before.pass_rate()) * 100.0);
    let before_passed = before.passed_ids();
    let after_passed = after.passed_ids();
    let after_failed = after.failed_ids();
    let mut results: Vec<(String, PatchedVerdict)> = Vec::with_capacity(manifest.entries.len());
    for entry in manifest.entries.iter_mut() {
        let regressed: Vec<String> = entry.prediction.at_risk.iter().filter(|task| before_passed.contains(*task) && after_failed.contains(*task)).cloned().collect();
        let expected: Vec<&String> = entry.prediction.expected_fix.iter().filter(|task| !task.trim().is_empty()).collect();
        let fixed = expected.iter().filter(|task| after_passed.contains(**task)).count();
        let verdict = if !regressed.is_empty() {
            // A regression outranks any amount of fixing: the edit broke a solved task.
            note_regression(entry, &regressed);
            PatchedVerdict::Regressed
        } else if expected.is_empty() {
            // Nothing was claimed, so nothing can be falsified either way.
            PatchedVerdict::Pending
        } else if fixed == expected.len() {
            PatchedVerdict::Confirmed
        } else if fixed > 0 {
            PatchedVerdict::Partial
        } else {
            PatchedVerdict::Refuted
        };
        entry.verdict = verdict;
        entry.measured_gain_pp = Some(gain_pp);
        results.push((entry.id.clone(), verdict));
    }
    Ok(results)
}

/// Reflection buckets (AutoSaddler): fixed / regressed / still-failing / still-passing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reflection {
    pub fixed: Vec<String>,
    pub regressed: Vec<String>,
    pub still_failing: Vec<String>,
    pub still_passing: Vec<String>,
}

/// Splits task ids into the four reflection buckets.
pub fn reflect(before: &RunSummary, after: &RunSummary, tracked: &[String]) -> Reflection {
    let before_passed = before.passed_ids();
    let after_passed = after.passed_ids();
    let mut fixed: BTreeSet<String> = BTreeSet::new();
    let mut regressed: BTreeSet<String> = BTreeSet::new();
    let mut still_failing: BTreeSet<String> = BTreeSet::new();
    let mut still_passing: BTreeSet<String> = BTreeSet::new();
    for task in tracked {
        let task = task.trim();
        if task.is_empty() {
            continue;
        }
        match (before_passed.contains(task), after_passed.contains(task)) {
            (false, true) => fixed.insert(task.to_string()),
            (true, false) => regressed.insert(task.to_string()),
            (false, false) => still_failing.insert(task.to_string()),
            (true, true) => still_passing.insert(task.to_string()),
        };
    }
    Reflection {
        fixed: fixed.into_iter().collect(),
        regressed: regressed.into_iter().collect(),
        still_failing: still_failing.into_iter().collect(),
        still_passing: still_passing.into_iter().collect(),
    }
}

/// Lessons extracted from a reflection (durable, small, human-readable).
pub fn lessons(reflection: &Reflection) -> Vec<String> {
    let mut lessons: Vec<String> = Vec::new();
    if !reflection.fixed.is_empty() {
        lessons.push(format!("edit fixed {}", describe_tasks(&reflection.fixed)));
    }
    if !reflection.regressed.is_empty() {
        lessons.push(format!("edit regressed {}", describe_tasks(&reflection.regressed)));
    }
    if !reflection.still_failing.is_empty() {
        lessons.push(format!("{} task(s) still failing", reflection.still_failing.len()));
    }
    if !reflection.still_passing.is_empty() {
        lessons.push(format!("{} already-solved task(s) stayed passing", reflection.still_passing.len()));
    }
    lessons.truncate(4);
    lessons
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn passed_in(run: &RunSummary, task: &str) -> bool {
    run.outcome(task).map(|outcome| outcome.passed).unwrap_or(false)
}

fn preview(tasks: &[String], limit: usize) -> String {
    let shown: Vec<&str> = tasks.iter().take(limit).map(String::as_str).collect();
    let mut text = shown.join(", ");
    if tasks.len() > shown.len() {
        text.push_str(", ...");
    }
    text
}

fn describe_tasks(tasks: &[String]) -> String {
    format!("{} task(s) ({})", tasks.len(), preview(tasks, 3))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn note_regression(entry: &mut ManifestEntry, tasks: &[String]) {
    let note = format!("regressed {}", preview(tasks, 3));
    match entry.note.as_mut() {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&note);
        }
        None => entry.note = Some(note),
    }
}

/// One manifest entry per proposed patch.
fn entry_for(round: u32, patch: &TargetedPatch) -> ManifestEntry {
    let component = patch.primary_component();
    let (component_kind, note) = match resolve_kind(patch, &component) {
        Some(kind) => (kind, None),
        None => (ComponentKind::SystemPrompt, Some(format!("component kind unresolved for '{component}'"))),
    };
    ManifestEntry {
        id: short_hash(format!("{round}:{}:{component}", patch.kind.as_str()).as_bytes()),
        round,
        component,
        component_kind,
        patch_kind: patch.kind,
        root_cause: patch.rationale.trim().to_string(),
        fix: describe_ops(&patch.ops),
        evidence: patch.evidence.clone(),
        prediction: patch.prediction.clone(),
        verdict: PatchedVerdict::Pending,
        measured_gain_pp: None,
        note,
    }
}

/// Component kind from the patch's own paths; `None` when no path is a known component.
fn resolve_kind(patch: &TargetedPatch, component: &str) -> Option<ComponentKind> {
    if let Some(kind) = path_kind(component) {
        return Some(kind);
    }
    patch.ops.iter().find_map(|op| path_kind(&op.component))
}

fn path_kind(path: &str) -> Option<ComponentKind> {
    let normalized = path.trim().replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    let bundle_prefix = format!("{BUNDLE_DIR}/");
    let relative = normalized.strip_prefix(&bundle_prefix).unwrap_or(normalized);
    component_kind_for_path(relative)
}

/// Short human description of the concrete operation ("append_section Rules in system.md").
fn describe_ops(ops: &[PatchOp]) -> String {
    if ops.is_empty() {
        return "(no ops)".to_string();
    }
    let described: Vec<String> = ops.iter().map(describe_op).collect();
    truncate_chars(&described.join("; "), 160)
}

fn describe_op(op: &PatchOp) -> String {
    let component = op.component.trim();
    let component = if component.is_empty() { SKILLS_DIR } else { component };
    match op.target.as_deref().map(str::trim).filter(|target| !target.is_empty()) {
        Some(target) => format!("{} {} in {}", op.action.as_str(), target, component),
        None => format!("{} {}", op.action.as_str(), component),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn outcome(task: &str, passed: bool) -> TaskOutcome {
        TaskOutcome { task_id: task.to_string(), passed, score: if passed { 1.0 } else { 0.0 }, tokens: 1, duration_ms: 1, failure: None, session_id: None, detail: None }
    }

    fn run(split: Split, passed: &[&str], failed: &[&str]) -> RunSummary {
        let mut outcomes: Vec<TaskOutcome> = Vec::new();
        for task in passed {
            outcomes.push(outcome(task, true));
        }
        for task in failed {
            outcomes.push(outcome(task, false));
        }
        RunSummary::new(split, Some("bundle".to_string()), Some(Variant::Code), outcomes)
    }

    fn gate(solved: &[&str]) -> AcceptanceGate {
        AcceptanceGate { solved: solved.iter().map(|task| task.to_string()).collect(), max_regress: 1, min_gain_pp: 0.0, require_dev: false }
    }

    fn bundle(files: &[(&str, &str)], round: u32) -> HarnessBundle {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for (path, body) in files {
            map.insert((*path).to_string(), (*body).to_string());
        }
        HarnessBundle::build(&map, None, round, Variant::Code)
    }

    fn op(action: crate::evolve::patch::PatchAction, component: &str, target: Option<&str>) -> PatchOp {
        PatchOp { action, component: component.to_string(), target: target.map(str::to_string), content: "c".to_string() }
    }

    fn patch(kind: crate::evolve::patch::PatchKind, ops: Vec<PatchOp>, expected: &[&str], at_risk: &[&str]) -> TargetedPatch {
        TargetedPatch {
            kind,
            rationale: "cluster of tool errors".to_string(),
            evidence: vec![EvidenceRef::new("s1", "t1", "tool_error")],
            ops,
            prediction: Prediction {
                expected_fix: expected.iter().map(|task| task.to_string()).collect(),
                at_risk: at_risk.iter().map(|task| task.to_string()).collect(),
                expected_gain_pp: 10.0,
                confidence: 0.5,
            },
        }
    }

    #[test]
    fn decide_accepts_a_gain_without_regressions() {
        let before = run(Split::Train, &["a"], &["b"]);
        let after = run(Split::Train, &["a", "b"], &[]);
        assert_eq!(decide(&before, &after, None, &gate(&["a"])), GateDecision::Accept);
    }

    #[test]
    fn decide_rejects_unmeasured_rounds() {
        // Dry runs and empty eval sets must never become a durable harness change.
        let empty = RunSummary::new(Split::Train, None, None, vec![]);
        match decide(&empty, &empty, None, &gate(&[])) {
            GateDecision::Reject { reason } => assert!(reason.contains("no tasks measured"), "reason: {reason}"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn decide_rejects_train_regression() {
        let before = run(Split::Train, &["a", "b"], &[]);
        let after = run(Split::Train, &["a"], &["b"]);
        let decision = decide(&before, &after, None, &gate(&["a", "b"]));
        match decision {
            GateDecision::Reject { reason } => {
                assert!(reason.contains("dropped"), "reason: {reason}");
                assert!(reason.contains("pp"), "reason carries numbers: {reason}");
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn decide_rejects_losing_the_solved_set() {
        // Flat pass rate, but a task the harness already solved now fails.
        let before = run(Split::Train, &["a"], &["x"]);
        let after = run(Split::Train, &["x"], &["a"]);
        let mut config = gate(&["a"]);
        config.max_regress = 0;
        let decision = decide(&before, &after, None, &config);
        match decision {
            GateDecision::Reject { reason } => assert!(reason.contains("lost"), "reason: {reason}"),
            other => panic!("expected reject, got {other:?}"),
        }
        // A single loss is tolerated when the budget allows it.
        let mut allowed = gate(&["a"]);
        allowed.max_regress = 1;
        allowed.min_gain_pp = 0.0;
        assert_eq!(decide(&before, &after, None, &allowed), GateDecision::Accept);
    }

    #[test]
    fn decide_rejects_gain_below_threshold() {
        let before = run(Split::Train, &["a"], &["b"]);
        let after = run(Split::Train, &["a", "b"], &[]);
        let mut config = gate(&["a"]);
        config.min_gain_pp = 60.0;
        match decide(&before, &after, None, &config) {
            GateDecision::Reject { reason } => assert!(reason.contains("gain"), "reason: {reason}"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn decide_rejects_dev_durability_drop() {
        let before = run(Split::Train, &["a"], &["b"]);
        let after = run(Split::Train, &["a", "b"], &[]);
        let dev_before = run(Split::Dev, &["d1", "d2"], &[]);
        let dev_after = run(Split::Dev, &["d1"], &["d2"]);
        let mut config = gate(&["a"]);
        config.require_dev = true;
        match decide(&before, &after, Some((&dev_before, &dev_after)), &config) {
            GateDecision::Reject { reason } => assert!(reason.contains("dev"), "reason: {reason}"),
            other => panic!("expected reject, got {other:?}"),
        }
        // Without the requirement the train gain is enough.
        config.require_dev = false;
        assert_eq!(decide(&before, &after, Some((&dev_before, &dev_after)), &config), GateDecision::Accept);
    }

    #[test]
    fn build_records_entries_sorted_and_stable() {
        let base = bundle(&[("system.md", "# System\n"), ("skills/rust.md", "# Rust\n")], 1);
        let candidate = bundle(&[("system.md", "# System\n\n## Rules\nbe brief.\n"), ("skills/rust.md", "# Rust\n")], 1);
        let patches = vec![
            patch(crate::evolve::patch::PatchKind::PromptRuleAdd, vec![op(crate::evolve::patch::PatchAction::AppendSection, "system.md", Some("Rules"))], &["a"], &[]),
            patch(crate::evolve::patch::PatchKind::SkillMod, vec![op(crate::evolve::patch::PatchAction::AppendSection, "skills/rust.md", Some("Tips"))], &["b"], &[]),
        ];
        let before = run(Split::Train, &["a"], &["b"]);
        let after = run(Split::Train, &["a", "b"], &[]);
        let decision = GateDecision::Accept;
        let manifest = build(2, &base, &candidate, &patches, &before, Some(&after), None, &decision);
        assert_eq!(manifest.schema, EVOLVE_SCHEMA);
        assert_eq!(manifest.round, 2);
        assert_eq!(manifest.base, base.id);
        assert_eq!(manifest.candidate, candidate.id);
        assert!(manifest.accepted);
        assert!(manifest.reject_reason.is_none());
        assert_eq!(manifest.before, Score { passed: 1, total: 2 });
        assert_eq!(manifest.after, Some(Score { passed: 2, total: 2 }));
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries[0].component, "skills/rust.md", "entries sort by component");
        assert_eq!(manifest.entries[1].component, "system.md");
        assert_eq!(manifest.entries[0].component_kind, ComponentKind::Skill);
        assert_eq!(manifest.entries[1].component_kind, ComponentKind::SystemPrompt);
        assert!(manifest.entries.iter().all(|entry| entry.verdict == PatchedVerdict::Pending));
        assert!(manifest.entries.iter().all(|entry| entry.measured_gain_pp.is_none()));
        assert!(manifest.entries[1].fix.contains("append_section"), "fix: {}", manifest.entries[1].fix);

        let rebuilt = build(2, &base, &candidate, &patches, &before, Some(&after), None, &decision);
        let ids: Vec<&String> = manifest.entries.iter().map(|entry| &entry.id).collect();
        let rebuilt_ids: Vec<&String> = rebuilt.entries.iter().map(|entry| &entry.id).collect();
        assert_eq!(ids, rebuilt_ids, "entry ids are stable for the same round/patch");

        let rejected = build(2, &base, &candidate, &patches, &before, Some(&after), None, &GateDecision::reject("too small"));
        assert!(!rejected.accepted);
        assert_eq!(rejected.reject_reason.as_deref(), Some("too small"));
    }

    #[test]
    fn build_handles_multi_component_and_unresolved_paths() {
        let base = bundle(&[("system.md", "# System\n")], 0);
        let candidate = bundle(&[("system.md", "# System\n"), ("middleware.md", "m\n")], 0);
        let multi = patch(
            crate::evolve::patch::PatchKind::InfraChange,
            vec![op(crate::evolve::patch::PatchAction::AppendSection, "system.md", Some("A")), op(crate::evolve::patch::PatchAction::AppendSection, "middleware.md", Some("B"))],
            &[],
            &[],
        );
        let empty = patch(crate::evolve::patch::PatchKind::LoopLogic, Vec::new(), &[], &[]);
        let unknown = patch(crate::evolve::patch::PatchKind::PromptRuleAdd, vec![op(crate::evolve::patch::PatchAction::AppendSection, "", Some("x"))], &[], &[]);
        let before = run(Split::Train, &[], &["a"]);
        let manifest = build(1, &base, &candidate, &[multi, empty, unknown], &before, None, None, &GateDecision::Accept);
        assert_eq!(manifest.entries.len(), 3);
        assert!(manifest.after.is_none());
        assert!(manifest.entries.iter().any(|entry| entry.component == "(multi)"));
        let unresolved = manifest.entries.iter().find(|entry| entry.component == "(multi)" && entry.note.as_deref().unwrap_or("").contains("unresolved"));
        assert!(unresolved.is_some(), "entries without a resolvable path carry a note");
        assert!(manifest.entries.iter().filter(|entry| entry.component == "(multi)").all(|entry| entry.fix == "(no ops)" || entry.fix.contains("append_section")));
    }

    #[test]
    fn verify_predictions_assigns_all_verdicts() {
        let before = run(Split::Train, &["c"], &["a", "b"]);
        let after = run(Split::Train, &["a"], &["b", "c"]);
        let patches = vec![
            patch(crate::evolve::patch::PatchKind::PromptRuleAdd, vec![op(crate::evolve::patch::PatchAction::AppendSection, "system.md", Some("A"))], &["a"], &[]),
            patch(crate::evolve::patch::PatchKind::ToolImplFix, vec![op(crate::evolve::patch::PatchAction::ReplaceBlock, "system.md", Some("B"))], &["a", "b"], &[]),
            patch(crate::evolve::patch::PatchKind::SkillAdd, vec![op(crate::evolve::patch::PatchAction::AddSkill, "", Some("x"))], &["b"], &[]),
            patch(crate::evolve::patch::PatchKind::MemoryPolicy, vec![op(crate::evolve::patch::PatchAction::SetParam, "policy/verify.json", Some("a.b"))], &["a"], &["c"]),
            patch(crate::evolve::patch::PatchKind::LoopLogic, vec![op(crate::evolve::patch::PatchAction::AppendSection, "system.md", Some("C"))], &[], &[]),
        ];
        let base = bundle(&[("system.md", "# System\n")], 1);
        let mut manifest = build(1, &base, &base, &patches, &before, Some(&after), None, &GateDecision::Accept);
        let verdicts = verify_predictions(&mut manifest, &before, &after).expect("verify");
        assert_eq!(verdicts.len(), 5);
        let by_verdict = |wanted: PatchedVerdict| verdicts.iter().filter(|(_, verdict)| *verdict == wanted).count();
        assert_eq!(by_verdict(PatchedVerdict::Confirmed), 1);
        assert_eq!(by_verdict(PatchedVerdict::Partial), 1);
        assert_eq!(by_verdict(PatchedVerdict::Refuted), 1);
        assert_eq!(by_verdict(PatchedVerdict::Regressed), 1);
        assert_eq!(by_verdict(PatchedVerdict::Pending), 1);
        for entry in &manifest.entries {
            assert_eq!(entry.measured_gain_pp, Some(0.0), "run-wide delta, not per entry");
        }
        // The regressed entry names its casualty in the note.
        assert!(manifest.entries.iter().any(|entry| entry.note.as_deref().unwrap_or("").contains('c')));

        let improved = run(Split::Train, &["a", "b", "c"], &[]);
        let mut manifest = build(1, &base, &base, &patches, &before, Some(&improved), None, &GateDecision::Accept);
        verify_predictions(&mut manifest, &before, &improved).expect("verify improved");
        assert!(manifest.entries.iter().all(|entry| entry.measured_gain_pp == Some(66.67)));
        assert!(manifest.entries.iter().all(|entry| entry.verdict != PatchedVerdict::Regressed));
    }

    #[test]
    fn reflect_splits_four_buckets() {
        let before = run(Split::Train, &["regressed", "passing"], &["fixed", "failing"]);
        let after = run(Split::Train, &["fixed", "passing"], &["regressed", "failing"]);
        let tracked: Vec<String> = ["fixed", "regressed", "failing", "passing", "unknown", "fixed"].iter().map(|task| task.to_string()).collect();
        let reflection = reflect(&before, &after, &tracked);
        assert_eq!(reflection.fixed, vec!["fixed".to_string()]);
        assert_eq!(reflection.regressed, vec!["regressed".to_string()]);
        assert_eq!(reflection.still_failing, vec!["failing".to_string(), "unknown".to_string()]);
        assert_eq!(reflection.still_passing, vec!["passing".to_string()]);

        let learned = lessons(&reflection);
        assert!(!learned.is_empty());
        assert!(learned.len() <= 4);
        assert!(learned[0].contains("fixed 1 task(s)"), "lessons: {learned:?}");
        assert!(learned[1].contains("regressed"));
        assert_eq!(reflect(&before, &after, &[]), Reflection::default(), "no tracked tasks, no buckets");
        assert!(lessons(&Reflection::default()).is_empty(), "empty reflection has no lessons");
    }
}
