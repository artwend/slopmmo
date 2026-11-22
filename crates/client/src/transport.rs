//! Lightyear transport for the client.
//!
//! The client is a raw connection: lightyear provides the UDP link, packet
//! assembly and the message plumbing, and everything here is game policy.

use std::net::SocketAddr;

use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

use crate::Local;

/// Systems that touch the wire, exposed so the tick can run between them:
/// snapshots are drained before the next solve, reports are written after it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetSet {
    Receive,
    Send,
}

#[derive(Resource, Default)]
pub struct NetStats {
    pub sent: u64,
    pub received: u64,
    pub corrections: u64,
    pub resyncs: u64,
}

pub struct ClientNetPlugin {
    pub server: SocketAddr,
    pub player_id: u64,
}

#[derive(Resource)]
struct Server(SocketAddr);

#[derive(Resource)]
struct PlayerId(u64);

impl Plugin for ClientNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ClientPlugins {
            tick_duration: net::TICK_DURATION,
        })
        .add_plugins(net::MovementProtocol)
        .configure_sets(Update, (NetSet::Receive, NetSet::Send).chain())
        .init_resource::<NetStats>()
        .insert_resource(Server(self.server))
        .insert_resource(PlayerId(self.player_id))
        .add_systems(Startup, spawn_client)
        .add_systems(
            Update,
            (
                drain_snapshots.in_set(NetSet::Receive),
                uplink.in_set(NetSet::Send),
            ),
        );
    }
}

fn spawn_client(mut commands: Commands, server: Res<Server>) {
    // Port 0: let the OS pick, the server learns the address from the packet. The bind address
    // must stay unspecified, not loopback, or the socket cannot reach a non-local server.
    let client = commands
        .spawn((
            RawClient,
            UdpIo::default(),
            LocalAddr(SocketAddr::from(([0, 0, 0, 0], 0))),
            PeerAddr(server.0),
        ))
        .id();
    info!("connecting to {}", server.0);
    commands.trigger(Connect { entity: client });
}

fn drain_snapshots(
    mut local: ResMut<Local>,
    mut stats: ResMut<NetStats>,
    mut clients: Query<Option<&mut MessageReceiver<proto::ServerSnapshot>>, With<Client>>,
) {
    for receiver in &mut clients {
        // The typed receiver appears once the first payload of that type arrives.
        let Some(mut receiver) = receiver else {
            continue;
        };
        for snapshot in receiver.receive() {
            stats.received += 1;
            if snapshot.resync {
                stats.resyncs += 1;
                info!(
                    "server placed this session at {:?}",
                    snapshot.shadow_position
                );
            }
            if local.accept_snapshot(&snapshot) {
                stats.corrections += 1;
            }
        }
    }
}

fn uplink(
    mut local: ResMut<Local>,
    mut stats: ResMut<NetStats>,
    player: Res<PlayerId>,
    mut clients: Query<Option<&mut MessageSender<proto::PlayerMovementPacket>>, With<Client>>,
) {
    // `report` owns the send cadence and the redundancy window.
    let Some(packet) = local.report(player.0) else {
        return;
    };
    for sender in &mut clients {
        let Some(mut sender) = sender else {
            continue;
        };
        sender.send::<net::InputChannel>(packet.clone());
        stats.sent += 1;
    }
}
