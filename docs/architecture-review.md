# MMORPG Movement Architecture — Dialogue Review

Summary and critical review of the design discussion (hybrid movement protocol, KCC, and
authority split), plus the decisions this repository is built on.

Status: **working notes**, not a final specification. Numbers marked `TBD` need measurement.

This document is the *review*: it records where the dialogue was wrong and what replaced it. The
decisions themselves — as the project owner made them, with verbatim quotes — are in
[`decisions/`](decisions/README.md); the assembled system is in
[`server-architecture.md`](server-architecture.md). Where the two disagree, the ADRs win.

---

## 1. What the design settled on

| Area | Decision |
| --- | --- |
| Replication | Snapshot state replication + message protocol hybrid |
| Transport | UDP, unreliable + sequenced where applicable, Protobuf-like binary |
| Engine | Bevy 0.20, fixed 60 Hz server tick |
| KCC | Custom deterministic core + Avian `MoveAndSlide` for terrain on the client |
| Terrain collision | **Client only.** Server holds no landscape data at all |
| Entity collision | **Server only** (players in PvP, bosses), custom capsule solver, no Avian |
| Anti-cheat | Deliberately deprioritised. Cheap checks only; "don't make cheating trivial" |
| Collision profiles | soft by default, hard for specific abilities (Hybrid) |
| Neighbour limit | nearest **K** players, K = 8 initially |

The resulting model is asymmetric by construction:

```
Client                                          Server
------                                          ------
input (60 Hz)                ──────────────▶    input buffer
KCC vs terrain (Avian)                          shadow kinematics
                                                spatial hash → pairs (K=8)
local separation vs peers    ◀──────────────    soft/hard solve
apply correction, re-resolve                    aggregate corrections
report final position        ──────────────▶    shadow state update
```

The client's position is authoritative for terrain. The server's position is a *shadow* — it
must never be described as authoritative.

---

## 2. Errors and contradictions found in the dialogue

### 2.1 Frequencies do not add up

The tick table lists `Server movement state 30–60 Hz`, `Hard Collision 60 Hz`,
`Soft Collision 20–30 Hz`. A subsystem cannot run faster than the tick that drives it.

Resolution: **solve collisions every tick.** Buy CPU back elsewhere — incremental spatial
hash updates (dirty cells), not a lower solve rate. A 20–30 Hz soft solve applied to a 60 Hz
KCC is visible as stepped pushes every 33–50 ms.

Adopted: the server tick is **60 Hz** (a 30 Hz tick was evaluated and set back) and every collision
mode is solved on every server tick. The client's KCC runs at the same rate; the wire format still
maps client ticks onto server ticks by ratio rather than assuming equality. See
[ADR 013](decisions/013-server-tick-pipeline.md).

### 2.2 `position_delta` was excluded, then reintroduced

The short branch correctly removed `position_delta` from `EntityCollisionCorrection` because
the server cannot guarantee the offset does not land inside a wall. The long branch then put
it back into the same struct.

A positional delta computed from a shadow state **accumulates drift**: the client is already
somewhere else, so every delta injects fresh error. Position corrections must be forbidden;
only velocity/impulse corrections are admissible.

### 2.3 "The client predicts hard collision" guarantees mispredictions

Hard collision is *asymmetric* (Shield Wall: blocker vs blocked) and depends on other players'
positions, which the client only interpolates. Predicting it means predicting wrong exactly
when it matters — a dash through a shield.

Resolution: remote players either do not block local prediction at all, or block it without
being part of the rollback set.

### 2.4 "Authoritative movement state" is a fiction

The server has no authoritative position — it receives one. `ConfirmedHistory`-style
reconciliation therefore does not apply in its standard form, and the whole machine degrades
to "local simulation + injected impulses". This has to be an explicit written decision, and
the server struct should be named *shadow*, never *authoritative*.

### 2.5 The serious one: predicted motion depends on non-rewound state

The client collides locally with NPCs and dynamic objects, but those entities are **not** in
the rollback history, and Avian does not rewind its spatial structure. Every replay after a
correction therefore produces a different result → permanent rubber-banding in crowds.

