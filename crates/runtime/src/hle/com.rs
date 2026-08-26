//! In-process COM objects implemented by the HLE layer. An object is a block of
//! interface pointers (one vtable pointer per interface) followed by nothing else;
//! its state lives host-side. Every method is a registered stub named
//! "Iface::Method" so traces and unimplemented-method errors name it.

use super::{ApiFn, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{D3dObject, Process};
use std::collections::HashMap;

pub const S_OK: u32 = 0;
pub const S_FALSE: u32 = 1;
pub const E_NOTIMPL: u32 = 0x8000_4001;
pub const E_NOINTERFACE: u32 = 0x8000_4002;
pub const E_FAIL: u32 = 0x8000_4005;
pub const DISP_E_MEMBERNOTFOUND: u32 = 0x8002_0003;

pub const DLL: &str = "comhle.dll";

pub type Method = (&'static str, u32, Option<ApiFn>);

pub struct Interface {
    pub name: &'static str,
    /// IIDs this interface pointer answers to in QueryInterface.
    pub iids: &'static [&'static str],
    /// Methods after IUnknown.
    pub methods: &'static [Method],
}

pub fn parse_iid(s: &str) -> [u8; 16] {
    let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let b = |i: usize| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
    let mut g = [0u8; 16];
    // Data1 (LE u32), Data2 (LE u16), Data3 (LE u16), Data4 bytes in order.
    g[0] = b(3);
    g[1] = b(2);
    g[2] = b(1);
    g[3] = b(0);
    g[4] = b(5);
    g[5] = b(4);
    g[6] = b(7);
    g[7] = b(6);
    for i in 8..16 {
        g[i] = b(i);
    }
    g
}

pub fn format_iid(g: &[u8; 16]) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        u32::from_le_bytes(g[..4].try_into().unwrap()),
        u16::from_le_bytes(g[4..6].try_into().unwrap()),
        u16::from_le_bytes(g[6..8].try_into().unwrap()),
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

const IUNKNOWN: &str = "{00000000-0000-0000-C000-000000000046}";

fn method_name(iface: &str, m: &str) -> String {
    format!("{iface}::{m}")
}

/// Registers the methods of an interface (QueryInterface/AddRef/Release are shared).
pub fn register_interface(r: &mut Registry, i: &Interface) {
    r.add_n(DLL, &method_name(i.name, "QueryInterface"), 3, query_interface);
    r.add_n(DLL, &method_name(i.name, "AddRef"), 1, add_ref);
    r.add_n(DLL, &method_name(i.name, "Release"), 1, release);
    for (name, nargs, f) in i.methods {
        let n = method_name(i.name, name);
        match f {
            Some(f) => {
                r.add_n(DLL, &n, *nargs, *f);
            }
            None => {
                r.intern(DLL, &n);
            }
        }
    }
}

fn vtable(c: &mut Ctx, i: &'static Interface) -> u32 {
    if let Some(&v) = c.p.com_vtables.get(i.name) {
        return v;
    }
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let v = c.p.heap_alloc(g, heap, (3 + i.methods.len() as u32) * 4, true);
    let mut names = vec!["QueryInterface", "AddRef", "Release"];
    names.extend(i.methods.iter().map(|m| m.0));
    for (k, m) in names.iter().enumerate() {
        let id = c.p.registry.intern(DLL, &method_name(i.name, m));
        let _ = c.host.write_u32(v + 4 * k as u32, Process::stub_addr(id));
    }
    c.p.com_vtables.insert(i.name, v);
    v
}

/// Creates an object exposing `ifaces` (the first is the primary interface).
/// Returns the pointer of the primary interface.
pub fn create(c: &mut Ctx, kind: &'static str, ifaces: &'static [&'static Interface], data: HashMap<&'static str, u32>) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let base = c.p.heap_alloc(g, heap, ifaces.len() as u32 * 4 + 4, true);
    for (k, i) in ifaces.iter().enumerate() {
        let vt = vtable(c, i);
        let _ = c.host.write_u32(base + 4 * k as u32, vt);
        c.p.com_ifaces.insert(base + 4 * k as u32, (base, k as u32));
    }
    c.p.com_classes.insert(base, ifaces);
    c.p.d3d_objects.insert(base, D3dObject { kind, refcount: 1, data });
    base
}

/// Object base and interface index of an interface pointer.
pub fn object_of(c: &Ctx, this: u32) -> Option<(u32, u32)> {
    c.p.com_ifaces.get(&this).copied()
}

pub fn field(c: &Ctx, this: u32, key: &str) -> u32 {
    let base = object_of(c, this).map(|o| o.0).unwrap_or(this);
    c.p.d3d_objects.get(&base).and_then(|o| o.data.get(key).copied()).unwrap_or(0)
}

pub fn set_field(c: &mut Ctx, this: u32, key: &'static str, v: u32) {
    let base = object_of(c, this).map(|o| o.0).unwrap_or(this);
    if let Some(o) = c.p.d3d_objects.get_mut(&base) {
        o.data.insert(key, v);
    }
}

pub fn ret(c: &Ctx, v: u32) -> ApiResult {
    Ok(Ret::stdcall(c.nargs(), v))
}

pub fn out(c: &mut Ctx, p: u32, v: u32) {
    if p != 0 {
        let _ = c.host.write_u32(p, v);
    }
}

fn query_interface(c: &mut Ctx) -> ApiResult {
    let (this, riid, ppv) = (c.arg(0), c.arg(1), c.arg(2));
    let mut g = [0u8; 16];
    let _ = c.host.read(riid, &mut g);
    let want = format_iid(&g);
    let Some((base, _)) = object_of(c, this) else {
        out(c, ppv, 0);
        return ret(c, E_NOINTERFACE);
    };
    let ifaces = c.p.com_classes.get(&base).copied().unwrap_or(&[]);
    let hit = if want == IUNKNOWN {
        Some(0)
    } else {
        ifaces.iter().position(|i| i.iids.iter().any(|s| s.eq_ignore_ascii_case(&want)))
    };
    match hit {
        Some(k) => {
            if let Some(o) = c.p.d3d_objects.get_mut(&base) {
                o.refcount += 1;
            }
            out(c, ppv, base + 4 * k as u32);
            c.trace(format!("({want}) = {}", ifaces[k].name));
            ret(c, S_OK)
        }
        None => {
            out(c, ppv, 0);
            let kind = c.p.d3d_objects.get(&base).map(|o| o.kind).unwrap_or("?");
            c.p.log(format!("QueryInterface on {kind}: {want} not supported"));
            ret(c, E_NOINTERFACE)
        }
    }
}

fn add_ref(c: &mut Ctx) -> ApiResult {
    let base = object_of(c, c.arg(0)).map(|o| o.0).unwrap_or(0);
    let n = match c.p.d3d_objects.get_mut(&base) {
        Some(o) => {
            o.refcount += 1;
            o.refcount
        }
        None => 1,
    };
    ret(c, n)
}

fn release(c: &mut Ctx) -> ApiResult {
    let base = object_of(c, c.arg(0)).map(|o| o.0).unwrap_or(0);
    let n = match c.p.d3d_objects.get_mut(&base) {
        Some(o) => {
            o.refcount = o.refcount.saturating_sub(1);
            o.refcount
        }
        None => 0,
    };
    ret(c, n)
}

pub fn okn(c: &mut Ctx) -> ApiResult {
    ret(c, S_OK)
}

pub fn notimpl(c: &mut Ctx) -> ApiResult {
    ret(c, E_NOTIMPL)
}

/// Writes 0 to the last argument (an out pointer) and returns S_OK.
pub fn out_zero(c: &mut Ctx) -> ApiResult {
    let n = c.nargs();
    let p = c.arg(n - 1);
    out(c, p, 0);
    ret(c, S_OK)
}

// ---- Shell.Explorer (WebBrowser control) -------------------------------------------
// Hosted by Delphi's TOleControl in every GM8.x runner (splash_show_web); it never
// renders anything here.

pub const CLSID_WEBBROWSER: &str = "{8856F961-340A-11D0-A96B-00C04FD705A2}";

fn wb_do_verb(c: &mut Ctx) -> ApiResult {
    let (this, verb, parent, rect) = (c.arg(0), c.arg(1) as i32, c.arg(5), c.arg(6));
    if field(c, this, "hwnd") == 0 && parent != 0 {
        let (l, t, r, b) = if rect != 0 { super::user32::read_rect(c, rect) } else { (0, 0, 0, 0) };
        let hwnd = super::user32::create_simple_child(c, "Shell Embedding", parent, l, t, r - l, b - t);
        set_field(c, this, "hwnd", hwnd);
    }
    c.trace(format!("(verb {verb}, parent {parent:#x})"));
    ret(c, S_OK)
}

fn wb_get_window(c: &mut Ctx) -> ApiResult {
    let (this, p) = (c.arg(0), c.arg(1));
    let h = field(c, this, "hwnd");
    out(c, p, h);
    ret(c, if h != 0 { S_OK } else { E_FAIL })
}

fn wb_set_object_rects(c: &mut Ctx) -> ApiResult {
    let (this, pos) = (c.arg(0), c.arg(1));
    let h = field(c, this, "hwnd");
    if h != 0 && pos != 0 {
        let (l, t, r, b) = super::user32::read_rect(c, pos);
        if let Some(w) = c.p.windows.get_mut(&h) {
            w.x = l;
            w.y = t;
            w.w = r - l;
            w.h = b - t;
        }
    }
    ret(c, S_OK)
}

fn wb_set_client_site(c: &mut Ctx) -> ApiResult {
    let (this, site) = (c.arg(0), c.arg(1));
    set_field(c, this, "site", site);
    ret(c, S_OK)
}

fn wb_get_client_site(c: &mut Ctx) -> ApiResult {
    let (this, p) = (c.arg(0), c.arg(1));
    let s = field(c, this, "site");
    out(c, p, s);
    ret(c, S_OK)
}

fn wb_misc_status(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    // SETCLIENTSITEFIRST | ACTIVATEWHENVISIBLE | INSIDEOUT | CANTLINKINSIDE | RECOMPOSEONRESIZE
    out(c, p, 0x0002_0000 | 0x100 | 0x80 | 0x10 | 0x1);
    ret(c, S_OK)
}

fn wb_get_extent(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    out(c, p, 2000);
    out(c, p + 4, 2000);
    ret(c, S_OK)
}

fn wb_get_class_id(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let g = parse_iid(CLSID_WEBBROWSER);
    let _ = c.host.write(p, &g);
    ret(c, S_OK)
}

fn wb_control_info(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let _ = c.host.write_u32(p, 16);
    let _ = c.host.fill(p + 4, 12, 0);
    ret(c, S_OK)
}

fn wb_navigate(c: &mut Ctx) -> ApiResult {
    let url = c.arg(1);
    let s = if url == 0 { String::new() } else { c.host.read_wstr(url).unwrap_or_default() };
    c.p.log(format!("WebBrowser.Navigate({s:?}) ignored"));
    ret(c, S_OK)
}

fn wb_navigate2(c: &mut Ctx) -> ApiResult {
    let v = c.arg(1);
    let vt = c.host.read_u16(v).unwrap_or(0);
    let s = if vt == 8 {
        let b = c.host.read_u32(v + 8).unwrap_or(0);
        if b == 0 { String::new() } else { c.host.read_wstr(b).unwrap_or_default() }
    } else {
        format!("<vt {vt}>")
    };
    c.p.log(format!("WebBrowser.Navigate2({s:?}) ignored"));
    ret(c, S_OK)
}

fn dispatch_invoke(c: &mut Ctx) -> ApiResult {
    let id = c.arg(1) as i32;
    c.trace(format!("(dispid {id})"));
    ret(c, DISP_E_MEMBERNOTFOUND)
}

fn dispatch_ids(c: &mut Ctx) -> ApiResult {
    let (n, ids) = (c.arg(3), c.arg(5));
    for i in 0..n {
        let _ = c.host.write_u32(ids + 4 * i, 0xFFFF_FFFF);
    }
    ret(c, 0x8002_0006)
}

/// IDispatch methods, shared by every dual interface.
macro_rules! dispatch_methods {
    () => {
        [
            ("GetTypeInfoCount", 2, Some(out_zero as ApiFn)),
            ("GetTypeInfo", 4, Some(notimpl as ApiFn)),
            ("GetIDsOfNames", 6, Some(dispatch_ids as ApiFn)),
            ("Invoke", 9, Some(dispatch_invoke as ApiFn)),
        ]
    };
}

static OLE_OBJECT: Interface = Interface {
    name: "IOleObject(WebBrowser)",
    iids: &["{00000112-0000-0000-C000-000000000046}"],
    methods: &[
        ("SetClientSite", 2, Some(wb_set_client_site)),
        ("GetClientSite", 2, Some(wb_get_client_site)),
        ("SetHostNames", 3, Some(okn)),
        ("Close", 2, Some(okn)),
        ("SetMoniker", 3, Some(notimpl)),
        ("GetMoniker", 4, Some(notimpl)),
        ("InitFromData", 4, Some(notimpl)),
        ("GetClipboardData", 3, Some(notimpl)),
        ("DoVerb", 7, Some(wb_do_verb)),
        ("EnumVerbs", 2, Some(notimpl)),
        ("Update", 1, Some(okn)),
        ("IsUpToDate", 1, Some(okn)),
        ("GetUserClassID", 2, Some(wb_get_class_id)),
        ("GetUserType", 3, Some(notimpl)),
        ("SetExtent", 3, Some(okn)),
        ("GetExtent", 3, Some(wb_get_extent)),
        ("Advise", 3, Some(out_zero)),
        ("Unadvise", 2, Some(okn)),
        ("EnumAdvise", 2, Some(notimpl)),
        ("GetMiscStatus", 3, Some(wb_misc_status)),
        ("SetColorScheme", 2, Some(notimpl)),
    ],
};

static PERSIST_STREAM_INIT: Interface = Interface {
    name: "IPersistStreamInit(WebBrowser)",
    iids: &["{7FD52380-4E07-101B-AE2D-08002B2EC713}", "{00000109-0000-0000-C000-000000000046}", "{0000010C-0000-0000-C000-000000000046}"],
    methods: &[
        ("GetClassID", 2, Some(wb_get_class_id)),
        ("IsDirty", 1, Some(|c| ret(c, S_FALSE))),
        ("Load", 2, Some(okn)),
        ("Save", 3, Some(okn)),
        ("GetSizeMax", 2, Some(|c| {
            let p = c.arg(1);
            if p != 0 {
                let _ = c.host.write_u64(p, 0);
            }
            ret(c, S_OK)
        })),
        ("InitNew", 1, Some(okn)),
    ],
};

static OLE_CONTROL: Interface = Interface {
    name: "IOleControl(WebBrowser)",
    iids: &["{B196B288-BAB4-101A-B69C-00AA00341D07}"],
    methods: &[
        ("GetControlInfo", 2, Some(wb_control_info)),
        ("OnMnemonic", 2, Some(okn)),
        ("OnAmbientPropertyChange", 2, Some(okn)),
        ("FreezeEvents", 2, Some(okn)),
    ],
};

static OLE_IN_PLACE_OBJECT: Interface = Interface {
    name: "IOleInPlaceObject(WebBrowser)",
    iids: &["{00000113-0000-0000-C000-000000000046}", "{00000114-0000-0000-C000-000000000046}"],
    methods: &[
        ("GetWindow", 2, Some(wb_get_window)),
        ("ContextSensitiveHelp", 2, Some(okn)),
        ("InPlaceDeactivate", 1, Some(okn)),
        ("UIDeactivate", 1, Some(okn)),
        ("SetObjectRects", 3, Some(wb_set_object_rects)),
        ("ReactivateAndUndo", 1, Some(okn)),
    ],
};

static OLE_IN_PLACE_ACTIVE_OBJECT: Interface = Interface {
    name: "IOleInPlaceActiveObject(WebBrowser)",
    iids: &["{00000117-0000-0000-C000-000000000046}"],
    methods: &[
        ("GetWindow", 2, Some(wb_get_window)),
        ("ContextSensitiveHelp", 2, Some(okn)),
        ("TranslateAccelerator", 2, Some(|c| ret(c, S_FALSE))),
        ("OnFrameWindowActivate", 2, Some(okn)),
        ("OnDocWindowActivate", 2, Some(okn)),
        ("ResizeBorder", 4, Some(okn)),
        ("EnableModeless", 2, Some(okn)),
    ],
};

/// IWebBrowser2 in vtable order (IDispatch, IWebBrowser, IWebBrowserApp, IWebBrowser2).
static WEB_BROWSER2: Interface = Interface {
    name: "IWebBrowser2",
    iids: &[
        "{D30C1661-CDAF-11D0-8A3E-00C04FC9E26E}",
        "{0002DF05-0000-0000-C000-000000000046}",
        "{EAB22AC1-30C1-11CF-A7EB-0000C05BAE0B}",
        "{00020400-0000-0000-C000-000000000046}",
    ],
    methods: &{
        let d = dispatch_methods!();
        [
            d[0], d[1], d[2], d[3],
            ("GoBack", 1, Some(okn as ApiFn)),
            ("GoForward", 1, Some(okn as ApiFn)),
            ("GoHome", 1, Some(okn as ApiFn)),
            ("GoSearch", 1, Some(okn as ApiFn)),
            ("Navigate", 6, Some(wb_navigate as ApiFn)),
            ("Refresh", 1, Some(okn as ApiFn)),
            ("Refresh2", 2, Some(okn as ApiFn)),
            ("Stop", 1, Some(okn as ApiFn)),
            ("get_Application", 2, Some(out_zero as ApiFn)),
            ("get_Parent", 2, Some(out_zero as ApiFn)),
            ("get_Container", 2, Some(out_zero as ApiFn)),
            ("get_Document", 2, Some(out_zero as ApiFn)),
            ("get_TopLevelContainer", 2, Some(out_zero as ApiFn)),
            ("get_Type", 2, Some(out_zero as ApiFn)),
            ("get_Left", 2, Some(out_zero as ApiFn)),
            ("put_Left", 2, Some(okn as ApiFn)),
            ("get_Top", 2, Some(out_zero as ApiFn)),
            ("put_Top", 2, Some(okn as ApiFn)),
            ("get_Width", 2, Some(out_zero as ApiFn)),
            ("put_Width", 2, Some(okn as ApiFn)),
            ("get_Height", 2, Some(out_zero as ApiFn)),
            ("put_Height", 2, Some(okn as ApiFn)),
            ("get_LocationName", 2, Some(out_zero as ApiFn)),
            ("get_LocationURL", 2, Some(out_zero as ApiFn)),
            ("get_Busy", 2, Some(out_zero as ApiFn)),
            ("Quit", 1, Some(okn as ApiFn)),
            ("ClientToWindow", 3, Some(okn as ApiFn)),
            ("PutProperty", 6, Some(okn as ApiFn)),
            ("GetProperty", 3, Some(notimpl as ApiFn)),
            ("get_Name", 2, Some(out_zero as ApiFn)),
            ("get_HWND", 2, Some(out_zero as ApiFn)),
            ("get_FullName", 2, Some(out_zero as ApiFn)),
            ("get_Path", 2, Some(out_zero as ApiFn)),
            ("get_Visible", 2, Some(out_zero as ApiFn)),
            ("put_Visible", 2, Some(okn as ApiFn)),
            ("get_StatusBar", 2, Some(out_zero as ApiFn)),
            ("put_StatusBar", 2, Some(okn as ApiFn)),
            ("get_StatusText", 2, Some(out_zero as ApiFn)),
            ("put_StatusText", 2, Some(okn as ApiFn)),
            ("get_ToolBar", 2, Some(out_zero as ApiFn)),
            ("put_ToolBar", 2, Some(okn as ApiFn)),
            ("get_MenuBar", 2, Some(out_zero as ApiFn)),
            ("put_MenuBar", 2, Some(okn as ApiFn)),
            ("get_FullScreen", 2, Some(out_zero as ApiFn)),
            ("put_FullScreen", 2, Some(okn as ApiFn)),
            ("Navigate2", 6, Some(wb_navigate2 as ApiFn)),
            ("QueryStatusWB", 3, Some(out_zero as ApiFn)),
            ("ExecWB", 5, Some(okn as ApiFn)),
            ("ShowBrowserBar", 4, Some(okn as ApiFn)),
            ("get_ReadyState", 2, Some(|c: &mut Ctx| {
                let p = c.arg(1);
                out(c, p, 4); // READYSTATE_COMPLETE
                ret(c, S_OK)
            })),
            ("get_Offline", 2, Some(out_zero as ApiFn)),
            ("put_Offline", 2, Some(okn as ApiFn)),
            ("get_Silent", 2, Some(out_zero as ApiFn)),
            ("put_Silent", 2, Some(okn as ApiFn)),
            ("get_RegisterAsBrowser", 2, Some(out_zero as ApiFn)),
            ("put_RegisterAsBrowser", 2, Some(okn as ApiFn)),
            ("get_RegisterAsDropTarget", 2, Some(out_zero as ApiFn)),
            ("put_RegisterAsDropTarget", 2, Some(okn as ApiFn)),
            ("get_TheaterMode", 2, Some(out_zero as ApiFn)),
            ("put_TheaterMode", 2, Some(okn as ApiFn)),
            ("get_AddressBar", 2, Some(out_zero as ApiFn)),
            ("put_AddressBar", 2, Some(okn as ApiFn)),
            ("get_Resizable", 2, Some(out_zero as ApiFn)),
            ("put_Resizable", 2, Some(okn as ApiFn)),
        ]
    },
};

static WEBBROWSER: [&Interface; 6] = [&OLE_OBJECT, &PERSIST_STREAM_INIT, &OLE_CONTROL, &OLE_IN_PLACE_OBJECT, &WEB_BROWSER2, &OLE_IN_PLACE_ACTIVE_OBJECT];

// ---- DirectMusic (GM8 plays MIDI through it; R4 connects it to a synthesizer) -------

fn dm_init_audio(c: &mut Ctx) -> ApiResult {
    let (pp_dm, pp_ds, hwnd) = (c.arg(1), c.arg(2), c.arg(3));
    for p in [pp_dm, pp_ds] {
        if p != 0 && c.host.read_u32(p).unwrap_or(0) == 0 {
            // Callers that ask for the device objects get none; GM8 passes NULL.
            let _ = c.host.write_u32(p, 0);
        }
    }
    c.trace(format!("(hwnd {hwnd:#x}, path {}, pchannels {}, flags {:#x})", c.arg(4), c.arg(5), c.arg(6)));
    ret(c, S_OK)
}

fn dm_get_time(c: &mut Ctx) -> ApiResult {
    let (prt, pmt) = (c.arg(1), c.arg(2));
    let us = c.p.clock_us;
    if prt != 0 {
        let _ = c.host.write_u64(prt, us * 10);
    }
    if pmt != 0 {
        // 120 bpm, 768 ticks per quarter note
        let _ = c.host.write_u32(pmt, (us * 768 * 2 / 1_000_000) as u32);
    }
    ret(c, S_OK)
}

fn dm_unsupported(c: &mut Ctx) -> ApiResult {
    let n = c.name();
    c.p.log(format!("{n}: not implemented, reporting failure"));
    ret(c, E_FAIL)
}

static DM_PERFORMANCE8: Interface = Interface {
    name: "IDirectMusicPerformance8",
    iids: &[
        "{679C4137-C62E-4147-B2B4-9D569ACB254C}",
        "{07D43D03-6523-11D2-871D-00600893B1BD}",
        "{6FC2CAE0-BC78-11D2-AFA6-00AA0024D8B6}",
    ],
    methods: &[
        ("Init", 4, Some(dm_init_audio)),
        ("PlaySegment", 6, Some(dm_unsupported)),
        ("Stop", 5, Some(okn)),
        ("GetSegmentState", 3, Some(dm_unsupported)),
        ("SetPrepareTime", 2, Some(okn)),
        ("GetPrepareTime", 2, Some(out_zero)),
        ("SetBumperLength", 2, Some(okn)),
        ("GetBumperLength", 2, Some(out_zero)),
        ("SendPMsg", 2, Some(dm_unsupported)),
        ("MusicToReferenceTime", 3, Some(dm_unsupported)),
        ("ReferenceToMusicTime", 4, Some(dm_unsupported)),
        ("IsPlaying", 3, Some(|c| ret(c, S_FALSE))),
        ("GetTime", 3, Some(dm_get_time)),
        ("AllocPMsg", 3, Some(dm_unsupported)),
        ("FreePMsg", 2, Some(okn)),
        ("GetGraph", 2, Some(dm_unsupported)),
        ("SetGraph", 2, Some(okn)),
        ("SetNotificationHandle", 4, Some(okn)),
        ("GetNotificationPMsg", 2, Some(|c| ret(c, S_FALSE))),
        ("AddNotificationType", 2, Some(okn)),
        ("RemoveNotificationType", 2, Some(okn)),
        ("AddPort", 2, Some(okn)),
        ("RemovePort", 2, Some(okn)),
        ("AssignPChannelBlock", 4, Some(okn)),
        ("AssignPChannel", 5, Some(okn)),
        ("PChannelInfo", 5, Some(dm_unsupported)),
        ("DownloadInstrument", 9, Some(dm_unsupported)),
        ("Invalidate", 3, Some(okn)),
        ("GetParam", 7, Some(dm_unsupported)),
        ("SetParam", 6, Some(okn)),
        ("GetGlobalParam", 4, Some(dm_unsupported)),
        ("SetGlobalParam", 4, Some(okn)),
        ("GetLatencyTime", 2, Some(dm_get_time)),
        ("GetQueueTime", 2, Some(dm_get_time)),
        ("AdjustTime", 3, Some(okn)),
        ("CloseDown", 1, Some(okn)),
        ("GetResolvedTime", 5, Some(dm_unsupported)),
        ("MIDIToMusic", 6, Some(dm_unsupported)),
        ("MusicToMIDI", 6, Some(dm_unsupported)),
        ("TimeToRhythm", 7, Some(dm_unsupported)),
        ("RhythmToTime", 7, Some(dm_unsupported)),
        ("InitAudio", 8, Some(dm_init_audio)),
        ("PlaySegmentEx", 10, Some(dm_unsupported)),
        ("StopEx", 5, Some(okn)),
        ("ClonePMsg", 3, Some(dm_unsupported)),
        ("CreateAudioPath", 4, Some(dm_unsupported)),
        ("CreateStandardAudioPath", 5, Some(dm_unsupported)),
        ("SetDefaultAudioPath", 2, Some(okn)),
        ("GetDefaultAudioPath", 2, Some(dm_unsupported)),
        ("GetParamEx", 8, Some(dm_unsupported)),
        ("SetParamEx", 7, Some(okn)),
    ],
};

static DM_LOADER8: Interface = Interface {
    name: "IDirectMusicLoader8",
    iids: &["{19E7C08C-0A44-4E6A-A116-595A7CD5DE8C}", "{2FFAACA2-5DCA-11D2-AFA6-00AA0024D8B6}"],
    methods: &[
        ("GetObject", 4, Some(dm_unsupported)),
        ("SetObject", 2, Some(dm_unsupported)),
        ("SetSearchDirectory", 4, Some(okn)),
        ("ScanDirectory", 4, Some(okn)),
        ("CacheObject", 2, Some(okn)),
        ("ReleaseObject", 2, Some(okn)),
        ("ClearCache", 2, Some(okn)),
        ("EnableCache", 3, Some(okn)),
        ("EnumObject", 4, Some(|c| ret(c, S_FALSE))),
        ("CollectGarbage", 1, Some(okn)),
        ("ReleaseObjectByUnknown", 2, Some(okn)),
        ("LoadObjectFromFile", 5, Some(dm_unsupported)),
    ],
};

static DM_PERFORMANCE_CLASS: [&Interface; 1] = [&DM_PERFORMANCE8];
static DM_LOADER_CLASS: [&Interface; 1] = [&DM_LOADER8];

/// (CLSID, object kind, interfaces) of every class CoCreateInstance can make.
static CLASSES: [(&str, &str, &[&Interface]); 3] = [
    (CLSID_WEBBROWSER, "WebBrowser", &WEBBROWSER),
    ("{D2AC2881-B39B-11D1-8704-00600893B1BD}", "DirectMusicPerformance", &DM_PERFORMANCE_CLASS),
    ("{D2AC2892-B39B-11D1-8704-00600893B1BD}", "DirectMusicLoader", &DM_LOADER_CLASS),
];

/// CoCreateInstance for the classes implemented here; None if the CLSID is unknown.
pub fn create_instance(c: &mut Ctx, clsid: &str, iid_p: u32, ppv: u32) -> Option<u32> {
    let &(_, kind, ifaces) = CLASSES.iter().find(|(id, _, _)| id.eq_ignore_ascii_case(clsid))?;
    let obj = create(c, kind, ifaces, HashMap::new());
    let mut g = [0u8; 16];
    let _ = c.host.read(iid_p, &mut g);
    let want = format_iid(&g);
    let k = if want == IUNKNOWN { Some(0) } else { ifaces.iter().position(|i| i.iids.iter().any(|s| s.eq_ignore_ascii_case(&want))) };
    Some(match k {
        Some(k) => {
            out(c, ppv, obj + 4 * k as u32);
            S_OK
        }
        None => {
            c.p.log(format!("CoCreateInstance({clsid}): interface {want} not supported"));
            out(c, ppv, 0);
            E_NOINTERFACE
        }
    })
}

pub fn register(r: &mut Registry) {
    for (_, _, ifaces) in CLASSES.iter() {
        for i in ifaces.iter() {
            register_interface(r, i);
        }
    }
}
