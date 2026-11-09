import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

import { fixture, stubCli } from './helpers.mjs';

// The evolution subsystem is opt-in and must stay inert without an explicit subcommand.
// These tests drive the real CLI; no network and no model calls are involved.

async function evolve(f, args) {
  return stubCli(f, args).done;
}

test('evolve init seeds the editable bundle and an eval set', async t => {
  const f = await fixture(t);
  const result = await evolve(f, ['evolve', 'init']);
  assert.equal(result.code, 0, result.stderr);

  const bundle = join(f.cwd, '.goon', 'harness');
  const files = (await readdir(bundle, { recursive: true })).map(path => path.split('\\').join('/'));
  for (const required of ['system.md', 'tools/guidance.md', 'middleware.md', 'memory.md', 'policy/context.json', 'policy/verify.json', 'policy/jit.json']) {
    assert.ok(files.includes(required), `missing component ${required} in ${files.join(', ')}`);
  }
  assert.ok(files.some(path => path.startsWith('skills/')), 'seed bundle carries at least one skill');

  const tasks = JSON.parse(await readFile(join(f.cwd, 'eval', 'tasks.json'), 'utf8'));
  assert.ok(Array.isArray(tasks.tasks) && tasks.tasks.length > 0, 'starter eval set is runnable');
});

test('evolve status reports components and bundles as JSON', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);

  const result = await stubCli(f, ['evolve', 'status', '--json']).done;
  assert.equal(result.code, 0, result.stderr);
  const status = JSON.parse(result.stdout.trim());
  assert.equal(status.total_components, 11, `expected the full seed bundle, got ${status.total_components}`);
  assert.ok(status.latest_bundle, 'a bundle snapshot exists after init');
  assert.equal(status.rounds, 0);
  assert.equal(status.solved, 0);
});

test('evolve run refuses to accept an unmeasured round', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);
  const before = await readFile(join(f.cwd, '.goon', 'harness', 'middleware.md'), 'utf8');

  // --dry-run evaluates nothing, so the gate must reject instead of durably changing the harness.
  const result = await evolve(f, ['evolve', 'run', '--dry-run']);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /REJECTED/i, 'dry round must not be accepted');
  assert.match(result.stdout, /no tasks measured/i);

  const after = await readFile(join(f.cwd, '.goon', 'harness', 'middleware.md'), 'utf8');
  assert.equal(after, before, 'a rejected round leaves the workspace bundle untouched');

  const log = await stubCli(f, ['evolve', 'log', '--json']).done;
  const manifests = JSON.parse(log.stdout.trim());
  assert.equal(manifests.length, 1);
  assert.equal(manifests[0].accepted, false);
  // Decision observability: the edit is still recorded with a prediction for the next round.
  assert.ok(manifests[0].entries.length >= 1);
  assert.ok(manifests[0].entries[0].prediction, 'manifest entries carry predictions');
});

test('evolve jit plans a harness for one task without a model call', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);

  const result = await stubCli(f, ['evolve', 'jit', '-p', 'fix the failing unit test in src/lib.rs', '--json']).done;
  assert.equal(result.code, 0, result.stderr);
  const payload = JSON.parse(result.stdout.trim());
  assert.equal(payload.decision.variant, 'code');
  assert.ok(payload.decision.profile, 'a profile is selected');
  assert.equal(payload.max_repairs, 2, 'repair budget matches the paper bound');
});

test('evolve revert restores the bundle a round started from', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);
  assert.equal((await evolve(f, ['evolve', 'run', '--dry-run'])).code, 0);

  const failed = await evolve(f, ['evolve', 'revert', '1']);
  assert.equal(failed.code, 0, failed.stderr);
  assert.match(failed.stdout, /Restored bundle/);
});

test('a bundle can be applied to a normal session and is reported as an event', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);
  const status = JSON.parse((await stubCli(f, ['evolve', 'status', '--json']).done).stdout.trim());

  // Stub mode keeps this offline: the point is that the bundle reaches the harness and is traced.
  const run = await stubCli(f, ['--bundle', status.latest_bundle, '--event-log', '-p', 'say hello']).done;
  assert.equal(run.code, 0, run.stderr);

  const sessionDir = join(f.data, 'goon', 'sessions');
  const logs = (await readdir(sessionDir)).filter(name => name.endsWith('.events.jsonl'));
  assert.ok(logs.length > 0, 'event log written');
  const body = await readFile(join(sessionDir, logs[0]), 'utf8');
  const events = body.trim().split('\n').map(line => JSON.parse(line));
  const harness = events.find(event => event.type === 'harness');
  assert.ok(harness, `expected a harness event, saw ${events.map(event => event.type).join(',')}`);
  assert.equal(harness.event.bundle, status.latest_bundle);
});

test('digest writes a corpus even when no run failed', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);
  const result = await evolve(f, ['evolve', 'digest', '--json']);
  assert.equal(result.code, 0, result.stderr);
  const corpus = JSON.parse(result.stdout.trim());
  assert.equal(corpus.sessions, 0);
  assert.deepEqual(corpus.items, []);
  const overview = await readFile(join(f.data, 'goon', 'harness', 'digest', 'analysis', 'overview.md'), 'utf8');
  assert.match(overview, /Experience digest/);
});

test('editing a component changes the snapshot id', async t => {
  const f = await fixture(t);
  assert.equal((await evolve(f, ['evolve', 'init'])).code, 0);
  const first = JSON.parse((await stubCli(f, ['evolve', 'status', '--json']).done).stdout.trim()).latest_bundle;

  const middleware = join(f.cwd, '.goon', 'harness', 'middleware.md');
  await writeFile(middleware, `${await readFile(middleware, 'utf8')}\n## manual rule\n- Prefer the smallest diff.\n`);

  const reseeded = await evolve(f, ['evolve', 'init']);
  assert.equal(reseeded.code, 0, reseeded.stderr);
  const second = JSON.parse((await stubCli(f, ['evolve', 'status', '--json']).done).stdout.trim()).latest_bundle;
  assert.notEqual(first, second, 'component observability: content change yields a new bundle id');
});
