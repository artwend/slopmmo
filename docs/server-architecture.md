# Server architecture

How the decisions in [`decisions/`](decisions/README.md) fit together into one system. Where this
document says "decided", the ADR is the authority; where it says "open", nobody has decided yet.

Read this as the map, not the specification: the repository implements only the boxed-in part at
the bottom (`Simulation node`, movement only).

---

## 1. Layers

```
                        ┌───────────────────────────┐
                        │        Game client        │
                        │  renderer, KCC, prediction│
                        └─────────┬─────────┬───────┘
                    unreliable    │         │    QUIC + TLS 1.3
                    UDP netcode   │         │
                                  ▼         ▼
             ┌────────────────────────┐  ┌─────────────────────────────┐
             │ Game simulation layer  │  │ Social & economy gateway    │
             │ movement, combat, AI,  │  │ chat, mail, trade, auction, │
             │ physics, replication   │  │ guild, friends, inventory   │
             │ (many nodes, sharded)  │  │ (horizontally scaled)       │
             └───────────┬────────────┘  └──────────────┬──────────────┘
                         │ authenticated internal RPC   │
                         ▼                              ▼
             ┌──────────────────────────────────────────────────────────┐
             │ World coordinator   │  Backend services + storage      │
             │ placement, handoff, │  character, inventory, ledger,   │
             │ channel assignment  │  mail, audit log, session tokens │
             └──────────────────────────────────────────────────────────┘
```

Rules that the layering enforces:

- The simulation layer **never** writes inventory, currency or mail (ADR 004).
- The social layer **never** participates in the tick loop; it is latency-tolerant by design.
- Authentication is a shared service: both channels accept the same session token, and neither
  channel is trusted by the other.
- The world coordinator is control plane, not data plane: it decides where a simulation runs, not
  what happens inside one.

---

## 2. World topology

Six nested concepts. Conflating any two of them is the most common way this kind of design rots.

| Concept | Meaning | Lifetime | Owner |
| --- | --- | --- | --- |
| **World** | all servers of one game world, one economy | permanent | backend |
| **Region** | a continent or a battleground group | permanent | world coordinator |
| **Zone** | a map with a boundary (city, dungeon, battlefield) | permanent | content data |
| **Instance** | one running copy of a zone's simulation | minutes–hours | world coordinator |
| **Channel** | players' selectable instance of a static zone (city #3) | minutes–hours | world coordinator |
| **Simulation node** | the process (a GameLift session) hosting one or more instances | hours | fleet |

Two regimes, decided in ADR 002 and ADR 003:

**Static regime — towns, starting areas, dungeons.** Fixed partition, no migration, players choose a
channel. Overload is handled by opening another channel, not by moving geometry. Session change is
explicit and player-visible.

**Dynamic regime — mass PvP.** A weighted Voronoi (power diagram) over player density, recomputed as
load moves. Cells are owned by simulation nodes and **move**; when a cell boundary sweeps past a
player, that player is handed off.

```
   static zone                     dynamic zone
   ┌────┬────┬────┐                ┌───┬─────┬────┐
   │ch 1│ch 2│ch 3│                │ A │  B  │ C  │   ← cells resize with density
   └────┴────┴────┘                └───┴─────┴────┘
     fixed owners                  migrating owners
```

### Handoff

A dynamic cell moving is an **actor migration with seamless handoff**:

1. The source node freezes the actor at a tick boundary and stops emitting its state.
2. Authority state is transferred: shadow position/velocity, `body_yaw`, applied correction id,
   collision modes, the player's tick offset, and the **epoch**.
3. The destination node publishes itself as the owner; snapshots for that player now come from it.
4. The client sees a new epoch and discards corrections from the old one (ADR 007).
5. Only then can the source node drop the actor.

The epoch is the whole trick: without it, a correction in flight from the old node arrives after the
handoff and is applied against the new node's state. With it, stale authority is dropped by
comparison.

**Open:** cross-cell collisions. Two players in adjacent cells that touch each other must be
resolved by *one* node, so the pair set has to include a border halo and a rule for which node owns
a border pair. Nothing in this repository addresses it — the prototype has one cell.

