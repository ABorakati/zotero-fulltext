/* progress-window.js: DOM glue for progress.xhtml (owner: js-ui). NOT in ZFT.LOAD_ORDER: runs in the window's own global.
 *
 * Contract with the plugin scope: ZFT.progress opens the window with
 *   Services.ww.openWindow(null, WINDOW_URL, '_blank', 'chrome,dialog=no,resizable,centerscreen', args)
 * where window.arguments[0].wrappedJSObject = { model: ZFT.progress.ProgressModel,
 *                                              onAction(item, action), onCancel(), onClose(), onCopy(), format(id, args) }.
 * This script subscribes to the model and re-renders on every change (batched with requestAnimationFrame):
 *   - header: counts line (data-l10n-id zotero-fulltext-progress-header with args) and, after the report, the summary
 *   - one row per item: title (truncated, full text in the tooltip), status text via document.l10n.setAttributes(el,
 *     row.l10n, row.args), and one <button> per RowAction (label from the action's l10n id)
 *   - after the report: grouped sections (model.groups()); the Cancel button is hidden and "Copy diagnostics" shown
 *   - Cancel -> args.onCancel(); Close -> window.close(); window 'unload' -> args.onClose() (does NOT cancel the run)
 * It must tolerate being opened after the run finished and being re-rendered thousands of times (no leaks).
 *
 * Rendering rules:
 *   - Rows are keyed by item id. A row node is created once and PATCHED in place (status text, state colour, buttons)
 *     only when its signature changed; the model tells us which item changed, so a frame touches only those rows.
 *   - Entering the report view re-parents the existing row nodes into the group sections (no node is rebuilt).
 *   - Buttons carry their own listener, so removing a button removes its listener; nothing is registered on the
 *     document per row. 'unload' unsubscribes from the model and cancels the pending frame.
 */
