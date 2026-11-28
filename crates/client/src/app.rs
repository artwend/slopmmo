//! Rendered client. WASD + Space + Shift; the Avian character controller is the
//! predicted simulation, so windowed play and the server run the same code.

use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::client::{InputDelayConfig, InputTimelineConfig};
use lightyear::prelude::*;

use shared::*;

use crate::character;
use crate::transport::ClientNetPlugin;
use crate::{Args, bindings};

pub fn run(args: &Args) {
    let mut app = App::new();
    let fixed_time = app.world().resource::<Time<Fixed>>();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "mmo — client".into(),
            resolution: (1280u32, 720u32).into(),
            ..default()
        }),
        ..default()
    }))
    .add_plugins(ClientPlugins {
        tick_duration: TICK_DURATION,
    })
    .add_plugins(SharedPlugin)
    .insert_resource(PredictionManager::default())
    .add_systems(
        Startup,
        (
            character::setup_scene,
            character::spawn_terrain,
            configure_input_delay,
        ),
    )
    .add_observer(character::spawn_predicted)
    .add_observer(character::spawn_interpolated)
    .add_systems(Update, character::follow_controlled);

    bindings::register(&mut app);

    if let Some(addr) = args.connect {
        app.add_plugins(ClientNetPlugin { server: addr });
    }

    app.run();
}

fn configure_input_delay(mut commands: Commands) {
    // A little input delay keeps a small buffer for jittery uplinks while
    // keeping the local character responsive.
    commands.insert_resource(
        InputTimelineConfig::default().with_input_delay(InputDelayConfig::balanced()),
    );
}
