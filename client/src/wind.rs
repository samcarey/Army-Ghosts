//! Wind: the one thing on the map that moves without anybody deciding to.
//!
//! **The point is camouflage, not weather.** In a game about creeping through
//! grass, a perfectly still field is a betrayal: it makes the human eye's best
//! trick — motion detection over a static background — free and infallible, so
//! a prone soldier crawling a pixel a tick is the ONLY thing changing on
//! screen and reads instantly however deep the grass he is in. The concealment
//! model already says he cannot be seen; the renderer was giving him away
//! anyway. Foliage that stirs on its own puts that crawl back among a thousand
//! other small movements, which is what long grass actually does.
//!
//! # What air does, and what water does
//!
//! The first version of this file got that wrong in a way worth writing down,
//! because the mistake is the obvious implementation. It leaned everything
//! along a travelling **sine wave**: a crest sweeps across the field, blades
//! bend one way, the trough arrives, blades bend the other way. Reported from
//! play as *"waves of water oscillating back and forth"*, and that is exactly
//! what it was — a sine is symmetric about zero, so a blade spends half its
//! life leaning INTO the wind. Air does not do that. **Wind blows one way and
//! varies in how hard**, and a blade it lets go of returns to upright rather
//! than swinging past it.
//!
//! So the field is now a single **downwind lean between 0 and 1** — never
//! negative, never upwind — and everything interesting is in how that number
//! varies. Three requirements, all of them from the deception rather than from
//! meteorology, and each with a test:
//!
//!   * **Fine in SPACE.** Neighbouring patches must catch different amounts of
//!     the same gust, or the field is one big object leaning together and a
//!     soldier out of step with it is *more* conspicuous, not less. The fine
//!     term runs at [`FINE_CELL`] — about two clump-widths — so two pawns a few
//!     paces apart are never in the same part of it.
//!   * **Slow in TIME.** Wind shifts strength over seconds, not frames. It is
//!     also what makes the effect do its job: motion that matches the speed of
//!     a crawl is motion a crawl hides in, and a fast flutter would read as a
//!     different KIND of movement from a soldier and mask nothing.
//!   * **The time variation must be independent of the spatial pattern.** Pure
//!     advection — a frozen field carried downwind — fails this: when the gust
//!     over you dies is then completely predictable from what is upwind, and
//!     what is predictable is not cover. Hence [`noise3`]: the field EVOLVES on
//!     a clock of its own as well as travelling.
//!
//! **Render-only, like `vision.rs`, `nameplate.rs` and `sound.rs` — and this
//! one has to be said out loud, because it looks like a game mechanic.** The
//! sim never hears about the wind: concealment, `visible_fraction`, the fog
//! tiles and every bot's decision are computed off `Scenario::depth` exactly as
//! before, and a bush's concealment circle stays where the sim put it while its
//! sprite leans a few pixels off it. The masking is done to the PLAYER'S EYE
//! and to nothing else. That is deliberate — a wind that changed what could be
//! seen would have to be sim state, which means integers, rollback registration
//! and a checksum, for a decoration.
//!
//! # Where the field is evaluated
//!
//! Twice, and it has to be. The tufts are baked into static meshes — thousands
//! of quads that never change, which is the whole reason they are a mesh and
//! not sprites — so the only place they can be made to lean is the vertex
//! shader. Bushes are sprites, which the CPU moves. So [`lean`] is written here
//! in Rust and transcribed into `client/assets/grass.wgsl`, the same split as
//! `sound.rs`'s `bell` and `sound.wgsl`'s, and for the same reason: there is no
//! way to share code across the shader boundary.
//!
//! What is NOT left to a careful reader is the numbers. Every constant the two
//! share appears in the shader by the same name, and
//! [`the_shader_runs_the_same_wind`](tests::the_shader_runs_the_same_wind)
//! reads `grass.wgsl` and holds each one against the Rust value — so the two
//! copies can only ever drift in shape, never in tuning. They have to agree:
//! a bush leaning east while the grass around its feet leans west does not read
//! as wind, it reads as a bug.
//!
//! The one thing that is NOT transcribed is which way the wind is blowing.
//! [`bearing`] wanders with the clock and nothing else, so the CPU works it out
//! once a frame and hands the shader the angle in `params.w` — one number,
//! computed in one place, and [`VEER_SWING`] never has to exist in two files.

use bevy::prelude::*;

use army_ghosts_sim::{Bush, Pos, Scenario};

use crate::grass::{GrassMaterial, GRASS_VERT};

