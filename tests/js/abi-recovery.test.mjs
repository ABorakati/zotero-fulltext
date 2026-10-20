// Trap-recovery tests: what JS sees when the module panics, and how to recover.
// Owned by work package core-wasm-xtask. Needs the SEPARATE debug-panic build (an extra `fx_debug_panic` export,
// never shipped): `cargo xtask build --debug-panic` writes <target>/xtask/fulltext-debug.wasm and `cargo xtask test`
// builds it before running the JS tests. This file builds nothing itself; without the file every test is skipped
// with an explanation. Where the file is searched: $ZFT_DEBUG_WASM, $CARGO_TARGET_DIR/xtask/, <repo>/target/xtask/.
// Run: node --test tests/js/abi-recovery.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import v8 from 'node:v8';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(fileURLToPath(new URL('../..', import.meta.url)));
const PLUGIN_WASM = path.join(repoRoot, 'plugin', 'content', 'fulltext.wasm');
const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: true });

const candidates = [
  process.env.ZFT_DEBUG_WASM,
  process.env.CARGO_TARGET_DIR && path.join(process.env.CARGO_TARGET_DIR, 'xtask', 'fulltext-debug.wasm'),
  path.join(repoRoot, 'target', 'xtask', 'fulltext-debug.wasm'),
].filter(Boolean);
const debugWasm = candidates.find((p) => existsSync(p));
const skip = debugWasm
  ? false
  : `no debug-panic module found (looked at: ${candidates.join(', ')}); run "cargo xtask build --debug-panic" ` +
    '(cargo xtask test builds it first) to enable the trap-recovery tests';

// V8 only collects wasm memories on GC; the leak test needs to force it.
v8.setFlagsFromString('--expose-gc');
const gc = vm.runInNewContext('gc');

let compiled; // ONE compiled module, instantiated many times: this is how the host recovers
async function compiledModule() {
  compiled ??= await WebAssembly.compile(await readFile(debugWasm));
  return compiled;
}

async function instantiate() {
  const { exports: x } = await WebAssembly.instantiate(await compiledModule(), {});
  return x;
}

/** The text and the (ptr, len) of fx_last_panic(); `text` is '' when no panic was recorded. */
function lastPanic(x) {
  const packed = x.fx_last_panic();
  assert.equal(typeof packed, 'bigint');
  const ptr = Number(packed >> 32n);
  const len = Number(packed & 0xffffffffn);
  const text = len === 0 ? '' : dec.decode(new Uint8Array(x.memory.buffer, ptr, len));
  return { packed, ptr, len, text };
}

/** Run `fx_debug_panic(code)` expecting the trap; return the error. */
function trap(x, code) {
  try {
    x.fx_debug_panic(code);
  } catch (e) {
    return e;
  }
  assert.fail(`fx_debug_panic(${code}) returned instead of trapping`);
}

const PANICS = [
  { code: 1, text: 'deliberate panic' },
  { code: 2, text: 'index out of bounds' },
];

/** The message must name the source file but contain no absolute path of the machine that built the module. */
function assertNoBuilderPath(message) {
  assert.match(message, /abi\.rs/, 'the panic location is part of the message');
  assert.doesNotMatch(message, /[A-Za-z]:[\\/]/, `drive-letter path in: ${message}`);
  assert.doesNotMatch(message, /[\\/]Users[\\/]|[\\/]home[\\/]/i, `home directory in: ${message}`);
  for (const secret of [os.homedir(), repoRoot, process.env.USERPROFILE, process.env.HOME]) {
    if (secret && secret.length > 3) {
      assert.ok(!message.toLowerCase().includes(secret.toLowerCase()), `builder path ${secret} leaked into: ${message}`);
    }
  }
}

test('the debug module exports exactly the plugin ABI plus fx_debug_panic', { skip }, async () => {
  const names = (m) => WebAssembly.Module.exports(m).map((e) => e.name).sort();
  const debug = await compiledModule();
  assert.deepEqual(WebAssembly.Module.imports(debug), [], 'the debug module imports nothing either');
  assert.ok(names(debug).includes('fx_debug_panic'));
  if (existsSync(PLUGIN_WASM)) {
    const plugin = await WebAssembly.compile(await readFile(PLUGIN_WASM));
    assert.ok(!names(plugin).includes('fx_debug_panic'), 'the shipped module must not contain the test export');
    assert.deepEqual(names(debug).filter((n) => n !== 'fx_debug_panic'), names(plugin));
  }
});

test('non-panic codes return unchanged and record nothing', { skip }, async () => {
  const x = await instantiate();
  for (const code of [0, 4, 7, 12345]) assert.equal(x.fx_debug_panic(code), code);
  assert.equal(x.fx_debug_panic(0xffff_fffe) >>> 0, 0xffff_fffe);
  assert.equal(lastPanic(x).len, 0);
});

