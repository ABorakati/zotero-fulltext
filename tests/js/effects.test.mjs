// ZFT.effects.Runner: the step loop, concurrency, barrier, cancellation, effect dispatch and completion shapes.
// The engine is replaced by a scripted fake host (canned StepOuts); every Zotero-facing service is a controllable fake.
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
  const { ZFT } = loadPlugin({ files: ['util.js', 'bodies.js', 'effects.js'], globals: { Zotero: zotero } });
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

// ---------------------------------------------------------------------------------------------------------------
test('exports: EFFECT_TYPES lists every proto.rs effect the runner understands', () => {
  const { ZFT } = makeKit();
  assert.deepEqual([...ZFT.effects.EFFECT_TYPES].sort(), [
    'browser_load', 'cancel', 'confirm', 'http', 'log', 'login_update', 'login_window', 'persist', 'progress', 'release',
    'run_finished', 'run_translators', 'save_pdf', 'trace', 'zotero_oa',
  ]);
  assert.equal(ZFT.effects.COMPLETION_BATCH_MS, 0);
  assert.equal(ZFT.effects.MAX_STEPS_PER_PUMP, 64);
});

test('1. concurrency: every http effect of one StepOut starts before any completes; completions arriving together share ONE step', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), http(2, { item: 11 }), http(3, { item: 12 })] }));
  assert.equal(kit.host.steps.length, 1);
  assert.equal(kit.svc.net.calls.length, 3, 'all three requests are in flight at once');
  assert.deepEqual(plain(kit.host.steps[0].commands), [CONFIGURE]);
  assert.equal(kit.runner.stats().ops, 3);

  // They finish in the same tick, in a different order than they started.
  kit.svc.net.calls[2].resolve(rawOk());
  kit.svc.net.calls[0].resolve(rawOk());
  kit.svc.net.calls[1].resolve(rawOk());
  await kit.drain();
  assert.equal(kit.host.steps.length, 2, 'N simultaneous completions cost one step');
  assert.deepEqual(completionsOf(kit, 1).map((c) => c.op).sort(), [1, 2, 3]);
  assert.equal(kit.runner.stats().ops, 0);
  await kit.runner.whenQuiet();
});

test('1b. staggered completions of one burst are folded; a later one gets its own step', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), http(2)] }));
  kit.svc.net.calls[0].resolve(rawOk());
  await flush();                         // handler finished, pump scheduled but the batch timer has not fired
  assert.equal(kit.host.steps.length, 1, 'the step waits for the batch window');
  kit.svc.net.calls[1].resolve(rawOk());
  await flush();
  await kit.drain();
  assert.equal(kit.host.steps.length, 2);
  assert.deepEqual(completionsOf(kit, 1).map((c) => c.op).sort(), [1, 2]);
});

test('2. more:true keeps stepping; next_timer arms exactly ONE timer (virtual deadline) and re-arms it', async () => {
  const kit = makeKit({ realTimers: false });
  kit.host.queue.push(out({ more: true }), out({ more: true }), out({ next_timer: 1500 }));
  kit.runner.send([CONFIGURE]);
  kit.timers.advance(0);                 // fire the batch timer: the pump starts
  for (let i = 0; i < 50 && kit.host.steps.length < 3; i++) await sleep(2);   // `more` yields real macrotasks
  await flush();
  assert.equal(kit.host.steps.length, 3, 'three steps in one pump');
  assert.deepEqual(kit.host.steps.map((s) => s.now), [1000, 1000, 1000]);
  assert.equal(kit.timers.pendingCount(), 1, 'one timer for next_timer');

  kit.timers.advance(499);
  await flush();
  assert.equal(kit.host.steps.length, 3, 'not yet at 1500');
  kit.host.queue.push(out({ next_timer: 3000 }));
  kit.timers.advance(1);
  await kit.drain();
  assert.equal(kit.host.steps.length, 4, 'the deadline triggered a step');
  assert.equal(kit.host.steps[3].now, 1500);
  assert.equal(kit.timers.pendingCount(), 1, 're-armed, still exactly one');

  kit.runner.send([{ type: 'dump_state' }]);   // a command folds in and re-arms rather than adding a second timer
  kit.host.queue.push(out({ next_timer: 4000 }));
  await kit.drain();
  assert.equal(kit.timers.pendingCount(), 1);
  kit.host.queue.push(out({ next_timer: null }));
  kit.runner.send([{ type: 'dump_state' }]);
  await kit.drain();
  assert.equal(kit.timers.pendingCount(), 0, 'next_timer null clears the timer');
});

test('2b. a deadline already in the past fires with delay 0', async () => {
  const kit = makeKit();
  kit.host.queue.push(out({ next_timer: 10 }), out());
  kit.runner.send([CONFIGURE]);
  await kit.drain();
  assert.equal(kit.host.steps.length, 2, 'the overdue timer produced a second step');
});

test('2c. pump is single-flight: a re-entrant pump()/send() inside a step does not run a second step concurrently', async () => {
  const kit = makeKit();
  let depth = 0;
  let maxDepth = 0;
  const reenter = (stepIn) => {
    depth++;
    maxDepth = Math.max(maxDepth, depth);
    if (kit.host.steps.length === 1) {
      kit.runner.pump();                                   // re-entrant pump
      kit.runner.send([{ type: 'cancel_run', run: 9 }]);   // re-entrant send
    }
    depth--;
    return out();
  };
  kit.host.queue.push(reenter);
  kit.runner.send([CONFIGURE]);
  await kit.drain();
  assert.equal(maxDepth, 1);
  assert.equal(kit.host.steps.length, 2, 'the work added during the step ran as the next step');
  assert.deepEqual(plain(kit.host.steps[1].commands), [{ type: 'cancel_run', run: 9 }]);
});

