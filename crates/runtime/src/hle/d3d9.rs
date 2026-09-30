//! Direct3D 9 (GameMaker: Studio 1.4 renders through it), built like the Direct3D 8
//! layer: COM objects in guest memory with host-side state, resources stored in guest
//! memory and uploaded when used. Shaders are translated to GLSL (`d3d9_shader`) when
//! created; draws go to the host as `gpu::DrawCall9` records, or through the Direct3D 8
//! fixed-function path when no shaders are set.

use super::gpu::{self, DrawCall9};
use super::{ApiFn, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{D3dObject, Process};
use crate::fasthash::HashMap;

pub const D3D_OK: u32 = 0;
pub const D3DERR_INVALIDCALL: u32 = 0x8876_086C;
pub const D3DERR_NOTAVAILABLE: u32 = 0x8876_086A;
pub const D3DERR_NOTFOUND: u32 = 0x8876_0866;
pub const E_NOINTERFACE: u32 = 0x8000_4002;
pub const S_FALSE: u32 = 1;

const D3DFMT_X8R8G8B8: u32 = 22;
const D3DFMT_D24S8: u32 = 75;
const D3DUSAGE_RENDERTARGET: u32 = 1;
const D3DUSAGE_DEPTHSTENCIL: u32 = 2;

const DLL: &str = "d3d9.dll";

type Method = (&'static str, u32, ApiFn);

/// Every interface the objects answer QueryInterface for, by vtable name.
const IIDS: &[(&str, &[&str])] = &[
    ("IDirect3D9Ex", &["{81BDCBCA-64D4-426D-AE8D-AD0147F4275C}", "{02177241-69FC-400C-8FF1-93A44DF6861D}"]),
    ("IDirect3DDevice9Ex", &["{D0223B96-BF7A-43FD-92BD-A43B0D82B9EB}", "{B18B10CE-2649-405A-870F-95F777D4313A}"]),
    ("IDirect3DTexture9", &["{85C31227-3DE5-4F00-9B3A-F11AC38C18B5}", "{580CA87E-1D3C-4D54-991D-B7D3E3C298CE}", "{05EEC05D-8F7D-4362-B999-D1BAF357C704}"]),
    ("IDirect3DSurface9", &["{0CFBAF3A-9FF6-429A-99B3-A2796AF8B89B}", "{05EEC05D-8F7D-4362-B999-D1BAF357C704}"]),
    ("IDirect3DVertexBuffer9", &["{B64BB1B5-FD70-4DF6-BF91-19D0A12455E3}", "{05EEC05D-8F7D-4362-B999-D1BAF357C704}"]),
    ("IDirect3DIndexBuffer9", &["{7C9DD65E-D3F7-4529-ACEE-785830ACDE35}", "{05EEC05D-8F7D-4362-B999-D1BAF357C704}"]),
    ("IDirect3DSwapChain9", &["{794950F2-ADFC-458A-905E-10A10B0B503B}", "{91886CAF-1C3D-4D2E-A0AB-3E4C7D8D3303}"]),
    ("IDirect3DQuery9", &["{D9771460-A695-4F26-BBD3-27B840B541CC}"]),
    ("IDirect3DStateBlock9", &["{B07C4FE5-310D-4BA8-A23C-4F0F206F218B}"]),
    ("IDirect3DVertexDeclaration9", &["{DD13C59C-36FA-4098-A8FB-C7ED39DC8546}"]),
    ("IDirect3DVertexShader9", &["{EFC5557E-6265-4613-8A94-43857889EB36}"]),
    ("IDirect3DPixelShader9", &["{6D3BDBDC-5B02-4415-B852-CE5E8BCCB289}"]),
];

/// Device state.
#[derive(Clone)]
pub struct State9 {
    pub device: u32,
    pub rs: [u32; 256],
    pub tss: [[u32; gpu::TSS_COUNT]; 8],
    pub samp: [[u32; 14]; 16],
    pub textures: [u32; 16],
    pub world: [f32; 16],
    pub view: [f32; 16],
    pub proj: [f32; 16],
    pub viewport: [f32; 6],
    pub scissor: [i32; 4],
    pub fvf: u32,
    pub decl: u32,
    /// Per stream: vertex buffer, offset, stride, frequency.
    pub streams: [(u32, u32, u32, u32); 4],
    pub indices: u32,
    pub vs: u32,
    pub ps: u32,
    pub vs_f: Box<[[f32; 4]; 256]>,
    pub ps_f: Box<[[f32; 4]; 224]>,
    pub vs_i: [[i32; 4]; 16],
    pub ps_i: [[i32; 4]; 16],
    pub vs_b: [u32; 16],
    pub ps_b: [u32; 16],
    /// Render target 0 (surface or texture object, 0 = back buffer) and its size.
    pub target: (u32, u32, u32),
    pub depth: u32,
    /// The surface bound as render target 0 (0 until the first SetRenderTarget: the
    /// back buffer).
    pub rt: u32,
    pub backbuffer: (u32, u32),
}

const IDENTITY: [f32; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

impl Default for State9 {
    fn default() -> Self {
        let mut rs = [0u32; 256];
        // D3D9 render state defaults that differ from zero.
        for (k, v) in [
            (7, 1),             // ZENABLE (with an auto depth buffer)
            (8, 3),             // FILLMODE solid
            (9, 2),             // SHADEMODE gouraud
            (14, 1),            // ZWRITEENABLE
            (16, 1),            // LASTPIXEL
            (19, 2),            // SRCBLEND one
            (20, 1),            // DESTBLEND zero
            (22, 3),            // CULLMODE ccw
            (23, 4),            // ZFUNC lessequal
            (25, 8),            // ALPHAFUNC always
            (37, 1f32.to_bits()), // FOGEND
            (38, 1f32.to_bits()), // FOGDENSITY
            (56, 8),            // STENCILFUNC always
            (59, 0xFFFF_FFFF),  // STENCILMASK
            (60, 0xFFFF_FFFF),  // TEXTUREFACTOR
            (63, 0xFFFF_FFFF),  // STENCILWRITEMASK
            (137, 1),           // LIGHTING
            (141, 1),           // COLORVERTEX
            (145, 1),           // DIFFUSEMATERIALSOURCE color1
            (146, 2),           // SPECULARMATERIALSOURCE color2
            (154, 1f32.to_bits()), // POINTSIZE
            (155, 1f32.to_bits()), // POINTSIZE_MIN
            (158, 1f32.to_bits()), // POINTSCALE_A
            (161, 1),           // MULTISAMPLEANTIALIAS
            (162, 0xFFFF_FFFF), // MULTISAMPLEMASK
            (166, 64f32.to_bits()), // POINTSIZE_MAX
            (168, 0xF),         // COLORWRITEENABLE
            (171, 1),           // BLENDOP add
            (190, 0xF),         // COLORWRITEENABLE1
            (191, 0xF),
            (192, 0xF),
            (199, 2),           // SRCBLENDALPHA one
            (200, 1),           // DESTBLENDALPHA zero
            (201, 1),           // BLENDOPALPHA add
        ] {
            rs[k] = v;
        }
        let mut tss = [[0u32; gpu::TSS_COUNT]; 8];
        for (i, s) in tss.iter_mut().enumerate() {
            s[gpu::tss::COLOROP] = if i == 0 { 4 } else { 1 }; // modulate / disable
            s[gpu::tss::COLORARG1] = 2; // texture
            s[gpu::tss::COLORARG2] = 0; // current
            s[gpu::tss::ALPHAOP] = if i == 0 { 2 } else { 1 }; // selectarg1 / disable
            s[gpu::tss::ALPHAARG1] = 2;
            s[gpu::tss::ALPHAARG2] = 0;
            s[gpu::tss::TEXCOORDINDEX] = i as u32;
        }
        let mut samp = [[0u32; 14]; 16];
        for s in samp.iter_mut() {
            s[1] = 1; // ADDRESSU wrap
            s[2] = 1;
            s[3] = 1;
            s[5] = 1; // MAGFILTER point
            s[6] = 1; // MINFILTER point
        }
        State9 {
            device: 0,
            rs,
            tss,
            samp,
            textures: [0; 16],
            world: IDENTITY,
            view: IDENTITY,
            proj: IDENTITY,
            viewport: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            scissor: [0; 4],
            fvf: 0,
            decl: 0,
            streams: [(0, 0, 0, 1); 4],
            indices: 0,
            vs: 0,
            ps: 0,
            vs_f: Box::new([[0.0; 4]; 256]),
            ps_f: Box::new([[0.0; 4]; 224]),
            vs_i: [[0; 4]; 16],
            ps_i: [[0; 4]; 16],
            vs_b: [0; 16],
            ps_b: [0; 16],
            target: (0, 0, 0),
            depth: 0,
            rt: 0,
            backbuffer: (0, 0),
        }
    }
}

fn okn(c: &mut Ctx) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), D3D_OK))
}

fn ret(c: &Ctx, v: u32) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), v))
}

/// Storage for resource contents: its own virtual-memory region, released with it.
fn alloc_storage(c: &mut Ctx, size: u32) -> u32 {
    let size = size.max(4).div_ceil(0x1000) * 0x1000;
    let Some(a) = c.p.vm.alloc(0, size, true, crate::mem::PAGE_READWRITE, false, "d3d9 resource") else { return 0 };
    if c.host.ensure_mapped(a, size).is_err() {
        return 0;
    }
    let _ = c.host.fill(a, size, 0);
    a
}

fn alloc(c: &mut Ctx, size: u32) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    c.p.heap_alloc(g, heap, size, true)
}

fn vtable(c: &mut Ctx, iface: &'static str, methods: &[Method]) -> u32 {
    if let Some(&v) = c.p.com_vtables.get(iface) {
        return v;
    }
    let v = alloc(c, methods.len() as u32 * 4);
    for (i, (name, _, _)) in methods.iter().enumerate() {
        let id = c.p.registry.lookup(DLL, &format!("{iface}::{name}")).expect("d3d9 method registered");
        let _ = c.host.write_u32(v + 4 * i as u32, Process::stub_addr(id));
    }
    c.p.com_vtables.insert(iface, v);
    v
}

fn new_object(c: &mut Ctx, iface: &'static str, methods: &[Method], data: HashMap<&'static str, u32>) -> u32 {
    let vt = vtable(c, iface, methods);
    let obj = alloc(c, 16);
    let _ = c.host.write_u32(obj, vt);
    c.p.d3d_objects.insert(obj, D3dObject { kind: iface, refcount: 1, data });
    obj
}

fn this(c: &Ctx) -> u32 {
    c.arg(0)
}

fn field(c: &Ctx, obj: u32, key: &str) -> u32 {
    c.p.d3d_objects.get(&obj).and_then(|o| o.data.get(key).copied()).unwrap_or(0)
}

fn set_field(c: &mut Ctx, obj: u32, key: &'static str, v: u32) {
    if let Some(o) = c.p.d3d_objects.get_mut(&obj) {
        o.data.insert(key, v);
    }
}

fn kind(c: &Ctx, obj: u32) -> &'static str {
    c.p.d3d_objects.get(&obj).map(|o| o.kind).unwrap_or("")
}

fn add_ref_obj(c: &mut Ctx, obj: u32) {
    if let Some(o) = c.p.d3d_objects.get_mut(&obj) {
        o.refcount += 1;
    }
}

fn out(c: &mut Ctx, p: u32, v: u32) {
    if p != 0 {
        let _ = c.host.write_u32(p, v);
    }
}

pub fn format_bpp(fmt: u32) -> u32 {
    match fmt {
        36 | 113 | 114 | 116 => 8, // A16B16G16R16, A16B16G16R16F, G32R32F... (8 bytes)
        115 => 16,                 // A32B32G32R32F
        _ => super::d3d8::format_bpp(fmt),
    }
}

