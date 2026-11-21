# ADR 013 — Fixed 60 Hz tick pipeline, no rewind, bounded extrapolation

- **Status:** Partial. The pipeline order and the "no rewind" rule are implemented; the frequency
  split table is collapsed to a single 60 Hz solve.
- **Decided by:** project owner, second dialogue, final turns (*«Далее»*, *«Продолжи»*), which
  accepted the eight-phase server tick.
- **Supporting verbatim:** *«…на сервере должны проверяться столкновения с боссами и другими игроками
  в пвп режиме»* (turn 6) and *«Только с ближайшими»* (turn 8) — both are pipeline phases.

## Context

A tick has to do eight different jobs, and the order between them decides whether a player sees
last tick's position, this tick's, or a mixture. The dialogue settled on an explicit phase list,
plus a per-system frequency budget:

| System | Dialogue frequency |
| --- | --- |
| Client KCC | 60 Hz |
| Server movement | 30–60 Hz |
| Soft collision solve | 20–30 Hz |
| Hard collision solve | 60 Hz |
| Boss body update | 30–60 Hz |
| Snapshots | adaptive |

The owner also accepted that the server does **not** rewind: there is no historical collision state
to resolve a shot against, so lag compensation for instant-hit abilities is a separate, unbuilt
decision.

## Decision

One tick, eight phases, in this order:

1. **Drain input** — read packets, map client ticks to server ticks, drop stale/duplicate.
2. **Movement update** — integrate intent for players without a fresh report; apply reports as the
   shadow base.
3. **Spatial index rebuild** — counting-sort rebuild of a uniform grid over player/boss colliders
   (CSR layout, buffers reused; `crates/sim/src/spatial.rs`).
4. **Broadphase** — nearest `K`, radius-bounded, pair de-duplication.
5. **Collision policy** — pick modes per pair (ADR 010), skip `None`.
6. **Solver** — inelastic capsule/capsule, budgeted vector-sum correction.
7. **State commit** — write positions, velocities, `applied_correction_seq`, bump epoch on handover.
8. **Replication** — build per-player snapshots with AOI, corrections, and acknowledgement.

Rules:

- **Fixed timestep**, no variable dt from wall clock. `TICK_HZ = 60`, `TICK_DT = 1/60`.
- **Event time is tick time**, never arrival time. A packet's `tick` decides when it applies; the
  socket's arrival only decides *whether* it still can.
- **No rewind.** Collisions are always solved at the current tick. Lag compensation for hitscan is
  out of scope until a combat design asks for it.
- **Bounded extrapolation.** A player with no fresh report is extrapolated for at most
  `REPORT_WINDOW = 30` ticks, then frozen.

## Consequences

- The dialogue's frequency split (soft at 20–30 Hz, hard at 60 Hz) is **collapsed**: everything is
  solved at 60 Hz. Rationale: the solve is a small fraction of a tick (measured: 0.042 ms/tick at 32
  bots, 1.004 ms/tick at 256 bots, i.e. 400× and 16.6× realtime headroom), so splitting frequencies
  adds state (a "last solved tick" per pair) for no measured gain. Keep the split as a knob if
  profiling ever demands it.
- Snapshot rate is currently tied to the tick; an adaptive snapshot rate needs a client-side
  interpolation buffer to be worth anything.
- Because there is no rewind, a high-ping player's shots land against current positions. That is a
  balance decision the combat design must own, not a movement bug.

## What this repository does

- `crates/server/src/main.rs` — `Game` owns the `Simulation` and the `World`; the fixed step is
  `ScheduleRunnerPlugin::run_loop(1 ms)` and the phases collapsed into `pump_net` →
  `authority::ingest` → `spatial` rebuild → `collision::solve` → snapshot build → `net::send`.
- `crates/sim/src/authority.rs` — per-player client→server tick mapping (`tick_offset`), so a client
  starting its counter at 0 can join, and a mapped tick in the future re-anchors instead of
  rejecting.
- The tick is **single-threaded by decision**, not by omission: determinism (above) comes first, and
  one core is not the constraint yet. It also **does not allocate** in the steady state, which
  `crates/sim/tests/allocations.rs` enforces with a counting allocator.
- Measured: 32 bots at 0.042 ms per tick (400× headroom), 256 bots at 1.004 ms per tick (16.6×).
- Not implemented: adaptive snapshot rate, per-system frequency split, lag compensation.

## Open questions

- When combat is designed, does it need hitscan lag compensation (a rewind buffer of capsule
  positions per tick) or can projectiles be simulated forward instead? The second is far cheaper and
  is compatible with "no rewind".
- Should the movement phase run at 30 Hz to halve the cost? Currently there is no reason to.
