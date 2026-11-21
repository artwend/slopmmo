# ADR 001 — Hybrid protocol: event messages plus selective state replication

- **Status:** Partial — the state plane exists, the event plane does not.
- **Decided by:** project owner, first dialogue, turn 1.
- **Verbatim:** *«Какой тип протокола лучше выбрать для ММОРПГ с 10к+ игроками на сервере —
  вариант как в старой lineage с сообщениями, где каждый айди соответствует какому то действию
  или использовать репликацию?»*

## Context

Two classical options for an MMO:

- **Opcode messages** (Lineage II style): `PacketId` → handler, server pushes semantic events
  (`MoveToLocation`, `AttackAnimation`, `HealthUpdate`). Minimal bandwidth, total control, but
  every client-side consequence of an event has to be re-implemented by hand.
- **State replication**: the server owns components and replicates deltas; the client renders
  state. Simple for movement and crowd scenes, wasteful for discrete gameplay events.

The requirement is 10 000+ players per simulation node, so the answer cannot be "replicate
everything to everyone".

## Decision

Build a **hybrid**: a server-authoritative event/message protocol for discrete gameplay, plus
**selective** state replication for continuous state (position, velocity, animation-relevant
yaw, health for the entities in range).

The governing rule: *replicate only what that specific client must see.* Area-of-interest
filtering is therefore not an optimisation, it is a structural part of the protocol.

## Consequences

- Two planes with different reliability needs:
  - state plane — unreliable, sequenced, high frequency, allowed to drop intermediate values;
  - event plane — reliable-enough, ordered where the order matters, low frequency.
- Protocol versioning becomes mandatory on both planes.
- AOI/interest management is on the critical path for any replication work, not a later polish
  item.
- Combat is event-driven on top of replicated position: an attack is an event, its knockback is
  a state correction.

## What this repository does

`proto` implements the state plane only: `PlayerMovementPacket` (client → server) and
`ServerSnapshot` (server → client, capped at `MAX_SNAPSHOT_PEERS = 16` peers per snapshot, which
is the simplest possible AOI). There is no event plane, no opcode dispatch and no versioning
field yet.

## Open questions

- Does the event plane need its own sequence space, or can it reuse snapshots with an embedded
  event list? (Cheaper: piggyback events on the next snapshot and ack them with the packet's
  `acked_seq`.)
- Where does entity spawn/despawn live — event plane or snapshot diffing?
