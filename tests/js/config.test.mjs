// ZFT.config: defaults -> EngineConfig, clamps, list parsing, null mappings, serde shape, validate(), PANE_LAYOUT.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { ROOT, loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { ftlIds } from './helpers/ftl-ids.mjs';

const FILES = ['util.js', 'config.js', 'harness.js'];

function env(prefOverrides = {}) {
  const zotero = makeFakeZotero();
  const { ZFT, ctx } = loadPlugin({ globals: { Zotero: zotero }, files: FILES });
  for (const [k, v] of Object.entries(prefOverrides)) zotero.Prefs.set(ZFT.PREF_BRANCH + k, v);
  return {
    ZFT, ctx, zotero, C: ZFT.config,
    // Arrays from the vm realm are not deepStrictEqual-comparable with host literals: normalise.
    validate: (c) => plain(ZFT.config.validate(c)),
    problems: (g) => plain(ZFT.config.problems(g)),
  };
}

/** buildEngineConfig fed from a plain object of overrides (everything else = default). */
function build(overrides) {
  const { C } = env();
  return plain(C.buildEngineConfig((k) => (k in overrides ? overrides[k] : undefined)));
}

const KCL_HOSTS = [
  'doi.org', 'dx.doi.org', 'www.bmj.com', 'www.webofscience.com', 'www.liebertpub.com', 'www.jneurosci.org',
  'www.psychiatryonline.org', 'psychiatryonline.org', 'www.atsjournals.org', 'www.ajronline.org', 'www.jospt.org',
  'www.jove.com', 'www.elgaronline.com', 'infoweb.newsbank.com', 'global.factiva.com', 'www.portal.euromonitor.com',
  'institutions.newscientist.com', 'www.lrb.co.uk', 'libguides.kcl.ac.uk',
];

const EXPECTED_DEFAULT = {
  oa: {
    openalex: true, openalex_key: '', europepmc: true, biorxiv: true, pmc_s3: true, europepmc_get_pdf: true,
    medrxiv_early: true, arxiv: true, zotero_oa: true, unpaywall_email: '', semantic_scholar_key: '', core_key: '',
    version_order: ['published', 'accepted', 'submitted'],
  },
  inst: {
    enabled: true, try_direct: true, use_openathens: true, use_saml: true, use_ezproxy: true, check_alma: true, login_mode: 'window',
    auto_open_login: true, login_timeout_sec: 300, group_libraries: 'ask', confirm_above: 10, batch_max: 20,
    hourly_cap: 30, daily_cap: 80, mb_per_hour: 300, min_gap_sec: 5, challenge_windows: true,
  },
  run: { item_concurrency: 4, hedge: true, pdf_max_mb: 100 },
  institution: {
    name: "King's College London", openathens_domain: 'kcl.ac.uk',
    ezproxy: { login_host: 'login.kcl.idm.oclc.org', suffix: 'kcl.idm.oclc.org' },
    ezproxy_hosts: KCL_HOSTS,
    alma: { base: 'https://librarysearch.kcl.ac.uk', inst: '44KCL_INST' },
    libkey_library_id: 286,
    idp_hosts: ['login.microsoftonline.com', 'login.live.com', 'login.openathens.net', 'kclidp.kcl.ac.uk'],
    entra_tenant: '8370cf14-16f3-4c16-b83c-724071654356',
    passthrough_hosts: ['journals.lww.com'],
    warmup_url: '',
    saml_entity_id: 'https://kclidp.kcl.ac.uk/idp/shibboleth',
    ezproxy_all_hosts: false,
  },
  endpoint_map: [],
  trace: false,
};

test('defaults produce the expected EngineConfig (via fake prefs and via an empty reader)', () => {
  const { C } = env();
  assert.deepEqual(plain(C.buildEngineConfig()), EXPECTED_DEFAULT);
  assert.deepEqual(plain(C.buildEngineConfig(() => undefined)), EXPECTED_DEFAULT);
});

test('a throwing pref reader falls back to the defaults and never throws', () => {
  const { C } = env();
  assert.deepEqual(plain(C.buildEngineConfig(() => { throw new Error('prefs gone'); })), EXPECTED_DEFAULT);
});