// ---- IUnknown ---------------------------------------------------------------------

fn query_interface(c: &mut Ctx) -> ApiResult {
    let (t, iid, pp) = (this(c), c.arg(1), c.arg(2));
    let mut g = [0u8; 16];
    let _ = c.host.read(iid, &mut g);
    let want = super::com::format_iid(&g);
    let k = kind(c, t);
    let ok = want == "{00000000-0000-0000-C000-000000000046}" || IIDS.iter().any(|(name, iids)| *name == k && iids.iter().any(|i| i.eq_ignore_ascii_case(&want)));
    if ok {
        add_ref_obj(c, t);
        out(c, pp, t);
        return ret(c, D3D_OK);
    }
    c.p.log(format!("d3d9: {k}::QueryInterface({want}) not supported"));
    out(c, pp, 0);
    ret(c, E_NOINTERFACE)
}

fn add_ref(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let n = match c.p.d3d_objects.get_mut(&t) {
        Some(o) => {
            o.refcount += 1;
            o.refcount
        }
        None => 1,
    };
    ret(c, n)
}

fn release(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let n = release_obj(c, t);
    ret(c, n)
}

/// The device's reference moves from `old` to `new`: Direct3D holds a reference to
/// every object bound to it (render targets, depth surface, textures, streams, index
/// buffer, shaders, declaration), so an application may release its own.
fn rebind(c: &mut Ctx, old: u32, new: u32) {
    if new != 0 {
        add_ref_obj(c, new);
    }
    if old != 0 {
        release_obj(c, old);
    }
}

/// Drops a reference, destroying the object at zero; returns the count left.
fn release_obj(c: &mut Ctx, t: u32) -> u32 {
    let n = match c.p.d3d_objects.get_mut(&t) {
        Some(o) => {
            o.refcount = o.refcount.saturating_sub(1);
            o.refcount
        }
        None => 0,
    };
    if n == 0 {
        if let Some(o) = c.p.d3d_objects.remove(&t) {
            if o.data.get("gpu").copied().unwrap_or(0) != 0 {
                c.p.gpu.texture_delete(t);
            }
            if o.data.get("shader").copied().unwrap_or(0) != 0 {
                c.p.gpu.shader_delete(t);
            }
            let owned = o.data.get("owned").copied().unwrap_or(0);
            for level in 0..16u32 {
                if owned & (1 << level) != 0 {
                    if let Some(&b) = o.data.get(level_key(level)) {
                        c.p.vm.release(b);
                    }
                }
            }
            if o.data.get("owned_bits").copied().unwrap_or(0) != 0 {
                if let Some(&b) = o.data.get("bits") {
                    c.p.vm.release(b);
                }
            }
            if let Some(&b) = o.data.get("heap_bits") {
                c.p.heap_free(b);
            }
            c.p.heap_free(t);
            c.trace(format!(": freed {}", o.kind));
        }
    }
    n
}

// ---- IDirect3D9(Ex) ------------------------------------------------------------------

fn write_display_mode(c: &mut Ctx, p: u32, ex: bool) {
    let vals: &[u32] = if ex { &[24, 1920, 1080, 60, D3DFMT_X8R8G8B8, 1] } else { &[1920, 1080, 60, D3DFMT_X8R8G8B8] };
    for (i, v) in vals.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
}

fn d3d_get_adapter_identifier(c: &mut Ctx) -> ApiResult {
    let p = c.arg(3);
    let _ = c.host.fill(p, 1100, 0);
    let _ = c.host.write_cstr(p, b"gmrecomp.dll");
    let _ = c.host.write_cstr(p + 512, b"gmrecomp WebGL2 adapter");
    let _ = c.host.write_cstr(p + 1024, b"\\\\.\\DISPLAY1");
    let _ = c.host.write_u32(p + 1056, 0x0001_0000); // DriverVersion
    let _ = c.host.write_u32(p + 1060, 0x0008_0000);
    ret(c, D3D_OK)
}

/// D3DCAPS9: a shader model 3 device.
fn write_caps(c: &mut Ctx, p: u32) {
    let f = |x: f32| x.to_bits();
    let _ = c.host.fill(p, 304, 0);
    let words: &[(u32, u32)] = &[
        (0, 1),             // DeviceType HAL
        (4, 0),             // AdapterOrdinal
        (8, 0x0002_0000),   // Caps READ_SCANLINE
        (12, 0xE002_0000),  // Caps2: CANRENDERWINDOWED | FULLSCREENGAMMA | DYNAMICTEXTURES | CANAUTOGENMIPMAP
        (16, 0x3A0),        // Caps3
        (20, 0x8000_000F),  // PresentationIntervals
        (24, 1),            // CursorCaps
        (28, 0x001B_EFF0),  // DevCaps
        (32, 0x000E_CEF2),  // PrimitiveMiscCaps
        (36, 0x0733_2191),  // RasterCaps
        (40, 0xFF),         // ZCmpCaps
        (44, 0x3FFF),       // SrcBlendCaps
        (48, 0x3FFF),       // DestBlendCaps
        (52, 0xFF),         // AlphaCmpCaps
        (56, 0x0008_4208),  // ShadeCaps
        (60, 0x0001_EC45),  // TextureCaps: no POW2 restriction, mipmaps, cube, alpha
        (64, 0x0303_0700),  // TextureFilterCaps
        (68, 0x0303_0700),  // CubeTextureFilterCaps
        (72, 0x0303_0700),  // VolumeTextureFilterCaps
        (76, 0x3F),         // TextureAddressCaps
        (80, 0x3F),         // VolumeTextureAddressCaps
        (84, 0x1F),         // LineCaps
        (88, 8192),         // MaxTextureWidth
        (92, 8192),         // MaxTextureHeight
        (96, 2048),         // MaxVolumeExtent
        (100, 8192),        // MaxTextureRepeat
        (104, 8192),        // MaxTextureAspectRatio
        (108, 16),          // MaxAnisotropy
        (112, f(1e10)),     // MaxVertexW
        (116, f(-32768.0)), // GuardBandLeft
        (120, f(-32768.0)), // GuardBandTop
        (124, f(32767.0)),  // GuardBandRight
        (128, f(32767.0)),  // GuardBandBottom
        (132, f(0.0)),      // ExtentsAdjust
        (136, 0x1FF),       // StencilCaps
        (140, 0x0018_0008), // FVFCaps
        (144, 0x03FF_FFFF), // TextureOpCaps
        (148, 8),           // MaxTextureBlendStages
        (152, 8),           // MaxSimultaneousTextures
        (156, 0x17B),       // VertexProcessingCaps
        (160, 8),           // MaxActiveLights
        (164, 6),           // MaxUserClipPlanes
        (168, 4),           // MaxVertexBlendMatrices
        (172, 0),           // MaxVertexBlendMatrixIndex
        (176, f(256.0)),    // MaxPointSize
        (180, 0x00FF_FFFF), // MaxPrimitiveCount
        (184, 0x00FF_FFFF), // MaxVertexIndex
        (188, 16),          // MaxStreams
        (192, 508),         // MaxStreamStride
        (196, 0xFFFE_0300), // VertexShaderVersion 3.0
        (200, 256),         // MaxVertexShaderConst
        (204, 0xFFFF_0300), // PixelShaderVersion 3.0
        (208, f(f32::MAX)), // PixelShader1xMaxValue
        (212, 0x3),         // DevCaps2
        (216, f(0.0)),      // MaxNpatchTessellationLevel
        (220, 0),           // Reserved5
        (224, 0),           // MasterAdapterOrdinal
        (228, 0),           // AdapterOrdinalInGroup
        (232, 1),           // NumberOfAdaptersInGroup
        (236, 0x30F),       // DeclTypes
        (240, 4),           // NumSimultaneousRTs
        (244, 0x0300_0300), // StretchRectFilterCaps
        (248, 1),           // VS20Caps.Caps (predication)
        (252, 24),          // VS20Caps.DynamicFlowControlDepth
        (256, 32),          // VS20Caps.NumTemps
        (260, 4),           // VS20Caps.StaticFlowControlDepth
        (264, 0x1F),        // PS20Caps.Caps
        (268, 24),          // PS20Caps.DynamicFlowControlDepth
        (272, 32),          // PS20Caps.NumTemps
        (276, 4),           // PS20Caps.StaticFlowControlDepth
        (280, 512),         // PS20Caps.NumInstructionSlots
        (284, 0x0300_0300), // VertexTextureFilterCaps
        (288, 32768),       // MaxVShaderInstructionsExecuted
        (292, 32768),       // MaxPShaderInstructionsExecuted
        (296, 4096),        // MaxVertexShader30InstructionSlots
        (300, 4096),        // MaxPixelShader30InstructionSlots
    ];
    for &(off, v) in words {
        let _ = c.host.write_u32(p + off, v);
    }
}

fn d3d_create_device(c: &mut Ctx) -> ApiResult {
    let ex = c.name().contains("CreateDeviceEx");
    let (hwnd, flags, pp) = (c.arg(3), c.arg(4), c.arg(5));
    let ppdev = if ex { c.arg(7) } else { c.arg(6) };
    let rd = |c: &Ctx, off: u32| c.host.read_u32(pp + off).unwrap_or(0);
    let (mut w, mut h, mut fmt) = (rd(c, 0), rd(c, 4), rd(c, 8));
    let (dev_hwnd, windowed, auto_depth, depth_fmt) = (rd(c, 28), rd(c, 32), rd(c, 36), rd(c, 40));
    let target_win = if dev_hwnd != 0 { dev_hwnd } else { hwnd };
    if w == 0 || h == 0 {
        if let Some(win) = c.p.windows.get(&target_win) {
            w = win.w.max(1) as u32;
            h = win.h.max(1) as u32;
        } else {
            (w, h) = (800, 600);
        }
        let _ = c.host.write_u32(pp, w);
        let _ = c.host.write_u32(pp + 4, h);
    }
    if fmt == 0 {
        fmt = D3DFMT_X8R8G8B8;
        let _ = c.host.write_u32(pp + 8, fmt);
    }
    c.trace(format!("({w}x{h} fmt {fmt} windowed {windowed} flags {flags:#x} hwnd {target_win:#x} depth {auto_depth}/{depth_fmt})"));
    add_ref_obj(c, this(c));
    let data = HashMap::from_iter([("w", w), ("h", h), ("fmt", fmt), ("hwnd", target_win), ("flags", flags), ("d3d", this(c)), ("depth_fmt", depth_fmt)]);
    let dev = new_object(c, "IDirect3DDevice9Ex", IDIRECT3DDEVICE9, data);
    reset_state(c, dev, w, h, auto_depth != 0);
    c.p.d3d.window = target_win;
    out(c, ppdev, dev);
    ret(c, D3D_OK)
}

fn reset_state(c: &mut Ctx, dev: u32, w: u32, h: u32, auto_depth: bool) {
    let mut st = State9 { device: dev, ..Default::default() };
    st.rs[gpu::rs::ZENABLE] = auto_depth as u32;
    st.viewport = [0.0, 0.0, w as f32, h as f32, 0.0, 1.0];
    st.scissor = [0, 0, w as i32, h as i32];
    st.target = (0, w, h);
    st.backbuffer = (w, h);
    c.p.d3d9 = Box::new(st);
    c.p.d3d.backbuffer = (w, h);
    c.p.gpu.resize(w, h);
    c.p.gpu.set_target(0, w, h);
}

