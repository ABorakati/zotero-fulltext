// ZFT.translators: Zotero's web translators on a page body we already hold. Fakes: Zotero.Utilities.Internal, a BodyStore
// stand-in with retain/release counters, fake timers for the TRANSLATE_TIMEOUT_MS race.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeTimers } from './helpers/fakes.mjs';

const flush = () => new Promise((resolve) => setImmediate(resolve));
const bytes = (s) => new TextEncoder().encode(s);

// Unhandled rejections would mean a translator promise escaped our handling.
const unhandled = [];
process.on('unhandledRejection', (reason) => unhandled.push(reason));
test.afterEach(async () => {
  await flush();
  assert.deepStrictEqual(unhandled.splice(0).map(String), [], 'no unhandled rejections');
});

function fakeDoc(n = 2) {
  const noscripts = Array.from({ length: n }, (_, i) => ({ id: i, removed: false, remove() { this.removed = true; } }));
  return {
    noscripts,
    querySelectorAll: (sel) => (sel === 'noscript' ? noscripts.filter((x) => !x.removed) : []),
  };
}

function fakeBodies(map = { 7: bytes('<html><noscript>x</noscript></html>') }) {
  const refs = {};
  return {
    refs,
    get: (h) => (map[h] ? { handle: h, kind: 'page', len: map[h].length } : null),
    retain(h) { refs[h] = (refs[h] || 0) + 1; return h; },
    async release(h) { refs[h] = (refs[h] || 0) - 1; },
    async readBytes(h) {
      if (map[h] instanceof Error) throw map[h];
      return map[h];
    },
  };
}

/** @param {object} [o] internal: overrides for Zotero.Utilities.Internal; extra: more Zotero/global props */
function setup(o = {}) {
  const timers = makeFakeTimers();
  const log = { blobCalls: [], docs: [], translateCalls: 0 };
  const internal = {
    async blobToHTMLDocument(blob, url) {
      const doc = fakeDoc(2);
      doc.url = url;
      doc.blobType = blob.type;
      doc.text = new TextDecoder().decode(new Uint8Array(await blob.arrayBuffer()));
      log.blobCalls.push({ type: blob.type, url });
      log.docs.push(doc);
      return doc;
    },
    async getFileFromDocument(doc) {
      log.translateCalls++;
      log.noscriptGoneAtCall = doc.noscripts.every((n) => n.removed);
      return { title: 'Full Text PDF', mimeType: 'application/pdf', url: 'https://pub.example/paper.pdf' };
    },
    ...(o.internal || {}),
  };
  for (const k of o.drop || []) delete internal[k];
  const Zotero = makeFakeZotero({ Utilities: { Internal: internal }, ...(o.zotero || {}) });
  const { ZFT } = loadPlugin({
    files: ['util.js', 'bodies.js', 'net.js', 'translators.js'],
    globals: {
      Zotero, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout,
      performance: { now: () => timers.now }, ...(o.globals || {}),
    },
  });
  return { ZFT, Zotero, internal, timers, log, scope: () => new ZFT.util.CancelScope() };
}

const eff = (extra = {}) => ({ page: { handle: 7, len: 40 }, url: 'https://pub.example/article/1', ...extra });

test('maps the translator hit to {url, mime, title}; noscript is gone before the translator sees the DOM', async () => {
  const env = setup();
  const bodies = fakeBodies();
  const out = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies });
  assert.deepStrictEqual(plain(out.found), { url: 'https://pub.example/paper.pdf', mime: 'application/pdf', title: 'Full Text PDF' });
  assert.equal(typeof out.ms, 'number');
  assert.equal(env.log.translateCalls, 1);
  assert.equal(env.log.noscriptGoneAtCall, true);
  assert.equal(env.log.blobCalls[0].url, 'https://pub.example/article/1', 'the final page URL is passed through');
  assert.deepStrictEqual(bodies.refs, { 7: 0 }, 'body retained for the run and released after it');
  assert.equal(env.timers.pendingCount(), 0);
});

test('no match, or a hit without a url, is found:null', async () => {
  for (const result of [false, null, undefined, { title: 't', mimeType: 'application/pdf' }, { url: '' }]) {
    const env = setup({ internal: { getFileFromDocument: async () => result } });
    const out = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() });
    assert.equal(out.found, null);
  }
});

