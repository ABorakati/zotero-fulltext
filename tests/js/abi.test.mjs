// ABI tests for the real plugin/content/fulltext.wasm (built by `cargo xtask build`).
// Owned by work package core-wasm-xtask; deliberately self-contained (no shared helpers) so the JS area's
// test utilities can change without touching it. Run: node --test tests/js/abi.test.mjs
// The trap-recovery tests live in abi-recovery.test.mjs (they need the separate debug-panic build).
import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const WASM = fileURLToPath(new URL('../../plugin/content/fulltext.wasm', import.meta.url));
const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: true });
const MiB = 1024 * 1024;

const PINNED_EXPORTS = [
  'fx_abi_version', 'fx_alloc', 'fx_free', 'fx_inspect', 'fx_step', 'fx_reset', 'fx_last_panic',
];
const ADDITIVE_EXPORTS = ['fx_login_state'];
// wasm-ld always exports the memory and the two layout globals; they are harmless and not part of the ABI.
const LINKER_EXPORTS = ['memory', '__data_end', '__heap_base'];
const ALLOWLIST = [...PINNED_EXPORTS, ...ADDITIVE_EXPORTS, ...LINKER_EXPORTS];

let module;
async function load() {
  if (!module) {
    const bytes = await readFile(WASM).catch(() => {
      throw new Error(`missing ${WASM}: run "cargo xtask build" first`);
    });
    module = await WebAssembly.compile(bytes);
  }
  return module;
}

/** A fresh instance wrapped in the minimal JS calling convention from the ABI docs. */
async function instantiate() {
  const mod = await load();
  const { exports: x } = await WebAssembly.instantiate(mod, {});
  const put = (bytes) => {
    const ptr = x.fx_alloc(bytes.length);
    assert.ok(ptr !== 0 || bytes.length === 0, 'fx_alloc returned null');
    new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes); // view AFTER alloc: memory may grow
    return ptr;
  };
  /** Copy a packed (ptr << 32 | len) result out, free it, return the raw text. */
  const takeText = (packed) => {
    assert.equal(typeof packed, 'bigint', 'u64 results arrive as BigInt');
    const ptr = Number(packed >> 32n);
    const len = Number(packed & 0xffffffffn);
    const text = dec.decode(new Uint8Array(x.memory.buffer, ptr, len)); // fatal decoder: must be valid UTF-8
    x.fx_free(ptr, len);
    return text;
  };
  const take = (packed) => {
    const parsed = JSON.parse(takeText(packed));
    const keys = Object.keys(parsed);
    assert.equal(keys.length, 1, `an envelope has exactly one key: ${keys}`);
    assert.ok(keys[0] === 'ok' || keys[0] === 'err', `unexpected envelope key ${keys[0]}`);
    return parsed;
  };
  return {
    x,
    put,
    take,
    takeText,
    /** SHA-256 of the whole linear memory (changes if anything in this instance changes). */
    memoryDigest: () => createHash('sha256').update(new Uint8Array(x.memory.buffer)).digest('hex'),
    /** fx_inspect(meta JSON, body bytes) -> parsed envelope. The callee frees both inputs. */
    inspect(meta, body = new Uint8Array(0)) {
      const m = enc.encode(JSON.stringify(meta));
      return take(x.fx_inspect(put(m), m.length, put(body), body.length));
    },
    /** fx_inspect with meta passed as raw bytes (for hostile input). */
    inspectRaw(metaBytes, body = new Uint8Array(0)) {
      return take(x.fx_inspect(put(metaBytes), metaBytes.length, put(body), body.length));
    },
    step(input) {
      const b = typeof input === 'string' ? enc.encode(input) : input instanceof Uint8Array ? input : enc.encode(JSON.stringify(input));
      return take(x.fx_step(put(b), b.length));
    },
    /** fx_login_state; url/html are strings or raw bytes. */
    loginState(url, html = '') {
      const u = typeof url === 'string' ? enc.encode(url) : url;
      const h = typeof html === 'string' ? enc.encode(html) : html;
      return take(x.fx_login_state(put(u), u.length, put(h), h.length));
    },
  };
}

