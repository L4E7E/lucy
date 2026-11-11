//! EGL/glow compositor. Cairo scenes are cached; transition frames stay on GPU.
use glow::HasContext;
#[path = "gpu_gauges.rs"]
mod gauges;
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
    SlideBack,
    CalendarNext,
    CalendarPrevious,
    NoticeExit,
    Close,
}
impl Effect {
    pub fn slide_direction(self) -> f32 {
        match self {
            Self::Slide => 1.0,
            Self::SlideBack => -1.0,
            _ => 0.0,
        }
    }
    fn is_slide(self) -> bool {
        self.slide_direction() != 0.0
    }
}
fn overlay_header_fade(overlay: bool, closing: bool, amount: f32) -> f32 {
    if !overlay {
        return 0.0;
    }
    if !closing {
        return 1.0;
    }
    let t = (amount / 0.45).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn calendar_header_return(amount: f32) -> f32 {
    // Exchange the enlarged HH:MM widget for the full bar date before the
    // surface is restored. Both cached rasters follow the same geometry curve.
    let t = (1.0 - amount / 0.55).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn calendar_clock_layout(amount: f32, calendar: bool) -> [f32; 2] {
    if !calendar {
        return [1.0, 1.0];
    }
    // The caption travels with the clock, but leaves before the collapsed bar's
    // date returns. Never show two dates in separate rows during the handoff.
    let t = ((amount - 0.55) / 0.25).clamp(0.0, 1.0);
    [
        super::expansion::CALENDAR_DATE_Y / super::expansion::CALENDAR_WIDGET_HEIGHT,
        t * t * (3.0 - 2.0 * t),
    ]
}

pub fn spring(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t >= 1.0 {
        1.0
    } else {
        1.0 - (-10.0 * t).exp() * ((10.0 * t).cos() + (10.0 * t).sin())
    }
}
pub fn indicator_shape(from: f32, to: f32, t: f32) -> [f32; 4] {
    if (from - to).abs() < 0.01 {
        return [to, to, 0.0, 5.0];
    }
    let t = t.clamp(0.0, 1.0);
    let smooth = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let wave = (std::f32::consts::PI * t).sin().max(0.0);
    [
        from + (to - from) * smooth(t),
        from + (to - from) * smooth(t.powf(1.7)),
        3.5 * wave,
        5.0 - 0.7 * wave,
    ]
}
struct Indicator {
    from: f32,
    to: f32,
    start: Instant,
    last: Cell<f32>,
}
impl Indicator {
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / 0.36).min(1.0)
    }
    fn animating(&self) -> bool {
        (self.from - self.to).abs() > 0.01 && (self.time() < 1.0 || self.last.get() < 1.0)
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
pub fn launcher_geometry(effect: Effect, t: f32) -> (f32, f32) {
    launcher_geometry_from(effect, t, 1.0)
}
fn launcher_close_amount(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    // Zero velocity at both endpoints; the surface and its blur share this curve.
    1.0 - t * t * (3.0 - 2.0 * t)
}
fn launcher_geometry_from(effect: Effect, t: f32, close_from: f32) -> (f32, f32) {
    if !matches!(effect, Effect::Open | Effect::Close) {
        return (1.0, 0.0);
    }
    if effect == Effect::Close {
        let remaining = launcher_close_amount(t);
        // Buffer alpha cannot fade Niri's backdrop blur. Shrink the actual
        // surface to a pixel instead, keeping material, content and blur joined
        // until destruction; never leave a 94%-sized blur-only rectangle.
        let start_scale = 0.94 + 0.06 * close_from;
        return (
            start_scale * remaining + 0.002 * (1.0 - remaining),
            12.0 * (1.0 - close_from) + 8.0 * (1.0 - remaining),
        );
    }
    let amount = super::expansion::relaxed(t);
    (0.94 + 0.06 * amount, 12.0 * (1.0 - amount))
}
fn shimmer_position(seconds: f32) -> f32 {
    let phase = (seconds.rem_euclid(1.95) / 1.4).min(1.0);
    -0.28 + 1.56 * phase
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
    clock_previous: Option<glow::NativeUniformLocation>,
    clock_progress: Option<glow::NativeUniformLocation>,
    clock_layout: Option<glow::NativeUniformLocation>,
    header_fade: Option<glow::NativeUniformLocation>,
    return_image: Option<glow::NativeUniformLocation>,
    return_blend: Option<glow::NativeUniformLocation>,
    motion_mode: Option<glow::NativeUniformLocation>,
    motion_rect: Option<glow::NativeUniformLocation>,
    list_rect: Option<glow::NativeUniformLocation>,
    reflow: Option<glow::NativeUniformLocation>,
    calendar_prev: Option<glow::NativeUniformLocation>,
    calendar_next: Option<glow::NativeUniformLocation>,
    indicator: Option<glow::NativeUniformLocation>,
    logical_size: Option<glow::NativeUniformLocation>,
    shimmer_rect: Option<glow::NativeUniformLocation>,
    shimmer_position: Option<glow::NativeUniformLocation>,
    corner_radius: Option<glow::NativeUniformLocation>,
    bar_style: Option<glow::NativeUniformLocation>,
    lock_sharp: Option<glow::NativeUniformLocation>,
    lock_amount: Option<glow::NativeUniformLocation>,
    lock_texel: Option<glow::NativeUniformLocation>,
}
pub struct Gpu {
    display: Handle,
    config: Handle,
    context: Handle,
    gl: RefCell<Option<glow::Context>>,
    program: RefCell<Option<Program>>,
    gauge_renderer: RefCell<Option<gauges::Renderer>>,
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
                gauge_renderer: RefCell::new(None),
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
        let vert="precision highp float; attribute vec2 position; varying highp vec2 uv; void main(){uv=vec2((position.x+1.0)*0.5,(1.0-position.y)*0.5);gl_Position=vec4(position,0.0,1.0);}";
        // Keep a stationary material under independently animated foreground content.
        // Niri owns blur; full-window motion is applied to wl_surface geometry, not this quad.
        // A mediump UV loses subpixel precision on large physical buffers.
        // That destroys already antialiased glyph edges, particularly in the
        // right-hand notification column. Keep locals, parameters and samplers
        // highp too, not just the varying. Unsupported GPUs retain SHM fallback.
        let frag = r#"precision highp float;
precision highp sampler2D;
varying highp vec2 uv;
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
uniform sampler2D clockPrevious;
uniform float clockProgress;
uniform vec2 clockLayout;
uniform float headerFade;
uniform sampler2D returnImage;
uniform float returnBlend;
uniform float motionMode;
uniform vec4 motionRect;
uniform vec4 listRect;
uniform float reflow;
uniform vec4 calendarPrev;
uniform vec4 calendarNext;
uniform vec4 indicator;
uniform vec2 logicalSize;
uniform vec4 shimmerRect;
uniform float shimmerPosition;
uniform float cornerRadius;
uniform float barStyle;
uniform sampler2D lockSharp;
uniform float lockAmount;
uniform vec2 lockTexel;
float shellEdge(vec2 pos,vec2 size){
    float radius=min(cornerRadius,min(size.x,size.y)*0.5);
    vec2 d=abs(pos-size*0.5)-(size*0.5-vec2(radius));
    return length(max(d,vec2(0.0)))+min(max(d.x,d.y),0.0)-radius;
}
// One material for the idle bar, every expansion frame, and the open panel.
// Neither a height threshold nor a second rim can swap its appearance.
vec4 shellMaterial(vec2 pos,vec2 size,float edge,float left){
    vec2 axis=vec2(size.x*0.65,size.y);
    float g=clamp(dot(pos,axis)/dot(axis,axis),0.0,1.0);
    vec4 highlight=g<0.48
        ?mix(vec4(1.0,1.0,1.0,0.115),vec4(0.91,0.95,1.0,0.035),g/0.48)
        :mix(vec4(0.91,0.95,1.0,0.035),vec4(1.0,1.0,1.0,0.065),(g-0.48)/0.52);
    vec4 base=vec4(vec3(25.0,27.0,34.0)/255.0*0.54,0.54);
    base=vec4(highlight.rgb*highlight.a,highlight.a)+base*(1.0-highlight.a);
    float diagonal=clamp(dot(pos,size)/dot(size,size),0.0,1.0);
    vec4 crystal=diagonal<0.35
        ?mix(vec4(1.0,1.0,1.0,0.35),vec4(1.0,1.0,1.0,0.045),diagonal/0.35)
        :mix(vec4(1.0,1.0,1.0,0.045),vec4(0.86,0.93,1.0,0.17),(diagonal-0.35)/0.65);
    float line=1.0-smoothstep(0.0,0.8,abs(edge+0.6));
    float alpha=crystal.a*line;
    base=vec4(crystal.rgb*alpha,alpha)+base*(1.0-alpha);
    float y=pos.y/size.y;
    float top=mix(0.40,0.62,left),bottom=mix(0.24,0.34,left);
    float rim=y<0.45?mix(top,0.055,y/0.45):mix(0.055,bottom,(y-0.45)/0.55);
    alpha=rim*(1.0-smoothstep(0.0,1.0,abs(edge+0.75)));
    return vec4(vec3(alpha),alpha)+base*(1.0-alpha);
}
vec4 illuminate(vec4 scene, vec2 q){
    if(shimmerRect.z<=0.0)return scene;
    vec2 local=(q-shimmerRect.xy)/shimmerRect.zw;
    if(local.x<0.0||local.x>1.0||local.y<0.0||local.y>1.0)return scene;
    float mask=texture2D(headerImage,local).a;
    // One finite beam leaves completely before re-entering from the left.
    // A wide, pure-white core is brighter without duplicating the highlight.
    float beam=1.0-smoothstep(0.09,0.24,abs(local.x-shimmerPosition));
    float a=mask*beam;
    return vec4(vec3(a),a)+scene*(1.0-a);
}
vec4 decorate(vec4 scene,vec2 q){
    if(indicator.w<0.5)return scene;
    vec2 pos=q*logicalSize;
    float d=length(pos-vec2(indicator.x,17.0))-indicator.w;
    if(indicator.z>0.01){
        float tail=length(pos-vec2(indicator.y,17.0))-indicator.z;
        float h=clamp(0.5+0.5*(tail-d)/6.0,0.0,1.0);
        d=mix(tail,d,h)-6.0*h*(1.0-h);
    }
    float a=1.0-smoothstep(-0.4,0.4,d);
    return vec4(vec3(0.96,0.97,1.0)*a,a)+scene*(1.0-a);
}
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
bool inside(vec2 q,vec4 r){return q.x>=r.x&&q.x<r.x+r.z&&q.y>=r.y&&q.y<r.y+r.w;}
vec4 pageSlide(vec2 q){
    float t=clamp(progress,0.0,1.0);
    vec2 a=q+vec2(displacement*t,0.0);
    vec2 b=q-vec2(displacement*(1.0-t),0.0);
    bool incoming=displacement>0.0?q.x>=1.0-t:q.x<t;
    // Exactly one page owns each pixel. No crossfade or fading text trails.
    if(incoming)return sampleScene(currentImage,b,ratios.xy);
    return sampleScene(previousImage,a,ratios.zw);
}
vec4 regional(vec2 q){
    vec4 now=sampleScene(currentImage,q,ratios.xy);
    if(abs(motionMode)<0.5)return now;
    float t=clamp(progress,0.0,1.0);
    if(abs(motionMode)<1.5){
        if(!inside(q,motionRect))return now;
        // Navigation buttons stay fixed, both on screen and in the sampled
        // source scenes (otherwise a second ghost button would slide away).
        if(inside(q,calendarPrev)||inside(q,calendarNext))return now;
        vec2 a=q+vec2(0.0,motionMode*t*motionRect.w);
        vec2 b=q-vec2(0.0,motionMode*(1.0-t)*motionRect.w);
        vec4 old=inside(a,motionRect)&&!inside(a,calendarPrev)&&!inside(a,calendarNext)?sampleScene(previousImage,a,ratios.zw):vec4(0.0);
        vec4 next=inside(b,motionRect)&&!inside(b,calendarPrev)&&!inside(b,calendarNext)?sampleScene(currentImage,b,ratios.xy):vec4(0.0);
        return next+old*(1.0-next.a);
    }
    if(!inside(q,listRect))return now;
    vec4 base=vec4(0.0);
    vec2 lower=q+vec2(0.0,reflow*t);
    if(q.y<motionRect.y)base=sampleScene(previousImage,q,ratios.zw);
    else if(lower.y>=motionRect.y+motionRect.w&&inside(lower,listRect))base=sampleScene(previousImage,lower,ratios.zw);
    // Reflowing rows already contain their text. Revealing the destination
    // behind them too would briefly draw the same notification twice.
    else base=abs(reflow)<0.001?now*t:vec4(0.0);
    base=mix(base,now,smoothstep(0.90,1.0,t));
    vec2 exiting=q+vec2(motionRect.z*t,0.0);
    vec4 card=inside(exiting,motionRect)&&inside(exiting,listRect)?sampleScene(previousImage,exiting,ratios.zw)*(1.0-t):vec4(0.0);
    return card+base*(1.0-card.a);
}
void main(){
    if(lockAmount>=0.0){
        vec4 locked=illuminate(regional(uv),uv);
        if(lockAmount>=0.9999){gl_FragColor=locked;return;}
        // Fixed-cost blur only during the short transition, never at idle.
        vec2 d=lockTexel*(lockAmount*18.0);
        vec4 desktop=texture2D(lockSharp,uv)*0.20;
        desktop+=(texture2D(lockSharp,uv+vec2(d.x,0.0))+texture2D(lockSharp,uv-vec2(d.x,0.0))+texture2D(lockSharp,uv+vec2(0.0,d.y))+texture2D(lockSharp,uv-vec2(0.0,d.y)))*0.12;
        desktop+=(texture2D(lockSharp,uv+d)+texture2D(lockSharp,uv-d)+texture2D(lockSharp,uv+vec2(d.x,-d.y))+texture2D(lockSharp,uv+vec2(-d.x,d.y)))*0.08;
        gl_FragColor=mix(desktop.bgra,locked,lockAmount);return;
    }
    if(joined>=0.0){
        vec2 pos=uv*extent.xy;
        float edge=shellEdge(pos,extent.xy);
        float mask=1.0-smoothstep(-0.7,0.3,edge);
        vec4 base=shellMaterial(pos,extent.xy,edge,0.0);
        vec2 q=(pos-vec2((extent.x-extent.z)*0.5,extent.w<=34.0?0.0:10.0*(1.0-joined)))/extent.zw;
        vec4 body=regional(q);
        if(blending>0.5&&abs(motionMode)<0.5){
            if(abs(displacement)>0.5)body=pageSlide(q);
            else body=mix(sampleScene(previousImage,q,ratios.zw),body,progress);
        }
        body*=smoothstep(0.08,0.65,joined);
        vec4 result=body+base*(1.0-body.a);
        vec4 clockInk=vec4(0.0);
        vec2 h=(pos-headerRect.xy)/headerRect.zw;
        if(h.x>=0.0&&h.x<=1.0&&h.y>=0.0&&h.y<=1.0){
            vec4 ink=texture2D(headerImage,h).bgra;
            if(clockProgress<1.0&&h.y<clockLayout.x){
                // Move only the cached clock ink; material and Niri blur stay still.
                vec2 entering=h-vec2(0.0,(1.0-clockProgress)*0.12);
                vec2 leaving=h+vec2(0.0,clockProgress*0.12);
                vec4 next=texture2D(headerImage,clamp(entering,0.0,1.0)).bgra;
                vec4 old=texture2D(clockPrevious,clamp(leaving,0.0,1.0)).bgra;
                next*=step(0.0,entering.y)*(1.0-step(clockLayout.x,entering.y));
                old*=step(0.0,leaving.y)*(1.0-step(clockLayout.x,leaving.y));
                ink=mix(old,next,clockProgress);
            }
            if(h.y>=clockLayout.x)ink*=clockLayout.y;
            clockInk=ink*(1.0-headerFade);
            if(returnBlend>0.0){
                // One moving canvas and one premultiplied composite, including
                // overlap pixels. A second over operation leaves double ink.
                clockInk=mix(clockInk,texture2D(returnImage,h).bgra,returnBlend);
            }
        }
        result=clockInk+result*(1.0-clockInk.a);
        gl_FragColor=result*mask;return;
    }
    vec4 plain=illuminate(regional(uv),uv);
    if(barStyle>=0.0){
        vec2 pos=uv*logicalSize;
        float edge=shellEdge(pos,logicalSize);
        float mask=1.0-smoothstep(-0.7,0.3,edge);
        vec4 base=shellMaterial(pos,logicalSize,edge,barStyle);
        vec4 ink=plain;
        if(blending>0.5)ink=mix(sampleScene(previousImage,uv,ratios.zw),plain,progress);
        ink=decorate(ink,uv);
        gl_FragColor=(ink+base*(1.0-ink.a))*mask;return;
    }
    if(abs(motionMode)>0.5){gl_FragColor=plain;return;}
    if(blending<0.5){gl_FragColor=decorate(plain,uv);return;}
    float t=clamp(progress,0.0,1.0);
    vec4 content;
    if(abs(displacement)>0.5)content=foreground(pageSlide(uv));
    else content=mix(foreground(sampleScene(previousImage,uv,ratios.zw)),foreground(plain),t);
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
            clock_previous: gl.get_uniform_location(native, "clockPrevious"),
            clock_progress: gl.get_uniform_location(native, "clockProgress"),
            clock_layout: gl.get_uniform_location(native, "clockLayout"),
            header_fade: gl.get_uniform_location(native, "headerFade"),
            return_image: gl.get_uniform_location(native, "returnImage"),
            return_blend: gl.get_uniform_location(native, "returnBlend"),
            motion_mode: gl.get_uniform_location(native, "motionMode"),
            motion_rect: gl.get_uniform_location(native, "motionRect"),
            list_rect: gl.get_uniform_location(native, "listRect"),
            reflow: gl.get_uniform_location(native, "reflow"),
            calendar_prev: gl.get_uniform_location(native, "calendarPrev"),
            calendar_next: gl.get_uniform_location(native, "calendarNext"),
            indicator: gl.get_uniform_location(native, "indicator"),
            logical_size: gl.get_uniform_location(native, "logicalSize"),
            shimmer_rect: gl.get_uniform_location(native, "shimmerRect"),
            shimmer_position: gl.get_uniform_location(native, "shimmerPosition"),
            corner_radius: gl.get_uniform_location(native, "cornerRadius"),
            bar_style: gl.get_uniform_location(native, "barStyle"),
            lock_sharp: gl.get_uniform_location(native, "lockSharp"),
            lock_amount: gl.get_uniform_location(native, "lockAmount"),
            lock_texel: gl.get_uniform_location(native, "lockTexel"),
        };
        *self.gauge_renderer.borrow_mut() = Some(gauges::Renderer::new(&gl)?);
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
    size: (i32, i32),
}
unsafe fn upload_header_pixels(gl: &glow::Context, bytes: &[u8], w: i32, h: i32, reuse: bool) {
    if reuse {
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
    } else {
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
}
const CLOCK_TURN_MS: u64 = 260;
struct ClockTurn {
    previous: Header,
    start: Instant,
    last: Cell<f32>,
}
impl ClockTurn {
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / (CLOCK_TURN_MS as f32 / 1000.0)).min(1.0)
    }
    fn active(&self) -> bool {
        self.time() < 1.0 || self.last.get() < 1.0
    }
}
fn clock_should_turn(previous: Option<i64>, minute: i64, settled: bool) -> bool {
    settled && previous.is_some_and(|old| old != minute)
}
struct LockBackdrop {
    texture: glow::NativeTexture,
    width: i32,
    height: i32,
    motion: LockMotion,
}
struct LockMotion {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    last: Cell<f32>,
}
impl LockMotion {
    fn time(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }
    fn amount(&self) -> f32 {
        self.from + (self.to - self.from) * super::expansion::relaxed(self.time())
    }
    fn active(&self) -> bool {
        self.time() < 1.0 || self.last.get() < 1.0
    }
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
    launcher: bool,
    launcher_close_from: f32,
    lock_backdrop: Option<LockBackdrop>,
    background: f32,
    bar_style: Option<bool>,
    expansion: Option<super::expansion::Expansion>,
    header: Option<Header>,
    return_header: Option<Header>,
    clock_minute: Option<i64>,
    clock_turn: Option<ClockTurn>,
    osd: bool,
    motion_rect: [f32; 4],
    list_rect: [f32; 4],
    reflow: f32,
    calendar_prev: [f32; 4],
    calendar_next: [f32; 4],
    indicator: Option<Indicator>,
    logical_size: (f32, f32),
    shimmer: Option<[f32; 4]>,
    shimmer_start: Instant,
    gauges: super::gauge::Batch,
    previous_gauges: super::gauge::Batch,
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
            launcher: false,
            launcher_close_from: 1.0,
            lock_backdrop: None,
            background: 0.8,
            bar_style: None,
            expansion: None,
            header: None,
            return_header: None,
            clock_minute: None,
            clock_turn: None,
            osd: false,
            motion_rect: [0.0; 4],
            list_rect: [0.0; 4],
            reflow: 0.0,
            calendar_prev: [0.0; 4],
            calendar_next: [0.0; 4],
            indicator: None,
            logical_size: (w as f32, h as f32),
            shimmer: None,
            shimmer_start: Instant::now(),
            gauges: super::gauge::Batch::default(),
            previous_gauges: super::gauge::Batch::default(),
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
    pub fn set_bar_style(&mut self, left: Option<bool>) {
        self.bar_style = left;
    }
    pub fn set_gauges(
        &mut self,
        specs: Vec<super::gauge::Gauge>,
        size: (i32, i32),
        page_changed: bool,
    ) -> bool {
        if page_changed {
            self.previous_gauges = self.gauges.clone();
            self.previous_gauges.freeze();
            self.gauges.clear();
        }
        self.gauges.update(specs, (size.0 as f32, size.1 as f32))
    }
    pub fn gauge_count(&self) -> usize {
        self.gauges.items.len()
    }
    pub fn set_launcher(&mut self, launcher: bool) {
        self.launcher = launcher;
    }
    pub fn set_lock_background(&mut self, bytes: &[u8], width: i32, height: i32) {
        if self.lock_backdrop.is_some() || !self.current() {
            return;
        }
        let gl = self.gpu.gl.borrow();
        let Some(gl) = gl.as_ref() else { return };
        unsafe {
            let Ok(texture) = gl.create_texture() else {
                return;
            };
            gl.active_texture(glow::TEXTURE3);
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
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(bytes)),
            );
            self.lock_backdrop = Some(LockBackdrop {
                texture,
                width,
                height,
                motion: LockMotion {
                    from: 0.0,
                    to: 1.0,
                    start: Instant::now(),
                    duration: Duration::from_millis(380),
                    last: Cell::new(0.0),
                },
            });
        }
    }
    pub fn lock_animating(&self) -> bool {
        self.lock_backdrop
            .as_ref()
            .is_some_and(|b| b.motion.active())
    }
    #[cfg(test)]
    pub fn lock_amount(&self) -> Option<f32> {
        self.lock_backdrop.as_ref().map(|b| b.motion.amount())
    }
    pub fn release_lock(&mut self) {
        if let Some(b) = &mut self.lock_backdrop {
            b.motion.from = b.motion.amount();
            b.motion.to = 0.0;
            b.motion.start = Instant::now();
            b.motion.duration = Duration::from_millis(300);
            b.motion.last.set(0.0);
        }
    }
    pub fn set_shimmer(&mut self, rect: Option<super::model::Rect>, size: (i32, i32)) {
        self.shimmer = rect.filter(|_| self.header.is_some()).map(|r| {
            [
                (r.x / f64::from(size.0)) as f32,
                (r.y / f64::from(size.1)) as f32,
                (r.w / f64::from(size.0)) as f32,
                (r.h / f64::from(size.1)) as f32,
            ]
        });
    }
    pub fn shimmer_active(&self) -> bool {
        self.shimmer.is_some()
    }
    pub fn set_return_width(&mut self, width: i32) {
        if let Some(expansion) = &mut self.expansion {
            expansion.source = width.max(1) as f32;
        }
    }
    pub fn set_osd(&mut self, osd: bool) {
        if osd && !self.osd {
            // Never crossfade an overlay from a cached copy of the clock bar.
            // The first overlay upload replaces both scene textures.
            self.fingerprint = None;
            self.active = false;
            self.effect = Effect::Immediate;
        }
        self.osd = osd;
    }
    pub fn set_indicator(&mut self, target: Option<f32>, size: (i32, i32)) {
        self.logical_size = (size.0 as f32, size.1 as f32);
        let Some(to) = target else {
            self.indicator = None;
            return;
        };
        if let Some(marker) = &mut self.indicator {
            if (marker.to - to).abs() > 0.01 {
                marker.from = indicator_shape(marker.from, marker.to, marker.time())[0];
                marker.to = to;
                marker.start = Instant::now();
                marker.last.set(0.0);
            }
        } else {
            self.indicator = Some(Indicator {
                from: to,
                to,
                start: Instant::now(),
                last: Cell::new(1.0),
            });
        }
    }
    pub fn begin_expansion(
        &mut self,
        source: i32,
        target: (i32, i32),
        calendar: bool,
        follow_width: bool,
    ) {
        if self.expansion.is_none() {
            self.expansion = Some(super::expansion::Expansion::new(
                source,
                target,
                calendar,
                calendar || follow_width,
            ));
        }
        if let Some(e) = &mut self.expansion {
            if follow_width && e.target.1 == target.1 {
                e.follow_width(target.0);
            } else {
                e.retarget(target, self.header.as_ref().map_or(e.source, |h| h.width));
            }
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
        self.gauges.clear();
        self.previous_gauges.clear();
        self.osd = false;
        self.expansion = None;
        self.clock_minute = None;
        if let Some(turn) = self.clock_turn.take() {
            if self.current() {
                if let Some(gl) = self.gpu.gl.borrow().as_ref() {
                    unsafe {
                        gl.delete_texture(turn.previous.texture);
                    }
                }
            }
        }
        for h in [self.header.take(), self.return_header.take()]
            .into_iter()
            .flatten()
        {
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
        self.clock_minute = None;
        self.upload_header(bytes, w, h, logical_width, false)
    }
    pub fn set_return_header(&mut self, bytes: &[u8], w: i32, h: i32, width: f32) {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        hash.write(bytes);
        hash.write_i32(w);
        hash.write_i32(h);
        let fingerprint = hash.finish();
        if self
            .return_header
            .as_ref()
            .is_some_and(|h| h.fingerprint == fingerprint)
            || !self.current()
        {
            return;
        }
        let gl = self.gpu.gl.borrow();
        let gl = gl.as_ref().unwrap();
        unsafe {
            let texture = match self.return_header.as_ref() {
                Some(h) => h.texture,
                None => match gl.create_texture() {
                    Ok(t) => t,
                    Err(_) => return,
                },
            };
            gl.active_texture(glow::TEXTURE5);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for (name, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
            }
            let reuse = self
                .return_header
                .as_ref()
                .is_some_and(|header| header.size == (w, h));
            upload_header_pixels(gl, bytes, w, h, reuse);
            self.return_header = Some(Header {
                texture,
                fingerprint,
                width,
                size: (w, h),
            });
        }
    }
    pub fn set_clock_header(
        &mut self,
        bytes: &[u8],
        w: i32,
        h: i32,
        logical_width: f32,
        minute: i64,
    ) -> bool {
        let settled = self
            .expansion
            .as_ref()
            .is_some_and(|e| !e.closing() && !e.animating());
        let turn = clock_should_turn(self.clock_minute, minute, settled);
        let changed = self.upload_header(bytes, w, h, logical_width, turn);
        self.clock_minute = Some(minute);
        changed
    }
    fn upload_header(
        &mut self,
        bytes: &[u8],
        w: i32,
        h: i32,
        logical_width: f32,
        animate: bool,
    ) -> bool {
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
            let reuse = !animate
                && self
                    .header
                    .as_ref()
                    .is_some_and(|header| header.size == (w, h));
            let texture = match (&self.header, animate) {
                (Some(h), false) => h.texture,
                _ => match gl.create_texture() {
                    Ok(t) => t,
                    Err(_) => return false,
                },
            };
            if let Some(turn) = self.clock_turn.take() {
                gl.delete_texture(turn.previous.texture);
            }
            if animate {
                self.clock_turn = self.header.take().map(|previous| ClockTurn {
                    previous,
                    start: Instant::now(),
                    last: Cell::new(0.0),
                });
            }
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
            upload_header_pixels(gl, bytes, w, h, reuse);
            self.header = Some(Header {
                texture,
                fingerprint,
                width: logical_width,
                size: (w, h),
            });
        }
        true
    }
    pub fn set_motion(&mut self, rect: super::model::Rect, size: (i32, i32), reflow: f64) {
        let (w, h) = (size.0 as f64, size.1 as f64);
        self.motion_rect = [
            (rect.x / w) as f32,
            (rect.y / h) as f32,
            (rect.w / w) as f32,
            (rect.h / h) as f32,
        ];
        self.list_rect = [
            (super::notifications::LIST_X / w) as f32,
            (62.0 / h) as f32,
            ((f64::from(super::notifications::WIDTH) - super::notifications::LIST_X - 20.0) / w)
                as f32,
            ((h - 82.0) / h) as f32,
        ];
        self.reflow = (reflow / h) as f32;
        self.calendar_prev = [
            (18.0 / w) as f32,
            ((62.0 + super::draw::CALENDAR_SHIFT) / h) as f32,
            (40.0 / w) as f32,
            (34.0 / h) as f32,
        ];
        self.calendar_next = [
            (262.0 / w) as f32,
            ((62.0 + super::draw::CALENDAR_SHIFT) / h) as f32,
            (40.0 / w) as f32,
            (34.0 / h) as f32,
        ];
    }
    pub fn regional_animation(&self) -> bool {
        matches!(
            self.effect,
            Effect::CalendarNext | Effect::CalendarPrevious | Effect::NoticeExit
        ) && self.animating()
    }
    pub fn page_transition(&self) -> bool {
        self.effect.is_slide() && self.animating()
    }
    pub fn effect(&self) -> Effect {
        self.effect
    }
    pub fn update(
        &mut self,
        bytes: &[u8],
        w: i32,
        h: i32,
        effect: Effect,
        known_changed: bool,
    ) -> bool {
        let hash = if known_changed {
            0
        } else {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            hash.write(bytes);
            hash.write_i32(w);
            hash.write_i32(h);
            hash.finish()
        };
        if (!known_changed && self.fingerprint == Some(hash)) || !self.current() {
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
        let reset = !self.animating()
            || matches!(
                effect,
                Effect::Slide
                    | Effect::SlideBack
                    | Effect::Immediate
                    | Effect::CalendarNext
                    | Effect::CalendarPrevious
                    | Effect::NoticeExit
            )
            || first;
        if reset {
            self.front = 1 - self.front;
            self.effect = if first && effect != Effect::Immediate {
                Effect::Open
            } else {
                effect
            };
            self.start = Instant::now();
            self.duration =
                Duration::from_millis(if self.launcher && self.effect == Effect::Open {
                    320
                } else if self.effect.is_slide()
                    || matches!(
                        self.effect,
                        Effect::Open
                            | Effect::CalendarNext
                            | Effect::CalendarPrevious
                            | Effect::NoticeExit
                    )
                {
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
                if self.fingerprint.is_some() && self.sizes[index] == (w, h) {
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        0,
                        0,
                        w,
                        h,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(bytes)),
                    );
                } else {
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
        self.gauges.animating()
            || self.lock_animating()
            || self.clock_turn.as_ref().is_some_and(ClockTurn::active)
            || self.expanding()
            || self.indicator.as_ref().is_some_and(Indicator::animating)
            || (self.active && (self.t() < 1.0 || self.last.get() < 1.0))
    }
    pub fn closing(&self) -> bool {
        self.expansion
            .as_ref()
            .map_or(self.effect == Effect::Close, |e| e.closing())
    }
    pub fn close(&mut self) {
        self.gauges.freeze();
        let header = self.header.as_ref().map_or(0.0, |h| h.width);
        if let Some(e) = &mut self.expansion {
            e.close(header);
            self.active = false;
            return;
        }
        if self.launcher {
            // Closing during entry must continue from its current geometry,
            // rather than snapping back to full size before shrinking again.
            self.launcher_close_from = if self.effect == Effect::Open {
                super::expansion::relaxed(self.t())
            } else {
                1.0
            };
        }
        self.effect = Effect::Close;
        self.start = Instant::now();
        self.duration = Duration::from_millis(if self.launcher { 220 } else { 190 });
        self.last.set(0.0);
        self.active = true;
    }
    pub fn geometry(&self, notice: bool) -> (f32, f32) {
        if self.launcher {
            if self.effect == Effect::Close {
                launcher_geometry_from(self.effect, self.t(), self.launcher_close_from)
            } else {
                launcher_geometry(self.effect, self.t())
            }
        } else {
            geometry(self.effect, self.t(), notice)
        }
    }
    pub fn present(&self, expansion: Option<super::expansion::Frame>) -> bool {
        self.present_frame(
            expansion,
            #[cfg(test)]
            None,
        )
    }
    fn present_frame(
        &self,
        expansion: Option<super::expansion::Frame>,
        #[cfg(test)] inspect: Option<&mut dyn FnMut(&glow::Context, i32, i32)>,
    ) -> bool {
        #[cfg(test)]
        let inspect_only = inspect.is_some();
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
            gl.uniform_1_f32(p.corner_radius.as_ref(), super::draw::PANEL_RADIUS as f32);
            gl.uniform_1_f32(
                p.bar_style.as_ref(),
                self.bar_style
                    .map_or(-1.0, |left| if left { 1.0 } else { 0.0 }),
            );
            let lock = self.lock_backdrop.as_ref();
            gl.uniform_1_f32(
                p.lock_amount.as_ref(),
                lock.map_or(-1.0, |b| b.motion.amount()),
            );
            if let Some(b) = lock {
                gl.active_texture(glow::TEXTURE3);
                gl.bind_texture(glow::TEXTURE_2D, Some(b.texture));
                gl.uniform_1_i32(p.lock_sharp.as_ref(), 3);
                gl.uniform_2_f32(
                    p.lock_texel.as_ref(),
                    1.0 / b.width as f32,
                    1.0 / b.height as f32,
                );
                b.motion.last.set(b.motion.time());
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(p.vertices));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            for (unit, index) in [(0, self.front), (1, 1 - self.front)] {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[index]));
            }
            gl.uniform_1_i32(p.now.as_ref(), 0);
            gl.uniform_1_i32(p.old.as_ref(), 1);
            let shine = self.shimmer.unwrap_or([0.0; 4]);
            gl.uniform_4_f32(
                p.shimmer_rect.as_ref(),
                shine[0],
                shine[1],
                shine[2],
                shine[3],
            );
            gl.uniform_1_f32(
                p.shimmer_position.as_ref(),
                shimmer_position(self.shimmer_start.elapsed().as_secs_f32()),
            );
            if self.shimmer.is_some() {
                if let Some(header) = &self.header {
                    gl.active_texture(glow::TEXTURE2);
                    gl.bind_texture(glow::TEXTURE_2D, Some(header.texture));
                    gl.uniform_1_i32(p.header_image.as_ref(), 2);
                }
            }
            gl.uniform_1_f32(
                p.header_fade.as_ref(),
                overlay_header_fade(
                    self.osd,
                    self.closing(),
                    expansion.map_or(0.0, |f| f.amount),
                ),
            );
            gl.uniform_1_i32(p.clock_previous.as_ref(), 4);
            gl.uniform_1_i32(p.return_image.as_ref(), 5);
            gl.active_texture(glow::TEXTURE5);
            gl.bind_texture(
                glow::TEXTURE_2D,
                self.return_header.as_ref().map(|h| h.texture),
            );
            gl.uniform_1_f32(
                p.return_blend.as_ref(),
                expansion
                    .filter(|_| self.return_header.is_some())
                    .map_or(0.0, |f| calendar_header_return(f.amount)),
            );
            let clock_layout = calendar_clock_layout(
                expansion.map_or(1.0, |f| f.amount),
                self.expansion.as_ref().is_some_and(|e| e.calendar),
            );
            gl.uniform_2_f32(p.clock_layout.as_ref(), clock_layout[0], clock_layout[1]);
            let clock_t = self.clock_turn.as_ref().map_or(1.0, ClockTurn::time);
            gl.uniform_1_f32(
                p.clock_progress.as_ref(),
                super::expansion::relaxed(clock_t),
            );
            gl.active_texture(glow::TEXTURE4);
            gl.bind_texture(
                glow::TEXTURE_2D,
                self.clock_turn.as_ref().map(|turn| turn.previous.texture),
            );
            let marker = self
                .indicator
                .as_ref()
                .map_or([0.0; 4], |i| indicator_shape(i.from, i.to, i.time()));
            gl.uniform_4_f32(
                p.indicator.as_ref(),
                marker[0],
                marker[1],
                marker[2],
                marker[3],
            );
            gl.uniform_2_f32(
                p.logical_size.as_ref(),
                self.logical_size.0,
                self.logical_size.1,
            );
            gl.uniform_1_f32(
                p.motion_mode.as_ref(),
                if t >= 1.0 {
                    0.0
                } else {
                    match self.effect {
                        Effect::CalendarNext => 1.0,
                        Effect::CalendarPrevious => -1.0,
                        Effect::NoticeExit => 2.0,
                        _ => 0.0,
                    }
                },
            );
            gl.uniform_4_f32(
                p.motion_rect.as_ref(),
                self.motion_rect[0],
                self.motion_rect[1],
                self.motion_rect[2],
                self.motion_rect[3],
            );
            gl.uniform_4_f32(
                p.list_rect.as_ref(),
                self.list_rect[0],
                self.list_rect[1],
                self.list_rect[2],
                self.list_rect[3],
            );
            gl.uniform_1_f32(p.reflow.as_ref(), self.reflow);
            gl.uniform_4_f32(
                p.calendar_prev.as_ref(),
                self.calendar_prev[0],
                self.calendar_prev[1],
                self.calendar_prev[2],
                self.calendar_prev[3],
            );
            gl.uniform_4_f32(
                p.calendar_next.as_ref(),
                self.calendar_next[0],
                self.calendar_next[1],
                self.calendar_next[2],
                self.calendar_next[3],
            );
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
                    f.header_height,
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
            let progress = if self.effect.is_slide() {
                super::expansion::relaxed(t)
            } else {
                1.0 - (1.0 - t).powi(3)
            };
            gl.uniform_1_f32(p.progress.as_ref(), progress);
            gl.uniform_1_f32(p.displacement.as_ref(), self.effect.slide_direction());
            gl.uniform_1_f32(p.background.as_ref(), self.background);
            gl.uniform_1_f32(
                p.blending.as_ref(),
                if (self.effect == Effect::Change || self.effect.is_slide()) && t < 1.0 {
                    1.0
                } else {
                    0.0
                },
            );
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            if let Some(renderer) = self.gpu.gauge_renderer.borrow().as_ref() {
                let (frame, offset, alpha) = if let Some(f) = expansion {
                    let target = self.expansion.as_ref().unwrap().target;
                    let amount = ((f.amount - 0.08) / (0.65 - 0.08)).clamp(0.0, 1.0);
                    (
                        (f.size.0 as f32, f.size.1 as f32),
                        ((f.size.0 - target.0) as f32 * 0.5, 10.0 * (1.0 - f.amount)),
                        amount * amount * (3.0 - 2.0 * amount),
                    )
                } else {
                    (self.logical_size, (0.0, 0.0), 1.0)
                };
                let target = self
                    .expansion
                    .as_ref()
                    .map_or(self.logical_size.0, |e| e.target.0 as f32);
                let pixel = frame.0 / self.w as f32;
                let slide = if self.effect.is_slide() && t < 1.0 {
                    self.effect.slide_direction()
                } else {
                    0.0
                };
                if slide != 0.0 {
                    renderer.paint(
                        gl,
                        &self.previous_gauges,
                        frame,
                        (
                            offset.0 + target
                                - self.previous_gauges.size.0
                                - slide * progress * target,
                            offset.1,
                        ),
                        alpha,
                        pixel,
                        1,
                    );
                }
                renderer.paint(
                    gl,
                    &self.gauges,
                    frame,
                    (
                        offset.0 + target - self.gauges.size.0 + slide * (1.0 - progress) * target,
                        offset.1,
                    ),
                    alpha,
                    pixel,
                    0,
                );
            }
            #[cfg(test)]
            if let Some(inspect) = inspect {
                inspect(gl, self.w, self.h);
            }
            #[cfg(test)]
            let ok = inspect_only || eglSwapBuffers(self.gpu.display, self.surface) != 0;
            #[cfg(not(test))]
            let ok = eglSwapBuffers(self.gpu.display, self.surface) != 0;
            self.last.set(t);
            if let Some(turn) = &self.clock_turn {
                turn.last.set(clock_t);
            }
            if let Some(indicator) = &self.indicator {
                indicator.last.set(indicator.time());
            }
            if let (Some(e), Some(f)) = (&self.expansion, expansion) {
                e.presented(f.time);
            }
            ok
        }
    }
}
#[cfg(test)]
impl Window {
    fn verify_calendar_returns_to_the_cached_bar_before_restoration(&mut self) {
        self.finish_expansion();
        self.update(&vec![0; 780 * 500 * 4], 780, 500, Effect::Immediate, true);
        // Red is the enlarged widget; blue is the normal date/time bar.
        self.set_header(&[0, 0, 255, 255], 1, 1, 90.0);
        self.set_return_header(&[255, 0, 0, 255], 1, 1, 180.0);
        self.begin_expansion(180, (780, 500), true, false);
        for amount in [1.0, 0.75, 0.55, 0.4, 0.2, 0.05, 0.0] {
            let frame = super::expansion::layout(amount, 180.0, (780, 500), 90.0, true);
            let blend = calendar_header_return(amount);
            let mut inspect = |gl: &glow::Context, w: i32, h: i32| {
                let mut pixel = [0; 4];
                unsafe {
                    gl.read_pixels(
                        ((frame.header.0 + 45.0 * frame.header_scale) / frame.size.0 as f32
                            * w as f32) as i32,
                        h - 1
                            - ((frame.header.1 + frame.header_height * 0.5) / frame.size.1 as f32
                                * h as f32) as i32,
                        1,
                        1,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(Some(&mut pixel)),
                    );
                }
                assert!(
                    f32::from(pixel[2]) + 2.0 >= 255.0 * blend,
                    "bar ink must appear while shrinking, amount={amount}: {pixel:?}"
                );
                assert!(
                    (f32::from(pixel[0]) - 255.0 * (1.0 - blend)).abs() <= 2.0,
                    "clock/bar overlap must be one premultiplied mix, amount={amount}: {pixel:?}"
                );
                if amount == 0.0 {
                    assert_eq!(
                        pixel,
                        [0, 0, 255, 255],
                        "last joined frame must already contain the normal bar"
                    );
                }
            };
            assert!(self.present_frame(Some(frame), Some(&mut inspect)));
        }
        self.finish_expansion();
        assert!(
            self.return_header.is_none(),
            "release the extra clock texture at idle"
        );
        println!("Calendar GPU: normal bar ink appears continuously before restoration; return texture released");
    }
    fn verify_overlay_excludes_cached_clock(&mut self) {
        let old = [0u8, 0, 255, 255].repeat(260 * 34);
        let osd = [255u8, 0, 0, 255].repeat(260 * 34);
        for _ in 0..3 {
            self.finish_expansion();
            self.update(&old, 260, 34, Effect::Immediate, true);
            self.set_header(&[0, 0, 255, 255], 1, 1, 180.0);
            self.begin_expansion(180, (260, 34), false, false);
            self.set_osd(true);
            assert!(
                self.fingerprint.is_none(),
                "overlay entry must discard old bar rasters"
            );
            self.update(&osd, 260, 34, Effect::Immediate, false);
            for amount in [0.0, 0.1, 0.3, 0.6, 1.0] {
                // Force the previous scene to be sampled as well as the header.
                self.effect = Effect::Change;
                self.active = true;
                self.start = Instant::now();
                let frame = super::expansion::layout(amount, 180.0, (260, 34), 180.0, false);
                let mut inspect = |gl: &glow::Context, _: i32, _: i32| {
                    let mut pixel = [0u8; 4];
                    unsafe {
                        gl.read_pixels(
                            130,
                            17,
                            1,
                            1,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelPackData::Slice(Some(&mut pixel)),
                        );
                    }
                    assert!(
                        u16::from(pixel[0]) <= u16::from(pixel[2]) + 2,
                        "old red clock leaked into opening at {amount}: {pixel:?}"
                    );
                };
                assert!(self.present_frame(Some(frame), Some(&mut inspect)));
            }
        }
        self.finish_expansion();
        println!("OSD GPU: no cached clock pixels across 3 openings and 15 intermediate frames");
    }
    /// GPU readback is deliberately test-only: never retain a live lock raster.
    fn verify_inline_gauge_contrast(&mut self) {
        use super::model::Rect;
        let old_size = self.logical_size;
        self.logical_size = (480.0, 500.0);
        let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, 480, 500).unwrap();
        {
            let cr = cairo::Context::new(&image).unwrap();
            cr.set_source_rgba(0.32, 0.33, 0.36, super::gauge::TRACK_ALPHA);
            cr.paint().unwrap();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            for x in [44.0, 400.0] {
                cr.rectangle(x, 99.0, 12.0, 12.0);
                cr.fill().unwrap();
            }
        }
        image.flush();
        for level in [0.0, 0.5, 1.0] {
            self.gauges.clear();
            self.set_gauges(
                vec![super::gauge::Gauge {
                    key: "inline-label-probe".into(),
                    rect: Rect {
                        x: 32.0,
                        y: 90.0,
                        w: 408.0,
                        h: 30.0,
                    },
                    clip: Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 480.0,
                        h: 500.0,
                    },
                    radius: 15.0,
                    vertical: false,
                    alpha: 1.0,
                    level,
                    direct: false,
                }],
                (480, 500),
                false,
            );
            self.update(&image.data().unwrap(), 480, 500, Effect::Immediate, true);
            let mut inspect = |gl: &glow::Context, _: i32, h: i32| {
                let read = |x| {
                    let mut pixel = [0; 4];
                    unsafe {
                        gl.read_pixels(
                            x,
                            h - 1 - 105,
                            1,
                            1,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelPackData::Slice(Some(&mut pixel)),
                        );
                    }
                    pixel[0]
                };
                for (x, covered) in [(50, level > 0.0), (406, level == 1.0)] {
                    let ink = read(x);
                    assert!(
                        if covered { ink < 70 } else { ink > 220 },
                        "GPU inline ink contrast at {level}: x={x}, ink={ink}"
                    );
                }
                if level > 0.0 {
                    assert!(read(90) > 245, "filled capsule must remain white");
                }
            };
            assert!(self.present_frame(None, Some(&mut inspect)));
        }
        self.gauges.clear();
        self.logical_size = old_size;
        println!("GPU gauges: cached inline labels retain contrast at 0/50/100% fill");
    }
    fn verify_shell_material_continuity(&mut self) {
        let old_size = self.logical_size;
        let old_style = self.bar_style;
        let capture = |window: &Self, frame| {
            let mut pixels = vec![0; (window.w * window.h * 4) as usize];
            let mut inspect = |gl: &glow::Context, w: i32, h: i32| unsafe {
                gl.read_pixels(
                    0,
                    0,
                    w,
                    h,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixels)),
                );
            };
            assert!(window.present_frame(frame, Some(&mut inspect)));
            pixels
        };
        for height in [34, 35, 60, 99, 100, 101, 250, 500] {
            let size = (480, height);
            self.finish_expansion();
            self.set_indicator(None, size);
            self.update(
                &vec![0; (size.0 * size.1 * 4) as usize],
                size.0,
                size.1,
                Effect::Immediate,
                true,
            );
            self.set_bar_style(Some(false));
            let stretched_bar = capture(self, None);
            self.set_bar_style(None);
            self.set_header(&[0; 4], 1, 1, 480.0);
            self.begin_expansion(480, size, false, false);
            let frame = super::expansion::Frame {
                size,
                header: (0.0, 0.0),
                header_scale: 1.0,
                header_height: 34.0,
                amount: 1.0,
                time: 1.0,
            };
            let panel = capture(self, Some(frame));
            assert_eq!(
                panel, stretched_bar,
                "bar and panel material diverged at {height}px"
            );
        }
        self.finish_expansion();
        self.logical_size = old_size;
        self.bar_style = old_style;
        println!("Shell material: identical GPU pixels for bar and expanded panel at 8 heights (34–500px)");
    }
    pub fn verify_text_copy(&mut self, expected: &[u8], logical: (i32, i32)) {
        let original_size = (self.w, self.h);
        let check = |window: &Self, frame, label| {
            let mut max_error = 0u8;
            let mut changed = 0usize;
            let mut inspect = |gl: &glow::Context, w: i32, h: i32| {
                let mut output = vec![0u8; (w * h * 4) as usize];
                unsafe {
                    gl.read_pixels(
                        0,
                        0,
                        w,
                        h,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(Some(&mut output)),
                    );
                }
                assert_eq!(output.len(), expected.len());
                // Joined surfaces have a rounded outer mask; test the content,
                // including both halves of UV space, rather than that mask.
                for y in 48..h as usize - 48 {
                    for x in 48..w as usize - 48 {
                        let a = (y * w as usize + x) * 4;
                        let b = ((h as usize - 1 - y) * w as usize + x) * 4;
                        for (source, channel) in [2, 1, 0, 3].into_iter().enumerate() {
                            let error = expected[a + source].abs_diff(output[b + channel]);
                            max_error = max_error.max(error);
                            changed += usize::from(error > 1);
                        }
                    }
                }
            };
            assert!(window.present_frame(frame, Some(&mut inspect)));
            assert_eq!(
                changed, 0,
                "{label}: GPU lost coverage in {changed} channels, max error {max_error}"
            );
            println!(
                "{label}: {}x{} physical pixels, max channel error {max_error}",
                window.w, window.h
            );
        };
        check(self, None, "lock copy");
        assert!(self.set_header(&[0; 4], 1, 1, 1.0));
        self.begin_expansion(logical.0, logical, false, false);
        let frame = super::expansion::layout(1.0, logical.0 as f32, logical, 1.0, false);
        check(self, Some(frame), "notification joined copy");
        self.finish_expansion();
        self.verify_overlay_excludes_cached_clock();
        self.verify_calendar_returns_to_the_cached_bar_before_restoration();
        self.verify_inline_gauge_contrast();
        self.verify_shell_material_continuity();
        self.update(
            expected,
            original_size.0,
            original_size.1,
            Effect::Immediate,
            true,
        );
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
                if let Some(h) = &self.return_header {
                    gl.delete_texture(h.texture);
                }
                if let Some(turn) = &self.clock_turn {
                    gl.delete_texture(turn.previous.texture);
                }
                if let Some(b) = &self.lock_backdrop {
                    gl.delete_texture(b.texture);
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
    fn calendar_caption_leaves_before_the_bar_date_returns() {
        use super::*;
        let mut previous = 0.0;
        for step in 0..=100 {
            let amount = step as f32 / 100.0;
            let [bottom, caption] = calendar_clock_layout(amount, true);
            assert!(bottom > 0.7 && bottom < 0.8);
            assert!(caption >= previous);
            assert_eq!(caption * calendar_header_return(amount), 0.0);
            assert_eq!(calendar_clock_layout(amount, false), [1.0, 1.0]);
            previous = caption;
        }
        assert_eq!(previous, 1.0);
    }
    #[test]
    fn calendar_clock_becomes_the_bar_before_the_last_frame() {
        assert_eq!(super::calendar_header_return(1.0), 0.0);
        assert_eq!(super::calendar_header_return(0.55), 0.0);
        assert_eq!(super::calendar_header_return(0.0), 1.0);
        assert!(super::calendar_header_return(0.05) > 0.97);
        let values: Vec<_> = (0..=100)
            .map(|step| super::calendar_header_return(1.0 - step as f32 / 100.0))
            .collect();
        assert!(values.windows(2).all(|v| v[1] >= v[0]));
        assert!(values.windows(2).all(|v| v[1] - v[0] < 0.03));
    }
    #[test]
    fn opening_overlay_never_reveals_clock_and_only_closing_fades_it_back() {
        for step in 0..=100 {
            let amount = step as f32 / 100.0;
            assert_eq!(super::overlay_header_fade(true, false, amount), 1.0);
            assert_eq!(super::overlay_header_fade(false, false, amount), 0.0);
        }
        assert_eq!(super::overlay_header_fade(true, true, 1.0), 1.0);
        assert_eq!(super::overlay_header_fade(true, true, 0.0), 0.0);
        let mut previous = 1.0;
        for step in (0..=100).rev() {
            let fade = super::overlay_header_fade(true, true, step as f32 / 100.0);
            assert!(fade <= previous);
            previous = fade;
        }
    }
    #[test]
    fn clock_turns_only_on_a_new_minute_in_an_already_open_calendar() {
        use super::*;
        assert!(!clock_should_turn(None, 600, true));
        assert!(!clock_should_turn(Some(600), 600, true));
        assert!(!clock_should_turn(Some(600), 601, false));
        assert!(clock_should_turn(Some(600), 601, true));
        assert!(clock_should_turn(Some(1439), 1440, true));
        assert_eq!(CLOCK_TURN_MS, 260);
    }
    #[test]
    fn launcher_close_reduces_surface_and_blur_together_without_reversing() {
        use super::{launcher_close_amount, launcher_geometry, Effect};
        let mut previous = 1.0;
        let mut previous_scale = 1.0;
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let amount = launcher_close_amount(t);
            assert!((0.0..=previous).contains(&amount));
            let (scale, _) = launcher_geometry(Effect::Close, t);
            assert!((0.002..=previous_scale).contains(&scale));
            previous_scale = scale;
            previous = amount;
        }
        let (scale, _) = launcher_geometry(Effect::Close, 1.0);
        assert_eq!((570.0 * scale).round(), 1.0);
        assert_eq!((465.0 * scale).round(), 1.0);
        assert_eq!(launcher_close_amount(0.0), 1.0);
        assert_eq!(launcher_close_amount(1.0), 0.0);
        assert!(launcher_close_amount(0.01) > 0.999);
        assert!(launcher_close_amount(0.99) < 0.001);
    }
    #[test]
    fn launcher_close_during_open_keeps_current_geometry() {
        use super::{launcher_geometry, launcher_geometry_from, Effect};
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            assert_eq!(
                launcher_geometry(Effect::Open, t),
                launcher_geometry_from(Effect::Close, 0.0, super::super::expansion::relaxed(t))
            );
        }
    }
    #[test]
    fn launcher_opens_without_bounce_or_extreme_scaling() {
        use super::*;
        let (mut last_scale, mut last_y) = (0.94, 12.0);
        for i in 0..=100 {
            let (scale, y) = launcher_geometry(Effect::Open, i as f32 / 100.0);
            assert!((0.94..=1.0).contains(&scale));
            assert!((0.0..=12.0).contains(&y));
            assert!(scale >= last_scale && y <= last_y);
            last_scale = scale;
            last_y = y;
        }
        assert_eq!(launcher_geometry(Effect::Open, 1.0), (1.0, 0.0));
        assert_eq!(launcher_geometry(Effect::Close, 1.0), (0.002, 8.0));
    }
    #[test]
    fn placeholder_light_makes_one_left_to_right_pass_then_reenters_offscreen() {
        assert!((super::shimmer_position(0.0) + 0.28).abs() < 0.001);
        for step in 1..140 {
            let previous = super::shimmer_position((step - 1) as f32 * 0.01);
            let next = super::shimmer_position(step as f32 * 0.01);
            assert!(next > previous);
            assert!((next - previous - 1.56 * 0.01 / 1.4).abs() < 0.0001);
        }
        assert!((super::shimmer_position(0.7) - 0.5).abs() < 0.001);
        assert!((super::shimmer_position(2.65) - 0.5).abs() < 0.001);
        assert!((super::shimmer_position(3.9) - super::shimmer_position(0.0)).abs() < 0.001);
        // The compact beam has radius 0.24: both ends are dark at the reset.
        assert!(super::shimmer_position(0.0) < -0.24);
        assert!(super::shimmer_position(1.399) > 1.24);
        for time in [1.4, 1.6, 1.94] {
            assert!((super::shimmer_position(time) - 1.28).abs() < 0.001);
        }
    }
    #[test]
    fn liquid_indicator_travels_without_jumping_and_settles_as_a_circle() {
        use super::indicator_shape;
        assert_eq!(indicator_shape(58.0, 90.0, 0.0), [58.0, 58.0, 0.0, 5.0]);
        let end = indicator_shape(58.0, 90.0, 1.0);
        assert_eq!((end[0], end[1], end[3]), (90.0, 90.0, 5.0));
        assert!(end[2] < 0.0001);
        let middle = indicator_shape(58.0, 90.0, 0.5);
        assert!(middle[0] > middle[1] && middle[0] < 90.0 && middle[2] > 3.0);
        let mut last = 58.0;
        for step in 0..=100 {
            let shape = indicator_shape(58.0, 90.0, step as f32 / 100.0);
            assert!(shape[0] >= last - 0.0001 && shape[0] <= 90.0001);
            last = shape[0];
        }
    }
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
        assert_eq!(geometry(Effect::SlideBack, 0.5, false), (1.0, 0.0));
        assert_eq!(geometry(Effect::Open, 0.0, true).1, -1.0);
    }
    #[test]
    fn navigation_pushes_one_full_page_and_reverses_when_going_back() {
        use super::Effect;
        for (effect, direction) in [(Effect::Slide, 1.0), (Effect::SlideBack, -1.0)] {
            assert!(effect.is_slide());
            assert_eq!(effect.slide_direction(), direction);
            for step in 0..=100 {
                let progress = super::super::expansion::relaxed(step as f32 / 100.0);
                let outgoing = -direction * progress;
                let incoming = direction * (1.0 - progress);
                assert!(((incoming - outgoing).abs() - 1.0).abs() < 0.00001);
            }
        }
        assert!(!Effect::Change.is_slide());
        assert_eq!(Effect::Change.slide_direction(), 0.0);
    }
}
