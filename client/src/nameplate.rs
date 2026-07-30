//! Teammate nameplates: a small green name over everyone on your side, and an
//! arrow on the edge of the screen for the ones who are off it.
//!
//! # Render-only, like spectating
//!
//! Nothing here touches the sim and nothing here may. Which pawns are *yours* is
//! a fact about who is holding this phone, and the sim has no point of view — it
//! simulates every pawn on every peer. So this sits on the same line
//! `client/src/vision.rs` and `client/src/spectate.rs` do: it reads the
//! rolled-back world and writes only pixels.
//!
//! # Why the names are green rather than the team tint
//!
//! `render::TEAM_COLORS` says which side a figure is on; a nameplate says
//! *friend*, which is a different message and only ever appears for friends. On
//! the TAN side a tan name over a tan soldier would be the one case where the
//! label is least readable and most needed, so the colour here is fixed and
//! bright rather than borrowed from the team.
//!
//! # Concealment does not hide a nameplate
//!
//! `vision::fade_hidden` fades a pawn the grass is hiding, and a plate over a
//! faded teammate stays fully lit on purpose. Concealment is about what the enemy
//! can find; knowing where your own side is IS the point of this, and nobody
//! else's screen is drawing it.

use bevy::prelude::*;
use bevy_ggrs::{LocalPlayers, Session};

use army_ghosts_sim::{Bot, Health, Player, Pos, Scenario, Team, MAX_PLAYERS};

use crate::hud::pawn_name;
use crate::SessionConfig;

/// One plate out of the pool, and the two children it writes through.
///
/// The children are held by `Entity` rather than looked up through `Children`
/// because there are exactly two of them and they never change — a lookup would
/// be re-deriving a fact this component can just state.
#[derive(Component)]
pub struct Nameplate {
    arrow: Entity,
    label: Entity,
}

/// The rotating chevron. A fixed-size box with the glyph centred inside it, so
/// spinning it never changes the plate's layout: `UiTransform` is a visual
/// transform and the unrotated box is what the row measures.
#[derive(Component)]
pub struct NameplateArrow;

#[derive(Component)]
pub struct NameplateLabel;

/// What an edge plate has to keep out of the way of: every button, plus the three
/// HUD readouts that aren't buttons (the round line, the health bar, the roster).
/// Written as one query type because it is one idea — "the parts of the screen
/// that already belong to something".
type HudBox = (
    &'static ComputedNode,
    &'static UiGlobalTransform,
    &'static InheritedVisibility,
);

type HudBoxFilter = Or<(
    With<Button>,
    With<crate::hud::RoundText>,
    With<crate::hud::HealthBar>,
    With<crate::hud::PlayerListText>,
)>;

/// Friendly green. Bright and slightly desaturated so it reads over both the
/// dark olive sward and the pale dry earth of a bare tile.
const NAME_COLOR: Color = Color::srgba(0.56, 0.95, 0.50, 0.92);
const NAME_SIZE: f32 = 11.0;
const ARROW_SIZE: f32 = 13.0;
/// The chevron's layout box. Square, so the glyph has room to point any way.
const ARROW_BOX: f32 = 13.0;

/// How far above a pawn's `Pos` the plate is anchored, world units. The sprite's
/// feet stand on `Pos` and its head reaches about 50 units up (`SOLDIER_SIZE`
/// times `STANCE_ANCHOR`), so this clears a standing figure. Deliberately NOT
/// per-stance: a name that dropped as its owner went prone would read as the
/// label falling off rather than as the soldier lying down, and a constant height
/// also keeps four teammates' names on one line when they are level with each
/// other.
const NAME_LIFT: f32 = 62.0;

/// How far inside the viewport an edge plate sits, logical px — a margin, not an
/// inset, and the same on all four sides. It can be this thin because an edge
/// plate is anchored BY ITS EDGE (see `Placement::pivot`) rather than by its
/// centre, so nothing hangs off the screen, and because plates step around the
/// HUD boxes rather than keeping a respectful distance from where they might be.
const EDGE_MARGIN: f32 = 2.0;

