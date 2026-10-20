// ZFT.menu: registration shape, onShowing, onCommand and the Tools-menu entries (fake Zotero.MenuManager).
import test from 'node:test';
import assert from 'node:assert/strict';
import { plain } from './helpers/zft-env.mjs';
import { makeShellEnv, recorder, callNames } from './helpers/shell-env.mjs';

const PLUGIN_ID = 'zotero-fulltext@aborakati';

function setup(opts) {
  const env = makeShellEnv(opts);
  env.keys = env.ZFT.menu.register();
  const byMenuID = (id) => env.menuManager.registered.find((r) => r.def.menuID === id).def;
  env.item = byMenuID('zft-item-menu');
  env.tools = byMenuID('zft-tools-menu');
  return env;
}

/** A menu context with recorders for setVisible/setEnabled. */
function context(items) {
  const seen = { visible: [], enabled: [] };
  return {
    seen, items,
    setVisible: (v) => seen.visible.push(v),
    setEnabled: (v) => seen.enabled.push(v),
  };
}

test('registration shape: ids, plugin id, targets, l10n ids and hooks', () => {
  const { ZFT, item, tools, keys, menuManager } = setup();
  assert.deepEqual(plain(keys), [`${PLUGIN_ID}-zft-item-menu`, `${PLUGIN_ID}-zft-tools-menu`]);
  assert.deepEqual(plain(ZFT.menu.MENU_IDS), { item: 'zft-item-menu', tools: 'zft-tools-menu' });
  assert.equal(menuManager.registered.length, 2);

  assert.equal(item.pluginID, PLUGIN_ID);
  assert.equal(item.target, 'main/library/item');
  assert.equal(item.menus.length, 2, 'the grouped item menu keeps to two entries');
  assert.deepEqual(plain(item.menus.map((m) => [m.menuType, m.l10nID])), [
    ['menuitem', 'zotero-fulltext-menu-get'], ['menuitem', 'zotero-fulltext-menu-get-oa'],
  ]);
  for (const m of item.menus) {
    assert.equal(typeof m.onShowing, 'function');
    assert.equal(typeof m.onCommand, 'function');
  }
  assert.equal(item.menus[0].onShowing, ZFT.menu.onShowing);

  assert.equal(tools.pluginID, PLUGIN_ID);
  assert.equal(tools.target, 'main/menubar/tools');
  assert.deepEqual(plain(tools.menus.filter((m) => m.menuType === 'menuitem').map((m) => m.l10nID)), [
    'zotero-fulltext-menu-signin', 'zotero-fulltext-menu-progress', 'zotero-fulltext-menu-prefs',
  ]);
  assert.ok(tools.menus.filter((m) => m.menuType !== 'menuitem').every((m) => m.menuType === 'separator' && !m.l10nID));
  for (const m of tools.menus.filter((x) => x.menuType === 'menuitem')) assert.equal(typeof m.onCommand, 'function');
});

test('register() is idempotent: a second call replaces the first registration', () => {
  const env = setup();
  const second = env.ZFT.menu.register();
  assert.deepEqual(plain(second), plain(env.keys));
  assert.equal(env.menuManager.registered.length, 2, 'still exactly two live registrations');
  assert.equal(env.calls.filter((c) => c[0] === 'registerMenu').length, 4);
  assert.equal(env.calls.filter((c) => c[0] === 'unregisterMenu').length, 2, 'the first pair was removed before the second was added');
  assert.deepEqual(plain(env.ZFT.menu.registeredKeys()), plain(second));
});

test('unregister() removes every registration by its returned key and is idempotent', () => {
  const env = setup();
  env.calls.length = 0;
  env.ZFT.menu.unregister();
  assert.deepEqual(plain(env.calls.map((c) => c[1][0])), plain(env.keys));
  assert.equal(env.menuManager.registered.length, 0);
  env.ZFT.menu.unregister();
  assert.equal(env.calls.length, 2, 'the second unregister does nothing');
  assert.deepEqual(plain(env.ZFT.menu.registeredKeys()), []);
});

