// ZFT.wasmHost: marshalling, trap handling, load checks, URL classification and the pure-instance worker protocol.
// Part (a) runs against a hand-assembled stub module (built below), part (b) against the real plugin/content/fulltext.wasm
// (skipped with a message when `cargo xtask build` has not produced it), part (c) drives the worker protocol with fake
// workers (and with the real inspect-worker.js in an in-process "worker" global).
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { loadPlugin, plain, wasmBuilt, WASM_PATH, CONTENT } from './helpers/zft-env.mjs';
import { makeFakeTimers, makeFakeZotero } from './helpers/fakes.mjs';

// ---------------------------------------------------------------------------------------------------------------
// A minimal wasm module with the full ABI surface. It does no real work: fx_step / fx_inspect / fx_login_state ECHO the
// first buffer they receive as the result (so a test chooses the Envelope by choosing the input), fx_alloc is a bump
// allocator that grows memory, and exported globals record what happened. A mutable `trap_next` global makes the
// next fx_step / fx_inspect / fx_reset execute `unreachable` after setting `panicked` (fx_last_panic then returns
// {"code":"panic","message":"boom"}).
// ---------------------------------------------------------------------------------------------------------------
const uleb = (n) => { let v = BigInt(n); const o = []; do { let b = Number(v & 0x7fn); v >>= 7n; if (v) b |= 0x80; o.push(b); } while (v); return o; };
const sleb = (n) => {
  let v = BigInt(n); const o = [];
  for (;;) {
    const b = Number(v & 0x7fn); v >>= 7n;
    const done = (v === 0n && !(b & 0x40)) || (v === -1n && (b & 0x40) !== 0);
    o.push(done ? b : b | 0x80);
    if (done) break;
  }
  return o;
};
const str = (s) => { const b = [...Buffer.from(s)]; return [...uleb(b.length), ...b]; };
const vec = (items) => [...uleb(items.length), ...items.flat()];
const section = (id, bytes) => [id, ...uleb(bytes.length), ...bytes];
const I32 = 0x7f;
const I64 = 0x7e;
const fnType = (params, results) => [0x60, ...uleb(params.length), ...params, ...uleb(results.length), ...results];
const G = (i) => [0x23, i];
const GS = (i) => [0x24, i];
const L = (i) => [0x20, i];
const LS = (i) => [0x21, i];
const C32 = (n) => [0x41, ...sleb(n)];
const PANIC_TEXT = JSON.stringify({ code: 'panic', message: 'boom' });
const PANIC_AT = 512;
// if (trap_next) { panicked = 1; unreachable }
const TRAP = [...G(1), 0x04, 0x40, ...C32(1), ...GS(2), 0x00, 0x0b];
// (ptr << 32) | len from two i32 locals
const PACK = (p, l) => [...L(p), 0xad, 0x42, 32, 0x86, ...L(l), 0xad, 0x84];

/** @param {{abi?: number, omit?: string[], trapInit?: number, importTable?: boolean, noMemory?: boolean}} [o] */
function buildStub(o = {}) {
  const abi = o.abi ?? 1;
  const fn = (locals, code) => { const body = [...vec(locals), ...code, 0x0b]; return [...uleb(body.length), ...body]; };
  const funcs = [
    fn([], C32(abi)),                                                                 // fx_abi_version
    fn([[1, I32]], [                                                                  // fx_alloc: bump + memory.grow
      ...G(0), ...LS(1), ...G(0), ...L(0), 0x6a, ...GS(0),
      ...G(0), 0x3f, 0, ...C32(16), 0x74, 0x4b,
      0x04, 0x40, ...G(0), 0x3f, 0, ...C32(16), 0x74, 0x6b, ...C32(16), 0x76, ...C32(1), 0x6a, 0x40, 0, 0x1a, 0x0b,
      ...L(1)]),
    fn([], [...G(3), ...C32(1), 0x6a, ...GS(3)]),                                     // fx_free: free_count++
    fn([], [...TRAP, ...L(3), ...GS(4), ...L(1), ...GS(5), ...PACK(0, 1)]),            // fx_inspect: echo meta
    fn([], [...TRAP, ...PACK(0, 1)]),                                                 // fx_step: echo input
    fn([], [...TRAP, ...G(7), ...C32(1), 0x6a, ...GS(7)]),                            // fx_reset: reset_count++
    fn([], [...G(2), 0x04, 0x7e, 0x42, ...sleb((BigInt(PANIC_AT) << 32n) | BigInt(PANIC_TEXT.length)), 0x05, 0x42, 0, 0x0b]),
    fn([], [...L(3), ...GS(6), ...PACK(0, 1)]),                                       // fx_login_state: echo url
  ];
  const types = [
    fnType([], [I32]), fnType([I32], [I32]), fnType([I32, I32], []), fnType([I32, I32, I32, I32], [I64]),
    fnType([I32, I32], [I64]), fnType([], []), fnType([], [I64]),
  ];
  const funcTypes = [0, 1, 2, 3, 4, 5, 6, 3];
  const funcNames = ['fx_abi_version', 'fx_alloc', 'fx_free', 'fx_inspect', 'fx_step', 'fx_reset', 'fx_last_panic', 'fx_login_state'];
  const globalNames = ['heap', 'trap_next', 'panicked', 'free_count', 'last_body_len', 'last_meta_len', 'last_html_len', 'reset_count'];
  const globalInit = [1024, o.trapInit ?? 0, 0, 0, 0, 0, 0, 0];
  const exportList = [];
  if (!o.noMemory) exportList.push([...str('memory'), 0x02, 0]);
  funcNames.forEach((n, i) => { if (!(o.omit || []).includes(n)) exportList.push([...str(n), 0x00, i]); });
  globalNames.forEach((n, i) => exportList.push([...str(n), 0x03, i]));
  return new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0,
    ...section(1, vec(types)),
    ...(o.importTable ? section(2, vec([[...str('env'), ...str('t'), 0x01, 0x70, 0x00, 0x01]])) : []),
    ...section(3, vec(funcTypes.map((t) => [t]))),
    ...section(5, vec([[0x00, 1]])),
    ...section(6, vec(globalInit.map((v) => [I32, 0x01, ...C32(v), 0x0b]))),
    ...section(7, vec(exportList)),
    ...section(10, vec(funcs)),
    ...section(11, vec([[0x00, ...C32(PANIC_AT), 0x0b, ...uleb(PANIC_TEXT.length), ...Buffer.from(PANIC_TEXT)]])),
  ]);
}

