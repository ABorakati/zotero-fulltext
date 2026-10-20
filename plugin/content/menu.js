/* ZFT.menu: item context-menu and Tools-menu entries (Zotero.MenuManager).
 *
 * Owner: js-shell. Verified in xpcom/pluginAPI/menuManager.js (Zotero 10.0.6):
 *   Zotero.MenuManager.registerMenu({menuID, pluginID, target, menus: [{menuType: 'menuitem'|'separator'|'submenu',
 *   l10nID, onShowing(event, context), onCommand(event, context), ...}]}) -> key | false;
 *   unregisterMenu(key). The key is the plugin-namespaced menuID, so unregister must use the RETURNED key, not the
 *   menuID. context has setVisible(bool), setEnabled(bool), setL10nArgs(str) and, for target 'main/library/item',
 *   context.items (the selection, captured when the menu was built). Hooks are wrapped by Zotero in a synchronous
 *   try/catch only: an async onCommand that rejects would be silent, so every handler here handles its own errors.
 *   Registrations are removed automatically on shutdown, but unregister explicitly anyway (idempotent).
 * Entries:
 *   main/library/item  menuitem  zotero-fulltext-menu-get      Mode 'full'     (OA sources, then the institution)
 *   main/library/item  menuitem  zotero-fulltext-menu-get-oa   Mode 'oa_only'  (never touches the institution)
 *     onShowing: setVisible(ZFT.items.menuEligible(context.items)) and nothing else (synchronous, no DB reads).
 *     onCommand: ZFT.runs.startRun({items: context.items, mode, interactive: true}); a failure is shown once with
 *     Services.prompt.alert (title zotero-fulltext-prefs-title, text zotero-fulltext-notice-start-failed {reason}); the
 *     handler never throws and returns a promise that never rejects (Zotero ignores it, tests await it).
 *   main/menubar/tools separator, then three menuitems:
 *     zotero-fulltext-menu-signin    -> ZFT.login.warmUp(ZFT.config.buildEngineConfig())  (enabled only while inst.enabled)
 *     zotero-fulltext-menu-progress  -> ZFT.progress.show()
 *     zotero-fulltext-menu-prefs     -> Zotero.Utilities.Internal.openPreferences(ZFT.plugin.paneID)
 * Labels come from the FTL .label attributes. The library item menu is a grouped target: keep to two entries.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const MENU_IDS = { item: 'zft-item-menu', tools: 'zft-tools-menu' };
  const TARGETS = { item: 'main/library/item', tools: 'main/menubar/tools' };
  const L10N_ID = /^zotero-fulltext-[a-z0-9-]+$/;

  /** Keys returned by Zotero.MenuManager.registerMenu for the live registrations. */
  let keys = [];

  /** @returns {string[]} the registration keys currently held (a copy) */
  function registeredKeys() {
    return keys.slice();
  }

  /**
   * The reason text for notice-start-failed. runs.startRun throws errors whose message is an l10n id; those are localised.
   * The one exception is the start-failed id itself (the item-count sanity cap), whose own reason travels in `error.reason`.
   */
  function reasonOf(e) {
    const message = e && e.message !== undefined ? String(e.message) : String(e);
    if (message === 'zotero-fulltext-notice-start-failed' && e && e.reason) return String(e.reason);
    return L10N_ID.test(message) ? ZFT.l10n.format(message) : message;
  }

  /** Show the start-failed alert. Never throws. */
  function reportFailure(e) {
    ZFT.util.log('error', 'menu action failed: ' + ZFT.util.errText(e));
    try {
      const title = ZFT.l10n.format('zotero-fulltext-prefs-title');
      const text = ZFT.l10n.format('zotero-fulltext-notice-start-failed', { reason: reasonOf(e) });
      let win = null;
      try { win = typeof Zotero.getMainWindow === 'function' ? Zotero.getMainWindow() : null; } catch (err) { win = null; }
      Services.prompt.alert(win || null, title, text);
    } catch (err) {
      ZFT.util.log('error', 'could not show the failure alert: ' + ZFT.util.errText(err));
    }
  }

  /**
   * Run `fn` now; a synchronous throw or a rejected promise goes to reportFailure.
   * @returns {Promise<void>} never rejects
   */
  function guarded(fn) {
    let result;
    try {
      result = fn();
    } catch (e) {
      reportFailure(e);
      return Promise.resolve();
    }
    return Promise.resolve(result).then(() => undefined, (e) => { reportFailure(e); });
  }

  /** @param {'full'|'oa_only'} mode @returns {(event: object, context: object) => Promise<void>} the onCommand handler for an item menu entry */
  function commandFor(mode) {
    return function onCommand(event, context) {
      return guarded(() => {
        const items = context && context.items ? Array.from(context.items) : [];
        if (!items.length) throw new Error('zotero-fulltext-notice-no-eligible');
        return ZFT.runs.startRun({ items, mode, interactive: true });
      });
    };
  }

  /** onShowing handler for item menu entries: visible when the selection has an eligible item. Synchronous. */
  function onShowing(event, context) {
    let visible = false;
    try {
      visible = !!ZFT.items.menuEligible(context && context.items ? context.items : []);
    } catch (e) {
      ZFT.util.log('warn', 'menuEligible failed: ' + ZFT.util.errText(e));
    }
    try { context.setVisible(visible); } catch (e) { ZFT.util.log('warn', 'setVisible failed: ' + ZFT.util.errText(e)); }
  }

  /** onShowing of the sign-in entry: only meaningful while institutional access is on. */
  function onShowingSignin(event, context) {
    try { context.setEnabled(!!ZFT.config.getPref('inst.enabled')); } catch (e) { ZFT.util.log('warn', 'setEnabled failed: ' + ZFT.util.errText(e)); }
  }

  function toolsMenus() {
    return [
      { menuType: 'separator' },
      {
        menuType: 'menuitem', l10nID: 'zotero-fulltext-menu-signin', onShowing: onShowingSignin,
        onCommand: () => guarded(() => ZFT.login.warmUp(ZFT.config.buildEngineConfig())),
      },
      {
        menuType: 'menuitem', l10nID: 'zotero-fulltext-menu-progress',
        onCommand: () => guarded(() => ZFT.progress.show()),
      },
      {
        menuType: 'menuitem', l10nID: 'zotero-fulltext-menu-prefs',
        onCommand: () => guarded(() => Zotero.Utilities.Internal.openPreferences(ZFT.plugin.paneID || undefined)),
      },
    ];
  }

  /** Register all menus. Idempotent (a second call replaces the first). @returns {string[]} registration keys */
  function register() {
    unregister();
    const manager = Zotero.MenuManager;
    if (!manager || typeof manager.registerMenu !== 'function') {
      ZFT.util.log('error', 'Zotero.MenuManager is not available; menus were not registered');
      return [];
    }
    const defs = [
      {
        menuID: MENU_IDS.item, pluginID: ZFT.ID, target: TARGETS.item,
        menus: [
          { menuType: 'menuitem', l10nID: 'zotero-fulltext-menu-get', onShowing, onCommand: commandFor('full') },
          { menuType: 'menuitem', l10nID: 'zotero-fulltext-menu-get-oa', onShowing, onCommand: commandFor('oa_only') },
        ],
      },
      { menuID: MENU_IDS.tools, pluginID: ZFT.ID, target: TARGETS.tools, menus: toolsMenus() },
    ];
    for (const def of defs) {
      let key = false;
      try { key = manager.registerMenu(def); } catch (e) { ZFT.util.log('error', `registerMenu ${def.menuID} failed: ${ZFT.util.errText(e)}`); }
      if (key) keys.push(key);
      else ZFT.util.log('warn', `registerMenu ${def.menuID} was refused`);
    }
    return keys.slice();
  }

  /** Unregister everything registered here. Idempotent. */
  function unregister() {
    const held = keys;
    keys = [];
    for (const key of held) {
      try { Zotero.MenuManager.unregisterMenu(key); } catch (e) { ZFT.util.log('warn', 'unregisterMenu failed: ' + ZFT.util.errText(e)); }
    }
  }

  ZFT.menu = { MENU_IDS, TARGETS, register, unregister, registeredKeys, commandFor, onShowing };
})(this);
