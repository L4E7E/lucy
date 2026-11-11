//! Display controls use the same glass, fonts and scroll geometry as Network.
use super::*;
use crate::display::{self, Menu, Request};

const TOP: f64 = 112.0;
fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
}

fn temperature_gauge(cr: &Context, rect: Rect, value: f64) {
    let _ = cr.save();
    round(cr, rect, 18.0);
    cr.clip();
    cr.set_source_rgba(0.95, 0.96, 0.97, 0.13);
    let _ = cr.paint();
    let w = rect.w * value.clamp(0.0, 100.0) / 100.0;
    if w > 0.0 {
        // Anchor the spectrum to the full range, not the current fill width.
        let gradient = cairo::LinearGradient::new(rect.x, rect.y, rect.x + rect.w, rect.y);
        gradient.add_color_stop_rgb(0.0, 1.0, 0.55, 0.24);
        gradient.add_color_stop_rgb(1.0, 1.0, 1.0, 1.0);
        round(cr, Rect { w, ..rect }, 18.0);
        let _ = cr.set_source(&gradient);
        let _ = cr.fill();
    }
    let _ = cr.restore();
}

impl Paint<'_> {
    fn display_select(&mut self, rect: Rect, value: &str, menu: Menu, active: bool) {
        crystal(self.cr, rect, 11.0, self.display_hover(rect) || active);
        // Center the value itself; the independent arrow must not shift it left.
        text(
            self.cr,
            &self.m.font,
            value,
            rect,
            false,
            false,
            0.0,
            pango::Alignment::Center,
        );
        let (x, y) = (rect.x + rect.w - 11.0, rect.y + rect.h / 2.0);
        self.cr.new_path();
        self.cr.move_to(x - 2.5, y - 1.5);
        self.cr.line_to(x, y + 1.5);
        self.cr.line_to(x + 2.5, y - 1.5);
        color(self.cr, WHITE, 0.85);
        self.cr.set_line_width(1.4);
        self.cr.set_line_cap(cairo::LineCap::Round);
        self.cr.set_line_join(cairo::LineJoin::Round);
        let _ = self.cr.stroke();
        self.hit(rect, Action::DisplayMenu(Some(menu)), "");
    }
    fn display_hover(&self, rect: Rect) -> bool {
        pointer(self.m).is_some_and(|(id, x, y)| {
            id == self.id && rect.contains(x, y) && self.clip.is_none_or(|clip| clip.contains(x, y))
        })
    }
    fn display_text(&self, value: &str, rect: Rect, size: f64, bold: bool, dim: bool) {
        text_ink(
            self.cr,
            &self.m.font,
            value,
            rect,
            bold,
            size,
            pango::Alignment::Left,
            if dim { (0.68, 0.71, 0.75) } else { WHITE },
        );
    }
    fn display_switch(&mut self, y: f64, title: &str, on: bool, action: Action, enabled: bool) {
        crystal(self.cr, r(20.0, y, 432.0, 52.0), 16.0, false);
        self.display_text(title, r(34.0, y + 11.0, 320.0, 30.0), 14.0, false, !enabled);
        let toggle = r(390.0, y + 14.0, 44.0, 24.0);
        round(self.cr, toggle, 12.0);
        color(
            self.cr,
            if on { WIFI_GREEN } else { (0.60, 0.63, 0.65) },
            if enabled {
                if on {
                    0.85
                } else {
                    0.30
                }
            } else {
                0.15
            },
        );
        let _ = self.cr.fill();
        self.cr.arc(
            toggle.x + if on { 32.0 } else { 12.0 },
            toggle.y + 12.0,
            9.0,
            0.0,
            2.0 * PI,
        );
        color(self.cr, WHITE, if enabled { 1.0 } else { 0.35 });
        let _ = self.cr.fill();
        if enabled {
            self.hit(toggle, action, title)
        }
    }
    fn display_gauge(&mut self, y: f64, value: f64, label: &str, target: Slider, enabled: bool) {
        let rect = r(32.0, y, 408.0, 36.0);
        let value = if enabled {
            self.m.display_gauge_value(&target, value)
        } else {
            0.0
        };
        if matches!(target, Slider::Temperature) {
            temperature_gauge(self.cr, rect, value);
        } else {
            gauge(self.cr, rect, value);
        }
        let icon = r(42.0, y + 6.0, 24.0, 24.0);
        gauge_icon(self.cr, rect, icon, value, |cr| {
            brightness_icon(cr, icon, value)
        });
        gauge_icon(self.cr, rect, r(325.0, y, 100.0, 36.0), value, |cr| {
            text(
                cr,
                &self.m.font,
                label,
                r(325.0, y, 100.0, 36.0),
                false,
                true,
                14.0,
                pango::Alignment::Right,
            );
        });
        if enabled {
            self.hit(rect, Action::Slider(target), "")
        }
    }
    fn display_canvas(&mut self, mut y: f64) -> f64 {
        let outputs = &self.m.display.outputs;
        let h = if outputs.len() > 1 { 216.0 } else { 122.0 };
        crystal(self.cr, r(20.0, y, 432.0, h), 18.0, false);
        self.display_text(
            "디스플레이 배치",
            r(34.0, y + 10.0, 230.0, 28.0),
            14.0,
            true,
            false,
        );
        self.button(
            r(378.0, y + 10.0, 60.0, 28.0),
            "식별",
            Action::DisplayIdentify,
            false,
            "",
        );
        // Reserve both horizontal and vertical arrangements at one fixed scale.
        // Dragging, edge previews and dropping must never resize monitor cards.
        let position = |o: &display::Output| {
            self.m
                .display_layout
                .iter()
                .find(|p| p.identity == o.identity)
                .map_or((o.x, o.y), |p| (p.x, p.y))
        };
        let minx = outputs.iter().map(|o| position(o).0).min().unwrap_or(0) as f64;
        let miny = outputs.iter().map(|o| position(o).1).min().unwrap_or(0) as f64;
        let maxx = outputs
            .iter()
            .map(|o| i64::from(position(o).0) + i64::from(o.logical_width))
            .max()
            .unwrap_or(1) as f64;
        let maxy = outputs
            .iter()
            .map(|o| i64::from(position(o).1) + i64::from(o.logical_height))
            .max()
            .unwrap_or(1) as f64;
        let total_width = outputs
            .iter()
            .map(|o| f64::from(o.logical_width))
            .sum::<f64>();
        let total_height = outputs
            .iter()
            .map(|o| f64::from(o.logical_height))
            .sum::<f64>();
        let factor = (340.0 / total_width.max(1.0)).min((h - 58.0) / total_height.max(1.0)) * 0.85;
        let cx = 236.0 - (maxx + minx) * 0.5 * factor;
        let cy = y + 42.0 + (h - 50.0) * 0.5 - (maxy + miny) * 0.5 * factor;
        let _ = self.cr.save();
        round(self.cr, r(28.0, y + 40.0, 416.0, h - 44.0), 12.0);
        self.cr.clip();
        let preview = self.m.display_snap_preview();
        let projected = |o: &display::Output| {
            preview
                .and_then(|draft| draft.iter().find(|p| p.identity == o.identity))
                .map_or_else(|| position(o), |p| (p.x, p.y))
        };
        let px0 = outputs.iter().map(|o| projected(o).0).min().unwrap_or(0) as f64;
        let py0 = outputs.iter().map(|o| projected(o).1).min().unwrap_or(0) as f64;
        let px1 = outputs
            .iter()
            .map(|o| f64::from(projected(o).0) + f64::from(o.logical_width))
            .fold(px0, f64::max);
        let py1 = outputs
            .iter()
            .map(|o| f64::from(projected(o).1) + f64::from(o.logical_height))
            .fold(py0, f64::max);
        let preview_cx = 236.0 - (px0 + px1) * 0.5 * factor;
        let preview_cy = y + 42.0 + (h - 50.0) * 0.5 - (py0 + py1) * 0.5 * factor;
        let duration = std::time::Duration::from_millis(180);
        for o in outputs {
            let target = self.m.display_drop_target(&o.name);
            let amount = self.m.motion_value(
                &format!("display-snap-{}", o.name),
                if target { 1.0 } else { 0.0 },
                duration,
            );
            if amount > 0.001 {
                let (x, oy) = projected(o);
                let inset = (1.0 - amount) * 6.0;
                let ghost = r(
                    preview_cx + f64::from(x) * factor + inset,
                    preview_cy + f64::from(oy) * factor + inset,
                    f64::from(o.logical_width) * factor - inset * 2.0,
                    f64::from(o.logical_height) * factor - inset * 2.0,
                );
                round(self.cr, ghost, 8.0);
                color(self.cr, ACCENT, 0.16 * amount);
                let _ = self.cr.fill_preserve();
                color(self.cr, ACCENT, 0.85 * amount);
                self.cr.set_line_width(1.5);
                let _ = self.cr.stroke();
            }
        }
        // The grabbed card follows the pointer above the independent snap ghost.
        for floating_pass in [false, true] {
            for (i, o) in outputs.iter().enumerate() {
                let floating = self
                    .m
                    .display_floating()
                    .filter(|(name, _, _)| *name == o.name);
                if floating.is_some() != floating_pass {
                    continue;
                }
                let (dx, dy) = projected(o);
                let screen_x = self.m.motion_value(
                    &format!("display-card-{}-x", o.name),
                    preview_cx + f64::from(dx) * factor,
                    duration,
                );
                let screen_y = self.m.motion_value(
                    &format!("display-card-{}-y", o.name),
                    preview_cy - y + f64::from(dy) * factor,
                    duration,
                ) + y;
                let (screen_x, screen_y) = floating.map_or((screen_x, screen_y), |(_, x, oy)| {
                    (cx + f64::from(x) * factor, cy + f64::from(oy) * factor)
                });
                let rect = r(
                    screen_x,
                    screen_y,
                    f64::from(o.logical_width) * factor,
                    f64::from(o.logical_height) * factor,
                );
                crystal(
                    self.cr,
                    rect,
                    8.0,
                    floating.is_some() || self.display_hover(rect),
                );
                text(
                    self.cr,
                    &self.m.font,
                    &(i + 1).to_string(),
                    rect,
                    false,
                    true,
                    22.0,
                    pango::Alignment::Center,
                );
                if outputs.len() > 1 && self.m.display.pending.is_none() {
                    self.hit(
                        rect,
                        Action::DisplayDrag(o.name.clone(), factor),
                        "드래그하여 화면 배치",
                    )
                } else {
                    self.hit(rect, Action::DisplayMonitor, "");
                }
            }
        }
        let _ = self.cr.restore();
        y += h + 12.0;
        if self.m.display_layout_changed() {
            self.button(
                r(234.0, y, 96.0, 32.0),
                "초기화",
                Action::DisplayResetLayout,
                false,
                "",
            );
            if display::validate_placements(outputs, &self.m.display_layout).is_ok() {
                self.button(
                    r(338.0, y, 114.0, 32.0),
                    "배치 적용",
                    Action::DisplayApplyLayout,
                    true,
                    "",
                );
            }
            y += 44.0;
        }
        y
    }
    fn display_output(&mut self, mut y: f64, index: usize, o: &display::Output) -> f64 {
        let menu = self.m.display_menu.as_ref();
        let resolution = menu == Some(&Menu::Resolution(o.name.clone()));
        let rate = menu == Some(&Menu::Rate(o.name.clone()));
        let scale = menu == Some(&Menu::Scale(o.name.clone()));
        let mut choices = vec![];
        if resolution {
            for (w, h) in o.resolutions() {
                if let Some(mode) = o.resolution_mode(w, h) {
                    choices.push((
                        format!("{w} × {h}"),
                        Action::Display(Request::Mode(o.name.clone(), mode)),
                        o.current.width == w && o.current.height == h,
                    ));
                }
            }
        }
        if rate {
            let mut modes: Vec<_> = o
                .modes
                .iter()
                .filter(|m| m.width == o.current.width && m.height == o.current.height)
                .cloned()
                .collect();
            modes.sort_by_key(|m| std::cmp::Reverse(m.refresh));
            modes.dedup();
            for mode in modes {
                let selected = mode == o.current;
                choices.push((
                    mode.rate(),
                    Action::Display(Request::Mode(o.name.clone(), mode)),
                    selected,
                ));
            }
        }
        if scale {
            let mut scales = display::SCALES.to_vec();
            let current = (o.scale * 100.0).round() as u32;
            scales.push(current);
            scales.sort_unstable();
            scales.dedup();
            for percent in scales {
                choices.push((
                    format!("{percent}%"),
                    Action::Display(Request::Scale(o.name.clone(), percent)),
                    percent == current,
                ));
            }
        }
        let extra = if choices.is_empty() {
            0.0
        } else {
            choices.len() as f64 * 34.0 + 12.0
        };
        // Unsupported brightness draws the same disabled gauge, with no extra row.
        let h = 194.0 + extra;
        let bottom = y + h;
        crystal(self.cr, r(20.0, y, 432.0, h), 18.0, false);
        text(
            self.cr,
            &self.m.font,
            &(index + 1).to_string(),
            r(32.0, y + 10.0, 30.0, 30.0),
            false,
            true,
            20.0,
            pango::Alignment::Center,
        );
        self.display_text(&o.label, r(72.0, y + 8.0, 350.0, 26.0), 15.0, true, false);
        self.display_text(&o.name, r(72.0, y + 33.0, 350.0, 20.0), 14.0, false, true);
        for (i, (title, value, menu, active)) in [
            (
                "해상도",
                format!("{} × {}", o.current.width, o.current.height),
                Menu::Resolution(o.name.clone()),
                resolution,
            ),
            ("주사율", o.current.rate(), Menu::Rate(o.name.clone()), rate),
            (
                "배율",
                format!("{:.0}%", o.scale * 100.0),
                Menu::Scale(o.name.clone()),
                scale,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let x = 32.0 + i as f64 * 140.0;
            text_ink(
                self.cr,
                &self.m.font,
                title,
                r(x, y + 64.0, 128.0, 22.0),
                false,
                12.0,
                pango::Alignment::Center,
                (0.68, 0.71, 0.75),
            );
            let rect = r(x, y + 90.0, 128.0, 34.0);
            self.display_select(rect, &value, menu, active);
        }
        y += 140.0;
        if !choices.is_empty() {
            y = self.display_choices(y, &choices);
        }
        self.display_gauge(
            y,
            o.brightness.unwrap_or(0.0),
            &o.brightness.map_or("—".into(), |v| format!("{v:.0}%")),
            Slider::DisplayBrightness(o.name.clone()),
            o.control.is_some(),
        );
        // Include the entire card (also without brightness control) and its gap
        // in the scroll extent so the last rim stays above the viewport edge.
        bottom + 12.0
    }
    fn display_choices(&mut self, mut y: f64, choices: &[(String, Action, bool)]) -> f64 {
        for (title, action, selected) in choices {
            self.button(
                r(40.0, y, 392.0, 30.0),
                title,
                action.clone(),
                *selected,
                "",
            );
            y += 34.0;
        }
        y + 12.0
    }
    fn night_page(&mut self, mut y: f64) -> f64 {
        let n = self.m.display.settings.night.clone();
        let supported = self.m.gamma_supported;
        self.display_switch(
            y,
            "야간 모드",
            n.enabled,
            Action::Display(Request::Night(!n.enabled)),
            supported,
        );
        y += 60.0;
        self.display_switch(
            y,
            "시간에 따라 자동 적용",
            n.automatic,
            Action::Display(Request::Automatic(!n.automatic)),
            supported,
        );
        y += 60.0;
        if n.automatic {
            crystal(self.cr, r(20.0, y, 432.0, 100.0), 16.0, false);
            for (i, start, title, time) in [(0, true, "시작", n.start), (1, false, "종료", n.end)]
            {
                let row = y + 10.0 + f64::from(i) * 46.0;
                self.display_text(title, r(34.0, row, 140.0, 34.0), 13.0, false, true);
                let clock = display::Night::time(time);
                let (hour, minute) = clock.split_once(':').unwrap();
                for (x, value, part) in [
                    (278.0, hour, display::ClockPart::Hour),
                    (358.0, minute, display::ClockPart::Minute),
                ] {
                    let rect = r(x, row, 64.0, 34.0);
                    crystal(self.cr, rect, 11.0, supported && self.display_hover(rect));
                    text(
                        self.cr,
                        &self.m.font,
                        value,
                        rect,
                        false,
                        true,
                        16.0,
                        pango::Alignment::Center,
                    );
                    if supported {
                        self.hit(rect, Action::DisplayTime(start, part), "");
                    }
                }
                text(
                    self.cr,
                    &self.m.font,
                    ":",
                    r(342.0, row, 16.0, 34.0),
                    false,
                    true,
                    16.0,
                    pango::Alignment::Center,
                );
            }
            y += 108.0;
        }
        crystal(self.cr, r(20.0, y, 432.0, 110.0), 18.0, false);
        self.display_text("색온도", r(34.0, y + 10.0, 360.0, 26.0), 14.0, true, false);
        self.display_gauge(
            y + 44.0,
            n.percent(),
            &format!("{} K", n.kelvin),
            Slider::Temperature,
            supported,
        );
        for (title, align) in [
            ("따뜻하게", pango::Alignment::Left),
            ("차갑게", pango::Alignment::Right),
        ] {
            text_ink(
                self.cr,
                &self.m.font,
                title,
                r(34.0, y + 82.0, 404.0, 20.0),
                false,
                11.0,
                align,
                (0.68, 0.71, 0.75),
            );
        }
        y += 118.0;
        let error = if !supported {
            Some("이 환경은 야간 모드 색온도 제어를 지원하지 않습니다")
        } else if !self.m.gamma_error.is_empty() {
            Some(self.m.gamma_error.as_str())
        } else {
            None
        };
        if let Some(error) = error {
            self.display_text(error, r(30.0, y, 410.0, 42.0), 12.0, false, true);
            y += 48.0;
        }
        y
    }
    pub(super) fn display_page(&mut self, height: i32) -> (f64, f64) {
        crystal(self.cr, r(20.0, 64.0, 432.0, 40.0), 14.0, false);
        let selected = self
            .m
            .icon_value("display-tab", if self.m.display_night { 1.0 } else { 0.0 });
        crystal(
            self.cr,
            r(24.0 + 212.0 * selected, 68.0, 212.0, 32.0),
            11.0,
            true,
        );
        for (i, title) in ["화면", "야간 모드"].iter().enumerate() {
            let rect = r(24.0 + 212.0 * i as f64, 68.0, 212.0, 32.0);
            text(
                self.cr,
                &self.m.font,
                title,
                rect,
                false,
                self.m.display_night == (i == 1),
                14.0,
                pango::Alignment::Center,
            );
            self.hit(rect, Action::DisplayTab(i == 1), "");
        }
        let mut top = TOP;
        if let Some(pending) = &self.m.display.pending {
            crystal(self.cr, r(20.0, top, 432.0, 92.0), 16.0, true);
            self.display_text(
                &format!("이 설정을 유지할까요?  {}초 후 복원", pending.remaining()),
                r(34.0, top + 8.0, 398.0, 28.0),
                14.0,
                true,
                false,
            );
            self.button(
                r(206.0, top + 46.0, 104.0, 32.0),
                "되돌리기",
                Action::Display(Request::Revert),
                false,
                "",
            );
            self.button(
                r(322.0, top + 46.0, 112.0, 32.0),
                "유지",
                Action::Display(Request::Keep),
                true,
                "",
            );
            top += 104.0;
        }
        if !self.m.display.error.is_empty() {
            self.display_text(
                &self.m.display.error,
                r(30.0, top, 410.0, 48.0),
                12.0,
                false,
                true,
            );
            top += 56.0;
        }
        let viewport = (f64::from(height) - top - 16.0).max(1.0);
        self.clip = Some(r(16.0, top, 440.0, viewport));
        let _ = self.cr.save();
        self.cr.rectangle(16.0, top, 440.0, viewport);
        self.cr.clip();
        let mut y = top - self.m.scroll;
        if self.m.display_night {
            y = self.night_page(y);
        } else if !self.m.display.ready {
            spinner(self.cr, 236.0, y + 40.0);
            y += 90.0;
        } else if self.m.display.outputs.is_empty() {
            self.display_text(
                "활성 디스플레이가 없습니다",
                r(30.0, y, 400.0, 40.0),
                14.0,
                false,
                true,
            );
            y += 52.0;
        } else {
            y = self.display_canvas(y);
            for (i, output) in self.m.display.outputs.iter().enumerate() {
                y = self.display_output(y, i, output);
            }
        }
        let _ = self.cr.restore();
        self.clip = None;
        (y + self.m.scroll - top, viewport)
    }
}

