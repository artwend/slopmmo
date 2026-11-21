# mmo

A minimal, testable skeleton of an MMORPG client and game server in Rust, implementing the
architecture that came out of the design dialogue review in
[`docs/architecture-review.md`](docs/architecture-review.md).

The point of this repository is not to be a game. It is to be a **working, measurable
implementation of the movement/authority split**, so the claims in the review can be checked by
running code instead of arguing about them.

- **Rust** 1.100.0 (pinned in `rust-toolchain.toml`)
- **Bevy** 0.20 (`MinimalPlugins` on the server, `DefaultPlugins` in the client window)
- **Lightyear** 0.31 for the wire (UDP, channels, message framing)
- 60 Hz fixed server tick, 30 Hz client input send with redundancy

## Layout

| Crate | Role |
| --- | --- |
| `crates/proto` | Wire format, constants, packet types. No Bevy, no logic; serde for the Lightyear path and a hand-rolled codec for the size budget |
| `crates/net` | The Lightyear protocol: two channels and the two messages, added identically by both binaries |
| `crates/sim` | Pure simulation: capsule KCC, spatial hash, pair solver, server authority model |
| `crates/server` | Headless Bevy app: fixed-step accumulator, bot benchmark, Lightyear endpoint |
| `crates/client` | Bevy window (WASD + Q/E + Space) and a headless scripted CI mode |

`proto` and `sim` are deliberately engine-free apart from `bevy_math` vectors, so the same code
runs unchanged on the client (prediction) and on the server (shadow).

## Docs

| Document | What it is |
| --- | --- |
| [`docs/dialogue-decisions.md`](docs/dialogue-decisions.md) | Every decision-making turn from the three design conversations, verbatim (Russian) with translation |
| [`docs/decisions/`](docs/decisions/README.md) | 14 ADRs: what was decided, by whom, at what cost, and what this repo actually does |
| [`docs/server-architecture.md`](docs/server-architecture.md) | The assembled system — layers, world topology, tick pipeline, protocols, ops, status |
| [`docs/architecture-review.md`](docs/architecture-review.md) | Critical review of the dialogue: what was wrong, what replaced it |

Status legend for the ADRs: **Implemented**, **Partial**, **Open** (decided, not built),
**Deviation** (we knowingly do something else). [ADR 006](docs/decisions/006-netcode-lightyear.md)
is now **Partial**: the wire is Lightyear 0.31 on two `SequencedUnreliable` channels; client-side
prediction, reconciliation and interpolation are still ours, because the two sides simulate
different collision sets.

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
cargo test --workspace          # 33 tests: proto, net, sim, server, client
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

### Live round trip

```sh
cargo run --release -p server -- --listen 127.0.0.1:4000
cargo run -p client -- --headless --connect 127.0.0.1:4000
```

The server default is loopback; pass `--listen 0.0.0.0:4000` to accept peers from other hosts
(Windows will ask about the firewall rule the first time), and `--connect <host>:4000` on the
client to reach them. The client always binds an unspecified local address so it can reach any
server, not just a local one.

The headless script is replayed against the real server over Lightyear, paced to real time so
round trips can complete. It reports `net_sent`, `net_snaps` and `net_corrections`, and fails if
no snapshot arrives:

```
net_sent        600
net_snaps       1185
net_corrections 0        # same KCC on both sides, so the wall slide needs no correction
result          ok
```

There is no `Hello`: the first movement packet's `player` field binds that link to a player id and
spawns its shadow at the reported pose. Two clients claiming one id is a known limitation — the
newest link wins and the body is not released from under it. A peer silent for 10 s is dropped
together with its shadow and its sequence state.

The client's tick counter starts at zero while the server's has been running since launch, so
every player carries an offset, anchored on its first report and re-anchored when a long hitch
pushes its clock into the future. Without it a join would be rejected as `StaleTick` forever.

One trap worth knowing: the headless runner drives `App::update` by hand, and Lightyear installs
its message send/receive systems in `Plugin::finish`. Without an explicit `app.finish()` /
`app.cleanup()` the client happily buffers 600 reports and transmits nothing at all.

The scripted session is about 20 s of simulated time, so `--seconds` must be larger than that or
the run ends with `failure script was truncated by --seconds`.

### Observing the traffic

Two things make an empty capture likely, and neither is a bug in the transport:

1. Without `--connect` the client is fully offline — it never opens a UDP socket, so nothing can
   appear anywhere. Check first:

   ```powershell
   Get-NetUDPEndpoint -LocalPort 4000          # the server must own it
   Get-NetUDPEndpoint -OwningProcess <client pid>   # empty means that client is offline
   ```

2. `127.0.0.1` traffic never touches a NIC on Windows. It is only visible on Npcap's loopback
   adapter, listed by Wireshark as **Adapter for loopback traffic capture** (`\Device\NPF_Loopback`).
   Capturing on Wi-Fi or Ethernet shows nothing local. Command line equivalent:

   ```powershell
   & 'C:\Program Files\Wireshark\dumpcap.exe' -i \Device\NPF_Loopback -f "udp port 4000" -w lo.pcapng
   ```

   then filter `udp.port == 4000`. A 20 s round trip is ~1100 frames: 60 Hz snapshots of ~69 B
   server to client, ~30 Hz reports of ~106-123 B the other way (they grow with redundancy), a
   one-off ~500 B channel registry on connect, and small ~45-57 B ping/ack frames both ways.

If both of those check out and the capture is still empty, run the server with
`RUST_LOG=lightyear_udp=trace` — it logs `Received UDP packet from new address` per new peer.

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
| Lightyear + built-in transport | Lightyear 0.31 transport, our prediction | ADR 006 is Partial: prediction/interpolation migration is the remaining step |

Open questions are listed in `docs/architecture-review.md`; the biggest is zone handover, which is
not implemented at all.

## What is not here

No auth, no persistence, no interest management beyond the K=8 neighbour cap, no zone handover, no
combat, no lag compensation, no anti-cheat beyond rate/reject checks. `--bots` and `--headless` are
the only automated entry points.
