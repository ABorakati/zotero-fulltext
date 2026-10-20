/* Shared WebAssembly probe. Plain classic script so it can be loaded into:
 *   - the plugin bootstrap sandbox      (Services.scriptloader.loadSubScript(url, scope))
 *   - the Zotero main window global      (Services.scriptloader.loadSubScript(url, window))
 *   - the shared system ESM global       (loadSubScript into an object created by an .mjs)
 *   - a ChromeWorker / Worker            (importScripts(url))
 * It always returns a JSON *string* so results cross compartment/thread boundaries cleanly.
 */
/* exported zfRunWasmTests, zfDescribeGlobal, zfTinyAddModuleBytes */

// Hand-written module: (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add)
var zfTinyAddModuleBytes = function () {
  return new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version 1
    0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, // type: (i32,i32)->i32
    0x03, 0x02, 0x01, 0x00, // function section: 1 func of type 0
    0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, // export "add" func 0
    0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b // code
  ]);
};

var zfDescribeGlobal = function () {
  var names = [
    'WebAssembly', 'fetch', 'Response', 'Request', 'Headers', 'TextEncoder', 'TextDecoder',
    'URL', 'URLSearchParams', 'AbortController', 'AbortSignal', 'crypto', 'DOMParser',
    'XMLHttpRequest', 'XMLSerializer', 'Blob', 'File', 'FileReader', 'atob', 'btoa',
    'structuredClone', 'queueMicrotask', 'setTimeout', 'requestIdleCallback',
    'SharedArrayBuffer', 'Atomics', 'BigInt', 'ReadableStream', 'WritableStream',
    'IOUtils', 'PathUtils', 'ChromeUtils', 'Services', 'Components', 'Cc', 'Ci', 'Cu',
    'Zotero', 'Worker', 'ChromeWorker', 'importScripts', 'postMessage', 'window',
    'document', 'globalThis', 'console', 'dump', 'Localization'
  ];
  var g = {};
  // No eval here: system-principal eval may be blocked (security.allow_eval_with_system_principal).
  for (var i = 0; i < names.length; i++) {
    var t;
    try { t = (names[i] in globalThis) ? typeof globalThis[names[i]] : 'undefined'; } catch (e) { t = 'throws:' + e; }
    g[names[i]] = t;
  }
  try { g['crypto.subtle'] = typeof crypto !== 'undefined' ? typeof crypto.subtle : 'n/a'; } catch (e) { g['crypto.subtle'] = 'throws:' + e; }
  try { g['globalThis.toString'] = Object.prototype.toString.call(globalThis); } catch (e) { g['globalThis.toString'] = 'throws:' + e; }
  try {
    if (typeof WebAssembly !== 'undefined') {
      g.WebAssemblyKeys = Object.getOwnPropertyNames(WebAssembly).sort().join(',');
    }
  } catch (e) { g.WebAssemblyKeys = 'throws:' + e; }
  return g;
};

