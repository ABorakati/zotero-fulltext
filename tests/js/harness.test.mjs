// ZFT.harness: dormant without the pref; with it, runs the checks, writes the documented JSON atomically and quits.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, makeContext, runScript, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { makeShellEnv, SHELL_FILES } from './helpers/shell-env.mjs';
import { makePaneDocument, makePaneWindow } from './helpers/pane-dom.mjs';

const RESULT = 'C:/tmp/harness/result.json';
const RESULT_PREF = 'extensions.zotero-harness.resultFile';
const QUIT_PREF = 'extensions.zotero-harness.quitWhenDone';
const ENDPOINT_PREF = 'extensions.zotero-fulltext.harness.endpoint_map';

/** Shell modules + recorders for IOUtils / Services.startup. `prefs` are set on the fake Zotero.Prefs. */
function setup(prefs = {}, extraGlobals = {}) {
  const writes = [];
  const quits = [];
  const order = [];
  const env = makeShellEnv({
    fakeTimers: false,
    services: { startup: { quit: (flags) => { quits.push(flags); order.push('quit'); } } },
    globals: {
      IOUtils: {
        writeUTF8: async (p, text, opts) => { writes.push({ path: p, text, opts }); order.push('write'); },
      },
      Ci: { nsIAppStartup: { eAttemptQuit: 2 } },
      ...extraGlobals,
    },
  });
  for (const [k, v] of Object.entries(prefs)) env.zotero.Prefs.set(k, v);
  env.writes = writes;
  env.quits = quits;
  env.order = order;
  env.ZFT.info = { id: 'zotero-fulltext@aborakati', version: '9.9.9', rootURI: 'x' };
  return env;
}

test('dormant: without the pref nothing is run, written or quit', async () => {
  for (const value of [undefined, '', '   ', 42, false]) {
    const env = setup(value === undefined ? {} : { [RESULT_PREF]: value });
    let ran = 0;
    env.ZFT.harness.CHECKS.modules = async () => { ran++; };
    assert.strictEqual(await env.ZFT.harness.maybeRun(), null, String(value));
    assert.equal(ran, 0);
    assert.deepEqual(env.writes, []);
    assert.deepEqual(env.quits, []);
  }
});

test('dormant: the environment variable alone does not activate it (only the throwaway profile pref does)', async () => {
  process.env.ZOTERO_HARNESS_RESULT_FILE = RESULT;
  try {
    const env = setup();
    assert.strictEqual(await env.ZFT.harness.maybeRun(), null);
    assert.deepEqual(env.writes, []);
  } finally {
    delete process.env.ZOTERO_HARNESS_RESULT_FILE;
  }
});

test('with the pref: runs every check, writes the documented JSON with a tmpPath, then quits', async () => {
  const env = setup({ [RESULT_PREF]: RESULT });
  const { harness } = env.ZFT;
  for (const name of Object.keys(harness.CHECKS)) harness.CHECKS[name] = async () => ({ name });
  const result = await harness.maybeRun();
  assert.deepEqual(Object.keys(result).sort(), ['checks', 'durationMs', 'ok', 'plugin', 'startedMs', 'version']);
  assert.equal(result.plugin, 'zotero-fulltext');
  assert.equal(result.version, '9.9.9');
  assert.equal(result.ok, true);
  assert.deepEqual(Object.keys(result.checks), ['modules', 'wasm', 'prefs', 'menu', 'menu_ui', 'prefpane', 'prefpane_ui', 'ftl', 'windows']);
  assert.deepEqual(plain(result.checks.wasm), { ok: true, detail: { name: 'wasm' } });
  assert.ok(Number.isInteger(result.startedMs) && result.startedMs > 1.7e12, 'wall-clock ms');
  assert.ok(Number.isInteger(result.durationMs) && result.durationMs >= 0);

  assert.equal(env.writes.length, 1);
  assert.equal(env.writes[0].path, RESULT);
  assert.deepEqual(plain(env.writes[0].opts), { tmpPath: RESULT + '.tmp' });
  assert.deepEqual(JSON.parse(env.writes[0].text), plain(result));
  assert.deepEqual(env.quits, [2], 'Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit)');
  assert.deepEqual(env.order, ['write', 'quit'], 'the result is on disk before Zotero quits');
});

