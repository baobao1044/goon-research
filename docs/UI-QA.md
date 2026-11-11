# UI QA — automated evidence for `goon desktop`

`goon web` serves one hand-written HTML/CSS/JS document out of the binary (`include_str!` in
`src/web/mod.rs`). `npm test` never looks at it, so before this script the only way to know whether
the desktop shell still worked was to open it and look. `scripts/ui-check.mjs` replaces that with
checks that fail on their own, plus committed screenshots to inspect afterwards.

Zero dependencies: Node built-ins plus the Chrome that is already installed. No Playwright, no
Puppeteer, no image library, no npm package.

```console
npm run ui:check                        # compare against the committed baseline
node scripts/ui-check.mjs --update      # accept the current UI as the new baseline
```

Exit code `0` when every check passed, `1` when anything failed (CI-safe).

## Requirements

| What | Detail |
| --- | --- |
| Node | >= 22.16 (uses the built-in global `WebSocket`) |
| Binary | `target/debug/goon.exe` / `target/release/goon.exe`, or `GOON_TEST_BIN`, or `GOON_BINARY`. Falls back to `bin/goon.mjs`, which cannot run the transcript state (see limits). |
| Chrome | Chrome or Edge. Auto-detected from `PROGRAMFILES` / `LOCALAPPDATA`; override with `--chrome <path>` or `CHROME_PATH`. |
| Build | The UI is embedded at compile time, so **rebuild before checking UI changes**. The script warns when `src/web/*` is newer than the binary ("binary embeds a stale UI"). |

Nothing touches your real state: the run gets a scratch `XDG_CONFIG_HOME` / `XDG_DATA_HOME` /
`GOON_DATA_DIR` under the OS temp directory, and the `dataDir` from `/api/meta` is asserted to point
there.

## What is checked

### Server and HTTP contract (no browser needed)

| Check | Meaning |
| --- | --- |
| `/api/meta` answers 200 within the timeout | The server came up on the chosen port (default: a free one, or `--port`). |
| server metadata is well formed | `version` is a string, `cwd` is the scratch workspace, `running` is a number. |
| server is isolated from real user state | `meta.dataDir` points inside the scratch dir, so the run cannot read or write real sessions. |
| binary is newer than `src/web` assets | PASS when the binary is current; WARN (with the list of newer files) when the embedded UI is stale. |
| `GET /` 200 `text/html` | The document is served with the right content type. |
| served document references `/app.css` and `/app.js` | Route names in the markup match the routes in `src/web/mod.rs`, and `#intro` / `#transcript` exist in the markup. |
| `GET /app.css` 200 `text/css`, `GET /app.js` 200 `…javascript` | Asset routes and their content types. |
| `/app.css` under 40 KB, `/app.js` under 60 KB | Budget check: the shell is meant to stay a single small self-contained document. |
| `GET /api/sessions` 200 JSON array | The session list still parses as an array. |
| unknown route answers 404 | The fallback route still 404s instead of serving the UI. |
| no external URL in served HTML/CSS/JS | `http://` / `https://` outside comments means the shell started depending on a CDN or remote asset. |

### Browser states (headless Chrome over the DevTools Protocol)

Three states are captured, each with DOM assertions and one screenshot:

| State | DOM assertions |
| --- | --- |
| `intro` — right after load (~0.4–0.9 s) | `#intro` is in the document and `#shell` still has `is-hidden`. |
| `idle` — after the intro | `#shell.is-in` appears and `#intro` is removed; `#transcript`, `#prompt`, `#send` exist; `#prompt` is fully inside the viewport, `#send` is inside it, and `documentElement.scrollWidth` does not exceed the viewport (no horizontal overflow). |
| `transcript` — a real run | The prompt is typed, the form is submitted, and a stub run is streamed back. The reply marker, the user turn, and an idle status must all be visible. |

The transcript run is offline and free: the web server spawns `goon --json --yes -p …` per prompt
and the child inherits `GOON_STUB=1` + `GOON_STUB_FIXTURE=scripts/ui-check/fixtures/stub-transcript.json`
from the script's environment, so the deterministic stub provider answers instead of a model.

Runtime hygiene checks run over the same session:

| Check | Meaning |
| --- | --- |
| no console errors or uncaught exceptions | `console.error`, `assert`, `Log.entryAdded` errors, `Runtime.exceptionThrown`. |
| no failed network requests | No `Network.loadingFailed` and no 4xx/5xx on a requested URL. |
| no third-party network requests | Every request stays on the local origin (no CDN, no telemetry endpoint, no font host). |

## Screenshots and the baseline

| Path | Purpose |
| --- | --- |
| `scripts/ui-check/shots/*.png` | This run's states. Git-ignored (see `scripts/ui-check/.gitignore`). |
| `scripts/ui-check/shots/*.diff.png` | Changed pixels against the baseline: magenta = different, teal = deliberately masked, dim grey = unchanged. |
| `scripts/ui-check/baseline/*.png` | The committed reference images (`intro`, `idle`, `transcript`). |
| `scripts/ui-check/baseline/baseline.json` | Chrome version, viewport, tolerance and per-file sha256/size that produced the baseline. |

