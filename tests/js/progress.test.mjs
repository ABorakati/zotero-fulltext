// ZFT.progress: DOM-free model, window controller, row actions, and the window script (progress-window.js) under a fake DOM.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { loadPlugin, plain, CONTENT } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';
import { loadWindowScript } from './helpers/fake-dom.mjs';

// ------------------------------------------------------------------------------------------ fixtures

const SNAPSHOTS = [
  { item: 1, key: '1/AAAAAAAA', title: 'Alpha: a study of things' },
  { item: 2, key: '1/BBBBBBBB', title: 'Beta' },
  { item: 3, key: '1/CCCCCCCC', title: 'Gamma' },
];

function row(state, l10n, extra = {}) {
  return { state, l10n, args: {}, ...extra };
}

/** Harness: plugin scope with util + progress, fake Services.ww recording windows, fake runs/save. */
function setup(opts = {}) {
  const launched = [];
  const calls = [];
  const windows = [];
  const zotero = makeFakeZotero({
    launchURL: (u) => launched.push(u),
    Utilities: { Internal: { activate: (w) => { w.activated = (w.activated || 0) + 1; } } },
  });
  const Services = {
    startup: { shuttingDown: !!opts.quitting },
    ww: {
      openWindow(parent, url, name, features, args) {
        if (opts.openThrows) throw new Error('boom');
        const win = {
          parent, url, name, features, args, closed: false,
          close() {
            if (this.closed) return;
            this.closed = true;
            // The window script calls args.onClose() on unload; the fake does it for the scriptless window.
            this.args.wrappedJSObject.onClose();
          },
          focus() { this.focused = (this.focused || 0) + 1; },
        };
        windows.push(win);
        return win;
      },
    },
  };
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero, Services }, files: ['util.js', 'progress.js'] });
  ZFT.runs = {
    cancelRun: (run) => calls.push(['cancelRun', run]),
    cancelItem: (run, item) => calls.push(['cancelItem', run, item]),
    userAction: (run, kind) => calls.push(['userAction', run, kind]),
  };
  ZFT.save = { attachFile: async (item, win) => { calls.push(['attachFile', item, win]); return { attachment_key: 'K' }; } };
  return { ZFT, zotero, launched, calls, windows };
}

function report(items, extra = {}) {
  return {
    run: 7,
    counts: { saved: 0, skipped: 0, not_found: 0, needs_user: 0, blocked: 0, deferred: 0, cancelled: 0, failed: 0 },
    items,
    ledger: { hour: 3, hour_cap: 30, day: 10, day_cap: 80, mb_hour: 120, mb_hour_cap: 300 },
    ...extra,
  };
}

const HANDOFF_ALL = {
  redirector_url: 'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1000%2Fabc',
  ezproxy_url: 'https://login.kcl.idm.oclc.org/login?url=https://www.bmj.com/content/1',
  libkey_url: 'https://libkey.io/libraries/286/10.1000/abc',
  doi_url: 'https://doi.org/10.1000/abc',
};

// ------------------------------------------------------------------------------------------ redactUrl

test('redactUrl keeps host and path only', () => {
  const { ZFT } = setup();
  const r = ZFT.progress.redactUrl;
  assert.equal(r('https://u:p@a.b/c/d?x=1#h'), 'a.b/c/d');
  assert.equal(r('http://example.org:8080/x.pdf?token=SECRET'), 'example.org:8080/x.pdf');
  assert.equal(r('https://a.b'), 'a.b/');
  assert.equal(r('not a url'), '(invalid url)');
  assert.equal(r(''), '(invalid url)');
  assert.equal(r(undefined), '(invalid url)');
  assert.equal(r('mailto:someone@example.org'), '(invalid url)');
  assert.equal(r('file:///C:/Users/me/secret.pdf'), '(invalid url)');
  assert.ok(r('https://a.b/' + 'x'.repeat(500)).length <= 160);
});

// ------------------------------------------------------------------------------------------ model

test('model starts queued in selection order and drops duplicate items', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, [...SNAPSHOTS, SNAPSHOTS[0]]);
  const rows = m.rows();
  assert.deepEqual(plain(rows.map((r) => r.item)), [1, 2, 3]);
  assert.ok(rows.every((r) => r.state === 'queued' && r.l10n === 'zotero-fulltext-status-queued' && r.actions.length === 0));
  assert.deepEqual(plain(m.counts()), { queued: 3, working: 0, waiting: 0, done: 0, attention: 0, failed: 0, skipped: 0, cancelled: 0, total: 3, finished: 0 });
  assert.equal(m.groups(), null);
});

test('applyRow stores state, message, args, route and actions; counts follow', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  assert.equal(m.applyRow(1, { state: 'working', l10n: 'zotero-fulltext-status-downloading', args: { host: 'x.org' }, route: 'oa', actions: [{ kind: 'cancel_item' }] }), true);
  const r = m.rowFor(1);
  assert.deepEqual(plain({ s: r.state, l: r.l10n, a: r.args, rt: r.route, ac: r.actions }),
    { s: 'working', l: 'zotero-fulltext-status-downloading', a: { host: 'x.org' }, rt: 'oa', ac: [{ kind: 'cancel_item' }] });
  const c = m.counts();
  assert.equal(c.working, 1);
  assert.equal(c.queued, 2);
  assert.equal(c.finished, 0);
  m.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa'));
  assert.equal(m.counts().done, 1);
  assert.equal(m.counts().working, 0);
  assert.equal(m.counts().finished, 1);
});

