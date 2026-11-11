#!/usr/bin/env node
// goon desktop UI check: machine-verifiable evidence for the `goon web` front-end.
//
// The shell (`src/web/`) is a hand-written HTML/CSS/JS document served by the binary. Nothing in
// `npm test` looks at it, so a design or behaviour change used to be reviewable only by opening the
// window and staring at it. This script closes that gap with three artefacts per run:
//
//   1. HTTP contract checks against the live server: routes, content types, size budgets, the
//      security posture (CSP, nosniff, no-referrer, Host/Origin guards, ETag revalidation) and an
//      "no external resource / no inline code" scan of the document.
//   2. Headless Chrome driven over the DevTools Protocol with Node's built-in `WebSocket` â€” no
//      Playwright, no Puppeteer, no new dependency: DOM assertions, console-error collection,
//      layout sanity, and screenshots of three states at two viewports.
//   3. A byte-level baseline comparison per screenshot (`scripts/ui-check/baseline/*.png`) with a
//      PNG pixel-diff, so a "changed" verdict points at the pixels that moved.
//
// Exit code is 1 if any check fails. `--update` rewrites the baselines instead of comparing.
//
// The server was rewritten (bounded HTTP parser, CSP, ETag, `/api/health`, 409 on a second run).
// Checks for that surface are gated on `GET /api/health`: against a binary that predates the
// rewrite they are reported as SKIP with a "rebuild" note instead of failing. Limits are documented
// in `docs/UI-QA.md`; the short version is that this is a layout and wiring check, not a full
// visual-regression suite and not a timing test for the intro animation.

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, open, readFile, rm, stat, writeFile } from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');
const WORK = join(HERE, 'ui-check');
const BASELINE_DIR = join(WORK, 'baseline');
const SHOTS_DIR = join(WORK, 'shots');
const FIXTURE = join(WORK, 'fixtures', 'stub-transcript.json');

const SIZES = { css: 40 * 1024, js: 60 * 1024 };
const VIEWPORTS = [
  { name: '1280x860', width: 1280, height: 860, suffix: '' },
  { name: '900x600', width: 900, height: 600, suffix: '-compact' },
];
// Regions the shell fills with per-run data. They are excluded from the pixel comparison: the
// session rail lists random session ids and the cwd chip prints the scratch directory. The rail is
// masked row by row (`MASK_CHILD_SELECTORS`) instead of by container, because the container can
// stretch to the full rail height and would hide a large part of the layout from the diff.
const MASK_SELECTORS = ['#cwd'];
const MASK_CHILD_SELECTORS = ['#sessions'];
const STUB_MARKER = 'UI-CHECK-STUB-OK';
// Injected before the document runs. It is registered as a constructable stylesheet on purpose:
// the rewritten server sends `style-src 'self'`, which blocks an inline <style> element, and
// `document.adoptedStyleSheets` is not subject to `style-src`. The fallback (inline <style>) only
// runs on a page without that CSP.
const FREEZE_CSS = 'html{--ui-check-freeze:1}' +
  '*,*::before,*::after{animation-duration:0.001s!important;animation-delay:0s!important;' +
  'animation-iteration-count:1!important;transition-duration:0.001s!important;transition-delay:0s!important;' +
  'scroll-behavior:auto!important}' +
  // The composer is focused by design, and a blinking text caret is the one thing in the document
  // that changes between two otherwise identical screenshots.
  '*,input,textarea,button:focus,[contenteditable]{caret-color:transparent!important}';

// ---------------------------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------------------------

const HELP = `goon desktop UI check

Usage: node scripts/ui-check.mjs [options]

  --update             overwrite scripts/ui-check/baseline/*.png instead of comparing
  --port <n>           bind the web server to a fixed port (default: pick a free one)
  --keep               leave the web server running after the report (debug by hand)
  --verbose            print every extra detail (bytes, urls, timings, dom values)
  --chrome <path>      Chrome executable (default: CHROME_PATH env, then well-known paths)
  --timeout <ms>       per-wait timeout budget (default: 20000)
  --tolerance <pct>    max share of changed pixels before a screenshot fails (default: 0.5)
  --no-shots           skip screenshots and baseline comparison (HTTP + DOM checks only)
  -h, --help           this text

Artifacts:
  scripts/ui-check/shots/*.png               screenshots from this run (both viewports)
  scripts/ui-check/shots/*.diff.png          changed pixels against the baseline
  scripts/ui-check/baseline/*.png            committed baseline (written by --update)
  scripts/ui-check/baseline/baseline.json    what produced the baseline (chrome, viewports, hashes)
`;

function parseArgs(argv) {
  const options = { update: false, keep: false, verbose: false, shots: true, port: 0, chrome: process.env.CHROME_PATH || '', timeout: 20_000, tolerance: 0.5, help: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      const value = argv[index + 1];
      if (value === undefined) throw new Error(`${arg} needs a value`);
      index += 1;
      return value;
    };
    switch (arg) {
      case '--update': options.update = true; break;
      case '--keep': options.keep = true; break;
      case '--verbose': options.verbose = true; break;
      case '--no-shots': options.shots = false; break;
      case '--help': case '-h': options.help = true; break;
      case '--port': options.port = Number(next()); break;
      case '--chrome': options.chrome = next(); break;
      case '--timeout': options.timeout = Number(next()); break;
      case '--tolerance': options.tolerance = Number(next()); break;
      default: throw new Error(`unknown option: ${arg}`);
    }
  }
  if (!Number.isFinite(options.tolerance) || options.tolerance < 0) throw new Error('--tolerance must be a number >= 0');
  if (!Number.isFinite(options.timeout) || options.timeout < 1000) throw new Error('--timeout must be >= 1000');
  return options;
}

// ---------------------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------------------

const results = [];
const SHOT_FILES = {};
let verbose = false;

function record(status, name, note = '') {
  results.push({ status, name, note });
  process.stdout.write(`  ${status.padEnd(4)}  ${name}${note ? `  ${note}` : ''}\n`);
  return status;
}

function detail(text) {
  if (verbose) process.stdout.write(`        ${text}\n`);
}

async function check(name, body) {
  try {
    return record('PASS', name, (await body()) ?? '');
  } catch (error) {
    return record('FAIL', name, error instanceof Error ? error.message : String(error));
  }
}

const warn = (name, note) => record('WARN', name, note);
const skip = (name, note) => record('SKIP', name, note);

const delay = ms => new Promise(settle => setTimeout(settle, ms));

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

function fail(message) {
  throw new Error(message);
}

// ---------------------------------------------------------------------------------------------
// Child processes (stdio goes to files: no pipes, no deadlock, and the logs survive a --keep run)
// ---------------------------------------------------------------------------------------------

function killTree(pid) {
  if (!pid) return;
  try {
    if (process.platform === 'win32') spawn('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' });
    else process.kill(pid, 'SIGKILL');
  } catch {
    // The process was already gone; nothing to clean up.
  }
}

async function spawnWithLogs(command, args, { cwd, env, logPath }) {
  await mkdir(dirname(logPath), { recursive: true });
  const out = await open(`${logPath}.out`, 'w');
  const err = await open(`${logPath}.err`, 'w');
  const child = spawn(command, args, { cwd, env, stdio: ['ignore', out.fd, err.fd], windowsHide: true });
  await out.close();
  await err.close();
  return child;
}