// ---------------------------------------------------------------------------------------------------------------
function setup(extraGlobals = {}) {
  const zotero = makeFakeZotero();
  const timers = makeFakeTimers();
  const { ZFT, ctx } = loadPlugin({ files: ['util.js', 'config.js', 'wasm-host.js'], globals: { Zotero: zotero, ...extraGlobals } });
  return { ZFT, ctx, zotero, timers };
}
const tick = () => new Promise((r) => setImmediate(r));
const NONE_META = { ok: { expect: 'none' } };   // the stub echoes the meta, so the meta IS the Envelope
const bytesOf = (n, fill = 1) => new Uint8Array(n).fill(fill);

async function stubHost(extra = {}, stubOpts = {}) {
  const env = setup(extra.globals || {});
  const host = await env.ZFT.wasmHost.load({
    bytes: buildStub(stubOpts), inspectInWorker: false, timers: env.timers, ...(extra.opts || {}),
  });
  return { ...env, host };
}

// ===============================================================================================================
// (a) stub module: load checks, marshalling, trap handling
// ===============================================================================================================
test('load: stub module gives two independent instances and reports the ABI', async () => {
  const { host } = await stubHost();
  assert.deepEqual(plain(host.diagnostics()), { abi: 1, engineTraps: 0, pureTraps: 0, pure: 'main', lastPanic: '' });
  assert.notEqual(host.engine.exports, host.pure.exports);
  assert.notEqual(host.engine.exports.memory, host.pure.exports.memory);
});

test('load: ABI mismatch, missing export, imports and bad bytes are rejected with specific messages', async () => {
  const { ZFT } = setup();
  const load = (bytes) => ZFT.wasmHost.load({ bytes, inspectInWorker: false });
  await assert.rejects(load(buildStub({ abi: 2 })), /wasm ABI mismatch: got 2/);
  await assert.rejects(load(buildStub({ omit: ['fx_login_state'] })), /missing exports: fx_login_state/);
  await assert.rejects(load(buildStub({ noMemory: true })), /missing exports: memory/);
  await assert.rejects(load(buildStub({ importTable: true })), /must import nothing, found: env\.t/);
  await assert.rejects(load(new Uint8Array([1, 2, 3])), /wasm compile failed/);
});

test('load: fetches the chrome:// URL by default and reports a failed fetch', async () => {
  const { ZFT } = setup();
  const urls = [];
  const stub = buildStub();
  const host = await ZFT.wasmHost.load({
    inspectInWorker: false,
    fetch: async (u) => { urls.push(u); return { ok: true, arrayBuffer: async () => stub.buffer.slice(0) }; },
  });
  assert.deepEqual(urls, ['chrome://zotero-fulltext/content/fulltext.wasm']);
  assert.equal(host.diagnostics().abi, 1);
  await assert.rejects(
    ZFT.wasmHost.load({ fetch: async () => ({ ok: false, status: 404 }) }),
    /cannot fetch chrome:\/\/zotero-fulltext\/content\/fulltext\.wasm: HTTP 404/,
  );
});

test('get(): concurrent callers share one load, a failed load is not cached, shutdown disposes', async () => {
  let calls = 0;
  let fail = true;
  const stub = buildStub();
  const { ZFT } = setup({
    fetch: async () => {
      calls++;
      if (fail) throw new Error('offline');
      return { ok: true, arrayBuffer: async () => stub.buffer.slice(0) };
    },
  });
  await assert.rejects(ZFT.wasmHost.get(), /offline/);
  fail = false;
  const [a, b] = await Promise.all([ZFT.wasmHost.get(), ZFT.wasmHost.get()]);
  assert.equal(a, b);
  assert.equal(calls, 2, 'one failed attempt plus one shared successful load');
  assert.equal(await ZFT.wasmHost.get(), a);
  assert.equal(a.diagnostics().pure, 'main', 'no ChromeWorker in this environment');
  await ZFT.wasmHost.shutdown();
  assert.equal(a.engine, null, 'dispose drops the instances');
  const again = await ZFT.wasmHost.get();
  assert.notEqual(again, a);
  await ZFT.wasmHost.shutdown();
});

test('the debug.inspect_in_worker pref (false) keeps the pure instance on the main thread even with a ChromeWorker', async () => {
  const created = [];
  const { ZFT, zotero } = setup({ ChromeWorker: class { constructor(u) { created.push(u); } postMessage() {} terminate() {} } });
  zotero.Prefs.set('extensions.zotero-fulltext.debug.inspect_in_worker', false);
  const host = await ZFT.wasmHost.load({ bytes: buildStub() });
  assert.equal(host.diagnostics().pure, 'main');
  assert.deepEqual(created, []);
});

test('marshalling: JSON round trip copies utf-8, frees the result buffer and never the input', async () => {
  const { host } = await stubHost();
  const x = host.engine.exports;
  const before = x.free_count.value;
  const out = host.step({ ok: { text: 'héllo ✓ 𝄞 \u0000 end', n: 1, nested: [1, { a: null }] } });
  assert.deepEqual(plain(out), { text: 'héllo ✓ 𝄞 \u0000 end', n: 1, nested: [1, { a: null }] });
  assert.equal(x.free_count.value - before, 1, 'exactly the returned buffer is freed by the host');
});

