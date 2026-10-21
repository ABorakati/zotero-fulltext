/* ZFT.harness: self-check hook for tools/zotero-harness/run.ps1 (throwaway Zotero profile ONLY).
 *
 * Owner: js-shell. Dormant unless the pref extensions.zotero-harness.resultFile is a non-empty string, which only
 * run.ps1 sets (in the throwaway profile's user.js), so a normal user session never runs this. The environment
 * variable ZOTERO_HARNESS_RESULT_FILE that the harness also sets is deliberately NOT consulted: a stray variable in a
 * real session must never make the plugin quit Zotero. Contract with the harness (tools/zotero-harness/README.md):
 * write the result JSON with IOUtils.writeUTF8(path, text, {tmpPath: path + '.tmp'}) and then quit with
 * Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit) unless extensions.zotero-harness.quitWhenDone is false.
 *
 * maybeRun() performs the smoke checks and writes:
 *   {plugin: 'zotero-fulltext', version, ok: boolean, checks: {name: {ok, detail}}, startedMs, durationMs}
 * Checks (ZFT.harness.CHECKS, each individually try/caught, so one failure still writes a result):
 *   modules   every module of MODULES exposes its declared names
 *   wasm      ZFT.wasmHost.get(): abi 1, pure inspect of a tiny HTML page returns an Inspection; a PRIVATE host
 *             (not the one runs use, so the engine state of a real run is never touched) accepts a Configure
 *             command built from the default EngineConfig
 *   prefs     every ZFT.config.DEFAULTS key is registered (prefs.js) with the declared type
 *   menu      ZFT.menu registered its menus (keys returned by registerMenu)
 *   menu_ui   a real main window's item and Tools popups, built by Zotero.MenuManager for an unsaved regular item, show our
 *             two + three entries with translated labels (the FTL link reached the main window)
 *   prefpane  ZFT.plugin.paneID was returned by PreferencePanes.register
 *   prefpane_ui  the real preferences window shows the pane: a control per pref, translated labels, no problems for the
 *             stock prefs, window.ZoteroFullTextPrefs published, and one edit (run.item_concurrency = 99) is clamped to 8
 *             through the pane's own change handler (the pref is restored afterwards); the method selector and "Uses:" line exist,
 *             choosing the Imperial preset writes its Shibboleth identity provider, and a pasted library proxy address is parsed
 *             into the proxy prefs (every institution pref is restored afterwards)
 *   ftl       ZFT.l10n.format('zotero-fulltext-prefs-title') is not the bare id. (The menu messages carry only a
 *             .label attribute and no value, so formatValueSync cannot resolve them; a value message is the right probe.)
 *   windows   progress.xhtml and login.xhtml open and close; console errors from zotero-fulltext sources fail the check
 * It never contacts the network and never logs in. The windows check passes no controller state, so the window
 * scripts must cope with that (a window opened without arguments must not throw).
 *
 * ZFT.harness.endpointMap(): returns the harness-only EngineConfig.endpoint_map parsed from the pref
 * extensions.zotero-fulltext.harness.endpoint_map (JSON [[real_origin, fake_origin], ...]) or []. Anything that is
 * not a JSON array of 2-string arrays yields [] (a half-applied map could send some requests to the real network).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const RESULT_PREF = 'extensions.zotero-harness.resultFile';
  const QUIT_PREF = 'extensions.zotero-harness.quitWhenDone';
  const ENDPOINT_PREF = 'extensions.zotero-fulltext.harness.endpoint_map';
  /** Waits in ms, read at call time (tests shorten them). */
  const TIMEOUTS = { uiReady: 30000, windowLoad: 10000, pane: 20000, menu: 10000, edit: 5000 };

  /**
   * The declared public surface per module (mirrors tests/js/helpers/api-manifest.mjs). Checked by names only, so a
   * module that gained extra names still passes; a missing or renamed name fails the `modules` check.
   */
  const MODULES = {
    util: ['OP_ERR_KINDS', 'OpError', 'CancelScope', 'Deferred', 'TaskSet', 'sleep', 'idle', 'monoNow', 'wallNow', 'log', 'errText', 'toU8', 'concatBytes', 'parseJson'],
    l10n: ['format', 'reset'],
    config: ['DEFAULTS', 'PANE_LAYOUT', 'PLUGIN_ONLY', 'KCL_EZPROXY_HOSTS', 'LIMITS', 'typeOf', 'getPref', 'setPref', 'parseList', 'buildEngineConfig', 'validate', 'problems'],
    state: ['FILE_NAME', 'VERSION', 'FLUSH_DEBOUNCE_MS', 'PERSIST_KEYS', 'init', 'getPersisted', 'set', 'flush', 'reset', 'registerTmpDir', 'unregisterTmpDir', 'sweepTmp', 'nextRunId', 'dataDir', 'tmpRoot', 'dispose'],
    bodies: ['INSPECT_TEXT_MAX', 'PDF_HEAD', 'PDF_TAIL', 'PAGE_MAX_BYTES', 'MEM_CAP_TOTAL', 'BodyStore', 'store'],
    wasmHost: ['ABI_VERSION', 'WASM_FILE', 'INSPECT_WATCHDOG_MS', 'REQUIRED_EXPORTS', 'EngineTrap', 'EngineError', 'WasmInstance', 'Host', 'load', 'get', 'shutdown'],
    net: ['RESPONSE_HEADERS', 'DROPPED_REQUEST_HEADERS', 'TIMEOUT_BACKSTOP_MS', 'request', 'buildHeaders', 'classifyError'],
    pdfCapture: ['capture', 'activeCount', 'disposeAll'],
    browser: ['MAX_BROWSERS', 'POLL_MS', 'URL_TRAIL_MAX', 'PAGE_DATA_TIMEOUT_MS', 'load', 'trailEntry', 'stats'],
    translators: ['TRANSLATE_TIMEOUT_MS', 'MAX_CONCURRENT', 'run', 'parsePage'],
    zoteroOa: ['LOOKUP_TIMEOUT_MS', 'lookup'],
    items: ['snapshot', 'snapshotAll', 'menuEligible', 'getItem', 'watchDeletions'],
    save: ['savePdf', 'attachFile', 'titleForVersion', 'isStorableUrl'],
    confirm: ['ask'],
    progress: ['WINDOW_URL', 'WINDOW_TYPE', 'ROW_STATES', 'ProgressModel', 'open', 'update', 'finish', 'show', 'modelFor', 'handleAction', 'redactUrl', 'closeAll'],
    login: ['WINDOW_URL', 'WINDOW_TYPE', 'POLL_MS', 'KEEP_WAITING_AT', 'decide', 'open', 'update', 'warmUp', 'closeAll'],
    effects: ['COMPLETION_BATCH_MS', 'MAX_STEPS_PER_PUMP', 'STALL_MS', 'TRACE_RING', 'EFFECT_TYPES', 'Runner', 'createRunner', 'syntheticReport'],
    runs: ['SHUTDOWN_WAIT_MS', 'MODES', 'USER_ACTIONS', 'init', 'startRun', 'cancelRun', 'cancelItem', 'userAction', 'reconfigure', 'activeRuns', 'report', 'runner', 'shutdown'],
    menu: ['MENU_IDS', 'TARGETS', 'register', 'unregister', 'registeredKeys', 'commandFor', 'onShowing'],
    harness: ['RESULT_PREF', 'QUIT_PREF', 'MODULES', 'CHECKS', 'TIMEOUTS', 'maybeRun', 'endpointMap'],
    plugin: ['PRELOAD_DELAY_MS', 'REASON', 'started', 'paneID', 'tasks', 'startup', 'shutdown', 'addToWindow', 'removeFromWindow', 'onMainWindowLoad', 'onMainWindowUnload'],
  };

  function readPref(name) {
    try { return Zotero.Prefs.get(name, true); } catch (e) { return undefined; }
  }

  /** @returns {string} the result file path, '' when the harness is not driving this session */
  function resultPath() {
    const v = readPref(RESULT_PREF);
    return typeof v === 'string' ? v.trim() : '';
  }

  /** @returns {Array<[string, string]>} see the header */
  function endpointMap() {
    const raw = readPref(ENDPOINT_PREF);
    if (typeof raw !== 'string' || !raw.trim()) return [];
    const parsed = ZFT.util.parseJson(raw, null);
    if (!Array.isArray(parsed)) return [];
    const out = [];
    for (const entry of parsed) {
      const ok = Array.isArray(entry) && entry.length === 2 && typeof entry[0] === 'string' && typeof entry[1] === 'string' && entry[0] && entry[1];
      if (!ok) return [];
      out.push([entry[0], entry[1]]);
    }
    return out;
  }

  function withTimeout(promise, ms) {
    return new Promise((resolve) => {
      const id = setTimeout(() => resolve(false), ms);
      Promise.resolve(promise).then(() => { clearTimeout(id); resolve(true); }, () => { clearTimeout(id); resolve(false); });
    });
  }

  // --- checks (each returns a detail value or throws) ---------------------------------------------------------------

  async function checkModules() {
    const missing = [];
    for (const [mod, names] of Object.entries(MODULES)) {
      const m = ZFT[mod];
      if (!m) { missing.push(mod); continue; }
      for (const name of names) if (!(name in m)) missing.push(mod + '.' + name);
    }
    if (missing.length) throw new Error('missing: ' + missing.join(', '));
    return { modules: Object.keys(MODULES).length };
  }

  async function checkWasm() {
    const host = await ZFT.wasmHost.get();
    const diag = host.diagnostics();
    if (diag.abi !== ZFT.wasmHost.ABI_VERSION) throw new Error('abi ' + diag.abi + ' != ' + ZFT.wasmHost.ABI_VERSION);
    const bytes = new TextEncoder().encode('<!doctype html><html><head><title>t</title></head><body><p>hello</p></body></html>');
    const inspection = await host.inspect({
      spec: { expect: 'page', requested: {}, route: 'oa' },
      resp: { status: 200, final_url: 'https://example.org/', headers: { 'content-type': 'text/html; charset=utf-8' }, body_len: bytes.length },
    }, bytes);
    if (!inspection || typeof inspection.expect !== 'string') throw new Error('inspect returned no Inspection');
    // A private host: Configure on the shared one would reset what a real run configured.
    const probe = await ZFT.wasmHost.load({ inspectInWorker: false });
    let effects;
    try {
      const out = probe.step({
        now: ZFT.util.monoNow(), wall: ZFT.util.wallNow(), completions: [],
        commands: [{ type: 'configure', protocol: ZFT.PROTOCOL, config: ZFT.config.buildEngineConfig(), persisted: {}, seed: 1 }],
      });
      if (!out || !Array.isArray(out.effects)) throw new Error('step returned no StepOut');
      effects = out.effects.length;
    } finally {
      try { await probe.dispose(); } catch (e) { ZFT.util.log('warn', 'harness probe host dispose failed: ' + ZFT.util.errText(e)); }
    }
    return { abi: diag.abi, inspect: inspection.expect, configure_effects: effects, pure: diag.pure };
  }

  async function checkPrefs() {
    const bad = [];
    for (const key of Object.keys(ZFT.config.DEFAULTS)) {
      const v = Zotero.Prefs.get(ZFT.PREF_BRANCH + key, true);
      const expected = ZFT.config.typeOf(key);
      const actual = typeof v === 'boolean' ? 'bool' : typeof v === 'number' ? 'int' : typeof v === 'string' ? 'string' : 'missing';
      if (actual !== expected) bad.push(`${key} (${actual}, want ${expected})`);
    }
    if (bad.length) throw new Error('prefs not registered: ' + bad.join(', '));
    return { prefs: Object.keys(ZFT.config.DEFAULTS).length };
  }

  async function checkMenu() {
    const keys = typeof ZFT.menu.registeredKeys === 'function' ? ZFT.menu.registeredKeys() : [];
    if (!Array.isArray(keys) || keys.length < 2 || !keys.every((k) => typeof k === 'string' && k)) {
      throw new Error('menus not registered: ' + JSON.stringify(keys));
    }
    return { keys: Array.from(keys) };
  }

  /**
   * The menus in a real main window: feed the registered item and Tools menus through Zotero's own popup builder (an unsaved
   * regular item is the selection) and read the labels Fluent produced. Proves the FTL link was inserted into the main window
   * (plugin.addToWindow) and that the entries show for an eligible selection.
   */
  async function checkMenuUi() {
    const win = Zotero.getMainWindow();
    if (!win) throw new Error('no main window');
    const doc = win.document;
    const itemPopup = doc.getElementById('zotero-itemmenu');
    const toolsPopup = doc.querySelector('#menu_ToolsPopup');
    if (!itemPopup || !toolsPopup) throw new Error('main window popups not found');
    const item = new Zotero.Item('journalArticle');
    const common = { tabType: 'library', tabSubType: undefined, tabID: 'zotero-pane' };
    const ours = (popup) => Array.from(popup.querySelectorAll('menuitem.zotero-custom-menu-item'))
      .filter((e) => /^zotero-fulltext-menu-/.test(e.getAttribute('data-l10n-id') || ''));
    try {
      Zotero.MenuManager.updateMenuPopup(itemPopup, 'main/library/item', {
        event: new win.Event('popupshowing'),
        getContext: () => ({ ...common, items: [item], collectionTreeRows: [] }),
      });
      Zotero.MenuManager.updateMenuPopup(toolsPopup, 'main/menubar/tools', {
        event: new win.Event('popupshowing'), ...common,
        getContext: () => ({ ...common, items: [] }),
      });
      // A short popup (or a small screen, as in the headless harness) makes Zotero fold plugin entries into its "more" submenu,
      // which only fills when it opens. Open it so the entries exist either way.
      let grouped = false;
      if (!ours(itemPopup).length) {
        const sub = itemPopup.querySelector('menu.zotero-custom-menu-item > menupopup');
        if (sub) {
          grouped = true;
          sub.dispatchEvent(new win.Event('popupshowing'));
        }
      }
      const translated = (e) => {
        const label = e.getAttribute('label') || '';
        return label && label.indexOf('zotero-fulltext-') !== 0;
      };
      const ready = await pollFor(() => {
        const all = ours(itemPopup).concat(ours(toolsPopup));
        return all.length >= 5 && all.every(translated) ? all : null;
      }, TIMEOUTS.menu);
      if (!ready) {
        throw new Error(`menu entries missing or untranslated: item ${ours(itemPopup).length}, tools ${ours(toolsPopup).length}`);
      }
      const items = ours(itemPopup);
      const tools = ours(toolsPopup);
      if (items.length !== 2 || tools.length !== 3) throw new Error(`expected 2 + 3 entries, found ${items.length} + ${tools.length}`);
      const hidden = items.filter((e) => e.hidden).length;
      if (hidden) throw new Error(`${hidden} item entries are hidden for an eligible selection`);
      return { item: items.map((e) => e.getAttribute('label')), tools: tools.map((e) => e.getAttribute('label')), grouped };
    } finally {
      for (const popup of [itemPopup, toolsPopup]) {
        try { popup.querySelectorAll('.zotero-custom-menu-item').forEach((e) => e.remove()); } catch (e) { /* the popup is going away */ }
      }
    }
  }

  async function checkPrefpane() {
    const id = ZFT.plugin.paneID;
    if (!id || typeof id !== 'string') throw new Error('no preference pane id');
    return { pane: id };
  }

  /** Poll `fn` until it returns something truthy (returned) or `ms` elapsed (null). */
  async function pollFor(fn, ms) {
    const deadline = ZFT.util.monoNow() + ms;
    for (;;) {
      let v = null;
      try { v = fn(); } catch (e) { v = null; }
      if (v) return v;
      if (ZFT.util.monoNow() >= deadline) return null;
      await ZFT.util.sleep(100);
    }
  }

  /**
   * The real preferences window: open the plugin's pane, wait for prefs-pane.js to build the controls, and drive one edit
   * through the pane's own handler. This is the only place the sandbox scoping of the pane script (window.ZoteroFullTextPrefs
   * for the inline onload) and the Fluent link in the fragment are exercised for real.
   */
  async function checkPrefpaneUi() {
    const paneID = ZFT.plugin.paneID;
    if (!paneID) throw new Error('no preference pane id');
    const win = Zotero.Utilities.Internal.openPreferences(paneID);
    if (!win) throw new Error('the preferences window did not open');
    const key = 'run.item_concurrency';
    const before = ZFT.config.getPref(key);
    const accessKeys = ZFT.config.INSTITUTION_KEYS.concat(['institution.preset', 'inst.enabled', 'inst.use_openathens', 'inst.use_saml', 'inst.use_ezproxy']);
    const savedAccess = {};
    for (const k of accessKeys) savedAccess[k] = ZFT.config.getPref(k);
    try {
      const root = await pollFor(() => {
        const r = win.document.getElementById('zft-prefs-root');
        const label = r && r.querySelector('label[for="zft-pref-oa-openalex"]');
        // children built AND the first label translated (Fluent runs after the build)
        return r && r.childElementCount > 0 && label && String(label.textContent || '').trim() ? r : null;
      }, TIMEOUTS.pane);
      if (!root) throw new Error('the preference pane did not build and translate its controls');
      const wanted = Object.keys(ZFT.config.DEFAULTS).length;
      const found = root.querySelectorAll('[id^="zft-pref-"]').length;
      if (found !== wanted) throw new Error(`${found} controls for ${wanted} prefs`);
      if (!win.ZoteroFullTextPrefs) throw new Error('window.ZoteroFullTextPrefs is not published');
      const problems = win.document.getElementById('zft-prefs-problems');
      if (problems && !problems.hidden) throw new Error('the pane lists problems for the stock prefs');
      const label = root.querySelector('label[for="zft-pref-oa-openalex"]');
      if (String(label.textContent).trim().indexOf('zotero-fulltext-') === 0) throw new Error('labels show message ids');

      const control = win.document.getElementById('zft-pref-run-item-concurrency');
      control.value = '99';
      control.dispatchEvent(new win.Event('change'));
      const clamped = await pollFor(() => ZFT.config.getPref(key) === 8, TIMEOUTS.edit);
      if (!clamped) throw new Error(`an edit did not reach the pref (value ${ZFT.config.getPref(key)})`);
      if (control.value !== '8') throw new Error('the control did not show the clamped value ' + control.value);

      // Access methods: the method selector and the "Uses:" line exist, and choosing a preset / typing a proxy address writes prefs
      // through the pane's own handlers (all of it is restored below).
      const doc = win.document;
      const methodSelect = doc.getElementById('zft-institution-method');
      const options = methodSelect ? Array.from(methodSelect.querySelectorAll('option')).map((o) => o.getAttribute('value')).join(',') : '';
      if (options !== 'openathens,saml,ezproxy') throw new Error('the method selector is missing or wrong: ' + options);
      const translated = await pollFor(() => Array.from(methodSelect.querySelectorAll('option')).every((o) => {
        const text = String(o.textContent || '').trim();
        return text && text.indexOf('zotero-fulltext-') !== 0;
      }), TIMEOUTS.edit);
      if (!translated) throw new Error('the method selector shows message ids instead of names');
      const uses = doc.getElementById('zft-institution-methods');
      if (!uses || uses.hidden || !uses.childElementCount) throw new Error('the "Uses:" line is missing or empty');
      const picker = doc.getElementById('zft-pref-institution-preset');
      picker.value = 'imperial';
      picker.dispatchEvent(new win.Event('change'));
      const preset = await pollFor(() => ZFT.config.getPref('institution.saml_entity_id') === 'https://shibboleth.imperial.ac.uk/shibboleth'
        && ZFT.config.getPref('institution.openathens_domain') === '', TIMEOUTS.edit);
      if (!preset) throw new Error('choosing Imperial did not write the Shibboleth identity provider');
      picker.value = 'other';
      picker.dispatchEvent(new win.Event('change'));
      const proxyInput = doc.getElementById('zft-proxy-prefix');
      if (!proxyInput) throw new Error('the library proxy address field is missing');
      proxyInput.value = 'https://ezproxy.example.ac.uk/login?url=';
      proxyInput.dispatchEvent(new win.Event('change'));
      const proxy = await pollFor(() => ZFT.config.getPref('institution.ezproxy_login_host') === 'ezproxy.example.ac.uk'
        && ZFT.config.getPref('institution.ezproxy_suffix') === 'ezproxy.example.ac.uk', TIMEOUTS.edit);
      if (!proxy) throw new Error('a pasted proxy address did not reach the prefs');
      return { controls: found, access: { options, preset: 'imperial', proxy: 'ezproxy.example.ac.uk' } };
    } finally {
      for (const k of accessKeys) {
        try { ZFT.config.setPref(k, savedAccess[k]); } catch (e) { ZFT.util.log('warn', 'harness could not restore ' + k); }
      }
      try { ZFT.config.setPref(key, before); } catch (e) { ZFT.util.log('warn', 'harness could not restore ' + key); }
      try { win.close(); } catch (e) { /* already closed */ }
    }
  }

  async function checkFtl() {
    const id = 'zotero-fulltext-prefs-title';
    const text = ZFT.l10n.format(id);
    if (!text || text === id) throw new Error('FTL message did not resolve: ' + id);
    return { text };
  }

  /** Open a chrome window, wait for load, close it. Console errors from our own sources are collected meanwhile. */
  async function openAndClose(url) {
    const errors = [];
    const listener = {
      observe(msg) {
        try {
          const isError = typeof Ci !== 'undefined' && msg instanceof Ci.nsIScriptError && !(msg.flags & Ci.nsIScriptError.warningFlag);
          if (isError && String(msg.sourceName || '').includes('zotero-fulltext')) {
            errors.push(String(msg.errorMessage || msg.message || '').slice(0, 300));
          }
        } catch (e) { /* a listener must not throw */ }
      },
      QueryInterface: ChromeUtils.generateQI(['nsIConsoleListener']),
    };
    Services.console.registerListener(listener);
    let win = null;
    try {
      const arg = {};
      arg.wrappedJSObject = arg;
      win = Services.ww.openWindow(null, url, '_blank', 'chrome,centerscreen,resizable,dialog=no,width=520,height=420', arg);
      const loaded = await withTimeout(new Promise((resolve) => {
        if (win.document && win.document.readyState === 'complete') resolve();
        else win.addEventListener('load', () => resolve(), { once: true });
      }), TIMEOUTS.windowLoad);
      if (!loaded) throw new Error('window did not load: ' + url);
      const root = win.document.documentElement;
      if (!root || root.localName === 'parsererror') throw new Error('window is not well-formed: ' + url);
      await ZFT.util.idle();
      return { url, errors };
    } finally {
      try { if (win) win.close(); } catch (e) { /* already closed */ }
      Services.console.unregisterListener(listener);
    }
  }

  async function checkWindows() {
    const urls = {
      progress: (ZFT.progress && ZFT.progress.WINDOW_URL) || ZFT.CONTENT_URL + 'progress.xhtml',
      login: (ZFT.login && ZFT.login.WINDOW_URL) || ZFT.CONTENT_URL + 'login.xhtml',
    };
    const detail = {};
    const failures = [];
    for (const [name, url] of Object.entries(urls)) {
      try {
        const r = await openAndClose(url);
        detail[name] = { errors: r.errors };
        if (r.errors.length) failures.push(`${name}: ${r.errors[0]}`);
      } catch (e) {
        detail[name] = { error: ZFT.util.errText(e) };
        failures.push(`${name}: ${ZFT.util.errText(e)}`);
      }
    }
    if (failures.length) throw new Error(failures.join(' | '));
    return detail;
  }

  /** name -> async function. Read at call time so a test (or a later package) can swap or add checks. */
  const CHECKS = {
    modules: checkModules,
    wasm: checkWasm,
    prefs: checkPrefs,
    menu: checkMenu,
    menu_ui: checkMenuUi,
    prefpane: checkPrefpane,
    prefpane_ui: checkPrefpaneUi,
    ftl: checkFtl,
    windows: checkWindows,
  };

  async function attempt(checks, name, fn) {
    try {
      checks[name] = { ok: true, detail: await fn() };
    } catch (e) {
      checks[name] = { ok: false, detail: ZFT.util.errText(e) };
      ZFT.util.log('warn', `harness check ${name} failed: ${ZFT.util.errText(e)}`);
    }
  }

  /** @returns {Promise<object|null>} the result object that was written, or null when dormant. Never rejects. */
  async function maybeRun() {
    const path = resultPath();
    if (!path) return null;
    const startedMs = ZFT.util.wallNow();
    const t0 = ZFT.util.monoNow();
    const checks = {};
    try {
      if (Zotero.uiReadyPromise) await withTimeout(Zotero.uiReadyPromise, TIMEOUTS.uiReady);
      for (const [name, fn] of Object.entries(CHECKS)) await attempt(checks, name, fn);
    } catch (e) {
      ZFT.util.log('error', 'harness run failed: ' + ZFT.util.errText(e));
    }
    const result = {
      plugin: 'zotero-fulltext',
      version: (ZFT.info && ZFT.info.version) || '',
      ok: Object.keys(checks).length > 0 && Object.values(checks).every((c) => c.ok),
      checks,
      startedMs,
      durationMs: ZFT.util.monoNow() - t0,
    };
    try {
      await IOUtils.writeUTF8(path, JSON.stringify(result, null, 2), { tmpPath: path + '.tmp' });
    } catch (e) {
      ZFT.util.log('error', 'harness result could not be written: ' + ZFT.util.errText(e));
    }
    if (readPref(QUIT_PREF) !== false) {
      try {
        Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit);
      } catch (e) {
        ZFT.util.log('error', 'harness quit failed: ' + ZFT.util.errText(e));
      }
    }
    return result;
  }

  ZFT.harness = { RESULT_PREF, QUIT_PREF, MODULES, CHECKS, TIMEOUTS, maybeRun, endpointMap };
})(this);
