/* ZFT.browser: Effect::BrowserLoad on a HiddenBrowser (default cookie jar, so SSO sessions are shared).
 *
 * Owner: js-zotero-io. Verified against .ref/omni-app/chrome/content/zotero/HiddenBrowser.mjs and
 * xpcom/browserRequest.js (Zotero 10.0.6):
 *  - const {HiddenBrowser} = ChromeUtils.importESModule('chrome://zotero/content/HiddenBrowser.mjs');
 *    hb = new HiddenBrowser({customUserAgent}); await hb._createdPromise; hb.webProgress; hb.currentURI.spec;
 *    hb.getPageData(['documentHTML','channelInfo']) (channelInfo.responseStatus); hb.destroy().
 *    customUserAgent is set for req.ua === 'plain' (Zotero.VersionHeader.getPlainFirefoxUA()) only.
 *  - NEVER trust hb.load()'s result: it rejects/returns false after 5 s without a top-level location change
 *    and ALWAYS for PDF responses (the MIME handler aborts the original listener). Call it, ignore the
 *    outcome (catch it), and drive everything from our own nsIWebProgressListener (NOTIFY_LOCATION): each
 *    top-level, non-same-document, non-about:blank location change resets the settle timer and appends to the
 *    url_trail (host + path, query/hash stripped, max 12 entries).
 *  - Settled = at least one navigation happened, the URL has been stable for req.settle_ms AND its class
 *    (ZFT.wasmHost Host.urlClass) is not in req.hold_while (e.g. do not settle on the OpenAthens redirector while
 *    it is still hopping).
 *  - PDF capture (req.capture_pdf): ZFT.pdfCapture.capture(hb._browser); a captured Blob ends the load at the next
 *    poll. A PDF response does not necessarily produce a location change, so the PDF check comes first.
 *  - Timeout (req.timeout_ms): if at least one top-level navigation happened, finish as settled and let the
 *    inspection classify the page (usually an IdP login page: the engine then asks for a login window);
 *    if nothing navigated, throw OpError('timeout').
 *  - Browser error pages (about:neterror / about:certerror / about:blocked) end the load at once: there is no
 *    document worth inspecting, so they become OpError 'network' / 'tls' / 'network'. This is the one URL test in
 *    this file and it is about Gecko's own pages, not about publishers.
 *  - Concurrency: at most MAX_BROWSERS live HiddenBrowsers (FIFO queue), because each is a content process.
 *  - Cancellation (ctx.scope): destroy the browser, dispose the capture, remove the listener, reject 'cancelled'.
 *    destroy() always runs in a finally. A load that is still waiting for a slot leaves the queue at once.
 *  - Challenge pages are never solved here: the page is returned and classified (Challenge{interactive});
 *    the engine may escalate to a LoginWindow of kind 'challenge' (a visible window the USER operates).
 *
 * Assumptions only the real-Zotero harness can confirm: that Gecko reports error pages to onLocationChange as
 * about:neterror?..., that hb.currentURI is the document URL when a PDF is captured, and the getPageData() shape.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const MAX_BROWSERS = 3;
  const POLL_MS = 250;
  const URL_TRAIL_MAX = 12;
  const PAGE_DATA_TIMEOUT_MS = 10000;
  /** Path part of a url_trail entry is cut to this many characters. */
  const TRAIL_PATH_MAX = 80;
  /** nsIWebProgressListener.LOCATION_CHANGE_SAME_DOCUMENT and nsIWebProgress.NOTIFY_LOCATION, used if Ci is absent. */
  const SAME_DOCUMENT_FLAG = 0x1;
  /** nsIWebProgressListener.LOCATION_CHANGE_ERROR_PAGE, used if Ci is absent. */
  const ERROR_PAGE_FLAG = 0x2;
  const NOTIFY_LOCATION_FLAG = 0x100;

  /** Live HiddenBrowser count and the FIFO of loads waiting for a slot. */
  let liveCount = 0;
  const waiters = [];

  /**
   * @typedef {object} BrowserRaw
   * @property {string} final_url   hb.currentURI.spec at the end (the PDF's URL when one was captured)
   * @property {number|null} status channelInfo.responseStatus when a page was read, else null
   * @property {number} ms
   * @property {Blob|null} pdf      captured PDF (caller stores it through ZFT.bodies.putBlobAsPdf)
   * @property {string|null} page_html  documentHTML when no PDF was captured and req.want_page
   * @property {string[]} url_trail
   */

  // ------------------------------------------------------------------ slot queue

  function makeRelease() {
    let released = false;
    return () => {
      if (released) return;
      released = true;
      liveCount--;
      pump();
    };
  }

  function pump() {
    while (liveCount < MAX_BROWSERS && waiters.length) {
      const w = waiters.shift();
      if (w.off) w.off();
      liveCount++;
      w.resolve(makeRelease());
    }
  }

  /** @returns {Promise<() => void>} the release function; rejects OpError('cancelled') if the scope is cancelled first */
  function acquireSlot(scope) {
    scope.throwIfCancelled();
    if (liveCount < MAX_BROWSERS && waiters.length === 0) {
      liveCount++;
      return Promise.resolve(makeRelease());
    }
    return new Promise((resolve, reject) => {
      const w = { resolve, off: null };
      w.off = scope.onCancel((reason) => {
        const i = waiters.indexOf(w);
        if (i !== -1) waiters.splice(i, 1);
        reject(new ZFT.util.OpError('cancelled', String(reason)));
      });
      waiters.push(w);
    });
  }

  // ------------------------------------------------------------------ small helpers

  function ci() {
    return global.Ci || (global.Components && global.Components.interfaces) || null;
  }

  function noop() {}

  /** Race `promise` against the scope's cancellation (rejects OpError('cancelled')); the cancel hook is removed afterwards. */
  function untilCancelled(promise, scope) {
    return new Promise((resolve, reject) => {
      const off = scope.onCancel((reason) => reject(new ZFT.util.OpError('cancelled', String(reason))));
      Promise.resolve(promise).then(
        (v) => { off(); resolve(v); },
        (e) => { off(); reject(e); }
      );
    });
  }

  /** The HiddenBrowser module, resolved at call time. @throws {ZFT.util.OpError} unsupported */
  function importHiddenBrowser() {
    try {
      const cu = global.ChromeUtils;
      const mod = cu.importESModule('chrome://zotero/content/HiddenBrowser.mjs');
      if (mod && typeof mod.HiddenBrowser === 'function') return mod.HiddenBrowser;
    } catch (e) {
      throw new ZFT.util.OpError('unsupported', 'HiddenBrowser is not available: ' + ZFT.util.errText(e));
    }
    throw new ZFT.util.OpError('unsupported', 'HiddenBrowser is not available');
  }

  /**
   * Strip a URL for url_trail: "host/path" without scheme, query or fragment, path truncated to 80 chars.
   * A bare "/" path is dropped ("https://a.b/" -> "a.b"). Non-http URLs keep their scheme ("about:neterror").
   * @param {string} url @returns {string}
   */
  function trailEntry(url) {
    const s = String(url === undefined || url === null ? '' : url);
    try {
      const u = new URL(s);
      if (u.protocol === 'http:' || u.protocol === 'https:') {
        const path = u.pathname === '/' ? '' : u.pathname;
        return u.host + path.slice(0, TRAIL_PATH_MAX);
      }
      return u.protocol + u.pathname.slice(0, TRAIL_PATH_MAX);
    } catch (e) {
      const rest = s.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '').replace(/[?#].*$/, '');
      return rest.slice(0, TRAIL_PATH_MAX + 40);
    }
  }

  /** @returns {{live: number, queued: number}} for diagnostics and tests */
  function stats() {
    return { live: liveCount, queued: waiters.length };
  }

  /** Gecko's own error pages: 'network' | 'tls' | null. */
  function errorPageKind(spec) {
    if (spec.startsWith('about:certerror')) return 'tls';
    if (spec.startsWith('about:neterror') || spec.startsWith('about:blocked')) return 'network';
    return null;
  }

  /** The error page was found by its location-change flag (the URL is the failed address): tell tls from network by documentURI. */
  async function refineErrorKind(hb, st, scope) {
    for (let i = 0; i < 5; i++) {
      let doc = '';
      try { doc = hb._browser && hb._browser.documentURI ? String(hb._browser.documentURI.spec || '') : ''; } catch (e) { doc = ''; }
      const kind = errorPageKind(doc);
      if (kind) {
        st.errorKind = kind;
        return;
      }
      if (!hb._browser || !hb._browser.documentURI) return; // nothing to read (never in a real browser)
      await ZFT.util.sleep(100, scope);
    }
  }

  // ------------------------------------------------------------------ the load

  async function isHeld(host, url, holdWhile) {
    if (!host || typeof host.urlClass !== 'function' || !holdWhile || !holdWhile.length) return false;
    try {
      const cls = await host.urlClass(url);
      return holdWhile.indexOf(cls) !== -1;
    } catch (e) {
      ZFT.util.log('warn', 'urlClass failed: ' + ZFT.util.errText(e));
      return false;
    }
  }

  function currentUrl(hb, st) {
    try {
      const spec = hb.currentURI && hb.currentURI.spec;
      if (spec) return String(spec);
    } catch (e) { /* browser already gone: use the last location we saw */ }
    return st.lastUrl || '';
  }

  /**
   * Run one BrowserLoad.
   * @param {object} req proto.rs BrowserReq {url, ua, capture_pdf, want_page, settle_ms, timeout_ms, hold_while}
   * @param {{scope: ZFT.util.CancelScope, host: object}} ctx host = ZFT.wasmHost Host (for urlClass)
   * @returns {Promise<BrowserRaw>}
   * @throws {ZFT.util.OpError} timeout | cancelled | network | tls (browser error page) | unsupported (HiddenBrowser or
   *         MIME handler missing) | internal
   */
  async function load(req, ctx) {
    const U = ZFT.util;
    const scope = ctx && ctx.scope;
    if (!scope) throw new U.OpError('internal', 'browser.load: ctx.scope is required');
    const host = ctx.host;
    let release = null;
    let hb = null;
    let hbReady = false;
    let cap = null;
    let webProgress = null;
    let listener = null;
    try {
      scope.throwIfCancelled();
      if (!req || typeof req.url !== 'string' || !/^https?:\/\//i.test(req.url)) {
        throw new U.OpError('internal', 'browser.load: an http(s) url is required');
      }
      if (!(Number(req.timeout_ms) > 0)) throw new U.OpError('internal', 'browser.load: timeout_ms must be positive');
      const HiddenBrowser = importHiddenBrowser();
      release = await acquireSlot(scope);

      const t0 = U.monoNow();
      const st = { navs: 0, lastChange: t0, lastUrl: '', trail: [], errorKind: null };

      const hbOptions = {};
      if (req.ua === 'plain') {
        const vh = global.Zotero && global.Zotero.VersionHeader;
        if (vh && typeof vh.getPlainFirefoxUA === 'function') hbOptions.customUserAgent = vh.getPlainFirefoxUA();
      }
      hb = new HiddenBrowser(hbOptions);
      await untilCancelled(hb._createdPromise, scope);
      hbReady = true;
      scope.throwIfCancelled();

      if (req.capture_pdf) cap = ZFT.pdfCapture.capture(hb._browser);

      // Our own location listener. The `listener` variable keeps it alive: nsISupportsWeakReference listeners are
      // held weakly by webProgress (HiddenBrowser keeps its own in a module Set for the same reason).
      const iface = ci();
      const sameDocument = (iface && iface.nsIWebProgressListener && iface.nsIWebProgressListener.LOCATION_CHANGE_SAME_DOCUMENT) || SAME_DOCUMENT_FLAG;
      const errorPage = (iface && iface.nsIWebProgressListener && iface.nsIWebProgressListener.LOCATION_CHANGE_ERROR_PAGE) || ERROR_PAGE_FLAG;
      const notifyLocation = (iface && iface.nsIWebProgress && iface.nsIWebProgress.NOTIFY_LOCATION) || NOTIFY_LOCATION_FLAG;
      listener = {
        QueryInterface: global.ChromeUtils.generateQI(['nsIWebProgressListener', 'nsISupportsWeakReference']),
        onLocationChange(progress, request, location, flags) {
          try {
            if (!progress || !progress.isTopLevel) return; // inner frames
            if (flags & sameDocument) return; // hash / pushState changes
            const spec = location && location.spec ? String(location.spec) : '';
            if (!spec || spec === 'about:blank') return; // the initial blank document
            st.navs++;
            st.lastChange = U.monoNow();
            st.lastUrl = spec;
            const entry = trailEntry(spec);
            if (st.trail.length < URL_TRAIL_MAX && st.trail[st.trail.length - 1] !== entry) st.trail.push(entry);
            if (!st.errorKind) st.errorKind = errorPageKind(spec);
            // Gecko reports an error page under the address that FAILED, with LOCATION_CHANGE_ERROR_PAGE set (verified in Zotero
            // 10.0.6: a DNS failure gave the Gecko error page's HTML back as if it were the site). The kind is refined below
            // from documentURI (about:certerror -> tls).
            if (!st.errorKind && (flags & errorPage)) {
              st.errorKind = 'network';
              st.errorByFlag = true;
            }
          } catch (e) {
            U.log('warn', 'onLocationChange: ' + U.errText(e));
          }
        },
      };
      webProgress = hb.webProgress;
      webProgress.addProgressListener(listener, notifyLocation);

      // Start the load and ignore what it says (see the header): rejections are swallowed here.
      try {
        const p = hb.load(req.url);
        if (p && typeof p.then === 'function') p.then(noop, noop);
      } catch (e) { /* the settle loop still ends with a timeout */ }

      // ---- settle loop: PDF captured | error page | URL stable (and not held) | timeout
      for (;;) {
        scope.throwIfCancelled();
        if (cap && cap.found()) break;
        if (st.errorKind) break;
        if (st.navs > 0 && U.monoNow() - st.lastChange >= req.settle_ms) {
          if (!(await isHeld(host, currentUrl(hb, st), req.hold_while))) break; // settled
          scope.throwIfCancelled(); // the await above may have outlived the scope
          if (cap && cap.found()) break;
        }
        if (U.monoNow() - t0 >= req.timeout_ms) {
          if (st.navs > 0) break; // navigated but never settled: the inspector decides
          throw new U.OpError('timeout', 'the browser never navigated within ' + req.timeout_ms + ' ms');
        }
        await U.sleep(POLL_MS, scope);
      }

      let pdf = null;
      if (cap && cap.found()) pdf = await cap.promise;
      if (!pdf && st.errorKind) {
        if (st.errorByFlag) await refineErrorKind(hb, st, scope);
        throw new U.OpError(st.errorKind, 'the browser showed an error page for ' + trailEntry(st.lastUrl));
      }

      const out = {
        final_url: pdf && cap.pdfUrl() ? cap.pdfUrl() : currentUrl(hb, st),
        status: null,
        ms: U.monoNow() - t0,
        pdf: pdf || null,
        page_html: null,
        url_trail: st.trail.slice(),
      };

      if (!pdf && req.want_page) {
        try {
          const data = await untilCancelled(
            hb.getPageData(['documentHTML', 'channelInfo'], { timeout: PAGE_DATA_TIMEOUT_MS }), scope);
          if (data && typeof data.documentHTML === 'string') out.page_html = data.documentHTML;
          const code = data && data.channelInfo && Number(data.channelInfo.responseStatus);
          if (code > 0) out.status = code;
        } catch (e) {
          if (scope.cancelled) throw e;
          U.log('info', 'getPageData failed (page_html stays null): ' + U.errText(e));
        }
        out.ms = U.monoNow() - t0;
      }
      return out;
    } catch (e) {
      if (e instanceof U.OpError) throw e;
      if (scope.cancelled) throw new U.OpError('cancelled', String(scope.reason));
      throw new U.OpError('internal', U.errText(e));
    } finally {
      // Every step is independent: one failing teardown must not skip the rest.
      if (webProgress && listener) {
        try { webProgress.removeProgressListener(listener); } catch (e) { U.log('debug', 'removeProgressListener: ' + U.errText(e)); }
      }
      if (cap) {
        try { cap.dispose(); } catch (e) { U.log('debug', 'capture dispose: ' + U.errText(e)); }
      }
      if (hb && hbReady) {
        try { hb.destroy(); } catch (e) { U.log('debug', 'HiddenBrowser destroy: ' + U.errText(e)); }
      } else if (hb) {
        // Cancelled while the browser was still being created: destroy it as soon as creation finishes
        // (if creation failed there is nothing to destroy, and destroy() would only log noise).
        const doomed = hb;
        Promise.resolve(doomed._createdPromise).then(() => {
          try { doomed.destroy(); } catch (e) { U.log('debug', 'HiddenBrowser destroy: ' + U.errText(e)); }
        }, noop);
      }
      if (release) release();
    }
  }

  ZFT.browser = { MAX_BROWSERS, POLL_MS, URL_TRAIL_MAX, PAGE_DATA_TIMEOUT_MS, load, trailEntry, stats };
})(this);