test('one failing check is recorded and the result is still written (ok=false)', async () => {
  const env = setup({ [RESULT_PREF]: RESULT });
  const { harness } = env.ZFT;
  for (const name of Object.keys(harness.CHECKS)) harness.CHECKS[name] = async () => 'fine';
  harness.CHECKS.wasm = async () => { throw new TypeError('wasm exploded'); };
  harness.CHECKS.menu = () => { throw new Error('sync explosion'); };
  harness.CHECKS.ftl = () => 'not a promise';
  const result = await harness.maybeRun();
  assert.equal(result.ok, false);
  assert.deepEqual(plain(result.checks.wasm), { ok: false, detail: 'TypeError: wasm exploded' });
  assert.deepEqual(plain(result.checks.menu), { ok: false, detail: 'Error: sync explosion' });
  assert.deepEqual(plain(result.checks.ftl), { ok: true, detail: 'not a promise' });
  assert.equal(result.checks.modules.ok, true, 'checks after a failure still run');
  assert.equal(result.checks.windows.ok, true);
  assert.equal(env.writes.length, 1);
  assert.equal(JSON.parse(env.writes[0].text).checks.wasm.ok, false);
  assert.equal(env.quits.length, 1);
});

test('quitWhenDone=false keeps Zotero running; anything else quits', async () => {
  for (const [value, expected] of [[false, 0], [true, 1], [undefined, 1]]) {
    const env = setup(value === undefined ? { [RESULT_PREF]: RESULT } : { [RESULT_PREF]: RESULT, [QUIT_PREF]: value });
    for (const name of Object.keys(env.ZFT.harness.CHECKS)) env.ZFT.harness.CHECKS[name] = async () => null;
    await env.ZFT.harness.maybeRun();
    assert.equal(env.writes.length, 1);
    assert.equal(env.quits.length, expected, `quitWhenDone=${value}`);
  }
});

test('a failing write or quit is logged; maybeRun still returns the result and never rejects', async () => {
  const env = setup({ [RESULT_PREF]: RESULT }, {
    IOUtils: { writeUTF8: async () => { throw new Error('disk full'); } },
  });
  env.services.startup.quit = () => { throw new Error('already quitting'); };
  for (const name of Object.keys(env.ZFT.harness.CHECKS)) env.ZFT.harness.CHECKS[name] = async () => null;
  const result = await env.ZFT.harness.maybeRun();
  assert.equal(result.plugin, 'zotero-fulltext');
  assert.ok(env.zotero.debugLog.some(([, m]) => /could not be written.*disk full/.test(m)));
  assert.ok(env.zotero.debugLog.some(([, m]) => /quit failed.*already quitting/.test(m)));
});

test('it waits for Zotero.uiReadyPromise (bounded) before running the checks', async () => {
  const env = setup({ [RESULT_PREF]: RESULT });
  let release;
  env.zotero.uiReadyPromise = new Promise((r) => { release = r; });
  let ran = false;
  for (const name of Object.keys(env.ZFT.harness.CHECKS)) env.ZFT.harness.CHECKS[name] = async () => { ran = true; };
  const p = env.ZFT.harness.maybeRun();
  await new Promise((r) => setTimeout(r, 20));
  assert.equal(ran, false, 'still waiting for the UI');
  release();
  await p;
  assert.equal(ran, true);
});

// --- endpointMap -----------------------------------------------------------------------------------------------------

test('endpointMap parses a JSON array of 2-string arrays and returns [] for anything else', () => {
  const cases = [
    [undefined, []],
    ['', []],
    ['not json', []],
    ['{"a":"b"}', []],
    ['"x"', []],
    ['[]', []],
    ['[["https://api.openalex.org","http://127.0.0.1:9/oa"]]', [['https://api.openalex.org', 'http://127.0.0.1:9/oa']]],
    ['[["a","b"],["c","d"]]', [['a', 'b'], ['c', 'd']]],
    ['[["a"]]', []],
    ['[["a","b","c"]]', []],
    ['[["a",2]]', []],
    ['[["a",""]]', []],
    ['[["a","b"], "oops"]', []],
    ['[["a","b"], null]', []],
    [123, []],
  ];
  for (const [raw, expected] of cases) {
    const env = setup(raw === undefined ? {} : { [ENDPOINT_PREF]: raw });
    assert.deepEqual(plain(env.ZFT.harness.endpointMap()), expected, String(raw));
  }
});

