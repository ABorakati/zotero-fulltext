// ZFT.state: persisted runtime state (state.json), debounced atomic writes, temp-dir registry and sweep.
// Real plugin/content/state.js in a vm context with the in-memory IOUtils/PathUtils and virtual timers.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeFs, makeFakeTimers, makeFakeZotero } from './helpers/fakes.mjs';

const DIR = '/profile/zotero-fulltext';
const FILE = DIR + '/state.json';
const TMP = DIR + '/tmp';

/** Let pending microtasks and the fake async IO finish. */
async function settle() {
  for (let i = 0; i < 8; i++) await new Promise((resolve) => setImmediate(resolve));
}

/**
 * A fresh plugin context over an (optionally shared) fake file system. The IOUtils wrapper records every
 * writeJSON/readJSON/move and can hold or fail writes: env.hold() blocks the next write until release().
 */
function env(shared = {}) {
  const fs = shared.fs || makeFakeFs();
  const timers = shared.timers || makeFakeTimers();
  const zotero = makeFakeZotero();
  const rec = { writes: [], reads: 0, moves: [], failWrites: 0, active: 0, maxActive: 0, gate: null };
  const io = fs.IOUtils;
  const realWrite = io.writeJSON;
  const realRead = io.readJSON;
  const realMove = io.move;
  const wrapped = Object.create(io);
  wrapped.writeJSON = async (p, obj, opts) => {
    rec.active += 1;
    rec.maxActive = Math.max(rec.maxActive, rec.active);
    try {
      const snapshot = JSON.parse(JSON.stringify(obj));
      rec.writes.push({ path: p, obj: snapshot, opts });
      if (rec.gate) await rec.gate;
      if (rec.failWrites > 0) { rec.failWrites -= 1; throw new Error('disk full'); }
      // Like the real IOUtils.writeJSON, the content is fixed at call time, not after the write gets going.
      return await realWrite.call(io, p, snapshot, opts);
    } finally {
      rec.active -= 1;
    }
  };
  wrapped.readJSON = async (p) => { rec.reads += 1; return realRead.call(io, p); };
  wrapped.move = async (a, b, o) => { rec.moves.push([a, b, o]); return realMove.call(io, a, b); };
  const { ZFT } = loadPlugin({
    globals: { Zotero: zotero, IOUtils: wrapped, PathUtils: fs.PathUtils, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout },
    files: ['util.js', 'state.js'],
  });
  const hold = () => {
    let release;
    rec.gate = new Promise((resolve) => { release = () => { rec.gate = null; resolve(); }; });
    return release;
  };
  return { ZFT, S: ZFT.state, fs, timers, zotero, rec, hold };
}

const readFile = (fs, path) => JSON.parse(new TextDecoder().decode(fs.files.get(path)));
const putFile = (fs, path, text) => fs.files.set(path, new TextEncoder().encode(text));
const logged = (zotero, re) => zotero.debugLog.some(([, m]) => re.test(m));

const DEFAULT_PERSISTED = {
  ledger: { committed: [], reserved: [] },
  halts: { inst_until: 0, reason: null, hosts: {} },
  ezproxy: {},
  alma: {},
};

test('init creates the data dir and tmp/ and starts from defaults when there is no file', async () => {
  const { S, fs } = env();
  await S.init();
  assert.ok(fs.dirs.has(DIR), 'data dir');
  assert.ok(fs.dirs.has(TMP), 'tmp dir');
  assert.equal(S.dataDir(), DIR);
  assert.equal(S.tmpRoot(), TMP);
  assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED);
  assert.equal(fs.files.has(FILE), false, 'nothing is written until something changes');
  await S.dispose();
});

test('init honours an explicit dir', async () => {
  const { S, fs } = env();
  await S.init({ dir: '/elsewhere/data' });
  assert.equal(S.dataDir(), '/elsewhere/data');
  assert.ok(fs.dirs.has('/elsewhere/data/tmp'));
  await S.dispose();
});

test('getPersisted returns a deep copy', async () => {
  const { S } = env();
  await S.init();
  const a = S.getPersisted();
  a.ledger.committed.push([1, 2]);
  a.halts.hosts.x = 5;
  assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED);
  await S.dispose();
});

