#!/usr/bin/env bash
# Regenerate the grass-strip concealment table.
# Usage: tools/grass-table.sh [outfile]
#
# Two pawns either side of a one-hex-wide strip of grass, one clear hex off it,
# measured in every stance pairing at every grass depth. The numbers are the
# sprite alpha `vision::fade_hidden` would write, so this is what tuning
# GRASS_EXTINCTION / GRASS_NEAR_T / GRASS_SAMPLES / HEX_R / STANCE_HEIGHT /
# GRASS_MAX_H actually does to visibility. Run it before and after a tweak.
#
# The rig itself is client/src/vision/strip_table.rs (scene layout and the
# properties it asserts); nothing here is part of the game build.
set -euo pipefail
cd "$(dirname "$0")/.."

log=$(mktemp -t grass-table)
trap 'rm -f "$log"' EXIT

# The test prints the table and *then* asserts the properties it shows, so a
# failure still leaves the table in the log to explain itself.
if ! cargo test --locked -p army-ghosts-client --features native \
    vision::strip_table -- --nocapture >"$log" 2>&1; then
  cat "$log" >&2
  echo "grass-table: the rig's own assertions failed — table above, if it got that far." >&2
  exit 1
fi

table=$(awk '/^<<<GRASS-TABLE$/{on=1; next} /^>>>GRASS-TABLE$/{on=0} on' "$log")
if [[ -z "$table" ]]; then
  cat "$log" >&2
  echo "grass-table: the test passed but printed no table (sentinels changed?)." >&2
  exit 1
fi

printf '%s\n' "$table"
if [[ -n "${1:-}" ]]; then
  printf '%s\n' "$table" >"$1"
  echo "grass-table: written to $1" >&2
fi
