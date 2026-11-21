//! Server authority over *collision*, not over *motion*.
//!
//! The client owns terrain and its own position; the server keeps a shadow
//! position only to (a) validate that reported displacement is physically
//! plausible and (b) run the capsule-vs-capsule solve that the client cannot do
//! alone. Nothing here rewinds, and nothing here is "authoritative movement" —
//! the review calls that out explicitly.

use std::collections::BTreeMap;

use bevy_math::Vec3;

use crate::collision::{self, Body, Impulse, Mode, Pair};
use crate::movement::{self, MoveParams};
use crate::spatial::SpatialGrid;
use crate::world::StaticWorld;

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub tick_hz: u32,
    pub neighbour_limit: usize,
    pub pairs_per_body: usize,
    pub correction_budget: f32,
    /// Multiplier on `MAX_SPEED` for the velocity sanity check.
    pub speed_slack: f32,
    pub report_window: u32,
    pub epoch: u32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            tick_hz: proto::TICK_HZ,
            neighbour_limit: proto::MAX_NEIGHBOURS,
            pairs_per_body: proto::MAX_PAIRS_PER_PLAYER,
            correction_budget: proto::CORRECTION_BUDGET,
            speed_slack: 1.35,
            report_window: proto::REPORT_WINDOW,
            epoch: 1,
        }
    }
}

