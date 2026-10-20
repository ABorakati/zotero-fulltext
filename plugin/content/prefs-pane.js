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
 *   4. Buttons: reset-institution (restores every institution.* default), clear-state (ZFT.state.reset(); confirm first),
 *      open-data (Zotero.File.reveal(ZFT.state.dataDir())).
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

  /** The live mount: {root, ZFT, listeners, controls, addedL10n} or null. */
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
      default:
        return { ok: true, value: String(raw === undefined || raw === null ? '' : raw).trim() };
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

  function setStatus(m, id) {
    const node = document.getElementById(STATUS_ID);
    if (!node) return;
    if (id) {
      setText(m.ZFT, node, id);
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
    const res = coerce(row, readControl(control));
    const ZFT = m.ZFT;
    if (!res.ok) {
      writeControl(control, ZFT.config.getPref(row.key));
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
    afterChange(m);
  }

  function renderRow(m, row) {
    const ZFT = m.ZFT;
    const d = buildRow(row, ZFT.config.getPref(row.key));
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
    for (const row of group.rows) section.appendChild(renderRow(m, row));
    return section;
  }

  /** Re-read every control from its pref (after the institution defaults were restored). */
  function syncControls(m) {
    for (const { row, control } of m.controls) writeControl(control, m.ZFT.config.getPref(row.key));
  }

  function resetInstitution(m) {
    const config = m.ZFT.config;
    for (const key of Object.keys(config.DEFAULTS)) {
      if (key.indexOf('institution.') === 0) config.setPref(key, config.DEFAULTS[key]);
    }
    syncControls(m);
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
    const m = { root, ZFT, listeners: [], controls: [], addedL10n: false };
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
    try { clear(m.root); } catch (e) { /* the pane is being torn down */ }
    if (m.addedL10n) {
      try { document.l10n.removeResourceIds([m.ZFT.FTL]); } catch (e) { /* the window is closing */ }
    }
  }

  return { init, destroy, buildRow, coerce, controlId, normalizeList };
})();

// The inline onload in prefs.xhtml is evaluated in the window, not in this sandbox.
window.ZoteroFullTextPrefs = ZoteroFullTextPrefs;
