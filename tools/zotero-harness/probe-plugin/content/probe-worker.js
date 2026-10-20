/* ChromeWorker / Worker probe. Loaded as chrome://zfprobe/content/probe-worker.js */
/* global zfRunWasmTests */
var importErr = null;
try {
  importScripts("chrome://zfprobe/content/wasm-tests.js");
} catch (e) {
  importErr = String(e);
}

onmessage = async function (ev) {
  var data = ev.data || {};
  var reply = { importScriptsError: importErr };
  try {
    reply.tests = importErr ? null : await zfRunWasmTests(data.tiny, data.rust, data.label || "worker");
  } catch (e) {
    reply.testsError = String(e);
  }
  // Can the worker load the wasm itself?
  try {
    var r = await fetch("chrome://zfprobe/content/probe_core.wasm");
    var buf = await r.arrayBuffer();
    var inst = await WebAssembly.instantiate(new Uint8Array(buf), {});
    reply.fetchChromeInWorker = { ok: r.ok, status: r.status, bytes: buf.byteLength, add: inst.instance.exports.zf_add(5, 6) };
  } catch (e) {
    reply.fetchChromeInWorker = { error: String(e) };
  }
  try {
    reply.instantiateStreamingInWorker = await (async function () {
      var res = await WebAssembly.instantiateStreaming(fetch("chrome://zfprobe/content/probe_core.wasm"), {});
      return { ok: true, add: res.instance.exports.zf_add(7, 8) };
    })();
  } catch (e) {
    reply.instantiateStreamingInWorker = { ok: false, error: String(e) };
  }
  postMessage(reply);
};
