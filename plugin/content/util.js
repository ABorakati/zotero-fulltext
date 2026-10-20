/* ZFT.util: dependency-free helpers shared by every module.
 *
 * Implemented in the skeleton (not a placeholder) because every package builds on it:
 *   OpError      typed failure that maps 1:1 onto proto.rs OpErr (kind, message, after_ms, status)
 *   CancelScope  AbortController replacement: a tree of cancellation scopes
 *   TaskSet      fire-and-forget tasks with a MANDATORY catch (no unhandled rejections)
 *   Deferred, sleep, idle, monoNow, wallNow, log, errText, byte helpers
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** proto.rs OpErrKind values. */
  const OP_ERR_KINDS = [
    'timeout', 'network', 'cancelled', 'too_large', 'tls', 'unsupported', 'offline',
    'already_has_pdf', 'not_editable', 'parent_gone', 'io', 'internal',
  ];

  /** A failure that becomes a CompletionMsg.err. Never thrown across the wasm boundary. */
  class OpError extends Error {
    /** @param {string} kind one of OP_ERR_KINDS  @param {string} [message]  @param {{afterMs?: number, status?: number}} [extra] */
    constructor(kind, message, extra) {
      super(message || kind);
      this.name = 'OpError';
      this.kind = OP_ERR_KINDS.includes(kind) ? kind : 'internal';
      this.afterMs = extra && extra.afterMs != null ? extra.afterMs : null;
      this.status = extra && extra.status != null ? extra.status : null;
    }
    /** @returns {{kind: string, message: string, after_ms?: number, status?: number}} proto.rs OpErr */
    toOpErr() {
      const e = { kind: this.kind, message: String(this.message || '').slice(0, 300) };
      if (this.afterMs != null) e.after_ms = this.afterMs;
      if (this.status != null) e.status = this.status;
      return e;
    }
  }

  /**
   * Cancellation tree. The plugin sandbox has no AbortController, so every async operation takes a
   * scope, registers teardown with onCancel(), and checks throwIfCancelled() between awaits.
   * Cancelling a parent cancels all children; callbacks run once, synchronously, in registration order.
   */
  class CancelScope {
    /** @param {CancelScope|null} [parent] */
    constructor(parent) {
      this._cancelled = false;
      this._reason = null;
      this._callbacks = new Set();
      this._offParent = null;
      if (parent) {
        if (parent.cancelled) {
          this._cancelled = true;
          this._reason = parent.reason;
        } else {
          this._offParent = parent.onCancel((reason) => this.cancel(reason));
        }
      }
    }
    get cancelled() { return this._cancelled; }
    get reason() { return this._reason; }
    /** @param {string} [reason] e.g. 'user', 'item_deleted', 'shutdown', 'timeout' */
    cancel(reason) {
      if (this._cancelled) return;
      this._cancelled = true;
      this._reason = reason || 'cancelled';
      const callbacks = Array.from(this._callbacks);
      this._callbacks.clear();
      for (const fn of callbacks) {
        try { fn(this._reason); } catch (e) { util.log('error', 'cancel callback failed: ' + util.errText(e)); }
      }
    }
    /** Register teardown. Runs immediately if already cancelled. @returns {() => void} unregister */
    onCancel(fn) {
      if (this._cancelled) {
        try { fn(this._reason); } catch (e) { util.log('error', 'cancel callback failed: ' + util.errText(e)); }
        return () => {};
      }
      this._callbacks.add(fn);
      return () => { this._callbacks.delete(fn); };
    }
    /** @returns {CancelScope} a child that is cancelled with this scope */
    child() { return new CancelScope(this); }
    /** Detach from the parent (call when the scope's work is finished). */
    dispose() {
      if (this._offParent) { this._offParent(); this._offParent = null; }
      this._callbacks.clear();
    }
    /** @throws {OpError} kind 'cancelled' if cancelled */
    throwIfCancelled() {
      if (this._cancelled) throw new OpError('cancelled', String(this._reason));
    }
  }

  /** A promise with its resolvers exposed. */
  function Deferred() {
    const d = {};
    d.promise = new Promise((resolve, reject) => { d.resolve = resolve; d.reject = reject; });
    return d;
  }

  /**
   * Fire-and-forget tasks. spawn() wraps fn in a catch that logs, so a forgotten task can never
   * produce an unhandled rejection. whenIdle() resolves when no task is running.
   */
  class TaskSet {
    constructor(name) {
      this.name = name || 'tasks';
      this._running = new Set();
      this._idle = [];
    }
    get size() { return this._running.size; }
    /** @param {string} label  @param {() => Promise<any>|any} fn  @returns {Promise<void>} never rejects */
    spawn(label, fn) {
      const task = Promise.resolve()
        .then(fn)
        .catch((e) => { util.log('error', `${this.name}/${label} failed: ${util.errText(e)}`); })
        .then(() => {
          this._running.delete(task);
          if (!this._running.size) {
            const waiters = this._idle;
            this._idle = [];
            for (const w of waiters) w();
          }
        });
      this._running.add(task);
      return task;
    }
    /** @returns {Promise<void>} */
    whenIdle() {
      if (!this._running.size) return Promise.resolve();
      return new Promise((resolve) => { this._idle.push(resolve); });
    }
  }

  /** setTimeout as a promise; rejects OpError('cancelled') if scope is cancelled first. */
  function sleep(ms, scope) {
    return new Promise((resolve, reject) => {
      if (scope && scope.cancelled) { reject(new OpError('cancelled', String(scope.reason))); return; }
      let off = null;
      const id = setTimeout(() => { if (off) off(); resolve(); }, Math.max(0, ms | 0));
      if (scope) {
        off = scope.onCancel((reason) => { clearTimeout(id); reject(new OpError('cancelled', String(reason))); });
      }
    });
  }

  /** Yield to the event loop (one macrotask) so UI events run between heavy steps. */
  function idle() { return new Promise((resolve) => setTimeout(resolve, 0)); }

  let lastMono = 0;
  let monoBase = null;
  /** Monotonic ms (integer, never decreases): StepIn.now. */
  function monoNow() {
    let t;
    if (typeof performance !== 'undefined' && performance && typeof performance.now === 'function') {
      t = performance.now();
    } else {
      if (monoBase === null) monoBase = Date.now();
      t = Date.now() - monoBase;
    }
    t = Math.floor(t);
    if (t < lastMono) t = lastMono;
    lastMono = t;
    return t;
  }
  /** Wall clock ms: StepIn.wall. */
  function wallNow() { return Date.now(); }

  const LEVELS = { debug: 5, info: 3, warn: 2, error: 1 };
  /** Log through Zotero.debug (the sandbox has no console). @param {'debug'|'info'|'warn'|'error'} level */
  function log(level, msg) {
    const text = '[zotero-fulltext] ' + String(msg);
    try {
      if (typeof Zotero !== 'undefined' && Zotero && typeof Zotero.debug === 'function') {
        Zotero.debug(text, LEVELS[level] || 3);
      } else if (typeof console !== 'undefined' && console && console.log) {
        console.log(level + ' ' + text);
      }
    } catch (e) { /* logging must never throw */ }
  }

  /** @returns {string} 'Name: message' for any thrown value */
  function errText(e) {
    if (e === undefined || e === null) return String(e);
    const name = e.name ? e.name + ': ' : '';
    return (name + (e.message !== undefined ? e.message : String(e))).slice(0, 500);
  }

  /** @param {ArrayBuffer|ArrayBufferView|Uint8Array} v @returns {Uint8Array} (no copy) */
  function toU8(v) {
    if (v instanceof Uint8Array) return v;
    if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
    return new Uint8Array(v);
  }
  /** @param {Uint8Array[]} parts @returns {Uint8Array} */
  function concatBytes(parts) {
    let n = 0;
    for (const p of parts) n += p.length;
    const out = new Uint8Array(n);
    let o = 0;
    for (const p of parts) { out.set(p, o); o += p.length; }
    return out;
  }
  /** JSON.parse that returns fallback on error. */
  function parseJson(text, fallback) {
    try { return JSON.parse(text); } catch (e) { return fallback; }
  }

  const util = {
    OP_ERR_KINDS, OpError, CancelScope, Deferred, TaskSet,
    sleep, idle, monoNow, wallNow, log, errText, toU8, concatBytes, parseJson,
  };
  ZFT.util = util;

  /**
   * ZFT.l10n: format a Fluent message outside any window (diagnostics, dialogs, notices).
   * Uses the scope-global Localization (plugins.js:169) over zotero-fulltext.ftl. In tests (or if the
   * FTL cannot be read) the fallback is "<id> {k=v, ...}" so assertions can still match ids.
   * Window DOM text uses document.l10n.setAttributes(el, id, args) instead.
   */
  let localization = null;
  const l10n = {
    /** @param {string} id message id @param {Object<string,string|number>} [args] @returns {string} */
    format(id, args) {
      try {
        if (!localization && typeof Localization !== 'undefined') {
          localization = new Localization([ZFT.FTL], true);
        }
        if (localization) {
          const text = localization.formatValueSync(id, args || {});
          if (text) return text;
        }
      } catch (e) { /* fall through to the id */ }
      const pairs = args ? Object.keys(args).map((k) => `${k}=${args[k]}`).join(', ') : '';
      return pairs ? `${id} {${pairs}}` : id;
    },
    /** Drop the cached Localization (shutdown, locale change). */
    reset() { localization = null; },
  };
  ZFT.l10n = l10n;
})(this);
