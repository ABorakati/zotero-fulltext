// ZFT.plugin lifecycle (startup/shutdown/windows/preload) and the bootstrap.js wiring, with fakes. The shell modules are the
// real ones; everything else is a recorder (helpers/shell-env.mjs), so this file does not depend on other work packages.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { PLUGIN, CONTENT, makeContext, runScript, plain } from './helpers/zft-env.mjs';
import { makeFakeTimers, makeFakeZotero, makeFakeServices } from './helpers/fakes.mjs';
import { makeShellEnv, makeFakeMainWindow, recorder, callNames } from './helpers/shell-env.mjs';

const REASON = { APP_SHUTDOWN: 2, ADDON_DISABLE: 4 };
const INFO = { id: 'zotero-fulltext@aborakati', version: '0.1.0', rootURI: 'jar:file:///x.xpi!/' };

/** A fake main window that is already open (Zotero.getMainWindows() returns it) and records into env.calls. */
function addWindow(env, name = 'win') {
  const win = makeFakeMainWindow(env.calls, { name });
  env.windows.push(win);
  return win;
}

/** Let queued microtasks (and the TaskSet's .then chains) run. */
async function flush(n = 10) {
  for (let i = 0; i < n; i++) await Promise.resolve();
}

test('startup registers the pane, the menus and every open main window exactly once, even when called twice', async () => {
  const env = makeShellEnv();
  const win1 = addWindow(env, 'w1');
  const win2 = addWindow(env, 'w2');
  const { ZFT, calls: c } = env;
  const a = ZFT.plugin.startup(INFO, 1);
  const b = ZFT.plugin.startup(INFO, 1);
  assert.strictEqual(a, b, 'concurrent calls share one promise');
  await a;
  await ZFT.plugin.startup(INFO, 1);
  assert.equal(c.filter((x) => x[0] === 'PreferencePanes.register').length, 1);
  assert.equal(c.filter((x) => x[0] === 'registerMenu').length, 2, 'item menu + tools menu, once');
  assert.equal(env.windows.length, 2);
  assert.equal(win1.ftlLinks.length, 1);
  assert.equal(win2.ftlLinks.length, 1);
  assert.equal(ZFT.plugin.started, true);
  assert.equal(ZFT.info.version, '0.1.0');
  assert.strictEqual(env.zotero.ZoteroFullText, ZFT, 'handle for the pane script and the harness');
});

test('the preference pane is registered with the documented arguments and its id is kept for the menu', async () => {
  const env = makeShellEnv();
  await env.ZFT.plugin.startup(INFO, 1);
  assert.equal(env.panes.registered.length, 1);
  const opts = plain(env.panes.registered[0]);
  assert.deepEqual(opts, {
    pluginID: 'zotero-fulltext@aborakati',
    src: 'chrome://zotero-fulltext/content/prefs.xhtml',
    scripts: ['chrome://zotero-fulltext/content/prefs-pane.js'],
    label: 'zotero-fulltext-prefs-title',   // the l10n fallback (no Localization in the test) is the id; Zotero gets raw text
    image: 'chrome://zotero-fulltext/content/icons/icon.svg',
    helpURL: 'https://github.com/ABorakati/zotero-fulltext',
  });
  assert.equal(env.ZFT.plugin.paneID, 'pane-1');
  assert.ok(fs.existsSync(path.join(CONTENT, 'icons', 'icon.svg')));
});

test('a rejected pane registration is logged; menus and windows still work', async () => {
  const env = makeShellEnv({ paneRejects: true });
  const win = addWindow(env);
  await env.ZFT.plugin.startup(INFO, 1);
  assert.equal(env.ZFT.plugin.paneID, null);
  assert.equal(env.menuManager.registered.length, 2);
  assert.equal(win.ftlLinks.length, 1);
  assert.ok(env.zotero.debugLog.some(([, m]) => /preference pane.*failed.*pane register failed/.test(m)));
});

