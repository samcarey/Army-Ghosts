# Army Ghosts

A web-based, mobile-first, top-down 2D shooter — old-PC *Army Men* vibes with
Ghost Recon Wildlands *Ghost War* mechanics on the roadmap (stealth, bushes,
squad modes).

- **Engine:** Bevy (Rust → WASM), 2D sprites
- **Multiplayer:** fully deterministic peer-to-peer rollback netcode
  (GGRS over matchbox WebRTC) — the only server is a tiny signaling service;
  the game sim is integer-only fixed-point math replayed identically on every
  peer
- **Controls:** left virtual joystick to move, right buttons to fire (touch);
  WASD + Space on desktop

## Play / develop

```bash
# Native dev build (local mode):
cargo run -p army-ghosts-client --features native

# Web build:
tools/build-web.sh
python3 -m http.server -d _site 8080   # → http://localhost:8080

# Multiplayer: run a signaling server (matchbox_server), then open
# http://localhost:8080/?room=CODE on two devices/tabs.
```

See `CLAUDE.md` for architecture, version pins, and the determinism rules.
