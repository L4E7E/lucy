//! Lucy owns the standard notification service. History stays in the login runtime directory.
use glib::variant::ToVariant;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
pub const NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/dev/lucy/Notifications";
pub const INTERFACE: &str = "dev.lucy.Notifications";
type Row = HashMap<String, glib::Variant>;
const XML: &str = r#"<node>
<interface name="org.freedesktop.Notifications">
<method name="GetCapabilities"><arg type="as" direction="out"/></method>
<method name="GetServerInformation"><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/></method>
<method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
<method name="CloseNotification"><arg type="u" direction="in"/></method>
<signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
<signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
</interface>
<interface name="dev.lucy.Notifications">
<method name="ListNotifications"><arg type="aa{sv}" direction="out"/></method>
<method name="ListHistory"><arg type="aa{sv}" direction="out"/></method>
<method name="ListModes"><arg type="as" direction="out"/></method>
<method name="SetModes"><arg type="as" direction="in"/></method>
<method name="DismissNotifications"><arg type="a{sv}" direction="in"/></method>
<method name="InvokeAction"><arg type="u" direction="in"/><arg type="s" direction="in"/></method>
</interface></node>"#;
struct Entry {
    row: Row,
    sender: String,
    timer: Option<glib::SourceId>,
    transient: bool,
}
#[allow(deprecated)]
struct State {
    bus: gio::DBusConnection,
    live: BTreeMap<u32, Entry>,
    history: BTreeMap<u32, Row>,
    modes: Vec<String>,
    next: u32,
    tx: glib::Sender<Vec<super::notifications::Notice>>,
}
fn cache_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|p| std::path::PathBuf::from(p).join("lucy-notification-history.json"))
}
fn rows_from_json(value: serde_json::Value) -> BTreeMap<u32, Row> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let id = v["id"].as_u64()? as u32;
            let row = v
                .as_object()?
                .iter()
                .filter_map(|(k, v)| {
                    let val = match k.as_str() {
                        "id" => id.to_variant(),
                        "urgency" => (v.as_u64().unwrap_or(1) as u8).to_variant(),
                        "received-at" => v.as_i64().filter(|t| *t > 0)?.to_variant(),
                        "actions" => serde_json::from_value::<HashMap<String, String>>(v.clone())
                            .ok()?
                            .to_variant(),
                        _ => v.as_str()?.to_variant(),
                    };
                    Some((k.clone(), val))
                })
                .collect();
            Some((id, row))
        })
        .collect()
}
fn cache(s: &State) {
    let Some(path) = cache_path() else { return };
    let rows: Vec<_> = s
        .history
        .values()
        .map(|row| {
            row.iter()
                .map(|(k, v)| {
                    let val = if let Some(v) = v.get::<String>() {
                        serde_json::json!(v)
                    } else if let Some(v) = v.get::<i64>() {
                        serde_json::json!(v)
                    } else if let Some(v) = v.get::<u32>() {
                        serde_json::json!(v)
                    } else if let Some(v) = v.get::<u8>() {
                        serde_json::json!(v)
                    } else if let Some(v) = v.get::<HashMap<String, String>>() {
                        serde_json::json!(v)
                    } else {
                        serde_json::Value::Null
                    };
                    (k.clone(), val)
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .collect();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("new");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
    {
        if serde_json::to_writer(&mut f, &serde_json::json!({"history":rows,"modes":s.modes}))
            .is_ok()
            && f.flush().is_ok()
        {
            let _ = std::fs::rename(tmp, path);
        }
    }
}
fn publish(s: &State) {
    let rows: Vec<_> = if s.modes.iter().any(|m| m == "do-not-disturb") {
        vec![]
    } else {
        s.live
            .values()
            .rev()
            .take(3)
            .map(|e| e.row.clone())
            .collect()
    };
    let _ =
        s.tx.send(super::notifications::parse(&(rows,).to_variant(), true));
    let _ = s.bus.emit_signal(
        None,
        PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(
            &(
                INTERFACE,
                HashMap::<String, glib::Variant>::new(),
                Vec::<String>::new(),
            )
                .to_variant(),
        ),
    );
}
fn close(s: &mut State, id: u32, reason: u32, history: bool) {
    if let Some(mut entry) = s.live.remove(&id) {
        if let Some(timer) = entry.timer.take() {
            timer.remove();
        }
        if history && !entry.transient {
            s.history.insert(id, entry.row);
        }
        if !entry.sender.is_empty() {
            let _ = s.bus.emit_signal(
                None,
                "/org/freedesktop/Notifications",
                NAME,
                "NotificationClosed",
                Some(&(id, reason).to_variant()),
            );
        }
    }
    if !history {
        s.history.remove(&id);
    }
    while s.history.len() > 50 {
        s.history.pop_first();
    }
    cache(s);
    publish(s);
}
pub struct Server {
    bus: gio::DBusConnection,
    ids: Vec<gio::RegistrationId>,
    owner: Option<gio::OwnerId>,
    state: Arc<Mutex<State>>,
}
impl Server {
    pub fn low_battery(&self, percent: u32) {
        let mut s = self.state.lock().unwrap();
        let id = s.next;
        s.next = s.next.wrapping_add(1).max(1000);
        let row = HashMap::from([
            ("id".into(), id.to_variant()),
            (
                "received-at".into(),
                (glib::real_time() / 1_000_000).to_variant(),
            ),
            ("app-name".into(), "Lucy".to_variant()),
            ("app-icon".into(), "battery-caution-symbolic".to_variant()),
            ("summary".into(), "배터리 부족".to_variant()),
            (
                "body".into(),
                format!("배터리가 {percent}% 남았습니다. 충전기를 연결해 주세요.").to_variant(),
            ),
            ("category".into(), "device.battery".to_variant()),
            ("urgency".into(), 2u8.to_variant()),
            ("desktop-entry".into(), "lucy-shell".to_variant()),
            (
                "actions".into(),
                HashMap::<String, String>::new().to_variant(),
            ),
        ]);
        s.live.insert(
            id,
            Entry {
                row,
                sender: String::new(),
                timer: None,
                transient: false,
            },
        );
        publish(&s);
    }
    #[allow(deprecated)]
    pub fn start(tx: glib::Sender<Vec<super::notifications::Notice>>) -> Result<Self, glib::Error> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
        let cached: Option<serde_json::Value> = cache_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok());
        let mut history = cached
            .as_ref()
            .map(|v| rows_from_json(v.get("history").unwrap_or(v).clone()))
            .unwrap_or_default();
        let has_owner = bus
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                Some(&(NAME,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                500,
                gio::Cancellable::NONE,
            )
            .is_ok();
        let modes = cached
            .as_ref()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v["modes"].clone()).ok())
            .or_else(|| {
                if !has_owner {
                    return None;
                }
                bus.call_sync(
                    Some(NAME),
                    "/fr/emersion/Mako",
                    "fr.emersion.Mako",
                    "ListModes",
                    None,
                    None,
                    gio::DBusCallFlags::NONE,
                    500,
                    gio::Cancellable::NONE,
                )
                .ok()
                .and_then(|v| v.child_value(0).get::<Vec<String>>())
            })
            .unwrap_or_else(|| vec!["default".into()]);
        // One-time handover: preserve Mako history, but not entries the user already deleted.
        if history.is_empty() && has_owner {
            if let Ok(v) = bus.call_sync(
                Some(NAME),
                "/fr/emersion/Mako",
                "fr.emersion.Mako",
                "ListHistory",
                None,
                None,
                gio::DBusCallFlags::NONE,
                500,
                gio::Cancellable::NONE,
            ) {
                let hidden: Vec<u32> = std::env::var_os("XDG_RUNTIME_DIR")
                    .and_then(|p| {
                        std::fs::read(
                            std::path::PathBuf::from(p).join("lucy-notification-hidden.json"),
                        )
                        .ok()
                    })
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|v| serde_json::from_value(v["ids"].clone()).ok())
                    .unwrap_or_default();
                for row in v.child_value(0).get::<Vec<Row>>().unwrap_or_default() {
                    if let Some(id) = row.get("id").and_then(|v| v.get::<u32>()) {
                        if !hidden.contains(&id) {
                            history.insert(id, row);
                        }
                    }
                }
            }
        }
        let next = history
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1000);
        let state = Arc::new(Mutex::new(State {
            bus: bus.clone(),
            live: BTreeMap::new(),
            history,
            modes,
            next,
            tx,
        }));
        cache(&state.lock().unwrap());
        let node = gio::DBusNodeInfo::for_xml(XML)?;
        let mut ids = vec![];
        for (path, name) in [("/org/freedesktop/Notifications", NAME), (PATH, INTERFACE)] {
            let shared = state.clone();
            ids.push(bus.register_object(path,&node.lookup_interface(name).unwrap(),move |_,sender,_,_,method,args,reply|{
                match method {
                    "GetCapabilities"=>reply.return_value(Some(&(vec!["actions","body","body-markup","icon-static","persistence"],).to_variant())),
                    "GetServerInformation"=>reply.return_value(Some(&("Lucy","Lucy","0.3.0","1.2").to_variant())),
                    "Notify"=>{
                        let Some((app,replace,icon,summary,body,actions,hints,timeout))=args.get::<(String,u32,String,String,String,Vec<String>,Row,i32)>() else {reply.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs","Invalid notification");return;};
                        let mut s=shared.lock().unwrap();
                        let id=if replace!=0 && s.live.contains_key(&replace){replace}else{let id=s.next;s.next=s.next.wrapping_add(1).max(1000);id};
                        if let Some(mut old)=s.live.remove(&id){if let Some(timer)=old.timer.take(){timer.remove();}}
                        let urgency=hints.get("urgency").and_then(|v|v.get::<u8>()).unwrap_or(1);
                        let category=hints.get("category").and_then(|v|v.get::<String>()).unwrap_or_default();
                        let suppress=super::notifications::poweralert(&app);
                        let action_map:HashMap<String,String>=actions.chunks_exact(2).map(|p|(p[0].clone(),p[1].clone())).collect();
                        let row=HashMap::from([("id".into(),id.to_variant()),("received-at".into(),(glib::real_time()/1_000_000).to_variant()),("app-name".into(),app.to_variant()),("app-icon".into(),icon.to_variant()),("summary".into(),summary.to_variant()),("body".into(),body.to_variant()),("category".into(),category.to_variant()),("urgency".into(),urgency.to_variant()),("desktop-entry".into(),hints.get("desktop-entry").cloned().unwrap_or_else(||"".to_variant())),("actions".into(),action_map.to_variant())]);
                        let expire=if timeout<0 {if urgency==2 {0}else{6000}}else{timeout};
                        if !suppress {
                            let timer=if expire>0 {let weak=Arc::downgrade(&shared);Some(glib::timeout_add_local_once(std::time::Duration::from_millis(expire as u64),move||{if let Some(state)=weak.upgrade(){let mut s=state.lock().unwrap();if let Some(e)=s.live.get_mut(&id){e.timer=None;}close(&mut s,id,1,true);}}))}else{None};
                            let transient=hints.get("transient").and_then(|v|v.get::<bool>()).unwrap_or(false);
                            s.live.insert(id,Entry{row,sender:sender.into(),timer,transient});publish(&s);
                        }
                        reply.return_value(Some(&(id,).to_variant()));
                    }
                    "CloseNotification"=>{if let Some((id,))=args.get::<(u32,)>(){close(&mut shared.lock().unwrap(),id,3,true);}reply.return_value(Some(&().to_variant()));}
                    "ListNotifications"=>reply.return_value(Some(&(shared.lock().unwrap().live.values().map(|e|e.row.clone()).collect::<Vec<_>>(),).to_variant())),
                    "ListHistory"=>reply.return_value(Some(&(shared.lock().unwrap().history.values().rev().cloned().collect::<Vec<_>>(),).to_variant())),
                    "ListModes"=>reply.return_value(Some(&(shared.lock().unwrap().modes.clone(),).to_variant())),
                    "SetModes"=>{if let Some((modes,))=args.get::<(Vec<String>,)>(){shared.lock().unwrap().modes=modes;cache(&shared.lock().unwrap());publish(&shared.lock().unwrap());}reply.return_value(Some(&().to_variant()));}
                    "DismissNotifications"=>{if let Some((opts,))=args.get::<(Row,)>(){if let Some(id)=opts.get("id").and_then(|v|v.get::<u32>()){close(&mut shared.lock().unwrap(),id,2,opts.get("history").and_then(|v|v.get()).unwrap_or(true));}}reply.return_value(Some(&().to_variant()));}
                    "InvokeAction"=>{
                        if let Some((id,action))=args.get::<(u32,String)>(){let mut s=shared.lock().unwrap();if let Some(entry)=s.live.get(&id){let _=s.bus.emit_signal(Some(&entry.sender),"/org/freedesktop/Notifications",NAME,"ActionInvoked",Some(&(id,action).to_variant()));}close(&mut s,id,2,true);}
                        reply.return_value(Some(&().to_variant()));
                    }
                    _=>reply.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod","Unknown method"),
                }
            },|_,_,_,_,_|false.to_variant(),|_,_,_,_,_,_|false)?);
        }
        let owner = gio::bus_own_name_on_connection(
            &bus,
            NAME,
            gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
            |_, _| eprintln!("Lucy notification service acquired"),
            |_, _| eprintln!("Lucy notification service waiting for previous owner"),
        );
        Ok(Self {
            bus,
            ids,
            owner: Some(owner),
            state,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            gio::bus_unown_name(owner);
        }
        for id in self.ids.drain(..) {
            let _ = self.bus.unregister_object(id);
        }
        let mut s = self.state.lock().unwrap();
        for (_, mut e) in std::mem::take(&mut s.live) {
            if let Some(timer) = e.timer.take() {
                timer.remove();
            }
            if !e.transient {
                s.history
                    .insert(e.row["id"].get::<u32>().unwrap_or(0), e.row);
            }
        }
        cache(&s);
    }
}
/// Only run on a private dbus-run-session bus and a disposable runtime directory.
#[allow(deprecated)]
pub fn self_test() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")?;
    if !runtime.starts_with("/tmp/lucy-notify-test.") {
        return Err("Use an isolated /tmp/lucy-notify-test.* runtime and dbus-run-session".into());
    }
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    if bus
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(NAME,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .is_ok()
    {
        return Err("A notification owner already exists; refusing to test this bus".into());
    }
    let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let snapshots = seen.clone();
    rx.attach(None, move |v| {
        snapshots.lock().unwrap().push(v);
        glib::ControlFlow::Continue
    });
    let server = Server::start(tx)?;
    let loop_ = glib::MainLoop::new(None, false);
    let finish = loop_.clone();
    let result = std::rc::Rc::new(std::cell::RefCell::new(None));
    let output = result.clone();
    let (done, done_rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    done_rx.attach(None, move |r: Result<(), String>| {
        *output.borrow_mut() = Some(r);
        finish.quit();
        glib::ControlFlow::Break
    });
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(|| -> Result<(), String> {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
                .map_err(|e| e.to_string())?;
            let call = |method: &str, args: Option<&glib::Variant>| {
                let standard = matches!(
                    method,
                    "Notify" | "GetServerInformation" | "CloseNotification"
                );
                bus.call_sync(
                    Some(NAME),
                    if standard {
                        "/org/freedesktop/Notifications"
                    } else {
                        PATH
                    },
                    if standard { NAME } else { INTERFACE },
                    method,
                    args,
                    None,
                    gio::DBusCallFlags::NONE,
                    1500,
                    gio::Cancellable::NONE,
                )
                .map_err(|e| e.to_string())
            };
            let info = call("GetServerInformation", None)?;
            assert_eq!(info.child_value(0).get::<String>().as_deref(), Some("Lucy"));
            let notify = |replace, timeout| {
                call(
                    "Notify",
                    Some(
                        &(
                            "Lucy-test",
                            replace,
                            "utilities-terminal",
                            "제목",
                            "본문",
                            vec!["default", "열기"],
                            Row::new(),
                            timeout,
                        )
                            .to_variant(),
                    ),
                )
                .map(|v| v.child_value(0).get::<u32>().unwrap())
            };
            let id = notify(0u32, 0i32)?;
            assert_eq!(notify(id, 0)?, id);
            let live = call("ListNotifications", None)?
                .child_value(0)
                .get::<Vec<Row>>()
                .unwrap();
            assert_eq!(live.len(), 1);
            let received = live[0]["received-at"]
                .get::<i64>()
                .ok_or("Missing receipt timestamp")?;
            assert!((glib::real_time() / 1_000_000 - received).abs() <= 2);
            for category in [
                "power.online",
                "power.offline",
                "power.update",
                "power.low",
                "power.critical",
            ] {
                call(
                    "Notify",
                    Some(
                        &(
                            "poweralertd",
                            0u32,
                            "battery",
                            "Power status",
                            "Current level: 80%",
                            Vec::<String>::new(),
                            HashMap::from([("category".to_owned(), category.to_variant())]),
                            0i32,
                        )
                            .to_variant(),
                    ),
                )?;
            }
            assert_eq!(
                call("ListNotifications", None)?
                    .child_value(0)
                    .get::<Vec<Row>>()
                    .unwrap()
                    .len(),
                1
            );
            call(
                "SetModes",
                Some(&(vec!["default", "do-not-disturb"],).to_variant()),
            )?;
            assert!(seen
                .lock()
                .unwrap()
                .last()
                .is_some_and(|rows: &Vec<super::notifications::Notice>| rows.is_empty()));
            call("SetModes", Some(&(vec!["default"],).to_variant()))?;
            let (action_tx, action_rx) = std::sync::mpsc::channel();
            let subscription = bus.signal_subscribe(
                None,
                Some(NAME),
                Some("ActionInvoked"),
                Some("/org/freedesktop/Notifications"),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, v| {
                    let _ = action_tx.send(v.get::<(u32, String)>());
                },
            );
            call("InvokeAction", Some(&(id, "default").to_variant()))?;
            assert_eq!(
                action_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .map_err(|e| e.to_string())?,
                Some((id, "default".into()))
            );
            bus.signal_unsubscribe(subscription);
            call(
                "DismissNotifications",
                Some(
                    &(HashMap::from([
                        ("id", id.to_variant()),
                        ("history", false.to_variant()),
                    ]),)
                        .to_variant(),
                ),
            )?;
            assert!(call("ListHistory", None)?
                .child_value(0)
                .get::<Vec<Row>>()
                .unwrap()
                .iter()
                .all(|row| row["id"].get::<u32>() != Some(id)));
            let expired = notify(0, 60)?;
            std::thread::sleep(std::time::Duration::from_millis(120));
            assert!(call("ListHistory", None)?
                .child_value(0)
                .get::<Vec<Row>>()
                .unwrap()
                .iter()
                .any(|r| r["id"].get::<u32>() == Some(expired)));
            assert!(call("ListNotifications", None)?
                .child_value(0)
                .get::<Vec<Row>>()
                .unwrap()
                .is_empty());
            Ok(())
        })
        .unwrap_or_else(|_| Err("Notification integration assertion failed".into()));
        let _ = done.send(r);
    });
    loop_.run();
    server.low_battery(19);
    {
        let mut s = server.state.lock().unwrap();
        let id = *s
            .live
            .keys()
            .next()
            .ok_or("Missing native low-battery warning")?;
        let row = &s.live[&id].row;
        assert_eq!(row["app-name"].get::<String>().as_deref(), Some("Lucy"));
        assert!(row["body"].get::<String>().unwrap().contains("19%"));
        assert_eq!(row["urgency"].get::<u8>(), Some(2));
        close(&mut s, id, 2, false);
    }
    drop(server);
    let cached: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cache_path().ok_or("No cache path")?)?)?;
    let restored = rows_from_json(cached["history"].clone());
    assert!(!restored.is_empty());
    assert!(restored.values().all(|row| row
        .get("received-at")
        .and_then(|v| v.get::<i64>())
        .is_some_and(|t| t > 0)));
    let r = result
        .borrow_mut()
        .take()
        .ok_or("No notification test result")?;
    r.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    println!("PASS: isolated Lucy notification service, replacement IDs, DND, actions, history deletion, expiration, receipt timestamps and cache restoration, poweralert suppression, native low-battery warning");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_restores_notification_types() {
        let rows = super::rows_from_json(
            serde_json::json!([{"id":1001,"app-name":"Test","body":"한글","urgency":2,"received-at":1800000000,"actions":{"default":"열기"}},{"id":1002,"app-name":"Legacy"}]),
        );
        assert_eq!(rows[&1001]["urgency"].get::<u8>(), Some(2));
        assert_eq!(rows[&1001]["body"].get::<String>().as_deref(), Some("한글"));
        assert_eq!(rows[&1001]["received-at"].get::<i64>(), Some(1_800_000_000));
        assert!(!rows[&1002].contains_key("received-at"));
    }
}
