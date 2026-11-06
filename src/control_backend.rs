use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Domain {
    Wifi,
    Bluetooth,
    Power,
    Audio,
    Brightness,
    Media,
}
pub(crate) const DOMAINS: [Domain; 6] = [
    Domain::Wifi,
    Domain::Bluetooth,
    Domain::Power,
    Domain::Audio,
    Domain::Brightness,
    Domain::Media,
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
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
    pub(crate) fn domain(&self) -> Domain {
        match self {
            Self::Bluetooth(_) => Domain::Bluetooth,
            Self::Profile(_) => Domain::Power,
            Self::Volume(_) | Self::Mute => Domain::Audio,
            Self::Brightness(_) => Domain::Brightness,
            Self::Media { .. } => Domain::Media,
        }
    }
    pub(crate) fn command(&self) -> (&'static str, Vec<String>) {
        let (program, args): (_, Vec<String>) = match self {
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
pub(crate) struct Media {
    pub(crate) player: String,
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) art_url: String,
    pub(crate) playing: bool,
}
#[derive(Clone, Default)]
pub(crate) struct State {
    pub(crate) wifi: Option<bool>,
    pub(crate) bluetooth: Option<bool>,
    pub(crate) bluetooth_devices: Arc<Vec<String>>,
    pub(crate) profile: Option<String>,
    pub(crate) profiles: Vec<String>,
    pub(crate) audio: Option<(f64, bool)>,
    pub(crate) brightness: Option<f64>,
    pub(crate) media: Option<Media>,
}
pub(crate) enum Request {
    Refresh(Domain),
    Write(Action),
    Quit,
}
pub(crate) enum Event {
    State(State),
    Done(Domain, bool),
}

pub(crate) const MEDIA_FORMAT: &str =
    "{{playerInstance}}\n{{status}}\n{{title}}\n{{artist}}\n{{mpris:artUrl}}";
pub(crate) fn parse_media(value: &str) -> Option<Media> {
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

pub(crate) fn read(program: &str, args: &[&str]) -> Option<String> {
    crate::bar_backend::output(program, args)
}
pub(crate) fn refresh(state: &mut State, domain: Domain) {
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
            });
            state.bluetooth_devices = if state.bluetooth == Some(true) {
                read("bluetoothctl", &["devices", "Connected"])
                    .map(|value| {
                        Arc::new(
                            value
                                .lines()
                                .filter_map(|line| {
                                    line.strip_prefix("Device ")
                                        .and_then(|v| v.split_once(' '))
                                        .map(|(_, name)| name.to_owned())
                                })
                                .collect(),
                        )
                    })
                    .unwrap_or_default()
            } else {
                Default::default()
            };
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

pub(crate) struct Backend {
    pub(crate) tx: mpsc::Sender<Request>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) child: Arc<Mutex<Option<Child>>>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
}
impl Backend {
    pub(crate) fn start(events: glib::Sender<Event>) -> Self {
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
