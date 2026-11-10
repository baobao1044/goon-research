# Terminal-Bench 4.1: status and how to run it

Target: <https://www.tbench.ai/> — Terminal-Bench 4.1, run through Harbor
(<https://github.com/harbor-framework/terminal-bench>).

## Verified working (2026-11-10)

The benchmark now **runs on this machine**, inside WSL2:

| Component | Version / state |
|---|---|
| WSL | enabled, `Ubuntu-26.04`, systemd running, root shell |
| Docker | 29.1.3, `overlayfs`, daemon started via `service docker start` |
| docker compose | v2.40.3 (`docker-compose-v2`) — the plain `docker.io` package ships without it and harbor fails with `unknown flag: --project-name` |
| harbor | 0.25.0 via `uv tool install harbor` |
| dataset | `terminal-bench/terminal-bench@4.1.0`, 66 tasks |

Both control agents produce the expected grade, which is what proves the harness and the
verifier are wired correctly:

| Agent | Task | Trials | Exceptions | Mean reward |
|---|---|---|---|---|
| `oracle` (runs the task's own solution) | `terminal-bench/html-js-filter` | 1 | 0 | **1.000** |
| `nop` (does nothing) | `terminal-bench/html-js-filter` | 1 | 0 | **0.000** |

Run command that works (note the timeout multiplier — WSL2's first environment start is slow;
without it harbor raises `EnvironmentStartTimeoutError` after 600s):

```bash
wsl -d Ubuntu-26.04 -e bash -lc '
  export PATH="$HOME/.local/bin:$PATH"; service docker start; sleep 2; cd /root
  harbor run -d terminal-bench/terminal-bench@4.1.0 -e docker -a oracle \
    -i "terminal-bench/html-js-filter" -l 1 -n 1 -y --timeout-multiplier 4.0'
```

Notes that cost time to discover:

- Task filters need the fully qualified name (`terminal-bench/html-js-filter`), not the short
  directory name.
- Five of the 66 tasks require a GPU (`fp8-rmsnorm-gemm`, `jax-speedrun-gpu`, `math-eval-grader`,
  `vllm-deepseek-streaming`, `vpp-loss-divergence`). Exclude them on a CPU-only host.
- The verifier runs in its own environment (`environment_mode = "separate"`) and its start is the
  slowest step; the multiplier applies to it too.

## Remaining blocker for a real model

The model gateway on Windows binds `0.0.0.0:20128`, but the WSL vNIC cannot reach it — Windows
Firewall drops the inbound connection (`curl http://192.168.32.1:20128/v1/models` from WSL returns
empty). Two ways to close it:

```powershell
# 1. allow the WSL subnet through the firewall (administrator)
New-NetFirewallRule -DisplayName "WSL gateway 20128" -Direction Inbound `
  -InterfaceAlias "vEthernet (WSL)" -LocalPort 20128 -Protocol TCP -Action Allow
```

or point the agent at a provider that the container can reach and pass the key with `--ae`
(`harbor run ... -a <agent> -m <model> --base-url ... --api-format openai_chat_completions`).

## What "benchmark goon" additionally requires

Harbor drives a fixed catalogue of agents (`harbor agent list`) or a custom agent passed as
`-a module.path:ClassName`. `goon` is not in the catalogue and does not speak ACP, so running it
needs an adapter class on Harbor's side:

1. install the `goon` binary inside the task environment (or bind-mount it),
2. translate the task instruction into `goon --yes -p '<instruction>'`,
3. stream/capture its event log into the trial's trajectory directory,
4. report model/token usage back through the agent context.

The adapter is pure host-side Python and leaves the tasks and the verifier untouched, which is
what keeps the numbers comparable. Its prerequisite is the working sandbox above, which now
exists.

