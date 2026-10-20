// Fakes for the Zotero/Gecko surface the plugin touches. Owner: js-shell; each work package may ADD fakes
// (append-only, keep existing signatures). Everything is synchronous/in-memory and deterministic.
import { readPrefsJs } from './zft-env.mjs';

/** In-memory IOUtils/PathUtils pair. Paths use '/'. */
export function makeFakeFs() {
  const files = new Map();   // path -> Uint8Array
  const dirs = new Set(['/']);
  const norm = (p) => String(p).replace(/\\/g, '/').replace(/\/+/g, '/');
  const parent = (p) => norm(p).replace(/\/[^/]*$/, '') || '/';
  const ioutils = {
    async makeDirectory(p) { dirs.add(norm(p)); },
    async exists(p) { return files.has(norm(p)) || dirs.has(norm(p)); },
    async write(p, bytes, opts = {}) {
      p = norm(p);
      dirs.add(parent(p));
      const add = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      if (opts.mode === 'append' && files.has(p)) {
        const old = files.get(p);
        const next = new Uint8Array(old.length + add.length);
        next.set(old); next.set(add, old.length);
        files.set(p, next);
      } else {
        files.set(p, add.slice());
      }
      return add.length;
    },
    async writeUTF8(p, text) { return this.write(p, new TextEncoder().encode(text)); },
    async writeJSON(p, obj) { return this.writeUTF8(p, JSON.stringify(obj)); },
    async read(p, opts = {}) {
      const b = files.get(norm(p));
      if (!b) throw Object.assign(new Error('NotFoundError: ' + p), { name: 'NotFoundError' });
      return opts.offset || opts.maxBytes != null ? b.slice(opts.offset || 0, (opts.offset || 0) + (opts.maxBytes ?? b.length)) : b.slice();
    },
    async readUTF8(p) { return new TextDecoder().decode(await this.read(p)); },
    async readJSON(p) { return JSON.parse(await this.readUTF8(p)); },
    async remove(p, opts = {}) {
      p = norm(p);
      files.delete(p);
      if (opts.recursive) {
        for (const k of [...files.keys()]) if (k.startsWith(p + '/')) files.delete(k);
        for (const d of [...dirs]) if (d === p || d.startsWith(p + '/')) dirs.delete(d);
      } else dirs.delete(p);
    },
    async move(a, b) { a = norm(a); b = norm(b); files.set(b, files.get(a)); files.delete(a); },
    async stat(p) {
      p = norm(p);
      if (files.has(p)) return { size: files.get(p).length, type: 'regular' };
      if (dirs.has(p)) return { size: 0, type: 'directory' };
      throw Object.assign(new Error('NotFoundError: ' + p), { name: 'NotFoundError' });
    },
    async getChildren(p) {
      p = norm(p);
      const out = new Set();
      for (const k of [...files.keys(), ...dirs]) if (k !== p && parent(k) === p) out.add(k);
      return [...out];
    },
  };
  const pathutils = {
    join: (...parts) => norm(parts.join('/')),
    filename: (p) => norm(p).split('/').pop(),
    parent,
  };
  return { files, dirs, IOUtils: ioutils, PathUtils: pathutils };
}

/** Deterministic timers: advance(ms) fires due callbacks in order. Use as deps.timers / ctx setTimeout. */
export function makeFakeTimers() {
  let now = 0;
  let seq = 1;
  const pending = new Map();
  return {
    get now() { return now; },
    setTimeout(fn, ms = 0) { const id = seq++; pending.set(id, { at: now + Math.max(0, ms), fn, id }); return id; },
    clearTimeout(id) { pending.delete(id); },
    /** Fire everything due within `ms` virtual milliseconds (callbacks may schedule more). */
    advance(ms) {
      const end = now + ms;
      for (;;) {
        const due = [...pending.values()].filter((t) => t.at <= end).sort((a, b) => a.at - b.at || a.id - b.id)[0];
        if (!due) break;
        pending.delete(due.id);
        now = Math.max(now, due.at);
        due.fn();
      }
      now = end;
    },
    pendingCount: () => pending.size,
  };
}

/**
 * Fake `Zotero` with prefs seeded from plugin/prefs.js. Extend per test: Object.assign(z, {...}).
 * Prefs.get(name, true) / set(name, value, true) mirror the real signature (global=true).
 */
export function makeFakeZotero(extra = {}) {
  const prefs = readPrefsJs();
  const debugLog = [];
  const zotero = {
    debugLog,
    debug(msg, level) { debugLog.push([level ?? 3, String(msg)]); },
    logError(e) { debugLog.push([1, 'ERROR ' + (e && e.message ? e.message : e)]); },
    warn(msg) { debugLog.push([2, String(msg)]); },
    Prefs: {
      store: prefs,
      get(name) { return prefs.has(name) ? prefs.get(name) : undefined; },
      set(name, value) { prefs.set(name, value); },
      clear(name) { prefs.delete(name); },
    },
    Profile: { dir: '/profile' },
    initializationPromise: Promise.resolve(),
    getMainWindows: () => [],
    getMainWindow: () => null,
    ...extra,
  };
  return zotero;
}

/** Minimal Services fake (prompt/ww/wm/obs/io recorders). */
export function makeFakeServices(extra = {}) {
  const calls = [];
  return {
    calls,
    prompt: { alert: (...a) => calls.push(['alert', a]), confirmEx: (...a) => { calls.push(['confirmEx', a]); return 0; } },
    obs: { notifyObservers: (...a) => calls.push(['notifyObservers', a]) },
    scriptloader: { loadSubScript: () => { throw new Error('loadSubScript not faked'); } },
    ...extra,
  };
}
