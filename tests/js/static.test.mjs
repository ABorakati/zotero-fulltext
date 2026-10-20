// Static checks of the plugin tree: syntax of standalone scripts, well-formed XHTML, manifest contract, chrome:// references.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { PLUGIN, CONTENT, makeContext, runScript, walk } from './helpers/zft-env.mjs';
import { checkXml } from './helpers/xml-check.mjs';
import { STANDALONE_SCRIPTS, XHTML_FILES } from './helpers/api-manifest.mjs';

test('manifest.json satisfies the Zotero 10 plugin contract', () => {
  const m = JSON.parse(fs.readFileSync(path.join(PLUGIN, 'manifest.json'), 'utf8'));
  assert.equal(m.manifest_version, 2);
  assert.match(m.version, /^\d+\.\d+\.\d+$/);
  const z = m.applications.zotero;
  assert.equal(z.id, 'zotero-fulltext@aborakati');
  assert.equal(z.update_url, 'https://raw.githubusercontent.com/ABorakati/zotero-fulltext/main/updates.json');
  assert.equal(z.strict_min_version, '10.0');
  assert.equal(z.strict_max_version, '10.*');
  for (const p of Object.values(m.icons || {})) assert.ok(fs.existsSync(path.join(PLUGIN, p)), 'icon ' + p);
});

test('namespace ID equals the manifest id', () => {
  const m = JSON.parse(fs.readFileSync(path.join(PLUGIN, 'manifest.json'), 'utf8'));
  const ctx = makeContext();
  runScript(ctx, 'namespace.js');
  assert.equal(ctx.ZFT.ID, m.applications.zotero.id);
});

test('every LOAD_ORDER file exists and is a classic script (no import/export, no top-level let/const/class)', () => {
  const ctx = makeContext();
  runScript(ctx, 'namespace.js');
  for (const f of ctx.ZFT.LOAD_ORDER) {
    const file = path.join(CONTENT, f);
    assert.ok(fs.existsSync(file), f + ' missing');
    const src = fs.readFileSync(file, 'utf8');
    assert.doesNotMatch(src, /^\s*(import|export)\s/m, f + ' uses ES module syntax');
    // Strip block comments, then require the first code token to open the IIFE.
    const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '').trim();
    assert.match(code, /^\(function \(global\) \{/, f + ' must be a single IIFE');
    assert.match(code, /\}\)\(this\);\s*$/, f + ' must end with })(this);');
  }
});

test('standalone scripts parse', () => {
  for (const rel of STANDALONE_SCRIPTS) {
    const file = path.join(PLUGIN, rel);
    assert.ok(fs.existsSync(file), rel + ' missing');
    assert.doesNotThrow(() => new vm.Script(fs.readFileSync(file, 'utf8'), { filename: file }), rel);
  }
});

test('XHTML files are well-formed', () => {
  for (const rel of XHTML_FILES) {
    const problems = checkXml(fs.readFileSync(path.join(PLUGIN, rel), 'utf8'));
    assert.deepEqual(problems, [], rel);
  }
});

test('every chrome://zotero-fulltext/content/ reference resolves to a file (wasm is a build output)', () => {
  const files = [...walk(PLUGIN, /\.(js|xhtml|json)$/)];
  const missing = [];
  for (const f of files) {
    const text = fs.readFileSync(f, 'utf8');
    for (const m of text.matchAll(/chrome:\/\/zotero-fulltext\/content\/([A-Za-z0-9_./-]+)/g)) {
      const rel = m[1];
      if (rel === 'fulltext.wasm' || rel.endsWith('/')) continue;
      if (!fs.existsSync(path.join(CONTENT, rel))) missing.push(`${path.relative(PLUGIN, f)} -> ${rel}`);
    }
  }
  assert.deepEqual(missing, []);
});

test('window scripts and xhtml agree on element ids', () => {
  const login = fs.readFileSync(path.join(CONTENT, 'login.xhtml'), 'utf8');
  for (const id of ['zft-login-browser', 'zft-login-waiting', 'zft-login-skip', 'zft-login-handoff', 'zft-login-keep', 'zft-login-popup-box']) {
    assert.ok(login.includes(`id="${id}"`), id);
  }
  const prog = fs.readFileSync(path.join(CONTENT, 'progress.xhtml'), 'utf8');
  for (const id of ['zft-progress-header', 'zft-progress-rows', 'zft-progress-cancel', 'zft-progress-close', 'zft-progress-copy']) {
    assert.ok(prog.includes(`id="${id}"`), id);
  }
});
