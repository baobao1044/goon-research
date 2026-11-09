//! Round orchestration: the closed loop that turns traces into a durable harness update.
//!
//! 1. snapshot the current bundle (base)
//! 2. digest session traces into an evidence corpus
//! 3. ask a `Proposer` for targeted patches
//! 4. apply patches to a staged copy, validate, snapshot the candidate
//! 5. evaluate train before/after, then the held-out dev split
//! 6. acceptance gate (seesaw + dev), write manifest, keep or revert
//! 7. next round turns the stored prediction into a verdict
#![allow(dead_code)]

use crate::evolve::bundle::{self, HarnessContext};
use crate::evolve::digest;
use crate::evolve::eval::{self, EvalOptions};
use crate::evolve::manifest::{self, AcceptanceGate};
use crate::evolve::patch::{self, EditScope, PatchAction, PatchKind, PatchOp, TargetedPatch};
use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Everything a proposer may read. It never sees the eval harness or the verifier source.
pub struct ProposeContext<'a> {
    pub corpus: &'a EvidenceCorpus,
    pub digests: &'a [digest::TaskDigest],
    pub files: &'a BTreeMap<String, String>,
    pub bundle: &'a HarnessBundle,
    pub round: u32,
    pub max_rounds: u32,
    pub rule: Option<&'a str>,
}

/// A source of targeted patches (deterministic heuristic, or a model call).
pub trait Proposer: Send + Sync {
    fn propose<'a>(&'a self, ctx: &'a ProposeContext<'a>) -> BoxFuture<'a, Result<Vec<TargetedPatch>>>;
}

// ---------------------------------------------------------------------------------------------
// Heuristic proposer: offline, deterministic, evidence-driven.
// ---------------------------------------------------------------------------------------------

/// Derives patches from clustered evidence without a model: the same observability inputs,
/// no token cost. Structured components win over prompt prose (AHE ablation: memory +5.6pp,
/// tools +3.3pp, middleware +2.2pp, prompt-only -2.3pp), so the ordering below prefers them.
#[derive(Clone, Debug, Default)]
pub struct HeuristicProposer {
    pub max_patches: usize,
}

impl HeuristicProposer {
    pub fn new(max_patches: usize) -> Self {
        Self { max_patches }
    }

    fn component_patch(files: &BTreeMap<String, String>, kind: ComponentKind, action: PatchAction, target: Option<&str>, content: String) -> TargetedPatch {
        let path = match kind {
            ComponentKind::SystemPrompt => SYSTEM_PROMPT_PATH,
            ComponentKind::ToolGuidance => TOOL_GUIDANCE_PATH,
            ComponentKind::Middleware => MIDDLEWARE_PATH,
            ComponentKind::MemoryPolicy => MEMORY_PATH,
            ComponentKind::VerifyPolicy => VERIFY_POLICY_PATH,
            ComponentKind::ContextPolicy => CONTEXT_POLICY_PATH,
            ComponentKind::JitPolicy => JIT_POLICY_PATH,
            _ => MIDDLEWARE_PATH,
        };
        let _ = files;
        TargetedPatch {
            kind: match kind {
                ComponentKind::ContextPolicy => PatchKind::ContextPolicy,
                ComponentKind::MemoryPolicy => PatchKind::MemoryPolicy,
                ComponentKind::ToolGuidance => PatchKind::ToolDescFix,
                ComponentKind::Middleware => PatchKind::PromptRuleAdd,
                _ => PatchKind::PromptRuleAdd,
            },
            rationale: format!("{} x{} clustered from traces", kind.as_str(), 0),
            evidence: vec![],
            ops: vec![PatchOp { action, component: path.to_string(), target: target.map(str::to_string), content }],
            prediction: Prediction::default(),
        }
    }
}

