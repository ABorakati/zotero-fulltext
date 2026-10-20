// ZFT.login: pure decide() table, the sign-in window controller under a scripted fake window/browser/host with virtual
// time, and the window script (login.js) under the fake DOM.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { loadPlugin, plain, CONTENT } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeTimers } from './helpers/fakes.mjs';
import { loadWindowScript } from './helpers/fake-dom.mjs';

const WINDOW_URL = 'chrome://zotero-fulltext/content/login.xhtml';
const ENTRY = 'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1000%2Fabc';
const IDP = 'https://login.microsoftonline.com/8370cf14/oauth2/authorize?client_id=SECRET2&nonce=SECRET3';
const PUBLISHER = 'https://journals.lww.com/annalsofsurgery/fulltext/2022/09000/x.4.aspx?casa_token=SECRET1';
const PUBLISHER_HTML = '<html><head><title>Article</title></head><body>Download PDF \u00e9</body></html>';
const LOGIN_REQ = {
  kind: 'login', realm: 'openathens', entry_url: ENTRY, item_title: 'Cold-stored whole blood',
  waiting: 3, timeout_ms: 20000, capture_pdf: true,
};

const tick = () => new Promise((r) => setImmediate(r));

/** Let microtasks and virtual timers run for `ms` of virtual time (50 ms steps). */
async function advance(timers, ms = 0) {
  await tick();
  for (let left = ms; left > 0;) {
    const step = Math.min(50, left);
    timers.advance(step);
    await tick();
    left -= step;
  }
}

/** Resolve with the settled value of p ('pending' marker if it has not settled yet). */
function watch(p) {
  const w = { settled: false, value: undefined, error: undefined };
  p.then((v) => { w.settled = true; w.value = v; }, (e) => { w.settled = true; w.error = e; });
  return w;
}

// ------------------------------------------------------------------------------------------ scripted fakes

class FakeBrowser {
  constructor() {
    this.html = '';
    this.currentURI = { spec: 'about:blank', prePath: 'about:' };
    this.loaded = [];
    this.loadThrows = false;
    this.actor = 'ok'; // 'ok' | 'throws' | 'never' | 'none'
    this.browsingContext = {
      customUserAgent: null,
      get currentWindowGlobal() {
        const b = this.__browser;
        if (b.actor === 'none') return null;
        return {
          getActor: () => {
            if (b.actor === 'throws') throw new Error('actor not ready');
            return { sendQuery: () => (b.actor === 'never' ? new Promise(() => {}) : Promise.resolve(b.html)) };
          },
        };
      },
    };
    Object.defineProperty(this.browsingContext, '__browser', { value: this, enumerable: false });
  }
  loadURI(uri, opts) {
    if (this.loadThrows) throw new Error('loadURI failed');
    this.loaded.push([uri.spec, opts]);
  }
  navigate(spec, html = '') {
    this.currentURI = { spec, prePath: new URL(spec).origin };
    this.html = html;
  }
}

class FakeLoginWindow {
  constructor(holder, ready) {
    this.holder = holder;
    this.args = holder.wrappedJSObject;
    this.closed = false;
    this.activated = 0;
    this.labels = [];
    this.browser = new FakeBrowser();
    this.loadListeners = [];
    this.zftLogin = { setLabels: (l) => this.labels.push(plain(l)) };
    this.document = {
      readyState: ready ? 'complete' : 'loading',
      documentURI: ready ? WINDOW_URL : 'about:blank',
      getElementById: (id) => (id === 'zft-login-browser' ? this.browser : null),
    };
  }
  addEventListener(type, fn) { if (type === 'load') this.loadListeners.push(fn); }
  makeReady() {
    this.document.readyState = 'complete';
    this.document.documentURI = WINDOW_URL;
    for (const fn of this.loadListeners.splice(0)) fn({ type: 'load' });
  }
  /** The user (or the controller) closes the window: the page script reports 'closed' on unload. */
  close() {
    if (this.closed) return;
    this.closed = true;
    this.args.onEvent({ type: 'closed' });
  }
  event(ev) { this.args.onEvent(ev); }
  get lastLabels() { return Object.assign({}, ...this.labels); }
}

/**
 * @param {{ready?: boolean, states?: (url: string, html: string) => object, inspect?: object, openThrows?: boolean}} [o]
 */
function setup(o = {}) {
  const timers = makeFakeTimers();
  const rec = { launched: [], inspect: [], states: [], putMemory: [], putBlob: [], staged: [], captures: [], windows: [], released: 0 };
  const zotero = makeFakeZotero({
    launchURL: (u) => rec.launched.push(u),
    Utilities: { Internal: { activate: (w) => { w.activated += 1; } } },
    VersionHeader: { getPlainFirefoxUA: () => 'Mozilla/5.0 PLAIN' },
  });
  const Services = {
    startup: { shuttingDown: false },
    io: { newURI: (s) => ({ spec: s }) },
    scriptSecurityManager: { getSystemPrincipal: () => 'SYSTEM' },
    ww: {
      openWindow(parent, url, name, features, holder) {
        if (o.openThrows) throw new Error('cannot open');
        const win = new FakeLoginWindow(holder, o.ready !== false);
        win.browser.loadThrows = !!o.loadThrows;
        win.openArgs = { parent, url, name, features };
        rec.windows.push(win);
        return win;
      },
    },
  };
  const { ZFT } = loadPlugin({
    globals: { Zotero: zotero, Services, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout, performance: { now: () => timers.now } },
    files: ['util.js', 'progress.js', 'login-controller.js'],
  });
  const host = {
    loginState: async ({ url, html }) => { rec.states.push([url, html.length]); return o.states ? o.states(url, html) : { state: 'pending' }; },
    inspect: async (meta, bytes) => { rec.inspect.push({ meta: plain(meta), len: bytes.length }); return o.inspect || { expect: 'none' }; },
  };
  const bodies = {
    putMemory: (bytes, opts) => { rec.putMemory.push({ len: bytes.length, opts: plain(opts) }); return { handle: 5, len: bytes.length }; },
    putBlobAsPdf: async (blob, run) => { rec.putBlob.push([blob, run]); return { handle: 9, len: 1234 }; },
    stageForInspect: async (handle) => { rec.staged.push(handle); return { bytes: new Uint8Array(6150), head_len: 4096, truncated: false, body_len: 1234 }; },
  };
  const captureState = { found: false, disposed: 0, blob: { type: 'application/pdf' } };
  ZFT.pdfCapture = {
    capture: (browser) => {
      rec.captures.push(browser);
      return { promise: Promise.resolve(captureState.blob), found: () => captureState.found, dispose: () => { captureState.disposed += 1; } };
    },
  };
  ZFT.wasmHost = { get: async () => host };
  const ctxFor = (scope) => ({ scope: scope || new ZFT.util.CancelScope(), host, bodies });
  return { ZFT, zotero, Services, timers, rec, host, bodies, captureState, ctxFor, win: () => rec.windows[rec.windows.length - 1] };
}

