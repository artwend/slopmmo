//! Headless server.
//!
//! Lightyear owns the UDP endpoint, the per-peer links, replication and
//! prediction. The server only spawns a character per connected client and runs
//! the same fixed-step simulation the clients predict.

mod transport;

use std::net::SocketAddr;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use clap::Parser;
use lightyear::connection::client::Connected;
use lightyear::input::bei::prelude::{Action, ActionOf};
use lightyear::prelude::server::*;
use lightyear::prelude::*;

use shared::protocol::*;
use shared::*;

fn main() {
    let args = Args::parse();

    // Probe the address ourselves so a bad `--listen` is still a clean
    // exit instead of a logged error from inside a lightyear observer.
    match std::net::UdpSocket::bind(args.listen) {
        Ok(socket) => drop(socket),
        Err(e) => {
            error!("cannot bind {}: {e}", args.listen);
            std::process::exit(2);
        }
    }

    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(1))))
        .add_plugins((
            TransformPlugin,
            bevy::input::InputPlugin,
            bevy::log::LogPlugin::default(),
            bevy::state::app::StatesPlugin,
        ))
        .insert_resource(ReplicationMetadata::new(SEND_INTERVAL))
        .add_plugins(ServerPlugins {
            tick_duration: TICK_DURATION,
        })
        .add_plugins(SharedPlugin)
        .add_plugins(transport::NetPlugin { addr: args.listen })
        .add_observer(handle_new_client)
        .add_observer(handle_connected)
        .add_systems(Startup, spawn_terrain);

    app.run();
}

/// A link entity is created for every remote address. `ReplicationSender`
/// enables replicating local entities to that peer.
pub(crate) fn handle_new_client(trigger: On<Add<LinkOf>>, mut commands: Commands) {
    commands
        .entity(trigger.entity)
        .insert((ReplicationSender, Name::from("Client")));
}

/// Spawn a player once the connection is confirmed, not when the first packet
/// arrives: an unvalidated connection may be rejected later.
pub(crate) fn handle_connected(
    trigger: On<Add<Connected>>,
    query: Query<&RemoteId, With<ClientOf>>,
    mut commands: Commands,
) {
    let Ok(client_id) = query.get(trigger.entity) else {
        return;
    };
    let client_id = client_id.0;
    let position = shared::initial_position(client_id);
    let player = commands
        .spawn((
            Player,
            PlayerId(client_id),
            shared::player_physics_bundle(position),
            // Replicate to everyone; the owner predicts, the rest interpolate.
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
            InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
            ControlledBy {
                owner: trigger.entity,
                lifetime: Default::default(),
            },
        ))
        .id();
    info!("created player {player:?} for client {client_id:?}");
    spawn_action_entities(&mut commands, player);
}

/// Action entities replicate with their context (`ActionOf<C>` is
/// `replicate_once`d), so the owning client can target them with input and the
/// other clients can replay the rebroadcast input.
fn spawn_action_entities(commands: &mut Commands, player: Entity) {
    commands.spawn((ActionOf::<Player>::new(player), Action::<Movement>::new()));
    commands.spawn((ActionOf::<Player>::new(player), Action::<Jump>::new()));
    commands.spawn((ActionOf::<Player>::new(player), Action::<Sprint>::new()));
}

/// Both sides collide against identical geometry, or the server would disagree
/// with every predicted move.
fn spawn_terrain(mut commands: Commands) {
    for b in shared::test_room().boxes() {
        commands.spawn(shared::terrain_bundle(b));
    }
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "server",
    about = "Headless MMO server: authoritative simulation over UDP"
)]
pub struct Args {
    /// UDP bind address
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:5888")]
    pub listen: SocketAddr,
}
