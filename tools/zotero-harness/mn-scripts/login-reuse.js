// THROWAWAY Zotero only. Drives ZFT.login.open() directly with a fake page classifier (host.loginState lands on example.com /
// example.org) to prove ONE sign-in window is reused across routes and closed after the 15 s grace. Never types into a page.
// Run: node tools/zotero-harness/mn.mjs tools/zotero-harness/mn-scripts/login-reuse.js --timeout 120
const ZFT = Zotero.ZoteroFullText;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
ZFT.login.closeAll();
await sleep(1500);
const t0 = Date.now();
const timeline = [];
const tags = new WeakMap();
let n = 0;
const seen = new Map();
function sample() {
  const live = new Set();
  for (const w of Services.wm.getEnumerator('zotero-fulltext:login')) {
    if (!tags.has(w)) tags.set(w, 'login#' + (++n));
    const tag = tags.get(w);
    live.add(tag);
    let url = '';
    try { url = w.document.getElementById('zft-login-browser').currentURI.spec; } catch (e) { url = '(none)'; }
    const hint = (w.document.getElementById('zft-login-hint') || {}).textContent || '';
    const key = `${tag} [${w.document.title}] ${url} hint="${hint}"`;
    if (seen.get(tag) !== key) { seen.set(tag, key); timeline.push(`${((Date.now() - t0) / 1000).toFixed(1)}s ${key}`); }
  }
  for (const tag of Array.from(seen.keys())) {
    if (!live.has(tag)) { seen.delete(tag); timeline.push(`${((Date.now() - t0) / 1000).toFixed(1)}s ${tag} CLOSED`); }
  }
}
const sampler = setInterval(sample, 250);
const host = {
  loginState: async ({ url }) => (/example\.(com|org)/.test(url) ? { state: 'landed' } : { state: 'pending' }),
  inspect: async () => ({ expect: 'none' }),
};
const bodies = { putMemory: async (bytes) => ({ handle: 1, len: bytes.length }) };
const mk = (entry, realm) => ({ kind: 'login', realm, entry_url: entry, item_title: 'reuse probe', waiting: 1, timeout_ms: 60000, capture_pdf: false });
const results = [];
try {
  const r1 = await ZFT.login.open({ op: 9001, run: 9001, req: mk('https://example.com/', 'openathens') }, { scope: new ZFT.util.CancelScope(), host, bodies });
  results.push(['route1', r1.result, r1.final_url]);
  await sleep(4000);
  const r2 = await ZFT.login.open({ op: 9002, run: 9001, req: mk('https://example.org/', 'saml') }, { scope: new ZFT.util.CancelScope(), host, bodies });
  results.push(['route2', r2.result, r2.final_url]);
  await sleep(18000);
} finally {
  clearInterval(sampler);
  sample();
}
return { timeline, results, windowsLeft: Array.from(Services.wm.getEnumerator('zotero-fulltext:login')).length, standby: ZFT.login.hasStandby() };
