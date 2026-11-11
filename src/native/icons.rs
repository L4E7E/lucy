//! Lucy status icons: one vector grid, independent of private-use font glyphs.
use super::{draw, model::Rect};
use cairo::Context;
use std::f64::consts::PI;

/// Two continuous return arrows on a symmetric 24 px grid.
pub fn refresh(cr: &Context, r: Rect, enabled: bool) {
    let _ = cr.save();
    // Match the spinner's 14 px ink diameter while retaining the 32 px hit box.
    cr.translate(r.x + (r.w - 18.0) / 2.0, r.y + (r.h - 16.0) / 2.0);
    cr.scale(0.75, 2.0 / 3.0);
    cr.set_source_rgba(0.96, 0.97, 1.0, if enabled { 1.0 } else { 0.28 });
    cr.set_line_width(2.4);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.new_path();
    cr.move_to(20.0, 9.0);
    cr.curve_to(18.6, 4.1, 12.8, 2.2, 8.3, 4.5);
    cr.curve_to(6.2, 5.5, 4.7, 7.4, 4.1, 9.5);
    cr.move_to(20.0, 4.0);
    cr.line_to(20.0, 9.0);
    cr.line_to(15.0, 9.0);
    cr.move_to(4.0, 15.0);
    cr.curve_to(5.4, 19.9, 11.2, 21.8, 15.7, 19.5);
    cr.curve_to(17.8, 18.5, 19.3, 16.6, 19.9, 14.5);
    cr.move_to(4.0, 20.0);
    cr.line_to(4.0, 15.0);
    cr.line_to(9.0, 15.0);
    let _ = cr.stroke();
    let _ = cr.restore();
}

/// Conventional app drawer: nine rounded tiles, with no logo asset or font glyph.
pub fn launcher(cr: &Context, r: Rect) {
    let size = 14.0_f64.min(r.w).min(r.h);
    if size <= 0.0 {
        return;
    }
    let _ = cr.save();
    cr.translate(r.x + (r.w - size) / 2.0, r.y + (r.h - size) / 2.0);
    cr.scale(size / 18.0, size / 18.0);
    cr.set_source_rgb(0.96, 0.97, 1.0);
    for row in 0..3 {
        for column in 0..3 {
            draw::round(
                cr,
                Rect {
                    x: f64::from(column) * 7.0,
                    y: f64::from(row) * 7.0,
                    w: 4.0,
                    h: 4.0,
                },
                1.1,
            );
            let _ = cr.fill();
        }
    }
    let _ = cr.restore();
}

pub fn value(text: &str) -> &str {
    text.split_once(' ').map_or("", |(_, value)| value)
}

pub fn battery_level(text: &str) -> u32 {
    text.split_whitespace()
        .last()
        .unwrap_or("")
        .trim_end_matches('%')
        .parse::<u32>()
        .unwrap_or(0)
        .min(100)
}

pub fn width(cr: &Context, font: &str, key: &str, text: &str) -> f64 {
    if key == "battery" {
        44.0
    } else {
        30.0 + draw::measure(cr, font, value(text))
    }
}

