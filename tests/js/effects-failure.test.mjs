// ZFT.effects.Runner failure policy: engine traps and errors, the stall watchdog, synthetic reports, service failures.
// Same scripted fake host and controllable fake services as effects.test.mjs (kept inline: a test file may not import another).
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeFs, makeFakeTimers, makeFakeZotero } from './helpers/fakes.mjs';

const enc = new TextEncoder();
const seq = (n, mod = 251) => Uint8Array.from({ length: n }, (_, i) => i % mod);
const flush = () => new Promise((r) => setImmediate(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const STATS = { tasks: 0, ops_inflight: 0, timers: 0, waiting_user: 0, polls: 1, lost_wakes: 0, stalled: false, mem_hint_kb: 0 };
const out = (o = {}) => ({ effects: [], next_timer: null, more: false, ignored: [], stats: STATS, ...o });
const CONFIGURE = { type: 'configure', protocol: 1, config: {}, persisted: {}, seed: 1 };
const HTTP_REQ = {
  method: 'get', url: 'https://api.openalex.org/works/doi:10.1371/x', headers: { Accept: 'application/json' }, ua: 'default',
  timeout_ms: 9000, follow_redirects: true, max_bytes: 1048576,
};
const API_SPEC = { expect: 'api', source: 'openalex', requested: { doi: '10.1371/x' } };
const PDF_SPEC = { expect: 'pdf', requested: { doi: '10.1371/x' }, route: 'oa' };
const http = (op, extra = {}) => ({
  type: 'http', op, run: 1, item: 10, route: 'oa', req: HTTP_REQ, retain: 'none', inspect: API_SPEC, ...extra,
});
const rawOk = (extra = {}) => ({
  status: 200, final_url: 'https://api.openalex.org/works/doi:10.1371/x', headers: { 'content-type': 'application/json' },
  bytes: 7, ms: 40, body: enc.encode('{"a":1}'), path: null, errorBody: null, ...extra,
});

/** Everything a Runner needs, with fake timers/clock, a real BodyStore over a fake fs, and controllable services. */
function makeKit(o = {}) {
  const timers = makeFakeTimers();
  const fsx = makeFakeFs();
  const zotero = makeFakeZotero();
  const { ZFT } = loadPlugin({ files: ['util.js', 'bodies.js', 'wasm-host.js', 'effects.js'], globals: { Zotero: zotero } });
  const registered = [];
  let dirSeq = 0;
  const store = new ZFT.bodies.BodyStore({
    fs: fsx.IOUtils, pathUtils: fsx.PathUtils,
    state: { async registerTmpDir(p) { registered.push(p); }, async unregisterTmpDir(p) { const i = registered.indexOf(p); if (i >= 0) registered.splice(i, 1); } },
    attachments: { async createTemporaryStorageDirectory() { const p = `/storage/tmp-${++dirSeq}`; await fsx.IOUtils.makeDirectory(p); return { path: p }; } },
  });

  /** A service function whose calls stay pending until the test settles them; it obeys ctx.scope like a real one. */
  const ctl = (label, { obey = true } = {}) => {
    const calls = [];
    const fn = (...args) => new Promise((resolve, reject) => {
      const call = { label, args, resolve, reject, scope: args[args.length - 1] && args[args.length - 1].scope };
      calls.push(call);
      if (obey && call.scope) call.scope.onCancel((reason) => reject(new ZFT.util.OpError('cancelled', String(reason))));
    });
    return { calls, fn };
  };
  const svc = {
    net: ctl('net.request'), browser: ctl('browser.load'), loginOpen: ctl('login.open'), translators: ctl('translators.run'),
    zoteroOa: ctl('zoteroOa.lookup'), save: ctl('save.savePdf', { obey: true }), confirm: ctl('confirm.ask'),
  };

  const stateCalls = [];
  const state = {
    hold: false,
    async set(key, value, opts) {
      const call = { key, value: plain(value), opts: plain(opts), settled: false };
      stateCalls.push(call);
      if (state.hold) await new Promise((resolve, reject) => { call.resolve = resolve; call.reject = reject; });
      call.settled = true;
    },
  };
  const progress = {
    updates: [], finishes: [],
    update(run, item, row) { progress.updates.push([run, item, plain(row)]); },
    finish(run, report) { progress.finishes.push([run, plain(report)]); },
  };
  const loginUpdates = [];
  const host = {
    steps: [], queue: [], inspected: [], resets: 0,
    inspectResult: () => ({ expect: 'none' }),
    step(stepIn) {
      host.steps.push(plain(stepIn));
      const next = host.queue.shift();
      return typeof next === 'function' ? next(stepIn) : (next || out());
    },
    async inspect(meta, bytes) { host.inspected.push({ meta: plain(meta), bytes: bytes ? [...bytes] : null }); return host.inspectResult(meta, bytes); },
    resetEngine() { host.resets++; },
  };
  const finished = [];
  const failures = [];
  const deps = {
    host, bodies: store,
    net: { request: svc.net.fn },
    browser: { load: svc.browser.fn },
    login: { open: svc.loginOpen.fn, update: (op, waiting) => loginUpdates.push([op, waiting]) },
    translators: { run: svc.translators.fn },
    zoteroOa: { lookup: svc.zoteroOa.fn },
    save: { savePdf: svc.save.fn },
    confirm: { ask: svc.confirm.fn },
    progress, state,
    clock: { monoNow: () => 1000 + timers.now, wallNow: () => 1_800_000_000_000 + timers.now },
    timers: o.realTimers ? undefined : timers,
    onRunFinished: (report) => finished.push(plain(report)),
    onEngineFailure: (err) => failures.push(err),
  };
  if (o.realTimers) delete deps.timers;
  const runner = ZFT.effects.createRunner(deps, o.runner);
  const drain = async (rounds = 8) => { for (let i = 0; i < rounds; i++) { timers.advance(0); await flush(); } };
  return { ZFT, timers, fsx, zotero, store, registered, svc, state, stateCalls, progress, loginUpdates, host, finished, failures, deps, runner, drain };
}

/** Queue a StepOut and run it through the runner. */
async function play(kit, stepOut, commands = [CONFIGURE]) {
  kit.host.queue.push(stepOut);
  kit.runner.send(commands);
  if (commands.length === 0) kit.runner.pump();   // nothing queued: an explicit pump (e.g. a timer deadline) steps anyway
  await kit.drain();
}
const completionsOf = (kit, step) => kit.host.steps[step].completions;

const START = (run, items) => ({ type: 'start_run', run, mode: 'full', interactive: true, items: items.map(([item, key]) => ({ item, key })), resume: [] });
const LOGIN_REQ = { kind: 'login', realm: 'openathens', entry_url: 'https://go.openathens.net/redirector/kcl.ac.uk', item_title: 't', waiting: 1, timeout_ms: 300000, capture_pdf: true };
const SAVE_REQ = { item: 11, body: { handle: 9, len: 100 }, trim_prefix: 0, version: 'published', url_to_store: 'https://doi.org/10.1/x', route: 'oa', source: 'pmc_s3' };
const save = (op, item, run = 1) => ({ type: 'save_pdf', op, run, item, req: { ...SAVE_REQ, item } });
const login = (op, item, run = 1) => ({ type: 'login_window', op, run, item, req: LOGIN_REQ });
const NEEDS = { kind: 'needs_user', why: 'engine_restarted', handoff: {} };
const ZERO_LEDGER = { hour: 0, hour_cap: 0, day: 0, day_cap: 0, mb_hour: 0, mb_hour_cap: 0 };
const ZERO_COUNTS = { saved: 0, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: 0, failed: 0 };
const SAVED = { kind: 'saved', route: 'oa', source: 'pmc_s3', version: 'published', url: 'https://doi.org/10.1/x', attachment_key: '1/SAVED' };

// ---------------------------------------------------------------------------------------------------------------
test('syntheticReport has the exact model.rs RunReport shape', () => {
  const { ZFT } = makeKit();
  const items = [{ item: 10, key: '1/A' }, { item: 11, key: '1/B' }, { item: 12, key: '1/C' }];
  const report = ZFT.effects.syntheticReport(4, items, new Map([[11, SAVED]]));
  assert.deepEqual(plain(report), {
    run: 4,
    counts: { ...ZERO_COUNTS, saved: 1, needs_user: 2 },
    items: [
      { item: 10, key: '1/A', outcome: NEEDS, trace: [] },
      { item: 11, key: '1/B', outcome: SAVED, trace: [] },
      { item: 12, key: '1/C', outcome: NEEDS, trace: [] },
    ],
    ledger: ZERO_LEDGER,
    halted: null,
    engine_restarts: 1,
  });
  assert.deepEqual(plain(ZFT.effects.syntheticReport(5, [], new Map())), {
    run: 5, counts: ZERO_COUNTS, items: [], ledger: ZERO_LEDGER, halted: null, engine_restarts: 1,
  });
  assert.equal(ZFT.effects.syntheticReport(6, [{ item: 1, key: 'k' }], undefined).counts.needs_user, 1, 'a missing saved map means nothing was saved');
});

test('7. EngineTrap from step: scopes cancelled, bodies released, synthetic reports, hooks called, sends ignored until Configure', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1, { retain: 'page', item: 10 }), save(2, 11), login(3, 12), http(4, { item: 10 })] }),
    [CONFIGURE, START(1, [[10, '1/A'], [11, '1/B'], [12, '1/C']])]);
  kit.svc.net.calls[0].resolve(rawOk());
  kit.svc.save.calls[0].resolve({ attachment_key: '1/SAVED', filename: 'b.pdf' });
  await kit.drain();
  assert.equal(kit.host.steps.length, 2);
  assert.equal(kit.store.stats().count, 1, 'the engine holds the page body of op 1');
  assert.equal(kit.runner.stats().ops, 2, 'the login window and op 4 are still running');

  const trap = new kit.ZFT.wasmHost.EngineTrap('wasm engine trap: unreachable', 'index out of bounds');
  kit.host.queue.push(() => { throw trap; });
  kit.runner.send([{ type: 'user_action', run: 1, kind: 'sign_in_now' }]);
  await kit.drain();

  assert.equal(kit.runner.failed, true);
  assert.equal(kit.runner.needsConfigure, true);
  assert.equal(kit.runner.stats().failed, true);
  assert.equal(kit.runner.stats().ops, 0);
  assert.equal(kit.svc.loginOpen.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.loginOpen.calls[0].scope.reason, 'engine_failure');
  assert.equal(kit.svc.net.calls[1].scope.cancelled, true, 'op 4 cancelled too');
  assert.equal(kit.store.stats().count, 0, 'all bodies released');
  assert.equal(kit.host.resets, 1, 'the engine instance was reset');
  assert.deepEqual(kit.failures, [trap]);

  const expected = {
    run: 1,
    counts: { ...ZERO_COUNTS, saved: 1, needs_user: 2 },
    items: [
      { item: 10, key: '1/A', outcome: NEEDS, trace: [] },
      { item: 11, key: '1/B', outcome: SAVED, trace: [] },
      { item: 12, key: '1/C', outcome: NEEDS, trace: [] },
    ],
    ledger: ZERO_LEDGER, halted: null, engine_restarts: 1,
  };
  assert.deepEqual(kit.finished, [expected], 'deps.onRunFinished got the synthetic report');
  assert.deepEqual(kit.progress.finishes, [[1, expected]], 'and so did the progress window');
  assert.deepEqual(kit.host.steps.flatMap((s) => s.completions).map((c) => c.op).sort(), [1, 2], 'no completion is sent for the ops that were cancelled');

  // Until reset, nothing is stepped.
  const stepsBefore = kit.host.steps.length;
  kit.runner.send([START(2, [[20, '1/Z']]), { type: 'cancel_run', run: 2 }]);
  await kit.drain();
  assert.equal(kit.host.steps.length, stepsBefore);
  assert.equal(kit.runner.stats().pendingCommands, 0);
  await kit.runner.pump();
  assert.equal(kit.host.steps.length, stepsBefore, 'pump() is inert while failed');

  // A fresh Configure (what ZFT.runs sends when needsConfigure is set) resumes service.
  kit.runner.send([CONFIGURE, START(2, [[20, '1/Z']])]);
  await kit.drain();
  assert.equal(kit.runner.failed, false);
  assert.equal(kit.runner.needsConfigure, false);
  assert.deepEqual(kit.host.steps.at(-1).commands.map((c) => c.type), ['configure', 'start_run']);
});

