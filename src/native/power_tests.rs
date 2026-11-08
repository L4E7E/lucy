//! Disposable D-Bus services: no writes to the real laptop or power daemons.
use super::*;
use std::{
    cell::RefCell,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/battery_test";
const XML: &str = r#"<node><interface name="org.freedesktop.DBus.Properties"><method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method><method name="Set"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/></method></interface><interface name="org.freedesktop.UPower"><method name="EnumerateDevices"><arg type="ao" direction="out"/></method></interface></node>"#;
type Fake = Arc<Mutex<HashMap<String, Properties>>>;
struct Harness {
    backend: Option<Backend>,
    daemon: Child,
    service: Option<thread::JoinHandle<()>>,
    stop: glib::Sender<()>,
    context: glib::MainContext,
    source: glib::SourceId,
    state: Rc<RefCell<State>>,
    data: Fake,
    writes: Arc<Mutex<Vec<String>>>,
    directory: PathBuf,
}
impl Harness {
    #[allow(deprecated)]
    fn new(readonly: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        let data = Arc::new(Mutex::new(HashMap::from([
            (
                UPOWER.into(),
                HashMap::from([("OnBattery".into(), false.to_variant())]),
            ),
            (
                BATTERY.into(),
                HashMap::from([
                    ("Type".into(), 2u32.to_variant()),
                    ("PowerSupply".into(), true.to_variant()),
                    ("IsPresent".into(), true.to_variant()),
                    ("Percentage".into(), 81.0.to_variant()),
                    ("EnergyFull".into(), 68.936.to_variant()),
                    ("EnergyFullDesign".into(), 73.0.to_variant()),
                    ("State".into(), 5u32.to_variant()),
                ]),
            ),
            (
                PPD.into(),
                HashMap::from([
                    ("ActiveProfile".into(), "balanced".to_variant()),
                    (
                        "Profiles".into(),
                        PROFILES
                            .map(|p| HashMap::from([("Profile".to_owned(), p.to_variant())]))
                            .to_vec()
                            .to_variant(),
                    ),
                ]),
            ),
            (
                PLATFORM.into(),
                HashMap::from([
                    ("ChangePlatformProfileOnAc".into(), true.to_variant()),
                    ("ChangePlatformProfileOnBattery".into(), true.to_variant()),
                    ("PlatformProfileOnAc".into(), 0u32.to_variant()),
                    ("PlatformProfileOnBattery".into(), 2u32.to_variant()),
                    ("ChargeControlEndThreshold".into(), 80u8.to_variant()),
                ]),
            ),
        ])));
        let writes = Arc::new(Mutex::new(vec![]));
        let data2 = data.clone();
        let writes2 = writes.clone();
        let address2 = address.clone();
        let (ready, receiver) = mpsc::channel();
        let (stop, stopped) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let service = thread::spawn(move || {
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
                    for name in [UPOWER, PPD, ASUS] {
                        bus.call_sync(
                            Some("org.freedesktop.DBus"),
                            "/org/freedesktop/DBus",
                            "org.freedesktop.DBus",
                            "RequestName",
                            Some(&(name, 0u32).to_variant()),
                            None,
                            DBusCallFlags::NONE,
                            2000,
                            gio::Cancellable::NONE,
                        )
                        .unwrap();
                    }
                    let info = gio::DBusNodeInfo::for_xml(XML).unwrap();
                    let mut registrations = vec![];
                    for (path, interface) in [
                        (UPOWER_PATH, "org.freedesktop.DBus.Properties"),
                        (UPOWER_PATH, UPOWER),
                        (DEVICE_PATH, "org.freedesktop.DBus.Properties"),
                        (PPD_PATH, "org.freedesktop.DBus.Properties"),
                        (ASUS_PATH, "org.freedesktop.DBus.Properties"),
                    ] {
                        let data = data2.clone();
                        let writes = writes2.clone();
                        registrations.push(
                            bus.register_object(
                                path,
                                &info.lookup_interface(interface).unwrap(),
                                move |_, _, _, _, method, args, invocation| match method {
                                    "EnumerateDevices" => invocation.return_value(Some(
                                        &(vec![ObjectPath::try_from(DEVICE_PATH).unwrap()],)
                                            .to_variant(),
                                    )),
                                    "GetAll" => {
                                        let interface =
                                            args.child_value(0).get::<String>().unwrap();
                                        let p = data
                                            .lock()
                                            .unwrap()
                                            .get(&interface)
                                            .cloned()
                                            .unwrap_or_default();
                                        invocation.return_value(Some(&(p,).to_variant()));
                                    }
                                    "Set" => {
                                        let (interface, key, value) =
                                            args.get::<(String, String, Variant)>().unwrap();
                                        writes.lock().unwrap().push(key.clone());
                                        let applied = if interface == PLATFORM
                                            && ["PlatformProfileOnAc", "PlatformProfileOnBattery"]
                                                .contains(&key.as_str())
                                        {
                                            value.get::<u32>().and_then(asus_profile)
                                        } else {
                                            None
                                        };
                                        let mut data = data.lock().unwrap();
                                        data.get_mut(&interface).unwrap().insert(key, value);
                                        if let Some(profile) = applied {
                                            data.get_mut(PPD).unwrap().insert(
                                                "ActiveProfile".into(),
                                                profile.to_variant(),
                                            );
                                        }
                                        invocation.return_value(None);
                                    }
                                    _ => invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.NotSupported",
                                        "unsupported",
                                    ),
                                },
                                |_, _, _, _, _| ().to_variant(),
                                |_, _, _, _, _, _| false,
                            )
                            .unwrap(),
                        );
                    }
                    let main_loop = glib::MainLoop::new(Some(&context), false);
                    let loop2 = main_loop.clone();
                    stopped.attach(Some(&context), move |_| {
                        loop2.quit();
                        glib::ControlFlow::Break
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
        receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "lucy-power-test.{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let context = glib::MainContext::new();
        let state = Rc::new(RefCell::new(State::default()));
        let state2 = state.clone();
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let source = rx.attach(Some(&context), move |s| {
            *state2.borrow_mut() = s;
            glib::ControlFlow::Continue
        });
        let backend =
            Backend::with_options(tx, readonly, directory.join("power.json"), Some(address));
        Self {
            backend: Some(backend),
            daemon,
            service: Some(service),
            stop,
            context,
            source,
            state,
            data,
            writes,
            directory,
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
                "state timeout: {state:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.backend.take());
        self.context
            .find_source_by_id(&self.source)
            .unwrap()
            .destroy();
        let _ = self.stop.send(());
        if let Some(service) = self.service.take() {
            let _ = service.join();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
#[test]
fn power_controls_round_trip_without_duplicate_asus_automation() {
    let h = Harness::new(false);
    let state = h.wait(|s| s.ready);
    assert_eq!(state.charge_limit, Some(80));
    assert_eq!(state.profile.as_deref(), Some("balanced"));
    assert!(state.settings.auto_ac && state.settings.auto_battery);
    assert!(
        h.writes.lock().unwrap().is_empty(),
        "startup must preserve existing ASUS policy"
    );
    for profile in ["performance", "balanced"] {
        h.send(Request::Profile(profile.into()));
        h.wait(|s| !s.busy && s.profile.as_deref() == Some(profile));
    }
    for limit in [100, 80] {
        h.send(Request::ChargeLimit(limit));
        h.wait(|s| !s.busy && s.charge_limit == Some(limit));
    }
    h.send(Request::Automatic(false, false));
    h.wait(|s| !s.busy && !s.settings.auto_battery);
    h.send(Request::AutomaticProfile(false, "performance".into()));
    h.wait(|s| !s.busy && s.settings.battery_profile == "performance");
    let saved = load(&h.directory.join("power.json")).unwrap();
    assert!(saved.managed_battery && !saved.managed_ac);
    h.send(Request::Timeout(false, Timer::Screen, 5));
    h.wait(|s| !s.busy && s.settings.battery_idle.screen == 5);
    h.send(Request::Timeout(false, Timer::Suspend, 15));
    h.wait(|s| !s.busy && s.settings.battery_idle.suspend == 15);
    h.send(Request::LowBattery(true));
    h.wait(|s| !s.busy && s.settings.low_battery);
    let saved = load(&h.directory.join("power.json")).unwrap();
    assert!(saved.low_battery);
    assert_eq!(
        saved.battery_idle,
        IdleTimes {
            screen: 5,
            suspend: 15
        }
    );
    h.data
        .lock()
        .unwrap()
        .get_mut(UPOWER)
        .unwrap()
        .insert("OnBattery".into(), true.to_variant());
    h.data
        .lock()
        .unwrap()
        .get_mut(BATTERY)
        .unwrap()
        .insert("Percentage".into(), 19.0.to_variant());
    h.send(Request::Refresh);
    h.wait(|s| {
        !s.busy && s.on_battery == Some(true) && s.profile.as_deref() == Some("power-saver")
    });
    h.send(Request::Profile("performance".into()));
    h.wait(|s| !s.busy && s.profile.as_deref() == Some("performance"));
    h.send(Request::Refresh);
    thread::sleep(Duration::from_millis(30));
    assert_eq!(
        h.wait(|s| !s.busy).profile.as_deref(),
        Some("performance"),
        "low battery policy must not repeatedly undo a manual choice"
    );
    h.send(Request::Timeout(false, Timer::Screen, 30));
    h.wait(|s| !s.busy && !s.error.is_empty());
    assert_eq!(
        load(&h.directory.join("power.json"))
            .unwrap()
            .battery_idle
            .screen,
        5
    );
}
#[test]
fn read_only_power_preview_never_writes_profiles_thresholds_or_idle_settings() {
    let h = Harness::new(true);
    h.wait(|s| s.ready);
    for request in [
        Request::Profile("performance".into()),
        Request::ChargeLimit(100),
        Request::Timeout(true, Timer::Suspend, 15),
        Request::LowBattery(true),
        Request::Idle(Timer::Suspend, true),
    ] {
        h.send(request);
    }
    thread::sleep(Duration::from_millis(100));
    h.wait(|s| s.ready);
    assert!(h.writes.lock().unwrap().is_empty());
    assert!(!h.directory.join("power.json").exists());
}

#[test]
fn editing_automatic_profiles_never_applies_them_to_the_current_mode() {
    let h = Harness::new(false);
    h.wait(|s| s.ready);
    h.send(Request::Profile("performance".into()));
    h.wait(|s| !s.busy && s.profile.as_deref() == Some("performance"));
    h.writes.lock().unwrap().clear();
    h.send(Request::AutomaticProfile(true, "power-saver".into()));
    let state = h.wait(|s| !s.busy && s.settings.ac_profile == "power-saver");
    assert_eq!(state.profile.as_deref(), Some("performance"));
    h.send(Request::Automatic(true, false));
    h.wait(|s| !s.busy && !s.settings.auto_ac);
    h.send(Request::Automatic(true, true));
    let state = h.wait(|s| !s.busy && s.settings.auto_ac);
    assert_eq!(state.profile.as_deref(), Some("performance"));
    h.send(Request::AutomaticProfile(false, "balanced".into()));
    let state = h.wait(|s| !s.busy && s.settings.battery_profile == "balanced");
    assert_eq!(state.profile.as_deref(), Some("performance"));
    assert!(!h
        .writes
        .lock()
        .unwrap()
        .iter()
        .any(|key| key == "ActiveProfile"));
    assert!(load(&h.directory.join("power.json")).unwrap().managed_ac);
    assert!(!h
        .writes
        .lock()
        .unwrap()
        .iter()
        .any(|key| key == "PlatformProfileOnAc" || key == "PlatformProfileOnBattery"));
    h.data
        .lock()
        .unwrap()
        .get_mut(UPOWER)
        .unwrap()
        .insert("OnBattery".into(), true.to_variant());
    h.send(Request::Refresh);
    h.wait(|s| s.on_battery == Some(true) && s.profile.as_deref() == Some("balanced"));
    h.data
        .lock()
        .unwrap()
        .get_mut(UPOWER)
        .unwrap()
        .insert("OnBattery".into(), false.to_variant());
    h.send(Request::Refresh);
    h.wait(|s| s.on_battery == Some(false) && s.profile.as_deref() == Some("power-saver"));
}

#[test]
fn failed_automatic_save_restores_asus_policy_without_changing_active_mode() {
    let h = Harness::new(false);
    h.wait(|s| s.ready);
    // A directory cannot be replaced by the atomic settings-file rename.
    fs::create_dir(h.directory.join("power.json")).unwrap();
    h.send(Request::AutomaticProfile(true, "performance".into()));
    let state = h.wait(|s| !s.busy && !s.error.is_empty());
    assert_eq!(state.profile.as_deref(), Some("balanced"));
    assert_eq!(state.settings.ac_profile, "balanced");
    assert!(state.settings.auto_ac && !state.settings.managed_ac);
    assert_eq!(
        *h.writes.lock().unwrap(),
        ["ChangePlatformProfileOnAc", "ChangePlatformProfileOnAc"]
    );
    assert_eq!(
        h.data.lock().unwrap()[PLATFORM]["ChangePlatformProfileOnAc"].get::<bool>(),
        Some(true)
    );
}
