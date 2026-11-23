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
    /// Yaw rate in rad/s produced by the camera, reported so the server can
    /// extrapolate rotation between reports.
    pub angular_velocity: Vec3,
    pub grounded: bool,
    pub yaw: f32,
    pub move_params: sim::movement::MoveParams,
}

impl LocalPlayer {
    pub fn spawn() -> Self {
        Self {
            pos: character::spawn_position(),
            vel: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
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

/// Anchor rate while the reported state is unchanged but the character moves.
///
/// The server dead-reckons `pos + vel × age` from the last report, so a steady
/// velocity needs no per-tick description. The anchor only bounds how long a
/// *lost* velocity change can leave the shadow on the old value.
const ANCHOR_SEND_HZ: u32 = 10;
const ANCHOR_SEND_TICKS: u32 = proto::CLIENT_TICK_HZ / ANCHOR_SEND_HZ;

/// Linear-velocity change the server cannot infer from the last report.
const REPORT_VELOCITY_EPSILON: f32 = 0.1;
/// Yaw-rate change that must be reported: the shadow extrapolates the last rate,
/// so a stopped turn would otherwise keep rotating at the old speed.
const REPORT_ANGULAR_EPSILON: f32 = 0.05;
/// Dead-reckoning error that must be corrected even when the velocity matches.
const REPORT_POSITION_EPSILON: f32 = 0.02;

/// What the last report put on the wire, so a tick the server can already infer
/// stays silent instead of describing the same state again.
#[derive(Clone, Copy)]
struct SentReport {
    tick: proto::Tick,
    position: [f32; 3],
    linear_velocity: [f32; 3],
    angular_velocity: [f32; 3],
    grounded: bool,
    applied_correction_seq: u16,
    input: proto::InputFrame,
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
    /// History-tracked peer colliders and the local solve against them.
    pub peers: sim::peers::LocalPeerSolve,
    /// Anchor mapping the client tick domain onto the server's. Set from the
    /// first snapshot; used to look the collider set up by server tick.
    server_origin: Option<(proto::Tick, proto::Tick)>,
    /// Contacts the local peer solve produced, for diagnostics and tests.
    pub peer_contacts: u64,
    pub peer_hard_contacts: u64,
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
            peers: sim::peers::LocalPeerSolve::default(),
            server_origin: None,
            peer_contacts: 0,
            peer_hard_contacts: 0,
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
        // The peer solve runs before the terrain step, so Avian moves the
        // character with the impulse already folded into its velocity. Peers
        // are read from the history at the tick this one predicts, never from
        // "the latest snapshot": that is what keeps a replay reproducible.
        self.solve_peers();
        self.recent.push_front(input);
        self.recent.truncate(proto::MAX_INPUT_FRAMES);
    }

    /// Solves against the peer colliders the server had at the predicted tick.
    fn solve_peers(&mut self) {
        let Some(tick) = self.predicted_server_tick() else {
            return;
        };
        let local = sim::collision::Body::player(0, self.player.pos, self.player.vel);
        let impulse = self.peers.solve(tick, &local, proto::CORRECTION_BUDGET);
        self.peer_contacts += u64::from(self.peers.contacts);
        self.peer_hard_contacts += u64::from(self.peers.hard_contacts);
        if impulse.delta_v != Vec3::ZERO {
            self.player.vel += impulse.delta_v;
        }
    }

    /// The server tick this client tick predicts, or `None` before the first
    /// snapshot has anchored the two domains.
    fn predicted_server_tick(&self) -> Option<proto::Tick> {
        self.server_origin
            .map(|origin| proto::map_client_tick(origin, self.tick, proto::SERVER_TICK_HZ))
    }

    /// The report due for the current tick, or `None` when the server can already
    /// infer the state and no anchor is due.
    ///
    /// Off-cadence ticks stay silent because input is tick-addressed. On the
    /// cadence, the report is sent when it carries something the shadow cannot
    /// derive — changed input, grounding, velocity, yaw rate, a correction
    /// acknowledgement, or a pose that no longer matches dead reckoning.
    /// Otherwise only an anchor goes out: `ANCHOR_SEND_HZ` while the character
    /// moves and [`proto::IDLE_SEND_HZ`] while it is stationary. Anchors carry
    /// no redundant input frames — there is no new input to recover.
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
            linear_velocity: self.player.vel.to_array(),
            angular_velocity: self.player.angular_velocity.to_array(),
            body_yaw: self.player.yaw,
            grounded: self.player.grounded,
            applied_correction_seq: self.corrections.last_seq(),
        };
        let newest = self.recent.front().copied().unwrap_or_default();
        let unchanged = self.unchanged(&report, &newest);
        let due = if unchanged {
            let interval = if self.stationary(&report) {
                proto::IDLE_SEND_TICKS
            } else {
                ANCHOR_SEND_TICKS
            };
            self.last_sent
                .is_none_or(|sent| sent.tick.since(self.tick) >= interval as i32)
        } else {
            true
        };
        if !due {
            return None;
        }

        self.sent_at = Some(self.tick);
        self.seq = self.seq.wrapping_add(1);
        let inputs = if unchanged {
            Vec::new()
        } else {
            self.recent.iter().copied().collect()
        };
        self.last_sent = Some(SentReport {
            tick: self.tick,
            position: report.position,
            linear_velocity: report.linear_velocity,
            angular_velocity: report.angular_velocity,
            grounded: report.grounded,
            applied_correction_seq: report.applied_correction_seq,
            input: newest,
        });
        Some(proto::PlayerMovementPacket {
            version: proto::PROTOCOL_VERSION,
            seq: self.seq,
            player: player_id,
            report,
            inputs,
        })
    }

