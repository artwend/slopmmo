//! Behaviour tests for the pieces the architecture review says were wrong.
//!
//! Each test name states the claim it defends; anything that regresses a claim
//! in `docs/architecture-review.md` should break one of these.

use bevy_math::Vec3;

use proto::Tick;

use sim::authority::{Reject, SimConfig, Simulation};
use sim::collision::{self, Body, Mode, build_pairs};
use sim::kcc::{self, Capsule, KccParams};
use sim::movement::{self, MoveParams};
use sim::spatial::SpatialGrid;
use sim::world::StaticWorld;
use sim::{Aabb, move_and_slide};

fn dt() -> f32 {
    proto::TICK_DT
}

const GRAVITY: f32 = -24.0;

/// Runs the KCC with a constant *desired* horizontal velocity and gravity: the
/// horizontal part is re-asserted every tick, the way a player holding a stick
/// would, so wall friction cannot silently kill the intent.
fn simulate(world: &StaticWorld, start: Vec3, vel: Vec3, ticks: u32) -> kcc::MoveOutput {
    let params = KccParams::default();
    let mut out = kcc::MoveOutput {
        position: start,
        velocity: vel,
        ..Default::default()
    };
    for _ in 0..ticks {
        out.velocity = Vec3::new(vel.x, out.velocity.y + GRAVITY * dt(), vel.z);
        out = move_and_slide(world, &params, out.position, out.velocity, dt());
    }
    out
}

#[test]
fn capsule_geometry_matches_its_height() {
    let c = Capsule::from_height(0.4, 1.8);
    assert!((c.height() - 1.8).abs() < 1e-5);
    assert!((c.foot(Vec3::new(0.0, 0.9, 0.0))).abs() < 1e-5);
}

#[test]
fn falling_body_lands_on_the_floor_and_is_grounded() {
    let world = StaticWorld::test_room();
    let out = simulate(&world, Vec3::new(0.0, 3.0, 0.0), Vec3::ZERO, 120);
    assert!(out.grounded, "should find the floor");
    assert!(
        (out.position.y - 0.9).abs() < 0.01,
        "rest height was {}",
        out.position.y
    );
    assert!(out.velocity.y.abs() < 0.01);
}

#[test]
fn wall_stops_horizontal_motion_instead_of_tunnelling() {
    let world = StaticWorld::test_room();
    let out = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(30.0, 0.0, 0.0),
        120,
    );
    // Inner wall face is at x = 9.5, capsule radius 0.4.
    assert!(
        out.position.x < 9.15,
        "escaped the room at x={}",
        out.position.x
    );
    assert!(out.velocity.x.abs() < 0.5, "wall did not absorb velocity");
}

#[test]
fn pillar_contact_slides_along_the_surface() {
    let world = StaticWorld::test_room();
    // Aim at the pillar's -z face (pillar spans x 1.5..2.5, z 1.5..2.5) with a
    // tangential +x component. Head-on velocity into a corner has no sliding
    // direction to preserve, so a face hit with lateral motion is the case that
    // actually exercises the projection.
    let out = simulate(
        &world,
        Vec3::new(1.2, 0.9, -4.0),
        Vec3::new(1.0, 0.0, 6.0),
        150,
    );
    assert!(
        out.position.z > 4.0,
        "should have slid past the pillar, ended at {out:?}"
    );
    assert!(
        out.position.x > 2.6,
        "should have been carried along the face, ended at {out:?}"
    );
}

#[test]
fn closed_pair_cancels_closing_velocity() {
    let mut a = Body::player(1, Vec3::new(0.0, 0.9, 0.0), Vec3::new(3.0, 0.0, 0.0));
    let mut b = Body::player(2, Vec3::new(0.5, 0.9, 0.0), Vec3::new(-3.0, 0.0, 0.0));
    a.mode = Mode::Soft;
    b.mode = Mode::Soft;
    let bodies = vec![a, b];

    let positions: Vec<Vec3> = bodies.iter().map(|x| x.pos).collect();
    let mut grid = SpatialGrid::new(4.0);
    grid.rebuild(&positions);
    let pairs = build_pairs(&bodies, &grid, &positions, 8, 8);
    assert_eq!(pairs.len(), 1);

    let imp = collision::solve(&bodies, &pairs, 99.0);
    let va = a.vel + imp[0].delta_v;
    let vb = b.vel + imp[1].delta_v;
    let rel = (vb - va).dot(pairs[0].normal);
    // The -6 m/s closing velocity is gone; what is left is only the bounded
    // penetration bias pushing them apart.
    assert!(rel >= -1e-3, "still closing after the solve: {rel}");
    assert!(
        rel <= collision::PENETRATION_BIAS_MAX + 1e-3,
        "bias overshot its cap: {rel}"
    );

    // Both move apart, not through each other.
    assert!(imp[0].delta_v.x < 0.0);
    assert!(imp[1].delta_v.x > 0.0);
}

