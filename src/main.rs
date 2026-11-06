//! Native Rust/GTK volume popup. Backend writes only follow explicit user signals.
mod artwork;
mod bar;
mod control;
mod dismiss;
mod gauge;
mod network;
mod panel;
mod tray;
use glib::{translate::ToGlibPtr, ControlFlow};
use gtk::{gdk, pango, prelude::*};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    env,
    ffi::CString,
    fs::OpenOptions,
    io::{BufRead, BufReader},
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

// Tiny binding to the installed library; all calls remain on GTK's main thread.
mod layer {
    use super::*;
    #[link(name = "gtk-layer-shell")]
    extern "C" {
        pub fn gtk_layer_is_supported() -> libc::c_int;
        fn gtk_layer_init_for_window(win: *mut gtk::ffi::GtkWindow);
        fn gtk_layer_set_namespace(win: *mut gtk::ffi::GtkWindow, ns: *const libc::c_char);
        fn gtk_layer_set_monitor(win: *mut gtk::ffi::GtkWindow, monitor: *mut gdk::ffi::GdkMonitor);
        fn gtk_layer_set_layer(win: *mut gtk::ffi::GtkWindow, layer: libc::c_int);
        fn gtk_layer_set_keyboard_mode(win: *mut gtk::ffi::GtkWindow, mode: libc::c_int);
        fn gtk_layer_set_exclusive_zone(win: *mut gtk::ffi::GtkWindow, zone: libc::c_int);
        fn gtk_layer_set_anchor(win: *mut gtk::ffi::GtkWindow, edge: libc::c_int, set: libc::c_int);
        fn gtk_layer_set_margin(
            win: *mut gtk::ffi::GtkWindow,
            edge: libc::c_int,
            margin: libc::c_int,
        );
    }
    pub fn window(style: &str, namespace: &str, monitor: &gdk::Monitor) -> gtk::Window {
        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        win.style_context().add_class(style);
        win.set_app_paintable(true);
        if let Some(visual) =
            gtk::prelude::GtkWindowExt::screen(&win).and_then(|screen| screen.rgba_visual())
        {
            win.set_visual(Some(&visual));
        }
        let ns = CString::new(namespace).unwrap();
        // SAFETY: live GTK/GDK objects, matching system ABI, namespace copied by library.
        unsafe {
            let ptr = win.to_glib_none().0;
            gtk_layer_init_for_window(ptr);
            gtk_layer_set_namespace(ptr, ns.as_ptr());
            gtk_layer_set_monitor(ptr, monitor.to_glib_none().0);
            gtk_layer_set_layer(ptr, 3); // overlay
            gtk_layer_set_keyboard_mode(ptr, 0);
            gtk_layer_set_exclusive_zone(ptr, -1);
        }
        win
    }
    pub fn anchor(win: &gtk::Window, edge: i32, margin: i32) {
        unsafe {
            gtk_layer_set_anchor(win.to_glib_none().0, edge, 1);
            gtk_layer_set_margin(win.to_glib_none().0, edge, margin);
        }
    }
    pub fn keyboard(win: &gtk::Window) {
        unsafe {
            gtk_layer_set_keyboard_mode(win.to_glib_none().0, 1);
        }
    }
    pub fn monitor(win: &gtk::Window, monitor: &gdk::Monitor) {
        // SAFETY: live GTK/GDK objects; called before mapping the popup again.
        unsafe { gtk_layer_set_monitor(win.to_glib_none().0, monitor.to_glib_none().0) }
    }
    pub fn top(win: &gtk::Window) {
        // SAFETY: live GTK window and the gtk-layer-shell layer enum (TOP = 2).
        unsafe { gtk_layer_set_layer(win.to_glib_none().0, 2) }
    }
}

