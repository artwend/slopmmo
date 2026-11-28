//! Decoding of message payloads out of a lightyear packet.
//!
//! Lightyear prefixes every registered message with its net id (a lightyear
//! varint) and follows it with the message body, serialized with postcard. Both
//! the message ids and the channel ids are handed out in registration order, so
//! they depend on the plugin set: the constants below are the ids this
//! repository's client and server agree on, and the tests decode real captures
//! so a protocol change shows up as a failure instead of silent nonsense.
//!
//! The three replicon channels do not carry registered messages at all: their
//! payloads are `bevy_replicon`'s own replication framing, so they are reported
//! by channel instead of being misread as a message id.

use lightyear::input::bei::prelude::BEIStateSequence;
use lightyear::input::input_message::InputMessage;
use serde::de::DeserializeOwned;

use crate::wire;

// `ChannelRegistry` ids, in registration order.
pub const PING_CHANNEL: u16 = 0;
pub const METADATA_CHANNEL: u16 = 1;
pub const UPDATES_CHANNEL: u16 = 2;
pub const MUTATIONS_CHANNEL: u16 = 3;
pub const MUTATION_ACKS_CHANNEL: u16 = 4;
pub const INPUT_CHANNEL: u16 = 5;

// `MessageRegistry` ids, in registration order.
pub const PING_NET_ID: u16 = 0;
pub const PONG_NET_ID: u16 = 1;
pub const PROTOCOL_CHECK_NET_ID: u16 = 2;
pub const SENDER_METADATA_NET_ID: u16 = 3;
pub const INPUT_NET_ID: u16 = 4;

/// Stands in for `shared::protocol::Player` in the message's `PhantomData`.
/// The type is never constructed, so only its `serde` impls matter.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PlayerActions;

/// The input message this game sends; the one message whose body is worth
/// decoding in full.
pub type PlayerInput = InputMessage<BEIStateSequence<PlayerActions>>;

/// Mirror of lightyear's `ProtocolCheck` event, which lives in a private
/// module. It carries the three protocol checksums exchanged on connect.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
pub struct ProtocolHashes {
    pub messages: Option<u64>,
    pub components: Option<u64>,
    pub channels: Option<u64>,
}

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

/// One of the three `bevy_replicon` channels, which carry replication updates
/// rather than registered messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplicationChannel {
    Updates,
    Mutations,
    MutationAcks,
}

impl ReplicationChannel {
    fn from_id(channel: u16) -> Option<Self> {
        match channel {
            UPDATES_CHANNEL => Some(Self::Updates),
            MUTATIONS_CHANNEL => Some(Self::Mutations),
            MUTATION_ACKS_CHANNEL => Some(Self::MutationAcks),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Updates => "updates",
            Self::Mutations => "mutations",
            Self::MutationAcks => "mutation-acks",
        }
    }
}

#[derive(Debug)]
pub enum Decoded {
    Input {
        net_id: u16,
        message: Box<PlayerInput>,
    },
    ProtocolCheck {
        net_id: u16,
        hashes: ProtocolHashes,
    },
    /// `SenderMetadata` is serialized with lightyear's `ToBytes` layout; it is
    /// recognized but not interpreted. `bytes` is the whole message length.
    SenderMetadata {
        net_id: u16,
        bytes: usize,
    },
    /// A replicon payload: entity updates, mutations or mutation acks.
    Replication {
        channel: ReplicationChannel,
        bytes: usize,
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
    /// A registered message this tool does not know, or a payload that is not a
    /// message at all.
    Unknown {
        net_id: Option<u16>,
        bytes: usize,
    },
}

/// The packet kinds a display filter can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A client input message.
    Input,
    /// A lightyear protocol message (checksum or sender metadata).
    Metadata,
    /// A replicon update, mutation or mutation acknowledgement payload.
    Replication,
    Ping,
    Pong,
    /// A compressed body, a malformed datagram or an unknown message.
    Internal,
    /// A datagram that carries only acknowledgements.
    Ack,
}

