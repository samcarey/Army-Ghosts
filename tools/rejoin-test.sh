#!/usr/bin/env bash
# Two native peers, one of which walks out mid-match and comes back.
#
# This is the only test that exercises the whole rejoin path end to end: a real
# matchbox room, a real GGRS session, a process actually dying, and a second
# process claiming its identity. The sim tests (`sim/tests/persist.rs`) prove
# that a restored world is a legal starting point for a lockstep session; this
# proves that two peers actually GET there — that the returning peer is
# recognised, that one peer and only one answers, that everybody moves to the
# next session generation together, and that nothing desyncs afterwards.
#
# What it asserts, in order:
#   1. both peers reach generation 0 (the ordinary lobby start still works)
#   2. the survivor notices the other one has come back
#   3. both peers build generation 1, restoring a world rather than a fresh one
#   4. the returning peer resumes into the SAME round it left
#   5. no DESYNC on either side, before or after
#
# It leans on two native-only hooks in `client/src/persist.rs`:
#   AG_PLAYER_ID  — pin the identity, so a fresh process is demonstrably the
#                   same PLAYER as the one that died. This is what a browser
#                   gets for free out of localStorage.
#   AG_STATE_DIR  — put the stored match somewhere disposable.
#
# Native peers open a window each; that is expected and is how the existing
# two-peer smoke test works too. Nothing is read off the screen — every
# assertion is a log line.
#
# ⚠ UNVERIFIED, and here is exactly why. A native build launched from a shell
# with no window-server session never runs a frame at all: winit gets no event
# loop, so bevy's Startup never fires and `begin_session_setup` never logs.
# Confirmed to be nothing to do with this feature — a binary built from `main`
# behaves identically, three log lines and then silence. So this script has
# never been executed; it needs a terminal that can open windows. The browser
# equivalents (`tools/persist-web.sh`) HAVE been run, and they cover the same
# ground on the platform the game actually ships to.
#
# Usage: tools/rejoin-test.sh [outdir]
set -uo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-target/rejoin-test}"
rm -rf "$OUT" && mkdir -p "$OUT"
# Rooms remember dead peers, so every run needs a code nobody has used. See
# CLAUDE.md: a matchbox_server that has accumulated abandoned rooms starts
# refusing to pair at all.
ROOM="rj$(date +%s)"
SIGNALING="${AG_SIGNALING:-ws://127.0.0.1:3536}"
PLAYER_A="aaaa0000$(date +%s | tail -c 9)"
PLAYER_B="bbbb0000$(date +%s | tail -c 9)"

say() { printf '\n=== %s\n' "$*"; }
fail() { printf '\nFAILED: %s\n' "$*" >&2; cleanup; exit 1; }

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null; done
    wait 2>/dev/null
}
trap cleanup EXIT

# Wait for a line to appear in a log, or give up. Peers take a second or two to
# mesh natively (the browser is the slow one); 60s is generous enough to
# survive a cold page cache and tight enough to fail rather than hang.
await() {
    local log="$1" pattern="$2" limit="${3:-60}" waited=0
    until grep -q "$pattern" "$log" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge "$limit" ]; then
            printf -- '--- %s (last 25 lines) ---\n' "$log" >&2
            tail -25 "$log" >&2
            return 1
        fi
    done
}

launch() {
    local who="$1" player="$2" log="$3"
    AG_ROOM="$ROOM" \
    AG_PLAYERS=2 \
    AG_BOTS=2 \
    AG_SIGNALING="$SIGNALING" \
    AG_PLAYER_ID="$player" \
    AG_STATE_DIR="$PWD/$OUT/state-$who" \
    AG_ICE=none \
        target/debug/army-ghosts-client > "$log" 2>&1 &
    PIDS+=($!)
    printf 'peer %s (player %s) is pid %s\n' "$who" "$player" "$!"
}

say "building"
cargo build -p army-ghosts-client --features native --locked \
    > "$OUT/build.log" 2>&1 || { tail -30 "$OUT/build.log"; fail "build"; }

if ! pgrep -f matchbox_server > /dev/null; then
    say "starting matchbox_server"
    matchbox_server > "$OUT/signaling.log" 2>&1 &
    PIDS+=($!)
    sleep 2
fi

say "room $ROOM: two peers in"
launch a "$PLAYER_A" "$OUT/a.log"
sleep 2   # staggered, as the two-peer smoke test does — simultaneous joins race
launch b "$PLAYER_B" "$OUT/b.log"

await "$OUT/a.log" "starting generation 0" || fail "peer a never started the match"
await "$OUT/b.log" "starting generation 0" || fail "peer b never started the match"
say "both peers are in generation 0"

# Let the match actually run: a round needs to be under way, and the bots need
# to have moved, or "resumed where we were" is indistinguishable from "started
# fresh".
sleep 6

say "peer b walks out"
B_PID="${PIDS[-1]}"
kill -9 "$B_PID" 2>/dev/null
# Long enough for GGRS to notice and disconnect the handle, and for peer a to
# carry on simulating without it — that is the "still show my character, still
# vulnerable" half of the feature, and if the survivor freezes here the match is
# over anyway.
sleep 5
grep -q "ggrs event" "$OUT/a.log" || printf 'note: peer a logged no ggrs event for the drop\n'
A_FRAMES_BEFORE=$(grep -c "" "$OUT/a.log")

say "peer b comes back as the same player"
launch b "$PLAYER_B" "$OUT/b2.log"

await "$OUT/a.log" "is back — resyncing" 45 || fail "peer a never recognised the returning player"
await "$OUT/a.log" "starting generation 1" 30 || fail "peer a never moved to generation 1"
await "$OUT/b2.log" "starting generation 1" 30 || fail "peer b never moved to generation 1"
await "$OUT/b2.log" "resuming generation 1" 30 || fail "peer b built a fresh world instead of resuming"

say "both peers are in generation 1"
grep -h "resuming generation 1" "$OUT/a.log" "$OUT/b2.log"

# The round they resumed into. Both peers restore the identical blob, so both
# lines must name the same round — a peer that restarted the series would say
# "round 1" here while the other said "round 3".
ROUND_A=$(grep -o "resuming generation 1 at round [0-9]*" "$OUT/a.log" | tail -1 | awk '{print $NF}')
ROUND_B=$(grep -o "resuming generation 1 at round [0-9]*" "$OUT/b2.log" | tail -1 | awk '{print $NF}')
[ -n "$ROUND_A" ] && [ "$ROUND_A" = "$ROUND_B" ] \
    || fail "peers resumed into different rounds: a=$ROUND_A b=$ROUND_B"
say "both resumed into round $ROUND_A"

# And it has to keep running. A resumed session that immediately stalls or
# desyncs would have passed everything above.
sleep 8
for log in "$OUT/a.log" "$OUT/b2.log"; do
    if grep -q "DESYNC" "$log"; then
        grep "DESYNC" "$log" >&2
        fail "desync in $log"
    fi
done
[ "$(grep -c "" "$OUT/a.log")" -gt "$A_FRAMES_BEFORE" ] || fail "peer a stopped logging after the resume"

say "PASSED — rejoined into round $ROUND_A with no desync (logs in $OUT)"
cleanup
trap - EXIT
exit 0
