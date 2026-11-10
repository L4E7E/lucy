//! UI-thread source surfaces: share immutable icons across backends, rasterize once.
use super::tray::Icon;
use cairo::{Format, ImageSurface};
use std::{cell::RefCell, sync::Weak};

const LIMIT: usize = 64;
const BYTES: usize = 2 * 1024 * 1024;
struct Entry {
    pixels: Weak<[u8]>,
    image: ImageSurface,
    width: i32,
    height: i32,
}
thread_local! {
    static CACHE: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}
pub fn get(icon: &Icon) -> Option<ImageSurface> {
    if icon.width <= 0
        || icon.height <= 0
        || icon.width > 512
        || icon.height > 512
        || icon.bytes.len() != icon.width as usize * icon.height as usize * 4
    {
        return None;
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.retain(|e| e.pixels.strong_count() != 0);
        let pixels = std::sync::Arc::downgrade(&icon.bytes);
        if let Some(index) = cache.iter().position(|e| {
            e.width == icon.width && e.height == icon.height && e.pixels.ptr_eq(&pixels)
        }) {
            let entry = cache.remove(index);
            let result = entry.image.clone();
            cache.push(entry);
            return Some(result);
        }
        // Cairo owns a mutable copy, never a pointer into shared immutable pixels.
        let image = ImageSurface::create_for_data(
            icon.bytes.to_vec(),
            Format::ARgb32,
            icon.width,
            icon.height,
            icon.width * 4,
        )
        .ok()?;
        let bytes = (image.stride() * image.height()) as usize;
        if bytes <= BYTES {
            while !cache.is_empty()
                && (cache.len() >= LIMIT
                    || cache
                        .iter()
                        .map(|e| (e.image.stride() * e.image.height()) as usize)
                        .sum::<usize>()
                        + bytes
                        > BYTES)
            {
                cache.remove(0);
            }
            cache.push(Entry {
                pixels: std::sync::Arc::downgrade(&icon.bytes),
                image: image.clone(),
                width: icon.width,
                height: icon.height,
            });
        }
        Some(image)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_pixels_reuse_one_surface_without_retaining_dead_icons() {
        CACHE.with(|c| c.borrow_mut().clear());
        let icon = Icon {
            bytes: [0, 0, 255, 255].repeat(4).into(),
            width: 2,
            height: 2,
        };
        let copy = icon.clone();
        assert!(std::sync::Arc::ptr_eq(&icon.bytes, &copy.bytes));
        let first = get(&icon).unwrap();
        assert_eq!(get(&copy).unwrap().to_raw_none(), first.to_raw_none());
        drop(icon);
        drop(copy);
        let replacement = Icon {
            bytes: [255, 0, 0, 255].repeat(4).into(),
            width: 2,
            height: 2,
        };
        let next = get(&replacement).unwrap();
        assert_ne!(first.to_raw_none(), next.to_raw_none());
        CACHE.with(|c| assert_eq!(c.borrow().len(), 1));
        let mut icons = Vec::new();
        for _ in 0..100 {
            icons.push(Icon {
                bytes: vec![255; 96 * 96 * 4].into(),
                width: 96,
                height: 96,
            });
            get(icons.last().unwrap()).unwrap();
        }
        CACHE.with(|c| {
            let c = c.borrow();
            assert!(c.len() <= LIMIT);
            assert!(
                c.iter()
                    .map(|e| (e.image.stride() * e.image.height()) as usize)
                    .sum::<usize>()
                    <= BYTES
            );
        });
        assert!(get(&Icon {
            bytes: vec![0; 3].into(),
            width: 2,
            height: 2
        })
        .is_none());
    }
}
