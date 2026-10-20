// ZFT.items: Zotero.Item -> ItemSnapshot (model.rs), menu pre-check, item lookup and the deletion observer.
// Plain-object fake items (methods + fields) and fakes for Zotero.Libraries / Zotero.Notifier.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';

const SNAPSHOT_KEYS = ['item', 'key', 'library', 'regular', 'feed', 'item_type', 'title', 'fields', 'pdf_attachments', 'best_attachment'];

/** Libraries known to the fake Zotero.Libraries. */
const LIBRARIES = {
  1: { libraryType: 'user', filesEditable: true, name: 'My Library' },
  2: { libraryType: 'group', filesEditable: false, name: 'Lab group' },
  3: { libraryType: 'feed', filesEditable: false, name: 'A feed' },
  4: { libraryType: 'publications', filesEditable: true, name: 'My Publications' },
};

function env(extra = {}) {
  const notifier = {
    registered: [],
    unregistered: [],
    registerObserver(observer, types, id) { this.registered.push({ observer, types, id }); return 'obs-' + this.registered.length; },
    unregisterObserver(observerID) { this.unregistered.push(observerID); },
  };
  const itemsById = new Map();
  const zotero = makeFakeZotero({
    Libraries: { get: (id) => LIBRARIES[id] || null },
    Notifier: notifier,
    Items: { get: (id) => itemsById.get(id) || false },
    ...extra,
  });
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero }, files: ['util.js', 'items.js'] });
  return { ZFT, I: ZFT.items, zotero, notifier, itemsById };
}

/**
 * A fake Zotero.Item. o.fields maps field names to values; names in o.invalid throw (like a field that does not
 * exist for the item type); o.unloaded lists data types the item claims not to have loaded yet.
 */
function makeItem(o = {}) {
  const calls = [];
  const loaded = { itemData: true, childItems: true };
  for (const t of o.unloaded || []) loaded[t] = false;
  const item = {
    calls,
    id: o.id ?? 11,
    key: o.key ?? 'ABCD2345',
    libraryID: o.libraryID ?? 1,
    itemType: o.itemType ?? 'journalArticle',
    isFeedItem: o.isFeedItem,
    _loaded: o.noLoadedFlags ? undefined : loaded,
    fields: { title: 'A title', DOI: '10.1371/journal.pmed.1000097', url: 'https://doi.org/10.1371/journal.pmed.1000097', ...(o.fields || {}) },
    isRegularItem() {
      calls.push('isRegularItem');
      if (o.regularThrows) throw new Error('boom');
      return o.regular ?? true;
    },
    getField(name) {
      calls.push('getField:' + name);
      if ((o.invalid || []).includes(name)) throw new Error(`"${name}" is not a valid field`);
      const v = this.fields[name];
      return v === undefined ? '' : v;
    },
    getDisplayTitle() {
      calls.push('getDisplayTitle');
      if (o.displayTitleThrows) throw new Error('boom');
      return o.displayTitle !== undefined ? o.displayTitle : this.fields.title || '';
    },
    numFileAttachmentsWithContentType(type) {
      calls.push('numFileAttachmentsWithContentType:' + type);
      if (o.countThrows) throw new Error('childItems not loaded');
      return o.pdfCount ?? 0;
    },
    async getBestAttachmentState() {
      calls.push('getBestAttachmentState');
      if (o.stateThrows) throw new Error('db locked');
      return o.best ?? { type: 'none' };
    },
    async loadDataType(type) {
      calls.push('loadDataType:' + type);
      loaded[type] = true;
    },
  };
  if (o.deleteGetDisplayTitle) delete item.getDisplayTitle;
  return item;
}