// ------------------------------------------------------------------------------------------ decide()

const base = { state: { state: 'pending' }, elapsedMs: 0, timeoutMs: 100000, extended: false, windowClosed: false, userAction: null, pdfCaptured: false };

test('decide(): every branch of the table', () => {
  const { ZFT } = setup();
  const d = (over) => plain(ZFT.login.decide({ ...base, ...over }));
  assert.deepEqual(d({}), { done: null });
  assert.deepEqual(d({ pdfCaptured: true }), { done: 'pdf' });
  assert.deepEqual(d({ windowClosed: true }), { done: 'closed_by_user' });
  assert.deepEqual(d({ userAction: 'skip' }), { done: 'closed_by_user' });
  assert.deepEqual(d({ userAction: 'handoff' }), { done: 'handoff' });
  assert.deepEqual(d({ state: { state: 'refused', code: 'AADSTS53000' } }), { done: 'ca_blocked', caError: 'AADSTS53000' });
  assert.deepEqual(d({ state: { state: 'refused' } }), { done: 'ca_blocked', caError: null });
  assert.deepEqual(d({ state: { state: 'refused', code: null } }), { done: 'ca_blocked', caError: null });
  assert.deepEqual(d({ state: { state: 'landed' } }), { done: 'landed' });
  assert.deepEqual(d({ state: { state: 'challenge_shown' } }), { done: null, showChallengeHint: true });
  assert.deepEqual(d({ elapsedMs: 100000 }), { done: 'timeout' });
  assert.deepEqual(d({ elapsedMs: 150000 }), { done: 'timeout' });
  assert.deepEqual(d({ elapsedMs: 100000, userAction: 'keep_waiting' }), { done: null, extend: true });
  assert.deepEqual(d({ elapsedMs: 100000, userAction: 'keep_waiting', extended: true }), { done: 'timeout' }, 'only one extension');
  assert.deepEqual(d({ elapsedMs: 99999 }), { done: null, offerKeepWaiting: true });
  assert.deepEqual(d({ elapsedMs: 75000 }), { done: null, offerKeepWaiting: true }, 'offered exactly at 75 %');
  assert.deepEqual(d({ elapsedMs: 74999 }), { done: null });
  assert.deepEqual(d({ elapsedMs: 90000, extended: true }), { done: null }, 'not offered again after the extension');
  assert.deepEqual(d({ elapsedMs: 80000, state: { state: 'challenge_shown' } }), { done: null, showChallengeHint: true, offerKeepWaiting: true });
});

test('decide(): priority collisions', () => {
  const { ZFT } = setup();
  const d = (over) => plain(ZFT.login.decide({ ...base, ...over }));
  const everything = { pdfCaptured: true, windowClosed: true, userAction: 'handoff', state: { state: 'refused', code: 'X' }, elapsedMs: 999999 };
  assert.equal(d(everything).done, 'pdf', 'a captured PDF beats everything, even a closed window');
  assert.equal(d({ ...everything, pdfCaptured: false }).done, 'closed_by_user');
  assert.equal(d({ ...everything, pdfCaptured: false, windowClosed: false }).done, 'handoff');
  assert.equal(d({ ...everything, pdfCaptured: false, windowClosed: false, userAction: null }).done, 'ca_blocked');
  assert.equal(d({ ...everything, pdfCaptured: false, windowClosed: false, userAction: null, state: { state: 'landed' } }).done, 'landed', 'landed beats timeout');
  assert.equal(d({ ...everything, pdfCaptured: false, windowClosed: false, userAction: null, state: { state: 'pending' } }).done, 'timeout');
  assert.equal(d({ windowClosed: true, userAction: 'skip', state: { state: 'landed' } }).done, 'closed_by_user', 'a closed window beats a landed state');
  assert.equal(d({ userAction: 'skip', state: { state: 'refused', code: 'X' } }).done, 'closed_by_user', 'skip beats refused');
  assert.equal(d({ userAction: 'handoff', state: { state: 'landed' } }).done, 'handoff', 'handoff beats landed');
  assert.equal(d({ state: { state: 'refused', code: 'X' }, elapsedMs: 200000 }).done, 'ca_blocked', 'refused beats timeout');
  assert.equal(d({ userAction: 'keep_waiting', state: { state: 'landed' }, elapsedMs: 200000 }).done, 'landed');
  assert.deepEqual(d({ state: { state: 'challenge_shown' }, elapsedMs: 100000 }), { done: 'timeout' }, 'a timeout ends a challenge wait');
  assert.deepEqual(plain(ZFT.login.decide(undefined)), { done: null }, 'total on garbage input');
  assert.deepEqual(plain(ZFT.login.decide({ state: 'landed' })), { done: 'landed' }, 'a bare state string is accepted');
});

