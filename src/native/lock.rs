//! Security boundary: ext-session-lock-v1, separate process, system PAM.
//! Never unlock on Escape, a failed auth, helper failure, or a compositor error.
use super::{
    auth::{self, Secret},
    keyboard::Keyboard,
    lock_draw,
};
use gio::prelude::SettingsExt;
use memmap2::MmapMut;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_registry, wl_seat,
        wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::{
    ext::session_lock::v1::client::{
        ext_session_lock_manager_v1 as manager, ext_session_lock_surface_v1 as locked,
        ext_session_lock_v1 as session,
    },
    wp::{
        fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
        viewporter::client::{wp_viewport, wp_viewporter},
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Waiting,
    Locked,
    Authenticated,
    Finished,
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum Probe {
    None,
    Auth,
    Prompt,
    Pixels,
    Motion,
}
impl Phase {
    fn authenticated(&mut self, success: bool) -> bool {
        if *self == Self::Locked && success {
            *self = Self::Authenticated;
            true
        } else {
            false
        }
    }
}
struct Output {
    object: wl_output::WlOutput,
    name: String,
    scale: i32,
    physical: (i32, i32),
}
struct Buffer {
    object: wl_buffer::WlBuffer,
    map: MmapMut,
    busy: bool,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.object.destroy();
    }
}
struct Window {
    // EGL wrappers must be destroyed before wl_surface.
    gpu: Option<super::gpu::Window>,
    surface: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    scale: Option<f64>,
    frame_pending: bool,
    frame_ready: bool,
    presented: u64,
    rasterized: u64,
    lock: Option<locked::ExtSessionLockSurfaceV1>,
    layer: Option<layer::ZwlrLayerSurfaceV1>,
    size: (i32, i32),
    ready: bool,
    buffers: Vec<Buffer>,
    background: Option<cairo::ImageSurface>,
    sharp: Option<cairo::ImageSurface>,
}
impl Drop for Window {
    fn drop(&mut self) {
        self.gpu.take();
        if let Some(l) = &self.lock {
            l.destroy();
        }
        if let Some(l) = &self.layer {
            l.destroy();
        }
        self.viewport.destroy();
        if let Some(f) = &self.fractional {
            f.destroy();
        }
        self.surface.destroy();
    }
}
struct Client {
    #[cfg(test)]
    verify_pixels: bool,
    #[cfg(test)]
    pixel_checks: u32,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    manager: Option<manager::ExtSessionLockManagerV1>,
    shell: Option<shell::ZwlrLayerShellV1>,
    lock: Option<session::ExtSessionLockV1>,
    outputs: HashMap<u32, Output>,
    windows: HashMap<u32, Window>,
    gpu: Option<std::rc::Rc<super::gpu::Gpu>>,
    keyboard: Keyboard,
    seat: Option<wl_seat::WlSeat>,
    keyboard_proxy: Option<wl_keyboard::WlKeyboard>,
    secret: Secret,
    phase: Phase,
    preview: bool,
    busy: bool,
    error: bool,
    dirty: bool,
    started: bool,
    quit: bool,
    font: String,
    auth_tx: mpsc::Sender<Secret>,
    auth_rx: mpsc::Receiver<bool>,
    ready_stdout: bool,
    ready_notify: bool,
    unlock_at: Option<Instant>,
    repeat: Option<(u32, Instant)>,
    repeat_rate: i32,
    repeat_delay: u64,
}
impl Client {
    fn window(&mut self, id: u32, qh: &QueueHandle<Self>) {
        if self.windows.contains_key(&id) {
            return;
        }
        let output = &self.outputs[&id];
        let surface = self.compositor.as_ref().unwrap().create_surface(qh, ());
        let viewport = self
            .viewporter
            .as_ref()
            .unwrap()
            .get_viewport(&surface, qh, ());
        let fractional = self
            .fractional
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, id));
        let (lock, layer) = if self.preview {
            let l = self.shell.as_ref().unwrap().get_layer_surface(
                &surface,
                Some(&output.object),
                shell::Layer::Overlay,
                "lucy-lock-preview".into(),
                qh,
                id,
            );
            l.set_anchor(
                layer::Anchor::Top
                    | layer::Anchor::Bottom
                    | layer::Anchor::Left
                    | layer::Anchor::Right,
            );
            l.set_size(0, 0);
            l.set_exclusive_zone(-1);
            l.set_keyboard_interactivity(if std::env::var_os("LUCY_UI_TEST_NO_INPUT").is_some() {
                let empty = self.compositor.as_ref().unwrap().create_region(qh, ());
                surface.set_input_region(Some(&empty));
                empty.destroy();
                layer::KeyboardInteractivity::None
            } else {
                layer::KeyboardInteractivity::Exclusive
            });
            surface.commit();
            (None, Some(l))
        } else {
            (
                Some(self.lock.as_ref().unwrap().get_lock_surface(
                    &surface,
                    &output.object,
                    qh,
                    id,
                )),
                None,
            )
        };
        // Only before requesting the lock: a locked session cannot be captured.
        let background = None;
        self.windows.insert(
            id,
            Window {
                surface,
                viewport,
                fractional,
                scale: None,
                frame_pending: false,
                frame_ready: false,
                presented: 0,
                rasterized: 0,
                lock,
                layer,
                gpu: None,
                size: (0, 0),
                ready: false,
                buffers: vec![],
                background,
                sharp: None,
            },
        );
    }
    fn configure(&mut self, id: u32, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if let Some(w) = self.windows.get_mut(&id) {
            w.size = (width as i32, height as i32);
            w.ready = true;
            self.dirty = true;
        }
    }
    fn render(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn std::error::Error>> {
        for (id, w) in &mut self.windows {
            if !w.ready {
                continue;
            }
            let o = &self.outputs[id];
            let scale = w
                .scale
                .unwrap_or_else(|| (f64::from(o.physical.0) / f64::from(w.size.0)).clamp(1.0, 4.0));
            let (width, height) = (
                (f64::from(w.size.0) * scale).ceil() as i32,
                (f64::from(w.size.1) * scale).ceil() as i32,
            );
            if width > 16384 || height > 16384 {
                return Err("잠금 화면 크기가 범위를 벗어났습니다".into());
            }
            if self.preview && std::env::var_os("LUCY_LOCK_TRACE").is_some() {
                eprintln!(
                    "Lucy lock raster: surface={}x{}, buffer={}x{}, scale={scale}",
                    w.size.0, w.size.1, width, height
                );
            }
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
            w.rasterized += 1;
            {
                let cr = cairo::Context::new(&image)?;
                cr.scale(scale, scale);
                lock_draw::paint(
                    &cr,
                    f64::from(w.size.0),
                    f64::from(w.size.1),
                    w.background.as_ref(),
                    &self.font,
                    self.secret.len(),
                    self.busy,
                    self.error,
                    self.preview,
                );
            }
            w.viewport.set_destination(w.size.0, w.size.1);
            let region = self.compositor.as_ref().unwrap().create_region(qh, ());
            region.add(0, 0, w.size.0, w.size.1);
            w.surface.set_opaque_region(Some(&region));
            region.destroy();
            if w.gpu.is_none() {
                if let Some(gpu) = self.gpu.clone() {
                    w.gpu = unsafe {
                        super::gpu::Window::new(gpu, w.surface.id().as_ptr().cast(), width, height)
                    };
                }
            }
            let mut bytes = image.data()?;
            if let Some(g) = w.gpu.as_mut() {
                let motion_allowed = {
                    #[cfg(test)]
                    {
                        !self.verify_pixels
                    }
                    #[cfg(not(test))]
                    {
                        true
                    }
                };
                if motion_allowed {
                    if let Some(mut sharp) = w.sharp.take() {
                        let (sw, sh) = (sharp.width(), sharp.height());
                        if let Ok(bytes) = sharp.data() {
                            g.set_lock_background(&bytes, sw, sh);
                        }
                    }
                }
                g.set_background(0.0);
                g.update(&bytes, width, height, super::gpu::Effect::Immediate, true);
                #[cfg(test)]
                if self.verify_pixels {
                    g.verify_text_copy(&bytes, w.size);
                    self.pixel_checks += 1;
                }
                let shine = !self.busy && self.secret.len() == 0;
                let mut mask = shine
                    .then(|| lock_draw::prompt_mask(&self.font, scale))
                    .flatten();
                if let Some((mask, bounds)) = mask.as_mut() {
                    let (mw, mh) = (mask.width(), mask.height());
                    if let Ok(bytes) = mask.data() {
                        g.set_header(&bytes, mw, mh, bounds.w as f32);
                        let entry =
                            lock_draw::prompt_rect(f64::from(w.size.0), f64::from(w.size.1));
                        g.set_shimmer(
                            Some(super::model::Rect {
                                x: entry.x + bounds.x,
                                y: entry.y,
                                ..*bounds
                            }),
                            w.size,
                        );
                    }
                } else {
                    g.set_shimmer(None, w.size);
                }
                if (g.shimmer_active() || g.lock_animating()) && !w.frame_pending {
                    w.surface.frame(qh, *id);
                    w.frame_pending = true;
                }
                w.frame_ready = false;
                if g.present(None) {
                    w.presented += 1;
                    continue;
                }
                w.gpu.take();
            }
            let len = (width as usize) * (height as usize) * 4;
            let index = w.buffers.iter().position(|b| !b.busy && b.map.len() == len);
            let index = if let Some(i) = index {
                i
            } else {
                // Never modify a buffer the compositor still owns.
                if w.buffers.len() >= 3 {
                    self.dirty = true;
                    continue;
                }
                let fd =
                    unsafe { libc::memfd_create(c"lucy-lock-frame".as_ptr(), libc::MFD_CLOEXEC) };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let file = unsafe { File::from_raw_fd(fd) };
                file.set_len(len as u64)?;
                let map = unsafe { MmapMut::map_mut(&file)? };
                let pool = self
                    .shm
                    .as_ref()
                    .unwrap()
                    .create_pool(file.as_fd(), len as i32, qh, ());
                let object = pool.create_buffer(
                    0,
                    width,
                    height,
                    width * 4,
                    wl_shm::Format::Argb8888,
                    qh,
                    *id,
                );
                pool.destroy();
                w.buffers.push(Buffer {
                    object,
                    map,
                    busy: false,
                });
                w.buffers.len() - 1
            };
            let b = &mut w.buffers[index];
            b.map.copy_from_slice(&bytes);
            b.busy = true;
            bytes.fill(0);
            w.surface.attach(Some(&b.object), 0, 0);
            w.surface.damage_buffer(0, 0, width, height);
            w.surface.commit();
        }
        Ok(())
    }
    fn animate(&mut self, qh: &QueueHandle<Self>) {
        let shine = !self.busy && self.secret.len() == 0;
        for (id, w) in &mut self.windows {
            if !std::mem::take(&mut w.frame_ready) || !w.ready {
                continue;
            }
            let Some(gpu) = w.gpu.as_mut() else {
                continue;
            };
            if !(gpu.lock_animating() || shine && gpu.shimmer_active()) {
                continue;
            }
            if !w.frame_pending {
                w.surface.frame(qh, *id);
                w.frame_pending = true;
            }
            if !gpu.present(None) {
                w.gpu.take();
                self.dirty = true;
            } else {
                w.presented += 1;
                if self.preview
                    && std::env::var_os("LUCY_LOCK_TRACE").is_some()
                    && w.presented % 120 == 0
                {
                    eprintln!(
                        "Lucy lock shine: frames={}, rasterized={}",
                        w.presented, w.rasterized
                    );
                }
            }
        }
    }
    fn key(&mut self, key: u32) {
        if self.busy {
            return;
        }
        let (sym, mut text, ctrl) = self.keyboard.private_key(key);
        match sym {
            0xff1b => {
                self.secret.clear();
                self.error = false;
                if self.preview {
                    self.phase = Phase::Authenticated;
                    self.begin_release();
                }
            }
            0xff08 => self.secret.backspace(),
            0xff0d | 0xff8d => {
                if self.preview {
                    self.secret.clear();
                } else if self.phase == Phase::Locked && self.secret.len() > 0 {
                    if let Ok(empty) = Secret::new() {
                        let secret = std::mem::replace(&mut self.secret, empty);
                        self.busy = self.auth_tx.send(secret).is_ok();
                        self.error = !self.busy;
                    }
                }
            }
            _ if ctrl && matches!(text.as_str(), "u" | "\u{15}") => self.secret.clear(),
            _ if !ctrl => {
                self.secret.append(&text);
                self.error = false;
            }
            _ => (),
        }
        // XKB's temporary UTF-8 string is never retained or logged.
        unsafe {
            libc::explicit_bzero(text.as_mut_ptr().cast(), text.len());
        }
        self.dirty = true;
    }
    fn begin_release(&mut self) {
        self.busy = true;
        self.repeat = None;
        self.secret.clear();
        self.unlock_at = Some(Instant::now() + Duration::from_millis(320));
        for w in self.windows.values_mut() {
            if let Some(gpu) = &mut w.gpu {
                gpu.release_lock();
            }
            w.frame_ready = true;
        }
    }
    fn result(&mut self, success: bool) {
        self.busy = false;
        if self.phase.authenticated(success) {
            self.begin_release();
        } else {
            self.secret.clear();
            self.error = true;
            self.dirty = true;
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match e {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, version.min(4), qh, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, qh, ())),
                "wp_viewporter" => s.viewporter = Some(r.bind(name, 1, qh, ())),
                "wp_fractional_scale_manager_v1" => s.fractional = Some(r.bind(name, 1, qh, ())),
                "ext_session_lock_manager_v1" => s.manager = Some(r.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" if s.preview => {
                    s.shell = Some(r.bind(name, version.min(4), qh, ()))
                }
                "wl_seat" if s.seat.is_none() => {
                    s.seat = Some(r.bind(name, version.min(7), qh, ()))
                }
                "wl_output" => {
                    let object = r.bind(name, version.min(4), qh, name);
                    s.outputs.insert(
                        name,
                        Output {
                            object,
                            name: String::new(),
                            scale: 1,
                            physical: (1920, 1080),
                        },
                    );
                }
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } => {
                s.windows.remove(&name);
                if let Some(o) = s.outputs.remove(&name) {
                    o.object.release();
                }
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for Client {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        e: wl_output::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let Some(o) = s.outputs.get_mut(id) {
            match e {
                wl_output::Event::Name { name } => o.name = name,
                wl_output::Event::Scale { factor } => o.scale = factor.max(1),
                wl_output::Event::Mode {
                    flags: WEnum::Value(flags),
                    width,
                    height,
                    ..
                } if flags.contains(wl_output::Mode::Current) => o.physical = (width, height),
                wl_output::Event::Done if s.started => s.window(*id, qh),
                _ => (),
            }
        }
    }
}
impl Dispatch<session::ExtSessionLockV1, ()> for Client {
    fn event(
        s: &mut Self,
        _: &session::ExtSessionLockV1,
        e: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            session::Event::Locked => {
                s.phase = Phase::Locked;
                if s.ready_stdout {
                    let _ = std::io::stdout().write_all(b"L");
                    let _ = std::io::stdout().flush();
                    s.ready_stdout = false;
                }
                if s.ready_notify {
                    notify_ready();
                    s.ready_notify = false;
                }
            }
            session::Event::Finished => {
                s.phase = Phase::Finished;
                s.quit = true;
            }
            _ => (),
        }
    }
}
impl Dispatch<locked::ExtSessionLockSurfaceV1, u32> for Client {
    fn event(
        s: &mut Self,
        l: &locked::ExtSessionLockSurfaceV1,
        e: locked::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let locked::Event::Configure {
            serial,
            width,
            height,
        } = e
        {
            l.ack_configure(serial);
            s.configure(*id, width, height);
        }
    }
}
impl Dispatch<layer::ZwlrLayerSurfaceV1, u32> for Client {
    fn event(
        s: &mut Self,
        l: &layer::ZwlrLayerSurfaceV1,
        e: layer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            layer::Event::Configure {
                serial,
                width,
                height,
            } => {
                l.ack_configure(serial);
                s.configure(*id, width, height);
            }
            layer::Event::Closed => s.quit = true,
            _ => (),
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        s: &mut Self,
        seat: &wl_seat::WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(c),
        } = e
        {
            if c.contains(wl_seat::Capability::Keyboard) && s.keyboard_proxy.is_none() {
                s.keyboard_proxy = Some(seat.get_keyboard(qh, ()));
            }
            if !c.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = s.keyboard_proxy.take() {
                    k.release();
                }
                s.repeat = None;
            }
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        e: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => s.keyboard.keymap(fd, size),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => s
                .keyboard
                .modifiers(mods_depressed, mods_latched, mods_locked, group),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                s.key(key);
                let sym = s.keyboard.symbol(key);
                if !matches!(sym, 0xff0d | 0xff8d | 0xff1b) && s.repeat_rate > 0 {
                    s.repeat = Some((key, Instant::now() + Duration::from_millis(s.repeat_delay)));
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Released),
                ..
            } => {
                if s.repeat.is_some_and(|r| r.0 == key) {
                    s.repeat = None;
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                s.repeat_rate = rate;
                s.repeat_delay = delay.max(0) as u64;
            }
            wl_keyboard::Event::Leave { .. } => s.repeat = None,
            _ => (),
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, u32> for Client {
    fn event(
        s: &mut Self,
        b: &wl_buffer::WlBuffer,
        e: wl_buffer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(e, wl_buffer::Event::Release) {
            if let Some(w) = s.windows.get_mut(id) {
                for v in &mut w.buffers {
                    if v.object == *b {
                        v.busy = false;
                    }
                }
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u32> for Client {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(w) = s.windows.get_mut(id) {
            w.frame_pending = false;
            w.frame_ready = true;
        }
    }
}
impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for Client {
    fn event(
        s: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(w) = s.windows.get_mut(id) {
                w.scale = Some(f64::from(scale) / 120.0);
                s.dirty = true;
            }
        }
    }
}
delegate_noop!(Client:ignore wl_compositor::WlCompositor);
delegate_noop!(Client:ignore wl_shm::WlShm);
delegate_noop!(Client:ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client:ignore wl_surface::WlSurface);
delegate_noop!(Client:ignore wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(Client:ignore manager::ExtSessionLockManagerV1);
delegate_noop!(Client:ignore wp_viewporter::WpViewporter);
delegate_noop!(Client:ignore wp_viewport::WpViewport);
delegate_noop!(Client:ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(Client:ignore shell::ZwlrLayerShellV1);

fn protect_process() -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn notify_ready() {
    use std::os::{unix::ffi::OsStrExt, unix::net::UnixDatagram};
    let Some(name) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let bytes = name.as_bytes();
    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    if bytes.is_empty() || bytes.len() >= 108 {
        return;
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, b) in bytes.iter().enumerate() {
        address.sun_path[i] = if i == 0 && *b == b'@' {
            0
        } else {
            *b as libc::c_char
        };
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        + bytes.len()
        + usize::from(bytes[0] != b'@');
    let message = b"READY=1\nSTATUS=Wayland session locked";
    unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_NOSIGNAL,
            (&address as *const libc::sockaddr_un).cast(),
            len as libc::socklen_t,
        );
    }
}
fn daemonize() -> Result<(), Box<dyn std::error::Error>> {
    // A dedicated unit escapes the bar/idle watcher's cgroup. Restarting the
    // shell must not kill the lock renderer. Type=notify waits for Locked.
    let service = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState",
            "--value",
            "lucy-lock.service",
        ])
        .output();
    if service
        .is_ok_and(|r| r.status.success() && String::from_utf8_lossy(&r.stdout).trim() == "loaded")
    {
        return if Command::new("systemctl")
            .args(["--user", "start", "lucy-lock.service"])
            .status()?
            .success()
        {
            Ok(())
        } else {
            Err("Lucy 잠금 서비스를 시작하지 못했습니다".into())
        };
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return Err("독립된 잠금을 위해 lucy-lock.service를 먼저 설치하세요".into());
    }
    let mut child = Command::new(std::env::current_exe()?)
        .args(["lock", "--ready-stdout"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut ready = child.stdout.take().unwrap();
    let mut byte = [0];
    let result = ready.read_exact(&mut byte);
    if result.is_ok() && byte[0] == b'L' {
        Ok(())
    } else {
        let _ = child.wait();
        Err("세션 잠금을 확보하지 못했습니다".into())
    }
}
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.iter().any(|s| {
        !matches!(
            s.as_str(),
            "--daemonize" | "--preview" | "--ready-stdout" | "--ready-notify"
        )
    }) {
        return Err("지원하지 않는 잠금 옵션입니다".into());
    }
    if args.iter().any(|s| s == "--preview") && args.len() != 1 {
        return Err("잠금 미리보기는 다른 잠금 옵션과 함께 사용할 수 없습니다".into());
    }
    if args.iter().any(|s| s == "--daemonize") {
        return daemonize();
    }
    let preview = args.iter().any(|s| s == "--preview");
    protect_process()?;
    if !preview && !auth::available() {
        return Err("시스템 swaylock PAM 인증 설정이 필요합니다".into());
    }
    let runtime = std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?);
    let guard = OpenOptions::new()
        .create(true)
        .append(true)
        .open(runtime.join(if preview {
            "lucy-lock-preview.lock"
        } else {
            "lucy-lock.lock"
        }))?;
    if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Lucy 잠금화면이 이미 실행 중입니다".into());
    }
    let (tx, rx) = mpsc::channel::<Secret>();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lucy-lock-pam".into())
        .spawn(move || {
            while let Ok(secret) = rx.recv() {
                if result_tx.send(auth::authenticate(secret)).is_err() {
                    break;
                }
            }
        })?;
    client(
        preview,
        args.iter().any(|s| s == "--ready-stdout"),
        args.iter().any(|s| s == "--ready-notify"),
        tx,
        result_rx,
        #[cfg(test)]
        Probe::None,
    )
}
fn client(
    preview: bool,
    ready_stdout: bool,
    ready_notify: bool,
    auth_tx: mpsc::Sender<Secret>,
    auth_rx: mpsc::Receiver<bool>,
    #[cfg(test)] probe: Probe,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let font = gio::Settings::new("org.gnome.desktop.interface")
        .string("font-name")
        .to_string();
    let mut s = Client {
        #[cfg(test)]
        verify_pixels: probe == Probe::Pixels,
        #[cfg(test)]
        pixel_checks: 0,
        compositor: None,
        shm: None,
        viewporter: None,
        fractional: None,
        manager: None,
        shell: None,
        lock: None,
        outputs: HashMap::new(),
        windows: HashMap::new(),
        gpu: None,
        keyboard: Keyboard::new(),
        seat: None,
        keyboard_proxy: None,
        secret: Secret::new()?,
        phase: Phase::Waiting,
        preview,
        busy: false,
        error: false,
        dirty: false,
        started: false,
        quit: false,
        font,
        auth_tx,
        auth_rx,
        ready_stdout,
        ready_notify,
        unlock_at: None,
        repeat: None,
        repeat_rate: 25,
        repeat_delay: 600,
    };
    queue.roundtrip(&mut s)?;
    queue.roundtrip(&mut s)?;
    if s.compositor.is_none()
        || s.shm.is_none()
        || s.viewporter.is_none()
        || (!preview && s.manager.is_none())
        || (preview && s.shell.is_none())
    {
        return Err("필요한 Wayland 잠금 프로토콜이 없습니다".into());
    }
    // Capture before requesting the lock (get_lock_surface cannot be committed yet).
    let captures: HashMap<_, _> = s
        .outputs
        .iter()
        .filter_map(|(id, o)| lock_draw::capture(&o.name).map(|i| (*id, i)))
        .collect();
    s.gpu = super::gpu::Gpu::new(&connection);
    if !preview {
        s.lock = Some(s.manager.as_ref().unwrap().lock(&qh, ()));
    }
    s.started = true;
    for id in s.outputs.keys().copied().collect::<Vec<_>>() {
        s.window(id, &qh);
        if let Some(capture) = captures.get(&id) {
            let w = s.windows.get_mut(&id).unwrap();
            w.background = Some(capture.blurred.clone());
            w.sharp = Some(capture.sharp.clone());
        }
    }
    drop(captures);
    connection.flush()?;
    let mut last_minute = glib::DateTime::now_local()?.minute();
    #[cfg(test)]
    let mut stage = 0;
    #[cfg(test)]
    let deadline = Instant::now() + Duration::from_secs(12);
    #[cfg(test)]
    let (mut held_frames, mut held_rasters, mut hold_until) = (0, 0, Instant::now());
    #[cfg(test)]
    let (mut entering, mut leaving) = (0, 0);
    loop {
        queue.dispatch_pending(&mut s)?;
        if s.quit {
            break;
        }
        if let Ok(success) = s.auth_rx.try_recv() {
            s.result(success);
        }
        // Authentication is the only real-lock path to this state. Keep the
        // compositor's lock and input grab until the visual release has ended.
        if s.phase == Phase::Authenticated && s.unlock_at.is_some_and(|t| Instant::now() >= t) {
            if !s.preview {
                s.lock.as_ref().unwrap().unlock_and_destroy();
            }
            connection.flush()?;
            queue.roundtrip(&mut s)?;
            break;
        }
        if let Some((key, deadline)) = s.repeat {
            if Instant::now() >= deadline {
                s.key(key);
                s.repeat = Some((
                    key,
                    Instant::now() + Duration::from_millis(1000 / s.repeat_rate.max(1) as u64),
                ));
            }
        }
        let minute = glib::DateTime::now_local()?.minute();
        if minute != last_minute {
            s.dirty = true;
            last_minute = minute;
        }
        if s.dirty {
            s.dirty = false;
            s.render(&qh)?;
        }
        s.animate(&qh);
        #[cfg(test)]
        if probe == Probe::Motion {
            assert!(Instant::now() < deadline, "lock blur animation stalled");
            assert!(
                s.lock.is_none(),
                "motion preview must never lock the desktop"
            );
            let frames: u64 = s.windows.values().map(|w| w.presented).sum();
            let rasters: u64 = s.windows.values().map(|w| w.rasterized).sum();
            let amounts: Vec<_> = s
                .windows
                .values()
                .filter_map(|w| w.gpu.as_ref().and_then(|g| g.lock_amount()))
                .collect();
            if stage == 0 && amounts.iter().any(|a| *a > 0.02 && *a < 0.98) {
                entering += 1;
            }
            if stage == 0 && !amounts.is_empty() && amounts.iter().all(|a| *a >= 0.9999) {
                assert_eq!(
                    amounts.len(),
                    s.windows.len(),
                    "desktop capture/GPU blur must be available"
                );
                assert!(entering >= 3, "entry must contain intermediate blur frames");
                assert_eq!(s.phase, Phase::Waiting);
                held_frames = frames;
                held_rasters = rasters;
                hold_until = Instant::now();
                s.phase = Phase::Authenticated; // Preview only: no PAM or compositor lock.
                s.begin_release();
                stage = 1;
            } else if stage == 1 && amounts.iter().any(|a| *a > 0.02 && *a < 0.98) {
                leaving += 1;
            }
        }
        #[cfg(test)]
        if probe == Probe::Pixels {
            assert!(Instant::now() < deadline, "GPU readback preview timed out");
            if !s.windows.is_empty() && s.windows.values().all(|w| w.ready && w.scale.is_some()) {
                assert!(s.pixel_checks >= s.windows.len() as u32);
                assert!(s.windows.values().all(|w| w.gpu.is_some()));
                assert!(s.lock.is_none(), "pixel test must never lock the desktop");
                s.quit = true;
                continue;
            }
        }
        #[cfg(test)]
        if probe == Probe::Auth {
            assert!(
                Instant::now() < deadline,
                "nested lock test timed out at {stage}: {:?}",
                s.phase
            );
            if s.phase == Phase::Locked && stage == 0 {
                assert!(s.windows.values().all(|w| w.ready));
                s.key(1);
                assert!(!s.quit, "Escape must not unlock");
                assert_eq!(s.phase, Phase::Locked);
                let mut secret = Secret::new()?;
                secret.append("incorrect");
                s.busy = true;
                s.auth_tx.send(secret)?;
                stage = 1;
            } else if stage == 1 && s.error {
                assert_eq!(
                    s.phase,
                    Phase::Locked,
                    "wrong authentication must keep the session locked"
                );
                let mut secret = Secret::new()?;
                secret.append("nested-test");
                s.busy = true;
                s.auth_tx.send(secret)?;
                stage = 2;
            }
        }
        #[cfg(test)]
        if probe == Probe::Prompt {
            assert!(
                Instant::now() < deadline,
                "prompt animation stalled at stage {stage}"
            );
            let frames: u64 = s.windows.values().map(|w| w.presented).sum();
            let rasters: u64 = s.windows.values().map(|w| w.rasterized).sum();
            let outputs = s.windows.len() as u64;
            if stage == 0 && frames >= 200 {
                assert!(
                    rasters <= outputs * 4,
                    "GPU animation must not rerasterize the lock screen"
                );
                assert!(s
                    .windows
                    .values()
                    .all(|w| w.gpu.as_ref().is_some_and(|g| g.shimmer_active())));
                println!("prompt: {frames} GPU frames, {rasters} Cairo rasterizations");
                s.secret.append("preview-only");
                s.dirty = true;
                held_frames = frames;
                held_rasters = rasters;
                hold_until = Instant::now() + Duration::from_millis(350);
                stage = 1;
            } else if stage == 1 && Instant::now() >= hold_until {
                assert!(
                    frames <= held_frames + outputs,
                    "typing must stop shimmer frame callbacks"
                );
                assert_eq!(rasters, held_rasters + outputs);
                println!("prompt: animation stopped while password dots are visible");
                s.secret.clear();
                s.dirty = true;
                held_frames = frames;
                stage = 2;
            } else if stage == 2 && frames >= held_frames + 120 {
                println!("prompt: animation resumed after clearing the preview input");
                stage = 3;
                s.quit = true;
            }
        }
        connection.flush()?;
        if let Some(read) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: connection.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = if s.unlock_at.is_some() {
                10
            } else if s.busy {
                40
            } else if let Some((_, t)) = s.repeat {
                t.saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, 1000) as i32
            } else {
                1000
            };
            let result = unsafe { libc::poll(&mut fd, 1, timeout) };
            if result > 0 {
                read.read()?;
            } else if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    if !preview && s.phase == Phase::Finished {
        return Err("잠금 요청이 거부되었거나 다른 잠금 클라이언트가 종료했습니다".into());
    }
    #[cfg(test)]
    if probe == Probe::Auth {
        assert_eq!(stage, 2);
        assert_eq!(s.phase, Phase::Authenticated);
    }
    #[cfg(test)]
    if probe == Probe::Prompt {
        assert_eq!(stage, 3);
        assert!(s.lock.is_none(), "prompt test must never lock the desktop");
        assert_eq!(s.phase, Phase::Waiting);
    }
    #[cfg(test)]
    if probe == Probe::Pixels {
        assert!(s.pixel_checks > 0);
        assert!(s.lock.is_none());
        assert_eq!(s.phase, Phase::Waiting);
    }
    #[cfg(test)]
    if probe == Probe::Motion {
        let frames: u64 = s.windows.values().map(|w| w.presented).sum();
        let rasters: u64 = s.windows.values().map(|w| w.rasterized).sum();
        assert_eq!(stage, 1);
        assert!(leaving >= 3 && frames > held_frames + 3);
        assert!(hold_until.elapsed() >= Duration::from_millis(300));
        assert!(
            rasters <= held_rasters + 1,
            "blur must animate entirely on the GPU"
        );
        assert!(s.windows.values().all(|w| w
            .gpu
            .as_ref()
            .and_then(|g| g.lock_amount())
            .is_some_and(|a| a <= 0.0001)));
        println!("lock blur: {entering} entry samples, {leaving} release samples, {frames} GPU frames, {rasters} Cairo rasterizations");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_success_after_locked_can_unlock() {
        let mut phase = Phase::Waiting;
        assert!(!phase.authenticated(true));
        assert!(!phase.authenticated(false));
        phase = Phase::Locked;
        assert!(!phase.authenticated(false));
        assert_eq!(phase, Phase::Locked);
        assert!(phase.authenticated(true));
        assert_eq!(phase, Phase::Authenticated);
        assert!(!phase.authenticated(true));
        phase = Phase::Finished;
        assert!(!phase.authenticated(true));
    }
    #[test]
    #[ignore = "requires an explicitly isolated nested Niri socket, never the live desktop"]
    fn nested_compositor_locks_rejects_wrong_auth_and_unlocks_after_success() {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
        let display = std::env::var("WAYLAND_DISPLAY").unwrap();
        assert!(runtime.starts_with("/tmp/lucy-osd-lock."));
        assert!(
            display.starts_with(&format!("{runtime}/")),
            "must never lock the host compositor"
        );
        let (tx, rx) = mpsc::channel::<Secret>();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            while let Ok(secret) = rx.recv() {
                if result_tx.send(secret.len() == 11).is_err() {
                    break;
                }
            }
        });
        client(false, false, false, tx, result_rx, Probe::Auth).unwrap();
        worker.join().unwrap();
    }
    #[test]
    #[ignore = "opens a harmless lock preview; requires a real Wayland EGL session"]
    fn gpu_prompt_animates_without_rasterizing_and_pauses_during_input() {
        let (tx, _rx) = mpsc::channel::<Secret>();
        let (_result_tx, result_rx) = mpsc::channel();
        client(true, false, false, tx, result_rx, Probe::Prompt).unwrap();
    }
    #[test]
    #[ignore = "opens an inputless preview and compares EGL pixels; requires a real Wayland GPU session"]
    fn gpu_preserves_lock_and_joined_notification_text_coverage() {
        assert!(std::env::var_os("LUCY_UI_TEST_NO_INPUT").is_some());
        let (tx, _rx) = mpsc::channel::<Secret>();
        let (_result_tx, result_rx) = mpsc::channel();
        client(true, false, false, tx, result_rx, Probe::Pixels).unwrap();
    }
    #[test]
    #[ignore = "opens an inputless preview; requires a real Wayland GPU session"]
    fn gpu_blur_enters_and_releases_without_rerasterizing_or_locking() {
        assert!(std::env::var_os("LUCY_UI_TEST_NO_INPUT").is_some());
        let (tx, _rx) = mpsc::channel::<Secret>();
        let (_result_tx, result_rx) = mpsc::channel();
        client(true, false, false, tx, result_rx, Probe::Motion).unwrap();
    }
}
