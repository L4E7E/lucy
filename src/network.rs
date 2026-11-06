//! NetworkManager frontend. Opening is read-only; mutations follow explicit clicks.
//! nmcli --ask runs in a private PTY; secrets travel over its input, never argv/files.
use super::{focused_monitor, label, layer};
use glib::{ControlFlow, Propagation};
use gtk::{gdk, gio, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    env,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, net::UnixDatagram, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
struct Device {
    name: String,
    kind: String,
    state: String,
    connection: String,
}
#[derive(Clone, Debug, PartialEq)]
struct Profile {
    name: String,
    uuid: String,
    kind: String,
    active: bool,
    wifi: Option<WifiProfile>,
}
#[derive(Clone, Debug, PartialEq)]
struct WifiProfile {
    ssid: Vec<u8>,
    interface: Option<String>,
    bssid: Option<String>,
    enterprise: bool,
}
impl WifiProfile {
    fn matches(&self, ap: &Ap) -> bool {
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

fn wifi_profile(bus: &gio::DBusConnection, path: &str) -> Option<WifiProfile> {
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
struct Ap {
    active: bool,
    bssid: String,
    ssid: String,
    signal: u8,
    security: String,
    device: String,
}
#[derive(Clone, Debug, PartialEq)]
struct State {
    wifi: bool,
    devices: Vec<Device>,
    profiles: Vec<Profile>,
    aps: Vec<Ap>,
    ips: HashMap<String, String>,
}

fn fields(line: &str) -> Vec<String> {
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
fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}
fn auth_trace(message: &str) {
    if env::var_os("LUCY_NETWORK_TRACE").is_some() {
        eprintln!("lucy network: {message}");
    }
}
fn nm(args: &[&str]) -> Result<String, String> {
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
fn access_points() -> Result<Vec<Ap>, String> {
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

fn scan_access_points(cancel: &AtomicBool) -> Result<Vec<Ap>, String> {
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

fn state() -> Result<State, String> {
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
enum Action {
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
    fn args(&self) -> Vec<String> {
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
struct WifiAttempt {
    device: Option<String>,
    uuid: Option<String>,
    ssid: Option<String>,
}
impl WifiAttempt {
    fn new(action: &Action, state: Option<&State>) -> Option<Self> {
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

    fn cancel(&self) {
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
enum Event {
    State(State),
    AccessPoints(Vec<Ap>),
    Error(String),
    Prompt(String),
    Finished(Result<(), String>),
}
enum Request {
    Refresh,
    Quit,
}

fn prompt_from(buffer: &str) -> Option<String> {
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

struct PtyChild(Child);
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
    fn stop(&mut self) {
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
fn interactive(
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

struct Backend {
    tx: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    monitor: Arc<Mutex<Option<Child>>>,
    handles: Vec<thread::JoinHandle<()>>,
}
impl Backend {
    #[allow(deprecated)]
    fn new(events: glib::Sender<Event>) -> Self {
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
    fn shutdown(&self) {
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

struct Ui {
    win: gtk::Window,
    dismiss: RefCell<Option<crate::dismiss::Backdrop>>,
    backdrops: Vec<gtk::Window>,
    stack: gtk::Stack,
    wifi: gtk::Box,
    ethernet: gtk::Box,
    vpn: gtk::Box,
    state: RefCell<Option<State>>,
    busy: Cell<bool>,
    scanning: Cell<bool>,
    spinner: gtk::Spinner,
    status: gtk::Label,
    cancel_button: gtk::Button,
    auth: gtk::Box,
    prompt: gtk::Label,
    secret: gtk::Entry,
    submit: gtk::Button,
    cancelled: Arc<AtomicBool>,
    closed: Cell<bool>,
    replies: RefCell<Option<mpsc::Sender<Vec<u8>>>>,
    action_thread: RefCell<Option<thread::JoinHandle<()>>>,
    tx: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    monitor: Arc<Mutex<Option<Child>>>,
    events: glib::Sender<Event>,
    auth_home: gtk::Box,
    auth_anchor: RefCell<Option<String>>,
    pending_wifi: RefCell<Option<Action>>,
    prefill: RefCell<Option<Vec<u8>>>,
    wifi_reply_sent: Cell<bool>,
    pending_delete: RefCell<Option<String>>,
    saved_expanded: Cell<bool>,
    tabs: Vec<(&'static str, gtk::ToggleButton)>,
    headless: bool,
    draft_selection: Cell<Option<(i32, i32)>>,
}
impl Ui {
    #[allow(deprecated)]
    fn new(backend: &Backend, events: glib::Sender<Event>, vpn_first: bool) -> Rc<Self> {
        Self::new_with_visibility(
            backend,
            events,
            vpn_first,
            env::args().any(|arg| arg == "--self-test-network-scan-headless"),
        )
    }

    #[allow(deprecated)]
    fn new_with_visibility(
        backend: &Backend,
        events: glib::Sender<Event>,
        vpn_first: bool,
        headless_scan: bool,
    ) -> Rc<Self> {
        let display = gdk::Display::default().unwrap();
        let monitor = focused_monitor(&display);
        let css = gtk::CssProvider::new();
        css.load_from_data(include_bytes!("style.css"))
            .expect("valid CSS");
        gtk::StyleContext::add_provider_for_screen(&gdk::Screen::default().unwrap(), &css, 800);
        let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let dismiss = if headless_scan {
            None
        } else {
            crate::dismiss::Backdrop::new("lucy-network-dismiss", clicked).ok()
        };
        let backdrops: Vec<_> = (0..if headless_scan {
            0
        } else {
            display.n_monitors()
        })
            .filter_map(|i| display.monitor(i))
            .map(|m| {
                let w = layer::window("audio-dismiss", "lucy-network-dismiss", &m);
                for edge in 0..4 {
                    layer::anchor(&w, edge, 0);
                }
                w.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
                w
            })
            .collect();
        let win = layer::window("audio-popup", "lucy-network-popup", &monitor);
        layer::anchor(&win, 2, 54);
        layer::anchor(&win, 1, 16);
        layer::keyboard(&win);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 14);
        card.style_context().add_class("audio-card");
        card.set_size_request(460, -1);
        win.add(&card);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.pack_start(&label("네트워크", Some("title")), true, true, 0);
        let close = gtk::Button::with_label("닫기  ×");
        header.pack_end(&close, false, false, 0);
        card.pack_start(&header, false, false, 0);
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::None);
        let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        navigation.set_homogeneous(true);
        navigation.style_context().add_class("network-tabs");
        let mut tabs = Vec::new();
        for (id, title) in [("wifi", "Wi-Fi"), ("ethernet", "이더넷"), ("vpn", "VPN")] {
            let button = gtk::ToggleButton::with_label(title);
            if let Some(text) = button
                .child()
                .and_then(|child| child.downcast::<gtk::Label>().ok())
            {
                text.set_xalign(0.5);
            }
            button.set_hexpand(true);
            navigation.pack_start(&button, true, true, 0);
            tabs.push((id, button));
        }
        card.pack_start(&navigation, false, false, 0);
        let mut boxes = Vec::new();
        for (id, title) in [("wifi", "Wi-Fi"), ("ethernet", "이더넷"), ("vpn", "VPN")] {
            let scroll =
                gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroll.set_overlay_scrolling(false);
            let height = (monitor.geometry().height() - 260).clamp(160, 440);
            scroll.set_min_content_height(height);
            scroll.set_max_content_height(height);
            let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
            content.set_margin_end(10);
            scroll.add(&content);
            stack.add_titled(&scroll, id, title);
            boxes.push(content);
        }
        card.pack_start(&stack, true, true, 0);
        let auth = gtk::Box::new(gtk::Orientation::Vertical, 8);
        auth.set_no_show_all(true);
        let prompt = label("", Some("section"));
        auth.pack_start(&prompt, false, false, 0);
        let input = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let secret = gtk::Entry::new();
        secret.set_max_length(4096);
        secret.set_visibility(false);
        secret.set_caps_lock_warning(false);
        secret.set_input_purpose(gtk::InputPurpose::Password);
        secret.set_icon_from_icon_name(
            gtk::EntryIconPosition::Secondary,
            Some("view-reveal-symbolic"),
        );
        secret.set_icon_tooltip_text(gtk::EntryIconPosition::Secondary, Some("비밀번호 표시"));
        secret.connect_icon_release(|entry, position, _| {
            if position == gtk::EntryIconPosition::Secondary {
                entry.set_visibility(!gtk::prelude::EntryExt::is_visible(entry));
            }
        });
        secret.connect_visibility_notify(|entry| {
            let visible = gtk::prelude::EntryExt::is_visible(entry);
            entry.set_icon_from_icon_name(
                gtk::EntryIconPosition::Secondary,
                if entry.input_purpose() == gtk::InputPurpose::Password {
                    Some(if visible {
                        "view-conceal-symbolic"
                    } else {
                        "view-reveal-symbolic"
                    })
                } else {
                    None
                },
            );
            entry.set_icon_tooltip_text(
                gtk::EntryIconPosition::Secondary,
                Some(if visible {
                    "비밀번호 숨기기"
                } else {
                    "비밀번호 표시"
                }),
            );
        });
        input.pack_start(&secret, true, true, 0);
        let submit = gtk::Button::with_label("확인");
        input.pack_end(&submit, false, false, 0);
        auth.pack_start(&input, false, false, 0);
        let auth_home = gtk::Box::new(gtk::Orientation::Vertical, 0);
        auth_home.pack_start(&auth, false, false, 0);
        card.pack_start(&auth_home, false, false, 0);
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        spinner.set_no_show_all(true);
        spinner.set_valign(gtk::Align::Center);
        footer.pack_start(&spinner, false, false, 0);
        let status = label("", Some("subtle"));
        status.set_line_wrap(true);
        status.set_max_width_chars(38);
        status.set_no_show_all(true);
        footer.pack_start(&status, true, true, 0);
        let cancel_button = gtk::Button::with_label("취소");
        cancel_button.set_no_show_all(true);
        footer.pack_end(&cancel_button, false, false, 0);
        card.pack_start(&footer, false, false, 0);
        let ui = Rc::new(Self {
            win,
            dismiss: RefCell::new(dismiss),
            backdrops,
            stack,
            wifi: boxes.remove(0),
            ethernet: boxes.remove(0),
            vpn: boxes.remove(0),
            state: RefCell::new(None),
            busy: Cell::new(false),
            scanning: Cell::new(false),
            spinner,
            status,
            cancel_button,
            auth,
            prompt,
            secret,
            submit,
            cancelled: Arc::new(AtomicBool::new(false)),
            closed: Cell::new(false),
            replies: RefCell::new(None),
            action_thread: RefCell::new(None),
            tx: backend.tx.clone(),
            stop: backend.stop.clone(),
            monitor: backend.monitor.clone(),
            events,
            auth_home,
            auth_anchor: RefCell::new(None),
            pending_wifi: RefCell::new(None),
            prefill: RefCell::new(None),
            wifi_reply_sent: Cell::new(false),
            pending_delete: RefCell::new(None),
            saved_expanded: Cell::new(false),
            tabs,
            headless: headless_scan,
            draft_selection: Cell::new(None),
        });
        ui.spinner.show();
        ui.spinner.start();
        ui.note("네트워크 불러오는 중…");
        let weak = Rc::downgrade(&ui);
        clicks.attach(None, move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
            ControlFlow::Break
        });
        for (name, button) in &ui.tabs {
            let weak = Rc::downgrade(&ui);
            let name = *name;
            button.connect_toggled(move |button| {
                if let Some(ui) = weak.upgrade() {
                    if button.is_active() {
                        ui.stack.set_visible_child_name(name);
                    } else if ui.stack.visible_child_name().as_deref() == Some(name) {
                        button.set_active(true);
                    }
                }
            });
        }
        let weak = Rc::downgrade(&ui);
        ui.stack.connect_visible_child_name_notify(move |stack| {
            if let Some(ui) = weak.upgrade() {
                let selected = stack.visible_child_name();
                for (name, button) in &ui.tabs {
                    button.set_active(selected.as_deref() == Some(*name));
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        close.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.win.connect_delete_event(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
            Propagation::Stop
        });
        let weak = Rc::downgrade(&ui);
        ui.win.connect_key_press_event(move |_, e| {
            if e.keyval() == gdk::keys::constants::Escape {
                if let Some(ui) = weak.upgrade() {
                    ui.hide();
                }
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.cancel_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.cancelled.store(true, Ordering::Release);
                ui.auth.hide();
                ui.secret.set_text("");
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.submit.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.answer();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.secret.connect_activate(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.answer();
            }
        });
        for backdrop in &ui.backdrops {
            let weak = Rc::downgrade(&ui);
            backdrop.connect_button_press_event(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.hide();
                }
                Propagation::Stop
            });
            if ui.dismiss.borrow().is_none() {
                backdrop.show_all();
            }
        }
        crate::panel::prepare(&card);
        if headless_scan {
            if let Some(content) = ui.win.child() {
                content.show_all();
            }
        } else {
            ui.win.show_all();
        }
        ui.stack
            .set_visible_child_name(if vpn_first { "vpn" } else { "wifi" });
        for (name, button) in &ui.tabs {
            button.set_active(ui.stack.visible_child_name().as_deref() == Some(*name));
        }
        ui
    }
    fn clear(box_: &gtk::Box) {
        for child in box_.children() {
            box_.remove(&child);
        }
    }
    fn note(&self, message: &str) {
        self.status.set_text(message);
        self.status.show();
    }
    fn row(
        self: &Rc<Self>,
        container: &gtk::Box,
        title: &str,
        subtitle: &str,
        button: &str,
        action: Action,
        enabled: bool,
    ) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.style_context().add_class("network-row");
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let name = label(&clean(title), None);
        name.set_max_width_chars(32);
        name.set_tooltip_text(Some(&clean(title)));
        texts.pack_start(&name, false, false, 0);
        let detail = label(&clean(subtitle), Some("subtle"));
        detail.set_max_width_chars(38);
        detail.set_tooltip_text(Some(&clean(subtitle)));
        texts.pack_start(&detail, false, false, 0);
        row.pack_start(&texts, true, true, 0);
        let control = gtk::Button::with_label(button);
        control.set_sensitive(enabled && !self.busy.get());
        row.pack_end(&control, false, false, 0);
        let weak = Rc::downgrade(self);
        control.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.pending_wifi.borrow_mut().take();
                let anchor = if let Action::Up(uuid) = &action {
                    ui.state.borrow().as_ref().and_then(|s| {
                        s.profiles
                            .iter()
                            .find(|p| p.uuid == *uuid && p.kind == "802-11-wireless")
                            .map(|_| format!("profile:{uuid}"))
                    })
                } else {
                    None
                };
                *ui.auth_anchor.borrow_mut() = anchor;
                ui.start(action.clone());
            }
        });
        container.pack_start(&row, false, false, 0);
        row
    }
    fn move_auth(&self, destination: &gtk::Box) {
        if let Some(parent) = self
            .auth
            .parent()
            .and_then(|p| p.downcast::<gtk::Container>().ok())
        {
            parent.remove(&self.auth);
        }
        destination.pack_start(&self.auth, false, false, 0);
    }
    fn wipe_prefill(&self) {
        if let Some(mut bytes) = self.prefill.borrow_mut().take() {
            bytes.fill(0);
        }
    }
    fn prepare_wifi(self: &Rc<Self>, ap: &Ap) {
        if self.busy.get() || ap.active {
            return;
        }
        let saved = self.state.borrow().as_ref().and_then(|state| {
            state
                .profiles
                .iter()
                .filter(|profile| profile.wifi.as_ref().is_some_and(|wifi| wifi.matches(ap)))
                .max_by_key(|profile| profile.active)
                .map(|profile| profile.uuid.clone())
        });
        if let Some(uuid) = saved {
            *self.auth_anchor.borrow_mut() = Some(format!("ap:{}", ap.bssid));
            self.start(Action::SavedWifi {
                uuid,
                device: ap.device.clone(),
                bssid: ap.bssid.clone(),
            });
            return;
        }
        let action = Action::Connect {
            target: ap.bssid.clone(),
            device: ap.device.clone(),
        };
        if ap.security.is_empty() || ap.security == "--" {
            self.pending_wifi.borrow_mut().take();
            self.auth_anchor.borrow_mut().take();
            self.start(action);
            return;
        }
        self.wipe_prefill();
        *self.auth_anchor.borrow_mut() = Some(format!("ap:{}", ap.bssid));
        *self.pending_wifi.borrow_mut() = Some(action);
        self.pending_delete.borrow_mut().take();
        self.secret.set_text("");
        self.secret.set_input_purpose(gtk::InputPurpose::Password);
        self.secret.set_visibility(false);
        self.secret.set_placeholder_text(Some("Wi-Fi 비밀번호"));
        self.prompt.set_text("비밀번호");
        self.submit.set_label("연결");
        self.submit.set_sensitive(true);
        self.rerender();
        self.show_auth();
    }
    fn ap_row(self: &Rc<Self>, ap: &Ap, detail: &str) {
        let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.style_context().add_class("network-row");
        let name_button = gtk::Button::new();
        name_button.style_context().add_class("wifi-name");
        name_button.set_sensitive(!self.busy.get());
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let title = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if !ap.security.is_empty() && ap.security != "--" {
            let lock =
                gtk::Image::from_icon_name(Some("changes-prevent-symbolic"), gtk::IconSize::Menu);
            lock.set_pixel_size(14);
            lock.style_context().add_class("wifi-lock");
            title.pack_start(&lock, false, false, 0);
        }
        let name = label(&clean(&ap.ssid), None);
        name.set_max_width_chars(30);
        title.pack_start(&name, true, true, 0);
        texts.pack_start(&title, false, false, 0);
        texts.pack_start(&label(detail, Some("subtle")), false, false, 0);
        name_button.add(&texts);
        row.pack_start(&name_button, true, true, 0);
        let control = gtk::Button::with_label(if ap.active { "해제" } else { "연결" });
        control.set_sensitive(!self.busy.get());
        row.pack_end(&control, false, false, 0);
        let weak = Rc::downgrade(self);
        let target = ap.clone();
        name_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.prepare_wifi(&target);
            }
        });
        let weak = Rc::downgrade(self);
        let target = ap.clone();
        control.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if target.active {
                    ui.pending_wifi.borrow_mut().take();
                    ui.auth_anchor.borrow_mut().take();
                    ui.start(Action::Disconnect(target.device.clone()));
                } else {
                    ui.prepare_wifi(&target);
                }
            }
        });
        group.pack_start(&row, false, false, 0);
        let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        group.pack_start(&slot, false, false, 0);
        if self.auth_anchor.borrow().as_deref() == Some(format!("ap:{}", ap.bssid).as_str()) {
            self.move_auth(&slot);
        }
        self.wifi.pack_start(&group, false, false, 0);
    }
    fn render(self: &Rc<Self>, state: State) {
        *self.state.borrow_mut() = Some(state.clone());
        self.move_auth(&self.auth_home);
        Self::clear(&self.wifi);
        Self::clear(&self.ethernet);
        Self::clear(&self.vpn);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.pack_start(&label("Wi-Fi", Some("section")), true, true, 0);
        let scan = gtk::Button::with_label(if self.scanning.get() {
            "검색 중…"
        } else {
            "다시 검색"
        });
        scan.set_sensitive(state.wifi && !self.busy.get());
        let weak = Rc::downgrade(self);
        scan.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.start(Action::Scan);
            }
        });
        header.pack_end(&scan, false, false, 0);
        let toggle = gtk::Switch::new();
        toggle.set_active(state.wifi);
        toggle.set_sensitive(!self.busy.get());
        toggle.set_valign(gtk::Align::Center);
        let weak = Rc::downgrade(self);
        toggle.connect_state_set(move |_, active| {
            if let Some(ui) = weak.upgrade() {
                ui.start(Action::Radio(active));
            }
            Propagation::Stop
        });
        header.pack_end(&toggle, false, false, 0);
        self.wifi.pack_start(&header, false, false, 0);
        for dev in state.devices.iter().filter(|d| d.kind == "wifi") {
            let text = format!(
                "{}  ·  {}{}",
                dev.name,
                if dev.state == "connected" {
                    "연결됨"
                } else {
                    &dev.state
                },
                state
                    .ips
                    .get(&dev.name)
                    .map(|ip| format!("  ·  {ip}"))
                    .unwrap_or_default()
            );
            self.wifi
                .pack_start(&label(&text, Some("subtle")), false, false, 0);
        }
        if !state.wifi {
            self.wifi
                .pack_start(&label("Wi-Fi 꺼짐", Some("subtle")), false, false, 0);
        } else if state.aps.is_empty() {
            self.wifi.pack_start(
                &label(
                    if self.scanning.get() {
                        "주변 Wi-Fi를 찾고 있습니다…"
                    } else {
                        "검색된 네트워크 없음"
                    },
                    Some("subtle"),
                ),
                false,
                false,
                0,
            );
        }
        for ap in &state.aps {
            let security = if ap.security.is_empty() || ap.security == "--" {
                "공개"
            } else {
                &ap.security
            };
            let detail = format!(
                "{}%  ·  {}{}",
                ap.signal,
                security,
                if ap.active { "  ·  연결됨" } else { "" }
            );
            self.ap_row(ap, &detail);
        }
        let saved = gtk::Expander::new(Some("저장된 Wi-Fi"));
        saved.set_expanded(self.saved_expanded.get());
        let weak = Rc::downgrade(self);
        saved.connect_expanded_notify(move |expander| {
            if let Some(ui) = weak.upgrade() {
                ui.saved_expanded.set(expander.is_expanded());
            }
        });
        let saved_rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for profile in state
            .profiles
            .iter()
            .filter(|p| p.kind == "802-11-wireless")
        {
            let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
            let row = self.row(
                &group,
                &profile.name,
                if profile.active {
                    "연결됨"
                } else {
                    "저장된 프로필"
                },
                if profile.active { "해제" } else { "연결" },
                if profile.active {
                    Action::Down(profile.uuid.clone())
                } else {
                    Action::Up(profile.uuid.clone())
                },
                state.wifi,
            );
            let delete = gtk::Button::with_label("삭제");
            delete.set_sensitive(!profile.active && !self.busy.get());
            if profile.active {
                delete.set_tooltip_text(Some("연결을 해제한 뒤 삭제할 수 있습니다."));
            }
            row.pack_end(&delete, false, false, 0);
            let weak = Rc::downgrade(self);
            let uuid = profile.uuid.clone();
            delete.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.saved_expanded.set(true);
                    *ui.pending_delete.borrow_mut() = Some(uuid.clone());
                    ui.rerender();
                }
            });
            let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
            group.pack_start(&slot, false, false, 0);
            if self.auth_anchor.borrow().as_deref()
                == Some(format!("profile:{}", profile.uuid).as_str())
            {
                self.move_auth(&slot);
            }
            if self.pending_delete.borrow().as_deref() == Some(profile.uuid.as_str()) {
                Self::clear(&row);
                let confirmation = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let description = gtk::Box::new(gtk::Orientation::Vertical, 3);
                let name = label(&profile.name, None);
                name.set_max_width_chars(20);
                description.pack_start(&name, false, false, 0);
                description.pack_start(
                    &label("저장된 암호도 삭제됩니다.", Some("subtle")),
                    false,
                    false,
                    0,
                );
                confirmation.pack_start(&description, true, true, 0);
                let yes = gtk::Button::with_label("삭제 확인");
                yes.set_sensitive(!profile.active && !self.busy.get());
                let weak = Rc::downgrade(self);
                let uuid = profile.uuid.clone();
                yes.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.pending_delete.borrow_mut().take();
                        ui.start(Action::Forget(uuid.clone()));
                    }
                });
                confirmation.pack_end(&yes, false, false, 0);
                let no = gtk::Button::with_label("취소");
                let weak = Rc::downgrade(self);
                no.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ui.pending_delete.borrow_mut().take();
                        ui.rerender();
                    }
                });
                confirmation.pack_end(&no, false, false, 0);
                row.pack_start(&confirmation, true, true, 0);
            }
            saved_rows.pack_start(&group, false, false, 0);
        }
        saved.add(&saved_rows);
        self.wifi.pack_start(&saved, false, false, 0);
        let ethernet: Vec<_> = state
            .devices
            .iter()
            .filter(|d| d.kind == "ethernet")
            .collect();
        if ethernet.is_empty() {
            self.ethernet.pack_start(
                &label("이더넷 장치가 연결되어 있지 않습니다.", Some("subtle")),
                false,
                false,
                0,
            );
        }
        for dev in &ethernet {
            let detail = format!(
                "{}  ·  {}",
                dev.connection,
                state.ips.get(&dev.name).unwrap_or(&dev.state)
            );
            self.row(
                &self.ethernet,
                &dev.name,
                &detail,
                if dev.state == "connected" {
                    "해제"
                } else {
                    "연결"
                },
                if dev.state == "connected" {
                    Action::Disconnect(dev.name.clone())
                } else {
                    Action::UpDevice(dev.name.clone())
                },
                true,
            );
        }
        self.ethernet.pack_start(
            &label("저장된 유선 프로필", Some("section")),
            false,
            false,
            0,
        );
        for profile in state.profiles.iter().filter(|p| p.kind == "802-3-ethernet") {
            self.row(
                &self.ethernet,
                &profile.name,
                if profile.active {
                    "연결됨"
                } else {
                    "저장된 프로필"
                },
                if profile.active { "해제" } else { "연결" },
                if profile.active {
                    Action::Down(profile.uuid.clone())
                } else {
                    Action::Up(profile.uuid.clone())
                },
                !ethernet.is_empty(),
            );
        }
        let vpn_profiles: Vec<_> = state
            .profiles
            .iter()
            .filter(|p| p.kind == "vpn" || p.kind == "wireguard")
            .collect();
        if vpn_profiles.is_empty() {
            self.vpn
                .pack_start(&label("저장된 VPN 없음", Some("subtle")), false, false, 0);
        }
        for profile in vpn_profiles {
            self.row(
                &self.vpn,
                &profile.name,
                if profile.active {
                    "연결됨"
                } else {
                    &profile.kind
                },
                if profile.active { "해제" } else { "연결" },
                if profile.active {
                    Action::Down(profile.uuid.clone())
                } else {
                    Action::Up(profile.uuid.clone())
                },
                true,
            );
        }
        self.wifi.show_all();
        self.ethernet.show_all();
        self.vpn.show_all();
    }
    fn rerender(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        if let Some(state) = state {
            self.render(state);
        }
    }
    fn start(self: &Rc<Self>, action: Action) {
        if let Action::Forget(uuid) = &action {
            let allowed = self.state.borrow().as_ref().is_some_and(|state| {
                state.profiles.iter().any(|profile| {
                    profile.uuid == *uuid && profile.kind == "802-11-wireless" && !profile.active
                })
            });
            if !allowed {
                self.note("연결되지 않은 저장 Wi-Fi만 삭제할 수 있습니다.");
                return;
            }
        }
        if self.closed.get() || self.busy.replace(true) {
            return;
        }
        self.pending_wifi.borrow_mut().take();
        self.wipe_prefill();
        self.wifi_reply_sent.set(false);
        if let Some(handle) = self.action_thread.borrow_mut().take() {
            let _ = handle.join();
        }
        self.cancelled.store(false, Ordering::Release);
        self.scanning.set(matches!(action, Action::Scan));
        self.auth.hide();
        self.secret.set_text("");
        self.spinner.show();
        self.spinner.start();
        self.note(if self.scanning.get() {
            "주변 Wi-Fi 검색 중…"
        } else {
            "처리 중…"
        });
        self.cancel_button.show();
        self.rerender();
        let (reply_tx, reply_rx) = mpsc::channel();
        *self.replies.borrow_mut() = Some(reply_tx);
        let events = self.events.clone();
        let cancel = self.cancelled.clone();
        let tx = self.tx.clone();
        let wifi = WifiAttempt::new(&action, self.state.borrow().as_ref());
        if matches!(action, Action::Radio(true)) {
            for delay in [2, 5] {
                let tx = self.tx.clone();
                glib::timeout_add_local_once(Duration::from_secs(delay), move || {
                    let _ = tx.send(Request::Refresh);
                });
            }
        }
        *self.action_thread.borrow_mut() = Some(thread::spawn(move || {
            if matches!(action, Action::Scan) {
                let result = scan_access_points(&cancel).map(|aps| {
                    let _ = events.send(Event::AccessPoints(aps));
                });
                let _ = events.send(Event::Finished(result));
                return;
            }
            let mut args = vec![
                // No animated progress/line erasure on the authentication PTY.
                "--terse".into(),
                "--colors".into(),
                "no".into(),
                "--ask".into(),
                "--wait".into(),
                // Lucy bounds active work itself; waiting for a user to return
                // to an authentication form must not expire nmcli's deadline.
                "86400".into(),
            ];
            args.extend(action.args());
            let result = interactive("nmcli", &args, &events, &reply_rx, &cancel, wifi.as_ref());
            let _ = events.send(Event::Finished(result));
            let _ = tx.send(Request::Refresh);
        }));
    }
    fn answer(self: &Rc<Self>) {
        let pending = self.pending_wifi.borrow_mut().take();
        if let Some(action) = pending {
            if self.secret.text().is_empty() {
                *self.pending_wifi.borrow_mut() = Some(action);
                self.secret.grab_focus();
                return;
            }
            let password = self.secret.text().as_bytes().to_vec();
            auth_trace("inline password submitted (content not logged)");
            self.start(action);
            *self.prefill.borrow_mut() = Some(password);
            self.submit.set_sensitive(false);
            return;
        }
        let secret = self.secret.text().as_bytes().to_vec();
        if let Some(tx) = self.replies.borrow().as_ref() {
            self.wifi_reply_sent.set(true);
            let _ = tx.send(secret);
        }
        self.secret.set_text("");
        self.auth.hide();
        self.submit.set_sensitive(false);
        self.spinner.show();
        self.spinner.start();
        self.note("인증 중…");
    }
    fn auth(&self, prompt: &str) {
        auth_trace("GTK received authentication prompt");
        let lower = prompt.to_ascii_lowercase();
        let wifi_secret =
            lower.contains("psk") || lower.contains("wep-key") || lower.contains("sae-password");
        if wifi_secret {
            if let Some(password) = self.prefill.borrow_mut().take() {
                auth_trace("queued inline password for Wi-Fi authentication");
                if let Some(tx) = self.replies.borrow().as_ref() {
                    self.wifi_reply_sent.set(true);
                    let _ = tx.send(password);
                }
                return;
            }
        }
        let identity =
            lower.contains("username") || lower.contains("user name") || lower.contains("identity");
        self.prompt.set_text(if identity {
            "사용자 이름"
        } else if lower.contains("yes/no") {
            "확인 응답 (yes/no)"
        } else {
            "네트워크 인증"
        });
        self.prompt.set_tooltip_text(Some(prompt));
        self.secret
            .set_input_purpose(if identity || lower.contains("yes/no") {
                gtk::InputPurpose::FreeForm
            } else {
                gtk::InputPurpose::Password
            });
        self.secret
            .set_visibility(identity || lower.contains("yes/no"));
        self.secret.set_text("");
        self.submit.set_label("확인");
        self.submit.set_sensitive(true);
        self.show_auth();
        self.spinner.stop();
        self.spinner.hide();
        self.note(if wifi_secret && self.wifi_reply_sent.get() {
            "Wi-Fi 인증을 완료하지 못했습니다. 비밀번호를 확인하고 다시 입력하세요."
        } else if wifi_secret {
            "Wi-Fi 비밀번호를 입력하세요."
        } else {
            "인증 정보를 입력하세요."
        });
    }
    fn show_auth(&self) {
        self.draft_selection.set(None);
        // This container deliberately ignores show_all during list rerenders.
        // Explicitly show it and its children when authentication is requested.
        for child in self.auth.children() {
            child.show_all();
        }
        self.auth.show();
        let auth = self.auth.downgrade();
        let secret = self.secret.downgrade();
        glib::idle_add_local_once(move || {
            let (Some(auth), Some(secret)) = (auth.upgrade(), secret.upgrade()) else {
                return;
            };
            if !auth.get_visible() {
                return;
            }
            secret.grab_focus();
            let mut parent = auth.parent();
            while let Some(widget) = parent {
                if let Ok(scroll) = widget.clone().downcast::<gtk::ScrolledWindow>() {
                    if let Some(content) = scroll
                        .child()
                        .and_then(|v| v.downcast::<gtk::Bin>().ok())
                        .and_then(|v| v.child())
                    {
                        if let Some((_, y)) = auth.translate_coordinates(&content, 0, 0) {
                            scroll
                                .vadjustment()
                                .clamp_page(y as f64, (y + auth.allocated_height()) as f64);
                        }
                    }
                    break;
                }
                parent = widget.parent();
            }
        });
    }
    fn finish(self: &Rc<Self>, result: Result<(), String>) {
        self.busy.set(false);
        self.scanning.set(false);
        self.spinner.stop();
        self.spinner.hide();
        self.auth.hide();
        self.secret.set_text("");
        self.cancel_button.hide();
        self.replies.borrow_mut().take();
        self.wipe_prefill();
        self.pending_wifi.borrow_mut().take();
        self.auth_anchor.borrow_mut().take();
        match result {
            Ok(()) => {
                self.status.hide();
            }
            Err(error) => self.note(&error),
        }
        self.rerender();
    }
    fn close(&self) {
        auth_trace("network popup closing; cancelling worker");
        if self.closed.replace(true) {
            return;
        }
        self.dismiss.borrow_mut().take();
        self.cancelled.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Request::Quit);
        if let Some(child) = self.monitor.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        self.secret.set_text("");
        self.wipe_prefill();
        self.replies.borrow_mut().take();
        for w in &self.backdrops {
            w.hide();
        }
        self.win.hide();
        gtk::main_quit();
    }

    fn hide(&self) {
        // Closing the popup is not cancelling the user's connection or draft.
        if self.auth.get_visible() {
            let position = self.secret.position();
            self.draft_selection.set(Some(
                self.secret
                    .selection_bounds()
                    .unwrap_or((position, position)),
            ));
        }
        self.dismiss.borrow_mut().take();
        for backdrop in &self.backdrops {
            backdrop.hide();
        }
        self.secret
            .set_visibility(self.secret.input_purpose() != gtk::InputPurpose::Password);
        self.win.hide();
    }

    #[allow(deprecated)]
    fn present(self: &Rc<Self>, tab: Option<&str>) {
        if let Some(tab) = tab {
            self.stack.set_visible_child_name(tab);
        }
        if self.win.is_visible() || self.headless {
            return;
        }
        let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(self);
        clicks.attach(None, move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
            ControlFlow::Break
        });
        *self.dismiss.borrow_mut() =
            crate::dismiss::Backdrop::new("lucy-network-dismiss", clicked).ok();
        if self.dismiss.borrow().is_none() {
            for backdrop in &self.backdrops {
                backdrop.show_all();
            }
        }
        let display = gdk::Display::default().unwrap();
        layer::monitor(&self.win, &focused_monitor(&display));
        self.win.show();
        if self.auth.get_visible() {
            self.secret.grab_focus();
            if let Some((start, end)) = self.draft_selection.take() {
                self.secret.select_region(start, end);
            }
        }
        let _ = self.tx.send(Request::Refresh);
    }
}

