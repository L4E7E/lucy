//! Clock typography and quiet, fixed-width telemetry; no additional polling.
use super::{
    bar_backend::{Metric, State},
    icons,
    model::Rect,
    text_cache,
};
use cairo::Context;

const STAT_GAP: f64 = 4.0;
const STAT_KEYS: [&str; 4] = ["temperature", "cpu", "memory", "disk"];
const STAT_ICON: Rect = Rect {
    x: 1.0,
    y: 8.0,
    w: 16.0,
    h: 18.0,
};

fn layout(cr: &Context, font: &str, value: &str, clock: bool) -> pango::Layout {
    let key = text_cache::Key::new(
        cr,
        font,
        value,
        if clock { 20 } else { 21 },
        0.0,
        -1,
        pango::Alignment::Left,
        pango::Weight::Medium,
    );
    text_cache::layout(key, || {
        let layout = text_cache::create(cr);
        layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
        layout.set_text(value);
        layout.set_single_paragraph_mode(true);
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrFontFeatures::new("tnum=1"));
        // Only the standalone calendar clock has a quiet separator; the bar
        // keeps its punctuation at the same brightness as its digits.
        if clock && value.len() == 5 {
            if let Some(index) = value.find(':') {
                let mut alpha = pango::AttrInt::new_foreground_alpha(36044);
                alpha.set_start_index(index as u32);
                alpha.set_end_index(index as u32 + 1);
                attrs.insert(alpha);
            }
        }
        layout.set_attributes(Some(&attrs));
        if clock && value.len() != 5 {
            if let Some(index) = value.find(':') {
                if let Some(rise) = colon_rise(&layout, index) {
                    let mut attr = pango::AttrInt::new_rise(rise);
                    attr.set_start_index(index as u32);
                    attr.set_end_index(index as u32 + 1);
                    attrs.insert(attr);
                    layout.set_attributes(Some(&attrs));
                }
            }
        }
        layout
    })
}

fn colon_rise(layout: &pango::Layout, index: usize) -> Option<i32> {
    let (mut digit, mut colon) = (None, None);
    let mut iter = layout.iter();
    loop {
        let offset = iter.index() as usize;
        if offset + 1 == index || offset == index {
            let (ink, _) = iter.cluster_extents();
            let center = ink.y() + ink.height() / 2;
            if offset == index {
                colon = Some(center);
            } else {
                digit = Some(center);
            }
        }
        if !iter.next_cluster() {
            break;
        }
    }
    Some(colon? - digit?)
}

pub fn clock_width(cr: &Context, font: &str, value: &str) -> f64 {
    f64::from(layout(cr, font, value, true).pixel_size().0)
}

pub fn clock(cr: &Context, font: &str, value: &str, r: Rect) {
    let layout = layout(cr, font, value, true);
    let (w, h) = layout.pixel_size();
    cr.set_source_rgb(0.96, 0.97, 1.0);
    let x = r.x + (r.w - f64::from(w)) / 2.0;
    let y = r.y + (r.h - f64::from(h)) / 2.0;
    let _ = cr.save();
    // Stretch only the standalone HH:MM widget, not the collapsed bar's date.
    // Keep the ink bottom anchored, just like the lock-screen clock.
    if value.len() == 5 {
        let (ink, _) = layout.pixel_extents();
        let bottom = y + f64::from(ink.y() + ink.height());
        cr.translate(0.0, bottom);
        cr.scale(1.0, super::lock_glass::CLOCK_HEIGHT_RATIO);
        cr.translate(0.0, -bottom);
    }
    cr.move_to(x, y);
    text_cache::show(cr, &layout);
    let _ = cr.restore();
}

fn fonts(font: &str) -> (String, String) {
    let mut number = pango::FontDescription::from_string(font);
    number.set_weight(pango::Weight::Medium);
    let mut unit = number.clone();
    unit.set_weight(pango::Weight::Normal);
    if number.is_size_absolute() {
        unit.set_absolute_size(f64::from(number.size()) * 0.85);
    } else {
        unit.set_size((f64::from(number.size()) * 0.85).round() as i32);
    }
    (number.to_string(), unit.to_string())
}

fn cells(cr: &Context, font: &str) -> (f64, f64) {
    let (number, unit) = fonts(font);
    (
        f64::from(layout(cr, &number, "100", false).pixel_size().0),
        ["°C", "%"]
            .iter()
            .map(|value| f64::from(layout(cr, &unit, value, false).pixel_size().0))
            .fold(0.0, f64::max),
    )
}

