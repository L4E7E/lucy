use super::{
    draw,
    keyboard::Keyboard,
    model::{Action, Model, Page, Slider},
    tray,
};
use crate::{
    audio_backend as audio, bar_backend as bar, control_backend as control, network_backend as net,
};
use gio::prelude::*;
use glib::ControlFlow;
use memmap2::MmapMut;
use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, net::UnixDatagram},
    },
    path::PathBuf,
    rc::Rc,
    thread,
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::wp::{
    cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1},
    fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
    text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3},
    viewporter::client::{wp_viewport, wp_viewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer,
};

#[derive(Clone)]
enum Role {
    Popup,
    Calendar,
    Launcher,
    Session,
    Expanded(String, String),
    Notice(u32),
    Bar(String, String),
}
struct Output {
    object: wl_output::WlOutput,
    name: String,
    scale: f64,
}
struct Buffer {
    object: wl_buffer::WlBuffer,
    map: MmapMut,
    width: i32,
    height: i32,
    busy: bool,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.object.destroy();
    }
}
struct Window {
    native_id: u32,
    expanded_from: i32,
    gpu: Option<super::gpu::Window>,
    qh: QueueHandle<Client>,
    page: Page,
    scroll: f64,
    query: String,
    bar_interaction: (bool, Option<(u32, usize)>),
    display_size: (i32, i32),
    margin_top: i32,
    presented: u64,
    surface: wl_surface::WlSurface,
    layer: layer::ZwlrLayerSurfaceV1,
    viewport: wp_viewport::WpViewport,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    role: Role,
    width: i32,
    height: i32,
    scale: f64,
    ready: bool,
    buffers: Vec<Buffer>,
    dirty: bool,
    frame: Option<wl_callback::WlCallback>,
}
impl Drop for Window {
    fn drop(&mut self) {
        self.gpu.take();
        self.layer.destroy();
        if let Some(f) = &self.fractional {
            f.destroy();
        }
        self.viewport.destroy();
        self.surface.destroy();
    }
}
impl Window {
    fn present(&mut self, _id: u32) -> bool {
        let Some(gpu) = self.gpu.as_ref() else {
            return false;
        };
        let expansion = gpu.expansion_frame();
        let (zoom, shift) = if matches!(self.role, Role::Bar(_, _) | Role::Expanded(_, _)) {
            (1.0, 0.0)
        } else {
            gpu.geometry(matches!(self.role, Role::Notice(_)))
        };
        let size = expansion.map_or(
            (
                (self.width as f32 * zoom).round().max(1.0) as i32,
                (self.height as f32 * zoom).round().max(1.0) as i32,
            ),
            |f| f.size,
        );
        if self.display_size != size {
            self.layer.set_size(size.0 as u32, size.1 as u32);
            self.display_size = size;
        }
        self.viewport.set_destination(size.0, size.1);
        match self.role {
            Role::Notice(_) => self.layer.set_margin(
                self.margin_top
                    + ((self.height + self.margin_top + 16) as f32 * shift).round() as i32,
                0,
                0,
                0,
            ),
            Role::Popup => self.layer.set_margin(54 + shift.round() as i32, 16, 0, 0),
            Role::Calendar => self.layer.set_margin(54 + shift.round() as i32, 0, 0, 0),
            Role::Launcher | Role::Session => {
                self.layer.set_margin((shift * 2.0).round() as i32, 0, 0, 0)
            }
            Role::Bar(_, _) => {}
            Role::Expanded(_, _) => self.layer.set_margin(6, 8, 0, 8),
        }
        self.frame = Some(self.surface.frame(&self.qh, self.native_id));
        let ok = gpu.present(expansion);
        self.presented += u64::from(ok);
        ok
    }
}

