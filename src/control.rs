//! Unified control center. Reads and writes stay off GTK's main thread;
//! D-Bus, PulseAudio and backlight events request narrowly scoped snapshots.
use crate::{focused_monitor, label, layer};
use glib::{ControlFlow, Propagation};
use gtk::{gdk, gio, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    env, fs,
    fs::OpenOptions,
    io::{BufRead, BufReader},
    os::fd::AsRawFd,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Domain {
    Wifi,
    Bluetooth,
    Power,
    Audio,
    Brightness,
    Media,
}
const DOMAINS: [Domain; 6] = [
    Domain::Wifi,
    Domain::Bluetooth,
    Domain::Power,
    Domain::Audio,
    Domain::Brightness,
    Domain::Media,
];

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Wifi(bool),
    Bluetooth(bool),
    Profile(String),
    Volume(u32),
    Mute,
    Brightness(u32),
    Media {
        player: String,
        command: &'static str,
    },
}
impl Action {
    fn domain(&self) -> Domain {
        match self {
            Self::Wifi(_) => Domain::Wifi,
            Self::Bluetooth(_) => Domain::Bluetooth,
            Self::Profile(_) => Domain::Power,
            Self::Volume(_) | Self::Mute => Domain::Audio,
            Self::Brightness(_) => Domain::Brightness,
            Self::Media { .. } => Domain::Media,
        }
    }
    fn command(&self) -> (&'static str, Vec<String>) {
        let (program, args): (_, Vec<String>) = match self {
            Self::Wifi(on) => (
                "nmcli",
                vec![
                    "radio".into(),
                    "wifi".into(),
                    if *on { "on" } else { "off" }.into(),
                ],
            ),
            Self::Bluetooth(on) => (
                "bluetoothctl",
                vec!["power".into(), if *on { "on" } else { "off" }.into()],
            ),
            Self::Profile(profile) => ("powerprofilesctl", vec!["set".into(), profile.clone()]),
            Self::Volume(value) => (
                "wpctl",
                vec![
                    "set-volume".into(),
                    "-l".into(),
                    "1.0".into(),
                    "@DEFAULT_AUDIO_SINK@".into(),
                    format!("{}%", value.min(&100)),
                ],
            ),
            Self::Mute => (
                "wpctl",
                vec![
                    "set-mute".into(),
                    "@DEFAULT_AUDIO_SINK@".into(),
                    "toggle".into(),
                ],
            ),
            Self::Brightness(value) => (
                "brightnessctl",
                vec![
                    "--class=backlight".into(),
                    "set".into(),
                    format!("{}%", value.clamp(&1, &100)),
                ],
            ),
            Self::Media { player, command } => (
                "playerctl",
                vec!["--player".into(), player.clone(), (*command).into()],
            ),
        };
        (program, args)
    }
}
#[derive(Clone, Default)]
struct Media {
    player: String,
    title: String,
    artist: String,
    art_url: String,
    playing: bool,
}
#[derive(Clone, Default)]
struct State {
    wifi: Option<bool>,
    bluetooth: Option<bool>,
    profile: Option<String>,
    profiles: Vec<String>,
    audio: Option<(f64, bool)>,
    brightness: Option<f64>,
    media: Option<Media>,
}
enum Request {
    Refresh(Domain),
    Write(Action),
    Quit,
}
enum Event {
    State(State),
    Done(Domain, bool),
}

const MEDIA_FORMAT: &str =
    "{{playerInstance}}\n{{status}}\n{{title}}\n{{artist}}\n{{mpris:artUrl}}";
fn parse_media(value: &str) -> Option<Media> {
    let mut lines = value.lines();
    let player = lines.next()?;
    let playing = lines.next()? == "Playing";
    let title = lines
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(player)
        .to_owned();
    Some(Media {
        player: player.to_owned(),
        title,
        artist: lines.next().unwrap_or_default().into(),
        art_url: lines.next().unwrap_or_default().into(),
        playing,
    })
}