const IDIRECT3D9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("RegisterSoftwareDevice", 2, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("GetAdapterCount", 1, |c| ret(c, 1)),
    ("GetAdapterIdentifier", 4, d3d_get_adapter_identifier),
    ("GetAdapterModeCount", 3, |c| ret(c, 1)),
    ("EnumAdapterModes", 5, |c| {
        let p = c.arg(4);
        write_display_mode(c, p, false);
        ret(c, D3D_OK)
    }),
    ("GetAdapterDisplayMode", 3, |c| {
        let p = c.arg(2);
        write_display_mode(c, p, false);
        ret(c, D3D_OK)
    }),
    ("CheckDeviceType", 6, okn),
    ("CheckDeviceFormat", 7, |c| {
        // No compressed or exotic formats; the usual color, depth and float ones.
        let fmt = c.arg(6);
        let ok = matches!(fmt, 20..=41 | 50..=52 | 70..=80 | 111..=116);
        ret(c, if ok { D3D_OK } else { D3DERR_NOTAVAILABLE })
    }),
    ("CheckDeviceMultiSampleType", 7, |c| {
        let (ms, quality) = (c.arg(5), c.arg(6));
        out(c, quality, 1);
        ret(c, if ms <= 1 { D3D_OK } else { D3DERR_NOTAVAILABLE })
    }),
    ("CheckDepthStencilMatch", 6, okn),
    ("CheckDeviceFormatConversion", 5, okn),
    ("GetDeviceCaps", 4, |c| {
        let p = c.arg(3);
        write_caps(c, p);
        ret(c, D3D_OK)
    }),
    ("GetAdapterMonitor", 2, |c| ret(c, 0x0001_0001)),
    ("CreateDevice", 7, d3d_create_device),
    ("GetAdapterModeCountEx", 3, |c| ret(c, 1)),
    ("EnumAdapterModesEx", 5, |c| {
        let p = c.arg(4);
        write_display_mode(c, p, true);
        ret(c, D3D_OK)
    }),
    ("GetAdapterDisplayModeEx", 4, |c| {
        let (p, rot) = (c.arg(2), c.arg(3));
        if p != 0 {
            write_display_mode(c, p, true);
        }
        out(c, rot, 1);
        ret(c, D3D_OK)
    }),
    ("CreateDeviceEx", 8, d3d_create_device),
    ("GetAdapterLUID", 3, |c| {
        let p = c.arg(2);
        let _ = c.host.write_u64(p, 0x1234);
        ret(c, D3D_OK)
    }),
];

// ---- IDirect3DDevice9(Ex) ---------------------------------------------------------------

fn dev_present(c: &mut Ctx) -> ApiResult {
    c.p.frames_presented += 1;
    let n = c.p.frames_presented;
    let draws = c.p.draw_calls;
    c.p.draw_calls = 0;
    c.p.log(format!("Present #{n} ({draws} draw calls, t={}ms, {} µs)", c.p.now_ms(), c.p.clock_us));
    let (src, dst, hwnd) = (c.arg(1), c.arg(2), c.arg(3));
    let (bw, bh) = c.p.d3d9.backbuffer;
    let rect = |c: &Ctx, p: u32| {
        let g = |k: u32| c.host.read_u32(p + 4 * k).unwrap_or(0) as i32;
        [g(0), g(1), g(2) - g(0), g(3) - g(1)]
    };
    let src_rect = if src != 0 { rect(c, src) } else { [0, 0, bw as i32, bh as i32] };
    let dst_size = if dst != 0 {
        let r = rect(c, dst);
        (r[2], r[3])
    } else {
        let win = if hwnd != 0 { hwnd } else { field(c, c.p.d3d9.device, "hwnd") };
        match c.p.windows.get(&win) {
            Some(w) if src == 0 => (w.w.max(1), w.h.max(1)),
            _ => (src_rect[2], src_rect[3]),
        }
    };
    super::d3d8::present_frame(c, src_rect, dst_size.0.max(1) as u32, dst_size.1.max(1) as u32)?;
    ret(c, D3D_OK)
}

fn dev_reset(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let pp = c.arg(1);
    let (mut w, mut h) = (c.host.read_u32(pp).unwrap_or(0), c.host.read_u32(pp + 4).unwrap_or(0));
    if w == 0 || h == 0 {
        (w, h) = (field(c, t, "w"), field(c, t, "h"));
    }
    let auto_depth = c.host.read_u32(pp + 36).unwrap_or(0) != 0;
    set_field(c, t, "w", w);
    set_field(c, t, "h", h);
    reset_state(c, t, w, h, auto_depth);
    c.trace(format!("({w}x{h})"));
    ret(c, D3D_OK)
}

fn new_surface(c: &mut Ctx, w: u32, h: u32, fmt: u32, usage: u32, pool: u32) -> u32 {
    let data = HashMap::from_iter([("w", w), ("h", h), ("fmt", fmt), ("usage", usage), ("pool", pool), ("type", 1)]);
    new_object(c, "IDirect3DSurface9", IDIRECT3DSURFACE9, data)
}

/// The back buffer surface (one object, handed out with a reference each time).
fn back_buffer(c: &mut Ctx) -> u32 {
    let dev = c.p.d3d9.device;
    let mut s = field(c, dev, "backbuffer");
    if s == 0 || !c.p.d3d_objects.contains_key(&s) || field(c, s, "backbuffer") == 0 {
        let (w, h) = c.p.d3d9.backbuffer;
        let fmt = field(c, dev, "fmt");
        s = new_surface(c, w, h, fmt, D3DUSAGE_RENDERTARGET, 0);
        set_field(c, s, "backbuffer", 1);
        set_field(c, dev, "backbuffer", s);
        // the device's own reference: the swap chain owns its back buffer
        add_ref_obj(c, s);
    } else {
        add_ref_obj(c, s);
    }
    s
}

/// Target id the host knows a surface by: 0 for the back buffer, the texture for a
/// texture level, else the surface itself (given host storage on first use).
fn target_id(c: &mut Ctx, surf: u32) -> u32 {
    if surf == 0 || field(c, surf, "backbuffer") != 0 {
        return 0;
    }
    let owner = field(c, surf, "owner");
    let id = if owner != 0 { owner } else { surf };
    if field(c, id, "gpu") == 0 {
        let (w, h, fmt) = (field(c, id, "w"), field(c, id, "h"), field(c, id, "fmt"));
        c.p.gpu.texture_create(id, w, h, 1, fmt, true);
        set_field(c, id, "gpu", 1);
    }
    id
}

fn dev_set_render_target(c: &mut Ctx) -> ApiResult {
    let (index, surf) = (c.arg(1), c.arg(2));
    if index != 0 {
        // Extra render targets are not supported; drawing goes to target 0 only.
        return ret(c, if surf == 0 { D3D_OK } else { D3DERR_INVALIDCALL });
    }
    if surf == 0 {
        return ret(c, D3DERR_INVALIDCALL);
    }
    let (w, h) = (field(c, surf, "w"), field(c, surf, "h"));
    let id = target_id(c, surf);
    let old = c.p.d3d9.rt;
    rebind(c, old, surf);
    c.p.d3d9.rt = surf;
    let st = &mut c.p.d3d9;
    st.target = (id, w, h);
    st.viewport = [0.0, 0.0, w as f32, h as f32, 0.0, 1.0];
    st.scissor = [0, 0, w as i32, h as i32];
    c.p.d3d.target = (id, w, h);
    c.p.gpu.set_target(id, w, h);
    c.trace(format!("({surf:#x}) -> target {id:#x} {w}x{h}"));
    ret(c, D3D_OK)
}

fn dev_get_render_target(c: &mut Ctx) -> ApiResult {
    let (index, pp) = (c.arg(1), c.arg(2));
    if index != 0 {
        out(c, pp, 0);
        return ret(c, D3DERR_NOTFOUND);
    }
    let (target, tw, th) = c.p.d3d9.target;
    let rt = c.p.d3d9.rt;
    let s = if rt != 0 && c.p.d3d_objects.contains_key(&rt) {
        add_ref_obj(c, rt);
        rt
    } else if target == 0 {
        back_buffer(c)
    } else if kind(c, target) == "IDirect3DSurface9" {
        add_ref_obj(c, target);
        target
    } else {
        let fmt = field(c, target, "fmt");
        let s = new_surface(c, tw, th, fmt, D3DUSAGE_RENDERTARGET, 0);
        set_field(c, s, "owner", target);
        s
    };
    out(c, pp, s);
    ret(c, D3D_OK)
}

fn dev_create_texture(c: &mut Ctx) -> ApiResult {
    let (w, h, mut levels, usage, fmt, pool, pp) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5), c.arg(6), c.arg(7));
    if w == 0 || h == 0 {
        return ret(c, D3DERR_INVALIDCALL);
    }
    if levels == 0 {
        levels = if usage & 0x400 != 0 { 1 } else { 32 - w.max(h).leading_zeros() }; // AUTOGENMIPMAP: one level
    }
    let data = HashMap::from_iter([("w", w), ("h", h), ("levels", levels), ("usage", usage), ("fmt", fmt), ("pool", pool), ("type", 3)]);
    let tex = new_object(c, "IDirect3DTexture9", IDIRECT3DTEXTURE9, data);
    c.p.textures_created += 1;
    c.p.gpu.texture_create(tex, w, h, levels, fmt, usage & D3DUSAGE_RENDERTARGET != 0);
    set_field(c, tex, "gpu", 1);
    c.trace(format!("({w}x{h} levels {levels} fmt {fmt} usage {usage:#x} pool {pool}) = {tex:#x}"));
    out(c, pp, tex);
    ret(c, D3D_OK)
}

fn dev_create_buffer(c: &mut Ctx, index: bool) -> ApiResult {
    let (len, usage, fvf_or_fmt, pool, pp) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5));
    let bits = alloc_storage(c, len.max(4));
    let data = HashMap::from_iter([("len", len), ("usage", usage), ("fvf", fvf_or_fmt), ("pool", pool), ("bits", bits), ("owned_bits", 1)]);
    let obj = if index {
        new_object(c, "IDirect3DIndexBuffer9", IDIRECT3DINDEXBUFFER9, data)
    } else {
        new_object(c, "IDirect3DVertexBuffer9", IDIRECT3DVERTEXBUFFER9, data)
    };
    c.trace(format!("({len} bytes usage {usage:#x} fvf/fmt {fvf_or_fmt:#x} pool {pool}) = {obj:#x}"));
    out(c, pp, obj);
    ret(c, D3D_OK)
}

fn dev_create_surface(c: &mut Ctx, usage: u32, pp_arg: u32) -> ApiResult {
    let (w, h, fmt) = (c.arg(1), c.arg(2), c.arg(3));
    let pool = if usage == 0 { c.arg(4) } else { 0 };
    let s = new_surface(c, w, h, fmt, usage, pool);
    out(c, c.arg(pp_arg), s);
    ret(c, D3D_OK)
}

// Transforms and states.

fn read_matrix(c: &Ctx, p: u32) -> [f32; 16] {
    let mut m = [0f32; 16];
    for (i, v) in m.iter_mut().enumerate() {
        *v = f32::from_bits(c.host.read_u32(p + 4 * i as u32).unwrap_or(0));
    }
    m
}

fn mat_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            r[i * 4 + j] = (0..4).map(|k| a[i * 4 + k] * b[k * 4 + j]).sum();
        }
    }
    r
}

fn transform(st: &mut State9, ty: u32) -> Option<&mut [f32; 16]> {
    match ty {
        2 => Some(&mut st.view),
        3 => Some(&mut st.proj),
        256 => Some(&mut st.world),
        _ => None,
    }
}

fn dev_set_transform(c: &mut Ctx) -> ApiResult {
    let (ty, p) = (c.arg(1), c.arg(2));
    let m = read_matrix(c, p);
    if let Some(slot) = transform(&mut c.p.d3d9, ty) {
        *slot = m;
    }
    ret(c, D3D_OK)
}

