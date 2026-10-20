// ZFT.bodies.BodyStore: handles, refcounts, caps, PDF slots (register before use), inspect staging, charset decoding.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeFs, makeFakeZotero } from './helpers/fakes.mjs';

function makeEnv(overrides = {}) {
  const fsx = makeFakeFs();
  const log = [];
  let n = 0;
  const state = {
    async registerTmpDir(p) { log.push(['register', p, fsx.dirs.has(p)]); },
    async unregisterTmpDir(p) { log.push(['unregister', p]); },
  };
  const attachments = {
    async createTemporaryStorageDirectory() {
      const p = `/storage/tmp-${++n}`;
      await fsx.IOUtils.makeDirectory(p);
      log.push(['create', p]);
      return { path: p };
    },
  };
  const { ZFT } = loadPlugin({ files: ['util.js', 'bodies.js'], globals: { Zotero: makeFakeZotero() } });
  const deps = { fs: fsx.IOUtils, state, attachments, pathUtils: fsx.PathUtils, ...overrides };
  const store = new ZFT.bodies.BodyStore(deps);
  return { ZFT, store, fsx, log, state, deps };
}
const seq = (n, mod = 251) => Uint8Array.from({ length: n }, (_, i) => i % mod);

/** Write a PDF-ish file of `len` bytes into a slot and adopt it. */
async function putPdf(env, len, run) {
  const slot = await env.store.allocPdfSlot(run);
  await env.fsx.IOUtils.write(slot.path, seq(len));
  const ref = env.store.putFile({ path: slot.path, dir: slot.dir, len, run });
  return { slot, ref };
}

test('handles start at 1, are never reused, and carry the body length', () => {
  const { store } = makeEnv();
  const a = store.putMemory(Uint8Array.of(1, 2, 3));
  const b = store.putMemory(new Uint8Array(0));
  assert.deepEqual(plain(a), { handle: 1, len: 3 });
  assert.deepEqual(plain(b), { handle: 2, len: 0 });
  assert.equal(store.get(1).refs, 1);
  assert.equal(store.get(1).kind, 'page');
  assert.equal(store.get(99), null);
});

test('refcounts: retain adds a reference, release at 0 frees, unknown or repeated release is a no-op', async () => {
  const { store, ZFT } = makeEnv();
  const { handle } = store.putMemory(seq(100));
  assert.equal(store.retain(handle), handle);
  assert.equal(store.get(handle).refs, 2);
  await store.release(handle);
  assert.equal(store.get(handle).refs, 1);
  assert.deepEqual(plain(store.stats()), { count: 1, memBytes: 100, files: 0 });
  await store.release(handle);
  assert.equal(store.get(handle), null);
  assert.deepEqual(plain(store.stats()), { count: 0, memBytes: 0, files: 0 });
  await store.release(handle);   // already gone
  await store.release(12345);    // never existed
  assert.equal(store.stats().memBytes, 0, 'memBytes never goes negative');
  assert.throws(() => store.retain(handle), (e) => e instanceof ZFT.util.OpError && e.kind === 'internal');
  const next = store.putMemory(Uint8Array.of(1));
  assert.ok(next.handle > handle, 'a freed handle is not reused');
});

test('putMemory copies unless adopt is set', () => {
  const { store } = makeEnv();
  const src = seq(10);
  const copy = store.putMemory(src);
  src[0] = 200;
  assert.equal(store.get(copy.handle).mem[0], 0, 'the stored bytes are a copy');
  const adopted = store.putMemory(src, { adopt: true, run: 4, kind: 'page' });
  assert.equal(store.get(adopted.handle).mem, src, 'adopted: same array');
  assert.equal(store.get(adopted.handle).run, 4);
});