/// About what a plate measures, logical px: the widest name plus its chevron, and
/// one line high. Used for keeping plates off the HUD and off each other, both of
/// which only need to know roughly how much room one takes up.
const PLATE_W: f32 = 76.0;
const PLATE_H: f32 = 15.0;
/// Clearance left when a plate steps around a piece of HUD.
const DODGE_GAP: f32 = 2.0;

/// Where a plate goes and, when its pawn is off the screen, which way its arrow
/// points (radians, clockwise from screen-right — the sense `UiTransform` uses).
///
/// `pivot` is where `at` sits ON the plate, in units of the plate's own size:
/// `(-0.5, -0.5)` is centred, which is what a name over a pawn's head wants, and
/// `0` / `-1` on an axis puts the plate wholly to one side of its anchor. That is
/// what lets an edge plate hug the edge — pinned to the left, it is anchored by
/// its own LEFT edge and grows inward, so the margin only has to clear the glass
/// rather than half a name.
#[derive(Debug, PartialEq)]
struct Placement {
    at: Vec2,
    arrow: Option<f32>,
    pivot: Vec2,
}

pub fn setup_nameplates(mut commands: Commands) {
    // A fixed pool, allocated once. A side can hold at most `TEAM_SIZE` pawns so
    // `MAX_PLAYERS` is slack, and the slack costs one hidden node each —
    // cheaper than spawning and despawning UI as teammates die.
    for _ in 0..MAX_PLAYERS {
        let arrow = commands
            .spawn((
                NameplateArrow,
                Node {
                    width: Val::Px(ARROW_BOX),
                    height: Val::Px(ARROW_BOX),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    // Out of the layout entirely while the pawn is on screen:
                    // `Visibility::Hidden` would keep the box, which would shove
                    // the name off its own anchor.
                    display: Display::None,
                    ..default()
                },
                UiTransform::IDENTITY,
            ))
            .with_children(|box_| {
                // ASCII, like every other glyph in this UI: the embedded default
                // font is missing most of what a nicer arrow would need.
                box_.spawn((
                    Text::new(">"),
                    TextFont { font_size: ARROW_SIZE, ..default() },
                    TextColor(NAME_COLOR),
                ));
            })
            .id();
        let label = commands
            .spawn((
                NameplateLabel,
                Text::new(""),
                TextFont { font_size: NAME_SIZE, ..default() },
                TextColor(NAME_COLOR),
            ))
            .id();
        commands
            .spawn((
                Nameplate { arrow, label },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(3.0),
                    padding: UiRect::axes(Val::Px(4.0), Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                // A dark pill, like the rest of this HUD wears. Not decoration: a
                // plate crosses grass, bare earth and — where a teammate stands
                // under the roster — the roster's own text, and green on green was
                // unreadable in the last of those. Faint enough to read as a
                // shadow under the name rather than as a button.
                BackgroundColor(Color::srgba(0.04, 0.06, 0.03, 0.55)),
                // Percent translation resolves against the node's OWN size, which
                // is the whole reason this is a `UiTransform` and not just
                // `left`/`top`: it centres a plate of unknown width on its
                // anchor point without anybody measuring the text.
                UiTransform::from_translation(Val2::percent(-50.0, -50.0)),
                Visibility::Hidden,
            ))
            .add_children(&[arrow, label]);
    }
}

/// Name every living teammate, on screen or off it.
///
/// Must run AFTER `render::camera_follow`, and it reads the camera's `Transform`
/// rather than its `GlobalTransform` for exactly that reason: propagation
/// happens in `PostUpdate`, so the global one is still last frame's and every
/// name would lag the camera by a frame — which reads as the labels sliding
/// about while you walk. The camera has no parent, so the local transform IS the
/// global one.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn update_nameplates(
    scenario: Res<Scenario>,
    local: Option<Res<LocalPlayers>>,
    session: Option<Res<Session<SessionConfig>>>,
    cameras: Query<(&Camera, &Transform), With<Camera2d>>,
    windows: Query<&Window>,
    hud: Query<HudBox, HudBoxFilter>,
    pawns: Query<(&Player, &Team, &Health, &Pos, Option<&Bot>)>,
    mut plates: Query<
        (&Nameplate, &mut Node, &mut UiTransform, &mut Visibility),
        Without<NameplateArrow>,
    >,
    mut arrows: Query<(&mut Node, &mut UiTransform), With<NameplateArrow>>,
    mut labels: Query<&mut Text, With<NameplateLabel>>,
) {
    // The grass rig is a scene to be photographed (`tools/grass-shots.sh`), and
    // its two pawns are on opposite sides anyway — but say so, so a caption never
    // ends up with a nameplate in it.
    let mine: Vec<(usize, Vec2, bool)> = if matches!(*scenario, Scenario::Arena) {
        teammates(local.as_deref(), &pawns)
    } else {
        Vec::new()
    };

    let num_players = match session.as_deref() {
        Some(Session::P2P(s)) => s.num_players(),
        Some(Session::SyncTest(s)) => s.num_players(),
        _ => 0,
    };
    let camera = cameras.single().ok();
    let view = camera.and_then(|(camera, _)| camera.logical_viewport_size());
    let mid_x = view.map_or(f32::MAX, |view| view.x) / 2.0;
    let keepout = hud_boxes(&windows, &hud);

    // Where the plates already dealt with ended up, so the rest can avoid landing
    // on top of them. Handle order, so which plate gives way is stable rather than
    // swapping about frame to frame.
    let mut taken: Vec<Vec2> = Vec::new();
    let mut wanted = mine.into_iter();
    for (plate, mut node, mut transform, mut visibility) in &mut plates {
        // A plate is only drawn once it has somewhere to be: no teammate, no
        // camera or no window and it stays hidden with last frame's text.
        let placed = wanted.next().and_then(|(handle, at, is_bot)| {
            let ((camera, camera_transform), view) = (camera?, view?);
            let head = Vec3::new(at.x, at.y + NAME_LIFT, 0.0);
            let screen = camera
                .world_to_viewport(&GlobalTransform::from(*camera_transform), head)
                .ok()?;
            let mut placed = dodge(place(screen, view), screen, &keepout);
            placed.at = destack(placed.at, &taken);
            // Back inside the glass: a plate that stepped past a tall roster, or
            // gave way to two others, could otherwise walk off the bottom.
            placed.at = contain(placed.at, placed.pivot, view);
            taken.push(placed.at);
            Some((handle, is_bot, placed))
        });
        let Some((handle, is_bot, placed)) = placed else {
            if *visibility != Visibility::Hidden {
                *visibility = Visibility::Hidden;
            }
            continue;
        };
        if *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
        }
        set(&mut node.left, Val::Px(placed.at.x));
        set(&mut node.top, Val::Px(placed.at.y));
        // The chevron goes on the side of the name nearest the edge the teammate
        // is beyond, so it points away from the label rather than through it.
        let reversed = placed.at.x > mid_x;
        let flow = if reversed { FlexDirection::RowReverse } else { FlexDirection::Row };
        if node.flex_direction != flow {
            node.flex_direction = flow;
        }
        let name = pawn_name(handle, is_bot, num_players);
        if let Ok(mut text) = labels.get_mut(plate.label) {
            if text.0 != name {
                text.0 = name;
            }
        }
        if let Ok((mut arrow_node, mut arrow_transform)) = arrows.get_mut(plate.arrow) {
            let display = if placed.arrow.is_some() { Display::Flex } else { Display::None };
            if arrow_node.display != display {
                arrow_node.display = display;
            }
            if let Some(angle) = placed.arrow {
                let rotation = Rot2::radians(angle);
                if arrow_transform.rotation != rotation {
                    arrow_transform.rotation = rotation;
                }
            }
        }
        set(&mut transform.translation.x, Val::Percent(placed.pivot.x * 100.0));
        set(&mut transform.translation.y, Val::Percent(placed.pivot.y * 100.0));
    }
}

