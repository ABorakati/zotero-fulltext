// Offline test of the e2e verdict logic (no Zotero): node --test tools/zotero-harness/e2e/evaluate.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const { evaluate, pdfFacts, parseCaseFilter, CASES } = createRequire(import.meta.url)('./plugin/bootstrap.js');

const pdf = { id: 1, contentType: 'application/pdf', size: 250000, head: '%PDF-1.7', pdfMagic: true, eof: true, valid: true };
const saved = { kind: 'saved', route: 'oa', source: 'pmc_s3', version: 'published', url: 'https://x/y.pdf', attachment_key: 'ABCD' };

/** A result in which everything went right. */
function good() {
  const items = CASES.map((c, i) => ({
    key: c.key, label: c.label, doi: c.doi, itemID: 10 + i, expect: c.expect, soft: c.soft,
    outcome: c.expect === 'saved' ? saved : { kind: 'not_found', reason: 'oa_only' },
    trace: [{ src: 'openalex', tier: 'api', class: 'ok', ms: 120 }],
    attachments: c.expect === 'saved' ? [Object.assign({}, pdf)] : [],
  }));
  return {
    meta: { mode: 'full', timeoutSec: 240 },
    zft: { ready: true, version: '0.1.0', readyMs: 3000 },
    menu: { keys: ['a', 'b'], options: [{ target: 'main/library/item', entries: [{ l10nID: 'zotero-fulltext-menu-get' }, { l10nID: 'zotero-fulltext-menu-get-oa' }] }] },
    prefsPane: { paneID: 'p1', listed: true },
    engineInst: { enabled: false, auto_open_login: false, challenge_windows: false },
    invoke: { via: 'menu-ui', l10nID: 'zotero-fulltext-menu-get', selectedIDs: [10, 11, 12, 13], startRunCalls: [{ mode: 'full', interactive: true, items: [10, 11, 12, 13], run: 1 }] },
    run: { timedOut: false, elapsedMs: 40000, report: { run: 1, counts: { saved: 3, not_found: 1 }, engine_restarts: 0, halted: null } },
    items,
    eventLoop: { samples: 400, maxMs: 120, over250: 0 },
    windows: { opened: [{ windowtype: 'zotero-fulltext:progress', href: 'chrome://zotero-fulltext/content/progress.xhtml' }], loginOpened: false },
    prompts: [], promptSpy: 'defineProperty', consoleErrors: [], debug: { errorsOurs: [] },
    smoke: { modules: { ok: true, detail: {} } },
  };
}
const status = (ev, name) => ev.checks.find((c) => c.name === name)?.status;

test('a clean run passes with no warnings', () => {
  const ev = evaluate(good());
  assert.equal(ev.verdict, 'PASS');
  assert.equal(ev.counts.fail, 0);
  assert.equal(ev.counts.warn, 0);
});

test('a missing hard OA item fails, a missing soft one only warns', () => {
  const r = good();
  r.items[0].outcome = { kind: 'not_found', reason: 'no_oa_copy' };
  r.items[0].attachments = [];
  r.items[2].outcome = { kind: 'blocked', host: 'www.medrxiv.org', handoff: {} };
  r.items[2].attachments = [];
  const ev = evaluate(r);
  assert.equal(status(ev, 'item:gold'), 'fail');
  assert.equal(status(ev, 'item:medrxiv'), 'warn');
  assert.equal(ev.verdict, 'FAIL');
});

test('the paywalled item must not need a login', () => {
  const r = good();
  r.items[3].outcome = { kind: 'needs_user', why: 'login_declined', handoff: {} };
  assert.equal(status(evaluate(r), 'item:paywalled'), 'fail');
});

test('a login window, a prompt, a plugin error and a UI stall each fail', () => {
  for (const [name, mutate] of [
    ['no_login_windows', (r) => { r.windows.loginOpened = true; }],
    ['no_prompts', (r) => { r.prompts.push({ method: 'alert', text: ['boom'] }); }],
    ['no_errors_from_plugin', (r) => { r.consoleErrors.push({ ours: true, text: 'x' }); }],
    ['ui_not_blocked', (r) => { r.eventLoop.maxMs = 5000; }],
    ['institutional_access_disabled', (r) => { r.engineInst.enabled = true; }],
    ['run_finished', (r) => { r.run.timedOut = true; }],
    ['attachments_valid', (r) => { r.items[0].attachments[0].valid = false; }],
  ]) {
    const r = good();
    mutate(r);
    assert.equal(status(evaluate(r), name), 'fail', name);
  }
});

test('fallback invocation and failed smoke checks are warnings', () => {
  const r = good();
  r.invoke.via = 'registered-onCommand';
  r.invoke.note = 'UI route failed';
  r.smoke.menu_ui = { ok: false, detail: 'x' };
  const ev = evaluate(r);
  assert.equal(status(ev, 'menu_command_path'), 'warn');
  assert.equal(status(ev, 'smoke_checks'), 'warn');
  assert.equal(ev.verdict, 'PASS');
});

test('pdfFacts reads the magic and the trailer', () => {
  const enc = (s) => Uint8Array.from(Buffer.from(s, 'latin1'));
  assert.deepEqual(pdfFacts(enc('%PDF-1.4\n'), enc('...%%EOF\n'), 9999), { size: 9999, head: '%PDF-1.4', pdfMagic: true, eof: true });
  assert.equal(pdfFacts(enc('<html>'), enc('</html>'), 10).pdfMagic, false);
});

test('case filter keeps the declared order and ignores unknown keys', () => {
  assert.deepEqual(parseCaseFilter('').map((c) => c.key), ['gold', 'arxiv', 'medrxiv', 'paywalled']);
  assert.deepEqual(parseCaseFilter('paywalled, gold,nope').map((c) => c.key), ['gold', 'paywalled']);
});
