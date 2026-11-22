//! Lightyear 0.31 transport framing, decoded from a raw UDP datagram.
//!
//! The layout is re-stated here instead of calling into lightyear: this crate is
//! a developer tool and must decode a capture without becoming a participant in
//! the protocol. Keep it in sync with
//! `lightyear_transport-0.31.0/src/packet/{header,message,packet,packet_type}.rs`.
//!
//! ```text
//! header (13 B)  type:u8  packet_id:u16be  last_ack:u16be  ack_bits:u32be  tick:u32be
//! Data           { channel:varint-u16  count:u8  { id:Option<u16be>  len:varint  bytes } * count } *
//! DataFragment   channel:varint-u16  message_id:u16be  fragment:varint-u64  count:varint-u64
//!                [compression:u8 on fragment 0]  len:varint  bytes  [Data batches]
//! AckOnly        <no body>
//! ```
//!
//! Lightyear's varint is a prefix-length encoding, not LEB128: the top two bits
//! of the first byte select 1, 2, 4 or 8 bytes and the value is big-endian.

use core::fmt;

/// Fixed size of [`PacketHeader`] on the wire.
pub const HEADER_BYTES: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Data,
    DataFragment,
    DataCompressed,
    DataFragmentCompressed,
    AckOnly,
}

impl PacketType {
    fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Data),
            1 => Some(Self::DataFragment),
            2 => Some(Self::DataCompressed),
            3 => Some(Self::DataFragmentCompressed),
            4 => Some(Self::AckOnly),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Data => "Data",
            Self::DataFragment => "DataFragment",
            Self::DataCompressed => "DataCompressed",
            Self::DataFragmentCompressed => "DataFragmentCompressed",
            Self::AckOnly => "AckOnly",
        }
    }
}

/// A malformed datagram, with the byte offset where parsing stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    Truncated { at: usize, need: usize, have: usize },
    UnknownPacketType { at: usize, value: u8 },
    ValueTooLarge { at: usize, field: &'static str },
    Trailing { at: usize, bytes: usize },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { at, need, have } => {
                write!(f, "truncated at byte {at}: need {need}, have {have}")
            }
            Self::UnknownPacketType { at, value } => {
                write!(f, "unknown packet type {value} at byte {at}")
            }
            Self::ValueTooLarge { at, field } => {
                write!(f, "{field} overflows u16 at byte {at}")
            }
            Self::Trailing { at, bytes } => {
                write!(f, "{bytes} trailing bytes at byte {at}")
            }
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    pub packet_type: PacketType,
    /// Sender's packet counter, wrapping at `u16`.
    pub packet_id: u16,
    /// Most recent packet id received by the sender.
    pub last_ack_packet_id: u16,
    /// Ack bits for the 32 packet ids before `last_ack_packet_id`.
    pub ack_bitfield: u32,
    /// Sender's current simulation tick.
    pub tick: u32,
}

#[derive(Debug)]
pub struct Packet<'a> {
    pub header: PacketHeader,
    pub body: Body<'a>,
    /// Length of the original datagram.
    pub len: usize,
}

#[derive(Debug)]
pub enum Body<'a> {
    /// One or more channel batches of whole messages.
    Data(Vec<Batch<'a>>),
    /// One transport fragment plus any whole messages packed after the last one.
    Fragment {
        channel: u16,
        fragment: Fragment<'a>,
        tail: Vec<Batch<'a>>,
    },
    AckOnly,
    /// The payload body is LZ4-compressed; this build does not enable that
    /// lightyear feature, so the body is reported but not decoded.
    Compressed,
}

#[derive(Debug)]
pub struct Batch<'a> {
    pub channel: u16,
    pub messages: Vec<Message<'a>>,
}

#[derive(Debug)]
pub struct Message<'a> {
    /// Transport message id, present on sequenced or fragmented delivery.
    pub id: Option<u16>,
    pub payload: &'a [u8],
}