test('round trip: set with flush writes atomically and a new instance reads it back', async () => {
  const first = env();
  await first.S.init();
  const ledger = { committed: [[1700000000000, 2048]], reserved: [{ id: 7, wall_ms: 1700000000100, bytes: 4096 }] };
  const halts = { inst_until: 1700000999999, reason: 'publisher_block', hosts: { 'www.sciencedirect.com': 1700000555555 } };
  await first.S.set('ledger', ledger, { flush: true });
  await first.S.set('halts', halts, { flush: true });
  await first.S.set('ezproxy', { 'www.bmj.com': { configured: true, at: 1700000000000 } });
  await first.S.set('alma', { '10.1/x': { held: false, at: 1700000000001 } });
  const id = first.S.nextRunId();
  await first.S.dispose();

  const last = first.rec.writes[first.rec.writes.length - 1];
  assert.equal(last.path, FILE);
  assert.equal(last.opts.tmpPath, FILE + '.tmp', 'atomic write through a temp file');
  assert.equal(last.obj.v, 1);

  const second = env({ fs: first.fs });
  await second.S.init();
  const got = plain(second.S.getPersisted());
  assert.deepEqual(got.ledger, ledger);
  assert.deepEqual(got.halts, halts);
  assert.deepEqual(got.ezproxy, { 'www.bmj.com': { configured: true, at: 1700000000000 } });
  assert.deepEqual(got.alma, { '10.1/x': { held: false, at: 1700000000001 } });
  assert.equal(second.S.nextRunId(), id + 1, 'run ids continue after a restart');
  await second.S.dispose();
});

test('set deep-copies the value, so later mutation by the caller does not leak in', async () => {
  const { S } = env();
  await S.init();
  const value = { committed: [[1, 2]], reserved: [] };
  await S.set('ledger', value);
  value.committed.push([3, 4]);
  assert.deepEqual(plain(S.getPersisted().ledger.committed), [[1, 2]]);
  await S.dispose();
});

test('a corrupt state.json is moved to state.json.bad and defaults are used', async () => {
  const { S, fs, rec, zotero } = env();
  putFile(fs, FILE, '{"v":1,"persisted":{');
  await S.init();
  assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED);
  assert.equal(fs.files.has(FILE), false);
  assert.equal(new TextDecoder().decode(fs.files.get(FILE + '.bad')), '{"v":1,"persisted":{');
  assert.equal(rec.moves.length, 1);
  assert.deepEqual(plain(rec.moves[0][2]), { noOverwrite: false }, 'an older .bad is overwritten');
  assert.ok(logged(zotero, /state\.json/));
  await S.dispose();
});

test('wrong version or wrong shapes are treated as corrupt', async () => {
  const bad = [
    '{"v":2,"persisted":{}}',
    '[]',
    '"text"',
    '{"persisted":{}}',
    '{"v":1,"persisted":[]}',
    '{"v":1,"persisted":{"ledger":[]}}',
    '{"v":1,"persisted":{"ledger":{"committed":5}}}',
    '{"v":1,"persisted":{"halts":{"inst_until":"soon"}}}',
    '{"v":1,"persisted":{"halts":{"reason":42}}}',
    '{"v":1,"persisted":{"halts":{"hosts":[]}}}',
    '{"v":1,"persisted":{"ezproxy":"x"}}',
    '{"v":1,"tmp_dirs":"x"}',
    '{"v":1,"run_counter":-1}',
    '{"v":1,"run_counter":"7"}',
  ];
  for (const text of bad) {
    const { S, fs } = env();
    putFile(fs, FILE, text);
    await S.init();
    assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED, text);
    assert.ok(fs.files.has(FILE + '.bad'), 'kept as .bad: ' + text);
    assert.equal(fs.files.has(FILE), false, text);
    await S.dispose();
  }
});

