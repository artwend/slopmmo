# MMORPG Movement Architecture — Design Review and Implementation Audit

Critical review of the design discussion (hybrid movement protocol, KCC, authority split), plus an
audit of the implementation this repository now contains (§7–§8). The dialogue decisions themselves
are quoted verbatim in [`decisions/`](decisions/README.md).

Status: **working notes**, not a final specification. Measured numbers state their build and
duration; extrapolations are labelled as extrapolations.

This document is the *review*: it records where the dialogue was wrong and what replaced it. The
decisions themselves — as the project owner made them, with verbatim quotes — are in
[`decisions/`](decisions/README.md); the assembled system is in
[`server-architecture.md`](server-architecture.md). Where the two disagree, the ADRs win.

---

## Where every issue stands

Every point this review raised, sorted into what the repository has closed and what it has
not done. The sections below keep the full argument; this is the working set.
Unnumbered on purpose: ADRs and code comments cite "§2.1", "§7 of the review", so the numbering must
not move.

### Closed

| Item | Settled by |
| --- | --- |
| §2.1 Frequencies do not add up | 60 Hz tick with every collision mode solved on every tick; the pose the solver reads is dead-reckoned from the last report, so the 30 Hz report cadence no longer leaves it stale (ADR 013, `Simulation::step`). |
| §2.2 `position_delta` on corrections | Corrections carry `velocity_delta` and `hard_normal` only; the server applies a velocity delta and never a position delta. |
| §2.3 Predicting hard collision | The client solves the replicated colliders itself (`sim::peers::LocalPeerSolve`) from a tick-keyed history, so a hard contact is resolved against the set that belonged to the predicted tick instead of being predicted blind. |
| §2.4 "Authoritative movement state" | The struct is a shadow, the wire field is `shadow_position`, and the docs use the same word. |
| §2.5 Prediction on non-rewound state | Predicted motion reads static terrain plus server-supplied colliders sampled from `PeerHistory` at the predicted tick. Nothing outside that history enters the local solve, and an out-of-order datagram is refused, so a later snapshot cannot change an earlier tick's answer. |
| §2.6 Soft collision numbers | Inelastic relative-normal impulse, vector-sum budget (`CORRECTION_BUDGET = 2.0`), 5 cm contact dead zone (`SOFT_MARGIN`). |
| §2.7 Capsule distance on X/Z only | `vertical_overlap` gates every pair. |
| §2.7 Input channel | `InputChannel` is `UnorderedUnreliable`, not sequenced: a late datagram still delivers its redundancy frames, and the ingest path's per-packet dedupe handles ordering. |
| §2.7 "Nearest 8" without a radius | `NEIGHBOUR_RADIUS = 2.0` and the K limit both apply. |
| §2.7 Validation from the self-declared velocity | The displacement check (`\|Δpos\| ≤ v_max · Δt · k`) is the primary rule; the velocity field is only sanity-checked for finiteness and magnitude. |
| §3.3 Idempotency of corrections | Monotonic `seq` + `epoch`, client `CorrectionLog`, `acked_seq` echoed in the snapshot. Sequence-keyed rather than tick-keyed replay, because there is no rewind to replay. |
| §3.5 Tick mapping 60/30 | `proto::map_client_tick` scales elapsed client ticks and is shared by server ingest and the client's peer-history lookup; corrections are stamped with the server tick. Only the combat hit-tick is left, and that is §3.6. |
| §3.7 Bevy/Avian API risk | Closed by implementation: `crates/kcc-avian` uses released `avian3d` 0.8's `MoveAndSlide` through the `virtual-character` component/plugin API, and the transport is Lightyear 0.31 over a raw UDP endpoint with no `PredictionHistory`/`ConfirmedHistory`. |
| §4.1 Client-side collider solve | `sim::peers::LocalPeerSolve` samples the replicated collider set at the predicted server tick and runs the same `contact_pair` / `solve_into` path as the server, before the terrain step consumes the velocity. `SimConfig::replicate_peers` is on by default now that the consumer exists. |
| §5 Correction budget and teleport threshold | `CORRECTION_BUDGET` and the displacement validator set both. The numbers exist; playtest tuning does not. |
| §5 Peer-collider history | `sim::peers::PeerHistory`: bounded at 32 samples, keyed by server tick, monotonic inserts (a late datagram is refused), interpolated between samples and held — never extrapolated — at the ends. |
| §5 Server tick: 60 Hz or 30 Hz | 60 Hz, every collision mode solved (ADR 013). |
| §7 Findings 1–10 (hot path) | All replaced; the zero-allocation guard (§7.4) now exercises penetrating pairs and correction emission, and fails the build if a tick allocates. Finding 11 (the listen default) is a documentation mismatch, not a hot-path defect; it is still open. |
| §8 Reconnect/resume | The shadow is parked on departure, a resume is clamped to it, and `resync` repeats until a report lands. |

