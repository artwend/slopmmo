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
interpolates peers between updates; without that, 30 Hz would be visible (the client-side consumer
does not exist yet — see the peer-list paragraph).

**Server, peer list.** `ServerSnapshot::peers` is built only when `SimConfig::replicate_peers` is on.
The client does not read it — there is no local peer solve yet — so the shipped configuration neither
scans for peers nor transmits them. The AOI selection and its tests are kept for the consumer that
lands with ADR 006's remaining plan; enabling the flag is then a one-line change.

The simulation still builds a snapshot for every player every tick. The filter lives in the
transport, so diagnostics, tests and future readers of `Simulation::snapshots()` see the full state.

## Consequences

- **Idle client:** 30 reports/s (~2.4 KB/s) → 4 reports/s of ~50 B (~0.2 KB/s).
- **Idle server:** 60 snapshots/s → at most 10/s per client (the 100 ms floor on a fast path), and
  less on a high-RTT link as the heartbeat stretches to 1 s. For a stationary player in an empty
  area, the server→client stream is effectively heartbeat-only. Measured on loopback: 184 snapshots
  over a 20 s script (~9/s) versus 77 with the old fixed 250 ms.
- **Moving peers (when enabled):** with `replicate_peers` on, peer motion goes out at 30 Hz instead
  of 60 Hz, halving the snapshot stream in a crowd, and corrections ride along at tick rate. With
  the shipped default off, a moving player generates no snapshots at all: the client predicts its
  pose, the shadow is not streamed, and corrections still go out on contact.
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

- `crates/client/src/main.rs`: `Local::report` gates on the idle predicate; `SentReport` stores what
  the last packet said. Tests cover idle keepalive, full cadence while moving, and a correction ack
  forcing a report out of idle.
- `crates/server/src/transport.rs`: `Peers::last_sent` (`SentSnapshot { at, peers_at }`),
  `plan_snapshot`, `PEER_UPDATE_INTERVAL`, `heartbeat_interval`, `snapshots_skipped`. Tests cover
  informational-only changes, millimetre sensitivity, peer membership and cadence, a correction
  keeping the peer cadence, heartbeat scaling with RTT, resync repetition, and forgetting the cached
  snapshot when a link is released.
- `crates/proto/src/lib.rs`: `IDLE_SEND_HZ`, `IDLE_SEND_TICKS`, `IDLE_SPEED_EPSILON`,
  `PEER_UPDATE_HZ`.
- `crates/sim/src/authority.rs`: `SimConfig::replicate_peers` (default `false`) gates `aoi_peers`.
  A simulation test pins the default, so the shipped configuration cannot start paying for the peer
  list unnoticed; the AOI tests turn the flag on to keep the selection covered.

## Open questions

- ~~The peer list is transmitted for a consumer that does not exist.~~ Resolved: it is no longer
  built or sent by default (`SimConfig::replicate_peers`, see below). What remains is the
  client-side consumer; the switch turns the feature back on with it.
- **Peer positions are absolute `f32` triples with a `u64` id per collider.** Millimetre
  fixed-point and a per-connection `u16` peer id would cut a peer entry roughly in half; delta
  encoding against the previous acknowledged peer set would cut it further.
- ~~Peer update rate.~~ Resolved: `PEER_UPDATE_HZ = 30`, a separate cadence from corrections.
- ~~Adaptive heartbeat.~~ Resolved: `2 × RTT`, clamped to 100 ms..1 s, falling back to 250 ms until
  a sample exists.