fn read(program: &str, args: &[&str]) -> Option<String> {
    crate::bar::output(program, args)
}
fn refresh(state: &mut State, domain: Domain) {
    match domain {
        Domain::Wifi => {
            state.wifi = read("nmcli", &["--terse", "--fields", "WIFI", "general"])
                .map(|value| value == "enabled")
        }
        Domain::Bluetooth => {
            state.bluetooth = read("bluetoothctl", &["show"]).and_then(|value| {
                value.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("Powered: ")
                        .map(|value| value == "yes")
                })
            })
        }
        Domain::Power => {
            state.profile = read("powerprofilesctl", &["get"]);
            state.profiles = read("powerprofilesctl", &["list"])
                .map(|value| {
                    value
                        .lines()
                        .filter_map(|line| {
                            let name = line
                                .trim()
                                .trim_start_matches('*')
                                .trim()
                                .trim_end_matches(':');
                            ["power-saver", "balanced", "performance"]
                                .contains(&name)
                                .then(|| name.to_owned())
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        Domain::Audio => {
            state.audio = read("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).and_then(|value| {
                Some((
                    value.split_whitespace().nth(1)?.parse::<f64>().ok()? * 100.0,
                    value.contains("MUTED"),
                ))
            })
        }
        Domain::Brightness => {
            state.brightness = read("brightnessctl", &["--class=backlight", "-m"])
                .and_then(|value| value.split(',').nth(3)?.trim_end_matches('%').parse().ok())
        }
        Domain::Media => {
            state.media = read("playerctl", &["--format", MEDIA_FORMAT, "metadata"])
                .and_then(|value| parse_media(&value))
        }
    }
}

struct Backend {
    tx: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    handles: Vec<thread::JoinHandle<()>>,
}
impl Backend {
    fn start(events: glib::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut state = State::default();
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                batch.extend(rx.try_iter());
                let mut domains = HashSet::new();
                let mut completed = Vec::new();
                let mut quit = false;
                for request in batch {
                    match request {
                        Request::Quit => {
                            quit = true;
                            break;
                        }
                        Request::Refresh(domain) => {
                            domains.insert(domain);
                        }
                        Request::Write(action) => {
                            let domain = action.domain();
                            let (program, args) = action.command();
                            let refs: Vec<_> = args.iter().map(String::as_str).collect();
                            let success = read(program, &refs).is_some();
                            domains.insert(domain);
                            completed.push((domain, success));
                        }
                    }
                }
                for domain in DOMAINS {
                    if domains.contains(&domain) {
                        refresh(&mut state, domain);
                    }
                }
                if !domains.is_empty() {
                    let _ = events.send(Event::State(state.clone()));
                }
                for (domain, success) in completed {
                    let _ = events.send(Event::Done(domain, success));
                }
                if quit {
                    break;
                }
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None::<Child>));
        let stopping = stop.clone();
        let holder = child.clone();
        let notify = tx.clone();
        let audio = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let Ok(mut child) = Command::new("pactl")
                    .arg("subscribe")
                    .env("LC_ALL", "C")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    break;
                };
                let stdout = child.stdout.take().unwrap();
                {
                    let mut slot = holder.lock().unwrap();
                    if stopping.load(Ordering::Acquire) {
                        let _ = child.kill();
                    }
                    *slot = Some(child);
                }
                for line in BufReader::new(stdout).lines() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(line) = line else { break };
                    if line.contains(" on sink #") || line.contains(" on server #") {
                        let _ = notify.send(Request::Refresh(Domain::Audio));
                    }
                }
                if let Some(mut child) = holder.lock().unwrap().take() {
                    let _ = child.wait();
                }
                if !stopping.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        for domain in DOMAINS {
            let _ = tx.send(Request::Refresh(domain));
        }
        Self {
            tx,
            stop,
            child,
            handles: vec![worker, audio],
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        // UI flushes its sliders first. Quit is queued after those final writes.
        let _ = self.tx.send(Request::Quit);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn icon(name: &str, size: i32) -> gtk::Image {
    let image = gtk::Image::from_icon_name(Some(name), gtk::IconSize::Button);
    image.set_pixel_size(size);
    image
}
fn icon_button(name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_image(Some(&icon(name, 18)));
    button.set_tooltip_text(Some(tooltip));
    button.set_can_focus(false);
    button
}
fn profile_name(profile: &str) -> &str {
    match profile {
        "power-saver" => "절전",
        "balanced" => "균형",
        "performance" => "성능",
        _ => "사용 불가",
    }
}
struct Tile {
    widget: gtk::Box,
    button: gtk::Button,
    value: gtk::Label,
    arrow: gtk::Button,
}
impl Tile {
    fn new(title: &str, symbol: &str, available: bool) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.style_context().add_class("cc-tile");
        let button = gtk::Button::new();
        button.set_hexpand(true);
        button.set_sensitive(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        heading.pack_start(&icon(symbol, 18), false, false, 0);
        heading.pack_start(&label(title, None), false, false, 0);
        let value = label("불러오는 중", Some("subtle"));
        content.pack_start(&heading, false, false, 0);
        content.pack_start(&value, false, false, 0);
        button.add(&content);
        let arrow = icon_button(
            "go-next-symbolic",
            if available {
                "세부 설정"
            } else {
                "세부 설정 준비 중"
            },
        );
        arrow.style_context().add_class("cc-arrow");
        arrow.set_image(Some(&icon("go-next-symbolic", 22)));
        arrow.set_sensitive(available);
        widget.pack_start(&button, true, true, 0);
        widget.pack_start(&arrow, false, false, 0);
        Self {
            widget,
            button,
            value,
            arrow,
        }
    }
    fn render(&self, enabled: Option<bool>, pending: bool, text: &str) {
        self.button.set_sensitive(enabled.is_some() && !pending);
        self.value.set_text(if pending { "변경 중…" } else { text });
        let style = self.button.style_context();
        if enabled == Some(true) {
            style.add_class("active");
        } else {
            style.remove_class("active");
        }
    }
}
struct Level {
    widget: gtk::Box,
    scale: gtk::Scale,
    percent: gtk::Label,
    button: gtk::Button,
    gear: gtk::Button,
    domain: Domain,
    syncing: Cell<bool>,
    dragging: Cell<bool>,
    quiet: Cell<Instant>,
    pending: Cell<Option<u32>>,
    timer: RefCell<Option<glib::SourceId>>,
    tx: mpsc::Sender<Request>,
}
impl Level {
    fn new(domain: Domain, tx: mpsc::Sender<Request>) -> Rc<Self> {
        let volume = domain == Domain::Audio;
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let button = icon_button(
            if volume {
                "audio-volume-high-symbolic"
            } else {
                "display-brightness-symbolic"
            },
            if volume {
                "음소거 전환"
            } else {
                "디스플레이 밝기"
            },
        );
        if !volume {
            button.set_sensitive(false);
            button.style_context().add_class("cc-passive");
        }
        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            if volume { 0.0 } else { 1.0 },
            100.0,
            1.0,
        );
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_sensitive(false);
        let percent = label("—", Some("volume"));
        percent.set_width_chars(4);
        percent.set_xalign(1.0);
        let gear = icon_button(
            "emblem-system-symbolic",
            if volume {
                "소리 세부 설정"
            } else {
                "디스플레이 세부 설정 준비 중"
            },
        );
        gear.set_sensitive(volume);
        widget.pack_start(&crate::gauge::wrap(&scale, &button), true, true, 0);
        widget.pack_start(&percent, false, false, 0);
        widget.pack_start(&gear, false, false, 0);
        let level = Rc::new(Self {
            widget,
            scale,
            percent,
            button,
            gear,
            domain,
            syncing: Cell::new(false),
            dragging: Cell::new(false),
            quiet: Cell::new(Instant::now()),
            pending: Cell::new(None),
            timer: RefCell::new(None),
            tx,
        });
        let weak = Rc::downgrade(&level);
        level.scale.connect_value_changed(move |scale| {
            let Some(level) = weak.upgrade() else { return };
            if level.syncing.get() {
                return;
            }
            let value = scale.value().round() as u32;
            level.percent.set_text(&format!("{value}%"));
            level.pending.set(Some(value));
            level.quiet.set(Instant::now() + Duration::from_millis(250));
            if level.timer.borrow().is_none() {
                let weak = Rc::downgrade(&level);
                *level.timer.borrow_mut() = Some(glib::timeout_add_local_once(
                    Duration::from_millis(35),
                    move || {
                        if let Some(level) = weak.upgrade() {
                            level.timer.borrow_mut().take();
                            level.flush();
                        }
                    },
                ));
            }
        });
        let weak = Rc::downgrade(&level);
        level.scale.connect_button_press_event(move |_, _| {
            if let Some(level) = weak.upgrade() {
                level.dragging.set(true);
            }
            Propagation::Proceed
        });
        let weak = Rc::downgrade(&level);
        level.scale.connect_button_release_event(move |_, _| {
            if let Some(level) = weak.upgrade() {
                level.dragging.set(false);
                level.flush();
            }
            Propagation::Proceed
        });
        level
    }
    fn flush(&self) {
        if let Some(value) = self.pending.take() {
            let action = if self.domain == Domain::Audio {
                Action::Volume(value)
            } else {
                Action::Brightness(value)
            };
            let _ = self.tx.send(Request::Write(action));
        }
    }
    fn render(&self, value: Option<f64>, muted: bool) {
        self.scale.set_sensitive(value.is_some());
        if self.domain == Domain::Audio {
            self.button.set_sensitive(value.is_some());
            self.button
                .set_image(Some(&crate::gauge::audio_icon(false, muted)));
            crate::gauge::mute_hint(&self.button, muted);
        }
        if self.dragging.get() || Instant::now() < self.quiet.get() {
            return;
        }
        self.syncing.set(true);
        self.scale.set_value(value.unwrap_or(0.0).min(100.0));
        self.syncing.set(false);
        self.percent
            .set_text(&value.map(|v| format!("{v:.0}%")).unwrap_or("—".into()));
    }
}
impl Drop for Level {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
    }
}