---

## 3. Simulation node

The unit of deployment. One process, N cores, one tick loop.

### 3.1 Tick pipeline (ADR 013)

```
 tick n (fixed 1/60 s)
  1 drain input      map client tick → server tick, drop stale/duplicate  ── O(packets)
  2 movement update  integrate intent, apply reports as shadow base       ── O(players)
  3 spatial rebuild  uniform hash over player/boss colliders              ── O(players)
  4 broadphase       nearest K within radius, pair dedup                  ── O(players·K)
  5 policy           per-pair mode: none / soft / hard                    ── O(pairs)
  6 solver           inelastic capsules, budgeted vector-sum correction   ── O(pairs)
  7 commit           write pose, applied_correction_seq, epoch bumps      ── O(players)
  8 replicate        AOI snapshot + corrections + ack per player          ── O(players·AOI)
```

Invariants:

- Event time is **tick time**. A packet's arrival decides whether it can still apply, never *when*
  it applies.
- No rewind. There is no historical collision state; lag compensation is unbuilt and belongs to the
  combat design (ADR 013).
- Extrapolation is bounded by `REPORT_WINDOW` (30 ticks), then the actor is frozen.
- Everything is deterministic: fixed `dt`, tie-broken iteration order, no wall-clock input.

### 3.2 Collision profiles (ADR 009, ADR 010)

| Class | Client solves | Server solves | Default mode |
| --- | --- | --- | --- |
| Terrain / statics | yes | **no data at all** | hard (client-only) |
| Own player vs terrain | yes | no | — |
| NPCs and normal mobs | yes | no | soft |
| Bosses | yes | yes | hard |
| Players in PvP | yes | yes | soft, hard per ability |
| Players outside PvP | yes | no | soft |

The server's collider set is `players ∪ bosses`, replicated to clients with a mode. There is no
terrain on the server — this is the decision that makes the tick cheap (ADR 012) and the reason
corrections carry velocity, not position.

### 3.3 Pair budget (ADR 011)

`MAX_NEIGHBOURS = 8`, `NEIGHBOUR_RADIUS = 2.0 m`, `MAX_PAIRS_PER_PLAYER = 8` after de-duplication,
deterministic order by distance then id. `MAX_SNAPSHOT_PEERS = 16` on the wire so a client knows
about colliders the server is not currently solving.

### 3.4 Replication (ADR 001, ADR 007)

Hybrid model: **state replication** for what changes every tick, **event messages** for what happens
once.

| Channel | Reliability | Rate | Content |
| --- | --- | --- | --- |
| client → server movement | unreliable, sequenced | 30 Hz, newest + 3 inputs | `PlayerMovementPacket` |
| server → client snapshot | unreliable, sequenced | tick rate (adaptive not built) | tick, ack, shadow pos, correction, peers |
| corrections | inside the snapshot | on change | `CollisionCorrection` with `seq/tick/epoch` |
| discrete events | reliable, ordered | on event | loot, ability cast confirmation, chat-adjacent UI |

Bandwidth, at today's encoding: client ≈ 81 B × 30/s ≈ 2.4 KB/s; server ≈ 28 B + 23 B per peer per
snapshot → 1.5 KB/s with 4 peers at 60 Hz, ~7 KB/s at the 16-peer AOI cap. Both are back-of-envelope
and are the reason AOI exists.

---

## 4. Social & economy layer (ADR 004, ADR 005)

A separate deployment with its own transport and its own data.

| Concern | Home |
| --- | --- |
| Chat channels, whispers | gateway, fan-out by subscription |
| Mail, attachments | gateway + mailbox store, TTL on attachments |
| Trade, auction | gateway + economy service, transactional |
| Guilds, friends, ignore | gateway + social graph store |
| Inventory, currency | **economy ledger only** — the simulation never writes it |
| Identity, session token | auth service, consumed by both channels |

Properties:

- **Idempotency everywhere.** Every mutation carries a client-generated request id; a retry must
  not duplicate an item or a payment.