async function readLog(path) {
  try {
    return await readFile(path, 'utf8');
  } catch {
    return '';
  }
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

function request(url, { method = 'GET', body = null, headers = {}, timeout = 10_000 } = {}) {
  return new Promise((settle, reject) => {
    const parsed = new URL(url);
    const req = http.request({ hostname: parsed.hostname, port: parsed.port, path: parsed.pathname + parsed.search, method, headers: { connection: 'close', ...headers } }, response => {
      const chunks = [];
      response.on('data', chunk => chunks.push(chunk));
      response.on('end', () => settle({ status: response.statusCode ?? 0, headers: response.headers, body: Buffer.concat(chunks) }));
      response.on('error', reject);
    });
    req.setTimeout(timeout, () => req.destroy(new Error(`timeout after ${timeout}ms`)));
    req.on('error', reject);
    req.end(body ?? undefined);
  });
}

/** Like `request`, but settles as soon as the response head arrives (used for a live SSE stream). */
function requestStream(url, { method = 'GET', body = null, headers = {}, timeout = 30_000 } = {}) {
  return new Promise((settle, reject) => {
    const parsed = new URL(url);
    const req = http.request({ hostname: parsed.hostname, port: parsed.port, path: parsed.pathname + parsed.search, method, headers: { connection: 'close', ...headers } }, response => {
      const chunks = [];
      const done = new Promise((finish, abort) => {
        response.on('data', chunk => chunks.push(chunk));
        response.on('end', () => finish({ status: response.statusCode ?? 0, headers: response.headers, body: Buffer.concat(chunks) }));
        response.on('error', abort);
      });
      settle({ status: response.statusCode ?? 0, headers: response.headers, done, close: () => req.destroy() });
    });
    req.setTimeout(timeout, () => req.destroy(new Error(`timeout after ${timeout}ms`)));
    req.on('error', reject);
    req.end(body ?? undefined);
  });
}

async function requestOk(url, options) {
  const response = await request(url, options);
  if (response.status !== 200) fail(`${url} returned ${response.status}`);
  return response;
}

function jsonBody(response, label) {
  try {
    return JSON.parse(response.body.toString('utf8'));
  } catch {
    return fail(`${label} is not JSON: ${response.body.toString('utf8').slice(0, 120)}`);
  }
}

function contentType(response) {
  return String(response.headers['content-type'] ?? '');
}

async function freePort() {
  return new Promise((settle, reject) => {
    const server = net.createServer();
    server.unref();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => settle(port));
    });
  });
}

// ---------------------------------------------------------------------------------------------
// Binary, Chrome, isolation env
// ---------------------------------------------------------------------------------------------

async function isFile(path) {
  try {
    return (await stat(path)).isFile() ? path : null;
  } catch {
    return null;
  }
}

async function resolveBinary() {
  const candidates = [];
  if (process.env.GOON_TEST_BIN) candidates.push(process.env.GOON_TEST_BIN);
  if (process.env.GOON_BINARY) candidates.push(process.env.GOON_BINARY);
  candidates.push(join(ROOT, 'target', 'debug', process.platform === 'win32' ? 'goon.exe' : 'goon'));
  candidates.push(join(ROOT, 'target', 'release', process.platform === 'win32' ? 'goon.exe' : 'goon'));
  for (const candidate of candidates) {
    const path = isAbsolute(candidate) ? candidate : resolve(ROOT, candidate);
    if (await isFile(path)) return { mode: 'native', command: path, args: [] };
  }
  // Last resort: the npm shim. `goon web` spawns `current_exe()` for each prompt, so with the shim
  // that child is node(1) and the transcript state cannot run; the caller degrades that check.
  const shim = join(ROOT, 'bin', 'goon.mjs');
  if (await isFile(shim)) return { mode: 'shim', command: process.execPath, args: [shim] };
  fail('no goon binary: build one with "cargo build --bin goon" or set GOON_TEST_BIN');
}

function chromeCandidates(explicit) {
  const list = [];
  if (explicit) list.push(explicit);
  if (process.platform === 'win32') {
    for (const base of ['PROGRAMFILES', 'PROGRAMFILES(X86)', 'LOCALAPPDATA']) {
      const root = process.env[base];
      if (root) list.push(join(root, 'Google', 'Chrome', 'Application', 'chrome.exe'));
      if (root) list.push(join(root, 'Microsoft', 'Edge', 'Application', 'msedge.exe'));
    }
  } else if (process.platform === 'darwin') {
    list.push('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome');
    list.push('/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge');
  } else {
    list.push('/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/microsoft-edge');
  }
  return list;
}

async function resolveChrome(explicit) {
  for (const candidate of chromeCandidates(explicit)) {
    if (await isFile(candidate)) return candidate;
  }
  return null;
}

/** Lists `sources` that are newer than the binary, i.e. the served UI predates the checkout. */
async function staleSince(binary, sources) {
  if (binary.mode !== 'native') return { assets: [], built: 'shim' };
  const builtAt = (await stat(binary.command)).mtimeMs;
  const newer = [];
  for (const source of sources) {
    try {
      const info = await stat(join(ROOT, source));
      if (info.mtimeMs > builtAt) newer.push(source);
    } catch {
      // A source that does not exist cannot make the binary stale.
    }
  }
  return { assets: newer, built: new Date(builtAt).toISOString() };
}