test('getPref falls back to the default on error or a missing pref; unknown keys are programmer errors', () => {
  const { C, zotero } = env();
  assert.equal(C.getPref('inst.login_timeout_sec'), 300);
  zotero.Prefs.get = () => { throw new Error('boom'); };
  assert.equal(C.getPref('run.item_concurrency'), 4);
  assert.throws(() => C.getPref('nope.nothing'), /unknown pref/);
  assert.throws(() => C.setPref('nope.nothing', 1), /unknown pref/);
});

test('setPref coerces to the key type and writes the full pref name', () => {
  const { C, zotero, ZFT } = env();
  C.setPref('run.item_concurrency', '6');
  C.setPref('run.hedge', 0);
  C.setPref('oa.core_key', 12);
  C.setPref('oa.unpaywall_email', null);
  assert.strictEqual(zotero.Prefs.store.get(ZFT.PREF_BRANCH + 'run.item_concurrency'), 6);
  assert.strictEqual(zotero.Prefs.store.get(ZFT.PREF_BRANCH + 'run.hedge'), false);
  assert.strictEqual(zotero.Prefs.store.get(ZFT.PREF_BRANCH + 'oa.core_key'), '12');
  assert.strictEqual(zotero.Prefs.store.get(ZFT.PREF_BRANCH + 'oa.unpaywall_email'), '');
});

test('integers are clamped into their ranges', () => {
  const cases = [
    ['run.item_concurrency', 0, 1], ['run.item_concurrency', 99, 8], ['run.item_concurrency', 3, 3],
    ['run.pdf_max_mb', 0, 1], ['run.pdf_max_mb', 100000, 500],
    ['inst.login_timeout_sec', 5, 30], ['inst.login_timeout_sec', 99999, 1800],
    ['inst.confirm_above', -3, 0], ['inst.hourly_cap', -1, 0], ['inst.daily_cap', -50, 0],
    ['inst.mb_per_hour', -1, 0], ['inst.min_gap_sec', -9, 0], ['inst.batch_max', 0, 1], ['inst.batch_max', -4, 1],
  ];
  for (const [key, input, expected] of cases) {
    const cfg = build({ [key]: input });
    const [group, field] = key.split('.');
    assert.equal(cfg[group][field], expected, `${key}=${input}`);
  }
});

test('non-numeric and fractional integer prefs: defaults for garbage, rounding for floats', () => {
  assert.equal(build({ 'run.item_concurrency': 'many' }).run.item_concurrency, 4);
  assert.equal(build({ 'run.item_concurrency': '  5 ' }).run.item_concurrency, 5);
  assert.equal(build({ 'run.item_concurrency': NaN }).run.item_concurrency, 4);
  assert.equal(build({ 'run.item_concurrency': 2.6 }).run.item_concurrency, 3);
  assert.equal(build({ 'inst.hourly_cap': Infinity }).inst.hourly_cap, 30);
});

test('lists: trimmed, empties dropped, hosts lower-cased and de-duplicated, pasted URLs reduced to hosts', () => {
  const cfg = build({
    'institution.ezproxy_hosts': ' DOI.org , doi.org,, Www.BMJ.com ,https://www.jove.com/path?q=1, ',
    'institution.idp_hosts': ',,',
    'institution.passthrough_hosts': 'Journals.LWW.com.,journals.lww.com',
  });
  assert.deepEqual(cfg.institution.ezproxy_hosts, ['doi.org', 'www.bmj.com', 'www.jove.com']);
  assert.deepEqual(cfg.institution.idp_hosts, []);
  assert.deepEqual(cfg.institution.passthrough_hosts, ['journals.lww.com']);
});

test('version_order keeps only the known versions, in the user order, and appends the missing ones', () => {
  assert.deepEqual(build({ 'oa.version_order': 'submitted, BOGUS ,Published' }).oa.version_order, ['submitted', 'published', 'accepted']);
  assert.deepEqual(build({ 'oa.version_order': '' }).oa.version_order, ['published', 'accepted', 'submitted']);
  assert.deepEqual(build({ 'oa.version_order': 'accepted,accepted,unknown' }).oa.version_order, ['accepted', 'published', 'submitted']);
});

