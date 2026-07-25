//! Local input collection → the `PlayerInput` structs GGRS sends each tick.
//! Milestone 1 adds the touch joystick + fire buttons; for the scaffold this
//! is keyboard only (WASD/arrows to move, Space to fire) so the desktop dev
//! loop works.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy_ggrs::{LocalInputs, LocalPlayers};

use army_ghosts_sim::{PlayerInput, BTN_ADS, BTN_FIRE};

use crate::ads::Ads;
use crate::touch::TouchControls;
use crate::SessionConfig;

pub fn read_local_inputs(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    touch: Res<TouchControls>,
    ads: Res<Ads>,
    local_players: Res<LocalPlayers>,
) {
    let mut local_inputs = HashMap::new();
    // Inputs drive the *first* local handle; any additional local handles
    // (synctest mode simulates every player locally) stay idle.
    let mut first = true;
    for handle in &local_players.0 {
        let mut input = PlayerInput::default();
        if first {
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
            first = false;
        }
        local_inputs.insert(*handle, input);
    }
    commands.insert_resource(LocalInputs::<SessionConfig>(local_inputs));
}
