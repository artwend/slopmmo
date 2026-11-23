//! The client half of §4.1: history-tracked peer colliders and the local solve.
//!
//! Each test states the claim it defends. The load-bearing ones are that a tick
//! reads the collider set that belonged to it (not "the newest"), that a late
//! datagram cannot rewrite an older tick, and that the local solve uses the same
//! contact rule as the server.

use bevy_math::Vec3;

use proto::{PeerCollider, PeerList, Tick, peer_mode};
use sim::collision::{Body, Mode};
use sim::peers::{LocalPeerSolve, PEER_HISTORY_DEPTH, PeerHistory, SampledPeer};

fn peer(player: u64, x: f32, z: f32, mode: u8) -> PeerCollider {
    PeerCollider {
        player,
        position: [x, 0.9, z],
        body_yaw: 0.0,
        mode,
    }
}

fn list(peers: &[PeerCollider]) -> PeerList {
    peers.iter().copied().collect()
}

fn sample(history: &PeerHistory, tick: u32) -> Vec<SampledPeer> {
    let mut out = Vec::new();
    history.sample(Tick::new(tick), &mut out);
    out
}

fn player(x: f32, z: f32) -> Body {
    Body::player(0, Vec3::new(x, 0.9, z), Vec3::ZERO)
}

#[test]
fn a_sample_is_read_at_the_tick_it_belongs_to() {
    let mut history = PeerHistory::new();
    // 0.6 m of travel over 30 server ticks: 1.2 m/s, well under the cap.
    assert!(history.record(Tick::new(10), &list(&[peer(2, 0.0, 0.0, peer_mode::SOFT)])));
    assert!(history.record(Tick::new(40), &list(&[peer(2, 0.6, 0.0, peer_mode::SOFT)])));

    let at_start = sample(&history, 10);
    assert_eq!(at_start.len(), 1);
    assert!((at_start[0].pos.x - 0.0).abs() < 1e-6);
    assert_eq!(
        at_start[0].vel,
        Vec3::ZERO,
        "an endpoint is held, not extrapolated"
    );

    let between = sample(&history, 25);
    assert!(
        (between[0].pos.x - 0.3).abs() < 1e-5,
        "interpolated position was {}",
        between[0].pos.x
    );
    assert!(
        (between[0].vel.x - 1.2).abs() < 1e-4,
        "interpolated velocity was {}",
        between[0].vel.x
    );

    // Past the newest sample the newest known pose is held, never extrapolated;
    // its velocity stays the last observed delta.
    let ahead = sample(&history, 10_000);
    assert!((ahead[0].pos.x - 0.6).abs() < 1e-6);
    assert!(
        (ahead[0].vel.x - 1.2).abs() < 1e-4,
        "the held newest lost its last observed velocity: {}",
        ahead[0].vel.x
    );

    // Before the oldest sample the oldest is held.
    let behind = sample(&history, 0);
    assert!((behind[0].pos.x - 0.0).abs() < 1e-6);
}

#[test]
fn a_late_or_duplicate_sample_cannot_rewrite_history() {
    let mut history = PeerHistory::new();
    assert!(history.record(Tick::new(10), &list(&[peer(2, 0.0, 0.0, peer_mode::SOFT)])));
    assert!(history.record(Tick::new(12), &list(&[peer(2, 0.4, 0.0, peer_mode::SOFT)])));

    assert!(
        !history.record(Tick::new(11), &list(&[peer(2, 9.0, 0.0, peer_mode::SOFT)])),
        "an out-of-order snapshot was accepted"
    );
    assert!(
        !history.record(Tick::new(12), &list(&[peer(2, 9.0, 0.0, peer_mode::SOFT)])),
        "a duplicate snapshot was accepted"
    );
    assert_eq!(history.newest(), Some(Tick::new(12)));
    assert_eq!(history.oldest(), Some(Tick::new(10)));
    assert_eq!(history.len(), 2);

    let now = sample(&history, 12);
    assert!(
        (now[0].pos.x - 0.4).abs() < 1e-6,
        "a late sample moved the peer"
    );
}

#[test]
fn a_peer_that_left_the_aoi_is_not_held_into_the_newest_set() {
    let mut history = PeerHistory::new();
    history.record(
        Tick::new(10),
        &list(&[
            peer(2, 0.5, 0.0, peer_mode::SOFT),
            peer(3, 1.0, 0.0, peer_mode::SOFT),
        ]),
    );
    history.record(Tick::new(12), &list(&[peer(2, 0.5, 0.0, peer_mode::SOFT)]));

    let newest = sample(&history, 12);
    assert!(
        newest.iter().all(|p| p.player != 3),
        "a departed peer was held into the newest set"
    );
    let between = sample(&history, 11);
    assert!(
        between.iter().any(|p| p.player == 3),
        "a peer still leaving the AOI vanished mid-span"
    );
}

