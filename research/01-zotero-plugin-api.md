# 01 — Zotero 10.x plugin architecture and UI APIs (installed: Zotero 10.0.6, Gecko 140 ESR)

Scope: everything the `zotero-fulltext` JS shell needs from Zotero itself. That covers manifest, bootstrap lifecycle, item context menu, prefs, Fluent l10n, progress UI, an interactive SSO login window that shares cookies with `Zotero.HTTP`, the dev workflow, and breaking changes from Zotero 7 to 10.

**Sources and confidence**
- **Primary source:** the extracted omni.ja of the installed build:
  - `.ref/omni-app` = `C:\Users\abora\zotero-fulltext\.ref\omni-app`
  - `.ref/omni-gre` = Gecko toolkit
  - Line numbers refer to these files. They are Babel-reformatted, so they differ slightly from GitHub. The matching GitHub tag is **`zotero/zotero@10.0.6`**; the branch is `10.0`. `application.ini`: `Version=10.0.6`, `BuildID=20261007153950`, `[Gecko] MinVersion=140.0`.
- **Real plugins:** four Zotero-10-era XPIs, copied and extracted to `C:\Users\abora\zotero-fulltext\.ref\xpis\<id>\`. Their state was read from the user's `extensions.json`, read-only.
- **Confidence levels:**
  - **high** = verified in source or empirically.
  - **medium** = official docs, or a strong inference from source.
  - **low** = inferred or secondhand.

---

## 0. TL;DR: decisions for zotero-fulltext

| Topic | Decision | Conf. |
|---|---|---|
| Manifest | `manifest_version: 2`. Use the `applications.zotero` key, not `browser_specific_settings`. **`id`, `update_url` and `strict_max_version` are all mandatory**: Zotero's Gecko patch rejects the XPI without them. | high |
| Max version | Release builds enforce `strict_max_version`. Example: zotron (`9.*`) is `appDisabled` in 10.0.6. Use `"strict_min_version": "10.0"` with either `"strict_max_version": "10.*"` (no update server) or `"10.0.*"` (with update manifest). | high |
| Context menu | `Zotero.MenuManager.registerMenu({menuID, pluginID, target: "main/library/item", menus:[...]})`. Selected items arrive in `context.items` in both `onShowing(event, context)` and `onCommand(event, context)`. Menus are auto-removed on plugin shutdown. | high |
| Labels | Fluent `.ftl` file **directly** in `locale/en-US/`, with a plugin-unique filename such as `zotero-fulltext.ftl`. Messages need a `.label` attribute. Call `window.MozXULElement.insertFTLIfNeeded("zotero-fulltext.ftl")` for each main window. | high |
| Prefs | `prefs.js` at the plugin root holds `pref("extensions.zotero-fulltext.x", ...)`. Read and write with `Zotero.Prefs.get/set(key, true)`. Add a pane with `Zotero.PreferencePanes.register({pluginID, src, scripts})`. | high |
| Progress | `Zotero.ProgressWindow` + `ItemProgress`, or `Zotero.ProgressQueues.create(...)` for the "Find Available PDF"-style table dialog. Zotero 10 has no toast or notification API. | high |
| SSO login window | Use **your own chrome XHTML window** with a `<browser type="content" maychangeremoteness="true">` and **no** `messagemanagergroup="basicViewer"`. Drive it from plugin code: `loadURI`, poll `browser.currentURI`, PageData actor for HTML, PDF MIME handler, `win.close()`. With no `userContextId` it shares the **default cookie jar** with `Zotero.HTTP.request`. `Zotero.openInViewer()` also works, but its window has an `ExternalLinkHandler` actor that sends direct `<a>` clicks to the system browser. | high (source) / medium (end-to-end untested) |
| Script loading | Register a chrome package with `aomStartup.registerChrome`. Load code through **`chrome://zotero-fulltext/content/...`** URLs, not `rootURI` jar:/file: URLs. Zotero `main` already needs `allowUnsafeURL` for jar: subscripts (Mozilla bug 1974213), so the next ESR will reject them. | high |
| WASM | `WebAssembly` is allowed in system-principal code (`javascript.options.wasm_trustedprincipals=true`). Beaver (a Zotero-10 plugin) ships a wasm-bindgen Rust crate loaded via `fetch(chrome://…_bg.wasm).arrayBuffer()` + `import(chrome://….js)` + `initSync({module})`. | high (pref + real plugin) |
| Dev profile | Use a separate profile and data dir: `zotero.exe -profile <dir> -no-remote -datadir <abs dir> -ZoteroDebugText -jsconsole`. Install with an extension proxy file. **Set `extensions.autoDisableScopes=0`**, or proxy-installed plugins arrive disabled. Delete `extensions.lastAppBuildId/lastAppVersion` to force a rescan. | high (source) / medium (flags) |

---

## 1. manifest.json

### 1.1 What Zotero's patched Gecko requires

`.ref/omni-gre/modules/Extension.sys.mjs:1874-1889` (high):

```js
if (this.type == "extension") {
  if (!manifest.applications?.zotero?.id)                 this.manifestError("applications.zotero.id not provided");
  if (!manifest.applications?.zotero?.update_url)         this.manifestError("applications.zotero.update_url not provided");
  if (!manifest.applications?.zotero?.strict_max_version) this.manifestError("applications.zotero.strict_max_version not provided");
  manifest.browser_specific_settings = undefined;   // ignored
  manifest.content_scripts = []; manifest.permissions = []; ...
```

The checks mean:
- **`applications.zotero.id`, `.update_url` and `.strict_max_version` are required.** If any is missing, install fails with "Extension is invalid" (`XPIInstall.sys.mjs:481-487`).
- `browser_specific_settings` is wiped, so you must use the deprecated `applications` key.
- The schema allows `applications` only with `max_manifest_version: 2` (`.ref/omni-gre/chrome/toolkit/content/extensions/schemas/manifest.json:18-23`), so use **`"manifest_version": 2`**.

`update_url` is schema-checked as `"format": "url"` (schema line 618-622). A URL that is never reachable only makes update checks fail quietly (medium).

The ID must match `gIDTest = /^(\{GUID\}|[a-z0-9-\._]*\@[a-z0-9-\._]+)$/i` (`XPIInstall.sys.mjs:177-178`, `XPIProvider.sys.mjs:201-202`). Use an email-like ID that is **not** a real email address, for example `zotero-fulltext@local`.

`XPIInstall.sys.mjs:492-596` maps the keys as follows (high):
- `bss = manifest.applications?.zotero`
- `addon.id = bss.id`
- `addon.updateURL = bss.update_url`
- `targetApplications = [{id:'zotero@zotero.org', minVersion: bss.strict_min_version, maxVersion: bss.strict_max_version}]`
- A `*` in `strict_min_version` throws (line 495-497).
- **`addon.strictCompatibility = !version.includes("-beta") && !version.includes("-dev") && !version.includes("SOURCE")`** (line 507). Release builds therefore enforce `strict_max_version`, even though `extensions.strictCompatibility=false` in defaults.

**Empirical (high):** the user's profile `extensions.json` (read-only) records:

| Plugin | min / max | State in Zotero 10.0.6 |
|---|---|---|
| zotron@diamondrill 0.2.5 | `6.999` / `9.*` | **`appDisabled=true`, active=false** |
| zotmoov@wileyy.com 1.2.32 | `6.999` / `10.*` | active |
| zotseek@zotero.org 1.22.6 | `7.999` / `10.0.*` | userDisabled (compatible) |
| beaver@jlegewie.com 0.26.0 | `6.999` / `10.0.*` | userDisabled (compatible) |

So real plugins today use `strict_max_version` `"10.0.*"` (beaver, zotseek) or `"10.*"` (zotmoov). `strict_min_version` is typically `"6.999"` or `"7.999"`.

