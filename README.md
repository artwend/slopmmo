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
- 60 Hz fixed server tick with a collision solve every tick, 30 Hz client input send with redundancy

## Layout

| Crate | Role |
| --- | --- |
| `crates/proto` | Wire format, constants, packet types. No Bevy, no logic; serde types that Lightyear serialises with postcard |
| `crates/net` | The Lightyear protocol: two channels and the two messages, added identically by both binaries |
| `crates/sim` | Pure simulation: capsule KCC reference solver, spatial hash, pair solver, server authority model |
| `crates/kcc-avian` | Avian's virtual character API (`VirtualCharacter`, `AutoMoveAndSlide`, `MoveAndSlidePlugin`), used by the client |
| `crates/server` | Headless Bevy app: fixed-step accumulator, bot benchmark, Lightyear endpoint |
| `crates/client` | Bevy window (WASD + Q/E + Space) and a headless scripted CI mode |
| `tools/packetdump` | **Dev only.** UDP relay + CLI that decodes Lightyear framing, both movement messages and ping/pong (packet rows, detail, rates) |

`proto` and `sim` are deliberately engine-free apart from `bevy_math` vectors, so the same code
runs unchanged on the client (prediction) and on the server (shadow). The one crate that does pull
in a physics engine is `kcc-avian`, and only the client depends on it.

## Docs

| Document | What it is |
| --- | --- |
| [`docs/dialogue-decisions.md`](docs/dialogue-decisions.md) | Every decision-making turn from the three design conversations, verbatim (Russian) with translation |
| [`docs/decisions/`](docs/decisions/README.md) | 14 ADRs: what was decided, by whom, at what cost, and what this repo actually does |
| [`docs/server-architecture.md`](docs/server-architecture.md) | The assembled system — layers, world topology, tick pipeline, protocols, ops, status |
| [`docs/architecture-review.md`](docs/architecture-review.md) | Critical review of the dialogue: what was wrong, what replaced it |

Status legend for the ADRs: **Implemented**, **Partial**, **Open** (decided, not built),
**Deviation** (we knowingly do something else). [ADR 006](docs/decisions/006-netcode-lightyear.md)
is now **Partial**: the wire is Lightyear 0.31, with an `UnorderedUnreliable` input channel and a
`SequencedUnreliable` snapshot channel; client-side prediction, reconciliation and interpolation
are still ours, because the two sides simulate different collision sets.

## Design in one page

The split is asymmetric, and that asymmetry is the whole architecture:

```
Client                                        Server
------                                        ------
owns terrain                                  owns no terrain at all
Avian virtual character, 60 Hz                shadow kinematics from reports, 60 Hz
receives peer colliders, does not solve them  spatial hash -> pairs (K = 8), every tick
predicts locally, never waits                 soft or hard impulse per pair
applies corrections (velocity / normal)       sends snapshot + correction
sends pose reports 30 Hz
```

Peer colliders are on the wire (`ServerSnapshot::peers`) but are not yet fed into the client's
solve, so that second line is half-built: prediction currently depends on terrain alone. The
remaining half is on the open list in [`architecture-review.md`](architecture-review.md).

Server and client both run at 60 Hz, and the server solves every collision mode on every tick.
Nothing is sub-sampled below the tick that drives it, because a 20–30 Hz soft solve under a 60 Hz
KCC is visible as stepped pushes.

Consequences that are enforced in code, not just documented:

- **No `position_delta` in corrections.** A correction is a velocity delta plus an optional hard
  contact normal, with `(seq, tick, epoch)` so re-delivery is idempotent
  (`client::CorrectionLog`). A positional delta computed from a shadow carries the shadow's drift
  and fights the client's own solver; there is no replay whose determinism it would preserve.
- **The server's position is a shadow.** It is reported back as `shadow_position`, marked
  informational in the packet type, and the client must not adopt it — except while the `resync`
  flag is set, which happens only when a resumed session's claim had to be clamped.
- **No rewind anywhere.** Lag compensation is not implemented; the review argues it should be
  built on collider state, not on motion, and that is the shape the peer colliders take.
- **Report rejects are counted, never clamped.** `Reject::{DuplicateSeq, StaleTick, TooFast,
  Teleport}` are metrics and cheat signals. The one deliberate clamp is a resume: the reported
  position is limited to the fixed resume budget from the parked shadow, not yet scaled by the time
  since detach (`architecture-review.md` §8).