const KINDS: [(&str, &str, &str); 4] = [
    ("sink", "sinks", "출력"),
    ("source", "sources", "마이크"),
    ("sink-input", "sink-inputs", "앱별 재생"),
    ("source-output", "source-outputs", "앱별 녹음"),
];

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn volume(item: &Value) -> f64 {
    let Some(channels) = item["volume"].as_object() else {
        return 0.0;
    };
    let total: f64 = channels
        .values()
        .map(|v| v["value"].as_f64().unwrap_or(0.0))
        .sum();
    total / channels.len().max(1) as f64 / 65536.0 * 100.0
}
fn description(item: &Value) -> String {
    for candidate in [
        text(item, "description"),
        text(&item["properties"], "application.name"),
        text(&item["properties"], "media.name"),
        text(item, "name"),
    ] {
        if !candidate.is_empty() {
            return candidate.replace('\0', "");
        }
    }
    item["index"].to_string()
}
fn volume_command(kind: &str, index: u64, percentage: u32) -> Vec<String> {
    vec![
        format!("set-{kind}-volume"),
        index.to_string(),
        format!("{}%", percentage.min(100)),
    ]
}
fn pactl(args: &[String]) -> Result<Vec<u8>, String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(output.stdout)
}
fn snapshot() -> Result<Value, String> {
    let mut state = serde_json::Map::new();
    for kind in ["info", "sinks", "sources", "sink-inputs", "source-outputs"] {
        let mut args = vec!["--format=json".into()];
        if kind != "info" {
            args.push("list".into());
        }
        args.push(kind.into());
        let bytes = pactl(&args)?;
        state.insert(
            kind.into(),
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
        );
    }
    Ok(Value::Object(state))
}

#[derive(Debug)]
enum Request {
    Refresh,
    Write(Vec<String>),
    Quit,
}
enum Event {
    State(Value),
    Error(String),
}
struct Backend {
    tx: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    subscriber: Arc<Mutex<Option<Child>>>,
    handles: Vec<thread::JoinHandle<()>>,
}
impl Backend {
    #[allow(deprecated)] // gtk3-rs 0.18 main-context channel, UI objects never cross threads.
    fn start(events: glib::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_events = events.clone();
        let worker = thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                if matches!(first, Request::Refresh) {
                    // Coalesce bursts of PipeWire device/volume events, not periodic polling.
                    thread::sleep(Duration::from_millis(60));
                }
                let mut requests = vec![first];
                requests.extend(rx.try_iter());
                let mut refresh = false;
                for request in requests {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match request {
                        Request::Quit => return,
                        Request::Refresh => refresh = true,
                        Request::Write(args) => {
                            if let Err(e) = pactl(&args) {
                                let _ = worker_events.send(Event::Error(e));
                            }
                            refresh = true;
                        }
                    }
                }
                if refresh {
                    match snapshot() {
                        Ok(state) => {
                            let _ = worker_events.send(Event::State(state));
                        }
                        Err(e) => {
                            let _ = worker_events.send(Event::Error(e));
                        }
                    }
                }
            }
        });
        let subscriber = Arc::new(Mutex::new(None::<Child>));
        let shared = subscriber.clone();
        let sub_stop = stop.clone();
        let sub_tx = tx.clone();
        let sub = thread::spawn(move || {
            let mut child = match Command::new("pactl")
                .arg("subscribe")
                .env("LC_ALL", "C")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    let _ = events.send(Event::Error(e.to_string()));
                    return;
                }
            };
            let stdout = child.stdout.take().unwrap();
            {
                let mut holder = shared.lock().unwrap();
                if sub_stop.load(Ordering::Acquire) {
                    let _ = child.kill();
                }
                *holder = Some(child);
            }
            for line in BufReader::new(stdout).lines() {
                if sub_stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(line) = line else { break };
                if ["sink", "source", "server", "card"]
                    .iter()
                    .any(|word| line.contains(word))
                {
                    let _ = sub_tx.send(Request::Refresh);
                }
            }
            if let Some(mut child) = shared.lock().unwrap().take() {
                let _ = child.wait();
            }
        });
        let _ = tx.send(Request::Refresh);
        Self {
            tx,
            stop,
            subscriber,
            handles: vec![worker, sub],
        }
    }
    fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Request::Quit);
        if let Some(child) = self.subscriber.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn label(value: &str, class: Option<&str>) -> gtk::Label {
    let label = gtk::Label::new(Some(value));
    label.set_xalign(0.0);
    label.set_ellipsize(pango::EllipsizeMode::End);
    if let Some(class) = class {
        label.style_context().add_class(class);
    }
    label
}

