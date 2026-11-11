//! Opt-in real-pointer display UI regression; hardware writes are disabled.
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
    status["display_controls"]
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
fn verify_control_gauges(
    socket: &UnixDatagram,
    server: &PathBuf,
    status: &impl Fn() -> serde_json::Value,
) {
    // All control gauges: real Wayland drag/wheel input in the read-only preview.
    // The model keeps fractional pointer positions; no mixer/backlight writes.
    socket.send_to(b"home", server).unwrap();
    thread::sleep(Duration::from_millis(550));
    let opened = status();
    let mut p = pointer::Pointer::new(opened["output"].as_str().unwrap());
    for target in [
        "Volume",
        "Brightness",
        "Audio(\"sink\", 7)",
        "Audio(\"source\", 8)",
        "Audio(\"sink-input\", 9)",
        "Audio(\"source-output\", 10)",
    ] {
        if target == "Audio(\"sink\", 7)" {
            socket.send_to(b"@preview-audio", &server).unwrap();
            thread::sleep(Duration::from_millis(400));
        }
        let x = p.width - 8.0 - status()["size"][0].as_f64().unwrap();
        let find = |s: &serde_json::Value| {
            s["gauge_controls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["target"] == target && c["rect"][3].as_f64().unwrap() >= 30.0)
                .cloned()
        };
        let mut gauge = None;
        for _ in 0..24 {
            if let Some(c) = find(&status()) {
                gauge = Some(c);
                break;
            }
            p.wheel(x + 466.0, 340.0, 25.0);
        }
        let gauge = gauge.unwrap_or_else(|| panic!("missing gauge {target}: {}", status()));
        let r = std::array::from_fn::<_, 4, _>(|i| gauge["rect"][i].as_f64().unwrap());
        p.drag_with_preview(
            x + r[0] + r[2] * 0.31,
            6.0 + r[1] + r[3] * 0.5,
            r[2] * (0.71125 - 0.31),
            0.0,
            || {
                let moving = status();
                let current = find(&moving).unwrap()["value"].as_f64().unwrap();
                assert!(
                    (current - 71.125).abs() < 1.0,
                    "{target} drag position: {current}; pointer={}, window={}, rect={r:?}, output={}", moving["pointer_xy"], moving["pointer_window"], moving["output"]
                );
                assert!(
                    (current - current.round()).abs() > 0.01,
                    "{target} pointer was quantized: {current}"
                );
            },
        );
        let before = status();
        let previous = find(&before).unwrap()["value"].as_f64().unwrap();
        p.wheel(x + r[0] + r[2] * 0.6, 6.0 + r[1] + r[3] * 0.5, 15.0);
        let after = status();
        let current = find(&after).unwrap()["value"].as_f64().unwrap();
        assert!(
            (current - previous + 2.0).abs() < 0.01,
            "{target} wheel: {previous} -> {current}"
        );
        assert!(
            after["presented"].as_u64().unwrap() > before["presented"].as_u64().unwrap() + 2,
            "{target} wheel did not present animation frames"
        );
    }
    socket.send_to(b"@hide", &server).unwrap();
    thread::sleep(Duration::from_millis(450));
    println!("PASS: fractional drag and animated wheel input for all six audio/brightness gauges; no hardware writes");
}
#[test]
#[ignore = "opens a read-only Lucy display preview; requires an unlocked Niri session"]
fn display_identification_scale_layout_and_night_controls_are_readonly() {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap());
    let display = if display.is_absolute() {
        display
    } else {
        runtime.join(display)
    };
    let directory = std::env::temp_dir().join(format!("lucy-display-ui.{}", std::process::id()));
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
        if s["display"]["ready"] == true && !s["bars"].as_array().unwrap().is_empty() {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(6));
        thread::sleep(Duration::from_millis(30));
    };

    if std::env::var_os("LUCY_TEST_GAUGES_ONLY").is_some() {
        verify_control_gauges(&socket, &server, &status);
        return;
    }
    let hardware = Command::new("niri")
        .args(["msg", "-j", "outputs"])
        .output()
        .unwrap()
        .stdout;
    if let Some(path) = std::env::var_os("LUCY_DISPLAY_SCREENSHOT") {
        socket.send_to(b"home", &server).unwrap();
        thread::sleep(Duration::from_millis(600));
        let output = initial["display"]["outputs"][0]["name"].as_str().unwrap();
        assert!(Command::new("grim")
            .args(["-o", output])
            .arg(PathBuf::from(path).with_extension("home.png"))
            .status()
            .unwrap()
            .success());
    }
    socket.send_to(b"display", &server).unwrap();
    thread::sleep(Duration::from_millis(600));
    let opened = status();
    assert_eq!(opened["page"], "display");
    assert_eq!(opened["size"][1], 500);
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
    click(&mut p, x, control(&opened, "DisplayIdentify").unwrap());
    let identified = status();
    assert!(identified["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["role"].as_str().unwrap().starts_with("Identify(")));
    if let Some(path) = std::env::var_os("LUCY_DISPLAY_SCREENSHOT") {
        assert!(Command::new("grim")
            .args(["-o", name])
            .arg(PathBuf::from(path).with_extension("outputs.png"))
            .status()
            .unwrap()
            .success());
    }
    let scale = seek(
        &status,
        &mut p,
        x,
        "DisplayMenu(Some(Scale(\"eDP-1\")))",
        -1.0,
    );
    click(&mut p, x, scale);
    let mut menu = status();
    if !menu["display_controls"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["action"].as_str().unwrap().starts_with("Display(Scale("))
    {
        p.wheel(x + 466.0, 340.0, 50.0);
        menu = status();
    }
    assert!(menu["display_controls"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["action"].as_str().unwrap().starts_with("Display(Scale(")));
    // Scroll the expanded scale choices without changing display tabs.
    p.wheel(x + 466.0, 340.0, 35.0);
    p.wheel(x + 466.0, 340.0, 35.0);
    assert_eq!(status()["display"]["night"], false);
    let choice = seek(&status, &mut p, x, "Display(Scale(\"eDP-1\", 150))", -1.0);
    click(&mut p, x, choice);
    assert!(status()["display"]["pending"].is_null());
    socket.send_to(b"@preview-display", &server).unwrap();
    thread::sleep(Duration::from_millis(250));
    socket.send_to(b"display", &server).unwrap();
    thread::sleep(Duration::from_millis(150));
    let fixture = status();
    assert_eq!(fixture["display"]["outputs"].as_array().unwrap().len(), 2);
    let rect = control(&fixture, "DisplayDrag(\"DP-3\", 0.075)")
        .or_else(|| {
            fixture["display_controls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| {
                    c["action"]
                        .as_str()
                        .unwrap()
                        .starts_with("DisplayDrag(\"DP-3\"")
                })
                .map(|c| std::array::from_fn(|i| c["rect"][i].as_f64().unwrap()))
        })
        .unwrap();
    let internal_rect = fixture["display_controls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["action"]
                .as_str()
                .unwrap()
                .starts_with("DisplayDrag(\"eDP-1\"")
        })
        .map(|c| std::array::from_fn::<_, 4, _>(|i| c["rect"][i].as_f64().unwrap()))
        .unwrap();
    let drag_dx = internal_rect[0] + 3.0 - (rect[0] + rect[2] / 2.0);
    p.drag_with_preview(
        x + rect[0] + rect[2] / 2.0,
        6.0 + rect[1] + rect[3] / 2.0,
        drag_dx,
        0.0,
        || {
            let moving = status();
            assert_eq!(moving["display"]["dragging"], true);
            let draft = moving["display"]["snap_preview"]
                .as_array()
                .expect("edge must reveal a separate snap ghost");
            let internal = draft.iter().find(|p| p["name"] == "eDP-1").unwrap();
            let external = draft.iter().find(|p| p["name"] == "DP-3").unwrap();
            assert!(internal["x"].as_i64().unwrap() > external["x"].as_i64().unwrap());
            let unchanged = moving["display"]["draft"].as_array().unwrap();
            assert_eq!(
                unchanged.iter().find(|p| p["name"] == "eDP-1").unwrap()["x"],
                0
            );
            let card = moving["display_controls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| {
                    c["action"]
                        .as_str()
                        .unwrap()
                        .starts_with("DisplayDrag(\"DP-3\"")
                })
                .unwrap();
            assert!((card["rect"][2].as_f64().unwrap() - rect[2]).abs() < 0.000001);
            assert!((card["rect"][3].as_f64().unwrap() - rect[3]).abs() < 0.000001);
            if let Some(path) = std::env::var_os("LUCY_DISPLAY_SCREENSHOT") {
                assert!(Command::new("grim")
                    .args(["-o", name])
                    .arg(PathBuf::from(path).with_extension("drag.png"))
                    .status()
                    .unwrap()
                    .success());
            }
        },
    );
    let dragged = status();
    assert_eq!(dragged["display"]["dragging"], false);
    assert!(!dragged["display"]["draft"].as_array().unwrap().is_empty());
    let draft = dragged["display"]["draft"].as_array().unwrap();
    let internal = draft.iter().find(|p| p["name"] == "eDP-1").unwrap();
    let external = draft.iter().find(|p| p["name"] == "DP-3").unwrap();
    assert!(internal["x"].as_i64().unwrap() > external["x"].as_i64().unwrap());
    assert!(
        control(&dragged, "DisplayApplyLayout").is_some(),
        "{dragged}"
    );
    click(&mut p, x, control(&dragged, "DisplayResetLayout").unwrap());
    let reset = status();
    assert!(reset["display"]["draft"].as_array().unwrap().is_empty());
    click(&mut p, x, control(&reset, "DisplayTab(true)").unwrap());
    let night = status();
    assert_eq!(night["display"]["night"], true);
    if night["display"]["night_enabled"] == false {
        click(&mut p, x, control(&night, "Display(Night(true))").unwrap());
    }
    let night = status();
    assert_eq!(
        night["display"]["gamma_active"], false,
        "preview must never acquire gamma controls"
    );
    if night["display"]["automatic"] == false {
        click(
            &mut p,
            x,
            control(&night, "Display(Automatic(true))").unwrap(),
        );
    }
    let clock = status();
    let start = clock["display"]["start"].as_u64().unwrap();
    let end = clock["display"]["end"].as_u64().unwrap();
    let scroll = clock["scroll"]["position"].clone();
    let hour = control(&clock, "DisplayTime(true, Hour)").unwrap();
    let minute = control(&clock, "DisplayTime(true, Minute)").unwrap();
    let step = if ((start / 60 + 1) % 24) * 60 + start % 60 == end {
        -1
    } else {
        1
    };
    p.wheel(
        x + hour[0] + hour[2] / 2.0,
        6.0 + hour[1] + hour[3] / 2.0,
        -15.0 * step as f64,
    );
    let after_hour = status();
    let expected = ((start / 60) as i64 + step).rem_euclid(24) as u64 * 60 + start % 60;
    assert_eq!(after_hour["display"]["start"], expected);
    assert_eq!(after_hour["display"]["end"], end);
    assert_eq!(after_hour["scroll"]["position"], scroll);
    let step = if expected / 60 * 60 + (expected % 60 + 1) % 60 == end {
        -1
    } else {
        1
    };
    p.wheel(
        x + minute[0] + minute[2] / 2.0,
        6.0 + minute[1] + minute[3] / 2.0,
        -15.0 * step as f64,
    );
    let after_minute = status();
    assert_eq!(
        after_minute["display"]["start"],
        expected / 60 * 60 + ((expected % 60) as i64 + step).rem_euclid(60) as u64
    );
    assert_eq!(after_minute["display"]["end"], end);
    assert_eq!(after_minute["scroll"]["position"], scroll);
    if let Some(path) = std::env::var_os("LUCY_DISPLAY_SCREENSHOT") {
        assert!(Command::new("grim")
            .args(["-o", name])
            .arg(PathBuf::from(path).with_extension("night.png"))
            .status()
            .unwrap()
            .success());
    }
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&hardware).unwrap(),
        serde_json::from_slice::<serde_json::Value>(
            &Command::new("niri")
                .args(["msg", "-j", "outputs"])
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
    );
    assert_eq!(status()["surface"], source);
    socket.send_to(b"@preview-media", &server).unwrap();
    thread::sleep(Duration::from_millis(450));
    let media = status();
    assert_eq!(media["page"], "home");
    assert_eq!(media["media"]["seekable"], true);
    assert_eq!(media["media_timer"], true);
    let position = media["media"]["position"].as_i64().unwrap();
    let frames = media["presented"].as_u64().unwrap();
    thread::sleep(Duration::from_millis(400));
    let advanced = status();
    assert!(advanced["media"]["position"].as_i64().unwrap() > position + 250_000);
    assert!(advanced["presented"].as_u64().unwrap() > frames);
    let progress = &advanced["media_controls"][0]["rect"];
    p.click(
        x + progress[0].as_f64().unwrap() + progress[2].as_f64().unwrap() * 0.75,
        6.0 + progress[1].as_f64().unwrap() + 10.0,
    );
    let sought = status()["media"]["position"].as_i64().unwrap();
    assert!(
        (sought - 135_000_000).abs() < 2_000_000,
        "seek position: {sought}"
    );
    if let Some(path) = std::env::var_os("LUCY_DISPLAY_SCREENSHOT") {
        assert!(Command::new("grim")
            .args(["-o", name])
            .arg(PathBuf::from(path).with_extension("media.png"))
            .status()
            .unwrap()
            .success());
    }
    socket.send_to(b"@hide", &server).unwrap();
    thread::sleep(Duration::from_millis(450));
    assert_eq!(
        status()["media_timer"],
        false,
        "hidden media must not poll or repaint"
    );
    verify_control_gauges(&socket, &server, &status);
    println!("PASS: live output list, temporary identify overlay, scale dropdown, scroll, dual display drag/reset, independent hour/minute wheel controls; no hardware writes");
    println!("PASS: native media progress, microsecond seeking, visible-only refresh timer; no player writes");
    println!("PASS: fractional drag and animated wheel input for volume, brightness, output, microphone, per-app playback/recording; no hardware writes");
}