struct Ui {
    win: gtk::Window,
    pages: gtk::Stack,
    audio_page: RefCell<Option<crate::AudioPage>>,
    network_page: RefCell<Option<crate::network::Page>>,
    persistent: Cell<bool>,
    dismiss: RefCell<Option<crate::dismiss::Backdrop>>,
    fallback: Vec<gtk::Window>,
    wifi: Tile,
    bluetooth: Tile,
    power: Tile,
    choices: gtk::Box,
    profiles: Vec<(String, gtk::Button)>,
    title: gtk::Label,
    artist: gtk::Label,
    artwork: crate::artwork::Artwork,
    previous: gtk::Button,
    play: gtk::Button,
    next: gtk::Button,
    volume: Rc<Level>,
    brightness: Rc<Level>,
    error: gtk::Label,
    state: RefCell<State>,
    pending: RefCell<HashSet<Domain>>,
    tx: mpsc::Sender<Request>,
    closed: Cell<bool>,
    open: Rc<dyn Fn(&str)>,
    subscriptions: RefCell<Vec<(gio::DBusConnection, gio::SignalSubscriptionId)>>,
    monitors: RefCell<Vec<gio::FileMonitor>>,
}
impl Ui {
    #[allow(deprecated)]
    fn new(tx: mpsc::Sender<Request>, open: Rc<dyn Fn(&str)>, headless: bool) -> Rc<Self> {
        let display = gdk::Display::default().unwrap();
        let monitor = focused_monitor(&display);
        let css = gtk::CssProvider::new();
        css.load_from_data(include_bytes!("style.css"))
            .expect("valid CSS");
        gtk::StyleContext::add_provider_for_screen(&gdk::Screen::default().unwrap(), &css, 800);
        let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let dismiss = if headless {
            None
        } else {
            crate::dismiss::Backdrop::new("lucy-control-dismiss", clicked).ok()
        };
        let fallback: Vec<_> = if !headless && dismiss.is_none() {
            (0..display.n_monitors())
                .filter_map(|i| display.monitor(i))
                .map(|monitor| {
                    let win = layer::window("audio-dismiss", "lucy-control-dismiss", &monitor);
                    for edge in 0..4 {
                        layer::anchor(&win, edge, 0);
                    }
                    win.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
                    win
                })
                .collect()
        } else {
            Vec::new()
        };
        let win = layer::window("audio-popup", "lucy-control-popup", &monitor);
        win.style_context().add_class("control-center");
        layer::anchor(&win, 2, 54);
        layer::anchor(&win, 1, 16);
        layer::keyboard(&win);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
        card.set_size_request(crate::panel::WIDTH, crate::panel::HEIGHT);
        card.style_context().add_class("audio-card");
        let pages = gtk::Stack::new();
        pages.set_homogeneous(true);
        pages.set_size_request(crate::panel::WIDTH, crate::panel::HEIGHT);
        pages.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
        pages.set_transition_duration(180);
        pages.add_named(&card, "home");
        win.add(&pages);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.pack_start(
            &gtk::Box::new(gtk::Orientation::Horizontal, 0),
            true,
            true,
            0,
        );
        let session = icon_button("system-shutdown-symbolic", "전원 메뉴");
        let close = icon_button("window-close-symbolic", "닫기");
        header.pack_start(&session, false, false, 0);
        header.pack_start(&close, false, false, 0);
        card.pack_start(&header, false, false, 0);
        let tiles = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        tiles.set_homogeneous(true);
        let wifi = Tile::new("Wi-Fi", "network-wireless-symbolic", true);
        let bluetooth = Tile::new("블루투스", "bluetooth-active-symbolic", false);
        let power = Tile::new("전원 모드", "power-profile-balanced-symbolic", false);
        for tile in [&wifi, &bluetooth, &power] {
            tiles.pack_start(&tile.widget, true, true, 0);
        }
        card.pack_start(&tiles, false, false, 0);
        let choices = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        choices.set_homogeneous(true);
        choices.set_no_show_all(true);
        let profiles = ["power-saver", "balanced", "performance"]
            .map(|name| {
                let button = gtk::Button::with_label(profile_name(name));
                choices.pack_start(&button, true, true, 0);
                (name.into(), button)
            })
            .to_vec();
        card.pack_start(&choices, false, false, 0);
        let media = gtk::Box::new(gtk::Orientation::Vertical, 10);
        media.style_context().add_class("cc-media");
        let title = label("재생 중인 미디어 없음", None);
        title.set_max_width_chars(42);
        let artist = label("", Some("subtle"));
        artist.set_max_width_chars(48);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        let artwork = crate::artwork::Artwork::new();
        row.pack_start(&artwork.widget, false, false, 0);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 6);
        text.set_valign(gtk::Align::Center);
        text.pack_start(&title, false, false, 0);
        text.pack_start(&artist, false, false, 0);
        row.pack_start(&text, true, true, 0);
        media.pack_start(&row, false, false, 0);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        controls.set_halign(gtk::Align::Center);
        let previous = icon_button("media-skip-backward-symbolic", "이전 곡");
        let play = icon_button("media-playback-start-symbolic", "재생 / 일시정지");
        let next = icon_button("media-skip-forward-symbolic", "다음 곡");
        play.style_context().add_class("cc-play");
        for button in [&previous, &play, &next] {
            button.set_sensitive(false);
            controls.pack_start(button, false, false, 0);
        }
        media.pack_start(&controls, false, false, 0);
        card.pack_start(&media, false, false, 0);
        let volume = Level::new(Domain::Audio, tx.clone());
        let brightness = Level::new(Domain::Brightness, tx.clone());
        card.pack_start(&volume.widget, false, false, 0);
        card.pack_start(&brightness.widget, false, false, 0);
        let error = label("", Some("subtle"));
        error.set_line_wrap(true);
        error.set_max_width_chars(48);
        error.set_no_show_all(true);
        card.pack_start(&error, false, false, 0);
        let ui = Rc::new(Self {
            win,
            pages,
            audio_page: RefCell::new(None),
            network_page: RefCell::new(None),
            persistent: Cell::new(false),
            dismiss: RefCell::new(dismiss),
            fallback,
            wifi,
            bluetooth,
            power,
            choices,
            profiles,
            title,
            artist,
            artwork,
            previous,
            play,
            next,
            volume,
            brightness,
            error,
            state: RefCell::new(State::default()),
            pending: RefCell::new(HashSet::new()),
            tx,
            closed: Cell::new(false),
            open,
            subscriptions: RefCell::new(Vec::new()),
            monitors: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&ui);
        close.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
        });
        let weak = Rc::downgrade(&ui);
        session.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.detail("session");
            }
        });
        let weak = Rc::downgrade(&ui);
        clicks.attach(None, move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            ControlFlow::Continue
        });
        let weak = Rc::downgrade(&ui);
        ui.win.connect_key_press_event(move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                if let Some(ui) = weak.upgrade() {
                    ui.close();
                }
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.win.connect_delete_event(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            Propagation::Stop
        });
        for win in &ui.fallback {
            let weak = Rc::downgrade(&ui);
            win.connect_button_press_event(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.close();
                }
                Propagation::Stop
            });
            win.show_all();
        }
        let weak = Rc::downgrade(&ui);
        ui.wifi.button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let on = ui.state.borrow().wifi.unwrap_or(false);
                ui.write(Action::Wifi(!on));
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.bluetooth.button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let on = ui.state.borrow().bluetooth.unwrap_or(false);
                ui.write(Action::Bluetooth(!on));
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.power.button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if ui.choices.get_visible() {
                    ui.choices.hide();
                } else {
                    for child in ui.choices.children() {
                        child.show_all();
                    }
                    ui.choices.show();
                }
            }
        });
        for (profile, button) in &ui.profiles {
            let weak = Rc::downgrade(&ui);
            let profile = profile.clone();
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.choices.hide();
                    ui.write(Action::Profile(profile.clone()));
                }
            });
        }
        for (button, mode) in [(&ui.wifi.arrow, "network"), (&ui.volume.gear, "audio")] {
            let weak = Rc::downgrade(&ui);
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.detail(mode);
                }
            });
        }
        let weak = Rc::downgrade(&ui);
        ui.volume.button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.write(Action::Mute);
            }
        });
        for (button, action) in [
            (&ui.previous, "previous"),
            (&ui.play, "play-pause"),
            (&ui.next, "next"),
        ] {
            let weak = Rc::downgrade(&ui);
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    let player = ui
                        .state
                        .borrow()
                        .media
                        .as_ref()
                        .map(|media| media.player.clone());
                    if let Some(player) = player {
                        ui.write(Action::Media {
                            player,
                            command: action,
                        });
                    }
                }
            });
        }
        if !headless {
            ui.win.show_all();
        }
        ui.render(State::default());
        ui
    }
    fn write(&self, action: Action) {
        self.pending.borrow_mut().insert(action.domain());
        let _ = self.tx.send(Request::Write(action));
        let state = self.state.borrow().clone();
        self.render(state);
    }
    fn render(&self, state: State) {
        self.wifi.render(
            state.wifi,
            self.pending.borrow().contains(&Domain::Wifi),
            match state.wifi {
                Some(true) => "켜짐",
                Some(false) => "꺼짐",
                None => "사용 불가",
            },
        );
        self.bluetooth.render(
            state.bluetooth,
            self.pending.borrow().contains(&Domain::Bluetooth),
            match state.bluetooth {
                Some(true) => "켜짐",
                Some(false) => "꺼짐",
                None => "사용 불가",
            },
        );
        self.power.render(
            state.profile.as_ref().map(|_| true),
            self.pending.borrow().contains(&Domain::Power),
            state
                .profile
                .as_deref()
                .map(profile_name)
                .unwrap_or("사용 불가"),
        );
        for (profile, button) in &self.profiles {
            button.set_sensitive(state.profiles.contains(profile));
            let style = button.style_context();
            if state.profile.as_ref() == Some(profile) {
                style.add_class("active");
            } else {
                style.remove_class("active");
            }
        }
        self.volume
            .render(state.audio.map(|s| s.0), state.audio.is_some_and(|s| s.1));
        self.brightness.render(state.brightness, false);
        if let Some(media) = &state.media {
            self.title.set_text(&media.title);
            self.title.set_tooltip_text(Some(&media.title));
            self.artist.set_text(&media.artist);
            self.artist.set_visible(!media.artist.is_empty());
            self.artwork.set_url(&media.art_url);
        } else {
            self.title.set_text("재생 중인 미디어 없음");
            self.artist.set_text("");
            self.artist.hide();
            self.artwork.set_url("");
        }
        self.play.set_image(Some(&icon(
            if state.media.as_ref().is_some_and(|media| media.playing) {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            },
            22,
        )));
        for button in [&self.previous, &self.play, &self.next] {
            button.set_sensitive(state.media.is_some());
        }
        *self.state.borrow_mut() = state;
    }
    fn detail(self: &Rc<Self>, mode: &str) {
        (self.open)(mode);
        if mode == "session" {
            let _ = Command::new(
                PathBuf::from(env::var("HOME").unwrap_or_default())
                    .join(".local/bin/desktop-session-menu"),
            )
            .spawn();
            return;
        }
        let weak = Rc::downgrade(self);
        let back = move || {
            if let Some(ui) = weak.upgrade() {
                ui.pages.set_visible_child_name("home");
            }
        };
        if mode == "audio" {
            if self.audio_page.borrow().is_none() {
                let page = crate::AudioPage::new(back);
                page.widget
                    .set_size_request(crate::panel::WIDTH, crate::panel::HEIGHT);
                self.pages.add_named(&page.widget, "audio");
                *self.audio_page.borrow_mut() = Some(page);
            }
            self.pages.set_visible_child_name("audio");
        } else if ["network", "ethernet", "vpn"].contains(&mode) {
            if self.network_page.borrow().is_none() {
                let page = crate::network::Page::new(back);
                page.widget
                    .set_size_request(crate::panel::WIDTH, crate::panel::HEIGHT);
                self.pages.add_named(&page.widget, "network");
                *self.network_page.borrow_mut() = Some(page);
            }
            if let Some(page) = self.network_page.borrow().as_ref() {
                page.select(match mode {
                    "ethernet" => "ethernet",
                    "vpn" => "vpn",
                    _ => "wifi",
                });
            }
            self.pages.set_visible_child_name("network");
        } else if mode == "home" {
            self.pages.set_visible_child_name("home");
        } else if ["bluetooth", "power", "display"].contains(&mode) {
            if self.pages.child_by_name(mode).is_none() {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
                card.style_context().add_class("audio-card");
                card.set_size_request(crate::panel::WIDTH, crate::panel::HEIGHT);
                let title = match mode {
                    "bluetooth" => "블루투스",
                    "power" => "전원",
                    _ => "디스플레이",
                };
                card.pack_start(&crate::panel::header(title, back), false, false, 0);
                card.pack_start(
                    &label("세부 설정은 아직 구현하지 않았습니다.", Some("subtle")),
                    false,
                    false,
                    0,
                );
                card.show_all();
                self.pages.add_named(&card, mode);
            }
            self.pages.set_visible_child_name(mode);
        }
    }
    fn close(&self) {
        if self.persistent.get() {
            self.volume.flush();
            self.brightness.flush();
            if let Some(page) = self.network_page.borrow().as_ref() {
                page.conceal();
            }
            self.dismiss.borrow_mut().take();
            for win in &self.fallback {
                win.hide();
            }
            self.win.hide();
            return;
        }
        if self.closed.replace(true) {
            return;
        }
        self.volume.flush();
        self.brightness.flush();
        self.dismiss.borrow_mut().take();
        for win in &self.fallback {
            win.hide();
        }
        self.win.hide();
        if gtk::main_level() > 0 {
            gtk::main_quit();
        }
    }
    #[allow(deprecated)]
    fn present(self: &Rc<Self>, page: &str) {
        self.detail(page);
        if self.win.is_visible() {
            return;
        }
        let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(self);
        clicks.attach(None, move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
            ControlFlow::Break
        });
        *self.dismiss.borrow_mut() =
            crate::dismiss::Backdrop::new("lucy-control-dismiss", clicked).ok();
        if self.dismiss.borrow().is_none() {
            for win in &self.fallback {
                win.show_all();
            }
        }
        layer::monitor(
            &self.win,
            &focused_monitor(&gdk::Display::default().unwrap()),
        );
        self.win.show();
    }
    fn watch(self: &Rc<Self>) {
        for bus_type in [gio::BusType::System, gio::BusType::Session] {
            let weak = Rc::downgrade(self);
            gio::bus_get(bus_type, gio::Cancellable::NONE, move |result| {
                let (Some(ui), Ok(bus)) = (weak.upgrade(), result) else {
                    return;
                };
                if ui.closed.get() {
                    return;
                }
                let notify = ui.tx.clone();
                let id = bus.signal_subscribe(
                    None,
                    Some("org.freedesktop.DBus.Properties"),
                    Some("PropertiesChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, args| {
                        let Some(interface) = args.child_value(0).get::<String>() else {
                            return;
                        };
                        let domain = match interface.as_str() {
                            "org.freedesktop.NetworkManager"
                            | "org.freedesktop.NetworkManager.Device"
                            | "org.freedesktop.NetworkManager.Connection.Active" => Domain::Wifi,
                            "org.bluez.Adapter1" => Domain::Bluetooth,
                            "org.freedesktop.UPower.PowerProfiles" | "net.hadess.PowerProfiles" => {
                                Domain::Power
                            }
                            "org.mpris.MediaPlayer2.Player" => Domain::Media,
                            _ => return,
                        };
                        let _ = notify.send(Request::Refresh(domain));
                    },
                );
                ui.subscriptions.borrow_mut().push((bus.clone(), id));
                let notify = ui.tx.clone();
                let id = bus.signal_subscribe(
                    Some("org.freedesktop.DBus"),
                    Some("org.freedesktop.DBus"),
                    Some("NameOwnerChanged"),
                    Some("/org/freedesktop/DBus"),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, args| {
                        if let Some((name, _, _)) = args.get::<(String, String, String)>() {
                            let domain = match name.as_str() {
                                "org.bluez" => Some(Domain::Bluetooth),
                                "org.freedesktop.NetworkManager" => Some(Domain::Wifi),
                                "org.freedesktop.UPower.PowerProfiles"
                                | "net.hadess.PowerProfiles" => Some(Domain::Power),
                                name if name.starts_with("org.mpris.MediaPlayer2.") => {
                                    Some(Domain::Media)
                                }
                                _ => None,
                            };
                            if let Some(domain) = domain {
                                let _ = notify.send(Request::Refresh(domain));
                            }
                        }
                    },
                );
                ui.subscriptions.borrow_mut().push((bus, id));
            });
        }
        if let Ok(devices) = fs::read_dir("/sys/class/backlight") {
            for device in devices.flatten() {
                for name in ["brightness", "actual_brightness"] {
                    let file = gio::File::for_path(device.path().join(name));
                    if let Ok(monitor) =
                        file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
                    {
                        let notify = self.tx.clone();
                        monitor.connect_changed(move |_, _, _, _| {
                            let _ = notify.send(Request::Refresh(Domain::Brightness));
                        });
                        self.monitors.borrow_mut().push(monitor);
                    }
                }
            }
        }
    }
}
impl Drop for Ui {
    fn drop(&mut self) {
        for (bus, id) in self.subscriptions.borrow_mut().drain(..) {
            bus.signal_unsubscribe(id);
        }
        for monitor in self.monitors.borrow().iter() {
            monitor.cancel();
        }
        // A detail page can reuse this process and GLib context.
        unsafe {
            for win in &self.fallback {
                win.destroy();
            }
            self.win.destroy();
        }
    }
}

