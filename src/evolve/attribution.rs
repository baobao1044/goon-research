//! Causal attribution: which patch actually caused which change?
//!
//! A round can propose several patches at once, and then a single before/after measurement cannot
//! say which one did anything. This module runs the controlled experiment the papers describe but
//! their round-based evaluation folds together:
//!
//! - baseline control (no patches),
//! - individual ablation (each patch alone),
//! - combined treatment (all patches together),
//! - repeated trials per arm, with the same model, tasks, timeouts and environment.
//!
//! It also computes the three efficiency metrics the round gate alone does not express:
//! success per 1K tokens, per-patch utility, and the train/dev generalization gap.
//!
//! Everything here is additive: it never mutates the workspace bundle.

use crate::evolve::eval::{self, EvalOptions};
use crate::evolve::patch::{apply_all, EditScope, TargetedPatch};
use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One arm of the experiment: a name plus the patches it applies.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub id: String,
    pub label: String,
    pub patches: Vec<TargetedPatch>,
}

impl Arm {
    pub fn baseline() -> Self {
        Self { id: "baseline".into(), label: "baseline (no patches)".into(), patches: vec![] }
    }

    pub fn single(index: usize, patch: &TargetedPatch) -> Self {
        Self { id: format!("patch-{}", index + 1), label: format!("{} on {}", patch.kind.as_str(), patch.primary_component()), patches: vec![patch.clone()] }
    }

    pub fn combined(patches: &[TargetedPatch]) -> Self {
        Self { id: "combined".into(), label: format!("all {} patches", patches.len()), patches: patches.to_vec() }
    }

    pub fn is_baseline(&self) -> bool {
        self.patches.is_empty()
    }
}

/// Builds the arms for one attribution experiment: control, one arm per patch, and the combined arm.
pub fn arms(patches: &[TargetedPatch]) -> Vec<Arm> {
    let mut arms = vec![Arm::baseline()];
    arms.extend(patches.iter().enumerate().map(|(index, patch)| Arm::single(index, patch)));
    if patches.len() > 1 {
        arms.push(Arm::combined(patches));
    }
    arms
}

/// Aggregate of one arm across all its trials.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArmResult {
    pub id: String,
    pub label: String,
    pub trials: u32,
    pub passed: u32,
    pub total: u32,
    pub tokens: u64,
    /// Bundle id the arm was measured with (`baseline` when no patch applied).
    pub bundle: Option<String>,
}

impl ArmResult {
    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f64 / self.total as f64
        }
    }

    pub fn tokens_per_task(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.tokens as f64 / self.total as f64
        }
    }
}

/// Effect of one patch relative to the baseline control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchEffect {
    pub arm: String,
    pub label: String,
    pub success_delta_pp: f64,
    pub tokens_delta_per_task: f64,
    /// `Delta success / Delta cost`, with the sign conventions documented on [`patch_utility`].
    pub utility: Option<f64>,
    pub verdict: AttributionVerdict,
    /// Percentage points, so two runs are comparable across task counts.
    pub tokens_delta_pct: Option<f64>,
}

/// What the trial evidence supports for one patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionVerdict {
    /// Improved success and did not cost more per task.
    Helpful,
    /// Improved success, but the extra tokens are not accounted for.
    HelpfulButCostly,
    /// Made success worse.
    Harmful,
    /// No measurable difference in success, but it needed fewer tokens per task.
    NeutralButCheaper,
    /// No measurable difference across all trials.
    Neutral,
    /// Too few trials to separate the arm from the control.
    Inconclusive,
}

impl AttributionVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            AttributionVerdict::Helpful => "helpful",
            AttributionVerdict::HelpfulButCostly => "helpful_but_costly",
            AttributionVerdict::Harmful => "harmful",
            AttributionVerdict::NeutralButCheaper => "neutral_but_cheaper",
            AttributionVerdict::Neutral => "neutral",
            AttributionVerdict::Inconclusive => "inconclusive",
        }
    }
}

/// The complete experiment report, durable on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttributionReport {
    pub schema: u64,
    pub created: String,
    pub round: u32,
    pub trials: u32,
    /// Identity of the measured environment; two reports are only comparable when these match.
    pub model: Option<String>,
    pub task_set: String,
    pub dev_set: String,
    pub timeout_ms: u64,
    pub stub: bool,
    pub arms: Vec<ArmResult>,
    pub effects: Vec<PatchEffect>,
    pub train_rate: f64,
    pub dev_rate: f64,
    /// `P_train - P_dev` for the combined arm (positive means train-only improvement).
    pub generalization_gap: Option<f64>,
    /// Optional operator-facing caveats (tiny trial counts, single-arm runs, ...).
    pub notes: Vec<String>,
}

