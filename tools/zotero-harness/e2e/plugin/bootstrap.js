/* zotero-fulltext E2E driver (bootstrap.js). Runs ONLY in the throwaway profile that tools/zotero-harness/e2e.ps1 creates.
 *
 * It is a second plugin (id e2e@zotero-fulltext.local) loaded next to the real one (zotero-fulltext@aborakati). Flow:
 *   1. wait for Zotero + zotero-fulltext (Zotero.ZoteroFullText = ZFT, menus registered, pane registered)
 *   2. switch institutional access OFF through prefs (OA only, no login or challenge windows), read the engine config back
 *   3. create four test items in the throwaway library: gold OA, arXiv, medRxiv, paywalled
 *   4. select them in the items tree and click the real "Get Full Text" entry of the real item context menu
 *      (ZoteroPane.buildItemContextMenu -> menuitem.doCommand -> the onCommand ZFT.menu registered). If the UI route
 *      fails it falls back to the registered onCommand, then to ZFT.menu.commandFor; the route used is reported.
 *   5. wait for the RunFinished report (handle.done, timeout pref extensions.zotero-e2e.timeoutSec, default 240 s)
 *   6. inspect the attachments (content type, size, %PDF- magic, %%EOF), windows opened, prompts, console errors, debug log,
 *      event-loop lag during the run, run the plugin's own smoke checks
 *   7. evaluate -> checks[] + verdict, write the JSON result atomically (IOUtils.writeUTF8 + tmpPath), quit Zotero
 *
 * Prefs (user.js, set by e2e.ps1): extensions.zotero-e2e.{resultFile,timeoutSec,mode,cases,smoke}.
 * Result path fallback: extensions.zotero-harness.resultFile, then env ZOTERO_HARNESS_RESULT_FILE.
 * extensions.zotero-harness.quitWhenDone=false keeps Zotero running.
 * The pure parts (evaluate, pdfFacts, parseCaseFilter, CASES) are exported for Node when `module` exists.
 */
/* global Zotero, Services, Ci, Components, IOUtils, PathUtils, ChromeUtils, module */

