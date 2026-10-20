/* zotero-fulltext SSO spike -- bootstrap.js
 *
 * Question (research gap B1): can a KCL user complete single sign-on inside a Zotero 10 window,
 * and do Zotero's download paths (Zotero.HTTP.request, HiddenBrowser PDF capture,
 * Zotero.Attachments.addAvailableFile) then reuse that session?
 *
 * Runs ONLY inside a throwaway profile created by tools/zotero-harness/run.ps1.
 *
 *   mode "auto"   (pref extensions.sso-spike.mode = "auto", the default): no human, no login.
 *                 Opens each start URL in a custom window and in Zotero.openInViewer, follows the
 *                 unauthenticated redirect chain up to the identity provider's sign-in form, and
 *                 records what happened. Non-GET requests to identity-provider hosts are
 *                 cancelled before they leave the process, so no SAML request is ever posted
 *                 to an IdP and nothing is ever typed.
 *   mode "manual" (pref = "manual"): a control window lets the PERSON run each step and sign in
 *                 themselves. After each sign-in the plugin runs download checks.
 *
 * The report never contains cookie values, form contents, SAML messages, tokens, or (in manual
 * mode) page text. URLs keep only an allow-list of query parameter values.
 */
/* global Zotero, Services, ChromeUtils, Components, IOUtils, PathUtils, setTimeout, clearTimeout, setInterval, clearInterval, TextDecoder */

var SP = null;
var chromeHandle = null;
var CI = Components.interfaces;
var CC = Components.classes;
var CR = Components.results;

const PKG = 'ssospike';
const BASE = 'chrome://ssospike/content/';
const ACTOR = 'SsoSpike';
const AUTO_WATCHDOG_MS = 14 * 60 * 1000;
const MANUAL_WATCHDOG_MS = 115 * 60 * 1000;

// ------------------------------------------------------------------------------------------
// Test targets. Entitlement was checked on 2026-10-20 with KCL's Alma resolver (no login):
//   10.1111/jgs.17623   -> "Wiley journals current" (KCL subscription), OpenAlex oa_status closed
//   10.1016/S0140-6736(19)31138-9 -> Elsevier ScienceDirect (KCL subscription), closed
//   10.1176/appi.ajp.20260398 -> "PsychiatryOnline Premium Package"; the free package excludes
//                                 the most recent year, so this 2026 article is paywalled
//   psychiatryonline.org is configured in KCL's EZproxy (brief 04, section 2.4)
// ------------------------------------------------------------------------------------------
const DEFAULT_CONFIG = {
  oaRedirector: 'https://go.openathens.net/redirector/kcl.ac.uk?url=',
  ezproxyLogin: 'https://login.kcl.idm.oclc.org/login?qurl=',
  flows: [
    {
      id: 'oa-wiley',
      kind: 'openathens',
      label: 'OpenAthens redirector -> Wiley (J Am Geriatr Soc 2022)',
      doi: '10.1111/jgs.17623',
      target: 'https://doi.org/10.1111/jgs.17623',
      pdfUrl: 'https://agsjournals.onlinelibrary.wiley.com/doi/pdfdirect/10.1111/jgs.17623',
      findMethods: ['doi'],
      second: {
        doi: '10.1111/jgs.18042',
        target: 'https://doi.org/10.1111/jgs.18042',
        pdfUrl: 'https://agsjournals.onlinelibrary.wiley.com/doi/pdfdirect/10.1111/jgs.18042',
      },
    },
    {
      id: 'ezp-ajp',
      kind: 'ezproxy',
      label: 'EZproxy login?qurl= -> PsychiatryOnline (Am J Psychiatry 2026)',
      doi: '10.1176/appi.ajp.20260398',
      target: 'https://psychiatryonline.org/doi/10.1176/appi.ajp.20260398',
      pdfUrl: 'https://psychiatryonline-org.kcl.idm.oclc.org/doi/pdf/10.1176/appi.ajp.20260398',
      unproxiedPdfUrl: 'https://psychiatryonline.org/doi/pdf/10.1176/appi.ajp.20260398',
      itemUrl: 'https://psychiatryonline-org.kcl.idm.oclc.org/doi/10.1176/appi.ajp.20260398',
      findMethods: ['url'],
      second: {
        doi: '10.1176/appi.ajp.20251008',
        target: 'https://psychiatryonline.org/doi/10.1176/appi.ajp.20251008',
        pdfUrl: 'https://psychiatryonline-org.kcl.idm.oclc.org/doi/pdf/10.1176/appi.ajp.20251008',
      },
    },
    {
      id: 'oa-elsevier',
      kind: 'openathens',
      optional: true,
      label: 'OpenAthens redirector -> Elsevier (Lancet 2019, via DOI)',
      doi: '10.1016/S0140-6736(19)31138-9',
      target: 'https://doi.org/10.1016/S0140-6736(19)31138-9',
      pdfUrl: 'https://www.sciencedirect.com/science/article/pii/S0140673619311389/pdfft?isDTMRedir=true&download=true',
      findMethods: ['doi'],
    },
    {
      id: 'ezp-bmj',
      kind: 'ezproxy',
      optional: true,
      label: 'EZproxy login?qurl= -> BMJ (free article: proves login and proxying only)',
      doi: '10.1136/bmj.m1091',
      target: 'https://www.bmj.com/content/368/bmj.m1091',
      pdfUrl: 'https://www-bmj-com.kcl.idm.oclc.org/content/368/bmj.m1091.full.pdf',
      unproxiedPdfUrl: 'https://www.bmj.com/content/368/bmj.m1091.full.pdf',
      itemUrl: 'https://www-bmj-com.kcl.idm.oclc.org/content/368/bmj.m1091',
      findMethods: ['url'],
    },
  ],
  // auto mode: Nature's chain to Entra is GET-only (brief 04, section 4.2)
  autoExtraFlows: [
    {
      id: 'oa-nature',
      kind: 'openathens',
      label: 'OpenAthens redirector -> Nature (GET-only chain)',
      target: 'https://www.nature.com/articles/nature12373',
    },
  ],
};

// Identity-provider hosts. In auto mode every non-GET/HEAD request to these is cancelled.
const IDP_HOST_RE = /(^|\.)(microsoftonline\.com|microsoft\.com|live\.com|msauth\.net|msftauth\.net|openathens\.net|idm\.oclc\.org|kcl\.ac\.uk)$/i;

// Query parameters whose values are safe to keep (targets and entity IDs, never secrets).
const SAFE_PARAMS = new Set([
  'url', 'qurl', 'idp', 'entityID', 'entityId', 'entityid', 'targetUrl', 'redirectUri', 'returnUrl',
  'appReturnURL', 'target', 'dest', 'federationId', 'federationid', 'mode', 'SAMLDS',
  'hw-shib-return-uri', 'destination', 'hook_url', 'app', 'ref', 'doi', 'download',
  'needAccess', 'isDTMRedir', 'genre', 'id',
]);

// ------------------------------------------------------------------------------------------
// small utilities
// ------------------------------------------------------------------------------------------
function sleep(ms) {
  return new Promise(r => setTimeout(r, ms));
}

function errStr(e) {
  if (e === undefined || e === null) {
    return String(e);
  }
  let s = (e.name ? e.name + ': ' : '') + (e.message !== undefined ? e.message : String(e));
  if (typeof e.result === 'number') {
    s += ' [0x' + (e.result >>> 0).toString(16) + ']';
  }
  return s.slice(0, 500);
}