test('2d. direct concurrent pump() calls share one promise and never overlap', async () => {
  const kit = makeKit();
  kit.host.queue.push(out({ more: true }), out());
  const a = kit.runner.pump();
  const b = kit.runner.pump();
  await Promise.all([a, b]);
  assert.equal(kit.host.steps.length, 2);
});

test('2e. MAX_STEPS_PER_PUMP: a never-idle engine yields to the event loop and the pump resumes later', async () => {
  const kit = makeKit();
  let n = 0;
  kit.host.step = (stepIn) => { kit.host.steps.push(plain(stepIn)); n++; return out({ more: n < 100 }); };
  await kit.runner.pump();
  assert.equal(kit.host.steps.length, kit.ZFT.effects.MAX_STEPS_PER_PUMP, 'one pump stops at the cap');
  assert.equal(kit.timers.pendingCount(), 1, 'and schedules its continuation');
  for (let i = 0; i < 60 && kit.host.steps.length < 100; i++) { kit.timers.advance(0); await sleep(3); }
  assert.equal(kit.host.steps.length, 100);
});

test('3. barrier: an http effect with barrier:true waits for every EARLIER persist write; others do not', async () => {
  const kit = makeKit();
  kit.state.hold = true;
  const persist = { type: 'persist', key: 'ledger', value: { committed: [], reserved: [{ id: 1, wall_ms: 1, bytes: 2 }] }, flush: true };
  await play(kit, out({ effects: [persist, http(1, { barrier: true, route: 'inst' }), http(2, { item: 11 })] }));
  assert.equal(kit.stateCalls.length, 1);
  assert.deepEqual({ key: kit.stateCalls[0].key, value: kit.stateCalls[0].value, opts: kit.stateCalls[0].opts, settled: kit.stateCalls[0].settled },
    { key: 'ledger', value: persist.value, opts: { flush: true }, settled: false });
  assert.deepEqual(kit.svc.net.calls.map((c) => c.args[0].url), [HTTP_REQ.url], 'only the non-barrier request started');
  assert.equal(kit.runner.stats().ops, 2);
  assert.equal(kit.runner.stats().persistsInFlight, 1);

  kit.stateCalls[0].resolve();
  await kit.drain();
  assert.equal(kit.svc.net.calls.length, 2, 'the barrier request started once the ledger was on disk');
  assert.equal(kit.stateCalls[0].settled, true);
  assert.equal(kit.runner.stats().persistsInFlight, 0);
});

test('3b. barrier only waits for persists dispatched BEFORE it; persist writes are serialised and failures do not break the chain', async () => {
  const kit = makeKit();
  kit.state.hold = true;
  const p = (n) => ({ type: 'persist', key: 'ledger', value: { n }, flush: n === 1 });
  await play(kit, out({ effects: [p(1), http(1, { barrier: true }), p(2)] }));
  assert.equal(kit.stateCalls.length, 1, 'the second write is queued behind the first');
  assert.equal(kit.svc.net.calls.length, 0);
  kit.stateCalls[0].reject(new Error('disk full'));   // a failed write is logged, the chain continues
  await kit.drain();
  assert.equal(kit.svc.net.calls.length, 1, 'barrier released: it did not wait for the later persist');
  assert.equal(kit.stateCalls.length, 2, 'the later persist ran after the failure');
  assert.ok(kit.zotero.debugLog.some(([, m]) => /persist ledger failed: Error: disk full/.test(m)));
  kit.stateCalls[1].resolve();
  await kit.drain();
  assert.equal(kit.runner.stats().persistsInFlight, 0);
});

test('3c. a barrier with no pending persist starts at once; browser_load honours the barrier too', async () => {
  const kit = makeKit();
  const browser = {
    type: 'browser_load', op: 5, run: 1, item: 10, route: 'inst', barrier: true,
    req: { url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=x', ua: 'plain', capture_pdf: true, want_page: true, settle_ms: 3000, timeout_ms: 60000, hold_while: [] },
    inspect: { expect: 'page', requested: {}, route: 'inst' },
  };
  await play(kit, out({ effects: [http(1, { barrier: true }), browser] }));
  assert.equal(kit.svc.net.calls.length, 1);
  assert.equal(kit.svc.browser.calls.length, 1);
});

test('4a. cancel{op}: aborts the handler scope, drops its completion, frees the op', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), http(2, { item: 11 })] }));
  const [c1, c2] = kit.svc.net.calls;
  assert.equal(c1.scope.cancelled, false);
  await play(kit, out({ effects: [{ type: 'cancel', op: 1 }] }), []);
  assert.equal(c1.scope.cancelled, true);
  assert.equal(c1.scope.reason, 'engine');
  assert.equal(c2.scope.cancelled, false);
  assert.equal(kit.runner.stats().ops, 1);
  c2.resolve(rawOk());
  await kit.drain();
  const allCompletions = kit.host.steps.flatMap((s) => s.completions.map((c) => c.op));
  assert.deepEqual(allCompletions, [2], 'no completion was sent for the op the engine cancelled');
  await play(kit, out({ effects: [{ type: 'cancel', op: 1 }, { type: 'cancel', op: 99999 }] }), []);   // unknown / finished: harmless
});

test('4a2. a result that arrives after the engine cancelled the op is dropped and its body released', async () => {
  const kit = makeKit();
  const late = [];
  kit.deps.net.request = () => new Promise((resolve) => late.push(resolve));   // ignores the scope on purpose
  await play(kit, out({ effects: [http(1, { retain: 'page' })] }));
  await play(kit, out({ effects: [{ type: 'cancel', op: 1 }] }), []);
  assert.equal(kit.runner.stats().ops, 0, 'the op is gone as soon as the engine cancels it');
  late[0](rawOk());
  await kit.drain();
  assert.equal(kit.store.stats().count, 0, 'nothing leaked');
  assert.deepEqual(kit.host.steps.flatMap((s) => s.completions), []);
});

