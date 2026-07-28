//! Local input collection → the `PlayerInput` structs GGRS sends each tick.
//! Touch (joystick + fire button + the bevy_ui buttons) is the real control
//! scheme; the keyboard equivalents keep the desktop dev loop and the headless
//! tests usable: WASD/arrows move, Space fires, Shift toggles sights, C goes
//! down a stance and V gets back up.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy_ggrs::{LocalInputs, LocalPlayers};

use army_ghosts_sim::{PlayerInput, Scenario, BTN_ADS, BTN_FIRE};

use crate::ads::Ads;
use crate::stance::StanceControl;
use crate::touch::TouchControls;
use crate::SessionConfig;

// A bevy system takes one parameter per thing it reads; nine is what this one
// legitimately reads.
#[allow(clippy::too_many_arguments)]
pub fn read_local_inputs(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    touch: Res<TouchControls>,
    ads: Res<Ads>,
    stance: Res<StanceControl>,
    local_players: Res<LocalPlayers>,
    scenario: Res<Scenario>,
    bots: Res<crate::menu::BotCount>,
    aggro: Res<crate::menu::Aggression>,
    side: Res<crate::menu::SidePick>,
) {
    let mut local_inputs = HashMap::new();
    // Inputs drive the *first* local handle; any additional local handles
    // (synctest mode simulates every player locally) stay idle.
    let mut first = true;
    for handle in &local_players.0 {
        let mut input = PlayerInput::default();
        // The bot count rides on handle 0's input and only handle 0's — that
        // is the copy `reconcile_bots` honours. Sent every tick as an absolute
        // number, not a "+1" edge, so replaying a tick reconciles to the same
        // world however many times rollback runs it.
        if *handle == 0 {
            input.set_bots(bots.0 as u8);
            // Same channel, same reasoning: an absolute level every tick, so a
            // replayed frame dials the bots to the same place. Unlike the bot
            // count this one is applied to bots that already exist, which is
            // what makes turning it mid-match do anything.
            input.set_aggression(aggro.0);
        }
        if first {
            // Which side this player wants. Unlike the two dials above it goes
            // on the FIRST LOCAL handle rather than on handle 0, because it is a
            // statement about whoever is holding the phone — and every player's
            // own copy is read, so it does not matter who is host.
            input.set_team_request(side.0);
            // Keyboard (desktop) …
            let mut x: i32 = 0;
            let mut y: i32 = 0;
            if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
                x -= 127;
            }
            if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
                x += 127;
            }
            if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
                y -= 127;
            }
            if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
                y += 127;
            }
            // … merged with the touch joystick (analog; wins when active).
            if touch.move_vec != Vec2::ZERO {
                x = (touch.move_vec.x * 127.0) as i32;
                y = (touch.move_vec.y * 127.0) as i32;
            }
            input.move_x = x.clamp(-127, 127) as i8;
            input.move_y = y.clamp(-127, 127) as i8;
            if keys.pressed(KeyCode::Space) || touch.firing {
                input.buttons |= BTN_FIRE;
            }
            // ADS is a local toggle (`ads.rs`); the sim reads it off this bit
            // so the movement lock is identical on every peer.
            if ads.active {
                input.buttons |= BTN_ADS;
            }
            // Same deal for the stance the player is asking for (`stance.rs`):
            // an absolute level, re-sent every tick, so replaying a frame
            // re-applies it identically.
            input.set_stance(stance.wanted);
            first = false;
        } else {
            // Idle handles still have to ASK for their stance every tick, or
            // the sim stands them back up — the wire carries the level wanted,
            // not a change. Normally that's just "stand"; the grass rig uses it
            // to pose the pawn nobody is driving.
            input.set_stance(scenario.idle_stance());
        }
        local_inputs.insert(*handle, input);
    }
    commands.insert_resource(LocalInputs::<SessionConfig>(local_inputs));
}
