use super::model::{self, Action, Hit, Model, Page, Rect, Slider};
use crate::audio_backend as audio;
use cairo::Context;
use glib::translate::{from_glib_full, ToGlibPtr};
use std::f64::consts::PI;

pub const WIDTH: i32 = 480;
pub const HEIGHT: i32 = 500;
pub const SESSION_WIDTH: i32 = 760;
pub const SESSION_HEIGHT: i32 = 176;
pub const WIFI_ROW_PITCH: f64 = 68.0;
const MINT: (f64, f64, f64) = (142.0 / 255.0, 216.0 / 255.0, 190.0 / 255.0);
const WHITE: (f64, f64, f64) = (0.96, 0.97, 1.0);
const WIFI_GREEN: (f64, f64, f64) = (0.24, 0.79, 0.42);
const EXPANDED_INSET: i32 = 24;
thread_local! {
    static GLASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static JOINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DRAW_OFFSET: std::cell::Cell<(f64,f64)> = const { std::cell::Cell::new((0.0,0.0)) };
    static HIT_BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
struct OffsetScope((f64, f64));
impl OffsetScope {
    fn enter(x: f64, y: f64) -> Self {
        Self(DRAW_OFFSET.with(|v| v.replace((x, y))))
    }
}
impl Drop for OffsetScope {
    fn drop(&mut self) {
        DRAW_OFFSET.with(|v| v.set(self.0));
    }
}
fn pointer(m: &Model) -> Option<(u32, f64, f64)> {
    let (x, y) = DRAW_OFFSET.with(|v| v.get());
    m.pointer.map(|(id, px, py)| (id, px - x, py - y))
}
struct JoinedScope(bool);
impl JoinedScope {
    fn enter() -> Self {
        Self(JOINED.with(|v| v.replace(true)))
    }
}
impl Drop for JoinedScope {
    fn drop(&mut self) {
        JOINED.with(|v| v.set(self.0));
    }
}
pub fn is_bar() -> bool {
    BAR.with(|v| v.get())
}
struct BarScope(bool);
impl BarScope {
    fn enter() -> Self {
        Self(BAR.with(|v| v.replace(true)))
    }
}
impl Drop for BarScope {
    fn drop(&mut self) {
        BAR.with(|v| v.set(self.0));
    }
}
pub fn is_glass() -> bool {
    GLASS.with(|v| v.get())
}
struct GlassScope(bool);
impl GlassScope {
    fn enter() -> Self {
        Self(GLASS.with(|v| v.replace(true)))
    }
}
impl Drop for GlassScope {
    fn drop(&mut self) {
        GLASS.with(|v| v.set(self.0));
    }
}

/// Cached vector material, not a desktop capture/refraction pass.
fn crystal(cr: &Context, r: Rect, radius: f64, active: bool) {
    let _ = cr.save();
    round(cr, r, radius);
    let fill = cairo::LinearGradient::new(r.x, r.y, r.x + r.w * 0.65, r.y + r.h);
    fill.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, if active { 0.25 } else { 0.115 });
    fill.add_color_stop_rgba(0.48, 0.91, 0.95, 1.0, if active { 0.12 } else { 0.035 });
    fill.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, if active { 0.16 } else { 0.065 });
    let _ = cr.set_source(&fill);
    let _ = cr.fill();
    let rim = cairo::LinearGradient::new(r.x, r.y, r.x + r.w, r.y + r.h);
    rim.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.35);
    rim.add_color_stop_rgba(0.35, 1.0, 1.0, 1.0, 0.045);
    rim.add_color_stop_rgba(1.0, 0.86, 0.93, 1.0, 0.17);
    round(
        cr,
        Rect {
            x: r.x + 0.6,
            y: r.y + 0.6,
            w: (r.w - 1.2).max(0.0),
            h: (r.h - 1.2).max(0.0),
        },
        radius,
    );
    let _ = cr.set_source(&rim);
    cr.set_line_width(0.8);
    let _ = cr.stroke();
    let _ = cr.restore();
}
fn glass_base(cr: &Context, r: Rect, alpha: f64) {
    round(cr, r, 18.0);
    cr.set_source_rgba(25.0 / 255.0, 27.0 / 255.0, 34.0 / 255.0, alpha);
    let _ = cr.fill();
    crystal(cr, r, 18.0, false);
    let rim = cairo::LinearGradient::new(r.x, r.y, r.x + r.w, r.y + r.h);
    rim.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.62);
    rim.add_color_stop_rgba(0.42, 1.0, 1.0, 1.0, 0.11);
    rim.add_color_stop_rgba(1.0, 0.88, 0.95, 1.0, 0.36);
    round(
        cr,
        Rect {
            x: r.x + 1.0,
            y: r.y + 1.0,
            w: r.w - 2.0,
            h: r.h - 2.0,
        },
        17.0,
    );
    let _ = cr.set_source(&rim);
    cr.set_line_width(1.4);
    let _ = cr.stroke();
}
fn bar_rim(cr: &Context, r: Rect, left: bool) {
    let rim = cairo::LinearGradient::new(r.x, r.y, r.x, r.y + r.h);
    rim.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, if left { 0.62 } else { 0.40 });
    rim.add_color_stop_rgba(0.45, 1.0, 1.0, 1.0, 0.055);
    rim.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, if left { 0.34 } else { 0.24 });
    round(
        cr,
        Rect {
            x: r.x + 0.75,
            y: r.y + 0.75,
            w: r.w - 1.5,
            h: r.h - 1.5,
        },
        13.25,
    );
    let _ = cr.set_source(&rim);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}
fn stack_tail_height(progress: f64, rows: usize) -> f64 {
    if rows > 1 {
        14.0 * (1.0 - progress.clamp(0.0, 1.0))
    } else {
        0.0
    }
}
fn notice_row_offset(index: usize, row_height: f64, progress: f64) -> f64 {
    let p = progress.clamp(0.0, 1.0);
    index as f64 * row_height * p + index.min(2) as f64 * 6.0 * (1.0 - p)
}
fn card_stack_tail(cr: &Context, card: Rect) {
    let _ = cr.save();
    // Rear layers may only peek out below, not through the translucent face.
    cr.rectangle(card.x, card.y + card.h, card.w, 14.0);
    cr.clip();
    for depth in [2.0, 1.0] {
        crystal(
            cr,
            Rect {
                x: card.x + depth * 5.0,
                y: card.y + depth * 6.0,
                w: card.w - depth * 10.0,
                ..card
            },
            16.0,
            false,
        );
    }
    let _ = cr.restore();
}
#[link(name = "pangocairo-1.0")]
extern "C" {
    fn pango_cairo_create_layout(cr: *mut cairo::ffi::cairo_t) -> *mut pango::ffi::PangoLayout;
    fn pango_cairo_show_layout(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
}

pub fn round(cr: &Context, r: Rect, radius: f64) {
    let d = radius.min(r.w / 2.0).min(r.h / 2.0);
    cr.new_sub_path();
    for (x, y, a, b) in [
        (r.x + r.w - d, r.y + d, -0.5, 0.0),
        (r.x + r.w - d, r.y + r.h - d, 0.0, 0.5),
        (r.x + d, r.y + r.h - d, 0.5, 1.0),
        (r.x + d, r.y + d, 1.0, 1.5),
    ] {
        cr.arc(x, y, d, a * PI, b * PI);
    }
    cr.close_path();
}
fn color(cr: &Context, c: (f64, f64, f64), alpha: f64) {
    let c = if is_glass() && c == MINT { WHITE } else { c };
    cr.set_source_rgba(c.0, c.1, c.2, alpha);
}
#[allow(clippy::too_many_arguments)]
pub fn text(
    cr: &Context,
    font: &str,
    s: &str,
    r: Rect,
    tint: bool,
    bold: bool,
    size: f64,
    align: pango::Alignment,
) {
    text_ink(
        cr,
        font,
        s,
        r,
        bold,
        size,
        align,
        if tint { MINT } else { (0.95, 0.96, 0.97) },
    );
}
#[allow(clippy::too_many_arguments)]
fn text_ink(
    cr: &Context,
    font: &str,
    s: &str,
    r: Rect,
    bold: bool,
    size: f64,
    align: pango::Alignment,
    ink: (f64, f64, f64),
) {
    let layout: pango::Layout =
        unsafe { from_glib_full(pango_cairo_create_layout(cr.to_raw_none())) };
    let mut desc = pango::FontDescription::from_string(font);
    if size > 0.0 {
        desc.set_absolute_size(size * f64::from(pango::SCALE));
    }
    if bold {
        desc.set_weight(pango::Weight::Semibold);
    }
    layout.set_font_description(Some(&desc));
    layout.set_text(s);
    layout.set_single_paragraph_mode(true);
    layout.set_width((r.w * f64::from(pango::SCALE)).max(0.0) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_alignment(align);
    let (_, h) = layout.pixel_size();
    cr.move_to(r.x, r.y + (r.h - f64::from(h)) / 2.0);
    color(cr, ink, 1.0);
    unsafe {
        pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0);
    }
}
pub fn measure(cr: &Context, font: &str, s: &str) -> f64 {
    let layout: pango::Layout =
        unsafe { from_glib_full(pango_cairo_create_layout(cr.to_raw_none())) };
    layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
    layout.set_text(s);
    f64::from(layout.pixel_size().0)
}

fn app_name_layout(cr: &Context, font: &str, name: &str, width: f64) -> pango::Layout {
    let layout: pango::Layout =
        unsafe { from_glib_full(pango_cairo_create_layout(cr.to_raw_none())) };
    let mut desc = pango::FontDescription::from_string(font);
    desc.set_absolute_size(12.0 * f64::from(pango::SCALE));
    layout.set_font_description(Some(&desc));
    layout.set_text(name);
    layout.set_width((width * f64::from(pango::SCALE)) as i32);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_ellipsize(pango::EllipsizeMode::None);
    layout.set_alignment(pango::Alignment::Center);
    layout
}
fn app_name(cr: &Context, font: &str, name: &str, r: Rect) {
    app_name_ink(cr, font, name, r, (0.95, 0.96, 0.97));
}
fn launcher_content(r: Rect) -> (Rect, Rect) {
    (
        Rect {
            x: r.x + (r.w - 40.0) / 2.0,
            y: r.y + 8.0,
            w: 40.0,
            h: 40.0,
        },
        Rect {
            x: r.x + 8.0,
            y: r.y + 56.0,
            w: r.w - 16.0,
            h: 32.0,
        },
    )
}
fn wifi_row_layout(y: f64, w: f64) -> (Rect, Rect, Rect, Rect) {
    let icon = Rect {
        x: 32.0,
        y: y + 10.0,
        w: 18.0,
        h: 18.0,
    };
    let action = Rect {
        x: 20.0 + w - 44.0,
        y: y + 14.0,
        w: 32.0,
        h: 32.0,
    };
    let name = Rect {
        x: icon.x + icon.w + 12.0,
        y: y + 7.0,
        w: action.x - 12.0 - (icon.x + icon.w + 12.0),
        h: 24.0,
    };
    let detail = Rect {
        y: y + 32.0,
        h: 18.0,
        ..name
    };
    (icon, name, detail, action)
}
fn app_name_ink(cr: &Context, font: &str, name: &str, r: Rect, ink: (f64, f64, f64)) {
    let layout = app_name_layout(cr, font, name, r.w);
    let height: f64 = layout
        .lines_readonly()
        .iter()
        .take(2)
        .map(|line| f64::from(line.pixel_extents().1.height()))
        .sum();
    let height = height.min(r.h);
    let _ = cr.save();
    cr.rectangle(r.x, r.y + (r.h - height) / 2.0, r.w, height);
    cr.clip();
    cr.move_to(r.x, r.y + (r.h - height) / 2.0);
    color(cr, ink, 1.0);
    unsafe {
        pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0);
    }
    let _ = cr.restore();
}
pub fn audio_icon(cr: &Context, r: Rect, mic: bool, muted: bool, level: f64) {
    let ink = if is_glass() && level >= 12.0 {
        (0.20, 0.22, 0.26)
    } else {
        (0.97, 0.99, 0.98)
    };
    let _ = cr.save();
    cr.translate(r.x + (r.w - 24.0) / 2.0, r.y + (r.h - 24.0) / 2.0);
    cr.set_source_rgb(ink.0, ink.1, ink.2);
    cr.set_line_width(1.8);
    cr.set_line_cap(cairo::LineCap::Round);
    if mic {
        if muted {
            cr.rectangle(0.0, 0.0, 24.0, 24.0);
            cr.move_to(3.0, 4.0);
            cr.line_to(22.0, 23.0);
            cr.line_to(24.0, 21.0);
            cr.line_to(5.0, 2.0);
            cr.close_path();
            cr.set_fill_rule(cairo::FillRule::EvenOdd);
            cr.clip();
        }
        round(
            cr,
            Rect {
                x: 8.5,
                y: 4.5,
                w: 7.0,
                h: 11.0,
            },
            3.5,
        );
        let _ = cr.fill();
        cr.arc(12.0, 12.0, 6.0, 0.0, PI);
        cr.move_to(12.0, 18.0);
        cr.line_to(12.0, 21.0);
        cr.move_to(8.0, 21.0);
        cr.line_to(16.0, 21.0);
    } else {
        cr.move_to(3.0, 8.5);
        cr.line_to(7.0, 8.5);
        cr.line_to(13.0, 4.0);
        cr.line_to(13.0, 20.0);
        cr.line_to(7.0, 15.5);
        cr.line_to(3.0, 15.5);
        cr.close_path();
        let _ = cr.fill();
        if !muted {
            for (i, radius) in [4.0, 6.5, 9.0].into_iter().enumerate() {
                let opacity = ((level / 100.0 * 3.0) - i as f64).clamp(0.0, 1.0);
                cr.new_sub_path();
                cr.arc(12.0, 12.0, radius, -0.7, 0.7);
                cr.set_source_rgba(ink.0, ink.1, ink.2, opacity);
                let _ = cr.stroke();
            }
            cr.set_source_rgb(ink.0, ink.1, ink.2);
        }
    }
    let _ = cr.stroke();
    if mic && muted {
        let _ = cr.restore();
        let _ = cr.save();
        cr.translate(r.x + (r.w - 24.0) / 2.0, r.y + (r.h - 24.0) / 2.0);
        cr.set_source_rgb(0.97, 0.99, 0.98);
        cr.set_line_width(1.8);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.move_to(5.0, 4.0);
        cr.line_to(21.0, 20.0);
        let _ = cr.stroke();
    }
    if muted && !mic {
        cr.move_to(17.0, 9.0);
        cr.line_to(22.0, 15.0);
        cr.move_to(22.0, 9.0);
        cr.line_to(17.0, 15.0);
        let _ = cr.stroke();
    }
    let _ = cr.restore();
}
fn brightness_icon(cr: &Context, r: Rect, level: f64) {
    let _ = cr.save();
    cr.translate(r.x + r.w / 2.0, r.y + r.h / 2.0);
    cr.new_path();
    if is_glass() && level >= 12.0 {
        cr.set_source_rgb(0.20, 0.22, 0.26);
    } else {
        cr.set_source_rgb(0.97, 0.99, 0.98);
    }
    cr.arc(0.0, 0.0, 4.0, 0.0, 2.0 * PI);
    let _ = cr.fill();
    cr.set_line_width(1.7);
    cr.set_line_cap(cairo::LineCap::Round);
    let end = 7.0 + level.clamp(0.0, 100.0) * 0.04;
    for i in 0..8 {
        let angle = f64::from(i) * PI / 4.0;
        cr.move_to(angle.cos() * 6.5, angle.sin() * 6.5);
        cr.line_to(angle.cos() * end, angle.sin() * end);
    }
    let _ = cr.stroke();
    let _ = cr.restore();
}
fn spinner(cr: &Context, x: f64, y: f64) {
    let _ = cr.save();
    cr.new_path();
    let angle = (glib::monotonic_time() % 1_000_000) as f64 / 1_000_000.0 * PI * 2.0;
    cr.arc(x, y, 6.0, angle, angle + PI * 1.4);
    color(cr, MINT, 1.0);
    cr.set_line_width(2.0);
    cr.set_line_cap(cairo::LineCap::Round);
    let _ = cr.stroke();
    let _ = cr.restore();
}
pub fn gauge(cr: &Context, r: Rect, value: f64) {
    let _ = cr.save();
    round(cr, r, 18.0);
    cr.clip();
    cr.set_source_rgba(0.95, 0.96, 0.97, 0.13);
    let _ = cr.paint();
    let w = r.w * value.clamp(0.0, 100.0) / 100.0;
    if w > 0.0 {
        round(cr, Rect { w, ..r }, 18.0);
        if is_glass() {
            cr.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            color(cr, MINT, 1.0);
        }
        let _ = cr.fill();
    }
    let _ = cr.restore();
}

