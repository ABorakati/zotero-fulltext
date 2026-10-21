// ZFT.browser: HiddenBrowser load with our own settle detection. A fake HiddenBrowser module is installed through a fake
// ChromeUtils.importESModule; location events are scripted on the fake clock that also drives the poll loop.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeTimers } from './helpers/fakes.mjs';

const flush = () => new Promise((resolve) => setImmediate(resolve));
const PLAIN_UA = 'Mozilla/5.0 (Plain) Firefox/140.0';
const Ci = {
  nsILoadContext: {},
  nsIWebProgress: { NOTIFY_LOCATION: 0x100 },
  nsIWebProgressListener: { LOCATION_CHANGE_SAME_DOCUMENT: 0x1 },
};

function fakeMimeHandler() {
  const log = [];
  const handlers = [];
  return {
    log, handlers,
    addHandlers(type, handler) { log.push('add'); handlers.push(handler); },
    removeHandlers(type, handler) { log.push('remove'); const i = handlers.indexOf(handler); if (i !== -1) handlers.splice(i, 1); },
    async deliver(browser, blob, name) {
      const channel = { notificationCallbacks: { getInterface: () => ({ topFrameElement: browser }) } };
      let handled = false;
      for (const h of [...handlers]) await h.onStartRequest(name, 'application/pdf', channel);
      for (const h of [...handlers]) handled = handled || (await h.onContent(blob, name, 'application/pdf', channel));
      return handled;
    },
  };
}

/**
 * @param {object} [o]
 * @param {Object<string, Array<{atMs:number,url:string,topLevel?:boolean,flags?:number}>>} [o.scripts] events per load URL
 * @param {(url:string)=>string} [o.urlClass]
 * @param {boolean} [o.mime=true] install Zotero.MIMETypeHandler
 * @param {object} [o.createGate] a Deferred that gates every HiddenBrowser's _createdPromise
 * @param {(hb:object,url:string)=>any} [o.load] override HiddenBrowser.prototype.load
 * @param {object} [o.pageData] value (or function) for getPageData
 */
function setup(o = {}) {
  const timers = makeFakeTimers();
  const instances = [];
  const mime = fakeMimeHandler();
  const scripts = o.scripts || {};

  class FakeHiddenBrowser {
    constructor(options) {
      this.options = options;
      this.destroyed = 0;
      this.loadCalls = [];
      this.pageDataCalls = [];
      this.currentURI = { spec: 'about:blank' };
      this._browser = { id: 'browser-' + instances.length };
      this.listeners = [];
      this.masks = [];
      this.removed = 0;
      const self = this;
      this.webProgress = {
        addProgressListener(l, mask) { self.listeners.push(l); self.masks.push(mask); },
        removeProgressListener(l) { self.removed++; const i = self.listeners.indexOf(l); if (i !== -1) self.listeners.splice(i, 1); },
      };
      this._createdPromise = o.createGate ? o.createGate.promise : Promise.resolve();
      instances.push(this);
    }
    load(url) {
      this.loadCalls.push(url);
      for (const ev of scripts[url] || []) {
        timers.setTimeout(() => {
          this.currentURI = { spec: ev.url };
          if (ev.docUrl) this._browser.documentURI = { spec: ev.docUrl };
          for (const l of [...this.listeners]) l.onLocationChange({ isTopLevel: ev.topLevel !== false }, null, { spec: ev.url }, ev.flags || 0);
        }, ev.atMs);
      }
      return o.load ? o.load(this, url) : Promise.resolve(true);
    }
    async getPageData(props, opts) {
      this.pageDataCalls.push({ props, opts });
      const pd = o.pageData === undefined ? { documentHTML: '<html>page</html>', channelInfo: { responseStatus: 200 } } : o.pageData;
      return typeof pd === 'function' ? pd(this) : pd;
    }
    destroy() { this.destroyed++; }
  }

  const ChromeUtils = {
    generateQI: (names) => function QueryInterface() { return { names }; },
    importESModule(url) {
      assert.equal(url, 'chrome://zotero/content/HiddenBrowser.mjs');
      if (o.noModule) throw new Error('ESM not found');
      return o.badModule ? {} : { HiddenBrowser: FakeHiddenBrowser };
    },
  };
  const Zotero = makeFakeZotero({
    VersionHeader: { getPlainFirefoxUA: () => PLAIN_UA },
    ...(o.mime === false ? {} : { MIMETypeHandler: mime }),
  });
  const { ZFT } = loadPlugin({
    files: ['util.js', 'pdf-capture.js', 'browser.js'],
    globals: {
      Zotero, ChromeUtils, Ci,
      setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout,
      performance: { now: () => timers.now },
    },
  });
  const host = { urlCalls: [], async urlClass(url) { this.urlCalls.push(url); return (o.urlClass || (() => 'other'))(url); } };
  const env = { ZFT, timers, instances, mime, host, scope: () => new ZFT.util.CancelScope() };

  /** Start a load and return {promise, state} without driving the clock. */
  env.start = (req, scope = env.scope()) => {
    const state = { done: false, value: undefined, error: undefined };
    const promise = ZFT.browser.load(req, { scope, host });
    promise.then((v) => { state.done = true; state.value = v; }, (e) => { state.done = true; state.error = e; });
    return { promise, state, scope };
  };
  /** Advance the fake clock in poll-sized steps until `state` is done. */
  env.drive = async (h, maxMs = 120000) => {
    await flush();
    for (let t = 0; !h.state.done && t < maxMs; t += ZFT.browser.POLL_MS) {
      timers.advance(ZFT.browser.POLL_MS);
      await flush();
    }
    assert.ok(h.state.done, 'the load did not finish within ' + maxMs + ' fake ms');
    if (h.state.error) throw h.state.error;
    return h.state.value;
  };
  env.run = (req) => env.drive(env.start(req));
  return env;
}

