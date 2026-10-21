// Access methods beyond OpenAthens: Shibboleth / UK federation (identity-provider search) and the library proxy (EZproxy).
// Pure helpers in ZFT.config (SAML search parsing against recorded MDQ answers, EZproxy prefix parsing, probe classification),
// the three new prefs, and the preferences-pane method selector, search, pick, proxy address and Test buttons.
//
// Fixtures under tests/js/fixtures/ are real answers of GET https://md.seamlessaccess.org/entities/?q=<text> (no sign-in, no
// credentials), recorded 2026-10-21: mdq-oxford.json (q=oxford, 8 entries incl. 3 test/hidden IdPs and OpenAthens-registered ones),
// mdq-queen-mary.json (q=queen mary, 3), mdq-empty.json (q=zzzzqqqq, none).
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadPlugin, makeContext, runScript, readPrefsJs, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { makePaneDocument, makePaneWindow } from './helpers/pane-dom.mjs';
import { ftlIds } from './helpers/ftl-ids.mjs';

const BRANCH = 'extensions.zotero-fulltext.';
const FIXTURES = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures');
const fixture = (name) => fs.readFileSync(path.join(FIXTURES, name), 'utf8');

function configEnv() {
  const zotero = makeFakeZotero();
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  return { ZFT, C: ZFT.config, zotero, store: zotero.Prefs.store, pref: (k) => zotero.Prefs.store.get(BRANCH + k) };
}

// --- prefs mirror ----------------------------------------------------------------------------------------------------------

test('the three new prefs are in DEFAULTS and plugin/prefs.js with the same types, and are forwarded to the engine', () => {
  const { C } = configEnv();
  const prefs = readPrefsJs();
  assert.strictEqual(prefs.get(BRANCH + 'inst.use_saml'), true);
  assert.strictEqual(prefs.get(BRANCH + 'institution.ezproxy_all_hosts'), false);
  assert.strictEqual(typeof prefs.get(BRANCH + 'institution.saml_entity_id'), 'string');
  for (const key of ['inst.use_saml', 'institution.saml_entity_id', 'institution.ezproxy_all_hosts']) {
    assert.strictEqual(prefs.get(BRANCH + key), C.DEFAULTS[key], key);
    assert.ok(!plain(C.PLUGIN_ONLY).includes(key), key);
  }
  // The defaults are the King's preset: its Shibboleth IdP is on, the proxy is not forced for every publisher.
  assert.equal(C.DEFAULTS['institution.saml_entity_id'], 'https://kclidp.kcl.ac.uk/idp/shibboleth');
  const cfg = plain(C.buildEngineConfig());
  assert.equal(cfg.inst.use_saml, true);
  assert.equal(cfg.institution.saml_entity_id, 'https://kclidp.kcl.ac.uk/idp/shibboleth');
  assert.equal(cfg.institution.ezproxy_all_hosts, false);
});

test('an empty saml_entity_id pref is emitted as null and an all-hosts proxy as true', () => {
  const { C } = configEnv();
  const cfg = plain(C.buildEngineConfig((k) => ({ 'institution.saml_entity_id': '', 'institution.ezproxy_all_hosts': true })[k]));
  assert.strictEqual(cfg.institution.saml_entity_id, null);
  assert.strictEqual(cfg.institution.ezproxy_all_hosts, true);
});

test('every new FTL id used by the access-method code exists', () => {
  const ids = ftlIds();
  for (const id of [
    'zotero-fulltext-prefs-inst-use-saml', 'zotero-fulltext-prefs-method-select', 'zotero-fulltext-prefs-method-openathens',
    'zotero-fulltext-prefs-method-saml', 'zotero-fulltext-prefs-method-ezproxy', 'zotero-fulltext-prefs-methods-uses',
    'zotero-fulltext-prefs-methods-none', 'zotero-fulltext-prefs-other-hint', 'zotero-fulltext-prefs-saml-hint',
    'zotero-fulltext-prefs-saml-query', 'zotero-fulltext-prefs-saml-search', 'zotero-fulltext-prefs-saml-use',
    'zotero-fulltext-prefs-saml-searching', 'zotero-fulltext-prefs-saml-found', 'zotero-fulltext-prefs-saml-no-results',
    'zotero-fulltext-prefs-saml-failed', 'zotero-fulltext-prefs-saml-query-short', 'zotero-fulltext-prefs-saml-chosen',
    'zotero-fulltext-prefs-saml-chosen-none', 'zotero-fulltext-prefs-saml-picked', 'zotero-fulltext-prefs-saml-entity-id',
    'zotero-fulltext-prefs-saml-entity-invalid', 'zotero-fulltext-prefs-proxy-hint', 'zotero-fulltext-prefs-proxy-prefix',
    'zotero-fulltext-prefs-proxy-all-hosts', 'zotero-fulltext-prefs-proxy-saved', 'zotero-fulltext-prefs-proxy-invalid',
    'zotero-fulltext-prefs-proxy-invalid-path', 'zotero-fulltext-prefs-proxy-invalid-port', 'zotero-fulltext-prefs-proxy-invalid-credentials',
    'zotero-fulltext-prefs-proxy-test', 'zotero-fulltext-prefs-proxy-test-checking', 'zotero-fulltext-prefs-proxy-test-ok',
    'zotero-fulltext-prefs-proxy-test-unknown', 'zotero-fulltext-prefs-proxy-test-failed', 'zotero-fulltext-prefs-proxy-test-needs-address',
    'zotero-fulltext-prefs-problem-no-method', 'zotero-fulltext-prefs-problem-saml-entity-id',
  ]) assert.ok(ids.has(id), id);
});

// --- SAML identity-provider search ----------------------------------------------------------------------------------------