fn set(slot: &mut Val, value: Val) {
    if *slot != value {
        *slot = value;
    }
}

/// Every piece of HUD currently on screen, as logical-px rectangles.
///
/// ASK THE UI where its boxes are rather than keeping a second copy of the
/// layout — same reason `touch.rs` asks the buttons where they are. The roster
/// grows a line per pawn, the START button comes and goes with the lobby, the
/// spectate button only exists while you are out: any number written down here
/// would be wrong for some match, and thin margins make being wrong visible.
///
/// `ComputedNode` works in PHYSICAL pixels and everything else here is logical,
/// which is the one conversion that has to happen (and the one `touch.rs` got
/// caught by).
fn hud_boxes(windows: &Query<&Window>, hud: &Query<HudBox, HudBoxFilter>) -> Vec<Rect> {
    let Some(scale) = windows.iter().next().map(|window| window.scale_factor()) else {
        return Vec::new();
    };
    if scale <= 0.0 {
        return Vec::new();
    }
    hud.iter()
        .filter(|(_, _, visible)| visible.get())
        .map(|(node, transform, _)| (node.size() / scale, transform.translation / scale))
        .filter(|(size, _)| size.x > 0.0 && size.y > 0.0)
        .map(|(size, centre)| Rect::from_center_size(centre, size))
        .collect()
}

