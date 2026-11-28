# Network Layer Review — Lightyear 0.31

Audit of the transport layer as it exists in the repository: what is correct against the
Lightyear 0.31 API, what is wrong, and what is merely mislabelled. The gameplay/authority design
is reviewed in [`architecture-review.md`](architecture-review.md); the decisions themselves are in
[`decisions/`](decisions/README.md). This document only covers the wire.

Status: **acted on, then superseded.** Every finding below was implemented in the hand-written
transport; commit `4af32ef` then deleted that whole layer (`crates/net`, `crates/proto`,
`crates/sim`, and the custom code in both `transport.rs` files) when the project moved to
Lightyear's built-in replication and prediction stack. The current transport is the raw UDP
endpoint in `crates/server/src/transport.rs` and `crates/client/src/transport.rs` with library-owned
replication, prediction, interpolation and input. The findings and resolutions below are kept as the
record of the removed design, not as a description of the current tree.

## Verified stack

`Cargo.lock`, not assumptions:

| Crate | Version |
| --- | --- |
| `lightyear` (and sub-crates) | 0.31.0 |
| `bevy` | 0.20.1 |
| `aeronet_io` | 0.22.0 |
| `postcard` | 1.2.0 |

Lightyear features built by the audited crate: `std`, `client`/`server`, `udp`, `raw_connection`.
**Not** enabled there: `replication`, `prediction`, `interpolation`, `metrics`, `debug`, `netcode`.
Several findings below follow from that list. (The current tree does enable `replication`,
`prediction` and `interpolation`, which is why the audited layer is now gone.)

Every API claim here was checked against the crate sources under
`~/.cargo/registry/src/*/lightyear_*-0.31.0/`, not against the book.

Baseline: `cargo test --workspace` passes (30 sim + 26 server-side + 13 client-side + net/proto
tests; 1 ignored by design — `corrections_are_integrated_once_per_tick`, §7.7).

---

## Findings

| # | Severity | Finding | Status |
| --- | --- | --- | --- |
| N1 | **High** | A link that never delivers a decodable packet is never evicted — unbounded, unauthenticated entity growth | Fixed |
| N2 | **Medium** | Multi-tick frames silently discard every snapshot (and correction) but the last | Fixed |
| N3 | **Medium** | `send_frequency` on `InputChannel` adds up to 33 ms of one-way input latency for no benefit | Fixed |
| N4 | **Medium** | `player` is a trusted packet field; one packet rebinds any player to the sender's link | Fixed |
| N5 | **Medium** | Peer-collider history is sampled ~one-way-latency in the past (anchor never compensates RTT) | Fixed |
| N6 | **Medium** | No protocol/version check in this build — the module comment claims a handshake that is compiled out | Fixed |
| N7 | Low-Med | No connection-state observability: link loss is silent, `MessageSender` then grows unbounded | Fixed |
| N8 | Low | `DATAGRAM_BUDGET = 1200` is the MTU, not a payload budget; the test does not prove "never fragments" | Fixed |
| N9 | Low | Two dead wire fields (`applied_correction_seq`, `acked_seq`) that look like the ack path | Fixed |
| N10 | Low | `protocol-inspector` hand-decodes lightyear framing with no test tying it to the library | Fixed |
| N11 | Low | `net` enables both `client` and `server` features, so each binary links the other role | Fixed |
| N12 | Info | Lightyear's `LocalTimeline`/`Tick` is unused; two tick domains exist, one of them dead | Documented |

---

### N1 — Silent links are immortal (High)

`RawServer` + `UdpEndpointPlugin` create a `ClientOf` link for any inbound datagram from a new
source address, **before** any decoding (`lightyear_udp-0.31.0/src/endpoint.rs`, `Entry::Vacant`
branch; `lightyear_raw_connection-0.31.0/src/server.rs::on_link_of_linked` inserts `ClientOf`).

Eviction is therefore ours, but it is keyed off a map filled only by successfully decoded movement
packets:

- `crates/server/src/transport.rs`, `ingest` — `peers.last_seen.insert(entity, now)` runs only
  after `receiver.receive()` yields a `PlayerMovementPacket`.
