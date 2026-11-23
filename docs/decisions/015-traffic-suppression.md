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

**Client.** A report is sent only when it carries something the server cannot already infer, or when
an anchor is due. Something changed when any of these did:

- a stick axis or button, grounding, or the applied-correction acknowledgement;
- the linear velocity by more than `REPORT_VELOCITY_EPSILON`, or the yaw rate by more than
  `REPORT_ANGULAR_EPSILON` — including *stopping* a turn, because the shadow extrapolates the last
  reported rate;
- the pose no longer matches the shadow's dead reckoning (`pos + vel × age`) within
  `REPORT_POSITION_EPSILON`.

Otherwise only an anchor goes out: `ANCHOR_SEND_HZ` (10 Hz) while the character moves and
`IDLE_SEND_HZ` (4 Hz) while it is stationary. Anchors carry no redundancy frames, because there is
no new input to recover. The 10 Hz anchor bounds how long a *lost* velocity change can leave the
shadow on the old value; the 4 Hz idle keepalive stays far inside `PEER_TIMEOUT` (10 s) and keeps
the displacement allowance that `Simulation::ingest` derives from the gap between reports from
growing without bound. At 10 Hz that allowance is ~2 m, at 4 Hz ~3 m.

A turn keeps the full 30 Hz cadence while it lasts. Yaw is dead-reckoned from the reported rate, but
a rate change — especially a stop — has to be heard promptly, and a moving turn changes the
camera-relative wish direction every tick anyway.

Crucially, a *change* is still described on the same 30 Hz cadence grid as before, so the worst-case
delay between a key press and its report is unchanged.

**Server.** The transport keeps the last snapshot it sent per player and compares the next tick's
snapshot against it. A datagram is produced only when:

- `resync` is set (and it keeps being produced until an accepted report clears it, so a lost
  placement datagram cannot strand a resumed client);
- the correction's `seq` or `epoch` changed, or a correction appeared/disappeared;
- the peer set changed — membership, mode, yaw, or a position differing by at least 1 mm.

`server_tick`, `acked_seq` and `shadow_position` are excluded from the comparison: they do not tell
the client anything it can act on, and including `acked_seq` would force a datagram every second tick
since the client reports at 30 Hz.

An unchanged snapshot is still re-sent on a heartbeat, so a lost final update is repaired and the
informational fields are refreshed. The interval adapts to the link's measured RTT: `2 × RTT` — one
trip to deliver the repeat, one to cover an unreliable channel having no acknowledgement — clamped to
100 ms..1 s, with 250 ms while the ping manager has no sample. A fast path repairs a lost update after
~100 ms; a 300 ms path is not sent heartbeats faster than 600 ms, because they would only queue behind
datagrams already in flight. The RTT comes from `Link::stats`, which Lightyear's ping manager
maintains per connected link.

**Server, peer cadence.** Peer colliders are refreshed at `proto::PEER_UPDATE_HZ` (30 Hz), not on
every tick. `plan_snapshot` decides three things independently: whether anything urgent happened
(correction or `resync`), whether the peer list is due, and whether the heartbeat expired. A
correction that lands between peer refreshes leaves immediately and carries the peer list that was
last sent, so peer traffic stays capped without delaying corrections. `resync` is exempt: it is a
rare placement request and the client cannot act on it without the current collider set. A client
interpolates peers between updates; without that, 30 Hz would be visible. The client-side consumer
now exists: `sim::peers::PeerHistory` keeps the replicated sets keyed by server tick and the fixed
tick interpolates between them.

**Server, peer list.** `ServerSnapshot::peers` is built only when `SimConfig::replicate_peers` is on.
It is on by default now that the client solves against the list; `sim::peers::LocalPeerSolve` reads
the tick-keyed history before the terrain step. Setting the flag to `false` skips both the AOI scan
and the wire bytes, for a client build without the local solve.

The simulation still builds a snapshot for every player every tick. The filter lives in the
transport, so diagnostics, tests and future readers of `Simulation::snapshots()` see the full state.

**Server, correction repetition.** A correction is no longer single-shot. The transport keeps the
newest unacknowledged correction per player in flight and repeats it on the ordinary send cadence —
which is the heartbeat cadence, so a repeat costs no extra datagram that the heartbeat would not
have sent anyway — until the client's `applied_correction_seq` echoes it, bounded by
`CORRECTION_RESEND_LIMIT` transmissions and `CORRECTION_INFLIGHT_TTL`. A newer correction supersedes
an unacknowledged older one: the client applies velocity deltas, not a timeline of them, so only the
newest state is worth sending. This is an exception to "superseded state stays off the wire" and it
is the intended one — an unacknowledged correction has not been superseded, it has been lost.

