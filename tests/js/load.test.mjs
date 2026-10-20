// Skeleton gate: every plugin/content/*.js in ZFT.LOAD_ORDER loads (vm, fakes) and exposes its declared API.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, makeContext, runScript } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { API } from './helpers/api-manifest.mjs';

test('namespace.js defines ZFT with a LOAD_ORDER', () => {
  const ctx = makeContext();
  runScript(ctx, 'namespace.js');
  assert.equal(ctx.ZFT.ID, 'zotero-fulltext@aborakati');
  assert.equal(ctx.ZFT.PROTOCOL, 1);
  assert.ok(ctx.ZFT.LOAD_ORDER.length >= 15);
  assert.equal(new Set(ctx.ZFT.LOAD_ORDER).size, ctx.ZFT.LOAD_ORDER.length, 'LOAD_ORDER has duplicates');
});

test('all LOAD_ORDER scripts load without touching Zotero at load time', () => {
  // No Zotero/Services/IOUtils globals at all: modules must resolve platform globals at CALL time.
  const { ZFT } = loadPlugin();
  for (const name of Object.keys(API)) {
    assert.ok(ZFT[name], `ZFT.${name} missing`);
  }
});

test('every declared function/constant is exposed', () => {
  const { ZFT } = loadPlugin({ globals: { Zotero: makeFakeZotero() } });
  const missing = [];
  for (const [mod, names] of Object.entries(API)) {
    for (const n of names) if (!(n in ZFT[mod])) missing.push(`${mod}.${n}`);
  }
  assert.deepEqual(missing, []);
});

test('scripts leak nothing into the global scope except ZFT', () => {
  const ctx = makeContext({ Zotero: makeFakeZotero() });
  const before = new Set(Object.keys(ctx));
  runScript(ctx, 'namespace.js');
  for (const f of ctx.ZFT.LOAD_ORDER) runScript(ctx, f);
  const leaked = Object.keys(ctx).filter((k) => !before.has(k) && k !== 'ZFT');
  assert.deepEqual(leaked, []);
});

test('util.l10n fallback returns the id plus args when no Localization exists', () => {
  const { ZFT } = loadPlugin();
  assert.equal(ZFT.l10n.format('zotero-fulltext-menu-get'), 'zotero-fulltext-menu-get');
  assert.equal(ZFT.l10n.format('zotero-fulltext-outcome-failed', { code: 'x' }), 'zotero-fulltext-outcome-failed {code=x}');
});
