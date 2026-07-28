#!/usr/bin/env bash
# Measure one BotProfile against another by playing them against each other.
# Usage: tools/selfplay.sh [selfplay options]     (`--help` for the full list)
#
#   tools/selfplay.sh -c reaction=8
#   tools/selfplay.sh -c aggression=0.9 -b aggression=0.1 --pairs 140
#
# Eight bots in the real arena, four a side, for a minute of game time; every
# split of the spawn points played from both sides with the same dice, scored on
# kills minus deaths, stopped by a sequential test as soon as the answer is in.
# A match is ~50 ms, so a verdict is usually a few seconds.
#
# RELEASE, always — the debug build is roughly thirty times slower and the whole
# point of the thing is that a run is cheap enough to do before committing a
# tuning change. It is not part of the game build; nothing in client/ or sim/
# depends on this crate.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --locked --release -p army-ghosts-harness >/dev/null 2>&1 ||
  cargo build --locked --release -p army-ghosts-harness   # rerun loudly to show why

exec ./target/release/selfplay "$@"
