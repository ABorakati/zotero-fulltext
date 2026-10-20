/* inspect-worker.js: ChromeWorker that runs the PURE wasm instance off the main thread.
 *
 * Owner: js-host-effects. NOT part of ZFT.LOAD_ORDER (own global; no ZFT, no Zotero). Started by
 * ZFT.wasmHost with new ChromeWorker('chrome://zotero-fulltext/content/inspect-worker.js').
 *
 * Messages (structured clone; byte buffers are TRANSFERRED):
 *   main -> worker  {id, kind: 'init', wasmUrl}                       fetch(wasmUrl).arrayBuffer() inside the worker,
 *                                                                     compile, instantiate (no imports), check fx_abi_version
 *   main -> worker  {id, kind: 'inspect', meta, body: ArrayBuffer}    fx_inspect(meta JSON, body)
 *   main -> worker  {id, kind: 'login_state', url, html}              fx_login_state(url text, html text)
 *   worker -> main  {id, ok: <Envelope ok value>} | {id, err: {code, message}} | {id, trap: <panic text>}
 *   worker -> main  {kind: 'ready', abi}                              once, after init
 *   worker -> main  {id, err: {code: 'init_failed', ...}}             init failed (id = the init message id)
 * A trap inside the worker re-instantiates the module and answers {trap}; the main side maps it to
 * Inspection::Failed and counts it. The main side owns the watchdog (it terminates a worker that is silent
 * for ZFT.wasmHost.INSPECT_WATCHDOG_MS and respawns it).
 *
 * Messages are handled strictly in order (one promise chain), so requests posted right after 'init' simply wait
 * for the module. The ABI marshalling is a small copy of ZFT.wasmHost.WasmInstance because this global has no ZFT;
 * ABI v1 is documented in crates/fulltext-wasm/src/lib.rs. Buffers passed to fx_inspect/fx_login_state are freed by
 * the callee; every returned buffer is copied out and released with fx_free.
 */
/* eslint-env worker */
(function (scope) {
  'use strict';

  const ABI_VERSION = 1;
  const REQUIRED = ['memory', 'fx_abi_version', 'fx_alloc', 'fx_free', 'fx_inspect', 'fx_login_state', 'fx_last_panic'];

  let wasmModule = null;
  let x = null;               // exports of the current instance
  let chain = Promise.resolve();

  function instantiate() {
    const exports = new WebAssembly.Instance(wasmModule, {}).exports;
    for (const name of REQUIRED) {
      if (exports[name] === undefined) throw new Error('wasm module is missing export ' + name);
    }
    const abi = exports.fx_abi_version();
    if (abi !== ABI_VERSION) throw new Error('wasm ABI mismatch: got ' + abi);
    x = exports;
  }

  async function init(wasmUrl) {
    const resp = await fetch(wasmUrl);
    if (!resp || resp.ok === false) throw new Error('cannot fetch ' + wasmUrl);
    wasmModule = await WebAssembly.compile(await resp.arrayBuffer());
    if (WebAssembly.Module.imports(wasmModule).length) throw new Error('wasm module must import nothing');
    instantiate();
  }

  function put(bytes) {
    if (bytes.length === 0) return 0;
    const ptr = x.fx_alloc(bytes.length);
    if (!ptr) throw new Error('fx_alloc failed');
    new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);   // view created after the alloc
    return ptr;
  }

  /** Copy the result out, free it, return the parsed Envelope. */
  function take(packed) {
    const ptr = Number(packed >> 32n);
    const len = Number(packed & 0xffffffffn);
    if (len === 0) return { err: { code: 'empty_result', message: 'export returned an empty buffer' } };
    let text;
    try {
      text = new TextDecoder('utf-8').decode(new Uint8Array(x.memory.buffer, ptr, len));
    } finally {
      x.fx_free(ptr, len);
    }
    try {
      return JSON.parse(text);
    } catch (e) {
      return { err: { code: 'bad_result', message: 'result is not JSON' } };
    }
  }

  function lastPanic() {
    try {
      const packed = x.fx_last_panic();
      const len = Number(packed & 0xffffffffn);
      if (!len) return '';
      const text = new TextDecoder('utf-8').decode(new Uint8Array(x.memory.buffer, Number(packed >> 32n), len));
      try { return String(JSON.parse(text).message || text); } catch (e) { return text; }
    } catch (e) {
      return '';
    }
  }

  function callTwo(name, a, b) {
    const pa = put(a);
    let pb;
    try {
      pb = put(b);
    } catch (e) {
      x.fx_free(pa, a.length);
      throw e;
    }
    return take(x[name](pa, a.length, pb, b.length));
  }

  /** Run fn against the instance; a throw is a trap: re-instantiate and answer {trap}. */
  function guarded(id, fn) {
    if (!x) {
      scope.postMessage({ id, err: { code: 'not_ready', message: 'the wasm instance is not available' } });
      return;
    }
    let env;
    try {
      env = fn();
    } catch (e) {
      const panic = lastPanic();
      try { instantiate(); } catch (e2) { x = null; }
      scope.postMessage({ id, trap: panic || String((e && e.message) || e) });
      return;
    }
    if (env && env.err) scope.postMessage({ id, err: env.err });
    else scope.postMessage({ id, ok: env ? env.ok : null });
  }

  async function handle(msg) {
    if (msg.kind === 'init') {
      try {
        await init(msg.wasmUrl);
        scope.postMessage({ kind: 'ready', abi: ABI_VERSION });
      } catch (e) {
        scope.postMessage({ id: msg.id, err: { code: 'init_failed', message: String((e && e.message) || e) } });
      }
    } else if (msg.kind === 'inspect') {
      const meta = new TextEncoder().encode(JSON.stringify(msg.meta));
      const body = msg.body ? new Uint8Array(msg.body) : new Uint8Array(0);
      guarded(msg.id, () => callTwo('fx_inspect', meta, body));
    } else if (msg.kind === 'login_state') {
      const enc = new TextEncoder();
      const url = enc.encode(String(msg.url || ''));
      const html = enc.encode(String(msg.html || ''));
      guarded(msg.id, () => callTwo('fx_login_state', url, html));
    } else {
      scope.postMessage({ id: msg.id, err: { code: 'unknown_kind', message: 'unknown message kind ' + msg.kind } });
    }
  }

  scope.onmessage = function (event) {
    const msg = (event && event.data) || {};
    chain = chain.then(() => handle(msg)).catch((e) => {
      scope.postMessage({ id: msg.id, err: { code: 'worker_error', message: String((e && e.message) || e) } });
    });
  };
})(typeof self !== 'undefined' ? self : this);
