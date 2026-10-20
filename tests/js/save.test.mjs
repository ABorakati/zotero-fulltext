// ZFT.save: Effect::SavePdf into Zotero storage plus the manual attach hand-off. Fakes: Zotero.Attachments/File/Libraries,
// an in-memory IOUtils, a BodyStore stand-in (get() returns the live record), ZFT.items and ZFT.state recorders.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin, plain } from './helpers/zft-env.mjs';
import { makeFakeZotero, makeFakeFs } from './helpers/fakes.mjs';

const bytes = (s) => new TextEncoder().encode(s);
const text = (u8) => new TextDecoder().decode(u8);
const flush = () => new Promise((resolve) => setImmediate(resolve));

const DIR = '/storage/tmp-AbC123';
const FILE = DIR + '/file.tmp';

const STRINGS = {
  'attachment.fullText': 'Full Text',
  'attachment.acceptedVersion': 'Accepted Version',
  'attachment.submittedVersion': 'Submitted Version',
  'pane.item.attachments.select': 'Select a file to attach',
};

/**
 * @param {object} [o]
 * @param {Uint8Array} [o.pdf] staged file content
 * @param {object} [o.itemProps] overrides for the parent item
 * @param {object|null} [o.library] result of Zotero.Libraries.get
 */
function setup(o = {}) {
  const fs = makeFakeFs();
  const calls = [];
  const created = [];
  const unregistered = [];
  const refs = { gate: null };

  const item = {
    id: 5, libraryID: 1, key: 'PARENT01', deleted: false, pdfCount: 0,
    async loadDataType(t) { calls.push('loadDataType:' + t); },
    numFileAttachmentsWithContentType(ct) { calls.push('numFiles:' + ct); return this.pdfCount; },
    ...(o.itemProps || {}),
  };
  const library = o.library === undefined ? { editable: true, filesEditable: true } : o.library;

  const Zotero = makeFakeZotero({
    getString: (k) => STRINGS[k],
    getMainWindow: () => ({ browsingContext: { id: 'main-bc' } }),
    Libraries: { get: (id) => { calls.push('library:' + id); return library; } },
    Attachments: {
      getFileBaseNameFromItem(it, opts) { calls.push('baseName'); return 'Smith - 2020 - A title ' + opts.attachmentTitle.length; },
      async createURLAttachmentFromTemporaryStorageDirectory(opts) {
        calls.push('create');
        const content = fs.files.get(opts.directory + '/' + opts.filename);
        created.push({ opts, content: content ? content.slice() : null });
        if (refs.gate) await refs.gate;
        if (o.createFails) throw o.createFails;
        await fs.IOUtils.remove(opts.directory, { recursive: true }); // the real one moves the directory into storage
        return { key: 'ATTACH01' };
      },
      async importFromFile(opts) { calls.push('importFromFile'); created.push({ imported: opts }); return { key: 'IMPORT01' }; },
    },
    File: {
      async rename(path, newName) {
        calls.push('rename');
        if (o.renameResult === false) return false;
        const dest = path.replace(/[^/]*$/, '') + newName;
        await fs.IOUtils.move(path, dest);
        return newName;
      },
    },
  });

  const { ZFT } = loadPlugin({
    files: ['util.js', 'config.js', 'save.js'],
    globals: { Zotero, IOUtils: fs.IOUtils, PathUtils: fs.PathUtils, ...(o.globals || {}) },
  });
  ZFT.items = { getItem: (id) => { calls.push('getItem:' + id); return o.noItem ? null : item; } };
  ZFT.state = { unregisterTmpDir: async (d) => { unregistered.push(d); } };

  // staged body + store stand-in
  const info = { handle: 9, kind: 'pdf', len: 0, run: 3, mem: null, path: FILE, dir: DIR, refs: 1 };
  const pdf = o.pdf || bytes('%PDF-1.7\n%body\n%%EOF');
  info.len = pdf.length;
  fs.files.set(FILE, pdf.slice());
  fs.dirs.add(DIR);
  const bodies = { get: (h) => (h === 9 && !o.noBody ? info : null) };
  if (o.bodyInfo) Object.assign(info, o.bodyInfo);

  const scope = new ZFT.util.CancelScope();
  const req = (extra = {}) => ({
    item: 5, body: { handle: 9, len: info.len }, trim_prefix: 0, version: 'published',
    url_to_store: 'https://doi.org/10.1097/SLA.0000000000005603', route: 'inst', source: 'lww', ...extra,
  });
  return { ZFT, fs, calls, created, unregistered, item, info, bodies, scope, req, refs, Zotero };
}

