//! Client: Bevy window with an Avian virtual character, plus a headless CI mode.
//!
//! The client owns terrain and its own position. The server only ever sends
//! collision corrections and peer colliders, so nothing here waits on a
//! round-trip to move.

mod app;
mod character;
mod headless;
mod transport;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use bevy::prelude::{Resource, Vec3};
use clap::Parser;

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    if args.headless {
        headless::run(&args)
    } else {
        app::run(&args);
        std::process::ExitCode::SUCCESS
    }
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "client",
    about = "Bevy window with an Avian virtual character, plus a headless CI mode",
    after_help = "window controls: WASD move, Q/E turn camera, Space jump, Shift sprint"
)]
pub struct Args {
    /// run a scripted session, print metrics, exit
    #[arg(long)]
    pub headless: bool,
    /// safety cap on simulated seconds
    #[arg(long, value_name = "S", default_value_t = 30.0)]
    pub seconds: f32,
    /// send reports to a UDP server, e.g. 127.0.0.1:4000
    #[arg(long, value_name = "ADDR")]
    pub connect: Option<SocketAddr>,
    /// numeric player id
    #[arg(long = "player", value_name = "ID", default_value_t = 1)]
    pub player_id: u64,
}

/// Shared local pose, used by both the window and headless paths.
pub struct LocalPlayer {
    pub pos: Vec3,
    pub vel: Vec3,
    pub grounded: bool,
    pub yaw: f32,
    pub move_params: sim::movement::MoveParams,
}

impl LocalPlayer {
    pub fn spawn() -> Self {
        Self {
            pos: character::spawn_position(),
            vel: Vec3::ZERO,
            grounded: true,
            yaw: 0.0,
            move_params: sim::movement::MoveParams::default(),
        }
    }

    /// One fixed tick of the velocity model.
    ///
    /// The geometry solve happens after this, in Avian's physics step; the
    /// movement model reads `grounded` from the previous step's `Grounded`
    /// marker.
    pub fn tick(&mut self, input: &proto::InputFrame, dt: f32) {
        self.vel =
            sim::movement::step_velocity(&self.move_params, input, self.vel, self.grounded, dt);
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

/// What the last report put on the wire, so a tick that says the same thing can
/// stay silent instead of being described again.
#[derive(Clone, Copy)]
struct SentReport {
    tick: proto::Tick,
    position: [f32; 3],
    grounded: bool,
    applied_correction_seq: u16,
}

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
    /// Last report put on the wire, or `None` before the first one.
    last_sent: Option<SentReport>,
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
            last_sent: None,
        }
    }
}

impl Local {
    /// One fixed tick with `input`; the tick number is stamped on the frame.
    ///
    /// The character entity is moved later in the same fixed step, by
    /// `MoveAndSlidePlugin`; `character::read_back` copies the result into
    /// `player` before the next tick.
    pub fn step(&mut self, input: proto::InputFrame) {
        self.tick = self.tick.add(1);
        let input = proto::InputFrame {
            tick: self.tick,
            ..input
        };
        self.player.tick(&input, proto::CLIENT_TICK_DT);
        self.recent.push_front(input);
        self.recent.truncate(proto::MAX_INPUT_FRAMES);
    }

