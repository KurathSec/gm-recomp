//! What the D3D HLE hands to the host's renderer. The device state (render states,
//! texture stage states, transforms, viewport) lives here; every draw is passed to the
//! host as a self-contained [`DrawCall`] whose vertex and index data the host reads
//! from guest memory during the call. Textures are identified by the guest address of
//! their D3D object and uploaded from guest memory when first used after a change.
//!
//! The browser implements [`Gpu`] with WebGL2; the harness uses the no-op default.

/// D3DRENDERSTATETYPE indices used by the hosts.
pub mod rs {
    pub const ZENABLE: usize = 7;
    pub const FILLMODE: usize = 8;
    pub const SHADEMODE: usize = 9;
    pub const ZWRITEENABLE: usize = 14;
    pub const ALPHATESTENABLE: usize = 15;
    pub const LASTPIXEL: usize = 16;
    pub const SRCBLEND: usize = 19;
    pub const DESTBLEND: usize = 20;
    pub const CULLMODE: usize = 22;
    pub const ZFUNC: usize = 23;
    pub const ALPHAREF: usize = 24;
    pub const ALPHAFUNC: usize = 25;
    pub const DITHERENABLE: usize = 26;
    pub const ALPHABLENDENABLE: usize = 27;
    pub const FOGENABLE: usize = 28;
    pub const SPECULARENABLE: usize = 29;
    pub const FOGCOLOR: usize = 34;
    pub const FOGTABLEMODE: usize = 35;
    pub const FOGSTART: usize = 36;
    pub const FOGEND: usize = 37;
    pub const FOGDENSITY: usize = 38;
    pub const STENCILENABLE: usize = 52;
    pub const TEXTUREFACTOR: usize = 60;
    pub const LIGHTING: usize = 137;
    pub const AMBIENT: usize = 139;
    pub const FOGVERTEXMODE: usize = 140;
    pub const COLORVERTEX: usize = 141;
    pub const NORMALIZENORMALS: usize = 143;
    pub const DIFFUSEMATERIALSOURCE: usize = 145;
    pub const POINTSIZE: usize = 154;
    pub const COLORWRITEENABLE: usize = 168;
    pub const BLENDOP: usize = 171;
}

/// D3DTEXTURESTAGESTATETYPE (Direct3D 8) indices.
pub mod tss {
    pub const COLOROP: usize = 1;
    pub const COLORARG1: usize = 2;
    pub const COLORARG2: usize = 3;
    pub const ALPHAOP: usize = 4;
    pub const ALPHAARG1: usize = 5;
    pub const ALPHAARG2: usize = 6;
    pub const TEXCOORDINDEX: usize = 11;
    pub const ADDRESSU: usize = 13;
    pub const ADDRESSV: usize = 14;
    pub const BORDERCOLOR: usize = 15;
    pub const MAGFILTER: usize = 16;
    pub const MINFILTER: usize = 17;
    pub const MIPFILTER: usize = 18;
    pub const TEXTURETRANSFORMFLAGS: usize = 24;
    pub const ADDRESSW: usize = 25;
    pub const COLORARG0: usize = 26;
    pub const ALPHAARG0: usize = 27;
    pub const RESULTARG: usize = 28;
}

pub const STAGES: usize = 8;
pub const TSS_COUNT: usize = 32;

/// One draw with everything needed to render it. `#[repr(C)]`: the browser reads it
/// in place (web/src/gpu.mjs mirrors the field offsets).
#[repr(C)]
#[derive(Clone)]
pub struct DrawCall {
    /// D3DPRIMITIVETYPE.
    pub prim: u32,
    pub prim_count: u32,
    /// Vertices addressed by the draw (for indexed draws: `min_index..min_index + n`).
    pub vertex_count: u32,
    pub fvf: u32,
    pub stride: u32,
    /// Guest address of vertex 0 (indices are relative to it).
    pub vertices: u32,
    /// Guest address of the first index, 0 for non-indexed draws.
    pub indices: u32,
    /// 2 or 4.
    pub index_size: u32,
    pub min_index: u32,
    /// Texture bound to stages 0 and 1 (object address, 0 = none), its D3DFORMAT, and
    /// whether its rows are stored bottom-up (render targets).
    pub texture: [u32; 2],
    pub tex_format: [u32; 2],
    pub tex_flip: [u32; 2],
    pub target_w: u32,
    pub target_h: u32,
    /// x, y, width, height, min z, max z.
    pub viewport: [f32; 6],
    pub world: [f32; 16],
    pub view: [f32; 16],
    pub proj: [f32; 16],
    pub rs: [u32; 256],
    /// Texture stage states of stages 0 and 1.
    pub tss: [[u32; TSS_COUNT]; 2],
}

