// ES module loaded with ChromeUtils.importESModule("chrome://zfprobe/content/probe-module.mjs").
// System ESMs share one privileged global; this checks what that global offers.

export const loadedAt = new Date().toISOString();

export function describe() {
  const scope = {};
  Services.scriptloader.loadSubScript("chrome://zfprobe/content/wasm-tests.js", scope);
  const g = scope.zfDescribeGlobal();
  g["import.meta.url"] = import.meta.url;
  return JSON.stringify(g);
}

export async function runWasm(tiny, rust, label) {
  const scope = {};
  Services.scriptloader.loadSubScript("chrome://zfprobe/content/wasm-tests.js", scope);
  return scope.zfRunWasmTests(tiny, rust, label || "esm-global");
}

// Direct check written in module code itself (not via loadSubScript).
export async function directCheck(rustBytes) {
  const out = { typeofWebAssembly: typeof WebAssembly };
  try {
    const own = new Uint8Array(rustBytes.length);
    own.set(Array.from(rustBytes));
    const { instance } = await WebAssembly.instantiate(own, {});
    out.add = instance.exports.zf_add(1, 2);
    out.ok = true;
  } catch (e) {
    out.ok = false;
    out.error = String(e);
  }
  try {
    const r = await fetch("chrome://zfprobe/content/probe_core.wasm");
    out.fetchChromeFromEsm = { ok: r.ok, status: r.status, bytes: (await r.arrayBuffer()).byteLength };
  } catch (e) {
    out.fetchChromeFromEsm = { error: String(e) };
  }
  return JSON.stringify(out);
}
