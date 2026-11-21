//! The tick is the only code that touches every player sixty times a second, so
//! it must not allocate. This binary installs a counting allocator and asserts
//! the steady state touches it zero times.
//!
//! Deliberately a single `#[test]`: a second test would run on another thread of
//! the same process and its allocations would land in the same counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use bevy_math::Vec3;

use sim::authority::{SimConfig, Simulation};
use sim::world::StaticWorld;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every request to the system allocator unchanged, so the
// returned pointers keep their original layout and provenance.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Eight touching pairs, laid out so that every pair shares a cell and the grid
/// window never changes shape. Each pair sits just inside the soft margin, so the
/// broadphase produces real pairs while the solve sees zero penetration and zero
/// closing velocity — the bodies never move, which keeps the measurement about
/// the allocator and not about drift.
fn build_sim() -> Simulation {
    let mut sim = Simulation::new(SimConfig::default());
    for i in 0..8u64 {
        let x = -8.0 + i as f32 * 2.0;
        sim.add_player(i * 2 + 1, Vec3::new(x, 0.9, 0.0));
        sim.add_player(i * 2 + 2, Vec3::new(x + 0.84, 0.9, 0.0));
    }
    sim
}

#[test]
fn the_steady_state_tick_does_not_allocate() {
    let world = StaticWorld::test_room();
    let mut sim = build_sim();

    // Warm up: first-use growth of the grid tables, the pair buffers, the body
    // scratch and the snapshot buffer all happen here.
    for _ in 0..8 {
        sim.step(&world);
    }

    assert_eq!(
        sim.stats.pairs, 8,
        "the measured window must exercise the pair path"
    );
    assert_eq!(
        sim.stats.corrections, 0,
        "the fixture must not move, or the grid window could grow mid-measurement"
    );

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    // Proves the counter is live: the warm-up must have allocated, otherwise a
    // zero delta below would mean nothing.
    assert!(before > 0, "the allocator counter never fired");
    for _ in 0..64 {
        sim.step(&world);
    }
    let spent = ALLOCATIONS.load(Ordering::Relaxed) - before;

    assert_eq!(
        spent, 0,
        "the tick allocated {spent} times over 64 steady-state ticks"
    );
    assert_eq!(sim.len(), 16);
    assert_eq!(sim.snapshots().len(), 16);
}