function req(extra = {}) {
  return {
    url: 'https://start.example/go', ua: 'default', capture_pdf: false, want_page: false,
    settle_ms: 3000, timeout_ms: 60000, hold_while: [], ...extra,
  };
}

test('settles after the URL has been stable for settle_ms; trail strips query and fragment', async () => {
  const env = setup({
    scripts: { 'https://start.example/go': [
      { atMs: 100, url: 'https://a.example/x?token=1#h' },
      { atMs: 500, url: 'https://b.example/y/z?q=1' },
    ] },
  });
  const out = await env.run(req());
  assert.equal(out.final_url, 'https://b.example/y/z?q=1');
  assert.deepStrictEqual(plain(out.url_trail), ['a.example/x', 'b.example/y/z']);
  assert.equal(out.ms, 3500, 'last change at 500 + settle 3000');
  assert.equal(out.pdf, null);
  assert.equal(out.page_html, null);
  assert.equal(out.status, null);
  assert.equal(env.instances[0].pageDataCalls.length, 0, 'want_page false: the page is not read');
  assert.deepStrictEqual(env.instances[0].loadCalls, ['https://start.example/go']);
});

test('a late redirect resets the settle timer', async () => {
  const env = setup({
    scripts: { 'https://start.example/go': [
      { atMs: 100, url: 'https://a.example/' },
      { atMs: 2900, url: 'https://b.example/' }, // just before a would have settled (3100)
    ] },
  });
  const out = await env.run(req());
  assert.equal(out.final_url, 'https://b.example/');
  assert.equal(out.ms, 6000, 'last change at 2900 + 3000, seen by the poll at 6000');
});

test('hold_while defers settling while the URL class is held, then settles on the next stable page', async () => {
  const env = setup({
    scripts: { 'https://start.example/go': [
      { atMs: 100, url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=x' },
      { atMs: 9000, url: 'https://journals.example/article' },
    ] },
    urlClass: (u) => (u.includes('go.openathens.net') ? 'openathens_redirector' : 'other'),
  });
  const out = await env.run(req({ hold_while: ['openathens_redirector'] }));
  assert.equal(out.final_url, 'https://journals.example/article');
  assert.equal(out.ms, 12000);
  assert.ok(env.host.urlCalls.includes('https://go.openathens.net/redirector/kcl.ac.uk?url=x'), 'the held URL was classified');
  // without hold_while the redirector would have settled at 3100/3250
  const env2 = setup({
    scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=x' }, { atMs: 9000, url: 'https://journals.example/article' }] },
  });
  const early = await env2.run(req());
  assert.equal(early.final_url, 'https://go.openathens.net/redirector/kcl.ac.uk?url=x');
});

test('timeout while navigating and held: finishes as settled (the inspector decides), page read when wanted', async () => {
  const env = setup({
    scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://login.example/idp' }] },
    urlClass: () => 'openathens_redirector',
  });
  const out = await env.run(req({ hold_while: ['openathens_redirector'], timeout_ms: 10000, want_page: true }));
  assert.equal(out.final_url, 'https://login.example/idp');
  assert.equal(out.ms >= 10000 && out.ms <= 10250, true);
  assert.equal(out.page_html, '<html>page</html>');
  assert.equal(out.status, 200);
  assert.deepStrictEqual(plain(env.instances[0].pageDataCalls), [{
    props: ['documentHTML', 'channelInfo'], opts: { timeout: env.ZFT.browser.PAGE_DATA_TIMEOUT_MS },
  }]);
});

test('timeout without any navigation is an OpError timeout and everything is torn down', async () => {
  const env = setup();
  const err = await env.run(req({ timeout_ms: 5000 })).catch((e) => e);
  assert.equal(err.kind, 'timeout');
  const hb = env.instances[0];
  assert.equal(hb.destroyed, 1);
  assert.equal(hb.listeners.length, 0, 'progress listener removed');
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
  assert.equal(env.timers.pendingCount(), 0);
});

test('page data failures leave page_html null (no error); status is only set from channelInfo', async () => {
  let env = setup({
    scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] },
    pageData: () => { throw new Error('Timed out getting documentHTML'); },
  });
  let out = await env.run(req({ want_page: true }));
  assert.equal(out.page_html, null);
  assert.equal(out.status, null);

  env = setup({
    scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] },
    pageData: { documentHTML: '<p>x</p>', channelInfo: null },
  });
  out = await env.run(req({ want_page: true }));
  assert.equal(out.page_html, '<p>x</p>');
  assert.equal(out.status, null);
});

