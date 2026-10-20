/* ZFT.state: persisted runtime state (<profile>/zotero-fulltext/state.json).
 *
 * Owner: js-runs. Holds what must survive a restart:
 *   persisted   the engine part {ledger, halts, ezproxy, alma} = proto.rs Persisted (opaque JSON to JS)
 *   tmp_dirs    every temp directory we created (swept on startup and shutdown)
 *   run_counter monotonic RunId source
 * File shape: {"v": 1, "persisted": {...}, "tmp_dirs": ["<abs path>", ...], "run_counter": 7}
 *
 * Write discipline: IOUtils.writeJSON(path, obj, {tmpPath: path + '.tmp'}) (atomic rename). Persist
 * effects with flush=false are debounced (FLUSH_DEBOUNCE_MS); flush=true writes immediately and the
 * returned promise resolves after the write. A corrupt file is renamed state.json.bad and replaced by
 * defaults. The licence ledger is reserve-then-commit inside the engine; JS only has to make sure a
 * flush=true Persist is on disk BEFORE any effect marked barrier=true starts (see effects.js).
 *
 * Details worth knowing:
 *  - The debounce timer is armed by the FIRST unsaved change and is not pushed back by later ones, so a
 *    change reaches the disk at most FLUSH_DEBOUNCE_MS after it was made, however busy the run is.
 *  - flush() never rejects. A failed write is logged and the state stays dirty: the next set()/flush()
 *    retries. Writes are serialised (a second flush waits for the first); a change that lands while a
 *    write is in flight keeps the state dirty and is written by the next flush.
 *  - Loading is defensive because the engine feeds this data straight back into Command::Configure and
 *    serde rejects the whole command on a wrong type: a file with a wrong top-level shape is moved to
 *    state.json.bad, and single malformed ledger/halt/cache entries are dropped (and logged) instead.
 *  - Nothing outside the registered list and <dataDir>/tmp is ever deleted.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  const FILE_NAME = 'state.json';
  const VERSION = 1;
  const FLUSH_DEBOUNCE_MS = 2000;
  /** proto.rs PersistKey values. */
  const PERSIST_KEYS = ['ledger', 'halts', 'ezproxy', 'alma'];
  const DATA_SUBDIR = 'zotero-fulltext';
  const TMP_SUBDIR = 'tmp';
  const MKDIR = { createAncestors: true, ignoreExisting: true };

  /** Loaded state of the current data dir, or null before init()/after dispose(). */
  let loaded = null;
  let initPromise = null;
  let initDir = null;
  /** Tail of the write chain; every link is a promise that never rejects. */
  let chain = Promise.resolve();

  const log = (level, msg) => ZFT.util.log(level, 'state: ' + msg);
  const errText = (e) => ZFT.util.errText(e);

  // ---------------------------------------------------------------- shape helpers
  function isObject(v) { return v !== null && typeof v === 'object' && !Array.isArray(v); }
  function isU64(v) { return Number.isSafeInteger(v) && v >= 0; }
  function clone(v) { return JSON.parse(JSON.stringify(v)); }
  const samePath = (a) => String(a).replace(/\\/g, '/');

  function defaultFor(key) {
    if (key === 'ledger') return { committed: [], reserved: [] };
    if (key === 'halts') return { inst_until: 0, reason: null, hosts: {} };
    return {};
  }
  function defaultData() {
    const persisted = {};
    for (const key of PERSIST_KEYS) persisted[key] = defaultFor(key);
    return { persisted, tmp_dirs: [], run_counter: 0 };
  }

  /** Keep the entries of a map/list that satisfy ok(); log how many were dropped. */
  function keepValid(list, ok, what) {
    const kept = list.filter(ok);
    if (kept.length !== list.length) log('warn', `dropped ${list.length - kept.length} malformed ${what} entries`);
    return kept;
  }

  /** @returns {object|null} a cleaned deep copy of persisted[key], or null when the value cannot be used at all */
  function normalizeKey(key, value) {
    if (!isObject(value)) return null;
    const out = clone(value);
    if (key === 'ledger') {
      if (out.committed === undefined) out.committed = [];
      if (out.reserved === undefined) out.reserved = [];
      if (!Array.isArray(out.committed) || !Array.isArray(out.reserved)) return null;
      out.committed = keepValid(out.committed, (e) => Array.isArray(e) && e.length === 2 && isU64(e[0]) && isU64(e[1]), 'ledger');
      out.reserved = keepValid(out.reserved, (e) => isObject(e) && isU64(e.id) && isU64(e.wall_ms) && isU64(e.bytes), 'reservation');
    } else if (key === 'halts') {
      if (out.inst_until === undefined) out.inst_until = 0;
      if (out.reason === undefined) out.reason = null;
      if (out.hosts === undefined) out.hosts = {};
      if (!isU64(out.inst_until) || !(out.reason === null || typeof out.reason === 'string') || !isObject(out.hosts)) return null;
      for (const host of Object.keys(out.hosts)) if (!isU64(out.hosts[host])) delete out.hosts[host];
    } else if (key === 'ezproxy' || key === 'alma') {
      const flag = key === 'ezproxy' ? 'configured' : 'held';
      for (const name of Object.keys(out)) {
        const e = out[name];
        if (!(isObject(e) && typeof e[flag] === 'boolean' && isU64(e.at))) delete out[name];
      }
    }
    return out;
  }

  /** @returns {object|null} {persisted, tmp_dirs, run_counter} for a parsed file, or null when its shape is wrong */
  function normalize(raw) {
    if (!isObject(raw) || raw.v !== VERSION) return null;
    const src = raw.persisted === undefined ? {} : raw.persisted;
    if (!isObject(src)) return null;
    const persisted = {};
    for (const key of PERSIST_KEYS) {
      const value = src[key] === undefined ? defaultFor(key) : normalizeKey(key, src[key]);
      if (value === null) return null;
      persisted[key] = value;
    }
    let tmpDirs = raw.tmp_dirs === undefined ? [] : raw.tmp_dirs;
    if (!Array.isArray(tmpDirs)) return null;
    tmpDirs = Array.from(new Set(tmpDirs.filter((p) => typeof p === 'string' && p !== '')));
    const counter = raw.run_counter === undefined ? 0 : raw.run_counter;
    if (!isU64(counter)) return null;
    return { persisted, tmp_dirs: tmpDirs, run_counter: counter };
  }

  // ---------------------------------------------------------------- loading
  function resolveDir(opts) {
    if (opts && opts.dir) return String(opts.dir);
    return PathUtils.join(Zotero.Profile.dir, DATA_SUBDIR);
  }

  /** Move an unusable state file out of the way (overwriting an older .bad). Never throws. */
  async function quarantine(path) {
    try {
      await IOUtils.move(path, path + '.bad', { noOverwrite: false });
      log('warn', `moved the unusable ${FILE_NAME} to ${FILE_NAME}.bad`);
    } catch (e) {
      log('error', `could not move ${path} aside: ${errText(e)}`);
    }
  }

  async function readData(path) {
    let raw;
    try {
      raw = await IOUtils.readJSON(path);
    } catch (e) {
      if (e && e.name === 'NotFoundError') return defaultData();
      log('error', `cannot read ${path}: ${errText(e)}`);
      await quarantine(path);
      return defaultData();
    }
    const data = normalize(raw);
    if (data) return data;
    log('error', `${path} has an unexpected shape`);
    await quarantine(path);
    return defaultData();
  }

  async function load(dir) {
    const path = PathUtils.join(dir, FILE_NAME);
    const tmpRoot = PathUtils.join(dir, TMP_SUBDIR);
    await IOUtils.makeDirectory(dir, MKDIR);
    await IOUtils.makeDirectory(tmpRoot, MKDIR);
    const data = await readData(path);
    loaded = { dir, path, tmpRoot, data, rev: 0, writtenRev: 0, timer: null };
  }

  /** Flush and forget the loaded file without touching the init bookkeeping. */
  async function closeLoaded() {
    const st = loaded;
    if (st) { clearTimer(st); await flush(); }
    await chain;
    loaded = null;
  }

  function need(what) {
    if (!loaded) throw new Error(`ZFT.state.${what} called before init()`);
    return loaded;
  }

  /**
   * Resolve the data dir (default PathUtils.join(Zotero.Profile.dir, 'zotero-fulltext')), create it and
   * its tmp/ child, load state.json. Idempotent; a second call with the same dir is a no-op.
   * @param {{dir?: string}} [opts] dir override for tests/harness
   * @returns {Promise<void>}
   */
  async function init(opts) {
    const dir = resolveDir(opts);
    if (loaded && loaded.dir === dir) return;
    if (initPromise && initDir === dir) { await initPromise; return; }
    const previous = initPromise;
    initDir = dir;
    const mine = (async () => {
      if (previous) { try { await previous; } catch (e) { /* that caller sees its own failure */ } }
      if (loaded && loaded.dir !== dir) await closeLoaded();
      if (!loaded) await load(dir);
    })();
    initPromise = mine;
    try {
      await mine;
    } catch (e) {
      if (initPromise === mine) { initPromise = null; initDir = null; }
      throw e;
    }
  }

  // ---------------------------------------------------------------- reading
  /** @returns {{ledger: object, halts: object, ezproxy: object, alma: object}} deep copy, for Command::Configure.persisted */
  function getPersisted() {
    return clone(need('getPersisted').data.persisted);
  }

  /** @returns {number} next RunId (increments run_counter; persisted lazily with the next write) */
  function nextRunId() {
    const st = need('nextRunId');
    st.data.run_counter += 1;
    markDirty(st);
    return st.data.run_counter;
  }

  /** @returns {string} absolute data dir (valid after init) */
  function dataDir() { return need('dataDir').dir; }
  /** @returns {string} <dataDir>/tmp */
  function tmpRoot() { return need('tmpRoot').tmpRoot; }

  // ---------------------------------------------------------------- writing
  function clearTimer(st) {
    if (st.timer !== null) { clearTimeout(st.timer); st.timer = null; }
  }
  /** Arm the debounce timer unless one is pending. */
  function armTimer(st) {
    if (st.timer !== null) return;
    st.timer = setTimeout(() => {
      st.timer = null;
      flush();
    }, FLUSH_DEBOUNCE_MS);
  }
  function markDirty(st) {
    st.rev += 1;
    armTimer(st);
  }

  async function writeIfDirty() {
    const st = loaded;
    if (!st || st.rev === st.writtenRev) return;
    clearTimer(st);
    const target = st.rev;
    const body = { v: VERSION, persisted: st.data.persisted, tmp_dirs: st.data.tmp_dirs, run_counter: st.data.run_counter };
    try {
      await IOUtils.writeJSON(st.path, body, { tmpPath: st.path + '.tmp' });
      // A change made while the write was running has a higher rev and stays dirty.
      st.writtenRev = Math.max(st.writtenRev, target);
    } catch (e) {
      log('error', `writing ${st.path} failed (will retry on the next change or flush): ${errText(e)}`);
    }
  }

  /** Write now if dirty. Resolves after the atomic rename; never rejects. @returns {Promise<void>} */
  function flush() {
    const next = chain.then(writeIfDirty);
    chain = next;
    return next;
  }

  /**
   * Apply a Persist effect. Replaces persisted[key] with value (deep-copied) and schedules a write.
   * Unknown keys and non-object values are logged and ignored. Never rejects.
   * @param {'ledger'|'halts'|'ezproxy'|'alma'} key
   * @param {object} value
   * @param {{flush?: boolean}} [opts] flush=true: write now; promise resolves after the file is on disk
   * @returns {Promise<void>}
   */
  async function set(key, value, opts) {
    const st = loaded;
    if (!st) { log('warn', `set(${key}) ignored: state is not initialised`); return; }
    if (!PERSIST_KEYS.includes(key)) { log('warn', `set: unknown key ${String(key)} ignored`); return; }
    let copy;
    try {
      copy = clone(value);
    } catch (e) {
      log('warn', `set(${key}) ignored: value is not JSON (${errText(e)})`);
      return;
    }
    if (!isObject(copy)) { log('warn', `set(${key}) ignored: value is not an object`); return; }
    st.data.persisted[key] = copy;
    markDirty(st);
    if (opts && opts.flush) await flush();
  }

  /** Forget some persisted keys (prefs "Clear sign-in and limits state" button). @param {string[]} [keys] default all */
  async function reset(keys) {
    const st = loaded;
    if (!st) return;
    const list = Array.isArray(keys) ? keys : PERSIST_KEYS;
    if (!list.length) return;
    for (const key of list) {
      if (PERSIST_KEYS.includes(key)) st.data.persisted[key] = defaultFor(key);
      else log('warn', `reset: unknown key ${String(key)} ignored`);
    }
    markDirty(st);
    await flush();
  }

  // ---------------------------------------------------------------- temp dirs
  /** Record a temp dir BEFORE using it (so a crash leaves it known). Resolves after it is on disk. @returns {Promise<void>} */
  async function registerTmpDir(path) {
    const st = need('registerTmpDir');
    if (typeof path !== 'string' || path === '') throw new Error('registerTmpDir: path must be a non-empty string');
    if (!st.data.tmp_dirs.includes(path)) {
      st.data.tmp_dirs.push(path);
      markDirty(st);
    }
    await flush();
  }
  /** Forget a temp dir after it has been removed or moved into storage (written lazily). */
  async function unregisterTmpDir(path) {
    const st = loaded;
    if (!st) return;
    const at = st.data.tmp_dirs.indexOf(path);
    if (at < 0) return;
    st.data.tmp_dirs.splice(at, 1);
    markDirty(st);
  }

  /** @returns {Promise<'removed'|'absent'|'failed'>} */
  async function removeDir(path) {
    try {
      if (!(await IOUtils.exists(path))) return 'absent';
      await IOUtils.remove(path, { recursive: true, ignoreAbsent: true });
      return 'removed';
    } catch (e) {
      log('warn', `could not remove ${path}: ${errText(e)}`);
      return 'failed';
    }
  }

  /**
   * Remove every registered temp dir (IOUtils.remove recursive, ignoreAbsent) plus anything under
   * <dataDir>/tmp, except paths in opts.keep. Called at startup (before the first run) and shutdown.
   * Registered dirs that could not be removed (and the kept ones) stay registered for the next sweep.
   * @param {{keep?: string[]}} [opts] @returns {Promise<number>} number of directories removed
   */
  async function sweepTmp(opts) {
    const st = loaded;
    if (!st) return 0;
    const keep = new Set(((opts && opts.keep) || []).map(samePath));
    const gone = new Set();           // registered paths that no longer need a registration
    let removed = 0;
    for (const p of st.data.tmp_dirs.slice()) {
      if (keep.has(samePath(p))) continue;
      const result = await removeDir(p);
      if (result === 'removed') removed += 1;
      if (result !== 'failed') gone.add(p);
    }
    let children = [];
    try {
      children = await IOUtils.getChildren(st.tmpRoot, { ignoreAbsent: true });
    } catch (e) {
      log('warn', `cannot list ${st.tmpRoot}: ${errText(e)}`);
    }
    for (const child of children) {
      if (keep.has(samePath(child))) continue;
      if ((await removeDir(child)) === 'removed') removed += 1;
    }
    if (gone.size) {
      st.data.tmp_dirs = st.data.tmp_dirs.filter((p) => !gone.has(p));
      markDirty(st);
      await flush();
    }
    return removed;
  }

  /** Flush, cancel timers, forget the loaded file (init() may be called again). */
  async function dispose() {
    const pending = initPromise;
    if (pending) { try { await pending; } catch (e) { /* nothing was loaded */ } }
    await closeLoaded();
    initPromise = null;
    initDir = null;
  }

  ZFT.state = {
    FILE_NAME, VERSION, FLUSH_DEBOUNCE_MS, PERSIST_KEYS,
    init, getPersisted, set, flush, reset, registerTmpDir, unregisterTmpDir, sweepTmp,
    nextRunId, dataDir, tmpRoot, dispose,
  };
})(this);
