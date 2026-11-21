//! Server-side capsule vs capsule solve.
//!
//! No terrain, no Avian, no rigid bodies. Two things this deliberately does
//! differently from the dialogue it replaces:
//!
//! 1. The impulse cancels **relative normal velocity** (inelastic contact)
//!    instead of adding a fixed push. A fixed 1 m/s push cannot keep up with a
//!    3.5 m/s walk, so players stay interpenetrated forever.
//! 2. The per-player budget is applied to the **vector sum** of impulses, not to
//!    each pair. Eight neighbours otherwise add up to eight times the limit.

use bevy_math::Vec3;

use crate::spatial::SpatialGrid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Not participating this tick.
    None,
    /// Pushed apart, may interpenetrate briefly.
    Soft,
    /// Blocks movement outright.
    Hard,
}

impl Mode {
    pub fn as_wire(self) -> u8 {
        match self {
            Self::None => proto::peer_mode::NONE,
            Self::Soft => proto::peer_mode::SOFT,
            Self::Hard => proto::peer_mode::HARD,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub id: u64,
    pub pos: Vec3,
    pub vel: Vec3,
    pub radius: f32,
    pub half_height: f32,
    /// 1.0 for players, 0.0 for immovable blockers and bosses.
    pub inv_mass: f32,
    pub mode: Mode,
}

impl Body {
    pub fn player(id: u64, pos: Vec3, vel: Vec3) -> Self {
        Self {
            id,
            pos,
            vel,
            radius: proto::PLAYER_RADIUS,
            half_height: proto::PLAYER_HEIGHT * 0.5,
            inv_mass: 1.0,
            mode: Mode::Soft,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Pair {
    pub a: u32,
    pub b: u32,
    /// Horizontal, pointing from `a` to `b`.
    pub normal: Vec3,
    pub distance: f32,
    pub penetration: f32,
    pub mode: Mode,
}

/// Depth bias: converts residual overlap into separation speed.
pub const PENETRATION_BIAS: f32 = 6.0;
/// Upper bound on the separation speed the bias may inject, so a deep overlap
/// resolves over several ticks instead of exploding.
pub const PENETRATION_BIAS_MAX: f32 = 1.5;

/// Nearest-neighbour candidate search, de-duplicated, capped per player.
///
/// Both a radius and a count limit are applied: `nearest K` alone would create
/// contacts across an empty field.
pub fn build_pairs(
    bodies: &[Body],
    grid: &SpatialGrid,
    positions: &[Vec3],
    neighbour_limit: usize,
    pairs_per_body: usize,
) -> Vec<Pair> {
    let mut candidates: Vec<u32> = Vec::with_capacity(32);
    let mut nearest: Vec<(f32, u32)> = Vec::with_capacity(32);
    let mut raw: Vec<Pair> = Vec::new();
    let search_radius = proto::NEIGHBOUR_RADIUS + proto::PLAYER_RADIUS * 2.0;

    for (i, body) in bodies.iter().enumerate() {
        if body.mode == Mode::None {
            continue;
        }
        grid.candidates(positions[i], search_radius, &mut candidates);

        nearest.clear();
        for &j in &candidates {
            let j = j as usize;
            if j == i || bodies[j].mode == Mode::None {
                continue;
            }
            if !vertical_overlap(body, &bodies[j]) {
                continue;
            }
            let d = horizontal_distance(body.pos, bodies[j].pos);
            let reach = body.radius + bodies[j].radius;
            if d > reach + proto::SOFT_MARGIN || d > search_radius {
                continue;
            }
            nearest.push((d, j as u32));
        }

        nearest.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        nearest.truncate(neighbour_limit);

        for &(d, j) in &nearest {
            // Each contact is discovered from both sides; keep the one where the
            // scanning body has the lower index so the list is de-duplicated.
            if (i as u32) > j {
                continue;
            }
            let (lo, hi) = if (i as u32) < j {
                (i as u32, j)
            } else {
                (j, i as u32)
            };
            let a = &bodies[lo as usize];
            let b = &bodies[hi as usize];
            let delta = b.pos - a.pos;
            let horizontal = Vec3::new(delta.x, 0.0, delta.z);
            let len = horizontal.length();
            // Perfectly coincident centres: pick a deterministic direction.
            let normal = if len > 1e-4 {
                horizontal / len
            } else {
                Vec3::new(1.0, 0.0, 0.0)
            };
            let reach = a.radius + b.radius;
            raw.push(Pair {
                a: lo,
                b: hi,
                normal,
                distance: d,
                penetration: (reach - d).max(0.0),
                mode: contact_mode(a, b),
            });
        }
    }

    // Deepest first, then id order: stable and independent of hash iteration.
    raw.sort_by(|x, y| {
        y.penetration
            .total_cmp(&x.penetration)
            .then(x.a.cmp(&y.a))
            .then(x.b.cmp(&y.b))
    });

    let mut load = vec![0u16; bodies.len()];
    let mut out = Vec::with_capacity(raw.len());
    for p in raw {
        if load[p.a as usize] as usize >= pairs_per_body
            || load[p.b as usize] as usize >= pairs_per_body
        {
            continue;
        }
        load[p.a as usize] += 1;
        load[p.b as usize] += 1;
        out.push(p);
    }
    out
}

fn vertical_overlap(a: &Body, b: &Body) -> bool {
    let alo = a.pos.y - a.half_height;
    let ahi = a.pos.y + a.half_height;
    let blo = b.pos.y - b.half_height;
    let bhi = b.pos.y + b.half_height;
    ahi > blo && alo < bhi
}

fn horizontal_distance(a: Vec3, b: Vec3) -> f32 {
    ((b.x - a.x).powi(2) + (b.z - a.z).powi(2)).sqrt()
}

fn contact_mode(a: &Body, b: &Body) -> Mode {
    if a.mode == Mode::Hard || b.mode == Mode::Hard {
        Mode::Hard
    } else if a.mode == Mode::Soft && b.mode == Mode::Soft {
        Mode::Soft
    } else {
        Mode::None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Impulse {
    pub delta_v: Vec3,
    pub hard_normal: Option<Vec3>,
    pub hard: bool,
}

/// Solves every pair once and returns per-body velocity deltas.
///
/// The result is a pure function of `(bodies, pairs, budget)`, so the same call
/// can be replayed for determinism tests.
pub fn solve(bodies: &[Body], pairs: &[Pair], budget: f32) -> Vec<Impulse> {
    let mut out = vec![Impulse::default(); bodies.len()];

    for p in pairs {
        let a = &bodies[p.a as usize];
        let b = &bodies[p.b as usize];
        if p.mode == Mode::None {
            continue;
        }

        let inv_sum = a.inv_mass + b.inv_mass;
        if inv_sum <= f32::EPSILON {
            continue;
        }

        let n = p.normal;
        let rel_vn = (b.vel - a.vel).dot(n);

        // Inelastic contact: cancel the closing velocity exactly.
        let mut j = 0.0;
        if rel_vn < 0.0 {
            j = -rel_vn / inv_sum;
        }
        // Residual overlap is converted into separation speed, capped.
        if p.penetration > 0.0 {
            j += (p.penetration * PENETRATION_BIAS).min(PENETRATION_BIAS_MAX) / inv_sum;
        }

        if j > 0.0 {
            out[p.a as usize].delta_v -= n * (j * a.inv_mass);
            out[p.b as usize].delta_v += n * (j * b.inv_mass);
        }

        if p.mode == Mode::Hard {
            out[p.a as usize].hard = true;
            out[p.a as usize].hard_normal = Some(-n);
            out[p.b as usize].hard = true;
            out[p.b as usize].hard_normal = Some(n);
        }
    }

    // Vector-sum budget, applied once per body.
    for imp in &mut out {
        let len = imp.delta_v.length();
        if len > budget && len > f32::EPSILON {
            imp.delta_v *= budget / len;
        }
    }

    out
}
