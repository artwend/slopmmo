//! Server authority over *collision*, not over *motion*.
//!
//! The client owns terrain and its own position; the server keeps a shadow
//! position only to (a) validate that reported displacement is physically
//! plausible and (b) run the capsule-vs-capsule solve that the client cannot do
//! alone. Nothing here rewinds, and nothing here is "authoritative movement" —
//! the review calls that out explicitly.
//!
//! A departed player keeps its shadow in a small parked table. Admission cannot
//! be allowed to trust a first report on a new link: deleting the shadow would
//! make "disconnect, reconnect, claim any position" a free teleport. A resume is
//! placed from the parked shadow instead.

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
    /// Multiplier on `MAX_SPEED` and `MAX_ANGULAR_SPEED` for the sanity checks.
    pub speed_slack: f32,
    pub report_window: u32,
    /// Ticks a departed player's shadow stays available for a resume.
    pub resume_window: u32,
    pub epoch: u32,
    /// Build the peer-collider list into snapshots.
    ///
    /// Off by default: the client has no consumer for it yet — it predicts its
    /// own pose and applies corrections, but there is no local peer solve — so
    /// scanning for peers and putting them on the wire is pure cost. Turn this
    /// on with the client-side consumer (ADR 011, ADR 015); the selection logic
    /// and its tests are kept ready for that.
    pub replicate_peers: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            tick_hz: proto::SERVER_TICK_HZ,
            neighbour_limit: proto::MAX_NEIGHBOURS,
            pairs_per_body: proto::MAX_PAIRS_PER_PLAYER,
            correction_budget: proto::CORRECTION_BUDGET,
            speed_slack: 1.35,
            report_window: proto::REPORT_WINDOW,
            resume_window: RESUME_WINDOW,
            epoch: 1,
            replicate_peers: false,
        }
    }
}

/// How long a departed player's shadow stays available to validate a resume.
///
/// Long enough for a crash-restart or a network outage; short enough that the
/// parked poses of players who are gone for good cannot accumulate.
pub const RESUME_WINDOW: u32 = 120 * proto::SERVER_TICK_HZ;

impl SimConfig {
    pub fn dt(&self) -> f32 {
        1.0 / self.tick_hz.max(1) as f32
    }

    /// Maps a client tick onto the server tick domain anchored by `origin`,
    /// a `(client tick, server tick)` pair.
    ///
    /// The two clocks do not share a rate: the client predicts at
    /// [`proto::CLIENT_TICK_HZ`], the server ticks at [`Self::tick_hz`]. Elapsed
    /// client ticks are therefore scaled, not added one-for-one — adding them
    /// directly made the mapped tick run ahead of the server by one tick every
    /// two client ticks, which tripped the future-staleness check and re-anchored
    /// the clock continuously.
    pub fn map_client_tick(
        &self,
        origin: (proto::Tick, proto::Tick),
        client: proto::Tick,
    ) -> proto::Tick {
        let elapsed = i64::from(client.0.wrapping_sub(origin.0.0) as i32);
        let scaled = elapsed * i64::from(self.tick_hz) / i64::from(proto::CLIENT_TICK_HZ);
        // `scaled` is an i32-range delta; the cast keeps negative deltas as
        // two's-complement wrapping sub, which is what `Tick::add` expects.
        origin.1.add(scaled as u32)
    }

    /// Largest displacement a resume may claim from the shadow it left behind.
    ///
    /// A disconnected client cannot legitimately move at all, so the allowance
    /// is the same one the displacement validator gives a report that is
    /// [`Self::report_window`] ticks late — the most the client can have moved
    /// after its last accepted report.
    pub fn resume_travel(&self) -> f32 {
        proto::MAX_SPEED * self.speed_slack * self.report_window as f32 * self.dt()
            + proto::POSITION_SLACK
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
    pub resumed: u32,
    pub resume_clamped: u32,
    pub pairs: u32,
    pub corrections: u32,
    pub hard_contacts: u32,
}

/// Where a resume landed, and whether the claim had to be clamped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resume {
    /// Position the shadow was reinstated at.
    pub pos: Vec3,
    /// Position the client claimed on the new link.
    pub reported: Vec3,
    /// True when the claim exceeded [`SimConfig::resume_travel`].
    pub clamped: bool,
}

/// The shadow of a player that left, kept until a resume or the window expires.
#[derive(Clone, Copy, Debug)]
struct ParkedShadow {
    pos: Vec3,
    vel: Vec3,
    angular_velocity: Vec3,
    body_yaw: f32,
    grounded: bool,
    at: proto::Tick,
}

