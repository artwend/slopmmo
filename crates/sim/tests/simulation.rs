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

/// The KCC and movement model are client-side, so these tests step them at the
/// client prediction rate. Server-side tests use `SimConfig::dt()`.
fn dt() -> f32 {
    proto::CLIENT_TICK_DT
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
            linear_velocity: vel.to_array(),
            angular_velocity: [0.0; 3],
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

    // Rotation is bounded by the same check, not just translation.
    let mut spinning = packet(4, 103, 1, Vec3::new(0.1, 0.9, 0.0), Vec3::ZERO);
    spinning.report.angular_velocity = [0.0, 400.0, 0.0];
    assert_eq!(sim.ingest(&spinning), Err(Reject::TooFast));
}

#[test]
fn duplicate_and_stale_packets_are_rejected_without_moving_the_shadow() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.tick = Tick::new(200);

    let first = packet(5, 200, 1, Vec3::new(0.01, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&first), Ok(()));
    let shadow = sim.player(1).unwrap().pos;

    assert_eq!(sim.ingest(&first), Err(Reject::DuplicateSeq));
    assert_eq!(sim.player(1).unwrap().pos, shadow);

    // Older than the report window, measured on the server clock. The client and
    // server rates need not match, so the client-tick offset is the server window
    // scaled by their ratio.
    let client_ticks_per_server_tick = (proto::CLIENT_TICK_HZ / proto::SERVER_TICK_HZ).max(1);
    let old_client_tick =
        200u32.wrapping_sub(client_ticks_per_server_tick * (proto::REPORT_WINDOW + 5));
    let old = packet(6, old_client_tick, 1, Vec3::new(0.02, 0.9, 0.0), Vec3::ZERO);
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
    assert_eq!(
        sim.player(1).unwrap().tick_origin,
        Some((Tick::new(0), Tick::new(5000)))
    );

    let next = packet(2, 1, 1, Vec3::new(0.02, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&next), Ok(()));

    // A long hitch moves the client's clock far ahead: re-anchor instead of
    // rejecting the session for the rest of its life.
    let jumped = packet(3, 20_000, 1, Vec3::new(0.05, 0.9, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&jumped), Ok(()));
    assert_eq!(
        sim.player(1).unwrap().tick_origin,
        Some((Tick::new(20_000), sim.tick))
    );
}

#[test]
fn a_60hz_client_clock_maps_onto_a_30hz_server_without_re_anchoring() {
    // The default server tick is 60 Hz too, so the mapping is the identity in the
    // shipped configuration. It still has to survive a rate split: half rate is
    // the case that used to re-anchor every half second.
    let config = SimConfig {
        tick_hz: 30,
        ..Default::default()
    };
    let mut sim = Simulation::new(config);
    sim.add_player(1, Vec3::ZERO);
    sim.tick = Tick::new(1000);
    let anchor = (Tick::new(0), sim.tick);

    // The client predicts at 60 Hz and reports every second tick, so exactly one
    // report lands per 30 Hz server tick. Treating both counters as the same rate
    // would map the client a tick ahead of the server for every two client ticks
    // and trip the future-staleness re-anchor.
    for i in 0..30u32 {
        sim.tick = Tick::new(1000 + i);
        let pos = Vec3::new(i as f32 * 0.02, 0.0, 0.0);
        let report = packet(i as u16 + 1, i * 2, 1, pos, Vec3::ZERO);
        assert_eq!(sim.ingest(&report), Ok(()), "report {i} was rejected");
    }

    assert_eq!(
        sim.player(1).unwrap().tick_origin,
        Some(anchor),
        "the clock domain re-anchored mid-stream"
    );
}

