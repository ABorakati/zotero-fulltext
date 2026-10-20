// Test environment: load the REAL plugin/content/*.js classic scripts into a vm context with fakes for the
// Zotero/Gecko globals. No npm dependencies. Owner: js-shell (everyone imports it; additive changes only).
//
//   const { ZFT, ctx } = loadPlugin({ globals: { Zotero: makeFakeZotero() } });
//
// Cross-realm note: objects created by plugin code come from the vm context's intrinsics. Byte types, URL, TextEncoder,
// WebAssembly and timers are injected from the host so bytes cross the boundary unchanged. Plain objects/arrays/errors
// from the plugin are NOT deepStrictEqual-comparable with host literals: compare with plain(x) (JSON round trip).
import vm from 'node:vm';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
export const PLUGIN = path.join(ROOT, 'plugin');
export const CONTENT = path.join(PLUGIN, 'content');
export const WASM_PATH = path.join(CONTENT, 'fulltext.wasm');

/** @returns {boolean} true when the wasm build output exists (tests that need it skip otherwise) */
export function wasmBuilt() {
  return fs.existsSync(WASM_PATH);
}

/** JSON round trip: turns cross-realm objects into host-realm ones for assert.deepStrictEqual. */
export function plain(x) {
  return x === undefined ? undefined : JSON.parse(JSON.stringify(x));
}

/** Create the vm context (not yet loaded). `globals` override/extend the defaults (e.g. Zotero, IOUtils, Services). */
export function makeContext(globals = {}) {
  const sandbox = {
    Uint8Array, ArrayBuffer, DataView, Uint32Array, Int32Array, BigInt, TextEncoder, TextDecoder,
    URL, URLSearchParams, WebAssembly, Blob, performance,
    setTimeout, clearTimeout, setInterval, clearInterval,
    crypto: globalThis.crypto,
    ...globals,
  };
  return vm.createContext(sandbox);
}

/** Run one plugin/content script in the context (classic script, `this` = context global). */
export function runScript(ctx, relPath, baseDir = CONTENT) {
  const file = path.join(baseDir, relPath);
  const src = fs.readFileSync(file, 'utf8');
  return new vm.Script(src, { filename: file }).runInContext(ctx);
}

/**
 * Load namespace.js plus ZFT.LOAD_ORDER (or `files`) into a fresh context.
 * @param {{globals?: object, files?: string[]}} [opts] files: subset to load (namespace.js is always first)
 * @returns {{ctx: object, ZFT: object}}
 */
export function loadPlugin(opts = {}) {
  const ctx = makeContext(opts.globals || {});
  runScript(ctx, 'namespace.js');
  const order = opts.files || ctx.ZFT.LOAD_ORDER;
  for (const f of order) runScript(ctx, f);
  return { ctx, ZFT: ctx.ZFT };
}

/** Parse plugin/prefs.js (pref(name, value) calls) into a Map. */
export function readPrefsJs() {
  const prefs = new Map();
  const ctx = vm.createContext({ pref: (k, v) => prefs.set(k, v) });
  new vm.Script(fs.readFileSync(path.join(PLUGIN, 'prefs.js'), 'utf8'), { filename: 'prefs.js' }).runInContext(ctx);
  return prefs;
}

/** All files under dir (recursive) whose name matches re. */
export function walk(dir, re, out = []) {
  if (!fs.existsSync(dir)) return out;
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (e.name === 'node_modules' || e.name === 'target' || e.name === '.git') continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, re, out);
    else if (re.test(e.name)) out.push(p);
  }
  return out;
}
