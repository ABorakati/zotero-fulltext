// Message ids of the plugin FTL (no Fluent parser needed: ids start at column 0). Owner: js-shell.
import fs from 'node:fs';
import path from 'node:path';
import { PLUGIN } from './zft-env.mjs';

export const FTL_PATH = path.join(PLUGIN, 'locale', 'en-US', 'zotero-fulltext.ftl');

/** @returns {Set<string>} every message id defined in zotero-fulltext.ftl */
export function ftlIds() {
  const ids = new Set();
  for (const line of fs.readFileSync(FTL_PATH, 'utf8').split(/\r?\n/)) {
    const m = line.match(/^([a-z][a-z0-9-]*)\s*=/);
    if (m) ids.add(m[1]);
  }
  return ids;
}
