# ADR 006 — Netcode library: Lightyear with its built-in transport

- **Status:** **Partial.** The transport half is implemented (Lightyear 0.31 owns the wire);
  prediction/reconciliation/interpolation are still hand-rolled and moved over deliberately.

- **Decided by:** project owner, first dialogue, turns 7 and 8.
- **Verbatim:** *«Для соединения клиент<-> game server я хочу использовать это
  https://docs.rs/bevy_replicon/latest/bevy_replicon/ или
  https://cbournhonesque.github.io/lightyear/book/»* → *«Я выбираю lightyear + встроенный
  транспорт.»* → *«Я перепутал, для движения обязательно нужно использовать Unreliable.»*

## Context

`bevy_replicon` and Lightyear both replicate ECS state, but only Lightyear carries the whole
client-side prediction stack the movement design depends on:

| Need (from ADRs 007–013) | bevy_replicon | Lightyear |
| --- | --- | --- |
| Client-side prediction of own character | no | yes (`PredictionHistory`, rollback) |
| Server reconciliation with a visual smoothing step | no | yes (`VisualCorrection`) |
| Input buffering tied to ticks, with redundancy | no | yes |
| Channel configuration (reliability, order, priority, send frequency) | no | yes (`ChannelSettings`) |
| Snapshot interpolation for other players | partially | yes (`ConfirmedHistory`) |
| Delta-compressed replication with prioritisation | no | yes |

The owner selected Lightyear **with its built-in transport**, and separately required movement to
travel over an unreliable channel.

## Decision

Use **Lightyear + built-in UDP transport** as the networking layer for client ↔ game server.
Movement and frequent state use unreliable channels; reliable/ordered channels are kept for
discrete commands where order matters.

Non-goals that were made explicit in the dialogue:

- Do not use Lightyear's stock prediction model as a black box. The client and server
  intentionally simulate *different* collision sets (ADR 009), so a correction is not a state
  copy: it is applied, the client replays its own KCC, and only then does the result become the
  new visual state.
- Do not rely on Lightyear's standard input buffer for the merged movement packet (ADR 007); the
  owner merged input and movement report into one message, so redundancy is our own concern.

## Implementation status

**Done — Lightyear owns the wire.** The raw UDP path (`crates/server/src/net.rs`,
`crates/client/src/net.rs`) is deleted.

- `crates/net` declares the protocol: `InputChannel` (client → server,
  `SequencedUnreliable`, `send_frequency = INPUT_SEND_HZ`, priority 2) and `SnapshotChannel`
  (server → client, `SequencedUnreliable`, every tick, priority 1), plus the two messages
  (`PlayerMovementPacket`, `ServerSnapshot`). Both binaries add the same plugin, so the channel
  and message hashes agreed at handshake.
- `crates/server/src/transport.rs`: a `ServerUdpIo` endpoint; lightyear spawns one link per remote
  address, and our admission system binds a `player` id to that link on the first movement packet.
  Peer timeout, superseded-link release and snapshot fan-out live here. A timed-out or torn-down link
  parks its shadow (`Simulation::detach_player`), so the next link for the same id resumes from the
  server's pose rather than being admitted wherever it claims.
- `crates/client/src/transport.rs`: `RawClient` + `UdpIo` + `LocalAddr(0.0.0.0:0)` +
  `PeerAddr(server)`, `Connect`, then a plain `MessageSender`/`MessageReceiver` pair.
- Verified live: `client --headless --connect 127.0.0.1:4000` → `net_sent 600`,
  `net_snaps 1185`, server logs `admitted player 1`.

**Still ours, on purpose.** The client keeps its own KCC, its own redundancy window
(`Local::report`), its own `CorrectionLog` idempotency and its own visual smoothing. Lightyear's
`PredictionHistory`/`VisualCorrection` are not used yet: the two sides simulate *different*
collision sets (ADR 009), so a correction is a velocity/normal hint plus a replay of the local
KCC, which is exactly what a stock state-copy prediction would get wrong.

**Deliberately not done yet.**

- Peer colliders are still hand-encoded into `MAX_SNAPSHOT_PEERS` (ADR 011). Replicating them as
  entities with a visibility filter is the next step; the K = 8 AOI cap stays.
- No delta compression or prioritisation — one snapshot per player per tick, padded to
  `DATAGRAM_BUDGET = 1200` worst case.
- A running client that never sends is never admitted (no `Hello`): identity is the `player` field
  of the first movement packet, per ADR 007.

## Remaining convergence plan

1. Replicate the peer-collider set as components (`Replicate`) instead of encoding
   `MAX_SNAPSHOT_PEERS` by hand, keeping the K = 8 AOI cap in the visibility filter.
2. Move the client's local character into Lightyear prediction: the KCC becomes the predicted
   system, corrections arrive as replicated components, and the client replays from its own
   history rather than from a shadow position.
3. Keep the headless harness as the acceptance test: identical invariant output, `net_snaps`
   replaced by a snapshot-count metric from the Lightyear client, same zero-malformed guarantee.

Acceptance: the live round trip passes through Lightyear, the headless invariant script produces
the same `peak_y`/`deterministic` result, and the bot benchmark still fits the tick budget.
The first clause is green today (`cargo test --workspace` = 39 tests, plus the live check).

## Open questions

- ~~Which Lightyear release pairs with Bevy 0.20?~~ Answered: Lightyear 0.31.0 on Bevy 0.20.1.
- ~~Does Lightyear's unreliable channel support sequenced (drop-older) semantics?~~ Answered:
  `ChannelMode::SequencedUnreliable` does, and it is what both channels use.
- Simplification worth considering: because the server cannot verify terrain, the predicted
  component may only need `position/velocity/applied_correction_seq`, with the KCC replayed from
  server-provided collision events — that is a smaller prediction surface than a stock setup.