Resolution: predicted motion may depend **only** on static geometry plus server-supplied
colliders that are themselves history-tracked. NPCs and decoration are non-blocking for
prediction.

### 2.6 Soft collision numbers are wrong

`penetration * w`, clamped to an absolute **1.0 m/s**, versus a 3.5 m/s walk speed: separation
always lags approach, so players permanently interpenetrate. And the 1 m/s cap is *per pair*,
so eight neighbours can sum to 8 m/s.

Resolution: clamp the **relative normal velocity** instead (inelastic contact):

```
j = -(1 + e) * (v_rel · n) / (1/m_a + 1/m_b)
```

This fixes penetration and jitter in one step. Add a per-player *vector* impulse budget and a
dead zone around the contact distance.

### 2.7 Other concrete defects

- Capsule distance computed on X/Z only → players stacked on different floors push each other.
  Needs a Y overlap test.
- "Nearest 8" without a radius bound: in an empty field that means contacts at 50 m.
  Both constraints are required.
- `Unreliable Sequenced` for input is the wrong channel: a late packet is dropped whole,
  including its redundancy frames. Use plain `Unreliable` + per-tick dedupe in a buffer.
- Speed validation reads the self-declared velocity field, which is trivially forged.
  Validate the **position delta** (`|Δpos| ≤ v_max · Δt · k`, and `Δpos ≈ v · Δt`).

---

## 3. What was never considered

1. **Boss/player vs terrain.** Server pushes → client re-resolves against terrain → client
   ends up inside the boss → server pushes again. Stable oscillation in wall corners.
2. **Zone/shard handover** was designed (Voronoi, cellular handover) but never connected to
   the physics model. Crossing a boundary resets the collision set discontinuously.
3. **Idempotency of corrections.** An impulse must live in history keyed by tick and be
   reproduced exactly once during replay, not sit in a "current impulse" field.
4. **No arithmetic**, despite "performance first". 10k players × 8 pairs × 30 Hz ≈ 2.4M pair
   evaluations/s plus 300k grid inserts/s. Feasible in a few ms/core — but it must be measured.
5. **Tick mapping** between a 60 Hz client and a 30 Hz server: two client ticks fall into one
   server tick. Which server tick does a correction attach to? *Resolved for the mapping itself:
   corrections are stamped with the server tick, and `SimConfig::map_client_tick` scales the
   client's tick domain onto the server's. Combat still has to decide which tick a hit resolves
   on.*
6. **Combat lag compensation** reuses server entity history — the dialogue says combat is a
   separate system but never says where that history lives.
7. **Bevy/Avian API risk.** `virtual-character` is a moving branch, not released Avian.
   Lightyear 0.30 `PredictionHistory` / `ConfirmedHistory` / `VisualCorrection` were asserted
   without verification against the docs.

---

## 4. Two changes this repository is built on

### 4.1 Invert the roles: the server owns collider state, not motion

The root cause of most problems above is that the server *synthesises motion* from a state it
cannot know. Instead:

- Players in PvP and bosses are replicated to the client as **kinematic colliders**.
- The client feeds them into its **own** solver, alongside terrain — one solver, one answer.
- The server's job is **validation and collider state**: did the reported trajectory actually
  touch the other capsule, and was it pushed through?

Consequences: no `EntityCollisionCorrection` carrying impulses, no idempotency problem, no
"correction vs terrain" conflict, and prediction stays reproducible because the collider set
is history-tracked too. The server's correction channel becomes an *exception* path, not the
normal path.

### 4.2 A cheap terrain oracle instead of no terrain

A coarse walkability grid (1–2 m cells per zone, a few KB) is not collision. It is enough to
reject "inside a wall" and teleports almost for free. This is exactly the stated compromise —
do not make cheating trivial, do not pay for a physics engine — and it was missing.

---

## 5. Open questions to resolve before feature work

- ~~Server tick: 60 Hz everywhere, or 30 Hz with entity collision sub-sampled? Measure first.~~
  Decided: 60 Hz server tick with every collision mode solved on it; a 30 Hz tick was evaluated
  and set back ([ADR 013](decisions/013-server-tick-pipeline.md)).
