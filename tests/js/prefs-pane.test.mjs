// plugin/content/prefs-pane.js (the preferences pane script) against a tiny fake DOM, plus prefs.xhtml's mount points.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { CONTENT, loadPlugin, makeContext, runScript, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { makePaneDocument, makePaneWindow } from './helpers/pane-dom.mjs';
import { ftlIds } from './helpers/ftl-ids.mjs';

const BRANCH = 'extensions.zotero-fulltext.';

/**
 * A preferences window: fake document/window, the real util+config modules as Zotero.ZoteroFullText, recorders for the
 * pieces owned by other packages (runs.reconfigure, state), and prefs-pane.js loaded into its own vm context.
 */
function setup(opts = {}) {
  const calls = [];
  const zotero = makeFakeZotero({ File: { reveal: async (p) => { calls.push(['reveal', p]); } } });
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  ZFT.runs = { reconfigure: async () => { calls.push(['reconfigure']); } };
  ZFT.state = {
    reset: async (keys) => { calls.push(['state.reset', keys]); },
    init: async () => { calls.push(['state.init']); },
    dataDir: () => '/profile/zotero-fulltext',
  };
  if (opts.noPlugin) delete zotero.ZoteroFullText;
  else zotero.ZoteroFullText = ZFT;
  const dom = makePaneDocument({ l10n: opts.l10n });
  const win = makePaneWindow();
  const confirms = [];
  const Services = { prompt: { confirm: (...a) => { confirms.push(a); return opts.confirm !== false; } } };
  const ctx = makeContext({ document: dom.document, window: win, Zotero: zotero, Services });
  runScript(ctx, 'prefs-pane.js');
  const P = ctx.ZoteroFullTextPrefs;
  return { P, ZFT, ctx, zotero, calls, dom, win, confirms, ...dom };
}

const idOf = (key) => 'zft-pref-' + key.replace(/[._]/g, '-');
const control = (env, key) => env.byId(idOf(key));
const pref = (env, key) => env.zotero.Prefs.store.get(BRANCH + key);
const rowsOf = (env) => plain(env.ZFT.config.PANE_LAYOUT).flatMap((g) => g.rows);
const settle = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };

test('the script publishes ZoteroFullTextPrefs on the window for the inline onload handler', () => {
  const env = setup();
  assert.equal(typeof env.P.init, 'function');
  assert.equal(typeof env.P.destroy, 'function');
  assert.strictEqual(env.win.ZoteroFullTextPrefs, env.P);
});

test('buildRow gives a plain descriptor per widget', () => {
  const { P, ZFT } = setup();
  const rows = Object.fromEntries(plain(ZFT.config.PANE_LAYOUT).flatMap((g) => g.rows).map((r) => [r.key, r]));
  assert.deepEqual(plain(P.buildRow(rows['oa.openalex'], true)), {
    key: 'oa.openalex', id: 'zft-pref-oa-openalex', widget: 'checkbox', tag: 'input', inputType: 'checkbox',
    labelId: 'zotero-fulltext-prefs-oa-openalex', secret: false, value: true,
  });
  const num = plain(P.buildRow(rows['run.item_concurrency'], 4));
  assert.deepEqual([num.tag, num.inputType, num.min, num.max, num.value], ['input', 'number', 1, 8, 4]);
  const secret = plain(P.buildRow(rows['oa.openalex_key'], ''));
  assert.deepEqual([secret.inputType, secret.secret], ['password', true]);
  assert.equal(plain(P.buildRow(rows['oa.unpaywall_email'], '')).inputType, 'text');
  assert.equal(plain(P.buildRow(rows['institution.ezproxy_hosts'], '')).inputType, 'text', 'lists are text inputs');
  const select = plain(P.buildRow(rows['inst.login_mode'], 'window'));
  assert.deepEqual([select.tag, select.inputType], ['select', null]);
  assert.deepEqual(select.options, [
    { value: 'window', labelId: 'zotero-fulltext-prefs-inst-login-mode-window' },
    { value: 'handoff', labelId: 'zotero-fulltext-prefs-inst-login-mode-handoff' },
  ]);
});

