//! Wire protocol shared by client and server.
//!
//! Design constraints taken from the architecture review:
//!
//! * Everything is tick addressed. There is no "current position" on the wire.
//! * Input travels with redundancy so a dropped datagram does not stall movement.
//! * The server never sends a position the client is expected to adopt; it sends
//!   collider state and, rarely, a velocity correction.

mod packet;
mod wire;

pub use packet::peer_mode;
pub use packet::{
    CollisionCorrection, KIND_PLAYER_MOVEMENT, KIND_SERVER_SNAPSHOT, MAX_SNAPSHOT_PEERS,
    MovementReport, PeerCollider, PeerList, PlayerMovementPacket, ServerSnapshot,
};
pub use wire::{Reader, WireError, Writer};

use core::f32::consts::TAU;
use serde::{Deserialize, Serialize};

/// Authoritative simulation rate.
pub const TICK_HZ: u32 = 60;
/// Seconds per simulation tick.
pub const TICK_DT: f32 = 1.0 / TICK_HZ as f32;
/// How often the client transmits. Lower than the tick rate on purpose: input
/// frames are addressed by tick, so the send rate is not the simulation rate.
pub const INPUT_SEND_HZ: u32 = 30;
/// Frames retransmitted in every packet: the newest plus this many predecessors.
pub const INPUT_REDUNDANCY: usize = 3;
/// Maximum number of input frames in a single packet.
pub const MAX_INPUT_FRAMES: usize = INPUT_REDUNDANCY + 1;

/// Starting neighbour bore. Both a radius and a count limit are required;
/// a bare "nearest 8" would create contacts across an empty field.
pub const MAX_NEIGHBOURS: usize = 8;
pub const NEIGHBOUR_RADIUS: f32 = 2.0;
/// Maximum accepted collision pairs per player, after pair de-duplication.
pub const MAX_PAIRS_PER_PLAYER: usize = 8;

pub const PLAYER_RADIUS: f32 = 0.4;
/// Full capsule height, including both caps.
pub const PLAYER_HEIGHT: f32 = 1.8;

/// Target gap kept between two touching capsules.
pub const SOFT_MARGIN: f32 = 0.05;
/// Maximum total velocity correction a single player may receive per tick.
pub const CORRECTION_BUDGET: f32 = 2.0;

/// Tolerance factor for the reported displacement check.
pub const POSITION_SLACK: f32 = 1.35;
/// Reports older than this many ticks are ignored.
pub const REPORT_WINDOW: u32 = 30;

pub const MAX_SPEED: f32 = 6.0;
pub const MAX_ACCEL: f32 = 24.0;

/// Fixed-point scale for positions and velocities: millimetres, millimetres/s.
pub const QUANT_MM: f32 = 1000.0;
/// Fixed-point scale for unit normals.
pub const QUANT_NORMAL: f32 = 1000.0;

/// Simulation tick counter.
///
/// Ordering is only meaningful modulo `u32` wrap; always compare with
/// [`Tick::since`] rather than `Ord`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Tick = Tick(0);

    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn add(self, n: u32) -> Self {
        Self(self.0.wrapping_add(n))
    }

    /// Ticks elapsed from `self` to `now`. Negative when `now` is in the past.
    pub const fn since(self, now: Tick) -> i32 {
        now.0.wrapping_sub(self.0) as i32
    }

    pub const fn is_stale(self, now: Tick, window: u32) -> bool {
        self.since(now) > window as i32
    }
}

/// One input sample, addressed to the tick it was produced on.
///
/// `move_x`/`move_z` are the camera-relative stick, quantised to `-127..=127`.
/// `camera_yaw` is sent because the server must never reconstruct it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFrame {
    pub tick: Tick,
    pub move_x: i8,
    pub move_z: i8,
    pub buttons: u8,
    pub camera_yaw: u16,
}

/// Input button bits.
pub mod buttons {
    pub const JUMP: u8 = 1 << 0;
    pub const SPRINT: u8 = 1 << 1;
    pub const CROUCH: u8 = 1 << 2;
}

impl InputFrame {
    pub fn jump(&self) -> bool {
        self.buttons & buttons::JUMP != 0
    }

    pub fn sprint(&self) -> bool {
        self.buttons & buttons::SPRINT != 0
    }

    /// Camera-relative wish direction in the XZ plane, length in `0..=1`.
    pub fn wish_dir(&self) -> [f32; 2] {
        let x = self.move_x as f32 / 127.0;
        let z = self.move_z as f32 / 127.0;
        let len = (x * x + z * z).sqrt();
        if len > 1.0 {
            [x / len, z / len]
        } else {
            [x, z]
        }
    }

    pub fn camera_yaw_rad(&self) -> f32 {
        yaw_dequantise(self.camera_yaw)
    }
}

pub fn quantise_position(v: f32) -> i32 {
    (v * QUANT_MM).round() as i32
}

pub fn dequantise_position(v: i32) -> f32 {
    v as f32 / QUANT_MM
}

pub fn quantise_normal(v: f32) -> i16 {
    (v * QUANT_NORMAL).round().clamp(-32767.0, 32767.0) as i16
}

pub fn dequantise_normal(v: i16) -> f32 {
    v as f32 / QUANT_NORMAL
}

pub fn quantise_yaw(rad: f32) -> u16 {
    let turns = rad.rem_euclid(TAU) / TAU;
    (turns * 65536.0).round() as u16
}

pub fn yaw_dequantise(q: u16) -> f32 {
    q as f32 / 65536.0 * TAU
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_ordering_survives_wrap() {
        let a = Tick::new(u32::MAX);
        let b = a.add(2);
        assert_eq!(b.0, 1);
        assert_eq!(a.since(b), 2);
        assert_eq!(b.since(a), -2);
    }

    #[test]
    fn yaw_roundtrip_is_stable() {
        for rad in [-3.0_f32, -1.5, 0.0, 0.9, 2.9, 6.2] {
            let back = yaw_dequantise(quantise_yaw(rad));
            let diff = (back - rad.rem_euclid(TAU)).abs();
            assert!(diff < 1e-3, "rad={rad} back={back}");
        }
    }

    #[test]
    fn wish_dir_is_clamped() {
        let f = InputFrame {
            move_x: 127,
            move_z: 127,
            ..Default::default()
        };
        let d = f.wish_dir();
        assert!((d[0] * d[0] + d[1] * d[1] - 1.0).abs() < 1e-4);
    }
}