/// The PREVAILING bearing, as a unit vector in world space — east and a little
/// south. Two jobs: it is the direction gusts travel along, and it is the
/// centre of the sector [`bearing`] wanders in. Deliberately not axis-aligned,
/// because a gust travelling due east arrives at every point on a north-south
/// line at the same instant and the field picks up a grain the eye finds.
const WIND_DIR_X: f32 = 0.9553;
const WIND_DIR_Y: f32 = -0.2955;

/// How far the bearing wanders either side of prevailing, in radians, and how
/// long a full wander takes. Wind holds a direction and then comes round; it
/// does not hold one forever and it does not box the compass. ±40 degrees over
/// three quarters of a minute is a shift you notice between one firefight and
/// the next and never mid-burst.
///
/// Rust-only: the bearing reaches the shader as an angle in `params.w`, so
/// these two never have to be transcribed.
const VEER_SWING: f32 = 0.70;
const VEER_PERIOD: f32 = 43.0;

/// **The coarse term: is it blowing here just now.** How big a region freshens
/// and lulls together, world px, and how long it takes to do it, seconds.
///
/// This is where "slow in time" lives. Eleven seconds is long enough that a
/// player crossing a patch of grass experiences it as a condition of the
/// ground rather than as a flicker, and short enough that waiting out a lull
/// is a real option rather than a wish.
const GUST_CELL: f32 = 260.0;
const GUST_PERIOD: f32 = 11.0;
/// Where that term has to reach before a region is being blown at full
/// strength, and where it stops being blown at all. **This is the knob for HOW
/// OFTEN it is blowing**, and it has now been wrong in both directions.
///
/// Value noise is bunched around its middle — over the arena [`noise3`] runs
/// p25 0.335, median 0.451, p75 0.576 — so a window picked to look reasonable
/// lands off where the numbers are. The first (0.34..0.72) left **91% of the
/// ground at the floor**; the second sat on the quartiles (0.33..0.60), which
/// is tidy and still left the map 47% calm and a typical patch of grass
/// leaning only 40% of the time. Reported from play as the grass needing to
/// blow *more, and more often*, which is exactly what those two numbers say.
///
/// Sitting the window near p05..p50 gives 14% calm, a patch leaning 76% of the
/// time, and — the part that is not obvious — **nearly twice the total motion**
/// (mean travel over 100 s: 10.0 before, 18.7 now). That last is why the fix
/// is here and not in [`GUST_FLOOR`]: raising the floor also leans the grass
/// more often, but a lean that is always there is an offset, not movement, and
/// the whole point is what MOVES. Measured every way round — lowering the
/// window while keeping [`FINE_SHARE`] high beats every combination that
/// bought the same activity by flattening the fine term.
const GUST_LO: f32 = 0.15;
const GUST_HI: f32 = 0.44;

/// **The fine term: which blades are catching it.** This is where "fine in
/// space" lives — about two clump-widths across, so a boot's worth of ground
/// either side of you is in a different part of it, and a patch of grass
/// dapples instead of tilting as a slab.
const FINE_CELL: f32 = 52.0;
/// How long a piece of that dapple lasts where it stands, seconds. Faster than
/// the gusts — this is the stirring inside them — and still several times
/// slower than the sine it replaced, which put a blade through a whole swing
/// about every one and a half seconds.
///
/// This and [`FINE_DRIFT`] together set how quickly a given clump changes, and
/// **that is a measured number rather than an intention**: the peak rate of
/// change is about 1.0 of lean per second, which is a deep clump's tip crossing
/// its own nine pixels in a second. A stir, not a flick.
const FINE_PERIOD: f32 = 2.4;
/// How fast the dapple travels downwind, world px/s. What it buys is the one
/// thing the old travelling wave got right: structure that MOVES across the
/// field, so the wind has a direction you can read off the ground and not only
/// off which way the blades point.
const FINE_DRIFT: f32 = 55.0;
/// How much of the lean the fine term owns. High, because it is the term doing
/// the camouflage — the coarse one only says whether there is anything to
/// dapple — and **because it is where the motion is**. Making the grass blow
/// more often can be bought either here or at [`GUST_LO`], and only one of the
/// two is free: lowering this raises the average lean while cutting how far
/// anything travels, which is more grass bent over and less grass moving.
/// Measured, at matched activity, 0.84 carries about 40% more travel than 0.66.
const FINE_SHARE: f32 = 0.84;

/// The lightest the air ever gets, as a share of a full lean. Not zero: dead
/// still grass beside grass that is moving reads as the dead patch being
/// switched off, and — the point of the whole file — a patch with no motion in
/// it is a patch a crawl shows up against perfectly.
///
/// **The window above it was measured rather than chosen, and that mattered
/// once already.** Value noise is bunched around its middle, so a window
/// picked to look reasonable sits off where the numbers actually are and the
/// wind never reaches either end. Print the distribution before moving
/// [`GUST_LO`]/[`GUST_HI`]; the tests state what the field has to DO and will
/// not tell you which knob to turn.
const GUST_FLOOR: f32 = 0.05;