test('endpointMap never throws when the prefs service does', () => {
  const env = setup();
  env.zotero.Prefs.get = () => { throw new Error('prefs unavailable'); };
  assert.deepEqual(plain(env.ZFT.harness.endpointMap()), []);
});

// --- the individual checks (real ones, with fakes) -------------------------------------------------------------------

/** Modules owned by other packages get synthetic stand-ins so the check logic is tested on its own. */
function withSyntheticModules(ZFT) {
  const shell = new Set(['util', 'l10n', 'config', 'menu', 'harness', 'plugin']);
  for (const [mod, names] of Object.entries(ZFT.harness.MODULES)) {
    if (!shell.has(mod)) ZFT[mod] = Object.fromEntries(names.map((n) => [n, true]));
  }
}

test('modules: passes when every declared name is present, names what is missing otherwise', async () => {
  const { ZFT } = setup();
  withSyntheticModules(ZFT);
  const detail = await ZFT.harness.CHECKS.modules();
  assert.equal(detail.modules, Object.keys(plain(ZFT.harness.MODULES)).length);
  assert.ok(detail.modules >= 20);
  delete ZFT.net.buildHeaders;
  ZFT.runs = undefined;
  await assert.rejects(ZFT.harness.CHECKS.modules(), /^Error: missing: net.buildHeaders, runs$/);
});

test('modules: the harness table lists every module of the API manifest and only names the manifest declares', async () => {
  const { API } = await import('./helpers/api-manifest.mjs');
  const { ZFT } = setup();
  const table = plain(ZFT.harness.MODULES);
  assert.deepEqual(Object.keys(API).filter((m) => !(m in table)), [], 'a module missing from harness.MODULES');
  assert.deepEqual(Object.keys(table).filter((m) => !(m in API)), [], 'a harness module the manifest does not know');
  for (const [mod, names] of Object.entries(table)) {
    assert.deepEqual(names.filter((n) => !API[mod].includes(n)), [], `${mod}: names the manifest does not declare`);
  }
});

test('prefs: every DEFAULTS key must be registered with its declared type', async () => {
  const env = setup();
  const detail = await env.ZFT.harness.CHECKS.prefs();
  assert.equal(detail.prefs, Object.keys(plain(env.ZFT.config.DEFAULTS)).length);
  env.zotero.Prefs.clear('extensions.zotero-fulltext.run.hedge');
  env.zotero.Prefs.set('extensions.zotero-fulltext.inst.batch_max', '20');
  await assert.rejects(env.ZFT.harness.CHECKS.prefs(), /inst\.batch_max \(string, want int\).*run\.hedge \(missing, want bool\)/);
});

test('menu: needs the registration keys of both menus', async () => {
  const env = setup();
  await assert.rejects(env.ZFT.harness.CHECKS.menu(), /menus not registered/);
  env.ZFT.menu.register();
  const detail = await env.ZFT.harness.CHECKS.menu();
  assert.equal(detail.keys.length, 2);
});

test('prefpane: needs the pane id from PreferencePanes.register', async () => {
  const env = setup();
  await assert.rejects(env.ZFT.harness.CHECKS.prefpane(), /no preference pane id/);
  await env.ZFT.plugin.startup({ id: 'x', version: '1', rootURI: 'y' }, 1);
  assert.deepEqual(plain(await env.ZFT.harness.CHECKS.prefpane()), { pane: 'pane-1' });
});

test('ftl: a value message must resolve to text (the bare id means Fluent did not load the file)', async () => {
  const noFluent = setup();
  await assert.rejects(noFluent.ZFT.harness.CHECKS.ftl(), /did not resolve: zotero-fulltext-prefs-title/);
  class FakeLocalization {
    formatValueSync(id) { return id === 'zotero-fulltext-prefs-title' ? 'Full Text' : null; }
  }
  const withFluent = setup({}, { Localization: FakeLocalization });
  assert.deepEqual(plain(await withFluent.ZFT.harness.CHECKS.ftl()), { text: 'Full Text' });
});

