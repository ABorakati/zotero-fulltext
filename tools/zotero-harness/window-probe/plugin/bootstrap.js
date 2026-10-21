/* zotero-fulltext WINDOW probe (bootstrap.js). Runs ONLY in the throwaway profile that tools/zotero-harness/window-probe.ps1 creates.
 *
 * Purpose: find out which windows a real "Get Full Text" run opens. It creates journal items for paywalled DOIs, selects them in the
 * items tree, builds the REAL item context menu and fires the REAL "Get Full Text" menu command (the same path a right-click takes),
 * and while the run goes it records EVERY window the application opens:
 *   - a domwindowopened observer (the opener's JS stack is captured inside the notification, i.e. while openWindow is still on the stack),
 *   - wrapped Services.ww.openWindow / Services.prompt.* / Zotero.openInViewer / Zotero.launchURL / Zotero.launchFile / main-window
 *     open+openDialog (each call with a stack),
 *   - a 300 ms poll over Services.wm.getEnumerator('') (windowtype, documentURI, title, size, readyState, visible text, element rects,
 *     browser URIs) and a PNG of each window once it has had time to render,
 *   - every console error (Services.console listener) with its source, so a load-time exception is attributed to its window.
 * It never types into a page, never signs in and never submits a form. Sign-in windows are only sampled and then closed.
 *
 * Prefs (set by window-probe.ps1): extensions.zotero-wprobe.{resultFile, shotDir, dois (JSON array of {doi,title?}), runSec, settleSec}.
 */
/* global Zotero, Services, Ci, Components, IOUtils, PathUtils, ChromeUtils */

