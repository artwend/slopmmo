//! Scripted, deterministic client run. This is what CI executes.
//!
//! Offline mode runs the same Avian character controller and the same movement
//! math as the networked client, without any sockets, so it can assert on
//! determinism and on the controller's invariants. With `--connect` the script is
//! replayed against a live server through the same bindings the windowed client
//! uses.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ::kcc::prelude::{
    Grounded, IslandPlugin, IslandSleepingPlugin, LinearVelocity, MoveAndSlidePlugin,
    PhysicsInterpolationPlugin, PhysicsPlugins, Position, VirtualCharacter,
};
use bevy::prelude::*;
use lightyear::input::bei::prelude::EnhancedInputSystems;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

use shared::protocol::*;
use shared::*;

use crate::transport::ClientNetPlugin;
use crate::{Args, bindings};

/// One scripted phase: a wish direction in world space plus modifiers.
#[derive(Clone, Copy)]
struct Phase {
    name: &'static str,
    ticks: u32,
    wish: Vec3,
    sprint: bool,
    jump: bool,
}

fn phases() -> Vec<Phase> {
    vec![
        Phase {
            name: "walk_forward",
            ticks: 90,
            wish: Vec3::NEG_Z,
            sprint: false,
            jump: false,
        },
        Phase {
            name: "sprint_forward",
            ticks: 90,
            wish: Vec3::NEG_Z,
            sprint: true,
            jump: false,
        },
        Phase {
            name: "strafe_into_wall",
            ticks: 600,
            wish: Vec3::X,
            sprint: true,
            jump: false,
        },
        Phase {
            name: "diagonal_slide",
            ticks: 300,
            wish: Vec3::new(1.0, 0.0, -1.0).normalize(),
            sprint: true,
            jump: false,
        },
        Phase {
            name: "jump",
            ticks: 1,
            wish: Vec3::ZERO,
            sprint: false,
            jump: true,
        },
        Phase {
            name: "settle",
            ticks: 120,
            wish: Vec3::ZERO,
            sprint: false,
            jump: false,
        },
    ]
}

fn total_ticks() -> u32 {
    phases().iter().map(|p| p.ticks).sum()
}

/// Where the script has got to, plus the extremes needed by the invariants.
#[derive(Resource)]
struct Script {
    phases: Vec<Phase>,
    index: usize,
    left: u32,
    budget: u32,
    ticks: u32,
    peak_y: f32,
    max_abs_x: f32,
    max_abs_z: f32,
    saw_controlled_player: bool,
    finished: bool,
}

impl Script {
    fn new(budget: u32) -> Self {
        let phases = phases();
        Self {
            left: phases[0].ticks,
            peak_y: shared::RESTING_HEIGHT,
            phases,
            index: 0,
            budget,
            ticks: 0,
            max_abs_x: 0.0,
            max_abs_z: 0.0,
            saw_controlled_player: false,
            finished: false,
        }
    }

    fn phase(&self) -> Option<Phase> {
        self.phases.get(self.index).copied()
    }

    fn advance(&mut self) {
        self.ticks += 1;
        self.left = self.left.saturating_sub(1);
        if self.left == 0 {
            self.index += 1;
            self.left = self.phases.get(self.index).map_or(0, |p| p.ticks);
        }
    }

    fn exhausted(&self) -> bool {
        self.ticks >= self.budget || self.index >= self.phases.len()
    }
}

struct Report {
    pos: Vec3,
    vel: Vec3,
    grounded: bool,
    peak_y: f32,
    max_abs_x: f32,
    max_abs_z: f32,
    ticks: u32,
}

/// One script tick per fixed step, feeding the shared velocity model directly.
fn apply_script(
    time: Res<Time>,
    params: Res<MovementParams>,
    mut script: ResMut<Script>,
    mut players: Query<(&mut LinearVelocity, Has<Grounded>), With<VirtualCharacter>>,
) {
    if script.finished {
        return;
    }
    if script.exhausted() {
        script.finished = true;
        return;
    }
    let Some(phase) = script.phase() else {
        script.finished = true;
        return;
    };
    let dt = time.delta_secs();
    if let Ok((mut velocity, grounded)) = players.single_mut() {
        velocity.0 = shared::step_velocity_raw(
            &params.0,
            phase.wish,
            phase.sprint,
            phase.jump,
            velocity.0,
            grounded,
            dt,
        );
    }
    script.advance();
}

