//! Event-driven UPower/PPD/ASUS integration. All blocking I/O stays off the UI thread.
use gio::{DBusCallFlags, DBusConnection};
use glib::{variant::ObjectPath, ToVariant, Variant};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const BATTERY: &str = "org.freedesktop.UPower.Device";
const PPD: &str = "org.freedesktop.UPower.PowerProfiles";
const PPD_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const ASUS: &str = "xyz.ljones.Asusd";
const ASUS_PATH: &str = "/xyz/ljones";
const PLATFORM: &str = "xyz.ljones.Platform";
pub const PROFILES: [&str; 3] = ["power-saver", "balanced", "performance"];
pub const SCREEN_MINUTES: [u32; 7] = [0, 2, 5, 10, 15, 30, 60];
pub const SLEEP_MINUTES: [u32; 6] = [0, 15, 30, 60, 90, 120];
type Properties = HashMap<String, Variant>;

fn hardware_profile(value: &str) -> Option<&'static str> {
    match value.trim() {
        "quiet" | "low-power" => Some("power-saver"),
        "balanced" => Some("balanced"),
        "performance" => Some("performance"),
        _ => None,
    }
}
fn read_profile(file: &mut fs::File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    Ok(value)
}
/// ASUS Fn+F5: present the confirmed kernel change immediately, without waiting
/// for PPD, battery enumeration, or a queued full power-management snapshot.
/// A mixed-driver global platform_profile may be "custom", so observe ASUS's
/// own device rather than inferring a profile from the aggregate file.
pub fn watch_hardware_profile(mut changed: impl FnMut(String) + 'static) -> Option<glib::SourceId> {
    let device = fs::read_dir("/sys/class/platform-profile")
        .ok()?
        .flatten()
        .map(|d| d.path())
        .find(|p| fs::read_to_string(p.join("name")).is_ok_and(|s| s.trim() == "asus-wmi"))?;
    let mut device_file = fs::File::open(device.join("profile")).ok()?;
    let mut previous = hardware_profile(&read_profile(&mut device_file).ok()?)?.to_owned();
    // Per-device stores notify the legacy aggregate, but do not always notify
    // their own class attribute. Poll the aggregate and read ASUS's actual mode.
    let mut file = fs::File::open("/sys/firmware/acpi/platform_profile").ok()?;
    read_profile(&mut file).ok()?;
    Some(glib::source::unix_fd_add_local(
        file.as_raw_fd(),
        glib::IOCondition::PRI
            | glib::IOCondition::ERR
            | glib::IOCondition::HUP
            | glib::IOCondition::NVAL,
        move |_, events| {
            if events.intersects(glib::IOCondition::HUP | glib::IOCondition::NVAL) {
                return glib::ControlFlow::Break;
            }
            if read_profile(&mut file).is_err() {
                return glib::ControlFlow::Break;
            }
            let Ok(value) = read_profile(&mut device_file) else {
                return glib::ControlFlow::Break;
            };
            if let Some(profile) = hardware_profile(&value) {
                if profile != previous {
                    previous = profile.into();
                    changed(profile.into());
                }
            }
            glib::ControlFlow::Continue
        },
    ))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleTimes {
    pub screen: u32,
    pub suspend: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    Screen,
    Suspend,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub managed_ac: bool,
    pub managed_battery: bool,
    pub auto_ac: bool,
    pub auto_battery: bool,
    pub ac_profile: String,
    pub battery_profile: String,
    pub low_battery: bool,
    pub ac_idle: IdleTimes,
    pub battery_idle: IdleTimes,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            managed_ac: false,
            managed_battery: false,
            auto_ac: false,
            auto_battery: false,
            ac_profile: "balanced".into(),
            battery_profile: "power-saver".into(),
            low_battery: false,
            ac_idle: IdleTimes::default(),
            battery_idle: IdleTimes::default(),
        }
    }
}
impl Settings {
    fn managed(&self, ac: bool) -> bool {
        if ac {
            self.managed_ac
        } else {
            self.managed_battery
        }
    }
    pub fn idle(&self, ac: bool) -> IdleTimes {
        if ac {
            self.ac_idle
        } else {
            self.battery_idle
        }
    }
    pub fn automatic(&self, ac: bool) -> bool {
        if ac {
            self.auto_ac
        } else {
            self.auto_battery
        }
    }
    pub fn profile(&self, ac: bool) -> &str {
        if ac {
            &self.ac_profile
        } else {
            &self.battery_profile
        }
    }
    fn decode(value: &Value) -> io::Result<Self> {
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "지원하지 않는 전원 설정 형식입니다",
            ));
        }
        let mut s = Self::default();
        for (key, target) in [
            ("managed_ac", &mut s.managed_ac),
            ("managed_battery", &mut s.managed_battery),
            ("auto_ac", &mut s.auto_ac),
            ("auto_battery", &mut s.auto_battery),
            ("low_battery", &mut s.low_battery),
        ] {
            if let Some(v) = value.get(key) {
                *target = v.as_bool().ok_or_else(invalid_settings)?;
            }
        }
        for (key, target) in [
            ("ac_profile", &mut s.ac_profile),
            ("battery_profile", &mut s.battery_profile),
        ] {
            if let Some(v) = value.get(key) {
                *target = v
                    .as_str()
                    .filter(|p| PROFILES.contains(p))
                    .ok_or_else(invalid_settings)?
                    .into();
            }
        }
        for (key, target) in [
            ("ac_idle", &mut s.ac_idle),
            ("battery_idle", &mut s.battery_idle),
        ] {
            if let Some(v) = value.get(key) {
                let screen = v
                    .get("screen")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(invalid_settings)?;
                let suspend = v
                    .get("suspend")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(invalid_settings)?;
                if !SCREEN_MINUTES.contains(&screen)
                    || !SLEEP_MINUTES.contains(&suspend)
                    || (suspend > 0 && suspend < screen)
                {
                    return Err(invalid_settings());
                }
                *target = IdleTimes { screen, suspend };
            }
        }
        Ok(s)
    }
    fn encode(&self) -> Value {
        json!({"version":1,"managed_ac":self.managed_ac,"managed_battery":self.managed_battery,"auto_ac":self.auto_ac,"auto_battery":self.auto_battery,"ac_profile":self.ac_profile,"battery_profile":self.battery_profile,"low_battery":self.low_battery,"ac_idle":{"screen":self.ac_idle.screen,"suspend":self.ac_idle.suspend},"battery_idle":{"screen":self.battery_idle.screen,"suspend":self.battery_idle.suspend}})
    }
}
fn invalid_settings() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "전원 설정 값이 올바르지 않습니다",
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Battery {
    pub percent: Option<f64>,
    pub health: Option<f64>,
    pub energy_full: Option<f64>,
    pub energy_design: Option<f64>,
    pub watts: Option<f64>,
    pub seconds: Option<i64>,
    pub cycles: Option<i32>,
    pub state: u32,
}
impl Battery {
    fn from_properties(p: &Properties) -> Self {
        let full = positive(p, "EnergyFull");
        let design = positive(p, "EnergyFullDesign");
        let state = get(p, "State").unwrap_or(0);
        let rate = positive(p, "EnergyRate");
        let seconds = match state {
            1 => get::<i64>(p, "TimeToFull").filter(|s| *s > 0),
            2 => get::<i64>(p, "TimeToEmpty").filter(|s| *s > 0),
            _ => None,
        }
        .or_else(|| {
            let energy = positive(p, "Energy")?;
            let rate = rate?;
            let wh = match state {
                1 => (full? - energy).max(0.0),
                2 => energy,
                _ => return None,
            };
            let seconds = wh / rate * 3600.0;
            (seconds.is_finite() && seconds > 0.0 && seconds < 7.0 * 86400.0)
                .then_some(seconds as i64)
        });
        Self {
            percent: get::<f64>(p, "Percentage")
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)),
            health: full
                .zip(design)
                .map(|(f, d)| (f / d * 100.0).clamp(0.0, 100.0))
                .or_else(|| positive(p, "Capacity").filter(|p| *p <= 100.0)),
            energy_full: full,
            energy_design: design,
            watts: rate.filter(|_| matches!(state, 1 | 2)),
            seconds,
            cycles: get::<i32>(p, "ChargeCycles").filter(|c| *c >= 0),
            state,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub ready: bool,
    pub busy: bool,
    pub battery: Option<Battery>,
    pub on_battery: Option<bool>,
    pub profile: Option<String>,
    pub profiles: Vec<String>,
    pub degraded: String,
    pub charge_limit: Option<u8>,
    pub asus: bool,
    pub settings: Settings,
    pub error: String,
}
impl State {
    pub fn idle(&self) -> IdleTimes {
        // Unknown source must never arm potentially disruptive automatic actions.
        self.on_battery
            .map_or(IdleTimes::default(), |b| self.settings.idle(!b))
    }
    pub fn status(&self) -> String {
        let Some(battery) = &self.battery else {
            return "배터리 없음".into();
        };
        match battery.state {
            1 => "충전 중".into(),
            2 => "사용 중".into(),
            4 => "충전 완료".into(),
            _ if self.on_battery == Some(false) => "충전 대기".into(),
            _ => "충전 상태 확인 중".into(),
        }
    }
    pub fn estimate(&self) -> String {
        let Some(b) = &self.battery else {
            return String::new();
        };
        let mut parts = vec![];
        if let Some(seconds) = b.seconds {
            let minutes = (seconds / 60).max(1);
            let time = if minutes >= 60 {
                format!("{}시간 {}분", minutes / 60, minutes % 60)
            } else {
                format!("{minutes}분")
            };
            parts.push(format!(
                "{} {time}",
                if b.state == 1 {
                    "완충까지 약"
                } else {
                    "약"
                }
            ));
        }
        parts.join(" · ")
    }
}
#[derive(Clone, Debug)]
pub enum Request {
    Refresh,
    Profile(String),
    Automatic(bool, bool), // AC source, enabled
    AutomaticProfile(bool, String),
    LowBattery(bool),
    ChargeLimit(u8),
    Timeout(bool, Timer, u32), // AC source, timer, minutes
    Idle(Timer, bool),         // true: idle, false: resumed
    Quit,
}
pub struct Backend {
    pub tx: mpsc::Sender<Request>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Backend {
    pub fn start(events: glib::Sender<State>, readonly: bool) -> Self {
        Self::with_options(events, readonly, settings_path(), None)
    }
    fn with_options(
        events: glib::Sender<State>,
        readonly: bool,
        path: PathBuf,
        address: Option<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("lucy-power".into())
            .spawn(move || {
                let bus = address.map_or_else(
                    || gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE),
                    |address| {
                        DBusConnection::for_address_sync(
                            &address,
                            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                                | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                            None,
                            gio::Cancellable::NONE,
                        )
                    },
                );
                let bus = match bus {
                    Ok(bus) => bus,
                    Err(_) => {
                        let _ = events.send(State {
                            ready: true,
                            error: "전원 관리 서비스에 연결할 수 없습니다".into(),
                            ..State::default()
                        });
                        return;
                    }
                };
                let (settings, config_error) = match load(&path) {
                    Ok(s) => (s, String::new()),
                    Err(e) => (Settings::default(), e.to_string()),
                };
                let mut worker = Worker {
                    bus,
                    path,
                    battery_path: None,
                    state: State {
                        settings,
                        error: config_error.clone(),
                        ..State::default()
                    },
                    last_policy: None,
                    config_error,
                };
                worker.refresh();
                if !readonly {
                    worker.automate(false);
                }
                let _ = events.send(worker.state.clone());
                while let Ok(first) = rx.recv() {
                    let mut requests = vec![first];
                    requests.extend(rx.try_iter());
                    let mut dirty = false;
                    let mut force_policy = false;
                    for request in requests {
                        if matches!(request, Request::Quit) {
                            return;
                        }
                        if matches!(request, Request::Refresh) {
                            dirty = true;
                            continue;
                        }
                        if readonly {
                            continue;
                        }
                        if let Request::Idle(timer, idle) = request {
                            if let Err(error) = worker.idle(timer, idle) {
                                worker.state.error = error;
                                let _ = events.send(worker.state.clone());
                            }
                            continue;
                        }
                        worker.state.busy = true;
                        let _ = events.send(worker.state.clone());
                        let affects_current_source = match &request {
                            // Future AC/battery preferences must not change today's mode.
                            Request::LowBattery(_) => worker.state.on_battery == Some(true),
                            _ => false,
                        };
                        let result = worker.change(request);
                        force_policy |= affects_current_source && result.is_ok();
                        worker.state.error = result.err().unwrap_or_default();
                        dirty = true;
                    }
                    if dirty {
                        worker.state.busy = false;
                        worker.refresh();
                        if !readonly {
                            worker.automate(force_policy);
                        }
                        let _ = events.send(worker.state.clone());
                    }
                }
            })
            .expect("power worker");
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
struct Worker {
    bus: DBusConnection,
    path: PathBuf,
    battery_path: Option<String>,
    state: State,
    last_policy: Option<(Option<bool>, bool)>,
    config_error: String,
}
fn get<T: glib::variant::FromVariant>(p: &Properties, key: &str) -> Option<T> {
    p.get(key)?.get()
}
fn positive(p: &Properties, key: &str) -> Option<f64> {
    get::<f64>(p, key).filter(|v| v.is_finite() && *v > 0.0)
}
impl Worker {
    fn call(
        &self,
        service: &str,
        path: &str,
        interface: &str,
        method: &str,
        args: Option<&Variant>,
    ) -> Result<Variant, glib::Error> {
        self.bus.call_sync(
            Some(service),
            path,
            interface,
            method,
            args,
            None,
            DBusCallFlags::NONE,
            2500,
            gio::Cancellable::NONE,
        )
    }
    fn properties(&self, service: &str, path: &str, interface: &str) -> Option<Properties> {
        self.call(
            service,
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&(interface,).to_variant()),
        )
        .ok()?
        .child_value(0)
        .get()
    }
    fn set(
        &self,
        service: &str,
        path: &str,
        interface: &str,
        key: &str,
        value: Variant,
    ) -> Result<(), String> {
        self.call(
            service,
            path,
            "org.freedesktop.DBus.Properties",
            "Set",
            Some(&(interface, key, value).to_variant()),
        )
        .map(|_| ())
        .map_err(|_| "변경하지 못했습니다. 장치 지원 여부와 권한을 확인하세요".into())
    }
    fn refresh(&mut self) {
        self.state.ready = true;
        self.state.on_battery = self
            .properties(UPOWER, UPOWER_PATH, UPOWER)
            .and_then(|p| get(&p, "OnBattery"));
        let mut battery = self
            .battery_path
            .as_ref()
            .and_then(|path| self.properties(UPOWER, path, BATTERY));
        if battery.is_none() {
            self.battery_path = None;
            if let Ok(devices) = self.call(UPOWER, UPOWER_PATH, UPOWER, "EnumerateDevices", None) {
                let mut paths = devices
                    .child_value(0)
                    .get::<Vec<ObjectPath>>()
                    .unwrap_or_default();
                paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                for path in paths {
                    if let Some(p) = self.properties(UPOWER, &path, BATTERY) {
                        if get::<u32>(&p, "Type") == Some(2)
                            && get::<bool>(&p, "PowerSupply") == Some(true)
                            && get::<bool>(&p, "IsPresent") != Some(false)
                        {
                            self.battery_path = Some(path.to_string());
                            battery = Some(p);
                            break;
                        }
                    }
                }
            }
        }
        self.state.battery = battery.as_ref().map(Battery::from_properties);
        self.state.charge_limit = None;
        if let Some(p) = self.properties(PPD, PPD_PATH, PPD) {
            self.state.profile = get(&p, "ActiveProfile");
            self.state.profiles = get::<Vec<Properties>>(&p, "Profiles")
                .unwrap_or_default()
                .iter()
                .filter_map(|p| {
                    get::<String>(p, "Profile").filter(|s| PROFILES.contains(&s.as_str()))
                })
                .collect();
            self.state.degraded = get(&p, "PerformanceDegraded").unwrap_or_default();
        } else {
            self.state.profile = None;
            self.state.profiles.clear();
        }
        self.state.asus = false;
        if let Some(p) = self.properties(ASUS, ASUS_PATH, PLATFORM) {
            self.state.asus = p.contains_key("ChangePlatformProfileOnAc")
                && p.contains_key("ChangePlatformProfileOnBattery");
            self.state.charge_limit = get(&p, "ChargeControlEndThreshold");
            if self.state.asus {
                if !self.state.settings.managed_ac {
                    self.state.settings.auto_ac =
                        get(&p, "ChangePlatformProfileOnAc").unwrap_or(false);
                    self.state.settings.ac_profile = get::<u32>(&p, "PlatformProfileOnAc")
                        .and_then(asus_profile)
                        .unwrap_or("balanced")
                        .into();
                }
                if !self.state.settings.managed_battery {
                    self.state.settings.auto_battery =
                        get(&p, "ChangePlatformProfileOnBattery").unwrap_or(false);
                    self.state.settings.battery_profile =
                        get::<u32>(&p, "PlatformProfileOnBattery")
                            .and_then(asus_profile)
                            .unwrap_or("power-saver")
                            .into();
                }
            }
        }
        if !self.config_error.is_empty() {
            self.state.error = self.config_error.clone();
        }
    }
    fn change(&mut self, request: Request) -> Result<(), String> {
        let mut settings = self.state.settings.clone();
        match request {
            Request::Profile(profile) => return self.profile(&profile),
            Request::ChargeLimit(limit) => {
                if ![80, 100].contains(&limit) || self.state.charge_limit.is_none() {
                    return Err("지원하지 않는 충전 상한입니다".into());
                }
                return self.set(
                    ASUS,
                    ASUS_PATH,
                    PLATFORM,
                    "ChargeControlEndThreshold",
                    limit.to_variant(),
                );
            }
            Request::Automatic(ac, enabled) => {
                if ac {
                    settings.auto_ac = enabled;
                } else {
                    settings.auto_battery = enabled;
                }
                return self.save_automatic(settings, ac);
            }
            Request::AutomaticProfile(ac, profile) => {
                if !self.state.profiles.contains(&profile) {
                    return Err("지원하지 않는 전원 모드입니다".into());
                }
                if ac {
                    settings.ac_profile = profile;
                } else {
                    settings.battery_profile = profile;
                }
                return self.save_automatic(settings, ac);
            }
            Request::LowBattery(enabled) => settings.low_battery = enabled,
            Request::Timeout(ac, kind, minutes) => {
                let times = if ac {
                    &mut settings.ac_idle
                } else {
                    &mut settings.battery_idle
                };
                match kind {
                    Timer::Screen if SCREEN_MINUTES.contains(&minutes) => times.screen = minutes,
                    Timer::Suspend if SLEEP_MINUTES.contains(&minutes) => times.suspend = minutes,
                    _ => return Err("지원하지 않는 대기 시간입니다".into()),
                }
                if times.suspend > 0 && times.suspend < times.screen {
                    return Err("자동 절전 시간은 화면 꺼짐 시간보다 짧을 수 없습니다".into());
                }
            }
            Request::Refresh | Request::Quit | Request::Idle(..) => return Ok(()),
        }
        if !self.config_error.is_empty() {
            return Err(self.config_error.clone());
        }
        save(&self.path, &settings).map_err(|e| format!("전원 설정을 저장하지 못했습니다: {e}"))?;
        self.state.settings = settings;
        Ok(())
    }
    fn save_automatic(&mut self, mut settings: Settings, ac: bool) -> Result<(), String> {
        if !self.config_error.is_empty() {
            return Err(self.config_error.clone());
        }
        // ASUS's PlatformProfileOnAc/Battery setters apply the profile immediately.
        // Never call them to save a future preference. Transfer only this source's
        // automation to Lucy, without changing the current profile or other source.
        let transfer = self.state.asus && !self.state.settings.managed(ac);
        let key = if ac {
            "ChangePlatformProfileOnAc"
        } else {
            "ChangePlatformProfileOnBattery"
        };
        if transfer {
            self.set(ASUS, ASUS_PATH, PLATFORM, key, false.to_variant())?;
        }
        if ac {
            settings.managed_ac = self.state.asus || settings.managed_ac;
        } else {
            settings.managed_battery = self.state.asus || settings.managed_battery;
        }
        if let Err(e) = save(&self.path, &settings) {
            if transfer {
                self.set(
                    ASUS,
                    ASUS_PATH,
                    PLATFORM,
                    key,
                    self.state.settings.automatic(ac).to_variant(),
                )
                .map_err(|_| {
                    "저장 실패 후 ASUS 자동 전환을 복구하지 못했습니다. 자동 전환 설정을 확인하세요"
                        .to_owned()
                })?;
            }
            return Err(format!("전원 설정을 저장하지 못했습니다: {e}"));
        }
        self.state.settings = settings;
        Ok(())
    }
    fn profile(&self, profile: &str) -> Result<(), String> {
        if !self.state.profiles.iter().any(|p| p == profile) {
            return Err("지원하지 않는 전원 모드입니다".into());
        }
        self.set(PPD, PPD_PATH, PPD, "ActiveProfile", profile.to_variant())
    }
    fn idle(&self, timer: Timer, idle: bool) -> Result<(), String> {
        match (timer, idle) {
            (Timer::Screen, off) => crate::bar_backend::output(
                "niri",
                &[
                    "msg",
                    "action",
                    if off {
                        "power-off-monitors"
                    } else {
                        "power-on-monitors"
                    },
                ],
            )
            .map(|_| ())
            .ok_or_else(|| "화면 전원을 변경하지 못했습니다".into()),
            (Timer::Suspend, true) => self
                .call(
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                    "Suspend",
                    Some(&(true,).to_variant()),
                )
                .map(|_| ())
                .map_err(|_| "자동 절전 요청을 완료하지 못했습니다".into()),
            _ => Ok(()),
        }
    }
    fn automate(&mut self, force: bool) {
        let low = self.state.on_battery == Some(true)
            && self.state.settings.low_battery
            && self
                .state
                .battery
                .as_ref()
                .and_then(|b| b.percent)
                .is_some_and(|p| p < 20.0);
        let stamp = (self.state.on_battery, low);
        let previous = self.last_policy.replace(stamp);
        let changed = force || previous.is_none_or(|p| p.0 != stamp.0);
        let target = if low && (force || previous.is_none_or(|p| !p.1)) {
            Some("power-saver")
        } else if changed
            && (!self.state.asus
                || self
                    .state
                    .on_battery
                    .is_some_and(|b| self.state.settings.managed(!b))
                || force)
        {
            self.state.on_battery.and_then(|b| {
                self.state
                    .settings
                    .automatic(!b)
                    .then(|| self.state.settings.profile(!b))
            })
        } else {
            None
        };
        if let Some(target) = target
            .filter(|p| self.state.profile.as_deref() != Some(*p))
            .map(str::to_owned)
        {
            match self.profile(&target) {
                Ok(()) => self.state.profile = Some(target),
                Err(e) => self.state.error = e,
            }
        }
    }
}
fn asus_profile(profile: u32) -> Option<&'static str> {
    match profile {
        0 => Some("balanced"),
        1 => Some("performance"),
        2 | 3 => Some("power-saver"),
        _ => None,
    }
}
fn settings_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("lucy/power.json")
}
fn load(path: &Path) -> io::Result<Settings> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() <= 16384 => {
            Settings::decode(&serde_json::from_slice::<Value>(&bytes)?)
        }
        Ok(_) => Err(invalid_settings()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e),
    }
}
fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    let parent = path.parent().ok_or_else(invalid_settings)?;
    fs::create_dir_all(parent)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".power-{}-{stamp}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(serde_json::to_string_pretty(&settings.encode())?.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
