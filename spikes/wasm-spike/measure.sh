#!/usr/bin/env bash
# Builds every size variant and copies the .wasm into out/. Run from Git Bash.
set -euo pipefail
cd "$(dirname "$0")"
export CARGO_TARGET_DIR="$PWD/target"   # keep the spike out of the global target dir
export RUSTC_WRAPPER=""                 # disable kache so timings are honest
T=wasm32-unknown-unknown
OUT=out
mkdir -p "$OUT"
log() { echo "[$(date +%H:%M:%S)] $*"; }

build() { # name profile pkg cargo-args...
  local name=$1 profile=$2 pkg=$3; shift 3
  local s=$(date +%s.%N)
  if [ "$pkg" = fulltext-core ]; then
    # crate-type = ["cdylib","rlib"] costs ~14% size; force a cdylib-only build
    cargo rustc -q -p "$pkg" --lib --crate-type cdylib --profile "$profile" --target $T "$@"
  else
    cargo build -q -p "$pkg" --profile "$profile" --target $T "$@"
  fi
  local e=$(date +%s.%N)
  local dir=$profile; [ "$profile" = dev ] && dir=debug
  local file="target/$T/$dir/${pkg//-/_}.wasm"
  cp "$file" "$OUT/$name.wasm"
  printf "%-28s %9d B  build %ss\n" "$name" "$(stat -c %s "$OUT/$name.wasm")" "$(awk "BEGIN{printf \"%.1f\", $e - $s}")"
}

log "ladder (opt-level z)"
build ladder-abi-only   release ladder
build ladder-json       release ladder --features json
build ladder-json-url   release ladder --features json,url

log "core per HTML backend (opt-level z)"
build core-none         release fulltext-core --no-default-features
build core-hand         release fulltext-core --no-default-features --features hand
build core-tl           release fulltext-core --no-default-features --features tl
build core-html5gum     release fulltext-core --no-default-features --features html5gum
build core-lolhtml      release fulltext-core --no-default-features --features lolhtml
build core-scraper      release fulltext-core --no-default-features --features scraper
build core-hand-debugpanic release fulltext-core --features debug-panic

log "opt-level comparison (core-hand)"
build core-hand-s       release-s  fulltext-core --no-default-features --features hand
build core-hand-o3      release-o3 fulltext-core --no-default-features --features hand
build core-html5gum-s   release-s  fulltext-core --no-default-features --features html5gum

log "cdylib+rlib penalty (plain cargo build of the dual crate-type)"
s=$(date +%s.%N); cargo build -q -p fulltext-core --release --target $T --no-default-features --features hand; cp target/$T/release/fulltext_core.wasm $OUT/core-hand-dualtype.wasm
printf "%-28s %9d B\n" core-hand-dualtype "$(stat -c %s $OUT/core-hand-dualtype.wasm)"

log "idna_adapter backends (core-hand, ladder-json-url)"
cp Cargo.lock Cargo.lock.bak
for v in 1.1.0 1.0.0; do
  cargo update -q -p idna_adapter --precise $v
  build ladder-json-url-idna$v release ladder --features json,url
  build core-hand-idna$v       release fulltext-core --no-default-features --features hand
  build core-html5gum-idna$v   release fulltext-core --no-default-features --features html5gum
done
cp Cargo.lock.bak Cargo.lock && rm Cargo.lock.bak

log "wasm-opt (binaryen version_117 from wasm-pack cache)"
WASM_OPT=${WASM_OPT:-/c/Users/abora/AppData/Local/.wasm-pack/wasm-opt-cdcb9e877b68d02e/bin/wasm-opt.exe}
FEAT="--enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue"
mkdir -p $OUT/opt
for n in ladder-abi-only ladder-json ladder-json-url core-hand core-hand-idna1.0.0 core-tl core-html5gum core-html5gum-idna1.0.0 core-lolhtml core-scraper; do
  s=$(date +%s%N); "$WASM_OPT" $FEAT -Oz "$OUT/$n.wasm" -o "$OUT/opt/$n.Oz.wasm"; e=$(date +%s%N)
  a=$(stat -c %s "$OUT/$n.wasm"); b=$(stat -c %s "$OUT/opt/$n.Oz.wasm")
  ga=$(gzip -9c "$OUT/$n.wasm" | wc -c); gb=$(gzip -9c "$OUT/opt/$n.Oz.wasm" | wc -c)
  printf "%-26s raw %8d -> %8d (%6.1f%%)  gzip %7d -> %7d (%6.1f%%)  %5d ms\n" $n $a $b $(awk "BEGIN{print 100*($b/$a-1)}") $ga $gb $(awk "BEGIN{print 100*($gb/$ga-1)}") $(( (e-s)/1000000 ))
done
