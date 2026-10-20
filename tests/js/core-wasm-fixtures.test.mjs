// The recorded fixtures through the REAL plugin/content/fulltext.wasm (built by `cargo xtask build`).
// Owned by work package core-integration; deliberately self-contained (the instantiate() helper below is a copy of
// the one in abi.test.mjs, nothing is imported across test files).
// Run: node --test tests/js/core-wasm-fixtures.test.mjs
//
// What this proves that the native tests cannot: the same analysis, reached through the byte-level ABI exactly as
// the plugin calls it (UTF-8 JSON meta, raw body bytes, packed u64 result, JS frees everything), gives the same
// answers on the recorded pages and API responses, binary bodies are judged from head ++ tail, and calling it
// hundreds of times does not leak linear memory.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('../../', import.meta.url));
const WASM = fileURLToPath(new URL('../../plugin/content/fulltext.wasm', import.meta.url));
const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: true });

const readText = (rel) => readFile(`${ROOT}${rel}`, 'utf8');
const readBytes = async (rel) => new Uint8Array(await readFile(`${ROOT}${rel}`));
const readJson = async (rel) => JSON.parse(await readText(rel));

let module;
async function load() {
  if (!module) {
    const bytes = await readFile(WASM).catch(() => {
      throw new Error(`missing ${WASM}: run "cargo xtask build" first`);
    });
    module = await WebAssembly.compile(bytes);
  }
  return module;
}

/** A fresh instance wrapped in the minimal JS calling convention from the ABI docs. */
async function instantiate() {
  const mod = await load();
  const { exports: x } = await WebAssembly.instantiate(mod, {});
  const put = (bytes) => {
    const ptr = x.fx_alloc(bytes.length);
    assert.ok(ptr !== 0 || bytes.length === 0, 'fx_alloc returned null');
    new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes); // view AFTER alloc: memory may grow
    return ptr;
  };
  /** Copy a packed (ptr << 32 | len) result out, free it, return the parsed envelope. */
  const take = (packed) => {
    assert.equal(typeof packed, 'bigint', 'u64 results arrive as BigInt');
    const ptr = Number(packed >> 32n);
    const len = Number(packed & 0xffffffffn);
    const text = dec.decode(new Uint8Array(x.memory.buffer, ptr, len)); // fatal decoder: must be valid UTF-8
    x.fx_free(ptr, len);
    const parsed = JSON.parse(text);
    const keys = Object.keys(parsed);
    assert.equal(keys.length, 1, `an envelope has exactly one key: ${keys}`);
    assert.ok(keys[0] === 'ok' || keys[0] === 'err', `unexpected envelope key ${keys[0]}`);
    return parsed;
  };
  return {
    x,
    /** fx_inspect(meta JSON, body bytes) -> parsed envelope. The callee frees both inputs. */
    inspect(meta, body = new Uint8Array(0)) {
      const m = enc.encode(JSON.stringify(meta));
      return take(x.fx_inspect(put(m), m.length, put(body), body.length));
    },
    /** fx_login_state(url, html); html_len 0 means "no document". */
    loginState(url, html = '') {
      const u = enc.encode(url);
      const h = typeof html === 'string' ? enc.encode(html) : html;
      return take(x.fx_login_state(put(u), u.length, put(h), h.length));
    },
  };
}

// ------------------------------------------------------------------------------------------------ HTML fixtures

/**
 * Status and headers the HTML fixtures were captured with (fixtures/html/README.md); everything else was a plain 200.
 * A re-implementation of html_response_facts in crates/fulltext-core/tests/common/mod.rs.
 */
function htmlResponseFacts(file) {
  switch (file) {
    case 'cloudflare__managed-challenge.html':
      return [403, { server: 'cloudflare', 'cf-mitigated': 'challenge' }];
    case 'cloudflare__blocked-attention-required.html':
      return [403, { server: 'cloudflare' }];
    case 'akamai__access-denied.html':
      return [403, { server: 'AkamaiGHost', 'akamai-grn': '0.1a2b3c4d.1792496400.1' }];
    case 'europepmc-web__pdf-render-cloudflare-challenge.html':
      return [403, { server: 'cloudflare', 'cf-mitigated': 'challenge' }];
    default:
      return [200, {}];
  }
}

