#!/usr/bin/env bash
# Persistence, in a real browser — which is the only place the feature actually
# lives, since "I refresh the window" is not a thing a native build does.
#
# Three checks, in increasing order of how much can go wrong:
#
#   refresh  Offline. Walk the player off its post, get it prone, reload the
#            page, and prove the world that comes back is the one that was
#            stored: same round, same tick, same position, same stance. This is
#            the storage path end to end and it is the reliable one.
#   rejoin   Two tabs in a room; one refreshes mid-match. Proves the returning
#            player is recognised by its stored id rather than its (new) peer
#            id, that exactly one peer answers, that everybody moves to the next
#            session generation together, and that both restore the SAME round
#            from the same blob.
#   control  The same two tabs with NOBODY refreshing. This is not a test of
#            this feature — it is the baseline the rejoin has to be read
#            against. Run it FIRST whenever the rejoin check reports a desync:
#            it says whether the baseline is broken or the rejoin is. That is
#            not hypothetical — it is how the round-clock bug was found, where
#            every p2p match started desynced because the warmup's clock ran
#            into the match and no two peers warm up for the same length of
#            time. The rejoin check looked broken; the baseline was.
#
# Notes for whoever edits this:
#   * it photographs the BUILT wasm, so `tools/build-web.sh` first;
#   * playwright is not a repo dependency — pass AG_NODE_PATH=/path/node_modules;
#   * headless-to-headless WebRTC needs --disable-features=WebRtcHideLocalIpsWithMdns
#     (in the .js files) and a matchbox_server; see CLAUDE.md;
#   * ?ice=none is NOT usable here. Chrome rejects an ICE server list with an
#     empty uri outright ("Failed to construct 'RTCPeerConnection'"), so that
#     flag only ever worked on the native path.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${AG_PORT:-8151}"
NODE_PATH="${AG_NODE_PATH:-$HOME/obs/node_modules}"
WHICH="${1:-all}"

[ -f _site/index.html ] || { echo "no _site — run tools/build-web.sh first" >&2; exit 1; }
command -v matchbox_server > /dev/null || { echo "matchbox_server not installed" >&2; exit 1; }

pgrep -f matchbox_server > /dev/null || { matchbox_server > /dev/null 2>&1 & sleep 2; }
if ! curl -fsS "http://127.0.0.1:$PORT/" 2>/dev/null | grep -q "Army Ghosts"; then
    python3 -m http.server -d _site "$PORT" > /dev/null 2>&1 &
    SERVER=$!
    trap 'kill $SERVER 2>/dev/null' EXIT
    sleep 2
fi

run() {
    printf '\n═══ %s ═══\n' "$1"
    NODE_PATH="$NODE_PATH" PORT="$PORT" node "tools/persist-$1.js"
}

status=0
case "$WHICH" in
    all)     run refresh || status=1; run rejoin || status=1; run control || true ;;
    refresh) run refresh || status=1 ;;
    rejoin)  run rejoin  || status=1 ;;
    control) run control || true ;;
    *) echo "usage: $0 [all|refresh|rejoin|control]" >&2; exit 2 ;;
esac
exit $status