test('null mappings: ezproxy, alma, libkey id and entra tenant', () => {
  const none = build({
    'institution.ezproxy_login_host': '', 'institution.alma_base': '', 'institution.libkey_library_id': 0,
    'institution.entra_tenant': '   ',
  });
  assert.equal(none.institution.ezproxy, null);
  assert.equal(none.institution.alma, null);
  assert.equal(none.institution.libkey_library_id, null);
  assert.equal(none.institution.entra_tenant, null);
  // A login host without a suffix cannot build proxied host names: also null.
  assert.equal(build({ 'institution.ezproxy_suffix': '' }).institution.ezproxy, null);
  const some = build({ 'institution.alma_base': 'https://alma.example.org///', 'institution.alma_inst': ' X_INST ', 'institution.libkey_library_id': 7 });
  assert.deepEqual(some.institution.alma, { base: 'https://alma.example.org', inst: 'X_INST' });
  assert.equal(some.institution.libkey_library_id, 7);
  assert.equal(build({ 'institution.libkey_library_id': -5 }).institution.libkey_library_id, null);
});

test('unknown login_mode / group_libraries fall back to the default; known ones are normalised', () => {
  assert.equal(build({ 'inst.login_mode': 'teleport' }).inst.login_mode, 'window');
  assert.equal(build({ 'inst.login_mode': ' HANDOFF ' }).inst.login_mode, 'handoff');
  assert.equal(build({ 'inst.login_mode': 7 }).inst.login_mode, 'window');
  assert.equal(build({ 'inst.group_libraries': 'sometimes' }).inst.group_libraries, 'ask');
  assert.equal(build({ 'inst.group_libraries': 'never' }).inst.group_libraries, 'never');
  assert.equal(build({ 'inst.group_libraries': 'allow' }).inst.group_libraries, 'allow');
});

test('booleans tolerate odd stored types; strings are trimmed', () => {
  const cfg = build({ 'oa.openalex': 'false', 'oa.arxiv': 0, 'oa.biorxiv': 'maybe', 'oa.openalex_key': '  k-123  ', 'institution.name': '  Test U ' });
  assert.equal(cfg.oa.openalex, false);
  assert.equal(cfg.oa.arxiv, false);
  assert.equal(cfg.oa.biorxiv, true, 'garbage keeps the default');
  assert.equal(cfg.oa.openalex_key, 'k-123');
  assert.equal(cfg.institution.name, 'Test U');
});

test('trace follows debug.trace; the plugin-only worker pref never reaches the config', () => {
  const cfg = build({ 'debug.trace': true, 'debug.inspect_in_worker': false });
  assert.equal(cfg.trace, true);
  assert.ok(!JSON.stringify(cfg).includes('inspect_in_worker'));
  assert.ok(!JSON.stringify(cfg).includes('inspect'), 'no inspect key anywhere');
});

test('endpoint_map comes from ZFT.harness.endpointMap() and is [] when the harness module is absent', () => {
  const withHarness = env({});
  withHarness.zotero.Prefs.set('extensions.zotero-fulltext.harness.endpoint_map', '[["https://api.openalex.org","http://127.0.0.1:9/oa"]]');
  assert.deepEqual(plain(withHarness.C.buildEngineConfig().endpoint_map), [['https://api.openalex.org', 'http://127.0.0.1:9/oa']]);
  const zotero = makeFakeZotero();
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'config.js'] });
  assert.deepEqual(plain(ZFT.config.buildEngineConfig().endpoint_map), []);
});

// --- serde shape ---------------------------------------------------------------------------------------------------

/** Field names of `pub struct <name> { ... }` in a Rust source (comments stripped). */
function rustStructFields(src, name) {
  const start = src.indexOf(`pub struct ${name} {`);
  assert.ok(start >= 0, `struct ${name} not found in proto.rs`);
  let depth = 0;
  let i = src.indexOf('{', start);
  const from = i;
  for (; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) break;
  }
  const body = src.slice(from + 1, i).replace(/\/\/[^\n]*/g, '');
  return [...body.matchAll(/\bpub\s+(\w+)\s*:/g)].map((m) => m[1]).sort();
}

