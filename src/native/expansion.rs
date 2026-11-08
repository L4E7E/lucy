//! Geometry shared by Wayland surface sizing and the GPU's joined material.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

pub const CLOCK_SCALE: f32 = 3.0;
pub const CALENDAR_CLOCK_SIZE: f64 = 46.0;
// The enlarged header's centered glyphs share the notification title's ink top.
pub const CALENDAR_CLOCK_Y: f32 = -8.0;
pub const PANEL_OPEN_MS: u64 = 360;
pub const NOTICE_OPEN_MS: u64 = 480;
/// Normalized critically damped response: monotone, with no early clipped
/// overshoot that makes a nominally long expansion snap open in its first 25%.
pub fn relaxed(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let decay = |v: f32| 1.0 - (-6.0 * v).exp() * (1.0 + 6.0 * v);
    (decay(t) / decay(1.0)).clamp(0.0, 1.0)
}

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
        (w as f32 - target.0 as f32) * 0.5 + (if calendar { 320.0 } else { target.0 as f32 }) * 0.5;
    Frame {
        size: (w, h),
        header: (
            origin + (destination - origin) * p - header * scale * 0.5,
            if calendar { CALENDAR_CLOCK_Y * p } else { 0.0 },
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
    panel: bool,
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    last: Cell<f32>,
    origin: Option<(f32, f32)>,
}
impl Expansion {
    pub fn new(source: i32, target: (i32, i32), calendar: bool, panel: bool) -> Self {
        Self {
            source: source as f32,
            target,
            calendar,
            panel,
            from: 0.0,
            to: 1.0,
            start: Instant::now(),
            duration: Duration::from_millis(if panel {
                PANEL_OPEN_MS
            } else if target.1 > 34 {
                NOTICE_OPEN_MS
            } else {
                360
            }),
            last: Cell::new(0.0),
            origin: None,
        }
    }
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }
    pub fn frame(&self, header: f32) -> Frame {
        let t = self.time();
        let ease = if self.closing() {
            t * t * (3.0 - 2.0 * t)
        } else if self.target.1 > 34 {
            relaxed(t)
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
        if let Some(origin) = self.origin {
            let target = if self.closing() {
                (self.source, 34.0)
            } else {
                (self.target.0 as f32, self.target.1 as f32)
            };
            let width = (origin.0 + (target.0 - origin.0) * ease).round().max(1.0) as i32;
            f.header.0 += (width - f.size.0) as f32 * 0.5;
            f.size = (
                width,
                (origin.1 + (target.1 - origin.1) * ease).round().max(1.0) as i32,
            );
        }
        f.time = t;
        f
    }
    pub fn close(&mut self, header: f32) {
        let f = self.frame(header);
        self.origin = Some((f.size.0 as f32, f.size.1 as f32));
        self.from = f.amount;
        self.to = 0.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(240);
        self.last.set(0.0);
    }
    pub fn closing(&self) -> bool {
        self.to == 0.0
    }
    pub fn reopen(&mut self, header: f32) {
        let f = self.frame(header);
        self.origin = Some((f.size.0 as f32, f.size.1 as f32));
        self.from = f.amount;
        self.to = 1.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(if self.panel {
            300
        } else if self.target.1 > 34 {
            360
        } else {
            240
        });
        self.last.set(0.0);
    }
    pub fn animating(&self) -> bool {
        self.time() < 1.0 || self.last.get() < 1.0
    }
    pub fn presented(&self, time: f32) {
        self.last.set(time);
    }
    pub fn retarget(&mut self, target: (i32, i32), header: f32) {
        if self.target == target {
            return;
        }
        let f = self.frame(header);
        self.origin = Some((f.size.0 as f32, f.size.1 as f32));
        self.target = target;
        self.from = f.amount;
        self.to = 1.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(if self.panel {
            300
        } else if target.1 > 34 {
            360
        } else {
            260
        });
        self.last.set(0.0);
    }
    pub fn follow_width(&mut self, width: i32) {
        // The tray already interpolates this layout width each frame. Restarting
        // retarget() here would double-ease and repeatedly lock the whole panel.
        self.target.0 = width.max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closing_returns_to_the_live_tray_width_without_a_last_frame_jump() {
        for (old, current) in [(480, 680), (680, 480)] {
            let mut e = Expansion::new(old, (current, 500), false, true);
            e.start = Instant::now() - Duration::from_secs(1);
            e.close(current as f32);
            e.source = current as f32; // Current collapsed bar, not opening snapshot.
            for t in [0, 40, 120, 200, 240, 300] {
                e.start = Instant::now() - Duration::from_millis(t);
                assert_eq!(e.frame(current as f32).size.0, current);
            }
        }
    }
    #[test]
    fn following_a_tray_width_does_not_restart_a_settled_panel() {
        let mut e = Expansion::new(480, (480, 500), false, true);
        e.start = Instant::now() - Duration::from_secs(1);
        e.presented(1.0);
        let start = e.start;
        for width in [482, 500, 530, 570, 530, 500, 480] {
            e.follow_width(width);
            assert_eq!(e.start, start);
            assert_eq!(e.frame(480.0).size, (width, 500));
            assert!(!e.animating());
        }
    }
    #[test]
    fn panels_and_notifications_open_gradually_without_slowing_the_osd() {
        assert_eq!(relaxed(0.0), 0.0);
        assert_eq!(relaxed(1.0), 1.0);
        assert!(relaxed(0.25) > 0.4 && relaxed(0.25) < 0.5);
        assert!(relaxed(0.75) < 0.97);
        let values: Vec<_> = (0..=100).map(|i| relaxed(i as f32 / 100.0)).collect();
        assert!(values.windows(2).all(|v| v[1] >= v[0]));
        for target in [(480, 500), (780, 500)] {
            assert_eq!(
                Expansion::new(170, target, false, true).duration,
                Duration::from_millis(360)
            );
        }
        assert_eq!(
            Expansion::new(170, (380, 140), false, false).duration,
            Duration::from_millis(480)
        );
        assert_eq!(
            Expansion::new(170, (224, 34), false, false).duration,
            Duration::from_millis(360)
        );
    }
    #[test]
    fn osd_only_changes_width_and_retarget_keeps_the_current_shape() {
        for i in 0..=100 {
            assert_eq!(
                layout(i as f32 / 100.0, 250.0, (360, 34), 250.0, false)
                    .size
                    .1,
                34
            );
        }
        let mut e = Expansion::new(250, (360, 34), false, false);
        e.start = Instant::now() - Duration::from_secs(1);
        e.retarget((380, 140), 250.0);
        assert_eq!(e.frame(250.0).size, (360, 34));
        e.start = Instant::now() - Duration::from_secs(1);
        assert_eq!(e.frame(250.0).size, (380, 140));
        e.close(250.0);
        assert_eq!(e.frame(250.0).size, (380, 140));
    }
    #[test]
    fn begins_at_real_bar_and_finishes_as_one_joined_panel() {
        for (calendar, source, target) in [(false, 480.0, (480, 500)), (true, 220.0, (780, 500))] {
            let a = layout(0.0, source, target, source, calendar);
            let b = layout(1.0, source, target, source, calendar);
            assert_eq!(a.size, (source as i32, 34));
            assert_eq!(a.header, (0.0, 0.0));
            assert_eq!(b.size, target);
            assert_eq!(b.header.1, if calendar { CALENDAR_CLOCK_Y } else { 0.0 });
            let center = b.header.0 + source * b.header_scale / 2.0;
            assert_eq!(b.header_scale, if calendar { CLOCK_SCALE } else { 1.0 });
            assert_eq!(
                center,
                if calendar {
                    160.0
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