test('a refused registration is left out of the keys; a throwing MenuManager does not escape', () => {
  const refused = makeShellEnv({ refuseMenus: ['zft-tools-menu'] });
  assert.deepEqual(plain(refused.ZFT.menu.register()), [`${PLUGIN_ID}-zft-item-menu`]);
  const throwing = makeShellEnv();
  throwing.zotero.MenuManager.registerMenu = () => { throw new Error('invalid menu'); };
  assert.deepEqual(plain(throwing.ZFT.menu.register()), []);
  assert.ok(throwing.zotero.debugLog.some(([, m]) => /registerMenu zft-item-menu failed.*invalid menu/.test(m)));
  const missing = makeShellEnv({ zotero: { MenuManager: undefined } });
  assert.deepEqual(plain(missing.ZFT.menu.register()), []);
  assert.doesNotThrow(() => missing.ZFT.menu.unregister());
});

test('onShowing sets visibility from ZFT.items.menuEligible synchronously and does not await', () => {
  const env = setup();
  const { ZFT, item } = env;
  const items = [{ id: 1 }, { id: 2 }];
  for (const eligible of [true, false]) {
    ZFT.items.menuEligible = recorder(env.calls, 'items.menuEligible', () => eligible);
    const ctx = context(items);
    const ret = item.menus[0].onShowing({}, ctx);
    assert.deepEqual(ctx.seen.visible, [eligible], 'setVisible was called before onShowing returned');
    assert.equal(ret, undefined);
    assert.strictEqual(env.calls.filter((c) => c[0] === 'items.menuEligible').pop()[1][0], items, 'the selection is passed as is');
  }
});

test('onShowing never throws: an eligibility error hides the entry; a missing setVisible is tolerated', () => {
  const env = setup();
  env.ZFT.items.menuEligible = () => { throw new Error('db locked'); };
  const ctx = context([{ id: 1 }]);
  assert.doesNotThrow(() => env.item.menus[0].onShowing({}, ctx));
  assert.deepEqual(ctx.seen.visible, [false]);
  assert.doesNotThrow(() => env.item.menus[1].onShowing({}, { items: [] }));
});

test('onCommand starts a run with the right mode for each entry and the selection', async () => {
  const env = setup();
  const items = [{ id: 7 }, { id: 8 }];
  await env.item.menus[0].onCommand({}, { items });
  await env.item.menus[1].onCommand({}, { items });
  const starts = env.calls.filter((c) => c[0] === 'runs.startRun').map((c) => c[1][0]);
  assert.equal(starts.length, 2);
  assert.deepEqual(plain(starts.map((s) => [s.mode, s.interactive, s.items.length])), [['full', true, 2], ['oa_only', true, 2]]);
  assert.deepEqual(plain(starts[0].items), items);
  assert.notStrictEqual(starts[0].items, items, 'the selection is copied (the menu may reuse its array)');
  assert.equal(callNames(env.calls).includes('alert'), false);
});

test('onCommand calls startRun synchronously (before its promise settles)', () => {
  const env = setup();
  const ret = env.item.menus[0].onCommand({}, { items: [{ id: 1 }] });
  assert.equal(env.calls.filter((c) => c[0] === 'runs.startRun').length, 1);
  assert.ok(ret && typeof ret.then === 'function');
  return ret;
});

test('onCommand: a failing start is shown once with the localised reason; the handler never rejects or throws', async () => {
  const env = setup();
  env.ZFT.runs.startRun = async () => { throw new Error('zotero-fulltext-notice-wasm-load-failed'); };
  await env.item.menus[0].onCommand({}, { items: [{ id: 1 }] });
  const alerts = env.calls.filter((c) => c[0] === 'alert');
  assert.equal(alerts.length, 1, 'exactly one alert');
  const [win, title, text] = alerts[0][1];
  assert.equal(win, null);
  assert.equal(title, 'zotero-fulltext-prefs-title');
  // The l10n fallback (no Localization in tests) renders "<id> {args}": the reason is itself an l10n id, resolved first.
  assert.equal(text, 'zotero-fulltext-notice-start-failed {reason=zotero-fulltext-notice-wasm-load-failed}');

  env.calls.length = 0;
  env.ZFT.runs.startRun = () => { throw new Error('synchronous failure'); };
  let ret;
  assert.doesNotThrow(() => { ret = env.item.menus[1].onCommand({}, { items: [{ id: 1 }] }); });
  await ret;
  const text2 = env.calls.find((c) => c[0] === 'alert')[1][2];
  assert.equal(text2, 'zotero-fulltext-notice-start-failed {reason=synchronous failure}');
  assert.ok(env.zotero.debugLog.some(([, m]) => /menu action failed.*synchronous failure/.test(m)));
});