fn gauge_icon(cr: &Context, gauge: Rect, icon: Rect, value: f64, paint: impl FnOnce(&Context)) {
    if !is_glass() {
        paint(cr);
        return;
    }
    let _ = cr.save();
    // A small alpha mask keeps both halves readable even across the 1~8% fill.
    cr.rectangle(icon.x - 2.0, icon.y - 2.0, icon.w + 4.0, icon.h + 4.0);
    cr.clip();
    cr.push_group();
    paint(cr);
    if let Ok(mask) = cr.pop_group() {
        color(cr, WHITE, 1.0);
        let _ = cr.mask(&mask);
        round(
            cr,
            Rect {
                w: gauge.w * value.clamp(0.0, 100.0) / 100.0,
                ..gauge
            },
            18.0,
        );
        cr.clip();
        cr.set_source_rgb(0.20, 0.22, 0.26);
        let _ = cr.mask(&mask);
    }
    let _ = cr.restore();
}

pub struct Scene {
    pub hits: Vec<Hit>,
    pub content_height: f64,
    pub viewport_height: f64,
}
struct Paint<'a> {
    cr: &'a Context,
    m: &'a Model,
    id: u32,
    hits: Vec<Hit>,
    clip: Option<Rect>,
}
impl Paint<'_> {
    fn access_point(&mut self, ap: &crate::network_backend::Ap, y: f64, w: f64) {
        if !ap.active {
            crystal(
                self.cr,
                Rect {
                    x: 20.0,
                    y,
                    w,
                    h: 60.0,
                },
                16.0,
                self.m
                    .wifi
                    .as_ref()
                    .is_some_and(|selected| selected.bssid == ap.bssid),
            );
        }
        let (icon, name, detail, action) = wifi_row_layout(y, w);
        let secured = !ap.security.is_empty() && ap.security != "--";
        super::icons::status_colored(
            self.cr,
            icon,
            if secured { "lock" } else { "network" },
            "",
            if ap.active { (0.55, 0.80, 1.0) } else { WHITE },
        );
        self.label(&ap.ssid, name, false, ap.active);
        text(
            self.cr,
            &self.m.font,
            &format!(
                "{}% · {}",
                ap.signal,
                if ap.active {
                    "연결됨"
                } else if ap.security == "--" {
                    "공개"
                } else {
                    &ap.security
                }
            ),
            detail,
            false,
            false,
            12.0,
            pango::Alignment::Left,
        );
        self.button(
            action,
            if ap.active { "×" } else { "link" },
            Action::Ap(ap.clone()),
            false,
            if ap.active { "연결 해제" } else { "연결" },
        );
    }
    fn header_button(&mut self, r: Rect, action: Action) {
        crystal(self.cr, r, 11.0, self.hot());
        super::icons::status(self.cr, r, "power", "");
        self.hit(r, action, "전원");
    }
    fn wifi_controls(&mut self, y: f64) {
        let on = self.m.wifi_enabled();
        crystal(
            self.cr,
            Rect {
                x: 20.0,
                y,
                w: f64::from(WIDTH) - 40.0,
                h: 44.0,
            },
            14.0,
            false,
        );
        self.label(
            "Wi-Fi",
            Rect {
                x: 32.0,
                y: y + 7.0,
                w: 42.0,
                h: 30.0,
            },
            false,
            true,
        );
        let toggle = Rect {
            x: 84.0,
            y: y + 10.0,
            w: 44.0,
            h: 24.0,
        };
        round(self.cr, toggle, 12.0);
        color(
            self.cr,
            if on { WIFI_GREEN } else { (0.60, 0.63, 0.65) },
            if on { 0.85 } else { 0.30 },
        );
        let _ = self.cr.fill();
        self.cr.new_path();
        self.cr.arc(
            toggle.x + if on { 32.0 } else { 12.0 },
            toggle.y + 12.0,
            9.0,
            0.0,
            2.0 * PI,
        );
        self.cr.set_source_rgb(0.96, 0.98, 0.97);
        let _ = self.cr.fill();
        if !self.m.busy {
            self.hit(toggle, Action::Wifi, "Wi-Fi 전원");
        }
        let refresh = Rect {
            x: WIDTH as f64 - 64.0,
            y: y + 6.0,
            w: 32.0,
            h: 32.0,
        };
        let enabled = on && !self.m.busy;
        crystal(self.cr, refresh, 10.0, self.hot() && enabled);
        if self.m.busy && self.m.radio_pending != Some(false) && !self.m.auth_prompt {
            spinner(
                self.cr,
                refresh.x + refresh.w / 2.0,
                refresh.y + refresh.h / 2.0,
            );
        } else {
            let _ = self.cr.save();
            self.cr.new_path();
            color(
                self.cr,
                if enabled { MINT } else { (0.6, 0.63, 0.65) },
                if enabled { 1.0 } else { 0.3 },
            );
            self.cr.set_line_width(1.8);
            self.cr.set_line_cap(cairo::LineCap::Round);
            let (x, cy) = (refresh.x + refresh.w / 2.0, refresh.y + refresh.h / 2.0);
            for angle in [0.0, PI] {
                self.cr.new_sub_path();
                self.cr.arc(x, cy, 7.0, angle - 1.1, angle + 0.9);
                let end = (
                    x + 7.0 * (angle + 0.9).cos(),
                    cy + 7.0 * (angle + 0.9).sin(),
                );
                let (nx, ny) = ((angle + 0.9).cos(), (angle + 0.9).sin());
                let (tx, ty) = (-ny, nx);
                self.cr
                    .move_to(end.0 - 3.5 * tx + 2.5 * nx, end.1 - 3.5 * ty + 2.5 * ny);
                self.cr.line_to(end.0, end.1);
                self.cr
                    .line_to(end.0 - 3.5 * tx - 2.5 * nx, end.1 - 3.5 * ty - 2.5 * ny);
                let _ = self.cr.stroke();
            }
            let _ = self.cr.restore();
        }
        if enabled {
            self.hit(refresh, Action::Scan, "새로고침");
        }
    }
    fn label(&self, s: &str, r: Rect, tint: bool, bold: bool) {
        text(
            self.cr,
            &self.m.font,
            s,
            r,
            tint,
            bold,
            0.0,
            pango::Alignment::Left,
        );
    }
    fn hit(&mut self, mut r: Rect, a: Action, _tip: &str) {
        if let Some(c) = self.clip {
            let (x, y) = (r.x.max(c.x), r.y.max(c.y));
            let (right, bottom) = ((r.x + r.w).min(c.x + c.w), (r.y + r.h).min(c.y + c.h));
            if right <= x || bottom <= y {
                return;
            }
            r = Rect {
                x,
                y,
                w: right - x,
                h: bottom - y,
            };
        }
        self.hits.push(Hit { rect: r, action: a });
    }
    fn hot(&self) -> bool {
        self.m.hover == Some((self.id, self.hits.len() + HIT_BASE.with(|v| v.get())))
    }
    fn button(&mut self, r: Rect, s: &str, a: Action, active: bool, tip: &str) {
        let r = if s.chars().count() == 1 && !(s == "›" && r.h > r.w) {
            Rect {
                x: r.x + (r.w - r.h) / 2.0,
                w: r.h,
                ..r
            }
        } else {
            r
        };
        crystal(self.cr, r, (r.h * 0.32).min(18.0), self.hot() || active);
        if s == "link" {
            super::icons::status_colored(self.cr, r, "link", "", (0.36, 0.67, 1.0));
        } else if s == "×" || s == "‹" || s == "›" {
            let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            self.cr.new_path();
            if s == "×" {
                self.cr.set_source_rgb(1.0, 0.64, 0.67);
            } else {
                color(self.cr, WHITE, 0.95);
            }
            self.cr.set_line_width(1.8);
            self.cr.set_line_cap(cairo::LineCap::Round);
            if s == "×" {
                self.cr.move_to(cx - 3.5, cy - 3.5);
                self.cr.line_to(cx + 3.5, cy + 3.5);
                self.cr.move_to(cx + 3.5, cy - 3.5);
                self.cr.line_to(cx - 3.5, cy + 3.5);
            } else {
                let d = if s == "›" { 1.0 } else { -1.0 };
                self.cr.move_to(cx - 2.5 * d, cy - 5.0);
                self.cr.line_to(cx + 2.5 * d, cy);
                self.cr.line_to(cx - 2.5 * d, cy + 5.0);
            }
            let _ = self.cr.stroke();
        } else {
            text(
                self.cr,
                &self.m.font,
                s,
                r,
                active,
                false,
                0.0,
                pango::Alignment::Center,
            );
        }
        self.hit(r, a, tip);
    }
    fn slider(&mut self, y: f64, w: f64, v: f64, target: Slider, mic: bool, muted: bool) {
        let r = Rect {
            x: 20.0,
            y,
            w,
            h: 38.0,
        };
        gauge(self.cr, r, v);
        self.hit(r, Action::Slider(target.clone()), "드래그 또는 휠로 조절");
        let icon = Rect {
            x: 26.0,
            y: y + 5.0,
            w: 30.0,
            h: 28.0,
        };
        if target != Slider::Brightness && self.hot() {
            round(self.cr, icon, 7.0);
            self.cr.set_source_rgba(0.0, 0.0, 0.0, 0.22);
            let _ = self.cr.fill();
        }
        gauge_icon(self.cr, r, icon, v, |cr| {
            if target == Slider::Brightness {
                brightness_icon(cr, icon, self.m.icon_value("brightness", v));
            } else {
                let key = format!("{target:?}");
                audio_icon(cr, icon, mic, muted, self.m.icon_value(&key, v));
            }
        });
        if target != Slider::Brightness {
            self.hit(
                icon,
                match &target {
                    Slider::Volume => Action::Mute,
                    Slider::Audio(k, i) => Action::AudioMute(k.clone(), *i, muted),
                    _ => unreachable!(),
                },
                if muted {
                    "음소거 해제"
                } else {
                    "음소거"
                },
            );
        }
        text_ink(
            self.cr,
            &self.m.font,
            &format!("{v:.0}%"),
            Rect {
                x: r.x + r.w - 61.0,
                y,
                w: 49.0,
                h: 38.0,
            },
            false,
            0.0,
            pango::Alignment::Right,
            if v >= 94.0 { (0.20, 0.22, 0.26) } else { WHITE },
        );
    }
    fn secret(&mut self, y: f64) {
        let r = Rect {
            x: 28.0,
            y,
            w: WIDTH as f64 - 56.0,
            h: 40.0,
        };
        crystal(self.cr, r, 13.0, false);
        let value = if self.m.reveal {
            self.m.secret.clone()
        } else {
            "•".repeat(self.m.secret.chars().count())
        };
        let _ = self.cr.save();
        self.cr.rectangle(38.0, y, r.w - 88.0, 40.0);
        self.cr.clip();
        let before = if self.m.reveal {
            self.m.secret[..self.m.cursor].to_owned()
        } else {
            "•".repeat(self.m.secret[..self.m.cursor].chars().count())
        };
        let cursor_width = measure(self.cr, &self.m.font, &before);
        let left = 40.0 - (cursor_width - (r.w - 100.0)).max(0.0);
        if self.m.selected_all {
            round(
                self.cr,
                Rect {
                    x: left,
                    y: y + 8.0,
                    w: measure(self.cr, &self.m.font, &value).max(2.0),
                    h: 24.0,
                },
                3.0,
            );
            color(self.cr, MINT, 0.25);
            let _ = self.cr.fill();
        }
        self.label(
            if value.is_empty() {
                "비밀번호"
            } else {
                &value
            },
            Rect {
                x: left,
                y,
                w: measure(self.cr, &self.m.font, &value).max(r.w - 86.0) + 4.0,
                h: 40.0,
            },
            false,
            false,
        );
        if !self.m.preedit.is_empty() {
            self.label(
                &self.m.preedit,
                Rect {
                    x: left + cursor_width,
                    y,
                    w: 100.0,
                    h: 40.0,
                },
                true,
                false,
            );
        }
        if self.m.entry {
            color(self.cr, MINT, 1.0);
            self.cr.set_line_width(1.0);
            self.cr.move_to(left + cursor_width, y + 10.0);
            self.cr.line_to(left + cursor_width, y + 30.0);
            let _ = self.cr.stroke();
        }
        let _ = self.cr.restore();
        self.hit(Rect { w: r.w - 82.0, ..r }, Action::Secret, "비밀번호 입력");
        self.button(
            Rect {
                x: r.x + r.w - 76.0,
                y: y + 4.0,
                w: 32.0,
                h: 32.0,
            },
            if self.m.reveal { "◉" } else { "◎" },
            Action::Eye,
            self.m.reveal,
            "비밀번호 표시 / 숨기기",
        );
        self.button(
            Rect {
                x: r.x + r.w - 38.0,
                y: y + 4.0,
                w: 32.0,
                h: 32.0,
            },
            "↵",
            Action::Submit,
            false,
            "연결",
        );
    }
}
fn notification_body_layout(cr: &Context, font: &str, body: &str, width: f64) -> pango::Layout {
    type Cache = std::collections::HashMap<(String, String, i32), pango::Layout>;
    thread_local! { static LAYOUTS: std::cell::RefCell<Cache> = std::cell::RefCell::new(Cache::new()); }
    let key = (font.to_owned(), body.to_owned(), (width * 64.0) as i32);
    if let Some(layout) = LAYOUTS.with(|cache| cache.borrow().get(&key).cloned()) {
        return layout;
    }
    let layout = app_name_layout(cr, font, &body.replace('\n', " "), width);
    let mut desc = pango::FontDescription::from_string(font);
    desc.set_absolute_size(14.0 * f64::from(pango::SCALE));
    layout.set_font_description(Some(&desc));
    layout.set_alignment(pango::Alignment::Left);
    layout.set_height(-3);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    LAYOUTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, layout.clone());
    });
    layout
}
fn sunday_offset(year: i32, month: i32) -> i32 {
    glib::DateTime::from_local(year, month, 1, 12, 0, 0.0)
        .unwrap()
        .day_of_week()
        % 7
}
fn notification_list(p: &mut Paint<'_>, height: i32) -> (f64, f64) {
    let cr = p.cr;
    let m = p.m;
    let x = super::notifications::LIST_X;
    let w = f64::from(super::notifications::WIDTH) - x - 20.0;
    let viewport = f64::from(height) - 82.0;
    cr.rectangle(x - 16.0, 20.0, 1.0, f64::from(height) - 40.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
    let _ = cr.fill();
    text(
        cr,
        &m.font,
        "알림",
        Rect {
            x,
            y: 18.0,
            w: w - 88.0,
            h: 30.0,
        },
        false,
        true,
        16.0,
        pango::Alignment::Left,
    );
    for (offset, key, action, active) in [
        (
            76.0,
            "notification",
            Action::NotificationDnd,
            m.notifications.dnd,
        ),
        (36.0, "trash", Action::ClearNotices, false),
    ] {
        let r = Rect {
            x: x + w - offset,
            y: 18.0,
            w: 32.0,
            h: 32.0,
        };
        let hot = pointer(m).is_some_and(|(id, px, py)| id == p.id && r.contains(px, py));
        crystal(cr, r, 10.0, active || hot);
        super::icons::status_colored(
            cr,
            r,
            key,
            if m.notifications.dnd { "󰂛" } else { "" },
            (0.95, 0.96, 0.97),
        );
        p.hit(r, action, "");
    }
    let _ = cr.save();
    let clip = Rect {
        x,
        y: 62.0,
        w,
        h: viewport,
    };
    cr.rectangle(x, 62.0, w, viewport);
    cr.clip();
    p.clip = Some(clip);
    let mut content = 0.0;
    for (key, rows) in super::notifications::groups(&m.notifications.notices) {
        let expanded = m.expanded_notifications.contains(&key);
        let progress = m.motion_value(
            &format!("notice:{key}"),
            if expanded { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(380),
        );
        let count = if expanded || progress > 0.001 {
            rows.len()
        } else {
            1
        };
        let stacked = progress < 1.0 && rows.len() > 1;
        let tail_height = stack_tail_height(progress, rows.len());
        let weights: Vec<f64> = (0..count)
            .map(|i| {
                if i == 0 {
                    1.0
                } else {
                    (progress * 1.30 - (i as f64 * 0.055).min(0.30)).clamp(0.0, 1.0)
                }
            })
            .collect();
        let bodies: Vec<_> = rows
            .iter()
            .map(|n| notification_body_layout(cr, &m.font, &n.body, w - 32.0))
            .collect();
        let card_height = 56.0
            + bodies
                .iter()
                .map(|l| f64::from(l.pixel_size().1))
                .fold(18.0_f64, f64::max);
        let row_height = card_height + 10.0;
        let group_y = 62.0 + content - m.scroll;
        let group_height = 48.0
            + row_height * (1.0 + rows.len().saturating_sub(1) as f64 * progress)
            + tail_height;
        content += group_height + 18.0;
        if group_y + group_height < 62.0 || group_y > 62.0 + viewport {
            continue;
        }
        let header = Rect {
            x,
            y: group_y,
            w,
            h: 42.0,
        };
        crystal(cr, header, 14.0, false);
        if rows.len() > 1 {
            p.hit(header, Action::NoticeGroup(key.clone()), "");
        }
        let icon_rect = Rect {
            x: x + 8.0,
            y: group_y + 9.0,
            w: 24.0,
            h: 24.0,
        };
        if let Some(icon) = m.notifications.icons.get(&rows[0].icon_name) {
            if let Ok(image) = cairo::ImageSurface::create_for_data(
                icon.bytes.clone(),
                cairo::Format::ARgb32,
                icon.width,
                icon.height,
                icon.width * 4,
            ) {
                let _ = cr.save();
                cr.translate(icon_rect.x, icon_rect.y);
                cr.scale(
                    24.0 / f64::from(image.width()),
                    24.0 / f64::from(image.height()),
                );
                let _ = cr.set_source_surface(image, 0.0, 0.0);
                let _ = cr.paint();
                let _ = cr.restore();
            }
        } else {
            round(cr, icon_rect, 8.0);
            color(cr, MINT, 0.13);
            let _ = cr.fill();
            super::icons::status(cr, icon_rect, "notification", "");
        }
        text(
            cr,
            &m.font,
            if rows[0].app.is_empty() {
                "알림"
            } else {
                &rows[0].app
            },
            Rect {
                x: x + 42.0,
                y: group_y,
                w: w - 80.0,
                h: 42.0,
            },
            true,
            true,
            15.0,
            pango::Alignment::Left,
        );
        if rows.len() > 1 {
            let r = Rect {
                x: x + w - 34.0,
                y: group_y + 6.0,
                w: 30.0,
                h: 30.0,
            };
            let hot = pointer(m).is_some_and(|(id, px, py)| id == p.id && r.contains(px, py));
            if hot {
                round(cr, r, 8.0);
                color(cr, MINT, 0.12);
                let _ = cr.fill();
            }
            super::icons::chevron(cr, r, expanded);
        }
        // Paint rear cards first, translating them into the collapsed stack.
        // Keep glyphs full-size; cropping each card's height made thin strips.
        for (index, notice) in rows.iter().take(count).enumerate().rev() {
            if weights[index] <= 0.001 {
                continue;
            }
            let y = group_y + 48.0 + notice_row_offset(index, row_height, progress);
            let card = Rect {
                x,
                y,
                w,
                h: card_height,
            };
            if y + row_height < 62.0 || y > 62.0 + viewport {
                continue;
            }
            let _ = cr.save();
            let reveal_y = if index == 0 {
                y
            } else {
                // Translucent faces must not reveal other cards' text beneath
                // them. Only the portion below the preceding card is exposed.
                y.max(
                    group_y
                        + 48.0
                        + notice_row_offset(index - 1, row_height, progress)
                        + card_height,
                )
            }
            .max(clip.y);
            let reveal = Rect {
                x,
                y: reveal_y,
                w,
                h: ((y + card_height + if index == 0 { tail_height } else { 3.0 })
                    .min(clip.y + clip.h)
                    - reveal_y)
                    .max(0.0),
            };
            cr.rectangle(reveal.x, reveal.y, reveal.w, reveal.h);
            cr.clip();
            p.clip = Some(reveal);
            let fading = index > 0 && weights[index] < 1.0;
            if fading {
                cr.push_group();
            }
            if stacked && index == 0 {
                card_stack_tail(cr, card);
            }
            // Soft offset shadow and separate tonal surfaces, never an outline.
            round(
                cr,
                Rect {
                    x: card.x,
                    y: y + 3.0,
                    w: card.w,
                    h: card.h,
                },
                12.0,
            );
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.16);
            let _ = cr.fill();
            let hovered = pointer(m).is_some_and(|(id, px, py)| {
                id == p.id && card.contains(px, py) && clip.contains(px, py)
            });
            crystal(cr, card, 16.0, hovered);
            p.hit(card, Action::NoticeHover, "");
            text(
                cr,
                &m.font,
                &notice.summary,
                Rect {
                    x: card.x + 14.0,
                    y: y + 9.0,
                    w: card.w - 58.0,
                    h: 27.0,
                },
                false,
                true,
                16.0,
                pango::Alignment::Left,
            );
            if notice.active {
                cr.new_path();
                cr.arc(card.x + card.w - 47.0, y + 22.0, 2.0, 0.0, 2.0 * PI);
                color(cr, MINT, 1.0);
                let _ = cr.fill();
            }
            let layout = &bodies[index];
            let _ = cr.save();
            cr.rectangle(
                card.x + 14.0,
                y + 42.0,
                card.w - 28.0,
                f64::from(layout.pixel_size().1),
            );
            cr.clip();
            cr.move_to(card.x + 14.0, y + 42.0);
            color(cr, (0.93, 0.94, 0.96), 1.0);
            unsafe {
                pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0);
            }
            let _ = cr.restore();
            let r = Rect {
                x: card.x + card.w - 34.0,
                y: y + 7.0,
                w: 28.0,
                h: 28.0,
            };
            if hovered {
                let hot = pointer(m).is_some_and(|(_, px, py)| r.contains(px, py));
                round(cr, r, 14.0);
                cr.set_source_rgba(1.0, 1.0, 1.0, if hot { 0.20 } else { 0.09 });
                let _ = cr.fill();
                text(
                    cr,
                    &m.font,
                    "×",
                    r,
                    false,
                    false,
                    21.0,
                    pango::Alignment::Center,
                );
            }
            p.hit(r, Action::DeleteNotices(vec![notice.id]), "");
            if fading {
                let _ = cr.pop_group_to_source();
                let _ = cr.paint_with_alpha(weights[index].powi(2));
            }
            p.clip = Some(clip);
            let _ = cr.restore();
        }
    }
    if m.notifications.notices.is_empty() {
        p.label(
            if !m.notifications.ready {
                "알림 불러오는 중…"
            } else if !m.notifications.error.is_empty() {
                &m.notifications.error
            } else {
                "알림이 없습니다."
            },
            Rect {
                x: x + 12.0,
                y: 75.0,
                w: w - 24.0,
                h: 40.0,
            },
            false,
            false,
        );
    }
    p.clip = None;
    let _ = cr.restore();
    (content, viewport)
}
pub fn popup_height(m: &Model) -> i32 {
    if m.page == Page::Calendar {
        super::notifications::HEIGHT
    } else if m.page == Page::Launcher {
        super::launcher::HEIGHT
    } else if m.page == Page::Session {
        SESSION_HEIGHT
    } else if m.page == Page::TrayMenu {
        (m.menu.len().min(1000) as i32 * 34 + 40).clamp(74, HEIGHT)
    } else {
        HEIGHT
    }
}
pub const NOTICE_WIDTH: i32 = 380;
pub fn notice_height(m: &Model, id: u32) -> i32 {
    let Some(n) = m.popup_notices.iter().find(|n| n.id == id) else {
        return 120;
    };
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
    let cr = Context::new(&surface).unwrap();
    let body = notification_body_layout(&cr, &m.font, &n.body, f64::from(NOTICE_WIDTH) - 32.0);
    let actions = n.actions.keys().any(|k| k != "default");
    (body.pixel_size().1 + 88 + if actions { 38 } else { 0 }).max(120)
}
pub fn notice_popup(cr: &Context, m: &Model, window: u32, id: u32, height: i32) -> Scene {
    let _palette = GlassScope::enter();
    let w = f64::from(NOTICE_WIDTH);
    let h = f64::from(height);
    round(
        cr,
        Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        },
        12.0,
    );
    cr.new_path();
    glass_base(
        cr,
        Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        },
        0.58,
    );
    let mut p = Paint {
        cr,
        m,
        id: window,
        hits: vec![],
        clip: None,
    };
    if let Some(n) = m.popup_notices.iter().find(|n| n.id == id) {
        p.hit(
            Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            Action::InvokeNotice(id, "default".into()),
            "",
        );
        let icon_rect = Rect {
            x: 14.0,
            y: 12.0,
            w: 24.0,
            h: 24.0,
        };
        if let Some(icon) = m.notifications.icons.get(&n.icon_name) {
            if let Ok(image) = cairo::ImageSurface::create_for_data(
                icon.bytes.clone(),
                cairo::Format::ARgb32,
                icon.width,
                icon.height,
                icon.width * 4,
            ) {
                let _ = cr.save();
                cr.translate(icon_rect.x, icon_rect.y);
                cr.scale(24.0 / f64::from(icon.width), 24.0 / f64::from(icon.height));
                let _ = cr.set_source_surface(image, 0.0, 0.0);
                let _ = cr.paint();
                let _ = cr.restore();
            }
        } else {
            super::icons::status_colored(
                cr,
                icon_rect,
                "notification",
                "",
                if n.urgency == 2 {
                    (1.0, 0.61, 0.66)
                } else {
                    (0.55, 0.76, 1.0)
                },
            );
        }
        text(
            cr,
            &m.font,
            if n.app.is_empty() { "알림" } else { &n.app },
            Rect {
                x: 48.0,
                y: 10.0,
                w: w - 100.0,
                h: 28.0,
            },
            true,
            true,
            14.0,
            pango::Alignment::Left,
        );
        text(
            cr,
            &m.font,
            &n.summary,
            Rect {
                x: 16.0,
                y: 40.0,
                w: w - 32.0,
                h: 26.0,
            },
            false,
            true,
            15.0,
            pango::Alignment::Left,
        );
        let body = notification_body_layout(cr, &m.font, &n.body, w - 32.0);
        cr.move_to(16.0, 72.0);
        color(cr, (0.95, 0.96, 0.97), 1.0);
        unsafe {
            pango_cairo_show_layout(cr.to_raw_none(), body.to_glib_none().0);
        }
        p.button(
            Rect {
                x: w - 40.0,
                y: 10.0,
                w: 28.0,
                h: 28.0,
            },
            "×",
            Action::DeleteNotices(vec![id]),
            false,
            "",
        );
        let mut actions: Vec<_> = n
            .actions
            .iter()
            .filter(|(k, _)| k.as_str() != "default")
            .collect();
        actions.sort_by_key(|(k, _)| *k);
        let count = actions.len().min(3);
        for (i, (key, label)) in actions.into_iter().take(count).enumerate() {
            let width = (w - 32.0 - (count.saturating_sub(1) as f64) * 6.0) / count as f64;
            p.button(
                Rect {
                    x: 16.0 + i as f64 * (width + 6.0),
                    y: h - 38.0,
                    w: width,
                    h: 28.0,
                },
                label,
                Action::InvokeNotice(id, key.clone()),
                false,
                "",
            );
        }
    }
    Scene {
        hits: p.hits,
        content_height: 0.0,
        viewport_height: 0.0,
    }
}
pub fn popup_width(m: &Model) -> i32 {
    if m.page == Page::Calendar {
        super::notifications::WIDTH
    } else if m.page == Page::Launcher {
        super::launcher::WIDTH
    } else if m.page == Page::Session {
        SESSION_WIDTH
    } else if m.page == Page::TrayMenu {
        340
    } else {
        WIDTH
    }
}
pub fn popup(cr: &Context, m: &Model, id: u32, height: i32) -> Scene {
    let _palette = GlassScope::enter();
    if m.page == Page::Session {
        return session(cr, m, id);
    }
    let width = popup_width(m);
    if !JOINED.with(|v| v.get()) {
        round(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: f64::from(height),
            },
            18.0,
        );
        glass_base(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: f64::from(height),
            },
            0.54,
        );
    }
    let mut p = Paint {
        cr,
        m,
        id,
        hits: vec![],
        clip: None,
    };
    let w = f64::from(width) - 40.0;
    let mut content = 0.0;
    let mut viewport = 0.0;
    if m.page == Page::Launcher {
        let search = Rect {
            x: 20.0,
            y: 20.0,
            w: f64::from(width) - 40.0,
            h: 40.0,
        };
        crystal(cr, search, 15.0, false);
        let _ = cr.save();
        cr.rectangle(search.x + 10.0, search.y, search.w - 20.0, search.h);
        cr.clip();
        let query = &m.launcher.query;
        let cursor = measure(cr, &m.font, &query[..m.launcher.cursor]);
        let left = search.x + 12.0 - (cursor - (search.w - 36.0)).max(0.0);
        if m.launcher.selected_all {
            cr.rectangle(left, 28.0, measure(cr, &m.font, query), 24.0);
            color(cr, MINT, 0.25);
            let _ = cr.fill();
        }
        p.label(
            if query.is_empty() && m.preedit.is_empty() {
                "앱 검색"
            } else {
                query
            },
            Rect {
                x: left,
                y: 20.0,
                w: measure(cr, &m.font, query).max(search.w),
                h: 40.0,
            },
            false,
            false,
        );
        if !m.preedit.is_empty() {
            p.label(
                &m.preedit,
                Rect {
                    x: left + cursor,
                    y: 20.0,
                    w: 200.0,
                    h: 40.0,
                },
                true,
                false,
            );
        }
        color(cr, MINT, 1.0);
        cr.set_line_width(1.0);
        cr.move_to(left + cursor, 30.0);
        cr.line_to(left + cursor, 50.0);
        let _ = cr.stroke();
        let _ = cr.restore();
        p.hit(search, Action::Search, "앱 검색");
        let apps = m.launcher.results();
        content = apps.len().div_ceil(5) as f64 * super::launcher::ROW_HEIGHT;
        viewport = f64::from(height) - 100.0;
        p.clip = Some(Rect {
            x: 20.0,
            y: 80.0,
            w,
            h: viewport,
        });
        let _ = cr.save();
        cr.rectangle(20.0, 80.0, w, viewport);
        cr.clip();
        for (index, app) in apps.iter().enumerate() {
            let x = 20.0 + (index % 5) as f64 * super::launcher::CELL_WIDTH;
            let y = 80.0 + (index / 5) as f64 * super::launcher::ROW_HEIGHT - m.scroll;
            if y + 100.0 < 80.0 || y > 80.0 + viewport {
                continue;
            }
            let r = Rect {
                x: x + 4.0,
                y: y + 4.0,
                w: 92.0,
                h: 96.0,
            };
            let (icon_rect, name_rect) = launcher_content(r);
            if m.launcher.selected == index || p.hot() {
                crystal(cr, r, 16.0, m.launcher.selected == index);
            }
            if let Some(icon) = &app.icon {
                if let Ok(image) = cairo::ImageSurface::create_for_data(
                    icon.bytes.clone(),
                    cairo::Format::ARgb32,
                    icon.width,
                    icon.height,
                    icon.width * 4,
                ) {
                    let _ = cr.save();
                    cr.translate(icon_rect.x, icon_rect.y);
                    cr.scale(
                        icon_rect.w / f64::from(icon.width),
                        icon_rect.h / f64::from(icon.height),
                    );
                    let _ = cr.set_source_surface(image, 0.0, 0.0);
                    let _ = cr.paint();
                    let _ = cr.restore();
                }
            } else {
                super::icons::status(cr, icon_rect, "apps", "");
            }
            app_name(cr, &m.font, &app.name, name_rect);
            p.hit(r, Action::App(app.id.clone()), &app.name);
        }
        if apps.is_empty() {
            p.label(
                if m.launcher.ready {
                    "검색 결과 없음"
                } else {
                    "앱 불러오는 중…"
                },
                Rect {
                    x: 30.0,
                    y: 100.0,
                    w: 400.0,
                    h: 40.0,
                },
                false,
                false,
            );
        }
        let _ = cr.restore();
        p.clip = None;
        if !m.status.is_empty() {
            p.label(
                &m.status,
                Rect {
                    x: 20.0,
                    y: f64::from(height) - 25.0,
                    w: 440.0,
                    h: 25.0,
                },
                false,
                false,
            );
        }
    } else if m.page == Page::Home {
        let power = Rect {
            x: WIDTH as f64 - 52.0,
            y: 16.0,
            w: 32.0,
            h: 32.0,
        };
        p.header_button(power, Action::Page(Page::Session));
        let tw = (w - 16.0) / 3.0;
        let arrow_w = 34.0;
        let tile_h = tw / 1.618_033_988_75;
        let offset = tile_h - 66.0;
        let wifi_name = m
            .network
            .as_ref()
            .and_then(|s| {
                s.aps
                    .iter()
                    .find(|ap| ap.active)
                    .map(|ap| ap.ssid.clone())
                    .or_else(|| {
                        s.profiles
                            .iter()
                            .find(|p| p.active && p.kind == "802-11-wireless")
                            .map(|p| p.name.clone())
                    })
            })
            .unwrap_or_else(|| {
                if m.wifi_searching() {
                    "검색 중".into()
                } else {
                    "연결 없음".into()
                }
            });
        let bluetooth_name = if m.common.bluetooth_devices.is_empty() {
            "연결 없음".into()
        } else {
            m.common.bluetooth_devices.join(" · ")
        };
        for (i, (symbol, title, value, on, a, page)) in [
            (
                "network",
                "Wi-Fi",
                wifi_name.as_str(),
                m.wifi_enabled(),
                Action::Wifi,
                Page::Network,
            ),
            (
                "bluetooth",
                "블루투스",
                bluetooth_name.as_str(),
                m.common.bluetooth.unwrap_or(false),
                Action::Bluetooth,
                Page::Bluetooth,
            ),
            (
                "profile",
                "전원 모드",
                model::profile_name(m.common.profile.as_deref().unwrap_or_default()),
                true,
                Action::Profile,
                Page::Power,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let x = 20.0 + i as f64 * (tw + 8.0);
            let r = Rect {
                x,
                y: 58.0,
                w: tw,
                h: tile_h,
            };
            if on {
                round(cr, r, 20.0);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.91);
                let _ = cr.fill();
            }
            crystal(cr, r, 20.0, false);
            let ink = if on { (0.12, 0.14, 0.18) } else { WHITE };
            let body_w = tw - arrow_w;
            let _ = cr.save();
            cr.translate(x + body_w / 2.0 - 11.0, 66.0);
            cr.scale(22.0 / 18.0, 22.0 / 18.0);
            super::icons::status_colored(
                cr,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 18.0,
                    h: 18.0,
                },
                symbol,
                if symbol == "network" {
                    ""
                } else if symbol == "profile" {
                    m.common.profile.as_deref().unwrap_or("balanced")
                } else {
                    ""
                },
                match symbol {
                    "network" => (0.18, 0.48, 0.91),
                    "bluetooth" => (0.55, 0.35, 0.89),
                    _ => (0.88, 0.53, 0.15),
                },
            );
            let _ = cr.restore();
            text_ink(
                cr,
                &m.font,
                title,
                Rect {
                    x: x + 2.0,
                    y: 58.0 + tile_h - 56.0,
                    w: body_w - 4.0,
                    h: 18.0,
                },
                true,
                12.5,
                pango::Alignment::Center,
                ink,
            );
            app_name_ink(
                cr,
                &m.font,
                value,
                Rect {
                    x: x + 2.0,
                    y: 58.0 + tile_h - 36.0,
                    w: body_w - 4.0,
                    h: 28.0,
                },
                if on {
                    (0.43, 0.45, 0.49)
                } else {
                    (0.70, 0.72, 0.76)
                },
            );
            p.hit(Rect { w: body_w, ..r }, a, "클릭하여 전환");
            let arrow = Rect {
                x: x + body_w,
                y: 58.0,
                w: arrow_w,
                h: tile_h,
            };
            cr.new_path();
            cr.set_source_rgba(ink.0, ink.1, ink.2, if on { 0.18 } else { 0.23 });
            cr.set_line_width(1.0);
            cr.move_to(arrow.x + 0.5, arrow.y + 15.0);
            cr.line_to(arrow.x + 0.5, arrow.y + arrow.h - 15.0);
            let _ = cr.stroke();
            // Native vector chevron: no glyph baseline or narrow stretched pill.
            cr.new_path();
            color(cr, ink, 0.85);
            cr.set_line_width(2.0);
            cr.set_line_cap(cairo::LineCap::Round);
            let (cx, cy) = (arrow.x + arrow.w / 2.0, arrow.y + arrow.h / 2.0);
            cr.move_to(cx - 2.5, cy - 5.0);
            cr.line_to(cx + 2.5, cy);
            cr.line_to(cx - 2.5, cy + 5.0);
            let _ = cr.stroke();
            p.hit(arrow, Action::Page(page), "세부 설정");
        }
        crystal(
            cr,
            Rect {
                x: 20.0,
                y: 140.0 + offset,
                w,
                h: 148.0,
            },
            18.0,
            false,
        );
        let art = Rect {
            x: 34.0,
            y: 184.0 + offset,
            w: 72.0,
            h: 72.0,
        };
        if let Some(media) = &m.common.media {
            let icon_rect = Rect {
                x: 34.0,
                y: 150.0 + offset,
                w: 24.0,
                h: 24.0,
            };
            if let Some(icon) = &m.media_app_icon {
                if let Ok(image) = cairo::ImageSurface::create_for_data(
                    icon.bytes.clone(),
                    cairo::Format::ARgb32,
                    icon.width,
                    icon.height,
                    icon.width * 4,
                ) {
                    let _ = cr.save();
                    cr.translate(icon_rect.x, icon_rect.y);
                    cr.scale(
                        24.0 / f64::from(image.width()),
                        24.0 / f64::from(image.height()),
                    );
                    let _ = cr.set_source_surface(image, 0.0, 0.0);
                    let _ = cr.paint();
                    let _ = cr.restore();
                }
            } else {
                text(
                    cr,
                    &m.font,
                    "♫",
                    icon_rect,
                    true,
                    false,
                    16.0,
                    pango::Alignment::Center,
                );
            }
            text(
                cr,
                &m.font,
                if m.media_app_name.is_empty() {
                    &media.player
                } else {
                    &m.media_app_name
                },
                Rect {
                    x: 66.0,
                    y: 149.0 + offset,
                    w: w - 58.0,
                    h: 26.0,
                },
                false,
                true,
                15.0,
                pango::Alignment::Left,
            );
        }
        round(cr, art, 9.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
        let _ = cr.fill();
        if let Some(image) = &m.cover {
            let _ = cr.save();
            round(cr, art, 9.0);
            cr.clip();
            cr.translate(art.x, art.y);
            cr.scale(
                72.0 / f64::from(image.width()),
                72.0 / f64::from(image.height()),
            );
            let _ = cr.set_source_surface(image, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
        } else {
            text(
                cr,
                &m.font,
                "♫",
                art,
                true,
                true,
                48.0,
                pango::Alignment::Center,
            );
        }
        p.label(
            m.common
                .media
                .as_ref()
                .map(|v| v.title.as_str())
                .unwrap_or("재생 중인 미디어 없음"),
            Rect {
                x: 120.0,
                y: 184.0 + offset,
                w: w - 118.0,
                h: 28.0,
            },
            false,
            false,
        );
        if let Some(media) = &m.common.media {
            p.label(
                &media.artist,
                Rect {
                    x: 120.0,
                    y: 213.0 + offset,
                    w: w - 118.0,
                    h: 22.0,
                },
                false,
                false,
            );
        }
        for (i, (icon, cmd)) in [
            ("⏮", "previous"),
            (
                if m.common.media.as_ref().is_some_and(|m| m.playing) {
                    "⏸"
                } else {
                    "▶"
                },
                "play-pause",
            ),
            ("⏭", "next"),
        ]
        .into_iter()
        .enumerate()
        {
            p.button(
                Rect {
                    x: WIDTH as f64 / 2.0 - 77.0 + i as f64 * 54.0,
                    y: 246.0 + offset,
                    w: 46.0,
                    h: 30.0,
                },
                icon,
                Action::Media(cmd),
                i == 1,
                "미디어 제어",
            );
        }
        p.slider(
            304.0 + offset,
            w - 42.0,
            m.common.audio.map_or(0.0, |a| a.0),
            Slider::Volume,
            false,
            m.common.audio.is_some_and(|a| a.1),
        );
        p.button(
            Rect {
                x: WIDTH as f64 - 52.0,
                y: 307.0 + offset,
                w: 32.0,
                h: 32.0,
            },
            "⚙",
            Action::Page(Page::Audio),
            false,
            "소리 세부 설정",
        );
        p.slider(
            360.0 + offset,
            w - 42.0,
            m.common.brightness.unwrap_or(0.0),
            Slider::Brightness,
            false,
            false,
        );
        p.button(
            Rect {
                x: WIDTH as f64 - 52.0,
                y: 363.0 + offset,
                w: 32.0,
                h: 32.0,
            },
            "⚙",
            Action::Page(Page::Display),
            false,
            "디스플레이 세부 설정",
        );
        if m.show_network_status() {
            p.label(
                &m.status,
                Rect {
                    x: 20.0,
                    y: 410.0 + offset,
                    w,
                    h: 28.0,
                },
                false,
                false,
            );
        }
    } else {
        if !matches!(m.page, Page::Calendar | Page::TrayMenu) {
            p.button(
                Rect {
                    x: 20.0,
                    y: 18.0,
                    w: 36.0,
                    h: 30.0,
                },
                "‹",
                Action::Page(Page::Home),
                false,
                "뒤로가기",
            );
        }
        p.label(
            match m.page {
                Page::Audio => "소리",
                Page::Network => "네트워크",
                Page::Bluetooth => "블루투스",
                Page::Power => "전원",
                Page::Display => "디스플레이",
                Page::Session => "전원",
                Page::TrayMenu => "",
                Page::Calendar => "",
                _ => "",
            },
            Rect {
                x: if m.page == Page::Calendar { 20.0 } else { 70.0 },
                y: 18.0,
                w: if m.page == Page::Calendar {
                    280.0
                } else {
                    340.0
                },
                h: 30.0,
            },
            false,
            true,
        );
        match m.page {
            Page::Audio => {
                p.clip = Some(Rect {
                    x: 16.0,
                    y: 62.0,
                    w: w + 8.0,
                    h: f64::from(height) - 78.0,
                });
                let _ = cr.save();
                cr.rectangle(16.0, 62.0, w + 8.0, f64::from(height) - 78.0);
                cr.clip();
                let mut y = 64.0 - m.scroll;
                for (kind, collection, title) in audio::KINDS {
                    p.label(
                        title,
                        Rect {
                            x: 20.0,
                            y,
                            w,
                            h: 24.0,
                        },
                        true,
                        true,
                    );
                    y += 30.0;
                    let items: Vec<_> = m.audio[collection]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|i| {
                            kind != "source" || !audio::text(i, "name").ends_with(".monitor")
                        })
                        .collect();
                    if kind == "sink" || kind == "source" {
                        let default = m.audio["info"][format!("default_{kind}_name")]
                            .as_str()
                            .unwrap_or_default();
                        let selected = items.iter().find(|i| audio::text(i, "name") == default);
                        p.button(
                            Rect {
                                x: 20.0,
                                y,
                                w,
                                h: 34.0,
                            },
                            &format!(
                                "{}   ▾",
                                selected
                                    .map(|i| audio::description(i))
                                    .unwrap_or("장치 없음".into())
                            ),
                            Action::DeviceMenu(kind.into()),
                            false,
                            "장치 선택",
                        );
                        y += 42.0;
                        if m.device_menu.as_deref() == Some(kind) {
                            for item in &items {
                                p.button(
                                    Rect {
                                        x: 28.0,
                                        y,
                                        w: w - 16.0,
                                        h: 32.0,
                                    },
                                    &audio::description(item),
                                    Action::Device(kind.into(), audio::text(item, "name").into()),
                                    false,
                                    "기본 장치 선택",
                                );
                                y += 36.0;
                            }
                        }
                        if let Some(item) = selected {
                            p.slider(
                                y,
                                w,
                                audio::volume(item),
                                Slider::Audio(kind.into(), item["index"].as_u64().unwrap_or(0)),
                                kind == "source",
                                item["mute"].as_bool().unwrap_or(false),
                            );
                            y += 54.0;
                        }
                    } else if items.is_empty() {
                        p.label(
                            "활성 앱 없음",
                            Rect {
                                x: 20.0,
                                y,
                                w,
                                h: 24.0,
                            },
                            false,
                            false,
                        );
                        y += 36.0;
                    } else {
                        for item in items {
                            p.label(
                                &audio::description(item),
                                Rect {
                                    x: 20.0,
                                    y,
                                    w,
                                    h: 24.0,
                                },
                                false,
                                false,
                            );
                            y += 26.0;
                            p.slider(
                                y,
                                w,
                                audio::volume(item),
                                Slider::Audio(kind.into(), item["index"].as_u64().unwrap_or(0)),
                                kind == "source-output",
                                item["mute"].as_bool().unwrap_or(false),
                            );
                            y += 50.0;
                        }
                    }
                }
                content = y + m.scroll - 64.0;
                viewport = f64::from(height) - 78.0;
                let _ = cr.restore();
                p.clip = None;
            }
            Page::Network => {
                let tabs = Rect {
                    x: 20.0,
                    y: 62.0,
                    w,
                    h: 38.0,
                };
                crystal(cr, tabs, 14.0, false);
                let index = match m.tab.as_str() {
                    "ethernet" => 1.0,
                    "vpn" => 2.0,
                    _ => 0.0,
                };
                let position = m.icon_value("network-tab", index);
                let segment = (w - 8.0) / 3.0;
                crystal(
                    cr,
                    Rect {
                        x: 24.0 + segment * position,
                        y: 66.0,
                        w: segment,
                        h: 30.0,
                    },
                    11.0,
                    true,
                );
                for (i, (tab, title)) in [("wifi", "Wi-Fi"), ("ethernet", "이더넷"), ("vpn", "VPN")]
                    .into_iter()
                    .enumerate()
                {
                    let r = Rect {
                        x: 24.0 + i as f64 * segment,
                        y: 66.0,
                        w: segment,
                        h: 30.0,
                    };
                    text(
                        cr,
                        &m.font,
                        title,
                        r,
                        m.tab == tab,
                        true,
                        0.0,
                        pango::Alignment::Center,
                    );
                    p.hit(r, Action::Tab(tab.into()), "탭 전환");
                }
                let bottom = if !m.show_network_status() {
                    f64::from(height) - 20.0
                } else {
                    f64::from(height) - 60.0
                };
                let mut list_top = 110.0;
                if m.tab == "wifi" {
                    p.wifi_controls(list_top);
                    list_top += 56.0;
                    if let Some(s) = &m.network {
                        for ap in s.aps.iter().filter(|ap| ap.active) {
                            crystal(
                                cr,
                                Rect {
                                    x: 20.0,
                                    y: list_top,
                                    w,
                                    h: 60.0,
                                },
                                18.0,
                                true,
                            );
                            p.access_point(ap, list_top, w);
                            list_top += 76.0;
                        }
                    }
                    p.label(
                        "사용 가능한 Wi-Fi",
                        Rect {
                            x: 32.0,
                            y: list_top,
                            w,
                            h: 24.0,
                        },
                        false,
                        true,
                    );
                    list_top += 28.0;
                }
                viewport = (bottom - list_top).max(0.0);
                p.clip = Some(Rect {
                    x: 16.0,
                    y: list_top - 4.0,
                    w: w + 8.0,
                    h: viewport + 4.0,
                });
                let _ = cr.save();
                cr.rectangle(16.0, list_top - 4.0, w + 8.0, viewport + 4.0);
                cr.clip();
                let mut y = list_top - m.scroll;
                if let Some(s) = &m.network {
                    if m.tab == "wifi" {
                        for ap in s.aps.iter().filter(|ap| !ap.active) {
                            p.access_point(ap, y, w);
                            y += WIFI_ROW_PITCH;
                            if m.wifi.as_ref().is_some_and(|a| a.bssid == ap.bssid)
                                && (!m.busy || m.auth_prompt)
                            {
                                p.secret(y);
                                y += 50.0;
                            }
                        }
                        if (m.auth_prompt || (m.wifi.is_some() && !m.busy))
                            && m.wifi.as_ref().is_none_or(|selected| {
                                !s.aps.iter().any(|ap| ap.bssid == selected.bssid)
                            })
                        {
                            p.secret(y);
                            y += 50.0;
                        }
                        p.button(
                            Rect {
                                x: 20.0,
                                y,
                                w,
                                h: 30.0,
                            },
                            if m.saved {
                                "▾ 저장된 Wi-Fi"
                            } else {
                                "▸ 저장된 Wi-Fi"
                            },
                            Action::Saved,
                            false,
                            "저장된 연결",
                        );
                        y += 38.0;
                        if m.saved {
                            for profile in s.profiles.iter().filter(|p| p.kind == "802-11-wireless")
                            {
                                crystal(
                                    cr,
                                    Rect {
                                        x: 20.0,
                                        y,
                                        w,
                                        h: 34.0,
                                    },
                                    11.0,
                                    profile.active,
                                );
                                p.label(
                                    &profile.name,
                                    Rect {
                                        x: 32.0,
                                        y,
                                        w: w - 162.0,
                                        h: 32.0,
                                    },
                                    profile.active,
                                    false,
                                );
                                p.button(
                                    Rect {
                                        x: WIDTH as f64 - 168.0,
                                        y,
                                        w: 62.0,
                                        h: 30.0,
                                    },
                                    if profile.active { "해제" } else { "연결" },
                                    Action::Net(if profile.active {
                                        crate::network_backend::Action::Down(profile.uuid.clone())
                                    } else {
                                        crate::network_backend::Action::Up(profile.uuid.clone())
                                    }),
                                    false,
                                    "저장된 연결",
                                );
                                p.button(
                                    Rect {
                                        x: WIDTH as f64 - 96.0,
                                        y,
                                        w: 64.0,
                                        h: 30.0,
                                    },
                                    "삭제",
                                    Action::Forget(profile.uuid.clone()),
                                    false,
                                    "저장 Wi-Fi 삭제",
                                );
                                y += 42.0;
                                if m.deleting.as_ref() == Some(&profile.uuid) {
                                    p.button(
                                        Rect {
                                            x: WIDTH as f64 - 184.0,
                                            y,
                                            w: 76.0,
                                            h: 30.0,
                                        },
                                        "취소",
                                        Action::CancelDelete,
                                        false,
                                        "취소",
                                    );
                                    p.button(
                                        Rect {
                                            x: WIDTH as f64 - 100.0,
                                            y,
                                            w: 68.0,
                                            h: 30.0,
                                        },
                                        "삭제 확인",
                                        Action::ConfirmDelete(profile.uuid.clone()),
                                        true,
                                        "삭제 확인",
                                    );
                                    y += 38.0;
                                }
                            }
                        }
                        if !s.wifi {
                            p.label(
                                "Wi-Fi가 꺼져 있습니다.",
                                Rect {
                                    x: 20.0,
                                    y,
                                    w,
                                    h: 28.0,
                                },
                                false,
                                false,
                            );
                            y += 34.0;
                        }
                    } else {
                        if m.tab == "ethernet" {
                            for d in s.devices.iter().filter(|d| d.kind == "ethernet") {
                                p.label(
                                    &format!("{} · {}", d.name, d.connection),
                                    Rect {
                                        x: 20.0,
                                        y,
                                        w: w - 90.0,
                                        h: 24.0,
                                    },
                                    false,
                                    false,
                                );
                                if let Some(ip) = s.ips.get(&d.name) {
                                    p.label(
                                        ip,
                                        Rect {
                                            x: 20.0,
                                            y: y + 24.0,
                                            w: w - 90.0,
                                            h: 22.0,
                                        },
                                        false,
                                        false,
                                    );
                                }
                                p.button(
                                    Rect {
                                        x: WIDTH as f64 - 96.0,
                                        y: y + 8.0,
                                        w: 68.0,
                                        h: 30.0,
                                    },
                                    if d.state == "connected" {
                                        "해제"
                                    } else {
                                        "연결"
                                    },
                                    Action::Net(if d.state == "connected" {
                                        crate::network_backend::Action::Disconnect(d.name.clone())
                                    } else {
                                        crate::network_backend::Action::UpDevice(d.name.clone())
                                    }),
                                    false,
                                    "유선 연결",
                                );
                                y += 58.0;
                            }
                        }
                        for profile in s.profiles.iter().filter(|p| {
                            if m.tab == "vpn" {
                                ["vpn", "wireguard"].contains(&p.kind.as_str())
                            } else {
                                p.kind == "802-3-ethernet"
                            }
                        }) {
                            p.label(
                                &profile.name,
                                Rect {
                                    x: 20.0,
                                    y,
                                    w: w - 90.0,
                                    h: 44.0,
                                },
                                profile.active,
                                false,
                            );
                            p.button(
                                Rect {
                                    x: WIDTH as f64 - 96.0,
                                    y: y + 7.0,
                                    w: 68.0,
                                    h: 30.0,
                                },
                                if profile.active { "해제" } else { "연결" },
                                Action::Net(if profile.active {
                                    crate::network_backend::Action::Down(profile.uuid.clone())
                                } else {
                                    crate::network_backend::Action::Up(profile.uuid.clone())
                                }),
                                false,
                                "프로필 연결",
                            );
                            y += 52.0;
                        }
                        if m.auth_prompt {
                            p.secret(y);
                            y += 50.0;
                        }
                        if y <= list_top - m.scroll {
                            p.label(
                                "사용 가능한 항목 없음",
                                Rect {
                                    x: 20.0,
                                    y,
                                    w,
                                    h: 30.0,
                                },
                                false,
                                false,
                            );
                            y += 38.0;
                        }
                    }
                } else {
                    p.label(
                        "네트워크 불러오는 중…",
                        Rect {
                            x: 20.0,
                            y,
                            w,
                            h: 30.0,
                        },
                        false,
                        false,
                    );
                    y += 38.0;
                }
                content = y + m.scroll - list_top;
                let _ = cr.restore();
                p.clip = None;
                if m.show_network_status() {
                    if m.busy && !m.auth_prompt {
                        spinner(cr, 27.0, f64::from(height) - 33.0);
                    }
                    p.label(
                        &m.status,
                        Rect {
                            x: if m.busy && !m.auth_prompt { 42.0 } else { 20.0 },
                            y: f64::from(height) - 48.0,
                            w: w - 64.0,
                            h: 30.0,
                        },
                        false,
                        false,
                    );
                    if m.busy {
                        p.button(
                            Rect {
                                x: WIDTH as f64 - 72.0,
                                y: f64::from(height) - 48.0,
                                w: 52.0,
                                h: 30.0,
                            },
                            "취소",
                            Action::Cancel,
                            false,
                            "현재 작업 취소",
                        );
                    }
                }
            }
            Page::TrayMenu => {
                content = m.menu.len() as f64 * 34.0;
                viewport = f64::from(height) - 36.0;
                p.clip = Some(Rect {
                    x: 16.0,
                    y: 18.0,
                    w: w + 8.0,
                    h: viewport,
                });
                let _ = cr.save();
                cr.rectangle(16.0, 18.0, w + 8.0, viewport);
                cr.clip();
                for (i, entry) in m.menu.iter().enumerate() {
                    if entry.enabled {
                        p.button(
                            Rect {
                                x: 20.0,
                                y: 20.0 + i as f64 * 34.0 - m.scroll,
                                w,
                                h: 30.0,
                            },
                            &entry.label,
                            Action::MenuEvent(entry.id),
                            false,
                            "메뉴 실행",
                        );
                    } else {
                        p.label(
                            &entry.label,
                            Rect {
                                x: 20.0,
                                y: 20.0 + i as f64 * 34.0 - m.scroll,
                                w,
                                h: 30.0,
                            },
                            false,
                            false,
                        );
                    }
                }
                let _ = cr.restore();
                p.clip = None;
            }
            Page::Calendar => {
                let now = glib::DateTime::now_local().unwrap();
                let total = now.year() * 12 + now.month() - 1 + m.calendar_offset;
                let year = total.div_euclid(12);
                let month = total.rem_euclid(12) + 1;
                p.button(
                    Rect {
                        x: 20.0,
                        y: 64.0,
                        w: 36.0,
                        h: 30.0,
                    },
                    "‹",
                    Action::CalendarOffset(-1),
                    false,
                    "이전 달",
                );
                text(
                    cr,
                    &m.font,
                    &format!("{year}년 {month}월"),
                    Rect {
                        x: 62.0,
                        y: 64.0,
                        w: 196.0,
                        h: 30.0,
                    },
                    true,
                    true,
                    0.0,
                    pango::Alignment::Center,
                );
                p.button(
                    Rect {
                        x: 264.0,
                        y: 64.0,
                        w: 36.0,
                        h: 30.0,
                    },
                    "›",
                    Action::CalendarOffset(1),
                    false,
                    "다음 달",
                );
                let first = sunday_offset(year, month);
                for (i, day) in ["일", "월", "화", "수", "목", "금", "토"]
                    .iter()
                    .enumerate()
                {
                    text(
                        cr,
                        &m.font,
                        day,
                        Rect {
                            x: 20.0 + i as f64 * super::notifications::CELL,
                            y: 100.0,
                            w: super::notifications::CELL,
                            h: 28.0,
                        },
                        true,
                        false,
                        0.0,
                        pango::Alignment::Center,
                    );
                }
                let days = match month {
                    2 => {
                        if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                            29
                        } else {
                            28
                        }
                    }
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                for day in 1..=days {
                    let n = first + day - 1;
                    let r = Rect {
                        x: 20.0 + f64::from(n % 7) * super::notifications::CELL,
                        y: 128.0 + f64::from(n / 7) * super::notifications::CELL,
                        w: super::notifications::CELL,
                        h: super::notifications::CELL,
                    };
                    if m.calendar_offset == 0 && day == now.day_of_month() {
                        round(cr, r, 8.0);
                        color(cr, MINT, 0.22);
                        let _ = cr.fill();
                    }
                    text(
                        cr,
                        &m.font,
                        &day.to_string(),
                        r,
                        false,
                        false,
                        0.0,
                        pango::Alignment::Center,
                    );
                }
                (content, viewport) = notification_list(&mut p, height);
            }
            _ => p.label(
                "세부 설정은 아직 구현하지 않았습니다.",
                Rect {
                    x: 20.0,
                    y: 76.0,
                    w,
                    h: 38.0,
                },
                false,
                false,
            ),
        }
    }
    if content > viewport && viewport > 0.0 {
        let top = if m.page == Page::Network {
            f64::from(height) - if m.show_network_status() { 60.0 } else { 20.0 } - viewport
        } else if m.page == Page::Launcher {
            80.0
        } else if m.page == Page::TrayMenu {
            18.0
        } else {
            62.0
        };
        let track = Rect {
            x: f64::from(width) - 17.0,
            y: top,
            w: 15.0,
            h: viewport,
        };
        let bar = model::Scrollbar::new(track, content, viewport, m.scroll);
        let hot = m.scroll_drag.is_some()
            || pointer(m).is_some_and(|(id, x, y)| id == p.id && track.contains(x, y));
        let thumb_width = if hot { 5.0 } else { 3.0 };
        round(
            cr,
            Rect {
                x: track.x + (track.w - thumb_width) / 2.0,
                y: bar.thumb.y,
                w: thumb_width,
                h: bar.thumb.h,
            },
            1.5,
        );
        color(cr, MINT, if hot { 0.85 } else { 0.55 });
        let _ = cr.fill();
        p.hit(track, Action::Scrollbar(bar), "");
    }
    Scene {
        hits: p.hits,
        content_height: content,
        viewport_height: viewport,
    }
}

