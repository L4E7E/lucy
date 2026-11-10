//! Bounded per-thread shaping cache. Translation is intentionally not a key:
//! scrolling changes positions, not glyph metrics. Never cache secret entries.
use cairo::{Context, Format, ImageSurface};
use glib::translate::{from_glib_full, ToGlibPtr};
use std::{cell::RefCell, collections::HashMap};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Key {
    font: String,
    text: String,
    scale: [u64; 4],
    options: Option<cairo::FontOptions>,
    kind: u8,
    size: u64,
    width: i32,
    align: i32,
    weight: i32,
}
impl Key {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cr: &Context,
        font: &str,
        text: &str,
        kind: u8,
        size: f64,
        width: i32,
        align: pango::Alignment,
        weight: pango::Weight,
    ) -> Self {
        use glib::translate::IntoGlib;
        let matrix = cr.matrix();
        Self {
            font: font.into(),
            text: text.into(),
            kind,
            size: size.to_bits(),
            width,
            align: align.into_glib(),
            weight: weight.into_glib(),
            options: cr.font_options().ok(),
            scale: [
                matrix.xx().to_bits(),
                matrix.yx().to_bits(),
                matrix.xy().to_bits(),
                matrix.yy().to_bits(),
            ],
        }
    }
}
#[derive(Default)]
struct Cache {
    entries: HashMap<Key, (pango::Layout, u64)>,
    clock: u64,
}
const LIMIT: usize = 384;
thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    static PRIVATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SMOOTH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static COVERAGE: RefCell<Vec<(pango::Layout, u64, Coverage)>> = const { RefCell::new(Vec::new()) };
    static CONTEXTS: RefCell<Vec<(ContextKey, pango::Context)>> = const { RefCell::new(Vec::new()) };
}
#[derive(PartialEq)]
struct ContextKey {
    matrix: [u64; 4],
    device_scale: [u64; 2],
    options: cairo::FontOptions,
}
#[link(name = "cairo")]
extern "C" {
    fn cairo_surface_get_font_options(
        surface: *mut cairo::ffi::cairo_surface_t,
        options: *mut cairo::ffi::cairo_font_options_t,
    );
}
pub struct Private(bool);
impl Private {
    pub fn enter() -> Self {
        Self(PRIVATE.with(|v| v.replace(true)))
    }
}
impl Drop for Private {
    fn drop(&mut self) {
        PRIVATE.with(|v| v.set(self.0));
    }
}
pub fn layout(key: Key, build: impl FnOnce() -> pango::Layout) -> pango::Layout {
    if PRIVATE.with(|v| v.get()) {
        return build();
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.clock = cache.clock.wrapping_add(1);
        let clock = cache.clock;
        if let Some((layout, last)) = cache.entries.get_mut(&key) {
            *last = clock;
            return layout.clone();
        }
        let layout = build();
        if cache.entries.len() >= LIMIT {
            let oldest = cache
                .entries
                .iter()
                .min_by_key(|(_, (_, age))| *age)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                cache.entries.remove(&oldest);
            }
        }
        cache.entries.insert(key, (layout.clone(), clock));
        layout
    })
}
#[link(name = "pangocairo-1.0")]
extern "C" {
    fn pango_cairo_create_context(cr: *mut cairo::ffi::cairo_t) -> *mut pango::ffi::PangoContext;
    #[cfg(test)]
    fn pango_cairo_create_layout(cr: *mut cairo::ffi::cairo_t) -> *mut pango::ffi::PangoLayout;
    fn pango_cairo_show_layout(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
    fn pango_cairo_layout_path(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
}
pub fn create(cr: &Context) -> pango::Layout {
    // Layouts stay independent, but share their font/shaping context for the
    // same transform/options. Never mutate a cached context: older cached
    // layouts may still refer to it, including layouts on the other output.
    let Ok(mut options) = cairo::FontOptions::new() else {
        let context = unsafe { from_glib_full(pango_cairo_create_context(cr.to_raw_none())) };
        return pango::Layout::new(&context);
    };
    let target = cr.target();
    unsafe {
        cairo_surface_get_font_options(target.to_raw_none(), options.to_raw_none());
    }
    if let Ok(explicit) = cr.font_options() {
        options.merge(&explicit);
    }
    let matrix = cr.matrix();
    let (dx, dy) = target.device_scale();
    let key = ContextKey {
        matrix: [matrix.xx(), matrix.yx(), matrix.xy(), matrix.yy()].map(f64::to_bits),
        device_scale: [dx.to_bits(), dy.to_bits()],
        options,
    };
    CONTEXTS.with(|contexts| {
        let mut contexts = contexts.borrow_mut();
        if let Some(index) = contexts.iter().position(|(k, _)| *k == key) {
            let entry = contexts.remove(index);
            let layout = pango::Layout::new(&entry.1);
            contexts.push(entry);
            return layout;
        }
        // SAFETY: Cairo is live; Pango returns a new, owned GObject reference.
        let context = unsafe { from_glib_full(pango_cairo_create_context(cr.to_raw_none())) };
        let layout = pango::Layout::new(&context);
        if contexts.len() >= 8 {
            contexts.remove(0);
        }
        contexts.push((key, context));
        layout
    })
}

/// Glass text uses grayscale coverage, not LCD subpixels or grid-fitted stems.
/// Keep this local to notification drawing, leaving system font settings alone.
pub struct SmoothText<'a>(&'a Context, Option<cairo::FontOptions>, bool);
impl<'a> SmoothText<'a> {
    pub fn enter(cr: &'a Context) -> Self {
        let old = cr.font_options().ok();
        if let Ok(mut options) = cairo::FontOptions::new() {
            options.set_antialias(cairo::Antialias::Gray);
            options.set_hint_style(cairo::HintStyle::None);
            options.set_hint_metrics(cairo::HintMetrics::Off);
            cr.set_font_options(&options);
        }
        Self(cr, old, SMOOTH.with(|v| v.replace(true)))
    }
}
impl Drop for SmoothText<'_> {
    fn drop(&mut self) {
        if let Some(options) = &self.1 {
            self.0.set_font_options(options);
        }
        SMOOTH.with(|v| v.set(self.2));
    }
}