impl AttributionReport {
    /// Successful tasks per 1000 tokens: how much work one thousand tokens buys.
    pub fn success_per_1k_tokens(&self) -> Option<f64> {
        let tokens: u64 = self.arms.iter().map(|arm| arm.tokens).sum();
        let passed: u32 = self.arms.iter().map(|arm| arm.passed).sum();
        (tokens > 0).then(|| success_per_1k(passed, tokens))
    }

    pub fn arm(&self, id: &str) -> Option<&ArmResult> {
        self.arms.iter().find(|arm| arm.id == id)
    }

    /// The arm whose effect on the given task should be attributed first (largest success delta).
    pub fn strongest(&self) -> Option<&PatchEffect> {
        self.effects
            .iter()
            .filter(|effect| effect.verdict != AttributionVerdict::Harmful)
            .max_by(|left, right| left.success_delta_pp.partial_cmp(&right.success_delta_pp).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Successful tasks per 1000 tokens (`E = success / tokens * 1000`).
pub fn success_per_1k(passed: u32, tokens: u64) -> f64 {
    if tokens == 0 {
        return 0.0;
    }
    passed as f64 * 1000.0 / tokens as f64
}

/// Patch utility `U = Delta success / Delta cost`.
///
/// Both inputs are per-run aggregates, so `delta_success` is a fraction in 0..=1 and `delta_cost`
/// is tokens per task. Three cases need naming rather than arithmetic:
/// - the patch saved cost (`delta_cost < 0`): utility is `None` and the caller reports "saves
///   cost", because dividing by a negative would flip the meaning;
/// - the patch cost nothing extra (`delta_cost == 0`): `None`, no denominator;
/// - no success change: utility is `0.0` even when cost changed.
pub fn patch_utility(delta_success: f64, delta_cost: f64) -> Option<f64> {
    if delta_success.abs() < f64::EPSILON {
        return Some(0.0);
    }
    if delta_cost <= 0.0 {
        return None;
    }
    Some(delta_success / delta_cost)
}

/// Generalization gap `G = P_train - P_dev`: positive means the harness fits the train split.
pub fn generalization_gap(train: f64, dev: f64) -> f64 {
    train - dev
}

/// Threshold below which an arm/control difference is treated as noise rather than a result.
const NOISE_PP: f64 = 1.0;

/// Compares one arm against the baseline control and classifies the effect.
pub fn effect_for(control: &ArmResult, arm: &ArmResult, trials: u32) -> PatchEffect {
    let success_delta_pp = (arm.pass_rate() - control.pass_rate()) * 100.0;
    let tokens_delta_per_task = arm.tokens_per_task() - control.tokens_per_task();
    let tokens_delta_pct = (control.tokens_per_task() > 0.0).then(|| tokens_delta_per_task / control.tokens_per_task() * 100.0);
    let utility = patch_utility(arm.pass_rate() - control.pass_rate(), tokens_delta_per_task);
    // One trial per arm cannot separate an effect from model noise; say so instead of guessing.
    let verdict = if trials < 2 {
        AttributionVerdict::Inconclusive
    } else if success_delta_pp <= -NOISE_PP {
        AttributionVerdict::Harmful
    } else if success_delta_pp >= NOISE_PP {
        if tokens_delta_per_task > 0.0 {
            AttributionVerdict::HelpfulButCostly
        } else {
            AttributionVerdict::Helpful
        }
    } else if tokens_delta_per_task < 0.0 {
        // Same outcomes for less money is still a real effect worth keeping.
        AttributionVerdict::NeutralButCheaper
    } else {
        AttributionVerdict::Neutral
    };
    PatchEffect { arm: arm.id.clone(), label: arm.label.clone(), success_delta_pp, tokens_delta_per_task, utility, verdict, tokens_delta_pct }
}

/// Knobs for one attribution experiment.
#[derive(Clone, Debug)]
pub struct ExperimentOptions {
    pub round: u32,
    /// Repetitions per arm. Two is the minimum that can distinguish signal from noise.
    pub trials: u32,
    pub eval_path: std::path::PathBuf,
    pub dev_path: Option<std::path::PathBuf>,
    pub max_tasks: usize,
    pub concurrency: u32,
    pub retries: u32,
    pub goon_bin: std::path::PathBuf,
    pub stub: bool,
    pub data_dir: Option<std::path::PathBuf>,
}

impl Default for ExperimentOptions {
    fn default() -> Self {
        Self {
            round: 1,
            trials: 2,
            eval_path: std::path::PathBuf::from("eval/train.json"),
            dev_path: None,
            max_tasks: 10,
            concurrency: 3,
            retries: 1,
            goon_bin: std::path::PathBuf::from("goon"),
            stub: false,
            data_dir: None,
        }
    }
}

/// Runs the full experiment: every arm, `trials` times, on the train split (and dev when given).
pub async fn run(store: &HarnessStore, _workspace: &std::path::Path, base_files: &BTreeMap<String, String>, patches: &[TargetedPatch], opts: &ExperimentOptions) -> Result<AttributionReport> {
    let arms = arms(patches);
    let mut results: Vec<ArmResult> = Vec::with_capacity(arms.len());
    let scope = EditScope::default_for_bundle();
    for arm in &arms {
        let mut files = base_files.clone();
        if !arm.is_baseline() {
            apply_all(&mut files, &arm.patches, &scope).with_context(|| format!("arm {} applies its patches", arm.id))?;
        }
        let bundle = crate::evolve::bundle::snapshot(store, &files, None, opts.round, Variant::Code)?;
        let mut aggregate = ArmResult { id: arm.id.clone(), label: arm.label.clone(), bundle: Some(bundle.id.clone()), ..ArmResult::default() };
        for trial in 0..opts.trials.max(1) {
            let set = eval::load(&opts.eval_path, Split::Train).with_context(|| format!("load eval set {}", opts.eval_path.display()))?;
            let summary = eval::run(&set, &eval_options(opts, &bundle.id)).await?;
            aggregate.trials += 1;
            aggregate.passed += summary.passed() as u32;
            aggregate.total += summary.total() as u32;
            aggregate.tokens += summary.tokens();
            let _ = trial;
        }
        results.push(aggregate);
    }

    let control = results.first().cloned().unwrap_or_default();
    let effects: Vec<PatchEffect> = results.iter().filter(|arm| arm.id != control.id && arm.id != "combined").map(|arm| effect_for(&control, arm, opts.trials)).collect();

    // Generalization: the combined arm on train vs dev (the baseline control is the honest floor).
    let dev_set = opts.dev_path.clone().unwrap_or_else(|| opts.eval_path.with_file_name("dev.json"));
    let combined = results.iter().find(|arm| arm.id == "combined").or_else(|| results.iter().find(|arm| arm.id != control.id));
    let mut dev_rate = 0.0;
    let mut dev_measured = false;
    let train_rate = combined.map(ArmResult::pass_rate).unwrap_or(0.0);
    if let Some(combined) = combined {
        if let Ok(set) = eval::load(&dev_set, Split::Dev) {
            if !set.tasks.is_empty() {
                if let Some(bundle) = combined.bundle.as_deref() {
                    let summary = eval::run(&set, &eval_options(opts, bundle)).await?;
                    dev_rate = summary.pass_rate();
                    dev_measured = true;
                }
            }
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if opts.trials < 2 {
        notes.push("trials < 2: arms cannot be separated from model noise, every effect is inconclusive".to_string());
    }
    if patches.len() > 1 {
        notes.push(format!("{} patches measured individually and together; interactions are real and a single arm cannot reveal them", patches.len()));
    }
    if !dev_measured {
        notes.push("dev split missing or empty: generalization gap cannot be computed".to_string());
    }
    if control.total == 0 {
        notes.push("baseline arm produced no measurements: the experiment says nothing".to_string());
    }

    let report = AttributionReport {
        schema: EVOLVE_SCHEMA,
        created: now_rfc3339(),
        round: opts.round,
        trials: opts.trials,
        model: std::env::var("GOON_MODEL").ok().filter(|value| !value.is_empty()),
        task_set: file_signature(&opts.eval_path),
        dev_set: file_signature(&dev_set),
        timeout_ms: 0,
        stub: opts.stub,
        arms: results,
        effects,
        train_rate,
        dev_rate,
        generalization_gap: dev_measured.then(|| generalization_gap(train_rate, dev_rate)),
        notes,
    };
    store.save_attribution(opts.round, &report)?;
    Ok(report)
}

fn eval_options(opts: &ExperimentOptions, bundle: &str) -> EvalOptions {
    EvalOptions {
        goon_bin: opts.goon_bin.clone(),
        workspace: opts.eval_path.parent().map(|parent| parent.to_path_buf()).unwrap_or_default(),
        bundle: Some(bundle.to_string()),
        data_dir: opts.data_dir.clone(),
        stub: opts.stub,
        max_tasks: opts.max_tasks,
        timeout_ms: 0,
        variant: Some(Variant::Code),
        concurrency: opts.concurrency,
        retries: opts.retries,
    }
}

/// Stable signature of a task set file: the report records what it measured.
fn file_signature(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => short_hash(&bytes),
        Err(_) => "missing".to_string(),
    }
}

impl HarnessStore {
    pub fn attribution_dir(&self) -> std::path::PathBuf {
        self.root().join("attribution")
    }

    pub fn save_attribution(&self, round: u32, report: &AttributionReport) -> Result<()> {
        self.ensure()?;
        let path = self.attribution_dir().join(format!("round-{round:03}.json"));
        let body = serde_json::to_string_pretty(report).context("encode attribution report")?;
        self.write_atomic(&path, format!("{body}\n").as_bytes())
    }

    pub fn load_attribution(&self, round: u32) -> Result<Option<AttributionReport>> {
        let path = self.attribution_dir().join(format!("round-{round:03}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let body = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?))
    }
}

/// Human-readable table of an experiment (one line per arm, then the metric block).
pub fn render(report: &AttributionReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("causal attribution | round {} | {} trial(s) per arm\n", report.round, report.trials));
    out.push_str(&format!("  task set {}  dev set {}\n", report.task_set, report.dev_set));
    out.push_str("\narm                 pass rate     tokens   tok/task\n");
    for arm in &report.arms {
        out.push_str(&format!("{:<18} {:>6.1}%  {:>9}  {:>8.0}\n", truncate_label(&arm.id), arm.pass_rate() * 100.0, arm.tokens, arm.tokens_per_task()));
    }
    if !report.effects.is_empty() {
        out.push_str("\npatch effect vs baseline\n");
        for effect in &report.effects {
            out.push_str(&format!(
                "{:<18} {:>+6.1}pp  {:>+9.0} tok/task  utility {:<8} {}\n",
                truncate_label(&effect.arm),
                effect.success_delta_pp,
                effect.tokens_delta_per_task,
                effect.utility.map(|value| format!("{value:.4}")).unwrap_or_else(|| "n/a".into()),
                effect.verdict.as_str()
            ));
        }
    }
    out.push_str("\nmetrics\n");
    match report.success_per_1k_tokens() {
        Some(value) => out.push_str(&format!("  success per 1K tokens   {value:.3}\n")),
        None => out.push_str("  success per 1K tokens   n/a (no tokens reported)\n"),
    }
    out.push_str(&format!(
        "  generalization gap      {} (train {:.1}% - dev {:.1}%)\n",
        match report.generalization_gap {
            Some(gap) => format!("{gap:+.1}pp"),
            None => "n/a".to_string(),
        },
        report.train_rate * 100.0,
        report.dev_rate * 100.0
    ));
    for note in &report.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

fn truncate_label(label: &str) -> String {
    if label.chars().count() <= 18 {
        label.to_string()
    } else {
        format!("{}…", label.chars().take(17).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolve::patch::{PatchAction, PatchKind, PatchOp};

    fn patch(kind: PatchKind) -> TargetedPatch {
        TargetedPatch {
            kind,
            rationale: "r".into(),
            evidence: vec![],
            ops: vec![PatchOp { action: PatchAction::AppendSection, component: "system.md".into(), target: Some("T".into()), content: "c".into() }],
            prediction: Prediction::default(),
        }
    }

    fn arm(id: &str, passed: u32, total: u32, tokens: u64) -> ArmResult {
        ArmResult { id: id.into(), label: id.into(), trials: 2, passed, total, tokens, bundle: None }
    }

    #[test]
    fn arms_are_control_each_patch_then_combined() {
        let patches = vec![patch(PatchKind::PromptRuleAdd), patch(PatchKind::ToolDescFix)];
        let built = arms(&patches);
        assert_eq!(built.len(), 4);
        assert_eq!(built[0].id, "baseline");
        assert!(built[0].is_baseline());
        assert_eq!(built[1].id, "patch-1");
        assert_eq!(built[2].id, "patch-2");
        assert_eq!(built[3].id, "combined");
        // A single patch needs no combined arm: it *is* the combination.
        assert_eq!(arms(&patches[..1]).len(), 2);
        assert_eq!(arms(&[]).len(), 1);
    }

    #[test]
    fn success_per_1k_tokens_is_a_ratio_not_a_percentage() {
        assert_eq!(success_per_1k(0, 1000), 0.0);
        assert!((success_per_1k(2, 1000) - 2.0).abs() < 1e-9);
        assert!((success_per_1k(1, 500) - 2.0).abs() < 1e-9);
        // No tokens means the metric is undefined, not zero-success.
        assert_eq!(success_per_1k(3, 0), 0.0);
    }

    #[test]
    fn patch_utility_handles_zero_and_negative_cost() {
        // +10% success for +100 tokens/task.
        assert_eq!(patch_utility(0.1, 100.0), Some(0.001));
        // Saving cost is not a division opportunity: the caller reports "saves cost".
        assert_eq!(patch_utility(0.1, -50.0), None);
        // Zero extra cost has no denominator either.
        assert_eq!(patch_utility(0.1, 0.0), None);
        // No success change is zero utility even when cost moved.
        assert_eq!(patch_utility(0.0, 100.0), Some(0.0));
    }

    #[test]
    fn generalization_gap_is_train_minus_dev() {
        assert!((generalization_gap(1.0, 0.5) - 0.5).abs() < 1e-9);
        assert!((generalization_gap(0.5, 1.0) + 0.5).abs() < 1e-9);
        assert_eq!(generalization_gap(0.75, 0.75), 0.0);
    }

    #[test]
    fn effects_classify_helpful_harmful_neutral_and_costly() {
        let control = arm("baseline", 4, 10, 10_000);
        let helpful = effect_for(&control, &arm("patch-1", 8, 10, 9_000), 2);
        assert_eq!(helpful.verdict, AttributionVerdict::Helpful);
        assert!((helpful.success_delta_pp - 40.0).abs() < 1e-9);
        // Cheaper *and* better has no ratio to report; the sign of the cost delta would lie.
        assert_eq!(helpful.utility, None);
        assert!(helpful.tokens_delta_per_task < 0.0);

        // Equal or higher cost with better success does have a utility: +40pp for +200 tokens/task.
        let paid = effect_for(&control, &arm("patch-paid", 8, 10, 12_000), 2);
        assert_eq!(paid.verdict, AttributionVerdict::HelpfulButCostly);
        assert!((paid.utility.unwrap() - 0.4 / 200.0).abs() < 1e-12);

        let costly = effect_for(&control, &arm("patch-2", 8, 10, 20_000), 2);
        assert_eq!(costly.verdict, AttributionVerdict::HelpfulButCostly);
        assert!(costly.tokens_delta_pct.unwrap() > 0.0);

        let harmful = effect_for(&control, &arm("patch-3", 2, 10, 10_000), 2);
        assert_eq!(harmful.verdict, AttributionVerdict::Harmful);

        let neutral = effect_for(&control, &arm("patch-4", 4, 10, 10_000), 2);
        assert_eq!(neutral.verdict, AttributionVerdict::Neutral);
        assert_eq!(neutral.utility, Some(0.0));

        // Same success for fewer tokens is an effect: keep it and say so.
        let cheaper = effect_for(&control, &arm("patch-cheap", 4, 10, 8_000), 2);
        assert_eq!(cheaper.verdict, AttributionVerdict::NeutralButCheaper);
        assert!(cheaper.tokens_delta_per_task < 0.0);
        assert_eq!(cheaper.tokens_delta_pct, Some(-20.0));

        // One trial is not evidence.
        assert_eq!(effect_for(&control, &arm("patch-5", 10, 10, 1), 1).verdict, AttributionVerdict::Inconclusive);
    }

    #[test]
    fn render_reports_the_three_metrics_and_the_notes() {
        let report = AttributionReport {
            schema: EVOLVE_SCHEMA,
            created: now_rfc3339(),
            round: 1,
            trials: 2,
            model: Some("test-model".into()),
            task_set: "abc".into(),
            dev_set: "def".into(),
            timeout_ms: 0,
            stub: true,
            arms: vec![arm("baseline", 2, 4, 2_000), arm("combined", 4, 4, 2_000)],
            effects: vec![effect_for(&arm("baseline", 2, 4, 2_000), &arm("patch-1", 4, 4, 2_000), 2)],
            train_rate: 1.0,
            dev_rate: 0.5,
            generalization_gap: Some(0.5),
            notes: vec!["trials < 2: noise".into()],
        };
        let text = render(&report);
        assert!(text.contains("success per 1K tokens"));
        assert!(text.contains("generalization gap"));
        assert!(text.contains("helpful"));
        assert!(text.contains("note:"));
        // success per 1K over every arm of the report.
        assert!((report.success_per_1k_tokens().unwrap() - (6.0 * 1000.0 / 4_000.0)).abs() < 1e-9);
        assert!(report.strongest().is_some());
    }
}
