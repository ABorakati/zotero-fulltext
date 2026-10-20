/* login.js: DOM glue for login.xhtml (owner: js-ui). NOT in ZFT.LOAD_ORDER: runs in the window's own global.
 *
 * Port of tools/sso-spike/plugin/content/login.js with the sspXxx ids renamed to zft-login-*. Contract with ZFT.login
 * (login-controller.js): window.arguments[0].wrappedJSObject = {
 *   entryUrl, itemTitle, waiting, kind: 'login'|'challenge', realm, timeoutMs,
 *   onEvent(ev)         ev = {type: 'popup', via, url} | {type: 'popup-error', stage, error} | {type: 'skip'} |
 *                       {type: 'handoff'} | {type: 'keep_waiting'} | {type: 'closed'},
 *   format(id, args)    ZFT.l10n.format
 * }
 * Responsibilities:
 *  - keep the origin / TLS bar current every 700 ms (browser.currentURI.prePath, browsingContext.secureBrowserUI.state
 *    & nsIWebProgressListener.STATE_IS_SECURE); set document.title to "<login-title>: <origin>";
 *  - implement nsIBrowserDOMWindow (window.browserDOMWindow = new ZftBrowserAccess()) so window.open / target=_blank
 *    pages open in the visible popup pane (#zft-login-popup-box) instead of a hidden browser; count popups;
 *  - wire the three buttons to args.onEvent({type: 'skip'|'handoff'|'keep_waiting'}); the controller does the rest;
 *  - setLabels({waiting, hint, showKeepWaiting}): called by the controller through window.zftLogin.setLabels() to update
 *    the "N items waiting" text (data-l10n-id zotero-fulltext-login-waiting with {count}), the hint line (a string, or
 *    {id, args} for a Fluent message; '' clears it) and the visibility of the Keep waiting button. Labels that arrive
 *    before the page has loaded are kept and applied on load;
 *  - 'unload' -> args.onEvent({type: 'closed'}).
 * It never loads a URL itself and never reads credentials.
 */
