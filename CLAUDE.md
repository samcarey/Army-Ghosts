# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

**Army Ghosts** — a web-based, mobile-first, top-down 2D shooter (old-PC *Army
Men* feel) that will grow Ghost Recon Wildlands *Ghost War* mechanics (stealth,
hiding in bushes, squad modes). Built on **Bevy 0.18** with fully deterministic
**peer-to-peer rollback multiplayer**: GGRS over matchbox WebRTC. The only
server anywhere is a matchbox *signaling* server (runs on the dev Mac); game
traffic is p2p and the sim is replayed identically on every peer.

Reference project: `~/bad-spaceship` (same author's 3D server-authoritative
game). Its CLAUDE.md documents most of the toolchain/deploy patterns this repo
reuses (Pages CI, Tailscale serving, wasm pitfalls). This project deliberately
differs: 2D, integer determinism, p2p rollback instead of lightyear
client-server.

## Version pins (read before touching Cargo.toml)

All engine-coupled versions live in `[workspace.dependencies]` in the root
`Cargo.toml`. **Bevy is deliberately 0.18, not 0.19**: the released netcode set
is bevy 0.18 + bevy_matchbox 0.14 + matchbox_socket 0.14 (`ggrs` feature) +
bevy_ggrs 0.20 + ggrs 0.11.1. bevy_ggrs 0.21+/ggrs 0.12+ break matchbox's
NonBlockingSocket impl; bevy_matchbox has no 0.19 release yet (checked
2026-07-24). Bump everything together when matchbox ships 0.19 support.

Other pins:
- Rust **1.97.1** via `rust-toolchain.toml`; `Cargo.lock` committed — build
  with `--locked` (build scripts do).
- `wasm-bindgen` crate pinned `=0.2.126` (client/Cargo.toml) and CLI version
  in `tools/build-web.sh` — the CLI rejects a version-mismatched module; bump
  both together. The CLI is auto-fetched into `target/tools/`, never installed
  globally (other projects on this machine pin different versions).

## Build & run

```bash
# Native dev loop (synctest/local mode):
cargo run -p army-ghosts-client --features native

# Native p2p test (per instance):
AG_ROOM=test AG_PLAYERS=2 AG_SIGNALING=ws://127.0.0.1:3536 cargo run -p army-ghosts-client --features native

# Web build → _site/ (fetches the pinned wasm-bindgen CLI on first run):
tools/build-web.sh            # release; --debug for a debug build
python3 -m http.server -d _site 8080
```

`client` needs exactly one of `native` / `web` on top of `default`
(the web build is `--no-default-features --features default,web`).

URL params (parsed by index.html into `window.__AG_NET__` pre-WASM):
`?room=CODE&players=N&signaling=wss://…`. No `room` ⇒ local synctest mode.
Native equivalents: `AG_ROOM`, `AG_PLAYERS`, `AG_SIGNALING` env vars.

## Architecture

- **`sim/`** (`army-ghosts-sim`) — the deterministic core. Integer-only: i32
  fixed-point positions (`FP = 256` subunits per world-unit/pixel), integer
  velocity/collision/isqrt math. All tick-state components are registered for
  GGRS rollback (snapshot/restore) and `Pos` is checksummed for desync
  detection. Generic over the ggrs `Config` so it never depends on matchbox.
  Runs in `GgrsSchedule` at `TICK_HZ` (60).
- **`client/`** — rendering (sprites; the ONLY place fixed-point → f32, via
  `Pos::to_f32`), input collection (`ReadInputs` schedule → `LocalInputs`),
  session bring-up (`net.rs`: launch-config parsing, matchbox socket,
  GGRS session build), camera follow.
- **`client/index.html`** — instant-paint loader (progress bar, streams the
  wasm, parses URL params before init). Mobile hardening: devicePixelRatio cap
  (1.5) so iOS Safari's GPU governor doesn't throttle, touch-action none,
  iOS tab-suspension reload recovery.

### Determinism rules (the whole point — do not bend)

1. **No floats in `sim/`.** Positions, velocities, collision tests, isqrt —
   all integer. Rendering may use floats freely but nothing flows back.
2. No randomness, no time reads, no HashMap-iteration-order dependence, no
   query-iteration-order-sensitive logic in the sim (when order matters, sort
   by a stable key first).
