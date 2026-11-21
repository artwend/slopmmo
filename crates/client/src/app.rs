//! Windowed client. WASD + Q/E, one local KCC solve per fixed tick.

use bevy::prelude::*;

use crate::{Args, LocalPlayer};

/// Shared world geometry for both the local solve and the rendered boxes.
#[derive(Resource)]
struct WorldRes(sim::StaticWorld);

#[derive(Resource)]
struct PlayerRes {
    player: LocalPlayer,
    accumulator: f32,
    input: proto::InputFrame,
    tick: proto::Tick,
    seq: u16,
    /// Last few inputs, re-sent for redundancy on a lossy link.
    recent: Vec<proto::InputFrame>,
    /// Network client, when `--connect` was supplied.
    net: Option<crate::net::ClientLink>,
    corrections: crate::CorrectionLog,
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
    .insert_resource(PlayerRes {
        player: LocalPlayer::spawn(),
        accumulator: 0.0,
        input: proto::InputFrame::default(),
        tick: proto::Tick::ZERO,
        seq: 0,
        recent: Vec::with_capacity(proto::MAX_INPUT_FRAMES),
        net: args.connect.as_ref().and_then(|a| {
            match crate::net::ClientLink::connect(a, args.player_id) {
                Ok(link) => {
                    println!("linked to {} as player {}", link.peer(), args.player_id);
                    Some(link)
                }
                Err(e) => {
                    eprintln!("connect to {a} failed: {e}");
                    None
                }
            }
        }),
        corrections: crate::CorrectionLog::default(),
    })
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (
            sample_input,
            simulate,
            uplink,
            sync_avatar,
            follow,
            pump_net,
        )
            .chain(),
    );

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

fn sample_input(keys: Res<ButtonInput<KeyCode>>, mut res: ResMut<PlayerRes>, time: Res<Time>) {
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
    res.player.yaw -= turn * 2.5 * time.delta_secs();

    res.input = proto::InputFrame {
        tick: res.tick,
        move_x: (x * 127.0).clamp(-127.0, 127.0) as i8,
        move_z: (z * 127.0).clamp(-127.0, 127.0) as i8,
        buttons,
        camera_yaw: proto::quantise_yaw(res.player.yaw),
    };
}

fn simulate(time: Res<Time>, world: Res<WorldRes>, mut res: ResMut<PlayerRes>) {
    res.accumulator += time.delta_secs();
    let mut steps = 0;
    while res.accumulator >= proto::TICK_DT && steps < 8 {
        res.tick = res.tick.add(1);
        let input = res.input;
        res.player.tick(&world.0, &input, proto::TICK_DT);
        res.recent.push(input);
        if res.recent.len() > proto::MAX_INPUT_FRAMES {
            res.recent.remove(0);
        }
        res.accumulator -= proto::TICK_DT;
        steps += 1;
    }
    if steps == 8 {
        res.accumulator = 0.0;
    }
}

/// Sends a report at `INPUT_SEND_HZ` with a few redundant inputs attached.
fn uplink(mut res: ResMut<PlayerRes>) {
    let PlayerRes {
        net,
        player,
        recent,
        tick,
        seq,
        ..
    } = &mut *res;
    let Some(link) = net.as_mut() else {
        return;
    };

    let every = (proto::TICK_HZ / proto::INPUT_SEND_HZ).max(1);
    if tick.0 % every != 0 {
        return;
    }

    *seq = seq.wrapping_add(1);
    let mut inputs = recent.clone();
    inputs.reverse();
    let packet = proto::PlayerMovementPacket {
        seq: *seq,
        player: link.player_id,
        report: proto::MovementReport {
            tick: *tick,
            position: player.pos.to_array(),
            velocity: player.vel.to_array(),
            body_yaw: player.yaw,
            grounded: player.grounded,
            applied_correction_seq: 0,
        },
        inputs,
    };
    link.send(&packet);
}

fn sync_avatar(mut q: Query<&mut Transform, With<Avatar>>, res: Res<PlayerRes>) {
    for mut t in &mut q {
        t.translation = res.player.pos;
        t.rotation = Quat::from_rotation_y(res.player.yaw);
    }
}

fn follow(res: Res<PlayerRes>, mut q: Query<&mut Transform, With<Camera3d>>) {
    let rotated = Quat::from_rotation_y(res.player.yaw) * Vec3::new(0.0, 6.0, 9.0);
    let eye = res.player.pos + rotated;
    for mut t in &mut q {
        t.translation = eye;
        t.look_at(res.player.pos + Vec3::Y * 0.6, Vec3::Y);
    }
}

fn pump_net(mut res: ResMut<PlayerRes>) {
    let PlayerRes {
        net,
        player,
        corrections,
        ..
    } = &mut *res;
    let Some(link) = net.as_mut() else {
        return;
    };
    for snapshot in link.drain_snapshots() {
        if let Some(c) = snapshot.correction
            && corrections.accept(&c)
        {
            player.apply_correction(&c);
        }
    }
}
