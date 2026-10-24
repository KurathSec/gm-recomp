//! Direct3D 11 and DXGI, as GameMaker Studio 2's runner uses them: the device, its
//! immediate context, buffers, 2D textures, views, state objects, shaders (DXBC,
//! translated by hle::dxbc) and a swap chain on the game's window.
//!
//! Draws become the Direct3D 9 renderer's [`DrawCall9`]: constant buffers fill its
//! float constants where the translated shader expects them, an input layout its
//! vertex elements, DXGI formats their D3DFORMAT equivalents, and the blend, depth and
//! rasterizer state objects its render states (their enumerations share Direct3D 9's
//! numbers). Textures are the Direct3D 9 renderer's too (same object fields, uploaded
//! by `d3d9::sync_texture`).
//!
//! The entry points are on demand and the interfaces are registered the first time a
//! device or factory is made, so no other game's stub addresses move.

use serde::{Deserialize, Serialize};
use super::com::{format_iid, E_NOINTERFACE, E_NOTIMPL, S_OK};
use super::gpu::{self, DrawCall9};
use super::{ApiFn, ApiResult, Ctx, Registry, Ret};
use crate::fasthash::HashMap;
use crate::mem::Guest;
use crate::process::{D3dObject, Process};

const DLL: &str = "d3d11.dll";
const DXGI_ERROR_NOT_FOUND: u32 = 0x887A_0002;
const E_INVALIDARG: u32 = 0x8007_0057;
const FEATURE_LEVEL_11_0: u32 = 0xB000;

const BIND_RENDER_TARGET: u32 = 0x20;
const BIND_DEPTH_STENCIL: u32 = 0x40;
const MAP_WRITE_DISCARD: u32 = 4;

type Method = (&'static str, u32, Option<ApiFn>);

/// What the context has bound, and the host-side halves of shaders and input layouts.
#[derive(Default, Serialize, Deserialize)]
pub struct State11 {
    pub device: u32,
    pub context: u32,
    pub backbuffer: u32,
    pub size: (u32, u32),
    pub window: u32,
    pub vs: u32,
    pub ps: u32,
    pub layout: u32,
    pub topology: u32,
    /// Per slot: buffer, stride, offset.
    pub vbs: [(u32, u32, u32); 8],
    /// Buffer, DXGI format, offset.
    pub ib: (u32, u32, u32),
    pub vs_cb: [u32; 14],
    pub ps_cb: [u32; 14],
    pub ps_srv: [u32; 16],
    pub ps_samplers: [u32; 16],
    pub rtv: u32,
    pub dsv: u32,
    pub blend: u32,
    pub blend_factor: [f32; 4],
    pub depth: u32,
    pub raster: u32,
    pub viewport: [f32; 6],
    pub scissor: [i32; 4],
    /// The current GPU target (texture object, 0: back buffer) and its size.
    pub target: (u32, u32, u32),
    /// The virtual time of the last Present.
    pub last_present_us: u64,
    pub shaders: HashMap<u32, super::dxbc::Shader>,
    /// Input layouts: (semantic name, index, DXGI format, slot, offset).
    pub layouts: HashMap<u32, Vec<(String, u32, u32, u32, u32)>>,
}

// ---- objects ----------------------------------------------------------------------------------

/// Every interface the objects answer QueryInterface for, by kind.
const IIDS: &[(&str, &[&str])] = &[
    ("ID3D11Device", &["{DB6F6DDB-AC77-4E88-8253-819DF9BBF140}", "{A04BFB29-08EF-43D6-A49C-A9BDBDCBE686}", "{4FFE0F0D-E87A-4EE4-B6D3-3A6DFCE5B7AE}"]),
    ("ID3D11DeviceContext", &["{C0BFA96C-E089-44FB-8EAF-26F8796190DA}", "{BB2C6FAA-B5FB-4082-8E6B-388B8CFA90E1}", "{1841E5C8-16B0-489B-BCC8-44CFB0D5DEAE}"]),
    ("ID3D11Buffer", &["{48570B85-D1EE-4FCD-A250-EB350722B037}", "{DC8E63F3-D12B-4952-B47B-5E45026A862D}", "{1841E5C8-16B0-489B-BCC8-44CFB0D5DEAE}"]),
    ("ID3D11Texture2D", &["{6F15AAF2-D208-4E89-9AB4-489535D34F9C}", "{DC8E63F3-D12B-4952-B47B-5E45026A862D}", "{1841E5C8-16B0-489B-BCC8-44CFB0D5DEAE}"]),
    ("ID3D11ShaderResourceView", &["{B0E06FE0-8192-4E1A-B1CA-36D7414710B2}", "{839D1216-BB2E-412B-B7F4-A9DBEBE08ED1}"]),
    ("ID3D11RenderTargetView", &["{DFDBA067-0B8D-4865-875B-D7B4516CC164}", "{839D1216-BB2E-412B-B7F4-A9DBEBE08ED1}"]),
    ("ID3D11DepthStencilView", &["{9FDAC92A-1876-48C3-AFAD-25B94F84A9B6}", "{839D1216-BB2E-412B-B7F4-A9DBEBE08ED1}"]),
    ("ID3D11VertexShader", &["{3B301D64-D678-4289-8897-22F8928B72F3}"]),
    ("ID3D11PixelShader", &["{EA82E40D-51DC-4F33-93D4-DB7C9125AE8C}"]),
    ("ID3D11InputLayout", &["{E4819DDC-4CF0-4025-BD26-5DE82A3E07B7}"]),
    ("ID3D11BlendState", &["{75B68FAA-347D-4159-8F45-A0640F01CD9A}"]),
    ("ID3D11DepthStencilState", &["{03823EFB-8D8F-4E1C-9AA2-F64BB2CBFDF1}"]),
    ("ID3D11RasterizerState", &["{9BB4AB81-AB1A-4D8F-B506-FC04200B6EE7}"]),
    ("ID3D11SamplerState", &["{DA6FEA51-564C-4487-9810-F0D0F9B4E3A5}"]),
    ("ID3D11Query", &["{D6C00747-87B7-425E-B84D-44D108560AFD}", "{4B35D0CD-1E15-4258-9C98-1B1333F6DD3B}"]),
    ("IDXGIFactory1", &["{770AAE78-F26F-4DBA-A829-253C83D1B387}", "{7B7166EC-21C7-44AE-B21A-C9AE321AE369}", "{AEC22FB8-76F3-4639-9BE0-28EB43A67A2E}"]),
    ("IDXGIAdapter1", &["{29038F61-3839-4626-91FD-086879011A05}", "{2411E7E1-12AC-4CCF-BD14-9798E8534DC0}", "{AEC22FB8-76F3-4639-9BE0-28EB43A67A2E}"]),
    ("IDXGIOutput", &["{AE02EEDB-C735-4690-8D52-5A8DC20213AA}", "{AEC22FB8-76F3-4639-9BE0-28EB43A67A2E}"]),
    ("IDXGISwapChain", &["{310D36A0-D2E7-4C0A-AA04-6A9D23B8886A}", "{3D3E0379-F9DE-4D58-BB6C-18D62992F1A6}", "{AEC22FB8-76F3-4639-9BE0-28EB43A67A2E}"]),
    ("IDXGIDevice1", &["{77DB970F-6276-48BA-BA28-070143B4392C}", "{54EC77FA-1377-44E6-8C32-88FD5F44C84C}", "{AEC22FB8-76F3-4639-9BE0-28EB43A67A2E}"]),
];
const IID_DXGI_DEVICE: [&str; 2] = ["{77DB970F-6276-48BA-BA28-070143B4392C}", "{54EC77FA-1377-44E6-8C32-88FD5F44C84C}"];
const IID_D3D11_DEVICE: &str = "{DB6F6DDB-AC77-4E88-8253-819DF9BBF140}";
const IUNKNOWN: &str = "{00000000-0000-0000-C000-000000000046}";

fn ret(c: &Ctx, v: u32) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), v))
}

fn this(c: &Ctx) -> u32 {
    c.arg(0)
}

fn alloc(c: &mut Ctx, size: u32) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    c.p.heap_alloc(g, heap, size, true)
}

/// Storage for a resource's contents: its own virtual-memory region.
fn alloc_storage(c: &mut Ctx, size: u32) -> u32 {
    let size = size.max(4).div_ceil(0x1000) * 0x1000;
    let Some(a) = c.p.vm.alloc(0, size, true, crate::mem::PAGE_READWRITE, false, "d3d11 resource") else { return 0 };
    if c.host.ensure_mapped(a, size).is_err() {
        return 0;
    }
    if !c.p.vm.last_fresh {
        let _ = c.host.fill(a, size, 0);
    }
    a
}

fn vtable(c: &mut Ctx, iface: &'static str, methods: &[Method]) -> u32 {
    if let Some(&v) = c.p.com_vtables.get(iface) {
        return v;
    }
    let ps = c.ptr_size();
    let v = alloc(c, methods.len() as u32 * ps);
    for (i, (name, _, _)) in methods.iter().enumerate() {
        let id = c.p.registry.lookup(DLL, &format!("{iface}::{name}")).expect("d3d11 method registered");
        c.write_ptr(v + ps * i as u32, Process::stub_addr(id));
    }
    c.p.com_vtables.insert(iface, v);
    v
}

fn new_object(c: &mut Ctx, iface: &'static str, data: HashMap<&'static str, u32>) -> u32 {
    let methods = methods_of(iface);
    let vt = vtable(c, iface, methods);
    let obj = alloc(c, 16);
    c.write_ptr(obj, vt);
    c.p.d3d_objects.insert(obj, D3dObject { kind: iface, refcount: 1, data });
    obj
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

/// Writes an interface pointer to an out parameter (8 bytes in a 64-bit process).
fn out(c: &mut Ctx, p: u32, v: u32) {
    if p != 0 {
        c.write_ptr(p, v);
    }
}

/// Writes a UINT/BOOL/enum out value.
fn out32(c: &mut Ctx, p: u32, v: u32) {
    if p != 0 {
        let _ = c.host.write_u32(p, v);
    }
}

fn rd(c: &Ctx, p: u32) -> u32 {
    c.host.read_u32(p).unwrap_or(0)
}

fn rdf(c: &Ctx, p: u32) -> f32 {
    f32::from_bits(rd(c, p))
}

fn read_iid(c: &Ctx, p: u32) -> String {
    let mut g = [0u8; 16];
    let _ = c.host.read(p, &mut g);
    format_iid(&g)
}

// ---- IUnknown ---------------------------------------------------------------------------------

fn query_interface(c: &mut Ctx) -> ApiResult {
    let (t, pp) = (this(c), c.arg(2));
    let want = read_iid(c, c.arg(1));
    let k = kind(c, t);
    // the device and its DXGI half are two objects
    let target = if k == "ID3D11Device" && IID_DXGI_DEVICE.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        field(c, t, "dxgi")
    } else if k == "IDXGIDevice1" && want.eq_ignore_ascii_case(IID_D3D11_DEVICE) {
        field(c, t, "d3d")
    } else if want == IUNKNOWN || IIDS.iter().any(|(name, iids)| *name == k && iids.iter().any(|i| i.eq_ignore_ascii_case(&want))) {
        t
    } else {
        0
    };
    if target != 0 {
        add_ref_obj(c, target);
        out(c, pp, target);
        return ret(c, S_OK);
    }
    c.p.log(format!("d3d11: {k}::QueryInterface({want}) not supported"));
    out(c, pp, 0);
    ret(c, E_NOINTERFACE)
}

