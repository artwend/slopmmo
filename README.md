# mmo

A minimal, testable skeleton of an MMORPG client and game server in Rust, implementing the
architecture that came out of the design dialogue review in
[`docs/architecture-review.md`](docs/architecture-review.md).

The point of this repository is not to be a game. It is a **working, measurable implementation of
server-authoritative movement with client prediction**, so the movement claims in the review can be
checked by running code instead of arguing about them. The wider collision-authority split argued in
the ADRs is still design only; the status table in
[`docs/server-architecture.md`](docs/server-architecture.md) records what is actually built.

- **Rust** 1.100.0 (pinned in `rust-toolchain.toml`)
- **Bevy** 0.20 (`MinimalPlugins` on the server, `DefaultPlugins` in the client window)
- **Lightyear** 0.31 for replication, prediction, interpolation, inputs and the UDP transport
- **Avian 0.8** character controller (`kcc`) driven by **bevy_enhanced_input** actions
- 60 Hz fixed tick shared by the Bevy fixed clock and Lightyear's timeline

## Layout

| Crate | Role |
| --- | --- |
| `crates/shared` | Everything both sides share: the network protocol (components, inputs, `ProtocolPlugin`), the physics/character pipeline, the one movement system, the level loader, and the pure simulation math (velocity model, reference capsule solver, test room for its parity tests) |
| `crates/kcc` | Avian's virtual character API (`VirtualCharacter`, `AutoMoveAndSlide`, `MoveAndSlidePlugin`) |
| `crates/server` | Headless Bevy app: UDP endpoint, one replicated player per connected client |
| `crates/client` | Bevy window with the predicted character, plus a headless scripted CI mode |
| `crates/protocol-inspector` | **Dev only.** UDP relay + CLI that inspects the live Lightyear protocol: input messages, protocol metadata, replicon traffic and ping/pong |

## Design in one page

Both sides run the same fixed-step simulation; Lightyear keeps them in sync:

```
Client                                        Server
------                                        ------
spawns the replicated player locally          spawns one player per connected client
inserts the KCC + colliders + visuals         same KCC + colliders, no rendering
reads BEI actions -> LinearVelocity           reads the received inputs -> LinearVelocity
PhysicsSchedule: KCC moves the capsule        PhysicsSchedule: identical move
predicts immediately, rolls back on delta     authoritative Position/Rotation/Velocity
interpolates everyone else                    replicates with prediction/interpolation targets
```

The details that matter:

- **The character controller is the predicted simulation.** A player is a `VirtualCharacter`
  with a capsule collider, not a rigid body. `shared::apply_player_input` turns the
  `Movement`/`Jump`/`Sprint` action values into a velocity with the same
  `shared::step_velocity_raw` on both sides, and `MoveAndSlidePlugin` moves the capsule
  during `PhysicsSchedule`. `Position`, `Rotation` and `LinearVelocity` are registered with
  custom (non-rigid-body) replication, prediction, interpolation and correction rules.
- **Input is BEI actions.** The server owns and replicates the action entities; the owning client
  attaches local `Bindings` once the player is `Controlled`, which marks them for lightyear's
  input pipeline. Inputs are rebroadcast, so every client predicts every player. `Jump` uses a
  `Press` condition, which makes its value the press edge and therefore replayable.
- **Both sides collide with the same level.** `shared::level` reads the default map,
  `assets/maps/playground.glb`, straight out of the file and bakes one static trimesh per glTF
  primitive, so the server (colliders only) and the client (the same colliders plus the scene for
  rendering) collide against identical geometry without the server needing an asset or render
  pipeline. A prediction that disagrees with the server becomes a rollback, which is the signal we
  want. `shared::test_room()` and `shared::terrain_bundle()` remain as the synthetic room the
  scripted CI run asserts against.
- **Fixed 60 Hz everywhere.** The Bevy fixed timestep, Lightyear's tick duration and the headless
  script all use `shared::TICK_HZ`, so one replay step is exactly one tick.