/** A fake wasm host with the Host surface the check uses. */
function fakeHost(overrides = {}) {
  const calls = [];
  const host = {
    calls,
    diagnostics: () => ({ abi: 1, pure: 'main', engineTraps: 0, pureTraps: 0, lastPanic: '' }),
    inspect: async (meta, bytes) => { calls.push(['inspect', meta, bytes.length]); return { expect: 'page', class: { kind: 'landing' } }; },
    step: (input) => { calls.push(['step', input]); return { effects: [{ type: 'log' }], next_timer: null, more: false, ignored: [], stats: {} }; },
    dispose: async () => { calls.push(['dispose']); },
    ...overrides,
  };
  return host;
}

test('wasm: pure inspect on the shared host, Configure on a PRIVATE host that is disposed afterwards', async () => {
  const env = setup();
  const shared = fakeHost();
  const probe = fakeHost();
  let loadOpts = null;
  env.ZFT.wasmHost = { ABI_VERSION: 1, get: async () => shared, load: async (opts) => { loadOpts = opts; return probe; } };
  const detail = await env.ZFT.harness.CHECKS.wasm();
  assert.deepEqual(plain(detail), { abi: 1, inspect: 'page', configure_effects: 1, pure: 'main' });
  assert.deepEqual(plain(loadOpts), { inspectInWorker: false });
  assert.deepEqual(shared.calls.map((c) => c[0]), ['inspect'], 'the shared (run) engine is never stepped');
  assert.equal(shared.calls[0][1].spec.expect, 'page');
  assert.equal(shared.calls[0][1].resp.status, 200);
  assert.equal(shared.calls[0][1].resp.body_len, shared.calls[0][2]);
  const step = probe.calls.find((c) => c[0] === 'step')[1];
  assert.equal(step.commands.length, 1);
  assert.equal(step.commands[0].type, 'configure');
  assert.equal(step.commands[0].protocol, 1);
  assert.deepEqual(plain(step.commands[0].config), plain(env.ZFT.config.buildEngineConfig()));
  assert.deepEqual(plain(step.commands[0].persisted), {});
  assert.ok(Number.isInteger(step.now) && Number.isInteger(step.wall));
  assert.deepEqual(probe.calls.map((c) => c[0]), ['step', 'dispose']);
});

test('wasm: an ABI mismatch, a junk inspection or a junk StepOut fails the check; the private host is still disposed', async () => {
  const env = setup();
  const make = (shared, probe) => {
    env.ZFT.wasmHost = { ABI_VERSION: 1, get: async () => shared, load: async () => probe };
  };
  make(fakeHost({ diagnostics: () => ({ abi: 2, pure: 'main' }) }), fakeHost());
  await assert.rejects(env.ZFT.harness.CHECKS.wasm(), /abi 2 != 1/);
  make(fakeHost({ inspect: async () => ({ nope: true }) }), fakeHost());
  await assert.rejects(env.ZFT.harness.CHECKS.wasm(), /no Inspection/);
  const badProbe = fakeHost({ step: () => ({ nothing: true }) });
  make(fakeHost(), badProbe);
  await assert.rejects(env.ZFT.harness.CHECKS.wasm(), /no StepOut/);
  assert.deepEqual(badProbe.calls.map((c) => c[0]), ['dispose']);
  const trapProbe = fakeHost({ step: () => { throw new Error('EngineTrap'); } });
  make(fakeHost(), trapProbe);
  await assert.rejects(env.ZFT.harness.CHECKS.wasm(), /EngineTrap/);
  assert.deepEqual(trapProbe.calls.map((c) => c[0]), ['dispose']);
  env.ZFT.wasmHost = { ABI_VERSION: 1, get: async () => { throw new Error('wasm file missing'); } };
  await assert.rejects(env.ZFT.harness.CHECKS.wasm(), /wasm file missing/);
});

