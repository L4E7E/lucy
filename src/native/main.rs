//! Lucy: GTK-free Wayland surfaces with cached content and EGL transitions.
#[path = "../audio_backend.rs"]
mod audio_backend;
mod auth;
#[path = "../bar_backend.rs"]
mod bar_backend;
mod battery;
mod bluetooth;
#[path = "../control_backend.rs"]
mod control_backend;
#[path = "../dismiss.rs"]
mod dismiss;
mod display;
mod draw;
mod expansion;
mod gamma;
mod gpu;
mod host;
mod icons;
mod ime;
mod keyboard;
mod launcher;
mod lock;
mod lock_draw;
mod lock_glass;
mod model;
#[path = "../network_backend.rs"]
mod network_backend;
mod notification_server;
mod notifications;
mod osd;
mod power;
mod scroll;
mod text_cache;
mod tray;
mod verification;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    glib::set_application_name("Lucy");
    glib::set_prgname(Some("lucy-shell"));
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "lock") {
        return lock::run(&args[2..]);
    }
    if args.get(1).is_some_and(|a| a == "osd") {
        return osd::cli(&args[2..]);
    }
    if std::env::args().any(|a| a == "--version") {
        println!("Lucy 0.3.0 (native Wayland; no GTK)");
        return Ok(());
    }
    if std::env::args().any(|a| a == "--self-test-notifications") {
        return notification_server::self_test();
    }
    if std::env::args().any(|a| a.starts_with("--self-test") && a != "--self-test-native") {
        return verification::run();
    }
    host::run()
}
