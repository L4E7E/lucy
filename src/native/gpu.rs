//! EGL/glow compositor. Cairo scenes are cached; transition frames stay on GPU.
use glow::HasContext;
use std::{
    cell::{Cell, RefCell},
    ffi::{c_char, c_void, CString},
    hash::Hasher,
    ptr,
    rc::Rc,
    time::{Duration, Instant},
};
type Handle = *mut c_void;
#[link(name = "EGL")]
extern "C" {
    fn eglGetPlatformDisplay(p: u32, n: Handle, a: *const isize) -> Handle;
    fn eglInitialize(d: Handle, a: *mut i32, b: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(d: Handle, a: *const i32, c: *mut Handle, size: i32, n: *mut i32) -> u32;
    fn eglCreateContext(d: Handle, c: Handle, share: Handle, a: *const i32) -> Handle;
    fn eglCreateWindowSurface(d: Handle, c: Handle, w: Handle, a: *const i32) -> Handle;
    fn eglMakeCurrent(d: Handle, draw: Handle, read: Handle, c: Handle) -> u32;
    fn eglSwapInterval(d: Handle, n: i32) -> u32;
    fn eglSwapBuffers(d: Handle, s: Handle) -> u32;
    fn eglDestroySurface(d: Handle, s: Handle) -> u32;
    fn eglDestroyContext(d: Handle, c: Handle) -> u32;
    fn eglTerminate(d: Handle) -> u32;
    fn eglGetProcAddress(name: *const c_char) -> Option<unsafe extern "C" fn()>;
}
#[link(name = "wayland-egl")]
extern "C" {
    fn wl_egl_window_create(s: Handle, w: i32, h: i32) -> Handle;
    fn wl_egl_window_resize(s: Handle, w: i32, h: i32, x: i32, y: i32);
    fn wl_egl_window_destroy(w: Handle);
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Effect {
    Immediate,
    Open,
    Change,
    Slide,
    Close,
}
pub fn spring(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t >= 1.0 {
        1.0
    } else {
        1.0 - (-10.0 * t).exp() * ((10.0 * t).cos() + (10.0 * t).sin())
    }
}
pub fn geometry(effect: Effect, t: f32, notice: bool) -> (f32, f32) {
    if !matches!(effect, Effect::Open | Effect::Close) {
        return (1.0, 0.0);
    }
    let p = if effect == Effect::Close {
        1.0 - t.clamp(0.0, 1.0).powi(2)
    } else {
        spring(t)
    };
    if notice {
        (0.98 + 0.02 * p, p - 1.0)
    } else {
        // A 1px rounded surface may be entirely clipped by the compositor;
        // then it need not deliver the frame callback that advances the opening.
        ((0.08 + 0.92 * p).max(0.08), -8.0 * (1.0 - p))
    }
}
struct Program {
    native: glow::NativeProgram,
    vertices: glow::NativeBuffer,
    now: Option<glow::NativeUniformLocation>,
    old: Option<glow::NativeUniformLocation>,
    ratios: Option<glow::NativeUniformLocation>,
    progress: Option<glow::NativeUniformLocation>,
    displacement: Option<glow::NativeUniformLocation>,
    background: Option<glow::NativeUniformLocation>,
    blending: Option<glow::NativeUniformLocation>,
    joined: Option<glow::NativeUniformLocation>,
    extent: Option<glow::NativeUniformLocation>,
    header_rect: Option<glow::NativeUniformLocation>,
    header_image: Option<glow::NativeUniformLocation>,
}
pub struct Gpu {
    display: Handle,
    config: Handle,
    context: Handle,
    gl: RefCell<Option<glow::Context>>,
    program: RefCell<Option<Program>>,
    _connection: wayland_client::Connection,
}
impl Gpu {
    pub fn new(connection: &wayland_client::Connection) -> Option<Rc<Self>> {
        if std::env::var_os("LUCY_SOFTWARE").is_some() {
            return None;
        }
        // SAFETY: the retained connection owns wl_display for the entire EGL lifetime.
        unsafe {
            let display = eglGetPlatformDisplay(
                0x31d8,
                connection.backend().display_ptr().cast(),
                ptr::null(),
            );
            if display.is_null() || eglInitialize(display, ptr::null_mut(), ptr::null_mut()) == 0 {
                return None;
            }
            eglBindAPI(0x30a0);
            let attrs = [
                0x3033, 4, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3038,
            ];
            let mut config = ptr::null_mut();
            let mut n = 0;
            if eglChooseConfig(display, attrs.as_ptr(), &mut config, 1, &mut n) == 0 || n == 0 {
                eglTerminate(display);
                return None;
            }
            let context = eglCreateContext(
                display,
                config,
                ptr::null_mut(),
                [0x3098, 2, 0x3038].as_ptr(),
            );
            if context.is_null() {
                eglTerminate(display);
                return None;
            }
            Some(Rc::new(Self {
                display,
                config,
                context,
                gl: RefCell::new(None),
                program: RefCell::new(None),
                _connection: connection.clone(),
            }))
        }
    }
    unsafe fn initialize_gl(&self) -> Result<(), String> {
        if self.program.borrow().is_some() {
            return Ok(());
        }
        let gl = glow::Context::from_loader_function(|name| {
            let name = CString::new(name).unwrap();
            eglGetProcAddress(name.as_ptr())
                .map_or(ptr::null(), |f| f as *const () as *const c_void)
        });
        let renderer = gl.get_parameter_string(glow::RENDERER);
        if renderer.to_lowercase().contains("llvmpipe")
            || renderer.to_lowercase().contains("softpipe")
        {
            return Err("Software GL; choosing SHM fallback".into());
        }
        let vert="attribute vec2 position; varying vec2 uv; void main(){uv=vec2((position.x+1.0)*0.5,(1.0-position.y)*0.5);gl_Position=vec4(position,0.0,1.0);}";
        // Keep a stationary material under independently animated foreground content.
        // Niri owns blur; full-window motion is applied to wl_surface geometry, not this quad.
        let frag = r#"precision mediump float;
varying vec2 uv;
uniform sampler2D currentImage;
uniform sampler2D previousImage;
uniform vec4 ratios;
uniform float progress;
uniform float displacement;
uniform float backgroundAlpha;
uniform float blending;
uniform float joined;
uniform vec4 extent;
uniform vec4 headerRect;
uniform sampler2D headerImage;
vec4 sampleScene(sampler2D image,vec2 q,vec2 ratio){
    q=(q-vec2(1.0-ratio.x,0.0))/ratio;
    if(q.x<0.0||q.x>1.0||q.y<0.0||q.y>1.0)return vec4(0.0);
    return texture2D(image,q).bgra;
}
vec4 foreground(vec4 pixel){
    if(pixel.a<backgroundAlpha+0.001)return vec4(0.0);
    float a=clamp((pixel.a-backgroundAlpha)/(1.0-backgroundAlpha),0.0,1.0);
    vec3 b=vec3(25.0,27.0,34.0)/255.0*backgroundAlpha;
    return vec4(max(vec3(0.0),pixel.rgb-b*(1.0-a)),a);
}
void main(){
    if(joined>=0.0){
        vec2 pos=uv*extent.xy;
        vec2 d=abs(pos-extent.xy*0.5)-(extent.xy*0.5-vec2(14.0));
        float edge=length(max(d,vec2(0.0)))+min(max(d.x,d.y),0.0)-14.0;
        float mask=1.0-smoothstep(-0.7,0.3,edge);
        float light=mix(0.115,0.05,clamp(uv.y*1.5,0.0,1.0));
        vec4 base=vec4(vec3(25.0,27.0,34.0)/255.0*0.54,0.54);
        base=vec4(vec3(light),light)+base*(1.0-light);
        float rim=(1.0-smoothstep(0.4,1.3,abs(edge+0.9)))*mix(0.40,0.16,uv.y);
        base=vec4(vec3(rim),rim)+base*(1.0-rim);
        vec2 q=(pos-vec2((extent.x-extent.z)*0.5,10.0*(1.0-joined)))/extent.zw;
        vec4 body=sampleScene(currentImage,q,ratios.xy);
        if(blending>0.5){
            vec4 a=sampleScene(previousImage,q+vec2(displacement*progress,0.0),ratios.zw);
            vec4 b=sampleScene(currentImage,q-vec2(displacement*(1.0-progress),0.0),ratios.xy);
            body=mix(a,b,progress);
        }
        body*=smoothstep(0.08,0.65,joined);
        vec4 result=body+base*(1.0-body.a);
        vec2 h=(pos-headerRect.xy)/headerRect.zw;
        if(h.x>=0.0&&h.x<=1.0&&h.y>=0.0&&h.y<=1.0){
            vec4 ink=texture2D(headerImage,h).bgra;
            result=ink+result*(1.0-ink.a);
        }
        gl_FragColor=result*mask;return;
    }
    vec4 plain=sampleScene(currentImage,uv,ratios.xy);
    if(blending<0.5){gl_FragColor=plain;return;}
    float t=clamp(progress,0.0,1.0);
    vec4 a=foreground(sampleScene(previousImage,uv+vec2(displacement*t,0.0),ratios.zw));
    vec4 b=foreground(sampleScene(currentImage,uv-vec2(displacement*(1.0-t),0.0),ratios.xy));
    vec4 content=mix(a,b,t);
    float material=min(plain.a,backgroundAlpha);
    vec4 base=vec4(vec3(25.0,27.0,34.0)/255.0*material,material);
    gl_FragColor=content+base*(1.0-content.a);
}"#;
        let native = gl.create_program()?;
        for (kind, source) in [(glow::VERTEX_SHADER, vert), (glow::FRAGMENT_SHADER, frag)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let error = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                gl.delete_program(native);
                return Err(error);
            }
            gl.attach_shader(native, shader);
            gl.delete_shader(shader);
        }
        gl.bind_attrib_location(native, 0, "position");
        gl.link_program(native);
        if !gl.get_program_link_status(native) {
            let error = gl.get_program_info_log(native);
            gl.delete_program(native);
            return Err(error);
        }
        let vertices = gl.create_buffer()?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
        let points: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes =
            std::slice::from_raw_parts(points.as_ptr().cast(), std::mem::size_of_val(&points));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
        let program = Program {
            native,
            vertices,
            now: gl.get_uniform_location(native, "currentImage"),
            old: gl.get_uniform_location(native, "previousImage"),
            ratios: gl.get_uniform_location(native, "ratios"),
            progress: gl.get_uniform_location(native, "progress"),
            displacement: gl.get_uniform_location(native, "displacement"),
            background: gl.get_uniform_location(native, "backgroundAlpha"),
            blending: gl.get_uniform_location(native, "blending"),
            joined: gl.get_uniform_location(native, "joined"),
            extent: gl.get_uniform_location(native, "extent"),
            header_rect: gl.get_uniform_location(native, "headerRect"),
            header_image: gl.get_uniform_location(native, "headerImage"),
        };
        *self.program.borrow_mut() = Some(program);
        *self.gl.borrow_mut() = Some(gl);
        eprintln!("Lucy EGL + glow renderer: {renderer}");
        Ok(())
    }
}
impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            eglDestroyContext(self.display, self.context);
            eglTerminate(self.display);
        }
    }
}
struct Header {
    texture: glow::NativeTexture,
    fingerprint: u64,
    width: f32,
}
pub struct Window {
    gpu: Rc<Gpu>,
    native: Handle,
    surface: Handle,
    textures: [glow::NativeTexture; 2],
    sizes: [(i32, i32); 2],
    front: usize,
    w: i32,
    h: i32,
    fingerprint: Option<u64>,
    effect: Effect,
    start: Instant,
    duration: Duration,
    last: Cell<f32>,
    active: bool,
    background: f32,
    expansion: Option<super::expansion::Expansion>,
    header: Option<Header>,
}
impl Window {
    /// The caller keeps wl_surface alive until this wrapper is dropped.
    pub unsafe fn new(gpu: Rc<Gpu>, surface: Handle, w: i32, h: i32) -> Option<Self> {
        let native = wl_egl_window_create(surface, w, h);
        if native.is_null() {
            return None;
        }
        let surface = eglCreateWindowSurface(gpu.display, gpu.config, native, ptr::null());
        if surface.is_null() {
            wl_egl_window_destroy(native);
            return None;
        }
        if eglMakeCurrent(gpu.display, surface, surface, gpu.context) == 0 {
            eglDestroySurface(gpu.display, surface);
            wl_egl_window_destroy(native);
            return None;
        }
        if let Err(e) = gpu.initialize_gl() {
            eprintln!("Lucy GL initialization: {e}");
            eglDestroySurface(gpu.display, surface);
            wl_egl_window_destroy(native);
            return None;
        }
        eglSwapInterval(gpu.display, 0);
        let textures = {
            let gl = gpu.gl.borrow();
            let gl = gl.as_ref()?;
            let textures = [gl.create_texture().ok()?, gl.create_texture().ok()?];
            for texture in textures {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                for (name, value) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
                }
            }
            textures
        };
        Some(Self {
            gpu,
            native,
            surface,
            textures,
            sizes: [(w, h); 2],
            front: 0,
            w,
            h,
            fingerprint: None,
            effect: Effect::Open,
            start: Instant::now(),
            duration: Duration::from_millis(300),
            last: Cell::new(1.0),
            active: false,
            background: 0.8,
            expansion: None,
            header: None,
        })
    }
    fn current(&self) -> bool {
        unsafe {
            eglMakeCurrent(
                self.gpu.display,
                self.surface,
                self.surface,
                self.gpu.context,
            ) != 0
        }
    }
    pub fn set_background(&mut self, alpha: f32) {
        self.background = alpha;
    }
    pub fn begin_expansion(&mut self, source: i32, target: (i32, i32), calendar: bool) {
        if self.expansion.is_none() {
            self.expansion = Some(super::expansion::Expansion::new(source, target, calendar));
        }
        if let Some(e) = &mut self.expansion {
            e.target = target;
        }
    }
    pub fn expansion_frame(&self) -> Option<super::expansion::Frame> {
        self.expansion
            .as_ref()
            .map(|e| e.frame(self.header.as_ref().map_or(e.source, |h| h.width)))
    }
    pub fn expanding(&self) -> bool {
        self.expansion.as_ref().is_some_and(|e| e.animating())
    }
    pub fn reopen_expansion(&mut self) {
        let width = self.header.as_ref().map_or(0.0, |h| h.width);
        if let Some(e) = &mut self.expansion {
            e.reopen(width);
        }
    }
    pub fn finish_expansion(&mut self) {
        self.expansion = None;
        if let Some(h) = self.header.take() {
            if self.current() {
                if let Some(gl) = self.gpu.gl.borrow().as_ref() {
                    unsafe {
                        gl.delete_texture(h.texture);
                    }
                }
            }
        }
        self.fingerprint = None;
        self.active = false;
        self.effect = Effect::Immediate;
    }
    pub fn set_header(&mut self, bytes: &[u8], w: i32, h: i32, logical_width: f32) -> bool {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        hash.write(bytes);
        hash.write_i32(w);
        hash.write_i32(h);
        let fingerprint = hash.finish();
        if self
            .header
            .as_ref()
            .is_some_and(|h| h.fingerprint == fingerprint)
            || !self.current()
        {
            return false;
        }
        let gl = self.gpu.gl.borrow();
        let gl = gl.as_ref().unwrap();
        unsafe {
            let texture = match &self.header {
                Some(h) => h.texture,
                None => match gl.create_texture() {
                    Ok(t) => t,
                    Err(_) => return false,
                },
            };
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for (name, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
            }
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                w,
                h,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(bytes)),
            );
            self.header = Some(Header {
                texture,
                fingerprint,
                width: logical_width,
            });
        }
        true
    }
    pub fn update(&mut self, bytes: &[u8], w: i32, h: i32, effect: Effect) -> bool {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        hash.write(bytes);
        hash.write_i32(w);
        hash.write_i32(h);
        let hash = hash.finish();
        if self.fingerprint == Some(hash) || !self.current() {
            return false;
        }
        let first = self.fingerprint.is_none();
        if (w, h) != (self.w, self.h) {
            unsafe {
                wl_egl_window_resize(self.native, w, h, 0, 0);
            }
            self.w = w;
            self.h = h;
        }
        let reset =
            !self.animating() || matches!(effect, Effect::Slide | Effect::Immediate) || first;
        if reset {
            self.front = 1 - self.front;
            self.effect = if first && effect != Effect::Immediate {
                Effect::Open
            } else {
                effect
            };
            self.start = Instant::now();
            self.duration =
                Duration::from_millis(if matches!(self.effect, Effect::Open | Effect::Slide) {
                    300
                } else {
                    160
                });
            self.last.set(0.0);
            self.active = self.effect != Effect::Immediate;
        }
        let gl = self.gpu.gl.borrow();
        let gl = gl.as_ref().unwrap();
        for index in if first { vec![0, 1] } else { vec![self.front] } {
            unsafe {
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[index]));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    w,
                    h,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(bytes)),
                );
            }
            self.sizes[index] = (w, h);
        }
        self.fingerprint = Some(hash);
        true
    }
    fn t(&self) -> f32 {
        if !self.active {
            return 1.0;
        }
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }
    pub fn animating(&self) -> bool {
        self.expanding() || (self.active && (self.t() < 1.0 || self.last.get() < 1.0))
    }
    pub fn closing(&self) -> bool {
        self.expansion
            .as_ref()
            .map_or(self.effect == Effect::Close, |e| e.closing())
    }
    pub fn close(&mut self) {
        let header = self.header.as_ref().map_or(0.0, |h| h.width);
        if let Some(e) = &mut self.expansion {
            e.close(header);
            self.active = false;
            return;
        }
        self.effect = Effect::Close;
        self.start = Instant::now();
        self.duration = Duration::from_millis(190);
        self.last.set(0.0);
        self.active = true;
    }
    pub fn geometry(&self, notice: bool) -> (f32, f32) {
        geometry(self.effect, self.t(), notice)
    }
    pub fn present(&self, expansion: Option<super::expansion::Frame>) -> bool {
        if !self.current() {
            return false;
        }
        let t = self.t();
        let gl = self.gpu.gl.borrow();
        let gl = gl.as_ref().unwrap();
        let program = self.gpu.program.borrow();
        let p = program.as_ref().unwrap();
        unsafe {
            gl.viewport(0, 0, self.w, self.h);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.disable(glow::BLEND);
            gl.use_program(Some(p.native));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(p.vertices));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            for (unit, index) in [(0, self.front), (1, 1 - self.front)] {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[index]));
            }
            gl.uniform_1_i32(p.now.as_ref(), 0);
            gl.uniform_1_i32(p.old.as_ref(), 1);
            gl.uniform_1_f32(p.joined.as_ref(), expansion.map_or(-1.0, |f| f.amount));
            if let (Some(f), Some(header)) = (expansion, &self.header) {
                gl.active_texture(glow::TEXTURE2);
                gl.bind_texture(glow::TEXTURE_2D, Some(header.texture));
                gl.uniform_1_i32(p.header_image.as_ref(), 2);
                let target = self.expansion.as_ref().unwrap().target;
                gl.uniform_4_f32(
                    p.extent.as_ref(),
                    f.size.0 as f32,
                    f.size.1 as f32,
                    target.0 as f32,
                    target.1 as f32,
                );
                gl.uniform_4_f32(
                    p.header_rect.as_ref(),
                    f.header.0,
                    f.header.1,
                    header.width * f.header_scale,
                    34.0 * f.header_scale,
                );
            }
            let a = self.sizes[self.front];
            let b = self.sizes[1 - self.front];
            gl.uniform_4_f32(
                p.ratios.as_ref(),
                a.0 as f32 / self.w as f32,
                a.1 as f32 / self.h as f32,
                b.0 as f32 / self.w as f32,
                b.1 as f32 / self.h as f32,
            );
            let progress = if self.effect == Effect::Slide {
                spring(t).clamp(0.0, 1.0)
            } else {
                1.0 - (1.0 - t).powi(3)
            };
            gl.uniform_1_f32(p.progress.as_ref(), progress);
            gl.uniform_1_f32(
                p.displacement.as_ref(),
                if self.effect == Effect::Slide {
                    0.09
                } else {
                    0.0
                },
            );
            gl.uniform_1_f32(p.background.as_ref(), self.background);
            gl.uniform_1_f32(
                p.blending.as_ref(),
                if matches!(self.effect, Effect::Change | Effect::Slide) && t < 1.0 {
                    1.0
                } else {
                    0.0
                },
            );
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            let ok = eglSwapBuffers(self.gpu.display, self.surface) != 0;
            self.last.set(t);
            if let (Some(e), Some(f)) = (&self.expansion, expansion) {
                e.presented(f.time);
            }
            ok
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            self.current();
            if let Some(gl) = self.gpu.gl.borrow().as_ref() {
                for t in self.textures {
                    gl.delete_texture(t);
                }
                if let Some(h) = &self.header {
                    gl.delete_texture(h.texture);
                }
            }
            eglDestroySurface(self.gpu.display, self.surface);
            wl_egl_window_destroy(self.native);
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn spring_settles_and_has_only_small_overshoot() {
        assert_eq!(super::spring(0.0), 0.0);
        assert_eq!(super::spring(1.0), 1.0);
        let max = (0..100)
            .map(|i| super::spring(i as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(max > 1.0 && max < 1.05);
    }
    #[test]
    fn geometry_moves_the_whole_surface_not_just_its_pixels() {
        use super::*;
        let (start, _) = geometry(Effect::Open, 0.0, false);
        let (end, y) = geometry(Effect::Open, 1.0, false);
        assert_eq!(start, 0.08);
        assert_eq!((end, y), (1.0, 0.0));
        assert_eq!(geometry(Effect::Close, 1.0, false).0, 0.08);
        // Even the smallest panel stays wider than two 11.25px corner radii.
        assert!(start * 340.0 > 22.5);
        assert_eq!(geometry(Effect::Slide, 0.5, false), (1.0, 0.0));
        assert_eq!(geometry(Effect::Open, 0.0, true).1, -1.0);
    }
}
