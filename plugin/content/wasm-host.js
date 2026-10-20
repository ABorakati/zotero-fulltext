/* ZFT.wasmHost: loads fulltext.wasm and wraps the hand-written, zero-import ABI.
 *
 * Owner: js-host-effects. ABI (UTF-8 JSON, u32 pointers/lengths, results packed as u64 =
 * (ptr << 32) | len, which arrives in JS as a BigInt; JS frees every RETURNED buffer with fx_free):
 *   fx_abi_version() -> u32            must be ABI_VERSION (1)
 *   fx_alloc(len) -> ptr               JS copies inputs into memory it allocated with this
 *   fx_free(ptr, len)
 *   fx_inspect(meta_ptr, meta_len, body_ptr, body_len) -> u64   Envelope<Inspection>; CALLEE frees both inputs
 *   fx_step(in_ptr, in_len) -> u64                              Envelope<StepOut>;   CALLEE frees the input
 *   fx_reset()                          drop engine state (engine instance only)
 *   fx_last_panic() -> u64              {"code":"panic","message":...} of the last panic, or len 0 if none; allocation-free,
 *                                       readable after a trap; fx_free ignores this buffer (so JS never frees it)
 *   fx_login_state(url_ptr, url_len, html_ptr, html_len) -> u64     Envelope<LoginState>; url and html are UTF-8 TEXT
 *        buffers (html_len 0 = no document); CALLEE frees both inputs (additive export, owner: core, spec
 *        crates/fulltext-wasm/src/lib.rs)
 * Envelope JSON: {"ok": <value>} or {"err": {"code": "...", "message": "..."}}. Errors are values, never traps.
 *
 * The SAME compiled WebAssembly.Module is instantiated twice: 'pure' (fx_inspect/fx_login_state, stateless)
 * and 'engine' (fx_step, stateful). A WebAssembly.RuntimeError poisons that instance only; the module is
 * cached, so re-instantiation is ~1 ms. An engine trap is NOT replayed: the run controller fails the
 * affected runs (ZFT.runs) and calls resetEngine() + Configure on the next run.
 *
 * Load path: fetch('chrome://zotero-fulltext/content/fulltext.wasm') -> arrayBuffer() -> WebAssembly.compile
 * (instantiateStreaming fails on chrome:// MIME, IOUtils.read fails for packed XPIs; research 03 section 5).
 *
 * Pure-instance placement: ChromeWorker by default (inspect-worker.js, 3 s watchdog, terminate+respawn,
 * requeue once, permanent fall-back to the main thread if the worker cannot start); the main-thread
 * fallback serialises inspections and yields (ZFT.util.idle) between them so a 2 MiB page never blocks
 * the UI for more than one parse at a time. step() is always on the main thread (it is fast: no parsing).
 *
 * Worker policy (all counted in diagnostics()):
 *   - a {trap} reply maps to Inspection::Failed{reason:'pure_trap'}; the 3rd trap switches to the main thread for good;
 *   - the watchdog (INSPECT_WATCHDOG_MS) terminates and respawns the worker; the culprit request is re-sent ONCE,
 *     the second timeout answers Failed{reason:'inspect_timeout'}; other queued requests are re-sent untouched;
 *   - a worker that is recycled twice in a row without ever having sent 'ready' (cannot start, init failed) or that
 *     cannot be created at all switches to the main thread for good; one warning is logged.
 * Test seams (opts): workerFactory() -> {postMessage, terminate, onmessage, onerror}, timers {setTimeout, clearTimeout}.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const ABI_VERSION = 1;
  const WASM_FILE = 'fulltext.wasm';
  /** A worker inspection that takes longer than this is abandoned (worker terminated and respawned). */
  const INSPECT_WATCHDOG_MS = 3000;
  /** Exports the module must have; load() rejects otherwise. */
  const REQUIRED_EXPORTS = [
    'memory', 'fx_abi_version', 'fx_alloc', 'fx_free', 'fx_inspect', 'fx_step', 'fx_reset', 'fx_last_panic', 'fx_login_state',
  ];
  /** Main-thread inspections of a body bigger than this yield to the event loop first. */
  const MAIN_YIELD_BYTES = 64 * 1024;
  const URL_CLASS_CACHE_MAX = 256;
  /** Pure-instance traps seen in the worker before it is abandoned. */
  const WORKER_TRAP_LIMIT = 3;
  /** Consecutive recycles of a worker that never became ready before it is abandoned. */
  const WORKER_UNREADY_LIMIT = 2;
  const WORKER_FILE = 'inspect-worker.js';

  /** The wasm instance trapped (panic/unreachable/OOM). The instance is poisoned until reinstantiated. */
  class EngineTrap extends Error {
    constructor(message, panic) { super(message); this.name = 'EngineTrap'; this.panic = panic || ''; }
  }
  /** The export returned {"err": {code, message}} (bad input, internal invariant): the instance is still usable. */
  class EngineError extends Error {
    constructor(code, message) { super(`${code}: ${message}`); this.name = 'EngineError'; this.code = code; }
  }

  // Platform globals are resolved at call time (tests swap them, the plugin sandbox differs from a window).
  function encodeText(text) { return new TextEncoder().encode(text); }
  function decodeText(bytes) { return new TextDecoder('utf-8').decode(bytes); }
  function noop() { /* intentionally empty */ }

  /** @returns {Uint8Array} UTF-8 JSON of any value. @throws {EngineError} bad_input when it cannot be serialised */
  function encodeJson(value) {
    let text;
    try { text = JSON.stringify(value); } catch (e) { throw new EngineError('bad_input', 'cannot serialise: ' + ZFT.util.errText(e)); }
    if (text === undefined) throw new EngineError('bad_input', 'cannot serialise: undefined');
    return encodeText(text);
  }

  /** One instantiation of the module (no imports). Synchronous calls; owns the linear-memory marshalling. */
  class WasmInstance {
    /** @param {WebAssembly.Module} module @param {'pure'|'engine'} role */
    constructor(module, role) {
      this.module = module;
      this.role = role;
      this.exports = null;
      this.poisoned = false;
      /** Value of fx_abi_version() read at the last init(). */
      this.abi = 0;
      /** Times this wrapper was (re)instantiated, and times it trapped. */
      this.generation = 0;
      this.traps = 0;
    }
    /** Instantiate (no imports), check REQUIRED_EXPORTS and fx_abi_version(). Also used to re-instantiate after a trap. */
    init() {
      const x = new WebAssembly.Instance(this.module, {}).exports;
      for (const name of REQUIRED_EXPORTS) {
        if (x[name] === undefined) throw new Error(`wasm module is missing export ${name}`);
      }
      const abi = x.fx_abi_version();
      if (abi !== ABI_VERSION) throw new Error(`wasm ABI mismatch: got ${abi}, expected ${ABI_VERSION}`);
      this.exports = x;
      this.abi = abi;
      this.poisoned = false;
      this.generation++;
    }
    /** JSON in, JSON out. @param {string} name export taking (ptr,len) @param {object} input @returns {object} the Envelope's ok value @throws {EngineTrap|EngineError} */
    callJson(name, input) {
      const bytes = encodeJson(input);
      return this._run(() => {
        const ptr = this._put(bytes);
        return this._take(this.exports[name](ptr, bytes.length));
      });
    }
    /** Two-buffer call for fx_inspect: meta JSON + raw body bytes (both copied into memory, the callee frees them). Same throw contract as callJson. */
    callMetaBody(name, meta, bodyBytes) {
      const metaBytes = encodeJson(meta);
      const body = bodyBytes ? ZFT.util.toU8(bodyBytes) : new Uint8Array(0);
      return this._run(() => this._callTwo(name, metaBytes, body));
    }
    /** Two-text-buffer call for fx_login_state(url, html). @returns {object} LoginState */
    callTexts(name, textA, textB) {
      const a = encodeText(String(textA == null ? '' : textA));
      const b = encodeText(String(textB == null ? '' : textB));
      return this._run(() => this._callTwo(name, a, b));
    }
    /** fx_reset(): drop the engine state of this instance. @throws {EngineTrap} */
    reset() {
      this._run(() => { this.exports.fx_reset(); });
    }
    /** @returns {string} last panic message, '' if none (safe on a poisoned instance; frees nothing: fx_free ignores that buffer) */
    lastPanic() {
      const x = this.exports;
      if (!x || typeof x.fx_last_panic !== 'function') return '';
      try {
        const packed = x.fx_last_panic();
        if (typeof packed !== 'bigint') return '';
        const len = Number(packed & 0xffffffffn);
        if (!len) return '';
        const ptr = Number(packed >> 32n);
        const text = decodeText(new Uint8Array(x.memory.buffer, ptr, len));
        const parsed = ZFT.util.parseJson(text, null);
        return parsed && typeof parsed.message === 'string' ? parsed.message : text;
      } catch (e) {
        return '';
      }
    }
    /** @returns {boolean} true if the optional export exists */
    has(name) { return !!(this.exports && typeof this.exports[name] === 'function'); }

    /** Run fn; anything an export throws poisons the instance and becomes EngineTrap. Our own EngineError passes through. */
    _run(fn) {
      if (this.poisoned) throw new EngineTrap(`wasm ${this.role} instance is poisoned by an earlier trap`, this.lastPanic());
      try {
        return fn();
      } catch (e) {
        if (e instanceof EngineError || e instanceof EngineTrap) throw e;
        this.poisoned = true;
        this.traps++;
        const panic = this.lastPanic();
        throw new EngineTrap(`wasm ${this.role} trap: ${ZFT.util.errText(e)}${panic ? ' (' + panic + ')' : ''}`, panic);
      }
    }
    /** Copy bytes into freshly allocated memory; empty input = (0, 0), no allocation. The view is created AFTER the alloc (memory may grow). */
    _put(bytes) {
      if (bytes.length === 0) return 0;
      const x = this.exports;
      const ptr = x.fx_alloc(bytes.length);
      if (!ptr) throw new EngineError('alloc_failed', `fx_alloc(${bytes.length}) returned 0`);
      new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);
      return ptr;
    }
    _callTwo(name, a, b) {
      const x = this.exports;
      const pa = this._put(a);
      let pb;
      try {
        pb = this._put(b);
      } catch (e) {
        x.fx_free(pa, a.length);
        throw e;
      }
      return this._take(x[name](pa, a.length, pb, b.length));
    }
    /** Copy a result buffer out, free it, parse the Envelope. @returns {any} ok value @throws {EngineError} */
    _take(packed) {
      const x = this.exports;
      if (typeof packed !== 'bigint') throw new EngineError('bad_abi', 'export did not return a u64');
      const ptr = Number(packed >> 32n);
      const len = Number(packed & 0xffffffffn);
      if (len === 0) throw new EngineError('empty_result', 'export returned an empty buffer');
      let text;
      try {
        text = decodeText(new Uint8Array(x.memory.buffer, ptr, len));
      } finally {
        x.fx_free(ptr, len);
      }
      const env = ZFT.util.parseJson(text, undefined);
      if (!env || typeof env !== 'object') throw new EngineError('bad_result', 'result is not a JSON object');
      if (env.err) throw new EngineError(String(env.err.code || 'error'), String(env.err.message || ''));
      if (!('ok' in env)) throw new EngineError('bad_result', 'envelope has neither ok nor err');
      return env.ok;
    }
  }

  /** The result a never-rejecting call gives when it could not get an answer. */
  function failedResult(kind, reason) {
    return kind === 'login_state' ? { state: 'pending' } : { expect: 'failed', reason };
  }

  /**
   * What ZFT.effects and ZFT.runs use. Obtain with ZFT.wasmHost.get(); tests construct fakes with the same
   * shape (step/inspect/loginState/urlClass/resetEngine/dispose) so they never need wasm.
   */
  class Host {
    /** @param {WebAssembly.Module} module @param {{inspectInWorker?: boolean, workerUrl?: string, url?: string, workerFactory?: Function, timers?: object}} [opts] */
    constructor(module, opts) {
      this.module = module;
      this.opts = opts || {};
      this.engine = null;
      this.pure = null;
      this.engineTraps = 0;
      this.pureTraps = 0;
      this._mode = 'main';
      this._disposed = false;
      this._lastPanic = '';
      this._slot = null;            // current worker: {worker, ready, dead, initId}
      this._pending = new Map();    // worker request id -> entry
      this._nextId = 1;
      this._unready = 0;            // consecutive recycles of never-ready workers
      this._workerTraps = 0;
      this._mainChain = Promise.resolve();
      this._urlClasses = new Map();
      this._warned = false;
    }
    /** Create both instances and the pure runner (worker or main-thread). @returns {Promise<void>} */
    async init() {
      this.engine = new WasmInstance(this.module, 'engine');
      this.engine.init();
      this.pure = new WasmInstance(this.module, 'pure');
      this.pure.init();
      const wantWorker = this.opts.inspectInWorker !== false
        && (typeof this.opts.workerFactory === 'function' || typeof ChromeWorker !== 'undefined');
      if (wantWorker) {
        this._mode = 'worker';
        if (!this._spawnWorker()) this._mode = 'main';
      }
    }
    /**
     * One engine step. @param {object} stepIn proto.rs StepIn {now, wall, commands, completions}
     * @returns {object} proto.rs StepOut @throws {EngineTrap} the engine instance is now poisoned: the caller
     * fails its runs, then calls resetEngine(). @throws {EngineError} the step was rejected, engine state unchanged.
     */
    step(stepIn) {
      if (!this.engine) throw new EngineError('disposed', 'the wasm host was disposed');
      const wasPoisoned = this.engine.poisoned;   // a refused call on a poisoned instance is not a new trap
      try {
        return this.engine.callJson('fx_step', stepIn);
      } catch (e) {
        if (e instanceof EngineTrap && !wasPoisoned) {
          this.engineTraps++;
          if (e.panic) this._lastPanic = e.panic;
        }
        throw e;
      }
    }
    /** fx_reset + clear poison (re-instantiates the engine instance when poisoned). */
    resetEngine() {
      if (!this.engine) throw new EngineError('disposed', 'the wasm host was disposed');
      const eng = this.engine;
      if (!eng.poisoned) {
        try {
          eng.reset();
          return;
        } catch (e) {
          if (!(e instanceof EngineTrap)) throw e;
          this.engineTraps++;
        }
      }
      eng.init();
    }
    /**
     * Classify a response with the pure instance. Never rejects: a pure trap or bad input yields
     * {expect: 'failed', reason: ...} (the instance is re-instantiated).
     * @param {object} meta proto/model InspectMeta {spec, resp}
     * @param {Uint8Array} bytes staged body (ZFT.bodies stageForInspect)
     * @returns {Promise<object>} model.rs Inspection
     */
    async inspect(meta, bytes) {
      try {
        const body = bytes ? ZFT.util.toU8(bytes) : new Uint8Array(0);
        return await this._pureCall({ kind: 'inspect', a: meta, b: body, weight: body.length });
      } catch (e) {
        return failedResult('inspect', 'inspect_error');
      }
    }
    /**
     * Login/challenge window poll = fx_login_state(url, html) on the pure instance (same placement rules as inspect()).
     * Never rejects: a trap or error envelope yields {state: 'pending'}.
     * @param {{url: string, html?: string}} args html = documentHTML text of the window's page (capped at 256 KiB by the caller)
     * @returns {Promise<object>} model.rs LoginState {state: 'pending'|'challenge_shown'|'landed'|'refused', code?}
     */
    async loginState(args) {
      try {
        const url = String((args && args.url) || '');
        const html = args && args.html ? String(args.html) : '';
        const out = await this._pureCall({ kind: 'login_state', a: url, b: html, weight: html.length });
        return out && typeof out.state === 'string' ? out : failedResult('login_state', '');
      } catch (e) {
        return failedResult('login_state', '');
      }
    }
    /**
     * URL-only classification for browser settle detection: inspect() a Probe ({expect:'probe', route:'inst'}) with an
     * empty body and RespMeta {status: 200, final_url: url, body_len: 0}, mapped to model.rs UrlClassKind
     * (IdpLogin -> idp_login, EzproxyNeedLogin -> ezproxy_login, PublisherSsoHop -> publisher_sso_hop, else other).
     * The ONE URL rule JS owns, because the redirector URL has no PageClass: hostname go.openathens.net with a path
     * starting /redirector/ -> openathens_redirector. Results cached per URL (bounded LRU, 256 entries).
     * @returns {Promise<string>}
     */
    async urlClass(url) {
      const key = String(url == null ? '' : url);
      const cache = this._urlClasses;
      if (cache.has(key)) {
        const hit = cache.get(key);
        cache.delete(key);
        cache.set(key, hit);   // re-insert = most recently used
        return hit;
      }
      let kind = redirectorKind(key);
      if (!kind) {
        const insp = await this.inspect(
          { spec: { expect: 'probe', route: 'inst' }, resp: { status: 200, final_url: key, body_len: 0 } },
          new Uint8Array(0),
        );
        if (!insp || insp.expect === 'failed') return 'other';   // do not cache a failure
        kind = probeKind(insp);
      }
      cache.set(key, kind);
      while (cache.size > URL_CLASS_CACHE_MAX) cache.delete(cache.keys().next().value);
      return kind;
    }
    /** @returns {{abi: number, engineTraps: number, pureTraps: number, pure: 'worker'|'main', lastPanic: string}} */
    diagnostics() {
      return {
        abi: this.engine ? this.engine.abi : 0,
        engineTraps: this.engineTraps,
        pureTraps: this.pureTraps,
        pure: this._mode,
        lastPanic: this._lastPanic,
      };
    }
    /** Terminate the worker, drop instances. */
    async dispose() {
      this._disposed = true;
      this._terminateSlot();
      const entries = Array.from(this._pending.values());
      this._pending.clear();
      for (const e of entries) {
        this._clearTimer(e);
        e.resolve(failedResult(e.kind, 'disposed'));
      }
      await this._mainChain;
      this.engine = null;
      this.pure = null;
      this._urlClasses.clear();
    }

    // ------------------------------------------------------------------ pure-instance placement
    _timers() {
      if (this.opts.timers) return this.opts.timers;
      return { setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: (id) => clearTimeout(id) };
    }
    _clearTimer(entry) {
      if (entry.timer != null) {
        this._timers().clearTimeout(entry.timer);
        entry.timer = null;
      }
    }
    /** @param {{kind: 'inspect'|'login_state', a: any, b: any, weight: number}} req @returns {Promise<object>} */
    _pureCall(req) {
      if (this._disposed || !this.pure) return Promise.resolve(failedResult(req.kind, 'disposed'));
      if (this._mode === 'worker') {
        if (this._slot || this._spawnWorker()) return this._viaWorker(req);
        this._mode = 'main';
      }
      return this._viaMain(req);
    }

    // ---- main thread
    _viaMain(req) {
      const run = async () => {
        if (this._disposed || !this.pure) return failedResult(req.kind, 'disposed');
        if (req.weight > MAIN_YIELD_BYTES) await ZFT.util.idle();
        return this._runPure(req);
      };
      const p = this._mainChain.then(run, run);
      this._mainChain = p.then(noop, noop);
      return p;
    }
    _runPure(req) {
      const pure = this.pure;
      if (!pure) return failedResult(req.kind, 'disposed');
      if (pure.poisoned) {
        try { pure.init(); } catch (e) { return failedResult(req.kind, 'pure_trap'); }
      }
      try {
        return req.kind === 'inspect'
          ? pure.callMetaBody('fx_inspect', req.a, req.b)
          : pure.callTexts('fx_login_state', req.a, req.b);
      } catch (e) {
        if (e instanceof EngineTrap) {
          this.pureTraps++;
          if (e.panic) this._lastPanic = e.panic;
          try { pure.init(); } catch (e2) { /* stays poisoned; the next call tries again */ }
          return failedResult(req.kind, 'pure_trap');
        }
        if (e instanceof EngineError) return failedResult(req.kind, 'inspect_error:' + e.code);
        return failedResult(req.kind, 'inspect_error');
      }
    }

    // ---- worker
    _wasmUrl() { return this.opts.url || ZFT.CONTENT_URL + WASM_FILE; }
    /** Start a worker. @returns {boolean} false if it cannot be created (the host then stays on the main thread) */
    _spawnWorker() {
      let worker;
      try {
        worker = typeof this.opts.workerFactory === 'function'
          ? this.opts.workerFactory()
          : new ChromeWorker(this.opts.workerUrl || ZFT.CONTENT_URL + WORKER_FILE);
      } catch (e) {
        this._giveUpWorker('cannot create the inspect worker: ' + ZFT.util.errText(e));
        return false;
      }
      const slot = { worker, ready: false, dead: false, initId: this._nextId++ };
      this._slot = slot;
      worker.onmessage = (event) => this._onWorkerMessage(slot, event && event.data);
      worker.onerror = (event) => {
        if (event && typeof event.preventDefault === 'function') event.preventDefault();
        this._recycleWorker(slot, 'worker error: ' + ((event && event.message) || 'unknown'), 'oldest');
      };
      try {
        worker.postMessage({ id: slot.initId, kind: 'init', wasmUrl: this._wasmUrl() });
      } catch (e) {
        this._giveUpWorker('cannot talk to the inspect worker: ' + ZFT.util.errText(e));
        return false;
      }
      return true;
    }
    _viaWorker(req) {
      return new Promise((resolve) => {
        const entry = { id: this._nextId++, kind: req.kind, a: req.a, b: req.b, weight: req.weight, attempts: 0, timer: null, resolve };
        this._sendEntry(entry);
      });
    }
    _sendEntry(entry) {
      const slot = this._slot;
      if (!slot || slot.dead) {
        this._runEntryOnMain(entry);
        return;
      }
      let msg;
      let transfer;
      if (entry.kind === 'inspect') {
        const copy = entry.b.slice();   // the original stays usable for a requeue
        msg = { id: entry.id, kind: 'inspect', meta: entry.a, body: copy.buffer };
        transfer = [copy.buffer];
      } else {
        msg = { id: entry.id, kind: 'login_state', url: entry.a, html: entry.b };
      }
      this._pending.set(entry.id, entry);
      entry.timer = this._timers().setTimeout(() => this._onWatchdog(slot, entry.id), INSPECT_WATCHDOG_MS);
      try {
        slot.worker.postMessage(msg, transfer);
      } catch (e) {
        this._recycleWorker(slot, 'postMessage failed: ' + ZFT.util.errText(e), entry.id);
      }
    }
    _runEntryOnMain(entry) {
      this._viaMain({ kind: entry.kind, a: entry.a, b: entry.b, weight: entry.weight }).then(entry.resolve, () => entry.resolve(failedResult(entry.kind, 'inspect_error')));
    }
    _onWorkerMessage(slot, msg) {
      if (slot !== this._slot || slot.dead || !msg) return;   // stale worker
      if (msg.kind === 'ready') {
        slot.ready = true;
        this._unready = 0;
        return;
      }
      if (msg.id === slot.initId) {
        if (msg.err) this._recycleWorker(slot, `worker init failed: ${msg.err.code}: ${msg.err.message}`, null);
        return;
      }
      const entry = this._pending.get(msg.id);
      if (!entry) return;
      this._pending.delete(msg.id);
      this._clearTimer(entry);
      if (msg.trap !== undefined) {
        this.pureTraps++;
        this._workerTraps++;
        this._lastPanic = String(msg.trap || '');
        entry.resolve(failedResult(entry.kind, 'pure_trap'));
        if (this._workerTraps >= WORKER_TRAP_LIMIT) this._giveUpWorker(`${this._workerTraps} pure instance traps in the worker`);
      } else if (msg.err) {
        entry.resolve(failedResult(entry.kind, 'inspect_error:' + (msg.err.code || 'unknown')));
      } else {
        entry.resolve(msg.ok);
      }
    }
    _onWatchdog(slot, id) {
      if (slot !== this._slot || slot.dead || !this._pending.has(id)) return;
      this._recycleWorker(slot, `inspect watchdog: no reply within ${INSPECT_WATCHDOG_MS} ms`, id);
    }
    /**
     * Terminate `slot`'s worker and, unless it must be abandoned, start a new one and re-send what was in flight.
     * @param {string|number|null} culprit request id charged one attempt; 'oldest' = the first pending request; null = nobody
     */
    _recycleWorker(slot, why, culprit) {
      if (slot !== this._slot || slot.dead) return;
      slot.dead = true;
      this._slot = null;
      try { slot.worker.terminate(); } catch (e) { /* already gone */ }
      const entries = Array.from(this._pending.values());
      this._pending.clear();
      for (const e of entries) this._clearTimer(e);
      this._unready = slot.ready ? 0 : this._unready + 1;
      ZFT.util.log('warn', 'inspect worker recycled: ' + why);
      let blame = culprit;
      if (blame === 'oldest') blame = entries.length ? entries[0].id : null;
      const alive = [];
      for (const e of entries) {
        // A worker that never became ready proves nothing against the request it was holding.
        if (e.id === blame && slot.ready) e.attempts++;
        if (e.attempts >= 2) e.resolve(failedResult(e.kind, 'inspect_timeout'));
        else alive.push(e);
      }
      if (this._disposed) {
        for (const e of alive) e.resolve(failedResult(e.kind, 'disposed'));
        return;
      }
      if (this._unready >= WORKER_UNREADY_LIMIT) {
        this._giveUpWorker('the inspect worker never became ready', alive);
        return;
      }
      if (!this._spawnWorker()) {
        for (const e of alive) this._runEntryOnMain(e);
        return;
      }
      for (const e of alive) this._sendEntry(e);
    }
    /** Switch to the main-thread pure instance for good (one warning). Pending requests are re-run there. */
    _giveUpWorker(why, extra) {
      const leftovers = extra ? extra.slice() : [];
      if (this._mode !== 'main' || !this._warned) {
        ZFT.util.log('warn', 'inspect worker disabled, using the main thread: ' + why);
        this._warned = true;
      }
      this._mode = 'main';
      const slot = this._slot;
      this._slot = null;
      if (slot) {
        slot.dead = true;
        try { slot.worker.terminate(); } catch (e) { /* already gone */ }
      }
      for (const e of this._pending.values()) {
        this._clearTimer(e);
        leftovers.push(e);
      }
      this._pending.clear();
      for (const e of leftovers) this._runEntryOnMain(e);
    }
    _terminateSlot() {
      const slot = this._slot;
      this._slot = null;
      if (slot) {
        slot.dead = true;
        try { slot.worker.terminate(); } catch (e) { /* already gone */ }
      }
    }
  }

  /** The one URL rule JS owns: the OpenAthens redirector has no PageClass. @returns {string|null} */
  function redirectorKind(url) {
    try {
      const u = new URL(url);
      if (u.hostname.toLowerCase() === 'go.openathens.net' && u.pathname.startsWith('/redirector/')) return 'openathens_redirector';
    } catch (e) { /* not an absolute URL */ }
    return null;
  }
  /** Map a Probe inspection onto model.rs UrlClassKind. */
  function probeKind(insp) {
    const cls = insp && insp.expect === 'probe' && insp.class ? insp.class.kind : '';
    if (cls === 'idp_login') return 'idp_login';
    if (cls === 'ezproxy_need_login') return 'ezproxy_login';
    if (cls === 'publisher_sso_hop') return 'publisher_sso_hop';
    return 'other';
  }

  let hostPromise = null;

  function prefInspectInWorker() {
    try { return ZFT.config.getPref('debug.inspect_in_worker') !== false; } catch (e) { return true; }
  }

  /**
   * Compile and instantiate. @param {{url?: string, bytes?: ArrayBuffer, module?: WebAssembly.Module,
   * inspectInWorker?: boolean, fetch?: Function}} [opts] default url = ZFT.CONTENT_URL + WASM_FILE, inspectInWorker from the pref
   * @returns {Promise<Host>} initialised host
   */
  async function load(opts) {
    const o = opts || {};
    let module = o.module;
    if (!module) {
      let bytes = o.bytes;
      if (!bytes) {
        const url = o.url || ZFT.CONTENT_URL + WASM_FILE;
        const resp = await (o.fetch ? o.fetch(url) : fetch(url));
        if (!resp || resp.ok === false) throw new Error(`cannot fetch ${url}: HTTP ${resp ? resp.status : 'none'}`);
        bytes = await resp.arrayBuffer();
      }
      try {
        module = await WebAssembly.compile(bytes);
      } catch (e) {
        throw new Error('wasm compile failed: ' + ZFT.util.errText(e));
      }
    }
    const imports = WebAssembly.Module.imports(module);
    if (imports.length) {
      throw new Error('wasm module must import nothing, found: ' + imports.map((i) => `${i.module}.${i.name}`).join(', '));
    }
    const present = WebAssembly.Module.exports(module).map((e) => e.name);
    const missing = REQUIRED_EXPORTS.filter((n) => !present.includes(n));
    if (missing.length) throw new Error('wasm module is missing exports: ' + missing.join(', '));
    const inspectInWorker = o.inspectInWorker !== undefined ? o.inspectInWorker : prefInspectInWorker();
    const host = new Host(module, Object.assign({}, o, { inspectInWorker }));
    await host.init();
    return host;
  }
  /** Singleton: first call loads (concurrent calls share one promise); a failed load is not cached. @returns {Promise<Host>} */
  function get() {
    if (!hostPromise) {
      hostPromise = load().catch((e) => { hostPromise = null; throw e; });
    }
    return hostPromise;
  }
  /** Dispose the singleton (plugin shutdown). */
  async function shutdown() {
    const p = hostPromise;
    hostPromise = null;
    if (p) {
      try { await (await p).dispose(); } catch (e) { ZFT.util.log('warn', 'wasm host dispose failed: ' + ZFT.util.errText(e)); }
    }
  }

  ZFT.wasmHost = {
    ABI_VERSION, WASM_FILE, INSPECT_WATCHDOG_MS, REQUIRED_EXPORTS,
    EngineTrap, EngineError, WasmInstance, Host, load, get, shutdown,
  };
})(this);