/** Services.ww / Services.console / Ci / ChromeUtils fakes for the windows check. */
function windowsEnv({ errorsFor = {}, loadFails = [] } = {}) {
  const opened = [];
  const listeners = new Set();
  class ScriptError {}
  const Ci = { nsIAppStartup: { eAttemptQuit: 2 }, nsIScriptError: Object.assign(ScriptError, { warningFlag: 1 }) };
  const env = setup({}, {
    Ci,
    ChromeUtils: { generateQI: (ifaces) => ifaces },
  });
  env.ctx.Services.ww = {
    openWindow(parent, url, name, features, arg) {
      const win = {
        url, features, arg, closed: false,
        document: { readyState: 'complete', documentElement: { localName: loadFails.includes(url) ? 'parsererror' : 'window' } },
        addEventListener() {},
        close() { win.closed = true; },
      };
      opened.push(win);
      // Script errors logged while the window loads, attributed to a source file.
      for (const err of errorsFor[url] || []) {
        const e = Object.assign(new ScriptError(), { flags: err.flags || 0, sourceName: err.source, errorMessage: err.message });
        for (const l of [...listeners]) l.observe(e);
      }
      return win;
    },
  };
  env.ctx.Services.console = {
    registerListener: (l) => listeners.add(l),
    unregisterListener: (l) => listeners.delete(l),
  };
  env.opened = opened;
  env.listeners = listeners;
  env.ZFT.progress.WINDOW_URL = 'chrome://zotero-fulltext/content/progress.xhtml';
  env.ZFT.login.WINDOW_URL = 'chrome://zotero-fulltext/content/login.xhtml';
  return env;
}

test('windows: progress.xhtml and login.xhtml open, load and close; listeners are removed', async () => {
  const env = windowsEnv();
  const detail = await env.ZFT.harness.CHECKS.windows();
  assert.deepEqual(plain(detail), { progress: { errors: [] }, login: { errors: [] } });
  assert.deepEqual(env.opened.map((w) => w.url), [
    'chrome://zotero-fulltext/content/progress.xhtml', 'chrome://zotero-fulltext/content/login.xhtml',
  ]);
  assert.ok(env.opened.every((w) => w.closed), 'both windows are closed again');
  assert.ok(env.opened.every((w) => w.arg.wrappedJSObject === w.arg), 'args are wrapped for window.arguments[0].wrappedJSObject');
  assert.equal(env.listeners.size, 0, 'the console listener is unregistered');
});

test('windows: a console error from zotero-fulltext sources fails the check; warnings and foreign errors do not', async () => {
  const progressUrl = 'chrome://zotero-fulltext/content/progress.xhtml';
  const env = windowsEnv({
    errorsFor: {
      [progressUrl]: [
        { source: 'chrome://zotero-fulltext/content/progress-window.js', message: 'TypeError: model is undefined' },
        { source: 'chrome://zotero-fulltext/content/progress-window.js', message: 'just a warning', flags: 1 },
        { source: 'chrome://zotero/content/other.js', message: 'someone else' },
      ],
    },
  });
  await assert.rejects(env.ZFT.harness.CHECKS.windows(), /progress: TypeError: model is undefined/);
  assert.ok(env.opened.every((w) => w.closed));
  assert.equal(env.listeners.size, 0);
  assert.equal(env.opened.length, 2, 'login.xhtml was still checked');
});

test('windows: a window that is not well-formed fails the check but the other window is still checked and everything is closed', async () => {
  const loginUrl = 'chrome://zotero-fulltext/content/login.xhtml';
  const env = windowsEnv({ loadFails: [loginUrl] });
  await assert.rejects(env.ZFT.harness.CHECKS.windows(), /login: Error: window is not well-formed/);
  assert.equal(env.opened.length, 2);
  assert.ok(env.opened.every((w) => w.closed));
});

test('the check list is exactly the documented one', () => {
  const env = setup();
  assert.deepEqual(Object.keys(env.ZFT.harness.CHECKS), ['modules', 'wasm', 'prefs', 'menu', 'menu_ui', 'prefpane', 'prefpane_ui', 'ftl', 'windows']);
  assert.ok(SHELL_FILES.includes('harness.js'));
});

// --- menu_ui / prefpane_ui (the checks that look at real windows) ------------------------------------------------------

/** A popup fake with exactly the DOM surface checkMenuUi uses. `pending` entries appear when the group submenu opens. */
function fakePopup(grouped) {
  const popup = {
    entries: [], pending: [], opened: 0,
    querySelectorAll(sel) {
      return sel === 'menuitem.zotero-custom-menu-item' || sel === '.zotero-custom-menu-item' ? popup.entries.slice() : [];
    },
    querySelector(sel) {
      return grouped && sel === 'menu.zotero-custom-menu-item > menupopup' ? popup.submenu : null;
    },
    submenu: {
      dispatchEvent(ev) {
        popup.opened++;
        popup.entries.push(...popup.pending.splice(0));
        return ev;
      },
    },
  };
  return popup;
}

