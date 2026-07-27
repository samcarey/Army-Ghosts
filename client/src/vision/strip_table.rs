//! Two pawns facing each other across a one-hex wall of grass: how much of each
//! other can they see, in every stance, at every grass depth?
//!
//! This is the smallest scene that asks the question the grass model exists to
//! answer — *is a band of cover worth crossing a map for?* — and it is a scene
//! the procedural field doesn't contain: [`grass_height`] has no edges that line
//! up with the fog's hexes and no patch of a known depth, so measuring it there
//! would test [`GRASS_SEED`] rather than the model.
//!
//! It is the same scene the game builds for [`Scenario::GrassStrip`], from the
//! same constants — `tools/grass-shots.sh` photographs it, this tabulates it,
//! and the two are the same thing rather than two descriptions of one. The
//! layout, west to east:
//!
//! ```text
//!     ( west pawn )  ( empty )  ( GRASS )  ( empty )  ( east pawn )
//!          |<-- 2 * STRIP_STANDOFF, 96 units, centre to centre -->|
//! ```
//!
//! Everything else — the sight-line stepping, the extinction, the stance
//! heights — is the shipping code. There is no rock or bush in the scene, so
//! `fade_hidden`'s cover term is 1 and the alpha it writes on the east pawn's
//! sprite is exactly the number tabulated here.

use super::*;
use army_ghosts_sim::{GRASS_BARE_BELOW, GRASS_MAX_H, STANCE_PRONE, STRIP_HALF_W, STRIP_STANDOFF};

/// Depths the wall is filled with, world units. Not a linear sweep — the heights
/// that mean something: nothing, each stance's eye height in turn (grass hides
/// what is shorter than it, so those are the thresholds where behaviour
/// changes), and the two ends of the band the arena's own field lives in.
/// Must stay in ascending order — the table asserts that deeper grass hides
/// more by walking consecutive rows (and checks the ordering first, so a
/// reshuffle fails saying so rather than as a bogus monotonicity break).
const STRIP_DEPTHS: [(i32, &str); 6] = [
    (0, "bare"),
    (GRASS_BARE_BELOW, "shallowest grass"),
    (15, "prone height"),
    (32, "shin"),
    (GRASS_MAX_H, "field ceiling"),
    (64, "standing height"),
];

/// The depth the spec is written against: shin-deep grass must hide two prone
/// pawns from each other completely.
const SHIN: i32 = 32;
/// What "completely" means. A pawn at 4% alpha is a smudge you'd have to know
/// was there; nothing in the game draws attention to it (no outline, no
/// nameplate), so this is invisible in practice.
const INVISIBLE: f32 = 0.04;

const STANCES: [&str; 3] = ["standing", "crouching", "prone"];

/// How far either alpha may differ from its mirror image. Not zero: the sight
/// line is sampled from the eye outwards ([`GRASS_NEAR_T`] onwards), so walking
/// it west-to-east lands its samples in different places than east-to-west and
/// the wall's edges get caught a fraction of a step differently.
const MIRROR_TOL: f32 = 0.05;

fn west() -> Vec2 {
    Vec2::new(-STRIP_STANDOFF as f32, 0.0)
}

fn east() -> Vec2 {
    Vec2::new(STRIP_STANDOFF as f32, 0.0)
}

/// One measurement: the target's sprite alpha, and the two halves of the model
/// that produced it — how much of the target is behind grass at all, and how
/// many units of grass are in the way.
fn look(
    eye: Vec2,
    eye_level: usize,
    target: Vec2,
    target_level: usize,
    depth: i32,
) -> (f32, Block) {
    let h = |level: usize| STANCE_HEIGHT[level] as f32;
    let scenario = Scenario::GrassStrip { depth, east_stance: 0 };
    let block = grass_block(eye, h(eye_level), target, h(target_level), |x, y| {
        scenario.depth(x, y)
    });
    (1.0 - block.conceal(), block)
}

/// Walk the sight line's row and measure what the wall actually is: how many
/// units of it the line crosses, and the widest bare run either side. Checking
/// the geometry off the same function the renderer asks, rather than asserting
/// the constants that produced it.
fn measure_row(depth: i32) -> (i32, i32) {
    let scenario = Scenario::GrassStrip { depth: depth.max(1), east_stance: 0 };
    let (mut grass, mut gap, mut run) = (0, 0, 0);
    for x in (west().x as i32)..=(east().x as i32) {
        if scenario.depth(x, 0) > 0 {
            grass += 1;
            gap = gap.max(run);
            run = 0;
        } else {
            run += 1;
        }
    }
    (grass, gap.max(run))
}

