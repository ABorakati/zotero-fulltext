// ZFT.runs: the run controller. Real plugin/content/runs.js (plus state.js for the sweep test) in a vm context; every
// service it wires together is a recording fake, so no wasm, no Zotero and no real engine are involved.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeFs, makeFakeTimers, makeFakeZotero } from './helpers/fakes.mjs';

/** Let pending microtasks and fake async work finish. */
async function settle() {
  for (let i = 0; i < 8; i++) await new Promise((resolve) => setImmediate(resolve));
}

const BASE_CONFIG = { oa: { openalex: true }, inst: { enabled: true }, run: { item_concurrency: 4 }, institution: { name: 'Test University' }, endpoint_map: [], trace: false };
const PERSISTED = { ledger: { committed: [[1, 2]], reserved: [] }, halts: { inst_until: 0, reason: null, hosts: {} }, ezproxy: {}, alma: {} };

/** A fake Runner that records everything the controller does to it. */
function makeRunner(rec) {
  const runner = {
    sent: [],                 // each send() argument (an array of commands)
    needsConfigure: true,
    disposed: false,
    quiet: null,              // set to a promise to hold whenQuiet() open
    sendThrows: null,
    send(commands) {
      rec('runner.send:' + commands.map((c) => c.type).join('+'));
      if (runner.sendThrows) throw runner.sendThrows;
      runner.sent.push(commands);
      if (commands.some((c) => c.type === 'configure')) runner.needsConfigure = false;
    },
    async pump() { rec('runner.pump'); },
    async whenQuiet() { rec('runner.whenQuiet'); if (runner.quiet) await runner.quiet; },
    async dispose() { rec('runner.dispose'); runner.disposed = true; },
    /** Commands of one type across all sends, in order. */
    commands(type) { return runner.sent.flat().filter((c) => c.type === type); },
  };
  return runner;
}

/**
 * Build a controller environment: fake collaborators, a loaded ZFT with state.js/items.js/runs.js, and a recorder
 * `calls` listing every collaborator call in order. `overrides()` returns the object for ZFT.runs.init().
 */
function env(opts = {}) {
  const calls = [];
  const rec = (s) => calls.push(s);
  const timers = makeFakeTimers();
  const zotero = makeFakeZotero();
  const runner = makeRunner(rec);
  let counter = opts.firstRunId ?? 0;
  const cfg = { value: JSON.parse(JSON.stringify(BASE_CONFIG)) };
  const created = { runnerDeps: null, runnerOpts: null, count: 0 };
  const watchers = [];      // deletion callbacks registered through items.watchDeletions
  const alerts = [];
  const progressOpened = [];
  const flags = { wasmFail: null, stateInitFail: null, traceOn: false, snapshotOverride: null };

  const parts = {
    state: {
      async init() { rec('state.init'); if (flags.stateInitFail) throw flags.stateInitFail; },
      async sweepTmp() { rec('state.sweepTmp'); return 0; },
      getPersisted() { return JSON.parse(JSON.stringify(PERSISTED)); },
      nextRunId() { counter += 1; return counter; },
      async flush() { rec('state.flush'); },
      async dispose() { rec('state.dispose'); },
    },
    items: {
      async snapshotAll(items) {
        rec('items.snapshotAll');
        if (flags.snapshotOverride) return flags.snapshotOverride;
        return items.map((it) => ({ item: it.id, key: '1/' + it.key, title: it.title || '', regular: true }));
      },
      watchDeletions(cb) {
        rec('items.watchDeletions');
        watchers.push(cb);
        return () => { rec('items.unwatch'); };
      },
    },
    config: {
      buildEngineConfig() { return JSON.parse(JSON.stringify(cfg.value)); },
      getPref(key) { return key === 'debug.trace' ? flags.traceOn : undefined; },
    },
    progress: {
      open(run, snaps, o) { rec('progress.open'); progressOpened.push({ run, snaps, o }); },
      closeAll() { rec('progress.closeAll'); },
    },
    login: { closeAll() { rec('login.closeAll'); } },
    pdfCapture: { disposeAll() { rec('pdfCapture.disposeAll'); } },
    net: { id: 'net' }, browser: { id: 'browser' }, translators: { id: 'translators' }, zoteroOa: { id: 'zoteroOa' },
    save: { id: 'save' }, confirm: { id: 'confirm' },
    wasmHost: {
      async get() { rec('wasmHost.get'); if (flags.wasmFail) throw flags.wasmFail; return { id: 'host' }; },
      async shutdown() { rec('wasmHost.shutdown'); },
    },
    bodies: { async releaseAll() { rec('bodies.releaseAll'); } },
    services: { prompt: { alert: (...a) => { rec('alert'); alerts.push(a); } } },
    timers: { setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout },
    createRunner(runnerDeps, runnerOpts) {
      rec('createRunner');
      created.runnerDeps = runnerDeps;
      created.runnerOpts = runnerOpts;
      created.count += 1;
      return runner;
    },
  };

  const { ZFT, ctx } = loadPlugin({
    globals: { Zotero: zotero, ...(opts.globals || {}) },
    files: ['util.js', 'state.js', 'items.js', 'runs.js'],
  });
  const overrides = (extra = {}) => ({ ...parts, ...extra });
  return { ZFT, ctx, R: ZFT.runs, calls, rec, runner, parts, cfg, created, watchers, alerts, progressOpened, flags, timers, zotero, overrides };
}