test('snapshot returns exactly the model.rs ItemSnapshot shape for a regular item with an attached PDF', async () => {
  const { I } = env();
  const item = makeItem({
    id: 42, key: 'ZXCV9876', libraryID: 1, itemType: 'journalArticle',
    fields: { title: 'Preferred reporting items', DOI: '10.1371/journal.pmed.1000097', PMID: '', PMCID: '', archiveID: '', extra: 'PMID: 19621072', url: 'https://example.org/a' },
    pdfCount: 2, best: { type: 'pdf', exists: true, key: 'PDFKEY22' },
  });
  const snap = await I.snapshot(item);
  assert.deepEqual(Object.keys(snap), SNAPSHOT_KEYS);
  assert.deepEqual(plain(snap), {
    item: 42,
    key: '1/ZXCV9876',
    library: { id: 1, kind: 'user', files_editable: true, name: 'My Library' },
    regular: true,
    feed: false,
    item_type: 'journalArticle',
    title: 'Preferred reporting items',
    fields: { doi: '10.1371/journal.pmed.1000097', pmid: '', pmcid: '', url: 'https://example.org/a', archive_id: '', extra: 'PMID: 19621072' },
    pdf_attachments: 2,
    best_attachment: { kind: 'pdf', exists: true },
  });
  assert.deepEqual(Object.keys(snap.fields), ['doi', 'pmid', 'pmcid', 'url', 'archive_id', 'extra']);
  assert.deepEqual(Object.keys(snap.library), ['id', 'kind', 'files_editable', 'name']);
  assert.deepEqual(Object.keys(snap.best_attachment), ['kind', 'exists']);
});

test('the read fields are DOI, PMID, PMCID, url, archiveID and extra', async () => {
  const { I } = env();
  const item = makeItem({ fields: { PMID: '19621072', PMCID: 'PMC2707599', archiveID: 'arXiv:1706.03762', extra: 'x' } });
  const snap = await I.snapshot(item);
  assert.equal(snap.fields.pmid, '19621072');
  assert.equal(snap.fields.pmcid, 'PMC2707599');
  assert.equal(snap.fields.archive_id, 'arXiv:1706.03762');
  for (const name of ['DOI', 'PMID', 'PMCID', 'url', 'archiveID', 'extra']) {
    assert.ok(item.calls.includes('getField:' + name), name);
  }
});

test('a field that is invalid for the item type, throws, or is not a string becomes ""', async () => {
  const { I } = env();
  const item = makeItem({
    itemType: 'book',
    invalid: ['DOI', 'archiveID'],
    fields: { PMID: 12345, PMCID: false, url: null, extra: undefined },
  });
  const snap = await I.snapshot(item);
  assert.equal(snap.fields.doi, '');
  assert.equal(snap.fields.archive_id, '');
  assert.equal(snap.fields.pmid, '12345', 'numbers are stringified');
  assert.equal(snap.fields.pmcid, '');
  assert.equal(snap.fields.url, '');
  assert.equal(snap.fields.extra, '');
});

test('library kinds map onto user | group | feed; unknown libraries are reported not editable', async () => {
  const { I, zotero } = env();
  assert.deepEqual(plain((await I.snapshot(makeItem({ libraryID: 2 }))).library), { id: 2, kind: 'group', files_editable: false, name: 'Lab group' });
  assert.deepEqual(plain((await I.snapshot(makeItem({ libraryID: 4 }))).library), { id: 4, kind: 'user', files_editable: true, name: 'My Publications' });
  const feed = await I.snapshot(makeItem({ libraryID: 3 }));
  assert.equal(feed.library.kind, 'feed');
  assert.equal(feed.feed, true, 'an item in a feed library is a feed item');
  const unknown = await I.snapshot(makeItem({ libraryID: 99 }));
  assert.deepEqual(plain(unknown.library), { id: 99, kind: 'user', files_editable: false, name: '' });
  assert.ok(zotero.debugLog.some(([, m]) => /library 99/.test(m)));
});