test('any failure is found:null with a duration, never a throw', async () => {
  const cases = {
    translatorThrows: setup({ internal: { getFileFromDocument: async () => { throw new Error('translator exploded'); } } }),
    translatorThrowsSync: setup({ internal: { getFileFromDocument: () => { throw new Error('sync'); } } }),
    parseFails: setup({ internal: { blobToHTMLDocument: async () => { throw new Error('bad html'); } } }),
  };
  for (const [name, env] of Object.entries(cases)) {
    const out = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() });
    assert.equal(out.found, null, name);
    assert.equal(typeof out.ms, 'number', name);
  }
  // body read failure and unknown handle
  const env = setup();
  const failing = fakeBodies({ 7: new Error('disk gone') });
  assert.equal((await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: failing })).found, null);
  assert.deepStrictEqual(failing.refs, { 7: 0 }, 'released even though reading failed');
  const unknown = fakeBodies({});
  assert.equal((await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: unknown })).found, null);
  assert.deepStrictEqual(unknown.refs, {}, 'nothing to retain');
  assert.equal(env.timers.pendingCount(), 0);
});

test('TRANSLATE_TIMEOUT_MS race: a translator that never answers yields found:null, frees its slot and releases the body', async () => {
  const env = setup({ internal: { getFileFromDocument: () => new Promise(() => {}) } });
  const bodies = fakeBodies();
  let result;
  const p = env.ZFT.translators.run(eff(), { scope: env.scope(), bodies }).then((r) => { result = r; });
  await flush();
  assert.equal(env.ZFT.translators.TRANSLATE_TIMEOUT_MS, 15000);
  env.timers.advance(14999);
  await flush();
  assert.equal(result, undefined);
  env.timers.advance(1);
  await p;
  assert.equal(result.found, null);
  assert.deepStrictEqual(bodies.refs, { 7: 0 });
  assert.equal(env.timers.pendingCount(), 0);
  // the slot is free again
  env.internal.getFileFromDocument = async () => ({ title: 't', mimeType: 'application/pdf', url: 'https://x/y.pdf' });
  const next = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies });
  assert.equal(next.found.url, 'https://x/y.pdf');
});

test('a translator that fails after its timeout does not cause an unhandled rejection', async () => {
  let rejectLate;
  const env = setup({ internal: { getFileFromDocument: () => new Promise((_, reject) => { rejectLate = reject; }) } });
  let result;
  const p = env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() }).then((r) => { result = r; });
  await flush();
  env.timers.advance(15000);
  await p;
  assert.equal(result.found, null);
  rejectLate(new Error('too late'));
  await flush();
});

test('concurrency: at most MAX_CONCURRENT translators run, the rest queue FIFO', async () => {
  const gates = [];
  const env = setup({
    internal: {
      getFileFromDocument: (doc) => new Promise((resolve) => { gates.push({ doc, resolve }); }),
    },
  });
  assert.equal(env.ZFT.translators.MAX_CONCURRENT, 2);
  const bodies = fakeBodies();
  const runs = [1, 2, 3, 4].map((i) => env.ZFT.translators.run(eff({ url: `https://pub.example/${i}` }), { scope: env.scope(), bodies }));
  await flush();
  assert.equal(gates.length, 2, 'two running, two queued');
  assert.equal(bodies.refs[7], 2, 'queued runs have not retained the body yet');

  gates[0].resolve({ url: 'https://pub.example/a.pdf', mimeType: 'application/pdf', title: 'A' });
  await flush();
  assert.equal(gates.length, 3, 'the oldest waiter started');
  assert.equal(gates[2].doc.url, 'https://pub.example/3');
  gates[1].resolve(false);
  await flush();
  assert.equal(gates.length, 4);
  gates[2].resolve(false);
  gates[3].resolve(false);
  const results = await Promise.all(runs);
  assert.equal(results[0].found.title, 'A');
  assert.deepStrictEqual(results.slice(1).map((r) => r.found), [null, null, null]);
  assert.deepStrictEqual(bodies.refs, { 7: 0 });
});

test('cancel: a queued run leaves the queue; a running run rejects cancelled and releases everything', async () => {
  const gates = [];
  const env = setup({ internal: { getFileFromDocument: () => new Promise((resolve) => { gates.push(resolve); }) } });
  const bodies = fakeBodies();
  const s1 = env.scope();
  const s2 = env.scope();
  const s3 = env.scope();
  const r1 = env.ZFT.translators.run(eff(), { scope: s1, bodies });
  const r2 = env.ZFT.translators.run(eff(), { scope: s2, bodies });
  const r3 = env.ZFT.translators.run(eff(), { scope: s3, bodies });
  const settle = (p) => p.then((v) => v, (e) => e);
  await flush();
  assert.equal(gates.length, 2);

  s3.cancel('queued gone');
  assert.equal((await settle(r3)).kind, 'cancelled');
  s1.cancel('user');
  assert.equal((await settle(r1)).kind, 'cancelled');
  assert.deepStrictEqual(bodies.refs, { 7: 1 }, 'only the still-running run holds the body');

  gates[1](false);
  assert.equal((await settle(r2)).found, null);
  assert.deepStrictEqual(bodies.refs, { 7: 0 });
  gates[0]({ url: 'https://late', mimeType: 'application/pdf', title: 't' }); // late result of the cancelled run: ignored
  await flush();
  assert.equal(env.timers.pendingCount(), 0);
});

