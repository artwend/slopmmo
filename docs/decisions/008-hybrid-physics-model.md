# ADR 008 — Hybrid physics: kinematic character controller, geometry solved on the client

- **Status:** Implemented. The character is an Avian `VirtualCharacter` moved by `crates/kcc`
  (`MoveAndSlidePlugin`), and `shared::kcc` remains the engine-free reference solver.
- **Decided by:** project owner, second dialogue, turn 1 and the radio pick.
- **Verbatim:** *«гибридная модель физики»*, then *«да, используем
  `avian/virtual-character/src/character_controller/virtual_character.rs` как основу»*.

## Context

Two pure models were on the table:

| | Full rigid-body physics | Pure kinematic character controller |
| --- | --- | --- |
| Feel | stumble, push, physical momentum | crisp, predictable, MMO-like |
| Determinism across client/server | hard (solver ordering, sleeping, friction) | easy (explicit integration) |
| Cost on the server | high | low |
| Terrain requirement on the server | yes | only if the server solves it |
| Network model | snapshot the body, correct it | snapshot the intent, replay it |

The owner picked **hybrid**: a kinematic character that is moved explicitly, but whose *motion
resolution* (collide-and-slide) comes from a real geometry layer rather than hand-written clamping.
Avian's `virtual_character.rs` was chosen as the reference implementation, with `MoveAndSlide` as
the geometry layer and explicitly **not** `AutoMoveAndSlide`.

## Decision

- Character bodies are **kinematic**: velocity is written by gameplay code, positions integrate
  explicitly, nothing pushes the player via impulses except our own inelastic contact solve.
- The **geometry layer** is `MoveAndSlide`-style: move, gather contacts, slide along the remaining
  tangential velocity, repeat for a bounded number of iterations. This is reused as-is; it is the
  part that is boring to write and easy to get wrong (internal edges, ground snapping, slopes).
- The entity-side API is the released `VirtualCharacter` plus the `AutoMoveAndSlide` settings
  component it requires, with `MoveAndSlidePlugin` running the loop in `PhysicsSchedule`. The
  dialogue's plan to avoid `AutoMoveAndSlide` did not survive the port onto released Avian.
- Dynamic bodies in the world (crates, doors, bosses) are **not** simulated as rigid bodies on the
  server; they are colliders with a mode (ADR 010).
- Rotation, air control and collision response are feel constants, not physics parameters (ADR 014).

## Consequences

- The player never "falls over": no toppling, no accumulated angular momentum. Acceptable for an
  MMO and much cheaper to predict.
- Anything that must react physically (a cart hit by a raid) needs an explicit gameplay rule or an
  entity that is simulated on a single authority, not an emergent solver result.
- Because the character is kinematic, a server *could* validate **displacement** instead of
  re-simulating physics — the basis of ADR 009, which this prototype does not build.
- Avian is the geometry provider, not the authority: the same `MoveAndSlide` result must be
  reproducible on the client for prediction to work.

## What this repository does

`crates/kcc` ports the `virtual-character` branch's API to released `avian3d` 0.8 instead of
forking the branch:

- `VirtualCharacter` + `AutoMoveAndSlide` are the entity-side API; `MoveAndSlidePlugin` runs the
  step in `PhysicsSchedule`, after the broad phase (so the collider trees are current on the first
  tick).
- `UpDirection`, `Grounded`, `SlopeHandling`, `GroundSnapping` and `StairStepping` carry the
  branch's fields and defaults. `Grounded` and `GroundSnapping` are applied because the movement
  model reads them every tick; `slide_on_ground`, `slide_on_ceiling` and `StairStepping` are carried
  for API parity but not applied yet.
- `AutoMoveAndSlide::on_hit` keeps the branch's `fn(MoveAndSlideHitData) -> bool` signature and maps
  `true`/`false` onto Avian 0.8's `Accept`/`Ignore`.

Both binaries install the same pipeline through `shared::SharedPlugin`: `PhysicsPlugins` (with the
transform-sync, interpolation, island and sleeping plugins disabled because they break rollback),
`MoveAndSlidePlugin`, and a `FixedUpdate` system that turns the BEI action values into
`LinearVelocity` with `shared::movement::step_velocity_raw`. `LightyearAvianPlugin` registers
`Position`, `Rotation`, `LinearVelocity` and `Grounded` for replication, prediction, interpolation
and correction, with `register_physics_components: false` because the character is not a rigid body.

`shared::kcc` stays as the engine-free reference solver: the deterministic capsule-vs-AABB
collide-and-slide is what the `kcc` parity test compares against, so the solver can be checked
without a physics engine.

Gap to close: real trimesh terrain. The prototype's terrain is still a handful of axis-aligned
boxes (`shared::test_room`), so both sides build box colliders; swapping in a trimesh is a change to
`shared::terrain_bundle`, not to the controller. Step-up is still absent, as in the branch.

## Open questions

- Which side owns ground snapping tolerance? If the client snaps and the server does not, every
  step is a false correction.
- Do slope limits differ per character (mounts, bosses)? If so, the value belongs to the character
  definition, not to the controller.