test('unknown items and malformed rows are logged and ignored', () => {
  const { ZFT, zotero } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  assert.equal(m.applyRow(99, row('working', 'x')), false);
  assert.equal(m.applyRow(1, { state: 'exploded', l10n: 'x' }), false);
  assert.equal(m.applyRow(1, null), false);
  assert.equal(m.counts().queued, 3);
  assert.ok(zotero.debugLog.some(([, msg]) => /unknown item 99/.test(msg)));
});

test('a row after a terminal state is ignored unless it is a retry transition', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  m.applyRow(1, row('attention', 'zotero-fulltext-status-login-declined'));
  assert.equal(m.applyRow(1, row('queued', 'zotero-fulltext-status-queued')), false);
  assert.equal(m.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa')), false);
  assert.equal(m.applyRow(1, row('failed', 'zotero-fulltext-status-failed')), false);
  assert.equal(m.rowFor(1).l10n, 'zotero-fulltext-status-login-declined');
  assert.equal(m.counts().attention, 1);
  // retry: back to working, then it may finish normally
  assert.equal(m.applyRow(1, row('working', 'zotero-fulltext-status-signing-in')), true);
  assert.equal(m.counts().attention, 0);
  assert.equal(m.counts().working, 1);
  assert.equal(m.applyRow(1, row('done', 'zotero-fulltext-status-saved-inst')), true);
  // waiting is a retry transition too (from failed)
  m.applyRow(2, row('failed', 'zotero-fulltext-status-failed'));
  assert.equal(m.applyRow(2, row('waiting', 'zotero-fulltext-status-waiting-login')), true);
  for (const terminal of ['done', 'skipped', 'cancelled']) {
    m.applyRow(3, row(terminal, 'zotero-fulltext-status-cancelled'));
    assert.equal(m.applyRow(3, row('skipped', 'x')), false, terminal);
    m.applyRow(3, row('working', 'zotero-fulltext-status-checking')); // retry back out for the next loop
  }
});

test('subscribe notifies with the changed item and unsubscribes cleanly', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  const seen = [];
  const off = m.subscribe((model, item) => seen.push(item));
  m.subscribe(() => { throw new Error('a broken listener must not break the others'); });
  m.applyRow(2, row('working', 'zotero-fulltext-status-checking'));
  m.applyRow(2, row('working', 'zotero-fulltext-status-checking')); // identical rows still notify (cheap, coalesced by the window)
  m.setReport(report([]));
  assert.deepEqual(seen, [2, 2, null]);
  off();
  m.applyRow(3, row('working', 'zotero-fulltext-status-checking'));
  assert.deepEqual(seen, [2, 2, null]);
});

test('counts() covers every state and the finished total', () => {
  const { ZFT } = setup();
  const snaps = Array.from({ length: 8 }, (_, i) => ({ item: i + 1, key: `1/K${i}`, title: `T${i}` }));
  const m = new ZFT.progress.ProgressModel(1, snaps);
  ZFT.progress.ROW_STATES.forEach((s, i) => { if (s !== 'queued') m.applyRow(i + 1, row(s, 'zotero-fulltext-status-checking')); });
  assert.deepEqual(plain(m.counts()), { queued: 1, working: 1, waiting: 1, done: 1, attention: 1, failed: 1, skipped: 1, cancelled: 1, total: 8, finished: 5 });
});

test('groups(): every Outcome kind lands in the documented bucket', () => {
  const { ZFT } = setup();
  const kinds = [
    ['saved', { kind: 'saved', route: 'oa', source: 'pmc_s3', version: 'published', url: 'https://doi.org/10.1/x', attachment_key: '1/AB' }, 'attached'],
    ['needs_user', { kind: 'needs_user', why: 'login_declined', handoff: {} }, 'needs_you'],
    ['not_entitled', { kind: 'not_entitled', handoff: {} }, 'needs_you'],
    ['blocked', { kind: 'blocked', host: 'x.org', handoff: {} }, 'needs_you'],
    ['halted', { kind: 'halted', handoff: {} }, 'needs_you'],
    ['deferred', { kind: 'deferred', cap: 'hourly' }, 'needs_you'],
    ['not_found', { kind: 'not_found', reason: 'no_oa_copy' }, 'not_found'],
    ['no_identifier', { kind: 'no_identifier' }, 'not_found'],
    ['failed', { kind: 'failed', code: 'x' }, 'failed'],
    ['skipped', { kind: 'skipped', reason: 'has_pdf' }, 'skipped'],
    ['cancelled', { kind: 'cancelled', by: 'user' }, 'skipped'],
  ];
  const snaps = kinds.map((k, i) => ({ item: i + 1, key: `1/K${i}`, title: `Title ${k[0]}` }));
  const m = new ZFT.progress.ProgressModel(7, snaps);
  m.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa'));
  m.setReport(report(kinds.map((k, i) => ({ item: i + 1, key: `1/K${i}`, outcome: k[1], trace: [] }))));
  const g = m.groups();
  assert.deepEqual(Object.keys(g), ['attached', 'needs_you', 'not_found', 'failed', 'skipped']);
  kinds.forEach(([name, , group], i) => {
    const entry = g[group].find((e) => e.item === i + 1);
    assert.ok(entry, `${name} -> ${group}`);
    assert.equal(entry.outcome.kind, name);
    assert.equal(entry.key, `1/K${i}`);
    assert.equal(entry.title, `Title ${name}`);
    assert.ok(entry.row && entry.row.item === i + 1, 'entry carries the last row');
    assert.ok(Array.isArray(entry.handoffActions));
  });
  assert.equal(g.attached.length, 1);
  assert.equal(g.needs_you.length, 5);
  assert.equal(g.not_found.length, 2);
  assert.equal(g.failed.length, 1);
  assert.equal(g.skipped.length, 2);
  assert.equal(m.groups(), g, 'cached until something changes');
  m.applyRow(1, row('working', 'zotero-fulltext-status-checking'));
  assert.notEqual(m.groups(), g, 'a late row invalidates the cache');
});