Zotero's guidance (medium):
- Set `strict_max_version` to the latest tested `x.x.*`. Bump it through the update manifest without a new release ([Zotero 7 for developers](https://www.zotero.org/support/dev/zotero_7_for_developers)).
- The Zotero 10 page says to update to `10.0.*` ([Zotero 10 for developers](https://www.zotero.org/support/dev/zotero_10_for_developers)).

### 1.2 Icons

`"icons": {"48": "...", "96": "..."}` with paths relative to the plugin root (beaver, zotron, zotseek). Zotero uses them in three places (high):
- The Plugins manager.
- `Zotero.Plugins.getIconURI(id, size)` (`plugins.js:493-504`).
- The default pref-pane image, which calls `getIconURI(pluginID, 24)` (`preferencePanes.js:151-152`).

PNG is the proven format.

### 1.3 Recommended manifest for zotero-fulltext

```json
{
  "manifest_version": 2,
  "name": "Zotero Full Text",
  "version": "0.1.0",
  "description": "Find and attach full-text PDFs via open access and institutional (EZproxy) access",
  "author": "…",
  "icons": { "48": "content/icons/icon48.png", "96": "content/icons/icon96.png" },
  "applications": {
    "zotero": {
      "id": "zotero-fulltext@local",
      "update_url": "https://example.invalid/zotero-fulltext/updates.json",
      "strict_min_version": "10.0",
      "strict_max_version": "10.*"
    }
  }
}
```

Notes on these choices:
- **Why `10.0` minimum:** we rely on `Zotero.HTTP.newCookieContext()`, the Zotero 10 FTL handling and MenuManager DOM cleanup.
- **`update_url`:** the placeholder must be replaced. A GitHub raw `updates.json` is the usual choice; see the open questions.

Update manifest format (Mozilla JSON, medium, from the Zotero 7 dev page):

```json
{ "addons": { "zotero-fulltext@local": { "updates": [ {
  "version": "0.1.0",
  "update_link": "https://…/zotero-fulltext-0.1.0.xpi",
  "update_hash": "sha256:<hex>",
  "applications": { "zotero": { "strict_min_version": "10.0", "strict_max_version": "10.*" } }
} ] } } }
```

---

## 2. bootstrap.js lifecycle and scope

### 2.1 Loader implementation

The loader is `chrome/content/zotero/xpcom/plugins.js`.

**Timing (high):**
- `Zotero.initComplete()` resolves `initializationDeferred` (`zotero.js:430`), fires `zotero-loaded`, then `await Zotero.Plugins.init()` (`zotero.js:443`).
- By the time `startup()` runs at APP_STARTUP, `Zotero.initializationPromise` has already resolved.
- `Zotero.uiReadyPromise` resolves later, when the items view loads (`zoteroPane.js:1620`).

**Reason constants** (`plugins.js:53-64`) are injected into the scope as globals:
`APP_STARTUP=1, APP_SHUTDOWN=2, ADDON_ENABLE=3, ADDON_DISABLE=4, ADDON_INSTALL=5, ADDON_UNINSTALL=6, ADDON_UPGRADE=7, ADDON_DOWNGRADE=8, MAIN_WINDOW_LOAD=9, MAIN_WINDOW_UNLOAD=10`.

**Hook call** (`_callMethod`, `plugins.js:224-292`):
- Called as `func.call(scope, params, reason)` with `params = {id, version, rootURI}` (lines 248-252), plus `{window}` for the window hooks.
- Zotero 10 passes **no `resourceURI`**.
- `rootURI` is a string ending in `/`: `jar:file:///…/id.xpi!/` for an XPI, or `file:///…/` for a proxy or unpacked install.
- If the hook returns a promise, **it is awaited** (lines 258-262), and errors are caught and logged.

**Order of operations:**

| Event | Steps (with source lines) |
|---|---|
| App startup (`init`, 67-91) | for each active, non-blocked extension: `_loadScope` → `setDefaultPrefs` → `registerLocales` → `startup(APP_STARTUP)`. Then it adds a shutdown listener (`shutdown(APP_SHUTDOWN)`) and **only then** registers the `Services.wm` listener that calls `onMainWindowLoad`/`onMainWindowUnload` for `chrome://zotero/content/zoteroPane.xhtml` windows (93-124). |
| Install (`onInstalled`, 873-898) | `_loadScope` → prefs → locales → `install(reason)` → `startup(reason)`. |
| Upgrade (`onInstalling`, 854-871) | old instance gets `shutdown(UPGRADE/DOWNGRADE)` → `uninstall(reason)` → `startupcache-invalidate` → unregister locales → clear default prefs → **`_unloadScope`**. The new version then loads a fresh scope. |
| Enable (`onEnabling`, 900-909) | `_loadScope` → prefs → locales → `startup(ADDON_ENABLE)`. |
| Disable (`onDisabled`, 911-919) | `shutdown(ADDON_DISABLE)` → unregister locales → clear default prefs. **The scope is not unloaded.** |
| Uninstall (`onUninstalling`, 921-931) | `shutdown(ADDON_UNINSTALL)` → `uninstall(ADDON_UNINSTALL)`. `onUninstalled` (933-937) then unloads the scope. |

Gotchas (high):
- **On disable→enable the sandbox is reused** (`_loadScope` returns early when `scopes.has(id)`, line 134). `bootstrap.js` is **not** re-read, and top-level `var`/`let` state survives. `startup()` must fully re-initialise state, and `shutdown()` must null it out.
- Sub-scripts loaded in `startup` with `ignoreCache: true` **are** re-read on enable. That makes them handy for a dev edit→disable→enable loop. Edits to `bootstrap.js` need a restart or reinstall.
- `onMainWindowLoad` is **not** called for windows that were already open when the plugin starts: install, enable, or a slow APP_STARTUP where the window loaded before the wm listener was added. In `startup()`, iterate `Zotero.getMainWindows()` (filter `win.ZoteroPane`) and make the per-window setup idempotent. This is the make-it-red `addToAllWindows()` pattern (`zotero/make-it-red` `src-2.0/make-it-red.js`).
- `onMainWindowUnload` is called with reason `MAIN_WINDOW_LOAD`, not `..._UNLOAD`. This is a Zotero bug at `plugins.js:120` and harmless.
- AddonManager listeners are async and not awaited by AddonManager. On upgrade, a new instance's `startup` can overlap the old async `shutdown`; Beaver's bootstrap guards against this with generation counters (`.ref/xpis/beaver@jlegewie.com/bootstrap.js:19-60`) (medium). Keep `shutdown()` quick and idempotent.
- Pattern used by all four real plugins: in `shutdown`, `if (reason === APP_SHUTDOWN) return;` to skip expensive teardown.

### 2.2 Globals in the bootstrap scope

The scope is a `Cu.Sandbox` with the **system principal** (`plugins.js:137-183`) (high):
- **wantGlobalProperties:** `atob, btoa, Blob, crypto, CSS, ChromeUtils, DOMParser, fetch, File, FileReader, TextDecoder, TextEncoder, URL, URLSearchParams, XMLHttpRequest`. `fetch` brings `Request`, `Response` and `Headers` (medium).
- **Explicitly assigned:** `Zotero, ChromeWorker, IOUtils, Localization, PathUtils, Services, Worker, XMLSerializer, setTimeout, clearTimeout, setInterval, clearInterval, requestIdleCallback, cancelIdleCallback`, plus the reason constants.
- **`Components`, `Cc`, `Ci`, `Cu`:** available. zotmoov's `bootstrap.js` uses `Cc[...]`, `Ci.amIAddonManagerStartup` and runs active in Zotero 10. The Zotero 7 dev page also states it (high).
- **`ChromeUtils.import`:** removed. A Zotero shim warns and redirects to `importESModule` (`plugins.js:185-199`).
- **Not available:** `window`, `document`, `AbortController`, `navigator`. Get them from `Zotero.getMainWindow()`. zotseek and beaver build a `ctx` object for their bundle and set `ctx.document`/`navigator` themselves.
- **WASM:** `javascript.options.wasm_trustedprincipals = true` (`.ref/omni-gre/greprefs.js:392`), so `WebAssembly` works in this scope. Beaver compiles WASM in its main script with `WebAssembly.compile(await (await fetch(url)).arrayBuffer())`, where `url = "chrome://beaver/content/lib/…wasm"` (high).

Useful Zotero helpers (`zotero.js`, high):

| Helper | Source |
|---|---|
| `Zotero.getMainWindow()` (most recent `navigator:browser`) | `zotero.js:77` |
| `Zotero.getMainWindows()` | `zotero.js:84` |
| `Zotero.getActiveZoteroPane()` | `zotero.js:93` |
| `Zotero.debug(msg, level=3, maxDepth, stack)` | `zotero.js:1379` |
| `Zotero.logError(e)` | `zotero.js:1424` |
| `Zotero.warn(e)` | `zotero.js:1432` |
| `Zotero.Plugins.getRootURI(id)` / `resolveURI(id, rel)` | `plugins.js:310-328` |

### 2.3 Loading more code

- **Classic scripts.** `Services.scriptloader.loadSubScript(url, targetObj)` or `loadSubScriptWithOptions(url, {target, ignoreCache: true})`. Zotero itself loads `bootstrap.js` this way (`plugins.js:203-211`).
  - **Future-proofing (high):** Zotero `main` adds `allowUnsafeURL: true` to these calls with the comment *"Plugins are loaded from jar:file: URIs, which the script loader otherwise refuses (Mozilla bug 1974213)"*. Compare `gh api …plugins.js?ref=main` with `ref=10.0.6`.
  - Beaver's bootstrap says the same thing: Firefox 153 refuses `loadSubScript` of the XPI's own jar:/file: URIs, so Beaver loads through its chrome URL (`.ref/xpis/beaver@jlegewie.com/bootstrap.js:96-103`).
  - **Load plugin scripts via `chrome://<pkg>/content/...`**, not `rootURI + 'x.js'`.
- **Chrome package registration** (high: used by all four plugins and documented by Zotero):

```js
var chromeHandle;
const aomStartup = Cc["@mozilla.org/addons/addon-manager-startup;1"].getService(Ci.amIAddonManagerStartup);
chromeHandle = aomStartup.registerChrome(Services.io.newURI(rootURI + "manifest.json"), [
  ["content", "zotero-fulltext", rootURI + "content/"],          // → chrome://zotero-fulltext/content/
  // ["locale", "zotero-fulltext", "en-US", rootURI + "locale/en-US/"],  // only for .dtd/.properties; Fluent doesn't need it
]);
// shutdown:
chromeHandle?.destruct(); chromeHandle = null;
```

  Relative paths such as `"chrome/content/"` also work; they resolve against the manifest URI (zotmoov).
- **ES modules.** `ChromeUtils.importESModule("chrome://zotero-fulltext/content/x.mjs")` requires a chrome:// or resource:// URL.
  - **Caveat (medium):** such modules live in the shared system module map and **cannot be unloaded**. After a plugin update or reload, the old module instance stays cached for the session. Prefer classic subscripts (`ignoreCache:true`) for code that changes during development, or put the version in the URL.
  - Beaver uses dynamic `import("chrome://beaver/content/lib/sentencex/sentencex_wasm.js")` from its subscript.
- **resource:// substitution** (alternative to chrome, medium): standard Gecko API. Zotero's error attribution resolves both forms (`plugins.js:412-437`).

```js
const resProto = Services.io.getProtocolHandler("resource").QueryInterface(Ci.nsIResProtocolHandler);
resProto.setSubstitution("zotero-fulltext", Services.io.newURI(rootURI));   // resource://zotero-fulltext/
// shutdown: resProto.setSubstitution("zotero-fulltext", null);
```

### 2.4 Loading the Rust core (wasm-bindgen)

Real-world precedent (high): Beaver (`.ref/xpis/beaver@jlegewie.com/content/lib/sentencex/sentencex_wasm.js` + `_bg.wasm`, a wasm-bindgen `--target web` build). Its loader code is in `content/scripts/beaver.js` (minified):

```js
const binary = await (await fetch("chrome://beaver/content/lib/sentencex/sentencex_wasm_bg.wasm")).arrayBuffer();
const mod = await import("chrome://beaver/content/lib/sentencex/sentencex_wasm.js");
mod.initSync({ module: binary });
```

Recommendations for us:
- Use `wasm-bindgen --target web` (or `wasm-pack build --target web`).
- Ship `core.js` + `core_bg.wasm` under `content/wasm/` and load them as above. Passing bytes to `initSync` avoids `import.meta.url` and `instantiateStreaming` MIME issues.
- The glue needs `TextEncoder`/`TextDecoder`, which the sandbox has.
- Alternative (medium, needs a spike): `--target no-modules` loaded with `loadSubScriptWithOptions`. Top-level `let wasm_bindgen` from a subscript does **not** become a property of a non-global target object, so the glue needs a wrapper. The `--target web` route is the one proven in Zotero 10.
- Parsing work is cheap. A `ChromeWorker` is optional; Beaver passes precompiled `WebAssembly.Module`s to workers.

### 2.5 Cleanup checklist for `shutdown()`

| Item | What to do |
|---|---|
| Menus | Auto-unregistered (pluginAPIBase observer). Since Zotero 10, MenuManager also removes the menu DOM. You may still call `Zotero.MenuManager.unregisterMenu(key)`. |
| Pref panes | Auto-unregistered (`preferencePanes.js:183-199`). |
| FTL links | Remove from each main window: `win.document.querySelector('[href="zotero-fulltext.ftl"]')?.remove()`. Call `Zotero.ftl.removeResourceIds([...])` if you added to `Zotero.ftl`. |
| Chrome / resource | `chromeHandle.destruct()`; remove the resource substitution. |
| Pref observers | `Zotero.Prefs.unregisterObserver(symbol)`. |
| MIME handlers | `Zotero.MIMETypeHandler.removeHandlers('application/pdf', handler)`. |
| Login windows | Close any open ones; abort in-flight requests (`cancellerReceiver`). |
| Global object | `delete Zotero.ZoteroFullText`. |
| APP_SHUTDOWN | Skip all of the above. |

---

## 3. Item context menu

### 3.1 `Zotero.MenuManager` (Zotero 8+), exact implementation

Source: `chrome/content/zotero/xpcom/pluginAPI/menuManager.js` (846 lines) + `pluginAPIBase.mjs`. High confidence throughout.

```js
Zotero.MenuManager = {
  registerMenu(options)    // → string (namespaced key) | false
  unregisterMenu(key)      // → boolean   (pass the RETURNED key, not your menuID)
  updateMenuPopup(popupElem, targetType, args)   // internal, called by Zotero menus
}
```

**Options** (`menuManager.js:185-214`, `79-109`):
- **`menuID`** (string): unique per plugin. The stored key is `CSS.escape(`${pluginID}-${menuID}`)` (`pluginAPIBase.mjs:312-320`), and that is what `registerMenu` returns. Registering the same ID twice fails validation and logs a warning.
- **`pluginID`** (string).
- **`target`** (string), one of `VALID_TARGETS` (`menuManager.js:30-59`):

| Group | Targets |
|---|---|
| Main menubar | `main/menubar/file`, `edit`, `view`, `go`, `tools`, `help` |
| Library context menus | **`main/library/item`**, `main/library/collection` |
| Add attachment / note | `main/library/addAttachment`, `main/library/addNote` |
| Tab context menu | `main/tab` |
| Reader menubar | `reader/menubar/file`, `edit`, `view`, `go`, `window` |
| Item pane | `itemPane/info/row` |
| Notes pane | `notesPane/addItemNote`, `notesPane/addStandaloneNote` |
| Sidenav | `sidenav/locate` |

- **`menus`** (MenuData[], required and non-empty).
- **Grouped targets** (`main/library/item`, `main/library/collection`) forbid **top-level separators** (lines 233-239). Zotero adds its own separator before plugin items. It moves overflow into a "More" submenu when the menu would exceed 80% of screen height (`_groupMenus`/`_computeAvailableMenuNum`, 728-807).

**MenuData** (`_menusTypeDefinition`, 113-181):

| Key | Type | Notes |
|---|---|---|
| `menuType` | `"menuitem" \| "separator" \| "submenu"` | required |
| `l10nID` | string | becomes `data-l10n-id` on the element (line 453-455). There is **no raw `label` option**. |
| `l10nArgs` | string (JSON) | object form is deprecated (129-140) |
| `icon`, `darkIcon` | string URL | applied via CSS vars `--custom-menu-icon-light/dark` (467-483). Use a chrome:// SVG 16×16 with `fill="context-fill"`. |
| `enableForTabTypes` | string[] | only for `main/menubar/*` and `reader/menubar/*` (e.g. `"library"`, `"reader/pdf"`) |
| `onShowing`, `onShown`, `onHiding`, `onHidden` | `(event, context) => void` | popup lifecycle |
| `onCommand` | `(event, context) => void` | click |
| `menus` | MenuData[] | required for `submenu` |

All functions are wrapped in try/catch by `_validateObject` (`pluginAPIBase.mjs:279-291`), but **only synchronous throws are caught**. Async handlers must catch their own errors.

**The `context` object** (`menuManager.js:600-645`) is `defaultContext` merged with the target's `getContext()`:
- **defaultContext:** `menuElem` (getter), `setL10nArgs(str)`, `setEnabled(bool)`, `setVisible(bool)`, `setIcon(icon, darkIcon)`.
- **For `main/library/item`** (`zoteroPane.js:4685-4705`): `{ collectionTreeRows, items, tabType: "library", tabSubType: undefined, tabID: "zotero-pane" }`, plus a deprecated `collectionTreeRow` getter. That getter **throws** when more than one collection row is selected and otherwise warns (Zotero 10 multi-select change).
- **`items`** is `ZoteroPane.getSelectedItems()` captured when the menu is built (`zoteroPane.js:4231`). It may contain regular items, attachments and notes.

**When hooks fire:**
- For `main/library/item`, Zotero calls `updateMenuPopup` from `buildItemContextMenu()` without an event. `onShowing` is therefore attached as a one-shot `popupshowing` listener (`menuManager.js:591-594`, `706-719`) and fires when `openPopupAtScreen` runs (`zoteroPane.js:3669-3683`).
- `onCommand` is a `command` listener with `{once:true}` that is torn down on `popuphidden` (652-670).
- **If any selected item is an annotation, plugin menus are not added at all** (`zoteroPane.js:4679-4680`, early `return` before `updateMenuPopup`).

**Lifecycle:**
- Registrations are dropped automatically when the plugin shuts down (`pluginAPIBase.mjs:362-373`, `345-356`).
- `MenuManagerInternal._unregisterByPluginID` also removes DOM nodes from main and reader windows (`menuManager.js:304-323`). The Zotero 10 dev notes call this a fix.
- `notifyType: false` means there is no live refresh. Changes take effect next time the menu opens; to "update" a menu, unregister and re-register it (zotmoov does this).

**Recommended registration for zotero-fulltext:**

```js
this.menuKey = Zotero.MenuManager.registerMenu({
  menuID: "get-fulltext",
  pluginID: "zotero-fulltext@local",
  target: "main/library/item",
  menus: [
    {
      menuType: "menuitem",
      l10nID: "zotero-fulltext-menu-get",          // FTL: zotero-fulltext-menu-get = \n    .label = Get Full Text
      icon: "chrome://zotero-fulltext/content/icons/fulltext.svg",
      onShowing: (event, ctx) => {
        const parents = toParentItems(ctx.items);
        ctx.setVisible(parents.length > 0);
        ctx.setEnabled((ctx.collectionTreeRows ?? []).every(r => r.filesEditable !== false));
      },
      onCommand: (event, ctx) => {
        ZoteroFullText.run(toParentItems(ctx.items)).catch(e => Zotero.logError(e));
      },
    },
  ],
});
function toParentItems(items = []) {
  const out = new Map();
  for (const it of items) {
    const p = it.isRegularItem() ? it : it.parentItem;   // attachment/note → its parent
    if (p && p.isRegularItem() && !p.isFeedItem) out.set(p.id, p);
  }
  return [...out.values()];
}
```

`collectionTreeRow.filesEditable` is the property Zotero itself checks before enabling "Find Full Text" (`zoteroPane.js:4427-4431`).

Real-world MenuManager usage:
- zotmoov: `.ref/xpis/zotmoov@wileyy.com/src/03-zotmoov-menus-new.js:31-180`, feature-detected with `'MenuManager' in Zotero`.
- zotseek: Tools menu with an icon, in its minified `content/scripts/index.js`.

### 3.2 Legacy approach: inject into `#zotero-itemmenu` (for comparison only)

`#zotero-itemmenu` is defined in `zoteroPane.xhtml:977-1029`.

**Gotcha (high):** `buildItemContextMenu()` shows and hides built-in entries by **position**: `menu.childNodes[m.findFile]`, with indexes from a fixed `options` list (`zoteroPane.js:4157-4194`). Inserting anything **before** the built-ins breaks Zotero's menu, so only `append` at the end.

You must also:
- add the item to every window (`onMainWindowLoad` plus already-open windows);
- remove it on `onMainWindowUnload`/`shutdown`;
- read the selection yourself.

```js
function addLegacyMenu(win) {
  const doc = win.document;
  win.MozXULElement.insertFTLIfNeeded("zotero-fulltext.ftl");
  const popup = doc.getElementById("zotero-itemmenu");
  const mi = doc.createXULElement("menuitem");
  mi.id = "zotero-fulltext-legacy-get";
  mi.setAttribute("data-l10n-id", "zotero-fulltext-menu-get");
  mi.addEventListener("command", () => ZoteroFullText.run(toParentItems(win.ZoteroPane.getSelectedItems())));
  popup.append(mi);   // never insertBefore built-ins
  popup.addEventListener("popupshowing", e => {
    if (e.target !== popup) return;
    mi.hidden = toParentItems(win.ZoteroPane.getSelectedItems()).length === 0;
  });
}
// remove: win.document.getElementById("zotero-fulltext-legacy-get")?.remove()
```

Use MenuManager. It handles grouping, cleanup, the annotation exclusion and the multi-select context.

---

## 4. Preferences

### 4.1 Default prefs: `prefs.js` at the plugin root

- Loaded by `setDefaultPrefs(addon)` (`plugins.js:507-540`) on startup, install and enable into the **default branch**. Values of type boolean, string or int are allowed; anything else logs "Invalid type" (no floats).
- Cleared by `clearDefaultPrefs` on disable, uninstall and upgrade (543-566).
- Zotero 10 loads it with `ignoreCache: true`, so default changes apply after an update without a restart (Zotero 10 dev notes, high).

```js
// prefs.js
pref("extensions.zotero-fulltext.ezproxyPrefix", "");   // e.g. login URL prefix — filled by user
pref("extensions.zotero-fulltext.useUnpaywall", true);
pref("extensions.zotero-fulltext.unpaywallEmail", "");  // optional placeholder only if an API requires it
pref("extensions.zotero-fulltext.maxConcurrent", 2);
```

### 4.2 Reading, writing and observing prefs

`xpcom/prefs.js`, high:
- `Zotero.Prefs.get(pref, global)` (248-284) and `Zotero.Prefs.set(pref, value, global)` (290-328).
  - Without `global`, the key is prefixed with `ZOTERO_CONFIG.PREF_BRANCH = 'extensions.zotero.'` (`resource/config.mjs:17`).
  - `set` coerces to the existing pref type; it creates bool, string or int if the pref is absent.
  - For our own keys: `Zotero.Prefs.get('extensions.zotero-fulltext.useUnpaywall', true)`. Both conventions exist in the wild: zotmoov and make-it-red use `extensions.<name>.*` + `global=true`; beaver and zotseek use `extensions.zotero.<name>.*` without `global`.
- `Zotero.Prefs.clear(pref, global)` (331).
- `Zotero.Prefs.registerObserver(name, handler, global) → symbol` (485) and `unregisterObserver(symbol)` (498).

### 4.3 Preference pane

`xpcom/preferencePanes.js:97-165`, high:

```js
await Zotero.PreferencePanes.register({
  pluginID: "zotero-fulltext@local",                 // required
  src: "content/preferences.xhtml",                   // required; relative to rootURI or absolute (resolved by Zotero.Plugins.resolveURI)
  id: "zotero-fulltext-prefs",                        // optional; must be unique (else throws)
  label: "Full Text",                                 // optional RAW string (rawLabel), default = plugin name
  image: "content/icons/icon24.png",                  // optional; default = manifest icon at 24px
  scripts: ["content/preferences.js"],                // optional
  stylesheets: ["content/preferences.css"],           // optional
  helpURL: "https://…",                               // optional
  // parent: "<other pane id>"                        // optional sub-pane (hidden from sidebar)
});   // → Promise<paneID>
```

How the pane is loaded (`chrome/content/zotero/preferences/preferences.js`):
- **`src` is an XHTML fragment**, not a document. No `<!DOCTYPE>`. XUL is the default namespace; use `html:` for HTML. It is parsed with `MozXULElement.parseXULToFragment` (lines 331-338) and translated with `document.l10n.translateFragment` (353-362).
- **Each pane's `scripts` run in their own `Cu.Sandbox` with `sandboxPrototype: window`** (313-321). This changed in Zotero 8: no shared `var`s between panes.
- **`preference="extensions.zotero-fulltext.x"`** on an input or checkbox binds two-way. The value must be a **full pref key**, because Zotero reads it with `Zotero.Prefs.get(preference, true)` (`_syncFromPref` 446-463, `_syncToPrefOnModify` 465-476, `attachToPreference` 485-552).
  - Non-checkbox values are written back as **strings**. Into an existing int pref that goes through `setIntPref`, so prefer string or bool prefs for text inputs.
  - Events emitted: `syncfrompreference`, `beforesynctopreference`, `synctopreference`.
- Wrap content in top-level `<groupbox>` elements (search integration) and namespace every `id`/`class`/`data-l10n-id` (Zotero 7 docs, medium).

Minimal fragment (the zotmoov pattern, `.ref/xpis/zotmoov@wileyy.com/preferences/prefs.xhtml:7-20`):

```xml
<linkset><html:link rel="localization" href="zotero-fulltext.ftl"/></linkset>
<vbox onload="ZoteroFullTextPrefs?.init?.()">
  <groupbox>
    <label><html:h2 data-l10n-id="zotero-fulltext-pref-institution"/></label>
    <html:input type="text" id="zotero-fulltext-ezproxy" preference="extensions.zotero-fulltext.ezproxyPrefix"/>
    <checkbox id="zotero-fulltext-oa" native="true" preference="extensions.zotero-fulltext.useUnpaywall"
              data-l10n-id="zotero-fulltext-pref-use-unpaywall"/>
  </groupbox>
</vbox>
```

---

## 5. Localization (Fluent)

How Zotero 10 registers plugin FTL (`plugins.js:569-694`, high):
- All `.ftl` files **directly inside** `locale/<locale>/` are read into memory.
- **Subfolders are not scanned.** `readDirectory` returns immediate children, and non-`.ftl` names are skipped (604-613, 705-750).
- Every plugin's files merge into **one** mock `L10nFileSource` named `zotero-plugins` with pre-path `zotero-plugins:{locale}/` (48-49, 660-687).
- **The resource ID is just the filename, shared across all plugins.** Use a plugin-unique name such as `zotero-fulltext.ftl`. Generic names like `addon.ftl` or `mainWindow.ftl` can collide with another plugin.
- This replaced the per-plugin `L10nFileSource` used up to 8.0.x (the 9.0.6 and 10.0.6 tags have the unified version). The Zotero 7 docs' advice to "use a subfolder" for generic filenames **no longer works** (high, compared `plugins.js` at 8.0.5, 9.0.6 and 10.0.6 via `gh api`).
- Locale fallback per file runs: exact match → same language → en-US → first available (624-650). **Shipping only `locale/en-US/` is fine.**

Using FTL:
- **Menu labels** come from the `.label` attribute (Zotero 8 dev page; zotmoov's `zotmoov.ftl`):
  ```ftl
  zotero-fulltext-menu-get =
      .label = Get Full Text
  zotero-fulltext-progress-title = Getting full text
  ```
- **Main window:** the FTL must be linked into the window's l10n: `win.MozXULElement.insertFTLIfNeeded("zotero-fulltext.ftl")`. Do this in `onMainWindowLoad` and for windows already open at startup. Remove it on shutdown with `win.document.querySelector('[href="zotero-fulltext.ftl"]')?.remove()` (make-it-red, zotmoov, zotseek).
- **In your own XHTML** (pref pane, login window): `<html:link rel="localization" href="zotero-fulltext.ftl"/>`.
- **In JS, outside any window:** `Localization` is a scope global (`plugins.js:169`). Use `const l10n = new Localization(["zotero-fulltext.ftl"], true); l10n.formatValueSync("zotero-fulltext-progress-title")`.
- **For APIs that call `Zotero.getString(key)`** (ProgressQueue title and columns): `Zotero.getString` first tries `Zotero.ftl.formatValueSync(name)` (`intl.js:128-141`). `Zotero.ftl` is a sync `Localization` over Zotero's own FTLs (`intl.js:83-93`).
  - After `Zotero.ftl.addResourceIds(["zotero-fulltext.ftl"])`, your **message values** resolve via `Zotero.getString`. The Zotero 10 dev page documents this for undo labels; remove with `removeResourceIds` on shutdown.
  - An unknown key **throws** in en-US (`intl.js:174-177`).

**Simplest acceptable approach for en-US only:** one file, `locale/en-US/zotero-fulltext.ftl`, plus `insertFTLIfNeeded` in each main window, plus `l10nID` in MenuManager. Hard-coded English strings in ProgressWindow headlines and lines are acceptable, because those APIs take raw text.

---

## 6. Progress and feedback UI

### 6.1 `Zotero.ProgressWindow`

`xpcom/progressWindow.js:103-377`, high:

```js
const pw = new Zotero.ProgressWindow({ closeOnClick: false /* default true */, window /* default main window */ });
pw.changeHeadline(text, cssIconKey?, postText?);   // 157
pw.addLines(labelsOrLabel, iconsOrIcon);           // 189 — each becomes an ItemProgress at 100%
pw.addDescription(text);                           // 208 — <a> markup turned into links
const line = new pw.ItemProgress(itemType, text, parentItemProgress?);   // 285
line.setProgress(percent);        // 325 — 0<p<100 shows progress arcs; 100 shows the icon
line.setItemTypeAndIcon(itemType, cssIcon = 'item-type');   // 349 — class "icon icon-16 icon-css icon-<cssIcon>", data-item-type=itemType
line.setText(text);               // 356
line.setError();                  // 371 — red text (+ cross.png background, which no longer exists in the skin)
pw.show();                        // 120 — opens chrome://zotero/content/progressWindow.xhtml (openDialog, dependent)
pw.startCloseTimer(ms = 2500, requireMouseOver?);   // 245
pw.close();                       // 267
Zotero.ProgressWindowSet.closeAll();
```

Notes:
- **The first `ItemProgress` argument is an item type**, not an icon URL. Use `item.getItemTypeIconName()`, `'journalArticle'` or `'attachmentPDF'`; Zotero uses `'attachmentPDF'` at `attachments.js:1586`.
- The old `chrome://zotero/skin/tick.png`/`cross.png` **do not exist** in 10.0.6. Toolkit code passing them, such as zotseek's bundle, shows nothing.
- Methods are deferred until the window loads, so calling them before `show()` is fine (`_deferUntilWindowLoad`).

Pattern (as in `zotero.js:800-830` and `fulltext.js:1810-1825`):

```js
const pw = new Zotero.ProgressWindow({ closeOnClick: false });
pw.changeHeadline("Getting full text");
const rows = new Map(items.map(it => [it.id, new pw.ItemProgress(it.getItemTypeIconName(), it.getDisplayTitle())]));
pw.show();
// per item: rows.get(id).setProgress(50); …success: setItemTypeAndIcon('attachmentPDF'); setProgress(100) …failure: setError()
pw.startCloseTimer(4000);
```

### 6.2 `Zotero.ProgressQueue`: the "Find Available PDF" table dialog

`xpcom/progressQueue.js:26-240` + `progressQueueDialog.js`, high:

```js
let q = Zotero.ProgressQueues.get("zotero-fulltext") || Zotero.ProgressQueues.create({
  id: "zotero-fulltext",
  title: "zotero-fulltext-progress-title",         // passed to Zotero.getString → see §5 (Zotero.ftl.addResourceIds)
  columns: ["general.item", "attachment.fullText"] // Zotero's own string keys, as used by findFile
});
q.addListener("cancel", () => { cancelled = true; });   // events: empty, cancel, rowadded, nonempty, rowupdated, rowdeleted
q.addRow(item);                                          // row {id, status: ROW_QUEUED, fileName: item.getDisplayTitle(), message}
q.updateRow(item.id, Zotero.ProgressQueue.ROW_PROCESSING, "Unpaywall…");  // ROW_QUEUED=1, PROCESSING=2, FAILED=3, SUCCEEDED=4
const dlg = q.getDialog(); dlg.showMinimizeButton(false); dlg.open();     // dlg.setStatus(msg), dlg.close(), dlg.isOpen()
```

Zotero's own usage is in `Zotero.Attachments.addAvailableFiles` (`attachments.js:1512-1600`). The window title is `Zotero.getString(title)` (`progressQueueDialog.js:51`), and column labels are `Zotero.getString(columns[i])` (`components/progressQueueTable.js:97-102`).

### 6.3 Newer notification APIs

No toast, notification-box or message-bar API exists in Zotero 10's chrome code (a search for `toast|notificationbox|appendNotification|PopupNotifications|moz-message-bar` found nothing). Use the two options above, or prompts:
- `Services.prompt.alert(win, title, msg)`
- `Services.prompt.confirmEx(...)`
- `Zotero.Prompt` (`xpcom/prompt.js`)

---

## 7. Interactive SSO login window that shares cookies with `Zotero.HTTP`

### 7.1 How Zotero 10 hosts web pages itself

**`Zotero.openInViewer(uri, options)`** (`zotero.js:1309-1364`, high):
- **options:** `{ onLoad(doc), allowJavaScript, userContextId, customUserAgent }`.
- It opens `chrome://zotero/content/standalone/basicViewer.xhtml` through `nsIWindowWatcher.openWindow(…, "chrome,dialog=yes,resizable,centerscreen,menubar,scrollbars", arg)` and **returns the window**.
- If a viewer with the same URI and `userContextId` is already open, it focuses that one instead.
- The `<browser>` (`basicViewer.xhtml:247-254`) has `type="content" remote="false" maychangeremoteness="true" disableglobalhistory="true" messagemanagergroup="basicViewer"`.
- `basicViewer.js:33-60, 77-91`:
  - With `userContextId`, it recreates the browser with a `usercontextid` attribute.
  - With `customUserAgent`, it sets `browser.browsingContext.customUserAgent`.
  - It loads via `browser.loadURI(uri, {triggeringPrincipal: system})`.

**`Zotero.BrowserRequest`** (`xpcom/browserRequest.js`, new in Zotero 10, © 2026) is Zotero's own pattern for "show a real page, let the user act, detect completion, close". Used for Cloudflare and captcha challenges during Find Full Text; high confidence:

| Function | Lines | What it does |
|---|---|---|
| `clearChallengeInViewer(url, {userContextId, captchaLocator, customUserAgent})` | 322-409 | `win = Zotero.openInViewer(url, …)`; `browser = win.document.querySelector('browser')`; **polls `browser.currentURI.spec` every 500 ms**. Asks the **PageData actor** `querySelectorMatches` whether the challenge element is still present, and requires stability for `browserRequest.onLoadTimeout` (3000 ms) before declaring success. Rejects if the user closes the window (wm listener) and times out after `browserRequest.timeout` (60000 ms) (`defaults/preferences/zotero.js:1675-1676`). Always `win.close()` in `finally`. |
| `_loadAndWaitForCookieInViewer` | 194-243 | Completion = a **specific cookie appears or changes** (`Services.cookies.getCookiesFromHost(host, userContextId ? {userContextId} : {})`, `_readCookieValue` 245-263, `_pollForCookie` 299-310). |
| `downloadPDFViaViewer(url, path)` | 616-668 | Opens a viewer and registers `Zotero.MIMETypeHandler.addHandlers('application/pdf', handler, true)`. **The PDF the user ends up on is captured as a Blob** and written with `Zotero.File.putContentsAsync`. |
| `_makePDFMIMETypeHandler` | 525-565 | Restricts capture to *our* browser by comparing `channel.notificationCallbacks.getInterface(Ci.nsILoadContext).topFrameElement` (or the `loadGroup` one) with our `<browser>`. |
| `_loadAndSettle` | 444-523 | The same idea with a `HiddenBrowser` and a progress listener on `NOTIFY_LOCATION`. |

The **"KCL login → land on PDF"** flow is exactly `downloadPDFViaViewer`: open the EZproxy-wrapped URL, the user completes SSO, the publisher serves a PDF, and the MIME handler captures it. For a login-only window (then continue with `Zotero.HTTP`), use the cookie-poll and URL-poll pattern.

### 7.2 Cookie-jar sharing

High from source and docs; not yet verified empirically.
- `Zotero.HTTP.request` uses a real `XMLHttpRequest` in system code: *"All other requests use the Firefox cookie jar unless the caller passed { anon: true }"* (`http.js:276-283`). It isolates only when `options.userContextId` is set (`http.js:307-310`).
- `Zotero.HTTP.download()` uses `fetch` with `credentials: 'include'` (`http.js:718-719`).
- A viewer or browser **without** `userContextId` uses the same default jar (`userContextId 0`).
- The docs make the same point: `openInViewer`'s `userContextId` "isolate[s] the viewer's cookies into the same jar as a Zotero.HTTP.request or HiddenBrowser using the same ID" (`zotero.js:1316-1317`), and *"Cookies acquired by the browser remain in the shared jar keyed on userContextId; a subsequent Zotero.HTTP.request using the same ID will see them"* (`browserRequest.js:96-99`).
- **Therefore:** if `userContextId` is omitted on both sides, the EZproxy session cookie set during the top-level SSO navigation is sent by later `Zotero.HTTP.request`/`download`/`Zotero.Attachments.downloadFile` calls to the proxy host.
- `Zotero.HTTP.newCookieContext()` → `{id, getCookies(host), dispose()}` (`http.js:159-176`) gives an isolated jar for both sides. Useful for tests; **not** what we want for the persistent KCL session.
- **Cookie behaviour:** `network.cookie.cookieBehavior = 5` (dFPI/Total Cookie Protection; `defaults/preferences/zotero.js:943`); the user profile does not override it. First-party cookies from top-level navigation go to the unpartitioned jar, which system-principal XHR reads (medium: standard Gecko behaviour, not verified here). Cookies set inside third-party iframes would be partitioned, so they are irrelevant for EZproxy.

### 7.3 Why not just `openInViewer` for SSO

The `ExternalLinkHandler` JSWindowActor is registered for `messageManagerGroups: ["feedAbstract", "basicViewer"]` (`actors/ActorManager.mjs:51-61`).
- Its child intercepts **left clicks whose `event.target` is an `<a>`/`<area>`** (or `label.text-link`). Unless the link is `javascript:` or a same-page hash, it calls `preventDefault()` and asks the parent to `Zotero.launchURL(href)`, i.e. the **system browser** (`ExternalLinkHandlerChild.mjs:4-62`).
- Form posts, JS redirects and clicks on elements *inside* an anchor (where the target is a span) are not affected.
- **Risk:** an SSO page whose "Sign in with institution" control is a plain `<a>` would bounce to Edge or Chrome, where the session would land in the wrong cookie store.

The **PageData** actor (`actors/ActorManager.mjs:4-8`, `PageDataChild.mjs`) is registered globally with no group restriction, so it works in any `<browser>`. It answers `title, characterSet, bodyText, cookie, documentHTML, querySelectorMatches, channelInfo` (status code).

### 7.4 Recommended login window (own XHTML, driven from plugin code)

Use Zotero's minimal `standalone/profilerViewer.xhtml` (lines 1-87) as a template. It is a XUL `<window>` with one `<browser type="content" remote="false" maychangeremoteness="true" disableglobalhistory="true">` that loads with a system triggering principal.

`content/login.xhtml` (served as `chrome://zotero-fulltext/content/login.xhtml`):

```xml
<?xml version="1.0"?>
<?xml-stylesheet href="chrome://global/skin/" type="text/css"?>
<window xmlns="http://www.mozilla.org/keymaster/gatekeeper/there.is.only.xul"
        xmlns:html="http://www.w3.org/1999/xhtml"
        windowtype="zotero-fulltext:login" title="Sign in"
        style="min-width: 900px; min-height: 700px;" persist="screenX screenY width height sizemode">
  <linkset><html:link rel="localization" href="zotero-fulltext.ftl"/></linkset>
  <vbox flex="1">
    <hbox align="center" style="padding: 4px;">
      <label id="zft-status" flex="1" crop="center"/>
      <button id="zft-cancel" data-l10n-id="zotero-fulltext-login-cancel"/>
    </hbox>
    <browser id="zft-browser" type="content" flex="1" remote="false"
             maychangeremoteness="true" disableglobalhistory="true"/>
    <!-- deliberately NO messagemanagergroup="basicViewer" → no ExternalLinkHandler -->
  </vbox>
</window>
```

Driver code, in plugin scope:

```js
async function interactiveLogin(startURL, { isDone, timeoutMs = 5 * 60_000 }) {
  const win = Services.ww.openWindow(null, "chrome://zotero-fulltext/content/login.xhtml", "_blank",
    "chrome,centerscreen,resizable,dialog=no", null);
  await new Promise(r => win.addEventListener("load", r, { once: true }));
  Zotero.Utilities.Internal.activate(win);                    // focus (utilities_internal.js:2545)
  const browser = win.document.getElementById("zft-browser");
  // optional: browser.browsingContext.customUserAgent = Zotero.VersionHeader.getPlainFirefoxUA(); (zotero.js:2327)
  browser.loadURI(Services.io.newURI(startURL), {
    triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
  });
  let closedByUser = false;
  win.addEventListener("unload", () => { closedByUser = true; }, { once: true });
  win.document.getElementById("zft-cancel").addEventListener("command", () => win.close());
  const deadline = Date.now() + timeoutMs;
  try {
    while (!closedByUser && Date.now() < deadline) {
      await Zotero.Promise.delay(500);
      const url = browser.currentURI?.spec;            // works for remote browsers too
      let html = null;
      try {
        const actor = browser.browsingContext?.currentWindowGlobal?.getActor("PageData");
        html = actor ? await actor.sendQuery("documentHTML") : null;   // waits for document ready
      } catch (_) {}
      if (await isDone({ url, html, cookies: (h) => Services.cookies.getCookiesFromHost(h, {}) })) return { ok: true, url };
    }
    return { ok: false, reason: closedByUser ? "closed" : "timeout" };
  } finally {
    if (!win.closed) win.close();                      // programmatic close
  }
}
```

`isDone` should call the Rust core: `is_login_page(url, html)`, `is_proxied_publisher_url(url)`, or "EZproxy session cookie present for the proxy host".

**Event-driven alternatives.** Polling is what Zotero itself does, and it survives process swaps.
- **Progress listener:** `browser.webProgress.addProgressListener(listener, Ci.nsIWebProgress.NOTIFY_LOCATION | Ci.nsIWebProgress.NOTIFY_STATE_WINDOW)`. `browser.addProgressListener` delegates to `this.webProgress` (= `browsingContext.webProgress`) (`.ref/omni-gre/chrome/toolkit/content/global/elements/browser-custom-element.mjs:622-624, 938-948`).
  - Implement `onLocationChange(webProgress, request, location, flags)`. Ignore events where `!webProgress.isTopLevel` or where `flags & LOCATION_CHANGE_SAME_DOCUMENT`; this is the HiddenBrowser pattern (`HiddenBrowser.mjs:189-219`).
  - Use `QueryInterface: ChromeUtils.generateQI(["nsIWebProgressListener", "nsISupportsWeakReference"])`, and **keep a strong reference to the listener**.
  - Caveat (medium): a browser with `maychangeremoteness` can get a new browsing context on a process switch. Polling `currentURI` is robust, and so is the `pagetitlechanged` event used by basicViewer and Mendeley auth (`import/mendeley/authViewer.js:32-45`).
- **PDF capture:** copy Zotero's `_makePDFMIMETypeHandler` (topFrameElement check) and call `Zotero.MIMETypeHandler.addHandlers('application/pdf', handler, true)`. The handler is `{onStartRequest(name, _, channel), onContent: async (blob, name) => boolean}` (`mimeTypeHandler.js:95-141`). Remove it in `finally`.
- **Mendeley precedent:** Zotero's Mendeley OAuth viewer forces remoteness with `browser.setAttribute('remote','true'); browser.changeRemoteness({remoteType: E10SUtils.DEFAULT_REMOTE_TYPE}); browser.construct()`, then detects the redirect URL in `pagetitlechanged` (`authViewer.js:31-44`). It **deletes** cookies created during auth on unload (`clearCookies`, lines 15-26). **Do not copy that part**; we want to keep the EZproxy session.
- **Popups:** a bare window has no `window.open`/`target=_blank` handler; `profilerViewer.xhtml` comments on this. SAML redirect flows don't need popups (medium).

---

## 8. Dev workflow

### 8.1 Separate dev profile and data directory

Required by the HARD RULES: never touch the real profile or `~/Zotero`.

Flags:

| Flag | What it does | Source |
|---|---|---|
| `-datadir <absolute path>` (or `profile`) | Forces the data dir. The parent must exist. | `BrowserContentHandler.sys.mjs:476`, `dataDirectory.js:60-81` (high) |
| `-ZoteroDebugText` | Debug to stdout. | `BrowserContentHandler.sys.mjs:466-468`, `debug.js:43-55` (high) |
| `-ZoteroDebug` | Debug output window. | same (high) |
| `-jsconsole` | Browser console. | `DevToolsStartup.sys.mjs:517-518` (high) |
| `-jsdebugger` | Browser Toolbox. | `DevToolsStartup.sys.mjs:538` (high) |
| `-profile <dir>`, `-no-remote`, `-purgecaches` | Standard Gecko flags. | medium |

**Without `-datadir`, a fresh profile may default to the user's `~/Zotero`.** Always pass it, and also pin it in `user.js`.

```bash
# one-time setup (Git Bash); dev dirs inside the project
mkdir -p /c/Users/abora/zotero-fulltext/.dev/profile/extensions /c/Users/abora/zotero-fulltext/.dev/data
cat > /c/Users/abora/zotero-fulltext/.dev/profile/user.js <<'EOF'
user_pref("extensions.zotero.useDataDir", true);
user_pref("extensions.zotero.dataDir", "C:\\Users\\abora\\zotero-fulltext\\.dev\\data");
user_pref("extensions.autoDisableScopes", 0);       // else proxy-installed plugins come up userDisabled
user_pref("extensions.zotero.debug.log", true);
user_pref("devtools.chrome.enabled", true);
EOF
# extension proxy file: name = plugin id, content = absolute path of the plugin source root (folder with manifest.json)
printf '%s' 'C:\Users\abora\zotero-fulltext\addon' > "/c/Users/abora/zotero-fulltext/.dev/profile/extensions/zotero-fulltext@local"
# launch (separate process; does not touch the user's running Zotero)
"/c/Program Files/Zotero/zotero.exe" -profile 'C:\Users\abora\zotero-fulltext\.dev\profile' -no-remote \
  -datadir 'C:\Users\abora\zotero-fulltext\.dev\data' -ZoteroDebugText -jsconsole
```

**Why `autoDisableScopes=0` (high, source):**
- Zotero ships `extensions.autoDisableScopes = 15` and `extensions.startupScanScopes = 0` (`defaults/preferences/zotero.js:20-21`).
- In `XPIDatabase.addMetadata` (`.ref/omni-gre/modules/addons/XPIDatabase.sys.mjs:3253-3370`), an add-on found by a directory scan is a "detected" install, and therefore `foreignInstall`. If its location scope is in `autoDisableScopes` (profile scope = 1), it gets `userDisabled = true; seen = false`.
- With 15 the proxy-installed plugin appears **disabled**; enable it once in Tools → Plugins, or set the pref to 0 beforehand.

**Proxy file details:**
- Implemented in `XPIProvider.sys.mjs:1116-1218`. The file name must equal the add-on ID, which must pass `gIDTest`.
- The first line is an absolute (or relative-to-dir) path that must exist and be a directory.
- **If the path doesn't exist, Gecko deletes the pointer file** ("Deleting stale pointer file", lines 1199-1210).
- A directory named `<id>`, or a symlink, also works.

**Forcing a rescan:**
- `startupScanScopes=0` means Gecko only rescans `extensions/` when the app build changes.
- After adding or changing the proxy file, **close the dev Zotero and delete the `extensions.lastAppBuildId` and `extensions.lastAppVersion` lines from the dev profile's `prefs.js`** ([plugin_development](https://www.zotero.org/support/dev/client_coding/plugin_development), medium; consistent with the scan logic).
- On the user's real profile these lines exist (`lastAppBuildId=20261007153950`, `lastAppVersion=10.0.6`). **Do not edit that file.**

**Iteration loop:**
- **Edits to subscripts or FTL:** Tools → Plugins → disable/enable. `startup` re-runs, and FTL/prefs are re-registered (`plugins.js:900-919`).
- **Edits to `bootstrap.js` or `manifest.json`:** restart the dev Zotero (the scope is reused on enable).
- **Run JavaScript:** Tools → Developer → Run JavaScript (`zoteroPane.xhtml:759-767`); ideal for poking `Zotero.MenuManager`, `Zotero.HTTP`, etc. Tools → Plugins opens `about:addons` in a basicViewer (`zoteroPane.xhtml:757-758`).
- **Community tooling (optional, low):** `zotero-plugin-scaffold` / `zotero-plugin-template` automate the same proxy-file and profile dance plus hot reload.

### 8.2 Debug output

- `Zotero.debug(msg, level)` writes to the debug log when logging is enabled. Prefs (`defaults/preferences/zotero.js:1595-1602`): `extensions.zotero.debug.log` (stdout), `.debug.store` (in-memory, for Help menu viewing), `.debug.level=5`.
- **Help → Debug Output Logging → Enable / View Output** (`zoteroPane.xhtml:819-845`; [debug_output](https://www.zotero.org/support/debug_output), medium).
- `Zotero.logError(e)` and `Zotero.warn(msg)` also go to the Error Console (Tools → Developer → Error Console). Zotero attributes errors to plugins by stack (`Zotero.Plugins.getPluginFromError`, `plugins.js:346-379`).
- On Windows, run `zotero.exe -ZoteroDebugText > debug.txt` from a terminal. The docs say `-console` may be needed (medium).

### 8.3 Packaging the XPI

The XPI is a plain zip with **`manifest.json`, `bootstrap.js` and `prefs.js` at the zip root** (verified by extracting the four real XPIs; high).
- No signing is needed: `xpinstall.signatures.required=false` (`defaults/preferences/zotero.js:1483`).

```bash
cd /c/Users/abora/zotero-fulltext/addon && 7z a -tzip ../dist/zotero-fulltext-0.1.0.xpi ./* -xr!*.map
```

Layout:

```
manifest.json  bootstrap.js  prefs.js
locale/en-US/zotero-fulltext.ftl
content/main.js  content/login.xhtml  content/preferences.xhtml  content/preferences.js
content/icons/icon48.png  content/icons/icon96.png  content/icons/fulltext.svg
content/wasm/zft_core.js  content/wasm/zft_core_bg.wasm
```

Install: Tools → Plugins → gear → *Install Plugin From File…*, into the dev profile only.

---

## 9. Breaking changes for plugins, Zotero 7 → 8 → 9 → 10

### Zotero 8 (Firefox 115 → 128 → 140)

Source: [Zotero 8 for developers](https://www.zotero.org/support/dev/zotero_8_for_developers), medium.
- All JSMs became ESMs. `ChromeUtils.import()` is gone; Zotero shims it in the plugin scope (`plugins.js:185-199`, high). No global imports, and ESMs are strict mode.
- Bluebird was removed. `Zotero.Promise.delay`/`defer` remain (`defer()` is no longer a constructor). `Zotero.spawn` was removed.
- `Services.appShell.hiddenDOMWindow` was removed outside macOS.
- `ZOTERO_CONFIG` must be imported.
- **Preference panes run in their own global scope** (confirmed in `preferences.js:313-321`, high).
- **New `Zotero.MenuManager` custom menu API.**
- Firefox 128 changes: `XPCOMUtils.defineLazyGetter` → `ChromeUtils.defineLazyGetter`; `nsIScriptableUnicodeConverter` removed; set button labels via the `label` property; and others.

### Zotero 9 (same Firefox 140)

No major developer-facing changes; bump `strict_max_version` ([Zotero 9 for developers](https://www.zotero.org/support/dev/zotero_9_for_developers), medium). The unified plugin FTL source is already in the 9.0.6 tag (high, from `gh api`).

### Zotero 10 (same Firefox 140)

Source: [Zotero 10 for developers](https://www.zotero.org/support/dev/zotero_10_for_developers), medium; each item below was checked in local code (high).

| Area | Change |
|---|---|
| Collections list | Multiple selection. Singular getters **throw**: `ZoteroPane.getCollectionTreeRow()` → `getCollectionTreeRows()`, `getSelectedLibraryID()` → `getSelectedLibraryIDs()`, `getSelectedCollection()` → `getSelectedCollections()`, etc. |
| Items list | `ItemTree#collectionTreeRow` is gone (use `viewMode`). Library header rows appear in multi-library views. The `ItemTree` split into `ItemTree`/`ItemTreeRow`/`CollectionViewItemTree`. |
| Menu contexts | `collectionTreeRow` throws for multi-select; use `collectionTreeRows` (`zoteroPane.js:4689-4698`). |
| Cookies | **`Zotero.CookieSandbox` removed** → `Zotero.HTTP.newCookieContext()` (`http.js:159-176`). `translate.setCookieSandbox()` takes the numeric ID. `getFileFromDocument`/`saveURI` no longer take `cookieSandbox`. |
| Downloads | **`Zotero.HTTP.download()` streams via `fetch()` and returns a `Response`** (`http.js:614-650`). |
| Plugin FTL | Consolidated into one source with per-locale fallback (`plugins.js:569-694`). Consequences: unique filenames, no subfolders. |
| prefs.js | Plugin `prefs.js` is loaded with the script cache disabled. |
| MenuManager | Now removes a plugin's DOM nodes on shutdown. |
| Other | Search API (condition groups; `fulltextWord` removed); full-text index on FTS5 (`ftindex` attached DB); WAL mode; undo/redo via `saveTx({undoAction})`; local HTTP server hardening (`Host` must be localhost, browser-like requests need a `Zotero-Allowed-Request` header); `item.setType()` attachment/regular conversion throws; stored-file paths must be bare names. |
| New internal API | `Zotero.BrowserRequest` (captcha and interstitial handling with hidden browser → viewer escalation and PDF capture). Not documented for plugins, but a good pattern source (§7.1). |

### Coming next (Zotero `main`, high)

`allowUnsafeURL: true` is added to jar:/file: `loadSubScriptWithOptions` calls (Mozilla bug 1974213). Plugin code that loads its own scripts from `rootURI` jar:/file: URLs will need the same option, or chrome:// URLs. Beaver already notes that Firefox 153 refuses these.

---

## 10. Zotero internals worth reusing (pointers for later briefs)

All from `xpcom/attachments.js` (high):

| Function | Lines | Notes |
|---|---|---|
| `Zotero.Attachments.addFileFromURLs(item, urlResolvers, {onAccessMethodStart})` → attachment item or `false` | 1872 | **Feed it our own ranked resolver list**: OA URLs from the Rust core plus EZproxy-rewritten publisher URLs. Zotero handles downloading, translator-based PDF discovery on landing pages (`pageURL`), file sniffing (PDF/EPUB only), attachment creation and the `BrowserRequest` captcha escalation. Resolver objects: `{url?, pageURL?, accessMethod, articleVersion?}`, or (async) functions returning arrays (`downloadFirstAvailableFile` docs, ~1960-1978). |
| `getFileResolvers(item, methods=['doi','url','oa','custom'])` | 1275 | Zotero's built-in DOI/URL/OA (Zotero's Unpaywall-backed service) and custom resolvers. Custom resolvers come from pref `extensions.zotero.findPDFs.resolvers` (`defaults/preferences/zotero.js:1610`). |
| `canFindFileForItem(item)` | 1250-1255 | regular, non-feed, has DOI/URL/PMCID, no existing PDF/EPUB |
| `importFromURL({libraryID, url, parentItemID, title, fileBaseName, contentType, referrer})` | 528 | |
| `importFromFile({file, parentItemID, title, contentType})` | 63 | |
| `downloadFile(url, path, {referrer, enforceFileType, shouldDisplayCaptcha})` | 1176 | |

Other useful pieces:
- `HiddenBrowser` (`chrome://zotero/content/HiddenBrowser.mjs`, import via `ChromeUtils.importESModule`):
  - Options: `new HiddenBrowser({userContextId, customUserAgent, allowJavaScript, blockRemoteResources})`.
  - Methods: `await hb._createdPromise; await hb.load(url, {requireSuccessfulStatus:true}); await hb.getPageData(['documentHTML','channelInfo']); hb.destroy()` (lines 47-336).
  - Gives JS-rendered landing-page HTML for the Rust parser.
- `Zotero.Proxies` (`xpcom/proxy.js:35-243`): Zotero's own proxy rewriting (`properToProxy`, `proxyToProper`, `getPotentialProxies`). Overlaps with our EZproxy logic; covered in another brief.

---

## Appendix A: minimal bootstrap.js for zotero-fulltext

```js
/* global Zotero, Services, Cc, Ci, APP_SHUTDOWN */
var chromeHandle, menuKey, ctx;
const FTL = "zotero-fulltext.ftl";

function install() {}
function uninstall() {}

async function startup({ id, version, rootURI }, reason) {
  const aom = Cc["@mozilla.org/addons/addon-manager-startup;1"].getService(Ci.amIAddonManagerStartup);
  chromeHandle = aom.registerChrome(Services.io.newURI(rootURI + "manifest.json"),
    [["content", "zotero-fulltext", rootURI + "content/"]]);

  ctx = { rootURI, id, version, Zotero };
  ctx._globalThis = ctx;
  Services.scriptloader.loadSubScriptWithOptions("chrome://zotero-fulltext/content/main.js",
    { target: ctx, ignoreCache: true });            // main.js defines ctx.ZoteroFullText (and loads WASM lazily)
  Zotero.ZoteroFullText = ctx.ZoteroFullText;

  await Zotero.PreferencePanes.register({ pluginID: id, src: "content/preferences.xhtml",
    scripts: ["content/preferences.js"] });

  menuKey = Zotero.MenuManager.registerMenu({ menuID: "get-fulltext", pluginID: id, target: "main/library/item",
    menus: [{ menuType: "menuitem", l10nID: "zotero-fulltext-menu-get",
      onShowing: (e, c) => c.setVisible(ctx.ZoteroFullText.eligible(c.items).length > 0),
      onCommand: (e, c) => ctx.ZoteroFullText.run(c.items).catch(err => Zotero.logError(err)) }] });

  for (const win of Zotero.getMainWindows()) if (win.ZoteroPane) onMainWindowLoad({ window: win });
}

function onMainWindowLoad({ window }) { window.MozXULElement.insertFTLIfNeeded(FTL); }
function onMainWindowUnload({ window }) { /* drop window refs */ }

function shutdown({ id }, reason) {
  if (reason === APP_SHUTDOWN) return;
  try { ctx?.ZoteroFullText?.shutdown(); } catch (e) { Zotero.logError(e); }
  if (menuKey) { Zotero.MenuManager.unregisterMenu(menuKey); menuKey = null; }  // also auto-removed
  for (const win of Zotero.getMainWindows()) win.document.querySelector(`[href="${FTL}"]`)?.remove();
  delete Zotero.ZoteroFullText;
  chromeHandle?.destruct(); chromeHandle = null; ctx = null;
}
```

---

## Appendix B: provenance of reference material

- Zotero app chrome: `C:\Users\abora\zotero-fulltext\.ref\omni-app\` (extracted omni.ja, 10.0.6). Gecko: `C:\Users\abora\zotero-fulltext\.ref\omni-gre\`.
- Plugin XPIs (copied read-only from the user profile): `C:\Users\abora\zotero-fulltext\.ref\xpis\{beaver@jlegewie.com, zotmoov@wileyy.com, zotron@diamondrill, zotseek@zotero.org}\`.
- GitHub: `zotero/zotero` tags `8.0.5`, `9.0.6`, `10.0.6` and branch `main` (`plugins.js`, compared via `gh api`); `zotero/make-it-red` `src-2.0/`.
- Docs:
  - https://www.zotero.org/support/dev/zotero_7_for_developers
  - https://www.zotero.org/support/dev/zotero_8_for_developers
  - https://www.zotero.org/support/dev/zotero_9_for_developers
  - https://www.zotero.org/support/dev/zotero_10_for_developers
  - https://www.zotero.org/support/dev/client_coding/plugin_development
  - https://www.zotero.org/support/debug_output
  - https://www.zotero.org/support/kb/multiple_profiles