#[derive(Debug)]
pub struct Fragment<'a> {
    pub message_id: u16,
    /// Zero-based index of this fragment.
    pub index: u64,
    /// Total number of fragments in the logical message.
    pub count: u64,
    /// Present only on fragment zero.
    pub compression: Option<u8>,
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, WireError> {
        if buf.len() < HEADER_BYTES {
            return Err(WireError::Truncated {
                at: 0,
                need: HEADER_BYTES,
                have: buf.len(),
            });
        }
        let mut cursor = Cursor::new(buf);

        let type_at = cursor.pos();
        let raw_type = cursor.u8()?;
        let packet_type = PacketType::from_u8(raw_type).ok_or(WireError::UnknownPacketType {
            at: type_at,
            value: raw_type,
        })?;
        let header = PacketHeader {
            packet_type,
            packet_id: cursor.u16_be()?,
            last_ack_packet_id: cursor.u16_be()?,
            ack_bitfield: cursor.u32_be()?,
            tick: cursor.u32_be()?,
        };

        let body = match packet_type {
            PacketType::Data => Body::Data(parse_batches(&mut cursor)?),
            PacketType::DataFragment => {
                let channel = cursor.varint_u16("channel id")?;
                let message_id = cursor.u16_be()?;
                let index = cursor.varint()?;
                let count = cursor.varint()?;
                let compression = if index == 0 { Some(cursor.u8()?) } else { None };
                let payload = cursor.bytes()?;
                // The builder may append whole single messages after the final
                // fragment; they use the same channel-batch framing.
                let tail = parse_batches(&mut cursor)?;
                Body::Fragment {
                    channel,
                    fragment: Fragment {
                        message_id,
                        index,
                        count,
                        compression,
                        payload,
                    },
                    tail,
                }
            }
            PacketType::AckOnly => {
                if cursor.remaining() != 0 {
                    return Err(WireError::Trailing {
                        at: cursor.pos(),
                        bytes: cursor.remaining(),
                    });
                }
                Body::AckOnly
            }
            PacketType::DataCompressed | PacketType::DataFragmentCompressed => Body::Compressed,
        };

        Ok(Self {
            header,
            body,
            len: buf.len(),
        })
    }
}

/// Splits one lightyear varint off the front of `buf`.
pub fn read_varint(buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut cursor = Cursor::new(buf);
    let value = cursor.varint().ok()?;
    Some((value, &buf[cursor.pos()..]))
}

