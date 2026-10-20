#!/usr/bin/env bash
# Fail if a wasm artifact is not exactly what a release build of the current
# crates produces.
#
# Usage: bash scripts/check-wasm-fresh.sh [ARTIFACT_DIR]   (default: js/wasm)
#
# Rebuilds in release mode into a temp dir (via scripts/build-wasm.sh) and
# byte-compares fql_emu_bg.wasm plus the generated glue (fql_emu.js and the
# .d.ts files) against ARTIFACT_DIR.
#
# Byte comparison is sound because the build is reproducible: release builds
# of the same sources are byte-identical across target dirs, CARGO_HOMEs and
# checkout paths (build-wasm.sh remaps the embedded panic paths), *provided*
# the toolchain matches: rustc, wasm-bindgen (pinned by Cargo.lock) and
# wasm-opt (binaryen). CI pins rustc 1.95.0, wasm-pack 0.15.0 and binaryen
# 132; build the artifact you publish with the same versions. On a mismatch
# the script also diffs the exported function list, to tell an API change
# from an internal one or from toolchain drift.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARTIFACT="$(cd "${1:-$ROOT/js/wasm}" && pwd)"
FILES=(fql_emu_bg.wasm fql_emu.js fql_emu.d.ts fql_emu_bg.wasm.d.ts)

for f in "${FILES[@]}"; do
  if [ ! -f "$ARTIFACT/$f" ]; then
    echo "check-wasm-fresh: $ARTIFACT/$f is missing; run: bash scripts/build-wasm.sh" >&2
    exit 1
  fi
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "check-wasm-fresh: rebuilding (release) into $TMP/out ..."
if ! WASM_OUT_DIR="$TMP/out" bash "$ROOT/scripts/build-wasm.sh" --release >"$TMP/build.log" 2>&1; then
  cat "$TMP/build.log" >&2
  echo "check-wasm-fresh: rebuild failed" >&2
  exit 1
fi

stale=()
for f in "${FILES[@]}"; do
  cmp -s "$ARTIFACT/$f" "$TMP/out/$f" || stale+=("$f")
done
if [ ${#stale[@]} -eq 0 ]; then
  echo "check-wasm-fresh: $ARTIFACT is fresh ($(wc -c <"$ARTIFACT/fql_emu_bg.wasm") bytes)"
  exit 0
fi

echo "check-wasm-fresh: STALE: ${stale[*]} in $ARTIFACT differ from a fresh release build" >&2
exports() {
  node -e 'const fs = require("fs");
    const m = new WebAssembly.Module(fs.readFileSync(process.argv[1]));
    for (const e of WebAssembly.Module.exports(m).map((e) => e.kind + " " + e.name).sort()) console.log(e)' "$1"
}
if command -v node >/dev/null 2>&1; then
  if diff <(exports "$ARTIFACT/fql_emu_bg.wasm") <(exports "$TMP/out/fql_emu_bg.wasm") >"$TMP/exports.diff"; then
    echo "  exported functions are identical: internal code changed, or the toolchain differs" >&2
    echo "  (this machine: $(rustc --version 2>/dev/null || echo 'rustc ?'); see grep 'wasm-opt' below)" >&2
    grep -i "wasm-opt" "$TMP/build.log" >&2 || true
  else
    echo "  exported functions changed (< artifact, > fresh build):" >&2
    sed 's/^/    /' "$TMP/exports.diff" >&2
  fi
fi
echo "  fix: bash scripts/build-wasm.sh --release" >&2
exit 1
