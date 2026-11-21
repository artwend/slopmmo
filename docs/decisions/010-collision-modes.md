# ADR 010 — Collision modes `None` / `Soft` / `Hard`, soft by default in PvP

- **Status:** Implemented.
- **Decided by:** project owner, second dialogue, turn 7 (chose "hybrid" among collision models).
- **Verbatim:** *«Hybrid»* in response to the choice between no player collision, soft collision and
  hard blocking; with *«Character collision: soft»* as the default from turn 3.

## Context

Three models were on the table:

- **No collision** — players pass through each other. Cheapest, and in a 200-player raid it looks
  acceptable. But melee range loses all meaning and stacking becomes the dominant tactic.
- **Soft collision** — bodies push each other apart but may interpenetrate briefly. Cheap, stable at
  high density, forgiving under prediction error.
- **Hard collision** — bodies block movement outright, like walls. Necessary for a shield wall or a
  boss body, but expensive: any correction becomes a position correction, and latency makes it feel
  sticky.

## Decision

**All three exist per collider**, not per game:

- `Mode::None` — no pair is generated; the client and the server both skip it.
- `Mode::Soft` — the default for player-vs-player, including in PvP. Inelastic impulse cancels
  relative normal velocity, plus a bounded positional push-out with a `SOFT_MARGIN` gap.
- `Mode::Hard` — opt-in per ability or entity (shield wall, boss body, a "grab"), blocking movement
  and emitting a hard contact normal so the client can slide.

The mode is **replicated with the collider** (`PeerCollider::mode`), because the client must solve
against the same mode the server uses; otherwise every hard contact disagrees.

## Consequences

- An ability can change the mode for a duration, which means the mode is not a static component:
  it needs an owner, a duration and a reset path that is itself replicated.
- Soft impulses must cancel **relative** velocity, not add a fixed push. A fixed 1 m/s push cannot
  keep up with a 3.5 m/s walk, so bodies stay interpenetrated forever (this was a real bug in the
  dialogue's formulation; see `../architecture-review.md`).
- The per-player correction budget applies to the **vector sum** of impulses, not per pair —
  otherwise eight neighbours add up to eight times the limit.
- Hard contacts are the only case where the server sends a direction the client must obey, so they
  are the only place where a cheat can be detected reliably.

## What this repository does

- `crates/sim/src/collision.rs` — `Mode` enum, inelastic impulse
  `j = -(v_rel·n) / (1/m_a + 1/m_b)`, `CORRECTION_BUDGET = 2.0` applied to the summed correction,
  `SOFT_MARGIN = 0.05`, `inv_mass = 0` for immovable bodies.
- `crates/proto/src/packet.rs` — `peer_mode::{NONE, SOFT, HARD}` travels in every `PeerCollider`.
- `crates/server/src/main.rs` — PvP peers are soft; a `HARD` mode exists in the solver and in the
  wire format but no gameplay ability switches it on yet.
- Tests cover soft push-out, budget clamping and hard-blocking normal selection.

## Open questions

- Should soft collision be disabled entirely above a player-density threshold (raid stacks), or is
  the budget enough to keep it stable?
- Does a hard contact need to also stop the client's input for that tick ("you cannot walk into the
  shield"), or is refusing to resolve the slide enough?
