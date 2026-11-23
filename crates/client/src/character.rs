//! The local virtual character: Avian's controller, wired to [`Local`].
//!
//! The movement model writes intent into [`Local::player`]. [`apply_local`]
//! copies it onto the character entity in `FixedUpdate`, Avian's
//! [`MoveAndSlidePlugin`] moves the entity in `FixedPostUpdate`, and
//! [`read_back`] copies the result into [`Local`] in `FixedLast` for reporting
//! and rendering.

use bevy::prelude::*;
use kcc_avian::prelude::*;

use crate::Local;

/// The entity carrying [`VirtualCharacter`] and the local pose.
#[derive(Resource, Clone, Copy, Debug)]
pub struct Character(pub Entity);

/// Length of the capsule's cylindrical section, excluding both caps.
pub const CAPSULE_LENGTH: f32 = proto::PLAYER_HEIGHT - proto::PLAYER_RADIUS * 2.0;

/// Spawns the local character at the resting height and records its entity.
pub fn spawn_player(world: &mut World) -> Entity {
    let entity = world.spawn(player_bundle(spawn_position())).id();
    world.insert_resource(Character(entity));
    entity
}

pub fn spawn_position() -> Vec3 {
    // Capsule centre resting on the floor: half_height + radius = 0.9.
    Vec3::new(0.0, 0.9, 0.0)
}

pub fn player_bundle(pos: Vec3) -> impl Bundle {
    (
        VirtualCharacter,
        AutoMoveAndSlide::default(),
        SlopeHandling::default(),
        GroundSnapping::default(),
        StairStepping::default(),
        UpDirection::default(),
        Collider::capsule(proto::PLAYER_RADIUS, CAPSULE_LENGTH),
        Position::from(pos),
        Rotation::default(),
        Transform::from_translation(pos),
    )
}

/// Spawns the static terrain as colliders, without any renderable.
pub fn spawn_terrain(world: &mut World, terrain: &sim::StaticWorld) {
    for b in terrain.boxes() {
        world.spawn(terrain_bundle(b));
    }
}

pub fn terrain_bundle(b: &sim::Aabb3d) -> impl Bundle {
    let size = b.max - b.min;
    let centre = Vec3::from((b.max + b.min) * 0.5);
    (
        RigidBody::Static,
        Collider::cuboid(size.x, size.y, size.z),
        Position::from(centre),
        Rotation::default(),
    )
}

/// Copies the movement model's intent onto the character before the fixed step.
pub fn apply_local(
    local: Res<Local>,
    character: Res<Character>,
    mut players: Query<(&mut Position, &mut LinearVelocity)>,
) {
    if let Ok((mut position, mut velocity)) = players.get_mut(character.0) {
        position.0 = local.player.pos;
        velocity.0 = local.player.vel;
    }
}

/// Copies the physics result back into [`Local`].
pub fn read_back(
    mut local: ResMut<Local>,
    character: Res<Character>,
    players: Query<(&Position, &LinearVelocity, Has<Grounded>)>,
) {
    if let Ok((position, velocity, grounded)) = players.get(character.0) {
        local.player.pos = position.0;
        local.player.vel = velocity.0;
        local.player.grounded = grounded;
    }
}