test('7b. EngineError (a rejected step) and a garbage StepOut fail the run the same way', async () => {
  for (const make of [
    (k) => () => { throw new k.ZFT.wasmHost.EngineError('bad_json', 'unknown variant'); },
    () => () => null,
    () => () => 'not an object',
  ]) {
    const kit = makeKit();
    await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
    kit.host.queue.push(make(kit));
    kit.runner.send([{ type: 'dump_state' }]);
    await kit.drain();
    assert.equal(kit.runner.failed, true);
    assert.equal(kit.finished.length, 1);
    assert.equal(kit.finished[0].items[0].outcome.why, 'engine_restarted');
    assert.equal(kit.failures.length, 1);
    assert.equal(kit.host.resets, 1);
  }
});

test('7c. a trap in the middle of a "more" loop ends the pump cleanly', async () => {
  const kit = makeKit();
  kit.host.queue.push(out({ more: true }), () => { throw new kit.ZFT.wasmHost.EngineTrap('trap', 'p'); });
  kit.runner.send([CONFIGURE, START(1, [[10, '1/A']])]);
  kit.timers.advance(0);
  for (let i = 0; i < 50 && !kit.runner.failed; i++) await sleep(2);
  await kit.drain();
  assert.equal(kit.runner.failed, true);
  assert.equal(kit.host.steps.length, 2);
  assert.equal(kit.finished.length, 1);
  assert.equal(kit.timers.pendingCount(), 0, 'no engine or stall timer survives a failure');
  await kit.runner.whenQuiet();
});