const item = (id) => ({ id, key: 'KEY' + id, title: 'Item ' + id });
const report = (run, extra = {}) => ({
  run, counts: { saved: 1, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: 0, failed: 0 },
  items: [], ledger: { hour: 0, hour_cap: 30, day: 0, day_cap: 80, mb_hour: 0, mb_hour_cap: 300 }, halted: null, engine_restarts: 0, ...extra,
});
const types = (commands) => plain(commands).map((c) => c.type);

// ------------------------------------------------------------------ init

test('init builds the controller in order: state, sweep, host, runner, then Configure', async () => {
  const e = env();
  await e.R.init(e.overrides());
  assert.deepEqual(e.calls, ['state.init', 'state.sweepTmp', 'wasmHost.get', 'createRunner', 'runner.send:configure']);
  assert.equal(e.R.runner(), e.runner);
});

test('the runner gets every service module, the host, the body store and both callbacks', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const d = e.created.runnerDeps;
  assert.deepEqual(plain(d.host), { id: 'host' });
  assert.equal(d.bodies, e.parts.bodies);
  for (const name of ['net', 'browser', 'translators', 'zoteroOa', 'save', 'confirm', 'progress', 'state', 'login']) {
    assert.equal(d[name], e.parts[name], name);
  }
  assert.equal(typeof d.onRunFinished, 'function');
  assert.equal(typeof d.onEngineFailure, 'function');
  assert.deepEqual(plain(e.created.runnerOpts), { trace: false });
  assert.equal('clock' in d, false, 'no clock override: the runner uses its default');
  assert.equal(d.timers, e.parts.timers, 'timers override is passed through');
});

test('the trace option follows the debug.trace pref', async () => {
  const e = env();
  e.flags.traceOn = true;
  await e.R.init(e.overrides());
  assert.deepEqual(plain(e.created.runnerOpts), { trace: true });
});

test('Configure carries the protocol, the engine config, the persisted state and a random seed below 2^53', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const [configure] = e.runner.sent[0];
  assert.deepEqual(Object.keys(configure), ['type', 'protocol', 'config', 'persisted', 'seed']);
  assert.equal(configure.type, 'configure');
  assert.equal(configure.protocol, 1);
  assert.deepEqual(plain(configure.config), BASE_CONFIG);
  assert.deepEqual(plain(configure.persisted), PERSISTED);
  assert.ok(Number.isSafeInteger(configure.seed) && configure.seed >= 0 && configure.seed < 2 ** 53, 'seed ' + configure.seed);
  assert.equal(Number.isInteger(configure.seed), true);
  assert.equal(e.runner.sent.length, 1, 'one send at init');
});

test('the seed is built from the random source: top word masked to 21 bits', async () => {
  const e = env();
  await e.R.init(e.overrides({ random: (words) => { words[0] = 0xffffffff; words[1] = 0xffffffff; } }));
  assert.equal(e.runner.sent[0][0].seed, 2 ** 53 - 1);
  const f = env();
  await f.R.init(f.overrides({ random: (words) => { words[0] = 0; words[1] = 12345; } }));
  assert.equal(f.runner.sent[0][0].seed, 12345);
});

test('a missing or failing random source falls back to Math.random', async () => {
  const e = env();
  await e.R.init(e.overrides({ random: () => { throw new Error('no crypto'); } }));
  const seed = e.runner.sent[0][0].seed;
  assert.ok(Number.isSafeInteger(seed) && seed >= 0 && seed < 2 ** 53);
});

test('ZFT.bodies.store is created from BodyStore({state}) unless a store is supplied', async () => {
  const e = env();
  e.ZFT.bodies = { store: null, BodyStore: class { constructor(deps) { this.deps = deps; } async releaseAll() {} } };
  const { bodies, ...rest } = e.overrides();
  await e.R.init(rest);
  assert.ok(e.ZFT.bodies.store instanceof e.ZFT.bodies.BodyStore);
  assert.equal(e.ZFT.bodies.store.deps.state, e.parts.state);
  assert.equal(e.created.runnerDeps.bodies, e.ZFT.bodies.store);
  await e.R.shutdown();
  assert.equal(e.ZFT.bodies.store, null, 'cleared on shutdown');
});

test('a supplied store is published as ZFT.bodies.store too', async () => {
  const e = env();
  e.ZFT.bodies = { store: null };
  await e.R.init(e.overrides());
  assert.equal(e.ZFT.bodies.store, e.parts.bodies);
});

test('init is memoised: concurrent callers share one init, later callers get the same result', async () => {
  const e = env();
  const a = e.R.init(e.overrides());
  const b = e.R.init();
  const c = e.R.init(e.overrides());
  assert.equal(a, b);
  assert.equal(a, c);
  await Promise.all([a, b, c]);
  await e.R.init();
  assert.equal(e.created.count, 1);
  assert.equal(e.calls.filter((c) => c === 'wasmHost.get').length, 1);
  assert.equal(e.calls.filter((c) => c === 'state.init').length, 1);
  assert.equal(e.runner.sent.length, 1);
});