- `evict_silent` iterates nothing but `peers.last_seen`.
- `evict_orphans` only reacts to `ClientOf` being *removed*, which cannot happen for a link nothing
  tears down.

One junk byte from a spoofed source address therefore produces a permanent `ClientOf` entity with
`Transport`, `MessageManager` and per-channel buffers, and nothing ever reaps it. Lightyear will
not cover this: there is no idle timeout or keepalive anywhere in `lightyear_link`,
`lightyear_connection`, `lightyear_raw_connection` or `lightyear_udp` in 0.31 (grepped).

**Fix.** Give every link a first-seen timestamp — an observer on `Add<ClientOf>` that seeds
`last_seen` is the smallest version — or drive `evict_silent` from
`Query<(Entity, &Link), With<ClientOf>>` and fall back to a creation time when `last_seen` has no
entry. A hard cap on concurrent links is worth adding in the same change.

---

### N2 — Multi-tick frames drop corrections (Medium)

`Game::advance` runs up to 8 ticks per frame (`crates/server/src/main.rs`); every
`Simulation::step` begins with `snapshots.clear()` (`crates/sim/src/authority.rs`,
`build_snapshots`), while `dispatch` sends at most one snapshot per player per frame behind a single
`snapshots_pending` bool (`crates/server/src/transport.rs`).

After any hitch longer than one tick period only the last tick's snapshot exists, so corrections
produced on the skipped ticks never reach the wire, and `plan_snapshot` never compares them. That is
the load regime where corrections matter, and it compounds §7.7 (corrections are a normal path, not
an exception).

**Fix.** Either keep a small per-tick pending queue that `dispatch` drains, or coalesce at the
source: accumulate the frame's corrections into one snapshot (sum `velocity_delta`, keep the latest
`hard_normal`). The second is cheaper and preserves the "one snapshot per player per wire event"
invariant; the `SequencedUnreliable` channel would keep only the newest datagram anyway.

---

### N3 — `send_frequency` costs latency (Medium)

`crates/net/src/lib.rs` sets `send_frequency` to `1 / INPUT_SEND_HZ` on `InputChannel`. In 0.31 a
channel only flushes when its repeating timer is *finished*
(`is_ready_to_send` → `timer.is_none_or(Timer::is_finished)`,
`lightyear_transport-0.31.0/src/channel/send.rs`); the timer is ticked from `Time<Real>` once per
frame in `TransportPlugin::buffer_receive`, and it starts unfinished — lightyear's own test
`bandwidth_limit_does_not_discard_before_send_frequency` asserts that nothing is collected before the
first period elapses.

A report produced just after a timer boundary therefore waits ≈33 ms before it can enter a packet:
mean ≈17 ms, worst ≈33 ms of added one-way input latency, and a bursty packet pattern. The client
already gates reports to `INPUT_SEND_HZ` in `Local::report`, so the setting saves no packets.
`SnapshotChannel` correctly uses `Duration::default()`.

**Fix.** `send_frequency: Duration::default()` on `InputChannel`; let the client's cadence be the
only gate. `priority: 2.0` is inert here in any case — each direction has exactly one outgoing
channel — so it should not be read as evidence that a bandwidth manager is configured.

---

### N4 — `player` is a trusted field (Medium, cheap hardening)

`ingest` does `peers.entity_of.insert(packet.player, entity)` unconditionally, on every packet. Any
client can send a packet carrying someone else's `player` id and take ownership of that shadow; the
code currently logs this as a friendly "moved to link / NAT rebind". The mirror case — a delayed
datagram from the old address — causes bind thrash, leaving two live links for one player and
snapshots hopping between them.

This sits inside the documented "don't make cheating trivial" budget, but the cheap fix is worth
taking: bind `player` → link on the first packet, reject a conflicting `player` from an
already-bound link, and allow a rebind only when the previous link has been silent beyond a short
window or a resume is in flight. That also removes the thrash case.

---

### N5 — Peer sampling lags by one-way latency (Medium)

`record_peers` anchors `(client tick at arrival, snapshot.server_tick)`, so `predicted_server_tick()`
maps the client's current tick onto the tick at which the snapshot was **produced** — one one-way
delay plus (with change-gated sends) up to a heartbeat in the past.

