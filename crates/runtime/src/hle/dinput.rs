//! DirectInput 8: the system keyboard and mouse and every connected controller as a
//! joystick (Windows lists an Xbox controller to DirectInput as well as to XInput),
//! over the same host input as user32 and XInput. Devices report their state in the
//! standard data formats (c_dfDIKeyboard, c_dfDIMouse/2, c_dfDIJoystick/2); buffered
//! data is not kept (GetDeviceData returns no items).

use super::com::{self, Interface, Method, field, out, out_ptr, ret, set_field};
use super::{ApiResult, Ctx, Registry, Ret};
use crate::fasthash::HashMap;
use crate::mem::Guest;

const DI_OK: u32 = 0;
const DI_NOTATTACHED: u32 = 1;
const DIERR_UNSUPPORTED: u32 = 0x8000_4001;
const DIERR_NOINTERFACE: u32 = 0x8000_4002;
const DIERR_DEVICENOTREG: u32 = 0x8004_0154;
const DIERR_INVALIDPARAM: u32 = 0x8007_0057;
const DIERR_NOTACQUIRED: u32 = 0x8007_000C;

const IID_DI8A: &str = "{BF798030-483A-4DA2-AA99-5D64ED369700}";
const IID_DI8W: &str = "{BF798031-483A-4DA2-AA99-5D64ED369700}";
const GUID_SYS_KEYBOARD: &str = "{6F1D2B61-D5A0-11CF-BFC7-444553540000}";
const GUID_SYS_MOUSE: &str = "{6F1D2B60-D5A0-11CF-BFC7-444553540000}";
/// Instance GUIDs of the controllers: this with the pad index added to Data1.
const GUID_PAD_BASE: &str = "{6F1D2B70-D5A0-11CF-BFC7-444553540000}";
/// "PIDVID" product GUID of the Xbox 360 controller (VID 045E, PID 028E).
const GUID_PAD_PRODUCT: &str = "{028E045E-0000-0000-0000-504944564944}";
const PAD_NAME: &str = "Controller (XBOX 360 For Windows)";

const KEYBOARD: u32 = 1;
const MOUSE: u32 = 2;
const JOYSTICK: u32 = 3;

/// DI8DEVTYPE (with subtype) of each kind.
fn dev_type(kind: u32) -> u32 {
    match kind {
        KEYBOARD => 0x413, // keyboard, PC enhanced
        MOUSE => 0x112,    // mouse, unknown subtype
        _ => 0x10215,      // gamepad, standard, HID
    }
}

fn guid_bytes(s: &str) -> [u8; 16] {
    com::parse_iid(s)
}

fn pad_guid(i: u32) -> [u8; 16] {
    let mut g = guid_bytes(GUID_PAD_BASE);
    let d1 = u32::from_le_bytes(g[..4].try_into().unwrap()) + i;
    g[..4].copy_from_slice(&d1.to_le_bytes());
    g
}

/// The device kind and pad index of an instance GUID.
fn device_of(g: &[u8; 16]) -> Option<(u32, u32)> {
    if *g == guid_bytes(GUID_SYS_KEYBOARD) {
        return Some((KEYBOARD, 0));
    }
    if *g == guid_bytes(GUID_SYS_MOUSE) {
        return Some((MOUSE, 0));
    }
    (0..4).find(|&i| *g == pad_guid(i)).map(|i| (JOYSTICK, i))
}

fn read_guid(c: &Ctx, p: u32) -> [u8; 16] {
    let mut g = [0u8; 16];
    let _ = c.host.read(p, &mut g);
    g
}

fn pad_connected(c: &mut Ctx, i: u32) -> bool {
    super::pads::connected_pad(c, i).is_some()
}

