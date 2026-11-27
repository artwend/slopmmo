pub mod protocol;

use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::time::Duration;

use crate::protocol::*;
use bevy::prelude::*;
use lightyear::prelude::PeerId;

pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ProtocolPlugin);
    }
}

pub const FIXED_TIMESTEP_HZ: f64 = 64.0;
pub const SERVER_PORT: u16 = 5888;
/// 0 means that the OS will assign any available port
pub const CLIENT_PORT: u16 = 0;
pub const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SERVER_PORT);
pub const SHARED_SETTINGS: SharedSettings = SharedSettings {
    protocol_id: 0,
    private_key: [0; 32],
};

pub const SEND_INTERVAL: Duration = Duration::from_millis(100);

pub const STEAM_APP_ID: u32 = 480; // Steamworks App ID for Spacewar, used for testing

#[derive(Copy, Clone, Debug)]
pub struct SharedSettings {
    /// An id to identify the protocol version
    pub protocol_id: u64,

    /// a 32-byte array to authenticate via the Netcode.io protocol
    pub private_key: [u8; 32],
}

pub fn initial_player_position(peer_id: PeerId) -> Vec3 {
    let slot = (peer_id.to_bits() % 6) as f32;
    Vec3::new((slot - 2.5) * 90.0, 0.9, 0.0)
}

// Applies movement input to a player position.
pub fn shared_movement_behaviour(mut position: Mut<ReplicablePosition>, input: Vec2) {
    const MOVE_SPEED: f32 = 10.0;
    position.0.y += input.y * MOVE_SPEED;
    position.0.x += input.x * MOVE_SPEED;
}