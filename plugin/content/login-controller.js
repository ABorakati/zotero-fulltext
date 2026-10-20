/* ZFT.login: the institutional sign-in window (Effect::LoginWindow / LoginUpdate) and the Tools-menu warm-up.
 *
 * Owner: js-ui. Window = login.xhtml + login.js (template: tools/sso-spike/plugin/content/login.xhtml/login.js, already
 * validated headless; the real sign-in was left to the user). Why a custom window: Zotero.openInViewer has no address
 * bar, sends <a> clicks to the system browser and hides popups (research/08).
 *
 * SINGLE-FLIGHT: the engine's LoginGate emits exactly one LoginWindow effect per sign-in epoch for ALL waiting items
 * and then LoginUpdate{op, waiting} as more items queue up. This controller therefore opens at most one window per op,
 * refuses a second concurrent open for the same realm (rejects OpError 'internal'), and just updates the "N items
 * waiting" label on update(). The window never navigates anywhere on its own.
 *
 * open() flow:
 *  1. Open login.xhtml (windowtype zotero-fulltext:login) with args.wrappedJSObject = {entryUrl, itemTitle, waiting,
 *     kind, realm, timeoutMs, onEvent(ev), format(id,args)}; wait for 'load'.
 *  2. browser.loadURI(entry_url) in the window's <browser id="zft-login-browser"> (default cookie jar, plain UA via
 *     browser.browsingContext.customUserAgent = Zotero.VersionHeader.getPlainFirefoxUA(); Cloudflare at SSO start
 *     does not clear with Zotero's default UA).
 *  3. If req.capture_pdf: ZFT.pdfCapture.capture(browserEl) so a PDF that arrives after sign-in is taken directly.
 *  4. Poll every POLL_MS: url = browser.currentURI.spec; PageData documentHTML (cheap text, cap 256 KiB);
 *     state = await host.loginState({url, html}); decide(...) below. No cookie inspection.
 *  5. Finish with LoginOk {result, final_url, ca_error?, pdf?: BodyRef, page?: BodyRef, inspect}:
 *       landed        the pure inspector says the browser is on the publisher/proxied site (page body staged via
 *                     ZFT.bodies, inspect = Inspection from host.inspect with spec Page{requested: {}, route: 'inst'})
 *       pdf           a PDF Blob was captured (ZFT.bodies.putBlobAsPdf; inspect = Pdf verdict)
 *       closed_by_user  user closed the window or pressed Skip
 *       handoff       user pressed "Open in browser": Zotero.launchURL(entry_url) (system browser) and finish
 *       timeout       timeoutMs elapsed (one "Keep waiting" extension of timeoutMs is allowed, offered at 75 %)
 *       ca_blocked    state refused{code} (Entra Conditional Access, e.g. AADSTS53000): ca_error = code
 *     A challenge page (state challenge_shown) keeps the window open and shows the hint; the user solves it.
 *  6. ALWAYS (finally): dispose capture, remove timers, close the window if still open, forget the op.
 * Security UI (login.js): a bar with the real origin (browser.currentURI.prePath) and TLS state, because the user types
 * credentials into this window; popups opened by SSO pages (window.open / target=_blank) appear in a visible pane
 * (nsIBrowserDOMWindow implementation from the spike), never hidden. Zotero's ExternalLinkHandler actor must NOT attach
 * (no messagemanagergroup attribute on the <browser>).
 *
 * Implementation notes:
 *  - The window script reads window.arguments[0].wrappedJSObject; args.wrappedJSObject === args (spike idiom).
 *  - User clicks arrive as args.onEvent({type}) and wake the poll loop at once, so Skip / Open in browser / closing the
 *    window react immediately instead of on the next 500 ms tick. Local flags (window closed, button pressed, PDF
 *    captured) are evaluated BEFORE the slow part of a poll (PageData round trip + wasm), so they never wait for it.
 *  - Every sleep is a ZFT.util.sleep on a CancelScope child of ctx.scope: cancelling the scope closes the window.
 *  - The window is left up for LINGER_MS after ca_blocked (and, for the warm-up, after landing) so the user can read
 *    what the page says before it disappears.
 *  - Nothing here reads cookies, form fields or credentials. Logs carry host + path only (ZFT.progress.redactUrl).
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const WINDOW_URL = 'chrome://zotero-fulltext/content/login.xhtml';
  const WINDOW_TYPE = 'zotero-fulltext:login';
  const POLL_MS = 500;
  /** Fraction of timeoutMs after which "Keep waiting" is offered. */
  const KEEP_WAITING_AT = 0.75;

  /** documentHTML is capped at this many characters before it is staged for the inspector. */
  const HTML_CAP = 256 * 1024;
  /** A PageData round trip that takes longer than this counts as "page not ready" (html = ''). */
  const PAGE_DATA_TIMEOUT_MS = 2000;
  /** How long the window stays visible after ca_blocked / (warm-up) landed. */
  const LINGER_MS = 1500;
  /** The window must finish loading within this time or the sign-in is abandoned (closed_by_user). */
  const LOAD_WAIT_MS = 15000;
  const LOAD_POLL_MS = 50;
  const DEFAULT_TIMEOUT_MS = 300000;
  const WARM_KEY = 'warmup';

  /** op (or WARM_KEY) -> live session. */
  const sessions = new Map();

  // ------------------------------------------------------------------------------------------ pure decision

  /**
   * Pure poll decision (exported for table tests). Returns what to do after one poll.
   * @param {{state: object, elapsedMs: number, timeoutMs: number, extended: boolean,
   *          windowClosed: boolean, userAction: ('skip'|'handoff'|'keep_waiting'|null), pdfCaptured: boolean}} s
   *        state = model.rs LoginState {state: 'pending'|'challenge_shown'|'landed'|'refused', code?}
   * @returns {{done: (null|'landed'|'pdf'|'closed_by_user'|'handoff'|'timeout'|'ca_blocked'), caError?: (string|null),
   *            extend?: boolean, offerKeepWaiting?: boolean, showChallengeHint?: boolean}}
   *   Priority: pdfCaptured > windowClosed/skip > handoff > refused > landed > timeout (after at most one extension).
   *   While not done, showChallengeHint and offerKeepWaiting are independent advice flags.
   */
  function decide(s) {
    const input = s || {};
    const st = typeof input.state === 'string' ? input.state : input.state && input.state.state;
    const code = input.state && typeof input.state === 'object' ? input.state.code : null;
    const elapsed = Number(input.elapsedMs) || 0;
    const timeout = Number(input.timeoutMs) || 0;
    if (input.pdfCaptured) return { done: 'pdf' };
    if (input.windowClosed || input.userAction === 'skip') return { done: 'closed_by_user' };
    if (input.userAction === 'handoff') return { done: 'handoff' };
    if (st === 'refused') return { done: 'ca_blocked', caError: code == null || code === '' ? null : String(code) };
    if (st === 'landed') return { done: 'landed' };
    if (timeout > 0 && elapsed >= timeout) {
      if (!input.extended && input.userAction === 'keep_waiting') return { done: null, extend: true };
      return { done: 'timeout' };
    }
    const out = { done: null };
    if (st === 'challenge_shown') out.showChallengeHint = true;
    if (!input.extended && timeout > 0 && elapsed >= KEEP_WAITING_AT * timeout) out.offerKeepWaiting = true;
    return out;
  }

  // ------------------------------------------------------------------------------------------ helpers

  function redact(url) {
    return ZFT.progress && typeof ZFT.progress.redactUrl === 'function' ? ZFT.progress.redactUrl(url) : '(url)';
  }

  function isHttpUrl(url) {
    if (typeof url !== 'string' || url.length === 0 || url.length > 8192) return false;
    try {
      const u = new URL(url);
      return u.protocol === 'http:' || u.protocol === 'https:';
    } catch (e) {
      return false;
    }
  }

  /** scheme://host/path without userinfo, query or fragment ('' when it is not an http(s) URL). */
  function stripUrl(url) {
    try {
      const u = new URL(String(url));
      return u.protocol === 'http:' || u.protocol === 'https:' ? u.protocol + '//' + u.host + u.pathname : '';
    } catch (e) {
      return '';
    }
  }

  function isOpen(win) {
    try { return !!win && !win.closed; } catch (e) { return false; }
  }

  function closeWindow(win) {
    try { if (win && !win.closed) win.close(); } catch (e) { /* already gone */ }
  }

  function quitting() {
    try { return typeof Services !== 'undefined' && !!Services.startup && Services.startup.shuttingDown === true; } catch (e) { return false; }
  }

  function bringToFront(win) {
    try {
      if (typeof Zotero !== 'undefined' && Zotero.Utilities && Zotero.Utilities.Internal && typeof Zotero.Utilities.Internal.activate === 'function') {
        Zotero.Utilities.Internal.activate(win);
      } else if (win && typeof win.focus === 'function') {
        win.focus();
      }
    } catch (e) { /* best effort */ }
  }

  function currentSpec(browser) {
    try { return browser && browser.currentURI ? String(browser.currentURI.spec || '') : ''; } catch (e) { return ''; }
  }

  /** Reject if `promise` takes longer than ms (the late settlement is swallowed). */
  function withTimeout(promise, ms) {
    return new Promise((resolve, reject) => {
      const id = setTimeout(() => reject(new Error('timeout')), ms);
      Promise.resolve(promise).then(
        (v) => { clearTimeout(id); resolve(v); },
        (e) => { clearTimeout(id); reject(e); }
      );
    });
  }

  function setLabels(session, labels) {
    try {
      const w = session.win;
      if (w && w.zftLogin && typeof w.zftLogin.setLabels === 'function') w.zftLogin.setLabels(labels);
    } catch (e) {
      ZFT.util.log('warn', 'login: setLabels failed: ' + ZFT.util.errText(e));
    }
  }

  /** Sleep up to ms; returns early when an event wakes the loop or the session is cancelled (callers re-check). */
  async function waitFor(session, ms) {
    const tick = session.scope.child();
    session.wake = () => tick.cancel('wake');
    try {
      await ZFT.util.sleep(ms, tick);
    } catch (e) {
      /* woken or cancelled: the loop re-reads its flags */
    } finally {
      session.wake = null;
      tick.dispose();
    }
  }

  /** Event from the window script (login.js). */
  function onWindowEvent(session, ev) {
    if (!ev || typeof ev.type !== 'string' || session.over) return;
    switch (ev.type) {
      case 'skip': session.userAction = 'skip'; break;
      case 'handoff': session.userAction = 'handoff'; break;
      case 'keep_waiting': session.userAction = 'keep_waiting'; break;
      case 'closed': session.windowClosed = true; break;
      case 'popup': ZFT.util.log('info', `login: popup via ${String(ev.via).slice(0, 40)} ${redact(ev.url)}`); return;
      case 'popup-error': ZFT.util.log('warn', `login: popup problem at ${String(ev.stage).slice(0, 40)}`); return;
      default: return;
    }
    session.pendingWake = true;
    if (session.wake) session.wake();
  }

  function createSession(key, params) {
    const ctx = params.ctx || {};
    return {
      key,
      op: params.op,
      run: params.run,
      req: params.req,
      realm: String(params.req.realm),
      warm: !!params.warm,
      ctx,
      host: ctx.host || null,
      bodies: ctx.bodies || null,
      scope: ctx.scope ? ctx.scope.child() : new ZFT.util.CancelScope(),
      timeoutMs: params.req.timeout_ms > 0 ? Number(params.req.timeout_ms) : DEFAULT_TIMEOUT_MS,
      waiting: Number(params.req.waiting) || 0,
      win: null,
      browser: null,
      capture: null,
      over: false,
      aborted: false,
      windowClosed: false,
      userAction: null,
      extended: false,
      pendingWake: false,
      sawState: false,
      wake: null,
      forced: ZFT.util.Deferred(),
      lastLabels: { hint: null, keep: false }, // the Keep waiting button starts hidden (markup)
      t0: 0,
    };
  }

  // ------------------------------------------------------------------------------------------ window lifecycle

  function windowReady(win) {
    try {
      const d = win.document;
      return !!d && d.readyState === 'complete' && String(d.documentURI) === WINDOW_URL;
    } catch (e) {
      return false;
    }
  }

  /**
   * Open the window, wait for it to load and start the sign-in page in its <browser>.
   * @returns {Promise<boolean>} false when the window could not be opened or was closed before the page started
   */
  async function openAndLoad(session) {
    const req = session.req;
    const holder = {
      entryUrl: req.entry_url,
      itemTitle: String(req.item_title || ''),
      waiting: session.waiting,
      kind: req.kind === 'challenge' ? 'challenge' : 'login',
      realm: session.realm,
      timeoutMs: session.timeoutMs,
      onEvent: (ev) => onWindowEvent(session, ev),
      format: (id, args) => ZFT.l10n.format(id, args),
    };
    holder.wrappedJSObject = holder;
    if (quitting()) return false;
    try {
      session.win = Services.ww.openWindow(null, WINDOW_URL, '_blank', 'chrome,centerscreen,resizable,dialog=no', holder);
    } catch (e) {
      ZFT.util.log('error', 'login: window could not be opened: ' + ZFT.util.errText(e));
      return false;
    }
    const win = session.win;
    if (!win) return false;
    // closeAll() may have run while openWindow was spinning: it beats us.
    if (session.over) return false;
    try {
      win.addEventListener('load', () => { if (session.wake) session.wake(); }, { once: true });
    } catch (e) { /* the readiness poll below covers it */ }

    let waited = 0;
    while (!windowReady(win)) {
      if (session.over || session.scope.cancelled || !isOpen(win) || session.windowClosed) return false;
      if (waited >= LOAD_WAIT_MS) {
        ZFT.util.log('error', 'login: window did not finish loading');
        return false;
      }
      await waitFor(session, LOAD_POLL_MS);
      waited += LOAD_POLL_MS;
    }
    if (session.over || session.windowClosed) return false;

    const browser = win.document.getElementById('zft-login-browser');
    if (!browser) {
      ZFT.util.log('error', 'login: window has no browser element');
      return false;
    }
    session.browser = browser;
    bringToFront(win);

    try {
      browser.browsingContext.customUserAgent = Zotero.VersionHeader.getPlainFirefoxUA();
    } catch (e) {
      ZFT.util.log('warn', 'login: could not set the plain user agent: ' + ZFT.util.errText(e));
    }
    const labels = { waiting: session.waiting };
    if (req.kind === 'challenge') {
      // A challenge window exists because the engine saw a human check: say so from the first frame.
      labels.hint = ZFT.l10n.format('zotero-fulltext-login-challenge-hint');
      session.lastLabels.hint = labels.hint;
    }
    setLabels(session, labels);

    // The capture is installed before the first navigation so no PDF can slip past it.
    if (req.capture_pdf && !session.warm) {
      try {
        session.capture = ZFT.pdfCapture.capture(browser);
      } catch (e) {
        ZFT.util.log('warn', 'login: PDF capture unavailable: ' + ZFT.util.errText(e));
      }
    }
    try {
      browser.loadURI(Services.io.newURI(req.entry_url), {
        triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
      });
    } catch (e) {
      ZFT.util.log('error', `login: loadURI failed for ${redact(req.entry_url)}: ${ZFT.util.errText(e)}`);
      return false;
    }
    ZFT.util.log('info', `login: window open at ${redact(req.entry_url)} (realm ${session.realm}, ${req.kind})`);
    return true;
  }

  /** documentHTML of the window's page, capped; '' when the actor is not ready. */
  async function readHtml(session) {
    try {
      const wg = session.browser.browsingContext && session.browser.browsingContext.currentWindowGlobal;
      if (!wg) return '';
      const text = await withTimeout(wg.getActor('PageData').sendQuery('documentHTML'), PAGE_DATA_TIMEOUT_MS);
      return typeof text === 'string' ? text.slice(0, HTML_CAP) : '';
    } catch (e) {
      return '';
    }
  }

  async function readState(session, url, html) {
    if (!url || url === 'about:blank' || !session.host || typeof session.host.loginState !== 'function') return { state: 'pending' };
    try {
      const st = await session.host.loginState({ url, html });
      return st && typeof st === 'object' ? st : { state: 'pending' };
    } catch (e) {
      return { state: 'pending' };
    }
  }

  function localFlags(session) {
    return {
      windowClosed: session.windowClosed || !isOpen(session.win),
      pdfCaptured: !!(session.capture && session.capture.found()),
      userAction: session.userAction,
    };
  }

  function pushLabels(session, r) {
    const hint = r.showChallengeHint ? ZFT.l10n.format('zotero-fulltext-login-challenge-hint') : '';
    const keep = !!r.offerKeepWaiting;
    const labels = {};
    if (session.lastLabels.hint !== hint && (hint || session.lastLabels.hint)) { labels.hint = hint; session.lastLabels.hint = hint; }
    if (session.lastLabels.keep !== keep) { labels.showKeepWaiting = keep; session.lastLabels.keep = keep; }
    if (Object.keys(labels).length) setLabels(session, labels);
  }

  /**
   * The poll loop. Resolves with {result, finalUrl, caError?, html?, blob?}; never rejects on its own errors
   * (an unexpected one becomes closed_by_user and is logged).
   */
  async function pollLoop(session) {
    session.t0 = ZFT.util.monoNow();
    const outcome = (result, extra) => Object.assign({ result, finalUrl: '', caError: null, html: '', blob: null }, extra || {});
    while (!session.over && !session.scope.cancelled) {
      if (!session.pendingWake) await waitFor(session, POLL_MS);
      session.pendingWake = false;
      if (session.over || session.scope.cancelled) break;

      // 1. Local facts only: a button, the closed window or a captured PDF never wait for the page round trip.
      const flags = localFlags(session);
      const elapsedBefore = ZFT.util.monoNow() - session.t0;
      const pre = decide(Object.assign({ state: { state: 'pending' }, elapsedMs: elapsedBefore, timeoutMs: session.timeoutMs, extended: session.extended }, flags));
      if (pre.done === 'pdf' || pre.done === 'closed_by_user' || pre.done === 'handoff') {
        return outcome(pre.done, { finalUrl: currentSpec(session.browser) });
      }

      // 2. The page: URL + html -> wasm login state.
      const url = currentSpec(session.browser);
      const html = await readHtml(session);
      const state = await readState(session, url, html);
      if (session.over) break;
      if (state.state && state.state !== 'pending') session.sawState = true;

      // 3. Decide with fresh flags (events may have arrived while we awaited).
      const r = decide(Object.assign({ state, elapsedMs: ZFT.util.monoNow() - session.t0, timeoutMs: session.timeoutMs, extended: session.extended }, localFlags(session)));
      if (r.extend) {
        session.extended = true;
        session.timeoutMs *= 2;
        session.userAction = null;
        pushLabels(session, { showChallengeHint: !!(state && state.state === 'challenge_shown'), offerKeepWaiting: false });
        continue;
      }
      if (r.done) {
        return outcome(r.done, { finalUrl: url, caError: r.caError == null ? null : r.caError, html: r.done === 'landed' ? html : '' });
      }
      // Until the classifier has said anything about the page, a challenge window keeps the hint it opened with.
      if (session.req.kind === 'challenge' && !session.sawState) r.showChallengeHint = true;
      pushLabels(session, r);
    }
    return outcome('closed_by_user');
  }

  /**
   * Run one sign-in window to its end and clean up. Resolves with the pollLoop outcome (PDF blob included);
   * rejects OpError('cancelled') when the scope is cancelled. The window, the capture and the timers are always released.
   */
  async function drive(session, lingerOn) {
    let offCancel = () => {};
    const cancelled = new Promise((resolve, reject) => {
      offCancel = session.scope.onCancel((reason) => reject(new ZFT.util.OpError('cancelled', String(reason))));
    });
    cancelled.catch(() => {}); // only awaited through the race below
    try {
      const loop = (async () => {
        const started = await openAndLoad(session);
        if (!started) return { result: 'closed_by_user', finalUrl: '', caError: null, html: '', blob: null };
        return pollLoop(session);
      })();
      loop.catch((e) => {
        if (!session.over) ZFT.util.log('error', 'login: poll loop failed: ' + ZFT.util.errText(e));
      });
      let outcome = await Promise.race([
        loop.catch(() => ({ result: 'closed_by_user', finalUrl: '', caError: null, html: '', blob: null })),
        session.forced.promise,
        cancelled,
      ]);
      if (outcome.result === 'handoff') launchEntry(session);
      if (session.capture && outcome.result === 'pdf') {
        outcome = Object.assign({}, outcome, { blob: await session.capture.promise });
      }
      if (lingerOn.has(outcome.result) && !session.aborted && isOpen(session.win) && !session.windowClosed) {
        if (outcome.result === 'ca_blocked') {
          setLabels(session, { hint: ZFT.l10n.format('zotero-fulltext-login-ca-blocked', { code: outcome.caError || 'unknown' }), showKeepWaiting: false });
        }
        await Promise.race([waitFor(session, LINGER_MS), session.forced.promise]);
        session.scope.throwIfCancelled();
      }
      return outcome;
    } finally {
      session.over = true;
      offCancel();
      if (session.wake) session.wake();
      if (session.capture) {
        try { session.capture.dispose(); } catch (e) { /* idempotent */ }
      }
      closeWindow(session.win);
      session.scope.dispose();
      if (sessions.get(session.key) === session) sessions.delete(session.key);
    }
  }

  /** "Open in my browser": the system browser gets the entry URL (validated http/https at open time). */
  function launchEntry(session) {
    try {
      if (isHttpUrl(session.req.entry_url)) Zotero.launchURL(new URL(session.req.entry_url).href);
    } catch (e) {
      ZFT.util.log('error', 'login: could not launch the browser: ' + ZFT.util.errText(e));
    }
  }

  // ------------------------------------------------------------------------------------------ results

  async function inspectSafe(host, meta, bytes) {
    try {
      return await host.inspect(meta, bytes);
    } catch (e) {
      ZFT.util.log('warn', 'login: inspect failed: ' + ZFT.util.errText(e));
      return { expect: 'failed', reason: 'inspect_error' };
    }
  }

  /** Build proto.rs LoginOk from a finished outcome (stages the page / PDF body). */
  async function buildLoginOk(session, outcome) {
    const ok = { result: outcome.result, final_url: '', inspect: { expect: 'none' } };
    const full = outcome.result === 'landed' || outcome.result === 'pdf';
    ok.final_url = full ? outcome.finalUrl : stripUrl(outcome.finalUrl);
    if (outcome.result === 'ca_blocked' && outcome.caError != null) ok.ca_error = outcome.caError;

    if (outcome.result === 'landed' && outcome.html && session.bodies && session.host) {
      const bytes = new TextEncoder().encode(outcome.html);
      try {
        ok.page = await session.bodies.putMemory(bytes, { kind: 'page', run: session.run });
      } catch (e) {
        ZFT.util.log('warn', 'login: could not keep the landing page: ' + ZFT.util.errText(e));
      }
      if (ok.page) {
        ok.inspect = await inspectSafe(session.host, {
          spec: { expect: 'page', requested: {}, route: 'inst' },
          resp: { status: 200, final_url: outcome.finalUrl, headers: { 'content-type': 'text/html' }, body_len: bytes.length },
        }, bytes);
      }
    } else if (outcome.result === 'pdf') {
      if (!outcome.blob || !session.bodies) throw new ZFT.util.OpError('internal', 'login: the captured PDF is gone');
      try {
        ok.pdf = await session.bodies.putBlobAsPdf(outcome.blob, session.run);
        const staged = await session.bodies.stageForInspect(ok.pdf.handle);
        ok.inspect = await inspectSafe(session.host, {
          spec: { expect: 'pdf', requested: {}, route: 'inst' },
          resp: {
            status: 200, final_url: outcome.finalUrl, headers: { 'content-type': 'application/pdf' },
            body_len: staged.body_len, head_len: staged.head_len == null ? null : staged.head_len, truncated: !!staged.truncated,
          },
        }, staged.bytes);
      } catch (e) {
        if (e instanceof ZFT.util.OpError) throw e;
        throw new ZFT.util.OpError('io', 'login: could not store the captured PDF: ' + ZFT.util.errText(e));
      }
    }
    return ok;
  }

  // ------------------------------------------------------------------------------------------ public API

  /**
   * Effect::LoginWindow.
   * @param {{op: number, req: object}} eff req = proto.rs LoginReq {kind: 'login'|'challenge', realm, entry_url, item_title, waiting, timeout_ms, capture_pdf}
   * @param {{scope: ZFT.util.CancelScope, host: object, bodies: object}} ctx host = wasm Host, bodies = BodyStore
   * @returns {Promise<object>} proto.rs LoginOk
   * @throws {ZFT.util.OpError} cancelled (scope cancelled: the window is closed) | internal (second window for the same realm)
   */
  async function open(eff, ctx) {
    const req = eff && eff.req;
    if (!req || typeof req !== 'object') throw new ZFT.util.OpError('internal', 'login: LoginWindow without a request');
    const realm = String(req.realm);
    for (const s of sessions.values()) {
      if (!s.over && !s.warm && s.realm === realm) {
        throw new ZFT.util.OpError('internal', `login: a sign-in window for realm ${realm} is already open`);
      }
    }
    if (ctx && ctx.scope) ctx.scope.throwIfCancelled();
    const session = createSession(eff.op, { op: eff.op, run: eff.run, req, ctx, warm: false });
    sessions.set(session.key, session);
    let outcome;
    if (!isHttpUrl(req.entry_url)) {
      // The sign-in page must be a web address; anything else never reaches a browser.
      ZFT.util.log('error', 'login: entry URL is not http(s), nothing opened');
      session.over = true;
      session.scope.dispose();
      sessions.delete(session.key);
      outcome = { result: 'closed_by_user', finalUrl: '', caError: null, html: '', blob: null };
    } else {
      outcome = await drive(session, new Set(['ca_blocked']));
    }
    return buildLoginOk(session, outcome);
  }

  /** Effect::LoginUpdate: change the "N items waiting" count of the window opened by op. No-op if it is gone. */
  function update(op, waiting) {
    const session = sessions.get(op);
    if (!session || session.over) return;
    session.waiting = Number(waiting) || 0;
    if (session.win) setLabels(session, { waiting: session.waiting });
  }

  /**
   * Tools menu "Sign in to King's...": open the same window at institution.warmup_url, or at
   * https://go.openathens.net/redirector/<openathens_domain>?url=<encoded https://doi.org/> when empty, with no
   * capture and no item; resolves when the user closes the window or the page lands. Cookies stay in the default jar
   * for the batches that follow. @param {object} config EngineConfig @returns {Promise<void>}
   */
  async function warmUp(config) {
    const existing = sessions.get(WARM_KEY);
    if (existing && !existing.over) {
      bringToFront(existing.win);
      return existing.done;
    }
    const inst = (config && config.institution) || {};
    let url = String(inst.warmup_url || '').trim();
    if (!url) {
      const domain = String(inst.openathens_domain || '').trim();
      if (domain) url = 'https://go.openathens.net/redirector/' + encodeURIComponent(domain) + '?url=' + encodeURIComponent('https://doi.org/');
    }
    if (!isHttpUrl(url)) {
      ZFT.util.log('warn', 'login: warm-up has no usable start address');
      return undefined;
    }
    const seconds = config && config.inst ? Number(config.inst.login_timeout_sec) : 0;
    let host = null;
    try {
      host = await ZFT.wasmHost.get();
    } catch (e) {
      ZFT.util.log('warn', 'login: warm-up without the page classifier: ' + ZFT.util.errText(e));
    }
    const again = sessions.get(WARM_KEY);
    if (again && !again.over) return again.done; // a concurrent warm-up won the race while we awaited the host
    const req = { kind: 'login', realm: 'openathens', entry_url: url, item_title: '', waiting: 0, timeout_ms: seconds > 0 ? seconds * 1000 : DEFAULT_TIMEOUT_MS, capture_pdf: false };
    const session = createSession(WARM_KEY, { op: null, run: null, req, ctx: { host }, warm: true });
    sessions.set(WARM_KEY, session);
    session.done = drive(session, new Set(['ca_blocked', 'landed'])).then(() => undefined, (e) => {
      ZFT.util.log('warn', 'login: warm-up ended: ' + ZFT.util.errText(e));
    });
    return session.done;
  }

  /** Close every login window (plugin shutdown) and settle their promises as closed_by_user. */
  function closeAll() {
    for (const session of Array.from(sessions.values())) {
      if (session.over) continue;
      session.aborted = true;
      session.forced.resolve({ result: 'closed_by_user', finalUrl: '', caError: null, html: '', blob: null });
      closeWindow(session.win);
      if (session.wake) session.wake();
    }
  }

  ZFT.login = { WINDOW_URL, WINDOW_TYPE, POLL_MS, KEEP_WAITING_AT, LINGER_MS, decide, open, update, warmUp, closeAll };
})(this);