fn parse_batches<'a>(cursor: &mut Cursor<'a>) -> Result<Vec<Batch<'a>>, WireError> {
    let mut batches = Vec::new();
    while cursor.remaining() > 0 {
        let channel = cursor.varint_u16("channel id")?;
        let count = cursor.u8()?;
        let mut messages = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let has_id = cursor.u8()? != 0;
            // `MessageId` is a fixed big-endian u16, unlike the varints used for
            // channel and message type ids.
            let id = if has_id { Some(cursor.u16_be()?) } else { None };
            let payload = cursor.bytes()?;
            messages.push(Message { id, payload });
        }
        batches.push(Batch { channel, messages });
    }
    Ok(batches)
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Truncated {
            at: self.pos,
            need: n,
            have: self.remaining(),
        })?;
        let out = self.buf.get(self.pos..end).ok_or(WireError::Truncated {
            at: self.pos,
            need: n,
            have: self.remaining(),
        })?;
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u16_be(&mut self) -> Result<u16, WireError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32_be(&mut self) -> Result<u32, WireError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn varint(&mut self) -> Result<u64, WireError> {
        let first = self.u8()?;
        match first >> 6 {
            0 => Ok(u64::from(first)),
            1 => {
                let second = self.u8()?;
                Ok(u64::from(u16::from_be_bytes([first, second]) & 0x3fff))
            }
            2 => {
                let rest = self.take(3)?;
                let value = u32::from_be_bytes([first, rest[0], rest[1], rest[2]]);
                Ok(u64::from(value & 0x3fff_ffff))
            }
            _ => {
                let rest = self.take(7)?;
                let value = u64::from_be_bytes([
                    first, rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6],
                ]);
                Ok(value & 0x3fff_ffff_ffff_ffff)
            }
        }
    }

    fn varint_u16(&mut self, field: &'static str) -> Result<u16, WireError> {
        let at = self.pos;
        let value = self.varint()?;
        u16::try_from(value).map_err(|_| WireError::ValueTooLarge { at, field })
    }

    fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let at = self.pos;
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| WireError::ValueTooLarge {
            at,
            field: "byte length",
        })?;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_varint(out: &mut Vec<u8>, value: u64) {
        if value <= 63 {
            out.push(value as u8);
        } else if value <= 16_383 {
            out.extend_from_slice(&((value as u16) | 0x4000).to_be_bytes());
        } else if value <= 1_073_741_823 {
            out.extend_from_slice(&((value as u32) | 0x8000_0000).to_be_bytes());
        } else {
            out.extend_from_slice(&(value | 0xc000_0000_0000_0000).to_be_bytes());
        }
    }

    fn header(packet_type: u8, packet_id: u16, tick: u32) -> Vec<u8> {
        let mut out = vec![packet_type];
        out.extend_from_slice(&packet_id.to_be_bytes());
        out.extend_from_slice(&7u16.to_be_bytes());
        out.extend_from_slice(&0x0f0e_0d0cu32.to_be_bytes());
        out.extend_from_slice(&tick.to_be_bytes());
        out
    }

    fn data_packet(payloads: &[&[u8]], channel: u16) -> Vec<u8> {
        let mut out = header(0, 42, 1234);
        put_varint(&mut out, u64::from(channel));
        out.push(payloads.len() as u8);
        for payload in payloads {
            out.push(0); // unreliable: no message id
            put_varint(&mut out, payload.len() as u64);
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn varint_roundtrips_at_every_width() {
        for value in [0, 1, 63, 64, 16_383, 16_384, 1_073_741_823, 1_073_741_824] {
            let mut bytes = Vec::new();
            put_varint(&mut bytes, value);
            let (back, rest) = read_varint(&bytes).expect("varint");
            assert_eq!(back, value);
            assert!(rest.is_empty());
        }

        // The prefix length is part of the encoding, not just a size hint.
        assert_eq!(read_varint(&[10]).unwrap().0, 10);
        assert_eq!(read_varint(&[0x40, 0x10]).unwrap().0, 16);
        assert_eq!(read_varint(&[0x80, 0, 0x40, 0x00]).unwrap().0, 16_384);
    }

    #[test]
    fn data_packet_parses_header_and_batches() {
        let bytes = data_packet(&[&[1, 2, 3], &[4, 5]], 3);
        let packet = Packet::parse(&bytes).expect("parse");
        assert_eq!(packet.header.packet_type, PacketType::Data);
        assert_eq!(packet.header.packet_id, 42);
        assert_eq!(packet.header.last_ack_packet_id, 7);
        assert_eq!(packet.header.ack_bitfield, 0x0f0e_0d0c);
        assert_eq!(packet.header.tick, 1234);
        let Body::Data(batches) = &packet.body else {
            panic!("not data");
        };
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].channel, 3);
        assert_eq!(batches[0].messages.len(), 2);
        assert_eq!(batches[0].messages[0].id, None);
        assert_eq!(batches[0].messages[0].payload, &[1, 2, 3]);
        assert_eq!(batches[0].messages[1].payload, &[4, 5]);
    }

    #[test]
    fn reliable_message_id_is_a_fixed_u16() {
        let mut bytes = header(0, 1, 1);
        put_varint(&mut bytes, 0);
        bytes.push(1);
        bytes.push(1);
        bytes.extend_from_slice(&0x1234u16.to_be_bytes());
        put_varint(&mut bytes, 1);
        bytes.push(0xaa);

        let packet = Packet::parse(&bytes).expect("parse");
        let Body::Data(batches) = &packet.body else {
            panic!("not data");
        };
        assert_eq!(batches[0].messages[0].id, Some(0x1234));
    }

    #[test]
    fn ack_only_must_not_carry_a_body() {
        let bytes = header(4, 9, 3);
        let packet = Packet::parse(&bytes).expect("ack-only");
        assert!(matches!(packet.body, Body::AckOnly));

        let mut with_body = header(4, 9, 3);
        with_body.push(0);
        assert!(matches!(
            Packet::parse(&with_body),
            Err(WireError::Trailing { .. })
        ));
    }

    #[test]
    fn compressed_packets_are_labelled_without_decoding() {
        let bytes = header(2, 9, 3);
        let packet = Packet::parse(&bytes).expect("compressed");
        assert_eq!(packet.header.packet_type, PacketType::DataCompressed);
        assert!(matches!(packet.body, Body::Compressed));
    }

    #[test]
    fn fragment_parses_compression_byte_and_tail_messages() {
        let mut bytes = header(1, 5, 99);
        put_varint(&mut bytes, 2); // channel
        bytes.extend_from_slice(&77u16.to_be_bytes()); // fixed message id
        put_varint(&mut bytes, 0); // first fragment
        put_varint(&mut bytes, 3); // three total
        bytes.push(0); // compression = none
        put_varint(&mut bytes, 2);
        bytes.extend_from_slice(&[0xde, 0xad]);

        // A whole message packed after the final fragment.
        put_varint(&mut bytes, 2);
        bytes.push(1);
        bytes.push(0);
        put_varint(&mut bytes, 1);
        bytes.push(0xbe);

        let packet = Packet::parse(&bytes).expect("parse");
        let Body::Fragment {
            channel,
            fragment,
            tail,
        } = &packet.body
        else {
            panic!("not a fragment");
        };
        assert_eq!(*channel, 2);
        assert_eq!(fragment.message_id, 77);
        assert_eq!(fragment.index, 0);
        assert_eq!(fragment.count, 3);
        assert_eq!(fragment.compression, Some(0));
        assert_eq!(fragment.payload, &[0xde, 0xad]);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].messages[0].payload, &[0xbe]);
    }

    #[test]
    fn truncation_is_an_error_and_never_a_panic() {
        let bytes = data_packet(&[&[1, 2, 3]], 0);
        for cut in 0..bytes.len() {
            let result = Packet::parse(&bytes[..cut]);
            if cut < HEADER_BYTES {
                assert!(result.is_err(), "partial header of {cut} bytes parsed");
            } else if cut == HEADER_BYTES {
                // A Data packet with no batches is valid: the header stood alone.
                let packet = result.expect("header-only Data packet is valid");
                let Body::Data(batches) = packet.body else {
                    panic!("header-only Data packet had a body");
                };
                assert!(batches.is_empty());
            } else {
                assert!(result.is_err(), "partial body of {cut} bytes parsed");
            }
        }
    }

    #[test]
    fn unknown_packet_type_is_reported_with_its_offset() {
        assert!(matches!(
            Packet::parse(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(WireError::UnknownPacketType { at: 0, value: 9 })
        ));
    }
}