/// One Direct3D 9 draw through vertex and pixel shaders. `#[repr(C)]`: the browser
/// reads it in place (web/src/gpu.mjs mirrors the field offsets).
#[repr(C)]
#[derive(Clone)]
pub struct DrawCall9 {
    /// D3DPRIMITIVETYPE.
    pub prim: u32,
    pub prim_count: u32,
    /// Vertices the draw addresses: `min_index..min_index + vertex_count`.
    pub vertex_count: u32,
    pub min_index: u32,
    /// Guest address of the first index (0: not indexed) and its size (2 or 4).
    pub indices: u32,
    pub index_size: u32,
    /// Per stream: guest address of vertex 0 and stride.
    pub streams: [[u32; 2]; 4],
    /// Vertex elements: stream, offset, D3DDECLTYPE, usage | usage index << 8.
    pub elements: [[u32; 4]; 16],
    pub element_count: u32,
    /// Shader objects (see `Gpu::shader_create`).
    pub vs: u32,
    pub ps: u32,
    /// Per sampler: texture object (0: none), its D3DFORMAT, rows stored bottom-up.
    pub textures: [[u32; 3]; 16],
    /// D3DSAMPLERSTATETYPE values per sampler (index = state).
    pub samplers: [[u32; 14]; 16],
    pub target_w: u32,
    pub target_h: u32,
    /// x, y, width, height, min z, max z.
    pub viewport: [f32; 6],
    /// x, y, width, height (used when SCISSORTESTENABLE).
    pub scissor: [i32; 4],
    pub rs: [u32; 256],
    pub vs_f: [[f32; 4]; 256],
    pub ps_f: [[f32; 4]; 224],
    pub vs_i: [[i32; 4]; 16],
    pub ps_i: [[i32; 4]; 16],
    pub vs_b: [u32; 16],
    pub ps_b: [u32; 16],
}

const _: () = assert!(std::mem::size_of::<DrawCall9>() == 2701 * 4);

/// The host renderer. Every method has a no-op default (the harness).
pub trait Gpu {
    /// The back buffer (render target 0) was created or resized.
    fn resize(&mut self, _w: u32, _h: u32) {}
    fn texture_create(&mut self, _id: u32, _w: u32, _h: u32, _levels: u32, _fmt: u32, _render_target: bool) {}
    /// Uploads one mip level from guest memory (`pitch` bytes per row).
    fn texture_upload(&mut self, _id: u32, _level: u32, _w: u32, _h: u32, _fmt: u32, _pitch: u32, _bits: u32) {}
    fn texture_delete(&mut self, _id: u32) {}
    /// Renders into texture `id` (level 0), or the back buffer for 0.
    fn set_target(&mut self, _id: u32, _w: u32, _h: u32) {}
    /// D3DCLEAR flags over the rectangle (x, y, w, h).
    fn clear(&mut self, _flags: u32, _color: u32, _z: f32, _stencil: u32, _rect: [i32; 4]) {}
    fn draw(&mut self, _d: &DrawCall) {}
    /// A Direct3D 9 shader translated to GLSL ES 3.00 (`id` is its D3D object).
    fn shader_create(&mut self, _id: u32, _vertex: bool, _glsl: &str) {}
    fn shader_delete(&mut self, _id: u32) {}
    fn draw9(&mut self, _d: &DrawCall9) {}
    /// Copies rectangle `src_rect` (x, y, w, h) of target/texture `src` (0: back buffer)
    /// into `dst_rect` of `dst`, scaling with linear or point filtering.
    fn blit(&mut self, _src: u32, _src_rect: [i32; 4], _dst: u32, _dst_rect: [i32; 4], _linear: bool) {}
    /// Reads target/texture `id` (w x h, 32-bit pixels) into guest memory at `dst`.
    fn read_target(&mut self, _id: u32, _w: u32, _h: u32, _dst: u32, _pitch: u32) {}
    /// End of a frame: rectangle `src` (x, y, w, h) of the back buffer is shown at
    /// `dst_w` x `dst_h`. `clock_us` is the guest's virtual time, which the host uses to
    /// pace frames against real time.
    fn present(&mut self, _clock_us: u64, _src: [i32; 4], _dst_w: u32, _dst_h: u32) {}
}

pub struct NullGpu;
impl Gpu for NullGpu {}

