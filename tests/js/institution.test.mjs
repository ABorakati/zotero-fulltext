// Institution picker: ZFT.config presets (INSTITUTIONS, applyPreset, matchPreset, parseHostname, redirector probe helpers),
// the prefs.js / DEFAULTS mirror for the new prefs, and the preferences-pane dropdown, "Other" fields and Test button.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, makeContext, runScript, readPrefsJs, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { makePaneDocument, makePaneWindow } from './helpers/pane-dom.mjs';
import { ftlIds } from './helpers/ftl-ids.mjs';

const BRANCH = 'extensions.zotero-fulltext.';

/** The OpenAthens domains verified on 2026-10-21 (unauthenticated redirector GET: 200 "Please wait", then 302 to the institution's IdP). */
const VERIFIED = {
  kcl: 'kcl.ac.uk', city: 'city.ac.uk', leeds: 'leeds.ac.uk', reading: 'reading.ac.uk', sgul: 'sgul.ac.uk',
  ucl: 'ucl.ac.uk', uea: 'uea.ac.uk', warwick: 'warwick.ac.uk',
};

function configEnv() {
  const zotero = makeFakeZotero();
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  const store = zotero.Prefs.store;
  return { ZFT, C: ZFT.config, zotero, store, pref: (k) => store.get(BRANCH + k) };
}

// --- DEFAULTS / prefs.js ------------------------------------------------------------------------------------------------

test('institution.preset and inst.keep_session exist in DEFAULTS and prefs.js with the same value and type', () => {
  const { C } = configEnv();
  const prefs = readPrefsJs();
  assert.strictEqual(C.DEFAULTS['institution.preset'], 'kcl');
  assert.strictEqual(C.DEFAULTS['inst.keep_session'], true);
  assert.strictEqual(prefs.get(BRANCH + 'institution.preset'), 'kcl');
  assert.strictEqual(prefs.get(BRANCH + 'inst.keep_session'), true);
  assert.deepEqual(plain(C.PLUGIN_ONLY).sort(), ['debug.inspect_in_worker', 'inst.keep_session', 'institution.preset']);
});

test('the plugin-only prefs never reach the EngineConfig', () => {
  const { C } = configEnv();
  const cfg = plain(C.buildEngineConfig((k) => ({ 'institution.preset': 'ucl', 'inst.keep_session': false })[k]));
  const json = JSON.stringify(cfg);
  assert.ok(!json.includes('keep_session') && !json.includes('preset'));
});

// --- presets ------------------------------------------------------------------------------------------------------------

test('INSTITUTION_KEYS is every institution.* default except the picker choice, in DEFAULTS order', () => {
  const { C } = configEnv();
  const expected = Object.keys(plain(C.DEFAULTS)).filter((k) => k.startsWith('institution.') && k !== 'institution.preset');
  assert.deepEqual(plain(C.INSTITUTION_KEYS), expected);
  assert.equal(expected.length, 14);
  assert.deepEqual(expected.slice(-2), ['institution.saml_entity_id', 'institution.ezproxy_all_hosts']);
});

test('the KCL preset is exactly the DEFAULTS institution values and comes first', () => {
  const { C } = configEnv();
  const presets = plain(C.INSTITUTIONS);
  assert.equal(presets[0].id, 'kcl');
  assert.equal(C.PRESET_DEFAULT, 'kcl');
  for (const key of C.INSTITUTION_KEYS) assert.strictEqual(presets[0].values[key], C.DEFAULTS[key], key);
});

test('the preset list: unique ids, KCL first, every key present, FTL names exist, OpenAthens domains are exactly the verified ones', () => {
  const { C } = configEnv();
  const presets = plain(C.INSTITUTIONS);
  const oa = Object.fromEntries(presets.filter((p) => p.values['institution.openathens_domain']).map((p) => [p.id, p.values['institution.openathens_domain']]));
  assert.deepEqual(oa, VERIFIED);
  assert.equal(new Set(presets.map((p) => p.id)).size, presets.length);
  assert.equal(presets[0].id, 'kcl');
  assert.deepEqual(presets.slice(1).map((p) => p.id), presets.slice(1).map((p) => p.id).sort(), 'the rest is in id order');
  assert.ok(presets.length >= 30);
  assert.ok(!presets.some((p) => p.id === 'other' || p.id === 'none'), 'the two special ids stay free');
  const ids = ftlIds();
  for (const p of presets) {
    assert.deepEqual(Object.keys(p.values).sort(), [...plain(C.INSTITUTION_KEYS)].sort(), p.id);
    const domain = p.values['institution.openathens_domain'];
    assert.ok(domain === '' || C.parseHostname(domain).ok, p.id);
    assert.ok(p.values['institution.name'].length > 3, p.id);
    assert.ok(ids.has(p.l10n), p.l10n);
    assert.match(p.l10n, /^zotero-fulltext-prefs-institution-preset-[a-z]+$/);
    assert.equal(p.l10n, 'zotero-fulltext-prefs-institution-preset-' + p.id);
  }
});

