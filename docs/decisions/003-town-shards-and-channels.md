# ADR 003 — Cities and newbie zones are static shards with channels

- **Status:** Open.
- **Decided by:** project owner, first dialogue, turn 4.
- **Verbatim:** *«Нужно что бы шарды для городов и других мирных локаций (нуболок) были
  статичными, возможно даже на эти локации нужно несколько шаров(каналов)»*

## Context

The Voronoi model from ADR 002 is right for a battlefield and wrong for a capital city. A city is
a social space: players expect to meet the same people, services are bound to a place, and
dynamic ownership churn would break trade, mail and NPC state. Cities also overload predictably
at the same spot (the plaza), which is a channel-count problem, not a geometry problem.

## Decision

Cities, starting locations and other peaceful areas are **static shards**:

- fixed map → fixed set of simulation nodes, no dynamic migration and no Voronoi re-balancing;
- a location may run **several channels** (instances of the same map) on separate nodes;
- a player picks a channel or is auto-assigned to a non-full one;
- moving between channels is a **controlled session change** (explicit transfer), never a silent
  handoff.

Dynamic spatial sharding stays confined to mass-PvP zones.

## Consequences

- The world model must distinguish "place" from "session": a location is a logical map, a channel
  is a running simulation of it.
- Channel assignment needs a load signal and a capacity limit per channel; both belong to the
  world coordinator, not to the simulation node.
- Social state must not be per-channel, otherwise friends and guilds fragment. Chat/guild/mail
  live in the separate backend (ADR 004) precisely so channels stay a simulation concern.
- Party/raid members in different channels need an explicit "same channel" rule for grouping,
  duels and trade.

## What this repository does

Nothing; there is one hard-coded room.

## Open questions

- Channel switching cost: full character reload, or a handoff that keeps the connection? Full
  reload is acceptable for cities and much simpler.
- Is the channel list visible to players (classic MMO) or hidden (auto-balancing)? Visible is a
  design decision with social consequences.
- Do world bosses and rare spawns exist per channel (farmable) or per location (contended)?
