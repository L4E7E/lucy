//! NetworkManager frontend. Opening Wi-Fi requests discovery, never a connection
//! or profile mutation. Connection/settings changes follow explicit clicks.
//! nmcli --ask runs in a private PTY; secrets travel over its input, never argv/files.
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Device {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) state: String,
    pub(crate) connection: String,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) uuid: String,
    pub(crate) kind: String,
    pub(crate) active: bool,
    pub(crate) wifi: Option<WifiProfile>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WifiProfile {
    pub(crate) ssid: Vec<u8>,
    pub(crate) interface: Option<String>,
    pub(crate) bssid: Option<String>,
    pub(crate) enterprise: bool,
}
impl WifiProfile {
    pub(crate) fn matches(&self, ap: &Ap) -> bool {
        self.ssid == ap.ssid.as_bytes()
            && self
                .interface
                .as_ref()
                .is_none_or(|interface| interface.is_empty() || interface == &ap.device)
            && self
                .bssid
                .as_ref()
                .is_none_or(|bssid| bssid.is_empty() || bssid.eq_ignore_ascii_case(&ap.bssid))
            && self.enterprise == ap.security.contains("802.1X")
    }
}

pub(crate) fn wifi_profile(bus: &gio::DBusConnection, path: &str) -> Option<WifiProfile> {
    type Settings = HashMap<String, HashMap<String, glib::Variant>>;
    // GetSettings deliberately excludes secrets. Use the actual SSID rather
    // than assuming that a user-renamed connection ID is its network name.
    let reply = bus
        .call_sync(
            Some("org.freedesktop.NetworkManager"),
            path,
            "org.freedesktop.NetworkManager.Settings.Connection",
            "GetSettings",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1500,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let (settings,) = reply.get::<(Settings,)>()?;
    let wifi = settings.get("802-11-wireless")?;
    let bssid = wifi.get("bssid").and_then(|v| {
        v.get::<String>().or_else(|| {
            v.get::<Vec<u8>>().map(|bytes| {
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
        })
    });
    Some(WifiProfile {
        ssid: wifi.get("ssid")?.get::<Vec<u8>>()?,
        interface: settings
            .get("connection")
            .and_then(|s| s.get("interface-name"))
            .and_then(|v| v.get()),
        bssid,
        enterprise: settings
            .get("802-11-wireless-security")
            .and_then(|s| s.get("key-mgmt"))
            .and_then(|v| v.get::<String>())
            .is_some_and(|s| s.contains("eap") || s == "ieee8021x"),
    })
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ap {
    pub(crate) active: bool,
    pub(crate) bssid: String,
    pub(crate) ssid: String,
    pub(crate) signal: u8,
    pub(crate) security: String,
    pub(crate) device: String,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct State {
    pub(crate) wifi: bool,
    pub(crate) devices: Vec<Device>,
    pub(crate) profiles: Vec<Profile>,
    pub(crate) aps: Vec<Ap>,
    pub(crate) ips: HashMap<String, String>,
}

pub(crate) fn fields(line: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut field = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            field.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ':' {
            result.push(std::mem::take(&mut field));
        } else {
            field.push(ch);
        }
    }
    if escaped {
        field.push('\\');
    }
    result.push(field);
    result
}
pub(crate) fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}
pub(crate) fn auth_trace(message: &str) {
    if env::var_os("LUCY_NETWORK_TRACE").is_some() {
        eprintln!("lucy network: {message}");
    }
}
pub(crate) fn nm(args: &[&str]) -> Result<String, String> {
    let out = Command::new("nmcli")
        .args(["--colors", "no", "--escape", "yes", "--terse"])
        .args(args)
        .env("LC_ALL", "C")
        .env("NM_CLI_PAGER", "")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(clean(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
pub(crate) fn access_points() -> Result<Vec<Ap>, String> {
    let mut aps: Vec<_> = nm(&[
        "--fields",
        "IN-USE,BSSID,SSID,SIGNAL,SECURITY,DEVICE",
        "device",
        "wifi",
        "list",
        "--rescan",
        "no",
    ])?
    .lines()
    .map(fields)
    .filter(|f| f.len() == 6 && !f[2].is_empty())
    .map(|f| Ap {
        active: f[0].trim() == "*",
        bssid: f[1].clone(),
        ssid: f[2].clone(),
        signal: f[3].parse().unwrap_or(0),
        security: f[4].clone(),
        device: f[5].clone(),
    })
    .collect();
    aps.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.signal.cmp(&a.signal))
            .then(a.ssid.cmp(&b.ssid))
    });
    let mut seen = HashSet::new();
    aps.retain(|ap| seen.insert((ap.ssid.clone(), ap.security.clone(), ap.device.clone())));
    Ok(aps)
}

pub(crate) fn scan_access_points(cancel: &AtomicBool) -> Result<Vec<Ap>, String> {
    // Unlike `wifi rescan`, `wifi list --rescan yes` waits for fresh results.
    // Keep the existing list visible, and allow cancellation while the driver scans.
    let mut child = Command::new("nmcli")
        .args([
            "--colors", "no", "--wait", "15", "device", "wifi", "list", "--rescan", "yes",
        ])
        .env("LC_ALL", "C")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancel.load(Ordering::Acquire) {
                "검색을 취소했습니다."
            } else {
                "검색 시간이 초과됐습니다. 다시 시도하세요."
            }
            .into());
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(Duration::from_millis(30)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(clean(&String::from_utf8_lossy(&output.stderr)));
    }
    access_points()
}

pub(crate) fn state() -> Result<State, String> {
    let wifi = nm(&["--fields", "WIFI", "general"])?.trim() == "enabled";
    let devices = nm(&[
        "--fields",
        "DEVICE,TYPE,STATE,CONNECTION",
        "device",
        "status",
    ])?
    .lines()
    .map(fields)
    .filter(|f| f.len() == 4 && ["wifi", "ethernet"].contains(&f[1].as_str()))
    .map(|f| Device {
        name: f[0].clone(),
        kind: f[1].clone(),
        state: f[2].clone(),
        connection: f[3].clone(),
    })
    .collect();
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok();
    let profiles = nm(&[
        "--fields",
        "NAME,UUID,TYPE,DEVICE,DBUS-PATH",
        "connection",
        "show",
    ])?
    .lines()
    .map(fields)
    .filter(|f| {
        f.len() == 5
            && ["802-11-wireless", "802-3-ethernet", "vpn", "wireguard"].contains(&f[2].as_str())
    })
    .map(|f| Profile {
        name: f[0].clone(),
        uuid: f[1].clone(),
        kind: f[2].clone(),
        active: !f[3].is_empty() && f[3] != "--",
        wifi: if f[2] == "802-11-wireless" {
            bus.as_ref().and_then(|bus| wifi_profile(bus, &f[4]))
        } else {
            None
        },
    })
    .collect();
    let aps = if wifi { access_points()? } else { Vec::new() };
    let mut ips = HashMap::new();
    let mut device = String::new();
    for line in nm(&["--fields", "GENERAL.DEVICE,IP4.ADDRESS", "device", "show"])?.lines() {
        let f = fields(line);
        if f.len() != 2 {
            continue;
        }
        if f[0] == "GENERAL.DEVICE" {
            device = f[1].clone();
        } else if f[0].starts_with("IP4.ADDRESS") {
            ips.entry(device.clone()).or_insert(f[1].clone());
        }
    }
    Ok(State {
        wifi,
        devices,
        profiles,
        aps,
        ips,
    })
}

#[derive(Clone, Debug)]
pub(crate) enum Action {
    Radio(bool),
    Scan,
    Up(String),
    SavedWifi {
        uuid: String,
        device: String,
        bssid: String,
    },
    UpDevice(String),
    Down(String),
    Forget(String),
    Disconnect(String),
    Connect {
        target: String,
        device: String,
    },
}
impl Action {
    pub(crate) fn args(&self) -> Vec<String> {
        let args: Vec<&str> = match self {
            Self::Radio(on) => vec!["radio", "wifi", if *on { "on" } else { "off" }],
            Self::Scan => vec!["device", "wifi", "rescan"],
            Self::Up(uuid) => vec!["connection", "up", "uuid", uuid],
            Self::SavedWifi {
                uuid,
                device,
                bssid,
            } => vec![
                "connection",
                "up",
                "uuid",
                uuid,
                "ifname",
                device,
                "ap",
                bssid,
            ],
            Self::UpDevice(device) => vec!["device", "connect", device],
            Self::Down(uuid) => vec!["connection", "down", "uuid", uuid],
            Self::Forget(uuid) => vec!["connection", "delete", "uuid", uuid],
            Self::Disconnect(device) => vec!["device", "disconnect", device],
            Self::Connect { target, device } => {
                vec!["device", "wifi", "connect", target, "ifname", device]
            }
        };
        args.into_iter().map(str::to_owned).collect()
    }
}

/// Identifies only the Wi-Fi activation initiated by this popup.
pub(crate) struct WifiAttempt {
    pub(crate) device: Option<String>,
    pub(crate) uuid: Option<String>,
    pub(crate) ssid: Option<String>,
}
impl WifiAttempt {
    pub(crate) fn new(action: &Action, state: Option<&State>) -> Option<Self> {
        let state = state?;
        match action {
            Action::Connect { target, device } => Some(Self {
                device: Some(device.clone()),
                uuid: None,
                ssid: Some(
                    state
                        .aps
                        .iter()
                        .find(|ap| ap.bssid == *target)?
                        .ssid
                        .clone(),
                ),
            }),
            Action::Up(uuid) | Action::SavedWifi { uuid, .. }
                if state
                    .profiles
                    .iter()
                    .any(|p| p.uuid == *uuid && p.kind == "802-11-wireless") =>
            {
                Some(Self {
                    device: None,
                    uuid: Some(uuid.clone()),
                    ssid: None,
                })
            }
            _ => None,
        }
    }

    pub(crate) fn cancel(&self) {
        let Ok(devices) = nm(&["-g", "DEVICE,TYPE", "device", "status"]) else {
            return;
        };
        for line in devices.lines() {
            let parts = fields(line);
            if parts.len() != 2
                || parts[1] != "wifi"
                || self.device.as_ref().is_some_and(|d| d != &parts[0])
            {
                continue;
            }
            let Ok(info) = nm(&[
                "-g",
                "GENERAL.STATE,GENERAL.CON-PATH,GENERAL.CON-UUID",
                "device",
                "show",
                &parts[0],
            ]) else {
                continue;
            };
            let values: Vec<_> = info.lines().collect();
            if values.len() != 3 {
                continue;
            }
            let code = values[0]
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u32>().ok());
            // A completed connection, or another connection restored by the user,
            // must survive closing this popup.
            if !code.is_some_and(|c| (40..100).contains(&c)) {
                continue;
            }
            let matches = if let Some(uuid) = &self.uuid {
                values[2] == uuid
            } else if let Some(ssid) = &self.ssid {
                nm(&[
                    "-g",
                    "802-11-wireless.ssid",
                    "connection",
                    "show",
                    "uuid",
                    values[2],
                ])
                .is_ok_and(|current| {
                    fields(current.trim_end_matches(['\r', '\n'])).first() == Some(ssid)
                })
            } else {
                false
            };
            if matches && values[1].starts_with("/org/freedesktop/NetworkManager/ActiveConnection/")
            {
                let _ = nm(&["--wait", "3", "connection", "down", "apath", values[1]]);
            }
        }
    }
}
pub(crate) enum Event {
    State(State),
    AccessPoints(Vec<Ap>),
    Error(String),
    Prompt(String),
    Finished(Result<(), String>),
}
pub(crate) enum Request {
    Refresh,
    Quit,
}

pub(crate) fn prompt_from(buffer: &str) -> Option<String> {
    // Readline and nmcli may repaint the line even with colours disabled.
    // Strip CSI sequences, not just ESC (which would leave e.g. "[K").
    let mut plain = String::new();
    let mut chars = buffer.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next() == Some('[') {
                for code in chars.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
        } else {
            plain.push(ch);
        }
    }
    // A progress repaint can follow an unanswered secret prompt in the same
    // PTY read. Only recover typed secret prompts, never arbitrary status lines.
    for line in plain.rsplit(['\n', '\r']) {
        let line = line.trim();
        if (line.contains("(802-11-wireless-security.")
            || line.contains("(vpn.secrets.")
            || line.contains("(802-1x."))
            && line.contains("):")
        {
            // Readline may repaint a prefilled, masked password after the
            // colon. It is still a question, not an answered status line.
            let end = line.find("):").unwrap() + 2;
            return Some(clean(&line[..end]));
        }
    }
    let last = plain.rsplit(['\n', '\r']).next().unwrap_or("").trim();
    if last.ends_with(':') || (last.contains('?') && last.ends_with(']')) {
        Some(clean(last))
    } else {
        None
    }
}