const pageMeta = (resp, route = 'oa') => ({ spec: { expect: 'page', requested: {}, route }, resp });

/** The meta JS builds for a recorded page: capture-time status and headers, base_url as the final URL. */
function htmlMeta(entry, body) {
  const [status, headers] = htmlResponseFacts(entry.file);
  return {
    status,
    final_url: entry.base_url,
    headers: { ...headers, 'content-type': 'text/html; charset=utf-8' },
    body_len: body.length,
  };
}

const htmlFixtures = async () => (await readJson('fixtures/html/expected.json')).fixtures;

const loadHtml = async (entries) =>
  Promise.all(entries.map(async (entry) => ({ entry, body: await readBytes(`fixtures/html/${entry.file}`) })));

test('every HTML fixture follows the expected.json oracle through fx_inspect (page spec)', async () => {
  const w = await instantiate();
  const fixtures = await loadHtml(await htmlFixtures());
  assert.ok(fixtures.length >= 30, `${fixtures.length} fixtures`);
  let landing = 0;
  for (const { entry, body } of fixtures) {
    const env = w.inspect(pageMeta(htmlMeta(entry, body)), body);
    assert.ok(env.ok, `${entry.file}: ${JSON.stringify(env)}`);
    const result = env.ok;
    assert.equal(result.expect, 'page', `${entry.file}: a ${entry.kind} page must come back as a page analysis`);
    const kind = result.class.kind;
    switch (entry.kind) {
      case 'landing': {
        assert.ok(['landing', 'html'].includes(kind), `${entry.file}: class ${kind}`);
        const allowed = [entry.expected_pdf_url, ...(entry.alternates ?? [])].filter(Boolean);
        if (entry.expected_pdf_url) {
          assert.ok(result.candidates.length > 0, `${entry.file}: no candidate; rejected ${JSON.stringify(result.rejected)}`);
          const top = result.candidates[0].url;
          assert.ok(allowed.includes(top), `${entry.file}: picked ${top}, want one of ${allowed}`);
          landing++;
        } else {
          assert.equal(result.candidates.length, 0, `${entry.file}: expected no candidate`);
        }
        for (const c of result.candidates) {
          assert.equal(c.kind, 'pdf', entry.file);
          assert.ok(c.url.startsWith('https://'), `${entry.file}: ${c.url}`);
          assert.ok(!(entry.must_not_select ?? []).includes(c.url), `${entry.file}: forbidden URL selected: ${c.url}`);
        }
        break;
      }
      case 'challenge':
        assert.equal(kind, 'challenge', `${entry.file}: ${JSON.stringify(result.class)}`);
        assert.equal(result.candidates.length, 0, entry.file);
        break;
      case 'blocked':
        assert.equal(kind, 'blocked', `${entry.file}: ${JSON.stringify(result.class)}`);
        assert.equal(result.candidates.length, 0, entry.file);
        break;
      case 'redirect':
        assert.deepEqual(result.class, { kind: 'redirect', next: entry.next_url }, entry.file);
        assert.equal(result.candidates.length, 0, entry.file);
        break;
      default:
        assert.fail(`${entry.file}: unknown kind ${entry.kind}`);
    }
  }
  assert.equal(landing, 22, 'landing pages with an expected PDF');
});

test('the same HTML under the pdf spec is a page analysis, and a probe reports the same class', async () => {
  const w = await instantiate();
  for (const { entry, body } of await loadHtml(await htmlFixtures())) {
    const resp = htmlMeta(entry, body);
    const viaPage = w.inspect(pageMeta(resp), body).ok;
    const viaPdf = w.inspect({ spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp }, body).ok;
    assert.equal(viaPdf.expect, 'page', `${entry.file}: HTML under the pdf spec must not be a bare pdf verdict`);
    assert.deepEqual(viaPdf, viaPage, entry.file);
    const probe = w.inspect({ spec: { expect: 'probe', route: 'oa' }, resp }, body).ok;
    assert.equal(probe.expect, 'probe', entry.file);
    assert.deepEqual(probe.class, viaPage.class, `${entry.file}: probe and page disagree`);
  }
});

