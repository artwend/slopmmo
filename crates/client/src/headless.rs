//! Scripted, deterministic client run. This is what CI executes.
//!
//! Exits non-zero when a movement invariant breaks, so it is usable as a smoke
//! test rather than a print-and-hope binary. Offline runs add no networking at
//! all, so the invariants never depend on the transport.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use bevy::prelude::*;
use kcc_avian::prelude::{MoveAndSlidePlugin, PhysicsPlugins};

use crate::character;
use crate::transport::{ClientNetPlugin, NetStats};
use crate::{Args, Local};

const FORWARD: i8 = -127;
const RIGHT: i8 = 127;

#[derive(Clone, Copy)]
struct Phase {
    name: &'static str,
    ticks: u32,
    move_x: i8,
    move_z: i8,
    buttons: u8,
}

fn phases() -> Vec<Phase> {
    vec![
        Phase {
            name: "walk_forward",
            ticks: 90,
            move_x: 0,
            move_z: FORWARD,
            buttons: 0,
        },
        Phase {
            name: "sprint_forward",
            ticks: 90,
            move_x: 0,
            move_z: FORWARD,
            buttons: proto::buttons::SPRINT,
        },
        Phase {
            name: "strafe_into_wall",
            ticks: 600,
            move_x: RIGHT,
            move_z: 0,
            buttons: proto::buttons::SPRINT,
        },
        Phase {
            name: "diagonal_slide",
            ticks: 300,
            move_x: RIGHT,
            move_z: FORWARD,
            buttons: proto::buttons::SPRINT,
        },
        Phase {
            name: "jump",
            ticks: 1,
            move_x: 0,
            move_z: 0,
            buttons: proto::buttons::JUMP,
        },
        Phase {
            name: "settle",
            ticks: 120,
            move_x: 0,
            move_z: 0,
            buttons: 0,
        },
    ]
}

fn total_ticks() -> u32 {
    phases().iter().map(|p| p.ticks).sum()
}

/// Where the script has got to, plus the extremes needed by the invariants.
#[derive(Resource)]
struct Script {
    phases: Vec<Phase>,
    index: usize,
    left: u32,
    budget: u32,
    ticks: u32,
    peak_y: f32,
    max_abs_x: f32,
    max_abs_z: f32,
    finished: bool,
}

impl Script {
    fn new(budget: u32) -> Self {
        let phases = phases();
        Self {
            left: phases[0].ticks,
            peak_y: 0.9,
            budget,
            phases,
            index: 0,
            ticks: 0,
            max_abs_x: 0.0,
            max_abs_z: 0.0,
            finished: false,
        }
    }
}

/// One script tick per fixed step, so the wire systems keep their normal place.
fn script_tick(mut local: ResMut<Local>, mut script: ResMut<Script>) {
    if script.finished {
        return;
    }
    if script.ticks >= script.budget || script.index >= script.phases.len() {
        script.finished = true;
        return;
    }

    let phase = script.phases[script.index];
    local.step(proto::InputFrame {
        move_x: phase.move_x,
        move_z: phase.move_z,
        buttons: phase.buttons,
        ..default()
    });
    script.ticks += 1;

    script.left -= 1;
    if script.left == 0 {
        script.index += 1;
        if let Some(next) = script.phases.get(script.index) {
            script.left = next.ticks;
        }
    }
}

/// Records the extremes the invariants need, after the step's pose is final.
fn track_extremes(local: Res<Local>, mut script: ResMut<Script>) {
    if script.ticks == 0 {
        return;
    }
    script.peak_y = script.peak_y.max(local.player.pos.y);
    script.max_abs_x = script.max_abs_x.max(local.player.pos.x.abs());
    script.max_abs_z = script.max_abs_z.max(local.player.pos.z.abs());
}

struct Report {
    pos: Vec3,
    vel: Vec3,
    grounded: bool,
    peak_y: f32,
    max_abs_x: f32,
    max_abs_z: f32,
    ticks: u32,
    sent: u64,
    received: u64,
    corrections: u64,
    resyncs: u64,
    peer_contacts: u64,
}

fn run_script(seconds: f32, connect: Option<SocketAddr>, player_id: u64) -> Report {
    let budget = ((seconds * proto::CLIENT_TICK_HZ as f32) as u32).max(1);

    let terrain = sim::StaticWorld::test_room();
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        PhysicsPlugins::new(FixedPostUpdate),
        MoveAndSlidePlugin::default(),
    ))
    .insert_resource(Time::<Fixed>::from_hz(proto::CLIENT_TICK_HZ as f64))
    .init_resource::<Local>()
    .init_resource::<NetStats>()
    .insert_resource(Script::new(budget))
    .add_systems(FixedUpdate, (script_tick, character::apply_local).chain())
    .add_systems(FixedLast, (character::read_back, track_extremes).chain());

    character::spawn_terrain(app.world_mut(), &terrain);
    character::spawn_player(app.world_mut());

    if let Some(addr) = connect {
        // A live run has to be debuggable; the offline run stays quiet so its
        // report is the only output.
        app.add_plugins(bevy::log::LogPlugin::default())
            .add_plugins(ClientNetPlugin {
                server: addr,
                player_id,
            });
    }

    // `App::run` would do this for us, but we drive `update` by hand. Lightyear
    // registers her message send/receive systems in `Plugin::finish`, so skipping
    // these two calls silently drops every packet in both directions.
    app.finish();
    app.cleanup();

    let live = connect.is_some();
    if !live {
        // Offline is driven one fixed step per update: the wall clock must not
        // decide how many steps a run takes, or two runs cannot be compared.
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
    }

    while !app.world().resource::<Script>().finished {
        if live {
            // The fixed clock follows real time, so the script advances at 60 Hz
            // and a round trip has time to complete.
            app.update();
            std::thread::sleep(Duration::from_millis(1));
        } else {
            let step = app.world().resource::<Time<Fixed>>().timestep();
            app.world_mut()
                .resource_mut::<Time<Fixed>>()
                .accumulate_overstep(step);
            app.update();
        }
    }

    let local = app.world().resource::<Local>();
    let script = app.world().resource::<Script>();
    let stats = app.world().resource::<NetStats>();
    Report {
        pos: local.player.pos,
        vel: local.player.vel,
        grounded: local.player.grounded,
        peak_y: script.peak_y,
        max_abs_x: script.max_abs_x,
        max_abs_z: script.max_abs_z,
        ticks: script.ticks,
        sent: stats.sent,
        received: stats.received,
        corrections: stats.corrections,
        resyncs: stats.resyncs,
        peer_contacts: local.peer_contacts,
    }
}

