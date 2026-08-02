# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

**Army Ghosts** — a web-based, mobile-first, top-down 2D shooter (old-PC *Army
Men* feel) built on Ghost Recon Wildlands *Ghost War* mechanics: two teams,
opposite ends of the field, **two-minute rounds with no respawning**, stealth and
hiding in grass and bushes. Built on **Bevy 0.18** with fully deterministic
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
`?room=CODE&players=N&bots=N&aggro=PCT&resume=0&signaling=wss://…`. No `room` ⇒
local synctest mode; `resume=0` starts a fresh world instead of picking up the
match this browser was in (see "Coming back"). Native equivalents: `AG_ROOM`,
`AG_PLAYERS`, `AG_BOTS`, `AG_AGGRO`, `AG_RESUME`, `AG_SIGNALING` env vars, plus
`AG_PLAYER_ID` and `AG_STATE_DIR`, which are native-only test hooks standing in
for `localStorage`. **index.html must forward every new param into
`__AG_NET__`** — it was missed once for `bots` and the param silently did
nothing.

## Architecture

- **`sim/`** (`army-ghosts-sim`) — the deterministic core. Integer-only: i32
  fixed-point positions (`FP = 256` subunits per world-unit/pixel), integer
  velocity/collision/isqrt math. All tick-state components are registered for
  GGRS rollback (snapshot/restore) and `Pos` is checksummed for desync
  detection. Generic over the ggrs `Config` so it never depends on matchbox.
  Runs in `GgrsSchedule` at `TICK_HZ` (60). `save.rs` is the one place it hands
  the world out whole — an integer/ASCII blob that is both the storage format
  and the wire format for a rejoin.
- **`client/`** — rendering (sprites; the ONLY place fixed-point → f32, via
  `Pos::to_f32`), input collection (`ReadInputs` schedule → `LocalInputs`),
  session bring-up (`net.rs`: launch-config parsing, matchbox socket,
  GGRS session build, and the rejoin handshake), camera follow, and everything
  that has to outlive the process (`persist.rs`: this browser's identity, and
  the match written to `localStorage`).
- **`harness/`** (`army-ghosts-harness`, bin `selfplay`) — the bot measuring
  rig, NOT part of the game build. Runs the real sim headless at whatever rate
  the CPU manages and decides whether one `BotProfile` beats another. See
  Testing → `tools/selfplay.sh`.
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

- **bevy_ggrs rollback**: peers exchange only `PlayerInput` (6 bytes:
  quantized i8 move x/y, quantized i8 **aim** x/y, a `buttons` bitflag byte, and
  a `dials` byte carrying
  bot aggression in bits 0-3 and this player's team request in bits 4-5, with 6-7
  spare), each peer simulates everything. Everything in both flag bytes is an
  ABSOLUTE value re-sent every tick, never an edge — see the stance and bot
  notes below for why rollback makes that the only workable choice.
  **Every multi-bit field encodes `1 + value`, so ZERO MEANS "NOT ASKING"** —
  stance, bot count, aggression and the team request alike, and the aim vector
  for the same reason by a different route (`PlayerInput::aim` falls back to the
  move vector, which is what `Facing` used to be outright, so every producer of
  an input that predates the second stick — bots, keyboard, harness, every test
  in the repo — keeps pointing exactly where it did). That is not
  tidiness. `PlayerInput::default()` is exactly what GGRS substitutes for a
  DISCONNECTED player (`sync_layer::synchronized_inputs` returns
  `T::Input::default()` from the disconnect frame on), so the all-zero encoding
  is the one an absent player transmits whether they mean to or not. Before
  this, refreshing your browser stood your pawn up out of the grass it was
  hiding in and — if you happened to hold handle 0 — deleted every bot in the
  match on the way out, because "no bots wanted" and "not asking about bots"
  were the same four bits. `a_blank_input_asks_for_nothing` pins it, and
  `sim/tests/persist.rs` pins what the sim does about it.
- **Lobby (open rooms)**: matchbox room string from `?room=`; socket URL is
  plain `{signaling}/{room}` (no `?next=` — everyone in the room meshes as
  they join). Channels in builder order: 0 reliable (lobby control), then a
  POOL of unreliable ones, one per session generation (`SESSION_CHANNELS`) —
  see the rejoin section for why a pool. Host = lowest sorted PeerId (identical
  on all peers). Everyone introduces themselves with `hi:<player id>:<gen>:<roster>`;
  the host taps START (or the room hits the `?players=` cap, default
  `MAX_PLAYERS = 8`) → broadcasts `go:0:<roster>|` on channel 0; every peer
  builds the session from that roster (Local for self; roster order = handle
  order), waiting until every member is reachable. Late joiners after start
  idle in warmup. Offline (no room)
  defaults to 1 player; explicit `?players=N` offline forces an N-handle
  synctest. UI on top: upper-left MENU → NEW ROOM (generated 5-char code,
  web navigates to `?room=`), COPY LINK beside the lobby roster (clipboard;
  both are bevy_ui `Button`+`Interaction`, which handles touch natively).
  `touch.rs` skips any touch that starts on a visible bevy_ui `Button`
  (`ComputedNode::contains_point`, in PHYSICAL px — multiply by
  `window.scale_factor()`), because the two stick zones are generous
  enough to swallow the sights/stance buttons otherwise; ask the UI where its
  buttons are rather than keeping a second copy of the layout. Same reason
  `read_start_input` ignores a tap while any button reads `Pressed`.
  **`START_BAND_Y` is logical pixels UP FROM THE BOTTOM EDGE, not a fraction of
  the height**, and it hugs the START pill's own row. As a fraction ("everything
  below 0.62") it was fine while the right thumb's home was a fire button off to
  one side of it, and stopped being fine the moment that thumb got a floating
  stick: a stick anchors wherever it lands, its natural home is the bottom
  centre-right, and the host walking around during warmup would have started the
  match by planting it. Laying the band out in the same units as the pill is
  what stops the two drifting apart on a screen of a different shape.
- **Controls: two sticks, and the trigger is the far end of one of them**
  (`client/src/touch.rs`). Left thumb walks, right thumb turns the barrel, and
  dragging the right one out to its rim (`AIM_RADIUS`) fires. Everything else is
  a bevy_ui button: the sights on the right edge above the aim thumb's arc, and
  the stance chevrons at the bottom CENTRE, the way down pinned to the bottom
  edge of the screen where a thumb finds it without looking.
  * **Firing is a stick position, not a button**, and the reason is the accuracy
    model. A thumb that has to leave the stick to shoot cannot turn while
    shooting — which is the one thing `Aim::swing` exists to charge for, so a
    scheme that made traverse-and-fire physically impossible would have quietly
    refunded the whole mechanic. It also takes pixel-precise thumb work out of a
    firefight: the whole right half of the screen is the control.
  * **Only the aim vector's DIRECTION reaches the sim; its length is spent on
    the trigger.** So aim is honoured at any deflection past `AIM_DEAD` and the
    remaining travel is free to mean something else. Inside the dead zone the
    barrel HOLDS where it was; only a lifted thumb releases it. Falling back at
    the centre would whip the aim to the movement heading every time a player let
    off the trigger by pulling in, and `Aim::swing` would charge them for the
    whip — an accuracy tax for stopping shooting.
  * **A player with an aim stick states an aim on EVERY tick, including the ticks
    their right thumb is off the glass** (`input::aim_request`). This is not
    tidiness, it is the fix for a real bug: leave `aim_*` blank and the sim's
    fallback hands the barrel back to the move stick, so **the left thumb starts
    steering** — which is exactly what a twin-stick player does not expect from a
    walk. With the thumb off, what gets asked for is the pawn's OWN current
    facing: a deliberate no-op, read back out of the world rather than remembered
    on the client, so there is no second copy to drift and no question about what
    to send on the first tick of a match.
    `TouchControls::touch_seen` exists solely to tell the two kinds of producer
    apart, because "does this player have a second stick" is a fact about the
    hardware and not about the world.
  * **The trigger bar rides the stick rather than sitting in a corner.** The
    stick has no fixed home — it is anchored wherever the thumb landed — and a
    progress bar that is not beside the thing making progress is a readout the
    player has to go looking for mid-firefight. It is bevy_ui rather than a
    camera-child sprite because it carries a word, and the word is the whole
    reason a player knows what the loading bar is for. It fills right-to-left,
    from the thumb's own side of the stick toward the far one, and both it and
    the knob light up on the shot.
  * It is the one piece of HUD that is not a `Button`, so it has to name itself
    in `nameplate.rs`'s `HudBoxFilter` or an edge plate will sit on it.
- **Aim down sights** (`client/src/ads.rs`): right-edge crosshair toggle
  (also Shift on a keyboard). The toggle is local UI state; it reaches the sim
  only as the `BTN_ADS` input bit, which SLOWS the pawn to `ADS_SPEED` (45%) of
  its stance's pace, so every peer applies the slow from the same input stream.
  * **It used to stop the pawn dead, and that was one price too many.** The sway
    model already charges for moving while aiming (the movement term in
    `Aim::settled`), and `Aim::stir` already gives a mover away in the grass — so
    rooting the shooter on top of those was charging for one decision three times
    and removing the choice rather than pricing it. You can now walk a weapon
    onto a target; you do it at a pace anyone watching has time to react to.
  * **`ADS_SWAY` multiplies the WHOLE settled hold, movement term included**, and
    that is what makes the change safe. `Aim::settled` used to RETURN EARLY on
    `ads`, skipping the movement term — exactly right while the sights rooted you
    and `speed` was therefore always zero, and a free perfect shot on the move
    the instant they didn't. Measured now (of 256): planted with sights 59,
    walking with sights 73, walking hip-fired 163, standing still hip-fired 133.
    Read the last two together — walking aimed beats standing unaimed, which is
    why anyone raises the sights while closing at all.
    `walking_with_the_sights_up_is_steadier_than_walking_without_them` pins the
    ordering rather than the numbers, so it survives tuning.
  * **The bots stopped needing the bit to mean "rooted".** `engage` used to set
    `BTN_ADS` purely to plant a bot, because the move vector doubled as the aim
    and zeroing it would have pointed the rifle at the ground. With aim on its own
    axis a bot says "walk nowhere, point there" directly — which is what freed
    the sights to mean only what they say.
  Everything else is render-only: the camera slides `ADS_SHIFT` (200 world
  units — half a "normal" mobile screen, deliberately fixed rather than
  window-relative) along the facing, smoothstepped over 500 ms, and a thin
  white line traces the shot to the first target it would hit, else the arena
  wall — flanked by two fainter lines at the edges of the cone the round is
  actually drawn from (see Accuracy). The shift rides on `render::CameraFocus`
  (the follow target) so the camera's own lerp doesn't fight the aim ease.
  * **The cone is the only thing that tells a player what the accuracy model is
    doing to them**, and that is why it is here rather than in a HUD readout.
    Everything the sim charges for happens to a number they cannot see, and a
    mechanic that silently decides whether your rounds land reads as the game
    cheating. Two lines opening and closing say it without a word: stop and they
    draw together, run and they fly apart.
  * It shows only with the sights up, deliberately. Hip fire is the state the
    cone is widest in and the one a player can least act on, and three lines
    swinging around the pawn at all times is permanent clutter on a phone.
    Wanting to know your spread is exactly the moment you should be aiming.
  * Each of the three is stopped by whatever is in *its own* way, not the
    centre's — an edge that clears a boulder the centre runs into is the useful
    half of the picture, because that is where a round can still get through.
