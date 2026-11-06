use gtk::prelude::*;

pub const WIDTH: i32 = 500;
pub const HEIGHT: i32 = 600;

pub fn header(title: &str, back: impl Fn() + 'static) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let button = gtk::Button::new();
    let icon = gtk::Image::from_icon_name(Some("go-previous-symbolic"), gtk::IconSize::Button);
    icon.set_pixel_size(20);
    button.set_image(Some(&icon));
    button.set_tooltip_text(Some("뒤로가기"));
    button.connect_clicked(move |_| back());
    header.pack_start(&button, false, false, 0);
    header.pack_start(&crate::label(title, Some("title")), true, true, 0);
    header.show_all();
    header
}

/// The viewport consumes the remaining space rather than growing the popup.
pub fn prepare(card: &gtk::Box) {
    fn scrolls(widget: &gtk::Widget) {
        if let Some(scroll) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            scroll.set_min_content_height(0);
            scroll.set_propagate_natural_height(false);
            scroll.set_vexpand(true);
        }
        if let Some(container) = widget.downcast_ref::<gtk::Container>() {
            for child in container.children() {
                scrolls(&child);
            }
        }
    }
    card.set_size_request(WIDTH, HEIGHT);
    scrolls(card.upcast_ref());
}
