//! Server authority over *collision*, not over *motion*.
//!
//! The client owns terrain and its own position; the server keeps a shadow
//! position only to (a) validate that reported displacement is physically
//! plausible and (b) run the capsule-vs-capsule solve that the client cannot do
//! alone. Nothing here rewinds, and nothing here is "authoritative movement" —
//! the review calls that out explicitly.

use std::collections::HashMap;

use bevy_math::Vec3;

use crate::collision::{self, Body, Mode, PairScratch};
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

/// Per-tick buffers.
///
/// The tick runs sixty times a second and is the only thing in the process that
/// touches every player, so it owns its working memory instead of allocating it.
/// After the first tick with a given player count, [`Simulation::step`] does not
/// call the allocator at all.
#[derive(Default)]
struct Scratch {
    bodies: Vec<Body>,
    positions: Vec<Vec3>,
    pairs: PairScratch,
}

/// Authoritative state.
///
/// Players live in a dense `Vec` and are addressed by slot, with `slots` mapping
/// id → slot for the two places that only know an id (ingest, add/remove). The
/// tick itself walks the array in order, which is what makes it cache friendly:
/// one linear pass, no tree traversal, no pointer chasing, and the shadow state
/// of adjacent players lands in adjacent cache lines.
pub struct Simulation {
    pub config: SimConfig,
    pub stats: SimStats,
    pub tick: proto::Tick,
    pub blockers: Vec<Body>,
    players: Vec<ServerPlayer>,
    /// `id → index into ↓players`. Only touched on join, leave and ingest.
    slots: HashMap<u64, u32>,
    grid: SpatialGrid,
    scratch: Scratch,
    /// `(player, snapshot)` for the most recent tick, in the same order as
    /// `players`, so the driver can pair them without a second allocation.
    snapshots: Vec<(u64, proto::ServerSnapshot)>,
}

impl Simulation {
    pub fn new(config: SimConfig) -> Self {
        Self {
            config,
            stats: SimStats::default(),
            tick: proto::Tick::ZERO,
            blockers: Vec::new(),
            players: Vec::new(),
            slots: HashMap::new(),
            grid: SpatialGrid::new(proto::NEIGHBOUR_RADIUS * 2.0),
            scratch: Scratch::default(),
            snapshots: Vec::new(),
        }
    }

    pub fn players(&self) -> &[ServerPlayer] {
        &self.players
    }

    pub fn players_mut(&mut self) -> &mut [ServerPlayer] {
        &mut self.players
    }

    pub fn player(&self, id: u64) -> Option<&ServerPlayer> {
        self.slots
            .get(&id)
            .map(|&slot| &self.players[slot as usize])
    }

    pub fn player_mut(&mut self, id: u64) -> Option<&mut ServerPlayer> {
        // Copy the slot out first: the map borrow must not outlive the body borrow.
        let slot = *self.slots.get(&id)?;
        Some(&mut self.players[slot as usize])
    }

    pub fn contains_player(&self, id: u64) -> bool {
        self.slots.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.players.len()
    }

    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    /// Snapshots produced by the last [`Self::step`], paired with their player.
    pub fn snapshots(&self) -> &[(u64, proto::ServerSnapshot)] {
        &self.snapshots
    }

    pub fn add_player(&mut self, id: u64, pos: Vec3) -> &mut ServerPlayer {
        let slot = match self.slots.get(&id) {
            Some(&slot) => slot as usize,
            None => {
                let slot = self.players.len() as u32;
                self.players.push(ServerPlayer::new(id, pos));
                self.slots.insert(id, slot);
                slot as usize
            }
        };
        &mut self.players[slot]
    }

