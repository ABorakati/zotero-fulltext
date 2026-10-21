// THROWAWAY Zotero only. Start a paywalled run, disable zotero-fulltext mid-run (as an uninstall/upgrade would), then
// report every window left open, with whether its document actually loaded.
const { AddonManager } = ChromeUtils.importESModule('resource://gre/modules/AddonManager.sys.mjs');
const win = Zotero.getMainWindow();
const ZP = win.ZoteroPane;
await ZP.itemsView.waitForLoad();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const snapshot = () => {
  const out = [];
  for (const w of Services.wm.getEnumerator(null)) {
    const d = w.document;
    out.push({ uri: String(d.documentURI), title: d.title, readyState: d.readyState, size: [w.innerWidth, w.innerHeight],
      text: (d.documentElement ? d.documentElement.textContent : '').replace(/\s+/g, ' ').trim().slice(0, 120) });
  }
  return out;
};
const ids = [];
for (const doi of ['10.1097/SLA.0000000000005603', '10.1016/S0140-6736(20)30183-5']) {
  const item = new Zotero.Item('journalArticle');
  item.setField('title', 'Disable test ' + doi);
  item.setField('DOI', doi);
  ids.push(await item.saveTx());
}
await ZP.selectItems(ids);
await ZP.buildItemContextMenu();
const el = win.document.getElementById('zotero-itemmenu').querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
if (!el) return { error: 'menuitem not found' };
el.doCommand();
await sleep(20000);
const before = snapshot();
const addon = await AddonManager.getAddonByID('zotero-fulltext@aborakati');
await addon.disable();
await sleep(5000);
const afterDisable = snapshot();
await addon.enable();
await sleep(8000);
const afterEnable = snapshot();
return { before, afterDisable, afterEnable };