`PeerHistory::sample` then interpolates between two samples that are both older than that, so the
local solve resolves contacts against peers where they were ~30–80 ms ago on a real link: at sprint
speed that is 0.2–0.5 m, exactly the regime the client-side solve exists for. The sampler itself is
correct — past the newest sample it holds rather than extrapolates — so the defect is in the anchor,
not in `sim::peers`.

**Fix.** Bias the anchor forward by the measured one-way delay (`Link::stats.rtt / 2`) and/or clamp
the requested tick to the newest sample, so stale history can never be consulted for "now".

---

### N6 — The protocol hash check is not in this build (Medium, documentation bug)

`crates/net/src/lib.rs` states that lightyear "hashes that order and refuses a session whose hashes
disagree". That is only true with the `replication` feature: `ProtocolCheckPlugin` is added under
`#[cfg(feature = "replication")]` (`lightyear-0.31.0/src/shared.rs`), and this workspace never
enables it. Message and channel ids are assigned purely by registration order.

The practical impact is "silent no-op with no diagnostic" rather than misdecode — a re-ordered
channel id fails the direction filter and the payload is dropped — but a mismatched client is then
indistinguishable from a dead server.

**Fix.** Put a `PROTOCOL_VERSION: u32` in the first client packet, or a tiny registered message, and
log a clear rejection. That is much cheaper than enabling replicon for the check alone.

---

### N7 — No connection-state handling anywhere (Low-Med)

`ClientState`, `Connected`, `Disconnected`, `Unlink` appear **zero** times in the project's crates.
`MessagePlugin::send` only runs for `With<Connected>`, and `MessagePlugin::clear` clears receivers
only — so once a client entity loses `Connected`, `uplink` keeps appending a report per sent tick to
`MessageSender<PlayerMovementPacket>` in a process that never exits.

**Fix.** Add `With<Connected>` to the `uplink` and `dispatch` queries (this also removes the
unbounded buffer), and add one `Update` system that logs `Disconnected`/`Unlinked`. 0.31 carries
structured `DisconnectedReason`/`UnlinkReason`, so the log can say why the link died. Reconnect and
backoff are a separate decision.

---

### N8 — `DATAGRAM_BUDGET` is the MTU, not a payload budget (Low)

`crates/net/src/lib.rs` sets the budget to 1200, which is exactly `lightyear_link::DEFAULT_MTU`. A
packet needs a 13-byte header plus per-message framing, and anything above `FRAGMENT_SIZE` is
fragmented into several datagrams (unreliable: no retransmit, so one lost fragment loses the
message). The assertion therefore does not prove what its comment claims.

No live bug: the real worst case is ~490 B for a 16-peer snapshot and ~110 B for a 4-frame input
packet, so there is roughly 2× headroom. The constant just invites a future `MAX_SNAPSHOT_PEERS` bump
to start fragmenting silently.

**Fix.** `1200 - 13`, or import lightyear's constant, and assert on a serialized packet rather than
on a message.

---

### N9 — Dead wire fields (Low)

- `MovementReport.applied_correction_seq` is written by the client and never read:
  `Simulation::ingest` ignores it.
- `ServerSnapshot.acked_seq` is written by the server and never read by the client (ADR 015 says so
  explicitly).

Both look like the acknowledgement path §3.3 describes, and neither is. Per ADR 015's own principle,
either use them — server suppresses a correction the client has acked; client derives loss from
`acked_seq` — or delete them. Each costs 1–3 bytes per packet in each direction and misleads the next
reader.

---

### N10 — `protocol-inspector` reimplements lightyear framing unguarded (Low)

`crates/protocol-inspector/src/wire.rs` restates the 0.31 wire format by hand. The layout was verified
correct for 0.31.0 against the real sources (13-byte header: `type:u8`, `packet_id:u16be`,
`last_ack:u16be`, `ack_bits:u32be`, `tick:u32be`, plus the packet-type discriminants). But the crate
does not depend on `lightyear`, so nothing fails when 0.32 changes the format — the tool will simply
decode garbage.

**Fix.** A dev-dependency round-trip test (serialize with lightyear, decode with `wire.rs`) is the
upgrade tripwire, and it costs one test.

