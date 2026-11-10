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
| `policy/verify.json` | verify commands + `maxRepairRounds` | `verify.commands`, `verify.max_repair_rounds` |
| `policy/loop.json` | `maxSteps`, `verifyRetries` | outer-loop bound |
| `policy/jit.json` | JIT + attribution + trace switches | pipeline construction |
| `tools/registry.json` | tool declarations (name, command template, schema, args) | real tools registered at harness start |

`tools/registry.json` is what makes `new_tool` a genuine capability edit rather than a label: an
entry becomes a callable tool whose command runs in the workspace. Substituted arguments are
restricted to `[A-Za-z0-9._\-/=:,@+]` and refused otherwise, so a tool declaration can never turn
into shell injection; approval, read-only and network policy still apply.

## 5. Middleware pipeline (paper 2)

Eight lifecycle hooks, ordered processors, five outcomes. The pipeline is only built when
evolution is enabled (`--evolve`, `--bundle`, or `evolve.enabled`), so a default session keeps its
exact previous behaviour (`Pipeline::disabled()` short-circuits every dispatch).

| Hook | Where it fires |
|---|---|
| `TaskStart` | first thing in a run, after the session records `task_started` |
| `StepStart` | top of each outer loop iteration (also enforces `loop.json maxSteps`) |
| `BeforeModel` | after context budgeting, immediately before the request |
| `AfterModel` | once a streamed step completed (carries tool-call and token counts) |
| `BeforeTool` | per tool call, before execution (attribution gate lives here) |
| `AfterTool` | per tool call, after its result is recorded |
| `StepEnd` | after verification, before the next iteration (JIT repair lives here) |
| `TaskEnd` | before the run returns (answered or stopped) |

Processors declare `Order` (PRE/NORMAL/POST) and an optional singleton `group`; registering two
processors in one group on one hook is rejected, and `replace()` swaps a group without touching
its neighbours. Outcomes: `Pass`, `Transform(events)`, `Intercept(events)`, and
`Interrupt { reason, events }` — the last one emits its events and forbids the action.

Built-ins:

- **AttributionGate** (`BeforeTool`, group `safety`) — after a failed verification, a mutating
  tool is refused until the assistant text contains
  `<attribution>observed…expected…diagnosis…</attribution>`. This is the runtime-substrate rule
  "attribute before recovering"; without it the model re-edits blindly.
- **JitRepairDirective** (`StepEnd`, group `jit`) — turns an unstable signal
  (`verify_fail`, `tool_error`, `context_overflow`, `parse_error`) into a repair event.
- **HookTracer** (all eight hooks, POST) — emits `harness_hook` events when `policy/jit.json`
  sets `"trace": true`.

## 6. JIT selection and repair (paper 3)

`--jit` / `evolve.jit` builds a `JitPlanner`. On a round it plans a profile from the bundle plus
the `jit.jsonl` archive (pass rate, then fewer tokens), and after an unstable step it repairs the
profile at most `MAX_JIT_REPAIRS = 2` times: overflowing context shrinks the conversation share,
a verify failure stages verify commands, a tool error adds recovery guidance. Repairs adjust the
profile and emit `jit_repaired`; they deliberately do **not** inject chat messages, because that
would displace the verification instruction the next step must act on.

## 7. Edit language (targeted modification, paper 6)

The evolution agent can only emit these operations; there is no "write this file" primitive:

`append_section`, `replace_block` (marker-bounded), `insert_bullet` (heading-scoped),
`set_param` (JSON policy), `add_skill`, `remove_skill`, `add_tool` (registers a real callable
tool in `tools/registry.json`).

A patch may omit `kind` or name one outside the vocabulary: the kind is then inferred from the
ops (an `append_section` on `middleware.md` is a prompt rule, `add_skill` is a skill edit,
`add_tool` is a new capability), so a slightly wrong label does not throw away a usable edit.

Hard limits enforced in `patch.rs`: `MAX_OPS = 8`, `MAX_GROWTH_BYTES = 8192`,
`MAX_REPLACED_RATIO = 0.5`, and every path must pass `EditScope`:

- allow: `.goon/harness/**`
- deny: `src/verify.rs`, `src/policy.rs`, `src/config.rs`, `eval/`, `tests/`, `sessions/`

`--yolo`, auto-approval and subagents cannot widen this. The verifier is the reward signal, so
it stays read-only to the thing being optimized.

