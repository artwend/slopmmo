//! Headless server.
//!
//! Two modes, both driven by the same [`Game`] tick:
//!
//! * `--bots N --duration S` — no sockets, N deterministic bots, metrics on
//!   exit. This is the CI target.
//! * `--listen 127.0.0.1:4000` — UDP through lightyear, snapshots back to clients.
//!
//! The server never owns terrain and never rewinds. See
//! `docs/architecture-review.md` for why.

mod bots;
mod transport;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use sim::authority::{SimConfig, Simulation};
use sim::movement::MoveParams;
use sim::world::StaticWorld;

/// Frame order: packets in, simulate, snapshots out.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SimSet {
    Ingest,
    Drive,
    Dispatch,
}

pub struct Game {
    pub sim: Simulation,
    pub world: StaticWorld,
    pub move_params: MoveParams,
    pub accumulator: f32,
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
            accumulator: 0.0,
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
        // `Simulation::step` owns the tick counter and writes the snapshots into
        // its own buffer, so this is the whole fixed step.
        self.sim.step(&self.world);
        self.snapshots_pending = true;
    }
}

#[derive(Resource)]
pub struct GameRes(pub Game);

fn main() {
    let args = Args::parse();

    if args.bots > 0 {
        run_bot_benchmark(args.bots, args.duration);
        return;
    }

    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(1))))
        .add_plugins(bevy::log::LogPlugin::default())
        .configure_sets(
            Update,
            (SimSet::Ingest, SimSet::Drive, SimSet::Dispatch).chain(),
        )
        .insert_resource(GameRes(Game::new()))
        .add_systems(Update, drive.in_set(SimSet::Drive));

    match args.listen_addr() {
        Some(addr) => {
            // Probe the address ourselves so a bad `--listen` is still a clean
            // exit instead of a logged error from inside a lightyear observer.
            match std::net::UdpSocket::bind(addr) {
                Ok(socket) => drop(socket),
                Err(e) => {
                    error!("cannot bind {addr}: {e}");
                    std::process::exit(2);
                }
            }
            app.add_plugins(transport::NetPlugin { addr });
        }
        None => info!("no socket, tick rate {} Hz", proto::TICK_HZ),
    }

    app.run();
}

fn drive(time: Res<Time>, mut game: ResMut<GameRes>) {
    // No sleeping in here: the schedule runner owns the frame pacing (see
    // `ScheduleRunnerPlugin::run_loop` in `main`), so this system either does
    // work or returns immediately.
    game.0.advance(time.delta_secs(), 8);
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
        // `step` advances the tick; hand the bots the tick they are reporting
        // for, exactly as the old driver did.
        let tick = game.sim.tick.add(1);

        for bot in roster.iter_mut() {
            let input = bot.input(tick, &mut rng);
            let (pos, vel) = bot.predict(&game.world, &game.move_params, &input, proto::TICK_DT);
            bot.pos = pos;
            bot.vel = vel;
            bot.grounded = vel.y <= 0.0;

            let packet = proto::PlayerMovementPacket {
                seq: (tick.0 & 0xFFFF) as u16,
                player: bot.id,
                report: proto::MovementReport {
                    tick,
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
    /// Parses `--listen` into an address, exiting with code 2 if it is unusable.
    fn listen_addr(&self) -> Option<SocketAddr> {
        let raw = self.listen.as_ref()?;
        match raw.parse() {
            Ok(addr) => Some(addr),
            Err(e) => {
                eprintln!("invalid --listen address {raw:?}: {e}");
                std::process::exit(2);
            }
        }
    }

    fn parse() -> Self {
        let mut out = Self {
            bots: 0,
            duration: 10.0,
            listen: Some("127.0.0.1:4000".to_string()),
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
                         \n  --listen ADDR   UDP bind address (default 127.0.0.1:4000)\n"
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
        assert!(a.sim.tick.0 >= 59, "too few ticks: {}", a.sim.tick.0);
        // f32 accumulation can land one tick either side of the exact second.
        assert!(
            a.sim.tick.0.abs_diff(b.sim.tick.0) <= 1,
            "accumulator drifted: a={} b={}",
            a.sim.tick.0,
            b.sim.tick.0
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
