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
  `touch.rs` skips any touch that starts on a visible bevy_ui `Button`
  (`ComputedNode::contains_point`, in PHYSICAL px — multiply by
  `window.scale_factor()`), because the joystick and fire zones are generous
  enough to swallow the sights/stance buttons otherwise; ask the UI where its
  buttons are rather than keeping a second copy of the layout. Same reason
  `read_start_input` ignores a tap while any button reads `Pressed`.
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
- **Stance** (`sim/src/lib.rs` `Stance` + `client/src/stance.rs`): standing /
  crouching / prone, driven by the two chevron buttons on the right edge (or
  C to go down, V to get up). What crosses the wire is the *level* the player
  is asking for, in bits 2-3 of the input byte, re-sent every tick — NOT a "go
  down one" edge. Rollback replays a tick as often as it likes, so an edge
  would apply as many times as the frame is re-simulated; an absolute level is
  idempotent. The sim owns the rest (`Stance::advance`): one level per request,
  `STANCE_DOWN_TICKS`/`STANCE_UP_TICKS` of being rooted for each (getting up is
  slower), and `STANCE_SPEED` scaling movement 100/56/31%. Crouched and prone
  both top out below `RUN_ABOVE`, so those stances never reach the sheet's run
  columns — which is why those columns are filled with the walk frames anyway:
  a rollback correction can read as a supersonic `Pos` delta for one frame and
  must not land on an empty frame. `Stance` is rollback-registered like every
  other tick-evolving component.
- **Health, damage and respawn** (`sim/src/lib.rs`: `Health`, `Deaths`,
  `Sweep`, `bullet_damage`, `resolve_hits`, `respawn_players`): a round takes
  off `HIT_DAMAGE_MAX` (42 of `MAX_HEALTH` 100) scaled twice — by how centered
  it was and by how far it flew — so three perfect rounds kill and a long graze
  is worth about a tenth of that. Both scalings have floors (`DAMAGE_EDGE_FRAC`
  30%, `DAMAGE_FAR_FRAC` 45% beyond `DAMAGE_FAR`), and damage is floored at 1: a
  hit is never worth nothing. Range is free of extra state — every round flies at
  `BULLET_SPEED`, so the ticks it has burned *are* its range.
  The part worth understanding is the **sweep**. A bullet covers 16 units a tick
  against a 24-unit pawn, so the old point-in-circle test on the post-move
  position was both tunnelling and unable to answer "how centered": a dead-center
  shot whose sampled position happened to land near the rim read as a graze.
  Rounds are now tested against the whole tick's travel as a segment, and the
  centeredness is the perpendicular distance from the pawn to the shot *line*,
  not to the contact point — a round entering someone's edge at the very end of a
  tick is still a dead-center shot, it just hasn't arrived yet.
  Consequences that are easy to miss:
  * Players, dummies and boulders now resolve in ONE nearest-impact pass in
    `resolve_hits` (the rock test moved out of `move_bullets`), because cover has
    to stop a round that would otherwise carry on into someone behind it. Ties
    are broken by (distance along the sweep, position, handle) — query iteration
    order is not a determinism guarantee, and two pawns can share a subunit.
  * Death is a flag (`Health::down`), not a despawn: rollback un-killing someone
    then only restores a component instead of resurrecting an entity the
    renderer has forgotten. While down you can't move, fire, change stance or be
    hit, and the client hides you via `Visibility` (NOT alpha — `fade_hidden`
    owns pawn alpha and would overwrite it a system later; `update_health_visuals`
    writes rgb only, and must run before it).
  * `Deaths` is sim state, so every peer's scoreboard agrees without anyone
    sending a score message.
  * **The spawn→spawn lanes are NOT clear.** Only spawn→dummy is (see
    `rock_layout`); there's a boulder at (30,-23) squarely between spawns 0 and 1.
    Any test that needs a clear shot must pick its lane deliberately —
    `sim/tests/combat.rs` fires spawn 2 → spawn 3 and asserts `lane_is_clear`
    up front, so a reseeded field fails saying what actually changed.