fn dev_get_transform(c: &mut Ctx) -> ApiResult {
    let (ty, p) = (c.arg(1), c.arg(2));
    let m = transform(&mut c.p.d3d9, ty).map(|m| *m).unwrap_or(IDENTITY);
    for (i, v) in m.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, v.to_bits());
    }
    ret(c, D3D_OK)
}

fn dev_multiply_transform(c: &mut Ctx) -> ApiResult {
    let (ty, p) = (c.arg(1), c.arg(2));
    let m = read_matrix(c, p);
    if let Some(slot) = transform(&mut c.p.d3d9, ty) {
        *slot = mat_mul(&m, slot);
    }
    ret(c, D3D_OK)
}

fn dev_set_viewport(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let g = |k: u32| c.host.read_u32(p + 4 * k).unwrap_or(0);
    c.p.d3d9.viewport = [g(0) as f32, g(1) as f32, g(2) as f32, g(3) as f32, f32::from_bits(g(4)), f32::from_bits(g(5))];
    ret(c, D3D_OK)
}

fn dev_get_viewport(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let v = c.p.d3d9.viewport;
    for (i, x) in [v[0] as u32, v[1] as u32, v[2] as u32, v[3] as u32, v[4].to_bits(), v[5].to_bits()].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *x);
    }
    ret(c, D3D_OK)
}

fn dev_set_render_state(c: &mut Ctx) -> ApiResult {
    let (k, v) = (c.arg(1) as usize, c.arg(2));
    if k < 256 {
        c.p.d3d9.rs[k] = v;
    }
    ret(c, D3D_OK)
}

fn dev_get_render_state(c: &mut Ctx) -> ApiResult {
    let (k, p) = (c.arg(1) as usize, c.arg(2));
    let v = if k < 256 { c.p.d3d9.rs[k] } else { 0 };
    out(c, p, v);
    ret(c, D3D_OK)
}

fn dev_set_texture(c: &mut Ctx) -> ApiResult {
    let (stage, tex) = (c.arg(1) as usize, c.arg(2));
    if stage < 16 {
        let old = c.p.d3d9.textures[stage];
        rebind(c, old, tex);
        c.p.d3d9.textures[stage] = tex;
    }
    ret(c, D3D_OK)
}

fn dev_get_texture(c: &mut Ctx) -> ApiResult {
    let (stage, p) = (c.arg(1) as usize, c.arg(2));
    let t = if stage < 16 { c.p.d3d9.textures[stage] } else { 0 };
    add_ref_obj(c, t);
    out(c, p, t);
    ret(c, D3D_OK)
}

fn dev_set_tss(c: &mut Ctx) -> ApiResult {
    let (stage, k, v) = (c.arg(1) as usize, c.arg(2) as usize, c.arg(3));
    if stage < 8 && k < gpu::TSS_COUNT {
        c.p.d3d9.tss[stage][k] = v;
    }
    ret(c, D3D_OK)
}

fn dev_get_tss(c: &mut Ctx) -> ApiResult {
    let (stage, k, p) = (c.arg(1) as usize, c.arg(2) as usize, c.arg(3));
    let v = if stage < 8 && k < gpu::TSS_COUNT { c.p.d3d9.tss[stage][k] } else { 0 };
    out(c, p, v);
    ret(c, D3D_OK)
}

fn dev_set_sampler_state(c: &mut Ctx) -> ApiResult {
    let (s, k, v) = (c.arg(1) as usize, c.arg(2) as usize, c.arg(3));
    if s < 16 && k < 14 {
        c.p.d3d9.samp[s][k] = v;
    }
    ret(c, D3D_OK)
}

fn dev_get_sampler_state(c: &mut Ctx) -> ApiResult {
    let (s, k, p) = (c.arg(1) as usize, c.arg(2) as usize, c.arg(3));
    let v = if s < 16 && k < 14 { c.p.d3d9.samp[s][k] } else { 0 };
    out(c, p, v);
    ret(c, D3D_OK)
}

fn dev_clear(c: &mut Ctx) -> ApiResult {
    let (count, rects, flags, color, z, stencil) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), f32::from_bits(c.arg(5)), c.arg(6));
    let vp = c.p.d3d9.viewport;
    let mut area = [vp[0] as i32, vp[1] as i32, vp[2] as i32, vp[3] as i32];
    if c.p.d3d9.rs[174] != 0 {
        // SCISSORTESTENABLE: clears are clipped to the scissor rectangle too
        let s = c.p.d3d9.scissor;
        let (x1, y1) = (area[0].max(s[0]), area[1].max(s[1]));
        let (x2, y2) = ((area[0] + area[2]).min(s[0] + s[2]), (area[1] + area[3]).min(s[1] + s[3]));
        area = [x1, y1, (x2 - x1).max(0), (y2 - y1).max(0)];
    }
    if count == 0 || rects == 0 {
        c.p.gpu.clear(flags, color, z, stencil, area);
    } else {
        for i in 0..count.min(256) {
            let g = |k: u32| c.host.read_u32(rects + 16 * i + 4 * k).unwrap_or(0) as i32;
            let (x1, y1, x2, y2) = (g(0).max(area[0]), g(1).max(area[1]), g(2).min(area[0] + area[2]), g(3).min(area[1] + area[3]));
            if x2 > x1 && y2 > y1 {
                c.p.gpu.clear(flags, color, z, stencil, [x1, y1, x2 - x1, y2 - y1]);
            }
        }
    }
    ret(c, D3D_OK)
}

// Shaders and declarations.

/// Reads a shader's token stream (up to and including the END token).
fn read_tokens(c: &Ctx, p: u32) -> Vec<u32> {
    let mut v = Vec::new();
    for i in 0..65536u32 {
        let t = c.host.read_u32(p + 4 * i).unwrap_or(0x0000_FFFF);
        v.push(t);
        if t == 0x0000_FFFF {
            break;
        }
    }
    v
}

fn dev_create_shader(c: &mut Ctx, vertex: bool) -> ApiResult {
    let (func, pp) = (c.arg(1), c.arg(2));
    let tokens = read_tokens(c, func);
    let iface = if vertex { "IDirect3DVertexShader9" } else { "IDirect3DPixelShader9" };
    let methods = if vertex { IDIRECT3DVERTEXSHADER9 } else { IDIRECT3DPIXELSHADER9 };
    let copy = alloc(c, tokens.len() as u32 * 4);
    for (i, t) in tokens.iter().enumerate() {
        let _ = c.host.write_u32(copy + 4 * i as u32, *t);
    }
    let obj = new_object(c, iface, methods, HashMap::from_iter([("heap_bits", copy), ("tokens", tokens.len() as u32)]));
    match super::d3d9_shader::translate(&tokens) {
        Ok(glsl) => {
            c.p.gpu.shader_create(obj, vertex, &glsl);
            set_field(c, obj, "shader", 1);
            if c.p.trace {
                c.p.log(format!("    {iface} {obj:#x}:\n{glsl}"));
            }
        }
        Err(e) => c.p.log(format!("d3d9: {iface} {obj:#x} not translated: {e}")),
    }
    out(c, pp, obj);
    ret(c, D3D_OK)
}

fn shader_get_function(c: &mut Ctx) -> ApiResult {
    let (t, data, size_p) = (this(c), c.arg(1), c.arg(2));
    let n = field(c, t, "tokens") * 4;
    if data == 0 {
        out(c, size_p, n);
        return ret(c, D3D_OK);
    }
    let src = field(c, t, "heap_bits");
    let mut b = vec![0u8; n as usize];
    let _ = c.host.read(src, &mut b);
    let _ = c.host.write(data, &b);
    out(c, size_p, n);
    ret(c, D3D_OK)
}

/// Reads D3DVERTEXELEMENT9s up to D3DDECL_END.
fn read_elements(c: &Ctx, p: u32) -> Vec<[u32; 4]> {
    let mut v = Vec::new();
    for i in 0..64u32 {
        let a = p + 8 * i;
        let stream = c.host.read_u16(a).unwrap_or(0xFF) as u32;
        if stream == 0xFF {
            break;
        }
        let offset = c.host.read_u16(a + 2).unwrap_or(0) as u32;
        let mut b = [0u8; 4];
        let _ = c.host.read(a + 4, &mut b);
        // type, method, usage, usage index
        v.push([stream, offset, b[0] as u32, b[2] as u32 | (b[3] as u32) << 8]);
    }
    v
}

fn new_declaration(c: &mut Ctx, elements: &[[u32; 4]]) -> u32 {
    let copy = alloc(c, (elements.len() as u32 + 1) * 16);
    for (i, e) in elements.iter().enumerate() {
        for k in 0..4 {
            let _ = c.host.write_u32(copy + 16 * i as u32 + 4 * k, e[k as usize]);
        }
    }
    new_object(c, "IDirect3DVertexDeclaration9", IDIRECT3DVERTEXDECLARATION9, HashMap::from_iter([("heap_bits", copy), ("count", elements.len() as u32)]))
}

fn declaration(c: &Ctx, decl: u32) -> Vec<[u32; 4]> {
    let (p, n) = (field(c, decl, "heap_bits"), field(c, decl, "count"));
    (0..n.min(16))
        .map(|i| {
            let mut e = [0u32; 4];
            for (k, v) in e.iter_mut().enumerate() {
                *v = c.host.read_u32(p + 16 * i + 4 * k as u32).unwrap_or(0);
            }
            e
        })
        .collect()
}

/// The elements an FVF describes (D3DDECLTYPE: FLOAT1..4 = 0..3, D3DCOLOR = 4).
pub fn fvf_elements(fvf: u32) -> Vec<[u32; 4]> {
    let mut v = Vec::new();
    let mut off = 0;
    let mut push = |ty: u32, usage: u32, index: u32, size: u32| {
        v.push([0, off, ty, usage | index << 8]);
        off += size;
    };
    match fvf & 0x400E {
        0x002 => push(2, 0, 0, 12),  // XYZ
        0x004 => push(3, 9, 0, 16),  // XYZRHW -> POSITIONT
        0x006 => push(3, 0, 0, 16),  // XYZB1 (blend weight ignored)
        0x4002 => push(3, 0, 0, 16), // XYZW
        _ => {}
    }
    if fvf & 0x10 != 0 {
        push(2, 3, 0, 12); // NORMAL
    }
    if fvf & 0x20 != 0 {
        push(0, 4, 0, 4); // PSIZE
    }
    if fvf & 0x40 != 0 {
        push(4, 10, 0, 4); // DIFFUSE -> COLOR0
    }
    if fvf & 0x80 != 0 {
        push(4, 10, 1, 4); // SPECULAR -> COLOR1
    }
    let ntex = (fvf >> 8) & 0xF;
    for i in 0..ntex {
        let fmt = (fvf >> (16 + 2 * i)) & 3;
        let (ty, size) = match fmt {
            0 => (1, 8),
            1 => (2, 12),
            2 => (3, 16),
            _ => (0, 4),
        };
        push(ty, 5, i, size);
    }
    v
}

fn set_constants(c: &mut Ctx, which: u8) -> ApiResult {
    let (start, data, count) = (c.arg(1) as usize, c.arg(2), c.arg(3) as usize);
    let rd = |c: &Ctx, i: usize| c.host.read_u32(data + 4 * i as u32).unwrap_or(0);
    match which {
        0 | 1 => {
            let n = if which == 0 { 256 } else { 224 };
            for k in 0..count {
                if start + k >= n {
                    break;
                }
                let v = [0, 1, 2, 3].map(|j| f32::from_bits(rd(c, 4 * k + j)));
                if which == 0 {
                    c.p.d3d9.vs_f[start + k] = v;
                } else {
                    c.p.d3d9.ps_f[start + k] = v;
                }
            }
        }
        2 | 3 => {
            for k in 0..count {
                if start + k >= 16 {
                    break;
                }
                let v = [0, 1, 2, 3].map(|j| rd(c, 4 * k + j) as i32);
                if which == 2 {
                    c.p.d3d9.vs_i[start + k] = v;
                } else {
                    c.p.d3d9.ps_i[start + k] = v;
                }
            }
        }
        _ => {
            for k in 0..count {
                if start + k >= 16 {
                    break;
                }
                let v = rd(c, k);
                if which == 4 {
                    c.p.d3d9.vs_b[start + k] = v;
                } else {
                    c.p.d3d9.ps_b[start + k] = v;
                }
            }
        }
    }
    ret(c, D3D_OK)
}