pub(crate) struct PtyChild(Child);
impl std::ops::Deref for PtyChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for PtyChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl PtyChild {
    pub(crate) fn stop(&mut self) {
        // pre_exec creates this child's own session/process group, never the caller's.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            self.stop();
        }
    }
}

/// The child has a real tty because nmcli's secret-agent/readline needs one.
/// No production path invokes a shell. Password input is never logged or put in argv.
#[allow(deprecated)]
pub(crate) fn interactive(
    program: &str,
    args: &[String],
    events: &glib::Sender<Event>,
    replies: &mpsc::Receiver<Vec<u8>>,
    cancel: &AtomicBool,
    wifi: Option<&WifiAttempt>,
) -> Result<(), String> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 240,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: valid output pointers and winsize; default termios requested.
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
    unsafe {
        if libc::tcgetattr(slave_fd, term.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let term = term.assume_init();
        // nmcli's masked Readline redisplay suppresses the prompt if ECHO is
        // disabled before Readline initializes. Disable echo only when replying.
        if libc::tcsetattr(slave_fd, libc::TCSANOW, &term) != 0
            || libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK) < 0
            || libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::fcntl(slave_fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .env("TERM", "dumb")
        .env("NM_CLI_PAGER", "")
        .stdin(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stdout(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = PtyChild(command.spawn().map_err(|e| e.to_string())?);
    auth_trace("authentication child started");
    // Command owns the parent-side slave Stdio handles until dropped.
    drop(command);
    let mut active_time = Duration::ZERO;
    let mut last_tick = Instant::now();
    let mut buffer = String::new();
    let mut waiting = false;
    let mut error = String::new();
    let mut prompts = 0;
    loop {
        let now = Instant::now();
        if !waiting {
            active_time += now.duration_since(last_tick);
        }
        last_tick = now;
        if cancel.load(Ordering::Acquire) || active_time > Duration::from_secs(90) {
            if let Some(wifi) = wifi {
                wifi.cancel();
            }
            child.stop();
            return Err(if cancel.load(Ordering::Acquire) {
                "작업을 취소했습니다."
            } else {
                "연결 응답 시간이 초과되었습니다. 다시 시도하세요."
            }
            .into());
        }
        let mut bytes = [0u8; 4096];
        loop {
            match master.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes[..n]));
                    auth_trace(if buffer.contains("802-11-wireless-security.psk") {
                        "PTY output: Wi-Fi PSK prompt marker present"
                    } else {
                        "PTY output received (content not logged)"
                    });
                    if buffer.len() > 16384 {
                        buffer = buffer
                            .chars()
                            .rev()
                            .take(4096)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                    }
                    for line in buffer
                        .lines()
                        .filter(|line| line.trim_start().starts_with("Error:"))
                    {
                        error = clean(line);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            auth_trace(if status.success() {
                "child succeeded"
            } else {
                "child failed"
            });
            return if status.success() {
                Ok(())
            } else {
                Err(if error.is_empty() {
                    "연결 작업에 실패했습니다. 프로필과 인증 정보를 확인하세요.".into()
                } else {
                    error
                })
            };
        }
        if !waiting {
            if let Some(prompt) = prompt_from(&buffer) {
                auth_trace("authentication prompt detected and queued");
                prompts += 1;
                if prompts > 12 {
                    if let Some(wifi) = wifi {
                        wifi.cancel();
                    }
                    child.stop();
                    return Err("인증 요청이 반복되어 중단했습니다.".into());
                }
                if events.send(Event::Prompt(prompt)).is_err() {
                    if let Some(wifi) = wifi {
                        wifi.cancel();
                    }
                    child.stop();
                    return Err("창이 닫혔습니다.".into());
                }
                waiting = true;
                buffer.clear();
            }
        }
        if waiting {
            if let Ok(mut reply) = replies.try_recv() {
                // Readline has now initialized and displayed the prompt. Prevent
                // kernel echo before writing anything, and clear a rejected
                // password which nmcli may have prefilled on a retry.
                let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
                let echo_disabled = unsafe {
                    if libc::tcgetattr(master_fd, term.as_mut_ptr()) == 0 {
                        let mut term = term.assume_init();
                        term.c_lflag &= !(libc::ECHO | libc::ECHONL);
                        libc::tcsetattr(master_fd, libc::TCSANOW, &term) == 0
                    } else {
                        false
                    }
                };
                if !echo_disabled {
                    reply.fill(0);
                    if let Some(wifi) = wifi {
                        wifi.cancel();
                    }
                    child.stop();
                    return Err("암호 입력 보호를 설정하지 못했습니다.".into());
                }
                reply.insert(0, 0x15); // Ctrl-U: clear Readline's prefilled input.
                reply.push(b'\n');
                let result = master.write_all(&reply);
                auth_trace(if result.is_ok() {
                    "reply written to authentication PTY"
                } else {
                    "PTY reply write failed"
                });
                reply.fill(0);
                if let Err(e) = result {
                    if let Some(wifi) = wifi {
                        wifi.cancel();
                    }
                    child.stop();
                    return Err(e.to_string());
                }
                waiting = false;
                buffer.clear();
            }
        }
        // Only while a user-requested operation is running; the UI thread never blocks.
        let mut pollfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pollfd, 1, 40);
        }
    }
}