#[derive(Clone, Debug)]
pub struct ServerPlayer {
    pub id: u64,
    /// Shadow pose for the current tick: the anchor advanced by dead reckoning
    /// when the report stream is older than the tick. Sent as an informational
    /// reference, except for the `resync` placement that follows a clamped resume.
    pub pos: Vec3,
    /// Pose of the last accepted report. Extrapolation is measured from here and
    /// the displacement check must use it: a pose that already moved forward
    /// would shrink the travel the client is accused of.
    pub anchor_pos: Vec3,
    pub vel: Vec3,
    /// Angular velocity from the last accepted report; only the Y (yaw)
    /// component is integrated. The character is kinematic and never falls over.
    pub angular_velocity: Vec3,
    pub body_yaw: f32,
    /// `body_yaw` of the last accepted report. Rotation is extrapolated from
    /// here for the same reason `anchor_pos` exists: re-integrating from an
    /// already-advanced pose would compound it.
    pub anchor_yaw: f32,
    pub grounded: bool,
    pub last_seq: u16,
    pub last_tick: proto::Tick,
    pub correction_seq: u16,
    pub last_correction_tick: Option<proto::Tick>,
    pub budget_used: f32,
    /// Maps the client's tick domain onto the server's. Established on the first
    /// accepted report and re-anchored when the domains drift apart, because the
    /// client's tick counter has nothing to do with the server's absolute tick.
    /// Stored as `(client tick, server tick)` at the anchor: the two clocks run
    /// at different rates, so the mapping needs a rate ratio, not a fixed offset.
    pub tick_origin: Option<(proto::Tick, proto::Tick)>,
    /// Server tick of the last accepted report. The displacement check is
    /// measured on this clock, never on the client-declared report tick.
    pub last_server_tick: proto::Tick,
    /// Set when a resume had to clamp the client's claim. Snapshots carry the
    /// placement while this is set; the first accepted report clears it, so a
    /// lost unreliable snapshot cannot strand the client.
    pub needs_resync: bool,
}

