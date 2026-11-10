# Terminal-Bench 4.1: status and how to run it

Target: <https://www.tbench.ai/> — Terminal-Bench 4.1, run through Harbor
(<https://github.com/harbor-framework/terminal-bench>).

## What was prepared on this machine

| Step | Result |
|---|---|
| `uv tool install harbor` | installed (`harbor`, `hb`, `hr` on PATH) |
| `harbor download terminal-bench/terminal-bench@4.1.0` | **66 tasks** downloaded to `~/terminal-bench` |
| `harbor job start ... --dry-run` | config resolves, agent/env/auth preflight runs |
| `harbor job start ... -e docker` | **blocked**: `Docker is not installed or not on PATH.` |

Task layout confirmed on disk: `instruction.md`, `solution/`, `tests/`, `environment/Dockerfile`,
`task.toml` (per-task verifier timeout, `environment_mode = "separate"`, GPU-requiring tasks
included).

## Why it cannot run here

Terminal-Bench executes every task inside a container image built from the task's `Dockerfile`.
Harbor's environment backends are: `docker`, `podman`, `singularity`, `apple-container`, `kata`,
`dind`, and the cloud providers (`modal`, `daytona`, `e2b`, `runloop`, `prime`, `beam`, `ec2`,
`gke`, …). There is **no host-process backend**, and this machine has:

- no Docker / Podman / WSL (WSL2 is not installed, and installing it needs admin + reboot),
- no Modal token or other cloud sandbox credentials,
- no provider API keys for the catalogue agents.

So the honest status is: **dataset and runner installed, execution blocked by the missing
sandbox**. No resolution rate is reported here, because any number produced without the task
images would be a fake Terminal-Bench score.

## To unblock

Either install a runtime (needs administrator rights):

```powershell
wsl --install            # then install Docker Desktop, or podman desktop
```

or point Harbor at a cloud sandbox (needs an account):

```powershell
uv tool install 'harbor[modal]'
modal token new
```

Then a real run is:

```bash
harbor run -d terminal-bench/terminal-bench@4.1.0 \
  -e docker            # or -e modal \
  -a <agent> -m <model> -k 5
harbor view jobs
```

## What "benchmark goon" would additionally require

Harbor drives a fixed catalogue of agents (`harbor agent list`) or a custom agent passed as
`-a module.path:ClassName`. `goon` is not in the catalogue and does not speak ACP, so running it
needs an adapter class implementing `harbor.agents.base.BaseAgent`:

1. install the `goon` binary inside the task environment (or bind-mount it),
2. translate the task instruction into `goon --yes -p '<instruction>'`,
3. stream/capture its event log into the trial's trajectory directory,
4. report model/token usage back through the agent context.

That adapter is pure Python on Harbor's side and does not touch the benchmark's tasks or verifier,
which is the property that keeps results comparable. It is the next piece of work; it cannot be
validated on this machine until a container runtime exists.