test('a failed wasm load rejects with the notice id, is not cached, and the retry works', async () => {
  const e = env();
  e.flags.wasmFail = new Error('compile error');
  e.ZFT.bodies = { store: null };
  const first = e.R.init(e.overrides());
  const second = e.R.init();
  await assert.rejects(first, (err) => err.message === 'zotero-fulltext-notice-wasm-load-failed' && /compile error/.test(err.reason));
  await assert.rejects(second, /wasm-load-failed/);
  assert.equal(e.created.count, 0);
  assert.equal(e.R.runner(), null);
  assert.equal(e.ZFT.bodies.store, null, 'no store published for a controller that never came up');
  assert.equal(e.runner.sent.length, 0, 'no Configure');

  e.flags.wasmFail = null;
  await e.R.init(e.overrides());
  assert.equal(e.created.count, 1);
  assert.equal(e.R.runner(), e.runner);
  assert.equal(e.runner.commands('configure').length, 1);
});

test('startRun reports a wasm failure with the same notice id', async () => {
  const e = env();
  e.flags.wasmFail = new Error('bad abi');
  const init = e.R.init(e.overrides());
  init.catch(() => {});
  await assert.rejects(e.R.startRun({ items: [item(1)] }), (err) => err.message === 'zotero-fulltext-notice-wasm-load-failed');
  assert.equal(plain(e.R.activeRuns()).length, 0);
});

test('a state init failure is a start-failed notice carrying the reason', async () => {
  const e = env();
  e.flags.stateInitFail = new Error('profile is read-only');
  await assert.rejects(e.R.init(e.overrides()), (err) => err.message === 'zotero-fulltext-notice-start-failed' && /profile is read-only/.test(err.reason));
  assert.equal(e.calls.includes('wasmHost.get'), false, 'the host is not even loaded');
  e.flags.stateInitFail = null;
  await e.R.init(e.overrides());
  assert.equal(e.created.count, 1);
});

test('a failing temp sweep does not stop init', async () => {
  const e = env();
  e.parts.state.sweepTmp = async () => { throw new Error('locked'); };
  await e.R.init(e.overrides());
  assert.equal(e.R.runner(), e.runner);
});

test('a runner that fails to take Configure turns into start-failed and leaves nothing behind', async () => {
  const e = env();
  e.ZFT.bodies = { store: null };
  e.runner.sendThrows = new Error('queue broke');
  await assert.rejects(e.R.init(e.overrides()), (err) => err.message === 'zotero-fulltext-notice-start-failed' && /queue broke/.test(err.reason));
  assert.equal(e.R.runner(), null);
  assert.equal(e.ZFT.bodies.store, null);
  assert.equal(e.runner.disposed, true, 'the half-built runner is disposed');
  e.runner.sendThrows = null;
  await e.R.init(e.overrides());
  assert.equal(e.R.runner(), e.runner);
});

test('the sweep of leftover temp dirs happens before the first Configure (real state over a fake file system)', async () => {
  const fs = makeFakeFs();
  const e = env({ globals: { IOUtils: fs.IOUtils, PathUtils: fs.PathUtils } });
  await fs.IOUtils.write('/profile/zotero-fulltext/state.json', new TextEncoder().encode(JSON.stringify({
    v: 1, persisted: { halts: { inst_until: 123, reason: 'r', hosts: {} } }, tmp_dirs: ['/storage/tmp-crashed'], run_counter: 41,
  })));
  await fs.IOUtils.write('/storage/tmp-crashed/file.tmp', new Uint8Array([1, 2, 3]));
  await fs.IOUtils.write('/profile/zotero-fulltext/tmp/old/file.tmp', new Uint8Array([1]));

  const seenAtConfigure = [];
  const realSend = e.runner.send;
  e.runner.send = (commands) => {
    if (commands.some((c) => c.type === 'configure')) {
      seenAtConfigure.push({ crashed: fs.files.has('/storage/tmp-crashed/file.tmp'), old: fs.files.has('/profile/zotero-fulltext/tmp/old/file.tmp') });
    }
    return realSend(commands);
  };
  const { state, ...rest } = e.overrides();
  await e.R.init(rest);
  assert.deepEqual(seenAtConfigure, [{ crashed: false, old: false }]);
  assert.equal(e.runner.sent[0][0].persisted.halts.inst_until, 123, 'Configure carries the persisted state that was loaded');
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.equal(handle.run, 42, 'run ids continue from the persisted counter');
  e.ZFT.state.dispose();
});

// ------------------------------------------------------------------ startRun

test('startRun sends exactly one start_run with the snapshots in selection order and opens progress first', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.calls.length = 0;
  const handle = await e.R.startRun({ items: [item(30), item(10), item(20)], mode: 'oa_only', interactive: true });
  assert.equal(handle.run, 1);
  assert.equal(typeof handle.cancel, 'function');
  assert.equal(handle.done instanceof Promise || typeof handle.done.then === 'function', true);
  assert.deepEqual(e.calls, ['items.snapshotAll', 'items.watchDeletions', 'progress.open', 'runner.send:start_run']);

  const starts = e.runner.commands('start_run');
  assert.equal(starts.length, 1);
  const cmd = plain(starts[0]);
  assert.deepEqual(Object.keys(cmd), ['type', 'run', 'mode', 'interactive', 'items', 'resume']);
  assert.equal(cmd.run, 1);
  assert.equal(cmd.mode, 'oa_only');
  assert.equal(cmd.interactive, true);
  assert.deepEqual(cmd.items.map((s) => s.item), [30, 10, 20]);
  assert.deepEqual(cmd.resume, []);
  assert.equal(e.runner.commands('configure').length, 1, 'no extra Configure for an unchanged config');

  assert.equal(e.progressOpened.length, 1);
  assert.equal(e.progressOpened[0].run, 1);
  assert.deepEqual(plain(e.progressOpened[0].o), { interactive: true });
  assert.deepEqual(plain(e.progressOpened[0].snaps).map((s) => s.item), [30, 10, 20]);
  assert.deepEqual(plain(e.R.activeRuns()), [1]);
});