test('coerce: checkbox, number clamping, text trimming, list normalising, select validation', () => {
  const { P } = setup();
  const num = { widget: 'number', min: 1, max: 8 };
  assert.deepEqual(plain(P.coerce({ widget: 'checkbox' }, 1)), { ok: true, value: true });
  assert.deepEqual(plain(P.coerce({ widget: 'checkbox' }, false)), { ok: true, value: false });
  assert.deepEqual(plain(P.coerce(num, '5')), { ok: true, value: 5 });
  assert.deepEqual(plain(P.coerce(num, '0')), { ok: true, value: 1 });
  assert.deepEqual(plain(P.coerce(num, '99')), { ok: true, value: 8 });
  assert.deepEqual(plain(P.coerce(num, ' 3.6 ')), { ok: true, value: 4 });
  assert.deepEqual(plain(P.coerce(num, '')), { ok: false });
  assert.deepEqual(plain(P.coerce(num, 'abc')), { ok: false });
  assert.deepEqual(plain(P.coerce({ widget: 'number' }, '-7')), { ok: true, value: -7 }, 'no bounds on the row: no clamp');
  assert.deepEqual(plain(P.coerce({ widget: 'text' }, '  me@kcl.ac.uk \n')), { ok: true, value: 'me@kcl.ac.uk' });
  assert.deepEqual(plain(P.coerce({ widget: 'text', secret: true }, '  key ')), { ok: true, value: 'key' });
  assert.deepEqual(plain(P.coerce({ widget: 'list' }, ' a.com , ,b.com,, ')), { ok: true, value: 'a.com,b.com' });
  const sel = { widget: 'select', options: [{ value: 'ask' }, { value: 'never' }] };
  assert.deepEqual(plain(P.coerce(sel, 'never')), { ok: true, value: 'never' });
  assert.deepEqual(plain(P.coerce(sel, 'bogus')), { ok: false });
});

test('init builds one labelled control per pref, in the layout groups, with the stored values', async () => {
  const env = setup();
  env.zotero.Prefs.set(BRANCH + 'run.item_concurrency', 6);
  env.zotero.Prefs.set(BRANCH + 'oa.unpaywall_email', 'me@kcl.ac.uk');
  env.zotero.Prefs.set(BRANCH + 'inst.login_mode', 'handoff');
  env.zotero.Prefs.set(BRANCH + 'oa.openalex', false);
  env.P.init();
  const root = env.byId('zft-prefs-root');
  const groups = root.children.filter((c) => c.localName === 'div');
  assert.deepEqual(groups.map((g) => g.id), ['oa', 'inst', 'limits', 'run', 'institution', 'debug'].map((id) => 'zft-group-' + id));
  for (const g of groups) {
    const heading = g.children[0];
    assert.equal(heading.localName, 'h2');
    assert.match(heading.getAttribute('data-l10n-id'), /^zotero-fulltext-prefs-.*-heading$/);
  }
  const keys = Object.keys(plain(env.ZFT.config.DEFAULTS));
  for (const key of keys) {
    const c = control(env, key);
    assert.ok(c, `control for ${key}`);
    const wrap = c.parentNode;
    assert.equal(wrap.getAttribute('data-key'), key);
    const label = wrap.children.find((e) => e.localName === 'label');
    assert.equal(label.getAttribute('for'), idOf(key));
    assert.match(label.getAttribute('data-l10n-id'), /^zotero-fulltext-prefs-/);
  }
  assert.equal(control(env, 'run.item_concurrency').value, '6');
  assert.equal(control(env, 'run.item_concurrency').type, 'number');
  assert.equal(control(env, 'run.item_concurrency').getAttribute('min'), '1');
  assert.equal(control(env, 'run.item_concurrency').getAttribute('max'), '8');
  assert.equal(control(env, 'oa.unpaywall_email').value, 'me@kcl.ac.uk');
  assert.equal(control(env, 'oa.openalex').type, 'checkbox');
  assert.equal(control(env, 'oa.openalex').checked, false);
  assert.equal(control(env, 'oa.arxiv').checked, true);
  assert.equal(control(env, 'inst.login_mode').localName, 'select');
  assert.equal(control(env, 'inst.login_mode').value, 'handoff');
  assert.deepEqual(control(env, 'inst.login_mode').children.map((o) => o.getAttribute('value')), ['window', 'handoff']);
  assert.ok(control(env, 'inst.login_mode').children.every((o) => /^zotero-fulltext-prefs-inst-login-mode-/.test(o.getAttribute('data-l10n-id'))));
  assert.ok(env.l10nCalls.some((c) => c[0] === 'translateFragment'), 'the built DOM is translated');
  assert.deepEqual(rowsOf(env).length, keys.length);
  assert.equal(env.byId('zft-prefs-problems').hidden, true, 'no problems with the defaults');
});

