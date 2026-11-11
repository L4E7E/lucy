//! Compact read-only telemetry in the control center's existing glass surface.
use super::*;
use crate::monitor::{Gpu, Usage};
use std::time::Duration;

const TOP: f64 = 62.0;
const THREADS_PER_ROW: usize = 32;
const VRAM_SECTION_Y: f64 = 53.0;
const MONITOR_GAUGE_HEIGHT: f64 = 10.0;
const CAPACITY_GAUGE_Y: f64 = 28.0;
const GPU_ROW_HEIGHT: f64 = 100.0;
const GPU_GAP: f64 = 8.0;
const CAPACITY_HEIGHT: f64 = 50.0;
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
    count.max(1) as f64 * GPU_ROW_HEIGHT + count.saturating_sub(1) as f64 * GPU_GAP
}
fn capacity_detail(usage: Option<Usage>) -> String {
    usage.map_or_else(
        || "—".into(),
        |u| format!("{} · {:.0}%", u.text(), u.percent()),
    )
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
                    direct: key.starts_with("media-progress:")
                        && (self.m.media_pending()
                            || matches!(self.m.dragging, Some((_, Slider::Media(_, _), _)))),
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
    pub(super) fn data_gauge(
        &self,
        r: Rect,
        key: &str,
        percent: Option<f64>,
        title: &str,
        detail: &str,
    ) {
        let _ = self.cr.save();
        round(self.cr, r, r.h / 2.0);
        // Known track alpha encodes antialiased glyph coverage for the GPU.
        // Source replaces the underlying glass only inside this small capsule.
        self.cr.set_operator(cairo::Operator::Source);
        self.cr
            .set_source_rgba(0.32, 0.33, 0.36, crate::gauge::TRACK_ALPHA);
        let _ = self.cr.fill();
        let _ = self.cr.restore();
        let label_width = if key.starts_with("monitor-gpu-") {
            264.0
        } else {
            112.0
        };
        let labels = || {
            if title.is_empty() && detail.is_empty() {
                return;
            }
            self.monitor_text(
                title,
                rect(r.x + 12.0, r.y, label_width, r.h),
                if r.h < 24.0 { 11.0 } else { 12.0 },
                true,
                false,
                false,
            );
            self.monitor_text(
                detail,
                rect(r.x + label_width + 28.0, r.y, r.w - label_width - 40.0, r.h),
                if r.h < 24.0 { 11.0 } else { 12.0 },
                false,
                true,
                false,
            );
        };
        if self.capture_monitor_gauge(r, key, percent, false, (r.h / 2.0) as f32, 1.0) {
            labels();
            return;
        }
        labels();
        if let Some(percent) = percent.filter(|p| p.is_finite()) {
            let amount = if key.starts_with("media-progress:") {
                // Software fallback follows the precise playback/seek clock;
                // do not arm an endless full-scene repaint for live time.
                percent.clamp(0.0, 100.0) / 100.0
            } else {
                self.m
                    .motion_value(key, percent.clamp(0.0, 100.0), Duration::from_millis(240))
                    / 100.0
            };
            let _ = self.cr.save();
            round(self.cr, r, r.h / 2.0);
            self.cr.clip();
            // Clip the whole capsule, even below one corner radius; no overflow.
            let fill = rect(r.x, r.y, r.w * amount, r.h);
            round(self.cr, fill, (r.h / 2.0).min(fill.w / 2.0));
            self.cr.set_source_rgb(1.0, 1.0, 1.0);
            let _ = self.cr.fill();
            // Repaint only the covered glyphs in dark ink on the SHM fallback.
            round(self.cr, fill, (r.h / 2.0).min(fill.w / 2.0));
            self.cr.clip();
            self.cr.push_group_with_content(cairo::Content::Alpha);
            labels();
            if let Ok(mask) = self.cr.pop_group() {
                self.cr.set_source_rgb(0.12, 0.13, 0.15);
                let _ = self.cr.mask(&mask);
            }
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
    fn monitor_device_caption(&self, text: &str, r: Rect) {
        self.monitor_text(text, r, 11.0, false, false, true);
    }
    fn monitor_gpu(&self, gpu: &Gpu, index: usize, count: usize, y: f64) {
        crystal(self.cr, rect(20.0, y, 432.0, GPU_ROW_HEIGHT), 18.0, false);
        let usage = if gpu.sleeping {
            "절전 중".into()
        } else {
            value(gpu.usage, "%")
        };
        let title = if count > 1 {
            format!("GPU {}", index + 1)
        } else {
            "GPU".into()
        };
        self.monitor_text(
            &title,
            rect(32.0, y + 5.0, 80.0, 20.0),
            13.0,
            true,
            false,
            false,
        );
        self.monitor_text(
            &format!("{} · {usage}", value(gpu.temperature, "°C")),
            rect(116.0, y + 5.0, 324.0, 20.0),
            12.0,
            false,
            true,
            false,
        );
        self.monitor_device_caption(
            if gpu.name.is_empty() {
                "장치 정보 없음"
            } else {
                &gpu.name
            },
            rect(32.0, y + 25.0, 408.0, 18.0),
        );
        self.data_gauge(
            rect(32.0, y + 46.0, 408.0, MONITOR_GAUGE_HEIGHT),
            &format!("monitor-gpu-{}", gpu.id),
            gpu.usage,
            "",
            "",
        );
        self.capacity_contents(
            y + VRAM_SECTION_Y,
            if gpu.integrated {
                "VRAM · 예약"
            } else {
                "VRAM"
            },
            &format!("monitor-vram-{}", gpu.id),
            gpu.vram,
        );
    }
    fn monitor_capacity(&self, y: f64, title: &str, key: &str, usage: Option<Usage>) {
        crystal(self.cr, rect(20.0, y, 432.0, CAPACITY_HEIGHT), 18.0, false);
        self.capacity_contents(y, title, key, usage);
    }
    // Share track geometry and readouts; VRAM uses the GPU device caption style.
    fn capacity_contents(&self, y: f64, title: &str, key: &str, usage: Option<Usage>) {
        let label = rect(32.0, y + 5.0, 112.0, 20.0);
        if key.starts_with("monitor-vram-") {
            self.monitor_device_caption(title, label);
        } else {
            self.monitor_text(title, label, 13.0, true, false, false);
        }
        self.monitor_text(
            &capacity_detail(usage),
            rect(148.0, y + 5.0, 292.0, 20.0),
            12.0,
            false,
            true,
            false,
        );
        self.data_gauge(
            rect(32.0, y + CAPACITY_GAUGE_Y, 408.0, MONITOR_GAUGE_HEIGHT),
            key,
            usage.map(Usage::percent),
            "",
            "",
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
        let content = cpu_height(state.threads.len())
            + gpu_height(state.gpus.len())
            + CAPACITY_HEIGHT * 2.0
            + 24.0;
        let viewport = f64::from(height) - TOP - 16.0;
        let clip = rect(16.0, TOP, 440.0, viewport);
        self.clip = Some(clip);
        let _ = self.cr.save();
        self.cr.rectangle(clip.x, clip.y, clip.w, clip.h);
        self.cr.clip();
        let mut y = TOP - self.m.scroll;
        self.monitor_cpu(y);
        y += cpu_height(state.threads.len()) + 8.0;
        if state.gpus.is_empty() {
            crystal(self.cr, rect(20.0, y, 432.0, GPU_ROW_HEIGHT), 18.0, false);
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
                self.monitor_gpu(
                    gpu,
                    index,
                    state.gpus.len(),
                    y + index as f64 * (GPU_ROW_HEIGHT + GPU_GAP),
                );
            }
        }
        y += gpu_height(state.gpus.len()) + 8.0;
        self.monitor_capacity(y, "RAM", "monitor-memory", state.memory);
        y += CAPACITY_HEIGHT + 8.0;
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
    fn monitor_keeps_control_center_size_and_scrolls_without_squeezing_metric_rows() {
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
        assert!(scene.content_height > scene.viewport_height);
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
            .any(|h| matches!(h.action, Action::Slider(_))));
        assert!(scene
            .hits
            .iter()
            .any(|h| matches!(h.action, Action::Scrollbar(_))));
        assert!(
            !scene
                .hits
                .iter()
                .any(|h| matches!(h.action, Action::Page(Page::Home))),
            "the standalone monitor must not link back to the control center"
        );
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
    fn ram_and_disk_share_the_same_header_and_text_free_slim_gauge() {
        let m = Model::new();
        let usage = Usage::new(25, 100);
        let mut reference = None;
        for key in ["monitor-memory", "monitor-disk"] {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 64).unwrap();
            let capture = GaugeCapture::enter();
            {
                let cr = Context::new(&image).unwrap();
                let p = Paint {
                    cr: &cr,
                    m: &m,
                    id: 1,
                    hits: vec![],
                    clip: None,
                };
                p.capacity_contents(0.0, "Capacity", key, usage);
            }
            let specs = capture.take();
            assert_eq!(specs.len(), 1);
            assert_eq!(
                (
                    specs[0].rect.x,
                    specs[0].rect.y,
                    specs[0].rect.w,
                    specs[0].rect.h
                ),
                (32.0, CAPACITY_GAUGE_Y, 408.0, 10.0)
            );
            image.flush();
            let stride = image.stride() as usize;
            let pixels = image.data().unwrap().to_vec();
            assert!(
                (40..432)
                    .all(|x| pixels[(CAPACITY_GAUGE_Y as usize + 5) * stride + x * 4 + 3] <= 164),
                "labels must stay above, never inside the gauge"
            );
            if let Some(previous) = &reference {
                assert_eq!(&pixels, previous, "{key} has a different visual layout");
            } else {
                reference = Some(pixels);
            }
        }
    }
    #[test]
    fn vram_label_matches_the_gpu_device_caption() {
        let m = Model::new();
        let render = |vram| {
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 64).unwrap();
            {
                let cr = Context::new(&image).unwrap();
                let p = Paint {
                    cr: &cr,
                    m: &m,
                    id: 1,
                    hits: vec![],
                    clip: None,
                };
                if vram {
                    p.capacity_contents(0.0, "VRAM · 예약", "monitor-vram-0", None);
                } else {
                    p.monitor_device_caption("VRAM · 예약", rect(32.0, 5.0, 112.0, 20.0));
                }
            }
            image.flush();
            let stride = image.stride() as usize;
            let pixels = image.data().unwrap();
            (5..25)
                .flat_map(|y| {
                    pixels[y * stride + 32 * 4..y * stride + 144 * 4]
                        .iter()
                        .copied()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(render(true), render(false));
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
        for gauge in gauges.iter().filter(|g| !g.vertical) {
            assert_eq!(gauge.rect.h, 10.0, "{} track height differs", gauge.key);
            assert_eq!(gauge.radius, 5.0, "{} corner radius differs", gauge.key);
        }
        for gauge in gauges.iter().filter(|g| g.key.starts_with("monitor-vram-")) {
            assert_eq!(gauge.rect.w, 408.0, "VRAM must use the full card width");
            assert_eq!(gauge.rect.h, MONITOR_GAUGE_HEIGHT);
            assert_eq!(gauge.rect.x, 52.0); // 32 px inset + centered 520 px surface.
            assert!(gauge.rect.y + gauge.rect.h <= gauge.clip.y + gauge.clip.h);
            let id = gauge.key.strip_prefix("monitor-vram-").unwrap();
            let usage = gauges
                .iter()
                .find(|g| g.key == format!("monitor-gpu-{id}"))
                .unwrap();
            assert_eq!(
                (gauge.rect.x, gauge.rect.w),
                (usage.rect.x, usage.rect.w),
                "GPU and VRAM tracks must align at both horizontal edges"
            );
            assert_eq!(usage.rect.h, gauge.rect.h);
            assert_eq!(
                gauge.rect.y - usage.rect.y,
                VRAM_SECTION_Y + CAPACITY_GAUGE_Y - 46.0
            );
        }
        let gpu0 = gauges.iter().find(|g| g.key == "monitor-gpu-0").unwrap();
        let gpu1 = gauges.iter().find(|g| g.key == "monitor-gpu-1").unwrap();
        let vram0 = gauges.iter().find(|g| g.key == "monitor-vram-0").unwrap();
        assert_eq!(gpu1.rect.y - gpu0.rect.y, GPU_ROW_HEIGHT + GPU_GAP);
        assert!(gpu1.rect.y > vram0.rect.y + vram0.rect.h + GPU_GAP);
        assert_eq!(gpu0.rect.h, gpu1.rect.h);
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
    #[test]
    fn inline_labels_keep_contrast_at_empty_half_and_full_capacity() {
        for scale in [1.0, 1.5, 2.0] {
            let render = |title: &str, detail: &str, level: f64| {
                let m = Model::new();
                let mut image = cairo::ImageSurface::create(
                    cairo::Format::ARgb32,
                    (448.0 * scale) as i32,
                    (44.0 * scale) as i32,
                )
                .unwrap();
                {
                    let cr = Context::new(&image).unwrap();
                    cr.scale(scale, scale);
                    let paint = Paint {
                        cr: &cr,
                        m: &m,
                        id: 1,
                        hits: vec![],
                        clip: None,
                    };
                    paint.data_gauge(
                        rect(20.0, 7.0, 408.0, 30.0),
                        "monitor-memory",
                        Some(level),
                        title,
                        detail,
                    );
                }
                image.flush();
                let pixels = image.data().unwrap().to_vec();
                pixels
            };
            for percent in [0.0, 50.0, 100.0] {
                let background = render("", "", percent);
                let labels = render("RAM", "15.0 / 30.0 GiB · 50%", percent);
                let changed = background
                    .chunks_exact(4)
                    .zip(labels.chunks_exact(4))
                    .filter(|(a, b)| a[0].abs_diff(b[0]) > 60)
                    .count();
                assert!(
                    changed > (150.0 * scale * scale) as usize,
                    "inline ink must stay legible at {percent}% / {scale}x"
                );
            }
        }
        assert_eq!(capacity_detail(None), "—");
        assert!(capacity_detail(Usage::new(25, 100)).ends_with("25%"));
    }
}