test('4b. send([cancel_run]) cancels the run scope immediately, answers its ops "cancelled" and still forwards the command', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1, { run: 5, item: 10 }), http(2, { run: 5, item: 11 }), http(3, { run: 6, item: 10 })] }));
  const [a, b, c] = kit.svc.net.calls;
  kit.runner.send([{ type: 'cancel_run', run: 5 }]);
  assert.equal(a.scope.cancelled, true, 'cancelled in the same tick');
  assert.equal(b.scope.cancelled, true);
  assert.equal(c.scope.cancelled, false, 'another run is untouched');
  assert.equal(a.scope.reason, 'user');
  assert.equal(kit.runner.stats().pendingCompletions, 2);
  assert.equal(kit.runner.stats().pendingCommands, 1);
  await kit.drain();
  const step = kit.host.steps[1];
  assert.deepEqual(plain(step.commands), [{ type: 'cancel_run', run: 5 }]);
  assert.deepEqual(step.completions.map((x) => [x.op, x.err.kind]).sort(), [[1, 'cancelled'], [2, 'cancelled']]);
  assert.equal(kit.runner.stats().ops, 1);
  c.resolve(rawOk());
  await kit.drain();
  assert.deepEqual(kit.host.steps[2].completions.map((x) => x.op), [3]);
});

test('4c. a cancelled run stays cancelled: a late effect for it is answered "cancelled" without starting work', async () => {
  const kit = makeKit();
  kit.runner.send([{ type: 'cancel_run', run: 5 }]);
  await kit.drain();
  await play(kit, out({ effects: [http(7, { run: 5, item: 10 })] }), []);
  assert.equal(kit.svc.net.calls.length, 0, 'the request never went out');
  assert.deepEqual(kit.host.steps.at(-1).completions.map((x) => [x.op, x.err.kind]), [[7, 'cancelled']]);
});

test('4d. cancel_item cancels only that item; the shared login window (run scope) survives it', async () => {
  const kit = makeKit();
  const login = { type: 'login_window', op: 9, run: 1, item: 10, req: { kind: 'login', realm: 'openathens', entry_url: 'https://go.openathens.net/redirector/kcl.ac.uk', item_title: 't', waiting: 2, timeout_ms: 300000, capture_pdf: true } };
  await play(kit, out({ effects: [http(1, { item: 10 }), http(2, { item: 11 }), login] }));
  kit.runner.send([{ type: 'cancel_item', run: 1, item: 10 }]);
  assert.equal(kit.svc.net.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.net.calls[1].scope.cancelled, false);
  assert.equal(kit.svc.loginOpen.calls[0].scope.cancelled, false, 'cancelling the item that opened the window must not close it for the others');
  await kit.drain();
  assert.deepEqual(plain(kit.host.steps[1].commands), [{ type: 'cancel_item', run: 1, item: 10 }]);
  kit.runner.send([{ type: 'cancel_run', run: 1 }]);
  assert.equal(kit.svc.loginOpen.calls[0].scope.cancelled, true, 'cancel_run closes it');
});

test('4e. a save that has started is not cancelled by the engine, cancel_item or cancel_run', async () => {
  const kit = makeKit();
  const save = { type: 'save_pdf', op: 4, run: 1, item: 10, req: { item: 10, body: { handle: 1, len: 5 }, trim_prefix: 0, version: 'published', url_to_store: 'https://doi.org/10.1/x', route: 'oa', source: 'pmc_s3' } };
  await play(kit, out({ effects: [save] }));
  const call = kit.svc.save.calls[0];
  await play(kit, out({ effects: [{ type: 'cancel', op: 4 }] }), [{ type: 'cancel_item', run: 1, item: 10 }, { type: 'cancel_run', run: 1 }]);
  assert.equal(call.scope.cancelled, false);
  assert.equal(kit.runner.stats().ops, 1);
  call.resolve({ attachment_key: '1/ABCD', filename: 'x.pdf' });
  await kit.drain();
  assert.deepEqual(plain(kit.host.steps.at(-1).completions), [{ op: 4, ok: { attachment_key: '1/ABCD', filename: 'x.pdf' } }]);
});

test('4f. an engine Cancel in the same StepOut as the effect that creates the op prevents the work', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), { type: 'cancel', op: 1 }] }));
  assert.equal(kit.svc.net.calls.length, 0);
  assert.equal(kit.runner.stats().ops, 0);
  assert.deepEqual(kit.host.steps.flatMap((s) => s.completions), []);
});

