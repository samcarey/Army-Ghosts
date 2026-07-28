//! Spectating: what you do for the rest of a round after you are killed.
//!
//! There is no respawn in Ghost War, so being shot used to mean a second and a
//! half of black and now means up to two minutes of it. The answer is the one
//! every game in this genre reached: you ride along with whoever is left on your
//! side, and a button walks you round them.
//!
//! # Render-only, all of it
//!
//! Nothing here touches the sim, and nothing here may. Who a peer is watching is
//! a fact about that peer's screen — two players spectating different teammates
//! is not a disagreement about the world, it is two people looking at different
//! parts of one — so it stays out of the input stream and out of rollback
//! entirely. That is the same line `client/src/vision.rs` sits on and for the
//! same reason: the sim cannot have a point of view, because every peer
//! simulates every pawn.
//!
//! The camera itself is `render::camera_follow`; this only decides which pawn it
//! is told to follow.

use bevy::prelude::*;
use bevy_ggrs::LocalPlayers;

use army_ghosts_sim::{Health, Player, Team};

/// Who the camera is watching, when it isn't watching you.
///
/// `None` means "your own pawn" — either you are alive, or there is nobody left
/// to watch. Holding a HANDLE rather than an `Entity` on purpose: rollback
/// despawns and respawns entities freely, and a stale `Entity` would leave the
/// camera pointing at nothing, whereas a handle is the sim's own stable name for
/// a pawn.
#[derive(Resource, Default, Debug)]
pub struct Spectating {
    pub watching: Option<usize>,
}

#[derive(Component)]
pub struct SpectateButton;

#[derive(Component)]
pub struct SpectateLabel;

/// Bottom-left, opposite the stance column and clear of the joystick's usual
/// thumb arc. Only visible while you are actually out.
const LEFT_OFFSET: f32 = 22.0;
const BOTTOM_OFFSET: f32 = 172.0;

pub fn setup_spectate(mut commands: Commands) {
    commands
        .spawn((
            SpectateButton,
            Button,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(LEFT_OFFSET),
                bottom: Val::Px(BOTTOM_OFFSET),
                padding: UiRect::axes(Val::Px(14.0), Val::Px(9.0)),
                border_radius: BorderRadius::all(Val::Px(9.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.10, 0.13, 0.07, 0.62)),
            Visibility::Hidden,
        ))
        .with_children(|pill| {
            pill.spawn((
                SpectateLabel,
                Text::new(""),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgba(0.85, 0.92, 0.75, 0.85)),
            ));
        });
}

/// Everyone still standing on the local player's side, in handle order.
///
/// Sorted so that cycling is a stable walk around the same list rather than
/// whatever order the query happened to produce this frame — that is not a
/// determinism requirement (nothing here is in the sim) but it is a usability
/// one, since a list that reshuffles itself makes "next" meaningless.
fn living_teammates(
    me: usize,
    pawns: &Query<(&Player, &Team, &Health)>,
) -> Vec<usize> {
    let Some(my_team) = pawns
        .iter()
        .find(|(player, ..)| player.handle == me)
        .map(|(_, team, _)| *team)
    else {
        return Vec::new();
    };
    let mut mates: Vec<usize> = pawns
        .iter()
        .filter(|(player, team, health)| {
            player.handle != me && **team == my_team && health.alive()
        })
        .map(|(player, ..)| player.handle)
        .collect();
    mates.sort_unstable();
    mates
}

/// Pick someone to watch while you're out, drop them when you're not, and step
/// through them on a tap or a key.
///
/// The order matters: it settles on a valid target BEFORE handling the press, so
/// a tap always moves you on from someone you were really watching rather than
/// from a teammate who died a frame ago.
pub fn update_spectate(
    keys: Res<ButtonInput<KeyCode>>,
    local: Option<Res<LocalPlayers>>,
    pawns: Query<(&Player, &Team, &Health)>,
    presses: Query<&Interaction, (Changed<Interaction>, With<SpectateButton>)>,
    mut spectating: ResMut<Spectating>,
) {
    let me = local.as_deref().and_then(|l| l.0.first().copied());
    let alive = me
        .and_then(|handle| pawns.iter().find(|(player, ..)| player.handle == handle))
        .map(|(_, _, health)| health.alive());

    // Alive, or not in the game at all: watch yourself.
    let (Some(me), Some(false)) = (me, alive) else {
        if spectating.watching.is_some() {
            spectating.watching = None;
        }
        return;
    };

    let mates = living_teammates(me, &pawns);
    let advance =
        presses.iter().any(|i| *i == Interaction::Pressed) || keys.just_pressed(KeyCode::Tab);
    let next = choose(spectating.watching, &mates, advance);
    if spectating.watching != next {
        spectating.watching = next;
    }
}

