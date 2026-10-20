// ZFT.net: Zotero.HTTP wrapper. Fake Zotero.HTTP records the options it receives and scripts the outcomes; fake timers
// drive the JS backstop. Real Zotero behaviour (progress events, securityInfo, reader.cancel) is for the harness.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeFs, makeFakeTimers } from './helpers/fakes.mjs';

const flush = () => new Promise((resolve) => setImmediate(resolve));
const bytes = (s) => new TextEncoder().encode(s);

// Zotero's exception shapes: plain constructor functions with no `name` of their own.
class TimeoutException extends Error { constructor(ms) { super('Request timed out' + (ms ? ` after ${ms} ms` : '')); } }
class CancelledException extends Error { constructor() { super('Request cancelled'); } }
class BrowserOfflineException extends Error { constructor() { super('Request could not be completed because Zotero is offline'); } }
class SecurityException extends Error {}
class UnexpectedStatusException extends Error {
  constructor(xmlhttp, status) { super('HTTP failed with status code ' + status); this.xmlhttp = xmlhttp; this.status = status; }
}

function setup(httpOverrides = {}) {
  const timers = makeFakeTimers();
  const fs = makeFakeFs();
  const HTTP = {
    TimeoutException, CancelledException, BrowserOfflineException, SecurityException, UnexpectedStatusException,
    ...httpOverrides,
  };
  const Zotero = makeFakeZotero({ HTTP, VersionHeader: { getPlainFirefoxUA: () => 'Mozilla/5.0 (Plain) Firefox/140.0' } });
  const { ZFT, ctx } = loadPlugin({
    files: ['util.js', 'bodies.js', 'net.js'],
    globals: {
      Zotero, IOUtils: fs.IOUtils, PathUtils: fs.PathUtils,
      setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout,
      performance: { now: () => timers.now },
    },
  });
  return { ZFT, ctx, timers, fs, HTTP, Zotero, scope: () => new ZFT.util.CancelScope() };
}

function req(extra = {}) {
  return {
    method: 'get', url: 'https://example.org/a', headers: {}, ua: 'default', timeout_ms: 5000,
    follow_redirects: true, max_bytes: 1_000_000, ...extra,
  };
}

/** A realistic XMLHttpRequest stand-in. */
function fakeXhr({ status = 200, url = 'https://example.org/a', body = bytes('{}'), headers = {}, channel } = {}) {
  const listeners = {};
  return {
    status, responseURL: url, readyState: 4, channel,
    response: body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
    getAllResponseHeaders: () => Object.entries(headers).map(([k, v]) => `${k}: ${v}`).join('\r\n') + '\r\n',
    getResponseHeader(n) {
      const hit = Object.keys(headers).find((k) => k.toLowerCase() === String(n).toLowerCase());
      return hit ? headers[hit] : null;
    },
    addEventListener(type, fn) { (listeners[type] ||= []).push(fn); },
    emit(type, ev) { for (const fn of listeners[type] || []) fn(ev); },
  };
}

/** Zotero.HTTP.request that records its call and resolves with `xhr` (after the observer/canceller hand-shake). */
function recordingRequest(calls, xhrFor) {
  return async (method, url, opts) => {
    calls.push({ method, url, opts });
    const xhr = xhrFor(url, opts);
    if (opts.requestObserver) opts.requestObserver(xhr);
    if (opts.cancellerReceiver) opts.cancellerReceiver(() => {});
    return xhr;
  };
}

async function kindOf(promise) {
  try { await promise; } catch (e) { return e.kind; }
  return 'resolved';
}