// ---------------------------------------------------------------------------------------------
// DevTools Protocol client (node:http harness + Node's built-in WebSocket)
// ---------------------------------------------------------------------------------------------

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.closed = false;
    socket.addEventListener('message', event => {
      let message;
      try {
        message = JSON.parse(typeof event.data === 'string' ? event.data : String(event.data));
      } catch {
        return;
      }
      if (message.id !== undefined) {
        const entry = this.pending.get(message.id);
        if (!entry) return;
        this.pending.delete(message.id);
        if (message.error) entry.reject(new Error(`${entry.method}: ${message.error.message}`));
        else entry.resolve(message.result);
        return;
      }
      for (const listener of this.listeners.get(message.method) ?? []) listener(message.params ?? {});
    });
    socket.addEventListener('close', () => {
      this.closed = true;
      for (const entry of this.pending.values()) entry.reject(new Error('devtools socket closed'));
      this.pending.clear();
    });
    socket.addEventListener('error', () => {
      for (const entry of this.pending.values()) entry.reject(new Error('devtools socket error'));
      this.pending.clear();
    });
  }

  static async connect(url, timeout) {
    const socket = new WebSocket(url);
    await new Promise((settle, reject) => {
      const timer = setTimeout(() => reject(new Error(`devtools connect timeout (${url})`)), timeout);
      socket.addEventListener('open', () => {
        clearTimeout(timer);
        settle();
      }, { once: true });
      socket.addEventListener('error', () => {
        clearTimeout(timer);
        reject(new Error(`devtools connect failed (${url})`));
      }, { once: true });
    });
    return new Cdp(socket);
  }

  send(method, params = {}) {
    if (this.closed) return Promise.reject(new Error(`cannot send ${method}: devtools closed`));
    const id = this.nextId++;
    return new Promise((settle, reject) => {
      this.pending.set(id, { resolve: settle, reject, method });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  on(method, listener) {
    const list = this.listeners.get(method) ?? [];
    list.push(listener);
    this.listeners.set(method, list);
  }

  waitEvent(method, timeout) {
    return new Promise((settle, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out waiting for ${method}`)), timeout);
      this.on(method, params => {
        clearTimeout(timer);
        settle(params);
      });
    });
  }

  close() {
    try {
      this.socket.close();
    } catch {
      // already closed
    }
  }
}

async function evaluate(cdp, expression) {
  const response = await cdp.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (response.exceptionDetails) {
    const description = response.exceptionDetails.exception?.description ?? response.exceptionDetails.text ?? 'evaluation failed';
    fail(description.split('\n')[0]);
  }
  return response.result.value;
}

async function waitForExpression(cdp, expression, timeout, label) {
  const start = Date.now();
  for (;;) {
    let value;
    try {
      value = await evaluate(cdp, expression);
    } catch {
      value = undefined;
    }
    if (value) return { value, elapsed: Date.now() - start };
    if (Date.now() - start > timeout) fail(`timed out after ${timeout}ms waiting for ${label}`);
    await delay(40);
  }
}

async function waitForQuiet(network, quietMs, timeout) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (network.inflight.size === 0 && Date.now() - network.lastAt > quietMs) return true;
    await delay(80);
  }
  return false;
}

/** Waits for two frames plus a short tail, so a capture never lands mid-layout or mid-paint. */
async function settleFrames(cdp, tailMs = 120) {
  try {
    await evaluate(cdp, `new Promise(settle => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(settle, ${Math.max(0, tailMs)}))))`);
  } catch {
    await delay(tailMs);
  }
}

// ---------------------------------------------------------------------------------------------
// Chrome lifecycle
// ---------------------------------------------------------------------------------------------

async function startChrome(chromePath, timeout, logPath) {
  const port = await freePort();
  // A throwaway profile per run: no cache, no service worker, no state carried between runs.
  const profile = join(tmpdir(), `goon-ui-check-chrome-${process.pid}-${Date.now()}`);
  await mkdir(profile, { recursive: true });
  const args = [
    '--headless=new',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-extensions',
    '--disable-background-networking',
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
    '--disable-sync',
    '--metrics-recording-only',
    '--mute-audio',
    '--hide-scrollbars',
    '--force-device-scale-factor=1',
    `--window-size=${VIEWPORTS[0].width},${VIEWPORTS[0].height}`,
    'about:blank',
  ];
  const child = await spawnWithLogs(chromePath, args, { env: process.env, logPath });
  const baseUrl = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + timeout;
  let version = null;
  for (;;) {
    try {
      const response = await request(`${baseUrl}/json/version`, { timeout: 1500 });
      if (response.status === 200) {
        version = JSON.parse(response.body.toString('utf8'));
        break;
      }
    } catch {
      // chrome is still binding its devtools port
    }
    if (Date.now() > deadline) {
      killTree(child.pid);
      fail(`chrome devtools endpoint did not come up within ${timeout}ms`);
    }
    await delay(120);
  }
  let target = null;
  for (;;) {
    const response = await request(`${baseUrl}/json/list`, { timeout: 2000 });
    const list = JSON.parse(response.body.toString('utf8'));
    target = list.find(entry => entry.type === 'page' && entry.webSocketDebuggerUrl);
    if (target) break;
    if (Date.now() > deadline) {
      killTree(child.pid);
      fail('chrome exposed no page target');
    }
    await delay(120);
  }
  return {
    child,
    version: version?.['Browser'] ?? 'unknown',
    wsUrl: target.webSocketDebuggerUrl,
    stop: () => killTree(child.pid),
    cleanup: () => rm(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }),
  };
}

// ---------------------------------------------------------------------------------------------
// PNG: decode, diff, encode (no image library, only node:zlib)
// ---------------------------------------------------------------------------------------------

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

function paeth(a, b, c) {
  const p = a + b - c;
  const pa = Math.abs(p - a);
  const pb = Math.abs(p - b);
  const pc = Math.abs(p - c);
  if (pa <= pb && pa <= pc) return a;
  return pb <= pc ? b : c;
}

function decodePng(buffer) {
  if (!Buffer.isBuffer(buffer) || buffer.length < 8 || !buffer.subarray(0, 8).equals(PNG_SIGNATURE)) return null;
  let offset = 8;
  let header = null;
  const idat = [];
  while (offset + 8 <= buffer.length) {
    const length = buffer.readUInt32BE(offset);
    const type = buffer.toString('ascii', offset + 4, offset + 8);
    const start = offset + 8;
    const end = start + length;
    if (end + 4 > buffer.length) return null;
    if (type === 'IHDR') {
      header = { width: buffer.readUInt32BE(start), height: buffer.readUInt32BE(start + 4), bitDepth: buffer[start + 8], colorType: buffer[start + 9], interlace: buffer[start + 12] };
    } else if (type === 'IDAT') {
      idat.push(buffer.subarray(start, end));
    } else if (type === 'IEND') {
      break;
    }
    offset = end + 4;
  }
  if (!header || idat.length === 0) return null;
  const channels = { 0: 1, 2: 3, 4: 2, 6: 4 }[header.colorType];
  if (!channels || header.bitDepth !== 8 || header.interlace !== 0) return null;
  const { width, height } = header;
  let raw;
  try {
    raw = zlib.inflateSync(Buffer.concat(idat));
  } catch {
    return null;
  }
  const stride = width * channels;
  if (raw.length < height * (stride + 1)) return null;
  const pixels = Buffer.alloc(height * stride);
  let previous = Buffer.alloc(stride);
  for (let y = 0; y < height; y += 1) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const out = pixels.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x += 1) {
      const left = x >= channels ? out[x - channels] : 0;
      const up = previous[x];
      const upLeft = x >= channels ? previous[x - channels] : 0;
      let value = line[x];
      if (filter === 1) value += left;
      else if (filter === 2) value += up;
      else if (filter === 3) value += (left + up) >> 1;
      else if (filter === 4) value += paeth(left, up, upLeft);
      else if (filter !== 0) return null;
      out[x] = value & 0xff;
    }
    previous = out;
  }
  return { width, height, channels, pixels };
}

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let crc = -1;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ -1) >>> 0;
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const typed = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typed));
  return Buffer.concat([length, typed, crc]);
}

function encodePng({ width, height, pixels }) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header[8] = 8;
  header[9] = 6;
  const stride = width * 4;
  const raw = Buffer.alloc(height * (stride + 1));
  for (let y = 0; y < height; y += 1) {
    raw[y * (stride + 1)] = 0;
    pixels.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([PNG_SIGNATURE, pngChunk('IHDR', header), pngChunk('IDAT', zlib.deflateSync(raw, { level: 9 })), pngChunk('IEND', Buffer.alloc(0))]);
}

/** Returns {changed, compared, percent, diff} where `diff` is a PNG highlighting moved pixels. */
function diffPng(before, after, masks = []) {
  if (before.width !== after.width || before.height !== after.height) {
    return { changed: 0, compared: 0, masked: 0, total: 0, percent: 100, diff: null, dimensionChange: true };
  }
  const { width, height, channels } = after;
  const total = width * height;
  // Regions the shell fills with run-specific data (session ids, scratch paths) are excluded from
  // the comparison; they differ on every run by design and would otherwise fail every time.
  const masked = new Uint8Array(total);
  for (const rect of masks) {
    const x0 = Math.max(0, rect.x);
    const y0 = Math.max(0, rect.y);
    const x1 = Math.min(width, rect.x + rect.width);
    const y1 = Math.min(height, rect.y + rect.height);
    for (let y = y0; y < y1; y += 1) {
      for (let x = x0; x < x1; x += 1) masked[y * width + x] = 1;
    }
  }
  let compared = 0;
  let changed = 0;
  const out = Buffer.alloc(total * 4);
  for (let index = 0; index < total; index += 1) {
    const offset = index * channels;
    if (masked[index]) {
      // Teal marks "not compared" so it cannot be mistaken for "unchanged".
      out[index * 4] = 14;
      out[index * 4 + 1] = 74;
      out[index * 4 + 2] = 96;
      out[index * 4 + 3] = 255;
      continue;
    }
    compared += 1;
    let different = before.channels !== channels;
    if (!different) {
      for (let channel = 0; channel < channels; channel += 1) {
        if (before.pixels[offset + channel] !== after.pixels[offset + channel]) {
          different = true;
          break;
        }
      }
    }
    if (different) {
      changed += 1;
      out[index * 4] = 255;
      out[index * 4 + 1] = 0;
      out[index * 4 + 2] = 90;
      out[index * 4 + 3] = 255;
    } else {
      const luma = Math.round((after.pixels[offset] * 0.299 + after.pixels[offset + 1] * 0.587 + after.pixels[offset + 2] * 0.114) * 0.22 + 34);
      out[index * 4] = luma;
      out[index * 4 + 1] = luma;
      out[index * 4 + 2] = luma;
      out[index * 4 + 3] = 255;
    }
  }
  return { changed, compared, masked: total - compared, total, percent: compared === 0 ? 0 : (changed / compared) * 100, diff: encodePng({ width, height, pixels: out }), dimensionChange: false };
}

function readPngSize(buffer) {
  if (buffer.length < 24) return { width: 0, height: 0 };
  return { width: buffer.readUInt32BE(16), height: buffer.readUInt32BE(20) };
}

// ---------------------------------------------------------------------------------------------
// Static source checks
// ---------------------------------------------------------------------------------------------

const stripCssComments = text => text.replace(/\/\*[\s\S]*?\*\//g, ' ');
// Block comments first, then line comments that are not the "//" of a URL scheme.
const stripJsComments = text => text.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:\\])\/\/[^\n]*/g, '$1');
const stripHtmlComments = text => text.replace(/<!--[\s\S]*?-->/g, ' ');

function findExternalUrls(label, text) {
  const hits = [];
  for (const match of text.matchAll(/https?:\/\/[^\s"'`()<>\\]+/g)) {
    hits.push(`${label}:${text.slice(0, match.index).split('\n').length} ${match[0]}`);
  }
  return hits;
}

