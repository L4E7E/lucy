//! Geometry shared by Wayland surface sizing and the GPU's joined material.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

pub const CLOCK_SCALE: f32 = 3.0;
pub const CALENDAR_CLOCK_SIZE: f64 = 58.0;
// The inset lives inside the cached clock/date canvas, not on its surface.
pub const CALENDAR_CLOCK_Y: f32 = 8.0;
pub const CALENDAR_CLOCK_WIDTH: f32 = 280.0;
pub const CALENDAR_DATE_Y: f32 = 86.0;
pub const CALENDAR_DATE_HEIGHT: f32 = 28.0;
pub const CALENDAR_WIDGET_HEIGHT: f32 = CALENDAR_DATE_Y + CALENDAR_DATE_HEIGHT;
pub const PANEL_OPEN_MS: u64 = 360;
pub const NOTICE_OPEN_MS: u64 = 480;
pub const OSD_OPEN_MS: u64 = 450;

#[cfg(test)]
mod osd_shape_tests {
    #[test]
    fn osd_expands_only_sideways() {
        for step in 0..=100 {
            let frame = super::layout(step as f32 / 100.0, 180.0, (260, 34), 180.0, false);
            assert_eq!(frame.size.1, 34);
            assert_eq!(frame.header_scale, 1.0);
            assert_eq!(frame.header.1, 0.0);
            assert!((180..=260).contains(&frame.size.0));
        }
    }
}
/// Normalized critically damped response: monotone, with no early clipped
/// overshoot that makes a nominally long expansion snap open in its first 25%.
pub fn relaxed(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let decay = |v: f32| 1.0 - (-6.0 * v).exp() * (1.0 + 6.0 * v);
    (decay(t) / decay(1.0)).clamp(0.0, 1.0)
}

