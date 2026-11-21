//! Client: Bevy window with a local KCC solve, plus a headless CI mode.
//!
//! The client owns terrain and its own position. The server only ever sends
//! collision corrections and peer colliders, so nothing here waits on a
//! round-trip to move.

mod app;
mod headless;
mod net;

use bevy::math::Vec3;
use std::time::Duration;

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match args.mode {
        Mode::Window => {
            app::run(&args);
            std::process::ExitCode::SUCCESS
        }
        Mode::Headless => headless::run(&args),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Window,
    Headless,
}

#[derive(Clone, Debug)]
pub struct Args {
    pub mode: Mode,
    pub seconds: f32,
    pub connect: Option<String>,
    pub player_id: u64,
}

impl Args {
    fn parse() -> Self {
        let mut out = Self {
            mode: Mode::Window,
            seconds: 30.0,
            connect: None,
            player_id: 1,
        };
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--headless" => out.mode = Mode::Headless,
                "--seconds" => out.seconds = it.next().and_then(|v| v.parse().ok()).unwrap_or(30.0),
                "--connect" => out.connect = it.next(),
                "--player" => out.player_id = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
                "--help" | "-h" => {
                    println!(
                        "usage: client [--headless] [--seconds S] [--connect ADDR] [--player ID]\n\
                         \n  --headless      run a scripted session, print metrics, exit\n\
                         \n  --seconds S     safety cap on simulated seconds (default 30)\n\
                         \n  --connect ADDR  send reports to a UDP server, e.g. 127.0.0.1:4000\n\
                         \n  --player ID     numeric player id (default 1)\n\
                         \n  window controls: WASD move, Q/E turn camera, Space jump, Shift sprint\n"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("unknown argument: {other}"),
            }
        }
        out
    }
}

/// Shared local pose, used by both the window and headless paths.
pub struct LocalPlayer {
    pub pos: Vec3,
    pub vel: Vec3,
    pub grounded: bool,
    pub yaw: f32,
    pub kcc: sim::kcc::KccParams,
    pub move_params: sim::movement::MoveParams,
}

impl LocalPlayer {
    pub fn spawn() -> Self {
        Self {
            // Capsule centre resting on the floor: half_height + radius = 0.9.
            pos: Vec3::new(0.0, 0.9, 0.0),
            vel: Vec3::ZERO,
            grounded: true,
            yaw: 0.0,
            kcc: sim::kcc::KccParams::default(),
            move_params: sim::movement::MoveParams::default(),
        }
    }

    /// One fixed tick: velocity model, then the terrain solve.
    pub fn tick(&mut self, world: &sim::StaticWorld, input: &proto::InputFrame, dt: f32) {
        self.vel =
            sim::movement::step_velocity(&self.move_params, input, self.vel, self.grounded, dt);
        let out = sim::move_and_slide(world, &self.kcc, self.pos, self.vel, dt);
        self.pos = out.position;
        self.vel = out.velocity;
        self.grounded = out.grounded;
    }

    /// Applies a server correction as a velocity delta. Idempotency is the
    /// caller's job: a correction may be delivered more than once.
    pub fn apply_correction(&mut self, c: &proto::CollisionCorrection) {
        self.vel += Vec3::from_array(c.velocity_delta);
        if let Some(n) = c.hard_normal {
            let n = Vec3::from_array(n);
            let vn = self.vel.dot(n);
            if vn < 0.0 {
                self.vel -= n * vn;
            }
        }
    }
}

/// Tracks which correction sequence numbers were already applied.
#[derive(Default)]
pub struct CorrectionLog {
    last: Option<(u16, u32)>,
    pub applied: u64,
    pub dropped: u64,
}

impl CorrectionLog {
    /// Highest accepted sequence number in the current epoch, echoed back to the
    /// server so it can stop re-sending corrections the client already applied.
    pub fn last_seq(&self) -> u16 {
        self.last.map(|(s, _)| s).unwrap_or(0)
    }

    /// Returns true when the correction should be applied.
    pub fn accept(&mut self, c: &proto::CollisionCorrection) -> bool {
        if let Some((seq, epoch)) = self.last {
            // Same epoch and older or equal sequence: a duplicate.
            if epoch == c.epoch && seq.wrapping_sub(c.seq) < 0x8000 {
                self.dropped += 1;
                return false;
            }
        }
        self.last = Some((c.seq, c.epoch));
        self.applied += 1;
        true
    }
}

pub const INPUT_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / proto::INPUT_SEND_HZ as u64);