/// Reserve the outer block, not blank digit cells between individual items.
fn stat_pitch(cr: &Context, font: &str) -> f64 {
    let (number, unit) = cells(cr, font);
    22.0 + number + 2.0 + unit + STAT_GAP
}

fn icon_left(key: &str) -> f64 {
    thread_local! {
        // Cache four vector bounds once, rather than rasterizing probes per frame.
        static LEFT: [f64; 4] = STAT_KEYS.map(|key| {
            let Ok(surface) = cairo::RecordingSurface::create(cairo::Content::Alpha, None) else { return STAT_ICON.x; };
            let Ok(cr) = Context::new(&surface) else { return STAT_ICON.x; };
            icons::status_rgba(&cr, STAT_ICON, key, "", (1.0, 1.0, 1.0, 1.0));
            surface.ink_extents().0
        });
    }
    LEFT.with(|left| left[STAT_KEYS.iter().position(|k| *k == key).unwrap()])
}

fn ink_right(layout: &pango::Layout) -> f64 {
    let (ink, _) = layout.extents();
    f64::from(ink.x() + ink.width()) / f64::from(pango::SCALE)
}

fn stat_right(cr: &Context, font: &str, metric: &Metric) -> f64 {
    let (number_font, unit_font) = fonts(font);
    let (number, unit) = parts(icons::value(&metric.text));
    let number = layout(cr, &number_font, number, false);
    let unit = layout(cr, &unit_font, unit, false);
    (22.0 + ink_right(&number)).max(23.5 + f64::from(number.pixel_size().0) + ink_right(&unit))
}

pub fn stats_width(cr: &Context, font: &str, state: &State) -> f64 {
    STAT_KEYS
        .iter()
        .filter(|key| state.values.contains_key(**key))
        .count() as f64
        * stat_pitch(cr, font)
}

fn stat_positions(cr: &Context, font: &str, state: &State, start: f64) -> Vec<(&'static str, f64)> {
    let spans: Vec<_> = STAT_KEYS
        .iter()
        .filter_map(|&key| {
            let metric = state.values.get(key)?;
            Some((key, icon_left(key), stat_right(cr, font, metric)))
        })
        .collect();
    let Some(first) = spans.first() else {
        return Vec::new();
    };
    let used = spans
        .iter()
        .map(|(_, left, right)| right - left)
        .sum::<f64>();
    let gap = if spans.len() > 1 {
        ((stats_width(cr, font, state) - first.1 - STAT_GAP - used) / (spans.len() - 1) as f64)
            .max(STAT_GAP)
    } else {
        0.0
    };
    let mut cursor = start + first.1;
    spans
        .into_iter()
        .map(|(key, left, right)| {
            let x = cursor - left;
            cursor += right - left + gap;
            (key, x)
        })
        .collect()
}

pub fn stats(cr: &Context, font: &str, state: &State, start: f64) -> f64 {
    for (key, x) in stat_positions(cr, font, state, start) {
        stat(cr, font, key, &state.values[key], x);
    }
    stats_width(cr, font, state)
}

fn parts(value: &str) -> (&str, &str) {
    let end = value
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(value.len());
    if end == 0 {
        (value, "")
    } else {
        value.split_at(end)
    }
}

