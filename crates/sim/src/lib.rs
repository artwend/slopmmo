//! Authoritative simulation primitives.
//!
//! No Bevy ECS and no physics engine: this crate must stay cheap to compile and
//! trivial to unit-test, because every decision this project makes about tick
//! rate, neighbour limits and solver cost is measured here first.
//!
//! Split of responsibility (see `docs/architecture-review.md`):
//!
//! * [`kcc`] — capsule vs static geometry, client side, terrain only.
//! * [`collision`] — capsule vs capsule soft/hard solve, server side, no terrain.
//! * [`peers`] — the client's history-tracked peer-collider set and local solve.
//! * [`authority`] — shadow state, validation, correction aggregation.
//! * [`movement`] — the input→velocity model shared by client and bots.

pub mod authority;
pub mod collision;
pub mod kcc;
pub mod movement;
pub mod peers;
pub mod spatial;
pub mod world;

pub use authority::{Reject, Resume, ServerPlayer, SimConfig, SimStats, Simulation};
pub use collision::{Body, Impulse, Mode, Pair, PairScratch};
pub use kcc::{Capsule, KccParams, MoveOutput};
pub use movement::{MoveParams, step_velocity};
pub use peers::{LocalPeerSolve, PEER_HISTORY_DEPTH, PeerHistory, SampledPeer};
pub use spatial::SpatialGrid;
pub use world::{Aabb, StaticWorld};

pub use collision::{build_pairs, solve, solve_into};
pub use kcc::move_and_slide;