## 8. Measuring a round (and what the numbers cost)

Evaluation runs the real CLI per task, up to `concurrency` tasks at once (default 3, clamp 1..8),
in a throwaway directory. Ordering of outcomes follows the task list, so two runs of the same set
are comparable.

- A task's own `timeout_ms` is authoritative; `EvalOptions::timeout_ms` is only a fallback for
  tasks that declare none. (Clamping to the global default silently starved longer tasks.)
- Infrastructure failures (`timeout`, `parse_error`, `context_overflow`) are retried once when
  `retries > 0`; a wrong answer or a failed verify is never retried.
- Each outcome carries the child's session id and its real token total, read from the child's own
  event log — so a round reports what it cost, not zero.

## 9. Causal attribution (which patch caused what)

A round applies several patches at once, so a single before/after measurement cannot say which
one did anything. `goon evolve attribute` runs the controlled experiment instead, without ever
touching the workspace bundle (every arm is measured against an in-memory copy):

| Arm | What it isolates |
|---|---|
| `baseline` | the control: same bundle, no patches |
| `patch-N` | one patch alone (individual ablation) |
| `combined` | all patches together (only when there is more than one) |

Each arm runs `--trials` times (default 2; one trial cannot separate an effect from model noise,
so every effect is reported `inconclusive`). The report records the task-set and dev-set hashes
plus the model id, because two experiments are only comparable when those match.

Three metrics the round gate does not express:

- **Success per 1K tokens** — `E = successful / tokens * 1000` over every arm of the experiment.
- **Patch utility** — `U = Δsuccess / Δcost`, where `Δcost` is tokens per task. When the patch
  *saves* cost (`Δcost < 0`) or changes nothing (`Δcost == 0`), `U` is reported as `n/a` rather
  than a negative or infinite ratio; with no success change `U = 0`.
- **Generalization gap** — `G = P_train - P_dev` for the measured arm; positive means the effect
  only holds on the split that drove the edit.

Per-patch verdicts: `helpful`, `helpful_but_costly`, `harmful`, `neutral_but_cheaper` (same
outcomes for fewer tokens), `neutral`, `inconclusive`. A measured example from a real run:
the "manifest preflight" patch left success at 100% and cut 3,652 tokens/task, so it reports
`neutral_but_cheaper` with `U = n/a` — which is exactly the case a pass-rate-only gate hides.

## 10. Mergeability grading (FrontierCode-style)

Cognition's FrontierCode is a **closed** benchmark: the tasks and the grader live on their side,
and the published material is methodology plus leaderboard, not a runnable suite. What this repo
can do is implement the same *grading shape* and run it on tasks we author ourselves — clearly
labelled as ours, never as FrontierCode scores.

Two pieces, both in `eval.rs`:

- **Rubric**: a task may carry `blockers` and weighted `criteria`, each a shell check. A failing
  blocker caps the task at 0 (a change-requested review); otherwise the score is the weighted
  fraction of passing criteria. Tasks without a rubric keep the ordinary pass/fail verdict, so the
  other eval modes are unchanged. `RunSummary::mean_score()` is the aggregate; for pass/fail sets
  it equals the pass rate.
- **Fair internet use**: after a run, the child's own event log is scanned for shell commands that
  touch solution-bearing sources (a `/pull/` or `/commit/` diff, a `.patch`/`.diff` fetch, or
  installing the project under test). A flagged run scores zero and reports
  `unfair_internet_use`; documentation lookups and search are deliberately allowed.

Measured example (3 authored tasks, real model): pass rate 3/3 but mean score **0.905**, because
the third task passed its blocker while skipping two weighted criteria (generator cleanup,
regression test) and landed at 0.714. That gap between "the tests pass" and "the maintainer would
merge this" is the whole point of the benchmark, and a pass-rate-only harness cannot see it.

## 11. Where the measurements come from (and their limits)

- Train split drives diagnosis and the round decision; dev split gates durability; test split is
  read at most once (AutoSaddler discipline).
- Eval runs the **real CLI** as a subprocess (`eval.rs`) with `GOON_HARNESS_BUNDLE` and
  `GOON_TASK_ID`, so the thing measured is the same binary the user runs.
- `--stub` keeps the loop fully offline and deterministic for tests.
- An empty or dry round is **rejected**, never accepted: no measurement, no durable change.