test('mode defaults to full and interactive to false; run ids come from the state counter', async () => {
  const e = env({ firstRunId: 6 });
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.equal(handle.run, 7);
  const cmd = e.runner.commands('start_run')[0];
  assert.equal(cmd.mode, 'full');
  assert.equal(cmd.interactive, false);
  const second = await e.R.startRun({ items: [item(2)], mode: null });
  assert.equal(second.run, 8);
  assert.equal(e.runner.commands('start_run')[1].mode, 'full');
  assert.deepEqual(plain(e.R.activeRuns()), [7, 8]);
});

test('startRun initialises lazily when nobody called init', async () => {
  const e = env();
  e.ZFT.runs.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.equal(handle.run, 1);
  assert.equal(e.created.count, 1);
});

test('done resolves with the report from onRunFinished, which also records it and frees the slot', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  let settled = false;
  handle.done.then(() => { settled = true; });
  await settle();
  assert.equal(settled, false);
  assert.equal(e.R.report(handle.run), null);

  const rep = report(handle.run);
  e.created.runnerDeps.onRunFinished(rep);
  const got = await handle.done;
  assert.equal(got, rep);
  assert.equal(e.R.report(handle.run), rep);
  assert.deepEqual(plain(e.R.activeRuns()), []);
  await settle();
  assert.ok(e.calls.includes('state.flush'), 'state is flushed when a run finishes');
});

test('done also resolves for a synthetic report from an engine failure and never rejects', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1), item(2)] });
  const synthetic = report(handle.run, {
    engine_restarts: 1,
    counts: { saved: 0, skipped: 0, not_found: 0, needs_user: 2, blocked: 0, deferred: 0, cancelled: 0, failed: 0 },
    items: [
      { item: 1, key: '1/KEY1', outcome: { kind: 'needs_user', why: 'engine_restarted', handoff: {} }, trace: [] },
      { item: 2, key: '1/KEY2', outcome: { kind: 'needs_user', why: 'engine_restarted', handoff: {} }, trace: [] },
    ],
  });
  e.created.runnerDeps.onRunFinished(synthetic);
  assert.equal((await handle.done).engine_restarts, 1);
  const outcome = await Promise.race([handle.done.then(() => 'resolved'), new Promise((r) => setTimeout(() => r('timeout'), 50))]);
  assert.equal(outcome, 'resolved');
});

test('onRunFinished tolerates junk: unknown runs, a second report for the same run, or nothing at all', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  const cb = e.created.runnerDeps.onRunFinished;
  assert.doesNotThrow(() => cb(undefined));
  assert.doesNotThrow(() => cb(null));
  assert.doesNotThrow(() => cb({}));
  assert.doesNotThrow(() => cb(report(999)));
  assert.equal(e.R.report(999).run, 999, 'a report for a run we did not start is still kept');
  assert.deepEqual(plain(e.R.activeRuns()), [handle.run]);
  cb(report(handle.run));
  assert.doesNotThrow(() => cb(report(handle.run)));
  await handle.done;
});

test('only the last 20 reports are kept', async () => {
  const e = env();
  await e.R.init(e.overrides());
  for (let run = 1; run <= 25; run++) e.created.runnerDeps.onRunFinished(report(run));
  assert.equal(e.R.report(5), null);
  assert.equal(e.R.report(6).run, 6);
  assert.equal(e.R.report(25).run, 25);
  e.created.runnerDeps.onRunFinished(report(6, { engine_restarts: 3 }));
  e.created.runnerDeps.onRunFinished(report(26));
  assert.equal(e.R.report(6).engine_restarts, 3, 'a re-reported run counts as the newest');
  assert.equal(e.R.report(7), null);
});

test('startRun input checks: nothing selected, too many items, bad mode', async () => {
  const e = env();
  await e.R.init(e.overrides());
  for (const items of [[], undefined, null]) {
    await assert.rejects(e.R.startRun({ items }), (err) => err.message === 'zotero-fulltext-notice-no-eligible');
  }
  await assert.rejects(e.R.startRun(), /no-eligible/);
  await assert.rejects(e.R.startRun({ items: Array.from({ length: 5001 }, (_, i) => item(i + 1)) }),
    (err) => err.message === 'zotero-fulltext-notice-start-failed' && /5000/.test(err.reason));
  await assert.rejects(e.R.startRun({ items: [item(1)], mode: 'turbo' }),
    (err) => err.message === 'zotero-fulltext-notice-start-failed' && /turbo/.test(err.reason));
  const ok = await e.R.startRun({ items: Array.from({ length: 5000 }, (_, i) => item(i + 1)) });
  assert.equal(ok.run, 1, 'exactly the cap is fine; the rejected attempts did not use run ids');
  assert.equal(e.runner.commands('start_run').length, 1);
});

test('when no item can be snapshotted the run is refused as no-eligible', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.flags.snapshotOverride = [];
  await assert.rejects(e.R.startRun({ items: [item(1)] }), /no-eligible/);
  assert.equal(e.runner.commands('start_run').length, 0);
  assert.deepEqual(plain(e.R.activeRuns()), []);
});

