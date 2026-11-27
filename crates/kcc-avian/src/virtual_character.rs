//! A virtual character controller using the [`MoveAndSlide`] algorithm.
//!
//! Ported from Avian's `virtual-character` branch. See [`VirtualCharacter`] and
//! [`AutoMoveAndSlide`] for the entity-side API.

use avian3d::prelude::*;
use bevy::{
    ecs::{intern::Interned, schedule::ScheduleLabel},
    prelude::*,
};

/// Marker component for virtual characters that use character controller logic.
///
/// A virtual character is an entity that does not use a [`RigidBody`] by
/// default, but instead relies on [`MoveAndSlide`] for movement and collisions.
#[derive(Component, Default)]
#[require(AutoMoveAndSlide)]
pub struct VirtualCharacter;

/// Component that enables automatic [`MoveAndSlide`] updates for an entity.
#[derive(Component, Clone, Debug)]
#[require(LinearVelocity, Position, Rotation, CustomPositionIntegration)]
pub struct AutoMoveAndSlide {
    /// If `true`, automatic [`MoveAndSlide`] updates are performed each frame.
    pub enabled: bool,
    /// Configuration for the [`MoveAndSlide`] algorithm.
    pub config: MoveAndSlideConfig,
    /// Optional custom shape for collision detection.
    ///
    /// If `None`, the entity's existing [`Collider`] is used, if available.
    pub custom_shape: Option<Collider>,
    /// Callback function invoked on each contact surface hit during movement.
    ///
    /// Returning `false` ignores the hit, exactly like
    /// [`MoveAndSlideHitResponse::Ignore`]; the character still stops at the
    /// contact but does not slide along it.
    pub on_hit: fn(MoveAndSlideHitData) -> bool,
}

impl Default for AutoMoveAndSlide {
    fn default() -> Self {
        Self::new(MoveAndSlideConfig::default(), |_| true)
    }
}

impl AutoMoveAndSlide {
    /// Creates a new [`AutoMoveAndSlide`] component with the given settings.
    #[inline]
    pub const fn new(config: MoveAndSlideConfig, on_hit: fn(MoveAndSlideHitData) -> bool) -> Self {
        Self {
            enabled: true,
            config,
            custom_shape: None,
            on_hit,
        }
    }
}

/// The system set the character step runs in, inside the physics schedule.
///
/// It runs after the broad phase so the collider trees queried by
/// [`MoveAndSlide`] already describe this step's colliders, including on the
/// first tick.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VirtualCharacterSystems;

/// Performs automatic [`MoveAndSlide`] updates for entities with
/// [`AutoMoveAndSlide`].
///
/// By default the step runs in the [`PhysicsSchedule`]; a different schedule can
/// be passed to [`MoveAndSlidePlugin::new`]. Add this plugin *after*
/// [`PhysicsPlugins`] so the physics step sets exist.
pub struct MoveAndSlidePlugin {
    schedule: Interned<dyn ScheduleLabel>,
}

impl MoveAndSlidePlugin {
    /// Creates a [`MoveAndSlidePlugin`] that runs in the given schedule.
    pub fn new(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: schedule.intern(),
        }
    }
}

impl Default for MoveAndSlidePlugin {
    fn default() -> Self {
        Self::new(PhysicsSchedule)
    }
}

impl Plugin for MoveAndSlidePlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            self.schedule,
            VirtualCharacterSystems
                .after(PhysicsStepSystems::BroadPhase)
                .before(PhysicsStepSystems::NarrowPhase),
        );
        app.add_systems(
            self.schedule,
            move_and_slide.in_set(VirtualCharacterSystems),
        );
    }
}

#[allow(clippy::type_complexity)]
fn move_and_slide(
    mut characters: Query<(
        Entity,
        &Position,
        &Rotation,
        Option<&mut Transform>,
        &LinearVelocity,
        Option<&Collider>,
        &AutoMoveAndSlide,
        Option<&UpDirection>,
        Option<&SlopeHandling>,
        Option<&GroundSnapping>,
        Has<Grounded>,
    )>,
    mut commands: Commands,
    move_and_slide: MoveAndSlide,
    time: Res<Time>,
) {
    let delta = time.delta();

    for (
        entity,
        position,
        rotation,
        mut transform,
        lin_vel,
        collider,
        config,
        up,
        slopes,
        snapping,
        was_grounded,
    ) in &mut characters
    {
        if !config.enabled {
            continue;
        }

        let Some(collider) = config.custom_shape.as_ref().or(collider) else {
            // No collider available, skip movement.
            continue;
        };

        let up = up.map_or(Dir3::Y, |up| up.0);
        let up_vector = *up;
        let ground_cos = slopes
            .map_or(SlopeHandling::default().max_ground_angle, |slopes| {
                slopes.max_ground_angle
            })
            .cos();
        let wall_slide_cos = slopes.map(|slopes| slopes.max_wall_slide_angle.cos());

        let filter = SpatialQueryFilter::from_excluded_entities([entity]);
        let on_hit = config.on_hit;
        let mut grounded = false;

        let output = move_and_slide.move_and_slide(
            collider,
            position.0,
            *rotation,
            lin_vel.0,
            delta,
            &config.config,
            &filter,
            |hit| {
                let normal = *hit.normal;
                let up_dot = normal.dot(up_vector);

                if up_dot >= ground_cos {
                    grounded = true;
                } else if let Some(wall_slide_cos) = wall_slide_cos
                    && up_dot < wall_slide_cos
                {
                    // Steeper than the wall-slide limit: treat the surface as
                    // vertical so sliding along it cannot move the character up
                    // or down.
                    let flat = Vec3::new(normal.x, 0.0, normal.z);
                    if let Ok(flat) = Dir3::new(flat.normalize_or_zero()) {
                        *hit.normal = flat;
                    }
                }

                if on_hit(hit) {
                    MoveAndSlideHitResponse::Accept
                } else {
                    MoveAndSlideHitResponse::Ignore
                }
            },
        );

        let mut new_position = output.position;

        if let Some(snapping) = snapping
            && was_grounded
            && (!snapping.skip_upward_movement || output.projected_velocity.y <= 0.0)
        {
            let probe = up_vector * -snapping.max_snap_distance;
            let skin_width = move_and_slide.length_unit.0 * config.config.skin_width;
            if let Some(hit) = move_and_slide.cast_move(
                collider,
                new_position,
                *rotation,
                probe,
                skin_width,
                &filter,
            ) && hit.normal1.dot(up_vector) >= ground_cos
            {
                new_position += up_vector * -hit.distance;
                grounded = true;
            }
        }

        if let Some(transform) = &mut transform {
            transform.translation = new_position;
        }

        // `MoveAndSlide` itself queries `Position` and `LinearVelocity`, so both
        // are read-only here and the result is committed through commands. The
        // physics writeback applies the new `Position` to `Transform` at the end
        // of the step anyway.
        let mut entity = commands.entity(entity);
        entity.insert((
            Position::from(new_position),
            LinearVelocity(output.projected_velocity),
        ));
        if grounded {
            entity.insert(Grounded);
        } else {
            entity.try_remove::<Grounded>();
        }
    }
}