---

### N11 — Feature unification (Low)

`crates/net/Cargo.toml` requests `features = ["std", "client", "server"]`, so both binaries compile
and link both halves. `MovementProtocol` only needs `add_channel`/`register_message`, which are not
role-gated; moving the role features into the two binary crates and keeping `net` on `std` should
cut compile time and binary size. Verify by building each target rather than assuming.

---

### N12 — Two tick domains, one dead (Info)

`ClientPlugins`/`ServerPlugins` are given tick durations and lightyear advances its own
`LocalTimeline`, but the game never reads it: `proto::Tick` (wrapping, compared through `since()`) is
the only clock on the wire and in the simulation. Harmless today, because no message or channel uses
`on_timeline` and nothing replicates.

Record it as a constraint: the moment anyone adopts `on_timeline`,
`lightyear_prediction` or `Replicate`, they must switch to `Res<LocalTimeline>` and stop passing
`proto::Tick` across the layer. 0.31's `Tick` is non-wrapping and saturating, so mixing the two is a
correctness bug, not a style choice.

---

## What is already right

Listed so these are not "fixed" during the next pass.

- **Protocol registration**: one shared plugin, added after the plugin group and before the entity —
  matches lightyear's documented ordering, and both sides register identically, which is the
  precondition for any future hash check.
- **Raw connection setup**: `RawClient` + `UdpIo` + `LocalAddr(0.0.0.0:0)` + `PeerAddr` + `Connect`;
  `ServerUdpIo` + `RawServer` + `LocalAddr` + `Start`. That is the 0.31 shape. `ServerUdpIo` still
  exists in 0.31 (it requires `UdpEndpoint` + `Server`), and `UdpPlugin` is added by
  `SharedPlugins`, so the client socket binds without extra plumbing.
- **Retiring a peer**: `try_insert(Disconnecting)` is the library-sanctioned path —
  `lightyear_connection::server` despawns `Disconnecting + ClientOf` in `Last`. Better than a hand
  despawn.
- **Channel modes**: `UnorderedUnreliable` for input (redundancy frames survive a late datagram) and
  `SequencedUnreliable` for snapshots (a superseded snapshot is worthless) is exactly right, and the
  message directions are filtered so a misdirected payload cannot decode.
- **`PeerList`**: inline, `Copy`, custom serde with a hard decode cap and a reject-on-overflow
  visitor — bounded deserialization, no per-snapshot heap allocation.
- **Correction idempotency**: monotonic `seq` + epoch + client `CorrectionLog` with half-range wrap
  comparison; duplicate delivery is genuinely harmless.
- **`MessageSender::send::<Channel>()` / `MessageReceiver::receive()`** are the current APIs, and both
  components are *required* components of `Client`/`ClientOf`. The `Option<&mut …>` guards and the
  "receivers appear on the first payload" comments are unnecessary but not wrong.

---

## Resolution

All twelve findings were implemented in one pass. The tests named here fail without the change.