test('a send failure rolls the run back and surfaces start-failed', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.runner.sendThrows = new Error('queue broke');
  await assert.rejects(e.R.startRun({ items: [item(1)] }), (err) => err.message === 'zotero-fulltext-notice-start-failed' && /queue broke/.test(err.reason));
  assert.deepEqual(plain(e.R.activeRuns()), []);
  assert.ok(e.calls.includes('items.unwatch'), 'the deletion watcher goes away with the failed run');
});

test('a progress window that cannot open does not stop the run', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.parts.progress.open = () => { throw new Error('no window'); };
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.equal(e.runner.commands('start_run').length, 1);
  assert.deepEqual(plain(e.R.activeRuns()), [handle.run]);
});

test('the deletion watcher is registered for the first active run and removed after the last one', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  const b = await e.R.startRun({ items: [item(2)] });
  assert.equal(e.calls.filter((c) => c === 'items.watchDeletions').length, 1);
  e.created.runnerDeps.onRunFinished(report(a.run));
  assert.equal(e.calls.includes('items.unwatch'), false, 'still one run active');
  e.created.runnerDeps.onRunFinished(report(b.run));
  assert.equal(e.calls.filter((c) => c === 'items.unwatch').length, 1);
  await e.R.startRun({ items: [item(3)] });
  assert.equal(e.calls.filter((c) => c === 'items.watchDeletions').length, 2, 'a later run watches again');
});

test('a failing watcher registration does not stop the run', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.parts.items.watchDeletions = () => { throw new Error('notifier gone'); };
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.deepEqual(plain(e.R.activeRuns()), [handle.run]);
});

// ------------------------------------------------------------------ cancel / user actions / deletions

test('cancelRun, cancelItem and userAction send the right commands', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(5)] });
  e.runner.sent.length = 0;

  e.R.cancelRun(handle.run);
  e.R.cancelItem(handle.run, 5);
  for (const kind of e.R.USER_ACTIONS) e.R.userAction(handle.run, kind);
  handle.cancel();
  assert.deepEqual(plain(e.runner.sent), [
    [{ type: 'cancel_run', run: 1 }],
    [{ type: 'cancel_item', run: 1, item: 5 }],
    [{ type: 'user_action', run: 1, kind: 'sign_in_now' }],
    [{ type: 'user_action', run: 1, kind: 'skip_sign_in' }],
    [{ type: 'user_action', run: 1, kind: 'keep_waiting' }],
    [{ type: 'user_action', run: 1, kind: 'retry_sign_in' }],
    [{ type: 'cancel_run', run: 1 }],
  ]);
  assert.deepEqual(plain(e.R.USER_ACTIONS), ['sign_in_now', 'skip_sign_in', 'keep_waiting', 'retry_sign_in']);
  assert.deepEqual(plain(e.R.MODES), ['full', 'oa_only']);
});

test('commands with invalid arguments are never sent (one bad command would fail the whole engine step)', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.runner.sent.length = 0;
  e.R.cancelRun('1');
  e.R.cancelRun(-1);
  e.R.cancelRun(1.5);
  e.R.cancelRun(undefined);
  e.R.cancelItem(1);
  e.R.cancelItem('1', 2);
  e.R.cancelItem(1, null);
  e.R.userAction(1, 'explode');
  e.R.userAction(1, undefined);
  e.R.userAction('x', 'keep_waiting');
  assert.deepEqual(e.runner.sent, []);
});

test('controls before init or after shutdown are ignored, not thrown', async () => {
  const e = env();
  assert.doesNotThrow(() => e.R.cancelRun(1));
  assert.doesNotThrow(() => e.R.cancelItem(1, 2));
  assert.doesNotThrow(() => e.R.userAction(1, 'keep_waiting'));
  assert.equal(e.R.runner(), null);
  assert.deepEqual(plain(e.R.activeRuns()), []);
  assert.equal(e.R.report(1), null);
  await e.R.reconfigure();
  await e.R.shutdown();
});

test('a send failure inside cancelRun is logged, not thrown', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.runner.sendThrows = new Error('queue broke');
  assert.doesNotThrow(() => e.R.cancelRun(1));
});

test('item deleted or trashed: cancel_item only for the runs that contain it, once', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1), item(2)] });
  const b = await e.R.startRun({ items: [item(2), item(3)] });
  assert.equal(e.watchers.length, 1);
  const gone = e.watchers[0];
  e.runner.sent.length = 0;

  gone(1);
  assert.deepEqual(plain(e.runner.sent), [[{ type: 'cancel_item', run: a.run, item: 1 }]]);
  e.runner.sent.length = 0;

  gone(2);
  assert.deepEqual(plain(e.runner.sent), [[{ type: 'cancel_item', run: a.run, item: 2 }, { type: 'cancel_item', run: b.run, item: 2 }]], 'both runs, one send');
  e.runner.sent.length = 0;

  gone(2);
  gone(1);
  gone(99);
  assert.deepEqual(e.runner.sent, [], 'a repeated event (trash then delete) and unrelated items send nothing');

  gone(3);
  assert.deepEqual(plain(e.runner.sent), [[{ type: 'cancel_item', run: b.run, item: 3 }]]);
});

test('a run that has finished is no longer told about deleted items', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  const b = await e.R.startRun({ items: [item(1)] });
  e.created.runnerDeps.onRunFinished(report(a.run));
  e.runner.sent.length = 0;
  e.watchers[0](1);
  assert.deepEqual(plain(e.runner.sent), [[{ type: 'cancel_item', run: b.run, item: 1 }]]);
});

