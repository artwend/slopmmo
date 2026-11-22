//! Lightyear transport for the game server.
//!
//! Lightyear owns the UDP endpoint, the per-peer links and packet sequencing. What
//! stays here is game plumbing: admitting a player to the sim, retiring peers that
//! went silent (their shadows are parked for a resume), and handing each tick's
//! snapshots to the right link.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use lightyear::connection::client::Disconnecting;
use lightyear::prelude::server::*;
use lightyear::prelude::*;

use crate::{Game, GameRes, SimSet};
use sim::authority::Reject;

/// A peer that has been silent this long is dropped, and its shadow is parked
/// for a resume.
///
/// UDP has no disconnect, so silence is the only signal that a client is gone.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the peer list may change on the wire.
///
/// Peer motion is interpolated on the client, so a peer update does not need the
/// server tick rate. A correction is unaffected: it goes out on the tick that
/// produces it, carrying whatever peer list was last sent.
pub const PEER_UPDATE_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / proto::PEER_UPDATE_HZ as u64);

/// Heartbeat bounds for a snapshot that carries nothing new.
///
/// The snapshot channel is unreliable and sequenced: the datagram carrying the
/// last change can be lost, so the heartbeat bounds how long a client can stay
/// stale. It also refreshes `acked_seq` and `shadow_position`, which are
/// informational and deliberately do not force a send on their own.
pub const SNAPSHOT_HEARTBEAT_MIN: Duration = Duration::from_millis(100);
pub const SNAPSHOT_HEARTBEAT_DEFAULT: Duration = Duration::from_millis(250);
pub const SNAPSHOT_HEARTBEAT_MAX: Duration = Duration::from_secs(1);

/// Two round trips: one for the datagram to arrive and be acted on, one to
/// cover the acknowledgement not existing at all on an unreliable channel.
const HEARTBEAT_RTT_MULTIPLIER: u32 = 2;

/// Heartbeat interval for a link with the given measured RTT.
///
/// A lost final update is repaired after roughly one round trip, so the
/// heartbeat scales with the path instead of paying the same 250 ms on a
/// loopback link and a transatlantic one. `rtt == 0` means the ping manager has
/// not produced a sample yet (or the transport does not measure one), and the
/// fixed default applies.
fn heartbeat_interval(rtt: Duration) -> Duration {
    if rtt.is_zero() {
        return SNAPSHOT_HEARTBEAT_DEFAULT;
    }
    rtt.saturating_mul(HEARTBEAT_RTT_MULTIPLIER)
        .clamp(SNAPSHOT_HEARTBEAT_MIN, SNAPSHOT_HEARTBEAT_MAX)
}

/// What was last put on the wire for a player, and when each part was current.
#[derive(Clone, Copy)]
struct SentSnapshot {
    snapshot: proto::ServerSnapshot,
    /// When this snapshot was sent.
    at: Instant,
    /// When `snapshot.peers` was captured. Older than `at` when a correction
    /// rode out with a peer list that was not due for a refresh.
    peers_at: Instant,
}

/// What the next tick should do for one player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SendPlan {
    send: bool,
    /// Replace the outgoing snapshot's peer list with the current one.
    refresh_peers: bool,
}

/// Bookkeeping for the links lightyear opens on our behalf, keyed by link entity.
#[derive(Resource, Default)]
pub struct Peers {
    player_of: HashMap<Entity, u64>,
    entity_of: HashMap<u64, Entity>,
    last_seen: HashMap<Entity, Instant>,
    /// Reused every frame by `evict_silent`.
    expired: Vec<Entity>,
    /// Last snapshot put on the wire for a player. The next tick's snapshot is
    /// compared against it so an unchanged one is not sent.
    last_sent: HashMap<u64, SentSnapshot>,
    pub admitted: u64,
    pub dropped: u64,
    pub resumed: u64,
    pub resume_clamped: u64,
    pub packets_in: u64,
    pub snapshots_out: u64,
    /// Ticks whose snapshot was identical to the last one sent, so no datagram
    /// was produced.
    pub snapshots_skipped: u64,
}

