//! Network protocol for the game.
//!
//! Everything here is registered identically on both ends by [`ProtocolPlugin`].
//! The pose and velocity components are Avian's, registered with custom rules in
//! `lib.rs`: a [`VirtualCharacter`](kcc::VirtualCharacter) is moved by the
//! character controller, not by a rigid body, so the default rigid-body
//! replication filter would never match it.

use bevy::prelude::*;
use lightyear::input::bei::prelude::*;
use lightyear::prelude::input::InputConfig;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

/// The player entity is the input context: BEI action entities hang off it.
#[derive(Component, Serialize, Deserialize, Reflect, Clone, Debug, PartialEq)]
pub struct Player;

/// Stable identity of the player, independent of its replicated entity id.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlayerId(pub PeerId);

/// WASD / left stick, in camera space: `x` is right, `y` is forward.
#[derive(Debug, InputAction)]
#[action_output(Vec2)]
pub struct Movement;

/// Jump. The client's `Press` condition makes the value true for a single
/// prediction tick, so the value itself is the press edge.
#[derive(Debug, InputAction)]
#[action_output(bool)]
pub struct Jump;

/// Held sprint modifier.
#[derive(Debug, InputAction)]
#[action_output(bool)]
pub struct Sprint;

/// Registers every message, component and input action the game sends.
#[derive(Clone)]
pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(InputPlugin::<Player>::new(InputConfig {
            // Remote clients need the inputs too: every client predicts every
            // player and rolls back on the authoritative input stream.
            rebroadcast_inputs: true,
            ..default()
        }));
        app.register_input_action::<Movement>();
        app.register_input_action::<Jump>();
        app.register_input_action::<Sprint>();

        app.component::<PlayerId>().replicate();
    }
}
