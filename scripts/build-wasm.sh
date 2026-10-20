#!/usr/bin/env bash
# Build the wasm artifact into js/wasm/. Requires wasm-pack (or run via nix:
#   nix shell nixpkgs#wasm-pack nixpkgs#wasm-bindgen-cli nixpkgs#binaryen nixpkgs#lld --command bash scripts/build-wasm.sh
# ).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
if ! command -v wasm-pack >/dev/null 2>&1; then
  if command -v nix >/dev/null 2>&1; then
    exec nix --extra-experimental-features 'nix-command flakes' shell \
      nixpkgs#wasm-pack nixpkgs#wasm-bindgen-cli nixpkgs#binaryen nixpkgs#lld \
      --command bash "$0" "$@"
  fi
  echo "wasm-pack not found; install it (cargo install wasm-pack) or use nix" >&2
  exit 1
fi
PROFILE="${1:---release}"
wasm-pack build crates/fql-emu-wasm --target nodejs --out-dir "$ROOT/js/wasm" --out-name fql_emu "$PROFILE"
# wasm-pack writes a package.json and .gitignore we don't want inside js/wasm.
rm -f "$ROOT/js/wasm/.gitignore" "$ROOT/js/wasm/README.md"
# js/ is an ESM package; the generated loader is CommonJS.
echo '{ "type": "commonjs" }' > "$ROOT/js/wasm/package.json"
ls -la "$ROOT/js/wasm"
