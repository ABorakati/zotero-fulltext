/* zotero-fulltext: namespace root.
 *
 * Loaded FIRST, as a classic script, by bootstrap.js (Services.scriptloader.loadSubScript into the
 * plugin scope = the bootstrap sandbox global) and by tests/js/helpers/zft-env.mjs (vm context).
 * Every other content/*.js file is a classic script too: an IIFE that attaches ONE object to the
 * shared ZFT namespace (ZFT.util, ZFT.net, ...). No top-level let/const/class (Firefox ESR 153
 * subscript rules), no import/export, no ES modules.
 *
 * Rules every file follows:
 *  - Platform globals (Zotero, Services, IOUtils, PathUtils, ChromeUtils, Cc, Ci, Localization,
 *    WebAssembly, ChromeWorker, setTimeout ...) are resolved at CALL time, never captured at load
 *    time, so tests can swap fakes onto the vm context.
 *  - No AbortController / console / structuredClone (missing in the plugin sandbox): use
 *    ZFT.util.CancelScope and ZFT.util.log.
 *  - Files load in ZFT.LOAD_ORDER; a file may use only modules listed before it at LOAD time, and
 *    any module at CALL time.
 */
(function (global) {
  'use strict';

  const ZFT = {
    /** Plugin id; must equal manifest.json applications.zotero.id. */
    ID: 'zotero-fulltext@aborakati',
    /** Chrome package name registered by bootstrap.js: chrome://zotero-fulltext/content/. */
    CHROME_PACKAGE: 'zotero-fulltext',
    CONTENT_URL: 'chrome://zotero-fulltext/content/',
    /** Fluent resource id (file name in locale/en-US). */
    FTL: 'zotero-fulltext.ftl',
    /** All prefs live under this branch; keys in ZFT.config.DEFAULTS are relative to it. */
    PREF_BRANCH: 'extensions.zotero-fulltext.',
    /** JS<->engine protocol version (docs/design/contracts/proto.rs PROTOCOL). */
    PROTOCOL: 1,
    /** Set by ZFT.plugin.startup(): {id, version, rootURI}. */
    info: null,
    /**
     * Scripts loaded after this one, in order. Window scripts (prefs-pane.js, login.js,
     * progress-window.js) and inspect-worker.js are NOT in this list: they run in their own
     * global and only talk to the modules below through objects handed to them.
     */
    LOAD_ORDER: [
      'util.js',
      'config.js',
      'state.js',
      'bodies.js',
      'wasm-host.js',
      'net.js',
      'pdf-capture.js',
      'browser.js',
      'translators.js',
      'zotero-oa.js',
      'items.js',
      'save.js',
      'confirm.js',
      'progress.js',
      'login-controller.js',
      'effects.js',
      'runs.js',
      'menu.js',
      'harness.js',
      'plugin.js',
    ],
  };

  global.ZFT = ZFT;
})(this);
