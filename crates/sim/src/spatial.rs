//! Uniform grid broadphase, rebuilt wholesale every tick.
//!
//! The grid is touched once per body per tick and once per query cell, so it is
//! laid out as a CSR (compressed sparse row) table — one flat `items` array
//! ordered by cell, plus a `starts` prefix table — instead of the
//! `HashMap<(i32, i32), Vec<u32>>` it replaces:
//!
//! * no hashing per inserted body and no hashing per queried cell,
//! * no per-cell heap `Vec`; the whole grid is one buffer set, allocated once and
//!   reused by every rebuild,
//! * the candidate scan walks contiguous memory in cell order.
//!
//! Rebuild is a counting sort, so it is O(bodies + cells) with no comparisons.
//! The obvious further optimisation — incremental updates for cells whose
//! occupants did not move — is measured before it is written; see
//! `docs/architecture-review.md` about buying CPU back from the broadphase
//! instead of from the solve rate.

use bevy_math::Vec3;

/// Cap on cells along one axis. A body outside the resulting window is clamped
/// into the border cell; the exact distance test in the caller rejects the
/// extra candidates, and one stray coordinate can never blow up the grid's
/// memory.
const MAX_CELLS_PER_AXIS: usize = 128;

#[derive(Clone, Debug)]
pub struct SpatialGrid {
    cell: f32,
    inv_cell: f32,
    /// Cell coordinate of the grid's (0, 0) corner.
    origin: (i32, i32),
    /// Occupied window, in cells.
    span: (usize, usize),
    occupied: usize,
    /// Per-body cell coordinate, then local cell index. Both are scratch.
    coords: Vec<(i32, i32)>,
    cells: Vec<u32>,
    /// Prefix table: cell `c` owns `items[starts[c]..starts[c + 1]]`.
    starts: Vec<u32>,
    cursor: Vec<u32>,
    items: Vec<u32>,
}

impl SpatialGrid {
    /// `cell` should be at least the largest expected query diameter.
    pub fn new(cell: f32) -> Self {
        let cell = cell.max(0.25);
        Self {
            cell,
            inv_cell: 1.0 / cell,
            origin: (0, 0),
            span: (0, 0),
            occupied: 0,
            coords: Vec::new(),
            cells: Vec::new(),
            starts: Vec::new(),
            cursor: Vec::new(),
            items: Vec::new(),
        }
    }

    pub fn cell_size(&self) -> f32 {
        self.cell
    }

    /// Cells holding at least one body. Diagnostic only: the broadphase does not
    /// need it, but a soak test can watch it to catch bodies drifting apart.
    pub fn occupied_cells(&self) -> usize {
        self.occupied
    }

    fn key(&self, p: Vec3) -> (i32, i32) {
        (
            (p.x * self.inv_cell).floor() as i32,
            (p.z * self.inv_cell).floor() as i32,
        )
    }

    /// Rebuilds the grid from scratch. Only the first rebuild for a given body
    /// count and world extent allocates.
    pub fn rebuild(&mut self, positions: &[Vec3]) {
        self.cells.clear();
        self.items.clear();
        self.coords.clear();
        self.occupied = 0;
        self.span = (0, 0);
        if positions.is_empty() {
            self.starts.clear();
            return;
        }

        let mut lo = (i32::MAX, i32::MAX);
        let mut hi = (i32::MIN, i32::MIN);
        for p in positions {
            let c = self.key(*p);
            lo = (lo.0.min(c.0), lo.1.min(c.1));
            hi = (hi.0.max(c.0), hi.1.max(c.1));
            self.coords.push(c);
        }

        // i64: `key` saturates for absurd coordinates, and the difference of two
        // saturated values would overflow i32.
        let span_x = span_of(lo.0, hi.0);
        let span_z = span_of(lo.1, hi.1);
        self.span = (span_x, span_z);
        self.origin = lo;
        let cells = span_x * span_z;

        self.cells.resize(positions.len(), 0);
        for (i, c) in self.coords.iter().enumerate() {
            let x = (c.0 as i64 - lo.0 as i64).clamp(0, span_x as i64 - 1);
            let z = (c.1 as i64 - lo.1 as i64).clamp(0, span_z as i64 - 1);
            self.cells[i] = z as u32 * span_x as u32 + x as u32;
        }

        // Counting sort: count per cell, prefix sum, scatter.
        self.starts.clear();
        self.starts.resize(cells + 1, 0);
        for &c in &self.cells {
            self.starts[c as usize + 1] += 1;
        }
        for i in 1..=cells {
            self.starts[i] += self.starts[i - 1];
        }
        self.occupied = self.starts[..cells]
            .iter()
            .zip(&self.starts[1..])
            .filter(|(a, b)| a != b)
            .count();

        self.cursor.clear();
        self.cursor.extend_from_slice(&self.starts[..cells]);
        self.items.resize(positions.len(), 0);
        for (i, &c) in self.cells.iter().enumerate() {
            let slot = &mut self.cursor[c as usize];
            self.items[*slot as usize] = i as u32;
            *slot += 1;
        }
    }