struct Row {
    widget: gtk::Box,
    name: gtk::Label,
    value: gtk::Label,
    mute: gtk::ToggleButton,
    scale: gtk::Scale,
    syncing: Cell<bool>,
    dragging: Cell<bool>,
    quiet: Cell<Instant>,
    pending: Cell<Option<u32>>,
    timer: RefCell<Option<glib::SourceId>>,
    kind: String,
    index: u64,
    tx: mpsc::Sender<Request>,
}
impl Row {
    fn new(kind: &str, item: &Value, tx: &mpsc::Sender<Request>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name = label("", None);
        name.set_max_width_chars(34);
        header.pack_start(&name, true, true, 0);
        let value = label("", Some("volume"));
        value.set_width_chars(5);
        header.pack_start(&value, false, false, 0);
        let mute = gtk::ToggleButton::new();
        widget.pack_start(&header, false, false, 0);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        mute.set_image(Some(&gauge::audio_icon(kind == "source", false)));
        widget.pack_start(&gauge::wrap(&scale, &mute), false, false, 0);
        let row = Rc::new(Self {
            widget,
            name,
            value,
            mute,
            scale,
            syncing: Cell::new(false),
            dragging: Cell::new(false),
            quiet: Cell::new(Instant::now()),
            pending: Cell::new(None),
            timer: RefCell::new(None),
            kind: kind.into(),
            index: item["index"].as_u64().unwrap_or(0),
            tx: tx.clone(),
        });
        let weak = Rc::downgrade(&row);
        row.mute.connect_toggled(move |button| {
            if let Some(row) = weak.upgrade() {
                button.set_image(Some(&gauge::audio_icon(
                    row.kind == "source",
                    button.is_active(),
                )));
                gauge::mute_hint(button, button.is_active());
                if !row.syncing.get() {
                    row.send(vec![
                        format!("set-{}-mute", row.kind),
                        row.index.to_string(),
                        if button.is_active() { "1" } else { "0" }.into(),
                    ]);
                }
            }
        });
        let weak = Rc::downgrade(&row);
        row.scale.connect_value_changed(move |scale| {
            let Some(row) = weak.upgrade() else { return };
            if row.syncing.get() {
                return;
            }
            let percent = scale.value().round() as u32;
            row.pending.set(Some(percent));
            row.value.set_text(&format!("{percent}%"));
            row.quiet.set(Instant::now() + Duration::from_millis(500));
            if row.timer.borrow().is_none() {
                let weak = Rc::downgrade(&row);
                let timer = glib::timeout_add_local_once(Duration::from_millis(65), move || {
                    if let Some(row) = weak.upgrade() {
                        row.timer.borrow_mut().take();
                        row.flush();
                    }
                });
                *row.timer.borrow_mut() = Some(timer);
            }
        });
        let weak = Rc::downgrade(&row);
        row.scale.connect_button_press_event(move |_, _| {
            if let Some(row) = weak.upgrade() {
                row.dragging.set(true);
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&row);
        row.scale.connect_button_release_event(move |_, _| {
            if let Some(row) = weak.upgrade() {
                row.dragging.set(false);
                if let Some(timer) = row.timer.borrow_mut().take() {
                    timer.remove();
                }
                row.flush();
            }
            glib::Propagation::Proceed
        });
        row.update(item);
        row
    }
    fn update(&self, item: &Value) {
        let name = description(item);
        self.name.set_text(&name);
        self.name.set_tooltip_text(Some(&name));
        if self.dragging.get() || Instant::now() < self.quiet.get() {
            return;
        }
        self.syncing.set(true);
        let value = volume(item);
        self.scale.set_value(value.min(100.0));
        self.value.set_text(&format!("{}%", value.round() as u32));
        self.mute
            .set_active(item["mute"].as_bool().unwrap_or(false));
        gauge::mute_hint(&self.mute, self.mute.is_active());
        self.syncing.set(false);
    }
    fn send(&self, args: Vec<String>) {
        self.quiet.set(Instant::now() + Duration::from_millis(500));
        let _ = self.tx.send(Request::Write(args));
        let tx = self.tx.clone();
        glib::timeout_add_local_once(Duration::from_millis(650), move || {
            let _ = tx.send(Request::Refresh);
        });
    }
    fn flush(&self) {
        if let Some(percent) = self.pending.take() {
            self.send(volume_command(&self.kind, self.index, percent));
        }
    }
}
impl Drop for Row {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
    }
}

struct Section {
    kind: &'static str,
    collection: &'static str,
    rows: gtk::Box,
    combo: Option<gtk::ComboBoxText>,
    options: RefCell<Vec<(String, String)>>,
    empty: gtk::Label,
}
struct Ui {
    win: gtk::Window,
    dismiss: RefCell<Option<dismiss::Backdrop>>,
    backdrops: Vec<gtk::Window>,
    sections: Vec<Section>,
    rows: RefCell<HashMap<(String, u64), Rc<Row>>>,
    syncing: Cell<bool>,
    closed: Cell<bool>,
    error: gtk::Label,
    tx: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    subscriber: Arc<Mutex<Option<Child>>>,
}
impl Ui {
    fn new(backend: &Backend) -> Rc<Self> {
        Self::new_with_visibility(backend, false)
    }
    fn new_with_visibility(backend: &Backend, embedded: bool) -> Rc<Self> {
        let display = gdk::Display::default().expect("Wayland display");
        let monitor = focused_monitor(&display);
        let css = gtk::CssProvider::new();
        css.load_from_data(include_bytes!("style.css"))
            .expect("valid popup CSS");
        gtk::StyleContext::add_provider_for_screen(&gdk::Screen::default().unwrap(), &css, 800);
        #[allow(deprecated)]
        let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let dismiss = if embedded {
            None
        } else {
            dismiss::Backdrop::new("lucy-audio-dismiss", clicked).ok()
        };
        let backdrops: Vec<_> = (0..if embedded || dismiss.is_some() {
            0
        } else {
            display.n_monitors()
        })
            .filter_map(|i| display.monitor(i))
            .map(|monitor| {
                let win = layer::window("audio-dismiss", "lucy-audio-dismiss", &monitor);
                for edge in 0..4 {
                    layer::anchor(&win, edge, 0);
                }
                win.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
                win
            })
            .collect();
        let win = layer::window("audio-popup", "lucy-audio-popup", &monitor);
        layer::anchor(&win, 2, 54);
        layer::anchor(&win, 1, 16);
        layer::keyboard(&win);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 14);
        card.style_context().add_class("audio-card");
        card.set_size_request(420, -1);
        win.add(&card);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.pack_start(&label("소리", Some("title")), true, true, 0);
        let close = gtk::Button::with_label("닫기  ×");
        header.pack_end(&close, false, false, 0);
        card.pack_start(&header, false, false, 0);
        let scroll = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_overlay_scrolling(false);
        scroll.set_propagate_natural_height(true);
        let height = (monitor.geometry().height() - 200).clamp(120, 500);
        scroll.set_min_content_height(height);
        scroll.set_max_content_height(height);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
        content.set_margin_end(8);
        scroll.add(&content);
        card.pack_start(&scroll, true, true, 0);
        let mut sections = Vec::new();
        for (kind, collection, title) in KINDS {
            let section = gtk::Box::new(gtk::Orientation::Vertical, 8);
            section.pack_start(&label(title, Some("section")), false, false, 0);
            let combo = if kind == "sink" || kind == "source" {
                let combo = gtk::ComboBoxText::new();
                for cell in combo.cells() {
                    if let Ok(text) = cell.downcast::<gtk::CellRendererText>() {
                        text.set_ellipsize(pango::EllipsizeMode::End);
                        text.set_max_width_chars(34);
                    }
                }
                section.pack_start(&combo, false, false, 0);
                Some(combo)
            } else {
                None
            };
            let rows = gtk::Box::new(gtk::Orientation::Vertical, 9);
            section.pack_start(&rows, false, false, 0);
            let empty = label(
                if combo.is_some() {
                    "장치 없음"
                } else {
                    "활성 앱 없음"
                },
                Some("subtle"),
            );
            empty.set_no_show_all(true);
            rows.pack_start(&empty, false, false, 0);
            content.pack_start(&section, false, false, 0);
            sections.push(Section {
                kind,
                collection,
                rows,
                combo,
                options: RefCell::new(vec![]),
                empty,
            });
        }
        // No footer/help sentence. Only an actual backend error is displayed.
        let error = label("", Some("subtle"));
        error.set_no_show_all(true);
        card.pack_start(&error, false, false, 0);
        let ui = Rc::new(Self {
            win,
            dismiss: RefCell::new(dismiss),
            backdrops,
            sections,
            rows: RefCell::new(HashMap::new()),
            syncing: Cell::new(false),
            closed: Cell::new(false),
            error,
            tx: backend.tx.clone(),
            stop: backend.stop.clone(),
            subscriber: backend.subscriber.clone(),
        });
        let weak = Rc::downgrade(&ui);
        clicks.attach(None, move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            ControlFlow::Break
        });
        let weak = Rc::downgrade(&ui);
        close.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.win.connect_key_press_event(move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                if let Some(ui) = weak.upgrade() {
                    ui.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        for backdrop in &ui.backdrops {
            let weak = Rc::downgrade(&ui);
            backdrop.connect_button_press_event(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.close();
                }
                glib::Propagation::Stop
            });
            backdrop.show_all();
        }
        for section in &ui.sections {
            if let Some(combo) = &section.combo {
                let weak = Rc::downgrade(&ui);
                let kind = section.kind;
                combo.connect_changed(move |combo| {
                    let Some(ui) = weak.upgrade() else { return };
                    if !ui.syncing.get() {
                        if let Some(name) = combo.active_id() {
                            let _ = ui.tx.send(Request::Write(vec![
                                format!("set-default-{kind}"),
                                name.into(),
                            ]));
                        }
                    }
                });
            }
        }
        panel::prepare(&card);
        if embedded {
            card.show_all();
        } else {
            ui.win.show_all();
        }
        ui
    }
    fn render(&self, state: &Value) {
        self.syncing.set(true);
        let mut wanted = HashSet::new();
        let mut rows = self.rows.borrow_mut();
        for section in &self.sections {
            let mut items: Vec<&Value> = state[section.collection]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| {
                    section.kind != "source" || !text(item, "name").ends_with(".monitor")
                })
                .collect();
            if let Some(combo) = &section.combo {
                let options: Vec<_> = items
                    .iter()
                    .map(|item| (text(item, "name").into(), description(item)))
                    .collect();
                if *section.options.borrow() != options {
                    combo.remove_all();
                    for (name, title) in &options {
                        combo.append(Some(name), title);
                    }
                    *section.options.borrow_mut() = options;
                }
                let key = format!("default_{}_name", section.kind);
                let default = text(&state["info"], &key);
                combo.set_active_id(Some(default));
                items.retain(|item| text(item, "name") == default);
            }
            section.empty.set_visible(items.is_empty());
            for item in items {
                let key = (
                    section.kind.to_string(),
                    item["index"].as_u64().unwrap_or(0),
                );
                wanted.insert(key.clone());
                let row = rows.entry(key).or_insert_with(|| {
                    let row = Row::new(section.kind, item, &self.tx);
                    section.rows.pack_start(&row.widget, false, false, 0);
                    row.widget.show_all();
                    row
                });
                row.update(item);
            }
        }
        rows.retain(|key, row| {
            if wanted.contains(key) {
                true
            } else {
                if let Some(parent) = row
                    .widget
                    .parent()
                    .and_then(|p| p.downcast::<gtk::Container>().ok())
                {
                    parent.remove(&row.widget);
                }
                false
            }
        });
        self.syncing.set(false);
        self.error.hide();
    }
    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.dismiss.borrow_mut().take();
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Request::Quit);
        if let Some(child) = self.subscriber.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        self.rows.borrow_mut().clear();
        for backdrop in &self.backdrops {
            backdrop.hide();
        }
        self.win.hide();
        gtk::main_quit();
    }
}