/// Records the extremes the invariants need, after the physics step.
fn track_extremes(script: ResMut<Script>, players: Query<&Position, With<VirtualCharacter>>) {
    if script.ticks == 0 {
        return;
    }
    if let Ok(position) = players.single() {
        let pos = position.0;
        let mut script = script;
        script.peak_y = script.peak_y.max(pos.y);
        script.max_abs_x = script.max_abs_x.max(pos.x.abs());
        script.max_abs_z = script.max_abs_z.max(pos.z.abs());
    }
}

fn physics_plugins() -> bevy::app::PluginGroupBuilder {
    PhysicsPlugins::default()
        .build()
        .disable::<PhysicsInterpolationPlugin>()
        .disable::<IslandPlugin>()
        .disable::<IslandSleepingPlugin>()
}

fn spawn_room(app: &mut App) {
    for b in shared::test_room().boxes() {
        app.world_mut().spawn(shared::terrain_bundle(b));
    }
}

fn run_offline(seconds: f32) -> Report {
    let budget = ((seconds * TICK_HZ as f32) as u32).max(1);

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(Time::<Fixed>::from_hz(TICK_HZ))
        .add_plugins(physics_plugins())
        .add_plugins(MoveAndSlidePlugin::default())
        .init_resource::<MovementParams>()
        .insert_resource(Script::new(budget))
        .add_systems(FixedUpdate, apply_script)
        .add_systems(FixedLast, track_extremes);

    spawn_room(&mut app);
    let player_id = app
        .world_mut()
        .spawn(shared::player_physics_bundle(Vec3::new(
            0.0,
            shared::RESTING_HEIGHT,
            0.0,
        )))
        .id();

    // `App::run` would do this for us, but the fixed steps are driven by hand.
    app.finish();
    app.cleanup();
    app.world_mut().resource_mut::<Time<Virtual>>().pause();

    while !app.world().resource::<Script>().finished {
        let step = app.world().resource::<Time<Fixed>>().timestep();
        app.world_mut()
            .resource_mut::<Time<Fixed>>()
            .accumulate_overstep(step);
        app.update();
    }

    let (peak_y, max_abs_x, max_abs_z, ticks) = {
        let script = app.world().resource::<Script>();
        (
            script.peak_y,
            script.max_abs_x,
            script.max_abs_z,
            script.ticks,
        )
    };
    let (pos, vel, grounded) = {
        let world = app.world();
        let pos = world.get::<Position>(player_id).map_or(Vec3::ZERO, |p| p.0);
        let vel = world
            .get::<LinearVelocity>(player_id)
            .map_or(Vec3::ZERO, |v| v.0);
        (pos, vel, world.get::<Grounded>(player_id).is_some())
    };
    Report {
        pos,
        vel,
        grounded,
        peak_y,
        max_abs_x,
        max_abs_z,
        ticks,
    }
}

/// Headless client that replays the script against a live server.
///
/// The script waits for the server to replicate the controlled player, then
/// presses the same keys a windowed player would. That exercises the whole
/// chain: bindings -> BEI evaluation -> lightyear input buffer -> server.
fn run_live(seconds: f32, server: SocketAddr) -> (Script, Option<Vec3>) {
    let budget = ((seconds * TICK_HZ as f32) as u32).max(1);

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins((
            TransformPlugin,
            bevy::input::InputPlugin,
            bevy::asset::AssetPlugin::default(),
            bevy::scene::ScenePlugin,
            bevy::log::LogPlugin::default(),
            bevy::state::app::StatesPlugin,
        ))
        .insert_resource(Time::<Fixed>::from_hz(TICK_HZ))
        .add_plugins(ClientPlugins {
            tick_duration: TICK_DURATION,
        })
        .add_plugins(SharedPlugin)
        .insert_resource(PredictionManager::default())
        .add_plugins(ClientNetPlugin { server })
        .insert_resource(Script::new(budget))
        .add_observer(spawn_predicted_physics)
        .add_systems(
            FixedPreUpdate,
            drive_script.before(EnhancedInputSystems::Update),
        );
    bindings::register(&mut app);

    spawn_room(&mut app);

    // `App::run` would do this, but the loop below drives `update` by hand.
    // Lightyear registers part of the send/receive plumbing in `finish`.
    app.finish();
    app.cleanup();

    // Leave time for the handshake and for the script to run.
    let deadline = Instant::now() + Duration::from_secs_f32(seconds.max(1.0) + 5.0);
    while !app.world().resource::<Script>().finished && Instant::now() < deadline {
        app.update();
        std::thread::sleep(Duration::from_millis(1));
    }

    let script = app.world().resource::<Script>();
    let text = Script {
        phases: script.phases.clone(),
        index: script.index,
        left: script.left,
        budget: script.budget,
        ticks: script.ticks,
        peak_y: script.peak_y,
        max_abs_x: script.max_abs_x,
        max_abs_z: script.max_abs_z,
        saw_controlled_player: script.saw_controlled_player,
        finished: script.finished,
    };
    let pos = app
        .world_mut()
        .query_filtered::<&Position, (With<Player>, With<Controlled>)>()
        .single(app.world())
        .ok()
        .map(|p| p.0);
    (text, pos)
}