test('browser settle probes (empty body, status 200, the window URL) classify SSO hops from the URL alone', async () => {
  const w = await instantiate();
  const probe = (url) => w.inspect({ spec: { expect: 'probe', route: 'inst' }, resp: { status: 200, final_url: url, body_len: 0 } }).ok;
  const ENTRA_URL = 'https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/oauth2/v2.0/authorize?client_id=x';
  const EZ_LOGIN = 'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373';
  assert.deepEqual(probe(ENTRA_URL), { expect: 'probe', class: { kind: 'idp_login', idp: 'entra', ca_error: null } });
  assert.deepEqual(probe(EZ_LOGIN), { expect: 'probe', class: { kind: 'ezproxy_need_login' } });
  assert.deepEqual(probe('https://www.thelancet.com/action/ssostart?redirectUri=%2Fdoi%2Ffull%2F10.1016%2Fx'), { expect: 'probe', class: { kind: 'publisher_sso_hop' } });
  assert.deepEqual(probe('https://www.nature.com/articles/nature12373'), { expect: 'probe', class: { kind: 'html' } });
  // A no-follow probe of a proxied host that bounces to the EZproxy login page: request URL as final_url + Location.
  const bounce = {
    status: 302,
    final_url: 'https://www-nature-com.kcl.idm.oclc.org/articles/nature12373',
    headers: { server: 'EZproxy', location: 'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373' },
    body_len: 0,
  };
  assert.deepEqual(w.inspect({ spec: { expect: 'probe', route: 'inst' }, resp: bounce }).ok, { expect: 'probe', class: { kind: 'ezproxy_need_login' } });
});

test('OA-side walls (only a content-type, no server header) are recognised from the body', async () => {
  const w = await instantiate();
  const manifest = (await readJson('fixtures/oa/manifest.json')).fixtures;
  const want = {
    'pmc-web__pdf-pow-interstitial.html': { kind: 'challenge', vendor: 'pmc', interactive: false },
    'europepmc-web__pdf-render-cloudflare-challenge.html': { kind: 'challenge', vendor: 'cloudflare', interactive: false },
    'medrxiv-web__pdf-403-block.html': { kind: 'blocked', vendor: 'cloudflare' },
  };
  for (const [file, cls] of Object.entries(want)) {
    const entry = manifest.find((m) => m.file === file);
    assert.ok(entry, `${file} is in the manifest`);
    const body = await readBytes(`fixtures/oa/${file}`);
    const resp = { status: entry.status, final_url: entry.url, headers: { 'content-type': entry.content_type }, body_len: body.length };
    const env = w.inspect({ spec: { expect: 'probe', route: 'oa' }, resp }, body);
    assert.deepEqual(env.ok?.class, cls, file);
  }
});

// ------------------------------------------------------------------------------------------------ API fixtures

const GOLD = '10.1371/journal.pmed.1000097';
const NONEXISTENT = '10.9999/zft.nonexistent.000001';

/**
 * One manifest file per parser: [file, source (snake_case Source), request DOI, expected status].
 * Two of them are the interesting failures: a 404 with an HTML body, and a 200 that says "nothing found".
 */
const API_SAMPLE = [
  ['openalex__gold.json', 'openalex', GOLD, 'found'],
  ['europepmc__nonexistent.json', 'europepmc', NONEXISTENT, 'empty'], // 200, hitCount 0
  ['biorxiv-api__medrxiv-details.json', 'biorxiv', '10.1101/2023.05.31.23290770', 'found'],
  ['pmc-s3__meta-gold.json', 'pmc_s3', GOLD, 'found'],
  ['arxiv__export-api.xml', 'arxiv', '10.48550/arxiv.1706.03762', 'found'],
  ['unpaywall__nonexistent-404.html', 'unpaywall', NONEXISTENT, 'not_found'], // 404 with an HTML body
  ['semanticscholar__error-429-unauthenticated.json', 'semantic_scholar', GOLD, 'throttled'],
  ['core__discover-gold-no-key.json', 'core', GOLD, 'found'],
  ['crossref__gold.json', 'doi', GOLD, 'found'], // Crossref is reported as Source::Doi
  ['zotero-oa__nonexistent.json', 'zotero_oa', NONEXISTENT, 'empty'], // 200, "[]"
];

