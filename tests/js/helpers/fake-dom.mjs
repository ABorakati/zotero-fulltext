// A tiny fake DOM + window for running the plugin's WINDOW scripts (progress-window.js, login.js) under Node.
// Owner: js-ui (additive helper; nothing else imports it). No npm dependencies, no real layout: just enough tree,
// attributes, events and Fluent (`document.l10n.setAttributes`) to assert what the scripts render.
//
//   const w = loadWindowScript('progress-window.js', { xhtml: 'progress.xhtml', args });
//   w.fire('load');                       // window 'load' listeners
//   w.byId('zft-progress-cancel').click();
//   w.flushFrames();                      // requestAnimationFrame callbacks
//
// The vm context global IS the window (window === globalThis), like a real chrome window.
import vm from 'node:vm';
import fs from 'node:fs';
import path from 'node:path';
import { CONTENT } from './zft-env.mjs';

export class FakeElement {
  constructor(doc, tag, ns) {
    this.ownerDocument = doc;
    this.tagName = String(tag);
    this.namespaceURI = ns || null;
    this.attributes = new Map();
    this.children = [];
    this.parentNode = null;
    this.listeners = new Map();
    this.style = {};
    this.hidden = false;
    this.disabled = false;
    this._text = '';
    doc.stats.created += 1;
  }
  get id() { return this.attributes.get('id') ?? ''; }
  setAttribute(k, v) {
    this.attributes.set(k, String(v));
    if (k === 'hidden') this.hidden = true;
  }
  getAttribute(k) { return this.attributes.has(k) ? this.attributes.get(k) : null; }
  hasAttribute(k) { return this.attributes.has(k); }
  removeAttribute(k) {
    this.attributes.delete(k);
    if (k === 'hidden') this.hidden = false;
  }
  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.children.push(child);
    this.ownerDocument.stats.appends += 1;
    return child;
  }
  removeChild(child) {
    const i = this.children.indexOf(child);
    if (i >= 0) this.children.splice(i, 1);
    child.parentNode = null;
    return child;
  }
  remove() { if (this.parentNode) this.parentNode.removeChild(this); }
  get textContent() {
    return this.children.length ? this.children.map((c) => c.textContent).join('') : this._text;
  }
  set textContent(v) {
    for (const c of this.children) c.parentNode = null;
    this.children = [];
    this._text = String(v);
    this.ownerDocument.stats.textSets += 1;
  }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type).add(fn);
    this.ownerDocument.stats.listeners += 1;
  }
  removeEventListener(type, fn) {
    if (this.listeners.get(type)?.delete(fn)) this.ownerDocument.stats.listeners -= 1;
  }
  fire(type, ev = {}) {
    for (const fn of [...(this.listeners.get(type) || [])]) fn({ type, target: this, ...ev });
  }
  click() { if (!this.disabled) this.fire('click'); }
  /** XUL <browser>.loadURI stand-in: records the loaded spec. */
  loadURI(uri) { (this.loaded ||= []).push(uri.spec); }
  /** All descendants (depth first), optionally filtered. */
  find(pred = () => true) {
    const out = [];
    const walk = (n) => { for (const c of n.children) { if (pred(c)) out.push(c); walk(c); } };
    walk(this);
    return out;
  }
  /** Fluent message currently attached ('' when none). */
  get l10nId() { return this.attributes.get('data-l10n-id') ?? ''; }
  get l10nArgs() { return this.attributes.has('data-l10n-args') ? JSON.parse(this.attributes.get('data-l10n-args')) : null; }
}

export class FakeDocument {
  constructor() {
    this.stats = { created: 0, appends: 0, textSets: 0, listeners: 0, l10nSets: 0 };
    this.title = '';
    this.readyState = 'complete';
    this.documentURI = '';
    this.root = new FakeElement(this, 'window');
    this.l10n = {
      setAttributes: (el, id, args) => {
        this.stats.l10nSets += 1;
        el.setAttribute('data-l10n-id', id);
        if (args) el.setAttribute('data-l10n-args', JSON.stringify(args));
        else el.removeAttribute('data-l10n-args');
      },
      formatValue: async (id) => `[${id}]`,
      ready: Promise.resolve(),
    };
  }
  createElement(tag) { return new FakeElement(this, tag, 'xul'); }
  createElementNS(ns, tag) { return new FakeElement(this, tag, ns); }
  createXULElement(tag) { return new FakeElement(this, tag, 'xul'); }
  getElementById(id) {
    let hit = null;
    const walk = (n) => { for (const c of n.children) { if (!hit && c.id === id) hit = c; if (!hit) walk(c); } };
    walk(this.root);
    return hit;
  }
}