fn get_constants(c: &mut Ctx, which: u8) -> ApiResult {
    let (start, data, count) = (c.arg(1) as usize, c.arg(2), c.arg(3) as usize);
    for k in 0..count {
        let i = start + k;
        let words: Vec<u32> = match which {
            0 if i < 256 => c.p.d3d9.vs_f[i].iter().map(|f| f.to_bits()).collect(),
            1 if i < 224 => c.p.d3d9.ps_f[i].iter().map(|f| f.to_bits()).collect(),
            2 if i < 16 => c.p.d3d9.vs_i[i].iter().map(|&v| v as u32).collect(),
            3 if i < 16 => c.p.d3d9.ps_i[i].iter().map(|&v| v as u32).collect(),
            4 if i < 16 => vec![c.p.d3d9.vs_b[i]],
            5 if i < 16 => vec![c.p.d3d9.ps_b[i]],
            _ => break,
        };
        let stride = words.len() as u32 * 4;
        for (j, w) in words.iter().enumerate() {
            let _ = c.host.write_u32(data + stride * k as u32 + 4 * j as u32, *w);
        }
    }
    ret(c, D3D_OK)
}

// Drawing.

fn level_dims(w: u32, h: u32, level: u32) -> (u32, u32) {
    ((w >> level).max(1), (h >> level).max(1))
}

fn level_key(level: u32) -> &'static str {
    const KEYS: [&str; 16] = ["bits0", "bits1", "bits2", "bits3", "bits4", "bits5", "bits6", "bits7", "bits8", "bits9", "bits10", "bits11", "bits12", "bits13", "bits14", "bits15"];
    KEYS[(level as usize).min(15)]
}

fn level_pitch(fmt: u32, w: u32) -> u32 {
    match fmt {
        // DXT1 / DXT2-5: rows of 4x4 blocks
        0x3154_5844 => w.div_ceil(4) * 8,
        0x3254_5844..=0x3554_5844 => w.div_ceil(4) * 16,
        _ => w * format_bpp(fmt),
    }
}

fn level_rows(fmt: u32, h: u32) -> u32 {
    if (0x3154_5844..=0x3554_5844).contains(&fmt) { h.div_ceil(4) } else { h }
}

/// Allocates (once) the pixel storage of one mip level and returns (pitch, bits).
fn level_bits(c: &mut Ctx, obj: u32, level: u32) -> (u32, u32) {
    let fmt = field(c, obj, "fmt");
    let (w, h) = level_dims(field(c, obj, "w"), field(c, obj, "h"), level);
    let pitch = level_pitch(fmt, w);
    let key = level_key(level);
    let mut bits = field(c, obj, key);
    if bits == 0 {
        bits = alloc_storage(c, (pitch * level_rows(fmt, h)).max(4));
        set_field(c, obj, key, bits);
        let owned = field(c, obj, "owned") | (1 << level.min(15));
        set_field(c, obj, "owned", owned);
    }
    (pitch, bits)
}

fn mark_dirty(c: &mut Ctx, tex: u32, level: u32) {
    let d = field(c, tex, "dirty") | (1 << level.min(15));
    set_field(c, tex, "dirty", d);
}

/// Uploads the levels of `tex` the guest changed since the last draw that used it.
fn sync_texture(c: &mut Ctx, tex: u32) {
    let dirty = field(c, tex, "dirty");
    if dirty == 0 {
        return;
    }
    set_field(c, tex, "dirty", 0);
    let (w, h, fmt) = (field(c, tex, "w"), field(c, tex, "h"), field(c, tex, "fmt"));
    for level in 0..16 {
        if dirty & (1 << level) == 0 {
            continue;
        }
        let bits = field(c, tex, level_key(level));
        if bits == 0 {
            continue;
        }
        let (lw, lh) = level_dims(w, h, level);
        c.p.gpu.texture_upload(tex, level, lw, lh, fmt, level_pitch(fmt, lw), bits);
    }
}

fn vertex_count(prim: u32, n: u32) -> u32 {
    match prim {
        1 => n,         // point list
        2 => 2 * n,     // line list
        3 => n + 1,     // line strip
        4 => 3 * n,     // triangle list
        5 | 6 => n + 2, // strip, fan
        _ => 0,
    }
}

/// Builds the host's draw record from the device state. `base` is added to every
/// stream's address (start vertex or base vertex index, in vertices).
fn submit(c: &mut Ctx, prim: u32, prim_count: u32, base: i32, min_index: u32, vertex_count: u32, indices: u32, index_size: u32, up: Option<(u32, u32)>) {
    c.p.draw_calls += 1;
    if prim_count == 0 {
        return;
    }
    let st = &c.p.d3d9;
    let elements = if st.decl != 0 { declaration(c, st.decl) } else { fvf_elements(st.fvf) };
    let mut streams = [[0u32; 2]; 4];
    for (i, s) in streams.iter_mut().enumerate() {
        let (vb, off, stride, _) = c.p.d3d9.streams[i];
        let (addr, stride) = match up {
            Some((p, stride)) if i == 0 => (p, stride),
            Some(_) => (0, 0),
            None if vb != 0 => (field(c, vb, "bits") + off, stride),
            None => (0, 0),
        };
        *s = [addr.wrapping_add((base as u32).wrapping_mul(stride)), stride];
    }
    if streams[0][0] == 0 {
        return;
    }
    let mut textures = [[0u32; 3]; 16];
    for s in 0..16 {
        let t = c.p.d3d9.textures[s];
        if t != 0 && c.p.d3d_objects.contains_key(&t) {
            sync_texture(c, t);
            textures[s] = [t, field(c, t, "fmt"), (field(c, t, "usage") & D3DUSAGE_RENDERTARGET != 0) as u32];
        }
    }
    let st = &c.p.d3d9;
    if st.vs == 0 && st.ps == 0 && st.decl == 0 && st.fvf != 0 {
        // Fixed function with an FVF: the Direct3D 8 renderer handles it.
        let d = gpu::DrawCall {
            prim,
            prim_count,
            vertex_count,
            fvf: st.fvf,
            stride: streams[0][1],
            vertices: streams[0][0],
            indices,
            index_size,
            min_index,
            texture: [textures[0][0], textures[1][0]],
            tex_format: [textures[0][1], textures[1][1]],
            tex_flip: [textures[0][2], textures[1][2]],
            target_w: st.target.1,
            target_h: st.target.2,
            viewport: st.viewport,
            world: st.world,
            view: st.view,
            proj: st.proj,
            rs: st.rs,
            tss: [ff_stage(st, 0), ff_stage(st, 1)],
        };
        c.p.gpu.draw(&d);
        return;
    }
    let mut el = [[0u32; 4]; 16];
    for (i, e) in elements.iter().take(16).enumerate() {
        el[i] = *e;
    }
    let d = DrawCall9 {
        prim,
        prim_count,
        vertex_count,
        min_index,
        indices,
        index_size,
        streams,
        elements: el,
        element_count: elements.len().min(16) as u32,
        vs: st.vs,
        ps: st.ps,
        textures,
        samplers: st.samp,
        target_w: st.target.1,
        target_h: st.target.2,
        viewport: st.viewport,
        scissor: st.scissor,
        rs: st.rs,
        vs_f: *st.vs_f,
        ps_f: *st.ps_f,
        vs_i: st.vs_i,
        ps_i: st.ps_i,
        vs_b: st.vs_b,
        ps_b: st.ps_b,
    };
    if c.p.trace {
        c.p.log(format!("    draw9 prim {prim} x{prim_count} vs {:#x} ps {:#x} tex {:#x} elements {}", d.vs, d.ps, textures[0][0], d.element_count));
    }
    c.p.gpu.draw9(&d);
}

/// A D3D9 texture stage with its sampler's states in the Direct3D 8 slots.
fn ff_stage(st: &State9, i: usize) -> [u32; gpu::TSS_COUNT] {
    let mut s = st.tss[i];
    s[gpu::tss::ADDRESSU] = st.samp[i][1];
    s[gpu::tss::ADDRESSV] = st.samp[i][2];
    s[gpu::tss::ADDRESSW] = st.samp[i][3];
    s[gpu::tss::BORDERCOLOR] = st.samp[i][4];
    s[gpu::tss::MAGFILTER] = st.samp[i][5];
    s[gpu::tss::MINFILTER] = st.samp[i][6];
    s[gpu::tss::MIPFILTER] = st.samp[i][7];
    s
}

fn dev_draw_primitive(c: &mut Ctx) -> ApiResult {
    let (prim, start, count) = (c.arg(1), c.arg(2), c.arg(3));
    let n = vertex_count(prim, count);
    submit(c, prim, count, start as i32, 0, n, 0, 0, None);
    ret(c, D3D_OK)
}

fn dev_draw_indexed_primitive(c: &mut Ctx) -> ApiResult {
    let (prim, base, min, num, start_index, count) = (c.arg(1), c.arg(2) as i32, c.arg(3), c.arg(4), c.arg(5), c.arg(6));
    let ib = c.p.d3d9.indices;
    if ib == 0 {
        return ret(c, D3DERR_INVALIDCALL);
    }
    let size = if field(c, ib, "fvf") == 102 { 4 } else { 2 }; // D3DFMT_INDEX32
    let indices = field(c, ib, "bits") + start_index * size;
    submit(c, prim, count, base, min, num, indices, size, None);
    ret(c, D3D_OK)
}

fn dev_draw_primitive_up(c: &mut Ctx) -> ApiResult {
    let (prim, count, data, stride) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
    let n = vertex_count(prim, count);
    submit(c, prim, count, 0, 0, n, 0, 0, Some((data, stride)));
    ret(c, D3D_OK)
}

fn dev_draw_indexed_primitive_up(c: &mut Ctx) -> ApiResult {
    let (prim, min, num, count, idx, fmt, data, stride) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5), c.arg(6), c.arg(7), c.arg(8));
    let size = if fmt == 102 { 4 } else { 2 };
    submit(c, prim, count, 0, min, num, idx, size, Some((data, stride)));
    ret(c, D3D_OK)
}

fn rect_at(c: &Ctx, p: u32, w: u32, h: u32) -> [i32; 4] {
    if p == 0 {
        return [0, 0, w as i32, h as i32];
    }
    let g = |k: u32| c.host.read_u32(p + 4 * k).unwrap_or(0) as i32;
    [g(0), g(1), g(2) - g(0), g(3) - g(1)]
}

fn dev_stretch_rect(c: &mut Ctx) -> ApiResult {
    let (src, src_r, dst, dst_r, filter) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5));
    let (sw, sh) = (field(c, src, "w"), field(c, src, "h"));
    let (dw, dh) = (field(c, dst, "w"), field(c, dst, "h"));
    let sr = rect_at(c, src_r, sw, sh);
    let dr = rect_at(c, dst_r, dw, dh);
    let (s, d) = (target_id(c, src), target_id(c, dst));
    c.p.gpu.blit(s, sr, d, dr, filter == 2);
    ret(c, D3D_OK)
}

