//! Capsule/cylinder vs static geometry, moved in substeps with depenetration.
//!
//! This is the *reference* solver. It resolves the same Avian collider the
//! character carries against static boxes, using plain math instead of a
//! physics schedule, so the contact resolution can be reasoned about and
//! unit-tested on its own. The client runs Avian's `MoveAndSlide` (in
//! `crates/kcc`) over the same terrain.
//!
//! No step-up yet — see the `TODO` on [`move_and_slide`]. Without a step height
//! the controller cannot climb a 0.3 m ledge, which is fine while the test room
//! is flat.

use ::kcc::avian3d::prelude::Collider;
use bevy_math::Vec3;
use bevy_shape::Aabb3d;

use crate::world::{Aabb3dExt, StaticWorld};

/// The vertical, round body the solver works with, read out of the collider.
///
/// The solver models the collider as the segment `center ± half_height` with
/// `radius` around it. That is exact for [`Collider::capsule`] and the
/// circumscribing capsule for [`Collider::cylinder`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct RoundDims {
    radius: f32,
    /// Half length of the axis.
    half_height: f32,
}

impl RoundDims {
    /// Reads the `y`-aligned body out of an Avian collider.
    ///
    /// Any other shape returns `None`: the solver only models round,
    /// `y`-aligned bodies against boxes.
    fn from_collider(collider: &Collider) -> Option<Self> {
        let shape = collider.shape();
        if let Some(capsule) = shape.as_capsule() {
            return Some(Self {
                radius: capsule.radius,
                half_height: capsule.half_height(),
            });
        }
        let cylinder = shape.as_cylinder()?;
        Some(Self {
            radius: cylinder.radius,
            half_height: cylinder.half_height,
        })
    }

    /// The two ends of the axis; `center` is the body midpoint.
    fn segment(&self, center: Vec3) -> (Vec3, Vec3) {
        (
            Vec3::new(center.x, center.y - self.half_height, center.z),
            Vec3::new(center.x, center.y + self.half_height, center.z),
        )
    }

    /// Lowest point of the body.
    fn foot(&self, center: Vec3) -> f32 {
        center.y - self.half_height - self.radius
    }
}

#[derive(Clone, Debug)]
pub struct KccParams {
    /// Round body the solver moves: [`Collider::capsule`] or [`Collider::cylinder`].
    pub collider: Collider,
    pub skin: f32,
    /// How far below the feet a support surface may be and still count.
    pub ground_probe: f32,
    /// A contact whose normal is more vertical than this is treated as ground
    /// and pushes straight up; anything flatter is a wall and pushes out
    /// horizontally. Without the split the capsule rolls over the bottom edge
    /// of every wall, which looks like wall-climbing.
    pub max_slope_cos: f32,
    pub substeps: u8,
    pub iterations: u8,
}