- **The simulation asks, it does not write.** Loot, currency drops and durability changes are
  requests to the economy service over authenticated internal RPC.
- **Chat is rate-limited and moderated here**, not in the tick loop, so an abuse spike cannot affect
  movement.

Open: the loot reservation race (two players picking one drop) and whether combat consumes items
directly or through a pre-issued token (ADR 004).

---

## 5. Persistence and character lifecycle

```
create → auth → character select → place in zone instance → play → handoff? → logout → persist
```

- **Character record**: stats, position hint, last zone/channel, appearance. Owned by the character
  service.
- **Inventory / currency / mail**: economy ledger, transactional, audited.
- **World state**: mostly ephemeral. A dropped item or a dead boss is simulation state; only the
  outcome is persisted (a loot grant, a kill credit).
- **Save cadence**: on meaningful change (level, quest flag, trade, logout), not per tick. A crash
  loses at most the current transient state, which is why position hints are hints.
- **Login placement** is a coordinator job: pick a zone instance with capacity, tell the client
  where to connect, then admit.

---

## 6. Operations on GameLift (ADR 002)

- A **simulation node is a GameLift game session**; the fleet is the shard pool.
- The world coordinator holds a session placement queue, both for static channels (open channel #4)
  and for dynamic cells (split a hot cell).
- Health is the tick budget: if a node's tick exceeds ~8 ms consistently, its cells are candidates
  for splitting.
- A node is drained by freezing its cells and handing them off, then terminating the session.
- QUIC / TLS endpoints for the social layer live outside GameLift, behind a normal load balancer.

---

## 7. Numbers in one place

| Quantity | Value | Source |
| --- | --- | --- |
| Server tick | 60 Hz (16.67 ms) | `proto::TICK_HZ` |
| Client send rate | 30 Hz | `proto::INPUT_SEND_HZ` |
| Input redundancy | newest + 3 frames | `proto::INPUT_REDUNDANCY` |
| Client packet | ~81 B | `proto::PlayerMovementPacket` |
| Snapshot | 28 B + 23 B per peer | `proto::ServerSnapshot` |
| Peer cap in snapshot | 16 | `proto::MAX_SNAPSHOT_PEERS` |
| Neighbour cap | 8 | `proto::MAX_NEIGHBOURS` |
| Neighbour radius | 2.0 m | `proto::NEIGHBOUR_RADIUS` |
| Report window | 30 ticks (0.5 s) | `proto::REPORT_WINDOW` |
| Correction budget | 2.0 m/s per tick (vector sum) | `proto::CORRECTION_BUDGET` |
| Player capsule | r 0.4 m, h 1.8 m | `proto::PLAYER_RADIUS/HEIGHT` |
| Measured tick cost | 0.042 ms at 32 bots | `server --bots 32` |
| Design target | 10 000 players in one PvP zone | dialogue turn 2 |

The 10 000-player target is **not** demonstrated by these numbers. 0.042 ms at 32 players is
linear-ish in the solver only; the unmeasured parts are replication bandwidth, AOI building and
handoff. Treat the target as a constraint that shapes the design, not as a result.

---

## 8. Implementation status

| Area | Status |
| --- | --- |
| Fixed 60 Hz tick, eight phases | Implemented (phases collapsed) |
| Client KCC + peer collision | Implemented |
| Server shadow + validation | Implemented |
| Spatial hash, K = 8, pair cap | Implemented |
| Correction protocol with `(seq, tick, epoch)` | Implemented |
| UDP transport with admission, timeout, error dedupe | Implemented |
| Per-player clock offset mapping | Implemented |
| Lightyear netcode | **Deviation** — raw UDP (ADR 006) |
| Roles, raids, parties | Absent |
| Combat, abilities, hard-mode switching | Absent |
| NPC / boss simulation | Absent |
| Sharding, channels, handoff, world coordinator | Absent (ADR 002, 003) |
| Social & economy layer | Absent (ADR 004, 005) |
| Auth, persistence | Absent |
| Lag compensation | Absent by decision (ADR 013) |