test('missing keys are filled with defaults and valid data is kept', async () => {
  const { S, fs, rec } = env();
  putFile(fs, FILE, JSON.stringify({
    v: 1,
    persisted: { halts: { inst_until: 5 }, ledger: { committed: [[1, 2]] } },
    tmp_dirs: ['/a', '/a', '', 7, '/b'],
    run_counter: 9,
  }));
  await S.init();
  const p = plain(S.getPersisted());
  assert.deepEqual(p.halts, { inst_until: 5, reason: null, hosts: {} });
  assert.deepEqual(p.ledger, { committed: [[1, 2]], reserved: [] });
  assert.deepEqual(p.ezproxy, {});
  assert.deepEqual(p.alma, {});
  assert.equal(S.nextRunId(), 10);
  assert.equal(rec.moves.length, 0, 'a usable file is left alone');
  await S.dispose();
});

test('single malformed entries are dropped instead of failing the whole file', async () => {
  const { S, fs, rec } = env();
  putFile(fs, FILE, JSON.stringify({
    v: 1,
    persisted: {
      ledger: { committed: [[1, 2], [3], 'x', [4, -1], [5, 6]], reserved: [{ id: 1, wall_ms: 2, bytes: 3 }, { id: 'a' }, null] },
      halts: { inst_until: 0, reason: null, hosts: { 'a.com': 5, 'b.com': 'later' } },
      ezproxy: { 'ok.com': { configured: true, at: 1 }, 'bad.com': { configured: 'yes', at: 1 }, 'worse.com': 3 },
      alma: { 'k': { held: true, at: 2 }, 'j': { held: true } },
    },
  }));
  await S.init();
  const p = plain(S.getPersisted());
  assert.deepEqual(p.ledger.committed, [[1, 2], [5, 6]]);
  assert.deepEqual(p.ledger.reserved, [{ id: 1, wall_ms: 2, bytes: 3 }]);
  assert.deepEqual(p.halts.hosts, { 'a.com': 5 });
  assert.deepEqual(p.ezproxy, { 'ok.com': { configured: true, at: 1 } });
  assert.deepEqual(p.alma, { k: { held: true, at: 2 } });
  assert.equal(rec.moves.length, 0);
  await S.dispose();
});

test('a second init with the same dir does not re-read; concurrent inits share one load', async () => {
  const { S, rec } = env();
  await Promise.all([S.init(), S.init(), S.init({ dir: DIR })]);
  assert.equal(rec.reads, 1);
  await S.init();
  assert.equal(rec.reads, 1);
  await S.dispose();
  await S.init();
  assert.equal(rec.reads, 2, 'dispose forgets the loaded file');
  await S.dispose();
});

test('init with another dir flushes the old state and loads the new one', async () => {
  const { S, fs } = env();
  await S.init();
  await S.set('halts', { inst_until: 9, reason: 'r', hosts: {} });
  await S.init({ dir: '/other' });
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 9, 'old dir was flushed');
  assert.equal(S.dataDir(), '/other');
  assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED);
  await S.dispose();
});

test('debounce: three sets give one write after FLUSH_DEBOUNCE_MS virtual milliseconds', async () => {
  const { S, timers, rec } = env();
  assert.equal(S.FLUSH_DEBOUNCE_MS, 2000);
  await S.init();
  await S.set('halts', { inst_until: 1, reason: null, hosts: {} });
  await S.set('halts', { inst_until: 2, reason: null, hosts: {} });
  await S.set('halts', { inst_until: 3, reason: null, hosts: {} });
  assert.equal(timers.pendingCount(), 1, 'one timer for all three changes');
  timers.advance(1999);
  await settle();
  assert.equal(rec.writes.length, 0, 'not yet');
  timers.advance(1);
  await settle();
  assert.equal(rec.writes.length, 1);
  assert.equal(rec.writes[0].obj.persisted.halts.inst_until, 3, 'the latest value');
  assert.equal(timers.pendingCount(), 0);
  // Nothing dirty any more: no further writes.
  timers.advance(10000);
  await settle();
  assert.equal(rec.writes.length, 1);
  // The next change arms the timer again.
  await S.set('alma', { 'a': { held: true, at: 1 } });
  assert.equal(timers.pendingCount(), 1);
  await S.dispose();
});

test('the debounce is bounded: a steady stream of changes still reaches disk within the window', async () => {
  const { S, timers, rec } = env();
  await S.init();
  for (let i = 1; i <= 4; i++) {
    await S.set('halts', { inst_until: i, reason: null, hosts: {} });
    timers.advance(600);
  }
  await settle();
  assert.equal(rec.writes.length, 1, 'written once the first change is 2000 ms old');
  assert.equal(rec.writes[0].obj.persisted.halts.inst_until, 4);
  await S.dispose();
});

