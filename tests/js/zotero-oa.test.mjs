// ZFT.zoteroOa: Zotero's private Unpaywall mirror via Zotero.Utilities.Internal.getOpenAccessPDFURLs.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeTimers } from './helpers/fakes.mjs';

const flush = () => new Promise((resolve) => setImmediate(resolve));

const unhandled = [];
process.on('unhandledRejection', (reason) => unhandled.push(reason));
test.afterEach(async () => {
  await flush();
  assert.deepStrictEqual(unhandled.splice(0).map(String), [], 'no unhandled rejections');
});

class TimeoutException extends Error {}
class BrowserOfflineException extends Error {}
class UnexpectedStatusException extends Error {
  constructor(status) { super('HTTP POST failed with status code ' + status); this.status = status; this.xmlhttp = { status }; }
}

function setup(getOpenAccessPDFURLs, { noInternal = false } = {}) {
  const timers = makeFakeTimers();
  const calls = [];
  const Zotero = makeFakeZotero({
    HTTP: { TimeoutException, BrowserOfflineException, UnexpectedStatusException },
    Utilities: noInternal ? {} : {
      Internal: getOpenAccessPDFURLs === undefined ? {} : {
        getOpenAccessPDFURLs: (...args) => { calls.push(args); return getOpenAccessPDFURLs(...args); },
      },
    },
  });
  const { ZFT } = loadPlugin({
    files: ['util.js', 'net.js', 'zotero-oa.js'],
    globals: { Zotero, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout, performance: { now: () => timers.now } },
  });
  return { ZFT, timers, calls, scope: () => new ZFT.util.CancelScope() };
}

const kindOf = (p) => p.then(() => 'resolved', (e) => e.kind);

test('maps {url, pageURL, version} to {url, page_url, version}, version verbatim, and asks with the lookup timeout', async () => {
  const env = setup(async () => [
    { url: 'https://repo.example/a.pdf', pageURL: 'https://repo.example/a', version: 'publishedVersion' },
    { url: 'https://repo.example/b.pdf', version: 'acceptedVersion' },
    { pageURL: 'https://repo.example/landing-only', version: 'submittedVersion' },
    { url: 'https://repo.example/c.pdf' },
  ]);
  const out = await env.ZFT.zoteroOa.lookup({ doi: '  10.1234/ABC  ' }, { scope: env.scope() });
  assert.deepStrictEqual(plain(out), [
    { url: 'https://repo.example/a.pdf', page_url: 'https://repo.example/a', version: 'publishedVersion' },
    { url: 'https://repo.example/b.pdf', page_url: null, version: 'acceptedVersion' },
    { url: '', page_url: 'https://repo.example/landing-only', version: 'submittedVersion' },
    { url: 'https://repo.example/c.pdf', page_url: null, version: null },
  ]);
  assert.equal(env.calls.length, 1);
  assert.equal(env.calls[0][0], '10.1234/ABC');
  assert.deepStrictEqual(plain(env.calls[0][1]), { timeout: 10000 });
  assert.equal(env.ZFT.zoteroOa.LOOKUP_TIMEOUT_MS, 10000);
  assert.equal(env.timers.pendingCount(), 0);
});

test('drops empty and malformed entries; non-arrays and empty arrays are an empty result, not an error', async () => {
  const env = setup(async () => [
    {}, null, 'x', 7, { url: '', pageURL: '' }, { url: '   ', pageURL: null }, { url: 5 },
    { url: 'https://ok.example/p.pdf', version: '' },
  ]);
  const out = await env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: env.scope() });
  assert.deepStrictEqual(plain(out), [{ url: 'https://ok.example/p.pdf', page_url: null, version: null }]);
  for (const raw of [[], null, undefined, { not: 'an array' }]) {
    const e = setup(async () => raw);
    assert.deepStrictEqual(plain(await e.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: e.scope() })), []);
  }
});

test('API missing is unsupported (and Zotero is not called)', async () => {
  for (const env of [setup(undefined), setup(undefined, { noInternal: true })]) {
    assert.equal(await kindOf(env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: env.scope() })), 'unsupported');
  }
});

test('thrown errors are mapped through ZFT.net.classifyError', async () => {
  const cases = [
    [new TimeoutException('Request timed out after 10000 ms'), 'timeout'],
    [new BrowserOfflineException('offline'), 'offline'],
    [new UnexpectedStatusException(503), 'network'],
    [new Error("Invalid DOI ''"), 'internal'],
    [new TypeError('NetworkError when attempting to fetch resource.'), 'network'],
  ];
  for (const [error, kind] of cases) {
    const env = setup(async () => { throw error; });
    assert.equal(await kindOf(env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: env.scope() })), kind, String(error));
    assert.equal(env.timers.pendingCount(), 0);
  }
  const sync = setup(() => { throw new Error('sync boom'); });
  assert.equal(await kindOf(sync.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: sync.scope() })), 'internal');
});

test('cancel rejects cancelled at once and a late result is ignored', async () => {
  let resolveLate;
  const env = setup(() => new Promise((resolve) => { resolveLate = resolve; }));
  const scope = env.scope();
  const p = env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope });
  await flush();
  scope.cancel('user');
  assert.equal(await kindOf(p), 'cancelled');
  resolveLate([{ url: 'https://late.example/x.pdf' }]);
  await flush();
  assert.equal(env.timers.pendingCount(), 0, 'backstop timer cleared');
});

test('a scope cancelled before the call rejects cancelled without calling Zotero', async () => {
  const env = setup(async () => []);
  const scope = env.scope();
  scope.cancel('earlier');
  assert.equal(await kindOf(env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope })), 'cancelled');
  assert.equal(env.calls.length, 0);
});

test('JS backstop: a lookup that never answers is abandoned as a timeout', async () => {
  const env = setup(() => new Promise(() => {}));
  const backstop = env.ZFT.zoteroOa.LOOKUP_BACKSTOP_MS;
  assert.ok(backstop > env.ZFT.zoteroOa.LOOKUP_TIMEOUT_MS + 5000, 'longer than Zotero\'s own retry budget');
  let settled = false;
  const p = env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, { scope: env.scope() }).catch((e) => { settled = true; return e; });
  await flush();
  env.timers.advance(backstop - 1);
  await flush();
  assert.equal(settled, false);
  env.timers.advance(1);
  assert.equal((await p).kind, 'timeout');
  assert.equal(env.timers.pendingCount(), 0);
});

test('bad input: an empty DOI or missing scope is internal', async () => {
  const env = setup(async () => []);
  for (const doi of ['', '   ', undefined, 42]) {
    assert.equal(await kindOf(env.ZFT.zoteroOa.lookup({ doi }, { scope: env.scope() })), 'internal');
  }
  assert.equal(await kindOf(env.ZFT.zoteroOa.lookup(null, { scope: env.scope() })), 'internal');
  assert.equal(await kindOf(env.ZFT.zoteroOa.lookup({ doi: '10.1/x' }, {})), 'internal');
  assert.equal(env.calls.length, 0);
});