3. Every component that evolves over ticks must be rollback-registered in
   `SimPlugin`, and anything worth checksumming should be.
4. Desync detection stays ON (`DesyncDetection::On` + `Pos` checksums). A
   desync event is always a bug; local synctest mode (`run without a room`,
   GGRS re-simulates every frame) is the canary — keep it working.

### Netcode model

- **bevy_ggrs rollback**: peers exchange only `PlayerInput` (3 bytes:
  quantized i8 move x/y + button bitflags), each peer simulates everything.
- **Lobby (open rooms)**: matchbox room string from `?room=`; socket URL is
  plain `{signaling}/{room}` (no `?next=` — everyone in the room meshes as
  they join). Two channels in builder order: 0 reliable (lobby control),
  1 unreliable (GGRS). Host = lowest sorted PeerId (identical on all peers).
  Host taps START (or the room hits the `?players=` cap, default
  `MAX_PLAYERS = 8`) → broadcasts `start:<uuid>,...` (sorted roster) on
  channel 0; every peer builds the session from that roster (Local for self;
  sorted order = handle order), waiting until its own mesh contains all
  members. Late joiners after start idle in warmup. Offline (no room)
  defaults to 1 player; explicit `?players=N` offline forces an N-handle
  synctest. UI on top: upper-left MENU → NEW ROOM (generated 5-char code,
  web navigates to `?room=`), COPY LINK beside the lobby roster (clipboard;
  both are bevy_ui `Button`+`Interaction`, which handles touch natively).
- **Aim down sights** (`client/src/ads.rs`): bottom-center crosshair toggle
  (also Shift on a keyboard). The toggle is local UI state; it reaches the sim
  only as the `BTN_ADS` input bit, which roots the pawn in place (the stick
  still turns it) so every peer applies the lock from the same input stream.
  Everything else is render-only: the camera slides `ADS_SHIFT` (200 world
  units — half a "normal" mobile screen, deliberately fixed rather than
  window-relative) along the facing, smoothstepped over 500 ms, and a thin
  white line traces the shot to the first target it would hit, else the arena
  wall. The shift rides on `render::CameraFocus` (the follow target) so the
  camera's own lerp doesn't fight the aim ease.
- **Character art** (`tools/gen_assets.py` `gen_soldier` + `client/src/render.rs`):
  the soldier is modelled ONCE in 3D — capsules in character space, x right,
  y forward, z up, origin on the ground between the feet — then rotated about z
  per facing and projected `SOLDIER_TILT` (40 deg) off straight-down. That's the
  3/4 view these games use: head up, feet down, upright on screen always. So
  the sprite must NEVER be rotated (there is no `orient_players` any more);
  `soldier.png` is a GRID, 13 animation columns x 16 facing rows, and
  `animate_players` picks the row from `Facing` (bearing clockwise from
  away-from-camera) and the column from gait. Orthographic projection keeps a
  sphere a circle, so a 3D capsule projects to a 2D capsule and the rasteriser
  stays cheap; parts paint far-to-near by depth along the view axis. The look
  is deliberately low contrast: shades in a narrow band, NO dark outlines
  between parts (roundness comes from a faked capsule normal), camo in
  quantised bands in part-local coords so it travels with the limb, and a noise
  jitter on the silhouette so nothing reads as a clean analytic curve.
  Two knock-ons that are easy to miss:
  * The sprite is anchored at the figure's ground point (`SOLDIER_GROUND`), so
    feet stand on `Pos` and the body rises above it.
  * Shots therefore have to be lifted `MUZZLE_LIFT` (22 px) or tracers and the
    ADS aim line appear to leave the soldier's boots. Bullets, trails and the
    aim line all apply it.
  * `PLAYER_COLORS` are muted but kept well ABOVE the ground tile in value.
    Muted is not invisible — tinting a camouflaged soldier down into the grass
    range makes them genuinely impossible to see on it.
