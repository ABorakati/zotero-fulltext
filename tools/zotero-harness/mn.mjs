// Marionette client for a THROWAWAY Zotero started by bidi-probe.ps1 (-marionette, port 2829). Node 24, no deps.
// Usage: node tools/zotero-harness/mn.mjs <script.js | -e "code"> [--port 2829] [--timeout 120]
// The code runs as the body of an async function in Zotero's main chrome window (Zotero, Services, window,
// document in scope); its return value is printed as JSON. Unhandled prompts are dismissed and reported.
import net from 'node:net';
import fs from 'node:fs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => { const i = argv.indexOf(name); return i >= 0 ? argv.splice(i, 2)[1] : dflt; };
const port = Number(opt('--port', 2829));
const timeoutSec = Number(opt('--timeout', 120));
const code = argv[0] === '-e' ? argv[1] : fs.readFileSync(argv[0], 'utf8');

const sock = net.connect(port, '127.0.0.1');
let buf = Buffer.alloc(0);
const waiters = [];
let id = 0;
sock.on('data', (d) => {
  buf = Buffer.concat([buf, d]);
  for (;;) {
    const colon = buf.indexOf(58); // ':'
    if (colon < 0) return;
    const n = Number(buf.subarray(0, colon).toString());
    if (buf.length < colon + 1 + n) return;
    const msg = JSON.parse(buf.subarray(colon + 1, colon + 1 + n).toString('utf8'));
    buf = buf.subarray(colon + 1 + n);
    const w = waiters.shift();
    if (w) w(msg);
  }
});
const next = () => new Promise((resolve) => waiters.push(resolve));
async function cmd(name, params = {}) {
  const m = JSON.stringify([0, ++id, name, params]);
  sock.write(`${Buffer.byteLength(m)}:${m}`);
  const r = await next();
  if (r[2]) throw new Error(`${r[2].error}: ${r[2].message}`);
  return r[3];
}

sock.on('error', (e) => { console.error('cannot connect:', e.message); process.exit(2); });
sock.on('connect', async () => {
  const timer = setTimeout(() => { console.error(`timeout after ${timeoutSec}s`); process.exit(3); }, timeoutSec * 1000);
  let code_ = 0;
  try {
    await next(); // hello
    await cmd('WebDriver:NewSession', { capabilities: { unhandledPromptBehavior: 'dismiss and notify' } });
    await cmd('Marionette:SetContext', { value: 'chrome' });
    await cmd('WebDriver:SetTimeouts', { script: timeoutSec * 1000 });
    const wrapped = `return (async () => { ${code}\n })().then(v => JSON.stringify(v === undefined ? null : v), e => JSON.stringify({ error: String(e), stack: e && e.stack }));`;
    const r = await cmd('WebDriver:ExecuteScript', { script: wrapped, args: [] });
    console.log(r.value);
  } catch (e) {
    console.error('FAILED:', e.message);
    code_ = 1;
  } finally {
    try { await cmd('WebDriver:DeleteSession'); } catch (e) { /* session already gone */ }
    clearTimeout(timer);
    sock.end();
    process.exit(code_);
  }
});