- Is the client's peer-collider set history-tracked from day one, or after the first prototype?
- Walkability grid resolution and how it is authored.
- Correction budget: max total impulse per player per tick, and the give-up/teleport threshold.
- Handover protocol for collision state across zone boundaries.

---

## 6. First milestone (this repository)

A running skeleton that answers the cheapest question first:

- shared wire protocol (`proto`) — tick-keyed input frames with redundancy, merged
  `PlayerMovementPacket`, aggregated server corrections with idempotency keys, peer colliders;
- pure simulation crate (`sim`) — kinematic model, spatial hash, soft/hard capsule solve,
  position validation, all unit-tested without Bevy;
- headless Bevy server (`server`) — fixed tick, UDP, N bot clients for load measurement;
- Bevy client (`client`) — windowed, or `--headless` to run scripted ticks in CI.

Everything that can be decided from numbers (tick rate, K, solver iterations, bandwidth per
player) is decided here, before any Lightyear integration.

---

## 7. Code review: the hot path (performance pass)

The second review asked for one thing: *"ensure that the server is a multithreaded, asynchronous,
non-blocking, data-oriented and cache-friendly application with no unnecessary allocations in
critical sections."* This section is what that audit found and what changed. Nothing here
contradicts an ADR; it makes the ADRs cheap enough to keep.

### 7.1 What was wrong

The tick was already small, but it was small in wall-clock terms only while the player count was
small. Every one of the following was on the per-tick path.

| # | Finding | Where | Cost |
| --- | --- | --- | --- |
| 1 | Four `Vec`s allocated per tick in the broadphase | `collision::build_pairs` | 4 allocations per tick |
| 2 | One `Vec<Impulse>` sized to the whole body list, per tick | `collision::solve` | 1 allocation + full write pass |
| 3 | A `Vec<(f32, usize)>` collected **per player per tick** for AOI peers, then sorted | `Simulation::build_snapshots` | 1 allocation + a sort per player per tick |
| 4 | A fresh `Vec<ServerSnapshot>` returned from `step`, and a second `Vec<u64>` of ids collected to zip it back up | `Simulation::step`, `Game::tick_once` | 2 allocations per tick |
| 5 | `BTreeMap<u64, ServerPlayer>` player storage | `Simulation::players` | O(log n) lookups, one node per player, pointer chasing on the one loop that touches everyone |
| 6 | `HashMap<(i32, i32), Vec<u32>>` spatial grid | `spatial::SpatialGrid` | a hash per body per tick, a heap `Vec` per occupied cell, churn on every clear |
| 7 | `snapshot.clone()` per player per tick | `transport::dispatch` | a heap clone of the peer list per send |
| 8 | `barrier_ok` recomputed a distance the caller had just computed | `Simulation::build_snapshots` | a second `sqrt` per body pair per player |
| 9 | `nearest.sort_by(..)` over every candidate before truncating to K | `collision::build_pairs` | O(c log c) where O(c · K) suffices |
| 10 | `std::thread::sleep(500 µs)` **inside a Bevy system** | `server::drive` | a blocked worker thread, and frame pacing hidden in game logic |
| 11 | `--help` advertised `0.0.0.0:4000` as the default listen address; the default is `127.0.0.1:4000` | `server::Args::parse` | a wrong doc, not a bug — but it misleads the operator |

Finding 10 is the one that mattered most to the *stated* requirement. Sleeping inside a system
blocks a task-pool thread that the executor wanted back, and it makes the frame loop's cadence a
property of game code. Bevy's schedule runner is the component that owns frame pacing; it now gets
to do its job (`ScheduleRunnerPlugin::run_loop(1 ms)`), and `drive` either does work or returns.

### 7.2 What replaced it

**Dense player storage** (`sim/src/authority.rs`). Players are a `Vec<ServerPlayer>` addressed by
slot, plus a `HashMap<u64, u32>` for the two places that only know an id (admission and removal).
Removal is `swap_remove` with the moved body's slot repaired. The tick is now one linear pass over
a contiguous array: no tree walk, no allocator traffic, and the shadow state of players that are
near each other — the ones the solver actually compares — lands in adjacent cache lines.

