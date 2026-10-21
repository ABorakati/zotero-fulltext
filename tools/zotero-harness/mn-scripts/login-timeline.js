// THROWAWAY Zotero only. Run "Get Full Text" on one paywalled DOI and sample every login window's page once a second,
// to see which sign-in windows open, what they show and when they close. Never types into any page.
const DOI = globalThis.__DOI || '10.1111/nep.13250';
const SECONDS = 60;
const win = Zotero.getMainWindow();
const ZP = win.ZoteroPane;
await ZP.itemsView.waitForLoad();
const item = new Zotero.Item('journalArticle');
item.setField('title', 'Timeline ' + DOI);
item.setField('DOI', DOI);
const id = await item.saveTx();
await ZP.selectItems([id]);
await ZP.buildItemContextMenu();
win.document.getElementById('zotero-itemmenu').querySelector('[data-l10n-id="zotero-fulltext-menu-get"]').doCommand();
const seen = new Map();
const tag = new WeakMap();
let n = 0;
const timeline = [];
for (let s = 0; s < SECONDS; s++) {
  await new Promise((r) => setTimeout(r, 1000));
  const liveNow = new Set();
  for (const w of Services.wm.getEnumerator('zotero-fulltext:login')) {
    if (!tag.has(w)) tag.set(w, 'login#' + (++n));
    liveNow.add(tag.get(w));
    const b = w.document.getElementById('zft-login-browser');
    let url = '';
    try { url = b.currentURI.spec.replace(/\?.*/, '?…'); } catch (e) { url = '(none)'; }
    const hint = (w.document.getElementById('zft-login-hint') || {}).textContent || '';
    const key = tag.get(w) + ' [' + w.document.title + '] ' + url + ' ' + hint.slice(0, 60);
    if (seen.get(tag.get(w)) !== key) { seen.set(tag.get(w), key); timeline.push(`${s}s ${key}`); }
  }
  for (const t of Array.from(seen.keys())) {
    if (!liveNow.has(t)) { seen.delete(t); timeline.push(`${s}s ${t} CLOSED`); }
  }
}
const pw = Services.wm.getMostRecentWindow('zotero-fulltext:progress');
const a = pw && (pw.arguments[0].wrappedJSObject || pw.arguments[0]);
const diag = a ? (a.onCopy ? a.onCopy() : a.model.diagnostics()) : '';
return { timeline, trace: diag.split('\n').filter((l) => l.startsWith('1/') || l.startsWith('window')).join('\n') };
