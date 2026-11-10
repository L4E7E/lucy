//! Opt-in Niri input regression helper. One persistent virtual pointer, no tools
//! or system configuration changes; dropping the connection removes the device.
use std::{collections::HashMap, process::Command, thread, time::Duration};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_output,
        wl_pointer::{Axis, ButtonState},
        wl_registry,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1 as manager, zwlr_virtual_pointer_v1 as pointer,
};

#[derive(Default)]
struct State {
    manager: Option<manager::ZwlrVirtualPointerManagerV1>,
    outputs: HashMap<u32, (wl_output::WlOutput, String)>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    s.manager = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    s.outputs
                        .insert(output.id().protocol_id(), (output, String::new()));
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            s.outputs.get_mut(&output.id().protocol_id()).unwrap().1 = name;
        }
    }
}
delegate_noop!(State: manager::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: pointer::ZwlrVirtualPointerV1);

pub struct Pointer {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    device: pointer::ZwlrVirtualPointerV1,
    pub width: f64,
    height: f64,
}
impl Pointer {
    pub fn new(name: &str) -> Self {
        let connection = Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).unwrap();
        queue.roundtrip(&mut state).unwrap();
        let output = &state
            .outputs
            .values()
            .find(|(_, n)| n == name)
            .expect("named output")
            .0;
        let device = state
            .manager
            .as_ref()
            .expect("virtual pointer v2")
            .create_virtual_pointer_with_output(None, Some(output), &qh, ());
        let result = Command::new("niri")
            .args(["msg", "-j", "outputs"])
            .output()
            .unwrap();
        assert!(result.status.success(), "input regression requires Niri");
        let outputs: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        let logical = &outputs[name]["logical"];
        Self {
            connection,
            queue,
            state,
            device,
            width: logical["width"].as_f64().unwrap(),
            height: logical["height"].as_f64().unwrap(),
        }
    }
    pub fn click(&mut self, x: f64, y: f64) {
        self.device.motion_absolute(
            1,
            x.round() as u32,
            y.round() as u32,
            self.width as u32,
            self.height as u32,
        );
        // Motion and press belong to one pointer frame: a physical pointer must
        // not move between the test's target placement and button press.
        self.device.button(2, 0x110, ButtonState::Pressed);
        self.device.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
        self.device.button(3, 0x110, ButtonState::Released);
        self.device.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
        self.connection.flush().unwrap();
        thread::sleep(Duration::from_millis(240));
    }
    pub fn wheel(&mut self, x: f64, y: f64, value: f64) {
        self.device.motion_absolute(
            4,
            x.round() as u32,
            y.round() as u32,
            self.width as u32,
            self.height as u32,
        );
        self.device.axis(5, Axis::VerticalScroll, value);
        self.device.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    #[allow(dead_code)]
    pub fn drag(&mut self, x: f64, y: f64, dx: f64, dy: f64) {
        self.drag_with_preview(x, y, dx, dy, || {});
    }
    #[allow(dead_code)]
    pub fn drag_with_preview(&mut self, x: f64, y: f64, dx: f64, dy: f64, preview: impl FnOnce()) {
        self.device.motion_absolute(
            1,
            x.round() as u32,
            y.round() as u32,
            self.width as u32,
            self.height as u32,
        );
        self.device.button(2, 0x110, ButtonState::Pressed);
        self.device.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
        thread::sleep(Duration::from_millis(80));
        for i in 1..=8 {
            self.device.motion_absolute(
                3,
                (x + dx * f64::from(i) / 8.0).round() as u32,
                (y + dy * f64::from(i) / 8.0).round() as u32,
                self.width as u32,
                self.height as u32,
            );
            self.device.frame();
            self.queue.roundtrip(&mut self.state).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        preview();
        // A screenshot/inspection may take long enough for another input device
        // to move. Make the tested drop coordinate and release one pointer frame.
        self.device.motion_absolute(
            4,
            (x + dx).round() as u32,
            (y + dy).round() as u32,
            self.width as u32,
            self.height as u32,
        );
        self.device.button(4, 0x110, ButtonState::Released);
        self.device.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
        thread::sleep(Duration::from_millis(250));
    }
}