impl Peers {
    /// Unbinds a link from its player and parks the shadow body for a resume.
    fn release(&mut self, entity: Entity, game: &mut Game) {
        self.last_seen.remove(&entity);
        let Some(player) = self.player_of.remove(&entity) else {
            return;
        };
        // A peer that reconnected on a new address leaves the newest link in
        // charge of the body; the old link must not take the body with it.
        if self.entity_of.get(&player) == Some(&entity) {
            self.entity_of.remove(&player);
            // A resume is a new stream: its first snapshot must not be compared
            // against what the old link happened to have been sent.
            self.last_sent.remove(&player);
            // The pose is kept, not deleted: admission cannot validate a
            // reconnect against anything if the shadow is gone, which made
            // "drop the link, reconnect anywhere" a free teleport.
            if game.sim.detach_player(player) {
                self.dropped += 1;
                info!("player {player} left; shadow kept for a resume");
            }
        }
    }
}

/// The server's networking half: lightyear plugins plus the game's systems.
pub struct NetPlugin {
    pub addr: SocketAddr,
}

#[derive(Resource)]
struct ListenAddr(SocketAddr);

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        // Order matters: plugin group, then the protocol, then the transport
        // entity. Lightyear hashes the protocol, so both ends must agree.
        app.add_plugins(ServerPlugins {
            tick_duration: net::SERVER_TICK_DURATION,
        })
        .add_plugins(net::MovementProtocol)
        .init_resource::<Peers>()
        .insert_resource(ListenAddr(self.addr))
        .add_systems(Startup, spawn_endpoint)
        .add_systems(Update, ingest.in_set(SimSet::Ingest))
        .add_systems(
            Update,
            (evict_silent, evict_orphans, dispatch)
                .chain()
                .in_set(SimSet::Dispatch),
        );
    }
}

fn spawn_endpoint(mut commands: Commands, addr: Res<ListenAddr>) {
    let server = commands
        .spawn((ServerUdpIo, RawServer, LocalAddr(addr.0)))
        .id();
    info!(
        "listening on {}, tick rate {} Hz",
        addr.0,
        proto::SERVER_TICK_HZ
    );
    commands.trigger(Start { entity: server });
}

/// Reads this frame's packets and feeds them to the simulation.
fn ingest(
    mut game: ResMut<GameRes>,
    mut peers: ResMut<Peers>,
    mut clients: Query<
        (
            Entity,
            Option<&mut MessageReceiver<proto::PlayerMovementPacket>>,
        ),
        With<ClientOf>,
    >,
) {
    let now = Instant::now();
    for (entity, receiver) in &mut clients {
        // Receivers appear on the first payload for that message type.
        let Some(mut receiver) = receiver else {
            continue;
        };
        for packet in receiver.receive() {
            peers.packets_in += 1;

            let mut consumed = false;
            if !game.0.sim.contains_player(packet.player) {
                if let Some(resume) = game.0.sim.resume_player(&packet) {
                    // `resume_player` consumed this report: it placed the body
                    // and armed the seq/tick baseline from it. Re-ingesting it
                    // would let the claim undo its own clamp.
                    peers.resumed += 1;
                    if resume.clamped {
                        peers.resume_clamped += 1;
                        info!(
                            "player {} resumed at {:?}, clamped from {:?}",
                            packet.player, resume.pos, resume.reported
                        );
                    } else {
                        info!("player {} resumed at {:?}", packet.player, resume.pos);
                    }
                    consumed = true;
                } else {
                    // First sight of this identity: admission trusts the report.
                    // A real deployment authenticates the session here and
                    // spawns at a zone-defined point instead.
                    let pos = Vec3::from_array(packet.report.position);
                    game.0.sim.add_player(packet.player, pos);
                    peers.admitted += 1;
                    info!("admitted player {} on link {entity}", packet.player);
                }
            } else if peers
                .entity_of
                .get(&packet.player)
                .is_some_and(|e| *e != entity)
            {
                // Same player on a new address: a NAT rebind or a reconnect.
                info!("player {} moved to link {entity}", packet.player);
            }

            peers.player_of.insert(entity, packet.player);
            peers.entity_of.insert(packet.player, entity);
            peers.last_seen.insert(entity, now);

            if !consumed
                && let Err(reject) = game.0.sim.ingest(&packet)
                && reject != Reject::DuplicateSeq
            {
                info!("rejected packet from player {}: {reject:?}", packet.player);
            }
        }
    }
}