test('PDF capture ends the load early with the Blob and the PDF URL; the capture and handler are cleaned up', async () => {
  const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://pub.example/landing' }] } });
  const h = env.start(req({ capture_pdf: true, want_page: true }));
  await flush();
  const hb = env.instances[0];
  assert.deepStrictEqual(env.mime.log, ['add']);
  env.timers.advance(1100);
  await flush();
  assert.equal(h.state.done, false);
  const blob = { size: 1234 };
  assert.equal(await env.mime.deliver(hb._browser, blob, 'https://pub.example/file.pdf'), true);
  const out = await env.drive(h);
  assert.equal(out.pdf, blob);
  assert.equal(out.final_url, 'https://pub.example/file.pdf');
  assert.equal(out.page_html, null, 'no page read once a PDF was captured');
  assert.equal(hb.pageDataCalls.length, 0);
  assert.equal(hb.destroyed, 1);
  assert.deepStrictEqual(env.mime.log, ['add', 'remove']);
  assert.equal(env.ZFT.pdfCapture.activeCount(), 0);
});

test('a PDF response with no location change at all is still captured', async () => {
  const env = setup();
  const h = env.start(req({ capture_pdf: true }));
  await flush();
  env.timers.advance(500);
  await flush();
  const blob = { size: 9 };
  await env.mime.deliver(env.instances[0]._browser, blob, 'https://pub.example/direct.pdf');
  const out = await env.drive(h);
  assert.equal(out.pdf, blob);
  assert.deepStrictEqual(plain(out.url_trail), []);
});

test('a PDF for another browser is not ours: the load keeps waiting and settles on its page', async () => {
  const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://pub.example/landing' }] } });
  const h = env.start(req({ capture_pdf: true }));
  await flush();
  assert.equal(await env.mime.deliver({ id: 'someone else' }, { size: 1 }, 'https://x/other.pdf'), false);
  const out = await env.drive(h);
  assert.equal(out.pdf, null);
  assert.equal(out.final_url, 'https://pub.example/landing');
});

test('capture_pdf without a MIME handler API is unsupported and the browser is destroyed', async () => {
  const env = setup({ mime: false });
  const err = await env.run(req({ capture_pdf: true })).catch((e) => e);
  assert.equal(err.kind, 'unsupported');
  assert.equal(env.instances[0].destroyed, 1);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
});

test('hb.load() rejecting, returning false or throwing is ignored: our listener drives the load', async () => {
  const variants = [
    () => Promise.reject(new Error('Page never loaded in hidden browser')),
    () => Promise.resolve(false),
    () => { throw new Error('sync boom'); },
  ];
  for (const load of variants) {
    const env = setup({ load, scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] } });
    const out = await env.run(req());
    assert.equal(out.final_url, 'https://a.example/');
  }
});