- **Accuracy** (`sim/src/lib.rs`: the constant block above `SPREAD_MAX`, `Aim`,
  `settle_aim`, `fire_bullets`) — a round no longer leaves the barrel along
  `Facing`. It leaves along a direction drawn from a **cone**, and how wide that
  cone is says everything about what the shooter was doing when they fired.
  **This is the mechanic run-and-gun loses to.** Before it, a sprinting pawn and
  one that had lain still in the grass for a minute fired exactly the same round,
  so every incentive the rest of the game is built on — stance, grass, patience,
  the whole no-respawn premise — was undercut by the one system that decides
  whether any of it pays.
  Five inputs, kept apart because they answer different questions and tune
  independently. The first three shipped together; the last two were added when
  measurement showed the first three alone were not enough (see "How the goal
  was actually reached" below):
  * **`Aim::sway`** — how steady the hold is, a consequence of POSTURE:
    `STANCE_SWAY` per stance, plus `MOVE_SWAY` scaled by the *square* of the
    fraction of a full run being covered, times `ADS_SWAY` (0.45) if the sights
    are up. It has a settled value for any posture and it EASES toward it —
    `SWAY_RISE_TICKS` (10) up, `SWAY_SETTLE_TICKS` (96, 1.6 s) down.
    **The asymmetry is the whole mechanic**, not polish: if steadiness came back
    as fast as it went, a run-and-gunner could stop for two ticks and shoot as
    well as someone who had held the corner for a minute. Unsteadiness is
    instant; steadiness is earned.
  * **`Aim::bloom`** — recoil owed for rounds already fired, gaining
    `RECOIL_PER_SHOT` times the stance's `STANCE_RECOIL` share and bleeding
    `RECOIL_DECAY` a tick. This is BOTH of the "when did you last shoot"
    questions at once: how long ago falls out of the decay, how long you have
    held the trigger falls out of the gain outrunning it. Standing fire saturates
    in about 9 rounds, crouched in 19, prone in 85 — so the first round after a
    pause is the accurate one in every stance and how many you get after it is
    what the stance buys.
  * **`Aim::swing`** — how fast the aim DIRECTION has been traversing, charged
    by `Aim::turn` (in `move_players`, the one place old and new facing are both
    in hand) whenever the barrel turns faster than `SWING_FREE` (~1.1°/tick) and
    honed back down by `SWING_DECAY` (~0.7 s). What this prices is TRACKING, and
    the price is steepest up close with nothing in the code asking about range:
    angular rate is speed over range, so the same crossing walk that saturates
    the cone at 40 units (measured: peak swing 244/256) tracks for free at 150
    (measured: 0). Shooting a mover is harder than shooting a camper, most of
    all at knife range, and honing onto a still target is what the dead zone
    makes free. Two things about it:
    - The dead zone is also what keeps joystick wobble from taxing a player who
      is merely holding a direction.
    - **It is why bot aim jitter became a bounded WALK** (`Bot::drift_x/y`): the
      offset used to be redrawn from scratch sixty times a second, which was
      harmless while the aim point was bookkeeping and became a barrel VIBRATING
      several degrees a tick the moment `turn` priced traverse. Every bot's cone
      sat saturated and `rounds_are_fought_to_a_finish` ran all three rounds to
      the clock. Walking the offset keeps each tick's turn inside the dead zone,
      so a shaky bot pays with misses that drift across the target — which is
      also what a hand actually does — rather than with a cone it never earned.
  * **`Aim::stir`** — how much this pawn has been MOVING lately (instant rise,
    `STIR_DECAY` = 1 s to fade), and it does not feed the cone at all: it feeds
    CONCEALMENT. `Block::conceal_moving` scales grass concealment down by
    `MOTION_REVEAL` (0.75) times stir SQUARED, so a sprint through deep grass
    forfeits most of it (measured: a pawn seen at 34/256 while still is seen at
    201/256 sprinting), a crouched trot ~23%, a prone crawl ~7% — stance caps
    speed, so the stances price themselves. **`Aim::flash` sets stir to full on
    every shot**: a muzzle flash is a flare above the grass whatever stance you
    fire from, so one aimed shot buys a second of being seen and a held trigger
    is a held flare. Deliberately range-independent — you spot grass rustling
    across the whole map; what range buys is knowing what is under it.
  * **`SPREAD_MAX`** (0.22) — what the sway + bloom + swing sum is worth in
    angle, as the **tangent of the half-angle** rather than an angle, because
    the question is where the round lands and `offset = tan × distance` is one
    multiply where an angle would be a trig table.
  Things that are load-bearing:
  * **`Aim` is rollback-registered AND checksummed**, like `Stance` and for one
    step further on the same argument: two peers disagreeing about how steady a
    shooter is disagree about where its rounds went, and that only reaches `Pos`
    when somebody dies of it. It also carries its own LCG, advanced ONLY when a
    round is actually fired, so two pawns holding fire never drift apart.
  * **The draw leans slightly toward the middle and then stops dead at the rim**
    (`SPREAD_CORE`, `taper`) — two flat steps in a 17:15 ratio, not a bell.
    A bell fades the rim to nothing and quietly refunds most of what a wide cone
    is supposed to cost; a flat draw reads as a broken gun rather than an
    unsteady soldier. Three things about it are load-bearing:
    - **Leaning inward is a SUBSIDY TO THE CARELESS.** It lifts the odds a round
      lands by a factor that grows with how bad the cone already is — a tight
      cone gets nothing because it was hitting anyway, a saturated one gets the
      full `1/c`. Measured: at core 7/8 the run-and-gun kit went from +14 elo
      (flat) to **+108**. So `SPREAD_MAX` was widened from 0.22 to 0.32 to pay
      for it, and the lean kept gentle. **Change one and re-measure the other.**
    - **It is piecewise-linear so it INVERTS in closed form**, which is why
      `bot::shot_quality` can stay exact integer arithmetic instead of an erf
      approximation. That function is what the `discipline` gate and the whole
      close-the-distance judgement are built on, so a shape the bots cannot
      compute the hit probability of is a shape that silently miscalibrates
      every one of them. `shot_quality_matches_the_rounds_it_predicts` pins the
      predictor against the actual draw at five ranges and four spreads.
    - `shot_quality` works in SUBUNITS, not through `Aim::cone_half_width` —
      whole world units truncate, and when the cone is within a unit or two of
      the target's own size that truncation is most of the answer (240 predicted
      against 254 actual, caught by that test).
  * **`step`/`step_speed` exist so the aim model and the movement share one
    answer.** How fast a shooter is going is most of what decides its cone, and
    `settle_aim` runs in the same tick as `move_players` off the same `Intent`;
    two copies of that arithmetic that drifted apart would charge a pawn for a
    sprint it never took. There is nothing keeping them honest but this being the
    only copy.
  * **A fresh pawn is NOT perfectly steady.** `Aim::rest` (and the constructor)
    start `sway` at standing's settled value. Zero was the first version and it
    handed every pawn on the field one free unmissable shot at the top of every
    round — caught by `rounds_kill_and_the_dead_stay_down`, which was trying to
    prove a standing shooter *cannot* reach across the arena and watched it land
    52 damage before settling into anything.
  * **The cone is angular, so closing the distance opts out of it.** At 30 units
    even a saturated cone is narrower than a pawn, which means charging to
    point-blank genuinely neutralises the cone on its own — measured, twice:
    widening `SPREAD_MAX` to 0.50 with the stance sways rescaled was tried and
    did not change the verdicts, only broke three combat tests. The counters to
    the charge are the OTHER mechanics: the charger is lit up by its own stir
    the whole way in, and a head-on approach is the cheapest thing in the game
    to track (near-zero angular rate), so the settled defender shoots first and
    accurately. Read the cone as "firing while moving is punished", not
    "aggression is punished".
  **How the goal was actually reached** — "make careful play win" took three
  attempts, and the failures are more instructive than the fix:
  * The cone alone taxed a runner's own shooting, and a bot (or a person) who
    charges and only fires after arriving never paid it — measured as
    `aggression=0.9` costing only -26 elo.
  * Movement-costs-concealment and traverse-costs-accuracy added the taxes that
    cannot be opted out of: being SEEN on the way in, and being hard-tracked
    only when crossing, not when being charged head-on.
  * **Firing itself had to cost something, and the cost had to be concealment**
    (`Aim::flash`), because with no ammunition every withheld
    positive-expectation shot is a donation to the other side. `discipline=0.0`
    beat two successive quality-threshold trigger gates (+424 and +247 elo, one
    a clean 18-of-18 sweep) before this was understood — there is provably no
    trigger policy for a VISIBLE bot that beats "fire whenever anything might
    land". What a hidden bot loses by firing is its concealment, which is why
    the discipline gate binds exactly while hidden and never after (see Bots).
- **Which way you are walking costs speed** (`sim/src/lib.rs` `heading_scale`,
  `STRAFE_SPEED` 75%, `BACKPEDAL_SPEED` 56%): full pace straight ahead, linear in
  the cosine between, so a forward diagonal keeps 93% and a backward one 62%.
  Measured out of 256: ahead 256, fore-diagonal 237, square 192, back-diagonal
  159, back 144.
  * **This only became a question when aiming split off movement.** While
    `Facing` WAS the move vector there was no such thing as walking sideways —
    you pointed wherever you walked, by construction, and every step was a
    forward step. A second stick makes strafing and backpedalling expressible,
    and a game about creeping through grass should not let a player retreat from
    a firefight as fast as they advanced into it with their sights on it the
    whole way. It stacks multiplicatively with the stance and with `ADS_SPEED`:
    all three are fractions of a pace.
  * **It is measured against the INPUT's own aim, not the `Facing` component**,
    which keeps `step`/`step_speed` a pure function of `(input, stance)` — the
    property that stops the movement and the aim model disagreeing about how fast
    a pawn is going. Reaching for a component would have put `settle_aim` in the
    position of needing a `Facing` it does not query, and made the answer depend
    on system order.
  * **Anything that steers by walking pays exactly nothing**, and *exactly* is
    load-bearing: for a bot, the keyboard or a test, `aim()` falls back to the
    move vector, so the cosine is a vector with itself and comes out at precisely
    `FP` with no rounding slack. A whole-game slowdown of a percent or two would
    be invisible in review and would move every number the harness has ever
    printed.
  * **The first version DID reach the bots, and `a_match_never_stops_moving_
    around_an_idle_player` caught it — its seventh distinct cause** (a 62-second
    freeze). `Act::Push` walked at the target's TRUE position while aiming at its
    led, jittered estimate, so the leftover angle between the two became a
    movement tax that grew with the bot's own aim error: **one dial secretly
    steering two things**, the same mistake `ADVANCE` was carved out of
    `aggression` to undo. The fix is in the Bots section — a bot walks where it
    looks — and what survives is a real strafe: a bot sidestepping a boulder
    (`veer`) while holding its rifle on the enemy genuinely moves at 75%.