fn add_ref(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    add_ref_obj(c, t);
    let n = c.p.d3d_objects.get(&t).map(|o| o.refcount).unwrap_or(1);
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
    // the device, context, factory and back buffer live as long as the process
    if n == 0 && !matches!(kind(c, t), "ID3D11Device" | "ID3D11DeviceContext" | "IDXGIFactory1" | "IDXGIDevice1" | "IDXGISwapChain") && field(c, t, "backbuffer") == 0 {
        destroy(c, t);
    }
    ret(c, n)
}

fn destroy(c: &mut Ctx, t: u32) {
    c.p.pending_uploads.retain(|&(x, _)| x != t);
    let Some(o) = c.p.d3d_objects.remove(&t) else { return };
    if o.data.get("gpu").copied().unwrap_or(0) != 0 {
        c.p.gpu.texture_delete(t);
    }
    if matches!(o.kind, "ID3D11VertexShader" | "ID3D11PixelShader") {
        if c.p.d3d11.shaders.remove(&t).is_some() {
            c.p.gpu.shader_delete(t);
        }
    }
    if o.kind == "ID3D11InputLayout" {
        c.p.d3d11.layouts.remove(&t);
    }
    for (k, v) in &o.data {
        if k.starts_with("bits") && *v != 0 {
            c.p.vm.release(*v);
        }
    }
}

fn ok_s(c: &mut Ctx) -> ApiResult {
    ret(c, S_OK)
}

fn ok_void(c: &mut Ctx) -> ApiResult {
    ret(c, 0)
}

fn not_impl(c: &mut Ctx) -> ApiResult {
    ret(c, E_NOTIMPL)
}

// ---- formats ----------------------------------------------------------------------------------

/// The D3DFORMAT a DXGI format's texels match (0: none).
fn d3d_format(dxgi: u32) -> u32 {
    match dxgi {
        28 | 29 | 27 => 32, // R8G8B8A8 -> A8B8G8R8 (R, G, B, A in memory)
        87 | 91 | 90 => 21, // B8G8R8A8 -> A8R8G8B8
        88 | 93 | 92 => 22, // B8G8R8X8 -> X8R8G8B8
        65 => 28,           // A8
        61 => 50,           // R8 (as L8)
        10 => 113,          // R16G16B16A16_FLOAT
        2 => 116,           // R32G32B32A32_FLOAT
        45 | 44 => 75,      // D24_UNORM_S8_UINT -> D24S8
        40 | 39 => 71,      // D32_FLOAT -> D32F_LOCKABLE (depth)
        55 => 80,           // D16_UNORM
        71 | 72 | 70 => 0x3154_5844, // BC1 -> DXT1
        74 | 75 | 73 => 0x3354_5844, // BC2 -> DXT3
        77 | 78 | 76 => 0x3554_5844, // BC3 -> DXT5
        _ => 0,
    }
}

/// The D3DDECLTYPE of a vertex element's DXGI format.
fn decl_type(dxgi: u32) -> Option<u32> {
    Some(match dxgi {
        41 => 0,       // R32_FLOAT
        16 => 1,       // R32G32_FLOAT
        6 => 2,        // R32G32B32_FLOAT
        2 => 3,        // R32G32B32A32_FLOAT
        87 => 4,       // B8G8R8A8_UNORM: D3DCOLOR
        30 => 5,       // R8G8B8A8_UINT: UBYTE4
        38 => 6,       // R16G16_SINT: SHORT2
        14 => 7,       // R16G16B16A16_SINT: SHORT4
        28 => 8,       // R8G8B8A8_UNORM: UBYTE4N
        37 => 9,       // R16G16_SNORM: SHORT2N
        13 => 10,      // R16G16B16A16_SNORM: SHORT4N
        35 => 11,      // R16G16_UNORM: USHORT2N
        11 => 12,      // R16G16B16A16_UNORM: USHORT4N
        34 => 15,      // R16G16_FLOAT
        10 => 16,      // R16G16B16A16_FLOAT
        _ => return None,
    })
}

fn decl_size(dxgi: u32) -> u32 {
    match dxgi {
        41 | 87 | 30 | 38 | 28 | 37 | 35 | 34 => 4,
        16 | 14 | 13 | 11 | 10 => 8,
        6 => 12,
        2 => 16,
        _ => 4,
    }
}

/// A semantic's Direct3D 9 usage (`a_<USAGE><index>` in translated shaders).
fn usage(semantic: &str) -> u32 {
    const USAGES: [&str; 11] = ["POSITION", "BLENDWEIGHT", "BLENDINDICES", "NORMAL", "PSIZE", "TEXCOORD", "TANGENT", "BINORMAL", "TESSFACTOR", "POSITIONT", "COLOR"];
    USAGES.iter().position(|u| u.eq_ignore_ascii_case(semantic)).unwrap_or(5) as u32
}

fn level_dims(w: u32, h: u32, level: u32) -> (u32, u32) {
    ((w >> level).max(1), (h >> level).max(1))
}

fn level_key(level: u32) -> &'static str {
    const KEYS: [&str; 16] = ["bits0", "bits1", "bits2", "bits3", "bits4", "bits5", "bits6", "bits7", "bits8", "bits9", "bits10", "bits11", "bits12", "bits13", "bits14", "bits15"];
    KEYS[(level as usize).min(15)]
}

fn level_pitch(fmt: u32, w: u32) -> u32 {
    match fmt {
        0x3154_5844 => w.div_ceil(4) * 8,
        0x3254_5844..=0x3554_5844 => w.div_ceil(4) * 16,
        _ => w * super::d3d9::format_bpp(fmt),
    }
}

fn level_rows(fmt: u32, h: u32) -> u32 {
    if (0x3154_5844..=0x3554_5844).contains(&fmt) { h.div_ceil(4) } else { h }
}

fn mark_dirty(c: &mut Ctx, tex: u32, level: u32) {
    let d = field(c, tex, "dirty") | (1 << level.min(15));
    set_field(c, tex, "dirty", d);
    if !c.p.pending_uploads.contains(&(tex, 9)) {
        c.p.pending_uploads.push((tex, 9));
    }
}

// ---- entry points ---------------------------------------------------------------------------

/// D3D11CreateDevice(adapter, driver, software, flags, levels, nlevels, sdk, ppDevice,
/// pLevel, ppContext), and with a swap chain (desc, ppSwapChain before ppDevice).
fn create_device(c: &mut Ctx) -> ApiResult {
    ensure_registered(c);
    let and_swap_chain = c.name().contains("AndSwapChain");
    let (levels, nlevels) = (c.arg(4), c.arg(5));
    let (sc_desc, pp_sc, pp_dev, p_level, pp_ctx) = if and_swap_chain { (c.arg(7), c.arg(8), c.arg(9), c.arg(10), c.arg(11)) } else { (0, 0, c.arg(7), c.arg(8), c.arg(9)) };
    // the first level asked for at or under 11_0
    let mut level = FEATURE_LEVEL_11_0;
    if levels != 0 {
        // (an array of D3D_FEATURE_LEVEL: 4-byte enums in both layouts)
        for i in 0..nlevels.min(16) {
            let l = rd(c, levels + 4 * i);
            if l <= FEATURE_LEVEL_11_0 {
                level = l;
                break;
            }
        }
    }
    let dev = device(c);
    set_field(c, dev, "level", level);
    out32(c, p_level, level);
    if pp_dev != 0 {
        add_ref_obj(c, dev);
        out(c, pp_dev, dev);
    }
    if pp_ctx != 0 {
        let ctx = c.p.d3d11.context;
        add_ref_obj(c, ctx);
        out(c, pp_ctx, ctx);
    }
    if and_swap_chain && pp_sc != 0 {
        let sc = new_swap_chain(c, dev, sc_desc);
        out(c, pp_sc, sc);
    }
    c.trace(format!("(level {level:#x}) = {dev:#x}"));
    Ok(Ret::stdcall(if and_swap_chain { 12 } else { 10 }, S_OK))
}

/// The device (one per process), its immediate context and its DXGI half.
fn device(c: &mut Ctx) -> u32 {
    if c.p.d3d11.device != 0 {
        return c.p.d3d11.device;
    }
    let dev = new_object(c, "ID3D11Device", HashMap::default());
    let ctx = new_object(c, "ID3D11DeviceContext", HashMap::from_iter([("device", dev)]));
    let dxgi = new_object(c, "IDXGIDevice1", HashMap::from_iter([("d3d", dev)]));
    set_field(c, dev, "dxgi", dxgi);
    c.p.d3d11.device = dev;
    c.p.d3d11.context = ctx;
    c.p.d3d11.topology = 4;
    c.p.d3d11.blend_factor = [1.0; 4];
    dev
}

/// CreateDXGIFactory / CreateDXGIFactory1 (riid, ppFactory) and CreateDXGIFactory2
/// (flags, riid, ppFactory).
fn create_factory(c: &mut Ctx) -> ApiResult {
    ensure_registered(c);
    let two = c.name().ends_with('2');
    let pp = if two { c.arg(2) } else { c.arg(1) };
    let f = factory(c);
    add_ref_obj(c, f);
    out(c, pp, f);
    Ok(Ret::stdcall(if two { 3 } else { 2 }, S_OK))
}

fn factory(c: &mut Ctx) -> u32 {
    if let Some((&f, _)) = c.p.d3d_objects.iter().find(|(_, o)| o.kind == "IDXGIFactory1") {
        return f;
    }
    let f = new_object(c, "IDXGIFactory1", HashMap::default());
    let adapter = new_object(c, "IDXGIAdapter1", HashMap::from_iter([("factory", f)]));
    let output = new_object(c, "IDXGIOutput", HashMap::from_iter([("adapter", adapter)]));
    set_field(c, f, "adapter", adapter);
    set_field(c, adapter, "output", output);
    f
}

// ---- IDXGIFactory -----------------------------------------------------------------------------

fn factory_enum_adapters(c: &mut Ctx) -> ApiResult {
    let (t, i, pp) = (this(c), c.arg(1), c.arg(2));
    if i != 0 {
        out(c, pp, 0);
        return ret(c, DXGI_ERROR_NOT_FOUND);
    }
    let a = field(c, t, "adapter");
    add_ref_obj(c, a);
    out(c, pp, a);
    ret(c, S_OK)
}

fn factory_create_swap_chain(c: &mut Ctx) -> ApiResult {
    let (desc, pp) = (c.arg(2), c.arg(3));
    let dev = device(c);
    let sc = new_swap_chain(c, dev, desc);
    out(c, pp, sc);
    ret(c, S_OK)
}

