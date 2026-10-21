// Minimal WebDriver BiDi client for a THROWAWAY Zotero started by bidi-probe.ps1 (Node 24, no dependencies).
// Usage: node bidi-client.mjs [port] [expression]   -> evaluates `expression` in Zotero's main chrome window.
const port = Number(process.argv[2] || 9333);
const expression = process.argv[3] || 'JSON.stringify({version: Zotero.version, plugins: Object.keys(Zotero.Plugins?._addonIDs || {}).length, title: document.title})';

const ws = new WebSocket(`ws://127.0.0.1:${port}/session`);
let id = 0;
const pending = new Map();
function send(method, params = {}) {
  const msg = { id: ++id, method, params };
  return new Promise((resolve, reject) => {
    pending.set(msg.id, { resolve, reject });
    ws.send(JSON.stringify(msg));
  });
}
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  const p = m.id && pending.get(m.id);
  if (!p) return;
  pending.delete(m.id);
  if (m.type === 'error') p.reject(new Error(`${m.error}: ${m.message}`));
  else p.resolve(m.result);
};
ws.onerror = (e) => { console.error('ws error', e.message || e); process.exit(2); };
ws.onopen = async () => {
  const t0 = Date.now();
  try {
    await send('session.new', { capabilities: {} });
    const tree = await send('browsingContext.getTree', { 'moz:scope': 'chrome' });
    const ctxs = tree.contexts.map((c) => ({ context: c.context, url: c.url }));
    console.log('chrome contexts:', JSON.stringify(ctxs));
    const main = ctxs.find((c) => /zoteroPane\.xhtml|zotero\.xhtml/.test(c.url)) || ctxs[0];
    const r = await send('script.evaluate', { expression, target: { context: main.context }, awaitPromise: true });
    console.log('result:', JSON.stringify(r.result));
    console.log(`round trip ${Date.now() - t0} ms`);
    await send('session.end');
  } catch (e) {
    console.error('FAILED:', e.message);
    process.exitCode = 1;
  }
  ws.close();
};
