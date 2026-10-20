/* Spike bootstrap: loads the hand-ABI glue + wasm. Not installed anywhere. */
var chromeHandle;

async function startup({ id, version, rootURI }) {
  await Zotero.initializationPromise;
  const aomStartup = Cc["@mozilla.org/addons/addon-manager-startup;1"].getService(Ci.amIAddonManagerStartup);
  chromeHandle = aomStartup.registerChrome(Services.io.newURI(rootURI + "manifest.json"), [
    ["content", "fulltext-spike", rootURI + "content/"],
  ]);
  // Plain script -> defines FulltextCore on the sandbox global (`this`).
  Services.scriptloader.loadSubScript("chrome://fulltext-spike/content/fulltext-core.js", this);
  const bytes = await (await fetch("chrome://fulltext-spike/content/fulltext_core.wasm")).arrayBuffer();
  Zotero.FulltextSpike = { core: await FulltextCore.load(bytes) };
  Zotero.debug("fulltext-spike: wasm core ABI " + Zotero.FulltextSpike.core.x.fx_abi_version());
}

function shutdown() {
  if (Zotero.FulltextSpike) delete Zotero.FulltextSpike;
  chromeHandle?.destruct();
  chromeHandle = null;
}
function install() {}
function uninstall() {}
