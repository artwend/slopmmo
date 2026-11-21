# MMORPG Movement Architecture — Dialogue Review

Summary and critical review of the design discussion (hybrid movement protocol, KCC, and
authority split), plus the decisions this repository is built on.

Status: **working notes**, not a final specification. Numbers marked `TBD` need measurement.

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
5. **Reconnect/resume** with a client-authoritative position.
6. **Tick mapping** between a 60 Hz client and a 30 Hz server: two client ticks fall into one
   server tick. Which server tick does a correction attach to?
7. **Combat lag compensation** reuses server entity history — the dialogue says combat is a
   separate system but never says where that history lives.
8. **Bevy/Avian API risk.** `virtual-character` is a moving branch, not released Avian.
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

- Server tick: 60 Hz everywhere, or 30 Hz with entity collision sub-sampled? Measure first.
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
