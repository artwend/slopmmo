# ADR 015 — Superseded state stays off the wire

- **Status:** Implemented.
- **Decided by:** engineering follow-up to ADRs 006 and 007. The message layouts and the reliability
  model are unchanged; only *when* a message is put on the wire changed.

## Context

Both directions used a fixed cadence:

- the client sent a `PlayerMovementPacket` at `INPUT_SEND_HZ = 30` for as long as it ran, even with
  the stick centred and the character asleep;
- the server sent one `ServerSnapshot` per player per server tick (60 Hz), even when the correction,
  the peer set and the resync flag were identical to the previous tick.

That contradicts the reason both channels are unreliable. Unordered/sequenced unreliable delivery is
chosen because a later message makes an earlier one pointless (ADR 007); a message that is
*identical* to the previous one is pointless when it is produced, not merely when it is late. Two
independent wastes follow:

- a solo or idle client costs ~2.4 KB/s upstream for reports that say "still standing here";
- an idle snapshot costs 60 datagrams/s down per client, while the only fields the client reads are
  `correction` and `resync`. `shadow_position` is informational (adopted only during `resync`) and
  `acked_seq` is not read at all by the current client.

## Decision

**Client.** A report is skipped unless at least one of these changed since the last report: a stick
axis or button, grounding, the pose (to 1 mm), effectively-zero velocity, or the applied-correction
sequence. Skips are bounded: after `IDLE_SEND_TICKS` (4 Hz) a keepalive report goes out. The
keepalive drops the redundancy frames, because there is no input history worth recovering.

`4 Hz` is a compromise:

- it stays far inside the server's `PEER_TIMEOUT` (10 s), so the link is not evicted;
- it bounds the displacement allowance that `Simulation::ingest` derives from the time since the last
  accepted report. At 4 Hz that allowance is a few metres rather than unbounded, which keeps the
  anti-cheat check meaningful without paying for 30 Hz while nothing moves;
- it is frequent enough that the server's dead-reckoned shadow is refreshed regularly.

**Server.** The transport keeps the last snapshot it sent per player and compares the next tick's
snapshot against it. A datagram is produced only when:

- `resync` is set (and it keeps being produced until an accepted report clears it, so a lost
  placement datagram cannot strand a resumed client);
- the correction's `seq` or `epoch` changed, or a correction appeared/disappeared;
- the peer set changed — membership, mode, yaw, or a position differing by at least 1 mm.

`server_tick`, `acked_seq` and `shadow_position` are excluded from the comparison: they do not tell
the client anything it can act on, and including `acked_seq` would force a datagram every second tick
since the client reports at 30 Hz. A `SNAPSHOT_HEARTBEAT` of 250 ms still sends an unchanged
snapshot, so a lost final update is repaired and the informational fields are refreshed.

The simulation still builds a snapshot for every player every tick. The filter lives in the
transport, so diagnostics, tests and future readers of `Simulation::snapshots()` see the full state.

## Consequences

- **Idle client:** 30 reports/s (~2.4 KB/s) → 4 reports/s of ~50 B (~0.2 KB/s).
- **Idle server:** 60 snapshots/s → 4/s per client. For the common case of a stationary player in an
  empty area, the server→client stream is effectively heartbeat-only.
- **Moving peers:** the peer set still changes every tick, so snapshots go out at the full rate while
  anyone nearby moves. This is correct, and it is the next place to optimise (see below).
- **Loss statistics:** `acked_seq` arrives at the heartbeat rate during quiet periods instead of every
  tick. Nothing in the client consumes it today; this would need revisiting before it is used for
  loss estimation.
- **CPU:** a suppressed snapshot never reaches postcard serialisation, and a suppressed report never
  allocates the redundancy vector.
- **Observability:** `Peers::snapshots_skipped` counts ticks that produced no datagram, alongside
  `snapshots_out`.

## What the repository does

- `crates/client/src/main.rs`: `Local::report` gates on the idle predicate; `SentReport` stores what
  the last packet said. Tests cover idle keepalive, full cadence while moving, and a correction ack
  forcing a report out of idle.
- `crates/server/src/transport.rs`: `Peers::last_sent`, `snapshot_changed`,
  `SNAPSHOT_HEARTBEAT`, `snapshots_skipped`. Tests cover informational-only changes, millimetre
  sensitivity, peer membership, correction sequence, resync, and forgetting the cached snapshot when
  a link is released.
- `crates/proto/src/lib.rs`: `IDLE_SEND_HZ`, `IDLE_SEND_TICKS`, `IDLE_SPEED_EPSILON`.

## Open questions

- **The peer list is still the largest avoidable cost.** The client does not read
  `ServerSnapshot::peers` yet (`docs/server-architecture.md` §8), so today it is transmitted for a
  consumer that does not exist. Either wire it into the local solve or stop sending it until it is.
- **Peer positions are absolute `f32` triples with a `u64` id per collider.** Millimetre
  fixed-point and a per-connection `u16` peer id would cut a peer entry roughly in half; delta
  encoding against the previous acknowledged peer set would cut it further.
- **Peer update rate.** Peer colliders do not need the full 60 Hz tick rate — interpolation on the
  client hides 20–30 Hz comfortably. That is a separate change from change-gating.
- **Adaptive heartbeat.** 250 ms is a fixed compromise; it could scale with measured RTT or with how
  recently a snapshot was lost.
