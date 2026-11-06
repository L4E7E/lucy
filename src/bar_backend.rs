use serde_json::Value;
use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Default)]
pub(crate) struct Metric {
    pub(crate) text: String,
    pub(crate) tooltip: String,
    pub(crate) critical: bool,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    pub(crate) values: HashMap<&'static str, Metric>,
}
pub(crate) enum Update {
    Metrics(State),
    Audio(Option<Metric>),
    Workspaces(Vec<Value>),
    Outputs(Value),
}

pub(crate) fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}
pub(crate) fn number(path: impl AsRef<Path>) -> Option<u64> {
    read(path).parse().ok()
}
pub(crate) fn cpu_times(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // guest and guest_nice are already included in user/nice.
    let n: Vec<u64> = fields
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if n.len() < 5 {
        return None;
    }
    Some((n.iter().sum(), n[3] + n[4]))
}
pub(crate) fn cpu_percent(previous: (u64, u64), current: (u64, u64)) -> u64 {
    let total = current.0.saturating_sub(previous.0);
    let idle = current.1.saturating_sub(previous.1);
    (100 * total.saturating_sub(idle))
        .checked_div(total)
        .unwrap_or(0)
}
pub(crate) fn memory(text: &str) -> Option<(u64, u64)> {
    let get = |key: &str| {
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == key)
                .then(|| parts.next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    let total = get("MemTotal:")?;
    Some((total.saturating_sub(get("MemAvailable:")?), total))
}
pub(crate) fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1073741824.0
}
pub(crate) fn usage(used: u64, total: u64, icon: &str) -> Metric {
    Metric {
        text: format!(
            "{icon} {}%",
            used.saturating_mul(100).checked_div(total).unwrap_or(0)
        ),
        tooltip: format!("{:.1} / {:.1} GiB", gib(used), gib(total)),
        critical: false,
    }
}
// Bounded helper calls: no persistent subprocesses and no shell interpolation.
pub(crate) fn output(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let bytes = reader.join().ok()?;
    ok.then(|| String::from_utf8_lossy(&bytes).trim().to_owned())
}
pub(crate) fn execute(program: String, args: Vec<String>) {
    thread::spawn(move || {
        let _ = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}
pub(crate) fn local(name: &str) -> String {
    format!("{}/.local/bin/{name}", env::var("HOME").unwrap_or_default())
}
pub(crate) fn action(program: &str, args: &[&str]) {
    execute(program.into(), args.iter().map(|s| s.to_string()).collect());
}
pub(crate) fn lucy(mode: &[&str]) {
    // Atomic updates unlink the running bar's old inode. Linux then reports
    // current_exe with a " (deleted)" suffix; launch the installed replacement.
    let exe = env::current_exe()
        .ok()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from(local("lucy-shell")));
    execute(
        exe.to_string_lossy().into_owned(),
        mode.iter().map(|s| s.to_string()).collect(),
    );
}

pub(crate) fn sample(previous: &mut Option<(u64, u64)>) -> State {
    let mut state = State::default();
    if let Some(current) = cpu_times(&read("/proc/stat")) {
        let text = previous
            .map(|old| format!(" {}%", cpu_percent(old, current)))
            .unwrap_or_else(|| " …".into());
        state.values.insert(
            "cpu",
            Metric {
                text,
                tooltip: "CPU 사용률".into(),
                critical: false,
            },
        );
        *previous = Some(current);
    }
    if let Some((used, total)) = memory(&read("/proc/meminfo")) {
        state
            .values
            .insert("memory", usage(used * 1024, total * 1024, "󰍛"));
    }
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: NUL-terminated path and writable statvfs storage, read only on success.
    if unsafe { libc::statvfs(c"/".as_ptr(), stat.as_mut_ptr()) } == 0 {
        let s = unsafe { stat.assume_init() };
        state.values.insert(
            "disk",
            usage(
                (s.f_blocks - s.f_bfree) * s.f_frsize,
                s.f_blocks * s.f_frsize,
                "",
            ),
        );
    }
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            if ["k10temp", "coretemp"].contains(&read(entry.path().join("name")).as_str()) {
                if let Some(t) = number(entry.path().join("temp1_input")) {
                    state.values.insert(
                        "temperature",
                        Metric {
                            text: format!(" {}°C", t / 1000),
                            tooltip: "CPU 온도".into(),
                            critical: t >= 90000,
                        },
                    );
                    break;
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
        if let Some(entry) = entries.flatten().next() {
            if let (Some(now), Some(max)) = (
                number(entry.path().join("brightness")),
                number(entry.path().join("max_brightness")),
            ) {
                state.values.insert(
                    "backlight",
                    Metric {
                        text: format!("󰃠 {}%", now * 100 / max.max(1)),
                        tooltip: "밝기 · 휠로 조절".into(),
                        critical: false,
                    },
                );
            }
        }
    }
    let supplies = Path::new("/sys/class/power_supply");
    let plugged = external_power(supplies);
    if let Ok(entries) = fs::read_dir(supplies) {
        for entry in entries.flatten() {
            if read(entry.path().join("type")) == "Battery" {
                if let Some(c) = number(entry.path().join("capacity")) {
                    let status = read(entry.path().join("status"));
                    state.values.insert(
                        "battery",
                        Metric {
                            text: format!(
                                "{} {c}%",
                                if plugged || status == "Charging" {
                                    "󰂄"
                                } else {
                                    ""
                                }
                            ),
                            tooltip: status,
                            critical: c <= 15,
                        },
                    );
                    break;
                }
            }
        }
    }
    if let Some(value) = output(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE,STATE", "device", "status"],
    ) {
        let connected: Vec<_> = value
            .lines()
            .filter(|l| {
                l.ends_with(":connected") && (l.contains(":wifi:") || l.contains(":ethernet:"))
            })
            .collect();
        state.values.insert(
            "network",
            Metric {
                text: if connected.iter().any(|l| l.contains(":wifi:")) {
                    ""
                } else if connected.is_empty() {
                    ""
                } else {
                    "󰈀"
                }
                .into(),
                tooltip: format!("{}\n클릭: 네트워크 · 우클릭: VPN", connected.join("\n")),
                critical: false,
            },
        );
    }
    let bluetooth = output("bluetoothctl", &["show"]);
    state.values.insert(
        "bluetooth",
        Metric {
            text: if bluetooth
                .as_ref()
                .is_some_and(|s| s.contains("Powered: yes"))
            {
                ""
            } else {
                "󰂲"
            }
            .into(),
            tooltip: "블루투스".into(),
            critical: false,
        },
    );
    state
}

fn external_power(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            read(entry.path().join("type")) != "Battery" && read(entry.path().join("online")) == "1"
        })
    })
}