impl Kind {
    /// Name shown in the packet log and used in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "INPUT",
            Self::Metadata => "METADATA",
            Self::Replication => "REPLICATION",
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
            Self::ProtocolCheck { net_id, .. } | Self::SenderMetadata { net_id, .. } => {
                (Kind::Metadata, Some(*net_id))
            }
            Self::Replication { .. } => (Kind::Replication, None),
            Self::Ping { net_id, .. } => (Kind::Ping, Some(*net_id)),
            Self::Pong { net_id, .. } => (Kind::Pong, Some(*net_id)),
            Self::Unknown { net_id, .. } => (Kind::Internal, *net_id),
        };
        MessageKey { kind, net_id }
    }
}

pub fn decode(channel: u16, payload: &[u8]) -> Decoded {
    if let Some(channel) = ReplicationChannel::from_id(channel) {
        return Decoded::Replication {
            channel,
            bytes: payload.len(),
        };
    }

    let Some((net_id, body)) = read_net_id(payload) else {
        return Decoded::Unknown {
            net_id: None,
            bytes: payload.len(),
        };
    };

    let decoded = match channel {
        PING_CHANNEL => decode_control(net_id, body),
        METADATA_CHANNEL => decode_metadata(net_id, body, payload.len()),
        INPUT_CHANNEL if net_id == INPUT_NET_ID => {
            decode_complete::<PlayerInput>(body).map(|message| Decoded::Input {
                net_id,
                message: Box::new(message),
            })
        }
        _ => None,
    };

    decoded.unwrap_or(Decoded::Unknown {
        net_id: Some(net_id),
        bytes: body.len(),
    })
}