fn focused_monitor(display: &gdk::Display) -> gdk::Monitor {
    if let Ok(output) = Command::new("niri")
        .args(["msg", "-j", "focused-output"])
        .output()
    {
        if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
            if let (Some(x), Some(y)) = (
                value["logical"]["x"].as_i64(),
                value["logical"]["y"].as_i64(),
            ) {
                if let Some(monitor) = display.monitor_at_point(x as i32 + 20, y as i32 + 20) {
                    return monitor;
                }
            }
        }
    }
    display.monitor(0).expect("at least one monitor")
}

fn self_test() {
    let (tx, rx) = mpsc::channel();
    let item = serde_json::json!({"index":123,"description":"가상 검증 행","mute":true,
        "volume":{"left":{"value":32768},"right":{"value":32768}}});
    let row = Row::new("sink-input", &item, &tx);
    row.update(&item);
    assert!(
        rx.try_recv().is_err(),
        "initialization and refresh must be read-only"
    );
    assert_eq!(row.scale.value(), 50.0);
    assert!(row.mute.is_active());
    row.scale.set_value(61.0);
    row.flush();
    match rx.try_recv().unwrap() {
        Request::Write(args) => assert_eq!(args, ["set-sink-input-volume", "123", "61%"]),
        _ => panic!("unexpected request"),
    }
    row.mute.set_active(false);
    match rx.try_recv().unwrap() {
        Request::Write(args) => assert_eq!(args, ["set-sink-input-mute", "123", "0"]),
        _ => panic!("unexpected request"),
    }
    println!("PASS: native GTK initialization/refresh read-only, slider/mute signals; no real audio writes");
}