test('7d. only UNFINISHED runs get a synthetic report; a failure before any run still resets the engine', async () => {
  const kit = makeKit();
  await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']]), START(2, [[20, '1/B']]), START(3, [[30, '1/C'], [31, '1/D']])]);
  await play(kit, out({ effects: [{ type: 'run_finished', run: 2, report: { run: 2, counts: {}, items: [], ledger: {} } }] }), [{ type: 'dump_state' }]);
  assert.equal(kit.finished.length, 1, 'run 2 finished normally');
  kit.host.queue.push(() => { throw new Error('boom'); });
  kit.runner.send([{ type: 'dump_state' }]);
  await kit.drain();
  assert.deepEqual(kit.finished.slice(1).map((r) => [r.run, r.items.length, r.engine_restarts]), [[1, 1, 1], [3, 2, 1]]);

  const idle = makeKit();
  idle.host.queue.push(() => { throw new Error('early'); });
  idle.runner.send([CONFIGURE]);
  await idle.drain();
  assert.equal(idle.runner.failed, true);
  assert.deepEqual(idle.finished, []);
  assert.equal(idle.host.resets, 1);
  assert.equal(idle.failures.length, 1);
});

test('7e. failure is idempotent', async () => {
  const kit = makeKit();
  await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
  kit.host.queue.push(() => { throw new Error('first'); });
  kit.runner.send([{ type: 'dump_state' }]);
  await kit.drain();
  kit.runner._fail(new Error('second'));
  await kit.drain();
  assert.equal(kit.failures.length, 1);
  assert.equal(kit.finished.length, 1);
  assert.equal(kit.host.resets, 1);
});