- **Terrain is client-side, and peers do not block prediction.** The client predicts against static
  terrain alone. A hard peer contact arrives as a velocity correction with a normal, not as a
  position snap; a boss-vs-terrain contact that only the client can see therefore cannot be
  resolved server-side. The intended client-side collider solve is on the open list.
- **The simulation tick does not allocate.** The grid, the solver scratch and the snapshot buffer
  are owned and reused, and `crates/sim/tests/allocations.rs` fails the build if a tick starts
  allocating (`docs/architecture-review.md` §7). The guard covers penetrating pairs and correction
  emission. Lightyear's receive buffers are pooled; its outgoing send buffers are outside the guard.

## Build, test, run

```sh
cargo build --workspace
cargo test --workspace          # 84 tests: proto, net, sim, kcc-avian, server, client, packetdump
                                 # plus one ignored repro for the correction-stacking defect (§7.7)
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
peak_y          1.8733     # jump apex: ~0.97 m above the 0.9 m resting height
deterministic   true
result          ok
```

Invariants: determinism, room bounds, jump clears 1.1 m, settles back to 0.9 m, grounded at the
end, and the script was not truncated by `--seconds` (default 30, a safety cap).

### Server bot benchmark (CI target)

```sh
cargo run --release -p server -- --bots 32 --duration 5
cargo run --release -p server -- --bots 256 --duration 5
```

Deterministic seed, no sockets. One machine's numbers, for scale only:

```
                   32 bots     256 bots
ticks              300         300
ms_per_tick        0.042       1.128
solve_us_per_tick  31.4        1038.9
pairs_last_tick    110         887
corrections        9017        76451
headroom_x         393.5       14.8
```

`headroom_x` is how many times over the tick budget is met at this bot count. Treat it as "the
solver is not the bottleneck yet", not as a benchmark. The bots predict at 60 Hz and report in the
client tick domain, so `rejected_stale` stays at zero.

At 256 bots the solve is about 1.17 µs per pair. Linear extrapolation to 10k players is ~35–40k
pairs, or ~40–47 ms per tick against the 16.7 ms budget; that is why zone sharding, not a bigger
core, is the requirement at that scale (`docs/architecture-review.md` §3.4). The 32-bot column
barely moves between builds because fixed per-tick bookkeeping dominates it; the 256-bot column is
the one that responds to the data layout. The review pass in `docs/architecture-review.md` §7
halved it in its own 3 s run (1.96 ms → 0.99 ms) and left `pairs` and `corrections` bit-identical
*within that run*. The numbers above are a 5 s run of the current build and are not directly
comparable to the 3 s figures.

### Live round trip

```sh
cargo run --release -p server -- --listen 127.0.0.1:4000
cargo run -p client -- --headless --connect 127.0.0.1:4000
```

The server binds `0.0.0.0:4000` by default, which accepts peers from other hosts (Windows will ask
about the firewall rule the first time). Pass `--listen 127.0.0.1:4000` to keep it on loopback; the
intended default is loopback, and that change is still open (`architecture-review.md` §7.1). Use
`--connect <host>:4000` on the client to reach any server; the client always binds an unspecified
local address, not just a local one.

The headless script is replayed against the real server over Lightyear, paced to real time so
round trips can complete. It reports `net_sent`, `net_snaps`, `net_corrections` and `net_resyncs`,
and fails if no snapshot arrives:

```
net_sent        601
net_snaps       1205
net_corrections 0        # the shadow never solves terrain, so a wall slide needs no correction
net_resyncs     0        # a continuous session is never placed by the server
result          ok
```

There is no `Hello`: the first movement packet's `player` field binds that link to a player id and
spawns its shadow at the reported pose. Two clients claiming one id is a known limitation — the
newest link wins while the body is live. A peer silent for 10 s is dropped, but its shadow is parked
for `RESUME_WINDOW` (120 s); the next link for the same id is placed from that shadow, and its
claimed position is clamped to the fixed 5.4 m resume budget (scaling that by the time since detach
is still open; see `architecture-review.md` §8). A clamped resume makes the client adopt
`shadow_position` (`net_resyncs` counts those snapshots), so the session continues from where the
server put it instead of being rejected as a teleport for the rest of its life. After the window,
the id is a first admission again.

The client's tick counter starts at zero while the server's has been running since launch, so
every player carries a `(client tick, server tick)` anchor, re-anchored when a long hitch pushes its
clock out of the window. The mapping scales elapsed client ticks by the server/client rate ratio;
both run at 60 Hz today, but the protocol does not assume it, and a mismatched pair stays aligned
instead of re-anchoring. Without the mapping, a join would be rejected as `StaleTick` forever.

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