### Open

| Item | What is missing |
| --- | --- |
| §3.1 Boss/player vs terrain | No boss controller (`push_out_of_blockers` is a no-op) and no walkability oracle, so neither the push/re-resolve loop nor a guard against it exists. |
| §3.2 and §5 Zone handover | No zone or handover code; `epoch` is threaded through corrections and snapshots, but nothing bumps it at a boundary crossing. |
| §3.4 Arithmetic at 10k | Measured to 256 bots, not closed for 10k. At 256 bots the solve is ~1.04 ms for ~890 pairs (~1.2 µs per pair); linear scaling to 10k is ~35–40k pairs, ~40–47 ms per tick against a 16.7 ms budget. "A few ms/core" needs zone sharding (or a much faster solver), and the per-pair cost needs a phase-level profile before anyone parallelises. |
| §3.6 Combat lag compensation | No server-side position history and no rewind (ADR 013 open question): hitscan would resolve against current positions. |
| §4.2 and §5 Walkability oracle | No coarse grid and no authoring story; the server cannot reject "inside a wall". |
| §7.5 Parallel solve, incremental grid | Deferred by decision, not defects: the tick keeps ~15× headroom and the solver owns ~90 % of it, so a parallel solve changes results and needs its own determinism test first. Profile the per-pair cost before building it; the current 1.2 µs/pair is high enough that a better sequential solver may buy more than threads. |
| §7.7 Correction stacking | The shadow is still `anchor + vel × age`, so a new velocity delta is applied to every tick since the report instead of once, and corrections still on the wire are not discounted. In the 256-bot benchmark corrections run at ~1.0 per bot per tick, so this is the normal path, not an edge case. It needs a per-tick integration plus an in-flight-correction set before the correction channel is the exception path §4.1 describes; the client-side solve is now built and history-tracked, but it predicts against that unpriced channel. |
| ADR 013 Adaptive snapshot rate | Snapshots are tied to the tick; an adaptive rate needs a client interpolation buffer to be worth anything. |
| §8 Resume allowance | `resume_travel()` is a fixed 5.4 m, not scaled by the time since detach. A rapid disconnect/reconnect cycle can bank the full amount per cycle. The fix is `min(cap, Δt · v_max · k) + slack`, where `Δt` comes from the parked shadow's tick. |
| §8 Server-sanctioned displacement | Only a clamped resume has a `resync` path. Respawn, teleport abilities, knockback and mounts have no way to move the shadow: a legitimate large displacement is rejected as `Teleport`, and `v_max` is a constant, so a faster mount trips the same check. Needs a `place_player`-style operation (bump `epoch`, set `needs_resync`, reset the report baseline) and a per-player speed scale. |

---

## 1. What the design settled on

| Area | Decision |
| --- | --- |
| Replication | Snapshot state replication + message protocol hybrid |
| Transport | UDP, unreliable + sequenced where applicable, Protobuf-like binary |
| Engine | Bevy 0.20, fixed 60 Hz server tick |
| KCC | Avian's virtual character (`VirtualCharacter` + `MoveAndSlidePlugin`) in `crates/kcc-avian`; `sim::kcc` is the engine-free reference solver. |
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

Every line of the left column is now built: the client samples the replicated peer colliders from a
tick-keyed history and separates from them in the same tick the terrain solve runs (§4.1). That is
why §2.3 and §2.5 are closed rather than merely answered by omission.

---

## 2. Errors and contradictions found in the dialogue

### 2.1 Frequencies do not add up — **closed**

The tick table lists `Server movement state 30–60 Hz`, `Hard Collision 60 Hz`,
`Soft Collision 20–30 Hz`. A subsystem cannot run faster than the tick that drives it.

Resolution: **solve collisions every tick.** Buy CPU back elsewhere — incremental spatial
hash updates (dirty cells), not a lower solve rate. A 20–30 Hz soft solve applied to a 60 Hz
KCC is visible as stepped pushes every 33–50 ms.

Adopted: the server tick is **60 Hz** (a 30 Hz tick was evaluated and set back) and every collision
mode is solved on every server tick. The client's KCC runs at the same rate; the wire format still
maps client ticks onto server ticks by ratio rather than assuming equality. See
[ADR 013](decisions/013-server-tick-pipeline.md).

