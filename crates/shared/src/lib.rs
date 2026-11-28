//! Gameplay shared by the client and the server.
//!
//! The protocol ([`protocol`]) is registered identically on both ends, and the
//! physics pipeline is installed once here so both sides simulate the character
//! with the same plugins, the same fixed timestep and the same movement model.
//!
//! Split of responsibility:
//!
//! * [`kcc`] — the reference capsule-vs-static-geometry solver.
//! * [`movement`] — the input→velocity model. This is the single movement
//!   implementation: the networked client and server both feed it the
//!   enhanced-input action values every fixed tick.
//! * [`world`] — the static test room, shared by both sides so they collide
//!   against identical geometry.

pub mod kcc;
pub mod movement;
pub mod protocol;
pub mod world;

use core::time::Duration;

use bevy::prelude::*;
// `::kcc` is the Avian controller crate; the bare `kcc` in this crate root is
// the engine-free reference solver (see [`kcc`]).
use ::kcc::avian3d::prelude::{
    IslandPlugin, IslandSleepingPlugin, PhysicsInterpolationPlugin, PhysicsPlugins,
    PhysicsTransformPlugin, RigidBody,
};
use ::kcc::prelude::{
    AutoMoveAndSlide, GroundSnapping, MoveAndSlidePlugin, SlopeHandling, StairStepping,
    UpDirection, VirtualCharacter,
};
use lightyear::avian3d::plugin::{AvianReplicationMode, LightyearAvianPlugin};
use lightyear::input::bei::prelude::{Action, Actions};
use lightyear::prelude::*;

use crate::protocol::*;

pub use crate::kcc::{KccParams, MoveOutput, move_and_slide};
pub use ::kcc::avian3d::prelude::{Collider, LinearVelocity, Position, Rotation};
pub use ::kcc::prelude::Grounded;
pub use bevy_shape::Aabb3d;
pub use movement::{MAX_ACCEL, MAX_SPEED, MoveParams, step_velocity_raw};
pub use world::{Aabb3dExt, StaticWorld};

/// Capsule radius used for every player, client and server.
pub const PLAYER_RADIUS: f32 = 0.4;
/// Full capsule height, including both caps.
pub const PLAYER_HEIGHT: f32 = 1.8;
/// Length of the capsule's cylindrical section, excluding both caps.
pub const CAPSULE_LENGTH: f32 = PLAYER_HEIGHT - 2.0 * PLAYER_RADIUS;
/// Height of the capsule centre while resting on the floor.
pub const RESTING_HEIGHT: f32 = CAPSULE_LENGTH * 0.5 + PLAYER_RADIUS;

/// The movement model's tunables, stored as a Bevy resource.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct MovementParams(pub MoveParams);

pub const FIXED_TIMESTEP_HZ: f64 = 64.0;
/// Duration of one tick.
pub const TICK_DURATION: Duration = Duration::from_nanos(1_000_000_000 / FIXED_TIMESTEP_HZ as u64);
/// How often replication is flushed to clients.
pub const SEND_INTERVAL: Duration = Duration::from_millis(100);

/// Registers the protocol and the physics/character pipeline.
#[derive(Clone)]
pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(protocol::ProtocolPlugin);

        // Avian integration first: it installs the position/transform sync and
        // must be finished before the physics schedule exists.
        app.add_plugins(LightyearAvianPlugin {
            replication_mode: AvianReplicationMode::Position {
                sync_to_transform: false,
            },
            // The character is not a rigid body, so the default registration
            // (filtered on `With<RigidBody>`) would never match it.
            register_physics_components: false,
            // A replay has to restore Avian's collider trees, not just the pose.
            rollback_resources: true,
            ..default()
        });

        // Islands and sleeping break rollback; the transform sync is owned by
        // `LightyearAvianPlugin`.
        app.add_plugins(
            PhysicsPlugins::default()
                .build()
                .disable::<PhysicsTransformPlugin>()
                .disable::<PhysicsInterpolationPlugin>()
                .disable::<IslandPlugin>()
                .disable::<IslandSleepingPlugin>(),
        );
        // Must come after `PhysicsPlugins`: it orders itself inside the physics step.
        app.add_plugins(MoveAndSlidePlugin::default());

        // Custom rules for the controller-driven character.
        app.component::<Position>()
            .replicate()
            .predict()
            .add_linear_interpolation()
            .add_correction();
        app.component::<Rotation>()
            .replicate()
            .predict()
            .add_linear_interpolation()
            .add_correction();
        app.component::<LinearVelocity>().replicate().predict();
        // Derived by the controller every physics step, but the first replayed
        // tick must see the grounded state of the tick it replays.
        app.component::<Grounded>().local_rollback();

        app.init_resource::<MovementParams>()
            .add_systems(FixedUpdate, apply_player_input);
    }
}

