//! Small GPU gauge descriptions and retargetable motion; no raster buffers/timers.
use super::model::Rect;
use std::{
    cell::Cell,
    time::{Duration, Instant},
};
pub const DURATION: Duration = Duration::from_millis(240);
// Horizontal tracks have a known alpha. Cached glyph coverage can therefore be
// recovered exactly from the scene alpha, without another mask texture/upload.
pub const TRACK_ALPHA: f64 = 0.64;
#[derive(Clone, Debug)]
pub struct Gauge {
    pub key: String,
    pub rect: Rect,
    pub clip: Rect,
    pub radius: f32,
    pub vertical: bool,
    pub alpha: f32,
    pub level: f32,
    /// Pointer scrubbing and unacknowledged seeks must show their exact target.
    pub direct: bool,
}
#[derive(Clone)]
pub struct Motion {
    pub gauge: Gauge,
    from: f32,
    at: Instant,
    last: Cell<f32>,
}
impl Motion {
    fn progress(&self, now: Instant) -> f32 {
        (now.saturating_duration_since(self.at).as_secs_f32() / DURATION.as_secs_f32()).min(1.0)
    }
    pub fn value(&self, now: Instant) -> f32 {
        let t = self.progress(now);
        self.from + (self.gauge.level - self.from) * (1.0 - (1.0 - t).powi(3))
    }
    fn moving(&self, now: Instant) -> bool {
        (self.from - self.gauge.level).abs() > 0.00001
            && (self.progress(now) < 1.0 || self.last.get() < 1.0)
    }
}
#[derive(Clone, Default)]
pub struct Batch {
    pub items: Vec<Motion>,
    pub size: (f32, f32),
}
impl Batch {
    pub fn update(&mut self, specs: Vec<Gauge>, size: (f32, f32)) -> bool {
        self.update_at(specs, size, Instant::now())
    }
    fn update_at(&mut self, specs: Vec<Gauge>, size: (f32, f32), now: Instant) -> bool {
        let mut old = std::mem::take(&mut self.items);
        let mut changed = old.len() != specs.len() || self.size != size;
        self.size = size;
        self.items = specs
            .into_iter()
            .filter(|s| s.level.is_finite())
            .map(|mut spec| {
                spec.level = spec.level.clamp(0.0, 1.0);
                if let Some(index) = old.iter().position(|m| m.gauge.key == spec.key) {
                    let mut motion = old.swap_remove(index);
                    let value = motion.value(now);
                    changed |= motion.gauge.level != spec.level
                        || motion.gauge.direct != spec.direct
                        || motion.gauge.alpha != spec.alpha
                        || bounds(motion.gauge.rect) != bounds(spec.rect)
                        || bounds(motion.gauge.clip) != bounds(spec.clip);
                    if spec.direct {
                        motion.from = spec.level;
                        motion.at = now;
                        motion.last.set(1.0);
                    } else if motion.gauge.level != spec.level {
                        motion.from = value;
                        motion.at = now;
                        motion.last.set(0.0);
                    }
                    motion.gauge = spec;
                    motion
                } else {
                    changed = true;
                    Motion {
                        from: spec.level,
                        gauge: spec,
                        at: now,
                        last: Cell::new(1.0),
                    }
                }
            })
            .collect();
        changed
    }
    pub fn animating(&self) -> bool {
        let now = Instant::now();
        self.items.iter().any(|m| m.moving(now))
    }
    pub fn presented(&self, now: Instant) {
        for m in &self.items {
            m.last.set(m.progress(now));
        }
    }
    pub fn freeze(&mut self) {
        let now = Instant::now();
        for m in &mut self.items {
            let value = m.value(now);
            m.from = value;
            m.gauge.level = value;
            m.last.set(1.0);
        }
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
}
fn bounds(r: Rect) -> (f64, f64, f64, f64) {
    (r.x, r.y, r.w, r.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(level: f32) -> Gauge {
        Gauge {
            key: "cpu-0".into(),
            rect: Rect {
                x: 30.0,
                y: 130.0,
                w: 20.0,
                h: 33.0,
            },
            clip: Rect {
                x: 20.0,
                y: 80.0,
                w: 440.0,
                h: 390.0,
            },
            radius: 4.0,
            vertical: true,
            alpha: 0.82,
            level,
            direct: false,
        }
    }
    #[test]
    fn hover_does_not_restart_motion_and_retargeting_is_continuous() {
        let now = Instant::now();
        let mut batch = Batch::default();
        batch.update_at(vec![spec(0.0)], (480.0, 500.0), now);
        batch.update_at(vec![spec(1.0)], (480.0, 500.0), now);
        let mid = now + Duration::from_millis(120);
        assert_eq!(batch.items[0].value(mid), 0.875);
        batch.update_at(vec![spec(1.0)], (480.0, 500.0), mid);
        assert_eq!(batch.items[0].value(mid), 0.875);
        batch.update_at(vec![spec(0.2)], (480.0, 500.0), mid);
        assert_eq!(batch.items[0].value(mid), 0.875);
        assert!((batch.items[0].value(mid + DURATION) - 0.2).abs() < 0.0001);
        batch.presented(mid + DURATION);
        assert!(!batch.items[0].moving(mid + DURATION));
    }
    #[test]
    fn direct_scrubbing_cancels_old_motion_and_releases_without_a_jump() {
        let now = Instant::now();
        let mut batch = Batch::default();
        batch.update_at(vec![spec(0.2)], (480.0, 500.0), now);
        batch.update_at(vec![spec(0.8)], (480.0, 500.0), now);
        let mid = now + Duration::from_millis(50);
        let mut pointer = spec(0.41125);
        pointer.direct = true;
        batch.update_at(vec![pointer], (480.0, 500.0), mid);
        assert_eq!(batch.items[0].value(mid), 0.41125);
        assert!(!batch.items[0].moving(mid));
        batch.update_at(vec![spec(0.41125)], (480.0, 500.0), mid);
        assert_eq!(batch.items[0].value(mid), 0.41125);
        assert!(!batch.items[0].moving(mid));
    }
    #[test]
    fn static_unchanged_and_cleared_gauges_request_no_extra_frames() {
        let mut batch = Batch::default();
        assert!(batch.update(vec![spec(0.4)], (480.0, 500.0)));
        assert!(!batch.animating());
        assert!(!batch.update(vec![spec(0.4)], (480.0, 500.0)));
        batch.clear();
        assert!(!batch.animating());
    }
}
