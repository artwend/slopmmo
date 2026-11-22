//! Decoding of message payloads out of a lightyear packet.
//!
//! Lightyear prefixes every serialized message with its registration net id (a
//! lightyear varint). The movement messages are postcard values; Lightyear 0.31's
//! `Ping`/`Pong` control messages use their own fixed layout on the dedicated
//! ping channel. This tool does not build a protocol registry: it identifies a
//! movement message by decoding it and requiring the whole payload to be
//! consumed, with the packet's direction as the tie-breaker. Any other internal
//! message is reported as unparsed instead of being mislabelled.

use proto::{PlayerMovementPacket, ServerSnapshot};
use serde::de::DeserializeOwned;

use crate::wire;

/// Lightyear 0.31 registers `Ping` and `Pong` before the game protocol, so they
/// own net ids 0 and 1 on channel 0. Everything in this analyzer is pinned to
/// that version by `wire.rs` as well.
const PING_CHANNEL: u16 = 0;
const PING_NET_ID: u16 = 0;
const PONG_NET_ID: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Self::ClientToServer => "C->S",
            Self::ServerToClient => "S->C",
        }
    }
}

#[derive(Debug)]
pub enum Decoded {
    Input {
        net_id: u16,
        packet: PlayerMovementPacket,
    },
    Snapshot {
        net_id: u16,
        packet: Box<ServerSnapshot>,
    },
    Ping {
        net_id: u16,
        id: u16,
    },
    Pong {
        net_id: u16,
        id: u16,
        /// Whole ticks spent between receiving the ping and sending the pong.
        tick_diff: u32,
        overstep: u8,
    },
    /// A registered lightyear message that is not part of the movement protocol
    /// (or a payload that does not decode cleanly).
    Unknown {
        net_id: Option<u16>,
        bytes: usize,
    },
}

/// The packet kinds a display filter can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Input,
    Snapshot,
    Ping,
    Pong,
    /// A lightyear-internal message, a compressed body or a malformed datagram.
    Internal,
    /// A datagram that carries only acknowledgements.
    Ack,
}

impl Kind {
    /// Name shown in the packet log and used in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "INPUT",
            Self::Snapshot => "SNAPSHOT",
            Self::Ping => "PING",
            Self::Pong => "PONG",
            Self::Internal => "INTERNAL",
            Self::Ack => "ACK",
        }
    }
}

/// What a display filter rule matches: a packet kind and its lightyear net id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageKey {
    pub kind: Kind,
    pub net_id: Option<u16>,
}

impl MessageKey {
    pub const fn ack() -> Self {
        Self {
            kind: Kind::Ack,
            net_id: None,
        }
    }

    /// A datagram whose message type is not visible: compressed or malformed.
    pub const fn internal() -> Self {
        Self {
            kind: Kind::Internal,
            net_id: None,
        }
    }
}

impl Decoded {
    pub fn key(&self) -> MessageKey {
        let (kind, net_id) = match self {
            Self::Input { net_id, .. } => (Kind::Input, Some(*net_id)),
            Self::Snapshot { net_id, .. } => (Kind::Snapshot, Some(*net_id)),
            Self::Ping { net_id, .. } => (Kind::Ping, Some(*net_id)),
            Self::Pong { net_id, .. } => (Kind::Pong, Some(*net_id)),
            Self::Unknown { net_id, .. } => (Kind::Internal, *net_id),
        };
        MessageKey { kind, net_id }
    }
}

pub fn decode(channel: u16, direction: Direction, payload: &[u8]) -> Decoded {
    let Some((net_id, body)) = read_net_id(payload) else {
        return Decoded::Unknown {
            net_id: None,
            bytes: payload.len(),
        };
    };

    if let Some(control) = decode_control(channel, net_id, body) {
        return control;
    }

    let input = decode_complete::<PlayerMovementPacket>(body);
    let snapshot = decode_complete::<ServerSnapshot>(body);
    match (input, snapshot, direction) {
        (Some(packet), None, _) => Decoded::Input { net_id, packet },
        (None, Some(packet), _) => Decoded::Snapshot {
            net_id,
            packet: Box::new(packet),
        },
        (Some(packet), Some(_), Direction::ClientToServer) => Decoded::Input { net_id, packet },
        (Some(_), Some(packet), Direction::ServerToClient) => Decoded::Snapshot {
            net_id,
            packet: Box::new(packet),
        },
        (None, None, _) => Decoded::Unknown {
            net_id: Some(net_id),
            bytes: body.len(),
        },
    }
}

fn decode_control(channel: u16, net_id: u16, body: &[u8]) -> Option<Decoded> {
    if channel != PING_CHANNEL {
        return None;
    }
    match (net_id, body.len()) {
        (PING_NET_ID, 2) => Some(Decoded::Ping {
            net_id,
            id: u16::from_be_bytes([body[0], body[1]]),
        }),
        (PONG_NET_ID, 7) => Some(Decoded::Pong {
            net_id,
            id: u16::from_be_bytes([body[0], body[1]]),
            tick_diff: u32::from_be_bytes([body[2], body[3], body[4], body[5]]),
            overstep: body[6],
        }),
        _ => None,
    }
}

fn read_net_id(payload: &[u8]) -> Option<(u16, &[u8])> {
    let (raw, body) = wire::read_varint(payload)?;
    Some((u16::try_from(raw).ok()?, body))
}

