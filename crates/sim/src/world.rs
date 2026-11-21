//! Static geometry the client collides against.
//!
//! Deliberately boring: axis-aligned boxes only. The client's real terrain will
//! come from a navmesh/trimesh through Avian; this exists so the skeleton can be
//! tested end to end without an asset pipeline.

use bevy_math::Vec3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn new(center: Vec3, half: Vec3) -> Self {
        Self {
            min: center - half,
            max: center + half,
        }
    }

    pub fn from_min_max(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    pub fn top(&self) -> f32 {
        self.max.y
    }

    /// Closest point on the box to the vertical segment at `(x, z)` spanning
    /// `y0..=y1`.
    pub fn closest_to_vertical_segment(&self, x: f32, z: f32, y0: f32, y1: f32) -> Vec3 {
        let cx = x.clamp(self.min.x, self.max.x);
        let cz = z.clamp(self.min.z, self.max.z);
        let cy = if y1 < self.min.y {
            self.min.y
        } else if y0 > self.max.y {
            self.max.y
        } else {
            (y0.max(self.min.y) + y1.min(self.max.y)) * 0.5
        };
        Vec3::new(cx, cy, cz)
    }

    /// Squared horizontal distance from `(x, z)` to the box footprint.
    pub fn horizontal_distance_sq(&self, x: f32, z: f32) -> f32 {
        let dx = (self.min.x - x).max(0.0).max(x - self.max.x);
        let dz = (self.min.z - z).max(0.0).max(z - self.max.z);
        dx * dx + dz * dz
    }

    pub fn overlaps_y(&self, lo: f32, hi: f32) -> bool {
        hi > self.min.y && lo < self.max.y
    }
}

#[derive(Clone, Debug, Default)]
pub struct StaticWorld {
    boxes: Vec<Aabb>,
}

impl StaticWorld {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, b: Aabb) -> &mut Self {
        self.boxes.push(b);
        self
    }

    pub fn boxes(&self) -> &[Aabb] {
        &self.boxes
    }

    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// Flat floor, four walls and one pillar at the origin, 20×20 m.
    pub fn test_room() -> Self {
        let mut w = Self::new();
        // Floor slab: top surface at y = 0.
        w.add(Aabb::new(
            Vec3::new(0.0, -0.5, 0.0),
            Vec3::new(12.0, 0.5, 12.0),
        ));
        let h = 2.0;
        let t = 0.5;
        w.add(Aabb::new(Vec3::new(0.0, h, -10.0), Vec3::new(10.0, h, t)));
        w.add(Aabb::new(Vec3::new(0.0, h, 10.0), Vec3::new(10.0, h, t)));
        w.add(Aabb::new(Vec3::new(-10.0, h, 0.0), Vec3::new(t, h, 10.0)));
        w.add(Aabb::new(Vec3::new(10.0, h, 0.0), Vec3::new(t, h, 10.0)));
        // Pillar to slide around.
        w.add(Aabb::new(
            Vec3::new(2.0, 1.5, 2.0),
            Vec3::new(0.5, 1.5, 0.5),
        ));
        w
    }
}