test('marshalling: err envelopes throw EngineError and leave the instance usable', async () => {
  const { host, ZFT } = await stubHost();
  assert.throws(() => host.step({ err: { code: 'bad_json', message: 'nope' } }), (e) => {
    return e instanceof ZFT.wasmHost.EngineError && e.code === 'bad_json' && /nope/.test(e.message);
  });
  assert.equal(host.engine.poisoned, false);
  assert.throws(() => host.step({ unexpected: true }), (e) => e instanceof ZFT.wasmHost.EngineError && e.code === 'bad_result');
  assert.throws(() => host.step({ circular: 1n }), (e) => e instanceof ZFT.wasmHost.EngineError && e.code === 'bad_input', 'BigInt cannot be serialised');
  assert.deepEqual(plain(host.step({ ok: { still: 'works' } })), { still: 'works' });
  assert.equal(host.diagnostics().engineTraps, 0);
});

test('marshalling: two-buffer call copies a body larger than the initial memory (views are re-created after growth)', async () => {
  const { host } = await stubHost();
  const x = host.pure.exports;
  const before = x.memory.buffer.byteLength;
  const heap0 = x.heap.value;
  const meta = { ok: { expect: 'none' } };
  const metaLen = Buffer.byteLength(JSON.stringify(meta));
  const body = new Uint8Array(300000).map((_, i) => i % 251);
  const out = host.pure.callMetaBody('fx_inspect', meta, body);
  assert.deepEqual(plain(out), { expect: 'none' });
  assert.ok(x.memory.buffer.byteLength > before, 'memory grew during the call');
  assert.equal(x.last_body_len.value, 300000);
  assert.equal(x.last_meta_len.value, metaLen);
  const copied = new Uint8Array(x.memory.buffer, heap0 + metaLen, 300000);
  assert.ok(Buffer.from(copied).equals(Buffer.from(body)), 'the body bytes arrived intact');
});

test('marshalling: an empty body or empty html is passed as (0, 0) without an allocation', async () => {
  const { host } = await stubHost();
  const x = host.pure.exports;
  const heap0 = x.heap.value;
  const metaLen = Buffer.byteLength(JSON.stringify(NONE_META));
  host.pure.callMetaBody('fx_inspect', NONE_META, new Uint8Array(0));
  assert.equal(x.heap.value - heap0, metaLen, 'only the meta buffer was allocated');
  assert.equal(x.last_body_len.value, 0);
  const state = JSON.stringify({ ok: { state: 'landed' } });
  assert.deepEqual(plain(host.pure.callTexts('fx_login_state', state, '')), { state: 'landed' });
  assert.equal(x.last_html_len.value, 0);
  host.pure.callTexts('fx_login_state', state, 'é<p>');
  assert.equal(x.last_html_len.value, 5, 'html is UTF-8 bytes (2 + 3), not UTF-16 units (4)');
});

test('trap: poisons the instance, exposes the panic, refuses further calls, resetEngine re-instantiates', async () => {
  const { host, ZFT } = await stubHost();
  const eng = host.engine;
  const firstExports = eng.exports;
  eng.exports.trap_next.value = 1;
  assert.throws(() => host.step({ ok: {} }), (e) => {
    return e instanceof ZFT.wasmHost.EngineTrap && e.panic === 'boom' && /trap/.test(e.message) && /boom/.test(e.message);
  });
  assert.equal(eng.poisoned, true);
  assert.equal(host.diagnostics().engineTraps, 1);
  assert.equal(host.diagnostics().lastPanic, 'boom');
  assert.equal(eng.lastPanic(), 'boom', 'readable on a poisoned instance');

  const heap = firstExports.heap.value;
  assert.throws(() => host.step({ ok: {} }), (e) => e instanceof ZFT.wasmHost.EngineTrap && /poisoned/.test(e.message));
  assert.equal(firstExports.heap.value, heap, 'a poisoned instance is not called again');
  assert.equal(host.diagnostics().engineTraps, 1, 'a refused call is not a new trap');

  host.resetEngine();
  assert.equal(eng.poisoned, false);
  assert.notEqual(eng.exports, firstExports, 're-instantiated from the cached module');
  assert.equal(eng.lastPanic(), '', 'the new instance has no panic');
  assert.deepEqual(plain(host.step({ ok: { back: true } })), { back: true });
});

test('resetEngine: healthy engine calls fx_reset in place; a trapping fx_reset falls back to re-instantiation', async () => {
  const { host } = await stubHost();
  const eng = host.engine;
  const x = eng.exports;
  host.resetEngine();
  assert.equal(x.reset_count.value, 1);
  assert.equal(eng.exports, x, 'no new instance when fx_reset succeeds');
  x.trap_next.value = 1;
  host.resetEngine();
  assert.notEqual(eng.exports, x);
  assert.equal(eng.poisoned, false);
  assert.equal(host.diagnostics().engineTraps, 1);
});

test('pure trap: inspect never rejects, answers Failed{pure_trap} and recovers on a fresh instance', async () => {
  const { host } = await stubHost();
  const first = host.pure.exports;
  first.trap_next.value = 1;
  const failed = await host.inspect(NONE_META, bytesOf(10));
  assert.deepEqual(plain(failed), { expect: 'failed', reason: 'pure_trap' });
  assert.equal(host.diagnostics().pureTraps, 1);
  assert.equal(host.diagnostics().lastPanic, 'boom');
  assert.notEqual(host.pure.exports, first);
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(10))), { expect: 'none' });
});

