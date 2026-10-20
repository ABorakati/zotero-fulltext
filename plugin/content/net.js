/* ZFT.net: Zotero.HTTP wrapper for Effect::Http.
 *
 * Owner: js-zotero-io. One call = one network round trip, no retries (the engine decides about retrying),
 * no interpretation of the response (the pure wasm inspector does that). Verified against
 * .ref/omni-app/chrome/content/zotero/xpcom/http.js (Zotero 10.0.6):
 *
 *  memory sink  Zotero.HTTP.request(method, url, {headers, body, responseType: 'arraybuffer', timeout,
 *               successCodes: false, errorDelayMax: 0, noRetryOnThrottle: true, followRedirects,
 *               cancellerReceiver, requestObserver}). Resolves with the XMLHttpRequest; final URL =
 *               xhr.responseURL. successCodes:false means 4xx/5xx resolve instead of rejecting and the
 *               uncapped Retry-After loop (http.js:1595-1607) is never entered.
 *               QUIRK 1: with successCodes:false a TRANSPORT failure (DNS, refused, TLS) ALSO resolves, with
 *               xhr.status === 0 (http.js:455-460 only rejects on `!success`); the security check at :552 is
 *               never reached. We therefore classify status 0 ourselves (tls | network).
 *               QUIRK 2: with followRedirects:false and a 3xx, request() resolves a FAKE object
 *               {status, getResponseHeader()} (no responseURL/response, http.js:487-502): handle it
 *               (status = 3xx, headers.location resolved against the request URL, final_url = request URL).
 *  file sink    Zotero.HTTP.download(url, path, {headers, timeout, successCodes: false, errorDelayMax: 0,
 *               noRetryOnThrottle: true, cancellerReceiver, onProgress}) streams via fetch() to disk and
 *               returns the fetch Response (response.url, .status, .headers). A non-2xx status returns the
 *               Response WITHOUT writing a file and WITHOUT consuming the body: read up to INSPECT_TEXT_MAX of
 *               it from response.body (reader) so the inspector can classify a 403 Cloudflare page.
 *               download() reports EVERY abort (cancel, byte cap, inactivity) as TimeoutException: track the
 *               real cause yourself (the `abortKind` of the per-call state below).
 *               download() only bounds connect time and INACTIVITY (timeout between chunks), never total
 *               time, so the JS backstop of the file sink is an inactivity backstop too: it is re-armed on
 *               every progress callback (a healthy 100 MB download is never killed by it, a stalled socket is).
 *  UA           Ua::Default: do not set User-Agent (VersionHeader adds Firefox/N Zotero/V). Ua::Plain: set
 *               User-Agent = Zotero.VersionHeader.getPlainFirefoxUA() (VersionHeader leaves a UA without
 *               "Zotero/" alone). One UA per host per run is the engine's job; net.js just obeys req.ua.
 *  Headers      request: req.headers verbatim except hop-by-hop/forbidden ones (Host, Content-Length,
 *               Cookie: cookies come from the default jar automatically) are dropped.
 *               response: only RESPONSE_HEADERS, names lower-cased, values truncated to 512 chars.
 *  Limits       req.timeout_ms enforced by Zotero's timeout AND a JS backstop timer (timeout_ms + 2000) that
 *               cancels and rejects OpError('timeout'); req.max_bytes enforced from a progress listener
 *               (memory sink: xhr 'progress' + early Content-Length via requestObserver; file sink: onProgress)
 *               -> OpError('too_large'). A completed body above max_bytes is rejected too (events can be
 *               coalesced for tiny bodies). max_bytes 0 = no cap.
 *  Cancellation Our own gate promise is raced against Zotero's promise, so a cancel/backstop/cap rejects at once
 *               with the TRUE cause even if Zotero is slow to react or reports a TimeoutException.
 *  Errors       Zotero.HTTP.TimeoutException -> 'timeout' (unless the abort was ours: cancel -> 'cancelled',
 *               cap -> 'too_large'); CancelledException -> 'cancelled'; BrowserOfflineException -> 'offline';
 *               xhr.status 0 with a TLS failure on the channel (HTTP.checkSecurity / securityInfo.errorCode) ->
 *               'tls'; other transport errors -> 'network'. Never 'internal' for a transport failure.
 *
 * Assumptions only the real-Zotero harness can confirm: the xhr 'progress' event fires for arraybuffer responses
 * (otherwise the post-completion size check still enforces max_bytes), securityInfo.errorCode on a failed channel,
 * fetch() reader cancel releasing the socket, and that fetch() accepts a User-Agent header for Ua::Plain downloads
 * (the memory sink sets it through XHR.setRequestHeader, which chrome code may do).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** Response headers copied into HttpOk.headers (lower-case). set-cookie is deliberately absent. */
  const RESPONSE_HEADERS = [
    'content-type', 'content-length', 'content-disposition', 'content-encoding', 'location', 'retry-after',
    'server', 'cf-mitigated', 'cf-ray', 'akamai-grn', 'refresh', 'link', 'x-ratelimit-remaining', 'www-authenticate',
    'last-modified', 'etag',
  ];
  /** Request headers net.js never forwards. */
  const DROPPED_REQUEST_HEADERS = ['host', 'content-length', 'cookie', 'connection', 'accept-encoding'];
  /** Backstop added to req.timeout_ms before JS cancels a request Zotero did not time out itself. */
  const TIMEOUT_BACKSTOP_MS = 2000;
  /** Response header values are cut at this many characters. */
  const HEADER_VALUE_MAX = 512;
  /** Used only when ZFT.bodies is not loaded (unit tests of this file alone). */
  const FALLBACK_TEXT_MAX = 2 * 1024 * 1024;

  const TLS_RE = /\b(SSL|TLS)\b|certificate|SEC_ERROR|SSL_ERROR|MOZILLA_PKIX|NS_ERROR_NET_INADEQUATE_SECURITY/i;
  // Deliberately specific: a programming error ("Cannot read properties of undefined (reading 'resolve')") must stay 'internal'.
  const NETWORK_RE = /NetworkError|failed to fetch|NS_ERROR_(NET|CONNECTION|UNKNOWN_HOST|PROXY|OFFLINE)|ECONN\w*|ENOTFOUND|unknown host|connection (refused|reset|closed|timed out)|could not (connect|resolve)|network (is )?unreachable/i;

  /**
   * @typedef {object} RawResponse
   * @property {number} status
   * @property {string} final_url
   * @property {Object<string,string>} headers   whitelisted, lower-case
   * @property {number} bytes                    total body bytes received
   * @property {number} ms                       request duration
   * @property {Uint8Array|null} body            full body when sink === 'memory' (null for 'file')
   * @property {string|null} path                file path when sink === 'file' and a 2xx body was written
   * @property {Uint8Array|null} errorBody       up to INSPECT_TEXT_MAX of a non-2xx body on the file sink
   */

  // ------------------------------------------------------------------ small helpers

  function opError(kind, message, extra) {
    return new ZFT.util.OpError(kind, message, extra);
  }

  function isOpError(e) {
    return !!e && (e instanceof ZFT.util.OpError || (e.name === 'OpError' && typeof e.toOpErr === 'function'));
  }

  /** Zotero.HTTP resolved at call time (tests swap fakes onto the context). */
  function zhttp() {
    const z = global.Zotero;
    return z && z.HTTP ? z.HTTP : null;
  }

  function plainUa() {
    try {
      const vh = global.Zotero && global.Zotero.VersionHeader;
      if (vh && typeof vh.getPlainFirefoxUA === 'function') return String(vh.getPlainFirefoxUA() || '');
    } catch (e) {
      ZFT.util.log('warn', 'getPlainFirefoxUA failed: ' + ZFT.util.errText(e));
    }
    return '';
  }

  function textMax() {
    return (ZFT.bodies && ZFT.bodies.INSPECT_TEXT_MAX) || FALLBACK_TEXT_MAX;
  }

  /** Remove a file, never throwing (the caller is already on a failure path). */
  async function removeQuiet(path) {
    try {
      if (global.IOUtils && path) await global.IOUtils.remove(path, { ignoreAbsent: true });
    } catch (e) {
      ZFT.util.log('warn', 'could not remove ' + path + ': ' + ZFT.util.errText(e));
    }
  }

  // ------------------------------------------------------------------ headers

  /**
   * Build the header object for Zotero.HTTP (applies the UA policy and DROPPED_REQUEST_HEADERS).
   * @param {object} req HttpReq @returns {Object<string,string>}
   */
  function buildHeaders(req) {
    const out = {};
    const plain = !!req && req.ua === 'plain';
    const src = (req && req.headers) || {};
    for (const name of Object.keys(src)) {
      const lower = name.toLowerCase();
      if (DROPPED_REQUEST_HEADERS.indexOf(lower) !== -1) continue;
      if (plain && lower === 'user-agent') continue; // replaced below
      const value = src[name];
      if (value === undefined || value === null) continue;
      out[name] = String(value);
    }
    if (plain) {
      const ua = plainUa();
      if (ua) out['User-Agent'] = ua;
    }
    return out;
  }

  function cut(value) {
    return String(value).slice(0, HEADER_VALUE_MAX);
  }

  /** XHR header block ("Name: value\r\n...") -> whitelisted lower-case map. Repeated names are joined with ", ". */
  function headersFromBlock(block) {
    const out = {};
    for (const line of String(block || '').split(/\r?\n/)) {
      const i = line.indexOf(':');
      if (i <= 0) continue;
      const name = line.slice(0, i).trim().toLowerCase();
      if (RESPONSE_HEADERS.indexOf(name) === -1) continue;
      const value = line.slice(i + 1).trim();
      out[name] = name in out ? out[name] + ', ' + value : value;
    }
    for (const name of Object.keys(out)) out[name] = cut(out[name]);
    return out;
  }

  /** Whitelisted headers through a getter (fake redirect xhr getResponseHeader, fetch Headers.get). */
  function headersFromGetter(get) {
    const out = {};
    for (const name of RESPONSE_HEADERS) {
      let v = null;
      try { v = get(name); } catch (e) { v = null; }
      if (v !== null && v !== undefined && v !== '') out[name] = cut(v);
    }
    return out;
  }

  function resolveLocation(location, base) {
    try { return new URL(location, base).href; } catch (e) { return String(location); }
  }

  // ------------------------------------------------------------------ errors

  /** Which Zotero.HTTP exception (or DOM error) is this? @returns {string|null} */
  function exceptionKind(e) {
    if (e === null || e === undefined || typeof e !== 'object') return null;
    const http = zhttp();
    const is = (n) => { const C = http && http[n]; return typeof C === 'function' && e instanceof C; };
    if (is('TimeoutException')) return 'timeout';
    if (is('CancelledException')) return 'cancelled';
    if (is('BrowserOfflineException')) return 'offline';
    if (is('SecurityException')) return 'security';
    if (is('UnexpectedStatusException')) return 'status';
    const n = e.name || (e.constructor && e.constructor.name) || '';
    if (n === 'TimeoutException') return 'timeout';
    if (n === 'CancelledException') return 'cancelled';
    if (n === 'BrowserOfflineException') return 'offline';
    if (n === 'SecurityException') return 'security';
    if (n === 'UnexpectedStatusException') return 'status';
    if (n === 'AbortError') return 'abort';
    return null;
  }

  /** A failed channel: tls when Zotero's own security check or securityInfo says so, else network. */
  function transportError(xhr, detail) {
    if (detail && TLS_RE.test(detail)) return opError('tls', detail);
    const http = zhttp();
    const channel = xhr && xhr.channel;
    if (channel && http && typeof http.checkSecurity === 'function') {
      try {
        http.checkSecurity(channel);
      } catch (e) {
        if (exceptionKind(e) === 'security') return opError('tls', ZFT.util.errText(e));
      }
    }
    let code = 0;
    try { code = Number(channel && channel.securityInfo && channel.securityInfo.errorCode) || 0; } catch (e) { code = 0; }
    if (code) return opError('tls', 'TLS error 0x' + (code >>> 0).toString(16));
    let status = 0;
    try { status = Number(channel && channel.status) || 0; } catch (e) { status = 0; }
    const hex = status ? ' (channel status 0x' + (status >>> 0).toString(16) + ')' : '';
    return opError('network', (detail || 'transport failure') + hex);
  }

  /**
   * Map any thrown value from Zotero.HTTP to an OpError (see Errors above).
   * @param {any} e @param {{scope?: ZFT.util.CancelScope, abortReason?: string, xhr?: object}} [info]
   *        abortReason: 'cancelled' | 'timeout' | 'too_large' = why WE aborted the request, if we did
   * @returns {ZFT.util.OpError}
   */
  function classifyError(e, info) {
    if (isOpError(e)) return e;
    info = info || {};
    const scope = info.scope;
    const msg = ZFT.util.errText(e);
    const kind = exceptionKind(e);
    const reason = info.abortReason || (scope && scope.cancelled ? 'cancelled' : null);

    // Zotero reports every abort (ours or not) as Timeout/Cancelled: the true cause wins.
    if (kind === 'timeout' || kind === 'cancelled' || kind === 'abort') {
      if (reason === 'too_large') return opError('too_large', 'response exceeded the size cap');
      if (reason === 'cancelled') return opError('cancelled', String((scope && scope.reason) || 'cancelled'));
      if (reason === 'timeout') return opError('timeout', 'request timed out');
      if (kind === 'timeout') return opError('timeout', msg);
      if (kind === 'abort') return opError('network', 'request aborted: ' + msg);
      return opError('cancelled', msg);
    }
    if (kind === 'offline') return opError('offline', msg);
    if (kind === 'security') return opError('tls', msg);
    if (kind === 'status') {
      const xhr = e.xmlhttp || info.xhr;
      if (xhr && Number(xhr.status) === 0) return transportError(xhr, msg);
      return opError('network', msg, e.status ? { status: Number(e.status) } : undefined);
    }
    if (info.xhr && Number(info.xhr.status) === 0) return transportError(info.xhr, msg);
    if (TLS_RE.test(msg)) return opError('tls', msg);
    if (NETWORK_RE.test(msg)) return opError('network', msg);
    return opError('internal', msg);
  }

  // ------------------------------------------------------------------ per-call state

  /**
   * Cancellation/backstop bookkeeping for one request. `gate` is rejected by the FIRST abort (scope cancel, JS
   * backstop, size cap) with the matching OpError; run() races a Zotero promise against it.
   */
  function makeCall(scope, timeoutMs, maxBytes) {
    const U = ZFT.util;
    const budget = timeoutMs + TIMEOUT_BACKSTOP_MS;
    const gate = U.Deferred();
    gate.promise.catch(() => {}); // the gate may reject with nobody racing it any more
    const call = {
      abortKind: null, xhr: null, received: 0, max: maxBytes, cancelFn: null, timer: null, offCancel: null,
      finished: false, gate, budget,
      setCanceller(fn) {
        call.cancelFn = fn;
        if (call.abortKind) call.fireCancel();
      },
      fireCancel() {
        const fn = call.cancelFn;
        if (!fn) return;
        try { fn(); } catch (e) { U.log('warn', 'cancel function threw: ' + U.errText(e)); }
      },
      abort(kind, detail) {
        if (call.abortKind || call.finished) return;
        call.abortKind = kind;
        call.disarm();
        call.fireCancel();
        let err;
        if (kind === 'cancelled') err = opError('cancelled', String(detail || 'cancelled'));
        else if (kind === 'too_large') err = opError('too_large', 'response exceeded ' + maxBytes + ' bytes');
        else err = opError('timeout', 'no response within ' + budget + ' ms');
        gate.reject(err);
      },
      arm() {
        call.disarm();
        call.timer = setTimeout(() => { call.timer = null; call.abort('timeout'); }, budget);
      },
      disarm() {
        if (call.timer !== null) { clearTimeout(call.timer); call.timer = null; }
      },
      run(thunk) {
        let p;
        try { p = Promise.resolve(thunk()); } catch (e) { p = Promise.reject(e); }
        return Promise.race([p, gate.promise]);
      },
      finish() {
        call.finished = true;
        call.disarm();
        if (call.offCancel) { call.offCancel(); call.offCancel = null; }
      },
    };
    call.offCancel = scope.onCancel((reason) => call.abort('cancelled', reason));
    call.arm();
    return call;
  }

  // ------------------------------------------------------------------ memory sink

  /** Watch an in-flight xhr for the byte cap (progress events and the announced Content-Length). */
  function observeXhr(xhr, call) {
    const max = call.max;
    if (!max || !xhr || typeof xhr.addEventListener !== 'function') return;
    xhr.addEventListener('progress', (ev) => {
      call.received = Number(ev && ev.loaded) || call.received;
      if (call.received > max) call.abort('too_large');
    });
    xhr.addEventListener('readystatechange', () => {
      if (xhr.readyState !== 2) return;
      let announced = NaN;
      try { announced = Number(xhr.getResponseHeader('content-length')); } catch (e) { announced = NaN; }
      if (announced > max) call.abort('too_large');
    });
  }

  async function memoryRequest(req, call) {
    const U = ZFT.util;
    const http = zhttp();
    if (!http || typeof http.request !== 'function') throw opError('unsupported', 'Zotero.HTTP.request is missing');
    const method = String(req.method || 'get').toUpperCase();
    const options = {
      headers: buildHeaders(req),
      responseType: 'arraybuffer',
      timeout: req.timeout_ms,
      successCodes: false,
      errorDelayMax: 0,
      noRetryOnThrottle: true,
      followRedirects: req.follow_redirects !== false,
      cancellerReceiver: (fn) => call.setCanceller(fn),
      requestObserver: (xhr) => { call.xhr = xhr; observeXhr(xhr, call); },
    };
    if (typeof req.body === 'string') options.body = req.body;

    const t0 = U.monoNow();
    const xhr = await call.run(() => http.request(method, req.url, options));
    call.xhr = xhr;

    const unfollowed = xhr.responseURL === undefined; // fake {status, getResponseHeader}, http.js:487-502
    const status = Number(xhr.status) || 0;
    if (!unfollowed && status === 0) throw transportError(xhr, 'HTTP ' + method + ' failed with status 0');

    let headers;
    let body;
    let finalUrl;
    if (unfollowed) {
      headers = headersFromGetter((n) => xhr.getResponseHeader(n));
      if (headers.location) headers.location = cut(resolveLocation(headers.location, req.url));
      body = new Uint8Array(0);
      finalUrl = req.url;
    } else {
      headers = typeof xhr.getAllResponseHeaders === 'function'
        ? headersFromBlock(xhr.getAllResponseHeaders())
        : headersFromGetter((n) => xhr.getResponseHeader(n));
      body = xhr.response ? U.toU8(xhr.response) : new Uint8Array(0);
      finalUrl = xhr.responseURL || req.url;
    }
    if (call.max && body.length > call.max) throw opError('too_large', 'response exceeded ' + call.max + ' bytes');
    return { status, final_url: finalUrl, headers, bytes: body.length, ms: U.monoNow() - t0, body, path: null, errorBody: null };
  }

  // ------------------------------------------------------------------ file sink

  /** Read up to INSPECT_TEXT_MAX of a non-2xx body, then cancel the rest of the stream. */
  async function readErrorBody(response, call) {
    const limit = textMax();
    const stream = response && response.body;
    if (!stream || typeof stream.getReader !== 'function') return new Uint8Array(0);
    const reader = stream.getReader();
    const parts = [];
    let n = 0;
    try {
      while (n < limit) {
        const { done, value } = await call.run(() => reader.read());
        if (done) break;
        if (!value || !value.length) continue;
        const chunk = n + value.length > limit ? value.subarray(0, limit - n) : value;
        parts.push(chunk);
        n += chunk.length;
      }
    } catch (e) {
      if (call.abortKind) throw e; // our own abort: report it
      ZFT.util.log('warn', 'error body read stopped early: ' + ZFT.util.errText(e));
    } finally {
      try { Promise.resolve(reader.cancel()).catch(() => {}); } catch (e) { /* reader already closed */ }
    }
    return ZFT.util.concatBytes(parts);
  }

  async function fileSize(path, fallback) {
    try {
      const st = await global.IOUtils.stat(path);
      if (st && typeof st.size === 'number') return st.size;
    } catch (e) { /* fall through */ }
    return fallback;
  }

  async function fileRequest(req, ctx, call) {
    const U = ZFT.util;
    const http = zhttp();
    if (!http || typeof http.download !== 'function') throw opError('unsupported', 'Zotero.HTTP.download is missing');
    const path = ctx.path;
    const options = {
      headers: buildHeaders(req),
      timeout: req.timeout_ms,
      successCodes: false,
      errorDelayMax: 0,
      noRetryOnThrottle: true,
      cancellerReceiver: (fn) => call.setCanceller(fn),
      onProgress: (total, contentLength) => {
        call.received = Number(total) || 0;
        if (call.max && (call.received > call.max || Number(contentLength) > call.max)) call.abort('too_large');
        else call.arm(); // inactivity backstop: re-armed on every chunk
      },
    };

    const t0 = U.monoNow();
    const response = await call.run(() => http.download(req.url, path, options));

    const status = Number(response.status) || 0;
    const finalUrl = response.url || req.url;
    const headers = response.headers && typeof response.headers.get === 'function'
      ? headersFromGetter((n) => response.headers.get(n))
      : {};

    if (status >= 200 && status < 300) {
      const bytes = await fileSize(path, call.received);
      if (call.max && bytes > call.max) throw opError('too_large', 'download exceeded ' + call.max + ' bytes');
      return { status, final_url: finalUrl, headers, bytes, ms: U.monoNow() - t0, body: null, path, errorBody: null };
    }
    // Non-2xx: download() wrote nothing. Keep the body for the inspector (403 Cloudflare page, 404 ...).
    const errorBody = await readErrorBody(response, call);
    return {
      status, final_url: finalUrl, headers, bytes: errorBody.length, ms: U.monoNow() - t0,
      body: null, path: null, errorBody,
    };
  }

  // ------------------------------------------------------------------ entry point

  function checkRequest(req, ctx, sink) {
    if (!req || typeof req !== 'object' || typeof req.url !== 'string' || !req.url) {
      throw opError('internal', 'net.request: malformed HttpReq');
    }
    if (!/^https?:\/\//i.test(req.url)) throw opError('unsupported', 'net.request: only http(s) URLs are supported');
    if (!(Number(req.timeout_ms) > 0)) throw opError('internal', 'net.request: timeout_ms must be positive');
    if (sink === 'file') {
      if (typeof ctx.path !== 'string' || !ctx.path) throw opError('internal', 'net.request: file sink needs ctx.path');
      if (req.follow_redirects === false) throw opError('internal', 'net.request: file sink always follows redirects');
      if (req.method && String(req.method).toLowerCase() !== 'get') throw opError('internal', 'net.request: file sink is GET only');
    }
  }

  /**
   * Perform one HTTP request.
   * @param {object} req proto.rs HttpReq {method, url, headers, body, ua, timeout_ms, follow_redirects, max_bytes}
   * @param {{scope: ZFT.util.CancelScope, sink: 'memory'|'file', path?: string}} ctx scope: cancels the request;
   *        sink 'file' requires path (from ZFT.bodies allocPdfSlot) and follow_redirects true
   * @returns {Promise<RawResponse>}
   * @throws {ZFT.util.OpError} timeout | network | cancelled | too_large | tls | offline | unsupported | io
   */
  async function request(req, ctx) {
    const scope = ctx && ctx.scope;
    if (!scope) throw opError('internal', 'net.request: ctx.scope is required');
    const sink = ctx.sink === 'file' ? 'file' : 'memory';
    let call = null;
    try {
      scope.throwIfCancelled();
      checkRequest(req, ctx, sink);
      call = makeCall(scope, Number(req.timeout_ms), Number(req.max_bytes) > 0 ? Number(req.max_bytes) : 0);
      return sink === 'file' ? await fileRequest(req, ctx, call) : await memoryRequest(req, call);
    } catch (e) {
      const err = classifyError(e, { scope, abortReason: call && call.abortKind, xhr: call && call.xhr });
      if (sink === 'file') await removeQuiet(ctx.path); // a partial file is never kept
      throw err;
    } finally {
      if (call) call.finish();
    }
  }

  ZFT.net = { RESPONSE_HEADERS, DROPPED_REQUEST_HEADERS, TIMEOUT_BACKSTOP_MS, request, buildHeaders, classifyError };
})(this);
