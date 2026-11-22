# ADR 013 — Fixed 60 Hz tick pipeline, no rewind, bounded extrapolation, solve every tick

- **Status:** Implemented. Every collision mode is solved on every server tick; there is no
  sub-sampled subsystem. The server and the client both run at 60 Hz.
- **Decided by:** project owner, second dialogue, final turns (*«Далее»*, *«Продолжи»*), which
  accepted the eight-phase server tick. A 30 Hz server tick was evaluated later and set back to
  60 Hz; the rule that survived both is "solve every tick".
- **Supporting verbatim:** *«…на сервере должны проверяться столкновения с боссами и другими игроками
  в пвп режиме»* (turn 6) and *«Только с ближайшими»* (turn 8) — both are pipeline phases.

## Context

A tick has to do eight different jobs, and the order between them decides whether a player sees
last tick's position, this tick's, or a mixture. The dialogue settled on an explicit phase list,
plus a per-system frequency budget:

| System | Dialogue frequency | Adopted |
| --- | --- | --- |
| Client KCC | 60 Hz | 60 Hz (`CLIENT_TICK_HZ`) |
| Server movement | 30–60 Hz | 60 Hz |
| Soft collision solve | 20–30 Hz | 60 Hz — every tick |
| Hard collision solve | 60 Hz | 60 Hz — every tick |
| Boss body update | 30–60 Hz | 60 Hz |
| Snapshots | adaptive | tied to the tick (60 Hz) |

The dialogue's table does not add up: a subsystem cannot run faster than the tick that drives it,
and a 20–30 Hz soft solve under a 60 Hz KCC is visible as stepped pushes every 33–50 ms. The
resolution is to **solve collisions every tick** and buy CPU back elsewhere — incremental spatial
hash updates (dirty cells), not a lower solve rate. The server tick stays at 60 Hz, so every
subsystem runs at the rate of the tick that drives it.

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
6. **Solver** — inelastic capsule/capsule, budgeted vector-sum correction. Runs on **every** tick,
   for soft and hard contacts alike.
7. **State commit** — write positions, velocities, `applied_correction_seq`, bump epoch on handover.
8. **Replication** — build per-player snapshots with AOI, corrections, and acknowledgement.

Rules:

- **Fixed timestep**, no variable dt from wall clock. `SERVER_TICK_HZ = 60`,
  `SERVER_TICK_DT = 1/60`; the client predicts on the matching `CLIENT_TICK_HZ`.
- **Client prediction is separate.** The KCC runs at `CLIENT_TICK_HZ = 60`; its tick counter is the
  domain of input frames and reports. The two rates happen to match today, but the server maps the
  client's domain onto its own by scaling elapsed client ticks, never by assuming they are equal
  (`SimConfig::map_client_tick`).
- **One tick, one solve.** No subsystem is sub-sampled below the tick. If the solve ever costs too
  much, the lever is the grid and the data layout (§7 of the review), not a lower solve rate.
- **Event time is tick time**, never arrival time. A packet's `tick` decides when it applies; the
  socket's arrival only decides *whether* it still can.
- **No rewind.** Collisions are always solved at the current tick. Lag compensation for hitscan is
  out of scope until a combat design asks for it.
- **Bounded extrapolation.** A player with no fresh report is extrapolated for at most
  `REPORT_WINDOW = 30` server ticks (0.5 s), then frozen. The window is defined in milliseconds and
  derived from the tick rate, so changing the rate does not silently change it.

## Consequences

- **Nothing runs above its driving tick.** Movement state, grid rebuild, broadphase and both
  collision modes are all 60 Hz. The client's local KCC and peer separation run at the same rate;
  server corrections arrive once per server tick.
- **One rate to reason about.** Running both at 60 Hz costs twice the CPU per simulated second of a
  30 Hz pipeline, but leaves no subsystem behind the tick. Measured cost is ~0.042 ms/tick at 32
  bots and ~1.13 ms/tick at 256 bots (release, 300-tick runs), i.e. ~394× and ~15× realtime headroom;
  there is no measured reason to trade the rate away.
- **Input cadence is half the tick.** `INPUT_SEND_HZ = 30` means one report every second server
  tick; the shadow is extrapolated between reports only as far as `REPORT_WINDOW`.
- Snapshot rate is currently tied to the tick (60 Hz); an adaptive snapshot rate needs a
  client-side interpolation buffer to be worth anything.
- Because there is no rewind, a high-ping player's shots land against current positions. That is a
  balance decision the combat design must own, not a movement bug.

## What this repository does

- `crates/server/src/main.rs` — `Game` owns the `Simulation` and the `World`; the fixed step is
  `SERVER_TICK_DT` under `ScheduleRunnerPlugin::run_loop(1 ms)`, and the phases collapse into
  `pump_net` → `authority::ingest` → `spatial` rebuild → `collision::solve` → snapshot build →
  `net::send`.
- `crates/sim/src/authority.rs` — one step is exactly one `Simulation::step`, solver included. The
  per-player client→server tick mapping (`tick_origin`) scales elapsed client ticks by
  `SERVER_TICK_HZ / CLIENT_TICK_HZ`, so a client starting its counter at 0 can join and the two
  clocks stay aligned; a mapped tick outside the window re-anchors instead of rejecting.
- `crates/sim/src/authority.rs` — the shadow pose is derived, not integrated: every tick sets
  `pos = anchor_pos + vel * min(age, REPORT_WINDOW) * dt`, where `anchor_pos` is the pose of the last
  accepted report and `age` is server ticks since it. `INPUT_SEND_HZ` is half the tick, so without
  this every second 60 Hz solve would read a 30 Hz pose. A `needs_resync` session is never
  extrapolated: it waits for the client to report from the placement the server gave it.
- The tick is **single-threaded by decision**, not by omission: determinism (above) comes first, and
  one core is not the constraint yet. It also **does not allocate** in the steady state, which
  `crates/sim/tests/allocations.rs` enforces with a counting allocator.
- Measured: 32 bots at 0.042 ms per tick (~394× headroom), 256 bots at 1.13 ms per tick (~15×).
  The 10k projection is ~40–47 ms/tick without zone sharding; see `architecture-review.md` §3.4.
- Not implemented: adaptive snapshot rate, lag compensation.

## Open questions

- When combat is designed, does it need hitscan lag compensation (a rewind buffer of capsule
  positions per tick) or can projectiles be simulated forward instead? The second is far cheaper and
  is compatible with "no rewind".
- Is 60 Hz enough for hard-collision feel? If playtests say no, the resolution is to raise the tick
  rate — not to sub-sample the solve. A 30 Hz tick was tried and reverted.