- **Character art** (`tools/gen_assets.py` `gen_soldier` + `client/src/render.rs`):
  the soldier is modelled ONCE in 3D — capsules in character space, x right,
  y forward, z up, origin on the ground between the feet — then rotated about z
  per facing and projected `SOLDIER_TILT` (40 deg) off straight-down. That's the
  3/4 view these games use: head up, feet down, upright on screen always. So
  the sprite must NEVER be rotated (there is no `orient_players` any more);
  `soldier.png` is a GRID, 16 facing rows x 39 columns (three 13-column stance
  blocks: standing, crouching, prone), and `animate_players` picks the row from
  `Facing` (bearing clockwise from away-from-camera), the block from `Stance`
  and the column from gait. Orthographic projection keeps a
  sphere a circle, so a 3D capsule projects to a 2D capsule and the rasteriser
  stays cheap; parts paint far-to-near by depth along the view axis. The look
  is deliberately low contrast: shades in a narrow band, NO dark outlines
  between parts (roundness comes from a faked capsule normal), camo in
  quantised bands in part-local coords so it travels with the limb, and a noise
  jitter on the silhouette so nothing reads as a clean analytic curve.
  Two knock-ons that are easy to miss:
  * The sprite is anchored at the figure's ground point (`STANCE_ANCHOR`), so
    feet stand on `Pos` and the body rises above it — except prone, which is
    anchored mid-body, because that is what a horizontal figure pivots around.
    `animate_players` re-anchors on every stance change.
  * Shots therefore have to be lifted (`muzzle_lift`: 22 px standing, 3 px
    prone) or tracers and the ADS aim line appear to leave the soldier's boots.
    Bullets carry the lift they were FIRED at in a render-only `MuzzleLift`
    (the shooter may stand up mid-flight); trails and the aim line apply it too.
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
  Art-wise the two are opposites. Boulders are grayscale top-down blobs, tinted
  and *spun* by their seed. Bushes (`gen_bushes`) are modelled in 3D exactly
  like the soldier — a small L-system of tapering branch capsules, each
  generation shorter, thinner, paler and more transparent, with leaf capsules
  on the last two — and projected at the same `SOLDIER_TILT`, so cover is seen
  from the figures' angle instead of straight down. Consequences:
  * The frames carry their own greens (the only COLOUR sheet), so the per-bush
    tint is near-white; and a 3/4-view bush must NEVER be rotated (it tips
    over), so variety is 6 variants + `flip_x` instead of a seed angle.
  * Orthographic projection keeps a sphere a circle, so the canopy still fills
    a circle around the frame centre and lines up with the sim's concealment
    circle — but it only fills `BUSH_FILL_PX` (30) of the 96px frame, not the
    boulders' 40, and `cover_size` scales each sheet by its own fill.
  * Leaves stop at `BUSH_LEAF_Z`: bare stems under the canopy are what makes
    the viewing angle read at all. Without them it's a green ball again.
