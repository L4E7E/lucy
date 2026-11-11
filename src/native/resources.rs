//! Return unused allocator pages only after the UI has settled; keep live caches.
use std::time::{Duration, Instant};

const MIN_FREE: usize = 4 * 1024 * 1024;
const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct IdleReclaim {
    visible: bool,
    pending: bool,
    last: Option<Instant>,
    pub runs: u64,
    pub last_micros: u64,
}
impl IdleReclaim {
    pub fn observe(&mut self, visible: bool) -> bool {
        self.pending |= self.visible && !visible;
        self.visible = visible;
        self.pending && !visible
    }
    fn eligible(&self, now: Instant, free: usize) -> bool {
        self.pending
            && !self.visible
            && free >= MIN_FREE
            && self
                .last
                .is_none_or(|last| now.duration_since(last) >= COOLDOWN)
    }
    pub fn run(&mut self, quiet: bool) {
        if !quiet {
            return;
        }
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            let now = Instant::now();
            // This releases only pages already free in glibc, never live icon,
            // glyph, animation or GL resources. No cache-quality tradeoff.
            let free = unsafe { libc::mallinfo2().fordblks };
            if self.eligible(now, free) {
                unsafe { libc::malloc_trim(0) };
                self.last = Some(now);
                self.runs += 1;
                self.last_micros = now.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            }
        }
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reclaim_waits_for_close_and_never_runs_during_visible_ui() {
        let mut memory = IdleReclaim::default();
        let now = Instant::now();
        assert!(!memory.observe(false));
        assert!(!memory.observe(true));
        assert!(!memory.eligible(now, MIN_FREE));
        assert!(memory.observe(false));
        assert!(!memory.eligible(now, MIN_FREE - 1));
        assert!(memory.eligible(now, MIN_FREE));
        memory.run(false);
        assert!(memory.pending);
        assert!(!memory.observe(true));
        assert!(!memory.eligible(now, MIN_FREE));
    }
    #[test]
    fn repeated_closes_cannot_trim_on_every_frame_or_every_open() {
        let now = Instant::now();
        let mut memory = IdleReclaim {
            pending: true,
            last: Some(now),
            ..Default::default()
        };
        assert!(!memory.eligible(now + Duration::from_secs(29), MIN_FREE));
        assert!(memory.eligible(now + COOLDOWN, MIN_FREE));
        memory.pending = false;
        assert!(!memory.eligible(now + COOLDOWN, MIN_FREE));
    }
}