#[test]
fn a_resume_is_clamped_to_the_shadow_it_left() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    let spawn = Vec3::new(2.0, 0.9, -3.0);
    sim.add_player(1, spawn);
    {
        let p = sim.player_mut(1).unwrap();
        p.vel = Vec3::new(1.0, 0.0, 2.0);
        p.angular_velocity = Vec3::new(0.0, 0.75, 0.0);
        p.body_yaw = 1.25;
        p.grounded = true;
    }

    assert!(sim.detach_player(1));
    assert!(!sim.contains_player(1), "detached body still in the world");

    // The reconnect claims the far corner of the map.
    let claim = Vec3::new(15.0, 0.9, 15.0);
    let resume = sim
        .resume_player(&packet(1, 0, 1, claim, Vec3::new(50.0, 0.0, 0.0)))
        .expect("the shadow was not kept for a resume");
    assert!(resume.clamped, "a cross-map claim was accepted");
    assert_eq!(resume.reported, claim);
    assert_eq!(sim.stats.resumed, 1);
    assert_eq!(sim.stats.resume_clamped, 1);
    assert!(sim.contains_player(1));

    let allowed = sim.config.resume_travel();
    let from_shadow = resume.pos - spawn;
    assert!(
        (from_shadow.length() - allowed).abs() < 1e-3,
        "clamped to {} instead of {allowed}",
        from_shadow.length()
    );
    assert!(
        from_shadow
            .normalize()
            .abs_diff_eq((claim - spawn).normalize(), 1e-3),
        "the clamp changed the claimed direction"
    );

    // The parked state is restored; the unvalidated claim's velocity is not.
    let p = sim.player(1).unwrap();
    assert_eq!(p.pos, resume.pos);
    assert_eq!(p.vel, Vec3::new(1.0, 0.0, 2.0));
    assert_eq!(p.angular_velocity, Vec3::new(0.0, 0.75, 0.0));
    assert!((p.body_yaw - 1.25).abs() < 1e-6);
    assert!(p.grounded);
}

#[test]
fn a_resumed_session_cannot_walk_its_clamp_back() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    sim.detach_player(1);

    // The admission report is consumed by the resume and becomes the baseline
    // for the displacement check. Without that, the client's absolute tick
    // counter would grant an unbounded allowance and the very next packet
    // would move the shadow to the claimed spot.
    let claim = Vec3::new(40.0, 0.0, 0.0);
    let resume = sim
        .resume_player(&packet(7, 0, 1, claim, Vec3::ZERO))
        .expect("resume lost");
    assert!(resume.clamped);

    assert_eq!(
        sim.ingest(&packet(8, 2, 1, claim, Vec3::ZERO)),
        Err(Reject::Teleport),
        "the claim undid its own clamp"
    );
    assert_eq!(sim.player(1).unwrap().pos, resume.pos);

    // A forged jump in the client tick must not buy the allowance back: the
    // clock re-anchor accepts the new domain, but travel is bounded by server
    // time since the last accepted report.
    assert_eq!(
        sim.ingest(&packet(9, 1_000_000, 1, claim, Vec3::ZERO)),
        Err(Reject::Teleport),
        "a tick jump undid the clamp"
    );

    // A report from the server's placement is accepted normally.
    assert_eq!(
        sim.ingest(&packet(10, 1_000_002, 1, resume.pos, Vec3::ZERO)),
        Ok(())
    );
}

#[test]
fn a_tick_jump_does_not_buy_a_teleport() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    assert_eq!(sim.ingest(&packet(1, 0, 1, Vec3::ZERO, Vec3::ZERO)), Ok(()));

    sim.tick = Tick::new(101);
    let jumped = packet(2, 1_000_000, 1, Vec3::new(100.0, 0.0, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&jumped), Err(Reject::Teleport));
    assert_eq!(sim.player(1).unwrap().pos, Vec3::ZERO);

    // The re-anchor still lets the session continue from where it is.
    let ok = packet(3, 1_000_002, 1, Vec3::new(0.05, 0.0, 0.0), Vec3::ZERO);
    assert_eq!(sim.ingest(&ok), Ok(()));
}

