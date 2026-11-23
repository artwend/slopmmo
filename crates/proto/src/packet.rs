//! Message layout for both directions.
//!
//! Size budget is part of the design, so some fields are quantised and there is
//! no nesting beyond one repeated array. Lightyear serialises both messages
//! with postcard; `net`'s tests enforce the datagram ceiling on the result.

use core::fmt;

use crate::Tick;
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Peers a snapshot may carry. Independent of the server's `K`: the client is
/// allowed to know about slightly more colliders than the server solves.
pub const MAX_SNAPSHOT_PEERS: usize = 16;

/// Collision mode attached to a replicated peer collider.
pub mod peer_mode {
    pub const NONE: u8 = 0;
    pub const SOFT: u8 = 1;
    pub const HARD: u8 = 2;
}

/// What the client reports after its own terrain solve.
///
/// This is *state*, not a command: the server uses it as the base of its shadow
/// simulation and validates the implied displacement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MovementReport {
    pub tick: Tick,
    pub position: [f32; 3],
    /// World-space linear velocity, m/s.
    pub linear_velocity: [f32; 3],
    /// World-space angular velocity, rad/s. Only the Y (yaw) component is
    /// simulated; the character has no pitch or roll authority. Carried as a
    /// vector so the shadow can integrate it like `linear_velocity`.
    pub angular_velocity: [f32; 3],
    pub body_yaw: f32,
    pub grounded: bool,
    /// Last correction the client has applied.
    ///
    /// The server's transport keeps an unacknowledged correction in flight and
    /// repeats it (bounded) until this field catches up, so a correction lost
    /// with its datagram is repaired instead of silently dropped.
    pub applied_correction_seq: u16,
}

/// Client→server. Input and report are merged into one packet: they describe
/// the same tick window, and splitting them doubled the header cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerMovementPacket {
    /// [`crate::PROTOCOL_VERSION`] of the sender. First field on the wire so a
    /// mismatched build is rejected before the rest is looked at.
    ///
    /// `Default` is *not* a valid version; a zero here means "not stamped" and
    /// the server rejects it, which is what a hand-built packet should do.
    pub version: u16,
    /// Per-connection packet counter, used for loss statistics.
    pub seq: u16,
    pub player: u64,
    pub report: MovementReport,
    /// Newest first. Empty on a report-only packet.
    pub inputs: Vec<crate::InputFrame>,
}

/// A replicated collider. The client feeds these into its own solver instead of
/// waiting for the server to move it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerCollider {
    pub player: u64,
    pub position: [f32; 3],
    pub body_yaw: f32,
    pub mode: u8,
}

impl PeerCollider {
    /// Neutral value used to fill the tail of a [`PeerList`].
    pub const EMPTY: Self = Self {
        player: 0,
        position: [0.0; 3],
        body_yaw: 0.0,
        mode: peer_mode::NONE,
    };
}

/// Inline, fixed-capacity peer list.
///
/// The server builds one of these per player per tick, so a `Vec` here would be
/// a heap allocation in the hot loop. The cap is also the wire budget
/// ([`MAX_SNAPSHOT_PEERS`]), so nothing is lost: the encoder never wrote more
/// than that anyway. Because it is inline, [`ServerSnapshot`] is `Copy` and can
/// be handed to the network layer without cloning.
///
/// The `serde` representation is a plain sequence of colliders, byte-identical
/// to the `Vec` this replaced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeerList {
    items: [PeerCollider; MAX_SNAPSHOT_PEERS],
    len: u8,
}

impl PeerList {
    pub const CAPACITY: usize = MAX_SNAPSHOT_PEERS;

    pub const fn new() -> Self {
        Self {
            items: [PeerCollider::EMPTY; MAX_SNAPSHOT_PEERS],
            len: 0,
        }
    }

    /// Appends a collider. Returns `false` when the list is already full, so a
    /// caller that must not silently drop peers can notice.
    pub fn push(&mut self, peer: PeerCollider) -> bool {
        let Some(slot) = self.items.get_mut(self.len as usize) else {
            return false;
        };
        *slot = peer;
        self.len += 1;
        true
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[PeerCollider] {
        &self.items[..self.len as usize]
    }

    pub fn iter(&self) -> core::slice::Iter<'_, PeerCollider> {
        self.as_slice().iter()
    }
}

