use bevy::curve::Curve;
use bevy::prelude::*;
use lightyear::prelude::input::bei::*;
use lightyear::prelude::input::InputConfig;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

// Components
#[derive(Component)]
pub struct InPvp;

#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlayerId(pub PeerId);

#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq, Reflect, Deref, DerefMut)]
pub struct ReplicablePosition(pub Vec3);
//reserved alternative name
// pub struct ReplicablePlayerPosition(pub Vec3);

impl Ease for ReplicablePosition {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        FunctionCurve::new(Interval::UNIT, move |t| {
            ReplicablePosition(Vec3::lerp(start.0, end.0, t))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(u8)]
enum Stance {
    #[default]
    Idle = 0,
    Walk = 1,
    Sprint = 2,
    Airborne = 3,
    Dodge = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ActionKind {
    Jump,
    Dodge,
    LightAttack,
    HeavyAttack,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
struct ActionInput {
    action: Option<ActionKind>,
    aim_yaw: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
struct MoveInput {
    move_xz: Vec2,
    sprint: bool,
}

// Channels
pub struct UnreliableStateChannel;

// Messages
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MovementReport {
    pos: Vec3,
    yaw: f32,
    stance: Stance,
}

// Inputs

// the context will be replicated
#[derive(Component, Serialize, Deserialize, Reflect, Clone, Debug, PartialEq)]
pub struct Player;

#[derive(Debug, InputAction)]
#[action_output(Vec2)]
pub struct Movement;

#[derive(Debug, Clone, Copy, InputAction)]
#[action_output(bool)]
pub struct Jump;

// Protocol
#[derive(Clone)]
pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        // messages
        app.register_message::<MovementReport>()
            .add_direction(NetworkDirection::ClientToServer);

        // inputs
        app.add_plugins(InputPlugin::<Player>::new(InputConfig {
            rebroadcast_inputs: true,
            ..default()
        }));
        app.register_input_action::<Movement>();
        app.register_input_action::<Jump>();

        // components
        // app.component::<PlayerId>().replicate();

        app.component::<ReplicablePosition>()
            .replicate()
            .predict()
            .add_linear_interpolation();

        // channels
        app.add_channel::<UnreliableStateChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            ..default()
        })
        .add_direction(NetworkDirection::ClientToServer);
    }
}