//! BlueZ client and application-scoped pairing agent. No shell commands or polling.
use gio::{DBusCallFlags as Flags, DBusConnection, DBusMethodInvocation};
use glib::{variant::ObjectPath, ControlFlow, ToVariant, Variant};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Mutex, thread, time::Duration};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const AGENT: &str = "/org/lucy/BluetoothAgent";
const AGENT_XML: &str = r#"<node><interface name="org.bluez.Agent1">
<method name="Release"/><method name="Cancel"/>
<method name="RequestPinCode"><arg type="o" direction="in"/><arg type="s" direction="out"/></method>
<method name="DisplayPinCode"><arg type="o" direction="in"/><arg type="s" direction="in"/></method>
<method name="RequestPasskey"><arg type="o" direction="in"/><arg type="u" direction="out"/></method>
<method name="DisplayPasskey"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="q" direction="in"/></method>
<method name="RequestConfirmation"><arg type="o" direction="in"/><arg type="u" direction="in"/></method>
<method name="RequestAuthorization"><arg type="o" direction="in"/></method>
<method name="AuthorizeService"><arg type="o" direction="in"/><arg type="s" direction="in"/></method>
</interface></node>"#;

type Properties = HashMap<String, Variant>;
type Interfaces = HashMap<String, Properties>;
type Objects = HashMap<ObjectPath, Interfaces>;
type Shared = Rc<RefCell<Core>>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Device {
    pub path: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub battery: Option<u8>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum PromptKind {
    Pin,
    Passkey,
    Confirm(u32),
    Authorize,
    Display(String),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub id: u64,
    pub device: String,
    pub kind: PromptKind,
}
impl Prompt {
    pub fn input(&self) -> bool {
        matches!(self.kind, PromptKind::Pin | PromptKind::Passkey)
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub ready: bool,
    pub adapter: Option<String>,
    pub powered: bool,
    pub scanning: bool,
    pub devices: Vec<Device>,
    pub pending: Option<(String, String)>,
    pub prompt: Option<Prompt>,
    pub error: Option<(String, String)>,
}
impl State {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn spinning(&self) -> bool {
        self.scanning || (self.busy() && self.prompt.is_none())
    }
}
#[derive(Clone, Debug)]
pub enum Request {
    Visible(bool),
    Power(bool),
    Scan,
    Connect(String),
    Disconnect(String),
    Remove(String),
    Reply(u64, Option<String>),
    Cancel,
    Quit,
}
pub struct Backend {
    pub tx: glib::Sender<Request>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Backend {
    pub fn start(events: glib::Sender<State>, readonly: bool) -> Self {
        Self::with_address(events, readonly, None)
    }
    #[allow(deprecated)] // Match the existing GLib 0.18 event channels.
    fn with_address(events: glib::Sender<State>, readonly: bool, address: Option<String>) -> Self {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let worker = thread::Builder::new()
            .name("lucy-bluetooth".into())
            .spawn(move || {
                let context = glib::MainContext::new();
                let _ = context.with_thread_default(|| {
                    let main_loop = glib::MainLoop::new(Some(&context), false);
                    let result = address
                        .map(glib::GString::from)
                        .map_or_else(
                            || {
                                gio::dbus_address_get_for_bus_sync(
                                    gio::BusType::System,
                                    gio::Cancellable::NONE,
                                )
                            },
                            Ok,
                        )
                        .and_then(|address| {
                            DBusConnection::for_address_sync(
                                &address,
                                gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                                    | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                                None,
                                gio::Cancellable::NONE,
                            )
                        });
                    let bus = match result {
                        Ok(bus) => bus,
                        Err(_) => {
                            let _ = events.send(State {
                                ready: true,
                                error: Some((
                                    String::new(),
                                    "시스템 버스에 연결할 수 없습니다".into(),
                                )),
                                ..State::default()
                            });
                            return;
                        }
                    };
                    bus.set_exit_on_close(false);
                    let core = Rc::new(RefCell::new(Core {
                        bus: bus.clone(),
                        events,
                        objects: HashMap::new(),
                        state: State::default(),
                        last_sent: None,
                        owner: String::new(),
                        visible: false,
                        readonly,
                        agent_ready: false,
                        invocation: None,
                        serial: 0,
                        operation: 0,
                        emit_pending: false,
                        scan_owned: false,
                        scan_generation: 0,
                    }));
                    // GDBus dispatches this closure on the registering thread's context.
                    // ThreadGuard enforces that invariant without making Rc thread-safe.
                    let guard = Mutex::new(glib::thread_guard::ThreadGuard::new(core.clone()));
                    let info = gio::DBusNodeInfo::for_xml(AGENT_XML).expect("agent XML");
                    let registration = bus
                        .register_object(
                            AGENT,
                            &info
                                .lookup_interface("org.bluez.Agent1")
                                .expect("agent interface"),
                            move |_, sender, _, _, method, args, invocation| {
                                let core = guard.lock().expect("agent dispatch").get_ref().clone();
                                agent_call(&core, sender, method, &args, invocation);
                            },
                            |_, _, _, _, _| ().to_variant(),
                            |_, _, _, _, _, _| false,
                        )
                        .ok();
                    let weak = Rc::downgrade(&core);
                    let properties = bus.signal_subscribe(
                        Some(BLUEZ),
                        Some("org.freedesktop.DBus.Properties"),
                        Some("PropertiesChanged"),
                        None,
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |_, _, path, _, _, args| {
                            let Some(core) = weak.upgrade() else { return };
                            if let Some((interface, changed, invalidated)) =
                                args.get::<(String, Properties, Vec<String>)>()
                            {
                                if !matches!(
                                    interface.as_str(),
                                    ADAPTER | DEVICE | "org.bluez.Battery1"
                                ) {
                                    return;
                                }
                                let mut c = core.borrow_mut();
                                if let Ok(path) = ObjectPath::try_from(path) {
                                    let p = c
                                        .objects
                                        .entry(path)
                                        .or_default()
                                        .entry(interface)
                                        .or_default();
                                    p.extend(changed);
                                    for key in invalidated {
                                        p.remove(&key);
                                    }
                                }
                                drop(c);
                                schedule_emit(&core);
                            }
                        },
                    );
                    let weak = Rc::downgrade(&core);
                    let interfaces = bus.signal_subscribe(
                        Some(BLUEZ),
                        Some("org.freedesktop.DBus.ObjectManager"),
                        None,
                        None,
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |_, _, _, _, method, args| {
                            let Some(core) = weak.upgrade() else { return };
                            let mut c = core.borrow_mut();
                            if method == "InterfacesAdded" {
                                if let Some((path, values)) = args.get::<(ObjectPath, Interfaces)>()
                                {
                                    c.objects.entry(path).or_default().extend(values);
                                }
                            } else if let Some((path, removed)) =
                                args.get::<(ObjectPath, Vec<String>)>()
                            {
                                if let Some(values) = c.objects.get_mut(&path) {
                                    for key in removed {
                                        values.remove(&key);
                                    }
                                    if values.is_empty() {
                                        c.objects.remove(&path);
                                    }
                                }
                            }
                            drop(c);
                            schedule_emit(&core);
                        },
                    );
                    let weak = Rc::downgrade(&core);
                    let owner = bus.signal_subscribe(
                        Some("org.freedesktop.DBus"),
                        Some("org.freedesktop.DBus"),
                        Some("NameOwnerChanged"),
                        None,
                        Some(BLUEZ),
                        gio::DBusSignalFlags::NONE,
                        move |_, _, _, _, _, args| {
                            let Some(core) = weak.upgrade() else { return };
                            if let Some((_, _, owner)) = args.get::<(String, String, String)>() {
                                service_owner(&core, owner);
                            }
                        },
                    );
                    let c = core.clone();
                    call(
                        &core,
                        "org.freedesktop.DBus",
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "GetNameOwner",
                        Some((BLUEZ,).to_variant()),
                        3000,
                        move |result| {
                            service_owner(
                                &c,
                                result
                                    .ok()
                                    .and_then(|v| v.get::<(String,)>().map(|v| v.0))
                                    .unwrap_or_default(),
                            );
                        },
                    );
                    let c = core.clone();
                    let loop_clone = main_loop.clone();
                    let requests = rx.attach(Some(&context), move |message| {
                        if matches!(message, Request::Quit) {
                            reject_prompt(&mut c.borrow_mut());
                            stop_scan(&c);
                            if c.borrow().agent_ready {
                                call(
                                    &c,
                                    BLUEZ,
                                    "/org/bluez",
                                    "org.bluez.AgentManager1",
                                    "UnregisterAgent",
                                    Some((object_path(AGENT),).to_variant()),
                                    1000,
                                    |_| {},
                                );
                            }
                            loop_clone.quit();
                            return ControlFlow::Break;
                        }
                        request(&c, message);
                        ControlFlow::Continue
                    });
                    main_loop.run();
                    // The request source removes itself on Quit.
                    let _ = requests;
                    bus.signal_unsubscribe(properties);
                    bus.signal_unsubscribe(interfaces);
                    bus.signal_unsubscribe(owner);
                    if let Some(id) = registration {
                        let _ = bus.unregister_object(id);
                    }
                    let _ = bus.close_sync(gio::Cancellable::NONE);
                });
            })
            .expect("Bluetooth worker");
        Self {
            tx,
            worker: Some(worker),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Core {
    bus: DBusConnection,
    events: glib::Sender<State>,
    objects: Objects,
    state: State,
    last_sent: Option<State>,
    owner: String,
    visible: bool,
    readonly: bool,
    agent_ready: bool,
    invocation: Option<DBusMethodInvocation>,
    serial: u64,
    operation: u64,
    emit_pending: bool,
    scan_owned: bool,
    scan_generation: u64,
}
fn object_path(path: &str) -> ObjectPath {
    ObjectPath::try_from(path).expect("BlueZ object path")
}
#[allow(clippy::too_many_arguments)]
fn call(
    core: &Shared,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    args: Option<Variant>,
    timeout: i32,
    done: impl FnOnce(Result<Variant, glib::Error>) + 'static,
) {
    let bus = core.borrow().bus.clone();
    bus.call(
        Some(destination),
        path,
        interface,
        method,
        args.as_ref(),
        None,
        Flags::NONE,
        timeout,
        gio::Cancellable::NONE,
        done,
    );
}
fn publish(core: &Shared) {
    let mut c = core.borrow_mut();
    c.state = snapshot(&c.objects, &c.state);
    // RSSI/transport chatter must not repaint a stationary list or hidden panel.
    if c.last_sent.as_ref() != Some(&c.state) {
        let _ = c.events.send(c.state.clone());
        c.last_sent = Some(c.state.clone());
    }
}
fn schedule_emit(core: &Shared) {
    if core.borrow().emit_pending {
        return;
    }
    core.borrow_mut().emit_pending = true;
    local_timeout(core, Duration::from_millis(60), |c| {
        c.borrow_mut().emit_pending = false;
        publish(c);
    });
}
fn local_timeout(core: &Shared, duration: Duration, callback: impl FnOnce(&Shared) + 'static) {
    let c = core.clone();
    let mut callback = Some(callback);
    // idle/timeout_add_local attach to the global context, not this worker's context.
    // A guarded closure lets an explicit source retain the correct context.
    let guarded = Mutex::new(glib::thread_guard::ThreadGuard::new(move || {
        if let Some(callback) = callback.take() {
            callback(&c);
        }
    }));
    let source =
        glib::source::timeout_source_new(duration, None, glib::Priority::DEFAULT, move || {
            guarded.lock().expect("Bluetooth timer").get_mut()();
            ControlFlow::Break
        });
    source.attach(Some(&glib::MainContext::ref_thread_default()));
}
fn property<T: glib::variant::FromVariant>(p: &Properties, key: &str) -> Option<T> {
    p.get(key)?.get()
}
fn snapshot(objects: &Objects, previous: &State) -> State {
    let mut state = previous.clone();
    let mut adapters: Vec<_> = objects
        .iter()
        .filter(|(_, v)| v.contains_key(ADAPTER))
        .collect();
    adapters.sort_by(|(a, av), (b, bv)| {
        (
            previous.adapter.as_deref() != Some(a.as_str()),
            !property::<bool>(&av[ADAPTER], "Powered").unwrap_or(false),
            a.as_str(),
        )
            .cmp(&(
                previous.adapter.as_deref() != Some(b.as_str()),
                !property::<bool>(&bv[ADAPTER], "Powered").unwrap_or(false),
                b.as_str(),
            ))
    });
    state.adapter = adapters.first().map(|(p, _)| p.to_string());
    state.powered = adapters
        .first()
        .and_then(|(_, v)| property(&v[ADAPTER], "Powered"))
        .unwrap_or(false);
    state.devices = objects
        .iter()
        .filter_map(|(path, values)| {
            let p = values.get(DEVICE)?;
            let adapter = property::<ObjectPath>(p, "Adapter")?;
            if Some(adapter.as_str()) != state.adapter.as_deref() {
                return None;
            }
            let name = property::<String>(p, "Alias").or_else(|| property(p, "Name"))?;
            let paired = property(p, "Paired").unwrap_or(false);
            let connected = property(p, "Connected").unwrap_or(false);
            // Ignore unnamed advertisements and stale unpaired BlueZ cache entries.
            if name.trim().is_empty()
                || (!paired && !connected && property::<i16>(p, "RSSI").is_none())
            {
                return None;
            }
            Some(Device {
                path: path.to_string(),
                name,
                paired,
                connected,
                battery: values
                    .get("org.bluez.Battery1")
                    .and_then(|p| property(p, "Percentage")),
            })
        })
        .collect();
    state.devices.sort_by(|a, b| {
        (!a.connected, !a.paired, a.name.to_lowercase(), &a.path).cmp(&(
            !b.connected,
            !b.paired,
            b.name.to_lowercase(),
            &b.path,
        ))
    });
    state
}
fn service_owner(core: &Shared, owner: String) {
    {
        let mut c = core.borrow_mut();
        reject_prompt(&mut c);
        c.owner = owner.clone();
        c.operation += 1;
        c.scan_generation += 1;
        c.scan_owned = false;
        c.agent_ready = false;
        c.objects.clear();
        c.state = State {
            ready: owner.is_empty(),
            error: owner.is_empty().then(|| {
                (
                    String::new(),
                    "블루투스 서비스가 실행되지 않았습니다".into(),
                )
            }),
            ..State::default()
        };
    }
    publish(core);
    if owner.is_empty() {
        return;
    }
    let c = core.clone();
    let expected = owner.clone();
    call(
        core,
        BLUEZ,
        "/",
        "org.freedesktop.DBus.ObjectManager",
        "GetManagedObjects",
        None,
        5000,
        move |result| {
            if c.borrow().owner != expected {
                return;
            }
            let mut inner = c.borrow_mut();
            inner.state.ready = true;
            match result {
                Ok(v) => inner.objects = v.child_value(0).get().unwrap_or_default(),
                Err(e) => inner.state.error = Some((String::new(), error_text(&e))),
            }
            drop(inner);
            publish(&c);
            start_scan(&c);
        },
    );
    if !core.borrow().readonly {
        let c = core.clone();
        call(
            core,
            BLUEZ,
            "/org/bluez",
            "org.bluez.AgentManager1",
            "RegisterAgent",
            Some((object_path(AGENT), "KeyboardDisplay").to_variant()),
            5000,
            move |result| {
                if c.borrow().owner != owner {
                    return;
                }
                c.borrow_mut().agent_ready = result.is_ok();
                if let Err(e) = result {
                    c.borrow_mut().state.error = Some((String::new(), error_text(&e)));
                    publish(&c);
                }
            },
        );
    }
}
fn start_scan(core: &Shared) {
    let (adapter, token) = {
        let mut c = core.borrow_mut();
        if c.readonly || !c.visible || !c.state.powered || c.state.busy() || c.scan_owned {
            return;
        }
        let Some(adapter) = c.state.adapter.clone() else {
            return;
        };
        c.scan_generation += 1;
        c.scan_owned = true;
        c.state.scanning = true;
        c.state.error = None;
        (adapter, c.scan_generation)
    };
    publish(core);
    let c = core.clone();
    call(
        core,
        BLUEZ,
        &adapter,
        ADAPTER,
        "StartDiscovery",
        None,
        5000,
        move |result| {
            if c.borrow().scan_generation != token {
                return;
            }
            if let Err(e) = result {
                let mut inner = c.borrow_mut();
                inner.scan_owned = false;
                inner.state.scanning = false;
                inner.state.error = Some((String::new(), error_text(&e)));
                drop(inner);
                publish(&c);
            } else {
                local_timeout(&c, Duration::from_secs(12), move |c| {
                    if c.borrow().scan_generation == token {
                        stop_scan(c);
                    }
                });
            }
        },
    );
}
fn stop_scan(core: &Shared) {
    let adapter = {
        let mut c = core.borrow_mut();
        if !c.scan_owned {
            return;
        }
        c.scan_owned = false;
        c.scan_generation += 1;
        c.state.scanning = false;
        c.state.adapter.clone()
    };
    if let Some(adapter) = adapter {
        call(
            core,
            BLUEZ,
            &adapter,
            ADAPTER,
            "StopDiscovery",
            None,
            2000,
            |_| {},
        );
    }
    publish(core);
}
fn request(core: &Shared, request: Request) {
    if let Request::Visible(visible) = request {
        core.borrow_mut().visible = visible;
        if visible {
            start_scan(core);
        } else {
            stop_scan(core);
        }
        return;
    }
    if core.borrow().readonly {
        return;
    }
    let remove = matches!(&request, Request::Remove(_));
    match request {
        Request::Scan => start_scan(core),
        Request::Power(on) => {
            if core.borrow().state.busy() {
                return;
            }
            let Some(adapter) = core.borrow().state.adapter.clone() else {
                return;
            };
            if !on {
                stop_scan(core);
            }
            let token = begin(core, &adapter, if on { "켜는 중…" } else { "끄는 중…" });
            let c = core.clone();
            let path = adapter.clone();
            call(
                core,
                BLUEZ,
                &adapter,
                "org.freedesktop.DBus.Properties",
                "Set",
                Some((ADAPTER, "Powered", on.to_variant()).to_variant()),
                8000,
                move |result| {
                    if c.borrow().operation != token {
                        return;
                    }
                    if result.is_ok() {
                        if let Some(p) = c
                            .borrow_mut()
                            .objects
                            .get_mut(&object_path(&path))
                            .and_then(|v| v.get_mut(ADAPTER))
                        {
                            p.insert("Powered".into(), on.to_variant());
                        }
                    }
                    finish(&c, token, &path, result);
                    if on {
                        start_scan(&c);
                    }
                },
            );
        }
        Request::Connect(path) => {
            if core.borrow().state.busy() || !core.borrow().state.powered {
                return;
            }
            let Some(device) = core
                .borrow()
                .state
                .devices
                .iter()
                .find(|d| d.path == path)
                .cloned()
            else {
                return;
            };
            if device.connected {
                return;
            }
            if !device.paired && !core.borrow().agent_ready {
                core.borrow_mut().state.error = Some((
                    path,
                    "페어링 인증 준비 중입니다. 잠시 후 다시 시도하세요".into(),
                ));
                publish(core);
                return;
            }
            stop_scan(core);
            let token = begin(
                core,
                &path,
                if device.paired {
                    "연결 중…"
                } else {
                    "페어링 중…"
                },
            );
            if device.paired {
                connect(core, path, token);
            } else {
                let c = core.clone();
                let p = path.clone();
                call(
                    core,
                    BLUEZ,
                    &path,
                    DEVICE,
                    "Pair",
                    None,
                    90000,
                    move |result| {
                        if c.borrow().operation != token {
                            return;
                        }
                        reject_prompt(&mut c.borrow_mut());
                        match result {
                            Ok(_) => {
                                // Trust only the device the user explicitly paired in Lucy.
                                let c2 = c.clone();
                                let path2 = p.clone();
                                call(
                                    &c,
                                    BLUEZ,
                                    &p,
                                    "org.freedesktop.DBus.Properties",
                                    "Set",
                                    Some((DEVICE, "Trusted", true.to_variant()).to_variant()),
                                    5000,
                                    move |result| {
                                        if c2.borrow().operation != token {
                                            return;
                                        }
                                        if let Err(e) = result {
                                            finish(&c2, token, &path2, Err(e));
                                        } else {
                                            connect(&c2, path2, token);
                                        }
                                    },
                                );
                            }
                            Err(e) if remote_error(&e) == "org.bluez.Error.AlreadyExists" => {
                                connect(&c, p, token)
                            }
                            Err(e) => finish(&c, token, &p, Err(e)),
                        }
                    },
                );
            }
        }
        Request::Disconnect(path) | Request::Remove(path) => {
            if core.borrow().state.busy() {
                return;
            }
            let Some(device) = core
                .borrow()
                .state
                .devices
                .iter()
                .find(|d| d.path == path)
                .cloned()
            else {
                return;
            };
            // Removing a connected input device must never happen by accident.
            if remove && device.connected {
                return;
            }
            let token = begin(
                core,
                &path,
                if remove {
                    "삭제 중…"
                } else {
                    "연결 해제 중…"
                },
            );
            let c = core.clone();
            let p = path.clone();
            let Some(adapter) = core.borrow().state.adapter.clone() else {
                return;
            };
            call(
                core,
                BLUEZ,
                if remove { &adapter } else { &path },
                if remove { ADAPTER } else { DEVICE },
                if remove { "RemoveDevice" } else { "Disconnect" },
                remove.then(|| (object_path(&path),).to_variant()),
                15000,
                move |result| {
                    finish(&c, token, &p, result);
                },
            );
        }
        Request::Reply(id, value) => reply(core, id, value),
        Request::Cancel => {
            let pending = core.borrow().state.pending.clone();
            let Some((path, stage)) = pending else { return };
            reject_prompt(&mut core.borrow_mut());
            core.borrow_mut().operation += 1;
            core.borrow_mut().state.pending = None;
            if stage == "페어링 중…" {
                call(
                    core,
                    BLUEZ,
                    &path,
                    DEVICE,
                    "CancelPairing",
                    None,
                    3000,
                    |_| {},
                );
            }
            publish(core);
        }
        Request::Visible(_) | Request::Quit => {}
    }
}
fn begin(core: &Shared, path: &str, stage: &str) -> u64 {
    let mut c = core.borrow_mut();
    c.operation += 1;
    c.state.pending = Some((path.into(), stage.into()));
    c.state.error = None;
    let token = c.operation;
    drop(c);
    publish(core);
    token
}
fn connect(core: &Shared, path: String, token: u64) {
    core.borrow_mut().state.pending = Some((path.clone(), "연결 중…".into()));
    publish(core);
    let c = core.clone();
    let p = path.clone();
    call(
        core,
        BLUEZ,
        &path,
        DEVICE,
        "Connect",
        None,
        25000,
        move |result| {
            let result = match result {
                Err(e) if remote_error(&e) == "org.bluez.Error.AlreadyConnected" => {
                    Ok(().to_variant())
                }
                other => other,
            };
            finish(&c, token, &p, result);
        },
    );
}
fn finish(core: &Shared, token: u64, path: &str, result: Result<Variant, glib::Error>) {
    let mut c = core.borrow_mut();
    if c.operation != token {
        return;
    }
    reject_prompt(&mut c);
    c.state.pending = None;
    c.state.error = result.err().map(|e| (path.into(), error_text(&e)));
    drop(c);
    publish(core);
}
fn reject_prompt(c: &mut Core) {
    if let Some(invocation) = c.invocation.take() {
        invocation.return_dbus_error("org.bluez.Error.Canceled", "Pairing cancelled");
    }
    c.state.prompt = None;
}
fn agent_call(
    core: &Shared,
    sender: &str,
    method: &str,
    args: &Variant,
    invocation: DBusMethodInvocation,
) {
    let mut c = core.borrow_mut();
    if c.owner != sender || c.readonly {
        invocation.return_dbus_error("org.bluez.Error.Rejected", "Unauthorized agent caller");
        return;
    }
    if method == "Release" || method == "Cancel" {
        reject_prompt(&mut c);
        invocation.return_value(None);
        drop(c);
        publish(core);
        return;
    }
    let path = args.child_value(0).str().unwrap_or_default().to_owned();
    if c.state.pending.as_ref().is_none_or(|p| p.0 != path) {
        invocation.return_dbus_error("org.bluez.Error.Rejected", "No user-initiated connection");
        return;
    }
    if method == "AuthorizeService" {
        invocation.return_value(None);
        return;
    }
    let kind = match method {
        "RequestPinCode" => PromptKind::Pin,
        "RequestPasskey" => PromptKind::Passkey,
        "RequestConfirmation" => PromptKind::Confirm(args.child_value(1).get().unwrap_or(0)),
        "RequestAuthorization" => PromptKind::Authorize,
        "DisplayPinCode" => {
            PromptKind::Display(args.child_value(1).str().unwrap_or_default().into())
        }
        "DisplayPasskey" => PromptKind::Display(format!(
            "{:06}",
            args.child_value(1).get::<u32>().unwrap_or(0)
        )),
        _ => {
            invocation.return_dbus_error("org.bluez.Error.Rejected", "Unsupported pairing method");
            return;
        }
    };
    let display = matches!(kind, PromptKind::Display(_));
    // DisplayPasskey can repeat as the user types; retain a stable prompt id.
    if !display
        || c.state
            .prompt
            .as_ref()
            .is_none_or(|p| !matches!(p.kind, PromptKind::Display(_)))
    {
        reject_prompt(&mut c);
        c.serial += 1;
    }
    c.state.prompt = Some(Prompt {
        id: c.serial,
        device: path,
        kind,
    });
    if display {
        invocation.return_value(None);
    } else {
        c.invocation = Some(invocation);
    }
    drop(c);
    publish(core);
}
fn reply(core: &Shared, id: u64, value: Option<String>) {
    let mut c = core.borrow_mut();
    let Some(prompt) = c.state.prompt.as_ref().filter(|p| p.id == id).cloned() else {
        return;
    };
    let Some(value) = value else {
        drop(c);
        request(core, Request::Cancel);
        return;
    };
    let output = match prompt.kind {
        PromptKind::Pin
            if !value.is_empty() && value.len() <= 16 && !value.chars().any(char::is_control) =>
        {
            Some((value,).to_variant())
        }
        PromptKind::Passkey if valid_passkey(&value) => {
            Some((value.parse::<u32>().expect("validated passkey"),).to_variant())
        }
        PromptKind::Confirm(_) | PromptKind::Authorize => Some(().to_variant()),
        _ => None,
    };
    let Some(output) = output else {
        c.state.error = Some((
            prompt.device,
            "PIN은 1~16자, 숫자 인증 코드는 1~6자리로 입력하세요".into(),
        ));
        drop(c);
        publish(core);
        return;
    };
    if let Some(invocation) = c.invocation.take() {
        invocation.return_value(Some(&output));
    }
    c.state.prompt = None;
    c.state.error = None;
    drop(c);
    publish(core);
}
fn valid_passkey(value: &str) -> bool {
    !value.is_empty() && value.len() <= 6 && value.bytes().all(|b| b.is_ascii_digit())
}
fn remote_error(error: &glib::Error) -> String {
    use glib::translate::{from_glib_full, ToGlibPtr};
    // GLib returns an owned, nullable string. gio 0.18 does not expose this wrapper.
    let name: Option<glib::GString> = unsafe {
        from_glib_full(gio::ffi::g_dbus_error_get_remote_error(
            error.to_glib_none().0,
        ))
    };
    name.map(|s| s.to_string()).unwrap_or_default()
}
fn error_text(error: &glib::Error) -> String {
    let name = remote_error(error);
    if name.ends_with("AuthenticationFailed") || name.ends_with("AuthenticationRejected") {
        "인증에 실패했습니다. 기기를 페어링 모드로 바꾸고 다시 시도하세요"
    } else if name.ends_with("AuthenticationCanceled") || name.ends_with("Canceled") {
        "연결이 취소되었습니다"
    } else if name.ends_with("AuthenticationTimeout")
        || name.ends_with("NoReply")
        || error.matches(gio::IOErrorEnum::TimedOut)
    {
        "응답 시간이 초과되었습니다. 기기의 전원과 페어링 모드를 확인하세요"
    } else if name.ends_with("NotReady") {
        "블루투스가 꺼져 있거나 어댑터가 준비되지 않았습니다"
    } else if name.ends_with("NotAuthorized") || name.ends_with("AccessDenied") {
        "블루투스를 제어할 권한이 없습니다"
    } else if name.ends_with("InProgress") {
        "기기에서 다른 작업이 진행 중입니다. 잠시 후 다시 시도하세요"
    } else if name.ends_with("ServiceUnknown") {
        "블루투스 서비스가 실행되지 않았습니다"
    } else {
        "기기에 연결할 수 없습니다. 전원과 거리를 확인한 뒤 다시 시도하세요"
    }
    .into()
}

#[cfg(test)]
#[path = "bluetooth_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passkey_is_ascii_six_digits_at_most() {
        for value in ["0", "000123", "999999"] {
            assert!(valid_passkey(value));
        }
        for value in ["", "1000000", "-1", "１２３", "12 3"] {
            assert!(!valid_passkey(value));
        }
    }
    #[test]
    fn snapshot_filters_stale_devices_and_keeps_saved_names_dynamic() {
        let mut objects = Objects::new();
        objects.insert(
            object_path("/org/bluez/hci0"),
            HashMap::from([(
                ADAPTER.into(),
                HashMap::from([("Powered".into(), true.to_variant())]),
            )]),
        );
        for (index, name, paired, connected) in [
            (1, "Mouse", true, true),
            (2, "Headset", true, false),
            (3, "stale", false, false),
        ] {
            objects.insert(
                object_path(&format!("/org/bluez/hci0/dev_{index}")),
                HashMap::from([(
                    DEVICE.into(),
                    HashMap::from([
                        (
                            "Adapter".into(),
                            object_path("/org/bluez/hci0").to_variant(),
                        ),
                        ("Alias".into(), name.to_variant()),
                        ("Paired".into(), paired.to_variant()),
                        ("Connected".into(), connected.to_variant()),
                    ]),
                )]),
            );
        }
        let state = snapshot(&objects, &State::default());
        assert!(state.powered);
        assert_eq!(
            state
                .devices
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["Mouse", "Headset"]
        );
        assert!(state.devices[0].connected);
    }
}
