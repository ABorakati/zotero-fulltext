// ZFT.confirm.ask: Services.prompt.confirmEx with three buttons mapped onto proto.rs ConfirmChoice.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';

// nsIPromptService constants (positions are multipliers, titles are small ints).
const CONSTANTS = {
  BUTTON_POS_0: 1, BUTTON_POS_1: 256, BUTTON_POS_2: 65536,
  BUTTON_TITLE_IS_STRING: 127,
  BUTTON_POS_0_DEFAULT: 0, BUTTON_POS_1_DEFAULT: 16777216, BUTTON_POS_2_DEFAULT: 33554432,
};

/** @param {{answer?: any, throws?: boolean, constants?: boolean, mainWindow?: any, onPrompt?: Function}} [o] */
function setup(o = {}) {
  const calls = [];
  const prompt = {
    ...(o.constants === false ? {} : CONSTANTS),
    confirmEx(...args) {
      calls.push(args);
      if (o.onPrompt) o.onPrompt(args);
      if (o.throws) throw new Error('no prompt service');
      return typeof o.answer === 'function' ? o.answer(args) : o.answer;
    },
  };
  const zotero = makeFakeZotero({ getMainWindow: () => (o.mainWindow === undefined ? { name: 'main' } : o.mainWindow) });
  const { ZFT } = loadPlugin({ globals: { Zotero: zotero, Services: { prompt } }, files: ['util.js', 'confirm.js'] });
  return { ZFT, calls, zotero };
}

const LARGE = { kind: 'large_batch', n: 34, cap: 20, library: '' };
const GROUP = { kind: 'group_library', n: 0, cap: 0, library: 'Shared lab' };

/** Decode confirmEx flags into [{title, isDefault}] per button position. */
function decodeFlags(flags) {
  const positions = [1, 256, 65536];
  const defaults = [0, 16777216, 33554432];
  const titles = positions.map((p) => Math.floor(flags / p) % 256);
  const dflt = defaults.findIndex((d, i) => i > 0 && (flags & d) !== 0);
  return { titles, defaultButton: dflt === -1 ? 0 : dflt, extra: flags - titles.reduce((acc, t, i) => acc + t * positions[i], 0) - (dflt > 0 ? defaults[dflt] : 0) };
}

test('large batch: three string buttons, localized text with {n, cap}, Continue is the default', async () => {
  const { ZFT, calls } = setup({ answer: 0 });
  const r = await ZFT.confirm.ask(LARGE, { scope: new ZFT.util.CancelScope() });
  assert.deepEqual(plain(r), { choice: 'yes' });
  assert.equal(calls.length, 1);
  const [parent, title, text, flags, b0, b1, b2, checkMsg, checkState] = calls[0];
  assert.deepEqual(plain(parent), { name: 'main' });
  assert.equal(title, 'zotero-fulltext-confirm-large-batch-title {n=34, cap=20}');
  assert.equal(text, 'zotero-fulltext-confirm-large-batch-body {n=34, cap=20}');
  assert.deepEqual([b0, b1, b2], ['zotero-fulltext-confirm-yes', 'zotero-fulltext-confirm-oa-only', 'zotero-fulltext-confirm-cancel']);
  assert.equal(checkMsg, null);
  assert.deepEqual(plain(checkState), {});
  const d = decodeFlags(flags);
  assert.deepEqual(d.titles, [127, 127, 127], 'all three buttons use string titles');
  assert.equal(d.defaultButton, 0);
  assert.equal(d.extra, 0, 'no stray bits');
});

test('group library: Save anyway / Open access only / Cancel with {library}; Enter does not save into the group', async () => {
  const { ZFT, calls } = setup({ answer: 1 });
  const r = await ZFT.confirm.ask(GROUP, { scope: new ZFT.util.CancelScope() });
  assert.deepEqual(plain(r), { choice: 'oa_only' });
  const [, title, text, flags, b0, b1, b2] = calls[0];
  assert.equal(title, 'zotero-fulltext-confirm-group-title {library=Shared lab}');
  assert.equal(text, 'zotero-fulltext-confirm-group-body {library=Shared lab}');
  assert.deepEqual([b0, b1, b2], ['zotero-fulltext-confirm-save-anyway', 'zotero-fulltext-confirm-oa-only', 'zotero-fulltext-confirm-cancel']);
  const d = decodeFlags(flags);
  assert.deepEqual(d.titles, [127, 127, 127]);
  assert.equal(d.defaultButton, 1);
});

