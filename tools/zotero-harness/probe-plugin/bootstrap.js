/* zotero-fulltext runtime probe -- bootstrap.js
 *
 * Collects facts about the Zotero 7+/10 plugin runtime and writes them as JSON to the file named
 * by the pref "extensions.zotero-harness.resultFile" (or env ZOTERO_HARNESS_RESULT_FILE), then
 * quits Zotero. Meant to run ONLY in a throwaway profile created by tools/zotero-harness/run.ps1.
 */
/* global Zotero, Services, ChromeUtils, IOUtils, PathUtils, ChromeWorker, Worker, Components */

var ZFP = null;
var chromeHandle = null;
const WATCHDOG_MS = 150000;          // write whatever we have after this long
const MAIN_WINDOW_WAIT_MS = 10000;   // how long to wait for onMainWindowLoad after uiReady
const UI_READY_WAIT_MS = 60000;
const CHROME_PKG = 'zfprobe';
const CHROME_BASE = 'chrome://' + CHROME_PKG + '/content/';

function plog(msg) {
  if (!ZFP) return;
  ZFP.log.push('+' + (Date.now() - ZFP.t0) + 'ms ' + msg);
  try { Zotero.debug('[zfprobe] ' + msg); } catch (e) {}
}

function errStr(e) {
  if (e === undefined || e === null) return String(e);
  var s = (e.name ? e.name + ': ' : '') + (e.message !== undefined ? e.message : String(e));
  if (e.result !== undefined && typeof e.result === 'number') s += ' [nsresult 0x' + (e.result >>> 0).toString(16) + ']';
  if (e.fileName) s += ' @' + String(e.fileName).replace(/^.*\//, '') + ':' + e.lineNumber;
  return s;
}

async function attempt(obj, name, fn) {
  var t = Date.now();
  try {
    var v = await fn();
    obj[name] = { ok: true, ms: Date.now() - t, value: v };
  }
  catch (e) {
    obj[name] = { ok: false, ms: Date.now() - t, error: errStr(e) };
    if (e && e.stack) obj[name].stack = String(e.stack).split('\n').slice(0, 3).join(' | ');
  }
  plog(name + ' -> ' + (obj[name].ok ? 'ok' : 'FAIL ' + obj[name].error));
  return obj[name];
}

function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }

function getResultFile() {
  var p = '';
  try { p = Services.prefs.getStringPref('extensions.zotero-harness.resultFile', ''); } catch (e) {}
  if (!p) { try { p = Services.env.get('ZOTERO_HARNESS_RESULT_FILE'); } catch (e) {} }
  return p;
}

function fnInfo(f) {
  if (typeof f !== 'function') return { type: typeof f };
  var src = '';
  try { src = Function.prototype.toString.call(f).slice(0, 300); } catch (e) { src = 'toString threw: ' + e; }
  return { type: 'function', length: f.length, name: f.name, src: src };
}

function keysOf(obj) {
  if (obj === null || obj === undefined) return { type: String(obj) };
  var own = [];
  try { own = Object.getOwnPropertyNames(obj).sort(); } catch (e) { return { type: typeof obj, error: errStr(e) }; }
  var protoMethods = [];
  try {
    var p = Object.getPrototypeOf(obj);
    if (p && p !== Object.prototype && p !== Function.prototype) {
      protoMethods = Object.getOwnPropertyNames(p).filter(function (k) { return k !== 'constructor'; }).sort();
    }
  } catch (e) {}
  return { type: typeof obj, ownKeys: own, protoKeys: protoMethods };
}

function prefInfo(name) {
  var ps = Services.prefs;
  var t = ps.getPrefType(name);
  var info = { type: ({ 0: 'invalid', 32: 'string', 64: 'int', 128: 'bool' })[t] || String(t) };
  try {
    if (t === ps.PREF_BOOL) info.value = ps.getBoolPref(name);
    else if (t === ps.PREF_INT) info.value = ps.getIntPref(name);
    else if (t === ps.PREF_STRING) info.value = ps.getStringPref(name);
    if (t !== ps.PREF_INVALID) {
      info.hasUserValue = ps.prefHasUserValue(name);
      info.locked = ps.prefIsLocked(name);
      try {
        var d = ps.getDefaultBranch('');
        if (t === ps.PREF_BOOL) info.defaultValue = d.getBoolPref(name);
        else if (t === ps.PREF_INT) info.defaultValue = d.getIntPref(name);
        else if (t === ps.PREF_STRING) info.defaultValue = d.getStringPref(name);
      } catch (e) { info.defaultValue = '(no default)'; }
    }
  } catch (e) { info.error = errStr(e); }
  return info;
}

// ---------------------------------------------------------------------------------------------
// Bootstrap entry points
// ---------------------------------------------------------------------------------------------

function install() {}
function uninstall() {}