test('7f. a handler that ignores its scope and answers after the failure is dropped; its body is released', async () => {
  const kit = makeKit();
  let late;
  kit.deps.net.request = () => new Promise((resolve) => { late = () => resolve(rawOk()); });
  await play(kit, out({ effects: [http(1, { retain: 'page' })] }), [CONFIGURE, START(1, [[10, '1/A']])]);
  kit.host.queue.push(() => { throw new Error('boom'); });
  kit.runner.send([{ type: 'dump_state' }]);
  await kit.drain();
  assert.equal(kit.runner.failed, true);
  late();
  await kit.drain();
  assert.equal(kit.store.stats().count, 0);
  assert.equal(kit.host.steps.length, 2);
  assert.equal(kit.runner.stats().pendingCompletions, 0);
});

test('7g. a started save is cancelled by a failure (the body it needs is gone) while a user cancel could not touch it', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [save(2, 11)] }), [CONFIGURE, START(1, [[11, '1/B']])]);
  kit.runner.send([{ type: 'cancel_run', run: 1 }]);
  assert.equal(kit.svc.save.calls[0].scope.cancelled, false);
  kit.host.queue.push(() => { throw new Error('boom'); });
  await kit.drain();
  assert.equal(kit.svc.save.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.save.calls[0].scope.reason, 'engine_failure');
});