- **No rewind.** Corrections come from Lightyear's prediction machinery; the server does not
  rewind history for lag compensation.

## Docs

| Document | What it is |
| --- | --- |
| [`docs/dialogue-decisions.md`](docs/dialogue-decisions.md) | Every decision-making turn from the three design conversations, verbatim (Russian) with translation |
| [`docs/decisions/`](docs/decisions/README.md) | 15 ADRs: what was decided, by whom, at what cost, and what this repo actually does |
| [`docs/server-architecture.md`](docs/server-architecture.md) | The assembled system — layers, world topology, tick pipeline, protocols, ops, status |
| [`docs/architecture-review.md`](docs/architecture-review.md) | Critical review of the dialogue: what was wrong, what replaced it |

[ADR 006](docs/decisions/006-netcode-lightyear.md) is now **implemented**: the wire, the input
buffering, prediction/rollback, interpolation and visual correction all come from Lightyear. The
older hand-rolled `net` path was removed, and the legacy `proto` packet crate is gone too.

## Build, test, run

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
```

Level maps (`assets/maps/*.glb`) live in Git LFS: run `git lfs install` once per machine, and
`git lfs pull` if a clone was made without LFS, or the client and server will fail to find the map.

### Headless client (CI target)

```sh
cargo run -p client -- --headless
```

Runs a fixed script (walk, sprint, into a wall, diagonal slide, jump, settle) twice against the
real Avian controller, checks the two runs are bit-identical, and exits non-zero if any invariant
breaks:

```
script_ticks    1201
peak_y          1.8733     # jump apex: ~0.97 m above the 0.9 m resting height
deterministic   true
result          ok
```

Invariants: determinism, room bounds, jump clears 1.1 m, settles back to 0.9 m, grounded at the
end, and the script was not truncated by `--seconds` (default 30, a safety cap). The script runs in
the synthetic `shared::test_room()`, whose walls the bounds invariant is written against; the level
itself is what the live round trip below exercises.

### Live round trip

```sh
cargo run -p server -- --listen 127.0.0.1:5888
cargo run -p client -- --headless --connect 127.0.0.1:5888
```

The headless client connects through the same Lightyear path as the windowed one, waits for the
server to replicate the controlled player, then drives the scripted inputs through the action
mocks, predicting against the same level the server simulates. It fails if the server never
replicates a controlled player or the script never runs. The scripted session is about 20 s, so
`--seconds` must be larger than that.

The server binds `0.0.0.0:5888` by default; pass `--listen 127.0.0.1:5888` to keep it on loopback.

### Windowed client

```sh
cargo run -p client -- --connect 127.0.0.1:5888
```

The window loads the default level, `assets/maps/playground.glb` (`shared::level::DEFAULT_LEVEL`),
so it renders even without a server; `--connect` adds the predicted player. Controls: **WASD**
move, **Space** jump, **Shift** sprint. The camera follows the predicted player.

### Parsed packet view (dev tool)

`crates/protocol-inspector` is a transparent UDP relay: point a client at it and it forwards every datagram
to the server while printing what it contains. It decodes the live protocol — the client's
`InputMessage` (end tick, target entities and their action states), the `ProtocolCheck` and
`SenderMetadata` metadata messages, and ping/pong. The three replicon channels carry
`bevy_replicon`'s own replication framing rather than messages, so they are reported by channel
and size:

```sh
cargo run -p protocol-inspector -- --listen 127.0.0.1:4001 --server 127.0.0.1:4000
cargo run -p client -- --connect 127.0.0.1:4001
```

`--detail` prints the decoded message as RON, `--hex` appends a datagram dump, and
`crates/protocol-inspector/protocol-inspector.toml` holds the display filter.

## What is not here

No auth, no persistence, no interest management, no zone handover, no combat and no lag
compensation. `--headless` is the only automated client entry point; the server no longer has a
bot benchmark.