// ---------------------------------------------------------------------------------------------------------------
// 5. every effect type, table driven: service arguments and completion shape
// ---------------------------------------------------------------------------------------------------------------
const LOGIN_REQ = { kind: 'login', realm: 'openathens', entry_url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=x', item_title: 'A title', waiting: 3, timeout_ms: 300000, capture_pdf: true };
const BROWSER_REQ = { url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=x', ua: 'plain', capture_pdf: true, want_page: true, settle_ms: 3000, timeout_ms: 60000, hold_while: ['openathens_redirector'] };
const SAVE_REQ = { item: 10, body: { handle: 9, len: 1834567 }, trim_prefix: 0, version: 'published', url_to_store: 'https://doi.org/10.1/x', route: 'inst', source: 'lww' };
const PAGE_SPEC = { expect: 'page', requested: { doi: '10.1/x' }, route: 'inst' };
const CASES = [
  {
    name: 'http, memory sink',
    effect: http(10),
    service: 'net',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), HTTP_REQ);
      assert.deepEqual(Object.keys(call.args[1]).sort(), ['scope', 'sink']);
      assert.equal(call.args[1].sink, 'memory');
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => rawOk({ ms: 40.4 }),
    completion: { op: 10, ok: { status: 200, final_url: 'https://api.openalex.org/works/doi:10.1371/x', headers: { 'content-type': 'application/json' }, bytes: 7, ms: 40, body: null, inspect: { expect: 'none' } } },
    after: (kit) => {
      assert.deepEqual(kit.host.inspected, [{
        meta: { spec: API_SPEC, resp: { status: 200, final_url: 'https://api.openalex.org/works/doi:10.1371/x', headers: { 'content-type': 'application/json' }, body_len: 7, truncated: false } },
        bytes: [...enc.encode('{"a":1}')],
      }]);
      assert.equal(kit.store.stats().count, 0);
    },
  },
  {
    name: 'http, retain page keeps the body in memory',
    effect: http(10, { retain: 'page' }),
    service: 'net',
    result: () => rawOk(),
    completion: { op: 10, ok: { status: 200, final_url: 'https://api.openalex.org/works/doi:10.1371/x', headers: { 'content-type': 'application/json' }, bytes: 7, ms: 40, body: { handle: 1, len: 7 }, inspect: { expect: 'none' } } },
    after: (kit) => {
      assert.deepEqual([...kit.store.get(1).mem], [...enc.encode('{"a":1}')]);
      assert.equal(kit.store.get(1).run, 1);
    },
  },
  {
    name: 'browser_load, page',
    effect: { type: 'browser_load', op: 10, run: 1, item: 10, route: 'inst', req: BROWSER_REQ, inspect: PAGE_SPEC },
    service: 'browser',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), BROWSER_REQ);
      assert.equal(call.args[1].host, kit.host);
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => ({ final_url: 'https://journals.lww.com/x', status: 200, ms: 8100, pdf: null, page_html: '<html>café</html>', url_trail: ['go.openathens.net/redirector/kcl.ac.uk'] }),
    completion: { op: 10, ok: { final_url: 'https://journals.lww.com/x', status: 200, ms: 8100, pdf: null, page: { handle: 1, len: 18 }, url_trail: ['go.openathens.net/redirector/kcl.ac.uk'], inspect: { expect: 'none' } } },
    after: (kit) => {
      assert.deepEqual(kit.host.inspected[0].meta, {
        spec: PAGE_SPEC,
        resp: { status: 200, final_url: 'https://journals.lww.com/x', headers: { 'content-type': 'text/html; charset=utf-8' }, body_len: 18, truncated: false },
      });
      assert.equal(new TextDecoder().decode(Uint8Array.from(kit.host.inspected[0].bytes)), '<html>café</html>');
      assert.equal(kit.store.get(1).kind, 'page');
    },
  },
  {
    name: 'browser_load, nothing readable (no page, no pdf)',
    effect: { type: 'browser_load', op: 10, run: 1, item: 10, route: 'inst', req: { ...BROWSER_REQ, want_page: false }, inspect: PAGE_SPEC },
    service: 'browser',
    result: () => ({ final_url: 'https://login.microsoftonline.com/x', status: null, ms: 900, pdf: null, page_html: null, url_trail: [] }),
    completion: { op: 10, ok: { final_url: 'https://login.microsoftonline.com/x', status: null, ms: 900, pdf: null, page: null, url_trail: [], inspect: { expect: 'none' } } },
    after: (kit) => {
      assert.equal(kit.host.inspected[0].meta.resp.status, 200, 'the probe needs a status; none was observed');
      assert.deepEqual(kit.host.inspected[0].bytes, []);
    },
  },
  {
    name: 'browser_load, captured PDF is stored in a slot and inspected as a pdf',
    effect: { type: 'browser_load', op: 10, run: 1, item: 10, route: 'inst', req: BROWSER_REQ, inspect: PAGE_SPEC },
    service: 'browser',
    result: () => ({ final_url: 'https://journals.lww.com/x.pdf', status: 200, ms: 5000, pdf: new Blob([seq(10000)]), page_html: null, url_trail: ['a/b'] }),
    completion: { op: 10, ok: { final_url: 'https://journals.lww.com/x.pdf', status: 200, ms: 5000, pdf: { handle: 1, len: 10000 }, page: null, url_trail: ['a/b'], inspect: { expect: 'none' } } },
    after: (kit) => {
      const meta = kit.host.inspected[0].meta;
      assert.deepEqual(meta.spec, { expect: 'pdf', requested: { doi: '10.1/x' }, route: 'inst' });
      assert.equal(meta.resp.body_len, 10000);
      assert.equal(meta.resp.head_len, 4096);
      assert.deepEqual(meta.resp.headers, { 'content-type': 'application/pdf' });
      assert.equal(kit.host.inspected[0].bytes.length, 4096 + 2048);
      assert.equal(kit.store.get(1).kind, 'pdf');
      assert.deepEqual(kit.registered, ['/storage/tmp-1']);
    },
  },
  {
    name: 'login_window',
    effect: { type: 'login_window', op: 10, run: 1, item: 10, req: LOGIN_REQ },
    service: 'loginOpen',
    args: (kit, call) => {
      assert.equal(call.args[0].op, 10);
      assert.deepEqual(plain(call.args[0].req), LOGIN_REQ);
      assert.equal(call.args[1].host, kit.host);
      assert.equal(call.args[1].bodies, kit.store);
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => ({ result: 'landed', final_url: 'https://journals.lww.com/x', inspect: { expect: 'none' } }),
    completion: { op: 10, ok: { result: 'landed', final_url: 'https://journals.lww.com/x', inspect: { expect: 'none' } } },
  },
  {
    name: 'login_window result with a body keeps the BodyRef',
    effect: { type: 'login_window', op: 10, run: 1, item: 10, req: LOGIN_REQ },
    service: 'loginOpen',
    result: (kit) => { const ref = kit.store.putMemory(enc.encode('<html/>'), { run: 1 }); return { result: 'landed', final_url: 'https://x', page: ref, inspect: { expect: 'none' } }; },
    completion: { op: 10, ok: { result: 'landed', final_url: 'https://x', page: { handle: 1, len: 7 }, inspect: { expect: 'none' } } },
    after: (kit) => assert.ok(kit.store.get(1), 'still owned by the engine until it releases it'),
  },
  {
    name: 'run_translators',
    effect: { type: 'run_translators', op: 10, run: 1, item: 10, page: { handle: 4, len: 1000 }, url: 'https://journals.lww.com/x' },
    service: 'translators',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), { page: { handle: 4, len: 1000 }, url: 'https://journals.lww.com/x' });
      assert.equal(call.args[1].bodies, kit.store);
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => ({ found: { url: 'https://x/a.pdf', mime: 'application/pdf', title: 'Full Text PDF' }, ms: 120 }),
    completion: { op: 10, ok: { found: { url: 'https://x/a.pdf', mime: 'application/pdf', title: 'Full Text PDF' }, ms: 120 } },
  },
  {
    name: 'zotero_oa passes the hit ARRAY through',
    effect: { type: 'zotero_oa', op: 10, run: 1, item: 10, doi: '10.1371/x' },
    service: 'zoteroOa',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), { doi: '10.1371/x' });
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => [{ url: 'https://x/a.pdf', page_url: null, version: 'publishedVersion' }],
    completion: { op: 10, ok: [{ url: 'https://x/a.pdf', page_url: null, version: 'publishedVersion' }] },
  },
  {
    name: 'zotero_oa with nothing found is an empty array, not an error',
    effect: { type: 'zotero_oa', op: 10, run: 1, item: 10, doi: '10.1371/x' },
    service: 'zoteroOa',
    result: () => [],
    completion: { op: 10, ok: [] },
  },
  {
    name: 'zotero_oa with a non-array answer is treated as no hits',
    effect: { type: 'zotero_oa', op: 10, run: 1, item: 10, doi: '10.1371/x' },
    service: 'zoteroOa',
    result: () => null,
    completion: { op: 10, ok: [] },
  },
  {
    name: 'save_pdf',
    effect: { type: 'save_pdf', op: 10, run: 1, item: 10, req: SAVE_REQ },
    service: 'save',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), SAVE_REQ);
      assert.equal(call.args[1].bodies, kit.store);
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => ({ attachment_key: '1/ABCD1234', filename: 'Smith 2022.pdf' }),
    completion: { op: 10, ok: { attachment_key: '1/ABCD1234', filename: 'Smith 2022.pdf' } },
  },
  {
    name: 'confirm',
    effect: { type: 'confirm', op: 10, run: 1, req: { kind: 'large_batch', n: 34, cap: 20, library: '' } },
    service: 'confirm',
    args: (kit, call) => {
      assert.deepEqual(plain(call.args[0]), { kind: 'large_batch', n: 34, cap: 20, library: '' });
      assert.equal(typeof call.args[1].scope.cancel, 'function');
    },
    result: () => ({ choice: 'oa_only' }),
    completion: { op: 10, ok: { choice: 'oa_only' } },
  },
];
for (const c of CASES) {
  test(`5. ${c.name}`, async () => {
    const kit = makeKit();
    await play(kit, out({ effects: [c.effect] }));
    const call = kit.svc[c.service].calls[0];
    assert.ok(call, 'the service was called');
    if (c.args) c.args(kit, call);
    call.resolve(await c.result(kit));
    await kit.drain();
    assert.equal(kit.host.steps.length, 2);
    assert.deepEqual(plain(kit.host.steps[1].completions), [c.completion]);
    if (c.after) c.after(kit);
    assert.equal(kit.runner.stats().ops, 0);
  });
}

