use crate::{
    audio_backend as audio, bar_backend as bar, control_backend as control, network_backend as net,
};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Home,
    Audio,
    Network,
    Bluetooth,
    Power,
    Display,
    Session,
    TrayMenu,
    Calendar,
    Launcher,
}
impl Page {
    pub fn parse(s: &str) -> Self {
        match s {
            "audio" => Self::Audio,
            "network" | "wifi" | "vpn" | "ethernet" => Self::Network,
            "bluetooth" => Self::Bluetooth,
            "power" => Self::Power,
            "display" => Self::Display,
            "session" => Self::Session,
            "tray" => Self::TrayMenu,
            "calendar" => Self::Calendar,
            "launcher" => Self::Launcher,
            _ => Self::Home,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Slider {
    Volume,
    Brightness,
    Audio(String, u64),
}
#[derive(Clone, Debug)]
pub enum Action {
    Page(Page),
    Wifi,
    Bluetooth,
    Profile,
    Mute,
    Slider(Slider),
    AudioMute(String, u64, bool),
    DeviceMenu(String),
    Device(String, String),
    Tab(String),
    Scan,
    Ap(net::Ap),
    Saved,
    Forget(String),
    CancelDelete,
    ConfirmDelete(String),
    Net(net::Action),
    Secret,
    Eye,
    Submit,
    Cancel,
    Media(&'static str),
    Workspace(u64),
    Launch,
    Search,
    App(String),
    Tray,
    TrayItem(String),
    MenuEvent(i32),
    CalendarOffset(i32),
    NoticeGroup(String),
    NoticeHover,
    Scrollbar(Scrollbar),
    DeleteNotices(Vec<u32>),
    ClearNotices,
    NotificationDnd,
    InvokeNotice(u32, String),
    Session(&'static str),
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Scrollbar {
    pub track: Rect,
    pub thumb: Rect,
    pub max: f64,
}
impl Scrollbar {
    pub fn new(track: Rect, content: f64, viewport: f64, scroll: f64) -> Self {
        let max = (content - viewport).max(0.0);
        let h = (track.h * viewport / content.max(1.0))
            .max(28.0)
            .min(track.h);
        Self {
            track,
            thumb: Rect {
                x: track.x,
                y: track.y + (track.h - h) * scroll.clamp(0.0, max) / max.max(1.0),
                w: track.w,
                h,
            },
            max,
        }
    }
}
impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}
#[derive(Clone)]
pub struct Hit {
    pub rect: Rect,
    pub action: Action,
}
pub fn next_profile(current: Option<&str>, available: &[String]) -> Option<String> {
    let order = ["power-saver", "balanced", "performance"];
    let start = order.iter().position(|p| Some(*p) == current).unwrap_or(2);
    (1..=3)
        .map(|n| order[(start + n) % 3])
        .find(|p| available.iter().any(|a| a == p))
        .map(str::to_owned)
}
pub fn profile_name(s: &str) -> &str {
    match s {
        "power-saver" => "조용",
        "balanced" => "균형",
        "performance" => "성능",
        _ => "사용 불가",
    }
}

pub struct Model {
    pub notifications: super::notifications::State,
    pub popup_notices: Vec<super::notifications::Notice>,
    pub expanded_notifications: std::collections::HashSet<String>,
    pub notification_request: Option<super::notifications::Command>,
    pub pointer: Option<(u32, f64, f64)>,
    pub launcher: super::launcher::Launcher,
    pub app_request: Option<String>,
    icon_values: RefCell<HashMap<String, (f64, f64, Instant)>>,
    animation_until: Cell<Instant>,
    pub radio_pending: Option<bool>,
    pub readonly: bool,
    pub page: Page,
    pub tab: String,
    pub common: control::State,
    pub audio: Value,
    pub network: Option<net::State>,
    pub metrics: bar::State,
    pub workspaces: Vec<Value>,
    pub bar_outputs: Value,
    pub font: String,
    pub hits: HashMap<u32, Vec<Hit>>,
    pub hover: Option<(u32, usize)>,
    pub dragging: Option<(u32, Slider, Rect)>,
    pub scroll_drag: Option<(u32, Scrollbar, f64)>,
    pub scroll: f64,
    pub content_height: f64,
    pub viewport_height: f64,
    pub device_menu: Option<String>,
    pub saved: bool,
    pub deleting: Option<String>,
    pub wifi: Option<net::Ap>,
    pub secret: String,
    pub cursor: usize,
    pub selected_all: bool,
    pub reveal: bool,
    pub entry: bool,
    pub ensure_entry_visible: bool,
    pub preedit: String,
    pub busy: bool,
    pub scanning: bool,
    pub status: String,
    pub auth_prompt: bool,
    pub cover: Option<cairo::ImageSurface>,
    pub cover_url: String,
    pub media_player: String,
    pub media_app_name: String,
    pub media_app_icon: Option<super::tray::Icon>,
    pub tray_open: bool,
    pub tray_items: Vec<super::tray::Item>,
    pub tray_request: Option<(String, bool)>,
    pub tray_event: Option<i32>,
    pub menu_item: String,
    pub menu: Vec<super::tray::MenuEntry>,
    pub calendar_offset: i32,
    calendar_wheel_delta: f64,
    pub common_tx: Option<mpsc::Sender<control::Request>>,
    pub audio_tx: Option<mpsc::Sender<audio::Request>>,
    pub network_tx: Option<mpsc::Sender<net::Request>>,
    pub net_events: Option<glib::Sender<net::Event>>,
    replies: Option<mpsc::Sender<Vec<u8>>>,
    prefill: Option<Vec<u8>>,
    cancel: Arc<AtomicBool>,
    operation: Option<thread::JoinHandle<()>>,
    quiet: Instant,
    last_write: Instant,
    pub visible: bool,
    pub profile_pending: bool,
}
impl Model {
    pub fn wifi_enabled(&self) -> bool {
        self.radio_pending.unwrap_or_else(|| {
            self.network
                .as_ref()
                .map_or(self.common.wifi.unwrap_or(false), |s| s.wifi)
        })
    }
    pub fn animating(&self) -> bool {
        Instant::now() < self.animation_until.get()
    }
    pub fn icon_value(&self, key: &str, target: f64) -> f64 {
        self.motion_value(key, target, Duration::from_millis(180))
    }
    pub fn motion_value(&self, key: &str, target: f64, duration: Duration) -> f64 {
        let now = Instant::now();
        let mut values = self.icon_values.borrow_mut();
        let entry = values
            .entry(key.to_owned())
            .or_insert((target, target, now));
        let t = (now.duration_since(entry.2).as_secs_f64() / duration.as_secs_f64()).min(1.0);
        let current = entry.0 + (entry.1 - entry.0) * (1.0 - (1.0 - t).powi(3));
        if (entry.1 - target).abs() > 0.001 {
            *entry = (current, target, now);
            self.animation_until
                .set(self.animation_until.get().max(now + duration));
        }
        current
    }
    pub fn new() -> Self {
        Self {
            notifications: Default::default(),
            popup_notices: vec![],
            expanded_notifications: Default::default(),
            notification_request: None,
            pointer: None,
            launcher: super::launcher::Launcher::default(),
            app_request: None,
            icon_values: RefCell::new(HashMap::new()),
            animation_until: Cell::new(Instant::now()),
            radio_pending: None,
            readonly: false,
            page: Page::Home,
            tab: "wifi".into(),
            common: control::State::default(),
            audio: Value::Null,
            network: None,
            metrics: bar::State::default(),
            workspaces: vec![],
            bar_outputs: Value::Null,
            font: "Sans 11".into(),
            hits: HashMap::new(),
            hover: None,
            dragging: None,
            scroll_drag: None,
            scroll: 0.0,
            content_height: 0.0,
            viewport_height: 300.0,
            device_menu: None,
            saved: false,
            deleting: None,
            wifi: None,
            secret: String::new(),
            cursor: 0,
            selected_all: false,
            reveal: false,
            entry: false,
            ensure_entry_visible: false,
            preedit: String::new(),
            busy: false,
            scanning: false,
            status: String::new(),
            auth_prompt: false,
            cover: None,
            cover_url: String::new(),
            media_player: String::new(),
            media_app_name: String::new(),
            media_app_icon: None,
            tray_open: false,
            tray_items: vec![],
            tray_request: None,
            tray_event: None,
            menu_item: String::new(),
            menu: vec![],
            calendar_offset: 0,
            calendar_wheel_delta: 0.0,
            common_tx: None,
            audio_tx: None,
            network_tx: None,
            net_events: None,
            replies: None,
            prefill: None,
            cancel: Arc::new(AtomicBool::new(false)),
            operation: None,
            quiet: Instant::now(),
            last_write: Instant::now() - Duration::from_secs(1),
            visible: true,
            profile_pending: false,
        }
    }
    pub fn navigate(&mut self, page: Page) {
        if page == Page::Launcher && self.page != page {
            self.launcher.reset_search();
        }
        if self.page == page {
            self.reveal = false;
            return;
        }
        self.page = page;
        self.scroll_drag = None;
        self.scroll = 0.0;
        self.hover = None;
        self.pointer = None;
        self.device_menu = None;
        self.reveal = false;
        self.entry = page == Page::Launcher
            || (page == Page::Network && (self.wifi.is_some() || self.auth_prompt));
        self.ensure_entry_visible = self.entry;
    }
    pub fn common_state(&mut self, mut next: control::State) {
        if self.profile_pending {
            next.profile = self.common.profile.clone();
        }
        if Instant::now() < self.quiet {
            if let (Some((_, muted)), Some((percent, _))) = (next.audio, self.common.audio) {
                next.audio = Some((percent, muted));
            }
            next.brightness = self.common.brightness;
        }
        self.common = next;
    }
    fn write(&self, a: control::Action) {
        if let Some(tx) = &self.common_tx {
            let _ = tx.send(control::Request::Write(a));
        }
    }
    fn audio_write(&self, args: Vec<String>) {
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(audio::Request::Write(args));
        }
    }
    pub fn hide(&mut self) {
        if self.page == Page::Calendar {
            self.calendar_offset = 0;
            self.calendar_wheel_delta = 0.0;
            self.scroll = 0.0;
        }
        if self.page == Page::Launcher {
            self.launcher.reset_search();
            self.scroll = 0.0;
        }
        self.visible = false;
        self.reveal = false;
        self.hover = None;
        self.pointer = None;
        self.dragging = None;
        self.scroll_drag = None;
    }
    pub fn begin_scroll_drag(&mut self, id: u32, bar: Scrollbar, y: f64) {
        let grab = if y >= bar.thumb.y && y < bar.thumb.y + bar.thumb.h {
            y - bar.thumb.y
        } else {
            bar.thumb.h / 2.0
        };
        self.scroll_drag = Some((id, bar, grab));
        self.drag_scroll(y);
    }
    pub fn drag_scroll(&mut self, y: f64) {
        if let Some((_, bar, grab)) = self.scroll_drag {
            let travel = (bar.track.h - bar.thumb.h).max(0.0);
            self.scroll = if travel > 0.0 {
                ((y - grab - bar.track.y) / travel).clamp(0.0, 1.0) * bar.max
            } else {
                0.0
            };
        }
    }
    pub fn calendar_wheel(&mut self, delta: f64) {
        if !delta.is_finite() {
            return;
        }
        self.calendar_wheel_delta += delta;
        let months = (self.calendar_wheel_delta / 15.0).trunc() as i32;
        self.calendar_offset += months;
        self.calendar_wheel_delta -= f64::from(months) * 15.0;
    }
    pub fn wifi_searching(&self) -> bool {
        self.busy && (self.scanning || self.radio_pending == Some(true)) && !self.auth_prompt
    }
    pub fn show_network_status(&self) -> bool {
        !self.status.is_empty() && !self.wifi_searching()
    }
    pub fn activate(&mut self, a: Action) {
        if self.readonly
            && matches!(
                &a,
                Action::Session(_)
                    | Action::Workspace(_)
                    | Action::Launch
                    | Action::App(_)
                    | Action::TrayItem(_)
                    | Action::MenuEvent(_)
            )
        {
            return;
        }
        match a {
            Action::Page(page) => self.navigate(page),
            Action::Wifi => {
                self.start(net::Action::Radio(!self.wifi_enabled()));
            }
            Action::Bluetooth => self.write(control::Action::Bluetooth(
                !self.common.bluetooth.unwrap_or(false),
            )),
            Action::Profile => {
                if let Some(p) = next_profile(self.common.profile.as_deref(), &self.common.profiles)
                {
                    self.common.profile = Some(p.clone());
                    self.profile_pending = self.common_tx.is_some();
                    self.write(control::Action::Profile(p));
                }
            }
            Action::Mute => self.write(control::Action::Mute),
            Action::AudioMute(kind, id, muted) => self.audio_write(vec![
                format!("set-{kind}-mute"),
                id.to_string(),
                if muted { "0" } else { "1" }.into(),
            ]),
            Action::DeviceMenu(kind) => {
                self.device_menu = if self.device_menu.as_ref() == Some(&kind) {
                    None
                } else {
                    Some(kind)
                };
            }
            Action::Device(kind, name) => {
                self.audio_write(vec![format!("set-default-{kind}"), name]);
                self.device_menu = None;
            }
            Action::Tab(tab) => {
                self.tab = tab;
                self.scroll = 0.0;
                self.entry = false;
            }
            Action::Scan if self.wifi_enabled() && !self.busy => self.start(net::Action::Scan),
            Action::Scan => {}
            Action::Net(a) => self.start(a),
            Action::Ap(ap) => {
                if self.busy {
                    return;
                }
                if ap.active {
                    self.start(net::Action::Disconnect(ap.device));
                    return;
                }
                self.wifi = Some(ap.clone());
                let saved = self
                    .network
                    .as_ref()
                    .and_then(|s| {
                        s.profiles
                            .iter()
                            .find(|p| p.wifi.as_ref().is_some_and(|w| w.matches(&ap)))
                    })
                    .cloned();
                if let Some(p) = saved {
                    self.start(net::Action::SavedWifi {
                        uuid: p.uuid,
                        device: ap.device,
                        bssid: ap.bssid,
                    });
                } else if ap.security.is_empty() || ap.security == "--" {
                    self.start(net::Action::Connect {
                        target: ap.ssid,
                        device: ap.device,
                    });
                } else {
                    self.wifi = Some(ap);
                    self.entry = true;
                    self.ensure_entry_visible = true;
                    self.auth_prompt = false;
                    self.status.clear();
                }
            }
            Action::Saved => self.saved = !self.saved,
            Action::Forget(id) => self.deleting = Some(id),
            Action::CancelDelete => self.deleting = None,
            Action::ConfirmDelete(id) => {
                if self.network.as_ref().is_some_and(|s| {
                    s.profiles
                        .iter()
                        .any(|p| p.uuid == id && p.kind == "802-11-wireless" && !p.active)
                }) {
                    self.deleting = None;
                    self.start(net::Action::Forget(id));
                }
            }
            Action::Secret => self.entry = true,
            Action::Eye => self.reveal = !self.reveal,
            Action::Submit => self.submit(),
            Action::Cancel => {
                self.cancel.store(true, Ordering::Release);
                self.wipe_secret();
            }
            Action::Media(command) => {
                if let Some(p) = &self.common.media {
                    self.write(control::Action::Media {
                        player: p.player.clone(),
                        command,
                    });
                }
            }
            Action::Workspace(id) => bar::action(
                "niri",
                &["msg", "action", "focus-workspace", &id.to_string()],
            ),
            Action::Launch => bar::lucy(&["launcher"]),
            Action::Search => self.entry = true,
            Action::App(id) => {
                self.app_request = Some(id);
                self.hide();
            }
            Action::Tray => self.tray_open = !self.tray_open,
            Action::TrayItem(id) => self.tray_request = Some((id, false)),
            Action::MenuEvent(id) => {
                self.tray_event = Some(id);
                self.visible = false;
            }
            Action::CalendarOffset(offset) => self.calendar_offset += offset,
            Action::NoticeGroup(key) => {
                if !self.expanded_notifications.remove(&key) {
                    self.expanded_notifications.insert(key);
                }
            }
            Action::NoticeHover => {}
            Action::Scrollbar(_) => {}
            Action::DeleteNotices(ids) => {
                if !self.readonly {
                    self.notifications.notices.retain(|n| !ids.contains(&n.id));
                    self.notification_request = Some(super::notifications::Command::Delete(ids));
                }
            }
            Action::ClearNotices => {
                if !self.readonly {
                    self.notification_request = Some(super::notifications::Command::Delete(
                        self.notifications.notices.iter().map(|n| n.id).collect(),
                    ));
                    self.notifications.notices.clear();
                    self.scroll = 0.0;
                }
            }
            Action::NotificationDnd => {
                if !self.readonly {
                    self.notifications.dnd = !self.notifications.dnd;
                    self.notification_request = Some(super::notifications::Command::Dnd);
                }
            }
            Action::InvokeNotice(id, action) => {
                if !self.readonly {
                    self.notification_request =
                        Some(super::notifications::Command::Invoke(id, action));
                }
            }
            Action::Session(command) => {
                match command {
                    "lock" => bar::action("loginctl", &["lock-session"]),
                    "suspend" => bar::action("systemctl", &["suspend"]),
                    "logout" => bar::action("niri", &["msg", "action", "quit"]),
                    "reboot" => bar::action("systemctl", &["reboot"]),
                    "poweroff" => bar::action("systemctl", &["poweroff"]),
                    _ => {}
                }
                self.hide();
            }
            Action::Slider(_) => {}
        }
    }
    pub fn slider(&mut self, target: &Slider, value: f64, force: bool) {
        let v = value.round().clamp(
            if *target == Slider::Brightness {
                1.0
            } else {
                0.0
            },
            100.0,
        ) as u32;
        self.quiet = Instant::now() + Duration::from_millis(400);
        match target {
            Slider::Volume => {
                self.common.audio = Some((f64::from(v), self.common.audio.is_some_and(|a| a.1)))
            }
            Slider::Brightness => self.common.brightness = Some(f64::from(v)),
            Slider::Audio(kind, id) => {
                let collection = collection(kind);
                if let Some(items) = self.audio[collection].as_array_mut() {
                    for item in items {
                        if item["index"].as_u64() == Some(*id) {
                            if let Some(channels) = item["volume"].as_object_mut() {
                                for channel in channels.values_mut() {
                                    channel["value"] =
                                        ((f64::from(v) * 65536.0 / 100.0) as u64).into();
                                }
                            }
                        }
                    }
                }
            }
        }
        if !force && self.last_write.elapsed() < Duration::from_millis(35) {
            return;
        }
        self.last_write = Instant::now();
        match target {
            Slider::Volume => self.write(control::Action::Volume(v)),
            Slider::Brightness => self.write(control::Action::Brightness(v)),
            Slider::Audio(kind, id) => self.audio_write(audio::volume_command(kind, *id, v)),
        }
    }
    pub fn audio_state(&mut self, mut next: Value) {
        if Instant::now() < self.quiet {
            for collection in ["sinks", "sources", "sink-inputs", "source-outputs"] {
                if let (Some(old), Some(new)) = (
                    self.audio[collection].as_array(),
                    next[collection].as_array_mut(),
                ) {
                    for item in new {
                        if let Some(prev) = old.iter().find(|p| p["index"] == item["index"]) {
                            item["volume"] = prev["volume"].clone();
                        }
                    }
                }
            }
        }
        if let Some(name) = next["info"]["default_sink_name"].as_str() {
            if let Some(item) = next["sinks"]
                .as_array()
                .and_then(|s| s.iter().find(|i| i["name"] == name))
            {
                let percent = if Instant::now() < self.quiet {
                    self.common
                        .audio
                        .map_or_else(|| audio::volume(item), |a| a.0)
                } else {
                    audio::volume(item)
                };
                self.common.audio = Some((percent, item["mute"].as_bool().unwrap_or(false)));
            }
        }
        self.audio = next;
    }
    fn wipe_secret(&mut self) {
        unsafe {
            self.secret.as_mut_vec().fill(0);
        }
        self.secret.clear();
        self.cursor = 0;
        self.selected_all = false;
        self.preedit.clear();
    }
    pub fn insert(&mut self, text: &str) {
        if self.page == Page::Launcher {
            self.launcher.insert(text);
            self.scroll = 0.0;
            return;
        }
        if self.entry && self.page == Page::Network && self.secret.len() + text.len() <= 4096 {
            if self.selected_all {
                self.wipe_secret();
            }
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            self.cursor = self.cursor.min(self.secret.len());
            self.secret.insert_str(self.cursor, &text);
            self.cursor += text.len();
        }
    }
    pub fn backspace(&mut self) {
        if self.page == Page::Launcher {
            self.launcher.backspace();
            self.scroll = 0.0;
            return;
        }
        if self.selected_all {
            self.wipe_secret();
            return;
        }
        if self.cursor > 0 {
            let start = self.secret[..self.cursor]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
            self.secret.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }
    pub fn move_cursor(&mut self, direction: i32) {
        if self.page == Page::Launcher {
            self.launcher.move_cursor(direction > 0);
            return;
        }
        self.selected_all = false;
        if direction < 0 {
            self.cursor = self.secret[..self.cursor]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
        } else {
            self.cursor += self.secret[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        }
    }
    pub fn delete(&mut self) {
        if self.page == Page::Launcher {
            self.launcher.delete();
            self.scroll = 0.0;
            return;
        }
        if self.selected_all {
            self.wipe_secret();
        } else if self.cursor < self.secret.len() {
            let end = self.cursor
                + self.secret[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            self.secret.replace_range(self.cursor..end, "");
        }
    }
    pub fn settle(&mut self, domain: control::Domain, success: bool) {
        if success && !self.busy {
            self.status.clear();
        }
        if domain == control::Domain::Power {
            self.profile_pending = false;
        }
        if !success {
            self.quiet = Instant::now();
            self.status = "변경하지 못했습니다. 장치와 권한을 확인하세요.".into();
            if let Some(tx) = &self.common_tx {
                let _ = tx.send(control::Request::Refresh(domain));
            }
        }
    }
    pub fn submit(&mut self) {
        if self.secret.is_empty() {
            return;
        }
        if self.busy {
            if let Some(tx) = &self.replies {
                let _ = tx.send(self.secret.as_bytes().to_vec());
                self.wipe_secret();
                self.auth_prompt = false;
                self.status = "인증 중…".into();
            }
            return;
        }
        if let Some(ap) = self.wifi.clone() {
            self.prefill = Some(self.secret.as_bytes().to_vec());
            self.start(net::Action::Connect {
                target: ap.ssid,
                device: ap.device,
            });
        }
    }
    pub fn start(&mut self, action: net::Action) {
        if self.busy {
            return;
        }
        let Some(events) = self.net_events.clone() else {
            return;
        };
        if let net::Action::Up(uuid) | net::Action::SavedWifi { uuid, .. } = &action {
            if let Some(state) = &self.network {
                if let Some(wifi) = state
                    .profiles
                    .iter()
                    .find(|p| &p.uuid == uuid)
                    .and_then(|p| p.wifi.as_ref())
                {
                    self.wifi = state.aps.iter().find(|ap| wifi.matches(ap)).cloned();
                }
            }
        }
        if let Some(handle) = self.operation.take() {
            let _ = handle.join();
        }
        let attempt = net::WifiAttempt::new(&action, self.network.as_ref());
        self.busy = true;
        self.radio_pending = if let net::Action::Radio(on) = &action {
            Some(*on)
        } else {
            None
        };
        self.scanning = matches!(action, net::Action::Scan);
        self.status = if matches!(action, net::Action::Radio(true)) {
            "Wi-Fi 켜는 중…"
        } else if self.scanning {
            "Wi-Fi 검색 중…"
        } else {
            "연결 처리 중…"
        }
        .into();
        self.auth_prompt = false;
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (replies, rx) = mpsc::channel();
        self.replies = Some(replies);
        let tx = self.network_tx.clone();
        self.operation = Some(thread::spawn(move || {
            let result = if matches!(action, net::Action::Scan) {
                net::scan_access_points(&cancel).map(|aps| {
                    let _ = events.send(net::Event::AccessPoints(aps));
                })
            } else {
                let mut args = vec![
                    "--terse".into(),
                    "--colors".into(),
                    "no".into(),
                    "--ask".into(),
                    "--wait".into(),
                    "86400".into(),
                ];
                args.extend(action.args());
                net::interactive("nmcli", &args, &events, &rx, &cancel, attempt.as_ref()).and_then(
                    |()| {
                        if matches!(action, net::Action::Radio(true))
                            && !cancel.load(Ordering::Acquire)
                        {
                            if let Ok(state) = net::state() {
                                let _ = events.send(net::Event::State(state));
                            }
                            net::scan_access_points(&cancel).map(|aps| {
                                let _ = events.send(net::Event::AccessPoints(aps));
                            })
                        } else {
                            Ok(())
                        }
                    },
                )
            };
            let _ = events.send(net::Event::Finished(result));
            if let Some(tx) = tx {
                let _ = tx.send(net::Request::Refresh);
            }
        }));
    }
    pub fn network_event(&mut self, event: net::Event) {
        match event {
            net::Event::State(s) => {
                if self.radio_pending == Some(true) && s.wifi {
                    self.scanning = true;
                    self.status = "Wi-Fi 검색 중…".into();
                }
                self.network = Some(s);
            }
            net::Event::AccessPoints(aps) => {
                if let Some(s) = &mut self.network {
                    s.aps = aps;
                }
            }
            net::Event::Error(e) => self.status = e,
            net::Event::Prompt(prompt) => {
                let lower = prompt.to_lowercase();
                if lower.contains("psk")
                    || lower.contains("wep-key")
                    || lower.contains("sae-password")
                {
                    if let Some(mut password) = self.prefill.take() {
                        if let Some(tx) = &self.replies {
                            let _ = tx.send(password.clone());
                        }
                        password.fill(0);
                        self.wipe_secret();
                        self.status = "인증 중…".into();
                        return;
                    }
                    self.status = "비밀번호를 확인하고 다시 입력하세요.".into();
                } else {
                    self.status = prompt;
                }
                self.auth_prompt = true;
                self.entry = true;
                self.ensure_entry_visible = true;
                self.reveal = false;
            }
            net::Event::Finished(result) => {
                self.radio_pending = None;
                self.busy = false;
                self.scanning = false;
                self.auth_prompt = false;
                self.replies = None;
                self.wipe_secret();
                if let Some(mut p) = self.prefill.take() {
                    p.fill(0);
                }
                self.status = result.err().unwrap_or_default();
                self.wifi = None;
            }
        }
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.replies = None;
        self.wipe_secret();
        if let Some(mut p) = self.prefill.take() {
            p.fill(0);
        }
        if let Some(h) = self.operation.take() {
            let _ = h.join();
        }
    }
}
pub fn collection(kind: &str) -> &str {
    match kind {
        "sink" => "sinks",
        "source" => "sources",
        "sink-input" => "sink-inputs",
        _ => "source-outputs",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn calendar_reopens_at_current_month_without_old_wheel_delta() {
        let mut m = super::Model::new();
        m.navigate(super::Page::Calendar);
        m.calendar_offset = -9;
        m.calendar_wheel_delta = 7.0;
        m.scroll = 100.0;
        m.hide();
        assert_eq!(m.calendar_offset, 0);
        assert_eq!(m.calendar_wheel_delta, 0.0);
        assert_eq!(m.scroll, 0.0);
    }

    #[test]
    fn network_tab_highlight_moves_between_actual_tab_positions() {
        let m = super::Model::new();
        assert_eq!(m.icon_value("network-tab", 0.0), 0.0);
        assert_eq!(m.icon_value("network-tab", 2.0), 0.0);
        std::thread::sleep(std::time::Duration::from_millis(35));
        let halfway = m.icon_value("network-tab", 2.0);
        assert!(halfway > 0.0 && halfway < 2.0);
        std::thread::sleep(std::time::Duration::from_millis(160));
        assert_eq!(m.icon_value("network-tab", 2.0), 2.0);
    }
    #[test]
    fn launcher_close_clears_search_but_preserves_apps_usage_and_wifi_draft() {
        let mut m = super::Model::new();
        m.secret = "wifi-draft".into();
        m.launcher.usage.insert("test.desktop".into(), 7);
        m.navigate(super::Page::Launcher);
        m.launcher.insert("terminal");
        m.launcher.selected = 3;
        m.launcher.selected_all = true;
        m.scroll = 150.0;
        m.activate(super::Action::App("test.desktop".into()));
        assert!(!m.visible);
        assert_eq!(m.app_request.as_deref(), Some("test.desktop"));
        assert!(m.launcher.query.is_empty());
        assert_eq!((m.launcher.cursor, m.launcher.selected), (0, 0));
        assert!(!m.launcher.selected_all);
        assert_eq!(m.scroll, 0.0);
        assert_eq!(m.launcher.usage["test.desktop"], 7);
        assert_eq!(m.secret, "wifi-draft");
    }
    #[test]
    fn scrollbar_drag_preserves_grab_offset_and_track_click_seeks() {
        use super::{Model, Rect, Scrollbar};
        let mut m = Model::new();
        let bar = Scrollbar::new(
            Rect {
                x: 300.0,
                y: 60.0,
                w: 15.0,
                h: 400.0,
            },
            1600.0,
            400.0,
            0.0,
        );
        m.begin_scroll_drag(1, bar, 80.0);
        assert_eq!(m.scroll, 0.0);
        m.drag_scroll(230.0);
        assert_eq!(m.scroll, 600.0);
        m.drag_scroll(900.0);
        assert_eq!(m.scroll, 1200.0);
        m.drag_scroll(-100.0);
        assert_eq!(m.scroll, 0.0);
        m.begin_scroll_drag(1, bar, 410.0);
        assert_eq!(m.scroll, 1200.0);
        m.hide();
        assert!(m.scroll_drag.is_none());
        let bar = Scrollbar::new(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 15.0,
                h: 400.0,
            },
            400.0,
            400.0,
            0.0,
        );
        m.begin_scroll_drag(1, bar, 200.0);
        assert_eq!(m.scroll, 0.0);
    }
    #[test]
    fn wifi_scan_occupies_tile_status_not_footer_and_preserves_errors() {
        let mut m = super::Model::new();
        m.status = "Wi-Fi 검색 중…".into();
        m.busy = true;
        m.radio_pending = Some(true);
        assert!(m.wifi_searching());
        assert!(!m.show_network_status());
        m.radio_pending = None;
        m.scanning = true;
        assert!(!m.show_network_status());
        m.busy = false;
        m.status = "검색 실패".into();
        assert!(m.show_network_status());
    }
    #[test]
    fn calendar_wheel_accumulates_smooth_deltas_without_scrolling_notifications() {
        let mut m = super::Model::new();
        m.scroll = 80.0;
        m.calendar_wheel(5.0);
        m.calendar_wheel(10.0);
        assert_eq!(m.calendar_offset, 1);
        m.calendar_wheel(-30.0);
        assert_eq!(m.calendar_offset, -1);
        m.calendar_wheel(f64::NAN);
        assert_eq!(m.calendar_offset, -1);
        assert_eq!(m.scroll, 80.0);
    }
    use super::*;
    #[test]
    fn icons_animate_toward_new_values_without_overshoot() {
        let m = Model::new();
        assert_eq!(m.icon_value("volume", 0.0), 0.0);
        assert_eq!(m.icon_value("volume", 100.0), 0.0);
        assert!(m.animating());
        m.icon_values.borrow_mut().get_mut("volume").unwrap().2 =
            Instant::now() - Duration::from_millis(90);
        let midpoint = m.icon_value("volume", 100.0);
        assert!(midpoint > 50.0 && midpoint < 100.0);
        m.icon_values.borrow_mut().get_mut("volume").unwrap().2 =
            Instant::now() - Duration::from_millis(200);
        assert_eq!(m.icon_value("volume", 100.0), 100.0);
    }
    #[test]
    fn profile_cycles_quiet_balanced_performance() {
        let p = vec![
            "power-saver".into(),
            "balanced".into(),
            "performance".into(),
        ];
        assert_eq!(
            next_profile(Some("power-saver"), &p).as_deref(),
            Some("balanced")
        );
        assert_eq!(
            next_profile(Some("balanced"), &p).as_deref(),
            Some("performance")
        );
        assert_eq!(
            next_profile(Some("performance"), &p).as_deref(),
            Some("power-saver")
        );
        assert_eq!(
            next_profile(Some("balanced"), &p[..2]).as_deref(),
            Some("power-saver")
        );
    }
    #[test]
    fn hidden_window_keeps_password_draft() {
        let mut m = Model::new();
        m.navigate(Page::Network);
        m.entry = true;
        m.insert("draft-test");
        m.hide();
        assert_eq!(m.secret, "draft-test");
        assert!(!m.reveal);
    }
}