test('7h. throwing hooks and UI callbacks cannot stop the failure policy', async () => {
  const kit = makeKit();
  kit.progress.finish = () => { throw new Error('window gone'); };
  kit.deps.onRunFinished = () => { throw new Error('listener bug'); };
  kit.host.resetEngine = () => { throw new Error('reset failed'); };
  kit.deps.onEngineFailure = () => { throw new Error('hook bug'); };
  await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
  kit.host.queue.push(() => { throw new Error('boom'); });
  kit.runner.send([{ type: 'dump_state' }]);
  await kit.drain();
  assert.equal(kit.runner.failed, true);
  assert.equal(kit.runner.needsConfigure, true);
  const log = kit.zotero.debugLog.map(([, m]) => m).join('\n');
  for (const what of ['window gone', 'listener bug', 'reset failed', 'hook bug']) assert.match(log, new RegExp(what));
});

// ---------------------------------------------------------------------------------------------------------------
// 8. stall detection
// ---------------------------------------------------------------------------------------------------------------
test('8. stall: three checks (stallMs/3 apart) find nothing in flight; DumpState is sent once, then the run is failed', async () => {
  const kit = makeKit({ runner: { stallMs: 300 } });
  await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
  assert.equal(kit.timers.pendingCount(), 1, 'the watchdog is armed by the first run');

  kit.timers.advance(100);
  await kit.drain(2);
  assert.equal(kit.runner.failed, false);
  assert.equal(kit.host.steps.length, 1, 'strike 1: nothing yet');

  kit.host.queue.push(out({ effects: [{ type: 'log', level: 'info', msg: 'dump: tasks=1 ops=0' }] }));
  kit.timers.advance(100);
  await kit.drain(2);
  assert.equal(kit.host.steps.length, 2, 'strike 2: DumpState sent');
  assert.deepEqual(plain(kit.host.steps[1].commands), [{ type: 'dump_state' }]);
  assert.ok(kit.zotero.debugLog.some(([, m]) => /engine: dump: tasks=1/.test(m)), 'the reply is logged');
  assert.equal(kit.runner.failed, false);

  kit.timers.advance(100);
  await kit.drain(2);
  assert.equal(kit.runner.failed, true, 'strike 3: still stalled');
  assert.equal(kit.failures.length, 1);
  assert.equal(kit.failures[0].message, 'engine stalled');
  assert.equal(kit.finished.length, 1);
  assert.equal(kit.finished[0].items[0].outcome.why, 'engine_restarted');
  assert.equal(kit.host.steps.flatMap((s) => s.commands).filter((c) => c.type === 'dump_state').length, 1, 'dumped once');
  assert.equal(kit.timers.pendingCount(), 0);
});