test('5b. a rejected service becomes {op, err}: OpError kinds pass through, anything else is "internal"', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [
    { type: 'zotero_oa', op: 1, run: 1, item: 10, doi: 'a' }, { type: 'zotero_oa', op: 2, run: 1, item: 11, doi: 'b' },
    { type: 'zotero_oa', op: 3, run: 1, item: 12, doi: 'c' },
  ] }));
  const [a, b, c] = kit.svc.zoteroOa.calls;
  a.reject(new kit.ZFT.util.OpError('timeout', 'too slow', { afterMs: 9000, status: 504 }));
  b.reject(new TypeError('x is not a function'));
  c.reject('a bare string');
  await kit.drain();
  const byOp = Object.fromEntries(kit.host.steps[1].completions.map((x) => [x.op, x.err]));
  assert.deepEqual(plain(byOp[1]), { kind: 'timeout', message: 'too slow', after_ms: 9000, status: 504 });
  assert.equal(byOp[2].kind, 'internal');
  assert.match(byOp[2].message, /TypeError: x is not a function/);
  assert.equal(byOp[3].kind, 'internal');
});

test('5c. login_update: the FRESH op is completed at once with {}; the open window gets the new count', async () => {
  const kit = makeKit();
  const login = { type: 'login_window', op: 9, run: 1, item: 10, req: LOGIN_REQ };
  await play(kit, out({ effects: [login] }));
  await play(kit, out({ effects: [{ type: 'login_update', op: 77, waiting: 4 }] }), []);
  assert.deepEqual(kit.loginUpdates, [[9, 4]], 'routed to the window op, not the fresh id');
  assert.deepEqual(plain(kit.host.steps.at(-1).completions), [{ op: 77, ok: {} }], 'completed at once, delivered with the next step');
  assert.equal(kit.runner.stats().ops, 1, 'the window is still open');
  assert.equal(kit.svc.loginOpen.calls[0].scope.cancelled, false);
});

test('5d. login_update with no window open is still completed; one that reuses the window op id never completes the window', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [{ type: 'login_update', op: 5, waiting: 1 }] }));
  await kit.drain();
  assert.deepEqual(kit.loginUpdates, []);
  assert.deepEqual(plain(kit.host.steps.at(-1).completions), [{ op: 5, ok: {} }]);

  const k2 = makeKit();
  await play(k2, out({ effects: [{ type: 'login_window', op: 9, run: 1, item: 10, req: LOGIN_REQ }] }));
  await play(k2, out({ effects: [{ type: 'login_update', op: 9, waiting: 2 }] }), []);
  await k2.drain();
  assert.deepEqual(k2.loginUpdates, [[9, 2]]);
  assert.deepEqual(k2.host.steps.flatMap((s) => s.completions), [], 'the window must not be completed by an update');
  assert.equal(k2.runner.stats().ops, 1);
});