/** More cases, beyond one per source: failures the engine must tell apart. */
const API_EXTRA = [
  ['openalex__nonexistent-404.html', 'openalex', NONEXISTENT, 'not_found', 'http_404'],
  ['unpaywall__error-no-email.json', 'unpaywall', GOLD, 'unparseable', 'bad_email'],
  ['openalex__paywalled.json', 'openalex', '10.1097/sla.0000000000005603', 'empty', undefined],
  ['europepmc-web__pdf-render-cloudflare-challenge.html', 'europepmc', GOLD, 'unparseable', 'http_403'],
  ['pmc-web__pdf-pow-interstitial.html', 'pmc_s3', GOLD, 'unparseable', undefined],
  ['biorxiv-api__medrxiv-pubs.json', 'biorxiv', '10.1101/2023.05.31.23290770', 'found', undefined],
];

async function apiInspect(w, manifest, [file, source, doi]) {
  const entry = manifest.find((m) => m.file === file);
  assert.ok(entry, `${file} is not in fixtures/oa/manifest.json`);
  const body = await readBytes(`fixtures/oa/${file}`);
  const resp = { status: entry.status, final_url: entry.url, headers: { 'content-type': entry.content_type }, body_len: body.length };
  return w.inspect({ spec: { expect: 'api', source, requested: { doi, rxiv: doi.startsWith('10.1101/') } }, resp }, body);
}

test('ten OA API fixtures (one per source) come back as ok api inspections with the manifest case semantics', async () => {
  const w = await instantiate();
  const manifest = (await readJson('fixtures/oa/manifest.json')).fixtures;
  assert.equal(new Set(API_SAMPLE.map((s) => s[1])).size, 10, 'one fixture per source');
  for (const row of API_SAMPLE) {
    const env = await apiInspect(w, manifest, row);
    assert.ok(env.ok, `${row[0]}: ${JSON.stringify(env)}`);
    assert.equal(env.ok.expect, 'api', row[0]);
    assert.equal(env.ok.source, row[1], row[0]);
    assert.equal(env.ok.status, row[3], `${row[0]}: note ${env.ok.note}`);
    if (row[3] === 'found') assert.ok(env.ok.candidates.length > 0 || env.ok.note || env.ok.learned, row[0]);
    else assert.deepEqual(env.ok.candidates ?? [], [], `${row[0]}: no candidates unless found`);
    for (const c of env.ok.candidates ?? []) assert.ok(c.url.startsWith('https://'), `${row[0]}: ${c.url}`);
  }
});

test('API failures carry their notes through the wire', async () => {
  const w = await instantiate();
  const manifest = (await readJson('fixtures/oa/manifest.json')).fixtures;
  for (const row of API_EXTRA) {
    const env = await apiInspect(w, manifest, row);
    assert.equal(env.ok?.status, row[3], `${row[0]}: ${JSON.stringify(env)}`);
    if (row[4]) assert.equal(env.ok.note, row[4], row[0]);
  }
  // Retry-After is read from the headers JS passes.
  const entry = manifest.find((m) => m.file === 'semanticscholar__error-429-unauthenticated.json');
  const body = await readBytes(`fixtures/oa/${entry.file}`);
  const resp = { status: 429, final_url: entry.url, headers: { 'content-type': 'application/json', 'retry-after': '17' }, body_len: body.length };
  const env = w.inspect({ spec: { expect: 'api', source: 'semantic_scholar', requested: {} }, resp }, body);
  assert.equal(env.ok.status, 'throttled');
  assert.equal(env.ok.retry_after_s, 17);
});

// ------------------------------------------------------------------------------------------------ binary bodies

const MB = 1_000_000;

/** A synthetic PDF of exactly `size` bytes: header, filler, trailer with the end-of-file marker. */
function syntheticPdf(size, prefix = new Uint8Array(0)) {
  const head = enc.encode('%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\n');
  const tail = enc.encode('\ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF\n');
  const out = new Uint8Array(size);
  out.set(prefix, 0);
  out.set(head, prefix.length);
  out.fill(0x78, prefix.length + head.length, size - tail.length); // 'x'
  out.set(tail, size - tail.length);
  return out;
}

