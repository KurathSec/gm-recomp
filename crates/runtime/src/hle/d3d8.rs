//! Direct3D 8 as COM objects in guest memory. Each interface gets one vtable whose
//! slots are HLE stubs; objects are `[vtable, ...]` blocks on the process heap with
//! their state kept host-side in `Process::d3d_objects`. The harness only records
//! draws and counts `Present`s; the browser backend (R3) turns the same calls into
//! WebGL2 work.

use super::{ApiFn, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{D3dObject, Process};
use std::collections::HashMap;

pub const D3D_OK: u32 = 0;
pub const D3DERR_INVALIDCALL: u32 = 0x8876_086C;
pub const D3DERR_NOTAVAILABLE: u32 = 0x8876_086A;
pub const E_NOINTERFACE: u32 = 0x8000_4002;
pub const S_FALSE: u32 = 1;

pub const D3DFMT_X8R8G8B8: u32 = 22;
pub const D3DFMT_A8R8G8B8: u32 = 21;

const DLL: &str = "d3d8.dll";

type Method = (&'static str, u32, ApiFn);

fn okn(c: &mut Ctx) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), D3D_OK))
}

fn ret(c: &Ctx, v: u32) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), v))
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
        let id = c.p.registry.lookup(DLL, &format!("{iface}::{name}")).expect("d3d8 method registered");
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

fn out(c: &mut Ctx, p: u32, v: u32) {
    if p != 0 {
        let _ = c.host.write_u32(p, v);
    }
}

pub fn format_bpp(fmt: u32) -> u32 {
    match fmt {
        20 => 3,
        21 | 22 | 71 | 75 | 77 | 79 => 4,
        23..=26 | 29 | 30 | 51 | 70 | 73 | 80 => 2,
        27 | 28 | 41 | 50 | 52 => 1,
        _ => 4,
    }
}

fn surface_desc(c: &mut Ctx, p: u32, fmt: u32, ty: u32, usage: u32, pool: u32, w: u32, h: u32) {
    let size = w * h * format_bpp(fmt);
    for (i, v) in [fmt, ty, usage, pool, size, 0, w, h].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
}

fn write_display_mode(c: &mut Ctx, p: u32) {
    for (i, v) in [1920u32, 1080, 60, D3DFMT_X8R8G8B8].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
}

fn write_caps(c: &mut Ctx, p: u32) {
    let f = |x: f32| x.to_bits();
    let caps: [u32; 53] = [
        1,            // DeviceType HAL
        0,            // AdapterOrdinal
        0x0002_0000,  // Caps: READ_SCANLINE
        0x000A_0000,  // Caps2: CANRENDERWINDOWED | FULLSCREENGAMMA
        0,            // Caps3
        0x8000_0001,  // PresentationIntervals: IMMEDIATE | ONE
        1,            // CursorCaps: COLOR
        0x001B_AFF0,  // DevCaps
        0x0000_0AF2,  // PrimitiveMiscCaps
        0x0073_6191,  // RasterCaps
        0xFF,         // ZCmpCaps
        0x1FFF,       // SrcBlendCaps
        0x1FFF,       // DestBlendCaps
        0xFF,         // AlphaCmpCaps
        0x0008_4208,  // ShadeCaps
        0x0001_4C05,  // TextureCaps (no POW2 restriction)
        0x0303_0300,  // TextureFilterCaps
        0x0303_0300,  // CubeTextureFilterCaps
        0,            // VolumeTextureFilterCaps
        0x1F,         // TextureAddressCaps
        0,            // VolumeTextureAddressCaps
        0x1F,         // LineCaps
        8192,         // MaxTextureWidth
        8192,         // MaxTextureHeight
        0,            // MaxVolumeExtent
        8192,         // MaxTextureRepeat
        8192,         // MaxTextureAspectRatio
        16,           // MaxAnisotropy
        f(1e10),      // MaxVertexW
        f(-32768.0),  // GuardBandLeft
        f(-32768.0),  // GuardBandTop
        f(32767.0),   // GuardBandRight
        f(32767.0),   // GuardBandBottom
        f(0.0),       // ExtentsAdjust
        0xFF,         // StencilCaps
        0x0010_0008,  // FVFCaps: 8 texcoords | PSIZE
        0x03FF_FFFF,  // TextureOpCaps
        8,            // MaxTextureBlendStages
        8,            // MaxSimultaneousTextures
        0x7B,         // VertexProcessingCaps
        8,            // MaxActiveLights
        6,            // MaxUserClipPlanes
        4,            // MaxVertexBlendMatrices
        0,            // MaxVertexBlendMatrixIndex
        f(256.0),     // MaxPointSize
        0x000F_FFFF,  // MaxPrimitiveCount
        0x000F_FFFF,  // MaxVertexIndex
        16,           // MaxStreams
        255,          // MaxStreamStride
        0xFFFE_0101,  // VertexShaderVersion 1.1
        96,           // MaxVertexShaderConst
        0xFFFF_0104,  // PixelShaderVersion 1.4
        f(8.0),       // MaxPixelShaderValue
    ];
    for (i, v) in caps.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
}