// ------------------------------------------------------------------------------------------ open(): outcomes

test('landed: window opened, plain UA, capture armed, polling until the inspector says landed; page staged and inspected', async () => {
  const states = (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' });
  const env = setup({ states, inspect: { expect: 'page', class: { kind: 'landing' } } });
  const { ZFT, timers, rec } = env;
  const p = ZFT.login.open({ op: 11, run: 3, item: 1201, req: LOGIN_REQ }, env.ctxFor());
  const w = watch(p);
  await advance(timers, 0);
  const win = env.win();
  assert.deepEqual(plain(win.openArgs), { parent: null, url: WINDOW_URL, name: '_blank', features: 'chrome,centerscreen,resizable,dialog=no' });
  const args = win.args;
  assert.equal(args.wrappedJSObject, args);
  assert.deepEqual(plain({ e: args.entryUrl, t: args.itemTitle, w: args.waiting, k: args.kind, r: args.realm, ms: args.timeoutMs }),
    { e: ENTRY, t: 'Cold-stored whole blood', w: 3, k: 'login', r: 'openathens', ms: 20000 });
  assert.equal(typeof args.format, 'function');
  assert.equal(win.activated, 1, 'brought to the front');
  assert.equal(win.browser.browsingContext.customUserAgent, 'Mozilla/5.0 PLAIN');
  assert.deepEqual(plain(win.browser.loaded), [[ENTRY, { triggeringPrincipal: 'SYSTEM' }]]);
  assert.equal(rec.captures.length, 1);
  assert.equal(rec.captures[0], win.browser, 'capture is armed on the window browser');
  assert.deepEqual(win.labels[0], { waiting: 3 });
  assert.equal(w.settled, false);

  win.browser.navigate(IDP);
  await advance(timers, 500);
  assert.equal(w.settled, false);
  win.browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(timers, 500);
  assert.equal(w.settled, true, JSON.stringify(w.error && w.error.message));
  const ok = plain(w.value);
  const bytes = new TextEncoder().encode(PUBLISHER_HTML).length;
  assert.equal(ok.result, 'landed');
  assert.equal(ok.final_url, PUBLISHER);
  assert.deepEqual(ok.page, { handle: 5, len: bytes });
  assert.deepEqual(ok.inspect, { expect: 'page', class: { kind: 'landing' } });
  assert.equal(ok.pdf, undefined);
  assert.deepEqual(rec.putMemory, [{ len: bytes, opts: { kind: 'page', run: 3 } }]);
  assert.deepEqual(rec.inspect[0], {
    meta: {
      spec: { expect: 'page', requested: {}, route: 'inst' },
      resp: { status: 200, final_url: PUBLISHER, headers: { 'content-type': 'text/html' }, body_len: bytes },
    },
    len: bytes,
  });
  assert.equal(win.closed, true, 'window closed at the end');
  assert.equal(env.captureState.disposed, 1, 'capture disposed');
  assert.equal(timers.pendingCount(), 0, 'no timer left behind');
  // the poll passed url + html to the wasm login state
  assert.ok(rec.states.some(([u, n]) => u === PUBLISHER && n === PUBLISHER_HTML.length));
});

test('landed without readable html: no page body, inspect stays none, still landed', async () => {
  for (const actor of ['throws', 'none']) {
    const env = setup({ states: (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' }) });
    const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
    await advance(env.timers, 0);
    env.win().browser.actor = actor;
    env.win().browser.navigate(PUBLISHER, PUBLISHER_HTML);
    await advance(env.timers, 500);
    assert.equal(p.value.result, 'landed', actor);
    assert.equal(p.value.page, undefined);
    assert.deepEqual(plain(p.value.inspect), { expect: 'none' });
    assert.equal(env.rec.putMemory.length, 0);
  }
});

test('a PageData actor that never answers is abandoned after 2 s and the poll goes on', async () => {
  const env = setup({ states: (url, html) => (url === PUBLISHER && html === '' ? { state: 'landed' } : { state: 'pending' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().browser.actor = 'never';
  env.win().browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(env.timers, 500);
  assert.equal(p.settled, false, 'still waiting for the actor');
  await advance(env.timers, 2000);
  assert.equal(p.settled, true);
  assert.equal(p.value.result, 'landed');
});

test('html is capped at 256 KiB before staging', async () => {
  const big = 'x'.repeat(400 * 1024);
  const env = setup({ states: (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().browser.navigate(PUBLISHER, big);
  await advance(env.timers, 500);
  assert.equal(p.value.page.len, 256 * 1024);
});

test('pdf: a captured PDF wins; stored through bodies, inspected with the staged head+tail', async () => {
  const env = setup({ inspect: { expect: 'pdf', is_pdf: true, junk_prefix: 0, has_eof: true, encrypted: false, looks_html: false, len: 1234 } });
  const { ZFT, timers, rec } = env;
  const p = watch(ZFT.login.open({ op: 2, run: 4, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(timers, 0);
  const win = env.win();
  win.browser.navigate('https://journals.lww.com/doi/pdf/10.1/x.pdf?download=1');
  env.captureState.found = true;
  await advance(timers, 500);
  assert.equal(p.settled, true);
  const ok = plain(p.value);
  assert.equal(ok.result, 'pdf');
  assert.deepEqual(ok.pdf, { handle: 9, len: 1234 });
  assert.equal(ok.page, undefined);
  assert.equal(ok.inspect.expect, 'pdf');
  assert.equal(rec.putBlob.length, 1);
  assert.equal(rec.putBlob[0][0], env.captureState.blob);
  assert.equal(rec.putBlob[0][1], 4, 'tagged with the run');
  assert.deepEqual(rec.staged, [9]);
  assert.deepEqual(rec.inspect[0].meta, {
    spec: { expect: 'pdf', requested: {}, route: 'inst' },
    resp: { status: 200, final_url: 'https://journals.lww.com/doi/pdf/10.1/x.pdf?download=1', headers: { 'content-type': 'application/pdf' }, body_len: 1234, head_len: 4096, truncated: false },
  });
  assert.equal(rec.inspect[0].len, 6150);
  assert.equal(win.closed, true);
  assert.equal(env.captureState.disposed, 1);
});

test('pdf: a captured PDF that cannot be stored becomes an io error, not a silent loss', async () => {
  const env = setup();
  env.bodies.putBlobAsPdf = async () => { throw new Error('disk full'); };
  const p = watch(env.ZFT.login.open({ op: 2, run: 4, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.captureState.found = true;
  await advance(env.timers, 500);
  assert.equal(p.settled, true);
  assert.equal(p.error && p.error.kind, 'io');
  assert.equal(env.win().closed, true);
});

test('capture is armed only when the request asks for it', async () => {
  const env = setup();
  env.ZFT.login.open({ op: 1, run: 1, item: 1, req: { ...LOGIN_REQ, capture_pdf: false } }, env.ctxFor());
  await advance(env.timers, 0);
  assert.equal(env.rec.captures.length, 0);
  env.ZFT.login.closeAll();
});

test('skip and a closed window finish immediately as closed_by_user (no wait for the next poll)', async () => {
  {
    const env = setup();
    const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
    await advance(env.timers, 0);
    env.win().event({ type: 'skip' });
    await advance(env.timers, 0);
    assert.equal(p.settled, true);
    assert.equal(p.value.result, 'closed_by_user');
    assert.equal(p.value.final_url, '');
    assert.equal(env.win().closed, true);
    assert.equal(env.rec.launched.length, 0);
  }
  {
    const env = setup();
    const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
    await advance(env.timers, 0);
    env.win().close(); // the user's own X
    await advance(env.timers, 0);
    assert.equal(p.value.result, 'closed_by_user');
    assert.equal(env.captureState.disposed, 1);
    assert.equal(env.timers.pendingCount(), 0);
  }
});

test('a window that is closed without an unload event is noticed by the poll (win.closed)', async () => {
  const env = setup();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().closed = true;
  await advance(env.timers, 500);
  assert.equal(p.value.result, 'closed_by_user');
});

test('handoff: the entry URL goes to the system browser, only when it is http(s)', async () => {
  const env = setup();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().event({ type: 'handoff' });
  await advance(env.timers, 0);
  assert.equal(p.value.result, 'handoff');
  assert.deepEqual(plain(env.rec.launched), [ENTRY]);
  assert.equal(env.win().closed, true);

  // a non-web entry URL never opens a window, never reaches launchURL
  for (const bad of ['file:///C:/Users/me/x.html', 'javascript:alert(1)', 'chrome://browser/content/', 'ftp://x/y', '', undefined, 'garbage']) {
    const e2 = setup();
    const ok = await e2.ZFT.login.open({ op: 1, run: 1, item: 1, req: { ...LOGIN_REQ, entry_url: bad } }, e2.ctxFor());
    assert.equal(ok.result, 'closed_by_user', String(bad));
    assert.equal(e2.rec.windows.length, 0);
    assert.equal(e2.rec.launched.length, 0);
  }
});

test('timeout: offered at 75 %, ends at timeout_ms; final_url carries no query', async () => {
  const env = setup();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  const win = env.win();
  win.browser.navigate(IDP);
  await advance(env.timers, 14000);
  assert.equal(win.lastLabels.showKeepWaiting, undefined, 'not yet');
  await advance(env.timers, 1000); // 15 s = 75 %
  assert.equal(win.lastLabels.showKeepWaiting, true);
  assert.equal(p.settled, false);
  await advance(env.timers, 5000);
  assert.equal(p.settled, true);
  assert.equal(p.value.result, 'timeout');
  assert.equal(p.value.final_url, 'https://login.microsoftonline.com/8370cf14/oauth2/authorize');
  assert.equal(win.closed, true);
  assert.equal(env.timers.pendingCount(), 0);
});

test('keep waiting: one extension of timeout_ms; asking early is remembered until the deadline', async () => {
  const states = (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' });
  const env = setup({ states });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  const win = env.win();
  win.browser.navigate(IDP);
  await advance(env.timers, 16000);
  assert.equal(win.lastLabels.showKeepWaiting, true);
  win.event({ type: 'keep_waiting' }); // at 16 s, before the deadline
  await advance(env.timers, 3000);
  assert.equal(p.settled, false, 'still inside the first window');
  await advance(env.timers, 1500); // past 20 s: extended instead of timing out
  assert.equal(p.settled, false);
  assert.equal(win.lastLabels.showKeepWaiting, false, 'the button goes away once used');
  await advance(env.timers, 17000); // 38.5 s < 40 s
  assert.equal(p.settled, false);
  win.browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(env.timers, 500);
  assert.equal(p.value.result, 'landed', 'the extension let the sign-in finish');
});

test('keep waiting only once: the extended window times out at twice timeout_ms', async () => {
  const env = setup();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().browser.navigate(IDP);
  await advance(env.timers, 16000);
  env.win().event({ type: 'keep_waiting' });
  await advance(env.timers, 4500);
  env.win().event({ type: 'keep_waiting' }); // a second request changes nothing
  await advance(env.timers, 15000); // 35.5 s
  assert.equal(p.settled, false);
  await advance(env.timers, 5000);
  assert.equal(p.value.result, 'timeout');
});

test('refused: ca_blocked with the error code; the window stays 1.5 s so the user can read it, then closes', async () => {
  const states = (url) => (url === IDP ? { state: 'refused', code: 'AADSTS53000' } : { state: 'pending' });
  const env = setup({ states });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  const win = env.win();
  win.browser.navigate(IDP);
  await advance(env.timers, 500);
  assert.equal(p.settled, false, 'lingering');
  assert.equal(win.closed, false);
  assert.equal(win.lastLabels.hint, 'zotero-fulltext-login-ca-blocked {code=AADSTS53000}');
  await advance(env.timers, 1500);
  assert.equal(p.settled, true);
  const ok = plain(p.value);
  assert.equal(ok.result, 'ca_blocked');
  assert.equal(ok.ca_error, 'AADSTS53000');
  assert.deepEqual(ok.inspect, { expect: 'none' });
  assert.equal(ok.final_url, 'https://login.microsoftonline.com/8370cf14/oauth2/authorize', 'no query string in the result');
  assert.equal(win.closed, true);
});

test('refused: closing the window during the linger ends it at once', async () => {
  const env = setup({ states: () => ({ state: 'refused', code: 'AADSTS53000' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().browser.navigate(IDP);
  await advance(env.timers, 500);
  assert.equal(p.settled, false);
  env.win().close();
  await advance(env.timers, 0);
  assert.equal(p.value.result, 'ca_blocked');
});

test('refused without a code leaves ca_error out', async () => {
  const env = setup({ states: () => ({ state: 'refused', code: null }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().browser.navigate(IDP);
  await advance(env.timers, 2100);
  assert.equal(p.value.result, 'ca_blocked');
  assert.equal(p.value.ca_error, undefined);
});

test('challenge: the hint appears while a human check is shown and goes when it is solved; the window stays open', async () => {
  let state = { state: 'challenge_shown' };
  const env = setup({ states: () => state });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  const win = env.win();
  win.browser.navigate('https://www.sciencedirect.com/science/article/pii/S1', '<title>Just a moment</title>');
  await advance(env.timers, 500);
  assert.equal(win.lastLabels.hint, 'zotero-fulltext-login-challenge-hint');
  assert.equal(p.settled, false);
  state = { state: 'pending' };
  await advance(env.timers, 500);
  assert.equal(win.lastLabels.hint, '', 'cleared once the page is no longer a challenge');
  state = { state: 'landed' };
  await advance(env.timers, 500);
  assert.equal(p.value.result, 'landed');
});

test('a challenge-kind window shows the hint from its first frame and keeps it until the page is classified', async () => {
  const env = setup({ states: () => ({ state: 'pending' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: { ...LOGIN_REQ, kind: 'challenge' } }, env.ctxFor()));
  await advance(env.timers, 0);
  const win = env.win();
  assert.equal(win.args.kind, 'challenge');
  assert.equal(win.labels[0].hint, 'zotero-fulltext-login-challenge-hint');
  win.browser.navigate('https://www.sciencedirect.com/science/article/pii/S1');
  await advance(env.timers, 1500);
  assert.equal(win.lastLabels.hint, 'zotero-fulltext-login-challenge-hint', 'not cleared by "pending" before any classification');
  env.ZFT.login.closeAll();
  await advance(env.timers, 0);
  assert.equal(p.value.result, 'closed_by_user');
});

// ------------------------------------------------------------------------------------------ open(): lifecycle and edge cases

test('scope cancellation closes the window, disposes the capture and rejects cancelled', async () => {
  const env = setup();
  const scope = new env.ZFT.util.CancelScope();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor(scope)));
  await advance(env.timers, 0);
  env.win().browser.navigate(IDP);
  await advance(env.timers, 500);
  scope.cancel('run_cancelled');
  await advance(env.timers, 0);
  assert.equal(p.settled, true);
  assert.equal(p.error && p.error.kind, 'cancelled');
  assert.equal(env.win().closed, true);
  assert.equal(env.captureState.disposed, 1);
  assert.equal(env.timers.pendingCount(), 0);
  // an already-cancelled scope never opens a window
  const e2 = setup();
  const s2 = new e2.ZFT.util.CancelScope();
  s2.cancel('x');
  await assert.rejects(e2.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, e2.ctxFor(s2)), (e) => e.kind === 'cancelled');
  assert.equal(e2.rec.windows.length, 0);
});

test('scope cancellation while the window is still loading', async () => {
  const env = setup({ ready: false });
  const scope = new env.ZFT.util.CancelScope();
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor(scope)));
  await advance(env.timers, 100);
  scope.cancel('shutdown');
  await advance(env.timers, 0);
  assert.equal(p.error && p.error.kind, 'cancelled');
  assert.equal(env.win().closed, true);
  assert.equal(env.win().browser.loaded.length, 0);
});

test('a second concurrent window for the same realm is rejected; another realm and a later one are fine', async () => {
  const env = setup();
  const first = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  await assert.rejects(env.ZFT.login.open({ op: 2, run: 1, item: 2, req: LOGIN_REQ }, env.ctxFor()), (e) => e.kind === 'internal');
  assert.equal(env.rec.windows.length, 1);
  const other = watch(env.ZFT.login.open({ op: 3, run: 1, item: 3, req: { ...LOGIN_REQ, realm: 'ezproxy', entry_url: 'https://login.kcl.idm.oclc.org/login?url=x' } }, env.ctxFor()));
  await advance(env.timers, 0);
  assert.equal(env.rec.windows.length, 2);
  env.win().event({ type: 'skip' });
  env.rec.windows[0].event({ type: 'skip' });
  await advance(env.timers, 0);
  assert.equal(first.value.result, 'closed_by_user');
  assert.equal(other.value.result, 'closed_by_user');
  // the realm is free again
  const again = watch(env.ZFT.login.open({ op: 4, run: 1, item: 4, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  assert.equal(env.rec.windows.length, 3);
  env.ZFT.login.closeAll();
  await advance(env.timers, 0);
  assert.equal(again.value.result, 'closed_by_user');
});

test('update() changes the waiting label; an unknown or finished op is a no-op; early updates reach the first frame', async () => {
  const env = setup({ ready: false });
  const p = watch(env.ZFT.login.open({ op: 7, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.ZFT.login.update(7, 9); // the window has not loaded yet
  env.win().makeReady();
  await advance(env.timers, 100);
  assert.ok(env.win().labels.length >= 1 && env.win().labels.every((l) => l.waiting === 9), 'the latest count is what the first frame gets');
  env.ZFT.login.update(7, 12);
  assert.deepEqual(env.win().labels[env.win().labels.length - 1], { waiting: 12 });
  const before = env.win().labels.length;
  env.ZFT.login.update(999, 4);
  assert.equal(env.win().labels.length, before);
  env.win().event({ type: 'skip' });
  await advance(env.timers, 0);
  assert.equal(p.settled, true);
  env.ZFT.login.update(7, 1);
  assert.equal(env.win().labels.length, before, 'finished: ignored');
  assert.doesNotThrow(() => env.ZFT.login.update(undefined, NaN));
});

test('window closed during load: closed_by_user, nothing loaded', async () => {
  const env = setup({ ready: false });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 200);
  env.win().close();
  await advance(env.timers, 100);
  assert.equal(p.value.result, 'closed_by_user');
  assert.equal(env.win().browser.loaded.length, 0);
  assert.equal(env.rec.captures.length, 0);
  assert.equal(env.timers.pendingCount(), 0);
});

test('a window that never finishes loading is abandoned after 15 s', async () => {
  const env = setup({ ready: false });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 14000);
  assert.equal(p.settled, false);
  await advance(env.timers, 2000);
  assert.equal(p.value.result, 'closed_by_user');
  assert.equal(env.win().closed, true);
});

test('a slow load works: the load event wakes the wait', async () => {
  const env = setup({ ready: false, states: (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 1000);
  assert.equal(env.win().browser.loaded.length, 0);
  env.win().makeReady();
  await advance(env.timers, 100);
  assert.equal(env.win().browser.loaded.length, 1);
  env.win().browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(env.timers, 500);
  assert.equal(p.value.result, 'landed');
});

test('loadURI throwing closes the window and finishes closed_by_user (logged without the query string)', async () => {
  const env = setup({ loadThrows: true });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 100);
  assert.equal(p.value.result, 'closed_by_user');
  assert.equal(env.win().closed, true);
  assert.equal(env.captureState.disposed, 1, 'the capture armed before loadURI is released');
  assert.ok(env.zotero.debugLog.some(([, m]) => m.includes('loadURI failed for go.openathens.net/redirector/kcl.ac.uk:')));
  assert.ok(env.zotero.debugLog.every(([, m]) => !/SECRET|doi\.org%2F10/.test(m)), 'no query string in any log line');
  assert.equal(env.timers.pendingCount(), 0);
});

test('openWindow throwing resolves closed_by_user', async () => {
  const env = setup({ openThrows: true });
  const ok = await env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor());
  assert.equal(ok.result, 'closed_by_user');
  assert.equal(env.timers.pendingCount(), 0);
});

test('no window while the application is quitting', async () => {
  const env = setup();
  env.Services.startup.shuttingDown = true;
  const ok = await env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor());
  assert.equal(ok.result, 'closed_by_user');
  assert.equal(env.rec.windows.length, 0);
});

test('closeAll settles every open sign-in as closed_by_user, closes the windows and leaves no timers', async () => {
  const env = setup();
  const a = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  const b = watch(env.ZFT.login.open({ op: 2, run: 1, item: 2, req: { ...LOGIN_REQ, realm: 'ezproxy' } }, env.ctxFor()));
  const c = watch(env.ZFT.login.warmUp({ institution: { openathens_domain: 'kcl.ac.uk', warmup_url: '' }, inst: { login_timeout_sec: 300 } }));
  await advance(env.timers, 300);
  assert.equal(env.rec.windows.length, 3);
  env.ZFT.login.closeAll();
  await advance(env.timers, 0);
  assert.equal(a.value.result, 'closed_by_user');
  assert.equal(b.value.result, 'closed_by_user');
  assert.equal(c.settled, true);
  assert.ok(env.rec.windows.every((w) => w.closed));
  assert.equal(env.timers.pendingCount(), 0);
  assert.equal(env.captureState.disposed, 2);
  assert.doesNotThrow(() => env.ZFT.login.closeAll());
});

test('closeAll while a window is still opening beats it', async () => {
  const env = setup({ ready: false });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  env.ZFT.login.closeAll();
  await advance(env.timers, 0);
  assert.equal(p.value.result, 'closed_by_user');
  assert.equal(env.win().closed, true);
  assert.equal(env.win().browser.loaded.length, 0, 'the sign-in page never started');
});

test('nothing sensitive is logged: no query strings, no cookies', async () => {
  const env = setup({ states: (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' }) });
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 0);
  env.win().event({ type: 'popup', via: 'openURI', url: 'https://idp.example.org/sso?SAMLRequest=SECRET4' });
  env.win().browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(env.timers, 500);
  assert.equal(p.value.result, 'landed');
  const log = env.zotero.debugLog.map(([, m]) => m).join('\n');
  assert.match(log, /idp\.example\.org\/sso/);
  assert.doesNotMatch(log, /SECRET|SAMLRequest|casa_token|%2F/);
});

// ------------------------------------------------------------------------------------------ warmUp

const WARM_CONFIG = { institution: { openathens_domain: 'kcl.ac.uk', warmup_url: '' }, inst: { login_timeout_sec: 120 } };

test('warmUp: opens the redirector for doi.org without capture or item, and resolves 1.5 s after landing', async () => {
  const states = (url) => (url === PUBLISHER ? { state: 'landed' } : { state: 'pending' });
  const env = setup({ states });
  const p = watch(env.ZFT.login.warmUp(WARM_CONFIG));
  await advance(env.timers, 100);
  const win = env.win();
  assert.equal(win.args.entryUrl, 'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F');
  assert.equal(win.args.itemTitle, '');
  assert.equal(win.args.waiting, 0);
  assert.equal(win.args.timeoutMs, 120000);
  assert.equal(env.rec.captures.length, 0);
  assert.deepEqual(win.labels[0], { waiting: 0 });
  assert.deepEqual(plain(win.browser.loaded[0][0]), win.args.entryUrl);
  win.browser.navigate(PUBLISHER, PUBLISHER_HTML);
  await advance(env.timers, 500);
  assert.equal(p.settled, false, 'the user sees the result first');
  assert.equal(win.closed, false);
  await advance(env.timers, 1500);
  assert.equal(p.settled, true);
  assert.equal(win.closed, true);
  assert.equal(env.rec.putMemory.length, 0, 'warm-up keeps no page');
  assert.equal(env.timers.pendingCount(), 0);
});

test('warmUp: institution.warmup_url wins; an empty config is a no-op; the user closing the window resolves it', async () => {
  const env = setup();
  const p = watch(env.ZFT.login.warmUp({ institution: { openathens_domain: 'kcl.ac.uk', warmup_url: 'https://example.org/sso/start?x=1' }, inst: {} }));
  await advance(env.timers, 100);
  assert.equal(env.win().args.entryUrl, 'https://example.org/sso/start?x=1');
  assert.equal(env.win().args.timeoutMs, 300000, 'default timeout');
  env.win().close();
  await advance(env.timers, 0);
  assert.equal(p.settled, true);

  const e2 = setup();
  await e2.ZFT.login.warmUp({ institution: {}, inst: {} });
  await e2.ZFT.login.warmUp(undefined);
  await e2.ZFT.login.warmUp({ institution: { warmup_url: 'javascript:alert(1)' } });
  assert.equal(e2.rec.windows.length, 0);
});

test('warmUp: a second call while the window is open reuses it', async () => {
  const env = setup();
  const a = env.ZFT.login.warmUp(WARM_CONFIG);
  const b = env.ZFT.login.warmUp(WARM_CONFIG);
  await advance(env.timers, 100);
  const c = env.ZFT.login.warmUp(WARM_CONFIG);
  await advance(env.timers, 100);
  assert.equal(env.rec.windows.length, 1);
  assert.ok(env.win().activated >= 1);
  env.win().event({ type: 'skip' });
  await advance(env.timers, 0);
  await Promise.all([a, b, c]);
});

test('warmUp works without the wasm classifier (the window just waits for the user)', async () => {
  const env = setup();
  env.ZFT.wasmHost = { get: async () => { throw new Error('wasm failed'); } };
  const p = watch(env.ZFT.login.warmUp(WARM_CONFIG));
  await advance(env.timers, 100);
  assert.equal(env.rec.windows.length, 1);
  env.win().event({ type: 'handoff' });
  await advance(env.timers, 0);
  assert.equal(p.settled, true);
  assert.deepEqual(plain(env.rec.launched), [env.win().args.entryUrl]);
});

test('a warm-up window does not block the per-realm single-flight of a real sign-in', async () => {
  const env = setup();
  env.ZFT.login.warmUp(WARM_CONFIG);
  await advance(env.timers, 100);
  const p = watch(env.ZFT.login.open({ op: 1, run: 1, item: 1, req: LOGIN_REQ }, env.ctxFor()));
  await advance(env.timers, 100);
  assert.equal(env.rec.windows.length, 2);
  env.ZFT.login.closeAll();
  await advance(env.timers, 0);
  assert.equal(p.value.result, 'closed_by_user');
});

// ------------------------------------------------------------------------------------------ login.js (window script)

function loginWindow(over = {}) {
  const events = [];
  const args = {
    entryUrl: ENTRY, itemTitle: 'T', waiting: 2, kind: 'login', realm: 'openathens', timeoutMs: 1000,
    onEvent: (ev) => events.push(ev),
    format: (id) => (id === 'zotero-fulltext-login-title' ? 'Sign in for full text' : id),
    ...over,
  };
  args.wrappedJSObject = args;
  const w = loadWindowScript('login.js', { xhtml: 'login.xhtml', args });
  // the real <browser> element is created by the markup; give the fake one the members login.js reads
  const browser = w.byId('zft-login-browser');
  browser.currentURI = { spec: 'about:blank', prePath: 'about:' };
  browser.browsingContext = { secureBrowserUI: { state: 0 } };
  return { w, events, browser, args };
}

test('login.js: buttons emit events; unload emits closed; setLabels before load is applied on load', () => {
  const { w, events } = loginWindow();
  w.zftLogin.setLabels({ waiting: 4, showKeepWaiting: true });
  w.fire('DOMContentLoaded');
  w.fire('load');
  const waiting = w.byId('zft-login-waiting');
  assert.equal(waiting.l10nId, 'zotero-fulltext-login-waiting');
  assert.deepEqual(plain(waiting.l10nArgs), { count: 4 });
  assert.equal(w.byId('zft-login-keep').hidden, false);
  w.byId('zft-login-skip').click();
  w.byId('zft-login-handoff').click();
  w.byId('zft-login-keep').click();
  assert.deepEqual(plain(events), [{ type: 'skip' }, { type: 'handoff' }, { type: 'keep_waiting' }]);
  w.close();
  assert.deepEqual(plain(events.slice(3)), [{ type: 'closed' }]);
  assert.equal(w.intervalCount(), 0, 'the bar timer is cleared');
});

test('login.js: setLabels updates the waiting text, the hint (string or message) and the Keep button; zero hides the text', () => {
  const { w } = loginWindow();
  w.fire('load');
  const waiting = w.byId('zft-login-waiting');
  const hint = w.byId('zft-login-hint');
  const keep = w.byId('zft-login-keep');
  assert.equal(keep.hidden, true, 'hidden until offered');
  w.zftLogin.setLabels({ waiting: 1 });
  assert.deepEqual(plain(waiting.l10nArgs), { count: 1 });
  w.zftLogin.setLabels({ waiting: 0 });
  assert.equal(waiting.l10nId, '');
  assert.equal(waiting.textContent, '');
  w.zftLogin.setLabels({ hint: 'Please complete the check' });
  assert.equal(hint.textContent, 'Please complete the check');
  w.zftLogin.setLabels({ hint: { id: 'zotero-fulltext-login-ca-blocked', args: { code: 'AADSTS53000' } } });
  assert.equal(hint.l10nId, 'zotero-fulltext-login-ca-blocked');
  assert.deepEqual(plain(hint.l10nArgs), { code: 'AADSTS53000' });
  w.zftLogin.setLabels({ hint: '' });
  assert.equal(hint.textContent, '');
  assert.equal(hint.l10nId, '');
  w.zftLogin.setLabels({ showKeepWaiting: true });
  assert.equal(keep.hidden, false);
  w.zftLogin.setLabels({ showKeepWaiting: false });
  assert.equal(keep.hidden, true);
  assert.doesNotThrow(() => { w.zftLogin.setLabels(null); w.zftLogin.setLabels('x'); w.zftLogin.setLabels({}); });
});

test('login.js: the bar shows the real origin and TLS state; the title is "<login title>: <origin>"', () => {
  const { w, browser } = loginWindow();
  w.fire('load');
  assert.equal(w.document.title, 'Sign in for full text');
  assert.equal(w.byId('zft-login-origin').textContent, '');
  browser.currentURI = { spec: 'https://login.microsoftonline.com/x/y?z=1', prePath: 'https://login.microsoftonline.com' };
  browser.browsingContext.secureBrowserUI.state = 0x10000;
  w.tickIntervals();
  assert.equal(w.byId('zft-login-origin').textContent, 'https://login.microsoftonline.com');
  assert.equal(w.byId('zft-login-secure').l10nId, 'zotero-fulltext-login-secure');
  assert.equal(w.byId('zft-login-secure').style.color, '#060');
  assert.equal(w.document.title, 'Sign in for full text: https://login.microsoftonline.com');
  browser.currentURI = { spec: 'http://evil.example.org/phish', prePath: 'http://evil.example.org' };
  browser.browsingContext.secureBrowserUI.state = 0;
  w.tickIntervals();
  assert.equal(w.byId('zft-login-secure').l10nId, 'zotero-fulltext-login-not-secure');
  assert.equal(w.byId('zft-login-secure').style.color, '#a00');
  assert.equal(w.document.title, 'Sign in for full text: http://evil.example.org');
  // the title never carries a query string or path
  assert.doesNotMatch(w.document.title, /phish|z=1/);
});

test('login.js: window.open / target=_blank pages open in the visible popup pane and are counted', () => {
  const { w, events } = loginWindow();
  w.fire('DOMContentLoaded');
  w.fire('load');
  const access = w.browserDOMWindow;
  assert.ok(access, 'nsIBrowserDOMWindow installed');
  assert.equal(access.canClose(), true);
  assert.equal(access.tabCount, 1);
  const box = w.byId('zft-login-popup-box');
  assert.equal(box.hidden, true);
  const b1 = access.openURIInFrame({ spec: 'https://idp.example.org/sso?x=1' }, { openWindowInfo: null }, 0, 0, '');
  assert.equal(box.hidden, false);
  assert.equal(box.children.length, 1);
  assert.equal(b1.getAttribute('type'), 'content');
  assert.equal(b1.getAttribute('maychangeremoteness'), 'true');
  access.createContentWindowInFrame({ spec: 'https://other.example.org/' }, {}, 0, 0, '');
  access.createContentWindow({ spec: 'https://third.example.org/' }, null, 0, 0, null, null);
  assert.equal(box.children.length, 3);
  const counter = w.byId('zft-login-popups');
  assert.equal(counter.l10nId, 'zotero-fulltext-login-popups');
  assert.deepEqual(plain(counter.l10nArgs), { count: 3 });
  assert.deepEqual(plain(events.map((e) => e.type + ':' + e.via)), ['popup:openURIInFrame', 'popup:createContentWindowInFrame', 'popup:createContentWindow']);
});

test('login.js: it never loads a URL or touches credentials itself', () => {
  const src = fs.readFileSync(path.join(CONTENT, 'login.js'), 'utf8');
  const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
  assert.doesNotMatch(code, /\.cookie|cookies|password|loginfmt|getCookie|Cookie/i);
  assert.equal((code.match(/\.loadURI\(/g) || []).length, 1, 'only popup panes load, and only what the page asked for');
  assert.doesNotMatch(code, /location\.href|\.src\s*=|window\.open/);
});

test('every id the login window script looks up exists in login.xhtml', () => {
  const script = fs.readFileSync(path.join(CONTENT, 'login.js'), 'utf8');
  const markup = fs.readFileSync(path.join(CONTENT, 'login.xhtml'), 'utf8');
  const ids = new Set([...script.matchAll(/'(zft-login-[a-z-]+)'/g)].map((m) => m[1]));
  assert.ok(ids.size >= 8, [...ids].join());
  for (const id of ids) assert.ok(markup.includes(`id="${id}"`), id);
  // the <browser> must not carry messagemanagergroup: Zotero's ExternalLinkHandler would send link clicks to the system browser
  assert.doesNotMatch(markup.replace(/<!--[\s\S]*?-->/g, ''), /messagemanagergroup/);
});