/** entityIDs read on 2026-10-21 from https://md.seamlessaccess.org/entities/?q=<name> (visible UK-federation IdPs, domain cross-checked). */
const VERIFIED_SAML = {
  kcl: 'https://kclidp.kcl.ac.uk/idp/shibboleth', imperial: 'https://shibboleth.imperial.ac.uk/shibboleth',
  oxford: 'https://registry.shibboleth.ox.ac.uk/idp', cambridge: 'https://shib.raven.cam.ac.uk/shibboleth',
  qmul: 'https://idp.shibboleth.qmul.ac.uk/idp/shibboleth', manchester: 'https://shib.manchester.ac.uk/shibboleth',
  edinburgh: 'https://idp.ed.ac.uk/shibboleth', glasgow: 'https://idp.gla.ac.uk/shibboleth', bath: 'https://idp.bath.ac.uk/shibboleth',
  birmingham: 'https://idp.bham.ac.uk/shibboleth', bristol: 'https://idp.bris.ac.uk/shibboleth', cardiff: 'https://idp.cardiff.ac.uk/shibboleth',
  exeter: 'https://elibrary.exeter.ac.uk/idp/shibboleth', lancaster: 'https://idp.lancs.ac.uk/idp/shibboleth',
  leeds: 'https://passport01.leeds.ac.uk/idp/shibboleth', leicester: 'https://idp.le.ac.uk/shibboleth', lse: 'https://lse.ac.uk/idp',
  newcastle: 'https://gateway.ncl.ac.uk/idp/shibboleth', nottingham: 'https://idp.nottingham.ac.uk/shibboleth',
  qub: 'https://qub.ac.uk/shibboleth', sheffield: 'https://idp.shef.ac.uk/shibboleth', southampton: 'https://webauth.soton.ac.uk/shibboleth',
  standrews: 'https://idp.st-andrews.ac.uk/shibboleth', sussex: 'https://idp.sussex.ac.uk/shibboleth',
  ucl: 'https://shib-idp.ucl.ac.uk/shibboleth', uea: 'https://login.uea.ac.uk/entity', warwick: 'https://idp.warwick.ac.uk/idp/shibboleth',
  york: 'https://shib.york.ac.uk/shibboleth',
};

/** Library proxies named by the library's own public page AND answering an unauthenticated login-page GET with the EZproxy sign-in page (2026-10-21). */
const VERIFIED_PROXY = {
  kcl: ['login.kcl.idm.oclc.org', 'kcl.idm.oclc.org'], edinburgh: ['login.eux.idm.oclc.org', 'eux.idm.oclc.org'],
  glasgow: ['login.uofglasgow.idm.oclc.org', 'uofglasgow.idm.oclc.org'], qub: ['qub.idm.oclc.org', 'qub.idm.oclc.org'],
  southampton: ['soton.idm.oclc.org', 'soton.idm.oclc.org'],
};

test('presets declare their access methods and the declaration equals what their values provide', () => {
  const { C } = configEnv();
  for (const p of plain(C.INSTITUTIONS)) {
    assert.ok(p.methods.length > 0 && p.methods.every((m) => ['openathens', 'saml', 'ezproxy'].includes(m)), p.id);
    const has = [];
    if (p.values['institution.openathens_domain']) has.push('openathens');
    if (p.values['institution.saml_entity_id']) has.push('saml');
    if (p.values['institution.ezproxy_login_host'] && p.values['institution.ezproxy_suffix']) has.push('ezproxy');
    assert.deepEqual(p.methods, has, p.id);
    // ... and the engine config activates exactly those methods.
    assert.deepEqual(plain(C.activeMethods(C.buildEngineConfig((k) => p.values[k]))), p.methods, p.id);
  }
  const by = Object.fromEntries(plain(C.INSTITUTIONS).map((p) => [p.id, p.methods]));
  assert.deepEqual(by.kcl, ['openathens', 'saml', 'ezproxy']);
  assert.deepEqual(by.imperial, ['saml']);
  assert.deepEqual(by.edinburgh, ['saml', 'ezproxy']);
  assert.deepEqual(by.city, ['openathens']);
  assert.deepEqual(by.ucl, ['openathens', 'saml']);
});

test('SAML entityIDs are exactly the verified ones; Imperial, Oxford, Cambridge, QMUL, Manchester and Edinburgh are present', () => {
  const { C } = configEnv();
  const saml = Object.fromEntries(plain(C.INSTITUTIONS).filter((p) => p.values['institution.saml_entity_id']).map((p) => [p.id, p.values['institution.saml_entity_id']]));
  assert.deepEqual(saml, VERIFIED_SAML);
  for (const id of ['imperial', 'oxford', 'cambridge', 'qmul', 'manchester', 'edinburgh']) assert.ok(saml[id], id);
  for (const url of Object.values(saml)) assert.ok(C.isEntityId(url), url);
  // The IdP's own host is listed as a sign-in host (the engine recognises its pages); generic OpenAthens hosts stay.
  const imperial = plain(C.buildEngineConfig((k) => C.findPreset('imperial').values[k])).institution;
  assert.ok(imperial.idp_hosts.includes('shibboleth.imperial.ac.uk') && imperial.idp_hosts.includes('login.openathens.net'));
  assert.equal(imperial.saml_entity_id, 'https://shibboleth.imperial.ac.uk/shibboleth');
  assert.equal(imperial.openathens_domain, '', 'Imperial is unknown to OpenAthens');
});

