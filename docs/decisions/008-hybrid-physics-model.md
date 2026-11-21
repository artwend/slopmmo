# ADR 008 — Hybrid physics: kinematic character controller, geometry solved on the client

- **Status:** Partial. The model is implemented in `sim::kcc`; Avian is not integrated yet.
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

`crates/sim/src/kcc.rs` implements a small deterministic capsule-vs-AABB collide-and-slide
(ground snap, slope limit, step-up, bounded slide iterations) and is used by the client for the
local character. `crates/sim/src/authority.rs` uses the same kinematics for the server's shadow
position. Avian itself is not a dependency yet: the prototype's "terrain" is a handful of
axis-aligned boxes (`sim::world::StaticWorld`), so a full trimesh solver would have been untested
weight.

Gap to close: replace `sim::kcc`'s static-solver with Avian's `MoveAndSlide` on real geometry while
keeping the current `Kcc` interface, then confirm the client and the shadow agree tick-for-tick on
flat ground.

## Open questions

- Which side owns ground snapping tolerance? If the client snaps and the server does not, every
  step is a false correction.
- Do slope limits differ per character (mounts, bosses)? If so, the value belongs to the character
  definition, not to the controller.
