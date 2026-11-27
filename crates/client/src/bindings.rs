//! Local input bindings for the player this client controls.
//!
//! The server owns and replicates the action entities; the client adds the
//! physical bindings once its player is `Controlled`. Adding `Bindings` is also
//! what marks the action for lightyear's input pipeline, so remote players'
//! actions deliberately never get them: their values come from rebroadcast
//! input instead.

// The binding observers are plain Bevy observer systems; their query filters are
// clearer inline than behind aliases.
#![allow(clippy::type_complexity)]

use bevy::ecs::relationship::Relationship;
use bevy::prelude::*;
use lightyear::input::bei::prelude::{
    Action, ActionOf, Actions, Binding, Bindings, DeadZone, Press, preset,
};
use lightyear::prelude::*;

use shared::protocol::*;

/// Adds every binding observer. Used by the windowed client and by the headless
/// script so both drive the same input path.
pub fn register(app: &mut App) {
    app.add_observer(add_movement_bindings)
        .add_observer(add_jump_bindings)
        .add_observer(add_sprint_bindings)
        .add_observer(add_bindings_to_controlled_player_actions);
}

fn movement_bindings() -> impl Scene {
    bsn! {
        // Normalizes diagonals: without it, two keys give a √2-long wish.
        DeadZone
        Bindings [
            {preset::cardinal::wasd_keys()}
            --
            {preset::axial::left_stick()}
        ]
    }
}

fn jump_bindings() -> impl Scene {
    bsn! {
        // The value becomes the press edge, which is what makes the jump
        // replayable from a buffered input snapshot.
        ~Press::default()
        Bindings [
            Binding(KeyCode::Space)
            --
            Binding(GamepadButton::South)
        ]
    }
}

fn sprint_bindings() -> impl Scene {
    bsn! {
        Bindings [
            Binding(KeyCode::ShiftLeft)
            --
            Binding(GamepadButton::West)
        ]
    }
}

/// Adds the local bindings for one action, but only for the player this client
/// controls.
fn bind_controlled(
    action: Entity,
    player: Entity,
    controlled_players: &Query<(), (With<Player>, With<Controlled>)>,
    commands: &mut Commands,
    scene: impl Scene,
) {
    if controlled_players.get(player).is_err() {
        return;
    }
    commands.entity(action).apply_scene(scene);
}

fn add_movement_bindings(
    trigger: On<Insert<(Action<Movement>, ActionOf<Player>)>>,
    actions: Query<&ActionOf<Player>, (With<Action<Movement>>, Without<Bindings>)>,
    controlled_players: Query<(), (With<Player>, With<Controlled>)>,
    mut commands: Commands,
) {
    let Ok(action_of) = actions.get(trigger.entity) else {
        return;
    };
    bind_controlled(
        trigger.entity,
        action_of.get(),
        &controlled_players,
        &mut commands,
        movement_bindings(),
    );
}

fn add_jump_bindings(
    trigger: On<Insert<(Action<Jump>, ActionOf<Player>)>>,
    actions: Query<&ActionOf<Player>, (With<Action<Jump>>, Without<Bindings>)>,
    controlled_players: Query<(), (With<Player>, With<Controlled>)>,
    mut commands: Commands,
) {
    let Ok(action_of) = actions.get(trigger.entity) else {
        return;
    };
    bind_controlled(
        trigger.entity,
        action_of.get(),
        &controlled_players,
        &mut commands,
        jump_bindings(),
    );
}

fn add_sprint_bindings(
    trigger: On<Insert<(Action<Sprint>, ActionOf<Player>)>>,
    actions: Query<&ActionOf<Player>, (With<Action<Sprint>>, Without<Bindings>)>,
    controlled_players: Query<(), (With<Player>, With<Controlled>)>,
    mut commands: Commands,
) {
    let Ok(action_of) = actions.get(trigger.entity) else {
        return;
    };
    bind_controlled(
        trigger.entity,
        action_of.get(),
        &controlled_players,
        &mut commands,
        sprint_bindings(),
    );
}

/// Covers the opposite ordering: the player is marked `Controlled` only after
/// its action entities have arrived.
fn add_bindings_to_controlled_player_actions(
    trigger: On<Add<(Player, Controlled, Actions<Player>)>>,
    players: Query<&Actions<Player>, (With<Player>, With<Controlled>)>,
    movement: Query<(), (With<Action<Movement>>, Without<Bindings>)>,
    jump: Query<(), (With<Action<Jump>>, Without<Bindings>)>,
    sprint: Query<(), (With<Action<Sprint>>, Without<Bindings>)>,
    mut commands: Commands,
) {
    let Ok(player_actions) = players.get(trigger.entity) else {
        return;
    };
    for action in player_actions.iter() {
        if movement.get(action).is_ok() {
            commands.entity(action).apply_scene(movement_bindings());
        } else if jump.get(action).is_ok() {
            commands.entity(action).apply_scene(jump_bindings());
        } else if sprint.get(action).is_ok() {
            commands.entity(action).apply_scene(sprint_bindings());
        }
    }
}