test('putMemory caps: PAGE_MAX_BYTES each, MEM_CAP_TOTAL overall; failures change nothing; release frees room', async () => {
  const { store, ZFT } = makeEnv();
  const { PAGE_MAX_BYTES, MEM_CAP_TOTAL } = ZFT.bodies;
  assert.equal(PAGE_MAX_BYTES, 8 * 1024 * 1024);
  assert.equal(MEM_CAP_TOTAL, 64 * 1024 * 1024);
  const tooBig = new Uint8Array(PAGE_MAX_BYTES + 1);
  assert.throws(() => store.putMemory(tooBig, { adopt: true }), (e) => e instanceof ZFT.util.OpError && e.kind === 'too_large');
  assert.equal(store.stats().count, 0);

  const block = new Uint8Array(PAGE_MAX_BYTES);   // exactly the per-body cap: allowed
  const refs = [];
  for (let i = 0; i < MEM_CAP_TOTAL / PAGE_MAX_BYTES; i++) refs.push(store.putMemory(block, { adopt: true }));
  assert.equal(store.stats().memBytes, MEM_CAP_TOTAL);
  assert.throws(() => store.putMemory(Uint8Array.of(1)), (e) => e instanceof ZFT.util.OpError && e.kind === 'too_large' && /exceed/.test(e.message));
  assert.equal(store.stats().memBytes, MEM_CAP_TOTAL, 'a rejected put changes nothing');
  await store.release(refs[0].handle);
  assert.equal(store.stats().memBytes, MEM_CAP_TOTAL - PAGE_MAX_BYTES);
  assert.doesNotThrow(() => store.putMemory(Uint8Array.of(1)));
});

test('allocPdfSlot registers the directory BEFORE returning it; the slot is <dir>/file.tmp', async () => {
  const { store, log, fsx } = makeEnv();
  const slot = await store.allocPdfSlot(7);
  assert.deepEqual(plain(slot), { dir: '/storage/tmp-1', path: '/storage/tmp-1/file.tmp' });
  assert.deepEqual(log, [['create', '/storage/tmp-1'], ['register', '/storage/tmp-1', true]], 'created, then registered while it exists');
  assert.ok(fsx.dirs.has('/storage/tmp-1'));
  const second = await store.allocPdfSlot();
  assert.equal(second.dir, '/storage/tmp-2');
});

test('allocPdfSlot: a failing registration removes the directory and reports io', async () => {
  const env = makeEnv();
  env.state.registerTmpDir = async () => { throw new Error('disk full'); };
  await assert.rejects(env.store.allocPdfSlot(1), (e) => e instanceof env.ZFT.util.OpError && e.kind === 'io' && /disk full/.test(e.message));
  assert.ok(!env.fsx.dirs.has('/storage/tmp-1'), 'the unregistered directory was removed again');
});

test('file bodies: release deletes the directory (recursive) and then unregisters it', async () => {
  const env = makeEnv();
  const { slot, ref } = await putPdf(env, 5000, 3);
  assert.deepEqual(plain(ref), { handle: 1, len: 5000 });
  assert.equal(env.store.pathOf(ref.handle), slot.path);
  assert.deepEqual(plain(env.store.stats()), { count: 1, memBytes: 0, files: 1 });
  assert.ok(env.fsx.files.has(slot.path));
  env.log.length = 0;
  await env.store.release(ref.handle);
  assert.ok(!env.fsx.files.has(slot.path));
  assert.ok(!env.fsx.dirs.has(slot.dir));
  assert.deepEqual(env.log, [['unregister', slot.dir]]);
  assert.equal(env.store.stats().files, 0);
});

test('file bodies: a failing removal keeps the directory registered for the sweep and does not throw', async () => {
  const env = makeEnv();
  const { slot, ref } = await putPdf(env, 100);
  env.fsx.IOUtils.remove = async () => { throw new Error('file in use'); };
  env.log.length = 0;
  await env.store.release(ref.handle);
  assert.equal(env.store.get(ref.handle), null, 'the handle is gone either way');
  assert.deepEqual(env.log, [], 'not unregistered: the startup sweep will retry');
  assert.ok(env.fsx.dirs.has(slot.dir));
});