// The Configure example from docs/design/contracts/proto.rs (test effect_examples_parse), copied verbatim.
const PROTO_CONFIGURE_EXAMPLE = {
  type: 'configure', protocol: 1, seed: 2718281828, persisted: {},
  config: {
    oa: {
      openalex: true, openalex_key: '', europepmc: true, biorxiv: true, pmc_s3: true, europepmc_get_pdf: true,
      medrxiv_early: true, arxiv: true, zotero_oa: true, unpaywall_email: '', semantic_scholar_key: '', core_key: '',
      version_order: ['published', 'accepted', 'submitted'],
    },
    inst: {
      enabled: true, try_direct: true, use_openathens: true, use_ezproxy: true, check_alma: true, login_mode: 'window',
      auto_open_login: true, login_timeout_sec: 300, group_libraries: 'ask', confirm_above: 10, batch_max: 20, hourly_cap: 30,
      daily_cap: 80, mb_per_hour: 300, min_gap_sec: 5, challenge_windows: true,
    },
    run: { item_concurrency: 4, hedge: true, pdf_max_mb: 100 },
    institution: {
      name: "King's College London", openathens_domain: 'kcl.ac.uk',
      ezproxy: { login_host: 'login.kcl.idm.oclc.org', suffix: 'kcl.idm.oclc.org' },
      ezproxy_hosts: ['doi.org', 'www.bmj.com'], alma: { base: 'https://librarysearch.kcl.ac.uk', inst: '44KCL_INST' },
      libkey_library_id: 286, idp_hosts: ['login.microsoftonline.com'], entra_tenant: '8370cf14-16f3-4c16-b83c-724071654356',
      passthrough_hosts: ['journals.lww.com'], warmup_url: '',
    },
  },
};

/** serde(default) fields the proto example may omit. */
const OPTIONAL_TOP_LEVEL = ['endpoint_map', 'trace'];

/**
 * Fields added for the access-method picker (all optional on the Rust side, serde(default)); docs/design/contracts/proto.rs may or
 * may not list them yet, so the shape checks accept both.
 */
const OPTIONAL_NEW = { 'config.inst': ['use_saml'], 'config.institution': ['saml_entity_id', 'ezproxy_all_hosts'] };

function compareShape(example, actual, where) {
  if (Array.isArray(example)) {
    assert.ok(Array.isArray(actual), `${where} must be an array`);
    if (example.length && actual.length) assert.equal(typeof actual[0], typeof example[0], `${where}[0] type`);
    return;
  }
  if (example !== null && typeof example === 'object') {
    assert.ok(actual !== null && typeof actual === 'object', `${where} must be an object`);
    const want = Object.keys(example).sort();
    const have = Object.keys(actual)
      .filter((k) => !(where === 'config' && OPTIONAL_TOP_LEVEL.includes(k)))
      .filter((k) => !(OPTIONAL_NEW[where] || []).includes(k) || k in example).sort();
    assert.deepEqual(have, want, `${where} keys`);
    for (const k of want) compareShape(example[k], actual[k], `${where}.${k}`);
    return;
  }
  assert.equal(typeof actual, typeof example, `${where} type`);
}

test('the default config has exactly the key sets of the proto.rs Configure example (recursively)', () => {
  const { C } = env();
  compareShape(PROTO_CONFIGURE_EXAMPLE.config, plain(C.buildEngineConfig()), 'config');
});