/// The noise lattice repeats every this many cells, in both space and in the
/// slice clock. **This is not decoration, it is what lets the wind run for
/// hours.** The fine term is sampled at a point that travels downwind forever,
/// and its hash multiplies coordinates by ~123 before taking a fractional
/// part: at an hour's drift the product needs 19 bits of mantissa and there
/// are only 24, so the hash quietly collapses onto a few dozen values and the
/// grass goes banded. Wrapping the lattice makes the field periodic, so the
/// sample point can be folded back into a bounded box with no seam — 256 cells
/// is 13,300 px of fine detail and 66,000 px of gust, both far larger than an
/// 800x600 arena, so the repeat is never in shot.
const LATTICE: f32 = 256.0;
/// How far apart consecutive slices of [`noise3`] are sampled. Large and
/// irregular, so one slice tells you nothing about the next; two axes rather
/// than one so the slices do not march along a diagonal.
const SLICE_X: f32 = 113.7;
const SLICE_Y: f32 = 271.3;

/// How far the tip of a clump travels between dead calm and a full gust, as a
/// fraction of the clump's OWN drawn height. A fraction rather than a number of
/// pixels because short grass and deep grass are the same plant: a fixed pixel
/// swing would have ankle-high tufts flailing and waist-high ones twitching.
///
/// It grew twice, for different reasons. First from 0.11 to 0.17 as
/// arithmetic rather than judgement — a signed wave swung a tip through TWICE
/// its amplitude and a one-signed lean only travels it once. Then it was
/// doubled again after a look at it, which is the only thing that could have
/// settled it: a deep clump's tip now crosses about eighteen pixels between
/// dead calm and a full gust.
///
/// **There is a ceiling on this and it is not far above.** A tip that travelled
/// its own height would be a blade lying flat, and one that travelled much past
/// half of it stops reading as a bend and starts reading as the clump being
/// dragged along the ground — the tuft sprite is a rigid quad and only its top
/// edge moves, so the illusion is a shear and shears give out. The `const`
/// block below is where the next person finds that out at build time.
pub const TUFT_SWAY_FRAC: f32 = 0.34;

const _: () = assert!(
    TUFT_SWAY_FRAC < 0.5,
    "a clump's tip would travel more than half its own height — it reads as \
     falling over rather than bending"
);
/// How far a clump's sample point is offset from its neighbour's, in fine
/// cells. Even at [`FINE_CELL`] two clumps four units apart sit almost on top
/// of each other in the noise; this is what keeps a stand of grass from moving
/// as one piece at the very finest scale. Small — at a whole cell it is not
/// grain any more, it is a second field.
pub const TUFT_GRAIN: f32 = 0.30;

/// How far a bush of [`BUSH_REF_PX`] on screen leans at a full gust, world px.
/// Bushes get a whole-sprite lean rather than the bend the grass gets: their
/// canopy is a modelled 3/4 view and its root sits INSIDE the canopy circle
/// (`gen_assets.py`: the ground line is only 0.22 of a frame below the frame's
/// centre), so pivoting about the root moves the visible bulk by well under a
/// pixel. Leaning the whole sprite is what actually reads.
///
/// **The cost of that choice, restated now that it is twice what it was.**
/// The lean is proportional to the sprite, and the sprite is proportional to
/// the canopy (`cover_size`: the frame is `96/30` of the radius, and the
/// canopy fills 30 of those 96 px) — so this number says, exactly, that **a
/// bush leans by half its own canopy radius at a full gust**, whatever size it
/// is. It was a quarter before.
///
/// Half a radius is the most that can be spent here and it is spent
/// deliberately. The sim's concealment circle does not move, so a bush at full
/// lean is that far out of register with the cover it provides; and because a
/// sprite cannot shear, the whole canopy travels rather than just its top,
/// which past this starts to read as a bush sliding over the ground instead of
/// leaning on it. The alternative — moving the circle — would be putting wind
/// in the sim, which the module note explains is not on offer.
const BUSH_SWAY_PX: f32 = 10.0;
/// The screen size a [`BUSH_SWAY_PX`] lean is quoted for; bushes scale off it,
/// so a big bush moves further than a small one while both lean the same
/// amount.
const BUSH_REF_PX: f32 = 64.0;
/// A bush is an individual rather than one clump among thousands, so it gets a
/// wider grain than the grass does — a thicket where every bush nods together
/// looks hinged.
const BUSH_GRAIN: f32 = 0.55;

