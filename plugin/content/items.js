/* ZFT.items: Zotero.Item -> proto/model ItemSnapshot (cheap field reads only) and item lookups.
 *
 * Owner: js-runs. The snapshot is the ONLY item data the engine sees; identifier cleaning, eligibility
 * decisions (regular? feed? has PDF? files editable?) and DOI normalisation all happen in Rust. JS reads fields
 * and reports facts. Verified APIs (Zotero 10.0.6):
 *   item.id, item.key, item.libraryID, item.itemType, item.getDisplayTitle() / item.getField('title'),
 *   item.isRegularItem() (data/item.js:2447), item.isFeedItem (FeedItem getter; undefined on Zotero.Item),
 *   item.getField(name) for 'DOI' | 'PMID' | 'PMCID' | 'url' | 'archiveID' | 'extra' (guard with
 *   Zotero.ItemFields.isValidForType(fieldID, item.itemTypeID) or try/catch: not every type has each field),
 *   item.numFileAttachmentsWithContentType('application/pdf') (:2857, needs 'childItems' loaded:
 *   await item.loadDataType('childItems') first),
 *   await item.getBestAttachmentState() (:4347) -> {type: 'none'|'pdf'|'snapshot'|'epub'|'image'|'video'|'other', exists, key},
 *   Zotero.Libraries.get(libraryID) -> {libraryType: 'user'|'group'|'feed', filesEditable, name}.
 * Mapping: library.kind = libraryType; best_attachment.kind = type 'pdf' -> 'pdf', 'none' -> 'none', else 'other';
 * best_attachment.exists = state.exists (null for 'none'); key = `${libraryID}/${item.key}`; feed = library is a feed
 * or item.isFeedItem; regular = item.isRegularItem().
 *
 * Choices made here:
 *  - PMID and PMCID are not Zotero fields: getField returns '' for them and the Rust side finds them in `extra`.
 *  - Every string is made well-formed (lone surrogates replaced): serde_json rejects them, and one bad title
 *    would otherwise fail the whole StartRun command.
 *  - The attachment queries are NOT swallowed: if they fail, snapshot() throws and snapshotAll() reports the
 *    item as a not-regular stub. That skips the item; guessing "no PDF" could attach a duplicate.
 *  - Library kinds the model does not know (e.g. 'publications', My Publications) map to 'user'.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const TITLE_MAX = 300;
  /** snapshotAll yields to the event loop after this many items. */
  const YIELD_EVERY = 25;

  const log = (level, msg) => ZFT.util.log(level, 'items: ' + msg);

  /** A string safe to put into JSON for serde: '' for null/false/undefined, lone surrogates replaced. */
  function text(value) {
    if (value === null || value === undefined || value === false) return '';
    const s = String(value);
    return typeof s.toWellFormed === 'function' ? s.toWellFormed() : s;
  }
  /** First `max` code points of s (never splits a surrogate pair). */
  function clip(s, max) {
    const points = Array.from(s);
    return points.length > max ? points.slice(0, max).join('') : s;
  }
  function isItem(item) {
    return !!item && typeof item === 'object' && typeof item.getField === 'function' && Number.isInteger(item.id);
  }

  /** item.getField(name) as a string; '' when the field is not valid for the item type or the read throws. */
  function fieldOrEmpty(item, name) {
    try {
      return text(item.getField(name));
    } catch (e) {
      return '';
    }
  }

  function titleOf(item) {
    let title = '';
    try {
      if (typeof item.getDisplayTitle === 'function') title = item.getDisplayTitle();
    } catch (e) { /* fall back to the plain field */ }
    if (!title) title = fieldOrEmpty(item, 'title');
    return clip(text(title).trim(), TITLE_MAX);
  }

  /** Load a data type the item says it lacks (items from the items tree are normally loaded already). */
  async function ensureLoaded(item, type) {
    try {
      if (item._loaded && !item._loaded[type] && typeof item.loadDataType === 'function') await item.loadDataType(type);
    } catch (e) {
      log('warn', `loadDataType(${type}) failed for item ${item.id}: ${ZFT.util.errText(e)}`);
    }
  }

  /** @returns {{info: object, feed: boolean}} model.rs LibraryInfo plus whether the library is a feed */
  function libraryOf(item) {
    const id = Number.isInteger(item.libraryID) ? item.libraryID : 0;
    let lib = null;
    try {
      lib = Zotero.Libraries.get(id);
    } catch (e) { /* handled below */ }
    if (!lib) {
      // Unknown library: report it as not file-editable so nothing is ever saved into it.
      log('warn', `library ${id} of item ${item.id} not found`);
      return { info: { id, kind: 'user', files_editable: false, name: '' }, feed: false };
    }
    let filesEditable = false;
    try { filesEditable = !!lib.filesEditable; } catch (e) { /* stays false */ }
    let name = '';
    try { name = text(lib.name); } catch (e) { /* stays empty */ }
    return {
      info: { id, kind: kindOf(lib.libraryType), files_editable: filesEditable, name },
      feed: lib.libraryType === 'feed',
    };
  }

  /** model.rs LibraryKind of a Zotero libraryType; 'publications' (My Publications) and anything unknown count as the user library. */
  function kindOf(libraryType) {
    return libraryType === 'group' || libraryType === 'feed' ? libraryType : 'user';
  }

  function bestAttachmentOf(state) {
    const type = state && state.type;
    if (!type || type === 'none') return { kind: 'none', exists: null };
    return { kind: type === 'pdf' ? 'pdf' : 'other', exists: typeof state.exists === 'boolean' ? state.exists : null };
  }

  function callBool(item, method) {
    try {
      return typeof item[method] === 'function' ? !!item[method]() : false;
    } catch (e) {
      return false;
    }
  }

  /**
   * @param {object} item Zotero.Item
   * @returns {Promise<object>} model.rs ItemSnapshot {item, key, library, regular, feed, item_type, title, fields, pdf_attachments, best_attachment}
   * @throws {Error} not for missing fields (they become ''), only when `item` is not an item object or the
   *   attachment queries of a regular item fail
   */
  async function snapshot(item) {
    if (!isItem(item)) throw new Error('ZFT.items.snapshot: not a Zotero item');
    await ensureLoaded(item, 'itemData');
    const library = libraryOf(item);
    const feed = library.feed || !!item.isFeedItem;
    const regular = callBool(item, 'isRegularItem');
    const snap = {
      item: item.id,
      key: `${item.libraryID}/${text(item.key)}`,
      library: library.info,
      regular,
      feed,
      item_type: text(item.itemType),
      title: titleOf(item),
      fields: {
        doi: fieldOrEmpty(item, 'DOI'),
        pmid: fieldOrEmpty(item, 'PMID'),
        pmcid: fieldOrEmpty(item, 'PMCID'),
        url: fieldOrEmpty(item, 'url'),
        archive_id: fieldOrEmpty(item, 'archiveID'),
        extra: fieldOrEmpty(item, 'extra'),
      },
      pdf_attachments: 0,
      best_attachment: { kind: 'none', exists: null },
    };
    if (regular && !feed) {
      await ensureLoaded(item, 'childItems');
      const pdfs = Number(item.numFileAttachmentsWithContentType('application/pdf'));
      snap.pdf_attachments = Number.isFinite(pdfs) && pdfs > 0 ? Math.trunc(pdfs) : 0;
      snap.best_attachment = bestAttachmentOf(await item.getBestAttachmentState());
    }
    return snap;
  }

  /** The stand-in for an item whose snapshot failed: valid ItemSnapshot, not regular, so the engine skips it. */
  function stub(item) {
    const libraryID = Number.isInteger(item.libraryID) ? item.libraryID : 0;
    return {
      item: item.id,
      key: `${libraryID}/${text(item.key)}`,
      library: { id: libraryID, kind: 'user', files_editable: false, name: '' },
      regular: false,
      feed: false,
      item_type: '',
      title: '',
      fields: { doi: '', pmid: '', pmcid: '', url: '', archive_id: '', extra: '' },
      pdf_attachments: 0,
      best_attachment: { kind: 'none', exists: null },
    };
  }

  /**
   * Snapshot many items; one failing item is replaced by a minimal not-regular snapshot and logged.
   * Entries that are not items at all (no integer id) cannot be given a snapshot and are dropped (logged).
   * Never rejects. @param {object[]} items @returns {Promise<object[]>} ItemSnapshot[] in input order
   */
  async function snapshotAll(items) {
    const out = [];
    let list = [];
    try { list = Array.from(items || []); } catch (e) { log('warn', 'snapshotAll: items is not iterable'); }
    let seen = 0;
    for (const item of list) {
      if (seen > 0 && seen % YIELD_EVERY === 0) await ZFT.util.idle();
      seen += 1;
      try {
        out.push(await snapshot(item));
      } catch (e) {
        if (isItem(item)) {
          log('warn', `snapshot of item ${item.id} failed, reporting it as not regular: ${ZFT.util.errText(e)}`);
          out.push(stub(item));
        } else {
          log('warn', 'snapshotAll: skipped an entry that is not a Zotero item');
        }
      }
    }
    return out;
  }

  /**
   * Synchronous menu pre-check (onShowing): true when at least one selected item is a regular, non-feed item.
   * No awaits, no database reads beyond already-loaded item fields. @param {object[]} items @returns {boolean}
   */
  function menuEligible(items) {
    if (!Array.isArray(items)) return false;
    for (const item of items) {
      if (item && typeof item === 'object' && callBool(item, 'isRegularItem') && !item.isFeedItem) return true;
    }
    return false;
  }

  /** @param {number} id @returns {object|null} Zotero.Items.get(id) or null */
  function getItem(id) {
    if (!Number.isInteger(id)) return null;
    try {
      return Zotero.Items.get(id) || null;
    } catch (e) {
      return null;
    }
  }

  /**
   * Observe item deletions/trashing so the run controller can cancel those items.
   * Notifier events 'delete' and 'trash' (type 'item') call onGone(itemID) once per id; exceptions from
   * onGone are caught and logged. If the observer cannot be registered the returned function is a no-op
   * (SavePdf re-checks the parent anyway). @param {(itemID: number) => void} onGone @returns {() => void} unregister
   */
  function watchDeletions(onGone) {
    const observer = {
      notify(event, type, ids) {
        if (type !== 'item' || (event !== 'delete' && event !== 'trash')) return;
        for (const raw of Array.from(ids || [])) {
          const id = typeof raw === 'string' && raw.trim() !== '' ? Number(raw) : raw;
          if (!Number.isInteger(id)) continue;
          try {
            onGone(id);
          } catch (e) {
            log('error', `deletion callback failed for item ${id}: ${ZFT.util.errText(e)}`);
          }
        }
      },
    };
    let observerID = null;
    try {
      observerID = Zotero.Notifier.registerObserver(observer, ['item'], 'zotero-fulltext');
    } catch (e) {
      log('error', `could not observe item deletions: ${ZFT.util.errText(e)}`);
      return () => {};
    }
    let active = true;
    return () => {
      if (!active) return;
      active = false;
      try {
        Zotero.Notifier.unregisterObserver(observerID);
      } catch (e) {
        log('warn', `unregisterObserver failed: ${ZFT.util.errText(e)}`);
      }
    };
  }

  ZFT.items = { snapshot, snapshotAll, menuEligible, getItem, watchDeletions };
})(this);
