//! Lightyear transport for the server.
//!
//! The raw connection ties the connection layer to the UDP link: every remote
//! address becomes a peer, and a peer is connected as soon as its link is up.
//! Everything above (replication, prediction inputs) is handled by lightyear.

use std::net::SocketAddr;

use bevy::prelude::*;
use lightyear::prelude::server::*;
use lightyear::prelude::*;

pub struct NetPlugin {
    pub addr: SocketAddr,
}

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ServerAddr(self.addr))
            .add_systems(Startup, spawn_server);
    }
}

#[derive(Resource)]
struct ServerAddr(SocketAddr);

fn spawn_server(mut commands: Commands, addr: Res<ServerAddr>) {
    // `ServerUdpIo` is an endpoint: it owns the socket and creates one link per
    // remote address. `RawServer` makes the link itself the connection.
    let server = commands
        .spawn((ServerUdpIo, RawServer, LocalAddr(addr.0)))
        .id();
    commands.trigger(Start { entity: server });
    info!("listening on {}", addr.0);
}