test('library proxies: only the verified ones; a proxy preset sends every publisher through it and keeps Alma / LibKey / Entra empty', () => {
  const { C } = configEnv();
  const proxy = Object.fromEntries(plain(C.INSTITUTIONS).filter((p) => p.values['institution.ezproxy_login_host']).map((p) => [p.id, [p.values['institution.ezproxy_login_host'], p.values['institution.ezproxy_suffix']]]));
  assert.deepEqual(proxy, VERIFIED_PROXY);
  for (const p of plain(C.INSTITUTIONS)) {
    const inst = plain(C.buildEngineConfig((k) => p.values[k])).institution;
    assert.equal(inst.ezproxy_all_hosts, p.id !== 'kcl' && p.methods.includes('ezproxy'), p.id);
    if (p.id === 'kcl') {
      assert.ok(inst.ezproxy && inst.alma && inst.libkey_library_id === 286 && inst.entra_tenant);
      assert.equal(inst.ezproxy_hosts.length, 19);
    } else {
      assert.deepEqual([inst.alma, inst.libkey_library_id, inst.entra_tenant, inst.ezproxy_hosts], [null, null, null, []], p.id);
      assert.ok(inst.idp_hosts.includes('login.openathens.net'), p.id);
      assert.deepEqual(inst.passthrough_hosts, ['journals.lww.com']);
      assert.equal(inst.ezproxy !== null, p.methods.includes('ezproxy'), p.id);
    }
    assert.equal(inst.saml_entity_id, p.values['institution.saml_entity_id'] || null, p.id);
  }
  // Every verified proxy prefix round-trips through the pasted-address parser.
  for (const [id, [host, suffix]] of Object.entries(VERIFIED_PROXY)) {
    const parsed = plain(C.parseProxyPrefix('https://' + host + '/login?url='));
    assert.deepEqual([parsed.ok, parsed.loginHost, parsed.suffix], [true, host, suffix], id);
  }
});

test('the picker options are the presets, then Other, then None, with FTL ids', () => {
  const { C } = configEnv();
  const row = plain(C.PANE_LAYOUT).flatMap((g) => g.rows).find((r) => r.key === 'institution.preset');
  assert.equal(row.widget, 'select');
  assert.deepEqual(row.options.map((o) => o.value), [...plain(C.INSTITUTIONS).map((p) => p.id), 'other', 'none']);
  const ids = ftlIds();
  for (const o of row.options) assert.ok(ids.has(o.label), o.label);
});

test('applyPreset(ucl) writes all institution.* prefs, the choice and inst.enabled; nothing of KCL survives', () => {
  const { C, pref, zotero } = configEnv();
  zotero.Prefs.set(BRANCH + 'inst.enabled', false);
  const res = plain(C.applyPreset('ucl'));
  assert.deepEqual(res, { id: 'ucl', enabled: true, name: 'University College London' });
  const ucl = plain(C.findPreset('ucl')).values;
  for (const key of C.INSTITUTION_KEYS) assert.strictEqual(pref(key), ucl[key], key);
  assert.strictEqual(pref('institution.preset'), 'ucl');
  assert.strictEqual(pref('inst.enabled'), true);
  const inst = plain(C.buildEngineConfig()).institution;
  assert.equal(inst.openathens_domain, 'ucl.ac.uk');
  assert.equal(inst.name, 'University College London');
  assert.deepEqual([inst.ezproxy, inst.alma, inst.libkey_library_id, inst.entra_tenant], [null, null, null, null]);
  assert.ok(!JSON.stringify(inst).includes('kcl'), 'no KCL host or id left over');
});

test('applyPreset(kcl) restores the exact defaults after another institution', () => {
  const { C } = configEnv();
  C.applyPreset('warwick');
  C.applyPreset('kcl');
  const fresh = configEnv().C;
  assert.deepEqual(plain(C.buildEngineConfig()), plain(fresh.buildEngineConfig()));
});

test('applyPreset(none) turns institutional access off and leaves the institution.* prefs alone', () => {
  const { C, pref } = configEnv();
  C.applyPreset('leeds');
  const before = Object.fromEntries(plain(C.INSTITUTION_KEYS).map((k) => [k, pref(k)]));
  const res = plain(C.applyPreset('none'));
  assert.deepEqual(res, { id: 'none', enabled: false, name: '' });
  assert.strictEqual(pref('inst.enabled'), false);
  assert.strictEqual(pref('institution.preset'), 'none');
  for (const k of plain(C.INSTITUTION_KEYS)) assert.strictEqual(pref(k), before[k], k);
  assert.equal(C.effectivePreset(), 'none');
  // Choosing an institution again switches it back on.
  C.applyPreset('leeds');
  assert.strictEqual(pref('inst.enabled'), true);
  assert.equal(C.effectivePreset(), 'leeds');
});