/// A swap chain from a DXGI_SWAP_CHAIN_DESC: its back buffer is the GPU's (target 0).
/// DXGI_SWAP_CHAIN_DESC offsets of BufferCount, OutputWindow and Windowed (the window
/// handle is 8-byte aligned in a 64-bit process), and its size.
fn swap_chain_desc_layout(c: &Ctx) -> (u32, u32, u32, u32) {
    if c.x64() { (40, 48, 56, 72) } else { (40, 44, 48, 60) }
}

fn new_swap_chain(c: &mut Ctx, dev: u32, desc: u32) -> u32 {
    let (count_at, hwnd_at, windowed_at, _) = swap_chain_desc_layout(c);
    let (mut w, mut h, fmt) = (rd(c, desc), rd(c, desc + 4), rd(c, desc + 16));
    let hwnd = rd(c, desc + hwnd_at);
    if w == 0 || h == 0 {
        let (ww, wh) = c.p.windows.get(&hwnd).map(|win| (win.w.max(1) as u32, win.h.max(1) as u32)).unwrap_or((800, 600));
        (w, h) = (ww, wh);
    }
    let sc = new_object(c, "IDXGISwapChain", HashMap::from_iter([("device", dev), ("w", w), ("h", h), ("fmt", fmt), ("hwnd", hwnd), ("count", rd(c, desc + count_at)), ("windowed", rd(c, desc + windowed_at))]));
    let bb = new_object(c, "ID3D11Texture2D", HashMap::from_iter([("w", w), ("h", h), ("fmt", 22), ("dxgi", fmt), ("levels", 1), ("bind", BIND_RENDER_TARGET), ("backbuffer", 1)]));
    set_field(c, sc, "buffer", bb);
    let st = &mut c.p.d3d11;
    st.backbuffer = bb;
    st.size = (w, h);
    st.window = hwnd;
    st.viewport = [0.0, 0.0, w as f32, h as f32, 0.0, 1.0];
    st.scissor = [0, 0, w as i32, h as i32];
    st.target = (0, w, h);
    c.p.d3d.window = hwnd;
    c.p.d3d.backbuffer = (w, h);
    c.p.gpu.resize(w, h);
    c.p.gpu.set_target(0, w, h);
    c.trace(format!("swap chain {w}x{h} fmt {fmt} hwnd {hwnd:#x}"));
    sc
}

fn object_get_parent(c: &mut Ctx) -> ApiResult {
    let (t, pp) = (this(c), c.arg(2));
    let parent = match kind(c, t) {
        "IDXGIAdapter1" => field(c, t, "factory"),
        "IDXGIOutput" => field(c, t, "adapter"),
        "IDXGIDevice1" => {
            let f = factory(c);
            field(c, f, "adapter")
        }
        "IDXGISwapChain" => factory(c),
        _ => 0,
    };
    if parent == 0 {
        out(c, pp, 0);
        return ret(c, E_NOINTERFACE);
    }
    add_ref_obj(c, parent);
    out(c, pp, parent);
    ret(c, S_OK)
}

// ---- IDXGIAdapter / IDXGIOutput ---------------------------------------------------------------

fn adapter_enum_outputs(c: &mut Ctx) -> ApiResult {
    let (t, i, pp) = (this(c), c.arg(1), c.arg(2));
    if i != 0 {
        out(c, pp, 0);
        return ret(c, DXGI_ERROR_NOT_FOUND);
    }
    let o = field(c, t, "output");
    add_ref_obj(c, o);
    out(c, pp, o);
    ret(c, S_OK)
}

/// DXGI_ADAPTER_DESC(1): a Description (WCHAR[128]), ids, memory sizes (SIZE_T), the LUID
/// and (DESC1) flags: 292 or 296 bytes, 304 or 312 in a 64-bit process.
fn adapter_get_desc(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let one = c.name().ends_with("GetDesc1");
    let size = match (c.x64(), one) {
        (false, false) => 292,
        (false, true) => 296,
        (true, false) => 304,
        (true, true) => 312,
    };
    let _ = c.host.write(p, &vec![0u8; size]);
    let name: Vec<u8> = "WebGL".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let _ = c.host.write(p, &name);
    let _ = c.host.write_u32(p + 256, 0x10DE); // vendor
    let _ = c.host.write_u32(p + 260, 1);
    let ps = c.ptr_size();
    let _ = c.host.write_u32(p + 272, 512 << 20); // dedicated video memory
    let _ = c.host.write_u32(p + 272 + 2 * ps, 512 << 20); // shared system memory
    ret(c, S_OK)
}

/// The window's size, else 1920x1080: the one display mode.
fn display_size(c: &Ctx) -> (u32, u32) {
    let _ = c;
    (1920, 1080)
}

/// DXGI_OUTPUT_DESC: DeviceName (WCHAR[32]), DesktopCoordinates, AttachedToDesktop,
/// Rotation, Monitor: 92 bytes, 96 in a 64-bit process.
fn output_get_desc(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let size = if c.x64() { 96 } else { 92 };
    let _ = c.host.write(p, &vec![0u8; size]);
    let name: Vec<u8> = "\\\\.\\DISPLAY1".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let _ = c.host.write(p, &name);
    let (w, h) = display_size(c);
    let _ = c.host.write_u32(p + 72, w);
    let _ = c.host.write_u32(p + 76, h);
    let _ = c.host.write_u32(p + 80, 1); // attached to the desktop
    let _ = c.host.write_u32(p + 84, 1); // DXGI_MODE_ROTATION_IDENTITY
    let _ = c.host.write_u32(p + 88, 0x0001_0001); // monitor
    ret(c, S_OK)
}

/// GetDisplayModeList(format, flags, pNum, pModes): a few common modes at 60 Hz.
fn output_get_display_mode_list(c: &mut Ctx) -> ApiResult {
    const MODES: [(u32, u32); 5] = [(800, 600), (1024, 768), (1280, 720), (1600, 900), (1920, 1080)];
    let (fmt, pn, p) = (c.arg(1), c.arg(3), c.arg(4));
    if p == 0 {
        out32(c, pn, MODES.len() as u32);
        return ret(c, S_OK);
    }
    let n = rd(c, pn).min(MODES.len() as u32);
    for (i, (w, h)) in MODES.iter().take(n as usize).enumerate() {
        let m = p + 28 * i as u32;
        for (k, v) in [*w, *h, 60, 1, fmt, 0, 0].into_iter().enumerate() {
            let _ = c.host.write_u32(m + 4 * k as u32, v);
        }
    }
    out32(c, pn, n);
    ret(c, S_OK)
}

fn output_find_closest_mode(c: &mut Ctx) -> ApiResult {
    let (want, p) = (c.arg(1), c.arg(2));
    let mut m = [0u8; 28];
    let _ = c.host.read(want, &mut m);
    let mut words: Vec<u32> = m.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    if words[0] == 0 || words[1] == 0 {
        (words[0], words[1]) = display_size(c);
    }
    (words[2], words[3]) = (60, 1);
    if words[4] == 0 {
        words[4] = 28;
    }
    for (k, v) in words.iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * k as u32, *v);
    }
    ret(c, S_OK)
}

// ---- IDXGISwapChain -------------------------------------------------------------------------

/// A display's frame (µs): what a runner that does not wait for its next frame gets.
const DISPLAY_FRAME_US: u64 = 1_000_000 / 60;

fn sc_present(c: &mut Ctx) -> ApiResult {
    // A runner that does not wait between frames (a game speed above what a display
    // shows, such as 9999) gets a display's frame, as vsync would give it: its
    // delta_time then follows what the player sees instead of a tenth of a millisecond.
    let since = c.p.clock_us.saturating_sub(c.p.d3d11.last_present_us);
    if c.p.d3d11.last_present_us != 0 && since < DISPLAY_FRAME_US / 2 {
        c.advance_clock(DISPLAY_FRAME_US - since);
    }
    c.p.d3d11.last_present_us = c.p.clock_us;
    c.p.frames_presented += 1;
    let n = c.p.frames_presented;
    let draws = c.p.draw_calls;
    c.p.draw_calls = 0;
    c.p.log(format!("Present #{n} ({draws} draw calls, t={}ms, {} µs)", c.p.now_ms(), c.p.clock_us));
    let (w, h) = c.p.d3d11.size;
    let win = c.p.d3d11.window;
    let (dw, dh) = c.p.windows.get(&win).map(|w| (w.w.max(1) as u32, w.h.max(1) as u32)).unwrap_or((w, h));
    super::d3d8::present_frame(c, [0, 0, w as i32, h as i32], dw, dh)?;
    ret(c, S_OK)
}

fn sc_get_buffer(c: &mut Ctx) -> ApiResult {
    let (t, pp) = (this(c), c.arg(3));
    let b = field(c, t, "buffer");
    add_ref_obj(c, b);
    out(c, pp, b);
    ret(c, S_OK)
}

fn sc_get_desc(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let (count_at, hwnd_at, windowed_at, size) = swap_chain_desc_layout(c);
    let _ = c.host.write(p, &vec![0u8; size as usize]);
    for (off, key) in [(0, "w"), (4, "h"), (16, "fmt"), (count_at, "count"), (hwnd_at, "hwnd"), (windowed_at, "windowed")] {
        let v = field(c, t, key);
        let _ = c.host.write_u32(p + off, v);
    }
    let _ = c.host.write_u32(p + 8, 60);
    let _ = c.host.write_u32(p + 12, 1);
    let _ = c.host.write_u32(p + 28, 1);
    let _ = c.host.write_u32(p + 36, 0x20); // DXGI_USAGE_RENDER_TARGET_OUTPUT
    ret(c, S_OK)
}

/// ResizeBuffers(count, w, h, format, flags): the back buffer takes the new size (0: the
/// window's).
fn sc_resize_buffers(c: &mut Ctx) -> ApiResult {
    let t = this(c);
    let (mut w, mut h) = (c.arg(2), c.arg(3));
    if w == 0 || h == 0 {
        let hwnd = field(c, t, "hwnd");
        (w, h) = c.p.windows.get(&hwnd).map(|win| (win.w.max(1) as u32, win.h.max(1) as u32)).unwrap_or(c.p.d3d11.size);
    }
    set_field(c, t, "w", w);
    set_field(c, t, "h", h);
    let bb = field(c, t, "buffer");
    set_field(c, bb, "w", w);
    set_field(c, bb, "h", h);
    c.p.d3d11.size = (w, h);
    c.p.d3d.backbuffer = (w, h);
    c.p.gpu.resize(w, h);
    if c.p.d3d11.target.0 == 0 {
        c.p.d3d11.target = (0, w, h);
        c.p.gpu.set_target(0, w, h);
    }
    ret(c, S_OK)
}

fn sc_get_fullscreen_state(c: &mut Ctx) -> ApiResult {
    let (pfs, pout) = (c.arg(1), c.arg(2));
    out32(c, pfs, 0);
    out(c, pout, 0);
    ret(c, S_OK)
}