test('a throwing Libraries.get or filesEditable getter does not break the snapshot', async () => {
  const { I } = env({
    Libraries: {
      get(id) {
        if (id === 7) throw new Error('db');
        return { libraryType: 'group', get filesEditable() { throw new Error('boom'); }, get name() { throw new Error('boom'); } };
      },
    },
  });
  const a = await I.snapshot(makeItem({ libraryID: 7 }));
  assert.deepEqual(plain(a.library), { id: 7, kind: 'user', files_editable: false, name: '' });
  const b = await I.snapshot(makeItem({ libraryID: 8 }));
  assert.deepEqual(plain(b.library), { id: 8, kind: 'group', files_editable: false, name: '' });
});

test('feed items (isFeedItem) are feed and get no attachment queries', async () => {
  const { I } = env();
  const item = makeItem({ isFeedItem: true, pdfCount: 5, best: { type: 'pdf', exists: true } });
  const snap = await I.snapshot(item);
  assert.equal(snap.feed, true);
  assert.equal(snap.regular, true);
  assert.equal(snap.pdf_attachments, 0);
  assert.deepEqual(plain(snap.best_attachment), { kind: 'none', exists: null });
  assert.equal(item.calls.some((c) => /Attachment/.test(c)), false);
});

test('non-regular items (attachment, note) get no attachment queries: 0 and {none, null}', async () => {
  const { I } = env();
  for (const itemType of ['attachment', 'note', 'annotation']) {
    const item = makeItem({ itemType, regular: false, pdfCount: 3, best: { type: 'pdf', exists: true } });
    const snap = await I.snapshot(item);
    assert.equal(snap.regular, false, itemType);
    assert.equal(snap.item_type, itemType);
    assert.equal(snap.pdf_attachments, 0);
    assert.deepEqual(plain(snap.best_attachment), { kind: 'none', exists: null });
    assert.equal(item.calls.includes('getBestAttachmentState'), false, itemType);
    assert.equal(item.calls.some((c) => c.startsWith('numFileAttachments')), false, itemType);
  }
});

test('a throwing isRegularItem counts as not regular', async () => {
  const { I } = env();
  const snap = await I.snapshot(makeItem({ regularThrows: true }));
  assert.equal(snap.regular, false);
});

test('best_attachment maps pdf, none and everything else', async () => {
  const { I } = env();
  const cases = [
    [{ type: 'pdf', exists: true }, { kind: 'pdf', exists: true }],
    [{ type: 'pdf', exists: false }, { kind: 'pdf', exists: false }],
    [{ type: 'none' }, { kind: 'none', exists: null }],
    [{ type: 'snapshot', exists: true }, { kind: 'other', exists: true }],
    [{ type: 'epub', exists: false }, { kind: 'other', exists: false }],
    [{ type: 'image', exists: true }, { kind: 'other', exists: true }],
    [{ type: 'video', exists: true }, { kind: 'other', exists: true }],
    [{ type: 'other', exists: true }, { kind: 'other', exists: true }],
    [{ type: 'pdf' }, { kind: 'pdf', exists: null }],
    [{ type: 'none', exists: true }, { kind: 'none', exists: null }],
    [null, { kind: 'none', exists: null }],
    [{}, { kind: 'none', exists: null }],
  ];
  for (const [state, expected] of cases) {
    const snap = await I.snapshot(makeItem({ best: state }));
    assert.deepEqual(plain(snap.best_attachment), expected, JSON.stringify(state));
  }
});

test('pdf_attachments counts application/pdf file attachments and never goes negative or fractional', async () => {
  const { I } = env();
  const item = makeItem({ pdfCount: 3 });
  assert.equal((await I.snapshot(item)).pdf_attachments, 3);
  assert.ok(item.calls.includes('numFileAttachmentsWithContentType:application/pdf'));
  assert.equal((await I.snapshot(makeItem({ pdfCount: -2 }))).pdf_attachments, 0);
  assert.equal((await I.snapshot(makeItem({ pdfCount: 2.9 }))).pdf_attachments, 2);
  assert.equal((await I.snapshot(makeItem({ pdfCount: NaN }))).pdf_attachments, 0);
});

