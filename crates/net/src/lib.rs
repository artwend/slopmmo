//! Wire protocol shared by the game server and the client.
//!
//! Lightyear owns framing, sequencing and the reliability rules; this crate only
//! says *what* travels on the wire. Both binaries add [`MovementProtocol`], so
//! channels and messages are registered in the same order on both ends.
//!
//! That order is load-bearing: lightyear assigns channel and message ids by
//! registration. It only compares the resulting protocol hash across peers when
//! its `replication` feature is enabled, which this build does not turn on, so
//! the hash is *not* checked here. `proto::PROTOCOL_VERSION` in every client
//! packet is the substitute: a mismatch is rejected and logged instead of being
//! silently ignored.

use bevy_app::{App, Plugin};
use core::time::Duration;
use lightyear::prelude::*;
use proto::{CLIENT_TICK_HZ, PlayerMovementPacket, SERVER_TICK_HZ, ServerSnapshot};

/// Duration of one client prediction tick. The client's Lightyear timeline uses it.
pub const CLIENT_TICK_DURATION: Duration =
    Duration::from_nanos(1_000_000_000 / CLIENT_TICK_HZ as u64);

/// Duration of one authoritative server tick. The server's Lightyear timeline
/// uses it.
pub const SERVER_TICK_DURATION: Duration =
    Duration::from_nanos(1_000_000_000 / SERVER_TICK_HZ as u64);

/// Largest message we are willing to put in one datagram.
///
/// Lightyear's `DEFAULT_MTU` is the size of a whole datagram including the
/// 13-byte packet header, and anything larger than the fragment size is split
/// across several packets — which, on an unreliable channel, means one lost
/// fragment loses the message. The budget is therefore the MTU minus the header,
/// and a message at the budget still fits one packet.
pub const HEADER_BYTES: usize = 13;
pub const MESSAGE_BUDGET: usize = lightyear::link::DEFAULT_MTU - HEADER_BYTES;

/// Client → server input and movement reports.
pub struct InputChannel;

/// Server → client authoritative snapshots.
pub struct SnapshotChannel;

/// Registers the game's channels and messages.
pub struct MovementProtocol;

impl Plugin for MovementProtocol {
    fn build(&self, app: &mut App) {
        app.add_channel::<InputChannel>(ChannelSettings {
            // Plain unordered unreliable, not sequenced: a late datagram still
            // carries the redundancy frames that let the ingest path recover the
            // missed ticks. Ordering is handled by the per-packet dedupe
            // (packet.seq), not by dropping the whole datagram at the channel.
            mode: ChannelMode::UnorderedUnreliable,
            // Flush every frame, *not* at the report rate. A non-default
            // `send_frequency` makes lightyear hold messages until its timer
            // wraps, so a report produced just after a boundary waits a whole
            // period before it can leave — up to 33 ms of one-way input latency
            // for a 30 Hz channel. The client's own cadence already gates
            // reports, so the frequency setting bought nothing.
            send_frequency: Duration::default(),
            retry_unsent_messages: false,
            ..Default::default()
        })
        .add_direction(NetworkDirection::ClientToServer);

        app.add_channel::<SnapshotChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            send_frequency: Duration::default(),
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
            version: proto::PROTOCOL_VERSION,
            seq: u16::MAX,
            player: u64::MAX,
            report: MovementReport {
                tick: Tick(u32::MAX),
                position: [120.0, -40.0, 120.0],
                linear_velocity: [-120.0, -120.0, 120.0],
                angular_velocity: [-120.0, -120.0, 120.0],
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
            input.len() <= MESSAGE_BUDGET,
            "input message: {} B, budget {} B",
            input.len(),
            MESSAGE_BUDGET
        );

        let snapshot = postcard::to_allocvec(&worst_case_snapshot()).expect("serialise");
        assert!(
            snapshot.len() <= MESSAGE_BUDGET,
            "snapshot message: {} B, budget {} B",
            snapshot.len(),
            MESSAGE_BUDGET
        );
    }
}