### Parsed packet view (dev tool)

`tools/packetdump` is a developer-only UDP relay that forwards traffic between a client and the
server and prints every Lightyear packet with its decoded messages as RON. It needs no Npcap,
capture driver, or administrator rights, and it forwards the bytes unchanged. Point the client at
the relay instead of the server:

```sh
cargo run -p server -- --listen 127.0.0.1:4000
cargo run -p packetdump -- --listen 127.0.0.1:4001 --server 127.0.0.1:4000
cargo run -p client -- --headless --connect 127.0.0.1:4001
```

The launcher wraps the same commands and manages every child process:

```powershell
.\tools\run-local.ps1 -Capture                          # server + 1 windowed client
.\tools\run-local.ps1 -Capture -Clients 2               # two windowed clients
.\tools\run-local.ps1 -Headless -Capture                # scripted clients instead
.\tools\run-local.ps1 -Headless -Capture -Clients 2 -Detail
.\tools\run-local.ps1 -Clients 0 -Capture               # server + analyzer only
.\tools\run-local.ps1 -c -hl -n 2 -d                    # short forms of the above
```

Short forms: `-c` Capture, `-n` Clients, `-d` Detail, `-a` NoAcks, `-hl` Headless,
`-t` Duration, `-sp` ServerPort, `-cp` CapturePort, `-pb` PlayerBase, `-r` Release,
`-nb` NoBuild.

It builds first (skip with `-NoBuild`), starts the server, starts packetdump when `-Capture` is
set, launches the clients, and stops every child when the clients exit or you press Ctrl+C. By
default each client opens a game window and runs until you close it. `-Headless` runs the fixed
~20 s script instead, so keep `-Duration` above that, and prints those client logs at the end.

It prints one row per datagram; a `Data` packet may carry several messages. The decoded payload is
compact RON:

```
#000007  1.233s  C->S :52144   20 B  pkt=6     t=13        PING     net_id=0  Ping(id: 1)
#000012  1.376s  S->C :52144   38 B  pkt=2     t=130       SNAPSHOT net_id=3  ServerSnapshot(server_tick: Tick(126), acked_seq: 14, shadow_position: (0.0, 0.9, -1.237), correction: None, peers: [], resync: false)
#000013  1.409s  C->S :52144   86 B  pkt=9     t=18        INPUT    net_id=2  PlayerMovementPacket(seq: 77, player: 1, report: MovementReport(tick: Tick(90), position: (0.0, 0.9, -1.5), velocity: (0.0, 0.0, -5.5), body_yaw: 0.0, grounded: true, applied_correction_seq: 3), inputs: [InputFrame(tick: Tick(90), move_x: 0, move_z: -127, buttons: 0, camera_yaw: 0)])
```

The `:<port>` column is the client's UDP endpoint, which keeps packets from different clients
distinguishable when several are connected.

`--detail` pretty-prints the same RON with indentation. `--hex` appends a dump of the raw datagram,
`--no-acks` hides ACK-only packets, and `--dir c2s|s2c` filters one side. `--count N --stats-every 0`
makes the tool usable from a script. Short flags are `-l`, `-s`, `-n`, `-t`, `-v`, `-x`, `-a`, `-d`,
`-nc`. The two movement messages and Lightyear's `Ping`/`Pong` control messages are decoded; any
other internal message is shown as `INTERNAL` and counted.

The rate summary also reports packet-id gaps and reordering per direction, so loss shows up
without opening Wireshark:

```
stats   1.00s  |  C->S    30 pkt      3.1 KiB    30.0 pkt/s      3.1 KiB/s  |  INPUT 30/3.1 KiB  |  gaps 0  reorder 0  bad 0
               |  S->C    60 pkt      4.2 KiB    60.0 pkt/s      4.2 KiB/s  |  SNAPSHOT 60/4.2 KiB  |  gaps 0  reorder 0  bad 0
```

Because the relay sits in the path, very verbose output can slow it down. Use `--count`, `--no-acks`
or `--dir` when timing matters.

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
| `position_delta` in corrections | none | injects drift against the client's own solver; there is no replay to be deterministic about |
| client predicts hard collisions | peers do not block prediction; hard contacts arrive as velocity corrections | the client cannot see peer positions reliably, and the server has no terrain to verify them |
| declared velocity validated | position delta between reports validated | declared velocity proves nothing |
| Soft solve at 20–30 Hz, hard at 60 Hz | one solve per 60 Hz server tick | a subsystem cannot outrun the tick that drives it |
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
