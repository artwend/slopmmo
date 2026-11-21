//! Message layout for both directions.
//!
//! Size budget is part of the design, so fields are quantised and there is no
//! nesting beyond one repeated array. A client packet costs about 60 bytes,
//! a server snapshot about 30 + 15 bytes per peer.

use crate::{
    MAX_INPUT_FRAMES, Tick, dequantise_normal, dequantise_position, quantise_normal,
    quantise_position,
};
use serde::{Deserialize, Serialize};

pub const KIND_PLAYER_MOVEMENT: u8 = 1;
pub const KIND_SERVER_SNAPSHOT: u8 = 2;

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
    pub velocity: [f32; 3],
    pub body_yaw: f32,
    pub grounded: bool,
    /// Last correction the client has applied, for idempotent re-delivery.
    pub applied_correction_seq: u16,
}

impl MovementReport {
    fn write(&self, w: &mut crate::Writer) {
        w.u32(self.tick.0);
        for v in self.position {
            w.i32(quantise_position(v));
        }
        for v in self.velocity {
            w.i32(quantise_position(v));
        }
        w.u16(crate::quantise_yaw(self.body_yaw));
        w.u16(self.applied_correction_seq);
        w.u8(self.grounded as u8);
    }

    fn read(r: &mut crate::Reader<'_>) -> Result<Self, crate::WireError> {
        let tick = Tick(r.u32()?);
        let mut position = [0.0; 3];
        for v in &mut position {
            *v = dequantise_position(r.i32()?);
        }
        let mut velocity = [0.0; 3];
        for v in &mut velocity {
            *v = dequantise_position(r.i32()?);
        }
        let body_yaw = crate::yaw_dequantise(r.u16()?);
        let applied_correction_seq = r.u16()?;
        let grounded = r.u8()? != 0;
        Ok(Self {
            tick,
            position,
            velocity,
            body_yaw,
            grounded,
            applied_correction_seq,
        })
    }
}

/// Client→server. Input and report are merged into one packet: they describe
/// the same tick window, and splitting them doubled the header cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerMovementPacket {
    /// Per-connection packet counter, used for loss statistics.
    pub seq: u16,
    pub player: u64,
    pub report: MovementReport,
    /// Newest first. Empty on a report-only packet.
    pub inputs: Vec<crate::InputFrame>,
}

impl PlayerMovementPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = crate::Writer::with_capacity(96);
        w.u8(KIND_PLAYER_MOVEMENT);
        w.u16(self.seq);
        w.u64(self.player);
        self.report.write(&mut w);
        let n = self.inputs.len().min(MAX_INPUT_FRAMES);
        w.u8(n as u8);
        for f in self.inputs.iter().take(n) {
            w.u32(f.tick.0);
            w.i8(f.move_x);
            w.i8(f.move_z);
            w.u8(f.buttons);
            w.u16(f.camera_yaw);
        }
        w.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, crate::WireError> {
        let mut r = crate::Reader::new(buf);
        match r.u8()? {
            KIND_PLAYER_MOVEMENT => {}
            other => return Err(crate::WireError::UnknownKind(other)),
        }
        let seq = r.u16()?;
        let player = r.u64()?;
        let report = MovementReport::read(&mut r)?;
        let n = r.count(MAX_INPUT_FRAMES)?;
        let mut inputs = Vec::with_capacity(n);
        for _ in 0..n {
            inputs.push(crate::InputFrame {
                tick: Tick(r.u32()?),
                move_x: r.i8()?,
                move_z: r.i8()?,
                buttons: r.u8()?,
                camera_yaw: r.u16()?,
            });
        }
        Ok(Self {
            seq,
            player,
            report,
            inputs,
        })
    }
}

/// A replicated collider. The client feeds these into its own solver instead of
/// waiting for the server to move it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerCollider {
    pub player: u64,
    pub position: [f32; 3],
    pub body_yaw: f32,
    pub mode: u8,
}

impl PeerCollider {
    const SIZE: usize = 8 + 12 + 2 + 1;

    fn write(&self, w: &mut crate::Writer) {
        w.u64(self.player);
        for v in self.position {
            w.i32(quantise_position(v));
        }
        w.u16(crate::quantise_yaw(self.body_yaw));
        w.u8(self.mode);
    }