#[test]
fn a_shadow_older_than_the_resume_window_is_forgotten() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.config.resume_window = 10;
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    sim.detach_player(1);

    sim.tick = Tick::new(111);
    assert!(
        sim.resume_player(&packet(1, 0, 1, Vec3::new(40.0, 0.0, 0.0), Vec3::ZERO))
            .is_none(),
        "a stale shadow was used to place a resume"
    );
    assert!(!sim.contains_player(1));
}

#[test]
fn a_clamped_resume_asks_for_a_resync_until_a_report_lands() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    sim.detach_player(1);

    let resume = sim
        .resume_player(&packet(1, 0, 1, Vec3::new(40.0, 0.0, 0.0), Vec3::ZERO))
        .expect("resume lost");
    assert!(resume.clamped);

    // The flag must be repeated: a lost unreliable snapshot must not leave the
    // client stranded where the server refused to put it.
    for _ in 0..2 {
        sim.step(&world);
        let snap = sim
            .snapshots()
            .iter()
            .find(|(id, _)| *id == 1)
            .expect("snapshot missing")
            .1;
        assert!(
            snap.resync,
            "the client was never told to adopt the placement"
        );
        assert_eq!(snap.shadow_position, resume.pos.to_array());
    }

    let synced = packet(2, 2, 1, sim.player(1).unwrap().pos, Vec3::ZERO);
    assert_eq!(sim.ingest(&synced), Ok(()));
    sim.step(&world);
    let snap = sim.snapshots().iter().find(|(id, _)| *id == 1).unwrap().1;
    assert!(!snap.resync, "resync was not cleared by an accepted report");
}

#[test]
fn an_unclamped_resume_is_taken_as_reported() {
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    sim.detach_player(1);

    let reported = Vec3::new(0.0, 0.9, 1.0);
    let resume = sim
        .resume_player(&packet(1, 0, 1, reported, Vec3::ZERO))
        .expect("resume lost");
    assert!(!resume.clamped);
    assert_eq!(resume.pos, reported);
    assert_eq!(sim.stats.resume_clamped, 0);
    assert!(!sim.player(1).unwrap().needs_resync);
}

#[test]
fn server_corrects_overlapping_players_and_stays_deterministic() {
    let run = || {
        let world = StaticWorld::test_room();
        let mut sim = Simulation::new(SimConfig::default());
        sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
        sim.add_player(2, Vec3::new(0.45, 0.9, 0.0));
        sim.tick = Tick::new(10);
        sim.player_mut(1).unwrap().vel = Vec3::new(2.0, 0.0, 0.0);
        sim.player_mut(2).unwrap().vel = Vec3::new(-2.0, 0.0, 0.0);

        let mut snaps = Vec::new();
        for _ in 0..30 {
            sim.step(&world);
            for (_, s) in sim.snapshots() {
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
fn shadow_extrapolates_between_reports_then_freezes() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));

    let speed = 1.2;
    let report = packet(
        1,
        100,
        1,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(speed, 0.0, 0.0),
    );
    assert_eq!(sim.ingest(&report), Ok(()));

    // The report was accepted at server tick 100, so the first tick after it is
    // already one tick of predicted travel, not zero.
    sim.step(&world);
    assert!((sim.player(1).unwrap().pos.x - speed * dt()).abs() < 1e-6);
    sim.step(&world);
    assert!((sim.player(1).unwrap().pos.x - speed * 2.0 * dt()).abs() < 1e-6);

    for _ in 0..proto::REPORT_WINDOW {
        sim.step(&world);
    }
    let frozen = sim.player(1).unwrap().pos.x;
    let limit = speed * proto::REPORT_WINDOW as f32 * dt();
    assert!(
        (frozen - limit).abs() < 1e-4,
        "extrapolation ran to {frozen}, window limit is {limit}"
    );

    // Frozen means frozen: the shadow does not keep walking on its own.
    sim.step(&world);
    assert_eq!(sim.player(1).unwrap().pos.x, frozen);
}

#[test]
fn shadow_dead_reckons_yaw_between_reports_then_freezes() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));

    let yaw = 0.25;
    let spin = 1.5;
    let mut report = packet(1, 100, 1, Vec3::new(0.0, 0.9, 0.0), Vec3::ZERO);
    report.report.body_yaw = yaw;
    report.report.angular_velocity = [0.0, spin, 0.0];
    assert_eq!(sim.ingest(&report), Ok(()));

    // The accepted report is the rotation anchor, so the first tick already
    // advances the yaw by one tick of spin.
    sim.step(&world);
    assert!((sim.player(1).unwrap().body_yaw - (yaw + spin * dt())).abs() < 1e-6);
    sim.step(&world);
    assert!((sim.player(1).unwrap().body_yaw - (yaw + spin * 2.0 * dt())).abs() < 1e-6);

    for _ in 0..proto::REPORT_WINDOW {
        sim.step(&world);
    }
    let frozen = sim.player(1).unwrap().body_yaw;
    let limit = yaw + spin * proto::REPORT_WINDOW as f32 * dt();
    assert!(
        (frozen - limit).abs() < 1e-4,
        "yaw extrapolation ran to {frozen}, window limit is {limit}"
    );

    sim.step(&world);
    assert_eq!(sim.player(1).unwrap().body_yaw, frozen);
}