// ------------------------------------------------------------------ Configure re-send rules

test('reconfigure before init does nothing', async () => {
  const e = env();
  await e.R.reconfigure();
  assert.equal(e.created.count, 0);
});

test('reconfigure re-sends Configure only when the config changed and no run is active', async () => {
  const e = env();
  await e.R.init(e.overrides());
  await e.R.reconfigure();
  assert.equal(e.runner.commands('configure').length, 1, 'unchanged: nothing sent');

  e.cfg.value.run.item_concurrency = 2;
  await e.R.reconfigure();
  const configures = e.runner.commands('configure');
  assert.equal(configures.length, 2);
  assert.equal(configures[1].config.run.item_concurrency, 2);
  assert.deepEqual(plain(configures[1].persisted), PERSISTED);
  assert.equal(configures[1].protocol, 1);
  assert.notEqual(configures[1].seed, undefined);

  await e.R.reconfigure();
  assert.equal(e.runner.commands('configure').length, 2, 'unchanged again');
});

test('reconfigure waits for the last active run to finish', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  const b = await e.R.startRun({ items: [item(2)] });
  e.cfg.value.inst.enabled = false;
  await e.R.reconfigure();
  assert.equal(e.runner.commands('configure').length, 1, 'deferred while runs are active');

  e.created.runnerDeps.onRunFinished(report(a.run));
  assert.equal(e.runner.commands('configure').length, 1, 'one run is still active');
  e.created.runnerDeps.onRunFinished(report(b.run));
  const configures = e.runner.commands('configure');
  assert.equal(configures.length, 2);
  assert.equal(configures[1].config.inst.enabled, false);

  e.created.runnerDeps.onRunFinished(report(77));
  assert.equal(e.runner.commands('configure').length, 2, 'no repeat');
});

test('a pending reconfigure whose change was undone sends nothing', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  e.cfg.value.inst.enabled = false;
  await e.R.reconfigure();
  e.cfg.value.inst.enabled = true;
  e.created.runnerDeps.onRunFinished(report(a.run));
  assert.equal(e.runner.commands('configure').length, 1);
});

test('startRun re-sends Configure first, in the same send, when the prefs changed and nothing is running', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.cfg.value.oa.openalex = false;
  e.runner.sent.length = 0;
  await e.R.startRun({ items: [item(1)] });
  assert.equal(e.runner.sent.length, 1);
  assert.deepEqual(types(e.runner.sent[0]), ['configure', 'start_run']);
  assert.equal(e.runner.sent[0][0].config.oa.openalex, false);
  const second = await e.R.startRun({ items: [item(2)] });
  assert.deepEqual(types(e.runner.sent[1]), ['start_run'], 'no repeat once the engine has the new config');
  assert.ok(second.run > 0);
});

test('startRun does not change the engine config under a run that is still active', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  e.cfg.value.oa.openalex = false;
  e.runner.sent.length = 0;
  await e.R.startRun({ items: [item(2)] });
  assert.deepEqual(types(e.runner.sent[0]), ['start_run']);
  e.created.runnerDeps.onRunFinished(report(a.run));
  assert.equal(e.runner.sent.length, 1, 'run 1 finished but run 2 is still active');
  e.created.runnerDeps.onRunFinished(report(2));
  assert.deepEqual(types(e.runner.sent[1]), ['configure'], 'applied once everything is idle');
});

test('unreadable prefs at startRun fall back to the previous config; after an engine failure they refuse the run', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.parts.config.buildEngineConfig = () => { throw new Error('bad pref'); };
  const handle = await e.R.startRun({ items: [item(1)] });
  assert.deepEqual(types(e.runner.sent.at(-1)), ['start_run']);
  e.created.runnerDeps.onRunFinished(report(handle.run));

  e.created.runnerDeps.onEngineFailure(new Error('trap'));
  await assert.rejects(e.R.startRun({ items: [item(2)] }), (err) => err.message === 'zotero-fulltext-notice-start-failed' && /bad pref/.test(err.reason));
  assert.deepEqual(plain(e.R.activeRuns()), [], 'nothing was started');
});

test('after an engine failure the next startRun sends Configure first, in the same step, with the persisted state', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  e.runner.needsConfigure = true;
  e.created.runnerDeps.onRunFinished(report(handle.run, { engine_restarts: 1 }));
  e.created.runnerDeps.onEngineFailure(new Error('EngineTrap: unreachable'));
  e.runner.sent.length = 0;

  const next = await e.R.startRun({ items: [item(2)] });
  assert.equal(e.runner.sent.length, 1);
  assert.deepEqual(types(e.runner.sent[0]), ['configure', 'start_run']);
  assert.deepEqual(plain(e.runner.sent[0][0].persisted), PERSISTED);
  assert.equal(e.runner.sent[0][0].protocol, 1);
  assert.equal(e.runner.sent[0][1].run, next.run);

  const third = await e.R.startRun({ items: [item(3)] });
  assert.deepEqual(types(e.runner.sent[1]), ['start_run'], 'only once per failure');
  assert.ok(third.run > next.run);
});

test('after an engine failure, reconfigure sends Configure even when the prefs are unchanged', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.created.runnerDeps.onEngineFailure(new Error('trap'));
  await e.R.reconfigure();
  assert.equal(e.runner.commands('configure').length, 2);
});

