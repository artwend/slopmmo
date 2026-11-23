---
name: lightyear
description: Use when writing, reviewing, migrating, or debugging server-authoritative multiplayer code with the Lightyear networking crate for Bevy (cBournhonesque/lightyear). Covers setup (Client/Server/Link entities, Netcode, UDP/WebTransport/WebSocket/Steam), protocol registration, inputs (native, leafwing, bevy_enhanced_input), replication on bevy_replicon, prediction/rollback, interpolation rules, correction, Avian physics integration, deterministic/P2P modes, visibility, and version migrations 0.25 to 0.31. Trigger on "lightyear", "ClientPlugins", "ServerPlugins", "Replicate", "PredictionManager", "InputBuffer", "rollback", "FrameInterpolate", "LightyearAvianPlugin".
---

# Lightyear (Bevy networking)

Reference snapshot: **Lightyear 0.31.0** (published 21 Oct 2026) for **Bevy 0.20**, Rust 1.97.1 or newer. Sections written from the 0.30.0 release notes (14 Sep 2026, Bevy 0.19) still apply to 0.31 unless noted, because 0.31 is mainly the Bevy 0.20 port and has no published release notes or migration guide (issue #1811).

Lightyear changes its API almost every release. Treat everything below as a map, not a spec. Always confirm exact signatures against the checklist in "Verify before you write".

## 1. Version facts

- Current: `lightyear = "0.31"` with Bevy 0.20 and Rust 1.97.1+ (README and docs.rs for 0.31.0).
- Bevy pairing: 0.31 -> Bevy 0.20, 0.28-0.30 -> 0.19, 0.26-0.27 -> 0.18, 0.25 -> 0.17, 0.20-0.24 -> 0.16, 0.18-0.19 -> 0.15, 0.16-0.17 -> 0.14. (The 0.30 release was Bevy 0.19; older README snapshots may still show `"0.29"`.)
- 0.31 dependencies seen on docs.rs: `aeronet_io ^0.22.0-rc.1`, `bevy_app`/`bevy_ecs ^0.20`. The Bevy 0.20 port pulled `bevy_replicon` from its upstream `bevy-0.20` branch and `bevy_egui 0.43.0-rc.1` for the examples at merge time; check the published crate manifests for the final versions.
- Earlier stack for reference (0.28): Aeronet 0.21, bevy_replicon 0.41, Avian 0.7, bevy_enhanced_input 0.26, leafwing-input-manager 0.21, Rust 1.95. 0.30 bumped replicon to 0.44. Use the versions Lightyear's own `Cargo.toml` pins; mismatched replicon/Avian/BEI/aeronet versions are the most common build failure.
- Default serializer is `postcard` (changed from bincode in 0.29). wasm is supported via WebTransport (and WebSocket since 0.24).

## 2. Mental model

- **One entity per connection.** No `ClientConfig`/`ConnectionManager` resources anymore. A client is an entity with `Client` + `Link` + an IO component + a connection layer. A server is an entity with `Server`; each connected peer becomes a child entity with `LinkOf`.
- **Layers** (components you add/remove on the link entity):
  - IO: `UdpIo` / `UdpEndpoint` (server), `WebTransport*`, WebSocket, Steam (via `aeronet`), `crossbeam` (tests/local).
  - Connection: `NetcodeClient` / `NetcodeServer` (netcode.io), or Steam. Identity via `LocalId`/`RemoteId` (`PeerId`).
  - Transport: `Transport` with channels (ordering/reliability).
  - Messages: `MessageSender<M>` / `MessageReceiver<M>`, `MessageManager`.
  - Replication: `ReplicationReceiver` on the receiving side; `Replicate` on entities for the sending side.
- **Lifecycle:** trigger `Connect`/`Disconnect` on a client, `Start`/`Stop` on a server. State via `Connecting`/`Connected`/`Disconnected`, `Starting`/`Started`/`Stopped`. Config changes to a disconnected client apply on the next connect.
- **Replication backend is `bevy_replicon`** (since 0.27). Lightyear keeps `Replicate` as the user-facing send marker; Replicon's own markers are `Replicated` (send side) and `Remote` (received entities). Lightyear adds prediction, interpolation, pre-spawning, hierarchy propagation, rooms and authority metadata on top.
- **Single entity per replicated object** (since 0.25). There are no separate Confirmed/Predicted/Interpolated entities. The entity gets a `Predicted` or `Interpolated` marker; for predicted/interpolated components the server state is stored as `Confirmed<C>` and the live value is `C`.
- **Global timelines/resources** (since 0.29): `LocalTimeline`, `InterpolationTimeline`, `InterpolationConfig`, `PredictionManager`, `Rollback`, `LastConfirmedInput`, `InputTimelineConfig`, `PreSpawnedReceiver` are **resources**, not components on the client entity. `InputTimeline` is gone; read `Res<LocalTimeline>` for the current tick.
- **`Tick` is a non-wrapping `u32` newtype** (0.27+/0.29). Use ordinary comparisons; arithmetic returning `Tick` saturates.

## 3. Crate layout

The `lightyear` crate re-exports sub-crates; you can depend on the sub-crates directly for compile-time savings. Workspace dirs drop the `lightyear_` prefix, package names keep it.

- `crates/io`: `aeronet`, `link`, `udp`, `crossbeam`, `websocket`, `webtransport`
- `crates/connection`: `connection`, `raw_connection`, `netcode`, `steam`
- `crates/core`: top-level `lightyear`, shared core, `sync`, utils, frame interpolation
- `crates/inputs`: `inputs`, `inputs_native`, `input_bei` (`lightyear_inputs_bei`), `inputs_leafwing`
- `crates/replication`: replication, prediction, interpolation
- `crates/transport`: serialization, transport, messages
- `crates/integration`: Avian (`lightyear_avian2d` / `lightyear_avian3d`)
- also `deterministic`, `platform`, `tools` (debug tracing), `tests`

Features seen in docs/examples (verify exact names in `Cargo.toml`): `client`, `server`, `udp`, `netcode`, `leafwing`, Avian integration feature, plus WebTransport/WebSocket/Steam features.

## 4. Setup

```rust
use lightyear::prelude::*;
use core::time::Duration;

const TICK_HZ: f64 = 60.0;
let tick_duration = Duration::from_secs_f64(1.0 / TICK_HZ);

// client app
app.add_plugins(ClientPlugins { tick_duration });
// server app
app.add_plugins(ServerPlugins { tick_duration });
// host-server: both plugin groups in one App
```

Client and server **must** use the same `tick_duration` and the same protocol plugin (same registration order).

Client entity (from the book; 0.29+ shapes):

```rust
let auth = Authentication::Manual {
    server_addr: SERVER_ADDR,
    client_id: 0,
    private_key: Key::default(),
    protocol_id: 0,
};
let client = commands.spawn((
    Client,                       // unit marker since 0.29
    LocalAddr(CLIENT_ADDR),
    PeerAddr(SERVER_ADDR),
    Link::default(),              // Link::default().with_conditioner(c) for simulated latency/loss
    ReplicationReceiver,          // needed to receive replication
    NetcodeClient::new(auth, NetcodeConfig::default())?,
    UdpIo::default(),
)).id();
commands.trigger_targets(Connect, client);

// prediction is enabled by a resource since 0.29
app.insert_resource(PredictionManager::default());
```

Server entity:

```rust
let server = commands.spawn((
    NetcodeServer::new(NetcodeConfig::default()),
    LocalAddr(SERVER_ADDR),
    UdpEndpoint::default(),
    Server,                       // role marker (authoritative server vs P2P endpoint)
)).id();
commands.trigger_targets(Start, server);
```

Notes:
- Connection state on clients: use the `ClientState` QueryData (0.29+), not the old `Client` fields.
- Disconnect/unlink reasons are enums (`UnlinkReason`, `DisconnectedReason`), not strings (0.29+).
- Netcode errors are emitted as Bevy messages (0.30).
- `MAX_CLIENTS` is configurable (0.30).
- Optional LZ4 packet compression exists (0.27+).
- The book is partly stale (older examples mention `ServerUdpIo`, `add_message`, `ChannelDirection`). Prefer `examples/simple_setup`, `examples/simple_box`, and docs.rs for the current names.

## 5. Protocol (shared by client and server)

Put all registration in one `ProtocolPlugin` added after `ClientPlugins`/`ServerPlugins` on both sides.

```rust
// channel (a ZST with the Channel derive)
app.add_channel::<MyChannel>(ChannelSettings {
    mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
    ..default()
})
.add_direction(NetworkDirection::Bidirectional);

// message: Serialize + Deserialize + Clone (+ MapEntities if it holds Entity)
app.register_message::<MyMsg>()
    .add_direction(NetworkDirection::ServerToClient);

// component registration (preferred builder API since 0.27)
app.component::<Health>().replicate();
app.component::<Position>()
    .replicate()
    .predict()
    .add_linear_interpolation()   // for types implementing Bevy's Ease
    .add_correction();
```

Component builder methods seen in release notes:
- `.replicate()`, `.replicate_once()`, `.replicate_filtered::<F>()`, any Replicon rule function
- `.predict()` (call before any prediction option), `.with_rollback_condition(f)`
- `.local_rollback()` (rollback without replication; renamed from `add_rollback`)
- `.add_linear_interpolation()`, `.add_correction()` (same-type case), `.add_linear_correction::<D>()` (was `add_linear_correction_fn`), `.custom_correction()` (you own the correction system; `enable_correction()` is deprecated)
- Diff replication: `.replicate_diff()`, `.predict_diff()` with Replicon's `Diffable` and `EntityDiffExt::apply_diff`. The old Lightyear delta manager and `add_delta_compression` were removed in 0.29. Lightyear's own `Diffable` remains only for prediction correction.
- Immutable components are supported; replicate relationships by registering them as normal components (no `RelationshipSync`).

Messages: send with `MessageSender<M>` on the link entity, receive with `MessageReceiver<M>`. `ServerMultiMessageSender` sends to several clients. Parallel sends via shared borrows exist since 0.30. Messages and events can target a remote timeline with `ChannelSettings::on_timeline` (receiver holds them until that timeline reaches the sender tick), mainly to fire server events on a client's interpolation timeline. Messages holding entities need `.add_map_entities()`; mapping resolves through Replicon's shared map (0.30).

## 6. Inputs

Three implementations share one generic input pipeline (buffering, input delay, rollback, rebroadcast to other clients):

- **Native**: any struct/enum that is `Serialize + Deserialize + Clone + PartialEq + Reflect + Debug + MapEntities`. Add `InputPlugin::<MyInput>::default()`. Include a `None`/default variant so the server can tell "no input" from "lost packet". Write inputs into `ActionState<MyInput>` in `FixedPreUpdate`, in the `WriteClientInputs` set; read them in `FixedUpdate`.
- **leafwing** (`leafwing` feature, `lightyear_inputs_leafwing`): networks `leafwing-input-manager` action states.
- **bevy_enhanced_input** (`lightyear_inputs_bei`): action entities. In 0.27 they had to be pre-spawned on both sides; 0.28 simplified to spawning them on the server and replicating; 0.30 automatically propagates `Replicate` on BEI action entities. Check which model your version uses.

Rules that matter:
- Inputs for tick N on the client are applied at tick N on the server. Gameplay must read inputs in `FixedUpdate`, never in `Update`.
- 0.30: a single `InputConfig<A>` is shared by client and server (`ServerInputConfig<S>` removed). `InputBuffer` is a ring buffer with no public `buffer` field; predicted inputs are no longer stored, call `InputBuffer::predict` to read them.
- Server-side hardening (0.28): future input ticks are bounded; implement the `ValidateInputs` hook and use `authorize_controlled_targets` to defend against spoofed targets. Do not trust client-supplied entity targets.
- Systems that need a synced clock: `SyncedLocalTimeline` (skips the system until synced) or `Option<SyncedLocalTimeline>` for code shared with server/host modes.
- Input buffering only begins after timeline sync; the receiver channel is drained while unsynced.

## 7. Replication (server side)

- Spawn on the server with `Replicate` (send-side marker) and the targeting components for prediction/interpolation, plus `ControlledBy` to tie an entity to a client's lifetime. Constructor names and target components changed in 0.27 and 0.30 ("simplify ReplicationTarget", "separate target and lifecycle markers"). Copy the pattern from `examples/simple_box` for the version you target instead of guessing.
- Replication is **server -> client** only on the Replicon backend. Client-to-server entity replication, distributed authority, per-component priority, authority switching and the `ReplicationGroup` API were removed or are not at parity since 0.27. Use messages or inputs for client -> server data.
- Resources replicate like components (0.28).
- Hierarchies: replication config (rooms, visibility, targets) propagates through hierarchies correctly as of 0.30. Child colliders keep local `Transform` and follow the root.
- `Persistent` on a client-side replicated entity prevents despawn when the client disconnects; put it on the `ReplicationReceiver` entity to cover everything it received. Server-side, `ControlledBy::Lifetime` decides what happens when the controlling client leaves.
- **Visibility / interest management**: built on Replicon visibility filters. Allocate global `RoomId`s with `RoomAllocator` and add `Rooms` to entities and clients. Ad-hoc local room ids no longer work. `NetworkVisibility` is a marker; use `ReplicationState` helpers for gain/lose visibility.
- **Pre-spawning**: `PreSpawned` lets the client spawn an entity locally and have the server's replicated copy match it. Works for any replicated entity, not only predicted ones; `PrePredicted` was removed in 0.25.
- Bandwidth: priority accumulation per message/channel/entity with a bandwidth cap (feature exists; the 0.27+ per-component priority is not at parity).

## 8. Prediction, rollback, interpolation

- Enable prediction with `app.insert_resource(PredictionManager::default())` on the client and `.predict()` on component registration. Target entities with the prediction/interpolation target components on the server.
- Rollback modes (state vs input separately): `Always` (force rollback on every update, useful as a stress test), `Check` (rollback only when received state/input differs from history), `Never`.
- Prediction/interpolation use Replicon confirmation and mutation-checkpoint data, so an unchanged component is not treated as a misprediction. Predicted entities can have confirmed states at different ticks.
- **Interpolation is rule-based (0.29+).** One rule is shared by delayed interpolation of `Interpolated` entities, `FrameInterpolate` between fixed ticks, and visual correction after rollback.
  - Shortest form: `.add_linear_interpolation().add_correction()` for `Ease` types.
  - Custom: `app.interpolate_with::<C>(InterpolationFns::interpolate(f))`; variants `interpolate_with_context`, `history_only()` (keeps `ConfirmedHistory<C>`, no interpolation), `no_history(f)`, `disabled()`.
  - Filtered and prioritized: `interpolate_with_priority_filtered::<C, With<X>>(prio, fns)`. Higher priority wins; ties go to the first registered; a filter gives no automatic priority bonus. Bundle rules via `interpolate_bundle_with::<(A, B)>`; default priority is the component count, so a `(Position, Rotation)` rule beats single-component rules.
- **Frame interpolation is opt-in**: add the type-erased `FrameInterpolate` marker to entities that need it. `FrameInterpolationPlugin` is non-generic and `SkipFrameInterpolation` is gone.
- **Correction easing (0.30)**: `CorrectionEase` = `Linear`, `Smoothstep`, `EaseOutCubic`, `EaseInOutCubic`, `Exponential` (default), chosen on a `CorrectionPolicy`.
- **Runtime timeline switching (0.30)**: move an entity between predicted and interpolated at runtime with a smooth blend:

```rust
commands.entity(e).insert(
    TimelineSwitch::to_predicted()
        .with_transition_secs(0.3)
        .with_ease(CorrectionEase::EaseOutCubic),
);
commands.entity(e).insert(TimelineSwitch::to_interpolated());
```
  See `examples/timeline_switch` (nearby free blocks predicted, carried blocks interpolated).
- Interpolated entities delay despawn until the interpolation timeline reaches the server despawn tick. Entities are not hidden until materialized (that change was reverted in 0.30).
- Lag compensation (`LagCompensationSpatialQuery`) is available so predicted entities can hit interpolated ones (FPS pattern; see `fps` and `projectiles` examples).
- `DeterministicPredicted { skip_despawn }`: by default entities spawned after a rollback's start tick are despawned during rollback; set `skip_despawn = true` for one-time server-replicated spawns.

## 9. Avian physics

```rust
app.add_plugins(LightyearAvianPlugin {
    replication_mode: AvianReplicationMode::Position { sync_to_transform: false },
    ..default()
});
```

- `LightyearAvianPlugin` is **not** added automatically; add it manually.
- Default mode is `Position`: replicate the compact physics pose, interpolate `Position`/`Rotation`, and keep simulation state separate from frame-interpolated visual state. Your `FixedUpdate` gameplay must write `Position`/`Rotation`, not `Transform`. Set `sync_to_transform: true` only if fixed-update gameplay reads or writes `Transform` directly. Use `AvianReplicationMode::Transform` only when scale or hierarchy state is part of the networked state.
- The plugin registers `Position`, `Rotation`, `LinearVelocity`, `AngularVelocity` for rigid-body roots (replicate filtered on `With<RigidBody>`) plus a Hermite bundle rule. If your protocol registers its own physics rules, set `register_physics_components: false`.
- Velocities are corrected together with `Position`/`Rotation` (0.30).
- Disable Avian's `IslandsPlugin` and `IslandSleepingPlugin` when using input-based replication; they break rollback.
- The old `PositionButInterpolateTransform` mode maps to `Position { sync_to_transform: false }`.
- Character controllers: keep all movement in the fixed schedule, driven only by buffered inputs and replicated state, so rollback can re-simulate it. Anything that reads wall-clock time, `Update`-schedule state, or non-replicated components will desync.

## 10. Deterministic replication and P2P

- **Deterministic (0.22+)**: replicate only inputs; every client simulates everything. Use `DeterministicPredicted` instead of `Predicted`. `InputDelayConfig::no_prediction` gives lockstep (note: no check that all peers' inputs arrived for a tick); with input delay, other clients' actions are predicted. Late-join catch-up (0.27) combines input replication with a one-time state snapshot gated by visibility filters. Works in host-client mode. The simulation must be truly deterministic.
- **P2P (0.29+)**, input-only, fixed number of peers:

```rust
app.add_plugins(ClientPlugins { tick_duration });
app.add_plugins(P2PSessionPlugin);
app.insert_resource(P2PSession::new(max_peers)); // capacity incl. local peer
commands.spawn((P2P, Link::default()));          // one per remote peer link
// once the initial cohort exists:
commands.trigger(P2PStart);                      // peers negotiate a start tick, then P2PStarted fires
```
  0.30 added an explicit session-start lifecycle and bounded P2P input rollbacks to session start. Most examples can run in P2P mode.

## 11. Migration cheat sheet

| Version | Breaking changes to expect |
|---|---|
| 0.31 | Bevy 0.20 port (PRs #1804, #1805), no official notes. Apply Bevy 0.20's own migration guide to your code. Steam: `aeronet_steam::SteamworksClient` replaced by `aeronet_steam::SteamworksSockets`. Avian-related migrations were applied (the PR says only "Apply Avian migrations") and lingering `f32` feature references were removed; check your Avian version and feature flags. Lightyear-specific API changes beyond these are unconfirmed; diff against the 0.30.0 tag. |
| 0.30 | `PeerMetadata` removed -> `NetworkingMetadata::peer_map` (`Res<NetworkingMetadata>`). `PacketBuilder::nack_rtt_multiple` removed -> `PacketNackSettings` on `Transport`. `ServerInputConfig<S>` removed -> shared `InputConfig<A>`. `InputBuffer` ring buffer + `predict`. Replicon 0.44. |
| 0.29 | `Client` is a unit marker (`ClientState` query data). `PredictionManager`, `Rollback`, `LastConfirmedInput`, `InputTimelineConfig`, `PreSpawnedReceiver` are resources. `LocalTimeline`/`InterpolationTimeline` are resources; `InputTimeline` removed; `SyncEvent<T>` -> `LocalTimelineShift { delta }`. Rule-based interpolation. `FrameInterpolate` type-erased and opt-in. `add_linear_correction_fn` -> `add_linear_correction`. Avian modes collapsed to `Position`. Delta manager removed -> Replicon `Diffable`. `Link::new(c)` -> `Link::default().with_conditioner(c)`; `LinkConditionerConfig` builders. `Tick` non-wrapping. Serializer -> postcard. Reason fields are enums. |
| 0.28 | Bevy 0.19 stack. BEI replication simplified. Input future-tick bounds + `ValidateInputs`. Resources replicate as components. Diff-compressed components. |
| 0.27 | Replication moved to `bevy_replicon`. `ReplicationGroup` removed; `RoomAllocator` + `Rooms`. `Tick` u16 -> u32. `app.component::<C>()` builder. `add_rollback` -> `local_rollback`. BEI action entities pre-spawned on both sides (later reversed in 0.28/0.30). Structured `lightyear_debug` tracing. Bevy 0.18. |
| 0.26 | Bevy 0.18. `LocalTimeline` became a resource. `NetworkVisibility` became a marker. |
| 0.25 | Bevy 0.17. Confirmed/Predicted/Interpolated merged into one entity; `Confirmed<C>` storage. `PredictionMode`/`InterpolationMode`/`InterpolationManager`/`PrePredicted` removed. `LightyearAvianPlugin` must be added manually. |
| 0.21 | Full rewrite into sub-crates; resources -> entities-per-connection. |

When upgrading across several versions, step through one release's notes at a time and compile between steps.

## 12. Debugging

- `lightyear_debug` tracing layer (via `lightyear_tools`, 0.27+): JSONL rows with categories (timeline, prediction, interpolation, input, sync, messages, entities, transport, components). Add the `LightyearDebug` component to sample components with typed or JSON formatters. Designed for automated desync analysis.
- Link conditioner (`Link::default().with_conditioner(...)`) for latency/jitter/loss testing.
- Force `RollbackMode::Always` to check the game survives constant rollbacks.
- Common desync causes: input read outside `FixedUpdate`; non-replicated state affecting simulation; mismatched protocol registration order; mismatched `tick_duration`; Avian islands/sleeping enabled; system ordering around the Lightyear sets; reading `Transform` when Avian replication is in `Position` mode.
- Examples to run for reference behavior: `simple_setup`, `simple_box`, `bevy_enhanced_input`, `avian_physics`, `avian_3d_character`, `deterministic_replication`, `projectiles`, `fps`, `lobby`, `auth`, `network_visibility`/rooms, `delta_compression`, `timeline_switch`.

## 13. Verify before you write

1. Read the workspace `Cargo.toml` of the exact tag/commit being targeted for Bevy, Replicon, Avian, BEI and leafwing versions.
2. Read the release notes for every version between the user's current version and the target (https://github.com/cBournhonesque/lightyear/releases). 0.31 has none; for it, compare the `0.30.0...0.31.0` diff and PRs #1804/#1805, and build against Bevy 0.20's migration guide.
3. Confirm names on docs.rs for the pinned version (`Replicate` constructors, target components, `Authentication`, IO components, plugin groups, prelude paths).
4. Cross-check with the matching `examples/` code at that tag. The book (https://cbournhonesque.github.io/lightyear/book/) lags the code.
5. State which version and Bevy pairing you assumed (default: Lightyear 0.31 / Bevy 0.20), and flag any API you could not confirm instead of inventing it. Code samples in this file come from 0.27-0.30 notes and docs; they have not been compiled against 0.31.

## 14. Output rules for agents

- Pin `lightyear`, `bevy`, and Lightyear-coupled crates together; never suggest mixing a Lightyear release with another Bevy major (0.31 needs Bevy 0.20; 0.28-0.30 need 0.19).
- Keep protocol registration in one shared plugin so both binaries register identically.
- Put gameplay that must roll back in `FixedUpdate`, driven by inputs and replicated components only.
- Do not suggest client-to-server entity replication or authority transfer on 0.27+; propose inputs/messages instead.
- Prefer the component builder and interpolation-rule APIs over deprecated forms (`add_delta_compression`, `add_linear_correction_fn`, `enable_correction`, `add_rollback`, `SyncEvent`, `RelationshipSync`).
- Validate client-supplied input and targets on the server.
