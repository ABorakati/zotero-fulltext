// ZFT.util is implemented in the skeleton: these tests are the contract for CancelScope, TaskSet, OpError, sleep.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';

function env() {
  const zotero = makeFakeZotero();
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero } });
  return { U: ZFT.util, zotero };
}

test('OpError maps onto proto.rs OpErr and unknown kinds become internal', () => {
  const { U } = env();
  assert.deepEqual(plain(new U.OpError('timeout', 'slow', { afterMs: 9000, status: 504 }).toOpErr()), { kind: 'timeout', message: 'slow', after_ms: 9000, status: 504 });
  assert.equal(new U.OpError('nonsense').kind, 'internal');
  for (const k of ['timeout', 'network', 'cancelled', 'too_large', 'tls', 'unsupported', 'offline', 'already_has_pdf', 'not_editable', 'parent_gone', 'io', 'internal']) {
    assert.ok(U.OP_ERR_KINDS.includes(k), k);
  }
});

test('CancelScope: callbacks once, children follow parents, late registration fires immediately', () => {
  const { U } = env();
  const parent = new U.CancelScope();
  const child = parent.child();
  const grandchild = child.child();
  const seen = [];
  child.onCancel((r) => seen.push('child:' + r));
  const off = grandchild.onCancel(() => seen.push('grandchild'));
  off();
  parent.cancel('user');
  parent.cancel('again');
  assert.deepEqual(seen, ['child:user']);
  assert.equal(grandchild.cancelled, true);
  assert.equal(grandchild.reason, 'user');
  let late = null;
  grandchild.onCancel((r) => { late = r; });
  assert.equal(late, 'user');
  assert.throws(() => grandchild.throwIfCancelled(), /user/);
  assert.equal(new U.CancelScope(parent).cancelled, true, 'child of a cancelled parent starts cancelled');
});

test('CancelScope: a throwing callback does not stop the others', () => {
  const { U } = env();
  const s = new U.CancelScope();
  const seen = [];
  s.onCancel(() => { throw new Error('boom'); });
  s.onCancel(() => seen.push('ok'));
  s.cancel();
  assert.deepEqual(seen, ['ok']);
});

test('sleep resolves, and rejects with cancelled when the scope is cancelled first', async () => {
  const { U } = env();
  await U.sleep(5);
  const s = new U.CancelScope();
  const p = U.sleep(10_000, s);
  s.cancel('stop');
  await assert.rejects(p, (e) => e.kind === 'cancelled');
  await assert.rejects(U.sleep(1, s), (e) => e.kind === 'cancelled');
});

test('TaskSet never rejects, logs failures, and whenIdle waits for all tasks', async () => {
  const { U, zotero } = env();
  const tasks = new U.TaskSet('t');
  const order = [];
  tasks.spawn('ok', async () => { await U.sleep(5); order.push('ok'); });
  tasks.spawn('bad', async () => { throw new Error('kaboom'); });
  assert.equal(tasks.size, 2);
  await tasks.whenIdle();
  assert.deepEqual(order, ['ok']);
  assert.equal(tasks.size, 0);
  assert.ok(zotero.debugLog.some(([, m]) => /t\/bad failed: Error: kaboom/.test(m)));
  await tasks.whenIdle();
});

test('monoNow never decreases and is an integer', () => {
  const { U } = env();
  let last = -1;
  for (let i = 0; i < 1000; i++) {
    const t = U.monoNow();
    assert.ok(Number.isInteger(t) && t >= last);
    last = t;
  }
});

test('byte helpers', () => {
  const { U } = env();
  assert.deepEqual([...U.concatBytes([new Uint8Array([1, 2]), new Uint8Array([3])])], [1, 2, 3]);
  assert.equal(U.toU8(new ArrayBuffer(4)).length, 4);
  const view = new Uint8Array([9, 8, 7, 6]);
  assert.deepEqual([...U.toU8(new DataView(view.buffer, 1, 2))], [8, 7]);
  assert.deepEqual(plain(U.parseJson('{bad', { x: 1 })), { x: 1 });
});
