/* fulltext-core JS glue for the hand-written wasm ABI (no wasm-bindgen).
 * Plain script: works via Services.scriptloader.loadSubScript (Zotero plugin
 * sandbox: TextEncoder/TextDecoder/WebAssembly present) and via require() in Node.
 * Needs: WebAssembly, BigInt (i64 results), TextEncoder, TextDecoder. No fetch,
 * no import.meta, no FinalizationRegistry, no DOM.
 */
(function (root) {
  "use strict";
  const enc = new TextEncoder();
  const dec = new TextDecoder("utf-8"); // output is always valid UTF-8 from Rust

  class FulltextCoreError extends Error {
    constructor(code, message) { super(`${code}: ${message}`); this.code = code; }
  }

  class FulltextCore {
    /** @param {BufferSource|WebAssembly.Module} src */
    static async load(src) {
      const mod = src instanceof WebAssembly.Module ? src : await WebAssembly.compile(src);
      const instance = await WebAssembly.instantiate(mod, {}); // no imports
      return new FulltextCore(instance, mod);
    }

    constructor(instance, module) {
      this.module = module;          // keep: re-instantiate cheaply after a trap
      this.x = instance.exports;
      this.poisoned = false;
      if (this.x.fx_abi_version() !== 1) throw new Error("fulltext-core ABI mismatch");
      this.x.fx_init?.();            // installs the panic hook
    }

    _lastPanic() {                   // readable even after a trap (static buffer, no alloc)
      if (!this.x.fx_last_panic) return "";
      const p = this.x.fx_last_panic();
      return dec.decode(new Uint8Array(this.x.memory.buffer, Number(p >> 32n), Number(p & 0xffffffffn)));
    }

    _in(bytes) {                     // copy bytes into linear memory
      const ptr = this.x.fx_alloc(bytes.length);
      new Uint8Array(this.x.memory.buffer, ptr, bytes.length).set(bytes); // view AFTER alloc (memory may grow)
      return ptr;
    }

    _guard(fn) {
      if (this.poisoned) throw new Error("fulltext-core instance poisoned by an earlier trap");
      try { return fn(); }
      catch (e) {
        if (e instanceof WebAssembly.RuntimeError) {
          this.poisoned = true;
          const msg = this._lastPanic();
          if (msg) e.message += ` (rust panic: ${msg})`;
        }
        throw e;
      }
    }

    /** JSON in -> JSON out. Returns the `ok` value or throws FulltextCoreError. */
    call(name, input) {
      return this._guard(() => {
        const bytes = enc.encode(JSON.stringify(input));
        const ptr = this._in(bytes);
        const packed = this.x[name](ptr, bytes.length); // BigInt (i64)
        this.x.fx_free(ptr, bytes.length);
        const outPtr = Number(packed >> 32n), outLen = Number(packed & 0xffffffffn);
        const text = dec.decode(new Uint8Array(this.x.memory.buffer, outPtr, outLen));
        this.x.fx_free(outPtr, outLen);
        const res = JSON.parse(text);
        if (res.err) throw new FulltextCoreError(res.err.code, res.err.message);
        return res.ok;
      });
    }

    /** Two-buffer call: HTML is encoded straight into linear memory (no JSON escaping). */
    landingRaw(html, url) {
      return this._guard(() => {
        const u = enc.encode(url), uPtr = this._in(u);
        const cap = html.length * 3;                    // UTF-16 -> UTF-8 worst case
        const hPtr = this.x.fx_alloc(cap);
        const { written } = enc.encodeInto(html, new Uint8Array(this.x.memory.buffer, hPtr, cap));
        const packed = this.x.fx_landing_raw(uPtr, u.length, hPtr, written);
        this.x.fx_free(uPtr, u.length); this.x.fx_free(hPtr, cap);
        const outPtr = Number(packed >> 32n), outLen = Number(packed & 0xffffffffn);
        const res = JSON.parse(dec.decode(new Uint8Array(this.x.memory.buffer, outPtr, outLen)));
        this.x.fx_free(outPtr, outLen);
        if (res.err) throw new FulltextCoreError(res.err.code, res.err.message);
        return res.ok;
      });
    }

    /** Raw bytes in (no JSON): first-1KiB %PDF- check. */
    sniffPdf(u8) {
      return this._guard(() => {
        const head = u8.subarray(0, 1024);
        const ptr = this._in(head);
        const r = this.x.fx_sniff_pdf(ptr, head.length);
        this.x.fx_free(ptr, head.length);
        return r === 1;
      });
    }

    /** Fresh instance from the same compiled module (use after a trap). */
    async reset() {
      const inst = await WebAssembly.instantiate(this.module, {});
      this.x = inst.exports; this.poisoned = false;
      this.x.fx_init?.();
      return this;
    }
  }

  const api = { FulltextCore, FulltextCoreError };
  if (typeof module === "object" && module.exports) module.exports = api;
  else Object.assign(root, api);
})(this);