/// Step an edge plate around the HUD, vertically.
///
/// Only ever applied to a plate that was CLAMPED to an edge: that one has no
/// natural home, so anywhere legible will do. A plate sitting over a pawn's head
/// is left where it is even if the roster is behind it — a name that jumped a
/// third of the way down the screen to get out of the way would no longer be
/// telling you which soldier it belongs to, which is the whole job.
///
/// It steps AWAY from the edge it was pinned to: a plate at the top goes down past
/// the round line, the health bar and the roster, one at the bottom goes up over
/// the sights button. Either way it ends up between the HUD and the middle of the
/// screen rather than on top of anything, which is what lets the margin be six
/// pixels instead of a guess at how much HUD might be in the way. The arrow is
/// recomputed from where the plate ended up, so it still points at the pawn.
fn dodge(mut placed: Placement, target: Vec2, boxes: &[Rect]) -> Placement {
    if placed.arrow.is_none() {
        return placed;
    }
    // Pinned to the BOTTOM edge, so it has to climb; everything else falls.
    let up = placed.pivot.y <= -1.0;
    // One pass per box at most. Each step clears the box it hit and moves further
    // from the edge, so it cannot come back to one it has already passed — and the
    // bound means overlapping boxes can't hand it back and forth forever.
    for _ in 0..boxes.len() {
        let box_ = plate_box(placed.at, placed.pivot);
        let Some(hit) = boxes.iter().find(|rect| !box_.intersect(**rect).is_empty()) else {
            break;
        };
        placed.at.y += if up {
            hit.min.y - box_.max.y - DODGE_GAP
        } else {
            hit.max.y - box_.min.y + DODGE_GAP
        };
    }
    let off = target - placed.at;
    placed.arrow = Some(off.y.atan2(off.x));
    placed
}

/// Drop a plate a line at a time until it isn't sitting on one already placed.
///
/// Two teammates off the same edge of the screen clamp to the same point, and
/// three of them did: at the top of a round the whole side musters on one line,
/// so every plate landed on the same pixel and the names read as one smear. Two
/// pawns standing together on screen do the same thing for the same reason.
fn destack(mut at: Vec2, taken: &[Vec2]) -> Vec2 {
    // Bounded by however many plates are already down, so it always terminates
    // even if the nudge somehow lands on a third one.
    for _ in 0..=taken.len() {
        let clash = taken
            .iter()
            .any(|other| (other.x - at.x).abs() < PLATE_W && (other.y - at.y).abs() < PLATE_H);
        if !clash {
            break;
        }
        at.y += PLATE_H;
    }
    at
}