test('memory sink: exact Zotero.HTTP.request options and a plain result', async () => {
  const calls = [];
  const env = setup({ request: recordingRequest(calls, () => fakeXhr({ body: bytes('{"ok":1}'), headers: { 'Content-Type': 'application/json' } })) });
  const res = await env.ZFT.net.request(req({ method: 'post', body: '{"q":1}', headers: { Accept: 'application/json' } }), { scope: env.scope(), sink: 'memory' });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, 'POST');
  assert.equal(calls[0].url, 'https://example.org/a');
  const o = calls[0].opts;
  assert.deepStrictEqual(Object.keys(o).sort(), [
    'body', 'cancellerReceiver', 'errorDelayMax', 'followRedirects', 'headers', 'noRetryOnThrottle',
    'requestObserver', 'responseType', 'successCodes', 'timeout',
  ]);
  assert.equal(o.responseType, 'arraybuffer');
  assert.equal(o.timeout, 5000);
  assert.equal(o.successCodes, false);
  assert.equal(o.errorDelayMax, 0);
  assert.equal(o.noRetryOnThrottle, true);
  assert.equal(o.followRedirects, true);
  assert.equal(o.body, '{"q":1}');
  assert.deepStrictEqual(plain(o.headers), { Accept: 'application/json' });
  assert.equal(res.status, 200);
  assert.equal(res.final_url, 'https://example.org/a');
  assert.equal(new TextDecoder().decode(res.body), '{"ok":1}');
  assert.equal(res.bytes, 8);
  assert.equal(res.path, null);
  assert.equal(res.errorBody, null);
  assert.deepStrictEqual(plain(res.headers), { 'content-type': 'application/json' });
});

test('GET without a body leaves the body option out and follow_redirects false is passed through', async () => {
  const calls = [];
  const env = setup({ request: recordingRequest(calls, () => fakeXhr()) });
  await env.ZFT.net.request(req({ follow_redirects: false }), { scope: env.scope(), sink: 'memory' });
  assert.equal('body' in calls[0].opts, false);
  assert.equal(calls[0].opts.followRedirects, false);
});

test('User-Agent policy: default sends none, plain sends the plain Firefox UA and replaces a caller UA', () => {
  const env = setup();
  const { buildHeaders } = env.ZFT.net;
  assert.deepStrictEqual(plain(buildHeaders(req({ ua: 'default', headers: { Accept: 'x' } }))), { Accept: 'x' });
  assert.deepStrictEqual(plain(buildHeaders(req({ ua: 'plain', headers: { Accept: 'x' } }))), {
    Accept: 'x', 'User-Agent': 'Mozilla/5.0 (Plain) Firefox/140.0',
  });
  const replaced = plain(buildHeaders(req({ ua: 'plain', headers: { 'user-agent': 'custom/1' } })));
  assert.deepStrictEqual(replaced, { 'User-Agent': 'Mozilla/5.0 (Plain) Firefox/140.0' });
  // default keeps a caller-supplied UA untouched
  assert.deepStrictEqual(plain(buildHeaders(req({ ua: 'default', headers: { 'User-Agent': 'custom/1' } }))), { 'User-Agent': 'custom/1' });
});

test('request header whitelist drops hop-by-hop headers case-insensitively', () => {
  const env = setup();
  const out = plain(env.ZFT.net.buildHeaders(req({
    headers: {
      Host: 'x', 'content-length': '3', COOKIE: 'a=b', Connection: 'keep-alive', 'Accept-Encoding': 'gzip',
      Accept: 'text/html', 'X-Custom': 'yes', 'Referer': 'https://r.example/',
    },
  })));
  assert.deepStrictEqual(out, { Accept: 'text/html', 'X-Custom': 'yes', Referer: 'https://r.example/' });
});

test('plain UA tolerates a missing VersionHeader (the default UA is used instead)', () => {
  const env = setup();
  env.Zotero.VersionHeader = undefined;
  assert.deepStrictEqual(plain(env.ZFT.net.buildHeaders(req({ ua: 'plain', headers: { A: '1' } }))), { A: '1' });
});

test('response headers: whitelist, lower-case names, joined repeats, values cut at 512, no set-cookie', async () => {
  const long = 'x'.repeat(900);
  const env = setup({
    request: recordingRequest([], () => fakeXhr({
      headers: {
        'Content-Type': 'text/html; charset=utf-8', 'SERVER': 'cloudflare', 'Set-Cookie': 'sid=secret', 'X-Powered-By': 'php',
        'Retry-After': '7', 'Akamai-GRN': '0.1.2', 'Link': long, 'CF-Mitigated': 'challenge',
      },
    })),
  });
  const res = await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' });
  const h = plain(res.headers);
  assert.equal(h['content-type'], 'text/html; charset=utf-8');
  assert.equal(h.server, 'cloudflare');
  assert.equal(h['retry-after'], '7');
  assert.equal(h['akamai-grn'], '0.1.2');
  assert.equal(h['cf-mitigated'], 'challenge');
  assert.equal(h.link.length, 512);
  assert.equal('set-cookie' in h, false);
  assert.equal('x-powered-by' in h, false);
});