test('set with flush resolves only after the file is written, and cancels the pending timer', async () => {
  const { S, fs, timers, rec } = env();
  await S.init();
  await S.set('halts', { inst_until: 1, reason: null, hosts: {} });
  assert.equal(timers.pendingCount(), 1);
  await S.set('ledger', { committed: [[5, 6]], reserved: [] }, { flush: true });
  assert.equal(rec.writes.length, 1);
  assert.deepEqual(readFile(fs, FILE).persisted.ledger.committed, [[5, 6]]);
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 1, 'the earlier unflushed change went along');
  assert.equal(timers.pendingCount(), 0);
  await S.dispose();
});

test('flushes are serialised: a second flush waits for the first write and nothing overlaps', async () => {
  const { S, fs, rec, hold } = env();
  await S.init();
  const release = hold();
  const first = S.set('halts', { inst_until: 1, reason: null, hosts: {} }, { flush: true });
  await settle();
  assert.equal(rec.writes.length, 1, 'first write started and is blocked');
  let secondDone = false;
  const second = S.set('halts', { inst_until: 2, reason: null, hosts: {} }, { flush: true }).then(() => { secondDone = true; });
  await settle();
  assert.equal(rec.writes.length, 1, 'the second write does not start while the first runs');
  assert.equal(secondDone, false);
  release();
  await Promise.all([first, second]);
  assert.equal(rec.writes.length, 2);
  assert.equal(rec.maxActive, 1);
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 2, 'last writer wins, in order');
  await S.dispose();
});

test('a change made during a write keeps the state dirty and is written afterwards', async () => {
  const { S, fs, timers, rec, hold } = env();
  await S.init();
  const release = hold();
  const flushing = S.set('halts', { inst_until: 1, reason: null, hosts: {} }, { flush: true });
  await settle();
  await S.set('halts', { inst_until: 2, reason: null, hosts: {} });
  release();
  await flushing;
  assert.equal(rec.writes.length, 1);
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 1, 'the in-flight write carried the old value');
  assert.equal(timers.pendingCount(), 1, 'the newer change armed the timer');
  timers.advance(2000);
  await settle();
  assert.equal(rec.writes.length, 2);
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 2);
  await S.dispose();
});

test('a failed write is logged, flush still resolves, and the next flush retries', async () => {
  const { S, fs, rec, zotero } = env();
  await S.init();
  rec.failWrites = 1;
  await S.set('halts', { inst_until: 4, reason: null, hosts: {} }, { flush: true });
  assert.equal(fs.files.has(FILE), false, 'nothing reached the disk');
  assert.ok(logged(zotero, /writing .* failed/));
  await S.flush();
  assert.equal(rec.writes.length, 2, 'retried');
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 4);
  await S.flush();
  assert.equal(rec.writes.length, 2, 'clean again');
  await S.dispose();
});

test('a failed debounced write is retried by the next set', async () => {
  const { S, fs, timers, rec } = env();
  await S.init();
  rec.failWrites = 1;
  await S.set('alma', { 'a': { held: true, at: 1 } });
  timers.advance(2000);
  await settle();
  assert.equal(fs.files.has(FILE), false);
  await S.set('alma', { 'a': { held: true, at: 1 }, 'b': { held: false, at: 2 } });
  timers.advance(2000);
  await settle();
  assert.deepEqual(Object.keys(readFile(fs, FILE).persisted.alma), ['a', 'b']);
  await S.dispose();
});

test('unknown keys and non-object values are ignored with a log line', async () => {
  const { S, zotero, rec } = env();
  await S.init();
  await S.set('bogus', { x: 1 }, { flush: true });
  await S.set('ledger', 'not an object', { flush: true });
  await S.set('ledger', null, { flush: true });
  await S.set('halts', [1, 2], { flush: true });
  await S.set('halts', undefined, { flush: true });
  assert.equal(rec.writes.length, 0);
  assert.deepEqual(plain(S.getPersisted()), DEFAULT_PERSISTED);
  assert.ok(logged(zotero, /unknown key/));
  await S.dispose();
});