// ------------------------------------------------------------------ engine failure notice

test('an engine failure shows ONE alert per failure, with the notice text', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const fail = e.created.runnerDeps.onEngineFailure;
  fail(Object.assign(new Error('boom'), { panic: 'index out of bounds' }));
  assert.equal(e.alerts.length, 1);
  const [win, title, text] = e.alerts[0];
  assert.equal(win, null, 'no main window in the fake: alert without a parent');
  assert.ok(String(title).includes('zotero-fulltext-progress-title'));
  assert.ok(String(text).includes('zotero-fulltext-notice-engine-failed'));
  fail(new Error('same failure reported again'));
  assert.equal(e.alerts.length, 1);

  // Recovery (the next run re-configures the engine) re-arms the notice for the next failure.
  await e.R.startRun({ items: [item(1)] });
  fail(new Error('second failure'));
  assert.equal(e.alerts.length, 2);
});

test('the engine-failure callback never throws, even without a prompt service', async () => {
  const e = env();
  await e.R.init(e.overrides({ services: { prompt: { alert() { throw new Error('dialog failed'); } } } }));
  assert.doesNotThrow(() => e.created.runnerDeps.onEngineFailure(new Error('x')));
  const f = env();
  await f.R.init(f.overrides({ services: {} }));
  assert.doesNotThrow(() => f.created.runnerDeps.onEngineFailure(undefined));
  assert.doesNotThrow(() => f.created.runnerDeps.onEngineFailure(null));
});

test('the alert goes to the main window when Zotero has one', async () => {
  const e = env();
  const win = { name: 'main' };
  e.zotero.getMainWindow = () => win;
  await e.R.init(e.overrides());
  e.created.runnerDeps.onEngineFailure(new Error('x'));
  assert.equal(e.alerts[0][0], win);
});

// ------------------------------------------------------------------ shutdown

test('shutdown order: cancel_run for every active run + shutdown, last pump, quiet wait, then the teardown steps', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const a = await e.R.startRun({ items: [item(1)] });
  const b = await e.R.startRun({ items: [item(2)] });
  e.calls.length = 0;
  e.runner.sent.length = 0;

  await e.R.shutdown();
  assert.deepEqual(plain(e.runner.sent), [[
    { type: 'cancel_run', run: a.run },
    { type: 'cancel_run', run: b.run },
    { type: 'shutdown' },
  ]]);
  // The deletion watcher is dropped once the runs are gone (position irrelevant); everything else is ordered.
  assert.ok(e.calls.includes('items.unwatch'));
  assert.deepEqual(e.calls.filter((c) => c !== 'items.unwatch'), [
    'runner.send:cancel_run+cancel_run+shutdown',
    'runner.pump',
    'runner.whenQuiet',
    'runner.dispose',
    'bodies.releaseAll',
    'state.flush',
    'state.sweepTmp',
    'pdfCapture.disposeAll',
    'login.closeAll',
    'progress.closeAll',
    'wasmHost.shutdown',
    'state.dispose',
  ]);
  assert.equal(e.R.runner(), null);
  assert.deepEqual(plain(e.R.activeRuns()), []);
});

test('shutdown resolves the done promise of a run the engine did not finish, with cancelled-by-shutdown items', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1), item(2)] });
  await e.R.shutdown();
  const rep = plain(await handle.done);
  assert.equal(rep.run, handle.run);
  assert.equal(rep.counts.cancelled, 2);
  assert.deepEqual(rep.items.map((i) => [i.item, i.key, i.outcome]), [
    [1, '1/KEY1', { kind: 'cancelled', by: 'shutdown' }],
    [2, '1/KEY2', { kind: 'cancelled', by: 'shutdown' }],
  ]);
  assert.equal(rep.engine_restarts, 0);
});

test('a run the engine finished during the last pump keeps the engine report', async () => {
  const e = env();
  await e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  const real = report(handle.run, { counts: { saved: 1, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: 0, failed: 0 } });
  e.runner.pump = async () => { e.rec('runner.pump'); e.created.runnerDeps.onRunFinished(real); };
  await e.R.shutdown();
  assert.equal(await handle.done, real);
});

test('shutdown is idempotent: concurrent and later calls share or skip the work', async () => {
  const e = env();
  await e.R.init(e.overrides());
  await e.R.startRun({ items: [item(1)] });
  const first = e.R.shutdown();
  const second = e.R.shutdown();
  assert.equal(first, second);
  await Promise.all([first, second]);
  const count = e.calls.length;
  await e.R.shutdown();
  await e.R.shutdown();
  assert.equal(e.calls.length, count, 'nothing is repeated');
  assert.equal(e.calls.filter((c) => c === 'runner.dispose').length, 1);
  assert.equal(e.calls.filter((c) => c === 'wasmHost.shutdown').length, 1);
});

test('shutdown waits at most SHUTDOWN_WAIT_MS for the runner to go quiet', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.runner.quiet = new Promise(() => {});             // never quiet
  let done = false;
  const p = e.R.shutdown().then(() => { done = true; });
  await settle();
  assert.equal(done, false, 'still waiting for quiet');
  assert.equal(e.calls.includes('runner.dispose'), false);
  e.timers.advance(e.R.SHUTDOWN_WAIT_MS - 1);
  await settle();
  assert.equal(done, false);
  e.timers.advance(1);
  await p;
  assert.equal(done, true);
  assert.ok(e.calls.includes('runner.dispose'));
  assert.equal(e.timers.pendingCount(), 0);
});