**CSR grid with reused buffers** (`sim/src/spatial.rs`). The uniform grid is a counting sort into
one flat `items` array plus a `starts` prefix table and a `cursor` scatter buffer, all allocated
once and reused by every rebuild. No hashing, no per-cell `Vec`, candidates arrive in cell order
(contiguous memory). The window is clamped to 128×128 cells, so one absurd coordinate cannot blow up
memory; bodies outside the window are clamped into the border cell and the caller's exact distance
test rejects them.

**Reusable physics scratch** (`sim/src/collision.rs`). `PairScratch` owns every buffer the pair
build and the solve need; `solve_into` writes into a caller-provided `Vec<Impulse>`. Nearest-neighbour
selection is a bounded insertion (`partition_point` + shift, at most K entries) instead of a sort of
everything followed by a truncate. The allocating free functions `build_pairs` and `solve` remain as
thin wrappers for the tests and one-shot analysis.

**Snapshots owned by the simulation** (`Simulation::snapshots()`), and an inline peer list.
`proto::PeerList` is a fixed-capacity array with hand-written compact serde, byte-identical on the
wire to the `Vec` it replaced, which makes `ServerSnapshot: Copy` — so the transport sends a copy
instead of cloning a heap vector, and the snapshot build never allocates. Peer selection is one
distance per candidate into an inline `[(f32, usize); 16]` window ordered by `(distance, body)`,
which reproduces the old stable-sort semantics exactly, and the redundant `barrier_ok` distance is
gone.

**Frame loop** (`server/src/main.rs`). `Game` no longer mirrors the tick or the snapshots; the
simulation owns both, and `tick_once` is just `step`. Pacing moved to the schedule runner. The
`--listen` help text now states the real default.

### 7.3 Result

Same binary, same seed, same workload, before and after (`--bots N --duration 3`, release):

| | 32 bots before | 32 bots after | 256 bots before | 256 bots after |
| --- | --- | --- | --- | --- |
| `ms_per_tick` | 0.038 | **0.039** | 1.963 | **0.993** |
| `solve_us_per_tick` | 31.4 | **27.5** | 1909.0 | **904.5** |
| `pairs_last_tick` | 85 | 85 | 876 | 876 |
| `corrections` | 5293 | 5293 | 45 929 | 45 929 |
| `headroom_x` | 435.8 | 427.5 | 8.5 | **16.8** |

The identical `pairs` and `corrections` counts are the point: this was a data-layout change, not a
behaviour change. At 32 bots the tick was dominated by fixed per-tick bookkeeping, so it lands within
noise; at 256 bots — where the per-player allocations and the tree walks actually bite — the tick
halved, and the remaining 904 µs is the solver doing real work, not overhead.

### 7.4 The allocation guard

`crates/sim/tests/allocations.rs` installs a counting `GlobalAlloc` and asserts that **64 consecutive
steady-state ticks allocate zero times** with 16 players in eight touching pairs. The fixture is built
so that bodies never move (pairs exist, penetration is zero), which removes grid-window growth from the
measurement. The test also asserts the warm-up *did* allocate, so a dead counter cannot pass.

This is a regression test, not a benchmark: it fails the moment someone adds a `vec![]`, a `collect()`
or a `clone()` of a heap value to `Simulation::step`.

### 7.5 Multithreading and asynchrony: what is true, and what is not

Claims that are true:

- The UDP endpoint is **non-blocking** (`set_nonblocking(true)` on the socket); no system ever waits
  on the network.
- Receive buffers are **pooled** by lightyear; a steady-state tick does not allocate for I/O.
- The I/O layer runs as an **async** task on the Bevy task pool (`multi_threaded` + `async_executor`
  features are enabled), so socket work is not on the tick thread.
- The process is **multi-threaded**: Bevy's multi-threaded executor distributes systems across the
  task pool.

Claim that would be false, and is therefore not made: **the authoritative tick itself is
single-threaded, deliberately.** Determinism is a stated requirement (ADR 013: fixed `dt`, tie-broken
iteration order), and the tick now costs 1.11 ms of a 16.67 ms budget at 256 players. Note also that
`ingest`, `drive` and `dispatch` all take `ResMut<GameRes>`, so the executor serialises them by
construction — that conflict *is* the design, not an oversight.