/** How the plugin stages a big binary body: head ++ tail, with head_len and the real length. */
function stage(full, headLen = 1024, tailLen = 1024) {
  const staged = new Uint8Array(headLen + tailLen);
  staged.set(full.subarray(0, headLen), 0);
  staged.set(full.subarray(full.length - tailLen), headLen);
  return { staged, resp: { status: 200, final_url: 'https://example.org/paper.pdf', headers: { 'content-type': 'application/pdf' }, body_len: full.length, head_len: headLen } };
}

test('a 5 MB PDF staged as head(1024) ++ tail(1024) is judged without ever copying it', async () => {
  const w = await instantiate();
  const full = syntheticPdf(5 * MB);
  assert.equal(full.length, 5_000_000);
  const { staged, resp } = stage(full);
  assert.equal(staged.length, 2048);
  for (const expect of ['pdf', 'page']) {
    const spec = { expect, requested: {}, route: 'oa' };
    const env = w.inspect({ spec, resp }, staged);
    assert.equal(env.ok?.expect, 'pdf', `${expect} spec: PDF bytes are a pdf verdict ${JSON.stringify(env)}`);
    assert.equal(env.ok.is_pdf, true);
    assert.equal(env.ok.has_eof, true, 'the tail carries %%EOF');
    assert.equal(env.ok.len, 5_000_000);
    assert.equal(env.ok.junk_prefix, 0);
    assert.equal(env.ok.looks_html, false);
  }
  const probe = w.inspect({ spec: { expect: 'probe', route: 'oa' }, resp }, staged);
  assert.deepEqual(probe.ok, { expect: 'probe', class: { kind: 'pdf', junk_prefix: 0 } });
});

test('the plugin staging sizes (head 4096, tail 2048) work too, and a missing %%EOF is reported', async () => {
  const w = await instantiate();
  const full = syntheticPdf(3 * MB);
  const { staged, resp } = stage(full, 4096, 2048);
  const env = w.inspect({ spec: { expect: 'pdf', requested: {}, route: 'inst' }, resp }, staged);
  assert.deepEqual([env.ok.is_pdf, env.ok.has_eof, env.ok.len], [true, true, 3 * MB]);

  const cut = full.slice(0, full.length - 40); // a truncated download: no end marker in the tail
  const staged2 = stage(cut);
  const env2 = w.inspect({ spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp: staged2.resp }, staged2.staged);
  assert.deepEqual([env2.ok.is_pdf, env2.ok.has_eof, env2.ok.len], [true, false, cut.length]);
});

test('a BOM-prefixed PDF reports junk_prefix 3, whole or staged', async () => {
  const w = await instantiate();
  const bom = Uint8Array.of(0xef, 0xbb, 0xbf);
  const small = syntheticPdf(10_000, bom);
  const whole = w.inspect(
    { spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp: { status: 200, final_url: 'https://example.org/a.pdf', headers: { 'content-type': 'application/pdf' }, body_len: small.length } },
    small,
  );
  assert.deepEqual([whole.ok.is_pdf, whole.ok.junk_prefix, whole.ok.len], [true, 3, 10_000]);

  const big = syntheticPdf(5 * MB, bom);
  const { staged, resp } = stage(big);
  const env = w.inspect({ spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp }, staged);
  assert.deepEqual([env.ok.is_pdf, env.ok.junk_prefix, env.ok.has_eof, env.ok.len], [true, 3, true, 5 * MB]);
  const probe = w.inspect({ spec: { expect: 'probe', route: 'oa' }, resp }, staged);
  assert.deepEqual(probe.ok.class, { kind: 'pdf', junk_prefix: 3 });
});

test('a staged HTML error page is still a page, not a verdict', async () => {
  const w = await instantiate();
  const html = enc.encode('<html><head><title>Access denied</title></head><body>Access denied</body></html>');
  const resp = { status: 403, final_url: 'https://example.org/x.pdf', headers: { 'content-type': 'text/html' }, body_len: html.length };
  const env = w.inspect({ spec: { expect: 'pdf', requested: {}, route: 'oa' }, resp }, html);
  assert.equal(env.ok?.expect, 'page');
});

// ------------------------------------------------------------------------------------------------ login window poll

const ENTRA = 'https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/oauth2/v2.0/authorize?client_id=x';
const REDIRECTOR = 'https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373';
const EZPROXY_LOGIN = 'https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373';