pub(crate) fn audio_metric() -> Option<Metric> {
    let value = output("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])?;
    let volume = value.split_whitespace().nth(1)?.parse::<f64>().ok()?;
    Some(Metric {
        text: if value.contains("MUTED") {
            "󰖁".into()
        } else {
            format!(" {:.0}%", volume * 100.0)
        },
        tooltip: "클릭: 소리 설정 · 우클릭: 음소거 · 휠: 음량".into(),
        critical: false,
    })
}

pub(crate) fn output_audio_event(line: &str) -> bool {
    // Ignore source and per-application stream events: this is the output gauge.
    line.contains(" on sink #") || line.contains(" on server #") || line.contains(" on card #")
}

pub(crate) struct AudioWatcher {
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) subscriber: Arc<Mutex<Option<Child>>>,
    pub(crate) wake: mpsc::Sender<()>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
}
impl AudioWatcher {
    pub(crate) fn new(updates: glib::Sender<Update>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let subscriber = Arc::new(Mutex::new(None::<Child>));
        let (wake, requests) = mpsc::channel();
        let stopping = stop.clone();
        let reader = thread::spawn(move || {
            while requests.recv().is_ok() && !stopping.load(Ordering::Acquire) {
                // Collapse a burst of sink/port events and rapid slider writes.
                thread::sleep(Duration::from_millis(15));
                while requests.try_recv().is_ok() {}
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                if updates.send(Update::Audio(audio_metric())).is_err() {
                    break;
                }
            }
        });
        let stopping = stop.clone();
        let holder = subscriber.clone();
        let notify = wake.clone();
        let events = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let child = Command::new("pactl")
                    .arg("subscribe")
                    .env("LC_ALL", "C")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn();
                let Ok(mut child) = child else {
                    let _ = notify.send(());
                    thread::sleep(Duration::from_secs(1));
                    continue;
                };
                let stdout = child.stdout.take().unwrap();
                {
                    let mut slot = holder.lock().unwrap();
                    if stopping.load(Ordering::Acquire) {
                        let _ = child.kill();
                    }
                    *slot = Some(child);
                }
                // Snapshot on initial subscription and after a server reconnect.
                let _ = notify.send(());
                for line in BufReader::new(stdout).lines() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    match line {
                        Ok(line) if output_audio_event(&line) => {
                            let _ = notify.send(());
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                if let Some(mut child) = holder.lock().unwrap().take() {
                    let _ = child.wait();
                }
                if !stopping.load(Ordering::Acquire) {
                    let _ = notify.send(());
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        Self {
            stop,
            subscriber,
            wake,
            handles: vec![reader, events],
        }
    }
}
impl Drop for AudioWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.send(());
        if let Some(child) = self.subscriber.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

pub(crate) struct Workers {
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
    pub(crate) wake: mpsc::Sender<()>,
    pub(crate) _audio: AudioWatcher,
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.send(());
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
pub(crate) fn workers(tx: glib::Sender<Update>) -> Workers {
    let audio = AudioWatcher::new(tx.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let (wake, rx) = mpsc::channel();
    let stopping = stop.clone();
    let metrics = tx.clone();
    let sampler = thread::spawn(move || {
        let mut previous = None;
        let mut outputs = Value::Null;
        while !stopping.load(Ordering::Acquire) {
            if let Some(next) = output("niri", &["msg", "-j", "outputs"])
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            {
                if next != outputs {
                    outputs = next;
                    if metrics.send(Update::Outputs(outputs.clone())).is_err() {
                        break;
                    }
                }
            }
            if metrics
                .send(Update::Metrics(sample(&mut previous)))
                .is_err()
            {
                break;
            }
            let _ = rx.recv_timeout(Duration::from_secs(5));
            while rx.try_recv().is_ok() {}
        }
    });
    let stopping = stop.clone();
    let events = thread::spawn(move || {
        // A timeout lets shutdown interrupt an otherwise idle event stream.
        while !stopping.load(Ordering::Acquire) {
            let stream = env::var("NIRI_SOCKET")
                .ok()
                .and_then(|path| UnixStream::connect(path).ok());
            let Some(mut stream) = stream else {
                thread::sleep(Duration::from_secs(1));
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            if stream.write_all(b"\"EventStream\"\n").is_err() {
                continue;
            }
            let mut pending = Vec::new();
            let mut buffer = [0; 8192];
            let mut spaces: Vec<Value> = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => pending.extend_from_slice(&buffer[..n]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(_) => break,
                }
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                        if update_spaces(&mut spaces, &event)
                            && tx.send(Update::Workspaces(spaces.clone())).is_err()
                        {
                            return;
                        }
                    }
                }
                if pending.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
            if !stopping.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(500));
            }
        }
    });
    Workers {
        stop,
        handles: vec![sampler, events],
        wake,
        _audio: audio,
    }
}
pub(crate) fn update_spaces(spaces: &mut Vec<Value>, event: &Value) -> bool {
    if let Some(all) = event["WorkspacesChanged"]["workspaces"].as_array() {
        *spaces = all.clone();
        return true;
    }
    if let Some(id) = event["WorkspaceActivated"]["id"].as_u64() {
        let output = spaces
            .iter()
            .find(|w| w["id"].as_u64() == Some(id))
            .map(|w| w["output"].clone());
        let focused = event["WorkspaceActivated"]["focused"]
            .as_bool()
            .unwrap_or(false);
        for workspace in spaces {
            if Some(&workspace["output"]) == output.as_ref() {
                workspace["is_active"] = (workspace["id"].as_u64() == Some(id)).into();
            }
            if focused {
                workspace["is_focused"] = (workspace["id"].as_u64() == Some(id)).into();
            }
        }
        return true;
    }
    false
}
#[cfg(test)]
mod tests {
    #[test]
    fn plugged_in_is_detected_even_when_battery_is_not_charging() {
        let root = std::env::temp_dir().join(format!(
            "lucy-power-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        for (name, kind, state) in [("BAT0", "Battery", "Not charging"), ("ADP0", "Mains", "")] {
            let dir = root.join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("type"), kind).unwrap();
            std::fs::write(dir.join("status"), state).unwrap();
        }
        std::fs::write(root.join("ADP0/online"), "1\n").unwrap();
        assert!(super::external_power(&root));
        std::fs::write(root.join("ADP0/online"), "0\n").unwrap();
        assert!(!super::external_power(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    #[test]
    fn cpu_excludes_guest_and_handles_reset() {
        assert_eq!(
            cpu_times("cpu 10 20 30 40 50 60 70 80 900 800"),
            Some((360, 90))
        );
        assert_eq!(cpu_percent((100, 40), (200, 65)), 75);
        assert_eq!(cpu_percent((200, 65), (100, 40)), 0);
    }
    #[test]
    fn memory_uses_available_including_reclaimable_cache() {
        assert_eq!(
            memory("MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 600 kB"),
            Some((400, 1000))
        );
        assert_eq!(memory("MemTotal: 1000 kB"), None);
    }
    #[test]
    fn activation_preserves_other_outputs() {
        let mut s = vec![
            serde_json::json!({"id":1,"output":"A","is_active":true}),
            serde_json::json!({"id":2,"output":"A","is_active":false}),
            serde_json::json!({"id":3,"output":"B","is_active":true}),
        ];
        update_spaces(
            &mut s,
            &serde_json::json!({"WorkspaceActivated":{"id":2,"focused":true}}),
        );
        assert_eq!(s[0]["is_active"], false);
        assert_eq!(s[1]["is_active"], true);
        assert_eq!(s[2]["is_active"], true);
        assert_eq!(s[1]["is_focused"], true);
    }
}
