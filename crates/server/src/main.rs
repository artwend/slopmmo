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

use shared::protocol::*;
use shared::*;
use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy_enhanced_input::prelude::{Action, ActionOf, Fire};
use clap::Parser;
use lightyear::connection::client::Connected;
use lightyear::prelude::server::*;
use lightyear::prelude::*;

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

fn main() {
    let args = Args::parse();

    if args.bots > 0 {
        run_bot_benchmark(args.bots, args.duration);
        return;
    }

    let addr = args.listen;

    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(1))));
    app.insert_resource(ReplicationMetadata::new(SEND_INTERVAL));
    // the physics/FixedUpdates systems that consume inputs should be run in this set.
    app.add_observer(movement);
    app.add_observer(handle_new_client);
    app.add_observer(handle_connected);

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

/// When a new client tries to connect to a server, an entity is created for it with the `LinkOf` component.
/// This entity represents the link between the server and that client.
///
/// You can add additional components to update the link. In this case we will add a `ReplicationSender` that
/// will enable us to replicate local entities to that client.
pub(crate) fn handle_new_client(trigger: On<Add<LinkOf>>, mut commands: Commands) {
    commands
        .entity(trigger.entity)
        .insert((ReplicationSender, Name::from("Client")));
}

/// If the new client connects to the server, we want to spawn a new player entity for it.
///
/// We have to react specifically on `Connected` because there is no guarantee that the connection request we
/// received was valid. The server could reject the connection attempt for many reasons (server is full, packet is invalid,
/// DDoS attempt, etc.). We want to start the replication only when the client is confirmed as connected.
pub(crate) fn handle_connected(
    trigger: On<Add<Connected>>,
    query: Query<&RemoteId, With<ClientOf>>,
    mut commands: Commands,
) {
    let Ok(client_id) = query.get(trigger.entity) else {
        return;
    };
    let client_id = client_id.0;
    let player_entity = commands
        .spawn((
            // Add the context component on the server; it will be replicated to the client
            Player,
            PlayerId(client_id),
            ReplicablePosition(shared::initial_player_position(client_id)),
            // we replicate the Player entity to all clients that are connected to this server
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
            InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
            ControlledBy {
                owner: trigger.entity,
                lifetime: Default::default(),
            },
        ))
        .id();
    info!(
        "Create player entity {:?} for client {:?}",
        player_entity, client_id
    );
    spawn_action_entities(&mut commands, player_entity);
}

/// Spawn the BEI action entities for a player.
///
/// The server owns and replicates action entities so the owning client can
/// target them in input messages and remote clients can receive rebroadcasted
/// input for prediction.
///
/// `InputPlugin<C>` installs `HierarchySendPlugin<ActionOf<C>>`, which makes
/// each action entity replicate like its context entity. The action therefore
/// does not need its own `Replicate` component.
fn spawn_action_entities(commands: &mut Commands, player_entity: Entity) {
    commands.spawn((
        ActionOf::<Player>::new(player_entity),
        Action::<Movement>::new(),
    ));
}

/// Applies received client inputs on the server so other clients can observe the result.
fn movement(trigger: On<Fire<Movement>>, mut position_query: Query<&mut ReplicablePosition>) {
    if let Ok(position) = position_query.get_mut(trigger.context) {
        shared::shared_movement_behaviour(position, trigger.value);
    }
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
                version: proto::PROTOCOL_VERSION,
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

    fn correction(seq: u16, dv: f32) -> proto::CollisionCorrection {
        proto::CollisionCorrection {
            seq,
            tick: proto::Tick::new(9),
            epoch: 1,
            velocity_delta: [dv, 0.0, 0.0],
            hard_normal: None,
        }
    }

    #[test]
    fn a_later_tick_without_a_correction_keeps_the_unsent_one() {
        let mut merged = proto::ServerSnapshot {
            server_tick: proto::Tick::new(9),
            correction: Some(correction(3, 1.0)),
            ..Default::default()
        };
        let next = proto::ServerSnapshot {
            server_tick: proto::Tick::new(10),
            ..Default::default()
        };
        merge_snapshot(&mut merged, &next);

        let kept = merged
            .correction
            .expect("the frame dropped a correction that was never sent");
        assert_eq!(kept.seq, 3);
        assert_eq!(merged.server_tick, proto::Tick::new(10));
    }

    #[test]
    fn two_ticks_of_contact_leave_as_one_accumulated_correction() {
        let mut merged = proto::ServerSnapshot {
            correction: Some(correction(3, 1.0)),
            ..Default::default()
        };
        let mut next = proto::ServerSnapshot {
            correction: Some(proto::CollisionCorrection {
                hard_normal: Some([1.0, 0.0, 0.0]),
                ..correction(4, 0.5)
            }),
            ..Default::default()
        };
        next.resync = true;
        merge_snapshot(&mut merged, &next);

        let c = merged.correction.expect("correction lost");
        assert_eq!(c.seq, 4, "the newer sequence number must win");
        assert!(
            (c.velocity_delta[0] - 1.5).abs() < 1e-6,
            "deltas were not summed"
        );
        assert_eq!(c.hard_normal, Some([1.0, 0.0, 0.0]));
        assert!(
            merged.resync,
            "resync is sticky and must come from the newest tick"
        );
    }

    #[test]
    fn a_multi_tick_frame_keeps_a_snapshot_for_every_player() {
        let mut game = Game::new();
        game.sim.add_player(1, Vec3::new(0.0, 0.9, 0.0));
        game.sim.add_player(2, Vec3::new(0.3, 0.9, 0.0));
        let step = proto::SERVER_TICK_DT;
        game.advance(step * 2.5, 8);

        assert_eq!(game.pending().len(), 2, "a player lost its snapshot");
        assert_eq!(game.pending()[0].0, 1);
        assert_eq!(game.pending()[1].0, 2);

        game.clear_pending();
        assert!(game.pending().is_empty());
    }
}