function findPattern(label, text, pattern, what) {
  const hits = [];
  for (const match of text.matchAll(pattern)) {
    hits.push(`${label}:${text.slice(0, match.index).split('\n').length} ${what}`);
  }
  return hits;
}

// ---------------------------------------------------------------------------------------------
// Baseline
// ---------------------------------------------------------------------------------------------

const relative = path => (path.startsWith(ROOT) ? path.slice(ROOT.length + 1).replaceAll('\\', '/') : path);

async function readBaselineMeta() {
  try {
    return JSON.parse(await readFile(join(BASELINE_DIR, 'baseline.json'), 'utf8'));
  } catch {
    return null;
  }
}

async function writeBaselineMeta(patch) {
  const previous = await readBaselineMeta();
  const merged = {
    generatedAt: new Date().toISOString(),
    chrome: patch.chrome,
    viewports: VIEWPORTS.map(viewport => viewport.name),
    tolerancePercent: patch.tolerance,
    masks: MASK_SELECTORS,
    artifacts: { ...previous?.artifacts, ...patch.artifacts },
  };
  await mkdir(BASELINE_DIR, { recursive: true });
  await writeFile(join(BASELINE_DIR, 'baseline.json'), `${JSON.stringify(merged, null, 2)}\n`);
}

async function compareOrUpdate(state, captured, options, chromeVersion) {
  const basePath = join(BASELINE_DIR, `${state}.png`);
  const shotPath = join(SHOTS_DIR, `${state}.png`);
  await writeFile(shotPath, captured.shot);
  const hash = sha256(captured.shot);
  const size = readPngSize(captured.shot);
  const summary = `${size.width}x${size.height} ${(captured.shot.length / 1024).toFixed(1)}KB${captured.masks.length ? ` ${captured.masks.length} masked` : ''}`;
  detail(`${shotPath} sha256=${hash.slice(0, 16)}â€¦ ${summary}`);

  if (options.update) {
    await mkdir(BASELINE_DIR, { recursive: true });
    await writeFile(basePath, captured.shot);
    await writeBaselineMeta({
      chrome: chromeVersion,
      tolerance: options.tolerance,
      artifacts: { [state]: { file: `${state}.png`, sha256: hash, bytes: captured.shot.length, width: size.width, height: size.height, maskedRegions: captured.masks } },
    });
    return `baseline updated  ${relative(basePath)}  ${summary}`;
  }

  let baseline;
  try {
    baseline = await readFile(basePath);
  } catch {
    fail(`no baseline for "${state}": run "node scripts/ui-check.mjs --update" (${relative(basePath)})`);
  }
  if (sha256(baseline) === hash) return `pixel-identical  ${relative(basePath)}  ${summary}`;

  const before = decodePng(baseline);
  const after = decodePng(captured.shot);
  if (!before || !after) fail(`screenshot changed (hash differs) and this PNG variant cannot be diffed: ${relative(shotPath)}`);
  const diff = diffPng(before, after, captured.masks);
  if (diff.dimensionChange) fail(`screenshot size changed ${before.width}x${before.height} -> ${after.width}x${after.height}`);
  if (diff.changed === 0) return `identical outside ${captured.masks.length} masked region(s) (${diff.masked} px excluded)`;

  await writeFile(join(SHOTS_DIR, `${state}.diff.png`), diff.diff);
  SHOT_FILES[`${state} diff`] = join(SHOTS_DIR, `${state}.diff.png`);
  const note = `${diff.changed}/${diff.compared} px (${diff.percent.toFixed(2)}%) different  ${relative(join(SHOTS_DIR, `${state}.diff.png`))}`;
  if (diff.percent > options.tolerance) fail(`baseline mismatch: ${note}  tolerance ${options.tolerance}%  shot ${relative(shotPath)}`);
  return `changed within tolerance: ${note}`;
}

// ---------------------------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------------------------

async function capture(cdp) {
  const response = await cdp.send('Page.captureScreenshot', { format: 'png', fromSurface: true, captureBeyondViewport: false });
  return Buffer.from(response.data, 'base64');
}

/** Screenshot plus the boxes that must be ignored when diffing (see MASK_SELECTORS). */
async function captureState(cdp) {
  await settleFrames(cdp);
  const shot = await capture(cdp);
  const box = `element => {
    if (!element) return [];
    const rect = element.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return [];
    return [{ x: Math.floor(rect.left) - 2, y: Math.floor(rect.top) - 2, width: Math.ceil(rect.width) + 4, height: Math.ceil(rect.height) + 4 }];
  }`;
  let masks = [];
  try {
    masks = await evaluate(cdp, `(() => {
      const box = ${box};
      const direct = ${JSON.stringify(MASK_SELECTORS)}.flatMap(selector => box(document.querySelector(selector)));
      const rows = ${JSON.stringify(MASK_CHILD_SELECTORS)}.flatMap(selector => {
        const root = document.querySelector(selector);
        return root ? [...root.children].flatMap(child => box(child)) : [];
      });
      return [...direct, ...rows];
    })()`);
  } catch {
    masks = [];
  }
  return { shot, masks: Array.isArray(masks) ? masks : [] };
}

// ---------------------------------------------------------------------------------------------
// Server-rewrite detection: the old server had no `/api/health`, no CSP, no ETag and answered a
// second `POST /api/prompt` with 200. Those checks are gated so a stale binary degrades to SKIP.
// ---------------------------------------------------------------------------------------------

async function detectServer(baseUrl) {
  try {
    const response = await request(`${baseUrl}/api/health`, { timeout: 5000 });
    if (response.status !== 200) return { rewritten: false, reason: `GET /api/health returned ${response.status}` };
    const payload = jsonBody(response, '/api/health');
    if (payload?.ok !== true) return { rewritten: false, reason: '/api/health did not report ok' };
    return { rewritten: true, health: payload };
  } catch (error) {
    return { rewritten: false, reason: error instanceof Error ? error.message : String(error) };
  }
}

const REBUILD_HINT = 'binary predates the rewritten server: run "cargo build --bin goon" and re-run';

// ---------------------------------------------------------------------------------------------
// One viewport pass: intro â†’ idle â†’ transcript
// ---------------------------------------------------------------------------------------------