test('samlSearchUrl / samlQueryOk: only the trimmed, encoded search text goes into the address; under 3 characters is not sent', () => {
  const { C } = configEnv();
  assert.equal(C.samlSearchUrl('  Queen Mary '), 'https://md.seamlessaccess.org/entities/?q=Queen%20Mary');
  assert.equal(C.samlSearchUrl("King's & co"), 'https://md.seamlessaccess.org/entities/?q=King\'s%20%26%20co');
  assert.equal(C.samlSearchUrl(undefined), 'https://md.seamlessaccess.org/entities/?q=');
  assert.deepEqual(['', ' ab ', 'abc', '  abc  ', null].map((q) => C.samlQueryOk(q)), [false, false, true, true, false]);
});

test('parseSamlSearch(oxford): test/hidden IdPs are dropped, UK federation members come first, OpenAthens-registered ones follow', () => {
  const { C } = configEnv();
  const raw = JSON.parse(fixture('mdq-oxford.json'));
  assert.equal(raw.length, 8);
  const hidden = raw.filter((e) => e.hidden === 'true').length;
  assert.equal(hidden, 3);
  const res = plain(C.parseSamlSearch(fixture('mdq-oxford.json')));
  assert.equal(res.length, raw.length - hidden);
  assert.deepEqual(res.map((e) => e.entityID), [
    'https://idp.brookes.ac.uk/idp/shibboleth', 'https://registry.shibboleth.ox.ac.uk/idp',
    'https://idp.mcsoxford.org/entity', 'https://idp.oup.com/openathens', 'https://idp.oxcis.ac.uk/entity',
  ]);
  assert.deepEqual(res.map((e) => e.ukFederation), [true, true, false, false, false]);
  const oxford = res.find((e) => e.entityID === 'https://registry.shibboleth.ox.ac.uk/idp');
  assert.deepEqual(oxford, { entityID: 'https://registry.shibboleth.ox.ac.uk/idp', title: 'University of Oxford', domain: 'ox.ac.uk', ukFederation: true });
  assert.ok(!res.some((e) => /test/i.test(e.entityID) && e.entityID.includes('iamtest')), 'the IAMTEST IdP is hidden');
  // The parsed array works as well as the text.
  assert.deepEqual(plain(C.parseSamlSearch(raw)), res);
  // A multi-domain entry (OUP) shows its first domain only.
  assert.equal(res.find((e) => e.entityID === 'https://idp.oup.com/openathens').domain, '72259815.sp1.oup.com');
});

test('parseSamlSearch(queen mary): the real QMUL IdP is first among the visible ones; its test IdP is dropped', () => {
  const { C } = configEnv();
  const res = plain(C.parseSamlSearch(fixture('mdq-queen-mary.json')));
  assert.deepEqual(res.map((e) => e.entityID), ['https://idp.shibboleth.qmul.ac.uk/idp/shibboleth', 'https://idp.qmc.ac.uk/shibboleth']);
  assert.equal(res[0].title, 'Queen Mary University of London');
  assert.equal(res[0].domain, 'qmul.ac.uk');
});

test('parseSamlSearch is total: empty, garbage, odd entries, duplicates, hide-from-discovery, non-IdPs and the 30 cap', () => {
  const { C } = configEnv();
  const parse = (x) => plain(C.parseSamlSearch(x));
  assert.deepEqual(parse(fixture('mdq-empty.json')), []);
  for (const bad of ['', 'not json', '{"a":1}', 'null', 'undefined', null, undefined, 42, {}]) assert.deepEqual(parse(bad), [], String(bad));
  const idp = (id, extra) => ({ type: 'idp', hidden: 'false', entityID: id, title: 'T ' + id, ...extra });
  const out = parse([
    null, 'x', 7, idp('https://a.example/idp'), idp('https://a.example/idp'),                       // duplicate
    idp('https://b.example/idp', { entity_category: ['http://refeds.org/category/hide-from-discovery'] }),
    { ...idp('https://c.example/idp'), type: 'sp' },                                                 // not an IdP
    idp('not an entity id'), idp(''), { type: 'idp', title: 'no id' },
    { entity_id: 'https://d.example/idp', title_langs: { en: 'D University' }, domain: 'd.example', registrationAuthority: ['http://ukfederation.org.uk'] },
    idp('urn:mace:ac.uk:sdss.ac.uk:provider:identity:dur.ac.uk', { title: '   ', title_langs: { en: 'Durham' }, scope: 'dur.ac.uk' }),
  ]);
  assert.deepEqual(out.map((e) => e.entityID), ['https://d.example/idp', 'https://a.example/idp', 'urn:mace:ac.uk:sdss.ac.uk:provider:identity:dur.ac.uk']);
  assert.equal(out[0].title, 'D University');
  assert.equal(out[2].title, 'Durham');
  assert.equal(out[2].domain, 'dur.ac.uk');
  const many = Array.from({ length: 80 }, (_, i) => idp(`https://idp${i}.example/shib`));
  assert.equal(parse(many).length, 30);
});

// --- EZproxy prefix parsing ------------------------------------------------------------------------------------------------

