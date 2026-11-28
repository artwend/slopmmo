# ADR 012 — Performance first; anti-cheat is cheap, best-effort and never the architect

- **Status:** Open — no server-side validation is implemented; the server trusts the input stream
  and simulates it. The "performance first" ordering is still the design rule.
- **Decided by:** project owner, second dialogue, turn 8.
- **Verbatim:** *«Не стоит ставить борьбу с читерами во главу угла… В первую очередь всегда стоит
  думать о производительности, а потом уже о читерах. Наша задача — просто не дать читерить
  слишком просто.»*

## Context

The obvious design reflex is server authority everywhere: hold the terrain, re-simulate every
player, validate every position, refuse anything unexplained. That is correct for an esport and
wrong for a 10 000-player PvP zone. The owner's constraint is explicit: **the server must not even
hold terrain data** (ADR 009), so perfect validation is off the table by construction. What is left
is a validation budget that must be cheap enough to run inside the 60 Hz tick for every connected
player.

## Decision

Rank every validation by cost/benefit and accept the gaps:

| Check | Cost | Kept? |
| --- | --- | --- |
| Displacement over a window vs `MAX_SPEED` (accounting for jump/fall) | arithmetic | **yes** |
| Clock offset sanity, report age (`REPORT_WINDOW`) | arithmetic | **yes** |
| Monotonic `seq` per connection, duplicate suppression | set/compare | **yes** |
| Collision with players in PvP, and with bosses | capsule narrow phase, `K = 8` | **yes** |
| Terrain penetration | needs the mesh | **no** — by decision |
| Re-simulating the client's exact KCC | doubles client CPU on the server | **no** |
| Full input replay with rollback on the server | latency, CPU | **no** |
| Statistical anomaly detection, ML | separate service | **not yet** |

Heuristic level, stated honestly: a modified client can walk through walls, teleport slowly, and
climb geometry. It cannot move faster than `MAX_SPEED` per window, cannot skip ticks, cannot win a
PvP collision, cannot replay old input, and cannot turn a reconnect into a teleport. That is the
intended bar: *"don't make cheating trivial"*.

## Consequences

- If a custom correction channel is built, corrections must be **velocity deltas and contact
  normals**, never positions — a cheat must not be able to use it to teleport. The prototype
  deviates: it uses Lightyear's stock predicted-state corrections, which are authoritative state
  snapshots rather than gameplay teleports.
- Any speed/displacement reject should be tolerant (`POSITION_SLACK ≈ 1.35`) because a legitimate
  step-up, jump arc or slope changes the horizontal/vertical ratio. A tight bound produces false
  positives on stairs, which are far more damaging than a small speed hack.
- Measure the displacement window on the **server tick counter**, not the client's report tick: the
  report tick should order and age the report, or a modified client could widen its own budget by
  forging a jump.
- Detection is out of scope for the simulation node; if it is ever added it belongs to a separate
  analytics service consuming the same reports, not to the tick loop.
- Server cost grows with players, not with terrain area — a design constraint that ADR 002's
  sharding model depends on.

## What this repository does

There is no validation layer. The server accepts whatever inputs Lightyear delivers, simulates the
same character controller as the client, and replicates the result, so a modified client can send
any action stream it likes. The only integrity property in the prototype is that the server is the
simulation authority — the client's prediction is corrected to the server's — and the raw UDP link
is unauthenticated.

## Open questions

- Should rejected reports produce a counter that a future analytics service can consume, or is a
  silent reject (current behaviour) enough? Silent rejects are cheap but invisible.
- Is a coarse heightmap (ADR 009 open question) worth it purely as an anti-cheat signal, even
  though it is not needed for correctness?