test('the returned button index maps to yes / oa_only / cancel; anything else is cancel', async () => {
  const expectations = [[0, 'yes'], [1, 'oa_only'], [2, 'cancel'], [-1, 'cancel'], [3, 'cancel'], [undefined, 'cancel'], [null, 'cancel'], ['1', 'cancel'], [NaN, 'cancel']];
  for (const kind of [LARGE, GROUP]) {
    for (const [answer, choice] of expectations) {
      const { ZFT } = setup({ answer });
      const r = await ZFT.confirm.ask(kind, { scope: new ZFT.util.CancelScope() });
      assert.deepEqual(plain(r), { choice }, `${kind.kind} ${String(answer)}`);
    }
  }
});

test('Services.prompt throwing resolves cancel instead of rejecting', async () => {
  const { ZFT, zotero } = setup({ throws: true });
  const r = await ZFT.confirm.ask(LARGE, { scope: new ZFT.util.CancelScope() });
  assert.deepEqual(plain(r), { choice: 'cancel' });
  assert.ok(zotero.debugLog.some(([, m]) => /prompt failed/.test(m)));
});

test('an unknown kind (or no request) never prompts and answers cancel', async () => {
  const { ZFT, calls } = setup({ answer: 0 });
  for (const req of [{ kind: 'nuke_everything' }, { kind: 'constructor' }, { kind: '__proto__' }, {}, null, undefined]) {
    const r = await ZFT.confirm.ask(req, { scope: new ZFT.util.CancelScope() });
    assert.deepEqual(plain(r), { choice: 'cancel' });
  }
  assert.equal(calls.length, 0);
});

test('the modal opens one macrotask later so the runner can finish dispatching the step first', async () => {
  const { ZFT, calls } = setup({ answer: 0 });
  const p = ZFT.confirm.ask(LARGE, { scope: new ZFT.util.CancelScope() });
  assert.equal(calls.length, 0, 'nothing synchronous');
  await Promise.resolve();
  assert.equal(calls.length, 0, 'not even a microtask');
  await p;
  assert.equal(calls.length, 1);
});

test('cancellation: before the prompt it never opens; during the prompt the answer is discarded', async () => {
  {
    const { ZFT, calls } = setup({ answer: 0 });
    const scope = new ZFT.util.CancelScope();
    scope.cancel('user');
    await assert.rejects(ZFT.confirm.ask(LARGE, { scope }), (e) => e.kind === 'cancelled');
    assert.equal(calls.length, 0);
  }
  {
    const { ZFT, calls } = setup({ answer: 0 });
    const scope = new ZFT.util.CancelScope();
    const p = ZFT.confirm.ask(LARGE, { scope });
    scope.cancel('user'); // before the macrotask: the prompt is skipped
    await assert.rejects(p, (e) => e.kind === 'cancelled');
    assert.equal(calls.length, 0);
  }
  {
    const scope0 = {};
    const { ZFT, calls } = setup({ answer: 0, onPrompt: () => scope0.s.cancel('shutdown') });
    scope0.s = new ZFT.util.CancelScope();
    await assert.rejects(ZFT.confirm.ask(LARGE, { scope: scope0.s }), (e) => e.kind === 'cancelled');
    assert.equal(calls.length, 1, 'the dialog was open when the run was cancelled; its answer is dropped');
  }
});

test('parent window: ctx.win wins, then the main window, then null', async () => {
  const win = { name: 'ctx' };
  {
    const { ZFT, calls } = setup({ answer: 2 });
    await ZFT.confirm.ask(LARGE, { scope: new ZFT.util.CancelScope(), win });
    assert.equal(calls[0][0], win);
  }
  {
    const { ZFT, calls } = setup({ answer: 2, mainWindow: null });
    await ZFT.confirm.ask(LARGE, { scope: new ZFT.util.CancelScope() });
    assert.equal(calls[0][0], null);
  }
  {
    const { ZFT, calls } = setup({ answer: 2 });
    await ZFT.confirm.ask(LARGE, undefined);
    assert.deepEqual(plain(calls[0][0]), { name: 'main' });
  }
});

test('flags fall back to the documented nsIPromptService values when the service object carries no constants', async () => {
  const withConst = setup({ answer: 0 });
  await withConst.ZFT.confirm.ask(GROUP, { scope: new withConst.ZFT.util.CancelScope() });
  const bare = setup({ answer: 0, constants: false });
  await bare.ZFT.confirm.ask(GROUP, { scope: new bare.ZFT.util.CancelScope() });
  assert.equal(bare.calls[0][3], withConst.calls[0][3]);
});

test('numeric arguments are coerced: garbage never reaches the message', async () => {
  const { ZFT, calls } = setup({ answer: 0 });
  await ZFT.confirm.ask({ kind: 'large_batch', n: 'many', cap: undefined, library: 'x' }, { scope: new ZFT.util.CancelScope() });
  assert.equal(calls[0][1], 'zotero-fulltext-confirm-large-batch-title {n=0, cap=0}');
});