#[test]
fn stacked_players_on_different_floors_do_not_collide() {
    let high = Body::player(1, Vec3::new(0.0, 5.0, 0.0), Vec3::ZERO);
    let low = Body::player(2, Vec3::new(0.0, 0.9, 0.0), Vec3::ZERO);
    let bodies = vec![high, low];
    let positions: Vec<Vec3> = bodies.iter().map(|x| x.pos).collect();
    let mut grid = SpatialGrid::new(4.0);
    grid.rebuild(&positions);
    assert!(
        build_pairs(&bodies, &grid, &positions, 8, 8).is_empty(),
        "vertical band check failed"
    );
}

#[test]
fn distant_players_produce_no_pairs_without_a_radius_limit() {
    let a = Body::player(1, Vec3::new(0.0, 0.9, 0.0), Vec3::ZERO);
    let b = Body::player(2, Vec3::new(100.0, 0.9, 0.0), Vec3::ZERO);
    let bodies = vec![a, b];
    let positions: Vec<Vec3> = bodies.iter().map(|x| x.pos).collect();
    let mut grid = SpatialGrid::new(4.0);
    grid.rebuild(&positions);
    assert!(build_pairs(&bodies, &grid, &positions, 8, 8).is_empty());
}

#[test]
fn hard_blocker_absorbs_players_and_keeps_its_velocity() {
    let player = Body::player(1, Vec3::new(0.0, 0.9, 0.0), Vec3::new(4.0, 0.0, 0.0));
    let blocker = Body {
        id: 99,
        pos: Vec3::new(0.7, 0.9, 0.0),
        vel: Vec3::ZERO,
        radius: 0.8,
        half_height: 1.8,
        inv_mass: 0.0,
        mode: Mode::Hard,
    };
    let bodies = vec![player, blocker];
    let positions: Vec<Vec3> = bodies.iter().map(|x| x.pos).collect();
    let mut grid = SpatialGrid::new(4.0);
    grid.rebuild(&positions);
    let pairs = build_pairs(&bodies, &grid, &positions, 8, 8);
    assert_eq!(pairs[0].mode, Mode::Hard);

    let imp = collision::solve(&bodies, &pairs, 99.0);
    assert_eq!(imp[1].delta_v, Vec3::ZERO, "immovable body moved");
    assert!(imp[0].delta_v.x < 0.0, "player was not pushed back");
    assert!(imp[0].hard_normal.is_some(), "hard normal not reported");
}

#[test]
fn eight_neighbours_cannot_exceed_the_correction_budget() {
    let centre = Body::player(1, Vec3::new(0.0, 0.9, 0.0), Vec3::new(1.0, 0.0, 0.0));
    let mut bodies = vec![centre];
    for i in 0..8u64 {
        let ang = i as f32 / 8.0 * std::f32::consts::TAU;
        bodies.push(Body::player(
            2 + i,
            Vec3::new(ang.cos() * 0.6, 0.9, ang.sin() * 0.6),
            Vec3::new(-ang.cos() * 1.0, 0.0, -ang.sin() * 1.0),
        ));
    }
    let positions: Vec<Vec3> = bodies.iter().map(|x| x.pos).collect();
    let mut grid = SpatialGrid::new(4.0);
    grid.rebuild(&positions);
    let pairs = build_pairs(&bodies, &grid, &positions, 8, 8);
    assert!(pairs.len() >= 8);

    let budget = 0.35;
    let imp = collision::solve(&bodies, &pairs, budget);
    for (i, d) in imp.iter().enumerate() {
        assert!(
            d.delta_v.length() <= budget + 1e-4,
            "body {i} got {} > {budget}",
            d.delta_v.length()
        );
    }
}

#[test]
fn movement_reaches_target_speed_and_stops_on_friction() {
    let params = MoveParams::default();
    let input = proto::InputFrame {
        move_x: 127,
        move_z: 0,
        ..Default::default()
    };
    let mut vel = Vec3::ZERO;
    for _ in 0..120 {
        vel = movement::step_velocity(&params, &input, vel, true, dt());
    }
    assert!(
        (vel.length() - params.max_speed).abs() < 0.2,
        "walk speed was {}",
        vel.length()
    );

    let idle = proto::InputFrame::default();
    for _ in 0..30 {
        vel = movement::step_velocity(&params, &idle, vel, true, dt());
    }
    assert!(vel.length() < 0.2, "friction did not stop the body");
}

#[test]
fn jump_leaves_the_ground_then_returns() {
    let params = MoveParams::default();
    let world = StaticWorld::test_room();
    let kcc = KccParams::default();
    let jump = proto::InputFrame {
        buttons: proto::buttons::JUMP,
        ..Default::default()
    };

    let mut out = kcc::MoveOutput {
        position: Vec3::new(0.0, 0.9, 0.0),
        velocity: Vec3::ZERO,
        grounded: true,
        contacts: 0,
    };
    let mut peak: f32 = 0.0;
    for i in 0..120 {
        let input = if i == 0 {
            jump
        } else {
            proto::InputFrame::default()
        };
        out.velocity = movement::step_velocity(&params, &input, out.velocity, out.grounded, dt());
        out = move_and_slide(&world, &kcc, out.position, out.velocity, dt());
        peak = peak.max(out.position.y);
    }
    assert!(peak > 1.2, "jump did not lift the capsule (peak {peak})");
    assert!(out.grounded, "did not land again");
    assert!((out.position.y - 0.9).abs() < 0.02);
}