const kindOf = (p) => p.then(() => 'resolved', (e) => e.kind);
const dirGone = (env) => !env.fs.dirs.has(DIR) && !env.fs.files.has(FILE);

test('happy path: argument shapes, order of operations and the SaveOk', async () => {
  const env = setup();
  const out = await env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies });
  assert.deepStrictEqual(plain(out), { attachment_key: '1/ATTACH01', filename: 'Smith - 2020 - A title 9.pdf' });
  assert.deepStrictEqual(env.calls, [
    'getItem:5', 'library:1', 'loadDataType:childItems', 'numFiles:application/pdf', 'baseName', 'rename', 'create',
  ]);
  assert.equal(env.created.length, 1);
  assert.deepStrictEqual(plain(env.created[0].opts), {
    directory: DIR, libraryID: 1, parentItemID: 5, filename: 'Smith - 2020 - A title 9.pdf', title: 'Full Text',
    url: 'https://doi.org/10.1097/SLA.0000000000005603', contentType: 'application/pdf',
  });
  assert.equal(text(env.created[0].content), '%PDF-1.7\n%body\n%%EOF', 'the renamed file was in place when the attachment was created');
  // moved into storage: the staged body is consumed and its directory unregistered, nothing else removed
  assert.equal(env.info.consumed, true);
  assert.equal(env.info.dir, null);
  assert.equal(env.info.path, null);
  assert.deepStrictEqual(env.unregistered, [DIR]);
});

test('the attachment title follows the version', async () => {
  const cases = { published: 'Full Text', unknown: 'Full Text', accepted: 'Accepted Version', submitted: 'Submitted Version' };
  for (const [version, title] of Object.entries(cases)) {
    const env = setup();
    await env.ZFT.save.savePdf(env.req({ version }), { scope: env.scope, bodies: env.bodies });
    assert.equal(env.created[0].opts.title, title, version);
    assert.equal(env.ZFT.save.titleForVersion(version), title);
  }
});

test('titleForVersion falls back to English when Zotero.getString is unavailable or throws', () => {
  const env = setup();
  env.Zotero.getString = () => { throw new Error('no strings'); };
  assert.equal(env.ZFT.save.titleForVersion('accepted'), 'Accepted Version');
  env.Zotero.getString = undefined;
  assert.equal(env.ZFT.save.titleForVersion('submitted'), 'Submitted Version');
  assert.equal(env.ZFT.save.titleForVersion('published'), 'Full Text');
  assert.equal(env.ZFT.save.titleForVersion('nonsense'), 'Full Text');
  env.Zotero.getString = () => '';
  assert.equal(env.ZFT.save.titleForVersion('published'), 'Full Text');
});

test('every precondition failure maps to its OpError and nothing is created; the staged directory is removed', async () => {
  const table = [
    ['parent_gone: no such item', { noItem: true }, 'parent_gone'],
    ['parent_gone: item trashed', { itemProps: { deleted: true } }, 'parent_gone'],
    ['not_editable: files not editable', { library: { editable: true, filesEditable: false } }, 'not_editable'],
    ['not_editable: library read-only', { library: { editable: false, filesEditable: true } }, 'not_editable'],
    ['not_editable: library unknown', { library: null }, 'not_editable'],
    ['already_has_pdf', { itemProps: { pdfCount: 1 } }, 'already_has_pdf'],
    ['internal: staged body unknown', { noBody: true }, 'internal'],
    ['internal: staged body is not a file', { bodyInfo: { kind: 'page', path: null, dir: null } }, 'internal'],
  ];
  for (const [name, opts, kind] of table) {
    const env = setup(opts);
    const err = await env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies }).catch((e) => e);
    assert.equal(err.kind, kind, name);
    assert.equal(env.calls.includes('create'), false, name + ': no attachment created');
    assert.equal(env.calls.includes('rename'), false, name + ': nothing renamed');
    if (!opts.noBody && !opts.bodyInfo) {
      assert.equal(dirGone(env), true, name + ': directory removed');
      assert.deepStrictEqual(env.unregistered, [DIR], name);
      assert.equal(env.info.consumed, true, name);
    }
  }
});

