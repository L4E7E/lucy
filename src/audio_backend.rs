use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
pub(crate) const KINDS: [(&str, &str, &str); 4] = [
    ("sink", "sinks", "출력"),
    ("source", "sources", "마이크"),
    ("sink-input", "sink-inputs", "앱별 재생"),
    ("source-output", "source-outputs", "앱별 녹음"),
];

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn volume(item: &Value) -> f64 {
    let Some(channels) = item["volume"].as_object() else {
        return 0.0;
    };
    let total: f64 = channels
        .values()
        .map(|v| v["value"].as_f64().unwrap_or(0.0))
        .sum();
    total / channels.len().max(1) as f64 / 65536.0 * 100.0
}
pub(crate) fn description(item: &Value) -> String {
    for candidate in [
        text(item, "description"),
        text(&item["properties"], "application.name"),
        text(&item["properties"], "media.name"),
        text(item, "name"),
    ] {
        if !candidate.is_empty() {
            return candidate.replace('\0', "");
        }
    }
    item["index"].to_string()
}
pub(crate) fn volume_command(kind: &str, index: u64, percentage: u32) -> Vec<String> {
    vec![
        format!("set-{kind}-volume"),
        index.to_string(),
        format!("{}%", percentage.min(100)),
    ]
}
pub(crate) fn pactl(args: &[String]) -> Result<Vec<u8>, String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(output.stdout)
}
pub(crate) fn snapshot() -> Result<Value, String> {
    let mut state = serde_json::Map::new();
    for kind in ["info", "sinks", "sources", "sink-inputs", "source-outputs"] {
        let mut args = vec!["--format=json".into()];
        if kind != "info" {
            args.push("list".into());
        }
        args.push(kind.into());
        let bytes = pactl(&args)?;
        state.insert(
            kind.into(),
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
        );
    }
    Ok(Value::Object(state))
}

pub(crate) fn default_output_event(line: &str) -> bool {
    line.contains(" on sink #") || line.contains(" on server #")
}

fn mixer_audio_event(line: &str) -> bool {
    ["sink", "source", "server", "card"]
        .iter()
        .any(|word| line.contains(word))
}

struct AudioListener {
    filter: fn(&str) -> bool,
    // Only enqueue work here: snapshots and coalescing belong to the consumers.
    notify: Box<dyn Fn() -> bool + Send>,
}
type AudioListeners = Arc<Mutex<HashMap<u64, AudioListener>>>;

#[derive(Default)]
struct AudioSubscriptions {
    listeners: AudioListeners,
    next_id: u64,
    worker: Option<AudioEventWorker>,
}

/// One blocking pactl event stream per process, owned by its active consumers.
pub(crate) struct AudioSubscription {
    shared: Arc<Mutex<AudioSubscriptions>>,
    id: u64,
}
impl AudioSubscription {
    pub(crate) fn new(
        filter: fn(&str) -> bool,
        notify: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        static SHARED: OnceLock<Arc<Mutex<AudioSubscriptions>>> = OnceLock::new();
        Self::subscribe(
            SHARED.get_or_init(Default::default).clone(),
            filter,
            notify,
            || {
                Command::new("pactl")
                    .arg("subscribe")
                    .env("LC_ALL", "C")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
            },
            Duration::from_secs(1),
        )
    }

    fn subscribe(
        shared: Arc<Mutex<AudioSubscriptions>>,
        filter: fn(&str) -> bool,
        notify: impl Fn() -> bool + Send + 'static,
        spawn: impl Fn() -> std::io::Result<Child> + Send + 'static,
        retry: Duration,
    ) -> Self {
        let mut subscriptions = shared.lock().unwrap();
        let id = subscriptions.next_id;
        subscriptions.next_id += 1;
        {
            let mut listeners = subscriptions.listeners.lock().unwrap();
            let listener = AudioListener {
                filter,
                notify: Box::new(notify),
            };
            // A late consumer needs its own initial snapshot even on an idle stream.
            if subscriptions.worker.is_some() {
                (listener.notify)();
            }
            listeners.insert(id, listener);
        }
        if subscriptions.worker.is_none() {
            subscriptions.worker = Some(AudioEventWorker::start(
                subscriptions.listeners.clone(),
                spawn,
                retry,
            ));
        }
        drop(subscriptions);
        Self { shared, id }
    }
}
impl Drop for AudioSubscription {
    fn drop(&mut self) {
        let mut subscriptions = self.shared.lock().unwrap();
        let empty = {
            let mut listeners = subscriptions.listeners.lock().unwrap();
            listeners.remove(&self.id);
            listeners.is_empty()
        };
        if empty {
            // Serialize the final reap with a new subscription: streams never overlap.
            drop(subscriptions.worker.take());
        }
    }
}