/// The rig is a wall with clear ground either side; the arena is grass all the
/// way. That changes the answer completely — the length term saturates on any
/// real sight line — so the table above says what the model does and this says
/// what the game feels like. Read down a column for "how much closer do I have
/// to get", across a row for "what does dropping to a knee buy me".
fn arena_section() -> String {
    let h = |level: usize| STANCE_HEIGHT[level] as f32;
    let field = |x, y| Scenario::Arena.depth(x, y);
    // A lane straight out of player 1's spawn, along the arena's middle.
    let eye = Vec2::new(-150.0, 0.0);
    let ranges = [40.0, 80.0, 150.0, 300.0];

    let mut out = format!(
        "\nIn the ARENA, where a tile is either bare or {}..{} deep, rather than one\n\
         wall with clear ground either side — alpha of a target east of a viewer\n\
         at (-150, 0). What a line crosses is what it gets:\n\n\
         | viewer    | target    |",
        GRASS_BARE_BELOW, GRASS_MAX_H
    );
    for r in ranges {
        out.push_str(&format!(" {r:>5.0} units |"));
    }
    out.push_str("\n|-----------|-----------|");
    for _ in ranges {
        out.push_str("-------------|");
    }
    out.push('\n');
    for (v, viewer) in STANCES.iter().enumerate() {
        for (t, target) in STANCES.iter().enumerate() {
            out.push_str(&format!("| {viewer:<9} | {target:<9} |"));
            for r in ranges {
                let alpha = 1.0
                    - grass_block(eye, h(v), eye + Vec2::new(r, 0.0), h(t), field).conceal();
                out.push_str(&format!(" {alpha:>11.3} |"));
            }
            out.push('\n');
        }
    }
    out
}