test('a quiet runner ends the wait at once and leaves no timer behind', async () => {
  const e = env();
  await e.R.init(e.overrides());
  await e.R.shutdown();
  assert.equal(e.timers.pendingCount(), 0);
});

test('every shutdown step runs even when earlier steps fail', async () => {
  const e = env();
  await e.R.init(e.overrides());
  e.runner.send = () => { e.rec('runner.send'); throw new Error('send failed'); };
  e.runner.pump = async () => { e.rec('runner.pump'); throw new Error('pump failed'); };
  e.runner.dispose = async () => { e.rec('runner.dispose'); throw new Error('dispose failed'); };
  e.parts.bodies.releaseAll = async () => { e.rec('bodies.releaseAll'); throw new Error('release failed'); };
  e.parts.state.flush = async () => { e.rec('state.flush'); throw new Error('flush failed'); };
  e.parts.state.sweepTmp = async () => { e.rec('state.sweepTmp'); throw new Error('sweep failed'); };
  e.parts.pdfCapture.disposeAll = () => { e.rec('pdfCapture.disposeAll'); throw new Error('capture failed'); };
  e.parts.login.closeAll = () => { e.rec('login.closeAll'); throw new Error('login failed'); };
  e.parts.progress.closeAll = () => { e.rec('progress.closeAll'); throw new Error('progress failed'); };
  e.parts.wasmHost.shutdown = async () => { e.rec('wasmHost.shutdown'); throw new Error('host failed'); };
  e.calls.length = 0;
  await e.R.shutdown();
  assert.deepEqual(e.calls.filter((c) => c !== 'runner.whenQuiet'), [
    'runner.send', 'runner.pump', 'runner.dispose', 'bodies.releaseAll', 'state.flush', 'state.sweepTmp',
    'pdfCapture.disposeAll', 'login.closeAll', 'progress.closeAll', 'wasmHost.shutdown', 'state.dispose',
  ]);
  assert.equal(e.R.runner(), null);
  assert.ok(e.zotero.debugLog.some(([, m]) => /shutdown: dispose runner failed/.test(m)));
});

test('after shutdown a later init rebuilds everything, and startRun works again', async () => {
  const e = env();
  await e.R.init(e.overrides());
  await e.R.startRun({ items: [item(1)] });
  await e.R.shutdown();
  assert.equal(e.R.runner(), null);

  e.calls.length = 0;
  e.runner.sent.length = 0;
  await e.R.init(e.overrides());
  assert.equal(e.created.count, 2);
  assert.deepEqual(e.calls, ['state.init', 'state.sweepTmp', 'wasmHost.get', 'createRunner', 'runner.send:configure']);
  assert.equal(e.R.runner(), e.runner);
  const handle = await e.R.startRun({ items: [item(2)] });
  assert.deepEqual(plain(e.R.activeRuns()), [handle.run]);
  assert.equal(e.runner.commands('configure').length, 1, 'a fresh engine got exactly one Configure');
});

test('startRun after shutdown rebuilds the controller instead of using the dead one', async () => {
  const e = env();
  await e.R.init(e.overrides());
  await e.R.shutdown();
  // No overrides this time: the real wasm host and friends would be used, which the sandbox lacks. Supply them via init.
  const rebuilt = e.R.init(e.overrides());
  const handle = await e.R.startRun({ items: [item(1)] });
  await rebuilt;
  assert.equal(e.created.count, 2);
  assert.ok(handle.run > 0);
});

test('shutdown during a running init waits for it, then tears down', async () => {
  const e = env();
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  e.parts.wasmHost.get = async () => { e.rec('wasmHost.get'); await gate; return { id: 'host' }; };
  const init = e.R.init(e.overrides());
  const down = e.R.shutdown();
  await settle();
  assert.equal(e.calls.includes('runner.dispose'), false);
  release();
  await Promise.all([init, down]);
  assert.ok(e.calls.includes('runner.dispose'));
  assert.equal(e.R.runner(), null);
});

test('shutdown after a failed init has nothing to tear down and does not throw', async () => {
  const e = env();
  e.flags.wasmFail = new Error('x');
  await assert.rejects(e.R.init(e.overrides()));
  await e.R.shutdown();
  assert.equal(e.calls.includes('runner.dispose'), false);
});

test('shutdown without init resolves at once', async () => {
  const e = env();
  await e.R.shutdown();
  assert.deepEqual(e.calls, []);
});

test('startRun that races a shutdown is refused with start-failed', async () => {
  const e = env();
  await e.R.init(e.overrides());
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const realSnapshot = e.parts.items.snapshotAll;
  e.parts.items.snapshotAll = async (items) => { await gate; return realSnapshot(items); };
  const starting = e.R.startRun({ items: [item(1)] });
  await settle();
  const down = e.R.shutdown();
  release();
  await assert.rejects(starting, (err) => err.message === 'zotero-fulltext-notice-start-failed' && /shutting down/.test(err.reason));
  await down;
  assert.deepEqual(plain(e.R.activeRuns()), []);
  assert.equal(e.runner.commands('start_run').length, 0);
});

test('the constants other modules rely on are exported', () => {
  const e = env();
  assert.equal(e.R.SHUTDOWN_WAIT_MS, 2000);
  assert.equal(e.R.MAX_ITEMS_PER_RUN, 5000);
  assert.equal(e.R.REPORTS_KEPT, 20);
});
