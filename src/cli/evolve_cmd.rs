//! `goon evolve` subcommands: init, status, digest, run, log, revert, jit, eval.
//!
//! The subsystem is opt-in: nothing here runs during a normal session unless the user asks for
//! it or passes `--bundle`/`GOON_HARNESS_BUNDLE`.

use crate::cli::args::Parsed;
use crate::cli::stdout_write;
use crate::config::{self, Config};
use crate::evolve::bundle;
use crate::evolve::digest::{self, DigestOptions};
use crate::evolve::engine::{self, EngineOptions, HeuristicProposer, ModelProposer, Proposer};
use crate::evolve::eval::{self, EvalOptions};
use crate::evolve::jit::{JitPlanner, MAX_JIT_REPAIRS};
use crate::evolve::store::HarnessStore;
use crate::evolve::types::*;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

fn workspace(values: &Parsed) -> PathBuf {
    let raw = values.str("cwd").unwrap_or_else(|| ".".to_string());
    std::path::absolute(raw).unwrap_or_else(|_| PathBuf::from("."))
}

fn store(_config: &Config) -> HarnessStore {
    HarnessStore::new(crate::evolve::state_root(&config::data_dir()))
}

fn variant_arg(values: &Parsed) -> Variant {
    values.str("variant").as_deref().and_then(Variant::parse).unwrap_or(Variant::Code)
}

fn eval_path(values: &Parsed, config: &Config) -> PathBuf {
    if let Some(raw) = values.str("tasks") {
        return PathBuf::from(raw);
    }
    if let Some(path) = &config.evolve.eval {
        return path.clone();
    }
    workspace(values).join("eval").join("tasks.json")
}

fn print_json(value: &serde_json::Value) {
    stdout_write(&format!("{}\n", crate::jsutil::stringify_pretty(value)));
}

/// Entry point for `goon evolve <sub>`.
pub async fn run(positionals: &[String], values: &Parsed, config: &Config) -> Result<()> {
    let sub = positionals.get(1).map(|s| s.to_lowercase()).unwrap_or_else(|| "status".into());
    match sub.as_str() {
        "init" | "seed" => init(values, config),
        "status" | "st" => status(values, config),
        "digest" | "evidence" => digest_cmd(values, config),
        "run" | "round" => run_round_cmd(values, config).await,
        "log" | "history" => log_cmd(values, config),
        "revert" | "restore" => revert(values, config),
        "jit" | "plan" => jit_cmd(values, config).await,
        "eval" => eval_cmd(values, config).await,
        "help" | "--help" => {
            stdout_write(EVOLVE_HELP);
            Ok(())
        }
        other => bail!("Unknown evolve subcommand '{other}'.\n\n{EVOLVE_HELP}"),
    }
}

const EVOLVE_HELP: &str = "\
goon evolve — observability-driven harness evolution (off by default)

  goon evolve init [--variant code|research|ops]     seed .goon/harness + eval/tasks.json
  goon evolve status [--json]                        components, rounds, last verdict
  goon evolve digest [--json]                        trace -> layered evidence corpus
  goon evolve run [--round N] [--dry-run] [--rule TEXT] [--model-proposer]
  goon evolve log [--json]                           manifests and predictions
  goon evolve revert <round|bundle-id>               restore a bundle into .goon/harness
  goon evolve jit -p '<task>' [--run]                plan a harness for one task
  goon evolve eval [--split train|dev|test]          measure the active bundle

Bundle stays editable and revertible: one component = one file under .goon/harness/.
";

fn init(values: &Parsed, config: &Config) -> Result<()> {
    let ws = workspace(values);
    let variant = variant_arg(values);
    let store = store(config);
    store.ensure()?;
    let files = if bundle::load_files(&ws)?.is_empty() { bundle::seed_files(variant) } else { bundle::load_files(&ws)? };
    bundle::write_files(&ws, &files)?;
    let snapshot = bundle::snapshot(&store, &files, None, 0, variant)?;
    let tasks = eval_path(values, config);
    if !tasks.exists() {
        if let Some(parent) = tasks.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        eval::write_template(&tasks)?;
    }
    if values.flag("json") {
        print_json(&json!({
            "bundle": snapshot.id,
            "components": snapshot.components,
            "dir": bundle_dir(&ws).to_string_lossy(),
            "eval": tasks.to_string_lossy(),
            "store": store.root().to_string_lossy(),
        }));
    } else {
        stdout_write(&format!(
            "Seeded {} components in {}\nBundle {}\nEval tasks {}\nState {}\n",
            snapshot.components.len(),
            bundle_dir(&ws).display(),
            snapshot.id,
            tasks.display(),
            store.root().display()
        ));
    }
    Ok(())
}