test('location events that are not navigations are ignored (inner frames, same-document, about:blank)', async () => {
  const env = setup({
    scripts: { 'https://start.example/go': [
      { atMs: 100, url: 'about:blank' },
      { atMs: 200, url: 'https://ad.example/frame', topLevel: false },
      { atMs: 300, url: 'https://a.example/page#section', flags: 1 },
    ] },
  });
  const err = await env.run(req({ timeout_ms: 4000 })).catch((e) => e);
  assert.equal(err.kind, 'timeout', 'none of them counted as a navigation');
});

test('the trail is de-duplicated and capped', async () => {
  const events = [];
  for (let i = 0; i < 15; i++) events.push({ atMs: 100 + i * 10, url: `https://h${i}.example/p` });
  events.splice(3, 0, { atMs: 131, url: 'https://h2.example/p?again=1' }); // same entry as the previous one
  const env = setup({ scripts: { 'https://start.example/go': events } });
  const out = await env.run(req());
  assert.equal(out.url_trail.length, env.ZFT.browser.URL_TRAIL_MAX);
  assert.equal(out.url_trail[2], 'h2.example/p');
  assert.equal(out.url_trail[3], 'h3.example/p', 'consecutive duplicate collapsed');
  assert.equal(out.final_url, 'https://h14.example/p');
});

test('browser error pages end the load: neterror is network, certerror is tls', async () => {
  for (const [spec, kind] of [
    ['about:neterror?e=dnsNotFound&u=https%3A//nope.invalid/', 'network'],
    ['about:certerror?e=nssFailure2&u=https%3A//bad.example/', 'tls'],
  ]) {
    const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: spec }] } });
    const err = await env.run(req()).catch((e) => e);
    assert.equal(err.kind, kind);
    assert.equal(env.instances[0].destroyed, 1);
  }
});

test('an error page reported under the FAILED address (LOCATION_CHANGE_ERROR_PAGE flag) ends the load too, instead of being returned as the site', async () => {
  // Zotero 10.0.6, verified in the login-probe harness: a DNS failure arrives as onLocationChange(failed URL, flags & 2).
  for (const [docUrl, kind] of [
    ['about:neterror?e=dnsNotFound&u=https%3A//nope.invalid/', 'network'],
    ['about:certerror?e=nssFailure2&u=https%3A//bad.example/', 'tls'],
    [undefined, 'network'],
  ]) {
    const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://nope.invalid/', flags: 2, docUrl }] } });
    const err = await env.run(req({ want_page: true })).catch((e) => e);
    assert.equal(err.kind, kind, String(docUrl));
    assert.equal(env.instances[0].destroyed, 1);
    assert.equal(env.instances[0].pageDataCalls.length, 0, 'the error page is never read as a page');
  }
  // an ordinary location change without the flag is untouched
  const ok = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://nope.invalid/', flags: 0 }] } });
  const out = await ok.run(req());
  assert.equal(out.final_url, 'https://nope.invalid/');
});

test('cancel destroys the browser, removes the listener and capture, frees the slot and rejects cancelled', async () => {
  const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] } });
  const h = env.start(req({ capture_pdf: true }));
  await flush();
  env.timers.advance(250);
  await flush();
  assert.equal(h.state.done, false);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 1, queued: 0 });
  h.scope.cancel('user');
  const err = await h.promise.catch((e) => e);
  assert.equal(err.kind, 'cancelled');
  const hb = env.instances[0];
  assert.equal(hb.destroyed, 1);
  assert.equal(hb.listeners.length, 0);
  assert.deepStrictEqual(env.mime.log, ['add', 'remove']);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
  assert.equal(env.timers.pendingCount(), 0, 'poll timer cleared');
});

test('a scope cancelled before the call never creates a browser', async () => {
  const env = setup();
  const scope = env.scope();
  scope.cancel('earlier');
  const h = env.start(req(), scope);
  const err = await h.promise.catch((e) => e);
  assert.equal(err.kind, 'cancelled');
  assert.equal(env.instances.length, 0);
});

test('cancel while the browser is still being created: rejected at once, destroyed as soon as creation finishes', async () => {
  const gate = {};
  gate.promise = new Promise((resolve) => { gate.resolve = resolve; });
  const env = setup({ createGate: gate });
  const h = env.start(req());
  await flush();
  assert.equal(env.instances.length, 1);
  h.scope.cancel('user');
  assert.equal((await h.promise.catch((e) => e)).kind, 'cancelled');
  assert.equal(env.instances[0].destroyed, 0, 'not yet created: nothing to destroy');
  gate.resolve();
  await flush();
  assert.equal(env.instances[0].destroyed, 1);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
});