test('secret rows are password inputs with a Show/Hide toggle; other rows have none', () => {
  const env = setup();
  env.P.init();
  for (const key of ['oa.openalex_key', 'oa.semantic_scholar_key', 'oa.core_key']) {
    const c = control(env, key);
    assert.equal(c.type, 'password', key);
    const toggle = c.parentNode.children.find((e) => e.localName === 'button');
    assert.ok(toggle, `toggle for ${key}`);
    assert.equal(toggle.getAttribute('type'), 'button');
    assert.equal(toggle.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-show-secret');
    toggle.fire('click');
    assert.equal(c.type, 'text');
    assert.equal(toggle.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-hide-secret');
    toggle.fire('click');
    assert.equal(c.type, 'password');
    assert.equal(toggle.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-show-secret');
  }
  assert.equal(control(env, 'oa.unpaywall_email').parentNode.children.some((e) => e.localName === 'button'), false);
});

test('a change writes the typed pref, clamps numbers, trims text and reconfigures the run controller', async () => {
  const env = setup();
  env.P.init();
  const num = control(env, 'run.item_concurrency');
  num.value = '99';
  await num.fire('change');
  await settle();
  assert.strictEqual(pref(env, 'run.item_concurrency'), 8);
  assert.equal(num.value, '8', 'the control shows the clamped value');
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 1);

  const email = control(env, 'oa.unpaywall_email');
  email.value = '  me@kcl.ac.uk ';
  await email.fire('change');
  assert.strictEqual(pref(env, 'oa.unpaywall_email'), 'me@kcl.ac.uk');
  assert.equal(email.value, 'me@kcl.ac.uk');

  const box = control(env, 'oa.biorxiv');
  box.checked = false;
  await box.fire('change');
  assert.strictEqual(pref(env, 'oa.biorxiv'), false);

  const list = control(env, 'institution.passthrough_hosts');
  list.value = ' a.com ,, b.com ';
  await list.fire('change');
  assert.strictEqual(pref(env, 'institution.passthrough_hosts'), 'a.com,b.com');
  assert.equal(list.value, 'a.com,b.com');

  const sel = control(env, 'inst.group_libraries');
  sel.value = 'never';
  await sel.fire('change');
  assert.strictEqual(pref(env, 'inst.group_libraries'), 'never');
  await settle();
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 5, 'one reconfigure per accepted change');
});

test('an invalid edit is rejected: the control snaps back and nothing is written or reconfigured', async () => {
  const env = setup();
  env.P.init();
  const num = control(env, 'run.pdf_max_mb');
  num.value = 'lots';
  await num.fire('change');
  assert.equal(num.value, '100');
  assert.strictEqual(pref(env, 'run.pdf_max_mb'), 100);
  const sel = control(env, 'inst.login_mode');
  sel.value = 'teleport';
  await sel.fire('change');
  assert.equal(sel.value, 'window');
  await settle();
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 0);
});

test('a failing pref write is logged and the control is restored', async () => {
  const env = setup();
  env.P.init();
  env.zotero.Prefs.set = () => { throw new Error('read-only prefs'); };
  const email = control(env, 'oa.core_key');
  email.value = 'abc';
  await email.fire('change');
  assert.equal(email.value, '');
  assert.ok(env.zotero.debugLog.some(([, m]) => /could not save oa\.core_key.*read-only prefs/.test(m)));
});

test('problems are listed (as l10n ids) and cleared as the prefs change', async () => {
  const env = setup();
  env.P.init();
  const box = env.byId('zft-prefs-problems');
  assert.equal(box.hidden, true);
  const domain = control(env, 'institution.openathens_domain');
  // King's has three sign-in methods: the problem only appears once the last of them is gone.
  for (const key of ['institution.openathens_domain', 'institution.saml_entity_id', 'institution.ezproxy_login_host']) {
    const c = control(env, key);
    assert.equal(box.hidden, true, 'still a method left before clearing ' + key);
    c.value = '';
    await c.fire('change');
  }
  assert.equal(box.hidden, false);
  assert.deepEqual(box.children.map((p) => p.getAttribute('data-l10n-id')), ['zotero-fulltext-prefs-problem-no-method']);
  const email = control(env, 'oa.unpaywall_email');
  email.value = 'nope';
  await email.fire('change');
  assert.deepEqual(box.children.map((p) => p.getAttribute('data-l10n-id')), [
    'zotero-fulltext-prefs-problem-no-method', 'zotero-fulltext-prefs-problem-unpaywall-email',
  ]);
  domain.value = 'kcl.ac.uk';
  await domain.fire('change');
  email.value = '';
  await email.fire('change');
  assert.equal(box.children.length, 0);
  assert.equal(box.hidden, true);
});

