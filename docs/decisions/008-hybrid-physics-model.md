# ADR 008 — Hybrid physics: kinematic character controller, geometry solved on the client

- **Status:** Implemented. The client's controller is Avian's `MoveAndSlide` (`crates/kcc-avian`);
  `sim::kcc` remains the engine-free reference solver.
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
- **Not** `AutoMoveAndSlide`: the automatic variant hides the loop and takes over the velocity,
  which conflicts with prediction/rollback because it decides when the movement "ended".
- Dynamic bodies in the world (crates, doors, bosses) are **not** simulated as rigid bodies on the
  server; they are colliders with a mode (ADR 010).
- Rotation, air control and collision response are feel constants, not physics parameters (ADR 014).

## Consequences

- The player never "falls over": no toppling, no accumulated angular momentum. Acceptable for an
  MMO and much cheaper to predict.
- Anything that must react physically (a cart hit by a raid) needs an explicit gameplay rule or an
  entity that is simulated on a single authority, not an emergent solver result.
- Because the character is kinematic, the server can validate **displacement** instead of
  re-simulating physics — the basis of ADR 009.
- Avian is the geometry provider, not the authority: the same `MoveAndSlide` result must be
  reproducible on the client for prediction to work.

## What this repository does

`crates/kcc-avian` ports the `virtual-character` branch's API to released `avian3d` 0.8 instead of
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

The client is an ordinary Avian app: `PhysicsPlugins` + `MoveAndSlidePlugin` run in
`FixedPostUpdate`, the avatar entity *is* the virtual character, and the movement model writes
`LinearVelocity` in `FixedUpdate`. `character::read_back` copies `Position`, `LinearVelocity` and
`Grounded` into `Local` for the report. The report carries that linear velocity alongside the
camera's `angular_velocity`: the server bounds the yaw rate the same way it bounds linear speed and
advances `body_yaw` from the report anchor while dead-reckoning, exactly as it advances position.

`crates/sim/src/kcc.rs` stays as the engine-free reference solver: the deterministic
capsule-vs-AABB collide-and-slide is what `sim`'s own tests and the server's bot benchmark use, so
the cheap test crate never pulls in a physics engine. `crates/sim/src/authority.rs` is unaffected;
the server's shadow still integrates reported motion and never solves terrain.

Gap to close: real trimesh terrain. The prototype's terrain is still a handful of axis-aligned
boxes, so the client builds box colliders; swapping in a trimesh is a change to
`character::terrain_bundle`, not to the controller. Step-up is still absent, as in the branch.

## Open questions

- Which side owns ground snapping tolerance? If the client snaps and the server does not, every
  step is a false correction.
- Do slope limits differ per character (mounts, bosses)? If so, the value belongs to the character
  definition, not to the controller.
