#!/usr/bin/env bash
# Build the web client and assemble the servable site in _site/.
# Usage: tools/build-web.sh [--debug]
#
# Works on macOS (Apple Silicon dev box) and Linux x86_64 (GitHub CI). The
# wasm-bindgen CLI version MUST match the wasm-bindgen crate in Cargo.lock
# (the CLI rejects a mismatched module); both are pinned — bump together:
#   - client/Cargo.toml   wasm-bindgen = "=X.Y.Z"
#   - here                WASM_BINDGEN_VERSION
# The CLI is fetched as a prebuilt binary into target/tools/ (NOT installed
# globally — other projects on this machine pin different versions).
set -euo pipefail
cd "$(dirname "$0")/.."

WASM_BINDGEN_VERSION=0.2.126

profile=release
profile_flag=--release
if [[ "${1:-}" == "--debug" ]]; then
  profile=debug
  profile_flag=""
fi

case "$(uname -sm)" in
  "Darwin arm64") triple=aarch64-apple-darwin ;;
  "Darwin x86_64") triple=x86_64-apple-darwin ;;
  "Linux x86_64") triple=x86_64-unknown-linux-musl ;;
  "Linux aarch64") triple=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported host: $(uname -sm)" >&2; exit 1 ;;
esac

wb_dir="target/tools/wasm-bindgen-${WASM_BINDGEN_VERSION}-${triple}"
wb="${wb_dir}/wasm-bindgen"
if [[ ! -x "$wb" ]]; then
  echo "fetching wasm-bindgen ${WASM_BINDGEN_VERSION} (${triple})..."
  mkdir -p target/tools
  curl -sSfL "https://github.com/rustwasm/wasm-bindgen/releases/download/${WASM_BINDGEN_VERSION}/wasm-bindgen-${WASM_BINDGEN_VERSION}-${triple}.tar.gz" \
    | tar xzf - -C target/tools
fi
"$wb" --version

cargo build -p army-ghosts-client --locked \
  --target wasm32-unknown-unknown \
  --no-default-features --features default,web \
  $profile_flag

rm -rf _site
mkdir -p _site/target
"$wb" --out-dir _site/target --out-name wasm --target web --no-typescript \
  "target/wasm32-unknown-unknown/${profile}/army-ghosts-client.wasm"
cp client/index.html _site/
cp -r client/assets _site/assets

echo
echo "done → _site/ ($(du -h _site/target/wasm_bg.wasm | cut -f1) wasm)"
echo "serve locally:  python3 -m http.server -d _site 8080"
