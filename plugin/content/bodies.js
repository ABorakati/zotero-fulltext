/* ZFT.bodies: the BodyRef handle store.
 *
 * Owner: js-host-effects. Large bodies never enter the engine: it only sees BodyRef {handle, len}
 * (proto.rs). This store owns the bytes behind every handle.
 *
 *   kind 'page'  HTML/JSON text kept in memory (Uint8Array), capped (PAGE_MAX_BYTES each,
 *                MEM_CAP_TOTAL overall); used by translators and the engine's retain=page.
 *   kind 'pdf'   ALWAYS file-backed: streamed to <storage>/tmp-XXXXXX/file.tmp by Zotero.HTTP.download
 *                (or written from a captured Blob). A save moves that directory into storage, so it is
 *                created with Zotero.Attachments.createTemporaryStorageDirectory() (same volume).
 *
 * Handles are positive integers, unique per session (never reused). Ref-counted: a stored body starts with
 * refcount 1 (owned by the engine; Effect::Release drops it); consumers that outlive the engine's reference
 * (an in-flight SavePdf, a translator run) call retain()/release(). Freed file bodies delete their
 * directory (IOUtils.remove recursive) and unregister it from ZFT.state. releaseAll(run) frees every
 * body tagged with that run (run finished, cancelled, or engine trap) whatever its refcount.
 *
 * Temp-dir discipline: allocPdfSlot() registers the directory in ZFT.state BEFORE returning it, so a crash at
 * any later point leaves a directory the startup sweep knows about. A slot that never becomes a body (the request
 * failed, the response was not a PDF, retain=none) is removed with discardSlot().
 *
 * Additive API beyond the skeleton: stageFile(path, len) (stage a streamed file that is not in the store yet),
 * discardSlot(slot). Dependencies (deps): fs (default IOUtils), state (default ZFT.state), attachments (default
 * Zotero.Attachments), pathUtils (default PathUtils); all resolved at call time so tests inject fakes.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** The inspector sees at most this much of a text body (RespMeta.truncated=true beyond it). */
  const INSPECT_TEXT_MAX = 2 * 1024 * 1024;
  /** Binary staging for fx_inspect: head ++ tail (RespMeta.head_len = head length actually staged). */
  const PDF_HEAD = 4096;
  const PDF_TAIL = 2048;
  /** In-memory page bodies: per body and total. */
  const PAGE_MAX_BYTES = 8 * 1024 * 1024;
  const MEM_CAP_TOTAL = 64 * 1024 * 1024;
  /** readBytes() refuses a file larger than this. */
  const READ_MAX_BYTES = 256 * 1024 * 1024;
  /** Bytes sniffed for <meta charset> when the Content-Type has none. */
  const CHARSET_SNIFF_BYTES = 1024;
  const SLOT_FILE = 'file.tmp';

  /**
   * @typedef {object} BodyInfo
   * @property {number} handle
   * @property {'page'|'pdf'} kind
   * @property {number} len        full byte length
   * @property {number|null} run   owning run (for releaseAll) or null
   * @property {Uint8Array|null} mem   in-memory bytes (page bodies)
   * @property {string|null} path  file path (pdf bodies)
   * @property {string|null} dir   directory to delete on free (pdf bodies)
   * @property {number} refs
   */

  function charsetFromContentType(contentType) {
    const m = /charset\s*=\s*["']?\s*([^\s;"']+)/i.exec(String(contentType || ''));
    return m ? m[1] : null;
  }
  /** <meta charset=...> / <meta http-equiv content="...; charset=..."> in the first bytes (ASCII view). */
  function charsetFromMeta(bytes) {
    let head = '';
    const n = Math.min(bytes.length, CHARSET_SNIFF_BYTES);
    for (let i = 0; i < n; i++) head += String.fromCharCode(bytes[i]);
    const m = /<meta[^>]+charset\s*=\s*["']?\s*([A-Za-z0-9_.:-]+)/i.exec(head);
    return m ? m[1] : null;
  }
  /** TextDecoder for a label; unknown labels fall back to utf-8. */
  function decoderFor(label) {
    if (label) {
      try { return new TextDecoder(label); } catch (e) { /* unknown label */ }
    }
    return new TextDecoder('utf-8');
  }

  /** Handle store; ZFT.bodies.store is the plugin-wide instance, tests create their own. */
  class BodyStore {
    /** @param {{fs?: object, state?: object, attachments?: object, pathUtils?: object}} [deps] fs: IOUtils-like; state: ZFT.state-like (registerTmpDir/unregisterTmpDir) */
    constructor(deps) {
      this._deps = deps || {};
      this._bodies = new Map();
      this._next = 1;
      this._memBytes = 0;
    }

    _fs() { return this._deps.fs || IOUtils; }
    _state() { return this._deps.state || ZFT.state; }
    _attachments() { return this._deps.attachments || Zotero.Attachments; }
    _join(dir, name) {
      const pu = this._deps.pathUtils || (typeof PathUtils !== 'undefined' ? PathUtils : null);
      if (pu && typeof pu.join === 'function') return pu.join(dir, name);
      const sep = dir.includes('\\') && !dir.includes('/') ? '\\' : '/';
      return dir.replace(/[\\/]+$/, '') + sep + name;
    }
    _add(info) {
      info.handle = this._next++;
      info.refs = 1;
      this._bodies.set(info.handle, info);
      return { handle: info.handle, len: info.len };
    }

    /** Keep bytes in memory. @throws {ZFT.util.OpError} 'too_large' above PAGE_MAX_BYTES or the total cap. @returns {{handle: number, len: number}} BodyRef */
    putMemory(bytes, opts) {
      const o = opts || {};
      const u8 = ZFT.util.toU8(bytes);
      if (u8.length > PAGE_MAX_BYTES) {
        throw new ZFT.util.OpError('too_large', `page body of ${u8.length} bytes exceeds ${PAGE_MAX_BYTES}`);
      }
      if (this._memBytes + u8.length > MEM_CAP_TOTAL) {
        throw new ZFT.util.OpError('too_large', `in-memory bodies would exceed ${MEM_CAP_TOTAL} bytes`);
      }
      this._memBytes += u8.length;
      return this._add({
        kind: o.kind === 'pdf' ? 'pdf' : 'page', len: u8.length, run: o.run == null ? null : o.run,
        mem: o.adopt ? u8 : u8.slice(), path: null, dir: null,
      });
    }
    /** Adopt a file written by the caller. @param {{path: string, dir: string, len: number, run?: number}} f @returns {{handle: number, len: number}} */
    putFile(f) {
      if (!f || !f.path || !f.dir) throw new ZFT.util.OpError('internal', 'putFile needs path and dir');
      return this._add({
        kind: 'pdf', len: Number(f.len) || 0, run: f.run == null ? null : f.run,
        mem: null, path: f.path, dir: f.dir,
      });
    }
    /**
     * Reserve a fresh storage temp dir for a streamed PDF.
     * @param {number} [run] @returns {Promise<{dir: string, path: string}>} path = <dir>/file.tmp (dir registered in ZFT.state)
     */
    async allocPdfSlot(run) {
      const dirObj = await this._attachments().createTemporaryStorageDirectory();
      const dir = dirObj && dirObj.path ? dirObj.path : String(dirObj);
      try {
        await this._state().registerTmpDir(dir);   // BEFORE the directory is used for anything
      } catch (e) {
        try { await this._fs().remove(dir, { recursive: true, ignoreAbsent: true }); } catch (e2) { /* swept at next start */ }
        throw new ZFT.util.OpError('io', 'cannot register temp dir: ' + ZFT.util.errText(e));
      }
      return { dir, path: this._join(dir, SLOT_FILE) };
    }
    /** Remove a slot that never became a body (failed request, non-PDF response, retain=none). Never throws. @returns {Promise<void>} */
    async discardSlot(slot) {
      if (!slot || !slot.dir) return;
      await this._removeDir(slot.dir);
    }
    /** Write a captured Blob (PDF from the MIME handler) into a new slot. @returns {Promise<{handle: number, len: number}>} */
    async putBlobAsPdf(blob, run) {
      const slot = await this.allocPdfSlot(run);
      try {
        const bytes = new Uint8Array(await blob.arrayBuffer());
        await this._fs().write(slot.path, bytes);
        return this.putFile({ path: slot.path, dir: slot.dir, len: bytes.length, run });
      } catch (e) {
        await this.discardSlot(slot);
        if (e instanceof ZFT.util.OpError) throw e;
        throw new ZFT.util.OpError('io', 'cannot store captured PDF: ' + ZFT.util.errText(e));
      }
    }
    /** @returns {BodyInfo|null} */
    get(handle) {
      return this._bodies.get(handle) || null;
    }
    /** Full bytes (reads the file for pdf bodies; refuses > 256 MiB). Memory bodies are returned without a copy: do not mutate. @returns {Promise<Uint8Array>} */
    async readBytes(handle) {
      const info = this._need(handle);
      if (info.mem) return info.mem;
      if (info.len > READ_MAX_BYTES) throw new ZFT.util.OpError('too_large', `body of ${info.len} bytes is too large to read`);
      try {
        return ZFT.util.toU8(await this._fs().read(info.path));
      } catch (e) {
        throw new ZFT.util.OpError('io', 'cannot read body file: ' + ZFT.util.errText(e));
      }
    }
    /** Decoded text; charset from opts.contentType or <meta charset>, default utf-8. @returns {Promise<string>} */
    async readText(handle, opts) {
      const bytes = await this.readBytes(handle);
      const label = charsetFromContentType(opts && opts.contentType) || charsetFromMeta(bytes);
      return decoderFor(label).decode(bytes);   // the decoder strips a BOM
    }
    /**
     * The bytes handed to fx_inspect for this body: a text body truncated to INSPECT_TEXT_MAX
     * ({bytes, head_len: null, truncated}), or for a pdf body head++tail ({bytes, head_len, truncated: false}).
     * @returns {Promise<{bytes: Uint8Array, head_len: number|null, truncated: boolean, body_len: number}>}
     */
    async stageForInspect(handle) {
      const info = this._need(handle);
      if (info.mem) return this.stageBytes(info.mem, info.kind);
      return this.stageFile(info.path, info.len);
    }
    /**
     * Same staging for bytes that are not (yet) in the store (HTTP/browser results).
     * kind 'pdf': the whole body when it is at most PDF_HEAD + PDF_TAIL, else head ++ tail with head_len = PDF_HEAD.
     * Any other kind is text: the first INSPECT_TEXT_MAX bytes, truncated flag set when cut. body_len is always the full length.
     */
    async stageBytes(bytes, kind) {
      const u8 = ZFT.util.toU8(bytes);
      const len = u8.length;
      if (kind === 'pdf') {
        if (len <= PDF_HEAD + PDF_TAIL) return { bytes: u8, head_len: null, truncated: false, body_len: len };
        return {
          bytes: ZFT.util.concatBytes([u8.subarray(0, PDF_HEAD), u8.subarray(len - PDF_TAIL)]),
          head_len: PDF_HEAD, truncated: false, body_len: len,
        };
      }
      const truncated = len > INSPECT_TEXT_MAX;
      return { bytes: truncated ? u8.subarray(0, INSPECT_TEXT_MAX) : u8, head_len: null, truncated, body_len: len };
    }
    /**
     * Binary staging for a file on disk (a streamed PDF): same result as stageBytes(bytes, 'pdf') without reading the
     * whole file. @param {string} path @param {number} [len] file length; stat()ed when missing
     */
    async stageFile(path, len) {
      const fs = this._fs();
      let size = Number(len);
      if (!Number.isFinite(size) || size < 0) {
        try { size = (await fs.stat(path)).size; } catch (e) { throw new ZFT.util.OpError('io', 'cannot stat body file: ' + ZFT.util.errText(e)); }
      }
      try {
        if (size === 0) return { bytes: new Uint8Array(0), head_len: null, truncated: false, body_len: 0 };
        if (size <= PDF_HEAD + PDF_TAIL) {
          const all = await fs.read(path, { maxBytes: size });
          return { bytes: ZFT.util.toU8(all), head_len: null, truncated: false, body_len: size };
        }
        const [head, tail] = await Promise.all([
          fs.read(path, { offset: 0, maxBytes: PDF_HEAD }),
          fs.read(path, { offset: size - PDF_TAIL, maxBytes: PDF_TAIL }),
        ]);
        return {
          bytes: ZFT.util.concatBytes([ZFT.util.toU8(head), ZFT.util.toU8(tail)]),
          head_len: PDF_HEAD, truncated: false, body_len: size,
        };
      } catch (e) {
        if (e instanceof ZFT.util.OpError) throw e;
        throw new ZFT.util.OpError('io', 'cannot stage body file: ' + ZFT.util.errText(e));
      }
    }
    /** @returns {string} absolute path of a pdf body @throws if not file-backed */
    pathOf(handle) {
      const info = this._need(handle);
      if (!info.path) throw new ZFT.util.OpError('internal', `body ${handle} is not file-backed`);
      return info.path;
    }
    /** refcount++ ; returns the same handle. */
    retain(handle) {
      this._need(handle).refs++;
      return handle;
    }
    /** refcount--; at 0 frees memory/file/dir. Unknown handle = no-op (idempotent). @returns {Promise<void>} */
    async release(handle) {
      const info = this._bodies.get(handle);
      if (!info) return;
      info.refs--;
      if (info.refs > 0) return;
      await this._free(info);
    }
    /** Free every body tagged with run (or all bodies when run is undefined), whatever its refcount. @returns {Promise<number>} count freed */
    async releaseAll(run) {
      const victims = [];
      for (const info of this._bodies.values()) {
        if (run === undefined || info.run === run) victims.push(info);
      }
      await Promise.all(victims.map((info) => this._free(info)));
      return victims.length;
    }
    /** @returns {{count: number, memBytes: number, files: number}} */
    stats() {
      let files = 0;
      for (const info of this._bodies.values()) if (info.path) files++;
      return { count: this._bodies.size, memBytes: this._memBytes, files };
    }

    _need(handle) {
      const info = this._bodies.get(handle);
      if (!info) throw new ZFT.util.OpError('internal', `unknown body handle ${handle}`);
      return info;
    }
    /** Forget the body at once (so a second release is a no-op), then delete what it owned. */
    async _free(info) {
      if (this._bodies.get(info.handle) !== info) return;
      this._bodies.delete(info.handle);
      if (info.mem) {
        this._memBytes -= info.len;
        info.mem = null;
      }
      if (info.dir) await this._removeDir(info.dir);
    }
    /** Remove a temp dir and unregister it; a failed removal stays registered for the sweep. Never throws. */
    async _removeDir(dir) {
      try {
        await this._fs().remove(dir, { recursive: true, ignoreAbsent: true });
      } catch (e) {
        ZFT.util.log('warn', `cannot remove temp dir (left for the sweep): ${ZFT.util.errText(e)}`);
        return;
      }
      try {
        await this._state().unregisterTmpDir(dir);
      } catch (e) {
        ZFT.util.log('warn', 'cannot unregister temp dir: ' + ZFT.util.errText(e));
      }
    }
  }

  ZFT.bodies = {
    INSPECT_TEXT_MAX, PDF_HEAD, PDF_TAIL, PAGE_MAX_BYTES, MEM_CAP_TOTAL,
    BodyStore,
    /** Plugin-wide store, created by ZFT.plugin.startup(). */
    store: null,
  };
})(this);
