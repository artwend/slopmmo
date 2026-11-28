# ADR 014 — Character feel: camera-relative rotation, moderate air control, soft collision

- **Status:** Partial — camera-relative movement and air control are implemented; turning the
  character toward its movement direction and soft character collision are not.
- **Decided by:** project owner, second dialogue, turn 3.
- **Verbatim:** *«выбираю: Rotation: camera Air control: moderate Character collision: soft»*

## Context

Movement feel was chosen from three axes, each with a small set of options:

| Axis | Options | Owner's pick |
| --- | --- | --- |
| Rotation | character-relative / camera-relative / turn-in-place | **camera** |
| Air control | none / limited / moderate / full | **moderate** |
| Character collision | none / soft / hard | **soft** |

"And the KCC must work under Lightyear prediction with rollback" was the accompanying constraint:
the client predicts, the server may correct, and the client replays. Any feel rule that depends on
wall-clock time or on frame count breaks under replay.

## Decision

- **Camera-relative rotation.** The character turns toward the movement direction expressed in
  camera space; the camera is the steering device. This is what makes WASD feel correct with a
  mouse-look camera, and it is what most MMO clients do. Turn-in-place is explicitly *not* used.
- **Moderate air control** — horizontal acceleration in air is a fraction of ground acceleration
  (implemented as a reduced `MAX_ACCEL` while airborne), enough to steer a jump, not enough to
  reverse it.
- **Soft character collision** as the default (ADR 010).
- All feel rules are **tick-based and replayable**: no `Time::delta` in gameplay math, no
  frame-dependent smoothing inside the KCC. Smoothing of the *visual* result is allowed and happens
  after the fact.

## Consequences

- Camera yaw is part of the input packet (`camera_yaw`), because rotation depends on it; the server
  needs it for the shadow's `body_yaw`.
- Under rollback, the client must replay with the camera yaw it had **at that tick**, not the current
  one, or the replay diverges. This is why `camera_yaw` is per input frame, not per packet.
- Turn-in-place animations, if ever wanted, must be authored as camera-relative behaviour with a
  dead zone, not as a separate model.
- Ground friction is linear (exponential settle) rather than a constant multiplier, so the stop
  distance is stable across tick rates.

## What this repository does

`crates/shared/src/movement.rs` — camera-relative wish direction (the BEI `Movement` action is in
camera space, with forward = −Z), `MAX_SPEED = 6.0`, `MAX_ACCEL = 24.0`, sprint at 1.5×, ground
friction 14, air acceleration 6 (a quarter of ground), gravity −24 m/s² and jump 7 m/s. The same
`step_velocity_raw` runs on both sides in `FixedUpdate`.

Not implemented: the character does not turn toward its movement direction yet — `Rotation` is
replicated and predicted but never written from input — and soft character collision does not exist
(ADR 010). Camera yaw is likewise not on the wire: the follow camera reads the controlled player's
`Position` and looks at it.

`shared::kcc` is the engine-free reference solver and is deterministic; it is used by the `kcc`
parity test. The networked path uses Avian's `MoveAndSlidePlugin`, which steps inside
`PhysicsSchedule` on the fixed tick, never on wall-clock delta.

Verified in the headless harness: the scripted run (walk, sprint, wall slide, diagonal slide, jump,
settle) reports `peak_y 1.8733`, `final_x`/`final_z` ±9.09, `grounded: true` and
`deterministic: true`.

## Open questions

- Sprint multiplier and strafe penalty are not yet tuning targets; both are currently linear.
- Does camera-relative rotation need a "face the target while attacking" override? Almost certainly
  yes for combat, which makes rotation a state machine rather than a pure function of input.