pub fn battery(cr: &Context, font: &str, r: Rect, level: u32, charging: bool) {
    let ink = if draw::is_bar() {
        (0.96, 0.97, 1.0)
    } else if draw::is_glass() {
        (0.55, 0.83, 0.65)
    } else {
        (232.0 / 255.0, 234.0 / 255.0, 237.0 / 255.0)
    };
    let _ = cr.save();
    let body = Rect {
        x: r.x + 3.0,
        y: r.y + 8.5,
        w: 34.0,
        h: 17.0,
    };
    draw::round(cr, body, 4.0);
    cr.set_source_rgba(ink.0, ink.1, ink.2, 0.75);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
    draw::round(
        cr,
        Rect {
            x: body.x + 2.5,
            y: body.y + 2.5,
            w: body.w - 5.0,
            h: body.h - 5.0,
        },
        2.0,
    );
    cr.clip();
    cr.rectangle(body.x, body.y, body.w * f64::from(level) / 100.0, body.h);
    if level <= 15 && !draw::is_bar() {
        cr.set_source_rgba(1.0, 0.52, 0.56, 0.28);
    } else {
        cr.set_source_rgba(ink.0, ink.1, ink.2, if charging { 0.28 } else { 0.18 });
    }
    let _ = cr.fill();
    let _ = cr.restore();
    let _ = cr.save();
    cr.set_source_rgba(ink.0, ink.1, ink.2, 0.8);
    draw::round(
        cr,
        Rect {
            x: body.x + body.w + 2.0,
            y: body.y + 4.5,
            w: 2.5,
            h: 8.0,
        },
        1.0,
    );
    let _ = cr.fill();
    let number = if charging {
        Rect {
            x: body.x + 1.0,
            w: body.w - 10.0,
            ..body
        }
    } else {
        body
    };
    draw::text(
        cr,
        font,
        &level.to_string(),
        number,
        false,
        true,
        11.0,
        pango::Alignment::Center,
    );
    if charging {
        // Reserve space after the live number, inside the battery outline.
        if draw::is_bar() {
            cr.set_source_rgb(0.96, 0.97, 1.0);
        } else {
            cr.set_source_rgb(1.0, 0.80, 0.40);
        }
        let x = body.x + body.w - 8.0;
        let y = body.y + 3.0;
        cr.move_to(x + 3.5, y);
        cr.line_to(x, y + 6.0);
        cr.line_to(x + 2.5, y + 6.0);
        cr.line_to(x + 1.0, y + 11.0);
        cr.line_to(x + 5.0, y + 4.5);
        cr.line_to(x + 2.5, y + 4.5);
        cr.close_path();
        let _ = cr.fill();
    }
    let _ = cr.restore();
}