impl Default for KccParams {
    fn default() -> Self {
        Self {
            collider: crate::player_collider(),
            skin: 0.001,
            ground_probe: 0.15,
            max_slope_cos: 0.6,
            substeps: 4,
            iterations: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveOutput {
    pub position: Vec3,
    pub velocity: Vec3,
    pub grounded: bool,
    /// Contacts resolved this tick, for diagnostics.
    pub contacts: u8,
}

/// Moves `start` by `velocity * dt`, sliding along static geometry.
///
/// `start` and the returned position are the body *centre*, not the feet.
///
/// TODO: step-up. A ledge within `step_height` should be climbed rather than
/// blocked; that needs a retry from `start + up * step_height` when the
/// horizontal move is rejected.
pub fn move_and_slide(
    world: &StaticWorld,
    params: &KccParams,
    start: Vec3,
    velocity: Vec3,
    dt: f32,
) -> MoveOutput {
    let body = RoundDims::from_collider(&params.collider);
    debug_assert!(
        body.is_some(),
        "KccParams::collider must be a capsule or a cylinder"
    );
    let Some(body) = body else {
        // Nothing sensible to solve against, so leave the pose alone.
        return MoveOutput {
            position: start,
            velocity,
            grounded: false,
            contacts: 0,
        };
    };

    let mut pos = start;
    let mut vel = velocity;
    let substeps = params.substeps.max(1);
    let dt_sub = dt / substeps as f32;
    let mut contacts = 0u8;

    for _ in 0..substeps {
        pos += vel * dt_sub;

        for _ in 0..params.iterations {
            let mut deepest: Option<(Vec3, f32)> = None;
            for b in world.boxes() {
                if let Some((n, depth)) = contact(&params.collider, b, pos)
                    && deepest.is_none_or(|(_, d)| depth > d)
                {
                    deepest = Some((n, depth));
                }
            }

            match deepest {
                Some((n, depth)) => {
                    let push = if n.y >= params.max_slope_cos {
                        // Supporting surface: move up only, never sideways.
                        Vec3::new(0.0, n.y * (depth + params.skin), 0.0)
                    } else {
                        // Wall: strip the vertical component so a corner contact
                        // cannot lever the capsule upwards.
                        let flat = Vec3::new(n.x, 0.0, n.z);
                        let len = flat.length();
                        if len > 1e-5 {
                            flat / len * (depth + params.skin)
                        } else {
                            n * (depth + params.skin)
                        }
                    };
                    pos += push;
                    let vn = vel.dot(n);
                    if vn < 0.0 {
                        vel -= n * vn;
                    }
                    contacts = contacts.saturating_add(1);
                }
                None => break,
            }
        }
    }

    let support = ground_probe(world, &params.collider, pos, params.ground_probe);
    let grounded = support.is_some();
    if grounded {
        if vel.y < 0.0 {
            vel.y = 0.0;
        }
        // Snap to the surface so walking downhill cannot accumulate a gap.
        if velocity.y <= 0.0
            && let Some(top) = support
        {
            pos.y = top + body.half_height + body.radius;
        }
    }

    MoveOutput {
        position: pos,
        velocity: vel,
        grounded,
        contacts,
    }
}

/// Overlap between the body and one box, as `(outward normal, depth)`.
///
/// One contact per box: at a box corner the normal is the corner-to-axis
/// direction, so a body driven exactly along that diagonal is stopped rather
/// than deflected. That is correct for a sharp corner; content that wants
/// rounded corners should bevel the collision geometry.
///
/// Returns `None` if the collider is not round or they do not overlap.
pub fn contact(collider: &Collider, aabb: &Aabb3d, pos: Vec3) -> Option<(Vec3, f32)> {
    let body = RoundDims::from_collider(collider)?;
    let (a, b) = body.segment(pos);
    let on_box = aabb.closest_to_vertical_segment(pos.x, pos.z, a.y, b.y);
    let sy = on_box.y.clamp(a.y, b.y);
    let on_seg = Vec3::new(pos.x, sy, pos.z);

    let delta = on_box - on_seg;
    let dist = delta.length();
    if dist > 1e-5 {
        return (dist < body.radius).then(|| (-delta / dist, body.radius - dist));
    }

    // The axis is inside the box. Eject through the cheapest face.
    let cap_min = Vec3::new(
        pos.x - body.radius,
        pos.y - body.half_height - body.radius,
        pos.z - body.radius,
    );
    let cap_max = Vec3::new(
        pos.x + body.radius,
        pos.y + body.half_height + body.radius,
        pos.z + body.radius,
    );

    let faces = [
        (Vec3::NEG_X, cap_max.x - aabb.min.x),
        (Vec3::X, aabb.max.x - cap_min.x),
        (Vec3::NEG_Y, cap_max.y - aabb.min.y),
        (Vec3::Y, aabb.max.y - cap_min.y),
        (Vec3::NEG_Z, cap_max.z - aabb.min.z),
        (Vec3::Z, aabb.max.z - cap_min.z),
    ];

    faces
        .into_iter()
        .filter(|(_, d)| *d > 0.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Highest surface that could support the body within `probe` metres.
///
/// Returns `None` if the collider is not round or nothing is in reach.
pub fn ground_probe(
    world: &StaticWorld,
    collider: &Collider,
    pos: Vec3,
    probe: f32,
) -> Option<f32> {
    let body = RoundDims::from_collider(collider)?;
    let foot = body.foot(pos);
    let footprint = body.radius * 0.9;
    let mut best: Option<f32> = None;

    for b in world.boxes() {
        let top = b.top();
        if top > foot + 0.02 || top < foot - probe {
            continue;
        }
        if b.horizontal_distance_sq(pos.x, pos.z) <= footprint * footprint {
            best = Some(best.map_or(top, |t: f32| t.max(top)));
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CAPSULE_LENGTH, PLAYER_RADIUS, RESTING_HEIGHT};

    #[test]
    fn reads_the_body_out_of_the_collider() {
        let params = KccParams::default();

        let body = RoundDims::from_collider(&params.collider).expect("default is a capsule");
        assert!((body.radius - PLAYER_RADIUS).abs() < 1e-6);
        assert!((body.half_height - CAPSULE_LENGTH * 0.5).abs() < 1e-6);

        let cylinder = RoundDims::from_collider(&Collider::cylinder(0.5, 2.0))
            .expect("cylinders are modelled as their circumscribing capsule");
        assert!((cylinder.radius - 0.5).abs() < 1e-6);
        assert!((cylinder.half_height - 1.0).abs() < 1e-6);

        assert!(RoundDims::from_collider(&Collider::cuboid(1.0, 1.0, 1.0)).is_none());
    }

    #[test]
    fn falls_onto_the_floor_and_rests_on_it() {
        let params = KccParams::default();
        let world = StaticWorld::test_room();
        let dt = 1.0 / 60.0;
        let mut pos = Vec3::new(0.0, 3.0, 0.0);
        let mut vel = Vec3::ZERO;
        let mut grounded = false;

        for _ in 0..180 {
            let out = move_and_slide(&world, &params, pos, vel, dt);
            pos = out.position;
            // Gravity, as the shared movement model applies it each tick.
            vel = Vec3::new(0.0, out.velocity.y - 24.0 * dt, 0.0);
            grounded = out.grounded;
        }

        assert!(grounded, "character should be resting on the floor");
        assert!(
            (pos.y - RESTING_HEIGHT).abs() < 0.02,
            "expected resting height {RESTING_HEIGHT}, got {}",
            pos.y
        );
    }
}