test('every config group has exactly the fields of its Rust struct in docs/design/contracts/proto.rs', () => {
  const src = fs.readFileSync(path.join(ROOT, 'docs', 'design', 'contracts', 'proto.rs'), 'utf8');
  const { C } = env();
  const cfg = plain(C.buildEngineConfig());
  const sorted = (o) => Object.keys(o).sort();
  assert.deepEqual(sorted(cfg), rustStructFields(src, 'EngineConfig'));
  assert.deepEqual(sorted(cfg.oa), rustStructFields(src, 'OaConfig'));
  const withNew = (fields, extra) => [...new Set([...fields, ...extra])].sort();
  assert.deepEqual(sorted(cfg.inst), withNew(rustStructFields(src, 'InstPrefs'), ['use_saml']));
  assert.deepEqual(sorted(cfg.run), rustStructFields(src, 'RunPrefs'));
  assert.deepEqual(sorted(cfg.institution), withNew(rustStructFields(src, 'Institution'), ['saml_entity_id', 'ezproxy_all_hosts']));
  assert.deepEqual(sorted(cfg.institution.ezproxy), rustStructFields(src, 'Ezproxy'));
  assert.deepEqual(sorted(cfg.institution.alma), rustStructFields(src, 'Alma'));
});

test('the config serialises to JSON serde accepts: unsigned 32-bit integers, known enum values, no undefined', () => {
  const { C } = env();
  // Hostile prefs: huge, negative, fractional, wrong types.
  const cfg = plain(C.buildEngineConfig((k) => ({
    'inst.hourly_cap': 1e15, 'inst.daily_cap': -1e15, 'inst.mb_per_hour': 12.5, 'run.pdf_max_mb': '77',
    'institution.libkey_library_id': 1e12, 'inst.login_mode': {}, 'oa.version_order': 42,
  })[k]));
  const u32 = (n) => Number.isInteger(n) && n >= 0 && n <= 4294967295;
  for (const n of [
    cfg.inst.login_timeout_sec, cfg.inst.confirm_above, cfg.inst.batch_max, cfg.inst.hourly_cap, cfg.inst.daily_cap,
    cfg.inst.mb_per_hour, cfg.inst.min_gap_sec, cfg.run.item_concurrency, cfg.run.pdf_max_mb, cfg.institution.libkey_library_id,
  ]) assert.ok(u32(n), String(n));
  assert.ok(['window', 'handoff'].includes(cfg.inst.login_mode));
  assert.ok(['ask', 'never', 'allow'].includes(cfg.inst.group_libraries));
  assert.deepEqual([...cfg.oa.version_order].sort(), ['accepted', 'published', 'submitted']);
  const json = JSON.stringify(cfg);
  assert.ok(!/undefined|NaN|Infinity/.test(json));
});

// --- validate / problems -------------------------------------------------------------------------------------------

test('validate() reports the misconfigurations by l10n id and nothing else', () => {
  const { C, validate, problems } = env();
  assert.deepEqual(validate(C.buildEngineConfig()), []);
  const base = plain(C.buildEngineConfig());
  const withInst = (patch) => ({ ...base, inst: { ...base.inst, ...(patch.inst || {}) }, institution: { ...base.institution, ...(patch.institution || {}) }, oa: { ...base.oa, ...(patch.oa || {}) } });
  const noMethod = { openathens_domain: '  ', saml_entity_id: null, ezproxy: null };
  assert.deepEqual(validate(withInst({ institution: { openathens_domain: '  ' } })), [], "King's still has Shibboleth and the proxy");
  assert.deepEqual(validate(withInst({ institution: noMethod })), ['zotero-fulltext-prefs-problem-no-method']);
  assert.deepEqual(validate(withInst({ inst: { enabled: false }, institution: noMethod })), [], 'no method needed while institutional access is off');
  assert.deepEqual(validate(withInst({ inst: { login_timeout_sec: 29 } })), ['zotero-fulltext-prefs-problem-login-timeout']);
  assert.deepEqual(validate(withInst({ inst: { login_timeout_sec: 30 } })), []);
  assert.deepEqual(validate(withInst({ oa: { unpaywall_email: 'not-an-email' } })), ['zotero-fulltext-prefs-problem-unpaywall-email']);
  assert.deepEqual(validate(withInst({ oa: { unpaywall_email: ' me@kcl.ac.uk ' } })), []);
  assert.deepEqual(validate(withInst({ oa: { unpaywall_email: '' } })), []);
  assert.deepEqual(
    validate(withInst({ institution: noMethod, inst: { login_timeout_sec: 1 }, oa: { unpaywall_email: 'x' } })),
    ['zotero-fulltext-prefs-problem-no-method', 'zotero-fulltext-prefs-problem-login-timeout', 'zotero-fulltext-prefs-problem-unpaywall-email'],
  );
});