test('parseProxyPrefix: ?url= and ?qurl= forms, with or without scheme, target, trailing slash; hostname vs login-subdomain suffixes', () => {
  const { C } = configEnv();
  const p = (raw) => plain(C.parseProxyPrefix(raw));
  const ok = (loginHost, suffix, param) => ({ ok: true, loginHost, suffix, param, prefix: `https://${loginHost}/login?url=` });
  assert.deepEqual(p('https://ezproxy.example.ac.uk/login?url='), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'url'));
  assert.deepEqual(p('https://ezproxy.example.ac.uk/login?qurl='), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'qurl'));
  assert.deepEqual(p('ezproxy.example.ac.uk/login?url='), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'url'));
  assert.deepEqual(p('  HTTPS://EZproxy.Example.AC.uk/Login/?URL=https://www.nature.com/articles/x  '), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'url'));
  assert.deepEqual(p('http://ezproxy.example.ac.uk'), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'url'), 'host only');
  assert.deepEqual(p('https://ezproxy.example.ac.uk/login'), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'url'));
  assert.deepEqual(p('https://ezproxy.example.ac.uk/login?foo=1&qurl=https%3A%2F%2Fwww.nature.com%2F#frag'), ok('ezproxy.example.ac.uk', 'ezproxy.example.ac.uk', 'qurl'));
  // Hosted proxies: the sign-in host is "login.<name>", proxied hosts hang off "<name>"; without "login." both are the host.
  assert.deepEqual(p('https://login.kcl.idm.oclc.org/login?url='), ok('login.kcl.idm.oclc.org', 'kcl.idm.oclc.org', 'url'));
  assert.deepEqual(p('https://login.eux.idm.oclc.org/login?url='), ok('login.eux.idm.oclc.org', 'eux.idm.oclc.org', 'url'));
  assert.deepEqual(p('http://soton.idm.oclc.org/login?url='), ok('soton.idm.oclc.org', 'soton.idm.oclc.org', 'url'));
  assert.deepEqual(p('https://qub.idm.oclc.org/login?url='), ok('qub.idm.oclc.org', 'qub.idm.oclc.org', 'url'));
  assert.deepEqual(p('https://login.example.org/login?url='), ok('login.example.org', 'example.org', 'url'), 'login.<domain> is a sign-in host');
  assert.deepEqual(p('https://login.example/login?url='), ok('login.example', 'login.example', 'url'), 'but login.<single label> keeps its own suffix');
  assert.deepEqual(p('https://login.proxy.example.ac.uk/login?url='), ok('login.proxy.example.ac.uk', 'proxy.example.ac.uk', 'url'));
});

test('parseProxyPrefix rejects non-proxy input with a specific reason: path proxies, ports, credentials, bad hosts and schemes', () => {
  const { C } = configEnv();
  const err = (raw) => plain(C.parseProxyPrefix(raw)).error;
  assert.equal(err('https://www.library.example.ac.uk/proxy/login?url='), 'zotero-fulltext-prefs-proxy-invalid-path');
  assert.equal(err('https://library.example.ac.uk/ezproxy?url='), 'zotero-fulltext-prefs-proxy-invalid-path');
  assert.equal(err('https://ezproxy.example.ac.uk:2443/login?url='), 'zotero-fulltext-prefs-proxy-invalid-port');
  assert.equal(err('ezproxy.example.ac.uk:2048'), 'zotero-fulltext-prefs-proxy-invalid-port');
  assert.equal(err('https://me:secret@ezproxy.example.ac.uk/login?url='), 'zotero-fulltext-prefs-proxy-invalid-credentials');
  for (const bad of ['', '   ', null, undefined, 'ftp://ezproxy.example.ac.uk/login?url=', 'https://localhost/login?url=', 'not a proxy', 'https://ezproxy..example.ac.uk/login', 'javascript:alert(1)', 'https://']) {
    assert.equal(err(bad), 'zotero-fulltext-prefs-proxy-invalid', String(bad));
  }
  const ids = ftlIds();
  for (const bad of ['', 'https://x.example.ac.uk:1/login', 'https://a@x.example.ac.uk', 'https://x.example.ac.uk/a/b']) assert.ok(ids.has(err(bad)), bad);
});

test('proxyPrefix / proxyTestUrl: the display form and the unauthenticated probe address', () => {
  const { C } = configEnv();
  assert.equal(C.proxyPrefix('ezproxy.example.ac.uk'), 'https://ezproxy.example.ac.uk/login?url=');
  assert.equal(C.proxyTestUrl('login.kcl.idm.oclc.org'), 'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.nature.com%2F');
});

test('classifyProxyResponse: EZproxy / Shibboleth sign-in page or a redirect to a login = recognised; a plain page or 404 = unknown; the rest cannot be judged', () => {
  const { C } = configEnv();
  const c = (s, t, l) => C.classifyProxyResponse(s, t, l);
  assert.equal(c(200, '<title>Shibboleth Authentication Request</title>'), 'recognised');
  assert.equal(c(200, '<h1>Login to EZproxy</h1>'), 'recognised');
  assert.equal(c(200, '<html>Welcome to our library catalogue</html>'), 'unknown');
  assert.equal(c(200, ''), 'unknown');
  assert.equal(c(302, '', 'https://login.eux.idm.oclc.org/login?qurl=x'), 'recognised');
  assert.equal(c(302, '', 'https://idp.example.ac.uk/idp/profile/SAML2/Redirect/SSO'), 'recognised');
  assert.equal(c(301, '', 'https://www.example.ac.uk/'), 'failed', 'a redirect to the home page is not a proxy');
  assert.equal(c(404), 'unknown');
  for (const s of [0, 403, 429, 500, 503]) assert.equal(c(s), 'failed', String(s));
});

// --- applyPreset switches the methods of the preset on ---------------------------------------------------------------------

test('applyPreset(imperial) writes the IdP, no OpenAthens domain, no proxy, and switches Shibboleth on', () => {
  const { C, pref, zotero } = configEnv();
  zotero.Prefs.set(BRANCH + 'inst.use_saml', false);
  C.applyPreset('imperial');
  assert.strictEqual(pref('inst.use_saml'), true);
  assert.strictEqual(pref('institution.saml_entity_id'), 'https://shibboleth.imperial.ac.uk/shibboleth');
  assert.strictEqual(pref('institution.openathens_domain'), '');
  assert.strictEqual(pref('institution.ezproxy_login_host'), '');
  assert.strictEqual(pref('institution.ezproxy_all_hosts'), false);
  assert.deepEqual(plain(C.currentMethods()), ['saml']);
  assert.equal(C.effectivePreset(), 'imperial');
  assert.deepEqual(plain(C.problems()), []);
});