function startup(params, reason) {
  var id = params.id, version = params.version, rootURI = params.rootURI;
  ZFP = {
    t0: Date.now(),
    log: [],
    finished: false,
    rootURI: rootURI,
    mainWindowLoadEvents: [],
    mainWindowLoadWaiters: [],
    consoleMessages: [],
    watchdog: null,
    consoleListener: null,
    tiny: null,
    rust: null,
    results: {
      schema: 'zotero-fulltext/runtime-probe/v1',
      meta: {},
      bootstrapScope: {},
      prefs: {},
      apis: {},
      rustLoading: {},
      wasm: {},
      mainWindow: {},
      menuManagerLiveTest: {},
      headless: {},
      secondMainWindow: {},
      cookieJarAndAttachSim: {}
    }
  };
  var R = ZFP.results;
  R.meta.plugin = { id: id, version: version, rootURI: rootURI, reason: reason, startupParamKeys: Object.keys(params) };
  R.meta.startupCalledAt = new Date().toISOString();
  try {
    R.meta.app = {
      zoteroVersion: Zotero.version,
      appVersion: Services.appinfo.version,
      appBuildID: Services.appinfo.appBuildID,
      platformVersion: Services.appinfo.platformVersion,
      platformBuildID: Services.appinfo.platformBuildID,
      OS: Services.appinfo.OS,
      XPCOMABI: Services.appinfo.XPCOMABI,
      processType: Services.appinfo.processType,
      inSafeMode: Services.appinfo.inSafeMode,
      env_MOZ_HEADLESS: Services.env.get('MOZ_HEADLESS'),
      profileDir: PathUtils.profileDir,
      zoteroDataDir: (Zotero.DataDirectory && Zotero.DataDirectory.dir) || null,
      zoteroInitialized: Zotero.initialized
    };
  } catch (e) { R.meta.appError = errStr(e); }
  try {
    var si = Services.startup.getStartupInfo();
    var base = si.process ? si.process.getTime() : null;
    var st = {};
    for (var k in si) {
      if (si[k] && si[k].getTime) st[k] = base ? si[k].getTime() - base : si[k].toISOString();
    }
    st.startupCalled = base ? Date.now() - base : null;
    R.meta.startupInfoMsFromProcessStart = st;
  } catch (e) { R.meta.startupInfoError = errStr(e); }
  try {
    var mw = Zotero.getMainWindow();
    R.headless.atStartup = {
      mainWindowExists: !!mw,
      mainWindowReadyState: mw ? mw.document.readyState : null,
      mainWindowHref: mw ? String(mw.location.href) : null,
      openWindows: Array.from(Services.wm.getEnumerator(null)).map(function (w) { try { return String(w.location.href); } catch (e) { return '?'; } })
    };
  } catch (e) { R.headless.atStartupError = errStr(e); }

  // Capture console messages that look relevant (wasm/CSP/our plugin).
  try {
    ZFP.consoleListener = {
      observe: function (msg) {
        try {
          var text = (msg && msg.message) || String(msg);
          if (ZFP.consoleMessages.length < 80 && /zfprobe|wasm|WebAssembly|Content-Security|eval|probe@zotero-fulltext/i.test(text)) {
            ZFP.consoleMessages.push('+' + (Date.now() - ZFP.t0) + 'ms ' + text.slice(0, 600));
          }
        } catch (e) {}
      },
      QueryInterface: ChromeUtils.generateQI(['nsIConsoleListener'])
    };
    Services.console.registerListener(ZFP.consoleListener);
  } catch (e) { R.meta.consoleListenerError = errStr(e); }
  // console.* API calls (e.g. wasm-bindgen's console.warn) go through ConsoleAPIStorage
  try {
    ZFP.consoleApiObserver = {
      observe: function (subject) {
        try {
          var m = subject.wrappedJSObject;
          var text = (m.arguments || []).map(function (a) { try { return typeof a === 'string' ? a : String(a); } catch (e) { return '?'; } }).join(' ');
          if (ZFP.consoleMessages.length < 80 && /zfprobe|wasm|WebAssembly|wbg|probe_wbg/i.test(text + ' ' + (m.filename || ''))) {
            ZFP.consoleMessages.push('+' + (Date.now() - ZFP.t0) + 'ms console.' + m.level + ' [' + String(m.filename || '').replace(/^.*\//, '') + '] ' + text.slice(0, 600));
          }
        } catch (e) {}
      }
    };
    Services.obs.addObserver(ZFP.consoleApiObserver, 'console-api-log-event');
  } catch (e) { R.meta.consoleApiObserverError = errStr(e); }

  // Register chrome://zfprobe/content/ -> <plugin>/content/
  try {
    var aomStartup = Components.classes['@mozilla.org/addons/addon-manager-startup;1']
      .getService(Components.interfaces.amIAddonManagerStartup);
    var manifestURI = Services.io.newURI(rootURI + 'manifest.json');
    chromeHandle = aomStartup.registerChrome(manifestURI, [['content', CHROME_PKG, 'content/']]);
    R.meta.chromeRegistered = true;
  } catch (e) { R.meta.chromeRegistered = false; R.meta.chromeRegisterError = errStr(e); }

  ZFP.watchdog = setTimeout(function () { finish('watchdog-' + WATCHDOG_MS + 'ms'); }, WATCHDOG_MS);
  plog('startup returned; main flow detached');
  mainFlow().catch(function (e) {
    R.meta.mainFlowError = errStr(e) + ' | ' + (e && e.stack ? String(e.stack).split('\n').slice(0, 4).join(' | ') : '');
    finish('mainFlow-error');
  });
  // deliberately NOT returning a promise: Zotero awaits startup() before registering its
  // main-window listener, so blocking here would bias the onMainWindowLoad measurement.
}

function onMainWindowLoad(params) {
  if (!ZFP) return;
  var win = params && params.window;
  var ev = { atMs: Date.now() - ZFP.t0, href: win ? String(win.location.href) : null, readyState: win ? win.document.readyState : null };
  ZFP.mainWindowLoadEvents.push(ev);
  plog('onMainWindowLoad fired: ' + JSON.stringify(ev));
  var waiters = ZFP.mainWindowLoadWaiters.splice(0);
  waiters.forEach(function (w) { w(win); });
}

function onMainWindowUnload() {}

function shutdown() {
  try { if (ZFP && ZFP.consoleListener) Services.console.unregisterListener(ZFP.consoleListener); } catch (e) {}
  try { if (ZFP && ZFP.consoleApiObserver) Services.obs.removeObserver(ZFP.consoleApiObserver, 'console-api-log-event'); } catch (e) {}
  try { if (chromeHandle) chromeHandle.destruct(); } catch (e) {}
  chromeHandle = null;
}

// ---------------------------------------------------------------------------------------------
// Main flow
// ---------------------------------------------------------------------------------------------

async function mainFlow() {
  var R = ZFP.results;
  var rootURI = ZFP.rootURI;

  // Load the shared wasm test script into THIS sandbox global.
  await attempt(R.bootstrapScope, 'loadSubScript.wasmTests.chromeURL', function () {
    Services.scriptloader.loadSubScript(CHROME_BASE + 'wasm-tests.js', globalThis);
    return typeof globalThis.zfRunWasmTests;
  });
  if (typeof globalThis.zfRunWasmTests !== 'function') {
    await attempt(R.bootstrapScope, 'loadSubScript.wasmTests.rootURI', function () {
      Services.scriptloader.loadSubScript(rootURI + 'content/wasm-tests.js', globalThis);
      return typeof globalThis.zfRunWasmTests;
    });
  }
  ZFP.tiny = globalThis.zfTinyAddModuleBytes();

  // (5) bootstrap scope globals + functional checks
  R.bootstrapScope.globals = globalThis.zfDescribeGlobal();
  await probeBootstrapFunctional(R.bootstrapScope);

  // (4) prefs
  R.prefs = probePrefs();

  // (5) Zotero API shapes
  R.apis = probeApis();
  await attempt(R.menuManagerLiveTest, 'registerAndUnregister', function () { return menuManagerLiveTest(); });

  // (3) Rust wasm loading methods (bootstrap scope)
  await probeRustLoading(R.rustLoading, rootURI);

  // (1) WebAssembly in the bootstrap sandbox
  await attempt(R.wasm, 'bootstrapSandbox', async function () {
    return JSON.parse(await globalThis.zfRunWasmTests(ZFP.tiny, ZFP.rust, 'bootstrap-sandbox'));
  });

  // (2) ES module in the shared system global, registered chrome:// URL
  await attempt(R.wasm, 'esModule', async function () {
    var m = ChromeUtils.importESModule(CHROME_BASE + 'probe-module.mjs');
    return {
      loadedAt: m.loadedAt,
      describe: JSON.parse(m.describe()),
      tests: JSON.parse(await m.runWasm(ZFP.tiny, ZFP.rust, 'esm-shared-global')),
      direct: JSON.parse(await m.directCheck(ZFP.rust || ZFP.tiny))
    };
  });

  // (2) workers
  await attempt(R.wasm, 'chromeWorker', function () { return probeWorker(ChromeWorker, 'chromeworker'); });
  await attempt(R.wasm, 'worker', function () { return probeWorker(Worker, 'worker'); });

  // (3b) wasm-bindgen generated glue (web target as ES module, no-modules target as script)
  R.wasmBindgen = {};
  await probeWasmBindgen(R.wasmBindgen);

  // (2b) non-system (null principal) sandbox, like translator sandboxes
  await attempt(R.wasm, 'nullPrincipalSandbox', function () {
    var Cu = Components.utils;
    var sb = Cu.Sandbox(null, { sandboxName: 'zfprobe-null-principal' });
    var bytes = Cu.cloneInto(Array.from(ZFP.tiny), sb);
    sb.bytes = bytes;
    return Cu.evalInSandbox(
      'var u = new Uint8Array(bytes); var r = { typeofWebAssembly: typeof WebAssembly };' +
      'try { r.add = new WebAssembly.Instance(new WebAssembly.Module(u), {}).exports.add(3, 4); } catch (e) { r.error = String(e); }' +
      'JSON.stringify(r)', sb);
  });

  // (6) UI readiness and main window
  var uiReady = await Promise.race([
    Zotero.uiReadyPromise.then(function () { return true; }),
    sleep(UI_READY_WAIT_MS).then(function () { return false; })
  ]);
  R.headless.uiReadyResolved = uiReady;
  R.headless.uiReadyAtMs = Date.now() - ZFP.t0;
  plog('uiReadyPromise resolved=' + uiReady);

  var win = null;
  if (ZFP.mainWindowLoadEvents.length) {
    win = Zotero.getMainWindow();
  } else {
    win = await Promise.race([
      new Promise(function (resolve) { ZFP.mainWindowLoadWaiters.push(resolve); }),
      sleep(MAIN_WINDOW_WAIT_MS).then(function () { return null; })
    ]);
  }
  R.headless.onMainWindowLoadFired = ZFP.mainWindowLoadEvents.length > 0;
  R.headless.onMainWindowLoadEvents = ZFP.mainWindowLoadEvents;
  var fallback = Zotero.getMainWindow();
  R.headless.afterWait = {
    getMainWindowExists: !!fallback,
    readyState: fallback ? fallback.document.readyState : null,
    openWindows: Array.from(Services.wm.getEnumerator(null)).map(function (w) { try { return String(w.location.href); } catch (e) { return '?'; } })
  };
  win = win || fallback;
  if (win) {
    await probeMainWindow(R.mainWindow, win, ZFP.mainWindowLoadEvents.length ? 'onMainWindowLoad' : 'Zotero.getMainWindow-fallback');
  } else {
    R.mainWindow.skipped = 'no main window available';
  }

  // (6b) does onMainWindowLoad fire for a main window opened AFTER plugin startup?
  await attempt(R.secondMainWindow, 'openMainWindowAfterStartup', function () { return probeSecondMainWindow(); });

  // (7) local-only cookie jar + attachment pipeline simulation (127.0.0.1 httpd in this process)
  if (!Services.prefs.getBoolPref('extensions.zotero-harness.skipCookieSim', false)) {
    await attempt(R.cookieJarAndAttachSim, 'run', function () { return probeCookiesAndAttach(R.cookieJarAndAttachSim); });
  }

  await finish('complete');
}

async function probeWasmBindgen(B) {
  var sample = JSON.stringify({ doi: 'https://doi.org/10.1000/ABC', text: 'wbg ok' });
  function exercise(api) {
    var out = { add: api.add(2, 3), processJson: JSON.parse(api.process_json(sample)) };
    try { api.fail_if_empty(''); out.failIfEmpty = 'did not throw'; } catch (e) { out.failIfEmpty = 'threw: ' + (e && e.message !== undefined ? e.message : String(e)); }
    out.failIfEmptyOk = api.fail_if_empty('abc');
    return out;
  }
  // web target via ChromeUtils.importESModule, default init (fetches ./probe_wbg_bg.wasm via import.meta.url)
  await attempt(B, 'web.importESModule(.js).defaultInit', async function () {
    var m = ChromeUtils.importESModule(CHROME_BASE + 'wbg-web/probe_wbg.js');
    await m.default();
    return exercise(m);
  });
  // same glue under .mjs URL = a fresh module instance; init with bytes we loaded ourselves
  var bytes = null;
  await attempt(B, 'fetch wbg wasm bytes', async function () {
    bytes = new Uint8Array(await (await fetch(CHROME_BASE + 'wbg-web/probe_wbg_bg.wasm')).arrayBuffer());
    return bytes.length;
  });
  var m2 = null;
  await attempt(B, 'web.importESModule(.mjs).initSync({module}) naive cross-compartment object', function () {
    m2 = ChromeUtils.importESModule(CHROME_BASE + 'wbg-web/probe_wbg.mjs');
    m2.initSync({ module: bytes });
    return exercise(m2);
  });
  await attempt(B, 'web.importESModule(.mjs).initSync(Cu.cloneInto({module}, esmGlobal))', function () {
    var g = Components.utils.getGlobalForObject(m2);
    m2.initSync(Components.utils.cloneInto({ module: bytes }, g));
    return exercise(m2);
  });
  // no-modules target as a classic script
  await attempt(B, 'nomod.loadSubScript(into object).wasm_bindgen visible?', function () {
    var s = {};
    Services.scriptloader.loadSubScript(CHROME_BASE + 'wbg-nomod/probe_wbg.js', s);
    return { typeofWasmBindgenOnTarget: typeof s.wasm_bindgen };
  });
  await attempt(B, 'nomod.new Function(src + return wasm_bindgen).initSync({module})', async function () {
    var src = await (await fetch(CHROME_BASE + 'wbg-nomod/probe_wbg.js')).text();
    var wb = new Function(src + '\nreturn wasm_bindgen;')();
    var nb = new Uint8Array(await (await fetch(CHROME_BASE + 'wbg-nomod/probe_wbg_bg.wasm')).arrayBuffer());
    wb.initSync({ module: nb });
    return exercise(wb);
  });
}

async function probeSecondMainWindow() {
  var before = ZFP.mainWindowLoadEvents.length;
  var winsBefore = Zotero.getMainWindows();
  var got = new Promise(function (resolve) { ZFP.mainWindowLoadWaiters.push(resolve); });
  Zotero.openMainWindow();
  var w = await Promise.race([got, sleep(15000).then(function () { return null; })]);
  var out = {
    onMainWindowLoadFired: ZFP.mainWindowLoadEvents.length > before,
    event: ZFP.mainWindowLoadEvents[before] || null,
    mainWindowsBefore: winsBefore.length,
    mainWindowsAfterOpen: Zotero.getMainWindows().length
  };
  await sleep(1500);
  // close only windows that were not there before
  var closed = 0;
  Zotero.getMainWindows().forEach(function (mw) {
    if (winsBefore.indexOf(mw) === -1) { try { mw.close(); closed++; } catch (e) {} }
  });
  await sleep(1000);
  out.closedNewWindows = closed;
  out.mainWindowsAfterClose = Zotero.getMainWindows().length;
  return out;
}

// Minimal ASCII PDF (no binary bytes, so httpd's string write is lossless)
var MINI_PDF = '%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n' +
  '3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n';

function listLocalCookies() {
  var out = [];
  try {
    for (var c of Services.cookies.cookies) {
      if (c.host.indexOf('127.0.0.1') === -1) continue;
      out.push({
        name: c.name, value: c.value, host: c.host, path: c.path, isSession: c.isSession,
        isHttpOnly: c.isHttpOnly, sameSite: c.sameSite,
        partitionKey: c.originAttributes.partitionKey, userContextId: c.originAttributes.userContextId
      });
    }
  } catch (e) { out.push({ error: errStr(e) }); }
  return out;
}

async function probeCookiesAndAttach(C) {
  var { HttpServer } = ChromeUtils.importESModule('chrome://remote/content/server/httpd.sys.mjs');
  var server = new HttpServer();
  var seen = [];
  function hdr(req, n) { try { return req.hasHeader(n) ? req.getHeader(n) : null; } catch (e) { return null; } }
  function rec(req) {
    var q = new URLSearchParams(req.queryString || '');
    seen.push({
      atMs: Date.now() - ZFP.t0, path: req.path, via: q.get('via'), cookie: hdr(req, 'Cookie'),
      userAgent: (hdr(req, 'User-Agent') || '').slice(0, 200), referer: hdr(req, 'Referer'),
      accept: (hdr(req, 'Accept') || '').slice(0, 120)
    });
  }
  function reply(res, req, status, type, body, extraHeaders) {
    res.setStatusLine(req.httpVersion, status, status === 200 ? 'OK' : 'Found');
    if (type) res.setHeader('Content-Type', type, false);
    (extraHeaders || []).forEach(function (h) { res.setHeader(h[0], h[1], false); });
    if (body) res.write(body);
  }
  server.registerPathHandler('/set-cookie', function (req, res) {
    rec(req);
    var name = new URLSearchParams(req.queryString || '').get('name') || 'c';
    reply(res, req, 200, 'text/plain', 'set ' + name, [['Set-Cookie', name + '=1; Path=/']]);
  });
  server.registerPathHandler('/echo', function (req, res) {
    rec(req);
    reply(res, req, 200, 'application/json', JSON.stringify({ cookie: hdr(req, 'Cookie') }));
  });
  server.registerPathHandler('/login', function (req, res) {
    rec(req);
    reply(res, req, 200, 'text/html', '<!doctype html><html><head><title>simulated SSO done</title></head><body>logged in</body></html>',
      [['Set-Cookie', 'sess=ok; Path=/; HttpOnly']]);
  });
  server.registerPathHandler('/login-form', function (req, res) {
    rec(req);
    reply(res, req, 200, 'text/html', '<!doctype html><html><head><title>Sign in</title></head><body><form><input name="user"><input type="password" name="pass"></form></body></html>');
  });
  server.registerPathHandler('/protected.pdf', function (req, res) {
    rec(req);
    if (/\bsess=ok\b/.test(hdr(req, 'Cookie') || '')) {
      reply(res, req, 200, 'application/pdf', MINI_PDF);
    } else {
      reply(res, req, 302, null, null, [['Location', '/login-form?via=redirect']]);
    }
  });
  server.registerPathHandler('/landing', function (req, res) {
    rec(req);
    var base = 'http://127.0.0.1:' + server.identity.primaryPort;
    reply(res, req, 200, 'text/html',
      '<!doctype html><html><head><title>Landing page</title>' +
      '<meta name="citation_title" content="zfprobe landing article">' +
      '<meta name="citation_pdf_url" content="' + base + '/protected.pdf?via=citation_pdf_url">' +
      '</head><body><a href="/protected.pdf?via=link">PDF</a></body></html>');
  });
  server.registerPathHandler('/doc', function (req, res) {
    rec(req);
    reply(res, req, 200, 'text/html', '<!doctype html><html><head><title>processDocuments page</title></head><body>x</body></html>',
      [['Set-Cookie', 'pd=1; Path=/']]);
  });
  server.start(-1);
  var port = server.identity.primaryPort;
  var base = 'http://127.0.0.1:' + port;
  C.base = base;
  var S = C.steps = {};
  try {
    await attempt(S, '01 Zotero.HTTP.request sets cookie zh', async function () {
      var x = await Zotero.HTTP.request('GET', base + '/set-cookie?name=zh&via=zhttp-set');
      return { status: x.status };
    });
    C.jarAfterZhttpSet = listLocalCookies();
    await attempt(S, '02 fetch default credentials -> echo', async function () {
      return (await (await fetch(base + '/echo?via=fetch-default')).json());
    });
    await attempt(S, '03 fetch credentials:include -> echo', async function () {
      return (await (await fetch(base + '/echo?via=fetch-include', { credentials: 'include' })).json());
    });
    await attempt(S, '04 Zotero.HTTP.request -> echo', async function () {
      var x = await Zotero.HTTP.request('GET', base + '/echo?via=zhttp');
      return JSON.parse(x.responseText);
    });
    await attempt(S, '05 XHR withCredentials=false -> echo', function () {
      return new Promise(function (resolve, reject) {
        var x = new XMLHttpRequest();
        x.open('GET', base + '/echo?via=xhr');
        x.onload = function () { resolve(JSON.parse(x.responseText)); };
        x.onerror = function () { reject(new Error('xhr error')); };
        x.send();
      });
    });
    await attempt(S, '06 fetch credentials:include sets cookie fe', async function () {
      var r = await fetch(base + '/set-cookie?name=fe&via=fetch-set', { credentials: 'include' });
      return { status: r.status };
    });
    await attempt(S, '07 Zotero.HTTP.request -> echo (sees fe?)', async function () {
      var x = await Zotero.HTTP.request('GET', base + '/echo?via=zhttp-after-fetch-set');
      return JSON.parse(x.responseText);
    });
    C.jarBeforeLogin = listLocalCookies();

    var tmpDir = PathUtils.join(PathUtils.tempDir, 'zfprobe-' + Date.now());
    await IOUtils.makeDirectory(tmpDir);
    await attempt(S, '08 Attachments.downloadFile protected.pdf BEFORE login (expect fail)', async function () {
      var p = PathUtils.join(tmpDir, 'pre.pdf');
      await Zotero.Attachments.downloadFile(base + '/protected.pdf?via=downloadFile-prelogin', p, { enforceFileType: true });
      return { unexpectedlySucceeded: true, size: (await IOUtils.stat(p)).size };
    });

    await attempt(S, '09 openInViewer(/login) sets HttpOnly sess cookie in a content <browser>', async function () {
      var n0 = seen.length;
      var vw = Zotero.openInViewer(base + '/login?via=viewer');
      var t0 = Date.now();
      while (Date.now() - t0 < 15000 && !seen.slice(n0).some(function (s) { return s.path === '/login'; })) await sleep(200);
      await sleep(1500);
      var title = null;
      try { title = vw.document.title; } catch (e) {}
      var browserInfo = null;
      try {
        var b = vw.document.documentElement.getElementsByTagName('browser')[0];
        browserInfo = { remote: b.getAttribute('remote'), isRemoteBrowser: b.isRemoteBrowser, currentURI: b.currentURI && b.currentURI.spec, userContextId: b.getAttribute('usercontextid') };
      } catch (e) { browserInfo = { error: errStr(e) }; }
      try { vw.close(); } catch (e) {}
      return { loginRequestSeen: seen.slice(n0).some(function (s) { return s.path === '/login'; }), viewerTitle: title, browser: browserInfo };
    });
    C.jarAfterViewerLogin = listLocalCookies();

    await attempt(S, '10 Attachments.downloadFile protected.pdf AFTER login', async function () {
      var p = PathUtils.join(tmpDir, 'post.pdf');
      await Zotero.Attachments.downloadFile(base + '/protected.pdf?via=downloadFile-postlogin', p, { enforceFileType: true });
      var u8 = await IOUtils.read(p);
      return { size: u8.length, head: String.fromCharCode.apply(null, Array.from(u8.slice(0, 8))) };
    });
    await attempt(S, '11 fetch credentials:include protected.pdf AFTER login', async function () {
      var r = await fetch(base + '/protected.pdf?via=fetch-postlogin', { credentials: 'include' });
      return { status: r.status, url: r.url, contentType: r.headers.get('content-type'), redirected: r.redirected };
    });
    await attempt(S, '12 fetch default credentials protected.pdf AFTER login', async function () {
      var r = await fetch(base + '/protected.pdf?via=fetch-default-postlogin');
      return { status: r.status, url: r.url, contentType: r.headers.get('content-type'), redirected: r.redirected };
    });

    // Zotero item + attachment pipeline (throwaway DB). addFileFromURLs forces http->https
    // unless Zotero.test is set, so it is set ONLY around these calls for the local http server.
    var item = null;
    await attempt(S, '13 create journalArticle item with DOI', async function () {
      item = new Zotero.Item('journalArticle');
      item.libraryID = Zotero.Libraries.userLibraryID;
      item.setField('title', 'zfprobe test article');
      item.setField('DOI', '10.5555/zfprobe.1');
      item.setField('url', base + '/landing?via=item-url-field');
      await item.saveTx();
      return { id: item.id, key: item.key, canFindFileForItem: Zotero.Attachments.canFindFileForItem(item) };
    });
    await attempt(S, '14 getFileResolvers(item) shape', function () {
      return Zotero.Attachments.getFileResolvers(item).map(function (r) { return typeof r === 'function' ? '[async function]' : r; });
    });
    async function attInfo(att) {
      if (!att) return att;
      return {
        id: att.id, key: att.key, parentID: att.parentID, linkMode: att.attachmentLinkMode,
        contentType: att.attachmentContentType, title: att.getField('title'), url: att.getField('url'),
        fileExists: await att.fileExists(), path: await att.getFilePathAsync()
      };
    }
    await attempt(S, '15 addFileFromURLs(item, [{url: protected.pdf}]) with Zotero.test=true', async function () {
      var prev = Zotero.test;
      Zotero.test = true;
      try {
        return await attInfo(await Zotero.Attachments.addFileFromURLs(item,
          [{ url: base + '/protected.pdf?via=addFileFromURLs-url', accessMethod: 'zfprobe-direct' }]));
      } finally { Zotero.test = prev; }
    });
    var item2 = null;
    await attempt(S, '16 addFileFromURLs(item2, [{pageURL: landing}]) -> translator finds citation_pdf_url?', async function () {
      item2 = new Zotero.Item('journalArticle');
      item2.libraryID = Zotero.Libraries.userLibraryID;
      item2.setField('title', 'zfprobe landing test');
      await item2.saveTx();
      var prev = Zotero.test;
      Zotero.test = true;
      try {
        return await attInfo(await Zotero.Attachments.addFileFromURLs(item2,
          [{ pageURL: base + '/landing?via=addFileFromURLs-pageURL', accessMethod: 'zfprobe-landing' }]));
      } finally { Zotero.test = prev; }
    });
    await attempt(S, '17 addFileFromURLs with plain http URL and Zotero.test=false (https forcing)', async function () {
      var item3 = new Zotero.Item('journalArticle');
      item3.libraryID = Zotero.Libraries.userLibraryID;
      item3.setField('title', 'zfprobe https-forcing test');
      await item3.saveTx();
      return await attInfo(await Zotero.Attachments.addFileFromURLs(item3,
        [{ url: base + '/protected.pdf?via=addFileFromURLs-noTestFlag', accessMethod: 'zfprobe-https-forcing' }]));
    });
    await attempt(S, '18 Zotero.HTTP.processDocuments(/doc)', async function () {
      var r = await Zotero.HTTP.processDocuments(base + '/doc?via=processDocuments', function (doc) { return doc.title; });
      return r;
    });
    C.jarFinal = listLocalCookies();
    try { await IOUtils.remove(tmpDir, { recursive: true }); } catch (e) {}
  } finally {
    C.serverLog = seen;
    await new Promise(function (r) { server.stop(r); });
  }
  return { base: base, requests: seen.length };
}

async function probeBootstrapFunctional(B) {
  var F = B.functional = {};
  await attempt(F, 'DOMParser', function () {
    var d = new DOMParser().parseFromString('<html><head><meta name="citation_pdf_url" content="https://x.test/a.pdf"></head><body><p>hi</p></body></html>', 'text/html');
    return { p: d.querySelector('p').textContent, citation_pdf_url: d.querySelector('meta[name="citation_pdf_url"]').getAttribute('content') };
  });
  await attempt(F, 'TextEncoderDecoder', function () {
    var u = new TextEncoder().encode('héllo');
    return { encodedLen: u.length, roundTrip: new TextDecoder().decode(u) };
  });
  await attempt(F, 'URL', function () {
    var u = new URL('https://login.ezproxy.example.ac.uk/login?url=https://doi.org/10.1/x');
    return { host: u.host, url: u.searchParams.get('url') };
  });
  await attempt(F, 'AbortController.inSandbox', function () {
    var ac = new AbortController(); ac.abort(); return ac.signal.aborted;
  });
  await attempt(F, 'AbortController.fromMainWindowOrHiddenWindow', function () {
    var w = Zotero.getMainWindow() || Services.appShell.hiddenDOMWindow;
    var ac = new w.AbortController(); ac.abort(); return { via: Zotero.getMainWindow() ? 'mainWindow' : 'hiddenDOMWindow', aborted: ac.signal.aborted };
  });
  await attempt(F, 'crypto.subtle.digest', async function () {
    var h = await crypto.subtle.digest('SHA-256', new TextEncoder().encode('abc'));
    return Array.from(new Uint8Array(h)).map(function (b) { return b.toString(16).padStart(2, '0'); }).join('');
  });
  await attempt(F, 'fetch.dataURL', async function () {
    var r = await fetch('data:application/json,%7B%22a%22%3A1%7D');
    return { status: r.status, json: await r.json() };
  });
  await attempt(F, 'ResponseCtor', function () {
    var r = new Response(new Uint8Array([1, 2]), { headers: { 'Content-Type': 'application/wasm' } });
    return { type: typeof r, ct: r.headers.get('content-type') };
  });
  await attempt(F, 'Cc_Ci_Cu', function () {
    return { Cc: typeof Cc, Ci: typeof Ci, Cu: typeof Cu, Components: typeof Components };
  });
  await attempt(F, 'IOUtils.PathUtils', function () {
    return { IOUtils: typeof IOUtils, writeUTF8: typeof IOUtils.writeUTF8, read: typeof IOUtils.read, tempDir: PathUtils.tempDir };
  });
}

function probePrefs() {
  var out = { wasmChildList: {}, selected: {} };
  try {
    var names = Services.prefs.getChildList('javascript.options.wasm').sort();
    names.forEach(function (n) { out.wasmChildList[n] = prefInfo(n); });
  } catch (e) { out.wasmChildListError = errStr(e); }
  [
    'javascript.options.wasm',
    'javascript.options.wasm_trustedprincipals',
    'javascript.options.wasm_baselinejit',
    'javascript.options.wasm_optimizingjit',
    'javascript.options.wasm_verbose',
    'javascript.options.shared_memory',
    'network.cookie.cookieBehavior',
    'network.cookie.cookieBehavior.pbmode',
    'network.cookie.sameSite.laxByDefault',
    'network.cookie.sameSite.noneRequiresSecure',
    'privacy.firstparty.isolate',
    'privacy.partition.network_state',
    'network.http.referer.XOriginPolicy',
    'security.allow_eval_with_system_principal',
    'security.allow_eval_in_parent_process',
    'security.allow_parent_unrestricted_js_loads',
    'extensions.zotero.httpServer.enabled',
    'extensions.zotero.dataDir',
    'extensions.zotero.useDataDir',
    'extensions.zotero.automaticScraperUpdates',
    'extensions.zotero.findPDFs.resolvers',
    'extensions.zotero.downloadAssociatedFiles',
    'xpinstall.signatures.required',
    'extensions.autoDisableScopes',
    'app.update.auto'
  ].forEach(function (n) { out.selected[n] = prefInfo(n); });
  try {
    out.zoteroFindPDFsPrefs = Services.prefs.getChildList('extensions.zotero.findPDFs').sort();
  } catch (e) {}
  return out;
}

function probeApis() {
  var A = {};
  var Z = Zotero;
  A['Zotero.version'] = Z.version;
  A['Zotero.platformVersion'] = Z.platformVersion;
  A['Zotero.isWin'] = Z.isWin;
  A['Zotero.MenuManager'] = keysOf(Z.MenuManager);
  A['Zotero.MenuManager.registerMenu'] = fnInfo(Z.MenuManager && Z.MenuManager.registerMenu);
  A['Zotero.MenuManager.unregisterMenu'] = fnInfo(Z.MenuManager && Z.MenuManager.unregisterMenu);
  A['Zotero.PreferencePanes'] = keysOf(Z.PreferencePanes);
  A['Zotero.PreferencePanes.register'] = fnInfo(Z.PreferencePanes && Z.PreferencePanes.register);
  A['Zotero.ProgressWindow'] = fnInfo(Z.ProgressWindow);
  try { A['Zotero.ProgressWindow.prototypeKeys'] = Object.getOwnPropertyNames(new Z.ProgressWindow({ closeOnClick: true })).sort(); } catch (e) { A['Zotero.ProgressWindow.prototypeKeys'] = 'error: ' + errStr(e); }
  A['Zotero.HTTP'] = keysOf(Z.HTTP);
  A['Zotero.HTTP.request'] = fnInfo(Z.HTTP && Z.HTTP.request);
  A['Zotero.HTTP.processDocuments'] = fnInfo(Z.HTTP && Z.HTTP.processDocuments);
  var att = Z.Attachments || {};
  ['addFileFromURLs', 'downloadFirstAvailableFile', 'addAvailableFile', 'addAvailableFiles', 'addAvailablePDF',
    'getFileResolvers', 'getPDFResolvers', 'importFromURL', 'downloadFile', 'canFindFileForItem', 'linkFromURL',
    'importFromFile', 'cleanAttachmentURI', 'getRenamedFileBaseNameIfAllowedType'].forEach(function (n) {
    A['Zotero.Attachments.' + n] = fnInfo(att[n]);
  });
  try {
    A['Zotero.Attachments.relevantKeys'] = Object.getOwnPropertyNames(att).filter(function (k) { return /resolver|available|url|download|find|pdf/i.test(k); }).sort();
  } catch (e) {}
  A['Zotero.BrowserDownload'] = keysOf(Z.BrowserDownload);
  A['Zotero.BrowserRequest'] = keysOf(Z.BrowserRequest);
  ['downloadPDF', 'downloadPDFViaViewer', 'clearChallenge', 'clearChallengeInViewer', 'getEntryForURL'].forEach(function (n) {
    A['Zotero.BrowserRequest.' + n] = fnInfo(Z.BrowserRequest && Z.BrowserRequest[n]);
  });
  A['Zotero.HTTP.download'] = fnInfo(Z.HTTP && Z.HTTP.download);
  A['Zotero.Utilities.Internal.getOpenAccessPDFURLs'] = fnInfo(Z.Utilities && Z.Utilities.Internal && Z.Utilities.Internal.getOpenAccessPDFURLs);
  A['Zotero.Utilities.Internal.getFileFromDocument'] = fnInfo(Z.Utilities && Z.Utilities.Internal && Z.Utilities.Internal.getFileFromDocument);
  A['Zotero.getMainWindows'] = fnInfo(Z.getMainWindows);
  A['Zotero.openMainWindow'] = fnInfo(Z.openMainWindow);
  A['Zotero.Proxies'] = keysOf(Z.Proxies);
  ['properToProxy', 'proxyToProper', 'getPotentialProxies', 'save', 'init'].forEach(function (n) {
    A['Zotero.Proxies.' + n] = fnInfo(Z.Proxies && Z.Proxies[n]);
  });
  A['Zotero.Proxy'] = fnInfo(Z.Proxy);
  A['Zotero.openInViewer'] = fnInfo(Z.openInViewer);
  A['Zotero.launchURL'] = fnInfo(Z.launchURL);
  A['Zotero.Utilities.cleanDOI'] = fnInfo(Z.Utilities && Z.Utilities.cleanDOI);
  A['Zotero.Plugins'] = keysOf(Z.Plugins);
  A['Zotero.ItemTreeManager'] = keysOf(Z.ItemTreeManager);
  A['Zotero.ItemPaneManager'] = keysOf(Z.ItemPaneManager);
  A['Zotero.getActiveZoteroPane'] = fnInfo(Z.getActiveZoteroPane);
  A['Zotero.getMainWindow'] = fnInfo(Z.getMainWindow);
  A['Zotero.File.getBinaryContentsAsync'] = fnInfo(Z.File && Z.File.getBinaryContentsAsync);
  return A;
}

function menuManagerLiveTest() {
  if (!Zotero.MenuManager) return 'Zotero.MenuManager missing';
  var id = Zotero.MenuManager.registerMenu({
    menuID: 'zfprobe-test-menu',
    pluginID: ZFP.results.meta.plugin.id,
    target: 'main/library/item',
    menus: [{ menuType: 'menuitem', l10nID: 'zfprobe-nonexistent-l10n-id', onCommand: function () {} }]
  });
  var unreg = id ? Zotero.MenuManager.unregisterMenu(id) : null;
  return { registerReturned: id, unregisterReturned: unreg };
}

async function quickRust(instance) {
  var e = instance.exports;
  var input = new TextEncoder().encode(JSON.stringify({ doi: 'doi:10.1038/NATURE12373', text: 'ok', numbers: [2, 3] }));
  var p = e.zf_alloc(input.length);
  new Uint8Array(e.memory.buffer, p, input.length).set(input);
  var packed = BigInt.asUintN(64, BigInt(e.zf_process_json(p, input.length)));
  e.zf_dealloc(p, input.length);
  var op = Number(packed & 0xffffffffn), ol = Number(packed >> 32n);
  var out = JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, op, ol).slice()));
  e.zf_dealloc(op, ol);
  return { doi_normalized: out.doi_normalized, sum: out.sum, ok: out.ok };
}