fn evict_silent(mut game: ResMut<GameRes>, mut peers: ResMut<Peers>, mut commands: Commands) {
    let now = Instant::now();
    // Same buffer every frame: silence is rare, so this normally collects
    // nothing and allocates nothing.
    let mut expired = std::mem::take(&mut peers.expired);
    expired.extend(
        peers
            .last_seen
            .iter()
            .filter(|(_, seen)| now.duration_since(**seen) > PEER_TIMEOUT)
            .map(|(entity, _)| *entity),
    );
    for &entity in &expired {
        peers.release(entity, &mut game.0);
        // `Disconnecting` closes the session and despawns the link next frame.
        commands.entity(entity).try_insert(Disconnecting);
    }
    expired.clear();
    peers.expired = expired;
}

/// A link lightyear tore down (or that we evicted) must not leave a live body
/// behind; its shadow is parked for a resume.
fn evict_orphans(
    mut game: ResMut<GameRes>,
    mut peers: ResMut<Peers>,
    mut gone: RemovedComponents<ClientOf>,
) {
    for entity in gone.read() {
        peers.release(entity, &mut game.0);
    }
}

/// The per-link components `dispatch` needs: the snapshot sender and the
/// transport link, whose measured RTT drives the heartbeat.
type ClientLinks<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static mut MessageSender<proto::ServerSnapshot>>,
        Option<&'static Link>,
    ),
    With<ClientOf>,
>;

/// Sends the most recent completed tick's snapshots, at most once each.
///
/// A snapshot is put on the wire only when it carries something the client can
/// act on — a correction, a resync, or a peer refresh — or when the heartbeat is
/// due. Peer motion is refreshed at [`PEER_UPDATE_INTERVAL`] instead of the tick
/// rate; a correction still leaves on the tick that produced it, carrying the
/// last peer list so the two cadences stay independent. The server still builds
/// a snapshot for every player every tick
/// ([`sim::authority::Simulation::step`]); this filter is about the network bill.
fn dispatch(mut game: ResMut<GameRes>, mut peers: ResMut<Peers>, mut clients: ClientLinks) {
    if !game.0.snapshots_pending {
        return;
    }
    let now = Instant::now();
    for &(id, snapshot) in game.0.sim.snapshots() {
        let Some(entity) = peers.entity_of.get(&id).copied() else {
            continue;
        };
        let Ok((_, sender, link)) = clients.get_mut(entity) else {
            continue;
        };
        let Some(mut sender) = sender else {
            continue;
        };
        let heartbeat = heartbeat_interval(link.map_or(Duration::ZERO, |l| l.stats.rtt));
        let prev = peers.last_sent.get(&id).copied();
        let plan = plan_snapshot(prev.as_ref(), &snapshot, now, heartbeat);
        if !plan.send {
            peers.snapshots_skipped += 1;
            continue;
        }
        let mut out = snapshot;
        let peers_at = if plan.refresh_peers {
            now
        } else {
            prev.map_or(now, |p| p.peers_at)
        };
        if !plan.refresh_peers
            && let Some(prev) = prev
        {
            out.peers = prev.snapshot.peers;
        }
        // `ServerSnapshot` is `Copy` and its peer list is inline, so this is a
        // flat copy instead of a per-player heap clone.
        sender.send::<net::SnapshotChannel>(out);
        peers.snapshots_out += 1;
        peers.last_sent.insert(
            id,
            SentSnapshot {
                snapshot: out,
                at: now,
                peers_at,
            },
        );
    }
    game.0.snapshots_pending = false;
}