- **Cover** (`sim/src/lib.rs`: `rock_layout` / `bush_layout`): two procedural
  fields, both pure integer rejection sampling from fixed seeds (`ROCK_SEED`,
  `BUSH_SEED`) — no floats, no RNG crate, so every peer builds the identical
  arena before the first tick, and `Pos` checksums catch it instantly if one
  doesn't. **Rocks** are solid: `push_out_of_cover` shoves the player back out
  along the surface normal, cancelling only the into-the-rock part of the step
  (so an angled approach deflects around instead of stopping dead), and bullets
  despawn on contact. **Bushes** stop nothing — they're concealment only, and
  come in overlapping clusters. Layout constants keep every gap walkable
  (`ROCK_GAP`/`ROCK_WALL_GAP` > the 24-unit player diameter) and keep the
  spawn→practice-dummy lane clear so `TARGET_POINTS` stays "dead ahead".
  `cargo test -p army-ghosts-sim` asserts both.
- **Line of sight** (`client/src/vision.rs` + `client/assets/fog.wgsl`):
  render-only — the sim never computes visibility (it can't; every peer
  simulates every pawn). Each piece of cover casts a soft shadow away from the
  local player into ONE `Mesh2d` rebuilt each frame.
- **The camera model.** Sight lines start `VIEW_PULLBACK` (50) *behind* the
  pawn, at TWO points `SHOULDER_OFFSET` (30) either side — a third-person
  camera looking over either shoulder, so you can peek around cover you're
  hugging. The pullback is per-caster, always along that caster's own bearing,
  so it behaves the same whichever way you face, and nothing between the
  cameras and the pawn can occlude (each caster is swept independently; there
  is no occlusion chain). Ground is dark only where BOTH shoulders are blocked.
  Sight lines are parameterised by a shared lateral fraction `t`: line `t`
  leaves the camera pair at `t * offset` and crosses the cover at `t * r`, so
  t = ±1 are exactly the two umbra boundaries. Umbra half width is therefore
  `offset + (r - offset) * x / dist`, which WIDENS when cover is broader than
  the camera pair and CONVERGES TO A POINT when it isn't — with offset 30 most
  cover is narrower, so most shadows are finite cones (a 16-radius rock at
  range 60 closes ~126 units behind it). The "am I inside it?" test is on the
  pawn, never the cameras, so standing in a bush hides you without blinding
  you.
- **Two effects, one number.** `Cast::coverage()` evaluates the same shadow the
  mesh and shader produce, so what hides an enemy always matches the ground.
  Full strength drives *player opacity* (`fade_hidden` — sampled at 5 points
  across the body so someone edging out of cover fades in rather than pops, and
  the local pawn never fades); terrain only gets `TERRAIN_SHADOW_SCALE` (0.5)
  of it. Cover is therefore total against players while the ground behind it
  merely dims, so you keep a sense of terrain you can't see into. NOTE bullets
  and tracers are NOT faded — a hidden enemy's shots still show.
- **Why it's shaped the way it is** (each of these was a visible bug once):
  each shadow starts INSIDE its caster and ramps from nothing where a sight
  line enters the circle to full where it leaves, so cover is lit on the
  player's side and rolls into darkness over its back like a sphere lit from
  one side — which is why cover draws UNDER the fog (rocks z 0.5, bushes 2.5,
  fog 5.0). `RIM_FEATHER` gives grazing lines a minimum ramp, else every rock
  gets a hard rim. The sideways falloff runs INWARD from the umbra boundary
  (zero exactly on it), never outward, or the shadow bleeds onto lit ground; it
  lives in `fog.wgsl` and rides in UV, evaluated per pixel, so the edge can be
  tighter than the ray spacing without a denser mesh. It is a FRACTION of the
  local half width (floored by `EDGE_MIN_FRACTION`) — an absolute width fights
  a converging cone and nothing ever reaches full strength.
  `NoFrustumCulling` is required: the Aabb is computed once when `Mesh2d` is
  added, and the stale box blinks the fog out as the camera moves.

## Gotchas already hit (don't rediscover)

- **`bevy/bevy_sprite_render` feature is required** on top of `bevy_sprite`.
  Bevy 0.17's render-crate split moved sprite rendering out; without it
  sprites exist, pass visibility (ViewVisibility=true), and silently never
  draw — empty clear-colored screen, no error.
- **ggrs 0.11 inputs are serde types**, not bytemuck Pod (that was older
  ggrs): derive `Serialize`/`Deserialize` on the input struct.
- **bevy_ggrs 0.20 API**: the app-extension trait is `RollbackApp`
  (`rollback_component_with_copy`, `checksum_component_with_hash`); tick rate
  is the `RollbackFrameRate` resource; desync event is
  `GgrsEvent::DesyncDetected`.