fn packet(seq: u16, tick: u32, player: u64, pos: Vec3, vel: Vec3) -> proto::PlayerMovementPacket {
    proto::PlayerMovementPacket {
        seq,
        player,
        report: proto::MovementReport {
            tick: Tick::new(tick),
            position: pos.to_array(),
            velocity: vel.to_array(),
            grounded: true,
            ..Default::default()
        },
        inputs: Vec::new(),
    }
}

#[test]
fn teleporting_reports_are_rejected() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.tick = Tick::new(100);

    let ok = packet(1, 100, 1, Vec3::new(0.05, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&ok), Ok(()));

    let bad = packet(2, 101, 1, Vec3::new(40.0, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&bad), Err(Reject::Teleport));

    let fast = packet(
        3,
        102,
        1,
        Vec3::new(0.1, 0.9, 0.0),
        Vec3::new(0.0, 0.0, 400.0),
    );
    assert_eq!(sim.ingest(&fast), Err(Reject::TooFast));
}

#[test]
fn duplicate_and_stale_packets_are_rejected_without_moving_the_shadow() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.tick = Tick::new(200);

    let first = packet(5, 200, 1, Vec3::new(0.01, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&first), Ok(()));
    let shadow = sim.players[&1].pos;

    assert_eq!(sim.ingest(&first), Err(Reject::DuplicateSeq));
    assert_eq!(sim.players[&1].pos, shadow);

    // Older than the report window.
    let old = packet(
        6,
        200u32.wrapping_sub(proto::REPORT_WINDOW + 5),
        1,
        Vec3::new(0.02, 0.9, 0.0),
        Vec3::ZERO,
    );
    assert!(matches!(
        sim.ingest(&old),
        Err(Reject::StaleTick) | Err(Reject::Teleport)
    ));
}

#[test]
fn a_client_clock_unrelated_to_the_server_tick_still_joins() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.tick = Tick::new(5000);

    // The client counts ticks from its own start; that is not a protocol error.
    let first = packet(1, 0, 1, Vec3::new(0.0, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&first), Ok(()));
    assert_eq!(sim.players[&1].tick_offset, Some(5000));

    let next = packet(2, 1, 1, Vec3::new(0.02, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&next), Ok(()));

    // A long hitch moves the client's clock far ahead: re-anchor instead of
    // rejecting the session for the rest of its life.
    let jumped = packet(3, 20_000, 1, Vec3::new(0.05, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&jumped), Ok(()));
    assert_eq!(
        sim.players[&1].tick_offset,
        Some(sim.tick.0.wrapping_sub(20_000))
    );
}

#[test]
fn server_corrects_overlapping_players_and_stays_deterministic() {
    let run = || {
        let world = StaticWorld::test_room();
        let mut sim = Simulation::new(SimConfig::default());
        sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
        sim.add_player(2, Vec3::new(0.45, 0.9, 0.0));
        sim.tick = Tick::new(10);
        sim.players.get_mut(&1).unwrap().vel = Vec3::new(2.0, 0.0, 0.0);
        sim.players.get_mut(&2).unwrap().vel = Vec3::new(-2.0, 0.0, 0.0);

        let mut snaps = Vec::new();
        for _ in 0..30 {
            for s in sim.step(&world) {
                snaps.push(format!(
                    "{:?} {:?} {:?}",
                    s.server_tick,
                    s.correction.map(|c| c.velocity_delta),
                    s.shadow_position
                ));
            }
        }
        (sim, snaps)
    };

    let (sim, first) = run();
    let (_, second) = run();
    assert_eq!(first, second, "solver is not deterministic");
    assert!(sim.stats.corrections > 0, "overlap was never corrected");
}

#[test]
fn snapshot_lists_nearby_peers_within_aoi_only() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.add_player(2, Vec3::new(1.0, 0.9, 0.0));
    sim.add_player(3, Vec3::new(9.0, 0.9, 9.0));

    let snaps = sim.step(&world);
    assert_eq!(snaps.len(), 3);
    let near = &snaps[0];
    assert!(
        near.peers.iter().any(|p| p.player == 2),
        "near peer missing from snapshot"
    );
    assert!(
        !near.peers.iter().any(|p| p.player == 3),
        "far peer leaked into AOI"
    );
}

#[test]
fn kcc_does_not_climb_a_ledge_taller_than_the_capsule_radius() {
    let mut world = StaticWorld::new();
    world.add(Aabb::new(
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(20.0, 0.5, 20.0),
    ));
    // 0.9 m ledge: taller than the 0.4 m capsule radius, so no amount of
    // corner rounding should let the capsule walk onto it without a jump.
    world.add(Aabb::from_min_max(
        Vec3::new(3.0, 0.0, -5.0),
        Vec3::new(10.0, 0.9, 5.0),
    ));

    let out = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(5.0, 0.0, 0.0),
        180,
    );
    assert!(
        out.position.x < 2.9,
        "skeleton KCC must block the ledge, got x={}",
        out.position.x
    );
}