    /// The report due for the current tick, or `None` off-cadence or idle.
    ///
    /// Off-cadence ticks stay silent because input is tick-addressed. An idle
    /// tick is also silent: no input, no motion, an unchanged pose and no new
    /// correction mean the next report would repeat the last one, so only the
    /// [`proto::IDLE_SEND_HZ`] keepalive goes out. Keepalives carry no redundant
    /// input frames — there is nothing to recover.
    pub fn report(&mut self, player_id: u64) -> Option<proto::PlayerMovementPacket> {
        if self.sent_at == Some(self.tick) {
            return None;
        }
        let every = (proto::CLIENT_TICK_HZ / proto::INPUT_SEND_HZ).max(1);
        if !self.tick.0.is_multiple_of(every) {
            return None;
        }

        let report = proto::MovementReport {
            tick: self.tick,
            position: self.player.pos.to_array(),
            velocity: self.player.vel.to_array(),
            body_yaw: self.player.yaw,
            grounded: self.player.grounded,
            applied_correction_seq: self.corrections.last_seq(),
        };
        let idle = self.idle(&report);
        let keepalive_due = self
            .last_sent
            .is_none_or(|sent| sent.tick.since(self.tick) >= proto::IDLE_SEND_TICKS as i32);
        if idle && !keepalive_due {
            return None;
        }

        self.sent_at = Some(self.tick);
        self.seq = self.seq.wrapping_add(1);
        let inputs = if idle {
            Vec::new()
        } else {
            self.recent.iter().copied().collect()
        };
        self.last_sent = Some(SentReport {
            tick: self.tick,
            position: report.position,
            grounded: report.grounded,
            applied_correction_seq: report.applied_correction_seq,
        });
        Some(proto::PlayerMovementPacket {
            seq: self.seq,
            player: player_id,
            report,
            inputs,
        })
    }

    /// True when a report says nothing the server has not already been told:
    /// no input, effectively no velocity, an unchanged pose and no new
    /// correction to acknowledge. Any of those changing is a fact the shadow
    /// needs, so the report is not idle.
    fn idle(&self, report: &proto::MovementReport) -> bool {
        let Some(sent) = self.last_sent else {
            return false;
        };
        let newest = self.recent.front().copied().unwrap_or_default();
        newest.move_x == 0
            && newest.move_z == 0
            && newest.buttons == 0
            && report.grounded == sent.grounded
            && report.applied_correction_seq == sent.applied_correction_seq
            && report.velocity.iter().map(|v| v * v).sum::<f32>()
                <= proto::IDLE_SPEED_EPSILON * proto::IDLE_SPEED_EPSILON
            && report
                .position
                .iter()
                .zip(sent.position.iter())
                .all(|(a, b)| (a - b).abs() <= 1.0 / proto::QUANT_MM)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_client_sends_only_the_keepalive() {
        let mut local = Local::default();
        let first = local.report(7).expect("the first report is always sent");
        assert_eq!(first.seq, 1);

        let mut sent = 0;
        // Two seconds of ticks: the 30 Hz cadence would send 60 packets.
        for _ in 0..(proto::CLIENT_TICK_HZ * 2) {
            local.step(proto::InputFrame::default());
            if let Some(packet) = local.report(7) {
                assert!(packet.inputs.is_empty(), "idle keepalive carried input");
                sent += 1;
            }
        }
        let expected = 2 * proto::IDLE_SEND_HZ as i32;
        assert!(
            (sent - expected).abs() <= 1,
            "idle client sent {sent} reports, expected ~{expected}"
        );
    }

    #[test]
    fn a_moving_client_keeps_the_full_cadence() {
        let mut local = Local::default();
        local.player.vel = Vec3::new(0.0, 0.0, -proto::MAX_SPEED);
        let mut sent = 0;
        for _ in 0..(proto::CLIENT_TICK_HZ * 2) {
            local.step(proto::InputFrame {
                move_z: -127,
                ..Default::default()
            });
            if let Some(packet) = local.report(7) {
                assert!(!packet.inputs.is_empty(), "movement report lost redundancy");
                sent += 1;
            }
        }
        // Two seconds at the 30 Hz cadence.
        assert_eq!(sent, 2 * proto::INPUT_SEND_HZ as usize);
    }

    #[test]
    fn an_idle_client_reports_a_fresh_correction_ack() {
        let mut local = Local::default();
        local.report(7).expect("first report");
        local.corrections.accept(&proto::CollisionCorrection {
            seq: 1,
            tick: proto::Tick::new(1),
            epoch: 1,
            velocity_delta: [0.0; 3],
            hard_normal: None,
        });

        let mut acked = None;
        for _ in 0..proto::IDLE_SEND_TICKS {
            local.step(proto::InputFrame::default());
            if let Some(packet) = local.report(7) {
                acked = Some(packet.report.applied_correction_seq);
                break;
            }
        }
        assert_eq!(acked, Some(1), "the correction ack was suppressed");
    }
}