pub(crate) struct Page {
    pub widget: gtk::Widget,
    ui: Rc<Ui>,
    backend: Backend,
    source: glib::SourceId,
}
impl Page {
    #[allow(deprecated)]
    pub fn new(back: impl Fn() + 'static) -> Self {
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = Backend::new(events.clone());
        let ui = Ui::new_with_visibility(&backend, events, false, true);
        let widget = ui.win.child().unwrap();
        ui.win.remove(&widget);
        let card = widget.clone().downcast::<gtk::Box>().unwrap();
        let header = card.children()[0].clone();
        card.remove(&header);
        let header = crate::panel::header("네트워크", back);
        card.pack_start(&header, false, false, 0);
        card.reorder_child(&header, 0);
        let weak = Rc::downgrade(&ui);
        let source = receiver.attach(None, move |event| {
            let Some(ui) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            match event {
                Event::State(state) => {
                    if !ui.busy.get() {
                        ui.spinner.stop();
                        ui.spinner.hide();
                        if ui.state.borrow().is_none() {
                            ui.status.hide();
                        }
                    }
                    if ui.state.borrow().as_ref() != Some(&state) {
                        ui.render(state);
                    }
                }
                Event::AccessPoints(aps) => {
                    if let Some(state) = ui.state.borrow_mut().as_mut() {
                        state.aps = aps;
                    }
                    ui.rerender();
                }
                Event::Error(error) => {
                    if !ui.busy.get() {
                        ui.spinner.stop();
                        ui.spinner.hide();
                    }
                    ui.note(&error);
                }
                Event::Prompt(prompt) => ui.auth(&prompt),
                Event::Finished(result) => ui.finish(result),
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
    pub fn select(&self, tab: &str) {
        self.ui.stack.set_visible_child_name(tab);
        let _ = self.ui.tx.send(Request::Refresh);
    }
    pub fn conceal(&self) {
        self.ui.secret.set_visibility(false);
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        self.ui.cancelled.store(true, Ordering::Release);
        self.ui.replies.borrow_mut().take();
        self.backend.shutdown();
        if let Some(handle) = self.ui.action_thread.borrow_mut().take() {
            let _ = handle.join();
        }
        if let Some(source) = glib::MainContext::default().find_source_by_id(&self.source) {
            source.destroy();
        }
        unsafe {
            self.ui.win.destroy();
        }
    }
}

struct NetworkControl {
    socket: Rc<UnixDatagram>,
    path: PathBuf,
}
impl NetworkControl {
    fn path() -> Result<PathBuf, env::VarError> {
        Ok(Path::new(&env::var("XDG_RUNTIME_DIR")?).join("lucy-network-control.sock"))
    }
    fn bind() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let socket = UnixDatagram::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket: Rc::new(socket),
            path,
        })
    }
    fn request(tab: &str) -> Result<(), Box<dyn std::error::Error>> {
        let socket = UnixDatagram::unbound()?;
        let path = Self::path()?;
        for attempt in 0..10 {
            match socket.send_to(tab.as_bytes(), &path) {
                Ok(_) => return Ok(()),
                Err(_) if attempt < 9 => thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!()
    }
    fn watch(&self, ui: &Rc<Ui>) -> glib::SourceId {
        let socket = self.socket.clone();
        let weak = Rc::downgrade(ui);
        glib::source::unix_fd_add_local(socket.as_raw_fd(), glib::IOCondition::IN, move |_, _| {
            let Some(ui) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut bytes = [0u8; 32];
            while let Ok(n) = socket.recv(&mut bytes) {
                match &bytes[..n] {
                    b"show" => ui.present(None),
                    b"vpn" => ui.present(Some("vpn")),
                    b"ethernet" => ui.present(Some("ethernet")),
                    _ => {}
                }
            }
            ControlFlow::Continue
        })
    }
}
impl Drop for NetworkControl {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[allow(deprecated)]
pub fn run(vpn_first: bool) -> Result<(), Box<dyn std::error::Error>> {
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(Path::new(&env::var("XDG_RUNTIME_DIR")?).join("lucy-network-popup.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if env::args().any(|arg| arg.starts_with("--self-test-network")) {
            return Err(
                "network popup is running; close the process before a standalone self-test".into(),
            );
        }
        return NetworkControl::request(if vpn_first {
            "vpn"
        } else if env::args().any(|arg| arg == "--ethernet") {
            "ethernet"
        } else {
            "show"
        });
    }
    if unsafe { layer::gtk_layer_is_supported() } == 0 {
        return Err("Wayland layer-shell required".into());
    }
    let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let control = NetworkControl::bind()?;
    let backend = Backend::new(events.clone());
    let ui = Ui::new(&backend, events, vpn_first);
    let control_source = control.watch(&ui);
    if env::args().any(|arg| arg == "--ethernet") {
        ui.stack.set_visible_child_name("ethernet");
    }
    if env::args().any(|arg| arg == "--self-test-network") {
        let loaded = Instant::now();
        let initial = state()?;
        let load_time = loaded.elapsed();
        let devices = initial.devices.len();
        let profiles = initial.profiles.len();
        for ap in initial.aps.iter().filter(|ap| ap.active) {
            assert!(
                initial
                    .profiles
                    .iter()
                    .any(|profile| profile.wifi.as_ref().is_some_and(|wifi| wifi.matches(ap))),
                "actual connected AP must match saved SSID metadata"
            );
        }
        ui.render(initial.clone());
        ui.render(initial.clone());
        assert!(
            !ui.busy.get(),
            "opening/rendering must never start an action"
        );
        ui.start(Action::Forget("not-an-existing-wifi-uuid".into()));
        assert!(
            !ui.busy.get(),
            "unknown/non-Wi-Fi deletion must not reach nmcli"
        );
        let fixture = Ap {
            active: false,
            bssid: "00:00:00:00:00:01".into(),
            ssid: "입력란 검증".into(),
            signal: 80,
            security: "WPA2".into(),
            device: "wlan0".into(),
        };
        let mut test_state = initial;
        test_state.aps.push(fixture.clone());
        test_state.profiles.push(Profile {
            name: "삭제 UI 검증".into(),
            uuid: "fixture-delete-only".into(),
            kind: "802-11-wireless".into(),
            active: false,
            wifi: None,
        });
        *ui.pending_delete.borrow_mut() = Some("fixture-delete-only".into());
        ui.render(test_state);
        fn find_button(widget: &gtk::Widget, text: &str) -> Option<gtk::Button> {
            if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
                if button.label().as_deref() == Some(text) {
                    return Some(button);
                }
            }
            widget
                .clone()
                .downcast::<gtk::Container>()
                .ok()?
                .children()
                .iter()
                .find_map(|w| find_button(w, text))
        }
        let confirm =
            find_button(ui.wifi.upcast_ref(), "삭제 확인").expect("delete confirmation visible");
        let row = confirm
            .parent()
            .and_then(|w| w.parent())
            .expect("confirmation is in the existing row");
        assert!(
            row.style_context().has_class("network-row"),
            "confirmation must not add a separate row below the clicked item"
        );
        assert!(
            !ui.busy.get(),
            "showing confirmation must not delete a profile"
        );
        ui.pending_delete.borrow_mut().take();
        ui.prepare_wifi(&fixture);
        assert!(
            ui.auth.get_visible() && ui.secret.get_visible(),
            "secured AP click must show the password container and input, not just reparent them"
        );
        assert!(
            !ui.busy.get(),
            "expanding an encrypted AP must not start a connection"
        );
        assert!(ui.wifi.is_ancestor(&ui.win));
        assert!(
            ui.auth.is_ancestor(&ui.wifi),
            "password entry must be inside the Wi-Fi list"
        );
        assert!(!gtk::prelude::EntryExt::is_visible(&ui.secret));
        ui.secret.set_text("fixture-only");
        ui.rerender();
        assert_eq!(
            ui.secret.text().as_str(),
            "fixture-only",
            "refresh must preserve typed input"
        );
        assert!(!ui.secret.is_caps_lock_warning());
        ui.secret.emit_by_name::<()>(
            "icon-release",
            &[
                &gtk::EntryIconPosition::Secondary,
                &gdk::Event::new(gdk::EventType::ButtonRelease),
            ],
        );
        assert!(
            gtk::prelude::EntryExt::is_visible(&ui.secret),
            "eye reveals password"
        );
        ui.secret.select_region(2, 5);
        ui.hide();
        assert!(!ui.closed.get() && ui.pending_wifi.borrow().is_some());
        assert_eq!(
            ui.secret.text().as_str(),
            "fixture-only",
            "hiding preserves draft in memory"
        );
        ui.present(None);
        assert_eq!(ui.secret.text().as_str(), "fixture-only");
        assert_eq!(
            ui.secret.selection_bounds(),
            Some((2, 5)),
            "reopening restores the editing selection"
        );
        ui.hide();
        NetworkControl::request("show")?;
        let reopen = Instant::now();
        while !ui.win.is_visible() && reopen.elapsed() < Duration::from_secs(1) {
            glib::MainContext::default().iteration(false);
        }
        assert!(ui.win.is_visible(), "IPC must reopen the existing window");
        assert_eq!(
            ui.secret.text().as_str(),
            "fixture-only",
            "IPC reopening preserves the entry"
        );
        ui.busy.set(true);
        ui.hide();
        ui.present(None);
        assert!(
            ui.busy.get() && !ui.cancelled.load(Ordering::Acquire),
            "hiding must not cancel a connection"
        );
        ui.busy.set(false);
        ui.pending_wifi.borrow_mut().take();
        ui.auth("Username:");
        assert!(gtk::prelude::EntryExt::is_visible(&ui.secret));
        ui.auth("Password (vpn.secrets.password):");
        assert!(!gtk::prelude::EntryExt::is_visible(&ui.secret));
        let (tx, rx) = mpsc::channel();
        *ui.replies.borrow_mut() = Some(tx);
        ui.secret.set_text("fixture-only");
        ui.answer();
        assert_eq!(rx.recv()?, b"fixture-only");
        assert!(ui.secret.text().is_empty());
        assert!(!gtk::prelude::WidgetExt::is_visible(&ui.auth));
        *ui.prefill.borrow_mut() = Some(b"prefill-only".to_vec());
        ui.auth("Password (802-11-wireless-security.psk):");
        assert_eq!(rx.recv()?, b"prefill-only");
        assert!(ui.prefill.borrow().is_none());
        backend.shutdown();
        ui.win.hide();
        for window in &ui.backdrops {
            window.hide();
        }
        drop(ui);
        drop(backend);
        control_source.remove();
        println!("PASS: live state ({devices} devices, {profiles} profiles, {:.0}ms); saved SSID matching, eye icon, IPC reopen, draft and busy state, password transfer, deletion guard; no connection writes", load_time.as_secs_f64() * 1000.0);
        return Ok(());
    }
    let weak = Rc::downgrade(&ui);
    let scan_test = env::args().any(|arg| {
        matches!(
            arg.as_str(),
            "--self-test-network-scan" | "--self-test-network-scan-headless"
        )
    });
    let scan_started = Cell::new(None::<Instant>);
    let scan_passed = Rc::new(Cell::new(false));
    let passed = scan_passed.clone();
    receiver.attach(None, move |event| {
        let Some(ui) = weak.upgrade() else {
            return ControlFlow::Break;
        };
        if ui.closed.get() {
            return ControlFlow::Break;
        }
        match event {
            Event::AccessPoints(aps) => {
                let state = ui.state.borrow().clone();
                if let Some(mut state) = state {
                    state.aps = aps;
                    ui.render(state);
                }
            }
            Event::State(state) => {
                if !ui.busy.get() {
                    ui.spinner.stop();
                    ui.spinner.hide();
                    // A refresh follows Finished; do not erase its failure message.
                    if ui.state.borrow().is_none() {
                        ui.status.hide();
                    }
                }
                if ui.state.borrow().as_ref() != Some(&state) {
                    ui.render(state);
                }
                if scan_test && scan_started.get().is_none() {
                    scan_started.set(Some(Instant::now()));
                    ui.start(Action::Scan);
                    assert!(ui.scanning.get() && ui.busy.get());
                    assert!(ui.spinner.get_visible() && ui.spinner.is_active());
                }
            }
            Event::Error(error) => {
                if !ui.busy.get() {
                    ui.spinner.stop();
                    ui.spinner.hide();
                }
                ui.note(&error);
            }
            Event::Prompt(prompt) => ui.auth(&prompt),
            Event::Finished(result) => {
                if scan_test {
                    passed.set(result.is_ok());
                    println!(
                        "SCAN TEST: {:.3}s, {} APs, result={:?}",
                        scan_started
                            .get()
                            .map(|t| t.elapsed().as_secs_f64())
                            .unwrap_or_default(),
                        ui.state
                            .borrow()
                            .as_ref()
                            .map(|s| s.aps.len())
                            .unwrap_or_default(),
                        result
                    );
                }
                ui.finish(result);
                if scan_test {
                    ui.close();
                    return ControlFlow::Break;
                }
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
    control_source.remove();
    backend.shutdown();
    if let Some(handle) = ui.action_thread.borrow_mut().take() {
        let _ = handle.join();
    }
    drop(ui);
    drop(backend);
    if scan_test && !scan_passed.get() {
        return Err("scan verification did not complete successfully".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "briefly disconnects current Wi-Fi; run explicitly with --test-threads=1"]
    #[allow(deprecated)]
    fn live_saved_wifi_reconnect_while_hidden() {
        gtk::init().expect("Wayland GTK session required");
        let initial = state().unwrap();
        let mut ap = initial.aps.iter().find(|ap| ap.active).unwrap().clone();
        let uuid = nm(&["-g", "GENERAL.CON-UUID", "device", "show", &ap.device])
            .unwrap()
            .trim()
            .to_owned();
        assert!(initial.profiles.iter().any(|profile| profile.uuid == uuid
            && profile.wifi.as_ref().is_some_and(|wifi| wifi.matches(&ap))));
        struct Restore(String, String);
        impl Drop for Restore {
            fn drop(&mut self) {
                if nm(&[
                    "-g",
                    "GENERAL.STATE,GENERAL.CON-UUID",
                    "device",
                    "show",
                    &self.1,
                ])
                .is_ok_and(|current| {
                    current
                        .lines()
                        .next()
                        .is_some_and(|state| state.starts_with("100 "))
                        && current.lines().nth(1) == Some(self.0.as_str())
                }) {
                    return;
                }
                let _ = nm(&["--wait", "15", "connection", "up", "uuid", &self.0]);
            }
        }
        let restore = Restore(uuid.clone(), ap.device.clone());
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = Backend::new(events.clone());
        let ui = Ui::new_with_visibility(&backend, events, false, true);
        ui.render(initial);
        nm(&["--wait", "5", "connection", "down", "uuid", &uuid]).unwrap();
        ap.active = false;
        let started = Instant::now();
        ui.prepare_wifi(&ap);
        assert!(ui.busy.get() && ui.pending_wifi.borrow().is_none());
        assert!(
            !ui.auth.get_visible(),
            "saved AP must not demand a password up front"
        );
        ui.hide();
        assert!(!ui.closed.get() && !ui.cancelled.load(Ordering::Acquire));
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        let weak = Rc::downgrade(&ui);
        let source = receiver.attach(None, move |event| {
            let ui = weak.upgrade().unwrap();
            match event {
                Event::State(state) => ui.render(state),
                Event::Prompt(_) => {
                    *completed.borrow_mut() = Some(Err(
                        "saved Wi-Fi unexpectedly asked for authentication".into(),
                    ));
                    ui.close();
                }
                Event::Finished(outcome) => {
                    *completed.borrow_mut() = Some(outcome.clone());
                    ui.finish(outcome);
                    assert!(
                        !ui.win.is_visible(),
                        "background completion must not open the popup"
                    );
                    ui.close();
                }
                _ => {}
            }
            ControlFlow::Continue
        });
        let timeout_ui = ui.clone();
        let timeout =
            glib::timeout_add_local_once(Duration::from_secs(20), move || timeout_ui.close());
        gtk::main();
        if let Some(source) = glib::MainContext::default().find_source_by_id(&timeout) {
            source.destroy();
        }
        glib::MainContext::default()
            .find_source_by_id(&source)
            .unwrap()
            .destroy();
        if let Some(worker) = ui.action_thread.borrow_mut().take() {
            worker.join().unwrap();
        }
        backend.shutdown();
        assert_eq!(
            *result.borrow(),
            Some(Ok(())),
            "saved connection must complete while hidden"
        );
        assert_eq!(
            nm(&["-g", "GENERAL.CON-UUID", "device", "show", &ap.device])
                .unwrap()
                .trim(),
            uuid
        );
        println!("PASS: saved Wi-Fi connected in {:.3}s without a prompt; hiding preserved the active task", started.elapsed().as_secs_f64());
        drop(restore);
    }

    /// Explicit opt-in only: attempts a wrong password on the supplied real AP,
    /// verifies the GTK retry state, cancels activation, and restores Wi-Fi.
    #[test]
    #[ignore = "changes live Wi-Fi; set LUCY_TEST_WIFI_BSSID and use --test-threads=1"]
    #[allow(deprecated)]
    fn live_wifi_wrong_password_and_cancel() {
        let target = env::var("LUCY_TEST_WIFI_BSSID").expect("explicit test AP required");
        gtk::init().expect("Wayland GTK session required");
        let mut initial = state().unwrap();
        initial.aps = scan_access_points(&AtomicBool::new(false)).unwrap();
        let ap = initial
            .aps
            .iter()
            .find(|ap| ap.bssid == target && !ap.active)
            .expect("test AP must be visible and not the current connection")
            .clone();
        let original = nm(&["-g", "GENERAL.CON-UUID", "device", "show", &ap.device]).unwrap();
        assert!(!original.trim().is_empty());
        struct Restore(String);
        impl Drop for Restore {
            fn drop(&mut self) {
                nm(&["--wait", "15", "connection", "up", "uuid", self.0.trim()])
                    .expect("restore original Wi-Fi");
            }
        }
        let restore = Restore(original);
        let (events, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = Backend::new(events.clone());
        let ui = Ui::new_with_visibility(&backend, events, false, true);
        ui.render(initial);
        ui.prepare_wifi(&ap);
        ui.secret.set_text("lucy-fixture-wrong-only");
        ui.answer();
        let prompts = Rc::new(Cell::new(0));
        let received = prompts.clone();
        let weak = Rc::downgrade(&ui);
        let source = receiver.attach(None, move |event| {
            let ui = weak.upgrade().unwrap();
            match event {
                Event::State(state) => ui.render(state),
                Event::Prompt(prompt) => {
                    received.set(received.get() + 1);
                    ui.auth(&prompt);
                    if received.get() == 1 {
                        ui.hide();
                        assert!(!ui.cancelled.load(Ordering::Acquire) && ui.busy.get());
                    }
                    if received.get() == 2 {
                        assert!(ui.auth.get_visible() && ui.secret.get_visible());
                        assert!(!ui.spinner.is_active());
                        assert!(ui.status.text().contains("비밀번호를 확인"));
                        ui.secret.set_text("fixture-retry-draft");
                        ui.hide();
                        ui.present(None);
                        assert_eq!(ui.secret.text().as_str(), "fixture-retry-draft");
                        println!("PASS: real nmcli accepted first reply and requested retry; GTK shows retry without spinner");
                        let ui = ui.clone();
                        glib::timeout_add_local_once(Duration::from_millis(300), move || {
                            ui.close();
                        });
                    }
                }
                Event::Finished(result) => {
                    println!("early result: {result:?}");
                    ui.close();
                }
                _ => {}
            }
            ControlFlow::Continue
        });
        let timeout_ui = ui.clone();
        let timeout =
            glib::timeout_add_local_once(Duration::from_secs(45), move || timeout_ui.close());
        gtk::main();
        if let Some(source) = glib::MainContext::default().find_source_by_id(&timeout) {
            source.destroy();
        }
        glib::MainContext::default()
            .find_source_by_id(&source)
            .unwrap()
            .destroy();
        if let Some(worker) = ui.action_thread.borrow_mut().take() {
            worker.join().unwrap();
        }
        backend.shutdown();
        assert_eq!(prompts.get(), 2, "real Wi-Fi must reach password retry");
        let device = nm(&["-g", "GENERAL.STATE", "device", "show", &ap.device]).unwrap();
        assert!(
            !device.starts_with("60 "),
            "closing popup must cancel need-auth activation"
        );
        drop(restore);
    }
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
