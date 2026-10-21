/* ZFT.progress: batch progress UI (controller + DOM-free model).
 *
 * Owner: js-ui. Zotero.ProgressWindow cannot show per-item rows with buttons (it only builds text lines,
 * progressWindow.js:208-235), so this is our own window (progress.xhtml + progress-window.js) fed by Effect::Progress
 * rows. The model is plain data + subscribe(); the window script is thin DOM glue; both are testable without a DOM.
 *
 *  - One window per run; closing it does NOT cancel the run (Tools > Show Full Text progress reopens it; the
 *    window's Cancel button sends CancelRun).
 *  - Row text = Fluent: row.l10n + row.args rendered with document.l10n.setAttributes() in the window; outside
 *    a window (diagnostics) with ZFT.l10n.format(). The ids the engine emits (crates/fulltext-engine/src/l10n.rs) are
 *    listed in docs/design/plugin-js.md section 8 and exist in locale/en-US/zotero-fulltext.ftl (ftl.test.mjs enforces it).
 *    The final report has NO per-outcome text of its own: each item shows its LAST row (the terminal row the engine emitted
 *    for the outcome) and is grouped by report.items[].outcome.kind. The only exception is an item whose last row is
 *    still not terminal when the report arrives (engine failure: effects.js builds a synthetic report); such a row is
 *    replaced by fallbackRow(outcome) so the report never shows "Looking for an open-access copy" under "Needs you".
 *  - RowAction handling (handleAction): open_browser {url} -> Zotero.launchURL(url) (system browser, user-initiated,
 *    http/https only); attach_file -> ZFT.save.attachFile(itemID, progressWindow); sign_in -> ZFT.runs.userAction(run,
 *    'sign_in_now'); retry -> ZFT.runs.userAction(run, 'retry_sign_in'); cancel_item -> ZFT.runs.cancelItem(run, item).
 *  - Final report (RunFinished): grouped sections Attached / Needs you / Not found / Failed / Skipped, each item
 *    with its last row and hand-off actions (Handoff.redirector_url / ezproxy_url / libkey_url / doi_url become
 *    open_browser actions), a ledger usage line, and a "Copy diagnostics" button: a redacted text block
 *    (URLs reduced to host+path, no query strings, no cookies, no titles, no API keys) from model.diagnostics().
 *
 * Window hand-over: the window receives ONE argument object (args) and reads it through
 * window.arguments[0].wrappedJSObject. args.wrappedJSObject === args (the idiom validated by the sso-spike), so it
 * works whether XPConnect hands the window the original object or a wrapper of it. args = {model, onAction(item, action),
 * onCancel(), onClose(), onCopy(), format(id, args)}.
 *
 * Model listeners are called as fn(model, item): item is the changed item id, or null for "report stored". The window
 * uses it to patch only the changed rows.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const WINDOW_URL = 'chrome://zotero-fulltext/content/progress.xhtml';
  const WINDOW_TYPE = 'zotero-fulltext:progress';

  /** Row states in display order (proto.rs RowState). */
  const ROW_STATES = ['queued', 'working', 'waiting', 'done', 'attention', 'failed', 'skipped', 'cancelled'];
  /** States an item does not leave (except by a retry transition, see applyRow). */
  const TERMINAL_STATES = new Set(['done', 'attention', 'failed', 'skipped', 'cancelled']);
  /** A row in one of these states after a terminal one is a "retry": the item is being worked on again. */
  const RETRY_STATES = new Set(['working', 'waiting']);

  /** Report groups in display order. */
  const GROUP_ORDER = ['attached', 'needs_you', 'not_found', 'failed', 'skipped'];
  /** model.rs Outcome.kind -> report group. */
  const GROUP_OF_KIND = {
    saved: 'attached',
    needs_user: 'needs_you',
    not_entitled: 'needs_you',
    blocked: 'needs_you',
    halted: 'needs_you',
    deferred: 'needs_you',
    not_found: 'not_found',
    no_identifier: 'not_found',
    failed: 'failed',
    skipped: 'skipped',
    cancelled: 'skipped',
  };
  /** Heading message per group. */
  const GROUP_L10N = {
    attached: 'zotero-fulltext-report-group-attached',
    needs_you: 'zotero-fulltext-report-group-needs-you',
    not_found: 'zotero-fulltext-report-group-not-found',
    failed: 'zotero-fulltext-report-group-failed',
    skipped: 'zotero-fulltext-report-group-skipped',
  };
  /** model.rs Handoff field -> button label message, in display order. */
  const HANDOFF_FIELDS = [
    ['redirector_url', 'zotero-fulltext-action-open-redirector'],
    ['wayfless_url', 'zotero-fulltext-action-open-wayfless'],
    ['ezproxy_url', 'zotero-fulltext-action-open-ezproxy'],
    ['libkey_url', 'zotero-fulltext-action-open-libkey'],
    ['doi_url', 'zotero-fulltext-action-open-doi'],
  ];
  /** proto.rs RowAction.kind -> default button label message. */
  const ACTION_L10N = {
    open_browser: 'zotero-fulltext-action-open-browser',
    attach_file: 'zotero-fulltext-action-attach-file',
    sign_in: 'zotero-fulltext-action-sign-in',
    retry: 'zotero-fulltext-action-retry',
    cancel_item: 'zotero-fulltext-action-cancel-item',
  };

  const QUEUED_L10N = 'zotero-fulltext-status-queued';
  const MAX_URL_TEXT = 160;

  // ------------------------------------------------------------------------------------------ helpers

  /** @returns {boolean} true for an absolute http(s) URL */
  function isHttpUrl(url) {
    if (typeof url !== 'string' || url.length === 0 || url.length > 8192) return false;
    try {
      const u = new URL(url);
      return u.protocol === 'http:' || u.protocol === 'https:';
    } catch (e) {
      return false;
    }
  }

  /** Reduce a URL to "host/path" for diagnostics (no scheme, userinfo, query or hash). Garbage -> "(invalid url)". */
  function redactUrl(url) {
    try {
      const u = new URL(String(url));
      if (!u.host) return '(invalid url)';
      return (u.host + u.pathname).slice(0, MAX_URL_TEXT);
    } catch (e) {
      return '(invalid url)';
    }
  }

  /** Single-line, pipe-free text for diagnostics fields. */
  function clean(value) {
    return String(value == null ? '' : value).replace(/[\r\n|]+/g, ' ').trim();
  }

  function plainArgs(args) {
    const out = {};
    if (args && typeof args === 'object') {
      for (const k of Object.keys(args)) out[k] = args[k];
    }
    return out;
  }

  /** Terminal row for an Outcome, used only when the engine's own terminal row never arrived (see header). */
  function fallbackRow(outcome) {
    const o = outcome || {};
    const row = (state, l10n, args) => ({ state, l10n, args: args || {}, route: null, actions: [] });
    switch (o.kind) {
      case 'saved': return row('done', o.route === 'inst' ? 'zotero-fulltext-status-saved-inst' : 'zotero-fulltext-status-saved-oa');
      case 'skipped': {
        const ids = {
          has_pdf: 'zotero-fulltext-status-skipped-has-pdf',
          pdf_added_meanwhile: 'zotero-fulltext-status-skipped-has-pdf',
          pdf_not_downloaded: 'zotero-fulltext-status-skipped-not-downloaded',
          read_only: 'zotero-fulltext-status-not-editable',
          not_regular: 'zotero-fulltext-status-skipped-not-regular',
          feed: 'zotero-fulltext-status-skipped-not-regular',
          in_progress_elsewhere: 'zotero-fulltext-status-skipped-in-progress',
        };
        return row('skipped', ids[o.reason] || 'zotero-fulltext-status-skipped-not-regular');
      }
      case 'no_identifier': return row('attention', 'zotero-fulltext-status-no-identifier');
      case 'not_found': return row('attention', 'zotero-fulltext-status-not-found');
      case 'not_entitled': return row('attention', 'zotero-fulltext-status-not-entitled');
      case 'needs_user':
        return row('attention', o.why === 'engine_restarted' ? 'zotero-fulltext-status-engine-restarted' : 'zotero-fulltext-status-needs-user');
      case 'blocked': return row('attention', 'zotero-fulltext-status-blocked', { host: String(o.host || '') });
      case 'deferred': return row('attention', 'zotero-fulltext-status-cap-reached', { cap: String(o.cap || 'batch') });
      case 'halted': return row('attention', 'zotero-fulltext-status-halted');
      case 'cancelled': return row('cancelled', 'zotero-fulltext-status-cancelled');
      default: return row('failed', 'zotero-fulltext-status-failed', { code: String(o.code || 'unknown') });
    }
  }

  /** Outcome hand-off URLs -> RowAction::OpenBrowser entries (http/https only). */
  function handoffActionsOf(outcome) {
    const handoff = outcome && outcome.handoff;
    const out = [];
    if (!handoff || typeof handoff !== 'object') return out;
    for (const [field, l10n] of HANDOFF_FIELDS) {
      const url = handoff[field];
      if (isHttpUrl(url)) out.push({ kind: 'open_browser', url, l10n });
    }
    return out;
  }

  // ------------------------------------------------------------------------------------------ model

  /**
   * Per-run, DOM-free view model.
   * rows(): [{item, key, title, state, l10n, args, route, actions}] in selection order.
   */
  class ProgressModel {
    /** @param {number} run @param {Array<{item: number, key: string, title: string}>} items */
    constructor(run, items) {
      this.run = run;
      this.items = [];
      this.report = null;
      this._listeners = new Set();
      this._rows = new Map();
      this._info = new Map();
      this._order = [];
      this._groups = null;
      this._signIn = null;
      this._counts = {};
      for (const s of ROW_STATES) this._counts[s] = 0;
      for (const it of items || []) {
        if (!it || this._rows.has(it.item)) continue; // a duplicate id would make two rows share one status
        const info = { item: it.item, key: String(it.key == null ? '' : it.key), title: String(it.title == null ? '' : it.title) };
        this.items.push(info);
        this._info.set(info.item, info);
        this._order.push(info.item);
        this._rows.set(info.item, {
          item: info.item, key: info.key, title: info.title,
          state: 'queued', l10n: QUEUED_L10N, args: {}, route: null, actions: [],
        });
        this._counts.queued += 1;
      }
    }

    /**
     * Apply an Effect::Progress row. Unknown item ids are logged and ignored. After a terminal state only a retry
     * transition (back to working/waiting) is accepted. @returns {boolean} true when the row changed the model
     */
    applyRow(item, row) {
      const cur = this._rows.get(item);
      if (!cur) {
        ZFT.util.log('warn', `progress: row for unknown item ${item} in run ${this.run} ignored`);
        return false;
      }
      if (!row || typeof row !== 'object' || !ROW_STATES.includes(row.state)) {
        ZFT.util.log('warn', `progress: malformed row for item ${item} ignored`);
        return false;
      }
      if (TERMINAL_STATES.has(cur.state) && !RETRY_STATES.has(row.state)) return false;
      this._assign(cur, row);
      this._groups = null;
      this._notify(item);
      return true;
    }

    _assign(cur, row) {
      this._counts[cur.state] -= 1;
      cur.state = row.state;
      this._counts[cur.state] += 1;
      cur.l10n = String(row.l10n || '');
      cur.args = plainArgs(row.args);
      cur.route = row.route || null;
      cur.actions = Array.isArray(row.actions) ? row.actions.filter((a) => a && typeof a.kind === 'string') : [];
    }

    /** Store the RunFinished report (rows keep their last state; report drives the grouped view). */
    setReport(report) {
      if (!report || typeof report !== 'object') {
        ZFT.util.log('warn', 'progress: setReport without a report ignored');
        return;
      }
      this.report = report;
      for (const ir of Array.isArray(report.items) ? report.items : []) {
        const cur = this._rows.get(ir && ir.item);
        if (cur && !TERMINAL_STATES.has(cur.state)) this._assign(cur, fallbackRow(ir.outcome));
      }
      this._groups = null;
      this._notify(null);
    }

    /**
     * A sign-in window is open and the run waits for it: the progress window shows a "Sign-in needed" banner with a button that
     * brings that window to the front. @param {({show: function, waiting?: number}|null)} info null clears the banner
     */
    setSignIn(info) {
      const next = info && typeof info.show === 'function' ? { show: info.show, waiting: Number(info.waiting) || 0 } : null;
      if (!next && !this._signIn) return;
      this._signIn = next;
      this._notify(null);
    }

    /** @returns {{show: function, waiting: number}|null} the open sign-in window's banner data */
    signIn() {
      return this._signIn || null;
    }

    /** @returns {Array<object>} rows in selection order */
    rows() {
      return this._order.map((id) => this._rows.get(id));
    }

    /** @returns {object|null} the live row of one item */
    rowFor(item) {
      return this._rows.get(item) || null;
    }

    /** @returns {Object<string, number>} count per RowState plus total and finished (items in a terminal state) */
    counts() {
      const c = Object.assign({}, this._counts);
      c.total = this._order.length;
      c.finished = c.done + c.attention + c.failed + c.skipped + c.cancelled;
      return c;
    }

    /** @returns {{attached: object[], needs_you: object[], not_found: object[], failed: object[], skipped: object[]}|null} null until a report exists */
    groups() {
      if (!this.report) return null;
      if (this._groups) return this._groups;
      const out = {};
      for (const g of GROUP_ORDER) out[g] = [];
      for (const ir of Array.isArray(this.report.items) ? this.report.items : []) {
        if (!ir || typeof ir !== 'object') continue;
        const outcome = ir.outcome || {};
        const group = GROUP_OF_KIND[outcome.kind] || 'failed';
        const live = this._rows.get(ir.item) || null;
        const info = this._info.get(ir.item) || null;
        const row = live || Object.assign({ item: ir.item, key: String(ir.key || ''), title: '' }, fallbackRow(outcome));
        const handoffActions = handoffActionsOf(outcome);
        // Row actions already carry the hand-off links the engine put there; show each URL once.
        const seen = new Set(row.actions.filter((a) => a.kind === 'open_browser').map((a) => a.url));
        const actions = row.actions.concat(handoffActions.filter((a) => !seen.has(a.url)));
        out[group].push({
          item: ir.item,
          key: String(ir.key || (info && info.key) || ''),
          title: info ? info.title : '',
          outcome,
          row,
          handoffActions,
          actions,
          group,
        });
      }
      this._groups = out;
      return out;
    }

    /** @returns {string} the message id to label an action button with */
    actionL10n(action) {
      if (action && action.kind === 'open_browser' && typeof action.l10n === 'string' && action.l10n) return action.l10n;
      return (action && ACTION_L10N[action.kind]) || ACTION_L10N.open_browser;
    }

    /** @returns {string} redacted multi-line diagnostics (see header) */
    diagnostics() {
      const lines = [`zotero-fulltext diagnostics, run ${this.run}`];
      const report = this.report;
      const counts = this.counts();
      lines.push(`items ${counts.total}, finished ${counts.finished}, working ${counts.working}, waiting ${counts.waiting}, queued ${counts.queued}`);
      if (report) {
        const c = report.counts || {};
        lines.push('counts ' + Object.keys(c).map((k) => `${clean(k)}=${clean(c[k])}`).join(' '));
        if (report.halted) lines.push(`halted ${clean(report.halted)}`);
        if (report.engine_restarts) lines.push(`engine_restarts ${clean(report.engine_restarts)}`);
        const reportItems = Array.isArray(report.items) ? report.items : [];
        for (const ir of reportItems) {
          if (!ir || typeof ir !== 'object') continue;
          const row = this._rows.get(ir.item);
          const outcome = ir.outcome || {};
          const trace = (Array.isArray(ir.trace) ? ir.trace : [])
            .map((t) => `${clean(t.src)} ${clean(t.tier)} ${clean(t.class)} ${clean(t.ms)}ms`).join('; ');
          lines.push([clean(ir.key), clean(outcome.kind) + describeOutcome(outcome), row ? clean(row.l10n) : '-', trace || '-'].join(' | '));
        }
        const l = report.ledger || {};
        lines.push(`ledger hour=${clean(l.hour)}/${clean(l.hour_cap)} day=${clean(l.day)}/${clean(l.day_cap)} mb_hour=${clean(l.mb_hour)}/${clean(l.mb_hour_cap)}`);
      } else {
        for (const row of this.rows()) lines.push([clean(row.key), `(${row.state})`, clean(row.l10n), '-'].join(' | '));
      }
      return lines.join('\n');
    }

    /** @param {(model: ProgressModel, item: (number|null)) => void} fn @returns {() => void} unsubscribe */
    subscribe(fn) {
      this._listeners.add(fn);
      return () => { this._listeners.delete(fn); };
    }

    _notify(item) {
      for (const fn of Array.from(this._listeners)) {
        try {
          fn(this, item);
        } catch (e) {
          ZFT.util.log('error', 'progress listener failed: ' + ZFT.util.errText(e));
        }
      }
    }
  }

  /** Non-sensitive outcome details for the diagnostics line (URLs reduced to host/path). */
  function describeOutcome(o) {
    const parts = [];
    const add = (k, v) => { if (v !== undefined && v !== null && v !== '') parts.push(`${k}=${clean(v)}`); };
    switch (o.kind) {
      case 'saved': add('route', o.route); add('source', o.source); add('version', o.version); if (o.url) add('url', redactUrl(o.url)); break;
      case 'skipped': add('reason', o.reason); break;
      case 'not_found': add('reason', o.reason); break;
      case 'needs_user': add('why', o.why); break;
      case 'blocked': add('host', o.host); break;
      case 'deferred': add('cap', o.cap); break;
      case 'cancelled': add('by', o.by); break;
      case 'failed': add('code', o.code); break;
      default: break;
    }
    const h = o.handoff;
    if (h && typeof h === 'object') {
      for (const [field] of HANDOFF_FIELDS) if (h[field]) add(field.replace('_url', ''), redactUrl(h[field]));
    }
    return parts.length ? ' ' + parts.join(' ') : '';
  }

  // ------------------------------------------------------------------------------------------ controller

  /** @type {Map<number, {run: number, model: ProgressModel, interactive: boolean, win: (object|null), args: (object|null)}>} */
  const entries = new Map();
  let lastRun = null;

  function isOpen(win) {
    try { return !!win && !win.closed; } catch (e) { return false; }
  }

  function closeWindow(win) {
    try { if (win && !win.closed) win.close(); } catch (e) { /* already gone */ }
  }

  function quitting() {
    try { return typeof Services !== 'undefined' && !!Services.startup && Services.startup.shuttingDown === true; } catch (e) { return false; }
  }

  /** Open (or reopen) the window of one run. @returns {boolean} false if it could not be opened */
  function openWindowFor(entry, lowered) {
    if (quitting() || typeof Services === 'undefined' || !Services.ww) return false;
    const args = {
      model: entry.model,
      onAction: (item, action) => {
        handleAction(entry.run, item, action).catch((e) => ZFT.util.log('error', 'progress action failed: ' + ZFT.util.errText(e)));
      },
      onCancel: () => {
        try { ZFT.runs.cancelRun(entry.run); } catch (e) { ZFT.util.log('error', 'progress cancel failed: ' + ZFT.util.errText(e)); }
      },
      // Closing the window never cancels the run; it only forgets the window so finish() knows it is closed.
      onClose: () => { if (entry.args === args) { entry.win = null; entry.args = null; } },
      onCopy: () => entry.model.diagnostics(),
      format: (id, a) => ZFT.l10n.format(id, a),
    };
    args.wrappedJSObject = args;
    try {
      const features = 'chrome,dialog=no,resizable,centerscreen' + (lowered ? ',alwaysLowered' : '');
      entry.args = args;
      entry.win = Services.ww.openWindow(null, WINDOW_URL, '_blank', features, args);
      return true;
    } catch (e) {
      entry.win = null;
      entry.args = null;
      ZFT.util.log('error', 'progress window could not be opened: ' + ZFT.util.errText(e));
      return false;
    }
  }

  function bringToFront(win) {
    try {
      if (typeof Zotero !== 'undefined' && Zotero.Utilities && Zotero.Utilities.Internal && typeof Zotero.Utilities.Internal.activate === 'function') {
        Zotero.Utilities.Internal.activate(win);
      } else if (typeof win.focus === 'function') {
        win.focus();
      }
    } catch (e) { /* best effort */ }
  }

  /**
   * Create the model for a run and open its window (not stolen to the foreground when interactive=false).
   * @param {number} run @param {object[]} snapshots ItemSnapshot[] (titles) @param {{interactive?: boolean}} [opts]
   * @returns {ProgressModel}
   */
  function open(run, snapshots, opts) {
    const interactive = !!(opts && opts.interactive);
    const old = entries.get(run);
    if (old) closeWindow(old.win);
    const items = (Array.isArray(snapshots) ? snapshots : []).map((s) => ({ item: s.item, key: s.key, title: s.title }));
    const entry = { run, model: new ProgressModel(run, items), interactive, win: null, args: null };
    entries.set(run, entry);
    lastRun = run;
    openWindowFor(entry, !interactive);
    return entry.model;
  }

  /** Effect::Progress. */
  function update(run, item, row) {
    const entry = entries.get(run);
    if (!entry) {
      ZFT.util.log('debug', `progress: row for unknown run ${run} ignored`);
      return;
    }
    entry.model.applyRow(item, row);
  }

  /** Effect::RunFinished: store the report, switch the window to the grouped view, notify. */
  function finish(run, report) {
    const entry = entries.get(run);
    if (!entry) {
      ZFT.util.log('debug', `progress: report for unknown run ${run} ignored`);
      return;
    }
    entry.model.setReport(report);
    if (!isOpen(entry.win)) {
      const groups = entry.model.groups();
      // A closed window reopens only when the user has something to do; otherwise it stays closed (the Tools menu reopens it).
      if (groups && groups.needs_you.length > 0) {
        openWindowFor(entry, false);
      }
    }
  }

  /**
   * Show (info = {show, waiting}) or clear (null) the "Sign-in needed" banner of the progress window of `run`. A sign-in window
   * belongs to the run that triggered it but serves every waiting item, so when `run` is unknown the banner goes to every run that
   * has not finished. The taskbar button of an open progress window is flashed too (it is the window the user is most likely looking at).
   * @param {(number|null)} run @param {({show: function, waiting?: number}|null)} info
   */
  function setSignIn(run, info) {
    const targets = [];
    const own = run == null ? null : entries.get(run);
    if (own) targets.push(own);
    else for (const e of entries.values()) if (!e.model.report) targets.push(e);
    for (const entry of targets) {
      entry.model.setSignIn(info);
      if (info && isOpen(entry.win)) {
        try { if (typeof entry.win.getAttention === 'function') entry.win.getAttention(); } catch (e) { /* best effort */ }
      }
    }
  }

  /** Bring the window of `run` (default: the most recent run) to the front, reopening it if closed. @returns {boolean} false if no run is known */
  function show(run) {
    const entry = entries.get(run == null ? lastRun : run);
    if (!entry) return false;
    if (isOpen(entry.win)) {
      bringToFront(entry.win);
      return true;
    }
    entry.win = null;
    return openWindowFor(entry, false);
  }

  /** @returns {ProgressModel|null} */
  function modelFor(run) {
    const entry = entries.get(run);
    return entry ? entry.model : null;
  }

  /** Dispatch a RowAction (see header). @param {number} run @param {number} item @param {object} action proto.rs RowAction @returns {Promise<void>} */
  async function handleAction(run, item, action) {
    if (!action || typeof action.kind !== 'string') {
      ZFT.util.log('warn', 'progress: malformed row action ignored');
      return;
    }
    switch (action.kind) {
      case 'open_browser': {
        if (!isHttpUrl(action.url)) {
          ZFT.util.log('warn', 'progress: refused to open a non-http(s) address');
          return;
        }
        Zotero.launchURL(new URL(action.url).href);
        return;
      }
      case 'attach_file': {
        const entry = entries.get(run);
        await ZFT.save.attachFile(item, entry && isOpen(entry.win) ? entry.win : undefined);
        return;
      }
      case 'sign_in':
        ZFT.runs.userAction(run, 'sign_in_now');
        return;
      case 'retry':
        ZFT.runs.userAction(run, 'retry_sign_in');
        return;
      case 'cancel_item':
        ZFT.runs.cancelItem(run, item);
        return;
      default:
        ZFT.util.log('warn', `progress: unknown row action ${String(action.kind).slice(0, 40)} ignored`);
    }
  }

  /** Close all progress windows (plugin shutdown). Late rows and reports for the forgotten runs are ignored. */
  function closeAll() {
    for (const entry of Array.from(entries.values())) closeWindow(entry.win);
    entries.clear();
    lastRun = null;
  }

  ZFT.progress = {
    WINDOW_URL, WINDOW_TYPE, ROW_STATES, ProgressModel,
    GROUP_ORDER, GROUP_L10N, ACTION_L10N,
    open, update, finish, show, setSignIn, modelFor, handleAction, redactUrl, closeAll,
  };
})(this);