impl PlayerMovementPacket {
    /// True when the packet was produced by the same protocol revision as this
    /// build. Every sender stamps [`crate::PROTOCOL_VERSION`]; a hand-built
    /// packet with the default `0` is therefore rejected rather than trusted.
    pub fn is_current_version(&self) -> bool {
        self.version == crate::PROTOCOL_VERSION
    }
}

impl Default for PeerList {
    fn default() -> Self {
        Self::new()
    }
}

impl FromIterator<PeerCollider> for PeerList {
    /// Convenience for construction sites (tests, fixtures). Overflow is a
    /// programming error here rather than wire input, so it panics.
    fn from_iter<I: IntoIterator<Item = PeerCollider>>(iter: I) -> Self {
        let mut out = Self::new();
        for peer in iter {
            assert!(out.push(peer), "more than {} peers", MAX_SNAPSHOT_PEERS);
        }
        out
    }
}

impl Serialize for PeerList {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.len()))?;
        for peer in self.iter() {
            seq.serialize_element(peer)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for PeerList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ListVisitor;

        impl<'de> Visitor<'de> for ListVisitor {
            type Value = PeerList;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MAX_SNAPSHOT_PEERS} peer colliders")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<PeerList, A::Error> {
                let mut out = PeerList::new();
                while let Some(peer) = seq.next_element::<PeerCollider>()? {
                    if !out.push(peer) {
                        return Err(de::Error::custom("too many peer colliders"));
                    }
                }
                Ok(out)
            }
        }

        d.deserialize_seq(ListVisitor)
    }
}

/// Aggregated per-player correction. Deliberately carries no position delta:
/// the server cannot guarantee such an offset would not land inside a wall.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CollisionCorrection {
    /// Idempotency key. A replayed `(seq, tick)` pair must be applied once.
    pub seq: u16,
    pub tick: Tick,
    /// Bumped on handover or mode change so stale corrections are discarded.
    pub epoch: u32,
    pub velocity_delta: [f32; 3],
    /// Present for hard contacts: the normal the client must slide along.
    pub hard_normal: Option<[f32; 3]>,
}

