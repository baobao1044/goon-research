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
| Build | The UI is embedded at compile time, so **rebuild before checking UI changes**. The script warns when anything under `src/web/` is newer than the binary ("binary embeds a stale UI"). |

Nothing touches your real state: the run gets a scratch `XDG_CONFIG_HOME` / `XDG_DATA_HOME` /
`GOON_DATA_DIR` under the OS temp directory, and the `dataDir` from `/api/meta` is asserted to point
there. The prompt the UI sends runs the deterministic stub provider (`GOON_STUB` +
`scripts/ui-check/fixtures/stub-transcript.json`, inherited by the server's child process), so
checking the shell costs no tokens and needs no network.

## What is checked

### Server and HTTP contract (no browser needed)

| Check | Meaning |
| --- | --- |
| `/api/meta` answers 200 within the timeout | The server came up on the chosen port (default: a free one, or `--port`). |
| server metadata is well formed | `version` is a string, `cwd` is the scratch workspace, `running` is a number. |
| server is isolated from real user state | `meta.dataDir` points inside the scratch dir, so the run cannot read or write real sessions. |
| binary is newer than `src/web` assets | PASS when the binary is current; WARN (with the list of newer files) when the embedded UI is stale. |
| server surface is the rewritten one | Probes `GET /api/health`. The old server had no such route, and the rewrite brought the CSP/ETag/409 behaviour — see the gated group below. |
| `GET /` 200 `text/html` | The document is served with the right content type. |
| served document references `/app.css` and `/app.js` | Route names in the markup match the routes in `src/web/mod.rs`, and `#intro` / `#transcript` exist in the markup. |
| document has no inline code or handlers | No `<script>` without `src`, no `<style>` block, no `style=""` attribute, no `on*=""` handler. The document ships with `script-src 'self'` / `style-src 'self'`, so any of those would be dead code at best. |
| `GET /app.css` 200 `text/css`, `GET /app.js` 200 `…javascript` | Asset routes and their content types. |
| `/app.css` under 40 KB, `/app.js` under 60 KB | Budget check: the shell is meant to stay a single small self-contained document. |
| `GET /api/sessions` 200 JSON array | The session list still parses as an array. |
| unknown route answers 404 | The fallback route still 404s instead of serving the UI. |
| no external URL in served HTML/CSS/JS | `http://` / `https://` outside comments means the shell started depending on a CDN or remote asset. |

### Rewritten server surface

These are gated on `GET /api/health`. Against a binary built before the server rewrite they are
reported as `SKIP` with a "run `cargo build --bin goon`" note, never as a failure, so an old binary
does not produce false alarms.

The API checks run **before** the browser phase (and the browser phase before the screenshots):
a full browser pass takes tens of seconds, and a parallel `cargo build` / `taskkill /IM goon.exe`
can kill the server in the middle of it. Cheap checks first means an interrupted run still produced
useful evidence.

| Check | Meaning |
| --- | --- |
| CSP on the document | `content-security-policy` on `/` contains `default-src 'none'`, `script-src 'self'`, `style-src 'self'`, `connect-src 'self'`, and no `unsafe-inline` / `unsafe-eval`. |
| nosniff + no-referrer headers | `x-content-type-options: nosniff` and `referrer-policy: no-referrer` on `/` and on an API response. |
| asset ETag revalidation (304) | `/app.css` and `/app.js` carry an `etag` and `cache-control: no-cache`; `If-None-Match` with that tag returns `304` with an empty body, an unrelated tag still returns `200`. |
| Host guard rejects a foreign Host | `Host: rebind.example` gets `403` (DNS-rebinding defence), `Host: localhost:<port>` still gets `200`. |
| Origin guard rejects a foreign Origin | `POST /api/prompt` with `Origin: https://evil.example` gets `403` **and** starts no run (`meta.running` stays `0`). |
| wrong verb on a known route is 405 | `DELETE /app.css` → `405`. |
| `/api/config` exposes no secrets | Required keys present, no key whose name looks like a credential, and no `sk-`/bearer-shaped value in the body. |
| `/api/runs` is a bare array | The runs list is the response body itself, not an object wrapping it. |
| `/api/sessions/:id/events returns a session log` | After a run: `{id, events[], truncated}` honouring `limit`; a traversal-shaped id → `400`; an unknown id → `404`. |
| second concurrent prompt is 409 | While one run streams, a second `POST /api/prompt` answers `409` (it used to answer `200`). |

### Browser states (headless Chrome over the DevTools Protocol)

Each viewport (`1280x860`, then `900x600`) is driven through the same three states, with DOM
assertions and one screenshot each:

| State | DOM assertions |
| --- | --- |
| `intro` — right after load | `#intro` is in the document and `#shell` still has `is-hidden`. |
| `idle` — after the intro | `#shell.is-in` appears and `#intro` is removed; `#transcript`, `#prompt`, `#send` exist; `#prompt` and `#send` are fully inside the viewport and `documentElement.scrollWidth` does not exceed it (no horizontal overflow). |
| `transcript` — a real run | The prompt is typed, the form is submitted, and a stub run is streamed back. The reply marker, the user turn, and an idle status must all be visible. |

Runtime hygiene checks run over the same session:

| Check | Meaning |
| --- | --- |
| no console errors or uncaught exceptions | `console.error`, `assert`, `Log.entryAdded` errors, `Runtime.exceptionThrown`. |
| no failed network requests | No `Network.loadingFailed` and no 4xx/5xx on a requested URL. |
| no third-party network requests | Every request stays on the local origin (no CDN, no telemetry endpoint, no font host). |

## Screenshots and the baseline

| Path | Purpose |
| --- | --- |
| `scripts/ui-check/shots/*.png` | This run's states, both viewports. Git-ignored (see `scripts/ui-check/.gitignore`). |
| `scripts/ui-check/shots/*.diff.png` | Changed pixels against the baseline: magenta = different, teal = deliberately masked, dim grey = unchanged. |
| `scripts/ui-check/baseline/*.png` | The committed reference images: `intro`, `idle`, `transcript` (1280x860) and `*-compact` (900x600). |
| `scripts/ui-check/baseline/baseline.json` | Chrome version, viewports, tolerance and per-file sha256/size that produced the baseline. |

Comparison is a byte hash first, then a pixel diff:

* identical hash → `pixel-identical`;
* different pixels **only inside the masked regions** → treated as identical (see below);
* different pixels elsewhere → `changed`: passes when the diff is under `--tolerance` (default
  `0.5%` of compared pixels), fails above it, and always writes a `.diff.png`.

### Determinism measures

Animations are the enemy of a stable screenshot, so the script injects one stylesheet before the
document runs that snaps every animation and transition to their end state
(`animation-duration: 0.001s`, `transition-duration: 0.001s`) and makes the text caret transparent
(the composer is focused by design, and a blinking caret was the only pixel that changed between two
otherwise identical runs).

That stylesheet is registered as a **constructable stylesheet** (`document.adoptedStyleSheets`), not
as an inline `<style>`: the rewritten server sends `style-src 'self'`, which blocks inline styles,
and a constructable sheet is not subject to `style-src`. If the marker property is still missing
after load, the script falls back to `Page.setBypassCSP` plus an inline `<style>` and reloads — and
says so in the report, because that path weakens what the page is allowed to do.

On top of that: a throwaway Chrome profile per run (no cache, no service worker), a fresh `?ui-check=`
query on every navigation, a fixed viewport with `deviceScaleFactor: 1`, two frames plus a short tail
before every capture, and a wait for the network to go quiet. The intro is captured straight after
the load event, well inside the ~1.9 s intro timer.

Two kinds of region are masked out of the pixel comparison because they are **supposed** to change
every run:

* `#cwd` — prints the scratch workspace path, which contains a pid and a timestamp;
* each row of `#sessions` — the rail lists session ids (`04514239 …`) and cwd fragments from the run
  that just happened. Rows are masked individually rather than the container, because the container
  can stretch to the full rail height and would hide a large part of the layout from the diff.

Masked pixels are painted teal in the diff image so they are never mistaken for unchanged UI. If a
regression moves *into* a masked region it will not be caught visually — the DOM assertions are the
safety net there.

With those measures the six states are reproducible on a fixed machine: repeated runs report
`pixel-identical` for the intro states and "identical outside masked regions" for the rest.

### Updating the baseline

```console
node scripts/ui-check.mjs --update
```

Run this only when the visual change is intended, and commit the regenerated PNGs together with the
change that produced them. Review the images first (`scripts/ui-check/shots/` before the update) —
`--update` accepts whatever the binary currently serves. Rebuild before running either mode: the
assets live in the binary, so a stale build silently describes the previous UI.

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

* **Not a full visual regression suite.** Six states, two viewports (1280×860 and 900×600), DPR 1.
  No mobile width, no dark/light theme matrix, no RTL, no zoom levels, no per-component screenshots.
* **No animation or timing verification.** Animations are frozen to their end state on purpose, so
  the intro's motion design is not tested — only that the intro exists, that it is on screen when the
  document loads, and that it is replaced by `#shell.is-in`. A UI that dismisses the intro faster
  than ~1 s can make the `intro` capture race; the check then fails loudly instead of passing
  silently.
* **No interaction coverage beyond one prompt.** The script types one prompt and submits it. Buttons,
  the stop control, keyboard shortcuts, chip clicks, the find bar, drag-and-drop, scroll behaviour,
  clipboard and the session rail's copy button are not exercised.
* **The screenshot session may bypass CSP.** Only when the constructable-stylesheet path does not
  take effect (see determinism). CSP itself is asserted from the HTTP headers and by the static
  inline-code scan, not by observing the browser block something.
* **New-surface checks need a rebuilt binary.** Against a binary older than the server rewrite, the
  CSP/ETag/guard/409 checks report `SKIP` instead of failing — a green run on a stale binary means
  "the old surface still works", not "the new surface is verified".
* **`/api/sessions/:id/events` depends on the event log.** The script exports `GOON_EVENT_LOG=1` so
  the child run writes one; without a log the route correctly answers `404` and the check would fail
  for the wrong reason.
* **Chrome must be installed.** Without it the HTTP checks still run, the browser checks are marked
  `SKIP`, and the run fails with `chrome available` — it is not silently green.
* **Baselines are machine-specific.** Font rendering and antialiasing differ per OS and per Chrome
  version, which is why `baseline.json` records the Chrome version and why a version bump produces a
  WARNING. Regenerate the baseline on the machine/CI image that will own it.
* **Pixel tolerance is a heuristic.** Real diff percentages are reported, but a small, genuine
  regression under 0.5% of pixels can pass. Tighten with `--tolerance 0.05` when reviewing a risky
  change; the diff image tells you what moved.
* **Not an accessibility, i18n, or security audit.** No axe run, no contrast math, no locale matrix,
  no fuzzing of the request parser (that belongs in `src/web/tests.rs`).
* **Sources moving under the run are reported honestly.** If `src/web/*` is edited (or the binary
  rebuilt) between `--update` and the compare run, the baseline legitimately mismatches. Re-run
  `--update` against a stable tree.
* **An external `taskkill /IM goon.exe` or a parallel `cargo build` can kill the server mid-run.**
  That shows up as `the web server exited during the run`; it is an environment problem, not a UI
  regression.

## Debugging a failure

1. Read the failing check's note — it names the assertion and usually the number that broke it.
2. `scripts/ui-check/shots/<state>.png` is what the UI looked like; `<state>.diff.png` is what moved
   against the baseline (magenta).
3. Re-run with `--verbose` for request URLs, timings, byte sizes, ETags and DOM values.
4. Re-run with `--keep` to poke the same server by hand (it prints the URL).
5. Server and Chrome logs live in the scratch directory printed in the header (`server.err`,
   `chrome.err`); with `--keep` that directory survives the run.