Honest limits compared with the papers: eval sets here are small (default
`max_tasks = 10`), rounds are manual, and there is no RL/training bridge. Expect a handful of
tasks and one round per invocation, not 89 tasks × 10 rounds versus Terminal-Bench.

## 12. Commands

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
goon evolve attribute --round N      # ablation: control, each patch alone, combined
goon evolve attribute --trials 3 --model-proposer   # same, model-proposed patches

goon --bundle <id|path> -p '...'     # apply a bundle to a normal session
goon --jit -p '...'                  # enable JIT selection for this run
```

State layout (`{data_dir}/harness/`): `snapshots/<id>/`, `bundles.jsonl`, `manifests/<round>.json`,
`runs/<id>.json`, `digest/`, `evodag.jsonl`, `solved.json`, `jit.jsonl`.

## 13. Failure modes handled explicitly

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
| Model re-edits blindly after a failed verify | attribution gate refuses the mutation |
| Run loops forever making tool calls | `policy/loop.json maxSteps` stops it |
| Bundle declares a tool with hostile arguments | argument charset allowlist refuses it |

## 14. The 29 design patterns (paper 4) and where this repo stands

`✅` implemented · `🟡` partial · `❌` not implemented · `—` out of scope

| # | Pattern | This repo |
|---|---|---|
| 1 | Event Sourcing | ✅ `eventlog.rs`, pipeline hook events |
| 2 | Policy-as-Code | 🟡 `policy.rs` + bundle `policy/*.json` |
| 3 | Recursive Composition | ✅ `subagents.rs` |
| 4 | Polymorphic Edits | 🟡 `tools.rs` input aliases |
| 5 | Deferred Loading | ❌ no `tool_search` |
| 6 | Template Method | ❌ |
| 7 | Protocol Interfaces | ✅ `LanguageModel`, `ToolExec`, `Approve`, `MemoryHooks` |
| 8 | LLM Summarization | ✅ `context.rs` compaction |
| 9 | Stuck Detection | 🟡 repeat counter + loop warning |
| 10 | Reflection Loop | 🟡 `coord.rs` reflect/rerank |
| 11 | Prompt Caching | ❌ no cache boundary control |
| 12 | Context Forking | 🟡 subagent context isolation |
| 13 | Middleware Pipeline | ✅ `evolve/pipeline.rs` eight hooks |
| 14 | JIT Repo Context | ✅ AGENTS.md chain, `@file` |
| 15 | Skills | ✅ `skills/*.md` + `$skill` |
| 16 | Conditional Activation | 🟡 skill scopes; no `paths:` trigger |
| 17 | Turn-Level Checkpoint | ✅ `session.rs` + file backups |
| 18 | Agent-Maintained Memory | 🟡 `memory.rs` lessons |
| 19 | Outer Verification Loop | ✅ `verify.rs` + `ErrNotVerified` |
| 20 | Self-Improving Skill Loop | 🟡 bundle patches can add skills; no curation |
| 21 | Lineage Compaction | ❌ |
| 22 | Session-Tree Version Control | ❌ |
| 23 | Minimal-Core / Extension-Host | 🟡 pipeline + bundle are the extension seam |
| 24 | Client/Server Harness | ❌ |
| 25 | Model-Family Prompt Matrix | 🟡 `provider.rs` capability table |
| 26 | Cache-Dialect Fanout | ❌ |
| 27 | Syntax-Aware Command Permissioning | 🟡 `risk.rs` shell scanning |
| 28 | Untrusted-Content Delimiting | ✅ `<untrusted_context>`, `redact.rs` |
| 29 | Harness Mimicry | — not adopted (ToS) |

Count: ✅ 10 · 🟡 12 · ❌ 6 · — 1. The gaps are deliberate: this is a single static binary aimed
at Termux, and every pattern skipped here (session trees, client/server split, cache fanout,
neural code RAG) is a large subsystem whose cost would land on every user.

## 15. Test coverage

`cargo test --lib evolve` — 87 tests over `bundle`, `store`, `digest`, `patch`, `manifest`,
`profile`, `jit`, `eval`, `pipeline`, `attribution`, `types`, plus CLI-level coverage in `tests/evolve.test.mjs`
(11 tests: seeding, status, digest, unmeasured-round rejection, JIT planning, revert, bundle
application with a `harness` event, snapshot identity, a bundle-declared tool that really runs,
`maxSteps` enforcement, and the attribution gate).