test('applyPreset(other) blanks a preset institution but keeps details that are already custom', () => {
  const { C, pref } = configEnv();
  const res = plain(C.applyPreset('other'));
  assert.deepEqual([res.id, res.enabled, res.name], ['other', true, '']);
  assert.strictEqual(pref('institution.preset'), 'other');
  assert.strictEqual(pref('institution.name'), '');
  assert.strictEqual(pref('institution.openathens_domain'), '');
  assert.strictEqual(pref('institution.ezproxy_login_host'), '');
  assert.strictEqual(pref('institution.alma_base'), '');
  assert.strictEqual(pref('institution.libkey_library_id'), 0);
  assert.strictEqual(pref('institution.entra_tenant'), '');
  assert.match(pref('institution.idp_hosts'), /login\.openathens\.net/);
  C.setPref('institution.name', 'Test University');
  C.setPref('institution.openathens_domain', 'test.ac.uk');
  C.applyPreset('other');
  assert.strictEqual(pref('institution.name'), 'Test University', 'a second "Other" does not wipe what was typed');
  assert.strictEqual(pref('institution.openathens_domain'), 'test.ac.uk');
  assert.equal(C.effectivePreset(), 'other');
});

test('applyPreset rejects an unknown id without writing anything', () => {
  const { C, store } = configEnv();
  const before = JSON.stringify([...store]);
  assert.throws(() => C.applyPreset('hogwarts'), /unknown institution preset/);
  assert.equal(JSON.stringify([...store]), before);
});

test('applyPreset writes through an injected setter (so a failing write surfaces) and reads through an injected getter', () => {
  const { C } = configEnv();
  const written = [];
  C.applyPreset('uea', (k, v) => written.push([k, v]));
  assert.deepEqual(written.map(([k]) => k), [...plain(C.INSTITUTION_KEYS), 'inst.use_openathens', 'inst.use_saml', 'institution.preset', 'inst.enabled']);
  assert.throws(() => C.applyPreset('uea', () => { throw new Error('read-only prefs'); }), /read-only prefs/);
});

test('matchPreset / effectivePreset: defaults are KCL, any hand edit is Other, inst.enabled=false is None', () => {
  const { C, zotero } = configEnv();
  assert.equal(C.matchPreset(), 'kcl');
  assert.equal(C.effectivePreset(), 'kcl');
  for (const p of plain(C.INSTITUTIONS)) {
    assert.equal(C.matchPreset((k) => p.values[k]), p.id);
  }
  zotero.Prefs.set(BRANCH + 'institution.alma_inst', 'X_INST');
  assert.equal(C.matchPreset(), 'other');
  assert.equal(C.effectivePreset(), 'other');
  C.applyPreset('kcl');
  assert.equal(C.effectivePreset(), 'kcl');
  zotero.Prefs.set(BRANCH + 'inst.enabled', false);
  assert.equal(C.effectivePreset(), 'none');
  assert.equal(C.matchPreset(), 'kcl', 'matchPreset ignores inst.enabled');
  // Normalisation: a pasted URL / different case still matches.
  assert.equal(C.matchPreset((k) => (k === 'institution.openathens_domain' ? 'HTTPS://KCL.AC.UK/x' : C.findPreset('kcl').values[k])), 'kcl');
  assert.equal(C.effectivePreset(() => { throw new Error('prefs gone'); }), 'kcl', 'a throwing reader falls back to the defaults');
});

test('institutionName follows the stored name and is empty when there is none', () => {
  const { C } = configEnv();
  assert.equal(C.institutionName(), "King's College London");
  C.applyPreset('ucl');
  assert.equal(C.institutionName(), 'University College London');
  C.applyPreset('other');
  assert.equal(C.institutionName(), '');
  assert.equal(C.institutionName(() => { throw new Error('x'); }), '');
});

// --- validation ---------------------------------------------------------------------------------------------------------

test('parseHostname accepts host names and OpenAthens domains, normalises them, and rejects everything else', () => {
  const { C } = configEnv();
  const ok = (raw) => plain(C.parseHostname(raw));
  assert.deepEqual(ok('kcl.ac.uk'), { ok: true, value: 'kcl.ac.uk' });
  assert.deepEqual(ok('  Login.KCL.idm.oclc.org. '), { ok: true, value: 'login.kcl.idm.oclc.org' });
  assert.deepEqual(ok('https://www.Example.org/path?q=1'), { ok: true, value: 'www.example.org' });
  assert.deepEqual(ok('nhs'), { ok: true, value: 'nhs' }, 'single-label OpenAthens domains exist');
  assert.deepEqual(ok('a-b.c-d.ac.uk'), { ok: true, value: 'a-b.c-d.ac.uk' });
  for (const bad of ['', '   ', null, undefined, 'me@kcl.ac.uk', 'kcl ac uk', 'kcl..ac.uk', '-kcl.ac.uk', 'kcl-.ac.uk', 'kcl.ac.uk:443', 'k_cl.ac.uk', 'ké.ac.uk', 'a'.repeat(64) + '.uk', '.kcl.ac.uk']) {
    assert.equal(C.parseHostname(bad).ok, false, JSON.stringify(bad));
  }
});