test('a failing initializationPromise rejects startup, leaves nothing behind, and a later startup can succeed', async () => {
  const env = makeShellEnv();
  env.zotero.initializationPromise = Promise.reject(new Error('zotero died'));
  await assert.rejects(env.ZFT.plugin.startup(INFO, 1), /zotero died/);
  assert.equal(env.ZFT.plugin.started, false);
  assert.equal(env.zotero.ZoteroFullText, undefined);
  assert.equal(env.panes.registered.length, 0);
  env.zotero.initializationPromise = Promise.resolve();
  await env.ZFT.plugin.startup(INFO, 1);
  assert.equal(env.ZFT.plugin.started, true);
  assert.equal(env.panes.registered.length, 1);
});

test('addToWindow: WeakSet guard; windows without MozXULElement are skipped with a log; a throwing insert is not marked', () => {
  const env = makeShellEnv();
  const { ZFT, calls } = env;
  const win = makeFakeMainWindow(calls);
  ZFT.plugin.addToWindow(win);
  ZFT.plugin.addToWindow(win);
  assert.equal(win.ftlLinks.length, 1);
  const bare = makeFakeMainWindow(calls, { xul: false });
  assert.doesNotThrow(() => ZFT.plugin.addToWindow(bare));
  assert.ok(env.zotero.debugLog.some(([, m]) => /without MozXULElement/.test(m)));
  const flaky = makeFakeMainWindow(calls);
  let first = true;
  const real = flaky.MozXULElement.insertFTLIfNeeded;
  flaky.MozXULElement.insertFTLIfNeeded = function (...a) { if (first) { first = false; throw new Error('not ready'); } return real.apply(this, a); };
  ZFT.plugin.addToWindow(flaky);
  assert.equal(flaky.ftlLinks.length, 0);
  ZFT.plugin.addToWindow(flaky);
  assert.equal(flaky.ftlLinks.length, 1, 'a failed insert is retried, not remembered');
  assert.doesNotThrow(() => ZFT.plugin.addToWindow(null));
});

test('onMainWindowLoad after startup does not double-insert the FTL; unload removes the link and allows a re-add', async () => {
  const env = makeShellEnv();
  const win = addWindow(env);
  const { ZFT } = env;
  await ZFT.plugin.startup(INFO, 1);
  assert.equal(win.ftlLinks.length, 1);
  ZFT.plugin.onMainWindowLoad({ window: win });
  assert.equal(win.ftlLinks.length, 1, 'no double insert');
  const late = makeFakeMainWindow(env.calls, { name: 'late' });
  ZFT.plugin.onMainWindowLoad({ window: late });
  assert.equal(late.ftlLinks.length, 1);
  ZFT.plugin.onMainWindowUnload({ window: late });
  assert.equal(late.ftlLinks.length, 0);
  ZFT.plugin.onMainWindowLoad({ window: late });
  assert.equal(late.ftlLinks.length, 1, 're-added after unload');
  assert.doesNotThrow(() => ZFT.plugin.onMainWindowLoad());
  assert.doesNotThrow(() => ZFT.plugin.onMainWindowUnload({}));
});

test('startup schedules one preload after PRELOAD_DELAY_MS; a failing preload is swallowed and logged', async () => {
  const env = makeShellEnv();
  const { ZFT, timers } = env;
  ZFT.runs.init = recorder(env.calls, 'runs.init', async () => { throw new Error('wasm missing'); });
  await ZFT.plugin.startup(INFO, 1);
  assert.equal(ZFT.plugin.PRELOAD_DELAY_MS, 2000);
  assert.equal(timers.pendingCount(), 1);
  timers.advance(1999);
  await flush();
  assert.equal(callNames(env.calls).includes('runs.init'), false, 'not before the delay');
  timers.advance(1);
  await ZFT.plugin.tasks.whenIdle();
  assert.equal(env.calls.filter((x) => x[0] === 'runs.init').length, 1);
  assert.ok(env.zotero.debugLog.some(([, m]) => /preload failed.*wasm missing/.test(m)));
  assert.equal(timers.pendingCount(), 0);
});