fn spawn_predicted_physics(trigger: On<Add<(Player, Predicted)>>, mut commands: Commands) {
    commands
        .entity(trigger.entity)
        .insert_if_new(shared::player_physics_bundle(Vec3::ZERO));
}

const SCRIPT_KEYS: [KeyCode; 6] = [
    KeyCode::KeyW,
    KeyCode::KeyA,
    KeyCode::KeyS,
    KeyCode::KeyD,
    KeyCode::ShiftLeft,
    KeyCode::Space,
];

/// Presses the keys the current phase asks for. One fixed tick of Space is a
/// full jump, because the action uses a `Press` condition.
fn drive_script(
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut script: ResMut<Script>,
    players: Query<(), (With<Player>, With<Controlled>)>,
) {
    for key in SCRIPT_KEYS {
        keys.release(key);
    }
    if script.finished {
        return;
    }
    if players.is_empty() {
        return;
    }
    script.saw_controlled_player = true;
    if script.exhausted() {
        script.finished = true;
        return;
    }
    let Some(phase) = script.phase() else {
        script.finished = true;
        return;
    };
    if phase.wish.x > 0.5 {
        keys.press(KeyCode::KeyD);
    } else if phase.wish.x < -0.5 {
        keys.press(KeyCode::KeyA);
    }
    if phase.wish.z < -0.5 {
        keys.press(KeyCode::KeyW);
    } else if phase.wish.z > 0.5 {
        keys.press(KeyCode::KeyS);
    }
    if phase.sprint {
        keys.press(KeyCode::ShiftLeft);
    }
    if phase.jump {
        keys.press(KeyCode::Space);
    }
    script.advance();
}

pub fn run(args: &Args) -> ExitCode {
    let a = run_offline(args.seconds);
    let b = run_offline(args.seconds);

    println!("script:");
    for p in phases() {
        println!("  {:<16} {:>4} ticks", p.name, p.ticks);
    }
    println!("phase_ticks     {}", a.ticks);
    println!("script_ticks    {}", total_ticks());
    println!("final_x         {:.4}", a.pos.x);
    println!("final_y         {:.4}", a.pos.y);
    println!("final_z         {:.4}", a.pos.z);
    println!("speed           {:.4}", a.vel.length());
    println!("grounded        {}", a.grounded);
    println!("peak_y          {:.4}", a.peak_y);
    println!("max_abs_x       {:.4}", a.max_abs_x);
    println!("max_abs_z       {:.4}", a.max_abs_z);

    let deterministic = a.pos == b.pos && a.vel == b.vel && a.ticks == b.ticks;
    println!("deterministic   {deterministic}");

    let mut failures = Vec::new();

    if let Some(addr) = args.connect {
        println!("net_server      {addr}");
        let (script, pos) = run_live(args.seconds, addr);
        println!("net_ticks       {}", script.ticks);
        println!(
            "net_completed   {}",
            script.finished && script.ticks >= total_ticks()
        );
        if let Some(pos) = pos {
            println!("net_x           {:.4}", pos.x);
            println!("net_y           {:.4}", pos.y);
            println!("net_z           {:.4}", pos.z);
        }
        if !script.saw_controlled_player {
            failures.push("the server never replicated a controlled player");
        }
        if script.ticks == 0 {
            failures.push("the live script never ran");
        }
    }

    if !deterministic {
        failures.push("two identical runs diverged");
    }
    if a.ticks < total_ticks() {
        failures.push("script was truncated by --seconds");
    }
    if a.max_abs_x > 9.2 || a.max_abs_z > 9.2 {
        failures.push("left the room bounds");
    }
    if a.peak_y < 1.1 {
        failures.push("jump never left the ground");
    }
    if !a.grounded {
        failures.push("did not settle back to the floor");
    }
    if (a.pos.y - shared::RESTING_HEIGHT).abs() > 0.05 {
        failures.push("resting height is wrong");
    }

    if failures.is_empty() {
        println!("result          ok");
        ExitCode::SUCCESS
    } else {
        for f in failures {
            println!("failure         {f}");
        }
        ExitCode::FAILURE
    }
}