#[allow(deprecated)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    glib::set_application_name("Lucy");
    glib::set_prgname(Some("lucy-shell"));
    if env::args().any(|arg| arg == "--version") {
        println!("Lucy 0.2.0 (lucy-shell; Rust; audio + network + bar)");
        return Ok(());
    }
    gtk::init()?;
    if env::args().any(|arg| arg == "--self-test-control") {
        return control::self_test();
    }
    if env::args().any(|arg| arg == "control" || arg == "audio") || env::args().len() == 1 {
        return control::run();
    }
    if env::args().any(|arg| arg == "bar") {
        return bar::run();
    }
    if env::args().any(|arg| arg == "network") {
        return if env::args().any(|arg| arg.starts_with("--self-test-network")) {
            network::run(env::args().any(|arg| arg == "--vpn"))
        } else {
            control::run()
        };
    }
    if env::args().any(|arg| arg == "--self-test") {
        self_test();
        return Ok(());
    }
    run_audio()
}

pub(crate) struct AudioPage {
    pub widget: gtk::Widget,
    ui: Rc<Ui>,
    backend: Backend,
    source: glib::SourceId,
}
impl AudioPage {
    #[allow(deprecated)]
    pub fn new(back: impl Fn() + 'static) -> Self {
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = Backend::start(events);
        let ui = Ui::new_with_visibility(&backend, true);
        let widget = ui.win.child().unwrap();
        ui.win.remove(&widget);
        let card = widget.clone().downcast::<gtk::Box>().unwrap();
        let header = card.children()[0].clone();
        card.remove(&header);
        let header = panel::header("소리", back);
        card.pack_start(&header, false, false, 0);
        card.reorder_child(&header, 0);
        let weak = Rc::downgrade(&ui);
        let source = receiver.attach(None, move |event| {
            let Some(ui) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            match event {
                Event::State(state) => ui.render(&state),
                Event::Error(_) => {
                    ui.error
                        .set_text("변경 실패 · 오디오 장치 연결을 확인하세요");
                    ui.error.show();
                }
            }
            ControlFlow::Continue
        });
        Self {
            widget,
            ui,
            backend,
            source,
        }
    }
}
impl Drop for AudioPage {
    fn drop(&mut self) {
        if let Some(source) = glib::MainContext::default().find_source_by_id(&self.source) {
            source.destroy();
        }
        for row in self.ui.rows.borrow().values() {
            row.flush();
        }
        self.backend.shutdown();
        unsafe {
            self.ui.win.destroy();
        }
    }
}