test('inspect and loginState never reject: error envelopes become Failed / pending', async () => {
  const { host } = await stubHost();
  const bad = await host.inspect({ err: { code: 'bad_json', message: 'x' } }, new Uint8Array(0));
  assert.deepEqual(plain(bad), { expect: 'failed', reason: 'inspect_error:bad_json' });
  assert.deepEqual(plain(await host.loginState({ url: JSON.stringify({ err: { code: 'x', message: 'y' } }) })), { state: 'pending' });
  assert.deepEqual(plain(await host.loginState({ url: JSON.stringify({ ok: { state: 'refused', code: 'AADSTS53000' } }), html: '<p>' })),
    { state: 'refused', code: 'AADSTS53000' });
  assert.deepEqual(plain(await host.loginState({ url: JSON.stringify({ ok: { nonsense: true } }) })), { state: 'pending' });
  assert.deepEqual(plain(await host.loginState(undefined)), { state: 'pending' });
  host.pure.exports.trap_next.value = 1;
  assert.deepEqual(plain(await host.loginState({ url: 'x' })), { state: 'pending' }, 'login_state has no trap switch in the stub, but must not reject either');
});

test('main-thread path serialises inspections and yields before a big body', async () => {
  const { host } = await stubHost();
  const x = host.pure.exports;
  const order = [];
  const call = (i, size) => host.inspect({ ok: { i } }, bytesOf(size)).then((r) => order.push(r.i));
  const all = Promise.all([call(1, 10), call(2, 200000), call(3, 10)]);
  assert.equal(x.last_body_len.value, 0, 'nothing ran synchronously');
  await all;
  assert.deepEqual(order, [1, 2, 3], 'a later small body does not overtake an earlier big one');
});

test('urlClass: the OpenAthens redirector rule runs before the probe; the probe class maps onto UrlClassKind', async () => {
  const { host } = await stubHost();
  const calls = [];
  const answers = new Map();
  host.inspect = async (meta, bytes) => { calls.push({ meta: plain(meta), len: bytes.length }); return answers.get(meta.resp.final_url) || { expect: 'probe', class: { kind: 'landing' } }; };
  const redirector = 'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1%2Fx';
  assert.equal(await host.urlClass(redirector), 'openathens_redirector');
  assert.equal(await host.urlClass('HTTPS://GO.OPENATHENS.NET/redirector/kcl.ac.uk'), 'openathens_redirector');
  assert.equal(calls.length, 0, 'no inspection for the redirector');

  const cases = [
    ['https://login.microsoftonline.com/common/oauth2/authorize', { expect: 'probe', class: { kind: 'idp_login', idp: 'entra', ca_error: null } }, 'idp_login'],
    ['https://login.kcl.idm.oclc.org/login?qurl=x', { expect: 'probe', class: { kind: 'ezproxy_need_login' } }, 'ezproxy_login'],
    ['https://www.nature.com/sso/hop', { expect: 'probe', class: { kind: 'publisher_sso_hop' } }, 'publisher_sso_hop'],
    ['https://go.openathens.net/other/path', { expect: 'probe', class: { kind: 'openathens_wait' } }, 'other'],
    ['https://example.org/a', { expect: 'probe', class: { kind: 'landing' } }, 'other'],
  ];
  for (const [url, answer, want] of cases) {
    answers.set(url, answer);
    assert.equal(await host.urlClass(url), want, url);
  }
  assert.deepEqual(calls[0], {
    meta: { spec: { expect: 'probe', route: 'inst' }, resp: { status: 200, final_url: cases[0][0], body_len: 0 } },
    len: 0,
  });
  assert.equal(calls.length, cases.length);
});

test('urlClass: results are cached (LRU, 256 entries); failures are not cached', async () => {
  const { host } = await stubHost();
  let calls = 0;
  let broken = false;
  host.inspect = async () => { calls++; return broken ? { expect: 'failed', reason: 'pure_trap' } : { expect: 'probe', class: { kind: 'idp_login' } }; };
  assert.equal(await host.urlClass('https://a.example/0'), 'idp_login');
  assert.equal(await host.urlClass('https://a.example/0'), 'idp_login');
  assert.equal(calls, 1);
  for (let i = 1; i < 256; i++) await host.urlClass('https://a.example/' + i);
  assert.equal(calls, 256);
  await host.urlClass('https://a.example/0');          // touch: now the most recently used
  await host.urlClass('https://a.example/256');        // 257th entry evicts the least recently used (/1), not /0
  assert.equal(calls, 257);
  await host.urlClass('https://a.example/0');
  assert.equal(calls, 257, '/0 survived the eviction');
  await host.urlClass('https://a.example/1');
  assert.equal(calls, 258, '/1 was evicted');
  broken = true;
  assert.equal(await host.urlClass('https://b.example/'), 'other');
  broken = false;
  assert.equal(await host.urlClass('https://b.example/'), 'idp_login', 'the earlier failure was not remembered');
});

// ===============================================================================================================
// (b) the real module
// ===============================================================================================================
const REAL = wasmBuilt() ? {} : { skip: `${path.relative(process.cwd(), WASM_PATH)} is missing: run "cargo xtask build" to include the real-wasm cases` };
const enc = new TextEncoder();
const CONFIGURE = {
  type: 'configure', protocol: 1, seed: 2718281828, persisted: {},
  config: {
    oa: {
      openalex: true, openalex_key: '', europepmc: true, biorxiv: true, pmc_s3: true, europepmc_get_pdf: true, medrxiv_early: true,
      arxiv: true, zotero_oa: true, unpaywall_email: '', semantic_scholar_key: '', core_key: '', version_order: ['published', 'accepted', 'submitted'],
    },
    inst: {
      enabled: true, try_direct: true, use_openathens: true, use_ezproxy: true, check_alma: true, login_mode: 'window', auto_open_login: true,
      login_timeout_sec: 300, group_libraries: 'ask', confirm_above: 10, batch_max: 20, hourly_cap: 30, daily_cap: 80, mb_per_hour: 300,
      min_gap_sec: 5, challenge_windows: true,
    },
    run: { item_concurrency: 4, hedge: true, pdf_max_mb: 100 },
    institution: {
      name: "King's College London", openathens_domain: 'kcl.ac.uk',
      ezproxy: { login_host: 'login.kcl.idm.oclc.org', suffix: 'kcl.idm.oclc.org' }, ezproxy_hosts: ['doi.org', 'www.bmj.com'],
      alma: { base: 'https://librarysearch.kcl.ac.uk', inst: '44KCL_INST' }, libkey_library_id: 286, idp_hosts: ['login.microsoftonline.com'],
      entra_tenant: '8370cf14-16f3-4c16-b83c-724071654356', passthrough_hosts: ['journals.lww.com'], warmup_url: '',
    },
  },
};
const pageMeta = (html) => ({
  spec: { expect: 'page', requested: { doi: '10.1371/journal.pmed.1000097' }, route: 'oa' },
  resp: { status: 200, final_url: 'https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097', headers: { 'content-type': 'text/html; charset=utf-8' }, body_len: Buffer.byteLength(html) },
});
const TINY_HTML = '<html><head><title>x</title><meta name="citation_pdf_url" content="https://example.org/a.pdf"></head><body><p>hello</p></body></html>';