/// Decides whether the next snapshot goes out, and whether it carries a fresh
/// peer list.
///
/// `server_tick`, `acked_seq` and `shadow_position` are deliberately excluded
/// from the comparison: the client predicts its own pose and only adopts the
/// shadow while `resync` is set, so those fields changing on their own do not
/// justify a datagram (the heartbeat refreshes them). Peer state is compared at
/// millimetre precision, the resolution the wire already cares about, so solver
/// noise cannot defeat suppression.
fn plan_snapshot(
    prev: Option<&SentSnapshot>,
    next: &proto::ServerSnapshot,
    now: Instant,
    heartbeat: Duration,
) -> SendPlan {
    let Some(prev) = prev else {
        return SendPlan {
            send: true,
            refresh_peers: true,
        };
    };
    let urgent = prev.snapshot.resync != next.resync
        || correction_changed(&prev.snapshot.correction, &next.correction);
    let peers_changed = peers_differ(&prev.snapshot.peers, &next.peers);
    // `resync` is a placement request the client cannot act on without the
    // current collider set, and it is rare, so it refreshes peers immediately.
    let refresh_peers =
        next.resync || (peers_changed && now.duration_since(prev.peers_at) >= PEER_UPDATE_INTERVAL);
    SendPlan {
        send: urgent || refresh_peers || now.duration_since(prev.at) >= heartbeat,
        refresh_peers,
    }
}

fn correction_changed(
    prev: &Option<proto::CollisionCorrection>,
    next: &Option<proto::CollisionCorrection>,
) -> bool {
    match (prev, next) {
        (None, None) => false,
        (Some(a), Some(b)) => a.seq != b.seq || a.epoch != b.epoch,
        _ => true,
    }
}

fn peers_differ(prev: &proto::PeerList, next: &proto::PeerList) -> bool {
    prev.len() != next.len()
        || prev.iter().zip(next.iter()).any(|(a, b)| {
            a.player != b.player
                || a.mode != b.mode
                || a.body_yaw.to_bits() != b.body_yaw.to_bits()
                || a.position
                    .iter()
                    .zip(b.position.iter())
                    .any(|(x, y)| millimetres(*x) != millimetres(*y))
        })
}

