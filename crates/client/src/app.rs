//! Windowed client. WASD + Q/E; Avian moves the virtual character once per fixed tick.

use bevy::prelude::*;
use kcc_avian::prelude::{MoveAndSlidePlugin, PhysicsPlugins};
use lightyear::input::bei::prelude::{preset, Action, ActionOf, Actions, Bindings, Fire};
use lightyear::prediction::rollback::DeterministicPredicted;
use lightyear::prelude::client::{InputDelayConfig, InputTimelineConfig};
use lightyear::prelude::*;

use shared::protocol::*;
use crate::character::{self, Character};
use crate::transport::{ClientNetPlugin, NetSet};
use crate::{Args, Local, WorldRes};

/// Camera turn speed, rad/s. Reported as angular velocity so the shadow can
/// extrapolate the body yaw between reports.
const TURN_RATE: f32 = 2.5;

#[derive(Resource, Default)]
struct PlayerRes {
}

pub fn run(args: &Args) {
    let world = sim::StaticWorld::test_room();
    let mut app = App::new();

    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "mmo — client".into(),
            resolution: (1280u32, 720u32).into(),
            ..default()
        }),
        ..default()
    }))
    // `MoveAndSlidePlugin` must come after `PhysicsPlugins`: it orders itself
    // inside the physics step sets those plugins create.
    .add_plugins((
        PhysicsPlugins::new(FixedPostUpdate),
        MoveAndSlidePlugin::default(),
    ))
    .insert_resource(Time::<Fixed>::from_hz(proto::CLIENT_TICK_HZ as f64))
    .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.07)))
    .insert_resource(WorldRes(world))
    .init_resource::<Local>()
    .init_resource::<PlayerRes>()
    .add_systems(Startup, (setup, configure_input_delay))
    .add_observer(add_bindings_to_controlled_action)
    .add_observer(add_bindings_to_controlled_player_actions)
    .add_observer(player_movement)
    .add_systems(Update, follow.after(NetSet::Receive));

    if let Some(addr) = args.connect {
        app.add_plugins(ClientNetPlugin {
            server: addr,
            player_id: args.player_id,
        });
    }

    app.run();
}

fn configure_input_delay(mut commands: Commands) {
    commands.insert_resource(
        InputTimelineConfig::default().with_input_delay(InputDelayConfig::balanced()),
    );
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    world: Res<WorldRes>,
) {
    let stone = materials.add(StandardMaterial {
        base_color: Color::srgb(0.32, 0.34, 0.38),
        perceptual_roughness: 0.85,
        ..default()
    });
    let pillar = materials.add(StandardMaterial {
        base_color: Color::srgb(0.55, 0.42, 0.30),
        ..default()
    });

    // The rendered boxes are the colliders; each mesh is built at the box size,
    // so no `Transform` scale has to be mirrored into the collider.
    for (i, b) in world.0.boxes().iter().enumerate() {
        let size = b.max - b.min;
        let centre = (b.max + b.min) * 0.5;
        commands.spawn((
            Mesh3d(meshes.add(Cuboid::new(size.x, size.y, size.z))),
            MeshMaterial3d(if i == world.0.len() - 1 {
                pillar.clone()
            } else {
                stone.clone()
            }),
            Transform::from_translation(Vec3::from(centre)),
            character::terrain_bundle(b),
        ));
    }

    // Ambient light is a camera component in 0.20, not a resource.
    commands.spawn((
        Camera3d::default(),
        AmbientLight {
            color: Color::WHITE,
            brightness: 320.0,
            ..default()
        },
        Transform::from_xyz(0.0, 6.0, 12.0).looking_at(Vec3::new(0.0, 0.9, 0.0), Vec3::Y),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 14_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(6.0, 12.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // The capsule mesh rides on the virtual character itself; the controller
    // writes its `Transform` every fixed step.
    let player = commands
        .spawn((
            character::player_bundle(character::spawn_position()),
            Mesh3d(meshes.add(Capsule3d::new(
                proto::PLAYER_RADIUS,
                character::CAPSULE_LENGTH,
            ))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.30, 0.65, 0.95),
                ..default()
            })),
        ))
        .id();
    commands.insert_resource(Character(player));
}

/// Applies local movement only to predicted entities owned by this client.
/// In host-server mode the server observer runs in the same app, so the client
/// observer is disabled.
fn player_movement(
    trigger: On<Fire<Movement>>,
    _input_timeline: SyncedLocalTimeline,
    mut position_query: Query<
        &mut ReplicablePosition,
        Or<(With<Predicted>, With<DeterministicPredicted>)>,
    >,
) {
    if let Ok(position) = position_query.get_mut(trigger.context) {
        // Pass Mut<ReplicablePosition> directly so change detection only fires when movement changes it.
        shared::shared_movement_behaviour(position, trigger.value);
    }
}

/// Local WASD bindings for a movement action entity.
fn key_bindings() -> impl Scene {
    bsn! {
        Action::<Movement>::new()
        Bindings [ 
            {preset::cardinal::wasd_keys()}
            --
            {preset::axial::left_stick()}
        ]
        --
        Action::<Jump>::new()
        Bindings [
            Binding(KeyCode::Space)
            --
            Binding(GamepadButton::South)
        ]
    }
}

/// Add local movement bindings once the replicated action entity is fully
/// ready for a player that we control.
fn add_bindings_to_controlled_action(
    trigger: On<Insert<(Action<Movement>, ActionOf<Player>)>>,
    actions: Query<&ActionOf<Player>, (With<Action<Movement>>, Without<Bindings>)>,
    controlled_players: Query<(), (With<Player>, With<Controlled>)>,
    mut commands: Commands,
) {
    let Ok(action_of) = actions.get(trigger.entity) else {
        return;
    };
    if controlled_players.get(action_of.get()).is_err() {
        return;
    }
    commands.entity(trigger.entity).apply_scene(key_bindings());
}

/// Add local movement bindings if the controlled player becomes ready after
/// its action entities.
fn add_bindings_to_controlled_player_actions(
    trigger: On<Add<(Player, Controlled, Actions<Player>)>>,
    players: Query<&Actions<Player>, (With<Player>, With<Controlled>)>,
    actions: Query<(), (With<Action<Movement>>, Without<Bindings>)>,
    mut commands: Commands,
) {
    let Ok(player_actions) = players.get(trigger.entity) else {
        return;
    };
    for action in player_actions.iter() {
        if actions.get(action).is_err() {
            continue;
        }
        commands.entity(action).apply_scene(key_bindings());
    }
}

fn follow(local: Res<Local>, mut q: Query<&mut Transform, With<Camera3d>>) {
    let rotated = Quat::from_rotation_y(local.player.yaw) * Vec3::new(0.0, 6.0, 9.0);
    let eye = local.player.pos + rotated;
    for mut t in &mut q {
        t.translation = eye;
        t.look_at(local.player.pos + Vec3::Y * 0.6, Vec3::Y);
    }
}