impl ServerPlayer {
    pub fn new(id: u64, pos: Vec3) -> Self {
        Self {
            id,
            pos,
            anchor_pos: pos,
            vel: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            body_yaw: 0.0,
            anchor_yaw: 0.0,
            grounded: false,
            last_seq: 0,
            last_tick: proto::Tick::ZERO,
            correction_seq: 0,
            last_correction_tick: None,
            budget_used: 0.0,
            tick_origin: None,
            last_server_tick: proto::Tick::ZERO,
            needs_resync: false,
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
    /// AOI candidate indices for the snapshot build, reused every tick.
    candidates: Vec<u32>,
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
    /// Shadows of players that left, keyed by id, until a resume or expiry.
    detached: HashMap<u64, ParkedShadow>,
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
            detached: HashMap::new(),
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
        // A fresh admission supersedes any shadow left by an earlier session.
        self.detached.remove(&id);
        let now = self.tick;
        let slot = match self.slots.get(&id) {
            Some(&slot) => slot as usize,
            None => {
                let slot = self.players.len() as u32;
                let mut player = ServerPlayer::new(id, pos);
                // Travel is measured from the moment the body appeared, not
                // from whatever tick the client puts in its first report.
                player.last_server_tick = now;
                self.players.push(player);
                self.slots.insert(id, slot);
                slot as usize
            }
        };
        &mut self.players[slot]
    }

    pub fn remove_player(&mut self, id: u64) -> Option<ServerPlayer> {
        self.detached.remove(&id);
        let slot = self.slots.remove(&id)? as usize;
        let removed = self.players.swap_remove(slot);
        // The tail body now occupies the vacated slot; re-point its id at it.
        if let Some(moved) = self.players.get(slot) {
            self.slots.insert(moved.id, slot as u32);
        }
        Some(removed)
    }

    /// Takes a live player out of the world but keeps the shadow it leaves
    /// behind, so a later resume can be validated against it.
    ///
    /// Deleting the pose instead would make "disconnect and reconnect" a free
    /// teleport: admission has nothing else to compare the first report to.
    pub fn detach_player(&mut self, id: u64) -> bool {
        self.expire_detached();
        let Some(removed) = self.remove_player(id) else {
            return false;
        };
        self.detached.insert(
            id,
            ParkedShadow {
                pos: removed.pos,
                vel: removed.vel,
                angular_velocity: removed.angular_velocity,
                body_yaw: removed.body_yaw,
                grounded: removed.grounded,
                at: self.tick,
            },
        );
        true
    }

    /// Reinstates a detached player from the shadow it left behind.
    ///
    /// The report on the new link is checked, not trusted: a disconnected
    /// client cannot legitimately move, so a claim farther from the parked
    /// shadow than [`SimConfig::resume_travel`] is clamped to that radius.
    /// The admission report also becomes the baseline for the ordinary
    /// displacement check — otherwise a client with a high tick counter would
    /// get an unbounded allowance on its next packet and could undo the clamp
    /// immediately.
    ///
    /// Returns `None` when no unexpired shadow exists for the id; the caller
    /// then treats the packet as a first admission.
    pub fn resume_player(&mut self, packet: &proto::PlayerMovementPacket) -> Option<Resume> {
        if self.contains_player(packet.player) {
            return None;
        }
        self.expire_detached();
        let shadow = self.detached.remove(&packet.player)?;

        let report = &packet.report;
        let reported = Vec3::from_array(report.position);
        let offset = reported - shadow.pos;
        let claim = offset.length();
        let allowed = self.config.resume_travel();
        let clamped = !claim.is_finite() || claim > allowed;
        let pos = if !claim.is_finite() {
            shadow.pos
        } else if clamped {
            shadow.pos + offset / claim * allowed
        } else {
            reported
        };

        let now = self.tick;
        let player = self.add_player(packet.player, pos);
        // The shadow's own state is restored; the claim's velocity is not part
        // of the placement and is not re-validated here.
        player.vel = shadow.vel;
        player.angular_velocity = shadow.angular_velocity;
        player.body_yaw = shadow.body_yaw;
        player.anchor_yaw = shadow.body_yaw;
        player.grounded = shadow.grounded;
        player.last_seq = packet.seq;
        player.last_tick = report.tick;
        player.tick_origin = Some((report.tick, now));
        player.needs_resync = clamped;

        self.stats.resumed += 1;
        if clamped {
            self.stats.resume_clamped += 1;
        }
        Some(Resume {
            pos,
            reported,
            clamped,
        })
    }

    /// Drops parked shadows older than [`SimConfig::resume_window`].
    ///
    /// Called whenever a shadow is parked or consumed, so a server that keeps
    /// seeing departures never accumulates them.
    fn expire_detached(&mut self) {
        let now = self.tick;
        let window = self.config.resume_window.min(i32::MAX as u32) as i32;
        self.detached
            .retain(|_, p| (0..=window).contains(&p.at.since(now)));
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
        //
        // The client predicts at `CLIENT_TICK_HZ` and the server ticks at
        // `tick_hz`, so the mapping scales elapsed client ticks instead of
        // adding them one-for-one.
        let origin = match player.tick_origin {
            Some(origin) => origin,
            None => {
                let origin = (report.tick, *tick);
                player.tick_origin = Some(origin);
                origin
            }
        };
        let mapped = config.map_client_tick(origin, report.tick);
        if tick.is_stale(mapped, config.report_window) {
            // Mapped into the future: the client's clock was paused, hitched or
            // restarted. Re-anchor rather than stalling it forever — the sequence
            // number and the displacement check still bound what it can claim.
            player.tick_origin = Some((report.tick, *tick));
        } else if mapped.is_stale(*tick, config.report_window) {
            stats.stale += 1;
            return Err(Reject::StaleTick);
        }

        let vel = Vec3::from_array(report.linear_velocity);
        let max_v = proto::MAX_SPEED * config.speed_slack * 1.6;
        if !vel.is_finite() || vel.length() > max_v {
            stats.too_fast += 1;
            return Err(Reject::TooFast);
        }

        // The same bound on rotation: a yaw rate beyond what a camera can
        // produce is as implausible as an over-speed linear velocity, and it
        // would otherwise buy unbounded dead-reckoned rotation.
        let angular = Vec3::from_array(report.angular_velocity);
        let max_w = proto::MAX_ANGULAR_SPEED * config.speed_slack * 1.6;
        if !angular.is_finite() || angular.length() > max_w {
            stats.too_fast += 1;
            return Err(Reject::TooFast);
        }

        // Displacement check: the report may not claim more travel than the
        // elapsed ticks could produce. This is the check the dialogue missed —
        // validating velocity alone lets a client teleport slowly. The elapsed
        // time is the *server's*, because the report's own tick is client data:
        // measuring with it would let a forged tick jump (which the clock
        // re-anchor above deliberately accepts) buy an unbounded allowance and
        // undo a resume clamp on the very next packet.
        let pos = Vec3::from_array(report.position);
        if !pos.is_finite() {
            return Err(Reject::Teleport);
        }
        // `Tick::since` is `now - self`, so this is "server ticks since the last
        // accepted report". The arguments were the other way round, which made
        // the subtraction negative and clamped the allowance to a single tick:
        // a legitimate catch-up report after a stalled stream was rejected as a
        // teleport and the shadow could never be re-synced.
        let elapsed = player.last_server_tick.since(*tick).max(1) as f32;
        let allowed =
            proto::MAX_SPEED * config.speed_slack * elapsed * config.dt() + proto::POSITION_SLACK;
        let travelled = (pos - player.anchor_pos).length();
        if travelled > allowed {
            stats.teleport += 1;
            return Err(Reject::Teleport);
        }

        // The anchor is the only pose extrapolation and the displacement check
        // may start from; `pos` will be advanced by the tick.
        player.anchor_pos = pos;
        player.pos = pos;
        player.vel = vel;
        player.angular_velocity = angular;
        player.body_yaw = report.body_yaw;
        player.anchor_yaw = report.body_yaw;
        player.grounded = report.grounded;
        player.last_seq = packet.seq;
        player.last_tick = report.tick;
        player.last_server_tick = *tick;
        // An accepted report proves the session is at the server's placement.
        player.needs_resync = false;

        stats.accepted += 1;
        Ok(())
    }

    /// One authoritative collision tick.
    ///
    /// Snapshots are written into the session's own buffer ([`Self::snapshots`])
    /// rather than returned, so the tick does not allocate. The whole pass is
    /// linear over a dense array: build bodies, rebuild the grid, build pairs,
    /// solve, write snapshots.
    ///
    /// Every collision mode is solved here, on every tick. There is no
    /// sub-sampled solve: the only rate that matters is [`SimConfig::tick_hz`].
    /// Reports arrive at `INPUT_SEND_HZ`, so bodies are dead-reckoned from their
    /// last report before the solve; the solver never reads a pose older than the
    /// tick it is running on.
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
            candidates,
        } = scratch;

