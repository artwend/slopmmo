//! Headless server.
//!
//! Two modes, both driven by the same [`crate::Game`] tick:
//!
//! * `--bots N --duration S` — no sockets, N deterministic bots, metrics on
//!   exit. This is the CI target.
//! * `--listen 0.0.0.0:4000` — UDP, real packets, snapshots back to clients.
//!
//! The server never owns terrain and never rewinds. See
//! `docs/architecture-review.md` for why.

mod bots;
mod net;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use bevy::prelude::*;

use sim::authority::{SimConfig, Simulation};
use sim::movement::MoveParams;
use sim::world::StaticWorld;

pub struct Game {
    pub sim: Simulation,
    pub world: StaticWorld,
    pub move_params: MoveParams,
    pub tick: proto::Tick,
    pub accumulator: f32,
    /// `(player, snapshot)` from the most recent completed tick.
    pub last_snapshots: Vec<(u64, proto::ServerSnapshot)>,
    /// Set by a tick, cleared once the snapshots have been sent. Without this the
    /// network system would re-send the same snapshot on every frame.
    pub snapshots_pending: bool,
}

impl Default for Game {
    fn default() -> Self {
        Self::new()
    }
}

impl Game {
    pub fn new() -> Self {
        Self {
            sim: Simulation::new(SimConfig::default()),
            world: StaticWorld::test_room(),
            move_params: MoveParams::default(),
            tick: proto::Tick::ZERO,
            accumulator: 0.0,
            last_snapshots: Vec::new(),
            snapshots_pending: false,
        }
    }

    /// Advances at most `max_ticks` fixed steps for `delta` seconds of wall time.
    pub fn advance(&mut self, delta: f32, max_ticks: u32) -> u32 {
        self.accumulator += delta;
        let step = proto::TICK_DT;
        let mut ran = 0;
        while self.accumulator >= step && ran < max_ticks {
            self.tick_once();
            self.accumulator -= step;
            ran += 1;
        }
        if ran == max_ticks && self.accumulator >= step {
            // Shed the backlog instead of spiralling into a slow-motion world.
            self.accumulator = 0.0;
        }
        ran
    }

    fn tick_once(&mut self) {
        self.tick = self.tick.add(1);
        let ids: Vec<u64> = self.sim.players.keys().copied().collect();
        let snaps = self.sim.step(&self.world);
        self.last_snapshots.clear();
        self.last_snapshots
            .extend(snaps.into_iter().zip(ids).map(|(s, id)| (id, s)));
        self.snapshots_pending = true;
    }
}

#[derive(Resource)]
struct GameRes(Game);

/// Socket plus the address map we need to reply to anonymous UDP senders.
#[derive(Resource)]
struct NetRes {
    server: net::UdpServer,
    peers: HashMap<u64, SocketAddr>,
    /// Last packet seen per peer. UDP has no disconnect, so silence is the only
    /// signal that a client is gone.
    last_seen: HashMap<u64, Instant>,
}

/// A peer that has been silent this long is dropped, along with its shadow body.
const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn main() {
    let args = Args::parse();

    if args.bots > 0 {
        run_bot_benchmark(args.bots, args.duration);
        return;
    }

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(bevy::log::LogPlugin::default())
        .insert_resource(GameRes(Game::new()))
        .add_systems(Update, drive);

    if let Some(addr) = args.listen.clone() {
        match net::UdpServer::bind(&addr) {
            Ok(server) => {
                let bound = server
                    .local_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| addr.clone());
                info!("listening on {bound}, tick rate {} Hz", proto::TICK_HZ);
                app.insert_resource(NetRes {
                    server,
                    peers: HashMap::new(),
                    last_seen: HashMap::new(),
                })
                .add_systems(Update, pump_net.after(drive));
            }
            Err(e) => {
                error!("cannot bind {addr}: {e}");
                std::process::exit(2);
            }
        }
    } else {
        info!("no socket, tick rate {} Hz", proto::TICK_HZ);
    }

    app.run();
}

fn drive(time: Res<Time>, mut game: ResMut<GameRes>) {
    let ran = game.0.advance(time.delta_secs(), 8);
    if ran == 0 {
        // Let the runner idle instead of spinning on an empty frame.
        std::thread::sleep(std::time::Duration::from_micros(500));
    }
}

fn pump_net(mut net: ResMut<NetRes>, mut game: ResMut<GameRes>) {
    let incoming = net.server.drain();
    for (from, pkt) in incoming {
        // First contact: admit the peer and give it a shadow body at the pose it
        // reports. A real deployment authenticates the session and spawns at a
        // zone-defined point instead of trusting the client's first position.
        if !game.0.sim.players.contains_key(&pkt.player) {
            let pos = Vec3::from_array(pkt.report.position);
            game.0.sim.add_player(pkt.player, pos);
            info!("admitted player {} from {from}", pkt.player);
        }
        // Always track the newest address: a NAT rebind must not orphan the peer.
        net.peers.insert(pkt.player, from);
        net.last_seen.insert(pkt.player, Instant::now());
        if let Err(reject) = game.0.sim.ingest(&pkt)
            && reject != sim::authority::Reject::DuplicateSeq
        {
            info!("rejected packet from {from}: {reject:?}");
        }
    }

    // A silent peer is gone. Dropping its sequence state as well as its address
    // means a reconnect is not mistaken for a stale or replayed packet.
    let now = Instant::now();
    let expired: Vec<u64> = net
        .last_seen
        .iter()
        .filter(|(_, seen)| now.duration_since(**seen) > PEER_TIMEOUT)
        .map(|(id, _)| *id)
        .collect();
    let gone: Vec<u64> = net
        .peers
        .keys()
        .copied()
        .filter(|id| !game.0.sim.players.contains_key(id))
        .collect();
    for id in expired.into_iter().chain(gone) {
        net.peers.remove(&id);
        net.last_seen.remove(&id);
        net.server.forget(id);
        game.0.sim.remove_player(id);
        info!("dropped player {id}");
    }

    if net.peers.is_empty() || !game.0.snapshots_pending {
        return;
    }

    // Send whatever the most recent completed tick produced, exactly once.
    for (id, snapshot) in &game.0.last_snapshots {
        let addr = net.peers.get(id).copied();
        if let Some(addr) = addr {
            net.server.send(addr, snapshot);
        }
    }
    game.0.snapshots_pending = false;
}