var zfRunWasmTests = async function (tinyIn, rustIn, label, rustInput) {
  var out = { label: label, ranAt: new Date().toISOString(), global: zfDescribeGlobal(), steps: {} };

  function errStr(e) {
    if (!e) return String(e);
    var s = (e.name ? e.name + ': ' : '') + (e.message || String(e));
    if (e.fileName) s += ' @' + e.fileName + ':' + e.lineNumber;
    return s;
  }
  async function step(name, fn) {
    var t0 = Date.now();
    try {
      var v = await fn();
      out.steps[name] = { ok: true, ms: Date.now() - t0, value: v };
    }
    catch (e) {
      out.steps[name] = { ok: false, ms: Date.now() - t0, error: errStr(e) };
    }
  }
  // Copy into a typed array owned by *this* global (inputs may be cross-compartment wrappers
  // or structured clones).
  function own(src) {
    if (!src) return null;
    var a = new Uint8Array(src.length);
    for (var i = 0; i < src.length; i++) a[i] = src[i];
    return a;
  }
  function utf8Encode(s) {
    if (typeof TextEncoder !== 'undefined') return new TextEncoder().encode(s);
    var bin = unescape(encodeURIComponent(s));
    var a = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) a[i] = bin.charCodeAt(i);
    return a;
  }
  function utf8Decode(bytes) {
    if (typeof TextDecoder !== 'undefined') return new TextDecoder().decode(bytes);
    var bin = '';
    for (var i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return decodeURIComponent(escape(bin));
  }
  function callRust(instance, inputObj) {
    var e = instance.exports;
    var input = utf8Encode(JSON.stringify(inputObj));
    var p = e.zf_alloc(input.length);
    new Uint8Array(e.memory.buffer, p, input.length).set(input);
    var packed = e.zf_process_json(p, input.length);
    e.zf_dealloc(p, input.length);
    var packedType = typeof packed;
    var r = BigInt.asUintN(64, BigInt(packed));
    var op = Number(r & 0xffffffffn);
    var ol = Number(r >> 32n);
    // copy out before freeing (memory.buffer may be detached by growth)
    var outBytes = new Uint8Array(e.memory.buffer, op, ol).slice();
    e.zf_dealloc(op, ol);
    return {
      i64ReturnType: packedType,
      abi: e.zf_abi_version(),
      add: e.zf_add(40, 2),
      memoryBytes: e.memory.buffer.byteLength,
      output: JSON.parse(utf8Decode(outBytes))
    };
  }

  // eval / Function matter for JS glue code (e.g. some generated loaders); wasm compile is separate.
  await step('js.eval', function () { return eval('1 + 1'); });
  await step('js.newFunction', function () { return new Function('return 2 + 2')(); });

  out.typeofWebAssembly = typeof WebAssembly;
  if (typeof WebAssembly === 'undefined') {
    return JSON.stringify(out);
  }

  var tiny = own(tinyIn || zfTinyAddModuleBytes());
  var rust = own(rustIn);
  var sample = rustInput || { doi: ' https://doi.org/10.1000/ABC.def ', text: 'héllo wörld', numbers: [1, 2, 3.5] };

  await step('tiny.validate', function () { return WebAssembly.validate(tiny); });
  await step('tiny.syncModuleInstance', function () {
    var m = new WebAssembly.Module(tiny);
    var inst = new WebAssembly.Instance(m, {});
    return { add_2_40: inst.exports.add(2, 40), exports: WebAssembly.Module.exports(m).map(function (x) { return x.name + ':' + x.kind; }) };
  });
  await step('tiny.asyncInstantiate', async function () {
    var r = await WebAssembly.instantiate(tiny, {});
    return r.instance.exports.add(20, 22);
  });
  await step('tiny.asyncCompile', async function () {
    var m = await WebAssembly.compile(tiny);
    var inst = await WebAssembly.instantiate(m, {});
    return inst.exports.add(1, 1);
  });
  if (tinyIn) {
    await step('tiny.foreignBytesDirect', function () {
      // pass the caller's (possibly wrapped/foreign) typed array straight to WebAssembly
      var m = new WebAssembly.Module(tinyIn);
      return WebAssembly.Module.exports(m).length;
    });
  }
  await step('memoryGrow', function () {
    var mem = new WebAssembly.Memory({ initial: 1, maximum: 512 });
    var before = mem.buffer.byteLength;
    mem.grow(255);
    return { before: before, after: mem.buffer.byteLength };
  });
  if (rust) {
    await step('rust.validate', function () { return WebAssembly.validate(rust); });
    await step('rust.syncCompileInstantiateCall', function () {
      var t0 = Date.now();
      var m = new WebAssembly.Module(rust);
      var t1 = Date.now();
      var inst = new WebAssembly.Instance(m, {});
      var t2 = Date.now();
      var res = callRust(inst, sample);
      res.compileMs = t1 - t0;
      res.instantiateMs = t2 - t1;
      res.imports = WebAssembly.Module.imports(m).length;
      return res;
    });
    await step('rust.asyncInstantiateCall', async function () {
      var r = await WebAssembly.instantiate(rust, {});
      return callRust(r.instance, sample);
    });
    await step('rust.badJsonErrorPath', async function () {
      var r = await WebAssembly.instantiate(rust, {});
      var e = r.instance.exports;
      var input = utf8Encode('{not json');
      var p = e.zf_alloc(input.length);
      new Uint8Array(e.memory.buffer, p, input.length).set(input);
      var packed = BigInt.asUintN(64, BigInt(e.zf_process_json(p, input.length)));
      var op = Number(packed & 0xffffffffn), ol = Number(packed >> 32n);
      var s = utf8Decode(new Uint8Array(e.memory.buffer, op, ol).slice());
      e.zf_dealloc(op, ol);
      return JSON.parse(s);
    });
    await step('rust.manyCallsNoLeak', async function () {
      var r = await WebAssembly.instantiate(rust, {});
      var t0 = Date.now();
      for (var i = 0; i < 2000; i++) callRust(r.instance, sample);
      return { calls: 2000, ms: Date.now() - t0, memoryBytes: r.instance.exports.memory.buffer.byteLength };
    });
  }
  out.streamingApis = {
    instantiateStreaming: typeof WebAssembly.instantiateStreaming,
    compileStreaming: typeof WebAssembly.compileStreaming
  };
  return JSON.stringify(out);
};