fn sc_get_containing_output(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let f = factory(c);
    let a = field(c, f, "adapter");
    let o = field(c, a, "output");
    add_ref_obj(c, o);
    out(c, pp, o);
    ret(c, S_OK)
}

fn sc_get_device(c: &mut Ctx) -> ApiResult {
    let (t, pp) = (this(c), c.arg(2));
    let want = read_iid(c, c.arg(1));
    let dev = field(c, t, "device");
    let obj = if IID_DXGI_DEVICE.iter().any(|i| i.eq_ignore_ascii_case(&want)) { field(c, dev, "dxgi") } else { dev };
    add_ref_obj(c, obj);
    out(c, pp, obj);
    ret(c, S_OK)
}

// ---- IDXGIDevice ----------------------------------------------------------------------------

fn dxgi_device_get_adapter(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let f = factory(c);
    let a = field(c, f, "adapter");
    add_ref_obj(c, a);
    out(c, pp, a);
    ret(c, S_OK)
}

// ---- ID3D11Device: resources ------------------------------------------------------------------

/// CreateBuffer(desc, initial, ppBuffer): D3D11_BUFFER_DESC {ByteWidth, Usage, BindFlags,
/// CPUAccessFlags, MiscFlags, StructureByteStride}.
fn dev_create_buffer(c: &mut Ctx) -> ApiResult {
    let (desc, init, pp) = (c.arg(1), c.arg(2), c.arg(3));
    let (size, usage, bind) = (rd(c, desc), rd(c, desc + 4), rd(c, desc + 8));
    let bits = alloc_storage(c, size);
    if bits == 0 {
        out(c, pp, 0);
        return ret(c, E_INVALIDARG);
    }
    if init != 0 {
        let src = rd(c, init);
        let mut b = vec![0u8; size as usize];
        let _ = c.host.read(src, &mut b);
        let _ = c.host.write(bits, &b);
    }
    let buf = new_object(c, "ID3D11Buffer", HashMap::from_iter([("size", size), ("usage", usage), ("bind", bind), ("bits0", bits), ("desc0", rd(c, desc + 12)), ("desc1", rd(c, desc + 16))]));
    out(c, pp, buf);
    ret(c, S_OK)
}

fn buffer_get_desc(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    for (off, key) in [(0, "size"), (4, "usage"), (8, "bind"), (12, "desc0"), (16, "desc1")] {
        let v = field(c, t, key);
        let _ = c.host.write_u32(p + off, v);
    }
    let _ = c.host.write_u32(p + 20, 0);
    ret(c, S_OK)
}

/// CreateTexture2D(desc, initial, ppTexture): D3D11_TEXTURE2D_DESC {Width, Height,
/// MipLevels, ArraySize, Format, SampleDesc {Count, Quality}, Usage, BindFlags,
/// CPUAccessFlags, MiscFlags}; initial data is one D3D11_SUBRESOURCE_DATA {pSysMem,
/// SysMemPitch, SysMemSlicePitch} per level.
fn dev_create_texture2d(c: &mut Ctx) -> ApiResult {
    let (desc, init, pp) = (c.arg(1), c.arg(2), c.arg(3));
    let w: Vec<u32> = (0..11).map(|i| rd(c, desc + 4 * i)).collect();
    let (width, height, mut levels, dxgi, usage, bind) = (w[0].max(1), w[1].max(1), w[2], w[4], w[7], w[8]);
    let fmt = d3d_format(dxgi);
    if fmt == 0 {
        c.p.log(format!("d3d11: CreateTexture2D: DXGI format {dxgi} not supported"));
        out(c, pp, 0);
        return ret(c, E_INVALIDARG);
    }
    if levels == 0 {
        levels = 32 - width.max(height).leading_zeros();
    }
    let levels = levels.min(16);
    let depth = bind & BIND_DEPTH_STENCIL != 0;
    let mut data = HashMap::from_iter([("w", width), ("h", height), ("fmt", fmt), ("dxgi", dxgi), ("levels", levels), ("usage", usage), ("bind", bind), ("cpu", w[9]), ("misc", w[10])]);
    // pixel storage per level (render targets too: copies and reads go through it)
    if !depth {
        for level in 0..levels {
            let (lw, lh) = level_dims(width, height, level);
            let bits = alloc_storage(c, level_pitch(fmt, lw) * level_rows(fmt, lh));
            data.insert(level_key(level), bits);
        }
    }
    let tex = new_object(c, "ID3D11Texture2D", data);
    // on the GPU: what can be drawn from or into (not staging copies)
    if !depth && usage != 3 {
        c.p.gpu.texture_create(tex, width, height, levels, fmt, bind & BIND_RENDER_TARGET != 0);
        set_field(c, tex, "gpu", 1);
    }
    if init != 0 && !depth {
        // (D3D11_SUBRESOURCE_DATA: 16 bytes, the pitch after an 8-byte pointer, in a
        // 64-bit process)
        let (stride, pitch_at) = if c.x64() { (16, 8) } else { (12, 4) };
        for level in 0..levels {
            let (src, pitch) = (rd(c, init + stride * level), rd(c, init + stride * level + pitch_at));
            if src != 0 {
                write_level(c, tex, level, src, pitch, None);
            }
        }
    }
    c.trace(format!("({width}x{height} levels {levels} dxgi {dxgi} usage {usage} bind {bind:#x}) = {tex:#x}"));
    out(c, pp, tex);
    ret(c, S_OK)
}

/// Copies rows from guest memory at `src` (`pitch` bytes apart) into a texture level,
/// into the box (left, top, right, bottom) or the whole level, and marks it for upload.
fn write_level(c: &mut Ctx, tex: u32, level: u32, src: u32, pitch: u32, area: Option<(u32, u32, u32, u32)>) {
    let fmt = field(c, tex, "fmt");
    let (lw, lh) = level_dims(field(c, tex, "w"), field(c, tex, "h"), level);
    let bits = field(c, tex, level_key(level));
    if bits == 0 {
        return;
    }
    let dst_pitch = level_pitch(fmt, lw);
    let (x0, y0, x1, y1) = area.unwrap_or((0, 0, lw, lh));
    let (x1, y1) = (x1.min(lw), y1.min(lh));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let block = (0x3154_5844..=0x3554_5844).contains(&fmt);
    let (bx0, by0, bx1, by1) = if block { (x0 / 4, y0 / 4, x1.div_ceil(4), y1.div_ceil(4)) } else { (x0, y0, x1, y1) };
    let unit = if block { level_pitch(fmt, 4) } else { super::d3d9::format_bpp(fmt) };
    let row_bytes = (bx1 - bx0) * unit;
    let mut row = vec![0u8; row_bytes as usize];
    for r in 0..(by1 - by0) {
        let _ = c.host.read(src + r * pitch, &mut row);
        let _ = c.host.write(bits + (by0 + r) * dst_pitch + bx0 * unit, &row);
    }
    if field(c, tex, "gpu") != 0 {
        mark_dirty(c, tex, level);
    }
}

fn texture_get_desc(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let vals = [field(c, t, "w"), field(c, t, "h"), field(c, t, "levels"), 1, field(c, t, "dxgi"), 1, 0, field(c, t, "usage"), field(c, t, "bind"), field(c, t, "cpu"), field(c, t, "misc")];
    for (i, v) in vals.into_iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * i as u32, v);
    }
    ret(c, S_OK)
}

fn resource_get_type(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let ty = if kind(c, t) == "ID3D11Buffer" { 1 } else { 3 }; // D3D11_RESOURCE_DIMENSION
    out32(c, p, ty);
    ret(c, 0)
}

/// Shader resource, render target and depth stencil views: the resource they show.
fn dev_create_view(c: &mut Ctx) -> ApiResult {
    let (res, pp) = (c.arg(1), c.arg(3));
    let iface = match c.name().rsplit("::").next().unwrap_or("") {
        "CreateShaderResourceView" => "ID3D11ShaderResourceView",
        "CreateRenderTargetView" => "ID3D11RenderTargetView",
        _ => "ID3D11DepthStencilView",
    };
    add_ref_obj(c, res);
    let v = new_object(c, iface, HashMap::from_iter([("res", res)]));
    out(c, pp, v);
    ret(c, S_OK)
}

fn view_get_resource(c: &mut Ctx) -> ApiResult {
    let (t, pp) = (this(c), c.arg(1));
    let r = field(c, t, "res");
    add_ref_obj(c, r);
    out(c, pp, r);
    ret(c, 0)
}

// ---- ID3D11Device: shaders and states ---------------------------------------------------------

fn read_bytes(c: &Ctx, p: u32, n: u32) -> Vec<u8> {
    let mut b = vec![0u8; n.min(1 << 20) as usize];
    let _ = c.host.read(p, &mut b);
    b
}

/// CreateVertexShader / CreatePixelShader(bytecode, length, linkage, ppShader).
fn dev_create_shader(c: &mut Ctx) -> ApiResult {
    let (code, len, pp) = (c.arg(1), c.arg(2), c.arg(4));
    let vertex = c.name().ends_with("CreateVertexShader");
    let iface = if vertex { "ID3D11VertexShader" } else { "ID3D11PixelShader" };
    let obj = new_object(c, iface, HashMap::default());
    let bytes = read_bytes(c, code, len);
    match super::dxbc::translate(&bytes) {
        Ok(s) if s.vertex == vertex => {
            c.p.gpu.shader_create(obj, vertex, &s.glsl);
            c.p.d3d11.shaders.insert(obj, s);
        }
        Ok(_) => c.p.log(format!("d3d11: {iface} {obj:#x}: the bytecode is of the other stage")),
        Err(e) => c.p.log(format!("d3d11: {iface} {obj:#x} ({len} bytes) not translated: {e}")),
    }
    out(c, pp, obj);
    ret(c, S_OK)
}

