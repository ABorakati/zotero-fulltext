// THROWAWAY Zotero only. Start a run, then install dist/zotero-fulltext-0.1.0.xpi over the running plugin (the user's
// "Install Plugin From File" path) and report windows + add-on state afterwards.
const { AddonManager } = ChromeUtils.importESModule('resource://gre/modules/AddonManager.sys.mjs');
const { FileUtils } = ChromeUtils.importESModule('resource://gre/modules/FileUtils.sys.mjs');
const XPI = 'C:\\Users\\abora\\zotero-fulltext\\dist\\zotero-fulltext-0.1.0.xpi';
const win = Zotero.getMainWindow();
const ZP = win.ZoteroPane;
await ZP.itemsView.waitForLoad();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const snapshot = () => Array.from(Services.wm.getEnumerator(null)).map((w) => {
  const d = w.document;
  return { uri: String(d.documentURI), title: d.title, readyState: d.readyState, size: [w.innerWidth, w.innerHeight],
    text: (d.documentElement ? d.documentElement.textContent : '').replace(/\s+/g, ' ').trim().slice(0, 100) };
});
const opened = [];
const obs = { observe(s, t) { if (t === 'domwindowopened') opened.push(new Error().stack.split('\n').slice(0, 12).join(' | ')); } };
Services.ww.registerNotification(obs);
const item = new Zotero.Item('journalArticle');
item.setField('title', 'Reinstall test');
item.setField('DOI', '10.1097/SLA.0000000000005603');
const id = await item.saveTx();
await ZP.selectItems([id]);
await ZP.buildItemContextMenu();
const el = win.document.getElementById('zotero-itemmenu').querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
if (!el) return { error: 'menuitem not found', windows: snapshot() };
el.doCommand();
await sleep(15000);
const before = snapshot();
const install = await AddonManager.getInstallForFile(new FileUtils.File(XPI));
await install.install();
await sleep(8000);
const after = snapshot();
const addon = await AddonManager.getAddonByID('zotero-fulltext@aborakati');
Services.ww.unregisterNotification(obs);
await ZP.buildItemContextMenu();
const menuAfter = !!win.document.getElementById('zotero-itemmenu').querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
return { before, after, opened, addon: addon && { version: addon.version, active: addon.isActive, pending: addon.pendingOperations }, menuAfter };
