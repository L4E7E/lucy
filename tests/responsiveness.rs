//! Opt-in real Wayland regression: ordinary unit tests never open desktop windows.
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Preview {
    child: Child,
    dir: PathBuf,
}
impl Drop for Preview {
    fn drop(&mut self) {
        // Only this test's child and its private temporary directory are owned here.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
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
#[ignore = "opens a read-only Wayland preview; run explicitly in a desktop session"]
fn idle_reopen_advances_without_unrelated_input() {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("Wayland session required"));
    let display =
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").expect("Wayland session required"));
    let display = if display.is_absolute() {
        display
    } else {
        runtime.join(display)
    };
    let dir = std::env::temp_dir().join(format!(
        "lucy-ipc-test.{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_lucy-shell"))
        .args(["control", "--isolated", "--preview"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("WAYLAND_DISPLAY", display)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let preview = Preview { child, dir };
    let socket = UnixDatagram::bind(preview.dir.join("test.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let server = preview.dir.join("lucy-control-native-preview.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.send_to(b"@status", &server).is_err() {
        assert!(Instant::now() < deadline, "preview failed to start");
        thread::sleep(Duration::from_millis(10));
    }
    let mut buffer = [0; 2048];
    socket
        .recv(&mut buffer)
        .expect("preview event loop failed to answer");
    let status = || {
        socket.send_to(b"@status", &server).unwrap();
        let mut bytes = [0; 2048];
        let n = socket.recv(&mut bytes).expect("UI event loop stalled");
        serde_json::from_slice::<serde_json::Value>(&bytes[..n]).unwrap()
    };
    for cycle in 0..12 {
        socket.send_to(b"@hide", &server).unwrap();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(status()["visible"], false);
        let start = Instant::now();
        socket.send_to(b"home", &server).unwrap();
        loop {
            let s = status();
            if s["presented"].as_u64().unwrap() > 0 {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "first frame stalled: {s}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let latency = start.elapsed();
        // Do not inject mouse/key/repaint events while awaiting animation progress.
        thread::sleep(Duration::from_millis(400));
        let s = status();
        assert_eq!(
            s["size"],
            serde_json::json!([480, 500]),
            "animation stalled: {s}"
        );
        if std::env::var_os("LUCY_SOFTWARE").is_none() {
            assert!(
                s["presented"].as_u64().unwrap() > 1,
                "no GPU frame progress: {s}"
            );
        }
        println!("reopen {cycle}: first frame {latency:?}");
    }
}
