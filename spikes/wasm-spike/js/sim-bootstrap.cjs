// Simulates Zotero's plugin sandbox (plugins.js _loadScope) in a bare vm global and
// runs plugin-skel/bootstrap.js startup(). Only the globals Zotero grants are present.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const skel = path.join(__dirname, "../plugin-skel");
const chromeToFile = (u) => path.join(skel, "content", u.replace("chrome://fulltext-spike/content/", ""));
const log = [];
const sandbox = vm.createContext({
  // wantGlobalProperties subset from plugins.js:141-156 + WebAssembly (JS builtin)
  TextEncoder, TextDecoder, URL, WebAssembly, console,
  fetch: async (u) => ({ arrayBuffer: async () => { const b = fs.readFileSync(chromeToFile(u)); return b.buffer.slice(b.byteOffset, b.byteOffset + b.length); } }),
  Zotero: { initializationPromise: Promise.resolve(), debug: (m) => log.push(m) },
  Cc: { "@mozilla.org/addons/addon-manager-startup;1": { getService: () => ({ registerChrome: () => ({ destruct() {} }) }) } },
  Ci: { amIAddonManagerStartup: {} },
  Services: {
    io: { newURI: (s) => ({ spec: s }) },
    scriptloader: { loadSubScript: (u, target) => vm.runInContext(fs.readFileSync(chromeToFile(u), "utf8"), sandbox, { filename: u }) },
  },
});
vm.runInContext(fs.readFileSync(path.join(skel, "bootstrap.js"), "utf8"), sandbox, { filename: "bootstrap.js" });
(async () => {
  // plugins.js:258 calls func.call(scope, params, reason)
  await vm.runInContext("startup", sandbox).call(sandbox, { id: "x", version: "0", rootURI: "jar:file:///x.xpi!/" }, 1);
  const core = sandbox.Zotero.FulltextSpike.core;
  console.log(log.join("\n"));
  console.log("doi:", core.call("fx_normalize_doi", { text: "https://doi.org/10.1186/1741-7015-8-18" }));
  vm.runInContext("shutdown", sandbox).call(sandbox);
  console.log("shutdown ok, Zotero.FulltextSpike =", sandbox.Zotero.FulltextSpike);
})().catch((e) => { console.error(e); process.exit(1); });