/// CreateInputLayout(descs, count, bytecode, length, ppLayout): D3D11_INPUT_ELEMENT_DESC
/// {SemanticName, SemanticIndex, Format, InputSlot, AlignedByteOffset, InputSlotClass,
/// InstanceDataStepRate}.
fn dev_create_input_layout(c: &mut Ctx) -> ApiResult {
    let (descs, n, pp) = (c.arg(1), c.arg(2), c.arg(5));
    let mut els = Vec::new();
    let mut next = [0u32; 16];
    // (32 bytes, the fields after an 8-byte SemanticName, in a 64-bit process)
    let (stride, f) = if c.x64() { (32, 8) } else { (28, 4) };
    for i in 0..n.min(16) {
        let d = descs + stride * i;
        let name = c.host.read_cstr(rd(c, d)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let (index, fmt, slot, mut offset) = (rd(c, d + f), rd(c, d + f + 4), rd(c, d + f + 8).min(15), rd(c, d + f + 12));
        if offset == 0xFFFF_FFFF {
            offset = next[slot as usize];
        }
        next[slot as usize] = offset + decl_size(fmt);
        els.push((name, index, fmt, slot, offset));
    }
    let obj = new_object(c, "ID3D11InputLayout", HashMap::default());
    c.p.d3d11.layouts.insert(obj, els);
    out(c, pp, obj);
    ret(c, S_OK)
}

/// The state objects keep their descriptions' words.
fn dev_create_state(c: &mut Ctx) -> ApiResult {
    let (desc, pp) = (c.arg(1), c.arg(2));
    let (iface, words) = match c.name().rsplit("::").next().unwrap_or("") {
        "CreateBlendState" => ("ID3D11BlendState", 2 + 8 * 8),
        "CreateDepthStencilState" => ("ID3D11DepthStencilState", 13),
        "CreateRasterizerState" => ("ID3D11RasterizerState", 10),
        _ => ("ID3D11SamplerState", 13),
    };
    let obj = new_object(c, iface, HashMap::default());
    let ws: Vec<u32> = (0..words).map(|i| rd(c, desc + 4 * i)).collect();
    let addr = alloc(c, 4 * words);
    for (i, v) in ws.iter().enumerate() {
        let _ = c.host.write_u32(addr + 4 * i as u32, *v);
    }
    set_field(c, obj, "desc", addr);
    set_field(c, obj, "words", words);
    out(c, pp, obj);
    ret(c, S_OK)
}

fn state_get_desc(c: &mut Ctx) -> ApiResult {
    let (t, p) = (this(c), c.arg(1));
    let (addr, words) = (field(c, t, "desc"), field(c, t, "words"));
    let b = read_bytes(c, addr, 4 * words);
    let _ = c.host.write(p, &b);
    ret(c, 0)
}

/// A state object's description word `i` (0 for no object: the defaults are handled by
/// the callers).
fn state_word(c: &Ctx, obj: u32, i: u32) -> u32 {
    if obj == 0 {
        return 0;
    }
    rd(c, field(c, obj, "desc") + 4 * i)
}

fn dev_create_query(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(2);
    let q = new_object(c, "ID3D11Query", HashMap::default());
    out(c, pp, q);
    ret(c, S_OK)
}

fn dev_check_format_support(c: &mut Ctx) -> ApiResult {
    let (fmt, p) = (c.arg(1), c.arg(2));
    // texture 2D, shader sample, render target, blendable, display... (D3D11_FORMAT_SUPPORT)
    let bits = if d3d_format(fmt) != 0 || decl_type(fmt).is_some() || matches!(fmt, 42 | 57) { 0x0FFF_FFFF } else { 0 };
    out32(c, p, bits);
    ret(c, if bits != 0 { S_OK } else { E_INVALIDARG })
}

fn dev_check_multisample(c: &mut Ctx) -> ApiResult {
    let (count, p) = (c.arg(2), c.arg(3));
    out32(c, p, (count <= 1) as u32);
    ret(c, S_OK)
}

fn dev_check_feature_support(c: &mut Ctx) -> ApiResult {
    let (p, size) = (c.arg(2), c.arg(3));
    let _ = c.host.write(p, &vec![0u8; size.min(256) as usize]);
    ret(c, S_OK)
}

fn dev_get_feature_level(c: &mut Ctx) -> ApiResult {
    let l = field(c, this(c), "level");
    ret(c, if l != 0 { l } else { FEATURE_LEVEL_11_0 })
}

fn dev_get_immediate_context(c: &mut Ctx) -> ApiResult {
    let ctx = c.p.d3d11.context;
    add_ref_obj(c, ctx);
    out(c, c.arg(1), ctx);
    ret(c, 0)
}

fn child_get_device(c: &mut Ctx) -> ApiResult {
    let dev = c.p.d3d11.device;
    add_ref_obj(c, dev);
    out(c, c.arg(1), dev);
    ret(c, 0)
}

// ---- ID3D11DeviceContext: binding ---------------------------------------------------------

/// An array of interface pointers (8-byte ones in a 64-bit process: their low halves).
fn read_ptrs(c: &Ctx, p: u32, n: u32) -> Vec<u32> {
    (0..n.min(16)).map(|i| if p == 0 { 0 } else { rd(c, p + c.ptr_size() * i) }).collect()
}

fn ctx_set_slots(c: &mut Ctx) -> ApiResult {
    let (start, n, pp) = (c.arg(1), c.arg(2), c.arg(3));
    let name = c.name().rsplit("::").next().unwrap_or("").to_string();
    let objs = read_ptrs(c, pp, n);
    let st = &mut c.p.d3d11;
    let slots: &mut [u32] = match name.as_str() {
        "VSSetConstantBuffers" => &mut st.vs_cb,
        "PSSetConstantBuffers" => &mut st.ps_cb,
        "PSSetShaderResources" => &mut st.ps_srv,
        "PSSetSamplers" => &mut st.ps_samplers,
        // the other stages draw nothing here
        _ => return ret(c, 0),
    };
    for (i, o) in objs.into_iter().enumerate() {
        if let Some(s) = slots.get_mut(start as usize + i) {
            *s = o;
        }
    }
    ret(c, 0)
}

fn ctx_set_shader(c: &mut Ctx) -> ApiResult {
    let s = c.arg(1);
    if c.name().ends_with("VSSetShader") {
        c.p.d3d11.vs = s;
    } else if c.name().ends_with("PSSetShader") {
        c.p.d3d11.ps = s;
    }
    ret(c, 0)
}

fn ctx_ia_set_input_layout(c: &mut Ctx) -> ApiResult {
    c.p.d3d11.layout = c.arg(1);
    ret(c, 0)
}

fn ctx_ia_set_vertex_buffers(c: &mut Ctx) -> ApiResult {
    let (start, n, pbufs, pstrides, poffsets) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5));
    for i in 0..n.min(8) {
        let slot = (start + i) as usize;
        if slot >= 8 {
            break;
        }
        let b = if pbufs != 0 { rd(c, pbufs + c.ptr_size() * i) } else { 0 };
        let s = if pstrides != 0 { rd(c, pstrides + 4 * i) } else { 0 };
        let o = if poffsets != 0 { rd(c, poffsets + 4 * i) } else { 0 };
        c.p.d3d11.vbs[slot] = (b, s, o);
    }
    ret(c, 0)
}

fn ctx_ia_set_index_buffer(c: &mut Ctx) -> ApiResult {
    c.p.d3d11.ib = (c.arg(1), c.arg(2), c.arg(3));
    ret(c, 0)
}

fn ctx_ia_set_topology(c: &mut Ctx) -> ApiResult {
    c.p.d3d11.topology = c.arg(1);
    ret(c, 0)
}

/// The GPU target of a render target view: 0 for the back buffer.
fn view_target(c: &Ctx, rtv: u32) -> (u32, u32, u32) {
    let res = field(c, rtv, "res");
    if res == 0 || field(c, res, "backbuffer") != 0 {
        let (w, h) = c.p.d3d11.size;
        return (0, w, h);
    }
    (res, field(c, res, "w"), field(c, res, "h"))
}

fn set_target(c: &mut Ctx, t: (u32, u32, u32)) {
    if c.p.d3d11.target != t {
        c.p.d3d11.target = t;
        c.p.gpu.set_target(t.0, t.1, t.2);
    }
}

fn ctx_om_set_render_targets(c: &mut Ctx) -> ApiResult {
    let (n, pp, dsv) = (c.arg(1), c.arg(2), c.arg(3));
    let rtv = if n > 0 && pp != 0 { rd(c, pp) } else { 0 };
    c.p.d3d11.rtv = rtv;
    c.p.d3d11.dsv = dsv;
    if rtv != 0 {
        let t = view_target(c, rtv);
        set_target(c, t);
    }
    ret(c, 0)
}

fn ctx_om_get_render_targets(c: &mut Ctx) -> ApiResult {
    let (n, pp, pdsv) = (c.arg(1), c.arg(2), c.arg(3));
    let (rtv, dsv) = (c.p.d3d11.rtv, c.p.d3d11.dsv);
    if n > 0 && pp != 0 {
        if rtv != 0 {
            add_ref_obj(c, rtv);
        }
        out(c, pp, rtv);
        for i in 1..n.min(8) {
            let at = pp + c.ptr_size() * i;
            out(c, at, 0);
        }
    }
    if pdsv != 0 {
        if dsv != 0 {
            add_ref_obj(c, dsv);
        }
        out(c, pdsv, dsv);
    }
    ret(c, 0)
}

fn ctx_om_set_blend_state(c: &mut Ctx) -> ApiResult {
    let (s, pf) = (c.arg(1), c.arg(2));
    c.p.d3d11.blend = s;
    if pf != 0 {
        c.p.d3d11.blend_factor = [rdf(c, pf), rdf(c, pf + 4), rdf(c, pf + 8), rdf(c, pf + 12)];
    }
    ret(c, 0)
}

fn ctx_om_set_depth_stencil_state(c: &mut Ctx) -> ApiResult {
    c.p.d3d11.depth = c.arg(1);
    ret(c, 0)
}

fn ctx_rs_set_state(c: &mut Ctx) -> ApiResult {
    c.p.d3d11.raster = c.arg(1);
    ret(c, 0)
}

fn ctx_rs_set_viewports(c: &mut Ctx) -> ApiResult {
    let (n, p) = (c.arg(1), c.arg(2));
    if n > 0 && p != 0 {
        c.p.d3d11.viewport = [rdf(c, p), rdf(c, p + 4), rdf(c, p + 8), rdf(c, p + 12), rdf(c, p + 16), rdf(c, p + 20)];
    }
    ret(c, 0)
}

fn ctx_rs_get_viewports(c: &mut Ctx) -> ApiResult {
    let (pn, p) = (c.arg(1), c.arg(2));
    if p != 0 {
        let vp = c.p.d3d11.viewport;
        for (i, v) in vp.iter().enumerate() {
            let _ = c.host.write_u32(p + 4 * i as u32, v.to_bits());
        }
    }
    out32(c, pn, 1);
    ret(c, 0)
}

fn ctx_rs_set_scissor_rects(c: &mut Ctx) -> ApiResult {
    let (n, p) = (c.arg(1), c.arg(2));
    if n > 0 && p != 0 {
        let g = |k: u32| rd(c, p + 4 * k) as i32;
        c.p.d3d11.scissor = [g(0), g(1), g(2) - g(0), g(3) - g(1)];
    }
    ret(c, 0)
}

// ---- ID3D11DeviceContext: data ----------------------------------------------------------------

/// Map(resource, subresource, type, flags, pMapped): the resource's storage (a buffer's
/// contents, a texture level's rows), written to in place; draws read it when they run,
/// so WRITE_DISCARD needs no new storage.
fn ctx_map(c: &mut Ctx) -> ApiResult {
    let (res, sub, ty, p) = (c.arg(1), c.arg(2), c.arg(3), c.arg(5));
    let _ = ty == MAP_WRITE_DISCARD;
    let (bits, pitch, slice) = if kind(c, res) == "ID3D11Buffer" {
        let size = field(c, res, "size");
        (field(c, res, "bits0"), size, size)
    } else {
        let level = sub % field(c, res, "levels").max(1);
        let fmt = field(c, res, "fmt");
        let (lw, lh) = level_dims(field(c, res, "w"), field(c, res, "h"), level);
        let pitch = level_pitch(fmt, lw);
        (field(c, res, level_key(level)), pitch, pitch * level_rows(fmt, lh))
    };
    if bits == 0 {
        return ret(c, E_INVALIDARG);
    }
    // D3D11_MAPPED_SUBRESOURCE: pData, RowPitch, DepthPitch
    let ps = c.ptr_size();
    out(c, p, bits);
    out32(c, p + ps, pitch);
    out32(c, p + ps + 4, slice);
    ret(c, S_OK)
}