#[allow(deprecated)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if unsafe { layer::gtk_layer_is_supported() } == 0 {
        return Err("Wayland layer-shell required".into());
    }
    let first_page = env::args()
        .skip_while(|arg| arg != "--page")
        .nth(1)
        .unwrap_or_else(|| {
            let args: Vec<_> = env::args().collect();
            if args.iter().any(|a| a == "audio") {
                "audio"
            } else if args.iter().any(|a| a == "network") {
                if args.iter().any(|a| a == "--vpn") {
                    "vpn"
                } else if args.iter().any(|a| a == "--ethernet") {
                    "ethernet"
                } else {
                    "network"
                }
            } else {
                "home"
            }
            .into()
        });
    let socket_path = PathBuf::from(env::var("XDG_RUNTIME_DIR")?).join("lucy-control.sock");
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(env::var("XDG_RUNTIME_DIR")?).join("lucy-control.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let socket = UnixDatagram::unbound()?;
        for attempt in 0..10 {
            match socket.send_to(first_page.as_bytes(), &socket_path) {
                Ok(_) => return Ok(()),
                Err(_) if attempt < 9 => thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(error.into()),
            }
        }
    }
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }
    let socket = UnixDatagram::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let backend = Backend::start(events);
    let ui = Ui::new(backend.tx.clone(), Rc::new(|_| {}), false);
    ui.persistent.set(true);
    ui.detail(&first_page);
    let weak = Rc::downgrade(&ui);
    let socket_source =
        glib::source::unix_fd_add_local(socket.as_raw_fd(), glib::IOCondition::IN, move |_, _| {
            let Some(ui) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut buffer = [0; 32];
            while let Ok(size) = socket.recv(&mut buffer) {
                if let Ok(page) = std::str::from_utf8(&buffer[..size]) {
                    ui.present(page);
                }
            }
            ControlFlow::Continue
        });
    ui.watch();
    let weak = Rc::downgrade(&ui);
    let event_source = receiver.attach(None, move |event| {
        let Some(ui) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        if ui.closed.get() {
            return ControlFlow::Break;
        }
        match event {
            Event::State(state) => ui.render(state),
            Event::Done(domain, success) => {
                ui.pending.borrow_mut().remove(&domain);
                if success {
                    ui.error.hide();
                } else {
                    if domain == Domain::Audio {
                        ui.volume.quiet.set(Instant::now());
                    }
                    if domain == Domain::Brightness {
                        ui.brightness.quiet.set(Instant::now());
                    }
                    ui.error
                        .set_text("변경하지 못했습니다. 장치 연결과 권한을 확인하세요.");
                    ui.error.show();
                }
                let state = ui.state.borrow().clone();
                ui.render(state);
            }
        }
        ControlFlow::Continue
    });
    let mut signal_sources = Vec::new();
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let weak = Rc::downgrade(&ui);
        signal_sources.push(glib::unix_signal_add_local(signal, move || {
            if let Some(ui) = weak.upgrade() {
                ui.persistent.set(false);
                ui.close();
            }
            ControlFlow::Break
        }));
    }
    gtk::main();
    for source in signal_sources
        .into_iter()
        .chain(std::iter::once(event_source))
        .chain(std::iter::once(socket_source))
    {
        if let Some(source) = glib::MainContext::default().find_source_by_id(&source) {
            source.destroy();
        }
    }
    drop(ui);
    drop(backend);
    drop(lock);
    let _ = fs::remove_file(socket_path);
    Ok(())
}