test('validate() flags a malformed OpenAthens domain only while institutional access is on and the domain is not empty', () => {
  const { C } = configEnv();
  const base = plain(C.buildEngineConfig());
  const with_ = (domain, enabled = true) => plain(C.validate({ ...base, inst: { ...base.inst, enabled }, institution: { ...base.institution, openathens_domain: domain } }));
  assert.deepEqual(with_('kcl.ac.uk'), []);
  assert.deepEqual(with_('not a domain'), ['zotero-fulltext-prefs-problem-openathens-domain-format']);
  assert.deepEqual(with_(''), [], 'an empty domain is fine while another method (here Shibboleth and the proxy) is set up');
  assert.deepEqual(with_('not a domain', false), []);
  assert.ok(ftlIds().has('zotero-fulltext-prefs-problem-openathens-domain-format'));
});

test('redirectorTestUrl is the public redirector with a public article DOI as the target', () => {
  const { C } = configEnv();
  assert.equal(C.redirectorTestUrl('ucl.ac.uk'), 'https://go.openathens.net/redirector/ucl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373');
});

test('classifyRedirectorResponse: 200 OpenAthens page, 201 and 3xx = recognised; 403 = unknown organisation; the rest cannot be judged', () => {
  const { C } = configEnv();
  const c = (s, t) => C.classifyRedirectorResponse(s, t);
  assert.equal(c(200, '<title>OpenAthens / Please wait...</title>'), 'recognised');
  assert.equal(c(200, '<title>Wi-Fi login</title>'), 'failed', 'a captive portal is not OpenAthens');
  assert.equal(c(200, ''), 'failed');
  assert.equal(c(201), 'recognised');
  assert.equal(c(302), 'recognised');
  assert.equal(c(307), 'recognised');
  assert.equal(c(403), 'unknown');
  for (const s of [0, 404, 429, 500, 503]) assert.equal(c(s), 'failed', String(s));
});

// --- the preferences pane -----------------------------------------------------------------------------------------------

function paneSetup(opts = {}) {
  const calls = [];
  const requests = [];
  const http = {
    request: async (method, url, options) => {
      requests.push({ method, url, options });
      if (opts.httpThrows) throw new Error(opts.httpThrows);
      return opts.httpResponse || { status: 200, responseText: '<title>OpenAthens / Please wait...</title>' };
    },
  };
  const zotero = makeFakeZotero({ HTTP: http, File: { reveal: async () => {} } });
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  ZFT.runs = { reconfigure: async () => { calls.push(['reconfigure']); } };
  ZFT.state = { reset: async () => {}, init: async () => {}, dataDir: () => '/profile/zotero-fulltext' };
  zotero.ZoteroFullText = ZFT;
  const dom = makePaneDocument();
  const win = makePaneWindow();
  const Services = { prompt: { confirm: () => true } };
  const ctx = makeContext({ document: dom.document, window: win, Zotero: zotero, Services });
  runScript(ctx, 'prefs-pane.js');
  return { P: ctx.ZoteroFullTextPrefs, ZFT, C: ZFT.config, zotero, calls, requests, dom, win, ...dom };
}

const idOf = (key) => 'zft-pref-' + key.replace(/[._]/g, '-');
const control = (env, key) => env.byId(idOf(key));
const pref = (env, key) => env.zotero.Prefs.store.get(BRANCH + key);
const settle = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
const choose = async (env, value) => { const sel = control(env, 'institution.preset'); sel.value = value; await sel.fire('change'); await settle(); };
const type = async (env, key, value) => { const c = control(env, key); c.value = value; await c.fire('change'); await settle(); };
const statusId = (env) => env.byId('zft-prefs-status').getAttribute('data-l10n-id');
const lastL10n = (env, id) => env.l10nCalls.filter((c) => c[0] === id).pop();

