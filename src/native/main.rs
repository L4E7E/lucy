//! Lucy: GTK-free Wayland surfaces with cached content and EGL transitions.
#[path = "../audio_backend.rs"]
mod audio_backend;
#[path = "../bar_backend.rs"]
mod bar_backend;
#[path = "../control_backend.rs"]
mod control_backend;
#[path = "../dismiss.rs"]
mod dismiss;
mod draw;
mod expansion;
mod gpu;
mod host;
mod icons;
mod keyboard;
mod launcher;
mod model;
#[path = "../network_backend.rs"]
mod network_backend;
mod notification_server;
mod notifications;
mod tray;
mod verification;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    glib::set_application_name("Lucy");
    glib::set_prgname(Some("lucy-shell"));
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
