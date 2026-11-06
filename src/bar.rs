//! Content-sized Wayland panels. Sampling and IPC never block the GTK thread.
use crate::{layer, *};
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
};

#[derive(Clone, Debug, Default)]
struct Metric {
    text: String,
    tooltip: String,
    critical: bool,
}
#[derive(Clone, Debug, Default)]
struct State {
    values: HashMap<&'static str, Metric>,
}
enum Update {
    Metrics(State),
    Audio(Option<Metric>),
    Workspaces(Vec<Value>),
    Outputs(Value),
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn number(path: impl AsRef<Path>) -> Option<u64> {
    read(path).parse().ok()
}
fn cpu_times(text: &str) -> Option<(u64, u64)> {
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
fn cpu_percent(previous: (u64, u64), current: (u64, u64)) -> u64 {
    let total = current.0.saturating_sub(previous.0);
    let idle = current.1.saturating_sub(previous.1);
    (100 * total.saturating_sub(idle))
        .checked_div(total)
        .unwrap_or(0)
}
fn memory(text: &str) -> Option<(u64, u64)> {
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
fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1073741824.0
}
fn usage(used: u64, total: u64, icon: &str) -> Metric {
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
fn execute(program: String, args: Vec<String>) {
    thread::spawn(move || {
        let _ = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}
fn local(name: &str) -> String {
    format!("{}/.local/bin/{name}", env::var("HOME").unwrap_or_default())
}
fn action(program: &str, args: &[&str]) {
    execute(program.into(), args.iter().map(|s| s.to_string()).collect());
}
fn lucy(mode: &[&str]) {
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

fn sample(previous: &mut Option<(u64, u64)>) -> State {
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
    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            if read(entry.path().join("type")) == "Battery" {
                if let Some(c) = number(entry.path().join("capacity")) {
                    let status = read(entry.path().join("status"));
                    state.values.insert(
                        "battery",
                        Metric {
                            text: format!("{} {c}%", if status == "Charging" { "󰂄" } else { "" }),
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
    let dnd =
        output("makoctl", &["mode"]).is_some_and(|s| s.lines().any(|s| s == "do-not-disturb"));
    state.values.insert(
        "notification",
        Metric {
            text: if dnd { "󰂛" } else { "󰂚" }.into(),
            tooltip: "클릭: 알림 복원 · 우클릭: 방해금지".into(),
            critical: dnd,
        },
    );
    state
}

fn audio_metric() -> Option<Metric> {
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

fn output_audio_event(line: &str) -> bool {
    // Ignore source and per-application stream events: this is the output gauge.
    line.contains(" on sink #") || line.contains(" on server #") || line.contains(" on card #")
}

struct AudioWatcher {
    stop: Arc<AtomicBool>,
    subscriber: Arc<Mutex<Option<Child>>>,
    wake: mpsc::Sender<()>,
    handles: Vec<thread::JoinHandle<()>>,
}
impl AudioWatcher {
    fn new(updates: glib::Sender<Update>) -> Self {
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

struct Workers {
    stop: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
    wake: mpsc::Sender<()>,
    _audio: AudioWatcher,
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
fn workers(tx: glib::Sender<Update>) -> Workers {
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
fn update_spaces(spaces: &mut Vec<Value>, event: &Value) -> bool {
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

struct Panel {
    windows: Vec<gtk::Window>,
    workspaces: gtk::Box,
    output: String,
    clock: gtk::Button,
    values: HashMap<&'static str, gtk::Button>,
}
fn button(text: &str, class: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    set_button_text(&b, text);
    b.style_context().add_class(class);
    b.set_can_focus(false);
    b
}
fn set_button_text(button: &gtk::Button, text: &str) {
    if let Some(label) = button.child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
        let markup: String = text
            .chars()
            .map(|c| {
                let escaped = glib::markup_escape_text(&c.to_string());
                if ('\u{e000}'..='\u{f8ff}').contains(&c)
                    || ('\u{f0000}'..='\u{ffffd}').contains(&c)
                {
                    format!("<span font_family=\"Symbols Nerd Font Mono\">{escaped}</span>")
                } else {
                    escaped.to_string()
                }
            })
            .collect();
        if label.label().as_str() != markup {
            label.set_markup(&markup);
        }
    }
}
fn panel_window(monitor: &gdk::Monitor, side: &str) -> (gtk::Window, gtk::Box) {
    let win = layer::window("lucy-bar", &format!("lucy-bar-{side}"), monitor);
    // Popups paint their inner card; panels paint the window CSS background.
    // app-paintable=true suppresses GtkWindow's default background drawing.
    win.set_app_paintable(false);
    layer::top(&win);
    layer::anchor(&win, 2, 6);
    if side == "left" {
        layer::anchor(&win, 0, 10);
    } else if side == "right" {
        layer::anchor(&win, 1, 10);
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.set_size_request(-1, 34);
    win.add(&content);
    (win, content)
}
impl Panel {
    fn new(
        monitor: &gdk::Monitor,
        output: String,
        wake: mpsc::Sender<()>,
        tray: &Rc<crate::tray::Tray>,
    ) -> Self {
        let (left, left_box) = panel_window(monitor, "left");
        let logo = button("", "logo");
        logo.set_tooltip_text(Some("애플리케이션"));
        logo.connect_clicked(|_| action("fuzzel", &[]));
        left_box.pack_start(&logo, false, false, 0);
        let workspaces = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        left_box.pack_start(&workspaces, false, false, 0);
        let (center, center_box) = panel_window(monitor, "center");
        let clock = button("…", "clock");
        center_box.pack_start(&clock, false, false, 0);
        clock.connect_clicked(|b| {
            let popover = gtk::Popover::new(Some(b));
            let calendar = gtk::Calendar::new();
            popover.add(&calendar);
            popover.connect_closed(|p| unsafe {
                p.destroy();
            });
            popover.show_all();
            popover.popup();
        });
        let (right, right_box) = panel_window(monitor, "right");
        let mut values = HashMap::new();
        for name in [
            "temperature",
            "cpu",
            "memory",
            "disk",
            "backlight",
            "audio",
            "network",
            "bluetooth",
            "battery",
            "notification",
            "power",
        ] {
            if name == "power" {
                right_box.pack_start(&tray.widget(), false, false, 0);
            }
            let b = button(if name == "power" { "⏻" } else { "…" }, name);
            b.set_tooltip_text(Some(name));
            b.connect_clicked(move |_| match name {
                "audio" | "network" => lucy(&["control", "--page", name]),
                "bluetooth" => lucy(&["control", "--page", "bluetooth"]),
                "backlight" => lucy(&["control", "--page", "display"]),
                "battery" => lucy(&["control", "--page", "power"]),
                "power" => lucy(&["control"]),
                "notification" => action("makoctl", &["restore"]),
                _ => (),
            });
            let refresh = wake.clone();
            b.connect_button_press_event(move |_, e| {
                if e.button() != 3 {
                    return glib::Propagation::Proceed;
                }
                match name {
                    "audio" => action("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]),
                    "network" => lucy(&["control"]),
                    "notification" => action("makoctl", &["mode", "-t", "do-not-disturb"]),
                    _ => return glib::Propagation::Proceed,
                }
                if name != "audio" {
                    let _ = refresh.send(());
                }
                glib::Propagation::Stop
            });
            if name == "audio" || name == "backlight" {
                b.add_events(gdk::EventMask::SCROLL_MASK);
                let refresh = wake.clone();
                b.connect_scroll_event(move |_, e| {
                    let up = match e.direction() {
                        gdk::ScrollDirection::Up => true,
                        gdk::ScrollDirection::Down => false,
                        _ => return glib::Propagation::Proceed,
                    };
                    if name == "audio" {
                        action(
                            "wpctl",
                            &[
                                "set-volume",
                                "-l",
                                "1.0",
                                "@DEFAULT_AUDIO_SINK@",
                                if up { "5%+" } else { "5%-" },
                            ],
                        );
                    } else {
                        action("brightnessctl", &["set", if up { "+5%" } else { "5%-" }]);
                    }
                    if name != "audio" {
                        let _ = refresh.send(());
                    }
                    glib::Propagation::Stop
                });
            }
            right_box.pack_start(&b, false, false, 0);
            values.insert(name, b);
        }
        // On smaller logical outputs, combine clock and workspaces rather than
        // allowing a centered island to overlap the system panel.
        let windows = if monitor.geometry().width() < 1500 {
            center_box.remove(&clock);
            left_box.pack_start(&clock, false, false, 0);
            unsafe {
                center.destroy();
            }
            left.style_context().add_class("compact");
            right.style_context().add_class("compact");
            vec![left, right]
        } else {
            vec![left, center, right]
        };
        let panel = Self {
            windows,
            workspaces,
            output,
            clock,
            values,
        };
        for w in &panel.windows {
            w.show_all();
        }
        panel
    }
    fn render(&self, state: &State) {
        for name in self.values.keys() {
            if *name == "power" {
                continue;
            }
            self.render_metric(name, state.values.get(name));
        }
    }
    fn render_metric(&self, name: &str, metric: Option<&Metric>) {
        if let Some(b) = self.values.get(name) {
            if let Some(metric) = metric {
                set_button_text(b, &metric.text);
                b.set_tooltip_text(Some(&metric.tooltip));
                b.show();
                if metric.critical {
                    b.style_context().add_class("critical");
                } else {
                    b.style_context().remove_class("critical");
                }
            } else {
                b.hide();
            }
        }
    }
    fn spaces(&self, spaces: &[Value]) {
        for child in self.workspaces.children() {
            self.workspaces.remove(&child);
        }
        let mut spaces: Vec<_> = spaces
            .iter()
            .filter(|w| w["output"].as_str() == Some(&self.output))
            .collect();
        spaces.sort_by_key(|w| w["idx"].as_u64());
        for space in spaces {
            let Some(index) = space["idx"].as_u64() else {
                continue;
            };
            let b = button(&space["idx"].to_string(), "workspace");
            if space["is_active"].as_bool() == Some(true) {
                b.style_context().add_class("active");
            }
            b.set_tooltip_text(space["name"].as_str());
            let output = self.output.clone();
            b.connect_clicked(move |_| {
                let output = output.clone();
                thread::spawn(move || {
                    if super::bar::output("niri", &["msg", "action", "focus-monitor", &output])
                        .is_some()
                    {
                        let _ = super::bar::output(
                            "niri",
                            &["msg", "action", "focus-workspace", &index.to_string()],
                        );
                    }
                });
            });
            self.workspaces.pack_start(&b, false, false, 0);
        }
        self.workspaces.show_all();
    }
    fn time(&self) {
        if let Ok(now) = glib::DateTime::now_local() {
            if let Ok(text) = now.format("%m월 %d일 %A %H:%M") {
                if self.clock.label().as_deref() != Some(text.as_str()) {
                    self.clock.set_label(&text);
                }
            }
            if let Ok(text) = now.format("%Y-%m-%d") {
                self.clock.set_tooltip_text(Some(&text));
            }
        }
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        for win in &self.windows {
            unsafe {
                win.destroy();
            }
        }
    }
}

#[allow(deprecated)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if unsafe { layer::gtk_layer_is_supported() } == 0 {
        return Err("Wayland layer-shell is required".into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}/lucy-bar.lock", env::var("XDG_RUNTIME_DIR")?))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Lucy bar is already running".into());
    }
    let css = gtk::CssProvider::new();
    css.load_from_data(include_bytes!("bar.css"))?;
    gtk::StyleContext::add_provider_for_screen(
        &gdk::Screen::default().ok_or("No screen")?,
        &css,
        800,
    );
    let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let workers = workers(tx);
    let display = gdk::Display::default().ok_or("No display")?;
    let panels = Rc::new(RefCell::new(Vec::<Panel>::new()));
    let state = Rc::new(RefCell::new(State::default()));
    let spaces = Rc::new(RefCell::new(Vec::<Value>::new()));
    let outputs = Rc::new(RefCell::new(Value::Null));
    let tray = crate::tray::Tray::new();
    let rebuild = {
        let panels = panels.clone();
        let state = state.clone();
        let spaces = spaces.clone();
        let wake = workers.wake.clone();
        let outputs = outputs.clone();
        move |display: &gdk::Display| {
            panels.borrow_mut().clear();
            for i in 0..display.n_monitors() {
                if let Some(monitor) = display.monitor(i) {
                    // GDK's Wayland monitor model is not the connector name. Match Niri logical geometry.
                    let geometry = monitor.geometry();
                    let output = {
                        let v = outputs.borrow();
                        v.as_object().and_then(|o| {
                            o.iter()
                                .find(|(_, v)| {
                                    v["logical"]["x"].as_i64() == Some(geometry.x() as i64)
                                        && v["logical"]["y"].as_i64() == Some(geometry.y() as i64)
                                })
                                .map(|(name, _)| name.clone())
                        })
                    }
                    .unwrap_or_default();
                    let p = Panel::new(&monitor, output, wake.clone(), &tray);
                    p.render(&state.borrow());
                    p.spaces(&spaces.borrow());
                    p.time();
                    panels.borrow_mut().push(p);
                }
            }
        }
    };
    rebuild(&display);
    let rebuild = Rc::new(rebuild);
    let added = rebuild.clone();
    display.connect_monitor_added(move |d, _| added(d));
    let removed = rebuild.clone();
    display.connect_monitor_removed(move |d, _| removed(d));
    let p = panels.clone();
    rx.attach(None, move |update| {
        match update {
            Update::Outputs(next) => {
                *outputs.borrow_mut() = next;
                rebuild(&display);
            }
            Update::Metrics(mut s) => {
                // General sampling must not replace an audio event with stale
                // state (or hide the audio button now owned by the watcher).
                if let Some(audio) = state.borrow().values.get("audio") {
                    s.values.insert("audio", audio.clone());
                }
                for p in p.borrow().iter() {
                    p.render(&s);
                }
                *state.borrow_mut() = s;
            }
            Update::Audio(metric) => {
                for p in p.borrow().iter() {
                    p.render_metric("audio", metric.as_ref());
                }
                let mut state = state.borrow_mut();
                if let Some(metric) = metric {
                    state.values.insert("audio", metric);
                } else {
                    state.values.remove("audio");
                }
            }
            Update::Workspaces(s) => {
                for p in p.borrow().iter() {
                    p.spaces(&s);
                }
                *spaces.borrow_mut() = s;
            }
        }
        ControlFlow::Continue
    });
    let p = panels.clone();
    let timer = glib::timeout_add_seconds_local(1, move || {
        for p in p.borrow().iter() {
            p.time();
        }
        ControlFlow::Continue
    });
    for signal in [libc::SIGTERM, libc::SIGINT] {
        glib::unix_signal_add_local(signal, || {
            gtk::main_quit();
            ControlFlow::Break
        });
    }
    gtk::main();
    timer.remove();
    panels.borrow_mut().clear();
    drop(workers);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_audio_events_exclude_unrelated_streams() {
        assert!(output_audio_event("Event 'change' on sink #42"));
        assert!(output_audio_event("Event 'change' on server #4294967295"));
        assert!(output_audio_event("Event 'new' on card #2"));
        assert!(!output_audio_event("Event 'change' on source #42"));
        assert!(!output_audio_event("Event 'change' on sink-input #42"));
        assert!(!output_audio_event("Event 'new' on client #42"));
    }

    #[test]
    #[ignore = "temporarily changes output volume by 1 percent and restores it; requires an unmuted default sink"]
    #[allow(deprecated)]
    fn live_audio_change_updates_gtk_immediately() {
        gtk::init().unwrap();
        let name = output("pactl", &["get-default-sink"]).unwrap();
        let sinks: Value =
            serde_json::from_str(&output("pactl", &["--format=json", "list", "sinks"]).unwrap())
                .unwrap();
        let sink = sinks
            .as_array()
            .unwrap()
            .iter()
            .find(|sink| sink["name"].as_str() == Some(&name))
            .unwrap();
        assert_eq!(sink["mute"].as_bool(), Some(false));
        let original: Vec<_> = sink["channel_map"]
            .as_str()
            .unwrap()
            .split(',')
            .map(|channel| {
                sink["volume"][channel]["value"]
                    .as_u64()
                    .unwrap()
                    .to_string()
            })
            .collect();
        struct Restore(String, Vec<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = Command::new("pactl")
                    .arg("set-sink-volume")
                    .arg(&self.0)
                    .args(&self.1)
                    .status();
            }
        }
        let _restore = Restore(name.clone(), original.clone());
        let current = original[0].parse::<u64>().unwrap();
        let wanted = if current < 64881 {
            current + 655
        } else {
            current.saturating_sub(655)
        };
        let expected = format!(" {:.0}%", wanted as f64 * 100.0 / 65536.0);
        let button = button("…", "audio");
        let panel = Panel {
            windows: Vec::new(),
            workspaces: gtk::Box::new(gtk::Orientation::Horizontal, 0),
            output: String::new(),
            clock: gtk::Button::new(),
            values: HashMap::from([("audio", button.clone())]),
        };
        let (updates, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let watcher = AudioWatcher::new(updates);
        let started = Rc::new(Cell::new(None::<Instant>));
        let beginning = started.clone();
        let elapsed = Rc::new(Cell::new(None::<Duration>));
        let observed = elapsed.clone();
        let setter = Rc::new(RefCell::new(None));
        let command = setter.clone();
        let native = env::var_os("LUCY_TEST_AUDIO_NATIVE").is_some();
        let source = receiver.attach(None, move |update| {
            if let Update::Audio(metric) = update {
                panel.render_metric("audio", metric.as_ref());
                if let Some(start) = beginning.get() {
                    let text = button
                        .child()
                        .unwrap()
                        .downcast::<gtk::Label>()
                        .unwrap()
                        .text();
                    if text == expected {
                        observed.set(Some(start.elapsed()));
                        gtk::main_quit();
                    }
                } else {
                    beginning.set(Some(Instant::now()));
                    let name = name.clone();
                    *command.borrow_mut() = Some(thread::spawn(move || {
                        if native {
                            Command::new("wpctl")
                                .args([
                                    "set-volume",
                                    "@DEFAULT_AUDIO_SINK@",
                                    &format!("{:.6}", wanted as f64 / 65536.0),
                                ])
                                .status()
                                .unwrap()
                        } else {
                            Command::new("pactl")
                                .args(["set-sink-volume", &name, &wanted.to_string()])
                                .status()
                                .unwrap()
                        }
                    }));
                }
            }
            ControlFlow::Continue
        });
        let timeout = glib::timeout_add_local_once(Duration::from_secs(2), gtk::main_quit);
        gtk::main();
        if let Some(timeout) = glib::MainContext::default().find_source_by_id(&timeout) {
            timeout.destroy();
        }
        glib::MainContext::default()
            .find_source_by_id(&source)
            .unwrap()
            .destroy();
        if let Some(setter) = setter.borrow_mut().take() {
            assert!(setter.join().unwrap().success());
        }
        drop(watcher);
        let elapsed = elapsed
            .get()
            .expect("audio event must update the GTK label");
        assert!(
            elapsed < Duration::from_millis(250),
            "volume update took {elapsed:?}"
        );
        println!(
            "PASS: real output volume change rendered in {:.1}ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }
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