async function realHost(opts = {}) {
  const env = setup();
  const host = await env.ZFT.wasmHost.load({ bytes: fs.readFileSync(WASM_PATH), inspectInWorker: false, timers: env.timers, ...opts });
  return { ...env, host };
}

test('real wasm: abi_version, instantiation twice, required exports', REAL, async () => {
  const { host, ZFT } = await realHost();
  assert.equal(host.diagnostics().abi, ZFT.wasmHost.ABI_VERSION);
  assert.notEqual(host.engine.exports.memory, host.pure.exports.memory);
  for (const name of ZFT.wasmHost.REQUIRED_EXPORTS) assert.ok(host.engine.exports[name] !== undefined, name);
});

test('real wasm: inspect of a tiny HTML page returns an Inspection with expect', REAL, async () => {
  const { host } = await realHost();
  const out = plain(await host.inspect(pageMeta(TINY_HTML), enc.encode(TINY_HTML)));
  assert.equal(out.expect, 'page');
  assert.equal(typeof out.class.kind, 'string');
  const pdf = enc.encode('%PDF-1.4\n1 0 obj<<>>endobj\n%%EOF\n');
  const verdict = plain(await host.inspect({
    spec: { expect: 'pdf', requested: {}, route: 'oa' },
    resp: { status: 200, final_url: 'https://example.org/a.pdf', headers: { 'content-type': 'application/pdf' }, body_len: pdf.length },
  }, pdf));
  // The verdict itself is core's business (core-wasm-fixtures); the host only has to deliver a well-formed PdfVerdict.
  assert.equal(verdict.expect, 'pdf');
  assert.equal(typeof verdict.is_pdf, 'boolean');
  assert.equal(verdict.len, pdf.length);
  const none = plain(await host.inspect({ spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://example.org/' } }, new Uint8Array(0)));
  assert.equal(none.expect, 'none');
  const bad = plain(await host.inspect({ not: 'a meta' }, new Uint8Array(0)));
  assert.equal(bad.expect, 'failed');
  assert.match(bad.reason, /^inspect_error:/);
});

/** Engine::step is a stub in early builds of the wasm; the engine cases run as soon as the build contains the engine. */
function engineMissing(t, e, ZFT) {
  if (e instanceof ZFT.wasmHost.EngineError && e.code === 'not_implemented') {
    t.skip('this fulltext.wasm build still has the Engine::step stub (rebuild with "cargo xtask build" after the engine packages land)');
    return true;
  }
  return false;
}

test('real wasm: step accepts a Configure command and returns a StepOut', REAL, async (t) => {
  const { host, ZFT } = await realHost();
  let out;
  try {
    out = plain(host.step({ now: 1000, wall: 1792496400123, commands: [CONFIGURE], completions: [] }));
  } catch (e) {
    if (engineMissing(t, e, ZFT)) return;
    throw e;
  }
  assert.ok(Array.isArray(out.effects));
  assert.equal(typeof out.more, 'boolean');
  assert.ok(out.stats && typeof out.stats.tasks === 'number');
  assert.throws(() => host.step({ now: 1, wall: 1, commands: [{ type: 'no_such_command' }] }), (e) => e instanceof ZFT.wasmHost.EngineError);
  assert.equal(host.engine.poisoned, false, 'an err envelope is a value, not a trap');
  host.resetEngine();
  assert.ok(Array.isArray(plain(host.step({ now: 2000, wall: 1792496400999, commands: [], completions: [] })).effects));
});

test('real wasm: loginState returns a valid LoginState', REAL, async () => {
  const { host } = await realHost();
  const st = plain(await host.loginState({ url: 'https://login.microsoftonline.com/x' }));
  assert.ok(['pending', 'challenge_shown', 'landed', 'refused'].includes(st.state), JSON.stringify(st));
  const withHtml = plain(await host.loginState({ url: 'https://login.microsoftonline.com/x', html: '<html><body>Sign in</body></html>' }));
  assert.ok(['pending', 'challenge_shown', 'landed', 'refused'].includes(withHtml.state));
});

test('real wasm: urlClass', REAL, async () => {
  const { host } = await realHost();
  const KINDS = ['openathens_redirector', 'idp_login', 'ezproxy_login', 'publisher_sso_hop', 'other'];
  assert.equal(await host.urlClass('https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1%2Fx'), 'openathens_redirector');
  // What the classifier answers for IdP / EZproxy URLs is core's contract (core-page); the host must map it onto a UrlClassKind.
  for (const url of ['https://login.microsoftonline.com/common/oauth2/authorize?client_id=1', 'https://login.kcl.idm.oclc.org/login?qurl=x', 'https://example.org/some/article']) {
    assert.ok(KINDS.includes(await host.urlClass(url)), url);
  }
  assert.equal(await host.urlClass('not a url'), 'other');
});

test('real wasm: 1000 inspect calls do not grow linear memory', REAL, async () => {
  const { host } = await realHost();
  const meta = { spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://example.org/' } };
  const body = enc.encode('x'.repeat(1000));
  for (let i = 0; i < 50; i++) await host.inspect(meta, body);   // warm up the allocator
  const before = host.pure.exports.memory.buffer.byteLength;
  for (let i = 0; i < 1000; i++) await host.inspect(meta, body);
  assert.equal(host.pure.exports.memory.buffer.byteLength, before);
});

test('real wasm: 1000 steps do not grow linear memory unboundedly', REAL, async (t) => {
  const { host, ZFT } = await realHost();
  try {
    host.step({ now: 1, wall: 1, commands: [CONFIGURE], completions: [] });
  } catch (e) {
    if (engineMissing(t, e, ZFT)) return;
    throw e;
  }
  for (let i = 0; i < 50; i++) host.step({ now: 2 + i, wall: 2 + i, commands: [], completions: [] });
  const before = host.engine.exports.memory.buffer.byteLength;
  for (let i = 0; i < 1000; i++) host.step({ now: 100 + i, wall: 100 + i, commands: [], completions: [] });
  assert.ok(host.engine.exports.memory.buffer.byteLength <= before + 65536, 'idle steps must not leak');
});

// ===============================================================================================================
// (c) the worker protocol
// ===============================================================================================================
/** Scripted fake worker: `script(worker, msg)` decides how to answer each posted message. */
function scriptedFactory(script) {
  const workers = [];
  const factory = () => {
    const w = {
      sent: [], transfers: [], terminated: false, onmessage: null, onerror: null,
      postMessage(msg, transfer) { w.sent.push(msg); w.transfers.push(transfer); script(w, msg); },
      terminate() { w.terminated = true; },
      reply(data) { if (w.onmessage) w.onmessage({ data }); },
    };
    workers.push(w);
    return w;
  };
  return { factory, workers };
}
const readyOnInit = (w, msg) => { if (msg.kind === 'init') w.reply({ kind: 'ready', abi: 1 }); };
const inspectMsgs = (w) => w.sent.filter((m) => m.kind === 'inspect');

async function workerHost(script, extra = {}) {
  const f = scriptedFactory(script);
  const env = await stubHost({ opts: { inspectInWorker: true, workerFactory: f.factory, ...extra } });
  return { ...env, ...f };
}

test('worker: the init message carries the wasm URL; requests are transferred copies and results pass through', async () => {
  const { host, workers, timers } = await workerHost((w, msg) => {
    readyOnInit(w, msg);
    if (msg.kind === 'inspect') w.reply({ id: msg.id, ok: { expect: 'none' } });
    if (msg.kind === 'login_state') w.reply({ id: msg.id, ok: { state: 'landed' } });
  });
  assert.equal(host.diagnostics().pure, 'worker');
  assert.equal(workers.length, 1);
  assert.deepEqual(plain(workers[0].sent[0]), { id: workers[0].sent[0].id, kind: 'init', wasmUrl: 'chrome://zotero-fulltext/content/fulltext.wasm' });

  const body = Uint8Array.from([1, 2, 3, 4]);
  const meta = { spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://e.org/' } };
  assert.deepEqual(plain(await host.inspect(meta, body)), { expect: 'none' });
  const msg = inspectMsgs(workers[0])[0];
  assert.deepEqual(plain(msg.meta), meta);
  assert.ok(msg.body instanceof ArrayBuffer);
  assert.deepEqual([...new Uint8Array(msg.body)], [1, 2, 3, 4]);
  assert.equal(workers[0].transfers[workers[0].sent.indexOf(msg)][0], msg.body, 'the copy is listed as transferable');
  assert.deepEqual([...body], [1, 2, 3, 4], 'the caller keeps its bytes');
  assert.equal(body.buffer.byteLength, 4);
  assert.equal(timers.pendingCount(), 0, 'the watchdog timer was cleared by the reply');

  assert.deepEqual(plain(await host.loginState({ url: 'https://login.microsoftonline.com/x', html: '<p>' })), { state: 'landed' });
  const login = workers[0].sent.find((m) => m.kind === 'login_state');
  assert.deepEqual(plain(login), { id: login.id, kind: 'login_state', url: 'https://login.microsoftonline.com/x', html: '<p>' });
});

test('worker: err replies map to Failed / pending; unknown ids and stale workers are ignored', async () => {
  const { host, workers } = await workerHost((w, msg) => {
    readyOnInit(w, msg);
    if (msg.kind === 'inspect') w.reply({ id: msg.id, err: { code: 'bad_json', message: 'x' } });
    if (msg.kind === 'login_state') w.reply({ id: msg.id, err: { code: 'bad_json', message: 'x' } });
  });
  assert.deepEqual(plain(await host.inspect(NONE_META, new Uint8Array(0))), { expect: 'failed', reason: 'inspect_error:bad_json' });
  assert.deepEqual(plain(await host.loginState({ url: 'x' })), { state: 'pending' });
  workers[0].reply({ id: 99999, ok: { expect: 'none' } });   // nobody waits for it
  workers[0].reply(undefined);
  assert.equal(host.diagnostics().pure, 'worker');
});

test('worker watchdog: terminate + respawn, re-send once, then Failed{inspect_timeout}', async () => {
  const { host, workers, timers } = await workerHost(readyOnInit);   // never answers inspect
  const p = host.inspect(NONE_META, bytesOf(3));
  const id = inspectMsgs(workers[0])[0].id;
  assert.equal(timers.pendingCount(), 1);
  timers.advance(host.opts.timers === timers ? 2999 : 0);
  assert.equal(workers.length, 1, 'not yet');
  timers.advance(1);
  assert.equal(workers[0].terminated, true);
  assert.equal(workers.length, 2, 'respawned');
  assert.equal(inspectMsgs(workers[1])[0].id, id, 're-sent once to the new worker');
  assert.deepEqual([...new Uint8Array(inspectMsgs(workers[1])[0].body)], [1, 1, 1], 'the bytes survived the first transfer');
  timers.advance(3000);
  assert.deepEqual(plain(await p), { expect: 'failed', reason: 'inspect_timeout' });
  assert.equal(workers.length, 3, 'a healthy (ready) worker is still replaced after a timeout');
  assert.equal(host.diagnostics().pure, 'worker');
  assert.equal(timers.pendingCount(), 0);
});

test('worker watchdog: the re-sent request can succeed; innocent queued requests are re-sent without being charged', async () => {
  const { host, workers, timers } = await workerHost(readyOnInit);
  const p1 = host.inspect({ ok: { n: 1 } }, bytesOf(2));
  const p2 = host.inspect({ ok: { n: 2 } }, bytesOf(2));
  const [m1, m2] = inspectMsgs(workers[0]);
  timers.advance(3000);   // both watchdogs are due; the first one recycles the worker and re-arms both
  assert.equal(workers.length, 2);
  assert.deepEqual(inspectMsgs(workers[1]).map((m) => m.id).sort(), [m1.id, m2.id].sort());
  for (const m of inspectMsgs(workers[1])) workers[1].reply({ id: m.id, ok: { expect: 'none', n: m.meta.ok.n } });
  assert.deepEqual(plain(await p1), { expect: 'none', n: 1 });
  assert.deepEqual(plain(await p2), { expect: 'none', n: 2 });
});

test('worker: two recycles of a worker that never became ready switch to the main thread for good', async () => {
  const { host, workers, timers, zotero } = await workerHost(() => {});   // no 'ready', no answers
  const p = host.inspect(NONE_META, bytesOf(3));
  timers.advance(3000);
  assert.equal(workers.length, 2);
  assert.equal(host.diagnostics().pure, 'worker');
  timers.advance(3000);
  assert.equal(workers[1].terminated, true);
  assert.equal(host.diagnostics().pure, 'main');
  assert.deepEqual(plain(await p), { expect: 'none' }, 'the pending request was answered by the main-thread instance');
  assert.equal(workers.length, 2, 'no third worker');
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(3))), { expect: 'none' });
  const warnings = zotero.debugLog.filter(([, m]) => /inspect worker disabled/.test(m));
  assert.equal(warnings.length, 1, 'logged once');
});

test('worker: an init failure counts as a never-ready recycle', async () => {
  const { host, workers } = await workerHost((w, msg) => {
    if (msg.kind === 'init') w.reply({ id: msg.id, err: { code: 'init_failed', message: 'no fetch' } });
  });
  assert.equal(workers.length, 2, 'respawned once after the first init failure');
  assert.equal(host.diagnostics().pure, 'main', 'the second init failure gave up');
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(1))), { expect: 'none' });
});