fn decode_control(net_id: u16, body: &[u8]) -> Option<Decoded> {
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

fn decode_metadata(net_id: u16, body: &[u8], payload_len: usize) -> Option<Decoded> {
    match net_id {
        PROTOCOL_CHECK_NET_ID => decode_complete::<ProtocolHashes>(body)
            .map(|hashes| Decoded::ProtocolCheck { net_id, hashes }),
        SENDER_METADATA_NET_ID => Some(Decoded::SenderMetadata {
            net_id,
            bytes: payload_len,
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
        // Only a decode that consumes the whole payload is accepted: another
        // message can share a prefix with the one we are looking for.
        Ok((value, [])) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The input-channel payload of a 22-byte datagram captured from the client
    /// while it stood still: the message carries no inputs at all.
    const IDLE_INPUT: &[u8] = &[0x04, 0x00, 0xb1, 0x01, 0x00, 0x00];

    /// The input-channel payload of a 103-byte datagram captured while the
    /// scripted client was moving: three targets with a full action snapshot.
    const MOVING_INPUT: &[u8] = &[
        0x04, 0x00, 0xb2, 0x01, 0x03, 0x00, 0xe5, 0xfc, 0xff, 0xff, 0x8f, 0x80, 0x80, 0x80, 0x80,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xe4, 0xfc, 0xff, 0xff, 0x8f, 0x80, 0x80, 0x80, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe6, 0xfc, 0xff, 0xff, 0x8f, 0x80,
        0x80, 0x80, 0x80, 0x01, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3f, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00,
    ];

    #[test]
    fn an_idle_input_message_decodes() {
        let Decoded::Input { net_id, message } = decode(INPUT_CHANNEL, IDLE_INPUT) else {
            panic!("not decoded as an input message");
        };
        assert_eq!(net_id, INPUT_NET_ID);
        assert_eq!(message.end_tick.0, 177);
        assert!(message.inputs.is_empty(), "an idle tick sends no targets");
        assert!(!message.rebroadcast);
    }

    #[test]
    fn a_moving_input_message_decodes() {
        let Decoded::Input { message, .. } = decode(INPUT_CHANNEL, MOVING_INPUT) else {
            panic!("not decoded as an input message");
        };
        assert_eq!(message.end_tick.0, 178);
        assert_eq!(message.inputs.len(), 3, "one target per predicted player");
        assert!(!message.rebroadcast);
    }

    #[test]
    fn a_protocol_check_decodes_to_its_checksums() {
        // Captured from the server's first packet after the handshake.
        let payload = [
            0x02, 0x01, 0xed, 0xbd, 0xd2, 0x84, 0xc6, 0x82, 0xb1, 0xc0, 0x03, 0x01, 0xb9, 0xb9,
            0xbf, 0xab, 0xb2, 0xc8, 0xb2, 0x90, 0xc9, 0x01, 0x01, 0xfa, 0xae, 0x83, 0x96, 0xa0,
            0xe5, 0x91, 0xc8, 0x38,
        ];
        let Decoded::ProtocolCheck { net_id, hashes } = decode(METADATA_CHANNEL, &payload) else {
            panic!("not decoded as a protocol check");
        };
        assert_eq!(net_id, PROTOCOL_CHECK_NET_ID);
        assert!(hashes.messages.is_some());
        assert!(hashes.components.is_some());
        assert!(hashes.channels.is_some());
    }

    #[test]
    fn replicon_channels_are_reported_by_channel() {
        for channel in [UPDATES_CHANNEL, MUTATIONS_CHANNEL, MUTATION_ACKS_CHANNEL] {
            let Decoded::Replication {
                channel: replication,
                bytes,
            } = decode(channel, &[0x11, 0x22, 0x33])
            else {
                panic!("channel {channel} was decoded as a message");
            };
            assert_eq!(Some(replication), ReplicationChannel::from_id(channel));
            assert_eq!(bytes, 3);
        }
    }

    #[test]
    fn ping_and_pong_use_the_ping_channel_layout() {
        let ping = [0x00, 0x12, 0x34];
        let Decoded::Ping { net_id, id } = decode(PING_CHANNEL, &ping) else {
            panic!("not a ping");
        };
        assert_eq!((net_id, id), (PING_NET_ID, 0x1234));

        let mut pong_body = 0x1234u16.to_be_bytes().to_vec();
        pong_body.extend_from_slice(&7u32.to_be_bytes());
        pong_body.push(0x80);
        let mut pong = vec![PONG_NET_ID as u8];
        pong.extend_from_slice(&pong_body);
        let Decoded::Pong {
            net_id,
            id,
            tick_diff,
            overstep,
        } = decode(PING_CHANNEL, &pong)
        else {
            panic!("not a pong");
        };
        assert_eq!(
            (net_id, id, tick_diff, overstep),
            (PONG_NET_ID, 0x1234, 7, 0x80)
        );

        // The same bytes on another channel are not control messages.
        assert!(matches!(
            decode(INPUT_CHANNEL, &ping),
            Decoded::Unknown {
                net_id: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn a_payload_that_is_not_a_message_is_unknown() {
        let Decoded::Unknown { net_id, bytes } = decode(INPUT_CHANNEL, &[]) else {
            panic!("not unknown");
        };
        assert_eq!(net_id, None);
        assert_eq!(bytes, 0);

        // A known net id whose body does not decode is not guessed at.
        let Decoded::Unknown { net_id, bytes } = decode(INPUT_CHANNEL, &[0x04, 0xff, 0xff]) else {
            panic!("misdecoded a broken input message");
        };
        assert_eq!((net_id, bytes), (Some(INPUT_NET_ID), 2));
    }

    #[test]
    fn decoded_messages_expose_the_key_a_display_filter_matches() {
        assert_eq!(
            decode(INPUT_CHANNEL, IDLE_INPUT).key(),
            MessageKey {
                kind: Kind::Input,
                net_id: Some(INPUT_NET_ID),
            }
        );
        assert_eq!(
            decode(UPDATES_CHANNEL, &[0xff]).key(),
            MessageKey {
                kind: Kind::Replication,
                net_id: None,
            }
        );
        assert_eq!(
            decode(
                METADATA_CHANNEL,
                &[0x03, 0x00, 0x00, 0x00, 0x06, 0x00, 0x46, 0x5d]
            )
            .key(),
            MessageKey {
                kind: Kind::Metadata,
                net_id: Some(SENDER_METADATA_NET_ID),
            }
        );
    }
}