function plog(msg) {
  if (!SP) {
    return;
  }
  const line = '+' + Math.round((Date.now() - SP.t0) / 1000) + 's ' + msg;
  SP.log.push(line);
  try { Zotero.debug('[sso-spike] ' + msg); } catch (e) {}
  if (SP.ctl && typeof SP.ctl.onLog === 'function') {
    try { SP.ctl.onLog(line); } catch (e) {}
  }
}

function redactURL(spec) {
  if (!spec) {
    return spec;
  }
  try {
    const u = new URL(String(spec));
    if (u.protocol !== 'http:' && u.protocol !== 'https:') {
      return u.protocol + '(' + String(spec).length + ' chars)';
    }
    const parts = [];
    for (const [k, v] of u.searchParams) {
      parts.push(SAFE_PARAMS.has(k) ? k + '=' + v.slice(0, 200) : k + '=<redacted>');
    }
    return u.origin + u.pathname.slice(0, 300) + (parts.length ? '?' + parts.join('&') : '') + (u.hash ? '#<hash>' : '');
  }
  catch (e) {
    return '<unparseable url>';
  }
}

function hostOf(spec) {
  try { return new URL(String(spec)).host; } catch (e) { return ''; }
}

function decodeAscii(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) {
    s += String.fromCharCode(bytes[i]);
  }
  return s;
}

