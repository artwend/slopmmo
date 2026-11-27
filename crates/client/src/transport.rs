//! Lightyear transport for the client.
//!
//! The client is a raw connection: lightyear owns the UDP link, the connection
//! state and everything above it.

use std::net::SocketAddr;

use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

pub struct ClientNetPlugin {
    pub server: SocketAddr,
}

impl Plugin for ClientNetPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ServerAddr(self.server))
            .add_systems(Startup, spawn_client);
    }
}

#[derive(Resource)]
struct ServerAddr(SocketAddr);

fn spawn_client(mut commands: Commands, addr: Res<ServerAddr>) {
    // Port 0: let the OS pick. The bind address must stay unspecified, not
    // loopback, or the socket cannot reach a non-local server.
    let client = commands
        .spawn((
            RawClient,
            UdpIo::default(),
            ReplicationReceiver,
            LocalAddr(SocketAddr::from(([0, 0, 0, 0], 0))),
            PeerAddr(addr.0),
            Name::from("Client"),
        ))
        .id();
    info!("connecting to {}", addr.0);
    commands.trigger(Connect { entity: client });
}
