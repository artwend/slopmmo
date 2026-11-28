//! The level the game runs on: one `.glb` map that the client draws and both
//! sides collide against.
//!
//! The map is parsed here rather than through Bevy's asset pipeline so the
//! headless server can build the very same colliders without render, asset or
//! async machinery. The client loads the same file through `AssetServer` for the
//! visuals and takes its colliders from here, so one file drives both.

use std::fmt;
use std::path::{Path, PathBuf};

use ::kcc::avian3d::prelude::{Collider, Position, RigidBody, Rotation};
use bevy::prelude::*;

use crate::{Aabb3d, RESTING_HEIGHT};

/// Map loaded when nothing else is requested.
pub const DEFAULT_LEVEL: &str = "maps/playground.glb";

/// Where a peer's capsule appears: the open floor of [`DEFAULT_LEVEL`] west of
/// the spawn corner, one slot every two metres along `z`.
pub fn spawn_point(slot: u32) -> Vec3 {
    Vec3::new(-12.0, RESTING_HEIGHT, (slot % 6) as f32 * 2.0 - 5.0)
}

/// The workspace `assets` directory.
///
/// Baked at compile time so `cargo run` and a standalone binary find the same
/// files, and so the client can hand this root to Bevy's asset server.
pub fn asset_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> sits two levels below the workspace root")
        .join("assets")
}

/// Absolute path of a level file.
pub fn level_path(level: &str) -> PathBuf {
    asset_root().join(level)
}

/// Why a level could not be turned into geometry.
#[derive(Debug)]
pub enum LevelError {
    Io(std::io::Error),
    Parse(gltf::Error),
    Empty,
}

impl fmt::Display for LevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "{e}"),
            Self::Empty => f.write_str("the level has no triangle geometry"),
        }
    }
}

impl std::error::Error for LevelError {}

impl From<std::io::Error> for LevelError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<gltf::Error> for LevelError {
    fn from(e: gltf::Error) -> Self {
        Self::Parse(e)
    }
}

/// Collision geometry of a level, with the bounds of everything it covers.
///
/// There is one static trimesh per glTF primitive and the node transforms are
/// baked into the vertices. Baking instead of using `Position`/`Rotation` keeps
/// the shapes identical on both sides whatever the scene hierarchy does, and
/// keeps every body at the origin so a replay never depends on a parent.
pub struct LevelGeometry {
    colliders: Vec<Collider>,
    bounds: Aabb3d,
}

impl LevelGeometry {
    /// Reads a binary glTF (`.glb`): geometry is taken straight from the
    /// embedded buffer, so external `.bin` sidecars are not supported.
    pub fn load(level: &str) -> Result<Self, LevelError> {
        let path = level_path(level);
        let bytes = std::fs::read(&path)?;
        // No validation: maps routinely require material extensions that only
        // matter to the renderer, and this reader wants triangles, nothing else.
        let gltf = gltf::Gltf::from_slice_without_validation(&bytes)?;
        let blob = gltf.blob.as_deref();

        let mut extents = Extents::default();
        let mut colliders = Vec::new();
        for node in root_nodes(&gltf.document) {
            collect(&node, blob, Mat4::IDENTITY, &mut colliders, &mut extents);
        }
        let bounds = extents.aabb().ok_or(LevelError::Empty)?;
        Ok(Self { colliders, bounds })
    }

    pub fn colliders(&self) -> &[Collider] {
        &self.colliders
    }

    /// Axis-aligned bounds of all level geometry, in world space.
    pub fn bounds(&self) -> &Aabb3d {
        &self.bounds
    }
}

/// Static body for one pre-baked level collider.
pub fn level_collider_bundle(collider: Collider) -> impl Bundle {
    (
        RigidBody::Static,
        collider,
        Position::default(),
        Rotation::default(),
    )
}

/// Spawns the level's static colliders into `commands`.
///
/// Both sides call this, so a predicted move is checked against exactly the
/// geometry the server checks it against.
pub fn spawn_colliders(commands: &mut Commands, level: &str) -> Result<usize, LevelError> {
    let geometry = LevelGeometry::load(level)?;
    for collider in geometry.colliders() {
        commands.spawn((
            Name::new("level collider"),
            level_collider_bundle(collider.clone()),
        ));
    }
    Ok(geometry.colliders().len())
}

fn root_nodes<'a>(document: &'a gltf::Document) -> Vec<gltf::Node<'a>> {
    match document.default_scene() {
        Some(scene) => scene.nodes().collect(),
        None => document
            .scenes()
            .flat_map(|scene| scene.nodes().collect::<Vec<_>>())
            .collect(),
    }
}

fn collect(
    node: &gltf::Node,
    blob: Option<&[u8]>,
    parent: Mat4,
    out: &mut Vec<Collider>,
    extents: &mut Extents,
) {
    let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        for primitive in mesh.primitives() {
            // Lines and points carry no solid geometry for a capsule to hit.
            if primitive.mode() == gltf::mesh::Mode::Triangles {
                push_primitive(&primitive, blob, world, out, extents);
            }
        }
    }
    for child in node.children() {
        collect(&child, blob, world, out, extents);
    }
}

