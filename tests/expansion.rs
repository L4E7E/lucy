//! Opt-in real compositor test: the same bar wl_surface must return after closing.
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[path = "support/pointer.rs"]
mod pointer;

struct Preview {
    child: Child,
    dir: PathBuf,
}
impl Drop for Preview {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.child.try_wait().ok().flatten().is_some() {
                let _ = fs::remove_dir_all(&self.dir);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[test]
#[ignore = "opens a read-only bar preview; explicitly run inside a Wayland session"]
fn bar_surface_expands_and_returns_without_replacement() {
    // Set LUCY_TEST_INPUT=1 to test real body controls with the outside-click
    // catcher enabled. Pass-through previews alone cannot catch input stacking bugs.
    let input = std::env::var_os("LUCY_TEST_INPUT").is_some();
    let osd_only = std::env::var_os("LUCY_TEST_OSD_ONLY").is_some();
    let monitor_only = std::env::var_os("LUCY_TEST_MONITOR_ONLY").is_some();
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap());
    let display = if display.is_absolute() {
        display
    } else {
        runtime.join(display)
    };
    let dir = std::env::temp_dir().join(format!(
        "lucy-expansion-test.{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lucy-shell"));
    command.args(["bar", "--isolated", "--preview"]);
    if input {
        command.env_remove("LUCY_UI_TEST_NO_INPUT");
    } else {
        command.env("LUCY_UI_TEST_NO_INPUT", "1");
    }
    let child = command
        .env("XDG_RUNTIME_DIR", &dir)
        .env("WAYLAND_DISPLAY", display)
        .env(
            "PULSE_SERVER",
            format!("unix:{}", runtime.join("pulse/native").display()),
        )
        .env("PIPEWIRE_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let preview = Preview { child, dir };
    let socket = UnixDatagram::bind(preview.dir.join("test.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let server = preview.dir.join("lucy-bar-native-preview.sock");
    let start = Instant::now();
    while socket.send_to(b"@status", &server).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    }
    let mut bytes = [0; 8192];
    socket.recv(&mut bytes).unwrap();
    let status = || {
        socket.send_to(b"@status", &server).unwrap();
        let mut b = [0; 8192];
        let n = socket.recv(&mut b).unwrap();
        serde_json::from_slice::<serde_json::Value>(&b[..n]).unwrap()
    };
    thread::sleep(Duration::from_millis(500));
    let initial = status();
    let bars = initial["bars"].as_array().unwrap();
    let mut pointer = None;
    for cycle in 0..if osd_only || monitor_only { 0 } else { 8 } {
        let (page, side, width) = if cycle % 2 == 0 {
            ("home", "right", 480)
        } else {
            ("calendar", "center", 780)
        };
        socket.send_to(page.as_bytes(), &server).unwrap();
        thread::sleep(Duration::from_millis(560));
        let opened = status();
        assert_eq!(opened["visible"], true, "cycle {cycle}: {opened}");
        let source = opened["surface"].clone();
        assert!(
            bars.iter()
                .any(|b| b["side"] == side && b["surface"] == source),
            "must borrow an original surface on the chosen output: opened={opened}, bars={bars:?}"
        );
        assert!(opened["borrowed_from"].is_number());
        assert_eq!(opened["size"][1], 500, "cycle {cycle}: {opened}");
        assert_eq!(
            opened["expanding"], false,
            "buttons must be interactive after settling"
        );
        assert!(opened["size"][0].as_i64().unwrap() >= width);
        assert!(!opened["bars"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["surface"] == source));
        if input {
            let name = bars.iter().find(|b| b["surface"] == source).unwrap()["output"]
                .as_str()
                .unwrap();
            // IPC opens follow the focused output, which can change between
            // cycles; do not keep a pointer bound to another monitor.
            let p = pointer.insert(pointer::Pointer::new(name));
            if page == "home" {
                let x = p.width - 8.0 - opened["size"][0].as_f64().unwrap()
                    + (opened["size"][0].as_f64().unwrap() - 480.0) / 2.0;
                if cycle == 0 {
                    let anchors = status()["tray_toggles"].clone();
                    for (target, effect) in [("audio", "Slide"), ("home", "SlideBack")] {
                        socket.send_to(target.as_bytes(), &server).unwrap();
                        thread::sleep(Duration::from_millis(65));
                        let moving = status();
                        assert_eq!(
                            moving["animation"], effect,
                            "page refresh must not replace the push transition"
                        );
                        assert_eq!(moving["animating"], true);
                        assert_eq!(moving["surface"], source);
                        assert_eq!(
                            moving["tray_toggles"], anchors,
                            "bar header must not slide with the page"
                        );
                        if let Some(directory) = std::env::var_os("LUCY_NAV_SCREENSHOTS") {
                            assert!(Command::new("grim")
                                .args(["-g", &format!("{},6 480x500", x as i32)])
                                .arg(PathBuf::from(directory).join(format!("{effect}.png")))
                                .status()
                                .unwrap()
                                .success());
                        }
                        thread::sleep(Duration::from_millis(300));
                    }
                    println!("detail navigation pushes left, Back pushes right, and the glass/header stay fixed");
                }
                // Joined inset (24) + the current compact media/audio row (357)
                // + output margin (6): click the gear itself, not above it.
                p.click(x + 444.0, 387.0);
                let clicked = status();
                assert_eq!(
                    clicked["visible"], true,
                    "body click must not hit the backdrop"
                );
                assert_eq!(clicked["page"], "audio");
                p.click(x + 38.0, 63.0);
                assert_eq!(status()["page"], "home");
                p.click(x + 144.0, 130.0);
                assert_eq!(status()["page"], "network");
                assert_eq!(
                    status()["network"]["scanning"],
                    false,
                    "read-only preview must not scan Wi-Fi"
                );
                p.click(x + 382.0, 112.0);
                assert_eq!(status()["tab"], "vpn");
                p.wheel(x + 382.0, 112.0, -15.0);
                assert_eq!(status()["tab"], "ethernet");
                p.wheel(x + 382.0, 112.0, -15.0);
                assert_eq!(status()["tab"], "wifi");
                p.click(x + 38.0, 63.0);
                assert_eq!(status()["page"], "home");
                p.click(x + 294.0, 130.0);
                let bluetooth = status();
                assert_eq!(bluetooth["page"], "bluetooth");
                assert_eq!(bluetooth["visible"], true);
                assert_eq!(bluetooth["surface"], source);
                assert_eq!(
                    bluetooth["bluetooth"]["scanning"], false,
                    "preview must not start discovery"
                );
            } else {
                let x = (p.width - opened["size"][0].as_f64().unwrap()) / 2.0;
                // Calendar navigation moved below the clock/date widget:
                // output margin 6 + button y (64 + 78) + half-height 15.
                p.click(x + 38.0, 163.0);
                let clicked = status();
                assert_eq!(
                    clicked["visible"], true,
                    "calendar click must not hit the backdrop"
                );
                assert_eq!(clicked["calendar_offset"], -1);
                p.click(x + 282.0, 163.0);
                assert_eq!(status()["calendar_offset"], 0);
                p.wheel(x + 166.0, 240.0, 15.0);
                assert_eq!(status()["calendar_offset"], 1);
                p.wheel(x + 166.0, 240.0, -15.0);
                assert_eq!(status()["calendar_offset"], 0);
            }
            println!("cycle {cycle}: {page} body controls respond to persistent pointer input");
        }
        if page == "home" {
            for detail in ["network", "audio", "bluetooth", "home"] {
                socket.send_to(detail.as_bytes(), &server).unwrap();
                thread::sleep(Duration::from_millis(40));
                assert_eq!(status()["surface"], source);
            }
            if cycle == 0 {
                socket.send_to(b"@preview-network", &server).unwrap();
                for tab in ["ethernet", "vpn"] {
                    socket.send_to(tab.as_bytes(), &server).unwrap();
                    thread::sleep(Duration::from_millis(220));
                    let network = status();
                    assert_eq!(network["tab"], tab);
                    let rows = network["network"]["connections"].as_array().unwrap();
                    assert_eq!(rows.len(), if tab == "ethernet" { 4 } else { 2 });
                    assert!(rows.iter().any(|r| r["operation"] == "connect"));
                    assert!(rows.iter().any(|r| r["operation"] == "disconnect"));
                    for row in rows {
                        assert_eq!(row["rect"][0], 416.0);
                        assert_eq!(row["rect"][2], 32.0);
                        assert_eq!(row["rect"][3], 32.0);
                    }
                    if let Some(directory) = std::env::var_os("LUCY_NETWORK_SCREENSHOTS") {
                        let output = bars.iter().find(|b| b["surface"] == source).unwrap()
                            ["output"]
                            .as_str()
                            .unwrap();
                        let mut p = pointer
                            .take()
                            .unwrap_or_else(|| pointer::Pointer::new(output));
                        let x = p.width - 8.0 - network["size"][0].as_f64().unwrap();
                        assert!(Command::new("grim")
                            .args(["-g", &format!("{},6 480x500", x as i32)])
                            .arg(PathBuf::from(directory).join(format!("{tab}.png")))
                            .status()
                            .unwrap()
                            .success());
                        // Even clicking these preview buttons must not invoke nmcli.
                        let r = &rows[0]["rect"];
                        p.click(
                            x + r[0].as_f64().unwrap() + 16.0,
                            6.0 + r[1].as_f64().unwrap() + 16.0,
                        );
                        assert_eq!(status()["network"]["busy"], false);
                        pointer = Some(p);
                    }
                }
                println!("Ethernet/VPN: glass cards use Wi-Fi's 32x32 link/X actions; preview remains read-only");
            }
        }
        let before_close = status();
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(100));
        if std::env::var_os("LUCY_SOFTWARE").is_none() {
            let mid = status();
            assert_eq!(mid["closing"], true);
            let h = mid["size"][1].as_i64().unwrap();
            assert!(h > 34 && h < 500, "closing must actually shrink: {mid}");
            if page == "calendar" {
                let rasterized = |state: &serde_json::Value| {
                    state["surfaces"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|w| w["surface"] == source)
                        .unwrap()["rasterized"]
                        .as_u64()
                        .unwrap()
                };
                assert_eq!(
                    rasterized(&mid),
                    rasterized(&before_close),
                    "calendar collapse must use cached GPU textures, not repaint each frame"
                );
            }
        }
        thread::sleep(Duration::from_millis(250));
        let closed = status();
        assert_eq!(closed["visible"], false);
        assert!(closed["borrowed_from"].is_null());
        let returned = closed["bars"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["surface"] == source)
            .unwrap();
        assert_eq!(returned["surface"], source);
        assert_eq!(returned["size"][1], 34);
        if page == "calendar" {
            let original = bars.iter().find(|b| b["surface"] == source).unwrap();
            assert_eq!(
                returned["size"], original["size"],
                "calendar must restore the full date bar width, not the HH:MM widget width"
            );
        }
        assert_eq!(closed["bars"].as_array().unwrap().len(), bars.len());
        println!("cycle {cycle}: {side} surface preserved, expanded, and restored");
    }
    if !osd_only {
        let deadline = Instant::now() + Duration::from_secs(5);
        let closed = loop {
            let state = status();
            if state["system_targets"].as_array().is_some_and(|targets| {
                targets.len() == bars.iter().filter(|b| b["side"] == "right").count()
            }) {
                break state;
            }
            assert!(
                Instant::now() < deadline,
                "bar metrics did not become ready"
            );
            thread::sleep(Duration::from_millis(25));
        };
        let targets = closed["system_targets"].as_array().unwrap();
        assert_eq!(
            targets.len(),
            bars.iter().filter(|b| b["side"] == "right").count()
        );
        for target in targets {
            let source = &target["surface"];
            if input {
                // Live numeric metrics may change the bar width while the
                // previous output is being tested. Click its current geometry.
                let current = status();
                let target = current["system_targets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["surface"] == *source)
                    .unwrap();
                let mut p = pointer::Pointer::new(target["output"].as_str().unwrap());
                let r = &target["rect"];
                p.click(
                    p.width - 8.0 - target["width"].as_f64().unwrap()
                        + r[0].as_f64().unwrap()
                        + r[2].as_f64().unwrap() / 2.0,
                    6.0 + r[1].as_f64().unwrap() + r[3].as_f64().unwrap() / 2.0,
                );
            } else {
                socket.send_to(b"system", &server).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let opened = loop {
                let state = status();
                if state["monitor"]["cpu"].is_number()
                    && state["size"][1] == 500
                    && state["expanding"] == false
                {
                    break state;
                }
                assert!(
                    Instant::now() < deadline,
                    "monitor did not become ready: {state}"
                );
                thread::sleep(Duration::from_millis(25));
            };
            assert_eq!(opened["page"], "system", "target {target}: {opened}");
            if input {
                assert_eq!(
                    &opened["surface"], source,
                    "system click must borrow that output's original bar surface"
                );
            }
            assert!(opened["monitor"]["threads"]
                .as_array()
                .is_some_and(|t| !t.is_empty()));
            assert!(opened["monitor"]["memory"]["total"].as_u64().unwrap() > 0);
            assert!(
                opened["monitor"]["disk"]["usage"]["total"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            println!("monitor GPU telemetry: {}", opened["monitor"]["gpus"]);
            if std::env::var_os("LUCY_TEST_NVIDIA").is_some() {
                let gpus = opened["monitor"]["gpus"].as_array().unwrap();
                let nvidia = gpus
                    .iter()
                    .find(|g| g["name"].as_str().is_some_and(|s| s.starts_with("NVIDIA")))
                    .expect("active NVIDIA telemetry must resolve its actual name through NVML");
                assert!(nvidia["usage"].is_number());
                assert!(nvidia["temperature"].is_number());
                assert!(nvidia["vram"]["total"].as_u64().is_some_and(|n| n > 0));
            }
            if let Some(directory) = std::env::var_os("LUCY_MONITOR_SCREENSHOTS") {
                let output = bars
                    .iter()
                    .find(|b| b["surface"] == opened["surface"])
                    .unwrap()["output"]
                    .as_str()
                    .unwrap();
                let raw = Command::new("niri")
                    .args(["msg", "-j", "outputs"])
                    .output()
                    .unwrap();
                assert!(raw.status.success());
                let outputs: serde_json::Value = serde_json::from_slice(&raw.stdout).unwrap();
                let logical = &outputs[output]["logical"];
                let width = opened["size"][0].as_i64().unwrap();
                let x =
                    logical["x"].as_i64().unwrap() + logical["width"].as_i64().unwrap() - 8 - width;
                let y = logical["y"].as_i64().unwrap() + 6;
                assert!(Command::new("grim")
                    .args([
                        "-s",
                        &logical["scale"].to_string(),
                        "-g",
                        &format!("{x},{y} {width}x500")
                    ])
                    .arg(PathBuf::from(directory).join(format!("monitor-{output}.png")))
                    .status()
                    .unwrap()
                    .success());
            }
            let sequence = opened["monitor"]["sequence"].as_u64().unwrap();
            thread::sleep(Duration::from_millis(600));
            assert!(status()["monitor"]["sequence"].as_u64().unwrap() > sequence);
            if opened["gpu_gauges"].as_u64().is_some_and(|n| n > 0) {
                let rasterized = |state: &serde_json::Value| {
                    state["surfaces"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|w| w["surface"] == opened["surface"])
                        .unwrap()["rasterized"]
                        .as_u64()
                        .unwrap()
                };
                let before = status();
                thread::sleep(Duration::from_millis(1600));
                let after = status();
                let samples = after["monitor"]["sequence"].as_u64().unwrap()
                    - before["monitor"]["sequence"].as_u64().unwrap();
                let rasters = rasterized(&after) - rasterized(&before);
                assert!(samples >= 2);
                assert!(
                    rasters <= samples + 4,
                    "GPU gauges must not rerasterize the whole scene per frame: {rasters} rasters / {samples} samples"
                );
                println!("GPU gauges: {rasters} scene updates for {samples} telemetry samples; animation does not rerasterize text/background");
                socket.send_to(b"home", &server).unwrap();
                thread::sleep(Duration::from_millis(65));
                let leaving = status();
                assert_eq!(leaving["surface"], opened["surface"]);
                assert_eq!(leaving["animation"], "SlideBack");
                // Home may retain one media progress gauge when a player is
                // present; only the monitor's telemetry gauges must disappear.
                assert!(leaving["gpu_gauges"].as_u64().unwrap() <= 1);
                assert_eq!(leaving["monitor"]["active"], false);
                thread::sleep(Duration::from_millis(300));
                socket.send_to(b"system", &server).unwrap();
                thread::sleep(Duration::from_millis(65));
                let returning = status();
                assert_eq!(returning["surface"], opened["surface"]);
                assert_eq!(returning["animation"], "Slide");
                assert!(returning["gpu_gauges"].as_u64().is_some_and(|n| n > 0));
                thread::sleep(Duration::from_millis(300));
            }
            socket.send_to(b"@hide", &server).unwrap();
            thread::sleep(Duration::from_millis(400));
            let closed = status();
            assert_eq!(closed["monitor"]["active"], false);
            let sequence = closed["monitor"]["sequence"].clone();
            thread::sleep(Duration::from_millis(650));
            assert_eq!(
                status()["monitor"]["sequence"],
                sequence,
                "hidden monitor must stop sampling"
            );
            println!("system monitor: live CPU/RAM/disk, original bar surface, same panel size, hidden sampling stopped");
            if !input {
                break;
            }
        }
    }
    if monitor_only {
        return;
    }
    if std::env::var_os("LUCY_SOFTWARE").is_none() && !osd_only {
        socket.send_to(b"home", &server).unwrap();
        thread::sleep(Duration::from_millis(560));
        let before = status();
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(80));
        let shrinking = status();
        socket.send_to(b"home", &server).unwrap();
        thread::sleep(Duration::from_millis(30));
        let reversed = status();
        assert_eq!(reversed["surface"], before["surface"]);
        assert_eq!(reversed["closing"], false);
        assert!(
            reversed["size"][1].as_i64().unwrap() >= shrinking["size"][1].as_i64().unwrap() - 2,
            "reopening must reverse, not snap to the bar"
        );
        thread::sleep(Duration::from_millis(440));
        assert_eq!(status()["size"][1], 500);
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(350));
        assert!(status()["borrowed_from"].is_null());
    }
    if input {
        for header in [true, false] {
            socket.send_to(b"calendar", &server).unwrap();
            thread::sleep(Duration::from_millis(560));
            let opened = status();
            let mut p = pointer::Pointer::new(opened["output"].as_str().unwrap());
            if header {
                let x = (p.width - opened["size"][0].as_f64().unwrap()) / 2.0;
                p.click(x + 166.0, 28.0);
            } else {
                p.click(20.0, 55.0);
            }
            let dismissed = status();
            assert_eq!(dismissed["visible"], false, "header={header}: {dismissed}");
            thread::sleep(Duration::from_millis(100));
            assert!(status()["borrowed_from"].is_null());
        }
        println!("enlarged clock and outside clicks both close the panel");
    }
    if std::env::var_os("LUCY_SOFTWARE").is_none() {
        let surface = |s: &serde_json::Value, prefix: &str| {
            s["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["role"].as_str().unwrap().starts_with(prefix))
                .unwrap_or_else(|| panic!("missing {prefix} surface: {s}"))
                .clone()
        };
        let mut center = serde_json::Value::Null;
        for _ in 0..3 {
            socket.send_to(b"@preview-toast", &server).unwrap();
            thread::sleep(Duration::from_millis(100));
            let mid = status();
            let toast = surface(&mid, "CenterOverlay");
            if center.is_null() {
                center = toast["surface"].clone();
                assert!(bars
                    .iter()
                    .any(|b| b["side"] == "center" && b["surface"] == center));
            }
            assert_eq!(
                toast["surface"], center,
                "notification must grow from the actual clock bar"
            );
            assert!(toast["size"][1].as_i64().unwrap() > 34);
            assert_eq!(
                toast["animating"], true,
                "popup must not snap open immediately"
            );
            thread::sleep(Duration::from_millis(440));
            let settled = surface(&status(), "CenterOverlay");
            assert_eq!(settled["animating"], false);
            assert_eq!(settled["size"][0], 380);
            socket.send_to(b"@preview-toast-clear", &server).unwrap();
            thread::sleep(Duration::from_millis(350));
            assert!(status()["bars"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["surface"] == center && b["size"][1] == 34));
        }
        println!(
            "notification popup: clock bar expands into one frameless-content card on all 3 cycles"
        );
        for kind in ["volume", "microphone", "brightness", "keyboard", "profile"] {
            socket
                .send_to(format!("@preview-osd:{kind}").as_bytes(), &server)
                .unwrap();
            thread::sleep(Duration::from_millis(100));
            let mid = status();
            let osd = surface(&mid, "CenterOverlay");
            assert_eq!(osd["surface"], center);
            assert_eq!(osd["size"][1], 34, "OSD may only expand horizontally");
            let source_width = bars.iter().find(|b| b["surface"] == center).unwrap()["size"][0]
                .as_i64()
                .unwrap();
            let target_width = (source_width + 60).max(236);
            assert!(
                osd["size"][0].as_i64().unwrap() < target_width - 20,
                "OSD must not finish expanding during the first 100ms: {osd}"
            );
            let mut widths = std::collections::BTreeSet::new();
            widths.insert(osd["size"][0].as_i64().unwrap());
            for _ in 0..3 {
                thread::sleep(Duration::from_millis(60));
                let sample = surface(&status(), "CenterOverlay");
                widths.insert(sample["size"][0].as_i64().unwrap());
            }
            assert!(
                widths.len() >= 3,
                "OSD skipped its intermediate widths: {widths:?}"
            );
            assert_eq!(mid["osd"]["kind"], kind);
            if kind == "keyboard" {
                assert_eq!(mid["osd"]["label"], "2단계");
                assert_eq!(mid["osd"]["step"], 2);
            }
            thread::sleep(Duration::from_millis(230));
            let settled = surface(&status(), "CenterOverlay");
            assert_eq!(
                settled["size"],
                serde_json::json!([(source_width + 60).max(236), 34])
            );
            assert_eq!(settled["animating"], false);
            assert!(
                settled["presented"].as_u64().unwrap() >= osd["presented"].as_u64().unwrap() + 8,
                "OSD must deliver intermediate compositor-paced frames: {settled}"
            );
            socket.send_to(b"@preview-osd-clear", &server).unwrap();
            thread::sleep(Duration::from_millis(350));
            assert!(status()["bars"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["surface"] == center && b["size"][1] == 34));
        }
        println!("all five compact OSDs reuse and restore the actual clock surface");
        if osd_only {
            thread::sleep(Duration::from_millis(100));
            assert_eq!(status()["frame_pump"], false);
            println!("OSD-only checks complete; no idle animation timer remains");
            return;
        }
        let before: std::collections::BTreeMap<_, _> = status()["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["role"].as_str().unwrap().starts_with("Bar(\"left\""))
            .map(|w| {
                (
                    w["surface"].as_u64().unwrap(),
                    w["presented"].as_u64().unwrap(),
                )
            })
            .collect();
        socket.send_to(b"@preview-indicator", &server).unwrap();
        thread::sleep(Duration::from_millis(420));
        // The fixture changes one output, not the first entry of another HashMap.
        let state = status();
        let after = state["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["role"].as_str().unwrap().starts_with("Bar(\"left\""))
            .max_by_key(|w| {
                w["presented"].as_u64().unwrap() - before[&w["surface"].as_u64().unwrap()]
            })
            .unwrap();
        let frames =
            after["presented"].as_u64().unwrap() - before[&after["surface"].as_u64().unwrap()];
        assert!(
            frames > 4,
            "indicator must animate independently of any open panel"
        );
        assert_eq!(after["animating"], false);
        println!("liquid indicator: {frames} compositor-paced frames");
        if input {
            let output = state["bars"]
                .as_array()
                .unwrap()
                .iter()
                .find(|bar| bar["surface"] == after["surface"])
                .unwrap()["output"]
                .as_str()
                .unwrap();
            let p = pointer.insert(pointer::Pointer::new(output));
            // Preview workspaces are fictitious: input is real, Niri focus is not changed.
            p.wheel(66.0, 23.0, 15.0);
            assert_eq!(status()["workspace_scroll_target"], 90001);
            p.wheel(66.0, 23.0, -15.0);
            assert_eq!(status()["workspace_scroll_target"], 90000);
            p.wheel(66.0, 23.0, -15.0);
            assert_eq!(status()["workspace_scroll_target"], 90000);
            println!("indicator wheel switches and clamps the output-local workspace target");
        }
        for _ in 0..2 {
            let state = status();
            let anchor = state["tray_toggles"].clone();
            let before = surface(&state, "Bar(\"right\"");
            socket.send_to(b"@preview-tray", &server).unwrap();
            thread::sleep(Duration::from_millis(330));
            let after = surface(&status(), "Bar(\"right\"");
            let frames =
                after["presented"].as_u64().unwrap() - before["presented"].as_u64().unwrap();
            assert!(
                frames > 4,
                "tray must animate while the control center is closed"
            );
            assert_eq!(after["animating"], false, "tray did not settle: {after}");
            assert_eq!(
                status()["tray_toggles"],
                anchor,
                "tray toggle must not move on screen"
            );
            println!("tray: {frames} compositor-paced frames");
        }
        socket.send_to(b"home", &server).unwrap();
        thread::sleep(Duration::from_millis(560));
        for _ in 0..2 {
            let before = status();
            let id = before["surface"].clone();
            socket.send_to(b"@preview-tray", &server).unwrap();
            let mut heights = vec![];
            for _ in 0..8 {
                thread::sleep(Duration::from_millis(40));
                let frame = status();
                assert_eq!(frame["surface"], id);
                assert_eq!(frame["visible"], true);
                assert_eq!(
                    frame["expanding"], false,
                    "tray must not restart the open panel animation: {frame}"
                );
                heights.push(frame["size"][1].as_i64().unwrap());
            }
            assert!(heights.into_iter().all(|h| h == 500));
            println!(
                "open control center: tray resizes without restarting expansion or blocking input"
            );
            let width = status()["size"][0].clone();
            socket.send_to(b"@hide", &server).unwrap();
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(25));
                let frame = status();
                let live = frame["surfaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["surface"] == id)
                    .unwrap();
                assert_eq!(
                    live["size"][0], width,
                    "closing tray resize must not jump: {frame}"
                );
            }
            socket.send_to(b"home", &server).unwrap();
            thread::sleep(Duration::from_millis(450));
        }
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(350));
        socket.send_to(b"@preview-calendar", &server).unwrap();
        thread::sleep(Duration::from_millis(560));
        let before = status();
        let start = Instant::now();
        for i in 0..240 {
            let message = if i % 120 < 60 {
                b"@preview-scroll:2".as_slice()
            } else {
                b"@preview-scroll:-2".as_slice()
            };
            socket.send_to(message, &server).unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        let after = status();
        let frames = after["presented"].as_u64().unwrap() - before["presented"].as_u64().unwrap();
        let fps = frames as f64 / start.elapsed().as_secs_f64();
        assert!(
            fps > 45.0,
            "scrolling frame throughput unexpectedly low: {fps:.1}"
        );
        let unique = (surface(&after, "Expanded")["rasterized"].as_u64().unwrap()
            - surface(&before, "Expanded")["rasterized"].as_u64().unwrap())
            as f64
            / start.elapsed().as_secs_f64();
        println!("100-notification scroll: {fps:.1} presented frames/s, {unique:.1} distinct content updates/s; velocity-preserving frame-clock interpolation");
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(350));
    }
}
