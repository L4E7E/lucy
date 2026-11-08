//! Power controls share Network's segmented glass geometry.
use super::*;
use crate::power::{self, Request, Timer};

pub(crate) const HERO_HEIGHT: f64 = 136.0;
pub(crate) fn menu_height(timer: Timer) -> f64 {
    let count = match timer {
        Timer::Screen => power::SCREEN_MINUTES.len(),
        Timer::Suspend => power::SLEEP_MINUTES.len(),
    };
    count as f64 * 30.0 + 12.0
}
fn group_height(m: &Model) -> f64 {
    let expanded = 52.0
        + 48.0
        + 62.0
        + if m.power.settings.automatic(m.power_ac) {
            48.0
        } else {
            0.0
        }
        + if m.power_ac { 0.0 } else { 62.0 }
        + 124.0
        + if m.power_idle_supported { 6.0 } else { 38.0 };
    42.0 + (expanded - 42.0) * m.power_extra_progress()
}
fn consumption(m: &Model) -> String {
    if let Some(b) = m.power.battery.as_ref().filter(|b| b.state == 2) {
        if let Some(watts) = b.watts {
            return format!("소비 전력 {watts:.1} W");
        }
    }
    m.metrics
        .values
        .get("soc_power")
        .map_or_else(|| "소비 전력 측정 불가".into(), |m| m.text.clone())
}
fn source_label(m: &Model) -> &'static str {
    match m.power.on_battery {
        Some(false) => "전원 연결",
        Some(true) => "배터리 사용",
        None => "전원 상태 확인 중",
    }
}