test('5e. fire-and-forget effects reach their services and produce no completion', async () => {
  const kit = makeKit({ runner: { trace: false } });
  const report = { run: 1, counts: { saved: 1, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: 0, failed: 0 }, items: [], ledger: { hour: 0, hour_cap: 30, day: 0, day_cap: 80, mb_hour: 0, mb_hour_cap: 300 } };
  const row = { state: 'working', l10n: 'zotero-fulltext-status-searching-oa', args: {}, route: 'oa' };
  const held = kit.store.putMemory(enc.encode('page'), { run: 1 });
  const other = kit.store.putMemory(enc.encode('other'), { run: 2 });
  const single = kit.store.putMemory(enc.encode('x'), { run: 3 });
  await play(kit, out({ effects: [
    { type: 'start-ignored-nothing' },
    { type: 'progress', run: 1, item: 10, row },
    { type: 'persist', key: 'halts', value: { inst_until: 5 }, flush: false },
    { type: 'release', handle: single.handle },
    { type: 'log', level: 'warn', msg: 'something odd' },
    { type: 'log', level: 'debug', msg: 'quiet detail' },
    { type: 'trace', run: 1, item: 10, ev: { k: 1 } },
    { type: 'run_finished', run: 1, report },
  ] }));
  assert.deepEqual(kit.progress.updates, [[1, 10, row]]);
  assert.deepEqual(kit.stateCalls.map((c) => [c.key, c.value, c.opts]), [['halts', { inst_until: 5 }, { flush: false }]]);
  assert.equal(kit.store.get(single.handle), null, 'release frees the body');
  assert.equal(kit.store.get(held.handle), null, 'run_finished released the bodies of that run');
  assert.ok(kit.store.get(other.handle), 'but not those of another run');
  assert.deepEqual(kit.progress.finishes, [[1, report]]);
  assert.deepEqual(kit.finished, [report]);
  const log = kit.zotero.debugLog.map(([, m]) => m);
  assert.ok(log.some((m) => /engine: something odd/.test(m)));
  assert.ok(!log.some((m) => /quiet detail/.test(m)), 'debug lines only with trace on');
  assert.deepEqual(plain(kit.runner.traceLog()), [], 'trace events are dropped when tracing is off');
  assert.ok(log.some((m) => /unknown effect type start-ignored-nothing/.test(m)));
  assert.equal(kit.host.steps.length, 1, 'no completion, so no second step');
});

test('5f. trace on: a ring buffer of TRACE_RING events and debug log lines', async () => {
  const kit = makeKit({ runner: { trace: true } });
  const events = [];
  for (let i = 0; i < kit.ZFT.effects.TRACE_RING + 25; i++) events.push({ type: 'trace', run: 1, item: 10, ev: { i } });
  await play(kit, out({ effects: [...events, { type: 'log', level: 'debug', msg: 'visible detail' }] }));
  const ring = plain(kit.runner.traceLog());
  assert.equal(ring.length, kit.ZFT.effects.TRACE_RING);
  assert.deepEqual(ring[0], { run: 1, item: 10, ev: { i: 25 } }, 'oldest events were dropped');
  assert.deepEqual(ring.at(-1), { run: 1, item: 10, ev: { i: kit.ZFT.effects.TRACE_RING + 24 } });
  assert.ok(kit.zotero.debugLog.some(([, m]) => /visible detail/.test(m)));
});

test('5g. an unknown effect type that carries an op is answered "unsupported" so the engine cannot hang', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [{ type: 'teleport', op: 12 }] }));
  await kit.drain();
  assert.deepEqual(plain(kit.host.steps[1].completions), [{ op: 12, err: { kind: 'unsupported', message: 'unknown effect teleport' } }]);
});

test('5h. a throwing UI callback (progress.update) never breaks the loop', async () => {
  const kit = makeKit();
  kit.progress.update = () => { throw new Error('window closed'); };
  await play(kit, out({ effects: [{ type: 'progress', run: 1, item: 10, row: {} }, http(1)] }));
  assert.equal(kit.svc.net.calls.length, 1, 'later effects still ran');
  assert.ok(kit.zotero.debugLog.some(([, m]) => /progress\.update failed: Error: window closed/.test(m)));
});

// ---------------------------------------------------------------------------------------------------------------
// 6. the http file sink
// ---------------------------------------------------------------------------------------------------------------
const PDF_URL = 'https://europepmc.org/articles/PMC1/pdf';
async function fileNet(kit, bytes, extra = {}) {
  // What ZFT.net.request does for the file sink: write the body to ctx.path, answer with the RawResponse.
  kit.deps.net.request = async (req, ctx) => {
    kit.sunk = { req, ctx };
    await kit.fsx.IOUtils.write(ctx.path, bytes);
    return {
      status: 200, final_url: PDF_URL, headers: { 'content-type': 'application/pdf' }, bytes: bytes.length, ms: 20,
      body: null, path: ctx.path, errorBody: null, ...extra,
    };
  };
}

