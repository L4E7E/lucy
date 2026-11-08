//! Opt-in, read-only Niri interaction test. No profile, charge, or idle writes.
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
#[path = "support/pointer.rs"]
mod pointer;
struct Preview {
    child: Child,
    directory: PathBuf,
}
fn control(status: &serde_json::Value, action: &str) -> Option<[f64; 4]> {
    status["power_controls"]
        .as_array()?
        .iter()
        .find(|c| c["action"] == action && c["rect"][3].as_f64().unwrap() >= 24.0)
        .map(|c| std::array::from_fn(|i| c["rect"][i].as_f64().unwrap()))
}
fn seek(
    status: &impl Fn() -> serde_json::Value,
    p: &mut pointer::Pointer,
    x: f64,
    action: &str,
    direction: f64,
) -> [f64; 4] {
    for _ in 0..40 {
        let s = status();
        if let Some(r) = control(&s, action) {
            return r;
        }
        // Only the pinned top-level tabs consume wheel input.
        p.wheel(x + 466.0, 340.0, direction * 20.0);
    }
    panic!("control not visible: {action}: {}", status());
}
fn click(p: &mut pointer::Pointer, x: f64, rect: [f64; 4]) {
    p.click(x + rect[0] + rect[2] / 2.0, 6.0 + rect[1] + rect[3] / 2.0);
}
impl Drop for Preview {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.child.try_wait().ok().flatten().is_some() {
                let _ = fs::remove_dir_all(&self.directory);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
#[test]
#[ignore = "opens a read-only Lucy power preview; run inside a Niri session"]
fn power_page_uses_live_data_and_anchored_dropdown_without_system_writes() {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap());
    let display = if display.is_absolute() {
        display
    } else {
        runtime.join(display)
    };
    let directory = std::env::temp_dir().join(format!("lucy-power-ui.{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_lucy-shell"))
        .args(["bar", "--isolated", "--preview"])
        .env("XDG_RUNTIME_DIR", &directory)
        .env("WAYLAND_DISPLAY", display)
        .env(
            "PULSE_SERVER",
            format!("unix:{}", runtime.join("pulse/native").display()),
        )
        .env("PIPEWIRE_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let preview = Preview { child, directory };
    let socket = UnixDatagram::bind(preview.directory.join("query.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let server = preview.directory.join("lucy-bar-native-preview.sock");
    let start = Instant::now();
    while socket.send_to(b"@status", &server).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    }
    let mut b = [0u8; 16384];
    socket.recv(&mut b).unwrap();
    let status = || {
        socket.send_to(b"@status", &server).unwrap();
        let mut bytes = [0; 16384];
        let n = socket.recv(&mut bytes).unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes[..n]).unwrap()
    };
    let start = Instant::now();
    let initial = loop {
        let s = status();
        if s["power"]["ready"] == true && !s["bars"].as_array().unwrap().is_empty() {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(6));
        thread::sleep(Duration::from_millis(30));
    };
    let profile = initial["power"]["profile"].clone();
    let limit = initial["power"]["charge_limit"].clone();
    socket.send_to(b"power", &server).unwrap();
    thread::sleep(Duration::from_millis(560));
    let opened = status();
    assert_eq!(opened["page"], "power");
    assert_eq!(opened["visible"], true);
    assert_eq!(opened["size"][1], 500);
    assert_eq!(opened["power"]["screen_armed"], false);
    assert_eq!(opened["power"]["suspend_armed"], false);
    let source = opened["surface"].clone();
    let name = initial["bars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["surface"] == source)
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let mut p = pointer::Pointer::new(name);
    let x = p.width - 8.0 - opened["size"][0].as_f64().unwrap();
    if let Some(path) = std::env::var_os("LUCY_POWER_SCREENSHOT") {
        assert!(Command::new("grim")
            .args(["-g", &format!("{},6 480x500", x as i32)])
            .arg(PathBuf::from(path).with_extension("hero.png"))
            .status()
            .unwrap()
            .success());
    }
    click(
        &mut p,
        x,
        control(&opened, "Power(Profile(\"performance\"))").unwrap(),
    );
    assert_eq!(status()["power"]["profile"], profile);
    let r = seek(&status, &mut p, x, "PowerExtra", 1.0);
    click(&mut p, x, r);
    assert_eq!(status()["power"]["extra"], true);
    thread::sleep(Duration::from_millis(60));
    let mid = status();
    let progress = mid["power"]["extra_progress"].as_f64().unwrap();
    assert!(
        progress > 0.0 && progress < 1.0,
        "disclosure must interpolate: {mid}"
    );
    thread::sleep(Duration::from_millis(320));
    assert_eq!(status()["power"]["extra_progress"], 1.0);
    let r = seek(&status, &mut p, x, "PowerPicker(Screen)", 1.0);
    click(&mut p, x, r);
    let dropdown = status();
    assert_eq!(dropdown["power"]["picker"], "Screen");
    let anchor = control(&dropdown, "PowerPicker(Screen)").unwrap();
    let menu = control(&dropdown, "PowerMenu").unwrap();
    assert_eq!((menu[0], menu[2]), (anchor[0], anchor[2]));
    assert_eq!(menu[1], anchor[1] + anchor[3] + 6.0);
    assert!(menu[1] + menu[3] <= 500.0, "dropdown must fit: {dropdown}");
    let choices: Vec<_> = dropdown["power_controls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["action"].as_str().unwrap().contains(", Screen, "))
        .collect();
    assert_eq!(choices.len(), 7);
    for (i, row) in choices.iter().enumerate() {
        assert_eq!(row["rect"][0], menu[0] + 5.0);
        assert_eq!(row["rect"][1], menu[1] + 6.0 + i as f64 * 30.0);
    }
    if let Some(path) = std::env::var_os("LUCY_POWER_SCREENSHOT") {
        let result = Command::new("grim")
            .args(["-g", &format!("{},6 480x500", x as i32)])
            .arg(path)
            .status()
            .unwrap();
        assert!(result.success());
    }
    p.click(x + 70.0, 300.0);
    assert!(status()["power"]["picker"].is_null());
    assert_eq!(status()["visible"], true);
    let r = seek(&status, &mut p, x, "PowerSource(false)", -1.0);
    click(&mut p, x, r);
    let after = status();
    assert_eq!(after["power"]["source_ac"], false);
    assert!(after["power"]["picker"].is_null());
    assert_eq!(after["power"]["charge_limit"], limit);
    assert_eq!(after["power"]["profile"], profile);
    assert_eq!(after["surface"], source);
    let body = control(&after, "PowerSource(false)").unwrap();
    let before_scroll = after["power"]["scroll"].as_f64().unwrap();
    p.wheel(
        x + body[0] + body[2] / 2.0,
        6.0 + body[1] + body[3] / 2.0,
        -15.0,
    );
    let scrolled = status();
    assert_eq!(
        scrolled["power"]["source_ac"], false,
        "body segments must not consume the wheel"
    );
    assert!(
        scrolled["power"]["scroll"].as_f64().unwrap() < before_scroll,
        "body wheel must scroll the page: {scrolled}"
    );
    assert_eq!(scrolled["power"]["profile"], profile);
    let r = seek(&status, &mut p, x, "PowerExtra", -1.0);
    click(&mut p, x, r);
    thread::sleep(Duration::from_millis(60));
    let mid = status();
    let progress = mid["power"]["extra_progress"].as_f64().unwrap();
    assert!(
        progress > 0.0 && progress < 1.0,
        "closing must interpolate: {mid}"
    );
    thread::sleep(Duration::from_millis(320));
    assert_eq!(status()["power"]["extra_progress"], 0.0);
    socket.send_to(b"@hide", &server).unwrap();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(status()["visible"], false);
    println!("PASS: live battery/health reads, shared surface, grouped settings, anchored single-column dropdown and outside dismissal, read-only profile/threshold preservation");
}
