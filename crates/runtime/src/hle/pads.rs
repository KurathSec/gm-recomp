//! Game controllers: XInput (GameMaker: Studio) and the winmm joystick API
//! (GameMaker 8), both over the pad states the host reports (the browser's
//! Gamepad API in its standard layout, which matches an Xbox controller).

use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;

const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
const JOYERR_PARMS: u32 = 165;
const JOYERR_UNPLUGGED: u32 = 167;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

fn pad(c: &mut Ctx, i: u32) -> Option<super::PadState> {
    super::user32::pump_input(c);
    c.p.pads.get(i as usize).copied().filter(|p| p.connected)
}

/// XINPUT_GAMEPAD button bits in the joystick API's button order.
const JOY_BUTTONS: [u16; 10] = [0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0x0020, 0x0010, 0x0040, 0x0080];

pub fn register(r: &mut Registry) {
    for dll in ["xinput1_3.dll", "xinput1_4.dll", "xinput9_1_0.dll", "xinput1_2.dll", "xinput1_1.dll"] {
        r.add(dll, "XInputGetState", |c| {
            let (i, out) = (c.arg(0), c.arg(1));
            let Some(p) = pad(c, i) else { return ok(2, ERROR_DEVICE_NOT_CONNECTED) };
            let _ = c.host.write_u32(out, p.packet);
            let _ = c.host.write_u16(out + 4, p.buttons);
            let _ = c.host.write(out + 6, &p.triggers);
            for (k, t) in p.thumbs.iter().enumerate() {
                let _ = c.host.write_u16(out + 8 + 2 * k as u32, *t as u16);
            }
            ok(2, 0)
        });
        r.add(dll, "XInputGetCapabilities", |c| {
            let (i, out) = (c.arg(0), c.arg(2));
            if pad(c, i).is_none() {
                return ok(3, ERROR_DEVICE_NOT_CONNECTED);
            }
            // XINPUT_CAPABILITIES: gamepad, every control present, vibration
            let _ = c.host.fill(out, 20, 0);
            let _ = c.host.write(out, &[1, 1]);
            let _ = c.host.write_u16(out + 4, 0xF3FF);
            let _ = c.host.write(out + 6, &[255, 255]);
            for k in 0..4 {
                let _ = c.host.write_u16(out + 8 + 2 * k, 0xFFC0);
            }
            let _ = c.host.write_u16(out + 16, 0xFFFF);
            let _ = c.host.write_u16(out + 18, 0xFFFF);
            ok(3, 0)
        });
        r.add(dll, "XInputSetState", |c| {
            let i = c.arg(0);
            ok(2, if pad(c, i).is_some() { 0 } else { ERROR_DEVICE_NOT_CONNECTED })
        });
        r.add(dll, "XInputEnable", |_c| ok(1, 0));
        r.add(dll, "XInputGetBatteryInformation", |c| {
            let (i, out) = (c.arg(0), c.arg(2));
            if pad(c, i).is_none() {
                return ok(3, ERROR_DEVICE_NOT_CONNECTED);
            }
            let _ = c.host.write(out, &[1, 3]); // wired, full
            ok(3, 0)
        });
        r.add(dll, "XInputGetKeystroke", |_c| ok(3, ERROR_DEVICE_NOT_CONNECTED));
    }
    let w = "winmm.dll";
    r.add(w, "joyGetNumDevs", |_c| ok(0, 16));
    for name in ["joyGetDevCapsA", "joyGetDevCapsW"] {
        r.add(w, name, |c| {
            let (i, out, size) = (c.arg(0), c.arg(1), c.arg(2));
            if pad(c, i).is_none() {
                return ok(3, if i < 16 { JOYERR_UNPLUGGED } else { JOYERR_PARMS });
            }
            let wide = c.name().ends_with('W');
            let _ = c.host.fill(out, size.min(728), 0);
            let _ = c.host.write_u16(out, 0x045E); // wMid: Microsoft
            let _ = c.host.write_u16(out + 2, 0x028E); // wPid: Xbox 360 controller
            let name = "Controller (XBOX 360 For Windows)";
            let after = if wide {
                let _ = c.host.write_wstr(out + 4, name);
                4 + 64
            } else {
                let _ = c.host.write_cstr(out + 4, name.as_bytes());
                4 + 32
            };
            // axis ranges (x, y, z) then periods, buttons, axes
            let vals = [0u32, 65535, 0, 65535, 0, 65535, 10, 10, 1000];
            for (k, v) in vals.iter().enumerate() {
                let _ = c.host.write_u32(out + after + 4 * k as u32, *v);
            }
            let rest = out + after + 36;
            // wRmin..wVmax, wCaps (Z, R, U, POV, POV4DIR), wMaxAxes, wNumAxes, wMaxButtons
            let more = [0u32, 65535, 0, 65535, 0, 65535, 0x1F, 6, 6, 32];
            for (k, v) in more.iter().enumerate() {
                let _ = c.host.write_u32(rest + 4 * k as u32, *v);
            }
            ok(3, 0)
        });
    }
    r.add(w, "joyGetPosEx", |c| joy_pos_impl(c, true));
    r.add(w, "joyGetPos", |c| joy_pos_impl(c, false));
}

fn joy_pos_impl(c: &mut Ctx, ex: bool) -> ApiResult {
    let (i, out) = (c.arg(0), c.arg(1));
    let Some(p) = pad(c, i) else { return ok(2, if i < 16 { JOYERR_UNPLUGGED } else { JOYERR_PARMS }) };
    let axis = |v: i16, invert: bool| -> u32 {
        let v = if invert { -(v as i32) - 1 } else { v as i32 };
        (v + 32768).clamp(0, 65535) as u32
    };
    let mut buttons = 0u32;
    for (k, bit) in JOY_BUTTONS.iter().enumerate() {
        if p.buttons & bit != 0 {
            buttons |= 1 << k;
        }
    }
    let (up, down, left, right) = (p.buttons & 1 != 0, p.buttons & 2 != 0, p.buttons & 4 != 0, p.buttons & 8 != 0);
    let pov = match (up, down, left, right) {
        (true, _, false, false) => 0,
        (true, _, false, true) => 4500,
        (false, false, false, true) => 9000,
        (false, true, false, true) => 13500,
        (false, true, false, false) => 18000,
        (false, true, true, false) => 22500,
        (false, false, true, false) => 27000,
        (true, _, true, false) => 31500,
        _ => 0xFFFF,
    };
    // triggers on the Z axis, as the Xbox controller's legacy driver reports them
    let z = (32768 + p.triggers[0] as i32 * 128 - p.triggers[1] as i32 * 128).clamp(0, 65535) as u32;
    if ex {
        // JOYINFOEX: size, flags, x, y, z, r, u, v, buttons, button number, pov
        let vals = [axis(p.thumbs[0], false), axis(p.thumbs[1], true), z, axis(p.thumbs[3], true), axis(p.thumbs[2], false), 0, buttons, buttons.count_ones(), pov];
        for (k, v) in vals.iter().enumerate() {
            let _ = c.host.write_u32(out + 8 + 4 * k as u32, *v);
        }
    } else {
        for (k, v) in [axis(p.thumbs[0], false), axis(p.thumbs[1], true), z, buttons].iter().enumerate() {
            let _ = c.host.write_u32(out + 4 * k as u32, *v);
        }
    }
    ok(2, 0)
}