test('onCommand: no selection is reported as "select an item"; a throwing alert is contained', async () => {
  const env = setup();
  await env.item.menus[0].onCommand({}, { items: [] });
  assert.equal(env.calls.filter((c) => c[0] === 'runs.startRun').length, 0);
  assert.match(env.calls.find((c) => c[0] === 'alert')[1][2], /Could not start|zotero-fulltext-notice-start-failed/);
  env.services.prompt.alert = () => { throw new Error('no window'); };
  await assert.doesNotReject(async () => env.item.menus[0].onCommand({}, undefined));
  assert.ok(env.zotero.debugLog.some(([, m]) => /could not show the failure alert.*no window/.test(m)));
});

test('onCommand: an error carrying the start-failed id uses its own reason text', async () => {
  const env = setup();
  const e = new Error('zotero-fulltext-notice-start-failed');
  e.reason = 'too many items';
  env.ZFT.runs.startRun = async () => { throw e; };
  await env.item.menus[0].onCommand({}, { items: [{ id: 1 }] });
  assert.equal(env.calls.find((c) => c[0] === 'alert')[1][2], 'zotero-fulltext-notice-start-failed {reason=too many items}');
});

test('Tools menu: sign-in warms up with the current EngineConfig', async () => {
  const env = setup();
  const signin = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-signin');
  await signin.onCommand({}, {});
  const call = env.calls.find((c) => c[0] === 'login.warmUp');
  assert.ok(call, 'warmUp called');
  assert.deepEqual(Object.keys(plain(call[1][0])).sort(), ['endpoint_map', 'inst', 'institution', 'oa', 'run', 'trace']);
  assert.equal(call[1][0].institution.openathens_domain, 'kcl.ac.uk');
});

test('Tools menu: sign-in is enabled only while institutional access is on', () => {
  const env = setup();
  const signin = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-signin');
  const on = context([]);
  signin.onShowing({}, on);
  assert.deepEqual(on.seen.enabled, [true]);
  env.zotero.Prefs.set('extensions.zotero-fulltext.inst.enabled', false);
  const off = context([]);
  signin.onShowing({}, off);
  assert.deepEqual(off.seen.enabled, [false]);
});

test('Tools menu: progress shows the window; preferences open the registered pane', async () => {
  const env = setup();
  const progress = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-progress');
  const prefs = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-prefs');
  await progress.onCommand({}, {});
  assert.equal(env.calls.filter((c) => c[0] === 'progress.show').length, 1);
  env.ZFT.plugin.paneID = 'plugin-pane-xyz';
  await prefs.onCommand({}, {});
  assert.deepEqual(plain(env.calls.find((c) => c[0] === 'openPreferences')[1]), ['plugin-pane-xyz']);
  env.calls.length = 0;
  env.ZFT.plugin.paneID = null;
  await prefs.onCommand({}, {});
  const args = env.calls.find((c) => c[0] === 'openPreferences')[1];
  assert.equal(args.length, 1);
  assert.strictEqual(args[0], undefined, 'no pane id: open the default pane');
});

test('Tools menu handlers contain their failures with the same alert', async () => {
  const env = setup();
  env.ZFT.progress.show = () => { throw new Error('window blocked'); };
  const progress = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-progress');
  await assert.doesNotReject(async () => progress.onCommand({}, {}));
  assert.equal(env.calls.filter((c) => c[0] === 'alert').length, 1);
  env.ZFT.login.warmUp = async () => { throw new Error('sso down'); };
  const signin = env.tools.menus.find((m) => m.l10nID === 'zotero-fulltext-menu-signin');
  await signin.onCommand({}, {});
  assert.equal(env.calls.filter((c) => c[0] === 'alert').length, 2);
});