test('pane: the dropdown lists the presets, Other and None, shows King\'s by default, and hides the Other fields', () => {
  const env = paneSetup();
  env.P.init();
  const picker = control(env, 'institution.preset');
  assert.equal(picker.localName, 'select');
  assert.deepEqual(picker.children.map((o) => o.getAttribute('value')), [...plain(env.C.INSTITUTIONS).map((p) => p.id), 'other', 'none']);
  assert.ok(picker.children.every((o) => /^zotero-fulltext-prefs-institution-preset(-[a-z]+)?$/.test(o.getAttribute('data-l10n-id'))));
  assert.equal(picker.value, 'kcl');
  assert.equal(env.byId('zft-institution-custom').hidden, true);
  assert.equal(env.byId('zft-institution-advanced').hidden, false);
  assert.equal(env.byId('zft-institution-test-result').hidden, true);
  // The picker is the first row of the institution group, and the custom + advanced sections follow it.
  const group = env.byId('zft-group-institution');
  assert.deepEqual(group.children.map((c) => c.localName), ['h2', 'div', 'p', 'div', 'details']);
  assert.equal(group.children[1].getAttribute('data-key'), 'institution.preset');
  assert.equal(group.children[2].id, 'zft-institution-methods');
  const custom = group.children[3];
  assert.equal(custom.id, 'zft-institution-custom');
  // The name row, then the method selector, then one panel per method with that method's own fields.
  assert.deepEqual(custom.children.filter((c) => c.getAttribute('data-key')).map((c) => c.getAttribute('data-key')), ['institution.name']);
  const panels = Object.fromEntries(custom.children.filter((c) => c.getAttribute('data-method')).map((c) => [c.getAttribute('data-method'), c]));
  assert.deepEqual(Object.keys(panels), ['openathens', 'saml', 'ezproxy']);
  const keysIn = (node) => node.children.filter((c) => c.getAttribute('data-key')).map((c) => c.getAttribute('data-key'));
  assert.deepEqual(keysIn(panels.openathens), ['institution.openathens_domain']);
  assert.deepEqual(keysIn(panels.saml), ['institution.saml_entity_id']);
  assert.deepEqual(keysIn(panels.ezproxy), ['institution.ezproxy_login_host', 'institution.ezproxy_suffix', 'institution.ezproxy_all_hosts']);
  assert.equal(control(env, 'inst.keep_session').checked, true, 'the new keep-session row is in the pane');
});

test('pane: choosing a preset writes every institution pref at once, refreshes the controls, says so, and reconfigures', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'ucl');
  const ucl = plain(env.C.findPreset('ucl')).values;
  for (const key of plain(env.C.INSTITUTION_KEYS)) {
    assert.strictEqual(pref(env, key), ucl[key], key);
    if (typeof ucl[key] === 'boolean') assert.equal(control(env, key).checked, ucl[key], `${key} control`);
    else assert.equal(control(env, key).value, String(ucl[key]), `${key} control`);
  }
  assert.strictEqual(pref(env, 'institution.preset'), 'ucl');
  assert.strictEqual(pref(env, 'inst.enabled'), true);
  assert.equal(control(env, 'institution.preset').value, 'ucl');
  assert.equal(env.byId('zft-institution-custom').hidden, true);
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-applied');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-institution-applied')[1]), { name: 'University College London' });
  assert.equal(env.byId('zft-prefs-status').hidden, false);
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 1);
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.openathens_domain, 'ucl.ac.uk');
  assert.equal(env.byId('zft-prefs-problems').hidden, true);
});

test('pane: the choice is remembered and shown again when the pane is reopened', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'warwick');
  env.P.destroy();
  assert.equal(env.byId('zft-prefs-root').children.length, 0, 'torn down');
  env.P.init();
  assert.equal(control(env, 'institution.preset').value, 'warwick');
  assert.equal(control(env, 'institution.openathens_domain').value, 'warwick.ac.uk');
  assert.equal(pref(env, 'institution.preset'), 'warwick');
});

test('pane: None sets inst.enabled=false, hides the details, and is remembered; ticking the checkbox again brings the institution back', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'leeds');
  await choose(env, 'none');
  assert.strictEqual(pref(env, 'inst.enabled'), false);
  assert.strictEqual(pref(env, 'institution.preset'), 'none');
  assert.equal(control(env, 'inst.enabled').checked, false, 'the checkbox follows');
  assert.equal(env.byId('zft-institution-advanced').hidden, true);
  assert.equal(env.byId('zft-institution-custom').hidden, true);
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-none-applied');
  assert.strictEqual(pref(env, 'institution.openathens_domain'), 'leeds.ac.uk', 'the institution values are kept');
  env.P.destroy();
  env.P.init();
  assert.equal(control(env, 'institution.preset').value, 'none');
  // Re-enable through the checkbox in the Institutional access group.
  const box = control(env, 'inst.enabled');
  box.checked = true;
  await box.fire('change');
  await settle();
  assert.equal(control(env, 'institution.preset').value, 'leeds');
  assert.strictEqual(pref(env, 'institution.preset'), 'leeds');
  assert.equal(env.byId('zft-institution-advanced').hidden, false);
  // And unticking it shows None.
  box.checked = false;
  await box.fire('change');
  assert.equal(control(env, 'institution.preset').value, 'none');
  assert.strictEqual(pref(env, 'institution.preset'), 'none');
});