async function sha256Hex(u8) {
  var h = await crypto.subtle.digest('SHA-256', u8);
  return Array.from(new Uint8Array(h)).map(function (b) { return b.toString(16).padStart(2, '0'); }).join('');
}

async function verifyBytes(u8, how) {
  if (!ZFP.rust && u8 && u8.length) { ZFP.rust = u8; ZFP.results.rustLoading.bytesUsedForOtherTestsFrom = how; }
  var r = await WebAssembly.instantiate(u8, {});
  return { bytes: u8.length, sha256: await sha256Hex(u8), call: await quickRust(r.instance) };
}

async function probeRustLoading(L, rootURI) {
  var rootWasm = rootURI + 'probe_core.wasm';
  var chromeWasm = CHROME_BASE + 'probe_core.wasm';
  L.urls = { rootWasm: rootWasm, chromeWasm: chromeWasm, rootURIScheme: Services.io.newURI(rootURI).scheme };
  var M = L.methods = {};

  async function viaFetch(url) {
    var r = await fetch(url);
    var info = { status: r.status, ok: r.ok, type: r.type, url: r.url, contentType: r.headers.get('content-type') };
    var u8 = new Uint8Array(await r.arrayBuffer());
    info.verify = await verifyBytes(u8, 'fetch ' + url);
    return info;
  }
  await attempt(M, 'fetch(rootURI)', function () { return viaFetch(rootWasm); });
  await attempt(M, 'fetch(chrome://)', function () { return viaFetch(chromeWasm); });
  await attempt(M, 'instantiateStreaming(fetch(chrome://))', async function () {
    var res = await WebAssembly.instantiateStreaming(fetch(chromeWasm), {});
    return await quickRust(res.instance);
  });
  await attempt(M, 'instantiateStreaming(fetch(rootURI))', async function () {
    var res = await WebAssembly.instantiateStreaming(fetch(rootWasm), {});
    return await quickRust(res.instance);
  });
  await attempt(M, 'compileStreaming(new Response(buf, application/wasm))', async function () {
    var buf = await (await fetch(chromeWasm)).arrayBuffer();
    var m = await WebAssembly.compileStreaming(new Response(buf, { headers: { 'Content-Type': 'application/wasm' } }));
    var inst = await WebAssembly.instantiate(m, {});
    return await quickRust(inst);
  });
  await attempt(M, 'IOUtils.read(file path from rootURI)', async function () {
    var uri = Services.io.newURI(rootWasm);
    if (!(uri instanceof Components.interfaces.nsIFileURL)) {
      throw new Error('rootURI is not a file: URL (' + uri.scheme + ') -- packed XPI; IOUtils cannot read inside the zip');
    }
    var path = uri.QueryInterface(Components.interfaces.nsIFileURL).file.path;
    var u8 = await IOUtils.read(path);
    return { path: path, verify: await verifyBytes(u8, 'IOUtils.read') };
  });
  function viaXHR(url) {
    return new Promise(function (resolve, reject) {
      var x = new XMLHttpRequest();
      x.open('GET', url);
      x.responseType = 'arraybuffer';
      x.onload = function () {
        verifyBytes(new Uint8Array(x.response), 'XHR ' + url)
          .then(function (v) { resolve({ status: x.status, contentType: x.getResponseHeader('content-type'), verify: v }); }, reject);
      };
      x.onerror = function () { reject(new Error('XHR onerror status=' + x.status)); };
      x.send();
    });
  }
  await attempt(M, 'XMLHttpRequest(chrome://)', function () { return viaXHR(chromeWasm); });
  await attempt(M, 'XMLHttpRequest(rootURI)', function () { return viaXHR(rootWasm); });
  await attempt(M, 'Zotero.HTTP.request(chrome://)', async function () {
    var xhr = await Zotero.HTTP.request('GET', chromeWasm, { responseType: 'arraybuffer' });
    return { status: xhr.status, verify: await verifyBytes(new Uint8Array(xhr.response), 'Zotero.HTTP.request') };
  });
  await attempt(M, 'NetUtil.newChannel(chrome://).open() sync', async function () {
    var { NetUtil } = ChromeUtils.importESModule('resource://gre/modules/NetUtil.sys.mjs');
    var ch = NetUtil.newChannel({ uri: chromeWasm, loadUsingSystemPrincipal: true });
    var stream = ch.open();
    var n = stream.available();
    var str = NetUtil.readInputStreamToString(stream, n);
    stream.close();
    var u8 = new Uint8Array(str.length);
    for (var i = 0; i < str.length; i++) u8[i] = str.charCodeAt(i) & 0xff;
    return { contentType: ch.contentType, verify: await verifyBytes(u8, 'NetUtil') };
  });
  await attempt(M, 'base64 in script (loadSubScript + atob)', async function () {
    var s = {};
    Services.scriptloader.loadSubScript(CHROME_BASE + 'probe_core.wasm.b64.js', s);
    var bin = atob(s.ZF_PROBE_WASM_B64);
    var u8 = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    var v = await verifyBytes(u8, 'base64');
    v.expectedSha256 = s.ZF_PROBE_WASM_SHA256;
    v.shaMatches = v.sha256 === s.ZF_PROBE_WASM_SHA256;
    return v;
  });
  L.anyMethodWorked = Object.keys(M).some(function (k) { return M[k].ok; });
}

