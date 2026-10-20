# core-ids-oa: identifiers, OA-API parsers, candidate ranking

Package id `core-ids-oa`. Implementer: Sonnet. Depends on: nothing (the skeleton compiles and `cargo test -p fulltext-core` is green).
Read first: `docs/design/core.md` sections 2, 3.1, 3.2; `docs/design/contracts/model.rs` (frozen); `research/05-oa-sources.md`
sections 2.1-2.11, 5, 8; `research/SUMMARY.md` section (d); `fixtures/oa/README.md` and `fixtures/oa/manifest.json`.

## Files you own (edit only these)
`crates/fulltext-core/src/ids.rs`, `crates/fulltext-core/src/oa/**` (all 12 files), `crates/fulltext-core/src/rank.rs`,
`crates/fulltext-core/tests/ids.rs`, `crates/fulltext-core/tests/oa_fixtures.rs`, `crates/fulltext-core/tests/rank.rs`,
and hand-made inputs under `crates/fulltext-core/tests/data/oa/`. Read-only for you: everything else, especially
`model.rs`, `tests/common/mod.rs` (use `mod common;` and `common::oa_fixture("openalex__gold.json") -> (RespMeta, Vec<u8>)`).
Environment: set `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-ids-oa` for every cargo command.

## Rules for all code
No `unwrap`, `expect`, `panic!`, indexing or string slicing outside tests (clippy denies them; use `get`, `?`, `let ... else`).
Every function is total: any bytes, any status. Parse JSON into `serde_json::Value` and read with `.get()` / `.as_str()`;
do NOT use strict typed structs (APIs add and null out fields). Keep the published signatures; you may add private helpers
and new `pub` items, and you replace the placeholder bodies and the doc lines marked SKELETON.
Do not edit `model.rs`. Remove every `SKELETON` marker and every `not implemented` string in your files.

## 1. ids.rs (published signatures, keep)
```rust
pub fn normalise(fields: &ItemFields) -> Ids;
pub fn clean_doi(raw: &str) -> Option<String>;            // DONE in the skeleton, keep behaviour, add tests only
pub fn clean_pmid(raw: &str) -> Option<String>;
pub fn clean_pmcid(raw: &str) -> Option<String>;
pub fn clean_arxiv(raw: &str) -> Option<String>;
pub fn arxiv_from_doi(doi: &str) -> Option<String>;       // implemented in terms of clean_arxiv
pub fn is_rxiv_doi(doi: &str) -> bool;                     // DONE
pub fn extra_field(extra: &str, key: &str) -> Option<String>;
pub fn apply_learned(ids: &mut Ids, learned: &LearnedIds, source: &str) -> bool;
pub fn same_doi(a: &str, b: &str) -> bool;                // DONE
```
Behaviour:
* `clean_pmid`: accepts `"19621072"`, `"PMID: 19621072"`, `"pmid:19621072"`, `pubmed.ncbi.nlm.nih.gov/19621072/`; 1-9 digits; no leading zeros kept (`"007"` -> `"7"`); anything else `None`.
* `clean_pmcid`: `"PMC2707599"`, `"pmc2707599"`, `"PMC2707599.1"`, `/pmc/articles/PMC2707599/`, `"PMCID: PMC2707599"` -> `"PMC2707599"`. Digits only (`"2707599"`) are accepted. A DOI or other text -> `None`.
* `clean_arxiv`: strips `arXiv:` prefix (any case), version suffix `v\d+`, `https://arxiv.org/abs|pdf/` prefix, `.pdf` suffix, trailing ` [cs.CL]` category. New-style `\d{4}\.\d{4,5}`, old-style `[a-z-]+(\.[A-Z]{2})?/\d{7}`. `arxiv_from_doi("10.48550/arxiv.1706.03762") == Some("1706.03762")`.
* `extra_field(extra, key)`: Zotero Extra is `Key: value` lines, case-insensitive key, first match wins, value trimmed; `"DOI: 10.1/x\nPMID: 5"`. Also accept `Key:value` without space. Ignore lines inside prose (a line must START with the key).
* `normalise`: DOI precedence `fields.doi`, Extra `DOI:`, a DOI inside `fields.url` (`doi.org`, `dx.doi.org`), `archive_id` when it cleans to a DOI. PMID/PMCID from fields then Extra. arXiv from: a `10.48550` DOI, `archive_id` (`arXiv:1706.03762`), an `arxiv.org/abs|pdf` URL, Extra `arXiv:`. `rxiv` iff the DOI starts with `10.1101/`. `url` kept only when it parses as http(s). `sources` maps each found id name (`doi`, `pmid`, `pmcid`, `arxiv`) to where it came from: `field`, `extra`, `url`, `archive_id`, `doi` (arXiv derived from the DOI). Empty/whitespace fields are ignored. Never invent an id.
* `apply_learned`: copy `doi/pmid/pmcid/arxiv` from `learned` into `ids` only where `ids` has none (cleaned with the same functions; invalid values ignored); set `rxiv` when a learned DOI starts `10.1101/`; record `sources[name] = source`; return whether anything changed.