/// Direct3D 8 device state.
pub struct DeviceState {
    pub rs: [u32; 256],
    pub tss: [[u32; TSS_COUNT]; STAGES],
    pub textures: [u32; STAGES],
    pub world: [f32; 16],
    pub view: [f32; 16],
    pub proj: [f32; 16],
    pub viewport: [f32; 6],
    pub fvf: u32,
    /// Stream 0: vertex buffer object and stride.
    pub stream: (u32, u32),
    /// Index buffer object and base vertex index.
    pub indices: (u32, u32),
    /// Current render target (texture object, 0 = back buffer) and its size.
    pub target: (u32, u32, u32),
    pub backbuffer: (u32, u32),
    /// The device window (where the frame is shown and mouse input goes).
    pub window: u32,
    /// Direct3D 8: the surfaces bound as render target and depth buffer (the device
    /// holds a reference to each).
    pub rt_surface: u32,
    pub depth_surface: u32,
}

pub const IDENTITY: [f32; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

impl Default for DeviceState {
    fn default() -> Self {
        let mut r = [0u32; 256];
        let f = |x: f32| x.to_bits();
        r[rs::ZENABLE] = 0;
        r[rs::FILLMODE] = 3;
        r[rs::SHADEMODE] = 2;
        r[rs::ZWRITEENABLE] = 1;
        r[rs::LASTPIXEL] = 1;
        r[rs::SRCBLEND] = 2;
        r[rs::DESTBLEND] = 1;
        r[rs::CULLMODE] = 3;
        r[rs::ZFUNC] = 4;
        r[rs::ALPHAFUNC] = 8;
        r[rs::FOGEND] = f(1.0);
        r[rs::FOGDENSITY] = f(1.0);
        r[rs::TEXTUREFACTOR] = 0xFFFF_FFFF;
        r[rs::LIGHTING] = 1;
        r[rs::COLORVERTEX] = 1;
        r[rs::DIFFUSEMATERIALSOURCE] = 1;
        r[rs::POINTSIZE] = f(1.0);
        r[rs::COLORWRITEENABLE] = 0xF;
        r[rs::BLENDOP] = 1;
        let mut t = [[0u32; TSS_COUNT]; STAGES];
        for (i, s) in t.iter_mut().enumerate() {
            s[tss::COLOROP] = if i == 0 { 4 } else { 1 };
            s[tss::COLORARG1] = 2;
            s[tss::COLORARG2] = 1;
            s[tss::ALPHAOP] = if i == 0 { 2 } else { 1 };
            s[tss::ALPHAARG1] = 2;
            s[tss::ALPHAARG2] = 1;
            s[tss::TEXCOORDINDEX] = i as u32;
            s[tss::ADDRESSU] = 1;
            s[tss::ADDRESSV] = 1;
            s[tss::ADDRESSW] = 1;
            s[tss::MAGFILTER] = 1;
            s[tss::MINFILTER] = 1;
            s[tss::MIPFILTER] = 0;
            s[tss::COLORARG0] = 1;
            s[tss::ALPHAARG0] = 1;
            s[tss::RESULTARG] = 1;
        }
        DeviceState {
            rs: r,
            tss: t,
            textures: [0; STAGES],
            world: IDENTITY,
            view: IDENTITY,
            proj: IDENTITY,
            viewport: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            fvf: 0,
            stream: (0, 0),
            indices: (0, 0),
            target: (0, 0, 0),
            backbuffer: (0, 0),
            window: 0,
            rt_surface: 0,
            depth_surface: 0,
        }
    }
}

/// Vertices addressed by `count` primitives of type `prim`.
pub fn vertices_for(prim: u32, count: u32) -> u32 {
    match prim {
        1 => count,
        2 => count * 2,
        3 => count + 1,
        4 => count * 3,
        5 | 6 => count + 2,
        _ => 0,
    }
}

/// Size in bytes of one vertex of format `fvf`.
pub fn fvf_stride(fvf: u32) -> u32 {
    let mut n = match fvf & 0xE {
        0x2 => 12,
        0x4 => 16,
        0x6 => 16,
        0x8 => 20,
        0xA => 24,
        0xC => 28,
        0xE => 32,
        _ => 0,
    };
    if fvf & 0x10 != 0 {
        n += 12;
    }
    if fvf & 0x20 != 0 {
        n += 4;
    }
    if fvf & 0x40 != 0 {
        n += 4;
    }
    if fvf & 0x80 != 0 {
        n += 4;
    }
    let count = (fvf >> 8) & 0xF;
    for i in 0..count {
        n += match (fvf >> (16 + 2 * i)) & 3 {
            0 => 8,
            1 => 12,
            2 => 16,
            _ => 4,
        };
    }
    n
}

// web/src/gpu.mjs reads DrawCall as 391 32-bit words.
const _: () = assert!(std::mem::size_of::<DrawCall>() == 391 * 4);
