//! Mako notification view and controls; no second notification daemon.
use gio::prelude::*;
use glib::variant::ToVariant;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
};
pub const WIDTH: i32 = 780;
pub const HEIGHT: i32 = 500;
pub const CELL: f64 = 40.0;
pub const LIST_X: f64 = 332.0;
const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = super::notification_server::PATH;
const INTERFACE: &str = super::notification_server::INTERFACE;

#[derive(Clone, Debug, Default)]
pub struct Notice {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub body: String,
    pub active: bool,
    pub icon_name: String,
    pub actions: HashMap<String, String>,
    pub urgency: u8,
}
#[derive(Clone, Default)]
pub struct State {
    pub notices: Vec<Notice>,
    pub ready: bool,
    pub error: String,
    pub dnd: bool,
    pub icons: HashMap<String, super::tray::Icon>,
}
pub enum Command {
    Delete(Vec<u32>),
    Dnd,
    Invoke(u32, String),
}
pub fn groups(notices: &[Notice]) -> Vec<(String, Vec<&Notice>)> {
    let mut groups: Vec<(String, Vec<&Notice>)> = vec![];
    for notice in notices {
        let key = notice.app.trim().to_lowercase();
        if let Some((_, rows)) = groups.iter_mut().find(|(k, _)| *k == key) {
            rows.push(notice);
        } else {
            groups.push((key, vec![notice]));
        }
    }
    for (_, rows) in &mut groups {
        rows.sort_by_key(|n| std::cmp::Reverse(n.id));
    }
    groups.sort_by_key(|(_, rows)| std::cmp::Reverse(rows[0].id));
    groups
}
fn hidden_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|p| std::path::PathBuf::from(p).join("lucy-notification-hidden.json"))
}
fn read_hidden(owner: &str) -> HashSet<u32> {
    hidden_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .filter(|v| v["owner"].as_str() == Some(owner))
        .and_then(|v| serde_json::from_value(v["ids"].clone()).ok())
        .unwrap_or_default()
}
fn publish(s: &Shared) {
    let _ = s.tx.send(State {
        notices: merge(&s.live, &s.history)
            .into_iter()
            .filter(|n| !s.hidden.contains(&n.id))
            .collect(),
        ready: true,
        error: s.error.clone(),
        dnd: s.modes.iter().any(|m| m == "do-not-disturb"),
        icons: s.icons.clone(),
    });
}
#[allow(deprecated)]
struct Shared {
    live: Vec<Notice>,
    history: Vec<Notice>,
    generation: u64,
    pending: u8,
    queued: bool,
    error: String,
    tx: glib::Sender<State>,
    owner: String,
    hidden: HashSet<u32>,
    modes: Vec<String>,
    icons: HashMap<String, super::tray::Icon>,
    icons_requested: HashSet<String>,
}
pub struct Monitor {
    bus: Option<gio::DBusConnection>,
    ids: Vec<gio::SignalSubscriptionId>,
    _state: Rc<RefCell<Shared>>,
    _file: Option<gio::FileMonitor>,
}

