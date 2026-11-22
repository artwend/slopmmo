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

/// A snapshot that carries nothing new is still re-sent this often.
///
/// The snapshot channel is unreliable and sequenced: the datagram carrying the
/// last change can be lost, so the heartbeat bounds how long a client can stay
/// stale. It also refreshes `acked_seq` and `shadow_position`, which are
/// informational and deliberately do not force a send on their own.
pub const SNAPSHOT_HEARTBEAT: Duration = Duration::from_millis(250);

/// Bookkeeping for the links lightyear opens on our behalf, keyed by link entity.
#[derive(Resource, Default)]
pub struct Peers {
    player_of: HashMap<Entity, u64>,
    entity_of: HashMap<u64, Entity>,
    last_seen: HashMap<Entity, Instant>,
    /// Reused every frame by `evict_silent`.
    expired: Vec<Entity>,
    /// Last snapshot put on the wire for a player, and when. The next tick's
    /// snapshot is compared against it so an unchanged one is not sent.
    last_sent: HashMap<u64, (proto::ServerSnapshot, Instant)>,
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

/// Sends the most recent completed tick's snapshots, at most once each.
///
/// A snapshot is put on the wire only when it carries something the client can
/// act on — a correction, a resync, or a changed peer set — or when the
/// heartbeat is due. The server still builds a snapshot for every player every
/// tick ([`sim::authority::Simulation::step`]); this filter is about the network
/// bill, not about skipping work that other readers may want.
fn dispatch(
    mut game: ResMut<GameRes>,
    mut peers: ResMut<Peers>,
    mut clients: Query<(Entity, Option<&mut MessageSender<proto::ServerSnapshot>>), With<ClientOf>>,
) {
    if !game.0.snapshots_pending {
        return;
    }
    let now = Instant::now();
    for &(id, snapshot) in game.0.sim.snapshots() {
        let Some(entity) = peers.entity_of.get(&id).copied() else {
            continue;
        };
        let Ok((_, sender)) = clients.get_mut(entity) else {
            continue;
        };
        let Some(mut sender) = sender else {
            continue;
        };
        let resend = peers.last_sent.get(&id).is_none_or(|(prev, at)| {
            // `resync` stays set until a report is accepted, so it must keep
            // repeating: the one datagram that would place a resumed client can
            // be the one that is lost.
            snapshot.resync
                || snapshot_changed(prev, &snapshot)
                || now.duration_since(*at) >= SNAPSHOT_HEARTBEAT
        });
        if !resend {
            peers.snapshots_skipped += 1;
            continue;
        }
        // `ServerSnapshot` is `Copy` and its peer list is inline, so this is a
        // flat copy instead of a per-player heap clone.
        sender.send::<net::SnapshotChannel>(snapshot);
        peers.snapshots_out += 1;
        peers.last_sent.insert(id, (snapshot, now));
    }
    game.0.snapshots_pending = false;
}

/// Whether `next` tells the client something `prev` did not.
///
/// `server_tick`, `acked_seq` and `shadow_position` are deliberately excluded:
/// the client predicts its own pose and only adopts the shadow while `resync`
/// is set, so those fields changing on their own do not justify a datagram (the
/// heartbeat refreshes them). Peer state is compared at millimetre precision,
/// the resolution the wire already cares about, so solver noise cannot defeat
/// suppression.
fn snapshot_changed(prev: &proto::ServerSnapshot, next: &proto::ServerSnapshot) -> bool {
    if prev.resync != next.resync {
        return true;
    }
    match (&prev.correction, &next.correction) {
        (None, None) => {}
        (Some(a), Some(b)) if a.seq == b.seq && a.epoch == b.epoch => {}
        _ => return true,
    }
    prev.peers.len() != next.peers.len()
        || prev.peers.iter().zip(next.peers.iter()).any(|(a, b)| {
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

    #[test]
    fn informational_fields_alone_do_not_justify_a_datagram() {
        let prev = snapshot_with(&[(2, [0.0, 0.9, 1.0])]);
        let next = proto::ServerSnapshot {
            server_tick: proto::Tick::new(10),
            acked_seq: 4,
            shadow_position: [5.0, 0.9, -5.0],
            ..prev
        };
        assert!(!snapshot_changed(&prev, &next));
    }

    #[test]
    fn actionable_changes_justify_a_datagram() {
        let prev = snapshot_with(&[(2, [0.0, 0.9, 1.0])]);

        let moved = snapshot_with(&[(2, [0.0, 0.9, 1.001])]);
        assert!(snapshot_changed(&prev, &moved));

        // Solver noise below the millimetre the wire cares about is not a change.
        let noise = snapshot_with(&[(2, [0.0, 0.9, 1.0004])]);
        assert!(!snapshot_changed(&prev, &noise));

        let joined = snapshot_with(&[(2, [0.0, 0.9, 1.0]), (3, [0.0, 0.9, 2.0])]);
        assert!(snapshot_changed(&prev, &joined));

        let corrected = proto::ServerSnapshot {
            correction: Some(proto::CollisionCorrection {
                seq: 1,
                tick: proto::Tick::new(9),
                epoch: 0,
                velocity_delta: [0.5, 0.0, 0.0],
                hard_normal: None,
            }),
            ..prev
        };
        assert!(snapshot_changed(&prev, &corrected));
        assert!(!snapshot_changed(&corrected, &corrected));

        let resync = proto::ServerSnapshot {
            resync: true,
            ..prev
        };
        assert!(snapshot_changed(&prev, &resync));
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
        peers
            .last_sent
            .insert(1, (snapshot_with(&[]), Instant::now()));

        peers.release(link, &mut game);
        assert!(
            peers.last_sent.is_empty(),
            "a resume compared against the old stream"
        );
    }
}