fn ctx_unmap(c: &mut Ctx) -> ApiResult {
    let (res, sub) = (c.arg(1), c.arg(2));
    if kind(c, res) == "ID3D11Texture2D" && field(c, res, "gpu") != 0 {
        let level = sub % field(c, res, "levels").max(1);
        mark_dirty(c, res, level);
    }
    ret(c, 0)
}

/// UpdateSubresource(dst, subresource, box, data, rowPitch, depthPitch).
fn ctx_update_subresource(c: &mut Ctx) -> ApiResult {
    let (dst, sub, pbox, src, pitch) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5));
    let area = (pbox != 0).then(|| (rd(c, pbox), rd(c, pbox + 4), rd(c, pbox + 12), rd(c, pbox + 16)));
    if kind(c, dst) == "ID3D11Buffer" {
        let size = field(c, dst, "size");
        let (x0, x1) = area.map(|(l, _, r, _)| (l, r.min(size))).unwrap_or((0, size));
        if x1 > x0 {
            let b = read_bytes(c, src, x1 - x0);
            let bits = field(c, dst, "bits0");
            let _ = c.host.write(bits + x0, &b);
        }
    } else {
        let level = sub % field(c, dst, "levels").max(1);
        write_level(c, dst, level, src, pitch, area);
    }
    ret(c, 0)
}

/// CopyResource(dst, src) and CopySubresourceRegion(dst, dstSub, x, y, z, src, srcSub,
/// box): a render target's pixels are read back from the GPU (screenshots, surface
/// reads); other textures are copied in guest memory.
fn ctx_copy(c: &mut Ctx) -> ApiResult {
    let region = c.name().ends_with("CopySubresourceRegion");
    let (dst, src) = if region { (c.arg(1), c.arg(6)) } else { (c.arg(1), c.arg(2)) };
    if kind(c, dst) == "ID3D11Buffer" && kind(c, src) == "ID3D11Buffer" {
        let n = field(c, dst, "size").min(field(c, src, "size"));
        let b = read_bytes(c, field(c, src, "bits0"), n);
        let bits = field(c, dst, "bits0");
        let _ = c.host.write(bits, &b);
        return ret(c, 0);
    }
    let (w, h) = (field(c, src, "w"), field(c, src, "h"));
    let rendered = field(c, src, "bind") & BIND_RENDER_TARGET != 0 || field(c, src, "backbuffer") != 0;
    let dst_bits = field(c, dst, "bits0");
    if dst_bits == 0 {
        return ret(c, 0);
    }
    let pitch = level_pitch(field(c, dst, "fmt"), field(c, dst, "w"));
    if rendered {
        let id = if field(c, src, "backbuffer") != 0 { 0 } else { src };
        c.p.gpu.read_target(id, w.min(field(c, dst, "w")), h.min(field(c, dst, "h")), dst_bits, pitch);
    } else {
        let src_bits = field(c, src, "bits0");
        let n = pitch * h.min(field(c, dst, "h"));
        let b = read_bytes(c, src_bits, n);
        let _ = c.host.write(dst_bits, &b);
    }
    if field(c, dst, "gpu") != 0 {
        mark_dirty(c, dst, 0);
    }
    ret(c, 0)
}

/// ClearRenderTargetView(rtv, color[4]).
fn ctx_clear_rtv(c: &mut Ctx) -> ApiResult {
    let (rtv, pc) = (c.arg(1), c.arg(2));
    let ch = |k: u32| (rdf(c, pc + 4 * k).clamp(0.0, 1.0) * 255.0).round() as u32;
    let color = ch(3) << 24 | ch(0) << 16 | ch(1) << 8 | ch(2);
    let t = view_target(c, rtv);
    let back = c.p.d3d11.target;
    set_target(c, t);
    c.p.gpu.clear(1, color, 1.0, 0, [0, 0, t.1 as i32, t.2 as i32]);
    set_target(c, back);
    ret(c, 0)
}

/// ClearDepthStencilView(dsv, flags, depth, stencil): of the current target's depth.
fn ctx_clear_dsv(c: &mut Ctx) -> ApiResult {
    let (flags, depth, stencil) = (c.arg(2), c.arg_f32(3), c.arg(4) & 0xFF);
    let t = c.p.d3d11.target;
    let f = (if flags & 1 != 0 { 2 } else { 0 }) | (if flags & 2 != 0 { 4 } else { 0 });
    c.p.gpu.clear(f, 0, depth, stencil, [0, 0, t.1 as i32, t.2 as i32]);
    ret(c, 0)
}

fn ctx_get_data(c: &mut Ctx) -> ApiResult {
    let (p, size) = (c.arg(2), c.arg(3));
    if p != 0 && size >= 4 {
        let _ = c.host.write(p, &vec![0u8; size.min(64) as usize]);
        let _ = c.host.write_u32(p, 1);
    }
    ret(c, S_OK)
}

// ---- ID3D11DeviceContext: draws ---------------------------------------------------------------

/// Direct3D 9 render states from the blend, depth and rasterizer state objects (their
/// enumerations share Direct3D 9's numbers).
fn render_states(c: &Ctx) -> [u32; 256] {
    let st = &c.p.d3d11;
    let mut rs = [0u32; 256];
    // blend: AlphaToCoverage, IndependentBlend, then per target {BlendEnable, SrcBlend,
    // DestBlend, BlendOp, SrcBlendAlpha, DestBlendAlpha, BlendOpAlpha, WriteMask}
    let b = |i: u32| state_word(c, st.blend, 2 + i);
    if st.blend != 0 {
        rs[gpu::rs::ALPHABLENDENABLE] = b(0);
        rs[gpu::rs::SRCBLEND] = b(1);
        rs[gpu::rs::DESTBLEND] = b(2);
        rs[gpu::rs::BLENDOP] = b(3);
        rs[206] = 1; // SEPARATEALPHABLENDENABLE
        rs[207] = b(4);
        rs[208] = b(5);
        rs[209] = b(6);
        rs[gpu::rs::COLORWRITEENABLE] = b(7) & 15;
    } else {
        rs[gpu::rs::SRCBLEND] = 2;
        rs[gpu::rs::DESTBLEND] = 1;
        rs[gpu::rs::BLENDOP] = 1;
        rs[gpu::rs::COLORWRITEENABLE] = 15;
    }
    // depth stencil: DepthEnable, DepthWriteMask, DepthFunc, StencilEnable, ...
    if st.dsv != 0 {
        let d = |i: u32| state_word(c, st.depth, i);
        let (enable, write, func) = if st.depth != 0 { (d(0), d(1), d(2)) } else { (1, 1, 2) };
        rs[gpu::rs::ZENABLE] = enable;
        rs[gpu::rs::ZWRITEENABLE] = (write != 0) as u32;
        rs[gpu::rs::ZFUNC] = func;
    }
    rs[gpu::rs::ZFUNC] = rs[gpu::rs::ZFUNC].max(1);
    // rasterizer: FillMode, CullMode, FrontCounterClockwise, DepthBias, DepthBiasClamp,
    // SlopeScaledDepthBias, DepthClipEnable, ScissorEnable, MultisampleEnable,
    // AntialiasedLineEnable (no object: solid, back faces culled)
    let r = |i: u32| state_word(c, st.raster, i);
    let (fill, cull, ccw, scissor) = if st.raster != 0 { (r(0), r(1), r(2), r(7)) } else { (3, 3, 0, 0) };
    rs[gpu::rs::FILLMODE] = fill;
    // D3D11_CULL_NONE/FRONT/BACK with clockwise fronts = D3DCULL_NONE/CW/CCW
    rs[gpu::rs::CULLMODE] = match (cull, ccw != 0) {
        (2, false) | (3, true) => 2,
        (3, false) | (2, true) => 3,
        _ => 1,
    };
    rs[174] = scissor; // SCISSORTESTENABLE
    rs
}

/// Direct3D 9 sampler states (ADDRESSU.. MIPFILTER) from a D3D11_SAMPLER_DESC {Filter,
/// AddressU, AddressV, AddressW, MipLODBias, MaxAnisotropy, ComparisonFunc,
/// BorderColor[4], MinLOD, MaxLOD}.
fn sampler_states(c: &Ctx, s: u32) -> [u32; 14] {
    let mut out = [0u32; 14];
    let (filter, u, v, w) = if s != 0 { (state_word(c, s, 0), state_word(c, s, 1), state_word(c, s, 2), state_word(c, s, 3)) } else { (0x15, 3, 3, 3) };
    let linear = |bit: u32| if filter & 0x40 != 0 || filter & bit != 0 { 2 } else { 1 };
    out[1] = u.max(1);
    out[2] = v.max(1);
    out[3] = w.max(1);
    out[5] = linear(4);
    out[6] = linear(16);
    out[7] = linear(1);
    out
}

/// Primitive type and count of `n` vertices in a topology.
fn primitives(topology: u32, n: u32) -> Option<(u32, u32)> {
    Some(match topology {
        1 => (1, n),
        2 => (2, n / 2),
        3 => (3, n.saturating_sub(1)),
        4 => (4, n / 3),
        5 => (5, n.saturating_sub(2)),
        _ => return None,
    })
}

