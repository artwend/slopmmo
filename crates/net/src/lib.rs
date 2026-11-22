//! Wire protocol shared by the game server and the client.
//!
//! Lightyear owns framing, sequencing and the reliability rules; this crate only
//! says *what* travels on the wire. Both binaries add [`MovementProtocol`], so
//! channels and messages are registered in the same order on both ends —
//! lightyear hashes that order and refuses a session whose hashes disagree.

use bevy_app::{App, Plugin};
use core::time::Duration;
use lightyear::prelude::*;
use proto::{INPUT_SEND_HZ, PlayerMovementPacket, ServerSnapshot, TICK_HZ};

/// Duration of one simulation tick. Every Lightyear timeline uses it.
pub const TICK_DURATION: Duration = Duration::from_nanos(1_000_000_000 / TICK_HZ as u64);

/// Largest packet we are willing to put in one UDP datagram.
///
/// Well under the 1472-byte UDP payload limit, so a snapshot never fragments.
pub const DATAGRAM_BUDGET: usize = 1200;

/// Client → server input and movement reports.
pub struct InputChannel;

/// Server → client authoritative snapshots.
pub struct SnapshotChannel;

/// Registers the game's channels and messages.
pub struct MovementProtocol;

impl Plugin for MovementProtocol {
    fn build(&self, app: &mut App) {
        app.add_channel::<InputChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            // Inputs are pointless once superseded, so the sender only flushes
            // them at the rate the client produces them.
            send_frequency: Duration::from_nanos(1_000_000_000 / INPUT_SEND_HZ as u64),
            priority: 2.0,
            retry_unsent_messages: false,
            ..Default::default()
        })
        .add_direction(NetworkDirection::ClientToServer);

        app.add_channel::<SnapshotChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            send_frequency: Duration::default(),
            priority: 1.0,
            retry_unsent_messages: false,
            ..Default::default()
        })
        .add_direction(NetworkDirection::ServerToClient);

        app.register_message::<PlayerMovementPacket>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<ServerSnapshot>()
            .add_direction(NetworkDirection::ServerToClient);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{
        CollisionCorrection, InputFrame, MAX_INPUT_FRAMES, MAX_SNAPSHOT_PEERS, MovementReport,
        PeerCollider, Tick,
    };

    /// Values a client can actually produce during a fight: four redundant input
    /// frames at full deflection, longest varints in every field.
    fn worst_case_client_packet() -> PlayerMovementPacket {
        PlayerMovementPacket {
            seq: u16::MAX,
            player: u64::MAX,
            report: MovementReport {
                tick: Tick(u32::MAX),
                position: [120.0, -40.0, 120.0],
                velocity: [-120.0, -120.0, 120.0],
                body_yaw: core::f32::consts::PI,
                grounded: false,
                applied_correction_seq: u16::MAX,
            },
            inputs: (0..MAX_INPUT_FRAMES)
                .map(|_| InputFrame {
                    tick: Tick(u32::MAX),
                    move_x: i8::MIN,
                    move_z: i8::MAX,
                    buttons: u8::MAX,
                    camera_yaw: u16::MAX,
                })
                .collect(),
        }
    }

    fn worst_case_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            server_tick: Tick(u32::MAX),
            acked_seq: u16::MAX,
            shadow_position: [120.0, -40.0, 120.0],
            correction: Some(CollisionCorrection {
                seq: u16::MAX,
                tick: Tick(u32::MAX),
                epoch: u32::MAX,
                velocity_delta: [-120.0, -120.0, 120.0],
                hard_normal: Some([-1.0, 0.0, 1.0]),
            }),
            resync: true,
            peers: (0..MAX_SNAPSHOT_PEERS)
                .map(|i| PeerCollider {
                    player: u64::MAX - i as u64,
                    position: [120.0, -40.0, 120.0],
                    body_yaw: core::f32::consts::PI,
                    mode: proto::peer_mode::HARD,
                })
                .collect(),
        }
    }

    #[test]
    fn postcard_roundtrips_the_packets() {
        let packet = worst_case_client_packet();
        let bytes = postcard::to_allocvec(&packet).expect("serialise");
        assert_eq!(
            postcard::from_bytes::<PlayerMovementPacket>(&bytes).expect("deserialise"),
            packet
        );

        let snapshot = worst_case_snapshot();
        let bytes = postcard::to_allocvec(&snapshot).expect("serialise");
        assert_eq!(
            postcard::from_bytes::<ServerSnapshot>(&bytes).expect("deserialise"),
            snapshot
        );
    }

    #[test]
    fn worst_case_packets_fit_one_datagram() {
        let input = postcard::to_allocvec(&worst_case_client_packet()).expect("serialise");
        assert!(
            input.len() <= DATAGRAM_BUDGET,
            "input packet: {} B",
            input.len()
        );

        let snapshot = postcard::to_allocvec(&worst_case_snapshot()).expect("serialise");
        assert!(
            snapshot.len() <= DATAGRAM_BUDGET,
            "snapshot packet: {} B",
            snapshot.len()
        );
    }
}
