/* zotero-fulltext bootstrap.js: thin loader. All behaviour lives in content/plugin.js (ZFT.plugin) so it is testable.
 *
 * Verified against Zotero 10.0.6 (xpcom/plugins.js): hooks are called as func.call(sandbox, {id, version, rootURI}, reason)
 * and returned promises are awaited; the sandbox is REUSED on disable -> enable, so startup()/shutdown() are idempotent;
 * onMainWindowLoad does not fire for the initial window (ZFT.plugin.startup adds it itself); prefs.js is loaded by Zotero
 * (setDefaultPrefs, plugins.js:507) and locale/<loc>/*.ftl files are registered automatically (plugins.js:584), so neither is
 * handled here. Scripts are loaded from chrome:// only (loading from jar:/file: rootURI breaks on Firefox 153, Mozilla
 * bug 1974213).
 */
/* global Zotero, Services, Cc, Ci */

var zftChromeHandle = null;
var zftStarted = false;

function install() {}

function uninstall() {}

async function startup({ id, version, rootURI }, reason) {
  if (zftStarted) {
    return;
  }
  zftStarted = true;
  try {
    await Zotero.initializationPromise;

    const aomStartup = Cc['@mozilla.org/addons/addon-manager-startup;1'].getService(Ci.amIAddonManagerStartup);
    zftChromeHandle = aomStartup.registerChrome(Services.io.newURI(rootURI + 'manifest.json'), [
      ['content', 'zotero-fulltext', rootURI + 'content/'],
    ]);

    // Every script is a classic script loaded into THIS sandbox global; each attaches one module to the shared ZFT namespace.
    const scope = this;
    const base = 'chrome://zotero-fulltext/content/';
    Services.scriptloader.loadSubScript(base + 'namespace.js', scope);
    for (const file of scope.ZFT.LOAD_ORDER) {
      Services.scriptloader.loadSubScript(base + file, scope);
    }

    await scope.ZFT.plugin.startup({ id, version, rootURI }, reason);
  }
  catch (e) {
    zftStarted = false;
    Zotero.logError(e);
    throw e;
  }
}

async function shutdown({ id, version, rootURI }, reason) {
  if (!zftStarted) {
    return;
  }
  zftStarted = false;
  try {
    if (typeof ZFT !== 'undefined' && ZFT.plugin) {
      await ZFT.plugin.shutdown(reason);
    }
  }
  catch (e) {
    Zotero.logError(e);
  }
  if (zftChromeHandle) {
    zftChromeHandle.destruct();
    zftChromeHandle = null;
  }
  // Drop the namespace so a re-enable starts from a clean sandbox global.
  try { delete this.ZFT; } catch (e) { this.ZFT = undefined; }
}

function onMainWindowLoad(event) {
  if (typeof ZFT !== 'undefined' && ZFT.plugin) {
    ZFT.plugin.onMainWindowLoad(event);
  }
}

function onMainWindowUnload(event) {
  if (typeof ZFT !== 'undefined' && ZFT.plugin) {
    ZFT.plugin.onMainWindowUnload(event);
  }
}