/// Component representing the up-direction for a virtual character.
///
/// Ground detection and slope handling are based on this up-direction. By
/// default it is the positive y-axis.
#[derive(Component, Clone, Copy, Debug)]
pub struct UpDirection(pub Dir3);

impl Default for UpDirection {
    fn default() -> Self {
        Self(Dir3::Y)
    }
}

/// Marker component indicating that a [`VirtualCharacter`] is currently
/// grounded. Added or removed automatically during character movement.
///
/// A character is grounded when it is in contact with a surface whose angle to
/// the [`UpDirection`] is less than
/// [`SlopeHandling::max_ground_angle`], or when [`GroundSnapping`] found ground
/// within its snap distance.
#[derive(Component, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[component(storage = "SparseSet")]
pub struct Grounded;

/// Component for configuring slope handling behavior for [`VirtualCharacter`]s.
#[derive(Component, Clone, Debug)]
pub struct SlopeHandling {
    /// The maximum angle (in radians) where a surface is considered ground
    /// relative to the [`UpDirection`]. Outside this angle, surfaces are walls.
    ///
    /// **Default**: 45 degrees (π / 4 radians)
    pub max_ground_angle: f32,

    /// The maximum angle (in radians) where sliding along walls is allowed
    /// relative to the [`UpDirection`]. Walls steeper than this angle are
    /// considered vertical, and sliding along them will not affect vertical
    /// movement.
    ///
    /// **Default**: 75 degrees (5π / 12 radians)
    pub max_wall_slide_angle: f32,

    /// Whether the character should slide down along slopes when colliding with
    /// them.
    ///
    /// **Default**: `false`. Not applied by this controller yet.
    pub slide_on_ground: bool,

    /// Whether the character should slide up along ceilings when colliding with
    /// them.
    ///
    /// **Default**: `true`. Not applied by this controller yet.
    pub slide_on_ceiling: bool,
}

impl Default for SlopeHandling {
    fn default() -> Self {
        Self {
            max_ground_angle: core::f32::consts::FRAC_PI_4, // 45 degrees
            max_wall_slide_angle: 75f32.to_radians(),       // 75 degrees
            slide_on_ground: false,
            slide_on_ceiling: true,
        }
    }
}

/// Component for configuring ground snapping for [`VirtualCharacter`]s.
///
/// Snapping keeps a character that started the step [`Grounded`] attached to the
/// ground when it moves down a slope or a small step: after the move, a
/// downward cast within [`max_snap_distance`](Self::max_snap_distance) pulls the
/// character back onto the surface.
///
/// Adding this component also adds [`SlopeHandling`] with default settings if it
/// is not already present.
#[derive(Component, Clone, Debug)]
#[require(SlopeHandling)]
pub struct GroundSnapping {
    /// The maximum distance from the ground within which snapping is performed.
    ///
    /// **Default**: `0.1`
    pub max_snap_distance: f32,

    /// If `true`, snapping is only performed when the character is moving
    /// downwards, so jumping is not cancelled.
    ///
    /// **Default**: `true`
    pub skip_upward_movement: bool,
}

impl Default for GroundSnapping {
    fn default() -> Self {
        Self {
            max_snap_distance: 0.1,
            skip_upward_movement: true,
        }
    }
}

/// Component for configuring stair stepping behavior for [`VirtualCharacter`]s.
///
/// **Not applied by this controller yet.** The component is part of the agreed
/// API; the step-up itself is still on the open list in the architecture review.
#[derive(Component, Clone, Debug)]
pub struct StairStepping {
    /// The maximum height of a stair that can be stepped onto, measured along
    /// the [`UpDirection`].
    ///
    /// **Default**: `0.3`
    pub max_height: f32,

    /// The minimum width of a stair that can be stepped onto, measured
    /// perpendicular to the [`UpDirection`].
    ///
    /// **Default**: `0.2`
    pub min_width: f32,
}

impl Default for StairStepping {
    fn default() -> Self {
        Self {
            max_height: 0.3,
            min_width: 0.2,
        }
    }
}