test('childItems and itemData are loaded first when the item says they are not', async () => {
  const { I } = env();
  const item = makeItem({ unloaded: ['itemData', 'childItems'], pdfCount: 1, best: { type: 'pdf', exists: true } });
  await I.snapshot(item);
  const order = item.calls.filter((c) => c.startsWith('loadDataType') || c === 'getField:DOI' || c.startsWith('numFile'));
  assert.deepEqual(order, ['loadDataType:itemData', 'getField:DOI', 'loadDataType:childItems', 'numFileAttachmentsWithContentType:application/pdf']);
  const loaded = makeItem({ pdfCount: 1 });
  await I.snapshot(loaded);
  assert.equal(loaded.calls.some((c) => c.startsWith('loadDataType')), false, 'already loaded: no reload');
  const bare = makeItem({ noLoadedFlags: true });
  await I.snapshot(bare);
  assert.equal(bare.calls.some((c) => c.startsWith('loadDataType')), false, 'no _loaded flags: nothing to load');
});

test('a failing loadDataType is logged and the snapshot continues', async () => {
  const { I, zotero } = env();
  const item = makeItem({ unloaded: ['itemData'] });
  item.loadDataType = async () => { throw new Error('db closed'); };
  const snap = await I.snapshot(item);
  assert.equal(snap.fields.doi, '10.1371/journal.pmed.1000097');
  assert.ok(zotero.debugLog.some(([, m]) => /loadDataType\(itemData\) failed/.test(m)));
});

test('title: display title preferred, then the title field; trimmed and limited to 300 characters', async () => {
  const { I } = env();
  assert.equal((await I.snapshot(makeItem({ displayTitle: '  Shown title \n', fields: { title: 'Field title' } }))).title, 'Shown title');
  assert.equal((await I.snapshot(makeItem({ displayTitle: '', fields: { title: ' Field title ' } }))).title, 'Field title');
  assert.equal((await I.snapshot(makeItem({ displayTitleThrows: true, fields: { title: 'Field title' } }))).title, 'Field title');
  assert.equal((await I.snapshot(makeItem({ deleteGetDisplayTitle: true, fields: { title: 'Field title' } }))).title, 'Field title');
  assert.equal((await I.snapshot(makeItem({ displayTitle: '', fields: { title: '' } }))).title, '');
  const long = 'x'.repeat(500);
  const t = (await I.snapshot(makeItem({ displayTitle: long }))).title;
  assert.equal(t.length, 300);
});

test('title clipping never splits a surrogate pair and lone surrogates are replaced', async () => {
  const { I } = env();
  const title = 'a'.repeat(299) + '\u{1F9EC}' + 'tail';
  const clipped = (await I.snapshot(makeItem({ displayTitle: title }))).title;
  assert.equal(Array.from(clipped).length, 300);
  assert.equal(clipped.endsWith('\u{1F9EC}'), true);
  assert.doesNotThrow(() => new TextEncoder().encode(clipped));
  const lone = (await I.snapshot(makeItem({ displayTitle: 'bad \uD800 title', fields: { extra: 'x\uDC00y' } })));
  assert.equal(lone.title, 'bad � title');
  assert.equal(lone.fields.extra, 'x�y');
  // The JSON that goes to the engine round-trips through UTF-8 without loss.
  const json = JSON.stringify(lone);
  assert.equal(new TextDecoder().decode(new TextEncoder().encode(json)), json);
});

test('snapshot rejects things that are not items', async () => {
  const { I } = env();
  for (const bad of [null, undefined, 5, 'x', {}, { id: 3 }, { getField() {} }, { id: 'a', getField() {} }]) {
    await assert.rejects(() => I.snapshot(bad), /not a Zotero item/);
  }
});