#[derive(Clone)]
struct Coverage {
    mask: ImageSurface,
    offset: (f64, f64),
    scale: f64,
}
const COVERAGE_BYTES: i32 = 2 * 1024 * 1024;
impl Coverage {
    fn new(cr: &Context, layout: &pango::Layout, scale: f64) -> Option<Self> {
        Self::sampled::<4>(cr, layout, scale)
    }
    fn sampled<const SAMPLE: usize>(
        cr: &Context,
        layout: &pango::Layout,
        scale: f64,
    ) -> Option<Self> {
        // Extract vector outlines at the layout's original transform. Enlarging
        // an already rasterized glyph would only enlarge its stair steps.
        let probe = ImageSurface::create(Format::A8, 1, 1).ok()?;
        let pc = Context::new(&probe).ok()?;
        pc.scale(scale, scale);
        if let Ok(options) = cr.font_options() {
            pc.set_font_options(&options);
        }
        pc.move_to(0.0, 0.0);
        unsafe { pango_cairo_layout_path(pc.to_raw_none(), layout.to_glib_none().0) };
        let (left, top, right, bottom) = pc.path_extents().ok()?;
        let (x, y) = ((left * scale).floor() - 1.0, (top * scale).floor() - 1.0);
        let (w, h) = (
            (right * scale).ceil() as i32 - x as i32 + 1,
            (bottom * scale).ceil() as i32 - y as i32 + 1,
        );
        if right <= left || bottom <= top || w <= 0 || h <= 0 || w > 2048 || h > 512 {
            return None;
        }
        let mut high =
            ImageSurface::create(Format::A8, w * SAMPLE as i32, h * SAMPLE as i32).ok()?;
        {
            let hc = Context::new(&high).ok()?;
            hc.scale(scale * SAMPLE as f64, scale * SAMPLE as f64);
            hc.translate(-x / scale, -y / scale);
            hc.set_antialias(cairo::Antialias::Best);
            hc.set_tolerance(0.01);
            hc.append_path(&pc.copy_path().ok()?);
            hc.set_source_rgb(1.0, 1.0, 1.0);
            hc.fill().ok()?;
        }
        let mut mask = ImageSurface::create(Format::A8, w, h).ok()?;
        let (source_stride, stride) = (high.stride() as usize, mask.stride() as usize);
        {
            let source = high.data().ok()?;
            let mut bytes = mask.data().ok()?;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let mut sum = 0u32;
                    for sy in 0..SAMPLE {
                        for sx in 0..SAMPLE {
                            sum += u32::from(
                                source[(y * SAMPLE + sy) * source_stride + x * SAMPLE + sx],
                            );
                        }
                    }
                    let count = (SAMPLE * SAMPLE) as u32;
                    bytes[y * stride + x] = ((sum + count / 2) / count) as u8;
                }
            }
        }
        Some(Self {
            mask,
            offset: (x, y),
            scale,
        })
    }
    fn bytes(&self) -> i32 {
        self.mask.stride() * self.mask.height()
    }
    fn paint(&self, cr: &Context, x: f64, y: f64) {
        let physical = cr.user_to_device(x, y);
        let Ok((x, y)) = cr.device_to_user(physical.0.round(), physical.1.round()) else {
            return;
        };
        if cr.save().is_err() {
            return;
        }
        cr.translate(x, y);
        cr.scale(1.0 / self.scale, 1.0 / self.scale);
        let _ = cr.mask_surface(&self.mask, self.offset.0, self.offset.1);
        let _ = cr.restore();
    }
}