impl Proposer for HeuristicProposer {
    fn propose<'a>(&'a self, ctx: &'a ProposeContext<'a>) -> BoxFuture<'a, Result<Vec<TargetedPatch>>> {
        Box::pin(async move {
            let limit = if self.max_patches == 0 { 3 } else { self.max_patches };
            let mut patches: Vec<TargetedPatch> = vec![];
            let top = ctx.corpus.top(limit.max(3));
            let mut overflow_seen = false;
            for item in top {
                if patches.len() >= limit {
                    break;
                }
                let evidence =
                    vec![EvidenceRef::new(item.sessions.first().map(String::as_str).unwrap_or("unknown"), item.tasks.first().map(String::as_str).unwrap_or("unknown"), item.signature.as_str())];
                match &item.signal {
                    FailureSignal::ContextOverflow => {
                        if overflow_seen {
                            continue;
                        }
                        overflow_seen = true;
                        let ratio = bundle::context(ctx.files).context_conversation.unwrap_or(0.55).min(0.7) * 0.85;
                        let mut proposal = HeuristicProposer::component_patch(ctx.files, ComponentKind::ContextPolicy, PatchAction::SetParam, Some("conversation_ratio"), json!(ratio).to_string());
                        proposal.kind = PatchKind::ContextPolicy;
                        proposal.rationale = format!("context overflow x{}: shrink conversation share", item.occurrences);
                        proposal.evidence = evidence;
                        proposal.prediction = Prediction { expected_fix: item.tasks.clone(), at_risk: vec![], expected_gain_pp: 2.0, confidence: 0.4 };
                        patches.push(proposal);
                    }
                    FailureSignal::VerifyFail => {
                        let mut proposal = HeuristicProposer::component_patch(
                            ctx.files,
                            ComponentKind::Middleware,
                            PatchAction::AppendSection,
                            Some("verification"),
                            "Run the configured verify command before claiming a task is done; quote the failing stage verbatim when it fails.".to_string(),
                        );
                        proposal.rationale = format!("verify failures x{}: enforce verify-before-claim", item.occurrences);
                        proposal.evidence = evidence;
                        proposal.prediction = Prediction { expected_fix: item.tasks.clone(), at_risk: vec![], expected_gain_pp: 1.5, confidence: 0.35 };
                        patches.push(proposal);
                    }
                    FailureSignal::ToolError => {
                        let mut proposal = HeuristicProposer::component_patch(
                            ctx.files,
                            ComponentKind::ToolGuidance,
                            PatchAction::AppendSection,
                            Some("tool-failure recovery"),
                            "Exit code 1 is normal: re-read the file and retry with an absolute path inside the workspace instead of assuming the tool is missing.".to_string(),
                        );
                        proposal.rationale = format!("tool errors x{}: add recovery guidance", item.occurrences);
                        proposal.evidence = evidence;
                        proposal.prediction = Prediction { expected_fix: item.tasks.clone(), at_risk: vec![], expected_gain_pp: 1.0, confidence: 0.3 };
                        patches.push(proposal);
                    }
                    FailureSignal::NoAnswer | FailureSignal::ParseError => {
                        let mut proposal = HeuristicProposer::component_patch(
                            ctx.files,
                            ComponentKind::Middleware,
                            PatchAction::AppendSection,
                            Some("final answer"),
                            "End every turn with a non-empty final answer in plain text; never end on a tool call alone.".to_string(),
                        );
                        proposal.rationale = format!("missing/empty answers x{}", item.occurrences);
                        proposal.evidence = evidence;
                        proposal.prediction = Prediction { expected_fix: item.tasks.clone(), at_risk: vec![], expected_gain_pp: 1.0, confidence: 0.3 };
                        patches.push(proposal);
                    }
                    _ => {}
                }
            }
            if patches.is_empty() {
                // Nothing actionable: emit a single memory lesson so the round is still falsifiable.
                let mut proposal = HeuristicProposer::component_patch(
                    ctx.files,
                    ComponentKind::MemoryPolicy,
                    PatchAction::AppendSection,
                    Some("lessons"),
                    format!("Round {}: no dominant failure cluster; keep the current harness.", ctx.round),
                );
                proposal.rationale = "no failure cluster above threshold".into();
                proposal.prediction = Prediction { expected_fix: vec![], at_risk: vec![], expected_gain_pp: 0.0, confidence: 0.0 };
                patches.push(proposal);
            }
            Ok(patches)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Model proposer: one bounded generation call per round.
// ---------------------------------------------------------------------------------------------

/// Asks the configured model for JSON patches, bounded by the same edit language. A malformed
/// answer is an error, not a silent empty round: predictions must stay falsifiable.
pub struct ModelProposer {
    pub model: std::sync::Arc<dyn crate::model::LanguageModel>,
    pub system: String,
    pub max_output_tokens: u64,
}

fn proposer_prompt(ctx: &ProposeContext<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("Round {}/{} of harness evolution. Propose at most 3 targeted patches.\n\n", ctx.round, ctx.max_rounds));
    if let Some(rule) = ctx.rule {
        out.push_str(&format!("Operator rule (highest priority): {rule}\n\n"));
    }
    out.push_str("Editable components:\n");
    for path in ctx.files.keys() {
        if let Some(kind) = component_kind_for_path(path) {
            out.push_str(&format!("- {path} ({})\n", kind.as_str()));
        }
    }
    out.push_str("\nEvidence (ranked clusters):\n");
    out.push_str(&digest::corpus_prompt(ctx.corpus, 8, 2500));
    out.push_str("\nTask digests:\n");
    for item in ctx.digests.iter().take(8) {
        out.push_str(&format!(
            "- {} {} steps={} failure={}\n",
            item.task_id,
            if item.passed { "pass" } else { "fail" },
            item.steps,
            item.failure.as_ref().map(|signal| signal.as_str().to_string()).unwrap_or_else(|| "-".into())
        ));
    }
    out.push_str(
        "\nReply with JSON only: {\"patches\":[{\"kind\":\"<patch_kind>\",\"rationale\":\"...\",\"evidence\":[{\"session_id\":\"...\",\"task_id\":\"...\",\"signal\":\"...\"}],\"prediction\":{\"expected_fix\":[\"task\"],\"at_risk\":[\"task\"],\"expected_gain_pp\":0.0,\"confidence\":0.0},\"ops\":[{\"action\":\"append_section|replace_block|insert_bullet|set_param|add_skill|remove_skill\",\"component\":\"<component path>\",\"target\":\"<optional marker/heading>\",\"content\":\"<text or JSON value>\"}]}]}\n",
    );
    out
}

impl Proposer for ModelProposer {
    fn propose<'a>(&'a self, ctx: &'a ProposeContext<'a>) -> BoxFuture<'a, Result<Vec<TargetedPatch>>> {
        Box::pin(async move {
            let request = crate::model::ModelRequest {
                system: Some(self.system.clone()),
                messages: vec![crate::model::user_text(&proposer_prompt(ctx))],
                tools: vec![],
                max_output_tokens: self.max_output_tokens.max(1200),
                effort: None,
                cancel: CancellationToken::new(),
                timeout: std::time::Duration::from_secs(180),
            };
            let response = self.model.generate(request).await.map_err(|error| anyhow::anyhow!("proposer model call failed: {error}"))?;
            let patches = patch::parse(&response.text).context("proposer returned unparsable patches")?;
            if patches.is_empty() {
                bail!("proposer returned no patches");
            }
            Ok(patches)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Round execution
// ---------------------------------------------------------------------------------------------

/// Knobs for one round.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub round: u32,
    pub max_rounds: u32,
    pub dry_run: bool,
    pub eval_path: PathBuf,
    pub eval_max_tasks: usize,
    pub goon_bin: PathBuf,
    pub stub: bool,
    pub verify_commands: Vec<String>,
    pub rule: Option<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            round: 1,
            max_rounds: 1,
            dry_run: false,
            eval_path: PathBuf::from("eval/tasks.json"),
            eval_max_tasks: 10,
            goon_bin: PathBuf::from("goon"),
            stub: false,
            verify_commands: vec![],
            rule: None,
        }
    }
}