pub fn identify(cr: &Context, m: &Model, index: usize) -> Scene {
    let rect = r(0.0, 0.0, 220.0, 140.0);
    round(cr, rect, 24.0);
    cr.set_source_rgba(0.07, 0.08, 0.10, 0.9);
    let _ = cr.fill();
    crystal(cr, rect, 24.0, false);
    text(
        cr,
        &m.font,
        &(index + 1).to_string(),
        r(0.0, 14.0, 220.0, 88.0),
        false,
        true,
        64.0,
        pango::Alignment::Center,
    );
    if let Some(o) = m.display.outputs.get(index) {
        text(
            cr,
            &m.font,
            &o.name,
            r(12.0, 106.0, 196.0, 24.0),
            false,
            false,
            14.0,
            pango::Alignment::Center,
        );
    }
    Scene {
        hits: vec![],
        content_height: 0.0,
        viewport_height: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bottom_scroll_preserves_last_card_rim_and_twelve_pixel_gap() {
        for count in [1, 2, 3] {
            for controls in 0..1 << count {
                let name = outputs_model(count, controls).display.outputs[count - 1]
                    .name
                    .clone();
                for menu in [
                    None,
                    Some(Menu::Resolution(name.clone())),
                    Some(Menu::Rate(name.clone())),
                    Some(Menu::Scale(name.clone())),
                ] {
                    let mut m = outputs_model(count, controls);
                    m.display_menu = menu.clone();
                    for height in [320, 400] {
                        m.scroll = 0.0;
                        let mut image =
                            cairo::ImageSurface::create(cairo::Format::ARgb32, 480, height)
                                .unwrap();
                        {
                            let cr = Context::new(&image).unwrap();
                            let scene = popup(&cr, &m, 1, height);
                            m.scroll = scene.content_height - scene.viewport_height;
                            assert!(m.scroll > 0.0);
                            cr.set_operator(cairo::Operator::Clear);
                            let _ = cr.paint();
                            cr.set_operator(cairo::Operator::Over);
                            let mut p = Paint {
                                cr: &cr,
                                m: &m,
                                id: 1,
                                hits: vec![],
                                clip: None,
                            };
                            let (content, viewport) = p.display_page(height);
                            assert_eq!(content, scene.content_height);
                            assert_eq!(viewport, scene.viewport_height);
                        }
                        image.flush();
                        let stride = image.stride() as usize;
                        let pixels = image.data().unwrap();
                        let bottom = (height - 16) as usize;
                        let last_ink = (TOP as usize..bottom)
                            .rfind(|y| pixels[y * stride + 236 * 4 + 3] > 0)
                            .unwrap();
                        assert_eq!(
                            bottom - last_ink - 1,
                            12,
                            "controls={controls:b}, menu={menu:?}, outputs={count}, height={height}"
                        );
                        for y in bottom - 12..bottom {
                            assert!((20..452).all(|x| pixels[y * stride + x * 4 + 3] == 0));
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn detail_cards_have_uniform_base_height_for_one_two_three_and_more_outputs() {
        for count in [1, 2, 3, 4] {
            for controls in 0..1 << count {
                let mut m = outputs_model(count, controls);
                let original = m.display.outputs.clone();
                for (index, o) in original.iter().enumerate() {
                    for (menu, extra) in [
                        (None, 0.0),
                        (Some(Menu::Resolution(o.name.clone())), 46.0),
                        (Some(Menu::Rate(o.name.clone())), 46.0),
                        (Some(Menu::Scale(o.name.clone())), 386.0),
                    ] {
                        m.display_menu = menu;
                        let mut image =
                            cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 620).unwrap();
                        {
                            let cr = Context::new(&image).unwrap();
                            let mut p = Paint {
                                cr: &cr,
                                m: &m,
                                id: 1,
                                hits: vec![],
                                clip: None,
                            };
                            let bottom = p.display_output(8.0, index, o);
                            assert_eq!(bottom, 8.0 + 194.0 + extra + 12.0);
                            let gauge = p.hits.iter().find(|hit| {
                                matches!(&hit.action, Action::Slider(Slider::DisplayBrightness(name)) if name == &o.name)
                            });
                            assert_eq!(gauge.is_some(), o.control.is_some());
                            if let Some(gauge) = gauge {
                                assert_eq!(gauge.rect.y, 8.0 + 140.0 + extra);
                                assert_eq!(bottom - 12.0 - gauge.rect.y - gauge.rect.h, 18.0);
                            }
                        }
                        image.flush();
                        let stride = image.stride() as usize;
                        let data = image.data().unwrap();
                        let ink: Vec<_> = (0..620)
                            .filter(|y| data[y * stride + 236 * 4 + 3] > 0)
                            .collect();
                        assert_eq!(ink[0], 8);
                        assert_eq!(*ink.last().unwrap(), (8.0 + 194.0 + extra) as usize - 1);
                    }
                }
                assert_eq!(
                    m.display.outputs, original,
                    "drawing changed actual output settings"
                );
            }
        }
    }
    #[test]
    fn temperature_gradient_is_smooth_and_brightness_remains_neutral() {
        let _glass = GlassScope::enter();
        let render = |target, value, enabled| {
            let m = model();
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 52).unwrap();
            let hits = {
                let cr = Context::new(&image).unwrap();
                let mut p = Paint {
                    cr: &cr,
                    m: &m,
                    id: 1,
                    hits: vec![],
                    clip: None,
                };
                p.display_gauge(8.0, value, "", target, enabled);
                p.hits
            };
            image.flush();
            let pixels = image.data().unwrap().to_vec();
            (pixels, hits)
        };
        let rgb = |pixels: &[u8], x: usize| {
            let i = (26 * 480 + x) * 4;
            let pixel = u32::from_ne_bytes(pixels[i..i + 4].try_into().unwrap());
            [(pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255]
        };
        let (full, _) = render(Slider::Temperature, 100.0, true);
        let warm = rgb(&full, 36);
        let neutral = rgb(&full, 438);
        assert!(warm[0] > warm[1] + 100 && warm[1] > warm[2] + 60);
        assert!(neutral.iter().all(|v| *v >= 254));
        for x in 76..318 {
            let a = rgb(&full, x);
            let b = rgb(&full, x + 1);
            assert_eq!(a[0], 255);
            for c in [1, 2] {
                assert!(b[c] >= a[c] && b[c] - a[c] <= 1);
            }
        }
        for value in [-10.0, 0.0, 1.0, 5.0, 25.0, 50.0, 100.0, 150.0] {
            let (temperature, hits) = render(Slider::Temperature, value, true);
            let (brightness, _) = render(Slider::DisplayBrightness("test".into()), value, true);
            assert_eq!(hits.len(), 1);
            assert!(matches!(
                hits[0].action,
                Action::Slider(Slider::Temperature)
            ));
            assert_eq!((hits[0].rect.x, hits[0].rect.w), (32.0, 408.0));
            // Recoloring must leave fill extent, rounded caps and alpha unchanged.
            for (a, b) in temperature.chunks_exact(4).zip(brightness.chunks_exact(4)) {
                assert_eq!(a[3], b[3], "alpha changed at {value}%");
            }
            if value >= 25.0 {
                assert_eq!(rgb(&temperature, 90), rgb(&full, 90));
                assert_eq!(rgb(&brightness, 90), [255, 255, 255]);
            }
        }
        let (disabled, hits) = render(Slider::Temperature, 80.0, false);
        assert!(hits.is_empty());
        assert_eq!(disabled, render(Slider::Temperature, 0.0, true).0);
        for (percent, kelvin) in [(0.0, 2500), (50.0, 4500), (100.0, 6500)] {
            let mut m = model();
            m.slider(&Slider::Temperature, percent, true);
            assert_eq!(m.display.settings.night.kelvin, kelvin);
        }
    }
    #[test]
    fn scrolling_moves_monitor_cards_with_canvas_without_starting_layout_motion() {
        let mut m = model();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&surface).unwrap();
        let scene = super::super::popup(&cr, &m, 1, 500);
        let initial = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::DisplayMonitor))
            .unwrap()
            .rect;
        for scroll in [20.0, 35.0, 0.0] {
            m.scroll = scroll;
            let scene = super::super::popup(&cr, &m, 1, 500);
            let card = scene
                .hits
                .iter()
                .find(|h| matches!(h.action, Action::DisplayMonitor))
                .unwrap()
                .rect;
            assert!((card.y - initial.y + scroll).abs() < 0.000001);
            assert!(
                !m.animating(),
                "scroll must not animate card positions separately"
            );
        }
    }
    #[test]
    fn edge_preview_is_separate_from_draft_and_cards_never_resize() {
        let mut m = model();
        let mut external = m.display.outputs[0].clone();
        external.name = "DP-3".into();
        external.identity = "external-test".into();
        external.x = 1706;
        external.logical_width = 1920;
        external.logical_height = 1080;
        m.display.outputs.push(external);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&surface).unwrap();
        let scene = super::super::popup(&cr, &m, 1, 500);
        let hit = scene
            .hits
            .iter()
            .find(|h| matches!(&h.action, Action::DisplayDrag(name, _) if name == "DP-3"))
            .unwrap();
        let Action::DisplayDrag(_, factor) = hit.action else {
            unreachable!()
        };
        let size = (hit.rect.w, hit.rect.h);
        m.begin_display_drag("DP-3".into(), factor, 0.0, 0.0);
        m.drag_display((-960.0 - 1706.0) * factor, 0.0);
        assert!(m.display_snap_preview().is_some());
        assert_eq!(
            m.display_layout
                .iter()
                .find(|p| p.name == "eDP-1")
                .unwrap()
                .x,
            0
        );
        let scene = super::super::popup(&cr, &m, 1, 500);
        let hit = scene
            .hits
            .iter()
            .find(|h| matches!(&h.action, Action::DisplayDrag(name, _) if name == "DP-3"))
            .unwrap();
        assert!((hit.rect.w - size.0).abs() < 0.000001);
        assert!((hit.rect.h - size.1).abs() < 0.000001);
        m.end_display_drag();
        assert_eq!(
            m.display_layout
                .iter()
                .find(|p| p.name == "eDP-1")
                .unwrap()
                .x,
            1920
        );
        assert_eq!(
            m.display_layout
                .iter()
                .find(|p| p.name == "DP-3")
                .unwrap()
                .x,
            0
        );
        // Reorienting the layout also keeps the exact same drawing scale.
        m.display_layout[0].x = 0;
        m.display_layout[0].y = 1080;
        m.display_layout[1].x = 0;
        m.display_layout[1].y = 0;
        let scene = super::super::popup(&cr, &m, 1, 500);
        let hit = scene
            .hits
            .iter()
            .find(|h| matches!(&h.action, Action::DisplayDrag(name, _) if name == "DP-3"))
            .unwrap();
        assert!((hit.rect.w - size.0).abs() < 0.000001);
        assert!((hit.rect.h - size.1).abs() < 0.000001);
    }
    #[test]
    fn dragging_into_monitor_center_has_no_snap_preview_and_does_not_commit() {
        let mut m = model();
        let mut external = m.display.outputs[0].clone();
        external.name = "DP-3".into();
        external.identity = "external-test".into();
        external.x = 1706;
        external.logical_width = 1920;
        external.logical_height = 1080;
        m.display.outputs.push(external);
        m.begin_display_drag("DP-3".into(), 0.07, 0.0, 0.0);
        m.drag_display((-107.0 - 1706.0) * 0.07, 0.0);
        assert!(m.display_snap_preview().is_none());
        m.end_display_drag();
        assert!(!m.display_layout_changed());
    }
    #[test]
    fn dropdown_value_is_centered_without_counting_the_arrow() {
        let m = model();
        let render = |value: &str| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 128, 34).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                let mut p = Paint {
                    cr: &cr,
                    m: &m,
                    id: 1,
                    hits: vec![],
                    clip: None,
                };
                p.display_select(
                    r(0.0, 0.0, 128.0, 34.0),
                    value,
                    Menu::Scale("eDP-1".into()),
                    false,
                );
            }
            image.flush();
            let data = image.data().unwrap().to_vec();
            data
        };
        let base = render("");
        let value = render("150%");
        let mut xs = vec![];
        let mut ys = vec![];
        for y in 0..34 {
            for x in 0..128 {
                let i = (y * 128 + x) * 4;
                if (0..4).any(|c| base[i + c].abs_diff(value[i + c]) > 30) {
                    xs.push(x);
                    ys.push(y);
                }
            }
        }
        let centerx = (*xs.iter().min().unwrap() + *xs.iter().max().unwrap()) as f64 / 2.0;
        let centery = (*ys.iter().min().unwrap() + *ys.iter().max().unwrap()) as f64 / 2.0;
        assert!((centerx - 63.5).abs() < 2.0, "value center {centerx}");
        assert!((centery - 16.5).abs() < 3.0, "value center {centery}");
    }
    fn model() -> Model {
        let mut m = Model::new();
        m.readonly = true;
        m.page = Page::Display;
        m.display.ready = true;
        let mode = serde_json::json!({"width":2560,"height":1600,"refresh_rate":165000});
        m.display.outputs = display::parse_outputs(
            &serde_json::json!({"eDP-1":{"make":"BOE","model":"Test","modes":[mode],"current_mode":0,"logical":{"x":0,"y":0,"width":1706,"height":1066,"scale":1.5}}}),
        );
        m.gamma_supported = true;
        m
    }
    fn outputs_model(count: usize, controls: usize) -> Model {
        let mut m = model();
        let internal = m.display.outputs[0].clone();
        m.display.outputs = (0..count)
            .map(|index| {
                let mut o = internal.clone();
                if index > 0 {
                    o.name = format!("DP-{index}");
                    o.identity = format!("external-test-{index}");
                    o.x = 1706 * index as i32;
                }
                if controls & (1 << index) != 0 {
                    o.control = Some(if index == 0 {
                        display::Brightness::Backlight("test".into())
                    } else {
                        display::Brightness::Ddc(index as u32, 100)
                    });
                    o.brightness = Some(50.0);
                } else {
                    o.brightness_error = "밝기 제어 미지원".into();
                }
                o
            })
            .collect();
        m
    }
    #[test]
    fn display_scale_menu_uses_shared_scrollbar_and_keeps_controls_inside_viewport() {
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&s).unwrap();
        let mut m = model();
        m.display_menu = Some(Menu::Scale("eDP-1".into()));
        let scene = super::super::popup(&cr, &m, 1, 500);
        assert!(scene.content_height > scene.viewport_height);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::DisplayIdentify)));
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Scrollbar(_))));
        assert!(scene
            .hits
            .iter()
            .all(|h| h.rect.y >= 0.0 && h.rect.y + h.rect.h <= 500.0));
    }
    #[test]
    fn confirmation_is_pinned_and_night_controls_use_own_slider() {
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&s).unwrap();
        let mut m = model();
        m.scroll = 80.0;
        m.display.pending = Some(display::Pending {
            before: vec![],
            after: vec![],
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(15),
        });
        let scene = super::super::popup(&cr, &m, 1, 500);
        let keep = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Display(Request::Keep)))
            .unwrap();
        assert_eq!(keep.rect.y, 158.0);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(&h.action, Action::DisplayMenu(Some(Menu::Resolution(_))))));
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(&h.action, Action::DisplayMenu(Some(Menu::Rate(_))))));
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(&h.action, Action::DisplayMenu(Some(Menu::Scale(_))))));
        m.display.pending = None;
        m.scroll = 0.0;
        m.display_night = true;
        let scene = super::super::popup(&cr, &m, 1, 500);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Slider(Slider::Temperature))));
        assert!(!scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Slider(Slider::Brightness))));
    }
    #[test]
    fn output_fields_have_equal_size_and_brightness_stays_inside_padded_card() {
        let mut m = model();
        m.display.outputs[0].control = Some(display::Brightness::Backlight("test".into()));
        m.display.outputs[0].brightness = Some(50.0);
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = super::super::popup(&cr, &m, 1, 500);
        let fields: Vec<_> = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::DisplayMenu(_)))
            .collect();
        assert_eq!(fields.len(), 3);
        for h in &fields {
            assert_eq!(h.rect.w, 128.0);
            assert_eq!(h.rect.h, 34.0);
            assert_eq!(h.rect.y, fields[0].rect.y);
        }
        assert_eq!(fields[1].rect.x - fields[0].rect.x, 140.0);
        assert_eq!(fields[2].rect.x - fields[1].rect.x, 140.0);
        let gauge = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Slider(Slider::DisplayBrightness(_))))
            .unwrap();
        // Canvas 122 + gap 12; output card contains the entire gauge + 18 px padding.
        let card_bottom = TOP + 122.0 + 12.0 + 194.0;
        assert_eq!(card_bottom - gauge.rect.y - gauge.rect.h, 18.0);
        assert!(scene.content_height <= scene.viewport_height);
    }
    #[test]
    fn night_schedule_and_captions_fit_joined_panel_without_status_footer() {
        let mut m = model();
        m.display_night = true;
        m.display.settings.night.automatic = true;
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 476).unwrap();
        let cr = Context::new(&s).unwrap();
        let scene = super::super::popup(&cr, &m, 1, 476);
        assert_eq!(scene.content_height, 346.0);
        assert!(scene.content_height <= scene.viewport_height);
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::DisplayTime(_, _)))
                .count(),
            4
        );
        assert!(scene
            .hits
            .iter()
            .all(|h| !matches!(h.action, Action::Display(Request::Time(_, _, _)))));
    }
    #[test]
    fn hovering_resolution_does_not_light_up_monitor_preview() {
        let mut m = model();
        let render = |m: &Model| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
            let scene = {
                let cr = Context::new(&image).unwrap();
                super::super::popup(&cr, m, 1, 500)
            };
            image.flush();
            let data = image.data().unwrap().to_vec();
            (scene, data)
        };
        let (scene, before) = render(&m);
        let (i, monitor) = scene
            .hits
            .iter()
            .enumerate()
            .find(|(_, h)| matches!(h.action, Action::DisplayMonitor))
            .unwrap();
        let rect = monitor.rect;
        let (resolution, button) = scene
            .hits
            .iter()
            .enumerate()
            .find(|(_, h)| matches!(h.action, Action::DisplayMenu(Some(Menu::Resolution(_)))))
            .unwrap();
        m.pointer = Some((1, button.rect.x + 5.0, button.rect.y + 5.0));
        m.hover = Some((1, resolution));
        let (_, resolution) = render(&m);
        let pixels = |data: &[u8]| -> Vec<u8> {
            let mut out = vec![];
            for y in rect.y.ceil() as usize..(rect.y + rect.h).floor() as usize {
                let start = y * 480 * 4 + rect.x.ceil() as usize * 4;
                let end = y * 480 * 4 + (rect.x + rect.w).floor() as usize * 4;
                out.extend_from_slice(&data[start..end]);
            }
            out
        };
        assert_eq!(pixels(&before), pixels(&resolution));
        m.pointer = Some((1, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0));
        m.hover = Some((1, i));
        let (_, hover) = render(&m);
        assert_ne!(pixels(&before), pixels(&hover));
    }
}