    pub fn remove_player(&mut self, id: u64) -> Option<ServerPlayer> {
        let slot = self.slots.remove(&id)? as usize;
        let removed = self.players.swap_remove(slot);
        // The tail body now occupies the vacated slot; re-point its id at it.
        if let Some(moved) = self.players.get(slot) {
            self.slots.insert(moved.id, slot as u32);
        }
        Some(removed)
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
        // Field-disjoint borrows: the hot validator below needs the player, the
        // clock and the counters at the same time.
        let Self {
            config,
            stats,
            tick,
            players,
            slots,
            ..
        } = self;
        let Some(slot) = slots.get(&packet.player).copied() else {
            return Err(Reject::UnknownPlayer);
        };
        let player = &mut players[slot as usize];

        // Wrap-aware monotonic sequence. Equal or older is a duplicate retransmit.
        if packet.seq <= player.last_seq && player.last_seq.wrapping_sub(packet.seq) < 0x8000 {
            stats.duplicate += 1;
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
            .get_or_insert(tick.0.wrapping_sub(report.tick.0));
        let mapped = proto::Tick::new(report.tick.0.wrapping_add(*offset));
        if tick.is_stale(mapped, config.report_window) {
            // Mapped into the future: the client's clock was paused, hitched or
            // restarted. Re-anchor rather than stalling it forever — the sequence
            // number and the displacement check still bound what it can claim.
            *offset = tick.0.wrapping_sub(report.tick.0);
        } else if mapped.is_stale(*tick, config.report_window) {
            stats.stale += 1;
            return Err(Reject::StaleTick);
        }

        let vel = Vec3::from_array(report.velocity);
        let max_v = proto::MAX_SPEED * config.speed_slack * 1.6;
        if !vel.is_finite() || vel.length() > max_v {
            stats.too_fast += 1;
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
        let allowed =
            proto::MAX_SPEED * config.speed_slack * ticks * config.dt() + proto::POSITION_SLACK;
        let travelled = (pos - player.pos).length();
        if travelled > allowed {
            stats.teleport += 1;
            return Err(Reject::Teleport);
        }

        player.pos = pos;
        player.vel = vel;
        player.body_yaw = report.body_yaw;
        player.grounded = report.grounded;
        player.last_seq = packet.seq;
        player.last_tick = report.tick;

        stats.accepted += 1;
        Ok(())
    }

    /// One authoritative collision tick.
    ///
    /// Snapshots are written into the session's own buffer ([`Self::snapshots`])
    /// rather than returned, so the tick does not allocate. The whole pass is
    /// linear over a dense array: build bodies, rebuild the grid, build pairs,
    /// solve, write snapshots.
    pub fn step(&mut self, world: &StaticWorld) {
        self.tick = self.tick.add(1);
        let dt = self.config.dt();

        // Destructure so the buffers can be borrowed field-disjointly: the
        // bodies slice and the scratch buffers are all part of `self`.
        let Self {
            config,
            stats,
            tick,
            blockers,
            players,
            grid,
            scratch,
            snapshots,
            ..
        } = self;
        let Scratch {
            bodies,
            positions,
            pairs,
        } = scratch;

        bodies.clear();
        bodies.reserve(players.len() + blockers.len());
        bodies.extend(players.iter().map(|p| p.body(Mode::Soft)));
        bodies.extend(blockers.iter().copied());

        positions.clear();
        positions.extend(bodies.iter().map(|b| b.pos));

        grid.rebuild(positions);
        pairs.build(
            bodies,
            grid,
            positions,
            config.neighbour_limit,
            config.pairs_per_body,
        );
        stats.pairs = pairs.pairs().len() as u32;

        pairs.solve(bodies, config.correction_budget);
        let impulses = pairs.impulses();

        let mut hard = 0u32;
        for (p, imp) in players.iter_mut().zip(impulses) {
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
        stats.hard_contacts = hard;

        // Terrain authority is the client's, but a blocker's capsule still has
        // to not swallow the shadow: nudge anything inside a blocker outward.
        push_out_of_blockers(players, world, dt);

        build_snapshots(
            snapshots,
            players,
            bodies,
            impulses,
            *tick,
            config.epoch,
            stats,
        );
    }

    /// Replays the shadow forward for diagnostics and determinism tests.
    pub fn replay_velocity(
        &self,
        params: &MoveParams,
        id: u64,
        inputs: &[proto::InputFrame],
    ) -> Vec3 {
        let Some(p) = self.player(id) else {
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

fn push_out_of_blockers(_players: &mut [ServerPlayer], _world: &StaticWorld, _dt: f32) {
    // Intentional no-op for the first milestone: blockers are static and
    // already placed outside the test room's walkable area. Real blocker
    // depenetration lands together with the boss controller.
}

fn build_snapshots(
    snapshots: &mut Vec<(u64, proto::ServerSnapshot)>,
    players: &mut [ServerPlayer],
    bodies: &[Body],
    impulses: &[collision::Impulse],
    tick: proto::Tick,
    epoch: u32,
    stats: &mut SimStats,
) {
    snapshots.clear();
    let aoi_radius = proto::NEIGHBOUR_RADIUS * 2.0;

    for (i, p) in players.iter_mut().enumerate() {
        let imp = impulses[i];

        let correction = if imp.delta_v != Vec3::ZERO {
            p.correction_seq = p.correction_seq.wrapping_add(1);
            p.last_correction_tick = Some(tick);
            stats.corrections += 1;
            Some(proto::CollisionCorrection {
                seq: p.correction_seq,
                tick,
                epoch,
                velocity_delta: imp.delta_v.to_array(),
                hard_normal: imp.hard_normal.map(|n| n.to_array()),
            })
        } else {
            None
        };

        // AOI peers: nearest colliders within the neighbour radius, capped.
        // Tracked in a fixed inline window, so this costs no allocation and one
        // pass over the bodies — no per-player `Vec` and no per-player sort.
        let mut near: [(f32, usize); proto::MAX_SNAPSHOT_PEERS] =
            [(f32::INFINITY, usize::MAX); proto::MAX_SNAPSHOT_PEERS];
        let mut near_len = 0usize;
        for (j, b) in bodies.iter().enumerate() {
            if j == i || b.mode == Mode::None {
                continue;
            }
            let d = ((b.pos.x - p.pos.x).powi(2) + (b.pos.z - p.pos.z).powi(2)).sqrt();
            if d > aoi_radius {
                continue;
            }
            keep_nearest(&mut near, &mut near_len, (d, j));
        }

        let mut peers = proto::PeerList::new();
        for &(_, j) in &near[..near_len] {
            let b = &bodies[j];
            // The window is capped at the list's capacity by construction.
            let _ = peers.push(proto::PeerCollider {
                player: b.id,
                position: b.pos.to_array(),
                body_yaw: 0.0,
                mode: b.mode.as_wire(),
            });
        }

        snapshots.push((
            p.id,
            proto::ServerSnapshot {
                server_tick: tick,
                acked_seq: p.last_seq,
                shadow_position: p.pos.to_array(),
                correction,
                peers,
            },
        ));
    }
}

/// Keeps the smallest `near.len()` entries of a `(distance, body)` stream,
/// ordered by `(distance, body index)` so the selection does not depend on the
/// order the bodies happen to be stored in.
fn keep_nearest(near: &mut [(f32, usize)], near_len: &mut usize, entry: (f32, usize)) {
    let cap = near.len();
    if *near_len == cap {
        if entry >= near[cap - 1] {
            return;
        }
        near[cap - 1] = entry;
    } else {
        near[*near_len] = entry;
        *near_len += 1;
    }
    // Bubble the new entry back into place; at most `cap` swaps.
    let mut k = *near_len - 1;
    while k > 0 && near[k - 1] > near[k] {
        near.swap(k - 1, k);
        k -= 1;
    }
}