fn session(cr: &Context, m: &Model, id: u32) -> Scene {
    let mut p = Paint {
        cr,
        m,
        id,
        hits: vec![],
        clip: None,
    };
    for (index, (name, command, icon)) in [
        ("잠금", "lock", "lock"),
        ("절전", "suspend", "sleep"),
        ("로그아웃", "logout", "logout"),
        ("다시 시작", "reboot", "restart"),
        ("전원 끄기", "poweroff", "power"),
    ]
    .into_iter()
    .enumerate()
    {
        let r = Rect {
            x: 48.0 + index as f64 * 136.0,
            y: 28.0,
            w: 120.0,
            h: 120.0,
        };
        round(cr, r, 28.0);
        cr.set_source_rgba(25.0 / 255.0, 27.0 / 255.0, 34.0 / 255.0, 0.48);
        let _ = cr.fill();
        crystal(cr, r, 28.0, p.hot());
        let _ = cr.save();
        cr.translate(r.x + (r.w - 36.0) / 2.0, r.y + 24.0);
        cr.scale(2.0, 2.0);
        super::icons::status_colored(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 18.0,
                h: 18.0,
            },
            icon,
            "",
            if command == "poweroff" {
                (1.0, 0.70, 0.72)
            } else {
                WHITE
            },
        );
        let _ = cr.restore();
        text(
            cr,
            &m.font,
            name,
            Rect {
                x: r.x + 10.0,
                y: r.y + 76.0,
                w: r.w - 20.0,
                h: 24.0,
            },
            false,
            false,
            14.0,
            pango::Alignment::Center,
        );
        p.hit(r, Action::Session(command), name);
    }
    Scene {
        hits: p.hits,
        content_height: 0.0,
        viewport_height: 0.0,
    }
}