test('4xx and 5xx are results, not errors (429 keeps retry-after)', async () => {
  for (const status of [403, 404, 429, 503]) {
    const env = setup({ request: recordingRequest([], () => fakeXhr({ status, body: bytes('<html>no</html>'), headers: { 'Retry-After': '30' } })) });
    const res = await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' });
    assert.equal(res.status, status);
    assert.equal(plain(res.headers)['retry-after'], '30');
    assert.equal(new TextDecoder().decode(res.body), '<html>no</html>');
  }
});

test('unfollowed redirect: Zotero resolves a fake {status, getResponseHeader}; Location is resolved against the request URL', async () => {
  const fake = {
    status: 302,
    getResponseHeader: (n) => ({ location: '/next?x=1', 'retry-after': null, server: 'nginx' })[n.toLowerCase()] ?? null,
  };
  const calls = [];
  const env = setup({ request: async (m, u, opts) => { calls.push(opts); return fake; } });
  const res = await env.ZFT.net.request(req({ follow_redirects: false, url: 'https://example.org/dir/page' }), { scope: env.scope(), sink: 'memory' });
  assert.equal(calls[0].followRedirects, false);
  assert.equal(res.status, 302);
  assert.equal(res.final_url, 'https://example.org/dir/page');
  assert.equal(res.headers.location, 'https://example.org/next?x=1');
  assert.equal(res.headers.server, 'nginx');
  assert.equal(res.body.length, 0);
  assert.equal(res.bytes, 0);
});

test('status 0 resolves under successCodes:false: classified as tls or network, never returned as a response', async () => {
  const mk = (channel) => setup({ request: recordingRequest([], () => fakeXhr({ status: 0, url: '', channel })) });

  let env = mk({ securityInfo: { errorCode: 0x805a2000 } });
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' })), 'tls');

  env = mk({ status: 0x804b000e });
  env.HTTP.checkSecurity = () => { throw new SecurityException('The connection is not secure'); };
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' })), 'tls');

  env = mk({ status: 0x804b000e });
  env.HTTP.checkSecurity = () => {}; // security state is fine
  const err = await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' }).catch((e) => e);
  assert.equal(err.kind, 'network');
  assert.match(err.message, /0x804b000e/);
});

test('size cap, memory sink: a progress event past max_bytes cancels the request and rejects too_large', async () => {
  let cancelled = 0;
  const env = setup({
    request: (m, u, opts) => new Promise((resolve, reject) => {
      const xhr = fakeXhr();
      opts.requestObserver(xhr);
      opts.cancellerReceiver(() => { cancelled++; reject(new CancelledException()); });
      setImmediate(() => xhr.emit('progress', { loaded: 5000 }));
    }),
  });
  assert.equal(await kindOf(env.ZFT.net.request(req({ max_bytes: 1000 }), { scope: env.scope(), sink: 'memory' })), 'too_large');
  assert.equal(cancelled, 1);
});

test('size cap, memory sink: an announced Content-Length past max_bytes aborts at HEADERS_RECEIVED', async () => {
  let cancelled = 0;
  const env = setup({
    request: (m, u, opts) => new Promise((resolve, reject) => {
      const xhr = fakeXhr({ headers: { 'Content-Length': '9999999' } });
      xhr.readyState = 2;
      opts.requestObserver(xhr);
      opts.cancellerReceiver(() => { cancelled++; reject(new CancelledException()); });
      setImmediate(() => xhr.emit('readystatechange'));
    }),
  });
  assert.equal(await kindOf(env.ZFT.net.request(req({ max_bytes: 1000 }), { scope: env.scope(), sink: 'memory' })), 'too_large');
  assert.equal(cancelled, 1);
});

