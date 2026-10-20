// Usage: node js/wasm-info.mjs a.wasm [b.wasm ...]
// Prints size, gzip size, imports, exports and per-section byte counts.
import { readFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { basename } from "node:path";

const SECTION = ["custom", "type", "import", "function", "table", "memory", "global", "export", "start", "element", "code", "data", "datacount", "tag"];

function leb(buf, pos) {
  let result = 0, shift = 0, b;
  do { b = buf[pos++]; result |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80);
  return [result >>> 0, pos];
}

function sections(buf) {
  const out = [];
  let pos = 8;
  while (pos < buf.length) {
    const id = buf[pos++];
    let size;
    [size, pos] = leb(buf, pos);
    let name = SECTION[id] ?? `id${id}`;
    if (id === 0) {
      let [nlen, p2] = leb(buf, pos);
      name = "custom:" + buf.subarray(p2, p2 + nlen).toString("utf8");
    }
    out.push({ name, size });
    pos += size;
  }
  return out;
}

const json = process.argv.includes("--json");
const rows = [];
for (const f of process.argv.slice(2).filter((a) => !a.startsWith("--"))) {
  const buf = readFileSync(f);
  const mod = new WebAssembly.Module(buf); // also validates
  const imports = WebAssembly.Module.imports(mod).map((i) => `${i.module}.${i.name}:${i.kind}`);
  const exports = WebAssembly.Module.exports(mod).map((e) => `${e.name}:${e.kind}`);
  const secs = sections(buf);
  const row = {
    file: basename(f),
    bytes: buf.length,
    gzip: gzipSync(buf, { level: 9 }).length,
    imports,
    exports,
    sections: Object.fromEntries(secs.map((s) => [s.name, s.size])),
  };
  rows.push(row);
  if (!json) {
    console.log(`${row.file}: ${row.bytes} B (gzip ${row.gzip} B) imports=${imports.length ? imports.join(",") : "NONE"}`);
    console.log(`   exports: ${exports.join(", ")}`);
    console.log(`   sections: ${secs.map((s) => `${s.name}=${s.size}`).join(" ")}`);
  }
}
if (json) console.log(JSON.stringify(rows, null, 2));