/* global window, document, requestAnimationFrame, cancelAnimationFrame, Components, Cc, Ci */
(function () {
  'use strict';

  const HTML_NS = 'http://www.w3.org/1999/xhtml';
  const MAX_TITLE_CHARS = 300;
  const GROUP_ORDER = ['attached', 'needs_you', 'not_found', 'failed', 'skipped'];
  const GROUP_L10N = {
    attached: 'zotero-fulltext-report-group-attached',
    needs_you: 'zotero-fulltext-report-group-needs-you',
    not_found: 'zotero-fulltext-report-group-not-found',
    failed: 'zotero-fulltext-report-group-failed',
    skipped: 'zotero-fulltext-report-group-skipped',
  };
  const STATE_COLOR = {
    queued: 'GrayText', working: 'inherit', waiting: '#b36b00', done: '#2a7a2a',
    attention: '#b36b00', failed: '#c00', skipped: 'GrayText', cancelled: 'GrayText',
  };

  const arg0 = window.arguments && window.arguments[0];
  const args = (arg0 && (arg0.wrappedJSObject || arg0)) || null;
  if (!args || !args.model) {
    return;
  }
  const model = args.model;

  /** item id -> {root, title, status, actions, statusSig, actionsSig} */
  const rowNodes = new Map();
  /** group -> {root, count, body} */
  const sections = new Map();
  /** items whose row changed since the last frame */
  const dirty = new Set();
  let needFull = true;
  let frame = 0;
  let disposed = false;
  let unsubscribe = null;
  let shownGroups = null;

  function el(id) {
    return document.getElementById(id);
  }

  function html(tag, style) {
    const node = document.createElementNS(HTML_NS, tag);
    if (style) {
      node.setAttribute('style', style);
    }
    return node;
  }

  function setMessage(node, id, a) {
    document.l10n.setAttributes(node, id, a || null);
  }

  // ------------------------------------------------------------------------------------------ rows

  function createRowNode(item) {
    const root = html('div', 'display:flex;gap:8px;align-items:center;padding:3px 0;border-bottom:1px solid rgba(128,128,128,0.2);');
    root.setAttribute('role', 'listitem');
    root.setAttribute('data-item', String(item));
    const title = html('div', 'flex:1 1 45%;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;');
    const status = html('span', 'flex:1 1 35%;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;');
    const actions = html('span', 'flex:none;display:flex;gap:4px;');
    root.appendChild(title);
    root.appendChild(status);
    root.appendChild(actions);
    const node = { root, title, status, actions, statusSig: null, actionsSig: null };
    rowNodes.set(item, node);
    return node;
  }

  function rowNode(item) {
    return rowNodes.get(item) || createRowNode(item);
  }

  function actionsSignature(actions) {
    return actions.map((a) => `${a.kind}:${a.url || ''}:${a.l10n || ''}`).join(',');
  }

  function buildActionButton(item, action) {
    const button = html('button');
    button.setAttribute('type', 'button');
    setMessage(button, model.actionL10n(action));
    button.addEventListener('click', () => {
      if (typeof args.onAction === 'function') {
        args.onAction(item, action);
      }
    });
    return button;
  }

  /** Bring one row node up to date with the model. `actions` overrides row.actions (report view: merged hand-off links). */
  function patchRow(item, actions) {
    const row = model.rowFor(item);
    if (!row) {
      return;
    }
    const node = rowNode(item);
    const titleText = row.title || row.key || '';
    if (node.title.textContent !== titleText.slice(0, MAX_TITLE_CHARS)) {
      node.title.textContent = titleText.slice(0, MAX_TITLE_CHARS);
      node.title.setAttribute('title', titleText.slice(0, 1000));
    }
    const sig = row.state + '|' + row.l10n + '|' + JSON.stringify(row.args);
    if (node.statusSig !== sig) {
      node.statusSig = sig;
      setMessage(node.status, row.l10n, row.args);
      node.status.style.color = STATE_COLOR[row.state] || 'inherit';
      node.root.setAttribute('data-state', row.state);
    }
    const list = actions || row.actions;
    const asig = actionsSignature(list);
    if (node.actionsSig !== asig) {
      node.actionsSig = asig;
      node.actions.textContent = '';
      for (const action of list) {
        node.actions.appendChild(buildActionButton(item, action));
      }
    }
  }

  // ------------------------------------------------------------------------------------------ report view

  function ensureSection(group) {
    let section = sections.get(group);
    if (!section) {
      const root = html('div', 'margin:10px 0 4px;');
      root.setAttribute('role', 'group');
      const heading = html('div', 'font-weight:bold;padding:2px 0;');
      const label = html('span');
      setMessage(label, GROUP_L10N[group]);
      const count = html('span', 'font-weight:normal;margin-inline-start:6px;opacity:0.7;');
      heading.appendChild(label);
      heading.appendChild(count);
      const body = html('div');
      root.appendChild(heading);
      root.appendChild(body);
      section = { root, count, body };
      sections.set(group, section);
    }
    return section;
  }

  /** Move the existing row nodes under their group headings (once per distinct groups object). */
  function layoutGroups(groups) {
    const rowsEl = el('zft-progress-rows');
    const placed = new Set();
    for (const group of GROUP_ORDER) {
      const entries = groups[group];
      const section = sections.get(group);
      if (!entries.length) {
        if (section && section.root.parentNode) {
          section.root.remove();
        }
        continue;
      }
      const s = ensureSection(group);
      s.count.textContent = String(entries.length);
      rowsEl.appendChild(s.root);
      for (const entry of entries) {
        patchRow(entry.item, entry.actions);
        s.body.appendChild(rowNode(entry.item).root);
        placed.add(entry.item);
      }
    }
    // Rows the report does not mention stay visible, below the sections, in selection order.
    for (const row of model.rows()) {
      if (!placed.has(row.item)) {
        patchRow(row.item);
        rowsEl.appendChild(rowNode(row.item).root);
      }
    }
    shownGroups = groups;
  }

  // ------------------------------------------------------------------------------------------ header and chrome

  function renderHeader(groups) {
    const header = el('zft-progress-header');
    const ledger = el('zft-progress-ledger');
    if (groups) {
      setMessage(header, 'zotero-fulltext-report-summary', {
        saved: groups.attached.length,
        needs: groups.needs_you.length,
        notfound: groups.not_found.length,
        failed: groups.failed.length,
        skipped: groups.skipped.length,
      });
      const l = (model.report && model.report.ledger) || {};
      setMessage(ledger, 'zotero-fulltext-report-ledger', {
        hour: l.hour || 0, hour_cap: l.hour_cap || 0,
        day: l.day || 0, day_cap: l.day_cap || 0,
        mb: l.mb_hour || 0, mb_cap: l.mb_hour_cap || 0,
      });
      ledger.hidden = false;
    } else {
      const c = model.counts();
      setMessage(header, 'zotero-fulltext-progress-header', { done: c.finished, total: c.total, working: c.working, waiting: c.waiting });
    }
  }

  function render() {
    if (disposed) {
      return;
    }
    const groups = model.groups();
    if (groups) {
      // The model hands out a new groups object whenever the report or any row changed, so a late row (a retry after
      // the report) lands here too and the layout pass re-patches every row with its merged hand-off actions.
      if (groups !== shownGroups) {
        layoutGroups(groups);
        el('zft-progress-cancel').hidden = true;
        el('zft-progress-copy').hidden = false;
      }
    } else if (needFull) {
      const rowsEl = el('zft-progress-rows');
      for (const row of model.rows()) {
        patchRow(row.item);
        rowsEl.appendChild(rowNode(row.item).root);
      }
    } else {
      for (const item of dirty) {
        patchRow(item);
      }
    }
    needFull = false;
    dirty.clear();
    renderHeader(groups);
    renderSignIn();
  }

  /** The "Sign-in needed" banner: visible while the model says a sign-in window is open (button = show that window). */
  function renderSignIn() {
    const banner = el('zft-progress-signin');
    if (banner) {
      banner.hidden = !(typeof model.signIn === 'function' && model.signIn());
    }
  }

  function schedule() {
    if (frame || disposed) {
      return;
    }
    const run = () => {
      frame = 0;
      render();
    };
    frame = typeof requestAnimationFrame === 'function' ? requestAnimationFrame(run) : window.setTimeout(run, 16);
  }

  // ------------------------------------------------------------------------------------------ lifecycle

  function copyDiagnostics() {
    const text = typeof args.onCopy === 'function' ? String(args.onCopy() || '') : model.diagnostics();
    const classes = typeof Cc !== 'undefined' ? Cc : Components.classes;
    const ifaces = typeof Ci !== 'undefined' ? Ci : Components.interfaces;
    classes['@mozilla.org/widget/clipboardhelper;1'].getService(ifaces.nsIClipboardHelper).copyString(text);
  }

  function dispose() {
    if (disposed) {
      return;
    }
    disposed = true;
    if (unsubscribe) {
      unsubscribe();
      unsubscribe = null;
    }
    if (frame) {
      if (typeof cancelAnimationFrame === 'function') {
        cancelAnimationFrame(frame);
      }
      frame = 0;
    }
    rowNodes.clear();
    sections.clear();
    dirty.clear();
    try {
      if (typeof args.onClose === 'function') {
        args.onClose();
      }
    } catch (e) { /* the controller may already be gone */ }
  }

  window.addEventListener('load', () => {
    el('zft-progress-cancel').addEventListener('click', () => {
      el('zft-progress-cancel').disabled = true;
      if (typeof args.onCancel === 'function') {
        args.onCancel();
      }
    });
    el('zft-progress-close').addEventListener('click', () => window.close());
    el('zft-progress-signin-show').addEventListener('click', () => {
      try {
        const info = typeof model.signIn === 'function' ? model.signIn() : null;
        if (info && typeof info.show === 'function') {
          info.show();
        }
      } catch (e) {
        try { Components.utils.reportError(e); } catch (e2) { /* nothing left to do */ }
      }
    });
    el('zft-progress-copy').addEventListener('click', () => {
      try {
        copyDiagnostics();
      } catch (e) {
        try { Components.utils.reportError(e); } catch (e2) { /* nothing left to do */ }
      }
    });
    unsubscribe = model.subscribe((m, item) => {
      if (item !== null && item !== undefined) {
        dirty.add(item);
      }
      // Not through the frame scheduler: an occluded window (the sign-in window sits on top of it) gets no animation frames.
      renderSignIn();
      schedule();
    });
    render();
    document.l10n.formatValue('zotero-fulltext-progress-title').then(
      (title) => { document.title = title; },
      () => { /* keep the default title */ }
    );
  });

  window.addEventListener('unload', dispose);
})();
