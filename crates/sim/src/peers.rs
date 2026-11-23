//! The client's half of §4.1: a history-tracked peer-collider set and the
//! local capsule solve against it.
//!
//! The server replicates players and bosses as kinematic colliders
//! ([`proto::PeerCollider`]). The client solves against them next to the
//! terrain, which is what makes prediction agree with the server instead of
//! waiting for a correction.
//!
//! Doing that only stays reproducible (§2.3, §2.5) if the solver reads the
//! collider set that belonged to the tick it is predicting, not "whatever
//! arrived last". A collider that moves under a replay changes the answer, and
//! a hard collider that appears late turns a walk into a blocked path. So the
//! replicated sets are kept in a bounded, tick-keyed history and sampled by
//! tick: no extrapolation, no rewriting of an older sample, and a late or
//! duplicated datagram cannot change what an earlier tick saw.
//!
//! The contact rule itself is [`crate::collision::contact_pair`], shared with the
//! server's broadphase. A second, slightly different "touch" test here would
//! make the two solvers disagree by construction.

use std::collections::VecDeque;

use bevy_math::Vec3;

use crate::collision::{self, Body, Impulse, Mode, Pair};
use proto::{PeerList, Tick};

/// Replicated collider sets kept per client.
///
/// Peers are refreshed at `proto::PEER_UPDATE_HZ` (30 Hz), so 32 samples cover
/// about a second — twice the `REPORT_WINDOW` a correction can reach back
/// through. Older sets are dropped; a target older than the oldest sample
/// holds that sample instead of inventing one.
pub const PEER_HISTORY_DEPTH: usize = 32;

/// Upper bound on a peer velocity inferred from two samples.
///
/// A collider that jumps (a resume, a teleport, a mode change) must not turn
/// its stale-to-new delta into a projectile in the local solve. Twice the
/// sprint speed covers a legitimate change of direction between samples.
pub const PEER_VELOCITY_CAP: f32 = proto::MAX_SPEED * 2.0;

#[derive(Clone, Copy, Debug)]
struct PeerSample {
    tick: Tick,
    peers: PeerList,
}

/// A collider as the local solve reads it: position at the sampled tick, the
/// velocity implied by the two bracketing samples, and the replicated mode.
#[derive(Clone, Copy, Debug)]
pub struct SampledPeer {
    pub player: u64,
    pub pos: Vec3,
    pub vel: Vec3,
    pub mode: Mode,
}

/// Bounded, tick-keyed history of replicated collider sets.
///
/// Inserts must be monotonic: an out-of-order or duplicated snapshot is
/// ignored rather than inserting an older set behind a newer one, because
/// sampling assumes the samples are ordered by tick.
#[derive(Clone, Debug, Default)]
pub struct PeerHistory {
    samples: VecDeque<PeerSample>,
}

impl PeerHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn newest(&self) -> Option<Tick> {
        self.samples.back().map(|s| s.tick)
    }

    pub fn oldest(&self) -> Option<Tick> {
        self.samples.front().map(|s| s.tick)
    }

    /// Records the collider set the server had at `tick`.
    ///
    /// Returns `false` when the sample is not newer than the newest one held:
    /// the snapshot channel is unreliable and may deliver a datagram late, and
    /// an older set must never land behind a newer one.
    pub fn record(&mut self, tick: Tick, peers: &PeerList) -> bool {
        if let Some(newest) = self.samples.back()
            && newest.tick.since(tick) <= 0
        {
            return false;
        }
        self.samples.push_back(PeerSample {
            tick,
            peers: *peers,
        });
        while self.samples.len() > PEER_HISTORY_DEPTH {
            self.samples.pop_front();
        }
        true
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Samples the collider set for `tick`.
    ///
    /// Between two recorded ticks the positions are interpolated and the
    /// velocity is the finite difference across the pair. At or past the newest
    /// sample the newest position is held but its velocity is the last observed
    /// delta; before the oldest sample the oldest pose is held with zero
    /// velocity. Nothing is extrapolated past either end: a peer that has not
    /// been seen at this tick is used where it was last known to be.
    pub fn sample(&self, tick: Tick, out: &mut Vec<SampledPeer>) {
        out.clear();
        let (Some(oldest), Some(newest)) = (self.samples.front(), self.samples.back()) else {
            return;
        };
        if newest.tick.since(tick) >= 0 {
            match self.samples.len() {
                1 => hold(&newest.peers, out),
                _ => {
                    let previous = &self.samples[self.samples.len() - 2];
                    interpolate(previous, newest, newest.tick, out);
                }
            }
            return;
        }
        if oldest.tick.since(tick) <= 0 {
            hold(&oldest.peers, out);
            return;
        }
        for (a, b) in self.samples.iter().zip(self.samples.iter().skip(1)) {
            if a.tick.since(tick) >= 0 && b.tick.since(tick) <= 0 {
                interpolate(a, b, tick, out);
                return;
            }
        }
        hold(&newest.peers, out);
    }
}

fn hold(peers: &PeerList, out: &mut Vec<SampledPeer>) {
    out.extend(peers.iter().map(|peer| SampledPeer {
        player: peer.player,
        pos: Vec3::from_array(peer.position),
        vel: Vec3::ZERO,
        mode: Mode::from_wire(peer.mode),
    }));
}

