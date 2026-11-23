# ADR 011 — Nearest-neighbour budget: spatial hash, K = 8, capped at both ends

- **Status:** Implemented.
- **Decided by:** project owner, second dialogue, turn 8.
- **Verbatim:** *«Только с ближайшими. И ещё. Не стоит ставить борьбу с читерами во главу угла…»*

## Context

Player-vs-player collision in a zone with thousands of bodies is `O(n²)` unless it is cut by
locality. Three separate questions must be answered together, and the dialogue answered only the
first:

1. **How are candidates found?** A uniform spatial hash on the XZ plane, one cell per ~2 m, rebuilt
   every tick. Cheaper and more cache-friendly than a BVH at MMO densities.
2. **How many neighbours are considered?** Not all of them: **the nearest**, up to `K = 8`.
3. **What happens at the limit?** "Nearest 8" without a radius means a lone player in an empty zone
   still considers 8 bodies across the map; and a body that is 9th-nearest is silently
   non-colliding. So the budget needs **both** a radius (`NEIGHBOUR_RADIUS`) and a count (`K`), and
   the pair list must be **deduplicated and capped at both ends** (`MAX_PAIRS_PER_PLAYER`).

## Decision

- Uniform spatial hash, cell size tied to `NEIGHBOUR_RADIUS`, rebuilt per tick.
- Candidate cap `MAX_NEIGHBOURS = 8`, radius `NEIGHBOUR_RADIUS = 2.0` m.
- A pair (`a`, `b`) is included only if `b` is in `a`'s nearest set **or** `a` is in `b`'s; each
  player contributes at most `MAX_PAIRS_PER_PLAYER = 8` pairs, and the union is de-duplicated so a
  symmetric pair is solved once.
- Ordering is deterministic (by distance, then by id) so the solve is reproducible for tests and for
  the client/server agreement.

## Consequences

- A player in the middle of a 40-player stack collides with 8 of them. That is a **design limit**,
  not a bug: it is why soft collision (ADR 010) has to look convincing at low fidelity.
- Asymmetric visibility is possible: A may collide with B while B does not collide with A. The union
  rule makes the behaviour symmetric at solve time, at the cost of one player exceeding its budget.
  Documented deliberately; the alternative (strict per-side caps) produces a solvable but visibly
  asymmetric shove.
- Because the set is rebuilt per tick and depends on positions, a client and a server with slightly
  different positions can select different neighbour sets. This is the *expected* source of most
  corrections, and it is why corrections are velocity-only (ADR 009).

## What this repository does

`crates/sim/src/spatial.rs` — `SpatialGrid` with `insert`/`nearest(pos, k, radius)`; deterministic
tie-breaking by id. `crates/sim/src/collision.rs` builds the pair set with the union rule and the
per-player cap. `MAX_SNAPSHOT_PEERS = 16` is deliberately larger than `K = 8`, so the client can
know about colliders the server is not currently solving and avoid walking into them. The list is
built when `SimConfig::replicate_peers` is on, which is the default now that the client-side solve
exists (ADR 015); the client keeps the replicated sets in a tick-keyed history
(`sim::peers::PeerHistory`).

## Open questions

- Does the AOI radius for *replication* (`MAX_SNAPSHOT_PEERS`) need to scale with player density, or
  is a fixed 16 acceptable in a raid?
- Is 2.0 m the right radius for two 0.4 m-radius capsules? It allows roughly one body of slack; a
  sprinting body at 6 m/s covers 0.1 m per tick, so the slack is ~10 ticks of closure.