/// Outcome of one round.
#[derive(Clone, Debug)]
pub struct RoundOutcome {
    pub accepted: bool,
    pub base: HarnessBundle,
    pub candidate: Option<HarnessBundle>,
    pub manifest: Option<ChangeManifest>,
    pub decision: GateDecision,
    pub before: RunSummary,
    pub after: Option<RunSummary>,
    pub dev_before: Option<RunSummary>,
    pub dev_after: Option<RunSummary>,
    pub patches: Vec<TargetedPatch>,
    pub evidence: EvidenceCorpus,
}

/// Validation of the staged bundle before any evaluation budget is spent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub ok: bool,
    pub checks: Vec<String>,
    pub failures: Vec<String>,
}

/// Applies patches to a staged copy and validates it (component schema + optional build/smoke).
pub fn validate_candidate(workspace: &Path, files: &BTreeMap<String, String>, build_cmds: &[String]) -> Result<ValidationReport> {
    let mut report = ValidationReport { ok: true, checks: vec![], failures: vec![] };
    for path in files.keys() {
        if !bundle::is_component_path(path) {
            report.ok = false;
            report.failures.push(format!("unknown component path: {path}"));
        }
    }
    if files.keys().any(|path| path.ends_with(".json")) {
        for (path, body) in files {
            if path.ends_with(".json") && serde_json::from_str::<serde_json::Value>(body).is_err() {
                report.ok = false;
                report.failures.push(format!("invalid JSON component: {path}"));
            }
        }
    }
    let ctx = bundle::context(files);
    for command in &ctx.verify_commands {
        if command.trim().is_empty() {
            report.ok = false;
            report.failures.push("empty verify command".into());
        }
    }
    report.checks.push(format!("{} components", files.len()));
    for command in build_cmds {
        let output = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" }).args(if cfg!(windows) { vec!["/C", command] } else { vec!["-c", command] }).current_dir(workspace).output();
        match output {
            Ok(result) if result.status.success() => report.checks.push(format!("ok: {command}")),
            Ok(result) => {
                report.ok = false;
                let stderr = String::from_utf8_lossy(&result.stderr);
                report.failures.push(format!("failed: {command}: {}", stderr.chars().take(300).collect::<String>()));
            }
            Err(error) => {
                report.ok = false;
                report.failures.push(format!("spawn failed: {command}: {error}"));
            }
        }
    }
    Ok(report)
}

