use glib::{variant::ObjectPath, ToVariant, Variant};
use std::{
    collections::{HashMap, HashSet},
    sync::{mpsc, Arc},
    thread,
    time::Instant,
};
#[cfg(test)]
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
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
    #[cfg_attr(not(feature = "legacy-gtk"), allow(dead_code))]
    Bluetooth(bool),
    Profile(String),
    Volume(u32),
    Mute,
    Brightness(u32),
    Media {
        player: String,
        command: &'static str,
    },
    MediaSeek {
        player: String,
        track: String,
        position: i64,
    },
}
impl Action {
    pub(crate) fn domain(&self) -> Domain {
        match self {
            Self::Bluetooth(_) => Domain::Bluetooth,
            Self::Profile(_) => Domain::Power,
            Self::Volume(_) | Self::Mute => Domain::Audio,
            Self::Brightness(_) => Domain::Brightness,
            Self::Media { .. } | Self::MediaSeek { .. } => Domain::Media,
        }
    }
    pub(crate) fn command(&self) -> Option<(&'static str, Vec<String>)> {
        let (program, args): (_, Vec<String>) = match self {
            Self::MediaSeek { .. } => return None,
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
        Some((program, args))
    }
    fn execute(&self) -> bool {
        if let Self::MediaSeek {
            player,
            track,
            position,
        } = self
        {
            let Ok(track) = ObjectPath::try_from(track.as_str()) else {
                return false;
            };
            if *position < 0 || track.as_str() == "/org/mpris/MediaPlayer2/TrackList/NoTrack" {
                return false;
            }
            return gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
                .ok()
                .is_some_and(|bus| seek(&bus, player, track, *position));
        }
        self.command().is_some_and(|(program, args)| {
            read(
                program,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .is_some()
        })
    }
}
fn seek(bus: &gio::DBusConnection, player: &str, track: ObjectPath, position: i64) -> bool {
    bus.call_sync(
        Some(&format!("org.mpris.MediaPlayer2.{player}")),
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
        "SetPosition",
        Some(&(track, position).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        800,
        gio::Cancellable::NONE,
    )
    .is_ok()
}
#[derive(Clone)]
pub(crate) struct Media {
    pub(crate) player: String,
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) art_url: String,
    pub(crate) playing: bool,
    pub(crate) duration: Option<i64>,
    pub(crate) position: Option<i64>,
    pub(crate) track: String,
    pub(crate) can_seek: bool,
    pub(crate) rate: f64,
    pub(crate) sampled: Option<Instant>,
}
impl Default for Media {
    fn default() -> Self {
        Self {
            player: String::new(),
            title: String::new(),
            artist: String::new(),
            art_url: String::new(),
            playing: false,
            duration: None,
            position: None,
            track: String::new(),
            can_seek: false,
            rate: 1.0,
            sampled: None,
        }
    }
}
impl Media {
    pub(crate) fn position_at(&self, now: Instant) -> Option<i64> {
        let base = self.position?;
        let elapsed = if self.playing {
            self.sampled
                .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64())
                * self.rate
                * 1_000_000.0
        } else {
            0.0
        };
        let value =
            (base as f64 + elapsed).clamp(0.0, self.duration.unwrap_or(i64::MAX) as f64) as i64;
        Some(value)
    }
    pub(crate) fn seekable(&self) -> bool {
        self.can_seek
            && self.duration.is_some_and(|v| v > 0)
            && self.position.is_some()
            && !self.track.is_empty()
            && self.track != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
    }
    pub(crate) fn apply_properties(&mut self, p: &HashMap<String, Variant>) {
        let metadata = p
            .get("Metadata")
            .and_then(|v| v.get::<HashMap<String, Variant>>())
            .unwrap_or_default();
        self.duration = metadata
            .get("mpris:length")
            .and_then(|v| v.get::<i64>())
            .filter(|v| *v > 0);
        self.track = metadata
            .get("mpris:trackid")
            .and_then(|v| v.get::<ObjectPath>())
            .map(String::from)
            .unwrap_or_default();
        self.position = p
            .get("Position")
            .and_then(|v| v.get::<i64>())
            .filter(|v| *v >= 0);
        self.can_seek = p
            .get("CanSeek")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        self.rate = p
            .get("Rate")
            .and_then(|v| v.get::<f64>())
            .filter(|v| v.is_finite() && *v != 0.0)
            .unwrap_or(1.0);
        if let Some(status) = p.get("PlaybackStatus").and_then(|v| v.get::<String>()) {
            self.playing = status == "Playing";
        }
        self.sampled = Some(Instant::now());
    }
}
pub(crate) fn media_time(value: Option<i64>) -> String {
    let Some(value) = value else {
        return "—:—".into();
    };
    let seconds = value.max(0) / 1_000_000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
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
    MediaPosition,
    Write(Action),
    Quit,
}
pub(crate) enum Event {
    State(Box<State>),
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
        rate: 1.0,
        ..Media::default()
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
                .and_then(|value| parse_media(&value));
            if let Some(media) = &mut state.media {
                refresh_media_position(media);
            }
        }
    }
}
fn refresh_media_position(media: &mut Media) -> bool {
    if let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        if let Ok(value) = bus.call_sync(
            Some(&format!("org.mpris.MediaPlayer2.{}", media.player)),
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&("org.mpris.MediaPlayer2.Player",).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            800,
            gio::Cancellable::NONE,
        ) {
            if let Some(props) = value.child_value(0).get::<HashMap<String, Variant>>() {
                media.apply_properties(&props);
                return true;
            }
        }
    }
    false
}