/// Writes a TCHAR string field of `chars` characters.
fn write_tstr(c: &mut Ctx, at: u32, chars: u32, s: &str, wide: bool) {
    if wide {
        let mut units: Vec<u16> = s.encode_utf16().take(chars as usize - 1).collect();
        units.push(0);
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let _ = c.host.write(at, &bytes);
    } else {
        let mut b: Vec<u8> = s.bytes().take(chars as usize - 1).collect();
        b.push(0);
        let _ = c.host.write(at, &b);
    }
}

/// Fills a DIDEVICEINSTANCE(A/W) at `at`.
fn write_instance(c: &mut Ctx, at: u32, kind: u32, index: u32, wide: bool) {
    let cs = if wide { 2 } else { 1 };
    let size = 40 + 2 * 260 * cs + 20;
    let _ = c.host.write(at, &vec![0u8; size as usize]);
    let _ = c.host.write_u32(at, size);
    let (inst, product, name) = match kind {
        KEYBOARD => (guid_bytes(GUID_SYS_KEYBOARD), guid_bytes(GUID_SYS_KEYBOARD), "Keyboard"),
        MOUSE => (guid_bytes(GUID_SYS_MOUSE), guid_bytes(GUID_SYS_MOUSE), "Mouse"),
        _ => (pad_guid(index), guid_bytes(GUID_PAD_PRODUCT), PAD_NAME),
    };
    let _ = c.host.write(at + 4, &inst);
    let _ = c.host.write(at + 20, &product);
    let _ = c.host.write_u32(at + 36, dev_type(kind));
    write_tstr(c, at + 40, 260, name, wide);
    write_tstr(c, at + 40 + 260 * cs, 260, name, wide);
    // HID usage page/usage: generic desktop gamepad (5), keyboard (6), mouse (2)
    let usage = match kind {
        KEYBOARD => 6,
        MOUSE => 2,
        _ => 5,
    };
    let tail = at + 40 + 2 * 260 * cs + 16;
    let _ = c.host.write_u16(tail, 1);
    let _ = c.host.write_u16(tail + 2, usage);
}

fn wide(c: &Ctx, this: u32) -> bool {
    field(c, this, "wide") != 0
}

// ---- IDirectInput8 -------------------------------------------------------------------------

fn create_device(c: &mut Ctx) -> ApiResult {
    let (this, rguid, pp) = (c.arg(0), c.arg(1), c.arg(2));
    let g = read_guid(c, rguid);
    let Some((kind, index)) = device_of(&g) else {
        out_ptr(c, pp, 0);
        return ret(c, DIERR_DEVICENOTREG);
    };
    if kind == JOYSTICK && !pad_connected(c, index) {
        out_ptr(c, pp, 0);
        return ret(c, DIERR_DEVICENOTREG);
    }
    let w = wide(c, this);
    let data = HashMap::from_iter([("kind", kind), ("index", index), ("wide", w as u32), ("rmin", 0), ("rmax", 65535)]);
    let class: &'static [&'static Interface] = if w { &DEVICE_W_CLASS } else { &DEVICE_A_CLASS };
    let dev = com::create(c, "DirectInputDevice8", class, data);
    c.trace(format!(" = {dev:#x} ({})", ["", "keyboard", "mouse", "joystick"][kind as usize]));
    out_ptr(c, pp, dev);
    ret(c, DI_OK)
}

fn enum_devices(c: &mut Ctx) -> ApiResult {
    let (this, ty, cb, ctx) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
    let w = wide(c, this);
    // DI8DEVCLASS_* (0 all, 1 other devices, 2 pointer, 3 keyboard, 4 game controllers)
    // or a DI8DEVTYPE_* (0x12 mouse, 0x13 keyboard, 0x14..0x18 game controllers)
    let wants = |kind: u32| match ty & 0xFF {
        0 => true,
        2 | 0x12 => kind == MOUSE,
        3 | 0x13 => kind == KEYBOARD,
        4 | 0x14..=0x18 => kind == JOYSTICK,
        _ => false,
    };
    let mut list = Vec::new();
    if wants(KEYBOARD) {
        list.push((KEYBOARD, 0));
    }
    if wants(MOUSE) {
        list.push((MOUSE, 0));
    }
    if wants(JOYSTICK) {
        for i in 0..4 {
            if pad_connected(c, i) {
                list.push((JOYSTICK, i));
            }
        }
    }
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let buf = c.p.heap_alloc(g, heap, 1200, true);
    for (kind, index) in list {
        write_instance(c, buf, kind, index, w);
        if c.call_guest(cb, &[buf, ctx])? == 0 {
            break; // DIENUM_STOP
        }
    }
    c.p.heap_free(buf);
    ret(c, DI_OK)
}