test('registerTmpDir is on disk before it returns; unregisterTmpDir is lazy', async () => {
  const { S, fs, timers, rec } = env();
  await S.init();
  await S.registerTmpDir('/storage/tmp-abc');
  assert.equal(rec.writes.length, 1);
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, ['/storage/tmp-abc']);
  await S.registerTmpDir('/storage/tmp-abc');
  assert.equal(rec.writes.length, 1, 'registering twice is a no-op');
  await S.unregisterTmpDir('/storage/tmp-abc');
  assert.equal(rec.writes.length, 1, 'not written yet');
  timers.advance(2000);
  await settle();
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, []);
  await S.unregisterTmpDir('/never/registered');
  await assert.rejects(() => S.registerTmpDir(''), /non-empty string/);
  await S.dispose();
});

test('sweepTmp removes registered dirs and the children of tmp/, honours keep, leaves everything else', async () => {
  const { S, fs, rec } = env();
  await S.init();
  const a = '/storage/tmp-a';
  const b = '/storage/tmp-b';
  const kept = '/storage/tmp-kept';
  const missing = '/storage/tmp-missing';
  for (const dir of [a, b, kept]) await fs.IOUtils.write(dir + '/file.tmp', new Uint8Array([1, 2, 3]));
  for (const dir of [a, b, kept, missing]) await S.registerTmpDir(dir);
  await fs.IOUtils.write(TMP + '/stale1/file.tmp', new Uint8Array([1]));
  await fs.IOUtils.write(TMP + '/stale2.bin', new Uint8Array([1]));
  await fs.IOUtils.write('/storage/other/precious.pdf', new Uint8Array([9]));
  await fs.IOUtils.write(DIR + '/notes.txt', new Uint8Array([9]));

  const removed = await S.sweepTmp({ keep: [kept] });
  assert.equal(removed, 4, 'a, b and two children of tmp/; the missing registered dir is not counted');
  assert.equal(fs.files.has(a + '/file.tmp'), false);
  assert.equal(fs.files.has(b + '/file.tmp'), false);
  assert.equal(fs.files.has(kept + '/file.tmp'), true, 'kept');
  assert.equal(fs.files.has(TMP + '/stale1/file.tmp'), false);
  assert.equal(fs.files.has(TMP + '/stale2.bin'), false);
  assert.equal(fs.files.has('/storage/other/precious.pdf'), true, 'outside the registry and tmp/');
  assert.equal(fs.files.has(DIR + '/notes.txt'), true);
  assert.ok(fs.dirs.has(TMP), 'the tmp/ root itself stays');
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, [kept], 'the persisted list now holds only the kept dir');

  assert.equal(await S.sweepTmp(), 1, 'without keep the last one goes too');
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, []);
  assert.equal(await S.sweepTmp(), 0);
  const writesBefore = rec.writes.length;
  assert.equal(await S.sweepTmp(), 0);
  assert.equal(rec.writes.length, writesBefore, 'an empty sweep writes nothing');
  await S.dispose();
});

test('sweepTmp keeps a registration whose removal failed so the next sweep retries it', async () => {
  const { S, fs, zotero } = env();
  await S.init();
  await fs.IOUtils.write('/storage/tmp-locked/file.tmp', new Uint8Array([1]));
  await fs.IOUtils.write('/storage/tmp-free/file.tmp', new Uint8Array([1]));
  await S.registerTmpDir('/storage/tmp-locked');
  await S.registerTmpDir('/storage/tmp-free');
  const realRemove = fs.IOUtils.remove;
  fs.IOUtils.remove = async (p, o) => {
    if (p === '/storage/tmp-locked') throw new Error('sharing violation');
    return realRemove.call(fs.IOUtils, p, o);
  };
  assert.equal(await S.sweepTmp(), 1);
  assert.ok(logged(zotero, /could not remove \/storage\/tmp-locked/));
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, ['/storage/tmp-locked']);
  fs.IOUtils.remove = realRemove;
  assert.equal(await S.sweepTmp(), 1);
  assert.deepEqual(readFile(fs, FILE).tmp_dirs, []);
  await S.dispose();
});

