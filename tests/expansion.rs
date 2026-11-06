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
    for cycle in 0..8 {
        let (page, side, width) = if cycle % 2 == 0 {
            ("home", "right", 480)
        } else {
            ("calendar", "center", 780)
        };
        socket.send_to(page.as_bytes(), &server).unwrap();
        thread::sleep(Duration::from_millis(450));
        let opened = status();
        assert_eq!(opened["visible"], true, "cycle {cycle}: {opened}");
        let source = opened["surface"].clone();
        assert!(
            bars.iter()
                .any(|b| b["side"] == side && b["surface"] == source),
            "must borrow an original surface on the chosen output"
        );
        assert!(opened["borrowed_from"].is_number());
        assert_eq!(opened["size"][1], 500);
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
            let p = pointer.get_or_insert_with(|| pointer::Pointer::new(name));
            if page == "home" {
                let x = p.width - 8.0 - opened["size"][0].as_f64().unwrap()
                    + (opened["size"][0].as_f64().unwrap() - 480.0) / 2.0;
                p.click(x + 444.0, 374.0);
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
                p.click(x + 382.0, 112.0);
                assert_eq!(status()["tab"], "vpn");
            } else {
                let x = (p.width - opened["size"][0].as_f64().unwrap()) / 2.0;
                p.click(x + 38.0, 85.0);
                let clicked = status();
                assert_eq!(
                    clicked["visible"], true,
                    "calendar click must not hit the backdrop"
                );
                assert_eq!(clicked["calendar_offset"], -1);
                p.click(x + 282.0, 85.0);
                assert_eq!(status()["calendar_offset"], 0);
                p.wheel(x + 166.0, 200.0, 15.0);
                assert_eq!(status()["calendar_offset"], 1);
                p.wheel(x + 166.0, 200.0, -15.0);
                assert_eq!(status()["calendar_offset"], 0);
            }
            println!("cycle {cycle}: {page} body controls respond to persistent pointer input");
        }
        if page == "home" {
            for detail in ["network", "audio", "home"] {
                socket.send_to(detail.as_bytes(), &server).unwrap();
                thread::sleep(Duration::from_millis(40));
                assert_eq!(status()["surface"], source);
            }
        }
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(100));
        if std::env::var_os("LUCY_SOFTWARE").is_none() {
            let mid = status();
            assert_eq!(mid["closing"], true);
            let h = mid["size"][1].as_i64().unwrap();
            assert!(h > 34 && h < 500, "closing must actually shrink: {mid}");
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
        assert_eq!(closed["bars"].as_array().unwrap().len(), bars.len());
        println!("cycle {cycle}: {side} surface preserved, expanded, and restored");
    }
    if std::env::var_os("LUCY_SOFTWARE").is_none() {
        socket.send_to(b"home", &server).unwrap();
        thread::sleep(Duration::from_millis(450));
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
        thread::sleep(Duration::from_millis(350));
        assert_eq!(status()["size"][1], 500);
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(350));
        assert!(status()["borrowed_from"].is_null());
    }
    if let Some(p) = pointer.as_mut() {
        for header in [true, false] {
            socket.send_to(b"calendar", &server).unwrap();
            thread::sleep(Duration::from_millis(450));
            if header {
                let x = (p.width - status()["size"][0].as_f64().unwrap()) / 2.0;
                p.click(x + 166.0, 28.0);
            } else {
                p.click(20.0, 55.0);
            }
            assert_eq!(status()["visible"], false);
            thread::sleep(Duration::from_millis(100));
            assert!(status()["borrowed_from"].is_null());
        }
        println!("enlarged clock and outside clicks both close the panel");
    }
}