async function runStates(context) {
  const { cdp, baseUrl, viewport, suffix, options, native, network, shots } = context;
  const label = viewport.name;

  await cdp.send('Emulation.setDeviceMetricsOverride', { width: viewport.width, height: viewport.height, deviceScaleFactor: 1, mobile: false });
  const loaded = cdp.waitEvent('Page.loadEventFired', options.timeout);
  const navigatedAt = Date.now();
  await cdp.send('Page.navigate', { url: `${baseUrl}/?ui-check=${encodeURIComponent(label)}` });
  await loaded;

  // The freeze stylesheet must actually apply, or every screenshot catches a random animation
  // frame. The rewritten server sends `style-src 'self'`, so the CSP-safe path is a constructable
  // stylesheet; only if that failed do we drop CSP for this page and reload.
  let frozen = await freezeApplied(cdp);
  if (!frozen) {
    await cdp.send('Page.setBypassCSP', { enabled: true });
    const reloaded = cdp.waitEvent('Page.loadEventFired', options.timeout);
    await cdp.send('Page.reload', { ignoreCache: false });
    await reloaded;
    frozen = await freezeApplied(cdp);
  }
  await check(`freeze stylesheet applies (${label})`, () => {
    if (!frozen) fail('neither adoptedStyleSheets nor an inline <style> took effect: screenshots would not be reproducible');
    return 'animations and caret frozen';
  });

  await check(`intro state: #intro on screen, #shell hidden (${label})`, async () => {
    const snapshot = await evaluate(cdp, `(() => {
      const shell = document.getElementById('shell');
      return { intro: Boolean(document.getElementById('intro')), shellHidden: Boolean(shell) && shell.classList.contains('is-hidden') };
    })()`);
    if (options.shots) {
      const captured = await captureState(cdp);
      shots[`intro${suffix}`] = captured;
      SHOT_FILES[`intro ${label}`] = join(SHOTS_DIR, `intro${suffix}.png`);
    }
    const at = Date.now() - navigatedAt;
    detail(`captured ${at}ms after navigation`);
    if (!snapshot.intro) fail(`#intro was gone ${at}ms after load: it auto-dismisses at ~1900ms and this pass was too slow`);
    if (!snapshot.shellHidden) fail('#shell was not hidden while the intro runs');
    return `#intro present, #shell.is-hidden, shot ${at}ms after load`;
  });

  await check(`intro auto-dismisses into #shell.is-in (${label})`, async () => {
    const { elapsed } = await waitForExpression(cdp, "Boolean(document.getElementById('shell')?.classList.contains('is-in')) && !document.getElementById('intro')", options.timeout, '#shell.is-in');
    return `#shell.is-in after ${elapsed}ms, #intro removed`;
  });

  // Both shell fetches land after the intro; wait for the network to go quiet so the idle capture is
  // not caught mid-paint.
  await waitForQuiet(network, 600, options.timeout);

  await check(`idle DOM contract: #transcript, #prompt, #send (${label})`, async () => {
    const snapshot = await evaluate(cdp, `(() => ({
      transcript: Boolean(document.getElementById('transcript')),
      prompt: Boolean(document.getElementById('prompt')),
      send: Boolean(document.getElementById('send')),
      title: document.title,
    }))()`);
    for (const id of ['transcript', 'prompt', 'send']) if (!snapshot[id]) fail(`#${id} is missing from the live document`);
    detail(`title="${snapshot.title}"`);
    return `title "${snapshot.title}"`;
  });

  await check(`idle layout: no overflow, composer inside the viewport (${label})`, async () => {
    const snapshot = await evaluate(cdp, `(() => {
      const box = document.getElementById('prompt')?.getBoundingClientRect();
      const send = document.getElementById('send')?.getBoundingClientRect();
      return {
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        promptVisible: Boolean(box) && box.width > 0 && box.height > 0 && box.top >= 0 && box.bottom <= window.innerHeight + 1,
        sendVisible: Boolean(send) && send.width > 0 && send.right <= window.innerWidth + 1,
      };
    })()`);
    if (options.shots) {
      shots[`idle${suffix}`] = await captureState(cdp);
      SHOT_FILES[`idle ${label}`] = join(SHOTS_DIR, `idle${suffix}.png`);
    }
    if (snapshot.scrollWidth > snapshot.innerWidth + 1) fail(`document overflows horizontally: scrollWidth ${snapshot.scrollWidth} > viewport ${snapshot.innerWidth}`);
    if (!snapshot.promptVisible) fail('#prompt is not fully inside the viewport');
    if (!snapshot.sendVisible) fail('#send is not inside the viewport');
    return `no h-overflow at ${label}`;
  });

  if (!native) {
    skip(`transcript state: stub run renders into #transcript (${label})`, 'npm shim cannot spawn child runs (goon web uses current_exe)');
    return;
  }

  await check(`transcript state: stub run renders into #transcript (${label})`, async () => {
    await evaluate(cdp, `(() => {
      const prompt = document.getElementById('prompt');
      prompt.value = 'ui-check stub run';
      prompt.dispatchEvent(new Event('input', { bubbles: true }));
      const form = document.getElementById('form');
      if (form && typeof form.requestSubmit === 'function') form.requestSubmit();
      else document.getElementById('send').click();
      return true;
    })()`);
    await waitForExpression(cdp, `document.body.innerText.includes(${JSON.stringify(STUB_MARKER)})`, options.timeout * 2, 'the stub reply in the transcript');
    // The SSE request closes when the child exits; wait for it so the shot is not mid-stream.
    await waitForQuiet(network, 500, options.timeout);
    const snapshot = await evaluate(cdp, `(() => ({
      text: document.getElementById('transcript').innerText,
      sendDisabled: Boolean(document.getElementById('send')?.disabled),
      status: document.getElementById('status')?.dataset?.state ?? '',
    }))()`);
    if (options.shots) {
      shots[`transcript${suffix}`] = await captureState(cdp);
      SHOT_FILES[`transcript ${label}`] = join(SHOTS_DIR, `transcript${suffix}.png`);
    }
    if (snapshot.status === 'running' || snapshot.sendDisabled) fail('the UI still believes a run is in progress after the stub turn finished');
    if (/\bexit\s+\d/.test(snapshot.text)) fail(`the transcript reports a failed child run: ${snapshot.text.slice(0, 200)}`);
    if (!snapshot.text.includes('ui-check stub run')) fail(`the user turn is missing from the transcript: ${snapshot.text.slice(0, 200)}`);
    detail(`transcript: ${snapshot.text.replaceAll('\n', ' | ').slice(0, 300)}`);
    return `marker + user turn rendered, status idle, ${snapshot.text.length} chars`;
  });
}

