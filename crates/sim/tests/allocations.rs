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

/// Eight overlapping pairs, laid out so that every pair shares a cell and the
/// grid window never changes shape. The 0.2 m penetration means every tick emits
/// a correction for all sixteen players, which is the busiest path in the
/// benchmark and exactly where an allocation would show up. The bodies are
/// re-armed below, because the solve separates them: without that the fixture
/// would stop exercising correction emission after a few ticks.
fn build_sim() -> Simulation {
    let mut sim = Simulation::new(SimConfig::default());
    for i in 0..8u64 {
        let x = -8.0 + i as f32 * 2.0;
        sim.add_player(i * 2 + 1, Vec3::new(x, 0.9, 0.0));
        sim.add_player(i * 2 + 2, Vec3::new(x + 0.6, 0.9, 0.0));
    }
    arm(&mut sim);
    sim
}

/// Restores the penetrating pose. Field writes only: this must not allocate, or
/// the measurement would count the fixture instead of the tick.
fn arm(sim: &mut Simulation) {
    for i in 0..8u64 {
        let x = -8.0 + i as f32 * 2.0;
        for (id, offset) in [(i * 2 + 1, 0.0), (i * 2 + 2, 0.6)] {
            let player = sim.player_mut(id).expect("player missing from fixture");
            player.pos = Vec3::new(x + offset, 0.9, 0.0);
            player.vel = Vec3::ZERO;
        }
    }
}

#[test]
fn the_steady_state_tick_does_not_allocate() {
    let world = StaticWorld::test_room();
    let mut sim = build_sim();

    // Warm up: first-use growth of the grid tables, the pair buffers, the body
    // scratch and the snapshot buffer all happen here. Re-arming keeps the
    // correction path hot during the warm-up too.
    for _ in 0..8 {
        arm(&mut sim);
        sim.step(&world);
    }

    assert_eq!(
        sim.stats.pairs, 8,
        "the measured window must exercise the pair path"
    );
    assert_eq!(
        sim.stats.corrections,
        8 * 16,
        "every warm-up tick must emit a correction for all sixteen players"
    );

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    // Proves the counter is live: the warm-up must have allocated, otherwise a
    // zero delta below would mean nothing.
    assert!(before > 0, "the allocator counter never fired");
    let corrections_before = sim.stats.corrections;
    for _ in 0..64 {
        arm(&mut sim);
        sim.step(&world);
    }
    let spent = ALLOCATIONS.load(Ordering::Relaxed) - before;

    assert_eq!(
        spent, 0,
        "the tick allocated {spent} times over 64 steady-state ticks"
    );
    assert_eq!(
        sim.stats.corrections - corrections_before,
        64 * 16,
        "the measured window stopped exercising correction emission"
    );
    assert_eq!(sim.len(), 16);
    assert_eq!(sim.snapshots().len(), 16);
}