test('applyPreset(edinburgh): Shibboleth plus the OCLC-hosted library proxy for every publisher; moving on to Imperial leaves nothing behind', () => {
  const { C, pref } = configEnv();
  C.applyPreset('edinburgh');
  assert.deepEqual(plain(C.currentMethods()), ['saml', 'ezproxy']);
  assert.strictEqual(pref('institution.ezproxy_login_host'), 'login.eux.idm.oclc.org');
  assert.strictEqual(pref('institution.ezproxy_suffix'), 'eux.idm.oclc.org');
  assert.strictEqual(pref('institution.ezproxy_all_hosts'), true);
  const cfg = plain(C.buildEngineConfig());
  assert.deepEqual(cfg.institution.ezproxy, { login_host: 'login.eux.idm.oclc.org', suffix: 'eux.idm.oclc.org' });
  assert.equal(cfg.institution.ezproxy_all_hosts, true);
  C.applyPreset('imperial');
  assert.strictEqual(pref('institution.ezproxy_login_host'), '');
  assert.strictEqual(pref('institution.ezproxy_all_hosts'), false);
  assert.ok(!JSON.stringify(plain(C.buildEngineConfig()).institution).includes('eux'));
});

test('applyPreset leaves a method the user switched off alone unless the preset offers it; an OpenAthens-only preset keeps Shibboleth as it was', () => {
  const { C, pref, zotero } = configEnv();
  zotero.Prefs.set(BRANCH + 'inst.use_ezproxy', false);
  C.applyPreset('city');                       // OpenAthens only
  assert.strictEqual(pref('inst.use_ezproxy'), false);
  assert.strictEqual(pref('inst.use_openathens'), true);
  assert.deepEqual(plain(C.currentMethods()), ['openathens']);
  C.applyPreset('kcl');                        // offers all three
  assert.strictEqual(pref('inst.use_ezproxy'), true);
});

// --- the preferences pane --------------------------------------------------------------------------------------------------

function paneSetup(opts = {}) {
  const requests = [];
  const calls = [];
  const http = {
    request: async (method, url, options) => {
      requests.push({ method, url, options });
      if (opts.respond) return opts.respond(url, options);
      return { status: 200, responseText: '[]' };
    },
  };
  const zotero = makeFakeZotero({ HTTP: http, File: { reveal: async () => {} } });
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  ZFT.runs = { reconfigure: async () => { calls.push(['reconfigure']); } };
  ZFT.state = { reset: async () => {}, init: async () => {}, dataDir: () => '/profile/zotero-fulltext' };
  zotero.ZoteroFullText = ZFT;
  const dom = makePaneDocument();
  const win = makePaneWindow();
  const ctx = makeContext({ document: dom.document, window: win, Zotero: zotero, Services: { prompt: { confirm: () => true } } });
  runScript(ctx, 'prefs-pane.js');
  return { P: ctx.ZoteroFullTextPrefs, ZFT, C: ZFT.config, zotero, calls, requests, win, ...dom };
}

const idOf = (key) => 'zft-pref-' + key.replace(/[._]/g, '-');
const control = (env, key) => env.byId(idOf(key));
const pref = (env, key) => env.zotero.Prefs.store.get(BRANCH + key);
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const choose = async (env, value) => { const sel = control(env, 'institution.preset'); sel.value = value; await sel.fire('change'); await settle(); };
const selectMethod = async (env, method) => { const sel = env.byId('zft-institution-method'); sel.value = method; await sel.fire('change'); await settle(); };
const click = async (env, id) => { await env.byId(id).fire('click'); await settle(); };
const typeInto = async (env, id, value) => { const c = env.byId(id); c.value = value; await c.fire('change'); await settle(); };
const statusId = (env) => env.byId('zft-prefs-status').getAttribute('data-l10n-id');
const lastL10n = (env, id) => env.l10nCalls.filter((c) => c[0] === id).pop();
const summaryMethods = (env) => env.byId('zft-institution-methods').children.filter((c) => c.getAttribute('data-method')).map((c) => c.getAttribute('data-method'));
const visiblePanels = (env) => ['openathens', 'saml', 'ezproxy'].filter((m) => env.byId('zft-method-' + m).hidden === false);

test('pane: a method selector with OpenAthens, Shibboleth and library proxy; one panel open at a time; King\'s opens on OpenAthens', () => {
  const env = paneSetup();
  env.P.init();
  const sel = env.byId('zft-institution-method');
  assert.equal(sel.localName, 'select');
  assert.deepEqual(sel.children.map((o) => o.getAttribute('value')), ['openathens', 'saml', 'ezproxy']);
  assert.deepEqual(sel.children.map((o) => o.getAttribute('data-l10n-id')), [
    'zotero-fulltext-prefs-method-openathens', 'zotero-fulltext-prefs-method-saml', 'zotero-fulltext-prefs-method-ezproxy',
  ]);
  assert.deepEqual(visiblePanels(env), ['openathens']);
  assert.ok(control(env, 'inst.use_saml'), 'the "use Shibboleth" checkbox is in the Institutional access group');
  assert.equal(control(env, 'inst.use_saml').checked, true);
  assert.ok(control(env, 'institution.ezproxy_all_hosts'));
  assert.ok(control(env, 'institution.saml_entity_id'));
});