test('worker: onerror recycles the worker and re-sends what was in flight', async () => {
  const { host, workers } = await workerHost(readyOnInit);
  const p = host.inspect({ ok: { n: 1 } }, bytesOf(2));
  const id = inspectMsgs(workers[0])[0].id;
  let prevented = false;
  workers[0].onerror({ message: 'boom', preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
  assert.equal(workers.length, 2);
  workers[1].reply({ id: inspectMsgs(workers[1])[0].id, ok: { expect: 'none' } });
  assert.deepEqual(plain(await p), { expect: 'none' });
  assert.equal(inspectMsgs(workers[1])[0].id, id);
});

test('worker traps: each answers Failed{pure_trap}; the third switches to the main thread for good', async () => {
  const { host, workers } = await workerHost((w, msg) => {
    readyOnInit(w, msg);
    if (msg.kind === 'inspect') w.reply({ id: msg.id, trap: 'index out of bounds' });
  });
  for (let i = 1; i <= 3; i++) {
    assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(1))), { expect: 'failed', reason: 'pure_trap' });
    assert.equal(host.diagnostics().pureTraps, i);
    assert.equal(host.diagnostics().pure, i < 3 ? 'worker' : 'main');
  }
  assert.equal(workers[0].terminated, true);
  assert.equal(host.diagnostics().lastPanic, 'index out of bounds');
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(1))), { expect: 'none' }, 'now on the main thread');
  assert.equal(inspectMsgs(workers[0]).length, 3, 'the worker saw no 4th request');
});

