/* ZFT.translators: run Zotero's own web translators on a page we already fetched.
 *
 * Owner: js-zotero-io. Effect::RunTranslators {page: BodyRef, url} -> TranslatorsOk {found: {url, mime, title}|null, ms}.
 * Verified API (Zotero 10.0.6, xpcom/utilities_internal.js):
 *   Zotero.Utilities.Internal.blobToHTMLDocument(blob, url)   :966  parse + Zotero.HTTP.wrapDocument(doc, url)
 *   Zotero.Utilities.Internal.getFileFromDocument(doc)        :1475 runs ONLY the first matching translator with
 *        libraryID:false (no item is saved) and returns {title, mimeType, url} for the first attachment whose
 *        mimeType is in Zotero.Attachments.FIND_AVAILABLE_FILE_TYPES, or false.
 * Pitfalls handled here:
 *  - Zotero's meta-refresh logic has no <noscript> filter: remove every <noscript> element from the parsed document
 *    BEFORE handing it to a translator (the Rust extractor strips it too; they must agree).
 *  - Translators may do their own network requests and run arbitrary page-specific code: wrap the call in a
 *    TRANSLATE_TIMEOUT_MS race, allow at most MAX_CONCURRENT at once (queue the rest), and treat any exception as
 *    found:null (the Rust extractor is the primary path; this is a bonus candidate source). A translator that
 *    outlives its timeout cannot be stopped; it keeps running detached and its late result/rejection is ignored.
 *  - Decode the page bytes with the charset from the Content-Type header (default utf-8) before parsing; pass the
 *    FINAL url of the page so relative links resolve. The effect carries no headers, so run() relies on the
 *    <meta charset> sniff (first 2 KiB) and parsePage() accepts an explicit contentType for callers that have one.
 *    blobToHTMLDocument decodes with FileReader.readAsText(blob), which honours the charset of the Blob type.
 *  - Feature-detect Zotero.Utilities.Internal.getFileFromDocument and blobToHTMLDocument; if missing,
 *    return {found: null, ms} (never throw 'unsupported' for a bonus source). Without blobToHTMLDocument a
 *    DOMParser + Zotero.HTTP.wrapDocument fallback is used.
 *  - The page body is retained (BodyStore.retain) for the duration of the run, because the engine may Release
 *    it (cancel, item done) while a translator is still working.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const TRANSLATE_TIMEOUT_MS = 15000;
  const MAX_CONCURRENT = 2;
  /** Bytes sniffed for <meta charset> when no Content-Type charset is known. */
  const CHARSET_SNIFF_BYTES = 2048;

  let running = 0;
  const waiters = [];
  const TIMED_OUT = { timedOut: true };

  // ------------------------------------------------------------------ concurrency limit

  function makeRelease() {
    let released = false;
    return () => {
      if (released) return;
      released = true;
      running--;
      pump();
    };
  }

  function pump() {
    while (running < MAX_CONCURRENT && waiters.length) {
      const w = waiters.shift();
      if (w.off) w.off();
      running++;
      w.resolve(makeRelease());
    }
  }

  function acquire(scope) {
    scope.throwIfCancelled();
    if (running < MAX_CONCURRENT && waiters.length === 0) {
      running++;
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

  // ------------------------------------------------------------------ parsing

  function internals() {
    const z = global.Zotero;
    return z && z.Utilities && z.Utilities.Internal ? z.Utilities.Internal : null;
  }

  /** Charset label from a Content-Type value or, failing that, a <meta charset> in the first bytes; '' if unknown. */
  function charsetOf(bytes, contentType) {
    const m = /charset\s*=\s*["']?([A-Za-z0-9_.:-]+)/i.exec(contentType || '');
    if (m) return m[1];
    let head = '';
    const n = Math.min(bytes.length, CHARSET_SNIFF_BYTES);
    for (let i = 0; i < n; i++) head += String.fromCharCode(bytes[i]);
    const meta = /<meta[^>]+charset\s*=\s*["']?([A-Za-z0-9_.:-]+)/i.exec(head);
    return meta ? meta[1] : '';
  }

  /** TextDecoder-valid charset label, else ''. */
  function validCharset(label) {
    if (!label) return '';
    try {
      void new TextDecoder(label);
      return label;
    } catch (e) {
      return '';
    }
  }

  function stripNoscript(doc) {
    const nodes = doc.querySelectorAll('noscript');
    for (const node of Array.from(nodes)) node.remove();
  }

  /**
   * Parse bytes into a Document with <noscript> removed (exported for tests).
   * @param {Uint8Array} bytes @param {string} url @param {string} [contentType] @returns {Promise<Document>}
   */
  async function parsePage(bytes, url, contentType) {
    const U = ZFT.util;
    const data = U.toU8(bytes);
    const charset = validCharset(charsetOf(data, contentType));
    const type = 'text/html' + (charset ? '; charset=' + charset : '');
    const internal = internals();
    let doc;
    if (internal && typeof internal.blobToHTMLDocument === 'function' && typeof global.Blob === 'function') {
      doc = await internal.blobToHTMLDocument(new global.Blob([data], { type }), url);
    } else if (typeof global.DOMParser === 'function' && global.Zotero && global.Zotero.HTTP && typeof global.Zotero.HTTP.wrapDocument === 'function') {
      const text = new TextDecoder(charset || 'utf-8').decode(data);
      doc = global.Zotero.HTTP.wrapDocument(new global.DOMParser().parseFromString(text, 'text/html'), url);
    } else {
      throw new U.OpError('unsupported', 'no HTML parser available');
    }
    stripNoscript(doc);
    return doc;
  }

  // ------------------------------------------------------------------ the run

  /** Resolve with the promise's value, TIMED_OUT after `ms`, or reject OpError('cancelled'); clean up in every case. */
  function raceTimeout(promise, ms, scope) {
    return new Promise((resolve, reject) => {
      let timer = null;
      let off = null;
      const done = () => {
        if (timer !== null) { clearTimeout(timer); timer = null; }
        if (off) { off(); off = null; }
      };
      timer = setTimeout(() => { timer = null; done(); resolve(TIMED_OUT); }, ms);
      off = scope.onCancel((reason) => { done(); reject(new ZFT.util.OpError('cancelled', String(reason))); });
      promise.then(
        (v) => { done(); resolve(v); },
        (e) => { done(); reject(e); }
      );
    });
  }

  /**
   * @param {{page: {handle: number, len: number}, url: string}} eff the RunTranslators effect fields
   * @param {{scope: ZFT.util.CancelScope, bodies: object}} ctx bodies = a ZFT.bodies.BodyStore
   * @returns {Promise<{found: {url: string, mime: string, title: string}|null, ms: number}>} proto.rs TranslatorsOk
   * @throws {ZFT.util.OpError} cancelled only; every other failure is found:null
   */
  async function run(eff, ctx) {
    const U = ZFT.util;
    const scope = ctx && ctx.scope;
    if (!scope) throw new U.OpError('internal', 'translators.run: ctx.scope is required');
    const t0 = U.monoNow();
    const result = (found) => ({ found, ms: Math.max(0, U.monoNow() - t0) });
    scope.throwIfCancelled();

    const bodies = ctx.bodies;
    const internal = internals();
    if (!eff || !eff.page || typeof eff.url !== 'string' || !bodies) return result(null);
    if (!internal || typeof internal.getFileFromDocument !== 'function') return result(null);

    const handle = eff.page.handle;
    const release = await acquire(scope);
    let retained = false;
    try {
      scope.throwIfCancelled();
      if (!bodies.get(handle)) return result(null);
      bodies.retain(handle);
      retained = true;

      const bytes = await bodies.readBytes(handle);
      scope.throwIfCancelled();
      const doc = await parsePage(bytes, eff.url);
      scope.throwIfCancelled();

      const translating = new Promise((resolve) => { resolve(internal.getFileFromDocument(doc)); });
      const hit = await raceTimeout(translating, TRANSLATE_TIMEOUT_MS, scope);
      if (hit === TIMED_OUT) {
        U.log('info', 'translators: timed out on ' + eff.url);
        return result(null);
      }
      if (hit && typeof hit.url === 'string' && hit.url) {
        return result({ url: hit.url, mime: String(hit.mimeType || ''), title: String(hit.title || '') });
      }
      return result(null);
    } catch (e) {
      if (scope.cancelled) throw new U.OpError('cancelled', String(scope.reason));
      if (e instanceof U.OpError && e.kind === 'cancelled') throw e;
      U.log('info', 'translators failed on ' + eff.url + ': ' + U.errText(e));
      return result(null);
    } finally {
      if (retained) {
        try { await bodies.release(handle); } catch (e) { U.log('debug', 'translators: release failed: ' + U.errText(e)); }
      }
      release();
    }
  }

  ZFT.translators = { TRANSLATE_TIMEOUT_MS, MAX_CONCURRENT, run, parsePage };
})(this);