function menuUiEnv({ grouped = false, translated = true, hiddenEntries = false, dropTools = false } = {}) {
  const env = setup();
  const item = fakePopup(grouped);
  const tools = fakePopup(false);
  const make = (l10nId, label) => {
    const attrs = { 'data-l10n-id': l10nId, label: translated ? label : l10nId };
    const e = { hidden: hiddenEntries, getAttribute: (n) => (n in attrs ? attrs[n] : null), remove() { e.removed = true; } };
    return e;
  };
  const updates = [];
  env.zotero.Item = class { constructor(type) { this.type = type; } };
  env.zotero.MenuManager = {
    updateMenuPopup(popup, target, args) {
      updates.push([target, args.getContext()]);
      if (target === 'main/library/item') {
        (grouped ? popup.pending : popup.entries).push(make('zotero-fulltext-menu-get', 'Get Full Text'), make('zotero-fulltext-menu-get-oa', 'Get Full Text (open access only)'));
      } else {
        popup.entries.push(make('zotero-fulltext-menu-signin', 'Sign In'), make('zotero-fulltext-menu-progress', 'Progress'));
        if (!dropTools) popup.entries.push(make('zotero-fulltext-menu-prefs', 'Preferences'));
      }
    },
  };
  const elements = { 'zotero-itemmenu': item };
  const win = {
    Event: class { constructor(type) { this.type = type; } },
    document: { getElementById: (id) => elements[id] || null, querySelector: (sel) => (sel === '#menu_ToolsPopup' ? tools : null) },
  };
  env.zotero.getMainWindow = () => win;
  env.ZFT.harness.TIMEOUTS.menu = 150;
  return { ...env, item, tools, updates };
}

test('menu_ui: both popups show our entries with translated labels for an eligible selection, then are cleaned up', async () => {
  const env = menuUiEnv();
  const detail = plain(await env.ZFT.harness.CHECKS.menu_ui());
  assert.deepEqual(detail, {
    item: ['Get Full Text', 'Get Full Text (open access only)'],
    tools: ['Sign In', 'Progress', 'Preferences'],
    grouped: false,
  });
  assert.deepEqual(env.updates.map((u) => u[0]), ['main/library/item', 'main/menubar/tools']);
  const itemContext = env.updates[0][1];
  assert.equal(itemContext.items.length, 1);
  assert.equal(itemContext.items[0].type, 'journalArticle', 'an unsaved regular item is the selection');
  assert.ok([...env.item.entries, ...env.tools.entries].every((e) => e.removed), 'no entries are left behind in the popups');
});

test('menu_ui: entries folded into the "more" submenu (small screen) are found by opening it', async () => {
  const env = menuUiEnv({ grouped: true });
  const detail = plain(await env.ZFT.harness.CHECKS.menu_ui());
  assert.equal(detail.grouped, true);
  assert.equal(env.item.opened, 1);
  assert.equal(detail.item.length, 2);
});

test('menu_ui: untranslated labels, missing entries and hidden entries fail the check (and still clean up)', async () => {
  const untranslated = menuUiEnv({ translated: false });
  await assert.rejects(untranslated.ZFT.harness.CHECKS.menu_ui(), /menu entries missing or untranslated: item 2, tools 3/);
  assert.ok(untranslated.item.entries.every((e) => e.removed));
  const missing = menuUiEnv({ dropTools: true });
  await assert.rejects(missing.ZFT.harness.CHECKS.menu_ui(), /menu entries missing or untranslated: item 2, tools 2/);
  const hidden = menuUiEnv({ hiddenEntries: true });
  await assert.rejects(hidden.ZFT.harness.CHECKS.menu_ui(), /2 item entries are hidden for an eligible selection/);
  const noWindow = menuUiEnv();
  noWindow.zotero.getMainWindow = () => null;
  await assert.rejects(noWindow.ZFT.harness.CHECKS.menu_ui(), /no main window/);
});