test('worker placement: no ChromeWorker, a throwing factory, a worker that cannot be posted to, inspectInWorker:false', async () => {
  const none = await stubHost({ opts: { inspectInWorker: true } });
  assert.equal(none.host.diagnostics().pure, 'main');

  const throwing = await stubHost({ opts: { inspectInWorker: true, workerFactory: () => { throw new Error('no workers here'); } } });
  assert.equal(throwing.host.diagnostics().pure, 'main');
  assert.ok(throwing.zotero.debugLog.some(([, m]) => /cannot create the inspect worker: Error: no workers here/.test(m)));

  const dead = await stubHost({ opts: { inspectInWorker: true, workerFactory: () => ({ postMessage() { throw new Error('closed'); }, terminate() {} }) } });
  assert.equal(dead.host.diagnostics().pure, 'main');

  let made = 0;
  const off = await stubHost({ opts: { inspectInWorker: false, workerFactory: () => { made++; return {}; } } });
  assert.equal(off.host.diagnostics().pure, 'main');
  assert.equal(made, 0);

  const created = [];
  const real = await stubHost({
    globals: { ChromeWorker: class { constructor(u) { created.push(u); this.sent = []; } postMessage(m) { this.sent.push(m); } terminate() {} } },
    opts: { inspectInWorker: true },
  });
  assert.equal(real.host.diagnostics().pure, 'worker');
  assert.deepEqual(created, ['chrome://zotero-fulltext/content/inspect-worker.js']);
});

