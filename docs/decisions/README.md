# Architecture decision records

Every decision below was made **by the project owner** in the three design dialogues
(`6b00b935…`, `6b00b91c…`, `6b00b8fb…`; see [`../dialogue-decisions.md`](../dialogue-decisions.md) for the
verbatim turns). They are recorded here retroactively so the repository has one place that says
what was decided, why, and what it costs.

Status legend:

- **Implemented** — the repository does this today.
- **Partial** — the repository does part of it; the gap is named in the ADR.
- **Open** — decided, not built. Kept here so it is not silently forgotten.
- **Deviation** — the repository knowingly does something else. The ADR says why and what it
  would take to converge.
- **Superseded** — a later decision replaced this one; the ADR body explains what changed.

Statuses reflect the tree after the Lightyear migration (ADR 006). That migration removed the
hand-rolled `net`/`proto` protocol and the `sim` crate, so several ADRs that were implemented in
that path are marked open, partial or deviating again; their decisions still stand as the design
target.

| ADR | Decision | Status |
| --- | --- | --- |
| [001](001-hybrid-protocol.md) | Hybrid protocol: event messages **and** selective state replication | Partial |
| [002](002-spatial-load-distribution.md) | Static shards for peaceful zones, dynamic Voronoi cells for mass PvP, on GameLift | Open |
| [003](003-town-shards-and-channels.md) | Cities and newbie zones are static shards with multiple channels | Open |
| [004](004-social-economy-backend.md) | Trade, mail, chat, guilds, inventory split into a separate secured backend | Open |
| [005](005-transport-split.md) | QUIC + TLS 1.3 for backend traffic, Lightyear UDP for the game channel | Open |
| [006](006-netcode-lightyear.md) | Netcode library: Lightyear plus its built-in transport | Implemented |
| [007](007-movement-packet.md) | One merged `PlayerMovementPacket`, unreliable, 30 Hz send / 60 Hz sim | Superseded |
| [008](008-hybrid-physics-model.md) | Hybrid physics: kinematic character on Avian `MoveAndSlide` | Implemented |
| [009](009-collision-ownership.md) | Client owns terrain collision, server owns player and boss collision | Deviation |
| [010](010-collision-modes.md) | Collision modes `None` / `Soft` / `Hard`, soft by default in PvP | Open |
| [011](011-neighbour-budget.md) | Spatial hash, nearest candidates, at most K = 8 pairs per player | Open |
| [012](012-performance-before-anticheat.md) | Performance first, anti-cheat cheap and best-effort | Open |
| [013](013-server-tick-pipeline.md) | Fixed 60 Hz tick pipeline, solve every tick, no rewind, bounded extrapolation | Partial |
| [014](014-character-feel.md) | Camera-relative rotation, moderate air control, soft character collision | Partial |
| [015](015-traffic-suppression.md) | Superseded state stays off the wire: idle reports and change-gated snapshots | Open |

Supporting documents:

- [`../server-architecture.md`](../server-architecture.md) — how all of the above fits together.
- [`../architecture-review.md`](../architecture-review.md) — the critical review of the dialogues
  and the corrections applied on top of these decisions.