| # | Change | Where | Covered by |
| --- | --- | --- | --- |
| N1 | Every link gets a silence clock the moment lightyear opens it (`On<Add<ClientOf>>` observer), so the ordinary `PEER_TIMEOUT` reaps links that never send a decodable packet. A `MAX_LINKS` cap drops the newest link instead of growing without bound. | `server/src/transport.rs`: `link_opened`, `MAX_LINKS` | `admission` and release tests; the observer is exercised by the live round trip |
| N2 | `Game` keeps a merged per-player pending queue instead of a `snapshots_pending` bool: corrections from every tick of a hitched frame survive, with velocity deltas summed and the newest normal/sequence winning. | `server/src/main.rs`: `Game::pending`, `merge_snapshot`; `dispatch` iterates it | `a_later_tick_without_a_correction_keeps_the_unsent_one`, `two_ticks_of_contact_leave_as_one_accumulated_correction`, `a_multi_tick_frame_keeps_a_snapshot_for_every_player` |
| N3 | `InputChannel::send_frequency` is `Duration::default()` (flush every frame); the client's cadence is the only gate. The inert `priority` overrides are gone. | `net/src/lib.rs` | wire test still passes; latency is structural (no timer) |
| N4 | `admission()` binds a link to one `player`, rejects a conflicting claim, and only allows a takeover once the previous link has been quiet for `REBIND_GRACE`. A binding is only honoured while the link is still the player's owner (`entity_of[player] == link`), so a superseded link cannot flip snapshot routing back. The binding and `last_seen` refresh only after the packet is accepted, so a link replaying duplicates or rejected claims still times out. | `server/src/transport.rs`: `Admission`, `admission`, `ingest` | `a_link_may_only_speak_for_the_player_it_was_bound_to`, `a_live_player_cannot_be_taken_over_by_a_second_link`, `a_superseded_link_cannot_take_its_player_back` |
| N5 | The clock anchor is biased by `Link::stats.rtt / 2` (`one_way_ticks`), so the client predicts the server's present tick; the collider set is still filed under the tick it describes. | `client/src/transport.rs`, `client/src/main.rs`: `record_peers` | `the_peer_anchor_is_biased_by_the_measured_one_way_delay`, `one_way_delay_is_half_the_measured_round_trip` |
| N6 | `proto::PROTOCOL_VERSION` is stamped in every packet (`PlayerMovementPacket::is_current_version`); the transport rejects a mismatch before admission and logs on powers of two, and the simulation returns `Reject::WrongVersion` as a backstop. | `proto`, `server/src/transport.rs`, `sim/src/authority.rs` | `a_packet_from_another_protocol_revision_is_rejected` (sim + admission) |
| N7 | Send paths are gated on `Connected` (client and server), and both binaries log `Disconnected`/`Unlinked` reasons instead of staying silent. Disconnects are counted and surfaced in the headless report. | `client/src/transport.rs`, `server/src/transport.rs`: `report_disconnects` | headless report fails on `net_disconnects > 0` |
| N8 | `MESSAGE_BUDGET = lightyear::link::DEFAULT_MTU - HEADER_BYTES`, and the size test asserts against it. | `net/src/lib.rs` | `worst_case_packets_fit_one_datagram` |
| N9 | `applied_correction_seq` is now the ack for a bounded in-flight correction set: the newest unacknowledged correction repeats on the send cadence (max `CORRECTION_RESEND_LIMIT`, `CORRECTION_INFLIGHT_TTL`) and stops when the client echoes it. `acked_seq` is read by the client to report reports-in-flight. | `server/src/transport.rs`: `ack_correction`, `correction_to_send`, `track_correction`; `client/src/transport.rs` | `an_unacknowledged_correction_is_repeated_then_dropped`, `the_resend_budget_bounds_an_unacknowledged_correction` |
| N10 | A dev-dependency test pushes a payload through the real lightyear transport and parses the resulting datagram with `wire.rs`. | `crates/protocol-inspector/src/wire.rs`, `Cargo.toml` | `decodes_a_packet_produced_by_lightyear` |
| N11 | `net` depends on lightyear with `std` only; the binaries supply their role. The client binary is now `client`-only. The server still needs `client` + `server` because lightyear 0.31's `lightyear_raw_connection` server module imports client-side lifecycle types — an upstream packaging constraint, documented in `crates/server/Cargo.toml`. | `net/Cargo.toml`, `server/Cargo.toml` | compiles per-target; `cargo build -p server` / `-p client` |
| N12 | The tick-domain constraint is recorded in ADR 006. | `decisions/006-netcode-lightyear.md` | — |

Two consequences worth knowing:

- A correction can now arrive more than once. That is safe because the client dedupes by
  `(epoch, seq)`, and it is the point: a correction is no longer a single-shot datagram.
- A newer simulation correction supersedes an unacknowledged older one rather than queueing behind
  it. The client applies velocity deltas, not a timeline of them, so the newest state is the only
  one worth sending.

Still open from the earlier list, deliberately: per-tick correction integration in the shadow
(§7.7) — the in-flight set was the half that belonged to the transport.

---

## Suggested order

Completed as proposed: N1 + N7 first, then N3 + N5, then N2 + N4, then the rest. N12 is documented
in ADR 006 rather than fixed, because the tick domains only collide if prediction or replication is
adopted later.