/// Server→client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub server_tick: Tick,
    /// Last client packet sequence the server accepted for this player.
    ///
    /// The client compares it with the sequence it last sent to estimate how
    /// many reports are still in flight; nothing else depends on it.
    pub acked_seq: u16,
    /// Server shadow position. Informational: the client must not adopt it,
    /// except while `resync` is set.
    pub shadow_position: [f32; 3],
    pub correction: Option<CollisionCorrection>,
    /// Replicated colliders near this player.
    ///
    /// The client files the list under `server_tick`, so the local solve reads
    /// the set that belonged to the tick it predicts rather than "whatever
    /// arrived last" (`sim::peers::PeerHistory`).
    pub peers: PeerList,
    /// Set while a resumed session has not yet been seen at the server's
    /// placement: the client must adopt `shadow_position`. Kept set until an
    /// accepted report arrives, so a lost unreliable snapshot cannot strand a
    /// resumed client far from its shadow.
    pub resync: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{INPUT_REDUNDANCY, InputFrame, buttons};

    fn frame(tick: u32) -> InputFrame {
        InputFrame {
            tick: Tick::new(tick),
            move_x: 100,
            move_z: -40,
            buttons: buttons::JUMP | buttons::SPRINT,
            camera_yaw: crate::quantise_yaw(1.25),
        }
    }

    /// Round-trips through the same codec Lightyear uses for these messages.
    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let bytes = postcard::to_allocvec(value).expect("serialise");
        postcard::from_bytes(&bytes).expect("deserialise")
    }

    #[test]
    fn client_packet_roundtrip() {
        let pkt = PlayerMovementPacket {
            version: crate::PROTOCOL_VERSION,
            seq: 77,
            player: 0xDEAD_BEEF,
            report: MovementReport {
                tick: Tick::new(1234),
                position: [12.345, 1.5, -7.25],
                linear_velocity: [3.5, -0.5, 0.0],
                angular_velocity: [0.0, 2.5, 0.0],
                body_yaw: 2.0,
                grounded: true,
                applied_correction_seq: 9,
            },
            inputs: (0..=INPUT_REDUNDANCY as u32).map(frame).collect(),
        };
        let back: PlayerMovementPacket = roundtrip(&pkt);
        assert_eq!(back.seq, pkt.seq);
        assert_eq!(back.player, pkt.player);
        assert_eq!(back.inputs.len(), pkt.inputs.len());
        assert_eq!(back.report.applied_correction_seq, 9);
        assert!(back.report.grounded);
        assert!((back.report.position[0] - 12.345).abs() < 1e-3);
        assert!((back.report.linear_velocity[0] - 3.5).abs() < 1e-3);
        assert!((back.report.angular_velocity[1] - 2.5).abs() < 1e-3);
        assert!((back.report.body_yaw - 2.0).abs() < 1e-3);
        assert_eq!(back.inputs[0].buttons, pkt.inputs[0].buttons);
        assert!(pkt.inputs[0].jump());
    }

    #[test]
    fn snapshot_roundtrip_with_and_without_correction() {
        let empty = ServerSnapshot {
            server_tick: Tick::new(9),
            acked_seq: 3,
            shadow_position: [1.0, 0.0, 2.0],
            correction: None,
            resync: false,
            peers: core::iter::once(PeerCollider {
                player: 1,
                position: [0.1, 0.2, 0.3],
                body_yaw: 0.5,
                mode: peer_mode::SOFT,
            })
            .collect(),
        };
        let back: ServerSnapshot = roundtrip(&empty);
        assert!(back.correction.is_none());
        assert!(!back.resync);
        assert_eq!(back.peers.len(), 1);
        assert_eq!(back.peers.as_slice()[0].mode, peer_mode::SOFT);

        let with = ServerSnapshot {
            correction: Some(CollisionCorrection {
                seq: 42,
                tick: Tick::new(9),
                epoch: 7,
                velocity_delta: [0.5, 0.0, -0.25],
                hard_normal: Some([1.0, 0.0, 0.0]),
            }),
            ..empty
        };
        let back: ServerSnapshot = roundtrip(&with);
        let c = back.correction.expect("correction survives");
        assert_eq!(c.seq, 42);
        assert_eq!(c.epoch, 7);
        assert!((c.velocity_delta[0] - 0.5).abs() < 1e-3);
        assert!(c.hard_normal.is_some());
        assert!(!back.resync, "resync invented from the correction flag");

        // The flag is independent of the correction, in both combinations.
        let resync_only = ServerSnapshot {
            resync: true,
            ..empty
        };
        let back: ServerSnapshot = roundtrip(&resync_only);
        assert!(back.resync);
        assert!(back.correction.is_none());

        let both = ServerSnapshot {
            resync: true,
            ..with
        };
        let back: ServerSnapshot = roundtrip(&both);
        assert!(back.resync);
        assert!(back.correction.is_some());
    }

    #[test]
    fn a_full_peer_list_is_carried_whole_and_an_overfull_one_cannot_be_built() {
        let full: PeerList = (0..MAX_SNAPSHOT_PEERS as u64)
            .map(|i| PeerCollider {
                player: i + 1,
                position: [i as f32, 0.9, 0.0],
                body_yaw: 0.0,
                mode: peer_mode::SOFT,
            })
            .collect();
        assert_eq!(full.len(), MAX_SNAPSHOT_PEERS);

        let snap = ServerSnapshot {
            peers: full,
            ..Default::default()
        };
        let back: ServerSnapshot = roundtrip(&snap);
        assert_eq!(back.peers, full, "last peer was truncated on the wire");

        // Capacity is enforced at insertion, so an overfull list cannot exist.
        let mut list = full;
        assert!(
            !list.push(PeerCollider {
                player: 99,
                ..PeerCollider::EMPTY
            }),
            "peer list grew past its capacity"
        );
        assert_eq!(list.len(), MAX_SNAPSHOT_PEERS);

        // A list claiming more colliders than the cap is rejected while
        // decoding instead of being silently truncated.
        let overfull: Vec<PeerCollider> = (0..=MAX_SNAPSHOT_PEERS as u64)
            .map(|i| PeerCollider {
                player: i + 1,
                ..PeerCollider::EMPTY
            })
            .collect();
        let bytes = postcard::to_allocvec(&overfull).expect("serialise");
        assert!(
            postcard::from_bytes::<PeerList>(&bytes).is_err(),
            "decoded more peers than the list can hold"
        );
    }

    #[test]
    fn truncated_packets_fail_cleanly() {
        let bytes = postcard::to_allocvec(&PlayerMovementPacket {
            inputs: vec![frame(1)],
            ..Default::default()
        })
        .expect("serialise");
        for cut in 0..bytes.len() {
            assert!(postcard::from_bytes::<PlayerMovementPacket>(&bytes[..cut]).is_err());
        }
    }
}
