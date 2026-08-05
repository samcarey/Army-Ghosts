//! Rendering: the float world. Sim entities carry integer `Pos`; these systems
//! attach sprites to them and mirror `Pos` into `Transform` each frame. This
//! is the only place fixed-point becomes f32 — never feed anything from here
//! back into the sim.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{
    Bullet, Bush, Facing, Health, Player, Pos, Rock, Scenario, Stance, Team, ARENA_HALF_H,
    ARENA_HALF_W, STANCE_COUNT, STANCE_STAND, TEAM_COUNT,
};

use crate::ads::Ads;

/// Tell the HTML loader the game is actually drawing frames (it holds its
/// "Starting..." screen until `<body data-game-ready="1">` so it never cuts
/// to a blank canvas while pipelines compile / assets decode). A few frames
/// in is a good-enough proxy for "first real frame presented".
#[cfg(target_arch = "wasm32")]
pub fn signal_game_ready(mut frames: Local<u32>) {
    if *frames > 10 {
        return;
    }
    *frames += 1;
    if *frames == 10 {
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.set_attribute("data-game-ready", "1");
        }
    }
}

/// The soldier sprite sheet (tools/gen_assets.py): a grid of 72px frames.
/// Rows are the 16 facings, clockwise from away-from-camera; columns are three
/// stance blocks (standing, crouching, prone) of 13 animation frames each
/// (0 idle, 1-6 walk, 7-12 run).
///
/// The soldier is drawn 3/4 — modelled in 3D and projected 40 degrees off
/// top-down — so it must NEVER be rotated: head stays up and feet stay down,
/// and the facing comes from picking a row. That's why there's no
/// `orient_players` any more.
#[derive(Resource)]
pub struct SoldierSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

const SOLDIER_FRAME_PX: u32 = 72;
/// Animation columns per stance, and the full sheet width in columns.
const STANCE_COLS: u32 = 13;
const SOLDIER_COLS: u32 = STANCE_COLS * STANCE_COUNT as u32;
const SOLDIER_DIRS: u32 = 16;
/// Where the pawn's `Pos` sits in the frame, as a fraction from the top, per
/// stance. Upright figures are anchored where their boots MEET THE GROUND, so
/// they stand on their `Pos` and rise above it; a prone figure is anchored
/// mid-body, because that is what it pivots around when it turns.
///
/// These are the silhouette's bottom, measured off `soldier.png` — NOT the
/// generator's `SOLDIER_GROUND_PY` (58.5), which is where the model's z = 0
/// plane lands. The two differ, and it matters: a boot is a capsule with real
/// extent, and in a 3/4 projection the part of it nearest the camera projects
/// BELOW the origin it is standing on. Anchored on the origin, every pawn's
/// boots hung below the line the grass at its feet is rooted on, which reads
/// exactly as reported: "note foot is below grass".
///
/// Measured bottoms per stance block, over all 16 facings:
///
/// ```text
///   standing   idle 61..64   walk 59..66
///   crouching  idle 61..66   walk 60..67
/// ```
///
/// Anchored on the IDLE MAXIMUM, so a pawn standing still never dips below its
/// own ground line whichever way it faces. The facings that bottom out higher
/// then float up to 3 frame px — invisible, since nothing draws a contact
/// shadow. A walking pawn's trailing boot still swings ~2 px below, and that one
/// is left alone deliberately: anchoring on the walk maximum would hover the
/// figure at idle, and per-frame anchors would bob the whole body in place of
/// swinging the feet. Re-measure with a bbox pass over the sheet if the model or
/// `SOLDIER_TILT` changes.
const STANCE_ANCHOR: [f32; STANCE_COUNT] = [
    64.0 / 72.0,
    66.0 / 72.0,
    36.0 / 72.0,
];
const IDLE_FRAME: usize = 0;
const WALK_START: usize = 1;
const RUN_START: usize = 7;
const CYCLE_FRAMES: usize = 6;
/// Rendered size (world px). The figure fills most of the frame's height, so
/// this draws a soldier about 42 px tall standing on the 24 px collision
/// circle — roughly the proportions these games use.
const SOLDIER_SIZE: f32 = 56.25;
/// How far above a pawn's `Pos` its weapon is drawn, world px, per stance. The
/// 3/4 sprite stands with its feet on the Pos, so the rifle sits this far up
/// the screen; shots have to be lifted to match or tracers appear to leave the
/// soldier's boots. Derived from the rifle's height in the sheet: z units x
/// sin(40 deg) x 38 px/unit x (SOLDIER_SIZE / frame), plus the `STANCE_ANCHOR`
/// correction (4.3 px standing, 5.9 crouching) — moving the anchor to the boots
/// raised the whole figure against `Pos`, and the muzzle rode up with it. A
/// crawling soldier's rifle is all but on the ground, hence the near-zero third
/// entry, and prone's anchor didn't move.
const STANCE_MUZZLE_LIFT: [f32; STANCE_COUNT] = [26.3, 19.9, 3.0];

pub fn muzzle_lift(level: u8) -> f32 {
    STANCE_MUZZLE_LIFT[(level as usize).min(STANCE_COUNT - 1)]
}

/// The rifle's PHYSICAL height for that stance, world units — the concealment
/// model's number, not the renderer's. See [`army_ghosts_sim::STANCE_MUZZLE_H`]
/// for why these are two constants and not one.
pub fn muzzle_height(level: u8) -> f32 {
    army_ghosts_sim::STANCE_MUZZLE_H[(level as usize).min(STANCE_COUNT - 1)] as f32
}

/// Render-only: the height a bullet's tracer flies at, fixed when it spawns
/// from the stance its shooter fired in. Rollback re-spawns bullets, which
/// re-runs `attach_sprites` against the rolled-back stance, so this stays
/// correct through corrections.
/// Two different numbers about one rifle, and **keeping them apart is the whole
/// reason this is a struct**. `lift` is how far up the SCREEN to draw the
/// tracer, foreshortened by the 3/4 projection; `height` is how high off the
/// ground the round actually is. Using the first where the second was wanted
/// flew every round at a little over half its true height and let grass bury
/// shots a real rifle clears — see [`army_ghosts_sim::STANCE_MUZZLE_H`].
#[derive(Component)]
pub struct MuzzleLift {
    lift: f32,
    height: f32,
}

