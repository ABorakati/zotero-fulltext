/* ZFT.config: preference schema and the EngineConfig builder.
 *
 * Owner: js-shell. DEFAULTS is the single source of truth for pref names, types and default values;
 * plugin/prefs.js must mirror it exactly (tests/js/prefs.test.mjs compares them). Every EngineConfig
 * field of docs/design/contracts/proto.rs has exactly one pref here.
 *
 * Key naming: short key = "<group>.<field>" where group/field are the proto.rs struct/field names
 * (oa.openalex -> EngineConfig.oa.openalex). Full pref name = ZFT.PREF_BRANCH + short key.
 * List-valued prefs are comma-separated strings. Booleans are bools, counts are ints.
 *
 * buildEngineConfig() never throws and never returns a value serde would reject: every int is clamped into
 * LIMITS, every enum falls back to its default, every list is normalised. Problems the clamping hides (a login
 * timeout below the minimum) are still reported by problems(), which the preferences pane shows.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** KCL's EZproxy host list (research/04 section 2.4, probed 2026-10-20); the runtime probe may refine it. */
  const KCL_EZPROXY_HOSTS = [
    'doi.org', 'dx.doi.org', 'www.bmj.com', 'www.webofscience.com', 'www.liebertpub.com',
    'www.jneurosci.org', 'www.psychiatryonline.org', 'psychiatryonline.org', 'www.atsjournals.org',
    'www.ajronline.org', 'www.jospt.org', 'www.jove.com', 'www.elgaronline.com',
    'infoweb.newsbank.com', 'global.factiva.com', 'www.portal.euromonitor.com',
    'institutions.newscientist.com', 'www.lrb.co.uk', 'libguides.kcl.ac.uk',
  ];

  /** Defaults, keyed by short pref key. The KCL values are the institution defaults of this build. */
  const DEFAULTS = {
    // --- EngineConfig.oa
    'oa.openalex': true,
    'oa.openalex_key': '',
    'oa.europepmc': true,
    'oa.biorxiv': true,
    'oa.pmc_s3': true,
    'oa.europepmc_get_pdf': true,
    'oa.medrxiv_early': true,
    'oa.arxiv': true,
    'oa.zotero_oa': true,
    'oa.unpaywall_email': '',          // no placeholder ships: Unpaywall stays off until the user sets one
    'oa.semantic_scholar_key': '',
    'oa.core_key': '',
    'oa.version_order': 'published,accepted,submitted',
    // --- EngineConfig.inst (InstPrefs)
    'inst.enabled': true,
    'inst.try_direct': true,
    'inst.use_openathens': true,
    'inst.use_ezproxy': true,
    'inst.check_alma': true,
    'inst.login_mode': 'window',       // 'window' | 'handoff'
    'inst.auto_open_login': true,
    'inst.login_timeout_sec': 300,
    'inst.group_libraries': 'ask',     // 'ask' | 'never' | 'allow'
    'inst.confirm_above': 10,
    'inst.batch_max': 20,
    'inst.hourly_cap': 30,
    'inst.daily_cap': 80,
    'inst.mb_per_hour': 300,
    'inst.min_gap_sec': 5,
    'inst.challenge_windows': true,
    // --- EngineConfig.run (RunPrefs)
    'run.item_concurrency': 4,
    'run.hedge': true,
    'run.pdf_max_mb': 100,
    // --- EngineConfig.institution
    'institution.name': "King's College London",
    'institution.openathens_domain': 'kcl.ac.uk',
    'institution.ezproxy_login_host': 'login.kcl.idm.oclc.org',
    'institution.ezproxy_suffix': 'kcl.idm.oclc.org',
    'institution.ezproxy_hosts': KCL_EZPROXY_HOSTS.join(','),
    'institution.alma_base': 'https://librarysearch.kcl.ac.uk',
    'institution.alma_inst': '44KCL_INST',
    'institution.libkey_library_id': 286,
    'institution.idp_hosts': 'login.microsoftonline.com,login.live.com,login.openathens.net,kclidp.kcl.ac.uk',
    'institution.entra_tenant': '8370cf14-16f3-4c16-b83c-724071654356',
    'institution.passthrough_hosts': 'journals.lww.com',
    'institution.warmup_url': '',      // '' = derive from the OpenAthens redirector
    // --- plugin-only (not part of EngineConfig)
    'debug.trace': false,              // EngineConfig.trace + extra JS logging
    'debug.inspect_in_worker': true,   // run the pure wasm instance in a ChromeWorker (falls back to main thread)
  };

  /** Short keys that must NOT be forwarded into EngineConfig. */
  const PLUGIN_ONLY = ['debug.inspect_in_worker'];

  /** Accepted ranges of the integer prefs, [min, max] inclusive. Used by the builder AND by the pane's number inputs. */
  const LIMITS = {
    'inst.login_timeout_sec': [30, 1800],
    'inst.confirm_above': [0, 100000],
    'inst.batch_max': [1, 100000],
    'inst.hourly_cap': [0, 100000],
    'inst.daily_cap': [0, 100000],
    'inst.mb_per_hour': [0, 1000000],
    'inst.min_gap_sec': [0, 3600],
    'run.item_concurrency': [1, 8],
    'run.pdf_max_mb': [1, 500],
    'institution.libkey_library_id': [0, 1000000000],
  };

  /** proto.rs Version values a user may order (Unknown is engine-internal). */
  const VERSIONS = ['published', 'accepted', 'submitted'];
  const LOGIN_MODES = ['window', 'handoff'];
  const GROUP_POLICIES = ['ask', 'never', 'allow'];

  const PROBLEM_OPENATHENS = 'zotero-fulltext-prefs-problem-openathens-domain';
  const PROBLEM_TIMEOUT = 'zotero-fulltext-prefs-problem-login-timeout';
  const PROBLEM_EMAIL = 'zotero-fulltext-prefs-problem-unpaywall-email';

  /**
   * Declarative layout of the preferences pane, consumed by prefs-pane.js and checked by tests/js/config.test.mjs:
   * every DEFAULTS key appears in exactly one row, the widget matches typeOf(key), every l10n id exists in the FTL.
   * Shape: [{id, heading: '<l10n id>', rows: [{key, widget: 'checkbox'|'text'|'number'|'select'|'list', label: '<l10n id>',
   *          min?, max?, options?: [{value, label: '<l10n id>'}], secret?: boolean}]}]
   * Ids are written out in full (not built from prefixes) so tests/js/ftl.test.mjs can see every one of them.
   */
  const PANE_LAYOUT = [
    {
      id: 'oa',
      heading: 'zotero-fulltext-prefs-oa-heading',
      rows: [
        { key: 'oa.openalex', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-openalex' },
        { key: 'oa.openalex_key', widget: 'text', label: 'zotero-fulltext-prefs-oa-openalex-key', secret: true },
        { key: 'oa.europepmc', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-europepmc' },
        { key: 'oa.europepmc_get_pdf', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-europepmc-get-pdf' },
        { key: 'oa.biorxiv', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-biorxiv' },
        { key: 'oa.medrxiv_early', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-medrxiv-early' },
        { key: 'oa.pmc_s3', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-pmc-s3' },
        { key: 'oa.arxiv', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-arxiv' },
        { key: 'oa.zotero_oa', widget: 'checkbox', label: 'zotero-fulltext-prefs-oa-zotero-oa' },
        { key: 'oa.unpaywall_email', widget: 'text', label: 'zotero-fulltext-prefs-oa-unpaywall-email' },
        { key: 'oa.semantic_scholar_key', widget: 'text', label: 'zotero-fulltext-prefs-oa-semantic-scholar-key', secret: true },
        { key: 'oa.core_key', widget: 'text', label: 'zotero-fulltext-prefs-oa-core-key', secret: true },
        { key: 'oa.version_order', widget: 'list', label: 'zotero-fulltext-prefs-oa-version-order' },
      ],
    },
    {
      id: 'inst',
      heading: 'zotero-fulltext-prefs-inst-heading',
      rows: [
        { key: 'inst.enabled', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-enabled' },
        { key: 'inst.try_direct', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-try-direct' },
        { key: 'inst.use_openathens', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-use-openathens' },
        { key: 'inst.use_ezproxy', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-use-ezproxy' },
        { key: 'inst.check_alma', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-check-alma' },
        {
          key: 'inst.login_mode', widget: 'select', label: 'zotero-fulltext-prefs-inst-login-mode',
          options: [
            { value: 'window', label: 'zotero-fulltext-prefs-inst-login-mode-window' },
            { value: 'handoff', label: 'zotero-fulltext-prefs-inst-login-mode-handoff' },
          ],
        },
        { key: 'inst.auto_open_login', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-auto-open-login' },
        {
          key: 'inst.login_timeout_sec', widget: 'number', label: 'zotero-fulltext-prefs-inst-login-timeout',
          min: LIMITS['inst.login_timeout_sec'][0], max: LIMITS['inst.login_timeout_sec'][1],
        },
        {
          key: 'inst.group_libraries', widget: 'select', label: 'zotero-fulltext-prefs-inst-group-libraries',
          options: [
            { value: 'ask', label: 'zotero-fulltext-prefs-inst-group-ask' },
            { value: 'never', label: 'zotero-fulltext-prefs-inst-group-never' },
            { value: 'allow', label: 'zotero-fulltext-prefs-inst-group-allow' },
          ],
        },
        { key: 'inst.challenge_windows', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-challenge-windows' },
      ],
    },
    {
      id: 'limits',
      heading: 'zotero-fulltext-prefs-limits-heading',
      rows: [
        { key: 'inst.confirm_above', widget: 'number', label: 'zotero-fulltext-prefs-inst-confirm-above', min: LIMITS['inst.confirm_above'][0], max: LIMITS['inst.confirm_above'][1] },
        { key: 'inst.batch_max', widget: 'number', label: 'zotero-fulltext-prefs-inst-batch-max', min: LIMITS['inst.batch_max'][0], max: LIMITS['inst.batch_max'][1] },
        { key: 'inst.hourly_cap', widget: 'number', label: 'zotero-fulltext-prefs-inst-hourly-cap', min: LIMITS['inst.hourly_cap'][0], max: LIMITS['inst.hourly_cap'][1] },
        { key: 'inst.daily_cap', widget: 'number', label: 'zotero-fulltext-prefs-inst-daily-cap', min: LIMITS['inst.daily_cap'][0], max: LIMITS['inst.daily_cap'][1] },
        { key: 'inst.mb_per_hour', widget: 'number', label: 'zotero-fulltext-prefs-inst-mb-per-hour', min: LIMITS['inst.mb_per_hour'][0], max: LIMITS['inst.mb_per_hour'][1] },
        { key: 'inst.min_gap_sec', widget: 'number', label: 'zotero-fulltext-prefs-inst-min-gap', min: LIMITS['inst.min_gap_sec'][0], max: LIMITS['inst.min_gap_sec'][1] },
      ],
    },
    {
      id: 'run',
      heading: 'zotero-fulltext-prefs-run-heading',
      rows: [
        { key: 'run.item_concurrency', widget: 'number', label: 'zotero-fulltext-prefs-run-item-concurrency', min: LIMITS['run.item_concurrency'][0], max: LIMITS['run.item_concurrency'][1] },
        { key: 'run.hedge', widget: 'checkbox', label: 'zotero-fulltext-prefs-run-hedge' },
        { key: 'run.pdf_max_mb', widget: 'number', label: 'zotero-fulltext-prefs-run-pdf-max-mb', min: LIMITS['run.pdf_max_mb'][0], max: LIMITS['run.pdf_max_mb'][1] },
      ],
    },
    {
      id: 'institution',
      heading: 'zotero-fulltext-prefs-institution-heading',
      rows: [
        { key: 'institution.name', widget: 'text', label: 'zotero-fulltext-prefs-institution-name' },
        { key: 'institution.openathens_domain', widget: 'text', label: 'zotero-fulltext-prefs-institution-openathens-domain' },
        { key: 'institution.ezproxy_login_host', widget: 'text', label: 'zotero-fulltext-prefs-institution-ezproxy-login-host' },
        { key: 'institution.ezproxy_suffix', widget: 'text', label: 'zotero-fulltext-prefs-institution-ezproxy-suffix' },
        { key: 'institution.ezproxy_hosts', widget: 'list', label: 'zotero-fulltext-prefs-institution-ezproxy-hosts' },
        { key: 'institution.alma_base', widget: 'text', label: 'zotero-fulltext-prefs-institution-alma-base' },
        { key: 'institution.alma_inst', widget: 'text', label: 'zotero-fulltext-prefs-institution-alma-inst' },
        { key: 'institution.libkey_library_id', widget: 'number', label: 'zotero-fulltext-prefs-institution-libkey-library-id', min: LIMITS['institution.libkey_library_id'][0], max: LIMITS['institution.libkey_library_id'][1] },
        { key: 'institution.idp_hosts', widget: 'list', label: 'zotero-fulltext-prefs-institution-idp-hosts' },
        { key: 'institution.entra_tenant', widget: 'text', label: 'zotero-fulltext-prefs-institution-entra-tenant' },
        { key: 'institution.passthrough_hosts', widget: 'list', label: 'zotero-fulltext-prefs-institution-passthrough-hosts' },
        { key: 'institution.warmup_url', widget: 'text', label: 'zotero-fulltext-prefs-institution-warmup-url' },
      ],
    },
    {
      id: 'debug',
      heading: 'zotero-fulltext-prefs-debug-heading',
      rows: [
        { key: 'debug.trace', widget: 'checkbox', label: 'zotero-fulltext-prefs-debug-trace' },
        { key: 'debug.inspect_in_worker', widget: 'checkbox', label: 'zotero-fulltext-prefs-debug-inspect-in-worker' },
      ],
    },
  ];

  /** @returns {'bool'|'int'|'string'} pref type of a short key, from its default */
  function typeOf(key) {
    const d = DEFAULTS[key];
    return typeof d === 'boolean' ? 'bool' : typeof d === 'number' ? 'int' : 'string';
  }

  /** Read one pref by short key (falls back to the default, never throws for a known key). Uses Zotero.Prefs.get(full, true). */
  function getPref(key) {
    if (!(key in DEFAULTS)) throw new Error('unknown pref ' + key);
    try {
      const v = Zotero.Prefs.get(ZFT.PREF_BRANCH + key, true);
      return v === undefined || v === null ? DEFAULTS[key] : v;
    } catch (e) {
      return DEFAULTS[key];
    }
  }

  /** Write one pref by short key. The value is coerced to the key's type so Zotero never sees a type change. */
  function setPref(key, value) {
    if (!(key in DEFAULTS)) throw new Error('unknown pref ' + key);
    const type = typeOf(key);
    let v = value;
    if (type === 'bool') v = !!value;
    else if (type === 'int') v = toInt(value, DEFAULTS[key]);
    else v = value === undefined || value === null ? '' : String(value);
    Zotero.Prefs.set(ZFT.PREF_BRANCH + key, v, true);
  }

  // --- value normalisation ---------------------------------------------------------------------------------------

  /** @returns {number} a rounded finite number, or `fallback` when `v` is not numeric */
  function toInt(v, fallback) {
    const n = typeof v === 'number' ? v : typeof v === 'string' && v.trim() !== '' ? Number(v) : NaN;
    return Number.isFinite(n) ? Math.round(n) : fallback;
  }
  function clampInt(v, lo, hi, fallback) {
    const n = toInt(v, fallback);
    return Math.min(hi, Math.max(lo, n));
  }
  function toBool(v, fallback) {
    if (typeof v === 'boolean') return v;
    if (typeof v === 'number') return v !== 0;
    if (v === 'true') return true;
    if (v === 'false') return false;
    return fallback;
  }
  function toStr(v, fallback) {
    return typeof v === 'string' ? v.trim() : fallback;
  }
  function oneOf(v, allowed, fallback) {
    const s = typeof v === 'string' ? v.trim().toLowerCase() : '';
    return allowed.includes(s) ? s : fallback;
  }
  /** Comma list: trimmed items, empties dropped, duplicates dropped (first wins), optional per-item mapping. */
  function parseList(v, map) {
    const out = [];
    const seen = new Set();
    for (const raw of String(v === undefined || v === null ? '' : v).split(',')) {
      const item = map ? map(raw.trim()) : raw.trim();
      if (item && !seen.has(item)) {
        seen.add(item);
        out.push(item);
      }
    }
    return out;
  }
  /** Lower-case host name; a pasted scheme, path or trailing dot is dropped ("https://Www.BMJ.com/x" -> "www.bmj.com"). */
  function normHost(s) {
    return String(s).trim().toLowerCase().replace(/^[a-z][a-z0-9+.-]*:\/\//, '').replace(/[/?#].*$/, '').replace(/\.+$/, '');
  }

  /** version_order: known versions only, user order first, missing ones appended in default order. */
  function parseVersionOrder(v) {
    const picked = parseList(v, (s) => (VERSIONS.includes(s.toLowerCase()) ? s.toLowerCase() : ''));
    for (const version of VERSIONS) if (!picked.includes(version)) picked.push(version);
    return picked;
  }

  /**
   * Build the proto.rs EngineConfig from prefs: shape {oa, inst, run, institution, endpoint_map, trace}.
   * Parses comma lists (trim, drop empties, lowercase hosts), maps ezproxy_login_host+suffix to
   * institution.ezproxy (null when either is empty), alma_base+alma_inst to institution.alma
   * (null when base is empty), libkey_library_id 0 -> null, entra_tenant '' -> null, clamps ints into LIMITS,
   * version_order to the three known versions. endpoint_map comes from ZFT.harness.endpointMap() ([] outside the harness).
   * @param {(key: string) => any} [get] pref reader, default getPref (tests pass a fake; unknown/undefined -> default)
   * @returns {object} EngineConfig (JSON-serialisable, accepted by serde)
   */
  function buildEngineConfig(get) {
    const read = (key) => {
      let v;
      try { v = (get || getPref)(key); } catch (e) { v = undefined; }
      return v === undefined || v === null ? DEFAULTS[key] : v;
    };
    const bool = (key) => toBool(read(key), DEFAULTS[key]);
    const str = (key) => toStr(read(key), DEFAULTS[key]);
    const int = (key) => {
      const [lo, hi] = LIMITS[key] || [0, 4294967295];
      return clampInt(read(key), lo, hi, DEFAULTS[key]);
    };
    const host = (key) => normHost(str(key));
    const hosts = (key) => parseList(read(key), normHost);

    const loginHost = host('institution.ezproxy_login_host');
    const suffix = host('institution.ezproxy_suffix');
    const almaBase = str('institution.alma_base').replace(/\/+$/, '');
    const libkey = int('institution.libkey_library_id');
    const tenant = str('institution.entra_tenant');

    return {
      oa: {
        openalex: bool('oa.openalex'),
        openalex_key: str('oa.openalex_key'),
        europepmc: bool('oa.europepmc'),
        biorxiv: bool('oa.biorxiv'),
        pmc_s3: bool('oa.pmc_s3'),
        europepmc_get_pdf: bool('oa.europepmc_get_pdf'),
        medrxiv_early: bool('oa.medrxiv_early'),
        arxiv: bool('oa.arxiv'),
        zotero_oa: bool('oa.zotero_oa'),
        unpaywall_email: str('oa.unpaywall_email'),
        semantic_scholar_key: str('oa.semantic_scholar_key'),
        core_key: str('oa.core_key'),
        version_order: parseVersionOrder(read('oa.version_order')),
      },
      inst: {
        enabled: bool('inst.enabled'),
        try_direct: bool('inst.try_direct'),
        use_openathens: bool('inst.use_openathens'),
        use_ezproxy: bool('inst.use_ezproxy'),
        check_alma: bool('inst.check_alma'),
        login_mode: oneOf(read('inst.login_mode'), LOGIN_MODES, DEFAULTS['inst.login_mode']),
        auto_open_login: bool('inst.auto_open_login'),
        login_timeout_sec: int('inst.login_timeout_sec'),
        group_libraries: oneOf(read('inst.group_libraries'), GROUP_POLICIES, DEFAULTS['inst.group_libraries']),
        confirm_above: int('inst.confirm_above'),
        batch_max: int('inst.batch_max'),
        hourly_cap: int('inst.hourly_cap'),
        daily_cap: int('inst.daily_cap'),
        mb_per_hour: int('inst.mb_per_hour'),
        min_gap_sec: int('inst.min_gap_sec'),
        challenge_windows: bool('inst.challenge_windows'),
      },
      run: {
        item_concurrency: int('run.item_concurrency'),
        hedge: bool('run.hedge'),
        pdf_max_mb: int('run.pdf_max_mb'),
      },
      institution: {
        name: str('institution.name'),
        openathens_domain: host('institution.openathens_domain'),
        ezproxy: loginHost && suffix ? { login_host: loginHost, suffix } : null,
        ezproxy_hosts: hosts('institution.ezproxy_hosts'),
        alma: almaBase ? { base: almaBase, inst: str('institution.alma_inst') } : null,
        libkey_library_id: libkey > 0 ? libkey : null,
        idp_hosts: hosts('institution.idp_hosts'),
        entra_tenant: tenant ? tenant : null,
        passthrough_hosts: hosts('institution.passthrough_hosts'),
        warmup_url: str('institution.warmup_url'),
      },
      endpoint_map: harnessEndpointMap(),
      trace: bool('debug.trace'),
    };
  }

  /** The harness-only endpoint map; [] when the harness module is absent or the pref is not set. */
  function harnessEndpointMap() {
    try {
      return ZFT.harness && typeof ZFT.harness.endpointMap === 'function' ? ZFT.harness.endpointMap() : [];
    } catch (e) {
      return [];
    }
  }

  /**
   * Human-readable misconfiguration problems (empty array = fine).
   * @param {object} config EngineConfig (any group may be missing)
   * @returns {string[]} l10n ids of the problems, e.g. 'zotero-fulltext-prefs-problem-openathens-domain'
   */
  function validate(config) {
    const c = config || {};
    const inst = c.inst || {};
    const institution = c.institution || {};
    const oa = c.oa || {};
    const out = [];
    if (inst.enabled && !String(institution.openathens_domain || '').trim()) out.push(PROBLEM_OPENATHENS);
    if (Number(inst.login_timeout_sec) < LIMITS['inst.login_timeout_sec'][0]) out.push(PROBLEM_TIMEOUT);
    const email = String(oa.unpaywall_email || '').trim();
    if (email && !/^[^\s@]+@[^\s@]+$/.test(email)) out.push(PROBLEM_EMAIL);
    return out;
  }

  /**
   * Problems of the CURRENT prefs: validate(buildEngineConfig(get)) plus the ones clamping hides (a hand-edited login
   * timeout below the minimum is clamped in the config, so only the raw value shows it).
   * @param {(key: string) => any} [get] pref reader, default getPref
   * @returns {string[]} l10n ids
   */
  function problems(get) {
    const out = validate(buildEngineConfig(get));
    let rawTimeout = NaN;
    try { rawTimeout = toInt((get || getPref)('inst.login_timeout_sec'), NaN); } catch (e) { /* keep NaN: nothing to add */ }
    if (rawTimeout < LIMITS['inst.login_timeout_sec'][0] && !out.includes(PROBLEM_TIMEOUT)) out.push(PROBLEM_TIMEOUT);
    return out;
  }

  ZFT.config = {
    DEFAULTS, PANE_LAYOUT, PLUGIN_ONLY, KCL_EZPROXY_HOSTS, LIMITS,
    typeOf, getPref, setPref, parseList, buildEngineConfig, validate, problems,
  };
})(this);