fn decode_complete<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    match postcard::take_from_bytes::<T>(bytes) {
        // A registry message can share a prefix with a movement message; only a
        // decode that consumes the whole payload is accepted.
        Ok((value, [])) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{InputFrame, MovementReport, PeerCollider, Tick, peer_mode};

    fn message_bytes(net_id: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if net_id <= 63 {
            out.push(net_id as u8);
        } else {
            out.extend_from_slice(&(net_id | 0x4000).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn sample_input() -> PlayerMovementPacket {
        PlayerMovementPacket {
            seq: 77,
            player: 0xDEAD_BEEF,
            report: MovementReport {
                tick: Tick::new(1234),
                position: [12.345, 1.5, -7.25],
                linear_velocity: [3.5, -0.5, 0.0],
                angular_velocity: [0.0, 1.5, 0.0],
                body_yaw: 2.0,
                grounded: true,
                applied_correction_seq: 9,
            },
            inputs: vec![InputFrame {
                tick: Tick::new(1234),
                move_x: 100,
                move_z: -40,
                buttons: proto::buttons::JUMP | proto::buttons::SPRINT,
                camera_yaw: proto::quantise_yaw(1.25),
            }],
        }
    }

    fn sample_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            server_tick: Tick::new(9),
            acked_seq: 3,
            shadow_position: [1.0, 0.0, 2.0],
            correction: None,
            peers: core::iter::once(PeerCollider {
                player: 1,
                position: [0.1, 0.2, 0.3],
                body_yaw: 0.5,
                mode: peer_mode::SOFT,
            })
            .collect(),
            resync: true,
        }
    }

    #[test]
    fn an_input_packet_decodes_in_both_directions() {
        let payload = message_bytes(2, &postcard::to_allocvec(&sample_input()).unwrap());
        for direction in [Direction::ClientToServer, Direction::ServerToClient] {
            let Decoded::Input { net_id, packet } = decode(1, direction, &payload) else {
                panic!("not decoded as input");
            };
            assert_eq!(net_id, 2);
            assert_eq!(packet.seq, 77);
            assert_eq!(packet.player, 0xDEAD_BEEF);
            assert!(packet.report.grounded);
            assert_eq!(packet.inputs.len(), 1);
            assert!((packet.report.position[0] - 12.345).abs() < 1e-4);
        }
    }

    #[test]
    fn a_snapshot_decodes_in_both_directions() {
        let payload = message_bytes(3, &postcard::to_allocvec(&sample_snapshot()).unwrap());
        for direction in [Direction::ClientToServer, Direction::ServerToClient] {
            let Decoded::Snapshot { net_id, packet } = decode(2, direction, &payload) else {
                panic!("not decoded as snapshot");
            };
            assert_eq!(net_id, 3);
            assert_eq!(packet.server_tick.0, 9);
            assert_eq!(packet.peers.len(), 1);
            assert!(packet.resync);
        }
    }

    #[test]
    fn a_payload_with_trailing_bytes_is_not_a_movement_message() {
        let mut payload = message_bytes(2, &postcard::to_allocvec(&sample_input()).unwrap());
        payload.push(0);
        assert!(matches!(
            decode(1, Direction::ClientToServer, &payload),
            Decoded::Unknown {
                net_id: Some(2),
                ..
            }
        ));
    }

    #[test]
    fn internal_payloads_are_reported_as_unknown() {
        // A plausible lightyear internal message: net id plus opaque bytes.
        let payload = message_bytes(4, &[0xff, 0x00, 0x12, 0x34, 0x56]);
        let Decoded::Unknown { net_id, bytes } = decode(0, Direction::ServerToClient, &payload)
        else {
            panic!("misdecoded an internal message");
        };
        assert_eq!(net_id, Some(4));
        assert_eq!(bytes, 5);
    }

    #[test]
    fn decoded_messages_expose_the_key_a_display_filter_matches() {
        let ping = message_bytes(0, &0x1234u16.to_be_bytes());
        assert_eq!(
            decode(0, Direction::ClientToServer, &ping).key(),
            MessageKey {
                kind: Kind::Ping,
                net_id: Some(0)
            }
        );

        let input = message_bytes(2, &postcard::to_allocvec(&sample_input()).unwrap());
        assert_eq!(
            decode(1, Direction::ClientToServer, &input).key(),
            MessageKey {
                kind: Kind::Input,
                net_id: Some(2)
            }
        );

        let unknown = message_bytes(4, &[0xff, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(
            decode(0, Direction::ServerToClient, &unknown).key(),
            MessageKey {
                kind: Kind::Internal,
                net_id: Some(4)
            }
        );
    }

    #[test]
    fn a_payload_without_a_net_id_is_unknown() {
        let Decoded::Unknown { net_id, bytes } = decode(1, Direction::ClientToServer, &[]) else {
            panic!("not unknown");
        };
        assert_eq!(net_id, None);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn ping_and_pong_use_the_ping_channel_layout() {
        let ping = message_bytes(0, &0x1234u16.to_be_bytes());
        let Decoded::Ping { net_id, id } = decode(0, Direction::ClientToServer, &ping) else {
            panic!("not a ping");
        };
        assert_eq!((net_id, id), (0, 0x1234));

        let mut pong_body = 0x1234u16.to_be_bytes().to_vec();
        pong_body.extend_from_slice(&7u32.to_be_bytes());
        pong_body.push(0x80);
        let pong = message_bytes(1, &pong_body);
        let Decoded::Pong {
            net_id,
            id,
            tick_diff,
            overstep,
        } = decode(0, Direction::ServerToClient, &pong)
        else {
            panic!("not a pong");
        };
        assert_eq!((net_id, id, tick_diff, overstep), (1, 0x1234, 7, 0x80));

        // The same bytes on a game channel are not control messages.
        assert!(matches!(
            decode(1, Direction::ClientToServer, &ping),
            Decoded::Unknown { .. }
        ));
    }
}
