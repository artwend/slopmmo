//! Ground and air acceleration.
//!
//! Kept separate from the KCC so both the client prediction and the server
//! shadow run the *same* function on the *same* inputs. Any divergence here is
//! a mispredict, and mispredicts are the thing this whole design is trying to
//! avoid.

use bevy_math::Vec3;

#[derive(Clone, Copy, Debug)]
pub struct MoveParams {
    pub max_speed: f32,
    pub sprint_speed: f32,
    pub ground_accel: f32,
    pub ground_friction: f32,
    pub air_accel: f32,
    pub gravity: f32,
    pub jump_speed: f32,
}

impl Default for MoveParams {
    fn default() -> Self {
        Self {
            max_speed: proto::MAX_SPEED,
            sprint_speed: proto::MAX_SPEED * 1.5,
            ground_accel: proto::MAX_ACCEL,
            ground_friction: 14.0,
            air_accel: 6.0,
            gravity: -24.0,
            jump_speed: 7.0,
        }
    }
}

impl MoveParams {
    pub fn target_speed(&self, sprint: bool) -> f32 {
        if sprint {
            self.sprint_speed
        } else {
            self.max_speed
        }
    }
}

/// Rotates the stick axes into world space using the frame's camera yaw.
///
/// Bevy cameras look down local `-Z`, so at yaw 0 "forward" is world `-Z`.
pub fn wish_world(input: &proto::InputFrame) -> Vec3 {
    let [x, z] = input.wish_dir();
    let yaw = input.camera_yaw_rad();
    let (s, c) = yaw.sin_cos();
    let dir = Vec3::new(c * x + s * z, 0.0, -s * x + c * z);
    if dir.length_squared() > 1.0 {
        dir.normalize()
    } else {
        dir
    }
}

/// One tick of velocity integration.
pub fn step_velocity(
    params: &MoveParams,
    input: &proto::InputFrame,
    velocity: Vec3,
    grounded: bool,
    dt: f32,
) -> Vec3 {
    let mut vel = velocity;

    // Stick input is camera-relative; yaw comes from the frame so the server can
    // replay the exact same vector from the same bytes.
    let wish = wish_world(input);
    let speed_target = params.target_speed(input.sprint());

    if grounded {
        // Linear friction, then acceleration clamped to the target speed.
        // Exponential friction alone cannot reach `max_speed`: it settles at
        // `accel * dt / (1 - exp(-friction * dt))`, which is why the first
        // version capped out at 2.6 m/s instead of 6.
        let horizontal = Vec3::new(vel.x, 0.0, vel.z);
        let speed = horizontal.length();
        if speed > 1e-5 {
            let kept = ((speed - params.ground_friction * dt) / speed).max(0.0);
            vel.x *= kept;
            vel.z *= kept;
        }

        let vel_h = Vec3::new(vel.x, 0.0, vel.z);
        let add = wish * speed_target - vel_h;
        if add.length_squared() > 1e-8 {
            let step = (params.ground_accel * dt).min(add.length());
            let dir = add.normalize();
            vel.x += dir.x * step;
            vel.z += dir.z * step;
        }

        if input.jump() {
            vel.y = params.jump_speed;
        }
    } else {
        let add = wish * speed_target - Vec3::new(vel.x, 0.0, vel.z);
        if add.length_squared() > 1e-8 {
            let step = (params.air_accel * dt).min(add.length());
            let dir = add.normalize();
            vel.x += dir.x * step;
            vel.z += dir.z * step;
        }
    }

    if !grounded || vel.y > 0.0 {
        vel.y += params.gravity * dt;
    }

    vel
}