- **Stance** (`sim/src/lib.rs` `Stance` + `client/src/stance.rs`): standing /
  crouching / prone, driven by the two chevron buttons at the bottom centre (or
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
- **Health and damage** (`sim/src/lib.rs`: `Health`, `Deaths`,
  `Sweep`, `bullet_damage`, `resolve_hits`, `tick_health`): a round takes
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
  * Players and boulders resolve in ONE nearest-impact pass in
    `resolve_hits` (the rock test moved out of `move_bullets`), because cover has
    to stop a round that would otherwise carry on into someone behind it. Ties
    are broken by (distance along the sweep, position, handle) — query iteration
    order is not a determinism guarantee, and two pawns can share a subunit.
  * **There is no respawn.** `Health::down` used to be a countdown to one and is
    now a count of how long you have been OUT — it only ever clears at the top of
    the next round (`round::run_round`), and nothing else in the sim writes it
    back to zero. That is the single rule the rest of the design hangs off: a
    death costs the round rather than a walk, which is what makes grass, stance
    and patience worth anything.
    Death is still a flag rather than a despawn: rollback un-killing someone
    then only restores a component instead of resurrecting an entity the
    renderer has forgotten. While down you can't move, fire, change stance or be
    hit, and the client hides you via `Visibility` (NOT alpha — `fade_hidden`
    owns pawn alpha and would overwrite it a system later; `update_health_visuals`
    writes rgb only, and must run before it).
  * `Deaths` and `Kills` are both sim state, so every peer's scoreboard agrees
    without anyone sending a score message. `Kills` credits `Bullet::owner` in a
    second pass over the same query (the victim's borrow is exclusive), and it
    credits team kills too — the harness's "teams" are bookkeeping the sim knows
    nothing about, and a kill it declined to count would quietly reward friendly
    fire. It exists for the self-play harness, which used to score on it; the
    harness now scores ROUNDS WON instead (see Testing), and kills survive as a
    scoreboard line and a diagnostic.
  * **PAWNS ARE SOLID** (`separate_players`), and the reason is worth keeping.
    Nothing used to stop two pawns occupying the same subunit, and that turned
    out to be a *stable trap*: `Act::Fight` roots a bot (with the same bit the
    sights button set, in those days), so two that closed to nothing could never walk apart —
    and could never shoot each other either, because a round is born
    `PLAYER_R + BULLET_R + 2` (16) units down the barrel and a target 1.6 units
    away spans only to 15.6, so every shot was born past it and flew away.
    Measured before the fix: one pair spent **3100 consecutive ticks (52 s)**
    locked, both firing, neither losing a point of health. Reported from the
    demo as "two bots stand on top of each other and fire in opposite directions
    forever" — which is what mutual point-blank aim looks like from above.
    Mechanics that matter: it runs after `move_players` over EVERY living pawn,
    not just the ones that moved (both of those bots were standing perfectly
    still, so a movers-only pass would not have freed them); every push is
    computed against a snapshot taken before any is applied, so the result can't
    depend on query order and a pair separates symmetrically; and it early-outs
    before touching the rock field, which is the common case now that bots hold
    a standoff. `sim/tests/combat.rs`
    `bots_never_lock_together_firing_and_unable_to_connect` asserts the
    DURATION of contact, not that pawns never touch — a scrum is legitimate,
    being unable to leave one is not. Note `bot.rs`'s `PUSH_STANDOFF` alone does
    NOT fix this (measured: 1413 ticks still locked with separation off); it
    only stops them nosing together in the first place.
  * **Only ONE lane between the muster lines is clear**, and that is the rock
    layout doing its job — four clear corridors from one spawn line straight into
    the other would be four sniping alleys. It is slot 2: (-330, 65) to (330, 65),
    which is handles 4 and 5. Any test that needs a shot to arrive has to use that
    one and say so; `sim/tests/combat.rs` asserts `lane_is_clear` up front, so a
    reseeded field fails saying what actually changed. Everything else in that
    file closes to point-blank instead, which works from anywhere.
- **Coming back** (`sim/src/save.rs`, `client/src/persist.rs`, the rejoin half of
  `client/src/net.rs`) — refresh the page and you are back in the match you were
  in, in the same round, where you left off; while you are gone your pawn stands
  where you left it and can still be shot.
  **A rejoin is not a join.** GGRS fixes its player list when the session is
  built and has no join-in-progress, and a reloaded browser tab is a NEW
  matchbox peer with a new `PeerId` — there is no seat to slide back into. So
  every peer in the match REBUILDS ITS SESSION, at frame 0, from one agreed
  world: exactly the manoeuvre the warmup→p2p swap already performs, handed a
  world instead of a fresh one. That world is a `save::Save` blob, and the same
  bytes serve both paths the feature has:
  * **From storage.** `persist::autosave` writes the blob to `localStorage`
    three times a second. Offline this IS the feature — there is nobody to ask —
    and a refresh restores bots, health, stances, the round clock and the series
    score. In a room it is a first paint while the mesh forms, nothing more.
  * **From the other peers.** In a room the stored copy is stale by however long
    the reload took, so it is worth nothing next to the world the people still
    playing are in. One peer captures live and broadcasts it.
  Things that are load-bearing:
  * **The blob is a determinism boundary.** Every peer restores the same bytes
    and must land on byte-identical worlds or the first tick of the resumed
    session is a desync. Hence integers and ASCII only, pawns written in HANDLE
    order (capturing the same world twice must give the same string), a version
    tag that rejects a blob from an older build outright, and a parser that
    returns `None` rather than a half-built world — it is fed from
    `localStorage`, which the user can edit, and from the network, where any
    peer can. `two_peers_restoring_one_blob_stay_in_lockstep` is the test that
    matters: decode one blob twice into two independent apps, play 600 ticks off
    identical inputs, compare EVERY tick.
  * **A handle is a position in the roster and must never move.** A player who
    has gone leaves a VACANT seat rather than being removed, because removing
    one would slide every later handle down and hand a returning player somebody
    else's pawn. A vacant seat is registered `PlayerType::Local` by EVERY peer
    and fed a blank input by all of them — which is the only way GGRS will run a
    session with nobody in a seat, and which works only because a blank input
    now asks for nothing.
  * **A GGRS session eats its channel.** `take_channel` moves it out of the
    socket and the session drops it when replaced; there is no giving it back.
    So the socket is built with a pool and the generation number in the `go:`
    message names which one (`GGRS_CHANNEL_BASE + generation`) — a peer that has
    used none lands on the same index as a peer that has been through three
    rebuilds. `SESSION_CHANNELS` is therefore a hard cap on rebuilds per socket.
  * **The room state rides in the INTRODUCTION, not in a reply to it.** `hi:`
    carries `<player id>:<generation>:<roster>` in one message. As two messages
    (hello, then a "busy" answer) there is a race a returning player loses: it
    learns your NAME from the hello, a name is all it needs to build a roster,
    and if the answer has not arrived it elects itself host and starts a SECOND
    match in a room that already has one. Measured in two browser tabs, not
    theorised. Folding them together makes it unrepresentable.
  * **Exactly one peer answers a rejoin** — the lowest-numbered seat that is
    actually present — or two rosters race and the room splits between them.
  * **What is deliberately NOT in the blob**: the rock and bush fields (pure
    functions of fixed seeds), rounds in the air (dropped, as a round boundary
    drops them), and a bot's mind (seed and 24 ticks of sightings, rebuilt from
    the handle by `Bot::seeded` — costs the bots their memory, buys one less
    thing that could disagree).
  * **The gun IS in the blob** — `Aim`'s sway, recoil, stir, swing and its own
    LCG seed, all five, which is what took `FORMAT` to 2. Rebuilding them from the handle the
    way a bot's mind is rebuilt would hand a player who reloaded mid-burst a
    clean first shot, and the whole accuracy model is about what you were doing a
    second ago; a rejoin is exactly a second ago. Carrying the seed also removed
    a bug the moment it was written: `Aim::from_parts` defensively forced it odd
    (copied from `Bot::seeded`, where it is harmless), so half of all restored
    pawns came back with a gun that was not the gun that was saved.
    `a_restored_world_is_the_world_that_was_saved` caught it as a one-off in a
    single field, which is exactly what comparing BLOBS rather than hand-picked
    assertions is for.
  * **The dials go in the blob**, and that is not bookkeeping: restore five bots
    into a client whose bot dial reads zero and `reconcile_bots` will correctly
    delete all five. The dial is where the count lives; the pawns are its
    consequence.
  * **The bot dials moved off handle 0** (`dialled` in the sim,
    `MatchRoom::dial_holder` in the client): they are read from the lowest handle
    that is actually ASKING, because handle 0 can now be an empty seat, and
    "handle 0's copy" then means "nobody's copy" — the dials froze for the whole
    room until that one person came back.
  * Identity is `persist::Identity`: an opaque token in `localStorage`, minted
    once. Not a name or a login — it says "same browser as before" and nothing
    else, and clearing site data is quitting. `AG_PLAYER_ID` overrides it
    natively, which is how a test makes a second process claim to be the first
    one coming back.
- **Teams and rounds** (`sim/src/round.rs` + `Team`/`spawn_post` in `lib.rs`) —
  the Ghost War shape, and the thing every other rule now hangs off. Two sides
  muster on opposite ends of the field, fight for `ROUND_SECONDS` (120), and
  **nobody respawns**. A round ends the moment one side has nobody standing, or
  on the clock — and then the side with more people left takes it, level pegging
  being a draw. `INTERMISSION_TICKS` (6 s) later the next one starts, everyone up,
  everyone back on their post. `Deaths` and `Kills` carry across the series; only
  health, position, stance and the trigger reset.
  * **`Round` is a rollback REGISTERED RESOURCE**, which is a path nothing else
    in this repo uses. There is one clock and one scoreboard so a resource is the
    honest shape, but a clock that carried on through a rollback would read
    differently on the re-run — that is a desync, so it is snapshotted and
    checksummed like `Health`. `sim/tests/combat.rs` is what would catch it going
    wrong, because it runs at `check_distance(2)`.
  * **`TEAM_SPAWNS` are exact mirrors in x**, and that is load-bearing rather than
    tidy: the rock and bush fields are NOT mirror-symmetric, so which end a side
    draws is worth something, and the self-play harness cancels it by playing
    every trial from both ends. That only works if the ends differ in the terrain
    and in nothing else.
  * **Which side you are on is a REQUEST, in the input stream** (`dials` bits
    4-5, `DIAL_TEAM_MASK`), not a lobby message — same idempotence argument as
    the stance level and the bot count. Unlike the two bot dials, EVERY player's
    own copy is read, because it is a statement about the sender. `round::balance`
    grants it at the top of the next round if that side has room and overrules it
    if not, so no amount of tapping can stack one end of the map.
  * **Requests are honoured between rounds, never during one.** A pawn does not
    move when its side changes, so a mid-round swap would change its colours where
    it stands — and colour is now how you tell friend from foe, so everyone around
    it would silently change what it is with nothing happening on screen.
  * `balance` is a single pass with a cap and the simplicity is the point: two
    peers have to reach the same answer, and anything that ITERATED to a balance
    could reach it by a different route. The cap comes from how many pawns there
    are rather than from `TEAM_SIZE`, so three pawns split 2-1 rather than 3-0.
  * `default_side(handle)` — alternating — is a pure function of the handle, so
    team membership is knowable without looking at the world. That is what lets
    the harness put one profile on each side by parity and what lets
    `reconcile_bots` place a bot without consulting anything that could differ
    between peers.
  * A side with NO pawns has not been wiped out, it has not turned up. Without
    that guard a warmup with no bots ends a round on its first tick and every
    tick after it, forever.
  * Between rounds `move_players`/`separate_players`/`fire_bullets` are gated off
    (`round_is_live`), so nobody walks or shoots while the banner is up. Rounds
    already in the air are deliberately NOT frozen — a shot fired in time still
    arrives.
  * `run_round` returns immediately on any scenario but `Arena`, which is what
    keeps the grass rig's two carefully placed pawns where they were put.
- **What the HUD says while a round is live, and what it saves** (`client/src/hud.rs`)
  — the top of the screen carries the series score and the clock, the upper right
  carries a **troop count** (`> ALPHA 3` over `  BRAVO 2`), and that is all. The
  full scoreboard lives on the centred banner instead.
  * **The board used to sit in the upper right for the whole match**, and it was
    the wrong thing to have there: eight lines over a quarter of a phone screen,
    covering the field, answering a question nobody asks with a round in the air.
    What you want mid-round is one number per side — with no respawns, how many
    are left on each IS the state of the round.
  * The `> ` marker on your own side is not decoration. It is a PREFIX of fixed
    width on both lines rather than a suffix on one, because the block is
    right-justified against the screen edge and a marker only one line carries
    would step the other sideways. And it exists at all because the sides no
    longer differ in colour — with a tan army opposite you, which count was yours
    went without saying. `TEAM_NAMES` being two five-letter words is what makes
    the two lines the same width with no padding.
  * The banner shows the board **between rounds** (under the result and
    `NEXT ROUND IN 0:04`) and **for `ROSTER_FLASH` seconds whenever a pawn joins
    or leaves** — bots come and go on a dial, so "who am I actually playing with"
    changes mid-match and needs saying. `hud::watch_roster` owns that timer and
    `update_round_banner` only reads it.
  * `BoardFlash` stores each pawn's NAME beside its handle rather than re-deriving
    it, because a pawn that has just left cannot be asked what it was called and
    "BOT 3 LEFT" is the whole message. Two arming rules are deliberate: a change
    to or from an EMPTY roster does not count (the warmup→p2p swap walks through
    empty on its way, and announcing that would be reporting the machinery), and
    names are refreshed even when the handles didn't move, because the seat count
    bots are numbered from shifts at exactly that swap.
  * The count-in moved OFF the round line when it moved onto the banner. Two
    countdowns on one screen is one too many, and the top line is about the
    SERIES, which between rounds has not changed.
  * The banner's two text lines are one `BannerLine` enum component, not two
    marker types: one system writes both and two separate `&mut Text` queries
    would each need a `Without` filter against the other. An empty line is
    `Display::None`, not an empty string — `row_gap` spaces children whatever
    their size, so a zero-height node still costs the gap either side.
  * The board's columns line up because bevy's embedded `default_font` is
    **FiraMono**; padding to `NAME_COL` would be pointless in a proportional face.
  * **A red skull (`skull.png`, `gen_assets.py` `gen_skull`) marks who is out of
    the round**, in a gutter to the LEFT of the names so alive and dead still
    start on the same column. It replaced a `" +"` suffix on the name, which was
    meant as a printer's dagger and got asked what it meant — which is the answer.
    Three things about it:
    - **The icon is hidden with `Visibility`, NOT `Display::None`**, because a
      hidden node keeps its box and that box IS the gutter. `Display::None` would
      take it away and step every living pawn's name 22 px left.
      (`nameplate.rs` uses `Display::None` on its arrow for the opposite reason.)
    - It is what forced the board from one multi-line `Text` into a **pool of
      row nodes** — an icon has to be an `ImageNode` and a `Text` cannot hold one.
      Pooled at `BOARD_ROWS`, same pattern as `setup_nameplates`.
    - **Each row carries its own `index`.** Handing board lines out in query
      iteration order scattered them: a row's place on screen is its place among
      the container's children, and iteration order is ARCHETYPE order. The tell
      was the `ALPHA` heading appearing under the `BRAVO` block with the right
      names on the wrong side — found by screenshot, invisible to the tests.
    - Judge the art SHRUNK. It ships at 17 px, an 8x downscale, and the first
      version — jaw nearly as wide as the cranium, three narrow tooth gaps, a
      2 px mouth line — read as a red blob with two eyes. A narrow jaw (the waist
      is what says "skull" when nothing else survives), two fat tooth gaps and a
      thicker mouth line fixed it. Those numbers are measurements, not taste.
- **Teammate nameplates** (`client/src/nameplate.rs`) — a small green name over
  everyone on your own side, and an arrow on the edge of the screen for the ones
  who are off it. Render-only, on the same line as `spectate.rs` and `vision.rs`:
  which pawns are *yours* is a fact about who is holding the phone, and the sim
  has no point of view. Names keep showing while you are dead, which is when
  knowing who is who is worth most.
  * **`hud::pawn_name` is the one place a pawn is named**, and it exists because
    three places had drifted: the spectate button used to spell a bot's number off
    its raw handle, so the button said "BOT 5" about the pawn the roster called
    "Bot 4". Survivable while the two sat in opposite corners; not once the name is
    also floating over the soldier.
  * **This is the ONLY thing that tells friend from foe**, since both sides wear
    the same `ARMY_GREEN` (see Character art). A plate says *friend* and only ever
    appears over one, so an unnamed figure is an enemy — which means anything that
    stops a plate reaching a living teammate makes them shootable by their own
    side rather than merely anonymous. Weigh changes here accordingly.
  * The label's colour is fixed and bright rather than borrowed from anywhere: it
    has to carry over the dark olive sward, the pale dry earth of a bare tile, and
    a green soldier directly underneath it.
  * **Concealment does not dim a plate.** `fade_hidden` fades a teammate the grass
    is hiding and the name over them stays lit: concealment is about what the ENEMY
    can find, and nobody else's screen draws this.
  * It runs AFTER `render::camera_follow` and reads the camera's `Transform`, NOT
    its `GlobalTransform` — propagation happens in `PostUpdate`, so the global one
    is a frame stale and every name would lag the camera as you walked.
  * **An edge plate is anchored BY ITS EDGE, not by its centre** (`Placement::pivot`,
    which rides out as the `UiTransform` percentage translation). That is what lets
    `EDGE_MARGIN` be two pixels: pinned to the left, the plate hangs to the RIGHT of
    its anchor, so nothing has to leave room for half a name. Centred anchoring
    with a margin wide enough to hold the name is what "the arrows need to be
    closer to the edge" was reporting.
  * **Which means the HUD has to be dodged rather than avoided.** `hud_boxes` asks
    every button plus the round line, health bar, troop count and the banner's
    PILL where they are (`ComputedNode`, in PHYSICAL px — the conversion `touch.rs`
    got caught by), and `dodge` walks an edge plate AWAY from the edge it was
    pinned to until it is clear: down past the troop count at the top, up over the
    sights button at the bottom. Written down as numbers instead, it would be wrong
    for some match — the banner grows a line per pawn and is only up between
    rounds, START comes and goes, the spectate button only exists while you are
    out. The banner enters as `BannerPill` and NOT as `RoundBanner`, whose node is
    the whole window (that is what centres the pill) and would push every plate
    clean off the screen.
  * `dodge` deliberately leaves ON-SCREEN plates alone: a name that jumped clear of
    the HUD would no longer say which soldier it belonged to.
  * A side musters on ONE LINE, so at the top of a round every plate clamped to the
    same pixel and the three names read as one smear; `destack` drops each onto its
    own line.
- **Pawns are not seats** (`sim/src/lib.rs`: `Intent`, `Bot`, `read_human_intent`).
  `move_players`/`fire_bullets` used to index `PlayerInputs[player.handle]`
  directly, which made "is a pawn" and "has a seat in the GGRS session" the same
  thing — a bot would have needed a network handle and someone to send its
  inputs. They now read an **`Intent`** (a `PlayerInput` in component form) and
  don't care where it came from: `read_human_intent` copies it off the wire for
  human pawns, and the bot brain computes it from the rolled-back world for bot
  pawns. Since every peer simulates every pawn from identical state, every peer
  computes identical bot intents — **zero bandwidth, and no peer is
  authoritative over a bot.**
  `Player.handle` stays the pawn's identity everywhere (`Bullet::owner`,
  `Deaths`, the roster, `SPAWN_POINTS`); bots take handles straight on from the
  humans, so pawns are always `0..players+bots` with no gaps, capped at
  `MAX_PLAYERS` because that's how many spawn points there are.
  Things that are easy to get wrong here:
  * `read_human_intent` uses `inputs.get(handle)`, NOT `inputs[handle]` — a bot's
    handle is deliberately outside the session's range, and a stray one should
    not panic in the middle of a rollback.
  * `move_players` keeps `With<Player>` even though it no longer reads `Player`.
    It is load-bearing: it's what makes the query provably disjoint from the
    `Without<Player>` rock query, and both touch `Pos`. Drop it and bevy panics
    with B0001 on the first tick.
  * Intent runs FIRST in `GgrsSchedule` and nothing precedes it, because both
    intent systems want the world exactly as the previous tick left it —
    the round's own reset included, which is why `run_round` runs LAST.
  * `Intent` is rollback-registered even though it's rewritten every tick and so
    can't strictly go stale. A bot that ever wants hysteresis (holding a heading,
    committing to a rush) would evolve it, and finding out then means finding out
    as a desync.
  * **Bot count in a room must be AGREED, not configured.** `?bots=N`/`AG_BOTS`
    is offline only; `finalize_p2p_session` passes 0 deliberately, because two
    peers joining with different `?bots=` build different worlds, which is a
    desync before the first tick. In a room it has to ride in the host's start
    roster.
  `sim/tests/combat.rs` `bot_pawns_are_simulated_without_a_session_seat` builds a
  session for 4 handles and a world with 8 pawns; it runs at `check_distance(2)`,
  so bot state that isn't rollback-safe fails there rather than in a match.