test('a browser that fails to create is an internal error, is not destroyed, and frees its slot', async () => {
  const env = setup({ createGate: { promise: Promise.reject(new Error('HiddenFrame failed')) } });
  const err = await env.run(req()).catch((e) => e);
  assert.equal(err.kind, 'internal');
  assert.equal(env.instances[0].destroyed, 0);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
});

test('slot queue: at most MAX_BROWSERS live browsers, the 4th waits FIFO and starts when one finishes', async () => {
  const env = setup({
    scripts: { 'https://one.example/': [{ atMs: 100, url: 'https://one.example/landed' }] },
  });
  assert.equal(env.ZFT.browser.MAX_BROWSERS, 3);
  const slow = (n) => req({ url: `https://slow${n}.example/`, timeout_ms: 600000 });
  const first = env.start(req({ url: 'https://one.example/' }));
  const b = env.start(slow(2));
  const c = env.start(slow(3));
  const d = env.start(slow(4));
  const e = env.start(slow(5));
  await flush();
  assert.equal(env.instances.length, 3);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 3, queued: 2 });

  // `first` settles (100 + 3000): its slot goes to the OLDEST waiter (d), not e
  const out = await env.drive(first);
  assert.equal(out.final_url, 'https://one.example/landed');
  await flush();
  assert.equal(env.instances.length, 4);
  assert.deepStrictEqual(env.instances[3].loadCalls, ['https://slow4.example/']);
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 3, queued: 1 });

  // a waiter that is cancelled leaves the queue without ever creating a browser
  e.scope.cancel('user');
  assert.equal((await e.promise.catch((x) => x)).kind, 'cancelled');
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 3, queued: 0 });
  assert.equal(env.instances.length, 4);

  for (const h of [b, c, d]) h.scope.cancel('done');
  for (const h of [b, c, d]) assert.equal((await h.promise.catch((x) => x)).kind, 'cancelled');
  assert.deepStrictEqual(plain(env.ZFT.browser.stats()),{ live: 0, queued: 0 });
  assert.ok(env.instances.every((hb) => hb.destroyed === 1));
});

test('UA: plain sets customUserAgent, default sets no option', async () => {
  const ev = { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] };
  let env = setup({ scripts: ev });
  await env.run(req({ ua: 'plain' }));
  assert.deepStrictEqual(plain(env.instances[0].options), { customUserAgent: PLAIN_UA });
  env = setup({ scripts: ev });
  await env.run(req({ ua: 'default' }));
  assert.deepStrictEqual(plain(env.instances[0].options), {});
});

test('the progress listener listens for NOTIFY_LOCATION', async () => {
  const env = setup({ scripts: { 'https://start.example/go': [{ atMs: 100, url: 'https://a.example/' }] } });
  const h = env.start(req());
  await flush();
  assert.deepStrictEqual(env.instances[0].masks, [0x100]);
  await env.drive(h);
});

test('HiddenBrowser missing is unsupported; bad requests are internal', async () => {
  let env = setup({ noModule: true });
  assert.equal((await env.run(req()).catch((e) => e)).kind, 'unsupported');
  env = setup({ badModule: true });
  assert.equal((await env.run(req()).catch((e) => e)).kind, 'unsupported');
  env = setup();
  assert.equal((await env.run(req({ url: 'file:///etc/passwd' })).catch((e) => e)).kind, 'internal');
  assert.equal((await env.run(req({ timeout_ms: 0 })).catch((e) => e)).kind, 'internal');
  assert.equal(env.instances.length, 0);
  assert.equal((await env.ZFT.browser.load(req(), { host: env.host }).catch((e) => e)).kind, 'internal', 'ctx.scope missing');
});

test('trailEntry: host + path, no scheme, query, fragment or credentials; path cut at 80', () => {
  const { trailEntry } = setup().ZFT.browser;
  assert.equal(trailEntry('https://a.b/c/d?x=1#h'), 'a.b/c/d');
  assert.equal(trailEntry('http://a.b/'), 'a.b');
  assert.equal(trailEntry('https://a.b'), 'a.b');
  assert.equal(trailEntry('https://user:pw@a.b:8080/x/y'), 'a.b:8080/x/y');
  assert.equal(trailEntry('https://a.b/' + 'p'.repeat(200)), 'a.b/' + 'p'.repeat(79));
  assert.equal(trailEntry('about:neterror?e=dns&u=https%3A//x/'), 'about:neterror');
  assert.equal(trailEntry('not a url?x=1#y'), 'not a url');
  assert.equal(trailEntry(''), '');
  assert.equal(trailEntry(undefined), '');
});