test('6a. file sink, retain pdf: slot allocated and registered, head+tail staged, BodyRef points at the file', async () => {
  const kit = makeKit();
  await fileNet(kit, seq(50000));
  await play(kit, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC })] }));
  await kit.drain();
  assert.equal(kit.sunk.ctx.sink, 'file');
  assert.equal(kit.sunk.ctx.path, '/storage/tmp-1/file.tmp');
  assert.deepEqual(kit.registered, ['/storage/tmp-1'], 'registered before use');
  const insp = kit.host.inspected[0];
  assert.deepEqual(insp.meta.spec, PDF_SPEC);
  assert.deepEqual(insp.meta.resp, { status: 200, final_url: PDF_URL, headers: { 'content-type': 'application/pdf' }, body_len: 50000, head_len: 4096, truncated: false });
  assert.equal(insp.bytes.length, 4096 + 2048, 'only head and tail reach the inspector');
  assert.deepEqual(insp.bytes.slice(0, 4096), [...seq(50000).subarray(0, 4096)]);
  assert.deepEqual(insp.bytes.slice(4096), [...seq(50000).subarray(50000 - 2048)]);
  assert.deepEqual(plain(kit.host.steps[1].completions[0].ok.body), { handle: 1, len: 50000 });
  assert.equal(kit.store.pathOf(1), '/storage/tmp-1/file.tmp');
  assert.ok(kit.fsx.files.has('/storage/tmp-1/file.tmp'), 'the file is kept for the save');
  assert.deepEqual(kit.registered, ['/storage/tmp-1']);
});

test('6b. file sink without retain (a pdf was only expected): the verdict is returned, the slot is removed', async () => {
  const kit = makeKit();
  await fileNet(kit, seq(3000));
  await play(kit, out({ effects: [http(1, { retain: 'none', inspect: PDF_SPEC })] }));
  await kit.drain();
  assert.equal(kit.sunk.ctx.sink, 'file', 'expect:pdf selects the file sink on its own');
  assert.equal(kit.host.inspected[0].bytes.length, 3000, 'a short file is staged whole');
  assert.equal(kit.host.inspected[0].meta.resp.head_len, undefined, 'no head_len for a complete body');
  assert.equal(kit.host.steps[1].completions[0].ok.body, null);
  assert.ok(!kit.fsx.dirs.has('/storage/tmp-1'));
  assert.deepEqual(kit.registered, [], 'unregistered again');
  assert.equal(kit.store.stats().count, 0);
});

test('6c. non-2xx on the file sink: the error page is what the inspector sees, no file is kept', async () => {
  const kit = makeKit();
  const page = enc.encode('<html>Just a moment... cloudflare</html>');
  kit.deps.net.request = async (req, ctx) => ({
    status: 403, final_url: PDF_URL, headers: { 'content-type': 'text/html', 'cf-mitigated': 'challenge' }, bytes: 0, ms: 30,
    body: null, path: null, errorBody: page,
  });
  await play(kit, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC })] }));
  await kit.drain();
  const insp = kit.host.inspected[0];
  assert.deepEqual(insp.bytes, [...page]);
  assert.deepEqual(insp.meta.resp, { status: 403, final_url: PDF_URL, headers: { 'content-type': 'text/html', 'cf-mitigated': 'challenge' }, body_len: page.length, truncated: false });
  const ok = kit.host.steps[1].completions[0].ok;
  assert.equal(ok.status, 403);
  assert.equal(ok.body, null, 'retain:pdf but no pdf arrived');
  assert.deepEqual(kit.registered, []);
  assert.equal(kit.store.stats().count, 0);
});

test('6d. a failed or cancelled file request removes its slot', async () => {
  const kit = makeKit();
  kit.deps.net.request = async () => { throw new kit.ZFT.util.OpError('timeout', 'slow host'); };
  await play(kit, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC })] }));
  await kit.drain();
  assert.equal(kit.host.steps[1].completions[0].err.kind, 'timeout');
  assert.deepEqual(kit.registered, []);
  assert.ok(!kit.fsx.dirs.has('/storage/tmp-1'));

  const k2 = makeKit();
  await play(k2, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC })] }));
  assert.equal(k2.svc.net.calls.length, 1);
  assert.deepEqual(k2.registered, ['/storage/tmp-1']);
  await play(k2, out({ effects: [{ type: 'cancel', op: 1 }] }), []);
  await k2.drain();
  assert.deepEqual(k2.registered, [], 'cancelling mid-download cleans the slot');
  assert.ok(!k2.fsx.dirs.has('/storage/tmp-1'));
});

test('6e. a result that arrives after the scope was cancelled never becomes a body', async () => {
  const kit = makeKit();
  let finish;
  kit.deps.net.request = (req, ctx) => new Promise((resolve) => {
    finish = async () => { await kit.fsx.IOUtils.write(ctx.path, seq(9000)); resolve({ status: 200, final_url: PDF_URL, headers: {}, bytes: 9000, ms: 1, body: null, path: ctx.path, errorBody: null }); };
  });
  await play(kit, out({ effects: [http(1, { retain: 'pdf', inspect: PDF_SPEC })] }));
  kit.runner.send([{ type: 'cancel_run', run: 1 }]);
  await finish();
  await kit.drain();
  assert.equal(kit.store.stats().count, 0);
  assert.deepEqual(kit.registered, []);
  assert.equal(kit.host.steps.at(-1).completions[0].err.kind, 'cancelled');
});

test('6f. inspect never throws into the runner: a rejecting host.inspect becomes Inspection::Failed', async () => {
  const kit = makeKit();
  kit.host.inspect = async () => { throw new Error('worker exploded'); };
  await play(kit, out({ effects: [http(1)] }));
  kit.svc.net.calls[0].resolve(rawOk());
  await kit.drain();
  assert.deepEqual(plain(kit.host.steps[1].completions[0].ok.inspect), { expect: 'failed', reason: 'inspect_error' });
});

test('6g. retain:page on a body above the page cap is an error "too_large" for that op only', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1, { retain: 'page' }), http(2, { item: 11 })] }));
  kit.svc.net.calls[0].resolve(rawOk({ body: new Uint8Array(kit.ZFT.bodies.PAGE_MAX_BYTES + 1), bytes: kit.ZFT.bodies.PAGE_MAX_BYTES + 1 }));
  kit.svc.net.calls[1].resolve(rawOk());
  await kit.drain();
  const byOp = Object.fromEntries(kit.host.steps[1].completions.map((c) => [c.op, c]));
  assert.equal(byOp[1].err.kind, 'too_large');
  assert.equal(byOp[2].ok.status, 200);
});