**Client, channel flush.** The input channel flushes every frame. Its `send_frequency` used to be
set to `INPUT_SEND_HZ`, which made lightyear hold each report until its own repeating timer wrapped:
up to one full period of extra one-way latency for a cadence the client already enforces. Only the
client's report gating decides when an input packet exists.

## Consequences

- **Steady client:** a constant velocity is already described by `pos + vel × age`, so 30 reports/s
  (~2.4 KB/s) drops to ~10 anchors/s (~0.8 KB/s) while walking and 4/s (~0.2 KB/s) while stationary.
  Changes and turns still leave at 30 Hz with their input frames. Measured over a 20 s scripted run
  through the packetdump relay: 564 → 147 reports; client→server 63 → 32 packets/s and server→client
  66 → 40 packets/s, because the server also sends fewer ACK-only datagrams for fewer client packets.
- **Idle server:** 60 snapshots/s → at most 10/s per client (the 100 ms floor on a fast path), and
  less on a high-RTT link as the heartbeat stretches to 1 s. For a stationary player in an empty
  area, the server→client stream is effectively heartbeat-only. Measured on loopback: 184 snapshots
  over a 20 s script (~9/s) versus 77 with the old fixed 250 ms.
- **Moving peers (when enabled):** with `replicate_peers` on, peer motion goes out at 30 Hz instead
  of 60 Hz, halving the snapshot stream in a crowd, and corrections ride along at tick rate. This is
  the shipped configuration now that the client solves against the peer set.
- **Loss statistics:** `acked_seq` arrives at the heartbeat rate during quiet periods instead of every
  tick. Nothing in the client consumes it today; this would need revisiting before it is used for
  loss estimation.
- **CPU:** a suppressed snapshot never reaches postcard serialisation, and a suppressed report never
  allocates the redundancy vector. Leaving the peer AOI unbuilt removes a per-player radius query and
  distance scan from every tick: `server --bots N --duration 5` drops from 0.040 to 0.023 ms/tick at
  32 bots and from 1.139 to 0.436 ms/tick at 256 bots on the same build.
- **Observability:** `Peers::snapshots_skipped` counts ticks that produced no datagram, alongside
  `snapshots_out`.

## What the repository does

- `crates/client/src/main.rs`: `Local::report` sends when the state changed or an anchor is due;
  `SentReport` stores what the last packet said (tick, pose, linear and angular velocity, input) so
  dead reckoning can be compared. `ANCHOR_SEND_HZ`, `REPORT_VELOCITY_EPSILON`,
  `REPORT_ANGULAR_EPSILON`, `REPORT_POSITION_EPSILON`. Tests cover idle anchors, steady-walk anchors,
  a velocity/input change, a correction ack, and the end of a turn.
- `crates/server/src/transport.rs`: `Peers::last_sent` (`SentSnapshot { at, peers_at }`),
  `plan_snapshot`, `PEER_UPDATE_INTERVAL`, `heartbeat_interval`, `snapshots_skipped`. Tests cover
  informational-only changes, millimetre sensitivity, peer membership and cadence, a correction
  keeping the peer cadence, heartbeat scaling with RTT, resync repetition, and forgetting the cached
  snapshot when a link is released.
- `crates/proto/src/lib.rs`: `IDLE_SEND_HZ`, `IDLE_SEND_TICKS`, `IDLE_SPEED_EPSILON`,
  `PEER_UPDATE_HZ`.
- `crates/sim/src/authority.rs`: `SimConfig::replicate_peers` (default `true`) gates `aoi_peers`.
  A simulation test pins the default and the built list, so the shipped configuration cannot
  silently stop carrying the colliders the client solves against; the AOI tests keep the selection
  covered.

## Open questions

- ~~The peer list is transmitted for a consumer that does not exist.~~ Resolved: the client-side
  consumer now exists (`sim::peers`), and `SimConfig::replicate_peers` is on by default.
- **Peer positions are absolute `f32` triples with a `u64` id per collider.** Millimetre
  fixed-point and a per-connection `u16` peer id would cut a peer entry roughly in half; delta
  encoding against the previous acknowledged peer set would cut it further.
- ~~Peer update rate.~~ Resolved: `PEER_UPDATE_HZ = 30`, a separate cadence from corrections.
- ~~Adaptive heartbeat.~~ Resolved: `2 × RTT`, clamped to 100 ms..1 s, falling back to 250 ms until
  a sample exists.