test('size cap, memory sink: a finished body over max_bytes is rejected even if no progress event fired', async () => {
  const env = setup({ request: recordingRequest([], () => fakeXhr({ body: new Uint8Array(2048) })) });
  assert.equal(await kindOf(env.ZFT.net.request(req({ max_bytes: 1000 }), { scope: env.scope(), sink: 'memory' })), 'too_large');
  const ok = setup({ request: recordingRequest([], () => fakeXhr({ body: new Uint8Array(2048) })) });
  const res = await ok.ZFT.net.request(req({ max_bytes: 0 }), { scope: ok.scope(), sink: 'memory' });
  assert.equal(res.bytes, 2048, 'max_bytes 0 means no cap');
});

test('abort cause: scope cancel mid-flight rejects cancelled at once and cancels the Zotero request', async () => {
  let cancelled = 0;
  const env = setup({
    request: (m, u, opts) => new Promise((resolve, reject) => {
      opts.cancellerReceiver(() => { cancelled++; reject(new CancelledException()); });
    }),
  });
  const scope = env.scope();
  const p = env.ZFT.net.request(req(), { scope, sink: 'memory' });
  await flush();
  scope.cancel('user');
  const err = await p.catch((e) => e);
  assert.equal(err.kind, 'cancelled');
  assert.equal(cancelled, 1);
  assert.equal(env.timers.pendingCount(), 0, 'backstop timer cleared');
});

test('abort cause: the first cause wins and the Zotero cancel function runs once', async () => {
  let cancelled = 0;
  const env = setup({
    request: (m, u, opts) => new Promise((resolve, reject) => {
      const xhr = fakeXhr();
      opts.requestObserver(xhr);
      opts.cancellerReceiver(() => { cancelled++; reject(new CancelledException()); });
      setImmediate(() => xhr.emit('progress', { loaded: 5000 })); // cap hit first ...
    }),
  });
  const scope = env.scope();
  const p = env.ZFT.net.request(req({ max_bytes: 1000 }), { scope, sink: 'memory' }).catch((e) => e);
  await flush();
  scope.cancel('user'); // ... then the user cancels
  env.timers.advance(60000); // ... and the backstop would have fired too
  assert.equal((await p).kind, 'too_large');
  assert.equal(cancelled, 1);
});

test('abort cause: Zotero reporting a TimeoutException for OUR cancel is still cancelled (download() does that)', async () => {
  const env = setup({
    download: (url, path, opts) => new Promise((resolve, reject) => {
      opts.cancellerReceiver(() => reject(new TimeoutException(5000)));
    }),
  });
  const scope = env.scope();
  const p = env.ZFT.net.request(req(), { scope, sink: 'file', path: '/tmp/d/file.tmp' });
  await flush();
  scope.cancel('shutdown');
  assert.equal((await p.catch((e) => e)).kind, 'cancelled');
});

test('abort cause: a genuine Zotero TimeoutException is timeout', async () => {
  const env = setup({ request: async () => { throw new TimeoutException(5000); } });
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' })), 'timeout');
});

test('JS backstop: a request Zotero never answers is cancelled and rejected timeout at timeout_ms + 2000', async () => {
  let cancelled = 0;
  const env = setup({
    request: (m, u, opts) => new Promise((resolve, reject) => {
      opts.cancellerReceiver(() => { cancelled++; /* a wedged socket: Zotero does not even reject */ });
    }),
  });
  let settled = false;
  const p = env.ZFT.net.request(req({ timeout_ms: 5000 }), { scope: env.scope(), sink: 'memory' }).catch((e) => { settled = true; return e; });
  await flush();
  env.timers.advance(6999);
  await flush();
  assert.equal(settled, false);
  env.timers.advance(1);
  const err = await p;
  assert.equal(err.kind, 'timeout');
  assert.equal(cancelled, 1);
  assert.equal(env.timers.pendingCount(), 0);
  assert.equal(env.ZFT.net.TIMEOUT_BACKSTOP_MS, 2000);
});

test('the backstop timer is cleared after a normal result', async () => {
  const env = setup({ request: recordingRequest([], () => fakeXhr()) });
  await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' });
  assert.equal(env.timers.pendingCount(), 0);
});

