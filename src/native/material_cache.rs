//! Reuse a popup's unchanged glass material; never cache text, input or desktop pixels.
use cairo::{Context, Format, ImageSurface};
use std::cell::RefCell;
const BYTES: usize = 4 * 1024 * 1024;
#[derive(PartialEq)]
struct Key {
    width: i32,
    height: i32,
    scale: u64,
    antialias: cairo::Antialias,
    tolerance: u64,
}
thread_local! {
    static CACHE: RefCell<Vec<(Key, ImageSurface)>> = const { RefCell::new(Vec::new()) };
}
pub fn paint(cr: &Context, width: i32, height: i32, render: impl FnOnce(&Context)) -> bool {
    let m = cr.matrix();
    let scale = m.xx();
    // Exact physical-pixel copies only. Fractional transforms keep vector drawing.
    if m.xy() != 0.0
        || m.yx() != 0.0
        || m.yy() != scale
        || !(1.0..=3.0).contains(&scale)
        || m.x0().fract() != 0.0
        || m.y0().fract() != 0.0
        || cr.target().device_scale() != (1.0, 1.0)
        || cr.operator() != cairo::Operator::Over
        || width <= 0
        || height <= 0
    {
        return false;
    }
    let (w, h) = (
        (f64::from(width) * scale).ceil() as i32,
        (f64::from(height) * scale).ceil() as i32,
    );
    let bytes = w as usize * h as usize * 4;
    if bytes > BYTES {
        return false;
    }
    let key = Key {
        width,
        height,
        scale: scale.to_bits(),
        antialias: cr.antialias(),
        tolerance: cr.tolerance().to_bits(),
    };
    let image = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.iter().position(|(k, _)| *k == key) {
            let entry = cache.remove(index);
            let image = entry.1.clone();
            cache.push(entry);
            return Some(image);
        }
        let image = ImageSurface::create(Format::ARgb32, w, h).ok()?;
        {
            let mc = Context::new(&image).ok()?;
            mc.scale(scale, scale);
            mc.set_antialias(cr.antialias());
            mc.set_tolerance(cr.tolerance());
            render(&mc);
        }
        while !cache.is_empty()
            && (cache.len() >= 8
                || cache
                    .iter()
                    .map(|(_, s)| (s.stride() * s.height()) as usize)
                    .sum::<usize>()
                    + bytes
                    > BYTES)
        {
            cache.remove(0);
        }
        cache.push((key, image.clone()));
        Some(image)
    });
    let Some(image) = image else {
        return false;
    };
    if cr.save().is_err() {
        return false;
    }
    cr.scale(1.0 / scale, 1.0 / scale);
    let painted = cr
        .set_source_surface(&image, 0.0, 0.0)
        .and_then(|_| cr.paint())
        .is_ok();
    let _ = cr.restore();
    painted
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_material_is_reused_with_a_hard_memory_budget() {
        CACHE.with(|c| c.borrow_mut().clear());
        let image = ImageSurface::create(Format::ARgb32, 600, 500).unwrap();
        let cr = Context::new(&image).unwrap();
        let builds = std::cell::Cell::new(0);
        for _ in 0..100 {
            assert!(paint(&cr, 600, 500, |_| builds.set(builds.get() + 1)));
        }
        assert_eq!(builds.get(), 1);
        for height in 400..420 {
            assert!(paint(&cr, 600, height, |_| {}));
        }
        CACHE.with(|c| {
            let cache = c.borrow();
            assert!(cache.len() <= 8);
            assert!(
                cache
                    .iter()
                    .map(|(_, s)| (s.stride() * s.height()) as usize)
                    .sum::<usize>()
                    <= BYTES
            );
        });
        cr.translate(0.0, 0.25);
        assert!(!paint(&cr, 600, 500, |_| panic!(
            "fractional transform must use vectors"
        )));
        CACHE.with(|c| c.borrow_mut().clear());
    }
}