The one figure below the tick that remains is the input report cadence (`INPUT_SEND_HZ = 30`), and it
is a packet rate, not a simulation rate: the shadow is dead-reckoned from the last accepted report on
every tick, capped at `REPORT_WINDOW`, so the solver *and* the state it reads run at the tick rate.

### 2.2 `position_delta` was excluded, then reintroduced — **closed**

The short branch correctly removed `position_delta` from `EntityCollisionCorrection` because
the server cannot guarantee the offset does not land inside a wall. The long branch then put
it back into the same struct.

A positional delta computed from a shadow state **accumulates drift**: the client is already
somewhere else, so every delta injects fresh error. Position corrections must be forbidden;
only velocity/impulse corrections are admissible.

### 2.3 "The client predicts hard collision" guarantees mispredictions — **deferred**

Hard collision is *asymmetric* (Shield Wall: blocker vs blocked) and depends on other players'
positions, which the client only interpolates. Predicting it means predicting wrong exactly
when it matters — a dash through a shield.

Resolution: remote players either do not block local prediction at all, or block it without
being part of the rollback set.

Status: **closed.** The client feeds peer colliders into its own solve, and the set it reads is
history-tracked: `sim::peers::PeerHistory` files each replicated set under the server tick it
describes, refuses out-of-order datagrams, interpolates between samples and holds the ends instead
of extrapolating. A hard contact is therefore resolved against the collider state that belonged to
the predicted tick, not against whatever happened to arrive last.

### 2.4 "Authoritative movement state" is a fiction — **closed**

The server has no authoritative position — it receives one. `ConfirmedHistory`-style
reconciliation therefore does not apply in its standard form, and the whole machine degrades
to "local simulation + injected impulses". This has to be an explicit written decision, and
the server struct should be named *shadow*, never *authoritative*.

### 2.5 The serious one: predicted motion depends on non-rewound state — **deferred**

The client collides locally with NPCs and dynamic objects, but those entities are **not** in
the rollback history, and the solver does not rewind its spatial structure. Every replay after a
correction therefore produces a different result → permanent rubber-banding in crowds.

Resolution: predicted motion may depend **only** on static geometry plus server-supplied
colliders that are themselves history-tracked. NPCs and decoration are non-blocking for
prediction.

Status: **closed for the same reason as §2.3.** Predicted motion now reads static terrain plus the
replicated colliders from the tick-keyed history; nothing outside that history enters the local
solve. A correction arriving later cannot rewrite an earlier tick's collider set, because the
history refuses an older sample rather than letting it land behind a newer one.

### 2.6 Soft collision numbers are wrong — **closed**

`penetration * w`, clamped to an absolute **1.0 m/s**, versus a 3.5 m/s walk speed: separation
always lags approach, so players permanently interpenetrate. And the 1 m/s cap is *per pair*,
so eight neighbours can sum to 8 m/s.

Resolution: clamp the **relative normal velocity** instead (inelastic contact):

```
j = -(1 + e) * (v_rel · n) / (1/m_a + 1/m_b)
```

This fixes penetration and jitter in one step. Add a per-player *vector* impulse budget and a
dead zone around the contact distance.

### 2.7 Other concrete defects — **closed**

- Capsule distance computed on X/Z only → players stacked on different floors push each other.
  Needs a Y overlap test.
- "Nearest 8" without a radius bound: in an empty field that means contacts at 50 m.
  Both constraints are required.
- `Unreliable Sequenced` for input was the wrong channel: a late packet is dropped whole,
  including its redundancy frames. Fixed: `InputChannel` is `UnorderedUnreliable` and the ingest
  path dedupes by packet sequence.
- Speed validation reads the self-declared velocity field, which is trivially forged.
  Validate the **position delta** (`|Δpos| ≤ v_max · Δt · k`, and `Δpos ≈ v · Δt`).

---

## 3. What the dialogue never considered

1. **Open.** **Boss/player vs terrain.** Server pushes → client re-resolves against terrain → client
   ends up inside the boss → server pushes again. Stable oscillation in wall corners.
2. **Open.** **Zone/shard handover** was designed (Voronoi, cellular handover) but never connected to
   the physics model. Crossing a boundary resets the collision set discontinuously.