/// Builds the host's draw record. `base` (vertices) is added to every stream's start;
/// `indices` is the guest address of the first index (0: not indexed).
fn submit(c: &mut Ctx, count: u32, base: i32, indices: u32, index_size: u32) {
    c.p.draw_calls += 1;
    let st = &c.p.d3d11;
    let Some((prim, prim_count)) = primitives(st.topology, count) else { return };
    if prim_count == 0 {
        return;
    }
    let (vs, ps) = (st.vs, st.ps);
    let (Some(vsh), Some(psh)) = (st.shaders.get(&vs), st.shaders.get(&ps)) else { return };
    let Some(layout) = st.layouts.get(&st.layout) else { return };
    // vertices: which ones the indices address
    let (min_index, vertex_count) = if indices != 0 {
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        for i in 0..count {
            let v = if index_size == 2 { c.host.read_u16(indices + 2 * i).unwrap_or(0) as u32 } else { rd(c, indices + 4 * i) };
            lo = lo.min(v);
            hi = hi.max(v);
        }
        (lo, hi - lo + 1)
    } else {
        (0, count)
    };
    let mut streams = [[0u32; 2]; 4];
    let mut elements = [[0u32; 4]; 16];
    let mut n = 0;
    for (name, index, fmt, slot, offset) in layout {
        let (Some(ty), true) = (decl_type(*fmt), (*slot as usize) < 4) else { continue };
        if n < 16 {
            elements[n] = [*slot, *offset, ty, usage(name) | index << 8];
            n += 1;
        }
    }
    for (s, stream) in streams.iter_mut().enumerate() {
        let (buf, stride, off) = st.vbs[s];
        if buf != 0 {
            let bits = field(c, buf, "bits0");
            *stream = [bits.wrapping_add(off).wrapping_add((base as u32).wrapping_mul(stride)), stride];
        }
    }
    if streams[0][0] == 0 {
        return;
    }
    // constants: each constant buffer where the translated shader reads it
    let mut vs_f = [[0f32; 4]; 256];
    let mut ps_f = [[0f32; 4]; 224];
    let fill = |c: &Ctx, cb: &[u32; 14], layout: &[(u32, u32, u32)], dst: &mut [[f32; 4]]| {
        for &(slot, base, size) in layout {
            let buf = cb.get(slot as usize).copied().unwrap_or(0);
            if buf == 0 {
                continue;
            }
            let (bits, bytes) = (field(c, buf, "bits0"), field(c, buf, "size"));
            let n = size.min(bytes / 16) as usize;
            let mut raw = vec![0u8; n * 16];
            let _ = c.host.read(bits, &mut raw);
            for (i, v) in raw.chunks_exact(16).enumerate() {
                if let Some(d) = dst.get_mut(base as usize + i) {
                    for k in 0..4 {
                        d[k] = f32::from_le_bytes([v[4 * k], v[4 * k + 1], v[4 * k + 2], v[4 * k + 3]]);
                    }
                }
            }
        }
    };
    fill(c, &st.vs_cb, &vsh.cbuffers, &mut vs_f);
    fill(c, &st.ps_cb, &psh.cbuffers, &mut ps_f);
    let samples = psh.samples.clone();
    let (srvs, smps) = (st.ps_srv, st.ps_samplers);
    let (viewport, scissor, target) = (st.viewport, st.scissor, st.target);
    // textures: sampler unit t# with the states of s#
    let mut textures = [[0u32; 3]; 16];
    let mut samplers = [[0u32; 14]; 16];
    for (t, s) in samples {
        let (t, s) = (t as usize, s as usize);
        if t >= 16 {
            continue;
        }
        let tex = field(c, srvs.get(t).copied().unwrap_or(0), "res");
        if tex != 0 && c.p.d3d_objects.contains_key(&tex) {
            super::d3d9::upload_pending(c, tex);
            textures[t] = [tex, field(c, tex, "fmt"), (field(c, tex, "bind") & BIND_RENDER_TARGET != 0) as u32];
        }
        samplers[t] = sampler_states(c, smps.get(s).copied().unwrap_or(0));
    }
    let rs = render_states(c);
    let d = DrawCall9 {
        prim,
        prim_count,
        vertex_count,
        min_index,
        indices,
        index_size,
        streams,
        elements,
        element_count: n as u32,
        vs,
        ps,
        textures,
        samplers,
        target_w: target.1,
        target_h: target.2,
        viewport,
        scissor,
        rs,
        vs_f,
        ps_f,
        vs_i: [[0; 4]; 16],
        ps_i: [[0; 4]; 16],
        vs_b: [0; 16],
        ps_b: [0; 16],
    };
    if c.p.trace {
        c.p.log(format!("    draw11 prim {prim} x{prim_count} vs {vs:#x} ps {ps:#x} tex {:#x} elements {n}", textures[0][0]));
    }
    c.p.gpu.draw9(&d);
}

fn ctx_draw(c: &mut Ctx) -> ApiResult {
    let (count, start) = (c.arg(1), c.arg(2));
    submit(c, count, start as i32, 0, 0);
    ret(c, 0)
}

fn ctx_draw_indexed(c: &mut Ctx) -> ApiResult {
    let (count, start, base) = (c.arg(1), c.arg(2), c.arg(3) as i32);
    let (ib, fmt, off) = c.p.d3d11.ib;
    if ib == 0 {
        return ret(c, 0);
    }
    let size = if fmt == 42 { 4 } else { 2 }; // R32_UINT, else R16_UINT
    let indices = field(c, ib, "bits0") + off + start * size;
    submit(c, count, base, indices, size);
    ret(c, 0)
}

fn ctx_clear_state(c: &mut Ctx) -> ApiResult {
    let st = &mut c.p.d3d11;
    st.vs = 0;
    st.ps = 0;
    st.layout = 0;
    st.vbs = Default::default();
    st.ib = (0, 0, 0);
    st.vs_cb = [0; 14];
    st.ps_cb = [0; 14];
    st.ps_srv = [0; 16];
    st.ps_samplers = [0; 16];
    st.blend = 0;
    st.depth = 0;
    st.raster = 0;
    ret(c, 0)
}

// ---- registration -----------------------------------------------------------------------------

/// ID3D11DeviceChild's methods, first after IUnknown's.
macro_rules! child {
    () => {
        [("GetDevice", 2, Some(child_get_device as ApiFn)), ("GetPrivateData", 4, Some(not_impl as ApiFn)), ("SetPrivateData", 4, Some(ok_s as ApiFn)), ("SetPrivateDataInterface", 3, Some(ok_s as ApiFn))]
    };
}

/// IDXGIObject's methods, first after IUnknown's.
macro_rules! dxgi_object {
    () => {
        [("SetPrivateData", 4, Some(ok_s as ApiFn)), ("SetPrivateDataInterface", 3, Some(ok_s as ApiFn)), ("GetPrivateData", 4, Some(not_impl as ApiFn)), ("GetParent", 3, Some(object_get_parent as ApiFn))]
    };
}

const UNKNOWN: [Method; 3] = [("QueryInterface", 3, Some(query_interface as ApiFn)), ("AddRef", 1, Some(add_ref as ApiFn)), ("Release", 1, Some(release as ApiFn))];

fn concat(parts: &[&[Method]]) -> &'static [Method] {
    Box::leak(parts.concat().into_boxed_slice())
}

