/* prefs-pane.js: script of the preferences pane (owner: js-shell). NOT in ZFT.LOAD_ORDER: Zotero loads it into the
 * preferences window together with prefs.xhtml (Zotero.PreferencePanes.register({scripts: [...]})).
 *
 * Zotero evaluates this file in a Cu.Sandbox whose prototype is the preferences window (preferences.js:_loadPane), BEFORE the
 * markup exists: `document`, `window`, `Zotero` and `Services` resolve through the prototype, but a top-level `var` stays in the
 * sandbox. The inline handler in prefs.xhtml (onload="ZoteroFullTextPrefs.init()") runs in the window, so the object is also
 * published as window.ZoteroFullTextPrefs. Zotero dispatches 'load' at each top-level child of the pane after inserting it.
 *
 * That global has `Zotero` but no `ZFT`; the plugin exposes its namespace as Zotero.ZoteroFullText (set by ZFT.plugin.startup,
 * deleted on shutdown). init() runs when the pane is mounted:
 *   1. const ZFT = Zotero.ZoteroFullText; bail out with a visible message if it is missing (plugin disabled).
 *   2. Build #zft-prefs-root from ZFT.config.PANE_LAYOUT: one group (heading + rows) per layout entry; checkbox ->
 *      <html:input type=checkbox>, text/number -> <html:input>, select -> <html:select>, list -> <html:input> showing the comma
 *      list; secret rows (*_key) are <html:input type=password> with a Show/Hide toggle. Every label is an element with
 *      data-l10n-id (value messages in the FTL).
 *   3. Bind each control to its pref WITHOUT PreferencesBindings: initial value from ZFT.config.getPref(key); on 'change'
 *      coerce (ints clamped to the row's min/max, text trimmed, lists normalised, selects checked against their options) and
 *      ZFT.config.setPref(key, value); then ZFT.config.problems() -> show problems (l10n ids) in #zft-prefs-problems; then
 *      ZFT.runs.reconfigure() (deferred by the controller while runs are active).
 *   4. Buttons: reset-institution (applies the King's College London preset), clear-state (ZFT.state.reset(); confirm first),
 *      open-data (Zotero.File.reveal(ZFT.state.dataDir())).
 *   5. Institution picker (rows with `section` in PANE_LAYOUT's institution group): the dropdown shows ZFT.config.effectivePreset()
 *      (a preset from ZFT.config.INSTITUTIONS, "Other" or "None"). Choosing an entry calls ZFT.config.applyPreset(), which writes all
 *      institution.* prefs at once plus institution.preset and inst.enabled, then refreshes every control. "Other" shows the name /
 *      OpenAthens domain / EZproxy fields (host-name format checked on change) and a Test button: ONE anonymous GET of the public
 *      OpenAthens redirector for a public article, redirects not followed, no sign-in, reported as recognised / unknown organisation.
 *      Editing any institution detail by hand turns the dropdown into Other (the data decides what the engine uses).
 *      Under the dropdown a read-only line lists the methods that are set up and switched on ("Uses: OpenAthens, Shibboleth / UK
 *      federation"). "Other" has a method selector with one panel each: OpenAthens (domain + Test as above); Shibboleth / UK
 *      federation (a name search of the public SeamlessAccess directory, ONE anonymous GET of md.seamlessaccess.org with the typed
 *      text only, a result list, "Use this identity provider" stores institution.saml_entity_id and shows the chosen name); library
 *      proxy (a pasted EZproxy address such as https://ezproxy.example.ac.uk/login?url= is parsed into ezproxy_login_host and
 *      ezproxy_suffix, "Use the proxy for every publisher" = institution.ezproxy_all_hosts, Test = ONE anonymous GET of the proxy's
 *      login page for a public publisher home page, redirects not followed, nothing submitted). A problem line appears while
 *      institutional access is on and no method is set up (ZFT.config.problems()).
 * init() is idempotent (a second call on the same mount point does nothing); destroy() removes every listener and the built DOM,
 * and runs on window unload. The pure helpers buildRow() and coerce() are exported for tests.
 */
/* global window, document, Zotero, Services */
'use strict';