#[allow(deprecated)]
pub(crate) fn run_audio() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = env::var("XDG_RUNTIME_DIR")?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{runtime}/lucy-audio-popup.lock"))?;
    // SAFETY: a valid file descriptor held for this entire process lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    if unsafe { layer::gtk_layer_is_supported() } == 0 {
        return Err("Wayland layer-shell is required".into());
    }
    let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let backend = Backend::start(events);
    let ui = Ui::new(&backend);
    let weak = Rc::downgrade(&ui);
    receiver.attach(None, move |event| {
        let Some(ui) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        if ui.closed.get() {
            return ControlFlow::Break;
        }
        match event {
            Event::State(state) => ui.render(&state),
            Event::Error(error) => {
                eprintln!("audio-popup: {error}");
                ui.error
                    .set_text("변경 실패 · 오디오 장치 연결을 확인하세요");
                ui.error.show();
            }
        }
        ControlFlow::Continue
    });
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let weak = Rc::downgrade(&ui);
        glib::unix_signal_add_local(signal, move || {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            ControlFlow::Break
        });
    }
    gtk::main();
    backend.shutdown();
    drop(ui);
    drop(backend);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_handles_channels_missing_and_boost() {
        assert_eq!(volume(&serde_json::json!({})), 0.0);
        assert_eq!(
            volume(&serde_json::json!({"volume":{"L":{"value":32768},"R":{"value":65536}}})),
            75.0
        );
        assert_eq!(
            volume(&serde_json::json!({"volume":{"L":{"value":98304}}})),
            150.0
        );
    }
    #[test]
    fn commands_are_bounded_argv_not_shell() {
        assert_eq!(
            volume_command("source", 42, 150),
            ["set-source-volume", "42", "100%"]
        );
        assert_eq!(
            volume_command("sink-input", 2, 0),
            ["set-sink-input-volume", "2", "0%"]
        );
    }
    #[test]
    fn description_prefers_device_then_application() {
        assert_eq!(
            description(
                &serde_json::json!({"description":"device","properties":{"application.name":"app"}})
            ),
            "device"
        );
        assert_eq!(
            description(&serde_json::json!({"properties":{"application.name":"app"}})),
            "app"
        );
    }
}