// ---- IUnknown ---------------------------------------------------------------------

fn query_interface(c: &mut Ctx) -> ApiResult {
    out(c, c.arg(2), 0);
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
    let n = match c.p.d3d_objects.get_mut(&t) {
        Some(o) => {
            o.refcount = o.refcount.saturating_sub(1);
            o.refcount
        }
        None => 0,
    };
    if n == 0 {
        if let Some(o) = c.p.d3d_objects.remove(&t) {
            c.trace(format!(": freed {}", o.kind));
        }
    }
    ret(c, n)
}

// ---- IDirect3D8 ---------------------------------------------------------------------

fn d3d_get_adapter_identifier(c: &mut Ctx) -> ApiResult {
    let p = c.arg(3);
    let _ = c.host.fill(p, 1100, 0);
    let _ = c.host.write_cstr(p, b"gmrecomp.dll");
    let _ = c.host.write_cstr(p + 512, b"gmrecomp WebGL2 adapter");
    let _ = c.host.write_u32(p + 1024, 0x0001_0000); // DriverVersion low
    let _ = c.host.write_u32(p + 1028, 0x0008_0001); // DriverVersion high
    let _ = c.host.write_u32(p + 1032, 0x10DE); // VendorId
    let _ = c.host.write_u32(p + 1036, 0x0001); // DeviceId
    ret(c, D3D_OK)
}

fn d3d_enum_adapter_modes(c: &mut Ctx) -> ApiResult {
    let mode = c.arg(2);
    let p = c.arg(3);
    if mode != 0 {
        return ret(c, D3DERR_INVALIDCALL);
    }
    write_display_mode(c, p);
    ret(c, D3D_OK)
}

fn d3d_get_adapter_display_mode(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    write_display_mode(c, p);
    ret(c, D3D_OK)
}

fn d3d_check_multisample(c: &mut Ctx) -> ApiResult {
    let ms = c.arg(5);
    ret(c, if ms <= 1 { D3D_OK } else { D3DERR_NOTAVAILABLE })
}

fn d3d_get_device_caps(c: &mut Ctx) -> ApiResult {
    let p = c.arg(3);
    write_caps(c, p);
    ret(c, D3D_OK)
}