- **Bots** (`sim/src/bot.rs`) — **utility scoring, because it is the only common
  architecture that is naturally rollback-safe**, and that falls out of the
  constraint rather than taste. A behavior tree carries a running-node cursor; an
  HTN plan (Killzone 2's bots) carries a task stack plus the world-state
  assumptions it was planned under; an FSM carries timers, and timers are where
  wall-clock creeps into a sim that must not read one. All must be snapshotted
  every tick and can desync. **Utility scoring carries almost nothing** — score
  the options against the world as it is, take the best — so re-evaluating a
  rolled-back tick returns the same answer by construction. Do not "upgrade" this
  to a behavior tree without re-reading that sentence.
  Three mechanisms are lifted from shipped shooters, in each case because the
  obvious implementation is the one that breaks under rollback:
  * **Reaction time is a RING BUFFER, not a timer** (Counter-Strike: Source's
    `UpdateReactionQueue`). `Memory` keeps `MEMORY_TICKS` (24) sightings
    round-robin and the bot attends to the one `profile.reaction` steps back.
    Fixed size, integer indices, measured in ticks: it snapshots for free, and
    the bot genuinely acts on stale information rather than being artificially
    slowed. It also does double duty — the *velocity* used for leading a target
    comes from differencing two entries, so there is no separate tracking state.
  * **`skill` and `accuracy` are DIFFERENT KNOBS** (Quake III's `BotAimAtEnemy`).
    `skill` GATES TECHNIQUES — above `LEAD_SKILL` the bot leads a moving target,
    below it it shoots where you were. `accuracy` only scales aim jitter. A weak
    bot is one that doesn't know to lead, not a good bot with shaky hands. The
    jitter is scaled BY RANGE so it's an angle, not a fixed offset; otherwise
    perfect accuracy at 300 units would be easier than at 30.
  * **Visibility is sampled across the body**, which is `visible_fraction`'s five
    points, so cover degrades in fifths instead of snapping.
  **What teams changed about the brain**, all of it measured rather than argued:
  * **Friend and foe are told apart in ONE place** — `look` skips same-team
    pawns, so a teammate never enters the memory buffer and therefore can never
    be aimed at, led, hunted or broken away from. Everything downstream reads
    the buffer, so there is no second place to get it wrong.
  * **A bot holds fire when a teammate is on the shot line**
    (`blocked_by_a_friend`, against the JITTERED aim point). Friendly fire is on
    and now costs a round rather than a walk, so this is arithmetic rather than
    politeness — but the GEOMETRY has to be an explicit projection along the
    shot, open at both ends, and a clamped point-to-segment distance is NOT that.
    `segment_hits_circle` folds everything behind the shooter onto the muzzle, so
    a teammate at your shoulder read as standing in your line; since
    `separate_players` holds pawns 24 units apart and the block radius is 26,
    every adjacent pair of teammates jammed each other's trigger permanently,
    whichever way either was facing. That is a DEADLOCK, not a missed shot: three
    bots huddled 32 units from a live enemy, all rooted and aiming, none firing,
    for the rest of the round. `only_a_friend_actually_in_the_lane_blocks_the_shot`
    and `touching_teammates_do_not_jam_each_other` pin both ends.
  * **`Act::Hunt` has an OBJECTIVE**, because a round opens with 660 units of
    grass between the two sides and nobody has a last known position to walk to.
    Without one the whole field crouches where it stands and every round is a
    draw. Which point took three tries and the harness settled it — see
    `objective`'s table; the two intuitive answers (the post facing you, the
    middle of your own lane) both leave the sides swapping ends without meeting.
  * **`ADVANCE` is a fixed weight, NOT `aggression`.** See the artefact note
    below: crossing the field when you have seen nobody is not aggression, it is
    playing the game, and conflating them made the best-measured aggression
    setting one that stops the match.
  * **`ARRIVED * 6` is the scale of "how far is it still".** Measured against the
    arena's own size (800 units) instead, the consideration falls under
    `Act::Settle` while a bot is still 165 units out, so both sides sit down 330
    apart — outside `ENGAGE_RANGE` — and a full arena fires NOT ONE ROUND in a
    minute.
  * **`Act::Settle`'s `FP / 8` is the pace of the whole game.** In grass this
    deep `visible` is a fraction almost everywhere, so `sharp(visible)` leaves
    Settle competitive far more often than "only when nothing else scores" makes
    it sound. Halving it to `FP / 16` takes the mean round from 66 s of fighting
    to 5.5 — eight bots wiping each other out before a human could cross the
    field. It is not a spare knob.
  * The visible consequence of all that: bots creep to point-blank range through
    the grass and kill in three shots, so a minute of eight of them is about
    FIFTEEN rounds fired and five deaths. Quiet, close and decisive is what this
    concealment model implies; it is not a sign they have stopped working.
  **What the accuracy model changed about the brain**, and it is less than it
  sounds because the architecture was already halfway there:
  * **`discipline` is a sixth `BotProfile` dial and it is AMBUSH PATIENCE — a
    gate on the trigger that binds only while the bot is still hidden.** It took
    three designs, and the two dead ones are worth more than the live one:
    - A gate on absolute `shot_quality` only ever passed point-blank (proximity
      saturates quality whatever the cone does — measured: "disciplined" bots
      fired at a mean spread of 250/256) and held fire FOREVER at long range.
      `discipline=0.0` beat it 23 of 25 decisive pairs.
    - A gate relative to the best-from-here honed shot fixed both ends and
      STILL lost +247 elo, and the loss is a theorem, not a tuning miss: fire
      costs no ammunition, so for a VISIBLE bot every withheld
      positive-expectation shot is a donation. No trigger policy beats "fire
      whenever anything might land" once you are seen.
    - What firing does cost is concealment (`Aim::flash`), and that cost only
      exists while you have concealment to lose. So the gate runs exactly while
      the bot is hidden — buried in its own grass, not lit by its own stir —
      and asks whether this shot is honed enough to be worth breaking cover
      for, `discipline` setting the bar as a share of the best shot available
      from here. One round later the flash has spent the concealment, the gate
      is moot by its own logic, and the bot fires at will like everyone else.
      The first shot out of an ambush is the aimed one; the burst after is not.
    It is a gate, not a consideration, so the three-considerations rule below
    is untouched. "Hidden" is the bot's honest estimate of itself —
    `is_concealed` at its own position plus its own low stir — both things it
    genuinely knows, so no peer can disagree.
  * **`Act::Fight` and `Act::Push` weigh `worth_a_round` where they used to weigh
    `nearness`**, and that swap is what stops the model deadlocking a match.
    Range alone said "you are close enough to shoot", which stopped being the
    same question as "you can hit them from here" the moment a round came out of
    a cone: two bots at a standoff both scored `Fight`, both rooted, both fired,
    and neither ever connected. Measured before the swap — the whole arena stood
    still for **38 seconds** and got through 4 rounds in 90 instead of 7.
    Scoring the *shot* closes the loop: a bot that cannot hit from where it
    stands scores `not(worth)` on `Push` and walks in until it can.
  * **A BOT WALKS WHERE IT LOOKS**, and the override that used to break that had
    to go for two independent reasons. `Act::Push` re-pointed the FEET at the
    target's true position after `engage` had aimed both feet and barrel at its
    led, jittered estimate. While the move vector doubled as the aim, that
    overwrote `engage`'s entire model with a dead-on bearing — so a closing bot
    shot straighter than its own profile allowed, and **`accuracy` was partly
    inert exactly while the range was still long enough for it to matter**. Then,
    once the axes were split and walking off your own facing started costing
    speed (`STRAFE_SPEED`), the leftover angle between feet and barrel turned
    into a movement tax that grew with the bot's own aim error — one dial
    steering two things again, and a 62-second freeze in the idle-player test.
    Deleting the override settles both: one direction, one meaning. Measured over
    40 matches: 2230 kills → 2192, mean round 24.5 s → 25.3, draws 44 of 360 →
    36. A bot now walks toward where it THINKS the enemy is, which is the more
    honest model anyway — and with leading on, walking at the lead point is
    walking to intercept.
    **It does NOT visibly move the dial landscape, which was worth checking
    rather than assuming** — the obvious prediction is that `accuracy` should
    bite harder once a closing bot actually pays for it, and 60-pair runs either
    side of the fix cannot see it: `accuracy=0.4` reads -350 elo before and -152
    after, and those are 2-of-17 and 10-of-34 decisive pairs whose confidence
    intervals (3.3-34.3% and 16.8-46.2%) overlap; `skill=0.2` goes +20 to +44,
    undecided both times. Read that as "unchanged within what the instrument can
    see", not as either number being real.
  * **`worth` is computed from the SETTLED cone, not the live one.** Using the
    live one reads as sound and is a feedback loop: moving widens the cone, a
    wide cone raises `Push`, and `Push` moves — so once a bot started closing it
    could never stop, because the reason it could not shoot was that it was
    walking. What that looks like from above is the whole side breaking into a
    charge and arriving in a heap. Asking what the shot would be worth *from a
    settled hold* makes the answer a fact about range and stance, which are the
    two things closing and getting down actually change.
  * **A bot holds fire for a lane as wide as the CONE, not as the aim line**
    (`blocked_by_a_friend`), measured where the teammate stands rather than at
    the target, since a cone opens with range. Without it the bots started
    shooting their own side the day rounds stopped flying straight.
  * **`Act::Hunt`'s third consideration is `risk`, and it is `hp` only while the
    bot can ACTUALLY SEE somebody.** Walking into a fight in progress while weak
    is what the hp term prices; searching is not that, and charging `hp` for it
    produced the same stall twice, at both ends of the mistake. Keyed on
    nothing: a lone survivor on 18 hp scored Hunt at `ADVANCE × 0.18 ≈ 23`
    against Settle's flat 32 and held still **84 seconds** with nobody in its
    memory. Keyed on a STALE contact: two hurt bots 45 units apart, each with a
    seconds-old memory of the other and neither able to see anything now, both
    prone in grass — both settled, **116 seconds**. A memory is not an enemy you
    are looking at. The rule that survives both: avoid a fight you can see,
    search whatever your health.
  * **Bots have no pathfinding and now notice when that isn't working**
    (`STUCK_TICKS`, `Bot::blocked`/`veer`). They steer straight at what they
    want and let `push_out_of_cover` deflect them, which works because it
    cancels only the into-the-rock part of a step — but a DEAD-ON approach
    cancels entirely and the bot pushes at the same boulder for the rest of the
    round (measured: 68 s, 23 units short of the enemy it was marching at, and
    `Act::Hunt` does not shoot). Three things this got wrong before it worked:
    - **"Did I move?" must be a checkpoint over a window, not tick-to-tick.**
      A bot leaning on a rock is shoved back to the surface every tick and lands
      on a different SUBUNIT, so exact inequality is true every tick while it
      goes nowhere.
    - **It must ask `step_speed`, not whether the move vector is non-zero** —
      a bot leaves `aim_x/aim_y` at zero and therefore still steers its barrel
      by walking (the fallback in `PlayerInput::aim`; humans have a second stick
      and bots do not), so an aiming bot (rooted by
      `BTN_ADS`) trivially "never moves". Getting this wrong rotated every
      aiming bot's aim a quarter turn: two bots at point-blank firing at right
      angles to each other, forever.
    - **The side it steps to must alternate when it doesn't help.** A fixed side
      is a coin flip against the geometry: a bot marching due north into a
      boulder veered due east into the arena wall and stayed there.
  * **Bots patrol, because a bot that arrived and found nothing used to sit down
    for the rest of the round.** `ARRIVED * 6` fades `Act::Hunt` out over the last
    stretch of the walk so a bot does not grind into the far wall — which is
    right, and which leaves `Act::Settle` the only thing scoring, and a bot that
    settles on empty ground never gets up again because nothing it can see will
    ever change. The recorded case: two bots stopped a hundred-odd units short of
    the enemy's line and spent the remaining **36 seconds** there, while the last
    live enemy lay prone and hidden in the middle of the map they had just walked
    across. Neither was hiding from the other; both had run out of anywhere to be.
    So the objective alternates ends, and the leg changes when the bot has watched
    one spot for `SEARCH_PATIENCE` (4 s) and nothing came.
    - **It is patience, not arrival.** Tying the flip to a distance means two
      thresholds that have to agree about what "arrived" means and don't: a bot
      stops advancing wherever `Hunt` falls under `Settle`, which moves with its
      health, so any fixed radius leaves a band where it is too close to keep
      walking and too far to have got anywhere. Measured with the flip at
      `ARRIVED`: it stopped 170 units short of a line it never reached and sat
      exactly as long as before.
    - **Contact resets it**, so a bot in a firefight is never on a schedule and
      one lying in ambush stays as long as it has anything to watch. Four seconds
      is deliberately long: holding still in cover IS the careful game.
    - Sweeping is also what finds someone prone in deep grass — concealment falls
      off with range, so a searcher that keeps moving eventually gets close
      enough and one that sits still never will.
  * **Most bot rounds come from a planted stance, but no longer all of them.**
    Before the ambush gate, no bot ever fired on the move (97 rounds over 90 s,
    none moving — `Act::Fight` roots the shooter, a free property of the
    behaviour list). The ambush gate deliberately reintroduced a minority of
    walking fire: an EXPOSED closer squeezing off taxed rounds is correct play
    now, measured at ~9%. `bots_stop_before_they_shoot` pins a 25% ceiling —
    walking spray as the rule rather than the taxed exception is the regression
    it guards.
  **The dial landscape after the full accuracy model is FLAT, and that is the
  measured result, not a failure to measure.** On the final tree (`--pairs
  80-100` against the shipping default): `aggression=0.9` about -61; the whole
  run-and-gun kit (`aggression=0.95,caution=0.05,discipline=0.0`) about -11,
  undecided at 100 pairs; `caution=0.05` about +27; both `discipline` extremes
  inside noise with ~70 of 80 pairs tied; `skill=1.0` dead level. Only three
  things stand out of the plain: `reaction` still dominates absolutely (5 sweeps
  every decisive pair, 23 loses every one), `accuracy` still registers (0.4
  about -249), and **`caution=0.9` — hide forever — loses every single decisive
  pair**, because a round is won by numbers standing and a side that never
  advances never changes the numbers.
  Read the flatness correctly before reaching for the dials:
  * Bots never produced run-and-gun inputs in the first place — they already
    stopped to shoot, and they close in straight lines, which is the cheapest
    thing in the game to track. The taxes the model levies fall on inputs
    HUMANS produce: strafing while firing, whip-aiming, sprinting between
    cover, holding the trigger. Bot-v-bot elo cannot see those.
  * An earlier tree on this branch showed run-and-gun losing decisively (-38,
    SPRT-confirmed) — and part of that edge was the hurt-survivor stall
    (`risk`, above): "careful" was winning partly by hiding out the clock in a
    way that also froze the game. Fixing the stall traded the headline number
    for honest resolution. When re-measuring after a bot change, know which of
    those two you are looking at.
  * Mean round on the final tree is ~33 s of fighting with a ~12% draw rate
    (was 71.5 s / 1.8% before the model). Faster and drawier: engagements
    resolve quicker because attackers are seen coming, and more rounds end
    with both sides still standing. Watch both numbers whenever these
    constants move.
  Things that will bite:
  * **Every behavior scores on EXACTLY THREE considerations.** Multiplying values
    in `0..=FP` drags a score down as considerations are added, which penalises
    the behaviors that think hardest; the textbook fix is the geometric mean,
    which is awkward in integer math. Sidestepped rather than solved — equal
    counts mean the bias is identical everywhere and cancels. Add a fourth to one
    behavior and you have silently down-weighted it. Add it to all five or none.
  * The RNG seed is **rollback state**. A bot whose seed didn't roll back would
    take a different shot on a re-simulated frame, which is a desync, not a
    glitch. `Bot::new` hashes the handle rather than adding it, because
    consecutive LCG seeds give visibly correlated first draws and "all the bots
    twitch together" is exactly the tell that gives it away.
  * A dead bot still pushes an empty sighting every tick, so "12 ticks ago" keeps
    meaning the same thing across a round boundary.
  * `SimPlugin` `init_resource::<Scenario>()`s, because `bot_think` asks what
    world it is in and anything building a bare app (the combat tests, the
    harness) would otherwise panic on the first tick.
  **How many bots there are rides in the INPUT STREAM** (`BTN_BOTS_SHIFT`, bits
  4-7 of the input byte, `0..=8`), applied by `reconcile_bots`. "Read it from a
  resource the menu writes" is the obvious implementation and is WRONG: a
  rollback re-runs a tick from a restored snapshot, and a resource the UI
  changed in between makes the re-run differ. An absolute count in the inputs is
  idempotent — replaying reconciles to the same number however often it happens
  — and it reaches every peer through the one channel they already agree on, so
  bots work in a room with no lobby message and no extra protocol. Same trick as
  the stance level, for the same reason.
  * Only the FIRST player's copy is honoured, so two people can't fight over it.
  * **`spawn_world` spawns no bots.** They arrive one per tick from the
    reconciler. Spawning some there as well gives the count two sources of truth
    and the reconciler — correctly — immediately undoes whichever it disagrees
    with. (This is exactly what happened when both existed.)
  * The reconciler picks the LOWEST FREE handle to add and the HIGHEST to
    remove, never query order: two peers must pick the same pawn.
  * `client/src/menu.rs` `BotCount` is UI state only; `input.rs` writes it into
    handle 0's input every tick. `?bots=N` / `AG_BOTS=N` seeds the same dial, so
    the URL and the menu are one setting (index.html must forward `bots` into
    `__AG_NET__` — it was missed once and the param silently did nothing).
  * `hud::board_lines` walks PAWNS, not session handles, or bots are absent from
    the scoreboard while busy killing people. A bot arriving or leaving is also
    what puts that board on screen mid-match (`hud::BoardFlash`).
  **How aggressive they are rides the same way** (`DIAL_AGGRO_MASK`, bits 0-3 of
  a SECOND input byte, applied by `apply_bot_dials`), and the menu's `− AGGRO n%
  +` row is the other half of it. Three things about it are load-bearing:
  * **`buttons` was full** — fire, sights, two bits of stance, four of bot count
    — so `PlayerInput` grew a `dials` byte and is 4 bytes on the wire, not 3.
    There are four spare bits in it for the next setting.
  * **It is applied EVERY TICK to bots that already exist**, not at spawn, or
    turning the dial would do nothing until you removed and re-added them, which
    is not what a dial in a menu means. That is only safe because the value is
    ABSOLUTE: writing the same level onto the same bot twice is writing it once,
    so a replayed tick lands on the identical world. `combat.rs`
    `the_aggression_dial_reaches_bots_already_in_the_match` runs it at
    `check_distance(2)`, so if that were wrong it fails as a checksum mismatch.
  * **Level 0 means "not asking"**, and that sentinel is what let this be added
    to the input at all: the self-play harness drives handle 0's input to set the
    bot count, and without it every run would have flattened both sides'
    aggression to one value and silently invalidated the measurement. The dial's
    lowest *position* is 1, which is aggression 0.0.
  `?aggro=PCT` / `AG_AGGRO=PCT` seeds it, same as `?bots=`. UI-side, the two
  rows are one widget (`menu.rs` `Dial` + `dial_row`) rather than two that have
  to be kept looking alike — a third setting is one line in `Dial::ALL`.
  Aggression is the dial that got a button because it is the one whose ends the
  harness separates most legibly (0.9 is about -251 elo against the default);
  the other four stay in `tools/selfplay.sh` where a number is measurable. NOTE
  the row now sits UNDER a `TEAM` row — three dials, still one widget.
  Tuning lives in `BotProfile` (skill / accuracy / reaction / aggression /
  caution) — one struct precisely so the self-play harness can vary it, which is
  `BotRoster`'s whole reason to exist: a per-handle profile table plus an RNG
  `salt`. It is CONFIG, not tick state — `reconcile_bots` reads it only at the
  instant a bot spawns and it must be constant for the match, exactly like
  `Scenario`. That constancy is the entire licence for reading a resource inside
  the rollback schedule; mutate it mid-match and it is the same desync that
  keeps the bot COUNT in the input stream instead.
  **The five dials are measured rather than picked** (`tools/selfplay.sh`, each
  against the shipping default, up to 150 pairs, elo from the pair win rate).
  **These are the post-ROUNDS figures and every earlier set in this file was
  measured on a different game** — respawning deathmatch, scattered spawns,
  scored on kills minus deaths. Where they disagree, these win; where a finding
  reversed, that is said below, because a reversal is more informative than
  either number on its own:
  * **`reaction` still dominates.** 5 ticks takes every decisive pair, 23 loses
    every one. Nothing else moves the result that far, which is unchanged from
    the old paradigm and is the reason it stays the difficulty knob.
  * **`caution` REVERSED, and it is the headline.** 0.1 is about **-482 elo** —
    the worst reading any dial has produced here — where under respawn it was
    +132. Both ends now lose (0.9 loses every decisive pair too), so the default
    0.5 sits near an optimum rather than on a slope. That is the whole paradigm
    change in one number: when a death costs a walk, recklessness is cheap; when
    it costs the round, it is the most expensive mistake available.
  * **`skill` now registers**, where it used to be indistinguishable from noise
    (0.2 was -28, 1.0 was -9). 1.0 is about **+162** and 0.2 about **-74**, both
    still short of a verdict at 150 pairs but both leaning the way the Quake III
    lineage would predict. Engagements happen at longer ranges across a field
    this size, so leading a target is worth something at last.
  * **`accuracy` matters and saturates.** 0.4 is about -155. 1.0 won 16 of 16
    decisive pairs and STILL came out undecided — sixteen wins is an LLR of
    +2.92 against a bound of +2.944, so it missed by one pair. Read that as
    "better, unproven", not as "no difference".
  * **Aggression is now only bad at the top.** 0.9 is about -251; 0.1 against
    0.5 is **150 pairs and not one of them decisive**. Under respawn 0.1 read as
    +432, and chasing that number is what turned up the bug in the next
    paragraph.
  **The aggression finding was an artefact, and the fix is worth knowing about.**
  `Act::Hunt` used to be weighted by `aggression`, so one dial secretly did two
  jobs: how hard to push someone you can SEE, and whether to cross the field at
  all. With the sides a field apart those come violently apart — a low setting
  is not "fight patiently", it is "let them do the walking" — so 0.2 beat the
  0.5 default in every decisive pair. Adopted as the default it played itself to
  a standstill: **330 of 450 rounds drawn**, mean round 101 s of a 120 s clock.
  Hunt now has its own fixed weight (`ADVANCE`), after which 0.2 and 0.5 are
  indistinguishable and the default was left alone. A dial whose best setting
  makes the game stop is a dial wired to the wrong thing.
  Read all of that as "which bot beats which bot", which is NOT the same
  question as "which bot is a good opponent for a person". The shipping default
  is deliberately still the documented "competent but beatable" one, because
  difficulty is a design choice and the harness only knows how to measure who
  wins rounds.
  `sim/tests/combat.rs` covers the three failures worth catching:
  `bots_decide_identically_in_identical_worlds` (two runs, 400 ticks, every pawn
  on the same subunit — catches an unseeded RNG or iteration-order dependence,
  which the synctest's checksums would NOT catch since they only prove state
  rolls back), `bots_open_fire_only_after_their_reaction_time`, and
  `bots_left_alone_fight_each_other` (30 s in one arena: ~21 deaths).
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
  * **Which gait block a pawn is on is a decision about a SEQUENCE of frames,
    not about this one** (`WalkAnim::advance`), and both halves of that were
    reported as a bug: "at medium running speeds the upper body keeps flickering
    back and forth between different angles". The run columns are not another
    set of legs — `gen_assets.py` builds them with `lean=0.16, crouch=0.05`, so
    crossing `RUN_ABOVE` pitches the whole TORSO forward and drops it, and a
    threshold crossed twice a second is a soldier changing posture twice a
    second. Two fixes, kept apart because they answer different questions:
    - **`RUN_BELOW` (hysteresis).** The band gets crossed constantly because of
      `heading_scale`: at full deflection the pace runs 120 px/s straight ahead,
      90 square on, 67.5 dead astern, decided by *where the barrel points*, so a
      player holding the left stick still and turning the right one sweeps the
      whole range. 70 sits under a backward diagonal (74.5) and over a straight
      backpedal (67.5), so a full-stick retreat always lands on the walk frames.
    - **`PACE_WINDOW` (averaging).** `Pos` moves on a SIM TICK and this runs on a
      RENDER FRAME, so when the two rates aren't locked, consecutive frames see
      one tick's whole step and then nothing — an instantaneous reading swings
      between double pace and a dead stop, and on a 120 Hz phone it does it every
      other frame. Averaging over ~6 ticks makes the number a property of the
      pawn rather than of the beat between two clocks.
    The window must NOT also decide whether the pawn is moving at all, or its lag
    lands on the step off and the stop, where a player feels it as the animation
    sticking. `STILL_ENOUGH` and the part-filled-window test answer that
    separately, and both are proofs rather than estimates: ground already covered
    can only grow, and a gap longer than a tick cannot be the sampling beat,
    which only skips a frame when frames are shorter than a tick. The cycle
    PHASE, meanwhile, advances on distance covered — that sum telescopes exactly
    however the frames land, which is what keeps footfalls glued to the ground.
  * The sprite is anchored at the figure's ground point (`STANCE_ANCHOR`), so
    feet stand on `Pos` and the body rises above it — except prone, which is
    anchored mid-body, because that is what a horizontal figure pivots around.
    `animate_players` re-anchors on every stance change.
  * Shots therefore have to be lifted (`muzzle_lift`: 22 px standing, 3 px
    prone) or tracers and the ADS aim line appear to leave the soldier's boots.
    Bullets carry the lift they were FIRED at in a render-only `MuzzleLift`
    (the shooter may stand up mid-flight); trails and the aim line apply it too.
  * **BOTH SIDES ARE THE SAME GREEN** (`ARMY_GREEN`, and `TEAM_COLORS` is that
    one constant twice rather than two entries that happen to match, so nobody
    drifts them apart by editing one). One bag of army men — which is the look —
    and the consequence is that **the tint no longer tells friend from foe;
    `nameplate.rs` does, and it is now the only thing that does.** An unnamed
    figure is an enemy. That is not a downgrade of a colour code, it is the
    reading moving to where it belongs: who is on your side is a fact about who
    is holding the phone, and the sim has no point of view, so it cannot be
    painted onto a sprite every peer draws identically. Identifying someone
    became something you do.
    The one hard constraint on the colour survives unchanged: it must sit well
    ABOVE the ground tile in value — the grass is a dark olive (62,74,42) and a
    soldier tinted into that range vanishes everywhere rather than only where the
    terrain hides him — hence a pale sage rather than anything field-coloured.
    `TEAM_SHADE_STEP` still nudges each pawn by its slot so four figures in one
    view are four figures, and it is keyed on the slot WITHIN a side precisely so
    slot 2 of each side matches: the nudge separates neighbours and must never
    grow into a side marker of its own.
  * **The sides are named ALPHA and BRAVO** (`TEAM_NAMES`, read by the menu's
    team dial, the round line and the roster so all three agree). Phonetic
    because they are no longer coloured — `GREEN 2 - 1 TAN` over two green
    armies would be a scoreboard naming something you cannot see.
- **Cover** (`sim/src/lib.rs`: `rock_layout` / `bush_layout`): two procedural
  fields, both pure integer rejection sampling from fixed seeds (`ROCK_SEED`,
  `BUSH_SEED`) — no floats, no RNG crate, so every peer builds the identical
  arena before the first tick, and `Pos` checksums catch it instantly if one
  doesn't. **Rocks** are solid: `push_out_of_cover` shoves the player back out
  along the surface normal, cancelling only the into-the-rock part of the step
  (so an angled approach deflects around instead of stopping dead), and bullets
  despawn on contact. **Bushes** stop nothing — they're concealment only, and
  come in overlapping clusters. Layout constants keep every gap walkable
  (`ROCK_GAP`/`ROCK_WALL_GAP` > the 24-unit player diameter) and keep the muster
  posts clear so a side can never be walled into its own line. **Nothing is
  excluded from the middle of the map any more** — the band that used to be kept
  clear ran from a spawn point to a practice dummy, and both ends of it are gone,
  so the centre now gets cover like everywhere else. `cargo test -p
  army-ghosts-sim` asserts the field stays walkable.
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
  boulders — carries `render::Grounded` and takes its z from
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
  **Grass concealment** — **the model lives in the SIM** (`sim/src/lib.rs`
  `grass_block`/`Block`/`visible_fraction`), in integer math, and the client
  calls it. It moved there when bots arrived: a bot decides from what it can
  see and every peer must reach the same decision, so the answer has to be
  integer and it has to be somewhere both the sim and the renderer can ask.
  `client/src/vision.rs` keeps only unit conversion (`Vec2`/f32 shares ⟷ `Pos`/FP),
  so there is exactly ONE implementation and what hides a bot is what hides a
  player by construction rather than by agreement. That includes motion:
  `fade_hidden` passes the enemy's `Aim::stir` into `Block::conceal_moving`, so
  a sprinter fades back IN on your screen by exactly the number the bots hunt
  it by. The fog TILES pass stir 0 — a tile asks about ground, and ground holds
  still.
  The port is checked, not asserted: `integer_concealment_matches_the_f32_model_it_replaced`
  reimplements the old f32 model verbatim and requires agreement within 2% over
  every stance pairing × 9 depths × 5 ranges, and
  `integer_and_f32_agree_on_the_tiled_arena` bounds the harder case — integer
  sample points can round to the far side of a hex edge from where the f32 ones
  landed, and since `covered` takes the WORST step, one reassigned sample moves
  the answer by a whole tile's depth. Measured: mean 0.0019, worst 0.0099.
  What did NOT move is `Cast`. That is a *camera* model — sight lines swept from
  behind either shoulder so you can peek around cover you're hugging — and it
  answers a different question from the one a pawn asks about itself, so the
  cover term is deliberately split: shoulder cameras for the player's view,
  a pawn-centred segment test (`visible_fraction`) for bots.
  The geometry: a blade of depth `g` at fraction `t`
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
  exactly the ceiling rather than a cousin of it. The extinction constant is
  **0.12 and no longer appears as a number anywhere** — it was folded into the
  integer `EXP_NEG` table, which *is* the constant now; tune it there and
  regenerate. It is anchored on the case the mechanic exists for: **two pawns
  lying either side of a body's width (~33 units) of shin-deep grass cannot see
  each other at all** (alpha 0.020, asserted by both `tools/grass-table.sh` and
  the sim's `prone_pawns_cannot_see_through_shin_deep_grass`).
  In the arena the answer depends on the tiles the line crosses, which is the
  point. Down the documented lane (viewer at (-150, 0), looking east) a standing
  viewer sees a standing target at alpha **0.875 at 40 units, 0.449 at 80, 0.266
  at 150**, and a prone one is gone past 80. A prone VIEWER is not automatically
  blind, but it is close: it sees a standing pawn at **0.121 at 40 units** and
  0.004 at 80. Going flat is for breaking contact; whether it also lets you
  fight depends entirely on the tile you picked.
  NOTE these figures are measured, and the previous set written here (0.92 /
  0.50 / 0.30, and 0.57 for the prone viewer) was stale — it came from an
  intermediate run before the final field constants landed, and was wrong at the
  commit that introduced it. The sim test `integer_and_f32_agree_on_the_tiled_arena`
  now prints this exact lane every run, so the prose can be checked against the
  code instead of trusted.
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
- **Gunfire you can hear but not see** (`client/src/sound.rs` +
  `client/assets/sound.wgsl`) — every round somebody ELSE fires throws a wedge
  of yellow light on the ground around your soldier, in a ring that never
  touches him, pointing roughly where the shot came from. Render-only, on the
  same line as `vision.rs` and `nameplate.rs`: what one pawn can hear is a fact
  about who is holding the phone, and the sim has no point of view. It is the
  counterweight to the concealment model — an enemy in deep grass 50 units away
  is genuinely invisible, and a firefight you cannot locate at all is arbitrary
  rather than tense. `Aim::flash` already made firing cost concealment to the
  BOTS' eyes through the ordinary sighting path; this is the human's half of the
  same bargain.
  * **THE ARC IS THE PROBABILITY DISTRIBUTION, and that is the whole design.**
    The opacity across the wedge is a bell (`bell`, a half-cosine, full at the
    centre and exactly zero at both rims) and the bearing error is drawn from a
    distribution of *that same shape*, so the brightness at an angle is
    proportional to the chance the shot really came from it. A player reading the
    picture the obvious way — bright means probably, faint means possibly, dark
    means no — is reading it correctly.
    - **This REVERSED the first version**, which drew a flat plateau and
      displaced it uniformly on the argument that a peak in the middle hands back
      the bearing the cue exists to withhold. It still does not hand it back —
      **the centre of any ONE arc is NOT the truth**, it is off by whatever the
      error field says at this offset. The truth is the AXIS OF THE SWING: walk
      at the sound and the wedge oscillates about the true bearing — average
      equal to the truth, amplitude equal to the bell's width, dwell time in the
      bell's own proportions (`walking_at_the_sound_swings_the_arc_around_the_
      truth` pins all three). A static read gives the width of the doubt; the
      bearing is earned by moving.
    - **The wedge's half-angle IS the error's whole budget** — there is no
      separate share held back (`OFFSET_SHARE` is gone), because the bell already
      vanishes at the rim. The shot is always inside the arc and the extremes of
      the arc are exactly the bearings it says are barely possible.
    - **The curve is written TWICE**, here and in `sound.wgsl`, with no way to
      share it across the shader boundary. `the_arc_is_the_probability_it_looks_
      like` is the only thing holding them together: it histograms the error over
      every offset on the map and holds each bin against `bell`.
  * **The error is a FIELD OVER THE OFFSET between the two pawns, not a clock and
    not a die** (`error_at`). It holds still while the two of you do — so six
    rounds from one rifle draw six arcs that agree, and there is nothing to
    average away — and it slides the moment either of you moves, in any
    direction, because only the difference of the two positions is ever read.
    Movement is what resolves a bearing, which is the same bargain the rest of
    the game makes. `ERROR_PITCH` (150/95 units) is how far you have to move
    relative to each other for a full sweep, ~1 s of walking, and the two are
    incommensurate so no heading holds the error still.
  * **A TRIANGLE wave for the sweep, then `asin` to bend it into the bell**
    (`triangle` → `toward_the_middle`), and both halves are load-bearing. The
    triangle is what makes the input EVEN — `sin` of the same phase is already
    arcsine-shaped and would pile the error up at the rim, the exact opposite of
    what is wanted — and `toward_the_middle` is the inverse CDF of the
    half-cosine, which is `asin` scaled and nothing more. Read it as motion,
    which is how it was asked for: **the drawn bearing crawls through the middle
    of the wedge and hurries through the edges**, because its rate is one over
    the density by construction. It spends its time where the light is, so the
    light is where it spends its time.
  * **Distance sets both the width and the strength, and it opens with the SQUARE
    ROOT of it.** The arena is 800x600 and nearly every shot fired in it lands in
    the first third of `HEAR_RANGE`, so a straight line spends most of its travel
    on distances that never happen and leaves every real engagement reading as
    "about here"; under a root, 300 units — a long shot in this game — is already
    a 70-degree wedge. So closing genuinely buys information (the arc and its
    error narrow together, both proportional) and standing off does not.
  * **A shot is identified by the FRAME IT WAS FIRED ON**, recovered from the
    shooter's `Cooldown` (`frame - (FIRE_COOLDOWN - left)`), with the last one
    per source remembered. There is no event to subscribe to and `Added<Bullet>`
    is a trap: bullets are rollback entities, so it fires again every time the
    frame they spawned on is re-simulated, which in a p2p match is most frames.
    Rollback rewinds the frame counter and the cooldown together, so the same
    shot recovers the same number however often it is replayed.
  * **A THIN band and a FLASH**: 5 world units of arc at 50 out (`RING_INNER` /
    `RING_OUTER`) for `PING_LIFE` 0.4 s. A fat wedge is a blob and what the eye
    reads off a blob is its bulk rather than its bearing; an arc that lingered
    would still be up when the next round landed, so a burst would read as one
    continuous glow instead of as gunfire. The fall is measured from the END of
    the attack rather than from the shot, or at a life this short the envelope
    peaks at 0.81 of full and the flash is a smudge.
  * Drawn at z 6.0 — ABOVE the fog (5.0), deliberately: a sound cue dimmed by the
    fog it exists to see through would be faintest exactly where it is needed.
    One shared unit quad, one `SoundMaterial` per live arc, the shape entirely in
    the fragment shader (a fan of triangles fine enough to hide its own facets
    would still band across the gradient).
  * **Whose ears these are is `render::camera_follow`'s choice, not
    `MatchRoom::me`** — the arcs are drawn around the pawn on screen, and hearing
    for one soldier while looking through another's eyes reads as the effect
    being broken.
  * See it: **`?scenario=gunfire`** (`AG_SCENARIO=gunfire`) — the arena with one
    pawn standing in the middle of it firing a round a second.

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
- **`FP` is 256, so a per-tick rate written as a fraction of it rounds HARD.**
  `RECOIL_DECAY` was first written `FP / 150`, meaning "a saturated bloom clears
  in 2.5 s". Integer division makes that **1**, not 1.7 — so it cleared in 4.3 s
  instead, and the extra 0.7 subunits a tick was the entire margin the stances
  were balanced on. Every stance saturated at the same rate: a prone shooter
  meant to hold a steady bipod was spraying at `SPREAD_MAX` after eight rounds,
  and the arena test that fires down the one clear lane could not kill anybody
  from any posture at all. The symptom pointed at the spread constants, which
  were fine. Write these as `FP / 128` or a plain integer — something that
  divides — and check the arithmetic in **whole subunits** before trusting it.
  `recoil_settles_at_a_different_rate_in_each_stance` asserts `RECOIL_DECAY >= 2`
  in a `const` block precisely so the next one is a build failure.
- **Don't run `cargo fmt` on this repo.** `main` is not rustfmt-clean and never
  has been (119 files' worth of diff), so formatting the crate you are editing
  buries a real change in several hundred lines of reflow — half of a 750-line
  diff, measured. If it happens anyway, the recovery is: stash your work, `cargo
  fmt` a clean tree and keep that as a baseline, restore your work, diff your
  tree against the baseline to get the semantic patch alone, then apply THAT to
  an unformatted checkout.
- **A FRESH WORLD MUST START A FRESH ROUND**, and forgetting it desynced every
  p2p match this game ever played. `Round` is a rollback-registered, CHECKSUMMED
  resource, so it is not part of the world — it outlives one. The warmup
  session's clock therefore ran straight into the match, and two peers cannot
  have warmed up for the same length of time, because the host sits waiting for
  the others to arrive. Measured in two browser tabs at GGRS frame 0, before a
  single tick of the match had been simulated: one peer's `Round.ticks` read
  **467** and the other's **149** — exactly the five seconds one spent waiting.
  Every match began desynced and said so at frame 10.
  **Frame 10 is not when it went wrong; it is `DesyncDetection { interval: 10 }`
  making its first comparison** — chasing what happens around frame 10 is
  chasing nothing. `spawn_world` now installs `Round::default()`, and
  `a_fresh_world_starts_a_fresh_round` is the regression test. Two lessons worth
  more than the fix: **a synctest can never catch this class of bug**, because
  it only ever compares a peer against itself, so the whole `check_distance(2)`
  suite passed through it; and when hunting one, align the two peers' state on
  the GGRS FRAME (`RollbackFrameCount`) rather than on anything the game counts
  — keying on the round clock compared a warmup world against a match world and
  read as noise for a run.
- **`RollbackOrdered` MUST be reset when a GGRS session is rebuilt**, and this
  one is a landmine. bevy_ggrs mixes `RollbackOrdered::order(rollback)` — a
  global, monotonically increasing registration index — into the PER-COMPONENT
  checksum (`snapshot/component_checksum.rs`, "Hashing the rollback index
  ensures this hash is unique and stable"), and the resource's own doc says it
  keeps entries "even if they have since been deleted". So the checksum of a
  pawn depends not on the pawn but on **how many rollback entities this peer has
  ever spawned**, over the whole life of the process. Two peers rebuilding a
  session from one agreed world therefore desync unless they happen to have
  spawned the same number of entities getting there. The original warmup→p2p
  swap survives only by that coincidence — every peer's warmup is the same one
  pawn and the same cover. Restoring a stored match into the warmup breaks the
  coincidence, and the symptom is maddening: byte-identical worlds, both peers
  logging the same round and the same pawn count, and `DESYNC at frame 50`.
  `finalize_p2p_session` now inserts a fresh `RollbackOrdered` between the
  teardown and the respawn, which is safe precisely because it is between
  sessions.
- **matchbox's `connected_peers()` is NOT the test for "can I reach this peer"**
  once a match is running. Measured: a browser tab that reloaded into a room
  with a match already in progress sat for 150 seconds with `connected_peers()`
  reporting NOBODY, while reading lobby messages — the whole rejoin handshake —
  from the very peer it was waiting for. matchbox raises `PeerState::Connected`
  only once EVERY channel of the socket has opened (`wait_for_ready` awaits them
  in turn), which is the likeliest reason a mid-match arrival never trips it.
  `net.rs`'s `reachable()` uses "we have received a packet from them" instead,
  which is direct evidence rather than an aggregate flag.
- **`?ice=none` does not work in a browser** and never has. It produces an ICE
  server entry with an empty `urls` list, and Chrome rejects the whole
  `RTCPeerConnection` construction with `SyntaxError: ICE server parsing failed:
  Empty uri` — which then cascades into a `RefCell already borrowed` panic from
  the wasm-bindgen executor, so the real error is several screens up. Native
  (webrtc-rs) tolerates it, which is why the flag reads as working. Fixing it
  means patching the vendored matchbox (`wasm.rs` wraps the config in a
  one-element list unconditionally); browser tests use default STUN instead.
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
  into a death, the death STICKS for the rest of the round, wiping out a side
  ends it, and the next round picks everyone back up on their post. It runs at
  `with_check_distance(2)`, so it re-simulates every frame and checksums:
  rollback-unsafe health, team or round state fails there rather than as a desync
  in a match — and the round is a rollback-registered RESOURCE, a path nothing
  else in the repo exercises, so this is the only thing that would catch it.
  Note `PlayerInputs` can only be filled by a session, which is why the systems
  can't just be called directly. `arena_with(humans, bots)` takes 0 humans and
  still builds a one-seat session, which is the shape the harness runs: a human
  pawn standing inert on a post is a free kill that quietly decides a match.
  Three of these are about the accuracy model and are worth knowing separately:
  * `rounds_kill_and_the_dead_stay_down` now fires **twice**, and the contrast is
    the test. Ten seconds of held trigger standing in the open at 660 units must
    NOT kill; the same soldier flat, braced and settled must. It is the one place
    the model's claim is stated end to end, and it is what caught a fresh pawn
    starting perfectly steady.
  * `the_cone_leans_toward_the_middle_and_stops_dead_at_its_edge` histograms
    200k rounds: the outermost bin must stay populated (a bell would empty it)
    and the inner half must outnumber the outer by a little and not a lot.
  * `bots_stop_before_they_shoot` pins that walking fire stays a taxed MINORITY
    (≤25%; ~12% measured) — the ambush gate made some of it correct play, so
    the bound is a ceiling, not the zero it once was (see Bots).
  * `the_aim_stick_turns_a_pawn_that_is_walking_the_other_way` pins the two
    thumbs staying apart end to end — turning on the spot without walking,
    walking without the barrel following, and a released aim stick going back to
    steering by walk (which is the fallback everything else in the repo relies
    on). It is in a session rather than a unit test because the turn is also
    where `Aim::swing` is charged, and a traverse charge that didn't roll back
    would be two peers disagreeing about how wide somebody's cone was — which
    only ever surfaces later, as a death.
  * `running_in_grass_gives_you_away` and
    `swinging_the_aim_costs_accuracy_until_it_is_honed_back_in` (lib tests) pin
    the stir and swing mechanics with printed measurements: still 34/256 seen
    vs 201 sprinting; peak swing 244 tracking a crosser at 40 units vs 0 at
    150.
  * **`a_match_never_stops_moving_around_an_idle_player` has now caught SIX
    distinct causes**, and the last three came from this branch: a hurt lone
    survivor that would not search (84 s), two hurt bots each sitting on a
    seconds-old memory of the other (116 s), and a bot walking into a boulder
    forever (68 s). It watches **200 seconds**
    now, not 90: a round that runs its full clock is a legitimate outcome (a
    still pawn is nearly invisible, so a last survivor genuinely can hide it
    out) and the old window quietly assumed every round ends in a wipe. Its
    round-count floor now only catches the clock itself wedging.
  * `recoil_settles_at_a_different_rate_in_each_stance` (a lib test) exists
    because `FP` is 256, so a per-tick decay written as a fraction of it is a very
    small integer and **rounds hard** — see the gotcha below.
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
  the wall and spawns NOTHING else (no boulders or bushes), and
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
  * **`Scenario::Gunfire` is the second rig, and it is the arena rather than a
    substitute for it**: real terrain, real cover, one pawn to walk around with
    and one standing dead centre firing a round a second (`DEMO_FIRE_TICKS`), so
    the sound arcs above can be looked at instead of waited for. Same
    offline-only rule, `players` forced to 2, and no bots — a bot would go
    hunting, and the point of the scene is ONE source of noise that stays where
    you left it. It fires NORTH rather than at you, because a demo that shoots
    the person looking at it is a demo that ends. The trigger rides in on
    `Scenario::idle_fire`, sent from the same idle branch of `input.rs` that
    poses the strip's east pawn: the sim is driven by inputs, and a pawn that
    fired because of a rule of its own would be a second source of truth for the
    trigger. Its frame counter is a `Local` in `read_local_inputs` rather than
    the round clock, which does not run in a scenario at all.
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
  and `SHOT`'s crop is sized to clear the HUD (health bar and troop count above,
  the STANCE COLUMN below), so a HUD move needs it re-checked — and one has
  happened: the stance chevrons moved from the right edge, which the crop misses
  by width, to the bottom centre, which it does not, so the rig's window had to
  grow from 620 px tall to 760 to keep them out of frame. The crop is now
  written as a half-size either side of the window's middle (where the rig parks
  the camera), so it follows the window instead of being two more numbers to
  keep in step.
- **`tools/selfplay.sh [options]`** — the bot measuring rig: does profile A
  actually beat profile B? Eight bots in the real arena, four a side, playing
  **Ghost War rounds** — muster at opposite ends, two minutes or until one side
  is wiped out, nobody respawning — and scored on **rounds won minus rounds
  lost**. `-c`/`-b` take `skill=0.8,reaction=6` style specs that fill in from the
  shipping profile, so a spec says exactly what is being varied — six dials now,
  `discipline` being the one the accuracy model added. A match is
  ~70 ms, so a verdict is usually a few seconds — run it before committing any
  change to `bot.rs` or `BotProfile`.
  It lives in its own crate (`harness/`, bin `selfplay`) for one reason: sim's
  rule is NO FLOATS and a likelihood ratio is made of logarithms. Keeping the
  statistics behind a crate boundary means that rule stays absolute rather than
  acquiring an exception. Nothing in `client/` or `sim/` depends on it.
  * **Rounds won, not kills minus deaths**, and that is not taste. Under respawn
    a death cost a walk and the trade WAS the objective. Under rounds a bot that
    trades one-for-one breaks even on kills and loses every round it is
    outnumbered at the end of, and one that kills three and dies has won nothing
    if its side still loses 4-1. Kills are still reported, as a diagnostic —
    "won every round killing nobody" and "won every round 4-0" are both possible
    and are not the same bot.
  * **Trials are PAIRS, not matches.** The two muster lines are exact mirrors but
    the rock and bush fields are NOT, so an unpaired run is partly measuring which
    END a profile drew. A pair is the same dice played with the candidate on each
    end in turn, and the two round differentials added. The cancellation is exact:
    two IDENTICAL profiles score 0 every single pair, because with the same
    profile on both sides the mirrored match is literally the same simulation
    (`identical_profiles_tie_every_time` asserts it).
    **The old 70 spawn splits are gone** — with two fixed lines there is exactly
    one split — so the salt is now the ONLY thing that varies a pair. That is a
    real loss of an independent source of variation, and it is why the run says so
    when the salt is inert: a bot that never misses never touches its dice, so
    `accuracy=1.0` on both sides leaves exactly one distinct pair and the run
    caps itself there rather than printing the same margin a hundred times.
    **The accuracy model quietly retired that caveat**: the gun has an LCG of its
    own, salted per match out of `BotRoster::salt` and drawn on EVERY shot
    whatever the profile is, so the salt is never inert now. The guard is still
    worth keeping — it costs nothing and it names a real failure — but no profile
    reaches it any more.
  * **A sequential test (Wald's SPRT), so it stops when the answer is in.** H0
    is "wins half the decisive pairs", H1 is `--p1` (default 0.60, ~+70 elo),
    alpha = beta = 0.05, bounds ±ln(19). Ties are dropped and counted rather than
    modelled, which is Wald's binomial test unmodified.
  * **THE TIE RATE IS THE THING TO UNDERSTAND about this harness**, and it is new.
    A pair's score is a small integer, so unlike a kills differential it lands on
    exactly zero a great deal of the time — typically only about a fifth of pairs
    come out decisive. That is what sets `--rounds` (default 9): measured against
    a caution=0.1 candidate, 3 rounds a match left 3 of 60 pairs decisive, 9 left
    12, 15 left 13. Worth knowing that the 3-round run did not merely say less,
    it pointed the OTHER WAY (66% on three decisive pairs against 25% on twelve).
    When a run reports mostly ties, **raise `--rounds` before `--pairs`** — the
    output says so.
  Two properties worth knowing before trusting a number: the whole thing is
  deterministic, so re-running a command gives the identical verdict — repeating
  it is not extra evidence, only more `--pairs` is. And **`NOT BETTER` means
  "not ahead by the margin", which covers both "worse" and "the same"** — read
  the rate line, which is why the output prints which one it was.
  `the_harness_separates_a_quick_bot_from_a_slow_one` is the test that the
  instrument works at all: reaction 3 vs reaction 23 on one-round matches must
  come out BETTER, or every number it ever printed was noise dressed as evidence.
  The run also prints the mean round length net of intermissions and how many
  rounds were DRAWN, which is the tell for the failure mode this scoring has and
  the old one didn't: two cautious profiles can spend the whole clock not finding
  each other, and a run full of drawn rounds measured almost nothing.
- **`a_match_never_stops_moving_around_an_idle_player`** (`sim/tests/combat.rs`)
  — the test for "then nothing happens", which is how the worst class of bug in
  this game gets reported. It watches 90 seconds from the seat that finds them:
  a player who stands on their post and does nothing, which is exactly the case
  every other test hides, because every other test has someone driving.
  It asserts MOTION, not kills, and that is the point. Three separate stalls have
  now been found here — a hurt bot lying down blind, a bot standing on a stale
  contact, and teammates jamming each other's shot lines — with unrelated causes
  and one identical signature: pawns alive, clock running, every position
  byte-identical for tens of seconds. The kills-and-rounds tests passed through
  all three, happily reporting a decided match. Without the friendly-fire fix it
  reports **50 seconds frozen and 3 rounds**; with it, 6 seconds and 7 rounds.
- **`sim/tests/persist.rs`** — the save/restore half of "coming back", in a real
  synctest session at `check_distance(2)` like `combat.rs`. Two promises, and
  they fail completely differently: that a restored world is the world that was
  saved (compared as BLOBS, so a new field has to survive this the day it is
  added rather than the day somebody remembers to assert on it), and that a
  restored world is a legal starting point for a LOCKSTEP session — one blob
  decoded twice into two independent apps, played 600 ticks off identical
  inputs, compared EVERY tick, because two worlds that diverge and re-converge
  would pass an end-state check while being exactly the bug. Also covers the
  disconnected-player rules directly: a blank input holds your ground and your
  stance, an absent player is still killable, and the bots survive whoever holds
  the dial dropping out.
- **`tools/persist-web.sh [all|refresh|rejoin|control]`** — the same feature in a
  real browser, which is the only place it actually lives (a native build does
  not have a window to refresh). `refresh` is offline and reliable: walk off the
  post, go prone, reload, prove the world that comes back is the stored one.
  `rejoin` is two tabs in a room with one refreshing mid-match; it asserts that
  the returning player is recognised by its stored id, that exactly one peer
  answers, that both move to the next generation, that both restore the same
  round from the same blob, and that nothing desyncs afterwards. `control` is
  the BASELINE and not a test of this feature at all: the same two tabs with
  nobody refreshing. **Run the control first whenever `rejoin` reports a
  desync** — it says whether the baseline is broken or the rejoin is, and it is
  exactly how the round-clock bug in "Gotchas" was separated from the feature
  that exposed it.
  Needs a current `_site` (`tools/build-web.sh`) and `AG_NODE_PATH`, same as
  `grass-shots.sh`.
- **`tools/rejoin-test.sh`** — the native two-process equivalent, and **it has
  never been run**: a native build launched from a shell with no window-server
  session never runs a frame at all (winit gets no event loop, so Startup never
  fires), and a binary from `main` behaves identically. It needs a terminal that
  can open windows.
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
