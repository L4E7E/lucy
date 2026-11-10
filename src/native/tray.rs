//! StatusNotifier and DBusMenu, without libdbusmenu-gtk or hidden GTK windows.
use glib::variant::ToVariant;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
const NAME: &str = "org.kde.StatusNotifierWatcher";
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
<signal name="StatusNotifierHostRegistered"/></interface></node>"#;
#[derive(Clone)]
pub struct Icon {
    pub bytes: Arc<[u8]>,
    pub width: i32,
    pub height: i32,
}
#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub icon_name: String,
    pub icon: Option<Icon>,
    pub menu: String,
}
#[derive(Clone)]
pub struct MenuEntry {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
}
pub enum Event {
    Items(Vec<Item>),
    Menu(String, Vec<MenuEntry>),
}
struct State {
    bus: Option<gio::DBusConnection>,
    items: BTreeMap<String, Item>,
    events: glib::Sender<Event>,
    subscriptions: Vec<gio::SignalSubscriptionId>,
    registration: Option<gio::RegistrationId>,
}
pub struct Tray {
    state: Arc<Mutex<State>>,
    owner: Option<gio::OwnerId>,
}
fn address(s: &str) -> Option<(&str, &str)> {
    let n = s.find('/')?;
    Some((&s[..n], &s[n..]))
}
fn canonical(sender: &str, value: &str) -> String {
    if value.starts_with('/') {
        format!("{sender}{value}")
    } else {
        format!("{value}/StatusNotifierItem")
    }
}
fn publish(s: &State) {
    let _ = s
        .events
        .send(Event::Items(s.items.values().cloned().collect()));
}
fn refresh(state: &Arc<Mutex<State>>, id: &str) {
    let Some((service, path)) = address(id) else {
        return;
    };
    let Some(bus) = state.lock().unwrap().bus.clone() else {
        return;
    };
    let weak = Arc::downgrade(state);
    let id = id.to_owned();
    bus.call(
        Some(service),
        path,
        "org.freedesktop.DBus.Properties",
        "GetAll",
        Some(&(ITEM,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        1500,
        gio::Cancellable::NONE,
        move |result| {
            let (Some(state), Ok(value)) = (weak.upgrade(), result) else {
                return;
            };
            let Some((props,)) = value.get::<(HashMap<String, glib::Variant>,)>() else {
                return;
            };
            let string = |key: &str| {
                props
                    .get(key)
                    .and_then(|v| v.str())
                    .unwrap_or_default()
                    .to_owned()
            };
            let pixmaps = props
                .get("IconPixmap")
                .and_then(|v| v.get::<Vec<(i32, i32, Vec<u8>)>>());
            let icon = pixmaps.and_then(|mut pixmaps| {
                pixmaps.retain(|(w, h, p)| {
                    *w > 0
                        && *h > 0
                        && *w <= 512
                        && *h <= 512
                        && p.len() == (*w as usize) * (*h as usize) * 4
                });
                pixmaps.sort_by_key(|(w, _, _)| (w - 24).abs());
                let (w, h, src) = pixmaps.into_iter().next()?;
                let mut b = vec![0; src.len()];
                for (s, d) in src.chunks_exact(4).zip(b.chunks_exact_mut(4)) {
                    let a = u16::from(s[0]);
                    d[0] = (u16::from(s[3]) * a / 255) as u8;
                    d[1] = (u16::from(s[2]) * a / 255) as u8;
                    d[2] = (u16::from(s[1]) * a / 255) as u8;
                    d[3] = s[0];
                }
                Some(Icon {
                    bytes: b.into(),
                    width: w,
                    height: h,
                })
            });
            let icon_name = string("IconName");
            let theme_path = string("IconThemePath");
            let mut s = state.lock().unwrap();
            let icon = icon.or_else(|| {
                s.items
                    .get(&id)
                    .filter(|old| old.icon_name == icon_name)
                    .and_then(|old| old.icon.clone())
            });
            let needs_icon = icon.is_none() && !icon_name.is_empty();
            if string("Status") == "Passive" {
                s.items.remove(&id);
            } else {
                s.items.insert(
                    id.clone(),
                    Item {
                        id: id.clone(),
                        title: {
                            let title = string("Title");
                            if title.is_empty() {
                                string("Id")
                            } else {
                                title
                            }
                        },
                        icon,
                        icon_name: icon_name.clone(),
                        menu: string("Menu"),
                    },
                );
            }
            publish(&s);
            drop(s);
            if needs_icon {
                named_icon(&state, &id, &icon_name, &theme_path);
            }
        },
    );
}
#[allow(deprecated)]
fn named_icon(state: &Arc<Mutex<State>>, id: &str, name: &str, theme_path: &str) {
    let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let weak = Arc::downgrade(state);
    let item_id = id.to_owned();
    rx.attach(None, move |icon: Option<Icon>| {
        if let (Some(state), Some(icon)) = (weak.upgrade(), icon) {
            let mut state = state.lock().unwrap();
            if let Some(item) = state.items.get_mut(&item_id) {
                item.icon = Some(icon);
            }
            publish(&state);
        }
        glib::ControlFlow::Break
    });
    let name = name.to_owned();
    let theme_path = theme_path.to_owned();
    std::thread::spawn(move || {
        let icon = (|| {
            static THEME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let theme = THEME.get_or_init(|| {
                crate::bar_backend::output(
                    "gsettings",
                    &["get", "org.gnome.desktop.interface", "icon-theme"],
                )
                .map(|s| s.trim_matches('\'').to_owned())
                .unwrap_or_else(|| "Adwaita".into())
            });
            let home = std::env::var("HOME").unwrap_or_default();
            let mut roots = vec![];
            if !theme_path.is_empty() {
                roots.push(std::path::PathBuf::from(&theme_path));
            }
            for theme in [theme.as_str(), "hicolor", "Adwaita"] {
                for root in [
                    format!("{home}/.local/share/icons"),
                    format!("{home}/.icons"),
                    "/usr/share/icons".into(),
                ] {
                    roots.push(std::path::PathBuf::from(root).join(theme));
                }
            }
            roots.push(std::path::PathBuf::from("/usr/share/pixmaps"));
            let candidates = [
                format!("{name}.svg"),
                format!("{name}.png"),
                format!("{name}.symbolic.png"),
                format!("{name}.xpm"),
            ];
            let mut found = None;
            'roots: for root in roots {
                let mut stack = vec![(root, 0)];
                let mut visited = 0;
                while let Some((path, depth)) = stack.pop() {
                    let Ok(entries) = std::fs::read_dir(path) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        visited += 1;
                        if visited > 40000 {
                            break;
                        }
                        let Ok(kind) = entry.file_type() else {
                            continue;
                        };
                        if kind.is_dir() && depth < 5 && entry.file_name() != "cursors" {
                            stack.push((entry.path(), depth + 1));
                        } else if candidates.iter().any(|n| entry.file_name() == n.as_str()) {
                            found = Some(entry.path());
                            break 'roots;
                        }
                    }
                }
            }
            let path = found?;
            if std::fs::metadata(&path).ok()?.len() > 2097152 {
                return None;
            }
            let p = gdk_pixbuf::Pixbuf::from_file_at_scale(path, 32, 32, true).ok()?;
            let (w, h) = (p.width(), p.height());
            let stride = p.rowstride() as usize;
            let channels = p.n_channels() as usize;
            let src = p.read_pixel_bytes();
            let mut bytes = vec![0; (w * h * 4) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * stride + x * channels;
                    let o = (y * w as usize + x) * 4;
                    let a = if channels == 4 { src[i + 3] } else { 255 };
                    bytes[o] = (u16::from(src[i + 2]) * u16::from(a) / 255) as u8;
                    bytes[o + 1] = (u16::from(src[i + 1]) * u16::from(a) / 255) as u8;
                    bytes[o + 2] = (u16::from(src[i]) * u16::from(a) / 255) as u8;
                    bytes[o + 3] = a;
                }
            }
            Some(Icon {
                bytes: bytes.into(),
                width: w,
                height: h,
            })
        })();
        let _ = tx.send(icon);
    });
}