function probeWorker(Ctor, label) {
  return new Promise(function (resolve, reject) {
    var w;
    var timer = setTimeout(function () {
      try { w && w.terminate(); } catch (e) {}
      reject(new Error('worker timeout 20s'));
    }, 20000);
    try {
      w = new Ctor(CHROME_BASE + 'probe-worker.js');
    } catch (e) {
      clearTimeout(timer);
      reject(e);
      return;
    }
    w.onmessage = function (ev) {
      clearTimeout(timer);
      var d = ev.data || {};
      var out = {
        constructor: label,
        importScriptsError: d.importScriptsError,
        testsError: d.testsError,
        fetchChromeInWorker: d.fetchChromeInWorker,
        instantiateStreamingInWorker: d.instantiateStreamingInWorker
      };
      try { out.tests = d.tests ? JSON.parse(d.tests) : null; } catch (e) { out.testsParseError = errStr(e); }
      try { w.terminate(); } catch (e) {}
      resolve(out);
    };
    w.onerror = function (ev) {
      clearTimeout(timer);
      try { ev.preventDefault(); } catch (e) {}
      reject(new Error('worker onerror: ' + (ev.message || ev) + ' ' + (ev.filename || '') + ':' + (ev.lineno || '')));
    };
    w.postMessage({ tiny: ZFP.tiny, rust: ZFP.rust, label: label });
  });
}

