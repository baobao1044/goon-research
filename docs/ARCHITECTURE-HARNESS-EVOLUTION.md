# Harness Evolution Architecture (6 papers)

Subsystem: `src/evolve/` (lib `goon`), CLI surface `goon evolve ...`, on-disk state
`{data_dir}/harness/`, editable bundle `.goon/harness/` in the workspace.

Everything here is **off by default**. Nothing in this subsystem runs during a normal session
unless the user invokes `goon evolve ...` or passes `--bundle` / `--jit`.

## 1. Papers and what was actually taken

| # | Paper | Mechanism taken | Where it lives |
|---|---|---|---|
| 1 | [arXiv:2604.25850](https://arxiv.org/abs/2604.25850) AHE — observability-driven evolution | Three observability pillars; edit = falsifiable contract with a prediction verified next round; file-level revertible components | `bundle.rs` (pillar 1), `digest.rs` (pillar 2), `manifest.rs` (pillar 3) |
| 2 | [arXiv:2606.14249](https://arxiv.org/abs/2606.14249) HarnessX — composable/evolvable foundry | Harness as replaceable slots; evolution engine driven by execution traces; "seesaw" gate (no regressions on solved tasks); variant isolation | `profile.rs` (slots), `engine.rs` (evolution loop), `manifest.rs` (seesaw) |
| 3 | [arXiv:2608.25593](https://arxiv.org/abs/2608.25593) JIT-Agent — just-in-time harness | Task-adaptive harness selection, repair on unstable execution, bounded retries (`MAX_JIT_REPAIRS = 2`) | `jit.rs` |
| 4 | [arXiv:2609.00006](https://arxiv.org/abs/2609.00006) Anatomy of coding agents | Component taxonomy, 29 patterns, skills as filesystem convention, no general agent framework as the runtime | `bundle.rs` component kinds, `docs/` §4, existing repo modules |
| 5 | [arXiv:2605.13357](https://arxiv.org/abs/2605.13357) Runtime substrate | 11 runtime responsibilities; requirement-level verification; attribution before recovery; traceable mediation | `docs/` §3, `verify.rs` + `engine.rs::validate_candidate` |
| 6 | [arXiv:2608.23041](https://arxiv.org/abs/2608.23041) AutoSaddler — durable updates | Diagnosis → **targeted** patch → verify → durable accept; 9 patch subtypes; capability-before-steering phasing; dev split; EvoDAG | `patch.rs`, `store.rs` (`evodag.jsonl`), `engine.rs` |

Deliberate non-goals (documented because the papers' headline numbers depend on them):
training/GRPO bridges, model-weight co-evolution, free-form harness rewriting, and
adaptive OS sandboxing. A single static binary on Termux cannot host those, and each one is a
new failure mode rather than a feature.

## 2. The loop (one round)

```
.goon/harness/**  ──snapshot──▶  bundle (content-addressed, parent link)
        │                              │
        │                    eval train split before ──▶ RunSummary
        │                              │
   sessions/*.events.jsonl ──digest──▶ EvidenceCorpus (L1/L2/L3)  + TaskDigest[]
                                       │
                              Proposer (heuristic | model)
                                       │  TargetedPatch[] (bounded edit language)
                                       ▼
                        apply to a staged copy + EditScope check
                                       │
                        validate (component schema, JSON, verify cmds, build)
                                       │
                        snapshot candidate ──▶ eval train after ──▶ eval dev after
                                       │
                    AcceptanceGate (unmeasured? regression? solved set? dev?)
                                       │
                    accept ──▶ write bundle to workspace, record solved, EvoDAG node
                    reject ──▶ restore base bundle, store reject reason
                                       │
                        ChangeManifest (predictions stored, duplicate to `log`)
                                       │
                    next round: verify_predictions → Confirmed/Partial/Refuted/Regressed
```

## 3. Runtime responsibilities (paper 5) mapped to code

| # | Responsibility | Owner in this repo |
|---|---|---|
| 1 | Task interface | `cli/`, `harness::run(prompt)` |
| 2 | Context manager | `context.rs`, `context_budget.rs`, bundle `policy/context.json` |
| 3 | Tool registry | `tools.rs`, `risk.rs`, `policy.rs` |
| 4 | Project memory | `memory.rs`, `rag.rs`, bundle `memory.md` |
| 5 | Task state | `session.rs`, `coord.rs` (todos/goals) |
| 6 | Observability | `eventlog.rs`, `emit` events, `digest.rs` |
| 7 | Failure attribution | `digest.rs` clusters + `FailureSignal` classification |
| 8 | Verification protocol | `verify.rs`, bundle `policy/verify.json`, `engine::validate_candidate` |
| 9 | Permission boundary | `policy.rs`, `approval.rs`, `EditScope` |
| 10 | Entropy auditor | `manifest::reflect` + lessons (bounded, advisory) |
| 11 | Intervention logger | approval events in `eventlog.rs`, surfaced by `digest.rs` |

## 4. Component slots (paper 4 + 1)

`.goon/harness/` is the whole editable harness. One component = one file, so every edit is
revertible and diffable without a VCS dependency.

| Path | Kind | Consumed as |
|---|---|---|
| `system.md` | system prompt | appended inside `<harness:policy>` |
| `tools/guidance.md` | tool guidance | `<harness:tool-guidance>` |
| `middleware.md` | turn rules | `<harness:rules>` bullets |
| `skills/*.md` | skills (SKILL.md convention) | `<harness:skills>` sections |
| `subagents/*.md` | role prompts | subagent role text |
| `memory.md` | durable lessons | `<harness:memory>` bullets |
| `policy/context.json` | context ratios | `Config` overrides (clamped) |
| `policy/verify.json` | verify commands | `verify.commands` |
| `policy/jit.json` | JIT selection | `evolve.jit` |

## 5. Edit language (targeted modification, paper 6)

The evolution agent can only emit these operations; there is no "write this file" primitive:

`append_section`, `replace_block` (marker-bounded), `insert_bullet` (heading-scoped),
`set_param` (JSON policy), `add_skill`, `remove_skill`.

Hard limits enforced in `patch.rs`: `MAX_OPS = 8`, `MAX_GROWTH_BYTES = 8192`,
`MAX_REPLACED_RATIO = 0.5`, and every path must pass `EditScope`:

- allow: `.goon/harness/**`
- deny: `src/verify.rs`, `src/policy.rs`, `src/config.rs`, `eval/`, `tests/`, `sessions/`

`--yolo`, auto-approval and subagents cannot widen this. The verifier is the reward signal, so
it stays read-only to the thing being optimized.

## 6. Where the measurements come from (and their limits)

- Train split drives diagnosis and the round decision; dev split gates durability; test split is
  read at most once (AutoSaddler discipline).
- Eval runs the **real CLI** as a subprocess (`eval.rs`) with `GOON_HARNESS_BUNDLE` and
  `GOON_TASK_ID`, so the thing measured is the same binary the user runs.
- `--stub` keeps the loop fully offline and deterministic for tests.
- An empty or dry round is **rejected**, never accepted: no measurement, no durable change.

Honest limits compared with the papers: eval sets here are small (default
`max_tasks = 10`), rounds are manual, and there is no RL/training bridge. Expect a handful of
tasks and one round per invocation, not 89 tasks × 10 rounds versus Terminal-Bench.

## 7. Commands

```bash
goon evolve init                     # seed .goon/harness + eval/tasks.json
goon evolve status                   # components, bundle, rounds, last verdict
goon evolve digest                   # traces -> layered evidence corpus
goon evolve run --dry-run            # plan a round without spending eval budget
goon evolve run                      # full round: digest -> patch -> verify -> gate
goon evolve run --model-proposer     # same, but the model proposes the patches
goon evolve log                      # manifests: prediction vs verdict
goon evolve revert 1                 # restore the bundle round 1 started from
goon evolve jit -p 'fix the test'    # task-adaptive harness plan (+ --run)
goon evolve eval --split dev         # measure the current bundle

goon --bundle <id|path> -p '...'     # apply a bundle to a normal session
goon --jit -p '...'                  # enable JIT selection for this run
```

State layout (`{data_dir}/harness/`): `snapshots/<id>/`, `bundles.jsonl`, `manifests/<round>.json`,
`runs/<id>.json`, `digest/`, `evodag.jsonl`, `solved.json`, `jit.jsonl`.

## 8. Failure modes handled explicitly

| Failure | Handling |
|---|---|
| Model proposes a full-file rewrite | `MAX_REPLACED_RATIO` + op allowlist reject it |
| Model proposes edits to `verify.rs` / `policy.rs` | `EditScope` deny list |
| Candidate does not parse/build | `validate_candidate` fails the round before evaluation |
| Candidate improves train but breaks solved tasks | seesaw gate rejects, workspace restored |
| Candidate only holds on train | dev gate rejects (when enabled) |
| Round has no tasks (dry run, empty set) | rejected as unverifiable |
| Trace log corrupt/torn | skipped and counted; torn tail still read |
| Prediction was wrong | next round records Refuted/Regressed on the manifest |
| User distrusts a bundle | `goon evolve revert <round>` restores the base snapshot |

## 9. Test coverage

`cargo test --lib evolve` — 69 tests over `bundle`, `store`, `digest`, `patch`, `manifest`,
`profile`, `jit`, `eval`, `types`, plus CLI-level coverage in `tests/evolve.test.mjs`.
