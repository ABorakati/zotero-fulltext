/* ZFT.effects: the effect runner = the only code that talks to the engine instance.
 *
 * Owner: js-host-effects. The engine is sans-IO: step(StepIn{now, wall, commands, completions}) returns
 * StepOut{effects, next_timer, more, ignored, stats}. The runner performs every Effect CONCURRENTLY (one Promise
 * per op, never awaited by the step loop), feeds completions back, and schedules the next step. The UI thread is
 * never blocked: a step is a short synchronous wasm call, all I/O is async, and `more` / `next_timer` are honoured
 * with macrotask yields.
 *
 * STEP LOOP (single-flight; commands and completions arriving while a step runs set `dirty` and are folded into the
 * next step):
 *   pump(): while (dirty) { take pending commands + completions; stepOut = host.step(...); dispatch effects in order;
 *           note `ignored`; if (stepOut.more) { await util.idle(); continue; } }  then arm ONE timer at next_timer
 *           (virtual deadline in the JS monotonic clock; re-armed after every step, cleared when null).
 *   Completions are batched: the first completion of a burst schedules a pump after COMPLETION_BATCH_MS (0 = next
 *   macrotask) so N simultaneous responses cost one step. Commands from send() are scheduled the same way.
 *
 * EFFECT DISPATCH (proto.rs Effect; "fire" = no completion expected):
 *   http          sink = (retain === 'pdf' || inspect.expect === 'pdf') ? file : memory. File sink: slot =
 *                 bodies.allocPdfSlot(run); net.request(req, {scope, sink, path}). Build RespMeta {status, final_url,
 *                 headers, body_len, head_len, truncated} and the staged bytes (bodies.stageBytes / stageFile),
 *                 raw = await host.inspect({spec: effect.inspect, resp}, staged.bytes), then HttpOk {status, final_url,
 *                 headers, bytes, ms, body, inspect}. body = BodyRef when retain !== 'none' (page: bodies.putMemory;
 *                 pdf: bodies.putFile) else null (file slot dir removed). Non-2xx on the file sink: inspect raw.errorBody.
 *   browser_load  raw = browser.load(req, {scope, host}); a captured Blob -> bodies.putBlobAsPdf and the inspect spec
 *                 becomes {expect:'pdf', requested: spec.requested, route: spec.route}; page_html -> UTF-8 -> putMemory.
 *                 -> BrowserOk {final_url, status, ms, pdf, page, url_trail, inspect}.
 *   login_window  login.open(effect, {scope, host, bodies}) -> LoginOk (the controller builds bodies + inspect itself).
 *                 Its scope hangs off the RUN scope, not the item scope: the one window serves every waiting item, so
 *                 cancelling the item that happened to open it must not close it (the engine sends Cancel{op} when nobody waits).
 *   login_update  the op id is FRESH (never the window's): login.update(windowOp, waiting) for the open window(s), then the
 *                 op is completed at once with ok {} (it lands in StepOut.ignored). If the id IS a live window op it is
 *                 not completed (that would end the window).
 *   run_translators  translators.run({page, url}, {scope, bodies}) -> TranslatorsOk.
 *   zotero_oa     zoteroOa.lookup({doi}, {scope}) -> ZoteroOaHit[] (the `ok` value is the JSON array).
 *   save_pdf      save.savePdf(req, {scope, bodies}) -> SaveOk; on success remember Outcome::Saved for synthetic reports.
 *                 A save that has started is never cancelled (its scope is detached; engine Cancel and cancel_run skip it).
 *   confirm       confirm.ask(req, {scope}) -> ConfirmOk.
 *   cancel        scope.cancel('engine') for that op and SUPPRESS its completion (the engine already dropped it).
 *   release       bodies.release(handle) (fire).
 *   progress      progress.update(run, item, row); remember the last row per item (fire).
 *   persist       state.set(key, value, {flush}); tracked in the persist queue (fire).
 *   run_finished  bodies.releaseAll(run); progress.finish(run, report); deps.onRunFinished(report) (fire).
 *   log, trace    ZFT.util.log / bounded ring buffer when opts.trace (fire).
 *   barrier:true  (http, browser_load) before the op starts, await every earlier persist write (reserve-then-commit
 *                 licence ledger: the reservation must be on disk before an institutional request leaves the machine).
 *
 * OP SCOPES: every run has a CancelScope; every item a child; every op a child of its item (or run) scope.
 * send([{type:'cancel_run'}]) / cancel_item cancel those scopes IMMEDIATELY (network aborts in the same tick) and
 * also forward the command to the engine. Handlers must obey ctx.scope (CancelScope) because there is no AbortController.
 * An op whose scope is cancelled by JS is answered AT ONCE with err.kind 'cancelled' (the handler may still be winding
 * down; its late result is dropped and any body it made is released): every op is answered exactly once.
 * A handler's rejection becomes {op, err: OpErr} (OpError.toOpErr(); anything else -> kind 'internal'); a throw is
 * never allowed to escape (TaskSet.spawn catches and logs).
 * Bodies: a completion that carries BodyRefs which the engine then lists in StepOut.ignored (or which JS drops because
 * the engine cancelled the op) is released by the runner, because the engine never owned them.
 *
 * FAILURE POLICY (no journal replay in v1): if host.step throws EngineTrap or EngineError, or the stall watchdog fires
 * (nothing in flight, no timer, no `more`, no waiting_user, unfinished runs, for STALL_MS), the runner: stops stepping,
 * cancels every op scope ('engine_failure'), releases all bodies, builds a synthetic RunReport per unfinished run
 * (items it saw saved -> Outcome::Saved; every other item -> Outcome::NeedsUser{why: 'engine_restarted'}; engine_restarts = 1),
 * calls progress.finish + deps.onRunFinished for each, calls host.resetEngine(), sets needsConfigure = true and
 * invokes deps.onEngineFailure(err). A stall first sends DumpState once and logs the reply.
 * While `failed` is set send() ignores every command except `configure`; a `configure` command clears the flag (this is
 * how ZFT.runs resumes after a failure), as does the explicit reset().
 * Stall detection: a check every STALL_MS / 3; two consecutive stalled checks send DumpState once, a third fails the run(s).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const COMPLETION_BATCH_MS = 0;
  const MAX_STEPS_PER_PUMP = 64;
  const STALL_MS = 15000;
  const TRACE_RING = 200;
  /** dispose() waits at most this long for running handlers to wind down. */
  const DISPOSE_WAIT_MS = 3000;
  /** proto.rs Effect "type" values this runner understands. */
  const EFFECT_TYPES = [
    'http', 'browser_load', 'login_window', 'login_update', 'run_translators', 'zotero_oa', 'save_pdf',
    'confirm', 'cancel', 'release', 'progress', 'persist', 'run_finished', 'log', 'trace',
  ];

  /**
   * @typedef {object} RunnerDeps
   * @property {{step: Function, inspect: Function, resetEngine: Function}} host      ZFT.wasmHost Host (or a fake)
   * @property {object} bodies     ZFT.bodies.BodyStore
   * @property {{request: Function}} net
   * @property {{load: Function}} browser
   * @property {{open: Function, update: Function}} login
   * @property {{run: Function}} translators
   * @property {{lookup: Function}} zoteroOa
   * @property {{savePdf: Function}} save
   * @property {{ask: Function}} confirm
   * @property {{update: Function, finish: Function}} progress
   * @property {{set: Function}} state
   * @property {{monoNow: () => number, wallNow: () => number}} [clock]            default ZFT.util
   * @property {{setTimeout: Function, clearTimeout: Function}} [timers]           default globals
   * @property {(report: object) => void} [onRunFinished]
   * @property {(err: Error) => void} [onEngineFailure]
   */

  /** @returns {{kind: string, message: string}} proto.rs OpErr for anything a handler threw */
  function opErrOf(e) {
    if (e && typeof e.toOpErr === 'function') return e.toOpErr();
    return { kind: 'internal', message: ZFT.util.errText(e).slice(0, 300) };
  }
  /** BodyRef handles carried by a completion payload (HttpOk.body, BrowserOk/LoginOk pdf and page). */
  function bodyHandles(ok) {
    const out = [];
    if (!ok || typeof ok !== 'object' || Array.isArray(ok)) return out;
    for (const key of ['body', 'pdf', 'page']) {
      const ref = ok[key];
      if (ref && typeof ref.handle === 'number') out.push(ref.handle);
    }
    return out;
  }
  /** model.rs RespMeta (nulls omitted) for the pure inspector. */
  function respMeta(status, finalUrl, headers, staged) {
    const meta = {
      status, final_url: finalUrl, headers: headers || {}, body_len: staged.body_len, truncated: !!staged.truncated,
    };
    if (staged.head_len != null) meta.head_len = staged.head_len;
    return meta;
  }
  function failedInspection(reason) { return { expect: 'failed', reason }; }

  class Runner {
    /** @param {RunnerDeps} deps @param {{trace?: boolean, stallMs?: number, disposeWaitMs?: number}} [opts] */
    constructor(deps, opts) {
      this.deps = deps;
      this.opts = opts || {};
      this.failed = false;
      this.needsConfigure = true;
      this._tasks = new ZFT.util.TaskSet('effects');
      this._root = new ZFT.util.CancelScope();
      this._runs = new Map();          // run -> {run, items, saved, rows, scope, itemScopes, started, finished}
      this._ops = new Map();           // op -> {op, type, run, item, scope, engineCancelled, posted, started, protected}
      this._pendingCommands = [];
      this._pendingCompletions = [];
      this._completionBodies = new Map();   // op -> handles the completion carried (released when the engine ignores it)
      this._pumping = false;
      this._dirty = false;
      this._pumpPromise = Promise.resolve();
      this._pumpTimer = null;
      this._engineTimer = null;
      this._lastOut = null;
      this._lastStats = null;
      this._steps = 0;
      this._persistQueue = Promise.resolve();
      this._persistsInFlight = 0;
      this._trace = [];
      this._stallTimer = null;
      this._stallStrikes = 0;
      this._dumpSent = false;
      this._quietWaiters = [];
      this._disposed = false;
    }

    // ------------------------------------------------------------------ public API
    /** Queue commands for the next step (CancelRun/CancelItem also cancel scopes at once) and schedule a pump. @param {object[]} commands */
    send(commands) {
      if (this._disposed || !Array.isArray(commands)) return;
      let queued = false;
      let started = false;
      for (const cmd of commands) {
        if (!cmd || typeof cmd.type !== 'string') continue;
        if (this.failed) {
          if (cmd.type !== 'configure') {
            ZFT.util.log('warn', `engine failed earlier: ignoring command ${cmd.type}`);
            continue;
          }
          this.failed = false;   // a new Configure is the resume signal
        }
        if (cmd.type === 'configure') {
          this.needsConfigure = false;
        } else if (cmd.type === 'start_run') {
          this._noteStartRun(cmd);
          started = true;
        } else if (cmd.type === 'cancel_run') {
          this.cancelScopes(cmd.run, 'user');
        } else if (cmd.type === 'cancel_item') {
          this._itemScope(cmd.run, cmd.item).cancel('user');
        }
        this._pendingCommands.push(cmd);
        queued = true;
      }
      if (started) this._updateStall();
      if (queued) this._schedulePump();
    }
    /** Run steps until the engine has no more immediate work, then arm the timer. Single-flight; safe to call any time. @returns {Promise<void>} */
    async pump() {
      if (this.failed || this._disposed) return;
      if (this._pumping) {
        this._dirty = true;
        return this._pumpPromise;
      }
      this._clearPumpTimer();
      this._pumping = true;
      this._dirty = true;   // an explicit pump always steps at least once (a timer deadline needs a step)
      const done = ZFT.util.Deferred();
      this._pumpPromise = done.promise;
      let more = false;
      let capped = false;
      try {
        let steps = 0;
        while ((this._dirty || more) && !this.failed && !this._disposed) {
          if (steps >= MAX_STEPS_PER_PUMP) {
            capped = true;
            break;
          }
          steps++;
          this._dirty = false;
          more = this._stepOnce();
          if (more) await ZFT.util.idle();
        }
      } catch (e) {
        ZFT.util.log('error', 'pump failed: ' + ZFT.util.errText(e));
      } finally {
        this._pumping = false;
        if (!this.failed && !this._disposed) {
          if (capped) this._schedulePump();   // yield one macrotask, then continue
          else this._armEngineTimer();
        }
        done.resolve();
        this._updateStall();
        this._notifyQuiet();
      }
    }
    /** Cancel every op of one run (or all runs) without telling the engine (shutdown, failure). */
    cancelScopes(run, reason) {
      const why = reason || 'cancelled';
      const force = why === 'engine_failure' || why === 'dispose';
      const all = run === undefined || run === null;
      if (all) {
        for (const r of Array.from(this._runs.values())) r.scope.cancel(why);
      } else {
        this._run(run).scope.cancel(why);
      }
      if (force) {   // saves that already started are detached from the scope tree: only a failure or dispose reaches them
        for (const rec of Array.from(this._ops.values())) {
          if (all || rec.run === run) rec.scope.cancel(why);
        }
      }
    }
    /** Clear the failed flag without a Configure (tests, manual recovery). needsConfigure stays set. */
    reset() {
      this.failed = false;
    }
    /** @returns {{ops: number, pendingCommands: number, pendingCompletions: number, steps: number, persistsInFlight: number, failed: boolean, lastStats: object|null}} */
    stats() {
      return {
        ops: this._ops.size,
        pendingCommands: this._pendingCommands.length,
        pendingCompletions: this._pendingCompletions.length,
        steps: this._steps,
        persistsInFlight: this._persistsInFlight,
        failed: this.failed,
        lastStats: this._lastStats,
      };
    }
    /** @returns {object[]} the trace ring buffer (opts.trace only) */
    traceLog() {
      return this._trace.slice();
    }
    /** Resolves when no op is in flight and no step is pending (tests, shutdown). @returns {Promise<void>} */
    whenQuiet() {
      if (this._isQuiet()) return Promise.resolve();
      const d = ZFT.util.Deferred();
      this._quietWaiters.push(d);
      return d.promise;
    }
    /** Cancel everything, clear timers, wait for tasks. */
    async dispose() {
      if (this._disposed) return;
      this._disposed = true;
      this._clearTimers();
      this._pendingCommands = [];
      this._pendingCompletions = [];
      this.cancelScopes(undefined, 'dispose');
      const timers = this._timers();
      let timer = null;
      const limit = new Promise((resolve) => {
        timer = timers.setTimeout(resolve, this.opts.disposeWaitMs != null ? this.opts.disposeWaitMs : DISPOSE_WAIT_MS);
      });
      await Promise.race([Promise.all([this._tasks.whenIdle(), this._persistQueue]), limit]);
      timers.clearTimeout(timer);
      this._notifyQuiet();
    }

    // ------------------------------------------------------------------ plumbing
    _timers() {
      return this.deps.timers || { setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: (id) => clearTimeout(id) };
    }
    _clock() {
      return this.deps.clock || ZFT.util;
    }
    _isQuiet() {
      return this._ops.size === 0 && this._pendingCompletions.length === 0 && this._pendingCommands.length === 0
        && !this._pumping && this._pumpTimer === null && this._persistsInFlight === 0;
    }
    _notifyQuiet() {
      if (!this._quietWaiters.length || !this._isQuiet()) return;
      const waiters = this._quietWaiters;
      this._quietWaiters = [];
      for (const d of waiters) d.resolve();
    }
    /** Run fn; log a sync throw or a rejected promise. UI and callback code must never break the loop. */
    _safe(label, fn) {
      try {
        const r = fn();
        if (r && typeof r.then === 'function') r.then(null, (e) => ZFT.util.log('error', `${label} failed: ${ZFT.util.errText(e)}`));
      } catch (e) {
        ZFT.util.log('error', `${label} failed: ${ZFT.util.errText(e)}`);
      }
    }
    _clearPumpTimer() {
      if (this._pumpTimer !== null) {
        this._timers().clearTimeout(this._pumpTimer);
        this._pumpTimer = null;
      }
    }
    _clearTimers() {
      this._clearPumpTimer();
      if (this._engineTimer !== null) {
        this._timers().clearTimeout(this._engineTimer);
        this._engineTimer = null;
      }
      if (this._stallTimer !== null) {
        this._timers().clearTimeout(this._stallTimer);
        this._stallTimer = null;
      }
    }
    /** A pump after COMPLETION_BATCH_MS; commands and completions that arrive meanwhile share one step. */
    _schedulePump() {
      this._dirty = true;
      if (this._pumping || this._pumpTimer !== null || this._disposed || this.failed) return;
      this._pumpTimer = this._timers().setTimeout(() => {
        this._pumpTimer = null;
        this._tasks.spawn('pump', () => this.pump());
      }, COMPLETION_BATCH_MS);
    }
    _armEngineTimer() {
      const timers = this._timers();
      if (this._engineTimer !== null) {
        timers.clearTimeout(this._engineTimer);
        this._engineTimer = null;
      }
      const at = this._lastOut ? this._lastOut.next_timer : null;
      if (at === null || at === undefined) return;
      const delay = Math.max(0, at - this._clock().monoNow());
      this._engineTimer = timers.setTimeout(() => {
        this._engineTimer = null;
        this._tasks.spawn('engine-timer', () => this.pump());
      }, delay);
    }

    // ------------------------------------------------------------------ run and scope bookkeeping
    _run(run) {
      let r = this._runs.get(run);
      if (!r) {
        r = {
          run, items: [], saved: new Map(), rows: new Map(),
          scope: this._root.child(), itemScopes: new Map(), started: false, finished: false,
        };
        this._runs.set(run, r);
      }
      return r;
    }
    _itemScope(run, item) {
      const r = this._run(run);
      let s = r.itemScopes.get(item);
      if (!s) {
        s = r.scope.child();
        r.itemScopes.set(item, s);
      }
      return s;
    }
    _noteStartRun(cmd) {
      const r = this._run(cmd.run);
      r.started = true;
      r.finished = false;
      r.items = (Array.isArray(cmd.items) ? cmd.items : []).map((it) => ({ item: it.item, key: it.key }));
    }
    _unfinishedRuns() {
      let n = 0;
      for (const r of this._runs.values()) if (r.started && !r.finished) n++;
      return n;
    }

    // ------------------------------------------------------------------ one step
    /** One synchronous engine step. @returns {boolean} the StepOut's `more` flag */
    _stepOnce() {
      const commands = this._pendingCommands;
      this._pendingCommands = [];
      const completions = this._pendingCompletions;
      this._pendingCompletions = [];
      const clock = this._clock();
      let out;
      try {
        out = this.deps.host.step({ now: clock.monoNow(), wall: clock.wallNow(), commands, completions });
      } catch (e) {
        this._fail(e);
        return false;
      }
      if (!out || typeof out !== 'object') {
        this._fail(new Error('engine returned no StepOut'));
        return false;
      }
      this._steps++;
      this._lastOut = out;
      this._lastStats = out.stats || null;
      const ignored = new Set(Array.isArray(out.ignored) ? out.ignored : []);
      for (const c of completions) {
        const handles = this._completionBodies.get(c.op);
        if (!handles) continue;
        this._completionBodies.delete(c.op);
        if (ignored.has(c.op)) this._releaseHandles(handles);   // the engine never owned these bodies
      }
      if (ignored.size && this.opts.trace) ZFT.util.log('debug', `engine ignored completions: ${Array.from(ignored).join(',')}`);
      for (const eff of Array.isArray(out.effects) ? out.effects : []) {
        try {
          this._dispatch(eff);
        } catch (e) {
          ZFT.util.log('error', `effect ${eff && eff.type} dispatch failed: ${ZFT.util.errText(e)}`);
        }
      }
      return !!out.more;
    }

    _dispatch(eff) {
      if (!eff || typeof eff.type !== 'string') return;
      switch (eff.type) {
        case 'http':
          this._startOp(eff, this._itemScope(eff.run, eff.item), (rec) => this._doHttp(eff, rec));
          break;
        case 'browser_load':
          this._startOp(eff, this._itemScope(eff.run, eff.item), (rec) => this._doBrowser(eff, rec));
          break;
        case 'login_window':
          this._startOp(eff, this._run(eff.run).scope, (rec) => this.deps.login.open(eff, {
            scope: rec.scope, host: this.deps.host, bodies: this.deps.bodies,
          }));
          break;
        case 'login_update':
          this._doLoginUpdate(eff);
          break;
        case 'run_translators':
          this._startOp(eff, this._itemScope(eff.run, eff.item), (rec) => this.deps.translators.run(
            { page: eff.page, url: eff.url }, { scope: rec.scope, bodies: this.deps.bodies },
          ));
          break;
        case 'zotero_oa':
          this._startOp(eff, this._itemScope(eff.run, eff.item), async (rec) => {
            const hits = await this.deps.zoteroOa.lookup({ doi: eff.doi }, { scope: rec.scope });
            return Array.isArray(hits) ? hits : [];
          });
          break;
        case 'save_pdf':
          this._startOp(eff, this._itemScope(eff.run, eff.item), (rec) => this._doSave(eff, rec));
          break;
        case 'confirm':
          this._startOp(eff, this._run(eff.run).scope, (rec) => this.deps.confirm.ask(eff.req, { scope: rec.scope }));
          break;
        case 'cancel':
          this._engineCancel(eff.op);
          break;
        case 'release':
          this._releaseHandles([eff.handle]);
          break;
        case 'progress': {
          const r = this._runs.get(eff.run);
          if (r) r.rows.set(eff.item, eff.row);
          this._safe('progress.update', () => this.deps.progress.update(eff.run, eff.item, eff.row));
          break;
        }
        case 'persist':
          this._doPersist(eff);
          break;
        case 'run_finished':
          this._doRunFinished(eff);
          break;
        case 'log':
          // Debug lines of the engine are only interesting when tracing.
          if (eff.level !== 'debug' || this.opts.trace) {
            ZFT.util.log(['debug', 'info', 'warn', 'error'].includes(eff.level) ? eff.level : 'info', 'engine: ' + eff.msg);
          }
          break;
        case 'trace':
          if (this.opts.trace) {
            this._trace.push({ run: eff.run, item: eff.item, ev: eff.ev });
            if (this._trace.length > TRACE_RING) this._trace.splice(0, this._trace.length - TRACE_RING);
          }
          break;
        default:
          ZFT.util.log('warn', `unknown effect type ${eff.type}`);
          if (typeof eff.op === 'number') {   // never leave the engine waiting
            this._pendingCompletions.push({ op: eff.op, err: { kind: 'unsupported', message: 'unknown effect ' + eff.type } });
            this._schedulePump();
          }
      }
    }

    // ------------------------------------------------------------------ ops
    /** Start one op as its own task under a child of `parent`; its result is posted exactly once. */
    _startOp(eff, parent, handler) {
      const op = eff.op;
      if (this._ops.has(op)) {
        ZFT.util.log('warn', `duplicate op ${op} (${eff.type}) ignored`);
        return;
      }
      const rec = {
        op, type: eff.type, run: eff.run, item: eff.item, scope: parent.child(),
        engineCancelled: false, posted: false, started: false, protected: false,
      };
      this._ops.set(op, rec);
      // Captured now: only persists dispatched BEFORE this effect gate it (reserve-then-commit).
      const gate = eff.barrier ? this._persistQueue : null;
      rec.scope.onCancel((reason) => this._post(rec, undefined, { kind: 'cancelled', message: String(reason) }));
      this._tasks.spawn(`${eff.type}#${op}`, async () => {
        let ok;
        let err = null;
        try {
          if (gate) await gate;
          rec.scope.throwIfCancelled();
          rec.started = true;
          ok = await handler(rec);
        } catch (e) {
          err = opErrOf(e);
        }
        rec.scope.dispose();
        this._post(rec, ok, err);
      });
    }
    /** Answer an op exactly once. Late results (already answered, engine-cancelled, engine failed) are dropped and their bodies released. */
    _post(rec, ok, err) {
      const handles = err ? [] : bodyHandles(ok);
      if (rec.posted) {
        this._releaseHandles(handles);
        return;
      }
      rec.posted = true;
      this._ops.delete(rec.op);
      if (rec.engineCancelled || this.failed || this._disposed) {
        this._releaseHandles(handles);
        this._notifyQuiet();
        return;
      }
      this._pendingCompletions.push(err ? { op: rec.op, err } : { op: rec.op, ok: ok === undefined ? null : ok });
      if (handles.length) this._completionBodies.set(rec.op, handles);
      this._schedulePump();
    }
    _engineCancel(op) {
      const rec = this._ops.get(op);
      if (!rec) return;   // already answered, or unknown
      if (rec.type === 'save_pdf' && rec.started) {
        if (this.opts.trace) ZFT.util.log('debug', `engine cancel of op ${op} ignored: the save already started`);
        return;
      }
      rec.engineCancelled = true;
      rec.scope.cancel('engine');
    }
    _releaseHandles(handles) {
      for (const h of handles) {
        this._tasks.spawn('release', () => this.deps.bodies.release(h));
      }
    }
    async _inspect(meta, bytes) {
      try {
        return (await this.deps.host.inspect(meta, bytes)) || failedInspection('inspect_error');
      } catch (e) {
        ZFT.util.log('warn', 'inspect failed: ' + ZFT.util.errText(e));
        return failedInspection('inspect_error');
      }
    }

    async _doHttp(eff, rec) {
      const { net, bodies } = this.deps;
      const scope = rec.scope;
      const spec = eff.inspect;
      const retain = eff.retain || 'none';
      const wantFile = retain === 'pdf' || !!(spec && spec.expect === 'pdf');
      let slot = null;
      let raw;
      try {
        if (wantFile) {
          slot = await bodies.allocPdfSlot(eff.run);
          scope.throwIfCancelled();
          raw = await net.request(eff.req, { scope, sink: 'file', path: slot.path });
        } else {
          raw = await net.request(eff.req, { scope, sink: 'memory' });
        }
      } catch (e) {
        if (slot) await bodies.discardSlot(slot);
        throw e;
      }
      let kept = false;
      try {
        const fileWritten = !!(slot && raw.path);
        let staged;
        if (fileWritten) staged = await bodies.stageFile(raw.path, raw.bytes);
        else staged = await bodies.stageBytes((slot ? raw.errorBody : raw.body) || new Uint8Array(0), 'page');
        const inspection = await this._inspect({ spec, resp: respMeta(raw.status, raw.final_url, raw.headers, staged) }, staged.bytes);
        scope.throwIfCancelled();
        let body = null;
        if (retain !== 'none') {
          if (fileWritten) {
            body = bodies.putFile({ path: raw.path, dir: slot.dir, len: staged.body_len, run: eff.run });
            kept = true;
          } else if (!slot && retain === 'page') {
            body = bodies.putMemory(raw.body || new Uint8Array(0), { run: eff.run, kind: 'page' });
          }
        }
        return {
          status: raw.status,
          final_url: raw.final_url,
          headers: raw.headers || {},
          bytes: Number.isFinite(raw.bytes) ? raw.bytes : staged.body_len,
          ms: Math.max(0, Math.round(raw.ms) || 0),
          body,
          inspect: inspection,
        };
      } finally {
        if (slot && !kept) await bodies.discardSlot(slot);
      }
    }

    async _doBrowser(eff, rec) {
      const { browser, bodies, host } = this.deps;
      const scope = rec.scope;
      const raw = await browser.load(eff.req, { scope, host });
      scope.throwIfCancelled();
      let pdf = null;
      let page = null;
      let committed = false;
      try {
        let spec = eff.inspect;
        let staged;
        let headers = {};
        if (raw.pdf) {
          pdf = await bodies.putBlobAsPdf(raw.pdf, eff.run);
          staged = await bodies.stageForInspect(pdf.handle);
          spec = {
            expect: 'pdf',
            requested: (spec && spec.requested) || {},
            route: (spec && spec.route) || eff.route,
          };
          headers = { 'content-type': 'application/pdf' };
        } else if (typeof raw.page_html === 'string') {
          const bytes = new TextEncoder().encode(raw.page_html);
          staged = await bodies.stageBytes(bytes, 'page');
          try {
            page = bodies.putMemory(bytes, { run: eff.run, kind: 'page', adopt: true });
          } catch (e) {
            if (!(e && e.kind === 'too_large')) throw e;
            ZFT.util.log('warn', 'page too large to keep for translators; inspecting the first part only');
          }
          headers = { 'content-type': 'text/html; charset=utf-8' };
        } else {
          staged = await bodies.stageBytes(new Uint8Array(0), 'page');
        }
        const status = raw.status != null ? raw.status : 200;
        const inspection = await this._inspect({ spec, resp: respMeta(status, raw.final_url, headers, staged) }, staged.bytes);
        scope.throwIfCancelled();
        committed = true;
        return {
          final_url: raw.final_url,
          status: raw.status != null ? raw.status : null,
          ms: Math.max(0, Math.round(raw.ms) || 0),
          pdf,
          page,
          url_trail: Array.isArray(raw.url_trail) ? raw.url_trail : [],
          inspect: inspection,
        };
      } finally {
        if (!committed) this._releaseHandles([pdf, page].filter(Boolean).map((b) => b.handle));
      }
    }

    async _doSave(eff, rec) {
      rec.protected = true;
      rec.scope.dispose();   // detach: cancelling the item or run no longer reaches a save that started
      const ok = await this.deps.save.savePdf(eff.req, { scope: rec.scope, bodies: this.deps.bodies });
      const req = eff.req;
      this._run(eff.run).saved.set(eff.item, {
        kind: 'saved', route: req.route, source: req.source, version: req.version,
        url: req.url_to_store, attachment_key: ok && ok.attachment_key,
      });
      return ok;
    }

    _doLoginUpdate(eff) {
      const own = this._ops.get(eff.op);
      let targets;
      if (own && own.type === 'login_window') {
        targets = [eff.op];   // defensive: an engine that reuses the window's id must not end the window
      } else {
        targets = [];
        for (const rec of this._ops.values()) if (rec.type === 'login_window') targets.push(rec.op);
      }
      for (const op of targets) this._safe('login.update', () => this.deps.login.update(op, eff.waiting));
      if (!own) {
        this._pendingCompletions.push({ op: eff.op, ok: {} });
        this._schedulePump();
      }
    }

    // ------------------------------------------------------------------ persistence and run end
    _doPersist(eff) {
      const state = this.deps.state;
      this._persistsInFlight++;
      const job = this._persistQueue.then(() => state.set(eff.key, eff.value, { flush: !!eff.flush }));
      this._persistQueue = job
        .catch((e) => ZFT.util.log('error', `persist ${eff.key} failed: ${ZFT.util.errText(e)}`))
        .then(() => {
          this._persistsInFlight--;
          this._notifyQuiet();
        });
    }

    _doRunFinished(eff) {
      const { run, report } = eff;
      this._tasks.spawn('releaseAll', () => this.deps.bodies.releaseAll(run));
      this._safe('progress.finish', () => this.deps.progress.finish(run, report));
      this._safe('onRunFinished', () => (this.deps.onRunFinished ? this.deps.onRunFinished(report) : undefined));
      const r = this._runs.get(run);
      if (r) {
        r.finished = true;
        r.scope.cancel('run_finished');   // nothing may outlive its run (windows, hidden browsers); started saves are detached
        this._runs.delete(run);
      }
    }

    // ------------------------------------------------------------------ failure policy
    /** Idempotent. See the header: cancel everything, synthetic reports, reset the engine instance. */
    _fail(err) {
      if (this.failed) return;
      this.failed = true;
      ZFT.util.log('error', 'engine failure: ' + ZFT.util.errText(err));
      this._clearTimers();
      this._pendingCommands = [];
      this._pendingCompletions = [];
      this._completionBodies.clear();
      this._stallStrikes = 0;
      this._dumpSent = false;
      const unfinished = Array.from(this._runs.values()).filter((r) => r.started && !r.finished);
      this.cancelScopes(undefined, 'engine_failure');
      for (const rec of this._ops.values()) rec.posted = true;   // late results of still-running handlers are dropped
      this._ops.clear();
      this._runs = new Map();
      this._tasks.spawn('releaseAll', () => this.deps.bodies.releaseAll());
      for (const r of unfinished) {
        const report = syntheticReport(r.run, r.items, r.saved);
        this._safe('progress.finish', () => this.deps.progress.finish(r.run, report));
        this._safe('onRunFinished', () => (this.deps.onRunFinished ? this.deps.onRunFinished(report) : undefined));
      }
      this._safe('host.resetEngine', () => this.deps.host.resetEngine());
      this._lastOut = null;
      this._lastStats = null;
      this.needsConfigure = true;
      this._safe('onEngineFailure', () => (this.deps.onEngineFailure ? this.deps.onEngineFailure(err) : undefined));
      this._notifyQuiet();
    }

    // ------------------------------------------------------------------ stall watchdog
    _stallIntervalMs() {
      return Math.max(1, Math.floor((this.opts.stallMs != null ? this.opts.stallMs : STALL_MS) / 3));
    }
    /** Keep the watchdog armed while a started run is unfinished, disarmed otherwise. */
    _updateStall() {
      if (this.failed || this._disposed) return;
      const active = this._unfinishedRuns() > 0;
      if (active && this._stallTimer === null) {
        this._stallTimer = this._timers().setTimeout(() => this._stallCheck(), this._stallIntervalMs());
      } else if (!active && this._stallTimer !== null) {
        this._timers().clearTimeout(this._stallTimer);
        this._stallTimer = null;
        this._stallStrikes = 0;
        this._dumpSent = false;
      }
    }
    _isStalled() {
      const out = this._lastOut;
      if (!out) return false;
      const stats = this._lastStats;
      return this._ops.size === 0 && !this._pumping && this._pumpTimer === null
        && this._pendingCommands.length === 0 && this._pendingCompletions.length === 0 && this._persistsInFlight === 0
        && !out.more && (out.next_timer === null || out.next_timer === undefined)
        && !(stats && stats.waiting_user > 0);
    }
    _stallCheck() {
      this._stallTimer = null;
      if (this.failed || this._disposed) return;
      if (this._unfinishedRuns() === 0) {
        this._stallStrikes = 0;
        return;
      }
      if (this._isStalled()) {
        this._stallStrikes++;
        if (this._stallStrikes >= 3 && this._dumpSent) {
          this._fail(new Error('engine stalled'));
          return;
        }
        if (this._stallStrikes >= 2 && !this._dumpSent) {
          this._dumpSent = true;
          ZFT.util.log('warn', 'engine looks stalled: requesting a state dump');
          this.send([{ type: 'dump_state' }]);
        }
      } else {
        this._stallStrikes = 0;
      }
      this._updateStall();
    }
  }

  /** @param {RunnerDeps} deps @param {{trace?: boolean, stallMs?: number}} [opts] @returns {Runner} */
  function createRunner(deps, opts) {
    return new Runner(deps, opts);
  }

  /**
   * Build the synthetic RunReport used on engine failure (exported for tests).
   * @param {number} run @param {Array<{item: number, key: string}>} items
   * @param {Map<number, object>} saved item -> proto Outcome::Saved @returns {object} model.rs RunReport
   */
  function syntheticReport(run, items, saved) {
    const list = Array.isArray(items) ? items : [];
    const lookup = saved && typeof saved.get === 'function' ? saved : { get: () => undefined };
    let nSaved = 0;
    const entries = list.map((it) => {
      const done = lookup.get(it.item);
      if (done) {
        nSaved++;
        return { item: it.item, key: it.key, outcome: done, trace: [] };
      }
      return {
        item: it.item, key: it.key, trace: [],
        outcome: { kind: 'needs_user', why: 'engine_restarted', handoff: {} },
      };
    });
    return {
      run,
      counts: {
        saved: nSaved, skipped: 0, not_found: 0, needs_user: list.length - nSaved, blocked: 0, deferred: 0, cancelled: 0, failed: 0,
      },
      items: entries,
      ledger: { hour: 0, hour_cap: 0, day: 0, day_cap: 0, mb_hour: 0, mb_hour_cap: 0 },
      halted: null,
      engine_restarts: 1,
    };
  }

  ZFT.effects = {
    COMPLETION_BATCH_MS, MAX_STEPS_PER_PUMP, STALL_MS, TRACE_RING, EFFECT_TYPES,
    Runner, createRunner, syntheticReport,
  };
})(this);