3. **Closed.** **Idempotency of corrections.** An impulse must live in history keyed by tick and be
   reproduced exactly once during replay, not sit in a "current impulse" field. Settled with a
   monotonic `seq` plus `epoch` and a client-side log: there is no replay to key by tick, because
   there is no rewind.
4. **Measured, not closed.** **No arithmetic**, despite "performance first". 10k players × 8 pairs
   × 60 Hz ≈ 4.8M pair evaluations/s plus 600k grid inserts/s. At 256 bots the solve is ~1.04 ms per
   tick for ~890 pairs, about **1.2 µs per pair**; linear scaling to 10k is ~35–40k pairs, or
   ~40–47 ms per tick against a 16.7 ms budget. "A few ms/core" does not survive the extrapolation:
   10k needs zone sharding or a substantially faster solver, and the per-pair cost itself deserves a
   profile before anyone adds threads.
5. **Closed** for the mapping itself; the combat tick is §3.6. **Tick mapping** between a 60 Hz
   client and a 30 Hz server: two client ticks fall into one server tick. Which server tick does a
   correction attach to? *Resolved for the mapping itself: corrections are stamped with the server
   tick, and `SimConfig::map_client_tick` scales the client's tick domain onto the server's. Combat
   still has to decide which tick a hit resolves on.*
6. **Open.** **Combat lag compensation** reuses server entity history — the dialogue says combat is a
   separate system but never says where that history lives.
7. **Closed.** **Bevy/Avian API risk.** `virtual-character` is a moving branch, not released Avian.
   Lightyear 0.30 `PredictionHistory` / `ConfirmedHistory` / `VisualCorrection` were asserted
   without verification against the docs. The transport is Lightyear 0.31 over a raw UDP endpoint,
   and the controller uses the branch's `VirtualCharacter` / `AutoMoveAndSlide` /
   `MoveAndSlidePlugin` API ported onto released `avian3d` 0.8 (`crates/kcc-avian`).

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

Status: **implemented on both sides.** The server keeps validation and collider state. The client
files the replicated colliders into `sim::peers::PeerHistory`, keyed by server tick, and its fixed
tick solves the local capsule against the set that belonged to the predicted tick — before the
terrain step consumes the velocity, so terrain and peers are resolved in the same tick. The history
is what keeps this from re-opening §2.3 and §2.5: an older datagram is refused, the ends are held
rather than extrapolated, and the contact rule is the server's `collision::contact_pair`, not a
second definition. `SimConfig::replicate_peers` is now on by default because the consumer exists.

The claim that corrections become an *exception* path is still the target state, not the current
one. The shipped server synthesises motion from reports and emits an impulse per contact: the
256-bot benchmark records **76,451 corrections over 300 ticks**, about one per bot per tick. That is
the normal path. §7.7's correction stacking is the remaining prerequisite for making it the
exception path; the client half is built, but it predicts against that unpriced channel.

### 4.2 A cheap terrain oracle instead of no terrain

A coarse walkability grid (1–2 m cells per zone, a few KB) is not collision. It is enough to
reject "inside a wall" and teleports almost for free. This is exactly the stated compromise —
do not make cheating trivial, do not pay for a physics engine — and it was missing.

Status: not implemented. It is still the cheapest defence against walking into geometry, and §3.1
needs it before a boss can push anyone.

---

## 5. Open questions to resolve before feature work

- Walkability grid resolution and how it is authored.
- Handover protocol for collision state across zone boundaries.

---

## 6. First milestone (this repository)

A running skeleton that answers the cheapest question first:

- shared wire protocol (`proto`) — tick-keyed input frames with redundancy, merged
  `PlayerMovementPacket`, aggregated server corrections with idempotency keys, peer colliders;
- pure simulation crate (`sim`) — kinematic model, spatial hash, soft/hard capsule solve,
  position validation, and the client's tick-keyed peer-collider history and local solve, all
  unit-tested without Bevy;
- headless Bevy server (`server`) — fixed tick, UDP, N bot clients for load measurement;
- Bevy client (`client`) — windowed, or `--headless` to run scripted ticks in CI.

Everything that can be decided from numbers (tick rate, K, solver iterations, bandwidth per
player) is decided here. Lightyear 0.31 is now integrated; what remains is feature work and the
open items above, not transport selection.

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
| 11 | The help text and the shipped default disagree on the listen address; the binary binds `0.0.0.0:4000`, while the docs describe a loopback default | `server::Args::parse` | a wrong doc, not a bug — but it exposes the socket on every interface by default |

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
`--listen` default is still `0.0.0.0:4000`; changing it to loopback is an open operational item,
not part of this performance pass.