fn dev_color_fill(c: &mut Ctx) -> ApiResult {
    let (surf, r, color) = (c.arg(1), c.arg(2), c.arg(3));
    let (w, h) = (field(c, surf, "w"), field(c, surf, "h"));
    let rect = rect_at(c, r, w, h);
    let id = target_id(c, surf);
    let (cur, cw, ch) = c.p.d3d9.target;
    c.p.gpu.set_target(id, w, h);
    c.p.gpu.clear(1, color, 1.0, 0, rect);
    c.p.gpu.set_target(cur, cw, ch);
    ret(c, D3D_OK)
}

fn dev_get_render_target_data(c: &mut Ctx) -> ApiResult {
    let (src, dst) = (c.arg(1), c.arg(2));
    let (w, h) = (field(c, src, "w"), field(c, src, "h"));
    let id = target_id(c, src);
    let (pitch, bits) = surface_bits(c, dst);
    c.p.gpu.read_target(id, w, h, bits, pitch);
    ret(c, D3D_OK)
}

/// Copies a system-memory surface's or texture level's bytes into another and marks
/// the destination for upload.
fn copy_bits(c: &mut Ctx, src: (u32, u32), dst: (u32, u32), rows: u32) {
    let n = src.0.min(dst.0);
    for y in 0..rows {
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(src.1 + y * src.0, &mut b);
        let _ = c.host.write(dst.1 + y * dst.0, &b);
    }
}

fn dev_update_surface(c: &mut Ctx) -> ApiResult {
    let (src, dst) = (c.arg(1), c.arg(3));
    let rows = level_rows(field(c, src, "fmt"), field(c, src, "h"));
    let s = surface_bits(c, src);
    let d = surface_bits(c, dst);
    copy_bits(c, s, d, rows);
    let owner = field(c, dst, "owner");
    if owner != 0 {
        let level = field(c, dst, "level");
        mark_dirty(c, owner, level);
    }
    ret(c, D3D_OK)
}

fn dev_update_texture(c: &mut Ctx) -> ApiResult {
    let (src, dst) = (c.arg(1), c.arg(2));
    let levels = field(c, src, "levels").min(field(c, dst, "levels"));
    let fmt = field(c, src, "fmt");
    for level in 0..levels {
        let (_, h) = level_dims(field(c, src, "w"), field(c, src, "h"), level);
        let s = level_bits(c, src, level);
        let d = level_bits(c, dst, level);
        copy_bits(c, s, d, level_rows(fmt, h));
        mark_dirty(c, dst, level);
    }
    ret(c, D3D_OK)
}

fn dev_create_state_block(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(2);
    let sb = new_object(c, "IDirect3DStateBlock9", IDIRECT3DSTATEBLOCK9, HashMap::default());
    let snap = (*c.p.d3d9).clone();
    c.p.d3d9_blocks.insert(sb, Box::new(snap));
    out(c, pp, sb);
    ret(c, D3D_OK)
}

fn dev_end_state_block(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let sb = new_object(c, "IDirect3DStateBlock9", IDIRECT3DSTATEBLOCK9, HashMap::default());
    let snap = (*c.p.d3d9).clone();
    c.p.d3d9_blocks.insert(sb, Box::new(snap));
    out(c, pp, sb);
    ret(c, D3D_OK)
}

fn dev_create_query(c: &mut Ctx) -> ApiResult {
    let (ty, pp) = (c.arg(1), c.arg(2));
    if pp == 0 {
        // a NULL pointer asks whether the type is supported
        return ret(c, if matches!(ty, 8 | 9) { D3D_OK } else { D3DERR_NOTAVAILABLE });
    }
    let q = new_object(c, "IDirect3DQuery9", IDIRECT3DQUERY9, HashMap::from_iter([("qtype", ty)]));
    out(c, pp, q);
    ret(c, D3D_OK)
}

fn dev_get_swap_chain(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(2);
    let dev = this(c);
    let mut sc = field(c, dev, "swapchain");
    if sc == 0 || !c.p.d3d_objects.contains_key(&sc) {
        sc = new_object(c, "IDirect3DSwapChain9", IDIRECT3DSWAPCHAIN9, HashMap::from_iter([("device", dev)]));
        set_field(c, dev, "swapchain", sc);
    } else {
        add_ref_obj(c, sc);
    }
    out(c, pp, sc);
    ret(c, D3D_OK)
}

fn dev_get_creation_parameters(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let vals = [0, 1, field(c, t, "hwnd"), field(c, t, "flags")];
    for (i, v) in vals.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

fn dev_get_depth_stencil(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let mut s = c.p.d3d9.depth;
    if s == 0 {
        let (w, h) = c.p.d3d9.backbuffer;
        let fmt = match field(c, c.p.d3d9.device, "depth_fmt") {
            0 => D3DFMT_D24S8,
            f => f,
        };
        s = new_surface(c, w, h, fmt, D3DUSAGE_DEPTHSTENCIL, 0);
        c.p.d3d9.depth = s;
    }
    add_ref_obj(c, s);
    out(c, pp, s);
    ret(c, D3D_OK)
}

fn unsupported(c: &mut Ctx) -> ApiResult {
    let n = c.name();
    c.p.log(format!("{n}: not supported"));
    ret(c, D3DERR_INVALIDCALL)
}

const IDIRECT3DDEVICE9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("TestCooperativeLevel", 1, okn),
    ("GetAvailableTextureMem", 1, |c| ret(c, 512 << 20)),
    ("EvictManagedResources", 1, okn),
    ("GetDirect3D", 2, |c| {
        let d3d = field(c, this(c), "d3d");
        add_ref_obj(c, d3d);
        out(c, c.arg(1), d3d);
        ret(c, D3D_OK)
    }),
    ("GetDeviceCaps", 2, |c| {
        let p = c.arg(1);
        write_caps(c, p);
        ret(c, D3D_OK)
    }),
    ("GetDisplayMode", 3, |c| {
        let p = c.arg(2);
        write_display_mode(c, p, false);
        ret(c, D3D_OK)
    }),
    ("GetCreationParameters", 2, dev_get_creation_parameters),
    ("SetCursorProperties", 4, okn),
    ("SetCursorPosition", 4, |c| ret(c, 0)),
    ("ShowCursor", 2, |c| ret(c, 1)),
    ("CreateAdditionalSwapChain", 3, unsupported),
    ("GetSwapChain", 3, dev_get_swap_chain),
    ("GetNumberOfSwapChains", 1, |c| ret(c, 1)),
    ("Reset", 2, dev_reset),
    ("Present", 5, dev_present),
    ("GetBackBuffer", 5, |c| {
        let s = back_buffer(c);
        out(c, c.arg(4), s);
        ret(c, D3D_OK)
    }),
    ("GetRasterStatus", 3, |c| {
        let p = c.arg(2);
        let _ = c.host.write_u32(p, 0);
        let _ = c.host.write_u32(p + 4, 0);
        ret(c, D3D_OK)
    }),
    ("SetDialogBoxMode", 2, okn),
    ("SetGammaRamp", 4, |c| ret(c, 0)),
    ("GetGammaRamp", 3, |c| {
        let p = c.arg(2);
        for ch in 0..3u32 {
            for i in 0..256u32 {
                let _ = c.host.write_u16(p + ch * 512 + 2 * i, (i * 257) as u16);
            }
        }
        ret(c, 0)
    }),
    ("CreateTexture", 9, dev_create_texture),
    ("CreateVolumeTexture", 10, unsupported),
    ("CreateCubeTexture", 8, unsupported),
    ("CreateVertexBuffer", 7, |c| dev_create_buffer(c, false)),
    ("CreateIndexBuffer", 7, |c| dev_create_buffer(c, true)),
    ("CreateRenderTarget", 9, |c| dev_create_surface(c, D3DUSAGE_RENDERTARGET, 7)),
    ("CreateDepthStencilSurface", 9, |c| dev_create_surface(c, D3DUSAGE_DEPTHSTENCIL, 7)),
    ("UpdateSurface", 5, dev_update_surface),
    ("UpdateTexture", 3, dev_update_texture),
    ("GetRenderTargetData", 3, dev_get_render_target_data),
    ("GetFrontBufferData", 3, okn),
    ("StretchRect", 6, dev_stretch_rect),
    ("ColorFill", 4, dev_color_fill),
    ("CreateOffscreenPlainSurface", 7, |c| dev_create_surface(c, 0, 5)),
    ("SetRenderTarget", 3, dev_set_render_target),
    ("GetRenderTarget", 3, dev_get_render_target),
    ("SetDepthStencilSurface", 2, |c| {
        let (old, new) = (c.p.d3d9.depth, c.arg(1));
        rebind(c, old, new);
        c.p.d3d9.depth = new;
        ret(c, D3D_OK)
    }),
    ("GetDepthStencilSurface", 2, dev_get_depth_stencil),
    ("BeginScene", 1, okn),
    ("EndScene", 1, okn),
    ("Clear", 7, dev_clear),
    ("SetTransform", 3, dev_set_transform),
    ("GetTransform", 3, dev_get_transform),
    ("MultiplyTransform", 3, dev_multiply_transform),
    ("SetViewport", 2, dev_set_viewport),
    ("GetViewport", 2, dev_get_viewport),
    ("SetMaterial", 2, okn),
    ("GetMaterial", 2, |c| {
        let _ = c.host.fill(c.arg(1), 68, 0);
        ret(c, D3D_OK)
    }),
    ("SetLight", 3, okn),
    ("GetLight", 3, |c| ret(c, D3DERR_INVALIDCALL)),
    ("LightEnable", 3, okn),
    ("GetLightEnable", 3, |c| {
        out(c, c.arg(2), 0);
        ret(c, D3D_OK)
    }),
    ("SetClipPlane", 3, okn),
    ("GetClipPlane", 3, |c| {
        let _ = c.host.fill(c.arg(2), 16, 0);
        ret(c, D3D_OK)
    }),
    ("SetRenderState", 3, dev_set_render_state),
    ("GetRenderState", 3, dev_get_render_state),
    ("CreateStateBlock", 3, dev_create_state_block),
    ("BeginStateBlock", 1, okn),
    ("EndStateBlock", 2, dev_end_state_block),
    ("SetClipStatus", 2, okn),
    ("GetClipStatus", 2, |c| {
        let _ = c.host.fill(c.arg(1), 8, 0);
        ret(c, D3D_OK)
    }),
    ("GetTexture", 3, dev_get_texture),
    ("SetTexture", 3, dev_set_texture),
    ("GetTextureStageState", 4, dev_get_tss),
    ("SetTextureStageState", 4, dev_set_tss),
    ("GetSamplerState", 4, dev_get_sampler_state),
    ("SetSamplerState", 4, dev_set_sampler_state),
    ("ValidateDevice", 2, |c| {
        out(c, c.arg(1), 1);
        ret(c, D3D_OK)
    }),
    ("SetPaletteEntries", 3, okn),
    ("GetPaletteEntries", 3, okn),
    ("SetCurrentTexturePalette", 2, okn),
    ("GetCurrentTexturePalette", 2, |c| {
        out(c, c.arg(1), 0);
        ret(c, D3D_OK)
    }),
    ("SetScissorRect", 2, |c| {
        let r = rect_at(c, c.arg(1), 0, 0);
        c.p.d3d9.scissor = r;
        ret(c, D3D_OK)
    }),
    ("GetScissorRect", 2, |c| {
        let (p, s) = (c.arg(1), c.p.d3d9.scissor);
        for (i, v) in [s[0], s[1], s[0] + s[2], s[1] + s[3]].iter().enumerate() {
            let _ = c.host.write_u32(p + 4 * i as u32, *v as u32);
        }
        ret(c, D3D_OK)
    }),
    ("SetSoftwareVertexProcessing", 2, okn),
    ("GetSoftwareVertexProcessing", 1, |c| ret(c, 0)),
    ("SetNPatchMode", 2, okn),
    ("GetNPatchMode", 1, |c| ret(c, 0)),
    ("DrawPrimitive", 4, dev_draw_primitive),
    ("DrawIndexedPrimitive", 7, dev_draw_indexed_primitive),
    ("DrawPrimitiveUP", 5, dev_draw_primitive_up),
    ("DrawIndexedPrimitiveUP", 9, dev_draw_indexed_primitive_up),
    ("ProcessVertices", 7, unsupported),
    ("CreateVertexDeclaration", 3, |c| {
        let (p, pp) = (c.arg(1), c.arg(2));
        let el = read_elements(c, p);
        let d = new_declaration(c, &el);
        out(c, pp, d);
        ret(c, D3D_OK)
    }),
    ("SetVertexDeclaration", 2, |c| {
        let old = c.p.d3d9.decl;
        rebind(c, old, c.arg(1));
        c.p.d3d9.decl = c.arg(1);
        if c.arg(1) != 0 {
            c.p.d3d9.fvf = 0;
        }
        ret(c, D3D_OK)
    }),
    ("GetVertexDeclaration", 2, |c| {
        let d = c.p.d3d9.decl;
        add_ref_obj(c, d);
        out(c, c.arg(1), d);
        ret(c, D3D_OK)
    }),
    ("SetFVF", 2, |c| {
        c.p.d3d9.fvf = c.arg(1);
        let old = c.p.d3d9.decl;
        rebind(c, old, 0);
        c.p.d3d9.decl = 0;
        ret(c, D3D_OK)
    }),
    ("GetFVF", 2, |c| {
        let v = c.p.d3d9.fvf;
        out(c, c.arg(1), v);
        ret(c, D3D_OK)
    }),
    ("CreateVertexShader", 3, |c| dev_create_shader(c, true)),
    ("SetVertexShader", 2, |c| {
        let old = c.p.d3d9.vs;
        rebind(c, old, c.arg(1));
        c.p.d3d9.vs = c.arg(1);
        ret(c, D3D_OK)
    }),
    ("GetVertexShader", 2, |c| {
        let s = c.p.d3d9.vs;
        add_ref_obj(c, s);
        out(c, c.arg(1), s);
        ret(c, D3D_OK)
    }),
    ("SetVertexShaderConstantF", 4, |c| set_constants(c, 0)),
    ("GetVertexShaderConstantF", 4, |c| get_constants(c, 0)),
    ("SetVertexShaderConstantI", 4, |c| set_constants(c, 2)),
    ("GetVertexShaderConstantI", 4, |c| get_constants(c, 2)),
    ("SetVertexShaderConstantB", 4, |c| set_constants(c, 4)),
    ("GetVertexShaderConstantB", 4, |c| get_constants(c, 4)),
    ("SetStreamSource", 5, |c| {
        let (i, vb, off, stride) = (c.arg(1) as usize, c.arg(2), c.arg(3), c.arg(4));
        if i < 4 {
            let old = c.p.d3d9.streams[i].0;
            rebind(c, old, vb);
            c.p.d3d9.streams[i] = (vb, off, stride, c.p.d3d9.streams[i].3);
        }
        ret(c, D3D_OK)
    }),
    ("GetStreamSource", 5, |c| {
        let i = (c.arg(1) as usize).min(3);
        let (vb, off, stride, _) = c.p.d3d9.streams[i];
        add_ref_obj(c, vb);
        out(c, c.arg(2), vb);
        out(c, c.arg(3), off);
        out(c, c.arg(4), stride);
        ret(c, D3D_OK)
    }),
    ("SetStreamSourceFreq", 3, |c| {
        let (i, f) = (c.arg(1) as usize, c.arg(2));
        if i < 4 {
            c.p.d3d9.streams[i].3 = f;
        }
        ret(c, D3D_OK)
    }),
    ("GetStreamSourceFreq", 3, |c| {
        let i = (c.arg(1) as usize).min(3);
        let f = c.p.d3d9.streams[i].3;
        out(c, c.arg(2), f);
        ret(c, D3D_OK)
    }),
    ("SetIndices", 2, |c| {
        let old = c.p.d3d9.indices;
        rebind(c, old, c.arg(1));
        c.p.d3d9.indices = c.arg(1);
        ret(c, D3D_OK)
    }),
    ("GetIndices", 2, |c| {
        let ib = c.p.d3d9.indices;
        add_ref_obj(c, ib);
        out(c, c.arg(1), ib);
        ret(c, D3D_OK)
    }),
    ("CreatePixelShader", 3, |c| dev_create_shader(c, false)),
    ("SetPixelShader", 2, |c| {
        let old = c.p.d3d9.ps;
        rebind(c, old, c.arg(1));
        c.p.d3d9.ps = c.arg(1);
        ret(c, D3D_OK)
    }),
    ("GetPixelShader", 2, |c| {
        let s = c.p.d3d9.ps;
        add_ref_obj(c, s);
        out(c, c.arg(1), s);
        ret(c, D3D_OK)
    }),
    ("SetPixelShaderConstantF", 4, |c| set_constants(c, 1)),
    ("GetPixelShaderConstantF", 4, |c| get_constants(c, 1)),
    ("SetPixelShaderConstantI", 4, |c| set_constants(c, 3)),
    ("GetPixelShaderConstantI", 4, |c| get_constants(c, 3)),
    ("SetPixelShaderConstantB", 4, |c| set_constants(c, 5)),
    ("GetPixelShaderConstantB", 4, |c| get_constants(c, 5)),
    ("DrawRectPatch", 4, unsupported),
    ("DrawTriPatch", 4, unsupported),
    ("DeletePatch", 2, okn),
    ("CreateQuery", 3, dev_create_query),
    ("SetConvolutionMonoKernel", 5, okn),
    ("ComposeRects", 9, unsupported),
    ("PresentEx", 6, dev_present),
    ("GetGPUThreadPriority", 2, |c| {
        out(c, c.arg(1), 0);
        ret(c, D3D_OK)
    }),
    ("SetGPUThreadPriority", 2, okn),
    ("WaitForVBlank", 2, okn),
    ("CheckResourceResidency", 3, okn),
    ("SetMaximumFrameLatency", 2, okn),
    ("GetMaximumFrameLatency", 2, |c| {
        out(c, c.arg(1), 1);
        ret(c, D3D_OK)
    }),
    ("CheckDeviceState", 2, okn),
    ("CreateRenderTargetEx", 10, |c| dev_create_surface(c, D3DUSAGE_RENDERTARGET, 7)),
    ("CreateOffscreenPlainSurfaceEx", 8, |c| dev_create_surface(c, 0, 5)),
    ("CreateDepthStencilSurfaceEx", 10, |c| dev_create_surface(c, D3DUSAGE_DEPTHSTENCIL, 7)),
    ("ResetEx", 3, dev_reset),
    ("GetDisplayModeEx", 4, |c| {
        let (p, rot) = (c.arg(2), c.arg(3));
        write_display_mode(c, p, true);
        out(c, rot, 1);
        ret(c, D3D_OK)
    }),
];

