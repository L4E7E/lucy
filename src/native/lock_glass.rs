//! Cached, glyph-local glass: supersampled coverage, curved refraction and light.
//! Never reads the screen or retains anything outside the already blurred image.
use super::{draw, model::Rect};
use cairo::{Context, Format, ImageSurface};
use glib::translate::ToGlibPtr;
use std::cell::RefCell;

#[link(name = "pangocairo-1.0")]
extern "C" {
    fn pango_cairo_layout_path(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
}

#[derive(PartialEq)]
struct Key {
    font: String,
    time: String,
    geometry: [u64; 4],
    background: usize,
}
#[derive(Clone)]
struct Artwork {
    image: ImageSurface,
    bounds: Rect,
    scale: f64,
}
struct Cached {
    key: Key,
    // Keep the source alive so its identity cannot be reused while cached.
    _background: Option<ImageSurface>,
    art: Artwork,
}
thread_local! {
    static CACHE: RefCell<Vec<Cached>> = const { RefCell::new(Vec::new()) };
}

fn samples(scale: f64) -> usize {
    (6.0 / scale).round().clamp(2.0, 4.0) as usize
}
const CLOCK_HEIGHT_RATIO: f64 = 1.14;

struct GlyphMask {
    alpha: Vec<u8>,
    w: usize,
    h: usize,
    bounds: Rect,
    path: cairo::Path,
}

fn mask(
    cr: &Context,
    font: &str,
    time: &str,
    width: f64,
    center: f64,
    scale: f64,
) -> Option<GlyphMask> {
    mask_with_ratio(cr, font, time, width, center, scale, CLOCK_HEIGHT_RATIO)
}
fn mask_with_ratio(
    cr: &Context,
    font: &str,
    time: &str,
    width: f64,
    center: f64,
    scale: f64,
    height_ratio: f64,
) -> Option<GlyphMask> {
    cr.save().ok()?;
    let geometry = (|| {
        // Hinting/snapping intended for small text visibly deforms large outlines.
        let mut options = cairo::FontOptions::new().ok()?;
        options.set_antialias(cairo::Antialias::Gray);
        options.set_hint_style(cairo::HintStyle::None);
        options.set_hint_metrics(cairo::HintMetrics::Off);
        cr.set_font_options(&options);
        let layout = super::text_cache::create(cr);
        let mut desc = pango::FontDescription::from_string(font);
        desc.set_absolute_size(96.0 * f64::from(pango::SCALE));
        desc.set_weight(pango::Weight::Semibold);
        layout.set_font_description(Some(&desc));
        layout.set_text(time);
        let (ink, logical) = layout.pixel_extents();
        let x = (width - f64::from(logical.width())) / 2.0;
        let y = center - 104.0 + (120.0 - f64::from(logical.height())) / 2.0;
        cr.new_path();
        let bottom = y + f64::from(ink.y() + ink.height());
        cr.save().ok()?;
        cr.translate(x, bottom);
        cr.scale(1.0, height_ratio);
        cr.move_to(0.0, y - bottom);
        // SAFETY: Cairo and the Pango layout are both live for this call.
        unsafe { pango_cairo_layout_path(cr.to_raw_none(), layout.to_glib_none().0) };
        cr.restore().ok()?;
        let (left, top, right, bottom) = cr.path_extents().ok()?;
        let bounds = Rect {
            x: ((left - 2.0) * scale).floor() / scale,
            y: ((top - 2.0) * scale).floor() / scale,
            w: right - left + 5.0,
            h: bottom - top + 5.0,
        };
        Some((cr.copy_path().ok()?, bounds))
    })();
    cr.new_path();
    cr.restore().ok()?;
    let (path, bounds) = geometry?;
    let w = (bounds.w * scale).ceil() as usize;
    let h = (bounds.h * scale).ceil() as usize;
    if w == 0 || h == 0 || w > 2048 || h > 1024 {
        return None;
    }
    // Only supersample the digits, never a full-screen lock buffer.
    let factor = samples(scale);
    let image = ImageSurface::create(Format::A8, (w * factor) as i32, (h * factor) as i32).ok()?;
    {
        let mc = Context::new(&image).ok()?;
        mc.set_antialias(cairo::Antialias::Best);
        mc.set_tolerance(0.02);
        mc.scale(scale * factor as f64, scale * factor as f64);
        mc.translate(-bounds.x, -bounds.y);
        mc.append_path(&path);
        mc.set_source_rgb(1.0, 1.0, 1.0);
        mc.fill().ok()?;
    }
    let stride = image.stride() as usize;
    let (w, h) = (w * factor, h * factor);
    let mut alpha = vec![0; w * h];
    image
        .with_data(|bytes| {
            for y in 0..h {
                alpha[y * w..(y + 1) * w].copy_from_slice(&bytes[y * stride..y * stride + w]);
            }
        })
        .ok()?;
    Some(GlyphMask {
        alpha,
        w,
        h,
        bounds,
        path,
    })
}

const FAR: f32 = 1e12;

/// Lower envelope of parabolas: exact squared Euclidean distance in O(n).
/// Unlike a chamfer stencil, this does not impose eight preferred directions.
fn distance_line(f: &[f32], out: &mut [f32], sites: &mut [usize], limits: &mut [f64]) {
    let Some(first) = f.iter().position(|v| *v < FAR) else {
        out.fill(FAR);
        return;
    };
    let mut k = 0;
    sites[0] = first;
    limits[0] = f64::NEG_INFINITY;
    limits[1] = f64::INFINITY;
    for q in first + 1..f.len() {
        if f[q] >= FAR {
            continue;
        }
        let intersection = |v: usize| {
            (f64::from(f[q]) + (q * q) as f64 - f64::from(f[v]) - (v * v) as f64)
                / (2.0 * (q - v) as f64)
        };
        let mut s = intersection(sites[k]);
        while k > 0 && s <= limits[k] {
            k -= 1;
            s = intersection(sites[k]);
        }
        k += 1;
        sites[k] = q;
        limits[k] = s;
        limits[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, value) in out.iter_mut().enumerate() {
        while limits[k + 1] < q as f64 {
            k += 1;
        }
        let delta = q as f32 - sites[k] as f32;
        *value = delta * delta + f[sites[k]];
    }
}

fn euclidean_squared(grid: &mut [f32], w: usize, h: usize) {
    let size = w.max(h);
    let (mut input, mut output) = (vec![0.0; size], vec![0.0; size]);
    let (mut sites, mut limits) = (vec![0; size], vec![0.0; size + 1]);
    for y in 0..h {
        input[..w].copy_from_slice(&grid[y * w..(y + 1) * w]);
        distance_line(&input[..w], &mut output[..w], &mut sites, &mut limits);
        grid[y * w..(y + 1) * w].copy_from_slice(&output[..w]);
    }
    for x in 0..w {
        for y in 0..h {
            input[y] = grid[y * w + x];
        }
        distance_line(&input[..h], &mut output[..h], &mut sites, &mut limits);
        for y in 0..h {
            grid[y * w + x] = output[y];
        }
    }
}

fn distances(alpha: &[u8], w: usize, h: usize) -> Vec<f32> {
    // Fractional coverage positions the boundary between pixels instead of
    // thresholding it into a staircase. Signed distances keep edge normals smooth.
    let mut outer: Vec<_> = alpha
        .iter()
        .map(|a| {
            if *a == 255 {
                FAR
            } else {
                (f32::from(*a) / 255.0 - 0.5).max(0.0).powi(2)
            }
        })
        .collect();
    let mut inner: Vec<_> = alpha
        .iter()
        .map(|a| {
            if *a == 0 {
                FAR
            } else {
                (0.5 - f32::from(*a) / 255.0).max(0.0).powi(2)
            }
        })
        .collect();
    euclidean_squared(&mut outer, w, h);
    euclidean_squared(&mut inner, w, h);
    for (outside, inside) in outer.iter_mut().zip(inner) {
        *outside = outside.sqrt() - inside.sqrt();
    }
    outer
}

struct Backdrop {
    bytes: Vec<u8>,
    w: usize,
    h: usize,
    stride: usize,
    scale: f64,
    origin: (f64, f64),
}

fn smooth_distance(d: &[f32], w: usize, h: usize) -> Vec<f32> {
    // A wider, separable Gaussian removes subpixel normal noise without
    // blurring the glyph coverage. The light and refraction share this field.
    const K: [f32; 9] = [1.0, 8.0, 28.0, 56.0, 70.0, 56.0, 28.0, 8.0, 1.0];
    let (mut horizontal, mut result) = (vec![0.0; d.len()], vec![0.0; d.len()]);
    for y in 0..h {
        for x in 0..w {
            horizontal[y * w + x] = K
                .iter()
                .enumerate()
                .map(|(i, weight)| {
                    let sx = (x as isize + i as isize - 4).clamp(0, w as isize - 1) as usize;
                    d[y * w + sx] * weight
                })
                .sum::<f32>()
                / 256.0;
        }
    }
    for y in 0..h {
        for x in 0..w {
            result[y * w + x] = K
                .iter()
                .enumerate()
                .map(|(i, weight)| {
                    let sy = (y as isize + i as isize - 4).clamp(0, h as isize - 1) as usize;
                    horizontal[sy * w + x] * weight
                })
                .sum::<f32>()
                / 256.0;
        }
    }
    result
}
impl Backdrop {
    fn new(image: &ImageSurface, width: f64, height: f64) -> Option<Self> {
        let (w, h) = (image.width() as usize, image.height() as usize);
        if w == 0 || h == 0 || image.format() != Format::ARgb32 {
            return None;
        }
        let scale = (width / w as f64).max(height / h as f64);
        let mut bytes = vec![];
        image.with_data(|b| bytes.extend_from_slice(b)).ok()?;
        Some(Self {
            bytes,
            w,
            h,
            stride: image.stride() as usize,
            scale,
            origin: (
                (width - w as f64 * scale) / 2.0,
                (height - h as f64 * scale) / 2.0,
            ),
        })
    }
    fn sample(&self, x: f64, y: f64) -> [f64; 3] {
        let x = ((x - self.origin.0) / self.scale - 0.5).clamp(0.0, (self.w - 1) as f64);
        let y = ((y - self.origin.1) / self.scale - 0.5).clamp(0.0, (self.h - 1) as f64);
        let (ix, iy) = (x as usize, y as usize);
        let (fx, fy) = (x.fract(), y.fract());
        let at = |x: usize, y: usize, c| f64::from(self.bytes[y * self.stride + x * 4 + c]);
        std::array::from_fn(|c| {
            let a = at(ix, iy, c) * (1.0 - fx) + at((ix + 1).min(self.w - 1), iy, c) * fx;
            let b = at(ix, (iy + 1).min(self.h - 1), c) * (1.0 - fx)
                + at((ix + 1).min(self.w - 1), (iy + 1).min(self.h - 1), c) * fx;
            (a * (1.0 - fy) + b * fy) / 255.0
        })
    }
}

fn render(cr: &Context, key: &Key, background: Option<&ImageSurface>) -> Option<Artwork> {
    let [width, height, center, scale] = key.geometry.map(f64::from_bits);
    let GlyphMask {
        alpha,
        w,
        h,
        bounds,
        path,
    } = mask(cr, &key.font, &key.time, width, center, scale)?;
    let factor = samples(scale);
    let raster = scale * factor as f64;
    let d = distances(&alpha, w, h);
    // Smooth the normals separately from coverage: a discrete distance field
    // otherwise makes specular highlights sparkle in little stair steps.
    let smooth = smooth_distance(&d, w, h);
    let backdrop = background.and_then(|i| Backdrop::new(i, width, height));
    let mut bytes = vec![0; w * h * 4];
    let step = (raster * 0.4).round().clamp(1.0, 3.0) as usize;
    for y in step..h - step {
        for x in step..w - step {
            let i = y * w + x;
            if alpha[i] == 0 {
                continue;
            }
            let dx = f64::from(smooth[i + step] - smooth[i - step]);
            let dy = f64::from(smooth[i + w * step] - smooth[i - w * step]);
            let length = dx.hypot(dy).max(0.001);
            let (nx, ny) = (-dx / length, -dy / length);
            let depth = f64::from(smooth[i]).max(0.0) / raster;
            let lens = (-depth / 3.4).exp();
            let sx = bounds.x + (x as f64 + 0.5) / raster + nx * lens * 14.0;
            let sy = bounds.y + (y as f64 + 0.5) / raster + ny * lens * 14.0;
            let sample = backdrop
                .as_ref()
                .map_or([0.14, 0.10, 0.08], |b| b.sample(sx, sy));
            // The sampled field supplies refraction only. Its small normal
            // errors must never be amplified into sharp specular stair steps.
            let coverage = f64::from(alpha[i]) / 255.0 * 0.86;
            for (c, sample) in sample.into_iter().enumerate() {
                let body = (sample * 0.86 + 0.14).clamp(0.0, 1.0);
                bytes[i * 4 + c] = (body * coverage * 255.0).round() as u8;
            }
            bytes[i * 4 + 3] = (coverage * 255.0).round() as u8;
        }
    }
    let styled =
        ImageSurface::create_for_data(bytes, Format::ARgb32, w as i32, h as i32, (w * 4) as i32)
            .ok()?;
    {
        // Exact Bezier contours, not a stroke reconstructed from pixel normals.
        // The glass surface shares the same curves as the supersampled mask.
        let vc = Context::new(&styled).ok()?;
        vc.set_antialias(cairo::Antialias::Best);
        vc.set_tolerance(0.01);
        vc.scale(raster, raster);
        vc.translate(-bounds.x, -bounds.y);
        vc.append_path(&path);
        vc.clip();
        let sheen = cairo::RadialGradient::new(
            bounds.x + bounds.w * 0.26,
            bounds.y + bounds.h * 0.14,
            0.0,
            bounds.x + bounds.w * 0.26,
            bounds.y + bounds.h * 0.14,
            bounds.w * 0.75,
        );
        sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.10);
        sheen.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.01);
        vc.set_source(&sheen).ok()?;
        vc.paint().ok()?;
        vc.append_path(&path);
        vc.set_line_width(1.4);
        vc.set_source_rgba(1.0, 1.0, 1.0, 0.34);
        vc.stroke().ok()?;
        for (cx, cy, radius, opacity, line_width) in [
            (0.25, -0.10, 0.68, 0.82, 2.8),
            (0.88, 1.15, 0.58, 0.62, 2.2),
        ] {
            let reflection = cairo::RadialGradient::new(
                bounds.x + bounds.w * cx,
                bounds.y + bounds.h * cy,
                0.0,
                bounds.x + bounds.w * cx,
                bounds.y + bounds.h * cy,
                bounds.w * radius,
            );
            reflection.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, opacity);
            reflection.add_color_stop_rgba(0.55, 1.0, 1.0, 1.0, opacity * 0.45);
            reflection.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
            vc.append_path(&path);
            vc.set_line_width(line_width);
            vc.set_source(&reflection).ok()?;
            vc.stroke().ok()?;
        }
    }
    // Resolve the material as well as coverage at the physical pixel size.
    // Downsampling just the alpha mask would leave jagged specular contours.
    let (ow, oh) = (w / factor, h / factor);
    let mut resolved = vec![0; ow * oh * 4];
    styled
        .with_data(|bytes| {
            for y in 0..oh {
                for x in 0..ow {
                    for c in 0..4 {
                        let mut sum = 0u32;
                        for sy in 0..factor {
                            for sx in 0..factor {
                                sum += u32::from(
                                    bytes[((y * factor + sy) * w + x * factor + sx) * 4 + c],
                                );
                            }
                        }
                        resolved[(y * ow + x) * 4 + c] =
                            ((sum * 88 + (factor * factor * 50) as u32)
                                / (factor * factor * 100) as u32) as u8;
                    }
                }
            }
        })
        .ok()?;
    Some(Artwork {
        image: ImageSurface::create_for_data(
            resolved,
            Format::ARgb32,
            ow as i32,
            oh as i32,
            (ow * 4) as i32,
        )
        .ok()?,
        bounds,
        scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supersampling_is_bounded_for_high_density_outputs() {
        for scale in [1.0, 1.5, 2.0, 3.0, 4.0] {
            let count = samples(scale);
            assert!((2..=4).contains(&count));
            assert!(scale * count as f64 <= 8.0);
        }
        assert_eq!(samples(1.5), 4);
    }
    #[test]
    fn distances_are_euclidean_not_an_eight_direction_chamfer() {
        let mut grid = vec![FAR; 81];
        grid[0] = 0.0;
        euclidean_squared(&mut grid, 9, 9);
        for y in 0..9 {
            for x in 0..9 {
                assert_eq!(grid[y * 9 + x], (x * x + y * y) as f32);
            }
        }
        let d = distances(&[0, 64, 128, 192, 255], 5, 1);
        assert!(d.windows(2).all(|v| v[0] < v[1]));
        assert!(d[1] < 0.0 && d[3] > 0.0);
    }

    #[test]
    fn digit_masks_have_smooth_coverage_at_fractional_and_integer_scales() {
        let image = ImageSurface::create(Format::ARgb32, 1, 1).unwrap();
        for scale in [1.0, 1.5, 2.0] {
            let cr = Context::new(&image).unwrap();
            cr.scale(scale, scale);
            let GlyphMask {
                alpha,
                w,
                h,
                bounds,
                ..
            } = mask(&cr, "Sans 11", "22:20", 640.0, 211.0, scale).unwrap();
            assert_eq!(alpha.len(), w * h);
            assert!(alpha.iter().filter(|a| **a > 0 && **a < 255).count() > 300);
            assert!(alpha.iter().filter(|a| **a == 255).count() > 3000);
            assert!(bounds.w < 400.0 && bounds.h < 120.0);
            assert!(alpha[..w].iter().all(|a| *a == 0));
            assert!(alpha[(h - 1) * w..].iter().all(|a| *a == 0));
            assert!(distances(&alpha, w, h).iter().all(|d| d.is_finite()));
        }
    }
    #[test]
    fn taller_clock_changes_only_glyph_height_and_keeps_the_bottom_and_width() {
        let image = ImageSurface::create(Format::ARgb32, 1, 1).unwrap();
        for scale in [1.0, 1.5, 2.0] {
            let cr = Context::new(&image).unwrap();
            cr.scale(scale, scale);
            let before =
                mask_with_ratio(&cr, "Sans 11", "22:20", 640.0, 211.0, scale, 1.0).unwrap();
            let after = mask(&cr, "Sans 11", "22:20", 640.0, 211.0, scale).unwrap();
            assert!((after.bounds.w - before.bounds.w).abs() < 0.01);
            assert_eq!(after.bounds.x, before.bounds.x);
            assert!(
                ((after.bounds.h - 5.0) / (before.bounds.h - 5.0) - CLOCK_HEIGHT_RATIO).abs()
                    < 0.005
            );
            assert!(
                (after.bounds.y + after.bounds.h - before.bounds.y - before.bounds.h).abs()
                    <= 1.0 / scale
            );
        }
    }

    #[test]
    fn glass_samples_the_existing_backdrop_instead_of_a_fixed_gray_gradient() {
        let target = ImageSurface::create(Format::ARgb32, 1, 1).unwrap();
        let cr = Context::new(&target).unwrap();
        let render_color = |rgb: (f64, f64, f64)| {
            let bg = ImageSurface::create(Format::ARgb32, 16, 12).unwrap();
            {
                let context = Context::new(&bg).unwrap();
                context.set_source_rgb(rgb.0, rgb.1, rgb.2);
                context.paint().unwrap();
            }
            let key = Key {
                font: "Sans 11".into(),
                time: "22:20".into(),
                geometry: [640.0, 480.0, 211.0, 1.5].map(f64::to_bits),
                background: 0,
            };
            let art = render(&cr, &key, Some(&bg)).unwrap();
            let mut channels = [0_u64; 3];
            art.image
                .with_data(|bytes| {
                    for p in bytes.chunks_exact(4).filter(|p| p[3] > 160 && p[3] < 195) {
                        for (c, sum) in channels.iter_mut().enumerate() {
                            *sum += u64::from(p[c]);
                        }
                    }
                })
                .unwrap();
            channels
        };
        let red = render_color((0.6, 0.1, 0.1));
        let blue = render_color((0.1, 0.1, 0.6));
        assert!(red[2] > red[0] * 3 / 2);
        assert!(blue[0] > blue[2] * 3 / 2);
    }

    #[test]
    fn typing_reuses_the_artwork_and_cache_is_bounded_per_output() {
        CACHE.with(|c| c.borrow_mut().clear());
        let image = ImageSurface::create(Format::ARgb32, 640, 480).unwrap();
        let cr = Context::new(&image).unwrap();
        paint(&cr, "Sans 11", "22:20", 640.0, 211.0, 480.0, None);
        let first = CACHE.with(|c| c.borrow()[0].art.image.clone());
        paint(&cr, "Sans 11", "22:20", 640.0, 211.0, 480.0, None);
        CACHE.with(|c| assert_eq!(c.borrow()[0].art.image.to_raw_none(), first.to_raw_none()));
        paint(&cr, "Sans 11", "22:21", 640.0, 211.0, 480.0, None);
        CACHE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.len(), 1, "old minutes must not accumulate");
            assert_eq!(c[0].key.time, "22:21");
            assert_ne!(c[0].art.image.to_raw_none(), first.to_raw_none());
        });
        for width in [641.0, 642.0, 643.0, 644.0] {
            paint(&cr, "Sans 11", "22:21", width, 211.0, 480.0, None);
        }
        CACHE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.len(), 4);
            let bytes: i32 = c
                .iter()
                .map(|e| e.art.image.stride() * e.art.image.height())
                .sum();
            assert!(
                bytes < 1024 * 1024,
                "cache must contain only small glyph crops"
            );
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    cr: &Context,
    font: &str,
    time: &str,
    width: f64,
    center: f64,
    height: f64,
    background: Option<&ImageSurface>,
) {
    let scale = cr
        .matrix()
        .xx()
        .abs()
        .max(cr.matrix().yy().abs())
        .clamp(1.0, 4.0);
    let key = Key {
        font: font.into(),
        time: time.into(),
        geometry: [width, height, center, scale].map(f64::to_bits),
        background: background.map_or(0, |s| s.to_raw_none() as usize),
    };
    let art = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.iter().find(|e| e.key == key) {
            return Some(entry.art.clone());
        }
        let art = render(cr, &key, background)?;
        cache.retain(|e| e.key.geometry != key.geometry || e.key.background != key.background);
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push(Cached {
            key,
            _background: background.cloned(),
            art: art.clone(),
        });
        Some(art)
    });
    if let Some(art) = art {
        let _ = cr.save();
        cr.translate(art.bounds.x, art.bounds.y);
        cr.scale(1.0 / art.scale, 1.0 / art.scale);
        let _ = cr.set_source_surface(&art.image, 0.0, 0.0);
        cr.source().set_filter(cairo::Filter::Bilinear);
        let _ = cr.paint();
        let _ = cr.restore();
    } else {
        draw::text(
            cr,
            font,
            time,
            Rect {
                x: 0.0,
                y: center - 104.0,
                w: width,
                h: 120.0,
            },
            false,
            true,
            96.0,
            pango::Alignment::Center,
        );
    }
}
