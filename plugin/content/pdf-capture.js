/* ZFT.pdfCapture: reference-counted PDF capture through Zotero.MIMETypeHandler.
 *
 * Owner: js-zotero-io. Why a dispatcher: MIMETypeHandler.addHandlers('application/pdf', h, true) replaces
 * the stream listener of EVERY document channel of that type while any handler is registered
 * (xpcom/mimeTypeHandler.js:154-203), and removeHandlers(type, h) deletes ALL handlers for the type when
 * only one is left (:128-141), so two overlapping captures that register/remove their own handlers break
 * each other. We therefore register ONE handler object while at least one capture is active
 * (refcount 0 -> 1 registers, 1 -> 0 removes, remove is idempotent) and route each PDF to the capture whose
 * <browser> is the channel's topFrameElement:
 *   onStartRequest(name, contentType, channel): channel.notificationCallbacks.getInterface(Ci.nsILoadContext)
 *       .topFrameElement, falling back to channel.loadGroup.notificationCallbacks...; remember per channel.
 *   onContent(blob, name, contentType, channel): if the browser belongs to a live capture -> resolve that
 *       capture with the Blob and return true (handled); otherwise return false (Zotero's default handling).
 * This mirrors Zotero.BrowserRequest._makePDFMIMETypeHandler (xpcom/browserRequest.js:525-559).
 *
 * Behaviours worth knowing (all verified in mimeTypeHandler.js):
 *  - While a capture is live EVERY application/pdf document load in the app goes through the handler; a PDF that
 *    belongs to no capture gets `false` and Zotero's own external-helper fallback (:304-322), exactly as it would
 *    for Zotero.BrowserRequest. Captures are short (a browser load), so the window is small.
 *  - A second PDF in the same browser after the first one was captured is swallowed (returns true, ignored): `false`
 *    would pop the external helper dialog for a PDF we deliberately ignore.
 *  - removeHandlers deletes all handlers of the type if ours is the last one, so if some other component removed ITS
 *    handler while ours was registered, ours may already be gone; captures then simply never fire and the caller's
 *    own timeout ends the load. Nothing here can detect that.
 *  - The handler is registered lazily and resolved at call time (Zotero.MIMETypeHandler may be absent in tests).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const PDF_MIME = 'application/pdf';

  /** The one handler object registered with Zotero while at least one capture is live; null otherwise. */
  let handler = null;
  /** <browser> -> live capture record. */
  const live = new Map();
  /** channel -> <browser>, remembered by onStartRequest (a WeakMap, so finished channels do not leak). */
  let channelBrowsers = new WeakMap();
  let warnedNoCi = false;

  /**
   * @typedef {object} Capture
   * @property {Promise<Blob|null>} promise  resolves with the first PDF Blob loaded in the browser; never rejects
   *                                          (resolves null after dispose() when no PDF arrived)
   * @property {() => boolean} found     true once a PDF arrived
   * @property {() => string|null} pdfUrl  channel name (URL) of the captured PDF, null before one arrived
   * @property {() => void} dispose      unregister this capture (idempotent); the last one removes the handler
   */

  function ci() {
    return global.Ci || (global.Components && global.Components.interfaces) || null;
  }

  /** Zotero.MIMETypeHandler resolved at call time. @throws {ZFT.util.OpError} unsupported */
  function mimeHandler() {
    const z = global.Zotero;
    const m = z && z.MIMETypeHandler;
    if (!m || typeof m.addHandlers !== 'function' || typeof m.removeHandlers !== 'function') {
      throw new ZFT.util.OpError('unsupported', 'Zotero.MIMETypeHandler is not available');
    }
    return m;
  }

  /** The <browser> element that loads this channel, or null. Mirrors browserRequest.js:529-547. */
  function topFrameOf(channel) {
    if (!channel) return null;
    const iface = ci();
    const iid = iface && iface.nsILoadContext;
    if (!iid && !warnedNoCi) {
      warnedNoCi = true;
      ZFT.util.log('warn', 'pdfCapture: Ci.nsILoadContext is unavailable, PDFs cannot be matched to their browser');
    }
    try {
      const b = channel.notificationCallbacks.getInterface(iid).topFrameElement;
      if (b) return b;
    } catch (e) { /* no load context on the channel itself */ }
    try {
      const b = channel.loadGroup.notificationCallbacks.getInterface(iid).topFrameElement;
      if (b) return b;
    } catch (e) { /* no load context on the load group either */ }
    return null;
  }

  function onStartRequest(name, contentType, channel) {
    if (!channel || typeof channel !== 'object') return;
    try {
      const b = topFrameOf(channel);
      if (b) channelBrowsers.set(channel, b);
    } catch (e) {
      ZFT.util.log('debug', 'pdfCapture.onStartRequest: ' + ZFT.util.errText(e));
    }
  }

  function onContent(blob, name, contentType, channel) {
    try {
      let browser = channel && typeof channel === 'object' ? channelBrowsers.get(channel) : null;
      if (!browser) browser = topFrameOf(channel);
      const rec = browser ? live.get(browser) : null;
      if (!rec) return false; // not ours: Zotero's default handling
      if (!rec.blob) {
        rec.blob = blob;
        rec.url = name || null;
        rec.deferred.resolve(blob);
      }
      return true; // ours (first PDF kept, any later one swallowed)
    } catch (e) {
      ZFT.util.log('warn', 'pdfCapture.onContent: ' + ZFT.util.errText(e));
      return false;
    }
  }

  /** Register the shared handler on the 0 -> 1 transition. @throws {ZFT.util.OpError} unsupported */
  function ensureHandler() {
    if (handler) return;
    const h = { onStartRequest, onContent };
    mimeHandler().addHandlers(PDF_MIME, h, true);
    handler = h;
  }

  /** Remove the shared handler on the 1 -> 0 transition. Idempotent; never throws. */
  function releaseHandler() {
    if (!handler) return;
    const h = handler;
    handler = null;
    try {
      mimeHandler().removeHandlers(PDF_MIME, h);
    } catch (e) {
      ZFT.util.log('warn', 'pdfCapture: could not remove the MIME handler: ' + ZFT.util.errText(e));
    }
  }

  /**
   * Start capturing PDFs loaded in `browser` (a XUL <browser> or HiddenBrowser._browser).
   * @param {object} browser @returns {Capture}
   * @throws {ZFT.util.OpError} unsupported (no MIMETypeHandler) | internal (bad browser, or one already captured)
   */
  function capture(browser) {
    const U = ZFT.util;
    if (!browser || (typeof browser !== 'object' && typeof browser !== 'function')) {
      throw new U.OpError('internal', 'pdfCapture.capture: a browser element is required');
    }
    if (live.has(browser)) throw new U.OpError('internal', 'pdfCapture.capture: this browser already has a live capture');
    ensureHandler();
    const deferred = U.Deferred();
    const rec = { browser, deferred, blob: null, url: null, disposed: false };
    live.set(browser, rec);

    function dispose() {
      if (rec.disposed) return;
      rec.disposed = true;
      live.delete(browser); // rec.disposed was false, so rec is still the entry for this browser
      deferred.resolve(null); // no-op when a PDF already resolved it
      if (live.size === 0) releaseHandler();
    }

    return { promise: deferred.promise, found: () => rec.blob !== null, pdfUrl: () => rec.url, dispose };
  }

  /** @returns {number} live captures (0 when the global handler is unregistered) */
  function activeCount() {
    return live.size;
  }

  /** Remove the handler and resolve every live capture with null (plugin shutdown). */
  function disposeAll() {
    for (const rec of Array.from(live.values())) {
      rec.disposed = true;
      rec.deferred.resolve(null);
    }
    live.clear();
    channelBrowsers = new WeakMap();
    releaseHandler();
  }

  ZFT.pdfCapture = { capture, activeCount, disposeAll };
})(this);