test('a scope cancelled before the call rejects cancelled without reading anything', async () => {
  const env = setup();
  const bodies = fakeBodies();
  const scope = env.scope();
  scope.cancel('earlier');
  const err = await env.ZFT.translators.run(eff(), { scope, bodies }).catch((e) => e);
  assert.equal(err.kind, 'cancelled');
  assert.equal(env.log.translateCalls, 0);
  assert.deepStrictEqual(bodies.refs, {});
});

test('missing Zotero internals are found:null, never unsupported (bonus source)', async () => {
  for (const drop of [['getFileFromDocument'], ['getFileFromDocument', 'blobToHTMLDocument']]) {
    const env = setup({ drop });
    const out = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() });
    assert.equal(out.found, null);
  }
  const env = setup();
  env.Zotero.Utilities = undefined;
  assert.equal((await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() })).found, null);
});

test('malformed effects are found:null', async () => {
  const env = setup();
  const bodies = fakeBodies();
  for (const e of [null, {}, { page: { handle: 7 } }, { url: 'https://x/' }]) {
    assert.equal((await env.ZFT.translators.run(e, { scope: env.scope(), bodies })).found, null);
  }
});

// ----------------------------------------------------------------------------------------------- parsePage

test('parsePage removes every <noscript> element and returns the document', async () => {
  const env = setup();
  const doc = await env.ZFT.translators.parsePage(bytes('<html><noscript>a</noscript><noscript>b</noscript></html>'), 'https://pub.example/x');
  assert.equal(doc.noscripts.length, 2);
  assert.equal(doc.noscripts.every((n) => n.removed), true);
  assert.equal(doc.querySelectorAll('noscript').length, 0);
});

test('parsePage passes the charset through the Blob type: Content-Type first, then <meta charset>, ignoring invalid labels', async () => {
  const env = setup();
  const { parsePage } = env.ZFT.translators;
  const type = async (body, contentType) => (await parsePage(bytes(body), 'https://x/', contentType)).blobType;
  assert.equal(await type('<html></html>'), 'text/html');
  // (Blob lower-cases its type)
  assert.equal(await type('<html></html>', 'text/html; charset=ISO-8859-1'), 'text/html; charset=iso-8859-1');
  assert.equal(await type('<html><head><meta charset="windows-1252"></head></html>'), 'text/html; charset=windows-1252');
  assert.equal(await type('<html><head><meta http-equiv="Content-Type" content="text/html; charset=utf-8"></head></html>'), 'text/html; charset=utf-8');
  assert.equal(await type('<html></html>', 'text/html; charset=not-a-real-charset'), 'text/html');
  assert.equal(await type('<html><meta charset=bogus-9></html>'), 'text/html');
  // Content-Type wins over the meta tag
  assert.equal(await type('<html><meta charset="windows-1252"></html>', 'text/html; charset=utf-8'), 'text/html; charset=utf-8');
});

test('parsePage without blobToHTMLDocument falls back to DOMParser + wrapDocument with the right decoding', async () => {
  const parsed = [];
  class FakeDOMParser {
    parseFromString(text, type) { const doc = fakeDoc(1); doc.text = text; doc.type = type; parsed.push(doc); return doc; }
  }
  const env = setup({
    drop: ['blobToHTMLDocument'],
    globals: { DOMParser: FakeDOMParser },
    zotero: { HTTP: { wrapDocument: (doc, url) => { doc.wrappedFor = url; return doc; } } },
  });
  // 0xE9 is "e acute" in windows-1252
  const latin = new Uint8Array([...bytes('<html><meta charset="windows-1252"><body>caf'), 0xe9, ...bytes('</body></html>')]);
  const doc = await env.ZFT.translators.parsePage(latin, 'https://pub.example/y');
  assert.equal(doc.wrappedFor, 'https://pub.example/y');
  assert.equal(doc.type, 'text/html');
  assert.match(doc.text, /café/);
  assert.equal(doc.noscripts[0].removed, true);
});

test('parsePage with no parser at all is unsupported (run() turns that into found:null)', async () => {
  const env = setup({ drop: ['blobToHTMLDocument'] });
  const err = await env.ZFT.translators.parsePage(bytes('<html></html>'), 'https://x/').catch((e) => e);
  assert.equal(err.kind, 'unsupported');
  const out = await env.ZFT.translators.run(eff(), { scope: env.scope(), bodies: fakeBodies() });
  assert.equal(out.found, null);
});