test('discardSlot removes and unregisters a slot that never became a body', async () => {
  const env = makeEnv();
  const slot = await env.store.allocPdfSlot();
  await env.fsx.IOUtils.write(slot.path, seq(10));
  env.log.length = 0;
  await env.store.discardSlot(slot);
  assert.ok(!env.fsx.files.has(slot.path));
  assert.deepEqual(env.log, [['unregister', slot.dir]]);
  await env.store.discardSlot(null);   // tolerated
});

test('putBlobAsPdf writes the blob into a registered slot and adopts it', async () => {
  const env = makeEnv();
  const bytes = seq(9000);
  const ref = await env.store.putBlobAsPdf(new Blob([bytes]), 5);
  assert.deepEqual(plain(ref), { handle: 1, len: 9000 });
  const info = env.store.get(ref.handle);
  assert.equal(info.kind, 'pdf');
  assert.equal(info.run, 5);
  assert.deepEqual([...env.fsx.files.get(info.path)], [...bytes]);
  assert.equal(env.log[1][0], 'register');
});

test('putBlobAsPdf: a failing write discards the slot and reports io', async () => {
  const env = makeEnv();
  env.fsx.IOUtils.write = async () => { throw new Error('quota'); };
  await assert.rejects(env.store.putBlobAsPdf(new Blob([seq(10)]), 5), (e) => e instanceof env.ZFT.util.OpError && e.kind === 'io' && /quota/.test(e.message));
  assert.ok(!env.fsx.dirs.has('/storage/tmp-1'));
  assert.deepEqual(env.log.at(-1), ['unregister', '/storage/tmp-1']);
  assert.equal(env.store.stats().count, 0);
});

test('releaseAll(run) frees every body of that run whatever its refcount; releaseAll() frees everything', async () => {
  const env = makeEnv();
  const a = env.store.putMemory(seq(10), { run: 1 });
  env.store.retain(a.handle);
  env.store.retain(a.handle);
  const b = env.store.putMemory(seq(20), { run: 2 });
  const noRun = env.store.putMemory(seq(30));
  const { slot, ref: f1 } = await putPdf(env, 6000, 1);
  const { ref: f2 } = await putPdf(env, 6000, 2);
  assert.equal(await env.store.releaseAll(1), 2);
  assert.equal(env.store.get(a.handle), null, 'refcount 3 does not protect it');
  assert.equal(env.store.get(f1.handle), null);
  assert.ok(!env.fsx.dirs.has(slot.dir));
  assert.ok(env.store.get(b.handle) && env.store.get(noRun.handle) && env.store.get(f2.handle));
  assert.equal(env.store.stats().memBytes, 50);
  assert.equal(await env.store.releaseAll(99), 0);
  assert.equal(await env.store.releaseAll(), 3);
  assert.deepEqual(plain(env.store.stats()), { count: 0, memBytes: 0, files: 0 });
  assert.equal(env.fsx.dirs.has('/storage/tmp-2'), false);
});

// ---------------------------------------------------------------------------------------------------------------
// staging for the inspector
// ---------------------------------------------------------------------------------------------------------------
test('stageBytes (page): the first INSPECT_TEXT_MAX bytes, truncated flag, full body_len, head_len null', async () => {
  const { store, ZFT } = makeEnv();
  const MAX = ZFT.bodies.INSPECT_TEXT_MAX;
  assert.equal(MAX, 2 * 1024 * 1024);
  const small = await store.stageBytes(seq(10), 'page');
  assert.deepEqual({ ...plain(small), bytes: [...small.bytes] }, { bytes: [...seq(10)], head_len: null, truncated: false, body_len: 10 });
  const exact = await store.stageBytes(new Uint8Array(MAX), 'page');
  assert.equal(exact.bytes.length, MAX);
  assert.equal(exact.truncated, false);
  const over = await store.stageBytes(new Uint8Array(MAX + 1), 'page');
  assert.equal(over.bytes.length, MAX);
  assert.equal(over.truncated, true);
  assert.equal(over.body_len, MAX + 1, 'body_len is the real length');
  assert.equal(over.head_len, null);
  const empty = await store.stageBytes(new Uint8Array(0), 'page');
  assert.deepEqual({ ...plain(empty), bytes: empty.bytes.length }, { bytes: 0, head_len: null, truncated: false, body_len: 0 });
  const other = await store.stageBytes(seq(5), undefined);
  assert.equal(other.body_len, 5, 'anything that is not pdf is text');
});