test('a synchronous throw from the preload is swallowed too, and shutdown cancels a pending preload', async () => {
  const env = makeShellEnv();
  const { ZFT, timers } = env;
  ZFT.runs.init = () => { throw new Error('sync boom'); };
  await ZFT.plugin.startup(INFO, 1);
  timers.advance(2000);
  await ZFT.plugin.tasks.whenIdle();
  assert.ok(env.zotero.debugLog.some(([, m]) => /preload failed.*sync boom/.test(m)));
  const env2 = makeShellEnv();
  await env2.ZFT.plugin.startup(INFO, 1);
  assert.equal(env2.timers.pendingCount(), 1);
  await env2.ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  assert.equal(env2.timers.pendingCount(), 0, 'the preload timer is cleared');
});

test('startup runs the harness hook (dormant here) without waiting for it', async () => {
  const env = makeShellEnv();
  const { ZFT } = env;
  let release;
  const gate = new Promise((r) => { release = r; });
  ZFT.harness.maybeRun = recorder(env.calls, 'harness.maybeRun', () => gate);
  await ZFT.plugin.startup(INFO, 1);   // resolves although maybeRun is still pending
  await flush();
  assert.equal(env.calls.filter((x) => x[0] === 'harness.maybeRun').length, 1);
  release();
  await ZFT.plugin.tasks.whenIdle();
});

test('full shutdown tears down in the documented order and is safe to repeat', async () => {
  const env = makeShellEnv();
  const win = addWindow(env);
  const { ZFT } = env;
  await ZFT.plugin.startup(INFO, 1);
  env.calls.length = 0;
  await ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  const order = callNames(env.calls).filter((n) => !['registerMenu'].includes(n));
  assert.deepEqual(order, [
    'runs.shutdown',
    'unregisterMenu', 'unregisterMenu',
    'PreferencePanes.unregister',
    'removeFTL',
    'login.closeAll', 'progress.closeAll', 'wasmHost.shutdown',
    'l10n.reset',
    'notifyObservers',
  ]);
  assert.deepEqual(plain(env.calls.find((x) => x[0] === 'notifyObservers')[1]), [null, 'startupcache-invalidate']);
  assert.deepEqual(plain(env.calls.find((x) => x[0] === 'PreferencePanes.unregister')[1]), ['pane-1']);
  assert.deepEqual(plain(env.calls.filter((x) => x[0] === 'unregisterMenu').map((x) => x[1][0])), [
    'zotero-fulltext@aborakati-zft-item-menu', 'zotero-fulltext@aborakati-zft-tools-menu',
  ]);
  assert.equal(win.ftlLinks.length, 0);
  assert.equal(ZFT.plugin.started, false);
  assert.equal(env.zotero.ZoteroFullText, undefined, 'the handle is released');
  assert.equal(ZFT.plugin.paneID, null);
  env.calls.length = 0;
  await ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  await ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  assert.deepEqual(env.calls, [], 'a repeated shutdown does nothing');
});

test('shutdown before any startup is a no-op', async () => {
  const env = makeShellEnv();
  await env.ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  assert.deepEqual(env.calls, []);
});

test('APP_SHUTDOWN only flushes through runs.shutdown: nothing is unregistered or removed', async () => {
  const env = makeShellEnv();
  const win = addWindow(env);
  const { ZFT } = env;
  await ZFT.plugin.startup(INFO, 1);
  env.calls.length = 0;
  await ZFT.plugin.shutdown(REASON.APP_SHUTDOWN);
  assert.deepEqual(callNames(env.calls), ['runs.shutdown']);
  assert.equal(win.ftlLinks.length, 1, 'the window keeps its FTL link');
  assert.strictEqual(env.zotero.ZoteroFullText, ZFT, 'the app is quitting; nothing else is torn down');
  assert.equal(ZFT.plugin.started, false);
  await ZFT.plugin.shutdown(REASON.APP_SHUTDOWN);
  assert.deepEqual(callNames(env.calls), ['runs.shutdown'], 'idempotent');
});

test('APP_SHUTDOWN with a failing runs.shutdown still resolves', async () => {
  const env = makeShellEnv();
  env.ZFT.runs.shutdown = async () => { throw new Error('flush failed'); };
  await env.ZFT.plugin.startup(INFO, 1);
  await env.ZFT.plugin.shutdown(REASON.APP_SHUTDOWN);
  assert.equal(env.ZFT.plugin.started, false);
  assert.ok(env.zotero.debugLog.some(([, m]) => /runs\.shutdown.*flush failed/.test(m)));
});