/// Notification text only. Keep original Pango shaping/wrapping and draw cached,
/// physical-pixel coverage; scrolling must not rerasterize high-resolution text.
pub fn show(cr: &Context, layout: &pango::Layout) {
    let matrix = cr.matrix();
    let scale = matrix.xx();
    if SMOOTH.with(|v| v.get())
        && !PRIVATE.with(|v| v.get())
        && matrix.xy() == 0.0
        && matrix.yx() == 0.0
        && (1.0..=4.0).contains(&scale)
        && (matrix.yy() - scale).abs() < 0.0001
        // Keep Pango's native color-glyph renderer for emoji sequences.
        && !layout.text().chars().any(|c| matches!(c as u32, 0xfe0f | 0x200d | 0x1f000..=0x1faff))
    {
        let coverage = COVERAGE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(index) = cache
                .iter()
                .position(|(l, s, _)| l == layout && *s == scale.to_bits())
            {
                let entry = cache.remove(index);
                let result = entry.2.clone();
                cache.push(entry);
                return Some(result);
            }
            let result = Coverage::new(cr, layout, scale)?;
            let bytes = result.bytes();
            while !cache.is_empty()
                && (cache.len() >= 48
                    || cache.iter().map(|(_, _, v)| v.bytes()).sum::<i32>() + bytes
                        > COVERAGE_BYTES)
            {
                cache.remove(0);
            }
            if bytes <= COVERAGE_BYTES {
                cache.push((layout.clone(), scale.to_bits(), result.clone()));
            }
            Some(result)
        });
        if let (Some(coverage), Ok((x, y))) = (coverage, cr.current_point()) {
            coverage.paint(cr, x, y);
            return;
        }
    }
    unsafe { pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0) };
}

#[derive(Clone)]
pub struct Caption {
    coverage: Coverage,
    pub width: f64,
    pub height: f64,
}

thread_local! {
    static CAPTIONS: RefCell<Vec<(Key, Caption)>> = const { RefCell::new(Vec::new()) };
}

impl Caption {
    pub fn paint(&self, cr: &Context, x: f64, y: f64) {
        self.coverage.paint(cr, x, y);
    }
}

