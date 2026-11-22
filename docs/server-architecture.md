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
- Extrapolation is bounded by `REPORT_WINDOW` (30 server ticks, 0.5 s), then the actor is frozen.
- A departed player's shadow is parked for `RESUME_WINDOW`; a resume is placed from that shadow and
  its claim is clamped to the travel possible since the last accepted report. Reconnecting is never
  a way to move.
- Travel is measured against the **server's** ticks since the last accepted report, never against
  the client-declared report tick. The report tick decides staleness and ordering only.
- Everything is deterministic: fixed `dt`, tie-broken iteration order, no wall-clock input.
- The steady state **does not allocate** and does not block. See §3.5.

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
deterministic order by distance then id. `MAX_SNAPSHOT_PEERS = 16` is the AOI replication cap, so a
client can learn about colliders the server is not currently solving. It is **not built or sent by
default**: the client has no local peer solve yet, so `SimConfig::replicate_peers` is `false` and the
shipped server skips both the scan and the bytes. The selection logic and its tests are kept for the
consumer that lands with ADR 006's remaining plan.

### 3.4 Replication (ADR 001, ADR 007)

Hybrid model: **state replication** for what changes every tick, **event messages** for what happens
once.

| Channel | Reliability | Rate | Content |
| --- | --- | --- | --- |
| client → server movement | unreliable, sequenced | 30 Hz while active; 4 Hz idle keepalive | `PlayerMovementPacket` |
| server → client snapshot | unreliable, sequenced | on change; adaptive heartbeat (2×RTT, 100 ms–1 s); peers ≤ 30 Hz | tick, ack, shadow pos, correction, peers, `resync` |
| corrections | inside the snapshot | on change | `CollisionCorrection` with `seq/tick/epoch` |
| discrete events | reliable, ordered | on event | loot, ability cast confirmation, chat-adjacent UI |

Bandwidth, at today's encoding: client ≈ 81 B × 30/s ≈ 2.4 KB/s while moving, dropping to ≈ 200 B/s
when idle (4 Hz keepalive with no redundancy frames). Server: ≈ 28 B per changed snapshot with the
peer list off (the shipped default), or ≈ 28 B + 23 B per peer when `SimConfig::replicate_peers` is
enabled — 1.5 KB/s with 4 peers at 60 Hz, ~7 KB/s at the 16-peer AOI cap. Both are back-of-envelope;
the peer list is the reason AOI exists, and it stays dark until a client consumes it.

Neither side streams on a fixed cadence any more (ADR 015). The client stays silent while its input,
pose, grounding and correction acknowledgement are unchanged. The server compares each tick's
snapshot with the last one it sent for that player — at millimetre precision — and only transmits a
change in a correction, `resync` or the peer set. `server_tick`, `acked_seq` and `shadow_position`
do not force a datagram because the client predicts its own pose and only adopts the shadow during
`resync`. A lost final update is repaired by a heartbeat that scales with the link's measured RTT, so
a quiet tick costs nothing and a slow path is not sent redundant datagrams. Peer colliders have a
third, independent cadence: they refresh at `PEER_UPDATE_HZ` (30 Hz) and a correction between
refreshes carries the last list, so corrections stay tick-rate without dragging peers with them.

### 3.5 Data layout and allocation discipline

The eight phases above are one linear pass over contiguous memory, and that is a requirement rather
than an accident (see `architecture-review.md` §7 for the audit that got here).

| Structure | Layout | Why |
| --- | --- | --- |
| Players | dense `Vec<ServerPlayer>` + `HashMap<u64, u32>` slot map | the tick walks everyone in order; ids are only needed on join, leave and ingest. `swap_remove` keeps it dense |
| Spatial grid | CSR: flat `items` ordered by cell + `starts` prefix table + `cursor` scatter buffer | counting sort instead of hashing; no per-cell `Vec`; buffers reused every rebuild |
| Broadphase / solver | `PairScratch` owned by the simulation | four `Vec`s and an `Impulse` array that never reallocate after the first tick |
| Snapshots | `Vec<(u64, ServerSnapshot)>` owned by the simulation, peers inline (`PeerList`) | `ServerSnapshot` is `Copy`, so the transport sends a copy instead of cloning a heap vector |
| AOI peer selection | grid candidates → inline `[(f32, usize); 16]` window per player | the grid culls the scan to nearby cells instead of every body; no per-player `Vec`, no per-player sort |

`crates/sim/tests/allocations.rs` guards this: a counting global allocator asserts that 64
steady-state ticks perform zero allocations. A `vec![]`, a `collect()` or a heap `clone()` added to
the tick fails that test.

