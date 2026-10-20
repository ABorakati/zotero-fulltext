/* ZFT.plugin: lifecycle logic behind bootstrap.js (kept here so Node tests can drive it with fakes).
 *
 * Owner: js-shell. bootstrap.js is deliberately thin: it registers the chrome package, loads namespace.js and
 * ZFT.LOAD_ORDER into the plugin scope, then calls ZFT.plugin.startup / shutdown / onMainWindowLoad / onMainWindowUnload.
 *
 * startup(info, reason)  idempotent (the sandbox is reused on disable -> enable, plugins.js:133-136); concurrent and
 *   repeated calls share one promise, a failed startup can be retried:
 *   1. return if already started; ZFT.info = info; Zotero.ZoteroFullText = ZFT (handle for the prefs-pane/window scripts and the
 *      harness; deleted on shutdown); await Zotero.initializationPromise (NOT uiReadyPromise)
 *   2. Zotero.PreferencePanes.register({pluginID: ZFT.ID, src: ZFT.CONTENT_URL + 'prefs.xhtml',
 *        scripts: [ZFT.CONTENT_URL + 'prefs-pane.js'], label: <ftl>, image: icon, helpURL: <repo url>}) -> pane id, kept in
 *        ZFT.plugin.paneID (menu.js opens it). A rejection is logged and startup continues: menus still work.
 *   3. ZFT.menu.register()
 *   4. for each window in Zotero.getMainWindows(): addToWindow(win)   // onMainWindowLoad never fires for the first window
 *   5. background preload after PRELOAD_DELAY_MS: ZFT.runs.init() (compile wasm, load state) so the first click is
 *      instant; failure only logs. The timer is cleared on shutdown.
 *   6. ZFT.harness.maybeRun() (no-op unless extensions.zotero-harness.resultFile is set), not awaited
 *   Every step after 1 is isolated: a failing step is logged and the next one still runs.
 * shutdown(reason)  idempotent; reason APP_SHUTDOWN (2): only `await ZFT.runs.shutdown()` (flush state); otherwise
 *   full teardown in this order: runs.shutdown, menu.unregister, PreferencePanes.unregister(paneID),
 *   removeFromWindow for every tracked window, login.closeAll / progress.closeAll, wasmHost.shutdown,
 *   ZFT.l10n.reset(), delete Zotero.ZoteroFullText, Services.obs.notifyObservers(null, 'startupcache-invalidate') (dev reloads),
 *   started = false. A shutdown that arrives while startup is still running waits for it first.
 * addToWindow(win): WeakSet guard; win.MozXULElement.insertFTLIfNeeded(ZFT.FTL). Windows without MozXULElement are skipped.
 * removeFromWindow(win): win.document.querySelector('[href="zotero-fulltext.ftl"]')?.remove().
 * Failures in one step must not skip the remaining teardown steps (each in its own try/catch + log).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const PRELOAD_DELAY_MS = 2000;
  /** nsIAddonManager / bootstrap reason codes we care about. */
  const REASON = { APP_STARTUP: 1, APP_SHUTDOWN: 2, ADDON_ENABLE: 3, ADDON_DISABLE: 4, ADDON_INSTALL: 5, ADDON_UNINSTALL: 6, ADDON_UPGRADE: 7, ADDON_DOWNGRADE: 8 };
  const REPO_URL = 'https://github.com/ABorakati/zotero-fulltext';
  const PANE_TITLE_ID = 'zotero-fulltext-prefs-title';

  /** Main windows that already have the FTL link (a window is never marked until the insert succeeded). */
  const windows = new WeakSet();
  let startPromise = null;
  let stopPromise = null;
  let preloadTimer = null;
  let tasks = null;

  function getTasks() {
    if (!tasks) tasks = new ZFT.util.TaskSet('plugin');
    return tasks;
  }

  /** Run one lifecycle step; a failure is logged and never propagates. */
  async function step(name, fn) {
    try {
      await fn();
    } catch (e) {
      ZFT.util.log('error', `plugin step "${name}" failed: ${ZFT.util.errText(e)}`);
    }
  }

  function schedulePreload() {
    cancelPreload();
    preloadTimer = setTimeout(() => {
      preloadTimer = null;
      getTasks().spawn('preload', () => ZFT.runs.init());
    }, PRELOAD_DELAY_MS);
  }

  function cancelPreload() {
    if (preloadTimer !== null) {
      clearTimeout(preloadTimer);
      preloadTimer = null;
    }
  }

  async function doStartup(info) {
    plugin.started = true;
    ZFT.info = info || null;
    Zotero.ZoteroFullText = ZFT;
    await Zotero.initializationPromise;

    await step('preference pane', async () => {
      plugin.paneID = await Zotero.PreferencePanes.register({
        pluginID: ZFT.ID,
        src: ZFT.CONTENT_URL + 'prefs.xhtml',
        scripts: [ZFT.CONTENT_URL + 'prefs-pane.js'],
        label: ZFT.l10n.format(PANE_TITLE_ID),
        image: ZFT.CONTENT_URL + 'icons/icon.svg',
        helpURL: REPO_URL,
      });
    });
    await step('menus', () => ZFT.menu.register());
    await step('main windows', () => {
      for (const win of Zotero.getMainWindows()) plugin.addToWindow(win);
    });
    await step('preload schedule', () => schedulePreload());
    getTasks().spawn('harness', () => ZFT.harness.maybeRun());
  }

  async function doShutdown(reason) {
    if (startPromise) {
      try { await startPromise; } catch (e) { /* a failed startup still needs its partial teardown */ }
    }
    cancelPreload();
    // Both paths flush the engine and state first; Zotero tears the rest down itself when the whole app exits.
    await step('runs.shutdown', () => ZFT.runs.shutdown());
    if (reason !== REASON.APP_SHUTDOWN) {
      await step('menu.unregister', () => ZFT.menu.unregister());
      await step('preference pane unregister', () => {
        if (plugin.paneID) Zotero.PreferencePanes.unregister(plugin.paneID);
      });
      await step('remove from windows', () => {
        for (const win of Zotero.getMainWindows()) {
          if (windows.has(win)) plugin.removeFromWindow(win);
        }
      });
      await step('login.closeAll', () => ZFT.login.closeAll());
      await step('progress.closeAll', () => ZFT.progress.closeAll());
      await step('wasmHost.shutdown', () => ZFT.wasmHost.shutdown());
      await step('l10n.reset', () => ZFT.l10n.reset());
      await step('release Zotero.ZoteroFullText', () => { delete Zotero.ZoteroFullText; });
      await step('startupcache-invalidate', () => Services.obs.notifyObservers(null, 'startupcache-invalidate'));
    }
    plugin.paneID = null;
    plugin.started = false;
    startPromise = null;
  }

  const plugin = {
    PRELOAD_DELAY_MS, REASON,
    /** @type {boolean} */
    started: false,
    /** Preference pane id returned by Zotero.PreferencePanes.register, null until registered (menu.js opens it). @type {string|null} */
    paneID: null,
    /** Fire-and-forget tasks of the lifecycle (preload, harness); tests await tasks.whenIdle(). */
    get tasks() { return getTasks(); },
    /** @param {{id: string, version: string, rootURI: string}} info @param {number} [reason] @returns {Promise<void>} */
    startup(info, reason) {
      if (!startPromise) {
        startPromise = doStartup(info, reason).catch((e) => {
          startPromise = null;
          plugin.started = false;
          try { delete Zotero.ZoteroFullText; } catch (err) { /* nothing to release */ }
          throw e;
        });
      }
      return startPromise;
    },
    /** @param {number} [reason] @returns {Promise<void>} */
    shutdown(reason) {
      if (stopPromise) return stopPromise;
      if (!startPromise && !plugin.started) return Promise.resolve();
      stopPromise = doShutdown(reason).then(
        () => { stopPromise = null; },
        (e) => { stopPromise = null; throw e; },
      );
      return stopPromise;
    },
    /** @param {object} win a Zotero main window */
    addToWindow(win) {
      if (!win || windows.has(win)) return;
      const xul = win.MozXULElement;
      if (!xul || typeof xul.insertFTLIfNeeded !== 'function') {
        ZFT.util.log('warn', 'main window without MozXULElement skipped');
        return;
      }
      try {
        xul.insertFTLIfNeeded(ZFT.FTL);
        windows.add(win);
      } catch (e) {
        ZFT.util.log('error', 'could not add the FTL to a window: ' + ZFT.util.errText(e));
      }
    },
    /** @param {object} win */
    removeFromWindow(win) {
      if (!win) return;
      try {
        const link = win.document.querySelector(`[href="${ZFT.FTL}"]`);
        if (link) link.remove();
      } catch (e) {
        ZFT.util.log('warn', 'could not remove the FTL from a window: ' + ZFT.util.errText(e));
      }
      windows.delete(win);
    },
    /** Bootstrap hook {window}. Never throws. */
    onMainWindowLoad(ev) {
      try {
        if (ev && ev.window) plugin.addToWindow(ev.window);
      } catch (e) {
        ZFT.util.log('error', 'onMainWindowLoad failed: ' + ZFT.util.errText(e));
      }
    },
    /** Bootstrap hook {window}. Never throws. */
    onMainWindowUnload(ev) {
      try {
        if (ev && ev.window) plugin.removeFromWindow(ev.window);
      } catch (e) {
        ZFT.util.log('error', 'onMainWindowUnload failed: ' + ZFT.util.errText(e));
      }
    },
  };
  ZFT.plugin = plugin;
})(this);