    fn stationary(&self, report: &proto::MovementReport) -> bool {
        report.linear_velocity.iter().map(|v| v * v).sum::<f32>()
            <= proto::IDLE_SPEED_EPSILON * proto::IDLE_SPEED_EPSILON
    }

    /// True when the new report tells the server nothing it could not infer from
    /// the last one: same input, grounding and correction ack, no yaw rate, a
    /// matching linear velocity, and a pose its dead reckoning already predicts.
    ///
    /// A turn keeps the full cadence: the shadow extrapolates yaw from the last
    /// reported rate but does not smooth it, so it is described while it lasts.
    fn unchanged(&self, report: &proto::MovementReport, newest: &proto::InputFrame) -> bool {
        let Some(sent) = self.last_sent else {
            return false;
        };
        if newest.move_x != sent.input.move_x
            || newest.move_z != sent.input.move_z
            || newest.buttons != sent.input.buttons
            || report.grounded != sent.grounded
            || report.applied_correction_seq != sent.applied_correction_seq
        {
            return false;
        }
        let angular = Vec3::from_array(report.angular_velocity);
        if angular.distance_squared(Vec3::from_array(sent.angular_velocity))
            > REPORT_ANGULAR_EPSILON * REPORT_ANGULAR_EPSILON
            || angular.length_squared() > REPORT_ANGULAR_EPSILON * REPORT_ANGULAR_EPSILON
        {
            return false;
        }

        let velocity = Vec3::from_array(report.linear_velocity);
        let sent_velocity = Vec3::from_array(sent.linear_velocity);
        if velocity.distance_squared(sent_velocity)
            > REPORT_VELOCITY_EPSILON * REPORT_VELOCITY_EPSILON
        {
            return false;
        }

        let age = sent.tick.since(report.tick).max(0) as f32;
        let predicted =
            Vec3::from_array(sent.position) + sent_velocity * (age * proto::CLIENT_TICK_DT);
        Vec3::from_array(report.position).distance_squared(predicted)
            <= REPORT_POSITION_EPSILON * REPORT_POSITION_EPSILON
    }