test('validate() tolerates partial and empty configs', () => {
  const { C, validate, problems } = env();
  assert.deepEqual(validate({}), []);
  assert.deepEqual(validate(null), []);
  assert.deepEqual(validate({ inst: { enabled: true } }), ['zotero-fulltext-prefs-problem-no-method']);
});

test('problems() also reports a hand-edited login timeout that buildEngineConfig() clamped away', () => {
  const { C, validate, problems } = env();
  const get = (k) => ({ 'inst.login_timeout_sec': 10 })[k];
  assert.equal(C.buildEngineConfig(get).inst.login_timeout_sec, 30, 'the config is clamped');
  assert.deepEqual(problems(get), ['zotero-fulltext-prefs-problem-login-timeout']);
  assert.deepEqual(problems(), []);
  assert.deepEqual(problems((k) => ({
    'institution.openathens_domain': '', 'institution.saml_entity_id': '', 'institution.ezproxy_login_host': '', 'inst.login_timeout_sec': 10,
  })[k]), [
    'zotero-fulltext-prefs-problem-no-method', 'zotero-fulltext-prefs-problem-login-timeout',
  ]);
});

// --- PANE_LAYOUT ---------------------------------------------------------------------------------------------------

test('PANE_LAYOUT: the six groups, every DEFAULTS key in exactly one row', () => {
  const { C } = env();
  const layout = plain(C.PANE_LAYOUT);
  assert.deepEqual(layout.map((g) => g.id), ['oa', 'inst', 'limits', 'run', 'institution', 'debug']);
  const seen = new Map();
  for (const group of layout) for (const row of group.rows) seen.set(row.key, (seen.get(row.key) || 0) + 1);
  const dupes = [...seen].filter(([, n]) => n !== 1).map(([k]) => k);
  assert.deepEqual(dupes, []);
  assert.deepEqual([...seen.keys()].sort(), Object.keys(plain(C.DEFAULTS)).sort());
});

test('PANE_LAYOUT: widgets match the pref types, ranges match LIMITS, secrets are the *_key prefs', () => {
  const { C } = env();
  const allowed = { bool: ['checkbox'], int: ['number'], string: ['text', 'list', 'select'] };
  for (const group of plain(C.PANE_LAYOUT)) {
    for (const row of group.rows) {
      assert.ok(allowed[C.typeOf(row.key)].includes(row.widget), `${row.key}: widget ${row.widget} for type ${C.typeOf(row.key)}`);
      if (row.widget === 'number') {
        const limits = plain(C.LIMITS)[row.key];
        assert.ok(limits, `${row.key} needs LIMITS`);
        assert.deepEqual([row.min, row.max], limits, row.key);
        assert.ok(Number.isInteger(row.min) && Number.isInteger(row.max) && row.min <= row.max);
      } else {
        assert.equal(row.min, undefined, row.key);
        assert.equal(row.max, undefined, row.key);
      }
      assert.equal(!!row.secret, /_key$/.test(row.key), `${row.key} secret flag`);
      assert.equal(row.widget === 'select', Array.isArray(row.options), `${row.key} options`);
    }
  }
});

test('PANE_LAYOUT: select options cover exactly the values buildEngineConfig accepts, and the defaults are among them', () => {
  const { C } = env();
  const rows = Object.fromEntries(plain(C.PANE_LAYOUT).flatMap((g) => g.rows).map((r) => [r.key, r]));
  assert.deepEqual(rows['inst.login_mode'].options.map((o) => o.value), ['window', 'handoff']);
  assert.deepEqual(rows['inst.group_libraries'].options.map((o) => o.value), ['ask', 'never', 'allow']);
  for (const key of ['inst.login_mode', 'inst.group_libraries']) {
    assert.ok(rows[key].options.some((o) => o.value === C.DEFAULTS[key]), key);
    for (const o of rows[key].options) {
      const cfg = plain(C.buildEngineConfig((k) => (k === key ? o.value : undefined)));
      assert.equal(cfg.inst[key.split('.')[1]], o.value, `${key}=${o.value} survives the builder`);
    }
  }
});