fn millimetres(v: f32) -> i32 {
    (v * proto::QUANT_MM).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reconnect_packet(player: u64, pos: Vec3) -> proto::PlayerMovementPacket {
        proto::PlayerMovementPacket {
            seq: 1,
            player,
            report: proto::MovementReport {
                tick: proto::Tick::ZERO,
                position: pos.to_array(),
                grounded: true,
                ..Default::default()
            },
            inputs: Vec::new(),
        }
    }

    #[test]
    fn releasing_a_link_parks_the_shadow_so_a_resume_cannot_teleport() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::ZERO);
        let mut world = World::new();
        let link = world.spawn_empty().id();

        let mut peers = Peers::default();
        peers.player_of.insert(link, 1);
        peers.entity_of.insert(1, link);
        peers.last_seen.insert(link, Instant::now());
        peers.release(link, &mut game);

        assert!(game.sim.is_empty(), "body outlived its link");
        assert_eq!(peers.dropped, 1);
        assert!(peers.player_of.is_empty());
        assert!(peers.entity_of.is_empty());
        assert!(peers.last_seen.is_empty());

        // A reconnect that claims a position across the map must be placed at
        // the parked shadow plus the plausible travel, not where it claimed.
        let claim = Vec3::new(40.0, 0.9, 0.0);
        let resume = game
            .sim
            .resume_player(&reconnect_packet(1, claim))
            .expect("the shadow was not kept for a resume");
        assert!(resume.clamped);
        assert_eq!(game.sim.stats.resume_clamped, 1);
        assert_eq!(game.sim.player(1).unwrap().pos, resume.pos);

        let allowed = game.sim.config.resume_travel();
        assert!(
            (resume.pos.length() - allowed).abs() < 1e-3,
            "clamped to {} instead of {allowed}",
            resume.pos.length()
        );
        assert!(
            resume.pos.normalize().abs_diff_eq(claim.normalize(), 1e-3),
            "the clamp changed the claimed direction"
        );
    }

    #[test]
    fn a_resume_inside_the_travel_allowance_is_taken_as_reported() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::ZERO);
        let mut world = World::new();
        let link = world.spawn_empty().id();

        let mut peers = Peers::default();
        peers.player_of.insert(link, 1);
        peers.entity_of.insert(1, link);
        peers.release(link, &mut game);

        let reported = Vec3::new(0.0, 0.9, 1.0);
        let resume = game
            .sim
            .resume_player(&reconnect_packet(1, reported))
            .expect("resume lost");
        assert!(!resume.clamped);
        assert_eq!(resume.pos, reported);
        assert_eq!(game.sim.player(1).unwrap().pos, reported);
    }

    #[test]
    fn releasing_a_superseded_link_keeps_the_body() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::ZERO);
        let mut world = World::new();
        let old = world.spawn_empty().id();
        let new = world.spawn_empty().id();

        let mut peers = Peers::default();
        // The player rebound to `new`, but `old` is still timed out first.
        peers.player_of.insert(old, 1);
        peers.player_of.insert(new, 1);
        peers.entity_of.insert(1, new);

        peers.release(old, &mut game);
        assert!(game.sim.contains_player(1), "rebind dropped the body");

        peers.release(new, &mut game);
        assert!(game.sim.is_empty());
        assert_eq!(peers.dropped, 1);
        assert!(
            game.sim
                .resume_player(&reconnect_packet(1, Vec3::ZERO))
                .is_some(),
            "the departing link's shadow was not parked"
        );
    }

    fn snapshot_with(peers: &[(u64, [f32; 3])]) -> proto::ServerSnapshot {
        proto::ServerSnapshot {
            server_tick: proto::Tick::new(9),
            acked_seq: 3,
            shadow_position: [1.0, 0.0, 2.0],
            correction: None,
            resync: false,
            peers: peers
                .iter()
                .map(|&(player, position)| proto::PeerCollider {
                    player,
                    position,
                    body_yaw: 0.0,
                    mode: proto::peer_mode::SOFT,
                })
                .collect(),
        }
    }

    fn sent_at(snapshot: proto::ServerSnapshot, at: Instant, peers_at: Instant) -> SentSnapshot {
        SentSnapshot {
            snapshot,
            at,
            peers_at,
        }
    }

    fn correction(seq: u16) -> proto::CollisionCorrection {
        proto::CollisionCorrection {
            seq,
            tick: proto::Tick::new(9),
            epoch: 0,
            velocity_delta: [0.5, 0.0, 0.0],
            hard_normal: None,
        }
    }

    /// A heartbeat long enough that only the property under test can trigger a send.
    const QUIET_HEARTBEAT: Duration = Duration::from_secs(10);

    #[test]
    fn informational_fields_alone_do_not_justify_a_datagram() {
        let now = Instant::now();
        let base = snapshot_with(&[(2, [0.0, 0.9, 1.0])]);
        let prev = sent_at(base, now, now);
        let next = proto::ServerSnapshot {
            server_tick: proto::Tick::new(10),
            acked_seq: 4,
            shadow_position: [5.0, 0.9, -5.0],
            ..base
        };
        let plan = plan_snapshot(Some(&prev), &next, now, QUIET_HEARTBEAT);
        assert!(!plan.send, "informational fields forced a datagram");
    }

    #[test]
    fn a_correction_leaves_immediately_without_refreshing_peers() {
        let now = Instant::now();
        let base = snapshot_with(&[(2, [0.0, 0.9, 1.0])]);
        let prev = sent_at(base, now, now);
        // Peer moved *and* a correction arrived. Both facts are new, but the
        // peer list is not due, so the correction must not drag it along.
        let mut next = snapshot_with(&[(2, [0.0, 0.9, 1.5])]);
        next.correction = Some(correction(1));

        let plan = plan_snapshot(
            Some(&prev),
            &next,
            now + Duration::from_millis(5),
            QUIET_HEARTBEAT,
        );
        assert!(plan.send);
        assert!(
            !plan.refresh_peers,
            "a correction bypassed the peer cadence"
        );

        // A repeat of the applied correction and an unchanged peer list is not news.
        let delivered = proto::ServerSnapshot {
            correction: Some(correction(1)),
            ..base
        };
        let delivered_prev = sent_at(delivered, now, now);
        let plan = plan_snapshot(
            Some(&delivered_prev),
            &delivered,
            now + Duration::from_millis(5),
            QUIET_HEARTBEAT,
        );
        assert!(!plan.send);
    }

    #[test]
    fn peer_movement_waits_for_the_peer_cadence() {
        let base = Instant::now();
        let prev = sent_at(snapshot_with(&[(2, [0.0, 0.9, 1.0])]), base, base);
        let moved = snapshot_with(&[(2, [0.0, 0.9, 1.5])]);

        let early = plan_snapshot(
            Some(&prev),
            &moved,
            base + Duration::from_millis(10),
            QUIET_HEARTBEAT,
        );
        assert!(!early.send, "a peer update bypassed its cadence");

        let due = plan_snapshot(
            Some(&prev),
            &moved,
            base + PEER_UPDATE_INTERVAL,
            QUIET_HEARTBEAT,
        );
        assert!(due.send);
        assert!(due.refresh_peers);

        // Solver noise below the millimetre the wire cares about is not a change.
        let noise = snapshot_with(&[(2, [0.0, 0.9, 1.0004])]);
        let plan = plan_snapshot(
            Some(&prev),
            &noise,
            base + PEER_UPDATE_INTERVAL,
            QUIET_HEARTBEAT,
        );
        assert!(!plan.send);
    }

    #[test]
    fn peer_membership_changes_still_wait_for_the_cadence() {
        let base = Instant::now();
        let prev = sent_at(snapshot_with(&[(2, [0.0, 0.9, 1.0])]), base, base);
        let joined = snapshot_with(&[(2, [0.0, 0.9, 1.0]), (3, [0.0, 0.9, 2.0])]);

        let early = plan_snapshot(
            Some(&prev),
            &joined,
            base + Duration::from_millis(10),
            QUIET_HEARTBEAT,
        );
        assert!(!early.send);

        let due = plan_snapshot(
            Some(&prev),
            &joined,
            base + PEER_UPDATE_INTERVAL,
            QUIET_HEARTBEAT,
        );
        assert!(due.send && due.refresh_peers);
    }

    #[test]
    fn an_unchanged_snapshot_waits_for_the_heartbeat() {
        let base = Instant::now();
        let snap = snapshot_with(&[]);
        let prev = sent_at(snap, base, base);
        let heartbeat = Duration::from_millis(250);

        let early = plan_snapshot(
            Some(&prev),
            &snap,
            base + Duration::from_millis(100),
            heartbeat,
        );
        assert!(!early.send);

        let due = plan_snapshot(Some(&prev), &snap, base + heartbeat, heartbeat);
        assert!(due.send);
        assert!(!due.refresh_peers, "a heartbeat should not refresh peers");
    }

    #[test]
    fn resync_refreshes_peers_immediately_and_keeps_repeating() {
        let base = Instant::now();
        let prev = sent_at(snapshot_with(&[]), base, base);
        let mut resync = snapshot_with(&[]);
        resync.resync = true;
        resync.peers = snapshot_with(&[(2, [0.0, 0.9, 2.0])]).peers;

        let first = plan_snapshot(Some(&prev), &resync, base, QUIET_HEARTBEAT);
        assert!(first.send && first.refresh_peers);

        // While resync is still set, the unchanged placement keeps repeating so
        // a lost datagram cannot strand the resumed client.
        let repeat = sent_at(resync, base, base);
        let again = plan_snapshot(Some(&repeat), &resync, base, QUIET_HEARTBEAT);
        assert!(again.send);
    }

    #[test]
    fn the_heartbeat_scales_with_the_measured_rtt() {
        assert_eq!(
            heartbeat_interval(Duration::ZERO),
            SNAPSHOT_HEARTBEAT_DEFAULT,
            "no sample yet should keep the fixed default"
        );
        assert_eq!(
            heartbeat_interval(Duration::from_millis(10)),
            SNAPSHOT_HEARTBEAT_MIN
        );
        assert_eq!(
            heartbeat_interval(Duration::from_millis(150)),
            Duration::from_millis(300)
        );
        assert_eq!(
            heartbeat_interval(Duration::from_secs(5)),
            SNAPSHOT_HEARTBEAT_MAX
        );
    }

    #[test]
    fn releasing_a_link_forgets_its_last_snapshot() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::ZERO);
        let mut world = World::new();
        let link = world.spawn_empty().id();

        let mut peers = Peers::default();
        peers.player_of.insert(link, 1);
        peers.entity_of.insert(1, link);
        let now = Instant::now();
        peers
            .last_sent
            .insert(1, sent_at(snapshot_with(&[]), now, now));

        peers.release(link, &mut game);
        assert!(
            peers.last_sent.is_empty(),
            "a resume compared against the old stream"
        );
    }
}
