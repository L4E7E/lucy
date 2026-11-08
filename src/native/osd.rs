//! Keyboard controls and measured OSD values. Command I/O never runs on the UI thread.
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::net::UnixDatagram},
    path::PathBuf,
    sync::mpsc,
    thread,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Volume,
    Microphone,
    Brightness,
    Keyboard,
    Profile,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Microphone => "microphone",
            Self::Brightness => "brightness",
            Self::Keyboard => "keyboard",
            Self::Profile => "profile",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "volume" => Self::Volume,
            "microphone" => Self::Microphone,
            "brightness" => Self::Brightness,
            "keyboard" => Self::Keyboard,
            "profile" => Self::Profile,
            _ => return None,
        })
    }
}
#[derive(Clone, Debug)]
pub struct State {
    pub kind: Kind,
    pub value: f64,
    pub muted: bool,
    pub profile: String,
    /// Exact hardware level; percentages remain internal gauge coordinates.
    pub step: Option<u32>,
}
impl State {
    pub fn profile(profile: String) -> Self {
        Self {
            kind: Kind::Profile,
            value: 0.0,
            muted: false,
            profile,
            step: None,
        }
    }
    pub fn label(&self) -> String {
        if self.kind == Kind::Keyboard {
            match self.step {
                Some(0) => "꺼짐".into(),
                Some(level) => format!("{level}단계"),
                None => "—".into(),
            }
        } else {
            format!("{:.0}%", self.value)
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub kind: Kind,
    pub operation: i32,
    pub show_only: bool,
    deduplicate: bool,
    hardware_level: Option<u32>,
}
impl Request {
    pub fn keyboard_refresh() -> Self {
        Self {
            kind: Kind::Keyboard,
            operation: 0,
            show_only: true,
            deduplicate: true,
            hardware_level: None,
        }
    }
    fn keyboard_hardware(level: u32) -> Self {
        Self {
            kind: Kind::Keyboard,
            operation: 0,
            show_only: true,
            deduplicate: false,
            hardware_level: Some(level),
        }
    }
}
pub fn parse(message: &str) -> Option<Request> {
    let mut parts = message.strip_prefix("@osd ")?.split_whitespace();
    let kind = Kind::parse(parts.next()?)?;
    let action = parts.next()?;
    let show_only = action == "show";
    let operation = match action {
        "up" => 1,
        "down" => -1,
        "mute" | "cycle" | "show" => 0,
        _ => return None,
    };
    if parts.next().is_some()
        || (!show_only && kind == Kind::Profile && operation != 0)
        || (!show_only && kind == Kind::Brightness && operation == 0)
    {
        return None;
    }
    Some(Request {
        kind,
        operation,
        show_only,
        deduplicate: false,
        hardware_level: None,
    })
}
fn keyboard_device() -> Option<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir("/sys/class/leds")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|s| s.to_string_lossy().ends_with("kbd_backlight"))
        })
        .collect();
    paths.sort();
    paths.into_iter().next()
}
fn keyboard_level(device: &std::path::Path) -> Option<(u32, u32)> {
    let level = std::fs::read_to_string(device.join("brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let max = std::fs::read_to_string(device.join("max_brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (max > 0).then_some((level, max))
}
fn keyboard_state(level: u32, max: u32) -> Option<State> {
    (max > 0 && level <= max).then(|| State {
        kind: Kind::Keyboard,
        value: f64::from(level) * 100.0 / f64::from(max),
        muted: level == 0,
        profile: String::new(),
        step: Some(level),
    })
}
fn hardware_state(level: u32, max: u32) -> Option<State> {
    keyboard_state(level.min(max), max)
}
fn read_hardware(file: &mut File) -> io::Result<u32> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0u8; 64];
    let n = match file.read(&mut bytes) {
        Ok(n) => n,
        // ASUS can report the attempted level -1 at the lower limit. The LED
        // class encodes this as ENODATA, but the actual LED is clamped to zero.
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => return Ok(0),
        Err(e) => return Err(e),
    };
    std::str::from_utf8(&bytes[..n])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| io::Error::other("invalid keyboard LED hardware level"))
}
/// Kernel sysfs notifications (POLLPRI), not inotify or an interval timer.
/// ASUS handles Fn+F2/F3 before input key events reach the compositor.
pub fn watch_keyboard_hardware(tx: mpsc::Sender<Option<Request>>) -> Option<glib::SourceId> {
    let mut file = File::open(keyboard_device()?.join("brightness_hw_changed")).ok()?;
    read_hardware(&mut file).ok()?;
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
            match read_hardware(&mut file) {
                Ok(level) => {
                    if tx.send(Some(Request::keyboard_hardware(level))).is_err() {
                        return glib::ControlFlow::Break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(_) => return glib::ControlFlow::Break,
            }
            glib::ControlFlow::Continue
        },
    ))
}
fn read(kind: Kind) -> Option<State> {
    let raw = match kind {
        Kind::Volume | Kind::Microphone => crate::bar_backend::output(
            "wpctl",
            &[
                "get-volume",
                if kind == Kind::Volume {
                    "@DEFAULT_AUDIO_SINK@"
                } else {
                    "@DEFAULT_AUDIO_SOURCE@"
                },
            ],
        )?,
        Kind::Brightness => {
            crate::bar_backend::output("brightnessctl", &["--class=backlight", "-m"])?
        }
        Kind::Keyboard => {
            let (level, max) = keyboard_level(&keyboard_device()?)?;
            return keyboard_state(level, max);
        }
        Kind::Profile => {
            return Some(State::profile(crate::bar_backend::output(
                "powerprofilesctl",
                &["get"],
            )?))
        }
    };
    let value = if kind == Kind::Brightness {
        raw.split(',').nth(3)?.trim_end_matches('%').parse().ok()?
    } else {
        raw.split_whitespace().nth(1)?.parse::<f64>().ok()? * 100.0
    };
    Some(State {
        kind,
        value,
        muted: raw.contains("MUTED"),
        profile: String::new(),
        step: None,
    })
}
fn execute(request: Request) -> Option<State> {
    if let Some(level) = request.hardware_level {
        let max = std::fs::read_to_string(keyboard_device()?.join("max_brightness"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        return hardware_state(level, max);
    }
    if request.show_only {
        return read(request.kind);
    }
    let (program, args): (&str, Vec<String>) = match request.kind {
        Kind::Volume | Kind::Microphone => {
            let device = if request.kind == Kind::Volume {
                "@DEFAULT_AUDIO_SINK@"
            } else {
                "@DEFAULT_AUDIO_SOURCE@"
            };
            if request.operation == 0 {
                (
                    "wpctl",
                    vec!["set-mute".into(), device.into(), "toggle".into()],
                )
            } else {
                (
                    "wpctl",
                    vec![
                        "set-volume".into(),
                        "-l".into(),
                        "1.0".into(),
                        device.into(),
                        format!(
                            "{}%{}",
                            request.operation.unsigned_abs() * 5,
                            if request.operation > 0 { "+" } else { "-" }
                        ),
                    ],
                )
            }
        }
        Kind::Brightness => (
            "brightnessctl",
            vec![
                "--class=backlight".into(),
                "--min-value=1".into(),
                "set".into(),
                format!(
                    "{}%{}",
                    request.operation.unsigned_abs() * 5,
                    if request.operation > 0 { "+" } else { "-" }
                ),
            ],
        ),
        Kind::Keyboard => {
            let device = keyboard_device()?;
            let name = device.file_name()?.to_str()?;
            let (level, max) = keyboard_level(&device)?;
            let value = if request.operation == 0 {
                if level > 0 {
                    0
                } else {
                    max
                }
            } else {
                (i64::from(level) + i64::from(request.operation)).clamp(0, i64::from(max)) as u32
            };
            (
                "brightnessctl",
                vec![
                    "--class=leds".into(),
                    format!("--device={name}"),
                    "--min-value=0".into(),
                    "set".into(),
                    value.to_string(),
                ],
            )
        }
        Kind::Profile => {
            let state = read(Kind::Profile)?;
            let mut common = crate::control_backend::State::default();
            crate::control_backend::refresh(&mut common, crate::control_backend::Domain::Power);
            let profile = super::model::next_profile(Some(&state.profile), &common.profiles)?;
            ("powerprofilesctl", vec!["set".into(), profile])
        }
    };
    crate::bar_backend::output(
        program,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    read(request.kind)
}
pub struct Backend {
    pub tx: mpsc::Sender<Option<Request>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Backend {
    pub fn start(events: glib::Sender<State>) -> Self {
        let (tx, rx) = mpsc::channel::<Option<Request>>();
        let worker = thread::Builder::new()
            .name("lucy-osd".into())
            .spawn(move || {
                let mut pending = None;
                let mut last_keyboard = read(Kind::Keyboard).map(|s| s.value);
                while let Some(mut request) = pending.take().or_else(|| rx.recv().ok()).flatten() {
                    if !request.show_only && request.operation != 0 {
                        while let Ok(next) = rx.try_recv() {
                            match next {
                                Some(r)
                                    if !r.show_only
                                        && r.kind == request.kind
                                        && r.operation.signum() == request.operation.signum() =>
                                {
                                    request.operation =
                                        request.operation.saturating_add(r.operation).clamp(-20, 20)
                                }
                                other => {
                                    pending = Some(other);
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(state) = execute(request) {
                        if state.kind == Kind::Keyboard {
                            let unchanged = last_keyboard == Some(state.value);
                            last_keyboard = Some(state.value);
                            if request.deduplicate && unchanged {
                                continue;
                            }
                        }
                        if events.send(state).is_err() {
                            break;
                        }
                    }
                }
            })
            .expect("OSD worker");
        Self {
            tx,
            worker: Some(worker),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.tx.send(None);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
pub fn cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let kind = args.first().ok_or("OSD 종류가 필요합니다")?;
    let operation = args
        .get(1)
        .map(String::as_str)
        .unwrap_or(if kind == "profile" { "cycle" } else { "mute" });
    let message = format!("@osd {kind} {operation}");
    let request = parse(&message).ok_or("지원하지 않는 OSD 동작입니다")?;
    let path = PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join("lucy-bar.sock");
    if UnixDatagram::unbound()?
        .send_to(message.as_bytes(), path)
        .is_err()
    {
        // Hardware keys still work if the resident shell is not running.
        execute(request).ok_or("장치 설정을 변경하지 못했습니다")?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_labels_use_discrete_levels_instead_of_percentages() {
        for (level, label) in [(0, "꺼짐"), (1, "1단계"), (2, "2단계"), (3, "3단계")] {
            let state = keyboard_state(level, 3).unwrap();
            assert_eq!(state.label(), label);
            assert_eq!(state.step, Some(level));
        }
        assert_eq!(hardware_state(4, 3).unwrap().label(), "3단계");
        // Do not hard-code this laptop's three levels for other hardware.
        assert_eq!(keyboard_state(5, 5).unwrap().label(), "5단계");
        let mut state = State::profile("balanced".into());
        state.kind = Kind::Volume;
        state.value = 42.0;
        assert_eq!(state.label(), "42%");
    }
    #[test]
    fn keyboard_levels_use_real_discrete_hardware_steps() {
        assert_eq!(keyboard_state(0, 3).unwrap().value, 0.0);
        assert!((keyboard_state(1, 3).unwrap().value - 100.0 / 3.0).abs() < 0.001);
        assert!((keyboard_state(2, 3).unwrap().value - 200.0 / 3.0).abs() < 0.001);
        assert_eq!(keyboard_state(3, 3).unwrap().value, 100.0);
        assert!(keyboard_state(4, 3).is_none());
        assert!(keyboard_state(0, 0).is_none());
        assert_eq!(parse("@osd keyboard up").unwrap().kind, Kind::Keyboard);
        assert!(parse("@osd keyboard show").unwrap().show_only);
        assert!(Request::keyboard_refresh().deduplicate);
        assert!(!parse("@osd keyboard show").unwrap().deduplicate);
    }
    #[test]
    fn asus_hardware_notifications_are_clamped_without_using_the_cached_led_level() {
        assert_eq!(hardware_state(4, 3).unwrap().value, 100.0);
        assert_eq!(hardware_state(0, 3).unwrap().value, 0.0);
        assert!((hardware_state(2, 3).unwrap().value - 200.0 / 3.0).abs() < 0.001);
        let request = Request::keyboard_hardware(2);
        assert!(request.show_only);
        assert!(!request.deduplicate);
        assert_eq!(request.hardware_level, Some(2));
        assert!(hardware_state(1, 0).is_none());
    }
    #[test]
    fn keyboard_requests_are_typed_and_cannot_inject_commands() {
        assert_eq!(parse("@osd volume up").unwrap().operation, 1);
        assert_eq!(
            parse("@osd brightness down").unwrap().kind,
            Kind::Brightness
        );
        for invalid in [
            "@osd volume up;reboot",
            "@osd brightness mute",
            "@osd profile up",
            "@osd volume up extra",
        ] {
            assert!(parse(invalid).is_none());
        }
    }
}