// ---------------------------------------------------------------------------------------------------------------
// body ownership
// ---------------------------------------------------------------------------------------------------------------
test('7a. bodies in completions the engine IGNORES are released by the runner; bodies it accepted stay until it releases them', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1, { retain: 'page' }), http(2, { retain: 'page', item: 11 })] }));
  kit.svc.net.calls[0].resolve(rawOk());
  kit.svc.net.calls[1].resolve(rawOk());
  kit.host.queue.push(out({ ignored: [1] }));
  await kit.drain();
  const handles = Object.fromEntries(kit.host.steps[1].completions.map((c) => [c.op, c.ok.body.handle]));
  assert.equal(kit.store.get(handles[1]), null, 'ignored: the engine never owned it');
  assert.ok(kit.store.get(handles[2]), 'accepted: the engine owns it');
  await play(kit, out({ effects: [{ type: 'release', handle: handles[2] }] }), []);
  assert.equal(kit.store.get(handles[2]), null);
});

test('7b. a body made by an op the user cancelled meanwhile is released, not leaked', async () => {
  const kit = makeKit();
  let late;
  kit.deps.net.request = () => new Promise((resolve) => { late = () => resolve(rawOk()); });
  await play(kit, out({ effects: [http(1, { retain: 'page' })] }));
  kit.runner.send([{ type: 'cancel_run', run: 1 }]);
  late();
  await kit.drain();
  assert.equal(kit.store.stats().count, 0);
});

test('7c. run_finished cancels what is left of the run (windows, hidden browsers) and forgets the run', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), { type: 'login_window', op: 2, run: 1, item: 10, req: LOGIN_REQ }] }), [
    CONFIGURE, { type: 'start_run', run: 1, mode: 'full', interactive: true, items: [{ item: 10, key: '1/A' }], resume: [] },
  ]);
  assert.equal(kit.runner.stats().ops, 2);
  await play(kit, out({ effects: [{ type: 'run_finished', run: 1, report: { run: 1 } }] }), []);
  assert.equal(kit.svc.net.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.loginOpen.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.net.calls[0].scope.reason, 'run_finished');
  await kit.drain();
  assert.deepEqual(kit.host.steps.flatMap((s) => s.completions).filter((c) => c.err).map((c) => c.err.kind), ['cancelled', 'cancelled']);
});

// ---------------------------------------------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------------------------------------------
test('commands keep their order, bad input to send() is ignored, configure clears needsConfigure', async () => {
  const kit = makeKit();
  assert.equal(kit.runner.needsConfigure, true);
  kit.runner.send(null);
  kit.runner.send([null, {}, { type: 5 }]);
  assert.equal(kit.runner.stats().pendingCommands, 0);
  kit.runner.send([CONFIGURE]);
  assert.equal(kit.runner.needsConfigure, false);
  kit.runner.send([{ type: 'user_action', run: 1, kind: 'sign_in_now' }, { type: 'dump_state' }]);
  assert.equal(kit.runner.stats().pendingCommands, 3);
  await kit.drain();
  assert.deepEqual(kit.host.steps[0].commands.map((c) => c.type), ['configure', 'user_action', 'dump_state']);
  assert.equal(kit.host.steps.length, 1, 'commands sent in the same tick share one step');
  assert.equal(kit.host.steps[0].now, 1000);
  assert.equal(kit.host.steps[0].wall, 1_800_000_000_000);
});

test('stats(), whenQuiet() and lastStats follow the op lifecycle', async () => {
  const kit = makeKit();
  let quiet = false;
  await play(kit, out({ effects: [http(1)], stats: { ...STATS, tasks: 3, ops_inflight: 1 } }));
  kit.runner.whenQuiet().then(() => { quiet = true; });
  await kit.drain();
  assert.equal(quiet, false, 'an op is in flight');
  const s = kit.runner.stats();
  assert.deepEqual({ ops: s.ops, steps: s.steps, failed: s.failed, tasks: s.lastStats.tasks }, { ops: 1, steps: 1, failed: false, tasks: 3 });
  kit.svc.net.calls[0].resolve(rawOk());
  await kit.drain();
  assert.equal(quiet, true);
  assert.equal(kit.runner.stats().steps, 2);
  await kit.runner.whenQuiet();
});

test('dispose cancels every op, clears timers, ignores later sends and does not hang on obedient handlers', async () => {
  const kit = makeKit();
  await play(kit, out({ effects: [http(1), http(2, { item: 11 })], next_timer: 5000 }));
  assert.equal(kit.timers.pendingCount(), 1);
  await kit.runner.dispose();
  assert.equal(kit.svc.net.calls[0].scope.cancelled, true);
  assert.equal(kit.svc.net.calls[0].scope.reason, 'dispose');
  assert.equal(kit.timers.pendingCount(), 0);
  kit.runner.send([CONFIGURE]);
  await kit.drain();
  assert.equal(kit.host.steps.length, 1);
  assert.deepEqual(kit.host.steps.flatMap((s) => s.completions), []);
  await kit.runner.dispose();   // idempotent
});

test('real timers: the default timers dependency drives batching end to end', async () => {
  const kit = makeKit({ realTimers: true });
  kit.host.queue.push(out({ effects: [http(1)] }));
  kit.runner.send([CONFIGURE]);
  for (let i = 0; i < 50 && kit.svc.net.calls.length < 1; i++) await sleep(2);
  assert.equal(kit.svc.net.calls.length, 1);
  kit.svc.net.calls[0].resolve(rawOk());
  await kit.runner.whenQuiet();
  assert.equal(kit.host.steps.length, 2);
  assert.deepEqual(kit.host.steps[1].completions.map((c) => c.op), [1]);
  await kit.runner.dispose();
});
