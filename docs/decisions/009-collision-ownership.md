# ADR 009 — Collision ownership: client owns terrain, server owns players and bosses

- **Status:** Implemented.
- **Decided by:** project owner, second dialogue, turns 4–6.
- **Verbatim:** *«Сервер не должен производить вычисления столкновений со статическими и
  динамическими объектами, это слишком затратно.»* → *«Да у сервера не должно быть данных о
  ландшафте. Только коллизии с другими игроками, нпц и динамическими объектами…»* →
  *«Коррекция. На сервере должны проверяться столкновения с боссами и другими игроками в пвп
  режиме. Остальные столкновения должны обрабатываться на клиенте для эффективности.»*

## Context

Ten thousand players in one zone is the design target (ADR 002). A server that narrow-phases every
player against terrain, props and each other cannot hold that; a server that narrow-phases nobody
cannot resolve player-vs-player at all.

The owner's reasoning, in order:

1. terrain and dynamic-object collision is too expensive to run for every player on the server;
2. so the server must not even **hold** terrain data — it has no landscape;
3. therefore the server's collision set is players, NPCs and dynamic objects;
4. refined: the server verifies collisions with **bosses** and with **other players in PvP mode**;
   everything else is a client-side efficiency concern.

## Decision

| Collider class | Solved on client | Solved on server | Why |
| --- | --- | --- | --- |
| Terrain, static props | yes | **no data** | scale; proven by the client's own capsule solve |
| Own player vs terrain | yes | no (validated indirectly) | the server sees the result, not the mesh |
| NPCs, mobs, non-boss creatures | yes | no | server cost, low exploit value |
| Bosses | yes | **yes** | a boss can pin or block a raid; needs one authority |
| Other players in PvP | yes | **yes** | the only place where cheating wins fights |
| Other players outside PvP | yes | no | cosmetic contact only |

The server therefore holds a **shadow state**: position, velocity, radius, height and mode for
each player, plus boss bodies. It never holds a mesh.

The client reports its post-solve `position`/`velocity`/`grounded`; the server does **not** trust
it blindly, it validates the *implied displacement* against `MAX_SPEED`, `REPORT_WINDOW`, clock
offset and collision modes (ADR 012).

## Consequences

- The server cannot answer "is this position inside a wall". It can only answer "did this player
  move further than a player could". Terrain exploits are therefore **accepted** and handled
  socially (ADR 012), not solved.
- A player standing on a rock is authoritative on the client and approximately right on the
  server. Bots, NPC aggro and boss adds must not depend on exact heights.
- Server-side corrections are **velocity changes and contact normals**, never destinations
  (no `position_delta`), because the server cannot promise where the client's terrain is.
- Boss collisions must be replicated to clients as colliders, so the client solves against the same
  body; otherwise the client walks through the boss and eats a correction every tick (ADR 010).
- Handover between simulation nodes must carry the shadow state plus the epoch (ADR 002).

## What this repository does

- `crates/sim/src/world.rs` — static AABBs exist **client-side and in tests only**;
  `crates/server/src/main.rs` never constructs a `StaticWorld`.
- `crates/sim/src/authority.rs` — the server keeps `ServerPlayer { position, velocity, tick_origin,
  last_seq, last_tick }` and validates the reported displacement, rejecting with
  `Reject::{StaleTick, Teleport, TooFast, DuplicateSeq, UnknownPlayer, MalformedTick}`.
- `crates/sim/src/collision.rs` — capsule-vs-capsule only, with `inv_mass = 0.0` for immovable
  bodies (bosses).
- `crates/proto/src/packet.rs` — the correction shape is deliberately `velocity_delta` + optional
  `hard_normal`; the snapshot's `shadow_position` is explicitly informational and the client must
  not adopt it.

## Open questions

- Does the server need *any* coarse terrain (a heightmap, not a mesh) to stop players flying through
  the world in a way that breaks boss line-of-sight? A 1 m heightmap is cheap; it was excluded by
  the decision, not by cost.
- Are NPCs really out of scope for server collision? Mobs that collide with the player are a feel
  feature players notice immediately.