fn bundle_dir(ws: &Path) -> PathBuf {
    crate::evolve::bundle_dir(ws)
}

fn status(values: &Parsed, config: &Config) -> Result<()> {
    let ws = workspace(values);
    let store = store(config);
    let files = bundle::load_files(&ws)?;
    let manifests = store.list_manifests().unwrap_or_default();
    let latest = store.latest_bundle().ok().flatten();
    let solved = store.solved().unwrap_or_default();
    let nodes = store.nodes().unwrap_or_default();
    if values.flag("json") {
        print_json(&json!({
            "workspace": ws.to_string_lossy(),
            "bundle_dir": bundle_dir(&ws).to_string_lossy(),
            "components": bundle::diff(&std::collections::BTreeMap::new(), &files),
            "total_components": files.len(),
            "latest_bundle": latest.as_ref().map(|b| b.id.clone()),
            "rounds": manifests.len(),
            "solved": solved.len(),
            "dag_nodes": nodes.len(),
            "last": manifests.last().map(|m| json!({
                "round": m.round,
                "accepted": m.accepted,
                "before": m.before,
                "after": m.after,
                "reject_reason": m.reject_reason,
                "verdicts": m.verdicts(),
            })),
        }));
        return Ok(());
    }
    stdout_write(&format!("workspace      {}\nbundle dir     {}\ncomponents     {}\n", ws.display(), bundle_dir(&ws).display(), files.len()));
    for entry in bundle::diff(&std::collections::BTreeMap::new(), &files) {
        stdout_write(&format!("  {:<24} {}\n", entry.kind.as_str(), entry.path));
    }
    stdout_write(&format!(
        "latest bundle  {}\nrounds         {}\nsolved tasks   {}\ndag nodes      {}\n",
        latest.map(|b| b.id).unwrap_or_else(|| "-".into()),
        manifests.len(),
        solved.len(),
        nodes.len()
    ));
    if let Some(last) = manifests.last() {
        let status = if last.accepted { "accepted".to_string() } else { last.reject_reason.clone().unwrap_or_else(|| "rejected".into()) };
        stdout_write(&format!("last round     {} ({})\n  before {} / after {}\n", last.round, status, score_text(&last.before), last.after.as_ref().map(score_text).unwrap_or_else(|| "-".into())));
    }
    Ok(())
}

fn score_text(score: &Score) -> String {
    format!("{}/{} ({:.1}%)", score.passed, score.total, score.pass_rate() * 100.0)
}