/** A complete `configure` command (the shape of the protocol example in proto.rs). */
function configureCommand() {
  return {
    type: 'configure', protocol: 1, seed: 2718281828, persisted: {},
    config: {
      oa: {
        openalex: true, openalex_key: '', europepmc: true, biorxiv: true, pmc_s3: true, europepmc_get_pdf: true,
        medrxiv_early: true, arxiv: true, zotero_oa: true, unpaywall_email: '', semantic_scholar_key: '', core_key: '',
        version_order: ['published', 'accepted', 'submitted'],
      },
      inst: {
        enabled: true, try_direct: true, use_openathens: true, use_ezproxy: true, check_alma: true,
        login_mode: 'window', auto_open_login: true, login_timeout_sec: 300, group_libraries: 'ask',
        confirm_above: 10, batch_max: 20, hourly_cap: 30, daily_cap: 80, mb_per_hour: 300, min_gap_sec: 5,
        challenge_windows: true,
      },
      run: { item_concurrency: 4, hedge: true, pdf_max_mb: 100 },
      institution: {
        name: "King's College London", openathens_domain: 'kcl.ac.uk',
        ezproxy: { login_host: 'login.kcl.idm.oclc.org', suffix: 'kcl.idm.oclc.org' },
        ezproxy_hosts: ['doi.org', 'www.bmj.com'],
        alma: { base: 'https://librarysearch.kcl.ac.uk', inst: '44KCL_INST' },
        libkey_library_id: 286, idp_hosts: ['login.microsoftonline.com'],
        entra_tenant: '8370cf14-16f3-4c16-b83c-724071654356', passthrough_hosts: ['journals.lww.com'], warmup_url: '',
      },
    },
  };
}

/** A one-item `start_run`: answered with not_configured until a `configure` has been applied to the engine. */
function startRunCommand(run) {
  return {
    type: "start_run", run, mode: "full", interactive: true,
    items: [{
      item: 1201, key: "1/Q7ZK2M4P",
      library: { id: 1, kind: "user", files_editable: true, name: "My Library" },
      regular: true, feed: false, item_type: "journalArticle", title: "Cold-stored whole blood in trauma resuscitation",
      fields: { doi: "10.1097/SLA.0000000000005603", pmid: "", pmcid: "", url: "", archive_id: "", extra: "" },
      pdf_attachments: 0, best_attachment: { kind: "none", exists: null },
    }],
  };
}