function htmlHints(text) {
  const t = String(text || '');
  const title = (t.match(/<title[^>]*>([\s\S]{0,200}?)<\/title>/i) || [])[1];
  return {
    title: title ? title.replace(/\s+/g, ' ').trim().slice(0, 120) : null,
    cloudflareChallenge: /<title>\s*Just a moment|challenges\.cloudflare\.com|cf-chl-|cf_chl_/i.test(t),
    ezproxyLoginForm: /name=['"]?EZproxyForm/i.test(t),
    entraLogin: /login\.microsoftonline\.com/i.test(t) && /loginfmt|Sign in to your account/i.test(t),
    samlAutoPost: /name=['"]?SAML(Request|Response)/i.test(t),
    openAthensPleaseWait: /OpenAthens\s*\/\s*Please wait/i.test(t),
    paywallWords: /(purchase|subscribe|get access|access options|buy article|rent this|institutional login|sign in to access)/i.test(t),
    citationPdfUrl: /<meta[^>]+name=["']citation_pdf_url["']/i.test(t),
    ezproxyNeedHost: /needhost|not configured|To access this site/i.test(t),
  };
}

function prefInfo(name) {
  const ps = Services.prefs;
  const t = ps.getPrefType(name);
  const info = { type: ({ 0: 'absent', 32: 'string', 64: 'int', 128: 'bool' })[t] || String(t) };
  try {
    if (t === ps.PREF_BOOL) {
      info.value = ps.getBoolPref(name);
    }
    else if (t === ps.PREF_INT) {
      info.value = ps.getIntPref(name);
    }
    else if (t === ps.PREF_STRING) {
      info.value = ps.getStringPref(name).slice(0, 200);
    }
    info.userSet = ps.prefHasUserValue(name);
    info.locked = ps.prefIsLocked(name);
  }
  catch (e) {
    info.error = errStr(e);
  }
  return info;
}

// ------------------------------------------------------------------------------------------
// cookies (names and flags only)
// ------------------------------------------------------------------------------------------
function sameSiteName(v) {
  const map = {};
  for (const k of Object.keys(CI.nsICookie)) {
    if (k.startsWith('SAMESITE_')) {
      map[CI.nsICookie[k]] = k.slice(9).toLowerCase();
    }
  }
  return map[v] !== undefined ? map[v] : String(v);
}

function cookieInventory() {
  const out = [];
  let all = [];
  try { all = Services.cookies.cookies; } catch (e) { return { error: errStr(e) }; }
  const nowMs = Date.now();
  for (const c of all) {
    let exp = null;
    if (!c.isSession) {
      // nsICookie.expiry is seconds in older Gecko and ms in newer Gecko; normalise
      const raw = Number(c.expiry);
      const ms = raw > 1e11 ? raw : raw * 1000;
      exp = Math.round((ms - nowMs) / 86400000 * 10) / 10;
    }
    out.push({
      host: c.host,
      name: c.name,
      path: c.path,
      session: c.isSession,
      expiresInDays: exp,
      httpOnly: c.isHttpOnly,
      secure: c.isSecure,
      sameSite: sameSiteName(c.sameSite),
      partitionKey: c.originAttributes ? c.originAttributes.partitionKey : null,
      userContextId: c.originAttributes ? c.originAttributes.userContextId : null,
    });
  }
  out.sort((a, b) => (a.host + a.name).localeCompare(b.host + b.name));
  return out;
}

function cookieHeaderNames(raw) {
  if (!raw) {
    return [];
  }
  return String(raw).split(';').map(s => s.trim().split('=')[0]).filter(Boolean).slice(0, 60);
}

function setCookieSummary(ch) {
  let raw = '';
  try { raw = ch.getResponseHeader('Set-Cookie'); } catch (e) {}
  if (!raw) {
    return [];
  }
  return raw.split('\n').filter(Boolean).slice(0, 30).map((line) => {
    const parts = line.split(';').map(s => s.trim());
    const rec = { name: parts[0].split('=')[0] };
    for (const p of parts.slice(1)) {
      const eq = p.indexOf('=');
      const k = (eq === -1 ? p : p.slice(0, eq)).toLowerCase();
      const v = eq === -1 ? '' : p.slice(eq + 1);
      if (k === 'samesite') rec.sameSite = v;
      else if (k === 'domain') rec.domain = v;
      else if (k === 'path') rec.path = v.slice(0, 80);
      else if (k === 'secure') rec.secure = true;
      else if (k === 'httponly') rec.httpOnly = true;
      else if (k === 'partitioned') rec.partitioned = true;
      else if (k === 'expires' || k === 'max-age') rec.persistent = true;
    }
    return rec;
  });
}

// ------------------------------------------------------------------------------------------
// network recorder and auto-mode IdP POST guard
// ------------------------------------------------------------------------------------------
const POLICY_TYPE_NAMES = (() => {
  const m = {};
  for (const k of Object.keys(CI.nsIContentPolicy)) {
    if (k.startsWith('TYPE_')) {
      m[CI.nsIContentPolicy[k]] = k.slice(5).toLowerCase();
    }
  }
  return m;
})();

const REC = {
  on: false,
  guard: false,
  label: null,
  t0: 0,
  entries: [],
  blocked: [],
  subresourceHosts: {},
  watchSystemHosts: null,
  pendingBrowserIds: new Set(),
  observe(subject, topic) {
    let ch;
    try { ch = subject.QueryInterface(CI.nsIHttpChannel); } catch (e) { return; }
    let host = '';
    try { host = ch.URI.host; } catch (e) { return; }
    if (topic === 'http-on-modify-request') {
      if (ch.browserId) {
        this.pendingBrowserIds.add(ch.browserId);
      }
      if (this.guard && ch.requestMethod !== 'GET' && ch.requestMethod !== 'HEAD' && IDP_HOST_RE.test(host)) {
        this.blocked.push({
          atMs: Date.now() - this.t0,
          label: this.label,
          method: ch.requestMethod,
          url: redactURL(ch.URI.spec),
          browserId: ch.browserId,
          type: POLICY_TYPE_NAMES[ch.loadInfo ? ch.loadInfo.externalContentPolicyType : -1] || null,
        });
        try { ch.cancel(CR.NS_BINDING_ABORTED); } catch (e) {}
      }
      return;
    }
    if (!this.on) {
      return;
    }
    const type = ch.loadInfo ? ch.loadInfo.externalContentPolicyType : -1;
    const typeName = POLICY_TYPE_NAMES[type] || String(type);
    const isNav = typeName === 'document' || typeName === 'subdocument';
    const isScripted = typeName === 'xmlhttprequest' || typeName === 'fetch';
    const isSystemWatched = ch.browserId === 0 && this.watchSystemHosts && this.watchSystemHosts.has(host);
    if (!isNav && !isScripted && !isSystemWatched) {
      if (ch.browserId !== 0) {
        this.subresourceHosts[host] = (this.subresourceHosts[host] || 0) + 1;
      }
      return;
    }
    if (this.entries.length >= 500) {
      return;
    }
    let status = null;
    let location = null;
    let server = null;
    let contentType = null;
    let cookieReq = '';
    try { status = ch.responseStatus; } catch (e) {}
    try { location = ch.getResponseHeader('Location'); } catch (e) {}
    try { server = ch.getResponseHeader('Server'); } catch (e) {}
    try { contentType = ch.contentType; } catch (e) {}
    try { cookieReq = ch.getRequestHeader('Cookie'); } catch (e) {}
    let ua = null;
    try { ua = ch.getRequestHeader('User-Agent'); } catch (e) {}
    if (ua && !this.uaSeen.includes(ua) && this.uaSeen.length < 5) {
      this.uaSeen.push(ua);
    }
    if (location) {
      try { location = ch.URI.resolve(location); } catch (e) {}
    }
    this.entries.push({
      atMs: Date.now() - this.t0,
      label: this.label,
      topic: topic.replace('http-on-', ''),
      browserId: ch.browserId,
      type: typeName,
      method: ch.requestMethod,
      url: redactURL(ch.URI.spec),
      status,
      location: location ? redactURL(location) : null,
      contentType,
      server: server ? String(server).slice(0, 40) : null,
      cookiesSent: cookieHeaderNames(cookieReq),
      setCookies: setCookieSummary(ch),
      uaHasZoteroToken: ua ? /Zotero\//.test(ua) : null,
    });
  },
  start(label) {
    this.on = true;
    this.label = label;
    this.t0 = Date.now();
    this.entries = [];
    this.blocked = [];
    this.subresourceHosts = {};
    this.watchSystemHosts = new Set();
    this.uaSeen = [];
    this.pendingBrowserIds = new Set();
  },
  uaSeen: [],
  stop() {
    this.on = false;
    const out = { entries: this.entries, idpPostsBlocked: this.blocked, subresourceHostCounts: this.subresourceHosts, userAgentsSeen: this.uaSeen };
    this.entries = [];
    this.blocked = [];
    this.subresourceHosts = {};
    this.watchSystemHosts = null;
    return out;
  },
};

const REC_TOPICS = ['http-on-modify-request', 'http-on-examine-response', 'http-on-examine-cached-response', 'http-on-examine-merged-response'];

// ------------------------------------------------------------------------------------------
// windows and inspection
// ------------------------------------------------------------------------------------------
function waitForLoad(win) {
  return new Promise((resolve) => {
    if (win.document && win.document.readyState === 'complete') {
      resolve();
      return;
    }
    win.addEventListener('load', () => resolve(), { once: true });
  });
}

async function openLoginWindow(kind, url, opts) {
  const options = opts || {};
  const events = [];
  let win;
  let browser;
  if (kind === 'custom') {
    const arg = {
      onEvent: (ev) => {
        const rec = Object.assign({ atMs: Date.now() - SP.t0 }, ev);
        if (rec.url) {
          rec.url = redactURL(rec.url);
        }
        events.push(rec);
        plog('custom window event: ' + JSON.stringify(rec));
      },
    };
    arg.wrappedJSObject = arg;
    win = Services.ww.openWindow(null, BASE + 'login.xhtml', '_blank', 'chrome,centerscreen,resizable,dialog=no', arg);
    await waitForLoad(win);
    browser = win.document.getElementById('ssp-browser');
    if (options.customUserAgent) {
      browser.browsingContext.customUserAgent = options.customUserAgent;
    }
    browser.loadURI(Services.io.newURI(url), {
      triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
    });
  }
  else {
    win = Zotero.openInViewer(url, options.customUserAgent ? { customUserAgent: options.customUserAgent } : {});
    await waitForLoad(win);
    await sleep(100);
    browser = win.document.querySelector('browser');
    // basicViewer.js starts the load in its own window 'load' handler. Once in the first auto
    // run that load never started (no request at all). Detect that and start it again.
    const bid = browser.browserId;
    let started = false;
    for (let i = 0; i < 8 && !started; i++) {
      await sleep(500);
      started = currentSpec(browser) !== 'about:blank'
        || REC.entries.some(e => e.browserId === bid)
        || REC.pendingBrowserIds.has(bid);
    }
    if (!started) {
      events.push({ atMs: Date.now() - SP.t0, type: 'viewer-load-not-started-after-4s-restarting' });
      plog('viewer load had not started after 4 s; calling basicViewer loadURI() again');
      try {
        win.loadURI(Services.io.newURI(url), {});
      }
      catch (e) {
        events.push({ type: 'viewer-restart-error', error: errStr(e) });
      }
    }
  }
  return {
    kind,
    win,
    browser,
    events,
    isClosed() {
      try { return win.closed; } catch (e) { return true; }
    },
    close() {
      try {
        if (!win.closed) {
          win.close();
        }
      }
      catch (e) {}
    },
  };
}

const TEXT_SAMPLE_HOSTS = new Set(['login.microsoftonline.com', 'go.openathens.net', 'login.openathens.net', 'login.kcl.idm.oclc.org']);

async function pageData(browser, msg, ms) {
  const wg = browser && browser.browsingContext && browser.browsingContext.currentWindowGlobal;
  if (!wg) {
    throw new Error('no currentWindowGlobal');
  }
  return Promise.race([
    wg.getActor('PageData').sendQuery(msg, {}),
    sleep(ms || 6000).then(() => { throw new Error('PageData timeout: ' + msg); }),
  ]);
}

// Page facts through Zotero's own PageData actor (registered for every <browser>). The HTML and
// text are analysed here and never stored, except a 300-character text sample of public IdP
// pages in auto mode.
async function inspect(browser, allowText) {
  const out = {};
  const host = hostOf(currentSpec(browser));
  try {
    out.title = String((await pageData(browser, 'title')) || '').slice(0, 200);
    const html = String((await pageData(browser, 'documentHTML')) || '');
    out.htmlChars = html.length;
    out.hasLoginfmt = /name=["']loginfmt["']/.test(html);
    out.hasPasswordField = /type=["']password["']/i.test(html);
    out.hasEZproxyForm = /name=["']?EZproxyForm/.test(html);
    out.hasSAMLRequestField = /name=["']SAMLRequest["']/.test(html);
    out.hasSAMLResponseField = /name=["']SAMLResponse["']/.test(html);
    out.cloudflareChallenge = /<title>\s*Just a moment|challenges\.cloudflare\.com|cf-chl-|cf_chl_/i.test(html.slice(0, 300000));
    out.openAthensPleaseWait = /OpenAthens\s*\/\s*Please wait/i.test(out.title);
    const text = String((await pageData(browser, 'bodyText')) || '');
    out.unsupportedBrowserHint = /not supported|unsupported browser|update your browser|browser (?:is|isn't|is not) (?:supported|compatible)/i.test(text);
    if (allowText && TEXT_SAMPLE_HOSTS.has(host)) {
      out.textSample = text.replace(/\s+/g, ' ').trim().slice(0, 300);
    }
    out.channel = await pageData(browser, 'channelInfo');
  }
  catch (e) {
    out.error = errStr(e);
  }
  // Diagnostic: does a JSWindowActor whose child module lives in THIS plugin load in the
  // page's (content) process?
  try {
    const wg = browser.browsingContext && browser.browsingContext.currentWindowGlobal;
    out.pluginActorPing = wg ? await Promise.race([
      wg.getActor(ACTOR).sendQuery('ping', {}),
      sleep(3000).then(() => 'timeout'),
    ]) : 'no windowGlobal';
  }
  catch (e) {
    out.pluginActorPing = 'error: ' + errStr(e);
  }
  try { out.isRemoteBrowser = browser.isRemoteBrowser; out.remoteType = browser.remoteType || null; } catch (e) {}
  return out;
}

// Console errors during a run (to explain flaky window behaviour); messages only, truncated.
const CONSOLE = {
  on: false,
  items: [],
  listener: {
    observe(m) {
      if (!CONSOLE.on || CONSOLE.items.length >= 40) {
        return;
      }
      try {
        let text = '';
        let file = '';
        if (m instanceof CI.nsIScriptError) {
          if (m.flags & CI.nsIScriptError.warningFlag) {
            return;
          }
          text = m.errorMessage;
          file = m.sourceName;
        }
        else {
          text = m.message;
        }
        if (!/basicViewer|browserWindowShim|ssospike|sso-spike|HiddenBrowser|browserRequest|loadURI|browsingContext/i.test(text + ' ' + file)) {
          return;
        }
        CONSOLE.items.push((String(file).replace(/^.*\//, '') + ': ' + String(text)).slice(0, 300));
      }
      catch (e) {}
    },
  },
  start() { this.items = []; this.on = true; },
  stop() { this.on = false; const x = this.items; this.items = []; return x; },
};

function currentSpec(browser) {
  try { return browser.currentURI ? browser.currentURI.spec : ''; } catch (e) { return ''; }
}

function startURLFor(flow, target) {
  const t = target || flow.target;
  if (flow.kind === 'ezproxy') {
    return SP.config.ezproxyLogin + encodeURIComponent(t);
  }
  return SP.config.oaRedirector + encodeURIComponent(t);
}

// ------------------------------------------------------------------------------------------
// checks
// ------------------------------------------------------------------------------------------
function plainUA() {
  return Zotero.VersionHeader.getPlainFirefoxUA();
}

async function httpTest(url, opts) {
  if (!url) {
    return { skipped: 'no url configured' };
  }
  const o = opts || {};
  const t = Date.now();
  if (REC.watchSystemHosts) {
    REC.watchSystemHosts.add(hostOf(url));
  }
  try {
    const reqOpts = {
      responseType: 'arraybuffer',
      successCodes: false,
      timeout: o.timeout || 60000,
      errorDelayMax: 0,
    };
    if (o.plainUA) {
      // Zotero.VersionHeader only inserts "Firefox/x" before "Zotero/"; a UA without "Zotero/"
      // passes through unchanged (zotero.js:2300-2320).
      reqOpts.headers = { 'User-Agent': plainUA() };
    }
    const xhr = await Zotero.HTTP.request('GET', url, reqOpts);
    const bytes = new Uint8Array(xhr.response || new ArrayBuffer(0));
    const out = {
      ua: o.plainUA ? 'plain' : 'zotero',
      ms: Date.now() - t,
      status: xhr.status,
      finalURL: redactURL(xhr.responseURL),
      finalHost: hostOf(xhr.responseURL),
      contentType: xhr.getResponseHeader('Content-Type'),
      bytes: bytes.length,
      isPDF: decodeAscii(bytes.slice(0, 5)) === '%PDF-',
    };
    if (!out.isPDF) {
      out.htmlHints = htmlHints(new TextDecoder('utf-8').decode(bytes.slice(0, 300000)));
    }
    return out;
  }
  catch (e) {
    return { ms: Date.now() - t, error: errStr(e) };
  }
}

async function hiddenPdfTest(url, opts) {
  if (!url) {
    return { skipped: 'no url configured' };
  }
  const o = opts || {};
  const t = Date.now();
  const path = PathUtils.join(SP.tmpDir, 'hb-' + Date.now() + '.pdf');
  let hb = null;
  try {
    if (!o.plainUA) {
      // Zotero as shipped: HiddenBrowser in the default jar, default UA, MIME capture
      await Zotero.BrowserRequest.downloadPDF(url, path, { shouldDisplayCaptcha: false });
    }
    else {
      // same mechanism (BrowserRequest._loadAndSettle + PDF MIME handler) with a plain Firefox UA
      const { HiddenBrowser } = ChromeUtils.importESModule('chrome://zotero/content/HiddenBrowser.mjs');
      hb = new HiddenBrowser({ customUserAgent: plainUA() });
      await hb._createdPromise;
      let blob = null;
      await Zotero.BrowserRequest._loadAndSettle(hb, url, { onPDF: (b) => { blob = b; } });
      if (!blob) {
        let title = null;
        try { title = String(await pageData(hb._browser, 'title', 3000)).slice(0, 120); } catch (e) {}
        throw new Error('Settled without receiving a PDF (page title: ' + title + ')');
      }
      await Zotero.File.putContentsAsync(path, blob);
    }
    const st = await IOUtils.stat(path);
    const head = await IOUtils.read(path, { maxBytes: 5 });
    return { ua: o.plainUA ? 'plain' : 'zotero', ms: Date.now() - t, ok: true, bytes: st.size, isPDF: decodeAscii(head) === '%PDF-' };
  }
  catch (e) {
    return { ua: o.plainUA ? 'plain' : 'zotero', ms: Date.now() - t, ok: false, error: errStr(e) };
  }
  finally {
    if (hb) {
      try { hb.destroy(); } catch (e) {}
    }
    try { await IOUtils.remove(path, { ignoreAbsent: true }); } catch (e) {}
  }
}

async function hiddenNavigateTest(url, maxMs, opts) {
  const o = opts || {};
  const limit = maxMs || 45000;
  const t = Date.now();
  const { HiddenBrowser } = ChromeUtils.importESModule('chrome://zotero/content/HiddenBrowser.mjs');
  const hb = new HiddenBrowser(o.plainUA ? { customUserAgent: plainUA() } : {});
  let pdf = null;
  let pdfHandler = null;
  try {
    await hb._createdPromise;
    pdfHandler = Zotero.BrowserRequest._makePDFMIMETypeHandler(hb._browser, (blob) => {
      pdf = { bytes: blob.size };
    });
    Zotero.MIMETypeHandler.addHandlers('application/pdf', pdfHandler, true);
    hb.load(url).catch(() => {});
    const urls = [];
    let last = '';
    let stableSince = Date.now();
    while (Date.now() - t < limit && !pdf) {
      await sleep(500);
      const cur = currentSpec(hb._browser);
      if (cur !== last) {
        urls.push(redactURL(cur));
        last = cur;
        stableSince = Date.now();
        continue;
      }
      if (Date.now() - stableSince > 7000 && !/go\.openathens\.net/.test(cur)) {
        break;
      }
    }
    const info = pdf ? null : await inspect(hb._browser, false);
    return {
      ua: o.plainUA ? 'plain' : 'zotero',
      ms: Date.now() - t,
      navigation: urls,
      finalHost: hostOf(last),
      finalURL: redactURL(last),
      endedOnIdP: /login\.microsoftonline\.com|login\.openathens\.net|login\.kcl\.idm\.oclc\.org\/login/.test(last),
      pdfCaptured: pdf,
      inspect: info,
    };
  }
  catch (e) {
    return { ms: Date.now() - t, error: errStr(e) };
  }
  finally {
    if (pdfHandler) {
      try { Zotero.MIMETypeHandler.removeHandlers('application/pdf', pdfHandler); } catch (e) {}
    }
    try { hb.destroy(); } catch (e) {}
  }
}

async function zoteroFindTest(flow) {
  const t = Date.now();
  try {
    const item = new Zotero.Item('journalArticle');
    item.setField('title', 'sso-spike ' + flow.id);
    if (flow.doi) {
      item.setField('DOI', flow.doi);
    }
    if (flow.itemUrl) {
      item.setField('url', flow.itemUrl);
    }
    await item.saveTx();
    const methods = flow.findMethods || ['doi', 'url'];
    const att = await Promise.race([
      Zotero.Attachments.addAvailableFile(item, { methods }),
      sleep(150000).then(() => 'timeout'),
    ]);
    if (att === 'timeout') {
      return { ms: Date.now() - t, methods, error: 'timeout after 150 s' };
    }
    if (!att) {
      return { ms: Date.now() - t, methods, attached: false };
    }
    const p = await att.getFilePathAsync();
    let bytes = null;
    let isPDF = null;
    if (p) {
      const st = await IOUtils.stat(p);
      bytes = st.size;
      isPDF = decodeAscii(await IOUtils.read(p, { maxBytes: 5 })) === '%PDF-';
    }
    return {
      ms: Date.now() - t,
      methods,
      attached: true,
      contentType: att.attachmentContentType,
      linkMode: att.attachmentLinkMode,
      bytes,
      isPDF,
      sourceURL: redactURL(att.getField('url')),
    };
  }
  catch (e) {
    return { ms: Date.now() - t, error: errStr(e) };
  }
}

// ------------------------------------------------------------------------------------------
// environment facts
// ------------------------------------------------------------------------------------------
function collectEnvironment() {
  const names = [
    'network.cookie.cookieBehavior', 'network.cookie.cookieBehavior.pbmode',
    'network.cookie.sameSite.laxByDefault', 'network.cookie.sameSite.noneRequiresSecure',
    'network.cookie.sameSite.schemeful', 'network.cookie.lifetimePolicy',
    'privacy.bounceTrackingProtection.mode', 'privacy.bounceTrackingProtection.bounceTrackingGracePeriodSec',
    'privacy.bounceTrackingProtection.bounceTrackingActivationLifetimeSec',
    'privacy.bounceTrackingProtection.bounceTrackingPurgeTimerPeriodSec',
    'privacy.partition.network_state', 'privacy.trackingprotection.enabled',
    'privacy.trackingprotection.fingerprinting.enabled', 'privacy.resistFingerprinting',
    'privacy.fingerprintingProtection', 'browser.contentblocking.category',
    'privacy.sanitize.sanitizeOnShutdown', 'privacy.clearOnShutdown.cookies',
    'privacy.clearOnShutdown_v2.cookiesAndStorage',
    'network.http.windows-sso.enabled', 'network.http.windows-sso.container-enabled.0',
    'network.http.microsoft-entra-sso.enabled', 'network.negotiate-auth.trusted-uris',
    'network.automatic-ntlm-auth.trusted-uris',
    'security.webauth.webauthn', 'security.webauthn.ctap2',
    'security.webauthn.enable_conditional_mediation', 'security.webauthn.always_allow_direct_attestation',
    'dom.disable_open_during_load', 'browser.link.open_newwindow',
    'network.http.referer.XOriginPolicy', 'network.http.referer.defaultPolicy',
    'general.useragent.override',
    'extensions.zotero.browserRequest.timeout', 'extensions.zotero.browserRequest.onLoadTimeout',
  ];
  const prefs = {};
  for (const n of names) {
    prefs[n] = prefInfo(n);
  }
  const childLists = {};
  for (const branch of ['security.webauth', 'privacy.bounceTrackingProtection.', 'network.http.windows-sso', 'network.cookie.sameSite']) {
    try { childLists[branch] = Services.prefs.getChildList(branch).sort(); } catch (e) { childLists[branch] = errStr(e); }
  }
  let ua = {};
  try {
    const base = CC['@mozilla.org/network/protocol;1?name=http'].getService(CI.nsIHttpProtocolHandler).userAgent;
    ua = {
      protocolHandler: base,
      zoteroFull: Zotero.VersionHeader.update(base),
      zoteroPlain: Zotero.VersionHeader.getPlainFirefoxUA(),
    };
  }
  catch (e) {
    ua.error = errStr(e);
  }
  return {
    zoteroVersion: Zotero.version,
    platformVersion: Services.appinfo.platformVersion,
    headless: Services.env.get('MOZ_HEADLESS') === '1',
    prefs,
    childLists,
    userAgent: ua,
    plainUAHosts: Zotero.BrowserRequest && Zotero.BrowserRequest.PLAIN_UA_HOSTS,
  };
}

// ------------------------------------------------------------------------------------------
// auto mode (no login)
// ------------------------------------------------------------------------------------------
async function watchToSignInForm(lw, maxMs) {
  const t = Date.now();
  const urls = [];
  let last = '';
  let stableSince = Date.now();
  let lastInspectAt = 0;
  let info = null;
  let reason = 'timeout';
  while (Date.now() - t < maxMs) {
    await sleep(500);
    if (lw.isClosed()) {
      reason = 'window-closed';
      break;
    }
    const cur = currentSpec(lw.browser);
    if (cur !== last) {
      urls.push({ atMs: Date.now() - t, url: redactURL(cur) });
      last = cur;
      stableSince = Date.now();
      info = null;
      continue;
    }
    const stableMs = Date.now() - stableSince;
    if (stableMs >= 1500 && Date.now() - lastInspectAt >= 1500) {
      info = await inspect(lw.browser, true);
      lastInspectAt = Date.now();
    }
    if (!info) {
      continue;
    }
    const host = hostOf(cur);
    if (host === 'login.microsoftonline.com' && info.hasLoginfmt) {
      reason = 'entra-sign-in-form-rendered';
      break;
    }
    if (REC.blocked.length && stableMs > (host === 'login.microsoftonline.com' ? 15000 : 5000)) {
      reason = 'stopped-at-blocked-idp-post';
      break;
    }
    if (info.openAthensPleaseWait) {
      continue;
    }
    if (stableMs > 12000) {
      reason = 'url-stable-12s';
      break;
    }
  }
  if (!info || info.error) {
    info = await inspect(lw.browser, true);
  }
  return { reason, ms: Date.now() - t, navigation: urls, finalURL: redactURL(last), finalHost: hostOf(last), finalPage: info };
}

async function autoRun(flow, kind, uaVariant) {
  const label = flow.id + ':' + kind + (uaVariant ? ':' + uaVariant : '');
  plog('auto run ' + label);
  Services.cookies.removeAll();
  const startURL = startURLFor(flow);
  REC.start(label);
  CONSOLE.start();
  const res = { label, flowId: flow.id, kind, uaVariant: uaVariant || 'zotero-default', startURL: redactURL(startURL) };
  let lw = null;
  try {
    const customUserAgent = uaVariant === 'plain' ? plainUA() : undefined;
    lw = await openLoginWindow(kind, startURL, { customUserAgent });
    res.browserId = lw.browser.browserId;
    res.browserRemoteAtStart = lw.browser.isRemoteBrowser;
    res.watch = await watchToSignInForm(lw, 50000);
    res.browserRemoteAtEnd = lw.browser.isRemoteBrowser;
    res.windowEvents = lw.events;
  }
  catch (e) {
    res.error = errStr(e);
  }
  finally {
    if (lw) {
      lw.close();
    }
  }
  await sleep(1500);
  res.consoleErrors = CONSOLE.stop();
  res.network = REC.stop();
  res.cookies = cookieInventory();
  plog('auto run ' + label + ' -> ' + (res.watch ? res.watch.reason + ' @ ' + res.watch.finalHost : res.error));
  return res;
}

async function autoCookieReuse() {
  // Real cross-site redirect chain (OpenAthens -> Nature SP -> OpenAthens -> Entra), GET only.
  const out = { probes: {} };
  Services.cookies.removeAll();
  const nature = SP.config.autoExtraFlows[0];
  REC.start('cookie-reuse:chain');
  const lw = await openLoginWindow('custom', startURLFor(nature), {});
  out.chain = await watchToSignInForm(lw, 50000);
  lw.close();
  await sleep(1000);
  out.chainNetwork = REC.stop();
  out.cookiesAfterChain = cookieInventory();
  // Cookies set by real servers during top-level cross-site navigations in a login window:
  // are they sent by the other request mechanisms to those hosts?
  for (const probeURL of ['https://secure.nature.com/', 'https://login.openathens.net/']) {
    const p = {};
    REC.start('cookie-reuse:http-request:' + probeURL);
    p.httpRequest = await httpTest(probeURL, { timeout: 20000 });
    await sleep(300);
    p.httpRequestFirstHop = REC.stop().entries.filter(e => e.url.startsWith(probeURL)).map(e => ({ type: e.type, status: e.status, cookiesSent: e.cookiesSent, browserId: e.browserId }));
    REC.start('cookie-reuse:hidden-browser:' + probeURL);
    p.hiddenBrowser = await hiddenNavigateTest(probeURL, 12000);
    await sleep(300);
    p.hiddenBrowserFirstHop = REC.stop().entries.filter(e => e.url.startsWith(probeURL)).map(e => ({ type: e.type, status: e.status, cookiesSent: e.cookiesSent, browserId: e.browserId }));
    REC.start('cookie-reuse:plain-fetch:' + probeURL);
    try {
      const r = await fetch(probeURL, { redirect: 'manual' });
      p.plainFetch = { status: r.status, type: r.type };
    }
    catch (e) {
      p.plainFetch = { error: errStr(e) };
    }
    await sleep(300);
    p.plainFetchFirstHop = REC.stop().entries.filter(e => e.url.startsWith(probeURL)).map(e => ({ type: e.type, status: e.status, cookiesSent: e.cookiesSent, browserId: e.browserId }));
    REC.start('cookie-reuse:fetch-include:' + probeURL);
    try {
      const r = await fetch(probeURL, { redirect: 'manual', credentials: 'include' });
      p.fetchCredentialsInclude = { status: r.status, type: r.type };
    }
    catch (e) {
      p.fetchCredentialsInclude = { error: errStr(e) };
    }
    await sleep(300);
    p.fetchIncludeFirstHop = REC.stop().entries.filter(e => e.url.startsWith(probeURL)).map(e => ({ type: e.type, status: e.status, cookiesSent: e.cookiesSent, browserId: e.browserId }));
    out.probes[probeURL] = p;
  }
  return out;
}

async function autoMain() {
  const R = SP.results;
  R.auto = { runs: [], baselines: {} };
  REC.guard = true;
  const flows = SP.config.autoExtraFlows.concat(SP.config.flows);
  for (const flow of flows) {
    for (const kind of ['custom', 'viewer']) {
      R.auto.runs.push(await autoRun(flow, kind));
    }
  }
  const wiley = SP.config.flows.find(f => f.id === 'oa-wiley');
  if (wiley) {
    R.auto.runs.push(await autoRun(wiley, 'custom', 'plain'));
    R.auto.runs.push(await autoRun(wiley, 'viewer', 'plain'));
  }
  // pre-login baselines for the download checks (what a download returns with no session)
  Services.cookies.removeAll();
  for (const flow of SP.config.flows) {
    REC.start('baseline:' + flow.id);
    const b = {
      httpRequestPdf: await httpTest(flow.pdfUrl),
      httpRequestPdfPlainUA: await httpTest(flow.pdfUrl, { plainUA: true }),
    };
    if (flow.unproxiedPdfUrl) {
      b.httpRequestUnproxiedPdf = await httpTest(flow.unproxiedPdfUrl);
      b.httpRequestUnproxiedPdfPlainUA = await httpTest(flow.unproxiedPdfUrl, { plainUA: true });
    }
    if (flow.id === 'oa-wiley') {
      b.hiddenBrowserPdf = await hiddenPdfTest(flow.pdfUrl);
      b.hiddenBrowserPdfPlainUA = await hiddenPdfTest(flow.pdfUrl, { plainUA: true });
      b.hiddenNavigatePdfUrlPlainUA = await hiddenNavigateTest(flow.pdfUrl, 25000, { plainUA: true });
      // after the plain-UA hidden browser cleared Cloudflare: does a plain-UA XHR now pass?
      b.httpRequestPdfPlainUAAfterClearance = await httpTest(flow.pdfUrl, { plainUA: true });
      b.httpRequestPdfZoteroUAAfterClearance = await httpTest(flow.pdfUrl);
      b.cookiesAfter = cookieInventory().filter(c => /wiley/.test(c.host));
    }
    b.network = REC.stop();
    R.auto.baselines[flow.id] = b;
    plog('baseline ' + flow.id + ' done');
  }
  try {
    R.auto.cookieReuse = await autoCookieReuse();
  }
  catch (e) {
    R.auto.cookieReuseError = errStr(e);
  }
  R.auto.cookiesAtEnd = cookieInventory().length;
  REC.guard = false;
}

// ------------------------------------------------------------------------------------------
// manual mode (the person signs in)
// ------------------------------------------------------------------------------------------
function makeController() {
  const steps = [];
  for (const f of SP.config.flows) {
    for (const kind of ['custom', 'viewer']) {
      steps.push({
        id: f.id + ':' + kind,
        flowId: f.id,
        kind,
        optional: !!f.optional,
        label: f.label + ' [' + (kind === 'custom' ? 'custom window' : 'Zotero viewer') + ']',
      });
    }
  }
  const ctl = {
    steps,
    state: { busy: false, awaitingUser: false, status: {}, logLines: SP.log },
    onLog: null,
    onChange: null,
    current: null,
    changed() {
      if (typeof this.onChange === 'function') {
        try { this.onChange(); } catch (e) {}
      }
    },

    async start(stepId, opts) {
      if (this.state.busy) {
        throw new Error('a step is already running');
      }
      const step = steps.find(s => s.id === stepId);
      const flow = SP.config.flows.find(f => f.id === step.flowId);
      this.state.busy = true;
      this.state.status[stepId] = 'preparing';
      this.changed();
      const clear = !opts || opts.clearCookies !== false;
      if (clear) {
        Services.cookies.removeAll();
      }
      const startURL = startURLFor(flow);
      const res = {
        stepId,
        flowId: flow.id,
        kind: step.kind,
        clearedCookiesFirst: clear,
        startURL: redactURL(startURL),
        startedAt: new Date().toISOString(),
        cookieNamesBefore: cookieInventory().map(c => c.host + ' ' + c.name),
      };
      REC.start(stepId);
      CONSOLE.start();
      plog(stepId + ': pre-login baseline downloads (can take up to a minute before the sign-in window opens)');
      res.baseline = {
        httpRequestPdf: await httpTest(flow.pdfUrl),
        httpRequestPdfPlainUA: await httpTest(flow.pdfUrl, { plainUA: true }),
      };
      if (flow.unproxiedPdfUrl) {
        res.baseline.httpRequestUnproxiedPdfPlainUA = await httpTest(flow.unproxiedPdfUrl, { plainUA: true });
      }
      // The auto run showed that Cloudflare Turnstile at Wiley's SSO start does not clear with
      // Zotero's "Zotero/10.x" UA suffix but clears with a plain Firefox UA, so the sign-in
      // windows use the plain UA unless the config says loginUA: "zotero".
      const loginUA = SP.config.loginUA === 'zotero' ? 'zotero' : 'plain';
      res.loginUA = loginUA;
      plog(stepId + ': opening the ' + (step.kind === 'custom' ? 'custom window' : 'Zotero viewer') + '. Sign in there, then press "Finished signing in".');
      const lw = await openLoginWindow(step.kind, startURL, loginUA === 'plain' ? { customUserAgent: plainUA() } : {});
      res.browserId = lw.browser.browserId;
      res.navigation = [];
      let last = '';
      const t0 = Date.now();
      const poll = setInterval(() => {
        const cur = currentSpec(lw.browser);
        if (cur && cur !== last && res.navigation.length < 300) {
          last = cur;
          res.navigation.push({ atMs: Date.now() - t0, url: redactURL(cur) });
        }
        if (lw.isClosed() && !res.windowClosedByUserAtMs) {
          res.windowClosedByUserAtMs = Date.now() - t0;
          plog(stepId + ': the sign-in window was closed');
        }
      }, 400);
      this.current = { step, flow, res, lw, poll, t0 };
      this.state.awaitingUser = true;
      this.state.status[stepId] = 'waiting for you to sign in';
      this.changed();
    },

    async _collectLoginWindow(cur, note) {
      clearInterval(cur.poll);
      const { res, lw } = cur;
      res.userNote = String(note || '').slice(0, 500);
      res.signInMs = Date.now() - cur.t0;
      res.loginWindow = {
        closedByUser: lw.isClosed(),
        finalURL: lw.isClosed() ? null : redactURL(currentSpec(lw.browser)),
        finalHost: lw.isClosed() ? null : hostOf(currentSpec(lw.browser)),
        page: lw.isClosed() ? null : await inspect(lw.browser, false),
        events: lw.events,
      };
      res.cookiesAfterSignIn = cookieInventory();
    },

    async finish(note) {
      const cur = this.current;
      if (!cur) {
        return;
      }
      this.state.awaitingUser = false;
      this.state.status[cur.step.id] = 'running checks (1-4 min)';
      this.changed();
      await this._collectLoginWindow(cur, note);
      const { flow, res, lw } = cur;
      res.checks = {};
      plog(cur.step.id + ': T1 Zotero.HTTP.request -> PDF URL (Zotero UA, then plain UA)');
      res.checks.T1_httpRequest_pdf = await httpTest(flow.pdfUrl);
      res.checks.T1p_httpRequest_pdf_plainUA = await httpTest(flow.pdfUrl, { plainUA: true });
      plog(cur.step.id + ': T2 HiddenBrowser + PDF MIME capture (Zotero UA, then plain UA)');
      res.checks.T2_hiddenBrowser_pdf = await hiddenPdfTest(flow.pdfUrl);
      res.checks.T2p_hiddenBrowser_pdf_plainUA = await hiddenPdfTest(flow.pdfUrl, { plainUA: true });
      plog(cur.step.id + ': T3 Zotero.Attachments.addAvailableFile');
      res.checks.T3_zotero_addAvailableFile = await zoteroFindTest(flow);
      if (flow.second) {
        plog(cur.step.id + ': T4 HiddenBrowser (plain UA) runs the start URL for a second article');
        res.checks.T4_hiddenBrowser_secondArticle_startURL = await hiddenNavigateTest(startURLFor(flow, flow.second.target), 45000, { plainUA: true });
        plog(cur.step.id + ': T5 Zotero.HTTP.request -> second PDF (plain UA)');
        res.checks.T5_httpRequest_secondArticle_pdf = await httpTest(flow.second.pdfUrl, { plainUA: true });
      }
      if (flow.itemUrl) {
        res.checks.T6_httpRequest_proxiedLanding = await httpTest(flow.itemUrl);
      }
      lw.close();
      await sleep(500);
      res.consoleErrors = CONSOLE.stop();
      res.network = REC.stop();
      res.finishedAt = new Date().toISOString();
      SP.results.manual.push(res);
      const c = res.checks;
      const yn = v => (v ? 'yes' : 'no');
      const summary = 'done: T1 pdf ' + yn(c.T1_httpRequest_pdf && c.T1_httpRequest_pdf.isPDF)
        + ' (plain UA ' + yn(c.T1p_httpRequest_pdf_plainUA && c.T1p_httpRequest_pdf_plainUA.isPDF) + ')'
        + ', T2 pdf ' + yn(c.T2_hiddenBrowser_pdf && c.T2_hiddenBrowser_pdf.isPDF)
        + ' (plain UA ' + yn(c.T2p_hiddenBrowser_pdf_plainUA && c.T2p_hiddenBrowser_pdf_plainUA.isPDF) + ')'
        + ', T3 attached ' + yn(c.T3_zotero_addAvailableFile && c.T3_zotero_addAvailableFile.attached)
        + (c.T5_httpRequest_secondArticle_pdf ? ', T5 pdf ' + yn(c.T5_httpRequest_secondArticle_pdf.isPDF) : '');
      this.state.status[cur.step.id] = summary;
      plog(cur.step.id + ': ' + summary);
      this.current = null;
      this.state.busy = false;
      await writePartial();
      this.changed();
    },

    async abandon(note) {
      const cur = this.current;
      if (!cur) {
        return;
      }
      this.state.awaitingUser = false;
      await this._collectLoginWindow(cur, note);
      cur.lw.close();
      await sleep(300);
      cur.res.abandoned = true;
      cur.res.consoleErrors = CONSOLE.stop();
      cur.res.network = REC.stop();
      cur.res.finishedAt = new Date().toISOString();
      SP.results.manual.push(cur.res);
      this.state.status[cur.step.id] = 'abandoned';
      plog(cur.step.id + ': abandoned');
      this.current = null;
      this.state.busy = false;
      await writePartial();
      this.changed();
    },

    async writeAndQuit(reason) {
      if (this.current) {
        try { await this.abandon('(auto-abandoned at quit: ' + reason + ')'); } catch (e) {}
      }
      await finish(reason);
    },
  };
  return ctl;
}

async function manualMain() {
  SP.results.manual = [];
  SP.results.externalLaunchesBlocked = [];
  // Zotero's viewer hands plain <a> clicks to the system browser (ExternalLinkHandler actor).
  // Record and block that here, so a session never lands in the wrong browser during the spike.
  SP.origLaunchURL = Zotero.launchURL;
  Zotero.launchURL = function (url) {
    const rec = { atMs: Date.now() - SP.t0, url: redactURL(String(url)) };
    SP.results.externalLaunchesBlocked.push(rec);
    plog('blocked a hand-off to the system browser: ' + rec.url);
  };
  SP.ctl = makeController();
  const arg = { ctl: SP.ctl };
  arg.wrappedJSObject = arg;
  const win = Services.ww.openWindow(null, BASE + 'control.xhtml', 'sso-spike-control', 'chrome,centerscreen,resizable,dialog=no', arg);
  await waitForLoad(win);
  win.addEventListener('unload', () => {
    if (!SP.finished) {
      SP.ctl.writeAndQuit('control-window-closed');
    }
  });
  plog('manual mode: control window open');
  let smoke = false;
  try { smoke = Services.prefs.getBoolPref('extensions.sso-spike.smokeTest', false); } catch (e) {}
  if (smoke) {
    await manualSmokeTest(win);
  }
}

// Exercises the manual-mode code paths without a person and without signing in: the IdP POST
// guard stays on, each step "finishes" while the window still shows the sign-in page, so every
// check must come back as not-a-PDF. Used only to validate the spike before handing it over.
async function manualSmokeTest(win) {
  REC.guard = true;
  SP.results.meta.smokeTest = true;
  const ctl = SP.ctl;
  const doc = win.document;
  await sleep(1500);
  SP.results.smoke = {
    controlWindowRows: doc.querySelectorAll('#ssp-steps tr').length,
    finishButtonDisabledAtStart: doc.getElementById('ssp-finish').disabled,
  };
  let steps = [['oa-wiley:custom', 25000], ['ezp-ajp:viewer', 12000]];
  try {
    const only = Services.prefs.getStringPref('extensions.sso-spike.smokeSteps', '');
    if (only) {
      steps = only.split(',').map(s => [s.trim(), 12000]);
    }
  }
  catch (e) {}
  for (const [stepId, waitMs] of steps) {
    await ctl.start(stepId, { clearCookies: true });
    SP.results.smoke[stepId + ':finishEnabledWhileWaiting'] = !doc.getElementById('ssp-finish').disabled;
    await sleep(waitMs);
    await ctl.finish('smoke test: no sign-in attempted');
  }
  SP.results.smoke.statusTexts = ctl.state.status;
  SP.results.smoke.controlWindowRowsAtEnd = doc.querySelectorAll('#ssp-steps tr').length;
  SP.results.smoke.controlWindowStatusCells = Array.from(doc.querySelectorAll('#ssp-steps tr td:last-child')).map(td => td.textContent).slice(0, 8);
  SP.results.smoke.controlLogLines = (doc.getElementById('ssp-log').value || '').split('\n').length;
  await ctl.writeAndQuit('smoke-test-complete');
}

// ------------------------------------------------------------------------------------------
// result file and lifecycle
// ------------------------------------------------------------------------------------------
function getResultFile() {
  let p = '';
  try { p = Services.prefs.getStringPref('extensions.zotero-harness.resultFile', ''); } catch (e) {}
  if (!p) {
    try { p = Services.env.get('ZOTERO_HARNESS_RESULT_FILE'); } catch (e) {}
  }
  return p;
}

async function writePartial() {
  const f = getResultFile();
  if (!f) {
    return;
  }
  try {
    SP.results.log = SP.log;
    await IOUtils.writeUTF8(f + '.partial.json', JSON.stringify(SP.results, null, 2), { tmpPath: f + '.partial.tmp' });
  }
  catch (e) {
    plog('writePartial failed: ' + errStr(e));
  }
}

async function finish(reason) {
  if (!SP || SP.finished) {
    return;
  }
  SP.finished = true;
  const R = SP.results;
  R.meta.finishReason = reason;
  R.meta.finishedAt = new Date().toISOString();
  R.meta.totalMs = Date.now() - SP.t0;
  R.log = SP.log;
  // wipe session state before quitting (the runbook also deletes the throwaway profile)
  try { Services.cookies.removeAll(); R.meta.cookiesClearedAtEnd = true; } catch (e) { R.meta.cookiesClearedAtEnd = errStr(e); }
  try { Services.cache2.clear(); } catch (e) {}
  const f = getResultFile();
  try {
    if (f) {
      await IOUtils.writeUTF8(f, JSON.stringify(R, null, 2), { tmpPath: f + '.tmp' });
      try { await IOUtils.remove(f + '.partial.json', { ignoreAbsent: true }); } catch (e) {}
    }
  }
  catch (e) {
    Zotero.debug('[sso-spike] result write failed: ' + e);
  }
  let quit = true;
  try { quit = Services.prefs.getBoolPref('extensions.zotero-harness.quitWhenDone', true); } catch (e) {}
  if (quit) {
    setTimeout(() => {
      try { Services.startup.quit(CI.nsIAppStartup.eAttemptQuit); } catch (e) {}
    }, 500);
  }
}

async function main() {
  try {
    await Zotero.initializationPromise;
    await Promise.race([Zotero.uiReadyPromise, sleep(60000)]);
  }
  catch (e) {
    plog('waiting for Zotero failed: ' + errStr(e));
  }
  SP.tmpDir = PathUtils.join(PathUtils.profileDir, 'sso-spike-tmp');
  await IOUtils.makeDirectory(SP.tmpDir, { ignoreExisting: true });
  let cfgPath = '';
  try { cfgPath = Services.prefs.getStringPref('extensions.sso-spike.configFile', ''); } catch (e) {}
  SP.config = JSON.parse(JSON.stringify(DEFAULT_CONFIG));
  if (cfgPath) {
    try {
      const user = await IOUtils.readJSON(cfgPath);
      Object.assign(SP.config, user);
      SP.results.meta.configFile = cfgPath;
    }
    catch (e) {
      SP.results.meta.configFileError = errStr(e);
    }
  }
  SP.results.config = SP.config;
  SP.results.environment = collectEnvironment();
  for (const t of REC_TOPICS) {
    Services.obs.addObserver(REC, t);
  }
  SP.observersAdded = true;
  try {
    Services.console.registerListener(CONSOLE.listener);
    SP.consoleListenerAdded = true;
  }
  catch (e) {
    SP.results.meta.consoleListenerError = errStr(e);
  }
  if (SP.mode === 'manual') {
    SP.watchdog = setTimeout(() => finish('watchdog'), MANUAL_WATCHDOG_MS);
    await manualMain();
    return;
  }
  SP.watchdog = setTimeout(() => finish('watchdog'), AUTO_WATCHDOG_MS);
  await autoMain();
  await finish('auto-complete');
}

function install() {}
function uninstall() {}

function startup(params) {
  let mode = 'auto';
  try { mode = Services.prefs.getStringPref('extensions.sso-spike.mode', 'auto'); } catch (e) {}
  SP = {
    t0: Date.now(),
    log: [],
    mode,
    finished: false,
    ctl: null,
    results: {
      schema: 'zotero-fulltext/sso-spike/v1',
      meta: {
        mode,
        plugin: { id: params.id, version: params.version, rootURI: params.rootURI },
        startedAt: new Date().toISOString(),
      },
    },
  };
  const aomStartup = CC['@mozilla.org/addons/addon-manager-startup;1'].getService(CI.amIAddonManagerStartup);
  chromeHandle = aomStartup.registerChrome(Services.io.newURI(params.rootURI + 'manifest.json'), [['content', PKG, 'content/']]);
  try {
    ChromeUtils.registerWindowActor(ACTOR, {
      child: { esModuleURI: BASE + 'SsoSpikeChild.mjs' },
      allFrames: false,
    });
  }
  catch (e) {
    SP.results.meta.actorError = errStr(e);
  }
  main().catch((e) => {
    SP.results.meta.mainError = errStr(e) + ' | ' + (e && e.stack ? String(e.stack).split('\n').slice(0, 4).join(' | ') : '');
    finish('main-error');
  });
}

function shutdown() {
  if (SP && SP.observersAdded) {
    for (const t of REC_TOPICS) {
      try { Services.obs.removeObserver(REC, t); } catch (e) {}
    }
  }
  if (SP && SP.consoleListenerAdded) {
    try { Services.console.unregisterListener(CONSOLE.listener); } catch (e) {}
  }
  if (SP && SP.origLaunchURL) {
    Zotero.launchURL = SP.origLaunchURL;
  }
  if (SP && SP.watchdog) {
    clearTimeout(SP.watchdog);
  }
  try { ChromeUtils.unregisterWindowActor(ACTOR); } catch (e) {}
  try { if (chromeHandle) chromeHandle.destruct(); } catch (e) {}
  chromeHandle = null;
}