test('a scope cancelled before the call rejects cancelled without touching Zotero.HTTP', async () => {
  let called = 0;
  const env = setup({ request: async () => { called++; return fakeXhr(); }, download: async () => { called++; } });
  const scope = env.scope();
  scope.cancel('earlier');
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope, sink: 'memory' })), 'cancelled');
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope, sink: 'file', path: '/x/file.tmp' })), 'cancelled');
  assert.equal(called, 0);
  assert.equal(env.timers.pendingCount(), 0);
});

test('request validation: missing scope, non-http URL, bad timeout, file sink preconditions', async () => {
  const env = setup({ request: recordingRequest([], () => fakeXhr()) });
  const run = (r, c) => kindOf(env.ZFT.net.request(r, c));
  assert.equal(await run(req(), {}), 'internal');
  assert.equal(await run(req({ url: 'file:///etc/passwd' }), { scope: env.scope(), sink: 'memory' }), 'unsupported');
  assert.equal(await run(req({ timeout_ms: 0 }), { scope: env.scope(), sink: 'memory' }), 'internal');
  assert.equal(await run(req(), { scope: env.scope(), sink: 'file' }), 'internal');
  assert.equal(await run(req({ follow_redirects: false }), { scope: env.scope(), sink: 'file', path: '/x/file.tmp' }), 'internal');
  assert.equal(await run(req({ method: 'post' }), { scope: env.scope(), sink: 'file', path: '/x/file.tmp' }), 'internal');
});

test('missing Zotero.HTTP functions are unsupported', async () => {
  const env = setup();
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'memory' })), 'unsupported');
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'file', path: '/x/file.tmp' })), 'unsupported');
});

test('classifyError: every error class', () => {
  const env = setup();
  const { classifyError } = env.ZFT.net;
  const scope = env.scope();
  const kind = (e, info) => classifyError(e, info).kind;

  assert.equal(kind(new TimeoutException(100)), 'timeout');
  assert.equal(kind(new CancelledException()), 'cancelled');
  assert.equal(kind(new BrowserOfflineException()), 'offline');
  assert.equal(kind(new SecurityException('bad cert')), 'tls');
  assert.equal(kind(new Error('SSL_ERROR_BAD_CERT_DOMAIN')), 'tls');
  assert.equal(kind(new Error('Peer\'s Certificate has expired.')), 'tls');
  assert.equal(kind(new TypeError('NetworkError when attempting to fetch resource.')), 'network');
  assert.equal(kind(new Error('NS_ERROR_UNKNOWN_HOST')), 'network');
  assert.equal(kind(new Error('something odd')), 'internal');
  assert.equal(kind(new TypeError("Cannot read properties of undefined (reading 'resolve')")), 'internal', 'a code bug is not a network error');
  assert.equal(kind(new Error('connection refused by peer')), 'network');
  assert.equal(kind(undefined), 'internal');

  // OpError passes through unchanged
  const op = new env.ZFT.util.OpError('io', 'disk');
  assert.equal(classifyError(op), op);

  // our own abort wins over what Zotero reports
  assert.equal(kind(new TimeoutException(), { abortReason: 'too_large' }), 'too_large');
  assert.equal(kind(new CancelledException(), { abortReason: 'too_large' }), 'too_large');
  assert.equal(kind(new CancelledException(), { abortReason: 'timeout' }), 'timeout');
  assert.equal(kind(new TimeoutException(), { abortReason: 'cancelled' }), 'cancelled');
  scope.cancel('user');
  assert.equal(kind(new TimeoutException(), { scope }), 'cancelled');
  assert.equal(classifyError(new TimeoutException(), { scope }).message, 'user');

  // fetch AbortError
  const abort = Object.assign(new Error('The operation was aborted.'), { name: 'AbortError' });
  assert.equal(kind(abort, { abortReason: 'timeout' }), 'timeout');
  assert.equal(kind(abort), 'network');

  // UnexpectedStatusException: transport failure vs a real status
  assert.equal(kind(new UnexpectedStatusException({ status: 0 }, 0)), 'network');
  const withStatus = classifyError(new UnexpectedStatusException({ status: 502 }, 502));
  assert.equal(withStatus.kind, 'network');
  assert.equal(withStatus.status, 502);

  // status 0 on the xhr: TLS from securityInfo, else network
  assert.equal(kind(new Error('x'), { xhr: { status: 0, channel: { securityInfo: { errorCode: 5 } } } }), 'tls');
  assert.equal(kind(new Error('x'), { xhr: { status: 0, channel: {} } }), 'network');
  assert.equal(kind(new Error('SEC_ERROR_UNKNOWN_ISSUER'), { xhr: { status: 0, channel: {} } }), 'tls', 'message flavour counts too');
});

