//! Windowed client. WASD + Q/E; Avian moves the virtual character once per fixed tick.

use bevy::prelude::*;
use kcc_avian::prelude::{MoveAndSlidePlugin, PhysicsPlugins};

use crate::character::{self, Character};
use crate::transport::{ClientNetPlugin, NetSet};
use crate::{Args, Local, WorldRes};

#[derive(Resource, Default)]
struct PlayerRes {
    input: proto::InputFrame,
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
    .add_systems(Startup, setup)
    .add_systems(
        FixedUpdate,
        (sample_input, tick_local, character::apply_local).chain(),
    )
    .add_systems(FixedLast, character::read_back)
    .add_systems(Update, follow.after(NetSet::Receive));

    if let Some(addr) = args.connect {
        app.add_plugins(ClientNetPlugin {
            server: addr,
            player_id: args.player_id,
        });
    }

    app.run();
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
            Transform::from_translation(centre),
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

fn sample_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut res: ResMut<PlayerRes>,
    mut local: ResMut<Local>,
    time: Res<Time>,
) {
    let axis = |pos: KeyCode, neg: KeyCode| -> f32 {
        let p = keys.pressed(pos) as i32;
        let n = keys.pressed(neg) as i32;
        (p - n) as f32
    };
    let x = axis(KeyCode::KeyD, KeyCode::KeyA);
    let z = axis(KeyCode::KeyS, KeyCode::KeyW);

    let mut buttons = 0u8;
    if keys.pressed(KeyCode::Space) {
        buttons |= proto::buttons::JUMP;
    }
    if keys.pressed(KeyCode::ShiftLeft) {
        buttons |= proto::buttons::SPRINT;
    }

    let turn = (keys.pressed(KeyCode::KeyE) as i32 - keys.pressed(KeyCode::KeyQ) as i32) as f32;
    local.player.yaw -= turn * 2.5 * time.delta_secs();

    res.input = proto::InputFrame {
        move_x: (x * 127.0).clamp(-127.0, 127.0) as i8,
        move_z: (z * 127.0).clamp(-127.0, 127.0) as i8,
        buttons,
        camera_yaw: proto::quantise_yaw(local.player.yaw),
        ..default()
    };
}

fn tick_local(mut local: ResMut<Local>, res: Res<PlayerRes>) {
    local.step(res.input);
}

fn follow(local: Res<Local>, mut q: Query<&mut Transform, With<Camera3d>>) {
    let rotated = Quat::from_rotation_y(local.player.yaw) * Vec3::new(0.0, 6.0, 9.0);
    let eye = local.player.pos + rotated;
    for mut t in &mut q {
        t.translation = eye;
        t.look_at(local.player.pos + Vec3::Y * 0.6, Vec3::Y);
    }
}
