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
use clap::Parser;

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
        let step = proto::SERVER_TICK_DT;
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

    let addr = args.listen;

    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(1))))
        .add_plugins(bevy::log::LogPlugin::default())
        .configure_sets(
            Update,
            (SimSet::Ingest, SimSet::Drive, SimSet::Dispatch).chain(),
        )
        .insert_resource(GameRes(Game::new()))
        .add_systems(Update, drive.in_set(SimSet::Drive));

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

    let total_ticks = (duration_s * proto::SERVER_TICK_HZ as f32).round() as u64;
    // Bots predict like a client and report once per server tick, tick-stamped in
    // the client's own domain. The real client reports at `INPUT_SEND_HZ`; the
    // benchmark sends every tick so the shadow is always fresh. Reporting server
    // ticks as if they were client ticks would look like a clock running at the
    // wrong rate and (correctly) be rejected as stale.
    let client_steps = (proto::CLIENT_TICK_HZ / proto::SERVER_TICK_HZ).max(1);
    let mut client_tick = proto::Tick::ZERO;
    let start = Instant::now();
    let mut solve_ns: u128 = 0;

    for _ in 0..total_ticks {
        for _ in 0..client_steps {
            client_tick = client_tick.add(1);
            for bot in roster.iter_mut() {
                let input = bot.input(client_tick, &mut rng);
                let (pos, vel) = bot.predict(
                    &game.world,
                    &game.move_params,
                    &input,
                    proto::CLIENT_TICK_DT,
                );
                bot.pos = pos;
                bot.vel = vel;
                bot.grounded = vel.y <= 0.0;
            }
        }

        for bot in roster.iter_mut() {
            let packet = proto::PlayerMovementPacket {
                seq: (client_tick.0 & 0xFFFF) as u16,
                player: bot.id,
                report: proto::MovementReport {
                    tick: client_tick,
                    position: bot.pos.to_array(),
                    linear_velocity: bot.vel.to_array(),
                    angular_velocity: [0.0; 3],
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

    let budget_ms = total_ticks as f64 * 1000.0 / proto::SERVER_TICK_HZ as f64;
    println!(
        "headroom_x      {:.1}",
        budget_ms / (elapsed.as_secs_f64() * 1000.0).max(0.001)
    );
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "server",
    about = "Headless MMO server: deterministic bot benchmark or a UDP game loop"
)]
pub struct Args {
    /// run N deterministic bots, no sockets, print metrics
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub bots: usize,
    /// bot benchmark length in seconds
    #[arg(long, value_name = "S", default_value_t = 10.0)]
    pub duration: f32,
    /// UDP bind address
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:4000")]
    pub listen: SocketAddr,
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
        // The server tick is 60 Hz, so one second is sixty steps.
        assert!(
            (59..=61).contains(&a.sim.tick.0),
            "wrong number of server ticks: {}",
            a.sim.tick.0
        );
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
