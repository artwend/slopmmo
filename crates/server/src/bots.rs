//! Deterministic bot driver for load and collision tests.
//!
//! No `rand`: an LCG keeps runs byte-identical, which is what makes the
//! determinism assertion in `sim/tests` meaningful when reproduced here.

use bevy_math::Vec3;
use sim::movement::MoveParams;

pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    pub fn unit(&mut self) -> f32 {
        self.next_u32() as f32 / u32::MAX as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

pub struct Bot {
    pub id: u64,
    pub pos: Vec3,
    pub vel: Vec3,
    pub grounded: bool,
    /// Point the bot keeps walking to; re-rolled when reached.
    pub goal: Vec3,
}

impl Bot {
    pub fn spawn(id: u64, rng: &mut Lcg) -> Self {
        // Ring of radius 2..8 m inside the test room's 9.5 m walls.
        let ang = rng.range(0.0, std::f32::consts::TAU);
        let r = rng.range(2.0, 8.0);
        let pos = Vec3::new(ang.cos() * r, 0.9, ang.sin() * r);
        Self {
            id,
            pos,
            vel: Vec3::ZERO,
            grounded: true,
            goal: Vec3::new(-pos.x, pos.y, -pos.z),
        }
    }

    /// Bots converge on the centre, so contacts actually happen.
    pub fn input(&mut self, tick: proto::Tick, rng: &mut Lcg) -> proto::InputFrame {
        let to_goal = self.goal - self.pos;
        let flat = Vec3::new(to_goal.x, 0.0, to_goal.z);
        if flat.length() < 1.0 {
            let ang = rng.range(0.0, std::f32::consts::TAU);
            let r = rng.range(0.0, 3.0);
            self.goal = Vec3::new(ang.cos() * r, 0.9, ang.sin() * r);
        }

        let dir = if flat.length() > 1e-3 {
            flat.normalize()
        } else {
            Vec3::ZERO
        };

        proto::InputFrame {
            tick,
            // `move_x` / `move_z` are the camera-relative wish axes.
            move_x: (dir.x * 127.0).clamp(-127.0, 127.0) as i8,
            move_z: (dir.z * 127.0).clamp(-127.0, 127.0) as i8,
            buttons: 0,
            camera_yaw: 0,
        }
    }

    pub fn predict(
        &self,
        world: &sim::StaticWorld,
        params: &MoveParams,
        input: &proto::InputFrame,
        dt: f32,
    ) -> (Vec3, Vec3) {
        let vel = sim::movement::step_velocity(params, input, self.vel, self.grounded, dt);
        let out = sim::move_and_slide(world, &sim::kcc::KccParams::default(), self.pos, vel, dt);
        (out.position, out.velocity)
    }
}
