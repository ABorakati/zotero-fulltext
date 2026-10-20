/* ZFT.runs: the run controller (selection -> StartRun -> report). Wires the real services into ZFT.effects.
 *
 * Owner: js-runs. One Runner (= one engine instance) lives for the whole plugin lifetime so the licence ledger,
 * halts and caches in the engine survive between runs; several runs may be active at once (RunId = ZFT.state.nextRunId()).
 *
 * init() / ensureReady() (lazy, idempotent, concurrent callers share one promise):
 *   1. ZFT.state.init(); ZFT.state.sweepTmp()  (stale temp dirs from a crashed session)
 *   2. host = await ZFT.wasmHost.get()  (a failed load throws; the menu shows zotero-fulltext-notice-wasm-load-failed once)
 *   3. ZFT.bodies.store = new BodyStore({state: ZFT.state}); runner = ZFT.effects.createRunner({host, bodies: store,
 *      net: ZFT.net, browser: ZFT.browser, login: ZFT.login, translators: ZFT.translators, zoteroOa: ZFT.zoteroOa,
 *      save: ZFT.save, confirm: ZFT.confirm, progress: ZFT.progress, state: ZFT.state, onRunFinished, onEngineFailure},
 *      {trace: pref debug.trace})
 *   4. Configure: runner.send([{type:'configure', protocol: ZFT.PROTOCOL, config: ZFT.config.buildEngineConfig(),
 *      persisted: ZFT.state.getPersisted(), seed: <random u53 from crypto.getRandomValues>}]). Re-sent when
 *      runner.needsConfigure (after an engine failure) or when prefs changed (reconfigure(): only while no run is active;
 *      otherwise deferred until the last active run finishes).
 *
 * startRun({items, mode, interactive}):
 *   snapshots = await ZFT.items.snapshotAll(items)  (ALL selected items, including attachments/notes: Rust decides skips)
 *   run = nextRunId(); model = ZFT.progress.open(run, snapshots, {interactive}); watch deletions;
 *   runner.send([{type:'start_run', run, mode, interactive, items: snapshots, resume: []}]);
 *   returns RunHandle {run, done: Promise<RunReport> (never rejects; engine failure resolves with the synthetic report),
 *   cancel()}.
 * cancelRun / cancelItem -> runner.send([{type:'cancel_run'|'cancel_item', ...}]); userAction(run, kind) ->
 * runner.send([{type:'user_action', run, kind}]) with kind in sign_in_now | skip_sign_in | keep_waiting | retry_sign_in.
 * Item deleted/trashed during a run -> cancel_item for every active run that contains it.
 * shutdown(): cancel_run for all active runs + {type:'shutdown'}, one last pump, wait <= SHUTDOWN_WAIT_MS for quiet,
 *   runner.dispose(), bodies.releaseAll(), state.flush() + sweepTmp(), pdfCapture.disposeAll(), login.closeAll(),
 *   progress.closeAll(), wasmHost.shutdown(). Idempotent.
 *
 * Implementation notes (decisions the header above leaves open):
 *  - Errors thrown to the UI are Error objects whose `message` is an l10n id (zotero-fulltext-notice-*); an optional
 *    `reason` string is the {$reason} argument of the start-failed message.
 *  - The controller sends only commands it has validated (integer ids, known kinds): serde rejects a whole StepIn on
 *    one malformed command, which would fail every run of the engine instance.
 *  - Configure is also re-sent by startRun when the prefs changed and no run is active, so a pane that forgot to call
 *    reconfigure() cannot leave the next run on stale settings. With runs active the change waits (pendingReconfigure).
 *  - `needConfigure` is the controller's own flag (set at init and by onEngineFailure, cleared when it sends Configure);
 *    it does not depend on how Runner.needsConfigure is maintained.
 *  - shutdown() also disposes ZFT.state (its last step) and resolves the done promise of any run the engine did not
 *    finish with a report whose items are Cancelled{by: shutdown}, so no caller waits forever.
 *  - Test seam: init(overrides) takes {state, items, config, progress, login, pdfCapture, net, browser, translators,
 *    zoteroOa, save, confirm, wasmHost ({get, shutdown}) or host (a ready Host), bodies (a store instance), createRunner,
 *    services ({prompt}), timers, clock, random}. Only the call that actually builds the controller uses them.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const SHUTDOWN_WAIT_MS = 2000;
  const MODES = ['full', 'oa_only'];
  const USER_ACTIONS = ['sign_in_now', 'skip_sign_in', 'keep_waiting', 'retry_sign_in'];
  /** Sanity cap on one run: a far larger selection is almost certainly a mistake. */
  const MAX_ITEMS_PER_RUN = 5000;
  /** Finished-run reports kept for report(run). */
  const REPORTS_KEPT = 20;

  const NOTICE = {
    noEligible: 'zotero-fulltext-notice-no-eligible',
    wasmLoadFailed: 'zotero-fulltext-notice-wasm-load-failed',
    engineFailed: 'zotero-fulltext-notice-engine-failed',
    startFailed: 'zotero-fulltext-notice-start-failed',
  };

  /** @typedef {{run: number, done: Promise<object>, cancel: () => void}} RunHandle */

  // ---------------------------------------------------------------- controller state (one engine per plugin lifetime)
  let initPromise = null;     // memoised init; null = not built (or shut down)
  let closing = null;         // shutdown promise while a shutdown is running
  let deps = null;            // resolved dependencies of the live controller
  let runnerInst = null;
  let tasks = null;           // TaskSet for fire-and-forget work
  let configHash = null;      // JSON of the EngineConfig the engine currently has
  let needConfigure = false;  // the engine has no (valid) Configure yet
  let pendingReconfigure = false;
  let failureNoticeShown = false;
  let unwatch = null;         // unregister function of the deletion observer
  /** run id -> {run, done: Deferred, ids: Set<itemID> (not yet reported gone), items: [{item, key}]} */
  const active = new Map();
  /** run id -> RunReport, oldest first. */
  const reports = new Map();

  const log = (level, msg) => ZFT.util.log(level, 'runs: ' + msg);
  const errText = (e) => ZFT.util.errText(e);

  /** An Error whose message is an l10n id (shown by the UI), with the detail in `reason`. */
  function noticeError(id, reason, cause) {
    const e = new Error(id);
    if (reason) e.reason = String(reason);
    if (cause) e.cause = cause;
    return e;
  }

  function isU32(n) { return Number.isInteger(n) && n >= 0 && n <= 4294967295; }

  // ---------------------------------------------------------------- dependencies
  function resolveDeps(o) {
    const wasmHost = o.wasmHost
      || (o.host ? { get: () => Promise.resolve(o.host), shutdown: () => Promise.resolve() } : ZFT.wasmHost);
    const pick = (key) => (o[key] !== undefined ? o[key] : ZFT[key]);
    return {
      state: pick('state'), items: pick('items'), config: pick('config'), progress: pick('progress'),
      login: pick('login'), pdfCapture: pick('pdfCapture'), net: pick('net'), browser: pick('browser'),
      translators: pick('translators'), zoteroOa: pick('zoteroOa'), save: pick('save'), confirm: pick('confirm'),
      wasmHost,
      createRunner: o.createRunner || ((runnerDeps, opts) => ZFT.effects.createRunner(runnerDeps, opts)),
      bodies: o.bodies || null,
      services: o.services || null,
      timers: o.timers || null,
      clock: o.clock || null,
      random: o.random || null,
    };
  }

  function services(d) {
    return d.services || (typeof Services !== 'undefined' ? Services : null);
  }
  /** Timer functions resolved at call time (fake timers in tests, the globals otherwise). */
  function timerApi(d) {
    return d.timers || { setTimeout: (fn, ms) => setTimeout(fn, ms), clearTimeout: (id) => clearTimeout(id) };
  }

  /** Random integer below 2^53 (the engine's u64 seed, exact in a JS number). */
  function randomSeed(d) {
    const words = new Uint32Array(2);
    try {
      if (d.random) d.random(words);
      else crypto.getRandomValues(words);
    } catch (e) {
      words[0] = Math.floor(Math.random() * 4294967296);
      words[1] = Math.floor(Math.random() * 4294967296);
    }
    return (words[0] & 0x1fffff) * 4294967296 + words[1];
  }

  /** @returns {{cmd: object, hash: string}} the Configure command for the current prefs and persisted state */
  function buildConfigure(d) {
    const config = d.config.buildEngineConfig();
    return {
      hash: JSON.stringify(config),
      cmd: {
        type: 'configure',
        protocol: ZFT.PROTOCOL,
        config,
        persisted: d.state.getPersisted(),
        seed: randomSeed(d),
      },
    };
  }

  /** Record that `built` was handed to the runner. */
  function markConfigured(built) {
    configHash = built.hash;
    needConfigure = false;
    pendingReconfigure = false;
    failureNoticeShown = false;
  }

  /** Send commands to the live runner. @returns {boolean} false when there is no runner or send failed */
  function sendCommands(commands) {
    const runner = runnerInst;
    if (!runner) {
      log('warn', 'no engine running, dropped: ' + commands.map((c) => c.type).join(', '));
      return false;
    }
    try {
      runner.send(commands);
      return true;
    } catch (e) {
      log('error', `send failed (${commands.map((c) => c.type).join(', ')}): ${errText(e)}`);
      return false;
    }
  }

  // ---------------------------------------------------------------- runner callbacks
  /** Runner callback: a run ended (engine RunFinished or a synthetic report after an engine failure). Never throws. */
  function onRunFinished(report) {
    try {
      const run = report && report.run;
      if (Number.isInteger(run)) {
        reports.delete(run);
        reports.set(run, report);
        while (reports.size > REPORTS_KEPT) reports.delete(reports.keys().next().value);
      }
      const entry = active.get(run);
      if (entry) {
        active.delete(run);
        entry.done.resolve(report);
      }
      if (!active.size) stopWatching();
      if (pendingReconfigure && !active.size && runnerInst && !closing) applyReconfigure();
      if (deps && tasks) {
        const state = deps.state;
        tasks.spawn('flush state', () => state.flush());
      }
    } catch (e) {
      log('error', `onRunFinished failed: ${errText(e)}`);
    }
  }

  /** Runner callback: the engine trapped/errored/stalled and the runner already failed the runs. Never throws. */
  function onEngineFailure(err) {
    try {
      needConfigure = true;
      configHash = null;
      log('error', 'engine failure: ' + errText(err) + (err && err.panic ? ' (panic: ' + err.panic + ')' : ''));
      if (failureNoticeShown || !deps) return;
      failureNoticeShown = true;
      const svc = services(deps);
      if (svc && svc.prompt && typeof svc.prompt.alert === 'function') {
        let win = null;
        try { win = Zotero.getMainWindow(); } catch (e) { /* alert without a parent */ }
        svc.prompt.alert(win, ZFT.l10n.format('zotero-fulltext-progress-title'), ZFT.l10n.format(NOTICE.engineFailed));
      }
    } catch (e) {
      log('error', `onEngineFailure failed: ${errText(e)}`);
    }
  }

  /** Deletion observer callback: cancel the item in every active run that still contains it. */
  function onItemGone(id) {
    const commands = [];
    for (const entry of active.values()) {
      if (entry.ids.delete(id)) commands.push({ type: 'cancel_item', run: entry.run, item: id });
    }
    if (commands.length) sendCommands(commands);
  }

  function startWatching(d) {
    if (unwatch) return;
    try {
      unwatch = d.items.watchDeletions(onItemGone);
    } catch (e) {
      log('warn', `deletion watcher not available: ${errText(e)}`);
    }
  }
  function stopWatching() {
    const off = unwatch;
    unwatch = null;
    if (!off) return;
    try { off(); } catch (e) { log('warn', `unwatch failed: ${errText(e)}`); }
  }

  // ---------------------------------------------------------------- init
  async function build(overrides) {
    const d = resolveDeps(overrides || {});
    try {
      await d.state.init();
    } catch (e) {
      log('error', `state init failed: ${errText(e)}`);
      throw noticeError(NOTICE.startFailed, 'state: ' + errText(e), e);
    }
    try {
      await d.state.sweepTmp();
    } catch (e) {
      log('warn', `temp sweep failed: ${errText(e)}`);
    }
    let host;
    try {
      host = await d.wasmHost.get();
    } catch (e) {
      log('error', `wasm load failed: ${errText(e)}`);
      throw noticeError(NOTICE.wasmLoadFailed, errText(e), e);
    }

    let storeAssigned = false;
    let runner = null;
    try {
      const store = d.bodies || new ZFT.bodies.BodyStore({ state: d.state });
      if (ZFT.bodies) { ZFT.bodies.store = store; storeAssigned = true; }
      const runnerDeps = {
        host, bodies: store, net: d.net, browser: d.browser, login: d.login, translators: d.translators,
        zoteroOa: d.zoteroOa, save: d.save, confirm: d.confirm, progress: d.progress, state: d.state,
        onRunFinished, onEngineFailure,
      };
      if (d.clock) runnerDeps.clock = d.clock;
      if (d.timers) runnerDeps.timers = d.timers;
      let trace = false;
      try { trace = !!d.config.getPref('debug.trace'); } catch (e) { /* tracing stays off */ }
      runner = d.createRunner(runnerDeps, { trace });

      const built = buildConfigure(d);
      deps = d;
      runnerInst = runner;
      tasks = new ZFT.util.TaskSet('runs');
      runner.send([built.cmd]);
      markConfigured(built);
    } catch (e) {
      log('error', `engine setup failed: ${errText(e)}`);
      deps = null;
      runnerInst = null;
      tasks = null;
      if (storeAssigned) ZFT.bodies.store = null;
      if (runner) {
        try { Promise.resolve(runner.dispose()).catch(() => {}); } catch (e2) { /* nothing to dispose */ }
      }
      throw noticeError(NOTICE.startFailed, errText(e), e);
    }
  }

  /**
   * Build everything (see header). Concurrent callers share one promise; a failure is not cached.
   * @param {object} [overrides] test seam (see the implementation notes) @returns {Promise<void>}
   * @throws {Error} message = l10n id: zotero-fulltext-notice-wasm-load-failed or zotero-fulltext-notice-start-failed (+ .reason)
   */
  function init(overrides) {
    if (closing) return closing.then(() => init(overrides));
    if (!initPromise) {
      const p = build(overrides);
      initPromise = p;
      p.catch(() => { if (initPromise === p) initPromise = null; });
    }
    return initPromise;
  }

  // ---------------------------------------------------------------- configuration
  /** Send Configure if the prefs differ from what the engine has (or it has none). Only call with no run active. */
  function applyReconfigure() {
    pendingReconfigure = false;
    const d = deps;
    if (!d || !runnerInst) return;
    let built;
    try {
      built = buildConfigure(d);
    } catch (e) {
      log('warn', `reconfigure skipped, the settings could not be read: ${errText(e)}`);
      return;
    }
    if (!needConfigure && built.hash === configHash) return;
    if (sendCommands([built.cmd])) markConfigured(built);
  }

  /** Re-read prefs and send Configure (deferred while runs are active). @returns {Promise<void>} */
  async function reconfigure() {
    if (closing || !initPromise) return;
    try { await initPromise; } catch (e) { return; }
    if (!runnerInst || closing) return;
    if (active.size) { pendingReconfigure = true; return; }
    applyReconfigure();
  }

  // ---------------------------------------------------------------- runs
  /**
   * Start a run for the selected Zotero items.
   * @param {{items: object[], mode?: 'full'|'oa_only', interactive?: boolean}} opts mode defaults to 'full', interactive to false
   * @returns {Promise<RunHandle>}
   * @throws {Error} message is an l10n id for the UI: no-eligible (nothing to do), wasm-load-failed, or start-failed (+ .reason)
   */
  async function startRun(opts) {
    const o = opts || {};
    await init();
    const d = deps;
    const runner = runnerInst;
    if (!d || !runner || closing) throw noticeError(NOTICE.startFailed, 'the plugin is shutting down');

    const items = Array.from(o.items || []);
    if (!items.length) throw new Error(NOTICE.noEligible);
    if (items.length > MAX_ITEMS_PER_RUN) throw noticeError(NOTICE.startFailed, `more than ${MAX_ITEMS_PER_RUN} items selected`);
    const mode = o.mode === undefined || o.mode === null ? 'full' : o.mode;
    if (!MODES.includes(mode)) throw noticeError(NOTICE.startFailed, 'unknown mode ' + String(mode));
    const interactive = o.interactive === true;

    const snapshots = await d.items.snapshotAll(items);
    if (!snapshots.length) throw new Error(NOTICE.noEligible);
    if (runnerInst !== runner || closing) throw noticeError(NOTICE.startFailed, 'the plugin is shutting down');

    // Configure first when the engine needs it (after a failure), or when the prefs changed and nothing else runs.
    const commands = [];
    let built = null;
    try {
      const fresh = buildConfigure(d);
      if (needConfigure || fresh.hash !== configHash) {
        if (needConfigure || !active.size) { built = fresh; commands.push(fresh.cmd); }
        else pendingReconfigure = true;
      }
    } catch (e) {
      if (needConfigure) throw noticeError(NOTICE.startFailed, 'settings: ' + errText(e), e);
      log('warn', `settings could not be re-read, using the previous ones: ${errText(e)}`);
    }

    const run = d.state.nextRunId();
    const entry = {
      run,
      done: ZFT.util.Deferred(),
      ids: new Set(snapshots.map((s) => s.item)),
      items: snapshots.map((s) => ({ item: s.item, key: s.key })),
    };
    active.set(run, entry);
    startWatching(d);
    try {
      d.progress.open(run, snapshots, { interactive });
    } catch (e) {
      log('warn', `progress window for run ${run} failed to open: ${errText(e)}`);
    }
    commands.push({ type: 'start_run', run, mode, interactive, items: snapshots, resume: [] });
    try {
      runner.send(commands);
    } catch (e) {
      active.delete(run);
      if (!active.size) stopWatching();
      throw noticeError(NOTICE.startFailed, errText(e), e);
    }
    if (built) markConfigured(built);
    return { run, done: entry.done.promise, cancel: () => cancelRun(run) };
  }

  /** @param {number} run */
  function cancelRun(run) {
    if (!isU32(run)) { log('warn', 'cancelRun: bad run id ' + String(run)); return; }
    sendCommands([{ type: 'cancel_run', run }]);
  }
  /** @param {number} run @param {number} item */
  function cancelItem(run, item) {
    if (!isU32(run) || !isU32(item)) { log('warn', `cancelItem: bad ids ${String(run)}/${String(item)}`); return; }
    sendCommands([{ type: 'cancel_item', run, item }]);
  }
  /** @param {number} run @param {'sign_in_now'|'skip_sign_in'|'keep_waiting'|'retry_sign_in'} kind */
  function userAction(run, kind) {
    if (!isU32(run) || !USER_ACTIONS.includes(kind)) { log('warn', `userAction: bad arguments ${String(run)}/${String(kind)}`); return; }
    sendCommands([{ type: 'user_action', run, kind }]);
  }

  /** @returns {number[]} ids of runs that have not finished */
  function activeRuns() {
    return Array.from(active.keys());
  }
  /** @returns {object|null} the RunReport of a finished run (the last REPORTS_KEPT are kept) */
  function report(run) {
    return reports.get(run) || null;
  }
  /** @returns {object|null} the Runner (harness / tests) */
  function runner() {
    return runnerInst;
  }

  // ---------------------------------------------------------------- shutdown
  /** Report for a run the engine never finished (shutdown): every item Cancelled{by: shutdown}. */
  function shutdownReport(entry) {
    const items = entry.items.map((i) => ({ item: i.item, key: i.key, outcome: { kind: 'cancelled', by: 'shutdown' }, trace: [] }));
    return {
      run: entry.run,
      counts: { saved: 0, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: items.length, failed: 0 },
      items,
      ledger: { hour: 0, hour_cap: 0, day: 0, day_cap: 0, mb_hour: 0, mb_hour_cap: 0 },
      halted: null,
      engine_restarts: 0,
    };
  }

  /** Resolve when the runner is quiet or SHUTDOWN_WAIT_MS passed, whichever is first. */
  function waitQuiet(runner, d) {
    return new Promise((resolve) => {
      const t = timerApi(d);
      const id = t.setTimeout(resolve, SHUTDOWN_WAIT_MS);
      const done = () => { t.clearTimeout(id); resolve(); };
      Promise.resolve().then(() => runner.whenQuiet()).then(done, done);
    });
  }

  async function doShutdown() {
    try { await initPromise; } catch (e) { /* a failed init built nothing */ }
    const d = deps;
    const runner = runnerInst;
    if (!d) { initPromise = null; return; }
    const step = async (name, fn) => {
      try { await fn(); } catch (e) { log('warn', `shutdown: ${name} failed: ${errText(e)}`); }
    };

    if (runner) {
      await step('cancel runs', () => {
        const commands = Array.from(active.keys()).map((run) => ({ type: 'cancel_run', run }));
        commands.push({ type: 'shutdown' });
        runner.send(commands);
      });
      await step('last pump', () => runner.pump());
      await step('wait for quiet', () => waitQuiet(runner, d));
      await step('dispose runner', () => runner.dispose());
    }
    if (tasks) await step('background tasks', () => tasks.whenIdle());
    const store = d.bodies || (ZFT.bodies && ZFT.bodies.store);
    if (store) await step('release bodies', () => store.releaseAll());
    await step('flush state', () => d.state.flush());
    await step('sweep temp dirs', () => d.state.sweepTmp());
    if (d.pdfCapture) await step('pdf capture', () => d.pdfCapture.disposeAll());
    if (d.login) await step('login windows', () => d.login.closeAll());
    if (d.progress) await step('progress windows', () => d.progress.closeAll());
    await step('wasm host', () => d.wasmHost.shutdown());
    await step('state', () => d.state.dispose());

    stopWatching();
    for (const entry of Array.from(active.values())) {
      active.delete(entry.run);
      entry.done.resolve(shutdownReport(entry));
    }
    if (ZFT.bodies && store && ZFT.bodies.store === store) ZFT.bodies.store = null;
    deps = null;
    runnerInst = null;
    tasks = null;
    configHash = null;
    needConfigure = false;
    pendingReconfigure = false;
    failureNoticeShown = false;
    initPromise = null;
  }

  /** Graceful stop (see header). Idempotent: concurrent calls share one run, later calls resolve at once. @returns {Promise<void>} */
  function shutdown() {
    if (closing) return closing;
    if (!initPromise) return Promise.resolve();
    const p = doShutdown().then(
      () => { if (closing === p) closing = null; },
      (e) => { log('error', `shutdown failed: ${errText(e)}`); if (closing === p) closing = null; },
    );
    closing = p;
    return p;
  }

  ZFT.runs = {
    SHUTDOWN_WAIT_MS, MODES, USER_ACTIONS, MAX_ITEMS_PER_RUN, REPORTS_KEPT,
    init, startRun, cancelRun, cancelItem, userAction, reconfigure, activeRuns, report, runner, shutdown,
  };
})(this);