// ----------------------------------------------------------------------------------------------- file sink

function fakeResponse({ status = 200, url = 'https://example.org/f.pdf', headers = {}, chunks } = {}) {
  const h = Object.fromEntries(Object.entries(headers).map(([k, v]) => [k.toLowerCase(), v]));
  const state = { cancelled: 0, reads: 0 };
  const response = {
    status, url, headers: { get: (n) => h[String(n).toLowerCase()] ?? null }, state,
    body: chunks ? {
      getReader() {
        let i = 0;
        return {
          async read() { state.reads++; return i < chunks.length ? { done: false, value: chunks[i++] } : { done: true, value: undefined }; },
          async cancel() { state.cancelled++; },
        };
      },
    } : null,
  };
  return response;
}

test('file sink: exact Zotero.HTTP.download options, file kept, size from the filesystem', async () => {
  const calls = [];
  const env = setup();
  env.HTTP.download = async (url, path, opts) => {
    calls.push({ url, path, opts });
    opts.cancellerReceiver(() => {});
    await env.fs.IOUtils.write(path, bytes('%PDF-1.7 hello'));
    opts.onProgress(14, 14);
    return fakeResponse({ url: 'https://cdn.example.org/final.pdf', headers: { 'Content-Type': 'application/pdf', 'Set-Cookie': 'x=1' } });
  };
  const res = await env.ZFT.net.request(req({ ua: 'plain' }), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' });
  assert.equal(calls[0].url, 'https://example.org/a');
  assert.equal(calls[0].path, '/tmp/d/file.tmp');
  assert.deepStrictEqual(Object.keys(calls[0].opts).sort(), [
    'cancellerReceiver', 'errorDelayMax', 'headers', 'noRetryOnThrottle', 'onProgress', 'successCodes', 'timeout',
  ]);
  assert.equal(calls[0].opts.timeout, 5000);
  assert.equal(calls[0].opts.successCodes, false);
  assert.equal(calls[0].opts.errorDelayMax, 0);
  assert.equal(calls[0].opts.noRetryOnThrottle, true);
  assert.deepStrictEqual(plain(calls[0].opts.headers), { 'User-Agent': 'Mozilla/5.0 (Plain) Firefox/140.0' });
  assert.equal(res.status, 200);
  assert.equal(res.final_url, 'https://cdn.example.org/final.pdf');
  assert.equal(res.path, '/tmp/d/file.tmp');
  assert.equal(res.bytes, 14);
  assert.equal(res.body, null);
  assert.equal(res.errorBody, null);
  assert.deepStrictEqual(plain(res.headers), { 'content-type': 'application/pdf' });
  assert.equal(env.fs.files.has('/tmp/d/file.tmp'), true);
  assert.equal(env.timers.pendingCount(), 0);
});

test('file sink: a non-2xx answer keeps no file but returns the (capped) error body for the inspector', async () => {
  const env = setup();
  const MiB = 1024 * 1024;
  let response;
  env.HTTP.download = async (url, path, opts) => {
    opts.cancellerReceiver(() => {});
    response = fakeResponse({
      status: 403, headers: { 'Content-Type': 'text/html', Server: 'cloudflare', 'CF-Mitigated': 'challenge' },
      chunks: [new Uint8Array(MiB).fill(65), new Uint8Array(MiB).fill(66), new Uint8Array(MiB).fill(67)],
    });
    return response;
  };
  const res = await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' });
  assert.equal(res.status, 403);
  assert.equal(res.path, null);
  assert.equal(res.body, null);
  assert.equal(res.errorBody.length, env.ZFT.bodies.INSPECT_TEXT_MAX, 'capped at INSPECT_TEXT_MAX');
  assert.equal(res.errorBody[0], 65);
  assert.equal(res.errorBody[res.errorBody.length - 1], 66);
  assert.equal(res.bytes, res.errorBody.length);
  assert.equal(plain(res.headers)['cf-mitigated'], 'challenge');
  assert.equal(response.state.cancelled, 1, 'the rest of the stream is cancelled');
  assert.equal(env.fs.files.has('/tmp/d/file.tmp'), false);
});

test('file sink: a small non-2xx body is read to the end', async () => {
  const env = setup();
  env.HTTP.download = async () => fakeResponse({ status: 404, chunks: [bytes('not '), bytes('found')] });
  const res = await env.ZFT.net.request(req(), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' });
  assert.equal(new TextDecoder().decode(res.errorBody), 'not found');
});

test('size cap, file sink: onProgress past max_bytes (or an announced length) aborts as too_large and removes the partial file', async () => {
  for (const progress of [[5000, null], [10, 9_000_000]]) {
    const env = setup();
    env.HTTP.download = (url, path, opts) => new Promise((resolve, reject) => {
      // like the real download(): every abort surfaces as a TimeoutException and the partial file is removed
      opts.cancellerReceiver(() => reject(new TimeoutException(5000)));
      env.fs.IOUtils.write(path, bytes('%PDF-partial')).then(() => opts.onProgress(progress[0], progress[1]));
    });
    const err = await env.ZFT.net.request(req({ max_bytes: 1000 }), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' }).catch((e) => e);
    assert.equal(err.kind, 'too_large');
    assert.equal(env.fs.files.has('/tmp/d/file.tmp'), false, 'partial file removed');
    assert.equal(env.timers.pendingCount(), 0);
  }
});

test('size cap, file sink: a completed file above max_bytes is removed and rejected', async () => {
  const env = setup();
  env.HTTP.download = async (url, path) => { await env.fs.IOUtils.write(path, new Uint8Array(4096)); return fakeResponse(); };
  const err = await env.ZFT.net.request(req({ max_bytes: 1000 }), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' }).catch((e) => e);
  assert.equal(err.kind, 'too_large');
  assert.equal(env.fs.files.has('/tmp/d/file.tmp'), false);
});

test('file sink backstop is an inactivity timer: progress re-arms it, silence for timeout_ms + 2000 kills the download', async () => {
  let cancelled = 0;
  let onProgress;
  const env = setup();
  env.HTTP.download = (url, path, opts) => new Promise((resolve, reject) => {
    onProgress = opts.onProgress;
    opts.cancellerReceiver(() => { cancelled++; /* wedged: never rejects */ });
    env.fs.IOUtils.write(path, bytes('%PDF-')).then(() => {});
  });
  let settled = false;
  const p = env.ZFT.net.request(req({ timeout_ms: 5000 }), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' })
    .catch((e) => { settled = true; return e; });
  await flush();
  // a slow but alive download: a chunk every 6 s for 30 s (each gap < 7 s) is never killed
  for (let t = 6000, total = 100; t <= 30000; t += 6000, total += 100) {
    env.timers.advance(6000);
    onProgress(total, 100000);
    await flush();
    assert.equal(settled, false, 'still alive at ' + t);
  }
  env.timers.advance(6999);
  await flush();
  assert.equal(settled, false);
  env.timers.advance(1);
  const err = await p;
  assert.equal(err.kind, 'timeout');
  assert.equal(cancelled, 1);
  assert.equal(env.fs.files.has('/tmp/d/file.tmp'), false, 'partial file removed');
  assert.equal(env.timers.pendingCount(), 0);
});

test('file sink: a transport failure from download() is classified (offline, network)', async () => {
  let env = setup({ download: async () => { throw new BrowserOfflineException(); } });
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' })), 'offline');
  env = setup({ download: async () => { throw new TypeError('NetworkError when attempting to fetch resource.'); } });
  assert.equal(await kindOf(env.ZFT.net.request(req(), { scope: env.scope(), sink: 'file', path: '/tmp/d/file.tmp' })), 'network');
});
