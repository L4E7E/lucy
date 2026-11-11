//! Outside-click surfaces share one pixel. The session scrim is translucent;
//! Niri blurs the live windows beneath it rather than a captured wallpaper.
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer,
};

pub struct Backdrop {
    stop: Arc<AtomicBool>,
    wake: UnixStream,
    worker: Option<thread::JoinHandle<()>>,
}
const SESSION_PIXEL: [u8; 4] = [5, 4, 4, 76];
impl Backdrop {
    pub fn new(namespace: &str, clicked: glib::Sender<()>) -> Result<Self, String> {
        Self::with_pixel(namespace, clicked, [0; 4])
    }
    pub fn session(clicked: glib::Sender<()>) -> Result<Self, String> {
        Self::with_pixel("lucy-session-backdrop", clicked, SESSION_PIXEL)
    }
    fn with_pixel(
        namespace: &str,
        clicked: glib::Sender<()>,
        pixel: [u8; 4],
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let namespace = namespace.to_owned();
        let (ready, rx) = mpsc::channel();
        let (wake, wake_rx) = UnixStream::pair().map_err(|e| e.to_string())?;
        let worker = thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
                let mut queue = connection.new_event_queue();
                let qh = queue.handle();
                connection.display().get_registry(&qh, ());
                let mut state = State {
                    namespace,
                    clicked,
                    compositor: None,
                    shm: None,
                    shell: None,
                    viewporter: None,
                    buffer: None,
                    outputs: HashMap::new(),
                    surfaces: HashMap::new(),
                    initialized: false,
                };
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                // Settle the output globals before creating fullscreen catchers.
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                if state.compositor.is_none()
                    || state.shm.is_none()
                    || state.shell.is_none()
                    || state.viewporter.is_none()
                {
                    return Err("layer-shell and viewporter required".into());
                }
                // SAFETY: owned memfd, four zero-filled bytes supplied by ftruncate.
                let fd = unsafe {
                    libc::memfd_create(c"lucy-dismiss-pixel".as_ptr(), libc::MFD_CLOEXEC)
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let mut file = unsafe { File::from_raw_fd(fd) };
                if unsafe { libc::ftruncate(file.as_raw_fd(), 4) } != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                file.write_all(&pixel).map_err(|e| e.to_string())?;
                let pool = state
                    .shm
                    .as_ref()
                    .unwrap()
                    .create_pool(file.as_fd(), 4, &qh, ());
                state.buffer =
                    Some(pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, ()));
                pool.destroy();
                state.initialized = true;
                let ids: Vec<_> = state.outputs.keys().copied().collect();
                for id in ids {
                    state.add_surface(id, &qh);
                }
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                // Commit the catcher before the popup maps above it.
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                let _ = ready.send(Ok(()));
                while !stopping.load(Ordering::Acquire) {
                    queue
                        .dispatch_pending(&mut state)
                        .map_err(|e| e.to_string())?;
                    connection.flush().map_err(|e| e.to_string())?;
                    if let Some(guard) = queue.prepare_read() {
                        let mut fds = [
                            libc::pollfd {
                                fd: connection.as_fd().as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: wake_rx.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                        if fds[1].revents != 0 {
                            break;
                        }
                        if result > 0 && fds[0].revents != 0 {
                            guard.read().map_err(|e| e.to_string())?;
                        } else if result < 0
                            && std::io::Error::last_os_error().kind()
                                != std::io::ErrorKind::Interrupted
                        {
                            return Err(std::io::Error::last_os_error().to_string());
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = ready.send(Err(error.clone()));
                eprintln!("Lucy dismiss: {error}");
            }
        });
        let guard = Self {
            stop,
            wake,
            worker: Some(worker),
        };
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(guard),
            Ok(Err(error)) => Err(error),
            Err(e) => Err(e.to_string()),
        }
    }
}
impl Drop for Backdrop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Surface {
    surface: wl_surface::WlSurface,
    layer: layer::ZwlrLayerSurfaceV1,
    viewport: wp_viewport::WpViewport,
}
impl Drop for Surface {
    fn drop(&mut self) {
        self.layer.destroy();
        self.viewport.destroy();
        self.surface.destroy();
    }
}
struct State {
    namespace: String,
    clicked: glib::Sender<()>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<shell::ZwlrLayerShellV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    buffer: Option<wl_buffer::WlBuffer>,
    outputs: HashMap<u32, wl_output::WlOutput>,
    surfaces: HashMap<u32, Surface>,
    initialized: bool,
}
impl State {
    fn add_surface(&mut self, id: u32, qh: &QueueHandle<Self>) {
        if !self.initialized || self.surfaces.contains_key(&id) {
            return;
        }
        let surface = self.compositor.as_ref().unwrap().create_surface(qh, ());
        let viewport = self
            .viewporter
            .as_ref()
            .unwrap()
            .get_viewport(&surface, qh, ());
        let layer = self.shell.as_ref().unwrap().get_layer_surface(
            &surface,
            self.outputs.get(&id),
            // Expanded panels reuse an already mapped bar surface. Updating its
            // layer does not reorder it above a newer surface in the same layer.
            // Keep their outside-click catcher below the panel, including input.
            if self.namespace == "lucy-expanded-dismiss" {
                shell::Layer::Top
            } else {
                shell::Layer::Overlay
            },
            self.namespace.clone(),
            qh,
            id,
        );
        layer.set_anchor(
            layer::Anchor::Top | layer::Anchor::Bottom | layer::Anchor::Left | layer::Anchor::Right,
        );
        layer.set_size(0, 0);
        if self.namespace == "lucy-expanded-dismiss" {
            layer.set_margin(42, 0, 0, 0);
        }
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(layer::KeyboardInteractivity::None);
        surface.commit();
        self.surfaces.insert(
            id,
            Surface {
                surface,
                layer,
                viewport,
            },
        );
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    state
                        .outputs
                        .insert(name, registry.bind(name, version.min(4), qh, name));
                    state.add_surface(name, qh);
                }
                "wl_seat" => {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ());
                }
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.surfaces.remove(&name);
                if let Some(output) = state.outputs.remove(&name) {
                    if wayland_client::Proxy::version(&output) >= 3 {
                        output.release();
                    }
                }
            }
            _ => (),
        }
    }
}
impl Dispatch<layer::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &layer::ZwlrLayerSurfaceV1,
        event: layer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            layer::Event::Configure {
                serial,
                width,
                height,
            } => {
                if let Some(surface) = state.surfaces.get(id) {
                    surface.layer.ack_configure(serial);
                    surface
                        .viewport
                        .set_destination(width.max(1) as i32, height.max(1) as i32);
                    surface.surface.attach(state.buffer.as_ref(), 0, 0);
                    surface.surface.damage(0, 0, i32::MAX, i32::MAX);
                    surface.surface.commit();
                }
            }
            layer::Event::Closed => {
                state.surfaces.remove(id);
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            let _ = state.clicked.send(());
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { .. } = event {
            state.add_surface(*id, qh);
        }
    }
}
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore shell::ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    #[test]
    fn session_scrim_is_premultiplied_and_leaves_live_windows_visible() {
        let [blue, green, red, alpha] = super::SESSION_PIXEL;
        assert!((1..255).contains(&alpha));
        assert!(blue <= alpha && green <= alpha && red <= alpha);
        let integration = include_str!("../integration/niri.kdl");
        let rule = integration
            .split("match namespace=\"^lucy-session-backdrop$\"")
            .nth(1)
            .unwrap()
            .split("layer-rule")
            .next()
            .unwrap();
        assert!(rule.contains("blur true"));
        assert!(rule.contains("xray false"));
    }
}