test('sweepTmp treats backslash and slash spellings of a kept path as the same path', async () => {
  const { S, fs } = env();
  await S.init();
  await fs.IOUtils.write('C:/storage/tmp-k/file.tmp', new Uint8Array([1]));
  await S.registerTmpDir('C:/storage/tmp-k');
  assert.equal(await S.sweepTmp({ keep: ['C:\\storage\\tmp-k'] }), 0);
  assert.equal(fs.files.has('C:/storage/tmp-k/file.tmp'), true);
  await S.dispose();
});

test('nextRunId is monotonic, lazily persisted, and unique across a restart', async () => {
  const first = env();
  await first.S.init();
  assert.deepEqual([first.S.nextRunId(), first.S.nextRunId(), first.S.nextRunId()], [1, 2, 3]);
  assert.equal(first.rec.writes.length, 0, 'ids do not force a write');
  first.timers.advance(2000);
  await settle();
  assert.equal(first.rec.writes.length, 1);
  assert.equal(readFile(first.fs, FILE).run_counter, 3);
  assert.equal(first.S.nextRunId(), 4);
  await first.S.dispose();

  const second = env({ fs: first.fs });
  await second.S.init();
  assert.equal(second.S.nextRunId(), 5);
  await second.S.dispose();
});

test('reset restores defaults for the given keys (all by default) and flushes', async () => {
  const { S, fs, rec, zotero } = env();
  await S.init();
  await S.set('halts', { inst_until: 9, reason: 'x', hosts: { 'a.com': 1 } });
  await S.set('ledger', { committed: [[1, 2]], reserved: [] });
  await S.set('ezproxy', { 'a.com': { configured: true, at: 1 } });
  await S.reset(['halts', 'nope']);
  assert.ok(logged(zotero, /reset: unknown key/));
  assert.deepEqual(readFile(fs, FILE).persisted.halts, DEFAULT_PERSISTED.halts);
  assert.deepEqual(readFile(fs, FILE).persisted.ledger.committed, [[1, 2]], 'other keys untouched');
  await S.reset();
  assert.deepEqual(readFile(fs, FILE).persisted, DEFAULT_PERSISTED);
  const writes = rec.writes.length;
  await S.reset([]);
  assert.equal(rec.writes.length, writes, 'an empty key list does nothing');
  await S.dispose();
});

test('dispose flushes pending changes, clears the timer, and forgets the loaded file', async () => {
  const { S, fs, timers } = env();
  await S.init();
  await S.set('halts', { inst_until: 8, reason: null, hosts: {} });
  assert.equal(timers.pendingCount(), 1);
  await S.dispose();
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 8);
  assert.equal(timers.pendingCount(), 0);
  assert.throws(() => S.getPersisted(), /before init/);
  assert.throws(() => S.nextRunId(), /before init/);
  assert.throws(() => S.dataDir(), /before init/);
  await S.set('halts', { inst_until: 1, reason: null, hosts: {} });
  assert.equal(await S.sweepTmp(), 0);
  await S.flush();
  await S.dispose();
});

test('dispose waits for a write that is still running', async () => {
  const { S, fs, hold } = env();
  await S.init();
  const release = hold();
  const pending = S.set('halts', { inst_until: 6, reason: null, hosts: {} }, { flush: true });
  await settle();
  let disposed = false;
  const disposing = S.dispose().then(() => { disposed = true; });
  await settle();
  assert.equal(disposed, false);
  release();
  await Promise.all([pending, disposing]);
  assert.equal(readFile(fs, FILE).persisted.halts.inst_until, 6);
});

test('init failure is reported and a later init can succeed', async () => {
  const { S, fs } = env();
  const realMake = fs.IOUtils.makeDirectory;
  fs.IOUtils.makeDirectory = async () => { throw new Error('read-only profile'); };
  await assert.rejects(() => S.init(), /read-only profile/);
  fs.IOUtils.makeDirectory = realMake;
  await S.init();
  assert.equal(S.dataDir(), DIR);
  await S.dispose();
});

test('the state module exposes its documented constants', () => {
  const { S } = env();
  assert.equal(S.FILE_NAME, 'state.json');
  assert.equal(S.VERSION, 1);
  assert.deepEqual(plain(S.PERSIST_KEYS), ['ledger', 'halts', 'ezproxy', 'alma']);
});