/// The full vtable (IUnknown first) of each interface.
fn methods_of(iface: &str) -> &'static [Method] {
    use std::sync::OnceLock;
    static TABLES: OnceLock<Vec<(&'static str, &'static [Method])>> = OnceLock::new();
    let tables = TABLES.get_or_init(|| {
        let f = |f: fn(&mut Ctx) -> ApiResult| Some(f as ApiFn);
        let device: &[Method] = &[
            ("CreateBuffer", 4, f(dev_create_buffer)),
            ("CreateTexture1D", 4, None),
            ("CreateTexture2D", 4, f(dev_create_texture2d)),
            ("CreateTexture3D", 4, None),
            ("CreateShaderResourceView", 4, f(dev_create_view)),
            ("CreateUnorderedAccessView", 4, None),
            ("CreateRenderTargetView", 4, f(dev_create_view)),
            ("CreateDepthStencilView", 4, f(dev_create_view)),
            ("CreateInputLayout", 6, f(dev_create_input_layout)),
            ("CreateVertexShader", 5, f(dev_create_shader)),
            ("CreateGeometryShader", 5, None),
            ("CreateGeometryShaderWithStreamOutput", 10, None),
            ("CreatePixelShader", 5, f(dev_create_shader)),
            ("CreateHullShader", 5, None),
            ("CreateDomainShader", 5, None),
            ("CreateComputeShader", 5, None),
            ("CreateClassLinkage", 2, None),
            ("CreateBlendState", 3, f(dev_create_state)),
            ("CreateDepthStencilState", 3, f(dev_create_state)),
            ("CreateRasterizerState", 3, f(dev_create_state)),
            ("CreateSamplerState", 3, f(dev_create_state)),
            ("CreateQuery", 3, f(dev_create_query)),
            ("CreatePredicate", 3, None),
            ("CreateCounter", 3, None),
            ("CreateDeferredContext", 3, None),
            ("OpenSharedResource", 4, None),
            ("CheckFormatSupport", 3, f(dev_check_format_support)),
            ("CheckMultisampleQualityLevels", 4, f(dev_check_multisample)),
            ("CheckCounterInfo", 2, None),
            ("CheckCounter", 10, None),
            ("CheckFeatureSupport", 4, f(dev_check_feature_support)),
            ("GetPrivateData", 4, f(not_impl)),
            ("SetPrivateData", 4, f(ok_s)),
            ("SetPrivateDataInterface", 3, f(ok_s)),
            ("GetFeatureLevel", 1, f(dev_get_feature_level)),
            ("GetCreationFlags", 1, f(ok_void)),
            ("GetDeviceRemovedReason", 1, f(ok_s)),
            ("GetImmediateContext", 2, f(dev_get_immediate_context)),
            ("SetExceptionMode", 2, f(ok_s)),
            ("GetExceptionMode", 1, f(ok_void)),
        ];
        let set4 = f(ctx_set_slots);
        let get4 = f(ok_void);
        let context: &[Method] = &[
            ("VSSetConstantBuffers", 4, set4),
            ("PSSetShaderResources", 4, set4),
            ("PSSetShader", 4, f(ctx_set_shader)),
            ("PSSetSamplers", 4, set4),
            ("VSSetShader", 4, f(ctx_set_shader)),
            ("DrawIndexed", 4, f(ctx_draw_indexed)),
            ("Draw", 3, f(ctx_draw)),
            ("Map", 6, f(ctx_map)),
            ("Unmap", 3, f(ctx_unmap)),
            ("PSSetConstantBuffers", 4, set4),
            ("IASetInputLayout", 2, f(ctx_ia_set_input_layout)),
            ("IASetVertexBuffers", 6, f(ctx_ia_set_vertex_buffers)),
            ("IASetIndexBuffer", 4, f(ctx_ia_set_index_buffer)),
            ("DrawIndexedInstanced", 6, None),
            ("DrawInstanced", 5, None),
            ("GSSetConstantBuffers", 4, set4),
            ("GSSetShader", 4, f(ctx_set_shader)),
            ("IASetPrimitiveTopology", 2, f(ctx_ia_set_topology)),
            ("VSSetShaderResources", 4, set4),
            ("VSSetSamplers", 4, set4),
            ("Begin", 2, f(ok_void)),
            ("End", 2, f(ok_void)),
            ("GetData", 5, f(ctx_get_data)),
            ("SetPredication", 3, f(ok_void)),
            ("GSSetShaderResources", 4, set4),
            ("GSSetSamplers", 4, set4),
            ("OMSetRenderTargets", 4, f(ctx_om_set_render_targets)),
            ("OMSetRenderTargetsAndUnorderedAccessViews", 8, None),
            ("OMSetBlendState", 4, f(ctx_om_set_blend_state)),
            ("OMSetDepthStencilState", 3, f(ctx_om_set_depth_stencil_state)),
            ("SOSetTargets", 4, f(ok_void)),
            ("DrawAuto", 1, None),
            ("DrawIndexedInstancedIndirect", 3, None),
            ("DrawInstancedIndirect", 3, None),
            ("Dispatch", 4, None),
            ("DispatchIndirect", 3, None),
            ("RSSetState", 2, f(ctx_rs_set_state)),
            ("RSSetViewports", 3, f(ctx_rs_set_viewports)),
            ("RSSetScissorRects", 3, f(ctx_rs_set_scissor_rects)),
            ("CopySubresourceRegion", 9, f(ctx_copy)),
            ("CopyResource", 3, f(ctx_copy)),
            ("UpdateSubresource", 7, f(ctx_update_subresource)),
            ("CopyStructureCount", 4, None),
            ("ClearRenderTargetView", 3, f(ctx_clear_rtv)),
            ("ClearUnorderedAccessViewUint", 3, None),
            ("ClearUnorderedAccessViewFloat", 3, None),
            ("ClearDepthStencilView", 5, f(ctx_clear_dsv)),
            ("GenerateMips", 2, f(ok_void)),
            ("SetResourceMinLOD", 3, f(ok_void)),
            ("GetResourceMinLOD", 2, f(ok_void)),
            ("ResolveSubresource", 6, None),
            ("ExecuteCommandList", 3, None),
            ("HSSetShaderResources", 4, set4),
            ("HSSetShader", 4, f(ctx_set_shader)),
            ("HSSetSamplers", 4, set4),
            ("HSSetConstantBuffers", 4, set4),
            ("DSSetShaderResources", 4, set4),
            ("DSSetShader", 4, f(ctx_set_shader)),
            ("DSSetSamplers", 4, set4),
            ("DSSetConstantBuffers", 4, set4),
            ("CSSetShaderResources", 4, set4),
            ("CSSetUnorderedAccessViews", 5, f(ok_void)),
            ("CSSetShader", 4, f(ctx_set_shader)),
            ("CSSetSamplers", 4, set4),
            ("CSSetConstantBuffers", 4, set4),
            ("VSGetConstantBuffers", 4, None),
            ("PSGetShaderResources", 4, None),
            ("PSGetShader", 4, None),
            ("PSGetSamplers", 4, None),
            ("VSGetShader", 4, None),
            ("PSGetConstantBuffers", 4, None),
            ("IAGetInputLayout", 2, None),
            ("IAGetVertexBuffers", 6, None),
            ("IAGetIndexBuffer", 4, None),
            ("GSGetConstantBuffers", 4, None),
            ("GSGetShader", 4, None),
            ("IAGetPrimitiveTopology", 2, None),
            ("VSGetShaderResources", 4, None),
            ("VSGetSamplers", 4, None),
            ("GetPredication", 3, None),
            ("GSGetShaderResources", 4, None),
            ("GSGetSamplers", 4, None),
            ("OMGetRenderTargets", 4, f(ctx_om_get_render_targets)),
            ("OMGetRenderTargetsAndUnorderedAccessViews", 7, None),
            ("OMGetBlendState", 4, None),
            ("OMGetDepthStencilState", 3, None),
            ("SOGetTargets", 3, None),
            ("RSGetState", 2, None),
            ("RSGetViewports", 3, f(ctx_rs_get_viewports)),
            ("RSGetScissorRects", 3, None),
            ("HSGetShaderResources", 4, get4),
            ("HSGetShader", 4, get4),
            ("HSGetSamplers", 4, get4),
            ("HSGetConstantBuffers", 4, get4),
            ("DSGetShaderResources", 4, get4),
            ("DSGetShader", 4, get4),
            ("DSGetSamplers", 4, get4),
            ("DSGetConstantBuffers", 4, get4),
            ("CSGetShaderResources", 4, get4),
            ("CSGetUnorderedAccessViews", 4, get4),
            ("CSGetShader", 4, get4),
            ("CSGetSamplers", 4, get4),
            ("CSGetConstantBuffers", 4, get4),
            ("ClearState", 1, f(ctx_clear_state)),
            ("Flush", 1, f(ok_void)),
            ("GetType", 1, f(ok_void)),
            ("GetContextFlags", 1, f(ok_void)),
            ("FinishCommandList", 3, None),
        ];
        let resource: &[Method] = &[("GetType", 2, f(resource_get_type)), ("SetEvictionPriority", 2, f(ok_void)), ("GetEvictionPriority", 1, f(ok_void))];
        let view: &[Method] = &[("GetResource", 2, f(view_get_resource)), ("GetDesc", 2, None)];
        let state: &[Method] = &[("GetDesc", 2, f(state_get_desc))];
        let factory: &[Method] = &[
            ("EnumAdapters", 3, f(factory_enum_adapters)),
            ("MakeWindowAssociation", 3, f(ok_s)),
            ("GetWindowAssociation", 2, None),
            ("CreateSwapChain", 4, f(factory_create_swap_chain)),
            ("CreateSoftwareAdapter", 3, None),
            ("EnumAdapters1", 3, f(factory_enum_adapters)),
            ("IsCurrent", 1, f(|c| ret(c, 1))),
        ];
        let adapter: &[Method] = &[("EnumOutputs", 3, f(adapter_enum_outputs)), ("GetDesc", 2, f(adapter_get_desc)), ("CheckInterfaceSupport", 3, f(ok_s)), ("GetDesc1", 2, f(adapter_get_desc))];
        let output: &[Method] = &[
            ("GetDesc", 2, f(output_get_desc)),
            ("GetDisplayModeList", 5, f(output_get_display_mode_list)),
            ("FindClosestMatchingMode", 4, f(output_find_closest_mode)),
            ("WaitForVBlank", 1, f(ok_s)),
            ("TakeOwnership", 3, f(ok_s)),
            ("ReleaseOwnership", 1, f(ok_void)),
            ("GetGammaControlCapabilities", 2, f(not_impl)),
            ("SetGammaControl", 2, f(ok_s)),
            ("GetGammaControl", 2, f(not_impl)),
            ("SetDisplaySurface", 2, f(not_impl)),
            ("GetDisplaySurfaceData", 2, f(not_impl)),
            ("GetFrameStatistics", 2, f(not_impl)),
        ];
        let swap_chain: &[Method] = &[
            ("GetDevice", 3, f(sc_get_device)),
            ("Present", 3, f(sc_present)),
            ("GetBuffer", 4, f(sc_get_buffer)),
            ("SetFullscreenState", 3, f(ok_s)),
            ("GetFullscreenState", 3, f(sc_get_fullscreen_state)),
            ("GetDesc", 2, f(sc_get_desc)),
            ("ResizeBuffers", 6, f(sc_resize_buffers)),
            ("ResizeTarget", 2, f(ok_s)),
            ("GetContainingOutput", 2, f(sc_get_containing_output)),
            ("GetFrameStatistics", 2, f(not_impl)),
            ("GetLastPresentCount", 2, f(not_impl)),
        ];
        let dxgi_device: &[Method] = &[
            ("GetAdapter", 2, f(dxgi_device_get_adapter)),
            ("CreateSurface", 6, None),
            ("QueryResourceResidency", 4, None),
            ("SetGPUThreadPriority", 2, f(ok_s)),
            ("GetGPUThreadPriority", 2, f(not_impl)),
            ("SetMaximumFrameLatency", 2, f(ok_s)),
            ("GetMaximumFrameLatency", 2, f(not_impl)),
        ];
        let child = child!();
        let object = dxgi_object!();
        vec![
            ("ID3D11Device", concat(&[&UNKNOWN, device])),
            ("ID3D11DeviceContext", concat(&[&UNKNOWN, &child, context])),
            ("ID3D11Buffer", concat(&[&UNKNOWN, &child, resource, &[("GetDesc", 2, f(buffer_get_desc))]])),
            ("ID3D11Texture2D", concat(&[&UNKNOWN, &child, resource, &[("GetDesc", 2, f(texture_get_desc))]])),
            ("ID3D11ShaderResourceView", concat(&[&UNKNOWN, &child, view])),
            ("ID3D11RenderTargetView", concat(&[&UNKNOWN, &child, view])),
            ("ID3D11DepthStencilView", concat(&[&UNKNOWN, &child, view])),
            ("ID3D11VertexShader", concat(&[&UNKNOWN, &child])),
            ("ID3D11PixelShader", concat(&[&UNKNOWN, &child])),
            ("ID3D11InputLayout", concat(&[&UNKNOWN, &child])),
            ("ID3D11BlendState", concat(&[&UNKNOWN, &child, state])),
            ("ID3D11DepthStencilState", concat(&[&UNKNOWN, &child, state])),
            ("ID3D11RasterizerState", concat(&[&UNKNOWN, &child, state])),
            ("ID3D11SamplerState", concat(&[&UNKNOWN, &child, state])),
            ("ID3D11Query", concat(&[&UNKNOWN, &child, &[("GetDataSize", 1, f(|c| ret(c, 4))), ("GetDesc", 2, None)]])),
            ("IDXGIFactory1", concat(&[&UNKNOWN, &object, factory])),
            ("IDXGIAdapter1", concat(&[&UNKNOWN, &object, adapter])),
            ("IDXGIOutput", concat(&[&UNKNOWN, &object, output])),
            ("IDXGISwapChain", concat(&[&UNKNOWN, &object, swap_chain])),
            ("IDXGIDevice1", concat(&[&UNKNOWN, &object, dxgi_device])),
        ]
    });
    tables.iter().find(|(n, _)| *n == iface).map(|(_, m)| *m).unwrap_or(&[])
}

/// Registers every interface's methods (once): the first device or factory a game makes.
fn ensure_registered(c: &mut Ctx) {
    if c.p.registry.lookup(DLL, "ID3D11Device::QueryInterface").is_some() {
        return;
    }
    for &(iface, _) in IIDS {
        for (name, nargs, f) in methods_of(iface) {
            let n = format!("{iface}::{name}");
            match f {
                Some(f) => {
                    c.p.registry.add_n(DLL, &n, *nargs, *f);
                }
                None => {
                    c.p.registry.intern(DLL, &n);
                }
            }
        }
    }
}

/// DwmGetCompositionTimingInfo(hwnd, info): a 60 Hz display (GameMaker Studio 2 reads its
/// refresh rate from DWM_TIMING_INFO {cbSize, rateRefresh {num, den}, qpcRefreshPeriod,
/// rateCompose, ...}).
fn dwm_timing_info(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let size = rd(c, p).clamp(4, 0x124);
    let _ = c.host.write(p + 4, &vec![0u8; size as usize - 4]);
    let period: u64 = 10_000_000 / 60; // QueryPerformanceFrequency is 10 MHz
    for (off, v) in [(4, 60), (8, 1), (12, period as u32), (16, (period >> 32) as u32), (20, 60), (24, 1)] {
        let _ = c.host.write_u32(p + off, v);
    }
    Ok(Ret::stdcall(2, S_OK))
}

pub fn register(r: &mut Registry) {
    r.add_on_demand("dwmapi.dll", "DwmGetCompositionTimingInfo", dwm_timing_info);
    r.add_on_demand(DLL, "D3D11CreateDevice", create_device);
    r.add_on_demand(DLL, "D3D11CreateDeviceAndSwapChain", create_device);
    for name in ["CreateDXGIFactory", "CreateDXGIFactory1", "CreateDXGIFactory2"] {
        r.add_on_demand("dxgi.dll", name, create_factory);
    }
}
