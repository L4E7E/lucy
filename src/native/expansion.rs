//! Geometry shared by Wayland surface sizing and the GPU's joined material.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

pub const CLOCK_SCALE: f32 = 1.30;

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub size: (i32, i32),
    pub header: (f32, f32),
    pub header_scale: f32,
    pub amount: f32,
    pub time: f32,
}
pub fn layout(amount: f32, source: f32, target: (i32, i32), header: f32, calendar: bool) -> Frame {
    let p = amount.clamp(0.0, 1.0);
    let w = (source + (target.0 as f32 - source) * p).round().max(1.0) as i32;
    let h = (34.0 + (target.1 as f32 - 34.0) * p).round().max(1.0) as i32;
    let scale = if calendar {
        1.0 + (CLOCK_SCALE - 1.0) * p
    } else {
        1.0
    };
    let origin = w as f32 * 0.5;
    let destination =
        (w as f32 - target.0 as f32) * 0.5 + (if calendar { 332.0 } else { target.0 as f32 }) * 0.5;
    Frame {
        size: (w, h),
        header: (
            origin + (destination - origin) * p - header * scale * 0.5,
            if calendar { 4.0 * p } else { 0.0 },
        ),
        header_scale: scale,
        amount: p,
        time: 1.0,
    }
}
pub struct Expansion {
    pub source: f32,
    pub target: (i32, i32),
    pub calendar: bool,
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    last: Cell<f32>,
}
impl Expansion {
    pub fn new(source: i32, target: (i32, i32), calendar: bool) -> Self {
        Self {
            source: source as f32,
            target,
            calendar,
            from: 0.0,
            to: 1.0,
            start: Instant::now(),
            duration: Duration::from_millis(360),
            last: Cell::new(0.0),
        }
    }
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }
    pub fn frame(&self, header: f32) -> Frame {
        let t = self.time();
        let ease = if self.closing() {
            t * t * (3.0 - 2.0 * t)
        } else {
            super::gpu::spring(t).clamp(0.0, 1.0)
        };
        let mut f = layout(
            self.from + (self.to - self.from) * ease,
            self.source,
            self.target,
            header,
            self.calendar,
        );
        f.time = t;
        f
    }
    pub fn close(&mut self, header: f32) {
        self.from = self.frame(header).amount;
        self.to = 0.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(240);
        self.last.set(0.0);
    }
    pub fn closing(&self) -> bool {
        self.to == 0.0
    }
    pub fn reopen(&mut self, header: f32) {
        self.from = self.frame(header).amount;
        self.to = 1.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(240);
        self.last.set(0.0);
    }
    pub fn animating(&self) -> bool {
        self.time() < 1.0 || self.last.get() < 1.0
    }
    pub fn presented(&self, time: f32) {
        self.last.set(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn begins_at_real_bar_and_finishes_as_one_joined_panel() {
        for (calendar, source, target) in [(false, 480.0, (480, 500)), (true, 220.0, (780, 500))] {
            let a = layout(0.0, source, target, source, calendar);
            let b = layout(1.0, source, target, source, calendar);
            assert_eq!(a.size, (source as i32, 34));
            assert_eq!(a.header, (0.0, 0.0));
            assert_eq!(b.size, target);
            let center = b.header.0 + source * b.header_scale / 2.0;
            assert_eq!(b.header_scale, if calendar { CLOCK_SCALE } else { 1.0 });
            assert_eq!(
                center,
                if calendar {
                    166.0
                } else {
                    target.0 as f32 / 2.0
                }
            );
            let mut last = a.size;
            for step in 1..=100 {
                let f = layout(step as f32 / 100.0, source, target, source, calendar);
                assert!(f.size.0 >= last.0 && f.size.1 >= last.1);
                assert!(f.header_scale >= 1.0 && f.header_scale <= CLOCK_SCALE);
                last = f.size;
            }
        }
    }
}
