/* zotero-fulltext login probe (bootstrap.js). Runs ONLY in the throwaway profile that tools/zotero-harness/login-probe.ps1 creates.
 *
 * Opens the REAL sign-in window of zotero-fulltext (ZFT.login.open, the same call the engine's LoginWindow effect makes) for a list of
 * scenarios pointing at PUBLIC pages, waits, then records what the window actually shows: browser.currentURI, remoteness, the
 * origin/TLS bar, the banner hint, the page title, focus state, the "[zotero-fulltext] login" debug lines, and a PNG of the window.
 * It never types into a page and never signs in. Scenarios (kind = 'challenge' | 'login') come from the pref
 * extensions.zotero-probe.scenarios (JSON array of {name, kind, url, waitSec?}); a default set is built in.
 *
 * Prefs (set by login-probe.ps1): extensions.zotero-probe.{resultFile, scenarios, waitSec, shotDir, expectFocus}.
 */
/* global Zotero, Services, Ci, Components, IOUtils, PathUtils, ChromeUtils */

var Probe = (function () {
  'use strict';

  const PREF = 'extensions.zotero-probe.';
  const WINDOW_TYPE = 'zotero-fulltext:login';
  const ZFT_WAIT_MS = 90000;
  const DEFAULT_SCENARIOS = [
    { name: 'challenge-example-org', kind: 'challenge', url: 'https://www.example.org/' },
    { name: 'login-example-org', kind: 'login', url: 'https://www.example.org/' },
    { name: 'challenge-europepmc-cf', kind: 'challenge', url: 'https://europepmc.org/articles/PMC2707599?pdf=render' },
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
    try { Zotero.debug('[zotero-fulltext-login-probe] ' + msg); } catch (e) { /* ignore */ }
  }
  async function pollFor(fn, ms, step) {
    const end = Date.now() + ms;
    for (;;) {
      let v = null;
      try { v = await fn(); } catch (e) { v = null; }
      if (v) return v;
      if (Date.now() >= end) return null;
      await sleep(step || 100);
    }
  }

  function loginWindows() {
    const out = [];
    const en = Services.wm.getEnumerator(WINDOW_TYPE);
    while (en.hasMoreElements()) out.push(en.getNext());
    return out;
  }

  function text(win, id) {
    try { return String(win.document.getElementById(id).textContent || ''); } catch (e) { return null; }
  }

  async function shot(win, path) {
    try {
      await IOUtils.makeDirectory(PathUtils.parent(path), { createAncestors: true, ignoreExisting: true });
      const host = Zotero.getMainWindow();
      const doc = host.document;
      const canvas = doc.createElementNS('http://www.w3.org/1999/xhtml', 'canvas');
      const w = win.innerWidth;
      const h = win.innerHeight;
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext('2d');
      ctx.drawWindow(win, 0, 0, w, h, 'rgb(255,255,255)', ctx.DRAWWINDOW_DRAW_VIEW | ctx.DRAWWINDOW_DRAW_CARET);
      // Remote content is not part of drawWindow(): add the page of every <browser> through its window global.
      const notes = [];
      for (const b of win.document.querySelectorAll('browser')) {
        try {
          const r = b.getBoundingClientRect();
          if (!r.width || !r.height) continue;
          const wg = b.browsingContext && b.browsingContext.currentWindowGlobal;
          if (!wg || typeof wg.drawSnapshot !== 'function') continue;
          const rect = new win.DOMRect(0, 0, r.width, r.height);
          const bmp = await wg.drawSnapshot(rect, 1, 'rgb(255,255,255)', false);
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

  async function snapshotWindow(win, name) {
    const info = { windowCount: loginWindows().length };
    try {
      const doc = win.document;
      info.docTitle = doc.title;
      info.windowTitle = win.document.title;
      const browser = doc.getElementById('zft-login-browser');
      info.browserFound = !!browser;
      if (browser) {
        let spec = '';
        try { spec = browser.currentURI ? browser.currentURI.spec : '(no currentURI)'; } catch (e) { spec = '(throws ' + errText(e) + ')'; }
        info.currentURI = spec;
        info.isRemoteBrowser = !!browser.isRemoteBrowser;
        info.remoteType = browser.remoteType === undefined ? '(undefined)' : String(browser.remoteType);
        info.remoteAttr = browser.getAttribute('remote');
        info.maychangeremoteness = browser.getAttribute('maychangeremoteness');
        info.browserRect = (() => { const r = browser.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; })();
        try { info.customUserAgent = String(browser.browsingContext.customUserAgent || ''); } catch (e) { info.customUserAgent = '(throws)'; }
        try { info.docReadyState = browser.contentDocument ? browser.contentDocument.readyState : '(null)'; } catch (e) { info.docReadyState = '(throws)'; }
        try {
          const wg = browser.browsingContext && browser.browsingContext.currentWindowGlobal;
          info.windowGlobalURI = wg ? String(wg.documentURI && wg.documentURI.spec) : '(none)';
          if (wg) {
            const title = await Promise.race([wg.getActor('PageData').sendQuery('title'), sleep(2500).then(() => '(PageData timeout)')]);
            info.pageTitle = String(title).slice(0, 120);
            const html = await Promise.race([wg.getActor('PageData').sendQuery('documentHTML'), sleep(2500).then(() => '')]);
            info.pageHtmlChars = String(html || '').length;
          }
        } catch (e) { info.pageDataError = errText(e); }
        try { info.loadingState = { busy: browser.webProgress.isLoadingDocument }; } catch (e) { /* ignore */ }
      }
      info.bar = {
        secure: text(win, 'zft-login-secure'),
        origin: text(win, 'zft-login-origin'),
        hint: text(win, 'zft-login-hint'),
        waiting: text(win, 'zft-login-waiting'),
        popups: text(win, 'zft-login-popups'),
      };
      try {
        const cs = (el) => {
          const r = el.getBoundingClientRect();
          const c = win.getComputedStyle(el);
          return { tag: el.localName, id: el.id || '', rect: [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)], display: c.display, dir: c.flexDirection, grow: c.flexGrow, basis: c.flexBasis, height: c.height, minH: c.minHeight, boxFlex: c.MozBoxFlex, boxOrient: c.MozBoxOrient, hidden: el.hidden };
        };
        const root = doc.documentElement;
        info.layout = [cs(root)].concat(Array.from(root.children).map(cs));
        const vb = doc.querySelector('vbox');
        if (vb) info.layout = info.layout.concat(Array.from(vb.children).map(cs));
      } catch (e) { info.layoutError = errText(e); }
      try {
        const tip = doc.getElementById('zft-login-tip');
        info.tipHidden = tip ? tip.hidden : null;
        info.tipText = tip ? String(tip.textContent || '').trim() : null;
        const er = doc.getElementById('zft-login-error');
        info.errorText = er && !er.hidden ? String(er.textContent || '').trim() : '';
      } catch (e) { /* ignore */ }
      info.hintRaw = (() => { try { return win.document.getElementById('zft-login-hint').getAttribute('data-l10n-id'); } catch (e) { return null; } })();
      info.bannerVisibleButtons = ['zft-login-keep', 'zft-login-handoff', 'zft-login-skip'].map((id) => {
        const b = doc.getElementById(id);
        return b ? { id, hidden: !!b.hidden, label: String(b.textContent || '').trim() } : { id, missing: true };
      });
      const sw = win.screen;
      info.rect = { screenX: win.screenX, screenY: win.screenY, outerW: win.outerWidth, outerH: win.outerHeight, screenW: sw && sw.width, screenH: sw && sw.height };
      info.windowState = win.windowState;
      let active = null;
      try { active = Services.focus.activeWindow; } catch (e) { /* ignore */ }
      info.isActiveWindow = active === win;
      info.activeWindowType = (() => { try { return active && active.document.documentElement.getAttribute('windowtype') || (active && active.location.href) || null; } catch (e) { return '(unreadable)'; } })();
      try { info.hasFocus = doc.hasFocus(); } catch (e) { info.hasFocus = null; }
      info.theme = (() => { try { return win.getComputedStyle(doc.documentElement).backgroundColor; } catch (e) { return null; } })();
    } catch (e) {
      info.error = errText(e);
    }
    if (S.shotDir) info.shot = await shot(win, PathUtils.join(S.shotDir, name + '.png'));
    return info;
  }

  function debugLines(re, sinceIndex) {
    try {
      const all = String(Zotero.Debug.get()).split('\n');
      return { total: all.length, lines: all.slice(sinceIndex || 0).filter((l) => re.test(l)).map((l) => l.slice(0, 400)) };
    } catch (e) {
      return { total: 0, lines: ['(Zotero.Debug.get failed: ' + errText(e) + ')'] };
    }
  }

  async function runScenario(ZFT, host, sc, defaultWait) {
    const rec = { name: sc.name, kind: sc.kind, url: sc.url };
    const dbgBefore = debugLines(/./, 0).total;
    const scope = new ZFT.util.CancelScope();
    const req = { kind: sc.kind, realm: 'openathens', entry_url: sc.url, item_title: 'Login probe item', waiting: 1, timeout_ms: 120000, capture_pdf: sc.capture_pdf !== false };
    let settled = null;
    let thrown = null;
    const t0 = Date.now();
    const p = ZFT.login.open({ op: 9000 + S.n, run: 1, req }, { scope, host, bodies: null })
      .then((v) => { settled = { ok: v, atMs: Date.now() - t0 }; }, (e) => { thrown = { err: errText(e), atMs: Date.now() - t0 }; });
    S.n += 1;
    // Wait for the window, sample at 2 s and at the end.
    const win0 = await pollFor(() => loginWindows()[0], 15000, 100);
    rec.windowAppearedMs = win0 ? Date.now() - t0 : null;
    if (!win0) {
      rec.noWindow = true;
    } else {
      const waitMs = (sc.waitSec || defaultWait) * 1000;
      rec.samples = [];
      let last = null;
      while (Date.now() - t0 < waitMs) {
        await sleep(1500);
        const open = loginWindows()[0];
        if (!open || open.closed) { rec.closedAtMs = Date.now() - t0; break; }
        const snap = await snapshotWindow(open, sc.name + '-' + (Date.now() - t0));
        last = snap;
        rec.samples.push({ atMs: Date.now() - t0, currentURI: snap.currentURI, remote: snap.isRemoteBrowser, pageTitle: snap.pageTitle, origin: snap.bar && snap.bar.origin, secure: snap.bar && snap.bar.secure, hint: snap.bar && snap.bar.hint, tip: snap.tipHidden === false, error: snap.errorText, active: snap.isActiveWindow, rect: snap.browserRect });
      }
      rec.final = last;
      rec.openDuringWait = !rec.closedAtMs;
    }
    rec.settledEarly = settled || thrown || null;
    scope.cancel('probe done');
    await Promise.race([p, sleep(8000)]);
    await sleep(500);
    rec.afterCancel = { settled, thrown, windowsLeft: loginWindows().length };
    rec.debug = debugLines(/\[zotero-fulltext\][^\n]*login|\[zotero-fulltext\][^\n]*pdfCapture|login:/i, dbgBefore);
    // Windows left over from this scenario are closed so the next one starts clean.
    for (const w of loginWindows()) { try { w.close(); } catch (e) { /* ignore */ } }
    await sleep(500);
    return rec;
  }

  /**
   * Full-engine scenario: one item with a DOI, institutional access ON, interactive run. Whatever sign-in / challenge window the engine
   * opens is sampled like the direct scenarios, for up to sc.waitSec seconds after the first window appears. Never types anything.
   */
  async function runEngineScenario(ZFT, sc, defaultWait) {
    const rec = { name: sc.name, kind: 'engine', doi: sc.doi, samples: [], windows: [] };
    const dbgBefore = debugLines(/./, 0).total;
    for (const [k, v] of Object.entries({ 'inst.enabled': true, 'inst.auto_open_login': true, 'inst.challenge_windows': true })) ZFT.config.setPref(k, v);
    const item = new Zotero.Item(sc.itemType || 'journalArticle');
    item.libraryID = Zotero.Libraries.userLibraryID;
    item.setField('title', sc.title || 'Login probe item');
    item.setField('DOI', sc.doi);
    await item.saveTx();
    const t0 = Date.now();
    const handle = await ZFT.runs.startRun({ items: [item], mode: 'full', interactive: true });
    let report = null;
    handle.done.then((r) => { report = r; });
    let tWin = 0;
    const limit = (sc.runSec || 90) * 1000;
    const seen = new Set();
    while (Date.now() - t0 < limit && !report) {
      await sleep(1500);
      const open = loginWindows()[0];
      if (!open || open.closed) { if (tWin) break; continue; }
      if (!tWin) tWin = Date.now();
      const snap = await snapshotWindow(open, sc.name + '-' + (Date.now() - t0));
      let banner = null;
      try {
        const pw = Services.wm.getMostRecentWindow('zotero-fulltext:progress');
        const b = pw && pw.document.getElementById('zft-progress-signin');
        banner = b ? { hidden: b.hidden, text: String(b.textContent || '').replace(/\s+/g, ' ').trim() } : { noProgressWindow: !pw };
        if (pw && !rec.progressShot && !b.hidden && S.shotDir) rec.progressShot = await shot(pw, PathUtils.join(S.shotDir, sc.name + '-progress.png'));
      } catch (e) { banner = { error: errText(e) }; }
      rec.samples.push({ atMs: Date.now() - t0, currentURI: snap.currentURI, remote: snap.isRemoteBrowser, pageTitle: snap.pageTitle, origin: snap.bar && snap.bar.origin, secure: snap.bar && snap.bar.secure, hint: snap.bar && snap.bar.hint, tip: snap.tipHidden === false, error: snap.errorText, active: snap.isActiveWindow, rect: snap.browserRect, progressBanner: banner });
      rec.final = snap;
      if (Date.now() - tWin > (sc.waitSec || defaultWait) * 1000) break;
    }
    rec.windowSeen = !!tWin;
    handle.cancel();
    await Promise.race([handle.done, sleep(10000)]);
    for (const w of loginWindows()) { try { w.close(); } catch (e) { /* ignore */ } }
    rec.report = report ? { counts: report.counts, items: (report.items || []).map((i) => i.outcome) } : null;
    rec.debug = debugLines(/\[zotero-fulltext\][^\n]*(login|pdfCapture|challenge)|login:/i, dbgBefore);
    return rec;
  }

  /**
   * keep_session check on SYNTHETIC cookies (dummy values) in the throwaway profile: an OpenAthens and an EZproxy session cookie must
   * become persistent with expiry ~ created + 12 h; a Microsoft and an unrelated session cookie must stay session cookies; with the
   * pref off nothing may change.
   */
  async function cookieTest(ZFT) {
    const out = { ok: false };
    try {
      const jar = Services.cookies;
      const names = [
        ['zft-probe.openathens.net', 'oa'], ['login.openathens.net', 'oa2'], ['www.sciencedirect-com.login.kcl.idm.oclc.org', 'ez'],
        ['login.microsoftonline.com', 'ms'], ['www.example.org', 'other'],
      ];
      const add = (host, name) => jar.add(host, '/', 'zft_probe_' + name, 'dummy', true, true, true, Math.floor(Date.now() / 1000) + 3600, {}, Ci.nsICookie.SAMESITE_NONE, Ci.nsICookie.SCHEME_HTTPS);
      const find = (host, name) => Array.from(jar.cookies).find((c) => c.host.replace(/^\./, '') === host && c.name === 'zft_probe_' + name);
      const describe = () => Object.fromEntries(names.map(([h, n]) => { const c = find(h, n); return [n, c ? { isSession: c.isSession, expiryInHours: c.isSession ? null : +(((c.expiry * 1000) - Date.now()) / 3600000).toFixed(2) } : null]; }));
      for (const [h, n] of names) add(h, n);
      out.before = describe();
      Services.prefs.setBoolPref('extensions.zotero-fulltext.inst.keep_session', false);
      out.offCount = ZFT.login.persistSession();
      out.afterOff = describe();
      Services.prefs.clearUserPref('extensions.zotero-fulltext.inst.keep_session');
      out.onCount = ZFT.login.persistSession();
      out.afterOn = describe();
      out.secondCount = ZFT.login.persistSession();
      for (const [h, n] of names) { try { jar.remove(h, 'zft_probe_' + n, '/', {}); } catch (e) { /* ignore */ } }
      out.cleaned = names.every(([h, n]) => !find(h, n));
      out.ok = true;
    } catch (e) { out.error = errText(e); }
    return out;
  }

  /** A HiddenBrowser load through ZFT.browser.load (the engine's first, silent attempt), to see what an unreachable host looks like. */
  async function runHiddenScenario(ZFT, host, sc) {
    const rec = { name: sc.name, kind: 'hidden', url: sc.url };
    const scope = new ZFT.util.CancelScope();
    try {
      const out = await ZFT.browser.load({ url: sc.url, ua: 'plain', capture_pdf: true, want_page: true, settle_ms: 1500, timeout_ms: 20000, hold_while: [] }, { scope, host });
      rec.result = { final_url: out.final_url, status: out.status, ms: out.ms, trail: out.url_trail, htmlChars: out.page_html ? out.page_html.length : null, htmlHead: out.page_html ? out.page_html.slice(0, 160) : null };
    } catch (e) {
      rec.error = { kind: e && e.kind, message: errText(e) };
    }
    return rec;
  }

  async function main(R) {
    const ZFT = await pollFor(() => {
      const Z = Zotero.ZoteroFullText;
      return Z && Z.plugin && Z.plugin.started && Z.login && Z.wasmHost ? Z : null;
    }, ZFT_WAIT_MS, 200);
    if (!ZFT) { R.fatal = 'zotero-fulltext never became ready'; return; }
    R.zft = { version: (ZFT.info && ZFT.info.version) || '', keepSessionPrefDefault: (() => { try { return ZFT.config.getPref('inst.keep_session'); } catch (e) { return '(no config pref)'; } })() };
    const host = await ZFT.wasmHost.get();
    plog('zotero-fulltext ready, wasm host ' + (host ? 'ok' : 'MISSING'));
    let scenarios = DEFAULT_SCENARIOS;
    try { const s = JSON.parse(prefStr(PREF + 'scenarios', '')); if (Array.isArray(s) && s.length) scenarios = s; } catch (e) { /* default */ }
    const defaultWait = prefInt(PREF + 'waitSec', 10);
    if (prefStr(PREF + 'cookieTest', '') === 'true') R.cookieTest = await cookieTest(ZFT);
    for (const sc of scenarios) {
      plog('scenario ' + sc.name);
      try { R.scenarios.push(sc.doi ? await runEngineScenario(ZFT, sc, defaultWait) : (sc.kind === 'hidden' ? await runHiddenScenario(ZFT, host, sc) : await runScenario(ZFT, host, sc, defaultWait))); }
      catch (e) { R.scenarios.push({ name: sc.name, driverError: errText(e) + ' | ' + String(e && e.stack).split('\n').slice(0, 3).join(' | ') }); }
    }
    if (typeof sleep === 'function' && prefInt(PREF + 'extraSec', 0) > 0) await sleep(prefInt(PREF + 'extraSec', 0) * 1000);
  }

  async function finish(R) {
    R.meta.elapsedMs = Date.now() - S.t0;
    R.meta.zotero = Zotero.version;
    const path = prefStr(PREF + 'resultFile', '') || prefStr('extensions.zotero-harness.resultFile', '') || (Services.env.exists('ZOTERO_HARNESS_RESULT_FILE') ? Services.env.get('ZOTERO_HARNESS_RESULT_FILE') : '');
    if (path) {
      try { await IOUtils.writeUTF8(path, JSON.stringify(R, null, 2), { tmpPath: path + '.tmp' }); plog('result written ' + path); }
      catch (e) { plog('write failed ' + errText(e)); }
    }
    await sleep(1500);
    try { Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit); } catch (e) { plog('quit failed ' + errText(e)); }
  }

  function start() {
    if (S) return;
    S = { t0: Date.now(), n: 0, shotDir: prefStr(PREF + 'shotDir', ''), R: { schema: 'zotero-fulltext/login-probe/v1', meta: {}, driverLog: [], scenarios: [] } };
    const R = S.R;
    // The plugin's own smoke harness must not fire (it would write the same result file and quit).
    try { Services.prefs.setStringPref('extensions.zotero-harness.resultFile', ''); } catch (e) { /* ignore */ }
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
function startup() { Probe.start(); }
function shutdown() { Probe.stop(); }
function onMainWindowLoad() {}
function onMainWindowUnload() {}
