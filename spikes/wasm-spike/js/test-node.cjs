// Node 24 stand-in for Gecko: node js/test-node.cjs [out/core-hand.wasm]
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");
const { FulltextCore, FulltextCoreError } = require("./fulltext-core.js");

const root = path.join(__dirname, "..");
const wasmPath = process.argv[2] ?? path.join(root, "out/core-hand.wasm");
const fx = (n) => fs.readFileSync(path.join(root, "fixtures", n));

(async () => {
  const bytes = fs.readFileSync(wasmPath);
  let t = performance.now();
  const mod = await WebAssembly.compile(bytes);
  const tCompile = performance.now() - t;
  t = performance.now();
  const core = await FulltextCore.load(mod);
  const tInst = performance.now() - t;
  console.log(`${path.basename(wasmPath)}: ${bytes.length} B, compile ${tCompile.toFixed(1)} ms, instantiate ${tInst.toFixed(2)} ms`);
  console.log("imports:", JSON.stringify(WebAssembly.Module.imports(mod)));

  // --- logic round-trips
  assert.equal(core.call("fx_normalize_doi", { text: "https://doi.org/10.1371/journal.pmed.1000097" }), "10.1371/journal.pmed.1000097");
  assert.equal(
    core.call("fx_proxy_url", { mode: "hostname", url: "https://www.nature.com/articles/x", suffix: "kcl.idm.oclc.org" }),
    "https://www-nature-com.kcl.idm.oclc.org/articles/x");
  assert.equal(
    core.call("fx_proxy_url", { mode: "prefix", url: "https://www.nature.com/articles/x", prefix: "https://login.kcl.idm.oclc.org/login?url=" }),
    "https://login.kcl.idm.oclc.org/login?url=https://www.nature.com/articles/x");
  const ranked = core.call("fx_rank_unpaywall", {
    best_oa_location: { url: "https://repo/x", host_type: "repository", version: "acceptedVersion" },
    oa_locations: [{ url_for_pdf: "https://pub/x.pdf", host_type: "publisher", version: "publishedVersion" }],
  });
  assert.equal(ranked[0].url, "https://pub/x.pdf");
  assert.equal(core.sniffPdf(new TextEncoder().encode("%PDF-1.7\n")), true);
  assert.equal(core.sniffPdf(new TextEncoder().encode("<html>")), false);

  // --- errors are values, not traps
  assert.throws(() => core.call("fx_proxy_url", { mode: "hostname", url: "not a url", suffix: "x" }),
    (e) => e instanceof FulltextCoreError && e.code === "bad_url");
  assert.throws(() => core.call("fx_proxy_url", { nope: 1 }), (e) => e.code === "bad_json");
  console.log("logic + error envelope: OK");

  // --- landing pages (UTF-8 round-trip incl. non-ASCII)
  const pages = {
    "plos-med.html": "https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097",
    "arxiv.html": "https://arxiv.org/abs/1706.03762",
    "bmc-med.html": "https://link.springer.com/article/10.1186/1741-7015-8-18",
    "pmc.html": "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/",
    "frontiers.html": "https://www.frontiersin.org/journals/psychology/articles/10.3389/fpsyg.2013.00863/full",
    "messy.html": "https://example.org/a/123",
  };
  for (const [file, url] of Object.entries(pages)) {
    const html = fx(file).toString("utf8");
    const out = core.call("fx_landing_pdf_candidates", { html, url });
    const N = 20;
    const t0 = performance.now();
    for (let i = 0; i < N; i++) core.call("fx_landing_pdf_candidates", { html, url });
    const ms = (performance.now() - t0) / N;
    let msRaw = NaN;
    if (core.x.fx_landing_raw) {
      const raw = core.landingRaw(html, url);
      assert.deepEqual(raw.candidates, out.candidates);
      const t1 = performance.now();
      for (let i = 0; i < N; i++) core.landingRaw(html, url);
      msRaw = (performance.now() - t1) / N;
    }
    console.log(`  ${file.padEnd(15)} ${(html.length / 1024).toFixed(0).padStart(4)} KB  json ${ms.toFixed(2).padStart(6)} ms/call  raw ${msRaw.toFixed(2).padStart(6)} ms/call  top=${out.candidates[0]?.url ?? "-"}`);
  }

  // --- leak check: memory must stop growing under repeated calls
  const html = fx("frontiers.html").toString("utf8");
  const before = core.x.memory.buffer.byteLength;
  for (let i = 0; i < 300; i++) core.call("fx_landing_pdf_candidates", { html, url: "https://www.frontiersin.org/x" });
  const after = core.x.memory.buffer.byteLength;
  console.log(`memory: ${before / 65536} -> ${after / 65536} pages after 300 x 767 KB calls`);
  assert.ok(after <= before * 2, "linear memory keeps growing: leak?");

  // --- big input forces memory.grow mid-call (views must be re-created)
  const big = html.repeat(8); // ~6 MB
  const tb = performance.now();
  const bigOut = core.call("fx_landing_pdf_candidates", { html: big, url: "https://www.frontiersin.org/x" });
  console.log(`6 MB page: ${(performance.now() - tb).toFixed(1)} ms, ${bigOut.candidates.length} candidates, memory now ${core.x.memory.buffer.byteLength / 65536} pages`);

  console.log("ALL OK");
})().catch((e) => { console.error("FAILED:", e); process.exit(1); });
