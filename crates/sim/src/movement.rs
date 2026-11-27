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

/// One tick of velocity integration from a wire-format input frame.
pub fn step_velocity(
    params: &MoveParams,
    input: &proto::InputFrame,
    velocity: Vec3,
    grounded: bool,
    dt: f32,
) -> Vec3 {
    // Stick input is camera-relative; yaw comes from the frame so the server can
    // replay the exact same vector from the same bytes.
    step_velocity_raw(
        params,
        wish_world(input),
        input.sprint(),
        input.jump(),
        velocity,
        grounded,
        dt,
    )
}

/// One tick of velocity integration from a world-space wish direction.
///
/// This is the single implementation of the movement model: the networked
/// client and server feed it the enhanced-input action values, while the
/// headless script feeds it the same values directly.
pub fn step_velocity_raw(
    params: &MoveParams,
    wish: Vec3,
    sprint: bool,
    jump: bool,
    velocity: Vec3,
    grounded: bool,
    dt: f32,
) -> Vec3 {
    let mut vel = velocity;
    let speed_target = params.target_speed(sprint);

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

        if jump {
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

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    #[test]
    fn ground_acceleration_reaches_the_target_speed() {
        let params = MoveParams::default();
        let mut vel = Vec3::ZERO;
        for _ in 0..60 {
            vel = step_velocity_raw(&params, Vec3::NEG_Z, false, false, vel, true, DT);
        }
        let speed = Vec3::new(vel.x, 0.0, vel.z).length();
        assert!(
            (speed - params.max_speed).abs() < 0.1,
            "settled at {speed} instead of {}",
            params.max_speed
        );
    }

    #[test]
    fn sprint_raises_the_target_speed() {
        let params = MoveParams::default();
        let settle = |sprint: bool| {
            let mut vel = Vec3::ZERO;
            for _ in 0..60 {
                vel = step_velocity_raw(&params, Vec3::NEG_Z, sprint, false, vel, true, DT);
            }
            Vec3::new(vel.x, 0.0, vel.z).length()
        };
        assert!(
            settle(true) > settle(false) + 1.0,
            "sprint must reach a higher speed than a walk"
        );
    }

    #[test]
    fn jump_only_lifts_a_grounded_character() {
        let params = MoveParams::default();
        let grounded = step_velocity_raw(&params, Vec3::ZERO, false, true, Vec3::ZERO, true, DT);
        // Gravity is applied in the same tick, so the lift is one tick short of
        // the configured jump speed.
        let expected = params.jump_speed + params.gravity * DT;
        assert!((grounded.y - expected).abs() < 1e-5);

        let airborne = step_velocity_raw(&params, Vec3::ZERO, false, true, Vec3::ZERO, false, DT);
        assert!(airborne.y < 0.0, "a mid-air jump must not add lift");
    }

    #[test]
    fn gravity_pulls_an_airborne_character_down() {
        let params = MoveParams::default();
        let vel = step_velocity_raw(&params, Vec3::ZERO, false, false, Vec3::ZERO, false, DT);
        assert!((vel.y - params.gravity * DT).abs() < 1e-6);
    }
}