fn notify_audio(listeners: &AudioListeners, line: Option<&str>) {
    listeners.lock().unwrap().retain(|_, listener| {
        line.is_some_and(|line| !(listener.filter)(line)) || (listener.notify)()
    });
}

struct AudioEventWorker {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}
impl AudioEventWorker {
    fn start(
        listeners: AudioListeners,
        spawn: impl Fn() -> std::io::Result<Child> + Send + 'static,
        retry: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None::<Child>));
        let (cancel, cancelled) = mpsc::channel();
        let stopping = stop.clone();
        let holder = child.clone();
        let handle = thread::Builder::new()
            .name("lucy-audio-events".into())
            .spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    if let Ok(mut child) = spawn() {
                        let stdout = child.stdout.take().expect("pactl stdout");
                        {
                            let mut slot = holder.lock().unwrap();
                            if stopping.load(Ordering::Acquire) {
                                let _ = child.kill();
                            }
                            *slot = Some(child);
                        }
                        // Refresh cached state when the reader starts or reconnects.
                        notify_audio(&listeners, None);
                        for line in BufReader::new(stdout).lines() {
                            if stopping.load(Ordering::Acquire) {
                                break;
                            }
                            let Ok(line) = line else { break };
                            notify_audio(&listeners, Some(&line));
                        }
                        let child = holder.lock().unwrap().take();
                        if let Some(mut child) = child {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    notify_audio(&listeners, None);
                    if cancelled.recv_timeout(retry).is_ok() {
                        break;
                    }
                }
            })
            .expect("audio event worker");
        Self {
            stop,
            child,
            cancel,
            handle: Some(handle),
        }
    }
}
impl Drop for AudioEventWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.cancel.send(());
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
pub(crate) enum Request {
    Refresh,
    Write(Vec<String>),
    Quit,
}
pub(crate) enum Event {
    State(Value),
    Error(String),
}
pub(crate) struct Backend {
    pub(crate) tx: mpsc::Sender<Request>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
    subscription: Option<AudioSubscription>,
}
impl Backend {
    #[allow(deprecated)] // gtk3-rs 0.18 main-context channel, UI objects never cross threads.
    pub(crate) fn start(events: glib::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_events = events.clone();
        let worker = thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                if matches!(first, Request::Refresh) {
                    // Coalesce bursts of PipeWire device/volume events, not periodic polling.
                    thread::sleep(Duration::from_millis(60));
                }
                let mut requests = vec![first];
                requests.extend(rx.try_iter());
                let mut refresh = false;
                for request in requests {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match request {
                        Request::Quit => return,
                        Request::Refresh => refresh = true,
                        Request::Write(args) => {
                            if let Err(e) = pactl(&args) {
                                let _ = worker_events.send(Event::Error(e));
                            }
                            refresh = true;
                        }
                    }
                }
                if refresh {
                    match snapshot() {
                        Ok(state) => {
                            let _ = worker_events.send(Event::State(state));
                        }
                        Err(e) => {
                            let _ = worker_events.send(Event::Error(e));
                        }
                    }
                }
            }
        });
        let sub_tx = tx.clone();
        let subscription = AudioSubscription::new(mixer_audio_event, move || {
            sub_tx.send(Request::Refresh).is_ok()
        });
        Self {
            tx,
            stop,
            handles: vec![worker],
            subscription: Some(subscription),
        }
    }
    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.subscription.take();
        let _ = self.tx.send(Request::Quit);
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_handles_channels_missing_and_boost() {
        assert_eq!(volume(&serde_json::json!({})), 0.0);
        assert_eq!(
            volume(&serde_json::json!({"volume":{"L":{"value":32768},"R":{"value":65536}}})),
            75.0
        );
        assert_eq!(
            volume(&serde_json::json!({"volume":{"L":{"value":98304}}})),
            150.0
        );
    }
    #[test]
    fn commands_are_bounded_argv_not_shell() {
        assert_eq!(
            volume_command("source", 42, 150),
            ["set-source-volume", "42", "100%"]
        );
        assert_eq!(
            volume_command("sink-input", 2, 0),
            ["set-sink-input-volume", "2", "0%"]
        );
    }
    #[test]
    fn description_prefers_device_then_application() {
        assert_eq!(
            description(
                &serde_json::json!({"description":"device","properties":{"application.name":"app"}})
            ),
            "device"
        );
        assert_eq!(
            description(&serde_json::json!({"properties":{"application.name":"app"}})),
            "app"
        );
    }
}

