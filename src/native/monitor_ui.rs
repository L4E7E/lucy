//! Compact read-only telemetry in the control center's existing glass surface.
use super::*;
use crate::monitor::{Gpu, Usage};
use std::time::Duration;

const TOP: f64 = 62.0;
const THREADS_PER_ROW: usize = 32;
fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
}
fn value(value: Option<f64>, unit: &str) -> String {
    value
        .filter(|n| n.is_finite())
        .map_or_else(|| "—".into(), |n| format!("{n:.0}{unit}"))
}
fn cpu_height(count: usize) -> f64 {
    92.0 + count.max(1).div_ceil(THREADS_PER_ROW).saturating_sub(1) as f64 * 48.0
}
fn gpu_height(count: usize) -> f64 {
    14.0 + count.max(1) as f64 * 78.0
}
impl Paint<'_> {
    #[allow(clippy::too_many_arguments)]
    fn capture_monitor_gauge(
        &self,
        r: Rect,
        key: &str,
        percent: Option<f64>,
        vertical: bool,
        radius: f32,
        alpha: f32,
    ) -> bool {
        GPU_GAUGES.with(|capture| {
            let mut capture = capture.borrow_mut();
            let Some(gauges) = capture.as_mut() else {
                return false;
            };
            if let Some(level) = percent.filter(|n| n.is_finite()) {
                let (x, y) = DRAW_OFFSET.with(|offset| offset.get());
                let translate = |mut r: Rect| {
                    r.x += x;
                    r.y += y;
                    r
                };
                gauges.push(crate::gauge::Gauge {
                    key: key.into(),
                    rect: translate(r),
                    clip: translate(self.clip.unwrap_or(r)),
                    radius,
                    vertical,
                    alpha,
                    level: (level / 100.0).clamp(0.0, 1.0) as f32,
                });
            }
            true
        })
    }
    fn monitor_text(&self, text: &str, r: Rect, size: f64, bold: bool, right: bool, dim: bool) {
        if right {
            // Position numeric text by its actual ink width, not an aligned
            // paragraph's offset; keep the same right inset on mixed-DPI outputs.
            let key = crate::text_cache::Key::new(
                self.cr,
                &self.m.font,
                text,
                31,
                size,
                -1,
                pango::Alignment::Left,
                pango::Weight::Normal,
            );
            let layout = crate::text_cache::layout(key, || {
                let layout = crate::text_cache::create(self.cr);
                let mut font = pango::FontDescription::from_string(&self.m.font);
                font.set_absolute_size(size * f64::from(pango::SCALE));
                layout.set_font_description(Some(&font));
                layout.set_single_paragraph_mode(true);
                layout.set_text(text);
                layout
            });
            let (width, height) = layout.pixel_size();
            let _ = self.cr.save();
            self.cr.rectangle(r.x, r.y, r.w, r.h);
            self.cr.clip();
            color(self.cr, if dim { (0.65, 0.68, 0.72) } else { WHITE }, 1.0);
            self.cr.move_to(
                (r.x + r.w - f64::from(width)).max(r.x),
                r.y + (r.h - f64::from(height)) / 2.0,
            );
            crate::text_cache::show(self.cr, &layout);
            let _ = self.cr.restore();
            return;
        }
        text_ink(
            self.cr,
            &self.m.font,
            text,
            r,
            bold,
            size,
            if right {
                pango::Alignment::Right
            } else {
                pango::Alignment::Left
            },
            if dim { (0.65, 0.68, 0.72) } else { WHITE },
        );
    }
    fn monitor_bar(&self, r: Rect, key: &str, percent: Option<f64>) {
        round(self.cr, r, r.h / 2.0);
        color(self.cr, WHITE, 0.10);
        let _ = self.cr.fill();
        if self.capture_monitor_gauge(r, key, percent, false, (r.h / 2.0) as f32, 0.88) {
            return;
        }
        if let Some(percent) = percent.filter(|p| p.is_finite()) {
            let amount =
                self.m
                    .motion_value(key, percent.clamp(0.0, 100.0), Duration::from_millis(240))
                    / 100.0;
            let _ = self.cr.save();
            round(self.cr, r, r.h / 2.0);
            self.cr.clip();
            // Clip the whole capsule, even below one corner radius; no overflow.
            let fill = rect(r.x, r.y, r.w * amount, r.h);
            round(self.cr, fill, (r.h / 2.0).min(fill.w / 2.0));
            color(self.cr, WHITE, 0.88);
            let _ = self.cr.fill();
            let _ = self.cr.restore();
        }
    }
    fn monitor_cpu(&mut self, y: f64) {
        let state = &self.m.monitor;
        let height = cpu_height(state.threads.len());
        crystal(self.cr, rect(20.0, y, 432.0, height), 18.0, false);
        self.monitor_text(
            "CPU",
            rect(32.0, y + 5.0, 60.0, 20.0),
            13.0,
            true,
            false,
            false,
        );
        let hovered = self
            .m
            .hover
            .and_then(|(id, index)| self.m.hits.get(&id)?.get(index))
            .and_then(|hit| {
                if let Action::MonitorThread(id) = hit.action {
                    state.threads.iter().find(|c| c.id == id)
                } else {
                    None
                }
            });
        let detail = if let Some(cpu) = hovered {
            format!("스레드 {} · {}", cpu.id, value(cpu.usage, "%"))
        } else {
            format!(
                "{} 스레드 · {} · {}",
                state.threads.len(),
                value(state.cpu, "%"),
                value(state.temperature, "°C")
            )
        };
        self.monitor_text(
            &detail,
            rect(96.0, y + 5.0, 344.0, 20.0),
            12.0,
            false,
            true,
            false,
        );
        self.monitor_text(
            &state.cpu_name,
            rect(32.0, y + 25.0, 408.0, 18.0),
            11.0,
            false,
            false,
            true,
        );
        for (index, cpu) in state.threads.iter().enumerate() {
            let row = index / THREADS_PER_ROW;
            let count = (state.threads.len() - row * THREADS_PER_ROW).min(THREADS_PER_ROW);
            let pitch = 408.0 / count as f64;
            let r = rect(
                32.0 + (index % THREADS_PER_ROW) as f64 * pitch + 3.0,
                y + 47.0 + row as f64 * 48.0,
                pitch - 6.0,
                33.0,
            );
            round(self.cr, r, 4.0);
            color(self.cr, WHITE, 0.10);
            let _ = self.cr.fill();
            let alpha = if hovered.is_some_and(|c| c.id == cpu.id) {
                1.0
            } else {
                0.82
            };
            if self.capture_monitor_gauge(
                r,
                &format!("monitor-cpu-{}", cpu.id),
                cpu.usage,
                true,
                4.0,
                alpha,
            ) {
                self.hit(r, Action::MonitorThread(cpu.id), "");
                continue;
            }
            if let Some(usage) = cpu.usage {
                let fraction = self.m.motion_value(
                    &format!("monitor-cpu-{}", cpu.id),
                    usage,
                    Duration::from_millis(240),
                ) / 100.0;
                let _ = self.cr.save();
                round(self.cr, r, 4.0);
                self.cr.clip();
                self.cr
                    .rectangle(r.x, r.y + r.h * (1.0 - fraction), r.w, r.h * fraction);
                color(
                    self.cr,
                    WHITE,
                    if hovered.is_some_and(|c| c.id == cpu.id) {
                        1.0
                    } else {
                        0.82
                    },
                );
                let _ = self.cr.fill();
                let _ = self.cr.restore();
            }
            self.hit(r, Action::MonitorThread(cpu.id), "");
        }
    }
    fn monitor_gpu(&self, gpu: &Gpu, y: f64) {
        self.monitor_text(
            &gpu.name,
            rect(32.0, y, 332.0, 20.0),
            12.0,
            true,
            false,
            false,
        );
        self.monitor_text(
            &value(gpu.temperature, "°C"),
            rect(368.0, y, 72.0, 20.0),
            12.0,
            false,
            true,
            false,
        );
        self.monitor_text(
            "GPU",
            rect(32.0, y + 20.0, 96.0, 16.0),
            11.0,
            false,
            false,
            true,
        );
        self.monitor_bar(
            rect(32.0, y + 37.0, 408.0, 8.0),
            &format!("monitor-gpu-{}", gpu.id),
            gpu.usage,
        );
        let usage = if gpu.sleeping {
            "절전 중".into()
        } else {
            value(gpu.usage, "%")
        };
        self.monitor_text(
            &usage,
            rect(144.0, y + 20.0, 296.0, 16.0),
            12.0,
            false,
            true,
            gpu.sleeping,
        );
        self.monitor_text(
            if gpu.integrated {
                "VRAM · 예약"
            } else {
                "VRAM"
            },
            rect(32.0, y + 47.0, 96.0, 16.0),
            11.0,
            false,
            false,
            true,
        );
        self.monitor_bar(
            rect(32.0, y + 64.0, 408.0, 8.0),
            &format!("monitor-vram-{}", gpu.id),
            gpu.vram.map(Usage::percent),
        );
        self.monitor_text(
            &gpu.vram.map_or_else(|| "—".into(), |u| u.text()),
            rect(144.0, y + 47.0, 296.0, 16.0),
            12.0,
            false,
            true,
            false,
        );
    }
    fn monitor_capacity(&self, y: f64, title: &str, key: &str, usage: Option<Usage>) {
        crystal(self.cr, rect(20.0, y, 432.0, 56.0), 18.0, false);
        self.monitor_text(
            title,
            rect(32.0, y + 6.0, 154.0, 20.0),
            12.0,
            true,
            false,
            false,
        );
        let detail = usage.map_or_else(
            || "—".into(),
            |u| format!("{} · {:.0}%", u.text(), u.percent()),
        );
        self.monitor_text(
            &detail,
            rect(188.0, y + 6.0, 252.0, 20.0),
            12.0,
            false,
            true,
            false,
        );
        self.monitor_bar(
            rect(32.0, y + 33.0, 408.0, 12.0),
            key,
            usage.map(Usage::percent),
        );
    }
    pub(super) fn monitor_page(&mut self, height: i32) -> (f64, f64) {
        let state = &self.m.monitor;
        if !state.ready {
            self.monitor_text(
                "시스템 정보 불러오는 중…",
                rect(32.0, 84.0, 400.0, 24.0),
                13.0,
                false,
                false,
                true,
            );
            return (0.0, 0.0);
        }
        let content = cpu_height(state.threads.len()) + gpu_height(state.gpus.len()) + 136.0;
        let viewport = f64::from(height) - TOP - 16.0;
        let clip = rect(16.0, TOP, 440.0, viewport);
        self.clip = Some(clip);
        let _ = self.cr.save();
        self.cr.rectangle(clip.x, clip.y, clip.w, clip.h);
        self.cr.clip();
        let mut y = TOP - self.m.scroll;
        self.monitor_cpu(y);
        y += cpu_height(state.threads.len()) + 8.0;
        crystal(
            self.cr,
            rect(20.0, y, 432.0, gpu_height(state.gpus.len())),
            18.0,
            false,
        );
        if state.gpus.is_empty() {
            self.monitor_text(
                "GPU 정보 없음",
                rect(32.0, y + 12.0, 408.0, 24.0),
                13.0,
                false,
                false,
                true,
            );
        } else {
            for (index, gpu) in state.gpus.iter().enumerate() {
                self.monitor_gpu(gpu, y + 7.0 + index as f64 * 78.0);
            }
        }
        y += gpu_height(state.gpus.len()) + 8.0;
        self.monitor_capacity(y, "RAM", "monitor-memory", state.memory);
        y += 64.0;
        let title = if state.disk.name.is_empty() {
            state.disk.mount.clone()
        } else {
            format!("{} · {}", state.disk.name, state.disk.mount)
        };
        self.monitor_capacity(y, &title, "monitor-disk", state.disk.usage);
        self.clip = None;
        let _ = self.cr.restore();
        (content, viewport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_monitor_fits_control_center_and_scales_to_more_threads_and_gpus() {
        let mut m = Model::new();
        m.navigate(Page::System);
        m.monitor.ready = true;
        m.monitor.cpu_name = "Fixture CPU".into();
        m.monitor.threads = (0..16)
            .map(|id| crate::monitor::Cpu {
                id,
                usage: Some(25.0),
            })
            .collect();
        m.monitor.gpus = vec![
            Gpu {
                name: "Integrated GPU".into(),
                ..Default::default()
            },
            Gpu {
                name: "Discrete GPU".into(),
                ..Default::default()
            },
        ];
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = Context::new(&surface).unwrap();
        let scene = expanded(&cr, &m, 1, WIDTH);
        assert_eq!(popup_height(&m), HEIGHT);
        assert_eq!(popup_width(&m), WIDTH);
        assert!(scene.content_height <= scene.viewport_height);
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|h| matches!(h.action, Action::MonitorThread(_)))
                .count(),
            16
        );
        assert!(!scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Slider(_) | Action::Scrollbar(_))));
        m.monitor.threads = (0..96)
            .map(|id| crate::monitor::Cpu {
                id,
                usage: Some(0.0),
            })
            .collect();
        let scene = expanded(&cr, &m, 1, WIDTH);
        assert!(scene.content_height > scene.viewport_height);
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Scrollbar(_))));
    }
    #[test]
    fn unknown_metrics_use_a_dash_instead_of_fake_zero() {
        assert_eq!(value(None, "%"), "—");
        assert_eq!(value(Some(f64::NAN), "°C"), "—");
        assert_eq!(value(Some(46.0), "°C"), "46°C");
    }
    #[test]
    fn gpu_gauges_keep_hits_and_viewport_without_arming_cpu_repaints() {
        let mut m = Model::new();
        m.navigate(Page::System);
        m.scroll = 20.0;
        m.monitor.ready = true;
        m.monitor.threads = (0..16)
            .map(|id| crate::monitor::Cpu {
                id,
                usage: Some(25.0),
            })
            .collect();
        let usage = Usage::new(25, 100).unwrap();
        m.monitor.memory = Some(usage);
        m.monitor.disk.usage = Some(usage);
        m.monitor.gpus = (0..2)
            .map(|id| Gpu {
                id: id.to_string(),
                usage: Some(50.0),
                vram: Some(usage),
                ..Default::default()
            })
            .collect();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 520, HEIGHT).unwrap();
        let cr = Context::new(&surface).unwrap();
        let reference = expanded(&cr, &m, 1, 520);
        let capture = GaugeCapture::enter();
        let scene = expanded(&cr, &m, 1, 520);
        let gauges = capture.take();
        assert_eq!(gauges.len(), 22); // 16 threads, GPU/VRAM × 2, RAM, disk.
        for gauge in gauges.iter().filter(|g| g.key.starts_with("monitor-vram-")) {
            assert_eq!(gauge.rect.w, 408.0, "VRAM must use the full card width");
            assert_eq!(gauge.rect.h, 8.0);
            assert_eq!(gauge.rect.x, 52.0); // 32 px inset + centered 520 px surface.
            assert!(gauge.rect.y + gauge.rect.h <= gauge.clip.y + gauge.clip.h);
            let id = gauge.key.strip_prefix("monitor-vram-").unwrap();
            let usage = gauges
                .iter()
                .find(|g| g.key == format!("monitor-gpu-{id}"))
                .unwrap();
            assert_eq!(
                (gauge.rect.x, gauge.rect.w, gauge.rect.h),
                (usage.rect.x, usage.rect.w, usage.rect.h),
                "GPU and VRAM tracks must align vertically with identical dimensions"
            );
            assert_eq!(gauge.rect.y - usage.rect.y, 27.0);
        }
        assert_eq!(scene.hits.len(), reference.hits.len());
        for (actual, expected) in scene.hits.iter().zip(&reference.hits) {
            assert_eq!(
                (actual.rect.x, actual.rect.y, actual.rect.w, actual.rect.h),
                (
                    expected.rect.x,
                    expected.rect.y,
                    expected.rect.w,
                    expected.rect.h
                )
            );
        }
        let cpu = &gauges[0];
        assert_eq!((cpu.rect.x, cpu.rect.y), (55.0, 113.0));
        assert_eq!((cpu.clip.x, cpu.clip.y), (36.0, 86.0));
        assert_eq!(cpu.clip.y + cpu.clip.h, 484.0);
        assert!(cpu.vertical);
        assert_eq!(cpu.level, 0.25);
        m.monitor.threads[0].usage = Some(75.0);
        expanded(&cr, &m, 1, 520);
        assert_eq!(capture.take()[0].level, 0.75);
        assert!(
            !m.animating(),
            "GPU gauge motion must not rerasterize the body"
        );
        drop(capture);
        assert!(GPU_GAUGES.with(|g| g.borrow().is_none()));
        // The SHM fallback still interpolates values when GPU capture is absent.
        m.monitor.threads[0].usage = Some(10.0);
        expanded(&cr, &m, 1, 520);
        assert!(m.animating());
    }
    #[test]
    fn capacity_numbers_stay_inside_the_right_inset_at_mixed_output_scales() {
        let m = Model::new();
        for scale in [1.0, 1.5, 2.0] {
            let mut surface = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                (480.0 * scale) as i32,
                (50.0 * scale) as i32,
            )
            .unwrap();
            {
                let cr = Context::new(&surface).unwrap();
                cr.scale(scale, scale);
                let p = Paint {
                    cr: &cr,
                    m: &m,
                    id: 1,
                    hits: vec![],
                    clip: None,
                };
                p.monitor_text(
                    "190.8 / 472.9 GiB · 40%",
                    rect(188.0, 10.0, 252.0, 20.0),
                    12.0,
                    false,
                    true,
                    false,
                );
            }
            surface.flush();
            let stride = surface.stride() as usize;
            let bytes = surface.data().unwrap();
            let mut right = 0;
            for y in 0..(50.0 * scale) as usize {
                for x in 0..(480.0 * scale) as usize {
                    if bytes[y * stride + x * 4 + 3] > 16 {
                        right = right.max(x);
                    }
                }
            }
            assert!(right < (440.0 * scale) as usize);
            assert!(right > (436.0 * scale) as usize);
        }
    }
}
