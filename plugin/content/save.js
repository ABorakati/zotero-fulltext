/* ZFT.save: Effect::SavePdf (and the manual "attach a file you downloaded" hand-off).
 *
 * Owner: js-zotero-io. Same storage shape as Zotero's Find Full Text (verified, xpcom/attachments.js):
 *   tmpDir  = the directory of the staged body (ZFT.bodies pdf bodies are created with
 *             Zotero.Attachments.createTemporaryStorageDirectory(), :2853, so the final move is a rename);
 *   rename  = Zotero.File.rename(<tmpDir>/file.tmp, `${base}.pdf`) with
 *             base = Zotero.Attachments.getFileBaseNameFromItem(parentItem, {attachmentTitle: title}) (:2380);
 *   create  = await Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory({directory, libraryID,
 *             parentItemID, filename, title, url, contentType: 'application/pdf'}) (:682) -> Zotero.Item
 *             (it saves the item, moves the directory into storage, queues full-text indexing).
 *   title   = Zotero.getString('attachment.fullText' | 'attachment.acceptedVersion' | 'attachment.submittedVersion')
 *             for version published|unknown / accepted / submitted.
 *   url     = SaveReq.url_to_store verbatim: always the UN-proxied canonical URL; reject (OpError 'internal')
 *             a url containing 'login?qurl=', '/redirector/', 'SAMLRequest' or the EZproxy suffix, as a last guard.
 * Before writing: re-check the parent still exists (else OpError 'parent_gone'), the library is files-editable
 * (else 'not_editable'), and the parent still has NO PDF attachment (else 'already_has_pdf': another process,
 * sync or the user added one meanwhile). If req.trim_prefix > 0 the body has junk before "%PDF-": rewrite the
 * file without those bytes (IOUtils.read -> subarray -> IOUtils.write, <= pdf_max_mb) before the rename.
 * On ANY failure the temp directory is removed (IOUtils.remove recursive, ignoreAbsent) and unregistered from
 * ZFT.state; on success it no longer exists (it was moved). Plain Zotero.File.putContentsAsync cannot take an
 * ArrayBuffer, so bytes are always written with IOUtils.write.
 *
 * Implementation notes:
 *  - Cancellation: the scope is checked up to the last validation. From the first write on the save is not
 *    cancellable (the engine ignores Cancel for a started SavePdf, and a half-moved attachment is worse than a
 *    late one).
 *  - The staged body (ctx.bodies.get(handle), kind 'pdf') is marked consumed on success AND after cleanup:
 *    its `dir` and `path` are set to null and `consumed` to true, because the directory is gone (moved into storage
 *    or deleted). BodyStore.release must therefore tolerate dir === null (no IO for it); the engine still sends
 *    Effect::Release for the handle.
 *  - isStorableUrl is a superset of the spec's EZproxy rule: ANY host equal to, or under, the configured EZproxy
 *    suffix is refused (HttpsHyphens hosts have a '-' in the first label, but the login host and plain
 *    "name.<suffix>" hosts are proxy-internal too and never a canonical article URL).
 *  - Errors: OpError passes through; filesystem failures become 'io'; everything else 'internal'.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** Files larger than this are never read into memory for trim_prefix rewriting. */
  const TRIM_MAX_BYTES = 512 * 1024 * 1024;
  const PDF_MAGIC = [0x25, 0x50, 0x44, 0x46, 0x2d]; // "%PDF-"
  const FALLBACK_TITLES = { fullText: 'Full Text', acceptedVersion: 'Accepted Version', submittedVersion: 'Submitted Version' };
  const IO_ERROR_NAMES = [
    'NotFoundError', 'NotAllowedError', 'NotReadableError', 'NoModificationAllowedError', 'InvalidAccessError',
    'ReadOnlyError', 'OperationError', 'DataError', 'UnknownError',
  ];
  const IO_ERROR_RE = /NS_ERROR_FILE|ENOSPC|ENOENT|EACCES|disk full|not enough space|permission denied|access is denied|sharing violation|IOUtils|OS\.File/i;

  function opError(kind, message) {
    return new ZFT.util.OpError(kind, message);
  }

  /** @returns {ZFT.util.OpError} */
  function toOpError(e) {
    const U = ZFT.util;
    if (e instanceof U.OpError) return e;
    const name = e && e.name ? String(e.name) : '';
    const msg = U.errText(e);
    if (IO_ERROR_NAMES.indexOf(name) !== -1 || IO_ERROR_RE.test(msg)) return opError('io', msg);
    return opError('internal', msg);
  }

  // ------------------------------------------------------------------ url guard and titles

  /** @returns {boolean} true if the url is safe to store (see the url rule above) */
  function isStorableUrl(url) {
    if (typeof url !== 'string' || !url) return false;
    let u;
    try { u = new URL(url); } catch (e) { return false; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
    const lower = url.toLowerCase();
    if (lower.includes('login?qurl=') || lower.includes('/redirector/') || lower.includes('samlrequest')) return false;
    let suffix = '';
    try { suffix = String(ZFT.config.getPref('institution.ezproxy_suffix') || '').trim().toLowerCase().replace(/^\.+/, ''); } catch (e) { suffix = ''; }
    if (suffix) {
      const host = u.hostname.toLowerCase();
      if (host === suffix || host.endsWith('.' + suffix)) return false;
    }
    return true;
  }

  /** @returns {string} localized attachment title for a model.rs Version ('published'|'accepted'|'submitted'|'unknown') */
  function titleForVersion(version) {
    const key = version === 'accepted' ? 'acceptedVersion' : version === 'submitted' ? 'submittedVersion' : 'fullText';
    try {
      const z = global.Zotero;
      if (z && typeof z.getString === 'function') {
        const s = z.getString('attachment.' + key);
        if (s && typeof s === 'string') return s;
      }
    } catch (e) { /* fall back to English */ }
    return FALLBACK_TITLES[key];
  }

  // ------------------------------------------------------------------ filesystem helpers

  async function removeDir(dir) {
    if (!dir) return;
    try {
      await global.IOUtils.remove(dir, { recursive: true, ignoreAbsent: true });
    } catch (e) {
      ZFT.util.log('warn', 'save: could not remove ' + dir + ': ' + ZFT.util.errText(e));
    }
    await forgetDir(dir);
  }

  async function forgetDir(dir) {
    try {
      if (ZFT.state && typeof ZFT.state.unregisterTmpDir === 'function') await ZFT.state.unregisterTmpDir(dir);
    } catch (e) {
      ZFT.util.log('debug', 'save: unregisterTmpDir failed: ' + ZFT.util.errText(e));
    }
  }

  /** The staged directory no longer exists: tell the body store so a later release does no IO. */
  function markConsumed(info) {
    if (!info || typeof info !== 'object') return;
    info.consumed = true;
    info.dir = null;
    info.path = null;
  }

  /** Rewrite `path` without its first `trim` bytes; the new first bytes must be "%PDF-". */
  async function trimPrefix(path, trim, expectedLen) {
    if (expectedLen > TRIM_MAX_BYTES) throw opError('io', 'file too large to rewrite (' + expectedLen + ' bytes)');
    const bytes = await global.IOUtils.read(path);
    if (trim >= bytes.length) throw opError('internal', 'trim_prefix is beyond the end of the file');
    for (let i = 0; i < PDF_MAGIC.length; i++) {
      if (bytes[trim + i] !== PDF_MAGIC[i]) throw opError('internal', 'trim_prefix does not point at %PDF-');
    }
    await global.IOUtils.write(path, bytes.subarray(trim));
  }

  // ------------------------------------------------------------------ SavePdf

  /** Everything the save needs from the parent item, or the OpError that says why not. */
  async function checkParent(itemId) {
    const item = ZFT.items.getItem(itemId);
    if (!item || item.deleted) throw opError('parent_gone', 'the parent item no longer exists');
    const Z = global.Zotero;
    const lib = Z && Z.Libraries && typeof Z.Libraries.get === 'function' ? Z.Libraries.get(item.libraryID) : null;
    if (!lib || lib.editable === false || lib.filesEditable === false) {
      throw opError('not_editable', 'the library does not allow file changes');
    }
    if (typeof item.loadDataType === 'function') await item.loadDataType('childItems');
    if (item.numFileAttachmentsWithContentType('application/pdf') !== 0) {
      throw opError('already_has_pdf', 'the item already has a PDF attachment');
    }
    return item;
  }

  /**
   * @param {object} req proto.rs SaveReq {item, body: BodyRef, trim_prefix, version, url_to_store, route, source}
   * @param {{scope: ZFT.util.CancelScope, bodies: object}} ctx bodies = a ZFT.bodies.BodyStore
   * @returns {Promise<{attachment_key: string, filename: string}>} proto.rs SaveOk (attachment_key = `${libraryID}/${key}`)
   * @throws {ZFT.util.OpError} already_has_pdf | not_editable | parent_gone | io | cancelled | internal
   */
  async function savePdf(req, ctx) {
    const scope = ctx && ctx.scope;
    const bodies = ctx && ctx.bodies;
    let info = null;
    let dir = null;
    try {
      if (!scope || !bodies) throw opError('internal', 'savePdf: ctx.scope and ctx.bodies are required');
      if (!req || typeof req !== 'object' || !req.body || !Number.isInteger(req.body.handle)) {
        throw opError('internal', 'savePdf: malformed SaveReq');
      }
      info = bodies.get(req.body.handle);
      dir = info && typeof info.dir === 'string' && info.dir ? info.dir : null;
      if (!isStorableUrl(req.url_to_store)) throw opError('internal', 'savePdf: refusing to store a non-canonical URL');
      scope.throwIfCancelled();

      const item = await checkParent(req.item);
      if (!info || info.kind !== 'pdf' || typeof info.path !== 'string' || !info.path || !dir) {
        throw opError('internal', 'savePdf: the staged PDF is not a file-backed body');
      }
      scope.throwIfCancelled(); // last cancellation point

      const Z = global.Zotero;
      const path = info.path;
      const trim = Number(req.trim_prefix) || 0;
      if (trim > 0) await trimPrefix(path, trim, info.len);

      const title = titleForVersion(req.version);
      const base = await Z.Attachments.getFileBaseNameFromItem(item, { attachmentTitle: title });
      const filename = await Z.File.rename(path, base + '.pdf');
      if (!filename) throw opError('io', 'could not rename the downloaded file');

      const attachment = await Z.Attachments.createURLAttachmentFromTemporaryStorageDirectory({
        directory: dir,
        libraryID: item.libraryID,
        parentItemID: item.id,
        filename,
        title,
        url: req.url_to_store,
        contentType: 'application/pdf',
      });

      // The directory now lives in storage/<KEY>/.
      markConsumed(info);
      await forgetDir(dir);
      return { attachment_key: item.libraryID + '/' + attachment.key, filename };
    } catch (e) {
      await removeDir(dir);
      markConsumed(info);
      throw toOpError(e);
    }
  }

  // ------------------------------------------------------------------ manual attach

  /** nsIFilePicker for one PDF; resolves the chosen path or null when the user cancelled. */
  async function pickPdf(win) {
    const Z = global.Zotero;
    const iface = global.Ci || (global.Components && global.Components.interfaces);
    const cc = global.Cc || (global.Components && global.Components.classes);
    if (!iface || !cc || !cc['@mozilla.org/filepicker;1']) throw opError('unsupported', 'the file picker is not available');
    const parent = win || (Z && typeof Z.getMainWindow === 'function' ? Z.getMainWindow() : null);
    if (!parent) throw opError('unsupported', 'no window to attach the file picker to');
    const picker = cc['@mozilla.org/filepicker;1'].createInstance(iface.nsIFilePicker);
    // Zotero's own string for this dialog (pane.item.attachments.select), so no new FTL id is needed.
    let title = 'Select a file to attach';
    try { title = Z.getString('pane.item.attachments.select') || title; } catch (e) { /* keep the English title */ }
    picker.init(parent.browsingContext, title, iface.nsIFilePicker.modeOpen);
    picker.appendFilter('PDF', '*.pdf');
    picker.appendFilters(iface.nsIFilePicker.filterAll);
    const rv = await new Promise((resolve) => { picker.open(resolve); });
    if (rv !== iface.nsIFilePicker.returnOK || !picker.file) return null;
    return picker.file.path;
  }

  /**
   * Hand-off action "I downloaded it myself": file picker (Zotero's FilePicker from the item's window) for a
   * PDF, then Zotero.Attachments.importFromFile({file, libraryID, parentItemID}) (:63). User-initiated only.
   * @param {number} itemID @param {object} [win] parent window @returns {Promise<{attachment_key: string}|null>} null if the picker was cancelled
   * @throws {ZFT.util.OpError} parent_gone | not_editable | unsupported | io | internal
   */
  async function attachFile(itemID, win) {
    try {
      const item = ZFT.items.getItem(itemID);
      if (!item || item.deleted) throw opError('parent_gone', 'the parent item no longer exists');
      const Z = global.Zotero;
      const lib = Z && Z.Libraries && typeof Z.Libraries.get === 'function' ? Z.Libraries.get(item.libraryID) : null;
      if (!lib || lib.editable === false || lib.filesEditable === false) {
        throw opError('not_editable', 'the library does not allow file changes');
      }
      const file = await pickPdf(win);
      if (!file) return null;
      const attachment = await Z.Attachments.importFromFile({ file, libraryID: item.libraryID, parentItemID: item.id });
      return { attachment_key: item.libraryID + '/' + attachment.key };
    } catch (e) {
      throw toOpError(e);
    }
  }

  ZFT.save = { savePdf, attachFile, titleForVersion, isStorableUrl };
})(this);
