//! Real GDBus calls on a disposable private bus, never the system BlueZ service.
use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc},
    time::Instant,
};

const RADIO: &str = "/org/bluez/hci0";
const NEW: &str = "/org/bluez/hci0/dev_new";
const INPUT: &str = "/org/bluez/hci0/dev_input";
const XML: &str = r#"<node>
<interface name="org.freedesktop.DBus.ObjectManager"><method name="GetManagedObjects"><arg type="a{oa{sa{sv}}}" direction="out"/></method></interface>
<interface name="org.bluez.AgentManager1"><method name="RegisterAgent"><arg type="o" direction="in"/><arg type="s" direction="in"/></method><method name="UnregisterAgent"><arg type="o" direction="in"/></method></interface>
<interface name="org.bluez.Adapter1"><method name="StartDiscovery"/><method name="StopDiscovery"/><method name="RemoveDevice"><arg type="o" direction="in"/></method></interface>
<interface name="org.freedesktop.DBus.Properties"><method name="Set"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/></method></interface>
<interface name="org.bluez.Device1"><method name="Pair"/><method name="Connect"/><method name="Disconnect"/><method name="CancelPairing"/></interface>
</node>"#;
enum Control {
    Mode(&'static str, mpsc::Sender<()>),
    Quit,
}
struct Fake {
    bus: DBusConnection,
    objects: Objects,
    agent: Option<(String, String)>,
    mode: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}
impl Fake {
    fn reset(&mut self) {
        self.objects.insert(
            object_path(RADIO),
            HashMap::from([(
                ADAPTER.into(),
                HashMap::from([("Powered".into(), true.to_variant())]),
            )]),
        );
        for (path, name, paired, connected) in [
            (NEW, "Test Headset", false, false),
            (INPUT, "Test Input", true, true),
        ] {
            self.objects.insert(
                object_path(path),
                HashMap::from([(
                    DEVICE.into(),
                    HashMap::from([
                        ("Adapter".into(), object_path(RADIO).to_variant()),
                        ("Alias".into(), name.to_variant()),
                        ("Paired".into(), paired.to_variant()),
                        ("Connected".into(), connected.to_variant()),
                        ("RSSI".into(), (-45i16).to_variant()),
                    ]),
                )]),
            );
        }
    }
    fn set(&mut self, path: &str, interface: &str, key: &str, value: Variant) {
        self.objects
            .get_mut(&object_path(path))
            .unwrap()
            .get_mut(interface)
            .unwrap()
            .insert(key.into(), value.clone());
        self.bus
            .emit_signal(
                None,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(
                    &(
                        interface,
                        HashMap::from([(key, value)]),
                        Vec::<String>::new(),
                    )
                        .to_variant(),
                ),
            )
            .unwrap();
    }
}
fn fake_call(
    fake: &Rc<RefCell<Fake>>,
    sender: &str,
    path: &str,
    method: &str,
    args: Variant,
    invocation: DBusMethodInvocation,
) {
    fake.borrow()
        .log
        .lock()
        .unwrap()
        .push(format!("{method}:{path}"));
    match method {
        "GetManagedObjects" => {
            invocation.return_value(Some(&(fake.borrow().objects.clone(),).to_variant()))
        }
        "RegisterAgent" => {
            fake.borrow_mut().agent =
                Some((sender.into(), args.child_value(0).str().unwrap().into()));
            invocation.return_value(None);
        }
        "Set" => {
            let (interface, key, value) = args.get::<(String, String, Variant)>().unwrap();
            fake.borrow_mut().set(path, &interface, &key, value);
            invocation.return_value(None);
        }
        "Connect" | "Disconnect" => {
            if fake.borrow().mode == "connect-fail" && method == "Connect" {
                invocation.return_dbus_error("org.bluez.Error.ConnectionAttemptFailed", "offline");
            } else {
                fake.borrow_mut().set(
                    path,
                    DEVICE,
                    "Connected",
                    (method == "Connect").to_variant(),
                );
                invocation.return_value(None);
            }
        }
        "RemoveDevice" => {
            let path = args.child_value(0).str().unwrap().to_owned();
            fake.borrow_mut().objects.remove(&object_path(&path));
            fake.borrow()
                .bus
                .emit_signal(
                    None,
                    "/",
                    "org.freedesktop.DBus.ObjectManager",
                    "InterfacesRemoved",
                    Some(&(object_path(&path), vec![DEVICE]).to_variant()),
                )
                .unwrap();
            invocation.return_value(None);
        }
        "Pair" => {
            let f = fake.borrow();
            let (owner, agent) = f.agent.clone().expect("application pairing agent");
            let (method, args) = match f.mode {
                "passkey" => ("RequestPasskey", (object_path(path),).to_variant()),
                "confirm" => (
                    "RequestConfirmation",
                    (object_path(path), 123456u32).to_variant(),
                ),
                _ => ("RequestPinCode", (object_path(path),).to_variant()),
            };
            let bus = f.bus.clone();
            drop(f);
            let fake = fake.clone();
            let path = path.to_owned();
            bus.call(
                Some(&owner),
                &agent,
                "org.bluez.Agent1",
                method,
                Some(&args),
                None,
                Flags::NONE,
                5000,
                gio::Cancellable::NONE,
                move |result| {
                    let valid = match result {
                        Ok(value) if method == "RequestPinCode" => {
                            value.get::<(String,)>().is_some_and(|v| v.0 == "1234")
                        }
                        Ok(value) if method == "RequestPasskey" => {
                            value.get::<(u32,)>() == Some((123456,))
                        }
                        Ok(_) => true,
                        Err(_) => false,
                    };
                    if valid {
                        fake.borrow_mut()
                            .set(&path, DEVICE, "Paired", true.to_variant());
                        invocation.return_value(None);
                    } else {
                        invocation.return_dbus_error(
                            "org.bluez.Error.AuthenticationFailed",
                            "incorrect PIN",
                        );
                    }
                },
            );
        }
        _ => invocation.return_value(None),
    }
}
struct Harness {
    backend: Option<Backend>,
    source: Option<glib::SourceId>,
    context: glib::MainContext,
    state: Rc<RefCell<State>>,
    control: glib::Sender<Control>,
    worker: Option<thread::JoinHandle<()>>,
    bus: Child,
    log: Arc<Mutex<Vec<String>>>,
}
impl Harness {
    #[allow(deprecated)]
    fn new(readonly: bool) -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("private D-Bus daemon");
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        let log = Arc::new(Mutex::new(vec![]));
        let log2 = log.clone();
        let (control, commands) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let (ready, start) = mpsc::channel();
        let address2 = address.clone();
        let worker = thread::spawn(move || {
            let context = glib::MainContext::new();
            context
                .with_thread_default(|| {
                    let bus = DBusConnection::for_address_sync(
                        &address2,
                        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                        None,
                        gio::Cancellable::NONE,
                    )
                    .unwrap();
                    bus.call_sync(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "RequestName",
                        Some(&(BLUEZ, 0u32).to_variant()),
                        None,
                        Flags::NONE,
                        2000,
                        gio::Cancellable::NONE,
                    )
                    .unwrap();
                    let fake = Rc::new(RefCell::new(Fake {
                        bus: bus.clone(),
                        objects: HashMap::new(),
                        agent: None,
                        mode: "pin",
                        log: log2,
                    }));
                    fake.borrow_mut().reset();
                    let info = gio::DBusNodeInfo::for_xml(XML).unwrap();
                    let mut registrations = vec![];
                    for (path, interface) in [
                        ("/", "org.freedesktop.DBus.ObjectManager"),
                        ("/org/bluez", "org.bluez.AgentManager1"),
                        (RADIO, ADAPTER),
                        (RADIO, "org.freedesktop.DBus.Properties"),
                        (NEW, DEVICE),
                        (NEW, "org.freedesktop.DBus.Properties"),
                        (INPUT, DEVICE),
                    ] {
                        let guard = Mutex::new(glib::thread_guard::ThreadGuard::new(fake.clone()));
                        registrations.push(
                            bus.register_object(
                                path,
                                &info.lookup_interface(interface).unwrap(),
                                move |_, sender, path, _, method, args, invocation| {
                                    let fake = guard.lock().unwrap().get_ref().clone();
                                    fake_call(&fake, sender, path, method, args, invocation);
                                },
                                |_, _, _, _, _| ().to_variant(),
                                |_, _, _, _, _, _| false,
                            )
                            .unwrap(),
                        );
                    }
                    let main_loop = glib::MainLoop::new(Some(&context), false);
                    let loop2 = main_loop.clone();
                    commands.attach(Some(&context), move |command| {
                        match command {
                            Control::Quit => {
                                loop2.quit();
                                return ControlFlow::Break;
                            }
                            Control::Mode(mode, ack) => {
                                fake.borrow_mut().mode = mode;
                                fake.borrow_mut().reset();
                                let f = fake.borrow();
                                f.bus
                                    .emit_signal(
                                        None,
                                        "/",
                                        "org.freedesktop.DBus.ObjectManager",
                                        "InterfacesAdded",
                                        Some(
                                            &(
                                                object_path(NEW),
                                                f.objects[&object_path(NEW)].clone(),
                                            )
                                                .to_variant(),
                                        ),
                                    )
                                    .unwrap();
                                let _ = ack.send(());
                            }
                        }
                        ControlFlow::Continue
                    });
                    ready.send(()).unwrap();
                    main_loop.run();
                    for registration in registrations {
                        bus.unregister_object(registration).unwrap();
                    }
                    bus.close_sync(gio::Cancellable::NONE).unwrap();
                })
                .unwrap();
        });
        start.recv_timeout(Duration::from_secs(3)).unwrap();
        let context = glib::MainContext::new();
        let state = Rc::new(RefCell::new(State::default()));
        let state2 = state.clone();
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let source = receiver.attach(Some(&context), move |s| {
            *state2.borrow_mut() = s;
            ControlFlow::Continue
        });
        let backend = Backend::with_address(events, readonly, Some(address));
        Self {
            backend: Some(backend),
            source: Some(source),
            context,
            state,
            control,
            worker: Some(worker),
            bus: daemon,
            log,
        }
    }
    fn send(&self, request: Request) {
        self.backend.as_ref().unwrap().tx.send(request).unwrap();
    }
    fn wait(&self, predicate: impl Fn(&State) -> bool) -> State {
        let start = Instant::now();
        loop {
            while self.context.pending() {
                self.context.iteration(false);
            }
            let state = self.state.borrow().clone();
            if predicate(&state) {
                return state;
            }
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "Bluetooth state timeout: {state:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn mode(&self, mode: &'static str) {
        let (tx, rx) = mpsc::channel();
        self.control.send(Control::Mode(mode, tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        self.wait(|s| {
            s.devices
                .iter()
                .any(|d| d.path == NEW && !d.paired && !d.connected)
        });
    }
    fn prompt(&self) -> Prompt {
        self.wait(|s| s.prompt.is_some()).prompt.unwrap()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.backend.take());
        if let Some(source) = self.source.take() {
            self.context.find_source_by_id(&source).unwrap().destroy();
        }
        let _ = self.control.send(Control::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = self.bus.kill();
        let _ = self.bus.wait();
    }
}
#[test]
fn private_bluez_pair_connect_remove_and_authentication_lifecycle() {
    let h = Harness::new(false);
    h.wait(|s| s.ready && s.devices.len() == 2);
    // Registration is asynchronous; wait for its service call before pairing.
    let deadline = Instant::now();
    while !h
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.starts_with("RegisterAgent:"))
    {
        assert!(deadline.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(5));
    }
    h.send(Request::Visible(true));
    h.wait(|s| s.scanning);
    h.send(Request::Connect(NEW.into()));
    let prompt = h.prompt();
    assert_eq!(prompt.kind, PromptKind::Pin);
    h.send(Request::Visible(false));
    h.wait(|s| !s.scanning && s.prompt.as_ref() == Some(&prompt));
    h.send(Request::Reply(prompt.id, Some("wrong".into())));
    h.wait(|s| !s.busy() && s.error.as_ref().is_some_and(|e| e.1.contains("인증")));
    h.send(Request::Connect(NEW.into()));
    let prompt = h.prompt();
    h.send(Request::Reply(prompt.id, Some("1234".into())));
    h.wait(|s| {
        !s.busy()
            && s.devices
                .iter()
                .any(|d| d.path == NEW && d.paired && d.connected)
    });
    h.send(Request::Disconnect(NEW.into()));
    h.wait(|s| !s.busy() && s.devices.iter().any(|d| d.path == NEW && !d.connected));
    let pair_count = h
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("Pair:"))
        .count();
    h.send(Request::Connect(NEW.into()));
    h.wait(|s| !s.busy() && s.devices.iter().any(|d| d.path == NEW && d.connected));
    assert_eq!(
        h.log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("Pair:"))
            .count(),
        pair_count
    );
    h.send(Request::Remove(INPUT.into()));
    h.send(Request::Disconnect(NEW.into()));
    h.wait(|s| !s.busy() && s.devices.iter().any(|d| d.path == NEW && !d.connected));
    h.send(Request::Remove(NEW.into()));
    h.wait(|s| !s.busy() && s.devices.len() == 1);
    for (mode, kind, reply) in [
        ("passkey", PromptKind::Passkey, "123456"),
        ("confirm", PromptKind::Confirm(123456), ""),
    ] {
        h.mode(mode);
        h.send(Request::Connect(NEW.into()));
        let prompt = h.prompt();
        assert_eq!(prompt.kind, kind);
        if mode == "passkey" {
            h.send(Request::Reply(prompt.id, Some("invalid".into())));
            h.wait(|s| s.prompt.as_ref() == Some(&prompt) && s.error.is_some());
        }
        h.send(Request::Reply(prompt.id, Some(reply.into())));
        h.wait(|s| !s.busy() && s.devices.iter().any(|d| d.path == NEW && d.connected));
    }
    h.mode("pin");
    h.send(Request::Connect(NEW.into()));
    h.prompt();
    h.send(Request::Cancel);
    h.wait(|s| !s.busy() && s.prompt.is_none());
    h.send(Request::Power(false));
    h.wait(|s| !s.busy() && !s.powered);
    h.send(Request::Visible(true));
    h.send(Request::Power(true));
    h.wait(|s| !s.busy() && s.powered && s.scanning);
    h.send(Request::Visible(false));
    h.wait(|s| !s.scanning);
    assert!(h
        .state
        .borrow()
        .devices
        .iter()
        .any(|d| d.path == INPUT && d.connected));
    assert!(!h
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|s| s == &format!("Disconnect:{INPUT}") || s.contains("RequestDefaultAgent")));
}
#[test]
fn preview_never_changes_adapter_or_registers_agent() {
    let h = Harness::new(true);
    h.wait(|s| s.ready);
    h.send(Request::Visible(true));
    h.send(Request::Power(false));
    h.send(Request::Connect(NEW.into()));
    thread::sleep(Duration::from_millis(100));
    h.wait(|s| s.ready);
    let log = h.log.lock().unwrap();
    assert!(log.iter().all(|s| s.starts_with("GetManagedObjects:")));
}
