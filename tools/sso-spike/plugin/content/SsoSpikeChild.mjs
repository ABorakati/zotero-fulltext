// sso-spike window actor (child side). Answers 'inspect' with facts about the current top-level
// document. It never returns form values, cookies, or (in manual mode) page text.

const TEXT_SAMPLE_HOSTS = new Set([
  'login.microsoftonline.com',
  'go.openathens.net',
  'login.openathens.net',
  'login.kcl.idm.oclc.org',
]);

function withTimeout(promise, ms, win) {
  return Promise.race([
    promise,
    new Promise(resolve => win.setTimeout(() => resolve('timeout'), ms)),
  ]);
}

export class SsoSpikeChild extends JSWindowActorChild {
  async receiveMessage(message) {
    if (message.name === 'ping') {
      let remoteType = null;
      try { remoteType = Services.appinfo.remoteType; } catch (e) {}
      return { ok: true, remoteType };
    }
    if (message.name !== 'inspect') {
      return null;
    }
    const allowText = !!(message.data && message.data.allowText);
    const doc = this.document;
    const win = this.contentWindow;
    if (doc.readyState === 'loading') {
      await withTimeout(new Promise(r => doc.addEventListener('DOMContentLoaded', r, { once: true })), 5000, win);
    }
    const q = sel => {
      try { return !!doc.querySelector(sel); } catch (e) { return null; }
    };
    let html = '';
    try { html = doc.documentElement ? doc.documentElement.outerHTML.slice(0, 300000) : ''; } catch (e) {}
    let text = '';
    try { text = doc.body ? doc.body.innerText || '' : ''; } catch (e) {}

    let uvpaa = null;
    let hasPKC = false;
    try {
      hasPKC = typeof win.PublicKeyCredential === 'function';
      if (hasPKC && typeof win.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable === 'function') {
        uvpaa = await withTimeout(win.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable(), 3000, win);
      }
    } catch (e) {
      uvpaa = 'error:' + (e && e.name);
    }

    let host = '';
    try { host = win.location.host; } catch (e) {}

    return {
      readyState: doc.readyState,
      contentType: doc.contentType,
      title: String(doc.title || '').slice(0, 200),
      host,
      // Microsoft Entra sign-in page markers
      hasLoginfmt: q('input[name="loginfmt"]'),
      hasPasswordField: q('input[type="password"]'),
      // EZproxy SAML hand-off page
      hasEZproxyForm: q('form[name="EZproxyForm"]'),
      // generic auto-POST SAML forms (never submitted by this actor)
      hasSAMLRequestField: q('input[name="SAMLRequest"]'),
      hasSAMLResponseField: q('input[name="SAMLResponse"]'),
      unsupportedBrowserHint: /not supported|unsupported browser|update your browser|browser (?:is|isn't|is not) (?:supported|compatible)/i.test(text),
      cloudflareChallenge: /<title>\s*Just a moment|challenges\.cloudflare\.com|cf-chl-|cf_chl_/i.test(html),
      openAthensPleaseWait: /OpenAthens\s*\/\s*Please wait/i.test(String(doc.title || '')),
      pdfViewer: doc.contentType === 'application/pdf' || /pdf\.js|pdfjs|viewerContainer/.test(html.slice(0, 20000)),
      hasPublicKeyCredential: hasPKC,
      uvpaa,
      cookieEnabled: !!win.navigator.cookieEnabled,
      userAgent: String(win.navigator.userAgent),
      textSample: allowText && TEXT_SAMPLE_HOSTS.has(host) ? text.replace(/\s+/g, ' ').trim().slice(0, 300) : null,
    };
  }
}