- **bevy_matchbox 0.14**: `MatchboxSocket::new_unreliable(url)` (the old
  `new_ggrs` constructor is gone); it's both a Resource and a Component;
  matchbox's `ggrs` feature impls NonBlockingSocket on `WebRtcChannel`.
- **getrandom on wasm**: `.cargo/config.toml` sets
  `getrandom_backend="wasm_js"`; the client `web` feature enables the
  matching `wasm_js`/`js` features for the getrandom 0.3 and 0.2 lines in the
  graph.
- **Headless browser testing**: playwright chromium lives in
  `~/Library/Caches/ms-playwright/`; launch with
  `--use-gl=swiftshader --enable-unsafe-swiftshader --no-sandbox`. Chrome's
  `--virtual-time-budget` screenshots are unreliable for wasm games — drive
  with playwright and real waits instead. Keep playwright's
  `deviceScaleFactor` at 1: any other value panics winit-web in headless
  ("created media query doesn't match, 1.5 != 1.5") and the page dies before
  `data-game-ready`. Zoom by cropping + `sips -z` afterwards instead.
- **Headless-to-headless WebRTC additionally needs
  `--disable-features=WebRtcHideLocalIpsWithMdns`** — headless Chrome can't
  resolve the mDNS-obfuscated host candidates, and the srflx fallback needs
  NAT hairpinning, so data channels never open without it. Sessions in the
  headless swiftshader environment take 60-90s to reach "starting p2p
  session" (huge debug wasm + software rendering) — poll patiently.
- **The vendored matchbox trickle-ICE patch** (`vendor/matchbox_socket/`, see
  `[patch.crates-io]` in the root Cargo.toml): upstream matchbox waits for
  ICE gathering to COMPLETE before sending the offer/answer. On
  multi-interface machines (Tailscale utuns!) browsers hit the full ~40s
  STUN transaction timeout per handshake leg → 80s+ connects that read as
  "p2p is broken". The patch sends offer/answer immediately and trickles
  candidates (both matchbox signal loops already buffer early candidates).
  Native (webrtc-rs) never had the wait — that's why native p2p worked while
  web stalled. Diagnose future regressions by diffing the two.
- **STUN itself is fine on this network** (verified: 28-47ms responses via
  python/node UDP probes) — do not blame the router when browser ICE is slow;
  it's the per-interface STUN timeout above.
- **Local same-Mac p2p tests are flaky because macOS firewall STEALTH MODE is
  on** (`socketfilterfw --getstealthmode`): unsolicited inbound UDP (ICE
  connectivity checks) is silently dropped, so browser-involved pairs
  sometimes stall at ICE `checking` → `failed` (~30s). Native↔native usually
  survives; anything involving the Playwright Chrome binary is a coin flip.
  Real phones talking to each other are unaffected. Fix for reliable local
  automation (needs sudo, ask the user): allowlist the browser in the app
  firewall or turn stealth mode off.
- **A long-running `matchbox_server` can wedge** after many abandoned test
  rooms — new pairings then stall even at the signaling stage. When p2p
  "mysteriously stops working": `pkill matchbox_server` and restart it, and
  always use FRESH room codes per test (rooms remember dead peers).
