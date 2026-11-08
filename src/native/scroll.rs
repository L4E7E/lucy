//! Frame-clock driven, critically damped scrolling. Repeated wheel input keeps
//! velocity rather than restarting an easing curve on every event.
use std::time::Instant;
pub const DISTANCE_SCALE: f64 = 3.45; // 15% more travel; animation timing is unchanged.
pub struct Scroll {
    pub position: f64,
    pub target: f64,
    velocity: f64,
    omega: f64,
    last: Instant,
}
impl Scroll {
    pub fn new() -> Self {
        Self {
            position: 0.0,
            target: 0.0,
            velocity: 0.0,
            omega: 48.0,
            last: Instant::now(),
        }
    }
    pub fn reset(&mut self, value: f64) {
        self.position = value;
        self.target = value;
        self.velocity = 0.0;
        self.last = Instant::now();
    }
    pub fn active(&self) -> bool {
        (self.position - self.target).abs() > 0.05 || self.velocity.abs() > 0.5
    }
    pub fn push(&mut self, position: f64, delta: f64, maximum: f64) {
        self.omega = 48.0;
        self.retarget(position, delta, maximum);
    }
    pub fn push_launcher(&mut self, position: f64, delta: f64, maximum: f64) {
        self.omega = 28.0;
        self.retarget(position, delta, maximum);
        // Immediate wheel response followed by a longer, critically damped glide.
        // Bound the impulse analytically so edges never bounce or overshoot.
        let remaining = self.target - self.position;
        let limit = self.omega * remaining.abs();
        let impulse = (self.velocity + delta * 11.0).clamp(-limit, limit);
        self.velocity = if impulse * remaining > 0.0 {
            impulse
        } else {
            0.0
        };
    }
    fn retarget(&mut self, position: f64, delta: f64, maximum: f64) {
        if (self.position - position).abs() > 0.1 {
            self.reset(position);
        }
        if !self.active() {
            self.last = Instant::now();
        }
        self.target = (self.target + delta).clamp(0.0, maximum.max(0.0));
    }
    pub fn tick(&mut self) -> f64 {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.step(dt.min(0.05));
        self.position
    }
    fn step(&mut self, dt: f64) {
        let omega = self.omega;
        let error = self.position - self.target;
        let c = self.velocity + omega * error;
        let decay = (-omega * dt).exp();
        self.position = self.target + (error + c * dt) * decay;
        self.velocity = (self.velocity - omega * c * dt) * decay;
        if !self.active() {
            self.position = self.target;
            self.velocity = 0.0;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_wheel_has_prompt_response_and_a_longer_smooth_tail() {
        let mut normal = Scroll::new();
        let mut launcher = Scroll::new();
        normal.push(0.0, 60.0, 1000.0);
        launcher.push_launcher(0.0, 60.0, 1000.0);
        normal.step(1.0 / 165.0);
        launcher.step(1.0 / 165.0);
        assert!(launcher.position > normal.position);
        normal.step(0.12);
        launcher.step(0.12);
        assert!(launcher.position < normal.position);
        assert!(launcher.position > 45.0);
        let velocity = launcher.velocity;
        launcher.push_launcher(launcher.position, 40.0, 1000.0);
        assert!(launcher.velocity >= velocity);
        for _ in 0..165 {
            launcher.step(1.0 / 165.0);
            assert!((0.0..=100.001).contains(&launcher.position));
        }
        assert_eq!(launcher.position, 100.0);
    }
    #[test]
    fn launcher_glide_is_refresh_independent_and_stops_exactly_at_bounds() {
        let mut a = Scroll::new();
        let mut b = Scroll::new();
        a.push_launcher(0.0, 80.0, 100.0);
        b.push_launcher(0.0, 80.0, 100.0);
        for _ in 0..12 {
            a.step(1.0 / 60.0);
        }
        for _ in 0..33 {
            b.step(1.0 / 165.0);
        }
        assert!((a.position - b.position).abs() < 0.001);
        a.push_launcher(a.position, 1000.0, 100.0);
        for _ in 0..165 {
            a.step(1.0 / 165.0);
            assert!((0.0..=100.001).contains(&a.position));
        }
        assert_eq!(a.position, 100.0);
        a.push_launcher(a.position, 1000.0, 100.0);
        assert!(!a.active());
        a.push_launcher(a.position, -1000.0, 100.0);
        for _ in 0..165 {
            a.step(1.0 / 165.0);
            assert!((0.0..=100.001).contains(&a.position));
        }
        assert_eq!(a.position, 0.0);
        a.push(0.0, 20.0, 100.0);
        assert_eq!(a.omega, 48.0, "other lists keep their original response");
    }
    #[test]
    fn list_distance_is_only_fifteen_percent_faster() {
        assert!((DISTANCE_SCALE / 3.0 - 1.15).abs() < f64::EPSILON * 2.0);
    }
    #[test]
    fn motion_is_refresh_rate_independent_and_does_not_overshoot() {
        let mut a = Scroll::new();
        let mut b = Scroll::new();
        a.push(0.0, 90.0, 1000.0);
        b.push(0.0, 90.0, 1000.0);
        for _ in 0..60 {
            a.step(1.0 / 60.0);
            assert!((0.0..=90.001).contains(&a.position));
        }
        for _ in 0..165 {
            b.step(1.0 / 165.0);
        }
        assert!((a.position - b.position).abs() < 0.001);
        assert_eq!(a.position, 90.0);
    }
    #[test]
    fn repeated_input_keeps_momentum_and_bounds_are_exact() {
        let mut s = Scroll::new();
        s.push(0.0, 45.0, 100.0);
        s.step(0.02);
        let velocity = s.velocity;
        s.push(s.position, 45.0, 100.0);
        assert_eq!(s.velocity, velocity);
        s.step(0.01);
        assert!(s.position > 0.0);
        s.push(s.position, 500.0, 100.0);
        assert_eq!(s.target, 100.0);
        s.reset(20.0);
        assert!(!s.active());
        assert_eq!(s.position, 20.0);
    }
}