test('groups(): hand-off URLs become open_browser actions (http/https only, fixed order, no duplicates)', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  m.applyRow(1, {
    state: 'attention', l10n: 'zotero-fulltext-status-needs-user', args: {},
    actions: [{ kind: 'open_browser', url: HANDOFF_ALL.redirector_url, l10n: 'zotero-fulltext-action-open-redirector' }, { kind: 'attach_file' }],
  });
  m.setReport(report([
    { item: 1, key: '1/AAAAAAAA', outcome: { kind: 'needs_user', why: 'login_declined', handoff: { ...HANDOFF_ALL } }, trace: [] },
    {
      item: 2, key: '1/BBBBBBBB', trace: [],
      outcome: { kind: 'not_entitled', handoff: { redirector_url: 'javascript:alert(1)', ezproxy_url: 'file:///etc/passwd', libkey_url: 'ftp://x/y', doi_url: 'https://doi.org/10.1/z' } },
    },
  ]));
  const [first] = m.groups().needs_you.filter((e) => e.item === 1);
  assert.deepEqual(plain(first.handoffActions), [
    { kind: 'open_browser', url: HANDOFF_ALL.redirector_url, l10n: 'zotero-fulltext-action-open-redirector' },
    { kind: 'open_browser', url: HANDOFF_ALL.ezproxy_url, l10n: 'zotero-fulltext-action-open-ezproxy' },
    { kind: 'open_browser', url: HANDOFF_ALL.libkey_url, l10n: 'zotero-fulltext-action-open-libkey' },
    { kind: 'open_browser', url: HANDOFF_ALL.doi_url, l10n: 'zotero-fulltext-action-open-doi' },
  ]);
  // merged list: the row's own actions first, then the hand-off links it did not already carry
  assert.deepEqual(plain(first.actions.map((a) => a.kind + ':' + (a.l10n || ''))), [
    'open_browser:zotero-fulltext-action-open-redirector', 'attach_file:',
    'open_browser:zotero-fulltext-action-open-ezproxy', 'open_browser:zotero-fulltext-action-open-libkey', 'open_browser:zotero-fulltext-action-open-doi',
  ]);
  const second = m.groups().needs_you.find((e) => e.item === 2);
  assert.deepEqual(plain(second.handoffActions), [{ kind: 'open_browser', url: 'https://doi.org/10.1/z', l10n: 'zotero-fulltext-action-open-doi' }]);
});

test('setReport replaces rows that never reached a terminal state (engine failure report)', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  m.applyRow(1, row('working', 'zotero-fulltext-status-searching-oa'));
  m.applyRow(2, row('done', 'zotero-fulltext-status-saved-oa'));
  m.setReport(report([
    { item: 1, key: '1/AAAAAAAA', outcome: { kind: 'needs_user', why: 'engine_restarted', handoff: {} }, trace: [] },
    { item: 2, key: '1/BBBBBBBB', outcome: { kind: 'saved', route: 'oa', source: 's', version: 'published', url: 'https://x.org/a', attachment_key: 'k' }, trace: [] },
    { item: 3, key: '1/CCCCCCCC', outcome: { kind: 'needs_user', why: 'engine_restarted', handoff: {} }, trace: [] },
  ], { engine_restarts: 1 }));
  assert.equal(m.rowFor(1).state, 'attention');
  assert.equal(m.rowFor(1).l10n, 'zotero-fulltext-status-engine-restarted');
  assert.equal(m.rowFor(3).l10n, 'zotero-fulltext-status-engine-restarted');
  assert.equal(m.rowFor(2).l10n, 'zotero-fulltext-status-saved-oa', 'a terminal row is never replaced');
  assert.equal(m.counts().finished, 3);
  assert.equal(m.counts().working, 0);
});

