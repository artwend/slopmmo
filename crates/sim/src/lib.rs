//! Engine-free simulation math shared by the client, the server and the tools.
//!
//! No Bevy ECS and no physics engine: this crate must stay cheap to compile and
//! trivial to unit-test, because every decision this project makes about tick
//! rate, acceleration and terrain cost is measured here first.
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
pub mod world;

pub use bevy_shape::Aabb3d;
pub use kcc::{Capsule, KccParams, MoveOutput};
pub use movement::{MoveParams, step_velocity, step_velocity_raw};
pub use world::{Aabb3dExt, StaticWorld};

pub use kcc::move_and_slide;

/// Capsule radius used for every player, client and server.
pub const PLAYER_RADIUS: f32 = proto::PLAYER_RADIUS;
/// Length of the capsule's cylindrical section, excluding both caps.
pub const CAPSULE_LENGTH: f32 = proto::PLAYER_HEIGHT - 2.0 * proto::PLAYER_RADIUS;
/// Height of the capsule centre while resting on the floor.
pub const RESTING_HEIGHT: f32 = CAPSULE_LENGTH * 0.5 + PLAYER_RADIUS;
