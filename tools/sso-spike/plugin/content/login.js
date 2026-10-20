/* sso-spike custom login window script.
 * The bootstrap opens this window, waits for 'load', and then loads the start URL itself.
 * This script only keeps the origin/TLS bar current and handles window.open/target=_blank
 * by showing the popup in a visible pane (Zotero's own browserWindowShim.js puts popups in a
 * hidden <browser>, which would hide any SSO step that opens a new window).
 */
/* global Services, ChromeUtils, Components */

var sspArgs = (window.arguments && window.arguments[0] && window.arguments[0].wrappedJSObject) || {};
var sspPopupCount = 0;

function sspEmit(ev) {
  try {
    if (typeof sspArgs.onEvent === 'function') {
      sspArgs.onEvent(ev);
    }
  }
  catch (e) {
    Components.utils.reportError(e);
  }
}

function sspUpdateBar() {
  const browser = document.getElementById('ssp-browser');
  const secureEl = document.getElementById('ssp-secure');
  const originEl = document.getElementById('ssp-origin');
  if (!browser || !secureEl || !originEl) {
    return;
  }
  let spec = '';
  let prePath = '';
  try {
    spec = browser.currentURI ? browser.currentURI.spec : '';
    prePath = browser.currentURI ? browser.currentURI.prePath : '';
  }
  catch (e) {}
  let state = 0;
  try {
    state = (browser.browsingContext && browser.browsingContext.secureBrowserUI)
      ? browser.browsingContext.secureBrowserUI.state : 0;
  }
  catch (e) {}
  const secure = !!(state & Components.interfaces.nsIWebProgressListener.STATE_IS_SECURE);
  secureEl.textContent = secure ? 'HTTPS ok' : (spec.startsWith('https:') ? 'HTTPS ?' : 'NOT SECURE');
  secureEl.style.color = secure ? '#060' : '#a00';
  originEl.textContent = prePath || spec || '(loading)';
  document.title = 'Sign in: ' + (prePath || '(loading)') + ' (sso-spike custom window)';
}

class SspBrowserAccess {
  QueryInterface = ChromeUtils.generateQI(['nsIBrowserDOMWindow']);

  _newPopupBrowser(aURI, aOpenWindowInfo, aTriggeringPrincipal, aSkipLoad, via) {
    sspPopupCount++;
    const url = aURI ? aURI.spec : '(about:blank)';
    sspEmit({ type: 'popup', via, url });
    const box = document.getElementById('ssp-popup-box');
    box.hidden = false;
    document.getElementById('ssp-popups').textContent = 'popups opened: ' + sspPopupCount;
    const label = document.createXULElement('label');
    label.setAttribute('value', 'Popup #' + sspPopupCount + ' (opened by the page)');
    box.appendChild(label);
    const b = document.createXULElement('browser');
    b.setAttribute('type', 'content');
    b.setAttribute('flex', '1');
    b.setAttribute('maychangeremoteness', 'true');
    b.setAttribute('disableglobalhistory', 'true');
    try {
      if (aOpenWindowInfo) {
        b.openWindowInfo = aOpenWindowInfo;
        const opener = aOpenWindowInfo.parent && aOpenWindowInfo.parent.top
          && aOpenWindowInfo.parent.top.embedderElement;
        if (opener && opener.remoteType) {
          b.setAttribute('remote', 'true');
          b.setAttribute('remoteType', opener.remoteType);
        }
      }
    }
    catch (e) {
      sspEmit({ type: 'popup-error', stage: 'openWindowInfo', error: String(e) });
    }
    box.appendChild(b);
    if (aURI && !aSkipLoad) {
      try {
        b.loadURI(aURI, {
          triggeringPrincipal: aTriggeringPrincipal || Services.scriptSecurityManager.getSystemPrincipal(),
        });
      }
      catch (e) {
        sspEmit({ type: 'popup-error', stage: 'loadURI', error: String(e) });
      }
    }
    return b;
  }

  createContentWindow(aURI, aOpenWindowInfo, aWhere, aFlags, aTriggeringPrincipal, aCsp) {
    const b = this._newPopupBrowser(aURI, aOpenWindowInfo, aTriggeringPrincipal, true, 'createContentWindow');
    return b ? b.browsingContext : null;
  }

  createContentWindowInFrame(aURI, aParams, aWhere, aFlags, aName) {
    const b = this._newPopupBrowser(aURI, aParams && aParams.openWindowInfo, aParams && aParams.triggeringPrincipal, true, 'createContentWindowInFrame');
    return b;
  }

  openURI(aURI, aOpenWindowInfo, aWhere, aFlags, aTriggeringPrincipal, aCsp) {
    const b = this._newPopupBrowser(aURI, aOpenWindowInfo, aTriggeringPrincipal, false, 'openURI');
    return b ? b.browsingContext : null;
  }

  openURIInFrame(aURI, aParams, aWhere, aFlags, aName) {
    return this._newPopupBrowser(aURI, aParams && aParams.openWindowInfo, aParams && aParams.triggeringPrincipal, false, 'openURIInFrame');
  }

  canClose() {
    return true;
  }

  get tabCount() {
    return 1;
  }
}

window.addEventListener('DOMContentLoaded', () => {
  window.browserDOMWindow = new SspBrowserAccess();
});

window.addEventListener('load', () => {
  sspUpdateBar();
  window.setInterval(sspUpdateBar, 700);
});