test('diagnostics: redacted, one line per item, no titles, no queries, no secrets', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  m.applyRow(1, row('done', 'zotero-fulltext-status-saved-inst'));
  m.applyRow(2, row('attention', 'zotero-fulltext-status-needs-user'));
  m.applyRow(3, row('attention', 'zotero-fulltext-status-blocked', { args: { host: 'www.sciencedirect.com' } }));
  m.setReport(report([
    {
      item: 1, key: '1/AAAAAAAA',
      outcome: { kind: 'saved', route: 'inst', source: 'lww', version: 'published', url: 'https://doi.org/10.1097/SLA.1?casa_token=SECRETTOKEN&utm=1', attachment_key: '1/ZZZZ' },
      trace: [{ src: 'openalex', tier: 'api', class: 'found', ms: 512 }, { src: 'redirector', tier: 'browser', class: 'pdf', ms: 8100 }],
    },
    {
      item: 2, key: '1/BBBBBBBB', trace: [],
      outcome: { kind: 'needs_user', why: 'login_declined', handoff: { redirector_url: HANDOFF_ALL.redirector_url, libkey_url: HANDOFF_ALL.libkey_url } },
    },
    { item: 3, key: '1/CCCCCCCC', outcome: { kind: 'blocked', host: 'www.sciencedirect.com', handoff: {} }, trace: [] },
  ], { halted: 'rate limited' }));
  const text = m.diagnostics();
  const lines = text.split('\n');
  assert.ok(lines.some((l) => l.startsWith('1/AAAAAAAA | saved route=inst source=lww version=published url=doi.org/10.1097/SLA.1 | zotero-fulltext-status-saved-inst | openalex api found 512ms; redirector browser pdf 8100ms')), text);
  assert.ok(lines.some((l) => l.startsWith('1/BBBBBBBB | needs_user why=login_declined redirector=go.openathens.net/redirector/kcl.ac.uk libkey=libkey.io/libraries/286/10.1000/abc | zotero-fulltext-status-needs-user')), text);
  assert.ok(lines.some((l) => l.startsWith('1/CCCCCCCC | blocked host=www.sciencedirect.com | zotero-fulltext-status-blocked')), text);
  assert.ok(lines.includes('ledger hour=3/30 day=10/80 mb_hour=120/300'), text);
  assert.ok(lines.includes('halted rate limited'));
  assert.doesNotMatch(text, /Alpha|Beta|Gamma|study of things/, 'titles never appear');
  assert.doesNotMatch(text, /SECRETTOKEN|casa_token|utm=|[?]/, 'query strings never appear');
  assert.doesNotMatch(text, /https?:\/\//, 'no scheme: URLs are reduced to host/path');
});

test('diagnostics before the report lists the live rows', () => {
  const { ZFT } = setup();
  const m = new ZFT.progress.ProgressModel(7, SNAPSHOTS);
  m.applyRow(1, row('working', 'zotero-fulltext-status-searching-oa'));
  const text = m.diagnostics();
  assert.match(text, /1\/AAAAAAAA \| \(working\) \| zotero-fulltext-status-searching-oa/);
  assert.match(text, /1\/BBBBBBBB \| \(queued\)/);
});

// ------------------------------------------------------------------------------------------ controller

test('open() creates the model and opens one window with the right features and args holder', () => {
  const { ZFT, windows } = setup();
  const model = ZFT.progress.open(7, SNAPSHOTS, { interactive: true });
  assert.ok(model instanceof ZFT.progress.ProgressModel);
  assert.equal(ZFT.progress.modelFor(7), model);
  assert.equal(windows.length, 1);
  const w = windows[0];
  assert.equal(w.parent, null);
  assert.equal(w.url, 'chrome://zotero-fulltext/content/progress.xhtml');
  assert.equal(w.name, '_blank');
  assert.equal(w.features, 'chrome,dialog=no,resizable,centerscreen');
  const args = w.args.wrappedJSObject;
  assert.equal(args.wrappedJSObject, args, 'the spike idiom: works whether the window sees the object or a wrapper');
  assert.equal(args.model, model);
  for (const fn of ['onAction', 'onCancel', 'onClose', 'onCopy', 'format']) assert.equal(typeof args[fn], 'function', fn);
  assert.equal(args.onCopy(), model.diagnostics());

  const { ZFT: z2, windows: w2 } = setup();
  z2.progress.open(8, SNAPSHOTS, { interactive: false });
  assert.equal(w2[0].features, 'chrome,dialog=no,resizable,centerscreen,alwaysLowered');
  z2.progress.open(9, SNAPSHOTS);
  assert.match(w2[1].features, /alwaysLowered/, 'not interactive by default');
});

test('update() feeds the model; rows for unknown runs are ignored', () => {
  const { ZFT } = setup();
  const model = ZFT.progress.open(7, SNAPSHOTS, {});
  ZFT.progress.update(7, 2, row('working', 'zotero-fulltext-status-checking'));
  assert.equal(model.rowFor(2).state, 'working');
  assert.doesNotThrow(() => ZFT.progress.update(99, 2, row('working', 'x')));
  assert.doesNotThrow(() => ZFT.progress.finish(99, report([])));
  assert.equal(ZFT.progress.modelFor(99), null);
});