#[cfg(test)]
mod subscription_tests {
    use super::*;
    use std::{io::Write, process::ChildStdin, time::Instant};

    type Spawned = (u32, ChildStdin);

    fn fake_spawn(
        starts: Arc<std::sync::atomic::AtomicUsize>,
        spawned: mpsc::Sender<Spawned>,
    ) -> impl Fn() -> std::io::Result<Child> + Send {
        move || {
            // A pipe-only event source: no PulseAudio connection or environment changes.
            let mut child = Command::new("/bin/sh")
                .args([
                    "-c",
                    "while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            starts.fetch_add(1, Ordering::SeqCst);
            let input = child.stdin.take().unwrap();
            if spawned.send((child.id(), input)).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::other("test receiver closed"));
            }
            Ok(child)
        }
    }

    fn receive(rx: &mpsc::Receiver<usize>, expected: &[usize]) {
        let mut actual: Vec<_> = expected
            .iter()
            .map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    fn assert_reaped(pid: u32) {
        // Only inspect the fake child owned by this test. ECHILD proves wait() reaped it.
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn filters_preserve_bar_control_and_mixer_event_scopes() {
        let listeners = AudioListeners::default();
        let (tx, rx) = mpsc::channel();
        let filters: [fn(&str) -> bool; 3] = [
            crate::bar_backend::output_audio_event,
            default_output_event,
            mixer_audio_event,
        ];
        for (id, filter) in filters.into_iter().enumerate() {
            let tx = tx.clone();
            listeners.lock().unwrap().insert(
                id as u64,
                AudioListener {
                    filter,
                    notify: Box::new(move || tx.send(id).is_ok()),
                },
            );
        }
        for (line, expected) in [
            ("Event 'change' on sink #42", &[0, 1, 2][..]),
            ("Event 'change' on server #4294967295", &[0, 1, 2][..]),
            ("Event 'new' on card #2", &[0, 2][..]),
            ("Event 'change' on source #42", &[2][..]),
            ("Event 'new' on sink-input #42", &[2][..]),
            ("Event 'remove' on source-output #42", &[2][..]),
            ("Event 'new' on client #42", &[][..]),
        ] {
            notify_audio(&listeners, Some(line));
            receive(&rx, expected);
            assert!(rx.try_recv().is_err(), "unrelated refresh for {line}");
        }
        notify_audio(&listeners, None);
        receive(&rx, &[0, 1, 2]);
    }

    #[test]
    fn three_consumers_share_one_stream_until_the_last_drop() {
        let shared = Arc::new(Mutex::new(AudioSubscriptions::default()));
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (spawned, children) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let first_tx = tx.clone();
        let first = AudioSubscription::subscribe(
            shared.clone(),
            crate::bar_backend::output_audio_event,
            move || first_tx.send(0).is_ok(),
            fake_spawn(starts.clone(), spawned.clone()),
            Duration::from_secs(1),
        );
        let (pid, mut input) = children.recv_timeout(Duration::from_secs(2)).unwrap();
        receive(&rx, &[0]);
        let second_tx = tx.clone();
        let second = AudioSubscription::subscribe(
            shared.clone(),
            default_output_event,
            move || second_tx.send(1).is_ok(),
            || panic!("late consumer must reuse the existing child"),
            Duration::from_secs(1),
        );
        receive(&rx, &[1]); // An idle stream still initializes a late consumer immediately.
        let third = AudioSubscription::subscribe(
            shared.clone(),
            mixer_audio_event,
            move || tx.send(2).is_ok(),
            || panic!("third consumer must reuse the existing child"),
            Duration::from_secs(1),
        );
        receive(&rx, &[2]);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        writeln!(input, "Event 'change' on sink #42").unwrap();
        receive(&rx, &[0, 1, 2]);
        drop(second);
        writeln!(input, "Event 'change' on sink #42").unwrap();
        receive(&rx, &[0, 2]);
        drop(first);
        writeln!(input, "Event 'change' on sink #42").unwrap();
        receive(&rx, &[2]);
        assert!(rx.try_recv().is_err());
        drop(third); // stdin remains open: shutdown must interrupt the blocking reader.
        assert!(shared.lock().unwrap().worker.is_none());
        assert_reaped(pid);

        let (tx, rx) = mpsc::channel();
        let restarted = AudioSubscription::subscribe(
            shared.clone(),
            default_output_event,
            move || tx.send(0).is_ok(),
            fake_spawn(starts.clone(), spawned),
            Duration::from_secs(1),
        );
        let (pid, _input) = children.recv_timeout(Duration::from_secs(2)).unwrap();
        receive(&rx, &[0]);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        drop(restarted);
        assert_reaped(pid);
    }

    #[test]
    fn reconnect_refreshes_each_consumer_and_restores_event_delivery() {
        let shared = Arc::new(Mutex::new(AudioSubscriptions::default()));
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (spawned, children) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let first_tx = tx.clone();
        let first = AudioSubscription::subscribe(
            shared.clone(),
            default_output_event,
            move || first_tx.send(0).is_ok(),
            fake_spawn(starts.clone(), spawned),
            Duration::from_millis(5),
        );
        let (old_pid, input) = children.recv_timeout(Duration::from_secs(2)).unwrap();
        receive(&rx, &[0]);
        let second = AudioSubscription::subscribe(
            shared,
            mixer_audio_event,
            move || tx.send(1).is_ok(),
            || panic!("second consumer must not spawn"),
            Duration::from_millis(5),
        );
        receive(&rx, &[1]);
        drop(input); // EOF simulates a server disconnect.
        let (pid, mut input) = children.recv_timeout(Duration::from_secs(2)).unwrap();
        receive(&rx, &[0, 0, 1, 1]); // Both disconnect and reconnect refresh both consumers.
        assert_reaped(old_pid);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        writeln!(input, "Event 'change' on source #42").unwrap();
        receive(&rx, &[1]);
        writeln!(input, "Event 'change' on sink #42").unwrap();
        receive(&rx, &[0, 1]);
        drop(first);
        drop(second);
        assert_reaped(pid);
    }

    #[test]
    fn shutdown_interrupts_failed_spawn_retry() {
        let shared = Arc::new(Mutex::new(AudioSubscriptions::default()));
        let (tx, rx) = mpsc::channel();
        let subscription = AudioSubscription::subscribe(
            shared.clone(),
            default_output_event,
            move || tx.send(0).is_ok(),
            || Err(std::io::Error::other("fake subscriber unavailable")),
            Duration::from_secs(60),
        );
        receive(&rx, &[0]);
        let start = Instant::now();
        drop(subscription);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(shared.lock().unwrap().worker.is_none());
    }

    #[test]
    fn shutdown_during_spawn_still_reaps_the_new_child() {
        let shared = Arc::new(Mutex::new(AudioSubscriptions::default()));
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (spawned, children) = mpsc::channel();
        let spawn = fake_spawn(starts, spawned);
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let subscription = AudioSubscription::subscribe(
            shared.clone(),
            default_output_event,
            || true,
            move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                spawn()
            },
            Duration::from_secs(1),
        );
        entering.recv_timeout(Duration::from_secs(2)).unwrap();
        let stopping = shared.lock().unwrap().worker.as_ref().unwrap().stop.clone();
        let shutdown = thread::spawn(move || drop(subscription));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !stopping.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "shutdown failed to signal the worker"
            );
            thread::yield_now();
        }
        release.send(()).unwrap();
        let (pid, _input) = children.recv_timeout(Duration::from_secs(2)).unwrap();
        shutdown.join().unwrap();
        assert_reaped(pid);
        assert!(shared.lock().unwrap().worker.is_none());
    }
}