// ---- resources --------------------------------------------------------------------------

fn res_get_device(c: &mut Ctx) -> ApiResult {
    let dev = c.p.d3d9.device;
    add_ref_obj(c, dev);
    out(c, c.arg(1), dev);
    ret(c, D3D_OK)
}

/// IDirect3DResource9 methods shared by every resource (after IUnknown).
macro_rules! resource_methods {
    ($ty:expr) => {
        [
            ("QueryInterface", 3, query_interface as ApiFn),
            ("AddRef", 1, add_ref as ApiFn),
            ("Release", 1, release as ApiFn),
            ("GetDevice", 2, res_get_device as ApiFn),
            ("SetPrivateData", 5, okn as ApiFn),
            ("GetPrivateData", 4, (|c: &mut Ctx| ret(c, D3DERR_NOTFOUND)) as ApiFn),
            ("FreePrivateData", 2, okn as ApiFn),
            ("SetPriority", 2, (|c: &mut Ctx| ret(c, 0)) as ApiFn),
            ("GetPriority", 1, (|c: &mut Ctx| ret(c, 0)) as ApiFn),
            ("PreLoad", 1, (|c: &mut Ctx| ret(c, 0)) as ApiFn),
            ("GetType", 1, (|c: &mut Ctx| ret(c, $ty)) as ApiFn),
        ]
    };
}

fn surface_desc(c: &mut Ctx, p: u32, fmt: u32, ty: u32, usage: u32, pool: u32, w: u32, h: u32) {
    for (i, v) in [fmt, ty, usage, pool, 0, 0, w, h].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
}

fn tex_get_level_desc(c: &mut Ctx) -> ApiResult {
    let (t, level, p) = (this(c), c.arg(1), c.arg(2));
    let (w, h) = level_dims(field(c, t, "w"), field(c, t, "h"), level);
    let (fmt, usage, pool) = (field(c, t, "fmt"), field(c, t, "usage"), field(c, t, "pool"));
    surface_desc(c, p, fmt, 1, usage, pool, w, h);
    ret(c, D3D_OK)
}

fn write_locked(c: &mut Ctx, out_rect: u32, pitch: u32, bits: u32, rect: u32, fmt: u32) {
    let mut p = bits;
    if rect != 0 {
        let l = c.host.read_u32(rect).unwrap_or(0);
        let top = c.host.read_u32(rect + 4).unwrap_or(0);
        p += if level_rows(fmt, 4) == 1 {
            // compressed: rows and columns of 4x4 blocks
            (top / 4) * pitch + (l / 4) * level_pitch(fmt, 4)
        } else {
            top * pitch + l * format_bpp(fmt)
        };
    }
    let _ = c.host.write_u32(out_rect, pitch);
    let _ = c.host.write_u32(out_rect + 4, p);
}

fn tex_lock_rect(c: &mut Ctx) -> ApiResult {
    let (t, level, out_rect, rect) = (this(c), c.arg(1), c.arg(2), c.arg(3));
    let (pitch, bits) = level_bits(c, t, level);
    let fmt = field(c, t, "fmt");
    write_locked(c, out_rect, pitch, bits, rect, fmt);
    ret(c, D3D_OK)
}

fn tex_unlock_rect(c: &mut Ctx) -> ApiResult {
    let (t, level) = (this(c), c.arg(1));
    mark_dirty(c, t, level);
    ret(c, D3D_OK)
}

fn tex_get_surface_level(c: &mut Ctx) -> ApiResult {
    let (t, level, pp) = (this(c), c.arg(1), c.arg(2));
    let (w, h) = level_dims(field(c, t, "w"), field(c, t, "h"), level);
    let (pitch, bits) = level_bits(c, t, level);
    let (fmt, usage, pool) = (field(c, t, "fmt"), field(c, t, "usage"), field(c, t, "pool"));
    let s = new_surface(c, w, h, fmt, usage, pool);
    set_field(c, s, "bits0", bits);
    set_field(c, s, "pitch", pitch);
    set_field(c, s, "owner", t);
    set_field(c, s, "level", level);
    out(c, pp, s);
    ret(c, D3D_OK)
}

static IDIRECT3DTEXTURE9: &[Method] = &{
    let r = resource_methods!(3);
    [
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10],
        ("SetLOD", 2, |c| ret(c, 0)),
        ("GetLOD", 1, |c| ret(c, 0)),
        ("GetLevelCount", 1, |c| {
            let n = field(c, this(c), "levels");
            ret(c, n)
        }),
        ("SetAutoGenFilterType", 2, okn),
        ("GetAutoGenFilterType", 1, |c| ret(c, 2)),
        ("GenerateMipSubLevels", 1, |c| ret(c, 0)),
        ("GetLevelDesc", 3, tex_get_level_desc),
        ("GetSurfaceLevel", 3, tex_get_surface_level),
        ("LockRect", 5, tex_lock_rect),
        ("UnlockRect", 2, tex_unlock_rect),
        ("AddDirtyRect", 2, |c| {
            mark_dirty(c, this(c), 0);
            ret(c, D3D_OK)
        }),
    ]
};

