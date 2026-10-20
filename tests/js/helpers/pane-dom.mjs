// A tiny fake DOM for the preferences-pane tests (no jsdom). Owner: js-shell.
// (Not fake-dom.mjs: that name belongs to the js-ui window-script helper.)
// Supports exactly what plugin/content/prefs-pane.js uses: createElementNS/createElement, setAttribute/getAttribute,
// appendChild/removeChild/firstChild, add/removeEventListener, value/checked/type/hidden/textContent, getElementById and a
// minimal querySelector ("tag[attr=\"v\"][attr2=\"v2\"]", "#id", ".class"). Events are dispatched with fire(), which returns a
// promise of every handler's return value so tests can await async handlers.

export class PaneEl {
  constructor(tag, doc) {
    this.tagName = String(tag).toUpperCase();
    this.localName = String(tag).toLowerCase();
    this.ownerDocument = doc;
    this.attrs = new Map();
    this.children = [];
    this.parentNode = null;
    this.listeners = new Map();
    this.hidden = false;
    this.checked = false;
    this.type = this.localName === 'input' ? 'text' : '';
    this._value = '';
    this._text = '';
  }
  get id() { return this.attrs.get('id') || ''; }
  get firstChild() { return this.children[0] || null; }
  get childElementCount() { return this.children.length; }
  get value() {
    if (this.localName === 'select' && this._value === '') {
      const first = this.children[0];
      return first ? first.attrs.get('value') || '' : '';
    }
    return this._value;
  }
  set value(v) { this._value = String(v); }
  get textContent() { return this._text; }
  set textContent(t) { this._text = String(t); this.children = []; }
  setAttribute(k, v) {
    this.attrs.set(k, String(v));
    if (k === 'type') this.type = String(v);
  }
  getAttribute(k) { return this.attrs.has(k) ? this.attrs.get(k) : null; }
  appendChild(c) { c.parentNode = this; this.children.push(c); return c; }
  removeChild(c) {
    const i = this.children.indexOf(c);
    if (i >= 0) this.children.splice(i, 1);
    c.parentNode = null;
    return c;
  }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type).add(fn);
  }
  removeEventListener(type, fn) {
    const set = this.listeners.get(type);
    if (set) set.delete(fn);
  }
  listenerCount() {
    let n = 0;
    for (const set of this.listeners.values()) n += set.size;
    return n;
  }
  /** DOM-style dispatch (fire-and-forget); fire() is the awaitable variant. */
  dispatchEvent(ev) { this.fire(ev.type, ev); return true; }
  /** Dispatch an event to this element's handlers; resolves with all handler results. */
  fire(type, ev = {}) {
    const results = [...(this.listeners.get(type) || [])].map((fn) => fn({ type, target: this, ...ev }));
    return Promise.all(results);
  }
  *walk() {
    yield this;
    for (const c of this.children) yield* c.walk();
  }
  matches(sel) {
    const parts = sel.match(/^([a-z0-9]*)((?:[#.][\w-]+|\[[^\]]+\])*)$/i);
    if (!parts) return false;
    if (parts[1] && parts[1].toLowerCase() !== this.localName) return false;
    for (const m of parts[2].matchAll(/#([\w-]+)|\.([\w-]+)|\[([\w-]+)(?:(\^?)="([^"]*)")?\]/g)) {
      if (m[1] && this.id !== m[1]) return false;
      if (m[2] && !(this.attrs.get('class') || '').split(/\s+/).includes(m[2])) return false;
      if (m[3]) {
        if (!this.attrs.has(m[3])) return false;
        const have = this.attrs.get(m[3]);
        if (m[5] !== undefined && (m[4] ? !have.startsWith(m[5]) : have !== m[5])) return false;
      }
    }
    return true;
  }
  querySelectorAll(sel) { return [...this.walk()].filter((e) => e !== this && e.matches(sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
}

/** The mount points prefs.xhtml provides (ids that prefs-pane.js looks up). */
export const MOUNT_IDS = [
  'zft-prefs-problems', 'zft-prefs-root', 'zft-prefs-reset-institution', 'zft-prefs-clear-state', 'zft-prefs-open-data',
  'zft-prefs-status', 'zft-prefs-privacy',
];

/**
 * @param {{l10n?: boolean}} [opts] l10n: give the document a recording `document.l10n` (default true)
 * @returns {{document: object, body: PaneEl, byId: (id: string) => PaneEl|null, l10nCalls: Array}}
 */
export function makePaneDocument(opts = {}) {
  const l10nCalls = [];
  const doc = {};
  const body = new PaneEl('body', doc);
  doc.body = body;
  doc.createElementNS = (ns, tag) => new PaneEl(tag, doc);
  doc.createElement = (tag) => new PaneEl(tag, doc);
  doc.getElementById = (id) => [...body.walk()].find((e) => e.id === id) || null;
  doc.querySelector = (sel) => body.querySelector(sel);
  if (opts.l10n !== false) {
    doc.l10n = {
      setAttributes(node, id, args) {
        l10nCalls.push([id, args]);
        node.setAttribute('data-l10n-id', id);
        node._text = 'text:' + id;   // what Fluent would put there (without touching children such as <option>)
      },
      translateFragment: async (root) => { l10nCalls.push(['translateFragment', root.id]); },
      addResourceIds: (ids) => { l10nCalls.push(['addResourceIds', ids]); },
      removeResourceIds: (ids) => { l10nCalls.push(['removeResourceIds', ids]); },
    };
  }
  for (const id of MOUNT_IDS) {
    const tag = id.endsWith('root') || id.endsWith('problems') ? 'div' : id.includes('privacy') || id.includes('status') ? 'p' : 'button';
    const e = new PaneEl(tag, doc);
    e.setAttribute('id', id);
    // prefs.xhtml ships the problem list and the status line hidden.
    if (id === 'zft-prefs-problems' || id === 'zft-prefs-status') e.hidden = true;
    body.appendChild(e);
  }
  return { document: doc, body, byId: doc.getElementById, l10nCalls };
}

/** A fake `window` with add/removeEventListener (for the unload hook). */
export function makePaneWindow() {
  const listeners = new Map();
  return {
    listeners,
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
    },
    removeEventListener(type, fn) {
      const set = listeners.get(type);
      if (set) set.delete(fn);
    },
    listenerCount() {
      let n = 0;
      for (const set of listeners.values()) n += set.size;
      return n;
    },
    fire(type) { return Promise.all([...(listeners.get(type) || [])].map((fn) => fn({ type }))); },
  };
}