fn osd_open(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub size: (i32, i32),
    pub header: (f32, f32),
    pub header_scale: f32,
    pub header_height: f32,
    pub amount: f32,
    pub time: f32,
}
pub fn layout(amount: f32, source: f32, target: (i32, i32), header: f32, calendar: bool) -> Frame {
    let p = amount.clamp(0.0, 1.0);
    let w = (source + (target.0 as f32 - source) * p).round().max(1.0) as i32;
    let h = (34.0 + (target.1 as f32 - 34.0) * p).round().max(1.0) as i32;
    let scale = if calendar {
        // The collapsed date bar and expanded clock/date share one canvas.
        // Starting from the HH:MM raster's width makes it detach from the bar.
        let start = source / header.max(1.0);
        start + (CLOCK_SCALE - start) * p
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
            0.0,
        ),
        header_scale: scale,
        header_height: if calendar {
            34.0 + (CALENDAR_WIDGET_HEIGHT - 34.0) * p
        } else {
            34.0
        },
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
                OSD_OPEN_MS
            }),
            last: Cell::new(0.0),
            origin: None,
        }
    }
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }
    pub fn frame(&self, header: f32) -> Frame {
        self.frame_at(self.time(), header)
    }
    fn frame_at(&self, t: f32, header: f32) -> Frame {
        let ease = if self.closing() {
            t * t * (3.0 - 2.0 * t)
        } else if self.target.1 > 34 {
            relaxed(t)
        } else {
            osd_open(t)
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
        // Reverse from a frame actually committed to Wayland, just like reopen.
        // Sampling wall time here can jump ahead when a frame callback is late.
        let f = self.frame_at(self.last.get(), header);
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
        // Reverse from the last visible geometry, not an unpresented point
        // further along the closing timeline when a callback was delayed.
        let f = self.frame_at(self.last.get(), header);
        self.origin = Some((f.size.0 as f32, f.size.1 as f32));
        self.from = f.amount;
        self.to = 1.0;
        self.start = Instant::now();
        self.duration = Duration::from_millis(if self.panel {
            300
        } else if self.target.1 > 34 {
            360
        } else {
            OSD_OPEN_MS
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
            OSD_OPEN_MS
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
    fn calendar_canvas_follows_the_surface_from_the_full_date_bar() {
        // Different date widths must not change the logical target, or make
        // HH:MM supply the collapsed surface width.
        for source in [154.0, 221.0, 308.0] {
            let header = (CALENDAR_CLOCK_WIDTH / CLOCK_SCALE).ceil();
            let first = layout(0.0, source, (780, 500), header, true);
            assert!(first.header.0.abs() < 0.001);
            assert_eq!(first.header.1, 0.0);
            assert_eq!(first.header_height, 34.0);
            assert!((header * first.header_scale - source).abs() < 0.001);
            let last = layout(1.0, source, (780, 500), header, true);
            assert_eq!(last.header_height, CALENDAR_WIDGET_HEIGHT);
            assert_eq!(last.header.0 + header * last.header_scale * 0.5, 160.0);
            for step in 0..=100 {
                let p = step as f32 / 100.0;
                let f = layout(p, source, (780, 500), header, true);
                let center = f.header.0 + header * f.header_scale * 0.5;
                assert!((center - f.size.0 as f32 * 0.5 + 230.0 * p).abs() < 0.001);
                assert!(
                    (f.header_height - (34.0 + (CALENDAR_WIDGET_HEIGHT - 34.0) * p)).abs() < 0.001
                );
                assert!(f.header_height <= f.size.1 as f32);
            }
        }
    }
    #[test]
    fn calendar_reversal_preserves_the_whole_clock_date_canvas() {
        let header = (CALENDAR_CLOCK_WIDTH / CLOCK_SCALE).ceil();
        for t in [0.05, 0.25, 0.55, 0.85, 1.0] {
            let mut e = Expansion::new(221, (780, 500), true, true);
            e.presented(t);
            let visible = e.frame_at(t, header);
            e.close(header);
            let closing = e.frame_at(0.0, header);
            assert_eq!(closing.size, visible.size);
            assert_eq!(closing.header, visible.header);
            assert_eq!(closing.header_scale, visible.header_scale);
            assert_eq!(closing.header_height, visible.header_height);
            e.presented(0.3);
            let visible = e.frame_at(0.3, header);
            e.reopen(header);
            let reopened = e.frame_at(0.0, header);
            assert_eq!(reopened.size, visible.size);
            assert_eq!(reopened.header, visible.header);
            assert_eq!(reopened.header_scale, visible.header_scale);
            assert_eq!(reopened.header_height, visible.header_height);
        }
    }
    #[test]
    fn closing_during_open_starts_at_the_last_presented_frame() {
        let mut e = Expansion::new(180, (780, 500), true, true);
        e.presented(0.3);
        let visible = e.frame_at(0.3, 90.0);
        e.start = Instant::now() - Duration::from_millis(350);
        e.close(90.0);
        let closed = e.frame_at(0.0, 90.0);
        assert_eq!(closed.size, visible.size);
        assert_eq!(closed.header, visible.header);
        assert_eq!(closed.header_scale, visible.header_scale);
    }
    #[test]
    fn interrupted_close_reopens_from_the_last_presented_size() {
        let mut e = Expansion::new(180, (260, 34), false, false);
        e.start = Instant::now() - Duration::from_secs(1);
        e.presented(1.0);
        e.close(180.0);
        e.presented(0.25);
        let visible = e.frame_at(0.25, 180.0);
        e.start = Instant::now() - Duration::from_millis(180);
        e.reopen(180.0);
        assert_eq!(e.frame_at(0.0, 180.0).size, visible.size);
    }
    #[test]
    fn osd_open_uses_the_whole_duration_instead_of_snapping_early() {
        assert_eq!(osd_open(0.0), 0.0);
        assert_eq!(osd_open(1.0), 1.0);
        assert!(osd_open(0.25) < 0.20);
        assert_eq!(osd_open(0.5), 0.5);
        assert!(osd_open(0.75) < 0.90);
        let mut e = Expansion::new(180, (260, 34), false, false);
        for (ms, low, high) in [(112, 190, 197), (225, 216, 224), (337, 243, 252)] {
            e.start = Instant::now() - Duration::from_millis(ms);
            let f = e.frame(180.0);
            assert!(
                (low..=high).contains(&f.size.0),
                "time={ms} size={:?}",
                f.size
            );
            assert_eq!(f.size.1, 34);
        }
    }
    #[test]
    fn closing_returns_to_the_live_tray_width_without_a_last_frame_jump() {
        for (old, current) in [(480, 680), (680, 480)] {
            let mut e = Expansion::new(old, (current, 500), false, true);
            e.start = Instant::now() - Duration::from_secs(1);
            e.presented(1.0);
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
    fn panels_and_notifications_keep_their_timing_while_osd_opens_more_gently() {
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
            Duration::from_millis(OSD_OPEN_MS)
        );
        let mut osd = Expansion::new(170, (224, 34), false, false);
        osd.close(170.0);
        assert_eq!(osd.duration, Duration::from_millis(240));
        osd.reopen(170.0);
        assert_eq!(osd.duration, Duration::from_millis(OSD_OPEN_MS));
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
        e.presented(1.0);
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
            assert_eq!(b.header.1, 0.0);
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