async function probeMainWindow(W, win, source) {
  W.source = source;
  try {
    W.info = {
      href: String(win.location.href),
      readyState: win.document.readyState,
      title: win.document.title,
      typeofZoteroPane: typeof win.ZoteroPane,
      typeofWebAssembly: typeof win.WebAssembly,
      innerWidth: win.innerWidth,
      innerHeight: win.innerHeight,
      itemsViewExists: !!(win.ZoteroPane && win.ZoteroPane.itemsView),
      itemContextMenuExists: !!win.document.getElementById('zotero-itemmenu')
    };
  } catch (e) { W.infoError = errStr(e); }
  try {
    var csp = win.document.csp;
    W.csp = csp ? { policyCount: csp.policyCount, policies: Array.from({ length: csp.policyCount }, function (_, i) { return csp.getPolicy(i); }) } : null;
  } catch (e) { W.cspError = errStr(e); }
  await attempt(W, 'loadSubScriptIntoWindow', function () {
    Services.scriptloader.loadSubScript(CHROME_BASE + 'wasm-tests.js', win);
    return typeof win.zfRunWasmTests;
  });
  await attempt(W, 'wasmTests', async function () {
    return JSON.parse(await win.zfRunWasmTests(ZFP.tiny, ZFP.rust, 'main-window'));
  });
  await attempt(W, 'AbortControllerFromWindow', function () {
    var ac = new win.AbortController(); ac.abort(); return ac.signal.aborted;
  });
  try { delete win.zfRunWasmTests; delete win.zfDescribeGlobal; delete win.zfTinyAddModuleBytes; } catch (e) {}
}