test('fx_login_state answers the representative URL / document pairs', async () => {
  const w = await instantiate();
  const data = (name) => readText(`crates/fulltext-core/tests/data/page/${name}`);
  const plos = (await htmlFixtures()).find((e) => e.file === 'plos__article-oa.html');
  const cases = [
    ['Entra sign-in URL, no document', ENTRA, '', { state: 'pending' }],
    ['OpenAthens redirector URL, no document', REDIRECTOR, '', { state: 'pending' }],
    ['EZproxy Shibboleth request page at the login host', EZPROXY_LOGIN, await data('ezproxy_login_page.html'), { state: 'pending' }],
    ['Cloudflare managed challenge on a publisher', 'https://www.sciencedirect.com/science/article/pii/S1743919121000406',
      await readText('fixtures/html/cloudflare__managed-challenge.html'), { state: 'challenge_shown' }],
    ['Entra conditional-access error', ENTRA, await data('entra_ca_error.html'), { state: 'refused', code: 'AADSTS53000' }],
    ['publisher landing page', plos.base_url, await readText('fixtures/html/plos__article-oa.html'), { state: 'landed' }],
    ['PDF URL, no document', 'https://example.org/paper.pdf', '', { state: 'landed' }],
  ];
  for (const [label, url, html, want] of cases) {
    const env = w.loginState(url, html);
    assert.deepEqual(env.ok, want, `${label}: ${JSON.stringify(env)}`);
  }
});

// ------------------------------------------------------------------------------------------------ guards

test('performance guard: all HTML fixtures are inspected in under 3 s in total', async (t) => {
  const w = await instantiate();
  const fixtures = await loadHtml(await htmlFixtures());
  assert.ok(fixtures.length >= 30);
  // Warm up the instance (first calls grow memory and fault pages in); the guard measures steady state.
  for (const { entry, body } of fixtures) w.inspect(pageMeta(htmlMeta(entry, body)), body);
  const started = performance.now();
  for (const { entry, body } of fixtures) {
    const env = w.inspect(pageMeta(htmlMeta(entry, body)), body);
    assert.ok(env.ok, entry.file);
  }
  const ms = performance.now() - started;
  t.diagnostic(`${fixtures.length} HTML fixtures inspected in ${ms.toFixed(1)} ms (${(ms / fixtures.length).toFixed(2)} ms each)`);
  console.log(`# ${fixtures.length} HTML fixtures inspected in ${ms.toFixed(1)} ms`);
  assert.ok(ms < 3000, `${fixtures.length} fixtures took ${ms.toFixed(0)} ms (limit 3000)`);
});

test('memory guard: 500 fixture inspections after warm-up do not grow linear memory', async (t) => {
  const w = await instantiate();
  const html = await loadHtml(await htmlFixtures());
  const manifest = (await readJson('fixtures/oa/manifest.json')).fixtures;
  // Preload every body so the measured loop is synchronous and the guard is about the wasm, not about file reads.
  const loaded = await Promise.all(
    [...API_SAMPLE, ...API_EXTRA].map(async (row) => {
      const entry = manifest.find((m) => m.file === row[0]);
      const body = await readBytes(`fixtures/oa/${row[0]}`);
      const resp = { status: entry.status, final_url: entry.url, headers: { 'content-type': entry.content_type }, body_len: body.length };
      return { meta: { spec: { expect: 'api', source: row[1], requested: { doi: row[2], rxiv: false } }, resp }, body };
    }),
  );
  const calls = [
    ...html.map(({ entry, body }) => () => w.inspect(pageMeta(htmlMeta(entry, body)), body)),
    ...loaded.map(({ meta, body }) => () => w.inspect(meta, body)),
  ];
  assert.ok(calls.length >= 40, `${calls.length} distinct inspections`);
  // Warm-up: every distinct input at least twice, so the allocator has seen the largest request of each.
  for (let round = 0; round < 3; round++) for (const call of calls) assert.ok(call().ok);
  const before = w.x.memory.buffer.byteLength;
  for (let i = 0; i < 500; i++) assert.ok(calls[i % calls.length]().ok);
  const after = w.x.memory.buffer.byteLength;
  t.diagnostic(`linear memory ${before} -> ${after} bytes over 500 inspections`);
  assert.equal(after, before, 'linear memory grew during steady-state inspections');
});