test('stageBytes (pdf): whole body up to head+tail, else head ++ tail with head_len = PDF_HEAD', async () => {
  const { store, ZFT } = makeEnv();
  const { PDF_HEAD, PDF_TAIL } = ZFT.bodies;
  assert.equal(PDF_HEAD, 4096);
  assert.equal(PDF_TAIL, 2048);
  for (const len of [0, 1, PDF_HEAD + PDF_TAIL]) {
    const s = await store.stageBytes(seq(len), 'pdf');
    assert.equal(s.bytes.length, len);
    assert.equal(s.head_len, null, `len ${len}: complete body`);
    assert.equal(s.body_len, len);
    assert.equal(s.truncated, false);
  }
  const body = seq(PDF_HEAD + PDF_TAIL + 1);
  const long = await store.stageBytes(body, 'pdf');
  assert.equal(long.head_len, PDF_HEAD);
  assert.equal(long.body_len, body.length);
  assert.equal(long.bytes.length, PDF_HEAD + PDF_TAIL);
  assert.deepEqual([...long.bytes.subarray(0, PDF_HEAD)], [...body.subarray(0, PDF_HEAD)]);
  assert.deepEqual([...long.bytes.subarray(PDF_HEAD)], [...body.subarray(body.length - PDF_TAIL)]);
});

test('stageFile / stageForInspect read only the head and tail of a big file', async () => {
  const env = makeEnv();
  const { store, ZFT, fsx } = env;
  const { PDF_HEAD, PDF_TAIL } = ZFT.bodies;
  const reads = [];
  const realRead = fsx.IOUtils.read.bind(fsx.IOUtils);
  fsx.IOUtils.read = async (p, opts) => { reads.push(plain(opts || {})); return realRead(p, opts); };

  const big = 1_000_000;
  const { ref } = await putPdf(env, big);
  reads.length = 0;
  const s = await store.stageForInspect(ref.handle);
  assert.equal(s.head_len, PDF_HEAD);
  assert.equal(s.body_len, big);
  assert.equal(s.bytes.length, PDF_HEAD + PDF_TAIL);
  const whole = seq(big);
  assert.deepEqual([...s.bytes.subarray(0, PDF_HEAD)], [...whole.subarray(0, PDF_HEAD)]);
  assert.deepEqual([...s.bytes.subarray(PDF_HEAD)], [...whole.subarray(big - PDF_TAIL)]);
  assert.deepEqual(reads.sort((a, b) => a.offset - b.offset), [{ offset: 0, maxBytes: PDF_HEAD }, { offset: big - PDF_TAIL, maxBytes: PDF_TAIL }]);

  // short file: whole content, no head_len
  const short = await putPdf(env, 100);
  reads.length = 0;
  const sShort = await store.stageForInspect(short.ref.handle);
  assert.equal(sShort.head_len, null);
  assert.equal(sShort.bytes.length, 100);
  assert.equal(sShort.body_len, 100);
  assert.deepEqual(reads, [{ maxBytes: 100 }]);

  // boundary: exactly head + tail is still "whole"
  const edge = await putPdf(env, PDF_HEAD + PDF_TAIL);
  assert.equal((await store.stageForInspect(edge.ref.handle)).head_len, null);
  const edge1 = await putPdf(env, PDF_HEAD + PDF_TAIL + 1);
  assert.equal((await store.stageForInspect(edge1.ref.handle)).head_len, PDF_HEAD);

  // empty file: nothing is read
  const empty = await putPdf(env, 0);
  reads.length = 0;
  const sEmpty = await store.stageForInspect(empty.ref.handle);
  assert.deepEqual({ ...plain(sEmpty), bytes: sEmpty.bytes.length }, { bytes: 0, head_len: null, truncated: false, body_len: 0 });
  assert.deepEqual(reads, []);
});