test('pane: Other reveals blank name / domain / EZproxy fields (no leftover KCL values) and asks to fill them in', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  const custom = env.byId('zft-institution-custom');
  assert.equal(custom.hidden, false);
  assert.equal(control(env, 'institution.preset').value, 'other');
  for (const key of ['institution.name', 'institution.openathens_domain', 'institution.ezproxy_login_host', 'institution.ezproxy_suffix', 'institution.alma_base', 'institution.entra_tenant']) {
    assert.equal(control(env, key).value, '', key);
  }
  assert.equal(control(env, 'institution.libkey_library_id').value, '0');
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-other-applied');
  assert.deepEqual(plain(env.byId('zft-prefs-problems').children.map((p) => p.getAttribute('data-l10n-id'))), ['zotero-fulltext-prefs-problem-no-method'], 'no method is flagged until one is filled in');
  // The user types details; a second "Other" choice and a reopen keep them.
  await type(env, 'institution.name', '  Test University ');
  await type(env, 'institution.openathens_domain', 'https://Test.AC.uk/');
  assert.strictEqual(pref(env, 'institution.name'), 'Test University');
  assert.strictEqual(pref(env, 'institution.openathens_domain'), 'test.ac.uk', 'normalised to a bare lower-case host');
  assert.equal(control(env, 'institution.openathens_domain').value, 'test.ac.uk');
  assert.equal(env.byId('zft-prefs-problems').hidden, true);
  assert.equal(control(env, 'institution.preset').value, 'other');
  assert.equal(env.byId('zft-institution-custom').hidden, false);
  env.P.destroy();
  env.P.init();
  assert.equal(control(env, 'institution.preset').value, 'other');
  assert.equal(control(env, 'institution.name').value, 'Test University');
  assert.equal(env.byId('zft-institution-custom').hidden, false);
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.name, 'Test University');
});

test('pane: an invalid domain or host is rejected with a message, the control snaps back, and nothing is stored', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await type(env, 'institution.openathens_domain', 'test.ac.uk');
  for (const bad of ['me@test.ac.uk', 'not a domain', 'test..ac.uk', 'test.ac.uk:8080']) {
    await type(env, 'institution.openathens_domain', bad);
    assert.equal(control(env, 'institution.openathens_domain').value, 'test.ac.uk', bad);
    assert.strictEqual(pref(env, 'institution.openathens_domain'), 'test.ac.uk', bad);
    assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-domain-invalid', bad);
  }
  await type(env, 'institution.ezproxy_login_host', 'login host');
  assert.strictEqual(pref(env, 'institution.ezproxy_login_host'), '');
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-domain-invalid');
  await type(env, 'institution.ezproxy_login_host', 'Login.Test.idm.oclc.org');
  await type(env, 'institution.ezproxy_suffix', 'test.idm.oclc.org');
  assert.deepEqual([pref(env, 'institution.ezproxy_login_host'), pref(env, 'institution.ezproxy_suffix')], ['login.test.idm.oclc.org', 'test.idm.oclc.org']);
  assert.equal(env.byId('zft-prefs-status').hidden, true, 'a good edit clears the message');
  assert.deepEqual(plain(env.ZFT.config.buildEngineConfig()).institution.ezproxy, { login_host: 'login.test.idm.oclc.org', suffix: 'test.idm.oclc.org' });
  // An empty value is allowed (the "empty domain" problem is the advisory for it).
  await type(env, 'institution.ezproxy_suffix', '');
  assert.strictEqual(pref(env, 'institution.ezproxy_suffix'), '');
});

test('pane: coerce() hostname format: valid -> normalised, invalid -> error id, empty -> ok, other text rows untouched', () => {
  const env = paneSetup();
  const row = { widget: 'text', format: 'hostname' };
  assert.deepEqual(plain(env.P.coerce(row, ' KCL.ac.uk ')), { ok: true, value: 'kcl.ac.uk' });
  assert.deepEqual(plain(env.P.coerce(row, 'a b')), { ok: false, error: 'zotero-fulltext-prefs-institution-domain-invalid' });
  assert.deepEqual(plain(env.P.coerce(row, '')), { ok: true, value: '' });
  assert.deepEqual(plain(env.P.coerce({ widget: 'text' }, ' a b ')), { ok: true, value: 'a b' });
});

test('pane: editing a detail of a preset turns the picker into Other and stores that; legacy hand-edited prefs open as Other', async () => {
  const env = paneSetup();
  env.P.init();
  await type(env, 'institution.alma_inst', 'MY_INST');
  assert.equal(control(env, 'institution.preset').value, 'other');
  assert.strictEqual(pref(env, 'institution.preset'), 'other');
  assert.equal(env.byId('zft-institution-custom').hidden, false);
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length, 1);

  const legacy = paneSetup();
  legacy.zotero.Prefs.set(BRANCH + 'institution.name', 'Elsewhere University');   // preset pref still says 'kcl' (the default)
  legacy.P.init();
  assert.equal(control(legacy, 'institution.preset').value, 'other');
  assert.equal(legacy.byId('zft-institution-custom').hidden, false);
  assert.strictEqual(pref(legacy, 'institution.preset'), 'kcl', 'opening the pane writes nothing');
});

test('pane: reset-institution after a custom or disabled setup restores King\'s and turns institutional access on', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'none');
  await env.byId('zft-prefs-reset-institution').fire('click');
  await settle();
  assert.equal(control(env, 'institution.preset').value, 'kcl');
  assert.strictEqual(pref(env, 'institution.preset'), 'kcl');
  assert.strictEqual(pref(env, 'inst.enabled'), true);
  assert.equal(control(env, 'inst.enabled').checked, true);
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-restored');
  assert.equal(env.byId('zft-institution-advanced').hidden, false);
});