fn d3d_create_device(c: &mut Ctx) -> ApiResult {
    let d3d = this(c);
    let hwnd = c.arg(3);
    let flags = c.arg(4);
    let pp = c.arg(5);
    let ppdev = c.arg(6);
    let mut w = c.host.read_u32(pp).unwrap_or(0);
    let mut h = c.host.read_u32(pp + 4).unwrap_or(0);
    let fmt = c.host.read_u32(pp + 8).unwrap_or(D3DFMT_X8R8G8B8);
    let windowed = c.host.read_u32(pp + 28).unwrap_or(1);
    let dev_hwnd = c.host.read_u32(pp + 24).unwrap_or(hwnd);
    if w == 0 || h == 0 {
        let target = if dev_hwnd != 0 { dev_hwnd } else { hwnd };
        if let Some(win) = c.p.windows.get(&target) {
            w = win.w.max(1) as u32;
            h = win.h.max(1) as u32;
        } else {
            w = 800;
            h = 600;
        }
    }
    c.trace(format!("({w}x{h} fmt {fmt} windowed {windowed} flags {flags:#x} hwnd {hwnd:#x})"));
    if let Some(o) = c.p.d3d_objects.get_mut(&d3d) {
        o.refcount += 1;
    }
    let data = HashMap::from([("w", w), ("h", h), ("fmt", fmt), ("hwnd", hwnd), ("flags", flags), ("d3d", d3d)]);
    let dev = new_object(c, "IDirect3DDevice8", IDIRECT3DDEVICE8, data);
    out(c, ppdev, dev);
    ret(c, D3D_OK)
}

const IDIRECT3D8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("RegisterSoftwareDevice", 2, okn),
    ("GetAdapterCount", 1, |c| ret(c, 1)),
    ("GetAdapterIdentifier", 4, d3d_get_adapter_identifier),
    ("GetAdapterModeCount", 2, |c| ret(c, 1)),
    ("EnumAdapterModes", 4, d3d_enum_adapter_modes),
    ("GetAdapterDisplayMode", 3, d3d_get_adapter_display_mode),
    ("CheckDeviceType", 6, okn),
    ("CheckDeviceFormat", 7, okn),
    ("CheckDeviceMultiSampleType", 6, d3d_check_multisample),
    ("CheckDepthStencilMatch", 6, okn),
    ("GetDeviceCaps", 4, d3d_get_device_caps),
    ("GetAdapterMonitor", 2, |c| ret(c, 1)),
    ("CreateDevice", 7, d3d_create_device),
];

// ---- IDirect3DDevice8 ---------------------------------------------------------------

fn dev_get_direct3d(c: &mut Ctx) -> ApiResult {
    let d3d = field(c, this(c), "d3d");
    if let Some(o) = c.p.d3d_objects.get_mut(&d3d) {
        o.refcount += 1;
    }
    out(c, c.arg(1), d3d);
    ret(c, D3D_OK)
}

fn dev_get_device_caps(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    write_caps(c, p);
    ret(c, D3D_OK)
}

fn dev_get_display_mode(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    write_display_mode(c, p);
    ret(c, D3D_OK)
}