test('PANE_LAYOUT: every heading, label and option id exists in the FTL', () => {
  const { C } = env();
  const ids = ftlIds();
  const missing = [];
  for (const group of plain(C.PANE_LAYOUT)) {
    if (!ids.has(group.heading)) missing.push(group.heading);
    for (const row of group.rows) {
      if (!ids.has(row.label)) missing.push(row.label);
      for (const o of row.options || []) if (!ids.has(o.label)) missing.push(o.label);
    }
  }
  assert.deepEqual(missing, []);
});

test('validate() problem ids and the pane texts the shell uses exist in the FTL', () => {
  const ids = ftlIds();
  for (const id of [
    'zotero-fulltext-prefs-problem-no-method', 'zotero-fulltext-prefs-problem-saml-entity-id', 'zotero-fulltext-prefs-problem-login-timeout',
    'zotero-fulltext-prefs-problem-unpaywall-email', 'zotero-fulltext-prefs-title', 'zotero-fulltext-prefs-show-secret',
    'zotero-fulltext-prefs-hide-secret', 'zotero-fulltext-prefs-unavailable', 'zotero-fulltext-prefs-clear-state-confirm',
    'zotero-fulltext-prefs-state-cleared', 'zotero-fulltext-prefs-institution-restored',
  ]) assert.ok(ids.has(id), id);
});

// --- access methods in the EngineConfig ------------------------------------------------------------------------------------

test('buildEngineConfig: saml_entity_id is null when empty, a trimmed string otherwise; ezproxy_all_hosts and use_saml are booleans', () => {
  const empty = build({ 'institution.saml_entity_id': '' });
  assert.strictEqual(empty.institution.saml_entity_id, null);
  assert.strictEqual(build({ 'institution.saml_entity_id': '   ' }).institution.saml_entity_id, null);
  assert.strictEqual(build({ 'institution.saml_entity_id': ' https://idp.example.ac.uk/shibboleth ' }).institution.saml_entity_id, 'https://idp.example.ac.uk/shibboleth');
  assert.strictEqual(build({}).institution.ezproxy_all_hosts, false);
  assert.strictEqual(build({ 'institution.ezproxy_all_hosts': true }).institution.ezproxy_all_hosts, true);
  assert.strictEqual(build({ 'institution.ezproxy_all_hosts': 'true' }).institution.ezproxy_all_hosts, true);
  assert.strictEqual(build({}).inst.use_saml, true);
  assert.strictEqual(build({ 'inst.use_saml': false }).inst.use_saml, false);
  const json = JSON.stringify(build({ 'institution.saml_entity_id': '' }));
  assert.ok(json.includes('"saml_entity_id":null'), 'serialised as null, not omitted');
  assert.ok(!/undefined/.test(json));
});

test('the new access-method prefs exist in DEFAULTS with the expected types and in PANE_LAYOUT', () => {
  const { C } = env();
  assert.strictEqual(C.DEFAULTS['inst.use_saml'], true);
  assert.strictEqual(C.DEFAULTS['institution.ezproxy_all_hosts'], false);
  assert.strictEqual(typeof C.DEFAULTS['institution.saml_entity_id'], 'string');
  assert.equal(C.typeOf('inst.use_saml'), 'bool');
  assert.equal(C.typeOf('institution.ezproxy_all_hosts'), 'bool');
  assert.equal(C.typeOf('institution.saml_entity_id'), 'string');
  const rows = Object.fromEntries(plain(C.PANE_LAYOUT).flatMap((g) => g.rows).map((r) => [r.key, r]));
  assert.equal(rows['inst.use_saml'].widget, 'checkbox');
  assert.equal(rows['institution.saml_entity_id'].format, 'entityid');
  assert.equal(rows['institution.ezproxy_all_hosts'].section, 'proxy');
  assert.equal(rows['institution.openathens_domain'].section, 'openathens');
  assert.ok(!(plain(C.PLUGIN_ONLY).includes('institution.saml_entity_id')), 'forwarded to the engine');
});