/// Small fixed labels only: supersample once, keep just the physical-size A8
/// coverage, and reuse it while scrolling. No full-window high-resolution buffer.
pub fn caption(cr: &Context, font: &str, value: &str, size: f64) -> Option<Caption> {
    if value.is_empty() {
        return None;
    }
    let key = Key::new(
        cr,
        font,
        value,
        9,
        size,
        -1,
        pango::Alignment::Left,
        pango::Weight::Normal,
    );
    if !PRIVATE.with(|v| v.get()) {
        if let Some(found) = CAPTIONS.with(|cache| {
            let mut cache = cache.borrow_mut();
            let index = cache.iter().position(|(k, _)| *k == key)?;
            let found = cache.remove(index);
            let result = found.1.clone();
            cache.push(found);
            Some(result)
        }) {
            return Some(found);
        }
    }
    let scale = cr
        .matrix()
        .xx()
        .hypot(cr.matrix().yx())
        .max(cr.matrix().yy().hypot(cr.matrix().xy()))
        .clamp(1.0, 4.0);
    let measure = ImageSurface::create(Format::A8, 1, 1).ok()?;
    let mc = Context::new(&measure).ok()?;
    mc.scale(scale, scale);
    let mut options = cairo::FontOptions::new().ok()?;
    options.set_antialias(cairo::Antialias::Gray);
    options.set_hint_style(cairo::HintStyle::None);
    options.set_hint_metrics(cairo::HintMetrics::Off);
    mc.set_font_options(&options);
    let layout = create(&mc);
    let mut desc = pango::FontDescription::from_string(font);
    desc.set_absolute_size(size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&desc));
    layout.set_text(value);
    layout.set_single_paragraph_mode(true);
    let (width, height) = layout.pixel_size();
    let result = Caption {
        coverage: Coverage::new(&mc, &layout, scale)?,
        width: f64::from(width),
        height: f64::from(height),
    };
    if !PRIVATE.with(|v| v.get()) {
        CAPTIONS.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 32 {
                cache.remove(0);
            }
            cache.push((key, result.clone()));
        });
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contexts_are_shared_bounded_and_preserve_original_text_pixels() {
        CONTEXTS.with(|c| c.borrow_mut().clear());
        for scale in [1.0, 1.5, 2.0] {
            for smooth in [false, true] {
                let mut actual = ImageSurface::create(Format::ARgb32, 900, 180).unwrap();
                let mut expected = ImageSurface::create(Format::ARgb32, 900, 180).unwrap();
                for (image, cached) in [(&actual, true), (&expected, false)] {
                    let cr = Context::new(image).unwrap();
                    cr.scale(scale, scale);
                    cr.translate(1.25, 2.75);
                    let _smooth = smooth.then(|| SmoothText::enter(&cr));
                    let layout = if cached {
                        create(&cr)
                    } else {
                        unsafe { from_glib_full(pango_cairo_create_layout(cr.to_raw_none())) }
                    };
                    let mut desc = pango::FontDescription::from_string("Sans 13");
                    desc.set_absolute_size(17.0 * f64::from(pango::SCALE));
                    layout.set_font_description(Some(&desc));
                    layout.set_text("알림 Notification 0123 · العربية");
                    cr.set_source_rgb(1.0, 1.0, 1.0);
                    cr.move_to(10.0, 12.0);
                    unsafe { pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0) };
                    if cached {
                        let another = create(&cr);
                        assert_ne!(layout, another);
                        assert_eq!(layout.context(), another.context());
                    }
                }
                assert_eq!(&*actual.data().unwrap(), &*expected.data().unwrap());
            }
        }
        assert_eq!(CONTEXTS.with(|c| c.borrow().len()), 6);
        let image = ImageSurface::create(Format::A8, 1, 1).unwrap();
        let cr = Context::new(&image).unwrap();
        for i in 1..=20 {
            cr.identity_matrix();
            cr.scale(1.0 + f64::from(i) / 10.0, 1.0);
            create(&cr);
        }
        assert_eq!(CONTEXTS.with(|c| c.borrow().len()), 8);
    }
    fn sample_layout(cr: &Context, value: &str) -> pango::Layout {
        let layout = create(cr);
        let mut desc = pango::FontDescription::from_string("Pretendard JP Medium 11");
        desc.set_absolute_size(14.0 * f64::from(pango::SCALE));
        layout.set_font_description(Some(&desc));
        layout.set_text(value);
        layout
    }
    #[test]
    fn smoothing_is_scoped_and_font_options_are_part_of_the_shape_cache_key() {
        let image = ImageSurface::create(Format::A8, 1, 1).unwrap();
        let cr = Context::new(&image).unwrap();
        let key = || {
            Key::new(
                &cr,
                "Sans 11",
                "알림",
                0,
                14.0,
                -1,
                pango::Alignment::Left,
                pango::Weight::Normal,
            )
        };
        let original = key();
        {
            let _smooth = SmoothText::enter(&cr);
            let options = cr.font_options().unwrap();
            assert_eq!(options.antialias(), cairo::Antialias::Gray);
            assert_eq!(options.hint_style(), cairo::HintStyle::None);
            assert_eq!(options.hint_metrics(), cairo::HintMetrics::Off);
            assert_ne!(key(), original);
            {
                let _nested = SmoothText::enter(&cr);
            }
            assert!(SMOOTH.with(|v| v.get()));
        }
        assert_eq!(key(), original);
        assert!(!SMOOTH.with(|v| v.get()));
    }
    #[test]
    fn notification_titles_bodies_and_times_have_antialiased_coverage_at_each_output_scale() {
        for scale in [1.0, 1.5, 2.0] {
            let image = ImageSurface::create(Format::ARgb32, 900, 240).unwrap();
            let cr = Context::new(&image).unwrap();
            cr.scale(scale, scale);
            let _smooth = SmoothText::enter(&cr);
            for value in [
                "알림 제목 Notification",
                "한글 본문과 영문 text 0123456789",
                "22:18",
                "2시간 전",
            ] {
                let layout = sample_layout(&cr, value);
                let coverage = Coverage::new(&cr, &layout, scale).unwrap();
                let mut levels = std::collections::HashSet::new();
                coverage
                    .mask
                    .with_data(|bytes| {
                        levels.extend(bytes.iter().copied().filter(|a| *a > 0 && *a < 255));
                    })
                    .unwrap();
                assert!(
                    levels.len() > 32,
                    "{value} at scale {scale}: {} coverage levels",
                    levels.len()
                );
                let stride = coverage.mask.stride() as usize;
                let (w, h) = (
                    coverage.mask.width() as usize,
                    coverage.mask.height() as usize,
                );
                coverage
                    .mask
                    .with_data(|bytes| {
                        // A transparent guard pixel prevents clipped antialiased edges.
                        assert!((0..w).all(|x| bytes[x] == 0 && bytes[(h - 1) * stride + x] == 0));
                        assert!((0..h)
                            .all(|y| bytes[y * stride] == 0 && bytes[y * stride + w - 1] == 0));
                    })
                    .unwrap();
            }
        }
    }
    #[test]
    fn notification_coverage_is_closer_to_a_dense_vector_reference_than_native_glyph_rasterization()
    {
        let image = ImageSurface::create(Format::A8, 900, 240).unwrap();
        let cr = Context::new(&image).unwrap();
        cr.scale(1.5, 1.5);
        let _smooth = SmoothText::enter(&cr);
        for value in ["한글 알림 내용 text", "22:18", "2시간 전"] {
            let layout = sample_layout(&cr, value);
            let reference = Coverage::sampled::<16>(&cr, &layout, 1.5).unwrap();
            let filtered = Coverage::new(&cr, &layout, 1.5).unwrap();
            let native =
                ImageSurface::create(Format::A8, reference.mask.width(), reference.mask.height())
                    .unwrap();
            {
                let nc = Context::new(&native).unwrap();
                nc.scale(1.5, 1.5);
                nc.translate(-reference.offset.0 / 1.5, -reference.offset.1 / 1.5);
                nc.set_font_options(&cr.font_options().unwrap());
                nc.move_to(0.0, 0.0);
                unsafe {
                    pango_cairo_show_layout(nc.to_raw_none(), layout.to_glib_none().0);
                }
            }
            let bytes = |s: &ImageSurface| {
                let mut v = vec![];
                s.with_data(|b| v.extend_from_slice(b)).unwrap();
                v
            };
            let reference = bytes(&reference.mask);
            let error = |s| {
                bytes(s)
                    .iter()
                    .zip(&reference)
                    .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
                    .sum::<f64>()
            };
            let (filtered_error, native_error) = (error(&filtered.mask), error(&native));
            eprintln!(
                "{value}: vector coverage error {filtered_error:.0}, native {native_error:.0}"
            );
            assert!(
                filtered_error < native_error,
                "{value}: vector coverage must improve edge accuracy"
            );
        }
    }
    #[test]
    fn notification_coverage_is_bounded_reused_on_scroll_and_never_retains_secrets() {
        COVERAGE.with(|c| c.borrow_mut().clear());
        let image = ImageSurface::create(Format::ARgb32, 900, 240).unwrap();
        let cr = Context::new(&image).unwrap();
        cr.scale(1.5, 1.5);
        let _smooth = SmoothText::enter(&cr);
        let layout = sample_layout(&cr, "알림 본문 테스트");
        cr.move_to(0.0, 10.0);
        show(&cr, &layout);
        let first = COVERAGE.with(|c| c.borrow()[0].2.mask.to_raw_none());
        cr.translate(0.0, -1.25);
        cr.move_to(0.0, 10.0);
        show(&cr, &layout);
        COVERAGE.with(|c| {
            assert_eq!(c.borrow().len(), 1);
            assert_eq!(c.borrow()[0].2.mask.to_raw_none(), first);
        });
        for i in 0..80 {
            show(&cr, &sample_layout(&cr, &format!("알림 본문 {i}")));
        }
        COVERAGE.with(|c| {
            let cache = c.borrow();
            assert!(cache.len() <= 48);
            assert!(cache.iter().map(|(_, _, v)| v.bytes()).sum::<i32>() <= COVERAGE_BYTES);
        });
        let before = COVERAGE.with(|c| c.borrow().len());
        {
            let _private = Private::enter();
            show(&cr, &sample_layout(&cr, "private-test"));
        }
        assert_eq!(before, COVERAGE.with(|c| c.borrow().len()));
    }
    #[test]
    fn captions_have_soft_fractional_coverage_and_reuse_physical_size_masks() {
        CAPTIONS.with(|c| c.borrow_mut().clear());
        let image = ImageSurface::create(Format::ARgb32, 300, 100).unwrap();
        let cr = Context::new(&image).unwrap();
        cr.scale(1.5, 1.5);
        let first = caption(&cr, "Sans 11", "21:48", 13.0).unwrap();
        assert!(first.coverage.mask.width() <= (first.width * 1.5).ceil() as i32 + 2);
        let mut partial = 0;
        let mut levels = std::collections::HashSet::new();
        first
            .coverage
            .mask
            .with_data(|bytes| {
                for alpha in bytes.iter().copied().filter(|a| *a > 0 && *a < 255) {
                    partial += 1;
                    levels.insert(alpha);
                }
            })
            .unwrap();
        assert!(partial > 80 && levels.len() > 32);
        cr.translate(0.0, -31.25);
        let second = caption(&cr, "Sans 11", "21:48", 13.0).unwrap();
        assert_eq!(
            first.coverage.mask.to_raw_none(),
            second.coverage.mask.to_raw_none()
        );
        for i in 0..70 {
            caption(&cr, "Sans 11", &format!("{i}시간 전"), 13.0).unwrap();
        }
        CAPTIONS.with(|c| {
            let cache = c.borrow();
            assert_eq!(cache.len(), 32);
            assert!(cache.iter().map(|(_, v)| v.coverage.bytes()).sum::<i32>() < 512 * 1024);
        });
        let before = CAPTIONS.with(|c| c.borrow().len());
        let _private = Private::enter();
        caption(&cr, "Sans 11", "private-test", 13.0).unwrap();
        assert_eq!(before, CAPTIONS.with(|c| c.borrow().len()));
    }
    #[test]
    fn secret_entry_layouts_are_not_retained() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&image).unwrap();
        let before = CACHE.with(|c| c.borrow().entries.len());
        {
            let _private = Private::enter();
            let key = Key::new(
                &cr,
                "Sans 11",
                "private-entry-test",
                0,
                0.0,
                100,
                pango::Alignment::Left,
                pango::Weight::Normal,
            );
            layout(key, || create(&cr));
        }
        assert_eq!(CACHE.with(|c| c.borrow().entries.len()), before);
    }
    #[test]
    fn shaping_key_ignores_scroll_translation_but_not_scale_or_weight() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&image).unwrap();
        let key = || {
            Key::new(
                &cr,
                "Sans 11",
                "시계",
                0,
                0.0,
                100,
                pango::Alignment::Left,
                pango::Weight::Normal,
            )
        };
        let before = key();
        cr.translate(12.0, -100.0);
        assert_eq!(key(), before);
        cr.scale(1.5, 1.5);
        assert_ne!(key(), before);
    }
    #[test]
    fn least_recent_entry_is_evicted_without_clearing_the_working_set() {
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&image).unwrap();
        CACHE.with(|c| *c.borrow_mut() = Cache::default());
        for n in 0..LIMIT + 5 {
            let key = Key::new(
                &cr,
                "Sans 11",
                &n.to_string(),
                0,
                0.0,
                100,
                pango::Alignment::Left,
                pango::Weight::Normal,
            );
            layout(key, || create(&cr));
        }
        CACHE.with(|c| assert_eq!(c.borrow().entries.len(), LIMIT));
    }
}