/* global window, document, Components, ChromeUtils, Services */
(function () {
  'use strict';

  const HTML_NS = 'http://www.w3.org/1999/xhtml';
  const BAR_INTERVAL_MS = 700;
  const FALLBACK_TITLE = 'Sign in';

  const arg0 = window.arguments && window.arguments[0];
  const args = (arg0 && (arg0.wrappedJSObject || arg0)) || {};

  let ready = false;
  let popupCount = 0;
  let barTimer = null;
  let titleBase = null;
  let pendingLabels = {};
  const barState = { secure: null, origin: null };

  function el(id) {
    return document.getElementById(id);
  }

  /** Tell the controller something happened. Never throws into page code. */
  function emit(ev) {
    try {
      if (typeof args.onEvent === 'function') {
        args.onEvent(ev);
      }
    } catch (e) {
      try { Components.utils.reportError(e); } catch (e2) { /* nothing left to do */ }
    }
  }

  function format(id, a) {
    try {
      if (typeof args.format === 'function') {
        return String(args.format(id, a) || '');
      }
    } catch (e) { /* fall through */ }
    return '';
  }

  /** Show a Fluent message in node, or clear it when id is falsy. */
  function setMessage(node, id, a) {
    if (!node) {
      return;
    }
    if (id) {
      document.l10n.setAttributes(node, id, a || null);
    } else {
      node.removeAttribute('data-l10n-id');
      node.removeAttribute('data-l10n-args');
      node.textContent = '';
    }
  }

  function applyLabels(labels) {
    if ('waiting' in labels) {
      const n = Number(labels.waiting) || 0;
      // 0 empties the text (the element stays as the banner's flexible spacer).
      setMessage(el('zft-login-waiting'), n > 0 ? 'zotero-fulltext-login-waiting' : '', { count: n });
    }
    if ('hint' in labels) {
      const hintEl = el('zft-login-hint');
      const hint = labels.hint;
      if (hint && typeof hint === 'object') {
        setMessage(hintEl, String(hint.id || ''), hint.args || null);
      } else {
        setMessage(hintEl, '');
        if (hintEl) {
          hintEl.textContent = hint ? String(hint) : '';
        }
      }
    }
    if ('showKeepWaiting' in labels) {
      const keep = el('zft-login-keep');
      if (keep) {
        keep.hidden = !labels.showKeepWaiting;
      }
    }
  }

  window.zftLogin = {
    /** Called by ZFT.login to update the banner. @param {{waiting?: number, hint?: (string|{id: string, args?: object}), showKeepWaiting?: boolean}} labels */
    setLabels(labels) {
      if (!labels || typeof labels !== 'object') {
        return;
      }
      if (!ready) {
        pendingLabels = Object.assign(pendingLabels, labels);
        return;
      }
      applyLabels(labels);
    },
  };

  /** Origin / TLS bar and window title. */
  function updateBar() {
    const browser = el('zft-login-browser');
    const secureEl = el('zft-login-secure');
    const originEl = el('zft-login-origin');
    if (!browser || !secureEl || !originEl) {
      return;
    }
    let spec = '';
    let prePath = '';
    try {
      spec = browser.currentURI ? browser.currentURI.spec : '';
      prePath = browser.currentURI ? browser.currentURI.prePath : '';
    } catch (e) { /* between navigations */ }
    const blank = !spec || spec === 'about:blank';
    let state = 0;
    try {
      state = (browser.browsingContext && browser.browsingContext.secureBrowserUI)
        ? browser.browsingContext.secureBrowserUI.state : 0;
    } catch (e) { /* no UI state yet */ }
    const secure = !blank && !!(state & Components.interfaces.nsIWebProgressListener.STATE_IS_SECURE);

    const origin = blank ? '' : (prePath || spec);
    if (barState.origin !== origin) {
      barState.origin = origin;
      originEl.textContent = origin;
      if (titleBase === null) {
        titleBase = format('zotero-fulltext-login-title') || FALLBACK_TITLE;
      }
      document.title = origin ? titleBase + ': ' + origin : titleBase;
    }
    const secureKey = blank ? 'none' : (secure ? 'yes' : 'no');
    if (barState.secure !== secureKey) {
      barState.secure = secureKey;
      setMessage(secureEl, blank ? '' : (secure ? 'zotero-fulltext-login-secure' : 'zotero-fulltext-login-not-secure'));
      secureEl.style.color = secure ? '#060' : '#a00';
    }
  }

  /**
   * nsIBrowserDOMWindow: SSO pages that call window.open() or use target=_blank get a visible pane under the page
   * (Zotero's own browserWindowShim.js puts popups in a hidden <browser>, which would hide a whole SSO step).
   */
  class ZftBrowserAccess {
    QueryInterface = ChromeUtils.generateQI(['nsIBrowserDOMWindow']);

    _newPopupBrowser(aURI, aOpenWindowInfo, aTriggeringPrincipal, aSkipLoad, via) {
      popupCount++;
      emit({ type: 'popup', via, url: aURI ? aURI.spec : '(about:blank)' });
      const box = el('zft-login-popup-box');
      box.hidden = false;
      setMessage(el('zft-login-popups'), 'zotero-fulltext-login-popups', { count: popupCount });
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
      } catch (e) {
        emit({ type: 'popup-error', stage: 'openWindowInfo', error: String(e) });
      }
      box.appendChild(b);
      if (aURI && !aSkipLoad) {
        try {
          b.loadURI(aURI, {
            triggeringPrincipal: aTriggeringPrincipal || Services.scriptSecurityManager.getSystemPrincipal(),
          });
        } catch (e) {
          emit({ type: 'popup-error', stage: 'loadURI', error: String(e) });
        }
      }
      return b;
    }

    createContentWindow(aURI, aOpenWindowInfo, aWhere, aFlags, aTriggeringPrincipal, aCsp) {
      const b = this._newPopupBrowser(aURI, aOpenWindowInfo, aTriggeringPrincipal, true, 'createContentWindow');
      return b ? b.browsingContext : null;
    }

    createContentWindowInFrame(aURI, aParams, aWhere, aFlags, aName) {
      return this._newPopupBrowser(aURI, aParams && aParams.openWindowInfo, aParams && aParams.triggeringPrincipal, true, 'createContentWindowInFrame');
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
    window.browserDOMWindow = new ZftBrowserAccess();
  });

  window.addEventListener('load', () => {
    ready = true;
    const wire = (id, type) => {
      const button = el(id);
      if (button) {
        button.addEventListener('click', () => emit({ type }));
      }
    };
    wire('zft-login-keep', 'keep_waiting');
    wire('zft-login-handoff', 'handoff');
    wire('zft-login-skip', 'skip');
    applyLabels(pendingLabels);
    pendingLabels = {};
    updateBar();
    barTimer = window.setInterval(updateBar, BAR_INTERVAL_MS);
  });

  window.addEventListener('unload', () => {
    if (barTimer !== null) {
      window.clearInterval(barTimer);
      barTimer = null;
    }
    emit({ type: 'closed' });
  });
})();
