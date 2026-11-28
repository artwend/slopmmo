# ADR 007 — One merged `PlayerMovementPacket`, unreliable, 30 Hz send / 60 Hz simulation

- **Status:** Superseded by Lightyear's input and replication pipeline (ADR 006). The merged
  packet no longer exists; what survived is the decision that input is unreliable, tick-addressed
  and redundant.
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
    report { tick, position, linear_velocity, angular_velocity, body_yaw, grounded,
             applied_correction_seq },
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

The numbers in this section describe the packet that was removed; they are kept as the design
record.

- At 4 input frames per packet the client sends ~93 bytes at 30 Hz ≈ 2.8 KB/s while active: 57 bytes
  of header + report, plus 9 bytes per input frame. The report, not the inputs, dominates. While
  idle it drops to a 4 Hz keepalive of ~62 bytes ≈ 0.3 KB/s (ADR 015).
- The server must tolerate gaps in the input stream and never extrapolate more than
  `REPORT_WINDOW = 30` server ticks (0.5 s).
- Duplicate packet suppression is a transport concern (`seq`), duplicate correction application is
  a protocol concern (`seq`, `tick`, `epoch`). Both exist and both are tested.

## What this repository does

The merged packet is gone. Inputs travel as Lightyear input messages for the `Player` context: the
client's `Movement`/`Jump`/`Sprint` action values are buffered by Lightyear's input timeline
(`InputTimelineConfig::balanced()` adds a small input delay) and rebroadcast to every client, so
every client predicts every player. The movement *result* is not on the input channel at all — the
server's `Position`, `Rotation` and `LinearVelocity` replicate as components, predicted by the owner
and interpolated by everyone else.

What carried over from this decision: input is tick-addressed, unreliable, and redundant (the
library repeats recent input frames in each message), and both sides simulate at
`shared::TICK_HZ`. What did not: the single merged packet, the 30 Hz report cadence, the hand-rolled
redundancy window and the server-side correction message.

## Open questions

- ~~Loss tolerance / redundancy~~ Answered by the migration: Lightyear's input pipeline owns
  redundancy, and `InputTimelineConfig` owns the delay/rollback window. The remaining tuning
  question is how much input delay to trade for jitter tolerance on a real uplink.
- ~~Explicit input acknowledgements~~ No longer application-visible; Lightyear tracks confirmed
  inputs as part of prediction and rollback.