    /// Applies a snapshot's placement and correction; false when no correction
    /// was applied (a resync-only snapshot returns false).
    ///
    /// `one_way_ticks` is the measured one-way delay, used only to anchor the
    /// client's clock to the server's present tick when filing the collider set.
    pub fn accept_snapshot(
        &mut self,
        snapshot: &proto::ServerSnapshot,
        one_way_ticks: i32,
    ) -> bool {
        self.record_peers(snapshot, one_way_ticks);
        if snapshot.resync {
            // A resumed session is placed by the server. This is the one case
            // where the shadow position is adopted instead of predicted;
            // dropping the local velocity stops the snap from turning into a
            // burst of movement or rotation.
            self.player.pos = Vec3::from_array(snapshot.shadow_position);
            self.player.vel = Vec3::ZERO;
            self.player.angular_velocity = Vec3::ZERO;
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

    /// Files the snapshot's collider set under the server tick it describes.
    ///
    /// The set is keyed by tick, not by arrival order, so two ticks that read
    /// the same history see the same colliders and a late datagram cannot
    /// rewrite an older tick's answer.
    ///
    /// The clock anchor is biased by the one-way delay: the snapshot was built
    /// `one_way_ticks` ago, so the server's *present* tick is that much ahead of
    /// it. Without the bias every local solve would be resolved against peers one
    /// round trip stale. The collider set itself stays filed at its own tick — it
    /// is genuine data for that tick, not for the present one.
    fn record_peers(&mut self, snapshot: &proto::ServerSnapshot, one_way_ticks: i32) {
        let observed = snapshot.server_tick.add(one_way_ticks.max(0) as u32);
        match self.server_origin {
            Some(origin) => {
                let mapped = proto::map_client_tick(origin, self.tick, proto::SERVER_TICK_HZ);
                // A paused, hitched or restarted client drifts out of the
                // anchored domain; re-anchor instead of sampling a mapping that
                // no longer describes the stream.
                if mapped.since(observed).abs() > proto::REPORT_WINDOW as i32 {
                    self.server_origin = Some((self.tick, observed));
                }
            }
            None => self.server_origin = Some((self.tick, observed)),
        }
        self.peers.record(snapshot.server_tick, &snapshot.peers);
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
    fn a_turning_client_keeps_the_full_cadence() {
        let mut local = Local::default();
        local.report(7).expect("the first report is always sent");
        local.player.angular_velocity = Vec3::new(0.0, 2.5, 0.0);

        let mut sent = 0;
        for _ in 0..(proto::CLIENT_TICK_HZ * 2) {
            local.step(proto::InputFrame::default());
            if let Some(packet) = local.report(7) {
                assert!(
                    !packet.inputs.is_empty(),
                    "a turning client sent a keepalive"
                );
                sent += 1;
            }
        }
        assert_eq!(sent, 2 * proto::INPUT_SEND_HZ as usize);
    }

    /// Steps the local tick and advances the pose the way Avian does after it,
    /// so dead reckoning can be compared against real movement.
    fn step_and_move(local: &mut Local, input: proto::InputFrame) {
        local.step(input);
        local.player.pos += local.player.vel * proto::CLIENT_TICK_DT;
    }

    #[test]
    fn a_steady_walk_sends_anchors_not_every_tick() {
        let mut local = Local::default();
        let input = proto::InputFrame {
            move_z: -127,
            ..Default::default()
        };
        // Converge to the steady velocity first; that phase legitimately reports
        // every cadence tick.
        for _ in 0..proto::CLIENT_TICK_HZ {
            step_and_move(&mut local, input);
            local.report(7);
        }

        let mut sent = 0;
        for _ in 0..(proto::CLIENT_TICK_HZ * 2) {
            step_and_move(&mut local, input);
            if let Some(packet) = local.report(7) {
                assert!(packet.inputs.is_empty(), "anchor carried redundancy frames");
                sent += 1;
            }
        }
        let expected = 2 * ANCHOR_SEND_HZ as i32;
        assert!(
            (sent - expected).abs() <= 2,
            "steady walk sent {sent} reports, expected ~{expected}"
        );
    }

    #[test]
    fn a_velocity_change_leaves_on_the_next_cadence_tick() {
        let mut local = Local::default();
        let walk = proto::InputFrame {
            move_z: -127,
            ..Default::default()
        };
        for _ in 0..proto::CLIENT_TICK_HZ {
            step_and_move(&mut local, walk);
            local.report(7);
        }

        let sprint = proto::InputFrame {
            move_z: -127,
            buttons: proto::buttons::SPRINT,
            ..Default::default()
        };
        let mut reported = None;
        for _ in 0..2 {
            step_and_move(&mut local, sprint);
            if let Some(packet) = local.report(7) {
                reported = Some(packet);
            }
        }
        let packet = reported.expect("a velocity change was suppressed");
        assert!(
            packet.inputs.iter().any(|f| f.sprint()),
            "the changed input was not reported with it"
        );
    }

    #[test]
    fn the_end_of_a_turn_is_reported_on_the_next_cadence_tick() {
        let mut local = Local::default();
        local.report(7).expect("first report");
        local.player.angular_velocity = Vec3::new(0.0, 2.5, 0.0);
        for _ in 0..4 {
            local.step(proto::InputFrame::default());
            local.report(7);
        }

        // The shadow extrapolates the reported yaw rate, so stopping a turn is a
        // fact it must hear before the next anchor.
        local.player.angular_velocity = Vec3::ZERO;
        let mut stopped = None;
        for _ in 0..2 {
            local.step(proto::InputFrame::default());
            if let Some(packet) = local.report(7) {
                stopped = Some(packet.report.angular_velocity);
            }
        }
        assert_eq!(stopped, Some([0.0; 3]), "the stopped turn was suppressed");
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

    fn snapshot_with_peers(
        server_tick: u32,
        peers: &[proto::PeerCollider],
    ) -> proto::ServerSnapshot {
        proto::ServerSnapshot {
            server_tick: proto::Tick::new(server_tick),
            peers: peers.iter().copied().collect(),
            ..Default::default()
        }
    }

    fn peer_at(player: u64, x: f32) -> proto::PeerCollider {
        proto::PeerCollider {
            player,
            position: [x, 0.9, 0.0],
            body_yaw: 0.0,
            mode: proto::peer_mode::SOFT,
        }
    }

    #[test]
    fn a_replicated_peer_is_solved_locally_before_any_correction_arrives() {
        let mut with_peer = Local::default();
        with_peer.player.pos = Vec3::new(0.0, 0.9, 0.0);
        with_peer.player.vel = Vec3::new(3.0, 0.0, 0.0);
        with_peer.accept_snapshot(&snapshot_with_peers(10, &[peer_at(2, 0.5)]), 0);
        with_peer.step(proto::InputFrame::default());

        let mut control = Local::default();
        control.player.pos = Vec3::new(0.0, 0.9, 0.0);
        control.player.vel = Vec3::new(3.0, 0.0, 0.0);
        control.step(proto::InputFrame::default());

        assert_eq!(
            with_peer.peer_contacts, 1,
            "the replicated peer was never solved"
        );
        assert!(
            with_peer.player.vel.x < control.player.vel.x - 0.5,
            "peer did not slow the closing player: {} vs control {}",
            with_peer.player.vel.x,
            control.player.vel.x
        );
        assert_eq!(control.peer_contacts, 0, "an offline tick invented a peer");
    }

    #[test]
    fn a_late_snapshot_cannot_replace_a_newer_collider_set() {
        let mut local = Local::default();
        local.player.pos = Vec3::new(0.0, 0.9, 0.0);
        local.player.vel = Vec3::new(3.0, 0.0, 0.0);
        // The newer datagram first: the peer is touching the player.
        local.accept_snapshot(&snapshot_with_peers(30, &[peer_at(2, 0.5)]), 0);
        // A late datagram from an older tick claiming the peer is far away.
        local.accept_snapshot(&snapshot_with_peers(20, &[peer_at(2, 20.0)]), 0);

        local.step(proto::InputFrame::default());
        assert_eq!(
            local.peer_contacts, 1,
            "a late datagram rewrote the collider set for a newer tick"
        );
    }

    #[test]
    fn the_peer_anchor_is_biased_by_the_measured_one_way_delay() {
        // No sample (or no measurable RTT): the anchor is the snapshot tick.
        let mut unbiased = Local::default();
        unbiased.accept_snapshot(&snapshot_with_peers(100, &[peer_at(2, 0.5)]), 0);
        assert_eq!(
            unbiased.predicted_server_tick(),
            Some(proto::Tick::new(100))
        );

        // With a sample, the client predicts the server's *present* tick, not
        // the tick the snapshot was built on.
        let mut biased = Local::default();
        biased.accept_snapshot(&snapshot_with_peers(100, &[peer_at(2, 0.5)]), 4);
        assert_eq!(
            biased.predicted_server_tick(),
            Some(proto::Tick::new(104)),
            "the clock was anchored one round trip in the past"
        );
        // The collider set itself is still filed under the tick it describes.
        assert_eq!(biased.peers.history().newest(), Some(proto::Tick::new(100)));
    }

    #[test]
    fn a_resync_places_the_player_and_keeps_the_peer_history() {
        let mut local = Local::default();
        local.accept_snapshot(&snapshot_with_peers(30, &[peer_at(2, 0.5)]), 0);
        local.accept_snapshot(
            &proto::ServerSnapshot {
                resync: true,
                shadow_position: [5.0, 0.9, 5.0],
                ..snapshot_with_peers(31, &[peer_at(2, 0.5)])
            },
            0,
        );

        assert_eq!(local.player.pos, Vec3::new(5.0, 0.9, 5.0));
        assert_eq!(local.player.vel, Vec3::ZERO);
        assert_eq!(
            local.peers.history().len(),
            2,
            "a placement is not a reason to forget collider ticks"
        );
    }
}