/// Everyone still standing on the local player's side, with where they are.
///
/// Excludes your own pawn — you know where you are, and a label pinned to the
/// middle of your own screen would be the one that never went away. It keeps
/// naming teammates while you are dead: the camera is riding along with one of
/// them by then (`spectate.rs`), and knowing who is who is worth MORE then than
/// it is while you are alive.
///
/// Same three-part rule as `spectate::living_teammates` — same team, still up,
/// not you — because it is answering the same question about the same pawns.
fn teammates(
    local: Option<&LocalPlayers>,
    pawns: &Query<(&Player, &Team, &Health, &Pos, Option<&Bot>)>,
) -> Vec<(usize, Vec2, bool)> {
    let Some(me) = local.and_then(|l| l.0.first().copied()) else {
        return Vec::new();
    };
    // Your OWN pawn's side, not the side of whoever the camera is watching:
    // teams only change between rounds, so a dead pawn still knows which side it
    // died on.
    let Some(my_team) = pawns
        .iter()
        .find(|(player, ..)| player.handle == me)
        .map(|(_, team, ..)| *team)
    else {
        return Vec::new();
    };
    let mut mates: Vec<(usize, Vec2, bool)> = pawns
        .iter()
        .filter(|(player, team, health, ..)| {
            player.handle != me && **team == my_team && health.alive()
        })
        .map(|(player, _, _, pos, bot)| {
            let (x, y) = pos.to_f32();
            (player.handle, Vec2::new(x, y), bot.is_some())
        })
        .collect();
    // Handle order, so a given teammate keeps the same plate entity from frame to
    // frame instead of swapping labels with someone else whenever the query
    // reshuffles.
    mates.sort_unstable_by_key(|&(handle, ..)| handle);
    mates
}

/// Put the plate over the pawn, or on the edge of the screen pointing at them.
///
/// Pure, and split out for the usual reason: the whole feature is these two
/// cases and neither needs a world to check.
///
/// The arrow appears as soon as the plate had to be PULLED to an edge, which is a
/// hair before the pawn itself leaves the screen — deliberately, because the
/// alternative is a name that slides off the top of the window and pops back as
/// an arrow a moment later. A teammate you can see at the very edge of the screen
/// gets a name at the edge with an arrow on it, which is true either way.
fn place(target: Vec2, view: Vec2) -> Placement {
    let lo = Vec2::splat(EDGE_MARGIN);
    // `max(lo)` for the window narrower than its own margins: a phone in a split
    // view, or the first frame before the canvas has a size. Better a plate in
    // the middle of a tiny window than a panic in `clamp`.
    let hi = (view - lo).max(lo);
    let at = target.clamp(lo, hi);
    let off = target - at;
    // Half a pixel of slack, so a teammate sitting exactly on the margin doesn't
    // flicker an arrow on and off. Both readings come off `off` rather than off
    // the arrow's angle: which axis got clamped is the question, and a teammate
    // due west of you is clamped in x while the angle's vertical component is
    // whatever rounding left behind.
    let arrow = (off.length_squared() > 0.25).then(|| off.y.atan2(off.x));
    let pivot = Vec2::new(pivot(off.x), pivot(off.y));
    // The pivot handles the plate that was CLAMPED. The one that wasn't can still
    // hang off the glass: a pawn ten pixels inside the right-hand margin is on
    // screen, so its name is centred over it, so half the name is past the edge —
    // which is how the arrow on a top-right plate went missing. Shove the whole
    // box back in rather than pinning it, or a name would jump a half-width
    // sideways the moment its owner walked near an edge.
    Placement { at: contain(at, pivot, view), arrow, pivot }
}

/// Slide an anchor until the plate hanging off it is wholly on the screen.
fn contain(at: Vec2, pivot: Vec2, view: Vec2) -> Vec2 {
    let lo = Vec2::splat(EDGE_MARGIN);
    // Pulled in from the far edge first, then pushed back off the near one, so in
    // a window too small to hold a whole name the NEAR edge wins: you read the
    // start of the name and lose the tail, rather than the other way round.
    let at = at - (plate_box(at, pivot).max - (view - lo)).max(Vec2::ZERO);
    at + (lo - plate_box(at, pivot).min).max(Vec2::ZERO)
}

/// Where the anchor sits on the plate along one axis, given which way the pawn
/// was clamped: hard against the near side when it was clamped, centred when it
/// wasn't.
fn pivot(off: f32) -> f32 {
    if off < -0.5 {
        0.0 // clamped at the low edge — the plate grows toward the middle
    } else if off > 0.5 {
        -1.0
    } else {
        -0.5
    }
}