test('problems already present in the prefs (hand-edited login timeout) show up on init', () => {
  const env = setup();
  env.zotero.Prefs.set(BRANCH + 'inst.login_timeout_sec', 10);
  env.P.init();
  const box = env.byId('zft-prefs-problems');
  assert.deepEqual(box.children.map((p) => p.getAttribute('data-l10n-id')), ['zotero-fulltext-prefs-problem-login-timeout']);
});

test('reset-institution restores every institution.* default, refreshes the controls and reconfigures', async () => {
  const env = setup();
  env.zotero.Prefs.set(BRANCH + 'institution.name', 'Elsewhere University');
  env.zotero.Prefs.set(BRANCH + 'institution.openathens_domain', 'elsewhere.edu');
  env.zotero.Prefs.set(BRANCH + 'institution.libkey_library_id', 5);
  env.zotero.Prefs.set(BRANCH + 'run.item_concurrency', 7);
  env.P.init();
  assert.equal(control(env, 'institution.name').value, 'Elsewhere University');
  await env.byId('zft-prefs-reset-institution').fire('click');
  await settle();
  const defaults = plain(env.ZFT.config.DEFAULTS);
  for (const key of Object.keys(defaults).filter((k) => k.startsWith('institution.'))) {
    assert.strictEqual(pref(env, key), defaults[key], key);
    if (typeof defaults[key] === 'boolean') assert.equal(control(env, key).checked, defaults[key], `${key} control`);
    else assert.equal(control(env, key).value, String(defaults[key]), `${key} control`);
  }
  assert.strictEqual(pref(env, 'run.item_concurrency'), 7, 'non-institution prefs are untouched');
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 1);
  const status = env.byId('zft-prefs-status');
  assert.equal(status.hidden, false);
  assert.equal(status.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-restored');
});

