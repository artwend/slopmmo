# mmo

A minimal, testable skeleton of an MMORPG client and game server in Rust, implementing the
architecture that came out of the design dialogue review in
[`docs/architecture-review.md`](docs/architecture-review.md).

The point of this repository is not to be a game. It is to be a **working, measurable
implementation of the movement/authority split**, so the claims in the review can be checked by
running code instead of arguing about them.

- **Rust** 1.100.0 (pinned in `rust-toolchain.toml`)
- **Bevy** 0.20 (`MinimalPlugins` on the server, `DefaultPlugins` in the client window)
- 60 Hz fixed server tick, 30 Hz client input send with redundancy

## Layout

| Crate | Role |
| --- | --- |
| `crates/proto` | Wire format, constants, packet types. No Bevy, no logic. `no_std`-friendly |
| `crates/sim` | Pure simulation: capsule KCC, spatial hash, pair solver, server authority model |
| `crates/server` | Headless Bevy app: fixed-step accumulator, bot benchmark, UDP endpoint |
| `crates/client` | Bevy window (WASD + Q/E + Space) and a headless scripted CI mode |

`proto` and `sim` are deliberately engine-free apart from `bevy_math` vectors, so the same code
runs unchanged on the client (prediction) and on the server (shadow).

## Design in one page

The split is asymmetric, and that asymmetry is the whole architecture:

```
Client                                        Server
------                                        ------
owns terrain                                  owns no terrain at all
KCC + peer colliders, 60 Hz                   shadow kinematics from reports
predicts locally, never waits                 spatial hash -> pairs (K = 8)
applies corrections (velocity / normal)       soft or hard impulse per pair
sends pose reports 30 Hz                      sends snapshot + correction
```

Consequences that are enforced in code, not just documented:

- **No `position_delta` in corrections.** A correction is a velocity delta plus an optional hard
  contact normal, with `(seq, tick, epoch)` so re-delivery is idempotent
  (`client::CorrectionLog`). Snapping the client's position would fight its own solver.
- **The server's position is a shadow.** It is reported back as `shadow_position`, marked
  informational in the packet type, and the client must not adopt it.
- **No rewind anywhere.** Lag compensation is not implemented; the review argues it should be
  built on collider state, not on motion, and that is the shape the peer colliders take.
- **Rejects are counted, never clamped.** `Reject::{DuplicateSeq, StaleTick, TooFast, Teleport}`
  are metrics and cheat signals.
- **Terrain is client-side**, so a boss-vs-terrain contact that only the client can see is
  reported as a hard contact rather than resolved server-side.

## Build, test, run

```sh
cargo build --workspace
cargo test --workspace          # 30 tests: proto, sim, server, client
cargo fmt --check
cargo clippy --workspace --all-targets
```

### Headless client (CI target)

```sh
cargo run -p client -- --headless
```

Runs a fixed script (walk, sprint, into a wall, diagonal slide, jump, settle) twice, checks the
two runs are bit-identical, and exits non-zero if any invariant breaks:

```
script_ticks    1201
peak_y          1.8633     # jump apex: 1.02 m above the 0.9 m resting height
deterministic   true
result          ok
```

Invariants: determinism, room bounds, jump clears 1.1 m, settles back to 0.9 m, grounded at the
end, and the script was not truncated by `--seconds` (default 30, a safety cap).

### Server bot benchmark (CI target)

```sh
cargo run --release -p server -- --bots 32 --duration 5
```

32 bots, no sockets, deterministic seed. One machine's numbers, for scale only:

```
ticks             300
ms_per_tick       0.040
solve_us_per_tick 28.8
pairs_last_tick   110
corrections       9010
headroom_x        416.3
```

`headroom_x` is how many times over the tick budget is met at this bot count. Treat it as "the
solver is not the bottleneck yet", not as a benchmark.

### Live UDP round trip

```sh
cargo run --release -p server -- --listen 127.0.0.1:4000
cargo run -p client -- --headless --connect 127.0.0.1:4000
```

The headless script is replayed against the real server, paced to real time so round trips can
complete. It reports `net_sent`, `net_snaps`, `net_corrections` and `net_malformed`, and fails if
no snapshot arrives. First contact admits the player and spawns its shadow at the reported pose.
A peer silent for 10 s is dropped together with its shadow and its sequence state.

The client's tick counter starts at zero while the server's has been running since launch, so
every player carries an offset, anchored on its first report and re-anchored when a long hitch
pushes its clock into the future. Without it a join would be rejected as `StaleTick` forever.

### Windowed client

```sh
cargo run -p client                    # add --connect 127.0.0.1:4000 to talk to a server
```

Controls: **WASD** move, **Q/E** turn the camera, **Space** jump, **Shift** sprint.

## Differences from the dialogue

The review found seven concrete errors and eight unexamined cases in the original design. The
implementation here diverges where the review concluded the design was wrong:

| Dialogue | Here | Why |
| --- | --- | --- |
| `position_delta` in corrections | none | breaks replay determinism on the client |
| client predicts hard collisions | hard contacts are reported | the server has no terrain to verify them |
| declared velocity validated | position delta between reports validated | declared velocity proves nothing |
| exponential ground friction | linear | exponential settles below `max_speed` |
| build up penetration from zero | capped bias, inelastic impulse | fixed push double-counts |
| "authoritative movement state" | shadow state | the client owns its position |
| corrections applied blindly | `(seq, tick, epoch)` gate | UDP duplicates are normal |
| report tick compared with the server tick | per-player offset mapping | the two clocks start at different times |

Open questions are listed in `docs/architecture-review.md`; the biggest is zone handover, which is
not implemented at all.

## What is not here

No auth, no persistence, no interest management beyond the K=8 neighbour cap, no zone handover, no
combat, no lag compensation, no anti-cheat beyond rate/reject checks. `--bots` and `--headless` are
the only automated entry points.
