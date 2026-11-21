//! Uniform grid broadphase.
//!
//! Rebuilt wholesale every tick for the skeleton. The obvious optimisation —
//! incremental updates for cells whose occupants did not move — is measured
//! before it is written; see the review's note about solving every tick and
//! buying CPU back from the broadphase instead of from the solve rate.

use std::collections::HashMap;

use bevy_math::Vec3;

#[derive(Clone, Debug)]
pub struct SpatialGrid {
    cell: f32,
    buckets: HashMap<(i32, i32), Vec<u32>>,
    scratch_cells: usize,
}

impl SpatialGrid {
    /// `cell` should be at least the largest expected query diameter.
    pub fn new(cell: f32) -> Self {
        Self {
            cell: cell.max(0.25),
            buckets: HashMap::new(),
            scratch_cells: 0,
        }
    }

    pub fn cell_size(&self) -> f32 {
        self.cell
    }

    pub fn occupied_cells(&self) -> usize {
        self.buckets.len()
    }

    fn key(&self, p: Vec3) -> (i32, i32) {
        (
            (p.x / self.cell).floor() as i32,
            (p.z / self.cell).floor() as i32,
        )
    }

    pub fn rebuild(&mut self, positions: &[Vec3]) {
        for v in self.buckets.values_mut() {
            v.clear();
        }
        for (i, p) in positions.iter().enumerate() {
            let k = self.key(*p);
            self.buckets.entry(k).or_default().push(i as u32);
        }
        self.scratch_cells = self.buckets.len();
    }

    /// Indices of every body whose cell intersects the query circle. Not exact:
    /// the caller still applies the distance test.
    pub fn candidates(&self, pos: Vec3, radius: f32, out: &mut Vec<u32>) {
        out.clear();
        let span = (radius / self.cell).ceil() as i32;
        let (cx, cz) = self.key(pos);
        for dz in -span..=span {
            for dx in -span..=span {
                if let Some(v) = self.buckets.get(&(cx + dx, cz + dz)) {
                    out.extend_from_slice(v);
                }
            }
        }
    }
}