        bodies.clear();
        bodies.reserve(players.len() + blockers.len());

        // Phase 2: dead-reckon the shadow between reports. Reports arrive at
        // `INPUT_SEND_HZ` (30 Hz) while the tick solves at 60 Hz, so without this
        // every second tick would solve against a frozen pose — the same
        // "subsystem behind the tick" the tick rate was raised to remove.
        // Deriving the pose from the anchor instead of integrating it keeps the
        // shadow drift-free and identical on replay.
        for p in players.iter_mut() {
            if p.tick_origin.is_none() || p.needs_resync {
                continue;
            }
            let age = p
                .last_server_tick
                .since(*tick)
                .clamp(0, config.report_window as i32);
            p.pos = p.anchor_pos + p.vel * (age as f32 * dt);
            // Rotation is dead-reckoned the same way and from its own anchor,
            // so a turning player is not frozen between reports.
            p.body_yaw = p.anchor_yaw + p.angular_velocity.y * (age as f32 * dt);
        }
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
            snapshots, players, bodies, impulses, grid, candidates, *tick, config, stats,
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
    grid: &SpatialGrid,
    candidates: &mut Vec<u32>,
    tick: proto::Tick,
    config: &SimConfig,
    stats: &mut SimStats,
) {
    snapshots.clear();

    for (i, p) in players.iter_mut().enumerate() {
        let imp = impulses[i];

        let correction = if imp.delta_v != Vec3::ZERO {
            p.correction_seq = p.correction_seq.wrapping_add(1);
            p.last_correction_tick = Some(tick);
            stats.corrections += 1;
            Some(proto::CollisionCorrection {
                seq: p.correction_seq,
                tick,
                epoch: config.epoch,
                velocity_delta: imp.delta_v.to_array(),
                hard_normal: imp.hard_normal.map(|n| n.to_array()),
            })
        } else {
            None
        };

        // Nothing reads peers yet, so the default build skips the AOI scan as
        // well as the wire bytes. See `SimConfig::replicate_peers`.
        let peers = if config.replicate_peers {
            aoi_peers(i, p, bodies, grid, candidates)
        } else {
            proto::PeerList::new()
        };

        snapshots.push((
            p.id,
            proto::ServerSnapshot {
                server_tick: tick,
                acked_seq: p.last_seq,
                shadow_position: p.pos.to_array(),
                correction,
                peers,
                resync: p.needs_resync,
            },
        ));
    }
}

/// Nearest peer colliders within the AOI radius, capped by the inline window.
///
/// The grid narrows the scan to the cells around the player instead of every
/// body; the exact distance test below is unchanged, so the peer set is
/// identical to a full scan. The inline window keeps this allocation free, and
/// its `(distance, index)` order makes the result independent of the order the
/// grid hands candidates back.
fn aoi_peers(
    index: usize,
    p: &ServerPlayer,
    bodies: &[Body],
    grid: &SpatialGrid,
    candidates: &mut Vec<u32>,
) -> proto::PeerList {
    let aoi_radius = proto::NEIGHBOUR_RADIUS * 2.0;
    let mut near: [(f32, usize); proto::MAX_SNAPSHOT_PEERS] =
        [(f32::INFINITY, usize::MAX); proto::MAX_SNAPSHOT_PEERS];
    let mut near_len = 0usize;
    grid.candidates(p.pos, aoi_radius, candidates);
    for &j in candidates.iter() {
        let j = j as usize;
        if j == index {
            continue;
        }
        let b = &bodies[j];
        if b.mode == Mode::None {
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
    peers
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