pub fn expanded(cr: &Context, m: &Model, id: u32, width: i32) -> Scene {
    let _joined = JoinedScope::enter();
    let x = f64::from(width - popup_width(m)) / 2.0;
    // Calendar's existing empty title row holds the enlarged clock. Other pages
    // need only 24 px extra: the bar and the old 18 px title inset share space.
    let inset = if m.page == Page::Calendar {
        0
    } else {
        EXPANDED_INSET
    };
    let y = f64::from(inset);
    let _ = cr.save();
    cr.translate(x, y);
    let offset = OffsetScope::enter(x, y);
    let mut scene = popup(cr, m, id, popup_height(m) - inset);
    drop(offset);
    let _ = cr.restore();
    for h in &mut scene.hits {
        h.rect.x += x;
        h.rect.y += y;
        if let Action::Scrollbar(b) = &mut h.action {
            b.track.x += x;
            b.track.y += y;
            b.thumb.x += x;
            b.thumb.y += y;
        }
    }
    scene
}
pub fn bar_header(
    cr: &Context,
    m: &Model,
    id: u32,
    side: &str,
    output: &str,
    width: i32,
    base: usize,
) -> Scene {
    let _joined = JoinedScope::enter();
    let old = HIT_BASE.with(|v| v.replace(base));
    let scene = bar(cr, m, id, side, output, width);
    HIT_BASE.with(|v| v.set(old));
    scene
}
pub fn glass_expanded_fallback(
    cr: &Context,
    m: &Model,
    id: u32,
    side: &str,
    output: &str,
    width: i32,
) -> Scene {
    glass_base(
        cr,
        Rect {
            x: 0.0,
            y: 0.0,
            w: f64::from(width),
            h: f64::from(popup_height(m)),
        },
        0.54,
    );
    let mut scene = expanded(cr, m, id, width);
    let hw = bar_width(cr, m, side, output);
    let frame = super::expansion::layout(
        1.0,
        hw as f32,
        (width, popup_height(m)),
        hw as f32,
        side == "center",
    );
    let (x, y, scale) = (
        f64::from(frame.header.0),
        f64::from(frame.header.1),
        f64::from(frame.header_scale),
    );
    let _ = cr.save();
    cr.translate(x, y);
    cr.scale(scale, scale);
    let _offset = OffsetScope::enter(x, y);
    let mut hs = bar_header(cr, m, id, side, output, hw, scene.hits.len());
    let _ = cr.restore();
    for h in &mut hs.hits {
        h.rect.x = h.rect.x * scale + x;
        h.rect.y = h.rect.y * scale + y;
        h.rect.w *= scale;
        h.rect.h *= scale;
    }
    scene.hits.extend(hs.hits);
    scene
}
fn controls_width(_cr: &Context, m: &Model) -> f64 {
    100.0
        + if m.metrics.values.contains_key("battery") {
            44.0
        } else {
            0.0
        }
}
fn bar_content_width(cr: &Context, m: &Model, side: &str, output: &str) -> i32 {
    match side {
        "left" => {
            48 + 32
                * m.workspaces
                    .iter()
                    .filter(|w| w["output"] == output)
                    .count() as i32
        }
        "center" => (measure(cr, &m.font, &clock(&glib::DateTime::now_local().unwrap())) + 24.0)
            .ceil() as i32,
        _ => {
            let n = ["temperature", "cpu", "memory", "disk"]
                .iter()
                .filter_map(|k| m.metrics.values.get(k).map(|metric| (*k, metric)))
                .map(|(key, metric)| super::icons::width(cr, &m.font, key, &metric.text) + 6.0)
                .sum::<f64>();
            (n + 48.0
                + controls_width(cr, m)
                + if m.tray_open {
                    m.tray_items.len() as f64 * 32.0
                } else {
                    0.0
                }) as i32
        }
    }
}
pub fn bar_width(cr: &Context, m: &Model, side: &str, output: &str) -> i32 {
    let width = bar_content_width(cr, m, side, output);
    if side == "right" {
        width.max(WIDTH)
    } else {
        width
    }
}
pub fn bar(cr: &Context, m: &Model, id: u32, side: &str, output: &str, width: i32) -> Scene {
    let _palette = GlassScope::enter();
    let _bar = BarScope::enter();
    if !JOINED.with(|v| v.get()) {
        round(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: 34.0,
            },
            14.0,
        );
        cr.set_source_rgba(25.0 / 255.0, 27.0 / 255.0, 34.0 / 255.0, 0.54);
        let _ = cr.fill();
        crystal(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: 34.0,
            },
            14.0,
            false,
        );
        bar_rim(
            cr,
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: 34.0,
            },
            side == "left",
        );
    }
    let mut p = Paint {
        cr,
        m,
        id,
        hits: vec![],
        clip: None,
    };
    if side == "left" {
        let launcher = Rect {
            x: 8.0,
            y: 3.0,
            w: 30.0,
            h: 28.0,
        };
        super::icons::logo(cr, launcher);
        p.hit(launcher, Action::Launch, "앱 런처");
        let mut x = 44.0;
        let mut workspaces: Vec<_> = m
            .workspaces
            .iter()
            .filter(|s| s["output"] == output)
            .collect();
        workspaces.sort_by_key(|s| s["idx"].as_u64().unwrap_or(0));
        for s in workspaces {
            let r = Rect {
                x,
                y: 3.0,
                w: 28.0,
                h: 28.0,
            };
            let active = s["is_active"].as_bool().unwrap_or(false);
            cr.new_path();
            cr.arc(
                r.x + r.w / 2.0,
                17.0,
                if active { 5.0 } else { 3.5 },
                0.0,
                2.0 * PI,
            );
            color(
                cr,
                WHITE,
                if active {
                    1.0
                } else if p.hot() {
                    0.65
                } else {
                    0.35
                },
            );
            let _ = cr.fill();
            p.hit(
                r,
                Action::Workspace(s["id"].as_u64().unwrap_or(0)),
                "작업 공간",
            );
            x += 32.0;
        }
    } else if side == "center" {
        let now = glib::DateTime::now_local().unwrap();
        let date = clock(&now);
        text(
            cr,
            &m.font,
            &date,
            Rect {
                x: 8.0,
                y: 0.0,
                w: f64::from(width) - 16.0,
                h: 34.0,
            },
            false,
            false,
            0.0,
            pango::Alignment::Center,
        );
        p.hit(
            Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: 34.0,
            },
            Action::Page(Page::Calendar),
            "달력",
        );
    } else {
        let mut x = 10.0 + f64::from(width - bar_content_width(cr, m, side, output)) / 2.0;
        for key in ["temperature", "cpu", "memory", "disk"] {
            if let Some(metric) = m.metrics.values.get(key) {
                if metric.critical {
                    cr.arc(x + 3.0, 29.0, 2.0, 0.0, 2.0 * PI);
                    color(cr, WHITE, 1.0);
                    let _ = cr.fill();
                }
                let w = super::icons::width(cr, &m.font, key, &metric.text);
                if key == "battery" {
                    super::icons::battery(
                        cr,
                        &m.font,
                        Rect {
                            x,
                            y: 0.0,
                            w,
                            h: 34.0,
                        },
                        super::icons::battery_level(&metric.text),
                        metric.text.starts_with("󰂄") || metric.tooltip == "Charging",
                    );
                } else {
                    super::icons::status(
                        cr,
                        Rect {
                            x,
                            y: 0.0,
                            w: 24.0,
                            h: 34.0,
                        },
                        key,
                        &metric.text,
                    );
                    p.label(
                        super::icons::value(&metric.text),
                        Rect {
                            x: x + 24.0,
                            y: 0.0,
                            w: w - 24.0,
                            h: 34.0,
                        },
                        false,
                        false,
                    );
                }
                x += w + 6.0;
            }
        }
        let group = Rect {
            x,
            y: 3.0,
            w: controls_width(cr, m),
            h: 28.0,
        };
        round(cr, group, 8.0);
        color(cr, MINT, if p.hot() { 0.18 } else { 0.0 });
        let _ = cr.fill();
        let audio = m
            .metrics
            .values
            .get("audio")
            .map_or("", |a| a.text.as_str());
        super::icons::status(
            cr,
            Rect {
                x: x + 5.0,
                y: 3.0,
                w: 24.0,
                h: 28.0,
            },
            "audio",
            audio,
        );
        for (offset, key) in [(37.0, "network"), (69.0, "bluetooth")] {
            super::icons::status(
                cr,
                Rect {
                    x: x + offset,
                    y: 3.0,
                    w: 24.0,
                    h: 28.0,
                },
                key,
                m.metrics.values.get(key).map_or("", |m| m.text.as_str()),
            );
        }
        if let Some(battery) = m.metrics.values.get("battery") {
            super::icons::battery(
                cr,
                &m.font,
                Rect {
                    x: x + 96.0,
                    y: 0.0,
                    w: 44.0,
                    h: 34.0,
                },
                super::icons::battery_level(&battery.text),
                battery.text.starts_with("󰂄") || battery.tooltip == "Charging",
            );
        }
        p.hit(group, Action::Page(Page::Home), "제어센터");
        x += group.w + 4.0;
        if m.tray_open {
            for item in &m.tray_items {
                let r = Rect {
                    x,
                    y: 5.0,
                    w: 24.0,
                    h: 24.0,
                };
                if let Some(icon) = &item.icon {
                    if let Ok(image) = cairo::ImageSurface::create_for_data(
                        icon.bytes.clone(),
                        cairo::Format::ARgb32,
                        icon.width,
                        icon.height,
                        icon.width * 4,
                    ) {
                        let _ = cr.save();
                        cr.translate(r.x + 3.0, r.y + 3.0);
                        cr.scale(
                            18.0 / f64::from(image.width()),
                            18.0 / f64::from(image.height()),
                        );
                        color(cr, WHITE, 1.0);
                        let _ = cr.mask_surface(image, 0.0, 0.0);
                        let _ = cr.restore();
                    }
                } else {
                    text(
                        cr,
                        &m.font,
                        "●",
                        r,
                        true,
                        false,
                        14.0,
                        pango::Alignment::Center,
                    );
                }
                p.hit(r, Action::TrayItem(item.id.clone()), &item.title);
                x += 32.0;
            }
        }
        {
            let r = Rect {
                x,
                y: 3.0,
                w: 26.0,
                h: 28.0,
            };
            if p.hot() {
                round(cr, r, 7.0);
                color(cr, MINT, 0.15);
                let _ = cr.fill();
            }
            super::icons::status(cr, r, "tray", if m.tray_open { "open" } else { "closed" });
            p.hit(r, Action::Tray, "트레이");
        }
    }
    Scene {
        hits: p.hits,
        content_height: 0.0,
        viewport_height: 0.0,
    }
}

