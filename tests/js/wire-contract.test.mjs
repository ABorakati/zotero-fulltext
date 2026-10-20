// JS <-> Rust wire contract, checked from both sides.
//  (a) static: every Effect variant of crates/fulltext-engine/src/proto.rs has a case in the JS effect runner (and the
//      runner has no case the engine cannot emit); every Command variant is built by some plugin script; the plain
//      enums JS mirrors (Mode, UserActionKind) match.
//  (b) dynamic (needs plugin/content/fulltext.wasm): the commands the REAL JS builds (real prefs -> EngineConfig, real
//      state.getPersisted(), real items.snapshot, real runs.js command construction) are fed to the REAL Rust engine.
//      serde rejects a whole StepIn on one malformed command, so any field-name or shape drift fails here.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { ROOT, CONTENT, WASM_PATH, loadPlugin, plain, wasmBuilt, walk } from './helpers/zft-env.mjs';
import { makeFakeFs, makeFakeTimers, makeFakeZotero } from './helpers/fakes.mjs';

const PROTO = fs.readFileSync(path.join(ROOT, 'crates', 'fulltext-engine', 'src', 'proto.rs'), 'utf8');
const snake = (s) => s.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();

/** Variant names of `pub enum <name> { ... }` (data-carrying variants included), in source order. */
function enumVariants(src, name) {
  const start = src.indexOf(`pub enum ${name} {`);
  assert.ok(start >= 0, `enum ${name} not found in proto.rs`);
  const open = src.indexOf('{', start);
  let depth = 0;
  let end = open;
  for (; end < src.length; end++) {
    if (src[end] === '{' || src[end] === '(' || src[end] === '[') depth++;
    else if (src[end] === '}' || src[end] === ')' || src[end] === ']') { depth--; if (depth === 0) break; }
  }
  const body = src.slice(open + 1, end).replace(/\/\/[^\n]*/g, '');
  const segments = [];
  let seg = '';
  depth = 0;
  for (const ch of body) {
    if (ch === '{' || ch === '(' || ch === '[') depth++;
    else if (ch === '}' || ch === ')' || ch === ']') depth--;
    if (ch === ',' && depth === 0) { segments.push(seg); seg = ''; } else seg += ch;
  }
  segments.push(seg);
  return segments
    .map((s) => s.replace(/#\[[^\]]*\]/g, '').trim())
    .filter(Boolean)
    .map((s) => s.match(/^([A-Z][A-Za-z0-9]*)/)?.[1])
    .filter(Boolean);
}

/** Variants of a `plain_enum!(<name> { A, B })` declaration. */
function plainEnum(name) {
  const m = PROTO.match(new RegExp(`plain_enum!\\(\\s*${name}\\s*\\{([^}]*)\\}\\)`));
  assert.ok(m, `plain_enum ${name} not found in proto.rs`);
  return m[1].split(',').map((s) => s.trim()).filter(Boolean);
}

const EFFECTS_JS = fs.readFileSync(path.join(CONTENT, 'effects.js'), 'utf8');

test('every Effect variant of proto.rs has a case in the JS effect runner, and nothing else does', () => {
  const variants = enumVariants(PROTO, 'Effect').map(snake);
  assert.ok(variants.length >= 15, `parsed only ${variants.length} Effect variants: ${variants}`);
  const sw = EFFECTS_JS.indexOf('switch (eff.type) {');
  assert.ok(sw >= 0, 'dispatch switch not found in effects.js');
  const dflt = EFFECTS_JS.indexOf('default:', sw);
  assert.ok(dflt > sw, 'dispatch switch has no default');
  const cases = [...EFFECTS_JS.slice(sw, dflt).matchAll(/case '([a-z_]+)':/g)].map((m) => m[1]);
  assert.deepEqual([...cases].sort(), [...variants].sort());
});

test('every Command variant of proto.rs is built by a plugin script, and the mirrored enums match', async () => {
  const commands = enumVariants(PROTO, 'Command').map(snake);
  assert.deepEqual(commands.sort(), ['cancel_item', 'cancel_run', 'configure', 'dump_state', 'shutdown', 'start_run', 'user_action']);
  const sources = walk(CONTENT, /\.js$/).map((f) => fs.readFileSync(f, 'utf8')).join('\n');
  for (const c of commands) assert.ok(sources.includes(`type: '${c}'`), `no plugin script builds a '${c}' command`);

  const { ZFT } = loadPlugin({ files: ['util.js', 'state.js', 'items.js', 'runs.js'], globals: { Zotero: makeFakeZotero() } });
  assert.deepEqual([...ZFT.runs.MODES].sort(), plainEnum('Mode').map(snake).sort());
  assert.deepEqual([...ZFT.runs.USER_ACTIONS].sort(), plainEnum('UserActionKind').map(snake).sort());
});

// ------------------------------------------------------------------------------------------------------------
// (b) the real engine in fulltext.wasm accepts what the real JS sends
// ------------------------------------------------------------------------------------------------------------
const REAL = wasmBuilt() ? {} : { skip: `${path.relative(process.cwd(), WASM_PATH)} is missing: run "cargo xtask build"` };

function fakeItem() {
  const fields = { title: 'Cold-stored whole blood in trauma resuscitation', DOI: '10.1097/SLA.0000000000005603', PMID: '', PMCID: '', archiveID: '', extra: '', url: '' };
  return {
    id: 1201, key: 'Q7ZK2M4P', libraryID: 1, itemType: 'journalArticle', isFeedItem: false,
    _loaded: { itemData: true, childItems: true },
    isRegularItem: () => true,
    getField: (name) => fields[name] ?? '',
    getDisplayTitle: () => fields.title,
    numFileAttachmentsWithContentType: () => 0,
    async getBestAttachmentState() { return { type: 'none' }; },
    async loadDataType() {},
  };
}

test('commands built by the real JS controller are accepted by the real engine', REAL, async () => {
  const fsx = makeFakeFs();
  const timers = makeFakeTimers();
  const zotero = makeFakeZotero({
    Libraries: { get: () => ({ libraryType: 'user', filesEditable: true, name: 'My Library' }) },
    Items: { get: () => false },
    Notifier: { registerObserver: () => 'obs', unregisterObserver() {} },
  });
  const { ZFT } = loadPlugin({
    files: ['util.js', 'config.js', 'wasm-host.js', 'state.js', 'items.js', 'runs.js'],
    globals: { Zotero: zotero, IOUtils: fsx.IOUtils, PathUtils: fsx.PathUtils },
  });
  const host = await ZFT.wasmHost.load({ bytes: fs.readFileSync(WASM_PATH), inspectInWorker: false, timers });

  /** Every runner.send() argument, in order (one StepIn each). */
  const batches = [];
  const runner = {
    needsConfigure: true,
    send(commands) { batches.push(plain(commands)); },
    async pump() {}, async whenQuiet() {}, async dispose() {},
  };
  const quiet = { open() {}, closeAll() {}, disposeAll() {} };
  await ZFT.runs.init({
    host, createRunner: () => runner, timers: { setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout },
    progress: quiet, login: quiet, pdfCapture: quiet, bodies: { async releaseAll() {} }, services: { prompt: { alert() {} } },
  });

  const handle = await ZFT.runs.startRun({ items: [fakeItem()], mode: 'full', interactive: true });
  for (const kind of ZFT.runs.USER_ACTIONS) ZFT.runs.userAction(handle.run, kind);
  ZFT.runs.cancelItem(handle.run, 1201);
  ZFT.runs.cancelRun(handle.run);
  await ZFT.runs.shutdown();

  const sent = batches.flat();
  assert.deepEqual(
    sent.map((c) => c.type),
    ['configure', 'start_run', 'user_action', 'user_action', 'user_action', 'user_action', 'cancel_item', 'cancel_run', 'cancel_run', 'shutdown'],
  );
  // effects.js asks for a state dump when it suspects a stall; its command is a constant.
  assert.ok(EFFECTS_JS.includes("{ type: 'dump_state' }"), 'effects.js no longer sends the dump_state literal this test replays');
  const dumpAt = batches.length - 1; // before the final shutdown batch
  batches.splice(dumpAt, 0, [{ type: 'dump_state' }]);

  let now = 1000;
  const outs = [];
  for (const commands of batches) {
    now += 10;
    // host.step throws EngineError (serde failure, invariant, ...) when the engine refuses the StepIn.
    outs.push(plain(host.step({ now, wall: 1792496400000 + now, commands, completions: [] })));
  }
  const startIdx = batches.findIndex((b) => b.some((c) => c.type === 'start_run'));
  assert.ok(batches.slice(0, startIdx + 1).some((b) => b.some((c) => c.type === 'configure')), 'the engine is configured before its first start_run');
  const progress = outs[startIdx].effects.filter((e) => e.type === 'progress');
  assert.ok(progress.length >= 1, `start_run produced no progress row: ${JSON.stringify(outs[startIdx].effects.map((e) => e.type))}`);
  assert.ok(progress.every((e) => e.item === 1201 && e.run === handle.run), 'progress rows carry the run and item ids JS sent');
  for (const out of outs) {
    assert.ok(Array.isArray(out.effects), 'every StepOut has an effects array');
    assert.equal(out.stats.lost_wakes ?? 0, 0);
  }
});

test('negative control: the engine refuses a command the contract does not define', REAL, async () => {
  const { ZFT } = loadPlugin({ files: ['util.js', 'config.js', 'wasm-host.js'], globals: { Zotero: makeFakeZotero() } });
  const host = await ZFT.wasmHost.load({ bytes: fs.readFileSync(WASM_PATH), inspectInWorker: false, timers: makeFakeTimers() });
  for (const bad of [
    { type: 'user_action', run: 1, kind: 'bogus' },
    { type: 'cancel_run', run: -1 },
    { type: 'cancel_everything' },
    { type: 'cancel_item', run: 1 },
  ]) {
    assert.throws(() => host.step({ now: 1, wall: 1, commands: [bad], completions: [] }), undefined, JSON.stringify(bad));
  }
});
