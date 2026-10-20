// The declared public surface of every plugin module (docs/design/plugin-js.md section 3). load.test.mjs asserts each name
// exists after loading the real scripts. Work packages may ADD names; removing or renaming one is a contract change.
export const API = {
  util: [
    'OP_ERR_KINDS', 'OpError', 'CancelScope', 'Deferred', 'TaskSet', 'sleep', 'idle', 'monoNow', 'wallNow', 'log', 'errText',
    'toU8', 'concatBytes', 'parseJson',
  ],
  l10n: ['format', 'reset'],
  config: ['DEFAULTS', 'PANE_LAYOUT', 'PLUGIN_ONLY', 'KCL_EZPROXY_HOSTS', 'LIMITS', 'typeOf', 'getPref', 'setPref', 'parseList', 'buildEngineConfig', 'validate', 'problems'],
  state: [
    'FILE_NAME', 'VERSION', 'FLUSH_DEBOUNCE_MS', 'PERSIST_KEYS', 'init', 'getPersisted', 'set', 'flush', 'reset',
    'registerTmpDir', 'unregisterTmpDir', 'sweepTmp', 'nextRunId', 'dataDir', 'tmpRoot', 'dispose',
  ],
  bodies: ['INSPECT_TEXT_MAX', 'PDF_HEAD', 'PDF_TAIL', 'PAGE_MAX_BYTES', 'MEM_CAP_TOTAL', 'BodyStore', 'store'],
  wasmHost: [
    'ABI_VERSION', 'WASM_FILE', 'INSPECT_WATCHDOG_MS', 'REQUIRED_EXPORTS', 'EngineTrap', 'EngineError', 'WasmInstance', 'Host',
    'load', 'get', 'shutdown',
  ],
  net: ['RESPONSE_HEADERS', 'DROPPED_REQUEST_HEADERS', 'TIMEOUT_BACKSTOP_MS', 'request', 'buildHeaders', 'classifyError'],
  pdfCapture: ['capture', 'activeCount', 'disposeAll'],
  browser: ['MAX_BROWSERS', 'POLL_MS', 'URL_TRAIL_MAX', 'PAGE_DATA_TIMEOUT_MS', 'load', 'trailEntry', 'stats'],
  translators: ['TRANSLATE_TIMEOUT_MS', 'MAX_CONCURRENT', 'run', 'parsePage'],
  zoteroOa: ['LOOKUP_TIMEOUT_MS', 'lookup'],
  items: ['snapshot', 'snapshotAll', 'menuEligible', 'getItem', 'watchDeletions'],
  save: ['savePdf', 'attachFile', 'titleForVersion', 'isStorableUrl'],
  confirm: ['ask'],
  progress: ['WINDOW_URL', 'WINDOW_TYPE', 'ROW_STATES', 'ProgressModel', 'open', 'update', 'finish', 'show', 'modelFor', 'handleAction', 'redactUrl', 'closeAll'],
  login: ['WINDOW_URL', 'WINDOW_TYPE', 'POLL_MS', 'KEEP_WAITING_AT', 'decide', 'open', 'update', 'warmUp', 'closeAll'],
  effects: ['COMPLETION_BATCH_MS', 'MAX_STEPS_PER_PUMP', 'STALL_MS', 'TRACE_RING', 'EFFECT_TYPES', 'Runner', 'createRunner', 'syntheticReport'],
  runs: ['SHUTDOWN_WAIT_MS', 'MODES', 'USER_ACTIONS', 'init', 'startRun', 'cancelRun', 'cancelItem', 'userAction', 'reconfigure', 'activeRuns', 'report', 'runner', 'shutdown'],
  menu: ['MENU_IDS', 'TARGETS', 'register', 'unregister', 'registeredKeys', 'commandFor', 'onShowing'],
  harness: ['RESULT_PREF', 'QUIT_PREF', 'MODULES', 'CHECKS', 'TIMEOUTS', 'maybeRun', 'endpointMap'],
  plugin: ['PRELOAD_DELAY_MS', 'REASON', 'started', 'paneID', 'tasks', 'startup', 'shutdown', 'addToWindow', 'removeFromWindow', 'onMainWindowLoad', 'onMainWindowUnload'],
};

/** Scripts that run in their own global (not in LOAD_ORDER) and the plugin-root files; checked for syntax only. */
export const STANDALONE_SCRIPTS = [
  'bootstrap.js', 'prefs.js',
  'content/prefs-pane.js', 'content/progress-window.js', 'content/login.js', 'content/inspect-worker.js',
];
export const XHTML_FILES = ['content/prefs.xhtml', 'content/progress.xhtml', 'content/login.xhtml'];