Tests (`tests/ids.rs`): table-driven vectors for every cleaner (valid, invalid, hostile: 10 KB junk, NULs, `10.`-only, unicode), at least 10 `normalise` cases including a journalArticle with DOI in Extra, a preprint with `archive_id = "arXiv:1706.03762v5"` and `10.48550` DOI, a medRxiv DOI, a URL-only item (`https://doi.org/10.1002/bjs.11646`), an item with every field empty (all `None`, `rxiv` false).

## 2. oa/ parsers
Common contract (module docs in `oa/mod.rs`; helpers `gate`, `empty`, `unparsed` are done): `parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection`.
1. Call `super::gate(source, resp)` first. Then parse the body; non-JSON/garbage bodies give `Unparseable` with a short `note` (never panic, never echo the body).
2. Status semantics: `Found` = the record exists AND yields at least one candidate or learned id; `Empty` = the API answered but has nothing usable (zero hits, `[]`, closed work, "no posts found", per-record error); `NotFound` = HTTP 404/410 only (from `gate`) or S3 `NoSuchKey`; `Throttled` = 429 or 503+Retry-After (from `gate`); `Unparseable` = could not read the body or an unexpected shape. `learned` and `oa_status` are filled in every status when the body states them.
3. Candidate fields: `url` absolute https (upgrade `http://`; drop non-http(s) and empty strings), `kind` `Pdf` only for a direct file URL else `Landing`, `source` = the parser's `Source`, `version` via `oa::version_from_str`, `license` lower-case (`cc-by`; `"CC BY"` -> `cc-by`; `TDM` stays `tdm`), `host_kind` via `oa::host_kind(url)` unless the API states it (`source.type == "repository"` etc.), `record_doi` = the DOI of the RECORD (lower-case, via `ids::clean_doi`), `landing_url` for a Pdf candidate when the same location has a landing page, `signal` = `"<source>:<what>"` (for example `openalex:locations[2].pdf_url`, `europepmc:getpdf`, `biorxiv:early_path`), `needs_browser` true only for hosts known to be walled (`pmc.ncbi.nlm.nih.gov/articles/*/pdf/*`, `europepmc.org/*?pdf=render`). Leave `score`/`tier` 0; `rank` sets them.
4. Compare DOIs case-insensitively (`ids::same_doi`). Dedupe URLs inside one response by `urls::canonical_key`.
5. Implement `oa::version_from_str`, `oa::host_kind`, `oa::s3_to_https` with unit tests (rules in their doc comments).

Per source (fixture names are in `fixtures/oa`; verify each expectation against the file before asserting, and assert STRUCTURE not whole URL lists because live data drifts):