impl Paint<'_> {
    fn power_profiles(&mut self, r: Rect, selected: Option<&str>, automatic: Option<bool>) {
        crystal(self.cr, r, 14.0, false);
        let width = (r.w - 8.0) / 3.0;
        if let Some(index) = power::PROFILES.iter().position(|p| Some(*p) == selected) {
            let key = match automatic {
                None => "power-mode",
                Some(true) => "power-mode-ac",
                Some(false) => "power-mode-battery",
            };
            let position = self.m.icon_value(key, index as f64);
            crystal(
                self.cr,
                Rect {
                    x: r.x + 4.0 + width * position,
                    y: r.y + 4.0,
                    w: width,
                    h: r.h - 8.0,
                },
                11.0,
                true,
            );
        }
        for (i, (profile, title)) in power::PROFILES
            .into_iter()
            .zip(["절전", "균형", "성능"])
            .enumerate()
        {
            let rect = Rect {
                x: r.x + 4.0 + width * i as f64,
                y: r.y + 4.0,
                w: width,
                h: r.h - 8.0,
            };
            let available = self.m.power.profiles.iter().any(|p| p == profile);
            text_ink(
                self.cr,
                &self.m.font,
                title,
                rect,
                selected == Some(profile),
                0.0,
                pango::Alignment::Center,
                if available { WHITE } else { (0.48, 0.51, 0.55) },
            );
            if available && !self.m.power.busy {
                self.hit(
                    rect,
                    Action::Power(automatic.map_or_else(
                        || Request::Profile(profile.into()),
                        |ac| Request::AutomaticProfile(ac, profile.into()),
                    )),
                    "전원 모드 선택",
                );
            }
        }
    }
    fn power_toggle(
        &mut self,
        y: f64,
        title: &str,
        on: bool,
        action: Action,
        enabled: bool,
        grouped: bool,
    ) {
        let (x, w) = if grouped {
            (32.0, 416.0)
        } else {
            (20.0, 440.0)
        };
        crystal(self.cr, Rect { x, y, w, h: 54.0 }, 16.0, false);
        text_ink(
            self.cr,
            &self.m.font,
            title,
            Rect {
                x: x + 12.0,
                y: y + 12.0,
                w: w - 100.0,
                h: 30.0,
            },
            false,
            14.0,
            pango::Alignment::Left,
            if enabled { WHITE } else { (0.54, 0.56, 0.60) },
        );
        let r = Rect {
            x: x + w - 60.0,
            y: y + 15.0,
            w: 44.0,
            h: 24.0,
        };
        round(self.cr, r, 12.0);
        color(
            self.cr,
            if on { WIFI_GREEN } else { (0.60, 0.63, 0.65) },
            if on { 0.85 } else { 0.30 },
        );
        let _ = self.cr.fill();
        self.cr.new_path();
        self.cr.arc(
            r.x + if on { 32.0 } else { 12.0 },
            r.y + 12.0,
            9.0,
            0.0,
            2.0 * PI,
        );
        self.cr
            .set_source_rgba(0.96, 0.98, 0.97, if enabled { 1.0 } else { 0.5 });
        let _ = self.cr.fill();
        if enabled {
            self.hit(Rect { x, y, w, h: 54.0 }, action, title);
        }
    }
    fn power_time(&mut self, mut y: f64, timer: Timer) -> f64 {
        let ac = self.m.power_ac;
        let times = self.m.power.settings.idle(ac);
        let value = match timer {
            Timer::Screen => times.screen,
            Timer::Suspend => times.suspend,
        };
        let title = match timer {
            Timer::Screen => "화면 꺼짐",
            Timer::Suspend => "자동 절전",
        };
        crystal(
            self.cr,
            Rect {
                x: 32.0,
                y,
                w: 416.0,
                h: 54.0,
            },
            16.0,
            false,
        );
        self.label(
            title,
            Rect {
                x: 44.0,
                y: y + 12.0,
                w: 220.0,
                h: 30.0,
            },
            false,
            false,
        );
        let enabled = self.m.power_idle_supported && !self.m.power.busy;
        let r = Rect {
            x: 284.0,
            y: y + 10.0,
            w: 152.0,
            h: 34.0,
        };
        crystal(self.cr, r, 11.0, self.m.power_picker == Some(timer));
        let label = if value == 0 {
            "사용 안 함".into()
        } else {
            format!("{value}분 후")
        };
        text_ink(
            self.cr,
            &self.m.font,
            &label,
            Rect { w: r.w - 24.0, ..r },
            false,
            13.0,
            pango::Alignment::Center,
            if enabled { WHITE } else { (0.54, 0.56, 0.60) },
        );
        let (x, cy) = (r.x + r.w - 15.0, r.y + r.h / 2.0);
        self.cr.new_path();
        self.cr.move_to(x - 3.5, cy - 1.5);
        self.cr.line_to(x, cy + 2.0);
        self.cr.line_to(x + 3.5, cy - 1.5);
        color(self.cr, WHITE, if enabled { 0.85 } else { 0.4 });
        self.cr.set_line_width(1.5);
        let _ = self.cr.stroke();
        if enabled {
            self.hit(r, Action::PowerPicker(timer), "시간 선택");
        }
        y += 62.0;
        y
    }
    fn power_dropdown(&mut self, height: i32) {
        let Some(timer) = self.m.power_picker else {
            return;
        };
        let Some(anchor) = self
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::PowerPicker(t) if t == timer))
            .map(|h| h.rect)
        else {
            return;
        };
        let r = Rect {
            x: anchor.x,
            y: anchor.y + anchor.h + 6.0,
            w: anchor.w,
            h: menu_height(timer),
        };
        self.hit(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 480.0,
                h: f64::from(height),
            },
            Action::PowerClosePicker,
            "목록 닫기",
        );
        self.hit(r, Action::PowerMenu, "시간 선택 목록");
        round(self.cr, r, 13.0);
        self.cr.set_source_rgba(0.12, 0.15, 0.17, 0.98);
        let _ = self.cr.fill();
        round(self.cr, r, 13.0);
        self.cr.set_source_rgba(0.92, 0.96, 1.0, 0.22);
        self.cr.set_line_width(1.0);
        let _ = self.cr.stroke();
        let times = self.m.power.settings.idle(self.m.power_ac);
        let value = match timer {
            Timer::Screen => times.screen,
            Timer::Suspend => times.suspend,
        };
        let choices: &[u32] = match timer {
            Timer::Screen => &power::SCREEN_MINUTES,
            Timer::Suspend => &power::SLEEP_MINUTES,
        };
        for (i, minutes) in choices.iter().copied().enumerate() {
            let row = Rect {
                x: r.x + 5.0,
                y: r.y + 6.0 + i as f64 * 30.0,
                w: r.w - 10.0,
                h: 30.0,
            };
            let enabled = !self.m.power.busy
                && match timer {
                    Timer::Screen => times.suspend == 0 || minutes <= times.suspend,
                    Timer::Suspend => minutes == 0 || minutes >= times.screen,
                };
            if minutes == value || (enabled && self.hot()) {
                round(self.cr, row, 7.0);
                self.cr.set_source_rgba(
                    0.95,
                    0.97,
                    1.0,
                    if minutes == value { 0.16 } else { 0.09 },
                );
                let _ = self.cr.fill();
            }
            let label = if minutes == 0 {
                "사용 안 함".into()
            } else {
                format!("{minutes}분")
            };
            text_ink(
                self.cr,
                &self.m.font,
                &label,
                Rect {
                    x: row.x + 9.0,
                    w: row.w - 34.0,
                    ..row
                },
                minutes == value,
                13.0,
                pango::Alignment::Left,
                if enabled { WHITE } else { (0.48, 0.51, 0.55) },
            );
            if minutes == value {
                let (x, y) = (row.x + row.w - 16.0, row.y + 15.0);
                self.cr.new_path();
                self.cr.move_to(x - 4.0, y);
                self.cr.line_to(x - 1.0, y + 3.0);
                self.cr.line_to(x + 5.0, y - 3.0);
                color(self.cr, WHITE, 1.0);
                self.cr.set_line_width(1.6);
                let _ = self.cr.stroke();
            }
            if enabled {
                self.hit(
                    row,
                    Action::Power(Request::Timeout(self.m.power_ac, timer, minutes)),
                    "대기 시간 설정",
                );
            }
        }
    }
    pub(super) fn power_page(&mut self, height: i32) -> (f64, f64) {
        let m = self.m;
        self.power_profiles(
            Rect {
                x: 20.0,
                y: 62.0,
                w: 440.0,
                h: 38.0,
            },
            m.common.profile.as_deref(),
            None,
        );
        if m.power.busy {
            spinner(self.cr, 443.0, 33.0);
        }
        let top = 112.0;
        let footer = if m.power.error.is_empty() { 20.0 } else { 76.0 };
        let viewport = (f64::from(height) - footer - top).max(0.0);
        self.clip = Some(Rect {
            x: 16.0,
            y: top - 4.0,
            w: 448.0,
            h: viewport + 4.0,
        });
        let _ = self.cr.save();
        self.cr.rectangle(16.0, top - 4.0, 448.0, viewport + 4.0);
        self.cr.clip();
        let mut y = top - m.scroll;
        crystal(
            self.cr,
            Rect {
                x: 20.0,
                y,
                w: 440.0,
                h: HERO_HEIGHT,
            },
            20.0,
            false,
        );
        text_ink(
            self.cr,
            &m.font,
            "현재 배터리 잔량",
            Rect {
                x: 34.0,
                y: y + 14.0,
                w: 250.0,
                h: 24.0,
            },
            false,
            13.0,
            pango::Alignment::Left,
            (0.72, 0.75, 0.79),
        );
        let percent = m
            .power
            .battery
            .as_ref()
            .and_then(|b| b.percent)
            .map_or_else(|| "—".into(), |p| format!("{p:.0}%"));
        text(
            self.cr,
            &m.font,
            &percent,
            Rect {
                x: 32.0,
                y: y + 38.0,
                w: 224.0,
                h: 62.0,
            },
            false,
            true,
            52.0,
            pango::Alignment::Left,
        );
        let charge = m
            .power
            .battery
            .as_ref()
            .and_then(|b| b.percent)
            .unwrap_or(0.0);
        gauge(
            self.cr,
            Rect {
                x: 34.0,
                y: y + 116.0,
                w: 412.0,
                h: 6.0,
            },
            m.icon_value("battery-charge", charge),
        );
        let title = source_label(m);
        text(
            self.cr,
            &m.font,
            title,
            Rect {
                x: 264.0,
                y: y + 38.0,
                w: 180.0,
                h: 28.0,
            },
            false,
            true,
            14.0,
            pango::Alignment::Left,
        );
        text_ink(
            self.cr,
            &m.font,
            &consumption(m),
            Rect {
                x: 264.0,
                y: y + 66.0,
                w: 180.0,
                h: 24.0,
            },
            false,
            12.0,
            pango::Alignment::Left,
            (0.72, 0.75, 0.79),
        );
        y += HERO_HEIGHT + 12.0;
        crystal(
            self.cr,
            Rect {
                x: 20.0,
                y,
                w: 440.0,
                h: 104.0,
            },
            18.0,
            false,
        );
        self.label(
            "배터리 성능 상태",
            Rect {
                x: 34.0,
                y: y + 10.0,
                w: 390.0,
                h: 24.0,
            },
            false,
            true,
        );
        let health = m.power.battery.as_ref().and_then(|b| b.health);
        let value = m.icon_value("battery-health", health.unwrap_or(0.0));
        let r = Rect {
            x: 34.0,
            y: y + 42.0,
            w: 412.0,
            h: 30.0,
        };
        gauge(self.cr, r, value);
        let label = health.map_or_else(|| "정보 없음".into(), |h| format!("{h:.1}%"));
        let label_rect = Rect {
            x: r.x + 12.0,
            w: r.w - 24.0,
            ..r
        };
        gauge_icon(self.cr, r, label_rect, value, |cr| {
            text(
                cr,
                &m.font,
                &label,
                label_rect,
                false,
                true,
                13.0,
                pango::Alignment::Right,
            )
        });
        let capacity = m
            .power
            .battery
            .as_ref()
            .and_then(|b| b.energy_full.zip(b.energy_design))
            .map_or_else(
                || "설계 용량 대비 현재 최대 충전 용량".into(),
                |(full, design)| format!("최대 {full:.1} Wh  /  설계 {design:.1} Wh"),
            );
        text_ink(
            self.cr,
            &m.font,
            &capacity,
            Rect {
                x: 34.0,
                y: y + 76.0,
                w: 412.0,
                h: 22.0,
            },
            false,
            11.0,
            pango::Alignment::Left,
            (0.66, 0.69, 0.73),
        );
        y += 116.0;
        crystal(
            self.cr,
            Rect {
                x: 20.0,
                y,
                w: 440.0,
                h: 54.0,
            },
            16.0,
            false,
        );
        self.label(
            "충전 상한",
            Rect {
                x: 32.0,
                y: y + 12.0,
                w: 220.0,
                h: 30.0,
            },
            false,
            false,
        );
        if let Some(limit) = m.power.charge_limit {
            crystal(
                self.cr,
                Rect {
                    x: 280.0,
                    y: y + 8.0,
                    w: 164.0,
                    h: 38.0,
                },
                13.0,
                false,
            );
            for (i, value) in [80u8, 100].into_iter().enumerate() {
                let r = Rect {
                    x: 284.0 + i as f64 * 78.0,
                    y: y + 12.0,
                    w: 78.0,
                    h: 30.0,
                };
                if limit == value {
                    crystal(self.cr, r, 10.0, true);
                }
                text(
                    self.cr,
                    &m.font,
                    &format!("{value}%"),
                    r,
                    false,
                    limit == value,
                    13.0,
                    pango::Alignment::Center,
                );
                if !m.power.busy {
                    self.hit(
                        r,
                        Action::Power(Request::ChargeLimit(value)),
                        "충전 상한 선택",
                    );
                }
            }
        } else {
            text_ink(
                self.cr,
                &m.font,
                "지원하지 않음",
                Rect {
                    x: 280.0,
                    y: y + 12.0,
                    w: 164.0,
                    h: 30.0,
                },
                false,
                12.0,
                pango::Alignment::Right,
                (0.54, 0.56, 0.60),
            );
        }
        y += 64.0;
        self.power_toggle(
            y,
            "절전 방지",
            m.power_awake,
            Action::PowerAwake,
            m.power_idle_supported,
            false,
        );
        y += 64.0;
        let disclosure = Rect {
            x: 20.0,
            y,
            w: 440.0,
            h: 42.0,
        };
        let group = Rect {
            h: group_height(m),
            ..disclosure
        };
        crystal(self.cr, group, 18.0, false);
        self.label(
            "자동 절전 설정",
            Rect {
                x: 32.0,
                y: y + 6.0,
                w: 360.0,
                h: 30.0,
            },
            false,
            true,
        );
        let (cx, cy) = (428.0, y + 21.0);
        self.cr.new_path();
        let reveal = m.power_extra_progress();
        let _ = self.cr.save();
        self.cr.translate(cx, cy);
        self.cr.rotate(-PI * reveal);
        self.cr.move_to(-4.0, -2.0);
        self.cr.line_to(0.0, 2.0);
        self.cr.line_to(4.0, -2.0);
        color(self.cr, WHITE, 0.85);
        self.cr.set_line_width(1.6);
        let _ = self.cr.stroke();
        let _ = self.cr.restore();
        self.hit(disclosure, Action::PowerExtra, "추가 절전 설정");
        y += 52.0;
        if reveal > 0.0001 {
            let _ = self.cr.save();
            let body = Rect {
                x: group.x,
                y: group.y + 42.0,
                w: group.w,
                h: (group.h - 42.0).max(0.0),
            };
            self.cr.rectangle(body.x, body.y, body.w, body.h);
            self.cr.clip();
            let previous_clip = self.clip;
            self.clip = Some(if let Some(c) = previous_clip {
                let top = c.y.max(body.y);
                Rect {
                    x: body.x,
                    y: top,
                    w: body.w,
                    h: ((c.y + c.h).min(body.y + body.h) - top).max(0.0),
                }
            } else {
                body
            });
            let hit_start = self.hits.len();
            let ac = m.power_ac;
            crystal(
                self.cr,
                Rect {
                    x: 32.0,
                    y,
                    w: 416.0,
                    h: 38.0,
                },
                14.0,
                false,
            );
            let position = m.icon_value("power-source", if ac { 1.0 } else { 0.0 });
            crystal(
                self.cr,
                Rect {
                    x: 36.0 + position * 204.0,
                    y: y + 4.0,
                    w: 204.0,
                    h: 30.0,
                },
                11.0,
                true,
            );
            for (i, (on_ac, name)) in [(false, "배터리 사용"), (true, "전원 연결")]
                .into_iter()
                .enumerate()
            {
                let r = Rect {
                    x: 36.0 + i as f64 * 204.0,
                    y: y + 4.0,
                    w: 204.0,
                    h: 30.0,
                };
                text(
                    self.cr,
                    &m.font,
                    name,
                    r,
                    false,
                    ac == on_ac,
                    13.0,
                    pango::Alignment::Center,
                );
                self.hit(r, Action::PowerSource(on_ac), "전원 조건 선택");
            }
            y += 48.0;
            let automatic = m.power.settings.automatic(ac);
            self.power_toggle(
                y,
                "전원 모드 자동 전환",
                automatic,
                Action::Power(Request::Automatic(ac, !automatic)),
                !m.power.busy && !m.power.profiles.is_empty(),
                true,
            );
            y += 62.0;
            if automatic {
                self.power_profiles(
                    Rect {
                        x: 32.0,
                        y,
                        w: 416.0,
                        h: 38.0,
                    },
                    Some(m.power.settings.profile(ac)),
                    Some(ac),
                );
                y += 48.0;
            }
            if !ac {
                self.power_toggle(
                    y,
                    "20% 미만일 때 절전 모드",
                    m.power.settings.low_battery,
                    Action::Power(Request::LowBattery(!m.power.settings.low_battery)),
                    !m.power.busy && m.power.profiles.iter().any(|p| p == "power-saver"),
                    true,
                );
                y += 62.0;
            }
            y = self.power_time(y, Timer::Screen);
            y = self.power_time(y, Timer::Suspend);
            if !m.power_idle_supported {
                text_ink(
                    self.cr,
                    &m.font,
                    "이 세션은 Wayland 유휴 알림을 지원하지 않습니다",
                    Rect {
                        x: 32.0,
                        y,
                        w: 416.0,
                        h: 26.0,
                    },
                    false,
                    12.0,
                    pango::Alignment::Left,
                    (0.72, 0.75, 0.79),
                );
            }
            if !m.power_extra {
                self.hits.truncate(hit_start);
            }
            self.clip = previous_clip;
            let _ = self.cr.restore();
        }
        y = disclosure.y + group.h + 10.0;
        for message in [if m.power.degraded.is_empty() {
            ""
        } else {
            "현재 장치 상태로 성능 모드가 제한될 수 있습니다"
        }] {
            if message.is_empty() {
                continue;
            }
            let layout = notification_body_layout(self.cr, &m.font, message, 416.0);
            self.cr.move_to(32.0, y + 2.0);
            self.cr.set_source_rgb(1.0, 0.72, 0.74);
            unsafe {
                pango_cairo_show_layout(self.cr.to_raw_none(), layout.to_glib_none().0);
            }
            y += f64::from(layout.pixel_size().1) + 12.0;
        }
        let content = y + m.scroll - top + m.power_picker.map_or(0.0, menu_height);
        let _ = self.cr.restore();
        self.clip = None;
        if !m.power.error.is_empty() {
            let r = Rect {
                x: 20.0,
                y: f64::from(height) - 64.0,
                w: 440.0,
                h: 48.0,
            };
            crystal(self.cr, r, 14.0, false);
            let layout = notification_body_lines(self.cr, &m.font, &m.power.error, 412.0, 2);
            self.cr.move_to(34.0, r.y + 6.0);
            self.cr.set_source_rgb(1.0, 0.72, 0.74);
            unsafe {
                pango_cairo_show_layout(self.cr.to_raw_none(), layout.to_glib_none().0);
            }
        }
        self.power_dropdown(height);
        (content, viewport)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn battery_hero_only_describes_the_power_source_not_charge_wait_states() {
        use super::*;
        let mut m = Model::new();
        for state in 0..=6 {
            m.power.battery = Some(power::Battery {
                state,
                ..Default::default()
            });
            m.power.on_battery = Some(false);
            assert_eq!(source_label(&m), "전원 연결");
            m.power.on_battery = Some(true);
            assert_eq!(source_label(&m), "배터리 사용");
        }
    }
    use super::*;
    fn model() -> Model {
        let mut m = Model::new();
        m.navigate(Page::Power);
        m.power_idle_supported = true;
        m.power_state(power::State {
            ready: true,
            profile: Some("balanced".into()),
            profiles: power::PROFILES.map(str::to_owned).to_vec(),
            charge_limit: Some(80),
            battery: Some(power::Battery {
                percent: Some(81.0),
                health: Some(94.4),
                energy_full: Some(68.9),
                energy_design: Some(73.0),
                ..power::Battery::default()
            }),
            ..power::State::default()
        });
        m
    }
    #[test]
    fn power_tabs_match_network_and_health_gauge_is_not_a_slider() {
        let m = model();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        let tabs: Vec<_> = scene
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::Power(Request::Profile(_))))
            .collect();
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].rect.x, 24.0);
        assert_eq!(tabs[0].rect.y, 66.0);
        assert_eq!(tabs[0].rect.h, 30.0);
        assert!(!scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Slider(_))));
        let scrollbar = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::Scrollbar(_)))
            .unwrap();
        assert_eq!(scrollbar.rect.y, 112.0);
    }
    #[test]
    fn idle_picker_disables_unsafe_time_order_and_supports_inline_selection() {
        let mut m = model();
        m.power_extra = true;
        m.power_ac = false;
        m.power_picker = Some(Timer::Suspend);
        m.power.settings.battery_idle.screen = 30;
        m.scroll = 500.0;
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        assert!(!scene.hits.iter().any(|h| matches!(
            h.action,
            Action::Power(Request::Timeout(false, Timer::Suspend, 15))
        )));
        assert!(scene.hits.iter().any(|h| matches!(
            h.action,
            Action::Power(Request::Timeout(false, Timer::Suspend, 30))
        )));
        assert!(scene
            .hits
            .iter()
            .filter(|h| !matches!(h.action, Action::Scrollbar(_) | Action::PowerClosePicker))
            .all(|h| h.rect.x + h.rect.w <= 463.0));
    }
    #[test]
    fn consumption_distinguishes_discharge_from_charging_and_chip_power() {
        let mut m = model();
        m.power.battery.as_mut().unwrap().watts = Some(24.0);
        m.power.battery.as_mut().unwrap().state = 1;
        assert_eq!(
            consumption(&m),
            "소비 전력 측정 불가",
            "charging watts are not system consumption"
        );
        m.metrics.values.insert(
            "soc_power",
            crate::bar_backend::Metric {
                text: "소비 전력 12.3 W".into(),
                ..Default::default()
            },
        );
        assert_eq!(consumption(&m), "소비 전력 12.3 W");
        m.power.battery.as_mut().unwrap().state = 2;
        assert_eq!(consumption(&m), "소비 전력 24.0 W");
        assert_eq!(
            HERO_HEIGHT - (116.0 + 6.0),
            14.0,
            "bottom gauge margin must match the 14px top margin"
        );
    }
    #[test]
    fn dropdown_is_anchored_below_its_button_with_one_vertical_column() {
        let mut m = model();
        m.power_extra = true;
        m.power_ac = false;
        m.power_picker = Some(Timer::Screen);
        m.scroll = 500.0;
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&image).unwrap();
        let scene = popup(&cr, &m, 1, HEIGHT);
        let anchor = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::PowerPicker(Timer::Screen)))
            .unwrap()
            .rect;
        let menu = scene
            .hits
            .iter()
            .find(|h| matches!(h.action, Action::PowerMenu))
            .unwrap()
            .rect;
        assert_eq!(
            (menu.x, menu.w, menu.y),
            (anchor.x, anchor.w, anchor.y + anchor.h + 6.0)
        );
        let rows: Vec<_> = scene
            .hits
            .iter()
            .filter(|h| {
                matches!(
                    h.action,
                    Action::Power(Request::Timeout(false, Timer::Screen, _))
                )
            })
            .collect();
        assert_eq!(rows.len(), power::SCREEN_MINUTES.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                (row.rect.x, row.rect.y),
                (menu.x + 5.0, menu.y + 6.0 + i as f64 * 30.0)
            );
        }
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::PowerClosePicker)));
    }
    #[test]
    fn read_only_power_actions_never_reach_system_worker() {
        let mut m = model();
        m.readonly = true;
        let (tx, rx) = std::sync::mpsc::channel();
        m.power_tx = Some(tx);
        for request in [
            Request::ChargeLimit(100),
            Request::Profile("performance".into()),
            Request::LowBattery(true),
            Request::Timeout(false, Timer::Suspend, 15),
        ] {
            m.activate(Action::Power(request));
        }
        m.activate(Action::PowerAwake);
        assert!(!m.power_awake);
        assert!(rx.try_recv().is_err());
    }
}
