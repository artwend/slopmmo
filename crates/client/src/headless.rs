//! Scripted, deterministic client run. This is what CI executes.
//!
//! Exits non-zero when a movement invariant breaks, so it is usable as a smoke
//! test rather than a print-and-hope binary.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use bevy::math::Vec3;

use crate::net::ClientLink;
use crate::{Args, CorrectionLog, LocalPlayer};

const FORWARD: i8 = -127;
const RIGHT: i8 = 127;

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

struct Report {
    pos: Vec3,
    vel: Vec3,
    grounded: bool,
    peak_y: f32,
    max_abs_x: f32,
    max_abs_z: f32,
    ticks: u32,
    sent: u64,
    snaps: u64,
    corrections: u64,
}

fn total_ticks() -> u32 {
    phases().iter().map(|p| p.ticks).sum()
}

fn run_script(seconds_limit: f32, mut net: Option<&mut ClientLink>) -> Report {
    let world = sim::StaticWorld::test_room();
    let mut player = LocalPlayer::spawn();
    let mut log = CorrectionLog::default();
    let budget = (seconds_limit * proto::TICK_HZ as f32) as u32;
    let send_every = (proto::TICK_HZ / proto::INPUT_SEND_HZ).max(1);
    // With a live server the script has to run at real time, otherwise the whole
    // session is over before a single round-trip can complete.
    let pacer = net.as_ref().map(|_| Instant::now());

    let mut tick = 0u32;
    let mut seq = 0u16;
    let mut corrections = 0u64;
    let mut peak_y = player.pos.y;
    let mut max_abs_x: f32 = 0.0;
    let mut max_abs_z: f32 = 0.0;

    for phase in phases() {
        for _ in 0..phase.ticks {
            if tick >= budget {
                break;
            }
            let input = proto::InputFrame {
                tick: proto::Tick::new(tick),
                move_x: phase.move_x,
                move_z: phase.move_z,
                buttons: phase.buttons,
                camera_yaw: 0,
            };
            player.tick(&world, &input, proto::TICK_DT);
            tick += 1;
            peak_y = peak_y.max(player.pos.y);
            max_abs_x = max_abs_x.max(player.pos.x.abs());
            max_abs_z = max_abs_z.max(player.pos.z.abs());

            if let Some(link) = net.as_deref_mut() {
                for snap in link.drain_snapshots() {
                    if let Some(c) = &snap.correction
                        && log.accept(c)
                    {
                        player.apply_correction(c);
                        corrections += 1;
                    }
                }
                if tick.is_multiple_of(send_every) {
                    seq = seq.wrapping_add(1);
                    link.send(&proto::PlayerMovementPacket {
                        seq,
                        player: link.player_id,
                        report: proto::MovementReport {
                            tick: proto::Tick::new(tick),
                            position: player.pos.to_array(),
                            velocity: player.vel.to_array(),
                            body_yaw: player.yaw,
                            grounded: player.grounded,
                            applied_correction_seq: log.last_seq(),
                        },
                        inputs: Vec::new(),
                    });
                }
            }

            if let Some(start) = pacer {
                let want = f64::from(tick) * f64::from(proto::TICK_DT);
                let elapsed = start.elapsed().as_secs_f64();
                if want > elapsed {
                    std::thread::sleep(Duration::from_secs_f64(want - elapsed));
                }
            }
        }
    }

    Report {
        pos: player.pos,
        vel: player.vel,
        grounded: player.grounded,
        peak_y,
        max_abs_x,
        max_abs_z,
        ticks: tick,
        sent: net.as_ref().map_or(0, |l| l.sent),
        snaps: net.as_ref().map_or(0, |l| l.received),
        corrections,
    }
}

pub fn run(args: &Args) -> ExitCode {
    let a = run_script(args.seconds, None);
    let b = run_script(args.seconds, None);

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

    let deterministic = a.pos == b.pos && a.vel == b.vel && a.ticks == b.ticks;
    println!("deterministic   {deterministic}");

    let mut failures = Vec::new();

    // Optional live round-trip: the script is replayed against a real server so
    // the encode/decode path and the correction idempotency are exercised.
    if let Some(addr) = &args.connect {
        match ClientLink::connect(addr, args.player_id) {
            Ok(mut link) => {
                println!("net_peer        {}", link.peer());
                let r = run_script(args.seconds, Some(&mut link));
                println!("net_sent        {}", r.sent);
                println!("net_snaps       {}", r.snaps);
                println!("net_corrections {}", r.corrections);
                println!("net_malformed   {}", link.malformed);
                if r.snaps == 0 {
                    failures.push("no snapshots from the server");
                }
                if link.malformed > 0 {
                    failures.push("decoded a malformed snapshot");
                }
            }
            Err(e) => {
                eprintln!("connect to {addr} failed: {e}");
                failures.push("could not connect to the server");
            }
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
    use crate::{CorrectionLog, LocalPlayer};

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
    fn hard_normal_removes_velocity_into_the_wall() {
        let mut p = LocalPlayer::spawn();
        p.vel = Vec3::new(-4.0, 0.0, 0.0);
        p.apply_correction(&proto::CollisionCorrection {
            seq: 1,
            tick: proto::Tick::new(1),
            epoch: 1,
            velocity_delta: [0.0, 0.0, 0.0],
            hard_normal: Some([1.0, 0.0, 0.0]),
        });
        assert!(
            p.vel.x >= -1e-4,
            "velocity into the wall survived: {}",
            p.vel.x
        );
    }
}
