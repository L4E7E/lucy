//! Small antialiased gauge quads over cached scene textures, not full-screen loops.
use super::*;
use crate::gauge::Batch;
pub(super) struct Renderer {
    program: glow::NativeProgram,
    frame: Option<glow::NativeUniformLocation>,
    rect: Option<glow::NativeUniformLocation>,
    clip: Option<glow::NativeUniformLocation>,
    style: Option<glow::NativeUniformLocation>,
    edge: Option<glow::NativeUniformLocation>,
}
impl Renderer {
    pub(super) unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let vertex = r#"precision highp float;
attribute vec2 position;
uniform vec4 frame;
uniform vec4 bounds;
varying highp vec2 point;
void main(){
    vec2 local=vec2((position.x+1.0)*0.5,(1.0-position.y)*0.5);
    point=bounds.xy+local*bounds.zw;
    gl_Position=vec4(point/frame.xy*vec2(2.0,-2.0)+vec2(-1.0,1.0),0.0,1.0);
}"#;
        let fragment = r#"precision highp float;
varying highp vec2 point;
uniform vec4 frame;
uniform vec4 bounds;
uniform vec4 clip;
uniform vec4 style;
uniform float pixel;
float rounded(vec2 p,vec4 r,float radius){
    vec2 d=abs(p-r.xy-r.zw*0.5)-(r.zw*0.5-vec2(radius));
    return length(max(d,vec2(0.0)))+min(max(d.x,d.y),0.0)-radius;
}
void main(){
    if(point.x<clip.x||point.y<clip.y||point.x>=clip.x+clip.z||point.y>=clip.y+clip.w||style.x<=0.0)discard;
    vec4 fill=bounds;float radius=0.0;
    if(style.y>0.5){fill.y+=fill.w*(1.0-style.x);fill.w*=style.x;}
    else {fill.z*=style.x;radius=min(fill.z,fill.w)*0.5;}
    float distance=max(rounded(point,bounds,style.w),rounded(point,fill,radius));
    distance=max(distance,rounded(point,vec4(0.0,0.0,frame.xy),min(20.0,min(frame.x,frame.y)*0.5)));
    float alpha=(1.0-smoothstep(-pixel*0.5,pixel*0.5,distance))*style.z*frame.z;
    gl_FragColor=vec4(vec3(0.96,0.97,1.0)*alpha,alpha);
}"#;
        let program = gl.create_program()?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, vertex),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let error = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                gl.delete_program(program);
                return Err(error);
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.bind_attrib_location(program, 0, "position");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let error = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(error);
        }
        Ok(Self {
            program,
            frame: gl.get_uniform_location(program, "frame"),
            rect: gl.get_uniform_location(program, "bounds"),
            clip: gl.get_uniform_location(program, "clip"),
            style: gl.get_uniform_location(program, "style"),
            edge: gl.get_uniform_location(program, "pixel"),
        })
    }
    pub(super) unsafe fn paint(
        &self,
        gl: &glow::Context,
        batch: &Batch,
        frame: (f32, f32),
        offset: (f32, f32),
        alpha: f32,
        pixel: f32,
    ) {
        if batch.items.is_empty() || alpha <= 0.0 {
            return;
        }
        let now = Instant::now();
        gl.use_program(Some(self.program));
        gl.enable(glow::BLEND);
        gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        gl.uniform_4_f32(self.frame.as_ref(), frame.0, frame.1, alpha, 0.0);
        gl.uniform_1_f32(self.edge.as_ref(), pixel);
        for motion in &batch.items {
            let g = &motion.gauge;
            let r = g.rect;
            let c = g.clip;
            let level = motion.value(now);
            if level <= 0.0 {
                continue;
            }
            gl.uniform_4_f32(
                self.rect.as_ref(),
                r.x as f32 + offset.0,
                r.y as f32 + offset.1,
                r.w as f32,
                r.h as f32,
            );
            gl.uniform_4_f32(
                self.clip.as_ref(),
                c.x as f32 + offset.0,
                c.y as f32 + offset.1,
                c.w as f32,
                c.h as f32,
            );
            gl.uniform_4_f32(
                self.style.as_ref(),
                level,
                if g.vertical { 1.0 } else { 0.0 },
                g.alpha,
                g.radius,
            );
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        gl.disable(glow::BLEND);
        batch.presented(now);
    }
}