const NONE_META = { spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://example.org/' } };

test('module has zero imports and exports exactly the documented ABI', async () => {
  const mod = await load();
  assert.deepEqual(WebAssembly.Module.imports(mod), [], 'the module must import nothing');
  const exported = WebAssembly.Module.exports(mod);
  assert.deepEqual(
    exported.map((e) => e.name).sort(),
    [...ALLOWLIST].sort(),
    'exports must equal the allowlist (debug exports belong in the debug-panic test build only)',
  );
  const kinds = Object.fromEntries(exported.map((e) => [e.name, e.kind]));
  assert.equal(kinds.memory, 'memory');
  for (const name of [...PINNED_EXPORTS, ...ADDITIVE_EXPORTS]) assert.equal(kinds[name], 'function', name);
  assert.ok(!('fx_debug_panic' in kinds), 'the plugin module must not export fx_debug_panic');
});

test('abi version is 1 and alloc/free are total', async () => {
  const { x } = await instantiate();
  assert.equal(x.fx_abi_version(), 1);
  assert.notEqual(x.fx_alloc(0), 0, 'zero-length alloc returns a non-null dangling pointer');
  x.fx_free(0, 0); // no-ops must not trap
  x.fx_free(0, 64);
  x.fx_free(x.fx_alloc(0), 0);
  const p = x.fx_alloc(1 << 16);
  assert.notEqual(p, 0);
  x.fx_free(p, 1 << 16);
});

test('fx_alloc of 64 MiB works, is writable at both ends, and is reusable after free', async () => {
  const { x } = await instantiate();
  const size = 64 * MiB;
  const p = x.fx_alloc(size);
  assert.notEqual(p, 0, 'a 64 MiB allocation must succeed');
  const view = new Uint8Array(x.memory.buffer, p, size); // view AFTER alloc: memory just grew
  view[0] = 0xa5;
  view[size - 1] = 0x5a;
  assert.equal(view[0], 0xa5);
  assert.equal(view[size - 1], 0x5a);
  x.fx_free(p, size);
  const grown = x.memory.buffer.byteLength;
  const q = x.fx_alloc(size);
  assert.notEqual(q, 0);
  x.fx_free(q, size);
  assert.equal(x.memory.buffer.byteLength, grown, 'the freed block is reused; memory does not grow again');
});

test('fx_alloc beyond what a wasm32 allocation can address returns 0 and never traps', async () => {
  const w = await instantiate();
  for (const len of [0x8000_0000, 0xf000_0000, 0xffff_ffff]) {
    assert.equal(w.x.fx_alloc(len), 0, `fx_alloc(${len.toString(16)}) must signal out-of-memory with 0`);
  }
  assert.equal(w.inspect(NONE_META).ok?.expect, 'none', 'the instance stays healthy afterwards');
});

test('fx_inspect: an Api spec yields an ok Inspection', async () => {
  const w = await instantiate();
  const out = w.inspect({
    spec: { expect: 'api', source: 'openalex', requested: { rxiv: false } },
    resp: { status: 200, final_url: 'https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097', body_len: 2 },
  }, enc.encode('{}'));
  assert.ok(out.ok, JSON.stringify(out));
  assert.equal(out.ok.expect, 'api');
  assert.equal(out.ok.source, 'openalex');
});

test('fx_inspect: a 2 MiB body goes through every spec and yields an ok result', async () => {
  const w = await instantiate();
  const html = enc.encode(`<!doctype html><html><head><title>t</title></head><body>${'<p>café 日本語</p>'.repeat(100_000)}</body></html>`);
  assert.ok(html.length >= 2 * MiB, `fixture is ${html.length} bytes`);
  const page = { spec: { expect: 'page', requested: {}, route: 'oa' },
    resp: { status: 200, final_url: 'https://example.org/a', headers: { 'content-type': 'text/html; charset=utf-8' }, body_len: html.length, truncated: true } };
  assert.ok(w.inspect(page, html.subarray(0, 2 * MiB)).ok, 'page spec, 2 MiB of HTML');

  // A PDF is staged as head ++ tail with head_len set, but a full 2 MiB body must work too.
  const pdf = new Uint8Array(2 * MiB).fill(0x41);
  pdf.set(enc.encode('%PDF-1.7\n'), 0);
  pdf.set(enc.encode('\n%%EOF\n'), pdf.length - 7);
  const pdfMeta = { spec: { expect: 'pdf', requested: {}, route: 'oa' },
    resp: { status: 200, final_url: 'https://example.org/a.pdf', headers: { 'content-type': 'application/pdf' }, body_len: pdf.length } };
  const inspected = w.inspect(pdfMeta, pdf);
  assert.ok(inspected.ok, JSON.stringify(inspected).slice(0, 300));
  assert.ok(w.inspect(NONE_META, pdf).ok, 'none spec, 2 MiB');
  assert.ok(w.inspect(pdfMeta, new Uint8Array(0)).ok, 'an empty body is a result, not a trap');
});

test('fx_inspect: malformed meta returns an err envelope (errors are values, never traps)', async () => {
  const { x } = await instantiate();
  const bad = enc.encode('{ not json');
  const ptr = x.fx_alloc(bad.length);
  new Uint8Array(x.memory.buffer, ptr, bad.length).set(bad);
  const packed = x.fx_inspect(ptr, bad.length, x.fx_alloc(0), 0);
  const len = Number(packed & 0xffffffffn);
  const text = dec.decode(new Uint8Array(x.memory.buffer, Number(packed >> 32n), len));
  x.fx_free(Number(packed >> 32n), len);
  const env = JSON.parse(text);
  assert.equal(env.err?.code, 'bad_json');
});

test('fx_step: bad input is an err value; a valid empty step is a well-formed envelope', async () => {
  const w = await instantiate();
  assert.equal(w.step('[]').err?.code, 'bad_json');
  const env = w.step({ now: 0, wall: 0 });
  assert.ok(env.ok || env.err, JSON.stringify(env));
  w.x.fx_reset();
});

test('fx_login_state returns an ok LoginState', async () => {
  const w = await instantiate();
  const env = w.loginState('https://login.microsoftonline.com/common/oauth2/authorize');
  assert.ok(['pending', 'challenge_shown', 'landed', 'refused'].includes(env.ok?.state), JSON.stringify(env));
});

test('fx_login_state: 1 MiB of non-ASCII html is read as text and answered', async () => {
  const w = await instantiate();
  const html = `<html><body>${'日本語 💡 café مرحبا '.repeat(40_000)}</body></html>`;
  assert.ok(enc.encode(html).length > MiB, 'fixture is larger than 1 MiB of UTF-8');
  const env = w.loginState('https://login.microsoftonline.com/common/oauth2/authorize', html);
  assert.ok(['pending', 'challenge_shown', 'landed', 'refused'].includes(env.ok?.state), JSON.stringify(env).slice(0, 300));
  // html_len == 0 means "no document yet".
  assert.ok(w.loginState('https://example.org/', '').ok?.state);
});

test('every result is a non-empty, valid UTF-8 JSON envelope, even for hostile input', async () => {
  const w = await instantiate();
  const braces = new Uint8Array(5_000_000).fill(0x7b); // 5 MB of "{"
  const deep = enc.encode('['.repeat(10_000));
  const results = [
    w.inspectRaw(new Uint8Array(0)),
    w.inspectRaw(Uint8Array.of(0xff, 0xfe, 0xfd)),
    w.inspectRaw(braces),
    w.inspectRaw(deep),
    w.inspect(NONE_META, braces),
    w.step(new Uint8Array(0)),
    w.step(braces),
    w.step(deep),
    w.step(Uint8Array.of(0xff, 0xfe)),
    w.step('{"now":18446744073709551616,"wall":0}'),
    w.step('{"now":-1,"wall":0}'),
    w.step('{"now":0,"wall":0,"commands":[{"type":"no_such_command"}]}'),
    w.loginState(Uint8Array.of(0xff, 0xfe), Uint8Array.of(0xc3, 0x28, 0xa0)),
    w.loginState('', braces),
  ];
  const errors = results.filter((r) => r.err).map((r) => r.err.code);
  assert.ok(errors.every((c) => typeof c === 'string' && c.length > 0));
  assert.ok(errors.includes('bad_json'));
  assert.equal(results.at(-1).ok?.state !== undefined, true, 'a login_state poll never fails');
  w.x.fx_reset();
});

test('fx_last_panic is empty before any panic', async () => {
  const { x } = await instantiate();
  assert.equal(x.fx_last_panic() & 0xffffffffn, 0n);
});

test('repeated calls do not grow memory (no leaked input or result buffers)', async () => {
  const w = await instantiate();
  const meta = { spec: { expect: 'none' }, resp: { status: 200, final_url: 'https://example.org/' } };
  for (let i = 0; i < 50; i++) w.inspect(meta, enc.encode('x'.repeat(1000))); // warm up the allocator
  const before = w.x.memory.buffer.byteLength;
  for (let i = 0; i < 2000; i++) w.inspect(meta, enc.encode('x'.repeat(1000)));
  assert.equal(w.x.memory.buffer.byteLength, before, 'linear memory grew during steady-state calls');
});

test('fx_step and fx_login_state do not leak their buffers either', async () => {
  const w = await instantiate();
  const html = 'x'.repeat(1000);
  for (let i = 0; i < 50; i++) { w.step({ now: i, wall: i }); w.loginState('https://example.org/', html); }
  const before = w.x.memory.buffer.byteLength;
  for (let i = 50; i < 2050; i++) { w.step({ now: i, wall: i }); w.loginState('https://example.org/', html); }
  assert.equal(w.x.memory.buffer.byteLength, before, 'linear memory grew during steady-state calls');
  w.x.fx_reset();
});

test('fx_reset then fx_step works, repeatedly', async () => {
  const w = await instantiate();
  w.x.fx_reset(); // reset before any step is fine too
  for (let i = 0; i < 100; i++) {
    const env = w.step({ now: i, wall: i });
    assert.ok(env.ok, `step after reset #${i}: ${JSON.stringify(env)}`);
    w.x.fx_reset();
  }
  const final = w.step({ now: 0, wall: 0 });
  assert.ok(final.ok, JSON.stringify(final));
  w.x.fx_reset();
  w.x.fx_reset(); // idempotent
});

test('two instances of one compiled module share nothing: fx_step on A and fx_reset on B are independent', async (t) => {
  const a = await instantiate();
  const b = await instantiate();
  assert.notEqual(a.x.memory, b.x.memory, 'each instance has its own linear memory');

  const configured = a.step({ now: 1, wall: 1, commands: [configureCommand()] });
  a.step({ now: 2, wall: 2 });
  const snapshot = a.memoryDigest();

  // B's reset and steps must not disturb A: not one byte of A's memory changes.
  b.x.fx_reset();
  assert.ok(b.step({ now: 5, wall: 5 }).ok);
  b.x.fx_reset();
  b.x.fx_reset();
  assert.equal(a.memoryDigest(), snapshot, 'instance A changed while only instance B ran');

  // A keeps answering exactly as before (state-less step), and B's panic buffer and A's stay independent.
  assert.ok(a.step({ now: 3, wall: 3 }).ok);
  assert.equal(a.x.fx_last_panic() & 0xffffffffn, 0n);
  assert.equal(b.x.fx_last_panic() & 0xffffffffn, 0n);

  // Engine-state level, observed through `start_run`: `not_configured` until a `configure` was applied to THIS instance.
  const isConfigured = (w, run) => w.step({ now: 10 + run, wall: 10 + run, commands: [startRunCommand(run)] }).err?.code !== "not_configured";
  if (configured.ok) {
    assert.ok(isConfigured(a, 1), "A is still configured after B was reset");
    assert.ok(!isConfigured(b, 2), "B was never configured: it must not see A's configuration");
    assert.ok(b.step({ now: 20, wall: 20, commands: [configureCommand()] }).ok);
    assert.ok(isConfigured(b, 3));
    a.x.fx_reset();
    assert.ok(!isConfigured(a, 4), "A forgot its configuration after its own fx_reset");
    assert.ok(isConfigured(b, 5), "B was not affected by A being reset");
  } else {
    assert.equal(configured.err?.code, "not_implemented", `configure failed: ${JSON.stringify(configured.err)}`);
    t.diagnostic("engine state-level independence not asserted: the engine does not implement configure yet");
  }
});
