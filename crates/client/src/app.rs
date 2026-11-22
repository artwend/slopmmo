//! Windowed client. WASD + Q/E, one local KCC solve per fixed tick.

use bevy::prelude::*;

use crate::transport::{ClientNetPlugin, NetSet};
use crate::{Args, Local, WorldRes};

#[derive(Resource, Default)]
struct PlayerRes {
    accumulator: f32,
    input: proto::InputFrame,
}

#[derive(Component)]
struct Avatar;

pub fn run(args: &Args) {
    let mut app = App::new();

    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "mmo — client".into(),
            resolution: (1280u32, 720u32).into(),
            ..default()
        }),
        ..default()
    }))
    .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.07)))
    .insert_resource(WorldRes(sim::StaticWorld::test_room()))
    .init_resource::<Local>()
    .init_resource::<PlayerRes>()
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (sample_input, simulate, sync_avatar, follow)
            .chain()
            // Corrections land before the solve, reports leave after it.
            .after(NetSet::Receive)
            .before(NetSet::Send),
    );

    if let Some(addr) = args.connect_addr() {
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

    let unit = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    for (i, b) in world.0.boxes().iter().enumerate() {
        let size = b.max - b.min;
        let centre = (b.max + b.min) * 0.5;
        commands.spawn((
            Mesh3d(unit.clone()),
            MeshMaterial3d(if i == world.0.len() - 1 {
                pillar.clone()
            } else {
                stone.clone()
            }),
            Transform::from_translation(centre).with_scale(size),
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

    // The avatar is only a stand-in for the capsule; the KCC does the real work.
    commands.spawn((
        Avatar,
        Mesh3d(meshes.add(Capsule3d::new(
            proto::PLAYER_RADIUS,
            proto::PLAYER_HEIGHT - proto::PLAYER_RADIUS * 2.0,
        ))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.30, 0.65, 0.95),
            ..default()
        })),
        Transform::from_xyz(0.0, 0.9, 0.0),
    ));
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

fn simulate(
    time: Res<Time>,
    world: Res<WorldRes>,
    mut local: ResMut<Local>,
    mut res: ResMut<PlayerRes>,
) {
    res.accumulator += time.delta_secs();
    let mut steps = 0;
    while res.accumulator >= proto::CLIENT_TICK_DT && steps < 8 {
        local.step(&world.0, res.input);
        res.accumulator -= proto::CLIENT_TICK_DT;
        steps += 1;
    }
    if steps == 8 {
        res.accumulator = 0.0;
    }
}

fn sync_avatar(mut q: Query<&mut Transform, With<Avatar>>, local: Res<Local>) {
    for mut t in &mut q {
        t.translation = local.player.pos;
        t.rotation = Quat::from_rotation_y(local.player.yaw);
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