fn push_primitive(
    primitive: &gltf::Primitive,
    blob: Option<&[u8]>,
    world: Mat4,
    out: &mut Vec<Collider>,
    extents: &mut Extents,
) {
    // A binary glTF has exactly one buffer, the blob itself.
    let reader = primitive.reader(|_| blob);
    let Some(positions) = reader.read_positions() else {
        return;
    };
    let vertices: Vec<Vec3> = positions
        .map(|p| world.transform_point3(Vec3::from_array(p)))
        .collect();

    let indices: Vec<[u32; 3]> = match reader.read_indices() {
        Some(indices) => {
            let flat: Vec<u32> = indices.into_u32().collect();
            flat.as_chunks::<3>().0.to_vec()
        }
        // Unindexed geometry is a flat list of triangle corners.
        None => (0..vertices.len() as u32 / 3)
            .map(|t| [3 * t, 3 * t + 1, 3 * t + 2])
            .collect(),
    };
    if indices.is_empty() {
        return;
    }

    let (lo, hi) = vertices
        .iter()
        .fold((Vec3::INFINITY, Vec3::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    extents.add(lo, hi);
    out.push(Collider::trimesh(vertices, indices));
}

/// Bounds of the geometry that was turned into colliders.
#[derive(Default)]
struct Extents {
    min: Vec3,
    max: Vec3,
    seen: bool,
}

impl Extents {
    fn add(&mut self, lo: Vec3, hi: Vec3) {
        self.min = if self.seen { self.min.min(lo) } else { lo };
        self.max = if self.seen { self.max.max(hi) } else { hi };
        self.seen = true;
    }

    fn aabb(&self) -> Option<Aabb3d> {
        self.seen.then(|| Aabb3d::from_min_max(self.min, self.max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_level_loads_as_static_geometry() {
        let level = LevelGeometry::load(DEFAULT_LEVEL).expect("the default level loads");
        assert!(!level.colliders().is_empty(), "level collision is empty");

        // The playground is a 300 m square; a broken transform chain shows up as
        // bounds that are off by orders of magnitude.
        let size = level.bounds().max - level.bounds().min;
        assert!(
            (size.x - 300.0).abs() < 1.0 && (size.z - 300.0).abs() < 1.0,
            "level is {size:?}, expected a 300 m square"
        );
    }

    #[test]
    fn a_player_spawned_on_the_level_stands_on_its_floor() {
        use crate::Grounded;
        use ::kcc::avian3d::prelude::PhysicsPlugins;
        use ::kcc::prelude::{
            AutoMoveAndSlide, GroundSnapping, LinearVelocity, MoveAndSlidePlugin, SlopeHandling,
            StairStepping, UpDirection, VirtualCharacter,
        };

        const HZ: f64 = 60.0;
        const DT: f32 = 1.0 / HZ as f32;
        const GRAVITY: f32 = -24.0;

        let level = LevelGeometry::load(DEFAULT_LEVEL).expect("the default level loads");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PhysicsPlugins::new(FixedPostUpdate)))
            .insert_resource(Time::<Fixed>::from_hz(HZ))
            .add_plugins(MoveAndSlidePlugin::default());
        for collider in level.colliders() {
            app.world_mut()
                .spawn(level_collider_bundle(collider.clone()));
        }

        let player = app
            .world_mut()
            .spawn((
                VirtualCharacter,
                AutoMoveAndSlide::default(),
                SlopeHandling::default(),
                GroundSnapping::default(),
                StairStepping::default(),
                UpDirection::default(),
                crate::player_collider(),
                Position::from(spawn_point(0)),
                Rotation::default(),
                LinearVelocity::default(),
            ))
            .id();

        // One manual overstep per `update` is exactly one fixed tick.
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        app.finish();
        app.cleanup();

        let mut vertical = 0.0;
        for _ in 0..240 {
            vertical += GRAVITY * DT;
            app.world_mut()
                .get_mut::<LinearVelocity>(player)
                .expect("player has LinearVelocity")
                .0 = Vec3::new(0.0, vertical, 0.0);
            let step = app.world().resource::<Time<Fixed>>().timestep();
            app.world_mut()
                .resource_mut::<Time<Fixed>>()
                .accumulate_overstep(step);
            app.update();
            vertical = app
                .world()
                .get::<LinearVelocity>(player)
                .expect("player has LinearVelocity")
                .0
                .y;
        }

        let position = app
            .world()
            .get::<Position>(player)
            .expect("player has Position")
            .0;
        assert!(
            app.world().get::<Grounded>(player).is_some(),
            "did not land on the level floor, ended at {position:?}"
        );
        assert!(
            (position.y - RESTING_HEIGHT).abs() < 0.05,
            "rest height was {}, expected {RESTING_HEIGHT}",
            position.y
        );
    }
}
