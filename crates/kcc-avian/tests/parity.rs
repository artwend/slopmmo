//! The virtual character has to answer the same questions as the reference
//! solver in `shared::kcc`.
//!
//! Each test mirrors one in the pure-solver suite, so the client can switch
//! backends without changing what the movement model relies on. Fixed steps are
//! driven by the test, not the wall clock.

use std::time::Duration;

use bevy::prelude::*;
use kcc_avian::prelude::*;
use shared::{Aabb3d, StaticWorld};

const GRAVITY: f32 = -24.0;
const TIMESTEP_HZ: f64 = 60.0;
const DT: f64 = 1.0 / TIMESTEP_HZ;

struct Rig {
    app: App,
    player: Entity,
}

impl Rig {
    fn new(world: &StaticWorld, start: Vec3) -> Self {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PhysicsPlugins::new(FixedPostUpdate)))
            .insert_resource(Time::<Fixed>::from_hz(TIMESTEP_HZ))
            .add_plugins(MoveAndSlidePlugin::default());

        for b in world.boxes() {
            let size = b.max - b.min;
            let centre = (b.max + b.min) * 0.5;
            app.world_mut().spawn((
                RigidBody::Static,
                Collider::cuboid(size.x, size.y, size.z),
                Position::from(Vec3::from(centre)),
                Rotation::default(),
            ));
        }

        let player = app
            .world_mut()
            .spawn((
                VirtualCharacter,
                AutoMoveAndSlide::default(),
                SlopeHandling::default(),
                GroundSnapping::default(),
                Collider::capsule(0.4, 1.0),
                Position::from(start),
                Rotation::default(),
            ))
            .id();

        // The clock must not add steps of its own: one manual overstep per
        // `update` is exactly one fixed tick.
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        app.finish();
        app.cleanup();

        Self { app, player }
    }

    fn step(&mut self, velocity: Vec3) {
        self.app
            .world_mut()
            .get_mut::<LinearVelocity>(self.player)
            .expect("player has LinearVelocity")
            .0 = velocity;
        self.app
            .world_mut()
            .resource_mut::<Time<Fixed>>()
            .accumulate_overstep(Duration::from_secs_f64(DT));
        self.app.update();
    }

    fn position(&self) -> Vec3 {
        self.app
            .world()
            .get::<Position>(self.player)
            .expect("player has Position")
            .0
    }

    fn velocity(&self) -> Vec3 {
        self.app
            .world()
            .get::<LinearVelocity>(self.player)
            .expect("player has LinearVelocity")
            .0
    }

    fn grounded(&self) -> bool {
        self.app.world().get::<Grounded>(self.player).is_some()
    }
}

/// Re-asserts a constant horizontal intent every tick, the way a held stick
/// does, and lets gravity drive the vertical axis.
fn simulate(world: &StaticWorld, start: Vec3, vel: Vec3, ticks: u32) -> (Vec3, Vec3, bool) {
    let mut rig = Rig::new(world, start);
    let mut vertical = vel.y;
    for _ in 0..ticks {
        vertical += GRAVITY * DT as f32;
        rig.step(Vec3::new(vel.x, vertical, vel.z));
        vertical = rig.velocity().y;
    }
    (rig.position(), rig.velocity(), rig.grounded())
}

#[test]
fn falling_body_lands_on_the_floor_and_is_grounded() {
    let world = StaticWorld::test_room();
    let (position, velocity, grounded) =
        simulate(&world, Vec3::new(0.0, 3.0, 0.0), Vec3::ZERO, 120);
    assert!(grounded, "should find the floor");
    assert!(
        (position.y - 0.9).abs() < 0.01,
        "rest height was {}",
        position.y
    );
    assert!(velocity.y.abs() < 0.01);
}

#[test]
fn wall_stops_horizontal_motion_instead_of_tunnelling() {
    let world = StaticWorld::test_room();
    let (position, velocity, _) = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(30.0, 0.0, 0.0),
        120,
    );
    // Inner wall face is at x = 9.5, capsule radius 0.4.
    assert!(position.x < 9.15, "escaped the room at x={}", position.x);
    assert!(velocity.x.abs() < 0.5, "wall did not absorb velocity");
}

#[test]
fn pillar_contact_slides_along_the_surface() {
    let world = StaticWorld::test_room();
    let (position, _, _) = simulate(
        &world,
        Vec3::new(1.2, 0.9, -4.0),
        Vec3::new(1.0, 0.0, 6.0),
        150,
    );
    assert!(
        position.z > 3.0,
        "should have slid past the pillar, ended at {position:?}"
    );
    assert!(
        position.x > 2.6,
        "should have been carried along the face, ended at {position:?}"
    );
}

#[test]
fn kcc_does_not_climb_a_ledge_taller_than_the_capsule_radius() {
    let mut world = StaticWorld::new();
    world.add(Aabb3d::new(
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(20.0, 0.5, 20.0),
    ));
    world.add(Aabb3d::from_min_max(
        Vec3::new(3.0, 0.0, -5.0),
        Vec3::new(10.0, 0.9, 5.0),
    ));

    let (position, _, _) = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(5.0, 0.0, 0.0),
        180,
    );
    assert!(
        position.x < 2.9,
        "virtual character must block the ledge, got x={}",
        position.x
    );
}

#[test]
fn staying_put_on_the_floor_does_not_drift() {
    let world = StaticWorld::test_room();
    let (position, _, grounded) = simulate(&world, Vec3::new(0.0, 0.9, 0.0), Vec3::ZERO, 240);
    assert!(grounded);
    assert!(
        (position - Vec3::new(0.0, 0.9, 0.0)).length() < 0.01,
        "resting capsule drifted to {position:?}"
    );
}

#[test]
fn two_runs_with_the_same_inputs_agree() {
    let world = StaticWorld::test_room();
    let a = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(2.0, 0.0, -3.0),
        120,
    );
    let b = simulate(
        &world,
        Vec3::new(0.0, 0.9, 0.0),
        Vec3::new(2.0, 0.0, -3.0),
        120,
    );
    assert_eq!(a, b, "the same inputs must produce the same pose");
}

#[test]
fn disabled_characters_do_not_move() {
    let world = StaticWorld::test_room();
    let mut rig = Rig::new(&world, Vec3::new(0.0, 0.9, 0.0));
    rig.app
        .world_mut()
        .get_mut::<AutoMoveAndSlide>(rig.player)
        .expect("player has AutoMoveAndSlide")
        .enabled = false;
    rig.step(Vec3::new(5.0, 0.0, 0.0));
    assert_eq!(rig.position(), Vec3::new(0.0, 0.9, 0.0));
}