test('pane: choosing a method shows that panel only', async () => {
  const env = paneSetup();
  env.P.init();
  await selectMethod(env, 'saml');
  assert.deepEqual(visiblePanels(env), ['saml']);
  await selectMethod(env, 'ezproxy');
  assert.deepEqual(visiblePanels(env), ['ezproxy']);
  await selectMethod(env, 'bogus');
  assert.deepEqual(visiblePanels(env), ['ezproxy'], 'an unknown value is ignored');
  await selectMethod(env, 'openathens');
  assert.deepEqual(visiblePanels(env), ['openathens']);
});

test('pane: the read-only "Uses:" line lists the active methods of a preset and follows the use-flags', async () => {
  const env = paneSetup();
  env.P.init();
  const line = env.byId('zft-institution-methods');
  assert.equal(line.hidden, false);
  assert.equal(line.children[0].getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-methods-uses');
  assert.deepEqual(summaryMethods(env), ['openathens', 'saml', 'ezproxy']);
  assert.deepEqual(line.children.filter((c) => c.getAttribute('data-method')).map((c) => c.getAttribute('data-l10n-id')), [
    'zotero-fulltext-prefs-method-openathens', 'zotero-fulltext-prefs-method-saml', 'zotero-fulltext-prefs-method-ezproxy',
  ]);
  await choose(env, 'imperial');
  assert.deepEqual(summaryMethods(env), ['saml']);
  await choose(env, 'edinburgh');
  assert.deepEqual(summaryMethods(env), ['saml', 'ezproxy']);
  // Switching a method off in the Institutional access group updates the line.
  const box = control(env, 'inst.use_ezproxy');
  box.checked = false;
  await box.fire('change');
  await settle();
  assert.deepEqual(summaryMethods(env), ['saml']);
  // None hides it.
  await choose(env, 'none');
  assert.equal(line.hidden, true);
});

test('pane: choosing Imperial writes the IdP, no OpenAthens domain and no proxy, and raises no problem', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'imperial');
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), 'https://shibboleth.imperial.ac.uk/shibboleth');
  assert.strictEqual(pref(env, 'institution.openathens_domain'), '');
  assert.equal(control(env, 'institution.saml_entity_id').value, 'https://shibboleth.imperial.ac.uk/shibboleth');
  assert.equal(control(env, 'institution.preset').value, 'imperial');
  assert.equal(env.byId('zft-prefs-problems').hidden, true);
  assert.equal(statusId(env), 'zotero-fulltext-prefs-institution-applied');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-institution-applied')[1]), { name: 'Imperial College London' });
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.saml_entity_id, 'https://shibboleth.imperial.ac.uk/shibboleth');
});

test('pane: Other with no method shows the "no sign-in method" problem; adding any one of the three clears it', async () => {
  for (const [method, setup] of [
    ['openathens', async (env) => { await typeInto(env, idOf('institution.openathens_domain'), 'test.ac.uk'); }],
    ['saml', async (env) => { await typeInto(env, idOf('institution.saml_entity_id'), 'https://idp.test.ac.uk/shibboleth'); }],
    ['ezproxy', async (env) => { await typeInto(env, 'zft-proxy-prefix', 'https://ezproxy.test.ac.uk/login?url='); }],
  ]) {
    const env = paneSetup();
    env.P.init();
    await choose(env, 'other');
    assert.deepEqual(plain(env.byId('zft-prefs-problems').children.map((p) => p.getAttribute('data-l10n-id'))), ['zotero-fulltext-prefs-problem-no-method'], method);
    assert.equal(env.byId('zft-institution-methods').children[0].getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-methods-none');
    await setup(env);
    assert.equal(env.byId('zft-prefs-problems').hidden, true, method);
    assert.deepEqual(summaryMethods(env), [method], method);
  }
});

test('pane: the method that already has details is the one the Other section opens on', () => {
  const saml = paneSetup();
  for (const [k, v] of Object.entries({ 'institution.name': 'Test U', 'institution.openathens_domain': '', 'institution.ezproxy_login_host': '', 'institution.ezproxy_suffix': '' })) saml.zotero.Prefs.set(BRANCH + k, v);
  saml.P.init();
  assert.deepEqual(visiblePanels(saml), ['saml']);
  assert.equal(saml.byId('zft-institution-method').value, 'saml');
  const proxy = paneSetup();
  for (const [k, v] of Object.entries({ 'institution.name': 'Test U', 'institution.openathens_domain': '', 'institution.saml_entity_id': '' })) proxy.zotero.Prefs.set(BRANCH + k, v);
  proxy.P.init();
  assert.deepEqual(visiblePanels(proxy), ['ezproxy']);
});

test('pane: Shibboleth search queries the public directory once, anonymously, lists visible IdPs, and the pick stores the entityID', async () => {
  const env = paneSetup({ respond: (url) => ({ status: 200, responseText: fixture('mdq-oxford.json') }) });
  env.P.init();
  await choose(env, 'other');
  await selectMethod(env, 'saml');
  assert.equal(env.byId('zft-saml-results').hidden, true);
  assert.equal(env.byId('zft-saml-use').hidden, true);
  assert.equal(env.byId('zft-saml-chosen').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-saml-chosen-none');
  env.byId('zft-saml-query').value = '  oxford ';
  await click(env, 'zft-saml-search');
  assert.equal(env.requests.length, 1);
  const { method, url, options } = env.requests[0];
  assert.equal(method, 'GET');
  assert.equal(url, 'https://md.seamlessaccess.org/entities/?q=oxford');
  assert.equal(options.anon, true, 'no cookies');
  assert.equal(options.followRedirects, undefined);
  for (const key of ['body', 'username', 'password']) assert.equal(options[key], undefined, key);
  assert.equal(options.headers.Accept, 'application/json');
  const results = env.byId('zft-saml-results');
  assert.equal(results.hidden, false);
  assert.equal(env.byId('zft-saml-use').hidden, false);
  assert.equal(results.children.length, 5);
  assert.deepEqual(results.children.map((o) => o.getAttribute('value')).slice(0, 2), ['https://idp.brookes.ac.uk/idp/shibboleth', 'https://registry.shibboleth.ox.ac.uk/idp']);
  assert.equal(results.children[1].textContent, 'University of Oxford (ox.ac.uk)');
  assert.equal(env.byId('zft-saml-status').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-saml-found');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-saml-found')[1]), { count: 5 });
  assert.equal(env.byId('zft-saml-search').disabled, false);
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), '', 'searching stores nothing');

  results.value = 'https://registry.shibboleth.ox.ac.uk/idp';
  await click(env, 'zft-saml-use');
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), 'https://registry.shibboleth.ox.ac.uk/idp');
  assert.strictEqual(pref(env, 'inst.use_saml'), true);
  assert.strictEqual(pref(env, 'institution.name'), 'University of Oxford', 'an empty name is filled with the IdP title');
  assert.match(pref(env, 'institution.idp_hosts'), /registry\.shibboleth\.ox\.ac\.uk/);
  assert.equal(control(env, 'institution.saml_entity_id').value, 'https://registry.shibboleth.ox.ac.uk/idp');
  assert.equal(control(env, 'institution.name').value, 'University of Oxford');
  assert.equal(statusId(env), 'zotero-fulltext-prefs-saml-picked');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-saml-picked')[1]), { name: 'University of Oxford' });
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-saml-chosen')[1]), { name: 'University of Oxford' });
  assert.deepEqual(summaryMethods(env), ['saml']);
  assert.equal(env.byId('zft-prefs-problems').hidden, true);
  assert.equal(env.calls.filter((c) => c[0] === 'reconfigure').length >= 1, true);
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.saml_entity_id, 'https://registry.shibboleth.ox.ac.uk/idp');
  // Reopen: the chosen IdP is still shown (by its entityID, as the title is not stored).
  env.P.destroy();
  env.P.init();
  assert.equal(control(env, 'institution.saml_entity_id').value, 'https://registry.shibboleth.ox.ac.uk/idp');
  assert.deepEqual(visiblePanels(env), ['saml']);
});

