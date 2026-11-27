//! Avian's virtual character controller, ported to released `avian3d` 0.8.
//!
//! This is the API agreed in ADR 008 and taken from Avian's `virtual-character`
//! branch: a [`VirtualCharacter`] entity with [`AutoMoveAndSlide`], moved by
//! [`MoveAndSlidePlugin`] inside Avian's `PhysicsSchedule`.
//!
//! The branch itself is a moving target, so the port is explicit about what it
//! relies on: Avian 0.8's released `MoveAndSlide` and `cast_move`, no fork.
//! [`SlopeHandling`], [`GroundSnapping`] and [`StairStepping`] are the branch's
//! components with the same fields and defaults; the branch declares them but
//! its system does not apply them yet. Here `SlopeHandling::max_ground_angle`
//! and `GroundSnapping` are applied, because `Grounded` is what the movement
//! model reads every tick. The remaining fields are carried for API parity and
//! documented as not applied.

mod virtual_character;

pub use avian3d;
pub use virtual_character::*;

/// The controller's components plus Avian's, so a client needs one import.
pub mod prelude {
    pub use crate::{
        AutoMoveAndSlide, GroundSnapping, Grounded, MoveAndSlidePlugin, SlopeHandling,
        StairStepping, UpDirection, VirtualCharacter, VirtualCharacterSystems,
    };
    pub use avian3d::prelude::*;
}
