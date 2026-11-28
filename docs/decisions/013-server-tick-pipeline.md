# ADR 013 — Fixed 60 Hz tick pipeline, no rewind, bounded extrapolation, solve every tick

- **Status:** Partial — the fixed 60 Hz tick is implemented by Bevy and Lightyear; the eight-phase
  server pipeline (spatial index, broadphase, capsule solve, snapshot build) is not built.
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
| Snapshots | adaptive | on change, RTT-adaptive heartbeat (ADR 015) |

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
   (CSR layout, buffers reused).
4. **Broadphase** — nearest `K`, radius-bounded, pair de-duplication.
5. **Collision policy** — pick modes per pair (ADR 010), skip `None`.
6. **Solver** — inelastic capsule/capsule, budgeted vector-sum correction. Runs on **every** tick,
   for soft and hard contacts alike.
7. **State commit** — write positions, velocities, `applied_correction_seq`, bump epoch on handover.
8. **Replication** — build per-player snapshots with AOI, corrections, and acknowledgement.

Rules:

- **Fixed timestep**, no variable dt from wall clock: 60 Hz on both sides, so the client predicts on
  the same step the server simulates.
- **Client prediction is separate.** The KCC's tick counter is the domain of input frames and
  reports. The rates match today, but a server that ran at a different rate would map the client
  domain onto its own by scaling elapsed ticks, never by assuming equality.
- **One tick, one solve.** No subsystem is sub-sampled below the tick. If the solve ever costs too
  much, the lever is the grid and the data layout (§7 of the review), not a lower solve rate.
- **Event time is tick time**, never arrival time. A packet's `tick` decides when it applies; the
  socket's arrival only decides *whether* it still can.
- **No rewind.** Collisions are always solved at the current tick. Lag compensation for hitscan is
  out of scope until a combat design asks for it.
- **Bounded extrapolation.** A player with no fresh report is extrapolated for at most 30 server
  ticks (0.5 s), then frozen. The window is defined in milliseconds and derived from the tick rate,
  so changing the rate does not silently change it.

## Consequences

The measured numbers and mechanisms below belonged to the removed eight-phase server; they are kept
as the design record.

- **Nothing runs above its driving tick.** Movement, grid rebuild, broadphase and both collision
  modes were all 60 Hz, and the client's KCC ran at the same rate.
- **One rate to reason about.** Running both sides at 60 Hz doubles CPU per simulated second versus
  a 30 Hz pipeline, but leaves no subsystem behind the tick. The removed server measured ~0.042
  ms/tick at 32 bots and ~1.13 ms/tick at 256 bots, i.e. ~394× and ~15× realtime headroom.
- **Input cadence was half the tick.** A report every second server tick was why the shadow had to
  be extrapolated between reports.
- **No rewind** is still true in the current tree: Lightyear predicts and corrects, but nothing
  keeps historical collision state, so lag compensation remains out of scope.

## What this repository does

- `crates/shared/src/lib.rs` — `TICK_HZ = 60` and `TICK_DURATION` are the single source of the rate.
  Both binaries insert `Time::<Fixed>::from_hz(TICK_HZ)`; `TICK_DURATION` is passed to
  `ClientPlugins`/`ServerPlugins`, so Lightyear's timeline and Bevy's physics step agree.
- `SharedPlugin` installs the physics pipeline and the movement system; the character is moved by
  `MoveAndSlidePlugin` in `PhysicsSchedule`. The server runs Lightyear's `ServerPlugins` on
  `MinimalPlugins` with a 1 ms schedule runner and has no game loop of its own.
- Client prediction replays exactly one tick on rollback because both sides share `TICK_HZ`.
- **No rewind**: nothing keeps historical collision state, so lag compensation remains unbuilt.
- Not implemented: the spatial-index, broadphase and capsule-solve phases; bounded extrapolation
  between reports (there are no reports); the zero-allocation guard (`crates/sim` was removed).

## Open questions

- When combat is designed, does it need hitscan lag compensation (a rewind buffer of capsule
  positions per tick) or can projectiles be simulated forward instead? The second is far cheaper and
  is compatible with "no rewind".
- Is 60 Hz enough for hard-collision feel? If playtests say no, the resolution is to raise the tick
  rate — not to sub-sample the solve. A 30 Hz tick was tried and reverted.
