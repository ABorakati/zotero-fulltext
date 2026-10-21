/* Harness-only helper for a THROWAWAY Zotero profile (installed by bidi-probe.ps1; never ship it).
 *
 * Gecko's Remote Agent (WebDriver BiDi) and Marionette both wait, before creating a session, for the observer topic
 * "browser-idle-startup-tasks-finished" (components/RemoteAgent.sys.mjs, Marionette.sys.mjs). Firefox's BrowserGlue
 * fires it; Zotero never does, so session creation hangs forever. Zotero's main window already has
 * windowtype="navigator:browser", so this topic is the only missing piece. */
/* global Zotero, Services */
async function startup() {
  await Zotero.uiReadyPromise;
  Services.obs.notifyObservers(null, 'browser-idle-startup-tasks-finished');
  Zotero.debug('[remote-unlock] fired browser-idle-startup-tasks-finished');
}

function shutdown() {}
function install() {}
function uninstall() {}
