//! Opaque, pre-blurred lock artwork. Captures never leave process memory.
use super::{draw, model::Rect};
use cairo::{Context, Format, ImageSurface};
use gdk_pixbuf::prelude::*;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const PROMPT_SIZE: f64 = 21.0;
const CAPTURE_LIMIT: u64 = 128 * 1024 * 1024;

pub struct Background {
    pub sharp: ImageSurface,
    pub blurred: ImageSurface,
}
pub fn capture(output: &str) -> Option<Background> {
    let mut child = Command::new("grim")
        // Fractional grim resampling on this GPU costs ~500 ms. Capture the
        // native buffer without PNG compression; decode/downsample only once.
        .args(["-t", "ppm", "-o", output, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pipe = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.take(CAPTURE_LIMIT + 1).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let result = rx.recv_timeout(Duration::from_millis(900));
    // Kill before joining: a stuck screenshot helper must not delay locking.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    let mut bytes = match result {
        Ok((Ok(_), bytes)) => bytes,
        _ => {
            eprintln!("Lucy lock: desktop capture timed out or failed for {output}; using opaque fallback");
            return None;
        }
    };
    if bytes.len() as u64 > CAPTURE_LIMIT {
        return None;
    }
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(|loader, width, height| {
        let scale = (960.0 / f64::from(width.max(height).max(1))).min(1.0);
        loader.set_size(
            (f64::from(width) * scale).round().max(1.0) as i32,
            (f64::from(height) * scale).round().max(1.0) as i32,
        );
    });
    let decoded = loader.write(&bytes);
    bytes.fill(0);
    decoded.ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let (w, h) = (pixbuf.width() as usize, pixbuf.height() as usize);
    let source = pixbuf.read_pixel_bytes();
    let channels = pixbuf.n_channels() as usize;
    let stride = pixbuf.rowstride() as usize;
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * channels;
            let j = (y * w + x) * 4;
            pixels[j..j + 4].copy_from_slice(&[source[i + 2], source[i + 1], source[i], 255]);
        }
    }
    let sharp =
        ImageSurface::create_for_data(pixels, Format::ARgb32, w as i32, h as i32, (w * 4) as i32)
            .ok()?;
    let factor = (192.0 / w.max(h) as f64).min(1.0);
    let (w, h) = (
        (w as f64 * factor).round().max(1.0) as usize,
        (h as f64 * factor).round().max(1.0) as usize,
    );
    let mut small = ImageSurface::create(Format::ARgb32, w as i32, h as i32).ok()?;
    {
        let cr = Context::new(&small).ok()?;
        cr.scale(factor, factor);
        cr.set_source_surface(&sharp, 0.0, 0.0).ok()?;
        cr.source().set_filter(cairo::Filter::Bilinear);
        cr.paint().ok()?;
    }
    small.flush();
    let mut pixels = small.data().ok()?.to_vec();
    // Three bounded separable box passes approximate a Gaussian blur once.
    let mut temp = vec![0; pixels.len()];
    for _ in 0..3 {
        for channel in 0..3 {
            for y in 0..h {
                for x in 0..w {
                    let lo = x.saturating_sub(5);
                    let hi = (x + 5).min(w - 1);
                    let sum: u32 = (lo..=hi)
                        .map(|v| u32::from(pixels[(y * w + v) * 4 + channel]))
                        .sum();
                    temp[(y * w + x) * 4 + channel] = (sum / (hi - lo + 1) as u32) as u8;
                }
            }
            for y in 0..h {
                for x in 0..w {
                    let lo = y.saturating_sub(5);
                    let hi = (y + 5).min(h - 1);
                    let sum: u32 = (lo..=hi)
                        .map(|v| u32::from(temp[(v * w + x) * 4 + channel]))
                        .sum();
                    pixels[(y * w + x) * 4 + channel] = (sum / (hi - lo + 1) as u32) as u8;
                }
            }
        }
    }
    let blurred =
        ImageSurface::create_for_data(pixels, Format::ARgb32, w as i32, h as i32, (w * 4) as i32)
            .ok()?;
    Some(Background { sharp, blurred })
}
pub fn field(width: f64, height: f64) -> Rect {
    Rect {
        x: (width - 320.0) / 2.0,
        y: height * 0.44 + 114.0,
        w: 320.0,
        h: 54.0,
    }
}
pub fn prompt_rect(width: f64, height: f64) -> Rect {
    let r = field(width, height);
    Rect {
        x: r.x + 18.0,
        w: r.w - 36.0,
        ..r
    }
}
pub fn prompt_mask(font: &str, scale: f64, busy: bool) -> Option<(ImageSurface, Rect)> {
    let r = prompt_rect(640.0, 480.0);
    let mut image = ImageSurface::create(
        Format::ARgb32,
        (r.w * scale).ceil() as i32,
        (r.h * scale).ceil() as i32,
    )
    .ok()?;
    {
        let cr = Context::new(&image).ok()?;
        cr.scale(scale, scale);
        draw::text(
            &cr,
            font,
            if busy {
                "확인 중…"
            } else {
                "비밀번호"
            },
            Rect {
                x: 0.0,
                y: 0.0,
                ..r
            },
            false,
            false,
            PROMPT_SIZE,
            pango::Alignment::Center,
        );
    }
    // Animate across the glyphs, not the wide, otherwise empty entry field.
    // Inspect only the fixed placeholder mask, never any password contents.
    let stride = image.stride() as usize;
    let (width, height) = (image.width() as usize, image.height() as usize);
    let (mut left, mut right) = (width, 0);
    {
        let bytes = image.data().ok()?;
        for y in 0..height {
            for x in 0..width {
                if bytes[y * stride + x * 4 + 3] != 0 {
                    left = left.min(x);
                    right = right.max(x + 1);
                }
            }
        }
    }
    if left >= right {
        return None;
    }
    let cropped =
        ImageSurface::create(Format::ARgb32, (right - left) as i32, height as i32).ok()?;
    {
        let cr = Context::new(&cropped).ok()?;
        cr.set_source_surface(&image, -(left as f64), 0.0).ok()?;
        cr.paint().ok()?;
    }
    Some((
        cropped,
        Rect {
            x: left as f64 / scale,
            y: 0.0,
            w: (right - left) as f64 / scale,
            h: r.h,
        },
    ))
}

