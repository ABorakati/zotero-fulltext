// Fakes and a loader for the js-shell tests (config, plugin, menu, harness). Owner: js-shell.
//
// makeShellEnv() loads ONLY the shell modules (util, config, menu, harness, plugin) into a vm context and replaces every
// other module the shell calls (runs, login, progress, wasmHost, items, state ...) with recorders, so these tests do not
// depend on the state of the other work packages. Every recorder appends to `calls` ([name, args]) so tests can assert
// ordering across modules.
import { loadPlugin } from './zft-env.mjs';
import { makeFakeTimers, makeFakeZotero, makeFakeServices } from './fakes.mjs';

/** Modules the shell owns; everything else is stubbed. */
export const SHELL_FILES = ['util.js', 'config.js', 'menu.js', 'harness.js', 'plugin.js'];

/** Zotero.MenuManager fake: registerMenu returns "<pluginID>-<menuID>" (like the real namespaced key), or false for ids in `refuse`. */
export function makeFakeMenuManager(calls, refuse = []) {
  const registered = [];
  return {
    registered,
    registerMenu(def) {
      calls.push(['registerMenu', [def.menuID]]);
      if (refuse.includes(def.menuID)) return false;
      const key = `${def.pluginID}-${def.menuID}`;
      registered.push({ key, def });
      return key;
    },
    unregisterMenu(key) {
      calls.push(['unregisterMenu', [key]]);
      const i = registered.findIndex((r) => r.key === key);
      if (i >= 0) registered.splice(i, 1);
      return i >= 0;
    },
  };
}

/** A fake Zotero main window; `xul: false` models a window without MozXULElement. */
export function makeFakeMainWindow(calls, { xul = true, name = 'win' } = {}) {
  const ftlLinks = [];
  const win = {
    name,
    ftlLinks,
    document: {
      querySelector(sel) {
        const m = sel.match(/^\[href="([^"]+)"\]$/);
        const link = m ? ftlLinks.find((l) => l.href === m[1]) : null;
        return link || null;
      },
    },
  };
  if (xul) {
    win.MozXULElement = {
      insertFTLIfNeeded(href) {
        calls.push(['insertFTL', [name, href]]);
        ftlLinks.push({
          href,
          remove() {
            calls.push(['removeFTL', [name, href]]);
            ftlLinks.splice(ftlLinks.indexOf(this), 1);
          },
        });
      },
    };
  }
  return win;
}

/** A recorder that returns `result` (or runs `impl`) and logs ['name', args] into `calls`. */
export function recorder(calls, name, impl) {
  return (...args) => {
    calls.push([name, args]);
    return impl ? impl(...args) : undefined;
  };
}

/**
 * @param {{zotero?: object, services?: object, globals?: object, windows?: object[], stubs?: boolean, fakeTimers?: boolean,
 *          files?: string[]}} [opts]
 * @returns {{ZFT: object, ctx: object, calls: Array, zotero: object, services: object, timers: object, menuManager: object,
 *            windows: object[], panes: object}}
 */
export function makeShellEnv(opts = {}) {
  const calls = [];
  const timers = makeFakeTimers();
  const windows = opts.windows || [];
  const menuManager = makeFakeMenuManager(calls, opts.refuseMenus || []);
  const panes = {
    registered: [],
    async register(options) {
      calls.push(['PreferencePanes.register', [options]]);
      if (opts.paneRejects) throw new Error('pane register failed');
      panes.registered.push(options);
      return 'pane-' + panes.registered.length;
    },
    unregister(id) {
      calls.push(['PreferencePanes.unregister', [id]]);
    },
  };
  const zotero = makeFakeZotero({
    MenuManager: menuManager,
    PreferencePanes: panes,
    getMainWindows: () => windows,
    getMainWindow: () => windows[0] || null,
    Utilities: { Internal: { openPreferences: recorder(calls, 'openPreferences') } },
    ...(opts.zotero || {}),
  });
  const services = makeFakeServices({
    prompt: { alert: recorder(calls, 'alert'), confirmEx: () => 0, confirm: () => true },
    obs: { notifyObservers: recorder(calls, 'notifyObservers') },
    ...(opts.services || {}),
  });
  const globals = { Zotero: zotero, Services: services, ...(opts.globals || {}) };
  if (opts.fakeTimers !== false) {
    globals.setTimeout = timers.setTimeout;
    globals.clearTimeout = timers.clearTimeout;
  }
  const { ZFT, ctx } = loadPlugin({ globals, files: opts.files || SHELL_FILES });

  if (opts.stubs !== false) {
    // Modules owned by other packages, as recorders. Tests replace single functions to script behaviour.
    ZFT.runs = {
      init: recorder(calls, 'runs.init', async () => {}),
      startRun: recorder(calls, 'runs.startRun', async () => ({ run: 1 })),
      shutdown: recorder(calls, 'runs.shutdown', async () => {}),
      reconfigure: recorder(calls, 'runs.reconfigure', async () => {}),
    };
    ZFT.login = { closeAll: recorder(calls, 'login.closeAll'), warmUp: recorder(calls, 'login.warmUp', async () => {}) };
    ZFT.progress = { closeAll: recorder(calls, 'progress.closeAll'), show: recorder(calls, 'progress.show') };
    ZFT.wasmHost = { shutdown: recorder(calls, 'wasmHost.shutdown', async () => {}) };
    ZFT.items = { menuEligible: recorder(calls, 'items.menuEligible', () => true) };
    const realReset = ZFT.l10n.reset;
    ZFT.l10n.reset = (...a) => { calls.push(['l10n.reset', a]); return realReset(...a); };
  }
  return { ZFT, ctx, calls, zotero, services, timers, menuManager, windows, panes };
}

/** Names of the recorded calls, in order. */
export function callNames(calls) {
  return calls.map((c) => c[0]);
}