test('a throwing teardown step does not stop the later steps', async () => {
  const env = makeShellEnv();
  const win = addWindow(env);
  const { ZFT } = env;
  await ZFT.plugin.startup(INFO, 1);
  env.calls.length = 0;
  ZFT.runs.shutdown = async () => { env.calls.push(['runs.shutdown', []]); throw new Error('runs exploded'); };
  ZFT.menu.unregister = () => { env.calls.push(['menu.unregister', []]); throw new Error('menu exploded'); };
  env.zotero.PreferencePanes.unregister = () => { env.calls.push(['PreferencePanes.unregister', []]); throw new Error('pane exploded'); };
  ZFT.login.closeAll = () => { env.calls.push(['login.closeAll', []]); throw new Error('login exploded'); };
  ZFT.wasmHost.shutdown = async () => { env.calls.push(['wasmHost.shutdown', []]); throw new Error('wasm exploded'); };
  await ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  assert.deepEqual(callNames(env.calls), [
    'runs.shutdown', 'menu.unregister', 'PreferencePanes.unregister', 'removeFTL', 'login.closeAll', 'progress.closeAll',
    'wasmHost.shutdown', 'l10n.reset', 'notifyObservers',
  ]);
  assert.equal(win.ftlLinks.length, 0);
  assert.equal(ZFT.plugin.started, false);
  for (const what of ['runs exploded', 'menu exploded', 'pane exploded', 'login exploded', 'wasm exploded']) {
    assert.ok(env.zotero.debugLog.some(([, m]) => m.includes(what)), what);
  }
});

test('a shutdown that arrives during startup waits for it, then tears everything down', async () => {
  let release;
  const gate = new Promise((r) => { release = r; });
  const env = makeShellEnv({ zotero: { initializationPromise: gate } });
  const starting = env.ZFT.plugin.startup(INFO, 1);
  const stopping = env.ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  await flush();
  assert.equal(env.panes.registered.length, 0, 'startup is still waiting for Zotero');
  release();
  await starting;
  await stopping;
  assert.equal(env.ZFT.plugin.started, false);
  assert.equal(env.menuManager.registered.length, 0, 'menus registered by the startup were unregistered');
  assert.ok(callNames(env.calls).includes('PreferencePanes.unregister'));
});

test('disable then enable in the same process starts again cleanly', async () => {
  const env = makeShellEnv();
  const win = addWindow(env);
  const { ZFT } = env;
  await ZFT.plugin.startup(INFO, 1);
  await ZFT.plugin.shutdown(REASON.ADDON_DISABLE);
  await ZFT.plugin.startup(INFO, 3);
  assert.equal(env.panes.registered.length, 2);
  assert.equal(env.menuManager.registered.length, 2);
  assert.equal(win.ftlLinks.length, 1);
  assert.equal(ZFT.plugin.started, true);
});

// --- bootstrap.js wiring ---------------------------------------------------------------------------------------------

/**
 * Run the REAL bootstrap.js in a vm context (its top level declares startup/shutdown/... as globals, exactly like Zotero's
 * bootstrap sandbox) with fakes for Cc/Ci/Services.scriptloader that load the real content scripts.
 */
function loadBootstrap() {
  const timers = makeFakeTimers();
  const zotero = makeFakeZotero({
    MenuManager: { registerMenu: (d) => `${d.pluginID}-${d.menuID}`, unregisterMenu: () => true },
    PreferencePanes: { register: async () => 'pane-x', unregister() {} },
    logError(e) { zotero.debugLog.push([1, 'ERROR ' + (e && e.message)]); },
  });
  const chrome = { registered: [], destructed: 0 };
  const services = makeFakeServices({
    io: { newURI: (spec) => ({ spec }) },
  });
  const cc = {
    '@mozilla.org/addons/addon-manager-startup;1': {
      getService: () => ({
        registerChrome(uri, manifest) {
          chrome.registered.push({ uri: uri.spec, manifest });
          return { destruct() { chrome.destructed++; } };
        },
      }),
    },
  };
  const ctx = makeContext({
    Zotero: zotero, Services: services, Cc: cc, Ci: { amIAddonManagerStartup: 'amIAddonManagerStartup' },
    setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout,
  });
  const loaded = [];
  services.scriptloader.loadSubScript = (url, scope) => {
    assert.strictEqual(scope, vm.runInContext('this', ctx), 'scripts load into the bootstrap global itself');
    assert.ok(url.startsWith('chrome://zotero-fulltext/content/'), url);
    const rel = url.slice('chrome://zotero-fulltext/content/'.length);
    loaded.push(rel);
    runScript(ctx, rel);
  };
  vm.runInContext(fs.readFileSync(path.join(PLUGIN, 'bootstrap.js'), 'utf8'), ctx, { filename: 'bootstrap.js' });
  const call = (fn, ...args) => vm.runInContext(`(function (fn, args) { return globalThis[fn].apply(this, args); })`, ctx).call(vm.runInContext('this', ctx), fn, args);
  return { ctx, zotero, chrome, loaded, call, timers };
}