test('stageFile stats the file when the length is unknown; read errors become io', async () => {
  const env = makeEnv();
  const slot = await env.store.allocPdfSlot();
  await env.fsx.IOUtils.write(slot.path, seq(10000));
  const s = await env.store.stageFile(slot.path);
  assert.equal(s.body_len, 10000);
  assert.equal(s.head_len, env.ZFT.bodies.PDF_HEAD);
  await assert.rejects(env.store.stageFile('/nope/file.tmp'), (e) => e.kind === 'io');
  await assert.rejects(env.store.stageFile('/nope/file.tmp', 5000), (e) => e.kind === 'io');
});

test('stageForInspect on a memory body stages it as text', async () => {
  const { store, ZFT } = makeEnv();
  const ref = store.putMemory(new Uint8Array(ZFT.bodies.INSPECT_TEXT_MAX + 10), {});
  const s = await store.stageForInspect(ref.handle);
  assert.equal(s.truncated, true);
  assert.equal(s.bytes.length, ZFT.bodies.INSPECT_TEXT_MAX);
  assert.equal(s.body_len, ZFT.bodies.INSPECT_TEXT_MAX + 10);
  await assert.rejects(store.stageForInspect(777), (e) => e.kind === 'internal');
});

// ---------------------------------------------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------------------------------------------
test('readBytes: memory bodies come back as they are, file bodies are read, huge files and unknown handles are refused', async () => {
  const env = makeEnv();
  const mem = env.store.putMemory(seq(20));
  assert.deepEqual([...await env.store.readBytes(mem.handle)], [...seq(20)]);
  const { ref } = await putPdf(env, 7000);
  assert.deepEqual([...await env.store.readBytes(ref.handle)], [...seq(7000)]);
  const huge = env.store.putFile({ path: '/storage/x/file.tmp', dir: '/storage/x', len: 300 * 1024 * 1024 });
  await assert.rejects(env.store.readBytes(huge.handle), (e) => e.kind === 'too_large');
  await assert.rejects(env.store.readBytes(31337), (e) => e.kind === 'internal');
  assert.throws(() => env.store.pathOf(mem.handle), (e) => e.kind === 'internal');
  assert.throws(() => env.store.pathOf(31337), (e) => e.kind === 'internal');
  env.fsx.files.delete(env.store.pathOf(ref.handle));
  await assert.rejects(env.store.readBytes(ref.handle), (e) => e.kind === 'io');
});

test('readText: charset from the Content-Type header, BOM stripped, unknown label falls back to utf-8', async () => {
  const { store } = makeEnv();
  const text = async (bytes, contentType) => store.readText(store.putMemory(Uint8Array.from(bytes)).handle, { contentType });
  const enc = (s) => [...new TextEncoder().encode(s)];

  assert.equal(await text(enc('caf\u00e9 \u2713'), 'text/html; charset=utf-8'), 'caf\u00e9 \u2713');
  assert.equal(await text(enc('caf\u00e9'), undefined), 'caf\u00e9', 'default utf-8');
  assert.equal(await text([0x63, 0x61, 0x66, 0xe9], 'text/html; charset=ISO-8859-1'), 'caf\u00e9');
  assert.equal(await text([0x63, 0x61, 0x66, 0xe9], 'text/html;charset="windows-1252"'), 'caf\u00e9');
  assert.equal(await text([0x80], 'text/html; charset=windows-1252'), '\u20ac', 'windows-1252, not latin-1');
  assert.equal(await text([0xef, 0xbb, 0xbf, ...enc('bom')], 'text/html; charset=utf-8'), 'bom', 'utf-8 BOM stripped');
  assert.equal(await text([0xef, 0xbb, 0xbf, ...enc('bom')], undefined), 'bom');
  assert.equal(await text([0xff, 0xfe, 0x61, 0x00, 0x62, 0x00], 'text/html; charset=utf-16le'), 'ab', 'utf-16 BOM stripped');
  assert.equal(await text(enc('x\u00e9y'), 'text/html; charset=no-such-charset'), 'x\u00e9y', 'unknown label: utf-8');
  assert.equal(await text([], 'text/html'), '');
});