pub fn status(cr: &Context, r: Rect, key: &str, state: &str) {
    status_colored(
        cr,
        r,
        key,
        state,
        if draw::is_bar() {
            (0.96, 0.97, 1.0)
        } else if draw::is_glass() {
            match key {
                "temperature" => (1.0, 0.71, 0.42),
                "cpu" => (0.72, 0.66, 1.0),
                "memory" => (0.50, 0.79, 1.0),
                "disk" => (1.0, 0.65, 0.75),
                "audio" => (0.60, 0.82, 1.0),
                "network" | "wired" | "ethernet" => (0.40, 0.72, 1.0),
                "bluetooth" => (0.66, 0.56, 1.0),
                "power" => (1.0, 0.72, 0.40),
                "notification" => (0.55, 0.76, 1.0),
                "trash" => (1.0, 0.60, 0.64),
                _ => (0.96, 0.97, 1.0),
            }
        } else if key == "power" {
            (0.95, 0.96, 0.97)
        } else {
            (232.0 / 255.0, 234.0 / 255.0, 237.0 / 255.0)
        },
    );
}
pub fn chevron(cr: &Context, r: Rect, expanded: bool) {
    let _ = cr.save();
    let cx = r.x + r.w / 2.0;
    let cy = r.y + r.h / 2.0;
    let direction = if expanded { -1.0 } else { 1.0 };
    cr.set_line_width(1.7);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    let ink = if draw::is_glass() {
        (0.96, 0.97, 1.0)
    } else {
        (232.0 / 255.0, 234.0 / 255.0, 237.0 / 255.0)
    };
    cr.set_source_rgb(ink.0, ink.1, ink.2);
    cr.move_to(cx - 4.5, cy - 2.25 * direction);
    cr.line_to(cx, cy + 2.25 * direction);
    cr.line_to(cx + 4.5, cy - 2.25 * direction);
    let _ = cr.stroke();
    let _ = cr.restore();
}
pub fn status_colored(cr: &Context, r: Rect, key: &str, state: &str, ink: (f64, f64, f64)) {
    status_rgba(cr, r, key, state, (ink.0, ink.1, ink.2, 1.0));
}
pub fn status_rgba(cr: &Context, r: Rect, key: &str, state: &str, ink: (f64, f64, f64, f64)) {
    // The bar keeps a single Network action; choose the jack for its wired state.
    let key = if key == "network" && state.contains('󰈀') {
        "ethernet"
    } else {
        key
    };
    let _ = cr.save();
    let size =
        r.w.min(r.h)
            .min(if key == "bluetooth" { 16.0 } else { 18.0 });
    cr.translate(r.x + (r.w - size) / 2.0, r.y + (r.h - size) / 2.0);
    cr.scale(size / 24.0, size / 24.0);
    cr.new_path();
    cr.set_source_rgba(ink.0, ink.1, ink.2, ink.3);
    cr.set_line_width(1.65);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    match key {
        "temperature" => {
            draw::round(
                cr,
                Rect {
                    x: 9.0,
                    y: 2.5,
                    w: 6.0,
                    h: 14.0,
                },
                3.0,
            );
            let _ = cr.stroke();
            cr.arc(12.0, 18.0, 4.0, 0.0, PI * 2.0);
            let _ = cr.stroke();
            cr.move_to(12.0, 8.0);
            cr.line_to(12.0, 18.0);
            let _ = cr.stroke();
            cr.arc(12.0, 18.0, 1.6, 0.0, PI * 2.0);
            let _ = cr.fill();
        }
        "cpu" => {
            draw::round(
                cr,
                Rect {
                    x: 5.5,
                    y: 5.5,
                    w: 13.0,
                    h: 13.0,
                },
                2.0,
            );
            cr.rectangle(9.0, 9.0, 6.0, 6.0);
            for n in [8.0, 12.0, 16.0] {
                cr.move_to(n, 2.5);
                cr.line_to(n, 5.0);
                cr.move_to(n, 19.0);
                cr.line_to(n, 21.5);
                cr.move_to(2.5, n);
                cr.line_to(5.0, n);
                cr.move_to(19.0, n);
                cr.line_to(21.5, n);
            }
        }
        "memory" => {
            draw::round(
                cr,
                Rect {
                    x: 3.0,
                    y: 6.0,
                    w: 18.0,
                    h: 11.0,
                },
                2.0,
            );
            for x in [6.0, 11.0, 16.0] {
                cr.rectangle(x, 9.0, 2.0, 4.0);
                cr.move_to(x, 18.0);
                cr.line_to(x, 20.0);
            }
        }
        "disk" => {
            draw::round(
                cr,
                Rect {
                    x: 4.0,
                    y: 3.0,
                    w: 16.0,
                    h: 18.0,
                },
                3.0,
            );
            cr.new_sub_path();
            cr.arc(12.0, 10.0, 4.0, 0.0, PI * 2.0);
            cr.move_to(7.0, 17.5);
            cr.line_to(14.0, 17.5);
            cr.move_to(17.0, 17.5);
            cr.line_to(17.1, 17.5);
        }
        "backlight" => {
            cr.arc(12.0, 12.0, 4.0, 0.0, 2.0 * PI);
            for i in 0..8 {
                let angle = f64::from(i) * PI / 4.0;
                cr.move_to(12.0 + angle.cos() * 7.0, 12.0 + angle.sin() * 7.0);
                cr.line_to(12.0 + angle.cos() * 10.0, 12.0 + angle.sin() * 10.0);
            }
        }
        "keyboard-backlight" => {
            draw::round(
                cr,
                Rect {
                    x: 1.5,
                    y: 10.0,
                    w: 21.0,
                    h: 12.0,
                },
                2.0,
            );
            let _ = cr.stroke();
            for y in [13.5, 16.5] {
                for x in [5.0, 8.5, 12.0, 15.5, 19.0] {
                    cr.move_to(x, y);
                    cr.line_to(x + 0.15, y);
                }
            }
            cr.move_to(7.0, 19.5);
            cr.line_to(17.0, 19.5);
            for (x, dx) in [(5.0, -2.0), (12.0, 0.0), (19.0, 2.0)] {
                cr.move_to(x, 6.5);
                cr.line_to(x + dx, 2.5);
            }
        }
        "audio" => {
            let waves = sound_waves(state);
            cr.move_to(3.0, 9.0);
            cr.line_to(7.0, 9.0);
            cr.line_to(12.0, 5.0);
            cr.line_to(12.0, 19.0);
            cr.line_to(7.0, 15.0);
            cr.line_to(3.0, 15.0);
            cr.close_path();
            let _ = cr.fill();
            if waves == 0 {
                cr.move_to(17.0, 9.0);
                cr.line_to(22.0, 15.0);
                cr.move_to(22.0, 9.0);
                cr.line_to(17.0, 15.0);
            } else {
                for radius in [5.0, 7.5, 10.0].into_iter().take(waves) {
                    cr.new_sub_path();
                    cr.arc(11.0, 12.0, radius, -0.65, 0.65);
                }
            }
        }
        "wired" | "ethernet" => {
            // Minimal jack silhouette with four contact pairs, no faceplate.
            cr.move_to(4.5, 5.5);
            cr.line_to(19.5, 5.5);
            cr.line_to(19.5, 13.5);
            cr.line_to(15.5, 13.5);
            cr.line_to(15.5, 18.5);
            cr.line_to(8.5, 18.5);
            cr.line_to(8.5, 13.5);
            cr.line_to(4.5, 13.5);
            cr.close_path();
            let _ = cr.stroke();
            cr.set_line_width(1.2);
            for i in 0..4 {
                let x = 7.0 + f64::from(i) * 10.0 / 3.0;
                cr.move_to(x, 8.0);
                cr.line_to(x, 10.5);
            }
        }
        "network" => {
            if state.contains('') {
                for radius in [5.0, 9.0, 13.0] {
                    cr.new_sub_path();
                    cr.arc(12.0, 19.0, radius, -PI * 0.76, -PI * 0.24);
                }
                cr.move_to(12.0, 19.0);
                cr.line_to(12.1, 19.0);
            } else {
                draw::round(
                    cr,
                    Rect {
                        x: 3.0,
                        y: 4.0,
                        w: 18.0,
                        h: 13.0,
                    },
                    2.0,
                );
                cr.move_to(12.0, 17.0);
                cr.line_to(12.0, 21.0);
                cr.move_to(7.0, 21.0);
                cr.line_to(17.0, 21.0);
                if state.contains('') {
                    cr.move_to(8.0, 7.0);
                    cr.line_to(16.0, 13.0);
                    cr.move_to(16.0, 7.0);
                    cr.line_to(8.0, 13.0);
                } else {
                    cr.move_to(7.0, 8.0);
                    cr.line_to(7.0, 12.0);
                    cr.move_to(11.0, 8.0);
                    cr.line_to(11.0, 12.0);
                    cr.move_to(15.0, 8.0);
                    cr.line_to(15.0, 12.0);
                }
            }
        }
        "vpn" => {
            cr.move_to(12.0, 2.5);
            cr.curve_to(9.5, 4.5, 6.0, 5.0, 3.5, 5.5);
            cr.line_to(3.5, 11.0);
            cr.curve_to(3.5, 16.0, 7.0, 19.5, 12.0, 22.0);
            cr.curve_to(17.0, 19.5, 20.5, 16.0, 20.5, 11.0);
            cr.line_to(20.5, 5.5);
            cr.curve_to(18.0, 5.0, 14.5, 4.5, 12.0, 2.5);
            cr.close_path();
            cr.move_to(8.0, 11.5);
            cr.line_to(11.0, 14.5);
            cr.line_to(16.0, 9.0);
        }
        "lock" => {
            cr.arc(12.0, 9.0, 4.0, PI, 2.0 * PI);
            cr.line_to(16.0, 11.0);
            cr.move_to(8.0, 11.0);
            cr.line_to(8.0, 9.0);
            let _ = cr.stroke();
            draw::round(
                cr,
                Rect {
                    x: 5.0,
                    y: 10.0,
                    w: 14.0,
                    h: 11.0,
                },
                3.0,
            );
            let _ = cr.stroke();
            cr.move_to(12.0, 14.0);
            cr.line_to(12.0, 17.0);
        }
        "link" => {
            let _ = cr.save();
            cr.translate(12.0, 12.0);
            cr.rotate(-PI / 4.0);
            for x in [-10.0, -2.0] {
                draw::round(
                    cr,
                    Rect {
                        x,
                        y: -4.0,
                        w: 12.0,
                        h: 8.0,
                    },
                    4.0,
                );
                let _ = cr.stroke();
            }
            cr.move_to(-3.0, 0.0);
            cr.line_to(3.0, 0.0);
            let _ = cr.stroke();
            let _ = cr.restore();
        }
        "profile" => match state {
            "power-saver" => {
                cr.move_to(6.0, 18.0);
                cr.curve_to(0.0, 10.0, 8.0, 2.0, 21.0, 3.0);
                cr.curve_to(22.0, 14.0, 15.0, 21.0, 6.0, 18.0);
                cr.close_path();
                cr.move_to(4.0, 21.0);
                cr.line_to(16.0, 8.0);
            }
            "performance" => {
                cr.arc(12.0, 14.0, 9.0, PI, 2.0 * PI);
                cr.line_to(20.0, 19.0);
                cr.line_to(4.0, 19.0);
                cr.close_path();
                cr.move_to(12.0, 14.0);
                cr.line_to(18.0, 8.0);
                for a in [PI * 1.15, PI * 1.5, PI * 1.85] {
                    cr.move_to(12.0 + a.cos() * 6.0, 14.0 + a.sin() * 6.0);
                    cr.line_to(12.0 + a.cos() * 8.0, 14.0 + a.sin() * 8.0);
                }
            }
            _ => {
                cr.move_to(12.0, 3.0);
                cr.line_to(12.0, 21.0);
                cr.move_to(7.0, 21.0);
                cr.line_to(17.0, 21.0);
                cr.move_to(3.0, 7.0);
                cr.line_to(21.0, 7.0);
                for x in [5.0, 19.0] {
                    cr.move_to(x, 7.0);
                    cr.line_to(x - 3.0, 14.0);
                    cr.curve_to(x - 2.0, 18.0, x + 2.0, 18.0, x + 3.0, 14.0);
                    cr.close_path();
                }
            }
        },
        "bluetooth" => {
            cr.move_to(12.0, 2.0);
            cr.line_to(18.0, 7.0);
            cr.line_to(6.0, 17.0);
            cr.move_to(6.0, 7.0);
            cr.line_to(18.0, 17.0);
            cr.line_to(12.0, 22.0);
            cr.line_to(12.0, 2.0);
            if state.contains('󰂲') {
                cr.move_to(3.0, 3.0);
                cr.line_to(21.0, 21.0);
            }
        }
        "trash" => {
            cr.move_to(6.0, 8.0);
            cr.line_to(18.0, 8.0);
            cr.move_to(9.0, 5.0);
            cr.line_to(15.0, 5.0);
            cr.move_to(7.0, 8.0);
            cr.line_to(8.0, 20.0);
            cr.line_to(16.0, 20.0);
            cr.line_to(17.0, 8.0);
            cr.move_to(10.0, 11.0);
            cr.line_to(10.5, 17.0);
            cr.move_to(14.0, 11.0);
            cr.line_to(13.5, 17.0);
            let _ = cr.stroke();
        }
        "notification" => {
            cr.move_to(4.0, 17.0);
            cr.line_to(6.0, 14.0);
            cr.line_to(6.0, 10.0);
            cr.arc(12.0, 10.0, 6.0, PI, 2.0 * PI);
            cr.line_to(18.0, 14.0);
            cr.line_to(20.0, 17.0);
            cr.close_path();
            cr.move_to(10.0, 21.0);
            cr.line_to(14.0, 21.0);
            cr.move_to(12.0, 2.0);
            cr.line_to(12.0, 4.0);
            if state.contains('󰂛') {
                cr.move_to(3.0, 3.0);
                cr.line_to(21.0, 21.0);
            }
        }
        "tray" => {
            let (a, b) = if state == "open" {
                (9.0, 15.0)
            } else {
                (15.0, 9.0)
            };
            cr.move_to(a, 6.0);
            cr.line_to(b, 12.0);
            cr.line_to(a, 18.0);
        }
        "sleep" => {
            cr.move_to(18.5, 18.0);
            cr.curve_to(5.5, 23.0, -1.0, 7.0, 10.0, 3.0);
            cr.curve_to(5.0, 12.0, 11.0, 20.0, 18.5, 18.0);
            cr.close_path();
        }
        "logout" => {
            cr.move_to(10.0, 3.0);
            cr.line_to(4.0, 3.0);
            cr.line_to(4.0, 21.0);
            cr.line_to(10.0, 21.0);
            cr.move_to(10.0, 12.0);
            cr.line_to(21.0, 12.0);
            cr.move_to(16.0, 7.0);
            cr.line_to(21.0, 12.0);
            cr.line_to(16.0, 17.0);
        }
        "restart" => {
            cr.arc(12.0, 12.0, 8.0, -PI * 0.3, PI * 1.4);
            cr.move_to(18.5, 3.0);
            cr.line_to(18.5, 7.0);
            cr.line_to(14.5, 7.0);
        }
        "power" => {
            cr.arc(12.0, 13.0, 8.0, -PI * 0.30, PI * 1.30);
            cr.move_to(12.0, 2.0);
            cr.line_to(12.0, 11.0);
        }
        "apps" => {
            for x in [4.0, 14.0] {
                for y in [4.0, 14.0] {
                    draw::round(
                        cr,
                        Rect {
                            x,
                            y,
                            w: 6.0,
                            h: 6.0,
                        },
                        1.5,
                    );
                }
            }
        }
        "control" => {
            for (y, knob) in [(6.0, 9.0), (12.0, 16.0), (18.0, 7.0)] {
                cr.move_to(3.0, y);
                cr.line_to(knob - 2.0, y);
                cr.move_to(knob + 2.0, y);
                cr.line_to(21.0, y);
                cr.new_sub_path();
                cr.arc(knob, y, 2.0, 0.0, 2.0 * PI);
            }
        }
        _ => {}
    }
    let _ = cr.stroke();
    let _ = cr.restore();
}
pub fn sound_waves(state: &str) -> usize {
    if state.contains('󰖁') {
        return 0;
    }
    let level = value(state)
        .trim_end_matches('%')
        .parse::<f64>()
        .unwrap_or(0.0);
    if level <= 0.0 {
        0
    } else if level <= 33.0 {
        1
    } else if level <= 66.0 {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wired_and_ethernet_aliases_draw_a_centered_rj45_socket() {
        let render = |key, state, alpha| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 256, 256).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                // Inspect contact geometry above the native antialiasing scale.
                cr.scale(8.0, 8.0);
                status_rgba(
                    &cr,
                    Rect {
                        x: 4.0,
                        y: 4.0,
                        w: 24.0,
                        h: 24.0,
                    },
                    key,
                    state,
                    (1.0, 1.0, 1.0, alpha),
                );
            }
            image.flush();
            let pixels = image.data().unwrap().to_vec();
            pixels
        };
        let wired = render("wired", "", 1.0);
        assert_eq!(wired, render("ethernet", "", 1.0));
        assert_eq!(wired, render("network", "󰈀", 1.0));
        assert_ne!(wired, render("network", "", 1.0));
        let alpha = |x: usize, y: usize| wired[(y * 256 + x) * 4 + 3];
        let mut bounds = (256, 256, 0, 0);
        for y in 0..256 {
            for x in 0..256 {
                if alpha(x, y) > 0 {
                    assert!((32..224).contains(&x) && (32..224).contains(&y));
                    bounds.0 = bounds.0.min(x);
                    bounds.1 = bounds.1.min(y);
                    bounds.2 = bounds.2.max(x);
                    bounds.3 = bounds.3.max(y);
                }
            }
        }
        assert!((bounds.0 + bounds.2).abs_diff(255) <= 1);
        assert!((bounds.1 + bounds.3).abs_diff(255) <= 1);
        // Four clean contact-pair marks keep the small bar icon readable.
        let mut contacts = 0;
        let mut previous = false;
        for x in 92..166 {
            let ink = alpha(x, 112) > 120;
            contacts += usize::from(ink && !previous);
            previous = ink;
        }
        assert_eq!(contacts, 4);
        // Lower central latch recess, with an open interior on either side.
        assert!(alpha(107, 160) > 100 && alpha(149, 160) > 100);
        assert_eq!(alpha(88, 164), 0);
        assert_eq!(alpha(168, 164), 0);
        // No square faceplate outside the jack itself.
        assert_eq!(alpha(74, 160), 0);
        assert_eq!(alpha(182, 160), 0);
        let dim = render("wired", "", 0.4);
        assert!(dim.chunks_exact(4).all(|p| p[3] <= 102));
        assert_eq!(render("ethernet", "", 0.0), vec![0; wired.len()]);
    }
    #[test]
    fn refresh_matches_loading_ring_ink_size_and_stays_centered() {
        use super::*;
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 32, 32).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            refresh(
                &cr,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 32.0,
                    h: 32.0,
                },
                true,
            );
        }
        image.flush();
        let stride = image.stride() as usize;
        let pixels = image.data().unwrap();
        let mut xs = vec![];
        let mut ys = vec![];
        for y in 0..32 {
            for x in 0..32 {
                if pixels[y * stride + x * 4 + 3] > 8 {
                    xs.push(x);
                    ys.push(y);
                }
            }
        }
        let (minx, maxx, miny, maxy) = (
            *xs.iter().min().unwrap(),
            *xs.iter().max().unwrap(),
            *ys.iter().min().unwrap(),
            *ys.iter().max().unwrap(),
        );
        assert!(
            (13..=15).contains(&(maxx - minx + 1)),
            "x bounds {minx}..{maxx}"
        );
        assert!(
            (13..=15).contains(&(maxy - miny + 1)),
            "y bounds {miny}..{maxy}"
        );
        assert!((minx + maxx).abs_diff(31) <= 1);
        assert!((miny + maxy).abs_diff(31) <= 1);
    }
    #[test]
    fn profile_states_and_session_power_use_distinct_centered_vectors() {
        let render = |key, state| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 32, 32).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                status_colored(
                    &cr,
                    Rect {
                        x: 4.0,
                        y: 4.0,
                        w: 24.0,
                        h: 24.0,
                    },
                    key,
                    state,
                    (1.0, 1.0, 1.0),
                );
            }
            image.flush();
            let bytes = image.data().unwrap().to_vec();
            assert!(bytes.chunks_exact(4).filter(|p| p[3] > 0).count() > 30);
            // All strokes stay within the padded icon box.
            for y in 0..32 {
                for x in 0..32 {
                    if x < 4 || y < 4 || x >= 28 || y >= 28 {
                        assert_eq!(bytes[(y * 32 + x) * 4 + 3], 0);
                    }
                }
            }
            bytes
        };
        let icons = [
            render("profile", "power-saver"),
            render("profile", "balanced"),
            render("profile", "performance"),
            render("power", ""),
            render("link", ""),
            render("lock", ""),
            render("sleep", ""),
            render("logout", ""),
            render("restart", ""),
        ];
        for i in 0..icons.len() {
            for j in i + 1..icons.len() {
                assert_ne!(icons[i], icons[j]);
            }
        }
    }
    #[test]
    fn charging_has_a_visible_bolt_without_replacing_the_number() {
        let pixels = |charging, level| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 48, 34).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                battery(
                    &cr,
                    "Sans 11",
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 44.0,
                        h: 34.0,
                    },
                    level,
                    charging,
                );
            }
            image.flush();
            let stride = image.stride() as usize;
            let data = image.data().unwrap();
            let ink = |left: usize, right: usize| {
                (10..24)
                    .flat_map(|y| (left..right).map(move |x| y * stride + x * 4 + 3))
                    .map(|i| u32::from(data[i]))
                    .sum::<u32>()
            };
            // Number on the left, bolt on the right; nothing beyond the terminal.
            (ink(7, 27), ink(29, 34), ink(43, 48))
        };
        for level in [0, 9, 68, 100] {
            let (number, bolt, outside) = pixels(true, level);
            assert!(number > 1000, "number {level} must remain visible");
            assert!(
                bolt > pixels(false, level).1 + 1000,
                "bolt must be inside the body"
            );
            assert_eq!(outside, 0);
        }
    }
    #[test]
    fn expanded_and_collapsed_chevrons_have_identical_bounds() {
        let bounds = |expanded| {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 40, 40).unwrap();
            {
                let cr = cairo::Context::new(&surface).unwrap();
                super::chevron(
                    &cr,
                    super::Rect {
                        x: 5.0,
                        y: 5.0,
                        w: 30.0,
                        h: 30.0,
                    },
                    expanded,
                );
            }
            surface.flush();
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let mut bounds = (40, 40, 0, 0);
            for y in 0..40 {
                for x in 0..40 {
                    if data[y * stride + x * 4 + 3] > 0 {
                        bounds.0 = bounds.0.min(x);
                        bounds.1 = bounds.1.min(y);
                        bounds.2 = bounds.2.max(x);
                        bounds.3 = bounds.3.max(y);
                    }
                }
            }
            bounds
        };
        assert_eq!(bounds(false), bounds(true));
    }
    use super::*;
    #[test]
    fn sound_icon_has_muted_and_three_volume_bands() {
        for (state, count) in [
            ("󰖁", 0),
            (" 0%", 0),
            (" 1%", 1),
            (" 33%", 1),
            (" 34%", 2),
            (" 66%", 2),
            (" 67%", 3),
            (" 100%", 3),
        ] {
            assert_eq!(sound_waves(state), count);
        }
    }
    #[test]
    fn launcher_is_a_centered_nine_tile_grid_inside_the_existing_hit_box() {
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 30, 28).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            launcher(
                &cr,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 30.0,
                    h: 28.0,
                },
            );
        }
        let stride = image.stride() as usize;
        let bytes = image.data().unwrap();
        for y in [8, 14, 19] {
            for x in [9, 15, 20] {
                assert_eq!(bytes[y * stride + x * 4 + 3], 255);
            }
            for x in [12, 17] {
                assert_eq!(bytes[y * stride + x * 4 + 3], 0);
            }
        }
        let mut bounds = (30, 0, 28, 0);
        for y in 0..28 {
            for x in 0..30 {
                if bytes[y * stride + x * 4 + 3] > 8 {
                    bounds = (
                        bounds.0.min(x),
                        bounds.1.max(x),
                        bounds.2.min(y),
                        bounds.3.max(y),
                    );
                }
            }
        }
        assert_eq!(bounds, (8, 21, 7, 20));
    }
    #[test]
    fn battery_number_is_live_clamped_and_has_no_percent_suffix() {
        assert_eq!(battery_level(" 79%"), 79);
        assert_eq!(battery_level("󰂄 100%"), 100);
        assert_eq!(battery_level(" 150%"), 100);
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 80, 34).unwrap();
        let cr = Context::new(&s).unwrap();
        assert_eq!(
            width(&cr, "Sans 11", "battery", " 9%"),
            width(&cr, "Sans 11", "battery", "󰂄 100%")
        );
        for level in [0, 9, 15, 79, 100] {
            battery(
                &cr,
                "Sans 11",
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 60.0,
                    h: 34.0,
                },
                level,
                false,
            );
        }
    }
}