for (const { code, text } of PANICS) {
  test(`panic code ${code}: RuntimeError, readable fx_last_panic, poisoned instance, clean recovery`, { skip }, async (t) => {
    const x = await instantiate();
    assert.equal(lastPanic(x).len, 0, 'nothing recorded before the panic');

    // (a) the call throws WebAssembly.RuntimeError
    const error = trap(x, code);
    assert.ok(error instanceof WebAssembly.RuntimeError, `${error?.constructor?.name}: ${error?.message}`);
    t.diagnostic(`V8 reports the trap as: ${error.message}`);

    // (b) fx_last_panic() still works on the dead instance and names the panic without any builder path
    const first = lastPanic(x);
    assert.ok(first.len > 0, 'the panic message must be stored before the trap');
    const body = JSON.parse(first.text);
    assert.equal(body.code, 'panic');
    assert.ok(body.message.includes(text), `message ${JSON.stringify(body.message)} must contain ${JSON.stringify(text)}`);
    assertNoBuilderPath(body.message);

    // The documented host rule is "fx_free every result": on the poisoned instance that is a no-op and must not trap
    // or disturb the stored message.
    x.fx_free(first.ptr, first.len);
    x.fx_free(first.ptr, first.len);
    const afterFree = lastPanic(x);
    assert.equal(afterFree.packed, first.packed, 'fx_free must leave the panic buffer where it was');
    assert.equal(afterFree.text, first.text);

    // (c) the poisoned instance keeps failing: JS must discard it
    const again = trap(x, code);
    assert.ok(again instanceof WebAssembly.RuntimeError);
    assert.ok(lastPanic(x).len > 0, 'the message is still readable after a second trap');

    // (d) a fresh instance from the SAME compiled module is healthy and has no panic recorded
    const fresh = await instantiate();
    assert.equal(fresh.fx_abi_version(), 1);
    assert.equal(lastPanic(fresh).len, 0, 'the new instance starts with an empty panic buffer');
    assert.equal(fresh.fx_debug_panic(9), 9);
    const ptr = fresh.fx_alloc(1024);
    assert.notEqual(ptr, 0);
    fresh.fx_free(ptr, 1024);
    // ...and the old one is unaffected by the new one (its message is still there).
    assert.ok(lastPanic(x).text.includes(text));
  });
}

test('a panic text from one instance never shows up in another', { skip }, async () => {
  const a = await instantiate();
  const b = await instantiate();
  trap(a, 1);
  assert.ok(lastPanic(a).len > 0);
  assert.equal(lastPanic(b).len, 0);
  trap(b, 2);
  assert.ok(lastPanic(a).text.includes('deliberate panic'));
  assert.ok(lastPanic(b).text.includes('index out of bounds'));
});

test('code 3: a request for u32::MAX bytes returns 0 from fx_alloc, does not trap and leaves the instance usable', { skip }, async () => {
  const x = await instantiate();
  assert.equal(x.fx_debug_panic(3), 0);
  assert.equal(x.fx_alloc(0xffff_ffff), 0);
  assert.equal(lastPanic(x).len, 0, 'running out of memory is a value, not a panic');
  // Still fully usable: round-trip a real call through the ABI.
  const meta = enc.encode(JSON.stringify({ spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://example.org/' } }));
  const ptr = x.fx_alloc(meta.length);
  new Uint8Array(x.memory.buffer, ptr, meta.length).set(meta);
  const packed = x.fx_inspect(ptr, meta.length, x.fx_alloc(0), 0);
  const len = Number(packed & 0xffffffffn);
  const p = Number(packed >> 32n);
  assert.equal(JSON.parse(dec.decode(new Uint8Array(x.memory.buffer, p, len))).ok?.expect, 'none');
  x.fx_free(p, len);
});

test('100 trap-and-reinstantiate cycles reuse one compiled module and do not leak', { skip }, async (t) => {
  const module = await compiledModule();
  const settle = async () => {
    gc();
    await new Promise((resolve) => setTimeout(resolve, 20));
    gc();
  };
  await settle();
  const rssBefore = process.memoryUsage().rss;
  const memories = []; // weak: a poisoned instance must be collectable once the host drops it
  const locations = new Set();
  for (let cycle = 0; cycle < 100; cycle++) {
    const { exports: x } = await WebAssembly.instantiate(module, {}); // the SAME module every time
    memories.push(new WeakRef(x.memory));
    assert.equal(lastPanic(x).len, 0, `cycle ${cycle}: a new instance starts clean`);
    assert.ok(trap(x, 1 + (cycle % 2)) instanceof WebAssembly.RuntimeError);
    const { text } = lastPanic(x);
    assert.ok(text.length > 0, `cycle ${cycle}: panic message readable`);
    locations.add(JSON.parse(text).message.split('\n')[0].replace(/:\d+:\d+/, ':L:C'));
    if (cycle % 20 === 19) await settle(); // let V8 release dead instances as it would in a long session
  }
  await settle();
  const collected = memories.filter((ref) => ref.deref() === undefined).length;
  const rssGrowth = (process.memoryUsage().rss - rssBefore) / (1024 * 1024);
  t.diagnostic(`${collected}/100 trapped instances collected, rss +${rssGrowth.toFixed(1)} MiB`);
  // Holding all 100 instances alive costs about 18 MiB of rss (measured); collected ones cost almost nothing.
  assert.ok(collected >= 90, `only ${collected} of 100 trapped instances were garbage-collected: something holds on to them`);
  assert.ok(rssGrowth < 10, `rss grew by ${rssGrowth.toFixed(1)} MiB over 100 trap-and-reinstantiate cycles`);
  assert.equal(locations.size, 1, `the panic location is stable across cycles: ${[...locations]}`);
});
