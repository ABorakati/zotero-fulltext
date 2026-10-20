// "twiggy top"-lite: node js/wasm-top.mjs unstripped.wasm [N]
// Uses the `name` custom section to attribute code-section bytes to functions,
// then groups by crate (first path segment of the demangled-ish symbol).
import { readFileSync } from "node:fs";

const buf = readFileSync(process.argv[2]);
const topN = Number(process.argv[3] ?? 25);
let pos = 8;
const leb = () => { let r = 0, s = 0, b; do { b = buf[pos++]; r |= (b & 0x7f) << s; s += 7; } while (b & 0x80); return r >>> 0; };
let importedFuncs = 0;
const bodies = [];
const names = new Map();
let dataSize = 0;
while (pos < buf.length) {
  const id = buf[pos++];
  const size = leb();
  const end = pos + size;
  if (id === 2) { // imports
    const n = leb();
    for (let i = 0; i < n; i++) {
      pos += leb(); pos += leb(); // module, name
      const kind = buf[pos++];
      if (kind === 0) { leb(); importedFuncs++; } else if (kind === 1) { pos++; const f = leb(); leb(); if (f & 1) leb(); }
      else if (kind === 2) { const f = leb(); leb(); if (f & 1) leb(); } else if (kind === 3) { pos += 2; }
    }
  } else if (id === 10) {
    const n = leb();
    for (let i = 0; i < n; i++) { const s = leb(); bodies.push(s); pos += s; }
  } else if (id === 11) {
    dataSize = size;
  } else if (id === 0) {
    const nl = leb();
    const nm = buf.subarray(pos, pos + nl).toString(); pos += nl;
    if (nm === "name") {
      while (pos < end) {
        const sub = buf[pos++]; const ss = leb(); const se = pos + ss;
        if (sub === 1) { const c = leb(); for (let i = 0; i < c; i++) { const idx = leb(); const l = leb(); names.set(idx, buf.subarray(pos, pos + l).toString()); pos += l; } }
        pos = se;
      }
    }
  }
  pos = end;
}
const fns = bodies.map((s, i) => ({ size: s, name: names.get(i + importedFuncs) ?? `func${i}` }));
const total = fns.reduce((a, f) => a + f.size, 0);
const crateOf = (n) => {
  const m = n.match(/^_ZN(\d+)([A-Za-z0-9_$.]+)/);
  let c = m ? m[2].slice(0, Number(m[1])) : n.split("::")[0];
  if (c.startsWith("_$LT$")) { const mm = n.match(/_\$LT\$(?:\d+)?([a-z0-9_]+)/); c = "<" + (mm ? mm[1] : "?"); }
  return c;
};
const byCrate = new Map();
for (const f of fns) { const c = crateOf(f.name); byCrate.set(c, (byCrate.get(c) ?? 0) + f.size); }
console.log(`code bytes: ${total}  data bytes: ${dataSize}  functions: ${fns.length}`);
console.log("-- by crate (first path segment) --");
[...byCrate].sort((a, b) => b[1] - a[1]).slice(0, topN).forEach(([c, s]) => console.log(`${String(s).padStart(8)} ${(100 * s / total).toFixed(1).padStart(5)}%  ${c}`));
console.log("-- top functions --");
fns.sort((a, b) => b.size - a.size).slice(0, topN).forEach((f) => console.log(`${String(f.size).padStart(8)}  ${f.name.slice(0, 140)}`));