    /// Indices of every body whose cell intersects the query circle. Not exact:
    /// the caller still applies the distance test.
    pub fn candidates(&self, pos: Vec3, radius: f32, out: &mut Vec<u32>) {
        out.clear();
        let (span_x, span_z) = self.span;
        if span_x == 0 || span_z == 0 {
            return;
        }
        // Never scan further than the grid is wide: a radius larger than the
        // whole world would otherwise loop over empty coordinates.
        let reach = ((radius * self.inv_cell).ceil() as i64).min(span_x.max(span_z) as i64);
        // Clamp the query into the window the same way `rebuild` clamps bodies,
        // so a query from outside a saturated grid still finds the border cell
        // its body was clamped into.
        let (cx, cz) = self.key(pos);
        let qx = (cx as i64 - self.origin.0 as i64).clamp(0, span_x as i64 - 1);
        let qz = (cz as i64 - self.origin.1 as i64).clamp(0, span_z as i64 - 1);
        for dz in -reach..=reach {
            let z = qz + dz;
            if z < 0 || z >= span_z as i64 {
                continue;
            }
            let row = z as usize * span_x;
            for dx in -reach..=reach {
                let x = qx + dx;
                if x < 0 || x >= span_x as i64 {
                    continue;
                }
                let cell = row + x as usize;
                let (start, end) = (self.starts[cell] as usize, self.starts[cell + 1] as usize);
                out.extend_from_slice(&self.items[start..end]);
            }
        }
    }
}

fn span_of(lo: i32, hi: i32) -> usize {
    (hi as i64 - lo as i64 + 1).clamp(1, MAX_CELLS_PER_AXIS as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_of(points: &[Vec3]) -> SpatialGrid {
        let mut grid = SpatialGrid::new(4.0);
        grid.rebuild(points);
        grid
    }

    #[test]
    fn every_body_is_reachable_from_its_own_cell() {
        let points: Vec<Vec3> = (0..16)
            .map(|i| Vec3::new(i as f32 * 1.5 - 12.0, 0.9, (i % 5) as f32 * 2.0))
            .collect();
        let grid = grid_of(&points);
        let mut out = Vec::new();
        for (i, p) in points.iter().enumerate() {
            grid.candidates(*p, 0.1, &mut out);
            assert!(out.contains(&(i as u32)), "body {i} missing from its cell");
        }
    }

    #[test]
    fn a_distant_body_is_not_a_candidate() {
        let points = vec![Vec3::new(0.0, 0.9, 0.0), Vec3::new(100.0, 0.9, 0.0)];
        let grid = grid_of(&points);
        let mut out = Vec::new();
        grid.candidates(points[0], 2.8, &mut out);
        assert_eq!(out, vec![0]);

        grid.candidates(points[1], 2.8, &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn absurd_coordinates_cannot_overflow_the_window() {
        let points = vec![
            Vec3::new(0.0, 0.9, 0.0),
            Vec3::new(3.0e38, 0.9, -3.0e38),
            Vec3::new(f32::INFINITY, 0.0, f32::NAN),
        ];
        let grid = grid_of(&points);
        assert!(
            grid.span.0 <= MAX_CELLS_PER_AXIS && grid.span.1 <= MAX_CELLS_PER_AXIS,
            "window escaped its cap: {:?}",
            grid.span
        );
        let mut out = Vec::new();
        grid.candidates(points[0], 2.8, &mut out);
        assert!(out.contains(&0), "finite body lost to a stray coordinate");
    }

    #[test]
    fn rebuilding_reuses_its_buffers() {
        let points: Vec<Vec3> = (0..8).map(|i| Vec3::new(i as f32, 0.9, 0.0)).collect();
        let mut grid = SpatialGrid::new(4.0);
        grid.rebuild(&points);
        let capacity = (
            grid.items.capacity(),
            grid.starts.capacity(),
            grid.cells.capacity(),
            grid.coords.capacity(),
        );
        for _ in 0..4 {
            grid.rebuild(&points);
        }
        assert_eq!(
            capacity,
            (
                grid.items.capacity(),
                grid.starts.capacity(),
                grid.cells.capacity(),
                grid.coords.capacity(),
            ),
            "rebuild reallocated its buffers"
        );
        assert_eq!(grid.items.len(), points.len());
    }
}