/// Deterministic N-bot soak test. Prints a machine-readable summary.
fn run_bot_benchmark(bot_count: usize, duration_s: f32) {
    let mut game = Game::new();
    let mut rng = bots::Lcg::new(0x5EED);
    let mut roster: Vec<bots::Bot> = (0..bot_count as u64)
        .map(|i| {
            let b = bots::Bot::spawn(i + 1, &mut rng);
            game.sim.add_player(i + 1, b.pos);
            b
        })
        .collect();

    let total_ticks = (duration_s * proto::TICK_HZ as f32).round() as u64;
    let start = Instant::now();
    let mut solve_ns: u128 = 0;

    for _ in 0..total_ticks {
        game.tick = game.tick.add(1);

        for bot in roster.iter_mut() {
            let input = bot.input(game.tick, &mut rng);
            let (pos, vel) = bot.predict(&game.world, &game.move_params, &input, proto::TICK_DT);
            bot.pos = pos;
            bot.vel = vel;
            bot.grounded = vel.y <= 0.0;

            let packet = proto::PlayerMovementPacket {
                seq: (game.tick.0 & 0xFFFF) as u16,
                player: bot.id,
                report: proto::MovementReport {
                    tick: game.tick,
                    position: pos.to_array(),
                    velocity: vel.to_array(),
                    body_yaw: 0.0,
                    grounded: bot.grounded,
                    applied_correction_seq: 0,
                },
                inputs: Vec::new(),
            };
            let _ = game.sim.ingest(&packet);
        }

        let t0 = Instant::now();
        game.sim.step(&game.world);
        solve_ns += t0.elapsed().as_nanos();
    }

    let elapsed = start.elapsed();
    let stats = game.sim.stats;
    println!("bots            {bot_count}");
    println!("ticks           {total_ticks}");
    println!("wall_ms         {:.2}", elapsed.as_secs_f64() * 1000.0);
    println!(
        "ms_per_tick     {:.3}",
        elapsed.as_secs_f64() * 1000.0 / total_ticks.max(1) as f64
    );
    println!(
        "solve_us_per_tick {:.1}",
        solve_ns as f64 / 1000.0 / total_ticks.max(1) as f64
    );
    println!("pairs_last_tick {}", stats.pairs);
    println!("corrections     {}", stats.corrections);
    println!("hard_contacts   {}", stats.hard_contacts);
    println!("accepted        {}", stats.accepted);
    println!("rejected_dup    {}", stats.duplicate);
    println!("rejected_stale  {}", stats.stale);
    println!("rejected_fast   {}", stats.too_fast);
    println!("rejected_tp     {}", stats.teleport);

    let budget_ms = total_ticks as f64 * 1000.0 / proto::TICK_HZ as f64;
    println!(
        "headroom_x      {:.1}",
        budget_ms / (elapsed.as_secs_f64() * 1000.0).max(0.001)
    );
}

#[derive(Clone, Debug)]
pub struct Args {
    pub bots: usize,
    pub duration: f32,
    pub listen: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut out = Self {
            bots: 0,
            duration: 10.0,
            listen: Some("0.0.0.0:4000".to_string()),
        };
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--bots" => out.bots = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                "--duration" => {
                    out.duration = it.next().and_then(|v| v.parse().ok()).unwrap_or(10.0)
                }
                "--listen" => out.listen = it.next(),
                "--help" | "-h" => {
                    println!(
                        "usage: server [--bots N --duration S] [--listen ADDR]\n\
                         \n  --bots N        run N deterministic bots, no sockets, print metrics\n\
                         \n  --duration S    bot benchmark length in seconds (default 10)\n\
                         \n  --listen ADDR   UDP bind address (default 0.0.0.0:4000)\n"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("unknown argument: {other}"),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_is_tick_rate_independent() {
        let mut a = Game::new();
        let mut b = Game::new();
        // Same one second of wall time, delivered at 60 Hz and at 6 Hz. The step
        // cap must be generous enough that neither path sheds backlog, or this
        // would be testing the cap instead of the accumulator.
        for _ in 0..60 {
            a.advance(1.0 / 60.0, 16);
        }
        for _ in 0..6 {
            b.advance(1.0 / 6.0, 16);
        }
        assert!(a.tick.0 >= 59, "too few ticks: {}", a.tick.0);
        // f32 accumulation can land one tick either side of the exact second.
        assert!(
            a.tick.0.abs_diff(b.tick.0) <= 1,
            "accumulator drifted: a={} b={}",
            a.tick.0,
            b.tick.0
        );
    }

    #[test]
    fn backlog_is_shed_instead_of_spiralling() {
        let mut g = Game::new();
        let ran = g.advance(5.0, 8);
        assert_eq!(ran, 8);
        assert_eq!(g.accumulator, 0.0);
    }
}