/// How far the ground sward's own texture slides between calm and a full gust,
/// in world px. Tiny, and it is not trying to be blades: the sward is the
/// thatch seen from almost straight down, and a couple of pixels of drift under
/// the tufts is what keeps bare-ish ground from being a still backdrop the
/// tufts move against. Applied to the tiled ground only — the tuft sheet's UVs
/// are an ATLAS, and sliding those samples the neighbouring frame.
pub const SWARD_RIPPLE_PX: f32 = 6.0;

/// `x - floor(x)`, which is what WGSL's `fract` means. Rust's `f32::fract`
/// keeps the sign (`(-0.3).fract() == -0.3`), so using it here would put the
/// two copies of the hash on different lattices for every negative coordinate
/// — which is half the arena.
fn fract(x: f32) -> f32 {
    x - x.floor()
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Fold a lattice coordinate into `0..LATTICE`. See [`LATTICE`].
fn wrap(x: f32) -> f32 {
    x - LATTICE * (x / LATTICE).floor()
}

/// The scrambler under the noise. Same shape as `sound.wgsl`'s, in floats
/// because nothing here is simulated and nothing here has to match another
/// machine — two peers seeing slightly different blades of grass lean is not a
/// desync, it is not even a disagreement.
fn hash21(p: Vec2) -> f32 {
    let mut q = Vec2::new(fract(p.x * 123.34), fract(p.y * 345.45));
    q += Vec2::splat(q.dot(q + Vec2::splat(34.345)));
    fract(q.x * q.y)
}

/// Value noise: hashed lattice, smoothstepped between, repeating every
/// [`LATTICE`] cells.
fn noise(p: Vec2) -> f32 {
    let i = p.floor();
    let f = p - i;
    let u = f * f * (Vec2::splat(3.0) - 2.0 * f);
    let at = |o: Vec2| {
        let c = i + o;
        hash21(Vec2::new(wrap(c.x), wrap(c.y)))
    };
    let (a, b) = (at(Vec2::ZERO), at(Vec2::X));
    let (c, d) = (at(Vec2::Y), at(Vec2::ONE));
    let top = a + (b - a) * u.x;
    let bottom = c + (d - c) * u.x;
    top + (bottom - top) * u.y
}

/// **Value noise in three dimensions**, as two 2D slices eased between — the
/// third axis being time.
///
/// This is the piece that makes the wind unpredictable rather than merely
/// moving. A field that only ADVECTED would be frozen turbulence: the gust
/// arriving over you in two seconds is already sitting on the ground upwind,
/// so when your cover dies is a thing a player could read off the map. Giving
/// the field its own clock means a gust can fade where it stands and another
/// can get up out of nothing, which is what makes a lull worth waiting for
/// instead of worth calculating.
///
/// Slices are indexed modulo [`LATTICE`] and the wrap is seamless, because
/// slice `LATTICE` is slice `0` — see the note there for why anything is
/// wrapped at all.
fn noise3(p: Vec2, t: f32) -> f32 {
    let i = t.floor();
    let f = t - i;
    let u = f * f * (3.0 - 2.0 * f);
    let slice = |n: f32| {
        let k = wrap(n);
        noise(p + Vec2::new(SLICE_X, SLICE_Y) * k)
    };
    let (a, b) = (slice(i), slice(i + 1.0));
    a + (b - a) * u
}

/// Which way the wind is blowing at time `t`, in radians. A slow wander about
/// the prevailing bearing — it holds a direction, comes round, and holds
/// another. A function of the clock alone, so it is the same everywhere on the
/// map at once: strength is what varies from place to place, direction is what
/// the weather is doing.
pub fn bearing(t: f32) -> f32 {
    let base = WIND_DIR_Y.atan2(WIND_DIR_X);
    // 1D noise off the 2D one, sampled along a line well off the lattice
    // corners so it never lands on a run of identical hashes.
    let wander = noise(Vec2::new(t / VEER_PERIOD, 0.37)) * 2.0 - 1.0;
    base + VEER_SWING * wander
}

/// Which way a leaning thing leans on screen at time `t`: downwind, with the y
/// component foreshortened by the same projection the soldier, bush and grass
/// sheets are all modelled at. A blade blown north travels less far up the
/// screen than one blown east travels across it, for the same reason a blade of
/// a given height only rises [`GRASS_VERT`] of it.
pub fn lean_dir(t: f32) -> Vec2 {
    let b = bearing(t);
    Vec2::new(b.cos(), b.sin() * GRASS_VERT)
}

/// **The wind field.** How far downwind whatever stands at `p` is leaning at
/// time `t` — **0 for upright, 1 for flat out, and never negative**, because
/// air does not push things upwind. `grain` is that object's own offset into
/// the fine term, in cells, so two things side by side catch the same gust
/// differently instead of moving as one piece.
///
/// Two terms, and they answer different questions:
///
///   * **Is it blowing here just now** — a slow, broad field ([`GUST_CELL`]
///     over [`GUST_PERIOD`]). This one lulls.
///   * **Which blades are catching it** — a fine, quicker field
///     ([`FINE_CELL`] over [`FINE_PERIOD`]) that also travels downwind, so the
///     dapple crosses the ground.
///
/// Multiplied rather than added, because "there is wind here" and "this blade
/// is in it" are conditions that both have to hold; added, a lull would merely
/// be a weaker gust and the ground would never go quiet — and quiet ground is
/// the only kind a crawl shows up against.
pub fn lean(p: Vec2, t: f32, grain: f32) -> f32 {
    let prevailing = Vec2::new(WIND_DIR_X, WIND_DIR_Y);
    let gusting = smoothstep(GUST_LO, GUST_HI, noise3(p / GUST_CELL, t / GUST_PERIOD));
    // The drift is folded into one lattice period before it is used, which is
    // exactly seamless because the field repeats — see `LATTICE`.
    let travelled = fract(FINE_DRIFT * t / (LATTICE * FINE_CELL)) * LATTICE;
    let dapple = noise3(
        p / FINE_CELL - prevailing * travelled + Vec2::new(grain, -grain),
        t / FINE_PERIOD,
    );
    let caught = 1.0 - FINE_SHARE + FINE_SHARE * dapple;
    GUST_FLOOR + (1.0 - GUST_FLOOR) * gusting * caught
}

/// A bush's own offset into the fine term, from the seed it already wears its
/// variant and tint from.
pub fn grain_of(seed: u32) -> f32 {
    ((seed / 4096 % 1024) as f32 / 1024.0 - 0.5) * 2.0 * BUSH_GRAIN
}

/// Hand the shader the two numbers it cannot work out for itself: the clock,
/// and which way the wind is blowing. Everything else about the field is a pure
/// function of world position and those, so thousands of baked tuft quads lean
/// without anything being respawned, re-extracted or re-uploaded.
///
/// **The measuring rig gets a dead calm** (`Scenario::GrassStrip`). It is an
/// instrument — `tools/grass-shots.sh` photographs it and captions each frame
/// with the alpha `tools/grass-table.sh` measured for that exact pairing — and
/// a rig whose blades sit somewhere different in every screenshot is one whose
/// pictures cannot be compared with the last set. Freezing it costs nothing:
/// the wind is visible in the arena and in `?scenario=gunfire`, which is where
/// anybody would look at it.
pub fn drive_grass(
    time: Res<Time>,
    scenario: Res<Scenario>,
    mut materials: ResMut<Assets<GrassMaterial>>,
) {
    let t = match *scenario {
        Scenario::GrassStrip { .. } => 0.0,
        _ => time.elapsed_secs(),
    };
    let angle = bearing(t);
    for (_, material) in materials.iter_mut() {
        material.params.z = t;
        material.params.w = angle;
    }
}

/// Lean every bush with the same field the grass leans with.
///
/// Runs AFTER `render::sync_transforms`, which rewrites every `Pos`-carrying
/// entity's translation from the sim each frame — so this adds an offset to a
/// value that was just laid down fresh, and there is nothing here that can
/// accumulate however many frames go by.
pub fn sway_bushes(
    time: Res<Time>,
    scenario: Res<Scenario>,
    mut bushes: Query<(&Bush, &Pos, &Sprite, &mut Transform)>,
) {
    if matches!(*scenario, Scenario::GrassStrip { .. }) {
        return;
    }
    let (t, dir) = (time.elapsed_secs(), lean_dir(time.elapsed_secs()));
    for (bush, pos, sprite, mut transform) in &mut bushes {
        let (x, y) = pos.to_f32();
        let size = sprite.custom_size.map(|s| s.y).unwrap_or(BUSH_REF_PX);
        let swing = lean(Vec2::new(x, y), t, grain_of(bush.seed))
            * BUSH_SWAY_PX
            * (size / BUSH_REF_PX);
        transform.translation.x += dir.x * swing;
        transform.translation.y += dir.y * swing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant the shader has a copy of, and what it is called there.
    /// Kept as a list rather than as one assertion apiece so adding a knob to
    /// the field is one line in two files and a compile error if it is one.
    ///
    /// The veer constants are deliberately absent: the bearing is computed on
    /// the CPU and handed over in `params.w`, so they exist once.
    const SHARED: [(&str, f32); 15] = [
        ("WIND_DIR_X", WIND_DIR_X),
        ("WIND_DIR_Y", WIND_DIR_Y),
        ("GUST_CELL", GUST_CELL),
        ("GUST_PERIOD", GUST_PERIOD),
        ("GUST_LO", GUST_LO),
        ("GUST_HI", GUST_HI),
        ("FINE_CELL", FINE_CELL),
        ("FINE_PERIOD", FINE_PERIOD),
        ("FINE_DRIFT", FINE_DRIFT),
        ("FINE_SHARE", FINE_SHARE),
        ("GUST_FLOOR", GUST_FLOOR),
        ("LATTICE", LATTICE),
        ("SLICE_X", SLICE_X),
        ("SLICE_Y", SLICE_Y),
        ("GRASS_VERT", GRASS_VERT),
    ];

    fn wgsl_const(name: &str) -> f32 {
        const SRC: &str = include_str!("../assets/grass.wgsl");
        let needle = format!("const {name}: f32 = ");
        let line = SRC
            .lines()
            .map(str::trim_start)
            .find(|line| line.starts_with(&needle))
            .unwrap_or_else(|| panic!("grass.wgsl declares no `{name}`"));
        let value = line[needle.len()..].split(';').next().unwrap().trim();
        value
            .parse()
            .unwrap_or_else(|_| panic!("grass.wgsl's `{name}` is not a number: {value:?}"))
    }

    /// A spread of places and times to sample the field over, so no test is
    /// reading one lucky corner of it.
    fn sample_grid(t: f32) -> Vec<(Vec2, f32)> {
        let mut out = Vec::new();
        let mut y = -300;
        while y <= 300 {
            let mut x = -400;
            while x <= 400 {
                let p = Vec2::new(x as f32, y as f32);
                out.push((p, lean(p, t, 0.0)));
                x += 12;
            }
            y += 12;
        }
        out
    }

    /// The grass leans in the vertex shader and the bushes lean on the CPU, so
    /// the field exists twice. The SHAPE has to be kept in step by reading both
    /// (there is no sharing code across the shader boundary, exactly as with
    /// `sound.rs`'s bell); the NUMBERS do not, and this is why.
    #[test]
    fn the_shader_runs_the_same_wind() {
        for (name, ours) in SHARED {
            assert_eq!(
                wgsl_const(name),
                ours,
                "grass.wgsl and wind.rs disagree about {name}"
            );
        }
    }

    /// **The whole correction, in one assertion.** The field this replaced was
    /// a travelling sine, so a blade spent half its life leaning INTO the wind
    /// — reported from play as "waves of water oscillating back and forth",
    /// which is precisely what a symmetric wave looks like from above. Air
    /// blows one way and varies in strength; the lean is therefore one-signed,
    /// and a blade let go of returns to upright rather than swinging past it.
    #[test]
    fn nothing_ever_leans_into_the_wind() {
        let mut worst: f32 = 1.0;
        let mut hardest: f32 = 0.0;
        for step in 0..400 {
            for (_, l) in sample_grid(step as f32 * 0.7) {
                worst = worst.min(l);
                hardest = hardest.max(l);
            }
        }
        println!("lean over the whole arena and 280 s: {worst:.3}..{hardest:.3}");
        assert!(worst >= 0.0, "something leaned upwind: {worst}");
        assert!(worst <= GUST_FLOOR + 0.01, "the grass never comes back up: {worst}");
        assert!(hardest > 0.9, "the wind never gets near full strength: {hardest}");
        assert!(hardest <= 1.0 + 1e-4, "the lean overshot: {hardest}");
    }

    /// Fine in space: a pace away is a different amount of wind, and a boot's
    /// width away is not. Both halves matter — the first is the requirement,
    /// the second is what stops it being satisfied by white noise, which would
    /// not read as wind at all.
    #[test]
    fn a_pace_away_catches_a_different_gust() {
        let (mut near, mut far, mut n) = (0.0, 0.0, 0);
        for step in 0..120 {
            let t = step as f32 * 0.9;
            for (p, l) in sample_grid(t) {
                near += (lean(p + Vec2::new(6.0, 0.0), t, 0.0) - l).abs();
                far += (lean(p + Vec2::new(45.0, 0.0), t, 0.0) - l).abs();
                n += 1;
            }
        }
        let (near, far) = (near / n as f32, far / n as f32);
        println!("mean |difference|: 6 units apart {near:.3}, 45 units apart {far:.3}");
        assert!(far > 0.06, "the field is flat across a pace: {far}");
        assert!(near < far * 0.5, "the field is noise, not wind: {near} vs {far}");
    }

    /// Slow in time: the lean at a point moves at the pace of weather, not of
    /// frames. This is the other half of the correction — the old sine put a
    /// blade through a full swing more than once a second — and it is what
    /// makes the effect do its job, since motion at the speed of a crawl is
    /// what a crawl can hide in.
    #[test]
    fn the_wind_shifts_at_the_pace_of_weather() {
        let mut fastest: f32 = 0.0;
        let mut travels = Vec::new();
        for spot in 0..24 {
            let p = Vec2::new((spot % 6) as f32 * 130.0 - 325.0, (spot / 6) as f32 * 140.0 - 210.0);
            let mut travelled = 0.0;
            let mut previous = lean(p, 0.0, 0.0);
            for step in 1..=6000 {
                let t = step as f32 / 60.0; // 100 s at the frame rate
                let now = lean(p, t, 0.0);
                fastest = fastest.max((now - previous).abs() * 60.0);
                travelled += (now - previous).abs();
                previous = now;
            }
            travels.push(travelled);
        }
        travels.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = travels[travels.len() / 2];
        println!(
            "fastest change {fastest:.2}/s; travel over 100 s at 24 spots: \
             {:.1} / {median:.1} / {:.1}",
            travels[0],
            travels[travels.len() - 1]
        );
        // A tip at a full lean stands about 18 px from upright, so a change of
        // one per second is a tip crossing 18 px in that second: a stir, not a
        // flick. The old sine peaked far above this, which is what "waves of
        // water" was reporting.
        assert!(fastest < 1.3, "the wind snaps rather than shifting: {fastest}/s");
        // …and it is not frozen either: the typical piece of ground has to get
        // somewhere over a round's worth of time. Judged on the MEDIAN spot,
        // because a spot that spends the whole run inside a lull is a feature.
        assert!(median > 5.0, "the wind barely moves at all: {median}");
    }

    /// One piece of ground has gusts and lulls, so the motion never settles
    /// into background the eye stops seeing. Measured as the envelope over
    /// five-second windows at a fixed point.
    #[test]
    fn one_spot_gets_gusts_and_lulls() {
        let p = Vec2::new(210.0, -140.0);
        let mut envelopes = Vec::new();
        for window in 0..40 {
            let mut peak: f32 = 0.0;
            for step in 0..300 {
                let t = window as f32 * 5.0 + step as f32 / 60.0;
                peak = peak.max(lean(p, t, 0.0));
            }
            envelopes.push(peak);
        }
        let quiet = envelopes.iter().cloned().fold(f32::MAX, f32::min);
        let loud = envelopes.iter().cloned().fold(0.0_f32, f32::max);
        println!("envelope over 200 s at {p:?}: quietest {quiet:.2}, loudest {loud:.2}");
        assert!(loud > 0.7, "it never really blows here: {loud}");
        assert!(quiet < 0.4, "it never lets up here: {quiet}");
        let mid = envelopes.iter().filter(|&&e| (0.25..0.85).contains(&e)).count();
        assert!(mid >= 4, "the wind only has two settings: {envelopes:?}");
    }

    /// At any instant the map is being blown unevenly. A field that leaned as
    /// one sheet would make a soldier out of step with it MORE conspicuous, not
    /// less.
    #[test]
    fn the_wind_is_not_the_same_everywhere_at_once() {
        let mut calm_shares = Vec::new();
        for step in 0..40 {
            let t = step as f32 * 7.3;
            let grid = sample_grid(t);
            let lo = grid.iter().map(|&(_, l)| l).fold(f32::MAX, f32::min);
            let hi = grid.iter().map(|&(_, l)| l).fold(0.0_f32, f32::max);
            // Whatever else is true, at no instant does the map lean alike.
            assert!(hi - lo > 0.5, "the whole map leans alike at t={t}: {lo}..{hi}");
            calm_shares.push(grid.iter().filter(|&&(_, l)| l < 0.25).count() as f32
                / grid.len() as f32);
        }
        let quietest = calm_shares.iter().cloned().fold(0.0_f32, f32::max);
        let busiest = calm_shares.iter().cloned().fold(f32::MAX, f32::min);
        println!(
            "share of the arena nearly calm, over 280 s: {:.0}%..{:.0}%",
            100.0 * busiest,
            100.0 * quietest
        );
        // **The claim is about the map over TIME, not at every instant**, and
        // that is a correction: this first demanded calm ground in every
        // photograph, which is a stronger thing than the design ever wanted and
        // caps how hard the wind is allowed to blow at its peak. A lull is
        // something that HAPPENS. What has to be true is that both states are
        // reachable — a lull worth waiting out, and weather that really gets
        // up — because a field that is always half calm is as predictable as
        // one that never is.
        assert!(quietest > 0.20, "the wind never lets up anywhere: {quietest}");
        assert!(busiest < 0.10, "it never really blows across the map: {busiest}");
    }

    /// **A lull has to be able to happen where you are standing**, not only by
    /// the gust that was over you moving off downwind. Advect the whole field by
    /// exactly one drift and a purely-carried field would be unchanged to the
    /// last bit — so what survives here is `noise3`'s own clock. It matters for
    /// the same reason the rest of the file does: if quiet ground were a
    /// function of what is upwind of you, a player could read the wind and know
    /// exactly when the grass around him was about to stop covering him, and
    /// cover you can time is not cover.
    #[test]
    fn a_lull_is_not_just_a_gust_that_moved_on() {
        let prevailing = Vec2::new(WIND_DIR_X, WIND_DIR_Y);
        let mut worst: f32 = 0.0;
        for step in 0..600 {
            let (t, d) = (step as f32 * 0.41, 3.0);
            let p = Vec2::new((step % 41) as f32 * 20.0 - 400.0, (step % 31) as f32 * 20.0 - 300.0);
            let carried = lean(p + prevailing * (FINE_DRIFT * d), t + d, 0.0);
            worst = worst.max((carried - lean(p, t, 0.0)).abs());
        }
        println!("the field differs by up to {worst:.2} under pure advection");
        assert!(worst > 0.25, "the wind is only ever carried along: {worst}");
    }

    /// The bearing wanders and comes back, which is what "mostly one way,
    /// occasionally switching" means. Both bounds are the point: a wind that
    /// held one bearing forever is a texture, and one that boxed the compass is
    /// a washing machine.
    #[test]
    fn the_bearing_wanders_without_boxing_the_compass() {
        let base = WIND_DIR_Y.atan2(WIND_DIR_X);
        let angles: Vec<f32> = (0..4000).map(|s| bearing(s as f32 * 0.25)).collect();
        let off = angles.iter().map(|a| (a - base).abs()).fold(0.0_f32, f32::max);
        let spread = angles.iter().cloned().fold(f32::MIN, f32::max)
            - angles.iter().cloned().fold(f32::MAX, f32::min);
        println!("over 1000 s the bearing spans {:.0} degrees", spread.to_degrees());
        assert!(off <= VEER_SWING + 1e-4, "the wind left its sector: {off}");
        assert!(spread > VEER_SWING, "the wind never comes round: {spread}");
    }

    /// Neighbours catch the same gust differently. Without this a stand of
    /// grass moves as one piece at the finest scale — invisible in any test
    /// that samples one clump, and the first thing the eye picks up.
    #[test]
    fn a_grain_offset_takes_a_clump_out_of_step_with_its_neighbour() {
        let p = Vec2::new(40.0, -25.0);
        let mut apart: f32 = 0.0;
        for step in 0..900 {
            let t = step as f32 / 30.0;
            apart = apart.max((lean(p, t, 0.0) - lean(p, t, TUFT_GRAIN)).abs());
        }
        assert!(apart > 0.1, "a full grain barely moves a clump: {apart}");
    }

    /// A bush's grain comes off the seed it already wears its variant and tint
    /// from, and has to stay inside the offset it is quoted at.
    #[test]
    fn every_bush_gets_a_grain_and_they_are_not_all_the_same() {
        let grains: Vec<f32> = (0u32..64).map(|s| grain_of(s.wrapping_mul(0x9E37_79B9))).collect();
        for &grain in &grains {
            assert!(grain.abs() <= BUSH_GRAIN, "bush grain {grain} is out of range");
        }
        let spread = grains.iter().cloned().fold(f32::MIN, f32::max)
            - grains.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread > BUSH_GRAIN, "every bush nods together: spread {spread}");
    }

    /// The lattice wrap is what lets the field run for hours without the hash
    /// losing its precision (see [`LATTICE`]), and it is only safe if it is
    /// SEAMLESS — a visible jump every few minutes would be worse than the
    /// banding it prevents. Slice `LATTICE` must be slice `0`, and a point a
    /// whole lattice away must be the same point.
    #[test]
    fn the_field_repeats_without_a_seam() {
        for step in 0..200 {
            let p = Vec2::new((step % 17) as f32 * 23.0, (step % 13) as f32 * 31.0);
            let t = step as f32 * 0.31;
            assert!((noise3(p, t) - noise3(p, t + LATTICE)).abs() < 1e-4, "slices do not wrap");
            let shifted = p + Vec2::splat(LATTICE);
            assert!((noise(p) - noise(shifted)).abs() < 1e-4, "space does not wrap");
        }
    }
}