test('bootstrap.js registers the chrome package, loads namespace.js + LOAD_ORDER, starts the plugin, and shuts down cleanly', async () => {
  const b = loadBootstrap();
  await b.call('startup', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, 5);
  assert.deepEqual(plain(b.chrome.registered), [{
    uri: 'file:///plugin/manifest.json',
    manifest: [['content', 'zotero-fulltext', 'file:///plugin/content/']],
  }]);
  const order = plain(b.ctx.ZFT.LOAD_ORDER);
  assert.deepEqual(b.loaded, ['namespace.js', ...order], 'namespace first, then LOAD_ORDER in order');
  assert.equal(b.ctx.ZFT.plugin.started, true);
  assert.strictEqual(b.zotero.ZoteroFullText, b.ctx.ZFT);

  // A second startup (Zotero never does this, but the sandbox is reused on re-enable) is a no-op.
  await b.call('startup', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, 5);
  assert.equal(b.chrome.registered.length, 1);

  // Replace the teardown targets owned by other packages so this test only checks the shell wiring.
  for (const [mod, fn] of [['runs', 'shutdown'], ['login', 'closeAll'], ['progress', 'closeAll'], ['wasmHost', 'shutdown']]) {
    b.ctx.ZFT[mod][fn] = async () => {};
  }
  const zft = b.ctx.ZFT;
  await b.call('shutdown', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, REASON.ADDON_DISABLE);
  assert.equal(zft.plugin.started, false);
  assert.equal(b.chrome.destructed, 1, 'chrome handle destructed');
  assert.equal(b.ctx.ZFT, undefined, 'the namespace is dropped so a re-enable starts clean');
  // Hooks after shutdown are harmless.
  assert.doesNotThrow(() => b.call('onMainWindowLoad', { window: {} }));
  assert.doesNotThrow(() => b.call('onMainWindowUnload', { window: {} }));
  assert.doesNotThrow(() => b.call('install'));
  assert.doesNotThrow(() => b.call('uninstall'));
});

test('bootstrap.js forwards the main-window hooks to ZFT.plugin', async () => {
  const b = loadBootstrap();
  await b.call('startup', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, 5);
  const seen = [];
  b.ctx.ZFT.plugin.onMainWindowLoad = (ev) => seen.push(['load', ev.window.name]);
  b.ctx.ZFT.plugin.onMainWindowUnload = (ev) => seen.push(['unload', ev.window.name]);
  b.call('onMainWindowLoad', { window: { name: 'w' } });
  b.call('onMainWindowUnload', { window: { name: 'w' } });
  assert.deepEqual(seen, [['load', 'w'], ['unload', 'w']]);
});

test('bootstrap.js: a failing startup resets so it can be retried, and logs the error', async () => {
  const b = loadBootstrap();
  b.zotero.initializationPromise = Promise.reject(new Error('no zotero'));
  await assert.rejects(b.call('startup', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, 5), /no zotero/);
  assert.ok(b.zotero.debugLog.some(([, m]) => /ERROR no zotero/.test(m)));
  b.zotero.initializationPromise = Promise.resolve();
  await b.call('startup', { id: INFO.id, version: '0.1.0', rootURI: 'file:///plugin/' }, 5);
  assert.equal(b.chrome.registered.length, 1);
});
