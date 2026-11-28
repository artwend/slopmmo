# ADR 006 — Netcode library: Lightyear with its built-in transport

- **Status:** **Implemented.** Lightyear 0.31 owns the wire and the prediction stack:
  replication, rollback/reconciliation, interpolation, visual correction and input buffering all
  come from the library. The earlier hand-rolled `net`/`proto` movement path was removed; the
  sections below describe that intermediate state and are kept as history.

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

Non-goals that were made explicit in the dialogue, both of which were later dropped:

- Do not use Lightyear's stock prediction model as a black box. *(Dropped: the client and server
  now simulate the same room, so stock prediction/rollback applies.)* The old design had the two
  sides simulate *different* collision sets (ADR 009), which is why a correction used to be a
  velocity hint plus a local KCC replay rather than a state copy.
- Do not rely on Lightyear's standard input buffer for the merged movement packet (ADR 007); the
  owner merged input and movement report into one message, so redundancy was our own concern.
  *(Dropped: ADR 007's packet is superseded, and the input pipeline is Lightyear's.)*

## Implementation status

**Implemented — Lightyear owns the wire and the prediction stack.** This supersedes the notes
below.

- `crates/shared` holds the whole protocol: the `Player` input context, the `Movement`/`Jump`/
  `Sprint` enhanced-input actions, `PlayerId`, and `LightyearAvianPlugin` plus the custom
  registration of `Position`, `Rotation`, `LinearVelocity` and `Grounded` for the
  controller-driven (non-rigid-body) character. `SharedPlugin` is added by both binaries.
- The player is a `VirtualCharacter` (Avian `MoveAndSlide`), not a rigid body. Both sides run
  `shared::apply_player_input` in `FixedUpdate` and the same physics pipeline, so the predicted
  result and the authoritative result come from identical code.
- The server spawns one player per connected client with `Replicate`, `PredictionTarget`,
  `InterpolationTarget` and `ControlledBy` (session-scoped: it despawns on disconnect). The
  server owns the action entities; the owning client attaches local bindings once the player is
  `Controlled`. Inputs are rebroadcast so every client predicts every player.
- Everything runs at `shared::TICK_HZ` (60 Hz): the Bevy fixed clock, Lightyear's tick duration
  and the headless script.
- Verified live: `cargo run -p server -- --listen 127.0.0.1:5888` plus
  `cargo run -p client -- --headless --connect 127.0.0.1:5888` completes the full script and ends
  at the same pose as the offline run.

### Historical: the intermediate hand-rolled path

The intermediate migration ran a hand-written protocol on top of Lightyear's raw transport: a `net`
crate declared `PlayerMovementPacket`/`ServerSnapshot`, `proto` defined their encodings, and `sim`
held the server shadow, the spatial grid, the capsule solver and the client's peer history. That
layer was deleted when Lightyear's replication and prediction stack were adopted (commit `4af32ef`).
Its design rationale is recorded in ADRs 007 and 009–015 and audited in
[`../network-review.md`](../network-review.md), which describes code that no longer exists.

**Still ours, on purpose.** The simulation: `shared::movement` is the single input→velocity model
and `crates/kcc` is the Avian virtual-character controller, with `shared::player_physics_bundle`
building the character on both sides. Lightyear owns everything around it — replication,
prediction and rollback, interpolation, input buffering and the UDP transport.

**Deliberately not done yet.**

- **No interest management.** Every player replicates to every client
  (`Replicate::to_clients(NetworkTarget::All)`); the test room is small enough that full
  replication is not yet the bottleneck.
- **No event plane.** Discrete gameplay messages, acknowledgements and prioritisation have no home
  yet (ADR 001).
- **No authentication or connection negotiation.** The raw UDP endpoint accepts any source address
  and spawns a player when a client connects.
- **No delta-compression or send-priority tuning.** Component replication uses the library defaults
  with a 100 ms send interval (`shared::SEND_INTERVAL`, passed to `ReplicationMetadata`).

## Next steps

- Replace `NetworkTarget::All` with a visibility/priority filter once player count makes full
  replication expensive.
- Add the event plane on reliable Lightyear channels (ADR 001).
- Put authentication and connection negotiation in front of the raw UDP link.

## Tick domains

There is one tick domain. `shared::TICK_HZ` (60 Hz) sets the Bevy `Time<Fixed>` step, and
`shared::TICK_DURATION` is passed to both `ClientPlugins` and `ServerPlugins`, so Lightyear's
timeline, the physics step and a prediction replay all describe the same tick. No custom tick type
crosses the wire; input messages and replicated components are Lightyear's own.

## Open questions

- ~~Which Lightyear release pairs with Bevy 0.20?~~ Answered: Lightyear 0.31 on Bevy 0.20.
- ~~Does Lightyear's unreliable channel support sequenced (drop-older) semantics?~~ No longer
  relevant: the hand-written channels were removed with `net`; inputs and replication use
  Lightyear's own channels and input pipeline.
- When does replicating every player to every client stop being acceptable, and does Lightyear's
  visibility filtering cover it without re-introducing a custom replication path?