test('closing the window never cancels the run; finish() reopens only for needs-you items', () => {
  const { ZFT, windows, calls } = setup();
  const model = ZFT.progress.open(7, SNAPSHOTS, { interactive: true });
  windows[0].close();
  assert.deepEqual(calls, [], 'closing is not a cancel');
  // nothing for the user to do: stays closed
  ZFT.progress.finish(7, report([
    { item: 1, key: '1/AAAAAAAA', outcome: { kind: 'saved', route: 'oa', source: 's', version: 'published', url: 'https://x.org/a', attachment_key: 'k' }, trace: [] },
    { item: 2, key: '1/BBBBBBBB', outcome: { kind: 'not_found', reason: 'no_oa_copy' }, trace: [] },
  ]));
  assert.equal(windows.length, 1);
  assert.ok(model.report);

  // a second run: needs-you item and a closed window -> reopened, in front
  const { ZFT: z2, windows: w2 } = setup();
  z2.progress.open(7, SNAPSHOTS, { interactive: false });
  w2[0].close();
  z2.progress.finish(7, report([{ item: 1, key: '1/AAAAAAAA', outcome: { kind: 'needs_user', why: 'login_declined', handoff: {} }, trace: [] }]));
  assert.equal(w2.length, 2);
  assert.equal(w2[1].features, 'chrome,dialog=no,resizable,centerscreen', 'a reopened report window is not alwaysLowered');
});

test('finish() with the window still open just stores the report', () => {
  const { ZFT, windows } = setup();
  const model = ZFT.progress.open(7, SNAPSHOTS, {});
  ZFT.progress.finish(7, report([{ item: 1, key: '1/AAAAAAAA', outcome: { kind: 'needs_user', why: 'login_declined', handoff: {} }, trace: [] }]));
  assert.equal(windows.length, 1);
  assert.ok(model.groups().needs_you.length === 1);
});

test('show(): reopens a closed window, activates an open one, defaults to the latest run, false when nothing is known', () => {
  const { ZFT, windows } = setup();
  assert.equal(ZFT.progress.show(), false);
  ZFT.progress.open(7, SNAPSHOTS, {});
  ZFT.progress.open(8, SNAPSHOTS, {});
  assert.equal(windows.length, 2);
  assert.equal(ZFT.progress.show(), true);
  assert.equal(windows.length, 2, 'open window: no second one');
  assert.equal(windows[1].activated, 1, 'brought to the front');
  windows[1].close();
  assert.equal(ZFT.progress.show(), true);
  assert.equal(windows.length, 3, 'closed window reopened');
  assert.equal(ZFT.progress.show(7), true);
  assert.equal(windows[0].activated, 1);
  assert.equal(ZFT.progress.show(1234), false);
});

test('the window buttons reach the run controller: Cancel -> cancelRun', () => {
  const { ZFT, windows, calls } = setup();
  ZFT.progress.open(7, SNAPSHOTS, {});
  windows[0].args.wrappedJSObject.onCancel();
  assert.deepEqual(plain(calls), [['cancelRun', 7]]);
});

test('open() survives a window that cannot be opened, and a quitting application', () => {
  const a = setup({ openThrows: true });
  const model = a.ZFT.progress.open(7, SNAPSHOTS, {});
  assert.ok(model, 'the model still works');
  assert.equal(a.ZFT.progress.show(7), false);
  const b = setup({ quitting: true });
  b.ZFT.progress.open(7, SNAPSHOTS, {});
  assert.equal(b.windows.length, 0, 'no window while Zotero quits');
});

test('handleAction dispatch table', async () => {
  const { ZFT, windows, calls, launched } = setup();
  ZFT.progress.open(7, SNAPSHOTS, {});
  const h = ZFT.progress.handleAction;
  await h(7, 1, { kind: 'open_browser', url: 'https://doi.org/10.1/x', l10n: 'zotero-fulltext-action-open-doi' });
  await h(7, 1, { kind: 'open_browser', url: 'http://example.org/a?b=c', l10n: 'zotero-fulltext-action-open-browser' });
  assert.deepEqual(plain(launched), ['https://doi.org/10.1/x', 'http://example.org/a?b=c']);
  for (const bad of ['javascript:alert(1)', 'file:///C:/x.pdf', 'ftp://x.org/y', 'chrome://browser/content/', 'data:text/html,hi', 'about:config', '', undefined, 42, 'not a url']) {
    await h(7, 1, { kind: 'open_browser', url: bad, l10n: 'x' });
  }
  assert.equal(launched.length, 2, 'non-http(s) addresses are refused');

  await h(7, 2, { kind: 'attach_file' });
  await h(7, 2, { kind: 'sign_in' });
  await h(7, 2, { kind: 'retry' });
  await h(7, 3, { kind: 'cancel_item' });
  await h(7, 3, { kind: 'detonate' });
  await h(7, 3, null);
  await h(7, 3, {});
  assert.deepEqual(plain(calls.slice(1)), [
    ['userAction', 7, 'sign_in_now'],
    ['userAction', 7, 'retry_sign_in'],
    ['cancelItem', 7, 3],
  ]);
  assert.deepEqual([calls[0][0], calls[0][1]], ['attachFile', 2]);
  assert.equal(calls[0][2], windows[0], 'the file picker is parented to the progress window');
});