    fn read(r: &mut crate::Reader<'_>) -> Result<Self, crate::WireError> {
        let player = r.u64()?;
        let mut position = [0.0; 3];
        for v in &mut position {
            *v = dequantise_position(r.i32()?);
        }
        let body_yaw = crate::yaw_dequantise(r.u16()?);
        let mode = r.u8()?;
        Ok(Self {
            player,
            position,
            body_yaw,
            mode,
        })
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

impl CollisionCorrection {
    fn write(&self, w: &mut crate::Writer) {
        w.u16(self.seq);
        w.u32(self.tick.0);
        w.u32(self.epoch);
        for v in self.velocity_delta {
            w.i32(quantise_position(v));
        }
        match self.hard_normal {
            Some(n) => {
                w.u8(1);
                for v in n {
                    w.i16(quantise_normal(v));
                }
            }
            None => {
                w.u8(0);
                for _ in 0..3 {
                    w.i16(0);
                }
            }
        }
    }

    fn read(r: &mut crate::Reader<'_>) -> Result<Self, crate::WireError> {
        let seq = r.u16()?;
        let tick = Tick(r.u32()?);
        let epoch = r.u32()?;
        let mut velocity_delta = [0.0; 3];
        for v in &mut velocity_delta {
            *v = dequantise_position(r.i32()?);
        }
        let has_normal = r.u8()? != 0;
        let mut n = [0.0; 3];
        for v in &mut n {
            *v = dequantise_normal(r.i16()?);
        }
        Ok(Self {
            seq,
            tick,
            epoch,
            velocity_delta,
            hard_normal: has_normal.then_some(n),
        })
    }
}

/// Server→client.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub server_tick: Tick,
    pub acked_seq: u16,
    /// Server shadow position. Informational: the client must not adopt it.
    pub shadow_position: [f32; 3],
    pub correction: Option<CollisionCorrection>,
    pub peers: Vec<PeerCollider>,
}

impl ServerSnapshot {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = crate::Writer::with_capacity(64 + self.peers.len() * PeerCollider::SIZE);
        w.u8(KIND_SERVER_SNAPSHOT);
        w.u32(self.server_tick.0);
        w.u16(self.acked_seq);
        for v in self.shadow_position {
            w.i32(quantise_position(v));
        }
        match &self.correction {
            Some(c) => {
                w.u8(1);
                c.write(&mut w);
            }
            None => w.u8(0),
        }
        let n = self.peers.len().min(MAX_SNAPSHOT_PEERS);
        w.u8(n as u8);
        for p in self.peers.iter().take(n) {
            p.write(&mut w);
        }
        w.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, crate::WireError> {
        let mut r = crate::Reader::new(buf);
        match r.u8()? {
            KIND_SERVER_SNAPSHOT => {}
            other => return Err(crate::WireError::UnknownKind(other)),
        }
        let server_tick = Tick(r.u32()?);
        let acked_seq = r.u16()?;
        let mut shadow_position = [0.0; 3];
        for v in &mut shadow_position {
            *v = dequantise_position(r.i32()?);
        }
        let correction = if r.u8()? != 0 {
            Some(CollisionCorrection::read(&mut r)?)
        } else {
            None
        };
        let n = r.count(MAX_SNAPSHOT_PEERS)?;
        let mut peers = Vec::with_capacity(n);
        for _ in 0..n {
            peers.push(PeerCollider::read(&mut r)?);
        }
        Ok(Self {
            server_tick,
            acked_seq,
            shadow_position,
            correction,
            peers,
        })
    }

    /// Upper bound on the encoded size, for buffer sizing on both ends.
    pub const MAX_ENCODED: usize = 64 + MAX_SNAPSHOT_PEERS * PeerCollider::SIZE;
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

    #[test]
    fn client_packet_roundtrip() {
        let pkt = PlayerMovementPacket {
            seq: 77,
            player: 0xDEAD_BEEF,
            report: MovementReport {
                tick: Tick::new(1234),
                position: [12.345, 1.5, -7.25],
                velocity: [3.5, -0.5, 0.0],
                body_yaw: 2.0,
                grounded: true,
                applied_correction_seq: 9,
            },
            inputs: (0..=INPUT_REDUNDANCY as u32).map(frame).collect(),
        };
        let bytes = pkt.encode();
        let back = PlayerMovementPacket::decode(&bytes).unwrap();
        assert_eq!(back.seq, pkt.seq);
        assert_eq!(back.player, pkt.player);
        assert_eq!(back.inputs.len(), pkt.inputs.len());
        assert_eq!(back.report.applied_correction_seq, 9);
        assert!(back.report.grounded);
        assert!((back.report.position[0] - 12.345).abs() < 1e-3);
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
            peers: vec![PeerCollider {
                player: 1,
                position: [0.1, 0.2, 0.3],
                body_yaw: 0.5,
                mode: peer_mode::SOFT,
            }],
        };
        let back = ServerSnapshot::decode(&empty.encode()).unwrap();
        assert!(back.correction.is_none());
        assert_eq!(back.peers.len(), 1);
        assert_eq!(back.peers[0].mode, peer_mode::SOFT);

        let with = ServerSnapshot {
            correction: Some(CollisionCorrection {
                seq: 42,
                tick: Tick::new(9),
                epoch: 7,
                velocity_delta: [0.5, 0.0, -0.25],
                hard_normal: Some([1.0, 0.0, 0.0]),
            }),
            ..empty.clone()
        };
        let back = ServerSnapshot::decode(&with.encode()).unwrap();
        let c = back.correction.expect("correction survives");
        assert_eq!(c.seq, 42);
        assert_eq!(c.epoch, 7);
        assert!((c.velocity_delta[0] - 0.5).abs() < 1e-3);
        assert!(c.hard_normal.is_some());
    }

    #[test]
    fn oversized_counts_are_rejected() {
        let mut bytes = PlayerMovementPacket::default().encode();
        // The input count is the byte right after the kind tag, seq, id and report.
        let idx = bytes.len() - 1;
        bytes[idx] = 200;
        assert!(matches!(
            PlayerMovementPacket::decode(&bytes),
            Err(crate::WireError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn truncated_packets_fail_cleanly() {
        let bytes = PlayerMovementPacket {
            inputs: vec![frame(1)],
            ..Default::default()
        }
        .encode();
        for cut in 0..bytes.len() {
            assert!(PlayerMovementPacket::decode(&bytes[..cut]).is_err());
        }
    }
}