/// Prints the concealment table, then asserts the properties it is supposed to
/// show. The printing comes first on purpose: when an assertion goes, the table
/// that explains why is already on screen.
#[test]
fn grass_strip_table() {
    let apart = east().x - west().x;
    let (strip_w, gap_w) = measure_row(GRASS_MAX_H);

    let mut out = String::new();
    out.push_str("<<<GRASS-TABLE\n");
    out.push_str(&format!(
        "Two pawns {apart:.0} units apart, one {strip_w}-unit wall of grass between them,\n\
         one clear hex ({gap_w} units) either side of it. No rocks or bushes, so each\n\
         number is the sprite alpha `fade_hidden` writes: 1.000 is plainly visible,\n\
         0.000 is invisible. Photograph it with tools/grass-shots.sh.\n\
         HEX_R={HEX_R}, GRASS_EXTINCTION={GRASS_EXTINCTION}, GRASS_SAMPLES={GRASS_SAMPLES},\n\
         STANCE_HEIGHT={STANCE_HEIGHT:?}.\n\n"
    ));
    out.push_str(
        "| grass depth        | west pawn | east pawn | east's alpha | west's alpha | east covered | east blocked |\n\
         |--------------------|-----------|-----------|--------------|--------------|--------------|--------------|\n",
    );

    // [depth][west stance][east stance]
    let mut seen_east = [[[0.0f32; 3]; 3]; STRIP_DEPTHS.len()];
    let mut seen_west = [[[0.0f32; 3]; 3]; STRIP_DEPTHS.len()];

    for (d, (depth, label)) in STRIP_DEPTHS.iter().enumerate() {
        for w in 0..3 {
            for e in 0..3 {
                let (east_alpha, block) = look(west(), w, east(), e, *depth);
                let (west_alpha, _) = look(east(), e, west(), w, *depth);
                seen_east[d][w][e] = east_alpha;
                seen_west[d][w][e] = west_alpha;
                out.push_str(&format!(
                    "| {:>2} {:<15} | {:<9} | {:<9} | {east_alpha:>12.3} | {west_alpha:>12.3} | {:>12.2} | {:>12.1} |\n",
                    depth, label, STANCES[w], STANCES[e], block.covered, block.length,
                ));
            }
        }
    }
    out.push_str(&arena_section());
    out.push_str(">>>GRASS-TABLE\n");
    println!("{out}");

    assert!(
        STRIP_DEPTHS.windows(2).all(|w| w[0].0 < w[1].0),
        "STRIP_DEPTHS must be ascending: {STRIP_DEPTHS:?}"
    );

    // ── The scene really is the scene ───────────────────────────────────────
    // Corner to corner across a flat-top hex: the wall is one fog tile wide,
    // and the pawns stand one clear tile off it. The sim owns those distances
    // (it has to — it spawns the pawns) and the fog's HEX_R lives here, so this
    // is where the two get checked against each other.
    let hex_w = (2.0 * HEX_R).round() as i32;
    assert_eq!(
        STRIP_HALF_W * 2,
        hex_w,
        "the wall must be one hex wide: STRIP_HALF_W vs HEX_R"
    );
    assert_eq!(
        STRIP_STANDOFF as f32,
        HEX_R * 3.0,
        "each pawn must stand two hex columns off the middle of the wall"
    );
    assert!(
        (strip_w - hex_w).abs() <= 1,
        "the grass must measure one hex across the sight line: {strip_w} vs {hex_w}"
    );
    assert!(
        gap_w >= hex_w,
        "each pawn must be a clear hex off the wall: widest bare run {gap_w}"
    );

    // ── What the table is supposed to show ──────────────────────────────────
    for w in 0..3 {
        for e in 0..3 {
            assert_eq!(
                (seen_east[0][w][e], seen_west[0][w][e]),
                (1.0, 1.0),
                "bare ground must hide nobody ({} vs {})",
                STANCES[w],
                STANCES[e]
            );
            // Deeper grass can only ever hide more.
            for d in 1..STRIP_DEPTHS.len() {
                assert!(
                    seen_east[d][w][e] <= seen_east[d - 1][w][e] + f32::EPSILON
                        && seen_west[d][w][e] <= seen_west[d - 1][w][e] + f32::EPSILON,
                    "{} units of grass hid less than {} ({} vs {})",
                    STRIP_DEPTHS[d].0,
                    STRIP_DEPTHS[d - 1].0,
                    STANCES[w],
                    STANCES[e]
                );
            }
            // Mirror image: what west sees of east must match what east sees of
            // west with the stances swapped, or the wall isn't centred.
            assert!(
                (seen_east[STRIP_DEPTHS.len() - 1][w][e] - seen_west[STRIP_DEPTHS.len() - 1][e][w])
                    .abs()
                    < MIRROR_TOL,
                "the wall must sit centred: {} vs {}",
                seen_east[STRIP_DEPTHS.len() - 1][w][e],
                seen_west[STRIP_DEPTHS.len() - 1][e][w]
            );
        }
    }

    let deep = &seen_east[STRIP_DEPTHS.len() - 1];
    for w in 0..3 {
        assert!(
            deep[w][2] <= deep[w][1] && deep[w][1] <= deep[w][0],
            "lower must never be easier to see, viewed from {}: {:?}",
            STANCES[w],
            deep[w]
        );
    }
    for e in 0..3 {
        assert!(
            deep[2][e] <= deep[0][e],
            "a prone viewer must not see more than a standing one, of a {}: {} vs {}",
            STANCES[e],
            deep[2][e],
            deep[0][e]
        );
    }
    // The spec this model was rebuilt for: two pawns lying either side of a
    // body's width of shin-deep grass cannot see each other. Both directions,
    // because concealment that only works one way is a bug that hides itself.
    let shin = STRIP_DEPTHS
        .iter()
        .position(|&(depth, _)| depth == SHIN)
        .expect("the shin case must stay in the table");
    let prone = STANCE_PRONE as usize;
    assert!(
        seen_east[shin][prone][prone] < INVISIBLE && seen_west[shin][prone][prone] < INVISIBLE,
        "two prone pawns must not see each other through {SHIN}-deep grass: {} / {}",
        seen_east[shin][prone][prone],
        seen_west[shin][prone][prone]
    );
    // ...and the rest of the ladder holds around it: standing in the same grass
    // is a fight, not a hiding place, and crouching is in between. Without this
    // the assertion above is satisfied by simply hiding everyone always.
    assert!(
        seen_east[shin][0][0] > 0.7,
        "shin-deep grass must not hide a standing pawn from a standing one: {}",
        seen_east[shin][0][0]
    );
    assert!(
        seen_east[shin][0][1] < seen_east[shin][0][0],
        "crouching must sit between standing and prone: {:?}",
        seen_east[shin][0]
    );
}