test('pane: picking an IdP keeps a name that is already set; a pick of a preset\'s IdP is shown under the preset\'s name', async () => {
  const env = paneSetup({ respond: () => ({ status: 200, responseText: fixture('mdq-queen-mary.json') }) });
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, idOf('institution.name'), 'My own label');
  await selectMethod(env, 'saml');
  await typeInto(env, 'zft-saml-query', 'queen mary');
  await click(env, 'zft-saml-search');
  await click(env, 'zft-saml-use');            // first result is selected by default
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), 'https://idp.shibboleth.qmul.ac.uk/idp/shibboleth');
  assert.strictEqual(pref(env, 'institution.name'), 'My own label');
  // After a reopen the entityID belongs to the QMUL preset, so the label is the preset's name.
  env.P.destroy();
  env.P.init();
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-saml-chosen').slice(1)[0]), { name: 'Queen Mary University of London' });
});

test('pane: a short search text sends nothing; the Enter key searches; no match, an HTTP error and a network error are reported', async () => {
  const env = paneSetup({ respond: () => ({ status: 200, responseText: fixture('mdq-empty.json') }) });
  env.P.init();
  await choose(env, 'other');
  await selectMethod(env, 'saml');
  await typeInto(env, 'zft-saml-query', 'ox');
  await click(env, 'zft-saml-search');
  assert.equal(env.requests.length, 0);
  assert.equal(env.byId('zft-saml-status').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-saml-query-short');
  env.byId('zft-saml-query').value = 'zzzzqqqq';
  await env.byId('zft-saml-query').fire('keydown', { key: 'Enter' });
  await settle();
  assert.equal(env.requests.length, 1);
  assert.equal(env.byId('zft-saml-status').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-saml-no-results');
  assert.equal(env.byId('zft-saml-results').hidden, true);
  await env.byId('zft-saml-query').fire('keydown', { key: 'a' });
  await settle();
  assert.equal(env.requests.length, 1, 'other keys do nothing');

  const http = paneSetup({ respond: () => ({ status: 503, responseText: '' }) });
  http.P.init();
  await choose(http, 'other');
  await typeInto(http, 'zft-saml-query', 'oxford');
  await click(http, 'zft-saml-search');
  assert.equal(http.byId('zft-saml-status').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-saml-failed');
  assert.deepEqual(plain(lastL10n(http, 'zotero-fulltext-prefs-saml-failed')[1]), { reason: 'HTTP 503' });

  const down = paneSetup({ respond: () => { throw new Error('offline'); } });
  down.P.init();
  await choose(down, 'other');
  await typeInto(down, 'zft-saml-query', 'oxford');
  await click(down, 'zft-saml-search');
  assert.deepEqual(plain(lastL10n(down, 'zotero-fulltext-prefs-saml-failed')[1]), { reason: 'offline' });
  assert.equal(down.byId('zft-saml-search').disabled, false);
  assert.strictEqual(pref(down, 'institution.saml_entity_id'), '', 'a failed search stores nothing');
});

test('pane: the entityID field accepts a valid ID and refuses garbage (control restored, message shown, nothing stored)', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, idOf('institution.saml_entity_id'), '  https://idp.test.ac.uk/shibboleth ');
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), 'https://idp.test.ac.uk/shibboleth');
  await typeInto(env, idOf('institution.saml_entity_id'), 'not an id');
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), 'https://idp.test.ac.uk/shibboleth');
  assert.equal(control(env, 'institution.saml_entity_id').value, 'https://idp.test.ac.uk/shibboleth');
  assert.equal(statusId(env), 'zotero-fulltext-prefs-saml-entity-invalid');
  assert.deepEqual(plain(env.P.coerce({ widget: 'text', format: 'entityid' }, 'urn:mace:x')), { ok: true, value: 'urn:mace:x' });
  assert.deepEqual(plain(env.P.coerce({ widget: 'text', format: 'entityid' }, '')), { ok: true, value: '' });
  await typeInto(env, idOf('institution.saml_entity_id'), '');
  assert.strictEqual(pref(env, 'institution.saml_entity_id'), '', 'empty removes it');
});