fn variant_of(task: &str) -> Variant {
    crate::evolve::jit::classify_task(task)
}

/// Runs one full evolution round.
pub async fn run_round(
    store: &HarnessStore,
    workspace: &Path,
    sessions_dir: &Path,
    config: &EvolveConfig,
    opts: &EngineOptions,
    proposer: &dyn Proposer,
    cancel: &CancellationToken,
) -> Result<RoundOutcome> {
    store.ensure()?;
    let variant = Variant::Code;

    // A previous prediction is verified by this round's measurement, so reload first.
    let previous = store.load_manifest(opts.round.saturating_sub(1))?;

    let mut files = bundle::load_files(workspace)?;
    if files.is_empty() {
        files = bundle::seed_files(variant);
        bundle::write_files(workspace, &files)?;
    }
    let base = bundle::snapshot(store, &files, previous.as_ref().map(|m| m.candidate.as_str()).filter(|id| !id.is_empty()), opts.round.saturating_sub(1), variant)?;

    // 1. Baseline measurement.
    let before = if opts.dry_run {
        RunSummary::new(Split::Train, Some(base.id.clone()), Some(variant), vec![])
    } else {
        let set = eval::load(&opts.eval_path, Split::Train).with_context(|| format!("load eval set {}", opts.eval_path.display()))?;
        let mut eval_opts = EvalOptions {
            goon_bin: opts.goon_bin.clone(),
            workspace: workspace.to_path_buf(),
            bundle: Some(base.id.clone()),
            data_dir: Some(crate::config::data_dir()),
            stub: opts.stub,
            max_tasks: opts.eval_max_tasks.max(1),
            timeout_ms: 120_000,
            variant: Some(variant),
        };
        if config.max_tasks > 0 {
            eval_opts.max_tasks = eval_opts.max_tasks.min(config.max_tasks);
        }
        eval::run(&set, &eval_opts).await?
    };

    // 2. Experience observability: distill traces (bounded).
    let digest_opts = digest::DigestOptions { max_sessions: 40, max_tasks: 20, max_tokens: if config.token_budget > 0 { config.token_budget.min(8000) } else { 6000 } };
    let (mut corpus, digests) = digest::digest_sessions(sessions_dir, &digest_opts).unwrap_or((EvidenceCorpus::empty(), vec![]));
    corpus.bundle = Some(base.id.clone());
    store.save_evidence(&format!("round-{:03}", opts.round), &corpus)?;

    // 3. Propose.
    let propose_ctx = ProposeContext { corpus: &corpus, digests: &digests, files: &files, bundle: &base, round: opts.round, max_rounds: opts.max_rounds, rule: opts.rule.as_deref() };
    let proposed = proposer.propose(&propose_ctx).await?;
    let order = patch::phased(&proposed, opts.round, opts.max_rounds, config.phased_patches);
    let selected: Vec<TargetedPatch> = order.iter().filter_map(|index| proposed.get(*index)).cloned().collect();
    if selected.is_empty() {
        bail!("no patches selected for round {}", opts.round);
    }
    if cancel.is_cancelled() {
        bail!("cancelled before patch application");
    }

    // 4. Apply to a staged copy and validate.
    let mut candidate_files = files.clone();
    let scope = EditScope::default_for_bundle();
    let reports = patch::apply_all(&mut candidate_files, &selected, &scope).context("apply targeted patches")?;
    let validation = validate_candidate(workspace, &candidate_files, &opts.verify_commands)?;
    if !validation.ok {
        let decision = GateDecision::reject(format!("candidate failed validation: {}", validation.failures.join("; ")));
        let manifest = manifest::build(opts.round, &base, &base, &selected, &before, None, None, &decision);
        let mut manifest = manifest;
        if config.require_dev_gate {
            manifest.entries.iter_mut().for_each(|entry| entry.note = Some("validation failed".into()));
        }
        store.save_manifest(&manifest)?;
        return Ok(RoundOutcome {
            accepted: false,
            base,
            candidate: None,
            manifest: Some(manifest),
            decision,
            before,
            after: None,
            dev_before: None,
            dev_after: None,
            patches: selected,
            evidence: corpus,
        });
    }
    let candidate = bundle::snapshot(store, &candidate_files, Some(&base.id), opts.round, variant)?;
    let _ = reports;

    // 5. After measurement (+ dev gate when configured).
    let after = if opts.dry_run {
        RunSummary::new(Split::Train, Some(candidate.id.clone()), Some(variant), vec![])
    } else {
        let set = eval::load(&opts.eval_path, Split::Train)?;
        let eval_opts = EvalOptions {
            goon_bin: opts.goon_bin.clone(),
            workspace: workspace.to_path_buf(),
            bundle: Some(candidate.id.clone()),
            data_dir: Some(crate::config::data_dir()),
            stub: opts.stub,
            max_tasks: opts.eval_max_tasks.max(1).min(if config.max_tasks > 0 { config.max_tasks } else { usize::MAX }),
            timeout_ms: 120_000,
            variant: Some(variant),
        };
        eval::run(&set, &eval_opts).await?
    };
    let dev = if config.require_dev_gate && !opts.dry_run {
        match eval::load(&opts.eval_path, Split::Dev) {
            Ok(set) if !set.tasks.is_empty() => {
                let base_opts = EvalOptions {
                    goon_bin: opts.goon_bin.clone(),
                    workspace: workspace.to_path_buf(),
                    bundle: Some(base.id.clone()),
                    data_dir: Some(crate::config::data_dir()),
                    stub: opts.stub,
                    max_tasks: set.tasks.len(),
                    timeout_ms: 120_000,
                    variant: Some(variant),
                };
                let dev_before = eval::run(&set, &base_opts).await?;
                let mut candidate_opts = base_opts.clone();
                candidate_opts.bundle = Some(candidate.id.clone());
                let dev_after = eval::run(&set, &candidate_opts).await?;
                Some((dev_before, dev_after))
            }
            _ => None,
        }
    } else {
        None
    };

    // 6. Gate.
    let solved = store.solved().unwrap_or_default();
    let gate = AcceptanceGate { solved: solved.clone(), max_regress: config.max_regress, min_gain_pp: config.min_gain_pp, require_dev: config.require_dev_gate };
    let decision = manifest::decide(&before, &after, dev.as_ref().map(|(b, a)| (b, a)), &gate);
    store.save_run(&before)?;
    store.save_run(&after)?;
    let mut manifest = manifest::build(opts.round, &base, &candidate, &selected, &before, Some(&after), dev.as_ref().map(|(b, a)| (b, a)), &decision);
    if let Some(mut previous) = previous {
        if let Err(error) = manifest::verify_predictions(&mut previous, &before, &after) {
            manifest.entries.iter_mut().for_each(|entry| entry.note = Some(format!("previous verdict failed: {error}")));
        } else {
            store.save_manifest(&previous)?;
        }
    }
    store.save_manifest(&manifest)?;

    // 7. Durable state: keep or revert the workspace bundle.
    let accepted = decision.accepted();
    if accepted {
        bundle::write_files(workspace, &candidate_files)?;
        store.record_solved(after.passed_ids())?;
        let reflection = manifest::reflect(&before, &after, &selected.iter().flat_map(|patch| patch.prediction.expected_fix.clone()).collect::<Vec<_>>());
        let node = DagNode::new(&candidate.id, Some(&base.id), opts.round, Score::from_run(&after), dev.as_ref().map(|(_, after)| Score::from_run(after)), manifest::lessons(&reflection));
        store.append_node(&node)?;
    } else {
        bundle::write_files(workspace, &files)?;
    }

    Ok(RoundOutcome {
        accepted,
        base,
        candidate: Some(candidate),
        manifest: Some(manifest),
        decision,
        before,
        after: Some(after),
        dev_before: dev.as_ref().map(|(b, _)| b.clone()),
        dev_after: dev.map(|(_, a)| a),
        patches: selected,
        evidence: corpus,
    })
}

/// Number of evolution rounds already recorded.
pub fn completed_rounds(store: &HarnessStore) -> usize {
    store.list_manifests().map(|manifests| manifests.len()).unwrap_or(0)
}

/// Resolves the bundle context for a workspace, if a bundle directory exists.
pub fn workspace_context(workspace: &Path) -> Option<HarnessContext> {
    let files = bundle::load_files(workspace).ok()?;
    if files.is_empty() {
        return None;
    }
    Some(bundle::context(&files))
}