fn get_device_status(c: &mut Ctx) -> ApiResult {
    let g = read_guid(c, c.arg(1));
    let r = match device_of(&g) {
        Some((JOYSTICK, i)) => {
            if pad_connected(c, i) {
                DI_OK
            } else {
                DI_NOTATTACHED
            }
        }
        Some(_) => DI_OK,
        None => DIERR_DEVICENOTREG,
    };
    ret(c, r)
}

const DI8_METHODS: &[Method] = &[
    ("CreateDevice", 4, Some(create_device)),
    ("EnumDevices", 5, Some(enum_devices)),
    ("GetDeviceStatus", 2, Some(get_device_status)),
    ("RunControlPanel", 3, Some(okn)),
    ("Initialize", 3, Some(okn)),
    ("FindDevice", 4, Some(|c: &mut Ctx| ret(c, DIERR_DEVICENOTREG))),
    ("EnumDevicesBySemantics", 6, Some(okn)),
    ("ConfigureDevices", 5, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
];

static DI8A: Interface = Interface { name: "IDirectInput8A", iids: &[IID_DI8A], methods: DI8_METHODS };
static DI8W: Interface = Interface { name: "IDirectInput8W", iids: &[IID_DI8W], methods: DI8_METHODS };
static DI8A_CLASS: [&Interface; 1] = [&DI8A];
static DI8W_CLASS: [&Interface; 1] = [&DI8W];

fn okn(c: &mut Ctx) -> ApiResult {
    ret(c, DI_OK)
}

// ---- IDirectInputDevice8 -------------------------------------------------------------------

fn get_capabilities(c: &mut Ctx) -> ApiResult {
    let (this, p) = (c.arg(0), c.arg(1));
    let kind = field(c, this, "kind");
    let (axes, buttons, povs) = match kind {
        KEYBOARD => (0, 128, 0),
        MOUSE => (3, 3, 0),
        _ => (5, 10, 1),
    };
    // DIDEVCAPS: size, flags (attached), type, axes, buttons, POVs, then force feedback
    for (k, v) in [44, 1, dev_type(kind), axes, buttons, povs, 0, 0, 0, 0, 0].iter().enumerate() {
        let _ = c.host.write_u32(p + 4 * k as u32, *v);
    }
    ret(c, DI_OK)
}

/// (guid, offset in DIJOYSTATE, DIDFT type) of a controller's objects.
fn pad_objects() -> Vec<(&'static str, u32, u32, &'static str)> {
    const AXIS: u32 = 2; // DIDFT_ABSAXIS
    const BUTTON: u32 = 4; // DIDFT_PSHBUTTON
    const POV: u32 = 0x10;
    let mut v = vec![
        ("{A36D02E0-C9F3-11CF-BFC7-444553540000}", 0, AXIS, "X Axis"),
        ("{A36D02E1-C9F3-11CF-BFC7-444553540000}", 4, AXIS | (1 << 8), "Y Axis"),
        ("{A36D02E2-C9F3-11CF-BFC7-444553540000}", 8, AXIS | (2 << 8), "Z Axis"),
        ("{A36D02F4-C9F3-11CF-BFC7-444553540000}", 12, AXIS | (3 << 8), "X Rotation"),
        ("{A36D02F5-C9F3-11CF-BFC7-444553540000}", 16, AXIS | (4 << 8), "Y Rotation"),
        ("{A36D02F2-C9F3-11CF-BFC7-444553540000}", 32, POV, "Hat Switch"),
    ];
    const NAMES: [&str; 10] = ["Button 0", "Button 1", "Button 2", "Button 3", "Button 4", "Button 5", "Button 6", "Button 7", "Button 8", "Button 9"];
    for (b, name) in NAMES.iter().enumerate() {
        v.push(("{A36D02F0-C9F3-11CF-BFC7-444553540000}", 48 + b as u32, BUTTON | ((b as u32) << 8), name));
    }
    v
}

fn enum_objects(c: &mut Ctx) -> ApiResult {
    let (this, cb, ctx, flags) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
    if field(c, this, "kind") != JOYSTICK {
        return ret(c, DI_OK);
    }
    let w = wide(c, this);
    let cs = if w { 2 } else { 1 };
    let size = 32 + 260 * cs + 24;
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let buf = c.p.heap_alloc(g, heap, size, true);
    for (guid, ofs, ty, name) in pad_objects() {
        // DIDFT_ALL, or the classes asked for (axes 3, buttons 0xC, POVs 0x10)
        if flags & 0x1F != 0 && flags & 0x1F & ty == 0 {
            continue;
        }
        let _ = c.host.write(buf, &vec![0u8; size as usize]);
        let _ = c.host.write_u32(buf, size);
        let _ = c.host.write(buf + 4, &guid_bytes(guid));
        let _ = c.host.write_u32(buf + 20, ofs);
        let _ = c.host.write_u32(buf + 24, ty);
        write_tstr(c, buf + 32, 260, name, w);
        if c.call_guest(cb, &[buf, ctx])? == 0 {
            break;
        }
    }
    c.p.heap_free(buf);
    ret(c, DI_OK)
}

// DIPROP_* are small integers passed as the GUID pointer.
const DIPROP_BUFFERSIZE: u32 = 1;
const DIPROP_RANGE: u32 = 4;
const DIPROP_INSTANCENAME: u32 = 13;
const DIPROP_PRODUCTNAME: u32 = 14;
const DIPROP_VIDPID: u32 = 24;

fn get_property(c: &mut Ctx) -> ApiResult {
    let (this, prop, ph) = (c.arg(0), c.arg(1), c.arg(2));
    let kind = field(c, this, "kind");
    match prop {
        DIPROP_RANGE if kind == JOYSTICK => {
            let (lo, hi) = (field(c, this, "rmin"), field(c, this, "rmax"));
            let _ = c.host.write_u32(ph + 16, lo);
            let _ = c.host.write_u32(ph + 20, hi);
        }
        DIPROP_BUFFERSIZE => {
            let n = field(c, this, "buffer");
            let _ = c.host.write_u32(ph + 16, n);
        }
        DIPROP_VIDPID if kind == JOYSTICK => {
            let _ = c.host.write_u32(ph + 16, 0x028E_045E);
        }
        DIPROP_INSTANCENAME | DIPROP_PRODUCTNAME => {
            let name = match kind {
                KEYBOARD => "Keyboard",
                MOUSE => "Mouse",
                _ => PAD_NAME,
            };
            write_tstr(c, ph + 16, 260, name, true);
        }
        _ => return ret(c, DIERR_UNSUPPORTED),
    }
    ret(c, DI_OK)
}

fn set_property(c: &mut Ctx) -> ApiResult {
    let (this, prop, ph) = (c.arg(0), c.arg(1), c.arg(2));
    match prop {
        DIPROP_RANGE => {
            // One range for every axis (games set them all alike).
            let lo = c.host.read_u32(ph + 16).unwrap_or(0);
            let hi = c.host.read_u32(ph + 20).unwrap_or(65535);
            if (lo as i32) >= (hi as i32) {
                return ret(c, DIERR_INVALIDPARAM);
            }
            set_field(c, this, "rmin", lo);
            set_field(c, this, "rmax", hi);
        }
        DIPROP_BUFFERSIZE => {
            let n = c.host.read_u32(ph + 16).unwrap_or(0);
            set_field(c, this, "buffer", n);
        }
        _ => {}
    }
    ret(c, DI_OK)
}

fn acquire(c: &mut Ctx) -> ApiResult {
    let this = c.arg(0);
    // a fresh acquisition starts the mouse's relative motion from here
    let (x, y) = c.p.mouse;
    set_field(c, this, "mx", x as u32);
    set_field(c, this, "my", y as u32);
    set_field(c, this, "acquired", 1);
    ret(c, DI_OK)
}

fn unacquire(c: &mut Ctx) -> ApiResult {
    let this = c.arg(0);
    set_field(c, this, "acquired", 0);
    ret(c, DI_OK)
}

fn get_device_state(c: &mut Ctx) -> ApiResult {
    let (this, size, p) = (c.arg(0), c.arg(1), c.arg(2));
    if field(c, this, "acquired") == 0 {
        return ret(c, DIERR_NOTACQUIRED);
    }
    super::user32::pump_input(c);
    let mut s = vec![0u8; size as usize];
    match field(c, this, "kind") {
        KEYBOARD => {
            let n = s.len().min(256);
            s[..n].copy_from_slice(&c.p.dik[..n]);
        }
        MOUSE => {
            // DIMOUSESTATE(2): relative x, y, wheel, then 4 (8) buttons
            let (x, y) = c.p.mouse;
            let (lx, ly) = (field(c, this, "mx") as i32, field(c, this, "my") as i32);
            set_field(c, this, "mx", x as u32);
            set_field(c, this, "my", y as u32);
            let vals = [x - lx, y - ly, 0];
            for (k, v) in vals.iter().enumerate() {
                if s.len() >= 4 * k + 4 {
                    s[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            for (k, bit) in [1u32, 2, 4].iter().enumerate() {
                if s.len() > 12 + k && c.p.mouse_buttons & bit != 0 {
                    s[12 + k] = 0x80;
                }
            }
        }
        _ => {
            // DIJOYSTATE(2): X, Y, Z, Rx, Ry, Rz, 2 sliders, 4 POVs, 32 buttons, ...
            let index = field(c, this, "index");
            let (lo, hi) = (field(c, this, "rmin") as i32 as i64, field(c, this, "rmax") as i32 as i64);
            let scale = |v: u32| -> i32 { (lo + (v as i64) * (hi - lo) / 65535) as i32 };
            let pad = super::pads::connected_pad(c, index);
            let put = |s: &mut Vec<u8>, off: usize, v: u32| {
                if s.len() >= off + 4 {
                    s[off..off + 4].copy_from_slice(&v.to_le_bytes());
                }
            };
            let centered = scale(32767) as u32;
            match pad {
                Some(p) => {
                    let j = super::pads::joy(&p);
                    for (k, v) in [j.x, j.y, j.z, j.rx, j.ry].iter().enumerate() {
                        put(&mut s, 4 * k, scale(*v) as u32);
                    }
                    put(&mut s, 20, centered);
                    put(&mut s, 32, j.pov.unwrap_or(u32::MAX));
                    for k in 1..4 {
                        put(&mut s, 32 + 4 * k, u32::MAX);
                    }
                    for b in 0..10 {
                        if s.len() > 48 + b && j.buttons & (1 << b) != 0 {
                            s[48 + b] = 0x80;
                        }
                    }
                }
                None => {
                    for k in 0..6 {
                        put(&mut s, 4 * k, centered);
                    }
                    for k in 0..4 {
                        put(&mut s, 32 + 4 * k, u32::MAX);
                    }
                }
            }
        }
    }
    let _ = c.host.write(p, &s);
    ret(c, DI_OK)
}

fn get_device_data(c: &mut Ctx) -> ApiResult {
    let inout = c.arg(3);
    out(c, inout, 0);
    ret(c, DI_OK)
}

fn get_device_info(c: &mut Ctx) -> ApiResult {
    let (this, p) = (c.arg(0), c.arg(1));
    let (kind, index, w) = (field(c, this, "kind"), field(c, this, "index"), wide(c, this));
    write_instance(c, p, kind, index, w);
    ret(c, DI_OK)
}

const DEVICE_METHODS: &[Method] = &[
    ("GetCapabilities", 2, Some(get_capabilities)),
    ("EnumObjects", 4, Some(enum_objects)),
    ("GetProperty", 3, Some(get_property)),
    ("SetProperty", 3, Some(set_property)),
    ("Acquire", 1, Some(acquire)),
    ("Unacquire", 1, Some(unacquire)),
    ("GetDeviceState", 3, Some(get_device_state)),
    ("GetDeviceData", 5, Some(get_device_data)),
    ("SetDataFormat", 2, Some(okn)),
    ("SetEventNotification", 2, Some(okn)),
    ("SetCooperativeLevel", 3, Some(okn)),
    ("GetObjectInfo", 4, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("GetDeviceInfo", 2, Some(get_device_info)),
    ("RunControlPanel", 3, Some(okn)),
    ("Initialize", 4, Some(okn)),
    ("CreateEffect", 5, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("EnumEffects", 4, Some(okn)),
    ("GetEffectInfo", 3, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("GetForceFeedbackState", 2, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("SendForceFeedbackCommand", 2, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("EnumCreatedEffectObjects", 4, Some(okn)),
    ("Escape", 2, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("Poll", 1, Some(|c: &mut Ctx| ret(c, if field(c, c.arg(0), "acquired") != 0 { DI_OK } else { DIERR_NOTACQUIRED }))),
    ("SendDeviceData", 5, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("EnumEffectsInFile", 5, Some(okn)),
    ("WriteEffectToFile", 5, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("BuildActionMap", 4, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("SetActionMap", 4, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
    ("GetImageInfo", 2, Some(|c: &mut Ctx| ret(c, DIERR_UNSUPPORTED))),
];

static DEVICE_A: Interface = Interface {
    name: "IDirectInputDevice8A",
    iids: &["{54D41080-DC15-4833-A41B-748F73A38179}"],
    methods: DEVICE_METHODS,
};
static DEVICE_W: Interface = Interface {
    name: "IDirectInputDevice8W",
    iids: &["{54D41081-DC15-4833-A41B-748F73A38179}"],
    methods: DEVICE_METHODS,
};
static DEVICE_A_CLASS: [&Interface; 1] = [&DEVICE_A];
static DEVICE_W_CLASS: [&Interface; 1] = [&DEVICE_W];

pub fn register(r: &mut Registry) {
    for i in [&DI8A, &DI8W, &DEVICE_A, &DEVICE_W] {
        com::register_interface(r, i);
    }
    r.add("dinput8.dll", "DirectInput8Create", |c| {
        let (riid, pp) = (c.arg(2), c.arg(3));
        let iid = com::format_iid(&read_guid(c, riid));
        let w = iid.eq_ignore_ascii_case(IID_DI8W);
        if !w && !iid.eq_ignore_ascii_case(IID_DI8A) {
            out_ptr(c, pp, 0);
            return Ok(Ret::stdcall(5, DIERR_NOINTERFACE));
        }
        let class: &'static [&'static Interface] = if w { &DI8W_CLASS } else { &DI8A_CLASS };
        let obj = com::create(c, "DirectInput8", class, HashMap::from_iter([("wide", w as u32)]));
        c.trace(format!(" = {obj:#x}"));
        out_ptr(c, pp, obj);
        Ok(Ret::stdcall(5, DI_OK))
    });
}