fn dev_get_creation_parameters(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let p = c.arg(1);
    let hwnd = field(c, t, "hwnd");
    let flags = field(c, t, "flags");
    for (i, v) in [0u32, 1, hwnd, flags].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

fn dev_reset(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let pp = c.arg(1);
    let w = c.host.read_u32(pp).unwrap_or(0);
    let h = c.host.read_u32(pp + 4).unwrap_or(0);
    if w != 0 && h != 0 {
        set_field(c, t, "w", w);
        set_field(c, t, "h", h);
    }
    c.trace(format!("({w}x{h})"));
    ret(c, D3D_OK)
}

fn dev_present(c: &mut Ctx) -> ApiResult {
    c.p.frames_presented += 1;
    let n = c.p.frames_presented;
    let draws = c.p.draw_calls;
    c.p.draw_calls = 0;
    c.p.log(format!("Present #{n} ({draws} draw calls, t={}ms)", c.p.now_ms()));
    if n >= c.p.frame_limit {
        return Err(super::ApiError(super::FRAME_LIMIT.into()));
    }
    ret(c, D3D_OK)
}

fn new_surface(c: &mut Ctx, w: u32, h: u32, fmt: u32, usage: u32, pool: u32) -> u32 {
    let data = HashMap::from([("w", w), ("h", h), ("fmt", fmt), ("usage", usage), ("pool", pool), ("type", 1)]);
    new_object(c, "IDirect3DSurface8", IDIRECT3DSURFACE8, data)
}

fn dev_get_back_buffer(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let (w, h, fmt) = (field(c, t, "w"), field(c, t, "h"), field(c, t, "fmt"));
    let s = new_surface(c, w, h, fmt, 1, 0);
    out(c, c.arg(3), s);
    ret(c, D3D_OK)
}

fn dev_get_render_target(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let (w, h, fmt) = (field(c, t, "w"), field(c, t, "h"), field(c, t, "fmt"));
    let s = new_surface(c, w, h, fmt, 1, 0);
    out(c, c.arg(1), s);
    ret(c, D3D_OK)
}

fn dev_get_depth_stencil(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let (w, h) = (field(c, t, "w"), field(c, t, "h"));
    let s = new_surface(c, w, h, 75, 2, 0);
    out(c, c.arg(1), s);
    ret(c, D3D_OK)
}

fn dev_create_texture(c: &mut Ctx) -> ApiResult {
    let w = c.arg(1);
    let h = c.arg(2);
    let mut levels = c.arg(3);
    let usage = c.arg(4);
    let fmt = c.arg(5);
    let pool = c.arg(6);
    let pp = c.arg(7);
    if w == 0 || h == 0 {
        return ret(c, D3DERR_INVALIDCALL);
    }
    if levels == 0 {
        levels = 32 - w.max(h).leading_zeros();
    }
    let data = HashMap::from([("w", w), ("h", h), ("levels", levels), ("usage", usage), ("fmt", fmt), ("pool", pool), ("type", 3)]);
    let tex = new_object(c, "IDirect3DTexture8", IDIRECT3DTEXTURE8, data);
    c.p.textures_created += 1;
    c.trace(format!("({w}x{h} levels {levels} fmt {fmt} usage {usage:#x} pool {pool}) = {tex:#x}"));
    out(c, pp, tex);
    ret(c, D3D_OK)
}

fn dev_create_buffer(c: &mut Ctx, index: bool) -> ApiResult {
    let len = c.arg(1);
    let usage = c.arg(2);
    let fvf_or_fmt = c.arg(3);
    let pool = c.arg(4);
    let pp = c.arg(5);
    let bits = alloc(c, len.max(4));
    let data = HashMap::from([("len", len), ("usage", usage), ("fvf", fvf_or_fmt), ("pool", pool), ("bits", bits)]);
    let obj = if index {
        new_object(c, "IDirect3DIndexBuffer8", IDIRECT3DINDEXBUFFER8, data)
    } else {
        new_object(c, "IDirect3DVertexBuffer8", IDIRECT3DVERTEXBUFFER8, data)
    };
    c.trace(format!("({len} bytes usage {usage:#x} fvf/fmt {fvf_or_fmt:#x} pool {pool}) = {obj:#x}"));
    out(c, pp, obj);
    ret(c, D3D_OK)
}

fn dev_create_render_target(c: &mut Ctx) -> ApiResult {
    let (w, h, fmt) = (c.arg(1), c.arg(2), c.arg(3));
    let s = new_surface(c, w, h, fmt, 1, 0);
    out(c, c.arg(6), s);
    ret(c, D3D_OK)
}

fn dev_create_depth_stencil_surface(c: &mut Ctx) -> ApiResult {
    let (w, h, fmt) = (c.arg(1), c.arg(2), c.arg(3));
    let s = new_surface(c, w, h, fmt, 2, 0);
    out(c, c.arg(5), s);
    ret(c, D3D_OK)
}

fn dev_create_image_surface(c: &mut Ctx) -> ApiResult {
    let (w, h, fmt) = (c.arg(1), c.arg(2), c.arg(3));
    let s = new_surface(c, w, h, fmt, 0, 2);
    out(c, c.arg(4), s);
    ret(c, D3D_OK)
}

fn dev_get_transform(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    for i in 0..16u32 {
        let v = if i % 5 == 0 { 1.0f32 } else { 0.0 };
        let _ = c.host.write_u32(p + 4 * i, v.to_bits());
    }
    ret(c, D3D_OK)
}

fn dev_get_viewport(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let p = c.arg(1);
    let (w, h) = (field(c, t, "w"), field(c, t, "h"));
    for (i, v) in [0u32, 0, w, h, 0.0f32.to_bits(), 1.0f32.to_bits()].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

fn dev_get_dword(c: &mut Ctx) -> ApiResult {
    let n = c.nargs();
    out(c, c.arg(n - 1), 0);
    ret(c, D3D_OK)
}

fn dev_get_raster_status(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let _ = c.host.write_u64(p, 0);
    ret(c, D3D_OK)
}

fn dev_get_gamma_ramp(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    for i in 0..768u32 {
        let v = ((i % 256) * 257) as u16;
        let _ = c.host.write_u16(p + 2 * i, v);
    }
    ret(c, D3D_OK)
}

fn dev_draw(c: &mut Ctx) -> ApiResult {
    c.p.draw_calls += 1;
    ret(c, D3D_OK)
}

fn dev_handle_out(c: &mut Ctx) -> ApiResult {
    let n = c.nargs();
    c.p.next_d3d_handle += 1;
    let h = c.p.next_d3d_handle;
    out(c, c.arg(n - 1), h);
    ret(c, D3D_OK)
}

fn dev_create_vertex_shader(c: &mut Ctx) -> ApiResult {
    c.p.next_d3d_handle += 1;
    let h = c.p.next_d3d_handle;
    out(c, c.arg(3), h);
    ret(c, D3D_OK)
}

fn dev_validate_device(c: &mut Ctx) -> ApiResult {
    out(c, c.arg(1), 1);
    ret(c, D3D_OK)
}

fn dev_set_render_state(c: &mut Ctx) -> ApiResult {
    let state = c.arg(1);
    let v = c.arg(2);
    c.trace(format!("({state}, {v:#x})"));
    ret(c, D3D_OK)
}

const IDIRECT3DDEVICE8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("TestCooperativeLevel", 1, okn),
    ("GetAvailableTextureMem", 1, |c| ret(c, 256 << 20)),
    ("ResourceManagerDiscardBytes", 2, okn),
    ("GetDirect3D", 2, dev_get_direct3d),
    ("GetDeviceCaps", 2, dev_get_device_caps),
    ("GetDisplayMode", 2, dev_get_display_mode),
    ("GetCreationParameters", 2, dev_get_creation_parameters),
    ("SetCursorProperties", 4, okn),
    ("SetCursorPosition", 4, |c| ret(c, 0)),
    ("ShowCursor", 2, |c| ret(c, 0)),
    ("CreateAdditionalSwapChain", 3, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("Reset", 2, dev_reset),
    ("Present", 5, dev_present),
    ("GetBackBuffer", 4, dev_get_back_buffer),
    ("GetRasterStatus", 2, dev_get_raster_status),
    ("SetGammaRamp", 3, |c| ret(c, 0)),
    ("GetGammaRamp", 2, dev_get_gamma_ramp),
    ("CreateTexture", 8, dev_create_texture),
    ("CreateVolumeTexture", 9, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("CreateCubeTexture", 7, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("CreateVertexBuffer", 6, |c| dev_create_buffer(c, false)),
    ("CreateIndexBuffer", 6, |c| dev_create_buffer(c, true)),
    ("CreateRenderTarget", 7, dev_create_render_target),
    ("CreateDepthStencilSurface", 6, dev_create_depth_stencil_surface),
    ("CreateImageSurface", 5, dev_create_image_surface),
    ("CopyRects", 6, okn),
    ("UpdateTexture", 3, okn),
    ("GetFrontBuffer", 2, okn),
    ("SetRenderTarget", 3, okn),
    ("GetRenderTarget", 2, dev_get_render_target),
    ("GetDepthStencilSurface", 2, dev_get_depth_stencil),
    ("BeginScene", 1, okn),
    ("EndScene", 1, okn),
    ("Clear", 7, okn),
    ("SetTransform", 3, okn),
    ("GetTransform", 3, dev_get_transform),
    ("MultiplyTransform", 3, okn),
    ("SetViewport", 2, okn),
    ("GetViewport", 2, dev_get_viewport),
    ("SetMaterial", 2, okn),
    ("GetMaterial", 2, okn),
    ("SetLight", 3, okn),
    ("GetLight", 3, okn),
    ("LightEnable", 3, okn),
    ("GetLightEnable", 3, dev_get_dword),
    ("SetClipPlane", 3, okn),
    ("GetClipPlane", 3, okn),
    ("SetRenderState", 3, dev_set_render_state),
    ("GetRenderState", 3, dev_get_dword),
    ("BeginStateBlock", 1, okn),
    ("EndStateBlock", 2, dev_handle_out),
    ("ApplyStateBlock", 2, okn),
    ("CaptureStateBlock", 2, okn),
    ("DeleteStateBlock", 2, okn),
    ("CreateStateBlock", 3, dev_handle_out),
    ("SetClipStatus", 2, okn),
    ("GetClipStatus", 2, okn),
    ("GetTexture", 3, dev_get_dword),
    ("SetTexture", 3, okn),
    ("GetTextureStageState", 4, dev_get_dword),
    ("SetTextureStageState", 4, okn),
    ("ValidateDevice", 2, dev_validate_device),
    ("GetInfo", 4, |c| ret(c, S_FALSE)),
    ("SetPaletteEntries", 3, okn),
    ("GetPaletteEntries", 3, okn),
    ("SetCurrentTexturePalette", 2, okn),
    ("GetCurrentTexturePalette", 2, dev_get_dword),
    ("DrawPrimitive", 4, dev_draw),
    ("DrawIndexedPrimitive", 6, dev_draw),
    ("DrawPrimitiveUP", 5, dev_draw),
    ("DrawIndexedPrimitiveUP", 9, dev_draw),
    ("ProcessVertices", 6, okn),
    ("CreateVertexShader", 5, dev_create_vertex_shader),
    ("SetVertexShader", 2, okn),
    ("GetVertexShader", 2, dev_get_dword),
    ("DeleteVertexShader", 2, okn),
    ("SetVertexShaderConstant", 4, okn),
    ("GetVertexShaderConstant", 4, okn),
    ("GetVertexShaderDeclaration", 4, okn),
    ("GetVertexShaderFunction", 4, okn),
    ("SetStreamSource", 4, okn),
    ("GetStreamSource", 4, okn),
    ("SetIndices", 3, okn),
    ("GetIndices", 3, okn),
    ("CreatePixelShader", 3, dev_handle_out),
    ("SetPixelShader", 2, okn),
    ("GetPixelShader", 2, dev_get_dword),
    ("DeletePixelShader", 2, okn),
    ("SetPixelShaderConstant", 4, okn),
    ("GetPixelShaderConstant", 4, okn),
    ("GetPixelShaderFunction", 4, okn),
    ("DrawRectPatch", 4, okn),
    ("DrawTriPatch", 4, okn),
    ("DeletePatch", 2, okn),
];

// ---- resources ----------------------------------------------------------------------

fn res_get_device(c: &mut Ctx) -> ApiResult {
    let dev = c.p.d3d_objects.iter().find(|(_, o)| o.kind == "IDirect3DDevice8").map(|(&a, _)| a).unwrap_or(0);
    out(c, c.arg(1), dev);
    ret(c, D3D_OK)
}

fn level_dims(w: u32, h: u32, level: u32) -> (u32, u32) {
    ((w >> level).max(1), (h >> level).max(1))
}

fn tex_get_level_desc(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let level = c.arg(1);
    let p = c.arg(2);
    let (w, h) = level_dims(field(c, t, "w"), field(c, t, "h"), level);
    let (fmt, usage, pool) = (field(c, t, "fmt"), field(c, t, "usage"), field(c, t, "pool"));
    surface_desc(c, p, fmt, 1, usage, pool, w, h);
    ret(c, D3D_OK)
}

fn level_key(level: u32) -> &'static str {
    const KEYS: [&str; 16] = ["bits0", "bits1", "bits2", "bits3", "bits4", "bits5", "bits6", "bits7", "bits8", "bits9", "bits10", "bits11", "bits12", "bits13", "bits14", "bits15"];
    KEYS[(level as usize).min(15)]
}

/// Allocates (once) the pixel storage of one mip level and returns (pitch, bits).
fn level_bits(c: &mut Ctx, obj: u32, level: u32) -> (u32, u32) {
    let (w, h) = level_dims(field(c, obj, "w"), field(c, obj, "h"), level);
    let pitch = w * format_bpp(field(c, obj, "fmt"));
    let key = level_key(level);
    let mut bits = field(c, obj, key);
    if bits == 0 {
        bits = alloc(c, (pitch * h).max(4));
        set_field(c, obj, key, bits);
    }
    (pitch, bits)
}

fn tex_lock_rect(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let level = c.arg(1);
    let out_rect = c.arg(2);
    let rect = c.arg(3);
    let (pitch, bits) = level_bits(c, t, level);
    let mut p = bits;
    if rect != 0 {
        let l = c.host.read_u32(rect).unwrap_or(0);
        let top = c.host.read_u32(rect + 4).unwrap_or(0);
        p += top * pitch + l * format_bpp(field(c, t, "fmt"));
    }
    let _ = c.host.write_u32(out_rect, pitch);
    let _ = c.host.write_u32(out_rect + 4, p);
    ret(c, D3D_OK)
}

fn tex_get_surface_level(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let level = c.arg(1);
    let (w, h) = level_dims(field(c, t, "w"), field(c, t, "h"), level);
    let (pitch, bits) = level_bits(c, t, level);
    let (fmt, usage, pool) = (field(c, t, "fmt"), field(c, t, "usage"), field(c, t, "pool"));
    let s = new_surface(c, w, h, fmt, usage, pool);
    set_field(c, s, "bits0", bits);
    set_field(c, s, "pitch", pitch);
    set_field(c, s, "owner", t);
    out(c, c.arg(2), s);
    ret(c, D3D_OK)
}

const IDIRECT3DTEXTURE8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("SetPrivateData", 5, okn),
    ("GetPrivateData", 4, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("FreePrivateData", 2, okn),
    ("SetPriority", 2, |c| ret(c, 0)),
    ("GetPriority", 1, |c| ret(c, 0)),
    ("PreLoad", 1, |c| ret(c, 0)),
    ("GetType", 1, |c| ret(c, 3)),
    ("SetLOD", 2, |c| ret(c, 0)),
    ("GetLOD", 1, |c| ret(c, 0)),
    ("GetLevelCount", 1, |c| {
        let n = field(c, this(c), "levels");
        ret(c, n)
    }),
    ("GetLevelDesc", 3, tex_get_level_desc),
    ("GetSurfaceLevel", 3, tex_get_surface_level),
    ("LockRect", 5, tex_lock_rect),
    ("UnlockRect", 2, okn),
    ("AddDirtyRect", 2, okn),
];

fn surf_get_desc(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let p = c.arg(1);
    let (w, h, fmt, usage, pool) = (field(c, t, "w"), field(c, t, "h"), field(c, t, "fmt"), field(c, t, "usage"), field(c, t, "pool"));
    surface_desc(c, p, fmt, 1, usage, pool, w, h);
    ret(c, D3D_OK)
}

fn surf_lock_rect(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let out_rect = c.arg(1);
    let rect = c.arg(2);
    let (pitch, bits) = level_bits(c, t, 0);
    let mut p = bits;
    if rect != 0 {
        let l = c.host.read_u32(rect).unwrap_or(0);
        let top = c.host.read_u32(rect + 4).unwrap_or(0);
        p += top * pitch + l * format_bpp(field(c, t, "fmt"));
    }
    let _ = c.host.write_u32(out_rect, pitch);
    let _ = c.host.write_u32(out_rect + 4, p);
    ret(c, D3D_OK)
}

const IDIRECT3DSURFACE8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("SetPrivateData", 5, okn),
    ("GetPrivateData", 4, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("FreePrivateData", 2, okn),
    ("GetContainer", 3, |c| ret(c, E_NOINTERFACE)),
    ("GetDesc", 2, surf_get_desc),
    ("LockRect", 4, surf_lock_rect),
    ("UnlockRect", 1, okn),
];

fn buf_lock(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let off = c.arg(1);
    let pp = c.arg(3);
    let bits = field(c, t, "bits");
    out(c, pp, bits + off);
    ret(c, D3D_OK)
}

fn vb_get_desc(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let p = c.arg(1);
    let vals = [100u32, 6, field(c, t, "usage"), field(c, t, "pool"), field(c, t, "len"), field(c, t, "fvf")];
    for (i, v) in vals.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

fn ib_get_desc(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let p = c.arg(1);
    let vals = [field(c, t, "fvf"), 7, field(c, t, "usage"), field(c, t, "pool"), field(c, t, "len")];
    for (i, v) in vals.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, *v);
    }
    ret(c, D3D_OK)
}

const IDIRECT3DVERTEXBUFFER8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("SetPrivateData", 5, okn),
    ("GetPrivateData", 4, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("FreePrivateData", 2, okn),
    ("SetPriority", 2, |c| ret(c, 0)),
    ("GetPriority", 1, |c| ret(c, 0)),
    ("PreLoad", 1, |c| ret(c, 0)),
    ("GetType", 1, |c| ret(c, 6)),
    ("Lock", 5, buf_lock),
    ("Unlock", 1, okn),
    ("GetDesc", 2, vb_get_desc),
];

const IDIRECT3DINDEXBUFFER8: &[Method] = &[
    ("QueryInterface", 3, query_interface),
    ("AddRef", 1, add_ref),
    ("Release", 1, release),
    ("GetDevice", 2, res_get_device),
    ("SetPrivateData", 5, okn),
    ("GetPrivateData", 4, |c| ret(c, D3DERR_NOTAVAILABLE)),
    ("FreePrivateData", 2, okn),
    ("SetPriority", 2, |c| ret(c, 0)),
    ("GetPriority", 1, |c| ret(c, 0)),
    ("PreLoad", 1, |c| ret(c, 0)),
    ("GetType", 1, |c| ret(c, 7)),
    ("Lock", 5, buf_lock),
    ("Unlock", 1, okn),
    ("GetDesc", 2, ib_get_desc),
];

fn register_iface(r: &mut Registry, iface: &str, methods: &[Method]) {
    for (name, nargs, f) in methods {
        r.add_n(DLL, &format!("{iface}::{name}"), *nargs, *f);
    }
}

pub fn register(r: &mut Registry) {
    r.add(DLL, "Direct3DCreate8", |c| {
        let sdk = c.arg(0);
        let obj = new_object(c, "IDirect3D8", IDIRECT3D8, HashMap::new());
        c.trace(format!("(sdk {sdk}) = {obj:#x}"));
        Ok(Ret::stdcall(1, obj))
    });
    r.add(DLL, "ValidatePixelShader", |_c| Ok(Ret::cdecl(0)));
    r.add(DLL, "ValidateVertexShader", |_c| Ok(Ret::cdecl(0)));
    r.add(DLL, "DebugSetMute", |_c| Ok(Ret::stdcall(0, 0)));
    register_iface(r, "IDirect3D8", IDIRECT3D8);
    register_iface(r, "IDirect3DDevice8", IDIRECT3DDEVICE8);
    register_iface(r, "IDirect3DTexture8", IDIRECT3DTEXTURE8);
    register_iface(r, "IDirect3DSurface8", IDIRECT3DSURFACE8);
    register_iface(r, "IDirect3DVertexBuffer8", IDIRECT3DVERTEXBUFFER8);
    register_iface(r, "IDirect3DIndexBuffer8", IDIRECT3DINDEXBUFFER8);
}