var WProbe = (function () {
  'use strict';

  const PREF = 'extensions.zotero-wprobe.';
  const ZFT_WAIT_MS = 90000;
  const DEFAULT_DOIS = [
    { doi: '10.1111/cts.13276', title: 'Wiley CTS probe' },
    { doi: '10.1111/ctr.14066', title: 'Wiley Clin Transplant probe' },
    { doi: '10.1097/SLA.0000000000005603', title: 'LWW Annals of Surgery probe' },
    { doi: '10.1016/S0140-6736(20)30183-5', title: 'Lancet probe' },
  ];

  let S = null;

  function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
  function errText(e) {
    if (e === undefined || e === null) return String(e);
    return ((e.name ? e.name + ': ' : '') + (e.message !== undefined ? e.message : String(e))).slice(0, 500);
  }
  function prefStr(n, d) { try { return Services.prefs.getStringPref(n, d); } catch (e) { return d; } }
  function prefInt(n, d) { try { return Services.prefs.getIntPref(n, d); } catch (e) { return d; } }
  function plog(msg) {
    const line = '+' + (S ? Date.now() - S.t0 : 0) + 'ms ' + msg;
    if (S) S.R.driverLog.push(line);
    try { Zotero.debug('[zotero-fulltext-window-probe] ' + msg); } catch (e) { /* ignore */ }
  }
  function stackNow(skip) {
    try {
      const lines = String(new Error().stack).split('\n');
      return lines.slice(skip || 1, (skip || 1) + 28).map((l) => l.slice(0, 220));
    } catch (e) { return ['(no stack)']; }
  }
  function brief(v) {
    try {
      if (v === null || v === undefined) return v;
      if (typeof v === 'string') return v.slice(0, 200);
      if (typeof v === 'number' || typeof v === 'boolean') return v;
      if (typeof v === 'function') return '(function)';
      return '(' + typeof v + ')' + (v.constructor && v.constructor.name ? ' ' + v.constructor.name : '');
    } catch (e) { return '(unprintable)'; }
  }

  // ------------------------------------------------------------------ the window registry

  function winKey(win) {
    try {
      if (!win.__zftWprobeId) win.__zftWprobeId = 'w' + (++S.nextId);
      return win.__zftWprobeId;
    } catch (e) { return null; }
  }

  function describeStatic(win) {
    const d = {};
    try { d.location = String(win.location && win.location.href); } catch (e) { d.location = '(throws)'; }
    try {
      const doc = win.document;
      d.documentURI = String(doc.documentURI);
      d.readyState = doc.readyState;
      d.title = doc.title;
      d.contentType = doc.contentType;
      const root = doc.documentElement;
      d.rootTag = root ? root.localName : null;
      d.rootId = root ? root.id : null;
      d.windowtype = root ? root.getAttribute('windowtype') : null;
      d.rootStyleAttr = root ? root.getAttribute('style') : null;
      d.rootTitleAttr = root ? root.getAttribute('title') : null;
    } catch (e) { d.docError = errText(e); }
    try { d.inner = [win.innerWidth, win.innerHeight]; d.outer = [win.outerWidth, win.outerHeight]; d.screenXY = [win.screenX, win.screenY]; } catch (e) { /* ignore */ }
    try { d.windowState = win.windowState; } catch (e) { /* ignore */ }
    try { d.hasArguments = !!(win.arguments && win.arguments.length); d.argTypes = win.arguments ? Array.from(win.arguments).map(brief) : null; } catch (e) { /* ignore */ }
    try { d.closed = !!win.closed; } catch (e) { /* ignore */ }
    return d;
  }

  function describeDeep(win) {
    const d = describeStatic(win);
    try {
      const doc = win.document;
      const root = doc.documentElement;
      d.textHead = String((root && root.textContent) || '').replace(/\s+/g, ' ').trim().slice(0, 400);
      const rect = (el) => { const r = el.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; };
      d.children = root ? Array.from(root.children).map((c) => ({ tag: c.localName, id: c.id || '', rect: rect(c), hidden: !!c.hidden })).slice(0, 12) : [];
      const vb = doc.querySelector('vbox, hbox, html\\:body, body');
      if (vb) d.firstBoxChildren = Array.from(vb.children).map((c) => ({ tag: c.localName, id: c.id || '', rect: rect(c), hidden: !!c.hidden, disp: win.getComputedStyle(c).display })).slice(0, 14);
      d.bg = win.getComputedStyle(root).backgroundColor;
      d.browsers = Array.from(doc.querySelectorAll('browser, iframe')).map((b) => {
        const o = { tag: b.localName, id: b.id || '', rect: rect(b), src: b.getAttribute('src') };
        try { o.currentURI = b.currentURI ? b.currentURI.spec : null; } catch (e) { o.currentURI = '(throws)'; }
        try { o.docURI = b.contentDocument ? String(b.contentDocument.documentURI) : null; } catch (e) { o.docURI = '(throws)'; }
        try { o.remote = !!b.isRemoteBrowser; o.remoteType = b.remoteType; } catch (e) { /* ignore */ }
        return o;
      }).slice(0, 6);
      d.l10nPending = (() => { try { return doc.l10n ? 'l10n present' : 'no document.l10n'; } catch (e) { return '(throws)'; } })();
    } catch (e) { d.deepError = errText(e); }
    return d;
  }

  async function shot(win, path) {
    try {
      await IOUtils.makeDirectory(PathUtils.parent(path), { createAncestors: true, ignoreExisting: true });
      const host = Zotero.getMainWindow();
      const canvas = host.document.createElementNS('http://www.w3.org/1999/xhtml', 'canvas');
      const w = Math.max(1, win.innerWidth);
      const h = Math.max(1, win.innerHeight);
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext('2d');
      ctx.drawWindow(win, 0, 0, w, h, 'rgb(40,40,40)', ctx.DRAWWINDOW_DRAW_VIEW | ctx.DRAWWINDOW_DRAW_CARET);
      const notes = [];
      for (const b of win.document.querySelectorAll('browser')) {
        try {
          const r = b.getBoundingClientRect();
          if (!r.width || !r.height) continue;
          const wg = b.browsingContext && b.browsingContext.currentWindowGlobal;
          if (!wg || typeof wg.drawSnapshot !== 'function') continue;
          const bmp = await wg.drawSnapshot(new win.DOMRect(0, 0, r.width, r.height), 1, 'rgb(255,255,255)', false);
          ctx.drawImage(bmp, r.x, r.y);
          notes.push('page ' + bmp.width + 'x' + bmp.height);
        } catch (e) { notes.push('snapshot failed: ' + errText(e)); }
      }
      const blob = await new Promise((res) => canvas.toBlob(res, 'image/png'));
      await IOUtils.write(path, new Uint8Array(await blob.arrayBuffer()));
      return { path, w, h, notes };
    } catch (e) {
      return { error: errText(e) };
    }
  }

  function record(win, how, extra) {
    const id = winKey(win);
    if (!id) return null;
    let rec = S.windows.get(id);
    if (!rec) {
      rec = { id, firstSeenMs: Date.now() - S.t0, how, samples: [], shots: [], errors: [] };
      S.windows.set(id, rec);
    }
    if (extra) Object.assign(rec, extra);
    return rec;
  }

  function onOpened(subject) {
    try {
      const win = subject;
      const rec = record(win, 'domwindowopened', { openerStack: stackNow(2), openedAtMs: Date.now() - S.t0, atOpen: describeStatic(win) });
      // the wrapped openWindow (if installed) adds the exact arguments
      if (S.lastOpenCall && Date.now() - S.lastOpenCall.t < 2000) rec.openCall = S.lastOpenCall;
      plog('window opened ' + rec.id + ' ' + JSON.stringify({ at: rec.atOpen.location, top: rec.openerStack[0] }));
      const onLoad = () => {
        rec.loadedMs = Date.now() - S.t0;
        rec.afterLoad = describeStatic(win);
      };
      try { win.addEventListener('load', onLoad, { once: true }); } catch (e) { /* ignore */ }
      try { win.addEventListener('unload', () => { rec.unloadMs = Date.now() - S.t0; }, { once: true }); } catch (e) { /* ignore */ }
    } catch (e) { plog('observer error ' + errText(e)); }
  }

  const observer = {
    observe(subject, topic) {
      if (topic === 'domwindowopened') onOpened(subject);
      else if (topic === 'domwindowclosed') {
        try { const r = S.windows.get(subject.__zftWprobeId); if (r) r.closedMs = Date.now() - S.t0; } catch (e) { /* ignore */ }
      }
    },
    QueryInterface: ChromeUtils.generateQI(['nsIObserver']),
  };

  const consoleListener = {
    observe(msg) {
      try {
        let text = '';
        let src = '';
        let line = 0;
        let kind = 'message';
        if (msg instanceof Ci.nsIScriptError) {
          kind = (msg.flags & 0x2) ? 'warning' : 'error';
          text = msg.errorMessage;
          src = msg.sourceName;
          line = msg.lineNumber;
          if (kind === 'warning') return;
        } else {
          text = msg.message;
        }
        if (!text || !/(zotero-fulltext|chrome:\/\/zotero-fulltext|login\.|progress|Error|exception|TypeError|ReferenceError|NS_ERROR)/i.test(text + ' ' + src)) return;
        S.R.consoleErrors.push({ atMs: Date.now() - S.t0, kind, src: String(src).slice(0, 200), line, text: String(text).slice(0, 500) });
        if (S.R.consoleErrors.length > 400) S.R.consoleErrors.shift();
      } catch (e) { /* ignore */ }
    },
    QueryInterface: ChromeUtils.generateQI(['nsIConsoleListener']),
  };

  function allWindows() {
    const out = [];
    try {
      const en = Services.wm.getEnumerator(null);
      while (en.hasMoreElements()) out.push(en.getNext());
    } catch (e) { /* ignore */ }
    return out;
  }

  const MAIN = 'zotero:main';

  async function tick() {
    for (const win of allWindows()) {
      let isMain = false;
      try { isMain = win.location.href.indexOf('zoteroPane.xhtml') !== -1 || win.document.documentElement.getAttribute('windowtype') === MAIN; } catch (e) { /* ignore */ }
      if (isMain) continue;
      const rec = record(win, 'enumeration');
      if (!rec) continue;
      const age = Date.now() - S.t0 - (rec.firstSeenMs || 0);
      const snap = describeDeep(win);
      snap.atMs = Date.now() - S.t0;
      rec.samples.push(snap);
      if (rec.samples.length > 4) rec.samples.splice(1, 1); // keep first + latest 3
      rec.latest = snap;
      // PNGs: at ~1.5 s of age, and again at ~6 s
      const wants = (age > 1500 && rec.shots.length === 0) || (age > 6000 && rec.shots.length === 1);
      if (wants && S.shotDir && !win.closed) {
        const name = rec.id + '-' + (snap.windowtype || snap.rootId || 'window').replace(/[^a-z0-9_-]/gi, '_') + '-' + (Date.now() - S.t0) + '.png';
        const r = await shot(win, PathUtils.join(S.shotDir, name));
        rec.shots.push(r);
      }
      // A native prompt (commonDialog / unknownContentType / file picker) blocks a nested event loop: close it after it has been recorded.
      if (rec.shots.length >= 1 && /commonDialog|unknownContentType/i.test(String(snap.location) + ' ' + String(snap.documentURI)) && age > 4000) {
        try { win.close(); rec.closedByProbe = true; } catch (e) { /* ignore */ }
      }
    }
  }

  // ------------------------------------------------------------------ call wrappers (with stacks)

  function wrap(obj, name, label, notes) {
    try {
      if (!obj || typeof obj[name] !== 'function') { notes.push(label + ': not a function'); return; }
      const orig = obj[name];
      obj[name] = function (...args) {
        const entry = { atMs: Date.now() - S.t0, fn: label, args: args.map(brief), stack: stackNow(2) };
        S.R.calls.push(entry);
        if (label === 'Services.ww.openWindow') S.lastOpenCall = { t: Date.now(), args: args.slice(1, 4).map(brief) };
        plog('call ' + label + ' ' + JSON.stringify(entry.args).slice(0, 200));
        return orig.apply(this, args);
      };
      notes.push(label + ': wrapped');
    } catch (e) { notes.push(label + ': wrap failed ' + errText(e)); }
  }

  function installWrappers() {
    const notes = [];
    wrap(Services.ww, 'openWindow', 'Services.ww.openWindow', notes);
    for (const m of ['alert', 'alertCheck', 'confirm', 'confirmCheck', 'confirmEx', 'prompt', 'select']) wrap(Services.prompt, m, 'Services.prompt.' + m, notes);
    wrap(Zotero, 'openInViewer', 'Zotero.openInViewer', notes);
    wrap(Zotero, 'launchURL', 'Zotero.launchURL', notes);
    wrap(Zotero, 'launchFile', 'Zotero.launchFile', notes);
    wrap(Zotero, 'launchFileWithApplication', 'Zotero.launchFileWithApplication', notes);
    try {
      const mw = Zotero.getMainWindow();
      wrap(mw, 'open', 'mainWindow.open', notes);
      wrap(mw, 'openDialog', 'mainWindow.openDialog', notes);
    } catch (e) { notes.push('main window wrap failed ' + errText(e)); }
    try { wrap(Zotero.Utilities.Internal, 'openPreferences', 'Zotero.Utilities.Internal.openPreferences', notes); } catch (e) { /* ignore */ }
    S.R.meta.wrappers = notes;
  }

  // ------------------------------------------------------------------ the run

  async function createItems(list) {
    const items = [];
    for (const d of list) {
      const item = new Zotero.Item('journalArticle');
      item.libraryID = Zotero.Libraries.userLibraryID;
      item.setField('title', d.title || ('Probe ' + d.doi));
      item.setField('DOI', d.doi);
      await item.saveTx();
      items.push(item);
    }
    return items;
  }

  async function triggerMenu(items, R) {
    const mw = Zotero.getMainWindow();
    const ZP = mw.ZoteroPane;
    R.menu = {};
    try {
      await Zotero.Schema.schemaUpdatePromise;
      await ZP.collectionsView.waitForLoad();
      await ZP.collectionsView.selectLibrary(Zotero.Libraries.userLibraryID);
      for (let i = 0; i < 50 && !((ZP.getCollectionTreeRows() || []).length); i++) await sleep(200);
      R.menu.treeRows = (ZP.getCollectionTreeRows() || []).length;
      for (let i = 0; i < 25 && !(ZP.itemsView && ZP.itemsView.collectionTreeRows && ZP.itemsView.collectionTreeRows.length); i++) await sleep(200);
      if (!(ZP.itemsView.collectionTreeRows && ZP.itemsView.collectionTreeRows.length)) {
        R.menu.forcedChangeRows = true;
        await ZP.itemsView.changeCollectionTreeRows(ZP.getCollectionTreeRows());
      }
      await ZP.itemsView.waitForLoad();
      await ZP.itemsView.selectItems(items.map((i) => i.id));
      R.menu.selected = ZP.getSelectedItems().map((i) => i.id);
    } catch (e) {
      R.menu.selectError = errText(e) + ' | ' + String(e && e.stack).split('\n').slice(0, 6).join(' | ');
      R.menu.itemsViewState = { hasView: !!ZP.itemsView, rows: ZP.itemsView && ZP.itemsView.collectionTreeRows && ZP.itemsView.collectionTreeRows.length };
    }
    const menu = mw.document.getElementById('zotero-itemmenu');
    R.menu.found = !!menu;
    if (!menu) return false;
    // Build the menu exactly as a right-click does.
    // ZoteroPane.onItemsContextMenuOpen = await buildItemContextMenu(); openPopup(menu). buildItemContextMenu is the part that matters.
    try { await ZP.buildItemContextMenu(); R.menu.builtContextMenu = true; } catch (e) { R.menu.buildError = errText(e); }
    await sleep(500);
    let mi = menu.querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
    R.menu.itemFound = !!mi;
    R.menu.children = Array.from(menu.children).map((c) => (c.localName + '#' + c.id + '|' + (c.getAttribute('data-l10n-id') || c.getAttribute('label') || '') + (c.hidden ? '|hidden' : ''))).slice(-14);
    R.menu.fulltextAnywhere = Array.from(mw.document.querySelectorAll('[data-l10n-id*="fulltext"]')).map((c) => c.localName + '#' + c.id + '|' + c.getAttribute('data-l10n-id') + '|parent=' + (c.parentNode && c.parentNode.id));
    if (mi) R.menu.itemHidden = !!mi.hidden;
    if (!mi || mi.hidden) {
      try { menu.openPopupAtScreen(100, 100, true); R.menu.openedPopup = true; await sleep(800); } catch (e) { R.menu.openPopupError = errText(e); }
      mi = menu.querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
      R.menu.itemFoundAfterOpen = !!mi;
      if (mi) R.menu.itemHiddenAfterOpen = !!mi.hidden;
    }
    if (!mi) return false;
    R.menu.label = mi.getAttribute('label') || mi.label || '';
    mi.doCommand();
    R.menu.commanded = true;
    return true;
  }

  async function main(R) {
    const ZFT = await (async () => {
      const end = Date.now() + ZFT_WAIT_MS;
      for (;;) {
        const Z = Zotero.ZoteroFullText;
        if (Z && Z.plugin && Z.plugin.started && Z.runs) return Z;
        if (Date.now() >= end) return null;
        await sleep(200);
      }
    })();
    if (!ZFT) { R.fatal = 'zotero-fulltext never became ready'; return; }
    R.zft = { version: (ZFT.info && ZFT.info.version) || '' };
    // The main window must be up before the menu exists.
    for (let i = 0; i < 150 && !(Zotero.getMainWindow() && Zotero.getMainWindow().ZoteroPane && Zotero.getMainWindow().document.getElementById('zotero-itemmenu')); i++) await sleep(200);
    installWrappers();
    // Optional experiment (throwaway profile only): JS source of an async function body, run before the menu path. It gets (Zotero, Services, S, sleep, plog, R).
    const experimentFile = prefStr(PREF + 'experiment', '');
    if (experimentFile) {
      try {
        const experiment = await IOUtils.readUTF8(experimentFile);
        const AsyncFunction = Object.getPrototypeOf(async function () { /* */ }).constructor;
        R.experiment = await new AsyncFunction('Zotero', 'Services', 'S', 'sleep', 'plog', 'R', 'tick', experiment)(Zotero, Services, S, sleep, plog, R, tick);
      } catch (e) { R.experimentError = errText(e) + ' | ' + String(e && e.stack).split('\n').slice(0, 4).join(' | '); }
    }
    if (prefStr(PREF + 'skipMenu', '') === 'true') { R.skippedMenu = true; for (let i = 0; i < 20; i++) { await sleep(300); await tick(); } return; }
    let dois = DEFAULT_DOIS;
    try {
      let raw = prefStr(PREF + 'dois', '');
      if (raw && raw.trim()[0] !== '[') raw = await IOUtils.readUTF8(raw); // a path to a JSON file
      const s = JSON.parse(raw);
      if (Array.isArray(s) && s.length) dois = s;
    } catch (e) { /* default */ }
    R.prefsSeen = {
      instEnabled: (() => { try { return ZFT.config.getPref('inst.enabled'); } catch (e) { return '(throws)'; } })(),
      autoOpenLogin: (() => { try { return ZFT.config.getPref('inst.auto_open_login'); } catch (e) { return '(throws)'; } })(),
      challengeWindows: (() => { try { return ZFT.config.getPref('inst.challenge_windows'); } catch (e) { return '(throws)'; } })(),
      institution: (() => { try { return ZFT.config.institutionName(); } catch (e) { return '(throws)'; } })(),
    };
    const items = await createItems(dois);
    R.items = items.map((i) => ({ id: i.id, doi: i.getField('DOI') }));
    const ok = await triggerMenu(items, R);
    plog('menu command fired: ' + ok);
    if (!ok) { R.fatal = 'could not fire the Get Full Text menu item: ' + JSON.stringify(R.menu); return; }
    const runSec = prefInt(PREF + 'runSec', 240);
    const t0 = Date.now();
    let lastWinCount = 0;
    let quietSince = Date.now();
    for (;;) {
      await sleep(300);
      await tick();
      const n = S.windows.size;
      if (n !== lastWinCount) { lastWinCount = n; quietSince = Date.now(); }
      // Sign-in windows are sampled, then closed (= the user pressing Skip); this never types or submits anything.
      for (const w of allWindows()) {
        try {
          const t = w.document.documentElement.getAttribute('windowtype');
          if (t === 'zotero-fulltext:login') {
            const rec = S.windows.get(w.__zftWprobeId);
            if (rec && rec.shots.length >= 2 && !rec.closedByProbe && Date.now() - S.t0 - rec.firstSeenMs > 9000) { w.close(); rec.closedByProbe = true; }
          }
        } catch (e) { /* ignore */ }
      }
      let running = false;
      try { running = !!ZFT.runs.activeRuns && ZFT.runs.activeRuns().length > 0; } catch (e) { running = undefined; }
      if ((Date.now() - t0) / 1000 > runSec) { R.timedOut = true; break; }
      if (running === false && Date.now() - quietSince > 8000 && Date.now() - t0 > 20000) break;
    }
    // settle
    const settle = prefInt(PREF + 'settleSec', 8);
    for (let i = 0; i < settle * 3; i++) { await sleep(333); await tick(); }
  }

  async function finish(R) {
    R.meta.elapsedMs = Date.now() - S.t0;
    R.meta.zotero = Zotero.version;
    R.windows = Array.from(S.windows.values());
    const path = prefStr(PREF + 'resultFile', '') || prefStr('extensions.zotero-harness.resultFile', '') || (Services.env.exists('ZOTERO_HARNESS_RESULT_FILE') ? Services.env.get('ZOTERO_HARNESS_RESULT_FILE') : '');
    // Close everything we did not create, then write.
    try { for (const run of Zotero.ZoteroFullText.runs.activeRuns()) Zotero.ZoteroFullText.runs.cancelRun(run); } catch (e) { /* ignore */ }
    try { Services.ww.unregisterNotification(observer); } catch (e) { /* ignore */ }
    try { Services.console.unregisterListener(consoleListener); } catch (e) { /* ignore */ }
    if (path) {
      try { await IOUtils.writeUTF8(path, JSON.stringify(R, null, 2), { tmpPath: path + '.tmp' }); plog('result written ' + path); }
      catch (e) { plog('write failed ' + errText(e)); }
    }
    await sleep(1000);
    for (const w of allWindows()) { try { if (w.location.href.indexOf('zoteroPane.xhtml') === -1) w.close(); } catch (e) { /* ignore */ } }
    await sleep(1000);
    try { Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit); } catch (e) { plog('quit failed ' + errText(e)); }
  }

  function start() {
    if (S) return;
    S = { t0: Date.now(), nextId: 0, windows: new Map(), lastOpenCall: null, shotDir: prefStr(PREF + 'shotDir', ''), R: { schema: 'zotero-fulltext/window-probe/v1', meta: {}, driverLog: [], calls: [], consoleErrors: [], windows: [] } };
    const R = S.R;
    try { Services.prefs.setStringPref('extensions.zotero-harness.resultFile', ''); } catch (e) { /* ignore */ }
    try { Services.ww.registerNotification(observer); } catch (e) { plog('registerNotification failed ' + errText(e)); }
    try { Services.console.registerListener(consoleListener); } catch (e) { plog('console listener failed ' + errText(e)); }
    (async () => {
      try { await main(R); } catch (e) { R.fatal = errText(e) + ' | ' + String(e && e.stack).split('\n').slice(0, 4).join(' | '); }
      await finish(R);
    })();
  }
  function stop() { S = null; }
  return { start, stop };
})();

function install() {}
function uninstall() {}
function startup() { WProbe.start(); }
function shutdown() { WProbe.stop(); }
function onMainWindowLoad() {}
function onMainWindowUnload() {}