/** The REAL prefs-pane.js running against the pane DOM fake, in the shape of the preferences window the check drives. */
function paneUiEnv({ init = true, l10n = true } = {}) {
  const env = setup();
  const dom = makePaneDocument({ l10n });
  const closed = [];
  const win = Object.assign(makePaneWindow(), {
    document: dom.document,
    Event: class { constructor(type) { this.type = type; } },
    close() { closed.push(true); },
  });
  const ctx = makeContext({ document: dom.document, window: win, Zotero: env.zotero, Services: env.services });
  env.zotero.ZoteroFullText = env.ZFT;
  runScript(ctx, 'prefs-pane.js');
  env.ZFT.plugin.paneID = 'pane-1';
  env.zotero.Utilities.Internal.openPreferences = (id) => {
    env.calls.push(['openPreferences', [id]]);
    if (init) ctx.ZoteroFullTextPrefs.init();
    return win;
  };
  env.ZFT.harness.TIMEOUTS.pane = 150;
  env.ZFT.harness.TIMEOUTS.edit = 150;
  return { ...env, dom, win, closed, ctx };
}

test('prefpane_ui: the real pane builds a control per pref, labels are translated, an edit is clamped through the handler; the pref is restored and the window closed', async () => {
  const env = paneUiEnv();
  env.zotero.Prefs.set('extensions.zotero-fulltext.run.item_concurrency', 3);
  const detail = plain(await env.ZFT.harness.CHECKS.prefpane_ui());
  assert.deepEqual(detail, {
    controls: Object.keys(plain(env.ZFT.config.DEFAULTS)).length,
    access: { options: 'openathens,saml,ezproxy', preset: 'imperial', proxy: 'ezproxy.example.ac.uk' },
  });
  const store = env.zotero.Prefs.store;
  assert.strictEqual(store.get('extensions.zotero-fulltext.institution.saml_entity_id'), 'https://kclidp.kcl.ac.uk/idp/shibboleth', 'the access-method edits are undone');
  assert.strictEqual(store.get('extensions.zotero-fulltext.institution.ezproxy_login_host') ?? 'login.kcl.idm.oclc.org', 'login.kcl.idm.oclc.org');
  assert.deepEqual(plain(env.calls.find((c) => c[0] === 'openPreferences')[1]), ['pane-1']);
  assert.strictEqual(env.zotero.Prefs.store.get('extensions.zotero-fulltext.run.item_concurrency'), 3, 'the harness edit is undone');
  assert.equal(env.closed.length, 1);
});

test('prefpane_ui: failures are reported and the pref is still restored and the window closed', async () => {
  const never = paneUiEnv({ init: false });
  await assert.rejects(never.ZFT.harness.CHECKS.prefpane_ui(), /did not build and translate its controls/);
  assert.equal(never.closed.length, 1);

  const ids = paneUiEnv({ l10n: false });
  await assert.rejects(ids.ZFT.harness.CHECKS.prefpane_ui(), /labels show message ids/);
  assert.equal(ids.closed.length, 1);

  const problem = paneUiEnv();
  for (const key of ['openathens_domain', 'saml_entity_id', 'ezproxy_login_host']) problem.zotero.Prefs.set('extensions.zotero-fulltext.institution.' + key, '');
  await assert.rejects(problem.ZFT.harness.CHECKS.prefpane_ui(), /lists problems for the stock prefs/);
  assert.equal(problem.closed.length, 1);

  const brokenSave = paneUiEnv();
  brokenSave.ZFT.config.setPref = () => { throw new Error('read-only'); };
  await assert.rejects(brokenSave.ZFT.harness.CHECKS.prefpane_ui(), /an edit did not reach the pref/);
  assert.equal(brokenSave.closed.length, 1);

  const unpublished = paneUiEnv();
  unpublished.win.ZoteroFullTextPrefs = undefined;
  await assert.rejects(unpublished.ZFT.harness.CHECKS.prefpane_ui(), /ZoteroFullTextPrefs is not published/);
});

test('prefpane_ui: no pane id or no window is an error', async () => {
  const noPane = paneUiEnv();
  noPane.ZFT.plugin.paneID = null;
  await assert.rejects(noPane.ZFT.harness.CHECKS.prefpane_ui(), /no preference pane id/);
  const noWindow = paneUiEnv();
  noWindow.zotero.Utilities.Internal.openPreferences = () => null;
  await assert.rejects(noWindow.ZFT.harness.CHECKS.prefpane_ui(), /preferences window did not open/);
});
