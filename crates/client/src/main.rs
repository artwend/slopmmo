//! Client: Bevy window with a local KCC solve, plus a headless CI mode.
//!
//! The client owns terrain and its own position. The server only ever sends
//! collision corrections and peer colliders, so nothing here waits on a
//! round-trip to move.

mod app;
mod headless;
mod transport;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use bevy::prelude::{Resource, Vec3};

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
    /// Parses `--connect` into an address, exiting with code 2 if it is unusable.
    pub fn connect_addr(&self) -> Option<SocketAddr> {
        let raw = self.connect.as_ref()?;
        match raw.parse() {
            Ok(addr) => Some(addr),
            Err(e) => {
                eprintln!("invalid --connect address {raw:?}: {e}");
                std::process::exit(2);
            }
        }
    }

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

/// Shared world geometry for both the local solve and the rendered boxes.
#[derive(Resource)]
pub struct WorldRes(pub sim::StaticWorld);

/// Everything the client's own tick needs, shared by the window and the script.
///
/// Both modes run exactly this code, so a headless pass means something for the
/// windowed build.
#[derive(Resource)]
pub struct Local {
    pub player: LocalPlayer,
    pub tick: proto::Tick,
    pub seq: u16,
    /// Newest-first inputs, re-sent with each report so a lost one can be filled in.
    pub recent: VecDeque<proto::InputFrame>,
    pub corrections: CorrectionLog,
    /// Tick of the last report, so a redelivered frame cannot double-send.
    sent_at: Option<proto::Tick>,
}

impl Default for Local {
    fn default() -> Self {
        Self {
            player: LocalPlayer::spawn(),
            tick: proto::Tick::ZERO,
            seq: 0,
            recent: VecDeque::with_capacity(proto::MAX_INPUT_FRAMES),
            corrections: CorrectionLog::default(),
            sent_at: None,
        }
    }
}

impl Local {
    /// One fixed tick with `input`; the tick number is stamped on the frame.
    pub fn step(&mut self, world: &sim::StaticWorld, input: proto::InputFrame) {
        self.tick = self.tick.add(1);
        let input = proto::InputFrame {
            tick: self.tick,
            ..input
        };
        self.player.tick(world, &input, proto::CLIENT_TICK_DT);
        self.recent.push_front(input);
        self.recent.truncate(proto::MAX_INPUT_FRAMES);
    }

    /// The report due for the current tick, or `None` off-cadence.
    pub fn report(&mut self, player_id: u64) -> Option<proto::PlayerMovementPacket> {
        if self.sent_at == Some(self.tick) {
            return None;
        }
        let every = (proto::CLIENT_TICK_HZ / proto::INPUT_SEND_HZ).max(1);
        if !self.tick.0.is_multiple_of(every) {
            return None;
        }
        self.sent_at = Some(self.tick);
        self.seq = self.seq.wrapping_add(1);
        Some(proto::PlayerMovementPacket {
            seq: self.seq,
            player: player_id,
            report: proto::MovementReport {
                tick: self.tick,
                position: self.player.pos.to_array(),
                velocity: self.player.vel.to_array(),
                body_yaw: self.player.yaw,
                grounded: self.player.grounded,
                applied_correction_seq: self.corrections.last_seq(),
            },
            inputs: self.recent.iter().copied().collect(),
        })
    }

    /// Applies a snapshot's placement and correction; false when no correction
    /// was applied (a resync-only snapshot returns false).
    pub fn accept_snapshot(&mut self, snapshot: &proto::ServerSnapshot) -> bool {
        if snapshot.resync {
            // A resumed session is placed by the server. This is the one case
            // where the shadow position is adopted instead of predicted;
            // dropping the local velocity stops the snap from turning into a
            // burst of movement.
            self.player.pos = Vec3::from_array(snapshot.shadow_position);
            self.player.vel = Vec3::ZERO;
        }
        let Some(c) = &snapshot.correction else {
            return false;
        };
        if !self.corrections.accept(c) {
            return false;
        }
        self.player.apply_correction(c);
        true
    }
}