test('pane: a pasted proxy address is parsed into login host + suffix; ?qurl= and hosted login.<name> forms work', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await selectMethod(env, 'ezproxy');
  await typeInto(env, 'zft-proxy-prefix', ' HTTPS://ezproxy.Example.ac.uk/login?qurl= ');
  assert.strictEqual(pref(env, 'institution.ezproxy_login_host'), 'ezproxy.example.ac.uk');
  assert.strictEqual(pref(env, 'institution.ezproxy_suffix'), 'ezproxy.example.ac.uk');
  assert.strictEqual(pref(env, 'inst.use_ezproxy'), true);
  assert.equal(env.byId('zft-proxy-prefix').value, 'https://ezproxy.example.ac.uk/login?url=', 'shown in canonical form');
  assert.equal(control(env, 'institution.ezproxy_login_host').value, 'ezproxy.example.ac.uk');
  assert.equal(control(env, 'institution.ezproxy_suffix').value, 'ezproxy.example.ac.uk');
  assert.equal(statusId(env), 'zotero-fulltext-prefs-proxy-saved');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-proxy-saved')[1]), { host: 'ezproxy.example.ac.uk' });
  assert.deepEqual(plain(env.ZFT.config.buildEngineConfig()).institution.ezproxy, { login_host: 'ezproxy.example.ac.uk', suffix: 'ezproxy.example.ac.uk' });
  assert.deepEqual(summaryMethods(env), ['ezproxy']);

  await typeInto(env, 'zft-proxy-prefix', 'https://login.kcl.idm.oclc.org/login?url=');
  assert.strictEqual(pref(env, 'institution.ezproxy_suffix'), 'kcl.idm.oclc.org');
  assert.equal(env.byId('zft-prefs-problems').hidden, true);
});

test('pane: "Use the proxy for every publisher" writes institution.ezproxy_all_hosts and reaches the engine config', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  const box = control(env, 'institution.ezproxy_all_hosts');
  assert.equal(box.type, 'checkbox');
  assert.equal(box.checked, false);
  box.checked = true;
  await box.fire('change');
  await settle();
  assert.strictEqual(pref(env, 'institution.ezproxy_all_hosts'), true);
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.ezproxy_all_hosts, true);
  assert.equal(control(env, 'institution.preset').value, 'other');
  box.checked = false;
  await box.fire('change');
  assert.strictEqual(pref(env, 'institution.ezproxy_all_hosts'), false);
});

test('pane: a bad proxy address is refused with its reason, the field snaps back, nothing changes; an empty field removes the proxy', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, 'zft-proxy-prefix', 'https://ezproxy.example.ac.uk/login?url=');
  for (const [bad, id] of [
    ['https://library.example.ac.uk/proxy/login?url=', 'zotero-fulltext-prefs-proxy-invalid-path'],
    ['https://ezproxy.example.ac.uk:2443/login?url=', 'zotero-fulltext-prefs-proxy-invalid-port'],
    ['https://a:b@ezproxy.example.ac.uk/login?url=', 'zotero-fulltext-prefs-proxy-invalid-credentials'],
    ['hello world', 'zotero-fulltext-prefs-proxy-invalid'],
  ]) {
    await typeInto(env, 'zft-proxy-prefix', bad);
    assert.equal(statusId(env), id, bad);
    assert.equal(env.byId('zft-proxy-prefix').value, 'https://ezproxy.example.ac.uk/login?url=', bad);
    assert.strictEqual(pref(env, 'institution.ezproxy_login_host'), 'ezproxy.example.ac.uk', bad);
  }
  await typeInto(env, 'zft-proxy-prefix', '');
  assert.strictEqual(pref(env, 'institution.ezproxy_login_host'), '');
  assert.strictEqual(pref(env, 'institution.ezproxy_suffix'), '');
  assert.equal(plain(env.ZFT.config.buildEngineConfig()).institution.ezproxy, null);
  assert.equal(env.byId('zft-proxy-prefix').value, '');
});

test('pane: editing the login host by hand updates the displayed address', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, idOf('institution.ezproxy_login_host'), 'Proxy.Test.ac.uk');
  assert.equal(env.byId('zft-proxy-prefix').value, 'https://proxy.test.ac.uk/login?url=');
});