/// The capsule collider every player carries, client and server.
///
/// The reference solver in [`kcc`] defaults to this collider, so both describe
/// the same body.
pub fn player_collider() -> Collider {
    Collider::capsule(PLAYER_RADIUS, CAPSULE_LENGTH)
}

/// Physics bundle shared by server-spawned players and client-side predicted
/// copies. Only the networked components are replicated; the rest is rebuilt
/// locally on the client.
pub fn player_physics_bundle(position: Vec3) -> impl Bundle {
    (
        VirtualCharacter,
        AutoMoveAndSlide::default(),
        SlopeHandling::default(),
        GroundSnapping::default(),
        StairStepping::default(),
        UpDirection::default(),
        player_collider(),
        Position::from(position),
        Rotation::default(),
        LinearVelocity::default(),
        // The controller writes this every physics step so the rendered capsule
        // follows the predicted/authoritative pose.
        Transform::from_translation(position),
    )
}

/// Collider-only bundle for one static terrain box.
pub fn terrain_bundle(b: &Aabb3d) -> impl Bundle {
    let size = b.max - b.min;
    let centre = Vec3::from((b.max + b.min) * 0.5);
    (
        RigidBody::Static,
        Collider::cuboid(size.x, size.y, size.z),
        Position::from(centre),
        Rotation::default(),
    )
}

/// The static room both sides collide against.
pub fn test_room() -> StaticWorld {
    StaticWorld::test_room()
}

/// Spawn a player slot for a peer.
pub fn initial_position(peer_id: PeerId) -> Vec3 {
    let slot = (peer_id.to_bits() % 6) as f32;
    Vec3::new((slot - 2.5) * 6.0, RESTING_HEIGHT, 0.0)
}

/// Feeds the player's BEI action values into the shared velocity model.
///
/// Runs on both sides: the client applies it to its predicted entity, the
/// server to the authoritative one, so a replay produces the same velocity.
#[allow(clippy::type_complexity)]
fn apply_player_input(
    time: Res<Time>,
    params: Res<MovementParams>,
    mut players: Query<
        (&Actions<Player>, Has<Grounded>, &mut LinearVelocity),
        With<VirtualCharacter>,
    >,
    movement: Query<&Action<Movement>>,
    jump: Query<&Action<Jump>>,
    sprint: Query<&Action<Sprint>>,
) {
    let dt = time.delta_secs();
    let params = params.0;
    for (player_actions, grounded, mut velocity) in &mut players {
        let mut wish = Vec3::ZERO;
        let mut pressed_jump = false;
        let mut pressed_sprint = false;
        for action in player_actions.iter() {
            if let Ok(value) = movement.get(action) {
                // Camera space to world space: forward is `-Z`.
                wish = Vec3::new(value.x, 0.0, -value.y);
            }
            if let Ok(value) = jump.get(action) {
                pressed_jump = **value;
            }
            if let Ok(value) = sprint.get(action) {
                pressed_sprint = **value;
            }
        }
        if wish.length_squared() > 1.0 {
            wish = wish.normalize();
        }
        velocity.0 = step_velocity_raw(
            &params,
            wish,
            pressed_sprint,
            pressed_jump,
            velocity.0,
            grounded,
            dt,
        );
    }
}
