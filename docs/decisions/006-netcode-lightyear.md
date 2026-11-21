# ADR 006 — Netcode library: Lightyear with its built-in transport

- **Status:** **Deviation.** The decision stands; the current scaffold does not implement it.
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

## Deviation in this repository

The prototype talks **raw UDP** (`crates/server/src/net.rs`, `crates/client/src/net.rs`) with a
hand-rolled binary codec in `proto`. It reproduces the *protocol shape* decided in the dialogue
(tick, sequence, redundancy, correction ids) and none of the *netcode machinery* (prediction
history, rollback, interpolation, delta compression, channel scheduling).

Why, stated plainly: the first scaffold was meant to be testable in one `cargo run` with no
dependency risk, and the Bevy version was pinned to 0.20 by the project brief while Lightyear's
compatible release had to be verified first. That is a scheduling argument, not a design one —
the decision in this ADR is unchanged.

## Convergence plan

1. Pin and build the Bevy 0.20-compatible Lightyear release in an isolated crate (`crates/net`)
   before touching gameplay code; add a `--transport lightyear` flag to both binaries.
2. Register the message types from `proto` on a Lightyear protocol: `PlayerMovementPacket` on an
   *unreliable, sequenced* channel at `INPUT_SEND_HZ`, `ServerSnapshot` on an unreliable channel,
   collision events on reliable-ordered.
3. Replicate the peer-collider set as components (`Replicate`) instead of encoding
   `MAX_SNAPSHOT_PEERS` by hand, keeping the K = 8 AOI cap in the visibility filter.
4. Move the client's local character into Lightyear prediction: the KCC becomes the predicted
   system, corrections arrive as replicated components, and the client replays from its own
   history rather than from a shadow position.
5. Keep the headless harness as the acceptance test: identical invariant output, `net_snaps`
   replaced by a snapshot-count metric from the Lightyear client, same zero-malformed guarantee.
6. Only then delete the raw UDP path.

Acceptance: the live round trip passes through Lightyear, the headless invariant script produces
the same `peak_y`/`deterministic` result, and the bot benchmark still fits the tick budget.

## Open questions

- Which Lightyear release pairs with Bevy 0.20 (0.31 was the candidate at decision time)?
- Does Lightyear's unreliable channel support the *sequenced* (drop-older) semantics we rely on,
  or do we need a custom message-ordering wrapper?
- Simplification worth considering: because the server cannot verify terrain, the predicted
  component may only need `position/velocity/applied_correction_seq`, with the KCC replayed from
  server-provided collision events — that is a smaller prediction surface than a stock setup.