test('a PDF that appeared meanwhile is detected after loading the child items', async () => {
  const env = setup({ itemProps: { pdfCount: 2 } });
  await env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies }).catch(() => {});
  assert.ok(env.calls.indexOf('loadDataType:childItems') < env.calls.indexOf('numFiles:application/pdf'));
});

test('validation: malformed request, ctx and non-canonical URLs are internal, and the directory is still removed', async () => {
  let env = setup();
  assert.equal(await kindOf(env.ZFT.save.savePdf(null, { scope: env.scope, bodies: env.bodies })), 'internal');
  assert.equal(await kindOf(env.ZFT.save.savePdf({ item: 5 }, { scope: env.scope, bodies: env.bodies })), 'internal');
  assert.equal(await kindOf(env.ZFT.save.savePdf(env.req(), { bodies: env.bodies })), 'internal');
  assert.equal(await kindOf(env.ZFT.save.savePdf(env.req(), { scope: env.scope })), 'internal');
  assert.equal(env.fs.files.has(FILE), true, 'malformed requests alone do not touch the staged file');

  for (const bad of [
    'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fx.example%2Fp.pdf',
    'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fx',
    'https://www-nature-com.kcl.idm.oclc.org/articles/s41586.pdf',
    '', 'not a url',
  ]) {
    env = setup();
    assert.equal(await kindOf(env.ZFT.save.savePdf(env.req({ url_to_store: bad }), { scope: env.scope, bodies: env.bodies })), 'internal', bad);
    assert.equal(env.calls.includes('create'), false);
    assert.equal(dirGone(env), true, 'directory removed for ' + bad);
  }
});

test('trim_prefix: bytes before %PDF- are removed before the rename', async () => {
  const pdf = new Uint8Array([...bytes('<junk>\n'), ...bytes('%PDF-1.4\nreal\n%%EOF')]);
  assert.equal(bytes('<junk>\n').length, 7);
  const env = setup({ pdf });
  await env.ZFT.save.savePdf(env.req({ trim_prefix: 7 }), { scope: env.scope, bodies: env.bodies });
  assert.equal(text(env.created[0].content), '%PDF-1.4\nreal\n%%EOF');
  assert.equal(env.created[0].content.length, pdf.length - 7);
});

test('trim_prefix that does not point at %PDF- (or beyond the file) fails internal and cleans up', async () => {
  for (const trim of [3, 1000]) {
    const env = setup({ pdf: new Uint8Array([...bytes('<junk>\n'), ...bytes('%PDF-1.4\nx')]) });
    const err = await env.ZFT.save.savePdf(env.req({ trim_prefix: trim }), { scope: env.scope, bodies: env.bodies }).catch((e) => e);
    assert.equal(err.kind, 'internal', 'trim ' + trim);
    assert.equal(env.calls.includes('create'), false);
    assert.equal(dirGone(env), true);
  }
});

test('trim_prefix on an absurdly large body is an io error', async () => {
  const env = setup({ bodyInfo: { len: 600 * 1024 * 1024 } });
  const err = await env.ZFT.save.savePdf(env.req({ trim_prefix: 5 }), { scope: env.scope, bodies: env.bodies }).catch((e) => e);
  assert.equal(err.kind, 'io');
  assert.equal(dirGone(env), true);
});