Concurrency, stated precisely: the UDP endpoint is non-blocking and lightyear pools its receive
buffers, the I/O layer runs as an async task on Bevy's task pool, and Bevy's executor is
multi-threaded — but the tick itself is single-threaded on purpose. Determinism is an invariant
(ADR 013), and the tick costs ~1.1 ms of a 16.67 ms budget at 256 players. `ingest`, `drive` and
`dispatch` all take `ResMut<GameRes>`, so the executor serialises them by construction; that conflict
is the design, not a missed optimisation.

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
| Server tick | 60 Hz (16.67 ms) | `proto::SERVER_TICK_HZ` |
| Client prediction | 60 Hz (16.7 ms) | `proto::CLIENT_TICK_HZ` |
| Client send rate | 30 Hz | `proto::INPUT_SEND_HZ` |
| Idle client send rate | 4 Hz keepalive, no redundancy frames | `proto::IDLE_SEND_HZ` |
| Snapshot heartbeat | adaptive: 2×RTT, clamped 100 ms–1 s (250 ms until a sample) | `server::transport::heartbeat_interval` |
| Peer refresh rate | 30 Hz, independent of the tick | `proto::PEER_UPDATE_HZ` |
| Client packet | ~81 B | `proto::PlayerMovementPacket` |
| Snapshot | 28 B + 23 B per peer | `proto::ServerSnapshot` |
| Peer cap in snapshot | 16 when enabled; list omitted in the shipped config | `proto::MAX_SNAPSHOT_PEERS`, `SimConfig::replicate_peers` |
| Neighbour cap | 8 | `proto::MAX_NEIGHBOURS` |
| Neighbour radius | 2.0 m | `proto::NEIGHBOUR_RADIUS` |
| Report window | 30 server ticks (0.5 s) | `proto::REPORT_WINDOW` |
| Resume window | 120 s (7200 ticks) | `sim::authority::RESUME_WINDOW` |
| Resume travel allowance | 5.4 m fixed cap; not yet scaled by time since detach | `SimConfig::resume_travel()` |
| Correction budget | 2.0 m/s per tick (vector sum) | `proto::CORRECTION_BUDGET` |
| Player capsule | r 0.4 m, h 1.8 m | `proto::PLAYER_RADIUS/HEIGHT` |
| Measured tick cost | 0.023 ms at 32 bots, 0.436 ms at 256 bots (0.040 / 1.139 with the peer AOI enabled) | `server --bots N --duration 5` |
| Allocation in a steady-state tick | 0 | `crates/sim/tests/allocations.rs` |
| Design target | 10 000 players in one PvP zone | dialogue turn 2 |

The 10 000-player target is **not** demonstrated by these numbers. 0.039 ms at 32 players is
linear-ish in the solver only; the unmeasured parts are replication bandwidth, AOI building and
handoff. Treat the target as a constraint that shapes the design, not as a result. The 32-bot figure
is also dominated by fixed per-tick bookkeeping, which is why it barely moved when the data layout
was rewritten — the 256-bot column is the one that reflects the layout (see
`architecture-review.md` §7.3).

---

## 8. Implementation status

| Area | Status |
| --- | --- |
| Fixed 60 Hz tick, eight phases, solve every tick | Implemented (phases collapsed) |
| Client KCC vs terrain | Implemented (Avian `VirtualCharacter` + `MoveAndSlidePlugin` in `kcc-avian`) |
| Client-side peer-collider solve | Absent — and the wire side is off by default (`SimConfig::replicate_peers`) so nothing is paid for until the solve exists |
| Server shadow + validation | Implemented |
| Spatial hash, K = 8, pair cap | Implemented (CSR counting-sort grid) |
| Zero-allocation steady-state tick | Implemented and guarded by a counting allocator |
| Correction protocol with `(seq, tick, epoch)` | Implemented |
| Correction integration (one tick per delta, in-flight discount) | Absent — the shadow applies a delta across the whole report window (`architecture-review.md` §7.7) |
| Server-sanctioned displacement (respawn, teleport, knockback, mounts) | Absent — only a clamped resume can move a shadow |
| Lightyear UDP transport with admission, timeout, orphan release | Implemented |
| Reconnect/resume clamp: parked shadow, clamped claim, `resync` adoption | Implemented |
| Per-player clock offset mapping | Implemented |
| Lightyear netcode | **Partial** — Lightyear 0.31 owns the wire, prediction/interpolation still ours (ADR 006) |
| Roles, raids, parties | Absent |
| Combat, abilities, hard-mode switching | Absent |
| NPC / boss simulation | Absent |
| Sharding, channels, handoff, world coordinator | Absent (ADR 002, 003) |
| Social & economy layer | Absent (ADR 004, 005) |
| Auth, persistence | Absent |
| Lag compensation | Absent by decision (ADR 013) |