async function freezeApplied(cdp) {
  try {
    return await evaluate(cdp, `getComputedStyle(document.documentElement).getPropertyValue('--ui-check-freeze').trim() === '1'`);
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------------------------

async function main() {
  const options = parseArgs(process.argv.slice(2));
  verbose = options.verbose;
  if (options.help) {
    process.stdout.write(HELP);
    return 0;
  }

  const started = Date.now();
  const binary = await resolveBinary();
  const chromePath = await resolveChrome(options.chrome);
  const scratch = join(tmpdir(), `goon-ui-check-${process.pid}-${Date.now()}`);
  const env = {
    ...process.env,
    XDG_CONFIG_HOME: join(scratch, 'config'),
    XDG_DATA_HOME: join(scratch, 'data'),
    GOON_DATA_DIR: join(scratch, 'data'),
    // The web server spawns `goon --json --yes -p â€¦` per prompt and the child inherits this env,
    // so the transcript state runs the deterministic stub provider instead of a real model.
    GOON_STUB: '1',
    GOON_STUB_FIXTURE: FIXTURE,
    // `GET /api/sessions/:id/events` reads the session's NDJSON log, which the child only writes
    // when the event log is on. Enabling it here is what lets the run exercise that route.
    GOON_EVENT_LOG: '1',
    NO_COLOR: '1',
  };
  for (const key of ['GOON_API_KEY', 'GOON_BASE_URL', 'GOON_MODEL']) delete env[key];
  const cwd = join(scratch, 'workspace');
  await mkdir(cwd, { recursive: true });
  await mkdir(join(scratch, 'config'), { recursive: true });
  await mkdir(join(scratch, 'data'), { recursive: true });
  await mkdir(SHOTS_DIR, { recursive: true });

  process.stdout.write('goon desktop UI check\n');
  process.stdout.write(`  root     ${ROOT}\n`);
  process.stdout.write(`  binary   ${binary.mode === 'native' ? relative(binary.command) : `${binary.command} ${relative(binary.args[0])} (shim)`}\n`);
  process.stdout.write(`  chrome   ${chromePath ?? '(not found)'}\n`);
  process.stdout.write(`  mode     ${options.update ? 'update baseline' : 'compare to baseline'}${options.shots ? '' : ' (screenshots disabled)'}\n`);
  process.stdout.write(`  scratch  ${scratch}\n\n`);

  const port = options.port || (await freePort());
  const baseUrl = `http://127.0.0.1:${port}`;
  const serverLog = join(scratch, 'server');
  let server = null;
  let chrome = null;
  let cdp = null;
  let keepServer = false;
  // A parallel `cargo build` / `taskkill /IM goon.exe` can kill this server mid-run; report that as
  // one clear failure instead of a page of connection-refused assertions.
  let serverExit = null;
  const requireServer = where => {
    if (serverExit) fail(`the web server exited during the run (${serverExit}) at ${where}: another process may have killed it (rebuild/taskkill)`);
  };

  const cleanup = async ({ keepScratch }) => {
    if (cdp) {
      cdp.close();
      cdp = null;
    }
    if (chrome) {
      chrome.stop();
      await chrome.cleanup().catch(() => {});
      chrome = null;
    }
    if (server && !keepServer) {
      killTree(server.pid);
      server = null;
    }
    if (!keepScratch) await rm(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }).catch(() => {});
  };

  const onSignal = () => {
    process.stdout.write('\n  interrupted: cleaning up\n');
    process.removeListener('SIGINT', onSignal);
    process.removeListener('SIGTERM', onSignal);
    cleanup({ keepScratch: false }).finally(() => process.exit(130));
  };
  process.once('SIGINT', onSignal);
  process.once('SIGTERM', onSignal);

  try {
    // ---------------------------------------------------------------- server
    server = await spawnWithLogs(binary.command, [...binary.args, 'web', '--no-open', '--port', String(port)], { cwd, env, logPath: serverLog });
    server.on('exit', (code, signal) => {
      serverExit = signal ? `signal ${signal}` : `code ${code}`;
    });

    let meta = null;
    const readyDeadline = Date.now() + options.timeout;
    let lastError = 'no response yet';
    while (Date.now() < readyDeadline) {
      if (serverExit) {
        lastError = `server exited with ${serverExit}: ${(await readLog(`${serverLog}.err`)).trim().split('\n').slice(-3).join(' | ')}`;
        break;
      }
      try {
        const response = await request(`${baseUrl}/api/meta`, { timeout: 1500 });
        if (response.status === 200) {
          meta = JSON.parse(response.body.toString('utf8'));
          lastError = '';
          break;
        }
        lastError = `/api/meta returned ${response.status}`;
      } catch (error) {
        lastError = error instanceof Error ? error.message : String(error);
      }
      await delay(100);
    }

    let ready = false;
    await check('/api/meta answers 200 within timeout', () => {
      if (!meta) fail(`server not ready: ${lastError}`);
      ready = true;
      return `${baseUrl}  v${meta.version ?? '?'}  ${Date.now() - started}ms to ready`;
    });
    if (!ready) {
      process.stdout.write(`\n  server stderr tail:\n${indent((await readLog(`${serverLog}.err`)).trim().split('\n').slice(-10).join('\n'))}\n`);
      return finish();
    }

    await check('server metadata is well formed', () => {
      if (typeof meta.version !== 'string' || meta.version.length === 0) fail('meta.version is missing');
      if (typeof meta.cwd !== 'string' || !meta.cwd.includes('workspace')) fail(`meta.cwd is not the scratch workspace (got ${String(meta.cwd)})`);
      if (typeof meta.running !== 'number') fail('meta.running is not a number');
      detail(`meta=${JSON.stringify(meta)}`);
      return `version ${meta.version}`;
    });

    await check('server is isolated from real user state', () => {
      const expected = join(scratch, 'data', 'goon');
      if (typeof meta.dataDir !== 'string' || resolve(meta.dataDir).toLowerCase() !== resolve(expected).toLowerCase()) {
        fail(`meta.dataDir is ${String(meta.dataDir)}, expected ${expected}`);
      }
      return `dataDir=${relative(meta.dataDir)} (scratch)`;
    });

    // `goon web` serves assets baked into the binary (include_str!), so checking a stale build would
    // describe the previous UI. Say so loudly instead of silently reporting on old code.
    const stale = await staleSince(binary, ['src/web/ui.html', 'src/web/ui.css', 'src/web/ui.js', 'src/web/mod.rs', 'src/web/http.rs', 'src/web/api.rs', 'src/web/sse.rs']);
    if (stale.assets.length) warn('binary embeds a stale UI', `newer source: ${stale.assets.join(', ')} â€” run "cargo build --bin goon" first`);
    else record('PASS', 'binary is newer than src/web assets', `built ${stale.built}`);

    const serverSurface = await detectServer(baseUrl);
    if (serverSurface.rewritten) {
      record('PASS', 'server surface is the rewritten one (/api/health)', `uptime ${serverSurface.health.uptimeMs}ms, runs ${serverSurface.health.runs}`);
    } else {
      warn('server surface predates the rewrite', `${serverSurface.reason} â€” ${REBUILD_HINT}`);
    }

    // ---------------------------------------------------------------- HTTP contract
    let page = null;
    let css = null;
    let js = null;
    let pageHeaders = null;

    await check('GET /  200 text/html', async () => {
      const response = await requestOk(`${baseUrl}/`);
      if (!contentType(response).startsWith('text/html')) fail(`content-type is ${contentType(response) || '(missing)'}`);
      page = response.body.toString('utf8');
      pageHeaders = response.headers;
      detail(`bytes=${response.body.length}`);
      return `${response.body.length}B  ${contentType(response)}`;
    });

    await check('served document references /app.css and /app.js', () => {
      if (!page) fail('no document to scan');
      for (const asset of ['/app.css', '/app.js']) if (!page.includes(asset)) fail(`document does not reference ${asset}`);
      if (!page.includes('id="intro"')) fail('document has no #intro element');
      if (!page.includes('id="transcript"')) fail('document has no #transcript element');
      return 'stylesheet + script + intro markup present';
    });

    await check('document has no inline code or handlers (CSP-safe)', () => {
      if (!page) fail('no document to scan');
      const html = stripHtmlComments(page);
      const hits = [
        ...findPattern('ui.html', html, /<script(?![^>]*\bsrc=)[^>]*>/gi, '<script> without src'),
        ...findPattern('ui.html', html, /<style\b/gi, '<style> block'),
        ...findPattern('ui.html', html, /\sstyle\s*=\s*["']/gi, 'style="" attribute'),
        ...findPattern('ui.html', html, /\son[a-z]+\s*=\s*["']/gi, 'on*="" handler'),
      ];
      if (hits.length) fail(`${hits.length} inline construct(s): ${hits.slice(0, 5).join(', ')}`);
      return 'no inline script/style/handler';
    });

    await check('GET /app.css  200 text/css', async () => {
      const response = await requestOk(`${baseUrl}/app.css`);
      if (!contentType(response).startsWith('text/css')) fail(`content-type is ${contentType(response) || '(missing)'}`);
      css = response.body.toString('utf8');
      detail(`bytes=${response.body.length} etag=${String(response.headers.etag ?? '(none)')}`);
      return `${response.body.length}B  ${contentType(response)}`;
    });

    await check('GET /app.js  200 javascript', async () => {
      const response = await requestOk(`${baseUrl}/app.js`);
      if (!/javascript|ecmascript/.test(contentType(response))) fail(`content-type is ${contentType(response) || '(missing)'}`);
      js = response.body.toString('utf8');
      detail(`bytes=${response.body.length} etag=${String(response.headers.etag ?? '(none)')}`);
      return `${response.body.length}B  ${contentType(response)}`;
    });

    await check(`/app.css stays under ${SIZES.css / 1024}KB`, () => {
      if (!css) fail('stylesheet was not fetched');
      const bytes = Buffer.byteLength(css);
      if (bytes > SIZES.css) fail(`${bytes}B exceeds ${SIZES.css}B`);
      return `${bytes}B / ${SIZES.css}B (${((bytes / SIZES.css) * 100).toFixed(0)}%)`;
    });

    await check(`/app.js stays under ${SIZES.js / 1024}KB`, () => {
      if (!js) fail('script was not fetched');
      const bytes = Buffer.byteLength(js);
      if (bytes > SIZES.js) fail(`${bytes}B exceeds ${SIZES.js}B`);
      return `${bytes}B / ${SIZES.js}B (${((bytes / SIZES.js) * 100).toFixed(0)}%)`;
    });

    await check('GET /api/sessions  200 json array', async () => {
      const response = await requestOk(`${baseUrl}/api/sessions`);
      if (!contentType(response).includes('application/json')) fail(`content-type is ${contentType(response) || '(missing)'}`);
      const parsed = jsonBody(response, '/api/sessions');
      if (!Array.isArray(parsed)) fail(`body is not an array: ${response.body.toString('utf8').slice(0, 120)}`);
      return `${parsed.length} session(s)`;
    });

    await check('unknown route answers 404', async () => {
      const response = await request(`${baseUrl}/__ui_check_missing__`);
      if (response.status !== 404) fail(`expected 404, got ${response.status}`);
      return '404';
    });

    await check('no external URL in served html/css/js (comments stripped)', () => {
      const hits = [
        ...findExternalUrls('ui.html', stripHtmlComments(page ?? '')),
        ...findExternalUrls('app.css', stripCssComments(css ?? '')),
        ...findExternalUrls('app.js', stripJsComments(js ?? '')),
      ];
      if (hits.length) fail(`${hits.length} external reference(s): ${hits.slice(0, 5).join(', ')}`);
      return 'html/css/js stay self-contained';
    });

    // ---------------------------------------------------------------- rewritten server surface
    if (!serverSurface.rewritten) {
      for (const name of ['CSP on the document', 'nosniff + no-referrer headers', 'asset ETag revalidation (304)', 'Host guard rejects a foreign Host', 'Origin guard rejects a foreign Origin', 'wrong verb on a known route is 405', '/api/config exposes no secrets', '/api/runs is a bare array', '/api/sessions/:id/events shape', 'second concurrent prompt is 409']) {
        skip(name, REBUILD_HINT);
      }
    } else {
      await check('CSP on the document', () => {
        const csp = String(pageHeaders?.['content-security-policy'] ?? '');
        if (!csp) fail('no content-security-policy header on /');
        for (const directive of ["default-src 'none'", "script-src 'self'", "style-src 'self'", "connect-src 'self'"]) {
          if (!csp.includes(directive)) fail(`CSP is missing ${directive}: ${csp}`);
        }
        if (/unsafe-inline|unsafe-eval/.test(csp)) fail(`CSP allows unsafe sources: ${csp}`);
        return csp;
      });

      await check('nosniff + no-referrer headers', async () => {
        const metaResponse = await requestOk(`${baseUrl}/api/meta`);
        const missing = [];
        for (const [label, headers] of [['GET /', pageHeaders], ['GET /api/meta', metaResponse.headers]]) {
          if (String(headers?.['x-content-type-options'] ?? '') !== 'nosniff') missing.push(`${label} x-content-type-options`);
          if (String(headers?.['referrer-policy'] ?? '') !== 'no-referrer') missing.push(`${label} referrer-policy`);
        }
        if (missing.length) fail(`missing/incorrect: ${missing.join(', ')}`);
        return 'x-content-type-options: nosniff, referrer-policy: no-referrer on every response';
      });

      await check('asset ETag revalidation (304)', async () => {
        for (const asset of ['/app.css', '/app.js']) {
          const first = await requestOk(`${baseUrl}${asset}`);
          const etag = String(first.headers.etag ?? '');
          const cacheControl = String(first.headers['cache-control'] ?? '');
          if (!etag) fail(`${asset} has no etag`);
          if (!cacheControl.includes('no-cache')) fail(`${asset} cache-control is "${cacheControl}" (expected no-cache)`);
          const second = await request(`${baseUrl}${asset}`, { headers: { 'if-none-match': etag } });
          if (second.status !== 304) fail(`${asset} returned ${second.status} for If-None-Match (expected 304)`);
          if (second.body.length !== 0) fail(`${asset} 304 carried a ${second.body.length}B body`);
          const wrong = await request(`${baseUrl}${asset}`, { headers: { 'if-none-match': '"stale-tag"' } });
          if (wrong.status !== 200) fail(`${asset} returned ${wrong.status} for an unrelated ETag (expected 200)`);
        }
        return 'etag + cache-control: no-cache, 304 only on a matching tag';
      });

      await check('Host guard rejects a foreign Host', async () => {
        const foreign = await request(`${baseUrl}/api/meta`, { headers: { host: 'rebind.example' } });
        if (foreign.status !== 403) fail(`foreign Host got ${foreign.status} (expected 403)`);
        const localhost = await request(`${baseUrl}/api/meta`, { headers: { host: `localhost:${port}` } });
        if (localhost.status !== 200) fail(`Host localhost:${port} got ${localhost.status} (expected 200)`);
        return '403 for a rebound hostname, 200 for localhost';
      });

      await check('Origin guard rejects a foreign Origin', async () => {
        const body = JSON.stringify({ prompt: 'ui-check origin guard' });
        const headers = { 'content-type': 'application/json', origin: 'https://evil.example' };
        const foreign = await request(`${baseUrl}/api/prompt`, { method: 'POST', body, headers });
        if (foreign.status !== 403) fail(`foreign Origin got ${foreign.status} (expected 403)`);
        const after = await requestOk(`${baseUrl}/api/meta`);
        const running = jsonBody(after, '/api/meta').running;
        if (running !== 0) fail(`the rejected request started a run (running=${running})`);
        return '403 for a cross-site Origin, no run started';
      });

      await check('wrong verb on a known route is 405', async () => {
        const response = await request(`${baseUrl}/app.css`, { method: 'DELETE' });
        if (response.status !== 405) fail(`DELETE /app.css got ${response.status} (expected 405)`);
        return '405 + allow header';
      });

      await check('/api/config exposes no secrets', async () => {
        const response = await requestOk(`${baseUrl}/api/config`);
        const payload = jsonBody(response, '/api/config');
        for (const key of ['provider', 'cwd', 'dataDir', 'readOnly']) if (!(key in payload)) fail(`/api/config is missing ${key}`);
        for (const key of Object.keys(payload)) {
          if (/key|token|secret|base.?url|bearer/i.test(key)) fail(`/api/config exposes "${key}"`);
        }
        if (/sk-|apikey|bearer /i.test(response.body.toString('utf8'))) fail('/api/config body looks like it carries a credential');
        return `${Object.keys(payload).length} fields, no credential-shaped key`;
      });

      await check('/api/runs is a bare array', async () => {
        const response = await requestOk(`${baseUrl}/api/runs`);
        const payload = jsonBody(response, '/api/runs');
        if (!Array.isArray(payload)) fail(`body is not an array: ${response.body.toString('utf8').slice(0, 120)}`);
        return `${payload.length} run(s)`;
      });
    }

    // ---------------------------------------------------------------- browser
    if (!chromePath) {
      record('FAIL', 'chrome available', 'not found: install Chrome, set CHROME_PATH, or pass --chrome <path>');
      for (const viewport of VIEWPORTS) for (const name of ['intro state', 'idle state', 'transcript state']) skip(`${name} (${viewport.name})`, 'needs Chrome');
      return finish();
    }
    record('PASS', 'chrome available', chromePath);

    chrome = await startChrome(chromePath, options.timeout, join(scratch, 'chrome'));
    detail(`chrome ${chrome.version}`);
    cdp = await Cdp.connect(chrome.wsUrl, options.timeout);

    const consoleErrors = [];
    const pageErrors = [];
    const network = { requests: [], urls: new Map(), inflight: new Map(), failed: [], lastAt: Date.now() };
    const ignorable = url => /\/(favicon\.ico|__ui_check_missing__)/.test(url);

    cdp.on('Runtime.consoleAPICalled', params => {
      if (params.type !== 'error' && params.type !== 'assert') return;
      const text = (params.args ?? []).map(arg => arg.value ?? arg.description ?? arg.type).join(' ');
      if (ignorable(text)) return;
      consoleErrors.push(`console.${params.type}: ${text}`);
    });
    cdp.on('Runtime.exceptionThrown', params => {
      const description = params.exceptionDetails?.exception?.description ?? params.exceptionDetails?.text ?? 'exception';
      pageErrors.push(description.split('\n')[0]);
    });
    cdp.on('Log.entryAdded', params => {
      if (params.entry?.level !== 'error') return;
      const text = params.entry.text ?? '';
      if (ignorable(text) || ignorable(params.entry.url ?? '')) return;
      consoleErrors.push(`log: ${text}`);
    });
    cdp.on('Network.requestWillBeSent', params => {
      network.requests.push(params.request?.url ?? '');
      network.urls.set(params.requestId, params.request?.url ?? '');
      network.inflight.set(params.requestId, Date.now());
      network.lastAt = Date.now();
    });
    const settleRequest = params => {
      network.inflight.delete(params.requestId);
      network.lastAt = Date.now();
    };
    cdp.on('Network.loadingFinished', settleRequest);
    cdp.on('Network.loadingFailed', params => {
      network.failed.push(`${network.urls.get(params.requestId) ?? params.requestId} ${params.errorText ?? ''}`.trim());
      settleRequest(params);
    });

    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    await cdp.send('Log.enable');
    await cdp.send('Network.enable');
    await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: FREEZE_SCRIPT });

    requireServer('before the browser pass');
    const shots = {};
    for (const viewport of VIEWPORTS) {
      await runStates({ cdp, baseUrl, viewport, suffix: viewport.suffix, options, native: binary.mode === 'native', network, shots });
    }

    // ---------------------------------------------------------------- runtime hygiene
    await check('no console errors or uncaught exceptions', () => {
      const all = [...pageErrors, ...consoleErrors];
      if (all.length) fail(`${all.length}: ${all.slice(0, 4).join(' ; ')}`);
      return 'console clean';
    });

    await check('no failed network requests', () => {
      const real = network.failed.filter(entry => !/favicon\.ico|__ui_check_missing__/.test(entry));
      if (real.length) fail(`${real.length}: ${real.slice(0, 4).join(' ; ')}`);
      return `${network.requests.length} request(s), none failed`;
    });

    await check('no third-party network requests', () => {
      const origin = new URL(baseUrl).origin;
      const external = [...new Set(network.requests)].filter(url => !url.startsWith(origin) && !url.startsWith('data:') && !url.startsWith('blob:') && !url.endsWith('/favicon.ico'));
      if (external.length) fail(`${external.length}: ${external.slice(0, 5).join(', ')}`);
      detail(`requests: ${[...new Set(network.requests)].map(url => url.replace(baseUrl, '')).join(' ')}`);
      return `${new Set(network.requests).size} unique URL(s), all same-origin`;
    });

    // ---------------------------------------------------------------- API behaviour after runs
    if (!serverSurface.rewritten) {
      skip('/api/sessions/:id/events returns a session log', REBUILD_HINT);
      skip('second concurrent prompt is 409', REBUILD_HINT);
    } else {
      await check('/api/sessions/:id/events returns a session log', async () => {
        requireServer('/api/sessions/:id/events');
        const list = jsonBody(await requestOk(`${baseUrl}/api/sessions`), '/api/sessions');
        const session = Array.isArray(list) ? list.find(entry => typeof entry?.id === 'string' && entry.id) : null;
        if (!session) fail('no session was recorded, so the event log route cannot be checked');
        const response = await requestOk(`${baseUrl}/api/sessions/${session.id}/events?limit=5`);
        const payload = jsonBody(response, 'events');
        if (payload.id !== session.id) fail(`payload id ${payload.id} does not match ${session.id}`);
        if (!Array.isArray(payload.events)) fail('events is not an array');
        if (typeof payload.truncated !== 'boolean') fail('truncated is not a boolean');
        if (payload.events.length > 5) fail(`limit=5 returned ${payload.events.length} events`);
        const bad = await request(`${baseUrl}/api/sessions/..%2F..%2Fetc/events`);
        if (bad.status !== 400) fail(`a traversal-shaped session id got ${bad.status} (expected 400)`);
        const missing = await request(`${baseUrl}/api/sessions/deadbeef/events`);
        if (missing.status !== 404) fail(`an unknown session id got ${missing.status} (expected 404)`);
        return `${payload.events.length} event(s), traversal 400, unknown 404`;
      });

      await check('second concurrent prompt is 409', async () => {
        requireServer('concurrent prompt');
        const url = `${baseUrl}/api/prompt`;
        const headers = { 'content-type': 'application/json' };
        const first = await requestStream(url, { method: 'POST', body: JSON.stringify({ prompt: 'ui-check concurrency keep-alive' }), headers });
        try {
          if (first.status !== 200) fail(`the first prompt returned ${first.status}`);
          // The response head means the server already incremented its run counter.
          const second = await request(url, { method: 'POST', body: JSON.stringify({ prompt: 'ui-check concurrency second' }), headers });
          if (second.status !== 409) fail(`the second concurrent prompt got ${second.status} (expected 409)`);
          jsonBody(second, '409 body');
        } finally {
          first.close();
          await first.done.catch(() => {});
        }
        return 'first run streamed, second answered 409';
      });
    }

    // ---------------------------------------------------------------- baselines
    if (!options.shots) {
      skip('baseline comparison', 'screenshots disabled (--no-shots)');
    } else if (serverExit) {
      // A killed server leaves Chrome error pages behind; writing those as a baseline (or diffing
      // against them) would be worse than useless.
      skip('baseline comparison', `screenshots are not usable: the web server exited during the run (${serverExit})`);
    } else {
      for (const [state, captured] of Object.entries(shots)) {
        await check(`baseline ${state}`, () => compareOrUpdate(state, captured, options, chrome.version));
      }
      const baselineMeta = await readBaselineMeta();
      if (baselineMeta?.chrome && chrome.version && baselineMeta.chrome !== chrome.version) {
        warn('baseline chrome version', `${baselineMeta.chrome} -> ${chrome.version} (review the diff, then --update)`);
      }
      if (baselineMeta?.viewports && baselineMeta.viewports.join(',') !== VIEWPORTS.map(viewport => viewport.name).join(',')) {
        warn('baseline viewports', `${baselineMeta.viewports.join(',')} -> ${VIEWPORTS.map(viewport => viewport.name).join(',')}`);
      }
    }
  } catch (error) {
    record('FAIL', 'harness', error instanceof Error ? error.message : String(error));
  } finally {
    if (options.keep && server && !serverExit) {
      keepServer = true;
      process.stdout.write(`\n  --keep: server left running at ${baseUrl}/ (pid ${server.pid}, logs in ${serverLog}.err)\n`);
    } else if (options.keep) {
      process.stdout.write(`\n  --keep: nothing to keep, the server already exited (${serverExit})\n`);
    }
    if (options.keep) process.stdout.write(`  scratch dir: ${scratch} (server logs: ${serverLog}.err)\n`);
    await cleanup({ keepScratch: options.keep });
  }

  return finish();
}

const FREEZE_SCRIPT = `(() => {
  const css = ${JSON.stringify(FREEZE_CSS)};
  // CSP-safe path: a constructable stylesheet is not an inline style and is not blocked by
  // style-src 'self'. Verified afterwards through the --ui-check-freeze marker property.
  try {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(css);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
    return;
  } catch (error) {
    // Fall through to an inline <style>: only works when the page has no CSP (older server).
  }
  const install = () => {
    if (!document.documentElement) return false;
    if (document.getElementById('ui-check-freeze')) return true;
    const style = document.createElement('style');
    style.id = 'ui-check-freeze';
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);
    return true;
  };
  if (install()) return;
  const observer = new MutationObserver(() => { if (install()) observer.disconnect(); });
  observer.observe(document, { childList: true, subtree: true });
})();`;

function indent(text) {
  return text.split('\n').map(line => `    ${line}`).join('\n');
}

function finish() {
  const failed = results.filter(entry => entry.status === 'FAIL');
  const passed = results.filter(entry => entry.status === 'PASS');
  const skipped = results.filter(entry => entry.status === 'SKIP');
  const warned = results.filter(entry => entry.status === 'WARN');
  process.stdout.write('\n  artifacts\n');
  const shots = Object.entries(SHOT_FILES);
  if (shots.length === 0) {
    process.stdout.write('    (none: screenshots disabled or not captured)\n');
  } else {
    const width = Math.max(...shots.map(([label]) => label.length));
    for (const [label, path] of shots) process.stdout.write(`    ${label.padEnd(width)}  ${relative(path)}\n`);
  }
  process.stdout.write(`  summary: ${passed.length} passed, ${failed.length} failed, ${warned.length} warning(s), ${skipped.length} skipped\n`);
  if (failed.length) {
    process.stdout.write(`  FAILED: ${failed.map(entry => entry.name).join(', ')}\n`);
    process.stdout.write('  exit 1\n');
    return 1;
  }
  process.stdout.write('  exit 0\n');
  return 0;
}

try {
  process.exit(await main());
} catch (error) {
  process.stdout.write(`\n  harness failed: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
  process.exit(1);
}
