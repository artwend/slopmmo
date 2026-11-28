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
2. Authority state is transferred: shadow position and linear/angular velocity, `body_yaw`, applied
   correction id, collision modes, the player's tick offset, and the **epoch**.
3. The destination node publishes itself as the owner; snapshots for that player now come from it.
4. The client sees a new epoch and discards state from the old one (the epoch concept is ADR 002's;
   the correction channel that used it was removed with the custom protocol).
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
  3 spatial rebuild  counting-sort rebuild of the uniform grid, buffers reused ── O(players)
  4 broadphase       nearest K within radius, pair dedup                  ── O(players·K)
  5 policy           per-pair mode: none / soft / hard                    ── O(pairs)
  6 solver           inelastic capsules, budgeted vector-sum correction   ── O(pairs)
  7 commit           write pose, applied_correction_seq, epoch bumps      ── O(players)
  8 replicate        AOI snapshot + corrections + ack per player          ── O(players·AOI)
```

Invariants:

- **One rate.** The server ticks at 60 Hz and every phase above runs on every tick, including both
  collision modes. Nothing is sub-sampled. The client's KCC predicts at the same 60 Hz; the server
  still scales the client's tick domain onto its own rather than assuming the rates are equal.
- Event time is **tick time**. A packet's arrival decides whether it can still apply, never *when*
  it applies.
- No rewind. There is no historical collision state; lag compensation is unbuilt and belongs to the
  combat design (ADR 013).
- Extrapolation is bounded by a report window of 30 server ticks (0.5 s), then the actor is frozen.
- A departed player's shadow is parked for a fixed resume window; a resume is placed from that shadow and
  its claim is clamped to the travel possible since the last accepted report. Reconnecting is never
  a way to move.
- Travel is measured against the **server's** ticks since the last accepted report, never against
  the client-declared report tick. The report tick decides staleness and ordering only. That
  elapsed time is capped at that window: a client that goes quiet cannot bank the silence as
  travel, and a report past the cap that overshoots is clamped to the cap and answered with
  `resync` instead of being rejected.
- Everything is deterministic: fixed `dt`, tie-broken iteration order, no wall-clock input.
- The steady state **does not allocate** and does not block. See §3.5.

**Current implementation.** The pipeline above is the target, not the code. The server runs
Lightyear's `ServerPlugins` on Bevy's fixed clock: Lightyear delivers inputs,
`shared::apply_player_input` writes `LinearVelocity`, `MoveAndSlidePlugin` moves the capsule, and
the resulting components replicate. There is no spatial index, broadphase, capsule solve, snapshot
build, report window or allocation guard.

### 3.2 Collision profiles (ADR 009, ADR 010)

| Class | Client solves | Server solves | Default mode |
| --- | --- | --- | --- |
| Terrain / statics | yes | **no data at all** | hard (client-only) |
| Own player vs terrain | yes | no | — |
| NPCs and normal mobs | yes | no | soft |
| Bosses | yes | yes | hard |
| Players in PvP | yes | yes | soft, hard per ability |
| Players outside PvP | yes | no | soft |

**Current implementation** deviates from the table: the server spawns the same
`shared::test_room()` colliders as the client, and both sides run the same `VirtualCharacter`
controller. Only terrain collision exists; there is no player-vs-player or boss collision, no mode
and no replicated collider list. The table is the target (ADR 009, ADR 010).

### 3.3 Pair budget (ADR 011)

**Not implemented.** `MAX_NEIGHBOURS = 8`, `NEIGHBOUR_RADIUS = 2.0 m` and
`MAX_PAIRS_PER_PLAYER = 8` are the target (ADR 011). The prototype has no spatial index, no pair
list and no AOI: every player replicates to every client.

### 3.4 Replication (ADR 001, ADR 006)

**Current implementation** is plain Lightyear replication; the hybrid target is not built.

| Direction | Mechanism | Cadence |
| --- | --- | --- |
| client → server | Lightyear input messages carrying the `Movement`/`Jump`/`Sprint` action values for the `Player` context; rebroadcast to every client | per client tick, buffered by `InputTimelineConfig::balanced()` |
| server → clients | Lightyear component replication: `Position`, `Rotation`, `LinearVelocity`, `Grounded`, `PlayerId` | `shared::SEND_INTERVAL` (100 ms), changed components only |
| own player | predicted and rolled back by Lightyear | per tick |
| other players | interpolated by Lightyear | per tick |
| discrete events | none yet (ADR 001 event plane) | — |

There is no AOI. `Replicate::to_clients(NetworkTarget::All)` sends every player to every client,
which is acceptable only because the prototype is one small room. There is no hand-written
snapshot, ack, correction message or peer list; reliability and change detection are the library's.

**Target.** Hybrid model: **state replication** for what changes every tick, **event messages** for
what happens once, with interest management deciding what each client may see. When the event plane
is added it belongs on reliable, ordered Lightyear channels. Bandwidth budgets in this document are
open design work, not measured behaviour of the current tree.

### 3.5 Data layout and allocation discipline

**Not implemented.** The custom server's dense player storage, CSR spatial grid, reusable solver
scratch, inline snapshot peer list and allocation guard were all removed with `crates/sim`. The
current tick is Bevy's physics schedule plus Lightyear's replication, and no allocation or
cache-layout work has been done on it. The audit and its measurements are kept in
[`architecture-review.md`](architecture-review.md) §7 as a record of the removed design.

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
| Server tick | 60 Hz (16.67 ms) | `shared::TICK_HZ`, `shared::TICK_DURATION` |
| Client simulation | 60 Hz, same fixed step | `shared::TICK_HZ` (`Time::<Fixed>`) |
| Replication send interval | 100 ms | `shared::SEND_INTERVAL` → `ReplicationMetadata` |
| Player capsule | r 0.4 m, h 1.8 m, resting centre 0.9 m | `shared::PLAYER_RADIUS`, `PLAYER_HEIGHT`, `RESTING_HEIGHT` |
| Walk / sprint speed | 6.0 / 9.0 m/s | `shared::MAX_SPEED`, `MoveParams::sprint_speed` |
| Ground / air acceleration | 24 / 6 m/s² | `shared::MAX_ACCEL`, `MoveParams` |
| Ground friction | 14 m/s² (linear) | `MoveParams::ground_friction` |
| Gravity / jump | −24 m/s², 7 m/s | `MoveParams` |
| Headless script | 1201 ticks; final pose ±9.09 m; `peak_y` 1.8733; deterministic | `cargo run -p client -- --headless` |
| Design target | 10 000 players in one PvP zone | dialogue turn 2 |

The 10 000-player target is **not** demonstrated by the prototype. There is no AOI, no server-side
collision solve and no load-test harness, so the numbers that would support it (bandwidth per
player, solve cost per pair, handoff cost) are unmeasured. Treat the target as a constraint that
shapes the design, not as a result.

The legacy wire constants (`SERVER_TICK_HZ`, `INPUT_SEND_HZ`, `MAX_SNAPSHOT_PEERS`,
`REPORT_WINDOW`, `CORRECTION_BUDGET`, the packet sizes and the bot benchmarks) were removed with the
`proto`/`sim` crates. Their historical values remain in ADRs 007–015,
[`architecture-review.md`](architecture-review.md) §7, and the git history before commit `4af32ef`.

---

## 8. Implementation status

| Area | Status |
| --- | --- |
| Fixed 60 Hz tick | Implemented — `shared::TICK_HZ` drives Bevy's fixed clock and both Lightyear plugin groups |
| Authoritative server simulation | Implemented — Lightyear `ServerPlugins`; one player spawned per connected client |
| Client KCC vs terrain | Implemented — Avian `VirtualCharacter` + `MoveAndSlidePlugin` (`kcc` crate) |
| Server vs terrain | Implemented — same `shared::test_room()` colliders on both sides (deviation from ADR 009) |
| Prediction / rollback / interpolation | Implemented — Lightyear component rules for `Position`, `Rotation`, `LinearVelocity`, `Grounded` |
| Input pipeline | Implemented — BEI actions, server-owned action entities, rebroadcast inputs, owner prediction |
| UDP transport | Implemented — Lightyear raw UDP (`ServerUdpIo` + `RawServer`; `RawClient` + `UdpIo`) |
| Headless CI client | Implemented — scripted run with determinism and invariant checks |
| Interest management / AOI | Absent — all players replicate to all clients |
| Peer / boss collision, collision modes | Absent (ADR 009, 010, 011) |
| Server shadow, displacement validation, resume | Absent — removed with the custom protocol (ADR 012) |
| Traffic suppression, heartbeats, corrections | Absent — Lightyear's replication cadence only (ADR 015) |
| Roles, raids, parties | Absent |
| Combat, abilities, hard-mode switching | Absent |
| NPC / boss simulation | Absent |
| Sharding, channels, handoff, world coordinator | Absent (ADR 002, 003) |
| Social & economy layer | Absent (ADR 004, 005) |
| Auth, persistence | Absent |
| Lag compensation | Absent by decision (ADR 013) |