test('an attachment-query failure on a regular item makes snapshot throw instead of guessing "no PDF"', async () => {
  const { I } = env();
  await assert.rejects(() => I.snapshot(makeItem({ countThrows: true })), /childItems not loaded/);
  await assert.rejects(() => I.snapshot(makeItem({ stateThrows: true })), /db locked/);
});

test('snapshotAll keeps input order and snapshots every item', async () => {
  const { I } = env();
  const items = [makeItem({ id: 3 }), makeItem({ id: 1 }), makeItem({ id: 2, itemType: 'note', regular: false })];
  const snaps = await I.snapshotAll(items);
  assert.deepEqual(plain(snaps.map((s) => s.item)), [3, 1, 2]);
  assert.deepEqual(plain(await I.snapshotAll([])), []);
  assert.deepEqual(plain(await I.snapshotAll(null)), []);
  assert.deepEqual(plain(await I.snapshotAll(undefined)), []);
  assert.deepEqual(plain((await I.snapshotAll(new Set([makeItem({ id: 8 })]))).map((s) => s.item)), [8], 'any iterable works');
});

test('snapshotAll replaces a failing item by a not-regular stub, logs it, and never rejects', async () => {
  const { I, zotero } = env();
  const items = [makeItem({ id: 1 }), makeItem({ id: 2, key: 'BADKEY22', libraryID: 2, stateThrows: true }), makeItem({ id: 3 })];
  const snaps = await I.snapshotAll(items);
  assert.deepEqual(plain(snaps.map((s) => s.item)), [1, 2, 3]);
  assert.deepEqual(plain(snaps[1]), {
    item: 2,
    key: '2/BADKEY22',
    library: { id: 2, kind: 'user', files_editable: false, name: '' },
    regular: false,
    feed: false,
    item_type: '',
    title: '',
    fields: { doi: '', pmid: '', pmcid: '', url: '', archive_id: '', extra: '' },
    pdf_attachments: 0,
    best_attachment: { kind: 'none', exists: null },
  });
  assert.deepEqual(plain(Object.keys(snaps[1])), SNAPSHOT_KEYS, 'the stub has the full shape');
  assert.equal(snaps[0].regular, true);
  assert.ok(zotero.debugLog.some(([, m]) => /snapshot of item 2 failed/.test(m)));
});

test('snapshotAll drops entries that are not items at all (no id to report them under)', async () => {
  const { I, zotero } = env();
  const snaps = await I.snapshotAll([makeItem({ id: 1 }), null, { id: 'x' }, 7, makeItem({ id: 2 })]);
  assert.deepEqual(plain(snaps.map((s) => s.item)), [1, 2]);
  assert.ok(zotero.debugLog.some(([, m]) => /not a Zotero item/.test(m)));
});

test('snapshotAll yields to the event loop every 25 items', async () => {
  const { I, ZFT } = env();
  let idles = 0;
  ZFT.util.idle = () => { idles += 1; return Promise.resolve(); };
  const items = Array.from({ length: 60 }, (_, i) => makeItem({ id: i + 1 }));
  const snaps = await I.snapshotAll(items);
  assert.equal(snaps.length, 60);
  assert.equal(idles, 2, 'after item 25 and after item 50');
  idles = 0;
  await I.snapshotAll(items.slice(0, 25));
  assert.equal(idles, 0, '25 items need no yield');
  await I.snapshotAll(items.slice(0, 26));
  assert.equal(idles, 1);
});

test('menuEligible: true when any selected item is a regular, non-feed item; never throws', () => {
  const { I } = env();
  const regular = makeItem({ regular: true });
  const note = makeItem({ regular: false });
  const feed = makeItem({ regular: true, isFeedItem: true });
  assert.equal(I.menuEligible([regular]), true);
  assert.equal(I.menuEligible([note, regular]), true);
  assert.equal(I.menuEligible([note]), false);
  assert.equal(I.menuEligible([feed]), false);
  assert.equal(I.menuEligible([note, feed]), false);
  assert.equal(I.menuEligible([feed, regular]), true);
  assert.equal(I.menuEligible([]), false);
  assert.equal(I.menuEligible(null), false);
  assert.equal(I.menuEligible(undefined), false);
  assert.equal(I.menuEligible('items'), false);
  assert.equal(I.menuEligible([null, undefined, 3, 'x', {}]), false);
  assert.equal(I.menuEligible([makeItem({ regularThrows: true })]), false);
  assert.equal(I.menuEligible([null, regular]), true);
});