test('dispose: terminates the worker, answers pending requests, later calls are Failed{disposed}', async () => {
  const { host, workers, timers, ZFT } = await workerHost(readyOnInit);
  const p = host.inspect(NONE_META, bytesOf(2));
  await host.dispose();
  assert.equal(workers[0].terminated, true);
  assert.deepEqual(plain(await p), { expect: 'failed', reason: 'disposed' });
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(2))), { expect: 'failed', reason: 'disposed' });
  assert.deepEqual(plain(await host.loginState({ url: 'x' })), { state: 'pending' });
  assert.throws(() => host.step({ ok: {} }), (e) => e instanceof ZFT.wasmHost.EngineError && e.code === 'disposed');
  assert.equal(timers.pendingCount(), 0);
});

// ---- the real inspect-worker.js, run in an in-process "worker" global
/** A ChromeWorker look-alike whose global is a vm context running the real plugin/content/inspect-worker.js. */
function inProcessWorkerFactory(wasmBytes, registry = []) {
  const src = fs.readFileSync(path.join(CONTENT, 'inspect-worker.js'), 'utf8');
  return () => {
    const worker = {
      onmessage: null, onerror: null, terminated: false, sent: [],
      postMessage(msg) { worker.sent.push(msg); setImmediate(() => { if (!worker.terminated) scope.onmessage({ data: msg }); }); },
      terminate() { worker.terminated = true; },
    };
    const scope = {
      onmessage: null,
      postMessage(data) { setImmediate(() => { if (!worker.terminated && worker.onmessage) worker.onmessage({ data }); }); },
    };
    const sandbox = vm.createContext({
      self: scope, WebAssembly, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer, BigInt,
      fetch: async () => ({ ok: true, arrayBuffer: async () => wasmBytes.slice().buffer }),
    });
    new vm.Script(src, { filename: 'inspect-worker.js' }).runInContext(sandbox);
    registry.push(worker);
    return worker;
  };
}

test('inspect-worker.js (stub module): init, inspect, login_state, trap recovery', async () => {
  const stub = buildStub({ trapInit: 1 });   // every fx_inspect traps; login_state does not consult the flag
  const registry = [];
  const env = setup();
  const host = await env.ZFT.wasmHost.load({
    bytes: stub, inspectInWorker: true, timers: env.timers, workerFactory: inProcessWorkerFactory(stub, registry),
  });
  assert.equal(host.diagnostics().pure, 'worker');
  assert.deepEqual(plain(await host.loginState({ url: JSON.stringify({ ok: { state: 'landed' } }), html: 'é' })), { state: 'landed' });
  assert.deepEqual(plain(await host.inspect(NONE_META, bytesOf(5))), { expect: 'failed', reason: 'pure_trap' });
  assert.equal(host.diagnostics().lastPanic, 'boom');
  assert.deepEqual(plain(await host.loginState({ url: JSON.stringify({ ok: { state: 'pending' } }) })), { state: 'pending' }, 'the worker survived and re-instantiated');
  assert.deepEqual(plain(await host.inspect({ err: { code: 'bad_json', message: 'm' } }, bytesOf(0))), { expect: 'failed', reason: 'pure_trap' }, 'still trapping: the flag is part of the fresh instance');
  await host.dispose();
});

test('inspect-worker.js (stub module): a module the worker cannot start counts as an init failure', async () => {
  const bad = buildStub({ abi: 2 });
  const env = setup();
  const host = await env.ZFT.wasmHost.load({
    module: await WebAssembly.compile(buildStub()), inspectInWorker: true, timers: env.timers,
    workerFactory: inProcessWorkerFactory(bad),
  });
  const out = await host.inspect(NONE_META, bytesOf(1));
  assert.deepEqual(plain(out), { expect: 'none' }, 'answered by the main-thread instance after the worker failed to initialise twice');
  assert.equal(host.diagnostics().pure, 'main');
});

test('inspect-worker.js (real module) gives the same answers as the main-thread instance', REAL, async () => {
  const wasm = fs.readFileSync(WASM_PATH);
  const env = setup();
  const viaWorker = await env.ZFT.wasmHost.load({ bytes: wasm, inspectInWorker: true, timers: env.timers, workerFactory: inProcessWorkerFactory(wasm) });
  const viaMain = await env.ZFT.wasmHost.load({ bytes: wasm, inspectInWorker: false });
  assert.equal(viaWorker.diagnostics().pure, 'worker');
  const pdf = enc.encode('%PDF-1.7\n' + 'x'.repeat(3000) + '\n%%EOF\n');
  const cases = [
    [pageMeta(TINY_HTML), enc.encode(TINY_HTML)],
    [{ spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp: { status: 200, final_url: 'https://e.org/a.pdf', headers: { 'content-type': 'application/pdf' }, body_len: pdf.length } }, pdf],
    [{ spec: { expect: 'probe', route: 'inst' }, resp: { status: 200, final_url: 'https://login.microsoftonline.com/x', body_len: 0 } }, new Uint8Array(0)],
    [{ spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://e.org/' } }, new Uint8Array(0)],
  ];
  for (const [meta, body] of cases) {
    assert.deepEqual(plain(await viaWorker.inspect(meta, body)), plain(await viaMain.inspect(meta, body)), JSON.stringify(meta.spec));
  }
  const url = 'https://login.microsoftonline.com/common/oauth2/authorize';
  assert.deepEqual(plain(await viaWorker.loginState({ url })), plain(await viaMain.loginState({ url })));
  const idp = 'https://login.microsoftonline.com/common/oauth2/authorize';
  assert.equal(await viaWorker.urlClass(idp), await viaMain.urlClass(idp));
  await viaWorker.dispose();
});
