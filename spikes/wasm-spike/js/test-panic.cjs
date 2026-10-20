// What does a Rust panic look like from JS with panic = "abort"? node js/test-panic.cjs
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const { FulltextCore } = require("./fulltext-core.js");

(async () => {
  const bytes = fs.readFileSync(path.join(__dirname, "../out/core-hand-debugpanic.wasm"));
  const core = await FulltextCore.load(bytes);
  console.log("fx_debug_panic(7) =", core.x.fx_debug_panic(7));
  for (const code of [1, 2]) {
    try {
      core._guard(() => core.x.fx_debug_panic(code));
    } catch (e) {
      console.log(`code ${code}: ${e.constructor.name} instanceof WebAssembly.RuntimeError=${e instanceof WebAssembly.RuntimeError} message=${JSON.stringify(e.message)}`);
      console.log("   stack head:", e.stack.split("\n").slice(0, 3).join(" | "));
    }
    console.log("   poisoned =", core.poisoned);
    try { core.call("fx_normalize_doi", { text: "10.1/x" }); } catch (e) { console.log("   next call:", e.message); }
    // Same instance still callable if we bypass the guard? (state may be corrupt; do not rely on it)
    console.log("   raw call on poisoned instance:", core.x.fx_abi_version());
    await core.reset();
    console.log("   after reset():", core.call("fx_normalize_doi", { text: "doi:10.1000/ABC" }));
  }
  // An allocation that cannot be satisfied: Rust's alloc error handler aborts -> trap
  try {
    core._guard(() => core.x.fx_alloc(0x7fff_fff0));
    console.log("huge alloc returned (unexpected)");
  } catch (e) {
    console.log(`huge alloc: ${e.constructor.name}: ${e.message}`);
  }
})();