pub(crate) struct Backend {
    pub(crate) tx: mpsc::Sender<Request>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
    subscription: Option<crate::audio_backend::AudioSubscription>,
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
                let mut media_position = false;
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
                        Request::MediaPosition => media_position = true,
                        Request::Write(action) => {
                            let domain = action.domain();
                            let success = action.execute();
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
                if media_position
                    && !domains.contains(&Domain::Media)
                    && !state.media.as_mut().is_some_and(refresh_media_position)
                {
                    completed.push((Domain::Media, false));
                }
                if !domains.is_empty() || media_position {
                    let _ = events.send(Event::State(Box::new(state.clone())));
                }
                for (domain, success) in completed {
                    let _ = events.send(Event::Done(domain, success));
                }
                if quit {
                    break;
                }
            }
        });
        let notify = tx.clone();
        let subscription = crate::audio_backend::AudioSubscription::new(
            crate::audio_backend::default_output_event,
            move || notify.send(Request::Refresh(Domain::Audio)).is_ok(),
        );
        for domain in DOMAINS {
            let _ = tx.send(Request::Refresh(domain));
        }
        Self {
            tx,
            handles: vec![worker],
            subscription: Some(subscription),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.subscription.take();
        // UI flushes its sliders first. Quit is queued after those final writes.
        let _ = self.tx.send(Request::Quit);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
#[cfg(test)]
mod media_tests {
    use super::*;
    #[test]
    #[allow(deprecated)]
    fn native_seek_reaches_only_the_selected_player_on_a_private_bus() {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        let address2 = address.clone();
        let (ready, receiver) = mpsc::channel();
        let (writes, received) = mpsc::channel();
        let (stop, stopped) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let worker = thread::spawn(move || {
            let context = glib::MainContext::new();
            context.with_thread_default(||{
                let bus=gio::DBusConnection::for_address_sync(&address2,gio::DBusConnectionFlags::AUTHENTICATION_CLIENT|gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,None,gio::Cancellable::NONE).unwrap();
                bus.call_sync(Some("org.freedesktop.DBus"),"/org/freedesktop/DBus","org.freedesktop.DBus","RequestName",Some(&("org.mpris.MediaPlayer2.LucyTest",0u32).to_variant()),None,gio::DBusCallFlags::NONE,1000,gio::Cancellable::NONE).unwrap();
                let info=gio::DBusNodeInfo::for_xml(r#"<node><interface name="org.mpris.MediaPlayer2.Player"><method name="SetPosition"><arg type="o" direction="in"/><arg type="x" direction="in"/></method></interface></node>"#).unwrap();
                let registration=bus.register_object("/org/mpris/MediaPlayer2",&info.lookup_interface("org.mpris.MediaPlayer2.Player").unwrap(),move|_,_,_,_,method,args,invocation|{
                    assert_eq!(method,"SetPosition");
                    writes.send(args.get::<(ObjectPath,i64)>().unwrap()).unwrap();
                    invocation.return_value(None);
                },|_,_,_,_,_|().to_variant(),|_,_,_,_,_,_|false).unwrap();
                let loop_=glib::MainLoop::new(Some(&context),false);
                let stop_loop=loop_.clone();
                stopped.attach(Some(&context),move |_|{stop_loop.quit();glib::ControlFlow::Break});
                ready.send(()).unwrap();loop_.run();
                bus.unregister_object(registration).unwrap();
                bus.close_sync(gio::Cancellable::NONE).unwrap();
            }).unwrap();
        });
        receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        let bus = gio::DBusConnection::for_address_sync(
            &address,
            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None,
            gio::Cancellable::NONE,
        )
        .unwrap();
        let track = ObjectPath::try_from("/org/lucy/track1").unwrap();
        let success = seek(&bus, "LucyTest", track.clone(), 45_225_000);
        let call = received.recv_timeout(Duration::from_secs(1));
        let _ = stop.send(());
        worker.join().unwrap();
        bus.close_sync(gio::Cancellable::NONE).unwrap();
        let _ = daemon.kill();
        let _ = daemon.wait();
        assert!(success);
        assert_eq!(call.unwrap(), (track, 45_225_000));
    }
    #[test]
    fn mpris_properties_use_microseconds_and_track_object_paths() {
        let track = ObjectPath::try_from("/org/lucy/track1").unwrap();
        let metadata = HashMap::from([
            ("mpris:trackid".to_string(), track.to_variant()),
            ("mpris:length".to_string(), 180_000_000i64.to_variant()),
        ]);
        let mut media = Media::default();
        media.apply_properties(&HashMap::from([
            ("Metadata".to_string(), metadata.to_variant()),
            ("Position".to_string(), 30_000_000i64.to_variant()),
            ("PlaybackStatus".to_string(), "Playing".to_variant()),
            ("Rate".to_string(), 1.0f64.to_variant()),
            ("CanSeek".to_string(), true.to_variant()),
        ]));
        assert!(media.seekable());
        assert_eq!(media.track, "/org/lucy/track1");
        assert_eq!(
            media.position_at(media.sampled.unwrap() + Duration::from_secs(2)),
            Some(32_000_000)
        );
        media.playing = false;
        assert_eq!(
            media.position_at(Instant::now() + Duration::from_secs(9)),
            Some(30_000_000)
        );
        assert_eq!((track, 45_000_001i64).to_variant().type_().as_str(), "(ox)");
    }
    #[test]
    fn playback_clamps_to_duration_and_unknown_tracks_are_not_seekable() {
        let mut media = Media {
            duration: Some(90_000_000),
            position: Some(89_000_000),
            playing: true,
            sampled: Some(Instant::now()),
            ..Default::default()
        };
        assert_eq!(
            media.position_at(media.sampled.unwrap() + Duration::from_secs(9)),
            Some(90_000_000)
        );
        assert!(!media.seekable());
        media.can_seek = true;
        media.track = "/org/mpris/MediaPlayer2/TrackList/NoTrack".into();
        assert!(!media.seekable());
        assert_eq!(media_time(Some(3_665_000_000)), "1:01:05");
        assert_eq!(media_time(None), "—:—");
    }
}