fn stat(cr: &Context, font: &str, key: &str, metric: &Metric, x: f64) {
    let (number_font, unit_font) = fonts(font);
    let value = icons::value(&metric.text);
    let (number, unit) = parts(value);
    let ink = if metric.critical {
        (1.0, 0.62, 0.42)
    } else {
        (0.96, 0.97, 1.0)
    };
    icons::status_rgba(
        cr,
        Rect {
            x: x + STAT_ICON.x,
            ..STAT_ICON
        },
        key,
        &metric.text,
        (ink.0, ink.1, ink.2, 0.80),
    );
    let numeric = layout(cr, &number_font, number, false);
    let suffix = layout(cr, &unit_font, unit, false);
    // Share the baseline, not the bounding-box center of unequal font sizes.
    let baseline = (34.0 - f64::from(numeric.pixel_size().1)) / 2.0
        + f64::from(numeric.baseline()) / f64::from(pango::SCALE);
    cr.set_source_rgb(ink.0, ink.1, ink.2);
    cr.move_to(
        x + 22.0,
        baseline - f64::from(numeric.baseline()) / f64::from(pango::SCALE),
    );
    text_cache::show(cr, &numeric);
    cr.set_source_rgba(ink.0, ink.1, ink.2, 0.75);
    cr.move_to(
        x + 23.5 + f64::from(numeric.pixel_size().0),
        baseline - f64::from(suffix.baseline()) / f64::from(pango::SCALE),
    );
    text_cache::show(cr, &suffix);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_clock_stretches_only_vertically_and_leaves_bar_unchanged() {
        let render = |value: &str, tall: bool, scale: f64| {
            let mut image = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                (440.0 * scale) as i32,
                (160.0 * scale) as i32,
            )
            .unwrap();
            {
                let cr = Context::new(&image).unwrap();
                cr.scale(scale, scale);
                let r = Rect {
                    x: 20.0,
                    y: 20.0,
                    w: 400.0,
                    h: 102.0,
                };
                if tall {
                    clock(&cr, "Sans 58px", value, r);
                } else {
                    let l = layout(&cr, "Sans 58px", value, true);
                    let (w, h) = l.pixel_size();
                    cr.set_source_rgb(0.96, 0.97, 1.0);
                    cr.move_to(
                        r.x + (r.w - f64::from(w)) / 2.0,
                        r.y + (r.h - f64::from(h)) / 2.0,
                    );
                    text_cache::show(&cr, &l);
                }
            }
            image.flush();
            let stride = image.stride() as usize;
            let width = image.width() as usize;
            let bytes = image.data().unwrap().to_vec();
            let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
            for (y, row) in bytes.chunks_exact(stride).enumerate() {
                for x in 0..width {
                    if row[x * 4 + 3] > 32 {
                        left = left.min(x);
                        top = top.min(y);
                        right = right.max(x);
                        bottom = bottom.max(y);
                    }
                }
            }
            (bytes, (left, top, right, bottom))
        };
        for scale in [1.0, 1.5, 2.0] {
            let (_, a) = render("22:20", false, scale);
            let (_, b) = render("22:20", true, scale);
            assert!(a.0.abs_diff(b.0) <= 1 && a.2.abs_diff(b.2) <= 1);
            assert!(a.3.abs_diff(b.3) <= 1);
            assert!((b.3 - b.1) as f64 > (a.3 - a.1) as f64 * 1.10);
            assert_eq!(
                render("11월 09일 22:20", false, scale).0,
                render("11월 09일 22:20", true, scale).0
            );
        }
    }
    #[test]
    fn bar_clock_colon_matches_digit_brightness_and_vertical_center() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 400, 60).unwrap();
        let cr = Context::new(&image).unwrap();
        let value = "11월 09일 월요일 10:13";
        let bar = layout(&cr, "Pretendard JP Medium 11", value, true);
        assert!(colon_rise(&bar, value.find(':').unwrap()).unwrap().abs() <= pango::SCALE);
        let attributes = bar.attributes().unwrap();
        let mut iter = attributes.iterator();
        loop {
            assert!(iter.get(pango::AttrType::ForegroundAlpha).is_none());
            if !iter.next_style_change() {
                break;
            }
        }
        let calendar = layout(&cr, "Pretendard JP Medium 58px", "10:13", true);
        let attributes = calendar.attributes().unwrap();
        let mut iter = attributes.iterator();
        assert!(iter.next_style_change());
        assert!(iter.get(pango::AttrType::ForegroundAlpha).is_some());
    }
    #[test]
    fn visible_gaps_match_for_short_and_long_cpu_values_not_just_icon_centers() {
        for scale in [1.0, 1.5, 2.0] {
            for font in ["Sans 11", "Pretendard JP Medium 11"] {
                for cpu in ["1%", "9%", "15%", "100%"] {
                    let mut state = State::default();
                    for (key, value) in STAT_KEYS.into_iter().zip(["48°C", cpu, "17%", "39%"]) {
                        state.values.insert(
                            key,
                            Metric {
                                text: format!("icon {value}"),
                                ..Default::default()
                            },
                        );
                    }
                    let mut image =
                        cairo::ImageSurface::create(cairo::Format::ARgb32, 1000, 68).unwrap();
                    let positions = {
                        let cr = Context::new(&image).unwrap();
                        cr.scale(scale, scale);
                        let positions = stat_positions(&cr, font, &state, 8.0);
                        let gaps: Vec<_> = positions
                            .windows(2)
                            .map(|pair| {
                                pair[1].1 + icon_left(pair[1].0)
                                    - pair[0].1
                                    - stat_right(&cr, font, &state.values[pair[0].0])
                            })
                            .collect();
                        assert!(gaps.iter().all(|gap| (gap - gaps[0]).abs() < 0.001));
                        stats(&cr, font, &state, 8.0);
                        positions
                            .into_iter()
                            .map(|(key, x)| {
                                (
                                    (x + icon_left(key)) * scale,
                                    (x + stat_right(&cr, font, &state.values[key])) * scale,
                                )
                            })
                            .collect::<Vec<_>>()
                    };
                    let stride = image.stride() as usize;
                    let bytes = image.data().unwrap();
                    let bounds: Vec<_> = positions
                        .iter()
                        .map(|(left, right)| {
                            let mut bounds = (1000usize, 0usize);
                            for y in 0..(34.0 * scale) as usize {
                                for x in (left.floor() as usize).saturating_sub(2)
                                    ..right.ceil() as usize + 2
                                {
                                    if bytes[y * stride + x * 4 + 3] > 8 {
                                        bounds = (bounds.0.min(x), bounds.1.max(x));
                                    }
                                }
                            }
                            assert!(bounds.0 < bounds.1);
                            bounds
                        })
                        .collect();
                    let gaps: Vec<_> = bounds.windows(2).map(|p| p[1].0 - p[0].1 - 1).collect();
                    assert!(
                        gaps.iter().max().unwrap() - gaps.iter().min().unwrap() <= 2,
                        "{font}, CPU {cpu}, {scale}x: actual pixel gaps {gaps:?}"
                    );
                }
            }
        }
    }
    #[test]
    fn reserved_stat_region_fits_temperature_and_percent_at_each_output_scale() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 600, 68).unwrap();
        for scale in [1.0, 1.5, 2.0] {
            let cr = Context::new(&image).unwrap();
            cr.scale(scale, scale);
            for font in ["Sans 11", "Pretendard JP Medium 11"] {
                let (number_font, unit_font) = fonts(font);
                let pitch = stat_pitch(&cr, font);
                for value in ["0%", "9%", "100%", "48°C", "100°C"] {
                    let (number, unit) = parts(value);
                    let width = 23.5
                        + f64::from(layout(&cr, &number_font, number, false).pixel_size().0)
                        + f64::from(layout(&cr, &unit_font, unit, false).pixel_size().0);
                    assert!(
                        width <= pitch - STAT_GAP,
                        "{value} must fit the same column at {scale}x"
                    );
                }
            }
        }
    }
    #[test]
    fn telemetry_preserves_the_original_value_and_unit() {
        assert_eq!(parts("48°C"), ("48", "°C"));
        assert_eq!(parts("100%"), ("100", "%"));
        assert_eq!(parts("…"), ("…", ""));
        assert_eq!(parts("0%"), ("0", "%"));
        assert_eq!(parts("-5°C"), ("-5", "°C"));
    }
    #[test]
    fn single_digit_stat_sits_next_to_its_icon_without_an_underlined_gauge() {
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 100, 34).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            stat(
                &cr,
                "Sans 11",
                "cpu",
                &Metric {
                    text: "icon 7%".into(),
                    ..Default::default()
                },
                0.0,
            );
        }
        let stride = image.stride() as usize;
        let bytes = image.data().unwrap();
        assert!(
            (8..26).any(|y| (22..30).any(|x| bytes[y * stride + x * 4 + 3] > 0)),
            "do not reserve empty digit slots between the icon and its value"
        );
        assert!(
            (27..34).all(|y| (0..100).all(|x| bytes[y * stride + x * 4 + 3] == 0)),
            "keep the baseline clear instead of drawing miniature gauges"
        );
    }
    #[test]
    fn clock_digits_have_equal_advances_and_keep_the_system_family() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 400, 120).unwrap();
        let cr = Context::new(&image).unwrap();
        for font in ["Sans Medium 58px", "Pretendard JP Medium 58px"] {
            assert_eq!(
                clock_width(&cr, font, "00:00"),
                clock_width(&cr, font, "11:11")
            );
            assert_eq!(
                clock_width(&cr, font, "11:11"),
                clock_width(&cr, font, "23:59")
            );
            let (number, unit) = fonts(font);
            let number_desc = pango::FontDescription::from_string(&number);
            let unit_desc = pango::FontDescription::from_string(&unit);
            assert_eq!(number_desc.is_size_absolute(), unit_desc.is_size_absolute());
            assert!(
                (f64::from(unit_desc.size()) / f64::from(number_desc.size()) - 0.85).abs() < 0.001
            );
            assert_eq!(
                pango::FontDescription::from_string(&number).family(),
                pango::FontDescription::from_string(&unit).family()
            );
        }
    }
}