test('pane: the proxy Test makes ONE anonymous GET of the login page, no redirects, and reports the verdict', async () => {
  const env = paneSetup({ respond: () => ({ status: 200, responseText: '<title>Shibboleth Authentication Request</title>' }) });
  env.P.init();
  await choose(env, 'other');
  await selectMethod(env, 'ezproxy');
  await typeInto(env, 'zft-proxy-prefix', 'https://ezproxy.example.ac.uk/login?url=');
  await click(env, 'zft-proxy-test');
  assert.equal(env.requests.length, 1);
  const { method, url, options } = env.requests[0];
  assert.equal(method, 'GET');
  assert.equal(url, 'https://ezproxy.example.ac.uk/login?qurl=https%3A%2F%2Fwww.nature.com%2F');
  assert.equal(options.anon, true);
  assert.equal(options.followRedirects, false);
  assert.equal(options.successCodes, false);
  for (const key of ['body', 'headers', 'username', 'password']) assert.equal(options[key], undefined, key);
  const result = env.byId('zft-proxy-test-result');
  assert.equal(result.hidden, false);
  assert.equal(result.getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-proxy-test-ok');
  assert.equal(result.getAttribute('data-state'), 'ok');
  assert.deepEqual(plain(lastL10n(env, 'zotero-fulltext-prefs-proxy-test-ok')[1]), { host: 'ezproxy.example.ac.uk' });
  assert.equal(env.byId('zft-proxy-test').disabled, false);
  assert.equal(env.byId('zft-institution-test-result').hidden, true, 'the OpenAthens result line is separate');
});

test('pane: the proxy Test tests what is typed even before it is saved, and distinguishes unknown, redirect, network error', async () => {
  const unknown = paneSetup({ respond: () => ({ status: 200, responseText: '<html>library home</html>' }) });
  unknown.P.init();
  await choose(unknown, 'other');
  unknown.byId('zft-proxy-prefix').value = 'ezproxy.nowhere.example.ac.uk';
  await click(unknown, 'zft-proxy-test');
  assert.equal(unknown.byId('zft-proxy-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-proxy-test-unknown');
  assert.equal(unknown.requests[0].url, 'https://ezproxy.nowhere.example.ac.uk/login?qurl=https%3A%2F%2Fwww.nature.com%2F');
  assert.strictEqual(pref(unknown, 'institution.ezproxy_login_host'), '', 'a test never changes the prefs');

  const redirect = paneSetup({ respond: () => ({ status: 302, getResponseHeader: (h) => (h === 'Location' ? 'https://login.eux.idm.oclc.org/login?qurl=x' : null) }) });
  redirect.P.init();
  await choose(redirect, 'other');
  await typeInto(redirect, 'zft-proxy-prefix', 'https://ezproxy.is.ed.ac.uk/login?url=');
  await click(redirect, 'zft-proxy-test');
  assert.equal(redirect.byId('zft-proxy-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-proxy-test-ok');

  const down = paneSetup({ respond: () => { throw new Error('getaddrinfo ENOTFOUND'); } });
  down.P.init();
  await choose(down, 'other');
  await typeInto(down, 'zft-proxy-prefix', 'https://ezproxy.example.ac.uk/login?url=');
  await click(down, 'zft-proxy-test');
  assert.equal(down.byId('zft-proxy-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-proxy-test-failed');
  assert.deepEqual(plain(lastL10n(down, 'zotero-fulltext-prefs-proxy-test-failed')[1]), { reason: 'getaddrinfo ENOTFOUND' });

  const odd = paneSetup({ respond: () => ({ status: 503, responseText: '' }) });
  odd.P.init();
  await choose(odd, 'other');
  await typeInto(odd, 'zft-proxy-prefix', 'https://ezproxy.example.ac.uk/login?url=');
  await click(odd, 'zft-proxy-test');
  assert.deepEqual(plain(lastL10n(odd, 'zotero-fulltext-prefs-proxy-test-failed')[1]), { reason: 'HTTP 503' });
});

test('pane: the proxy Test with an empty or invalid address asks for one and makes no request', async () => {
  const env = paneSetup();
  env.P.init();
  await choose(env, 'other');
  await click(env, 'zft-proxy-test');
  assert.equal(env.requests.length, 0);
  assert.equal(env.byId('zft-proxy-test-result').getAttribute('data-l10n-id'), 'zotero-fulltext-prefs-proxy-test-needs-address');
  env.byId('zft-proxy-prefix').value = 'https://x.example.ac.uk:99/login';
  await click(env, 'zft-proxy-test');
  assert.equal(env.requests.length, 0);
});

test('pane: choosing another institution clears every Test result and the search status', async () => {
  const env = paneSetup({ respond: () => ({ status: 200, responseText: '<title>Shibboleth Authentication Request</title>' }) });
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, 'zft-proxy-prefix', 'https://ezproxy.example.ac.uk/login?url=');
  await click(env, 'zft-proxy-test');
  assert.equal(env.byId('zft-proxy-test-result').hidden, false);
  await typeInto(env, 'zft-saml-query', 'ox');
  await click(env, 'zft-saml-search');
  assert.equal(env.byId('zft-saml-status').hidden, false);
  await choose(env, 'kcl');
  assert.equal(env.byId('zft-proxy-test-result').hidden, true);
  assert.equal(env.byId('zft-saml-status').hidden, true);
});

test('pane: closing the pane while a search or a proxy test is out writes nothing and does not throw', async () => {
  let release;
  const env = paneSetup({ respond: () => new Promise((resolve) => { release = () => resolve({ status: 200, responseText: fixture('mdq-oxford.json') }); }) });
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, 'zft-saml-query', 'oxford');
  const pending = env.byId('zft-saml-search').fire('click');
  await settle();
  env.P.destroy();
  release();
  await pending;
  await settle();
  assert.ok(!env.zotero.debugLog.some(([, m]) => /identity provider search failed/.test(m)));
});

test('pane: no listener leaks from the method selector, search, results or proxy controls', async () => {
  const env = paneSetup({ respond: () => ({ status: 200, responseText: fixture('mdq-queen-mary.json') }) });
  const count = () => [...env.body.walk()].reduce((n, e) => n + e.listenerCount(), 0) + env.win.listenerCount();
  env.P.init();
  await choose(env, 'other');
  await typeInto(env, 'zft-saml-query', 'queen mary');
  await click(env, 'zft-saml-search');
  assert.ok(count() > 40);
  env.P.destroy();
  assert.equal(count(), 0);
});

test('pane: init is idempotent with the method sections', () => {
  const env = paneSetup();
  env.P.init();
  const first = env.byId('zft-institution-method');
  env.P.init();
  assert.equal(env.byId('zft-institution-method'), first);
});