test('clear-state asks first: declined does nothing, accepted resets the saved state and says so', async () => {
  const declined = setup({ confirm: false });
  declined.P.init();
  await declined.byId('zft-prefs-clear-state').fire('click');
  await settle();
  assert.equal(declined.confirms.length, 1);
  assert.equal(declined.calls.filter((c) => c[0] === 'state.reset').length, 0);
  assert.equal(declined.byId('zft-prefs-status').hidden, true);

  const accepted = setup({ confirm: true });
  accepted.P.init();
  await accepted.byId('zft-prefs-clear-state').fire('click');
  await settle();
  assert.equal(accepted.calls.filter((c) => c[0] === 'state.reset').length, 1);
  assert.equal(accepted.byId('zft-prefs-status').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-state-cleared');
  const [, title, text] = accepted.confirms[0];
  assert.equal(title, 'zotero-fulltext-prefs-title');
  assert.equal(text, 'zotero-fulltext-prefs-clear-state-confirm');
});

test('clear-state failures are logged, not thrown', async () => {
  const env = setup();
  env.ZFT.state.reset = async () => { throw new Error('disk full'); };
  env.P.init();
  await env.byId('zft-prefs-clear-state').fire('click');
  await settle();
  assert.ok(env.zotero.debugLog.some(([, m]) => /clear state failed.*disk full/.test(m)));
});

test('open-data reveals the plugin data folder (after making sure it exists)', async () => {
  const env = setup();
  env.P.init();
  await env.byId('zft-prefs-open-data').fire('click');
  await settle();
  assert.deepEqual(env.calls.filter((c) => c[0] === 'state.init' || c[0] === 'reveal').map((c) => c.join(':')), ['state.init', 'reveal:/profile/zotero-fulltext']);
  env.zotero.File.reveal = async () => { throw new Error('no file manager'); };
  await env.byId('zft-prefs-open-data').fire('click');
  await settle();
  assert.ok(env.zotero.debugLog.some(([, m]) => /open data folder failed.*no file manager/.test(m)));
});

test('init is idempotent: a second call builds nothing and registers no extra listeners', () => {
  const env = setup();
  env.P.init();
  const root = env.byId('zft-prefs-root');
  const count = () => [...env.body.walk()].reduce((n, e) => n + e.listenerCount(), 0) + env.win.listenerCount();
  const before = { children: root.children.length, listeners: count(), first: root.children[0] };
  assert.ok(before.listeners > 30);
  env.P.init();
  env.P.init();
  assert.equal(root.children.length, before.children);
  assert.strictEqual(root.children[0], before.first, 'the same DOM, not a rebuild');
  assert.equal(count(), before.listeners);
});

test('destroy removes every listener and the built DOM; init can mount again; destroy is safe twice', () => {
  const env = setup();
  const count = () => [...env.body.walk()].reduce((n, e) => n + e.listenerCount(), 0) + env.win.listenerCount();
  assert.equal(count(), 0);
  env.P.init();
  const mountedListeners = count();
  assert.ok(mountedListeners > 30);
  env.P.destroy();
  assert.equal(count(), 0, 'no listener leaks');
  assert.equal(env.byId('zft-prefs-root').children.length, 0);
  assert.doesNotThrow(() => env.P.destroy());
  env.P.init();
  assert.equal(count(), mountedListeners, 're-mounting gives the same wiring, nothing doubled');
  assert.ok(control(env, 'oa.arxiv'));
});

test('closing the window (unload) destroys the pane', async () => {
  const env = setup();
  env.P.init();
  assert.equal(env.win.listenerCount(), 1);
  await env.win.fire('unload');
  assert.equal(env.win.listenerCount(), 0);
  assert.equal(env.byId('zft-prefs-root').children.length, 0);
});

test('a new mount point (the pane was reloaded) replaces the old build', () => {
  const env = setup();
  env.P.init();
  const old = env.byId('zft-prefs-root');
  const fresh = env.document.createElement('div');
  fresh.setAttribute('id', 'zft-prefs-root');
  env.body.removeChild(old);
  env.body.appendChild(fresh);
  env.P.init();
  assert.ok(fresh.children.length > 0);
  assert.equal(old.children.length, 0, 'the old build was torn down');
  assert.equal(old.listenerCount() + [...old.walk()].reduce((n, e) => n + e.listenerCount(), 0), 0);
});

test('without the plugin (disabled) init shows a message and throws nothing; a later init works once it is back', () => {
  const env = setup({ noPlugin: true });
  assert.doesNotThrow(() => env.P.init());
  const box = env.byId('zft-prefs-problems');
  assert.equal(box.hidden, false);
  assert.equal(box.children[0].getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-unavailable');
  assert.equal(env.byId('zft-prefs-root').children.length, 0);
  env.zotero.ZoteroFullText = env.ZFT;
  env.P.init();
  assert.ok(control(env, 'oa.arxiv'));
});

test('without document.l10n the labels get their text directly from the FTL formatter', () => {
  const env = setup({ l10n: false });
  env.P.init();
  const label = control(env, 'oa.arxiv').parentNode.children.find((e) => e.localName === 'label');
  assert.equal(label.textContent, 'zotero-fulltext-prefs-oa-arxiv', 'the l10n fallback is the id in tests; in Zotero it is the English text');
});

test('init failing half-way (after the listeners are wired) cleans up after itself', () => {
  const env = setup();
  env.document.l10n.translateFragment = () => { throw new Error('l10n exploded'); };
  assert.doesNotThrow(() => env.P.init());
  const count = [...env.body.walk()].reduce((n, e) => n + e.listenerCount(), 0) + env.win.listenerCount();
  assert.equal(count, 0, 'no listeners survive a failed init');
  assert.equal(env.byId('zft-prefs-root').children.length, 0);
  assert.ok(env.zotero.debugLog.some(([, m]) => /init failed.*l10n exploded/.test(m)));
  env.document.l10n.translateFragment = async () => {};
  env.P.init();
  assert.ok(control(env, 'oa.arxiv'), 'a later init still works');
});

// --- prefs.xhtml ------------------------------------------------------------------------------------------------------

test('prefs.xhtml provides every mount point prefs-pane.js looks up and the inline onload hook', () => {
  const xhtml = fs.readFileSync(path.join(CONTENT, 'prefs.xhtml'), 'utf8');
  const script = fs.readFileSync(path.join(CONTENT, 'prefs-pane.js'), 'utf8');
  for (const id of ['zft-prefs-root', 'zft-prefs-problems', 'zft-prefs-status', 'zft-prefs-reset-institution', 'zft-prefs-clear-state', 'zft-prefs-open-data']) {
    assert.ok(xhtml.includes(`id="${id}"`), `${id} in prefs.xhtml`);
    assert.ok(script.includes(`'${id}'`), `${id} in prefs-pane.js`);
  }
  assert.match(xhtml, /onload="ZoteroFullTextPrefs\.init\(\)"/);
  assert.match(xhtml, /<html:link rel="localization" href="zotero-fulltext\.ftl"\/>/);
  assert.match(script, /window\.ZoteroFullTextPrefs = ZoteroFullTextPrefs/);
});

test('every data-l10n-id in prefs.xhtml exists in the FTL', () => {
  const xhtml = fs.readFileSync(path.join(CONTENT, 'prefs.xhtml'), 'utf8');
  const ids = ftlIds();
  const used = [...xhtml.matchAll(/data-l10n-id="([^"]+)"/g)].map((m) => m[1]);
  assert.ok(used.length >= 4);
  assert.deepEqual(used.filter((id) => !ids.has(id)), []);
});