var E2E = (function () {
  'use strict';

  const PLUGIN_ID = 'e2e@zotero-fulltext.local';
  const PREF = 'extensions.zotero-e2e.';
  const HARNESS_RESULT_PREF = 'extensions.zotero-harness.resultFile';
  const QUIT_PREF = 'extensions.zotero-harness.quitWhenDone';
  const SCHEMA = 'zotero-fulltext/e2e/v1';

  const DEFAULT_RUN_TIMEOUT_SEC = 240;
  const ZFT_WAIT_MS = 90000;
  const UI_READY_WAIT_MS = 60000;
  const HANDLE_WAIT_MS = 60000;
  const CANCEL_GRACE_MS = 20000;
  const QUIT_SETTLE_MS = 2000;
  /** Watchdog = run timeout + this: whatever is known is written, then Zotero quits. */
  const WATCHDOG_EXTRA_MS = 120000;
  const MIN_PDF_BYTES = 1024;
  const LAG_WARN_MS = 750;
  const LAG_FAIL_MS = 2500;
  const HEARTBEAT_MS = 100;
  const SMOKE_DEFAULT = ['modules', 'wasm', 'prefs', 'menu', 'menu_ui', 'prefpane', 'ftl'];
  const LOGIN_WINDOW_TYPE = 'zotero-fulltext:login';
  const PROGRESS_WINDOW_TYPE = 'zotero-fulltext:progress';
  const INST_TRACE_RE = /ezproxy|openathens|alma|libkey|login|institution/i;

  /** Prefs (short keys under extensions.zotero-fulltext.) that make the run OA only and window free. */
  const INST_OFF = {
    'inst.enabled': false,
    'inst.auto_open_login': false,
    'inst.challenge_windows': false,
  };

  /**
   * The four items. expect: 'saved' = a PDF attachment must appear; 'not_found' = the engine must report NotFound and nothing is
   * attached (institutional access is off, so a paywalled paper can only be NotFound). soft: a miss is a warning, not a failure
   * (medRxiv sits behind a Cloudflare WAF that blocks intermittently, see fixtures/oa/README.md).
   */
  const CASES = [
    {
      key: 'gold', label: 'gold OA (PLOS Medicine, PRISMA)', itemType: 'journalArticle', doi: '10.1371/journal.pmed.1000097',
      title: 'Preferred Reporting Items for Systematic Reviews and Meta-Analyses: The PRISMA Statement',
      publicationTitle: 'PLoS Medicine', date: '2009', expect: 'saved', soft: false,
    },
    {
      key: 'arxiv', label: 'arXiv (Attention Is All You Need)', itemType: 'preprint', doi: '10.48550/arXiv.1706.03762',
      title: 'Attention Is All You Need', repository: 'arXiv', date: '2017', expect: 'saved', soft: false,
    },
    {
      key: 'medrxiv', label: 'medRxiv (bariatric surgery cohort)', itemType: 'preprint', doi: '10.1101/2023.05.31.23290770',
      title: 'Long-term clinical outcomes of bariatric surgery in adults with severe obesity: A population-based retrospective cohort study',
      repository: 'medRxiv', date: '2023', expect: 'saved', soft: true,
    },
    {
      key: 'paywalled', label: 'paywalled (Annals of Surgery)', itemType: 'journalArticle', doi: '10.1097/SLA.0000000000005603',
      title: 'Use of Cold-Stored Whole Blood is Associated With Improved Mortality in Hemostatic Resuscitation of Major Bleeding',
      publicationTitle: 'Annals of Surgery', date: '2022', expect: 'not_found', soft: false,
    },
  ];

  // ------------------------------------------------------------------ pure helpers (testable in Node)

  function errText(e) {
    if (e === undefined || e === null) return String(e);
    const name = e.name ? e.name + ': ' : '';
    return (name + (e.message !== undefined ? e.message : String(e))).slice(0, 600);
  }

  function clip(s, n) {
    const t = String(s);
    return t.length > n ? t.slice(0, n) + '...' : t;
  }

  function sameSet(a, b) {
    const x = Array.from(a).sort((p, q) => p - q);
    const y = Array.from(b).sort((p, q) => p - q);
    return x.length === y.length && x.every((v, i) => v === y[i]);
  }

  /** @param {string} text comma list of case keys, '' = all @returns {object[]} */
  function parseCaseFilter(text) {
    const wanted = String(text || '').split(',').map((s) => s.trim().toLowerCase()).filter(Boolean);
    if (!wanted.length) return CASES.slice();
    return CASES.filter((c) => wanted.includes(c.key));
  }

  function ascii(bytes) {
    let s = '';
    for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i] & 0x7f);
    return s;
  }

  /** @param {Uint8Array} head first bytes @param {Uint8Array} tail last bytes @param {number} size */
  function pdfFacts(head, tail, size) {
    const headText = ascii(head || []);
    const tailText = ascii(tail || []);
    return {
      size,
      head: headText.slice(0, 8),
      pdfMagic: headText.startsWith('%PDF-'),
      eof: tailText.includes('%%EOF'),
    };
  }

  function outcomeSummary(o) {
    if (!o || typeof o !== 'object') return String(o);
    const rest = Object.keys(o).filter((k) => k !== 'kind' && k !== 'handoff' && k !== 'url' && k !== 'attachment_key')
      .map((k) => `${k}=${typeof o[k] === 'object' ? JSON.stringify(o[k]) : o[k]}`);
    return o.kind + (rest.length ? ' (' + rest.join(', ') + ')' : '');
  }

  /**
   * @param {object} R the result object (see main())
   * @returns {{verdict: 'PASS'|'FAIL', counts: {pass: number, warn: number, fail: number}, checks: {name: string, status: string, detail: string}[]}}
   */
  function evaluate(R) {
    const checks = [];
    const add = (name, status, detail) => { checks.push({ name, status, detail: detail === undefined || detail === null ? '' : String(detail) }); };
    const meta = R.meta || {};
    const items = Array.isArray(R.items) ? R.items : [];
    const run = R.run || {};
    const report = run.report || null;

    if (R.fatal) add('driver', 'fail', R.fatal);

    // plugin
    if (R.zft && R.zft.ready) add('plugin_started', 'pass', `zotero-fulltext ${R.zft.version || '?'} ready ${R.zft.readyMs} ms after the driver started`);
    else add('plugin_started', 'fail', (R.zft && R.zft.reason) || 'zotero-fulltext never became ready');

    // menu registration
    const menu = R.menu || {};
    const wanted = ['zotero-fulltext-menu-get', 'zotero-fulltext-menu-get-oa'];
    const entries = [];
    for (const o of menu.options || []) {
      if (o.target === 'main/library/item') for (const e of o.entries || []) entries.push(e.l10nID);
    }
    if (!menu.keys || menu.keys.length < 2) add('menu_registered', 'fail', 'MenuManager.registerMenu keys: ' + JSON.stringify(menu.keys || []));
    else if (menu.optionsError) add('menu_registered', 'warn', `keys ${JSON.stringify(menu.keys)}; MenuManager internals unreadable: ${menu.optionsError}`);
    else if (!wanted.every((w) => entries.includes(w))) add('menu_registered', 'fail', `main/library/item entries ${JSON.stringify(entries)}, want ${JSON.stringify(wanted)}`);
    else add('menu_registered', 'pass', `keys ${JSON.stringify(menu.keys)}; item entries ${entries.join(', ')}`);

    // preference pane
    const pane = R.prefsPane || {};
    if (pane.paneID && pane.listed) add('prefs_pane_registered', 'pass', `pane ${pane.paneID} is in Zotero.PreferencePanes.pluginPanes`);
    else add('prefs_pane_registered', 'fail', `paneID ${pane.paneID || 'none'}, listed ${!!pane.listed}`);

    // institutional access off
    const inst = R.engineInst || null;
    if (inst && inst.enabled === false) add('institutional_access_disabled', 'pass', `EngineConfig.inst.enabled=false, auto_open_login=${inst.auto_open_login}, challenge_windows=${inst.challenge_windows}`);
    else add('institutional_access_disabled', 'fail', 'EngineConfig.inst = ' + JSON.stringify(inst));

    // how the command was invoked
    const inv = R.invoke || {};
    if (inv.via === 'menu-ui') add('menu_command_path', 'pass', `real item context menu (${inv.l10nID}), selection ${JSON.stringify(inv.selectedIDs)}`);
    else if (inv.via) add('menu_command_path', 'warn', `fell back to ${inv.via}: ${inv.note}`);
    else add('menu_command_path', 'fail', 'the command was never invoked: ' + (inv.note || 'unknown'));
    const calls = inv.startRunCalls || [];
    if (inv.via) {
      if (calls.length !== 1) add('start_run_called_once', 'fail', `ZFT.runs.startRun was called ${calls.length} times`);
      else if (calls[0].interactive !== true || calls[0].mode !== meta.mode || !sameSet(calls[0].items, items.map((i) => i.itemID))) {
        add('start_run_called_once', 'fail', 'startRun arguments differ from the menu contract: ' + JSON.stringify(calls[0]));
      } else add('start_run_called_once', 'pass', `startRun({mode: ${calls[0].mode}, interactive: true, ${calls[0].items.length} items}) -> run ${calls[0].run}`);
    }

    // the run
    if (run.timedOut) add('run_finished', 'fail', `no RunFinished within ${meta.timeoutSec} s (cancel sent; report after cancel: ${report ? 'yes' : 'no'})`);
    else if (!report) add('run_finished', 'fail', 'no RunReport: ' + (run.note || 'startRun never returned a handle'));
    else add('run_finished', 'pass', `run ${report.run} finished in ${run.elapsedMs} ms; counts ${JSON.stringify(report.counts)}`);
    if (report) {
      if (report.engine_restarts) add('engine_stable', 'fail', `engine_restarts=${report.engine_restarts}, halted=${report.halted}`);
      else add('engine_stable', 'pass', 'no engine restart' + (report.halted ? `; halted=${report.halted}` : ''));
    }

    // per item
    for (const it of items) {
      const kind = it.outcome && it.outcome.kind;
      const att = it.attachments || [];
      const validAtt = att.filter((a) => a.valid);
      const name = 'item:' + it.key;
      const what = outcomeSummary(it.outcome);
      if (!it.outcome) { add(name, it.soft ? 'warn' : 'fail', 'no entry in the RunReport'); continue; }
      if (it.expect === 'saved') {
        if (kind === 'saved' && validAtt.length === 1 && att.length === 1) {
          add(name, 'pass', `saved via ${it.outcome.source} (${it.outcome.route}, ${it.outcome.version}); ${validAtt[0].size} bytes`);
        } else if (kind === 'saved') {
          add(name, 'fail', `outcome saved but ${att.length} attachments, ${validAtt.length} valid PDFs`);
        } else {
          add(name, it.soft ? 'warn' : 'fail', `expected a saved PDF, got ${what}`);
        }
      } else if (it.expect === 'not_found') {
        if (kind === 'not_found' && att.length === 0) add(name, 'pass', `${what}; nothing attached`);
        else if (kind === 'saved' && validAtt.length === 1) add(name, 'warn', 'an open copy was found for the "paywalled" paper (source drift?): ' + it.outcome.source);
        else add(name, 'fail', `expected not_found, got ${what}; ${att.length} attachments`);
      }
    }

    // attachment sanity
    const all = [];
    for (const it of items) for (const a of it.attachments || []) all.push(Object.assign({ item: it.key }, a));
    const badAtt = all.filter((a) => !a.valid);
    const noEof = all.filter((a) => a.valid && !a.eof);
    if (badAtt.length) add('attachments_valid', 'fail', badAtt.map((a) => `${a.item}: ${a.contentType}, ${a.size} bytes, head ${JSON.stringify(a.head)}`).join('; '));
    else if (noEof.length) add('attachments_valid', 'warn', `no %%EOF in the last KiB of: ${noEof.map((a) => a.item).join(', ')} (truncated PDF?)`);
    else add('attachments_valid', 'pass', all.length ? `${all.length} PDF attachment(s): application/pdf, %PDF- magic, %%EOF, >= ${MIN_PDF_BYTES} bytes` : 'no attachments created');

    // no institutional route, no windows
    const w = R.windows || {};
    if (w.loginOpened) add('no_login_windows', 'fail', 'a login window opened: ' + JSON.stringify((w.opened || []).filter((o) => o.windowtype === LOGIN_WINDOW_TYPE || /login\.xhtml/.test(o.href || ''))));
    else add('no_login_windows', 'pass', `${(w.opened || []).length} window(s) opened during the run, none a login window` + ((w.opened || []).length ? ` (${(w.opened || []).map((o) => o.windowtype || o.href).join(', ')})` : ''));
    if (inv.via && !(w.opened || []).some((o) => o.windowtype === 'zotero-fulltext:progress' || /progress.xhtml/.test(o.href || ''))) {
      add('progress_window_opened', 'warn', 'the interactive run did not open the progress window');
    } else if (inv.via) add('progress_window_opened', 'pass', 'progress.xhtml opened for the interactive run');
    const instSteps = [];
    for (const it of items) for (const s of it.trace || []) if (INST_TRACE_RE.test(String(s.src) + ' ' + String(s.tier))) instSteps.push(`${it.key}:${s.src}/${s.tier}`);
    if (instSteps.length) add('no_institutional_trace', 'fail', 'institutional steps in the trace: ' + instSteps.join(', '));
    else add('no_institutional_trace', 'pass', 'no alma/ezproxy/openathens/libkey step in any item trace');

    // dialogs
    const prompts = R.prompts || [];
    if (prompts.length) add('no_prompts', 'fail', `${prompts.length} dialog(s) intercepted; first: ${JSON.stringify(prompts[0])}`);
    else add('no_prompts', 'pass', 'no alert/confirm/prompt dialog was requested' + (R.promptSpy === 'none' ? ' (spy not installed, so this only means nothing blocked)' : ''));

    // UI responsiveness
    const lag = R.eventLoop || {};
    if (!lag.samples) add('ui_not_blocked', 'warn', 'no event-loop samples');
    else if (lag.maxMs > LAG_FAIL_MS) add('ui_not_blocked', 'fail', `main thread stalled ${lag.maxMs} ms (limit ${LAG_FAIL_MS}); ${lag.over250} ticks over 250 ms`);
    else if (lag.maxMs > LAG_WARN_MS) add('ui_not_blocked', 'warn', `main thread stalled ${lag.maxMs} ms (warn above ${LAG_WARN_MS}); ${lag.over250} ticks over 250 ms`);
    else add('ui_not_blocked', 'pass', `max timer lag ${lag.maxMs} ms over ${lag.samples} ticks of ${HEARTBEAT_MS} ms; ${lag.over250} ticks over 250 ms`);

    // what the progress window shows
    const ui = R.progressUI;
    if (ui) {
      if (!ui.found) add('progress_window_text', 'warn', 'the progress window was no longer open to read' + (ui.error ? ': ' + ui.error : ''));
      else {
        const bad = [].concat(ui.untranslated || [], ui.rawIds || []);
        if (bad.length) add('progress_window_text', 'fail', 'untranslated or raw message ids on screen: ' + bad.slice(0, 6).join(', '));
        else if (!ui.header) add('progress_window_text', 'fail', 'the progress window header is empty');
        else if ((ui.missingTitles || []).length) add('progress_window_text', 'fail', 'item titles missing from the window: ' + ui.missingTitles.map((t) => clip(t, 40)).join(' | '));
        else add('progress_window_text', 'pass', `header "${clip(ui.header, 90)}"; ${ui.l10nCount} translated labels; buttons [${(ui.buttons || []).join(', ')}]`);
      }
    }

    // errors
    const ours = (R.consoleErrors || []).filter((e) => e.ours);
    const debugErrors = (R.debug && R.debug.errorsOurs) || [];
    if (ours.length || debugErrors.length) {
      add('no_errors_from_plugin', 'fail', `console ${ours.length} (first: ${ours[0] ? clip(ours[0].text, 200) : '-'}); debug log level-1 ${debugErrors.length} (first: ${debugErrors[0] ? clip(debugErrors[0], 200) : '-'})`);
    } else add('no_errors_from_plugin', 'pass', 'no console error from zotero-fulltext sources, no level-1 line in the debug log');
    // Zotero 10.0.6 logs a source-less "uncaught exception: undefined" shortly after startup for ANY plugin that ships a locale .ftl
    // file (reproduced with a plugin that has only a one-line FTL and an empty bootstrap.js), so it says nothing about this plugin.
    const isKnownZoteroNoise = (e) => !e.source && e.text === 'uncaught exception: undefined';
    const other = (R.consoleErrors || []).filter((e) => !e.ours && !isKnownZoteroNoise(e));
    const noise = (R.consoleErrors || []).filter((e) => !e.ours && isKnownZoteroNoise(e));
    if (other.length) add('other_console_errors', 'warn', `${other.length} console error(s) not from zotero-fulltext; first: ${clip(other[0].text, 200)}`);
    else if (noise.length) add('other_console_errors', 'pass', `only Zotero's own startup message "uncaught exception: undefined" (${noise.length}x; seen with any FTL-shipping plugin, not ours)`);

    // the plugin's own smoke checks
    if (R.smoke) {
      const failed = Object.keys(R.smoke).filter((k) => !R.smoke[k].ok);
      if (failed.length) add('smoke_checks', 'warn', failed.map((k) => `${k}: ${clip(R.smoke[k].detail, 160)}`).join(' | '));
      else add('smoke_checks', 'pass', Object.keys(R.smoke).join(', '));
    }

    const counts = { pass: 0, warn: 0, fail: 0 };
    for (const c of checks) counts[c.status] += 1;
    return { verdict: counts.fail ? 'FAIL' : 'PASS', counts, checks };
  }

  // ------------------------------------------------------------------ platform helpers

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  async function pollFor(fn, ms, step) {
    const end = Date.now() + ms;
    for (;;) {
      let v = null;
      try { v = await fn(); } catch (e) { v = null; }
      if (v) return v;
      if (Date.now() >= end) return null;
      await sleep(step || 100);
    }
  }

  /** @returns {Promise<{timedOut?: true, value?: any, error?: string}>} */
  function withTimeout(promise, ms) {
    return new Promise((resolve) => {
      const id = setTimeout(() => resolve({ timedOut: true }), ms);
      Promise.resolve(promise).then(
        (value) => { clearTimeout(id); resolve({ value }); },
        (e) => { clearTimeout(id); resolve({ error: errText(e) }); },
      );
    });
  }

  function prefStr(name, d) { try { return Services.prefs.getStringPref(name, d); } catch (e) { return d; } }
  function prefInt(name, d) { try { return Services.prefs.getIntPref(name, d); } catch (e) { return d; } }
  function prefBool(name, d) { try { return Services.prefs.getBoolPref(name, d); } catch (e) { return d; } }

  function CI() { return typeof Ci !== 'undefined' ? Ci : Components.interfaces; }

  let S = null; // driver state, null before start() and after stop()

  function plog(msg) {
    const line = '+' + (S ? Date.now() - S.t0 : 0) + 'ms ' + msg;
    if (S) S.R.driverLog.push(line);
    try { Zotero.debug('[zotero-fulltext-e2e] ' + msg); } catch (e) { /* logging must never throw */ }
  }

  function mark(phase) {
    if (!S) return;
    S.R.phases[phase] = Date.now() - S.t0;
    plog('phase ' + phase);
  }

  function resultPath() {
    let p = prefStr(PREF + 'resultFile', '') || prefStr(HARNESS_RESULT_PREF, '');
    if (!p) { try { p = Services.env.get('ZOTERO_HARNESS_RESULT_FILE') || ''; } catch (e) { p = ''; } }
    return p;
  }

  // ------------------------------------------------------------------ observers: console, windows, prompts, event loop

  /** A console source that belongs to zotero-fulltext itself (chrome://zotero-fulltext/ or <repo>/plugin/), not to this driver. */
  function isOursSource(source) {
    const s = String(source || '');
    if (/zotero-harness[\\/]+e2e/.test(s)) return false;
    return /chrome:\/\/zotero-fulltext\//.test(s) || /zotero-fulltext[\\/]+plugin[\\/]/.test(s);
  }

  function installConsole(R) {
    const ci = CI();
    const listener = {
      observe(msg) {
        try {
          if (!(msg instanceof ci.nsIScriptError)) return;
          const warning = !!(msg.flags & ci.nsIScriptError.warningFlag);
          const source = String(msg.sourceName || '');
          const text = String(msg.errorMessage || msg.message || '');
          const ours = isOursSource(source) || /\[zotero-fulltext\]/.test(text);
          if (warning) { R.consoleWarnings += 1; return; }
          let stack = '';
          try {
            for (let f = msg.stack, n = 0; f && n < 12; f = f.parent || f.caller, n++) stack += `${f.functionDisplayName || '?'}@${f.source}:${f.line}:${f.column}\n`;
          } catch (e) { /* no stack on this message */ }
          if (R.consoleErrors.length < 100) R.consoleErrors.push({ t: Date.now() - S.t0, source: clip(source, 200), line: msg.lineNumber, ours, category: String(msg.category || ''), stack: clip(stack, 1500), text: clip(text, 500) });
        } catch (e) { /* a listener must not throw */ }
      },
      QueryInterface: ChromeUtils.generateQI(['nsIConsoleListener']),
    };
    Services.console.registerListener(listener);
    const api = {
      observe(subject) {
        try {
          const m = subject.wrappedJSObject;
          if (!m || m.level !== 'error') return;
          const source = String(m.filename || '');
          const text = (m.arguments || []).map((a) => { try { return typeof a === 'string' ? a : String(a); } catch (e) { return '?'; } }).join(' ');
          if (R.consoleErrors.length < 100) R.consoleErrors.push({ t: Date.now() - S.t0, source: clip(source, 200), line: m.lineNumber, ours: isOursSource(source), api: true, text: clip(text, 500) });
        } catch (e) { /* ignore */ }
      },
    };
    Services.obs.addObserver(api, 'console-api-log-event');
    return () => {
      try { Services.console.unregisterListener(listener); } catch (e) { /* gone */ }
      try { Services.obs.removeObserver(api, 'console-api-log-event'); } catch (e) { /* gone */ }
    };
  }

  function listWindows() {
    const out = [];
    try {
      for (const w of Services.wm.getEnumerator(null)) {
        try { out.push({ href: String(w.location.href), windowtype: w.document.documentElement.getAttribute('windowtype') || '' }); } catch (e) { out.push({ href: '?', windowtype: '' }); }
      }
    } catch (e) { /* enumerator unavailable */ }
    return out;
  }

  function installWindowWatch(R) {
    const observer = {
      observe(subject, topic) {
        if (topic !== 'domwindowopened' || !S || !S.watchWindows) return;
        const rec = { t: Date.now() - S.t0, href: '', windowtype: '' };
        if (R.windows.opened.length < 60) R.windows.opened.push(rec);
        const fill = () => {
          try {
            rec.href = String(subject.location.href);
            rec.windowtype = subject.document.documentElement.getAttribute('windowtype') || '';
            if (rec.windowtype === LOGIN_WINDOW_TYPE || /login\.xhtml/.test(rec.href)) R.windows.loginOpened = true;
          } catch (e) { /* window already gone */ }
        };
        try { subject.addEventListener('load', fill, { once: true }); } catch (e) { /* ignore */ }
        fill();
      },
    };
    Services.ww.registerNotification(observer);
    return () => { try { Services.ww.unregisterNotification(observer); } catch (e) { /* gone */ }; };
  }

  /** Replaces Services.prompt so a surprise dialog is recorded instead of blocking a headless run for ever. */
  function installPromptSpy(R) {
    const defaults = { alert: undefined, alertCheck: undefined, confirm: false, confirmCheck: false, confirmEx: 1, prompt: false, promptUsernameAndPassword: false, promptPassword: false, select: false };
    const stub = {};
    for (const name of Object.keys(defaults)) {
      stub[name] = function () {
        try {
          R.prompts.push({ method: name, text: Array.from(arguments).filter((a) => typeof a === 'string').map((s) => clip(s, 300)) });
        } catch (e) { /* ignore */ }
        return defaults[name];
      };
    }
    let real = null;
    try { real = Services.prompt; } catch (e) { real = null; }
    try {
      Object.defineProperty(Services, 'prompt', { configurable: true, enumerable: true, get() { return stub; } });
      if (Services.prompt === stub) {
        R.promptSpy = 'defineProperty';
        return () => { try { Object.defineProperty(Services, 'prompt', { configurable: true, enumerable: true, get() { return real; } }); } catch (e) { /* gone */ } };
      }
    } catch (e) { /* try the next way */ }
    try {
      const saved = {};
      for (const name of Object.keys(defaults)) { saved[name] = real[name]; real[name] = stub[name]; }
      if (real.alert === stub.alert) {
        R.promptSpy = 'assign';
        return () => { for (const name of Object.keys(saved)) { try { real[name] = saved[name]; } catch (e) { /* gone */ } } };
      }
    } catch (e) { /* no spy */ }
    R.promptSpy = 'none';
    return () => {};
  }

  /** Timer drift during the run: a main thread that is blocked delays this tick by the length of the stall. */
  function startHeartbeat(R) {
    const lag = R.eventLoop;
    let last = Date.now();
    let stopped = false;
    const started = last;
    function tick() {
      if (stopped) return;
      const now = Date.now();
      const late = Math.max(0, now - last - HEARTBEAT_MS);
      last = now;
      lag.samples += 1;
      if (late > lag.maxMs) { lag.maxMs = late; lag.atMs = now - started; }
      if (late > 250) lag.over250 += 1;
      setTimeout(tick, HEARTBEAT_MS);
    }
    setTimeout(tick, HEARTBEAT_MS);
    return () => { stopped = true; };
  }

  // ------------------------------------------------------------------ zotero-fulltext access

  async function waitForZft(R) {
    const zft = await pollFor(() => {
      const Z = Zotero.ZoteroFullText;
      if (Z && Z.plugin && Z.plugin.started && Z.menu && Z.runs && Z.config && Z.menu.registeredKeys().length >= 2) return Z;
      return null;
    }, ZFT_WAIT_MS, 200);
    if (!zft) {
      const Z = Zotero.ZoteroFullText;
      R.zft = { ready: false, reason: 'Zotero.ZoteroFullText ' + (Z ? 'is set but the plugin is not started or its menus are not registered (started=' + (Z.plugin && Z.plugin.started) + ')' : 'is missing: zotero-fulltext did not load') };
      return null;
    }
    R.zft = { ready: true, id: zft.ID, version: (zft.info && zft.info.version) || '', rootURI: (zft.info && zft.info.rootURI) || '', readyMs: Date.now() - S.t0 };
    return zft;
  }

  function applyPrefs(ZFT, R) {
    const applied = {};
    for (const key of Object.keys(INST_OFF)) {
      ZFT.config.setPref(key, INST_OFF[key]);
      applied[key] = ZFT.config.getPref(key);
    }
    R.prefsApplied = applied;
    const config = ZFT.config.buildEngineConfig();
    R.engineInst = JSON.parse(JSON.stringify(config.inst));
    R.engineOa = JSON.parse(JSON.stringify(config.oa));
    R.engineRun = JSON.parse(JSON.stringify(config.run));
  }

  function collectMenuInfo(ZFT) {
    const menu = { keys: ZFT.menu.registeredKeys(), options: [], optionsError: null };
    try {
      for (const target of ['main/library/item', 'main/menubar/tools']) {
        for (const o of Zotero.MenuManager._menuManager.getCustomMenuOptions(target)) {
          if (o.pluginID !== ZFT.ID) continue;
          menu.options.push({ menuID: o.menuID, pluginID: o.pluginID, target: o.target, entries: (o.menus || []).map((m) => ({ type: m.menuType, l10nID: m.l10nID || null })) });
        }
      }
    } catch (e) { menu.optionsError = errText(e); }
    return menu;
  }

  function collectPaneInfo(ZFT) {
    const panes = [];
    try {
      for (const p of Zotero.PreferencePanes.pluginPanes || []) {
        if (p.pluginID === ZFT.ID) panes.push({ id: p.id, label: p.label || null, src: p.src || null });
      }
    } catch (e) { /* listed stays false */ }
    return { paneID: ZFT.plugin.paneID || null, listed: panes.some((p) => p.id === ZFT.plugin.paneID), panes };
  }

  // ------------------------------------------------------------------ items

  function trySet(item, field, value) {
    try { item.setField(field, value); } catch (e) { plog(`setField ${field} failed: ${errText(e)}`); }
  }

  async function createItems(R, cases) {
    const libraryID = Zotero.Libraries.userLibraryID;
    const created = [];
    for (const c of cases) {
      const item = new Zotero.Item(c.itemType);
      item.libraryID = libraryID;
      item.setField('title', c.title);
      item.setField('DOI', c.doi);
      if (c.publicationTitle) trySet(item, 'publicationTitle', c.publicationTitle);
      if (c.repository) trySet(item, 'repository', c.repository);
      if (c.date) trySet(item, 'date', c.date);
      item.addTag('zotero-fulltext-e2e');
      await item.saveTx();
      created.push({ c, item });
      R.items.push({ key: c.key, label: c.label, doi: c.doi, itemType: c.itemType, itemID: item.id, zoteroKey: item.key, expect: c.expect, soft: c.soft, outcome: null, trace: [], attachments: [] });
    }
    return created;
  }

  // ------------------------------------------------------------------ invoking the command

  function findMenuitem(root, l10nID) {
    return root.querySelector(`menuitem.zotero-custom-menu-item[data-l10n-id="${l10nID}"]`);
  }

  /** Wraps ZFT.runs.startRun (the menu looks it up at call time) to capture the arguments and the RunHandle. */
  function installRunsSpy(ZFT) {
    const original = ZFT.runs.startRun;
    const spy = { calls: [], handle: null, error: null };
    ZFT.runs.startRun = function (opts) {
      const call = { mode: opts && opts.mode, interactive: opts && opts.interactive, items: Array.from((opts && opts.items) || []).map((i) => i && i.id) };
      spy.calls.push(call);
      const p = original.apply(this, arguments);
      Promise.resolve(p).then(
        (h) => { call.run = h && h.run; if (!spy.handle) spy.handle = h; },
        (e) => { call.error = errText(e); if (!spy.error) spy.error = errText(e); },
      );
      return p;
    };
    spy.restore = () => { ZFT.runs.startRun = original; };
    return spy;
  }

  /** @param {() => void} beforeFire called right before the command fires (the event-loop heartbeat starts there) */
  async function invokeCommand(ZFT, items, mode, beforeFire) {
    const ids = items.map((i) => i.id);
    const l10nID = mode === 'oa_only' ? 'zotero-fulltext-menu-get-oa' : 'zotero-fulltext-menu-get';
    const inv = { via: null, note: '', l10nID, mode, selectedIDs: [], startRunCalls: [] };
    try {
      let visible = null;
      ZFT.menu.onShowing({}, { items, setVisible(v) { visible = v; } });
      inv.onShowingVisible = visible;
    } catch (e) { inv.onShowingVisible = 'threw: ' + errText(e); }

    // 1. the real item context menu of the main window
    try {
      const win = Zotero.getMainWindow();
      const ZP = win && win.ZoteroPane;
      if (!ZP || !ZP.itemsView) throw new Error('no main window / ZoteroPane.itemsView');
      await ZP.itemsView.waitForLoad();
      await ZP.selectItems(ids);
      const settled = await pollFor(() => sameSet(ZP.getSelectedItems(true), ids), 15000);
      inv.selectedIDs = ZP.getSelectedItems(true);
      if (!settled) throw new Error('the items tree selection did not settle on ' + JSON.stringify(ids) + ', has ' + JSON.stringify(inv.selectedIDs));
      await ZP.buildItemContextMenu();
      const popup = win.document.getElementById('zotero-itemmenu');
      if (!popup) throw new Error('#zotero-itemmenu not found');
      let el = findMenuitem(popup, l10nID);
      if (!el) {
        // Zotero folds plugin entries into a "more" submenu when the popup would be too tall; it fills on popupshowing.
        for (const sub of popup.querySelectorAll('menu.zotero-custom-menu-item > menupopup')) {
          sub.dispatchEvent(new win.Event('popupshowing'));
          el = findMenuitem(sub, l10nID);
          if (el) { inv.grouped = true; break; }
        }
      }
      if (!el) throw new Error(`menuitem ${l10nID} not found in the item context menu`);
      inv.menuitemHidden = !!el.hidden;
      inv.menuitemDisabled = !!el.disabled;
      beforeFire();
      el.doCommand();
      inv.via = 'menu-ui';
      return inv;
    } catch (e) {
      inv.note = 'UI route failed: ' + errText(e);
      plog(inv.note);
    }

    // 2. the onCommand that was registered with MenuManager
    try {
      const opt = Zotero.MenuManager._menuManager.getCustomMenuOptions('main/library/item').find((o) => o.pluginID === ZFT.ID);
      const menu = opt && opt.menus.find((m) => m.l10nID === l10nID);
      if (!menu || typeof menu.onCommand !== 'function') throw new Error('registered entry ' + l10nID + ' not found');
      beforeFire();
      menu.onCommand({}, { items });
      inv.via = 'registered-onCommand';
      return inv;
    } catch (e) {
      inv.note += ' | registered onCommand failed: ' + errText(e);
    }

    // 3. the command factory itself
    try {
      beforeFire();
      ZFT.menu.commandFor(mode)({}, { items });
      inv.via = 'ZFT.menu.commandFor';
    } catch (e) {
      inv.note += ' | ZFT.menu.commandFor failed: ' + errText(e);
    }
    return inv;
  }

  // ------------------------------------------------------------------ inspecting the outcome

  async function inspectAttachments(parentItem) {
    const ids = await Zotero.DB.columnQueryAsync(
      'SELECT itemID FROM itemAttachments WHERE parentItemID=? AND itemID NOT IN (SELECT itemID FROM deletedItems)', [parentItem.id]);
    const out = [];
    for (const id of ids) {
      const att = await Zotero.Items.getAsync(id);
      const rec = {
        id, key: att.key, title: att.getField('title'), contentType: att.attachmentContentType, linkMode: att.attachmentLinkMode,
        filename: att.attachmentFilename, url: att.getField('url'), path: null, size: 0, head: '', pdfMagic: false, eof: false, valid: false, error: null,
      };
      try {
        const path = await att.getFilePathAsync();
        if (!path) throw new Error('attachment file does not exist');
        rec.path = path;
        const stat = await IOUtils.stat(path);
        const head = await IOUtils.read(path, { maxBytes: 16 });
        const tail = await IOUtils.read(path, { offset: Math.max(0, stat.size - 1024) });
        Object.assign(rec, pdfFacts(head, tail, stat.size));
      } catch (e) { rec.error = errText(e); }
      rec.valid = rec.contentType === 'application/pdf' && rec.pdfMagic && rec.size >= MIN_PDF_BYTES;
      out.push(rec);
    }
    return out;
  }

  /** What the progress window actually shows once the run is over (rendered text, translated labels, buttons). */
  async function snapshotProgressWindow(titles) {
    await sleep(800); // rendering is batched with requestAnimationFrame and Fluent translates asynchronously
    let w = null;
    try {
      for (const win of Services.wm.getEnumerator(null)) {
        try { if (win.document.documentElement.getAttribute('windowtype') === PROGRESS_WINDOW_TYPE) { w = win; break; } } catch (e) { /* skip */ }
      }
    } catch (e) { /* enumerator unavailable */ }
    if (!w) return { found: false };
    const d = w.document;
    const text = (id) => { const e = d.getElementById(id); return e ? String(e.textContent || '').replace(/\s+/g, ' ').trim() : ''; };
    const rows = text('zft-progress-rows');
    const l10n = [];
    for (const e of d.querySelectorAll('[data-l10n-id]')) {
      l10n.push({ id: e.getAttribute('data-l10n-id'), text: String(e.textContent || '').replace(/\s+/g, ' ').trim(), hidden: !!e.hidden });
    }
    const all = (rows + ' ' + text('zft-progress-header')).trim();
    return {
      found: true, title: String(d.title || ''), header: text('zft-progress-header'), rows: clip(rows, 3000),
      buttons: Array.from(d.querySelectorAll('button')).filter((b) => !b.hidden).map((b) => String(b.textContent || '').trim()),
      l10nCount: l10n.length,
      untranslated: l10n.filter((x) => !x.hidden && !x.text).map((x) => x.id).slice(0, 20),
      rawIds: (all.match(/zotero-fulltext-[a-z0-9-]+/g) || []).slice(0, 20),
      missingTitles: titles.filter((t) => !all.includes(String(t).slice(0, 40))),
    };
  }

  async function collectDebug(R) {
    let text = '';
    try { text = String((await Zotero.Debug.get()) || ''); } catch (e) { R.debug.error = errText(e); }
    const base = resultPath().replace(/\.json$/i, '');
    if (base) {
      R.debug.file = base + '.zotero-debug.txt';
      try { await IOUtils.writeUTF8(R.debug.file, text); } catch (e) { R.debug.error = errText(e); }
    }
    const lines = text.split('\n');
    const ours = lines.filter((l) => l.includes('[zotero-fulltext]'));
    R.debug.totalLines = lines.length;
    R.debug.errorsOurs = ours.filter((l) => /^\(1\)/.test(l)).slice(-50).map((l) => clip(l, 400));
    R.debug.warningsOursCount = ours.filter((l) => /^\(2\)/.test(l)).length;
    R.debug.tailOurs = ours.slice(-150).map((l) => clip(l, 400));
    R.debug.tail = lines.filter((l) => l.trim()).slice(-100).map((l) => clip(l, 400));
  }

  async function runSmoke(ZFT, R) {
    const names = SMOKE_DEFAULT.filter((n) => ZFT.harness && ZFT.harness.CHECKS && ZFT.harness.CHECKS[n]);
    const out = {};
    for (const name of names) {
      try {
        out[name] = { ok: true, detail: await ZFT.harness.CHECKS[name]() };
      } catch (e) {
        out[name] = { ok: false, detail: errText(e) };
      }
    }
    R.smoke = out;
  }

  // ------------------------------------------------------------------ main

  function newResult() {
    return {
      schema: SCHEMA,
      meta: {}, phases: {}, zft: null, prefsApplied: null, engineInst: null, engineOa: null, engineRun: null,
      menu: null, prefsPane: null, invoke: null, items: [], run: { timedOut: false, note: '', elapsedMs: 0, report: null },
      eventLoop: { samples: 0, maxMs: 0, atMs: 0, over250: 0 },
      windows: { atStart: [], opened: [], atEnd: [], loginOpened: false },
      prompts: [], promptSpy: null, progressUI: null, consoleErrors: [], consoleWarnings: 0, debug: {}, smoke: null,
      fatal: null, driverLog: [],
    };
  }

  async function main(R) {
    const timeoutSec = prefInt(PREF + 'timeoutSec', DEFAULT_RUN_TIMEOUT_SEC);
    const mode = prefStr(PREF + 'mode', 'full') === 'oa_only' ? 'oa_only' : 'full';
    const cases = parseCaseFilter(prefStr(PREF + 'cases', ''));
    Object.assign(R.meta, {
      driver: PLUGIN_ID, startedAt: new Date(S.t0).toISOString(), mode, timeoutSec, cases: cases.map((c) => c.key), smoke: prefBool(PREF + 'smoke', true),
      network: 'live (OA sources only, no institution, no login)',
    });
    await Zotero.initializationPromise;
    try {
      R.meta.zotero = Zotero.version;
      R.meta.platform = Services.appinfo.platformVersion;
      R.meta.headless = Services.env.get('MOZ_HEADLESS') === '1';
      R.meta.dataDir = Zotero.DataDirectory && Zotero.DataDirectory.dir;
      R.meta.profileDir = PathUtils.profileDir;
    } catch (e) { R.meta.metaError = errText(e); }
    mark('zotero_initialized');

    const ui = await withTimeout(Zotero.uiReadyPromise, UI_READY_WAIT_MS);
    if (ui.timedOut) plog('uiReadyPromise not resolved in ' + UI_READY_WAIT_MS + ' ms; continuing');
    mark('ui_ready');
    // uiReadyPromise resolves when the (still empty) items tree has loaded, before the collection tree has selected a row.
    // Until collectionsView.itemTreeView is set, ZoteroPane.selectItems throws and adding items races the tree's own loading.
    const paneReady = await pollFor(() => {
      const w = Zotero.getMainWindow();
      const cv = w && w.ZoteroPane && w.ZoteroPane.collectionsView;
      return !!(cv && cv.itemTreeView && cv.selection && cv.selection.count > 0);
    }, UI_READY_WAIT_MS, 50);
    if (!paneReady) plog('main pane (collection row selected, items tree attached) not ready in ' + UI_READY_WAIT_MS + ' ms; continuing');
    mark('pane_ready');
    R.windows.atStart = listWindows();

    const ZFT = await waitForZft(R);
    mark('zft_ready');
    if (!ZFT) return;

    applyPrefs(ZFT, R);
    R.menu = collectMenuInfo(ZFT);
    R.prefsPane = collectPaneInfo(ZFT);
    mark('prefs_applied');

    const created = await createItems(R, cases);
    mark('items_created');

    S.watchWindows = true;
    const restorePrompts = installPromptSpy(R);
    const spy = installRunsSpy(ZFT);
    let stopHeartbeat = () => {};
    let firedAt = 0;
    let outcome;
    try {
      R.invoke = await invokeCommand(ZFT, created.map((x) => x.item), mode, () => {
        if (firedAt) return;
        firedAt = Date.now();
        stopHeartbeat = startHeartbeat(R);
      });
      mark('command_invoked');
      if (!R.invoke.via) { R.run.note = 'no route could invoke the command'; return; }

      const got = await pollFor(() => spy.handle || spy.error, HANDLE_WAIT_MS, 100);
      R.invoke.startRunCalls = spy.calls;
      if (!got || !spy.handle) { R.run.note = spy.error ? 'startRun rejected: ' + spy.error : 'startRun returned no handle within ' + HANDLE_WAIT_MS + ' ms'; return; }
      const handle = spy.handle;
      mark('run_started');

      outcome = await withTimeout(handle.done, timeoutSec * 1000);
      if (outcome.timedOut) {
        R.run.timedOut = true;
        plog('run timed out; cancelling');
        try { handle.cancel(); } catch (e) { plog('cancel failed: ' + errText(e)); }
        outcome = await withTimeout(handle.done, CANCEL_GRACE_MS);
      }
      R.run.report = outcome.value || ZFT.runs.report(handle.run) || null;
      R.run.runID = handle.run;
      R.run.elapsedMs = Date.now() - firedAt;
      mark('run_finished');
    } finally {
      stopHeartbeat();
      R.invoke = R.invoke || { via: null, note: 'invoke threw', startRunCalls: spy.calls };
      R.invoke.startRunCalls = spy.calls;
      spy.restore();
      restorePrompts();
      S.watchWindows = false;
    }

    try { R.progressUI = await snapshotProgressWindow(created.map((x) => x.c.title)); } catch (e) { R.progressUI = { found: false, error: errText(e) }; }

    // per-item outcome and what really landed in the library
    for (const rec of R.items) {
      const entry = created.find((x) => x.item.id === rec.itemID);
      const reported = R.run.report && R.run.report.items && R.run.report.items.find((i) => i.item === rec.itemID);
      if (reported) { rec.outcome = reported.outcome; rec.trace = (reported.trace || []).slice(0, 60); }
      try { rec.attachments = await inspectAttachments(entry.item); } catch (e) { rec.attachmentsError = errText(e); }
    }
    mark('inspected');

    if (prefBool(PREF + 'smoke', true)) {
      await runSmoke(ZFT, R);
      mark('smoke_done');
    }
  }

  async function finish(R, why) {
    if (S.finished) return;
    S.finished = true;
    try { clearTimeout(S.watchdog); } catch (e) { /* ignore */ }
    S.watchWindows = false;
    for (const off of S.cleanups) { try { off(); } catch (e) { /* ignore */ } }
    S.cleanups = [];
    R.windows.atEnd = listWindows();
    try { await collectDebug(R); } catch (e) { R.debug.error = errText(e); }
    R.meta.finishedBy = why;
    R.meta.elapsedMs = Date.now() - S.t0;
    const ev = evaluate(R);
    const out = Object.assign({ schema: R.schema, verdict: ev.verdict, counts: ev.counts, checks: ev.checks }, R);
    const path = resultPath();
    if (!path) {
      plog('no result path (pref ' + PREF + 'resultFile, ' + HARNESS_RESULT_PREF + ' or env ZOTERO_HARNESS_RESULT_FILE): nothing written');
    } else {
      try {
        await IOUtils.writeUTF8(path, JSON.stringify(out, null, 2), { tmpPath: path + '.tmp' });
        plog('result written to ' + path + ' verdict ' + ev.verdict);
      } catch (e) { plog('writing the result failed: ' + errText(e)); }
    }
    if (prefBool(QUIT_PREF, true)) {
      // Let the items tree finish rendering the new attachments' state: quitting while that async work is in flight makes Zotero
      // itself log "Database permanently closed" / NS_ERROR_NOT_INITIALIZED (renderHasAttachmentCell) during shutdown.
      await sleep(QUIT_SETTLE_MS);
      try {
        Services.startup.quit(CI().nsIAppStartup.eAttemptQuit);
      } catch (e) { plog('quit failed: ' + errText(e)); }
    }
  }

  function start() {
    if (S) return;
    S = { t0: Date.now(), R: newResult(), finished: false, watchWindows: false, cleanups: [], watchdog: null };
    const R = S.R;
    const timeoutSec = prefInt(PREF + 'timeoutSec', DEFAULT_RUN_TIMEOUT_SEC);
    S.watchdog = setTimeout(() => {
      R.fatal = R.fatal || 'watchdog: the driver did not finish within ' + (timeoutSec + WATCHDOG_EXTRA_MS / 1000) + ' s';
      finish(R, 'watchdog');
    }, timeoutSec * 1000 + WATCHDOG_EXTRA_MS);
    // Detached: bootstrap startup() must return at once, Zotero awaits it during plugin loading.
    (async () => {
      let why = 'done';
      try {
        S.cleanups.push(installConsole(R));
        S.cleanups.push(installWindowWatch(R));
        await main(R);
      } catch (e) {
        R.fatal = errText(e) + (e && e.stack ? ' | ' + String(e.stack).split('\n').slice(0, 4).join(' | ') : '');
        why = 'exception';
      }
      await finish(R, why);
    })();
  }

  function stop() {
    if (!S) return;
    try { clearTimeout(S.watchdog); } catch (e) { /* ignore */ }
    for (const off of S.cleanups) { try { off(); } catch (e) { /* ignore */ } }
    S.cleanups = [];
    S = null;
  }

  return { start, stop, pure: { evaluate, pdfFacts, parseCaseFilter, outcomeSummary, CASES, INST_OFF, SCHEMA } };
})();

function install() {}
function uninstall() {}
function startup() { E2E.start(); }
function shutdown() { E2E.stop(); }
function onMainWindowLoad() {}
function onMainWindowUnload() {}

if (typeof module !== 'undefined' && module.exports) module.exports = E2E.pure;
