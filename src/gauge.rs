use gtk::prelude::*;

fn capsule(cr: &gtk::cairo::Context, width: f64, height: f64, value: f64) {
    fn rounded(cr: &gtk::cairo::Context, width: f64, height: f64) {
        let r = (height / 2.0).min(width / 2.0);
        cr.new_sub_path();
        for (x, y, from, to) in [
            (width - r, r, -0.5, 0.0),
            (width - r, height - r, 0.0, 0.5),
            (r, height - r, 0.5, 1.0),
            (r, r, 1.0, 1.5),
        ] {
            cr.arc(
                x,
                y,
                r,
                from * std::f64::consts::PI,
                to * std::f64::consts::PI,
            );
        }
        cr.close_path();
    }
    let height = height.min(36.0);
    let radius = height / 2.0;
    let _ = cr.save();
    cr.new_sub_path();
    cr.arc(
        width - radius,
        radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        radius,
        radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
    cr.clip();
    cr.set_source_rgba(242.0 / 255.0, 244.0 / 255.0, 247.0 / 255.0, 0.13);
    let _ = cr.paint();
    let fill_width = width * value.clamp(0.0, 100.0) / 100.0;
    if fill_width > 0.0 {
        rounded(cr, fill_width, height);
    }
    cr.set_source_rgb(142.0 / 255.0, 216.0 / 255.0, 190.0 / 255.0);
    if fill_width > 0.0 {
        let _ = cr.fill();
    }
    let _ = cr.restore();
}

/// Both states use the same filled silhouette and exact 24px canvas.
pub fn audio_icon(microphone: bool, muted: bool) -> gtk::DrawingArea {
    let image = gtk::DrawingArea::new();
    image.set_size_request(24, 24);
    image.connect_draw(move |_, cr| {
        cr.set_source_rgb(0.96, 0.98, 0.97);
        cr.set_line_width(1.8);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        if microphone {
            cr.arc(12.0, 8.0, 3.5, std::f64::consts::PI, 0.0);
            cr.arc(12.0, 12.0, 3.5, 0.0, std::f64::consts::PI);
            cr.close_path();
            let _ = cr.fill();
            cr.arc(12.0, 12.0, 6.0, 0.0, std::f64::consts::PI);
            cr.move_to(12.0, 18.0);
            cr.line_to(12.0, 21.0);
            cr.move_to(8.0, 21.0);
            cr.line_to(16.0, 21.0);
        } else {
            cr.move_to(3.0, 8.5);
            cr.line_to(7.0, 8.5);
            cr.line_to(13.0, 4.0);
            cr.line_to(13.0, 20.0);
            cr.line_to(7.0, 15.5);
            cr.line_to(3.0, 15.5);
            cr.close_path();
            let _ = cr.fill();
            if !muted {
                cr.arc(12.0, 12.0, 6.0, -0.7, 0.7);
                cr.new_sub_path();
                cr.arc(12.0, 12.0, 10.0, -0.7, 0.7);
            }
        }
        let _ = cr.stroke();
        if muted {
            cr.move_to(17.0, 9.0);
            cr.line_to(22.0, 15.0);
            cr.move_to(22.0, 9.0);
            cr.line_to(17.0, 15.0);
            let _ = cr.stroke();
        }
        glib::Propagation::Stop
    });
    image
}

pub fn mute_hint(button: &impl IsA<gtk::Widget>, muted: bool) {
    button.set_tooltip_text(Some(if muted {
        "음소거 해제"
    } else {
        "음소거"
    }));
}

/// Keep GtkRange input/accessibility while drawing a capsule without a thumb.
pub fn wrap(scale: &gtk::Scale, icon: &impl IsA<gtk::Widget>) -> gtk::Overlay {
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.style_context().add_class("capsule-scale");
    scale.connect_draw(|scale, cr| {
        let _ = cr.save();
        cr.translate(
            0.0,
            (f64::from(scale.allocated_height()) - 36.0).max(0.0) / 2.0,
        );
        capsule(cr, f64::from(scale.allocated_width()), 36.0, scale.value());
        let _ = cr.restore();
        glib::Propagation::Proceed
    });
    scale.connect_value_changed(|scale| scale.queue_draw());
    let overlay = gtk::Overlay::new();
    overlay.add(scale);
    icon.set_halign(gtk::Align::Start);
    icon.set_valign(gtk::Align::Center);
    icon.set_margin_start(10);
    icon.style_context().add_class("gauge-icon");
    overlay.add_overlay(icon);
    overlay.set_overlay_pass_through(icon, !icon.is::<gtk::Button>());
    if icon.is::<gtk::Button>() {
        icon.add_events(
            gtk::gdk::EventMask::ENTER_NOTIFY_MASK | gtk::gdk::EventMask::LEAVE_NOTIFY_MASK,
        );
        icon.connect_enter_notify_event(|widget, _| {
            if let Some(window) = widget.window() {
                window
                    .set_cursor(gtk::gdk::Cursor::from_name(&window.display(), "pointer").as_ref());
            }
            glib::Propagation::Proceed
        });
        icon.connect_leave_notify_event(|widget, _| {
            if let Some(window) = widget.window() {
                window.set_cursor(None);
            }
            glib::Propagation::Proceed
        });
    }
    overlay
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_values_stay_inside_capsule() {
        for value in [0.0, 1.0, 4.0, 8.0, 50.0, 100.0] {
            let mut surface =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 400, 36).unwrap();
            {
                let cr = gtk::cairo::Context::new(&surface).unwrap();
                capsule(&cr, 400.0, 36.0, value);
            }
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            for (x, y) in [(0, 0), (1, 1), (399, 0), (398, 1), (0, 35), (399, 35)] {
                assert_eq!(
                    data[y * stride + x * 4 + 3],
                    0,
                    "{value}% overflow at {x},{y}"
                );
            }
            if value > 0.0 {
                assert!(data[18 * stride + 3] > 0);
            }
        }
    }
}