test('failures while writing: filesystem errors are io, everything else internal; the directory is always removed', async () => {
  const named = (name, message) => Object.assign(new Error(message), { name });
  const table = [
    ['rename refused (target exists)', { renameResult: false }, 'io'],
    ['NotFoundError', { createFails: named('NotFoundError', 'moving failed') }, 'io'],
    ['NotAllowedError', { createFails: named('NotAllowedError', 'denied') }, 'io'],
    ['message says disk', { createFails: new Error('NS_ERROR_FILE_NO_DEVICE_SPACE') }, 'io'],
    ['database error', { createFails: new Error('database is locked') }, 'internal'],
  ];
  for (const [name, opts, kind] of table) {
    const env = setup(opts);
    const err = await env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies }).catch((e) => e);
    assert.equal(err.kind, kind, name);
    assert.equal(err.name, 'OpError', name + ': only OpErrors leave the adapter');
    assert.equal(dirGone(env), true, name + ': directory removed');
    assert.deepStrictEqual(env.unregistered, [DIR], name);
    assert.equal(env.info.consumed, true, name);
  }
  // an OpError thrown by Zotero-side code is kept as is
  const env = setup();
  env.Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory = async () => { throw new env.ZFT.util.OpError('parent_gone', 'deleted mid-save'); };
  assert.equal(await kindOf(env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies })), 'parent_gone');
});

test('IOUtils failures while trimming are io', async () => {
  const pdf = new Uint8Array([...bytes('xx'), ...bytes('%PDF-1.4\nx')]);
  const env = setup({ pdf });
  env.fs.IOUtils.read = async () => { throw Object.assign(new Error('NotReadableError'), { name: 'NotReadableError' }); };
  assert.equal(await kindOf(env.ZFT.save.savePdf(env.req({ trim_prefix: 2 }), { scope: env.scope, bodies: env.bodies })), 'io');
  assert.equal(dirGone(env), true);
});

test('a scope cancelled before the save rejects cancelled, writes nothing and removes the staged directory', async () => {
  const env = setup();
  env.scope.cancel('user');
  assert.equal(await kindOf(env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies })), 'cancelled');
  assert.equal(env.calls.includes('rename'), false);
  assert.equal(env.calls.includes('create'), false);
  assert.equal(dirGone(env), true);
});

test('once the attachment is being created the save is not cancellable', async () => {
  const env = setup();
  let release;
  env.refs.gate = new Promise((resolve) => { release = resolve; });
  const p = env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies });
  await flush();
  assert.equal(env.calls.includes('create'), true);
  env.scope.cancel('late cancel');
  release();
  const out = await p;
  assert.equal(out.attachment_key, '1/ATTACH01');
});

test('a failed save leaves a missing-dir-tolerant body record (dir/path null) so BodyStore.release does no IO', async () => {
  const env = setup({ createFails: new Error('boom') });
  await env.ZFT.save.savePdf(env.req(), { scope: env.scope, bodies: env.bodies }).catch(() => {});
  assert.equal(env.info.dir, null);
  assert.equal(env.info.path, null);
  assert.equal(env.info.consumed, true);
});

// ----------------------------------------------------------------------------------------------- isStorableUrl

test('isStorableUrl: canonical URLs pass, proxy / SSO / login URLs are refused', () => {
  const { isStorableUrl } = setup().ZFT.save;
  const good = [
    'https://doi.org/10.1097/SLA.0000000000005603',
    'https://www.nature.com/articles/s41586-020-2649-2.pdf?download=true',
    'http://repo.example.org/files/a.pdf',
    'https://europepmc.org/articles/PMC123/pdf',
    'https://xkcl.idm.oclc.org/path', // suffix must match on a label boundary
    'https://kcl.idm.oclc.org.evil.example/path', // a different registrable domain
    'https://pub.example/path/login?foo=bar',
  ];
  const bad = [
    '', null, undefined, 42, {}, 'not a url', 'ftp://x.example/a.pdf', 'javascript:alert(1)',
    'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fpub.example%2Fa.pdf',
    'https://LOGIN.kcl.idm.oclc.org/LOGIN?QURL=x',
    'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1%2Fx',
    'https://pub.example/redirector/foo',
    'https://idp.example/saml?SAMLRequest=abcd',
    'https://pub.example/x?samlrequest=abcd',
    'https://www-nature-com.kcl.idm.oclc.org/articles/s41586-020-2649-2.pdf',
    'https://doi-org.kcl.idm.oclc.org/10.1097/x',
    'https://login.kcl.idm.oclc.org/',
    'https://kcl.idm.oclc.org/',
    'https://www.kcl.idm.oclc.org/menu',
  ];
  for (const url of good) assert.equal(isStorableUrl(url), true, String(url));
  for (const url of bad) assert.equal(isStorableUrl(url), false, String(url));
});