test('handleAction passes no parent window when the progress window is closed; window-side action errors never escape', async () => {
  const { ZFT, windows, calls } = setup();
  ZFT.progress.open(7, SNAPSHOTS, {});
  windows[0].close();
  await ZFT.progress.handleAction(7, 1, { kind: 'attach_file' });
  assert.equal(calls[0][2], undefined);
  ZFT.save.attachFile = async () => { throw new Error('picker exploded'); };
  ZFT.progress.open(8, SNAPSHOTS, {});
  const args = windows[windows.length - 1].args.wrappedJSObject;
  assert.doesNotThrow(() => args.onAction(1, { kind: 'attach_file' }));
  await new Promise((r) => setTimeout(r, 5)); // let the rejected promise settle: it is logged, not unhandled
});

test('closeAll closes every window and later rows or reports are ignored', () => {
  const { ZFT, windows } = setup();
  ZFT.progress.open(7, SNAPSHOTS, {});
  ZFT.progress.open(8, SNAPSHOTS, {});
  ZFT.progress.closeAll();
  assert.ok(windows.every((w) => w.closed));
  assert.equal(ZFT.progress.modelFor(7), null);
  assert.doesNotThrow(() => {
    ZFT.progress.update(7, 1, row('working', 'x'));
    ZFT.progress.finish(7, report([{ item: 1, key: 'k', outcome: { kind: 'needs_user', why: 'login_declined', handoff: {} }, trace: [] }]));
  });
  assert.equal(windows.length, 2, 'a report arriving during shutdown does not reopen anything');
  assert.equal(ZFT.progress.show(), false);
});

// ------------------------------------------------------------------------------------------ the window script

function openWindowScript(snapshots = SNAPSHOTS, mutate) {
  const env = setup();
  const model = env.ZFT.progress.open(7, snapshots, { interactive: true });
  if (mutate) mutate(model, env);
  const args = env.windows[0].args.wrappedJSObject;
  const w = loadWindowScript('progress-window.js', { xhtml: 'progress.xhtml', args: env.windows[0].args });
  w.fire('load');
  return { ...env, model, args, w };
}

const rowEl = (w, item) => w.document.root.find((e) => e.getAttribute('data-item') === String(item))[0];
const statusOf = (w, item) => rowEl(w, item).children[1];
const buttonsOf = (w, item) => rowEl(w, item).children[2].children;

test('window: renders the header counts, one row per item and Fluent status text', async () => {
  const { w, model } = openWindowScript();
  const header = w.byId('zft-progress-header');
  assert.equal(header.l10nId, 'zotero-fulltext-progress-header');
  assert.deepEqual(plain(header.l10nArgs), { done: 0, total: 3, working: 0, waiting: 0 });
  const rows = w.byId('zft-progress-rows').children;
  assert.equal(rows.length, 3);
  assert.equal(rows[0].children[0].textContent, 'Alpha: a study of things');
  assert.equal(rows[0].children[0].getAttribute('title'), 'Alpha: a study of things', 'full title in the tooltip');
  assert.match(rows[0].children[0].attributes.get('style') || '', /text-overflow:ellipsis/);
  assert.equal(statusOf(w, 1).l10nId, 'zotero-fulltext-status-queued');
  assert.equal(w.byId('zft-progress-copy').hidden, true);
  assert.equal(w.byId('zft-progress-ledger').hidden, true);
  assert.equal(w.byId('zft-progress-cancel').hidden, false);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(w.document.title, '[zotero-fulltext-progress-title]');
  assert.equal(model._listeners.size, 1);
});

test('window: a row change patches that row only, and many changes coalesce into one frame', () => {
  const { w, model } = openWindowScript();
  const created = w.document.stats.created;
  const sets = w.document.stats.l10nSets;
  for (let i = 0; i < 50; i += 1) model.applyRow(2, { state: 'working', l10n: 'zotero-fulltext-status-waiting-host', args: { host: 'h.org', seconds: String(i) } });
  assert.equal(w.pendingFrames(), 1, 'coalesced with requestAnimationFrame');
  assert.equal(statusOf(w, 2).l10nId, 'zotero-fulltext-status-queued', 'nothing is rendered before the frame');
  w.flushFrames();
  assert.equal(statusOf(w, 2).l10nId, 'zotero-fulltext-status-waiting-host');
  assert.deepEqual(plain(statusOf(w, 2).l10nArgs), { host: 'h.org', seconds: '49' });
  assert.equal(statusOf(w, 1).l10nId, 'zotero-fulltext-status-queued', 'other rows untouched');
  assert.equal(w.document.stats.created, created, 'no node was created');
  assert.equal(w.document.stats.l10nSets - sets, 2, 'one status + the header');
  assert.deepEqual(plain(w.byId('zft-progress-header').l10nArgs), { done: 0, total: 3, working: 1, waiting: 0 });
  assert.equal(rowEl(w, 2).getAttribute('data-state'), 'working');
});

