// ZFT.pdfCapture: one ref-counted MIMETypeHandler shared by all captures, routed by topFrameElement.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadPlugin } from './helpers/zft-env.mjs';
import { makeFakeZotero } from './helpers/fakes.mjs';

const Ci = { nsILoadContext: { name: 'nsILoadContext' } };

/** Zotero.MIMETypeHandler stand-in that records every add/remove and can deliver a PDF like _StreamListener does. */
function fakeMimeHandler() {
  const log = [];
  const handlers = [];
  return {
    log, handlers,
    addHandlers(type, handler, ignoreContentDisposition) {
      log.push(['add', type, ignoreContentDisposition]);
      handlers.push(handler);
    },
    removeHandlers(type, handler) {
      log.push(['remove', type]);
      const i = handlers.indexOf(handler);
      if (i !== -1) handlers.splice(i, 1);
    },
    /** @returns {Promise<boolean>} what onContent answered (Zotero's `handled`) */
    async deliver(channel, blob, name = 'https://pub.example/x.pdf') {
      let handled = false;
      for (const h of [...handlers]) {
        if (h.onStartRequest) await h.onStartRequest(name, 'application/pdf', channel);
      }
      for (const h of [...handlers]) {
        if (h.onContent) handled = handled || (await h.onContent(blob, name, 'application/pdf', channel));
        if (handled) break;
      }
      return handled;
    },
  };
}

/** A channel whose load context reports `browser` as topFrameElement. */
function channelFor(browser) {
  return {
    notificationCallbacks: {
      getInterface(iid) {
        assert.equal(iid, Ci.nsILoadContext);
        return { topFrameElement: browser };
      },
    },
  };
}

function setup(withHandler = true) {
  const mime = fakeMimeHandler();
  const Zotero = makeFakeZotero(withHandler ? { MIMETypeHandler: mime } : {});
  const { ZFT } = loadPlugin({ files: ['util.js', 'pdf-capture.js'], globals: { Zotero, Ci } });
  return { ZFT, mime, Zotero };
}

const blobOf = (s) => ({ size: s.length, tag: s }); // the capture never looks inside the Blob

test('one capture: handler added once for application/pdf (ignoreContentDisposition), PDF resolves it, dispose removes it', async () => {
  const { ZFT, mime } = setup();
  const browser = { id: 'b1' };
  const cap = ZFT.pdfCapture.capture(browser);
  assert.deepStrictEqual(mime.log, [['add', 'application/pdf', true]]);
  assert.equal(ZFT.pdfCapture.activeCount(), 1);
  assert.equal(cap.found(), false);
  assert.equal(cap.pdfUrl(), null);

  const blob = blobOf('pdf-1');
  assert.equal(await mime.deliver(channelFor(browser), blob, 'https://pub.example/a.pdf'), true);
  assert.equal(await cap.promise, blob);
  assert.equal(cap.found(), true);
  assert.equal(cap.pdfUrl(), 'https://pub.example/a.pdf');

  cap.dispose();
  assert.deepStrictEqual(mime.log, [['add', 'application/pdf', true], ['remove', 'application/pdf']]);
  assert.equal(ZFT.pdfCapture.activeCount(), 0);
  assert.equal(await cap.promise, blob, 'a captured PDF stays resolved after dispose');
});

test('two overlapping captures: handler added once and removed once, each gets only its own PDF', async () => {
  const { ZFT, mime } = setup();
  const a = { id: 'a' };
  const b = { id: 'b' };
  const capA = ZFT.pdfCapture.capture(a);
  const capB = ZFT.pdfCapture.capture(b);
  assert.equal(mime.log.filter((e) => e[0] === 'add').length, 1);
  assert.equal(ZFT.pdfCapture.activeCount(), 2);

  const blobB = blobOf('for-b');
  const blobA = blobOf('for-a');
  assert.equal(await mime.deliver(channelFor(b), blobB), true);
  assert.equal(capA.found(), false, 'A did not receive B\'s PDF');
  assert.equal(await capB.promise, blobB);
  assert.equal(await mime.deliver(channelFor(a), blobA), true);
  assert.equal(await capA.promise, blobA);

  capA.dispose();
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 0, 'still one live capture: handler stays registered');
  assert.equal(ZFT.pdfCapture.activeCount(), 1);
  capB.dispose();
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 1);
  assert.equal(mime.handlers.length, 0);
  assert.equal(ZFT.pdfCapture.activeCount(), 0);
});

test('dispose is idempotent and resolves a pending capture with null', async () => {
  const { ZFT, mime } = setup();
  const other = ZFT.pdfCapture.capture({ id: 'other' });
  const cap = ZFT.pdfCapture.capture({ id: 'x' });
  cap.dispose();
  cap.dispose();
  assert.equal(await cap.promise, null);
  assert.equal(cap.found(), false);
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 0, 'other capture still live');
  other.dispose();
  other.dispose();
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 1, 'removed exactly once');
});

test('a PDF for a browser nobody captures returns false (Zotero default handling), also with no load context at all', async () => {
  const { ZFT, mime } = setup();
  const cap = ZFT.pdfCapture.capture({ id: 'mine' });
  assert.equal(await mime.deliver(channelFor({ id: 'stranger' }), blobOf('x')), false);
  assert.equal(await mime.deliver({}, blobOf('x')), false, 'channel without notificationCallbacks');
  assert.equal(await mime.deliver(null, blobOf('x')), false, 'null channel');
  assert.equal(cap.found(), false);
  cap.dispose();
});