impl MuzzleLift {
    /// Screen offset, px — what `sync_transforms` draws with.
    pub fn lift(&self) -> f32 {
        self.lift
    }

    /// Physical height off the ground, world units — what grass has to beat, so
    /// `vision::fade_hidden` reads this and never `lift`. A prone shooter's
    /// rounds skim at 6 units and are buried by nearly anything; a standing
    /// one's ride at 44 and clear most of the field.
    pub fn height(&self) -> f32 {
        self.height
    }
}

/// Animation thresholds/cadence, world px/s. Full stick = 120 px/s (run);
/// partial thumbstick deflection walks. One stride cycle per 36 px walked
/// keeps footfalls glued to the ground speed.
const IDLE_BELOW: f32 = 6.0;
const RUN_ABOVE: f32 = 78.0;
/// …and once running, keep running until the pace falls back THIS far. The two
/// blocks are not two sets of legs: `gen_assets.py` builds the run frames with
/// `lean=0.16, crouch=0.05`, which pitches the whole UPPER BODY forward and
/// drops it, so a bare threshold does not flicker a stride, it flickers the
/// soldier's posture between two visibly different angles.
///
/// The band is where it is because of `heading_scale`, and this is the part
/// that makes the crossing routine rather than rare: at full deflection the
/// pace runs from 120 px/s straight ahead down to 90 square on and 67.5 dead
/// astern, all of it decided by where the BARREL points. A player holding the
/// left stick still and turning the right one sweeps the whole range, so
/// `RUN_ABOVE` gets crossed by aiming. 70 is under the 74.5 of a backward
/// diagonal and over the 67.5 of a straight backpedal, so a full-stick retreat
/// always lands on the walk frames however fast the player was going before it.
const RUN_BELOW: f32 = 70.0;
const CYCLE_LEN_PX: f32 = 36.0;
/// How long a stretch the pace is averaged over before it is believed.
/// `Pos` only moves on a SIM TICK and this runs on a RENDER FRAME, so whenever
/// the two rates aren't locked, consecutive frames see one tick's whole step
/// and then nothing at all — an instantaneous reading swings between double
/// pace and a dead stop, and on a 120 Hz phone it does it every other frame.
/// A window several ticks long makes the number a property of the pawn instead
/// of a property of the beat between two clocks.
const PACE_WINDOW: f32 = 0.1;
/// How long a pawn has to sit perfectly still before the legs are put away.
/// This is NOT a second helping of the window above: it is what keeps the
/// window's lag away from the moment a player stops walking, where a tenth of a
/// second of held stride reads as the animation sticking. It cannot be fooled
/// by the sampling beat, because that only ever skips a frame when frames are
/// SHORTER than a sim tick — a gap this long means the pawn really has stopped.
const STILL_ENOUGH: f32 = 0.05;

/// Render-only walk-cycle state (deliberately NOT rollback-registered:
/// cosmetic, so rollbacks never touch it and determinism is untouched).
#[derive(Component, Default)]
pub struct WalkAnim {
    phase: f32,
    last_pos: Option<Vec2>,
    /// Ground covered and time spent since the pace was last worked out.
    walked: f32,
    since: f32,
    /// How long since this pawn last moved at all.
    still: f32,
    /// The believed pace, and whether the run block is currently held.
    pace: f32,
    running: bool,
}

impl WalkAnim {
    /// Take one frame's travel and answer which of the 13 animation columns to
    /// draw. Split out from the system so the thing that actually flickered —
    /// the gait decision over a sequence of frames — can be tested without a
    /// window.
    fn advance(&mut self, moved: f32, dt: f32, level: u8) -> usize {
        self.walked += moved;
        self.since += dt;
        self.still = if moved > 0.0 { 0.0 } else { self.still + dt };
        if self.since >= PACE_WINDOW {
            self.pace = self.walked / self.since;
            self.walked = 0.0;
            self.since = 0.0;
        }
        // Whether a pawn is moving AT ALL is asked separately from how fast,
        // and answered without waiting for the window, because the window's lag
        // lands on the two instants a player is watching for it — the step off
        // and the stop. Both halves are proofs rather than estimates: ground
        // already covered inside this window can only grow, so it settles the
        // question early; and a still stretch longer than a tick settles the
        // other one (see `STILL_ENOUGH`).
        let moving = self.still < STILL_ENOUGH
            && (self.pace >= IDLE_BELOW || self.walked >= IDLE_BELOW * PACE_WINDOW);
        if !moving {
            self.phase = 0.0;
            self.running = false;
            return IDLE_FRAME;
        }
        // The cycle itself advances on DISTANCE COVERED, not on the averaged
        // pace: that sum telescopes exactly however the frames land, which is
        // what keeps footfalls glued to the ground.
        self.phase = (self.phase + moved / CYCLE_LEN_PX).fract();
        // Only a standing soldier can outrun the walk cycle: crouching and
        // crawling top out below the threshold, and the sheet's run columns for
        // those stances just repeat the walk.
        let bar = if self.running { RUN_BELOW } else { RUN_ABOVE };
        self.running = level == STANCE_STAND && self.pace > bar;
        let start = if self.running { RUN_START } else { WALK_START };
        start + ((self.phase * CYCLE_FRAMES as f32) as usize).min(CYCLE_FRAMES - 1)
    }
}

/// The tracer streak texture (drawn pointing +x; rotated to flight angle).
#[derive(Resource)]
pub struct TracerImage(Handle<Image>);

/// The boulder sheet (tools/gen_assets.py): one row of 96px grayscale blob
/// variants whose outlines average `ROCK_FILL_PX` of the frame, so a rock of
/// sim radius `r` draws at `2r * FRAME / FILL`.
#[derive(Resource)]
pub struct RockSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