fn clock(now: &glib::DateTime) -> String {
    format!(
        "{:02}월 {:02}일 {} {:02}:{:02}",
        now.month(),
        now.day_of_month(),
        [
            "월요일",
            "화요일",
            "수요일",
            "목요일",
            "금요일",
            "토요일",
            "일요일"
        ][(now.day_of_week() - 1) as usize],
        now.hour(),
        now.minute()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn wifi_toggle_is_green_and_connected_card_is_compact() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        m.network = Some(crate::network_backend::State {
            wifi: true,
            devices: vec![],
            profiles: vec![],
            aps: vec![crate::network_backend::Ap {
                active: true,
                bssid: "active".into(),
                ssid: "Connected".into(),
                signal: 80,
                security: "WPA2".into(),
                device: "wlan0".into(),
            }],
            ips: Default::default(),
        });
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            let scene = popup(&cr, &m, 1, HEIGHT);
            let active = scene
                .hits
                .iter()
                .find(|h| matches!(&h.action,Action::Ap(ap)if ap.active))
                .unwrap();
            assert_eq!(active.rect.y, 180.0);
        }
        image.flush();
        let stride = image.stride() as usize;
        let p = image.data().unwrap();
        let b = &p[132 * stride + 92 * 4..132 * stride + 92 * 4 + 4];
        assert!(b[1] > b[2] + 50 && b[1] > b[0] + 40);
    }
    #[test]
    fn workspace_indicators_are_large_white_and_small_dim_circles() {
        let mut m = Model::new();
        m.workspaces = (1..=2)
            .map(|i| serde_json::json!({"id":i,"idx":i,"output":"test","is_active":i==1}))
            .collect();
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 112, 34).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            bar_header(&cr, &m, 1, "left", "test", 112, 0);
        }
        image.flush();
        let stride = image.stride() as usize;
        let p = image.data().unwrap();
        assert!(p[17 * stride + 58 * 4 + 3] > 240);
        assert!(p[17 * stride + 90 * 4 + 3] < 120);
        let count = |x: usize| {
            (8..27)
                .flat_map(|y| (x..x + 28).map(move |xx| y * stride + xx * 4 + 3))
                .filter(|&i| p[i] > 0)
                .count()
        };
        assert!(count(44) > count(76));
        assert_eq!(p[7 * stride + 58 * 4 + 3], 0);
    }
    #[test]
    fn joined_calendar_reuses_title_space_and_preserves_scrollbar_coordinates() {
        let mut m = Model::new();
        m.navigate(Page::Calendar);
        m.notifications.notices = (0..10)
            .map(|id| crate::notifications::Notice {
                id,
                app: format!("App {id}"),
                body: "Body".into(),
                ..Default::default()
            })
            .collect();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        let normal = popup(&cr, &m, 1, 500);
        let joined = expanded(&cr, &m, 1, 780);
        let before = normal
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Scrollbar(_)))
            .unwrap();
        let after = joined
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Scrollbar(_)))
            .unwrap();
        assert_eq!(after.rect.y, before.rect.y);
        let Action::Scrollbar(track) = &after.action else {
            unreachable!()
        };
        assert_eq!(track.track.y, after.rect.y);
        let offset = OffsetScope::enter(0.0, 24.0);
        m.pointer = Some((1, 740.0, 124.0));
        assert_eq!(pointer(&m), Some((1, 740.0, 100.0)));
        drop(offset);
        assert_eq!(pointer(&m), m.pointer);
    }
    #[test]
    fn expanded_pages_fit_one_compact_panel_and_keep_header_separate() {
        let mut m = Model::new();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        assert_eq!(bar_width(&cr, &m, "right", "test"), WIDTH);
        for page in [
            Page::Home,
            Page::Audio,
            Page::Network,
            Page::Bluetooth,
            Page::Power,
            Page::Display,
            Page::Calendar,
        ] {
            m.navigate(page);
            let width = popup_width(&m);
            let scene = expanded(&cr, &m, 1, width);
            assert!(
                scene.hits.iter().all(|h| {
                    let top = if page == Page::Calendar
                        && h.rect.x >= super::super::notifications::LIST_X
                    {
                        18.0
                    } else {
                        40.0
                    };
                    h.rect.y >= top && h.rect.y + h.rect.h <= f64::from(HEIGHT)
                }),
                "page {page:?} has an out-of-bounds hit"
            );
            if page == Page::Home {
                let power = scene
                    .hits
                    .iter()
                    .find(|h| matches!(h.action, Action::Page(Page::Session)))
                    .unwrap();
                assert_eq!(power.rect.y, 40.0);
            }
        }
    }
    #[test]
    fn gauge_icon_contrasts_on_both_sides_of_a_low_white_fill() {
        let _palette = GlassScope::enter();
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 38.0,
        };
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 100, 38).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            gauge(&cr, r, 8.0);
            let icon = Rect {
                x: 0.0,
                y: 8.0,
                w: 20.0,
                h: 20.0,
            };
            gauge_icon(&cr, r, icon, 8.0, |cr| {
                cr.rectangle(icon.x, icon.y, icon.w, icon.h);
                cr.set_source_rgb(1.0, 1.0, 1.0);
                let _ = cr.fill();
            });
        }
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        assert!(data[18 * stride + 4 * 4 + 2] < 80);
        assert!(data[18 * stride + 14 * 4 + 2] > 240);
    }
    #[test]
    fn session_has_five_centered_square_glass_buttons_and_transparent_gaps() {
        let mut m = Model::new();
        m.navigate(Page::Session);
        assert_eq!(
            (popup_width(&m), popup_height(&m)),
            (SESSION_WIDTH, SESSION_HEIGHT)
        );
        let mut image =
            cairo::ImageSurface::create(cairo::Format::ARgb32, SESSION_WIDTH, SESSION_HEIGHT)
                .unwrap();
        let scene;
        {
            let cr = Context::new(&image).unwrap();
            scene = popup(&cr, &m, 1, SESSION_HEIGHT);
        }
        assert_eq!(scene.hits.len(), 5);
        for (hit, command) in scene
            .hits
            .iter()
            .zip(["lock", "suspend", "logout", "reboot", "poweroff"])
        {
            assert!(matches!(hit.action, Action::Session(c) if c == command));
            assert_eq!((hit.rect.w, hit.rect.h), (120.0, 120.0));
            assert_eq!(hit.rect.y, 28.0);
        }
        assert_eq!(scene.hits[0].rect.x, 48.0);
        assert_eq!(
            f64::from(SESSION_WIDTH) - scene.hits[4].rect.x - 120.0,
            48.0
        );
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        assert_eq!(data[88 * stride + 176 * 4 + 3], 0);
        assert_eq!(data[3], 0);
        assert!(data[100 * stride + 70 * 4 + 3] > 120);
    }
    #[test]
    fn readonly_session_preview_never_executes_system_actions() {
        let mut m = Model::new();
        m.navigate(Page::Session);
        m.readonly = true;
        for command in ["lock", "suspend", "logout", "reboot", "poweroff"] {
            m.activate(Action::Session(command));
            assert!(m.visible);
            assert_eq!(m.page, Page::Session);
        }
    }
    #[test]
    fn glass_gauge_fill_is_opaque_white_not_translucent_gray() {
        let _palette = GlassScope::enter();
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 100, 38).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            gauge(
                &cr,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 38.0,
                },
                50.0,
            );
        }
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        assert_eq!(&data[19 * stride + 20 * 4..19 * stride + 21 * 4], &[255; 4]);
        assert!(data[19 * stride + 80 * 4 + 3] < 50);
    }
    #[test]
    fn wifi_lock_name_and_action_have_independent_padding() {
        let (icon, name, detail, action) = wifi_row_layout(300.0, 440.0);
        assert_eq!(name.x - icon.x - icon.w, 12.0);
        assert_eq!(action.x - name.x - name.w, 12.0);
        assert_eq!((name.x, name.w), (detail.x, detail.w));
        assert_eq!(20.0 + 440.0 - action.x - action.w, 12.0);
        assert_eq!(WIFI_ROW_PITCH - 60.0, 8.0);
    }
    #[test]
    fn wifi_controls_and_list_actions_share_alignment() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        let ap = crate::network_backend::Ap {
            active: true,
            bssid: "active".into(),
            ssid: "Connected".into(),
            signal: 80,
            security: "WPA2".into(),
            device: "wlan0".into(),
        };
        m.network = Some(crate::network_backend::State {
            wifi: true,
            devices: vec![],
            profiles: vec![],
            aps: vec![
                ap.clone(),
                crate::network_backend::Ap {
                    active: false,
                    bssid: "other".into(),
                    ..ap
                },
            ],
            ips: Default::default(),
        });
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        let refresh = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Scan))
            .unwrap();
        let toggle = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Wifi))
            .unwrap();
        assert_eq!(toggle.rect.x, 84.0);
        assert_eq!(toggle.rect.x - 32.0 - 42.0, 10.0);
        assert_eq!(refresh.rect.w, refresh.rect.h);
        let aps = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::Ap(_)))
            .collect::<Vec<_>>();
        assert_eq!(aps.len(), 2);
        assert!(aps
            .iter()
            .all(|ap| (ap.rect.x, ap.rect.w) == (refresh.rect.x, refresh.rect.w)));
        let tabs = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::Tab(_)))
            .collect::<Vec<_>>();
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].rect.x - 20.0, 4.0);
        assert_eq!(tabs[2].rect.x + tabs[2].rect.w, 456.0);
        assert!(tabs
            .windows(2)
            .all(|p| p[0].rect.x + p[0].rect.w == p[1].rect.x));
    }
    #[test]
    fn notification_headers_and_cards_have_equal_width_and_glass_controls_are_square() {
        let mut m = Model::new();
        m.navigate(Page::Calendar);
        m.notifications.notices = (1..=2)
            .map(|id| crate::notifications::Notice {
                id,
                app: "Test".into(),
                summary: "Title".into(),
                body: "Body".into(),
                ..Default::default()
            })
            .collect();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        let header = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::NoticeGroup(_)))
            .unwrap();
        let card = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::NoticeHover))
            .unwrap();
        assert_eq!((header.rect.x, header.rect.w), (card.rect.x, card.rect.w));
        for h in scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::NotificationDnd | Action::ClearNotices))
        {
            assert_eq!(h.rect.w, 32.0);
            assert_eq!(h.rect.w, h.rect.h);
        }
    }
    #[test]
    fn launcher_focus_has_a_translucent_glass_rim() {
        let mut m = Model::new();
        m.navigate(Page::Launcher);
        m.launcher.apps = (0..2)
            .map(|id| super::super::launcher::App {
                id: id.to_string(),
                name: "App".into(),
                search: "app".into(),
                icon_name: String::new(),
                icon: None,
            })
            .collect();
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 540, 560).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            popup(&cr, &m, 1, 560);
        }
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        let selected = data[85 * stride + 70 * 4 + 3];
        let idle = data[85 * stride + 170 * 4 + 3];
        assert!(selected > idle + 20);
        assert!(selected < 255);
    }
    #[test]
    fn collapsed_stack_tail_has_no_end_of_animation_jump() {
        assert_eq!(stack_tail_height(0.0, 4), 14.0);
        assert_eq!(stack_tail_height(1.0, 4), 0.0);
        assert_eq!(stack_tail_height(0.0, 1), 0.0);
        for step in 0..1000 {
            let a = stack_tail_height(f64::from(step) / 1000.0, 4);
            let b = stack_tail_height(f64::from(step + 1) / 1000.0, 4);
            assert!((a - b).abs() <= 0.014_001);
        }
        for index in 0..10 {
            assert_eq!(notice_row_offset(index, 100.0, 1.0), index as f64 * 100.0);
            assert_eq!(
                notice_row_offset(index, 100.0, 0.0),
                index.min(2) as f64 * 6.0
            );
            let near_end = notice_row_offset(index, 100.0, 0.0001);
            assert!((near_end - notice_row_offset(index, 100.0, 0.0)).abs() < 0.1);
        }
    }
    #[test]
    fn status_bar_vector_icons_and_logo_are_neutral_white() {
        let _glass = GlassScope::enter();
        let _bar = BarScope::enter();
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 360, 34).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            let mut options = cairo::FontOptions::new().unwrap();
            options.set_antialias(cairo::Antialias::Gray);
            cr.set_font_options(&options);
            for (i, key) in [
                "temperature",
                "cpu",
                "memory",
                "disk",
                "audio",
                "network",
                "bluetooth",
            ]
            .iter()
            .enumerate()
            {
                super::super::icons::status(
                    &cr,
                    Rect {
                        x: i as f64 * 32.0,
                        y: 0.0,
                        w: 28.0,
                        h: 34.0,
                    },
                    key,
                    " 50%",
                );
            }
            super::super::icons::logo(
                &cr,
                Rect {
                    x: 240.0,
                    y: 0.0,
                    w: 34.0,
                    h: 34.0,
                },
            );
            super::super::icons::battery(
                &cr,
                "Sans 11",
                Rect {
                    x: 288.0,
                    y: 0.0,
                    w: 44.0,
                    h: 34.0,
                },
                10,
                true,
            );
        }
        image.flush();
        let pixels = image.data().unwrap();
        assert!(pixels.chunks_exact(4).any(|p| p[3] > 200));
        for (i, pixel) in pixels.chunks_exact(4).enumerate().filter(|(_, p)| p[3] > 0) {
            assert!(
                pixel[..3].iter().max().unwrap() - pixel[..3].iter().min().unwrap() <= 12,
                "pixel {i}: {pixel:?}"
            );
        }
    }
    #[test]
    fn stacked_cards_only_draw_under_the_front_card() {
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 200, 104).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            card_stack_tail(
                &cr,
                Rect {
                    x: 10.0,
                    y: 10.0,
                    w: 180.0,
                    h: 70.0,
                },
            );
        }
        image.flush();
        let stride = image.stride() as usize;
        let pixels = image.data().unwrap();
        for y in 0..80 {
            for x in 0..200 {
                assert_eq!(pixels[y * stride + x * 4 + 3], 0);
            }
        }
        assert!((80..94).any(|y| (20..180).any(|x| pixels[y * stride + x * 4 + 3] > 0)));
    }

    #[test]
    fn workspace_buttons_have_spacing_even_for_two_digit_indexes() {
        let mut m = Model::new();
        m.workspaces = (1..=12)
            .map(|i| serde_json::json!({"id":i,"idx":i,"output":"test","is_active":i==1}))
            .collect();
        let tmp = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&tmp).unwrap();
        let width = bar_width(&cr, &m, "left", "test");
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, width, 34).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = bar(&cr, &m, 1, "left", "test", width);
        let workspaces = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::Workspace(_)))
            .collect::<Vec<_>>();
        assert_eq!(workspaces.len(), 12);
        for pair in workspaces.windows(2) {
            assert!(pair[0].rect.x + pair[0].rect.w + 3.9 <= pair[1].rect.x);
        }
        assert!(workspaces.last().unwrap().rect.x + 28.0 < f64::from(width));
    }

    #[test]
    fn enabled_control_tiles_have_white_backgrounds() {
        let mut m = Model::new();
        m.common.wifi = Some(true);
        m.common.bluetooth = Some(true);
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            popup(&cr, &m, 1, 500);
        }
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        let pixel = &data[90 * stride + 40 * 4..90 * stride + 40 * 4 + 4];
        assert!(pixel[0] > 210 && pixel[1] > 210 && pixel[2] > 210);
    }
    #[test]
    fn glass_palette_never_leaks_into_the_status_bar() {
        let m = Model::new();
        let render_bar = || {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 200, 34).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                bar(&cr, &m, 8, "left", "test", 200);
            }
            image.flush();
            let bytes = image.data().unwrap().to_vec();
            bytes
        };
        let before = render_bar();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        popup(&cr, &m, 1, 500);
        assert!(!is_glass());
        assert_eq!(before, render_bar());
    }

    #[test]
    fn glass_panel_remains_translucent_and_uses_neutral_white_gauges() {
        let mut m = Model::new();
        m.common.brightness = Some(50.0);
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            popup(&cr, &m, 1, 500);
        }
        image.flush();
        let stride = image.stride() as usize;
        let data = image.data().unwrap();
        assert!(data[480 * stride + 440 * 4 + 3] < 190);
        let gauge = &data[414 * stride + 150 * 4..414 * stride + 150 * 4 + 3];
        assert!(gauge.iter().max().unwrap() - gauge.iter().min().unwrap() < 12);
    }
    #[test]
    fn home_tiles_have_golden_ratio_and_separate_detail_arrows() {
        let m = Model::new();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, 500);
        for action in [Action::Wifi, Action::Bluetooth, Action::Profile] {
            let h = scene
                .hits
                .iter()
                .find(|h| std::mem::discriminant(&h.action) == std::mem::discriminant(&action))
                .unwrap();
            assert!(((h.rect.w + 34.0) / h.rect.h - 1.618_033_988_75).abs() < 0.001);
            assert!(h.rect.h < 90.0);
        }
        let arrow = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Page(Page::Network)))
            .unwrap();
        assert!(arrow.rect.h > arrow.rect.w);
        assert_eq!(arrow.rect.w, 34.0);
    }

    #[test]
    fn connected_wifi_is_pinned_above_scanned_list_without_duplicates() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        let active = crate::network_backend::Ap {
            active: true,
            bssid: "aa".into(),
            ssid: "Connected".into(),
            signal: 90,
            security: "WPA2".into(),
            device: "wlan0".into(),
        };
        let mut aps = vec![active.clone()];
        for i in 0..12 {
            aps.push(crate::network_backend::Ap {
                active: false,
                bssid: format!("{i}"),
                ssid: format!("AP {i}"),
                ..active.clone()
            });
        }
        m.network = Some(crate::network_backend::State {
            wifi: true,
            devices: vec![],
            profiles: vec![],
            aps,
            ips: Default::default(),
        });
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, 500);
        let connected = scene
            .hits
            .iter()
            .filter(|h| matches!(&h.action, Action::Ap(ap) if ap.active))
            .collect::<Vec<_>>();
        assert_eq!(connected.len(), 1);
        let y = connected[0].rect.y;
        let track = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Scrollbar(_)))
            .unwrap();
        assert!(y + connected[0].rect.h < track.rect.y);
        m.scroll = 90.0;
        let scene = popup(&cr, &m, 1, 500);
        let pinned = scene
            .hits
            .iter()
            .find(|h| matches!(&h.action, Action::Ap(ap) if ap.active))
            .unwrap();
        assert_eq!(pinned.rect.y, y);
    }
    #[test]
    fn sunday_first_calendar_offsets_match_known_dates() {
        assert_eq!(sunday_offset(2026, 11), 0);
        assert_eq!(sunday_offset(2026, 6), 1);
        assert_eq!(sunday_offset(2026, 8), 6);
    }
    #[test]
    fn notification_body_is_at_most_three_complete_lines_with_ellipsis() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, 500).unwrap();
        let cr = Context::new(&surface).unwrap();
        let body = "한글과 English를 섞은 긴 알림입니다.\n".repeat(30);
        let layout = notification_body_layout(&cr, "Sans 11", &body, 396.0);
        assert_eq!(layout.line_count(), 3);
        assert!(layout.is_ellipsized());
        let height = layout.pixel_size().1;
        for line in layout.lines_readonly() {
            assert!(line.pixel_extents().1.height() > 0);
        }
        assert!(height >= 3 * 14);
        let short = notification_body_layout(&cr, "Sans 11", "짧은 알림", 396.0);
        assert_eq!(short.line_count(), 1);
        assert!(!short.is_ellipsized());
    }
    #[test]
    fn tray_menu_uses_compact_width_and_auto_height_with_a_draggable_scrollbar() {
        let mut m = Model::new();
        m.page = Page::TrayMenu;
        assert_eq!(popup_width(&m), 340);
        for count in [0, 1, 3, 50] {
            m.menu = (0..count)
                .map(|id| super::super::tray::MenuEntry {
                    id,
                    label: format!("메뉴 {id}"),
                    enabled: true,
                })
                .collect();
            let h = popup_height(&m);
            assert_eq!(h, (count * 34 + 40).clamp(74, HEIGHT));
            let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 340, h).unwrap();
            let cr = Context::new(&surface).unwrap();
            let scene = popup(&cr, &m, 1, h);
            assert_eq!(
                scene
                    .hits
                    .iter()
                    .any(|h| matches!(h.action, Action::Scrollbar(_))),
                count == 50
            );
            assert!(!scene
                .hits
                .iter()
                .any(|h| matches!(h.action, Action::Page(_))));
            assert!(scene.hits.iter().all(|h| h.rect.x + h.rect.w <= 340.0));
        }
    }
    use super::*;
    #[test]
    fn status_controls_have_no_idle_tint_and_highlight_only_on_hover() {
        let mut m = Model::new();
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&s).unwrap();
        let width = bar_width(&cr, &m, "right", "");
        let render = |m: &Model| {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, width, 34).unwrap();
            let scene;
            {
                let cr = Context::new(&s).unwrap();
                scene = bar(&cr, m, 1, "right", "", width);
            }
            s.flush();
            let pixels = s.data().unwrap().to_vec();
            (scene, pixels)
        };
        let (scene, idle) = render(&m);
        let index = scene
            .hits
            .iter()
            .position(|h| matches!(h.action, Action::Page(Page::Home)))
            .unwrap();
        let x = scene.hits[index].rect.x as usize + 2;
        let group = (17 * width as usize + x) * 4;
        let background = (17 * width as usize + x - 5) * 4;
        // Adjacent points differ slightly due to the glass gradient, not idle tint.
        assert!(idle[group..group + 4]
            .iter()
            .zip(&idle[background..background + 4])
            .all(|(a, b)| a.abs_diff(*b) <= 5));
        m.hover = Some((1, index));
        let (_, hover) = render(&m);
        assert_ne!(&idle[group..group + 4], &hover[group..group + 4]);
    }
    #[test]
    fn launcher_names_wrap_without_ellipsis_and_never_paint_outside_two_lines() {
        let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, 120, 100).unwrap();
        {
            let cr = Context::new(&s).unwrap();
            let layout = app_name_layout(
                &cr,
                "Sans 11",
                "Very long application name with several more words",
                94.0,
            );
            assert!(layout.line_count() > 2);
            assert_eq!(layout.ellipsize(), pango::EllipsizeMode::None);
            app_name(
                &cr,
                "Sans 11",
                "Very long application name with several more words",
                Rect {
                    x: 10.0,
                    y: 10.0,
                    w: 94.0,
                    h: 36.0,
                },
            );
        }
        s.flush();
        let stride = s.stride() as usize;
        let data = s.data().unwrap();
        for y in 0..100 {
            for x in 0..120 {
                if !(10..104).contains(&x) || !(10..46).contains(&y) {
                    assert_eq!(data[y * stride + x * 4 + 3], 0);
                }
            }
        }
    }
    #[test]
    fn clock_stays_in_center_for_every_output_width() {
        let mut m = Model::new();
        m.bar_outputs = serde_json::json!({"laptop":{"logical":{"width":1706}},"external":{"logical":{"width":1920}}});
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 1000, 34).unwrap();
        let cr = Context::new(&s).unwrap();
        for output in ["laptop", "external"] {
            let center = bar(
                &cr,
                &m,
                1,
                "center",
                output,
                bar_width(&cr, &m, "center", output),
            );
            assert!(center
                .hits
                .iter()
                .any(|h| matches!(h.action, Action::Page(Page::Calendar))));
            let left = bar(
                &cr,
                &m,
                2,
                "left",
                output,
                bar_width(&cr, &m, "left", output),
            );
            assert!(!left
                .hits
                .iter()
                .any(|h| matches!(h.action, Action::Page(Page::Calendar))));
        }
        assert_eq!(
            bar_width(&cr, &m, "center", "laptop"),
            bar_width(&cr, &m, "center", "external")
        );
    }
    #[test]
    fn status_controls_are_one_hit_target_without_detail_or_brightness_routes() {
        let mut m = Model::new();
        for (key, text) in [("audio", " 50%"), ("network", ""), ("bluetooth", "")] {
            m.metrics.values.insert(
                key,
                crate::bar_backend::Metric {
                    text: text.into(),
                    ..Default::default()
                },
            );
        }
        for (key, text) in [
            ("temperature", " 50°C"),
            ("cpu", " 5%"),
            ("memory", "󰍛 20%"),
            ("disk", " 40%"),
            ("battery", " 80%"),
        ] {
            m.metrics.values.insert(
                key,
                crate::bar_backend::Metric {
                    text: text.into(),
                    ..Default::default()
                },
            );
        }
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 1000, 34).unwrap();
        let cr = Context::new(&s).unwrap();
        let width = bar_width(&cr, &m, "right", "");
        m.metrics.values.insert(
            "backlight",
            crate::bar_backend::Metric {
                text: "󰃠 80%".into(),
                ..Default::default()
            },
        );
        assert_eq!(bar_width(&cr, &m, "right", ""), width);
        let scene = bar(&cr, &m, 1, "right", "", width);
        assert_eq!(
            scene.hits.len(),
            2,
            "system information must have no click targets"
        );
        let group = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Page(Page::Home)))
            .unwrap();
        assert_eq!(
            group.rect.w, 144.0,
            "battery belongs to the same hover and click group"
        );
        m.metrics.values.get_mut("audio").unwrap().text = " 100%".into();
        assert_eq!(
            bar_width(&cr, &m, "right", ""),
            width,
            "volume number must not affect bar width"
        );
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::Page(Page::Home)))
                .count(),
            1
        );
        assert!(!scene.hits.iter().any(|h| matches!(
            h.action,
            Action::Page(Page::Audio | Page::Network | Page::Bluetooth | Page::Display)
        )));
    }
    #[test]
    fn display_gear_hover_does_not_highlight_the_brightness_icon() {
        let mut m = Model::new();
        let render = |m: &Model| {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
            let scene;
            {
                let cr = Context::new(&s).unwrap();
                scene = popup(&cr, m, 1, HEIGHT);
            }
            s.flush();
            let pixels = s.data().unwrap().to_vec();
            (scene, pixels)
        };
        let (scene, before) = render(&m);
        let index = scene
            .hits
            .iter()
            .position(|h| matches!(h.action, Action::Page(Page::Display)))
            .unwrap();
        m.hover = Some((1, index));
        let (_, after) = render(&m);
        for y in 353..381 {
            let start = (y * WIDTH as usize + 26) * 4;
            assert_eq!(
                &before[start..start + 30 * 4],
                &after[start..start + 30 * 4]
            );
        }
    }
    #[test]
    fn launcher_is_five_columns_with_scroll_clipped_hit_boxes() {
        let mut m = Model::new();
        m.navigate(Page::Launcher);
        for i in 0..40 {
            m.launcher.apps.push(super::super::launcher::App {
                id: format!("test{i:02}"),
                name: format!("App {i:02}"),
                search: format!("app {i:02}"),
                icon_name: String::new(),
                icon: None,
            });
        }
        let s =
            cairo::ImageSurface::create(cairo::Format::ARgb32, popup_width(&m), popup_height(&m))
                .unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, popup_height(&m));
        let apps: Vec<_> = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::App(_)))
            .collect();
        assert_eq!(apps.len(), 25);
        assert!(apps[..5].iter().all(|h| h.rect.y == 84.0));
        assert!(apps
            .iter()
            .take(20)
            .all(|h| h.rect.w == 92.0 && h.rect.h == 96.0));
        assert_eq!(apps[5].rect.y - apps[0].rect.y, 104.0);
        assert!(apps[5].rect.y > apps[0].rect.y);
        assert!(scene.content_height > scene.viewport_height);
        assert!(apps
            .iter()
            .all(|h| h.rect.y >= 80.0 && h.rect.y + h.rect.h <= 540.0));
    }
    #[test]
    fn launcher_focus_has_equal_padding_around_icon_and_two_line_name() {
        let r = Rect {
            x: 24.0,
            y: 84.0,
            w: 92.0,
            h: 96.0,
        };
        let (icon, name) = launcher_content(r);
        for padding in [
            icon.y - r.y,
            name.x - r.x,
            r.x + r.w - name.x - name.w,
            r.y + r.h - name.y - name.h,
            name.y - icon.y - icon.h,
        ] {
            assert_eq!(padding, 8.0);
        }
        assert_eq!(icon.x + icon.w / 2.0, name.x + name.w / 2.0);
    }
    #[test]
    fn calendar_and_notifications_share_a_panel_without_back_or_close_buttons() {
        let mut m = Model::new();
        m.page = Page::Calendar;
        assert_eq!(
            (popup_width(&m), popup_height(&m)),
            (crate::notifications::WIDTH, crate::notifications::HEIGHT)
        );
        let s =
            cairo::ImageSurface::create(cairo::Format::ARgb32, popup_width(&m), popup_height(&m))
                .unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, popup_height(&m));
        assert!(!scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Page(_))));
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::CalendarOffset(_)))
                .count(),
            2
        );
        assert_eq!(scene.hits.len(), 4);
        assert_eq!(crate::notifications::CELL, 40.0);
    }
    #[test]
    fn notifications_group_expand_and_hover_delete_without_header_overlap() {
        let mut m = Model::new();
        m.page = Page::Calendar;
        m.notifications.notices = (1..=4)
            .rev()
            .map(|id| crate::notifications::Notice {
                id,
                app: "Codex".into(),
                summary: "실제 답변".into(),
                body: "세 줄을 넘는 긴 답변 본문입니다. ".repeat(20),
                active: false,
                icon_name: String::new(),
                ..Default::default()
            })
            .collect();
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, 500).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, 500);
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::NoticeGroup(_)))
                .count(),
            1
        );
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::DeleteNotices(_))));
        m.pointer = Some((1, 740.0, 80.0));
        let scene = popup(&cr, &m, 1, 500);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(&h.action, Action::DeleteNotices(ids) if ids == &[4])));
        m.activate(Action::NoticeGroup("codex".into()));
        let _ = popup(&cr, &m, 1, 500);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let scene = popup(&cr, &m, 1, 500);
        assert!(scene.content_height > scene.viewport_height);
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::NoticeGroup(_)))
                .count(),
            1,
            "only the app header toggles a group"
        );
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(&h.action, Action::DeleteNotices(ids) if ids.len()==1)));
        assert!(scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::NoticeGroup(_) | Action::DeleteNotices(_)))
            .all(|h| h.rect.y >= 62.0));
        m.activate(Action::DeleteNotices(vec![4]));
        assert_eq!(m.notifications.notices.len(), 3);
        m.activate(Action::ClearNotices);
        assert!(m.notifications.notices.is_empty());
        m.readonly = true;
        m.activate(Action::NotificationDnd);
        assert!(!m.notifications.dnd);
    }

    #[test]
    fn notification_group_header_has_its_own_background() {
        let mut m = Model::new();
        m.page = Page::Calendar;
        m.notifications.notices.push(crate::notifications::Notice {
            id: 1,
            app: "Test".into(),
            body: "본문".into(),
            ..Default::default()
        });
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 780, 500).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            popup(&cr, &m, 1, 500);
        }
        image.flush();
        let stride = image.stride() as usize;
        let pixels = image.data().unwrap();
        let alpha = |x: usize, y: usize| pixels[y * stride + x * 4 + 3];
        assert!(alpha(550, 63) > alpha(550, 107));
    }

    #[test]
    fn media_header_padding_does_not_overlap_controls_or_sliders() {
        let mut m = Model::new();
        m.common.media = Some(crate::control_backend::Media {
            player: "test".into(),
            title: "Track".into(),
            ..Default::default()
        });
        m.media_app_name = "Music App".into();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, 500);
        for h in &scene.hits {
            if matches!(h.action, Action::Media(_)) {
                let offset = (440.0 - 16.0) / 3.0 / 1.618_033_988_75 - 66.0;
                assert_eq!(h.rect.y, 246.0 + offset);
                assert!(h.rect.y + h.rect.h < 288.0 + offset);
            }
            if matches!(h.action, Action::Slider(Slider::Volume)) {
                assert!(h.rect.y >= 304.0);
            }
        }
    }
    #[test]
    fn wifi_refresh_has_no_hit_target_when_radio_off_or_busy() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        m.network = Some(crate::network_backend::State {
            wifi: false,
            devices: vec![],
            profiles: vec![],
            aps: vec![],
            ips: Default::default(),
        });
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Wifi) && h.rect.w == 44.0));
        assert!(!scene.hits.iter().any(|h| matches!(h.action, Action::Scan)));
        m.network.as_mut().unwrap().wifi = true;
        assert!(popup(&cr, &m, 1, HEIGHT)
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Scan)));
        m.busy = true;
        assert!(!popup(&cr, &m, 1, HEIGHT)
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Scan)));
    }
    #[test]
    fn spinner_does_not_connect_or_inherit_an_existing_path() {
        let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, 80, 80).unwrap();
        {
            let cr = Context::new(&s).unwrap();
            cr.move_to(1.0, 1.0);
            cr.line_to(79.0, 79.0);
            spinner(&cr, 40.0, 40.0);
        }
        s.flush();
        let stride = s.stride() as usize;
        let data = s.data().unwrap();
        for y in 0..80 {
            for x in 0..80 {
                if !(31..49).contains(&x) || !(31..49).contains(&y) {
                    assert_eq!(data[y * stride + x * 4 + 3], 0);
                }
            }
        }
    }
    #[test]
    fn low_gauge_never_paints_outside_capsule() {
        for v in [0.0, 1.0, 4.0, 8.0, 50.0, 100.0] {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, 400, 38).unwrap();
            {
                let cr = Context::new(&s).unwrap();
                gauge(
                    &cr,
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 400.0,
                        h: 38.0,
                    },
                    v,
                );
            }
            let stride = s.stride() as usize;
            let b = s.data().unwrap();
            for (x, y) in [(0, 0), (1, 1), (399, 0), (0, 37), (399, 37)] {
                assert_eq!(b[y * stride + x * 4 + 3], 0, "{v}% outside {x},{y}");
            }
        }
    }
    #[test]
    fn authentication_entry_survives_missing_access_point() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        m.entry = true;
        m.auth_prompt = true;
        m.busy = true;
        m.wifi = Some(crate::network_backend::Ap {
            active: false,
            bssid: "00:00:00:00:00:01".into(),
            ssid: "UI fixture".into(),
            signal: 80,
            security: "WPA2".into(),
            device: "wlan0".into(),
        });
        m.network = Some(crate::network_backend::State {
            wifi: true,
            devices: vec![],
            profiles: vec![],
            aps: vec![],
            ips: Default::default(),
        });
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 400).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, 400);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Secret)));
    }
    #[test]
    fn long_tray_menu_scrolls_without_header_hit_overlap() {
        let mut m = Model::new();
        m.navigate(Page::TrayMenu);
        m.menu = (0..30)
            .map(|id| super::super::tray::MenuEntry {
                id,
                label: format!("Entry {id}"),
                enabled: true,
            })
            .collect();
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 580).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, 580);
        assert!(scene.content_height > scene.viewport_height);
        assert!(scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::MenuEvent(_)))
            .all(|h| h.rect.y >= 18.0));
        assert!(!scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Page(_))));
        m.scroll = scene.content_height - scene.viewport_height;
        let scene = popup(&cr, &m, 1, 580);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::MenuEvent(29))));
    }
    #[test]
    fn uniform_height_and_audio_device_menu() {
        let mut m = Model::new();
        for page in [
            Page::Home,
            Page::Audio,
            Page::Network,
            Page::Bluetooth,
            Page::Power,
            Page::Display,
            Page::Calendar,
        ] {
            m.page = page;
            assert_eq!(popup_height(&m), HEIGHT);
        }
        m.page = Page::Audio;
        assert_eq!(popup_height(&m), HEIGHT);
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 400).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = popup(&cr, &m, 1, 400);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::DeviceMenu(_))));
    }
}