/** Populate a document with one element per id="..." in an XHTML file (flat, tag + hidden + data-l10n-id kept). */
export function populateFromXhtml(doc, xhtmlText) {
  const markup = xhtmlText.replace(/<!--[\s\S]*?-->/g, '');
  for (const m of markup.matchAll(/<([A-Za-z:]+)\s([^>]*?)>/g)) {
    const attrs = [...m[2].matchAll(/([A-Za-z:-]+)="([^"]*)"/g)];
    const id = attrs.find((a) => a[1] === 'id');
    if (!id) continue;
    const el = new FakeElement(doc, m[1].replace(/^html:/, ''), 'xhtml');
    for (const [, k, v] of attrs) el.setAttribute(k, v);
    if (el.getAttribute('hidden') === 'false') el.hidden = false;
    doc.root.appendChild(el);
  }
  doc.stats.created = 0; doc.stats.appends = 0; doc.stats.textSets = 0; doc.stats.l10nSets = 0;
}

/**
 * Run a window script in a fake chrome window.
 * @param {string} script file under plugin/content/
 * @param {{xhtml?: string, args?: object, globals?: object, documentURI?: string}} [opts]
 */
export function loadWindowScript(script, opts = {}) {
  const doc = new FakeDocument();
  const xhtmlPath = opts.xhtml ? path.join(CONTENT, opts.xhtml) : null;
  if (xhtmlPath) populateFromXhtml(doc, fs.readFileSync(xhtmlPath, 'utf8'));
  doc.documentURI = opts.documentURI || '';
  const listeners = new Map();
  const frames = [];
  const timeouts = [];
  const intervals = new Map();
  let nextTimer = 1;
  const copied = [];
  const sandbox = {
    document: doc,
    arguments: opts.args ? [opts.args] : [],
    closed: false,
    copied,
    reported: [],
    Components: {
      classes: { '@mozilla.org/widget/clipboardhelper;1': { getService: () => ({ copyString: (t) => copied.push(t) }) } },
      interfaces: { nsIClipboardHelper: {}, nsIWebProgressListener: { STATE_IS_SECURE: 0x10000 } },
      utils: { reportError: (e) => sandbox.reported.push(String(e)) },
    },
    ChromeUtils: { generateQI: (list) => () => list },
    Services: { scriptSecurityManager: { getSystemPrincipal: () => 'SYSTEM' } },
    ...(opts.globals || {}),
  };
  sandbox.window = sandbox;
  sandbox.addEventListener = (type, fn) => {
    if (!listeners.has(type)) listeners.set(type, []);
    listeners.get(type).push(fn);
  };
  sandbox.close = () => {
    if (sandbox.closed) return;
    sandbox.closed = true;
    sandbox.fire('unload');
  };
  sandbox.requestAnimationFrame = (fn) => { frames.push(fn); return frames.length; };
  sandbox.cancelAnimationFrame = (id) => { if (frames[id - 1]) frames[id - 1] = null; };
  sandbox.setInterval = (fn, ms) => { const id = nextTimer++; intervals.set(id, { fn, ms }); return id; };
  sandbox.clearInterval = (id) => { intervals.delete(id); };
  sandbox.setTimeout = (fn, ms) => { const id = nextTimer++; timeouts.push({ id, fn, ms }); return id; };
  sandbox.clearTimeout = (id) => { const i = timeouts.findIndex((t) => t.id === id); if (i >= 0) timeouts.splice(i, 1); };
  sandbox.fire = (type) => { for (const fn of [...(listeners.get(type) || [])]) fn({ type }); };
  sandbox.flushFrames = () => {
    const run = frames.splice(0).filter(Boolean);
    for (const fn of run) fn();
    return run.length;
  };
  sandbox.pendingFrames = () => frames.filter(Boolean).length;
  sandbox.tickIntervals = () => { for (const { fn } of [...intervals.values()]) fn(); };
  sandbox.intervalCount = () => intervals.size;
  sandbox.listenerCount = (type) => (listeners.get(type) || []).length;
  sandbox.byId = (id) => doc.getElementById(id);

  vm.createContext(sandbox);
  const file = path.join(CONTENT, script);
  new vm.Script(fs.readFileSync(file, 'utf8'), { filename: file }).runInContext(sandbox);
  return sandbox;
}
