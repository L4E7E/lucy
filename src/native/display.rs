//! Niri output modes and hardware brightness; blocking work stays off the UI.
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub const CONFIRM_SECONDS: u64 = 15;
pub const MIN_KELVIN: u32 = 2500;
pub const MAX_KELVIN: u32 = 6500;
pub const SCALES: [u32; 11] = [75, 100, 125, 150, 175, 200, 225, 250, 300, 350, 400];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}
impl Mode {
    pub fn argument(&self) -> String {
        format!(
            "{}x{}@{:.3}",
            self.width,
            self.height,
            f64::from(self.refresh) / 1000.0
        )
    }
    pub fn rate(&self) -> String {
        if self.refresh.is_multiple_of(1000) {
            format!("{} Hz", self.refresh / 1000)
        } else {
            format!("{:.3} Hz", f64::from(self.refresh) / 1000.0)
        }
    }
    fn decode(v: &Value) -> Option<Self> {
        let m = Self {
            width: u32::try_from(v["width"].as_u64()?).ok()?,
            height: u32::try_from(v["height"].as_u64()?).ok()?,
            refresh: u32::try_from(v["refresh_rate"].as_u64()?).ok()?,
        };
        (m.width > 0 && m.height > 0 && m.refresh > 0).then_some(m)
    }
    fn encode(&self) -> Value {
        json!({"width":self.width,"height":self.height,"refresh_rate":self.refresh})
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Brightness {
    Backlight(String),
    Ddc(u32, u32),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub name: String,
    pub label: String,
    pub identity: String,
    pub current: Mode,
    pub modes: Vec<Mode>,
    pub scale: f64,
    pub x: i32,
    pub y: i32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub brightness: Option<f64>,
    pub control: Option<Brightness>,
    pub brightness_error: String,
}
impl Output {
    pub fn resolutions(&self) -> Vec<(u32, u32)> {
        let mut values: Vec<_> = self.modes.iter().map(|m| (m.width, m.height)).collect();
        values.sort_unstable_by(|a, b| b.cmp(a));
        values.dedup();
        values
    }
    pub fn resolution_mode(&self, width: u32, height: u32) -> Option<Mode> {
        self.modes
            .iter()
            .filter(|m| m.width == width && m.height == height)
            .min_by_key(|m| m.refresh.abs_diff(self.current.refresh))
            .cloned()
    }
}
pub fn parse_outputs(v: &Value) -> Vec<Output> {
    let mut outputs = vec![];
    for (name, v) in v.as_object().into_iter().flatten() {
        if !v["logical"].is_object() {
            continue;
        }
        let modes: Vec<_> = v["modes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Mode::decode)
            .collect();
        let Some(current) = v["current_mode"]
            .as_u64()
            .and_then(|i| modes.get(i as usize))
            .cloned()
        else {
            continue;
        };
        let make = v["make"].as_str().unwrap_or("");
        let model = v["model"].as_str().unwrap_or("");
        let serial = v["serial"].as_str().unwrap_or("");
        outputs.push(Output {
            name: name.clone(),
            label: {
                let device = format!("{} {}", make.trim(), model.trim());
                if device.trim().is_empty() {
                    name.clone()
                } else {
                    device.trim().to_owned()
                }
            },
            identity: format!("{name}|{make}|{model}|{serial}"),
            current,
            modes,
            scale: v["logical"]["scale"].as_f64().unwrap_or(1.0),
            x: v["logical"]["x"].as_i64().unwrap_or(0) as i32,
            y: v["logical"]["y"].as_i64().unwrap_or(0) as i32,
            logical_width: v["logical"]["width"].as_u64().unwrap_or(1) as u32,
            logical_height: v["logical"]["height"].as_u64().unwrap_or(1) as u32,
            brightness: None,
            control: None,
            brightness_error: String::new(),
        });
    }
    outputs.sort_by_key(|o| (!o.name.starts_with("eDP-"), o.name.clone()));
    outputs
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Night {
    pub enabled: bool,
    pub automatic: bool,
    pub start: u32,
    pub end: u32,
    pub kelvin: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockPart {
    Hour,
    Minute,
}
impl Default for Night {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: false,
            start: 21 * 60,
            end: 7 * 60,
            kelvin: 4500,
        }
    }
}
impl Night {
    pub fn adjust(&mut self, start: bool, part: ClockPart, steps: i32) -> Result<(), String> {
        let value = if start { self.start } else { self.end };
        let next = match part {
            ClockPart::Hour => {
                (i64::from(value / 60) + i64::from(steps)).rem_euclid(24) as u32 * 60 + value % 60
            }
            ClockPart::Minute => {
                value / 60 * 60 + (i64::from(value % 60) + i64::from(steps)).rem_euclid(60) as u32
            }
        };
        if next == if start { self.end } else { self.start } {
            return Err("시작과 종료 시간은 다르게 설정하세요".into());
        }
        if start {
            self.start = next
        } else {
            self.end = next
        }
        Ok(())
    }
    pub fn active(&self, minute: u32) -> bool {
        self.enabled
            && (!self.automatic
                || if self.start < self.end {
                    minute >= self.start && minute < self.end
                } else {
                    minute >= self.start || minute < self.end
                })
    }
    pub fn temperature(&self, minute: u32) -> Option<u32> {
        self.active(minute).then_some(self.kelvin)
    }
    pub fn time(value: u32) -> String {
        format!("{:02}:{:02}", value / 60, value % 60)
    }
    pub fn percent(&self) -> f64 {
        f64::from(self.kelvin - MIN_KELVIN) / f64::from(MAX_KELVIN - MIN_KELVIN) * 100.0
    }
}
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub night: Night,
    pub modes: BTreeMap<String, Mode>,
    pub scales: BTreeMap<String, f64>,
    pub positions: BTreeMap<String, (i32, i32)>,
}
impl Settings {
    fn decode(v: &Value) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "디스플레이 설정 형식이 올바르지 않습니다",
            )
        };
        if v["version"] != 1 {
            return Err(invalid());
        }
        let n = &v["night"];
        let night = Night {
            enabled: n["enabled"].as_bool().ok_or_else(invalid)?,
            automatic: n["automatic"].as_bool().ok_or_else(invalid)?,
            start: n["start"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v < 1440)
                .ok_or_else(invalid)?,
            end: n["end"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v < 1440)
                .ok_or_else(invalid)?,
            kelvin: n["kelvin"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| (MIN_KELVIN..=MAX_KELVIN).contains(v))
                .ok_or_else(invalid)?,
        };
        if night.start == night.end {
            return Err(invalid());
        }
        let mut modes = BTreeMap::new();
        for (k, v) in v["modes"].as_object().ok_or_else(invalid)? {
            modes.insert(k.clone(), Mode::decode(v).ok_or_else(invalid)?);
        }
        let mut scales = BTreeMap::new();
        if v.get("scales").is_some_and(|v| !v.is_object())
            || v.get("positions").is_some_and(|v| !v.is_object())
        {
            return Err(invalid());
        }
        for (k, v) in v["scales"].as_object().into_iter().flatten() {
            scales.insert(
                k.clone(),
                v.as_f64()
                    .filter(|v| v.is_finite() && (0.5..=4.0).contains(v))
                    .ok_or_else(invalid)?,
            );
        }
        let mut positions = BTreeMap::new();
        for (k, v) in v["positions"].as_object().into_iter().flatten() {
            positions.insert(
                k.clone(),
                (
                    v[0].as_i64()
                        .and_then(|v| i32::try_from(v).ok())
                        .filter(|v| v.unsigned_abs() <= 32768)
                        .ok_or_else(invalid)?,
                    v[1].as_i64()
                        .and_then(|v| i32::try_from(v).ok())
                        .filter(|v| v.unsigned_abs() <= 32768)
                        .ok_or_else(invalid)?,
                ),
            );
        }
        Ok(Self {
            night,
            modes,
            scales,
            positions,
        })
    }
    fn encode(&self) -> Value {
        json!({"version":1,"night":{"enabled":self.night.enabled,"automatic":self.night.automatic,"start":self.night.start,"end":self.night.end,"kelvin":self.night.kelvin},"modes":self.modes.iter().map(|(k,v)|(k.clone(),v.encode())).collect::<BTreeMap<_,_>>(),"scales":self.scales,"positions":self.positions})
    }
}
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("lucy/display.json")
}
fn save(path: &Path, s: &Settings) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("설정 경로 오류"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        f.write_all(serde_json::to_string_pretty(&s.encode())?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[derive(Clone, Debug)]
pub struct Pending {
    pub before: Vec<Output>,
    pub after: Vec<Output>,
    pub deadline: Instant,
}
impl Pending {
    pub fn remaining(&self) -> u64 {
        self.deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            + 1
    }
}
#[derive(Clone, Debug, Default)]
pub struct State {
    pub ready: bool,
    pub outputs: Vec<Output>,
    pub settings: Settings,
    pub pending: Option<Pending>,
    pub error: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Refresh,
    Mode(String, Mode),
    Scale(String, u32),
    Place(Vec<Placement>),
    Keep,
    Revert,
    Brightness(String, u32),
    Night(bool),
    Automatic(bool),
    Temperature(u32),
    Time(bool, ClockPart, i32),
    Quit,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Menu {
    Resolution(String),
    Rate(String),
    Scale(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub name: String,
    pub identity: String,
    pub x: i32,
    pub y: i32,
}
impl Placement {
    pub fn from_output(o: &Output) -> Self {
        Self {
            name: o.name.clone(),
            identity: o.identity.clone(),
            x: o.x,
            y: o.y,
        }
    }
}
pub fn same_layout(a: &[Output], b: &[Output]) -> bool {
    a.len() == b.len()
        && a.iter().all(|o| {
            b.iter().any(|p| {
                o.identity == p.identity
                    && o.current == p.current
                    && (o.scale - p.scale).abs() < 0.0001
                    && o.x == p.x
                    && o.y == p.y
            })
        })
}
pub fn validate_placements(outputs: &[Output], positions: &[Placement]) -> Result<(), String> {
    if outputs.len() != positions.len() || positions.is_empty() {
        return Err("화면 목록이 변경되었습니다. 새로고침해 주세요".into());
    }
    for (i, p) in positions.iter().enumerate() {
        let o = outputs
            .iter()
            .find(|o| o.identity == p.identity && o.name == p.name)
            .ok_or("화면 연결이 변경되었습니다")?;
        if p.x.unsigned_abs() > 32768
            || p.y.unsigned_abs() > 32768
            || positions[..i].iter().any(|v| v.name == p.name)
        {
            return Err("올바르지 않은 화면 위치입니다".into());
        }
        for other in &positions[..i] {
            let b = outputs
                .iter()
                .find(|o| o.identity == other.identity)
                .ok_or("화면 연결이 변경되었습니다")?;
            if p.x < other.x + b.logical_width as i32
                && other.x < p.x + o.logical_width as i32
                && p.y < other.y + b.logical_height as i32
                && other.y < p.y + o.logical_height as i32
            {
                return Err("디스플레이가 겹치지 않도록 배치해 주세요".into());
            }
        }
    }
    Ok(())
}
fn restore_layout(before: &[Output]) -> Result<(), String> {
    let mut current = query()?;
    for old in before {
        let Some(now) = current.iter().find(|o| o.identity == old.identity) else {
            continue;
        };
        if old.current != now.current && now.modes.contains(&old.current) {
            apply_mode(&old.name, &old.current)?;
        }
        if (old.scale - now.scale).abs() > 0.0001 {
            read(
                "niri",
                &["msg", "output", &old.name, "scale", &old.scale.to_string()],
            )?;
        }
    }
    current = query()?;
    for old in before {
        if current
            .iter()
            .any(|o| o.identity == old.identity && (o.x != old.x || o.y != old.y))
        {
            read(
                "niri",
                &[
                    "msg",
                    "output",
                    &old.name,
                    "position",
                    "set",
                    &old.x.to_string(),
                    &old.y.to_string(),
                ],
            )?;
        }
    }
    Ok(())
}

fn read(program: &str, args: &[&str]) -> Result<String, String> {
    #[cfg(test)]
    if let Some(result) =
        TEST_COMMAND.with(|mock| mock.borrow_mut().as_mut().map(|f| f(program, args)))
    {
        return result;
    }
    crate::bar_backend::output(program, args)
        .ok_or_else(|| format!("{program} 명령을 실행하지 못했습니다. 장치와 권한을 확인하세요."))
}
fn query() -> Result<Vec<Output>, String> {
    let v = read("niri", &["msg", "-j", "outputs"])?;
    serde_json::from_str(&v)
        .map(|v| parse_outputs(&v))
        .map_err(|_| "Niri 출력 정보를 읽지 못했습니다".into())
}
fn apply_mode(name: &str, mode: &Mode) -> Result<(), String> {
    read("niri", &["msg", "output", name, "mode", &mode.argument()]).map(|_| ())
}
fn backlight(name: &str) -> Option<(Brightness, f64)> {
    if !name.starts_with("eDP-") {
        return None;
    }
    let gpu = fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter(|p| {
            p.file_name()
                .to_string_lossy()
                .ends_with(&format!("-{name}"))
        })
        .find(|p| {
            fs::read_to_string(p.path().join("status")).is_ok_and(|s| s.trim() == "connected")
        })?
        .path()
        .join("device/device")
        .canonicalize()
        .ok()?;
    fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .find_map(|d| {
            let path = d.path().canonicalize().ok()?;
            if !path.starts_with(&gpu) {
                return None;
            }
            let max = fs::read_to_string(path.join("max_brightness"))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()?;
            let value = fs::read_to_string(path.join("brightness"))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()?;
            (max > 0.0).then(|| {
                (
                    Brightness::Backlight(d.file_name().to_string_lossy().into_owned()),
                    (value / max * 100.0).clamp(0.0, 100.0),
                )
            })
        })
}
fn ddc_displays(value: &str) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    let (mut bus, mut connector) = (None, None);
    for line in value.lines().chain(std::iter::once("Display end")) {
        let line = line.trim();
        if line.starts_with("Display ") || line.starts_with("Invalid display") {
            if let (Some(b), Some(c)) = (bus.take(), connector.take()) {
                map.insert(c, b);
            }
        }
        if let Some(v) = line.strip_prefix("I2C bus:") {
            bus = v
                .trim()
                .strip_prefix("/dev/i2c-")
                .and_then(|s| s.parse().ok());
        }
        if let Some(v) = line.strip_prefix("DRM connector:") {
            connector = v.trim().split_once('-').map(|(_, c)| c.to_owned());
        }
    }
    map
}
fn brightness(outputs: &mut [Output]) {
    let external = outputs.iter().any(|o| {
        !o.name.starts_with("eDP-") && o.control.is_none() && o.brightness_error.is_empty()
    });
    let ddc = if external {
        crate::bar_backend::output("ddcutil", &["detect", "--brief"])
            .map(|s| ddc_displays(&s))
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    for o in outputs {
        if let Some((control, value)) = backlight(&o.name) {
            o.control = Some(control);
            o.brightness = Some(value);
            continue;
        }
        if o.control.is_some() || !o.brightness_error.is_empty() {
            continue;
        }
        if let Some(bus) = ddc.get(&o.name) {
            if let Ok(value) = read(
                "ddcutil",
                &["--bus", &bus.to_string(), "getvcp", "10", "--terse"],
            ) {
                let fields: Vec<_> = value.split_whitespace().collect();
                if let (Some(value), Some(max)) = (
                    fields.get(3).and_then(|v| v.parse::<f64>().ok()),
                    fields.get(4).and_then(|v| v.parse::<f64>().ok()),
                ) {
                    if max > 0.0 && max <= 65535.0 && value.is_finite() && max.is_finite() {
                        o.control = Some(Brightness::Ddc(*bus, max as u32));
                        o.brightness = Some((value / max * 100.0).clamp(0.0, 100.0));
                        continue;
                    }
                }
            }
        }
        o.brightness_error = if o.name.starts_with("eDP-") {
            "이 화면의 백라이트를 찾지 못했습니다"
        } else {
            "밝기 조절에는 모니터의 DDC/CI 지원과 ddcutil 접근 권한이 필요합니다"
        }
        .into();
    }
}
pub struct Backend {
    pub tx: mpsc::Sender<Request>,
    worker: Option<thread::JoinHandle<()>>,
}

// Roll back only outputs we still own, not a newly plugged monitor or a change
// made independently by the user while confirmation was open.
fn rollback(p: &Pending) -> Result<(), String> {
    let current = query()?;
    let owned: Vec<_> = p
        .before
        .iter()
        .filter(|old| {
            current
                .iter()
                .find(|o| o.identity == old.identity)
                .is_some_and(|now| {
                    p.after.is_empty()
                        || p.after
                            .iter()
                            .find(|o| o.identity == old.identity)
                            .is_some_and(|after| {
                                same_layout(std::slice::from_ref(now), std::slice::from_ref(after))
                            })
                })
        })
        .cloned()
        .collect();
    restore_layout(&owned)
}
fn change(state: &mut State, request: &Request) -> Result<(), String> {
    let outputs = query()?;
    let previous = state.pending.clone();
    if previous
        .as_ref()
        .is_some_and(|p| !p.after.is_empty() && !same_layout(&outputs, &p.after))
    {
        return Err(
            "화면 연결이나 설정이 외부에서 변경되었습니다. 먼저 되돌리기를 눌러 주세요".into(),
        );
    }
    // Every unconfirmed edit shares the first baseline, not the previous edit.
    let baseline = previous
        .as_ref()
        .map_or_else(|| outputs.clone(), |p| p.before.clone());
    match request {
        Request::Mode(name, mode) => {
            let o = outputs
                .iter()
                .find(|o| &o.name == name)
                .ok_or("화면 연결이 해제되었습니다")?;
            if !o.modes.contains(mode) {
                return Err("이 화면에서 지원하지 않는 모드입니다".into());
            }
            if o.current == *mode {
                return Ok(());
            }
        }
        Request::Scale(name, percent) => {
            let o = outputs
                .iter()
                .find(|o| &o.name == name)
                .ok_or("화면 연결이 해제되었습니다")?;
            if !SCALES.contains(percent) {
                return Err("지원하지 않는 배율입니다".into());
            }
            if (o.scale - f64::from(*percent) / 100.0).abs() < 0.0001 {
                return Ok(());
            }
        }
        Request::Place(positions) => validate_placements(&outputs, positions)?,
        _ => return Err("화면 변경 요청 오류".into()),
    }
    state.pending = Some(Pending {
        before: baseline.clone(),
        after: vec![],
        deadline: Instant::now() + Duration::from_secs(CONFIRM_SECONDS),
    });
    let applied = (|| -> Result<(), String> {
        match request {
            Request::Mode(name, mode) => apply_mode(name, mode)?,
            Request::Scale(name, percent) => {
                read(
                    "niri",
                    &[
                        "msg",
                        "output",
                        name,
                        "scale",
                        &(f64::from(*percent) / 100.0).to_string(),
                    ],
                )?;
            }
            Request::Place(positions) => {
                for p in positions {
                    read(
                        "niri",
                        &[
                            "msg",
                            "output",
                            &p.name,
                            "position",
                            "set",
                            &p.x.to_string(),
                            &p.y.to_string(),
                        ],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    })();
    if let Err(e) = applied {
        if restore_layout(&outputs).is_ok() {
            state.pending = previous;
        }
        return Err(e);
    }
    let after = query()?;
    if same_layout(&baseline, &after) {
        state.pending = None;
    } else if let Some(p) = &mut state.pending {
        p.after = after.clone();
        p.deadline = Instant::now() + Duration::from_secs(CONFIRM_SECONDS);
    }
    state.outputs = after;
    Ok(())
}
fn keep(state: &mut State, path: &Path) -> Result<(), String> {
    if let Some(p) = &state.pending {
        if !same_layout(&query()?, &p.after) {
            return Err("화면 상태가 변경되어 저장하지 않았습니다".into());
        }
        let mut settings = state.settings.clone();
        for o in &p.after {
            settings.modes.insert(o.identity.clone(), o.current.clone());
            settings.scales.insert(o.identity.clone(), o.scale);
            settings.positions.insert(o.identity.clone(), (o.x, o.y));
        }
        save(path, &settings).map_err(|e| e.to_string())?;
        state.settings = settings;
        state.pending = None;
    }
    Ok(())
}
fn coalescible(a: &Request, b: &Request) -> bool {
    match (a, b) {
        (Request::Brightness(a, _), Request::Brightness(b, _)) => a == b,
        (Request::Temperature(_), Request::Temperature(_))
        | (Request::Refresh, Request::Refresh) => true,
        _ => false,
    }
}
fn configured(output: &Output, settings: &Settings) -> Output {
    let mut saved = output.clone();
    if let Some(mode) = settings
        .modes
        .get(&output.identity)
        .filter(|m| output.modes.contains(m))
    {
        saved.current = mode.clone();
    }
    if let Some(scale) = settings.scales.get(&output.identity) {
        saved.scale = *scale;
    }
    if let Some((x, y)) = settings.positions.get(&output.identity) {
        saved.x = *x;
        saved.y = *y;
    }
    saved
}
fn latest(
    mut request: Request,
    rx: &mpsc::Receiver<Request>,
    queue: &mut VecDeque<Request>,
) -> Request {
    while let Ok(next) = rx.try_recv() {
        if coalescible(&request, &next) {
            request = next
        } else {
            queue.push_back(next);
            break;
        }
    }
    request
}

#[cfg(test)]
type MockCommand = Box<dyn FnMut(&str, &[&str]) -> Result<String, String>>;
#[cfg(test)]
thread_local! {static TEST_COMMAND:std::cell::RefCell<Option<MockCommand>>=const{std::cell::RefCell::new(None)};}
impl Backend {
    pub fn start(events: glib::Sender<State>, readonly: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("lucy-display".into())
            .spawn(move || {
                let path = config_path();
                let settings = match fs::read_to_string(&path) {
                    Ok(v) => serde_json::from_str(&v)
                        .map_err(io::Error::other)
                        .and_then(|v| Settings::decode(&v)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
                    Err(e) => Err(e),
                };
                let (mut state, config_ok) = match settings {
                    Ok(s) => (
                        State {
                            settings: s,
                            ..State::default()
                        },
                        true,
                    ),
                    Err(e) => (
                        State {
                            error: e.to_string(),
                            ..State::default()
                        },
                        false,
                    ),
                };
                let refresh = |s: &mut State| {
                    match query() {
                        Ok(mut o) => {
                            for output in &mut o {
                                if !output.name.starts_with("eDP-") {
                                    if let Some(old) =
                                        s.outputs.iter().find(|p| p.identity == output.identity)
                                    {
                                        output.control = old.control.clone();
                                        output.brightness = old.brightness;
                                        if s.pending.is_some() {
                                            output.brightness_error = old.brightness_error.clone();
                                        }
                                    }
                                }
                            }
                            brightness(&mut o);
                            s.outputs = o
                        }
                        Err(e) => s.error = e,
                    }
                    s.ready = true;
                };
                refresh(&mut state);
                if !readonly && config_ok {
                    let saved: Vec<_> = state
                        .outputs
                        .iter()
                        .map(|o| configured(o, &state.settings))
                        .collect();
                    if !same_layout(&saved, &state.outputs) {
                        if let Err(e) = restore_layout(&saved) {
                            state.error = e;
                        }
                    }
                    refresh(&mut state);
                }
                let _ = events.send(state.clone());
                let mut queue = VecDeque::new();
                loop {
                    let request = if let Some(request) = queue.pop_front() {
                        Ok(request)
                    } else if state.pending.is_some() {
                        rx.recv_timeout(Duration::from_secs(1))
                            .or_else(|e| match e {
                                mpsc::RecvTimeoutError::Timeout => Ok(Request::Refresh),
                                e => Err(e),
                            })
                    } else {
                        rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    };
                    let Ok(request) = request else {
                        if let Some(p) = &state.pending {
                            let _ = rollback(p);
                        }
                        break;
                    };
                    let request = latest(request, &rx, &mut queue);
                    let quit = request == Request::Quit;
                    if readonly && !matches!(request, Request::Refresh | Request::Quit) {
                        continue;
                    }
                    let result = (|| -> Result<(), String> {
                        if (quit
                            || request == Request::Revert
                            || state
                                .pending
                                .as_ref()
                                .is_some_and(|p| Instant::now() >= p.deadline))
                            && state.pending.is_some()
                        {
                            rollback(state.pending.as_ref().unwrap())?;
                            state.pending = None;
                            refresh(&mut state);
                            if request == Request::Revert || quit {
                                return Ok(());
                            }
                        }
                        match request {
                            Request::Refresh => {
                                let previous: Vec<_> =
                                    state.outputs.iter().map(|o| o.identity.clone()).collect();
                                refresh(&mut state);
                                if !readonly && config_ok && state.pending.is_none() {
                                    let added: Vec<_> = state
                                        .outputs
                                        .iter()
                                        .filter(|o| !previous.contains(&o.identity))
                                        .filter_map(|o| {
                                            let saved = configured(o, &state.settings);
                                            (!same_layout(
                                                std::slice::from_ref(o),
                                                std::slice::from_ref(&saved),
                                            ))
                                            .then_some(saved)
                                        })
                                        .collect();
                                    if !added.is_empty() {
                                        restore_layout(&added)?;
                                        refresh(&mut state);
                                    }
                                }
                            }
                            Request::Mode(_, _) | Request::Scale(_, _) | Request::Place(_) => {
                                change(&mut state, &request)?;
                                refresh(&mut state);
                            }
                            Request::Keep => keep(&mut state, &path)?,
                            Request::Brightness(ref name, percent) => {
                                let o = state
                                    .outputs
                                    .iter_mut()
                                    .find(|o| &o.name == name)
                                    .ok_or("화면 연결이 해제되었습니다")?;
                                let value = percent.clamp(1, 100).to_string();
                                match o
                                    .control
                                    .as_ref()
                                    .ok_or("이 화면에서는 밝기를 조절할 수 없습니다")?
                                {
                                    Brightness::Backlight(device) => {
                                        read(
                                            "brightnessctl",
                                            &["--device", device, "set", &format!("{value}%")],
                                        )?;
                                    }
                                    Brightness::Ddc(bus, max) => {
                                        read(
                                            "ddcutil",
                                            &[
                                                "--bus",
                                                &bus.to_string(),
                                                "setvcp",
                                                "10",
                                                &(f64::from(percent.clamp(1, 100))
                                                    * f64::from(*max)
                                                    / 100.0)
                                                    .round()
                                                    .to_string(),
                                            ],
                                        )?;
                                    }
                                }
                                o.brightness = Some(f64::from(percent.clamp(1, 100)));
                            }
                            Request::Night(_)
                            | Request::Automatic(_)
                            | Request::Temperature(_)
                            | Request::Time(_, _, _) => {
                                let mut settings = state.settings.clone();
                                let n = &mut settings.night;
                                match request {
                                    Request::Night(v) => n.enabled = v,
                                    Request::Automatic(v) => n.automatic = v,
                                    Request::Temperature(v) => {
                                        n.kelvin = v.clamp(MIN_KELVIN, MAX_KELVIN)
                                    }
                                    Request::Time(start, part, steps) => {
                                        n.adjust(start, part, steps)?
                                    }
                                    _ => {}
                                }
                                save(&path, &settings).map_err(|e| e.to_string())?;
                                state.settings = settings;
                            }
                            Request::Revert | Request::Quit => {}
                        }
                        Ok(())
                    })();
                    if let Err(e) = result {
                        state.error = e
                    } else if request != Request::Refresh {
                        state.error.clear();
                    }
                    let _ = events.send(state.clone());
                    if quit {
                        break;
                    }
                }
            })
            .expect("display worker");
        Self {
            tx,
            worker: Some(worker),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Quit);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    pub fn fixture() -> Value {
        json!({"eDP-1":{"make":"BOE","model":"Internal","serial":"A","modes":[{"width":2560,"height":1600,"refresh_rate":60002},{"width":2560,"height":1600,"refresh_rate":165000}],"current_mode":1,"logical":{"x":0,"y":0,"width":1706,"height":1066,"scale":1.5}},"DP-3":{"make":"Test","model":"External","serial":"B","modes":[{"width":1920,"height":1080,"refresh_rate":74973},{"width":1920,"height":1080,"refresh_rate":59940}],"current_mode":0,"logical":{"x":1706,"y":0,"width":1920,"height":1080,"scale":1.0}}})
    }
    struct Fake {
        outputs: Rc<RefCell<Value>>,
        writes: Rc<RefCell<Vec<Vec<String>>>>,
    }
    impl Fake {
        fn new(fail: bool) -> Self {
            let outputs = Rc::new(RefCell::new(fixture()));
            let writes = Rc::new(RefCell::new(vec![]));
            let o = outputs.clone();
            let w = writes.clone();
            let mut fail = fail;
            TEST_COMMAND.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |program, args| {
                    assert_eq!(program, "niri");
                    if args == ["msg", "-j", "outputs"] {
                        return Ok(o.borrow().to_string());
                    }
                    w.borrow_mut()
                        .push(args.iter().map(|s| (*s).to_string()).collect());
                    if fail && args[3] == "position" && args[2] == "DP-3" {
                        fail = false;
                        return Err("simulated second output failure".into());
                    }
                    let mut data = o.borrow_mut();
                    let output = &mut data[args[2]];
                    match args[3] {
                        "mode" => {
                            let mode = output["modes"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .position(|v| Mode::decode(v).unwrap().argument() == args[4])
                                .unwrap();
                            output["current_mode"] = json!(mode);
                        }
                        "scale" => {
                            output["logical"]["scale"] = json!(args[4].parse::<f64>().unwrap());
                        }
                        "position" => {
                            assert_eq!(args[4], "set");
                            output["logical"]["x"] = json!(args[5].parse::<i32>().unwrap());
                            output["logical"]["y"] = json!(args[6].parse::<i32>().unwrap());
                        }
                        _ => panic!("unexpected command {args:?}"),
                    }
                    Ok(String::new())
                }))
            });
            Self { outputs, writes }
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            TEST_COMMAND.with(|slot| slot.borrow_mut().take());
        }
    }
    #[test]
    fn successive_unconfirmed_edits_share_the_first_baseline_and_restart_timeout() {
        let _f = Fake::new(false);
        let original = query().unwrap();
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        state.pending.as_mut().unwrap().deadline = Instant::now();
        change(
            &mut state,
            &Request::Mode("eDP-1".into(), original[0].modes[0].clone()),
        )
        .unwrap();
        let pending = state.pending.as_ref().unwrap();
        assert!(same_layout(&pending.before, &original));
        assert!(pending.remaining() >= CONFIRM_SECONDS - 1);
        assert_eq!(state.outputs[0].scale, 2.0);
        rollback(pending).unwrap();
        assert!(same_layout(&query().unwrap(), &original));
    }
    #[test]
    fn returning_to_original_clears_confirmation() {
        let _f = Fake::new(false);
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        change(&mut state, &Request::Scale("eDP-1".into(), 150)).unwrap();
        assert!(state.pending.is_none());
    }
    #[test]
    fn failed_later_edit_restores_previous_candidate_and_keeps_first_baseline() {
        let _f = Fake::new(true);
        let original = query().unwrap();
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        let candidate = query().unwrap();
        let mut positions: Vec<_> = candidate.iter().map(Placement::from_output).collect();
        positions[0].x = 1920;
        positions[1].x = 0;
        assert!(change(&mut state, &Request::Place(positions)).is_err());
        assert!(same_layout(&query().unwrap(), &candidate));
        assert!(same_layout(
            &state.pending.as_ref().unwrap().before,
            &original
        ));
        rollback(state.pending.as_ref().unwrap()).unwrap();
        assert!(same_layout(&query().unwrap(), &original));
    }
    #[test]
    fn scale_and_mode_changes_revert_without_touching_confirmation_store() {
        let f = Fake::new(false);
        let original = query().unwrap();
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        assert_eq!(state.outputs[0].scale, 2.0);
        assert!(state.pending.as_ref().unwrap().remaining() <= 16);
        assert!(state.settings.scales.is_empty());
        rollback(state.pending.take().as_ref().unwrap()).unwrap();
        assert!(same_layout(&query().unwrap(), &original));
        let mode = original[0].modes[0].clone();
        change(&mut state, &Request::Mode("eDP-1".into(), mode)).unwrap();
        rollback(state.pending.as_ref().unwrap()).unwrap();
        assert!(same_layout(&query().unwrap(), &original));
        assert!(f
            .writes
            .borrow()
            .iter()
            .any(|a| a.last().is_some_and(|s| s == "2560x1600@60.002")));
    }
    #[test]
    fn unsupported_modes_scales_and_overlapping_layouts_never_write() {
        let f = Fake::new(false);
        let outputs = query().unwrap();
        let mut state = State::default();
        assert!(change(&mut state, &Request::Scale("eDP-1".into(), 999)).is_err());
        assert!(change(
            &mut state,
            &Request::Mode(
                "eDP-1".into(),
                Mode {
                    width: 999,
                    height: 999,
                    refresh: 60000
                }
            )
        )
        .is_err());
        let mut positions: Vec<_> = outputs.iter().map(Placement::from_output).collect();
        positions[1].x = 0;
        assert!(change(&mut state, &Request::Place(positions.clone())).is_err());
        positions[1].x = i32::MIN;
        assert!(validate_placements(&outputs, &positions).is_err());
        assert!(f.writes.borrow().is_empty());
        assert!(state.pending.is_none());
    }
    #[test]
    fn partial_position_failure_restores_all_original_positions() {
        let _f = Fake::new(true);
        let outputs = query().unwrap();
        let mut state = State::default();
        let mut positions: Vec<_> = outputs.iter().map(Placement::from_output).collect();
        positions[0].x = 1920;
        positions[1].x = 0;
        assert!(change(&mut state, &Request::Place(positions)).is_err());
        assert!(same_layout(&query().unwrap(), &outputs));
        assert!(state.pending.is_none());
    }
    #[test]
    fn unplugged_or_replaced_monitor_is_never_reconfigured_by_rollback() {
        let f = Fake::new(false);
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        f.outputs
            .borrow_mut()
            .as_object_mut()
            .unwrap()
            .remove("DP-3");
        rollback(state.pending.as_ref().unwrap()).unwrap();
        assert_eq!(query().unwrap()[0].scale, 1.5);
        state.pending = None;
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        f.outputs.borrow_mut()["eDP-1"]["serial"] = json!("different hardware");
        f.writes.borrow_mut().clear();
        rollback(state.pending.as_ref().unwrap()).unwrap();
        assert!(f.writes.borrow().is_empty());
    }
    #[test]
    fn external_scale_edit_is_not_overwritten_by_rollback() {
        let f = Fake::new(false);
        let mut state = State::default();
        change(&mut state, &Request::Scale("eDP-1".into(), 200)).unwrap();
        f.outputs.borrow_mut()["eDP-1"]["logical"]["scale"] = json!(1.75);
        rollback(state.pending.as_ref().unwrap()).unwrap();
        assert_eq!(query().unwrap()[0].scale, 1.75);
    }
    #[test]
    fn only_explicit_keep_persists_exact_modes_scales_and_positions() {
        use std::os::unix::fs::PermissionsExt;
        let _f = Fake::new(false);
        let mut state = State::default();
        let dir = std::env::temp_dir().join(format!(
            "lucy-display-store.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("display.json");
        change(&mut state, &Request::Scale("eDP-1".into(), 175)).unwrap();
        assert!(!path.exists());
        keep(&mut state, &path).unwrap();
        assert!(state.pending.is_none());
        let settings =
            Settings::decode(&serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()).unwrap();
        let identity = &state.outputs[0].identity;
        assert_eq!(settings.scales[identity], 1.75);
        assert_eq!(settings.modes[identity].refresh, 165000);
        assert_eq!(settings.positions[identity], (0, 0));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn slider_coalescing_preserves_keep_and_quit_order() {
        let (tx, rx) = mpsc::channel();
        for r in [
            Request::Temperature(3500),
            Request::Temperature(4000),
            Request::Keep,
            Request::Temperature(5000),
            Request::Quit,
        ] {
            tx.send(r).unwrap();
        }
        let mut queue = VecDeque::new();
        assert_eq!(
            latest(Request::Temperature(3000), &rx, &mut queue),
            Request::Temperature(4000)
        );
        assert_eq!(queue.pop_front(), Some(Request::Keep));
        assert_eq!(rx.recv().unwrap(), Request::Temperature(5000));
        assert_eq!(rx.recv().unwrap(), Request::Quit);
    }
    #[test]
    fn schedules_include_midnight_without_inverting_daytime() {
        let mut n = Night {
            enabled: true,
            automatic: true,
            ..Night::default()
        };
        for (m, on) in [
            (0, true),
            (420, false),
            (1200, false),
            (1260, true),
            (1439, true),
        ] {
            assert_eq!(n.active(m), on)
        }
        n.start = 8 * 60;
        n.end = 17 * 60;
        assert!(!n.active(0));
        assert!(n.active(12 * 60));
        n.enabled = false;
        assert!(!n.active(12 * 60));
    }
    #[test]
    fn settings_roundtrip_rejects_invalid_temperatures_and_times() {
        let s = Settings::default();
        assert_eq!(Settings::decode(&s.encode()).unwrap().night, s.night);
        for (key, v) in [
            ("kelvin", json!(0)),
            ("start", json!(1440)),
            ("end", json!(1260)),
        ] {
            let mut j = s.encode();
            j["night"][key] = v;
            assert!(Settings::decode(&j).is_err());
        }
    }
    #[test]
    fn mode_preserves_fractional_refresh() {
        let m = Mode {
            width: 1920,
            height: 1080,
            refresh: 59940,
        };
        assert_eq!(m.argument(), "1920x1080@59.940");
        assert_eq!(m.rate(), "59.940 Hz");
    }
    #[test]
    fn panel_name_uses_actual_device_and_connector_only_as_fallback() {
        let mut v = fixture();
        let o = parse_outputs(&v);
        assert_eq!(o[0].label, "BOE Internal");
        v["eDP-1"]["make"] = json!("");
        v["eDP-1"]["model"] = json!("NE140QDM-NX8");
        assert_eq!(parse_outputs(&v)[0].label, "NE140QDM-NX8");
        v["eDP-1"]["model"] = json!("");
        assert_eq!(parse_outputs(&v)[0].label, "eDP-1");
    }
    #[test]
    fn hours_and_minutes_scroll_independently_and_wrap_without_carry() {
        let mut n = Night {
            start: 23 * 60 + 59,
            end: 7 * 60,
            ..Night::default()
        };
        n.adjust(true, ClockPart::Minute, 1).unwrap();
        assert_eq!(n.start, 23 * 60);
        n.adjust(true, ClockPart::Minute, -1).unwrap();
        assert_eq!(n.start, 23 * 60 + 59);
        n.adjust(true, ClockPart::Hour, 1).unwrap();
        assert_eq!(n.start, 59);
        n.adjust(true, ClockPart::Hour, -1).unwrap();
        assert_eq!(n.start, 23 * 60 + 59);
        n.adjust(false, ClockPart::Minute, -1).unwrap();
        assert_eq!(n.end, 7 * 60 + 59);
        assert_eq!(n.start, 23 * 60 + 59);
        let before = n.clone();
        assert!(n.adjust(true, ClockPart::Hour, -16).is_err());
        assert_eq!(n, before);
    }
    #[test]
    fn disabled_outputs_and_invalid_indices_are_not_listed() {
        let m = json!({"width":2560,"height":1600,"refresh_rate":165000});
        let v = json!({"eDP-1":{"logical":{"scale":1.5},"modes":[m],"current_mode":0},"HDMI-A-1":{"logical":null,"modes":[m],"current_mode":0},"DP-1":{"logical":{},"modes":[],"current_mode":9}});
        let o = parse_outputs(&v);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].current.refresh, 165000);
    }
    #[test]
    fn ddc_devices_are_matched_by_connector_not_list_order() {
        let map=ddc_displays("Display 1\n I2C bus: /dev/i2c-7\n DRM connector: card0-DP-3\nDisplay 2\n I2C bus: /dev/i2c-4\n DRM connector: card1-HDMI-A-1\n");
        assert_eq!(map.get("DP-3"), Some(&7));
        assert_eq!(map.get("HDMI-A-1"), Some(&4));
    }
}