| module | input | rules | fixture expectations |
|---|---|---|---|
| `openalex` | `GET /works/doi:<doi>` JSON (also `/works/pmid:`, list endpoints with `results[]`: use the first result whose DOI/PMID matches `requested`, else the first) | candidates from EVERY `locations[]` entry with `is_oa == true` (`pdf_url` -> Pdf, `landing_page_url` -> Landing), plus `best_oa_location`/`primary_location` if OA and not already present, plus `open_access.oa_url`. Skip locations with no URL. `record_doi` = work `doi`. `learned.pmid/pmcid` from `ids.*` (URL forms: clean them). `oa_status` from `open_access.oa_status`. Closed work: `Empty` + `oa_status "closed"`, no candidates. `content_urls.pdf` / `has_content.pdf` -> a Pdf candidate flagged `signal = "openalex:content_api"` (paid; rank assigns `PAID_API`) | `gold`: Landing at journals.plos.org, no Pdf, `learned.pmcid == "PMC2707599"`; `green`: a Pdf at arxiv.org and a PMC location, `oa_status == "bronze"`; `arxiv`: `record_doi` differs from the request (merged work); `medrxiv`: record is the PLOS ONE version; `paywalled`: Empty/closed; `nonexistent-404.html`: NotFound; `by-pmcid-404.html`: NotFound; `filter-doi-select`/`title-search`: list shape handled; `openalex-content__no-key.json` (401): Unparseable |
| `europepmc` | search `core`/`lite` JSON | see module doc. `resultList.result[]`; pick the entry whose `doi` matches. `fullTextUrlList.fullTextUrl[]`: `documentStyle` pdf -> Pdf, html/doi -> Landing; keep only `availabilityCode` `OA` or `F`. If `pmcid` present and `isOpenAccess == "Y"` add the `api/getPdf?pmcid=` Pdf candidate (signal `europepmc:getpdf`); never emit `?pdf=render` as Pdf unless nothing else (then `needs_browser`). `source == "PPR"` -> host_kind Preprint | `gold`: pmcid + getPdf; `green`: pmcid PMC4221854; `medrxiv`: PPR record; `nonexistent`: Empty (hitCount 0); `paywalled`: Empty or Found with only subscription links dropped; `by-extid-pmid`, `by-pmcid`, `batch-or-lite`: shapes handled; `europepmc-web__pdf-render-cloudflare-challenge.html` (403): `gate` -> Unparseable |
| `biorxiv` | `details` and `pubs` JSON | see module doc; `details` -> latest version; both early-path and canonical URL, `host_kind Preprint`, `version Submitted`; `server` from the record (`medrxiv`/`biorxiv`); date `YYYY-MM-DD` -> path `YYYY/MM/DD`; `pubs` -> `note = "published_doi:<doi>"` when `published_doi` present | `medrxiv-details`: early URL contains `/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf`; `medrxiv-pubs`: published_doi note; `nonexistent`: Empty |
| `pmc_s3` | three shapes (module doc) | `metadata/PMC<n>.<v>.json`: `pdf_url` `s3://pmc-oa-opendata/...?md5=...` -> `https://pmc-oa-opendata.s3.amazonaws.com/...`, `license_code` lower-case, version from the object (`.1` etc. is NOT always 1, keep the key's version). AAM with `license_code == "TDM"` or no `pdf_url` -> Empty (note `no_pdf`). ListObjectsV2: pick highest version `.pdf` key; `NoSuchKey` -> NotFound. idconv: `records[]`, a record with `status == "error"` -> Empty; else learned ids | `meta-gold`: Pdf candidate on `pmc-oa-opendata.s3.amazonaws.com`; `meta-green-aam`: Empty; `list-PMC2707599`: Found, `list-PMC4221854`: Empty; `meta-nonexistent.xml` (404): NotFound; `ncbi-idconv__*`: learned pmcid/pmid/doi; `ncbi-pmc-oa__retired-404.html`: NotFound; `pmc-web__pdf-pow-interstitial.html` (200 HTML): Unparseable (that file is for core-page) |
| `arxiv` | Atom | module doc | `export-api`: Pdf `https://arxiv.org/pdf/1706.03762`; `export-api-nonexistent`: Empty |
| `unpaywall` | JSON | module doc; `is_oa == false` -> Empty | `gold`, `green`, `medrxiv`, `paywalled`; 404 HTML -> NotFound; `error-no-email`/`error-test-example-email` (422) -> Unparseable note `bad_email` |
| `semantic_scholar` | JSON | module doc; also batch array (first non-null) | `green`, `gold`, `paywalled`; 404 JSON -> NotFound; `error-429-unauthenticated` -> Throttled; `batch*` |
| `core_api` | JSON | module doc; all candidates `Aggregator`, version Unknown | `discover-*`, `output-green-no-key`, `output-paywalled-hit-no-key` (404 `text/html`) -> NotFound, `search-no-key-500` -> Unparseable |
| `crossref` | JSON (`Source::Doi`) | module doc; `link[]` with `content-type == "application/pdf"`; `intended-application == "text-mining"` -> `signal = "crossref:link_tdm"`; `resource.primary.URL` -> Landing; `license[].URL` mapped to a licence slug when it is a Creative Commons URL; `posted-content`/`preprint` -> `oa_status = "preprint"`; learned `doi` never overwritten | `gold`, `green` (has TDM pdf link), `medrxiv`, `paywalled`; `nonexistent-404.txt` -> NotFound |
| `alma` | XML | module doc. Build hand-made inputs in `tests/data/oa/alma-*.xml` from the shape in `research/04-kcl-institutional-access.md` section 4.5 (one held, one not held, one with `Is_free` 1) | held -> Found with Landing candidate `Source::Alma`; none -> Empty |
| `zotero_oa` | JSON array | `parse` and `candidates_from_hit` | `gold`/`green`/`medrxiv` arrays; `[]` -> Empty (`arxiv`, `paywalled`, `nonexistent`, `invalid-doi`) |

DOAJ and DataCite fixtures are not parsed in v1; ignore them. Write XML scanning as small tolerant helpers private to the module (no XML crate).

## 3. rank.rs (published signatures, keep `new_candidate`)
```rust
pub fn merge(pool: &mut Vec<Candidate>, more: Vec<Candidate>);
pub fn score(c: &Candidate, prefs: &RankPrefs) -> i32;
pub fn rank(pool: Vec<Candidate>, prefs: &RankPrefs) -> Vec<Candidate>;
```
`rank` must, in order: (1) drop candidates with an empty/non-http(s) URL; (2) normalise aliases: `http` -> `https`, `www.ncbi.nlm.nih.gov/pmc/articles/PMC<n>/...` and `.../pmc/articles/<n>` -> `https://pmc.ncbi.nlm.nih.gov/articles/PMC<n>/`, `arxiv.org/abs/<id>` -> `arxiv.org/pdf/<id>` (kind Pdf); (3) merge duplicates by `urls::canonical_key` (query INCLUDED): best version per `prefs.version_order` (empty = published, accepted, submitted, unknown), keep a licence if any has one, lower non-zero tier, higher score, first `source`; (4) assign `tier` (constants in `rank::tier`): host `pmc-oa-opendata.s3.amazonaws.com` -> PMC_S3; `europepmc.org/api/getPdf` -> EUROPEPMC_GETPDF; `arxiv.org/pdf` -> ARXIV; `signal` containing `biorxiv:early_path`/`biorxiv:canonical` or rxiv hosts -> RXIV_EARLY; `content.openalex.org` -> PAID_API; `needs_browser` or `europepmc.org/*?pdf=render` -> BROWSER_ONLY; `Pdf` on `HostKind::Publisher` -> DIRECT_PUBLISHER, other `Pdf` -> DIRECT_REPOSITORY; `Landing` -> LANDING; (5) drop `europepmc.org/*?pdf=render` and `pmc.ncbi.nlm.nih.gov/articles/*/pdf/*` when a PMC_S3 or EUROPEPMC_GETPDF candidate exists, and drop `core.ac.uk/*` when any non-Aggregator Pdf exists; (6) compute `score` (suggested: base 100, version bonus published +20 / accepted +10, Creative Commons licence +5, publisher host +10, aggregator -30, `record_doi` present and different from `prefs.requested.doi` with `host_kind` Aggregator/Other and no recognised source -40, `needs_browser` -10; any deterministic scheme is fine if the tests below hold); (7) stable sort by (`tier` ascending, `score` descending, version order, original index); (8) truncate to `max_candidates` when non-zero.

Tests (`tests/rank.rs`), at least: tier assignment for each tier constant; the PLOS case (`...file?id=X&type=printable` and `...type=manuscript` stay separate); `http`/`https` duplicate merge; PMC and arXiv alias rewriting; the OpenAlex arXiv fixture (a Pdf at `arxiv.org/pdf/1706.03762` must outrank the `record_doi 10.65215/...` mirror, which stays in the list); the medRxiv case (pool = OpenAlex `medrxiv` + bioRxiv `medrxiv-details`): the PLOS ONE publisher copy (its `record_doi` differs from the request but its source is a recognised publisher) is kept and tiered `DIRECT_PUBLISHER` or `LANDING` by its kind, and the early-path PDF is kept with tier `RXIV_EARLY`; `version_order = [Accepted, Published]` flips same-tier order; `max_candidates`; idempotence (`rank(rank(x)) == rank(x)`); property test over shuffled input (deterministic seed) giving identical output.

## 4. Acceptance (all must pass)
```
set CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-ids-oa
cargo test -p fulltext-core --test ids --test oa_fixtures --test rank
cargo test -p fulltext-core
cargo clippy -p fulltext-core --all-targets -- -D warnings
git grep --no-index -n -E "SKELETON|not implemented" -- crates/fulltext-core/src/ids.rs crates/fulltext-core/src/oa crates/fulltext-core/src/rank.rs
```
The last command must print nothing (exit code 1). `tests/oa_fixtures.rs` must include a coverage test asserting that every manifest file whose name starts with `openalex__`, `europepmc__`, `biorxiv-api__`, `pmc-s3__`, `ncbi-idconv__`, `arxiv__`, `unpaywall__`, `semanticscholar__`, `core__`, `crossref__`, `zotero-oa__` is fed through `oa::parse` by some test (a table of file -> `Source`).