test('activeMethods: configured AND switched on, in a fixed order; partial configs count a missing flag as on', () => {
  const { C } = env();
  const cfg = (inst, institution) => plain(C.activeMethods({ inst, institution }));
  const full = { openathens_domain: 'x.ac.uk', saml_entity_id: 'https://idp.x.ac.uk/shibboleth', ezproxy: { login_host: 'a.b.c', suffix: 'b.c' } };
  assert.deepEqual(cfg({}, full), ['openathens', 'saml', 'ezproxy']);
  assert.deepEqual(cfg({ use_saml: false }, full), ['openathens', 'ezproxy']);
  assert.deepEqual(cfg({ use_openathens: false, use_ezproxy: false }, full), ['saml']);
  assert.deepEqual(cfg({}, { openathens_domain: ' ', saml_entity_id: null, ezproxy: null }), []);
  assert.deepEqual(plain(C.activeMethods(null)), []);
  assert.deepEqual(plain(C.currentMethods()), ['openathens', 'saml', 'ezproxy'], "King's defaults");
});

test('validate: no method configured is one problem line only while institutional access is on; a bad entityID is flagged', () => {
  const { C, validate } = env();
  const none = { openathens_domain: '', saml_entity_id: null, ezproxy: null };
  const base = plain(C.buildEngineConfig());
  const mk = (inst, institution) => ({ ...base, inst: { ...base.inst, ...inst }, institution: { ...base.institution, ...institution } });
  assert.deepEqual(validate(mk({}, none)), ['zotero-fulltext-prefs-problem-no-method']);
  assert.deepEqual(validate(mk({ enabled: false }, none)), []);
  assert.deepEqual(validate(mk({}, { ...none, saml_entity_id: 'https://idp.x.ac.uk/shibboleth' })), []);
  assert.deepEqual(validate(mk({}, { ...none, ezproxy: { login_host: 'ezproxy.x.ac.uk', suffix: 'ezproxy.x.ac.uk' } })), []);
  assert.deepEqual(validate(mk({ use_saml: false }, { ...none, saml_entity_id: 'https://idp.x.ac.uk/shibboleth' })), ['zotero-fulltext-prefs-problem-no-method'], 'switched off counts as not set up');
  assert.deepEqual(validate(mk({}, { ...none, openathens_domain: 'not a domain', saml_entity_id: 'idp x' })), [
    'zotero-fulltext-prefs-problem-openathens-domain-format', 'zotero-fulltext-prefs-problem-saml-entity-id',
  ]);
});

test('isEntityId: http(s) URLs and urn: ids only, no whitespace, bounded', () => {
  const { C } = env();
  for (const ok of ['https://idp.example.ac.uk/shibboleth', 'http://idp.example.org/entity', 'urn:mace:ac.uk:sdss.ac.uk:provider:identity:dur.ac.uk']) assert.equal(C.isEntityId(ok), true, ok);
  for (const bad of ['', ' ', null, undefined, 42, 'idp.example.ac.uk', 'ftp://idp.example.ac.uk/x', 'https://', 'https://idp x/y', 'https://idp.example.ac.uk/\nx', 'urn:', 'https://a.example/' + 'x'.repeat(1100)]) assert.equal(C.isEntityId(bad), false, String(bad).slice(0, 40));
});

test('withIdpHost adds the host of an https entityID once and leaves urn: ids alone', () => {
  const { C } = env();
  assert.equal(C.withIdpHost('login.openathens.net', 'https://idp.x.ac.uk/shibboleth'), 'login.openathens.net,idp.x.ac.uk');
  assert.equal(C.withIdpHost('idp.x.ac.uk', 'https://IDP.x.ac.uk/shibboleth'), 'idp.x.ac.uk');
  assert.equal(C.withIdpHost('a.b', 'urn:mace:x'), 'a.b');
  assert.equal(C.withIdpHost('', 'https://idp.x.ac.uk/s'), 'idp.x.ac.uk');
});