test('menuEligible is synchronous and reads no fields', () => {
  const { I } = env();
  const item = makeItem({ regular: true });
  const result = I.menuEligible([item]);
  assert.equal(typeof result, 'boolean');
  assert.deepEqual(item.calls, ['isRegularItem']);
});

test('getItem looks the item up through Zotero.Items and returns null for anything missing', () => {
  const { I, itemsById } = env();
  const item = makeItem({ id: 5 });
  itemsById.set(5, item);
  assert.equal(I.getItem(5), item);
  assert.equal(I.getItem(6), null, 'Zotero returns false for a missing item');
  assert.equal(I.getItem('5'), null);
  assert.equal(I.getItem(null), null);
  assert.equal(I.getItem(1.5), null);
});

test('getItem returns null when Zotero.Items.get throws', () => {
  const { I } = env({ Items: { get() { throw new Error('closed'); } } });
  assert.equal(I.getItem(1), null);
});

test('watchDeletions registers an item observer and calls onGone for delete and trash events', () => {
  const { I, notifier } = env();
  const gone = [];
  const unregister = I.watchDeletions((id) => gone.push(id));
  assert.equal(notifier.registered.length, 1);
  const reg = notifier.registered[0];
  assert.deepEqual(plain(reg.types), ['item']);
  assert.equal(reg.id, 'zotero-fulltext');
  assert.equal(typeof reg.observer.notify, 'function');

  reg.observer.notify('delete', 'item', [1, 2]);
  reg.observer.notify('trash', 'item', [3]);
  reg.observer.notify('modify', 'item', [4]);
  reg.observer.notify('add', 'item', [5]);
  reg.observer.notify('delete', 'collection', [6]);
  reg.observer.notify('trash', 'collection-item', [7]);
  reg.observer.notify('delete', 'item', []);
  reg.observer.notify('delete', 'item', undefined);
  reg.observer.notify('trash', 'item', ['8', 'x', 9.5, null]);
  assert.deepEqual(gone, [1, 2, 3, 8]);

  assert.equal(notifier.unregistered.length, 0);
  unregister();
  assert.deepEqual(notifier.unregistered, ['obs-1']);
  unregister();
  assert.deepEqual(notifier.unregistered, ['obs-1'], 'unregistering twice does nothing');
});

test('watchDeletions: an exception in onGone is logged and the remaining ids are still delivered', () => {
  const { I, notifier, zotero } = env();
  const gone = [];
  I.watchDeletions((id) => { gone.push(id); if (id === 1) throw new Error('listener bug'); });
  assert.doesNotThrow(() => notifier.registered[0].observer.notify('delete', 'item', [1, 2]));
  assert.deepEqual(gone, [1, 2]);
  assert.ok(zotero.debugLog.some(([, m]) => /deletion callback failed for item 1/.test(m)));
});

test('watchDeletions: failing registration or unregistration is logged, never thrown', () => {
  const failing = env();
  failing.notifier.registerObserver = () => { throw new Error('no notifier'); };
  const off = failing.I.watchDeletions(() => {});
  assert.doesNotThrow(() => off());
  assert.ok(failing.zotero.debugLog.some(([, m]) => /could not observe item deletions/.test(m)));

  const other = env();
  other.notifier.unregisterObserver = () => { throw new Error('gone already'); };
  const off2 = other.I.watchDeletions(() => {});
  assert.doesNotThrow(() => off2());
  assert.ok(other.zotero.debugLog.some(([, m]) => /unregisterObserver failed/.test(m)));
});