var ZoteroFullTextPrefs = (function () {
  const HTML_NS = 'http://www.w3.org/1999/xhtml';
  const ROOT_ID = 'zft-prefs-root';
  const PROBLEMS_ID = 'zft-prefs-problems';
  const STATUS_ID = 'zft-prefs-status';
  const UNAVAILABLE_ID = 'zotero-fulltext-prefs-unavailable';
  const DOMAIN_INVALID_ID = 'zotero-fulltext-prefs-institution-domain-invalid';
  const CUSTOM_ID = 'zft-institution-custom';
  const ADVANCED_ID = 'zft-institution-advanced';
  const TEST_BUTTON_ID = 'zft-institution-test';
  const TEST_RESULT_ID = 'zft-institution-test-result';
  const TEST_TIMEOUT_MS = 15000;
  const ENTITY_INVALID_ID = 'zotero-fulltext-prefs-saml-entity-invalid';
  const METHODS_ID = 'zft-institution-methods';
  const METHOD_SELECT_ID = 'zft-institution-method';
  const METHOD_PANEL_PREFIX = 'zft-method-';           // + 'openathens' | 'saml' | 'ezproxy'
  const SAML_QUERY_ID = 'zft-saml-query';
  const SAML_SEARCH_ID = 'zft-saml-search';
  const SAML_RESULTS_ID = 'zft-saml-results';
  const SAML_USE_ID = 'zft-saml-use';
  const SAML_STATUS_ID = 'zft-saml-status';
  const SAML_CHOSEN_ID = 'zft-saml-chosen';
  const PROXY_PREFIX_ID = 'zft-proxy-prefix';
  const PROXY_TEST_ID = 'zft-proxy-test';
  const PROXY_RESULT_ID = 'zft-proxy-test-result';
  const METHOD_NAME_IDS = {
    openathens: 'zotero-fulltext-prefs-method-openathens',
    saml: 'zotero-fulltext-prefs-method-saml',
    ezproxy: 'zotero-fulltext-prefs-method-ezproxy',
  };

  /** The live mount: {root, ZFT, listeners, controls, byKey, addedL10n} or null. */
  let mounted = null;

  function getZFT() {
    try { return Zotero.ZoteroFullText || null; } catch (e) { return null; }
  }

  function log(level, msg) {
    const ZFT = getZFT();
    try {
      if (ZFT && ZFT.util) ZFT.util.log(level, 'prefs pane: ' + msg);
      else Zotero.debug('[zotero-fulltext] prefs pane: ' + msg, 2);
    } catch (e) { /* logging must never throw */ }
  }

  // --- pure helpers ---------------------------------------------------------------------------------------------

  /** Element id of a pref's control: 'oa.openalex_key' -> 'zft-pref-oa-openalex-key'. */
  function controlId(key) {
    return 'zft-pref-' + String(key).replace(/[._]/g, '-');
  }

  /**
   * Plain descriptor of one PANE_LAYOUT row (no DOM): what element to create and with which attributes.
   * @param {object} row PANE_LAYOUT row @param {*} [value] current pref value
   * @returns {{key: string, id: string, widget: string, tag: 'input'|'select', inputType: string|null, labelId: string,
   *            secret: boolean, min?: number, max?: number, options?: Array<{value: string, labelId: string}>, value: *}}
   */
  function buildRow(row, value) {
    const d = {
      key: row.key, id: controlId(row.key), widget: row.widget, tag: 'input', inputType: 'text',
      labelId: row.label, secret: !!row.secret, value,
    };
    if (row.widget === 'checkbox') {
      d.inputType = 'checkbox';
    } else if (row.widget === 'number') {
      d.inputType = 'number';
      if (typeof row.min === 'number') d.min = row.min;
      if (typeof row.max === 'number') d.max = row.max;
    } else if (row.widget === 'select') {
      d.tag = 'select';
      d.inputType = null;
      d.options = (row.options || []).map((o) => ({ value: o.value, labelId: o.label }));
    } else if (d.secret) {
      d.inputType = 'password';
    }
    return d;
  }

  /** Comma list -> trimmed items without empties, joined with ','. */
  function normalizeList(raw) {
    return String(raw === undefined || raw === null ? '' : raw).split(',').map((s) => s.trim()).filter(Boolean).join(',');
  }

  /**
   * Turn what a control holds into the value to store.
   * @param {object} row PANE_LAYOUT row @param {*} raw control value (boolean for checkboxes, string otherwise)
   * @returns {{ok: boolean, value?: *}} ok=false means "ignore this edit and restore the control"
   */
  function coerce(row, raw) {
    switch (row.widget) {
      case 'checkbox':
        return { ok: true, value: !!raw };
      case 'number': {
        const text = String(raw === undefined || raw === null ? '' : raw).trim();
        const n = text === '' ? NaN : Number(text);
        if (!Number.isFinite(n)) return { ok: false };
        let v = Math.round(n);
        if (typeof row.min === 'number') v = Math.max(row.min, v);
        if (typeof row.max === 'number') v = Math.min(row.max, v);
        return { ok: true, value: v };
      }
      case 'select': {
        const v = String(raw);
        return (row.options || []).some((o) => o.value === v) ? { ok: true, value: v } : { ok: false };
      }
      case 'list':
        return { ok: true, value: normalizeList(raw) };
      default: {
        const text = String(raw === undefined || raw === null ? '' : raw).trim();
        if (row.format === 'entityid' && text !== '') {
          const ZFT = getZFT();
          if (ZFT && ZFT.config && typeof ZFT.config.isEntityId === 'function') {
            return ZFT.config.isEntityId(text) ? { ok: true, value: text } : { ok: false, error: ENTITY_INVALID_ID };
          }
        }
        if (row.format === 'hostname' && text !== '') {
          const ZFT = getZFT();
          if (ZFT && ZFT.config && typeof ZFT.config.parseHostname === 'function') {
            const host = ZFT.config.parseHostname(text);
            return host.ok ? { ok: true, value: host.value } : { ok: false, error: DOMAIN_INVALID_ID };
          }
        }
        return { ok: true, value: text };
      }
    }
  }

  // --- DOM helpers ----------------------------------------------------------------------------------------------

  function el(tag, attrs) {
    const e = typeof document.createElementNS === 'function' ? document.createElementNS(HTML_NS, tag) : document.createElement(tag);
    if (attrs) for (const k of Object.keys(attrs)) e.setAttribute(k, attrs[k]);
    return e;
  }

  function clear(node) {
    while (node && node.firstChild) node.removeChild(node.firstChild);
  }

  /** Localise `node` with message `id`: data-l10n-id when document.l10n exists, else the formatted text. */
  function setText(ZFT, node, id, args) {
    if (document.l10n && typeof document.l10n.setAttributes === 'function') {
      document.l10n.setAttributes(node, id, args);
    } else {
      node.textContent = ZFT.l10n.format(id, args);
    }
  }

  function listen(m, target, type, fn) {
    target.addEventListener(type, fn);
    m.listeners.push({ target, type, fn });
  }

  function readControl(control) {
    return control.type === 'checkbox' ? !!control.checked : control.value;
  }
  function writeControl(control, value) {
    if (control.type === 'checkbox') control.checked = !!value;
    else control.value = String(value === undefined || value === null ? '' : value);
  }

  function showUnavailable() {
    const box = document.getElementById(PROBLEMS_ID);
    if (!box) return;
    clear(box);
    const p = el('p');
    if (document.l10n && typeof document.l10n.setAttributes === 'function') document.l10n.setAttributes(p, UNAVAILABLE_ID);
    else p.textContent = UNAVAILABLE_ID;
    box.appendChild(p);
    box.hidden = false;
  }

  // --- behaviour ------------------------------------------------------------------------------------------------

  function setStatus(m, id, args) {
    const node = document.getElementById(STATUS_ID);
    if (!node) return;
    if (id) {
      setText(m.ZFT, node, id, args);
      node.hidden = false;
    } else {
      node.hidden = true;
    }
  }

  function refreshProblems(m) {
    const box = document.getElementById(PROBLEMS_ID);
    if (!box) return;
    let ids = [];
    try {
      ids = typeof m.ZFT.config.problems === 'function'
        ? m.ZFT.config.problems()
        : m.ZFT.config.validate(m.ZFT.config.buildEngineConfig());
    } catch (e) {
      log('error', 'problems check failed: ' + e);
    }
    clear(box);
    for (const id of ids) {
      const p = el('p');
      setText(m.ZFT, p, id);
      box.appendChild(p);
    }
    box.hidden = ids.length === 0;
  }

  function reconfigure(m) {
    Promise.resolve()
      .then(() => m.ZFT.runs.reconfigure())
      .catch((e) => { log('warn', 'reconfigure failed: ' + e); });
  }

  /** A pref changed: refresh the problem list and tell the run controller (it defers while runs are active). */
  function afterChange(m) {
    refreshProblems(m);
    reconfigure(m);
  }

  function onControlChange(m, row, control) {
    setStatus(m, null);
    if (row.key === PICKER_KEY) {
      onPickerChange(m, row, control);
      return;
    }
    const res = coerce(row, readControl(control));
    const ZFT = m.ZFT;
    if (!res.ok) {
      writeControl(control, ZFT.config.getPref(row.key));
      if (res.error) setStatus(m, res.error);
      return;
    }
    try {
      ZFT.config.setPref(row.key, res.value);
    } catch (e) {
      log('error', `could not save ${row.key}: ${e}`);
      writeControl(control, ZFT.config.getPref(row.key));
      return;
    }
    writeControl(control, res.value);
    if (row.key === 'inst.enabled' || row.key.indexOf('inst.use_') === 0 || row.key.indexOf('institution.') === 0) syncPicker(m);
    afterChange(m);
  }

  // --- institution picker ---------------------------------------------------------------------------------------

  const PICKER_KEY = 'institution.preset';

  /** Show the "Other" fields only for Other, and the advanced details for everything but None. */
  function updateVisibility(m) {
    const picker = m.byKey[PICKER_KEY];
    const custom = document.getElementById(CUSTOM_ID);
    const advanced = document.getElementById(ADVANCED_ID);
    const value = picker ? picker.value : '';
    if (custom) custom.hidden = value !== m.ZFT.config.PRESET_OTHER;
    if (advanced) advanced.hidden = value === m.ZFT.config.PRESET_NONE;
    showMethodPanel(m);
    refreshDerived(m);
  }

  // --- access methods (the "Other" panels and the read-only summary) -------------------------------------------------

  /** Which method panel the "Other" section opens on: the first method that already has details, else OpenAthens. */
  function initialMethod(m) {
    const config = m.ZFT.config;
    try {
      const cfg = config.buildEngineConfig();
      if (String(cfg.institution.openathens_domain || '').trim()) return 'openathens';
      if (cfg.institution.saml_entity_id) return 'saml';
      if (cfg.institution.ezproxy) return 'ezproxy';
    } catch (e) { log('error', 'initialMethod failed: ' + e); }
    return 'openathens';
  }

  /** Show the panel of the selected method only. */
  function showMethodPanel(m) {
    for (const method of m.ZFT.config.METHODS) {
      const panel = document.getElementById(METHOD_PANEL_PREFIX + method);
      if (panel) panel.hidden = method !== m.method;
    }
    const select = document.getElementById(METHOD_SELECT_ID);
    if (select && select.value !== m.method) select.value = m.method;
  }

  /** "Uses: OpenAthens, Shibboleth / UK federation": the methods that are set up and switched on (read-only). */
  function updateMethodsSummary(m) {
    const node = document.getElementById(METHODS_ID);
    if (!node) return;
    const picker = m.byKey[PICKER_KEY];
    clear(node);
    if (!picker || picker.value === m.ZFT.config.PRESET_NONE) {
      node.hidden = true;
      return;
    }
    let active = [];
    try { active = m.ZFT.config.currentMethods(); } catch (e) { log('error', 'currentMethods failed: ' + e); }
    const first = el('span');
    if (active.length === 0) {
      setText(m.ZFT, first, 'zotero-fulltext-prefs-methods-none');
      node.appendChild(first);
    } else {
      setText(m.ZFT, first, 'zotero-fulltext-prefs-methods-uses');
      node.appendChild(first);
      active.forEach((method, i) => {
        const sep = el('span');
        sep.textContent = i === 0 ? ' ' : ', ';
        node.appendChild(sep);
        const name = el('span', { 'data-method': method });
        setText(m.ZFT, name, METHOD_NAME_IDS[method]);
        node.appendChild(name);
      });
    }
    node.hidden = false;
  }

  /** Who the stored identity provider is, for people: the name picked or typed, a preset's name, else the entityID itself. */
  function samlLabel(m) {
    const config = m.ZFT.config;
    const entityId = String(config.getPref('institution.saml_entity_id') || '').trim();
    if (!entityId) return '';
    if (m.samlPicked && m.samlPicked.entityID === entityId) return m.samlPicked.title;
    const preset = config.INSTITUTIONS.find((p) => p.values['institution.saml_entity_id'] === entityId);
    if (preset) return preset.values['institution.name'];
    return entityId;
  }

  function updateSamlChosen(m) {
    const node = document.getElementById(SAML_CHOSEN_ID);
    if (!node) return;
    const label = samlLabel(m);
    if (label) setText(m.ZFT, node, 'zotero-fulltext-prefs-saml-chosen', { name: label });
    else setText(m.ZFT, node, 'zotero-fulltext-prefs-saml-chosen-none');
  }

  /** The address field of the proxy panel shows the stored login host as https://host/login?url= (empty without one). */
  function updateProxyPrefix(m) {
    const input = document.getElementById(PROXY_PREFIX_ID);
    if (!input) return;
    const host = String(m.ZFT.config.getPref('institution.ezproxy_login_host') || '').trim();
    input.value = host ? m.ZFT.config.proxyPrefix(host) : '';
  }

  /** Everything that is computed from the prefs (and not a pref's own control): summary line, chosen IdP, proxy address. */
  function refreshDerived(m) {
    updateMethodsSummary(m);
    updateSamlChosen(m);
    updateProxyPrefix(m);
  }

  /**
   * After the institution prefs (or inst.enabled) changed by hand: show what they now amount to (a preset, Other, or None) and
   * keep the stored institution.preset in step. Editing a preset's details therefore turns the picker into Other.
   */
  function syncPicker(m) {
    const picker = m.byKey[PICKER_KEY];
    if (!picker) return;
    const config = m.ZFT.config;
    let id = config.PRESET_OTHER;
    try { id = config.effectivePreset(); } catch (e) { log('error', 'effectivePreset failed: ' + e); }
    try {
      if (config.getPref(PICKER_KEY) !== id) config.setPref(PICKER_KEY, id);
    } catch (e) {
      log('error', `could not save ${PICKER_KEY}: ${e}`);
    }
    writeControl(picker, id);
    updateVisibility(m);
  }

  /** The user picked an entry: apply it (all institution.* prefs at once), refresh every control, say what happened. */
  function onPickerChange(m, row, control) {
    const ZFT = m.ZFT;
    const res = coerce(row, readControl(control));
    if (!res.ok) {
      writeControl(control, ZFT.config.effectivePreset());
      return;
    }
    let applied;
    try {
      applied = ZFT.config.applyPreset(res.value);
    } catch (e) {
      log('error', `could not apply institution ${res.value}: ${e}`);
      syncControls(m);
      updateVisibility(m);
      return;
    }
    m.method = initialMethod(m);
    m.samlPicked = null;
    syncControls(m);
    updateVisibility(m);
    clearTestResult();
    setSamlStatus(m, null);
    if (applied.id === ZFT.config.PRESET_NONE) setStatus(m, 'zotero-fulltext-prefs-institution-none-applied');
    else if (applied.id === ZFT.config.PRESET_OTHER) setStatus(m, 'zotero-fulltext-prefs-institution-other-applied');
    else setStatus(m, 'zotero-fulltext-prefs-institution-applied', { name: applied.name });
    afterChange(m);
  }

  function clearResult(id) {
    const node = document.getElementById(id);
    if (node) node.hidden = true;
  }

  function clearTestResult() {
    clearResult(TEST_RESULT_ID);
    clearResult(PROXY_RESULT_ID);
  }

  function showResult(m, id, state, l10nId, args) {
    const node = document.getElementById(id);
    if (!node) return;
    node.setAttribute('data-state', state);
    setText(m.ZFT, node, l10nId, args);
    node.hidden = false;
  }

  function showTestResult(m, state, id, args) {
    showResult(m, TEST_RESULT_ID, state, id, args);
  }

  /**
   * Ask OpenAthens whether it knows the typed domain: ONE anonymous GET of the public redirector for a public article DOI,
   * redirects not followed (anon = no cookies sent or kept). No sign-in, no credentials, nothing stored.
   * @returns {Promise<'recognised'|'unknown'|'failed'|'invalid'>}
   */
  async function testInstitution(m) {
    const ZFT = m.ZFT;
    const control = m.byKey['institution.openathens_domain'];
    const parsed = ZFT.config.parseHostname(control ? control.value : '');
    if (!parsed.ok) {
      showTestResult(m, 'failed', 'zotero-fulltext-prefs-institution-test-needs-domain');
      return 'invalid';
    }
    const button = document.getElementById(TEST_BUTTON_ID);
    if (button) button.disabled = true;
    showTestResult(m, 'checking', 'zotero-fulltext-prefs-institution-test-checking');
    try {
      let verdict = 'failed';
      let reason = '';
      try {
        const xhr = await Zotero.HTTP.request('GET', ZFT.config.redirectorTestUrl(parsed.value), {
          anon: true, followRedirects: false, successCodes: false, errorDelayMax: 0, noRetryOnThrottle: true,
          timeout: TEST_TIMEOUT_MS, responseType: 'text',
        });
        const status = xhr && typeof xhr.status === 'number' ? xhr.status : 0;
        let text = '';
        try { text = status === 200 && typeof xhr.responseText === 'string' ? xhr.responseText : ''; } catch (e) { text = ''; }
        verdict = ZFT.config.classifyRedirectorResponse(status, text);
        if (verdict === 'failed') reason = status ? 'HTTP ' + status : 'no answer';
      } catch (e) {
        verdict = 'failed';
        reason = e && e.message ? String(e.message) : String(e);
      }
      // The pane may have been closed while the request was out.
      if (mounted !== m) return verdict;
      if (verdict === 'recognised') showTestResult(m, 'ok', 'zotero-fulltext-prefs-institution-test-ok', { domain: parsed.value });
      else if (verdict === 'unknown') showTestResult(m, 'unknown', 'zotero-fulltext-prefs-institution-test-unknown', { domain: parsed.value });
      else showTestResult(m, 'failed', 'zotero-fulltext-prefs-institution-test-failed', { reason });
      return verdict;
    } finally {
      if (button) button.disabled = false;
    }
  }

  // --- library proxy: pasted address and Test -----------------------------------------------------------------------

  /** The proxy address field changed: parse it into login host + suffix and store both; an empty field removes the proxy. */
  function onProxyPrefixChange(m, input) {
    setStatus(m, null);
    const ZFT = m.ZFT;
    const text = String(input.value || '').trim();
    clearResult(PROXY_RESULT_ID);
    try {
      if (text === '') {
        ZFT.config.setPref('institution.ezproxy_login_host', '');
        ZFT.config.setPref('institution.ezproxy_suffix', '');
      } else {
        const parsed = ZFT.config.parseProxyPrefix(text);
        if (!parsed.ok) {
          setStatus(m, parsed.error);
          updateProxyPrefix(m);
          return;
        }
        ZFT.config.setPref('institution.ezproxy_login_host', parsed.loginHost);
        ZFT.config.setPref('institution.ezproxy_suffix', parsed.suffix);
        ZFT.config.setPref('inst.use_ezproxy', true);
        setStatus(m, 'zotero-fulltext-prefs-proxy-saved', { host: parsed.loginHost });
      }
    } catch (e) {
      log('error', 'could not save the proxy address: ' + e);
      updateProxyPrefix(m);
      return;
    }
    syncControls(m);
    syncPicker(m);
    afterChange(m);
  }

  /**
   * Ask the library proxy whether it is an EZproxy: ONE anonymous GET of its login page for a public publisher home page,
   * redirects not followed, nothing submitted, no cookies sent or kept.
   * @returns {Promise<'recognised'|'unknown'|'failed'|'invalid'>}
   */
  async function testProxy(m) {
    const ZFT = m.ZFT;
    const input = document.getElementById(PROXY_PREFIX_ID);
    const parsed = ZFT.config.parseProxyPrefix(input ? input.value : '');
    if (!parsed.ok) {
      showResult(m, PROXY_RESULT_ID, 'failed', 'zotero-fulltext-prefs-proxy-test-needs-address');
      return 'invalid';
    }
    const button = document.getElementById(PROXY_TEST_ID);
    if (button) button.disabled = true;
    showResult(m, PROXY_RESULT_ID, 'checking', 'zotero-fulltext-prefs-proxy-test-checking');
    try {
      let verdict = 'failed';
      let reason = '';
      try {
        const xhr = await Zotero.HTTP.request('GET', ZFT.config.proxyTestUrl(parsed.loginHost), {
          anon: true, followRedirects: false, successCodes: false, errorDelayMax: 0, noRetryOnThrottle: true,
          timeout: TEST_TIMEOUT_MS, responseType: 'text',
        });
        const status = xhr && typeof xhr.status === 'number' ? xhr.status : 0;
        let text = '';
        let location = '';
        try { text = status === 200 && typeof xhr.responseText === 'string' ? xhr.responseText : ''; } catch (e) { text = ''; }
        try { location = status >= 300 && status < 400 && typeof xhr.getResponseHeader === 'function' ? String(xhr.getResponseHeader('Location') || '') : ''; } catch (e) { location = ''; }
        verdict = ZFT.config.classifyProxyResponse(status, text, location);
        if (verdict === 'failed') reason = status ? 'HTTP ' + status : 'no answer';
      } catch (e) {
        verdict = 'failed';
        reason = e && e.message ? String(e.message) : String(e);
      }
      if (mounted !== m) return verdict;
      if (verdict === 'recognised') showResult(m, PROXY_RESULT_ID, 'ok', 'zotero-fulltext-prefs-proxy-test-ok', { host: parsed.loginHost });
      else if (verdict === 'unknown') showResult(m, PROXY_RESULT_ID, 'unknown', 'zotero-fulltext-prefs-proxy-test-unknown', { host: parsed.loginHost });
      else showResult(m, PROXY_RESULT_ID, 'failed', 'zotero-fulltext-prefs-proxy-test-failed', { reason });
      return verdict;
    } finally {
      if (button) button.disabled = false;
    }
  }

  // --- Shibboleth: identity-provider search ----------------------------------------------------------------------------

  function setSamlStatus(m, id, args) {
    const node = document.getElementById(SAML_STATUS_ID);
    if (!node) return;
    if (id) {
      setText(m.ZFT, node, id, args);
      node.hidden = false;
    } else {
      node.hidden = true;
    }
  }

  function showSamlResults(m, entries) {
    const select = document.getElementById(SAML_RESULTS_ID);
    const useButton = document.getElementById(SAML_USE_ID);
    if (!select || !useButton) return;
    clear(select);
    m.samlResults = new Map();
    for (const entry of entries) {
      m.samlResults.set(entry.entityID, entry);
      const option = el('option', { value: entry.entityID });
      option.textContent = entry.domain ? `${entry.title} (${entry.domain})` : entry.title;
      select.appendChild(option);
    }
    select.value = entries.length ? entries[0].entityID : '';
    select.hidden = entries.length === 0;
    useButton.hidden = entries.length === 0;
  }

  /**
   * Search the public SeamlessAccess metadata directory by name: ONE anonymous GET, the typed text is the only thing sent.
   * @returns {Promise<'found'|'none'|'short'|'failed'>}
   */
  async function searchSaml(m) {
    const ZFT = m.ZFT;
    const input = document.getElementById(SAML_QUERY_ID);
    const query = input ? input.value : '';
    if (!ZFT.config.samlQueryOk(query)) {
      showSamlResults(m, []);
      setSamlStatus(m, 'zotero-fulltext-prefs-saml-query-short');
      return 'short';
    }
    const button = document.getElementById(SAML_SEARCH_ID);
    if (button) button.disabled = true;
    setSamlStatus(m, 'zotero-fulltext-prefs-saml-searching');
    try {
      let entries = [];
      let reason = '';
      try {
        const xhr = await Zotero.HTTP.request('GET', ZFT.config.samlSearchUrl(query), {
          anon: true, headers: { Accept: 'application/json' }, successCodes: false, errorDelayMax: 0, noRetryOnThrottle: true,
          timeout: TEST_TIMEOUT_MS, responseType: 'text',
        });
        const status = xhr && typeof xhr.status === 'number' ? xhr.status : 0;
        if (status === 200) entries = ZFT.config.parseSamlSearch(typeof xhr.responseText === 'string' ? xhr.responseText : '');
        else reason = status ? 'HTTP ' + status : 'no answer';
      } catch (e) {
        reason = e && e.message ? String(e.message) : String(e);
      }
      if (mounted !== m) return reason ? 'failed' : entries.length ? 'found' : 'none';
      if (reason) {
        showSamlResults(m, []);
        setSamlStatus(m, 'zotero-fulltext-prefs-saml-failed', { reason });
        return 'failed';
      }
      showSamlResults(m, entries);
      if (entries.length === 0) {
        setSamlStatus(m, 'zotero-fulltext-prefs-saml-no-results');
        return 'none';
      }
      setSamlStatus(m, 'zotero-fulltext-prefs-saml-found', { count: entries.length });
      return 'found';
    } finally {
      if (button) button.disabled = false;
    }
  }

  /** "Use this identity provider": store the picked entityID (and the name, when none is set), switch Shibboleth on. */
  async function pickSaml(m) {
    const ZFT = m.ZFT;
    const select = document.getElementById(SAML_RESULTS_ID);
    const entry = select && m.samlResults ? m.samlResults.get(select.value) : null;
    if (!entry) return null;
    setStatus(m, null);
    try {
      ZFT.config.setPref('institution.saml_entity_id', entry.entityID);
      ZFT.config.setPref('inst.use_saml', true);
      ZFT.config.setPref('institution.idp_hosts', ZFT.config.withIdpHost(ZFT.config.getPref('institution.idp_hosts'), entry.entityID));
      if (!String(ZFT.config.getPref('institution.name') || '').trim()) ZFT.config.setPref('institution.name', entry.title);
    } catch (e) {
      log('error', 'could not save the identity provider: ' + e);
      return null;
    }
    m.samlPicked = entry;
    syncControls(m);
    syncPicker(m);
    setStatus(m, 'zotero-fulltext-prefs-saml-picked', { name: entry.title });
    afterChange(m);
    return entry;
  }

  function renderRow(m, row) {
    const ZFT = m.ZFT;
    const d = buildRow(row, currentValue(m, row));
    const wrap = el('div', { class: 'zft-row', 'data-key': d.key });
    const label = el('label', { for: d.id });
    setText(ZFT, label, d.labelId);

    const control = el(d.tag, { id: d.id });
    if (d.tag === 'select') {
      for (const o of d.options) {
        const option = el('option', { value: o.value });
        setText(ZFT, option, o.labelId);
        control.appendChild(option);
      }
    } else {
      control.setAttribute('type', d.inputType);
      if (d.min !== undefined) control.setAttribute('min', String(d.min));
      if (d.max !== undefined) control.setAttribute('max', String(d.max));
      if (d.inputType === 'number') control.setAttribute('step', '1');
      if (d.inputType === 'text' || d.inputType === 'password') control.setAttribute('autocomplete', 'off');
      if (d.secret) control.setAttribute('spellcheck', 'false');
    }
    writeControl(control, d.value);
    listen(m, control, 'change', () => onControlChange(m, row, control));
    m.controls.push({ row, control });
    m.byKey[row.key] = control;

    if (d.widget === 'checkbox') {
      wrap.appendChild(control);
      wrap.appendChild(label);
    } else {
      wrap.appendChild(label);
      wrap.appendChild(control);
    }
    if (d.secret) wrap.appendChild(renderSecretToggle(m, control));
    return wrap;
  }

  /** Show/Hide button for a password-style field. */
  function renderSecretToggle(m, control) {
    const button = el('button', { type: 'button', class: 'zft-secret-toggle' });
    setText(m.ZFT, button, 'zotero-fulltext-prefs-show-secret');
    listen(m, button, 'click', () => {
      const showing = control.type === 'text';
      control.type = showing ? 'password' : 'text';
      setText(m.ZFT, button, showing ? 'zotero-fulltext-prefs-show-secret' : 'zotero-fulltext-prefs-hide-secret');
    });
    return button;
  }

  function renderGroup(m, group) {
    const section = el('div', { class: 'zft-group', id: 'zft-group-' + group.id });
    const heading = el('h2');
    setText(m.ZFT, heading, group.heading);
    section.appendChild(heading);
    if (group.rows.some((r) => r.section)) renderSections(m, section, group.rows);
    else for (const row of group.rows) section.appendChild(renderRow(m, row));
    return section;
  }

  function testRow(m, buttonId, resultId, labelId, onClick) {
    const row = el('div', { class: 'zft-row zft-test-row' });
    const button = el('button', { id: buttonId, type: 'button' });
    setText(m.ZFT, button, labelId);
    listen(m, button, 'click', onClick);
    row.appendChild(button);
    const result = el('span', { id: resultId, role: 'status' });
    result.hidden = true;
    row.appendChild(result);
    return row;
  }

  function panel(method) {
    const node = el('div', { id: METHOD_PANEL_PREFIX + method, class: 'zft-method-panel', 'data-method': method });
    node.hidden = true;
    return node;
  }

  function hintParagraph(m, id) {
    const hint = el('p', { class: 'zft-hint' });
    setText(m.ZFT, hint, id);
    return hint;
  }

  /**
   * The institution group: the picker row, the read-only "Uses: ..." line, then the "Other" fields in #zft-institution-custom
   * (hint, name, a method selector and one panel per method: OpenAthens domain + Test, Shibboleth search + chosen identity
   * provider, library proxy address + Test), then the remaining rows inside a collapsed <details>. updateVisibility() shows or
   * hides them.
   */
  function renderSections(m, section, rows) {
    const bySection = (name) => rows.filter((r) => r.section === name);
    for (const row of bySection('picker')) section.appendChild(renderRow(m, row));
    const summary = el('p', { id: METHODS_ID, class: 'zft-methods', role: 'status' });
    section.appendChild(summary);

    const custom = el('div', { id: CUSTOM_ID, class: 'zft-custom' });
    custom.appendChild(hintParagraph(m, 'zotero-fulltext-prefs-other-hint'));
    for (const row of bySection('custom')) custom.appendChild(renderRow(m, row));

    // Method selector (UI state only: which panel is open).
    const selectRow = el('div', { class: 'zft-row' });
    const selectLabel = el('label', { for: METHOD_SELECT_ID });
    setText(m.ZFT, selectLabel, 'zotero-fulltext-prefs-method-select');
    const select = el('select', { id: METHOD_SELECT_ID });
    for (const method of m.ZFT.config.METHODS) {
      const option = el('option', { value: method });
      setText(m.ZFT, option, METHOD_NAME_IDS[method]);
      select.appendChild(option);
    }
    m.method = initialMethod(m);
    select.value = m.method;
    listen(m, select, 'change', () => {
      if (m.ZFT.config.METHODS.includes(select.value)) m.method = select.value;
      showMethodPanel(m);
    });
    selectRow.appendChild(selectLabel);
    selectRow.appendChild(select);
    custom.appendChild(selectRow);

    // OpenAthens: domain + Test.
    const oa = panel('openathens');
    for (const row of bySection('openathens')) oa.appendChild(renderRow(m, row));
    oa.appendChild(testRow(m, TEST_BUTTON_ID, TEST_RESULT_ID, 'zotero-fulltext-prefs-institution-test', () => action(m, 'institution test', testInstitution)));
    custom.appendChild(oa);

    // Shibboleth: name search in the federation metadata, results list, chosen identity provider, entityID field.
    const saml = panel('saml');
    saml.appendChild(hintParagraph(m, 'zotero-fulltext-prefs-saml-hint'));
    const queryRow = el('div', { class: 'zft-row' });
    const queryLabel = el('label', { for: SAML_QUERY_ID });
    setText(m.ZFT, queryLabel, 'zotero-fulltext-prefs-saml-query');
    const query = el('input', { id: SAML_QUERY_ID, type: 'text', autocomplete: 'off' });
    const searchButton = el('button', { id: SAML_SEARCH_ID, type: 'button' });
    setText(m.ZFT, searchButton, 'zotero-fulltext-prefs-saml-search');
    listen(m, searchButton, 'click', () => action(m, 'identity provider search', searchSaml));
    listen(m, query, 'keydown', (ev) => {
      if (ev && ev.key === 'Enter') action(m, 'identity provider search', searchSaml);
    });
    queryRow.appendChild(queryLabel);
    queryRow.appendChild(query);
    queryRow.appendChild(searchButton);
    saml.appendChild(queryRow);
    const status = el('p', { id: SAML_STATUS_ID, class: 'zft-hint', role: 'status' });
    status.hidden = true;
    saml.appendChild(status);
    const resultsRow = el('div', { class: 'zft-row' });
    const results = el('select', { id: SAML_RESULTS_ID, size: '6', 'aria-label': 'identity providers' });
    results.hidden = true;
    const useButton = el('button', { id: SAML_USE_ID, type: 'button' });
    setText(m.ZFT, useButton, 'zotero-fulltext-prefs-saml-use');
    useButton.hidden = true;
    listen(m, useButton, 'click', () => action(m, 'identity provider pick', pickSaml));
    resultsRow.appendChild(results);
    resultsRow.appendChild(useButton);
    saml.appendChild(resultsRow);
    const chosen = el('p', { id: SAML_CHOSEN_ID, class: 'zft-chosen' });
    saml.appendChild(chosen);
    for (const row of bySection('saml')) saml.appendChild(renderRow(m, row));
    custom.appendChild(saml);

    // Library proxy: pasted address (parsed into login host + suffix), the two host fields, "every publisher", Test.
    const proxy = panel('ezproxy');
    proxy.appendChild(hintParagraph(m, 'zotero-fulltext-prefs-proxy-hint'));
    const prefixRow = el('div', { class: 'zft-row' });
    const prefixLabel = el('label', { for: PROXY_PREFIX_ID });
    setText(m.ZFT, prefixLabel, 'zotero-fulltext-prefs-proxy-prefix');
    const prefix = el('input', { id: PROXY_PREFIX_ID, type: 'text', autocomplete: 'off', spellcheck: 'false' });
    listen(m, prefix, 'change', () => onProxyPrefixChange(m, prefix));
    prefixRow.appendChild(prefixLabel);
    prefixRow.appendChild(prefix);
    proxy.appendChild(prefixRow);
    for (const row of bySection('proxy')) proxy.appendChild(renderRow(m, row));
    proxy.appendChild(testRow(m, PROXY_TEST_ID, PROXY_RESULT_ID, 'zotero-fulltext-prefs-proxy-test', () => action(m, 'proxy test', testProxy)));
    custom.appendChild(proxy);
    section.appendChild(custom);

    const advanced = el('details', { id: ADVANCED_ID, class: 'zft-advanced' });
    const summaryTag = el('summary');
    setText(m.ZFT, summaryTag, 'zotero-fulltext-prefs-institution-advanced');
    advanced.appendChild(summaryTag);
    for (const row of bySection('advanced')) advanced.appendChild(renderRow(m, row));
    section.appendChild(advanced);
  }

  /** What a control should show: its pref, except the picker, which shows what the institution prefs amount to. */
  function currentValue(m, row) {
    if (row.key === PICKER_KEY) {
      try { return m.ZFT.config.effectivePreset(); } catch (e) { log('error', 'effectivePreset failed: ' + e); }
    }
    return m.ZFT.config.getPref(row.key);
  }

  /** Re-read every control from its pref (after the institution defaults were restored). */
  function syncControls(m) {
    for (const { row, control } of m.controls) writeControl(control, currentValue(m, row));
  }

  /** Restore the King's College London defaults: every institution.* pref (the picker choice included) and inst.enabled. */
  function resetInstitution(m) {
    m.ZFT.config.applyPreset(m.ZFT.config.PRESET_DEFAULT);
    syncControls(m);
    updateVisibility(m);
    clearTestResult();
    setStatus(m, 'zotero-fulltext-prefs-institution-restored');
    afterChange(m);
  }

  async function clearState(m) {
    const ZFT = m.ZFT;
    const confirmed = Services.prompt.confirm(window, ZFT.l10n.format('zotero-fulltext-prefs-title'), ZFT.l10n.format('zotero-fulltext-prefs-clear-state-confirm'));
    if (!confirmed) return;
    await ZFT.state.reset();
    setStatus(m, 'zotero-fulltext-prefs-state-cleared');
  }

  async function openDataFolder(m) {
    const ZFT = m.ZFT;
    await ZFT.state.init();
    await Zotero.File.reveal(ZFT.state.dataDir());
  }

  /** Run an async button action; a failure is logged, never thrown into the event loop. */
  function action(m, name, fn) {
    return Promise.resolve().then(() => fn(m)).catch((e) => { log('error', `${name} failed: ${e}`); });
  }

  function bindButton(m, id, name, fn) {
    const button = document.getElementById(id);
    if (button) listen(m, button, 'click', () => action(m, name, fn));
  }

  /** Mount the pane. Idempotent. */
  function init() {
    const root = document.getElementById(ROOT_ID);
    if (!root) return;
    if (mounted && mounted.root === root) return;
    if (mounted) destroy();

    const ZFT = getZFT();
    if (!ZFT || !ZFT.config) {
      showUnavailable();
      return;
    }
    const m = { root, ZFT, listeners: [], controls: [], byKey: {}, addedL10n: false };
    mounted = m;
    try {
      // The fragment carries <link rel="localization">; add the resource ourselves only if Zotero did not pick it up.
      if (document.l10n && typeof document.l10n.addResourceIds === 'function'
        && !document.querySelector('link[rel="localization"][href="' + ZFT.FTL + '"]')) {
        document.l10n.addResourceIds([ZFT.FTL]);
        m.addedL10n = true;
      }
      clear(root);
      for (const group of ZFT.config.PANE_LAYOUT) root.appendChild(renderGroup(m, group));
      bindButton(m, 'zft-prefs-reset-institution', 'reset institution', resetInstitution);
      bindButton(m, 'zft-prefs-clear-state', 'clear state', clearState);
      bindButton(m, 'zft-prefs-open-data', 'open data folder', openDataFolder);
      listen(m, window, 'unload', destroy);
      updateVisibility(m);
      refreshProblems(m);
      if (document.l10n && typeof document.l10n.translateFragment === 'function') {
        Promise.resolve(document.l10n.translateFragment(root)).catch((e) => { log('warn', 'translateFragment failed: ' + e); });
      }
    } catch (e) {
      log('error', 'init failed: ' + e);
      destroy();
    }
  }

  /** Unmount: remove every listener and the built DOM. Safe to call twice. */
  function destroy() {
    const m = mounted;
    if (!m) return;
    mounted = null;
    for (const { target, type, fn } of m.listeners) {
      try { target.removeEventListener(type, fn); } catch (e) { /* the element is gone already */ }
    }
    m.listeners.length = 0;
    m.controls.length = 0;
    m.byKey = {};
    try { clear(m.root); } catch (e) { /* the pane is being torn down */ }
    if (m.addedL10n) {
      try { document.l10n.removeResourceIds([m.ZFT.FTL]); } catch (e) { /* the window is closing */ }
    }
  }

  return { init, destroy, buildRow, coerce, controlId, normalizeList };
})();

// The inline onload in prefs.xhtml is evaluated in the window, not in this sandbox.
window.ZoteroFullTextPrefs = ZoteroFullTextPrefs;