pub fn run(args: &Args) -> ExitCode {
    let a = run_script(args.seconds, None, args.player_id);
    let b = run_script(args.seconds, None, args.player_id);

    println!("script:");
    for p in phases() {
        println!("  {:<16} {:>4} ticks", p.name, p.ticks);
    }
    println!("phase_ticks     {}", a.ticks);
    println!("script_ticks    {}", total_ticks());
    println!("final_x         {:.4}", a.pos.x);
    println!("final_y         {:.4}", a.pos.y);
    println!("final_z         {:.4}", a.pos.z);
    println!("speed           {:.4}", a.vel.length());
    println!("grounded        {}", a.grounded);
    println!("peak_y          {:.4}", a.peak_y);
    println!("max_abs_x       {:.4}", a.max_abs_x);
    println!("max_abs_z       {:.4}", a.max_abs_z);
    println!("peer_contacts   {}", a.peer_contacts);

    let deterministic = a.pos == b.pos && a.vel == b.vel && a.ticks == b.ticks;
    println!("deterministic   {deterministic}");

    let mut failures = Vec::new();

    // Optional live round-trip: the script is replayed against a real server so
    // the encode/decode path and correction idempotency are exercised.
    if let Some(addr) = args.connect {
        println!("net_server      {addr}");
        let r = run_script(args.seconds, Some(addr), args.player_id);
        println!("net_sent        {}", r.sent);
        println!("net_snaps       {}", r.received);
        println!("net_corrections {}", r.corrections);
        println!("net_resyncs     {}", r.resyncs);
        println!("net_peer_hits   {}", r.peer_contacts);
        if r.received == 0 {
            failures.push("no snapshots from the server");
        }
    }

    if !deterministic {
        failures.push("two identical runs diverged");
    }
    if a.ticks < total_ticks() {
        failures.push("script was truncated by --seconds");
    }
    if a.max_abs_x > 9.2 || a.max_abs_z > 9.2 {
        failures.push("left the room bounds");
    }
    if a.peak_y < 1.1 {
        failures.push("jump never left the ground");
    }
    if !a.grounded {
        failures.push("did not settle back to the floor");
    }
    if (a.pos.y - 0.9).abs() > 0.05 {
        failures.push("resting height is wrong");
    }

    if failures.is_empty() {
        println!("result          ok");
        ExitCode::SUCCESS
    } else {
        for f in failures {
            println!("failure         {f}");
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CorrectionLog;

    #[test]
    fn corrections_are_idempotent() {
        let mut log = CorrectionLog::default();
        let c = proto::CollisionCorrection {
            seq: 4,
            tick: proto::Tick::new(10),
            epoch: 1,
            velocity_delta: [0.5, 0.0, 0.0],
            hard_normal: None,
        };
        assert!(log.accept(&c));
        assert!(!log.accept(&c), "duplicate applied twice");
        assert_eq!(log.dropped, 1);

        let newer = proto::CollisionCorrection { seq: 5, ..c };
        assert!(log.accept(&newer));

        // A new epoch resets the ordering: reconnect/handover.
        let other_epoch = proto::CollisionCorrection {
            seq: 1,
            epoch: 2,
            ..c
        };
        assert!(log.accept(&other_epoch));
    }

    #[test]
    fn a_resent_snapshot_is_applied_once() {
        let mut local = Local::default();
        let snapshot = proto::ServerSnapshot {
            correction: Some(proto::CollisionCorrection {
                seq: 4,
                tick: proto::Tick::new(10),
                epoch: 1,
                velocity_delta: [0.5, 0.0, 0.0],
                hard_normal: None,
            }),
            ..default()
        };
        assert!(local.accept_snapshot(&snapshot));
        let after_first = local.player.vel;
        assert!(!local.accept_snapshot(&snapshot), "duplicate applied twice");
        assert_eq!(local.player.vel, after_first);
    }

    #[test]
    fn hard_normal_removes_velocity_into_the_wall() {
        let mut local = Local::default();
        local.player.vel = Vec3::new(-4.0, 0.0, 0.0);
        let applied = local.accept_snapshot(&proto::ServerSnapshot {
            correction: Some(proto::CollisionCorrection {
                seq: 1,
                tick: proto::Tick::new(1),
                epoch: 1,
                velocity_delta: [0.0, 0.0, 0.0],
                hard_normal: Some([1.0, 0.0, 0.0]),
            }),
            ..default()
        });
        assert!(applied);
        assert!(
            local.player.vel.x >= -1e-4,
            "velocity into the wall survived: {}",
            local.player.vel.x
        );
    }
}
