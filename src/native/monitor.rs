//! On-demand, read-only telemetry. Never polls or holds GPU handles while hidden.
use crate::bar_backend as bar;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
#[path = "monitor_nvml.rs"]
mod nvml;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}
impl Usage {
    pub fn new(used: u64, total: u64) -> Option<Self> {
        (total > 0).then_some(Self {
            used: used.min(total),
            total,
        })
    }
    pub fn percent(self) -> f64 {
        100.0 * self.used as f64 / self.total.max(1) as f64
    }
    pub fn text(self) -> String {
        let unit = if self.total < 1024 * 1024 * 1024 {
            "MiB"
        } else {
            "GiB"
        };
        let divisor = if unit == "MiB" {
            1048576.0
        } else {
            1073741824.0
        };
        format!(
            "{:.1} / {:.1} {unit}",
            self.used as f64 / divisor,
            self.total as f64 / divisor
        )
    }
}
#[derive(Clone, Debug)]
pub struct Cpu {
    pub id: u32,
    pub usage: Option<f64>,
}
#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub id: String,
    pub name: String,
    pub integrated: bool,
    pub sleeping: bool,
    pub usage: Option<f64>,
    pub temperature: Option<f64>,
    pub vram: Option<Usage>,
}
#[derive(Clone, Debug, Default)]
pub struct Disk {
    pub name: String,
    pub mount: String,
    pub usage: Option<Usage>,
}
#[derive(Clone, Debug, Default)]
pub struct State {
    pub ready: bool,
    pub sequence: u64,
    pub cpu_name: String,
    pub cpu: Option<f64>,
    pub threads: Vec<Cpu>,
    pub temperature: Option<f64>,
    pub gpus: Vec<Gpu>,
    pub memory: Option<Usage>,
    pub disk: Disk,
}
impl State {
    pub fn status(&self, active: bool) -> serde_json::Value {
        let usage = |u: Usage| serde_json::json!({"used":u.used,"total":u.total});
        serde_json::json!({
            "ready":self.ready,"sequence":self.sequence,"active":active,
            "cpu_name":self.cpu_name,"cpu":self.cpu,"temperature":self.temperature,
            "threads":self.threads.iter().map(|t|serde_json::json!({"id":t.id,"usage":t.usage})).collect::<Vec<_>>(),
            "memory":self.memory.map(usage),
            "disk":{"name":self.disk.name,"mount":self.disk.mount,"usage":self.disk.usage.map(usage)},
            "gpus":self.gpus.iter().map(|g|serde_json::json!({"id":g.id,"name":g.name,"integrated":g.integrated,"sleeping":g.sleeping,"usage":g.usage,"temperature":g.temperature,"vram":g.vram.map(usage)})).collect::<Vec<_>>()
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Request {
    Visible(bool),
    Stop,
}
pub struct Worker {
    pub tx: mpsc::Sender<Request>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub fn new(updates: glib::Sender<State>) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("lucy-monitor".into())
            .spawn(move || {
                run_worker(
                    rx,
                    Duration::from_millis(500),
                    Sampler::new,
                    Sampler::sample,
                    |state| updates.send(state).is_ok(),
                );
            })
            .expect("monitor worker");
        Self {
            tx,
            handle: Some(handle),
        }
    }
}

fn run_worker<S>(
    rx: mpsc::Receiver<Request>,
    interval: Duration,
    mut create: impl FnMut() -> S,
    mut sample: impl FnMut(&mut S) -> State,
    mut publish: impl FnMut(State) -> bool,
) {
    let mut sampler = None;
    let mut sequence = 0;
    loop {
        let mut request = if sampler.is_some() {
            match rx.recv_timeout(interval) {
                Ok(r) => Some(r),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            }
        } else {
            match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => break,
            }
        };
        for queued in rx.try_iter() {
            if matches!(request, Some(Request::Stop)) {
                break;
            }
            request = Some(queued);
        }
        match request {
            Some(Request::Stop) => break,
            Some(Request::Visible(false)) => {
                sampler = None;
                continue;
            }
            Some(Request::Visible(true)) => {
                if sampler.is_some() {
                    continue;
                }
                sampler = Some(create());
            }
            None => {}
        }
        if let Some(sampler) = &mut sampler {
            sequence += 1;
            let mut state = sample(sampler);
            state.sequence = sequence;
            if !publish(state) {
                break;
            }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

type Times = (u64, u64);
fn times(text: &str) -> BTreeMap<String, Times> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let key = fields.next()?;
            if key != "cpu"
                && key
                    .strip_prefix("cpu")
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_none()
            {
                return None;
            }
            // guest/guest_nice are already included in user/nice; do not count twice.
            let fields = fields
                .take(8)
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            if fields.len() < 4 {
                return None;
            }
            let total = fields.iter().try_fold(0u64, |a, b| a.checked_add(*b))?;
            Some((
                key.to_owned(),
                (total, fields[3].checked_add(*fields.get(4).unwrap_or(&0))?),
            ))
        })
        .collect()
}
fn percent(previous: Option<&Times>, current: Times) -> Option<f64> {
    let previous = previous?;
    let total = current.0.checked_sub(previous.0)?;
    let idle = current.1.checked_sub(previous.1)?;
    (total > 0 && idle <= total).then(|| 100.0 * (total - idle) as f64 / total as f64)
}
fn temperature(path: &Path) -> Option<f64> {
    let value: f64 = bar::read(path).parse().ok()?;
    let value = value / 1000.0;
    (value.is_finite() && (-20.0..=150.0).contains(&value)).then_some(value)
}
fn cpu_sensor() -> Option<PathBuf> {
    fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .find_map(|e| {
            matches!(
                bar::read(e.path().join("name")).as_str(),
                "k10temp" | "coretemp"
            )
            .then(|| e.path().join("temp1_input"))
        })
}
fn mount_name(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            if left.split_whitespace().nth(4)? != "/" {
                return None;
            }
            Some(match right.split_whitespace().next()? {
                "btrfs" => "Btrfs".to_owned(),
                "ext4" => "Ext4".to_owned(),
                "xfs" => "XFS".to_owned(),
                other => other.to_owned(),
            })
        })
        .unwrap_or_default()
}
fn disk_usage(path: &str) -> Option<Usage> {
    let path = CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: NUL-terminated path and valid writable statvfs storage.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Usage::new(
        stat.f_blocks
            .checked_sub(stat.f_bfree)?
            .checked_mul(stat.f_frsize)?,
        stat.f_blocks.checked_mul(stat.f_frsize)?,
    )
}
struct Device {
    path: PathBuf,
    pci: String,
    vendor: String,
    name: String,
    integrated: bool,
    sensor: Option<PathBuf>,
}
fn pci_name(vendor: &str, device: &str, text: &str) -> Option<String> {
    let mut selected = false;
    for line in text.lines().filter(|s| !s.starts_with('#')) {
        if !line.starts_with(char::is_whitespace) {
            selected = line.split_whitespace().next() == Some(vendor);
        } else if selected && line.starts_with('\t') && !line.starts_with("\t\t") {
            let (id, name) = line.trim().split_once(char::is_whitespace)?;
            if id == device {
                return Some(name.trim().to_owned());
            }
        }
    }
    None
}
fn amd_name(device: &str, revision: &str, text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let fields: Vec<_> = line.splitn(3, ',').map(str::trim).collect();
        (fields.len() == 3
            && fields[0].eq_ignore_ascii_case(device)
            && fields[1].eq_ignore_ascii_case(revision))
        .then(|| fields[2].to_owned())
    })
}
fn devices() -> Vec<Device> {
    let pci_ids = bar::read("/usr/share/hwdata/pci.ids");
    let amd_ids = bar::read("/usr/share/libdrm/amdgpu.ids");
    let mut devices: Vec<_> = fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix("card")?
                .parse::<u32>()
                .ok()?;
            let path = entry.path().join("device");
            let real = fs::canonicalize(&path).ok()?;
            let pci = real.file_name()?.to_str()?.to_owned();
            let vendor = bar::read(path.join("vendor"))
                .trim_start_matches("0x")
                .to_owned();
            let device = bar::read(path.join("device"))
                .trim_start_matches("0x")
                .to_owned();
            let revision = bar::read(path.join("revision"))
                .trim_start_matches("0x")
                .to_owned();
            let name = if vendor == "1002" {
                amd_name(&device, &revision, &amd_ids)
            } else {
                None
            }
            .or_else(|| pci_name(&vendor, &device, &pci_ids))
            .unwrap_or_else(|| format!("GPU {vendor}:{device}"));
            let sensor = fs::read_dir(path.join("hwmon"))
                .ok()
                .and_then(|dirs| dirs.flatten().next())
                .map(|d| d.path().join("temp1_input"));
            // AMD APUs expose shared GTT alongside a small firmware-reserved VRAM pool.
            let integrated = vendor == "1002"
                && query_allowed(&bar::read(path.join("power/runtime_status")))
                && bar::number(path.join("mem_info_vram_total"))
                    .is_some_and(|n| n <= 1024 * 1024 * 1024);
            Some(Device {
                path,
                pci,
                vendor,
                name,
                integrated,
                sensor,
            })
        })
        .collect();
    devices.sort_by(|a, b| a.pci.cmp(&b.pci));
    devices
}
fn query_allowed(status: &str) -> bool {
    status.trim() == "active"
}
struct Sampler {
    previous: BTreeMap<String, Times>,
    devices: Vec<Device>,
    nvml: Option<nvml::Nvml>,
    nvml_attempted: bool,
    sensor: Option<PathBuf>,
    state: State,
    slow: Option<Instant>,
}
impl Sampler {
    fn new() -> Self {
        Self {
            previous: BTreeMap::new(),
            devices: devices(),
            nvml: None,
            nvml_attempted: false,
            sensor: cpu_sensor(),
            slow: None,
            state: State {
                cpu_name: bar::read("/proc/cpuinfo")
                    .lines()
                    .find_map(|l| {
                        l.split_once(':')
                            .filter(|(k, _)| k.trim() == "model name")
                            .map(|(_, v)| v.trim().to_owned())
                    })
                    .unwrap_or_default(),
                disk: Disk {
                    name: mount_name(&bar::read("/proc/self/mountinfo")),
                    mount: "/".into(),
                    usage: None,
                },
                ..State::default()
            },
        }
    }
    fn sample(&mut self) -> State {
        let current = times(&bar::read("/proc/stat"));
        self.state.cpu = current
            .get("cpu")
            .and_then(|t| percent(self.previous.get("cpu"), *t));
        self.state.threads = current
            .iter()
            .filter_map(|(key, time)| {
                Some(Cpu {
                    id: key.strip_prefix("cpu")?.parse().ok()?,
                    usage: percent(self.previous.get(key), *time),
                })
            })
            .collect();
        self.state.threads.sort_by_key(|c| c.id);
        self.previous = current;
        self.state.temperature = self.sensor.as_deref().and_then(temperature);
        self.state.gpus.clear();
        for device in &mut self.devices {
            let status = bar::read(device.path.join("power/runtime_status"));
            let mut gpu = Gpu {
                id: device.pci.clone(),
                name: device.name.clone(),
                integrated: device.integrated,
                sleeping: matches!(status.as_str(), "suspended" | "suspending"),
                ..Gpu::default()
            };
            if query_allowed(&status) {
                if device.vendor == "10de" {
                    if !self.nvml_attempted {
                        self.nvml = nvml::Nvml::load();
                        self.nvml_attempted = true;
                    }
                    if let Some(nvml) = &self.nvml {
                        nvml.sample(&device.pci, &mut gpu);
                    }
                    if gpu.name != device.name {
                        device.name.clone_from(&gpu.name);
                    }
                } else {
                    gpu.usage = bar::number(device.path.join("gpu_busy_percent"))
                        .filter(|n| *n <= 100)
                        .map(|n| n as f64);
                    gpu.temperature = device.sensor.as_deref().and_then(temperature);
                    gpu.vram = bar::number(device.path.join("mem_info_vram_used"))
                        .zip(bar::number(device.path.join("mem_info_vram_total")))
                        .and_then(|(u, t)| Usage::new(u, t));
                }
            }
            self.state.gpus.push(gpu);
        }
        if self
            .slow
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.state.memory = bar::memory(&bar::read("/proc/meminfo"))
                .and_then(|(u, t)| Usage::new(u.checked_mul(1024)?, t.checked_mul(1024)?));
            self.state.disk.usage = disk_usage("/");
            self.slow = Some(Instant::now());
        }
        self.state.ready = true;
        self.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_worker_drops_collectors_and_waits_without_polling() {
        struct Collector(mpsc::Sender<&'static str>);
        impl Drop for Collector {
            fn drop(&mut self) {
                let _ = self.0.send("released");
            }
        }
        let (requests, rx) = mpsc::channel();
        let (events, observed) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_worker(
                rx,
                Duration::from_millis(10),
                || Collector(events.clone()),
                |_| {
                    events.send("sample").unwrap();
                    State::default()
                },
                |_| true,
            )
        });
        assert!(observed.recv_timeout(Duration::from_millis(35)).is_err());
        requests.send(Request::Visible(true)).unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            "sample"
        );
        requests.send(Request::Visible(false)).unwrap();
        while observed.recv_timeout(Duration::from_secs(1)).unwrap() != "released" {}
        assert!(
            observed.recv_timeout(Duration::from_millis(35)).is_err(),
            "no hidden samples or retained GPU handles"
        );
        requests.send(Request::Visible(true)).unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            "sample"
        );
        requests.send(Request::Stop).unwrap();
        worker.join().unwrap();
        assert_eq!(observed.try_iter().last(), Some("released"));
    }
    #[test]
    fn per_thread_counters_are_dynamic_and_do_not_double_count_guest() {
        let old =
            times("cpu 10 0 10 80 0 0 0 0 100\ncpu0 5 0 5 40 0 0 0 0\ncpu12 5 0 5 40 0\nintr 100");
        let new = times(
            "cpu 30 0 30 140 0 0 0 0 900\ncpu0 15 0 15 70 0\ncpu12 15 0 15 70 0\ncpu15 3 0 2 5 0",
        );
        assert_eq!(old["cpu"], (100, 80));
        assert_eq!(percent(old.get("cpu"), new["cpu"]), Some(40.0));
        assert_eq!(percent(old.get("cpu0"), new["cpu0"]), Some(40.0));
        assert_eq!(percent(old.get("cpu15"), new["cpu15"]), None);
        assert_eq!(percent(Some(&(200, 100)), (100, 50)), None);
        assert_eq!(percent(Some(&(100, 50)), (100, 50)), None);
        assert!(!old.contains_key("intr"));
    }
    #[test]
    fn unavailable_or_zero_total_is_not_fabricated_as_zero_usage() {
        assert_eq!(Usage::new(2, 0), None);
        assert_eq!(Usage::new(20, 10).unwrap().percent(), 100.0);
        assert_eq!(State::default().cpu, None);
        assert_eq!(Gpu::default().vram, None);
        assert!(Usage::new(128 << 20, 512 << 20)
            .unwrap()
            .text()
            .ends_with("MiB"));
        assert!(Usage::new(4 << 30, 8 << 30)
            .unwrap()
            .text()
            .ends_with("GiB"));
    }
    #[test]
    fn sleeping_or_transitioning_gpus_are_never_queried() {
        assert!(query_allowed("active\n"));
        for status in ["suspended", "suspending", "resuming", "", "unsupported"] {
            assert!(!query_allowed(status));
        }
    }
    #[test]
    fn filesystem_and_gpu_labels_come_from_actual_metadata() {
        assert_eq!(
            mount_name("35 24 0:32 /@ / rw - btrfs /dev/nvme0n1p2 rw"),
            "Btrfs"
        );
        assert_eq!(mount_name("35 24 0:32 / /tmp rw - tmpfs tmpfs rw"), "");
        assert_eq!(
            pci_name(
                "10de",
                "2d59",
                "1002  AMD\n\t1900  APU\n10de  NVIDIA\n\t2d59  GPU name\n\t\t1043  ASUS"
            ),
            Some("GPU name".into())
        );
        assert_eq!(
            amd_name("1900", "b1", "1900, B1, AMD Radeon 780M Graphics"),
            Some("AMD Radeon 780M Graphics".into())
        );
    }
}
