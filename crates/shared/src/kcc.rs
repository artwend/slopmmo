//! Capsule vs static geometry, moved in substeps with depenetration.
//!
//! This is the engine-free *reference* solver. The client uses
//! `crates/kcc` instead, which runs Avian's `MoveAndSlide` over the same
//! terrain; keeping this one pure is what lets `sim`'s tests and the server's
//! bot benchmark run without a physics engine.
//!
//! No step-up yet — see the `TODO` on [`move_and_slide`]. Without a step height
//! the controller cannot climb a 0.3 m ledge, which is fine while the test room
//! is flat.

use bevy_math::Vec3;
use bevy_shape::Aabb3d;

use crate::world::{Aabb3dExt, StaticWorld};
use crate::{PLAYER_HEIGHT, PLAYER_RADIUS};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capsule {
    pub radius: f32,
    /// Half length of the cylindrical section, excluding both caps.
    pub half_height: f32,
}

impl Capsule {
    pub fn from_height(radius: f32, height: f32) -> Self {
        Self {
            radius,
            half_height: (height * 0.5 - radius).max(0.0),
        }
    }

    /// The two ends of the capsule axis; `center` is the capsule midpoint.
    pub fn segment(&self, center: Vec3) -> (Vec3, Vec3) {
        (
            Vec3::new(center.x, center.y - self.half_height, center.z),
            Vec3::new(center.x, center.y + self.half_height, center.z),
        )
    }

    /// Lowest point of the capsule.
    pub fn foot(&self, center: Vec3) -> f32 {
        center.y - self.half_height - self.radius
    }

    pub fn height(&self) -> f32 {
        (self.half_height + self.radius) * 2.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct KccParams {
    pub capsule: Capsule,
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
            capsule: Capsule::from_height(PLAYER_RADIUS, PLAYER_HEIGHT),
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
/// `start` and the returned position are the capsule *centre*, not the feet.
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
                if let Some((n, depth)) = contact(&params.capsule, b, pos)
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

    let support = ground_probe(world, &params.capsule, pos, params.ground_probe);
    let grounded = support.is_some();
    if grounded {
        if vel.y < 0.0 {
            vel.y = 0.0;
        }
        // Snap to the surface so walking downhill cannot accumulate a gap.
        if velocity.y <= 0.0
            && let Some(top) = support
        {
            pos.y = top + params.capsule.half_height + params.capsule.radius;
        }
    }

    MoveOutput {
        position: pos,
        velocity: vel,
        grounded,
        contacts,
    }
}

/// Overlap between the capsule and one box, as `(outward normal, depth)`.
///
/// One contact per box: at a box corner the normal is the corner-to-axis
/// direction, so a capsule driven exactly along that diagonal is stopped rather
/// than deflected. That is correct for a sharp corner; content that wants
/// rounded corners should bevel the collision geometry.
pub fn contact(capsule: &Capsule, aabb: &Aabb3d, pos: Vec3) -> Option<(Vec3, f32)> {
    let (a, b) = capsule.segment(pos);
    let on_box = aabb.closest_to_vertical_segment(pos.x, pos.z, a.y, b.y);
    let sy = on_box.y.clamp(a.y, b.y);
    let on_seg = Vec3::new(pos.x, sy, pos.z);

    let delta = on_box - on_seg;
    let dist = delta.length();
    if dist > 1e-5 {
        return (dist < capsule.radius).then(|| (-delta / dist, capsule.radius - dist));
    }

    // The axis is inside the box. Eject through the cheapest face.
    let cap_min = Vec3::new(
        pos.x - capsule.radius,
        pos.y - capsule.half_height - capsule.radius,
        pos.z - capsule.radius,
    );
    let cap_max = Vec3::new(
        pos.x + capsule.radius,
        pos.y + capsule.half_height + capsule.radius,
        pos.z + capsule.radius,
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

/// Highest surface that could support the capsule within `probe` metres.
pub fn ground_probe(world: &StaticWorld, capsule: &Capsule, pos: Vec3, probe: f32) -> Option<f32> {
    let foot = capsule.foot(pos);
    let footprint = capsule.radius * 0.9;
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