// ---------------------------------------------------------------------------------------------
// Finish: write JSON, then quit
// ---------------------------------------------------------------------------------------------

async function finish(why) {
  if (!ZFP || ZFP.finished) return;
  ZFP.finished = true;
  try { clearTimeout(ZFP.watchdog); } catch (e) {}
  var R = ZFP.results;
  R.meta.finishReason = why;
  R.meta.finishedAt = new Date().toISOString();
  R.meta.elapsedMsSinceStartup = Date.now() - ZFP.t0;
  R.headless.onMainWindowLoadFiredByFinish = ZFP.mainWindowLoadEvents.length > 0;
  R.headless.onMainWindowLoadEventsByFinish = ZFP.mainWindowLoadEvents;
  R.log = ZFP.log;
  R.consoleMessages = ZFP.consoleMessages;
  try { Services.console.unregisterListener(ZFP.consoleListener); } catch (e) {}
  try { Services.obs.removeObserver(ZFP.consoleApiObserver, 'console-api-log-event'); } catch (e) {}

  var resultFile = getResultFile();
  R.meta.resultFile = resultFile;
  if (!resultFile) {
    Zotero.logError('[zfprobe] no result file configured (pref extensions.zotero-harness.resultFile / env ZOTERO_HARNESS_RESULT_FILE)');
    return;
  }
  try {
    var dbg = await Zotero.Debug.get();
    await IOUtils.writeUTF8(resultFile.replace(/\.json$/i, '') + '.zotero-debug.txt', dbg);
  } catch (e) { R.meta.debugDumpError = errStr(e); }
  try {
    await IOUtils.writeUTF8(resultFile, JSON.stringify(R, null, 2), { tmpPath: resultFile + '.tmp' });
  } catch (e) {
    Zotero.logError('[zfprobe] failed to write result: ' + errStr(e));
  }
  var quitPref = true;
  try { quitPref = Services.prefs.getBoolPref('extensions.zotero-harness.quitWhenDone', true); } catch (e) {}
  if (!quitPref) return;
  setTimeout(function () {
    try {
      var ok = Services.startup.quit(Components.interfaces.nsIAppStartup.eAttemptQuit);
      Zotero.debug('[zfprobe] eAttemptQuit returned ' + ok);
    } catch (e) { Zotero.debug('[zfprobe] eAttemptQuit threw ' + e); }
    setTimeout(function () {
      try { Services.startup.quit(Components.interfaces.nsIAppStartup.eForceQuit); } catch (e) {}
    }, 10000);
  }, 500);
}
