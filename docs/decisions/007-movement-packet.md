# ADR 007 — One merged `PlayerMovementPacket`, unreliable, 30 Hz send / 60 Hz simulation

- **Status:** Implemented (with a redundancy deviation, see below).
- **Decided by:** project owner, third dialogue, turns 2 and 3.
- **Verbatim:** *«Я хочу объединить PlayerInput и MovementReport.»* and *«Отличное
  предложение»* to the consolidation of: Unreliable Sequenced, 30 sends/s, two consecutive input
  frames per packet, one current movement snapshot, server keeps the previous accepted snapshot
  for swept collision, collision events carry their own sequence ids and are acknowledged by the
  client, and a redelivered correction must not re-apply an impulse.

## Context

The first proposal had three messages: `PlayerInput` (intent), `MovementReport` (result), and
`EntityCollisionCorrection` (server→client). Input and report describe **the same tick window**, so
sending them separately doubles headers, doubles the loss surface and lets a report arrive without
the input that produced it.

The owner's follow-on requirements (accepted in the same exchange) fix the transport shape:

- movement data must be **unreliable** — a lost packet is superseded, never retransmitted;
- **sequenced**, not ordered: a newer packet makes an older one pointless;
- send at half the simulation rate, with **input redundancy** in each packet so a single loss does
  not cost a tick;
- the server keeps the **previous accepted snapshot** so it can test the swept path between two
  reports instead of only the endpoint;
- corrections carry a **sequence number plus tick plus epoch** so duplicates are idempotent;
- **no re-applied impulses** — applying a correction twice must be a no-op.

## Decision

```
PlayerMovementPacket {
    seq, player,
    report { tick, position, velocity, body_yaw, grounded, applied_correction_seq },
    inputs: [ { tick, move_x, move_z, buttons, camera_yaw } ],   // newest first
}
```

- Sent over an **unreliable, sequenced** channel at `INPUT_SEND_HZ = 30`.
- Each packet carries the newest input frame **plus its predecessors** (`INPUT_REDUNDANCY`), so
  losing one packet still delivers every tick's input.
- Exactly **one** movement report per packet — the current one. No report history is retransmitted.
- The client KCC and the authoritative server both run at 60 Hz (`CLIENT_TICK_HZ` /
  `SERVER_TICK_HZ`, ADR 013); input frames are addressed by the client's tick, never by arrival
  time. The server still maps that domain onto its own by ratio, so the wire format does not assume
  the two rates are equal.
- Server→client uses `ServerSnapshot { server_tick, acked_seq, shadow_position, correction, peers,
  resync }` where `correction` is `Option<CollisionCorrection { seq, tick, epoch, velocity_delta,
  hard_normal }>` and `resync` is the resume-placement flag added later
  (`../architecture-review.md` §8).

## Why input must not be reliable-ordered

Reliable-ordered input gives every late packet a chance to arrive eventually, which means the
server can act on input many ticks old — and a client that withholds or reorders its own packets
gains a timing advantage. Unreliable + sequenced makes "old input" simply **gone**, which is the
fairness property the owner asked for. Redundancy inside newer packets recovers loss without
giving anyone a time machine.

## Consequences

- At 4 input frames per packet the client sends ~81 bytes at 30 Hz ≈ 2.4 KB/s while active: 45 bytes
  of header + report, plus 9 bytes per input frame. The report, not the inputs, dominates. While
  idle it drops to a 4 Hz keepalive of ~50 bytes ≈ 0.2 KB/s (ADR 015).
- The server must tolerate gaps in the input stream and never extrapolate more than
  `REPORT_WINDOW = 30` server ticks (0.5 s).
- Duplicate packet suppression is a transport concern (`seq`), duplicate correction application is
  a protocol concern (`seq`, `tick`, `epoch`). Both exist and both are tested.

## What this repository does

Implemented in `crates/proto/src/packet.rs`:

- `PlayerMovementPacket` with `seq`, `player`, `report`, `inputs`, encoded in ~81 bytes at full
  redundancy.
- `INPUT_SEND_HZ = 30`, `CLIENT_TICK_HZ = 60`; the client sends a report every 2nd prediction tick,
  which is every second 60 Hz server tick. Reports that would repeat the previous one are suppressed
  and replaced by a 4 Hz keepalive (ADR 015); the packet layout is unchanged.
- `INPUT_REDUNDANCY = 3` → **4** frames per packet (newest + 3), not 2 as in the dialogue. Chosen
  so that three consecutive losses still leave one fresh frame; still well inside the 60-byte
  budget. Recorded here because it changes the loss tolerance from 1 to 3 packets.
- `CollisionCorrection` carries `seq`/`tick`/`epoch` and is applied idempotently
  (`crates/client/src/headless.rs` `CorrectionLog` tracks applied ids).
- `ServerSnapshot` carries `resync`: set while a clamped resume has not yet been acknowledged by an
  accepted report, so the client adopts the server's placement. This is the only case in which a
  client applies a position from the server; corrections remain velocity deltas and normals.
- No `position_delta` in corrections — the review (`../architecture-review.md`) rejected it: a
  server that cannot see terrain cannot promise an offset lands outside a wall.

## Open questions

- Loss tolerance is currently "3 packets", i.e. 100 ms. Is that right for a 150 ms-RTT player, or
  should redundancy adapt to measured loss?
- Do we need explicit input acknowledgements (server tells the client the last tick it consumed)?
  Today the client only learns from `server_tick` in the snapshot.