fn digest_cmd(values: &Parsed, config: &Config) -> Result<()> {
    let store = store(config);
    store.ensure()?;
    let sessions = digest::sessions_dir_for(&config::data_dir());
    let opts = DigestOptions { max_sessions: 80, max_tasks: 40, max_tokens: 6000 };
    let (corpus, digests) = digest::digest_sessions(&sessions, &opts)?;
    let dir = store.digest_dir();
    digest::write_digest(&dir, &corpus, &digests)?;
    store.save_evidence("latest", &corpus)?;
    if values.flag("json") {
        print_json(&json!({
            "sessions": corpus.sessions_scanned,
            "skipped": corpus.sessions_skipped,
            "items": corpus.items,
            "tokens": corpus.tokens_estimate,
            "truncated": corpus.truncated,
            "dir": dir.to_string_lossy(),
        }));
    } else {
        stdout_write(&format!(
            "sessions {}  clusters {}  failures {}  tokens ~{}\nwritten to {}\n",
            corpus.sessions_scanned,
            corpus.items.len(),
            corpus.total_occurrences(),
            corpus.tokens_estimate,
            dir.display()
        ));
        for item in corpus.top(8) {
            stdout_write(&format!("  {:>3}x {:<20} {}\n", item.occurrences, item.signal.as_str(), truncate(&item.signature, 90)));
        }
    }
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

async fn run_round_cmd(values: &Parsed, config: &Config) -> Result<()> {
    let ws = workspace(values);
    let store = store(config);
    store.ensure()?;
    let round = values.str("round").and_then(|raw| raw.parse::<u32>().ok()).unwrap_or_else(|| (engine::completed_rounds(&store) as u32) + 1);
    let max_rounds = config.evolve.rounds.max(round);
    let cancel = tokio_util::sync::CancellationToken::new();
    let opts = EngineOptions {
        round,
        max_rounds,
        dry_run: values.flag("dry-run"),
        eval_path: eval_path(values, config),
        dev_path: values.str("dev-tasks").map(PathBuf::from),
        eval_max_tasks: values.str("max-tasks").and_then(|raw| raw.parse().ok()).unwrap_or(config.evolve.max_tasks.max(1)),
        goon_bin: values.str("goon-bin").map(PathBuf::from).or_else(|| std::env::current_exe().ok()).unwrap_or_else(|| PathBuf::from("goon")),
        stub: values.flag("stub") || config.stub,
        verify_commands: values.list("verify-cmd"),
        rule: values.str("rule"),
    };
    let sessions = digest::sessions_dir_for(&config::data_dir());
    let proposer: Box<dyn Proposer> = if values.flag("model-proposer") {
        let model = crate::provider::create_model(&config::resolve_runtime(config, &workspace(values))?, None)?;
        Box::new(ModelProposer { model, system: "You are a harness engineer. Emit only JSON patches that follow the documented edit language.".into(), max_output_tokens: 1800 })
    } else {
        Box::new(HeuristicProposer::new(3))
    };
    let outcome = engine::run_round(&store, &ws, &sessions, &config.evolve, &opts, proposer.as_ref(), &cancel).await.context("evolution round failed")?;

    if values.flag("json") {
        print_json(&json!({
            "round": round,
            "accepted": outcome.accepted,
            "decision": outcome.decision,
            "base": outcome.base.id,
            "candidate": outcome.candidate.as_ref().map(|b| b.id.clone()),
            "before": outcome.before,
            "after": outcome.after,
            "dev_before": outcome.dev_before,
            "dev_after": outcome.dev_after,
            "patches": outcome.patches,
            "manifest": outcome.manifest,
            "evidence": {"items": outcome.evidence.items.len(), "tokens": outcome.evidence.tokens_estimate},
        }));
        return Ok(());
    }
    stdout_write(&format!(
        "round {}  base {}\n  patches        {}\n  train before   {:.1}%  after {}\n",
        round,
        outcome.base.id,
        outcome.patches.len(),
        outcome.before.pass_rate() * 100.0,
        outcome.after.as_ref().map(|run| format!("{:.1}%", run.pass_rate() * 100.0)).unwrap_or_else(|| "not measured".into())
    ));
    if let (Some(before), Some(after)) = (&outcome.dev_before, &outcome.dev_after) {
        stdout_write(&format!("  dev   before   {:.1}%  after {:.1}%\n", before.pass_rate() * 100.0, after.pass_rate() * 100.0));
    }
    for patch in &outcome.patches {
        stdout_write(&format!("  + {:<18} {}\n", patch.kind.as_str(), patch.rationale));
    }
    match &outcome.decision {
        GateDecision::Accept => stdout_write("  decision       ACCEPTED (durable bundle updated)\n"),
        GateDecision::Reject { reason } => stdout_write(&format!("  decision       REJECTED: {reason}\n  workspace bundle restored to base\n")),
    }
    Ok(())
}

fn log_cmd(values: &Parsed, config: &Config) -> Result<()> {
    let store = store(config);
    let manifests = store.list_manifests()?;
    if values.flag("json") {
        print_json(&json!(manifests));
        return Ok(());
    }
    if manifests.is_empty() {
        stdout_write("No evolution rounds yet. Run `goon evolve run`.\n");
        return Ok(());
    }
    for manifest in manifests {
        stdout_write(&format!(
            "round {:<3} {}  {}  train {} -> {}\n",
            manifest.round,
            if manifest.accepted { "accepted" } else { "rejected" },
            manifest.candidate,
            score_text(&manifest.before),
            manifest.after.as_ref().map(score_text).unwrap_or_else(|| "-".into())
        ));
        for entry in &manifest.entries {
            stdout_write(&format!(
                "   {:<24} {:<18} {:<10}{}\n",
                entry.component,
                entry.patch_kind.as_str(),
                entry.verdict.as_str(),
                if entry.prediction.expected_fix.is_empty() { String::new() } else { format!("predicts {}", entry.prediction.expected_fix.join(",")) }
            ));
        }
    }
    Ok(())
}

fn revert(values: &Parsed, config: &Config) -> Result<()> {
    let target = values.list("bundle").first().cloned().or_else(|| values.positionals.get(2).cloned());
    let Some(target) = target else {
        bail!("Usage: goon evolve revert <round|bundle-id>");
    };
    let ws = workspace(values);
    let store = store(config);
    // A round number restores the bundle that round started from.
    let bundle_id = if let Ok(round) = target.parse::<u32>() {
        let manifests = store.list_manifests()?;
        manifests.iter().find(|manifest| manifest.round == round).map(|manifest| manifest.base.clone()).unwrap_or(target.clone())
    } else {
        target.clone()
    };
    bundle::apply_to_workspace(&store, &bundle_id, &ws)?;
    stdout_write(&format!("Restored bundle {bundle_id} into {}\n", bundle_dir(&ws).display()));
    Ok(())
}

async fn jit_cmd(values: &Parsed, config: &Config) -> Result<()> {
    let task = values.str("prompt").or_else(|| values.positionals.get(2).cloned()).ok_or_else(|| anyhow::anyhow!("Usage: goon evolve jit -p '<task>' [--run]"))?;
    let ws = workspace(values);
    let store = store(config);
    let planner = JitPlanner::new(store);
    let files = bundle::load_files(&ws).ok().filter(|files| !files.is_empty());
    let decision = planner.plan(&task, files.as_ref())?;
    if values.flag("json") {
        print_json(&json!({ "decision": decision, "max_repairs": MAX_JIT_REPAIRS }));
        return Ok(());
    }
    stdout_write(&format!(
        "variant        {}\nbundle         {}\nprofile        {}\nconfidence     {:.2}\nrationale      {}\n",
        decision.variant.as_str(),
        decision.bundle.clone().unwrap_or_else(|| "-".into()),
        decision.profile.name,
        decision.confidence,
        decision.rationale
    ));
    if values.flag("run") {
        let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("goon"));
        let status = tokio::process::Command::new(bin).arg("-p").arg(&task).arg("--yes").current_dir(&ws).status().await.context("spawn goon run")?;
        let code = status.code().unwrap_or(1);
        stdout_write(&format!("run exit {code}\n"));
        return Ok(());
    }
    Ok(())
}