### 7.3 Result

The before/after table is the data-layout pass in commit `8f42194` (before = its parent),
`--bots N --duration 3`, release. The label matters: pair and correction counts grow with run
length because the bots keep converging, so these figures are only comparable within this run.

| | 32 bots before | 32 bots after | 256 bots before | 256 bots after |
| --- | --- | --- | --- | --- |
| `ms_per_tick` | 0.038 | **0.039** | 1.963 | **0.993** |
| `solve_us_per_tick` | 31.4 | **27.5** | 1909.0 | **904.5** |
| `pairs_last_tick` | 85 | 85 | 876 | 876 |
| `corrections` | 5293 | 5293 | 45 929 | 45 929 |
| `headroom_x` | 435.8 | 427.5 | 8.5 | **16.8** |

The identical `pairs` and `corrections` counts are the point *within this build pair*: this was a
data-layout change, not a behaviour change. At 32 bots the tick was dominated by fixed per-tick
bookkeeping, so it lands within noise; at 256 bots — where the per-player allocations and the tree
walks actually bite — the tick halved. The remaining 904 µs is about **1.03 µs per pair** of solver
work, not overhead. That is high enough that the per-pair cost deserves a profile before anyone
parallelises it.

The current build on the documented 5 s workload (`--duration 5`, release) is:

| | 32 bots | 256 bots |
| --- | --- | --- |
| `ms_per_tick` | 0.042 | 1.128 |
| `solve_us_per_tick` | 31.4 | 1038.9 |
| `pairs_last_tick` | 110 | 887 |
| `corrections` | 9 017 | 76 451 |
| `headroom_x` | 393.5 | 14.8 |

At 256 bots that is **1.17 µs of solve per pair**. The extrapolation to 10k players, and why it does
not fit in 16.7 ms without sharding, is in §3.4. The 3 s and 5 s numbers are not alternatives for
the same claim; quote the one whose duration is stated.

### 7.4 The allocation guard

`crates/sim/tests/allocations.rs` installs a counting `GlobalAlloc` and asserts that **64 consecutive
steady-state ticks allocate zero times** with 16 players in eight *overlapping* pairs. Each pair is
re-armed to 0.2 m of penetration before every measured tick, so the solve emits a correction for all
sixteen players every tick. That is the busiest path in the benchmark — corrections run about once
per bot per tick — and the one where a missing scratch buffer or a `clone()` would actually appear.
Re-arming matters because the solve separates the bodies; without it the fixture would go quiet after
a few ticks and measure an idle tick. The re-arm writes fields only, and every pair stays in the same
grid cell, so it neither allocates nor changes the grid window.

The test also asserts that the warm-up *did* allocate (a dead counter cannot pass) and that the
measured window really emitted `64 × 16` corrections. This is a regression test, not a benchmark: it
fails the moment someone adds a `vec![]`, a `collect()` or a `clone()` of a heap value to
`Simulation::step`.

### 7.5 Multithreading and asynchrony: what is true, and what is not

Claims that are true:

- The UDP endpoint is **non-blocking** (`set_nonblocking(true)` on the socket); no system ever waits
  on the network.
- Receive buffers are **pooled** by lightyear; a steady-state tick does not allocate for *receive*
  I/O. Lightyear's outgoing serialisation and send buffers are **not** covered by the guard; §7.6
  lists that as a known gap.
- The I/O layer runs as an **async** task on the Bevy task pool (`multi_threaded` + `async_executor`
  features are enabled), so socket work is not on the tick thread.
- The process is **multi-threaded**: Bevy's multi-threaded executor distributes systems across the
  task pool.

Claim that would be false, and is therefore not made: **the authoritative tick itself is
single-threaded, deliberately.** Determinism is a stated requirement (ADR 013: fixed `dt`, tie-broken
iteration order), and the tick now costs ~1.13 ms of a 16.67 ms budget at 256 players (5 s run;
§7.3 gives the exact figures and how they vary with duration). Note also that
`ingest`, `drive` and `dispatch` all take `ResMut<GameRes>`, so the executor serialises them by
construction — that conflict *is* the design, not an oversight.

The numbers above are aggregate: `solve_us_per_tick` times the whole `step`, and the pair count is
taken after the broadphase. They do not separate the pair build from the impulse pass, so the
1.17 µs/pair has not been attributed yet. Measure that split before choosing where to parallelise.

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

### 7.7 Correction stacking: known open defect