test('topFrameElement falls back to channel.loadGroup.notificationCallbacks', async () => {
  const { ZFT, mime } = setup();
  const browser = { id: 'viaLoadGroup' };
  const cap = ZFT.pdfCapture.capture(browser);
  const channel = {
    get notificationCallbacks() { throw new Error('no callbacks'); },
    loadGroup: { notificationCallbacks: { getInterface: () => ({ topFrameElement: browser }) } },
  };
  assert.equal(await mime.deliver(channel, blobOf('lg')), true);
  assert.equal(cap.found(), true);
  cap.dispose();
});

test('onContent works even if onStartRequest never ran for the channel', async () => {
  const { ZFT, mime } = setup();
  const browser = { id: 'b' };
  const cap = ZFT.pdfCapture.capture(browser);
  const blob = blobOf('late');
  const handled = await mime.handlers[0].onContent(blob, 'https://x/y.pdf', 'application/pdf', channelFor(browser));
  assert.equal(handled, true);
  assert.equal(await cap.promise, blob);
  cap.dispose();
});

test('only the first PDF of a capture is kept; a later one is swallowed (true) so no external helper pops up', async () => {
  const { ZFT, mime } = setup();
  const browser = { id: 'b' };
  const cap = ZFT.pdfCapture.capture(browser);
  const first = blobOf('first');
  assert.equal(await mime.deliver(channelFor(browser), first, 'https://x/1.pdf'), true);
  assert.equal(await mime.deliver(channelFor(browser), blobOf('second'), 'https://x/2.pdf'), true);
  assert.equal(await cap.promise, first);
  assert.equal(cap.pdfUrl(), 'https://x/1.pdf');
  cap.dispose();
});

test('disposeAll resolves every capture null, removes the handler once, and capturing works again afterwards', async () => {
  const { ZFT, mime } = setup();
  const a = ZFT.pdfCapture.capture({ id: 'a' });
  const b = ZFT.pdfCapture.capture({ id: 'b' });
  ZFT.pdfCapture.disposeAll();
  assert.equal(await a.promise, null);
  assert.equal(await b.promise, null);
  assert.equal(ZFT.pdfCapture.activeCount(), 0);
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 1);
  a.dispose(); // late disposes are harmless
  b.dispose();
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 1);
  ZFT.pdfCapture.disposeAll(); // idempotent
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 1);

  const browser = { id: 'again' };
  const c = ZFT.pdfCapture.capture(browser);
  assert.equal(mime.log.filter((e) => e[0] === 'add').length, 2, 'handler registered again');
  const blob = blobOf('after');
  assert.equal(await mime.deliver(channelFor(browser), blob), true);
  assert.equal(await c.promise, blob);
  c.dispose();
  assert.equal(mime.log.filter((e) => e[0] === 'remove').length, 2);
});

test('a stale dispose of an old capture does not remove a newer capture of the same browser', async () => {
  const { ZFT, mime } = setup();
  const browser = { id: 'same' };
  const old = ZFT.pdfCapture.capture(browser);
  ZFT.pdfCapture.disposeAll();
  const fresh = ZFT.pdfCapture.capture(browser);
  old.dispose();
  assert.equal(ZFT.pdfCapture.activeCount(), 1);
  assert.equal(await mime.deliver(channelFor(browser), blobOf('x')), true);
  assert.equal(fresh.found(), true);
  fresh.dispose();
});

test('capture argument and API errors are OpErrors and leave nothing registered', async () => {
  let env = setup(false); // no Zotero.MIMETypeHandler at all
  assert.throws(() => env.ZFT.pdfCapture.capture({ id: 'x' }), (e) => e.kind === 'unsupported');
  assert.equal(env.ZFT.pdfCapture.activeCount(), 0);

  env = setup();
  assert.throws(() => env.ZFT.pdfCapture.capture(null), (e) => e.kind === 'internal');
  const b = { id: 'dup' };
  const cap = env.ZFT.pdfCapture.capture(b);
  assert.throws(() => env.ZFT.pdfCapture.capture(b), (e) => e.kind === 'internal');
  assert.equal(env.ZFT.pdfCapture.activeCount(), 1);
  cap.dispose();

  env = setup();
  let first = true;
  const realAdd = env.mime.addHandlers;
  env.mime.addHandlers = (...args) => { if (first) { first = false; throw new Error('boom'); } return realAdd.apply(env.mime, args); };
  assert.throws(() => env.ZFT.pdfCapture.capture({ id: 'x' }), /boom/);
  assert.equal(env.ZFT.pdfCapture.activeCount(), 0, 'a failed registration leaves no live capture');
  const ok = env.ZFT.pdfCapture.capture({ id: 'y' });
  assert.equal(env.ZFT.pdfCapture.activeCount(), 1);
  ok.dispose();
});

test('a throwing removeHandlers does not break dispose', async () => {
  const { ZFT, mime } = setup();
  mime.removeHandlers = () => { throw new Error('gone'); };
  const cap = ZFT.pdfCapture.capture({ id: 'x' });
  assert.doesNotThrow(() => cap.dispose());
  assert.equal(ZFT.pdfCapture.activeCount(), 0);
  mime.removeHandlers = () => {};
  const again = ZFT.pdfCapture.capture({ id: 'y' });
  again.dispose();
});

test('Zotero.MIMETypeHandler is resolved at call time, not at load time', async () => {
  const { ZFT, Zotero } = setup(false);
  const mime = fakeMimeHandler();
  Zotero.MIMETypeHandler = mime;
  const cap = ZFT.pdfCapture.capture({ id: 'late' });
  assert.equal(mime.log.length, 1);
  cap.dispose();
});