struct Client {
    gpu: Option<Rc<super::gpu::Gpu>>,
    gpu_failed: bool,
    connection: Connection,
    model: Model,
    bar: bool,
    initialized: bool,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<shell::ZwlrLayerShellV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    cursor_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    cursor: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    seat: Option<wl_seat::WlSeat>,
    text_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    input_enabled: bool,
    input_rectangle: Option<(i32, i32, i32, i32)>,
    input_surrounding: Option<(String, usize, bool)>,
    keyboard: Keyboard,
    keyboard_proxy: Option<wl_keyboard::WlKeyboard>,
    repeat: Option<(u32, std::time::Instant)>,
    repeat_delay: u64,
    repeat_rate: i32,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,
    offer: Option<wl_data_offer::WlDataOffer>,
    offer_mimes: Vec<String>,
    paste: bool,
    outputs: HashMap<u32, Output>,
    windows: HashMap<u32, Window>,
    pointer_window: Option<u32>,
    pointer_xy: (f64, f64),
    pointer_size: (i32, i32),
    serial: u32,
    next_id: u32,
    backdrop: Option<crate::dismiss::Backdrop>,
    clicked: glib::Sender<()>,
    wakeup: glib::Sender<()>,
    wakeup_pending: bool,
    loan_id: Option<u32>,
    automated_preview: bool,
    loop_: glib::MainLoop,
}
impl Client {
    fn new(
        connection: Connection,
        bar: bool,
        preview: bool,
        clicked: glib::Sender<()>,
        wakeup: glib::Sender<()>,
        loop_: glib::MainLoop,
    ) -> Self {
        Self {
            gpu: None,
            gpu_failed: false,
            connection,
            model: Model::new(),
            bar,
            initialized: false,
            compositor: None,
            shm: None,
            shell: None,
            viewporter: None,
            fractional: None,
            cursor_manager: None,
            cursor: None,
            pointer: None,
            seat: None,
            text_manager: None,
            text_input: None,
            input_enabled: false,
            input_rectangle: None,
            input_surrounding: None,
            keyboard: Keyboard::new(),
            keyboard_proxy: None,
            repeat: None,
            repeat_delay: 600,
            repeat_rate: 25,
            data_manager: None,
            data_device: None,
            offer: None,
            offer_mimes: vec![],
            paste: false,
            outputs: HashMap::new(),
            windows: HashMap::new(),
            pointer_window: None,
            pointer_xy: (0.0, 0.0),
            pointer_size: (1, 1),
            serial: 0,
            next_id: 10,
            backdrop: None,
            clicked,
            wakeup,
            wakeup_pending: false,
            loan_id: None,
            automated_preview: preview && env::var_os("LUCY_UI_TEST_NO_INPUT").is_some(),
            loop_,
        }
    }
    fn create(
        &mut self,
        id: u32,
        role: Role,
        output: Option<&wl_output::WlOutput>,
        scale: f64,
        qh: &QueueHandle<Self>,
    ) {
        let surface = self.compositor.as_ref().unwrap().create_surface(qh, id);
        if self.automated_preview {
            let region = self.compositor.as_ref().unwrap().create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        let viewport = self
            .viewporter
            .as_ref()
            .unwrap()
            .get_viewport(&surface, qh, ());
        let fractional = self
            .fractional
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, id));
        let namespace = match &role {
            Role::Popup => "lucy-control-popup".to_owned(),
            Role::Calendar => "lucy-calendar-popup".to_owned(),
            Role::Launcher => "lucy-launcher-popup".to_owned(),
            Role::Session => "lucy-session-popup".to_owned(),
            Role::Notice(_) => "lucy-notification-popup".to_owned(),
            Role::Bar(side, _) => format!("lucy-bar-{side}"),
            Role::Expanded(_, _) => unreachable!("expanded panels reuse an existing bar"),
        };
        let layer = self.shell.as_ref().unwrap().get_layer_surface(
            &surface,
            output,
            if matches!(
                role,
                Role::Popup | Role::Calendar | Role::Launcher | Role::Session | Role::Notice(_)
            ) {
                shell::Layer::Overlay
            } else {
                shell::Layer::Top
            },
            namespace,
            qh,
            id,
        );
        let (w, h) = match &role {
            Role::Notice(id) => (draw::NOTICE_WIDTH, draw::notice_height(&self.model, *id)),
            Role::Popup | Role::Calendar | Role::Launcher | Role::Session => (
                draw::popup_width(&self.model),
                draw::popup_height(&self.model),
            ),
            Role::Bar(side, _) => {
                if side == "left" {
                    layer.set_anchor(layer::Anchor::Top | layer::Anchor::Left);
                } else if side == "right" {
                    layer.set_anchor(layer::Anchor::Top | layer::Anchor::Right);
                } else {
                    layer.set_anchor(layer::Anchor::Top);
                }
                (200, 34)
            }
            Role::Expanded(_, _) => unreachable!(),
        };
        if let Role::Notice(_) = role {
            layer.set_anchor(layer::Anchor::Top);
            layer.set_margin(54, 0, 0, 0);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(layer::KeyboardInteractivity::None);
        } else if matches!(
            role,
            Role::Popup | Role::Calendar | Role::Launcher | Role::Session
        ) {
            if matches!(role, Role::Launcher | Role::Session) {
                layer.set_anchor(layer::Anchor::empty());
            } else if matches!(role, Role::Calendar) {
                layer.set_anchor(layer::Anchor::Top);
                layer.set_margin(54, 0, 0, 0);
            } else {
                layer.set_anchor(layer::Anchor::Top | layer::Anchor::Right);
                layer.set_margin(54, 16, 0, 0);
            }
            layer.set_keyboard_interactivity(layer::KeyboardInteractivity::Exclusive);
            layer.set_exclusive_zone(-1);
        } else {
            layer.set_margin(6, 8, 0, 8);
            layer.set_keyboard_interactivity(layer::KeyboardInteractivity::None);
            layer.set_exclusive_zone(-1);
        }
        layer.set_size(w as u32, h as u32);
        if self.automated_preview {
            layer.set_keyboard_interactivity(layer::KeyboardInteractivity::None);
        }
        surface.commit();
        self.windows.insert(
            id,
            Window {
                native_id: id,
                expanded_from: 0,
                gpu: None,
                qh: qh.clone(),
                page: self.model.page,
                scroll: self.model.scroll,
                query: self.model.launcher.query.clone(),
                bar_interaction: (self.model.tray_open, None),
                display_size: (w, h),
                margin_top: 54,
                presented: 0,
                surface,
                layer,
                viewport,
                fractional,
                role,
                width: w,
                height: h,
                scale,
                ready: false,
                buffers: vec![],
                dirty: true,
                frame: None,
            },
        );
    }
    fn bars(&mut self, id: u32, qh: &QueueHandle<Self>) {
        let Some(output) = self.outputs.get(&id) else {
            return;
        };
        if output.name.is_empty() {
            return;
        }
        let (object, name, scale) = (output.object.clone(), output.name.clone(), output.scale);
        for side in ["left", "center", "right"] {
            if self
                .windows
                .values()
                .any(|w| matches!(&w.role,Role::Bar(s,n)|Role::Expanded(s,n)if s==side&&n==&name))
            {
                continue;
            }
            self.next_id += 1;
            self.create(
                self.next_id,
                Role::Bar(side.into(), name.clone()),
                Some(&object),
                scale,
                qh,
            );
        }
    }
    fn show(&mut self, page: &str, qh: &QueueHandle<Self>) {
        self.show_at(page, None, qh);
    }
    fn popup_notifications(
        &mut self,
        notices: Vec<super::notifications::Notice>,
        qh: &QueueHandle<Self>,
    ) {
        let ids: Vec<_> = notices.iter().map(|n| n.id).collect();
        let closing: Vec<_> = self
            .windows
            .iter()
            .filter_map(|(id, w)| match w.role {
                Role::Notice(n) if !ids.contains(&n) => Some(*id),
                _ => None,
            })
            .collect();
        for id in closing {
            let w = self.windows.get_mut(&id).unwrap();
            if let Some(gpu) = w.gpu.as_mut() {
                if !gpu.closing() {
                    gpu.close();
                    if w.frame.is_none() {
                        w.present(id);
                    }
                }
            } else {
                self.windows.remove(&id);
                self.model.hits.remove(&id);
            }
        }
        self.model.popup_notices = notices;
        let focused = self
            .model
            .workspaces
            .iter()
            .find(|w| w["is_focused"] == true)
            .and_then(|w| w["output"].as_str());
        let output = self
            .outputs
            .values()
            .find(|o| Some(o.name.as_str()) == focused)
            .or_else(|| self.outputs.values().next())
            .map(|o| (o.object.clone(), o.scale));
        let mut margin = 54;
        for notice in ids {
            let id = (notice & 0x3fff_ffff) | 0x4000_0000;
            if self
                .windows
                .get(&id)
                .is_some_and(|w| w.gpu.as_ref().is_some_and(|g| g.closing()))
            {
                self.windows.remove(&id);
            }
            if !self.windows.contains_key(&id) {
                let (ref object, scale) = output.clone().map_or((None, 1.0), |(o, s)| (Some(o), s));
                self.create(id, Role::Notice(notice), object.as_ref(), scale, qh);
            }
            if let Some(w) = self.windows.get_mut(&id) {
                w.margin_top = margin;
                w.layer.set_margin(margin, 0, 0, 0);
            }
            margin += draw::notice_height(&self.model, notice) + 8;
        }
        self.redraw(qh);
    }
    fn show_at(&mut self, page: &str, output_name: Option<&str>, qh: &QueueHandle<Self>) {
        let target = Page::parse(page);
        let reversible=self.windows.get(&1).is_some_and(|w| {
            matches!(&w.role,Role::Expanded(side,output) if (side=="center")== (target==Page::Calendar) && !matches!(target,Page::Session|Page::Launcher|Page::TrayMenu) && output_name.is_none_or(|n|n==output)) && w.gpu.as_ref().is_some_and(|g|g.closing())
        });
        if reversible {
            self.model.navigate(target);
            self.model.visible = true;
            self.windows
                .get_mut(&1)
                .unwrap()
                .gpu
                .as_mut()
                .unwrap()
                .reopen_expansion();
            self.redraw(qh);
            return;
        }
        if self
            .windows
            .get(&1)
            .is_some_and(|w| w.gpu.as_ref().is_some_and(|g| g.closing()))
        {
            self.hide_immediate();
        }
        let calendar = Page::parse(page) == Page::Calendar;
        let launcher = Page::parse(page) == Page::Launcher;
        let session = Page::parse(page) == Page::Session;
        if self.windows.get(&1).is_some_and(|w| {
            (matches!(w.role, Role::Calendar)
                || matches!(&w.role, Role::Expanded(side,_) if side == "center"))
                != calendar
                || matches!(w.role, Role::Launcher) != launcher
                || matches!(w.role, Role::Session) != session
        }) {
            self.hide_immediate();
        }
        if ["vpn", "ethernet"].contains(&page) {
            self.model.tab = page.into();
        }
        self.model.navigate(Page::parse(page));
        self.model.visible = true;
        if !self.windows.contains_key(&1) {
            // Opening a panel must not spawn and wait for niri on the UI thread.
            let name = output_name.map(str::to_owned).or_else(|| {
                self.model
                    .workspaces
                    .iter()
                    .find(|w| w["is_focused"] == true)
                    .and_then(|w| w["output"].as_str())
                    .map(str::to_owned)
            });
            let output = self
                .outputs
                .values()
                .find(|o| Some(&o.name) == name.as_ref())
                .or_else(|| self.outputs.values().next())
                .map(|o| (o.object.clone(), o.scale));
            let side = if calendar { "center" } else { "right" };
            if self.bar && !launcher && !session && Page::parse(page) != Page::TrayMenu {
                let source = self.windows.iter().find(|(_, w)| matches!(&w.role, Role::Bar(s,n) if s == side && name.as_ref().is_none_or(|wanted| wanted == n))).map(|(id,_)| *id);
                if let Some(source) = source {
                    self.backdrop = if self.automated_preview {
                        None
                    } else {
                        crate::dismiss::Backdrop::new("lucy-expanded-dismiss", self.clicked.clone())
                            .ok()
                    };
                    let mut win = self.windows.remove(&source).unwrap();
                    let Role::Bar(side, output) = win.role.clone() else {
                        unreachable!()
                    };
                    win.expanded_from = win.width;
                    win.role = Role::Expanded(side, output);
                    win.layer.set_layer(shell::Layer::Overlay);
                    win.layer
                        .set_keyboard_interactivity(layer::KeyboardInteractivity::Exclusive);
                    if self.automated_preview {
                        win.layer
                            .set_keyboard_interactivity(layer::KeyboardInteractivity::None);
                    }
                    self.loan_id = Some(source);
                    if self.pointer_window == Some(source) {
                        self.pointer_window = Some(1);
                        self.pointer_size = win.display_size;
                        self.model.hover = None;
                    }
                    self.model.hits.remove(&source);
                    self.windows.insert(1, win);
                    self.redraw(qh);
                    return;
                }
            }
            self.backdrop = if session {
                crate::dismiss::Backdrop::session(self.clicked.clone())
            } else {
                crate::dismiss::Backdrop::new(
                    if launcher {
                        "lucy-launcher-dismiss"
                    } else if calendar {
                        "lucy-calendar-dismiss"
                    } else {
                        "lucy-control-dismiss"
                    },
                    self.clicked.clone(),
                )
            }
            .ok();
            let (object, scale) = output.map_or((None, 1.0), |(o, s)| (Some(o), s));
            self.create(
                1,
                if launcher {
                    Role::Launcher
                } else if calendar {
                    Role::Calendar
                } else if session {
                    Role::Session
                } else {
                    Role::Popup
                },
                object.as_ref(),
                scale,
                qh,
            );
        }
        self.redraw(qh);
    }
    fn hide(&mut self) {
        if let Some(win) = self.windows.get_mut(&1) {
            if let Some(gpu) = win.gpu.as_mut() {
                if gpu.closing() {
                    return;
                }
                self.model.hide();
                if !matches!(win.role, Role::Session | Role::Expanded(_, _)) {
                    self.backdrop = None;
                }
                self.repeat = None;
                gpu.close();
                if win.frame.is_none() {
                    win.present(1);
                }
                let _ = self.connection.flush();
                self.wake();
                return;
            }
        }
        self.hide_immediate();
    }
    fn hide_immediate(&mut self) {
        self.repeat = None;
        self.model.hide();
        self.input_state();
        self.backdrop = None;
        let window = self.windows.remove(&1);
        if let (Some(source), Some(mut win)) = (self.loan_id.take(), window) {
            if let Role::Expanded(side, output) = win.role.clone() {
                win.role = Role::Bar(side, output);
                win.layer.set_layer(shell::Layer::Top);
                win.layer
                    .set_keyboard_interactivity(layer::KeyboardInteractivity::None);
                win.layer.set_margin(6, 8, 0, 8);
                if let Some(gpu) = &mut win.gpu {
                    gpu.finish_expansion();
                }
                win.expanded_from = 0;
                win.dirty = true;
                win.page = self.model.page;
                win.bar_interaction = (self.model.tray_open, None);
                let qh = win.qh.clone();
                if self.pointer_window == Some(1) {
                    self.pointer_window = Some(source);
                    self.model.hover = None;
                }
                self.windows.insert(source, win);
                let _ = self.paint(source, &qh);
            }
        }
        if self.windows.is_empty() {
            self.gpu.take();
        }
        self.model.hits.remove(&1);
        let _ = self.connection.flush();
    }
    fn logical_id(&self, id: u32) -> u32 {
        if self.loan_id == Some(id) {
            1
        } else {
            id
        }
    }
    fn redraw(&mut self, qh: &QueueHandle<Self>) {
        if !self.bar && !self.model.visible {
            self.hide();
            return;
        }
        if std::mem::take(&mut self.model.ensure_entry_visible)
            && self.model.page == Page::Network
            && self.model.tab == "wifi"
        {
            if let Some(state) = &self.model.network {
                let index = self
                    .model
                    .wifi
                    .as_ref()
                    .and_then(|wifi| {
                        state
                            .aps
                            .iter()
                            .filter(|a| !a.active)
                            .position(|a| a.bssid == wifi.bssid)
                    })
                    .map_or(state.aps.iter().filter(|a| !a.active).count(), |i| i + 1);
                let bottom = index as f64 * draw::WIFI_ROW_PITCH + 50.0;
                let viewport = self.model.viewport_height.max(1.0);
                if bottom - self.model.scroll > viewport {
                    self.model.scroll = (bottom - viewport).max(0.0);
                }
            }
        }
        let ids: Vec<_> = self.windows.keys().copied().collect();
        for id in ids {
            if let Err(e) = self.paint(id, qh) {
                eprintln!("Lucy native draw: {e}");
            }
        }
        self.input_state();
        let _ = self.connection.flush();
        if self.gpu.is_some() {
            self.wake();
        }
    }
    fn wake(&mut self) {
        if !self.wakeup_pending {
            self.wakeup_pending = true;
            let _ = self.wakeup.send(());
        }
    }
    fn paint(&mut self, id: u32, qh: &QueueHandle<Self>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(win) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        win.dirty = true;
        if !win.ready || win.frame.is_some() {
            return Ok(());
        }
        if self.gpu.is_none() && !self.gpu_failed {
            self.gpu = super::gpu::Gpu::new(&self.connection);
            self.gpu_failed = self.gpu.is_none();
        }
        if win.gpu.as_ref().is_some_and(|g| g.closing()) {
            win.dirty = false;
            return Ok(());
        }
        let desired = match &win.role {
            Role::Notice(id) => (draw::NOTICE_WIDTH, draw::notice_height(&self.model, *id)),
            Role::Popup | Role::Calendar | Role::Launcher | Role::Session => (
                draw::popup_width(&self.model),
                draw::popup_height(&self.model),
            ),
            Role::Bar(side, output) => {
                let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
                let cr = cairo::Context::new(&s)?;
                (draw::bar_width(&cr, &self.model, side, output), 34)
            }
            Role::Expanded(side, output) => {
                let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
                let cr = cairo::Context::new(&image)?;
                (
                    draw::popup_width(&self.model).max(draw::bar_width(
                        &cr,
                        &self.model,
                        side,
                        output,
                    )),
                    draw::popup_height(&self.model),
                )
            }
        };
        if (win.width, win.height) != desired {
            win.width = desired.0;
            win.height = desired.1;
            win.layer.set_size(win.width as u32, win.height as u32);
        }
        let (w, h) = (
            (f64::from(win.width) * win.scale).ceil() as i32,
            (f64::from(win.height) * win.scale).ceil() as i32,
        );
        if win.gpu.is_none() {
            if let Some(gpu) = &self.gpu {
                // SAFETY: Window::drop releases EGL before destroying wl_surface.
                win.gpu = unsafe {
                    super::gpu::Window::new(gpu.clone(), win.surface.id().as_ptr().cast(), w, h)
                };
                if win.gpu.is_none() {
                    self.gpu_failed = true;
                    self.gpu.take();
                }
            }
        }
        if let Some(gpu) = win.gpu.as_mut() {
            gpu.set_background(
                if matches!(win.role, Role::Session | Role::Expanded(_, _)) {
                    0.0
                } else if matches!(win.role, Role::Notice(_)) {
                    0.58
                } else {
                    0.54
                },
            );
            let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h)?;
            let mut header_changed = false;
            {
                let cr = cairo::Context::new(&image)?;
                cr.scale(win.scale, win.scale);
                let mut options = cairo::FontOptions::new()?;
                options.set_antialias(cairo::Antialias::Gray);
                cr.set_font_options(&options);
                let render = |cr: &cairo::Context| match &win.role {
                    Role::Notice(notice) => {
                        draw::notice_popup(cr, &self.model, id, *notice, win.height)
                    }
                    Role::Bar(side, output) => {
                        draw::bar(cr, &self.model, id, side, output, win.width)
                    }
                    Role::Expanded(_, _) => draw::expanded(cr, &self.model, id, win.width),
                    _ => draw::popup(cr, &self.model, id, win.height),
                };
                let mut scene = render(&cr);
                if matches!(
                    win.role,
                    Role::Popup
                        | Role::Calendar
                        | Role::Launcher
                        | Role::Session
                        | Role::Expanded(_, _)
                ) {
                    self.model.content_height = scene.content_height;
                    self.model.viewport_height = scene.viewport_height;
                    let scroll = self
                        .model
                        .scroll
                        .clamp(0.0, (scene.content_height - scene.viewport_height).max(0.0));
                    if scroll != self.model.scroll {
                        self.model.scroll = scroll;
                        cr.set_operator(cairo::Operator::Clear);
                        cr.paint()?;
                        cr.set_operator(cairo::Operator::Over);
                        scene = if matches!(win.role, Role::Expanded(_, _)) {
                            draw::expanded(&cr, &self.model, id, win.width)
                        } else {
                            draw::popup(&cr, &self.model, id, win.height)
                        };
                    }
                }
                if let Role::Expanded(side, output) = &win.role {
                    let header_width = draw::bar_width(&cr, &self.model, side, output);
                    // Rasterize the clock at its final resolution, then let EGL
                    // scale it down during the bar-to-calendar transition.
                    let raster_scale = win.scale
                        * if side == "center" {
                            f64::from(super::expansion::CLOCK_SCALE)
                        } else {
                            1.0
                        };
                    let mut header = cairo::ImageSurface::create(
                        cairo::Format::ARgb32,
                        (f64::from(header_width) * raster_scale).ceil() as i32,
                        (34.0 * raster_scale).ceil() as i32,
                    )?;
                    let mut hs;
                    {
                        let hc = cairo::Context::new(&header)?;
                        hc.scale(raster_scale, raster_scale);
                        hc.set_font_options(&options);
                        hs = draw::bar_header(
                            &hc,
                            &self.model,
                            id,
                            side,
                            output,
                            header_width,
                            scene.hits.len(),
                        );
                    }
                    let header_frame = super::expansion::layout(
                        1.0,
                        win.expanded_from as f32,
                        (win.width, win.height),
                        header_width as f32,
                        side == "center",
                    );
                    let scale = f64::from(header_frame.header_scale);
                    for hit in &mut hs.hits {
                        hit.rect.x = hit.rect.x * scale + f64::from(header_frame.header.0);
                        hit.rect.y = hit.rect.y * scale + f64::from(header_frame.header.1);
                        hit.rect.w *= scale;
                        hit.rect.h *= scale;
                    }
                    scene.hits.extend(hs.hits);
                    header.flush();
                    let (hw, hh) = (header.width(), header.height());
                    header_changed = gpu.set_header(&header.data()?, hw, hh, header_width as f32);
                    gpu.begin_expansion(
                        win.expanded_from,
                        (win.width, win.height),
                        side == "center",
                    );
                }
                self.model.hits.insert(id, scene.hits);
            }
            image.flush();
            let interaction = (
                self.model.tray_open,
                self.model.hover.filter(|(window, _)| *window == id),
            );
            let effect = if matches!(win.role, Role::Expanded(_, _)) && gpu.expanding() {
                super::gpu::Effect::Immediate
            } else if matches!(win.role, Role::Bar(_, _)) && win.bar_interaction == interaction {
                // Numeric telemetry needs one frame, not a transition on every sample.
                super::gpu::Effect::Immediate
            } else if win.page != self.model.page && !matches!(win.role, Role::Bar(_, _)) {
                super::gpu::Effect::Slide
            } else if win.scroll != self.model.scroll || win.query != self.model.launcher.query {
                // Wheel/drag/search already produce successive viewport snapshots.
                // Crossfading them makes text trail and delays the actual scroll.
                super::gpu::Effect::Immediate
            } else if matches!(self.model.page, Page::Network | Page::Calendar)
                || self.model.animating()
            {
                // The tab highlight/icons already interpolate their geometry.
                // Do not crossfade these animation samples a second time.
                super::gpu::Effect::Immediate
            } else {
                super::gpu::Effect::Change
            };
            win.page = self.model.page;
            win.scroll = self.model.scroll;
            win.query.clone_from(&self.model.launcher.query);
            win.bar_interaction = interaction;
            let changed = gpu.update(&image.data()?, w, h, effect);
            if !changed && !header_changed && !gpu.animating() {
                win.dirty = false;
                return Ok(());
            }
            if !win.present(id) {
                return Err("EGL presentation failed".into());
            }
            win.dirty = false;
            // paint also runs from the first layer Configure, before redraw had
            // a GPU. eglSwapBuffers can read our frame callback into the queue;
            // no readable-fd edge remains to dispatch it unless we wake here.
            self.wake();
            return Ok(());
        }
        win.buffers
            .retain(|b| b.busy || (b.width, b.height) == (w, h));
        let index = if let Some(i) = win
            .buffers
            .iter()
            .position(|b| !b.busy && b.width == w && b.height == h)
        {
            i
        } else {
            if win.buffers.iter().filter(|b| b.busy).count() >= 3 {
                return Ok(());
            }
            let fd =
                unsafe { libc::memfd_create(c"lucy-native-frame".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let len = (w * h * 4) as usize;
            file.set_len(len as u64)?;
            let map = unsafe { MmapMut::map_mut(&file)? };
            let pool = self
                .shm
                .as_ref()
                .unwrap()
                .create_pool(file.as_fd(), len as i32, qh, ());
            let object =
                pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, win.native_id);
            pool.destroy();
            win.buffers.push(Buffer {
                object,
                map,
                width: w,
                height: h,
                busy: false,
            });
            win.buffers.len() - 1
        };
        let buffer = &mut win.buffers[index];
        {
            let surface = unsafe {
                cairo::ImageSurface::create_for_data_unsafe(
                    buffer.map.as_mut_ptr(),
                    cairo::Format::ARgb32,
                    w,
                    h,
                    w * 4,
                )?
            };
            let cr = cairo::Context::new(&surface)?;
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.paint()?;
            cr.set_operator(cairo::Operator::Over);
            let mut options = cairo::FontOptions::new()?;
            options.set_antialias(cairo::Antialias::Gray);
            cr.set_font_options(&options);
            cr.scale(win.scale, win.scale);
            let mut scene = match &win.role {
                Role::Notice(notice) => {
                    draw::notice_popup(&cr, &self.model, id, *notice, win.height)
                }
                Role::Popup | Role::Calendar | Role::Launcher | Role::Session => {
                    draw::popup(&cr, &self.model, id, win.height)
                }
                Role::Bar(side, output) => draw::bar(&cr, &self.model, id, side, output, win.width),
                Role::Expanded(side, output) => {
                    draw::glass_expanded_fallback(&cr, &self.model, id, side, output, win.width)
                }
            };
            if matches!(
                win.role,
                Role::Popup
                    | Role::Calendar
                    | Role::Launcher
                    | Role::Session
                    | Role::Expanded(_, _)
            ) {
                self.model.content_height = scene.content_height;
                self.model.viewport_height = scene.viewport_height;
                let scroll = self
                    .model
                    .scroll
                    .clamp(0.0, (scene.content_height - scene.viewport_height).max(0.0));
                if scroll != self.model.scroll {
                    self.model.scroll = scroll;
                    cr.set_operator(cairo::Operator::Source);
                    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
                    cr.paint()?;
                    cr.set_operator(cairo::Operator::Over);
                    scene = draw::popup(&cr, &self.model, id, win.height);
                }
            }
            self.model.hits.insert(id, scene.hits);
            surface.flush();
        }
        win.viewport.set_destination(win.width, win.height);
        win.surface.attach(Some(&buffer.object), 0, 0);
        win.surface.damage_buffer(0, 0, w, h);
        win.frame = Some(win.surface.frame(qh, win.native_id));
        win.surface.commit();
        win.display_size = (win.width, win.height);
        win.presented += 1;
        buffer.busy = true;
        win.dirty = false;
        Ok(())
    }
    fn input_state(&mut self) {
        let search = self.model.page == Page::Launcher;
        let surrounding = search.then(|| {
            (
                self.model.launcher.query.clone(),
                self.model.launcher.cursor,
                self.model.launcher.selected_all,
            )
        });
        let enabled =
            self.model.visible && self.model.entry && (self.model.page == Page::Network || search);
        let rectangle = self
            .model
            .hits
            .get(&1)
            .and_then(|hits| {
                hits.iter()
                    .find(|h| matches!(h.action, Action::Secret | Action::Search))
            })
            .map(|h| {
                (
                    h.rect.x as i32 + 12,
                    h.rect.y as i32 + 4,
                    h.rect.w as i32 - 20,
                    32,
                )
            });
        if let Some(input) = &self.text_input {
            if enabled
                && (!self.input_enabled
                    || self.input_rectangle != rectangle
                    || self.input_surrounding != surrounding)
            {
                if !self.input_enabled {
                    input.enable();
                    input.set_content_type(
                        if search {
                            zwp_text_input_v3::ContentHint::None
                        } else {
                            zwp_text_input_v3::ContentHint::HiddenText
                                | zwp_text_input_v3::ContentHint::SensitiveData
                        },
                        if search {
                            zwp_text_input_v3::ContentPurpose::Normal
                        } else {
                            zwp_text_input_v3::ContentPurpose::Password
                        },
                    );
                    input.set_surrounding_text(String::new(), 0, 0);
                }
                if search {
                    input.set_surrounding_text(
                        self.model.launcher.query.clone(),
                        self.model.launcher.cursor as i32,
                        if self.model.launcher.selected_all {
                            0
                        } else {
                            self.model.launcher.cursor as i32
                        },
                    );
                }
                if let Some((x, y, w, h)) = rectangle {
                    input.set_cursor_rectangle(x, y, w, h);
                }
                input.commit();
            } else if !enabled && self.input_enabled {
                input.disable();
                input.commit();
            }
        }
        self.input_rectangle = if enabled { rectangle } else { None };
        self.input_surrounding = if enabled { surrounding } else { None };
        self.input_enabled = enabled;
    }
    fn key(&mut self, key: u32, qh: &QueueHandle<Self>) -> bool {
        let (sym, text, ctrl) = self.keyboard.key(key);
        if sym == 0xff1b {
            self.hide();
            return false;
        }
        if self.model.page == Page::Launcher {
            match sym {
                0xff0d | 0xff8d => {
                    let id = self
                        .model
                        .launcher
                        .results()
                        .get(self.model.launcher.selected)
                        .map(|a| a.id.clone());
                    if let Some(id) = id {
                        self.model.activate(Action::App(id));
                        // Model::hide changes state; a unified bar keeps rendering
                        // while hidden, so explicitly close the launcher surface too.
                        self.hide();
                        return false;
                    }
                }
                0xff08 => self.model.backspace(),
                0xffff => self.model.delete(),
                0xff51 if ctrl => self.model.move_cursor(-1),
                0xff53 if ctrl => self.model.move_cursor(1),
                0xff51 => self.model.launcher.select(-1),
                0xff53 | 0xff09 => self.model.launcher.select(1),
                0xff52 => self.model.launcher.select(-5),
                0xff54 => self.model.launcher.select(5),
                0xff50 => self.model.launcher.selected = 0,
                0xff57 => {
                    self.model.launcher.selected =
                        self.model.launcher.results().len().saturating_sub(1)
                }
                0x61 | 0x41 if ctrl => self.model.launcher.selected_all = true,
                0x76 | 0x56 if ctrl => self.paste = true,
                _ if !ctrl => self.model.insert(&text),
                _ => {}
            }
            let row = super::launcher::ROW_HEIGHT;
            let top = (self.model.launcher.selected / 5) as f64 * row;
            let viewport = f64::from(draw::popup_height(&self.model)) - 100.0;
            if top < self.model.scroll {
                self.model.scroll = top;
            } else if top + row > self.model.scroll + viewport {
                self.model.scroll = top + row - viewport;
            }
            self.redraw(qh);
            return !ctrl
                && (matches!(sym, 0xff08 | 0xffff | 0xff51 | 0xff52 | 0xff53 | 0xff54)
                    || !text.is_empty());
        }
        if !self.model.entry || self.model.page != Page::Network {
            return false;
        }
        match sym {
            0xff0d | 0xff8d => self.model.submit(),
            0xff08 => self.model.backspace(),
            0xffff => self.model.delete(),
            0xff51 => self.model.move_cursor(-1),
            0xff53 => self.model.move_cursor(1),
            0xff50 => {
                self.model.cursor = 0;
                self.model.selected_all = false;
            }
            0xff57 => {
                self.model.cursor = self.model.secret.len();
                self.model.selected_all = false;
            }
            0x61 | 0x41 if ctrl => self.model.selected_all = true,
            0x76 | 0x56 if ctrl => self.paste = true,
            _ => {
                if !ctrl {
                    self.model.insert(&text);
                }
            }
        }
        self.redraw(qh);
        !ctrl
            && (matches!(sym, 0xff08 | 0xffff | 0xff51 | 0xff53)
                || text.chars().any(|c| !c.is_control()))
    }
    fn motion(&mut self, id: u32, x: f64, y: f64, qh: &QueueHandle<Self>) {
        if let Some(w) = self.windows.get(&id) {
            self.pointer_size = w.display_size;
        }
        if self.windows.get(&id).is_some_and(|w| {
            matches!(w.role, Role::Expanded(_, _)) && w.gpu.as_ref().is_some_and(|g| g.expanding())
        }) {
            self.pointer_window = Some(id);
            self.pointer_xy = (x, y);
            return;
        }
        let (x, y) = self
            .windows
            .get(&id)
            .filter(|w| w.gpu.is_some() && !matches!(w.role, Role::Expanded(_, _)))
            .map_or((x, y), |w| {
                (
                    x * f64::from(w.width) / f64::from(w.display_size.0.max(1)),
                    y * f64::from(w.height) / f64::from(w.display_size.1.max(1)),
                )
            });
        self.pointer_window = Some(id);
        self.model.pointer = Some((id, x, y));
        self.pointer_xy = (x, y);
        if self.model.scroll_drag.is_some() {
            self.model.drag_scroll(y);
            self.redraw(qh);
            return;
        }
        if let Some((_, target, r)) = self.model.dragging.clone() {
            self.model.slider(&target, (x - r.x) / r.w * 100.0, false);
            self.redraw(qh);
            return;
        }
        let next = self
            .model
            .hits
            .get(&id)
            .and_then(|h| {
                h.iter()
                    .enumerate()
                    .rev()
                    .find(|(_, h)| h.rect.contains(x, y))
            })
            .map(|(i, _)| (id, i));
        if self.model.hover != next {
            self.model.hover = next;
            if let Some(cursor) = &self.cursor {
                cursor.set_shape(
                    self.serial,
                    if next.is_some() {
                        wp_cursor_shape_device_v1::Shape::Pointer
                    } else {
                        wp_cursor_shape_device_v1::Shape::Default
                    },
                );
            }
            self.redraw(qh);
        }
    }
    fn button(&mut self, pressed: bool, button: u32, qh: &QueueHandle<Self>) {
        if !pressed {
            if self.model.scroll_drag.take().is_some() {
                self.redraw(qh);
            }
            if let Some((_, target, r)) = self.model.dragging.take() {
                self.model
                    .slider(&target, (self.pointer_xy.0 - r.x) / r.w * 100.0, true);
                self.redraw(qh);
            }
            return;
        }
        self.repeat = None;
        let Some(id) = self.pointer_window else {
            return;
        };
        if let Some(w) = self.windows.get(&id) {
            if let Role::Bar(side, _) | Role::Expanded(side, _) = &w.role {
                let change = f64::from(w.display_size.0 - self.pointer_size.0);
                self.pointer_xy.0 += if side == "right" {
                    change
                } else if side == "center" {
                    change / 2.0
                } else {
                    0.0
                };
                self.pointer_size = w.display_size;
            }
        }
        let Some(hit) = self
            .model
            .hits
            .get(&id)
            .and_then(|hits| {
                hits.iter()
                    .rev()
                    .find(|h| h.rect.contains(self.pointer_xy.0, self.pointer_xy.1))
            })
            .cloned()
        else {
            return;
        };
        if button == 0x111 {
            if let Action::TrayItem(id) = hit.action {
                self.model.tray_request = Some((id, true));
            }
            self.redraw(qh);
            return;
        }
        if button != 0x110 {
            return;
        }
        if id == 1 && self.loan_id.is_some() {
            if self
                .windows
                .get(&1)
                .is_some_and(|w| w.gpu.as_ref().is_some_and(|g| g.expanding()))
            {
                return;
            }
            if matches!(hit.action,Action::Page(p) if p == self.model.page) {
                self.hide();
                return;
            }
        }
        if matches!(hit.action, Action::Page(Page::Session)) {
            self.show_at("session", None, qh);
            return;
        }
        if self.bar && id != 1 {
            if let Action::Page(page) = hit.action {
                if self.model.visible && self.model.page == page && self.windows.contains_key(&1) {
                    self.hide();
                } else {
                    let output = self.windows.get(&id).and_then(|w| match &w.role {
                        Role::Bar(_, name) => Some(name.clone()),
                        _ => None,
                    });
                    self.show_at(page_name(page), output.as_deref(), qh);
                }
                return;
            }
        }
        if let Action::Scrollbar(bar) = hit.action {
            self.model.begin_scroll_drag(id, bar, self.pointer_xy.1);
        } else if let Action::Slider(target) = hit.action {
            self.model.dragging = Some((id, target.clone(), hit.rect));
            self.model.slider(
                &target,
                (self.pointer_xy.0 - hit.rect.x) / hit.rect.w * 100.0,
                true,
            );
        } else {
            self.model.activate(hit.action);
        }
        if !self.model.visible {
            self.hide();
            if self.bar {
                self.redraw(qh);
            }
        } else {
            self.redraw(qh);
        }
    }
    fn scroll(&mut self, value: f64, qh: &QueueHandle<Self>) {
        if !value.is_finite() || value == 0.0 {
            return;
        }
        if self.pointer_window == Some(1)
            && self.model.page == Page::Calendar
            && self.pointer_xy.0 < super::notifications::LIST_X
        {
            self.model.calendar_wheel(value);
            self.redraw(qh);
            return;
        }
        if let Some(id) = self.pointer_window {
            if let Some(hit) = self
                .model
                .hits
                .get(&id)
                .and_then(|h| {
                    h.iter()
                        .rev()
                        .find(|h| h.rect.contains(self.pointer_xy.0, self.pointer_xy.1))
                })
                .cloned()
            {
                if let Action::Slider(target) = hit.action {
                    let current = match &target {
                        Slider::Volume => self.model.common.audio.map_or(0.0, |a| a.0),
                        Slider::Brightness => self.model.common.brightness.unwrap_or(0.0),
                        Slider::Audio(k, i) => self.model.audio[super::model::collection(k)]
                            .as_array()
                            .and_then(|v| v.iter().find(|a| a["index"].as_u64() == Some(*i)))
                            .map_or(0.0, audio::volume),
                    };
                    self.model
                        .slider(&target, current - value.signum() * 2.0, true);
                    self.redraw(qh);
                    return;
                }
                if self.bar && id != 1 {
                    return;
                }
            }
        }
        self.model.scroll = (self.model.scroll + value * 3.0).clamp(
            0.0,
            (self.model.content_height - self.model.viewport_height).max(0.0),
        );
        self.redraw(qh);
    }
}
fn page_name(p: Page) -> &'static str {
    match p {
        Page::Home => "home",
        Page::Audio => "audio",
        Page::Network => "network",
        Page::Bluetooth => "bluetooth",
        Page::Power => "power",
        Page::Display => "display",
        Page::Session => "session",
        Page::TrayMenu => "tray",
        Page::Calendar => "calendar",
        Page::Launcher => "launcher",
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        r: &wl_registry::WlRegistry,
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
                "wl_compositor" => state.compositor = Some(r.bind(name, version.min(4), qh, ())),
                "wl_shm" => state.shm = Some(r.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => state.shell = Some(r.bind(name, version.min(4), qh, ())),
                "wp_viewporter" => state.viewporter = Some(r.bind(name, 1, qh, ())),
                "wp_fractional_scale_manager_v1" => {
                    state.fractional = Some(r.bind(name, 1, qh, ()))
                }
                "wp_cursor_shape_manager_v1" => {
                    state.cursor_manager = Some(r.bind(name, 1, qh, ()))
                }
                "zwp_text_input_manager_v3" => state.text_manager = Some(r.bind(name, 1, qh, ())),
                "wl_data_device_manager" => {
                    state.data_manager = Some(r.bind(name, version.min(3), qh, ()))
                }
                "wl_output" => {
                    state.outputs.insert(
                        name,
                        Output {
                            object: r.bind(name, version.min(4), qh, name),
                            name: String::new(),
                            scale: 1.0,
                        },
                    );
                }
                "wl_seat" => state.seat = Some(r.bind(name, version.min(7), qh, ())),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(o) = state.outputs.remove(&name) {
                    if state
                        .windows
                        .get(&1)
                        .is_some_and(|w| matches!(&w.role,Role::Expanded(_,n) if n == &o.name))
                    {
                        state.hide_immediate();
                    }
                    state
                        .windows
                        .retain(|_, w| !matches!(&w.role,Role::Bar(_,n)if n==&o.name));
                    if o.object.version() >= 3 {
                        o.object.release();
                    }
                }
            }
            _ => {}
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
                wl_output::Event::Scale { factor } => o.scale = f64::from(factor),
                wl_output::Event::Done if s.initialized && s.bar => s.bars(*id, qh),
                _ => {}
            }
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
        qh: &QueueHandle<Self>,
    ) {
        let resolved = s.logical_id(*id);
        let id = &resolved;
        if s.windows.get(id).is_none_or(|w| w.layer != *l) {
            return;
        }
        match e {
            layer::Event::Configure { serial, .. } => {
                l.ack_configure(serial);
                if let Some(w) = s.windows.get(id) {
                    if w.ready && w.gpu.is_some() {
                        w.surface.commit();
                        let _ = s.connection.flush();
                        return;
                    }
                }
                if let Some(w) = s.windows.get_mut(id) {
                    w.ready = true;
                }
                if let Err(e) = s.paint(*id, qh) {
                    eprintln!("configure: {e}");
                }
                let _ = s.connection.flush();
            }
            layer::Event::Closed => {
                s.windows.remove(id);
                if *id == 1 {
                    s.hide();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for Client {
    fn event(
        s: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        e: wp_fractional_scale_v1::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let resolved = s.logical_id(*id);
        let id = &resolved;
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = e {
            if let Some(w) = s.windows.get_mut(id) {
                w.scale = f64::from(scale) / 120.0;
            }
            let _ = s.paint(*id, qh);
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u32> for Client {
    fn event(
        s: &mut Self,
        callback: &wl_callback::WlCallback,
        _: wl_callback::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let resolved = s.logical_id(*id);
        let id = &resolved;
        let Some(window) = s.windows.get_mut(id) else {
            return;
        };
        if window.frame.as_ref() != Some(callback) {
            return;
        }
        window.frame = None;
        if let Some(gpu) = &window.gpu {
            if gpu.closing() && !gpu.animating() {
                if matches!(window.role, Role::Notice(_)) {
                    s.windows.remove(id);
                    s.model.hits.remove(id);
                } else {
                    s.hide_immediate();
                }
                let _ = s.connection.flush();
                return;
            }
            if (!window.dirty || gpu.closing()) && gpu.animating() {
                window.present(*id);
                let _ = s.connection.flush();
                s.wake();
                return;
            }
        }
        if window.dirty {
            let _ = s.paint(*id, qh);
            s.input_state();
        }
        let _ = s.connection.flush();
    }
}
impl Dispatch<wl_buffer::WlBuffer, u32> for Client {
    fn event(
        s: &mut Self,
        b: &wl_buffer::WlBuffer,
        e: wl_buffer::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let resolved = s.logical_id(*id);
        let id = &resolved;
        if let wl_buffer::Event::Release = e {
            if let Some(w) = s.windows.get_mut(id) {
                if let Some(buffer) = w.buffers.iter_mut().find(|x| x.object == *b) {
                    buffer.busy = false;
                }
                if w.dirty {
                    let _ = s.paint(*id, qh);
                    let _ = s.connection.flush();
                }
            }
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
            if c.contains(wl_seat::Capability::Pointer) && s.pointer.is_none() {
                s.pointer = Some(seat.get_pointer(qh, ()));
            }
            if c.contains(wl_seat::Capability::Keyboard) && s.keyboard_proxy.is_none() {
                s.keyboard_proxy = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_pointer::WlPointer,
        e: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match e {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                s.serial = serial;
                if let Some(id) = surface.data::<u32>() {
                    let id = s.logical_id(*id);
                    if s.windows.get(&id).is_some_and(|w| w.surface == surface) {
                        s.motion(id, surface_x, surface_y, qh);
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                s.pointer_window = None;
                s.model.pointer = None;
                s.model.hover = None;
                s.redraw(qh);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(id) = s.pointer_window {
                    s.motion(id, surface_x, surface_y, qh);
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(state),
                ..
            } => {
                s.serial = serial;
                s.button(state == wl_pointer::ButtonState::Pressed, button, qh);
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } => s.scroll(value, qh),
            _ => {}
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
        qh: &QueueHandle<Self>,
    ) {
        match e {
            wl_keyboard::Event::Leave { .. } => s.repeat = None,
            wl_keyboard::Event::Keymap {
                fd,
                size,
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
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
                s.repeat = if s.key(key, qh) && s.repeat_rate > 0 {
                    Some((
                        key,
                        std::time::Instant::now()
                            + std::time::Duration::from_millis(s.repeat_delay),
                    ))
                } else {
                    None
                };
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
            _ => {}
        }
    }
}
impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for Client {
    fn event(
        s: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        e: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match e {
            zwp_text_input_v3::Event::Enter { .. } => {
                s.input_enabled = false;
                s.input_rectangle = None;
                s.input_surrounding = None;
                s.input_state();
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                s.input_enabled = false;
                s.input_rectangle = None;
                s.input_surrounding = None;
            }
            zwp_text_input_v3::Event::CommitString { text: Some(t) } => {
                s.model.insert(&t);
                s.model.preedit.clear();
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                s.model.preedit = text.unwrap_or_default()
            }
            zwp_text_input_v3::Event::Done { .. } => s.redraw(qh),
            _ => {}
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_data_device::WlDataDevice,
        e: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Selection { id } = e {
            s.offer = id;
        }
    }
    wayland_client::event_created_child!(Client,wl_data_device::WlDataDevice,[0 => (wl_data_offer::WlDataOffer,())]);
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        e: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = e {
            if !s.offer_mimes.contains(&mime_type) {
                s.offer_mimes.push(mime_type);
            }
        }
    }
}
delegate_noop!(Client:ignore wl_compositor::WlCompositor);
delegate_noop!(Client:ignore wayland_client::protocol::wl_region::WlRegion);
impl Dispatch<wl_surface::WlSurface, u32> for Client {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Client:ignore wl_shm::WlShm);
delegate_noop!(Client:ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client:ignore shell::ZwlrLayerShellV1);
delegate_noop!(Client:ignore wp_viewporter::WpViewporter);
delegate_noop!(Client:ignore wp_viewport::WpViewport);
delegate_noop!(Client:ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(Client:ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(Client:ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(Client:ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(Client:ignore wl_data_device_manager::WlDataDeviceManager);

struct Runtime {
    queue: RefCell<EventQueue<Client>>,
    client: RefCell<Client>,
    qh: QueueHandle<Client>,
    tray: RefCell<Option<tray::Tray>>,
    pulse: RefCell<Option<glib::SourceId>>,
    notifications: RefCell<Option<super::notifications::Monitor>>,
    notification_server: RefCell<Option<super::notification_server::Server>>,
}
impl Runtime {
    #[allow(deprecated)]
    fn load_apps(self: &Rc<Self>) {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(self);
        rx.attach(
            None,
            move |(apps, done): (Vec<super::launcher::App>, bool)| {
                let Some(rt) = weak.upgrade() else {
                    return ControlFlow::Break;
                };
                let mut c = rt.client.borrow_mut();
                c.model.launcher.apps = apps;
                c.model.launcher.ready = true;
                let len = c.model.launcher.results().len();
                c.model.launcher.selected = c.model.launcher.selected.min(len.saturating_sub(1));
                c.redraw(&rt.qh);
                if done {
                    ControlFlow::Break
                } else {
                    ControlFlow::Continue
                }
            },
        );
        std::thread::spawn(move || {
            let mut apps = super::launcher::catalog();
            if tx.send((apps.clone(), false)).is_err() {
                return;
            }
            super::launcher::load_icons(&mut apps);
            let _ = tx.send((apps, true));
        });
    }
    fn animate(self: &Rc<Self>) {
        let needs = {
            let c = self.client.borrow();
            c.model.visible
                && ((c.model.busy && !c.model.auth_prompt)
                    || c.model.animating()
                    || c.repeat.is_some())
        };
        if !needs || self.pulse.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let mut last_spinner = std::time::Instant::now();
        let source = glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let Some(rt) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut c = rt.client.borrow_mut();
            if !c.model.visible {
                c.repeat = None;
            }
            let busy = c.model.visible && c.model.busy && !c.model.auth_prompt;
            let animating = c.model.visible && c.model.animating();
            if !busy && !animating && c.repeat.is_none() {
                if c.model.visible {
                    // Commit the exact final geometry after the interpolation deadline.
                    c.redraw(&rt.qh);
                }
                rt.pulse.borrow_mut().take();
                return ControlFlow::Break;
            }
            if let Some((key, deadline)) = c.repeat {
                if std::time::Instant::now() >= deadline {
                    let repeating = c.key(key, &rt.qh);
                    c.repeat = if repeating {
                        Some((
                            key,
                            std::time::Instant::now()
                                + std::time::Duration::from_millis(
                                    1000 / (c.repeat_rate.max(1) as u64),
                                ),
                        ))
                    } else {
                        None
                    };
                }
            }
            if animating || (busy && last_spinner.elapsed() >= std::time::Duration::from_millis(32))
            {
                c.redraw(&rt.qh);
                last_spinner = std::time::Instant::now();
            }
            ControlFlow::Continue
        });
        *self.pulse.borrow_mut() = Some(source);
    }
    #[allow(deprecated)]
    fn effects(self: &Rc<Self>) {
        let (request, event, menu, app, notice) = {
            let mut c = self.client.borrow_mut();
            (
                c.model.tray_request.take(),
                c.model.tray_event.take(),
                c.model.menu_item.clone(),
                c.model.app_request.take(),
                c.model.notification_request.take(),
            )
        };
        if let (Some(command), Some(monitor)) = (notice, self.notifications.borrow().as_ref()) {
            monitor.command(command);
        }
        if let Some(id) = app {
            let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
            let weak = Rc::downgrade(self);
            rx.attach(None, move |(id, result): (String, Result<(), String>)| {
                if let Some(rt) = weak.upgrade() {
                    let mut c = rt.client.borrow_mut();
                    match result {
                        Ok(()) => c.model.launcher.record(&id),
                        Err(error) => {
                            c.model.status = error;
                            c.show("launcher", &rt.qh);
                        }
                    }
                }
                ControlFlow::Break
            });
            std::thread::spawn(move || {
                let result = super::launcher::launch(&id);
                let _ = tx.send((id, result));
            });
        }
        if let Some(tray) = self.tray.borrow().as_ref() {
            if let Some((id, context)) = request {
                tray.activate(&id, context);
            }
            if let Some(id) = event {
                tray.clicked(&menu, id);
            }
        }
    }
    fn dispatch(&self) {
        let mut q = self.queue.borrow_mut();
        let mut c = self.client.borrow_mut();
        if let Some(read) = q.prepare_read() {
            // EGL can consume the readable fd while leaving our events queued.
            // Wake-channel dispatch must never wait for a new socket event.
            let mut fd = libc::pollfd {
                fd: c.connection.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 0) } > 0 {
                if let Err(e) = read.read() {
                    if !matches!(&e,wayland_client::backend::WaylandError::Io(io) if io.kind()==std::io::ErrorKind::WouldBlock)
                    {
                        eprintln!("Wayland read: {e}");
                        c.loop_.quit();
                        return;
                    }
                }
            }
        }
        if let Err(e) = q.dispatch_pending(&mut c) {
            eprintln!("Wayland dispatch: {e}");
            c.loop_.quit();
        }
        let _ = c.connection.flush();
    }
    fn changed(&self) {
        self.client.borrow_mut().redraw(&self.qh);
    }
    fn paste(self: &Rc<Self>) {
        let mut c = self.client.borrow_mut();
        if !std::mem::take(&mut c.paste) {
            return;
        }
        let Some(offer) = c.offer.clone() else {
            return;
        };
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return;
        }
        let input = unsafe { File::from_raw_fd(fds[0]) };
        let output = unsafe { File::from_raw_fd(fds[1]) };
        offer.receive("text/plain;charset=utf-8".into(), output.as_fd());
        drop(output);
        let _ = c.connection.flush();
        drop(c);
        let weak = Rc::downgrade(self);
        let mut input = input;
        let mut bytes = vec![];
        glib::source::unix_fd_add_local(
            input.as_raw_fd(),
            glib::IOCondition::IN | glib::IOCondition::HUP,
            move |_, _| {
                let mut buf = [0; 4096];
                loop {
                    match input.read(&mut buf) {
                        Ok(0) => {
                            if let Some(rt) = weak.upgrade() {
                                let mut c = rt.client.borrow_mut();
                                c.model.insert(&String::from_utf8_lossy(&bytes));
                                c.redraw(&rt.qh);
                            }
                            return ControlFlow::Break;
                        }
                        Ok(n) => {
                            if bytes.len() + n > 4096 {
                                return ControlFlow::Break;
                            }
                            bytes.extend_from_slice(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return ControlFlow::Continue
                        }
                        Err(_) => return ControlFlow::Break,
                    }
                }
            },
        );
    }
}

#[allow(deprecated)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().collect();
    let bar = args.iter().any(|a| a == "bar");
    let native_test = args.iter().any(|a| a == "--self-test-native");
    let preview = native_test || args.iter().any(|a| a == "--preview");
    let isolated = native_test || args.iter().any(|a| a == "--isolated");
    let page = args
        .iter()
        .position(|a| a == "--page")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or_else(|| {
            if args.iter().any(|a| a == "audio") {
                "audio"
            } else if args.iter().any(|a| a == "--vpn") {
                "vpn"
            } else if args.iter().any(|a| a == "--ethernet") {
                "ethernet"
            } else if args.iter().any(|a| a == "network") {
                "network"
            } else if args.iter().any(|a| a == "launcher") {
                "launcher"
            } else {
                "home"
            }
        });
    let runtime = PathBuf::from(env::var("XDG_RUNTIME_DIR")?);
    if !bar && !isolated {
        let request = UnixDatagram::unbound()?;
        let target = runtime.join("lucy-bar.sock");
        for attempt in 0..100 {
            match request.send_to(page.as_bytes(), &target) {
                Ok(_) => return Ok(()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    if attempt == 0 {
                        // During daemon startup the lock can precede the socket.
                        // Never fall back to another resident renderer/process.
                        std::process::Command::new(env::current_exe()?)
                            .arg("bar")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .spawn()?;
                    }
                    thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
        return Err("Lucy shell did not create its IPC socket within 2 seconds".into());
    }
    let launcher = page == "launcher";
    let name = if bar {
        "bar"
    } else if launcher {
        "launcher"
    } else {
        "control"
    };
    let suffix = if isolated { "-native-preview" } else { "" };
    let socket_path = runtime.join(format!("lucy-{name}{suffix}.sock"));
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(runtime.join(format!("lucy-{name}{suffix}.lock")))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if bar {
            return Ok(());
        }
        let socket = UnixDatagram::unbound()?;
        socket.send_to(page.as_bytes(), socket_path)?;
        return Ok(());
    }
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }
    let socket = UnixDatagram::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue::<Client>();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let loop_ = glib::MainLoop::new(None, false);
    let (clicked, clicks) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let (wakeup, wakes) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let mut client = Client::new(
        connection.clone(),
        bar,
        preview,
        clicked,
        wakeup,
        loop_.clone(),
    );
    client.model.readonly = preview;
    queue.roundtrip(&mut client)?;
    queue.roundtrip(&mut client)?;
    if client.compositor.is_none()
        || client.shm.is_none()
        || client.viewporter.is_none()
        || client.shell.is_none()
    {
        return Err("compositor must support layer-shell + viewporter".into());
    }
    if let (Some(manager), Some(pointer)) = (&client.cursor_manager, &client.pointer) {
        client.cursor = Some(manager.get_pointer(pointer, &qh, ()));
    }
    if let (Some(manager), Some(seat)) = (&client.text_manager, &client.seat) {
        client.text_input = Some(manager.get_text_input(seat, &qh, ()));
    }
    if let (Some(manager), Some(seat)) = (&client.data_manager, &client.seat) {
        client.data_device = Some(manager.get_data_device(seat, &qh, ()));
    }
    client.initialized = true;
    client.model.font = bar::output(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "font-name"],
    )
    .map(|s| s.trim_matches('\'').to_owned())
    .unwrap_or_else(|| "Sans 11".into());
    if launcher || bar {
        client.model.launcher.load_usage();
    }
    if bar {
        client.model.hide();
        client.model.bar_outputs = bar::output("niri", &["msg", "-j", "outputs"])
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for id in client.outputs.keys().copied().collect::<Vec<_>>() {
            client.bars(id, &qh);
        }
    } else {
        client.show(page, &qh);
    }
    let rt = Rc::new(Runtime {
        queue: RefCell::new(queue),
        client: RefCell::new(client),
        qh,
        tray: RefCell::new(None),
        pulse: RefCell::new(None),
        notifications: RefCell::new(None),
        notification_server: RefCell::new(None),
    });
    let weak = Rc::downgrade(&rt);
    clicks.attach(None, move |_| {
        if let Some(rt) = weak.upgrade() {
            rt.client.borrow_mut().hide();
        }
        ControlFlow::Continue
    });
    let weak = Rc::downgrade(&rt);
    let wayland_source = glib::source::unix_fd_add_local(
        connection.as_fd().as_raw_fd(),
        glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
        move |_, condition| {
            if let Some(rt) = weak.upgrade() {
                if condition.intersects(glib::IOCondition::HUP | glib::IOCondition::ERR) {
                    rt.client.borrow().loop_.quit();
                    return ControlFlow::Break;
                }
                rt.dispatch();
                rt.paste();
                rt.effects();
                rt.animate();
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        },
    );
    let weak = Rc::downgrade(&rt);
    let socket_source = glib::source::unix_fd_add_local(
        socket.as_raw_fd(),
        glib::IOCondition::IN,
        move |_, _| {
            let Some(rt) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut buffer = [0; 64];
            while let Ok((n, sender)) = socket.recv_from(&mut buffer) {
                if let Ok(page) = std::str::from_utf8(&buffer[..n]) {
                    if page == "@status" {
                        let c = rt.client.borrow();
                        let w = c.windows.get(&1);
                        let status = serde_json::json!({
                            "visible": c.model.visible,
                            "page": page_name(c.model.page),
                            "ready": w.is_some_and(|w| w.ready),
                            "presented": w.map_or(0, |w| w.presented),
                            "size": w.map(|w| w.display_size),
                            "closing": w.is_some_and(|w| w.gpu.as_ref().is_some_and(|g| g.closing())),
                            "surface": w.map(|w| w.surface.id().protocol_id()),
                            "borrowed_from": c.loan_id,
                            "expanding": w.is_some_and(|w| w.gpu.as_ref().is_some_and(|g| g.expanding())),
                            "pointer_window": c.pointer_window,
                            "pointer_xy": [c.pointer_xy.0, c.pointer_xy.1],
                            "calendar_offset": c.model.calendar_offset,
                            "tab": c.model.tab,
                            "bars": c.windows.values().filter_map(|w| match &w.role { Role::Bar(side, output) => Some(serde_json::json!({"side":side,"output":output,"surface":w.surface.id().protocol_id(),"size":[w.width,w.height]})), _=>None }).collect::<Vec<_>>(),
                        });
                        if let Some(path) = sender.as_pathname() {
                            let _ = socket.send_to(status.to_string().as_bytes(), path);
                        }
                    } else if page == "@hide" {
                        rt.client.borrow_mut().hide();
                    } else {
                        rt.client.borrow_mut().show(page, &rt.qh);
                    }
                }
            }
            ControlFlow::Continue
        },
    );
    let weak = Rc::downgrade(&rt);
    let wake_source = wakes.attach(None, move |_| {
        if let Some(rt) = weak.upgrade() {
            rt.client.borrow_mut().wakeup_pending = false;
            rt.dispatch();
            rt.effects();
            rt.animate();
            ControlFlow::Continue
        } else {
            ControlFlow::Break
        }
    });
    let mut sources = vec![wayland_source, socket_source, wake_source];
    if bar && !preview {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |notices| {
            if let Some(rt) = weak.upgrade() {
                rt.client.borrow_mut().popup_notifications(notices, &rt.qh);
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
        *rt.notification_server.borrow_mut() = Some(super::notification_server::Server::start(tx)?);
    }
    *rt.notifications.borrow_mut() = if !launcher {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |state| {
            let Some(rt) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut c = rt.client.borrow_mut();
            c.model.notifications = state;
            if (c.model.visible && c.model.page == Page::Calendar)
                || c.windows
                    .values()
                    .any(|w| matches!(w.role, Role::Notice(_)))
            {
                c.redraw(&rt.qh);
            }
            ControlFlow::Continue
        }));
        Some(super::notifications::Monitor::start(tx))
    } else {
        None
    };
    let app_monitor = if launcher || bar {
        rt.load_apps();
        let monitor = gio::AppInfoMonitor::get();
        let weak = Rc::downgrade(&rt);
        monitor.connect_changed(move |_| {
            if let Some(rt) = weak.upgrade() {
                rt.load_apps();
            }
        });
        Some(monitor)
    } else {
        None
    };
    let mut common_backend = None;
    let mut audio_backend = None;
    let mut net_backend = None;
    let mut bar_workers = None;
    let mut subscriptions = vec![];
    let test_passed = Rc::new(std::cell::Cell::new(!native_test));
    let mut file_monitors = vec![];
    let font_settings = gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup("org.gnome.desktop.interface", true))
        .map(|schema| {
            let settings = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None);
            let weak = Rc::downgrade(&rt);
            settings.connect_changed(Some("font-name"), move |s, _| {
                if let Some(rt) = weak.upgrade() {
                    rt.client.borrow_mut().model.font = s.string("font-name").to_string();
                    rt.changed();
                }
            });
            settings
        });
    if bar {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        *rt.tray.borrow_mut() = Some(tray::Tray::new(tx));
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |event| {
            let Some(rt) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            let mut c = rt.client.borrow_mut();
            match event {
                tray::Event::Items(items) => c.model.tray_items = items,
                tray::Event::Menu(id, menu) => {
                    c.model.menu_item = id;
                    c.model.menu = menu;
                    c.show("tray", &rt.qh);
                }
            }
            c.redraw(&rt.qh);
            ControlFlow::Continue
        }));
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        bar_workers = Some(bar::workers(tx));
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |event| {
            if let Some(rt) = weak.upgrade() {
                let mut c = rt.client.borrow_mut();
                match event {
                    bar::Update::Metrics(mut state) => {
                        if let Some(a) = c.model.metrics.values.get("audio") {
                            state.values.insert("audio", a.clone());
                        }
                        c.model.metrics = state;
                    }
                    bar::Update::Audio(a) => {
                        if let Some(a) = a {
                            c.model.metrics.values.insert("audio", a);
                        }
                    }
                    bar::Update::Workspaces(s) => c.model.workspaces = s,
                    bar::Update::Outputs(o) => {
                        c.model.bar_outputs = o;
                        for id in c.outputs.keys().copied().collect::<Vec<_>>() {
                            c.bars(id, &rt.qh);
                        }
                    }
                }
                c.redraw(&rt.qh);
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
        let weak = Rc::downgrade(&rt);
        let minute = std::cell::Cell::new(-1);
        sources.push(glib::timeout_add_seconds_local(1, move || {
            if let Some(rt) = weak.upgrade() {
                let now = glib::DateTime::now_local().map_or(-1, |n| n.minute());
                if minute.replace(now) != now {
                    rt.changed();
                }
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
    }
    if !launcher {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = control::Backend::start(tx);
        if !preview {
            rt.client.borrow_mut().model.common_tx = Some(backend.tx.clone());
        }
        let refresh = backend.tx.clone();
        common_backend = Some(backend);
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |event| {
            if let Some(rt) = weak.upgrade() {
                let mut c = rt.client.borrow_mut();
                match event {
                    control::Event::State(s) => c.model.common_state(s),
                    control::Event::Done(domain, success) => c.model.settle(domain, success),
                }
                c.redraw(&rt.qh);
                drop(c);
                request_cover(&rt);
                rt.animate();
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = audio::Backend::start(tx);
        if !preview {
            rt.client.borrow_mut().model.audio_tx = Some(backend.tx.clone());
        }
        audio_backend = Some(backend);
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |event| {
            if let Some(rt) = weak.upgrade() {
                let mut c = rt.client.borrow_mut();
                match event {
                    audio::Event::State(s) => c.model.audio_state(s),
                    audio::Event::Error(e) => c.model.status = e,
                }
                c.redraw(&rt.qh);
                drop(c);
                rt.animate();
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let backend = net::Backend::new(tx.clone());
        {
            let mut c = rt.client.borrow_mut();
            c.model.network_tx = Some(backend.tx.clone());
            if !preview {
                c.model.net_events = Some(tx);
            }
        }
        net_backend = Some(backend);
        let weak = Rc::downgrade(&rt);
        sources.push(rx.attach(None, move |event| {
            if let Some(rt) = weak.upgrade() {
                let mut c = rt.client.borrow_mut();
                c.model.network_event(event);
                c.redraw(&rt.qh);
                drop(c);
                rt.animate();
                ControlFlow::Continue
            } else {
                ControlFlow::Break
            }
        }));
        for bus_type in [gio::BusType::System, gio::BusType::Session] {
            if let Ok(bus) = gio::bus_get_sync(bus_type, gio::Cancellable::NONE) {
                let tx = refresh.clone();
                let id = bus.signal_subscribe(
                    None,
                    Some("org.freedesktop.DBus.Properties"),
                    Some("PropertiesChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, args| {
                        if let Some(interface) = args.child_value(0).get::<String>() {
                            let d = match interface.as_str() {
                                "org.bluez.Adapter1" | "org.bluez.Device1" => {
                                    control::Domain::Bluetooth
                                }
                                "org.freedesktop.UPower.PowerProfiles"
                                | "net.hadess.PowerProfiles" => control::Domain::Power,
                                "org.mpris.MediaPlayer2.Player" => control::Domain::Media,
                                "org.freedesktop.NetworkManager" => control::Domain::Wifi,
                                _ => return,
                            };
                            let _ = tx.send(control::Request::Refresh(d));
                        }
                    },
                );
                subscriptions.push((bus.clone(), id));
                let tx = refresh.clone();
                let id = bus.signal_subscribe(
                    Some("org.freedesktop.DBus"),
                    Some("org.freedesktop.DBus"),
                    Some("NameOwnerChanged"),
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, args| {
                        if let Some((name, _, _)) = args.get::<(String, String, String)>() {
                            let domain = if name.starts_with("org.mpris.MediaPlayer2.") {
                                control::Domain::Media
                            } else if name == "org.bluez" {
                                control::Domain::Bluetooth
                            } else if name.contains("PowerProfiles") {
                                control::Domain::Power
                            } else {
                                return;
                            };
                            let _ = tx.send(control::Request::Refresh(domain));
                        }
                    },
                );
                subscriptions.push((bus, id));
            }
        }
        if let Ok(devices) = fs::read_dir("/sys/class/backlight") {
            for device in devices.flatten() {
                for name in ["brightness", "actual_brightness"] {
                    let file = gio::File::for_path(device.path().join(name));
                    if let Ok(monitor) =
                        file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
                    {
                        let tx = refresh.clone();
                        monitor.connect_changed(move |_, _, _, _| {
                            let _ = tx.send(control::Request::Refresh(control::Domain::Brightness));
                        });
                        file_monitors.push(monitor);
                    }
                }
            }
        }
    }
    if native_test {
        let phase = std::cell::Cell::new(0);
        let ready_attempts = std::cell::Cell::new(0);
        let layer_id = std::cell::Cell::new(0u32);
        let weak = Rc::downgrade(&rt);
        let passed = test_passed.clone();
        sources.push(glib::timeout_add_local(std::time::Duration::from_millis(220),move||{
            let Some(rt)=weak.upgrade()else{return ControlFlow::Break;};let mut c=rt.client.borrow_mut();let i=phase.get();
            if i==0 {
                if c.keyboard.key(30).0==0 {ready_attempts.set(ready_attempts.get()+1);if ready_attempts.get()<20{return ControlFlow::Continue;}eprintln!("native UI test: compositor keyboard map was not received within 4 seconds");c.loop_.quit();return ControlFlow::Break;}
                if let Some(w)=c.windows.get(&1){layer_id.set(w.layer.id().protocol_id());}
            }
            if i<=4 {
                if c.windows.get(&1).is_none_or(|w|w.layer.id().protocol_id()!=layer_id.get()) {eprintln!("native UI test: navigation replaced the layer surface");c.loop_.quit();return ControlFlow::Break;}
                if c.windows.get(&1).is_none_or(|w|(w.width,w.height)!=(draw::WIDTH,draw::HEIGHT)) {eprintln!("native UI test: popup dimensions differ between pages");c.loop_.quit();return ControlFlow::Break;}
                c.model.navigate([Page::Home,Page::Audio,Page::Network,Page::Display,Page::Home][i as usize]);c.redraw(&rt.qh);
            } else if i==5 { c.model.navigate(Page::Network);c.model.entry=true;c.model.insert("native-draft-only");c.hide(); }
            else if i==6 {c.show("audio",&rt.qh);}
            else if i==7 {if c.model.secret!="native-draft-only"{eprintln!("native UI test: draft was lost");c.loop_.quit();return ControlFlow::Break;}c.hide();}
            else if i==8 {c.show("calendar",&rt.qh);}
            else if i==9 {
                if c.windows.get(&1).is_none_or(|w| !matches!(w.role,Role::Calendar) || (w.width,w.height)!=(super::notifications::WIDTH,super::notifications::HEIGHT)) {eprintln!("native UI test: calendar/notification surface dimensions are incorrect");c.loop_.quit();return ControlFlow::Break;}
                c.hide();
            }
            else if i==10 {c.show("home",&rt.qh);}
            else if i==11 {
                c.model.launcher.apps=super::launcher::catalog();c.model.launcher.ready=true;c.show("launcher",&rt.qh);
            }
            else if i==12 {
                if c.windows.get(&1).is_none_or(|w| !matches!(w.role,Role::Launcher) || (w.width,w.height)!=(super::launcher::WIDTH,super::launcher::HEIGHT)) {eprintln!("native UI test: launcher surface is missing");c.loop_.quit();return ControlFlow::Break;}
                if c.model.hits.get(&1).is_none_or(|hits| !hits.iter().any(|h|matches!(h.action,Action::Search)) || !hits.iter().any(|h|matches!(h.action,Action::App(_)))) {eprintln!("native UI test: launcher search or application grid is missing");c.loop_.quit();return ControlFlow::Break;}
                c.model.insert("lucy-test-한글");c.model.backspace();
                if c.model.launcher.query!="lucy-test-한" || c.model.secret!="native-draft-only" {eprintln!("native UI test: search editing overwrote network draft");c.loop_.quit();return ControlFlow::Break;}
                c.hide();
            }
            else if i==13 {
                c.show("launcher",&rt.qh);
                if !c.model.launcher.query.is_empty() || c.model.launcher.selected!=0 || c.model.scroll!=0.0 {eprintln!("native UI test: launcher did not reset on reopening");c.loop_.quit();return ControlFlow::Break;}
                c.model.readonly=false;
                c.key(28,&rt.qh);
                c.model.readonly=true;
                let requested=c.model.app_request.take().is_some();
                let closing=c.windows.get(&1).is_none_or(|w|w.gpu.as_ref().is_some_and(|g|g.closing()));
                if !requested || c.model.visible || !closing {eprintln!("native UI test: Enter did not close launcher surface");c.loop_.quit();return ControlFlow::Break;}
            }
            else if i==14 {c.show("home",&rt.qh);}
            else if i==15 {
                c.pointer_window=Some(1);c.pointer_xy=(444.0,32.0);c.button(true,0x110,&rt.qh);
            }
            else if i==16 {
                if c.windows.get(&1).is_none_or(|w| !matches!(w.role,Role::Session) || (w.width,w.height)!=(draw::SESSION_WIDTH,draw::SESSION_HEIGHT)) || c.backdrop.is_none() {eprintln!("native UI test: fullscreen session scrim or centered chooser is missing");c.loop_.quit();return ControlFlow::Break;}
                if c.model.hits.get(&1).is_none_or(|hits| hits.len()!=5 || !hits.iter().all(|h|matches!(h.action,Action::Session(_)) && h.rect.w==h.rect.h)) {eprintln!("native UI test: session chooser does not contain five square actions");c.loop_.quit();return ControlFlow::Break;}
                c.key(1,&rt.qh);
            }
            else if i==17 {
                if c.model.visible || c.windows.contains_key(&1) || c.backdrop.is_some() {eprintln!("native UI test: Escape did not remove the session chooser and scrim");c.loop_.quit();return ControlFlow::Break;}
                c.show("home",&rt.qh);
            }
            else {passed.set(true);println!("PASS: control pages retain one 480x500 surface; separate calendar, launcher and session chooser; five square session buttons and Escape dismissal; launcher grid and UTF-8 search; network draft preserved; GTK not initialized. No app launches or session actions.");c.loop_.quit();return ControlFlow::Break;}
            phase.set(i+1);ControlFlow::Continue
        }));
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let loop_ = loop_.clone();
        sources.push(glib::unix_signal_add_local(signal, move || {
            loop_.quit();
            ControlFlow::Break
        }));
    }
    connection.flush()?;
    loop_.run();
    for source in sources {
        if let Some(source) = glib::MainContext::default().find_source_by_id(&source) {
            source.destroy();
        }
    }
    for (bus, id) in subscriptions {
        bus.signal_unsubscribe(id);
    }
    for monitor in file_monitors {
        monitor.cancel();
    }
    drop(font_settings);
    drop(app_monitor);
    rt.notifications.borrow_mut().take();
    rt.notification_server.borrow_mut().take();
    rt.client.borrow_mut().hide_immediate();
    drop(rt);
    drop(net_backend);
    drop(audio_backend);
    drop(common_backend);
    drop(bar_workers);
    let _ = fs::remove_file(socket_path);
    drop(lock);
    if native_test && !test_passed.get() {
        return Err("native UI verification failed".into());
    }
    Ok(())
}

#[allow(deprecated)]
fn request_cover(rt: &Rc<Runtime>) {
    request_media_app(rt);
    let mut c = rt.client.borrow_mut();
    let url = c
        .model
        .common
        .media
        .as_ref()
        .map(|m| m.art_url.clone())
        .unwrap_or_default();
    if c.model.cover_url == url {
        return;
    }
    c.model.cover_url = url.clone();
    c.model.cover = None;
    if url.is_empty() {
        return;
    }
    drop(c);
    let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let weak = Rc::downgrade(rt);
    rx.attach(None, move |data: Option<(String, Vec<u8>, i32, i32)>| {
        if let (Some(rt), Some((url, bytes, w, h))) = (weak.upgrade(), data) {
            let mut c = rt.client.borrow_mut();
            if c.model.cover_url == url {
                c.model.cover =
                    cairo::ImageSurface::create_for_data(bytes, cairo::Format::ARgb32, w, h, w * 4)
                        .ok();
                c.redraw(&rt.qh);
            }
        }
        ControlFlow::Break
    });
    thread::spawn(move || {
        let data = (|| {
            let bytes = if url.starts_with("file://") {
                let (path, _) = glib::filename_from_uri(&url).ok()?;
                let file = File::open(path).ok()?;
                if file.metadata().ok()?.len() > 2097152 {
                    return None;
                }
                let mut b = vec![];
                file.take(2097153).read_to_end(&mut b).ok()?;
                b
            } else if url.starts_with("http://") || url.starts_with("https://") {
                let out = std::process::Command::new("curl")
                    .args([
                        "--silent",
                        "--fail",
                        "--location",
                        "--max-redirs",
                        "3",
                        "--max-time",
                        "3",
                        "--max-filesize",
                        "2097152",
                        "--proto",
                        "=http,https",
                        "--proto-redir",
                        "=http,https",
                        "--",
                        &url,
                    ])
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                out.stdout
            } else {
                return None;
            };
            if bytes.len() > 2097152 {
                return None;
            }
            let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
            let image = gdk_pixbuf::Pixbuf::from_stream_at_scale(
                &stream,
                72,
                72,
                true,
                gio::Cancellable::NONE,
            )
            .ok()?;
            let (w, h) = (image.width(), image.height());
            let mut pixels = vec![0; (w * h * 4) as usize];
            let input = image.read_pixel_bytes();
            let rgba = input.as_ref();
            let stride = image.rowstride() as usize;
            let channels = image.n_channels() as usize;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * stride + x * channels;
                    let o = (y * w as usize + x) * 4;
                    let a = if channels == 4 { rgba[i + 3] } else { 255 };
                    pixels[o] = (u16::from(rgba[i + 2]) * u16::from(a) / 255) as u8;
                    pixels[o + 1] = (u16::from(rgba[i + 1]) * u16::from(a) / 255) as u8;
                    pixels[o + 2] = (u16::from(rgba[i]) * u16::from(a) / 255) as u8;
                    pixels[o + 3] = a;
                }
            }
            Some((url, pixels, w, h))
        })();
        let _ = tx.send(data);
    });
}

#[allow(deprecated)]
fn request_media_app(rt: &Rc<Runtime>) {
    let mut c = rt.client.borrow_mut();
    let player = c
        .model
        .common
        .media
        .as_ref()
        .map(|m| m.player.clone())
        .unwrap_or_default();
    if c.model.media_player == player {
        return;
    }
    c.model.media_player = player.clone();
    c.model.media_app_name = player.clone();
    c.model.media_app_icon = None;
    if player.is_empty() {
        return;
    }
    drop(c);
    let (tx, rx) =
        glib::MainContext::channel::<(String, String, Option<tray::Icon>)>(glib::Priority::DEFAULT);
    let weak = Rc::downgrade(rt);
    rx.attach(None, move |(player, name, icon)| {
        if let Some(rt) = weak.upgrade() {
            let mut c = rt.client.borrow_mut();
            if c.model.media_player == player {
                c.model.media_app_name = name;
                c.model.media_app_icon = icon;
                c.redraw(&rt.qh);
            }
        }
        ControlFlow::Break
    });
    thread::spawn(move || {
        use glib::variant::ToVariant;
        let props = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .ok()
            .and_then(|bus| {
                bus.call_sync(
                    Some(&format!("org.mpris.MediaPlayer2.{player}")),
                    "/org/mpris/MediaPlayer2",
                    "org.freedesktop.DBus.Properties",
                    "GetAll",
                    Some(&("org.mpris.MediaPlayer2",).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    1000,
                    gio::Cancellable::NONE,
                )
                .ok()
            })
            .and_then(|v| v.child_value(0).get::<HashMap<String, glib::Variant>>())
            .unwrap_or_default();
        let name = props
            .get("Identity")
            .and_then(|v| v.get::<String>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| player.clone());
        let desktop = props
            .get("DesktopEntry")
            .and_then(|v| v.get::<String>())
            .unwrap_or_else(|| player.split('.').next().unwrap_or(&player).into());
        let catalog = super::launcher::catalog();
        let icon_name = catalog
            .iter()
            .find(|a| {
                a.id.trim_end_matches(".desktop")
                    .eq_ignore_ascii_case(&desktop)
                    || a.name.eq_ignore_ascii_case(&name)
            })
            .map_or(desktop, |a| a.icon_name.clone());
        let mut apps = [super::launcher::App {
            id: player.clone(),
            name: name.clone(),
            search: String::new(),
            icon_name,
            icon: None,
        }];
        super::launcher::load_icons(&mut apps);
        let _ = tx.send((player, name, apps[0].icon.take()));
    });
}