test('8b. no stall while an op is in flight, a timer is armed or the user is being waited for', async () => {
  const scenarios = [
    ['op in flight', out({ effects: [http(1)] })],
    ['next_timer armed', out({ next_timer: 10_000_000 })],
    ['waiting for the user', out({ stats: { ...STATS, waiting_user: 1 } })],
  ];
  for (const [name, stepOut] of scenarios) {
    const kit = makeKit({ runner: { stallMs: 300 } });
    await play(kit, stepOut, [CONFIGURE, START(1, [[10, '1/A']])]);
    for (let i = 0; i < 12; i++) { kit.timers.advance(100); await kit.drain(2); }
    assert.equal(kit.runner.failed, false, name);
    assert.deepEqual(kit.host.steps.flatMap((s) => s.commands).filter((c) => c.type === 'dump_state'), [], name);
  }
});

test('8c. a stalled check followed by activity resets the count', async () => {
  const kit = makeKit({ runner: { stallMs: 300 } });
  await play(kit, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
  kit.timers.advance(100);                       // strike 1
  await kit.drain(2);
  await play(kit, out({ effects: [http(1)] }), [{ type: 'user_action', run: 1, kind: 'keep_waiting' }]);
  kit.timers.advance(100);                       // not stalled: an op is running
  await kit.drain(2);
  kit.svc.net.calls[0].resolve(rawOk());
  kit.host.queue.push(out());
  await kit.drain();
  kit.timers.advance(100);                       // strike 1 again, not 2: no DumpState
  await kit.drain(2);
  assert.equal(kit.runner.failed, false);
  assert.deepEqual(kit.host.steps.flatMap((s) => s.commands).filter((c) => c.type === 'dump_state'), []);
});

test('8d. the watchdog follows the runs: off when the last run finishes, never armed for effects of runs it was not told about', async () => {
  const kit = makeKit({ runner: { stallMs: 300 } });
  await play(kit, out({ effects: [http(1, { run: 77 })] }));
  assert.equal(kit.timers.pendingCount(), 0, 'no start_run, no watchdog');

  const k2 = makeKit({ runner: { stallMs: 300 } });
  await play(k2, out(), [CONFIGURE, START(1, [[10, '1/A']])]);
  assert.equal(k2.timers.pendingCount(), 1);
  await play(k2, out({ effects: [{ type: 'run_finished', run: 1, report: { run: 1 } }] }), [{ type: 'dump_state' }]);
  assert.equal(k2.timers.pendingCount(), 0, 'finished: the watchdog is disarmed');
  for (let i = 0; i < 6; i++) { k2.timers.advance(100); await k2.drain(2); }
  assert.equal(k2.runner.failed, false);
});

// ---------------------------------------------------------------------------------------------------------------
// 9. service failures
// ---------------------------------------------------------------------------------------------------------------
test('9. every OpError kind a service throws reaches the engine unchanged, with its extras', async () => {
  const { ZFT } = makeKit();
  for (const kind of ZFT.util.OP_ERR_KINDS) {
    const kit = makeKit();
    kit.deps.net.request = async () => { throw new kit.ZFT.util.OpError(kind, 'm-' + kind, { afterMs: 1500, status: 429 }); };
    await play(kit, out({ effects: [http(1)] }));
    await kit.drain();
    assert.deepEqual(plain(kit.host.steps[1].completions), [{ op: 1, err: { kind, message: 'm-' + kind, after_ms: 1500, status: 429 } }], kind);
  }
});

test('9b. non-OpError failures become "internal": async throw, sync throw, rejected string, undefined', async () => {
  const kit = makeKit();
  kit.deps.zoteroOa.lookup = () => { throw new TypeError('sync boom'); };
  kit.deps.confirm.ask = () => Promise.reject('plain string');
  kit.deps.translators.run = () => Promise.reject(undefined);
  kit.deps.net.request = async () => { throw new RangeError('async boom'); };
  await play(kit, out({ effects: [
    { type: 'zotero_oa', op: 1, run: 1, item: 10, doi: 'x' },
    { type: 'confirm', op: 2, run: 1, req: { kind: 'large_batch', n: 1, cap: 1, library: '' } },
    { type: 'run_translators', op: 3, run: 1, item: 10, page: { handle: 1, len: 1 }, url: 'u' },
    http(4),
  ] }));
  await kit.drain();
  const byOp = Object.fromEntries(kit.host.steps[1].completions.map((c) => [c.op, c.err]));
  assert.deepEqual(Object.values(byOp).map((e) => e.kind), ['internal', 'internal', 'internal', 'internal']);
  assert.match(byOp[1].message, /TypeError: sync boom/);
  assert.match(byOp[4].message, /RangeError: async boom/);
  assert.equal(kit.runner.failed, false, 'service failures never fail the run');
});

test('9c. a slot that cannot be allocated, a second login window and an overlong error message', async () => {
  const kit = makeKit();
  kit.store.allocPdfSlot = async () => { throw new kit.ZFT.util.OpError('io', 'storage unavailable'); };
  kit.deps.login.open = async () => { throw new kit.ZFT.util.OpError('internal', 'a login window is already open for this realm'); };
  kit.deps.save.savePdf = async () => { throw new kit.ZFT.util.OpError('already_has_pdf', 'x'.repeat(1000)); };
  await play(kit, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC }), login(2, 10), save(3, 10)] }));
  await kit.drain();
  const byOp = Object.fromEntries(kit.host.steps[1].completions.map((c) => [c.op, c.err]));
  assert.equal(byOp[1].kind, 'io');
  assert.equal(byOp[2].kind, 'internal');
  assert.equal(byOp[3].kind, 'already_has_pdf');
  assert.equal(byOp[3].message.length, 300, 'messages are capped');
  assert.equal(kit.runner.stats().ops, 0);
});

