// zotero-fulltext default preferences. Loaded by Zotero's plugin manager through a `pref(name, value)` shim
// (xpcom/plugins.js setDefaultPrefs): only booleans, strings and integers are valid. MUST mirror
// ZFT.config.DEFAULTS in content/config.js exactly (tests/js/prefs.test.mjs compares them).
// Lists are comma-separated strings. KCL (King's College London) institution defaults; see research/04.

// --- open-access sources (EngineConfig.oa)
pref("extensions.zotero-fulltext.oa.openalex", true);
pref("extensions.zotero-fulltext.oa.openalex_key", "");
pref("extensions.zotero-fulltext.oa.europepmc", true);
pref("extensions.zotero-fulltext.oa.biorxiv", true);
pref("extensions.zotero-fulltext.oa.pmc_s3", true);
pref("extensions.zotero-fulltext.oa.europepmc_get_pdf", true);
pref("extensions.zotero-fulltext.oa.medrxiv_early", true);
pref("extensions.zotero-fulltext.oa.arxiv", true);
pref("extensions.zotero-fulltext.oa.zotero_oa", true);
pref("extensions.zotero-fulltext.oa.unpaywall_email", "");
pref("extensions.zotero-fulltext.oa.semantic_scholar_key", "");
pref("extensions.zotero-fulltext.oa.core_key", "");
pref("extensions.zotero-fulltext.oa.version_order", "published,accepted,submitted");

// --- institutional access behaviour (EngineConfig.inst)
pref("extensions.zotero-fulltext.inst.enabled", true);
pref("extensions.zotero-fulltext.inst.try_direct", true);
pref("extensions.zotero-fulltext.inst.use_openathens", true);
pref("extensions.zotero-fulltext.inst.use_ezproxy", true);
pref("extensions.zotero-fulltext.inst.use_saml", true);
pref("extensions.zotero-fulltext.inst.check_alma", true);
pref("extensions.zotero-fulltext.inst.login_mode", "window");
pref("extensions.zotero-fulltext.inst.auto_open_login", true);
pref("extensions.zotero-fulltext.inst.keep_session", true);
pref("extensions.zotero-fulltext.inst.login_timeout_sec", 300);
pref("extensions.zotero-fulltext.inst.group_libraries", "ask");
pref("extensions.zotero-fulltext.inst.confirm_above", 10);
pref("extensions.zotero-fulltext.inst.batch_max", 20);
pref("extensions.zotero-fulltext.inst.hourly_cap", 30);
pref("extensions.zotero-fulltext.inst.daily_cap", 80);
pref("extensions.zotero-fulltext.inst.mb_per_hour", 300);
pref("extensions.zotero-fulltext.inst.min_gap_sec", 5);
pref("extensions.zotero-fulltext.inst.challenge_windows", true);

// --- run behaviour (EngineConfig.run)
pref("extensions.zotero-fulltext.run.item_concurrency", 4);
pref("extensions.zotero-fulltext.run.hedge", true);
pref("extensions.zotero-fulltext.run.pdf_max_mb", 100);

// --- institution (EngineConfig.institution): King's College London by default; institution.preset is the picker choice
pref("extensions.zotero-fulltext.institution.preset", "kcl");
pref("extensions.zotero-fulltext.institution.name", "King's College London");
pref("extensions.zotero-fulltext.institution.openathens_domain", "kcl.ac.uk");
pref("extensions.zotero-fulltext.institution.ezproxy_login_host", "login.kcl.idm.oclc.org");
pref("extensions.zotero-fulltext.institution.ezproxy_suffix", "kcl.idm.oclc.org");
pref("extensions.zotero-fulltext.institution.ezproxy_hosts", "doi.org,dx.doi.org,www.bmj.com,www.webofscience.com,www.liebertpub.com,www.jneurosci.org,www.psychiatryonline.org,psychiatryonline.org,www.atsjournals.org,www.ajronline.org,www.jospt.org,www.jove.com,www.elgaronline.com,infoweb.newsbank.com,global.factiva.com,www.portal.euromonitor.com,institutions.newscientist.com,www.lrb.co.uk,libguides.kcl.ac.uk");
pref("extensions.zotero-fulltext.institution.alma_base", "https://librarysearch.kcl.ac.uk");
pref("extensions.zotero-fulltext.institution.alma_inst", "44KCL_INST");
pref("extensions.zotero-fulltext.institution.libkey_library_id", 286);
pref("extensions.zotero-fulltext.institution.idp_hosts", "login.microsoftonline.com,login.live.com,login.openathens.net,kclidp.kcl.ac.uk");
pref("extensions.zotero-fulltext.institution.entra_tenant", "8370cf14-16f3-4c16-b83c-724071654356");
pref("extensions.zotero-fulltext.institution.passthrough_hosts", "journals.lww.com");
pref("extensions.zotero-fulltext.institution.warmup_url", "");
pref("extensions.zotero-fulltext.institution.saml_entity_id", "https://kclidp.kcl.ac.uk/idp/shibboleth");
pref("extensions.zotero-fulltext.institution.ezproxy_all_hosts", false);

// --- plugin-only
pref("extensions.zotero-fulltext.debug.trace", false);
pref("extensions.zotero-fulltext.debug.inspect_in_worker", true);
