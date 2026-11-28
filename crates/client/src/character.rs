//! Rendering side of the player entities and of the level.
//!
//! The simulated components are inserted for the predicted player only; remote
//! players are interpolated copies whose [`Position`] is written by lightyear,
//! so they get visuals and a transform but no character controller.

use bevy::prelude::*;
use lightyear::prelude::*;

use shared::protocol::*;
use shared::*;

/// Material shared by every player capsule.
#[derive(Resource)]
pub struct PlayerMaterial(pub Handle<StandardMaterial>);

/// Camera and lights.
pub fn setup_scene(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    commands.insert_resource(PlayerMaterial(materials.add(StandardMaterial {
        base_color: Color::srgb(0.30, 0.65, 0.95),
        ..default()
    })));

    let spawn = shared::level::spawn_point(0);
    commands.spawn((
        Camera3d::default(),
        AmbientLight {
            color: Color::WHITE,
            brightness: 320.0,
            ..default()
        },
        Transform::from_translation(spawn + Vec3::new(0.0, 6.0, 12.0))
            .looking_at(spawn + Vec3::Y * 0.9, Vec3::Y),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 14_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(6.0, 12.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// The default level: the map is drawn from the same file its colliders are
/// baked from, so what is on screen is what the capsule hits. The server takes
/// the same colliders through [`shared::level`].
pub fn spawn_level(mut commands: Commands, assets: Res<AssetServer>) {
    let level = shared::level::DEFAULT_LEVEL;
    commands.spawn((
        Name::new("level"),
        WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(level))),
    ));
    match shared::level::spawn_colliders(&mut commands, level) {
        Ok(count) => info!("level {level}: {count} static colliders"),
        Err(e) => error!("cannot load level {level}: {e}"),
    }
}

fn capsule_mesh(meshes: &mut Assets<Mesh>) -> Mesh3d {
    Mesh3d(meshes.add(Capsule3d::new(
        shared::PLAYER_RADIUS,
        shared::CAPSULE_LENGTH,
    )))
}

/// Own player: predicts the character controller locally.
pub fn spawn_predicted(
    trigger: On<Add<(Player, Predicted)>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Res<PlayerMaterial>,
) {
    commands.entity(trigger.entity).insert_if_new((
        shared::player_physics_bundle(Vec3::ZERO),
        capsule_mesh(&mut meshes),
        MeshMaterial3d(material.0.clone()),
    ));
}

/// Everyone else: no controller, the interpolated pose is written by lightyear.
pub fn spawn_interpolated(
    trigger: On<Add<(Player, Interpolated)>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Res<PlayerMaterial>,
) {
    commands.entity(trigger.entity).insert_if_new((
        Transform::default(),
        capsule_mesh(&mut meshes),
        MeshMaterial3d(material.0.clone()),
    ));
}

/// Third-person follow camera on the controlled player.
pub fn follow_controlled(
    controlled: Query<&Position, (With<Player>, With<Controlled>)>,
    mut camera: Query<&mut Transform, (With<Camera3d>, Without<Player>)>,
) {
    let Ok(position) = controlled.single() else {
        return;
    };
    let target = position.0;
    for mut transform in &mut camera {
        transform.translation = target + Vec3::new(0.0, 6.0, 9.0);
        transform.look_at(target + Vec3::Y * 0.6, Vec3::Y);
    }
}