#[path = "power_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_profiles_use_confirmed_modes_not_custom_or_guesses() {
        assert_eq!(hardware_profile("quiet\n"), Some("power-saver"));
        assert_eq!(hardware_profile("low-power"), Some("power-saver"));
        assert_eq!(hardware_profile("balanced"), Some("balanced"));
        assert_eq!(hardware_profile("performance"), Some("performance"));
        assert_eq!(hardware_profile("custom"), None);
        assert_eq!(hardware_profile(""), None);
    }
    #[test]
    fn charge_and_battery_health_are_independent_and_unknown_is_not_zero() {
        let p = HashMap::from([
            ("Percentage".into(), 81.0.to_variant()),
            ("EnergyFull".into(), 68.936.to_variant()),
            ("EnergyFullDesign".into(), 73.0.to_variant()),
            ("State".into(), 5u32.to_variant()),
        ]);
        let battery = Battery::from_properties(&p);
        assert_eq!(battery.percent, Some(81.0));
        assert!((battery.health.unwrap() - 94.4329).abs() < 0.001);
        assert_eq!(battery.seconds, None);
        assert_eq!(Battery::from_properties(&Properties::new()).health, None);
    }
    #[test]
    fn invalid_settings_cannot_arm_sleep_or_inject_commands() {
        let settings = Settings::default();
        assert_eq!(Settings::decode(&settings.encode()).unwrap(), settings);
        for change in [
            json!({"version":1,"ac_profile":"balanced; touch /tmp/no"}),
            json!({"version":1,"ac_idle":{"screen":30,"suspend":15}}),
            json!({"version":1,"battery_idle":{"screen":-1,"suspend":0}}),
            json!({"version":2}),
        ] {
            assert!(Settings::decode(&change).is_err());
        }
        assert_eq!(State::default().idle(), IdleTimes::default());
    }
    #[test]
    fn charging_threshold_state_does_not_claim_to_be_charging() {
        let state = State {
            on_battery: Some(false),
            charge_limit: Some(80),
            battery: Some(Battery {
                percent: Some(81.0),
                state: 5,
                ..Battery::default()
            }),
            ..State::default()
        };
        assert_eq!(state.status(), "충전 대기");
        assert!(state.estimate().is_empty());
    }
}
