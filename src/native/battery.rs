//! Low-battery edge detection, fed by the bar's existing metrics subscription.
#[derive(Default)]
pub struct Watch {
    warned: bool,
}
impl Watch {
    pub fn update(&mut self, reading: Option<(u32, bool)>) -> Option<u32> {
        let (percent, plugged) = reading?;
        if percent >= 20 || plugged {
            self.warned = false;
            return None;
        }
        if self.warned {
            return None;
        }
        self.warned = true;
        Some(percent)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_warn_below_twenty_and_once_per_discharge() {
        let mut w = Watch::default();
        for percent in [80, 21, 20] {
            assert_eq!(w.update(Some((percent, false))), None);
        }
        assert_eq!(w.update(Some((19, false))), Some(19));
        for percent in [19, 18, 5] {
            assert_eq!(w.update(Some((percent, false))), None);
        }
        assert_eq!(w.update(Some((18, true))), None);
        assert_eq!(w.update(Some((18, false))), Some(18));
        assert_eq!(w.update(Some((20, false))), None);
        assert_eq!(w.update(Some((19, false))), Some(19));
    }
    #[test]
    fn missing_battery_never_becomes_zero_percent_or_rearms_a_warning() {
        let mut w = Watch::default();
        assert_eq!(w.update(None), None);
        assert_eq!(w.update(Some((19, false))), Some(19));
        assert_eq!(w.update(None), None);
        assert_eq!(w.update(Some((18, false))), None);
    }
}