/// The bush sheet: same 96px frames, but the bushes are *colour* (fractal
/// branches and leaves, see `tools/gen_assets.py`) and modelled in 3D at the
/// soldiers' viewing angle — so unlike boulders they have a definite up and
/// must never be spun by their seed.
#[derive(Resource)]
pub struct BushSheet {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

/// Both cover sheets use 96px frames; a piece of cover with sim radius `r`
/// draws at `2r * FRAME / (2 * FILL)`, where `FILL` is the mean radius the
/// generator fills its frame to.
const COVER_FRAME_PX: u32 = 96;
const ROCK_VARIANTS: u32 = 4;
const ROCK_FILL_PX: f32 = 40.0;
const BUSH_VARIANTS: u32 = 6;
const BUSH_FILL_PX: f32 = 30.0;

/// Canopy opacity. Deliberately partial: one bush is a smudge you can still
/// make out a soldier through, and overlapping bushes stack toward solid — the
/// same stacking the shadow layer does in `vision.rs`. Higher than it used to
/// be because the canopy now has real gaps in it: the leaves do some of the
/// see-through that a flat alpha used to have to do alone.
const BUSH_ALPHA: f32 = 0.90;

/// Bullet look: `tracer.png` is a 32x8 gradient streak — bright rounded head,
/// tail fading to nothing — stretched along the velocity to cover exactly the
/// ground the round crossed this frame. **One sprite is the whole tracer.**
///
/// It used to be a short sprite plus a chain of per-frame `TrailSegment`
/// rectangles, and that was reported exactly as it looked: *"a single shot with
/// its trail looks like a bunch of bullets and trails lined up tip to tip"*.
/// Three things made it read that way, and the third is the one that mattered.
/// Each segment carried a UNIFORM alpha, so the tail was banded rather than
/// graded. Consecutive segments stepped in brightness once per frame, and after
/// concealment was baked into them they could even step back UP, which no single
/// object ever does. And each segment was a frame's travel — 16 px — against a
/// 9 px bullet, so every dash in the chain was LARGER than the round leading it.
///
/// The texture already did all of this properly, which is what makes deleting
/// the chain a simplification rather than a trade: a gradient is what a capsule
/// with a fading tail IS, and one stretched quad has no seams to band.
const BULLET_COLOR: Color = Color::srgb(0.94, 0.70, 0.28);
/// Narrow, because the streak is now EVENLY lit where it used to average about
/// half its alpha down the gradient. One uniform 16x1.5 bar carries about the
/// same total light as the old 9x1.5 bullet plus its five trail dashes did —
/// the light is reorganised into one object rather than added to.
const BULLET_WIDTH: f32 = 1.5;
/// Shortest the streak is ever drawn — what a round gets on its first frame,
/// before it has any travel to smear across.
const BULLET_LEN_MIN: f32 = 13.0;

/// How many frames' travel the streak covers. At 40, and at
/// [`army_ghosts_sim::BULLET_SPEED`], that is about 3200 px — four times the
/// arena, so in practice **this no longer decides anything**: the length is
/// capped at how far the round has actually flown, and no round outlives the
/// arena. It is the ceiling, and what is drawn is the honest muzzle-to-round
/// distance. If a longer trail is wanted, the lever is the TAIL FADE in
/// `gen_assets.py`, not this. A shot reads as a bolt drawn down its whole line rather
/// than as an object travelling along it, and the tail duly reaches back past
/// the muzzle it left — invisibly, because the sprite fades out at that end.
///
/// **It is coupled to the SHAPE of `tracer.png` and cannot be changed alone.**
/// The engine lays one copy per frame spaced by a frame's travel, so a copy
/// covering `k` frames' worth ripples the integrated image by about `2 / k`. At
/// `k = 1` the copies abut and the sprite must be flat along its length or it
/// reads as a row of bullets — measured at 256% ripple, and reported twice. At
/// `k = 20` they overlap twentyfold, the ripple is nil, and the faded tip and
/// tail this sprite now carries are affordable. Bring this back down toward 1
/// and `gen_assets.py`'s `gen_tracer` has to go flat again.
const STREAK_TRAVELS: f32 = 40.0;
/// Most travel a single frame is believed to represent, as a multiple of one
/// TICK's flight. Anything past this is a rollback teleport rather than flying —
/// a round can be moved the width of the map between two frames — and is capped
/// here, at the travel, rather than at the drawn length: the length is whatever
/// [`STREAK_TRAVELS`] makes of an honest travel, so guarding the input is the
/// only place the guard means anything.
const STREAK_MAX_TICKS: f32 = 3.0;

/// Marks a sprite that stands ON the ground, so its draw order comes from where
/// it stands rather than from a fixed layer: `grass::y_sort` of its ground line,
/// plus a bias.
///
/// Everything in the field shares one z band — pawns, boulders, practice
/// dummies and every band of grass — which is what makes the grass behave: the
/// clumps between you and the camera are drawn after you and swallow your legs,
/// the ones behind you are drawn before you and don't. It also means you can now
/// walk *behind* a boulder as well as in front of one.
///
/// `reach` is how far SOUTH of `Pos` the thing's nearest edge sits, and it is
/// not decoration: a pawn's ground line is its feet (0), but a boulder's is the
/// southern rim of its own footprint. Sorting a boulder by its centre instead
/// let every clump standing in its southern half draw over it — grass growing
/// out of a rock, which is what it looks like.
#[derive(Component)]
pub struct Grounded {
    reach: f32,
    bias: f32,
}

/// Where this round was drawn last frame, so the streak can be stretched across
/// the ground it has covered since. Render-only; never rollback-registered.
///
/// Stretching to the TRAVEL rather than to a fixed length is what keeps the
/// tracer continuous at any frame rate: the streak is always exactly the smear
/// the round made, so consecutive frames abut instead of leaving gaps at low
/// rates or piling up at high ones.
#[derive(Component, Default)]
pub struct Streak {
    last: Option<Vec2>,
    /// The length last drawn, kept so a frame in which the round did not move
    /// redraws the SAME bar rather than collapsing to the minimum. A phone
    /// rendering above the tick rate sees no movement on half its frames, and a
    /// streak that changed size on those would pulse at the beat between the two
    /// clocks — the same trap `PACE_WINDOW` exists for on the walk cycle.
    len: f32,
}

/// The tint every soldier wears, multiplied over the grayscale sheet: one bag of
/// green army men, which is the whole reason this game looks the way it does.
///
/// Pale sage rather than anything field-coloured, and that is the one hard
/// constraint on it: the grass is a dark olive (62,74,42) and a soldier tinted
/// down into that range vanishes into it, which is the difference between
/// concealment the terrain grants you and a figure nobody can see anywhere.
///
/// **BOTH SIDES ARE THIS COLOUR, and it is the same constant twice rather than
/// two entries that happen to match**, so nobody drifts them apart by editing
/// one. Colour used to be the friend-from-foe reading and it deliberately isn't
/// any more — [`crate::nameplate`] is, and it says something colour cannot: an
/// unnamed figure is an enemy *from where you are standing*, which is a fact
/// about the viewer and so cannot live in a sim every peer runs identically.
/// The knock-on is that identifying someone is now a thing you do rather than a
/// thing the palette does for you, which is the point.
const ARMY_GREEN: Color = Color::srgb(0.60, 0.78, 0.52);
const TEAM_COLORS: [Color; TEAM_COUNT] = [ARMY_GREEN, ARMY_GREEN];

/// Human-facing names for the two sides. Used by the menu's team dial, the round
/// banner and the roster, so all three agree.
///
/// Phonetic rather than coloured, because the sides no longer differ in colour
/// and a scoreboard reading `GREEN 2 - 1 TAN` over two green armies would be
/// naming something you cannot see.
pub const TEAM_NAMES: [&str; TEAM_COUNT] = ["ALPHA", "BRAVO"];

/// How far a pawn's tint is nudged per slot within its side, so four figures in
/// one view are still four figures. Small on purpose, and now for a second
/// reason: it must never grow far enough to read as a side marker. It is keyed
/// on the slot WITHIN a side, so slot 2 of one side and slot 2 of the other wear
/// exactly the same shade — the nudge separates neighbours and says nothing
/// whatever about who they are fighting for, which is the whole arrangement
/// [`ARMY_GREEN`] describes.
///
/// Four steps is also the ceiling on how dark this may go. The bottom of the
/// range is what [`ARMY_GREEN`]'s note is about: keep stepping and the last man
/// on the line is field-coloured.
const TEAM_SHADE_STEP: f32 = 0.05;

/// The tint a pawn wears: army green, shaded a little by its place on the line.
pub fn team_color(team: Team, handle: usize) -> Color {
    let base = TEAM_COLORS[team.index()];
    // Handles alternate between the sides, so `handle / TEAM_COUNT` is the
    // pawn's place within its own — 0, 1, 2, 3 rather than 0, 2, 4, 6.
    let step = (handle / TEAM_COUNT) as f32 * TEAM_SHADE_STEP;
    let shade = 1.0 - step;
    let rgb = base.to_srgba();
    Color::srgb(rgb.red * shade, rgb.green * shade, rgb.blue * shade)
}

/// The colour a pawn flashes toward when a round lands on it, and how far. Not
/// all the way to red: the tint has to read as "that one just got hit" at a
/// glance across the field without repainting the figure into a different
/// player.
const HURT_COLOR: Color = Color::srgb(1.0, 0.32, 0.24);
const HURT_MIX: f32 = 0.65;

const Z_GROUND: f32 = -10.0;
/// Bullets and their trails ride ABOVE the y-sorted band: a round in flight is
/// off the ground, and a tracer that blinked out behind one tuft and back in
/// after it — a round covers 16 px a frame across 4-unit bands, so four
/// crossings every frame — would read as flicker rather than as cover.
///
/// **Grass hides a round by fading it, not by drawing over it**
/// ([`crate::vision::fade_hidden`]), and that is the more honest half of the
/// choice rather than a consolation for it: grass depth is a FIELD, so what a
/// round crosses is the whole sight line back to your eye, not just whichever
/// clumps happen to have been baked into a mesh in front of it. Sorting can only
/// ever answer for the tufts that got drawn.
///
/// There is no separate trail layer any more — the tracer sprite IS the trail,
/// so the round and its streak cannot come apart in the sort order the way two
/// entities on two z's could.
const Z_BULLET: f32 = 2.0;
/// Cover draws *below* the fog mesh at z=5.0 (`vision.rs`) — on purpose: each
/// shadow starts inside its own caster and rolls over its back, so the fog is
/// what shades every rock and bush from the player's side. Canopies go over
/// the boulders and the pawns — you hide *under* a bush.
const Z_BUSH: f32 = 2.5;

/// World units per pixel in `Scenario::GrassStrip`. The pawns stand
/// `2 * STRIP_STANDOFF` (96 units) apart, so at a third of a unit per pixel the
/// pair spans about 290 px of a phone-width window: both of them, the wall
/// between them, and enough ground either side to see where the grass stops.
const STRIP_ZOOM: f32 = 0.33;

pub fn setup_scene(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
    scenario: Res<Scenario>,
) {
    match *scenario {
        // The game: unzoomed, one world unit per pixel, following the pawn.
        // The gunfire demo is the game — walking around the noise at the scale
        // you would actually hear it at is the whole exercise, and the tracer
        // range is the same bargain: a round has to be judged at the size it is
        // actually drawn, so zooming would be answering a different question.
        Scenario::Arena | Scenario::Gunfire | Scenario::Tracers => {
            commands.spawn(Camera2d);
        }
        // The rig is a scene to be looked AT, not played: frame both pawns and
        // the wall between them from a fixed camera (`camera_follow` leaves it
        // alone), close enough to see what the grass is doing to the far one.
        Scenario::GrassStrip { .. } => {
            commands.spawn((
                Camera2d,
                Projection::Orthographic(OrthographicProjection {
                    scale: STRIP_ZOOM,
                    ..OrthographicProjection::default_2d()
                }),
            ));
        }
    }
    commands.insert_resource(SoldierSheet {
        image: assets.load("soldier.png"),
        layout: layouts.add(TextureAtlasLayout::from_grid(
            UVec2::splat(SOLDIER_FRAME_PX),
            SOLDIER_COLS,
            SOLDIER_DIRS,
            None,
            None,
        )),
    });
    commands.insert_resource(TracerImage(assets.load("tracer.png")));
    let cover_grid = |variants| {
        TextureAtlasLayout::from_grid(UVec2::splat(COVER_FRAME_PX), variants, 1, None, None)
    };
    commands.insert_resource(RockSheet {
        image: assets.load("rocks.png"),
        layout: layouts.add(cover_grid(ROCK_VARIANTS)),
    });
    commands.insert_resource(BushSheet {
        image: assets.load("bushes.png"),
        layout: layouts.add(cover_grid(BUSH_VARIANTS)),
    });
    commands.insert_resource(ClearColor(Color::srgb(0.08, 0.10, 0.06)));
    // Tiled grass/dirt ground across the arena (texture from tools/gen_assets.py).
    commands.spawn((
        Sprite {
            image: assets.load("ground.png"),
            custom_size: Some(Vec2::new((ARENA_HALF_W * 2) as f32, (ARENA_HALF_H * 2) as f32)),
            image_mode: SpriteImageMode::Tiled {
                tile_x: true,
                tile_y: true,
                stretch_value: 1.0,
            },
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, Z_GROUND),
    ));
}

/// Give every newly spawned sim entity its look. Runs on `Added<..>` so
/// rollback-respawned entities (bullets especially) get sprites too.
pub fn attach_sprites(
    mut commands: Commands,
    soldier: Res<SoldierSheet>,
    tracer: Res<TracerImage>,
    rock_sheet: Res<RockSheet>,
    bush_sheet: Res<BushSheet>,
    new_players: Query<(Entity, &Player, &Team), Added<Player>>,
    new_bullets: Query<(Entity, &Bullet), Added<Bullet>>,
    stances: Query<(&Player, &Stance)>,
    new_rocks: Query<(Entity, &Rock), Added<Rock>>,
    new_bushes: Query<(Entity, &Bush), Added<Bush>>,
) {
    for (entity, player, team) in &new_players {
        // Grayscale soldier sheet x team tint = one-color plastic figure. The
        // facing comes from picking a sheet row; walk/run frames from
        // `animate_players`. `update_health_visuals` rewrites the colour every
        // frame — this is only what it looks like before the first one.
        let sprite = Sprite {
            image: soldier.image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: soldier.layout.clone(),
                index: IDLE_FRAME,
            }),
            color: team_color(*team, player.handle),
            custom_size: Some(Vec2::splat(SOLDIER_SIZE)),
            ..default()
        };
        commands.entity(entity).insert((
            sprite,
            // Feet on the pawn's Pos, body rising above it (`animate_players`
            // re-anchors when the stance changes).
            Anchor(stance_anchor(STANCE_STAND)),
            WalkAnim::default(),
            Grounded { reach: 0.0, bias: 0.0 },
            Transform::default(),
        ));
    }
    for (entity, bullet) in &new_bullets {
        // Velocity is constant for a bullet's whole life, so the flight-angle
        // rotation is set once here; `sync_transforms` only writes translation.
        let angle = (bullet.vy as f32).atan2(bullet.vx as f32);
        // Fired from the shooter's rifle, wherever that was: a crawling
        // soldier's rounds skim the ground, a standing one's fly at chest
        // height. Frozen at spawn — the shooter may stand up mid-flight.
        let level = stances
            .iter()
            .find(|(p, _)| p.handle == bullet.owner)
            .map(|(_, stance)| stance.level)
            .unwrap_or(STANCE_STAND);
        commands.entity(entity).insert((
            Sprite {
                image: tracer.0.clone(),
                color: BULLET_COLOR,
                // Placeholder extent; `sync_transforms` restretches it every
                // frame to the ground this round has actually covered.
                custom_size: Some(Vec2::new(BULLET_LEN_MIN, BULLET_WIDTH)),
                ..default()
            },
            MuzzleLift { lift: muzzle_lift(level), height: muzzle_height(level) },
            Streak::default(),
            // The streak is far longer than the arena and is centred half its
            // own length BEHIND the round, so its centre is routinely off
            // screen while most of it is not. A sprite's `Aabb` is derived from
            // `custom_size`, which this one rewrites every frame — exactly the
            // stale-bounds shape that blinks `vision.rs`'s fog out — so the
            // cheapest correct answer is not to cull it at all. One short-lived
            // quad per round in flight.
            bevy::camera::visibility::NoFrustumCulling,
            Transform::from_xyz(0.0, 0.0, Z_BULLET)
                .with_rotation(Quat::from_rotation_z(angle)),
        ));
    }
    for (entity, rock) in &new_rocks {
        // Variant, spin and shade all come off the rock's own seed, so a dozen
        // boulders out of four textures still read as a dozen boulders — and
        // every peer draws the same field (cosmetic, but it keeps screenshots
        // comparable across machines).
        // Lightened when the grass went in: against an olive ground tile 0.38
        // read as stone, but with a sward around it a dark boulder reads as a
        // hole in the ground rather than a rock standing in the field.
        let shade = 0.52 + (rock.seed / 1024 % 64) as f32 * 0.0022;
        let angle = (rock.seed / ROCK_VARIANTS % 360) as f32 * std::f32::consts::PI / 180.0;
        commands.entity(entity).insert((
            Sprite {
                image: rock_sheet.image.clone(),
                texture_atlas: Some(TextureAtlas {
                    layout: rock_sheet.layout.clone(),
                    index: (rock.seed % ROCK_VARIANTS) as usize,
                }),
                color: Color::srgb(shade, shade * 0.98, shade * 0.92),
                custom_size: Some(cover_size(rock.r, ROCK_FILL_PX)),
                ..default()
            },
            // Sorted by the southern rim of its own footprint, so the grass
            // inside that footprint draws BEFORE it — a boulder displaces the
            // sward, it doesn't grow out of it. Only clumps standing in front of
            // the rim cover it, which is what grass in front of a rock does.
            // The hair of bias puts it just under the pawns: a boulder and a
            // soldier on the same line are close enough that the tie should go
            // to the soldier.
            Grounded { reach: rock.r as f32, bias: -0.002 },
            Transform::from_rotation(Quat::from_rotation_z(angle)),
        ));
    }
    for (entity, bush) in &new_bushes {
        // The bush frames carry their own greens, so the tint is near-white —
        // just enough per-seed value drift that neighbours in a thicket don't
        // look stamped. No rotation (a 3/4-view bush spun on its z tips over);
        // variety comes from six variants plus a mirror.
        let shade = 0.86 + (bush.seed / 1024 % 32) as f32 * 0.0075;
        commands.entity(entity).insert((
            Sprite {
                image: bush_sheet.image.clone(),
                texture_atlas: Some(TextureAtlas {
                    layout: bush_sheet.layout.clone(),
                    index: (bush.seed % BUSH_VARIANTS) as usize,
                }),
                color: Color::srgba(shade * 0.97, shade, shade * 0.93, BUSH_ALPHA),
                custom_size: Some(cover_size(bush.r, BUSH_FILL_PX)),
                flip_x: bush.seed / BUSH_VARIANTS % 2 == 1,
                ..default()
            },
            Transform::from_xyz(0.0, 0.0, Z_BUSH),
        ));
    }
}

