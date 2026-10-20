// Fluent contract: the FTL is well-formed enough, ids are unique, and EVERY "zotero-fulltext-..." string literal in plugin/
// and in the Rust crates (engine emits l10n ids) resolves to a message. Cross-area gate: keeps engine and UI in step.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { ROOT, PLUGIN, walk } from './helpers/zft-env.mjs';

const FTL_PATH = path.join(PLUGIN, 'locale', 'en-US', 'zotero-fulltext.ftl');

/** Minimal Fluent reader: message ids at column 0, attributes ('.name =') and continuation lines. */
export function readFtl(text) {
  const messages = new Map();
  let current = null;
  for (const [idx, raw] of text.split(/\r?\n/).entries()) {
    if (/^\s*(#|$)/.test(raw)) { if (/^\S/.test(raw) || raw.trim() === '') continue; else continue; }
    const m = raw.match(/^([a-z][a-z0-9-]*)\s*=\s*(.*)$/);
    if (m) {
      assert.ok(!messages.has(m[1]), `duplicate message id ${m[1]} (line ${idx + 1})`);
      current = { id: m[1], value: m[2], attrs: [], line: idx + 1 };
      messages.set(m[1], current);
      continue;
    }
    assert.ok(/^\s+/.test(raw), `line ${idx + 1} is not indented continuation text: ${raw}`);
    assert.ok(current, `continuation before any message (line ${idx + 1})`);
    const a = raw.match(/^\s+\.([a-z][a-z0-9-]*)\s*=\s*(.*)$/);
    if (a) current.attrs.push(a[1]);
    else current.value += '\n' + raw.trim();
  }
  return messages;
}

test('FTL parses; every message has a value or an attribute; ids use the plugin prefix', () => {
  const messages = readFtl(fs.readFileSync(FTL_PATH, 'utf8'));
  assert.ok(messages.size > 100);
  for (const msg of messages.values()) {
    assert.match(msg.id, /^zotero-fulltext-/, msg.id);
    assert.ok(msg.value.trim() !== '' || msg.attrs.length > 0, `${msg.id} is empty`);
  }
});

test('menu messages carry a .label attribute (MenuManager l10nID)', () => {
  const messages = readFtl(fs.readFileSync(FTL_PATH, 'utf8'));
  for (const id of ['zotero-fulltext-menu-get', 'zotero-fulltext-menu-get-oa', 'zotero-fulltext-menu-signin', 'zotero-fulltext-menu-progress', 'zotero-fulltext-menu-prefs']) {
    assert.ok(messages.get(id)?.attrs.includes('label'), id);
  }
});

test('every quoted zotero-fulltext-* l10n id used in plugin/ or crates/ exists in the FTL', () => {
  const messages = readFtl(fs.readFileSync(FTL_PATH, 'utf8'));
  const sources = [
    ...walk(PLUGIN, /\.(js|xhtml)$/),
    ...walk(path.join(ROOT, 'crates'), /\.rs$/),
  ];
  const used = new Map();
  for (const f of sources) {
    const text = fs.readFileSync(f, 'utf8');
    for (const m of text.matchAll(/["'`](zotero-fulltext-[a-z0-9]+(?:-[a-z0-9]+)*)["'`]/g)) {
      if (!used.has(m[1])) used.set(m[1], path.relative(ROOT, f));
    }
    // data-l10n-id="..." in markup
    for (const m of text.matchAll(/data-l10n-id="(zotero-fulltext-[a-z0-9-]+)"/g)) {
      if (!used.has(m[1])) used.set(m[1], path.relative(ROOT, f));
    }
  }
  const missing = [...used].filter(([id]) => !messages.has(id)).map(([id, f]) => `${id} (${f})`);
  assert.deepEqual(missing, []);
});

test('the outcome/status catalogue of plugin-js.md section 8 is present', () => {
  const messages = readFtl(fs.readFileSync(FTL_PATH, 'utf8'));
  const required = [
    'zotero-fulltext-status-searching-oa', 'zotero-fulltext-status-waiting-login', 'zotero-fulltext-status-saved-oa',
    'zotero-fulltext-status-saved-inst', 'zotero-fulltext-status-skipped-has-pdf', 'zotero-fulltext-status-no-identifier',
    'zotero-fulltext-status-not-found', 'zotero-fulltext-status-login-declined', 'zotero-fulltext-status-blocked',
    'zotero-fulltext-status-cap-reached', 'zotero-fulltext-status-cancelled', 'zotero-fulltext-status-failed',
    'zotero-fulltext-action-open-browser', 'zotero-fulltext-action-open-libkey', 'zotero-fulltext-action-open-redirector',
    'zotero-fulltext-action-sign-in', 'zotero-fulltext-action-retry',
  ];
  for (const id of required) assert.ok(messages.has(id), id);
});

test('the FTL is pure ASCII: Zotero 10.0.6 silently fails to start a plugin whose FTL has a character above U+00FF', () => {
  // registerLocales() (xpcom/plugins.js) hands the file text to L10nFileSource.createMock, which needs byte strings. A
  // single ellipsis made Zotero.Plugins.init() stop before the plugin was started and logged nothing (found with the
  // harness, 2026-10-20). Latin-1 characters would be decoded wrongly too, so the rule is ASCII only: write special
  // characters as Fluent string placeables with a unicode escape instead of typing them.
  const buf = fs.readFileSync(FTL_PATH);
  assert.notDeepEqual([...buf.subarray(0, 3)], [0xef, 0xbb, 0xbf], 'no byte order mark');
  const offenders = buf.toString('utf8').split(/\r?\n/)
    .map((line, i) => [i + 1, line])
    .filter(([, line]) => /[^\x00-\x7F]/.test(line))
    .map(([n, line]) => `line ${n}: ${line.trim().slice(0, 60)}`);
  assert.deepEqual(offenders, []);
});

test('Fluent unicode escapes in the FTL are well-formed string placeables', () => {
  const text = fs.readFileSync(FTL_PATH, 'utf8');
  const bs = String.fromCharCode(92);
  for (const [n, line] of text.split(/\r?\n/).entries()) {
    if (!line.includes(bs)) continue;
    // Every backslash must start a \uXXXX escape inside a quoted placeable: { "\uXXXX" }
    const stripped = line.split(bs + 'u').slice(1);
    assert.ok(stripped.length > 0, `line ${n + 1}: unexpected backslash`);
    assert.match(line, /\{ "(?:[^"\\]|\\u[0-9A-Fa-f]{4})+" \}/, `line ${n + 1}: escape outside a string placeable`);
    for (const rest of stripped) assert.match(rest, /^[0-9A-Fa-f]{4}/, `line ${n + 1}: bad unicode escape`);
  }
});