pub fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in s.chars().take(8192) {
        if c == '<' {
            tag = true;
        } else if c == '>' && tag {
            tag = false;
        } else if !tag && (!c.is_control() || c == '\n') {
            out.push(c);
        }
    }
    for (encoded, decoded) in [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&amp;", "&"),
    ] {
        out = out.replace(encoded, decoded);
    }
    out
}
fn app_icon_name(desktop: &str, app: &str) -> String {
    if desktop.is_empty() {
        app.trim().to_lowercase()
    } else {
        desktop.trim_end_matches(".desktop").into()
    }
}
#[allow(deprecated)]
fn load_icons(shared: &Rc<RefCell<Shared>>) {
    let names: HashSet<String> = {
        let s = shared.borrow();
        s.live
            .iter()
            .chain(&s.history)
            .map(|n| n.icon_name.clone())
            .filter(|name| !name.is_empty() && !s.icons_requested.contains(name))
            .collect()
    };
    if names.is_empty() {
        return;
    }
    shared
        .borrow_mut()
        .icons_requested
        .extend(names.iter().cloned());
    let (tx, rx) = glib::MainContext::channel::<Vec<super::launcher::App>>(glib::Priority::DEFAULT);
    let weak = Rc::downgrade(shared);
    rx.attach(None, move |apps| {
        if let Some(shared) = weak.upgrade() {
            let mut s = shared.borrow_mut();
            for app in apps {
                if let Some(icon) = app.icon {
                    s.icons.insert(app.id, icon);
                }
            }
            publish(&s);
        }
        glib::ControlFlow::Break
    });
    std::thread::spawn(move || {
        let catalog = super::launcher::catalog();
        let mut apps: Vec<_> = names
            .into_iter()
            .map(|name| {
                let icon_name = catalog
                    .iter()
                    .find(|a| {
                        a.id.trim_end_matches(".desktop")
                            .eq_ignore_ascii_case(&name)
                            || a.name.eq_ignore_ascii_case(&name)
                    })
                    .map_or_else(|| name.clone(), |a| a.icon_name.clone());
                super::launcher::App {
                    id: name,
                    name: String::new(),
                    search: String::new(),
                    icon_name,
                    icon: None,
                }
            })
            .collect();
        super::launcher::load_icons(&mut apps);
        let _ = tx.send(apps);
    });
}
fn routine_charge(app: &str, body: &str) -> bool {
    app == "poweralertd"
        && [
            "Battery charging",
            "Battery pending charge",
            "Battery fully charged",
        ]
        .iter()
        .any(|p| body.starts_with(p))
}
pub(crate) fn parse(value: &glib::Variant, active: bool) -> Vec<Notice> {
    if value.n_children() == 0 {
        return vec![];
    }
    let rows = value
        .child_value(0)
        .get::<Vec<HashMap<String, glib::Variant>>>()
        .unwrap_or_default();
    rows.into_iter()
        .take(100)
        .filter_map(|row| {
            let id = row.get("id")?.get::<u32>()?;
            let get = |key: &str| {
                row.get(key)
                    .and_then(|v| v.get::<String>())
                    .map(|s| plain(&s))
                    .unwrap_or_default()
            };
            let (app, body) = (get("app-name"), get("body"));
            if routine_charge(&app, &body) {
                return None;
            }
            Some(Notice {
                id,
                app,
                body,
                summary: get("summary"),
                active,
                actions: row.get("actions").and_then(|v| v.get()).unwrap_or_default(),
                urgency: row.get("urgency").and_then(|v| v.get()).unwrap_or(1),
                icon_name: {
                    let icon = get("app-icon");
                    if !icon.is_empty() {
                        icon
                    } else {
                        app_icon_name(&get("desktop-entry"), &get("app-name"))
                    }
                },
            })
        })
        .collect()
}
fn merge(live: &[Notice], history: &[Notice]) -> Vec<Notice> {
    let mut all: BTreeMap<u32, Notice> = history
        .iter()
        .chain(live)
        .map(|n| (n.id, n.clone()))
        .collect();
    std::mem::take(&mut all)
        .into_values()
        .rev()
        .take(100)
        .collect()
}
#[allow(deprecated)]
fn refresh(bus: &gio::DBusConnection, state: &Rc<RefCell<Shared>>) {
    let generation = {
        let mut s = state.borrow_mut();
        s.generation = s.generation.wrapping_add(1);
        s.pending = 3;
        s.error.clear();
        s.generation
    };
    for (method, active) in [
        ("ListNotifications", true),
        ("ListHistory", false),
        ("ListModes", false),
    ] {
        let weak = Rc::downgrade(state);
        bus.call(
            Some(NAME),
            PATH,
            INTERFACE,
            method,
            None,
            None,
            gio::DBusCallFlags::NONE,
            1500,
            gio::Cancellable::NONE,
            move |reply| {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                let mut s = shared.borrow_mut();
                if s.generation != generation {
                    return;
                }
                match reply {
                    Ok(value) => {
                        if method == "ListModes" {
                            s.modes = value.child_value(0).get().unwrap_or_default();
                        } else {
                            let notices = parse(&value, active);
                            if active {
                                s.live = notices;
                            } else {
                                s.history = notices;
                            }
                        }
                    }
                    Err(_) => s.error = "알림 서비스를 읽을 수 없습니다.".into(),
                }
                s.pending = s.pending.saturating_sub(1);
                if s.pending == 0 {
                    publish(&s);
                    drop(s);
                    load_icons(&shared);
                }
            },
        );
    }
}
fn schedule(bus: &gio::DBusConnection, state: &Rc<RefCell<Shared>>) {
    if state.borrow().queued {
        return;
    }
    state.borrow_mut().queued = true;
    let bus = bus.clone();
    let weak = Rc::downgrade(state);
    glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().queued = false;
            refresh(&bus, &state);
        }
    });
}
impl Monitor {
    #[allow(deprecated)]
    pub fn start(tx: glib::Sender<State>) -> Self {
        let state = Rc::new(RefCell::new(Shared {
            live: vec![],
            history: vec![],
            generation: 0,
            pending: 0,
            queued: false,
            error: String::new(),
            tx,
            owner: String::new(),
            hidden: HashSet::new(),
            modes: vec![],
            icons: HashMap::new(),
            icons_requested: HashSet::new(),
        }));
        let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
            let _ = state.borrow().tx.send(State {
                ready: true,
                error: "알림 서비스에 연결할 수 없습니다.".into(),
                ..Default::default()
            });
            return Self {
                bus: None,
                ids: vec![],
                _state: state,
                _file: None,
            };
        };
        let owner = bus
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                Some(&(NAME,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .ok()
            .and_then(|v| v.child_value(0).get::<String>())
            .unwrap_or_default();
        {
            let mut s = state.borrow_mut();
            s.hidden = read_hidden(&owner);
            s.owner = owner;
        }
        let file = hidden_path().and_then(|path| {
            gio::File::for_path(path)
                .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
                .ok()
        });
        if let Some(file) = &file {
            let weak = Rc::downgrade(&state);
            file.connect_changed(move |_, _, _, _| {
                if let Some(state) = weak.upgrade() {
                    let mut s = state.borrow_mut();
                    s.hidden = read_hidden(&s.owner);
                    publish(&s);
                }
            });
        }
        let mut ids = vec![];
        for (interface, signal, path) in [
            ("org.freedesktop.DBus.Properties", "PropertiesChanged", PATH),
            (NAME, "NotificationClosed", "/org/freedesktop/Notifications"),
        ] {
            let weak = Rc::downgrade(&state);
            ids.push(bus.signal_subscribe(
                Some(NAME),
                Some(interface),
                Some(signal),
                Some(path),
                None,
                gio::DBusSignalFlags::NONE,
                move |bus, _, _, _, _, _| {
                    if let Some(state) = weak.upgrade() {
                        schedule(bus, &state);
                    }
                },
            ));
        }
        let weak = Rc::downgrade(&state);
        ids.push(bus.signal_subscribe(
            Some("org.freedesktop.DBus"),
            Some("org.freedesktop.DBus"),
            Some("NameOwnerChanged"),
            Some("/org/freedesktop/DBus"),
            Some(NAME),
            gio::DBusSignalFlags::NONE,
            move |bus, _, _, _, _, params| {
                if let Some(state) = weak.upgrade() {
                    {
                        let mut s = state.borrow_mut();
                        s.owner = params.child_value(2).get().unwrap_or_default();
                        s.hidden = read_hidden(&s.owner);
                    }
                    schedule(bus, &state);
                }
            },
        ));
        refresh(&bus, &state);
        Self {
            bus: Some(bus),
            ids,
            _state: state,
            _file: file,
        }
    }
    pub fn command(&self, command: Command) {
        let Some(bus) = &self.bus else {
            return;
        };
        match command {
            Command::Invoke(id, action) => {
                bus.call(
                    Some(NAME),
                    PATH,
                    INTERFACE,
                    "InvokeAction",
                    Some(&(id, action).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1500,
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
            Command::Delete(ids) => {
                let mut s = self._state.borrow_mut();
                let current = read_hidden(&s.owner);
                s.hidden.extend(current);
                s.hidden.extend(ids.iter().copied());
                if let Some(path) = hidden_path() {
                    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
                    let bytes =
                        serde_json::to_vec(&serde_json::json!({"owner": s.owner, "ids": s.hidden}))
                            .unwrap_or_default();
                    if std::fs::write(&tmp, bytes)
                        .and_then(|_| std::fs::rename(&tmp, &path))
                        .is_err()
                    {
                        s.error = "알림 삭제 상태를 저장하지 못했습니다.".into();
                    }
                }
                publish(&s);
                drop(s);
                for id in ids {
                    let opts =
                        HashMap::from([("id", id.to_variant()), ("history", false.to_variant())]);
                    let weak = Rc::downgrade(&self._state);
                    bus.call(
                        Some(NAME),
                        PATH,
                        INTERFACE,
                        "DismissNotifications",
                        Some(&(opts,).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        1500,
                        gio::Cancellable::NONE,
                        move |_| {
                            if let Some(s) = weak.upgrade() {
                                publish(&s.borrow());
                            }
                        },
                    );
                }
            }
            Command::Dnd => {
                let bus2 = bus.clone();
                let weak = Rc::downgrade(&self._state);
                bus.call(
                    Some(NAME),
                    PATH,
                    INTERFACE,
                    "ListModes",
                    None,
                    None,
                    gio::DBusCallFlags::NONE,
                    1500,
                    gio::Cancellable::NONE,
                    move |reply| {
                        let Ok(value) = reply else {
                            if let Some(s) = weak.upgrade() {
                                refresh(&bus2, &s);
                            }
                            return;
                        };
                        let mut modes: Vec<String> = value.child_value(0).get().unwrap_or_default();
                        if modes.iter().any(|m| m == "do-not-disturb") {
                            modes.retain(|m| m != "do-not-disturb");
                        } else {
                            modes.push("do-not-disturb".into());
                        }
                        let bus3 = bus2.clone();
                        bus2.call(
                            Some(NAME),
                            PATH,
                            INTERFACE,
                            "SetModes",
                            Some(&(modes,).to_variant()),
                            None,
                            gio::DBusCallFlags::NONE,
                            1500,
                            gio::Cancellable::NONE,
                            move |_| {
                                if let Some(s) = weak.upgrade() {
                                    refresh(&bus3, &s);
                                }
                            },
                        );
                    },
                );
            }
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(bus) = &self.bus {
            for id in self.ids.drain(..) {
                bus.signal_unsubscribe(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::variant::ToVariant;
    #[test]
    fn groups_are_case_insensitive_and_newest_first() {
        let rows =
            [(2, "Codex"), (5, "Mail"), (8, " codex "), (1, "Mail")].map(|(id, app)| Notice {
                id,
                app: app.into(),
                summary: String::new(),
                body: String::new(),
                active: false,
                icon_name: String::new(),
                ..Default::default()
            });
        let result = groups(&rows);
        assert_eq!(result[0].0, "codex");
        assert_eq!(
            result[0].1.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![8, 2]
        );
        assert_eq!(result[1].0, "mail");
    }
    #[test]
    fn encoded_code_in_notification_body_is_preserved() {
        assert_eq!(plain("&lt;T&gt; &amp; x\n둘째 줄"), "<T> & x\n둘째 줄");
    }
    #[test]
    fn metadata_is_parsed_and_routine_charging_history_is_filtered() {
        let rows = vec![
            HashMap::from([
                ("id".to_owned(), 1u32.to_variant()),
                ("app-name".into(), "Codex".to_variant()),
                ("summary".into(), "<b>완료</b>".to_variant()),
                ("body".into(), "안녕 &amp; 테스트".to_variant()),
            ]),
            HashMap::from([
                ("id".to_owned(), 2u32.to_variant()),
                ("app-name".into(), "poweralertd".to_variant()),
                (
                    "body".into(),
                    "Battery pending charge\nCurrent level: 80%".to_variant(),
                ),
            ]),
        ];
        let notices = parse(&(rows,).to_variant(), true);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].summary, "완료");
        assert_eq!(notices[0].body, "안녕 & 테스트");
    }
    #[test]
    fn active_and_history_are_deduplicated_newest_first() {
        let notice = |id, active| Notice {
            id,
            active,
            app: String::new(),
            summary: String::new(),
            body: String::new(),
            icon_name: String::new(),
            ..Default::default()
        };
        let result = merge(
            &[notice(3, true), notice(2, true)],
            &[notice(2, false), notice(1, false)],
        );
        assert_eq!(
            result.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert!(result[1].active);
        assert!(!routine_charge("poweralertd", "Battery critically low"));
    }
}