/// On-screen size of a piece of cover: scale the frame so the blob the
/// generator filled it to (`fill` px mean radius) lands on the sim radius.
fn cover_size(radius: i32, fill: f32) -> Vec2 {
    Vec2::splat((radius * 2) as f32 * COVER_FRAME_PX as f32 / (fill * 2.0))
}

/// Where a stance's sprite hangs relative to the pawn's `Pos`.
fn stance_anchor(level: u8) -> Vec2 {
    Vec2::new(0.0, 0.5 - STANCE_ANCHOR[(level as usize).min(STANCE_COUNT - 1)])
}

/// Advance each soldier's walk/run cycle from their *rendered* travel (Pos
/// delta per frame — works for remote players too, and rollback corrections
/// just read as a brief stutter). Stationary → idle frame; sub-max analog
/// deflection → walk cycle; near-full speed → run cycle. The stance picks
/// which block of columns all of that indexes into.
///
/// This hands the frame's distance to [`WalkAnim::advance`] and does nothing
/// with it itself: turning travel into a gait is where the averaging window and
/// the run hysteresis live, and both of those are about a SEQUENCE of frames.
pub fn animate_players(
    time: Res<Time>,
    mut players: Query<
        (&Pos, &Facing, &Stance, &mut WalkAnim, &mut Sprite, &mut Anchor),
        With<Player>,
    >,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    for (pos, facing, stance, mut anim, mut sprite, mut anchor) in &mut players {
        let wanted = stance_anchor(stance.level);
        if anchor.0 != wanted {
            anchor.0 = wanted;
        }
        let (x, y) = pos.to_f32();
        let p = Vec2::new(x, y);
        let moved = match anim.last_pos {
            // Cap: a rollback correction or respawn can jump Pos; don't let
            // one frame's teleport read as supersonic legs.
            Some(last) => (p - last).length().min(6.0),
            None => 0.0,
        };
        anim.last_pos = Some(p);
        let Some(atlas) = sprite.texture_atlas.as_mut() else { continue };
        let column = anim.advance(moved, dt, stance.level);
        // Row = facing, measured clockwise from "away from the camera", which
        // is how the generator lays the sheet out; the stance selects which
        // 13-column block of that row to read.
        let bearing = (facing.x as f32).atan2(facing.y as f32);
        let step = std::f32::consts::TAU / SOLDIER_DIRS as f32;
        let row = (bearing / step).round().rem_euclid(SOLDIER_DIRS as f32) as usize;
        let block = (stance.level as usize).min(STANCE_COUNT - 1) * STANCE_COLS as usize;
        let index = row * SOLDIER_COLS as usize + block + column;
        if atlas.index != index {
            atlas.index = index;
        }
    }
}

