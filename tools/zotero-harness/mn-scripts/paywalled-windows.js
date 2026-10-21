// Run with: node tools/zotero-harness/mn.mjs tools/zotero-harness/mn-scripts/paywalled-windows.js --timeout 180
// THROWAWAY Zotero only. Creates paywalled items, fires the real "Get Full Text" context-menu command with
// institutional access ON, then reports every window that opened (never types anything into them).
const DOIS = ['10.1111/cts.13276', '10.1097/SLA.0000000000005603', '10.1016/S0140-6736(20)30183-5'];
const WAIT_MS = 45000;
const win = Zotero.getMainWindow();
const ZP = win.ZoteroPane;
await ZP.itemsView.waitForLoad();

const opened = [];
const t0 = Date.now();
const observer = {
  observe(subject, topic) {
    if (topic !== 'domwindowopened') return;
    const w = subject;
    const rec = { atMs: Date.now() - t0, stack: new Error().stack.split('\n').slice(0, 25).join(' | ') };
    opened.push(rec);
    w.addEventListener('load', () => {
      rec.uri = String(w.document.documentURI);
      rec.windowtype = w.document.documentElement.getAttribute('windowtype');
    }, { once: true });
  },
};
Services.ww.registerNotification(observer);

const ids = [];
for (const doi of DOIS) {
  const item = new Zotero.Item('journalArticle');
  item.setField('title', 'Paywalled test ' + doi);
  item.setField('DOI', doi);
  ids.push(await item.saveTx());
}
await ZP.selectItems(ids);
await ZP.buildItemContextMenu();
const popup = win.document.getElementById('zotero-itemmenu');
let el = popup.querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
if (!el) {
  for (const sub of popup.querySelectorAll('menu > menupopup')) {
    sub.dispatchEvent(new win.Event('popupshowing'));
    el = sub.querySelector('[data-l10n-id="zotero-fulltext-menu-get"]');
    if (el) break;
  }
}
if (!el) return { error: 'menuitem not found' };
el.doCommand();
await new Promise((r) => setTimeout(r, WAIT_MS));
Services.ww.unregisterNotification(observer);

const windows = [];
for (const w of Services.wm.getEnumerator(null)) {
  const d = w.document;
  const info = {
    uri: String(d.documentURI), windowtype: d.documentElement.getAttribute('windowtype'), title: d.title,
    readyState: d.readyState, size: [w.innerWidth, w.innerHeight], hidden: d.documentElement.hidden,
    text: (d.documentElement.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 300),
  };
  const b = d.querySelector('browser');
  if (b) {
    try { info.browserURI = b.currentURI && b.currentURI.spec; } catch (e) { info.browserURI = 'ERR ' + e; }
    const r = b.getBoundingClientRect();
    info.browserRect = [Math.round(r.width), Math.round(r.height)];
  }
  windows.push(info);
}
return { opened, windows };