/// The rectangle a plate occupies, given where it is anchored and how it hangs.
fn plate_box(at: Vec2, pivot: Vec2) -> Rect {
    let size = Vec2::new(PLATE_W, PLATE_H);
    let top_left = at + pivot * size;
    Rect::from_corners(top_left, top_left + size)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: Vec2 = Vec2::new(400.0, 800.0);

    /// The ordinary case: the name sits where the pawn is, with no arrow on it.
    #[test]
    fn a_teammate_in_view_is_named_where_they_stand() {
        let placed = place(Vec2::new(200.0, 400.0), VIEW);
        assert_eq!(placed.at, Vec2::new(200.0, 400.0));
        assert_eq!(placed.arrow, None);
    }

    /// Off the screen: the plate goes to the edge and grows an arrow pointing the
    /// way you would have to walk.
    #[test]
    fn a_teammate_off_the_screen_gets_an_edge_arrow_pointing_at_them() {
        // Screen coords are y-DOWN, and the rotation is clockwise, so a chevron
        // at angle 0 points right, +PI/2 points down and -PI/2 points up.
        let cases = [
            (Vec2::new(-500.0, 400.0), std::f32::consts::PI), // west
            (Vec2::new(900.0, 400.0), 0.0),                   // east
            (Vec2::new(200.0, -300.0), -std::f32::consts::FRAC_PI_2), // north
            (Vec2::new(200.0, 1500.0), std::f32::consts::FRAC_PI_2), // south
        ];
        for (target, expected) in cases {
            let placed = place(target, VIEW);
            let angle = placed.arrow.unwrap_or_else(|| panic!("{target} got no arrow"));
            // Compared as a direction, not a number: PI and -PI are the same way.
            let turn = (angle - expected).sin().abs();
            assert!(turn < 1e-4, "{target}: arrow {angle} wanted {expected}");
        }
    }

    /// Whatever it is pointing at, the whole plate — not just its anchor — stays
    /// on the glass. That is the thing the thin margin depends on: the pivot has
    /// to swing to the pinned edge, or a six-pixel margin would cut every edge
    /// name in half.
    #[test]
    fn the_whole_plate_stays_on_the_screen() {
        // The band just inside each margin is the one that mattered: a pawn there
        // is on screen, so its plate is centred over it, so half the plate is
        // over the edge. That is where a top-right plate lost its arrow.
        for x in [-9000.0, -1.0, 0.0, 12.0, 30.0, 200.0, 370.0, 388.0, 399.0, 9000.0] {
            for y in [-9000.0, -1.0, 0.0, 10.0, 400.0, 790.0, 799.0, 9000.0] {
                let placed = place(Vec2::new(x, y), VIEW);
                let box_ = plate_box(placed.at, placed.pivot);
                assert!(
                    box_.min.x >= 0.0
                        && box_.min.y >= 0.0
                        && box_.max.x <= VIEW.x
                        && box_.max.y <= VIEW.y,
                    "({x}, {y}) hung the plate off the screen at {box_:?}"
                );
            }
        }
    }

    /// And it really is AT the edge: a teammate off to the west leaves their name
    /// against the left-hand glass, not a name's width inside it.
    #[test]
    fn an_edge_plate_hugs_the_edge() {
        let west = place(Vec2::new(-500.0, 400.0), VIEW);
        assert_eq!(plate_box(west.at, west.pivot).min.x, EDGE_MARGIN);
        let east = place(Vec2::new(900.0, 400.0), VIEW);
        assert_eq!(plate_box(east.at, east.pivot).max.x, VIEW.x - EDGE_MARGIN);
        let north = place(Vec2::new(200.0, -300.0), VIEW);
        assert_eq!(plate_box(north.at, north.pivot).min.y, EDGE_MARGIN);
        let south = place(Vec2::new(200.0, 1500.0), VIEW);
        assert_eq!(plate_box(south.at, south.pivot).max.y, VIEW.y - EDGE_MARGIN);
    }

    /// A corner is both directions at once, so the arrow has to be diagonal
    /// rather than picking whichever axis is bigger.
    #[test]
    fn a_teammate_past_a_corner_gets_a_diagonal_arrow() {
        let angle = place(Vec2::new(-400.0, -400.0), VIEW).arrow.unwrap();
        assert!(angle.cos() < 0.0 && angle.sin() < 0.0, "north-west read as {angle}");
    }

    /// A window smaller than the margins it is supposed to respect: no panic, and
    /// the plate lands somewhere inside it.
    #[test]
    fn a_window_smaller_than_its_own_margins_still_places_a_plate() {
        let tiny = Vec2::new(8.0, 8.0);
        let placed = place(Vec2::new(4.0, 4.0), tiny);
        // Nothing fits, so the near corner wins: you read the start of the name.
        assert_eq!(plate_box(placed.at, placed.pivot).min, Vec2::splat(EDGE_MARGIN));
    }

    /// The roster hangs down the right-hand side, which is exactly where a
    /// teammate away to the east puts an edge plate. It goes below the roster and
    /// keeps pointing at them.
    #[test]
    fn an_edge_plate_behind_the_roster_moves_below_it() {
        let roster = Rect::new(270.0, 40.0, 400.0, 240.0);
        let target = Vec2::new(900.0, 120.0); // off the east edge, level with the roster
        let placed = dodge(place(target, VIEW), target, &[roster]);
        let box_ = plate_box(placed.at, placed.pivot);
        assert!(box_.min.y >= roster.max.y, "still behind the roster at {box_:?}");
        let angle = placed.arrow.unwrap();
        assert!(angle.cos() > 0.0 && angle.sin() < 0.0, "stopped pointing north-east: {angle}");
    }

    /// A plate pinned to the BOTTOM edge climbs instead: the sights button lives
    /// down there, and dropping further would take the name off the screen.
    #[test]
    fn an_edge_plate_under_the_sights_button_moves_above_it() {
        // Flush with the bottom of the window, which is where a thumb control
        // actually sits — with a two-pixel margin there is no room to slip under
        // one, so the plate has to go over the top of it.
        let button = Rect::new(160.0, 700.0, 240.0, 800.0);
        let target = Vec2::new(200.0, 1500.0); // straight off the bottom
        let placed = dodge(place(target, VIEW), target, &[button]);
        let box_ = plate_box(placed.at, placed.pivot);
        assert!(box_.max.y <= button.min.y, "still under the button at {box_:?}");
    }

    /// ...but a plate over a pawn's head stays with the pawn. Moving it would
    /// break the one thing it is for.
    #[test]
    fn a_plate_over_a_pawn_ignores_the_hud() {
        let roster = Rect::new(270.0, 40.0, 400.0, 240.0);
        let at = Vec2::new(330.0, 100.0);
        assert_eq!(dodge(place(at, VIEW), at, &[roster]).at, at);
    }

    /// Two boxes deep — the round line and the health bar are stacked against the
    /// top edge, so clearing one drops the plate straight onto the other.
    #[test]
    fn a_plate_steps_past_a_whole_stack_of_hud() {
        let boxes = [Rect::new(100.0, 8.0, 300.0, 26.0), Rect::new(116.0, 42.0, 284.0, 54.0)];
        let target = Vec2::new(200.0, -300.0);
        let placed = dodge(place(target, VIEW), target, &boxes);
        let box_ = plate_box(placed.at, placed.pivot);
        for rect in boxes {
            assert!(box_.intersect(rect).is_empty(), "landed on {rect:?} at {box_:?}");
        }
    }

    /// A side musters on one line, so every plate clamps to the same point unless
    /// something separates them.
    #[test]
    fn plates_landing_on_each_other_are_stacked_into_lines() {
        let first = Vec2::new(200.0, 66.0);
        let second = destack(first, &[first]);
        let third = destack(first, &[first, second]);
        assert_eq!(second, Vec2::new(200.0, 66.0 + PLATE_H));
        assert_eq!(third, Vec2::new(200.0, 66.0 + 2.0 * PLATE_H));
    }

    /// Far enough apart in x and they share a line — a name only gives way to one
    /// it would actually overlap.
    #[test]
    fn plates_side_by_side_are_left_alone() {
        let first = Vec2::new(80.0, 400.0);
        assert_eq!(destack(Vec2::new(300.0, 400.0), &[first]), Vec2::new(300.0, 400.0));
    }
}