#[test]
fn history_is_bounded() {
    let mut history = PeerHistory::new();
    for tick in 0..(PEER_HISTORY_DEPTH as u32 + 16) {
        history.record(
            Tick::new(100 + tick),
            &list(&[peer(2, tick as f32, 0.0, peer_mode::SOFT)]),
        );
    }
    assert_eq!(history.len(), PEER_HISTORY_DEPTH);
    assert_eq!(
        history.oldest(),
        Some(Tick::new(100 + 16)),
        "the oldest samples were not dropped"
    );
}

#[test]
fn a_peer_beyond_the_soft_margin_is_not_a_contact() {
    let mut solve = LocalPeerSolve::new();
    // reach = 0.8, margin = 0.05.
    solve.record(
        Tick::new(10),
        &list(&[peer(
            2,
            0.8 + proto::SOFT_MARGIN + 1e-3,
            0.0,
            peer_mode::SOFT,
        )]),
    );
    let impulse = solve.solve(Tick::new(10), &player(0.0, 0.0), proto::CORRECTION_BUDGET);
    assert_eq!(solve.contacts, 0);
    assert_eq!(impulse.delta_v, Vec3::ZERO);
}

#[test]
fn a_soft_contact_pushes_a_closing_player_out() {
    let mut solve = LocalPeerSolve::new();
    solve.record(Tick::new(10), &list(&[peer(2, 0.5, 0.0, peer_mode::SOFT)]));
    let mut local = player(0.0, 0.0);
    local.vel = Vec3::new(3.0, 0.0, 0.0);

    let impulse = solve.solve(Tick::new(10), &local, proto::CORRECTION_BUDGET);
    assert_eq!(solve.contacts, 1);
    assert_eq!(solve.hard_contacts, 0);
    assert!(
        impulse.delta_v.x < 0.0,
        "the contact did not push the closing player back: {:?}",
        impulse.delta_v
    );
    assert!(!impulse.hard);
    assert!(impulse.hard_normal.is_none());

    // Budget applies to the vector sum, exactly as on the server.
    assert!(impulse.delta_v.length() <= proto::CORRECTION_BUDGET + 1e-5);
}

#[test]
fn a_hard_contact_reports_the_surface_normal() {
    let mut solve = LocalPeerSolve::new();
    solve.record(Tick::new(10), &list(&[peer(2, 0.5, 0.0, peer_mode::HARD)]));
    let mut local = player(0.0, 0.0);
    local.vel = Vec3::new(3.0, 0.0, 0.0);

    let impulse = solve.solve(Tick::new(10), &local, proto::CORRECTION_BUDGET);
    assert_eq!(solve.contacts, 1);
    assert_eq!(solve.hard_contacts, 1);
    assert!(impulse.hard);
    let normal = impulse.hard_normal.expect("hard contact without a normal");
    assert!(
        normal.x < -0.99,
        "the local body must be told to slide away from +x, got {normal:?}"
    );
}

#[test]
fn the_solve_reads_the_set_that_belongs_to_its_tick() {
    let mut solve = LocalPeerSolve::new();
    // Near at tick 30, far at tick 10. The same solver is asked about both.
    solve.record(Tick::new(10), &list(&[peer(2, 20.0, 0.0, peer_mode::SOFT)]));
    solve.record(Tick::new(30), &list(&[peer(2, 0.5, 0.0, peer_mode::SOFT)]));
    let local = player(0.0, 0.0);

    let early = solve.solve(Tick::new(10), &local, proto::CORRECTION_BUDGET);
    assert_eq!(
        solve.contacts, 0,
        "tick 10 saw a collider that was not there"
    );
    assert_eq!(early.delta_v, Vec3::ZERO);

    let late = solve.solve(Tick::new(30), &local, proto::CORRECTION_BUDGET);
    assert_eq!(solve.contacts, 1, "tick 30 missed the near collider");
    assert_ne!(late.delta_v, Vec3::ZERO);
}

#[test]
fn a_mode_promoted_to_hard_between_samples_is_hard() {
    let mut history = PeerHistory::new();
    history.record(Tick::new(10), &list(&[peer(2, 0.5, 0.0, peer_mode::SOFT)]));
    history.record(Tick::new(12), &list(&[peer(2, 0.5, 0.0, peer_mode::HARD)]));

    let between = sample(&history, 11);
    assert_eq!(between[0].mode, Mode::Hard);
}

#[test]
fn the_same_history_and_tick_solve_to_the_same_delta() {
    let activate = || {
        let mut solve = LocalPeerSolve::new();
        solve.record(
            Tick::new(10),
            &list(&[
                peer(2, 0.5, 0.0, peer_mode::SOFT),
                peer(3, -0.5, 0.0, peer_mode::HARD),
                peer(4, 0.0, 0.5, peer_mode::SOFT),
            ]),
        );
        let mut local = player(0.0, 0.0);
        local.vel = Vec3::new(1.5, 0.0, -1.0);
        solve.solve(Tick::new(10), &local, proto::CORRECTION_BUDGET)
    };
    let first = activate();
    let second = activate();
    assert_eq!(first.delta_v.to_array(), second.delta_v.to_array());
    assert_eq!(first.hard_normal, second.hard_normal);
}
