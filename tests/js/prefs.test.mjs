// prefs.js (what Zotero loads) must mirror ZFT.config.DEFAULTS (what the code reads).
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, readPrefsJs, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';

test('prefs.js defines exactly the DEFAULTS keys with identical values and types', () => {
  const { ZFT } = loadPlugin({ globals: { Zotero: makeFakeZotero() } });
  const prefs = readPrefsJs();
  const defaults = plain(ZFT.config.DEFAULTS);
  const expected = new Map(Object.entries(defaults).map(([k, v]) => [ZFT.PREF_BRANCH + k, v]));
  assert.deepEqual([...prefs.keys()].sort(), [...expected.keys()].sort());
  for (const [k, v] of expected) {
    assert.strictEqual(prefs.get(k), v, k);
    assert.ok(['boolean', 'string', 'number'].includes(typeof v), k + ' must be bool/string/int for Zotero setDefaultPrefs');
    if (typeof v === 'number') assert.ok(Number.isInteger(v), k);
  }
});

test('KCL institution defaults match research/04', () => {
  const { ZFT } = loadPlugin();
  const d = plain(ZFT.config.DEFAULTS);
  assert.equal(d['institution.openathens_domain'], 'kcl.ac.uk');
  assert.equal(d['institution.ezproxy_login_host'], 'login.kcl.idm.oclc.org');
  assert.equal(d['institution.ezproxy_suffix'], 'kcl.idm.oclc.org');
  assert.equal(d['institution.alma_inst'], '44KCL_INST');
  assert.equal(d['institution.libkey_library_id'], 286);
  assert.equal(d['institution.entra_tenant'], '8370cf14-16f3-4c16-b83c-724071654356');
  assert.equal(d['institution.ezproxy_hosts'].split(',').length, 19);
});

test('limits default to the licence-friendly values of research/SUMMARY (c)', () => {
  const { ZFT } = loadPlugin();
  const d = plain(ZFT.config.DEFAULTS);
  assert.deepEqual(
    [d['inst.batch_max'], d['inst.confirm_above'], d['inst.hourly_cap'], d['inst.daily_cap'], d['inst.mb_per_hour'], d['inst.min_gap_sec']],
    [20, 10, 30, 80, 300, 5],
  );
});