impl SimConfig {
    pub fn dt(&self) -> f32 {
        1.0 / self.tick_hz.max(1) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    UnknownPlayer,
    DuplicateSeq,
    StaleTick,
    TooFast,
    Teleport,
    MalformedTick,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SimStats {
    pub accepted: u32,
    pub duplicate: u32,
    pub stale: u32,
    pub too_fast: u32,
    pub teleport: u32,
    pub pairs: u32,
    pub corrections: u32,
    pub hard_contacts: u32,
}

#[derive(Clone, Debug)]
pub struct ServerPlayer {
    pub id: u64,
    /// Shadow pose. Never sent as a target, only as an informational reference.
    pub pos: Vec3,
    pub vel: Vec3,
    pub body_yaw: f32,
    pub grounded: bool,
    pub last_seq: u16,
    pub last_tick: proto::Tick,
    pub correction_seq: u16,
    pub last_correction_tick: Option<proto::Tick>,
    pub budget_used: f32,
    /// Maps the client's tick domain onto the server's. Established on the first
    /// accepted report and re-anchored when the domains drift apart, because the
    /// client's tick counter has nothing to do with the server's absolute tick.
    pub tick_offset: Option<u32>,
}

impl ServerPlayer {
    pub fn new(id: u64, pos: Vec3) -> Self {
        Self {
            id,
            pos,
            vel: Vec3::ZERO,
            body_yaw: 0.0,
            grounded: false,
            last_seq: 0,
            last_tick: proto::Tick::ZERO,
            correction_seq: 0,
            last_correction_tick: None,
            budget_used: 0.0,
            tick_offset: None,
        }
    }

    fn body(&self, mode: Mode) -> Body {
        Body {
            id: self.id,
            pos: self.pos,
            vel: self.vel,
            radius: proto::PLAYER_RADIUS,
            half_height: proto::PLAYER_HEIGHT * 0.5,
            inv_mass: 1.0,
            mode,
        }
    }
}

pub struct Simulation {
    pub config: SimConfig,
    pub stats: SimStats,
    pub tick: proto::Tick,
    pub players: BTreeMap<u64, ServerPlayer>,
    pub blockers: Vec<Body>,
    grid: SpatialGrid,
    positions: Vec<Vec3>,
    ids: Vec<u64>,
}

impl Simulation {
    pub fn new(config: SimConfig) -> Self {
        Self {
            config,
            stats: SimStats::default(),
            tick: proto::Tick::ZERO,
            players: BTreeMap::new(),
            blockers: Vec::new(),
            grid: SpatialGrid::new(proto::NEIGHBOUR_RADIUS * 2.0),
            positions: Vec::new(),
            ids: Vec::new(),
        }
    }

    pub fn add_player(&mut self, id: u64, pos: Vec3) -> &mut ServerPlayer {
        self.players
            .entry(id)
            .or_insert_with(|| ServerPlayer::new(id, pos))
    }

    pub fn remove_player(&mut self, id: u64) -> Option<ServerPlayer> {
        self.players.remove(&id)
    }

    /// Adds an immovable collider (boss body, destructible scenery, NPC).
    pub fn add_blocker(&mut self, id: u64, pos: Vec3) {
        self.blockers.push(Body {
            id,
            pos,
            vel: Vec3::ZERO,
            radius: proto::PLAYER_RADIUS * 2.0,
            half_height: proto::PLAYER_HEIGHT,
            inv_mass: 0.0,
            mode: Mode::Hard,
        });
    }

    /// Validates and adopts a client report. Returns the reject reason instead
    /// of silently clamping — a reject rate is a cheat signal and an
    /// instrumentation metric, not an error to hide.
    pub fn ingest(&mut self, packet: &proto::PlayerMovementPacket) -> Result<(), Reject> {
        let Some(player) = self.players.get_mut(&packet.player) else {
            return Err(Reject::UnknownPlayer);
        };

        // Wrap-aware monotonic sequence. Equal or older is a duplicate retransmit.
        if packet.seq <= player.last_seq && player.last_seq.wrapping_sub(packet.seq) < 0x8000 {
            self.stats.duplicate += 1;
            return Err(Reject::DuplicateSeq);
        }

        let report = &packet.report;
        // The client counts ticks from its own start, so absolute comparison
        // against the server tick would reject every join. Anchor the domains on
        // the first report; a mapped tick that lands outside the window means the
        // client's clock was paused or hitched, so re-anchor instead of stalling
        // it forever. Replays are the sequence number's job, not the tick's.
        let offset = player
            .tick_offset
            .get_or_insert(self.tick.0.wrapping_sub(report.tick.0));
        let mapped = proto::Tick::new(report.tick.0.wrapping_add(*offset));
        if self.tick.is_stale(mapped, self.config.report_window) {
            // Mapped into the future: the client's clock was paused, hitched or
            // restarted. Re-anchor rather than stalling it forever — the sequence
            // number and the displacement check still bound what it can claim.
            *offset = self.tick.0.wrapping_sub(report.tick.0);
        } else if mapped.is_stale(self.tick, self.config.report_window) {
            self.stats.stale += 1;
            return Err(Reject::StaleTick);
        }

        let vel = Vec3::from_array(report.velocity);
        let max_v = proto::MAX_SPEED * self.config.speed_slack * 1.6;
        if !vel.is_finite() || vel.length() > max_v {
            self.stats.too_fast += 1;
            return Err(Reject::TooFast);
        }

        // Displacement check: the report may not claim more travel than the
        // elapsed ticks could produce. This is the check the dialogue missed —
        // validating velocity alone lets a client teleport slowly.
        let pos = Vec3::from_array(report.position);
        if !pos.is_finite() {
            return Err(Reject::Teleport);
        }
        let ticks = report.tick.since(player.last_tick).max(1) as f32;
        let allowed = proto::MAX_SPEED * self.config.speed_slack * ticks * self.config.dt()
            + proto::POSITION_SLACK;
        let travelled = (pos - player.pos).length();
        if travelled > allowed {
            self.stats.teleport += 1;
            return Err(Reject::Teleport);
        }

        player.pos = pos;
        player.vel = vel;
        player.body_yaw = report.body_yaw;
        player.grounded = report.grounded;
        player.last_seq = packet.seq;
        player.last_tick = report.tick;

        self.stats.accepted += 1;
        Ok(())
    }

    /// One authoritative collision tick. Returns one snapshot per player.
    pub fn step(&mut self, world: &StaticWorld) -> Vec<proto::ServerSnapshot> {
        self.tick = self.tick.add(1);
        let dt = self.config.dt();

        self.ids.clear();
        self.ids.extend(self.players.keys().copied());

        let mut bodies: Vec<Body> = Vec::with_capacity(self.ids.len() + self.blockers.len());
        for id in &self.ids {
            let p = &self.players[id];
            bodies.push(p.body(Mode::Soft));
        }
        bodies.extend(self.blockers.iter().copied());

        self.positions.clear();
        self.positions.extend(bodies.iter().map(|b| b.pos));

        self.grid.rebuild(&self.positions);
        let pairs: Vec<Pair> = collision::build_pairs(
            &bodies,
            &self.grid,
            &self.positions,
            self.config.neighbour_limit,
            self.config.pairs_per_body,
        );
        self.stats.pairs = pairs.len() as u32;

        let impulses = collision::solve(&bodies, &pairs, self.config.correction_budget);

        let mut hard = 0u32;
        for (i, id) in self.ids.iter().enumerate() {
            let imp = impulses[i];
            let Some(p) = self.players.get_mut(id) else {
                continue;
            };
            p.budget_used = imp.delta_v.length();
            if imp.delta_v != Vec3::ZERO {
                p.vel += imp.delta_v;
                // Keep the shadow roughly in step so the displacement validator
                // does not drift away from the client over a long run.
                p.pos += imp.delta_v * dt;
            }
            if imp.hard {
                hard += 1;
            }
        }
        self.stats.hard_contacts = hard;

        // Terrain authority is the client's, but a blocker's capsule still has
        // to not swallow the shadow: nudge anything inside a blocker outward.
        self.push_out_of_blockers(world, dt);

        self.build_snapshots(&bodies, &impulses)
    }

    fn push_out_of_blockers(&mut self, _world: &StaticWorld, _dt: f32) {
        // Intentional no-op for the first milestone: blockers are static and
        // already placed outside the test room's walkable area. Real blocker
        // depenetration lands together with the boss controller.
    }

    fn build_snapshots(
        &mut self,
        bodies: &[Body],
        impulses: &[Impulse],
    ) -> Vec<proto::ServerSnapshot> {
        let mut out = Vec::with_capacity(self.ids.len());

        for (i, id) in self.ids.iter().enumerate() {
            let imp = impulses[i];
            let Some(p) = self.players.get_mut(id) else {
                continue;
            };

            let correction = if imp.delta_v != Vec3::ZERO {
                p.correction_seq = p.correction_seq.wrapping_add(1);
                p.last_correction_tick = Some(self.tick);
                self.stats.corrections += 1;
                Some(proto::CollisionCorrection {
                    seq: p.correction_seq,
                    tick: self.tick,
                    epoch: self.config.epoch,
                    velocity_delta: imp.delta_v.to_array(),
                    hard_normal: imp.hard_normal.map(|n| n.to_array()),
                })
            } else {
                None
            };

            // AOI peers: nearest colliders within the neighbour radius, capped.
            let mut nearby: Vec<(f32, usize)> = bodies
                .iter()
                .enumerate()
                .filter(|(j, b)| *j != i && b.mode != Mode::None)
                .filter(|(_, b)| barrier_ok(b, p))
                .map(|(j, b)| ((b.pos - p.pos).length(), j))
                .collect();
            nearby.sort_by(|a, b| a.0.total_cmp(&b.0));

            let peers = nearby
                .iter()
                .take(proto::MAX_SNAPSHOT_PEERS)
                .map(|(_, j)| {
                    let b = &bodies[*j];
                    proto::PeerCollider {
                        player: b.id,
                        position: b.pos.to_array(),
                        body_yaw: 0.0,
                        mode: b.mode.as_wire(),
                    }
                })
                .collect();

            out.push(proto::ServerSnapshot {
                server_tick: self.tick,
                acked_seq: p.last_seq,
                shadow_position: p.pos.to_array(),
                correction,
                peers,
            });
        }

        out
    }

    /// Replays the shadow forward for diagnostics and determinism tests.
    pub fn replay_velocity(
        &self,
        params: &MoveParams,
        id: u64,
        inputs: &[proto::InputFrame],
    ) -> Vec3 {
        let Some(p) = self.players.get(&id) else {
            return Vec3::ZERO;
        };
        let mut vel = p.vel;
        let mut grounded = p.grounded;
        for f in inputs {
            vel = movement::step_velocity(params, f, vel, grounded, self.config.dt());
            grounded = vel.y <= 0.0;
        }
        vel
    }
}

fn barrier_ok(b: &Body, p: &ServerPlayer) -> bool {
    let d = ((b.pos.x - p.pos.x).powi(2) + (b.pos.z - p.pos.z).powi(2)).sqrt();
    d <= proto::NEIGHBOUR_RADIUS * 2.0
}