#[test]
fn a_stalled_report_stream_catches_up_within_the_window() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));

    let speed = proto::MAX_SPEED;
    let start = packet(
        1,
        100,
        1,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(speed, 0.0, 0.0),
    );
    assert_eq!(sim.ingest(&start), Ok(()));
    for _ in 0..20 {
        sim.step(&world);
    }

    // The client really did move during the gap, so the catch-up report is
    // legitimate even though it is far from the extrapolated pose's neighbour:
    // travel is measured from the anchor over server time.
    let caught_up = Vec3::new(speed * 20.0 * dt(), 0.9, 0.0);
    let catch_up = packet(2, 120, 1, caught_up, Vec3::new(speed, 0.0, 0.0));
    assert_eq!(
        sim.ingest(&catch_up),
        Ok(()),
        "a legitimate catch-up after a stalled stream was rejected"
    );
    assert_eq!(sim.player(1).unwrap().anchor_pos, caught_up);
}

#[test]
fn a_clamped_resume_does_not_drift_before_the_next_report() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.tick = Tick::new(100);
    sim.add_player(1, Vec3::ZERO);
    sim.player_mut(1).unwrap().vel = Vec3::new(3.0, 0.0, 0.0);
    assert!(sim.detach_player(1));

    let resume = sim
        .resume_player(&packet(1, 0, 1, Vec3::new(40.0, 0.0, 0.0), Vec3::ZERO))
        .expect("resume lost");
    assert!(resume.clamped);

    // The parked velocity is not a prediction baseline: a clamped session waits
    // for the client to report from the placement it was given.
    for _ in 0..2 {
        sim.step(&world);
        assert_eq!(sim.player(1).unwrap().pos, resume.pos);
    }
}

#[test]
fn peer_colliders_are_not_built_until_a_client_consumes_them() {
    let world = StaticWorld::test_room();
    let mut sim = Simulation::new(SimConfig::default());
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.add_player(2, Vec3::new(1.0, 0.9, 0.0));

    sim.step(&world);
    for (_, snapshot) in sim.snapshots() {
        assert!(
            snapshot.peers.is_empty(),
            "peer colliders were built for a client that does not read them"
        );
    }
    assert!(
        !sim.config.replicate_peers,
        "the shipped configuration must not pay for the peer list"
    );
}