/// Who to watch, out of `mates`, given who is being watched now and whether the
/// player has just asked to move on.
///
/// Pure, and separated out for exactly that reason: everything interesting about
/// spectating is in these four cases and none of them need a world to check.
///
/// `None` means "your own body". That is what an empty `mates` returns — your
/// whole side is down, and the alternative would be snapping the camera to the
/// people who just killed you, which is both a spoiler and not information you
/// are owed.
fn choose(current: Option<usize>, mates: &[usize], advance: bool) -> Option<usize> {
    if mates.is_empty() {
        return None;
    }
    // Whoever we were watching if they are still standing, otherwise the first
    // teammate — which is also what picks a target on the tick you die, and what
    // moves you on when the one you were watching is killed.
    let index = current
        .and_then(|who| mates.iter().position(|&mate| mate == who))
        .unwrap_or(0);
    let index = if advance { (index + 1) % mates.len() } else { index };
    Some(mates[index])
}

/// Show the button only while there is someone to watch, and name them on it.
pub fn update_spectate_button(
    spectating: Res<Spectating>,
    pawns: Query<(&Player, Option<&army_ghosts_sim::Bot>)>,
    mut buttons: Query<&mut Visibility, With<SpectateButton>>,
    mut labels: Query<&mut Text, With<SpectateLabel>>,
) {
    let watching = spectating.watching;
    for mut visibility in &mut buttons {
        let wanted = if watching.is_some() { Visibility::Inherited } else { Visibility::Hidden };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    let Some(handle) = watching else { return };
    let is_bot = pawns
        .iter()
        .any(|(player, bot)| player.handle == handle && bot.is_some());
    // The chevron is the affordance: the label says who, the arrow says there
    // are more. ASCII, because the embedded default font is missing most of
    // what a nicer glyph would need.
    let line = format!("{} {} >", if is_bot { "BOT" } else { "PLAYER" }, handle + 1);
    for mut text in &mut labels {
        if text.0 != line {
            text.0 = line.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nobody left on your side: back to your own body, never to an enemy.
    #[test]
    fn a_wiped_out_side_leaves_you_on_yourself() {
        assert_eq!(choose(Some(4), &[], false), None);
        assert_eq!(choose(None, &[], true), None);
    }

    /// The tick you die, the camera picks somebody without being asked.
    #[test]
    fn dying_picks_a_teammate() {
        assert_eq!(choose(None, &[2, 4, 6], false), Some(2));
    }

    /// A tap walks round the list and wraps, so every teammate is two taps away
    /// from any other and there is no end to get stuck at.
    #[test]
    fn tapping_walks_round_and_wraps() {
        let mates = [2, 4, 6];
        assert_eq!(choose(Some(2), &mates, true), Some(4));
        assert_eq!(choose(Some(4), &mates, true), Some(6));
        assert_eq!(choose(Some(6), &mates, true), Some(2));
    }

    /// Without a tap it holds still — otherwise the camera would walk the roster
    /// on its own every frame.
    #[test]
    fn it_stays_put_until_asked() {
        assert_eq!(choose(Some(4), &[2, 4, 6], false), Some(4));
    }

    /// The one you were watching is killed: you move on rather than following a
    /// corpse. `living_teammates` has already dropped them from the list, so all
    /// this has to do is not insist on a handle that isn't there.
    #[test]
    fn watching_someone_who_dies_moves_you_on() {
        assert_eq!(choose(Some(4), &[2, 6], false), Some(2));
    }
}