/// A frosted, refractive treatment clipped to the digits, not another panel.
/// Uses only the existing in-memory blurred capture; no continuous effect loop.
fn glass_clock(
    cr: &Context,
    font: &str,
    time: &str,
    width: f64,
    center: f64,
    height: f64,
    background: Option<&ImageSurface>,
) {
    super::lock_glass::paint(cr, font, time, width, center, height, background);
}
#[allow(clippy::too_many_arguments)]
pub fn paint(
    cr: &Context,
    width: f64,
    height: f64,
    background: Option<&ImageSurface>,
    font: &str,
    count: usize,
    busy: bool,
    error: bool,
    preview: bool,
) {
    cr.set_source_rgb(0.08, 0.10, 0.14);
    let _ = cr.paint();
    if let Some(image) = background {
        let _ = cr.save();
        let scale = (width / f64::from(image.width())).max(height / f64::from(image.height()));
        cr.translate(
            (width - f64::from(image.width()) * scale) / 2.0,
            (height - f64::from(image.height()) * scale) / 2.0,
        );
        cr.scale(scale, scale);
        let _ = cr.set_source_surface(image, 0.0, 0.0);
        cr.source().set_filter(cairo::Filter::Bilinear);
        let _ = cr.paint();
        let _ = cr.restore();
    }
    cr.set_source_rgba(0.025, 0.035, 0.055, 0.52);
    let _ = cr.paint();
    let now = glib::DateTime::now_local().unwrap();
    let center = height * 0.44;
    glass_clock(
        cr,
        font,
        &now.format("%H:%M").unwrap(),
        width,
        center,
        height,
        background,
    );
    draw::text(
        cr,
        font,
        &format!(
            "{} {}",
            now.format("%m월 %d일").unwrap(),
            [
                "월요일",
                "화요일",
                "수요일",
                "목요일",
                "금요일",
                "토요일",
                "일요일"
            ][(now.day_of_week() - 1) as usize]
        ),
        Rect {
            x: 0.0,
            y: center + 20.0,
            w: width,
            h: 34.0,
        },
        false,
        false,
        20.0,
        pango::Alignment::Center,
    );
    let r = field(width, height);
    draw::round(cr, r, 26.0);
    let fill = cairo::LinearGradient::new(r.x, r.y, r.x + r.w, r.y + r.h);
    fill.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.15);
    fill.add_color_stop_rgba(1.0, 0.75, 0.85, 1.0, 0.055);
    let _ = cr.set_source(&fill);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.22);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    let text = if busy {
        "확인 중…".into()
    } else if count == 0 {
        "비밀번호".into()
    } else {
        "•".repeat(count.min(28))
    };
    let placeholder = busy || count == 0;
    if placeholder {
        let _ = cr.save();
        let clip = prompt_rect(width, height);
        cr.rectangle(clip.x, clip.y, clip.w, clip.h);
        cr.clip();
        cr.push_group();
    }
    draw::text(
        cr,
        font,
        &text,
        Rect {
            x: r.x + 18.0,
            y: r.y,
            w: r.w - 36.0,
            h: r.h,
        },
        false,
        false,
        if placeholder { PROMPT_SIZE } else { 17.0 },
        pango::Alignment::Center,
    );
    if placeholder {
        let _ = cr.pop_group_to_source();
        let _ = cr.paint_with_alpha(0.48);
        let _ = cr.restore();
    }
    if error {
        draw::text(
            cr,
            font,
            "비밀번호를 확인해 주세요",
            Rect {
                x: r.x - 60.0,
                y: r.y + 66.0,
                w: r.w + 120.0,
                h: 28.0,
            },
            false,
            false,
            15.0,
            pango::Alignment::Center,
        );
    }
    if preview {
        draw::text(
            cr,
            font,
            "미리보기 · 실제 잠금 아님 · Esc로 닫기",
            Rect {
                x: 0.0,
                y: height - 70.0,
                w: width,
                h: 30.0,
            },
            false,
            false,
            15.0,
            pango::Alignment::Center,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checking_label_matches_placeholder_height_and_has_its_own_shimmer_mask() {
        for scale in [1.0, 1.5, 2.0] {
            let mut heights = Vec::new();
            for busy in [false, true] {
                let (mut image, bounds) = prompt_mask("Sans 11", scale, busy).unwrap();
                assert_eq!(bounds.h, 54.0);
                let (w, h, stride) = (
                    image.width() as usize,
                    image.height() as usize,
                    image.stride() as usize,
                );
                let bytes = image.data().unwrap();
                let rows: Vec<_> = (0..h)
                    .filter(|&y| (0..w).any(|x| bytes[y * stride + x * 4 + 3] != 0))
                    .collect();
                heights.push((rows[0], *rows.last().unwrap()));
            }
            assert!(heights[0].0.abs_diff(heights[1].0) <= 1);
            assert!(heights[0].1.abs_diff(heights[1].1) <= 1);
        }
    }
    #[test]
    fn placeholder_shimmer_mask_excludes_blank_entry_margins_at_each_scale() {
        for scale in [1.0, 1.5, 2.0] {
            let (mut image, bounds) = prompt_mask("Sans 11", scale, false).unwrap();
            assert!(bounds.w > 30.0 && bounds.w < 100.0);
            assert!(bounds.x > 80.0);
            assert!((f64::from(image.width()) - bounds.w * scale).abs() < 0.001);
            let stride = image.stride() as usize;
            let (width, height) = (image.width() as usize, image.height() as usize);
            let bytes = image.data().unwrap();
            for x in [0, width - 1] {
                assert!((0..height).any(|y| bytes[y * stride + x * 4 + 3] != 0));
            }
        }
    }
    #[test]
    fn glass_is_clipped_to_clock_digits_not_a_background_plate_or_the_date() {
        let mut image = ImageSurface::create(Format::ARgb32, 640, 480).unwrap();
        {
            let cr = Context::new(&image).unwrap();
            glass_clock(&cr, "Sans 11", "22:20", 640.0, 211.0, 480.0, None);
        }
        image.flush();
        let stride = image.stride() as usize;
        let bytes = image.data().unwrap();
        let mut digits = 0;
        for y in 0..480 {
            for x in 0..640 {
                let alpha = bytes[y * stride + x * 4 + 3];
                if alpha > 0 {
                    assert!(
                        alpha < 230,
                        "clock glass must not become an opaque chrome edge"
                    );
                    digits += 1;
                    assert!((100..228).contains(&y), "glass escaped clock bounds");
                    assert!((150..490).contains(&x), "glass must follow glyphs only");
                }
            }
        }
        assert!(digits > 4000);
    }
}