pub(crate) struct Backend {
    pub(crate) tx: mpsc::Sender<Request>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) monitor: Arc<Mutex<Option<Child>>>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
}
impl Backend {
    #[allow(deprecated)]
    pub(crate) fn new(events: glib::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_events = events.clone();
        let worker = thread::spawn(move || {
            while let Ok(request) = rx.recv() {
                if worker_stop.load(Ordering::Acquire) || matches!(request, Request::Quit) {
                    break;
                }
                if rx.try_iter().any(|r| matches!(r, Request::Quit)) {
                    break;
                }
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                match state() {
                    Ok(state) => {
                        let _ = worker_events.send(Event::State(state));
                    }
                    Err(e) => {
                        let _ = worker_events.send(Event::Error(e));
                    }
                }
            }
        });
        let monitor = Arc::new(Mutex::new(None::<Child>));
        let holder = monitor.clone();
        let watch_stop = stop.clone();
        let watch_tx = tx.clone();
        let watcher = thread::spawn(move || {
            let mut child = match Command::new("nmcli")
                .args(["--colors", "no", "monitor"])
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
                let mut slot = holder.lock().unwrap();
                if watch_stop.load(Ordering::Acquire) {
                    let _ = child.kill();
                }
                *slot = Some(child);
            }
            for line in BufReader::new(stdout).lines() {
                if watch_stop.load(Ordering::Acquire) || line.is_err() {
                    break;
                }
                let _ = watch_tx.send(Request::Refresh);
            }
            if let Some(mut child) = holder.lock().unwrap().take() {
                let _ = child.wait();
            }
        });
        let _ = tx.send(Request::Refresh);
        Self {
            tx,
            stop,
            monitor,
            handles: vec![worker, watcher],
        }
    }
    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Request::Quit);
        if let Some(child) = self.monitor.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
        for thread in self.handles.drain(..) {
            let _ = thread.join();
        }
    }
}
#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use glib::ControlFlow;
    use std::{cell::Cell, rc::Rc};
    #[test]
    fn escaped_fields() {
        assert_eq!(
            fields(r"*:AA\:BB\:CC:Café\: lab\\x:90:WPA2:wlan0"),
            ["*", "AA:BB:CC", "Café: lab\\x", "90", "WPA2", "wlan0"]
        );
    }
    #[test]
    fn prompt_detection() {
        assert_eq!(
            prompt_from("Password (vpn.secrets.password): "),
            Some("Password (vpn.secrets.password):".into())
        );
        assert!(prompt_from("connected\n").is_none());
        assert!(prompt_from("Error: failed\n").is_none());
        assert_eq!(
            prompt_from("\r\x1b[2KPassword (802-11-wireless-security.psk): \x1b[K"),
            Some("Password (802-11-wireless-security.psk):".into())
        );
        assert_eq!(
            prompt_from("Password (802-11-wireless-security.psk): \r\x1b[2K/ connecting"),
            Some("Password (802-11-wireless-security.psk):".into())
        );
        assert!(prompt_from("\r\x1b[2K/ connecting").is_none());
        assert_eq!(
            prompt_from("Password (802-11-wireless-security.psk): ***********************"),
            Some("Password (802-11-wireless-security.psk):".into())
        );
    }
    #[test]
    fn arguments_never_use_shell() {
        let a = Action::Connect {
            target: "a; $(touch /tmp/no)".into(),
            device: "wlan0".into(),
        };
        assert_eq!(
            a.args(),
            [
                "device",
                "wifi",
                "connect",
                "a; $(touch /tmp/no)",
                "ifname",
                "wlan0"
            ]
        );
        assert_eq!(
            Action::Up("uuid".into()).args(),
            ["connection", "up", "uuid", "uuid"]
        );
        assert_eq!(
            Action::Forget("specific-wifi-uuid".into()).args(),
            ["connection", "delete", "uuid", "specific-wifi-uuid"]
        );
        assert_eq!(
            Action::SavedWifi {
                uuid: "saved-uuid".into(),
                device: "wlan0".into(),
                bssid: "00:11:22:33:44:55".into()
            }
            .args(),
            [
                "connection",
                "up",
                "uuid",
                "saved-uuid",
                "ifname",
                "wlan0",
                "ap",
                "00:11:22:33:44:55"
            ]
        );
    }
    #[test]
    fn saved_wifi_matches_network_and_device_constraints() {
        let mut ap = Ap {
            active: false,
            bssid: "00:11:22:33:44:55".into(),
            ssid: "a:renamed-network".into(),
            signal: 90,
            security: "WPA2".into(),
            device: "wlan0".into(),
        };
        let mut wifi = WifiProfile {
            ssid: ap.ssid.as_bytes().to_vec(),
            interface: None,
            bssid: None,
            enterprise: false,
        };
        assert!(wifi.matches(&ap));
        wifi.interface = Some("wlan1".into());
        assert!(!wifi.matches(&ap));
        wifi.interface = None;
        wifi.bssid = Some("00:11:22:33:44:66".into());
        assert!(!wifi.matches(&ap));
        wifi.bssid = None;
        ap.security = "WPA2 802.1X".into();
        assert!(!wifi.matches(&ap));
        wifi.enterprise = true;
        assert!(wifi.matches(&ap));
    }
    #[test]
    fn clean_control_chars() {
        assert_eq!(clean("a\0b\nc"), "abc");
    }

    #[test]
    #[allow(deprecated)]
    fn pty_authentication_roundtrip_and_cancellation() {
        let context = glib::MainContext::new();
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let (reply_tx, reply_rx) = mpsc::channel();
        let prompts = Rc::new(Cell::new(0));
        let count = prompts.clone();
        let source = receiver.attach(Some(&context), move |event| {
            if let Event::Prompt(prompt) = event {
                count.set(count.get() + 1);
                assert!(prompt.contains("Password") || prompt.contains("Code"));
                reply_tx.send(b"fixture-only".to_vec()).unwrap();
            }
            ControlFlow::Continue
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || {
            interactive("/bin/sh", &[
            "-c".into(),
            "printf '\\r\\033[2KPassword (802-11-wireless-security.psk): \\033[K\\r/ connecting'; IFS= read -r a; printf '\\nCode: '; IFS= read -r b; [ \"$a\" = fixture-only ] && [ \"$b\" = fixture-only ]".into()
        ], &events, &reply_rx, &worker_cancel, None)
        });
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < Duration::from_secs(5) {
            context.iteration(false);
            thread::sleep(Duration::from_millis(5));
        }
        cancel.store(true, Ordering::Release);
        assert!(
            worker.join().unwrap().is_ok(),
            "PTY multi-step authentication failed"
        );
        assert_eq!(prompts.get(), 2);
        context.find_source_by_id(&source).unwrap().destroy();

        let (events, _receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let (_tx, replies) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let worker = thread::spawn(move || {
            interactive("/bin/sleep", &["10".into()], &events, &replies, &stop, None)
        });
        thread::sleep(Duration::from_millis(60));
        cancel.store(true, Ordering::Release);
        let start = Instant::now();
        assert!(worker.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