test('readText: <meta charset> is used when the header has none; the header wins over the page', async () => {
  const { store } = makeEnv();
  const latin = (s) => Uint8Array.from([...s].map((c) => c.charCodeAt(0)));
  const read = (bytes, contentType) => store.readText(store.putMemory(bytes).handle, { contentType });
  const meta = latin('<html><head><meta charset="iso-8859-1"></head><body>caf\u00e9</body></html>');
  assert.equal(await read(meta, 'text/html'), '<html><head><meta charset="iso-8859-1"></head><body>caf\u00e9</body></html>');
  const httpEquiv = latin('<meta http-equiv="Content-Type" content="text/html; charset=windows-1252">\u00e9');
  assert.ok((await read(httpEquiv, '')).endsWith('\u00e9'));
  const utf8Doc = new TextEncoder().encode('<meta charset="iso-8859-1">\u00e9');
  assert.equal(await read(utf8Doc, 'text/html; charset=utf-8'), '<meta charset="iso-8859-1">\u00e9', 'header charset beats <meta>');
});

test('defaults come from the platform globals at call time (IOUtils, ZFT.state, Zotero.Attachments, PathUtils)', async () => {
  const fsx = makeFakeFs();
  const registered = [];
  const zotero = makeFakeZotero({
    Attachments: { async createTemporaryStorageDirectory() { await fsx.IOUtils.makeDirectory('/s/tmp-x'); return { path: '/s/tmp-x' }; } },
  });
  const { ZFT } = loadPlugin({
    files: ['util.js', 'bodies.js'], globals: { Zotero: zotero, IOUtils: fsx.IOUtils, PathUtils: fsx.PathUtils },
  });
  ZFT.state = { registerTmpDir: async (p) => registered.push(p), unregisterTmpDir: async (p) => registered.splice(registered.indexOf(p), 1) };
  const store = new ZFT.bodies.BodyStore();
  const slot = await store.allocPdfSlot();
  assert.deepEqual(registered, ['/s/tmp-x']);
  assert.equal(slot.path, '/s/tmp-x/file.tmp');
  await store.discardSlot(slot);
  assert.deepEqual(registered, []);
  assert.equal(ZFT.bodies.store, null, 'the plugin-wide store is created by the plugin, not by this file');
});

test('without PathUtils the slot path keeps the separator style of the directory', async () => {
  const win = makeEnv({ pathUtils: null });
  win.deps.attachments.createTemporaryStorageDirectory = async () => ({ path: 'C:\\Users\\a\\Zotero\\storage\\tmp-abc' });
  const slot = await win.store.allocPdfSlot();
  assert.equal(slot.path, 'C:\\Users\\a\\Zotero\\storage\\tmp-abc\\file.tmp');
  const posix = makeEnv({ pathUtils: null });
  posix.deps.attachments.createTemporaryStorageDirectory = async () => ({ path: '/home/a/storage/tmp-abc/' });
  assert.equal((await posix.store.allocPdfSlot()).path, '/home/a/storage/tmp-abc/file.tmp');
});
