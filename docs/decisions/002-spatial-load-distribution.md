# ADR 002 — Static shards for peaceful zones, dynamic Voronoi cells for mass PvP

- **Status:** Open — nothing of the world topology exists in code.
- **Decided by:** project owner, first dialogue, turns 2 and 3.
- **Verbatim:** *«10 000 игроков одновременно в одной PvP-зоне … А как называется технология
  использующаяся в сотовых сетях с handover клиента на другую вышку, но идея в том, что "вышки"
  тоже могут перемещаться.»* — then *«Можно ли использовать диаграмму воронова для
  кластеризации? Предположим что сервера будут находится в amazon Gamelift.»*

## Context

A single PvP zone must hold ~10 000 concurrent players. Fixed grid sharding handles uniform
density; real PvP crowds are extremely non-uniform (one keep, one bridge, one boss). The owner
asked for the cellular-network analogy: coverage cells whose *base stations can move*, i.e.
simulation ownership that follows the crowd instead of the crowd following the map.

## Decision

Two regimes, deliberately different:

| Regime | Where | Ownership |
| --- | --- | --- |
| **Static sharding** | cities, newbie zones, instances | fixed map → fixed simulation node |
| **Dynamic spatial sharding** | mass PvP | weighted Voronoi / power diagram, re-balanced as load moves |

For the dynamic regime: server sites are Voronoi generators in world XZ; a player belongs to the
cell of its nearest site; cell weights come from load (player count, combat intensity, CPU), not
distance alone — hence **weighted dynamic Voronoi**, not a plain nearest-site diagram. Cells may
split when one generator's population explodes and merge when they empty.

Ownership transfer uses the cellular vocabulary on purpose: **actor migration** + **seamless
handoff**, with the client keeping its connection instead of reconnecting.

Hosting target: **Amazon GameLift Servers** — game sessions map to simulation nodes, so node
count is elastic and the fleet is the unit of scaling.

## Consequences

- A world coordinator service is required: it owns the site list, computes cells, decides splits
  and merges, and instructs nodes to hand over entities. This is a piece of infrastructure no
  other part of the design can absorb.
- Handoff needs a transfer protocol: snapshot of the entity (state, pending inputs, collision
  participation), an ownership **epoch**, and a rule for in-flight corrections during the
  transfer. The epoch in `proto::CollisionCorrection` exists for exactly this reason.
- Cross-cell collisions (two players straddling a boundary, melee across a border) need either
  duplicated collision state or a one-tick borrow from the neighbour. Not solved.
- Zone borders for static shards need explicit gates; PvP cell borders must be invisible.

## What this repository does

Nothing. The prototype is a single process with a single `test_room()`; the word "shard"
appears only in this document and the README.

## Open questions

- Do cells overlap by a margin (each node simulates a halo of foreign players) or is collision
  state explicit and shared? A halo is simpler and matches the "nearest neighbours" budget.
- Handoff while a player is mid-collision: transfer the pair, or drop participation for a tick?
- Who is authoritative for a player standing exactly on a Voronoi edge — and for how long, to
  avoid ownership flapping?