Comparison is a byte hash first, then a pixel diff:

* identical hash → `pixel-identical`;
* different pixels **only inside the masked regions** → treated as identical (see below);
* different pixels elsewhere → `changed`: passes when the diff is under `--tolerance` (default
  `0.5%` of compared pixels), fails above it, and always writes a `.diff.png`.

### Determinism measures

Animations are the enemy of a stable screenshot, so the script injects one stylesheet before the
document runs that snaps every animation and transition to its end state
(`animation-duration: 0.001s`, `transition-duration: 0.001s`) and makes the text caret transparent
(the composer is focused by design, and a blinking caret was the only pixel that changed between two
otherwise identical runs). The viewport is fixed at 1280×860 with `deviceScaleFactor: 1`, each
capture waits for two frames plus a short tail, and the network has to be quiet before the shutter.
The intro state is captured straight after the load event, well inside the ~1.9 s intro timer.

With those measures the three states are reproducible on a fixed machine: repeated runs report
`pixel-identical` for `intro` and "identical outside masked regions" for `idle` and `transcript`.

Two regions are masked out of the pixel comparison because they are **supposed** to change every
run (`MASK_SELECTORS` in the script):

* `#cwd` — prints the scratch workspace path, which contains a pid and a timestamp;
* `#sessions` — the rail lists session ids (`04514239 …`) and cwd fragments from the run that just
  happened.

Masked pixels are painted teal in the diff image so they are never mistaken for unchanged UI. If a
regression moves *into* a masked region it will not be caught visually — the DOM assertions are the
safety net there.

### Updating the baseline

```console
node scripts/ui-check.mjs --update
```

Run this only when the visual change is intended, and commit the regenerated PNGs together with the
change that produced them. Review the images first (`scripts/ui-check/shots/` before the update) —
`--update` accepts whatever the binary currently serves.

## Options

```text
--update             overwrite scripts/ui-check/baseline/*.png instead of comparing
--port <n>           bind the web server to a fixed port (default: a free one)
--keep               leave the web server running after the report (debug by hand)
--verbose            print every extra detail (bytes, urls, timings, dom values)
--chrome <path>      Chrome executable (default: CHROME_PATH env, then well-known paths)
--timeout <ms>       per-wait timeout budget (default: 20000)
--tolerance <pct>    max share of changed pixels before a screenshot fails (default: 0.5)
--no-shots           skip screenshots and baseline comparison (HTTP + DOM checks only)
-h, --help           usage
```

`--keep` prints the URL and the scratch directory and leaves the server running, so you can open the
same document in a normal browser next to the captured screenshots. Stop it with Ctrl+C or `taskkill`.

## Honest limits

* **Not a full visual regression suite.** Three states at one viewport (1280×860) at DPR 1. There is
  no mobile width, no dark/light theme matrix, no RTL, no zoom levels, no per-component screenshots.
* **No animation or timing verification.** Animations are frozen to their end state on purpose, so
  the intro's motion design is not tested — only that the intro exists, that it is on screen when the
  document loads, and that it is replaced by `#shell.is-in`. A UI that auto-dismisses the intro
  faster than ~1 s can make the `intro` capture race; the check then fails loudly instead of passing
  silently.
* **No interaction coverage beyond one prompt.** The script types one prompt and submits it. Buttons,
  the stop control, keyboard shortcuts, chip clicks, scroll behaviour, clipboard, and the session
  rail's copy button are not exercised.
* **Chrome must be installed.** Without it the HTTP checks still run, the browser checks are marked
  `SKIP`, and the run fails with `chrome available` — it is not silently green.
* **Baselines are machine-specific.** Font rendering and antialiasing differ per OS and per Chrome
  version, which is why `baseline.json` records the Chrome version and why a version bump produces a
  WARNING. Regenerate the baseline on the machine/CI image that will own it.
* **Pixel tolerance is a heuristic.** Real diff percentages are reported, but a small, genuine
  regression under 0.5% of pixels can pass. Tighten with `--tolerance 0.05` when reviewing a risky
  change; the diff image tells you what moved.
* **Not an accessibility, i18n, or security audit.** No axe run, no contrast math, no locale matrix,
  no CSP/XSS assertions.
* **Serving is from the binary.** `src/web/*` edits only reach the checks after `cargo build`; the
  staleness WARNING is the only thing standing between you and a green run against the previous UI.

## Debugging a failure

1. Read the failing check's note — it names the assertion and usually the number that broke it.
2. `scripts/ui-check/shots/<state>.png` is what the UI looked like; `<state>.diff.png` is what moved
   against the baseline (magenta).
3. Re-run with `--verbose` for request URLs, timings, byte sizes and DOM values.
4. Re-run with `--keep` to poke the same server by hand (it prints the URL).
5. Server and Chrome logs live in the scratch directory printed in the header (`server.err`,
   `chrome.err`); with `--keep` that directory survives the run.