/// Show what the sim's [`Health`] is doing to a pawn: flash it toward
/// [`HURT_COLOR`] for the few ticks after it takes a round, and hide it
/// entirely while it's down.
///
/// Must run BEFORE `vision::fade_hidden`, which owns the same sprites' alpha —
/// this writes rgb only and leaves the alpha where concealment put it. Hiding
/// goes through `Visibility` for the same reason: an alpha of zero here would
/// be overwritten a system later.
pub fn update_health_visuals(
    mut players: Query<(&Player, &Team, &Health, &mut Sprite, &mut Visibility), With<Player>>,
) {
    for (player, team, health, mut sprite, mut visibility) in &mut players {
        let wanted = if health.alive() { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
        let base = team_color(*team, player.handle);
        let tint = if health.hurt > 0 {
            base.mix(&HURT_COLOR, HURT_MIX)
        } else {
            base
        };
        // Alpha belongs to `fade_hidden`; only the color is ours.
        let alpha = sprite.color.alpha();
        sprite.color = tint.with_alpha(alpha);
    }
}

/// Mirror integer sim positions into render transforms.
///
/// A round is the one thing here that is not simply placed: it is SMEARED
/// across the ground it covered since the last frame, which is what makes one
/// stretched sprite a whole tracer (see [`BULLET_COLOR`]). The streak's bright
/// head sits on the round's true `Pos` and its faded tail reaches back to where
/// it was drawn last — so the sprite is centred half a frame's travel BEHIND
/// the round, which is exactly where the light of a moving object was.
pub fn sync_transforms(
    mut movers: Query<(&Pos, Option<&Grounded>, &mut Transform), Without<Bullet>>,
    mut bullets: Query<(&Bullet, &Pos, &MuzzleLift, &mut Streak, &mut Sprite, &mut Transform)>,
) {
    for (pos, grounded, mut transform) in &mut movers {
        let (x, y) = pos.to_f32();
        transform.translation.x = x;
        transform.translation.y = y;
        // Anything standing in the field re-sorts as it walks: draw order is
        // where your feet are, so the grass in front of you covers you and the
        // grass behind you doesn't (see `grass::y_sort`).
        if let Some(grounded) = grounded {
            transform.translation.z = crate::grass::y_sort(y - grounded.reach) + grounded.bias;
        }
    }
    // Rounds fly at the weapon height they were fired from, not ankle height.
    for (bullet, pos, lift, mut streak, mut sprite, mut transform) in &mut bullets {
        let (x, y) = pos.to_f32();
        let head = Vec2::new(x, y + lift.lift());
        // The flight bearing comes off the VELOCITY rather than off the
        // transform's own rotation, which `attach_sprites` set once at spawn:
        // reading back the thing we are about to write is how a rounding error
        // turns into a drift.
        let dir = Vec2::new(bullet.vx as f32, bullet.vy as f32).normalize_or_zero();
        // One tick's flight, which is what a round covers between two sim steps
        // and therefore the natural unit of "how much travel is a frame worth".
        let per_tick = (army_ghosts_sim::BULLET_SPEED / army_ghosts_sim::FP) as f32;
        // A round's FIRST frame has no previous position to measure from, so it
        // is credited one tick rather than nothing — otherwise every shot opened
        // on a stub and jumped to full length a frame later.
        let travelled = streak
            .last
            .map_or(per_tick, |last| (head - last).length())
            .min(per_tick * STREAK_MAX_TICKS);
        // **A trail cannot precede the shot.** At `STREAK_TRAVELS` frames long the
        // streak is longer than the arena, so a round that has only just left
        // the barrel would trail 1600 px of bolt back THROUGH its own shooter
        // and out the far side — which reads, exactly and wrongly, as a man
        // firing the other way. So the length is also capped at how far this
        // round has actually flown, which the sim already tracks as its
        // remaining `ttl`. The bolt therefore grows out of the muzzle and, since
        // no round outlives the arena, in practice always reads as a line drawn
        // from where the shot came from to where it has got to.
        let flown = (army_ghosts_sim::BULLET_TTL - bullet.ttl) as f32 * per_tick;
        if travelled > 0.5 || streak.len == 0.0 {
            streak.len = (travelled * STREAK_TRAVELS).min(flown).max(BULLET_LEN_MIN);
        }
        let len = streak.len;
        sprite.custom_size = Some(Vec2::new(len, BULLET_WIDTH));
        let mid = head - dir * (len / 2.0);
        transform.translation.x = mid.x;
        transform.translation.y = mid.y;
        streak.last = Some(head);
    }
}

/// Where the camera is easing to, before the aim shift. Tracked separately
/// from the camera transform so the follow lerp and the ADS ease don't fight
/// over the same value (the shift rides on top of the focus point).
#[derive(Resource, Default)]
pub struct CameraFocus(Vec2);

/// Keep the local player centered-ish: the camera eases toward them, offset by
/// however far sights are raised. (Uses floats freely — camera position is
/// render-only state.)
#[allow(clippy::too_many_arguments)]
pub fn camera_follow(
    local_players: Option<Res<LocalPlayers>>,
    spectating: Res<crate::spectate::Spectating>,
    players: Query<(&Player, &Pos)>,
    mut cameras: Query<&mut Transform, (With<Camera2d>, Without<Player>)>,
    mut focus: ResMut<CameraFocus>,
    ads: Res<Ads>,
    time: Res<Time>,
    scenario: Res<Scenario>,
) {
    // The rig's camera is fixed on the wall, showing both pawns at once — see
    // `setup_scene`.
    if matches!(*scenario, Scenario::GrassStrip { .. }) {
        return;
    }
    let Some(local) = local_players else { return };
    let Some(first_local) = local.0.first() else { return };
    // Whoever you are watching if you are out, otherwise your own pawn. The
    // camera eases to them rather than cutting, which also does the work of
    // making the change of subject legible — you see the ground go past.
    let watched = spectating.watching.unwrap_or(*first_local);
    let Some((_, pos)) = players.iter().find(|(p, _)| p.handle == watched) else {
        return;
    };
    let Ok(mut camera) = cameras.single_mut() else { return };
    let (x, y) = pos.to_f32();
    let t = (time.delta_secs() * 5.0).min(1.0);
    focus.0 = focus.0.lerp(Vec2::new(x, y), t);
    let aim = focus.0 + ads.camera_offset();
    camera.translation.x = aim.x;
    camera.translation.y = aim.y;
}

// NOTE: **do not clamp this camera to the arena.** It was, briefly, on the
// reasoning that a pawn standing on a muster post puts a third of the screen
// outside the world and that looks like a bug. Clamping looks far worse and was
// reported immediately: on any view wider than the arena the bound pins the axis
// outright, so the camera stops following you at all, and on a view slightly
// narrower it follows you *partly*, which reads as the camera being broken
// rather than as a deliberate edge. A camera locked to the player is the thing
// players actually notice; black beyond the wall is not.


#[cfg(test)]
mod tests {
    use super::*;
    use army_ghosts_sim::{STANCE_CROUCH, STANCE_PRONE};

    /// One frame of a pawn travelling at `pace` px/s, at `fps`.
    fn frame(anim: &mut WalkAnim, pace: f32, fps: f32) -> usize {
        let dt = 1.0 / fps;
        anim.advance(pace * dt, dt, STANCE_STAND)
    }

    /// Which of the three blocks a column belongs to, which is all the eye
    /// actually reads: idle, upright walk, or torso pitched forward to run.
    fn block(column: usize) -> u8 {
        match column {
            IDLE_FRAME => 0,
            c if c < RUN_START => 1,
            _ => 2,
        }
    }

    fn blocks(pace: f32, fps: f32, seconds: f32) -> Vec<u8> {
        let mut anim = WalkAnim::default();
        let frames = (seconds * fps) as usize;
        (0..frames).map(|_| block(frame(&mut anim, pace, fps))).collect()
    }

    /// The reported bug: at a pace near the run threshold the soldier's upper
    /// body flickered between two angles, because the run block pitches the
    /// torso forward (`lean` in gen_assets.py) and the threshold was bare.
    ///
    /// Swept across the whole band and back, the posture must change exactly
    /// twice: out on the way up, back on the way down.
    #[test]
    fn sweeping_through_the_run_threshold_changes_posture_once_each_way() {
        let mut anim = WalkAnim::default();
        let mut seen = Vec::new();
        // Settled into the walk first: for its first window a fresh pawn has no
        // pace to believe yet and shows the idle frame, which is a legitimate
        // change of posture and not the one under test.
        for _ in 0..12 {
            frame(&mut anim, 60.0, 60.0);
        }
        // 3 s up from a walk to a full run and 3 s back, at 60 fps — with a
        // small ripple on it, because no thumb pushes a stick smoothly and the
        // whole complaint is about a pace that sits near the bar and wobbles.
        for i in 0..360 {
            let t = (i as f32 / 180.0 - 1.0).abs();
            let ripple = 4.0 * (i as f32 * 0.7).sin();
            seen.push(block(frame(&mut anim, 120.0 - 60.0 * t + ripple, 60.0)));
        }
        let changes = seen.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(
            changes, 2,
            "posture changed {changes} times crossing the threshold twice: {seen:?}"
        );
    }

    /// Holding a pace INSIDE the hysteresis band is the same test standing
    /// still: whichever block the pawn arrived on, it must keep it.
    #[test]
    fn a_pace_inside_the_band_holds_whichever_posture_it_arrived_on() {
        let mid = (RUN_ABOVE + RUN_BELOW) / 2.0;
        let mut walking = WalkAnim::default();
        let mut running = WalkAnim::default();
        // Arrive from below and from above, then hold for two seconds each.
        for _ in 0..60 {
            frame(&mut walking, RUN_BELOW - 10.0, 60.0);
            frame(&mut running, 120.0, 60.0);
        }
        assert_eq!(block(frame(&mut walking, mid, 60.0)), 1);
        assert_eq!(block(frame(&mut running, mid, 60.0)), 2);
        for _ in 0..120 {
            assert_eq!(block(frame(&mut walking, mid, 60.0)), 1, "walk broke into a run");
            assert_eq!(block(frame(&mut running, mid, 60.0)), 2, "run dropped to a walk");
        }
    }

    /// `Pos` moves on a sim tick and this runs on a render frame, so a screen
    /// that isn't locked to 60 Hz sees one tick's whole step and then nothing.
    /// Sampled per frame that reads as double pace alternating with a dead
    /// stop; averaged over a window it reads as the pace the pawn is holding.
    #[test]
    fn the_beat_between_the_sim_and_the_screen_does_not_reach_the_gait() {
        let mut anim = WalkAnim::default();
        let dt = 1.0 / 120.0;
        // 60 px/s — a clear walk — delivered as 120 Hz frames carrying a whole
        // 60 Hz tick's step on every other one.
        let step = 60.0 / 60.0;
        let mut seen = Vec::new();
        for i in 0..240 {
            let moved = if i % 2 == 0 { step } else { 0.0 };
            seen.push(block(anim.advance(moved, dt, STANCE_STAND)));
        }
        // The opening window is idle (nothing is believed yet); after that the
        // walk must be unbroken — no run frames, and no blinking back to idle.
        assert!(
            seen[24..].iter().all(|&b| b == 1),
            "the sampling beat reached the gait: {:?}",
            &seen[24..]
        );
    }

    /// The stances that cannot outrun the walk cycle must never reach the run
    /// block, whatever the pace says — the sheet's run columns for those just
    /// repeat the walk, and a crawling soldier does not lean into a sprint.
    #[test]
    fn only_a_standing_soldier_reaches_the_run_frames() {
        for level in [STANCE_CROUCH, STANCE_PRONE] {
            let mut anim = WalkAnim::default();
            for _ in 0..120 {
                let column = anim.advance(120.0 / 60.0, 1.0 / 60.0, level);
                assert!(column < RUN_START, "stance {level} reached column {column}");
            }
        }
    }

    #[test]
    fn standing_still_idles_and_walking_does_not() {
        assert!(blocks(0.0, 60.0, 1.0).iter().all(|&b| b == 0));
        assert!(blocks(60.0, 60.0, 1.0)[24..].iter().all(|&b| b == 1));
        assert!(blocks(120.0, 60.0, 1.0)[24..].iter().all(|&b| b == 2));
    }

    /// The window averages a PACE, and a player would feel it if it also
    /// decided whether the pawn is moving: a tenth of a second of standing
    /// there after the stick goes over, and of held stride after it comes back,
    /// at every start and every stop. Both must land within a couple of frames.
    #[test]
    fn the_legs_start_and_stop_with_the_player_not_with_the_window() {
        let mut anim = WalkAnim::default();
        let mut started = None;
        for i in 0..30 {
            if frame(&mut anim, 60.0, 60.0) != IDLE_FRAME {
                started = Some(i);
                break;
            }
        }
        assert!(matches!(started, Some(i) if i <= 2), "legs started at {started:?}");

        for _ in 0..60 {
            frame(&mut anim, 60.0, 60.0);
        }
        let mut stopped = None;
        for i in 0..30 {
            if frame(&mut anim, 0.0, 60.0) == IDLE_FRAME {
                stopped = Some(i);
                break;
            }
        }
        assert!(matches!(stopped, Some(i) if i <= 4), "legs stopped at {stopped:?}");
    }
}
