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
 * Institution picker: INSTITUTIONS lists the built-in presets (KCL = DEFAULTS); applyPreset(id) writes every institution.* pref,
 * the plugin-only institution.preset (the remembered choice) and inst.enabled. 'other' = custom details, 'none' = open access only
 * (inst.enabled = false). effectivePreset() derives what the picker shows from the data, so it can never disagree with the engine.
 *
 * Access methods: an institution signs people in through OpenAthens, Shibboleth / UK federation (institution.saml_entity_id, found by
 * name in the SeamlessAccess metadata) and/or a library proxy (EZproxy: ezproxy_login_host + ezproxy_suffix, ezproxy_all_hosts =
 * every publisher). Each INSTITUTIONS entry declares its `methods`; inst.use_openathens / use_saml / use_ezproxy switch a method
 * off; activeMethods() lists what is set up and on, and validate() reports 'no-method' when institutional access is on without one.
 * Pure helpers for the pane: parseSamlSearch, parseProxyPrefix, proxyTestUrl, classifyProxyResponse, withIdpHost.
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
    'inst.use_saml': true,             // Shibboleth / UK federation sign-in (institution.saml_entity_id)
    'inst.check_alma': true,
    'inst.login_mode': 'window',       // 'window' | 'handoff'
    'inst.auto_open_login': true,
    'inst.keep_session': true,         // plugin-only: keep the institutional sign-in between Zotero sessions (not in EngineConfig)
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
    'institution.preset': 'kcl',       // plugin-only: the picker choice ('kcl' | another INSTITUTIONS id | 'other' | 'none')
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
    // SAML / EZproxy extras. KCL's IdP entityID: UK federation metadata via md.seamlessaccess.org, 2026-10-21 (host kclidp.kcl.ac.uk).
    'institution.saml_entity_id': 'https://kclidp.kcl.ac.uk/idp/shibboleth',   // '' = no Shibboleth sign-in (null in the EngineConfig)
    'institution.ezproxy_all_hosts': false,   // true = send every publisher through the proxy, not only ezproxy_hosts
    // --- plugin-only (not part of EngineConfig)
    'debug.trace': false,              // EngineConfig.trace + extra JS logging
    'debug.inspect_in_worker': true,   // run the pure wasm instance in a ChromeWorker (falls back to main thread)
  };

  /** Short keys that must NOT be forwarded into EngineConfig. */
  const PLUGIN_ONLY = ['debug.inspect_in_worker', 'inst.keep_session', 'institution.preset'];

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

  // --- institution presets ---------------------------------------------------------------------------------------

  /** The two non-preset picker entries. 'other' = custom details typed by the user; 'none' = open access only. */
  const PRESET_OTHER = 'other';
  const PRESET_NONE = 'none';
  const PRESET_DEFAULT = 'kcl';

  /** The pref keys an institution choice owns (everything under institution.* except the choice itself), DEFAULTS order. */
  const INSTITUTION_KEYS = [
    'institution.name', 'institution.openathens_domain', 'institution.ezproxy_login_host', 'institution.ezproxy_suffix',
    'institution.ezproxy_hosts', 'institution.alma_base', 'institution.alma_inst', 'institution.libkey_library_id',
    'institution.idp_hosts', 'institution.entra_tenant', 'institution.passthrough_hosts', 'institution.warmup_url',
    'institution.saml_entity_id', 'institution.ezproxy_all_hosts',
  ];

  /** The sign-in methods an institution can offer, in display order. Each has a use flag (inst.use_<method>) and one set of institution.* details. */
  const METHODS = ['openathens', 'saml', 'ezproxy'];
  const METHOD_USE_PREFS = { openathens: 'inst.use_openathens', saml: 'inst.use_saml', ezproxy: 'inst.use_ezproxy' };

  /** Sign-in hosts every OpenAthens institution passes through; a preset adds its own IdP host when it has one. */
  const GENERIC_IDP_HOSTS = 'login.openathens.net,login.microsoftonline.com,login.live.com';

  /**
   * The institution.* values of a preset. Whatever a preset does not know stays empty (null / false in the EngineConfig):
   *   openathens  the OpenAthens domain (the redirector knows it)            -> institution.openathens_domain
   *   saml        the UK-federation identity provider's entityID              -> institution.saml_entity_id
   *   proxy       {loginHost, suffix} of the library's EZproxy                -> institution.ezproxy_login_host / _suffix
   *   allHosts    send every publisher through the proxy (a proxy preset has no host list of its own)
   *   idpHost     an extra sign-in host the engine should treat as an identity provider (an entityID's own host is added automatically)
   * Alma, LibKey and Entra are only known for KCL (research/04).
   */
  function presetValues(o) {
    const idpHosts = [GENERIC_IDP_HOSTS];
    if (o.idpHost) idpHosts.push(o.idpHost);
    const samlHost = o.saml ? entityIdHost(o.saml) : '';
    if (samlHost && samlHost !== o.idpHost) idpHosts.push(samlHost);
    return {
      'institution.name': o.name,
      'institution.openathens_domain': o.openathens || '',
      'institution.ezproxy_login_host': o.proxy ? o.proxy.loginHost : '',
      'institution.ezproxy_suffix': o.proxy ? o.proxy.suffix : '',
      'institution.ezproxy_hosts': '',
      'institution.alma_base': '',
      'institution.alma_inst': '',
      'institution.libkey_library_id': 0,
      'institution.idp_hosts': idpHosts.join(','),
      'institution.entra_tenant': '',
      // The redirector is a no-op for LWW whichever institution asks (research/04 section 4.3), so this is not KCL-specific.
      'institution.passthrough_hosts': DEFAULTS['institution.passthrough_hosts'],
      'institution.warmup_url': '',
      'institution.saml_entity_id': o.saml || '',
      'institution.ezproxy_all_hosts': !!o.allHosts,
    };
  }

  /** A preset that only knows its OpenAthens domain (and maybe its IdP host); blank arguments make the blank "Other" institution. */
  function openAthensValues(name, domain, idpHost) {
    return presetValues({ name, openathens: domain, idpHost });
  }

  /** The host of an entityID that is a URL ('' for urn: ids and anything else). */
  function entityIdHost(entityId) {
    const m = /^https?:\/\/([^/?#:@\s]+)/i.exec(String(entityId || ''));
    return m ? m[1].toLowerCase() : '';
  }

  function kclValues() {
    const out = {};
    for (const key of INSTITUTION_KEYS) out[key] = DEFAULTS[key];
    return out;
  }

  /** Read the methods a preset's values provide (what the preset really has, whatever it declares). */
  function methodsOfValues(values) {
    const out = [];
    if (String(values['institution.openathens_domain'] || '').trim()) out.push('openathens');
    if (String(values['institution.saml_entity_id'] || '').trim()) out.push('saml');
    if (String(values['institution.ezproxy_login_host'] || '').trim() && String(values['institution.ezproxy_suffix'] || '').trim()) out.push('ezproxy');
    return out;
  }

  function makePreset(id, l10n, methods, values) {
    return { id, l10n, methods, values };
  }

  /**
   * Built-in institutions, in picker order (KCL first, then by id). `methods` declares the access methods the preset offers
   * (any of 'openathens', 'saml', 'ezproxy'; tests/js/institution.test.mjs checks it equals what `values` provides) and `values`
   * holds every INSTITUTION_KEYS pref; applying a preset writes all of them and switches its methods on. KCL is exactly DEFAULTS.
   * Every datum was checked on 2026-10-21 without signing in to anything:
   *  - openathens: unauthenticated GET of https://go.openathens.net/redirector/<domain>?url=<public Nature DOI>: a recognised
   *    organisation answers 200 "OpenAthens / Please wait..." then 302 to sp.nature.com/saml/login?idp=<entityID>; an unknown one
   *    answers 403. Imperial, Oxford, Cambridge, QMUL, Manchester and Edinburgh answered 403, so they have no OpenAthens domain.
   *  - saml: the entityID is the entry of the same organisation (domain cross-checked) in the UK Access Management Federation
   *    metadata as served by SeamlessAccess, GET https://md.seamlessaccess.org/entities/?q=<name> (visible IdPs only).
   *    City St George's and Reading publish OpenAthens-local-authentication entityIDs (".../oala/metadata") and St George's
   *    only a legacy urn:mace:ac.uk:sdss id, so those three stay OpenAthens-only.
   *  - ezproxy: only where the library's own public page names the prefix (Edinburgh library.ed.ac.uk eresources-access,
   *    Southampton library.soton.ac.uk/offcampus/ezproxy, Glasgow gla.ac.uk library news, Queen's Belfast libguides.qub.ac.uk
   *    ezproxymigrationoclc) AND an unauthenticated GET of https://<host>/login?qurl=https%3A%2F%2Fwww.nature.com%2F answers with
   *    the EZproxy sign-in page (200 "Shibboleth Authentication Request"). Hostname-style proxies: suffix = host without "login.".
   *    These presets have no host list of their own, so the proxy is used for every publisher (ezproxy_all_hosts).
   * Ids are written out in full in `l10n` so tests/js/ftl.test.mjs sees them.
   */
  const INSTITUTIONS = [
    makePreset('kcl', 'zotero-fulltext-prefs-institution-preset-kcl', ['openathens', 'saml', 'ezproxy'], kclValues()),
    makePreset('bath', 'zotero-fulltext-prefs-institution-preset-bath', ['saml'],
      presetValues({ name: 'University of Bath', saml: 'https://idp.bath.ac.uk/shibboleth' })),
    makePreset('birmingham', 'zotero-fulltext-prefs-institution-preset-birmingham', ['saml'],
      presetValues({ name: 'University of Birmingham', saml: 'https://idp.bham.ac.uk/shibboleth' })),
    makePreset('bristol', 'zotero-fulltext-prefs-institution-preset-bristol', ['saml'],
      presetValues({ name: 'University of Bristol', saml: 'https://idp.bris.ac.uk/shibboleth' })),
    makePreset('cambridge', 'zotero-fulltext-prefs-institution-preset-cambridge', ['saml'],
      presetValues({ name: 'University of Cambridge', saml: 'https://shib.raven.cam.ac.uk/shibboleth' })),
    makePreset('cardiff', 'zotero-fulltext-prefs-institution-preset-cardiff', ['saml'],
      presetValues({ name: 'Cardiff University', saml: 'https://idp.cardiff.ac.uk/shibboleth' })),
    makePreset('city', 'zotero-fulltext-prefs-institution-preset-city', ['openathens'], openAthensValues("City St George's, University of London", 'city.ac.uk', '')),
    makePreset('edinburgh', 'zotero-fulltext-prefs-institution-preset-edinburgh', ['saml', 'ezproxy'],
      presetValues({ name: 'University of Edinburgh', saml: 'https://idp.ed.ac.uk/shibboleth', proxy: { loginHost: 'login.eux.idm.oclc.org', suffix: 'eux.idm.oclc.org' }, allHosts: true })),
    makePreset('exeter', 'zotero-fulltext-prefs-institution-preset-exeter', ['saml'],
      presetValues({ name: 'University of Exeter', saml: 'https://elibrary.exeter.ac.uk/idp/shibboleth' })),
    makePreset('glasgow', 'zotero-fulltext-prefs-institution-preset-glasgow', ['saml', 'ezproxy'],
      presetValues({ name: 'University of Glasgow', saml: 'https://idp.gla.ac.uk/shibboleth', proxy: { loginHost: 'login.uofglasgow.idm.oclc.org', suffix: 'uofglasgow.idm.oclc.org' }, allHosts: true })),
    makePreset('imperial', 'zotero-fulltext-prefs-institution-preset-imperial', ['saml'],
      presetValues({ name: 'Imperial College London', saml: 'https://shibboleth.imperial.ac.uk/shibboleth' })),
    makePreset('lancaster', 'zotero-fulltext-prefs-institution-preset-lancaster', ['saml'],
      presetValues({ name: 'Lancaster University', saml: 'https://idp.lancs.ac.uk/idp/shibboleth' })),
    makePreset('leeds', 'zotero-fulltext-prefs-institution-preset-leeds', ['openathens', 'saml'],
      presetValues({ name: 'University of Leeds', openathens: 'leeds.ac.uk', saml: 'https://passport01.leeds.ac.uk/idp/shibboleth', idpHost: 'passport01.leeds.ac.uk' })),
    makePreset('leicester', 'zotero-fulltext-prefs-institution-preset-leicester', ['saml'],
      presetValues({ name: 'University of Leicester', saml: 'https://idp.le.ac.uk/shibboleth' })),
    makePreset('lse', 'zotero-fulltext-prefs-institution-preset-lse', ['saml'],
      presetValues({ name: 'London School of Economics and Political Science', saml: 'https://lse.ac.uk/idp' })),
    makePreset('manchester', 'zotero-fulltext-prefs-institution-preset-manchester', ['saml'],
      presetValues({ name: 'University of Manchester', saml: 'https://shib.manchester.ac.uk/shibboleth' })),
    makePreset('newcastle', 'zotero-fulltext-prefs-institution-preset-newcastle', ['saml'],
      presetValues({ name: 'Newcastle University', saml: 'https://gateway.ncl.ac.uk/idp/shibboleth' })),
    makePreset('nottingham', 'zotero-fulltext-prefs-institution-preset-nottingham', ['saml'],
      presetValues({ name: 'University of Nottingham', saml: 'https://idp.nottingham.ac.uk/shibboleth' })),
    makePreset('oxford', 'zotero-fulltext-prefs-institution-preset-oxford', ['saml'],
      presetValues({ name: 'University of Oxford', saml: 'https://registry.shibboleth.ox.ac.uk/idp' })),
    makePreset('qmul', 'zotero-fulltext-prefs-institution-preset-qmul', ['saml'],
      presetValues({ name: 'Queen Mary University of London', saml: 'https://idp.shibboleth.qmul.ac.uk/idp/shibboleth' })),
    makePreset('qub', 'zotero-fulltext-prefs-institution-preset-qub', ['saml', 'ezproxy'],
      presetValues({ name: "Queen's University Belfast", saml: 'https://qub.ac.uk/shibboleth', proxy: { loginHost: 'qub.idm.oclc.org', suffix: 'qub.idm.oclc.org' }, allHosts: true })),
    makePreset('reading', 'zotero-fulltext-prefs-institution-preset-reading', ['openathens'], openAthensValues('University of Reading', 'reading.ac.uk', '')),
    makePreset('sgul', 'zotero-fulltext-prefs-institution-preset-sgul', ['openathens'], openAthensValues("St George's, University of London", 'sgul.ac.uk', '')),
    makePreset('sheffield', 'zotero-fulltext-prefs-institution-preset-sheffield', ['saml'],
      presetValues({ name: 'University of Sheffield', saml: 'https://idp.shef.ac.uk/shibboleth' })),
    makePreset('southampton', 'zotero-fulltext-prefs-institution-preset-southampton', ['saml', 'ezproxy'],
      presetValues({ name: 'University of Southampton', saml: 'https://webauth.soton.ac.uk/shibboleth', proxy: { loginHost: 'soton.idm.oclc.org', suffix: 'soton.idm.oclc.org' }, allHosts: true })),
    makePreset('standrews', 'zotero-fulltext-prefs-institution-preset-standrews', ['saml'],
      presetValues({ name: 'University of St Andrews', saml: 'https://idp.st-andrews.ac.uk/shibboleth' })),
    makePreset('sussex', 'zotero-fulltext-prefs-institution-preset-sussex', ['saml'],
      presetValues({ name: 'University of Sussex', saml: 'https://idp.sussex.ac.uk/shibboleth' })),
    makePreset('ucl', 'zotero-fulltext-prefs-institution-preset-ucl', ['openathens', 'saml'],
      presetValues({ name: 'University College London', openathens: 'ucl.ac.uk', saml: 'https://shib-idp.ucl.ac.uk/shibboleth', idpHost: 'shib-idp.ucl.ac.uk' })),
    makePreset('uea', 'zotero-fulltext-prefs-institution-preset-uea', ['openathens', 'saml'],
      presetValues({ name: 'University of East Anglia', openathens: 'uea.ac.uk', saml: 'https://login.uea.ac.uk/entity', idpHost: 'login.uea.ac.uk' })),
    makePreset('warwick', 'zotero-fulltext-prefs-institution-preset-warwick', ['openathens', 'saml'],
      presetValues({ name: 'University of Warwick', openathens: 'warwick.ac.uk', saml: 'https://idp.warwick.ac.uk/idp/shibboleth', idpHost: 'idp.warwick.ac.uk' })),
    makePreset('york', 'zotero-fulltext-prefs-institution-preset-york', ['saml'],
      presetValues({ name: 'University of York', saml: 'https://shib.york.ac.uk/shibboleth' })),
  ];

  const PROBLEM_NO_METHOD = 'zotero-fulltext-prefs-problem-no-method';
  const PROBLEM_SAML = 'zotero-fulltext-prefs-problem-saml-entity-id';
  const PROBLEM_OPENATHENS_FORMAT = 'zotero-fulltext-prefs-problem-openathens-domain-format';
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
        { key: 'inst.use_saml', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-use-saml' },
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
        { key: 'inst.keep_session', widget: 'checkbox', label: 'zotero-fulltext-prefs-inst-keep-session' },
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
      // `section` (institution group only): 'picker' = the dropdown (always shown); the "Other" fields are shown only while the
      // picker says Other: 'custom' = the name, then one panel per sign-in method chosen with the method selector ('openathens',
      // 'saml', 'proxy'); 'advanced' = the rest, inside a collapsed <details> (hidden for None).
      // `format`: 'hostname' makes the pane reject a non-empty value that is not a valid host name; 'entityid' one that is not a
      // plausible SAML entityID.
      rows: [
        {
          key: 'institution.preset', widget: 'select', section: 'picker', label: 'zotero-fulltext-prefs-institution-preset',
          options: INSTITUTIONS.map((p) => ({ value: p.id, label: p.l10n })).concat([
            { value: PRESET_OTHER, label: 'zotero-fulltext-prefs-institution-preset-other' },
            { value: PRESET_NONE, label: 'zotero-fulltext-prefs-institution-preset-none' },
          ]),
        },
        { key: 'institution.name', widget: 'text', section: 'custom', label: 'zotero-fulltext-prefs-institution-name' },
        { key: 'institution.openathens_domain', widget: 'text', section: 'openathens', format: 'hostname', label: 'zotero-fulltext-prefs-institution-openathens-domain' },
        { key: 'institution.saml_entity_id', widget: 'text', section: 'saml', format: 'entityid', label: 'zotero-fulltext-prefs-saml-entity-id' },
        { key: 'institution.ezproxy_login_host', widget: 'text', section: 'proxy', format: 'hostname', label: 'zotero-fulltext-prefs-institution-ezproxy-login-host' },
        { key: 'institution.ezproxy_suffix', widget: 'text', section: 'proxy', format: 'hostname', label: 'zotero-fulltext-prefs-institution-ezproxy-suffix' },
        { key: 'institution.ezproxy_all_hosts', widget: 'checkbox', section: 'proxy', label: 'zotero-fulltext-prefs-proxy-all-hosts' },
        { key: 'institution.ezproxy_hosts', widget: 'list', section: 'advanced', label: 'zotero-fulltext-prefs-institution-ezproxy-hosts' },
        { key: 'institution.alma_base', widget: 'text', section: 'advanced', label: 'zotero-fulltext-prefs-institution-alma-base' },
        { key: 'institution.alma_inst', widget: 'text', section: 'advanced', label: 'zotero-fulltext-prefs-institution-alma-inst' },
        { key: 'institution.libkey_library_id', widget: 'number', section: 'advanced', label: 'zotero-fulltext-prefs-institution-libkey-library-id', min: LIMITS['institution.libkey_library_id'][0], max: LIMITS['institution.libkey_library_id'][1] },
        { key: 'institution.idp_hosts', widget: 'list', section: 'advanced', label: 'zotero-fulltext-prefs-institution-idp-hosts' },
        { key: 'institution.entra_tenant', widget: 'text', section: 'advanced', label: 'zotero-fulltext-prefs-institution-entra-tenant' },
        { key: 'institution.passthrough_hosts', widget: 'list', section: 'advanced', label: 'zotero-fulltext-prefs-institution-passthrough-hosts' },
        { key: 'institution.warmup_url', widget: 'text', section: 'advanced', label: 'zotero-fulltext-prefs-institution-warmup-url' },
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
   * (null when base is empty), libkey_library_id 0 -> null, entra_tenant '' -> null, saml_entity_id '' -> null, clamps ints into LIMITS,
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
    const samlEntityId = str('institution.saml_entity_id');

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
        use_saml: bool('inst.use_saml'),
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
        saml_entity_id: samlEntityId ? samlEntityId : null,
        ezproxy_all_hosts: bool('institution.ezproxy_all_hosts'),
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
   * @returns {string[]} l10n ids of the problems, e.g. 'zotero-fulltext-prefs-problem-no-method' (institutional access is on but
   *   neither OpenAthens, Shibboleth nor a library proxy is set up and switched on)
   */
  function validate(config) {
    const c = config || {};
    const inst = c.inst || {};
    const institution = c.institution || {};
    const oa = c.oa || {};
    const out = [];
    const domain = String(institution.openathens_domain || '').trim();
    if (inst.enabled && activeMethods(c).length === 0) out.push(PROBLEM_NO_METHOD);
    if (inst.enabled && domain && !parseHostname(domain).ok) out.push(PROBLEM_OPENATHENS_FORMAT);
    const entityId = String(institution.saml_entity_id || '').trim();
    if (inst.enabled && entityId && !isEntityId(entityId)) out.push(PROBLEM_SAML);
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

  // --- institution picker helpers (pure; the pane and the tests drive them) ---------------------------------------------

  /** Host name syntax (LDH labels, no scheme/port/path/email). A single label is allowed: OpenAthens has domains such as "nhs". */
  const HOSTNAME_RE = /^(?=.{1,253}$)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*$/;

  /**
   * Normalise (normHost: lower-case, scheme/path/trailing dot dropped) and check a host or OpenAthens domain.
   * @param {*} raw @returns {{ok: boolean, value: string}} value is the normalised text (also when not ok); '' is not ok
   */
  function parseHostname(raw) {
    const value = raw === undefined || raw === null ? '' : normHost(raw);
    return { ok: HOSTNAME_RE.test(value), value };
  }

  /** @returns {object|null} the INSTITUTIONS entry with this id */
  function findPreset(id) {
    return INSTITUTIONS.find((p) => p.id === id) || null;
  }

  /**
   * Which picker entry the CURRENT institution prefs correspond to: the id of the preset whose normalised EngineConfig
   * institution equals the current one, else 'other'. (Independent of institution.preset, so hand-edited prefs and prefs from
   * before the picker existed still resolve.)
   * @param {(key: string) => any} [get] pref reader, default getPref
   */
  function matchPreset(get) {
    const mine = JSON.stringify(buildEngineConfig(get).institution);
    for (const p of INSTITUTIONS) {
      if (JSON.stringify(buildEngineConfig((key) => p.values[key]).institution) === mine) return p.id;
    }
    return PRESET_OTHER;
  }

  /**
   * What the picker should show: 'none' while institutional access is off (inst.enabled = false, which is also what choosing
   * None writes), else matchPreset(). The data decides, not the stored institution.preset, so the picker can never disagree
   * with what the engine will use; applyPreset() and the pane keep institution.preset in step with this value.
   */
  function effectivePreset(get) {
    let enabled;
    try { enabled = (get || getPref)('inst.enabled'); } catch (e) { enabled = undefined; }
    return toBool(enabled === undefined || enabled === null ? DEFAULTS['inst.enabled'] : enabled, true) ? matchPreset(get) : PRESET_NONE;
  }

  /**
   * Apply a picker choice by writing prefs.
   *  - a preset id: every institution.* pref, then institution.preset, and inst.enabled = true;
   *  - 'none': inst.enabled = false and institution.preset = 'none' (the institution.* prefs are left alone);
   *  - 'other': inst.enabled = true; the institution.* prefs are blanked (only the generic OpenAthens sign-in hosts and the
   *    LWW passthrough stay) unless they are already custom, so a previous institution's EZproxy/Alma/Entra values cannot leak
   *    into the new one.
   * @param {string} id @param {(key: string, value: *) => void} [set] default setPref @param {(key: string) => any} [get] default getPref
   * @returns {{id: string, enabled: boolean, name: string}} name = the institution name now stored ('' for none/other-blank)
   * @throws {Error} unknown id
   */
  function applyPreset(id, set, get) {
    const write = set || setPref;
    if (id === PRESET_NONE) {
      write('inst.enabled', false);
      write('institution.preset', PRESET_NONE);
      return { id, enabled: false, name: '' };
    }
    let values = null;
    if (id === PRESET_OTHER) {
      if (matchPreset(get) !== PRESET_OTHER) values = openAthensValues('', '', '');
    } else {
      const preset = findPreset(id);
      if (!preset) throw new Error('unknown institution preset ' + id);
      values = preset.values;
    }
    if (values) for (const key of INSTITUTION_KEYS) write(key, values[key]);
    // A preset switches on the methods it offers (a user may have unticked one earlier); the others are left as they are.
    const chosen = id === PRESET_OTHER ? null : findPreset(id);
    if (chosen) for (const method of chosen.methods) write(METHOD_USE_PREFS[method], true);
    write('institution.preset', id);
    write('inst.enabled', true);
    const name = values ? values['institution.name'] : String((get || getPref)('institution.name') || '');
    return { id, enabled: true, name };
  }

  /** The institution name for labels ('' when none is set). */
  function institutionName(get) {
    try { return String((get || getPref)('institution.name') || '').trim(); } catch (e) { return ''; }
  }

  /** Address of the unauthenticated redirector probe for an OpenAthens domain (the target is a public Nature article DOI). */
  function redirectorTestUrl(domain) {
    return 'https://go.openathens.net/redirector/' + domain + '?url=' + encodeURIComponent('https://doi.org/10.1038/nature12373');
  }

  /**
   * Read the answer of redirectorTestUrl() (requested WITHOUT following redirects): 3xx or 201 = the organisation is known;
   * 200 with an "OpenAthens" page ("Please wait...") = known; 403 = unknown organisation (research/04 section 4.3); anything
   * else, or a 200 that is not an OpenAthens page (captive portal), could not be judged.
   * @param {number} status @param {string} [text] response body
   * @returns {'recognised'|'unknown'|'failed'}
   */
  function classifyRedirectorResponse(status, text) {
    if (status === 403) return 'unknown';
    if (status === 201 || (status >= 300 && status < 400)) return 'recognised';
    if (status === 200 && /openathens/i.test(String(text || ''))) return 'recognised';
    return 'failed';
  }

  // --- access methods: OpenAthens / Shibboleth (SAML) / library proxy (EZproxy) -------------------------------------------

  /** Plausible SAML entityID: an http(s) URL or a urn:, no whitespace or control characters, at most 1024 characters. */
  function isEntityId(raw) {
    const s = typeof raw === 'string' ? raw : '';
    return s.length > 0 && s.length <= 1024 && !/[\s\u0000-\u001f\u007f]/.test(s) && /^(?:https?:\/\/[^/]+|urn:\S+)/i.test(s);
  }

  /**
   * The sign-in methods that are configured AND switched on in an EngineConfig, in METHODS order. A missing use flag counts as on
   * (partial configs), a method without its details does not count.
   * @param {object} config EngineConfig (any group may be missing) @returns {Array<'openathens'|'saml'|'ezproxy'>}
   */
  function activeMethods(config) {
    const c = config || {};
    const inst = c.inst || {};
    const institution = c.institution || {};
    const on = (key) => inst[key] !== false;
    const out = [];
    if (on('use_openathens') && String(institution.openathens_domain || '').trim()) out.push('openathens');
    if (on('use_saml') && String(institution.saml_entity_id || '').trim()) out.push('saml');
    if (on('use_ezproxy') && institution.ezproxy) out.push('ezproxy');
    return out;
  }

  /** A comma list of identity-provider hosts with the host of this entityID added (unchanged for a urn: id or a host already listed). */
  function withIdpHost(list, entityId) {
    const hosts = parseList(list, normHost);
    const host = entityIdHost(entityId);
    if (host && !hosts.includes(host)) hosts.push(host);
    return hosts.join(',');
  }

  /** activeMethods() of the CURRENT prefs. @param {(key: string) => any} [get] pref reader, default getPref */
  function currentMethods(get) {
    return activeMethods(buildEngineConfig(get));
  }

  // --- Shibboleth: identity-provider search in the SeamlessAccess metadata service ------------------------------------------

  const MDQ_ENTITIES_URL = 'https://md.seamlessaccess.org/entities/';
  const SAML_QUERY_MIN = 3;
  const SAML_RESULT_MAX = 30;
  const UK_FEDERATION = 'http://ukfederation.org.uk';

  /** @returns {boolean} true when the typed search text is long enough to send (trimmed, at least 3 characters) */
  function samlQueryOk(raw) {
    return String(raw === undefined || raw === null ? '' : raw).trim().length >= SAML_QUERY_MIN;
  }

  /** Address of the public, unauthenticated name search (JSON). The search text is the only thing sent. */
  function samlSearchUrl(query) {
    return MDQ_ENTITIES_URL + '?q=' + encodeURIComponent(String(query === undefined || query === null ? '' : query).trim());
  }

  /**
   * Turn the JSON answer of samlSearchUrl() into picker entries: identity providers only (type "idp", a plausible entityID),
   * no test/hidden ones (hidden "true" or the REFEDS hide-from-discovery category), no duplicates, UK federation members first
   * (otherwise the answer's order), at most 30. Never throws: anything unusable gives [].
   * @param {string|Array} json the response body or the parsed array
   * @returns {Array<{entityID: string, title: string, domain: string, ukFederation: boolean}>}
   */
  function parseSamlSearch(json) {
    let list = json;
    if (typeof json === 'string') {
      try { list = JSON.parse(json); } catch (e) { return []; }
    }
    if (!Array.isArray(list)) return [];
    const seen = new Set();
    const out = [];
    for (const e of list) {
      if (!e || typeof e !== 'object') continue;
      if (e.type !== undefined && e.type !== 'idp') continue;
      if (e.hidden === true || e.hidden === 'true') continue;
      const categories = Array.isArray(e.entity_category) ? e.entity_category : [];
      if (categories.some((x) => /hide-from-discovery/.test(String(x)))) continue;
      const entityID = typeof e.entityID === 'string' ? e.entityID : (typeof e.entity_id === 'string' ? e.entity_id : '');
      if (!isEntityId(entityID) || seen.has(entityID)) continue;
      seen.add(entityID);
      const en = e.title_langs && typeof e.title_langs.en === 'string' ? e.title_langs.en : '';
      const title = String(typeof e.title === 'string' && e.title.trim() ? e.title : en).trim() || entityID;
      const domain = String(e.domain || e.scope || '').split(',')[0].trim();
      const authorities = Array.isArray(e.registrationAuthority) ? e.registrationAuthority : [];
      out.push({ entityID, title, domain, ukFederation: authorities.includes(UK_FEDERATION) });
    }
    const ordered = out.filter((x) => x.ukFederation).concat(out.filter((x) => !x.ukFederation));
    return ordered.slice(0, SAML_RESULT_MAX);
  }

  // --- library proxy (EZproxy) -------------------------------------------------------------------------------------------

  /**
   * Parse a pasted EZproxy start address such as https://ezproxy.example.ac.uk/login?url= (also ?qurl=, with or without the
   * scheme, with or without a target after the "=", or just the host). The engine needs the host the sign-in lives on and the
   * suffix proxied hosts get ("www-nature-com.<suffix>"). For a hosted "login.<name>.idm.oclc.org" the suffix is the host without
   * "login."; every other host is its own suffix. Only the standard "/login" path, no port and no credentials are accepted.
   * @param {*} raw
   * @returns {{ok: true, loginHost: string, suffix: string, param: string, prefix: string}
   *   | {ok: false, error: string}} error = an l10n id of the pane
   */
  function parseProxyPrefix(raw) {
    const text = String(raw === undefined || raw === null ? '' : raw).trim();
    if (!text) return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid' };
    const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(text) ? text : 'https://' + text;
    const m = /^([a-z][a-z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(?:\?([^#]*))?(?:#.*)?$/i.exec(withScheme);
    if (!m || !/^https?$/i.test(m[1])) return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid' };
    const authority = m[2];
    if (authority.includes('@')) return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid-credentials' };
    if (/:\d*$/.test(authority)) return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid-port' };
    const host = parseHostname(authority);
    if (!host.ok || !host.value.includes('.')) return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid' };
    const path = m[3].replace(/\/+$/, '').toLowerCase();
    if (path !== '' && path !== '/login') return { ok: false, error: 'zotero-fulltext-prefs-proxy-invalid-path' };
    let param = '';
    for (const pair of String(m[4] || '').split('&')) {
      const name = pair.split('=')[0].toLowerCase();
      if (name === 'url' || name === 'qurl') { param = name; break; }
    }
    const loginHost = host.value;
    const suffix = /^login\..+\..+/.test(loginHost) ? loginHost.slice('login.'.length) : loginHost;
    return { ok: true, loginHost, suffix, param: param || 'url', prefix: proxyPrefix(loginHost) };
  }

  /** The start address shown for a login host: https://<host>/login?url= */
  function proxyPrefix(loginHost) {
    return 'https://' + loginHost + '/login?url=';
  }

  /** Address of the unauthenticated proxy probe: the proxy's login page asked to open a public publisher home page. */
  function proxyTestUrl(loginHost) {
    return 'https://' + loginHost + '/login?qurl=' + encodeURIComponent('https://www.nature.com/');
  }

  /**
   * Read the answer of proxyTestUrl() (requested WITHOUT following redirects): a 200 page that is an EZproxy or Shibboleth
   * sign-in page, or a redirect to a /login, EZproxy, OCLC-hosted or SSO address = a proxy; 404 or a 200 that is something else =
   * not an EZproxy there; anything else could not be judged. A page is never submitted.
   * @param {number} status @param {string} [text] response body @param {string} [location] Location header of a redirect
   * @returns {'recognised'|'unknown'|'failed'}
   */
  function classifyProxyResponse(status, text, location) {
    if (status === 200) {
      return /ezproxy|Shibboleth Authentication Request|idm\.oclc\.org/i.test(String(text || '')) ? 'recognised' : 'unknown';
    }
    if (status >= 300 && status < 400) {
      return /\/login|ezproxy|idm\.oclc\.org|shibboleth|saml|sso/i.test(String(location || '')) ? 'recognised' : 'failed';
    }
    if (status === 404) return 'unknown';
    return 'failed';
  }

  ZFT.config = {
    DEFAULTS, PANE_LAYOUT, PLUGIN_ONLY, KCL_EZPROXY_HOSTS, LIMITS,
    typeOf, getPref, setPref, parseList, buildEngineConfig, validate, problems,
    INSTITUTIONS, INSTITUTION_KEYS, PRESET_OTHER, PRESET_NONE, PRESET_DEFAULT,
    parseHostname, findPreset, matchPreset, effectivePreset, applyPreset, institutionName, redirectorTestUrl,
    classifyRedirectorResponse,
    METHODS, METHOD_USE_PREFS, isEntityId, entityIdHost, withIdpHost, activeMethods, currentMethods,
    samlQueryOk, samlSearchUrl, parseSamlSearch, parseProxyPrefix, proxyPrefix, proxyTestUrl, classifyProxyResponse,
  };
})(this);