- **Warmup → p2p session swap** (`client/src/net.rs`): with a `room`, the
  client starts a 1-player synctest immediately (playable while waiting).
  When the match starts, `run_lobby` despawns all `Rollback` entities,
  removes the `Session` resource, and stashes the channel in
  `PendingSession`; `finalize_p2p_session` builds the real session **one
  frame later** (bevy_ggrs needs a no-session tick to reset frame/snapshot
  state) and must also insert a fresh `Time<GgrsTime>` — otherwise
  bevy_ggrs `advance_to`s an earlier moment and panics ("tried to move time
  backwards").
- **HUD**: bevy_ui needs `bevy_ui_render` (same render-split trap as
  sprites) + `bevy_text` + `default_font`. The embedded default font has no
  `…` glyph (renders as a box) — use ASCII `...`.
- **`ColorMaterial` silently ignores vertex colors on a mesh that gains its
  `ATTRIBUTE_COLOR` after spawn.** bevy gates them behind a `VERTEX_COLORS`
  shader def decided at *first* specialization, and
  `specialize_material2d_meshes` only re-specializes on `Changed<Mesh2d>` /
  `Changed<MeshMaterial2d>` — never because the mesh asset grew an attribute.
  A mesh spawned empty and filled in later therefore keeps a pipeline compiled
  without the def forever. Symptom is maddening: geometry is perfect (the
  vertex stride comes from the packed buffer either way), the mesh really does
  report `contains_attribute(ATTRIBUTE_COLOR) == true`, there are no shader
  errors, and every triangle just renders flat at the material color with
  vertex alpha discarded. Seeding the attributes at spawn and calling
  `set_changed()` on `Mesh2d` every frame both failed to fix it. The fix that
  works is a custom `Material2d` whose `specialize` forces the vertex layout
  (`layout.0.get_layout(&[POSITION.at_shader_location(0),
  COLOR.at_shader_location(4)])`) with a shader that declares `@location(4)`
  unconditionally — see `FogMaterial`. Also: `LinearRgba::rgb(v)` is NOT
  `Color::srgb(v)`; feeding sRGB numbers to a shader as linear gives a washed
  out, far paler color (`.to_linear()` on the way in).

## Public dev serving (caddy vhost on this Mac)

**`https://army-ghosts.dev.whoeverwants.com:7443`** — the public URL. The
router forwards ONLY public **:7443** to this Mac (verified externally via
check-host.net: 7443 open, 443/80 filtered) — every `:443` caddy site
(cmd-api, the `*.dev` wildcard block) is LAN/loopback-reachable only. The
:7443 site is a dedicated block appended to `/opt/homebrew/etc/Caddyfile`
(own Route 53 DNS-01 cert); a twin fragment for :443 lives at
`/Users/sccarey/devbox/caddy.d/army-ghosts.caddy` (useful for loopback
tests). Both: `/ws/*` → matchbox_server :3536 (prefix stripped), everything
else → `file_server` on `_site/` with zstd/gzip encode (the ~50MB dev wasm
→ ~11MB on the wire). index.html derives signaling as
`wss://<host:port>/ws` from `location.host`, so the port rides along
automatically. Apply changes with
`caddy reload --config /opt/homebrew/etc/Caddyfile` (admin API is open on
localhost:2019, no sudo needed). index.html auto-selects same-origin `/ws`
signaling on any non-localhost, non-ts.net host. NOTE: public URLs are NOT
testable from inside this LAN (no NAT hairpin — curl gives 000); test via
`--resolve <host>:443:127.0.0.1` locally or from a cellular device.
Cross-network play relies on plain STUN; symmetric/CGNAT cellular pairs may
need a TURN server eventually.

## Testing

- `cargo test -p army-ghosts-sim` — pure-integer layout/collision checks
  (rock + bush fields land and stay walkable, cover deflects an angled walk).
- Native smoke: run without a room (synctest re-simulates every frame — it
  catches nondeterminism AND rollback-unsafe state immediately).
- Web smoke: `tools/build-web.sh` + local http.server + headless chromium
  screenshot (see scratchpad pattern; keyboard works via playwright's
  `page.keyboard`).
- Two-peer local test: run `matchbox_server` (installed via
  `cargo install matchbox_server --version 0.14.0`, listens on `0.0.0.0:3536`),
  open two browser tabs with `?room=X` (or two native instances with
  `AG_ROOM=X`; native connects in ~1s and is the fast way to test session
  logic). Fastest full check: two native instances with `AG_PLAYERS=2`
  (cap 2 → the lobby auto-starts, exercising the whole start-roster
  handshake headlessly), staggered by a few seconds; assert "starting p2p
  session" + no DESYNC lines in both logs. The default cap (8) needs a
  START trigger: Enter on the host instance, or tap/click bottom-center.

## Deployment (planned; mirrors bad-spaceship)

- Dev: build on this Mac, serve `_site/` + `matchbox_server` over Tailscale
  (`tailscale serve` gives the MagicDNS TLS that makes `wss://` work from
  phones).
- Prod: GitHub Actions → Pages (source must be "GitHub Actions", not branch —
  see bad-spaceship CLAUDE.md "Deployment" for the Jekyll-conflict tell-tales).
  The signaling server needs public reachability for non-tailnet players
  (tailscale funnel, later).