test('window: thousands of rows render once, then single updates touch a single row', () => {
  const many = Array.from({ length: 3000 }, (_, i) => ({ item: i + 1, key: `1/K${i}`, title: `Title ${i}` }));
  const { w, model } = openWindowScript(many);
  assert.equal(w.byId('zft-progress-rows').children.length, 3000);
  const before = { ...w.document.stats };
  model.applyRow(1500, row('done', 'zotero-fulltext-status-saved-oa'));
  w.flushFrames();
  assert.equal(w.document.stats.created, before.created, 'no row was rebuilt');
  assert.equal(w.document.stats.appends, before.appends, 'no row was re-attached');
  assert.equal(w.document.stats.l10nSets - before.l10nSets, 2);
  assert.equal(statusOf(w, 1500).l10nId, 'zotero-fulltext-status-saved-oa');
});

test('window: action buttons are labelled, call onAction, are replaced when the actions change and do not pile up', () => {
  const clicks = [];
  const { w, model, args } = openWindowScript(SNAPSHOTS, (m, env) => {
    env.windows[0].args.wrappedJSObject.onAction = (item, action) => clicks.push([item, action.kind, action.url || null]);
  });
  const actions = [
    { kind: 'open_browser', url: 'https://doi.org/10.1/x', l10n: 'zotero-fulltext-action-open-doi' },
    { kind: 'attach_file' },
    { kind: 'sign_in' },
    { kind: 'retry' },
    { kind: 'cancel_item' },
  ];
  model.applyRow(1, row('attention', 'zotero-fulltext-status-needs-user', { actions }));
  w.flushFrames();
  const buttons = buttonsOf(w, 1);
  assert.deepEqual(buttons.map((b) => b.l10nId), [
    'zotero-fulltext-action-open-doi', 'zotero-fulltext-action-attach-file', 'zotero-fulltext-action-sign-in',
    'zotero-fulltext-action-retry', 'zotero-fulltext-action-cancel-item',
  ]);
  buttons[0].click();
  buttons[2].click();
  assert.deepEqual(clicks, [[1, 'open_browser', 'https://doi.org/10.1/x'], [1, 'sign_in', null]]);
  assert.equal(typeof args.onAction, 'function');

  const old = buttons[0];
  model.applyRow(1, row('working', 'zotero-fulltext-status-signing-in', { actions: [{ kind: 'cancel_item' }] }));
  w.flushFrames();
  assert.equal(buttonsOf(w, 1).length, 1);
  assert.equal(old.parentNode, null, 'the old button is detached with its listener');
  // identical actions: buttons are kept (no churn)
  const keep = buttonsOf(w, 1)[0];
  model.applyRow(1, row('working', 'zotero-fulltext-status-institutional', { actions: [{ kind: 'cancel_item' }] }));
  w.flushFrames();
  assert.equal(buttonsOf(w, 1)[0], keep);
});

test('window: after the report the rows move into group sections, Cancel goes away and the ledger appears', () => {
  const { w, model } = openWindowScript();
  model.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa'));
  model.applyRow(2, row('attention', 'zotero-fulltext-status-needs-user', { actions: [{ kind: 'attach_file' }] }));
  model.applyRow(3, row('attention', 'zotero-fulltext-status-not-found'));
  w.flushFrames();
  const nodes = [1, 2, 3].map((i) => rowEl(w, i));
  const created = w.document.stats.created;
  model.setReport(report([
    { item: 1, key: '1/AAAAAAAA', outcome: { kind: 'saved', route: 'oa', source: 's', version: 'published', url: 'https://x.org/a', attachment_key: 'k' }, trace: [] },
    { item: 2, key: '1/BBBBBBBB', outcome: { kind: 'needs_user', why: 'login_declined', handoff: { libkey_url: HANDOFF_ALL.libkey_url } }, trace: [] },
    { item: 3, key: '1/CCCCCCCC', outcome: { kind: 'not_found', reason: 'no_oa_copy' }, trace: [] },
  ]));
  w.flushFrames();
  const rowsEl = w.byId('zft-progress-rows');
  const sectionIds = rowsEl.children.map((s) => s.children[0].children[0].l10nId);
  assert.deepEqual(sectionIds, ['zotero-fulltext-report-group-attached', 'zotero-fulltext-report-group-needs-you', 'zotero-fulltext-report-group-not-found']);
  assert.deepEqual(rowsEl.children.map((s) => s.children[0].children[1].textContent), ['1', '1', '1']);
  nodes.forEach((n, i) => assert.equal(rowEl(w, i + 1), n, 'the same row node, only re-parented'));
  assert.equal(nodes[1].parentNode.parentNode, rowsEl.children[1]);
  assert.ok(w.document.stats.created - created <= 20, 'only the three section shells and the changed buttons are new');
  assert.deepEqual(buttonsOf(w, 2).map((b) => b.l10nId), ['zotero-fulltext-action-attach-file', 'zotero-fulltext-action-open-libkey'], 'row actions + hand-off links');
  assert.equal(w.byId('zft-progress-cancel').hidden, true);
  assert.equal(w.byId('zft-progress-copy').hidden, false);
  const header = w.byId('zft-progress-header');
  assert.equal(header.l10nId, 'zotero-fulltext-report-summary');
  assert.deepEqual(plain(header.l10nArgs), { saved: 1, needs: 1, notfound: 1, failed: 0, skipped: 0 });
  const ledger = w.byId('zft-progress-ledger');
  assert.equal(ledger.hidden, false);
  assert.equal(ledger.l10nId, 'zotero-fulltext-report-ledger');
  assert.deepEqual(plain(ledger.l10nArgs), { hour: 3, hour_cap: 30, day: 10, day_cap: 80, mb: 120, mb_cap: 300 });
});