#[test]
fn snapshot_lists_nearby_peers_within_aoi_only() {
    let world = StaticWorld::test_room();
    let config = SimConfig {
        replicate_peers: true,
        ..Default::default()
    };
    let mut sim = Simulation::new(config);
    sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
    sim.add_player(2, Vec3::new(1.0, 0.9, 0.0));
    sim.add_player(3, Vec3::new(9.0, 0.9, 9.0));

    sim.step(&world);
    let snaps = sim.snapshots();
    assert_eq!(snaps.len(), 3);
    let near = &snaps[0].1;
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
fn grid_aoi_matches_a_full_scan_across_cell_boundaries() {
    let world = StaticWorld::test_room();
    let config = SimConfig {
        replicate_peers: true,
        ..Default::default()
    };
    let mut sim = Simulation::new(config);

    // A spread of players straddling the 4 m grid cells, plus a few far away.
    // Spacing keeps capsules out of penetration, so the solve never moves the
    // shadow and the AOI query centre equals the indexed position.
    let mut id = 1u64;
    for gx in 0..7 {
        for gz in 0..7 {
            let x = -9.0 + gx as f32 * 1.3;
            let z = -9.0 + gz as f32 * 1.3;
            sim.add_player(id, Vec3::new(x, 0.9, z));
            id += 1;
        }
    }
    sim.add_player(id, Vec3::new(60.0, 0.9, 60.0));
    id += 1;
    sim.add_player(id, Vec3::new(-60.0, 0.9, 2.0));

    sim.step(&world);

    let aoi = proto::NEIGHBOUR_RADIUS * 2.0;
    let players = sim.players();
    let snaps = sim.snapshots();
    assert_eq!(snaps.len(), players.len());
    for (i, p) in players.iter().enumerate() {
        // Brute-force reference: every other body within the AOI radius, the
        // nearest MAX_SNAPSHOT_PEERS ordered by (distance, body index) — the
        // same total order the inline window keeps, so the result is unique.
        let mut want: Vec<(f32, usize)> = Vec::new();
        for (j, q) in players.iter().enumerate() {
            if j == i {
                continue;
            }
            let d = ((q.pos.x - p.pos.x).powi(2) + (q.pos.z - p.pos.z).powi(2)).sqrt();
            if d <= aoi {
                want.push((d, j));
            }
        }
        want.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        want.truncate(proto::MAX_SNAPSHOT_PEERS);
        let want_ids: Vec<u64> = want.iter().map(|&(_, j)| players[j].id).collect();
        let got_ids: Vec<u64> = snaps[i].1.peers.iter().map(|pc| pc.player).collect();
        assert_eq!(got_ids, want_ids, "AOI peers diverge for player {}", p.id);
    }
}

/// Known open defect, kept as a repro (architecture-review.md §7.7).
///
/// The shadow pose is derived from `anchor + vel × age`, and `vel` already
/// includes every correction applied since the anchor. A new delta is therefore
/// multiplied by the ticks elapsed since the report instead of being integrated
/// once. The intended invariant is
/// `pos(n) = anchor + Σ vel_after(k) · dt`.
///
/// Ignored until the fix lands; run explicitly with
/// `cargo test -p sim --test simulation -- --ignored`.
#[test]
#[ignore = "correction stacking: see architecture-review.md §7.7"]
fn corrections_are_integrated_once_per_tick() {
    let world = StaticWorld::test_room();
    let config = SimConfig::default();
    let dt = config.dt();
    let mut sim = Simulation::new(config);
    sim.tick = Tick::new(100);

    let start = Vec3::new(0.0, 0.9, 0.0);
    let peer = Vec3::new(0.5, 0.9, 0.0);
    sim.add_player(1, start);
    sim.add_player(2, peer);
    assert_eq!(sim.ingest(&packet(1, 100, 1, start, Vec3::ZERO)), Ok(()));
    assert_eq!(sim.ingest(&packet(1, 100, 2, peer, Vec3::ZERO)), Ok(()));

    let mut expected = start;
    for _ in 0..10 {
        sim.step(&world);
        expected += sim.player(1).unwrap().vel * dt;
    }
    let actual = sim.player(1).unwrap().pos;
    assert!(
        (actual - expected).length() < 1e-4,
        "shadow integrated to {actual:?}; one-tick integration predicts {expected:?}"
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