/// Read real state, but route every UI write/detail launch into test channels.
pub fn self_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut state = State::default();
    for domain in DOMAINS {
        refresh(&mut state, domain);
    }
    let (tx, rx) = mpsc::channel();
    let opened = Rc::new(RefCell::new(Vec::new()));
    let record = opened.clone();
    let ui = Ui::new(
        tx,
        Rc::new(move |mode| record.borrow_mut().push(mode.to_owned())),
        true,
    );
    ui.render(state.clone());
    assert!(rx.try_recv().is_err(), "rendering must not write settings");
    assert!(
        !ui.bluetooth.arrow.is_sensitive()
            && !ui.power.arrow.is_sensitive()
            && !ui.brightness.gear.is_sensitive()
    );
    assert!(ui.wifi.arrow.is_sensitive() && ui.volume.gear.is_sensitive());
    if let Some(path) = env::var_os("LUCY_CONTROL_SCREENSHOT") {
        ui.win.show_all();
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            glib::MainContext::default().iteration(false);
            thread::sleep(Duration::from_millis(5));
        }
        let monitor = focused_monitor(&gdk::Display::default().unwrap()).geometry();
        let (w, h) = (ui.win.allocated_width(), ui.win.allocated_height());
        let crop = format!(
            "{},{} {}x{}",
            monitor.x() + monitor.width() - 16 - w,
            monitor.y() + 54,
            w,
            h
        );
        Command::new("grim")
            .args(["-s", "1", "-g", &crop])
            .arg(path)
            .status()?;
    }
    fn next(rx: &mpsc::Receiver<Request>) -> Action {
        match rx.recv_timeout(Duration::from_millis(200)).unwrap() {
            Request::Write(action) => action,
            _ => panic!("expected explicit UI write"),
        }
    }
    if let Some(on) = state.wifi {
        ui.wifi.button.emit_clicked();
        assert_eq!(next(&rx), Action::Wifi(!on));
    }
    if let Some(on) = state.bluetooth {
        ui.bluetooth.button.emit_clicked();
        assert_eq!(next(&rx), Action::Bluetooth(!on));
    }
    ui.power.button.emit_clicked();
    assert!(ui.choices.get_visible());
    if let Some((profile, button)) = ui
        .profiles
        .iter()
        .find(|(profile, _)| state.profiles.contains(profile))
    {
        button.emit_clicked();
        assert_eq!(next(&rx), Action::Profile(profile.clone()));
        assert!(!ui.choices.get_visible());
    }
    let wanted_volume = if ui.volume.scale.value() == 43.0 {
        44
    } else {
        43
    };
    ui.volume.scale.set_value(wanted_volume as f64);
    ui.volume.flush();
    assert_eq!(next(&rx), Action::Volume(wanted_volume));
    ui.volume.button.emit_clicked();
    assert_eq!(next(&rx), Action::Mute);
    let wanted_brightness = if ui.brightness.scale.value() == 51.0 {
        52
    } else {
        51
    };
    ui.brightness.scale.set_value(wanted_brightness as f64);
    ui.brightness.flush();
    assert_eq!(next(&rx), Action::Brightness(wanted_brightness));
    let mut media_state = state.clone();
    media_state.media = Some(Media {
        player: "lucy-validation".into(),
        title: "미디어 검증".into(),
        artist: "Lucy".into(),
        art_url: String::new(),
        playing: true,
    });
    ui.render(media_state);
    assert_eq!(ui.title.text().as_str(), "미디어 검증");
    for (button, action) in [
        (&ui.previous, "previous"),
        (&ui.play, "play-pause"),
        (&ui.next, "next"),
    ] {
        button.emit_clicked();
        assert_eq!(
            next(&rx),
            Action::Media {
                player: "lucy-validation".into(),
                command: action
            }
        );
    }
    ui.wifi.arrow.emit_clicked();
    assert_eq!(ui.pages.visible_child_name().as_deref(), Some("network"));
    let network_widget = ui.network_page.borrow().as_ref().unwrap().widget.clone();
    assert_eq!(
        network_widget
            .toplevel()
            .unwrap()
            .downcast::<gtk::Window>()
            .unwrap(),
        ui.win
    );
    assert_eq!(
        ui.pages
            .toplevel()
            .unwrap()
            .downcast::<gtk::Window>()
            .unwrap(),
        ui.win
    );
    ui.volume.gear.emit_clicked();
    assert_eq!(ui.pages.visible_child_name().as_deref(), Some("audio"));
    assert_eq!(
        ui.audio_page
            .borrow()
            .as_ref()
            .unwrap()
            .widget
            .toplevel()
            .unwrap()
            .downcast::<gtk::Window>()
            .unwrap(),
        ui.win
    );
    assert_eq!(*opened.borrow(), ["network", "audio"]);
    ui.detail("home");
    ui.detail("network");
    assert_eq!(
        ui.network_page.borrow().as_ref().unwrap().widget,
        network_widget,
        "navigation must reuse the original page"
    );
    let art_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/album.svg");
    ui.artwork
        .set_url(glib::filename_to_uri(&art_path, None).unwrap().as_str());
    let start = Instant::now();
    while ui.artwork.widget.pixbuf().is_none() && start.elapsed() < Duration::from_secs(2) {
        glib::MainContext::default().iteration(false);
        thread::sleep(Duration::from_millis(2));
    }
    let art = ui
        .artwork
        .widget
        .pixbuf()
        .expect("album artwork must load off-thread");
    assert_eq!((art.width(), art.height()), (72, 72));
    if let Some(path) = env::var_os("LUCY_CONTROL_SCREENSHOT") {
        for mode in ["audio", "network", "bluetooth", "power", "display", "home"] {
            ui.detail(mode);
            let until = Instant::now() + Duration::from_millis(230);
            while Instant::now() < until {
                glib::MainContext::default().iteration(false);
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(
                (ui.win.allocated_width(), ui.win.allocated_height()),
                (crate::panel::WIDTH, crate::panel::HEIGHT),
                "{mode} must retain the same window size"
            );
            let monitor = focused_monitor(&gdk::Display::default().unwrap()).geometry();
            let crop = format!(
                "{},{} {}x{}",
                monitor.x() + monitor.width() - 16 - crate::panel::WIDTH,
                monitor.y() + 54,
                crate::panel::WIDTH,
                crate::panel::HEIGHT
            );
            Command::new("grim")
                .args(["-s", "1", "-g", &crop])
                .arg(format!("{}.{mode}.png", path.to_string_lossy()))
                .status()?;
        }
    }
    println!("PASS: control center real-state read, Wi-Fi/Bluetooth/profile actions, sliders, media controls, detail routing, disabled future details; no real settings writes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "runs a private fake MPRIS player on the desktop session bus"]
    fn mpris_metadata_and_commands_round_trip() {
        use glib::variant::ToVariant;
        use std::collections::HashMap;
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let xml = gio::DBusNodeInfo::for_xml(
            r#"<node>
          <interface name="org.mpris.MediaPlayer2">
            <property name="Identity" type="s" access="read"/>
            <property name="CanQuit" type="b" access="read"/>
            <property name="CanRaise" type="b" access="read"/>
            <property name="HasTrackList" type="b" access="read"/>
          </interface>
          <interface name="org.mpris.MediaPlayer2.Player">
            <method name="Previous"/><method name="Next"/><method name="PlayPause"/>
            <property name="PlaybackStatus" type="s" access="read"/>
            <property name="Metadata" type="a{sv}" access="read"/>
            <property name="CanControl" type="b" access="read"/>
            <property name="CanPlay" type="b" access="read"/>
            <property name="CanPause" type="b" access="read"/>
            <property name="CanGoNext" type="b" access="read"/>
            <property name="CanGoPrevious" type="b" access="read"/>
            <property name="Position" type="x" access="read"/>
          </interface></node>"#,
        )
        .unwrap();
        let (calls, received) = mpsc::channel();
        let mut registrations = Vec::new();
        for interface in ["org.mpris.MediaPlayer2", "org.mpris.MediaPlayer2.Player"] {
            let calls = calls.clone();
            registrations.push(
                bus.register_object(
                    "/org/mpris/MediaPlayer2",
                    &xml.lookup_interface(interface).unwrap(),
                    move |_, _, _, _, method, _, reply| {
                        let _ = calls.send(method.to_owned());
                        reply.return_value(Some(&().to_variant()));
                    },
                    |_, _, _, _, property| match property {
                        "Identity" => "Lucy test player".to_variant(),
                        "PlaybackStatus" => "Playing".to_variant(),
                        "Metadata" => HashMap::from([
                            ("xesam:title", "Lucy validation track".to_variant()),
                            ("xesam:artist", vec!["Lucy artist"].to_variant()),
                            (
                                "mpris:artUrl",
                                concat!(
                                    "file://",
                                    env!("CARGO_MANIFEST_DIR"),
                                    "/tests/fixtures/album.svg"
                                )
                                .to_variant(),
                            ),
                        ])
                        .to_variant(),
                        "Position" => 0i64.to_variant(),
                        "CanQuit" | "CanRaise" | "HasTrackList" => false.to_variant(),
                        _ => true.to_variant(),
                    },
                    |_, _, _, _, _, _| false,
                )
                .unwrap(),
            );
        }
        let name = "org.mpris.MediaPlayer2.lucy_validation";
        bus.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(name, 0u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
        )
        .unwrap();
        let worker = thread::spawn(|| {
            let metadata = read(
                "playerctl",
                &[
                    "--player",
                    "lucy_validation",
                    "--format",
                    MEDIA_FORMAT,
                    "metadata",
                ],
            )
            .unwrap();
            let media = parse_media(&metadata).unwrap();
            assert_eq!(media.title, "Lucy validation track");
            assert!(media.playing);
            assert_eq!(media.artist, "Lucy artist");
            assert_eq!(media.player, "lucy_validation");
            assert!(media.art_url.ends_with("/tests/fixtures/album.svg"));
            for command in ["previous", "play-pause", "next"] {
                let (program, args) = Action::Media {
                    player: media.player.clone(),
                    command,
                }
                .command();
                assert!(read(
                    program,
                    &args.iter().map(String::as_str).collect::<Vec<_>>()
                )
                .is_some());
            }
        });
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < Duration::from_secs(5) {
            glib::MainContext::default().iteration(false);
            thread::sleep(Duration::from_millis(2));
        }
        let result = worker.join();
        for registration in registrations {
            bus.unregister_object(registration).unwrap();
        }
        bus.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ReleaseName",
            Some(&(name,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
        )
        .unwrap();
        result.unwrap();
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            ["Previous", "PlayPause", "Next"]
        );
        println!("PASS: real playerctl/MPRIS metadata and previous/play-pause/next RPCs against isolated test player");
    }
    #[test]
    fn control_commands_are_literal_and_levels_bounded() {
        assert_eq!(
            Action::Wifi(false).command(),
            ("nmcli", vec!["radio".into(), "wifi".into(), "off".into()])
        );
        assert_eq!(Action::Volume(140).command().1.last().unwrap(), "100%");
        assert_eq!(Action::Brightness(0).command().1.last().unwrap(), "1%");
        assert_eq!(
            Action::Media {
                player: "test".into(),
                command: "play-pause"
            }
            .command(),
            (
                "playerctl",
                vec!["--player".into(), "test".into(), "play-pause".into()]
            )
        );
    }
}