test('isStorableUrl uses the configured EZproxy suffix', () => {
  const env = setup();
  env.Zotero.Prefs.set('extensions.zotero-fulltext.institution.ezproxy_suffix', 'ezproxy.uni.example.edu');
  const { isStorableUrl } = env.ZFT.save;
  assert.equal(isStorableUrl('https://www-bmj-com.ezproxy.uni.example.edu/content/1'), false);
  assert.equal(isStorableUrl('https://www-bmj-com.kcl.idm.oclc.org/content/1'), true, 'the KCL suffix is no longer configured');
  env.Zotero.Prefs.set('extensions.zotero-fulltext.institution.ezproxy_suffix', '');
  assert.equal(isStorableUrl('https://www-bmj-com.ezproxy.uni.example.edu/content/1'), true, 'no suffix configured: only the other rules apply');
  assert.equal(isStorableUrl('https://x.example/login?qurl=a'), false);
});

// ----------------------------------------------------------------------------------------------- attachFile

function withPicker(o = {}) {
  const Ci = { nsIFilePicker: { modeOpen: 0, returnOK: 0, returnCancel: 1, filterAll: 1 } };
  const picks = { inits: [], filters: [], opened: 0 };
  const Cc = {
    '@mozilla.org/filepicker;1': {
      createInstance(iface) {
        assert.equal(iface, Ci.nsIFilePicker);
        return {
          file: o.picked === undefined ? { path: '/home/u/Downloads/paper.pdf' } : o.picked,
          init(...args) { picks.inits.push(args); },
          appendFilter(...args) { picks.filters.push(args); },
          appendFilters(mask) { picks.filters.push(['mask', mask]); },
          open(cb) { picks.opened++; cb(o.rv === undefined ? Ci.nsIFilePicker.returnOK : o.rv); },
        };
      },
    },
  };
  const env = setup({ ...o, globals: { Ci, Cc } });
  return { ...env, picks };
}

test('attachFile: picker for a PDF, then Zotero.Attachments.importFromFile with the parent', async () => {
  const env = withPicker();
  const win = { browsingContext: { id: 'item-window-bc' } };
  const out = await env.ZFT.save.attachFile(5, win);
  assert.deepStrictEqual(plain(out), { attachment_key: '1/IMPORT01' });
  assert.deepStrictEqual(plain(env.picks.inits), [[{ id: 'item-window-bc' }, 'Select a file to attach', 0]]);
  assert.deepStrictEqual(plain(env.picks.filters), [['PDF', '*.pdf'], ['mask', 1]]);
  assert.deepStrictEqual(plain(env.created[0].imported), { file: '/home/u/Downloads/paper.pdf', libraryID: 1, parentItemID: 5 });
});

test('attachFile falls back to the main window and reports a cancelled picker as null', async () => {
  const env = withPicker({ rv: 1 });
  assert.equal(await env.ZFT.save.attachFile(5), null);
  assert.deepStrictEqual(plain(env.picks.inits[0][0]), { id: 'main-bc' });
  assert.equal(env.calls.includes('importFromFile'), false);
  const noFile = withPicker({ picked: null });
  assert.equal(await noFile.ZFT.save.attachFile(5, { browsingContext: {} }), null);
});

test('attachFile preconditions: parent_gone, not_editable, no picker available, import failure', async () => {
  let env = withPicker({ noItem: true });
  assert.equal(await kindOf(env.ZFT.save.attachFile(5, {})), 'parent_gone');
  assert.equal(env.picks.opened, 0);
  env = withPicker({ library: { editable: true, filesEditable: false } });
  assert.equal(await kindOf(env.ZFT.save.attachFile(5, {})), 'not_editable');
  assert.equal(env.picks.opened, 0);
  env = setup(); // no Cc / Ci in the context
  assert.equal(await kindOf(env.ZFT.save.attachFile(5, { browsingContext: {} })), 'unsupported');
  env = withPicker();
  env.Zotero.Attachments.importFromFile = async () => { throw Object.assign(new Error('copy failed'), { name: 'NotAllowedError' }); };
  assert.equal(await kindOf(env.ZFT.save.attachFile(5, { browsingContext: {} })), 'io');
});