fn interpolate(a: &PeerSample, b: &PeerSample, tick: Tick, out: &mut Vec<SampledPeer>) {
    let span = a.tick.since(b.tick);
    let elapsed = a.tick.since(tick).clamp(0, span);
    let fraction = if span > 0 {
        elapsed as f32 / span as f32
    } else {
        1.0
    };
    let seconds = span as f32 * proto::SERVER_TICK_DT;

    for pa in a.peers.iter() {
        let pa_pos = Vec3::from_array(pa.position);
        match b.peers.iter().find(|pb| pb.player == pa.player) {
            Some(pb) => {
                let pb_pos = Vec3::from_array(pb.position);
                let mut vel = if seconds > 0.0 && pb_pos.is_finite() && pa_pos.is_finite() {
                    (pb_pos - pa_pos) / seconds
                } else {
                    Vec3::ZERO
                };
                if !vel.is_finite() {
                    vel = Vec3::ZERO;
                } else if vel.length_squared() > PEER_VELOCITY_CAP * PEER_VELOCITY_CAP {
                    vel = vel.normalize_or_zero() * PEER_VELOCITY_CAP;
                }
                out.push(SampledPeer {
                    player: pa.player,
                    pos: pa_pos.lerp(pb_pos, fraction),
                    vel,
                    mode: merge_mode(pa.mode, pb.mode),
                });
            }
            // Present in the older set only: it left the AOI by the newer one.
            // Hold it where it was last known mid-span, but not at the newer
            // sample itself, where the set is authoritative.
            None if fraction < 1.0 => out.push(SampledPeer {
                player: pa.player,
                pos: pa_pos,
                vel: Vec3::ZERO,
                mode: Mode::from_wire(pa.mode),
            }),
            None => {}
        }
    }

    // Present in the newer set only: it entered the AOI. Its first known
    // position is the newer sample's.
    for pb in b.peers.iter() {
        if a.peers.iter().any(|pa| pa.player == pb.player) {
            continue;
        }
        out.push(SampledPeer {
            player: pb.player,
            pos: Vec3::from_array(pb.position),
            vel: Vec3::ZERO,
            mode: Mode::from_wire(pb.mode),
        });
    }
}

/// Mode of a collider whose two samples disagree: either sample claiming
/// `Hard` makes it hard, mirroring `collision::contact_mode`'s promotion.
fn merge_mode(a: u8, b: u8) -> Mode {
    match (Mode::from_wire(a), Mode::from_wire(b)) {
        (Mode::Hard, _) | (_, Mode::Hard) => Mode::Hard,
        (Mode::Soft, _) | (_, Mode::Soft) => Mode::Soft,
        _ => Mode::None,
    }
}

/// The client's local peer solve: the history plus the scratch it reads.
///
/// Owned by the client's fixed tick. Every buffer keeps its capacity, so after
/// the first contact tick the solve does not allocate.
#[derive(Debug, Default)]
pub struct LocalPeerSolve {
    history: PeerHistory,
    sampled: Vec<SampledPeer>,
    bodies: Vec<Body>,
    pairs: Vec<Pair>,
    impulses: Vec<Impulse>,
    /// Contacts solved on the last call, and how many of them were hard.
    pub contacts: u32,
    pub hard_contacts: u32,
}

impl LocalPeerSolve {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn history(&self) -> &PeerHistory {
        &self.history
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Newest server tick the history can speak for.
    pub fn newest_tick(&self) -> Option<Tick> {
        self.history.newest()
    }

    /// Records the collider set the server had at `tick`. See [`PeerHistory::record`].
    pub fn record(&mut self, tick: Tick, peers: &PeerList) -> bool {
        self.history.record(tick, peers)
    }

    /// Solves the local body against the collider set recorded for `tick`.
    ///
    /// Returns the velocity delta the local body takes, shaped exactly like the
    /// server's: an inelastic relative-normal impulse plus a bounded penetration
    /// bias, capped by `budget` on the vector sum. Peers are modelled as movable
    /// player capsules (`inv_mass = 1`), because that is how the server models
    /// the bodies whose delta the client is predicting.
    ///
    /// The result is a pure function of `(history, tick, local, budget)` and the
    /// pair order is (penetration, index), matching the server's broadphase, so
    /// the same inputs produce the same delta on every call.
    pub fn solve(&mut self, tick: Tick, local: &Body, budget: f32) -> Impulse {
        self.contacts = 0;
        self.hard_contacts = 0;
        self.history.sample(tick, &mut self.sampled);
        if self.sampled.is_empty() {
            return Impulse::default();
        }

        self.bodies.clear();
        self.bodies.push(*local);
        for peer in &self.sampled {
            if peer.mode == Mode::None {
                continue;
            }
            self.bodies.push(Body {
                id: peer.player,
                pos: peer.pos,
                vel: peer.vel,
                radius: proto::PLAYER_RADIUS,
                half_height: proto::PLAYER_HEIGHT * 0.5,
                inv_mass: 1.0,
                mode: peer.mode,
            });
        }
        if self.bodies.len() == 1 {
            return Impulse::default();
        }

        self.pairs.clear();
        for (index, peer) in self.bodies.iter().enumerate().skip(1) {
            if let Some(pair) = collision::contact_pair(&self.bodies[0], peer, 0, index as u32) {
                self.pairs.push(pair);
            }
        }
        // Deepest first, then id order: the same total order the server's
        // broadphase uses for the impulse accumulation and the pair cap.
        self.pairs.sort_by(|x, y| {
            y.penetration
                .total_cmp(&x.penetration)
                .then(x.a.cmp(&y.a))
                .then(x.b.cmp(&y.b))
        });
        self.pairs.truncate(proto::MAX_PAIRS_PER_PLAYER);
        self.contacts = self.pairs.len() as u32;
        self.hard_contacts = self
            .pairs
            .iter()
            .filter(|pair| pair.mode == Mode::Hard)
            .count() as u32;
        if self.pairs.is_empty() {
            return Impulse::default();
        }

        collision::solve_into(&self.bodies, &self.pairs, budget, &mut self.impulses);
        self.impulses[0]
    }
}