async fn eval_cmd(values: &Parsed, config: &Config) -> Result<()> {
    let ws = workspace(values);
    let store = store(config);
    let split = values.str("split").as_deref().and_then(Split::parse).unwrap_or(Split::Train);
    let path = eval_path(values, config);
    let set = eval::load(&path, split).with_context(|| format!("load {}", path.display()))?;
    let bundle_id = values.list("bundle").first().cloned().or_else(|| store.latest_bundle().ok().flatten().map(|bundle| bundle.id));
    let opts = EvalOptions {
        goon_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("goon")),
        workspace: ws,
        bundle: bundle_id.clone(),
        data_dir: Some(config::data_base()),
        stub: values.flag("stub") || config.stub,
        max_tasks: values.str("max-tasks").and_then(|raw| raw.parse().ok()).unwrap_or(config.evolve.max_tasks.max(1)),
        timeout_ms: 120_000,
        variant: Some(variant_arg(values)),
    };
    let run = eval::run(&set, &opts).await?;
    if values.flag("json") {
        print_json(&json!(run));
    } else {
        stdout_write(&format!(
            "split {}  bundle {}  {}/{} ({:.1}%)  tokens {}\n",
            split.as_str(),
            bundle_id.unwrap_or_else(|| "-".into()),
            run.passed(),
            run.total(),
            run.pass_rate() * 100.0,
            run.tokens()
        ));
        for outcome in &run.outcomes {
            stdout_write(&format!(
                "  {} {:<40} {}\n",
                if outcome.passed { "PASS" } else { "FAIL" },
                truncate(&outcome.task_id, 40),
                outcome.failure.as_ref().map(|signal| signal.as_str().to_string()).unwrap_or_default()
            ));
        }
    }
    Ok(())
}