Where parallelism goes next, in the order it should be tried:

1. **Snapshot build per player** — embarrassingly parallel once each worker owns its own peer window.
   Cheap to verify: `ServerSnapshot` order is already deterministic.
2. **Broadphase per cell** — split the cell range across workers; the counting sort gives a natural
   boundary and the pair list can be merged in cell order.
3. **Solve per body range** — bin pairs by body, solve bins in parallel, then apply. This is where the
   time actually is (~90 % of the tick at 256 players), and it is the only one with a real design cost:
   sequential impulses across a bin boundary are not the same as a global pass, so it changes results
   and needs its own determinism test.

The incremental spatial update that §2.1 suggested as the way to "buy CPU back" turned out not to be
first: the grid is a small fraction of the tick, and a counting sort over a reused buffer is already
cheap. It stays on the list for when the player count makes the rebuild visible again.

### 7.6 Known remaining allocation/blocking sites

Honest list, so the next review does not have to rediscover them:

- `Peers` keeps three `HashMap`s and allocates on admission, rebind and eviction — per packet and per
  event, not per tick. `evict_silent` now reuses a scratch `Vec` per frame instead of allocating one.
- `Simulation::slots` grows on join and never shrinks; that is intentional (no rehash churn), but it
  means the map is sized for the peak concurrent population.
- Lightyear serialises outgoing messages into its own buffers; that cost is inside
  `MessageSender::send` and is not covered by the allocation guard.
- `std::net::UdpSocket::bind` probe in `main` is a one-shot startup check, not a hot path.

---

## 8. Reconnect/resume: the shadow now outlives the link

Moved out of §3 (*what was never considered*), because it is now considered and fixed. The dialogue
never connected reconnection to the movement model, and the transport resolved it the worst possible
way: a silent peer's shadow body was deleted, and the first report on the next link was admitted at
whatever position it claimed. Dropping the connection and reconnecting was therefore an instant,
repeatable teleport — the one check the server *does* have, the displacement rule in
`Simulation::ingest`, was skipped entirely.

The fix keeps that check's baseline instead of deleting it:

- **Departure parks the shadow.** `Peers::release` calls `Simulation::detach_player`, which removes
  the body from the world but keeps its pose, velocity, yaw and grounded flag in a small parked
  table keyed by player id.
- **Resume is clamped to the shadow.** `Simulation::resume_player` reinstates the parked state and
  accepts the reported position only as far as `SimConfig::resume_travel` allows — the same
  `MAX_SPEED × speed_slack × REPORT_WINDOW` budget the displacement validator gives a report that is
  half a second late, plus `POSITION_SLACK` (5.4 m with today's constants). A claim beyond that is
  clamped along the claimed direction; the claimed velocity is discarded and the shadow's last
  validated velocity is restored.
- **The admission report arms the baseline.** The resumed player's `last_seq`/`last_tick` come from
  the report that performed the resume, so the *next* packet is measured from the placement. Without
  that, a client with a high tick counter would get an unbounded allowance on its next report and
  undo the clamp immediately.
- **Travel is bounded by server time.** The displacement allowance is computed from the server ticks
  elapsed since the last accepted report, not from the difference between the report's own tick
  fields. The report tick is client data; measuring with it let a forged tick jump — which the clock
  re-anchor deliberately accepts — buy an unbounded allowance, so the clamp could be walked back one
  packet later.
- **The client is told to adopt the placement.** A clamped resume sets `resync` on the player's
  snapshots. The client applies `shadow_position` (and zeroes its velocity) while the flag is set;
  the server clears it on the first accepted report. Repeating it matters: the snapshot channel is
  unreliable, and a one-shot flag lost on the wire would leave the client forever outside the
  displacement budget.
- **Parked shadows expire.** After `RESUME_WINDOW` (120 s) a resume is no longer possible and the id
  is a first admission again.

What this does *not* fix, stated plainly: identity is still the `player` field of the first packet
(ADR 006), so an attacker who does not care about a particular id can join as a fresh one, and the
first admission still trusts the report. Both belong to the unbuilt auth and spawn-point path; this
change makes sure that *keeping* an identity is not a way around validation.
