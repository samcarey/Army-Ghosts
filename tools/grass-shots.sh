#!/usr/bin/env bash
# Photograph the concealment rig: a one-hex wall of grass, a pawn either side,
# nothing else on the map.
# Usage: tools/grass-shots.sh [outdir]        (default target/grass-shots)
#
# The companion to tools/grass-table.sh — same scene, same constants, so this
# shows what the numbers there look like. It runs the table first and takes the
# depths AND the captions from it, which is why they can never drift apart.
#
# The scene is `Scenario::GrassStrip` (sim/src/lib.rs), reachable in any build:
#   ?scenario=strip:<depth>:<east stance>     web, offline only
#   AG_SCENARIO=strip:52:2                    native
# Run it after touching GRASS_EXTINCTION, GRASS_NEAR_T, STANCE_HEIGHT, HEX_R,
# grass.wgsl or gen_assets.py's grass tile.
#
# Needs: a built _site (tools/build-web.sh) and playwright. Playwright is not a
# repo dependency — install it wherever and point AG_NODE_PATH at the
# node_modules, e.g.
#   mkdir -p ~/.pw && cd ~/.pw && npm init -y && npm install playwright
#   AG_NODE_PATH=~/.pw/node_modules tools/grass-shots.sh
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-target/grass-shots}
# Not 8080/8099: this Mac already serves other projects there, and python's
# http.server fails to bind SILENTLY — you end up screenshotting someone else's
# site and wondering why the grass looks wrong.
port=${AG_PORT:-8123}

if [[ -n "${AG_NODE_PATH:-}" ]]; then
  export NODE_PATH="$AG_NODE_PATH"
fi
if ! node -e "require('playwright')" 2>/dev/null; then
  echo "grass-shots: playwright not found. See the header of this script." >&2
  exit 1
fi
if [[ ! -f _site/index.html ]]; then
  echo "grass-shots: no _site build — run tools/build-web.sh first." >&2
  exit 1
fi

mkdir -p "$out"
tools/grass-table.sh "$out/table.md" >/dev/null

python3 -m http.server -d _site "$port" >/dev/null 2>&1 &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
for _ in $(seq 20); do
  curl -sf "http://127.0.0.1:$port/" | grep -q "Army Ghosts" && break
  sleep 0.25
done
if ! curl -sf "http://127.0.0.1:$port/" | grep -q "Army Ghosts"; then
  echo "grass-shots: nothing serving Army Ghosts on $port (already in use?)." >&2
  exit 1
fi

node tools/grass-shots.js "$out/table.md" "$out" "http://127.0.0.1:$port"
