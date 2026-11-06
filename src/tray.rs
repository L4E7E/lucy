//! StatusNotifier watcher/host. Item properties and activation use async D-Bus.
use crate::*;
use glib::{translate::FromGlibPtrNone, variant::ToVariant};
use gtk::gio;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/>
</interface></node>"#;

#[link(name = "dbusmenu-gtk3")]
extern "C" {
    fn dbusmenu_gtkmenu_new(
        name: *const libc::c_char,
        path: *const libc::c_char,
    ) -> *mut gtk::ffi::GtkMenu;
}

fn address(id: &str) -> Option<(&str, &str)> {
    let split = id.find('/')?;
    if split == 0 {
        return None;
    }
    Some((&id[..split], &id[split..]))
}
fn canonical(sender: &str, value: &str) -> String {
    if value.starts_with('/') {
        format!("{sender}{value}")
    } else {
        format!("{value}/StatusNotifierItem")
    }
}
#[derive(Clone)]
struct Item {
    properties: HashMap<String, glib::Variant>,
}
impl Item {
    fn string(&self, name: &str) -> String {
        self.properties
            .get(name)
            .and_then(|v| v.str())
            .unwrap_or("")
            .into()
    }
}
enum Event {
    Add(String),
    Remove(String),
    Ready(gio::DBusConnection),
}
type TrayView = (glib::WeakRef<gtk::Box>, Rc<Cell<bool>>);
pub struct Tray {
    connection: RefCell<Option<gio::DBusConnection>>,
    items: RefCell<HashMap<String, Item>>,
    pending: RefCell<HashSet<String>>,
    views: RefCell<Vec<TrayView>>,
    owner: RefCell<Option<gio::OwnerId>>,
}
impl Tray {
    #[allow(deprecated)]
    pub fn new() -> Rc<Self> {
        let tray = Rc::new(Self {
            connection: RefCell::new(None),
            items: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashSet::new()),
            views: RefCell::new(Vec::new()),
            owner: RefCell::new(None),
        });
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(&tray);
        rx.attach(None, move |event| {
            let Some(tray) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            match event {
                Event::Add(id) => tray.refresh(&id),
                Event::Remove(id) => {
                    tray.pending.borrow_mut().remove(&id);
                    tray.items.borrow_mut().remove(&id);
                    tray.render();
                }
                Event::Ready(conn) => tray.attach(conn),
            }
            ControlFlow::Continue
        });
        let registered = Arc::new(Mutex::new(Vec::<String>::new()));
        let owner = gio::bus_own_name(
            gio::BusType::Session,
            WATCHER,
            gio::BusNameOwnerFlags::NONE,
            move |conn, _| {
                let interface = gio::DBusNodeInfo::for_xml(XML)
                    .expect("valid SNI XML")
                    .lookup_interface(WATCHER)
                    .unwrap();
                let names = registered.clone();
                let props = registered.clone();
                let notify = tx.clone();
                let _ = conn.register_object(
                    PATH,
                    &interface,
                    move |conn, sender, _, _, method, args, reply| {
                        if method == "RegisterStatusNotifierItem" {
                            let Some((value,)) = args.get::<(String,)>() else {
                                reply.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    "Expected service or object path",
                                );
                                return;
                            };
                            let id = canonical(sender, &value);
                            let mut names = names.lock().unwrap();
                            if !names.contains(&id) {
                                names.push(id.clone());
                                let _ = notify.send(Event::Add(id.clone()));
                                let _ = conn.emit_signal(
                                    None,
                                    PATH,
                                    WATCHER,
                                    "StatusNotifierItemRegistered",
                                    Some(&(id,).to_variant()),
                                );
                            }
                        }
                        reply.return_value(Some(&().to_variant()));
                    },
                    move |_, _, _, _, name| match name {
                        "RegisteredStatusNotifierItems" => {
                            props.lock().unwrap().clone().to_variant()
                        }
                        "IsStatusNotifierHostRegistered" => true.to_variant(),
                        _ => 0i32.to_variant(),
                    },
                    |_, _, _, _, _, _| false,
                );
                let notify = tx.clone();
                let names = registered.clone();
                conn.signal_subscribe(
                    Some("org.freedesktop.DBus"),
                    Some("org.freedesktop.DBus"),
                    Some("NameOwnerChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |conn, _, _, _, _, args| {
                        if let Some((name, _, new)) = args.get::<(String, String, String)>() {
                            if new.is_empty() {
                                let mut names = names.lock().unwrap();
                                names.retain(|id| {
                                    if address(id).is_some_and(|(bus, _)| bus == name) {
                                        let _ = notify.send(Event::Remove(id.clone()));
                                        let _ = conn.emit_signal(
                                            None,
                                            PATH,
                                            WATCHER,
                                            "StatusNotifierItemUnregistered",
                                            Some(&(id,).to_variant()),
                                        );
                                        false
                                    } else {
                                        true
                                    }
                                });
                            }
                        }
                    },
                );
                let _ = tx.send(Event::Ready(conn));
            },
            |conn, _| {
                let _ = conn.emit_signal(None, PATH, WATCHER, "StatusNotifierHostRegistered", None);
            },
            |_, _| {},
        );
        *tray.owner.borrow_mut() = Some(owner);
        tray
    }
    fn attach(self: &Rc<Self>, conn: gio::DBusConnection) {
        *self.connection.borrow_mut() = Some(conn.clone());
        let weak = Rc::downgrade(self);
        conn.signal_subscribe(
            None,
            Some(WATCHER),
            None,
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, signal, args| {
                if let (Some(tray), Some((id,))) = (weak.upgrade(), args.get::<(String,)>()) {
                    if signal == "StatusNotifierItemRegistered" {
                        tray.refresh(&id);
                    } else if signal == "StatusNotifierItemUnregistered" {
                        tray.pending.borrow_mut().remove(&id);
                        tray.items.borrow_mut().remove(&id);
                        tray.render();
                    }
                }
            },
        );
        let weak = Rc::downgrade(self);
        conn.signal_subscribe(
            None,
            Some(ITEM),
            None,
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, path, _, _, _| {
                if let Some(tray) = weak.upgrade() {
                    let ids: Vec<_> = tray
                        .items
                        .borrow()
                        .keys()
                        .filter(|id| address(id).is_some_and(|(_, p)| p == path))
                        .cloned()
                        .collect();
                    for id in ids {
                        tray.refresh(&id);
                    }
                }
            },
        );
        let weak = Rc::downgrade(self);
        conn.call(
            Some(WATCHER),
            PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(WATCHER, "RegisteredStatusNotifierItems").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
            move |reply| {
                if let (Some(tray), Ok(reply)) = (weak.upgrade(), reply) {
                    if let Some((list,)) = reply.get::<(glib::Variant,)>() {
                        if let Some(ids) = list.get::<Vec<String>>() {
                            for id in ids {
                                tray.refresh(&id);
                            }
                        }
                    }
                }
            },
        );
        // An existing watcher may already own the name while Waybar is still running.
        conn.call(
            Some(WATCHER),
            PATH,
            WATCHER,
            "RegisterStatusNotifierHost",
            Some(
                &(conn
                    .unique_name()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),)
                    .to_variant(),
            ),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
            |_| {},
        );
    }
    fn refresh(self: &Rc<Self>, id: &str) {
        let Some((bus, path)) = address(id) else {
            return;
        };
        let Some(conn) = self.connection.borrow().clone() else {
            return;
        };
        if !self.pending.borrow_mut().insert(id.to_owned()) {
            return;
        }
        let weak = Rc::downgrade(self);
        let key = id.to_owned();
        conn.call(
            Some(bus),
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&(ITEM,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(tray) = weak.upgrade() else {
                    return;
                };
                if !tray.pending.borrow_mut().remove(&key) {
                    return;
                }
                if let Ok(reply) = result {
                    if let Some((properties,)) = reply.get::<(HashMap<String, glib::Variant>,)>() {
                        if tray
                            .items
                            .borrow()
                            .get(&key)
                            .is_some_and(|item| item.properties == properties)
                        {
                            return;
                        }
                        tray.items.borrow_mut().insert(key, Item { properties });
                        tray.render();
                    }
                }
            },
        );
    }
    pub fn widget(self: &Rc<Self>) -> gtk::Box {
        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let toggle = gtk::ToggleButton::with_label("▴");
        toggle.set_can_focus(false);
        toggle.set_tooltip_text(Some("트레이 펼치기 / 접기"));
        let reveal = gtk::Revealer::new();
        reveal.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
        reveal.set_transition_duration(200);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        reveal.add(&row);
        let weak = reveal.downgrade();
        let expanded = Rc::new(Cell::new(false));
        let active = expanded.clone();
        let tray = Rc::downgrade(self);
        toggle.connect_toggled(move |b| {
            active.set(b.is_active());
            if let Some(tray) = tray.upgrade() {
                tray.render();
            }
            if let Some(r) = weak.upgrade() {
                r.set_reveal_child(b.is_active());
                b.set_label(if b.is_active() { "▾" } else { "▴" });
            }
        });
        outer.pack_start(&toggle, false, false, 0);
        outer.pack_start(&reveal, false, false, 0);
        self.views.borrow_mut().push((row.downgrade(), expanded));
        self.render();
        outer
    }
    fn render(&self) {
        self.views.borrow_mut().retain(|(view, expanded)| {
            let Some(row) = view.upgrade() else {
                return false;
            };
            for child in row.children() {
                // Destroy detached native menus with their owner button, rather
                // than retaining their D-Bus clients until process shutdown.
                unsafe {
                    child.destroy();
                }
            }
            if !expanded.get() {
                return true;
            }
            let items = self.items.borrow();
            let mut keys: Vec<_> = items.keys().collect();
            keys.sort();
            for id in keys {
                let item = &items[id];
                if item.string("Status") == "Passive" {
                    continue;
                }
                let Some((bus, path)) = address(id) else {
                    continue;
                };
                let b = gtk::Button::new();
                b.set_can_focus(false);
                b.set_tooltip_text(Some(&item.string("Title")));
                let image = gtk::Image::new();
                let icon = item.string("IconName");
                if !icon.is_empty() {
                    image.set_icon_name(Some(&icon));
                    image.set_pixel_size(16);
                } else if let Some(pixmap) = item
                    .properties
                    .get("IconPixmap")
                    .and_then(|v| v.get::<Vec<(i32, i32, Vec<u8>)>>())
                {
                    if let Some((w, h, argb)) = pixmap
                        .into_iter()
                        .filter(|(w, h, b)| {
                            *w > 0
                                && *h > 0
                                && *w <= 512
                                && *h <= 512
                                && b.len() == (*w as usize) * (*h as usize) * 4
                        })
                        .min_by_key(|(w, _, _)| (w - 16).abs())
                    {
                        let mut rgba = argb;
                        for pixel in rgba.chunks_exact_mut(4) {
                            pixel.rotate_left(1);
                        }
                        let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                            &glib::Bytes::from_owned(rgba),
                            gtk::gdk_pixbuf::Colorspace::Rgb,
                            true,
                            8,
                            w,
                            h,
                            w * 4,
                        );
                        image.set_from_pixbuf(
                            pixbuf
                                .scale_simple(16, 16, gtk::gdk_pixbuf::InterpType::Bilinear)
                                .as_ref(),
                        );
                    }
                } else {
                    image.set_icon_name(Some("application-x-executable-symbolic"));
                    image.set_pixel_size(16);
                }
                b.set_image(Some(&image));
                b.set_always_show_image(true);
                let menu_path = item.string("Menu");
                let menu = if menu_path.starts_with('/') {
                    let name = CString::new(bus).ok();
                    let path = CString::new(menu_path).ok();
                    match (name, path) {
                        (Some(name), Some(path)) => {
                            // SAFETY: libdbusmenu returns a GtkMenu; GTK reference retained by wrapper.
                            let pointer =
                                unsafe { dbusmenu_gtkmenu_new(name.as_ptr(), path.as_ptr()) };
                            if pointer.is_null() {
                                None
                            } else {
                                Some(unsafe { gtk::Menu::from_glib_none(pointer) })
                            }
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(menu) = menu.clone() {
                    b.connect_destroy(move |_| unsafe {
                        menu.destroy();
                    });
                }
                let menu_click = menu.clone();
                let is_menu = item
                    .properties
                    .get("ItemIsMenu")
                    .and_then(|v| v.get::<bool>())
                    .unwrap_or(false);
                let conn = self.connection.borrow().clone();
                let bus = bus.to_owned();
                let path = path.to_owned();
                b.connect_clicked(move |button| {
                    if is_menu {
                        if let Some(menu) = &menu_click {
                            menu.popup_at_widget(
                                button,
                                gdk::Gravity::SouthEast,
                                gdk::Gravity::NorthEast,
                                None::<&gdk::Event>,
                            );
                            return;
                        }
                    }
                    if let Some(conn) = &conn {
                        conn.call(
                            Some(&bus),
                            &path,
                            ITEM,
                            "Activate",
                            Some(&(0i32, 0i32).to_variant()),
                            None,
                            gio::DBusCallFlags::NONE,
                            2000,
                            None::<&gio::Cancellable>,
                            |_| {},
                        );
                    }
                });
                b.connect_button_press_event(move |b, event| {
                    if event.button() == 3 {
                        if let Some(menu) = &menu {
                            menu.popup_at_widget(
                                b,
                                gdk::Gravity::SouthEast,
                                gdk::Gravity::NorthEast,
                                None::<&gdk::Event>,
                            );
                            return glib::Propagation::Stop;
                        }
                    }
                    glib::Propagation::Proceed
                });
                row.pack_start(&b, false, false, 0);
            }
            row.show_all();
            true
        });
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.get_mut().take() {
            gio::bus_unown_name(owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn item_registration_accepts_service_and_sender_path() {
        assert_eq!(canonical(":1.7", "/Tray"), ":1.7/Tray");
        assert_eq!(
            canonical(":1.7", "org.example.App"),
            "org.example.App/StatusNotifierItem"
        );
        assert_eq!(address(":1.7/Tray"), Some((":1.7", "/Tray")));
        assert_eq!(address("/Tray"), None);
    }
}