/// (pitch, bits) of a surface: a texture level's storage, or its own.
fn surface_bits(c: &mut Ctx, s: u32) -> (u32, u32) {
    let owner = field(c, s, "owner");
    if owner != 0 {
        return (field(c, s, "pitch"), field(c, s, "bits0"));
    }
    level_bits(c, s, 0)
}

fn surf_get_desc(c: &mut Ctx) -> ApiResult {
    let (s, p) = (this(c), c.arg(1));
    let (fmt, usage, pool, w, h) = (field(c, s, "fmt"), field(c, s, "usage"), field(c, s, "pool"), field(c, s, "w"), field(c, s, "h"));
    surface_desc(c, p, fmt, 1, usage, pool, w, h);
    ret(c, D3D_OK)
}

fn surf_lock_rect(c: &mut Ctx) -> ApiResult {
    let (s, out_rect, rect) = (this(c), c.arg(1), c.arg(2));
    let (pitch, bits) = surface_bits(c, s);
    let fmt = field(c, s, "fmt");
    write_locked(c, out_rect, pitch, bits, rect, fmt);
    ret(c, D3D_OK)
}

fn surf_unlock_rect(c: &mut Ctx) -> ApiResult {
    let s = this(c);
    let owner = field(c, s, "owner");
    if owner != 0 {
        let level = field(c, s, "level");
        mark_dirty(c, owner, level);
    }
    ret(c, D3D_OK)
}

static IDIRECT3DSURFACE9: &[Method] = &{
    let r = resource_methods!(1);
    [
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10],
        ("GetContainer", 3, |c| {
            let owner = field(c, this(c), "owner");
            let o = if owner != 0 { owner } else { c.p.d3d9.device };
            add_ref_obj(c, o);
            out(c, c.arg(2), o);
            ret(c, D3D_OK)
        }),
        ("GetDesc", 2, surf_get_desc),
        ("LockRect", 4, surf_lock_rect),
        ("UnlockRect", 1, surf_unlock_rect),
        ("GetDC", 2, |c| ret(c, D3DERR_INVALIDCALL)),
        ("ReleaseDC", 2, |c| ret(c, D3DERR_INVALIDCALL)),
    ]
};

fn buf_lock(c: &mut Ctx) -> ApiResult {
    let (t, off, pp) = (this(c), c.arg(1), c.arg(3));
    let bits = field(c, t, "bits");
    out(c, pp, bits + off);
    ret(c, D3D_OK)
}

fn buf_get_desc(c: &mut Ctx, index: bool) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let (len, usage, fvf, pool) = (field(c, t, "len"), field(c, t, "usage"), field(c, t, "fvf"), field(c, t, "pool"));
    let vals: Vec<u32> = if index { vec![fvf, 3, usage, pool, len] } else { vec![100, 2, usage, pool, len, fvf] };
    for (i, v) in vals.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

static IDIRECT3DVERTEXBUFFER9: &[Method] = &{
    let r = resource_methods!(6);
    [r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10], ("Lock", 5, buf_lock), ("Unlock", 1, okn), ("GetDesc", 2, |c| buf_get_desc(c, false))]
};

static IDIRECT3DINDEXBUFFER9: &[Method] = &{
    let r = resource_methods!(7);
    [r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10], ("Lock", 5, buf_lock), ("Unlock", 1, okn), ("GetDesc", 2, |c| buf_get_desc(c, true))]
};

static IDIRECT3DVERTEXDECLARATION9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("GetDeclaration", 3, |c| {
        let (t, p, n_p) = (this(c), c.arg(1), c.arg(2));
        let el = declaration(c, t);
        let n = el.len() as u32 + 1;
        if p != 0 {
            for (i, e) in el.iter().enumerate() {
                let a = p + 8 * i as u32;
                let _ = c.host.write_u16(a, e[0] as u16);
                let _ = c.host.write_u16(a + 2, e[1] as u16);
                let _ = c.host.write(a + 4, &[e[2] as u8, 0, e[3] as u8, (e[3] >> 8) as u8]);
            }
            let a = p + 8 * el.len() as u32;
            let _ = c.host.write_u16(a, 0xFF);
            let _ = c.host.write_u16(a + 2, 0);
            let _ = c.host.write(a + 4, &[17, 0, 0, 0]);
        }
        out(c, n_p, n);
        ret(c, D3D_OK)
    }),
];

static IDIRECT3DVERTEXSHADER9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("GetFunction", 3, shader_get_function),
];

static IDIRECT3DPIXELSHADER9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("GetFunction", 3, shader_get_function),
];

static IDIRECT3DSTATEBLOCK9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, |c| {
        let t = this(c);
        let r = release(c);
        if !c.p.d3d_objects.contains_key(&t) {
            c.p.d3d9_blocks.remove(&t);
        }
        r
    }),
    ("GetDevice", 2, res_get_device),
    ("Capture", 1, |c| {
        let t = this(c);
        let snap = (*c.p.d3d9).clone();
        c.p.d3d9_blocks.insert(t, Box::new(snap));
        ret(c, D3D_OK)
    }),
    ("Apply", 1, |c| {
        let t = this(c);
        if let Some(b) = c.p.d3d9_blocks.get(&t) {
            // Render targets and the back buffer are not part of a state block.
            let mut s = (**b).clone();
            let cur = &c.p.d3d9;
            (s.target, s.depth, s.rt, s.backbuffer, s.device) = (cur.target, cur.depth, cur.rt, cur.backbuffer, cur.device);
            // the device's references follow the bindings the block changes
            let mut moves: Vec<(u32, u32)> = cur.textures.iter().zip(&s.textures).map(|(&a, &b)| (a, b)).collect();
            moves.extend(cur.streams.iter().zip(&s.streams).map(|(a, b)| (a.0, b.0)));
            moves.extend([(cur.indices, s.indices), (cur.vs, s.vs), (cur.ps, s.ps), (cur.decl, s.decl)]);
            c.p.d3d9 = Box::new(s);
            for (old, new) in moves {
                if old != new {
                    rebind(c, old, new);
                }
            }
        }
        ret(c, D3D_OK)
    }),
];

static IDIRECT3DQUERY9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("GetType", 1, |c| {
        let t = field(c, this(c), "qtype");
        ret(c, t)
    }),
    ("GetDataSize", 1, |c| {
        let t = field(c, this(c), "qtype");
        ret(c, if t == 8 { 4 } else { 8 })
    }),
    ("Issue", 2, okn),
    ("GetData", 4, |c| {
        // Every query is complete at once: events TRUE, occlusion counts some pixels.
        let (t, p, size) = (this(c), c.arg(1), c.arg(2));
        let v = if field(c, t, "qtype") == 8 { 1 } else { 1000 };
        if p != 0 && size >= 4 {
            let _ = c.host.write_u32(p, v);
        }
        ret(c, D3D_OK)
    }),
];

static IDIRECT3DSWAPCHAIN9: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("Present", 6, |c| {
        // same arguments as the device's Present after `this`
        dev_present(c)
    }),
    ("GetFrontBufferData", 2, okn),
    ("GetBackBuffer", 4, |c| {
        let s = back_buffer(c);
        out(c, c.arg(3), s);
        ret(c, D3D_OK)
    }),
    ("GetRasterStatus", 2, |c| {
        let p = c.arg(1);
        let _ = c.host.fill(p, 8, 0);
        ret(c, D3D_OK)
    }),
    ("GetDisplayMode", 2, |c| {
        let p = c.arg(1);
        write_display_mode(c, p, false);
        ret(c, D3D_OK)
    }),
    ("GetDevice", 2, res_get_device),
    ("GetPresentParameters", 2, |c| {
        let p = c.arg(1);
        let (w, h) = c.p.d3d9.backbuffer;
        let dev = c.p.d3d9.device;
        let vals = [w, h, field(c, dev, "fmt"), 1, 0, 0, 1, field(c, dev, "hwnd"), 1, 1, field(c, dev, "depth_fmt"), 0, 0, 0];
        for (i, v) in vals.iter().enumerate() {
            let _ = c.host.write_u32(p + 4 * i as u32, *v);
        }
        ret(c, D3D_OK)
    }),
];

fn register_iface(r: &mut Registry, iface: &str, methods: &[Method]) {
    for (name, nargs, f) in methods {
        r.add_n(DLL, &format!("{iface}::{name}"), *nargs, *f);
    }
}

fn create(c: &mut Ctx) -> ApiResult {
    let ex = c.name().ends_with("Ex");
    let obj = new_object(c, "IDirect3D9Ex", IDIRECT3D9, HashMap::default());
    c.trace(format!("(sdk {}) = {obj:#x}", c.arg(0)));
    if ex {
        // Direct3DCreate9Ex(sdk, &d3d) -> HRESULT
        out(c, c.arg(1), obj);
        return Ok(Ret::stdcall(2, D3D_OK));
    }
    Ok(Ret::stdcall(1, obj))
}

pub fn register(r: &mut Registry) {
    r.add(DLL, "Direct3DCreate9", create);
    r.add(DLL, "Direct3DCreate9Ex", create);
    r.add(DLL, "D3DPERF_BeginEvent", |_c| Ok(Ret::stdcall(2, 0)));
    r.add(DLL, "D3DPERF_EndEvent", |_c| Ok(Ret::stdcall(0, 0)));
    r.add(DLL, "D3DPERF_SetMarker", |_c| Ok(Ret::stdcall(2, 0)));
    r.add(DLL, "D3DPERF_SetRegion", |_c| Ok(Ret::stdcall(2, 0)));
    r.add(DLL, "D3DPERF_GetStatus", |_c| Ok(Ret::stdcall(0, 0)));
    r.add(DLL, "D3DPERF_QueryRepeatFrame", |_c| Ok(Ret::stdcall(0, 0)));
    r.add(DLL, "D3DPERF_SetOptions", |_c| Ok(Ret::stdcall(1, 0)));
    r.add(DLL, "DebugSetMute", |_c| Ok(Ret::stdcall(0, 0)));
    r.add(DLL, "DebugSetLevel", |_c| Ok(Ret::stdcall(0, 0)));
    r.add(DLL, "Direct3DShaderValidatorCreate9", |_c| Ok(Ret::stdcall(0, 0)));
    register_iface(r, "IDirect3D9Ex", IDIRECT3D9);
    register_iface(r, "IDirect3DDevice9Ex", IDIRECT3DDEVICE9);
    register_iface(r, "IDirect3DTexture9", IDIRECT3DTEXTURE9);
    register_iface(r, "IDirect3DSurface9", IDIRECT3DSURFACE9);
    register_iface(r, "IDirect3DVertexBuffer9", IDIRECT3DVERTEXBUFFER9);
    register_iface(r, "IDirect3DIndexBuffer9", IDIRECT3DINDEXBUFFER9);
    register_iface(r, "IDirect3DVertexDeclaration9", IDIRECT3DVERTEXDECLARATION9);
    register_iface(r, "IDirect3DVertexShader9", IDIRECT3DVERTEXSHADER9);
    register_iface(r, "IDirect3DPixelShader9", IDIRECT3DPIXELSHADER9);
    register_iface(r, "IDirect3DStateBlock9", IDIRECT3DSTATEBLOCK9);
    register_iface(r, "IDirect3DQuery9", IDIRECT3DQUERY9);
    register_iface(r, "IDirect3DSwapChain9", IDIRECT3DSWAPCHAIN9);
}