test('window: opened after the run finished it shows the grouped view straight away', () => {
  const { w } = openWindowScript(SNAPSHOTS, (m) => {
    m.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa'));
    m.setReport(report([{ item: 1, key: '1/AAAAAAAA', outcome: { kind: 'saved', route: 'oa', source: 's', version: 'published', url: 'https://x.org/a', attachment_key: 'k' }, trace: [] }]));
  });
  assert.equal(w.byId('zft-progress-header').l10nId, 'zotero-fulltext-report-summary');
  assert.equal(w.byId('zft-progress-cancel').hidden, true);
  assert.equal(w.byId('zft-progress-copy').hidden, false);
  // items the report does not mention stay visible below the sections
  const rowsEl = w.byId('zft-progress-rows');
  assert.equal(rowsEl.children.length, 1 + 2);
  assert.equal(rowEl(w, 2).getAttribute('data-item'), '2');
});

test('window: a late row after the report (retry) re-lays out the groups without rebuilding rows', () => {
  const { w, model } = openWindowScript();
  model.applyRow(2, row('attention', 'zotero-fulltext-status-login-declined', { actions: [{ kind: 'sign_in' }] }));
  model.setReport(report([{ item: 2, key: '1/BBBBBBBB', outcome: { kind: 'needs_user', why: 'login_declined', handoff: {} }, trace: [] }]));
  w.flushFrames();
  const node = rowEl(w, 2);
  const created = w.document.stats.created;
  model.applyRow(2, row('working', 'zotero-fulltext-status-signing-in'));
  w.flushFrames();
  assert.equal(rowEl(w, 2), node);
  assert.equal(statusOf(w, 2).l10nId, 'zotero-fulltext-status-signing-in');
  assert.equal(w.document.stats.created, created);
});

test('window: Cancel sends onCancel once, Close closes, unload unsubscribes and tells the controller', () => {
  const { ZFT, w, model, windows, calls } = openWindowScript();
  const cancel = w.byId('zft-progress-cancel');
  cancel.click();
  cancel.click();
  assert.equal(cancel.disabled, true);
  assert.deepEqual(plain(calls), [['cancelRun', 7]], 'a disabled button cannot fire twice');
  assert.equal(model._listeners.size, 1);
  w.byId('zft-progress-close').click();
  assert.equal(w.closed, true);
  assert.equal(model._listeners.size, 0, 'no leaked model listener');
  assert.deepEqual(plain(calls), [['cancelRun', 7]], 'closing never cancels');
  // no render after unload
  model.applyRow(1, row('working', 'zotero-fulltext-status-checking'));
  assert.equal(w.pendingFrames(), 0);
  // the controller learned that the window is gone: a report with something for the user reopens it
  assert.equal(windows.length, 1);
  ZFT.progress.finish(7, report([{ item: 2, key: '1/BBBBBBBB', outcome: { kind: 'needs_user', why: 'login_declined', handoff: {} }, trace: [] }]));
  assert.equal(windows.length, 2);
});

test('window: Copy diagnostics copies args.onCopy() text through the clipboard helper', () => {
  const { w, model } = openWindowScript();
  model.applyRow(1, row('done', 'zotero-fulltext-status-saved-oa'));
  model.setReport(report([{ item: 1, key: '1/AAAAAAAA', outcome: { kind: 'saved', route: 'oa', source: 's', version: 'published', url: 'https://x.org/a?q=1', attachment_key: 'k' }, trace: [] }]));
  w.flushFrames();
  w.byId('zft-progress-copy').click();
  assert.equal(w.copied.length, 1);
  assert.equal(w.copied[0], model.diagnostics());
  assert.doesNotMatch(w.copied[0], /Alpha/);
});

test('window: a very long title is capped in the node and in the tooltip; an empty title falls back to the item key', () => {
  const long = 'W'.repeat(5000);
  const { w } = openWindowScript([{ item: 1, key: '1/AAAAAAAA', title: long }, { item: 2, key: '1/BBBBBBBB', title: '' }]);
  const title = rowEl(w, 1).children[0];
  assert.equal(title.textContent.length, 300);
  assert.equal(title.getAttribute('title').length, 1000);
  assert.equal(rowEl(w, 2).children[0].textContent, '1/BBBBBBBB');
});

test('window: without arguments it does nothing and throws nothing', () => {
  const w = loadWindowScript('progress-window.js', { xhtml: 'progress.xhtml' });
  assert.doesNotThrow(() => w.fire('load'));
  assert.equal(w.listenerCount('load'), 0);
});

test('every id the window script looks up exists in progress.xhtml', () => {
  const script = fs.readFileSync(path.join(CONTENT, 'progress-window.js'), 'utf8');
  const markup = fs.readFileSync(path.join(CONTENT, 'progress.xhtml'), 'utf8');
  const ids = new Set([...script.matchAll(/el\('(zft-[a-z-]+)'\)/g)].map((m) => m[1]));
  assert.ok(ids.size >= 5);
  for (const id of ids) assert.ok(markup.includes(`id="${id}"`), id);
});