test('9d. a duplicate op id is ignored, not started twice', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), http(1, { item: 11 })] }));
  assert.equal(kit.svc.net.calls.length, 1);
  assert.ok(kit.zotero.debugLog.some(([, m]) => /duplicate op 1/.test(m)));
});

test('9e. persist with a synchronous throw in state.set is logged and later persists still run', async () => {
  const kit = makeKit();
  let n = 0;
  kit.deps.state.set = (key) => { n++; if (n === 1) throw new Error('sync write error'); };
  const persist = (k) => ({ type: 'persist', key: k, value: {}, flush: true });
  await play(kit, out({ effects: [persist('ledger'), persist('halts'), http(1, { barrier: true })] }));
  assert.equal(n, 2);
  assert.equal(kit.svc.net.calls.length, 1, 'the barrier request still started');
  assert.ok(kit.zotero.debugLog.some(([, m]) => /persist ledger failed: Error: sync write error/.test(m)));
});

test('9f. dispose does not hang on a handler that ignores cancellation (bounded by disposeWaitMs)', async () => {
  const kit = makeKit({ runner: { disposeWaitMs: 500 } });
  kit.deps.net.request = () => new Promise(() => {});   // never settles, ignores the scope
  await play(kit, out({ effects: [http(1)] }));
  let done = false;
  const p = kit.runner.dispose().then(() => { done = true; });
  await flush();
  assert.equal(done, false);
  kit.timers.advance(500);
  await p;
  assert.equal(done, true);
  assert.equal(kit.timers.pendingCount(), 0);
});

test('9g. no unhandled rejection can escape: handlers that reject AFTER cancellation are swallowed', async () => {
  const kit = makeKit();
  const rejections = [];
  const onRej = (e) => rejections.push(e);
  process.on('unhandledRejection', onRej);
  try {
    await play(kit, out({ effects: [http(1), http(2, { item: 11 })] }));
    kit.runner.send([{ type: 'cancel_run', run: 1 }]);
    kit.svc.net.calls[0].reject(new Error('socket reset after abort'));
    kit.svc.net.calls[1].reject(new Error('socket reset after abort'));
    await kit.drain();
    await sleep(20);
  } finally {
    process.off('unhandledRejection', onRej);
  }
  assert.deepEqual(rejections, []);
});
