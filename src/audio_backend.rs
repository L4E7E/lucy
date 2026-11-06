use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
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
    pub(crate) subscriber: Arc<Mutex<Option<Child>>>,
    pub(crate) handles: Vec<thread::JoinHandle<()>>,
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
        let subscriber = Arc::new(Mutex::new(None::<Child>));
        let shared = subscriber.clone();
        let sub_stop = stop.clone();
        let sub_tx = tx.clone();
        let sub = thread::spawn(move || {
            let mut child = match Command::new("pactl")
                .arg("subscribe")
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
                let mut holder = shared.lock().unwrap();
                if sub_stop.load(Ordering::Acquire) {
                    let _ = child.kill();
                }
                *holder = Some(child);
            }
            for line in BufReader::new(stdout).lines() {
                if sub_stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(line) = line else { break };
                if ["sink", "source", "server", "card"]
                    .iter()
                    .any(|word| line.contains(word))
                {
                    let _ = sub_tx.send(Request::Refresh);
                }
            }
            if let Some(mut child) = shared.lock().unwrap().take() {
                let _ = child.wait();
            }
        });
        let _ = tx.send(Request::Refresh);
        Self {
            tx,
            stop,
            subscriber,
            handles: vec![worker, sub],
        }
    }
    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Request::Quit);
        if let Some(child) = self.subscriber.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
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
