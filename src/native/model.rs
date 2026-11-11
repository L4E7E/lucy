use super::bluetooth as bt;
use super::display;
use super::power;
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
    System,
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
            "system" | "monitor" => Self::System,
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
    DisplayBrightness(String),
    Temperature,
    Media(String, String),
    Audio(String, u64),
}
#[derive(Clone, Debug)]
pub enum Action {
    Page(Page),
    MonitorThread(u32),
    Wifi,
    Bluetooth,
    Bt(bt::Request),
    BtForget(Option<String>),
    Profile,
    Power(power::Request),
    PowerExtra,
    PowerSource(bool),
    PowerAwake,
    PowerPicker(power::Timer),
    PowerClosePicker,
    PowerMenu,
    Display(display::Request),
    DisplayMenu(Option<display::Menu>),
    DisplayTab(bool),
    DisplayIdentify,
    DisplayMonitor,
    DisplayTime(bool, display::ClockPart),
    DisplayDrag(String, f64),
    DisplayResetLayout,
    DisplayApplyLayout,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabGroup {
    Network,
    Profile,
    Display,
}
struct WorkspaceWheel {
    output: String,
    delta: f64,
    at: Instant,
    active: u64,
    target: u64,
}
struct ClockWheel {
    start: bool,
    part: display::ClockPart,
    delta: f64,
    at: Instant,
}
fn tab_group(action: &Action) -> Option<TabGroup> {
    match action {
        Action::Tab(_) => Some(TabGroup::Network),
        Action::Power(power::Request::Profile(_)) => Some(TabGroup::Profile),
        Action::DisplayTab(_) => Some(TabGroup::Display),
        _ => None,
    }
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

#[derive(Default)]
pub struct PinInput {
    pub value: String,
    pub cursor: usize,
    pub selected_all: bool,
}
impl PinInput {
    pub fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if (if self.selected_all {
            0
        } else {
            self.value.len()
        }) + text.len()
            > 64
        {
            return;
        }
        if self.selected_all {
            *self = Self::default();
        }
        self.value.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }
    pub fn backspace(&mut self) {
        if self.selected_all {
            *self = Self::default();
            return;
        }
        let end = self.cursor;
        self.move_cursor(-1);
        self.value.replace_range(self.cursor..end, "");
    }
    pub fn delete(&mut self) {
        if self.selected_all {
            *self = Self::default();
            return;
        }
        let start = self.cursor;
        self.move_cursor(1);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    pub fn move_cursor(&mut self, direction: i32) {
        self.selected_all = false;
        if direction < 0 {
            self.cursor = self.value[..self.cursor]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
        } else {
            self.cursor += self.value[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        }
    }
}
pub const SESSION_ACTIONS: [(&str, &str, &str); 5] = [
    ("잠금", "lock", "lock"),
    ("절전", "suspend", "sleep"),
    ("로그아웃", "logout", "logout"),
    ("다시 시작", "reboot", "restart"),
    ("전원 끄기", "poweroff", "power"),
];
pub fn session_shortcut(sym: u32) -> Option<&'static str> {
    let index = match sym {
        0x31..=0x35 => sym - 0x31,
        0xffb1..=0xffb5 => sym - 0xffb1,
        _ => return None,
    };
    Some(SESSION_ACTIONS[index as usize].1)
}
#[derive(Clone)]
struct DisplayDrag {
    name: String,
    factor: f64,
    pointer: (f64, f64),
    origin: (i32, i32),
    baseline: Vec<display::Placement>,
    floating: (i32, i32),
    preview: Option<Vec<display::Placement>>,
}
struct DisplaySliderDraft {
    target: Slider,
    identity: Option<String>,
    percent: f64,
    sent: Option<(u32, u64)>,
}
pub struct Model {
    pub display: display::State,
    pub display_tx: Option<mpsc::Sender<display::Request>>,
    pub display_menu: Option<display::Menu>,
    pub display_night: bool,
    pub display_identify: bool,
    display_clock_wheel: Option<ClockWheel>,
    display_time_draft: Option<(u32, u32, Instant)>,
    display_slider_drafts: Vec<DisplaySliderDraft>,
    display_slider_serial: u64,
    display_last_write: Instant,
    pub display_layout: Vec<display::Placement>,
    display_drag: Option<DisplayDrag>,
    pub gamma_supported: bool,
    pub gamma_active: bool,
    pub gamma_error: String,
    pub power: power::State,
    pub power_tx: Option<mpsc::Sender<power::Request>>,
    pub power_extra: bool,
    pub power_ac: bool,
    pub power_awake: bool,
    pub power_idle_supported: bool,
    pub power_picker: Option<power::Timer>,
    pub bluetooth: bt::State,
    pub bluetooth_tx: Option<glib::Sender<bt::Request>>,
    pub bt_input: PinInput,
    pub bt_deleting: Option<String>,
    pub notifications: super::notifications::State,
    pub popup_notices: Vec<super::notifications::Notice>,
    pub expanded_notifications: std::collections::HashSet<String>,
    pub notice_revision: u64,
    pub notice_exit: Option<Rect>,
    pub removed_notices: std::collections::HashSet<u32>,
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
    pub osd: Option<super::osd::State>,
    pub audio: Value,
    pub network: Option<net::State>,
    pub metrics: bar::State,
    pub monitor: super::monitor::State,
    pub monitor_tx: Option<mpsc::Sender<super::monitor::Request>>,
    pub workspaces: Vec<Value>,
    workspace_wheel: Option<WorkspaceWheel>,
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
    pub preedit_cursor: (i32, i32),
    pub busy: bool,
    pub scanning: bool,
    wifi_scan_pending: bool,
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
    tab_wheel_delta: f64,
    tab_wheel_group: Option<TabGroup>,
    tab_wheel_time: Instant,
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
    media_seek_draft: Option<(String, String, i64, Instant)>,
}
impl Model {
    pub fn delete_surrounding(&mut self, before: u32, after: u32) {
        let (value, cursor, selected) = match self.page {
            Page::Launcher => (
                &mut self.launcher.query,
                &mut self.launcher.cursor,
                self.launcher.selected_all,
            ),
            Page::Bluetooth => (
                &mut self.bt_input.value,
                &mut self.bt_input.cursor,
                self.bt_input.selected_all,
            ),
            Page::Network if self.entry => (&mut self.secret, &mut self.cursor, self.selected_all),
            _ => return,
        };
        if super::ime::delete_surrounding(value, cursor, selected, before, after)
            && self.page == Page::Launcher
        {
            self.launcher.selected = 0;
            self.launcher.keyboard_focus = !self.launcher.query.is_empty();
            self.scroll = 0.0;
        }
    }
    pub fn display_layout_changed(&self) -> bool {
        !self.display_layout.is_empty()
            && self.display.outputs.iter().any(|o| {
                self.display_layout
                    .iter()
                    .find(|p| p.identity == o.identity)
                    .is_some_and(|p| p.x != o.x || p.y != o.y)
            })
    }
    pub fn display_dragging(&self) -> bool {
        self.display_drag.is_some()
    }
    pub fn display_drop_target(&self, name: &str) -> bool {
        self.display_drag
            .as_ref()
            .is_some_and(|d| d.name == name && d.preview.is_some())
    }
    pub fn display_snap_preview(&self) -> Option<&[display::Placement]> {
        self.display_drag.as_ref()?.preview.as_deref()
    }
    pub fn display_floating(&self) -> Option<(&str, i32, i32)> {
        let drag = self.display_drag.as_ref()?;
        Some((&drag.name, drag.floating.0, drag.floating.1))
    }
    pub fn begin_display_drag(&mut self, name: String, factor: f64, x: f64, y: f64) {
        if self.display.outputs.len() < 2
            || self.display.pending.is_some()
            || !factor.is_finite()
            || factor <= 0.0
        {
            return;
        }
        if self.display_layout.is_empty() {
            self.display_layout = self
                .display
                .outputs
                .iter()
                .map(display::Placement::from_output)
                .collect();
        }
        if let Some(p) = self.display_layout.iter().find(|p| p.name == name) {
            self.display_drag = Some(DisplayDrag {
                name,
                factor,
                pointer: (x, y),
                origin: (p.x, p.y),
                baseline: self.display_layout.clone(),
                floating: (p.x, p.y),
                preview: None,
            });
        }
    }
    pub fn drag_display(&mut self, x: f64, y: f64) {
        let Some(drag) = self.display_drag.as_mut() else {
            return;
        };
        let x = drag.origin.0 + ((x - drag.pointer.0) / drag.factor).round() as i32;
        let y = drag.origin.1 + ((y - drag.pointer.1) / drag.factor).round() as i32;
        drag.floating = (x.clamp(-16000, 16000), y.clamp(-16000, 16000));
        let Some(output) = self.display.outputs.iter().find(|o| o.name == drag.name) else {
            return;
        };
        let center = (
            f64::from(x) + f64::from(output.logical_width) * 0.5,
            f64::from(y) + f64::from(output.logical_height) * 0.5,
        );
        let margin = if drag.preview.is_some() { 28.0 } else { 20.0 } / drag.factor;
        let near_edge = drag
            .baseline
            .iter()
            .filter(|p| p.name != drag.name)
            .any(|p| {
                let Some(o) = self
                    .display
                    .outputs
                    .iter()
                    .find(|o| o.identity == p.identity)
                else {
                    return false;
                };
                let (left, top) = (f64::from(p.x), f64::from(p.y));
                let (right, bottom) = (
                    left + f64::from(o.logical_width),
                    top + f64::from(o.logical_height),
                );
                ((center.0 - left).abs().min((center.0 - right).abs()) <= margin
                    && center.1 >= top - margin
                    && center.1 <= bottom + margin)
                    || ((center.1 - top).abs().min((center.1 - bottom).abs()) <= margin
                        && center.0 >= left - margin
                        && center.0 <= right + margin)
            });
        drag.preview = if near_edge {
            display::snap_layout(
                &self.display.outputs,
                &drag.baseline,
                &drag.name,
                x.clamp(-16000, 16000),
                y.clamp(-16000, 16000),
            )
            .filter(|draft| {
                draft.iter().any(|p| {
                    drag.baseline
                        .iter()
                        .find(|old| old.identity == p.identity)
                        .is_some_and(|old| old.x != p.x || old.y != p.y)
                })
            })
        } else {
            None
        };
    }
    pub fn end_display_drag(&mut self) {
        if let Some(drag) = self.display_drag.take() {
            if let Some(preview) = drag.preview {
                self.display_layout = preview;
            }
        }
    }
    pub fn display_send(&mut self, request: display::Request) {
        match &request {
            display::Request::Night(v) => self.display.settings.night.enabled = *v,
            display::Request::Automatic(v) => self.display.settings.night.automatic = *v,
            display::Request::Time(start, part, steps) => {
                if let Err(e) = self.display.settings.night.adjust(*start, *part, *steps) {
                    self.display.error = e;
                    return;
                }
                self.display.error.clear();
                self.display_time_draft = Some((
                    self.display.settings.night.start,
                    self.display.settings.night.end,
                    Instant::now(),
                ));
            }
            _ => {}
        }
        if !self.readonly {
            if let Some(tx) = &self.display_tx {
                let _ = tx.send(request);
            }
        }
    }
    pub fn display_state(&mut self, mut state: display::State) {
        self.display_slider_serial = self.display_slider_serial.max(state.slider_serial);
        if let Some((start, end, at)) = self.display_time_draft {
            if !state.error.is_empty()
                || (state.settings.night.start == start && state.settings.night.end == end)
                || at.elapsed() > Duration::from_secs(3)
            {
                self.display_time_draft = None;
            } else {
                state.settings.night.start = start;
                state.settings.night.end = end;
            }
        }
        if (!display::same_layout(&state.outputs, &self.display.outputs))
            || state
                .outputs
                .iter()
                .map(|o| &o.identity)
                .collect::<Vec<_>>()
                != self
                    .display
                    .outputs
                    .iter()
                    .map(|o| &o.identity)
                    .collect::<Vec<_>>()
        {
            self.display_layout.clear();
            self.display_drag = None;
            self.display_menu = None;
        }
        if matches!(self.dragging, Some((_, Slider::Brightness, _))) || Instant::now() < self.quiet
        {
            for output in &mut state.outputs {
                if !matches!(output.control, Some(display::Brightness::Backlight(_)))
                    || self
                        .display_slider_drafts
                        .iter()
                        .any(|draft| draft.identity.as_ref() == Some(&output.identity))
                {
                    continue;
                }
                if let Some(old) = self
                    .display
                    .outputs
                    .iter()
                    .find(|o| o.identity == output.identity)
                {
                    output.brightness = old.brightness;
                }
            }
        }
        self.display_slider_drafts.retain(|draft| {
            if let Slider::DisplayBrightness(name) = &draft.target {
                if !state.outputs.iter().any(|o| {
                    &o.name == name
                        && Some(&o.identity) == draft.identity.as_ref()
                        && o.control.is_some()
                }) {
                    return false;
                }
            }
            let dragging = self
                .dragging
                .as_ref()
                .is_some_and(|(_, target, _)| target == &draft.target);
            dragging
                || draft
                    .sent
                    .is_none_or(|(_, serial)| state.slider_serial < serial)
        });
        for draft in &self.display_slider_drafts {
            match &draft.target {
                Slider::DisplayBrightness(name) => {
                    if let Some(o) = state.outputs.iter_mut().find(|o| &o.name == name) {
                        o.brightness = Some(draft.percent);
                    }
                }
                Slider::Temperature => {
                    state.settings.night.kelvin = self.display.settings.night.kelvin;
                }
                _ => unreachable!(),
            }
        }
        self.display = state;
    }
    pub fn display_gauge_value(&self, target: &Slider, value: f64) -> f64 {
        let value = self
            .display_slider_drafts
            .iter()
            .find(|draft| &draft.target == target)
            .map_or(value, |draft| draft.percent);
        let key = match target {
            Slider::DisplayBrightness(name) => format!(
                "display-brightness-{}",
                self.display
                    .outputs
                    .iter()
                    .find(|o| &o.name == name)
                    .map_or(name.as_str(), |o| o.identity.as_str())
            ),
            Slider::Temperature => "display-temperature".into(),
            _ => return value,
        };
        self.gauge_motion(&key, value, self.dragging_slider(target))
    }
    fn dragging_slider(&self, target: &Slider) -> bool {
        self.dragging.as_ref().is_some_and(|(_, t, _)| t == target)
    }
    fn gauge_motion(&self, key: &str, value: f64, direct: bool) -> f64 {
        let value = if value.is_finite() {
            value.clamp(0.0, 100.0)
        } else {
            0.0
        };
        if direct {
            // Pointer samples never chase an easing curve. Seed the next wheel
            // or backend transition from the exact last displayed position.
            self.icon_values
                .borrow_mut()
                .insert(key.into(), (value, value, Instant::now()));
            value
        } else {
            self.motion_value(key, value, Duration::from_millis(100))
        }
    }
    pub fn slider_value(&self, target: &Slider, value: f64) -> f64 {
        if matches!(target, Slider::DisplayBrightness(_) | Slider::Temperature) {
            return self.display_gauge_value(target, value);
        }
        // Playback already has its own monotonic microsecond clock and seek
        // acknowledgement. Do not turn it into an endlessly retargeted easing.
        if matches!(target, Slider::Media(_, _)) {
            return value;
        }
        self.gauge_motion(
            &format!("gauge:{target:?}"),
            value,
            self.dragging_slider(target),
        )
    }
    pub fn slider_level(&self, target: &Slider) -> f64 {
        match target {
            Slider::Volume => self.common.audio.map_or(0.0, |a| a.0),
            Slider::Brightness => self.common.brightness.unwrap_or(0.0),
            Slider::DisplayBrightness(name) => self
                .display
                .outputs
                .iter()
                .find(|o| &o.name == name)
                .and_then(|o| o.brightness)
                .unwrap_or(0.0),
            Slider::Temperature => self.display.settings.night.percent(),
            Slider::Media(_, _) => self.media_progress(),
            Slider::Audio(kind, id) => self.audio[collection(kind)]
                .as_array()
                .and_then(|v| v.iter().find(|a| a["index"].as_u64() == Some(*id)))
                .map_or(0.0, audio::volume),
        }
    }
    pub fn readout_value(&self, key: &str, value: f64) -> f64 {
        self.gauge_motion(&format!("gauge:{key}"), value, false)
    }
    pub fn reset_readout(&self, key: &str, value: f64) {
        self.gauge_motion(&format!("gauge:{key}"), value, true);
    }
    fn display_slider(&mut self, target: &Slider, value: f64, force: bool) {
        if !value.is_finite() {
            return;
        }
        let (percent, identity, v) = match target {
            Slider::DisplayBrightness(name) => {
                let Some(o) = self
                    .display
                    .outputs
                    .iter_mut()
                    .find(|o| &o.name == name && o.control.is_some())
                else {
                    return;
                };
                let percent = value.clamp(1.0, 100.0);
                o.brightness = Some(percent);
                if matches!(o.control, Some(display::Brightness::Backlight(_))) {
                    self.common.brightness = o.brightness;
                }
                (percent, Some(o.identity.clone()), percent.round() as u32)
            }
            Slider::Temperature => {
                let percent = value.clamp(0.0, 100.0);
                let kelvin = display::MIN_KELVIN
                    + ((percent / 100.0 * f64::from(display::MAX_KELVIN - display::MIN_KELVIN)
                        / 50.0)
                        .round() as u32)
                        * 50;
                self.display.settings.night.kelvin = kelvin;
                (percent, None, kelvin)
            }
            _ => unreachable!(),
        };
        // Select immediate presentation for pointer samples too. Start this
        // finite window only on input, never from a frame that samples the gauge.
        self.animation_until.set(
            self.animation_until
                .get()
                .max(Instant::now() + Duration::from_millis(100)),
        );
        let index = self
            .display_slider_drafts
            .iter()
            .position(|draft| &draft.target == target);
        let draft = if let Some(index) = index {
            &mut self.display_slider_drafts[index]
        } else {
            self.display_slider_drafts.push(DisplaySliderDraft {
                target: target.clone(),
                identity: identity.clone(),
                percent,
                sent: None,
            });
            self.display_slider_drafts.last_mut().unwrap()
        };
        let unchanged = draft.identity == identity && draft.sent.is_some_and(|(old, _)| old == v);
        draft.identity = identity;
        draft.percent = percent;
        if !unchanged {
            draft.sent = None;
        }
        // Drawing never enters this path. Backend work is driven only by input,
        // rate limited during dragging, coalesced by the worker, and flushed on release.
        if self.readonly
            || (!force
                && (unchanged || self.display_last_write.elapsed() < Duration::from_millis(35)))
        {
            return;
        }
        let Some(tx) = &self.display_tx else {
            return;
        };
        self.display_slider_serial += 1;
        let serial = self.display_slider_serial;
        let request = match target {
            Slider::DisplayBrightness(name) => {
                display::Request::Brightness(name.clone(), v, serial)
            }
            Slider::Temperature => display::Request::Temperature(v, serial),
            _ => unreachable!(),
        };
        match tx.send(request) {
            Ok(()) => {
                draft.sent = Some((v, serial));
                self.display_last_write = Instant::now();
            }
            Err(_) => {
                self.display.error = "디스플레이 제어 연결이 종료되었습니다".into();
                self.display_slider_drafts.clear();
            }
        }
    }
    pub fn power_send(&self, request: power::Request) {
        if !self.readonly {
            if let Some(tx) = &self.power_tx {
                let _ = tx.send(request);
            }
        }
    }
    pub fn power_state(&mut self, state: power::State) {
        if !self.power.ready {
            self.power_ac = state.on_battery != Some(true);
        }
        if !state.busy {
            self.profile_pending = false;
            self.common.profile = state.profile.clone();
            self.common.profiles = state.profiles.clone();
        }
        self.power = state;
    }
    pub fn bt_send(&self, request: bt::Request) {
        if let Some(tx) = &self.bluetooth_tx {
            // Preview is strictly read-only, including automatic discovery.
            if !self.readonly {
                let _ = tx.send(request);
            }
        }
    }
    pub fn bluetooth_state(&mut self, state: bt::State) {
        let previous = self.bluetooth.prompt.as_ref().map(|p| p.id);
        let next = state.prompt.as_ref().map(|p| p.id);
        if previous != next {
            self.bt_input = PinInput::default();
            self.preedit.clear();
        }
        self.bluetooth = state;
        self.common.bluetooth = self
            .bluetooth
            .adapter
            .as_ref()
            .map(|_| self.bluetooth.powered);
        self.common.bluetooth_devices = Arc::new(
            self.bluetooth
                .devices
                .iter()
                .filter(|d| d.connected)
                .map(|d| d.name.clone())
                .collect(),
        );
        if self.page == Page::Bluetooth {
            self.entry = self
                .bluetooth
                .prompt
                .as_ref()
                .is_some_and(bt::Prompt::input);
            if previous != next && next.is_some() {
                self.ensure_entry_visible = true;
            }
        }
    }
    pub fn bt_submit(&mut self) {
        if let Some(prompt) = &self.bluetooth.prompt {
            self.bt_send(bt::Request::Reply(
                prompt.id,
                Some(self.bt_input.value.clone()),
            ));
        }
    }
    pub fn spinning(&self) -> bool {
        (self.busy && !self.auth_prompt)
            || (self.page == Page::Power && self.power.busy)
            || (self.page == Page::Display && !self.display.ready)
            || (self.page == Page::Bluetooth && self.bluetooth.spinning())
    }
    pub fn wifi_enabled(&self) -> bool {
        self.radio_pending.unwrap_or_else(|| {
            self.network
                .as_ref()
                .map_or(self.common.wifi.unwrap_or(false), |s| s.wifi)
        })
    }
    fn scan_wifi_on_entry(&mut self) -> bool {
        if !self.wifi_scan_pending || self.readonly || self.busy {
            return false;
        }
        if self.network.is_none() && self.common.wifi.is_none() {
            return false; // Wait for the first snapshot, not a timer.
        }
        self.wifi_scan_pending = false;
        if !self.wifi_enabled() {
            return false;
        }
        self.start(net::Action::Scan);
        true
    }
    pub fn animating(&self) -> bool {
        Instant::now() < self.animation_until.get()
    }
    pub fn icon_value(&self, key: &str, target: f64) -> f64 {
        self.motion_value(key, target, Duration::from_millis(180))
    }
    pub fn tray_progress(&self) -> f64 {
        self.motion_value(
            "tray-open",
            if self.tray_open { 1.0 } else { 0.0 },
            Duration::from_millis(260),
        )
    }
    pub fn power_extra_progress(&self) -> f64 {
        self.motion_value(
            "power-extra-open",
            if self.power_extra { 1.0 } else { 0.0 },
            Duration::from_millis(320),
        )
    }
    pub fn tray_animating(&self) -> bool {
        self.icon_values
            .borrow()
            .get("tray-open")
            .is_some_and(|(from, to, start)| {
                (from - to).abs() > 0.001 && start.elapsed() < Duration::from_millis(260)
            })
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
            display: display::State::default(),
            display_tx: None,
            display_menu: None,
            display_night: false,
            display_identify: false,
            display_clock_wheel: None,
            display_time_draft: None,
            display_slider_drafts: vec![],
            display_slider_serial: 0,
            display_last_write: Instant::now() - Duration::from_secs(1),
            display_layout: vec![],
            display_drag: None,
            gamma_supported: false,
            gamma_active: false,
            gamma_error: String::new(),
            power: power::State::default(),
            osd: None,
            power_tx: None,
            power_extra: false,
            power_ac: true,
            power_awake: false,
            power_idle_supported: false,
            power_picker: None,
            bluetooth: bt::State::default(),
            bluetooth_tx: None,
            bt_input: PinInput::default(),
            bt_deleting: None,
            notifications: Default::default(),
            popup_notices: vec![],
            expanded_notifications: Default::default(),
            notice_revision: 0,
            notice_exit: None,
            removed_notices: Default::default(),
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
            monitor: super::monitor::State::default(),
            monitor_tx: None,
            workspaces: vec![],
            workspace_wheel: None,
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
            preedit_cursor: (0, 0),
            busy: false,
            scanning: false,
            wifi_scan_pending: false,
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
            tab_wheel_delta: 0.0,
            tab_wheel_group: None,
            tab_wheel_time: Instant::now(),
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
            media_seek_draft: None,
        }
    }
    pub fn navigate(&mut self, page: Page) {
        if let Some(tx) = &self.monitor_tx {
            let _ = tx.send(super::monitor::Request::Visible(page == Page::System));
        }
        if page != Page::Display {
            self.display_menu = None;
        }
        if page == Page::Display && (self.page != page || !self.visible) {
            self.display_send(display::Request::Refresh);
        }
        if page != Page::Power {
            self.power_picker = None;
        }
        self.bt_send(bt::Request::Visible(page == Page::Bluetooth));
        if page == Page::Network && self.tab == "wifi" {
            if self.page != page || !self.visible {
                self.wifi_scan_pending = !self.readonly && !self.busy;
                self.scan_wifi_on_entry();
            }
        } else {
            self.wifi_scan_pending = false;
        }
        if page == Page::Launcher && self.page != page {
            self.launcher.reset_search();
        }
        if self.page == page {
            self.reveal = false;
            if page == Page::Bluetooth {
                self.ensure_entry_visible = self.entry;
            }
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
            || (page == Page::Bluetooth
                && self
                    .bluetooth
                    .prompt
                    .as_ref()
                    .is_some_and(bt::Prompt::input))
            || (page == Page::Network && (self.wifi.is_some() || self.auth_prompt));
        self.ensure_entry_visible = self.entry;
    }
    pub fn common_state(&mut self, mut next: control::State) {
        if let Some((player, track, position, at)) = &self.media_seek_draft {
            if let Some(media) = next
                .media
                .as_mut()
                .filter(|m| &m.player == player && &m.track == track)
            {
                let dragging =
                    matches!(&self.dragging,Some((_,Slider::Media(p,t),_))if p==player&&t==track);
                let now = Instant::now();
                let elapsed = if media.playing {
                    now.saturating_duration_since(*at).as_secs_f64() * media.rate * 1_000_000.0
                } else {
                    0.0
                };
                let expected = (*position as f64 + elapsed)
                    .clamp(0.0, media.duration.unwrap_or(i64::MAX) as f64)
                    as i64;
                let acknowledged = !dragging
                    && media.sampled.is_some_and(|t| t >= *at)
                    && media
                        .position_at(now)
                        .is_some_and(|p| p.abs_diff(expected) <= 500_000);
                if dragging
                    || (!acknowledged && at.elapsed() < Duration::from_secs(3))
                    || media.sampled.is_none_or(|t| t < *at)
                {
                    media.position = Some(*position);
                    media.sampled = Some(*at);
                } else {
                    self.media_seek_draft = None;
                }
            } else {
                self.media_seek_draft = None;
            }
        }
        // Cached updates from other domains cannot acknowledge a seek or
        // replace a newer playback clock. Apply this after seek validation.
        if let (Some(old), Some(incoming)) = (&self.common.media, &mut next.media) {
            if old.player == incoming.player
                && old.track == incoming.track
                && incoming.sampled < old.sampled
            {
                incoming.position = old.position;
                incoming.sampled = old.sampled;
                incoming.playing = old.playing;
                incoming.rate = old.rate;
            }
        }
        if self.power.ready {
            next.profile = self.common.profile.clone();
            next.profiles = self.power.profiles.clone();
        }
        if self.bluetooth.ready {
            next.bluetooth = self
                .bluetooth
                .adapter
                .as_ref()
                .map(|_| self.bluetooth.powered);
            next.bluetooth_devices = Arc::new(
                self.bluetooth
                    .devices
                    .iter()
                    .filter(|d| d.connected)
                    .map(|d| d.name.clone())
                    .collect(),
            );
        }
        if self.profile_pending {
            next.profile = self.common.profile.clone();
        }
        if Instant::now() < self.quiet || self.dragging_slider(&Slider::Volume) {
            if let (Some((_, muted)), Some((percent, _))) = (next.audio, self.common.audio) {
                next.audio = Some((percent, muted));
            }
        }
        if Instant::now() < self.quiet || self.dragging_slider(&Slider::Brightness) {
            next.brightness = self.common.brightness;
        }
        if !matches!(self.dragging, Some((_, Slider::DisplayBrightness(_), _))) {
            let internal = self
                .display
                .outputs
                .iter()
                .filter(|o| matches!(o.control, Some(display::Brightness::Backlight(_))))
                .count();
            if internal == 1 {
                for output in &mut self.display.outputs {
                    if matches!(output.control, Some(display::Brightness::Backlight(_)))
                        && !self.display_slider_drafts.iter().any(|draft| {
                            draft.identity.as_ref() == Some(&output.identity)
                                && draft.target == Slider::DisplayBrightness(output.name.clone())
                        })
                    {
                        output.brightness = next.brightness;
                    }
                }
            }
        }
        self.common = next;
        self.scan_wifi_on_entry();
    }
    fn write(&self, a: control::Action) {
        if !self.readonly {
            if let Some(tx) = &self.common_tx {
                let _ = tx.send(control::Request::Write(a));
            }
        }
    }
    fn audio_write(&self, args: Vec<String>) {
        if !self.readonly {
            if let Some(tx) = &self.audio_tx {
                let _ = tx.send(audio::Request::Write(args));
            }
        }
    }
    pub fn hide(&mut self) {
        if let Some(tx) = &self.monitor_tx {
            let _ = tx.send(super::monitor::Request::Visible(false));
        }
        self.display_drag = None;
        self.display_menu = None;
        self.wifi_scan_pending = false;
        self.power_picker = None;
        self.bt_send(bt::Request::Visible(false));
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
        self.power_picker = None;
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
    pub fn workspace_scroll_target(&self) -> Option<u64> {
        self.workspace_wheel.as_ref().map(|w| w.target)
    }
    pub fn scroll_workspaces(
        &mut self,
        window: u32,
        output: &str,
        x: f64,
        y: f64,
        delta: f64,
    ) -> bool {
        if !delta.is_finite() || delta == 0.0 || output.is_empty() {
            return false;
        }
        let Some(hits) = self.hits.get(&window) else {
            return false;
        };
        let mut region: Option<Rect> = None;
        for h in hits
            .iter()
            .filter(|h| matches!(h.action, Action::Workspace(_)))
        {
            region = Some(match region {
                None => h.rect,
                Some(r) => Rect {
                    x: r.x.min(h.rect.x),
                    y: r.y.min(h.rect.y),
                    w: (r.x + r.w).max(h.rect.x + h.rect.w) - r.x.min(h.rect.x),
                    h: (r.y + r.h).max(h.rect.y + h.rect.h) - r.y.min(h.rect.y),
                },
            });
        }
        if !region.is_some_and(|r| r.contains(x, y)) {
            return false;
        }
        let mut spaces: Vec<_> = self
            .workspaces
            .iter()
            .filter(|w| w["output"].as_str() == Some(output) && w["id"].as_u64().is_some())
            .collect();
        spaces.sort_by_key(|w| w["idx"].as_u64().unwrap_or(0));
        let Some(active) = spaces
            .iter()
            .find(|w| w["is_active"] == true)
            .and_then(|w| w["id"].as_u64())
        else {
            return false;
        };
        let now = Instant::now();
        let wheel = self.workspace_wheel.get_or_insert_with(|| WorkspaceWheel {
            output: output.into(),
            delta: 0.0,
            at: now,
            active,
            target: active,
        });
        if wheel.output != output
            || now.duration_since(wheel.at) > Duration::from_millis(240)
            || !spaces
                .iter()
                .any(|w| w["id"].as_u64() == Some(wheel.target))
        {
            wheel.output = output.into();
            wheel.target = active;
            wheel.delta = 0.0;
        }
        // Track an optimistic target while compositor activation events catch up.
        if wheel.active != active {
            if wheel.target == wheel.active {
                wheel.target = active;
            }
            wheel.active = active;
        }
        if wheel.delta * delta < 0.0 {
            wheel.delta = 0.0;
        }
        wheel.at = now;
        wheel.delta += delta;
        let steps = (wheel.delta / 15.0).trunc() as i64;
        if steps != 0 {
            wheel.delta -= steps as f64 * 15.0;
            let current = spaces
                .iter()
                .position(|w| w["id"].as_u64() == Some(wheel.target))
                .unwrap_or(0);
            let next = (current as i64)
                .saturating_add(steps)
                .clamp(0, spaces.len() as i64 - 1) as usize;
            let target = spaces[next]["id"].as_u64().unwrap();
            if target != wheel.target {
                wheel.target = target;
                if !self.readonly {
                    bar::focus_workspace(target, true);
                }
            }
        }
        true
    }
    /// Only the pinned, top-level tabs consume wheel input. Segmented controls
    /// inside the scrollable body must leave it for ordinary vertical scrolling.
    pub fn scroll_tabs(&mut self, window: u32, x: f64, y: f64, delta: f64) -> bool {
        if !delta.is_finite() {
            return false;
        }
        let Some(hits) = self.hits.get(&window) else {
            return false;
        };
        let Some(group) = hits
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y) && tab_group(&h.action).is_some())
            .and_then(|h| tab_group(&h.action))
        else {
            return false;
        };
        let mut choices: Vec<_> = hits
            .iter()
            .filter(|h| tab_group(&h.action) == Some(group))
            .collect();
        choices.sort_by(|a, b| a.rect.x.total_cmp(&b.rect.x));
        let selected = choices
            .iter()
            .position(|h| match &h.action {
                Action::Tab(tab) => tab == &self.tab,
                Action::Power(power::Request::Profile(p)) => {
                    self.common.profile.as_ref() == Some(p)
                }
                Action::DisplayTab(night) => self.display_night == *night,
                _ => false,
            })
            .unwrap_or(0);
        let now = Instant::now();
        if self.tab_wheel_group != Some(group)
            || now.duration_since(self.tab_wheel_time) > Duration::from_millis(240)
            || self.tab_wheel_delta * delta < 0.0
        {
            self.tab_wheel_delta = 0.0;
        }
        self.tab_wheel_group = Some(group);
        self.tab_wheel_time = now;
        self.tab_wheel_delta += delta;
        let steps = (self.tab_wheel_delta / 15.0).trunc() as i32;
        if steps != 0 {
            self.tab_wheel_delta -= f64::from(steps) * 15.0;
            let next = (selected as i64 + i64::from(steps))
                .clamp(0, choices.len().saturating_sub(1) as i64) as usize;
            if next != selected {
                let action = choices[next].action.clone();
                self.activate(action);
            }
        }
        true
    }
    pub fn scroll_display_time(&mut self, window: u32, x: f64, y: f64, delta: f64) -> bool {
        if self.page != Page::Display || !self.display_night || !delta.is_finite() || delta == 0.0 {
            return false;
        }
        let target = self.hits.get(&window).and_then(|hits| {
            hits.iter().rev().find_map(|h| {
                if let Action::DisplayTime(start, part) = h.action {
                    h.rect.contains(x, y).then_some((start, part))
                } else {
                    None
                }
            })
        });
        let Some((start, part)) = target else {
            self.display_clock_wheel = None;
            return false;
        };
        let now = Instant::now();
        let wheel = self.display_clock_wheel.get_or_insert(ClockWheel {
            start,
            part,
            delta: 0.0,
            at: now,
        });
        if wheel.start != start
            || wheel.part != part
            || now.duration_since(wheel.at) > Duration::from_millis(240)
            || wheel.delta * delta < 0.0
        {
            wheel.delta = 0.0;
        }
        wheel.start = start;
        wheel.part = part;
        wheel.at = now;
        wheel.delta += delta;
        let steps = (wheel.delta / 15.0).trunc().clamp(-1440.0, 1440.0) as i32;
        if steps != 0 {
            wheel.delta -= f64::from(steps) * 15.0;
            self.display_send(display::Request::Time(start, part, -steps));
        }
        true
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
            Action::DisplayTab(night) => {
                self.display_night = night;
                self.display_menu = None;
                self.display_clock_wheel = None;
                self.scroll = 0.0;
            }
            Action::DisplayIdentify => self.display_identify = true,
            Action::DisplayMonitor | Action::DisplayTime(_, _) => {}
            Action::DisplayDrag(_, _) => {}
            Action::DisplayResetLayout => {
                self.display_layout.clear();
                self.display_drag = None;
            }
            Action::DisplayApplyLayout => {
                if self.display_layout_changed() {
                    self.display_send(display::Request::Place(self.display_layout.clone()));
                }
            }
            Action::Display(request) => {
                self.display_menu = None;
                self.display_send(request);
            }
            Action::DisplayMenu(menu) => {
                self.display_menu = if self.display_menu == menu {
                    None
                } else {
                    menu
                };
            }
            Action::Page(page) => self.navigate(page),
            Action::Wifi => {
                self.start(net::Action::Radio(!self.wifi_enabled()));
            }
            Action::Bluetooth => self.bt_send(bt::Request::Power(!self.bluetooth.powered)),
            Action::Bt(request) => {
                if let bt::Request::Remove(path) = &request {
                    if self.bt_deleting.as_ref() != Some(path) {
                        return;
                    }
                    self.bt_deleting = None;
                }
                self.bt_send(request);
            }
            Action::BtForget(path) => {
                self.bt_deleting = path;
                self.ensure_entry_visible = true;
            }
            Action::Profile => {
                if let Some(p) = next_profile(self.common.profile.as_deref(), &self.common.profiles)
                {
                    self.common.profile = Some(p.clone());
                    self.profile_pending = self.common_tx.is_some() || self.power_tx.is_some();
                    if self.power_tx.is_some() {
                        self.power_send(power::Request::Profile(p));
                    } else {
                        self.write(control::Action::Profile(p));
                    }
                }
            }
            Action::Power(request) => {
                if matches!(request, power::Request::Timeout(..)) {
                    self.power_picker = None;
                }
                self.power_send(request);
            }
            Action::PowerExtra => {
                self.power_extra_progress();
                self.power_extra = !self.power_extra;
                self.power_picker = None;
            }
            Action::PowerSource(ac) => {
                self.power_ac = ac;
                self.power_picker = None;
            }
            Action::PowerPicker(timer) => {
                self.power_picker = if self.power_picker == Some(timer) {
                    None
                } else {
                    Some(timer)
                };
                if self.power_picker.is_some() {
                    if let Some(hits) = self.hits.get(&1) {
                        let row = hits
                            .iter()
                            .find(|h| matches!(h.action, Action::PowerPicker(t) if t == timer));
                        let bottom = hits.iter().find_map(|h| {
                            if let Action::Scrollbar(b) = h.action {
                                Some(b.track.y + b.track.h)
                            } else {
                                None
                            }
                        });
                        if let (Some(row), Some(bottom)) = (row, bottom) {
                            let menu_height = super::draw::power_ui::menu_height(timer);
                            self.scroll +=
                                (row.rect.y + row.rect.h + menu_height + 12.0 - bottom).max(0.0);
                        }
                    }
                }
            }
            Action::PowerClosePicker => self.power_picker = None,
            Action::PowerMenu => {}
            Action::PowerAwake if self.power_idle_supported && !self.readonly => {
                self.power_awake = !self.power_awake
            }
            Action::PowerAwake => {}
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
                let entering_wifi = tab == "wifi" && self.tab != tab;
                self.tab = tab;
                self.scroll = 0.0;
                self.entry = self.tab == "wifi" && (self.wifi.is_some() || self.auth_prompt);
                self.wifi_scan_pending = entering_wifi && !self.readonly && !self.busy;
                self.scan_wifi_on_entry();
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
            Action::Submit => {
                if self.page == Page::Bluetooth {
                    self.bt_submit();
                } else {
                    self.submit();
                }
            }
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
            Action::Workspace(id) => bar::focus_workspace(id, false),
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
            Action::NoticeHover | Action::MonitorThread(_) => {}
            Action::Scrollbar(_) => {}
            Action::DeleteNotices(ids) => {
                self.prepare_notice_exit(&ids);
                self.removed_notices.extend(ids.iter().copied());
                self.notifications.notices.retain(|n| !ids.contains(&n.id));
                if !self.readonly {
                    self.notification_request = Some(super::notifications::Command::Delete(ids));
                }
            }
            Action::ClearNotices => {
                let ids: Vec<_> = self.notifications.notices.iter().map(|n| n.id).collect();
                self.notice_exit = Some(Rect {
                    x: super::notifications::LIST_X,
                    y: 62.0,
                    w: f64::from(super::notifications::WIDTH) - super::notifications::LIST_X - 20.0,
                    h: self.viewport_height,
                });
                self.notice_revision = self.notice_revision.wrapping_add(1);
                self.removed_notices.extend(ids.iter().copied());
                if !self.readonly {
                    self.notification_request = Some(super::notifications::Command::Delete(ids));
                }
                self.notifications.notices.clear();
                self.scroll = 0.0;
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
                    "lock" => bar::lucy(&["lock", "--daemonize"]),
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
        if !value.is_finite() {
            return;
        }
        if matches!(target, Slider::DisplayBrightness(_) | Slider::Temperature) {
            self.display_slider(target, value, force);
            return;
        }
        if let Slider::Media(player, track) = target {
            if !value.is_finite() {
                return;
            }
            let Some(media) = self
                .common
                .media
                .as_mut()
                .filter(|m| &m.player == player && &m.track == track && m.seekable())
            else {
                return;
            };
            let position =
                (media.duration.unwrap() as f64 * value.clamp(0.0, 100.0) / 100.0).round() as i64;
            let position = position.min(media.duration.unwrap().saturating_sub(1));
            let now = Instant::now();
            media.position = Some(position);
            media.sampled = Some(now);
            self.media_seek_draft = Some((player.clone(), track.clone(), position, now));
            if force && self.dragging.is_none() && !self.readonly {
                self.write(control::Action::MediaSeek {
                    player: player.clone(),
                    track: track.clone(),
                    position,
                });
            }
            return;
        }
        let percent = value.clamp(
            if matches!(target, Slider::Brightness | Slider::DisplayBrightness(_)) {
                1.0
            } else {
                0.0
            },
            100.0,
        );
        let v = percent.round() as u32;
        self.animation_until.set(
            self.animation_until
                .get()
                .max(Instant::now() + Duration::from_millis(100)),
        );
        self.quiet = Instant::now() + Duration::from_millis(400);
        match target {
            Slider::Volume => {
                self.common.audio = Some((percent, self.common.audio.is_some_and(|a| a.1)))
            }
            Slider::Brightness => self.common.brightness = Some(percent),
            Slider::DisplayBrightness(_) | Slider::Temperature => unreachable!(),
            Slider::Media(_, _) => unreachable!(),
            Slider::Audio(kind, id) => {
                let collection = collection(kind);
                if let Some(items) = self.audio[collection].as_array_mut() {
                    for item in items {
                        if item["index"].as_u64() == Some(*id) {
                            if let Some(channels) = item["volume"].as_object_mut() {
                                for channel in channels.values_mut() {
                                    channel["value"] =
                                        ((percent * 65536.0 / 100.0).round() as u64).into();
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
            Slider::DisplayBrightness(_) | Slider::Temperature => unreachable!(),
            Slider::Audio(kind, id) => self.audio_write(audio::volume_command(kind, *id, v)),
            Slider::Media(_, _) => unreachable!(),
        }
    }
    pub fn media_position(&self) -> Option<i64> {
        let media = self.common.media.as_ref()?;
        if matches!(&self.dragging,Some((_,Slider::Media(player,track),_))if player==&media.player&&track==&media.track)
        {
            media.position
        } else {
            media.position_at(Instant::now())
        }
    }
    pub fn media_pending(&self) -> bool {
        self.media_seek_draft.is_some()
    }
    pub fn media_progress(&self) -> f64 {
        self.common
            .media
            .as_ref()
            .and_then(|m| m.duration)
            .filter(|d| *d > 0)
            .map_or(0.0, |duration| {
                self.media_position().unwrap_or(0) as f64 / duration as f64 * 100.0
            })
    }
    pub fn notification_state(&mut self, mut state: super::notifications::State) {
        // Optimistic deletion must not be undone by an already queued snapshot.
        self.removed_notices
            .retain(|id| state.notices.iter().any(|n| n.id == *id));
        state
            .notices
            .retain(|n| !self.removed_notices.contains(&n.id));
        self.notifications = state;
    }
    fn prepare_notice_exit(&mut self, ids: &[u32]) {
        let hits = self.hits.get(&1);
        let close = hits.and_then(|hits| hits.iter().find(|h| matches!(&h.action, Action::DeleteNotices(found) if found.iter().any(|id| ids.contains(id)))));
        let card = close.and_then(|close| {
            hits?.iter().find(|h| {
                matches!(h.action, Action::NoticeHover)
                    && h.rect.contains(close.rect.x + 14.0, close.rect.y + 14.0)
            })
        });
        let mut rect = card.map(|h| h.rect).unwrap_or(Rect {
            x: super::notifications::LIST_X,
            y: 62.0,
            w: f64::from(super::notifications::WIDTH) - super::notifications::LIST_X - 20.0,
            h: self.viewport_height,
        });
        if let Some(notice) = self
            .notifications
            .notices
            .iter()
            .find(|n| ids.contains(&n.id))
        {
            if self
                .notifications
                .notices
                .iter()
                .filter(|n| n.app.eq_ignore_ascii_case(&notice.app))
                .all(|n| ids.contains(&n.id))
            {
                rect.y -= 48.0;
                rect.h += 48.0 + 28.0;
            } else {
                rect.h += 10.0;
            }
        }
        rect.y = rect.y.max(62.0);
        self.notice_exit = Some(rect);
        self.notice_revision = self.notice_revision.wrapping_add(1);
    }
    pub fn audio_state(&mut self, mut next: Value) {
        if Instant::now() < self.quiet || self.dragging.is_some() {
            for collection in ["sinks", "sources", "sink-inputs", "source-outputs"] {
                if let (Some(old), Some(new)) = (
                    self.audio[collection].as_array(),
                    next[collection].as_array_mut(),
                ) {
                    for item in new {
                        let dragging = self.dragging.as_ref().is_some_and(|(_, target, _)| {
                            matches!(target, Slider::Audio(kind, id) if self::collection(kind) == collection && item["index"].as_u64() == Some(*id))
                        });
                        if Instant::now() < self.quiet || dragging {
                            if let Some(prev) = old.iter().find(|p| p["index"] == item["index"]) {
                                item["volume"] = prev["volume"].clone();
                            }
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
                let percent =
                    if Instant::now() < self.quiet || self.dragging_slider(&Slider::Volume) {
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
        if self.page == Page::Bluetooth {
            if self.entry {
                self.bt_input.insert(text);
            }
            return;
        }
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
        if self.page == Page::Bluetooth {
            self.bt_input.backspace();
            return;
        }
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
        if self.page == Page::Bluetooth {
            self.bt_input.move_cursor(direction);
            return;
        }
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
        if self.page == Page::Bluetooth {
            self.bt_input.delete();
            return;
        }
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
        if domain == control::Domain::Media && !success && self.media_seek_draft.take().is_some() {
            if let Some(tx) = &self.common_tx {
                let _ = tx.send(control::Request::MediaPosition);
            }
        }
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
        if self.readonly || self.busy {
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
                self.scan_wifi_on_entry();
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
                let scan = self.scanning;
                self.radio_pending = None;
                self.busy = false;
                self.scanning = false;
                self.auth_prompt = false;
                self.replies = None;
                // A background refresh must not discard an unfinished password
                // when the user reopens the Wi-Fi page.
                if !scan {
                    self.wipe_secret();
                    if let Some(mut p) = self.prefill.take() {
                        p.fill(0);
                    }
                    self.wifi = None;
                }
                self.status = result.err().unwrap_or_default();
            }
        }
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;
    #[test]
    fn detailed_sampling_is_enabled_only_for_the_monitor_and_stops_on_hide() {
        let (tx, rx) = mpsc::channel();
        let mut m = Model::new();
        m.monitor_tx = Some(tx);
        m.navigate(Page::System);
        assert!(matches!(
            rx.recv().unwrap(),
            super::super::monitor::Request::Visible(true)
        ));
        m.hide();
        assert!(matches!(
            rx.recv().unwrap(),
            super::super::monitor::Request::Visible(false)
        ));
        m.navigate(Page::System);
        assert!(matches!(
            rx.recv().unwrap(),
            super::super::monitor::Request::Visible(true)
        ));
        m.navigate(Page::Home);
        assert!(matches!(
            rx.recv().unwrap(),
            super::super::monitor::Request::Visible(false)
        ));
        assert_eq!(Page::parse("monitor"), Page::System);
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
    fn session_numbers_and_keypad_match_visible_button_order() {
        use super::*;
        for (index, (_, command, _)) in SESSION_ACTIONS.iter().enumerate() {
            assert_eq!(session_shortcut(0x31 + index as u32), Some(*command));
            assert_eq!(session_shortcut(0xffb1 + index as u32), Some(*command));
        }
        for sym in [0x30, 0x36, 0xff1b, 0xff0d] {
            assert_eq!(session_shortcut(sym), None);
        }
    }
    #[test]
    fn delayed_media_reports_cannot_undo_drag_or_acknowledge_the_wrong_position() {
        use super::*;
        let mut m = Model::new();
        m.common.media = Some(control::Media {
            player: "test".into(),
            track: "/track/one".into(),
            duration: Some(180_000_000),
            position: Some(10_000_000),
            can_seek: true,
            sampled: Some(Instant::now()),
            ..Default::default()
        });
        let cached = m.common.clone();
        let slider = Slider::Media("test".into(), "/track/one".into());
        m.dragging = Some((
            1,
            slider.clone(),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 20.0,
            },
        ));
        m.slider(&slider, 75.0, true);
        let mut late = cached.clone();
        late.media.as_mut().unwrap().sampled = Some(Instant::now());
        m.common_state(late.clone());
        assert_eq!(m.media_position(), Some(135_000_000));
        assert!(m.media_pending());
        m.dragging = None;
        m.slider(&slider, 80.0, true);
        // Even a newly sampled GetAll can still report the position before the
        // application's asynchronous seek finishes.
        for _ in 0..5 {
            late.media.as_mut().unwrap().sampled = Some(Instant::now());
            m.common_state(late.clone());
            assert_eq!(m.media_position(), Some(144_000_000));
            assert!(m.media_pending());
        }
        m.common_state(cached.clone());
        assert!(m.media_pending());
        let mut confirmed = late;
        confirmed.media.as_mut().unwrap().position = Some(144_000_000);
        confirmed.media.as_mut().unwrap().sampled = Some(Instant::now());
        m.common_state(confirmed);
        assert!(!m.media_pending());
        m.common_state(cached);
        assert_eq!(m.media_position(), Some(144_000_000));
    }
    #[test]
    fn media_drag_seeks_once_on_release_and_rejects_stale_tracks() {
        use super::*;
        let mut m = Model::new();
        let (tx, rx) = std::sync::mpsc::channel();
        m.common_tx = Some(tx);
        m.common.media = Some(control::Media {
            player: "test".into(),
            track: "/track/one".into(),
            duration: Some(180_000_000),
            position: Some(0),
            can_seek: true,
            ..Default::default()
        });
        let slider = Slider::Media("test".into(), "/track/one".into());
        m.dragging = Some((
            1,
            slider.clone(),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 20.0,
            },
        ));
        m.slider(&slider, 25.125, true);
        assert!(rx.try_recv().is_err());
        assert_eq!(m.media_position(), Some(45_225_000));
        m.dragging = None;
        m.slider(&slider, 50.5, true);
        assert!(matches!(
            rx.try_recv().unwrap(),
            control::Request::Write(control::Action::MediaSeek {
                position: 90_900_000,
                ..
            })
        ));
        m.common.media.as_mut().unwrap().track = "/track/two".into();
        m.slider(&slider, 75.0, true);
        assert!(rx.try_recv().is_err());
        assert_eq!(m.common.media.as_ref().unwrap().position, Some(90_900_000));
    }
    #[test]
    fn display_countdown_does_not_close_an_open_selector() {
        use super::*;
        let mut m = Model::new();
        m.display_menu = Some(display::Menu::Scale("eDP-1".into()));
        let mut state = m.display.clone();
        state.pending = Some(display::Pending {
            before: vec![],
            after: vec![],
            deadline: Instant::now() + Duration::from_secs(15),
        });
        m.display_state(state);
        assert_eq!(m.display_menu, Some(display::Menu::Scale("eDP-1".into())));
    }
    #[test]
    fn all_audio_and_brightness_sliders_keep_fractional_drag_samples_and_reject_stale_snapshots() {
        for target in [
            Slider::Volume,
            Slider::Brightness,
            Slider::Audio("sink".into(), 7),
            Slider::Audio("source".into(), 7),
            Slider::Audio("sink-input".into(), 7),
            Slider::Audio("source-output".into(), 7),
        ] {
            let mut m = Model::new();
            m.readonly = true;
            m.common.audio = Some((20.0, false));
            m.common.brightness = Some(20.0);
            let device = serde_json::json!({"index":7,"name":"fixture","volume":{"front-left":{"value":13107},"front-right":{"value":13107}}});
            m.audio = serde_json::json!({"info":{"default_sink_name":"fixture"},"sinks":[device.clone()],"sources":[device.clone()],"sink-inputs":[device.clone()],"source-outputs":[device]});
            let stale_audio = m.audio.clone();
            let (common_tx, common_rx) = mpsc::channel();
            let (audio_tx, audio_rx) = mpsc::channel();
            m.common_tx = Some(common_tx);
            m.audio_tx = Some(audio_tx);
            m.dragging = Some((
                1,
                target.clone(),
                Rect {
                    x: 20.0,
                    y: 100.0,
                    w: 400.0,
                    h: 38.0,
                },
            ));
            for percent in [41.125, 41.375, 42.875] {
                m.slider(&target, percent, true);
                let current = match &target {
                    Slider::Volume => m.common.audio.unwrap().0,
                    Slider::Brightness => m.common.brightness.unwrap(),
                    Slider::Audio(kind, _) => audio::volume(&m.audio[collection(kind)][0]),
                    _ => unreachable!(),
                };
                assert!((m.slider_value(&target, current) - percent).abs() < 0.001);
                m.quiet = Instant::now() - Duration::from_secs(10);
                m.common_state(control::State {
                    audio: Some((20.0, true)),
                    brightness: Some(20.0),
                    ..Default::default()
                });
                m.audio_state(stale_audio.clone());
                let current = match &target {
                    Slider::Volume => m.common.audio.unwrap().0,
                    Slider::Brightness => m.common.brightness.unwrap(),
                    Slider::Audio(kind, _) => audio::volume(&m.audio[collection(kind)][0]),
                    _ => unreachable!(),
                };
                assert!(
                    (current - percent).abs() < 0.001,
                    "stale snapshot overwrote {target:?}"
                );
            }
            m.dragging = None;
            m.common_state(control::State {
                audio: Some((20.0, false)),
                brightness: Some(20.0),
                ..Default::default()
            });
            m.audio_state(stale_audio);
            assert!(common_rx.try_recv().is_err());
            assert!(audio_rx.try_recv().is_err());
        }
    }
    #[test]
    fn all_control_gauge_motion_is_continuous_bounded_and_finishes_without_writes() {
        for target in [
            Slider::Volume,
            Slider::Brightness,
            Slider::Audio("sink".into(), 1),
            Slider::Audio("source".into(), 2),
            Slider::Audio("sink-input".into(), 3),
            Slider::Audio("source-output".into(), 4),
        ] {
            let mut m = Model::new();
            let key = format!("gauge:{target:?}");
            assert_eq!(m.slider_value(&target, 20.0), 20.0);
            assert_eq!(m.slider_value(&target, 60.0), 20.0);
            m.icon_values.borrow_mut().get_mut(&key).unwrap().2 =
                Instant::now() - Duration::from_millis(50);
            let mid = m.slider_value(&target, 60.0);
            assert!(mid > 40.0 && mid < 60.0);
            assert!((m.slider_value(&target, 30.0) - mid).abs() < 0.1);
            m.dragging = Some((
                1,
                target.clone(),
                Rect {
                    x: 20.0,
                    y: 100.0,
                    w: 400.0,
                    h: 38.0,
                },
            ));
            assert_eq!(m.slider_value(&target, 41.125), 41.125);
            m.dragging = None;
            assert_eq!(m.slider_value(&target, 41.125), 41.125);
            m.animation_until
                .set(Instant::now() - Duration::from_secs(1));
            for _ in 0..100 {
                assert_eq!(m.slider_value(&target, 41.125), 41.125);
                assert!(
                    !m.animating(),
                    "static {target:?} must not keep requesting frames"
                );
            }
        }
        let m = Model::new();
        for key in [
            "battery-charge",
            "battery-health",
            "osd-volume",
            "osd-microphone",
            "osd-brightness",
            "osd-keyboard",
        ] {
            m.reset_readout(key, 20.0);
            assert_eq!(m.readout_value(key, 20.0), 20.0);
            assert_eq!(m.readout_value(key, 80.0), 20.0);
            m.icon_values
                .borrow_mut()
                .get_mut(&format!("gauge:{key}"))
                .unwrap()
                .2 = Instant::now() - Duration::from_millis(200);
            assert_eq!(m.readout_value(key, 80.0), 80.0);
        }
    }
    fn display_slider_model() -> (Model, mpsc::Receiver<display::Request>) {
        let mut m = Model::new();
        m.page = Page::Display;
        m.quiet = Instant::now() - Duration::from_secs(1);
        let mode = serde_json::json!({"width":1920,"height":1080,"refresh_rate":60000});
        m.display.outputs = display::parse_outputs(&serde_json::json!({
            "eDP-1":{"make":"Test","model":"Panel","modes":[mode],"current_mode":0,"logical":{}},
            "DP-1":{"make":"Test","model":"DDC","modes":[mode],"current_mode":0,"logical":{}},
            "DP-2":{"make":"Test","model":"Unsupported","modes":[mode],"current_mode":0,"logical":{}}
        }));
        m.display.outputs[0].control = Some(display::Brightness::Backlight("test".into()));
        m.display.outputs[1].control = Some(display::Brightness::Ddc(1, 100));
        m.display.outputs[0].brightness = Some(20.0);
        m.display.outputs[1].brightness = Some(30.0);
        let (tx, rx) = mpsc::channel();
        m.display_tx = Some(tx);
        (m, rx)
    }
    #[test]
    fn display_slider_drag_follows_fractional_input_and_flushes_the_latest_value() {
        let (mut m, rx) = display_slider_model();
        let target = Slider::DisplayBrightness("eDP-1".into());
        let rect = Rect {
            x: 32.0,
            y: 140.0,
            w: 408.0,
            h: 36.0,
        };
        let mut old = m.display.clone();
        m.dragging = Some((1, target.clone(), rect));
        m.slider(&target, 41.125, true);
        assert_eq!(
            rx.try_recv().unwrap(),
            display::Request::Brightness("eDP-1".into(), 41, 1)
        );
        m.display_last_write = Instant::now();
        m.slider(&target, 41.375, false);
        assert_eq!(m.display_gauge_value(&target, 0.0), 41.375);
        assert!(rx.try_recv().is_err());
        old.slider_serial = 1;
        old.outputs[0].brightness = Some(41.0);
        m.display_state(old.clone());
        assert_eq!(m.display.outputs[0].brightness, Some(41.375));
        m.slider(&target, 42.875, false);
        assert_eq!(m.display.outputs[0].brightness, Some(42.875));
        assert!(rx.try_recv().is_err());
        m.display_state(old.clone());
        assert_eq!(m.display.outputs[0].brightness, Some(42.875));
        m.dragging = None;
        m.common_state(control::State {
            brightness: Some(41.0),
            ..Default::default()
        });
        assert_eq!(m.display.outputs[0].brightness, Some(42.875));
        m.slider(&target, 42.875, true);
        assert_eq!(
            rx.try_recv().unwrap(),
            display::Request::Brightness("eDP-1".into(), 43, 2)
        );
        // An old completion arriving after release must not need a time window.
        m.quiet = Instant::now() - Duration::from_secs(10);
        m.display_state(old.clone());
        assert_eq!(m.display.outputs[0].brightness, Some(42.875));
        old.slider_serial = 2;
        old.outputs[0].brightness = Some(43.0);
        m.display_state(old);
        assert_eq!(m.display.outputs[0].brightness, Some(43.0));
        assert!(m.display_slider_drafts.is_empty());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn display_slider_snapshots_acknowledge_each_latest_request_including_failures() {
        let (mut m, rx) = display_slider_model();
        let target = Slider::DisplayBrightness("DP-1".into());
        let mut state = m.display.clone();
        m.slider(&target, 50.0, true);
        m.slider(&target, 75.0, true);
        m.slider(&Slider::Temperature, 25.0, true);
        assert_eq!(rx.try_iter().count(), 3);
        state.slider_serial = 1;
        state.outputs[1].brightness = Some(50.0);
        state.outputs[0].brightness = Some(24.0);
        state.error = "이전 요청 실패".into();
        m.display_state(state.clone());
        assert_eq!(m.display.outputs[1].brightness, Some(75.0));
        assert_eq!(m.display.outputs[0].brightness, Some(24.0));
        assert_eq!(m.display.settings.night.kelvin, 3500);
        state.slider_serial = 2;
        state.outputs[1].brightness = Some(75.0);
        state.error.clear();
        m.display_state(state.clone());
        assert_eq!(m.display_slider_drafts.len(), 1);
        assert_eq!(m.display.settings.night.kelvin, 3500);
        state.slider_serial = 3;
        state.settings.night.kelvin = 3500;
        m.display_state(state.clone());
        assert!(m.display_slider_drafts.is_empty());
        m.slider(&target, 90.0, true);
        assert_eq!(
            rx.try_recv().unwrap(),
            display::Request::Brightness("DP-1".into(), 90, 4)
        );
        state.slider_serial = 4;
        state.error = "DDC 쓰기 실패".into();
        m.display_state(state);
        assert_eq!(m.display.outputs[1].brightness, Some(75.0));
        assert_eq!(m.display.error, "DDC 쓰기 실패");
        assert!(m.display_slider_drafts.is_empty());
    }
    #[test]
    fn display_slider_drafts_do_not_follow_replaced_disconnected_or_unsupported_monitors() {
        for change in [0, 1, 2] {
            let (mut m, _rx) = display_slider_model();
            let mut state = m.display.clone();
            let target = Slider::DisplayBrightness("DP-1".into());
            m.slider(&target, 80.0, true);
            match change {
                0 => state.outputs[1].identity = "replacement".into(),
                1 => {
                    state.outputs.remove(1);
                }
                _ => state.outputs[1].control = None,
            }
            let authoritative = state.outputs.clone();
            m.display_state(state);
            assert!(m.display_slider_drafts.is_empty());
            assert_eq!(m.display.outputs, authoritative);
        }
    }
    #[test]
    fn display_slider_motion_retargets_continuously_settles_and_never_writes() {
        let (mut m, rx) = display_slider_model();
        let target = Slider::Temperature;
        assert_eq!(m.display_gauge_value(&target, 50.0), 50.0);
        m.slider(&target, 52.0, true);
        assert_eq!(
            rx.try_recv().unwrap(),
            display::Request::Temperature(4600, 1)
        );
        assert_eq!(m.display_gauge_value(&target, 52.0), 50.0);
        m.icon_values
            .borrow_mut()
            .get_mut("display-temperature")
            .unwrap()
            .2 = Instant::now() - Duration::from_millis(50);
        let mid = m.display_gauge_value(&target, 52.0);
        assert!(mid > 51.0 && mid < 52.0);
        m.slider(&target, 30.0, true);
        assert_eq!(
            rx.try_recv().unwrap(),
            display::Request::Temperature(3700, 2)
        );
        assert!((m.display_gauge_value(&target, 30.0) - mid).abs() < 0.05);
        m.icon_values
            .borrow_mut()
            .get_mut("display-temperature")
            .unwrap()
            .2 = Instant::now() - Duration::from_millis(200);
        m.animation_until
            .set(Instant::now() - Duration::from_secs(1));
        for _ in 0..30 {
            assert_eq!(m.display_gauge_value(&target, 30.0), 30.0);
            assert!(!m.animating());
        }
        assert!(rx.try_recv().is_err());
        // Even inside one 50 K hardware step the pointer moves continuously.
        m.dragging = Some((
            1,
            target.clone(),
            Rect {
                x: 32.0,
                y: 140.0,
                w: 408.0,
                h: 36.0,
            },
        ));
        for value in [50.1, 50.2, 50.3] {
            m.slider(&target, value, false);
            assert_eq!(m.display_gauge_value(&target, 0.0), value);
        }
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn display_slider_invalid_input_and_unsupported_controls_never_write() {
        let (mut m, rx) = display_slider_model();
        for target in [
            Slider::Temperature,
            Slider::DisplayBrightness("eDP-1".into()),
        ] {
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                m.slider(&target, value, true);
            }
        }
        m.slider(&Slider::DisplayBrightness("DP-2".into()), 50.0, true);
        assert_eq!(m.display.outputs[2].brightness, None);
        assert!(m.display_slider_drafts.is_empty());
        assert!(rx.try_recv().is_err());
        m.readonly = true;
        m.slider(&Slider::Temperature, 30.0, true);
        m.slider(&Slider::DisplayBrightness("eDP-1".into()), 70.0, true);
        assert_eq!(m.display.settings.night.kelvin, 3700);
        assert_eq!(m.display.outputs[0].brightness, Some(70.0));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn schedule_wheel_consumes_only_clock_fields_and_keeps_draft_through_old_snapshots() {
        use super::*;
        let mut m = Model::new();
        m.readonly = true;
        m.page = Page::Display;
        m.display_night = true;
        m.display.settings.night.start = 21 * 60 + 59;
        m.display.settings.night.end = 7 * 60;
        m.hits.insert(
            1,
            vec![
                Hit {
                    rect: Rect {
                        x: 200.0,
                        y: 200.0,
                        w: 60.0,
                        h: 34.0,
                    },
                    action: Action::DisplayTime(true, display::ClockPart::Hour),
                },
                Hit {
                    rect: Rect {
                        x: 280.0,
                        y: 200.0,
                        w: 60.0,
                        h: 34.0,
                    },
                    action: Action::DisplayTime(true, display::ClockPart::Minute),
                },
            ],
        );
        assert!(!m.scroll_display_time(1, 100.0, 200.0, -15.0));
        assert!(m.scroll_display_time(1, 220.0, 210.0, -5.0));
        assert_eq!(m.display.settings.night.start, 21 * 60 + 59);
        assert!(m.scroll_display_time(1, 220.0, 210.0, -10.0));
        assert_eq!(m.display.settings.night.start, 22 * 60 + 59);
        m.scroll_display_time(1, 300.0, 210.0, -15.0);
        assert_eq!(m.display.settings.night.start, 22 * 60);
        assert_eq!(m.display.settings.night.end, 7 * 60);
        let old = display::State::default();
        m.display_state(old);
        assert_eq!(m.display.settings.night.start, 22 * 60);
        let mut confirmed = display::State::default();
        confirmed.settings.night.start = 22 * 60;
        m.display_state(confirmed);
        assert!(m.display_time_draft.is_none());
        assert!(!m.scroll_display_time(1, 300.0, 210.0, f64::NAN));
    }
    #[test]
    fn workspace_wheel_uses_output_local_order_and_accumulates_fine_input() {
        use super::*;
        let mut m = Model::new();
        m.readonly = true; // Never focus the real compositor in tests.
        m.workspaces = vec![
            serde_json::json!({"id":73,"idx":3,"output":"eDP-1","is_active":false}),
            serde_json::json!({"id":8,"idx":1,"output":"DP-1","is_active":true}),
            serde_json::json!({"id":22,"idx":2,"output":"eDP-1","is_active":false}),
            serde_json::json!({"id":11,"idx":1,"output":"eDP-1","is_active":true}),
        ];
        m.hits.insert(
            4,
            [11, 22, 73]
                .into_iter()
                .enumerate()
                .map(|(i, id)| Hit {
                    rect: Rect {
                        x: 44.0 + i as f64 * 32.0,
                        y: 3.0,
                        w: 28.0,
                        h: 28.0,
                    },
                    action: Action::Workspace(id),
                })
                .collect(),
        );
        assert!(!m.scroll_workspaces(4, "eDP-1", 23.0, 17.0, 15.0));
        assert!(!m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, f64::NAN));
        // The small gaps between dots belong to the indicator too.
        assert!(m.scroll_workspaces(4, "eDP-1", 74.0, 17.0, 5.0));
        assert_eq!(m.workspace_scroll_target(), Some(11));
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, 10.0);
        assert_eq!(m.workspace_scroll_target(), Some(22));
        // Advance again even before Niri publishes the first activation event.
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, 15.0);
        assert_eq!(m.workspace_scroll_target(), Some(73));
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, 150.0);
        assert_eq!(m.workspace_scroll_target(), Some(73));
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, -15.0);
        assert_eq!(m.workspace_scroll_target(), Some(22));
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, -150.0);
        assert_eq!(m.workspace_scroll_target(), Some(11));
        m.scroll_workspaces(4, "DP-1", 58.0, 17.0, 15.0);
        assert_eq!(m.workspace_scroll_target(), Some(8));
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, 10.0);
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, -5.0);
        assert_eq!(m.workspace_wheel.as_ref().unwrap().delta, -5.0);
        m.workspace_wheel.as_mut().unwrap().at = Instant::now() - Duration::from_secs(1);
        m.scroll_workspaces(4, "eDP-1", 58.0, 17.0, 10.0);
        assert_eq!(m.workspace_wheel.as_ref().unwrap().delta, 10.0);
    }
    #[test]
    fn wifi_entry_waits_for_state_and_scans_once_without_interrupting_operations() {
        use super::*;
        let mut m = Model::new();
        m.navigate(Page::Network);
        assert!(m.wifi_scan_pending); // Initial state is not yet known.
        assert!(!m.scan_wifi_on_entry());
        m.common.wifi = Some(true);
        assert!(m.scan_wifi_on_entry());
        assert!(!m.scan_wifi_on_entry());
        m.common.wifi = Some(false);
        m.wifi_scan_pending = true;
        assert!(!m.scan_wifi_on_entry());
        assert!(!m.wifi_scan_pending);
        m.common.wifi = Some(true);
        m.busy = true;
        m.hide();
        m.navigate(Page::Network);
        assert!(!m.wifi_scan_pending);
        m.busy = false;
        m.hide();
        m.readonly = true;
        m.navigate(Page::Network);
        assert!(!m.wifi_scan_pending);
        m.readonly = false;
        m.common.wifi = None;
        m.activate(Action::Tab("vpn".into()));
        assert!(!m.wifi_scan_pending);
        m.activate(Action::Tab("wifi".into()));
        assert!(m.wifi_scan_pending);
        m.hide();
        m.common_state(control::State {
            wifi: Some(true),
            ..Default::default()
        });
        assert!(!m.wifi_scan_pending); // No delayed scan after dismissal.
    }
    #[test]
    fn wifi_discovery_does_not_discard_an_unfinished_password() {
        use super::*;
        let mut m = Model::new();
        m.wifi = Some(net::Ap {
            active: false,
            bssid: "00:11:22:33:44:55".into(),
            ssid: "Fixture".into(),
            signal: 70,
            security: "WPA2".into(),
            device: "test0".into(),
        });
        m.secret = "unfinished-test-secret".into();
        m.cursor = m.secret.len();
        m.entry = true;
        m.busy = true;
        m.scanning = true;
        m.network_event(net::Event::Finished(Ok(())));
        assert!(!m.busy);
        assert!(!m.scanning);
        assert!(m.wifi.is_some());
        assert!(m.entry);
        assert_eq!(m.secret, "unfinished-test-secret");
    }
    #[test]
    fn tab_wheel_accumulates_fine_input_and_stops_at_the_ends() {
        use super::*;
        let mut m = Model::new();
        m.hits.insert(
            1,
            ["wifi", "ethernet", "vpn"]
                .into_iter()
                .enumerate()
                .map(|(i, t)| Hit {
                    rect: Rect {
                        x: i as f64 * 100.0,
                        y: 0.0,
                        w: 100.0,
                        h: 30.0,
                    },
                    action: Action::Tab(t.into()),
                })
                .collect(),
        );
        assert!(m.scroll_tabs(1, 50.0, 15.0, 5.0));
        assert_eq!(m.tab, "wifi");
        assert!(m.scroll_tabs(1, 50.0, 15.0, 10.0));
        assert_eq!(m.tab, "ethernet");
        assert!(m.scroll_tabs(1, 50.0, 15.0, 15.0));
        assert_eq!(m.tab, "vpn");
        assert!(m.scroll_tabs(1, 50.0, 15.0, 15.0));
        assert_eq!(m.tab, "vpn");
        assert!(m.scroll_tabs(1, 50.0, 15.0, -15.0));
        assert_eq!(m.tab, "ethernet");
        assert!(!m.scroll_tabs(1, 50.0, 100.0, 15.0));
        assert!(!m.scroll_tabs(1, 50.0, 15.0, f64::NAN));
    }
    #[test]
    fn body_segments_never_consume_wheel_or_change_settings() {
        use super::*;
        let mut m = Model::new();
        let (tx, rx) = mpsc::channel();
        m.power_tx = Some(tx);
        m.common.profile = Some("balanced".into());
        m.power.settings.ac_profile = "balanced".into();
        m.hits.insert(
            1,
            power::PROFILES
                .into_iter()
                .enumerate()
                .map(|(i, p)| Hit {
                    rect: Rect {
                        x: i as f64 * 100.0,
                        y: 0.0,
                        w: 100.0,
                        h: 30.0,
                    },
                    action: Action::Power(power::Request::AutomaticProfile(true, p.into())),
                })
                .collect(),
        );
        assert!(!m.scroll_tabs(1, 50.0, 15.0, 15.0));
        assert!(rx.try_recv().is_err());
        assert_eq!(m.common.profile.as_deref(), Some("balanced"));
        m.hits.insert(
            1,
            vec![Hit {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 30.0,
                },
                action: Action::PowerSource(false),
            }],
        );
        assert!(!m.scroll_tabs(1, 50.0, 15.0, -15.0));
        assert!(m.power_ac);
        m.hits.insert(
            1,
            power::PROFILES
                .into_iter()
                .enumerate()
                .map(|(i, p)| Hit {
                    rect: Rect {
                        x: i as f64 * 100.0,
                        y: 0.0,
                        w: 100.0,
                        h: 30.0,
                    },
                    action: Action::Power(power::Request::Profile(p.into())),
                })
                .collect(),
        );
        assert!(m.scroll_tabs(1, 50.0, 15.0, -15.0));
        assert!(matches!(rx.try_recv().unwrap(),power::Request::Profile(p) if p=="power-saver"));
    }
    #[test]
    fn power_disclosure_reverses_from_current_progress_instead_of_jumping() {
        use super::*;
        let mut m = Model::new();
        assert_eq!(m.power_extra_progress(), 0.0);
        m.activate(Action::PowerExtra);
        assert!(m.power_extra_progress() < 0.01);
        m.icon_values
            .borrow_mut()
            .get_mut("power-extra-open")
            .unwrap()
            .2 = Instant::now() - Duration::from_millis(120);
        let mid = m.power_extra_progress();
        assert!(mid > 0.0 && mid < 1.0);
        assert!(m.animating());
        m.activate(Action::PowerExtra);
        assert!((m.power_extra_progress() - mid).abs() < 0.02);
        m.icon_values
            .borrow_mut()
            .get_mut("power-extra-open")
            .unwrap()
            .2 = Instant::now() - Duration::from_secs(1);
        assert_eq!(m.power_extra_progress(), 0.0);
    }
    #[test]
    fn bluetooth_pin_survives_hiding_without_overwriting_wifi_draft() {
        use super::*;
        let mut m = Model::new();
        m.secret = "wifi-draft".into();
        m.cursor = m.secret.len();
        m.navigate(Page::Bluetooth);
        let state = bt::State {
            prompt: Some(bt::Prompt {
                id: 1,
                device: "test".into(),
                kind: bt::PromptKind::Pin,
            }),
            ..bt::State::default()
        };
        m.bluetooth_state(state.clone());
        m.insert("가12");
        m.move_cursor(-1);
        m.backspace();
        assert_eq!(m.bt_input.value, "가2");
        m.hide();
        m.navigate(Page::Bluetooth);
        m.visible = true;
        m.bluetooth_state(state);
        assert!(m.entry);
        assert_eq!(m.bt_input.value, "가2");
        assert_eq!(m.secret, "wifi-draft");
        m.bluetooth_state(bt::State::default());
        assert!(m.bt_input.value.is_empty());
        assert!(!m.entry);
    }
    #[test]
    fn bluetooth_state_wins_over_stale_common_backend_snapshot() {
        use super::*;
        let mut m = Model::new();
        m.bluetooth_state(bt::State {
            ready: true,
            powered: true,
            adapter: Some("adapter".into()),
            devices: vec![bt::Device {
                name: "Connected Device".into(),
                connected: true,
                ..bt::Device::default()
            }],
            ..bt::State::default()
        });
        m.common_state(control::State::default());
        assert_eq!(m.common.bluetooth, Some(true));
        assert_eq!(m.common.bluetooth_devices.as_slice(), ["Connected Device"]);
    }
    #[test]
    fn deleting_a_notice_tracks_its_exit_and_rejects_stale_backend_rows() {
        use super::*;
        let mut m = Model::new();
        m.notifications.notices.push(crate::notifications::Notice {
            id: 41,
            app: "Fixture".into(),
            ..Default::default()
        });
        let old = m.notifications.clone();
        m.hits.insert(
            1,
            vec![
                Hit {
                    rect: Rect {
                        x: 332.0,
                        y: 110.0,
                        w: 428.0,
                        h: 80.0,
                    },
                    action: Action::NoticeHover,
                },
                Hit {
                    rect: Rect {
                        x: 726.0,
                        y: 117.0,
                        w: 28.0,
                        h: 28.0,
                    },
                    action: Action::DeleteNotices(vec![41]),
                },
            ],
        );
        m.activate(Action::DeleteNotices(vec![41]));
        assert_eq!(m.notice_revision, 1);
        let r = m.notice_exit.unwrap();
        assert_eq!(r.y, 62.0, "a final card takes its app header with it");
        assert!(r.h > 80.0);
        m.notification_state(old);
        assert!(m.notifications.notices.is_empty());
        assert!(m.removed_notices.contains(&41));
        m.notification_state(Default::default());
        assert!(m.removed_notices.is_empty());
    }
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