test('pane: Test asks OpenAthens once, anonymously, without following redirects, and reports "recognised"', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await type(env, 'institution.openathens_domain', 'Leeds.ac.uk');
  await env.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(env.requests.length, 1);
  const { method, url, options } = env.requests[0];
  assert.equal(method, 'GET');
  assert.equal(url, 'https://go.openathens.net/redirector/leeds.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373');
  assert.equal(options.anon, true, 'no cookies sent or kept');
  assert.equal(options.followRedirects, false, 'never reaches a sign-in page');
  assert.equal(options.successCodes, false);
  for (const key of ['body', 'headers', 'username', 'password']) assert.equal(options[key], undefined, `no ${key}`);
  const result = env.byId('zft-institution-test-result');
  assert.equal(result.hidden, false);
  assert.equal(result.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-test-ok');
  assert.equal(result.getAttribute('data-state'), 'ok');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-institution-test-ok')[1]), { domain: 'leeds.ac.uk' });
  assert.equal(env.byId('zft-institution-test').disabled, false, 'the button is usable again');
  assert.equal(env.zotero.debugLog.filter(([, m]) => /password|credential/i.test(m)).length, 0);
});

test('pane: Test reports a 302 (an unfollowed redirect) as recognised, and a 403 as unknown organisation', async () => {
  const redirected = paneSetup({ httpResponse: { status: 302, getResponseHeader: () => 'https://sp.nature.com/saml/login' } });
  redirected.P.init();
  await choose(redirected, 'other');
  await type(redirected, 'institution.openathens_domain', 'ucl.ac.uk');
  await redirected.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(redirected.byId('zft-institution-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-test-ok');

  const unknown = paneSetup({ httpResponse: { status: 403, responseText: '<title>OpenAthens: 403</title>' } });
  unknown.P.init();
  await choose(unknown, 'other');
  await type(unknown, 'institution.openathens_domain', 'bogus-university.example');
  await unknown.byId('zft-institution-test').fire('click');
  await settle();
  const result = unknown.byId('zft-institution-test-result');
  assert.equal(result.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-test-unknown');
  assert.equal(result.getAttribute('data-state'), 'unknown');
  assert.deepEqual(plain(lastL10n(unknown, 'zotero-fulltext-prefs-institution-test-unknown')[1]), { domain: 'bogus-university.example' });
  assert.strictEqual(pref(unknown, 'institution.openathens_domain'), 'bogus-university.example', 'the test never changes the prefs');
});

test('pane: Test reports a network failure or odd status as "could not check", never as recognised', async () => {
  const down = paneSetup({ httpThrows: 'offline' });
  down.P.init();
  await choose(down, 'other');
  await type(down, 'institution.openathens_domain', 'ucl.ac.uk');
  await down.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(down.byId('zft-institution-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-test-failed');
  assert.deepEqual(plain(lastL10n(down, 'zotero-fulltext-prefs-institution-test-failed')[1]), { reason: 'offline' });
  assert.equal(down.byId('zft-institution-test').disabled, false);

  const odd = paneSetup({ httpResponse: { status: 503, responseText: '' } });
  odd.P.init();
  await choose(odd, 'other');
  await type(odd, 'institution.openathens_domain', 'ucl.ac.uk');
  await odd.byId('zft-institution-test').fire('click');
  await settle();
  assert.deepEqual(plain(lastL10n(odd, 'zotero-fulltext-prefs-institution-test-failed')[1]), { reason: 'HTTP 503' });
});

test('pane: Test with an empty or malformed domain asks for one and makes no request', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await env.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(env.requests.length, 0);
  assert.equal(env.byId('zft-institution-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-institution-test-needs-domain');
  // Typing something invalid and pressing Test without the change event firing first.
  control(env, 'institution.openathens_domain').value = 'no good';
  await env.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(env.requests.length, 0);
});

test('pane: choosing another entry clears an old Test result; the pane survives closing while a test is out', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await type(env, 'institution.openathens_domain', 'ucl.ac.uk');
  await env.byId('zft-institution-test').fire('click');
  await settle();
  assert.equal(env.byId('zft-institution-test-result').hidden, false);
  await choose(env, 'kcl');
  assert.equal(env.byId('zft-institution-test-result').hidden, true);

  // Close the pane while the request is pending: no throw, no result written into the torn-down pane.
  let release;
  const slow = paneSetup();
  slow.zotero.HTTP.request = () => new Promise((resolve) => { release = () => resolve({ status: 403 }); });
  slow.P.init();
  await choose(slow, 'other');
  await type(slow, 'institution.openathens_domain', 'ucl.ac.uk');
  const click = slow.byId('zft-institution-test').fire('click');
  await settle();
  slow.P.destroy();
  release();
  await click;
  await settle();
  assert.ok(!slow.zotero.debugLog.some(([, m]) => /institution test failed/.test(m)));
});

test('pane: no listener leaks from the picker, the details section or the Test button', async () => {
  const env = paneSetup();
  const count = () => [...env.body.walk()].reduce((n, e) => n + e.listenerCount(), 0) + env.win.listenerCount();
  env.P.init();
  assert.ok(count() > 30);
  await choose(env, 'other');
  env.P.destroy();
  assert.equal(count(), 0);
});
