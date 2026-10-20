/* ZFT.zoteroOa: Effect::ZoteroOa via Zotero's private Unpaywall mirror.
 *
 * Owner: js-zotero-io. Calls Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi, {timeout}) (xpcom/utilities_internal.js:1438):
 * a POST to the Zotero services host with its own 1s/2s error delays (max 5 s). It resolves an array of
 * {url?, pageURL?, version?} ('submittedVersion' | 'acceptedVersion' | 'publishedVersion') and THROWS on an
 * invalid DOI or an HTTP/transport failure. Never hand-roll that POST. The user can switch the source off
 * (oa.zotero_oa), and the endpoint is private, so every failure is non-fatal to the item.
 *
 * Completion payload (`ok`): a JSON ARRAY of proto.rs ZoteroOaHit
 *   [{url, page_url, version}] with `version` passed through verbatim (the engine maps it) and entries
 *   with neither url nor page_url dropped. An empty array means "nothing found" (not an error).
 *   proto.rs declares ZoteroOaHit.url as a required String (page_url / version are optional), so an entry that has
 *   only a pageURL is emitted with url: "" (never null / missing, which would fail the engine's decode).
 *
 * getOpenAccessPDFURLs has no cancel hook: cancellation rejects at once and the late result is ignored. Its own retry
 * budget is timeout + at most 5 s of error delays, so the JS backstop is LOOKUP_BACKSTOP_MS after that.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const LOOKUP_TIMEOUT_MS = 10000;
  /** Zotero's 5 s error-retry budget plus 2 s slack: after this the lookup is abandoned as a timeout. */
  const LOOKUP_BACKSTOP_MS = LOOKUP_TIMEOUT_MS + 7000;

  function internals() {
    const z = global.Zotero;
    return z && z.Utilities && z.Utilities.Internal ? z.Utilities.Internal : null;
  }

  function str(v) {
    return typeof v === 'string' ? v.trim() : '';
  }

  /** Raw service array -> ZoteroOaHit[] (see the header). */
  function mapHits(raw) {
    const out = [];
    if (!Array.isArray(raw)) return out;
    for (const h of raw) {
      if (!h || typeof h !== 'object') continue;
      const url = str(h.url);
      const pageUrl = str(h.pageURL);
      if (!url && !pageUrl) continue;
      out.push({ url, page_url: pageUrl || null, version: str(h.version) || null });
    }
    return out;
  }

  /**
   * @param {{doi: string}} eff the ZoteroOa effect fields
   * @param {{scope: ZFT.util.CancelScope}} ctx
   * @returns {Promise<Array<{url: string, page_url: string|null, version: string|null}>>}
   * @throws {ZFT.util.OpError} unsupported (API missing) | timeout | network | cancelled | internal (bad DOI)
   */
  async function lookup(eff, ctx) {
    const U = ZFT.util;
    const scope = ctx && ctx.scope;
    if (!scope) throw new U.OpError('internal', 'zoteroOa.lookup: ctx.scope is required');
    scope.throwIfCancelled();
    const internal = internals();
    if (!internal || typeof internal.getOpenAccessPDFURLs !== 'function') {
      throw new U.OpError('unsupported', 'Zotero.Utilities.Internal.getOpenAccessPDFURLs is not available');
    }
    const doi = str(eff && eff.doi);
    if (!doi) throw new U.OpError('internal', 'zoteroOa.lookup: empty DOI');

    let timer = null;
    let off = null;
    try {
      const raw = await new Promise((resolve, reject) => {
        timer = setTimeout(() => {
          timer = null;
          reject(new U.OpError('timeout', 'Zotero OA lookup took longer than ' + LOOKUP_BACKSTOP_MS + ' ms'));
        }, LOOKUP_BACKSTOP_MS);
        off = scope.onCancel((reason) => reject(new U.OpError('cancelled', String(reason))));
        new Promise((r) => { r(internal.getOpenAccessPDFURLs(doi, { timeout: LOOKUP_TIMEOUT_MS })); }).then(resolve, reject);
      });
      return mapHits(raw);
    } catch (e) {
      throw ZFT.net.classifyError(e, { scope });
    } finally {
      if (timer !== null) clearTimeout(timer);
      if (off) off();
    }
  }

  ZFT.zoteroOa = { LOOKUP_TIMEOUT_MS, LOOKUP_BACKSTOP_MS, lookup };
})(this);