fn entries(node: &glib::Variant, depth: usize, out: &mut Vec<MenuEntry>) {
    if depth > 3 || out.len() > 100 || node.n_children() != 3 {
        return;
    }
    let Some(id) = node.child_value(0).get::<i32>() else {
        return;
    };
    let props = node
        .child_value(1)
        .get::<HashMap<String, glib::Variant>>()
        .unwrap_or_default();
    let enabled = props
        .get("enabled")
        .and_then(|v| v.get::<bool>())
        .unwrap_or(true);
    let visible = props
        .get("visible")
        .and_then(|v| v.get::<bool>())
        .unwrap_or(true);
    let label = props
        .get("label")
        .and_then(|v| v.str())
        .unwrap_or_default()
        .replace('_', "");
    if visible && !label.is_empty() {
        out.push(MenuEntry {
            id,
            label: format!("{}{label}", "  ".repeat(depth.saturating_sub(1))),
            enabled,
        });
    }
    if visible {
        for child in node.child_value(2).iter() {
            if let Some(child) = child.as_variant() {
                entries(&child, depth + 1, out);
            }
        }
    }
}
impl Tray {
    pub fn new(events: glib::Sender<Event>) -> Self {
        let state = Arc::new(Mutex::new(State {
            bus: None,
            items: BTreeMap::new(),
            events,
            subscriptions: vec![],
            registration: None,
        }));
        let weak = Arc::downgrade(&state);
        let owner = gio::bus_own_name(
            gio::BusType::Session,
            NAME,
            gio::BusNameOwnerFlags::NONE,
            move |bus, _| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                state.lock().unwrap().bus = Some(bus.clone());
                let names = state.clone();
                let props = state.clone();
                let interface = gio::DBusNodeInfo::for_xml(XML)
                    .unwrap()
                    .lookup_interface(NAME)
                    .unwrap();
                let registration = bus
                    .register_object(
                        PATH,
                        &interface,
                        move |bus, sender, _, _, method, args, reply| {
                            if method == "RegisterStatusNotifierItem" {
                                if let Some((value,)) = args.get::<(String,)>() {
                                    let id = canonical(sender, &value);
                                    refresh(&names, &id);
                                    let _ = bus.emit_signal(
                                        None,
                                        PATH,
                                        NAME,
                                        "StatusNotifierItemRegistered",
                                        Some(&(id,).to_variant()),
                                    );
                                }
                            }
                            reply.return_value(Some(&().to_variant()));
                        },
                        move |_, _, _, _, property| match property {
                            "RegisteredStatusNotifierItems" => props
                                .lock()
                                .unwrap()
                                .items
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .to_variant(),
                            "IsStatusNotifierHostRegistered" => true.to_variant(),
                            _ => 0i32.to_variant(),
                        },
                        |_, _, _, _, _, _| false,
                    )
                    .ok();
                state.lock().unwrap().registration = registration;
                let weak = Arc::downgrade(&state);
                let id = bus.signal_subscribe(
                    None,
                    Some(ITEM),
                    None,
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, sender, path, _, _, _| {
                        if let Some(s) = weak.upgrade() {
                            let ids: Vec<_> = s
                                .lock()
                                .unwrap()
                                .items
                                .keys()
                                .filter(|id| {
                                    address(id)
                                        .is_some_and(|(service, p)| service == sender || p == path)
                                })
                                .cloned()
                                .collect();
                            for id in ids {
                                refresh(&s, &id);
                            }
                        }
                    },
                );
                state.lock().unwrap().subscriptions.push(id);
                let weak = Arc::downgrade(&state);
                let id = bus.signal_subscribe(
                    Some("org.freedesktop.DBus"),
                    Some("org.freedesktop.DBus"),
                    Some("NameOwnerChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, args| {
                        if let (Some(s), Some((name, _, owner))) =
                            (weak.upgrade(), args.get::<(String, String, String)>())
                        {
                            if owner.is_empty() {
                                let mut s = s.lock().unwrap();
                                s.items.retain(|id, _| {
                                    address(id).is_none_or(|(service, _)| service != name)
                                });
                                publish(&s);
                            }
                        }
                    },
                );
                state.lock().unwrap().subscriptions.push(id);
                // Import live items from the previous host before the name changes owner.
                let weak = Arc::downgrade(&state);
                bus.call(
                    Some(NAME),
                    PATH,
                    "org.freedesktop.DBus.Properties",
                    "Get",
                    Some(&(NAME, "RegisteredStatusNotifierItems").to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                    move |result| {
                        if let (Some(s), Ok(v)) = (weak.upgrade(), result) {
                            if let Some((items,)) = v.get::<(glib::Variant,)>() {
                                if let Some(ids) = items.get::<Vec<String>>() {
                                    for id in ids {
                                        refresh(&s, &id);
                                    }
                                }
                            }
                        }
                    },
                );
            },
            |bus, _| {
                let _ = bus.emit_signal(
                    None,
                    PATH,
                    NAME,
                    "StatusNotifierHostRegistered",
                    Some(&().to_variant()),
                );
            },
            |_, _| {},
        );
        Self {
            state,
            owner: Some(owner),
        }
    }
    pub fn activate(&self, id: &str, menu: bool) {
        let Some((service, path)) = address(id) else {
            return;
        };
        let state = self.state.lock().unwrap();
        let Some(bus) = state.bus.clone() else {
            return;
        };
        if !menu {
            bus.call(
                Some(service),
                path,
                ITEM,
                "Activate",
                Some(&(0i32, 0i32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1500,
                gio::Cancellable::NONE,
                |_| {},
            );
            return;
        }
        let Some(item) = state.items.get(id) else {
            return;
        };
        if !item.menu.starts_with('/') {
            bus.call(
                Some(service),
                path,
                ITEM,
                "ContextMenu",
                Some(&(0i32, 0i32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1500,
                gio::Cancellable::NONE,
                |_| {},
            );
            return;
        }
        let events = state.events.clone();
        let id = id.to_owned();
        bus.call(
            Some(service),
            &item.menu,
            "com.canonical.dbusmenu",
            "AboutToShow",
            Some(&(0i32,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
        bus.call(
            Some(service),
            &item.menu,
            "com.canonical.dbusmenu",
            "GetLayout",
            Some(&(0i32, -1i32, Vec::<String>::new()).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1500,
            gio::Cancellable::NONE,
            move |result| {
                if let Ok(value) = result {
                    let mut menu = vec![];
                    entries(&value.child_value(1), 0, &mut menu);
                    let _ = events.send(Event::Menu(id, menu));
                }
            },
        );
    }
    pub fn clicked(&self, id: &str, event: i32) {
        let Some((service, _)) = address(id) else {
            return;
        };
        let state = self.state.lock().unwrap();
        let (Some(bus), Some(item)) = (state.bus.as_ref(), state.items.get(id)) else {
            return;
        };
        bus.call(
            Some(service),
            &item.menu,
            "com.canonical.dbusmenu",
            "Event",
            Some(&(event, "clicked", 0i32.to_variant(), 0u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1500,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            gio::bus_unown_name(owner);
        }
        let mut s = self.state.lock().unwrap();
        if let Some(bus) = s.bus.clone() {
            for id in s.subscriptions.drain(..) {
                bus.signal_unsubscribe(id);
            }
            if let Some(id) = s.registration.take() {
                let _ = bus.unregister_object(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn item_address_normalizes_paths() {
        assert_eq!(canonical(":1.4", "/x"), ":1.4/x");
        assert_eq!(
            address("org.example.App/Item"),
            Some(("org.example.App", "/Item"))
        );
    }
}