- **Grass** (`sim/src/lib.rs` `grass_height`/`grass_cover` + `client/src/grass.rs`
  + `client/assets/grass.wgsl`): unlike rocks and bushes there are NO grass
  entities. The depth anywhere is a pure integer function of position — three
  octaves of value noise (lattice hash, smoothstep interpolation, cells
  300/105/38) — so nothing is spawned, stored, rolled back or checksummed, and
  the renderer, a test or a future sim rule can all ask "how deep here?" for a
  few multiplies. It's integer for the usual reason: float noise on two machines
  is exactly the sort of thing that wouldn't match.
  **The depth is QUANTIZED TO HEX TILES: one depth per tile, constant across it,
  different from tile to tile.** The noise is sampled once at the tile's centre
  (`hex_cell` → `hex_centre`) and that answer covers the whole tile, so the map
  is a honeycomb of even swards — a tile is a thing you can look at and judge the
  cost of crossing. `HEX_R` therefore lives in the SIM and `vision.rs` reads it:
  the fog paints these same tiles, and the tile you can see has to be the tile
  you're hiding in. Variety comes at three scales, all wanted: the coarse octave
  drifts whole regions (area to area), the 105/38-unit octaves differ across the
  24-unit tile pitch (tile to tile), and `GRASS_TILE_JITTER` (±6) breaks up
  neighbours where the noise happens to be level.
  **THE MIX IS THE DESIGN**, and `grass_field_has_a_mix_of_depths` prints it as a
  histogram rather than just asserting it: ~780 tiles, 49 distinct depths, mean
  33, of which **9% are BARE** (depth 0 — the ground texture, no tufts at all),
  2% short enough to leave a prone pawn showing, and **6% deep enough to bury a
  crouching one**, with the bulk in between. Every stance needs ground that suits
  it and ground that doesn't, or the terrain isn't saying anything.
  Three knobs, and they are deliberately independent:
  * **`GRASS_BARE_SPREAD`** — how much of the noise's bottom end comes out bare.
    A separate decision from the depth mapping ON PURPOSE: folding bare ground
    into the bottom of one 0..`GRASS_MAX_H` range drags the whole range down with
    it, which took the crouch-burying tiles from 8% of the map to 3%. Bare ground
    and how deep grass gets are different questions. It's applied BEFORE the
    per-tile jitter, or single bare tiles speckle through grassy ground instead
    of forming patches.
  * **`GRASS_BARE_BELOW`** (14) — the shallowest a GRASSY tile can be. Nothing
    lands between 0 and it, so a tile either has grass or it doesn't; that gap is
    what makes bare ground read as a place rather than as the field thinning out.
    It sits just under a prone pawn's 15, which is why bare ground does nearly
    all the work of "lying down doesn't help here" and short grass barely any —
    the test asserts the SUM of the two for that reason.
  * **the two curves** — a contrast stretch opens the middle half of the noise
    out to the whole band (without it every tile lands on the mean), then a
    square bias is applied **to the shallow half only**. Biasing the whole range,
    which is what the original 0..72 field did, costs the deep tiles again.
  How much of you it hides is *emergent* — it's whichever clumps happen to stand
  between you and the camera (see y-sorting below) — but the sim still states the
  rule: `grass_cover` = depth / `STANCE_HEIGHT` (64/52/15 units), i.e. grass
  hides whatever is shorter than it. A prone pawn is fully buried on ~88% of the
  map, a crouching one on 6%, a standing one nowhere.
  The shade sprite follows that number.
  The renderer asks `Scenario::depth` (see Testing) rather than `grass_height`
  directly, so the measuring scenario can swap the whole field for a wall of
  grass of a known depth without a second code path through the three layers.
  Rendering is three layers, all off that one number:
  * **The field** — one static `Mesh2d` over the arena (`GrassMaterial`, another
    vertex-color material for the `ColorMaterial` reason below), textured with
    `grass.png` tiled in WORLD uv and tinted per vertex. Its alpha saturates
    FAST (`0.12 + 3f`): grass covers soil long before it gets tall, so anything
    from ankle deep up is solid sward and only genuinely bare ground (bare tiles,
    and the rig's clear lanes) shows the ground texture underneath.
    **That texture is DRY EARTH, not green** (`gen_ground`): it is only ever
    visible where a tile is bare, so it is the thing that makes open ground read
    as open. It used to be a muted army green, and under the 12% sward tint that
    came out looking like mown grass — standing on a bare tile then read as
    standing in short grass with your boots poking out below the blades beside
    you, which is exactly how it got reported. The tint spread is WIDE (pale dry green to dark lush
    green) and that is what actually makes one tile read as deeper than the next
    — from almost straight down a third more blade height barely registers, since
    the silhouettes overlap into the same mass either way, so depth has to carry
    in value and hue as well. The shader crosses two octaves of the same texture
    at different scales, or the 128px tile reads as a grid.
  * **Tufts** — ~28,000 small clumps on a fine jittered grid (`TUFT_STEP` 4),
    each drawn as tall as the grass is deep where it stands. Baked into static
    meshes (quads with atlas UVs) rather than sprites: nothing about a tuft ever
    changes, and on a phone the scarce resource is per-frame sprite extraction,
    not triangles. Same material as the field with the octave crossing turned
    off — atlas UVs would sample the neighbouring frame.
    **`tuft_density` is nearly FLAT on purpose (0.80..1.00): depth sets how tall
    the grass is, not whether there is any.** Two earlier versions ramped
    acceptance with depth and both drew the same complaint — the shallow end
    reads as scattered clumps with ground between them, because thinning the
    count and narrowing the sprites (a clump is `TUFT_ASPECT` as wide as it is
    tall) compound. For the same reason the grid must stay finer than the
    narrowest tuft: at the field's floor a clump is ~12px wide, so a 6-unit grid
    left gaps however many were accepted. Short grass is a complete carpet, just
    a short one. Per-clump height jitter is only ±10% for a related reason: it
    competes directly with the tile-to-tile depth difference, and at the ±22% it
    used to be, neighbouring tiles read as one noisy sward instead of two swards
    of different depth.
    **Each clump wears a skirt of short hard-bent leaves at its root**
    (`gen_tufts`), and that is not decoration. Without it a frame is ~26% opaque
    at the ground line against ~33% higher up — bare stems — and a pawn's boots
    sit exactly on that line, so you see them through every clump in front of
    them whatever the sort order does. The skirt takes the root band to ~50%.
    This was the third and actual cause of a "foot under the grass" report that
    also had two real but insufficient causes (see `GRASS_BAND` below and
    `render::STANCE_ANCHOR`); if it comes back, measure the sheet's opacity by
    row before touching the ordering again.
  * **Shade** — the only thing parented to a pawn: a `shade.png` gradient over
    its lower body, reaching as far up as `grass_cover` says the grass buries
    it (`STANCE_SHADE`, measured off `soldier.png` bboxes — prone's ground line
    hangs 17px BELOW `Pos` because that sprite is anchored mid-body).
  **Y-SORTING is what makes the grass behave, and it is not optional.** Grass is
  baked one mesh per `GRASS_BAND` (4-unit) slice of the arena, each drawn at
  the z of its MIDDLE line, and everything standing on the ground — pawns,
  boulders, practice dummies — carries `render::Grounded` and takes its z from
  `grass::y_sort` of its GROUND LINE in `sync_transforms`. So the clumps between
  you and the camera are drawn after you and swallow your legs, the ones behind
  you are drawn before you and don't, and walking north uncovers you a clump at
  a time.
  The ground line is `Pos.y - Grounded::reach`, and the reach is not decoration:
  a pawn's is its feet (0), but a boulder's is the southern rim of its own
  footprint (`rock.r`). Sorted by its centre instead, every clump standing in a
  boulder's southern half draws over it, which reads as **grass growing out of
  solid stone** — and it did. The other half of that fix is in `tuft_bands`,
  which skips candidates inside a rock at all (`Scenario::rocks`, so the rig,
  which has none, still grows grass where the arena's boulders would be).
  Bushes are exempt from all of this: their canopies sit at a fixed z 2.5, above
  the whole sort band, because you hide *under* a bush.
  A mesh has one sort key, which is the only reason bands exist, and the ONLY
  place a pawn can be slotted into the grass is between two bands — so some
  grass near your feet always sorts wrong, and the question is only how much and
  which way. Two things bound it, and both were reported before they were fixed:
  `GRASS_BAND` is 4 rather than 12 (150 band meshes and draw calls instead of
  50 — raise it first if the grass ever costs too much on a phone), and the key
  is the band's MIDDLE rather than its southern edge, which splits the error
  instead of piling it all on the north side. At 12-and-southern a blade grew out
  of your knee; at 4-and-southern a few still sprouted above the bottom of your
  boot; at 4-and-middle the worst case is 2 units either way.
  Consequences: the whole band `Z_SORT_LO..Z_SORT_HI`
  (0.1..1.8) is spoken for, so bullets (2.0), trails (1.9), the ADS aim line
  (1.85) and bush canopies (2.5) must stay above it; and boulders now sort with
  pawns, so you can walk behind one as well as in front of it.
  The predecessor was a "curtain": a band of blades parented to each pawn and
  scaled by `grass_cover`. Do not go back to it. Grass that moves with you reads
  as grass you are *wearing*, and it covered heads while boots stuck out.
- **Line of sight** (`client/src/vision.rs` + `client/assets/fog.wgsl`):
  render-only — the sim never computes visibility (it can't; every peer
  simulates every pawn). Split in two halves, and keeping them apart is what
  makes it tractable:
  * **The rays are continuous.** Cover resolves into `Cast` shadow cones swept
    from the viewer (see the camera model below); grass, being a depth field and
    not a set of casters, gets an elevation ray test instead (`grass_conceal`,
    below). Both answer the same question about any point: how much of someone
    standing THERE can be seen from HERE.
  * **The display is quantized to hexes.** The arena is a flat-top hex grid
    (`HEX_R` 16, ~750 tiles); each tile integrates that answer over its own area
    (`HEX_PROBES` points) and paints it flat over its middle (`TILE_PLATEAU`)
    with a rim blending to a value shared with the neighbours meeting at each
    corner — tiles without hard-edged polygons, and no seams, because both sides
    of an edge interpolate between the same two corner values. Tiles also ease
    toward their target over `TILE_EASE` (0.14s) instead of snapping. The fog
    draws over everything at z 5, so a pawn straddling two tiles is shaded by
    both, which is the tell that the fog belongs to the ground and not to them.
  Two things this buys beyond legibility: the mesh is static (only vertex colors
  change), and because the answer is quantized anyway it only needs recomputing
  when the viewer moves ~a third of a tile — standing still is free, walking
  updates ~20x/s instead of 60. Two things it costs: the soft penumbra is gone
  (that was `RIM_FEATHER`, the blur skirts and a per-pixel feather in
  `fog.wgsl` — `git log` if it's wanted back), and a tile carries ONE color, so
  bush haze no longer tints greener than boulder grey, only weaker.
  **Grass concealment** (`grass_conceal`): a blade of depth `g` at fraction `t`
  along the sight line, seen from an eye at height `E` (the viewer's
  `STANCE_HEIGHT`), hides the target up to `E + (g - E) / t` — similar triangles.
  The share of the body under that line then answers TWO questions, and keeping
  them apart is the whole model (`Block`):
  * **`covered`** — the WORST step on the line: how much of the target is behind
    grass at all. Pure geometry, saturating at 1. If the tallest blade between
    you only reaches his knees, his head is in clear air and no amount of
    distance can hide it.
  * **`length`** — Beer-Lambert extinction over the blocked length: how solid
    that grass is. Blades have gaps, so a hand's width is a screen you see
    through and a body's width is opaque. The ONLY place distance enters.
  Concealment is their product, so `grass_cover` (depth / `STANCE_HEIGHT`) is now
  exactly the ceiling rather than a cousin of it. `GRASS_EXTINCTION` is 0.12,
  anchored on the case the mechanic exists for: **two pawns lying either side of
  a body's width (~33 units) of shin-deep grass cannot see each other at all**
  (alpha 0.017 — `tools/grass-table.sh` asserts it). In the arena the answer
  depends on the tiles the line crosses, which is the point; down the spawn lane
  a standing viewer sees a standing target at alpha ~0.92 at 40 units, ~0.50 at
  80, ~0.30 at 150, and a prone one is gone past 80. A prone VIEWER is not
  automatically blind any more — in a short tile it still sees a standing pawn at
  ~0.57 at 40 units — but it loses everything past ~80. Going flat is mostly for
  breaking contact; where it also lets you fight depends on the tile you picked.
  The predecessor was extinction over the blocked length ALONE, with no
  geometric ceiling, and it was wrong in both directions at once: enough distance
  hid anybody behind anything (ankle-deep grass eventually erased a standing
  man), while 30 units of shin-deep grass — which you genuinely cannot see a
  prone man through — dimmed him by a quarter. The `covered` term is the fix.
  Note what it means with a tiled field: a long sight line crosses many tiles and
  `covered` takes the DEEPEST one, so range costs you visibility in steps as the
  line picks up deeper tiles — which is why the arena numbers fall off with
  distance even though `length` saturates within ~50 units. Widening
  `GRASS_MIN_H..GRASS_MAX_H` therefore hides people at range faster than it
  sounds like it should; check `tools/grass-table.sh`'s arena table after any
  change to the band.
  Tiles ask it about a STANDING target, which is the question a player asks of a
  patch of ground — asking about the dirt itself would darken the whole map,
  since grass hides dirt long before it hides a soldier.
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
  (rock + bush fields land and stay walkable, cover deflects an angled walk),
  plus `tests/combat.rs`, which drives a REAL synctest session a tick at a time
  (`TimeUpdateStrategy::ManualDuration` — bevy_ggrs steps off `Time`'s delta, so
  a manual clock makes ticks countable) to prove rounds land, damage accumulates
  into a death, and the pawn comes back whole. It runs at
  `with_check_distance(2)`, so it re-simulates every frame and checksums:
  rollback-unsafe health state fails there rather than as a desync in a match.
  Note `PlayerInputs` can only be filled by a session, which is why the systems
  can't just be called directly.
- **`tools/grass-table.sh [outfile]`** — the concealment measuring rig
  (`client/src/vision/strip_table.rs`). Two pawns either side of a ONE-HEX-wide
  strip of grass, one clear hex off it, tabulated over every grass depth x every
  stance pairing x both directions, in the units that matter: the sprite alpha
  `fade_hidden` would write, plus the blocked sight-line length that produced it.
  Run it before and after touching `GRASS_EXTINCTION`, `GRASS_NEAR_T`,
  `GRASS_SAMPLES`, `HEX_R`, `STANCE_HEIGHT` or `GRASS_MAX_H` — those constants
  are otherwise only judgeable by eye. The scene is `Scenario::GrassStrip` (see
  below), not the procedural field — which contains no patch of known depth
  aligned to known hexes, so measuring it there would test `GRASS_SEED`. It's
  also a real test — it asserts what it prints (bare ground hides nobody, deeper
  grass never hides less, lower is never easier to see, a prone viewer never sees
  more, each side's numbers mirror the other's, and the sim's `STRIP_HALF_W` /
  `STRIP_STANDOFF` still match the fog's `HEX_R`), and prints before asserting so
  a failure explains itself — including the spec the model is built around (two
  prone pawns can't see each other through shin-deep grass) and the guards that
  stop it being satisfied by hiding everyone always. It prints a second table
  for the ARENA — the same stances at 40/80/150/300 units through the real field
  — because the rig is a wall with clear ground either side and the arena is
  grass all the way, which changes the answer completely.
- **`Scenario` (`sim/src/lib.rs`) — the rig, playable.** `Scenario::Arena` is the
  game; `Scenario::GrassStrip { depth, east_stance }` is the concealment scene
  the table measures, built for real: `spawn_world` puts one pawn either side of
  the wall and spawns NOTHING else (no boulders, bushes or dummies), and
  `Scenario::depth` replaces the procedural field with grass `depth` deep inside
  `STRIP_HALF_W` of x=0 and bare ground everywhere else. The renderer asks the
  scenario rather than `grass_height` directly (`grass.rs`, `vision.rs`), so
  field, tufts, shade, fog and player fade all agree. Reach it with
  `?scenario=strip:<depth>:<east stance>` on the web or `AG_SCENARIO=strip:52:2`
  natively. **Offline only** — `parse_scenario` returns `Arena` whenever a room
  is set, because peers building different worlds is a desync by construction;
  `players` is forced to 2, `camera_follow` leaves the camera fixed on the wall,
  and `setup_scene` zooms it (`STRIP_ZOOM`). One thing that is easy to get wrong:
  the east pawn has no player, so its stance can't be a spawn value alone — the
  wire carries the level a pawn is ASKING for every tick, and `input.rs` would
  otherwise send "stand" for it and quietly stand it back up. Hence
  `Scenario::idle_stance`, which every non-first local handle sends.
- **`tools/grass-shots.sh [outdir]`** — that scene, photographed. Numbers can
  stay put while the picture rots, so this is the companion to the table above:
  it runs `grass-table.sh` first and takes both the depths AND each frame's
  caption from it, then drives a headless browser through
  `?scenario=strip:...`, cropping three frames per depth (both standing, east
  prone, west-camera prone) into `target/grass-shots/grass-strip.png`. The
  captioned alpha is the table's number for that exact pairing, so the picture
  and the measurement can't drift. Run it after touching `GRASS_EXTINCTION`,
  `GRASS_NEAR_T`, `STANCE_HEIGHT`, `HEX_R`, `grass.wgsl` or `gen_assets.py`'s
  grass tile. Notes for whoever edits it: it needs a CURRENT `_site` build
  (`tools/build-web.sh`) — it photographs the built wasm, not the source tree;
  playwright is not a repo dependency, so pass `AG_NODE_PATH=/path/to/node_modules`;
  and `SHOT`'s crop is sized to clear the HUD (health bar and roster above, the
  sights button below, stance buttons right), so a HUD move needs it re-checked.
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