The shadow pose is `anchor_pos + vel × age`, and `vel` already includes every correction applied
since the anchor. A newly emitted delta is therefore not applied once: it is folded into the velocity
used for the whole window, so its positional effect is multiplied by the ticks elapsed since the
report. Corrections still on the wire are not discounted either. The client's `(seq, tick, epoch)`
gate prevents duplicates, not stacking.

At 60 Hz with 30 Hz reports the age is 1–2 in the common case; a lost report, a hitch or a longer
report interval pushes it higher. A 2 m/s correction at age 5 displaces the shadow by 5 × 2 × `dt`
in the next solve instead of 2 × `dt`, which can overshoot the contact and re-derive in the opposite
direction. The benchmark hides the effect because its bots report every tick; it is still the normal
path in a real session, where the measured rate is ~1.0 correction per bot per tick.

The intended fix has two halves, and both need a test before the correction channel can be trusted
as the exception path §4.1 describes. The client half of §4.1 is now built and history-tracked, so
this is the remaining defect on that path:

1. integrate the shadow one tick at a time (`pos += vel * dt`) instead of recomputing it from the
   anchor with the current velocity, so a new delta cannot reach into ticks already integrated;
2. keep corrections sent but not acknowledged by `applied_correction_seq` in a bounded in-flight
   set, add their sum to the solve velocity, and emit only the residual, so work already on the wire
   is not re-derived.

Until both exist, the correction channel is an *unpriced* normal path, not the exception path §4.1
describes.

The repro is `crates/sim/tests/simulation.rs::corrections_are_integrated_once_per_tick`, ignored until
the fix lands (`cargo test -p sim --test simulation -- --ignored`). On a two-player overlap fixture it
ends 0.034 m ahead of the one-tick integral after 10 ticks; the divergence grows with the number of
corrections in the window.

---

## 8. Reconnect/resume: the shadow now outlives the link

Moved out of §3 (*what the dialogue never considered*): the link-loss case is now considered and
fixed; the refinements that are still missing are listed at the end of this section. The dialogue
never connected reconnection to the movement model, and the transport resolved it the worst possible
way: a silent peer's shadow body was deleted, and the first report on the next link was admitted at
whatever position it claimed. Dropping the connection and reconnecting was therefore an instant,
repeatable teleport — the one check the server *does* have, the displacement rule in
`Simulation::ingest`, was skipped entirely.

The fix keeps that check's baseline instead of deleting it:

- **Departure parks the shadow.** `Peers::release` calls `Simulation::detach_player`, which removes
  the body from the world but keeps its pose, velocity, yaw and grounded flag in a small parked
  table keyed by player id. `release` fires when the link is torn down or when the peer has been
  silent for `PEER_TIMEOUT` (10 s), so a timeout is the slowest way to reach the parked state.
- **Resume is clamped to the shadow.** `Simulation::resume_player` reinstates the parked state and
  accepts the reported position only as far as the travel budget allows — the same
  `MAX_SPEED × speed_slack × REPORT_WINDOW` cap the displacement validator gives a report that is
  half a second late, plus `POSITION_SLACK` (5.4 m with today's constants). A claim beyond that is
  clamped along the claimed direction; the claimed velocity is discarded and the shadow's last
  validated velocity is restored.
- **The shipped allowance is still a fixed 5.4 m.** The intended rule scales it by the time since
  the shadow was parked — `min(cap, Δt · v_max · k) + slack`, with `Δt` from `ParkedShadow::at` —
  so a disconnect/reconnect cycle cannot bank the full amount. That rule is on the open list; today
  a rapid cycle can still collect 5.4 m each time. Anything that can force a clean link teardown
  therefore gets a small but repeatable displacement allowance.
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

What this does *not* fix, stated plainly: there is no server-sanctioned displacement path. Respawn,
a teleport ability, knockback and a mount can all move a legitimate player farther than the budget,
and none of them can set `resync` or bump `epoch`; only a clamped resume can. `v_max` is also a
constant, so a faster mount trips the same check. The missing operation is a `place_player`-style
call: set the pose, reset the report baseline, set `resync`, bump the player's `epoch`, and (for a
mount) carry a per-player speed scale into both the velocity sanity check and the displacement
allowance. Until that exists, a legitimately displaced player is rejected until they reconnect.

Two further gaps sit outside this mechanism. Identity is still the `player` field of the first packet
(ADR 006), so an attacker who does not care about a particular id can join as a fresh one, and the
first admission still trusts the report. Both belong to the unbuilt auth and spawn-point path; this
change makes sure that *keeping* an identity is not a way around validation.
