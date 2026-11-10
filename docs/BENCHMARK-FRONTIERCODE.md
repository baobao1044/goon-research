# FrontierCode-style benchmark run

## What FrontierCode is, and why this repo does not report its score

FrontierCode (Cognition, 1.0 → 1.1 → 2.0) is a **closed** benchmark. The published material is the
methodology and the leaderboard:

- tasks authored by open-source maintainers (>40 hours each) and hosted by Cognition,
- grading by an ensemble of unit tests, rubrics and verifiers, with reviewer-defined **blockers**,
- score = weighted aggregate of rubric items, **zero when a blocker fails**,
- runs flagged for consulting solution-bearing sources are **zeroed** (the 1.1 "fair internet use"
  safeguard),
- datasets: 100-task Main / 150-task Extended (1.1), 132 tasks (2.0).

There is no public task set and no public runner, so this repository cannot produce a
FrontierCode score and does not claim one. What it does instead is implement the same *grading
shape* and run it on tasks authored here, which is useful for exactly the property FrontierCode
was built to expose: the gap between "tests pass" and "the maintainer would merge this".

## What was implemented

| Mechanism | Where | Notes |
|---|---|---|
| Blockers + weighted criteria | `src/evolve/eval.rs` (`Rubric`, `Criterion`, `grade`) | failing blocker caps the task at 0 |
| Aggregate score | `RunSummary::mean_score()` | equals pass rate for pass/fail sets |
| Fair-internet-use detector | `unfair_internet_use`, `scan_fair_use` | flags PR diffs/refs, `.patch`/`.diff` fetches, installing the project under test; documentation stays allowed |
| Score reporting | `goon evolve eval` | prints `pass/total (pct)  score X.XXX  tokens N` |

Tasks are plain JSON (`eval/train.json`), each with `prompt`, seeded `files`, and a `rubric`:

```json
{
  "id": "reset-generator-state",
  "prompt": "Bug report: resetHandlers() ... Fix the bug, restart the generator, add a regression test.",
  "files": { "handlers.mjs": "...", "test-bug.mjs": "...", "test-cleanup.mjs": "..." },
  "rubric": {
    "blockers": [{ "id": "blocker-reported-bug", "command": "node test-bug.mjs" }],
    "criteria": [
      { "id": "generator-cleanup", "weight": 3, "command": "node test-cleanup.mjs" },
      { "id": "regression-test-added", "weight": 2, "command": "node -e \"...\"" },
      { "id": "no-scope-creep", "weight": 2, "command": "node -e \"...\"" }
    ]
  }
}
```

## Measured run (2026-11-09)

Harness: `goon` with the bundle in `.goon/harness/`, model `CW/deepseek-ai/DeepSeek-V4.1-Flash-1017`
served by a local OpenAI-compatible gateway, `effort=low`, one trial per task.

| Task | Blocker | Score | Tokens | Weighted detail |
|---|---|---|---|---|
| `range-off-by-one` | pass | **1.000** | 46,791 | all 5 weight points |
| `validate-size` | pass | **1.000** | 37,691 | all 4 weight points |
| `reset-generator-state` | pass | **0.714** | 72,564 | cleanup (3) missed, regression test (2) missed, no-scope-creep (2) kept |
| **Aggregate** | 3/3 pass | **0.905** | 156,046 | |

The third task is the point of the exercise: pass/fail says "solved", the rubric says "the reported
bug is fixed, the cleanup the reporter asked for is not, and no regression test was added". A
pass-rate-only harness cannot see that difference; this one reports it as `0.714`.

Reproduce (needs the gateway and an API key; nothing here is mocked):

```bash
cd goon-cli
goon init --provider openai-compatible --base-url "$GATEWAY/v1" --model "$MODEL" --effort low
goon evolve init
goon evolve eval --split train --tasks eval/train.json --json
```

## Limits, stated plainly

- These are **our** tasks, not FrontierCode's. Scores here are not comparable to the leaderboard.
- One trial per task: task-level model variance is real (the same suite has scored 2/3 and 3/3
  across runs), so single-run numbers are indicative, not definitive. Use `--trials` (or
  `goon evolve attribute`) when the number has to be defensible.
- The fair-internet-use detector is a heuristic over shell commands in the event log. It catches
  the obvious solution-bearing sources; it is not Cognition's verified scanner.
- Rubric criteria are shell checks authored by whoever writes the task, which means a careless
  criterion measures the wrong thing. That is a property of rubric grading, not of this harness.
