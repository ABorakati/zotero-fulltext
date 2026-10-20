// Load wasm-bindgen 0.2.126 output the way a Zotero plugin would (bytes in, no fetch):
//  - web target: dynamic import() + initSync({ module: bytes })  (Beaver's pattern)
//  - no-modules target: evaluated as a classic script in a fresh global (vm), then initSync
import { readFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import vm from "node:vm";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(here, "../out");
const html = readFileSync(path.join(here, "../fixtures/frontiers.html"), "utf8");
const url = "https://www.frontiersin.org/journals/psychology/articles/10.3389/fpsyg.2013.00863/full";

function bench(label, fn) {
  const first = fn();
  const N = 20, t = performance.now();
  for (let i = 0; i < N; i++) fn();
  console.log(`${label}: ${((performance.now() - t) / N).toFixed(2)} ms/call  top=${JSON.parse(first)[0].url}`);
}

// web target
const web = await import(pathToFileURL(path.join(out, "bindgen-web/bindgen_demo.js")).href);
web.initSync({ module: readFileSync(path.join(out, "bindgen-web/bindgen_demo_bg.wasm")) });
bench("bindgen --target web       ", () => web.landing(html, url));
try { web.landing(html, "not a url"); } catch (e) { console.log("  JsError surfaced as:", e.constructor.name, e.message); }

// no-modules target in a bare global that only has what the Zotero plugin sandbox has
const ctx = vm.createContext({ TextEncoder, TextDecoder, URL, WebAssembly, console });
vm.runInContext(readFileSync(path.join(out, "bindgen-no-modules/bindgen_demo.js"), "utf8"), ctx);
console.log("  top-level `let wasm_bindgen` visible as a global property:", "wasm_bindgen" in ctx);
const wb = vm.runInContext("wasm_bindgen", ctx);
const wasmBytes = readFileSync(path.join(out, "bindgen-no-modules/bindgen_demo_bg.wasm"));
// 1) options object created in ANOTHER realm (like a Gecko cross-compartment wrapper)
try {
  wb.initSync({ module: wasmBytes });
} catch (e) {
  console.log("  cross-realm initSync({module}) ->", e.constructor.name + ":", e.message);
}
// 2) same call with the options object created inside the glue's own realm
ctx.__bytes = wasmBytes;
vm.runInContext("wasm_bindgen.initSync({ module: __bytes })", ctx);
bench("bindgen --target no-modules", () => wb.landing(html, url));
