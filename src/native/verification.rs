//! Read actual device data, but never issue a write during a self-test.
use super::{
    draw,
    model::{Action, Model, Page, Slider},
};
use crate::{audio_backend as audio, control_backend as control, network_backend as net};
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut m = Model::new();
    for domain in control::DOMAINS {
        control::refresh(&mut m.common, domain);
    }
    m.audio = audio::snapshot()?;
    m.network = Some(net::state()?);
    let (tx, rx) = std::sync::mpsc::channel();
    m.common_tx = Some(tx);
    m.common.profile = Some("performance".into());
    m.common.profiles = vec![
        "power-saver".into(),
        "balanced".into(),
        "performance".into(),
    ];
    for expected in ["power-saver", "balanced", "performance"] {
        m.activate(Action::Profile);
        match rx.recv_timeout(std::time::Duration::from_millis(100))? {
            control::Request::Write(control::Action::Profile(p)) => assert_eq!(p, expected),
            _ => panic!("profile click must send exactly one cycle action"),
        }
    }
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, draw::WIDTH, 600)?;
    let cr = cairo::Context::new(&surface)?;
    for page in [
        Page::Home,
        Page::Audio,
        Page::Network,
        Page::Bluetooth,
        Page::Power,
        Page::Display,
        Page::Calendar,
        Page::Session,
    ] {
        m.navigate(page);
        let h = draw::popup_height(&m);
        assert!((180..=600).contains(&h));
        let scene = draw::popup(&cr, &m, 1, h);
        assert!(!scene.hits.is_empty());
    }
    m.navigate(Page::Home);
    let scene = draw::popup(&cr, &m, 1, draw::popup_height(&m));
    let slider = scene
        .hits
        .iter()
        .find(|h| matches!(h.action, Action::Slider(Slider::Volume)))
        .unwrap();
    assert!(slider.rect.x + slider.rect.w < f64::from(draw::WIDTH));
    m.navigate(Page::Network);
    m.entry = true;
    m.insert("test-한글");
    m.move_cursor(-1);
    m.backspace();
    assert_eq!(m.secret, "test-글");
    m.selected_all = true;
    m.insert("draft-only");
    m.hide();
    assert_eq!(m.secret, "draft-only");
    m.navigate(Page::Audio);
    m.navigate(Page::Network);
    assert_eq!(m.secret, "draft-only");
    println!("PASS: GTK-free scenes; live audio/network reads; quiet/balanced/performance cycle via mock channel; uniform layouts; UTF-8 editing and hidden draft preservation. No system writes.");
    Ok(())
}
