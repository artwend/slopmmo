//! Lightyear transport for the game server.
//!
//! Lightyear owns the UDP endpoint, the per-peer links and packet sequencing. What
//! stays here is game plumbing: admitting a player to the sim, dropping peers that
//! went silent, and handing each tick's snapshots to the right link.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use lightyear::connection::client::Disconnecting;
use lightyear::prelude::server::*;
use lightyear::prelude::*;

use crate::{Game, GameRes, SimSet};
use sim::authority::Reject;

/// A peer that has been silent this long is dropped, along with its shadow body.
///
/// UDP has no disconnect, so silence is the only signal that a client is gone.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// Bookkeeping for the links lightyear opens on our behalf, keyed by link entity.
#[derive(Resource, Default)]
pub struct Peers {
    player_of: HashMap<Entity, u64>,
    entity_of: HashMap<u64, Entity>,
    last_seen: HashMap<Entity, Instant>,
    pub admitted: u64,
    pub dropped: u64,
    pub packets_in: u64,
    pub snapshots_out: u64,
}

impl Peers {
    /// Unbinds a link from its player and removes the shadow body.
    fn release(&mut self, entity: Entity, game: &mut Game) {
        self.last_seen.remove(&entity);
        let Some(player) = self.player_of.remove(&entity) else {
            return;
        };
        // A peer that reconnected on a new address leaves the newest link in
        // charge of the body; the old link must not take the body with it.
        if self.entity_of.get(&player) == Some(&entity) {
            self.entity_of.remove(&player);
            if game.sim.remove_player(player).is_some() {
                self.dropped += 1;
                info!("dropped player {player}");
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
            tick_duration: net::TICK_DURATION,
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
    info!("listening on {}, tick rate {} Hz", addr.0, proto::TICK_HZ);
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

            // Admission is identity-by-packet: the first report from a link says
            // who it is. A real deployment authenticates the session here and
            // spawns at a zone-defined point instead of trusting the client's
            // position.
            if !game.0.sim.players.contains_key(&packet.player) {
                let pos = Vec3::from_array(packet.report.position);
                game.0.sim.add_player(packet.player, pos);
                peers.admitted += 1;
                info!("admitted player {} on link {entity}", packet.player);
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

            if let Err(reject) = game.0.sim.ingest(&packet)
                && reject != Reject::DuplicateSeq
            {
                info!("rejected packet from player {}: {reject:?}", packet.player);
            }
        }
    }
}

fn evict_silent(mut game: ResMut<GameRes>, mut peers: ResMut<Peers>, mut commands: Commands) {
    let now = Instant::now();
    let expired: Vec<Entity> = peers
        .last_seen
        .iter()
        .filter(|(_, seen)| now.duration_since(**seen) > PEER_TIMEOUT)
        .map(|(entity, _)| *entity)
        .collect();
    for entity in expired {
        peers.release(entity, &mut game.0);
        // `Disconnecting` closes the session and despawns the link next frame.
        commands.entity(entity).try_insert(Disconnecting);
    }
}

/// A link lightyear tore down (or that we evicted) must not leave a body behind.
fn evict_orphans(
    mut game: ResMut<GameRes>,
    mut peers: ResMut<Peers>,
    mut gone: RemovedComponents<ClientOf>,
) {
    for entity in gone.read() {
        peers.release(entity, &mut game.0);
    }
}

/// Sends the most recent completed tick's snapshots, exactly once.
fn dispatch(
    mut game: ResMut<GameRes>,
    mut peers: ResMut<Peers>,
    mut clients: Query<(Entity, Option<&mut MessageSender<proto::ServerSnapshot>>), With<ClientOf>>,
) {
    if !game.0.snapshots_pending {
        return;
    }
    for (id, snapshot) in &game.0.last_snapshots {
        let Some(entity) = peers.entity_of.get(id).copied() else {
            continue;
        };
        let Ok((_, sender)) = clients.get_mut(entity) else {
            continue;
        };
        let Some(mut sender) = sender else {
            continue;
        };
        sender.send::<net::SnapshotChannel>(snapshot.clone());
        peers.snapshots_out += 1;
    }
    game.0.snapshots_pending = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_a_link_removes_the_shadow_body() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::ZERO);
        let mut world = World::new();
        let link = world.spawn_empty().id();

        let mut peers = Peers::default();
        peers.player_of.insert(link, 1);
        peers.entity_of.insert(1, link);
        peers.last_seen.insert(link, Instant::now());
        peers.release(link, &mut game);

        assert!(game.sim.players.is_empty(), "body outlived its link");
        assert_eq!(peers.dropped, 1);
        assert!(peers.player_of.is_empty());
        assert!(peers.entity_of.is_empty());
        assert!(peers.last_seen.is_empty());
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
        assert!(game.sim.players.contains_key(&1), "rebind dropped the body");

        peers.release(new, &mut game);
        assert!(game.sim.players.is_empty());
        assert_eq!(peers.dropped, 1);
    }
}
