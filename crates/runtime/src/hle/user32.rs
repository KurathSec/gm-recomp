//! user32: window classes and windows, the message queue, properties, atoms,
//! timers, keyboard state and the small set of queries a VCL application makes.

use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{Handle, Msg, Window, WndClass};

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SHOWWINDOW: u32 = 0x0018;
pub const WM_ACTIVATEAPP: u32 = 0x001C;
pub const WM_SETCURSOR: u32 = 0x0020;
pub const WM_GETMINMAXINFO: u32 = 0x0024;
pub const WM_WINDOWPOSCHANGING: u32 = 0x0046;
pub const WM_WINDOWPOSCHANGED: u32 = 0x0047;
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_NCCALCSIZE: u32 = 0x0083;
pub const WM_NCPAINT: u32 = 0x0085;
pub const WM_NCACTIVATE: u32 = 0x0086;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_USER: u32 = 0x0400;

const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CHILD: u32 = 0x4000_0000;

pub fn register(r: &mut Registry) {
    let u = "user32.dll";
    r.add(u, "RegisterClassA", |c| {
        let p = c.arg(0);
        let atom = register_class(c, p, false, false);
        ok(1, atom)
    });
    r.add(u, "RegisterClassW", |c| {
        let p = c.arg(0);
        ok(1, register_class(c, p, false, true))
    });
    r.add(u, "RegisterClassExA", |c| {
        let p = c.arg(0);
        ok(1, register_class(c, p + 4, true, false))
    });
    r.add(u, "RegisterClassExW", |c| {
        let p = c.arg(0);
        ok(1, register_class(c, p + 4, true, true))
    });
    r.add(u, "UnregisterClassA", |c| {
        let name = class_name_arg(c, 0, false);
        c.p.classes.remove(&name);
        ok(2, 1)
    });
    r.add(u, "GetClassInfoA", |c| {
        let name = class_name_arg(c, 1, false);
        let out = c.arg(2);
        match c.p.classes.get(&name).cloned() {
            Some(cls) => {
                write_class(c, out, &cls, false);
                ok(3, 1)
            }
            None => ok(3, 0),
        }
    });
    r.add(u, "GetClassInfoExA", |c| {
        let name = class_name_arg(c, 1, false);
        let out = c.arg(2);
        match c.p.classes.get(&name).cloned() {
            Some(cls) => {
                let _ = c.host.write_u32(out, 48);
                write_class(c, out + 4, &cls, false);
                ok(3, 1)
            }
            None => ok(3, 0),
        }
    });
    r.add(u, "GetClassNameA", |c| {
        let hwnd = c.arg(0);
        let name = c.p.windows.get(&hwnd).map(|w| w.class.clone()).unwrap_or_default();
        let n = super::kernel32_copy_str(c, c.arg(1), c.arg(2), name.as_bytes());
        ok(3, n)
    });
    r.add(u, "GetClassLongA", |c| {
        let hwnd = c.arg(0);
        let idx = c.arg(1) as i32;
        let cls = c.p.windows.get(&hwnd).and_then(|w| c.p.classes.get(&w.class)).cloned().unwrap_or_default();
        ok(2, match idx { -26 => cls.style, -24 => cls.wndproc, -12 => cls.cursor, -14 => cls.icon, -10 => cls.background, -16 => cls.instance, _ => 0 })
    });
    r.add(u, "SetClassLongA", |_c| ok(3, 0));
    r.add(u, "CreateWindowExA", |c| {
        let exstyle = c.arg(0);
        let class = class_name_arg(c, 1, false);
        let title = c.str_arg(2);
        let style = c.arg(3);
        let (x, y, w, h) = (c.arg_i32(4), c.arg_i32(5), c.arg_i32(6), c.arg_i32(7));
        let (parent, menu, instance, param) = (c.arg(8), c.arg(9), c.arg(10), c.arg(11));
        let hwnd = create_window(c, exstyle, &class, &title, style, x, y, w, h, parent, menu, instance, param)?;
        ok(12, hwnd)
    });
    r.add(u, "CreateWindowExW", |c| {
        let exstyle = c.arg(0);
        let class = class_name_arg(c, 1, true);
        let title = c.wstr_arg(2);
        let style = c.arg(3);
        let (x, y, w, h) = (c.arg_i32(4), c.arg_i32(5), c.arg_i32(6), c.arg_i32(7));
        let (parent, menu, instance, param) = (c.arg(8), c.arg(9), c.arg(10), c.arg(11));
        let hwnd = create_window(c, exstyle, &class, &title, style, x, y, w, h, parent, menu, instance, param)?;
        ok(12, hwnd)
    });
    r.add(u, "DestroyWindow", |c| {
        let hwnd = c.arg(0);
        if c.p.windows.contains_key(&hwnd) {
            send_message(c, hwnd, WM_DESTROY, 0, 0)?;
            c.p.windows.remove(&hwnd);
        }
        ok(1, 1)
    });
    r.add(u, "DefWindowProcA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        ok(4, def_window_proc(c, hwnd, msg, wp, lp))
    });
    r.alias(u, "DefWindowProcW", u, "DefWindowProcA");
    r.add(u, "CallWindowProcA", |c| {
        let (proc_, hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3), c.arg(4));
        let r = if proc_ == 0 || crate::process::Process::stub_id(proc_).is_some() {
            def_window_proc(c, hwnd, msg, wp, lp)
        } else {
            c.call_guest(proc_, &[hwnd, msg, wp, lp])?
        };
        ok(5, r)
    });
    r.alias(u, "CallWindowProcW", u, "CallWindowProcA");
    r.add(u, "SendMessageA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let r = send_message(c, hwnd, msg, wp, lp)?;
        ok(4, r)
    });
    r.alias(u, "SendMessageW", u, "SendMessageA");
    r.add(u, "SendMessageTimeoutA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let r = send_message(c, hwnd, msg, wp, lp)?;
        let out = c.arg(6);
        if out != 0 {
            let _ = c.host.write_u32(out, r);
        }
        ok(7, 1)
    });
    r.add(u, "SendNotifyMessageA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        send_message(c, hwnd, msg, wp, lp)?;
        ok(4, 1)
    });
    r.add(u, "PostMessageA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        post_message(c, hwnd, msg, wp, lp);
        ok(4, 1)
    });
    r.alias(u, "PostMessageW", u, "PostMessageA");
    r.add(u, "PostThreadMessageA", |c| {
        let (msg, wp, lp) = (c.arg(1), c.arg(2), c.arg(3));
        post_message(c, 0, msg, wp, lp);
        ok(4, 1)
    });
    r.add(u, "PostQuitMessage", |c| {
        let code = c.arg(0);
        post_message(c, 0, WM_QUIT, code, 0);
        ok(1, 0)
    });
    r.add(u, "GetMessageA", |c| {
        let (out, hwnd_filter) = (c.arg(0), c.arg(1));
        let msg = get_message(c, hwnd_filter, true)?;
        write_msg(c, out, &msg);
        ok(4, (msg.msg != WM_QUIT) as u32)
    });
    r.alias(u, "GetMessageW", u, "GetMessageA");
    r.add(u, "PeekMessageA", |c| {
        let (out, hwnd_filter, remove) = (c.arg(0), c.arg(1), c.arg(4) & 1 != 0);
        pump_timers(c);
        let found = peek_message(c, hwnd_filter, remove);
        match found {
            Some(m) => {
                write_msg(c, out, &m);
                ok(5, 1)
            }
            None => ok(5, 0),
        }
    });
    r.alias(u, "PeekMessageW", u, "PeekMessageA");
    r.add(u, "TranslateMessage", |_c| ok(1, 0));
    r.add(u, "DispatchMessageA", |c| {
        let p = c.arg(0);
        let (hwnd, msg, wp, lp) = (c.host.read_u32(p).unwrap_or(0), c.host.read_u32(p + 4).unwrap_or(0), c.host.read_u32(p + 8).unwrap_or(0), c.host.read_u32(p + 12).unwrap_or(0));
        let r = if msg == WM_TIMER && lp != 0 {
            // timer with a callback procedure
            c.call_guest(lp, &[hwnd, msg, wp, c.p.now_ms()])?
        } else {
            send_message(c, hwnd, msg, wp, lp)?
        };
        ok(1, r)
    });
    r.alias(u, "DispatchMessageW", u, "DispatchMessageA");
    r.add(u, "GetQueueStatus", |c| ok(1, if c.p.messages.is_empty() { 0 } else { 0x0001_0001 }));
    r.add(u, "GetInputState", |_c| ok(0, 0));
    r.add(u, "MsgWaitForMultipleObjects", |c| {
        pump_timers(c);
        ok(5, if c.p.messages.is_empty() { c.advance_clock(1000); 0x102 } else { c.arg(0) })
    });
    r.add(u, "WaitMessage", |c| {
        c.advance_clock(1000);
        pump_timers(c);
        ok(0, 1)
    });
    r.add(u, "InSendMessage", |_c| ok(0, 0));
    r.add(u, "ReplyMessage", |_c| ok(1, 1));
    r.add(u, "GetMessageTime", |c| ok(0, c.p.now_ms()));
    r.add(u, "GetMessagePos", |_c| ok(0, 0));
    r.add(u, "RegisterWindowMessageA", |c| {
        let name = c.str_arg(0);
        let a = atom(c, &format!("msg:{name}"));
        ok(1, 0xC000 + (a as u32 & 0x3FFF))
    });
    r.alias(u, "RegisterWindowMessageW", u, "RegisterWindowMessageA");
    r.add(u, "ShowWindow", |c| {
        let (hwnd, cmd) = (c.arg(0), c.arg(1));
        let was = show_window(c, hwnd, cmd != 0)?;
        ok(2, was as u32)
    });
    r.add(u, "ShowWindowAsync", |c| {
        let (hwnd, cmd) = (c.arg(0), c.arg(1));
        let was = show_window(c, hwnd, cmd != 0)?;
        ok(2, was as u32)
    });
    r.add(u, "UpdateWindow", |c| {
        let hwnd = c.arg(0);
        if c.p.windows.get(&hwnd).map(|w| w.invalid).unwrap_or(false) {
            send_message(c, hwnd, WM_PAINT, 0, 0)?;
        }
        ok(1, 1)
    });
    r.add(u, "SetWindowPos", |c| {
        let (hwnd, x, y, w, h, flags) = (c.arg(0), c.arg_i32(2), c.arg_i32(3), c.arg_i32(4), c.arg_i32(5), c.arg(6));
        if let Some(win) = c.p.windows.get_mut(&hwnd) {
            if flags & 2 == 0 { win.x = x; win.y = y; }
            if flags & 1 == 0 { win.w = w; win.h = h; }
            if flags & 0x40 != 0 { win.visible = true; }
            if flags & 0x80 != 0 { win.visible = false; }
        }
        if flags & 1 == 0 {
            let wh = (w.max(0) as u32 & 0xFFFF) | ((h.max(0) as u32) << 16);
            send_message(c, hwnd, WM_SIZE, 0, wh)?;
        }
        if flags & 2 == 0 {
            send_message(c, hwnd, WM_MOVE, 0, (x as u32 & 0xFFFF) | ((y as u32) << 16))?;
        }
        ok(7, 1)
    });
    r.add(u, "MoveWindow", |c| {
        let (hwnd, x, y, w, h) = (c.arg(0), c.arg_i32(1), c.arg_i32(2), c.arg_i32(3), c.arg_i32(4));
        if let Some(win) = c.p.windows.get_mut(&hwnd) {
            win.x = x; win.y = y; win.w = w; win.h = h;
        }
        let wh = (w.max(0) as u32 & 0xFFFF) | ((h.max(0) as u32) << 16);
        send_message(c, hwnd, WM_SIZE, 0, wh)?;
        ok(6, 1)
    });
    r.add(u, "GetWindowRect", |c| {
        let (hwnd, out) = (c.arg(0), c.arg(1));
        let (x, y, w, h) = c.p.windows.get(&hwnd).map(|w| (w.x, w.y, w.w, w.h)).unwrap_or((0, 0, 800, 600));
        write_rect(c, out, x, y, x + w, y + h);
        ok(2, 1)
    });
    r.add(u, "GetClientRect", |c| {
        let (hwnd, out) = (c.arg(0), c.arg(1));
        let (w, h) = c.p.windows.get(&hwnd).map(|w| (w.w, w.h)).unwrap_or((800, 600));
        write_rect(c, out, 0, 0, w, h);
        ok(2, 1)
    });
    r.add(u, "AdjustWindowRectEx", |_c| ok(4, 1));
    r.add(u, "AdjustWindowRect", |_c| ok(3, 1));
    r.add(u, "ClientToScreen", |c| {
        let (hwnd, pt) = (c.arg(0), c.arg(1));
        let (x, y) = c.p.windows.get(&hwnd).map(|w| (w.x, w.y)).unwrap_or((0, 0));
        let px = c.host.read_u32(pt).unwrap_or(0) as i32;
        let py = c.host.read_u32(pt + 4).unwrap_or(0) as i32;
        let _ = c.host.write_u32(pt, (px + x) as u32);
        let _ = c.host.write_u32(pt + 4, (py + y) as u32);
        ok(2, 1)
    });
    r.add(u, "ScreenToClient", |c| {
        let (hwnd, pt) = (c.arg(0), c.arg(1));
        let (x, y) = c.p.windows.get(&hwnd).map(|w| (w.x, w.y)).unwrap_or((0, 0));
        let px = c.host.read_u32(pt).unwrap_or(0) as i32;
        let py = c.host.read_u32(pt + 4).unwrap_or(0) as i32;
        let _ = c.host.write_u32(pt, (px - x) as u32);
        let _ = c.host.write_u32(pt + 4, (py - y) as u32);
        ok(2, 1)
    });
    r.add(u, "MapWindowPoints", |_c| ok(4, 0));
    r.add(u, "InvalidateRect", |c| {
        let hwnd = c.arg(0);
        invalidate(c, hwnd);
        ok(3, 1)
    });
    r.add(u, "InvalidateRgn", |c| {
        let hwnd = c.arg(0);
        invalidate(c, hwnd);
        ok(3, 1)
    });
    r.add(u, "ValidateRect", |c| {
        let hwnd = c.arg(0);
        if let Some(w) = c.p.windows.get_mut(&hwnd) { w.invalid = false; }
        ok(2, 1)
    });
    r.add(u, "RedrawWindow", |c| {
        let hwnd = c.arg(0);
        invalidate(c, hwnd);
        ok(4, 1)
    });
    r.add(u, "GetUpdateRect", |c| {
        let hwnd = c.arg(0);
        let inv = c.p.windows.get(&hwnd).map(|w| w.invalid).unwrap_or(false);
        ok(3, inv as u32)
    });
    r.add(u, "BeginPaint", |c| {
        let (hwnd, ps) = (c.arg(0), c.arg(1));
        let (w, h) = c.p.windows.get(&hwnd).map(|w| (w.w, w.h)).unwrap_or((0, 0));
        let dc = super::gdi32::create_dc(c, hwnd);
        let _ = c.host.write_u32(ps, dc);
        let _ = c.host.write_u32(ps + 4, 1);
        write_rect(c, ps + 8, 0, 0, w, h);
        if let Some(win) = c.p.windows.get_mut(&hwnd) { win.invalid = false; }
        ok(2, dc)
    });
    r.add(u, "EndPaint", |c| {
        let ps = c.arg(1);
        let dc = c.host.read_u32(ps).unwrap_or(0);
        c.p.handles.remove(&dc);
        ok(2, 1)
    });
    r.add(u, "GetDC", |c| {
        let hwnd = c.arg(0);
        let dc = super::gdi32::create_dc(c, hwnd);
        ok(1, dc)
    });
    r.add(u, "GetWindowDC", |c| {
        let hwnd = c.arg(0);
        let dc = super::gdi32::create_dc(c, hwnd);
        ok(1, dc)
    });
    r.add(u, "GetDCEx", |c| {
        let hwnd = c.arg(0);
        let dc = super::gdi32::create_dc(c, hwnd);
        ok(3, dc)
    });
    r.add(u, "ReleaseDC", |c| {
        let dc = c.arg(1);
        c.p.handles.remove(&dc);
        ok(2, 1)
    });
    r.add(u, "GetWindowLongA", |c| {
        let (hwnd, idx) = (c.arg(0), c.arg_i32(1));
        ok(2, get_window_long(c, hwnd, idx))
    });
    r.alias(u, "GetWindowLongW", u, "GetWindowLongA");
    r.add(u, "SetWindowLongA", |c| {
        let (hwnd, idx, v) = (c.arg(0), c.arg_i32(1), c.arg(2));
        let old = get_window_long(c, hwnd, idx);
        if let Some(w) = c.p.windows.get_mut(&hwnd) {
            match idx {
                -4 => w.wndproc = v,
                -16 => w.style = v,
                -20 => w.exstyle = v,
                -21 => w.userdata = v,
                -12 => w.id = v,
                _ => { w.longs.insert(idx, v); }
            }
        }
        ok(3, old)
    });
    r.alias(u, "SetWindowLongW", u, "SetWindowLongA");
    r.add(u, "GetWindowThreadProcessId", |c| {
        let out = c.arg(1);
        if out != 0 {
            let _ = c.host.write_u32(out, 0x1234);
        }
        ok(2, c.p.main_thread)
    });
    r.add(u, "GetWindowTextA", |c| {
        let hwnd = c.arg(0);
        let t = c.p.windows.get(&hwnd).map(|w| w.title.clone()).unwrap_or_default();
        let n = super::kernel32_copy_str(c, c.arg(1), c.arg(2), t.as_bytes());
        ok(3, n)
    });
    r.add(u, "GetWindowTextLengthA", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.title.len() as u32).unwrap_or(0))
    });
    r.add(u, "SetWindowTextA", |c| {
        let (hwnd, t) = (c.arg(0), c.str_arg(1));
        if let Some(w) = c.p.windows.get_mut(&hwnd) { w.title = t; }
        ok(2, 1)
    });
    r.alias(u, "SetWindowTextW", u, "SetWindowTextA");
    r.add(u, "GetParent", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.parent).unwrap_or(0))
    });
    r.add(u, "SetParent", |c| {
        let (hwnd, parent) = (c.arg(0), c.arg(1));
        let old = c.p.windows.get(&hwnd).map(|w| w.parent).unwrap_or(0);
        if let Some(w) = c.p.windows.get_mut(&hwnd) { w.parent = parent; }
        ok(2, old)
    });
    r.add(u, "GetAncestor", |c| {
        let hwnd = c.arg(0);
        let mut h = hwnd;
        while let Some(p) = c.p.windows.get(&h).map(|w| w.parent) {
            if p == 0 { break; }
            h = p;
        }
        ok(2, h)
    });
    r.add(u, "GetWindow", |c| {
        let (hwnd, cmd) = (c.arg(0), c.arg(1));
        // GW_OWNER 4, GW_CHILD 5, siblings 0-3
        let r = match cmd {
            4 => 0,
            5 => c.p.windows.get(&hwnd).and_then(|w| w.children.first().copied()).unwrap_or(0),
            _ => 0,
        };
        ok(2, r)
    });
    r.add(u, "IsWindow", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.contains_key(&hwnd) as u32)
    });
    r.add(u, "IsWindowVisible", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.visible).unwrap_or(false) as u32)
    });
    r.add(u, "IsWindowEnabled", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.enabled).unwrap_or(false) as u32)
    });
    r.add(u, "IsIconic", |_c| ok(1, 0));
    r.add(u, "IsZoomed", |_c| ok(1, 0));
    r.add(u, "IsChild", |_c| ok(2, 0));
    r.add(u, "EnableWindow", |c| {
        let (hwnd, en) = (c.arg(0), c.arg(1) != 0);
        let was = c.p.windows.get(&hwnd).map(|w| w.enabled).unwrap_or(false);
        if let Some(w) = c.p.windows.get_mut(&hwnd) { w.enabled = en; }
        ok(2, (!was) as u32)
    });
    r.add(u, "GetFocus", |c| ok(0, c.p.focus));
    r.add(u, "SetFocus", |c| {
        let hwnd = c.arg(0);
        let old = c.p.focus;
        c.p.focus = hwnd;
        if hwnd != 0 {
            send_message(c, hwnd, WM_SETFOCUS, old, 0)?;
        }
        ok(1, old)
    });
    r.add(u, "GetActiveWindow", |c| ok(0, c.p.active));
    r.add(u, "SetActiveWindow", |c| {
        let old = c.p.active;
        c.p.active = c.arg(0);
        ok(1, old)
    });
    r.add(u, "GetForegroundWindow", |c| ok(0, c.p.active));
    r.add(u, "SetForegroundWindow", |c| {
        c.p.active = c.arg(0);
        ok(1, 1)
    });
    r.add(u, "BringWindowToTop", |_c| ok(1, 1));
    r.add(u, "GetCapture", |c| ok(0, c.p.capture));
    r.add(u, "SetCapture", |c| {
        let old = c.p.capture;
        c.p.capture = c.arg(0);
        ok(1, old)
    });
    r.add(u, "ReleaseCapture", |c| {
        c.p.capture = 0;
        ok(0, 1)
    });
    r.add(u, "GetDesktopWindow", |_c| ok(0, 0x0001_0000));
    r.add(u, "GetTopWindow", |_c| ok(1, 0));
    r.add(u, "FindWindowA", |_c| ok(2, 0));
    r.add(u, "FindWindowExA", |_c| ok(4, 0));
    r.add(u, "WindowFromPoint", |_c| ok(2, 0));
    r.add(u, "ChildWindowFromPoint", |_c| ok(3, 0));
    r.add(u, "EnumWindows", |_c| ok(2, 1));
    r.add(u, "EnumThreadWindows", |_c| ok(3, 1));
    r.add(u, "EnumChildWindows", |_c| ok(3, 1));
    r.add(u, "GetSystemMetrics", |c| {
        let i = c.arg(0);
        ok(1, system_metric(i))
    });
    r.add(u, "SystemParametersInfoA", |c| {
        let (action, out) = (c.arg(0), c.arg(2));
        match action {
            0x30 => { write_rect(c, out, 0, 0, 1920, 1040); } // SPI_GETWORKAREA
            0x29 => { if out != 0 { let _ = c.host.fill(out, 60, 0); let _ = c.host.write_u32(out, 60); } } // SPI_GETNONCLIENTMETRICS
            _ => { if out != 0 { let _ = c.host.write_u32(out, 0); } }
        }
        ok(4, 1)
    });
    r.alias(u, "SystemParametersInfoW", u, "SystemParametersInfoA");
    r.add(u, "GetSysColor", |c| {
        let i = c.arg(0);
        ok(1, match i { 5 => 0xFFFFFF, 15 => 0xF0F0F0, 8 => 0, 13 => 0xD77800, 18 => 0, _ => 0xC0C0C0 })
    });
    r.add(u, "GetSysColorBrush", |c| {
        let i = c.arg(0);
        let color = match i { 5 => 0xFFFFFF, 15 => 0xF0F0F0, _ => 0xC0C0C0 };
        let b = c.p.new_gdi(crate::process::GdiObject::Brush { color });
        ok(1, b)
    });
    r.add(u, "LoadIconA", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(2, h)
    });
    r.alias(u, "LoadIconW", u, "LoadIconA");
    r.add(u, "LoadCursorA", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Cursor);
        ok(2, h)
    });
    r.alias(u, "LoadCursorW", u, "LoadCursorA");
    r.add(u, "LoadImageA", |c| {
        let kind = c.arg(2);
        let h = c.p.new_gdi(match kind { 1 => crate::process::GdiObject::Icon, 2 => crate::process::GdiObject::Cursor, _ => crate::process::GdiObject::Bitmap { w: 32, h: 32, bpp: 32, bits: 0, pitch: 0 } });
        ok(6, h)
    });
    r.add(u, "LoadBitmapA", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Bitmap { w: 32, h: 32, bpp: 32, bits: 0, pitch: 0 });
        ok(2, h)
    });
    r.add(u, "LoadStringA", |c| {
        let n = super::kernel32_copy_str(c, c.arg(2), c.arg(3), b"");
        ok(4, n)
    });
    r.add(u, "DestroyIcon", |_c| ok(1, 1));
    r.add(u, "DestroyCursor", |_c| ok(1, 1));
    r.add(u, "CopyIcon", |c| ok(1, c.arg(0)));
    r.add(u, "CopyImage", |c| ok(5, c.arg(0)));
    r.add(u, "CreateIconIndirect", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(1, h)
    });
    r.add(u, "GetIconInfo", |_c| ok(2, 0));
    r.add(u, "DrawIconEx", |_c| ok(9, 1));
    r.add(u, "DrawIcon", |_c| ok(4, 1));
    r.add(u, "SetCursor", |c| ok(1, c.arg(0)));
    r.add(u, "GetCursor", |_c| ok(0, 0));
    r.add(u, "ShowCursor", |c| ok(1, if c.arg(0) != 0 { 1 } else { -1i32 as u32 }));
    r.add(u, "GetCursorPos", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p, 400);
        let _ = c.host.write_u32(p + 4, 300);
        ok(1, 1)
    });
    r.add(u, "SetCursorPos", |_c| ok(2, 1));
    r.add(u, "ClipCursor", |_c| ok(1, 1));
    r.add(u, "GetClipCursor", |c| {
        write_rect(c, c.arg(0), 0, 0, 1920, 1080);
        ok(1, 1)
    });
    r.add(u, "GetKeyState", |c| {
        let vk = c.arg(0) as usize & 0xFF;
        ok(1, (c.p.key_state[vk] as u32) << 8)
    });
    r.add(u, "GetAsyncKeyState", |c| {
        let vk = c.arg(0) as usize & 0xFF;
        ok(1, (c.p.key_state[vk] as u32) << 8)
    });
    r.add(u, "GetKeyboardState", |c| {
        let p = c.arg(0);
        let ks = c.p.key_state;
        let _ = c.host.write(p, &ks);
        ok(1, 1)
    });
    r.add(u, "SetKeyboardState", |_c| ok(1, 1));
    r.add(u, "GetKeyboardLayout", |_c| ok(1, 0x0409_0409));
    r.add(u, "GetKeyboardLayoutList", |_c| ok(2, 0));
    r.add(u, "GetKeyboardLayoutNameA", |c| {
        super::kernel32_copy_str(c, c.arg(0), 9, b"00000409");
        ok(1, 1)
    });
    r.add(u, "ActivateKeyboardLayout", |_c| ok(2, 0));
    r.add(u, "MapVirtualKeyA", |c| ok(2, c.arg(0)));
    r.alias(u, "MapVirtualKeyW", u, "MapVirtualKeyA");
    r.add(u, "MapVirtualKeyExA", |c| ok(3, c.arg(0)));
    r.add(u, "VkKeyScanA", |c| ok(1, c.arg(0) & 0xFF));
    r.add(u, "GetKeyNameTextA", |c| {
        let n = super::kernel32_copy_str(c, c.arg(1), c.arg(2), b"Key");
        ok(3, n)
    });
    r.add(u, "ToAsciiEx", |_c| ok(6, 0));
    r.add(u, "ToAscii", |_c| ok(5, 0));
    r.add(u, "ToUnicode", |_c| ok(6, 0));
    r.add(u, "OemToCharA", |c| {
        let s = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        let _ = c.host.write_cstr(c.arg(1), &s);
        ok(2, 1)
    });
    r.add(u, "CharToOemA", |c| {
        let s = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        let _ = c.host.write_cstr(c.arg(1), &s);
        ok(2, 1)
    });
    r.add(u, "CharNextA", |c| {
        let p = c.arg(0);
        ok(1, if c.host.read_u8(p).unwrap_or(0) == 0 { p } else { p + 1 })
    });
    r.add(u, "CharPrevA", |c| {
        let (start, p) = (c.arg(0), c.arg(1));
        ok(2, if p > start { p - 1 } else { start })
    });
    r.add(u, "CharUpperA", |c| {
        let p = c.arg(0);
        if p < 0x10000 {
            return ok(1, (p as u8).to_ascii_uppercase() as u32);
        }
        let s = c.host.read_cstr(p).unwrap_or_default().to_ascii_uppercase();
        let _ = c.host.write_cstr(p, &s);
        ok(1, p)
    });
    r.add(u, "CharLowerA", |c| {
        let p = c.arg(0);
        if p < 0x10000 {
            return ok(1, (p as u8).to_ascii_lowercase() as u32);
        }
        let s = c.host.read_cstr(p).unwrap_or_default().to_ascii_lowercase();
        let _ = c.host.write_cstr(p, &s);
        ok(1, p)
    });
    r.add(u, "CharUpperBuffA", |c| {
        let (p, n) = (c.arg(0), c.arg(1));
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(p, &mut b);
        b.make_ascii_uppercase();
        let _ = c.host.write(p, &b);
        ok(2, n)
    });
    r.add(u, "CharLowerBuffA", |c| {
        let (p, n) = (c.arg(0), c.arg(1));
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(p, &mut b);
        b.make_ascii_lowercase();
        let _ = c.host.write(p, &b);
        ok(2, n)
    });
    r.add(u, "IsCharAlphaA", |c| ok(1, (c.arg(0) as u8).is_ascii_alphabetic() as u32));
    r.add(u, "IsCharAlphaNumericA", |c| ok(1, (c.arg(0) as u8).is_ascii_alphanumeric() as u32));
    r.add(u, "IsCharUpperA", |c| ok(1, (c.arg(0) as u8).is_ascii_uppercase() as u32));
    r.add(u, "IsCharLowerA", |c| ok(1, (c.arg(0) as u8).is_ascii_lowercase() as u32));
    r.add(u, "wsprintfA", |c| {
        // cdecl: (buf, fmt, ...)
        let (buf, fmt) = (c.arg(0), c.str_arg(1));
        let s = format_wsprintf(c, &fmt, c.regs.esp + 12);
        let _ = c.host.write_cstr(buf, s.as_bytes());
        Ok(Ret::cdecl(s.len() as u32))
    });
    r.add(u, "MessageBoxA", |c| {
        let (text, caption) = (c.str_arg(1), c.str_arg(2));
        c.p.log(format!("MessageBox [{caption}]: {text}"));
        ok(4, 1)
    });
    r.alias(u, "MessageBoxW", u, "MessageBoxA");
    r.add(u, "MessageBoxExA", |c| {
        let (text, caption) = (c.str_arg(1), c.str_arg(2));
        c.p.log(format!("MessageBox [{caption}]: {text}"));
        ok(5, 1)
    });
    r.add(u, "MessageBeep", |_c| ok(1, 1));
    r.add(u, "SetTimer", |c| {
        let (hwnd, id, ms, proc_) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let due = c.p.clock_us + ms.max(1) as u64 * 1000;
        let id = if hwnd == 0 { id.max(1) + 0x100 } else { id };
        let hw = if hwnd == 0 { 0 } else { hwnd };
        let w = c.p.windows.entry(hw).or_insert_with(|| default_window());
        w.timers.insert(id, (ms.max(1), due));
        w.longs.insert(0x1000 + id as i32, proc_);
        ok(4, id)
    });
    r.add(u, "KillTimer", |c| {
        let (hwnd, id) = (c.arg(0), c.arg(1));
        let id = if hwnd == 0 { id.max(1) + 0x100 } else { id };
        if let Some(w) = c.p.windows.get_mut(&hwnd) {
            w.timers.remove(&id);
        }
        ok(2, 1)
    });
    r.add(u, "SetPropA", |c| {
        let (hwnd, key, v) = (c.arg(0), prop_key(c, 1), c.arg(2));
        if let Some(w) = c.p.windows.get_mut(&hwnd) {
            w.props.insert(key, v);
        }
        ok(3, 1)
    });
    r.add(u, "GetPropA", |c| {
        let (hwnd, key) = (c.arg(0), prop_key(c, 1));
        ok(2, c.p.windows.get(&hwnd).and_then(|w| w.props.get(&key)).copied().unwrap_or(0))
    });
    r.add(u, "RemovePropA", |c| {
        let (hwnd, key) = (c.arg(0), prop_key(c, 1));
        let v = c.p.windows.get_mut(&hwnd).and_then(|w| w.props.remove(&key)).unwrap_or(0);
        ok(2, v)
    });
    r.add(u, "GetWindowPlacement", |c| {
        let p = c.arg(1);
        let _ = c.host.fill(p, 44, 0);
        let _ = c.host.write_u32(p, 44);
        let _ = c.host.write_u32(p + 8, 1);
        ok(2, 1)
    });
    r.add(u, "SetWindowPlacement", |_c| ok(2, 1));
    r.add(u, "GetMenu", |_c| ok(1, 0));
    r.add(u, "SetMenu", |_c| ok(2, 1));
    r.add(u, "GetSystemMenu", |_c| ok(2, 0));
    r.add(u, "CreateMenu", |_c| ok(0, 0));
    r.add(u, "CreatePopupMenu", |_c| ok(0, 0));
    r.add(u, "DestroyMenu", |_c| ok(1, 1));
    r.add(u, "GetMenuItemCount", |_c| ok(1, 0));
    r.add(u, "AppendMenuA", |_c| ok(4, 1));
    r.add(u, "InsertMenuA", |_c| ok(5, 1));
    r.add(u, "DeleteMenu", |_c| ok(3, 1));
    r.add(u, "RemoveMenu", |_c| ok(3, 1));
    r.add(u, "EnableMenuItem", |_c| ok(3, 0));
    r.add(u, "CheckMenuItem", |_c| ok(3, 0));
    r.add(u, "GetMenuItemInfoA", |_c| ok(4, 0));
    r.add(u, "SetMenuItemInfoA", |_c| ok(4, 1));
    r.add(u, "GetMenuStringA", |_c| ok(5, 0));
    r.add(u, "GetMenuState", |_c| ok(3, 0xFFFF_FFFF));
    r.add(u, "DrawMenuBar", |_c| ok(1, 1));
    r.add(u, "TrackPopupMenu", |_c| ok(7, 0));
    r.add(u, "GetMenuItemID", |_c| ok(2, 0));
    r.add(u, "GetSubMenu", |_c| ok(2, 0));
    r.add(u, "SetMenuDefaultItem", |_c| ok(3, 1));
    r.add(u, "ModifyMenuA", |_c| ok(5, 1));
    r.add(u, "DrawTextA", |c| ok(5, 16));
    r.alias(u, "DrawTextW", u, "DrawTextA");
    r.add(u, "DrawTextExA", |_c| ok(6, 16));
    r.add(u, "DrawEdge", |_c| ok(4, 1));
    r.add(u, "DrawFrameControl", |_c| ok(4, 1));
    r.add(u, "DrawFocusRect", |_c| ok(2, 1));
    r.add(u, "FillRect", |_c| ok(3, 1));
    r.add(u, "FrameRect", |_c| ok(3, 1));
    r.add(u, "InvertRect", |_c| ok(2, 1));
    r.add(u, "DrawStateA", |_c| ok(10, 1));
    r.add(u, "SetRect", |c| {
        write_rect(c, c.arg(0), c.arg_i32(1), c.arg_i32(2), c.arg_i32(3), c.arg_i32(4));
        ok(5, 1)
    });
    r.add(u, "SetRectEmpty", |c| {
        write_rect(c, c.arg(0), 0, 0, 0, 0);
        ok(1, 1)
    });
    r.add(u, "CopyRect", |c| {
        let (d, s) = (c.arg(0), c.arg(1));
        let mut b = [0u8; 16];
        let _ = c.host.read(s, &mut b);
        let _ = c.host.write(d, &b);
        ok(2, 1)
    });
    r.add(u, "OffsetRect", |c| {
        let (p, dx, dy) = (c.arg(0), c.arg_i32(1), c.arg_i32(2));
        let r = read_rect(c, p);
        write_rect(c, p, r.0 + dx, r.1 + dy, r.2 + dx, r.3 + dy);
        ok(3, 1)
    });
    r.add(u, "InflateRect", |c| {
        let (p, dx, dy) = (c.arg(0), c.arg_i32(1), c.arg_i32(2));
        let r = read_rect(c, p);
        write_rect(c, p, r.0 - dx, r.1 - dy, r.2 + dx, r.3 + dy);
        ok(3, 1)
    });
    r.add(u, "IntersectRect", |c| {
        let (d, a, b) = (c.arg(0), read_rect(c, c.arg(1)), read_rect(c, c.arg(2)));
        let (l, t, r2, bo) = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
        if l < r2 && t < bo {
            write_rect(c, d, l, t, r2, bo);
            ok(3, 1)
        } else {
            write_rect(c, d, 0, 0, 0, 0);
            ok(3, 0)
        }
    });
    r.add(u, "UnionRect", |c| {
        let (d, a, b) = (c.arg(0), read_rect(c, c.arg(1)), read_rect(c, c.arg(2)));
        write_rect(c, d, a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3));
        ok(3, 1)
    });
    r.add(u, "SubtractRect", |c| {
        let (d, a) = (c.arg(0), read_rect(c, c.arg(1)));
        write_rect(c, d, a.0, a.1, a.2, a.3);
        ok(3, 1)
    });
    r.add(u, "IsRectEmpty", |c| {
        let r2 = read_rect(c, c.arg(0));
        ok(1, (r2.2 <= r2.0 || r2.3 <= r2.1) as u32)
    });
    r.add(u, "EqualRect", |c| {
        let (a, b) = (read_rect(c, c.arg(0)), read_rect(c, c.arg(1)));
        ok(2, (a == b) as u32)
    });
    r.add(u, "PtInRect", |c| {
        let r2 = read_rect(c, c.arg(0));
        let (x, y) = (c.arg_i32(1), c.arg_i32(2));
        ok(3, (x >= r2.0 && x < r2.2 && y >= r2.1 && y < r2.3) as u32)
    });
    r.add(u, "GetWindowRgn", |_c| ok(2, 0));
    r.add(u, "SetWindowRgn", |_c| ok(3, 1));
    r.add(u, "GetScrollInfo", |_c| ok(3, 0));
    r.add(u, "SetScrollInfo", |_c| ok(4, 0));
    r.add(u, "ShowScrollBar", |_c| ok(3, 1));
    r.add(u, "EnableScrollBar", |_c| ok(3, 1));
    r.add(u, "GetScrollPos", |_c| ok(2, 0));
    r.add(u, "SetScrollPos", |_c| ok(4, 0));
    r.add(u, "GetScrollRange", |_c| ok(4, 1));
    r.add(u, "SetScrollRange", |_c| ok(5, 1));
    r.add(u, "ScrollWindow", |_c| ok(5, 1));
    r.add(u, "ScrollWindowEx", |_c| ok(8, 0));
    r.add(u, "GetDoubleClickTime", |_c| ok(0, 500));
    r.add(u, "GetCaretBlinkTime", |_c| ok(0, 500));
    r.add(u, "CreateCaret", |_c| ok(4, 1));
    r.add(u, "DestroyCaret", |_c| ok(0, 1));
    r.add(u, "ShowCaret", |_c| ok(1, 1));
    r.add(u, "HideCaret", |_c| ok(1, 1));
    r.add(u, "SetCaretPos", |_c| ok(2, 1));
    r.add(u, "GetDlgItem", |_c| ok(2, 0));
    r.add(u, "GetDlgCtrlID", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.id).unwrap_or(0))
    });
    r.add(u, "IsDialogMessageA", |_c| ok(2, 0));
    r.add(u, "DialogBoxParamA", |_c| ok(5, 0xFFFF_FFFF));
    r.add(u, "CreateDialogParamA", |_c| ok(5, 0));
    r.add(u, "EndDialog", |_c| ok(2, 1));
    r.add(u, "SendDlgItemMessageA", |_c| ok(5, 0));
    r.add(u, "RegisterHotKey", |_c| ok(4, 1));
    r.add(u, "UnregisterHotKey", |_c| ok(2, 1));
    r.add(u, "SetWindowsHookExA", |c| {
        let h = c.p.new_handle(Handle::Dummy("hook"));
        ok(4, h)
    });
    r.add(u, "UnhookWindowsHookEx", |_c| ok(1, 1));
    r.add(u, "CallNextHookEx", |_c| ok(4, 0));
    r.add(u, "GetProcessWindowStation", |_c| ok(0, 0x44));
    r.add(u, "GetUserObjectInformationA", |_c| ok(5, 0));
    r.add(u, "GetUserObjectInformationW", |_c| ok(5, 0));
    r.add(u, "MonitorFromWindow", |_c| ok(2, 0x0001_0001));
    r.add(u, "MonitorFromPoint", |_c| ok(3, 0x0001_0001));
    r.add(u, "MonitorFromRect", |_c| ok(2, 0x0001_0001));
    r.add(u, "GetMonitorInfoA", |c| {
        let p = c.arg(1);
        let _ = c.host.write_u32(p, 40);
        write_rect(c, p + 4, 0, 0, 1920, 1080);
        write_rect(c, p + 20, 0, 0, 1920, 1040);
        let _ = c.host.write_u32(p + 36, 1);
        ok(2, 1)
    });
    r.alias(u, "GetMonitorInfoW", u, "GetMonitorInfoA");
    r.add(u, "EnumDisplayMonitors", |c| {
        let (cb, lp) = (c.arg(2), c.arg(3));
        if cb != 0 {
            let rect = c.p.heap_alloc(c.host, 0, 16, true);
            write_rect(c, rect, 0, 0, 1920, 1080);
            c.call_guest(cb, &[0x0001_0001, 0, rect, lp])?;
        }
        ok(4, 1)
    });
    r.add(u, "EnumDisplaySettingsA", |c| {
        let (mode, out) = (c.arg(1), c.arg(2));
        if mode != 0 && mode != 0xFFFF_FFFF {
            return ok(3, 0);
        }
        // DEVMODEA: dmSize at 68, dmBitsPerPel 104, dmPelsWidth 108, dmPelsHeight 112, dmDisplayFrequency 120
        let _ = c.host.fill(out, 156, 0);
        let _ = c.host.write_u16(out + 68, 156);
        let _ = c.host.write_u32(out + 40, 0x0060_0000 | 0x0004_0000); // dmFields
        let _ = c.host.write_u32(out + 104, 32);
        let _ = c.host.write_u32(out + 108, 1920);
        let _ = c.host.write_u32(out + 112, 1080);
        let _ = c.host.write_u32(out + 120, 60);
        ok(3, 1)
    });
    r.add(u, "EnumDisplayDevicesA", |_c| ok(4, 0));
    r.add(u, "ChangeDisplaySettingsA", |_c| ok(2, 0));
    r.add(u, "ChangeDisplaySettingsExA", |_c| ok(5, 0));
    r.add(u, "GetLastActivePopup", |c| ok(1, c.arg(0)));
    r.add(u, "GetLastInputInfo", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p + 4, c.p.now_ms());
        ok(1, 1)
    });
    r.add(u, "LockWindowUpdate", |_c| ok(1, 1));
    r.add(u, "SetLayeredWindowAttributes", |_c| ok(4, 1));
    r.add(u, "FlashWindow", |_c| ok(2, 0));
    r.add(u, "FlashWindowEx", |_c| ok(1, 0));
    r.add(u, "ExitWindowsEx", |_c| ok(2, 0));
    r.add(u, "OpenClipboard", |_c| ok(1, 0));
    r.add(u, "CloseClipboard", |_c| ok(0, 1));
    r.add(u, "EmptyClipboard", |_c| ok(0, 1));
    r.add(u, "GetClipboardData", |_c| ok(1, 0));
    r.add(u, "SetClipboardData", |_c| ok(2, 0));
    r.add(u, "IsClipboardFormatAvailable", |_c| ok(1, 0));
    r.add(u, "RegisterClipboardFormatA", |_c| ok(1, 0xC100));
    r.add(u, "GetClipboardFormatNameA", |_c| ok(3, 0));
    r.add(u, "CountClipboardFormats", |_c| ok(0, 0));
    r.add(u, "EnumClipboardFormats", |_c| ok(1, 0));
    r.add(u, "SetWindowContextHelpId", |_c| ok(2, 1));
    r.add(u, "GetWindowContextHelpId", |_c| ok(1, 0));
    r.add(u, "WinHelpA", |_c| ok(4, 0));
    r.add(u, "GetGUIThreadInfo", |_c| ok(2, 0));
    r.add(u, "AttachThreadInput", |_c| ok(3, 1));
    r.add(u, "keybd_event", |_c| ok(4, 0));
    r.add(u, "mouse_event", |_c| ok(5, 0));
    r.add(u, "SendInput", |c| ok(3, c.arg(0)));
    r.add(u, "GetRawInputData", |_c| ok(5, 0));
    r.add(u, "RegisterRawInputDevices", |_c| ok(3, 1));
    r.add(u, "SetProcessDPIAware", |_c| ok(0, 1));
    r.add(u, "IsWindowUnicode", |_c| ok(1, 0));
    r.add(u, "GetWindowInfo", |_c| ok(2, 0));
    r.add(u, "GetTitleBarInfo", |_c| ok(2, 0));
    r.add(u, "GetComboBoxInfo", |_c| ok(2, 0));
    r.add(u, "GetListBoxInfo", |_c| ok(1, 0));
    r.add(u, "DefFrameProcA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(2), c.arg(3), c.arg(4));
        ok(5, def_window_proc(c, hwnd, msg, wp, lp))
    });
    r.add(u, "DefMDIChildProcA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        ok(4, def_window_proc(c, hwnd, msg, wp, lp))
    });
    r.add(u, "DefDlgProcA", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        ok(4, def_window_proc(c, hwnd, msg, wp, lp))
    });
    r.add(u, "GetWindowDpiAwarenessContext", |_c| ok(1, 0));
    r.add(u, "GetDpiForWindow", |_c| ok(1, 96));
    r.add(u, "GetDpiForSystem", |_c| ok(0, 96));
    r.add(u, "GetKeyboardType", |c| ok(1, match c.arg(0) { 0 => 4, 1 => 0, _ => 12 }));
    r.add(u, "GetMessageExtraInfo", |_c| ok(0, 0));
    r.add(u, "TranslateAcceleratorA", |_c| ok(3, 0));
    r.add(u, "LoadAcceleratorsA", |_c| ok(2, 0));
    r.add(u, "LoadMenuA", |_c| ok(2, 0));
    r.add(u, "CheckMenuRadioItem", |_c| ok(5, 1));
    r.add(u, "GetMenuItemRect", |_c| ok(4, 0));
    r.add(u, "GetMenuDefaultItem", |_c| ok(3, 0xFFFF_FFFF));
    r.add(u, "MenuItemFromPoint", |_c| ok(4, 0xFFFF_FFFF));
    r.add(u, "HiliteMenuItem", |_c| ok(4, 0));
    r.add(u, "GetMenuInfo", |_c| ok(2, 0));
    r.add(u, "SetMenuInfo", |_c| ok(2, 1));
    r.add(u, "GetMenuBarInfo", |_c| ok(4, 0));
    r.add(u, "TrackPopupMenuEx", |_c| ok(6, 0));
    r.add(u, "InsertMenuItemA", |_c| ok(4, 1));
    r.add(u, "EndMenu", |_c| ok(0, 1));
    r.add(u, "GetMenuContextHelpId", |_c| ok(1, 0));
    r.add(u, "CreateAcceleratorTableA", |_c| ok(2, 0));
    r.add(u, "DestroyAcceleratorTable", |_c| ok(1, 1));
    r.add(u, "GetWindowModuleFileNameA", |c| {
        let n = super::kernel32_copy_str(c, c.arg(1), c.arg(2), b"");
        ok(3, n)
    });
    r.add(u, "DdeInitializeA", |_c| ok(4, 1));
    r.add(u, "CreateMDIWindowA", |_c| ok(10, 0));
    r.add(u, "ArrangeIconicWindows", |_c| ok(1, 0));
    r.add(u, "CascadeWindows", |_c| ok(5, 0));
    r.add(u, "TileWindows", |_c| ok(5, 0));
    r.add(u, "GetNextDlgTabItem", |_c| ok(3, 0));
    r.add(u, "GetNextDlgGroupItem", |_c| ok(3, 0));
    r.add(u, "MapDialogRect", |_c| ok(2, 1));
    r.add(u, "ChildWindowFromPointEx", |_c| ok(4, 0));
    r.add(u, "RealChildWindowFromPoint", |_c| ok(3, 0));
    r.add(u, "GetShellWindow", |_c| ok(0, 0));
    r.add(u, "GetWindowDC", |c| {
        let hwnd = c.arg(0);
        let dc = super::gdi32::create_dc(c, hwnd);
        ok(1, dc)
    });
    r.add(u, "PaintDesktop", |_c| ok(1, 1));
    r.add(u, "GetUpdateRgn", |_c| ok(3, 1));
    r.add(u, "ExcludeUpdateRgn", |_c| ok(2, 1));
    r.add(u, "DeferWindowPos", |c| ok(8, c.arg(0)));
    r.add(u, "BeginDeferWindowPos", |_c| ok(1, 1));
    r.add(u, "EndDeferWindowPos", |_c| ok(1, 1));
    r.add(u, "AnimateWindow", |_c| ok(3, 1));
    r.add(u, "SetWindowRgn", |_c| ok(3, 1));
    r.add(u, "PrintWindow", |_c| ok(3, 1));
    r.add(u, "UpdateLayeredWindow", |_c| ok(9, 1));
    r.add(u, "SetProcessDefaultLayout", |_c| ok(1, 1));
    r.add(u, "GetProcessDefaultLayout", |_c| ok(1, 1));
    r.add(u, "IsHungAppWindow", |_c| ok(1, 0));
    r.add(u, "SwitchToThisWindow", |_c| ok(2, 0));
    r.add(u, "AllowSetForegroundWindow", |_c| ok(1, 1));
    r.add(u, "LockSetForegroundWindow", |_c| ok(1, 1));
    r.add(u, "GetLayeredWindowAttributes", |_c| ok(4, 0));
    r.add(u, "GetPhysicalCursorPos", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p, 400);
        let _ = c.host.write_u32(p + 4, 300);
        ok(1, 1)
    });
    r.add(u, "ChangeWindowMessageFilter", |_c| ok(2, 1));
    r.add(u, "NotifyWinEvent", |_c| ok(4, 0));
    r.add(u, "SetWinEventHook", |_c| ok(7, 0));
    r.add(u, "UnhookWinEvent", |_c| ok(1, 1));
    r.add(u, "GetOpenClipboardWindow", |_c| ok(0, 0));
    r.add(u, "SetMessageExtraInfo", |_c| ok(1, 0));
    r.add(u, "BroadcastSystemMessageA", |_c| ok(5, 1));
    r.add(u, "GetWindowThreadProcessId", |c| {
        let out = c.arg(1);
        if out != 0 {
            let _ = c.host.write_u32(out, 0x1234);
        }
        ok(2, c.p.main_thread)
    });
    r.add(u, "GetThreadDesktop", |_c| ok(1, 0x48));
    r.add(u, "OpenInputDesktop", |_c| ok(3, 0x48));
    r.add(u, "CloseDesktop", |_c| ok(1, 1));
    r.add(u, "GetSystemMetricsForDpi", |c| {
        let i = c.arg(0);
        ok(2, system_metric(i))
    });
    r.add(u, "SetWindowsHookA", |_c| ok(2, 0));
    r.add(u, "UnhookWindowsHook", |_c| ok(2, 1));
    r.add(u, "GetWindowWord", |_c| ok(2, 0));
    r.add(u, "SetWindowWord", |_c| ok(3, 0));
    r.add(u, "GetClassWord", |_c| ok(2, 0));
    r.add(u, "SetClassWord", |_c| ok(3, 0));
    r.add(u, "GetMessageA_", |_c| ok(4, 0));
}

fn default_window() -> Window {
    Window {
        class: String::new(),
        wndproc: 0,
        parent: 0,
        style: 0,
        exstyle: 0,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        visible: false,
        title: String::new(),
        userdata: 0,
        id: 0,
        props: Default::default(),
        longs: Default::default(),
        timers: Default::default(),
        children: Vec::new(),
        invalid: false,
        enabled: true,
        sel: (0, 0),
    }
}

fn class_name_arg(c: &Ctx, i: u32, wide: bool) -> String {
    let a = c.arg(i);
    if a < 0x10000 {
        return format!("#{a}");
    }
    if wide { c.host.read_wstr(a).unwrap_or_default() } else { c.host.read_cstr_lossy(a) }
}

fn prop_key(c: &Ctx, i: u32) -> String {
    let a = c.arg(i);
    if a < 0x10000 { format!("#{a}") } else { c.host.read_cstr_lossy(a) }
}

fn atom(c: &mut Ctx, name: &str) -> u16 {
    if let Some(&a) = c.p.atoms.get(name) {
        return a;
    }
    let a = c.p.next_atom;
    c.p.next_atom = c.p.next_atom.wrapping_add(1).max(0xC000);
    c.p.atoms.insert(name.to_string(), a);
    a
}

fn register_class(c: &mut Ctx, p: u32, ex: bool, wide: bool) -> u32 {
    // WNDCLASS(A/W): style, wndproc, clsExtra, wndExtra, hInstance, hIcon, hCursor, hbrBackground, menuName, className
    let r = |c: &Ctx, off: u32| c.host.read_u32(p + off).unwrap_or(0);
    let name_ptr = r(c, 36);
    let name = if name_ptr < 0x10000 { format!("#{name_ptr}") } else if wide { c.host.read_wstr(name_ptr).unwrap_or_default() } else { c.host.read_cstr_lossy(name_ptr) };
    let cls = WndClass {
        style: r(c, 0),
        wndproc: r(c, 4),
        cls_extra: r(c, 8) as i32,
        wnd_extra: r(c, 12) as i32,
        instance: r(c, 16),
        icon: r(c, 20),
        cursor: r(c, 24),
        background: r(c, 28),
        menu_name: String::new(),
        atom: 0,
    };
    let a = atom(c, &format!("class:{name}"));
    let mut cls = cls;
    cls.atom = a;
    let _ = ex;
    c.trace(format!("({name:?}, wndproc={:#x}) = {a:#x}", cls.wndproc));
    c.p.classes.insert(name, cls);
    a as u32
}

fn write_class(c: &mut Ctx, out: u32, cls: &WndClass, _wide: bool) {
    let w = |c: &mut Ctx, off: u32, v: u32| { let _ = c.host.write_u32(out + off, v); };
    w(c, 0, cls.style);
    w(c, 4, cls.wndproc);
    w(c, 8, cls.cls_extra as u32);
    w(c, 12, cls.wnd_extra as u32);
    w(c, 16, cls.instance);
    w(c, 20, cls.icon);
    w(c, 24, cls.cursor);
    w(c, 28, cls.background);
    w(c, 32, 0);
    w(c, 36, 0);
}

#[allow(clippy::too_many_arguments)]
fn create_window(c: &mut Ctx, exstyle: u32, class: &str, title: &str, style: u32, x: i32, y: i32, w: i32, h: i32, parent: u32, menu: u32, instance: u32, param: u32) -> Result<u32, super::ApiError> {
    let cls = c.p.classes.get(class).cloned();
    let wndproc = cls.as_ref().map(|k| k.wndproc).unwrap_or(0);
    let hwnd = c.p.next_hwnd;
    c.p.next_hwnd += 0x10;
    let (x, y) = (if x == i32::MIN { 100 } else { x }, if y == i32::MIN { 100 } else { y });
    let (w, h) = (if w == i32::MIN { 800 } else { w }, if h == i32::MIN { 600 } else { h });
    let win = Window {
        class: class.to_string(),
        wndproc,
        parent,
        style,
        exstyle,
        x,
        y,
        w,
        h,
        visible: false,
        title: title.to_string(),
        userdata: 0,
        id: if style & WS_CHILD != 0 { menu } else { 0 },
        props: Default::default(),
        longs: Default::default(),
        timers: Default::default(),
        children: Vec::new(),
        invalid: false,
        enabled: true,
        sel: (0, 0),
    };
    c.p.windows.insert(hwnd, win);
    if let Some(p) = c.p.windows.get_mut(&parent) {
        p.children.push(hwnd);
    }
    c.trace(format!("({class:?}, {title:?}, style={style:#x}, {x},{y} {w}x{h}, parent={parent:#x}) = {hwnd:#x}"));
    if wndproc != 0 {
        // CREATESTRUCT in guest memory
        let cs = c.p.heap_alloc(c.host, 0, 48, true);
        let strs = [param, instance, menu, parent, h as u32, w as u32, y as u32, x as u32, style, 0, 0, exstyle];
        for (i, v) in strs.iter().enumerate() {
            let _ = c.host.write_u32(cs + 4 * i as u32, *v);
        }
        let name_a = c.p.heap_alloc(c.host, 0, title.len() as u32 + 1, false);
        let _ = c.host.write_cstr(name_a, title.as_bytes());
        let _ = c.host.write_u32(cs + 36, name_a);
        let class_a = c.p.heap_alloc(c.host, 0, class.len() as u32 + 1, false);
        let _ = c.host.write_cstr(class_a, class.as_bytes());
        let _ = c.host.write_u32(cs + 40, class_a);
        if send_message(c, hwnd, WM_NCCREATE, 0, cs)? == 0 {
            c.p.windows.remove(&hwnd);
            return Ok(0);
        }
        let rect = c.p.heap_alloc(c.host, 0, 16, true);
        write_rect(c, rect, x, y, x + w, y + h);
        send_message(c, hwnd, WM_NCCALCSIZE, 0, rect)?;
        if send_message(c, hwnd, WM_CREATE, 0, cs)? == 0xFFFF_FFFF {
            c.p.windows.remove(&hwnd);
            return Ok(0);
        }
        send_message(c, hwnd, WM_SIZE, 0, (w as u32 & 0xFFFF) | ((h as u32) << 16))?;
        send_message(c, hwnd, WM_MOVE, 0, (x as u32 & 0xFFFF) | ((y as u32) << 16))?;
    }
    if style & WS_VISIBLE != 0 {
        show_window(c, hwnd, true)?;
    }
    Ok(hwnd)
}

fn show_window(c: &mut Ctx, hwnd: u32, show: bool) -> Result<bool, super::ApiError> {
    let was = match c.p.windows.get_mut(&hwnd) {
        Some(w) => {
            let was = w.visible;
            w.visible = show;
            was
        }
        None => return Ok(false),
    };
    if was != show {
        send_message(c, hwnd, WM_SHOWWINDOW, show as u32, 0)?;
        if show {
            let (w, h) = c.p.windows.get(&hwnd).map(|w| (w.w, w.h)).unwrap_or((0, 0));
            c.p.active = hwnd;
            c.p.focus = hwnd;
            send_message(c, hwnd, WM_ACTIVATEAPP, 1, 0)?;
            send_message(c, hwnd, WM_NCACTIVATE, 1, 0)?;
            send_message(c, hwnd, WM_ACTIVATE, 1, 0)?;
            send_message(c, hwnd, WM_SETFOCUS, 0, 0)?;
            send_message(c, hwnd, WM_SIZE, 0, (w as u32 & 0xFFFF) | ((h as u32) << 16))?;
            invalidate(c, hwnd);
        }
    }
    Ok(was)
}

fn invalidate(c: &mut Ctx, hwnd: u32) {
    if let Some(w) = c.p.windows.get_mut(&hwnd) {
        if !w.invalid {
            w.invalid = true;
            c.p.messages.push_back(Msg { hwnd, msg: WM_PAINT, wparam: 0, lparam: 0, time: c.p.now_ms(), pt: (0, 0) });
        }
    }
}

pub fn send_message(c: &mut Ctx, hwnd: u32, msg: u32, wp: u32, lp: u32) -> Result<u32, super::ApiError> {
    let proc_ = match c.p.windows.get(&hwnd) {
        Some(w) => w.wndproc,
        None => return Ok(0),
    };
    if c.p.trace {
        c.p.log(format!("    -> WndProc({hwnd:#x}, {msg:#x}, {wp:#x}, {lp:#x})"));
    }
    if proc_ == 0 || crate::process::Process::stub_id(proc_).is_some() {
        return Ok(def_window_proc(c, hwnd, msg, wp, lp));
    }
    c.call_guest(proc_, &[hwnd, msg, wp, lp])
}

fn post_message(c: &mut Ctx, hwnd: u32, msg: u32, wp: u32, lp: u32) {
    let t = c.p.now_ms();
    c.p.messages.push_back(Msg { hwnd, msg, wparam: wp, lparam: lp, time: t, pt: (0, 0) });
}

/// Queues WM_TIMER messages whose due time has passed.
fn pump_timers(c: &mut Ctx) {
    let now = c.p.clock_us;
    let mut due = Vec::new();
    for (&hwnd, w) in c.p.windows.iter_mut() {
        for (&id, (ms, when)) in w.timers.iter_mut() {
            if *when <= now {
                *when = now + *ms as u64 * 1000;
                let proc_ = w.longs.get(&(0x1000 + id as i32)).copied().unwrap_or(0);
                due.push((hwnd, id, proc_));
            }
        }
    }
    for (hwnd, id, proc_) in due {
        if !c.p.messages.iter().any(|m| m.hwnd == hwnd && m.msg == WM_TIMER && m.wparam == id) {
            post_message(c, hwnd, WM_TIMER, id, proc_);
        }
    }
}

fn peek_message(c: &mut Ctx, hwnd_filter: u32, remove: bool) -> Option<Msg> {
    let idx = c.p.messages.iter().position(|m| hwnd_filter == 0 || m.hwnd == hwnd_filter)?;
    if remove {
        c.p.messages.remove(idx)
    } else {
        c.p.messages.get(idx).copied()
    }
}

fn get_message(c: &mut Ctx, hwnd_filter: u32, _remove: bool) -> Result<Msg, super::ApiError> {
    loop {
        pump_timers(c);
        if let Some(m) = peek_message(c, hwnd_filter, true) {
            return Ok(m);
        }
        // Nothing queued: advance the virtual clock to the next timer, or idle in 1 ms steps.
        let next = c.p.windows.values().flat_map(|w| w.timers.values().map(|(_, when)| *when)).min();
        match next {
            Some(when) if when > c.p.clock_us => c.p.clock_us = when,
            _ => c.advance_clock(1000),
        }
        if c.p.frames_presented >= c.p.frame_limit {
            return Ok(Msg { hwnd: 0, msg: WM_QUIT, wparam: 0, lparam: 0, time: c.p.now_ms(), pt: (0, 0) });
        }
        if c.p.clock_us > 600_000_000 {
            // 10 virtual minutes without progress: stop
            return Err(super::ApiError("message loop idle for 10 virtual minutes".into()));
        }
    }
}

fn write_msg(c: &mut Ctx, out: u32, m: &Msg) {
    let _ = c.host.write_u32(out, m.hwnd);
    let _ = c.host.write_u32(out + 4, m.msg);
    let _ = c.host.write_u32(out + 8, m.wparam);
    let _ = c.host.write_u32(out + 12, m.lparam);
    let _ = c.host.write_u32(out + 16, m.time);
    let _ = c.host.write_u32(out + 20, m.pt.0 as u32);
    let _ = c.host.write_u32(out + 24, m.pt.1 as u32);
}

pub fn write_rect(c: &mut Ctx, p: u32, l: i32, t: i32, r: i32, b: i32) {
    if p == 0 {
        return;
    }
    let _ = c.host.write_u32(p, l as u32);
    let _ = c.host.write_u32(p + 4, t as u32);
    let _ = c.host.write_u32(p + 8, r as u32);
    let _ = c.host.write_u32(p + 12, b as u32);
}

pub fn read_rect(c: &Ctx, p: u32) -> (i32, i32, i32, i32) {
    let r = |o: u32| c.host.read_u32(p + o).unwrap_or(0) as i32;
    (r(0), r(4), r(8), r(12))
}

fn get_window_long(c: &Ctx, hwnd: u32, idx: i32) -> u32 {
    let Some(w) = c.p.windows.get(&hwnd) else { return 0 };
    match idx {
        -4 => w.wndproc,
        -16 => w.style,
        -20 => w.exstyle,
        -21 => w.userdata,
        -12 => w.id,
        -8 => w.parent,
        -6 => c.p.classes.get(&w.class).map(|k| k.instance).unwrap_or(0),
        _ => w.longs.get(&idx).copied().unwrap_or(0),
    }
}

/// Start offsets of the lines of an edit control's text (CR LF, CR or LF break lines).
fn edit_lines(t: &[char]) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut i = 0;
    while i < t.len() {
        if t[i] == '\r' || t[i] == '\n' {
            if t[i] == '\r' && t.get(i + 1) == Some(&'\n') {
                i += 1;
            }
            starts.push(i as u32 + 1);
        }
        i += 1;
    }
    starts
}

/// The edit-control messages VCL's TMemo/TRichEdit use (text model only).
fn edit_message(c: &mut Ctx, hwnd: u32, msg: u32, wp: u32, lp: u32) -> u32 {
    let wide = c.name().ends_with('W');
    let Some(w) = c.p.windows.get(&hwnd) else { return 0 };
    let text: Vec<char> = w.title.chars().collect();
    let (s0, s1) = w.sel;
    let len = text.len() as u32;
    let lines = edit_lines(&text);
    let line_of = |ch: u32| lines.iter().rposition(|&st| st <= ch).unwrap_or(0) as u32;
    let line_len = |line: usize| -> u32 {
        let st = lines[line];
        let mut e = if line + 1 < lines.len() { lines[line + 1] } else { len };
        while e > st && matches!(text.get(e as usize - 1), Some('\r' | '\n')) {
            e -= 1;
        }
        e - st
    };
    match msg {
        0x00B0 => {
            // EM_GETSEL
            if wp != 0 {
                let _ = c.host.write_u32(wp, s0);
            }
            if lp != 0 {
                let _ = c.host.write_u32(lp, s1);
            }
            (s0.min(0xFFFF)) | (s1.min(0xFFFF) << 16)
        }
        0x00B1 => {
            // EM_SETSEL
            let (a, b) = (wp as i32, lp as i32);
            let sel = if a == -1 { (s1, s1) } else { let b = if b < 0 { len } else { (b as u32).min(len) }; ((a as u32).min(len), b) };
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.sel = (sel.0.min(sel.1), sel.0.max(sel.1));
            }
            0
        }
        0x0434 => {
            // EM_EXGETSEL
            let _ = c.host.write_u32(lp, s0);
            let _ = c.host.write_u32(lp + 4, s1);
            0
        }
        0x0437 => {
            // EM_EXSETSEL
            let a = c.host.read_u32(lp).unwrap_or(0) as i32;
            let b = c.host.read_u32(lp + 4).unwrap_or(0) as i32;
            let sel = if a == 0 && b == -1 { (0, len) } else if a < 0 { (s1, s1) } else { ((a as u32).min(len), if b < 0 { len } else { (b as u32).min(len) }) };
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.sel = (sel.0.min(sel.1), sel.0.max(sel.1));
            }
            sel.1
        }
        0x00C2 => {
            // EM_REPLACESEL: the inserted text also goes to the log.
            let t = if lp == 0 { String::new() } else if wide { c.host.read_wstr(lp).unwrap_or_default() } else { c.host.read_cstr_lossy(lp) };
            let class = c.p.windows.get(&hwnd).map(|w| w.class.clone()).unwrap_or_default();
            if !t.trim().is_empty() {
                c.p.log(format!("[{class} {hwnd:#x}] {}", t.trim_end()));
            }
            let ins: Vec<char> = t.chars().collect();
            let (a, b) = (s0.min(len) as usize, s1.min(len) as usize);
            let mut nt: Vec<char> = text[..a].to_vec();
            nt.extend(&ins);
            nt.extend(&text[b..]);
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.title = nt.into_iter().collect();
                let caret = (a + ins.len()) as u32;
                w.sel = (caret, caret);
            }
            0
        }
        0x00BA => lines.len() as u32, // EM_GETLINECOUNT
        0x00BB => {
            // EM_LINEINDEX
            let line = if wp as i32 == -1 { line_of(s0) } else { wp };
            lines.get(line as usize).copied().unwrap_or(0xFFFF_FFFF)
        }
        0x00C1 => {
            // EM_LINELENGTH
            if wp as i32 == -1 { 0 } else { line_len(line_of(wp) as usize) }
        }
        0x00C9 => {
            // EM_LINEFROMCHAR
            line_of(if wp as i32 == -1 { s0 } else { wp })
        }
        0x00C4 => {
            // EM_GETLINE: buffer starts with its capacity
            let line = wp as usize;
            if line >= lines.len() {
                return 0;
            }
            let st = lines[line] as usize;
            let n = line_len(line) as usize;
            let cap = c.host.read_u16(lp).unwrap_or(0) as usize;
            let s: String = text[st..st + n.min(cap)].iter().collect();
            if wide {
                for (k, u) in s.encode_utf16().enumerate() {
                    let _ = c.host.write_u16(lp + 2 * k as u32, u);
                }
            } else {
                let _ = c.host.write(lp, s.as_bytes());
            }
            n.min(cap) as u32
        }
        0x00B8 => 0, // EM_GETMODIFY
        0x00CE => 0, // EM_GETFIRSTVISIBLELINE
        _ => 0,
    }
}

fn def_window_proc(c: &mut Ctx, hwnd: u32, msg: u32, wp: u32, lp_: u32) -> u32 {
    match msg {
        WM_NCCREATE => 1,
        WM_CLOSE => {
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.visible = false;
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_SETCURSOR => 0,
        WM_NCACTIVATE => 1,
        WM_PAINT => {
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.invalid = false;
            }
            0
        }
        0x000C => {
            // WM_SETTEXT
            let wide = c.name().ends_with('W');
            let t = if lp_ == 0 { String::new() } else if wide { c.host.read_wstr(lp_).unwrap_or_default() } else { c.host.read_cstr_lossy(lp_) };
            if let Some(w) = c.p.windows.get_mut(&hwnd) {
                w.title = t;
            }
            1
        }
        0x000D => {
            // WM_GETTEXT
            let wide = c.name().ends_with('W');
            let t = c.p.windows.get(&hwnd).map(|w| w.title.clone()).unwrap_or_default();
            if wide { super::kernel32::copy_wstr_out(c, lp_, wp, &t) } else { super::kernel32_copy_str(c, lp_, wp, t.as_bytes()) }
        }
        0x000E => {
            // WM_GETTEXTLENGTH
            let wide = c.name().ends_with('W');
            c.p.windows.get(&hwnd).map(|w| if wide { w.title.encode_utf16().count() } else { w.title.len() } as u32).unwrap_or(0)
        }
        0x00B0..=0x00D9 | 0x0434 | 0x0437 => edit_message(c, hwnd, msg, wp, lp_),
        0x0084 => 1, // WM_NCHITTEST -> HTCLIENT
        0x0021 => 3, // WM_MOUSEACTIVATE -> MA_ACTIVATE
        0x0087 => 0, // WM_GETDLGCODE
        0x0046 | 0x0047 => 0,
        _ => {
            let _ = wp;
            0
        }
    }
}

fn system_metric(i: u32) -> u32 {
    match i {
        0 => 1920, // SM_CXSCREEN
        1 => 1080,
        2 => 17,   // SM_CXVSCROLL
        3 => 17,
        4 => 23,   // SM_CYCAPTION
        5 | 6 => 1,
        7 | 8 => 3,
        9 => 17,
        10 => 17,
        11 | 12 => 32,
        13 | 14 => 32,
        15 => 19,
        16 => 1920, // SM_CXFULLSCREEN
        17 => 1020,
        19 => 1,    // SM_MOUSEPRESENT
        20 | 21 => 17,
        22 => 0,
        23 => 0,
        28 => 112,  // SM_CXMIN
        29 => 27,
        30 => 21,   // SM_CXSIZE
        31 => 20,
        32 | 33 => 4,
        34 => 112,
        35 => 27,
        36 | 37 => 4,
        38 | 39 => 16,
        40 => 1,
        41 => 0,
        42 => 0,
        43 => 3,    // SM_CMOUSEBUTTONS
        45 | 46 => 2,
        47 | 48 => 75,
        49 | 50 => 16,
        51 | 52 => 16,
        53 | 54 => 16,
        63 => 0,
        67 => 1,    // SM_SLOWMACHINE? SM_CLEANBOOT
        75 => 1,    // SM_XVIRTUALSCREEN? (0)
        76 | 77 => 0,
        78 => 1920,
        79 => 1080,
        80 => 1,
        _ => 0,
    }
}

fn format_wsprintf(c: &Ctx, fmt: &str, mut args: u32) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            spec.push(n);
            chars.next();
            if n.is_ascii_alphabetic() || n == '%' {
                break;
            }
        }
        let conv = spec.chars().last().unwrap_or('%');
        let width: usize = spec.trim_end_matches(|ch: char| ch.is_ascii_alphabetic()).trim_start_matches('-').trim_start_matches('0').parse().unwrap_or(0);
        let zero = spec.starts_with('0');
        let v = c.host.read_u32(args).unwrap_or(0);
        let s = match conv {
            'd' | 'i' => { args += 4; (v as i32).to_string() }
            'u' => { args += 4; v.to_string() }
            'x' => { args += 4; format!("{v:x}") }
            'X' => { args += 4; format!("{v:X}") }
            'c' => { args += 4; ((v as u8) as char).to_string() }
            's' => { args += 4; if v == 0 { "(null)".into() } else { c.host.read_cstr_lossy(v) } }
            '%' => "%".into(),
            _ => { args += 4; String::new() }
        };
        if s.len() < width {
            let pad = if zero { "0" } else { " " }.repeat(width - s.len());
            out.push_str(&pad);
        }
        out.push_str(&s);
    }
    out
}

// ---- Unicode variants, menus, carets, display settings (second batch) --------------

fn wstr(c: &Ctx, a: u32) -> String {
    if a == 0 { String::new() } else { c.host.read_wstr(a).unwrap_or_default() }
}

fn wprop_key(c: &Ctx, i: u32) -> String {
    let a = c.arg(i);
    if a < 0x10000 { format!("#{a}") } else { wstr(c, a) }
}

fn case_map_w(c: &mut Ctx, p: u32, n: Option<u32>, upper: bool) -> u32 {
    if p < 0x10000 {
        let ch = char::from_u32(p).unwrap_or('\0');
        let m = if upper { ch.to_uppercase().next() } else { ch.to_lowercase().next() }.unwrap_or(ch);
        return m as u32;
    }
    let count = match n {
        Some(n) => n,
        None => wstr(c, p).encode_utf16().count() as u32,
    };
    for i in 0..count {
        let u = c.host.read_u16(p + 2 * i).unwrap_or(0);
        if let Some(ch) = char::from_u32(u as u32) {
            let m = if upper { ch.to_uppercase().next() } else { ch.to_lowercase().next() }.unwrap_or(ch);
            if (m as u32) < 0x10000 {
                let _ = c.host.write_u16(p + 2 * i, m as u16);
            }
        }
    }
    if n.is_some() { count } else { p }
}

fn write_devmode_w(c: &mut Ctx, p: u32) {
    let _ = c.host.fill(p, 220, 0);
    let _ = c.host.write_wstr(p, "gmrecomp display");
    let _ = c.host.write_u16(p + 64, 0x0401);
    let _ = c.host.write_u16(p + 66, 0x0401);
    let _ = c.host.write_u16(p + 68, 220);
    let _ = c.host.write_u32(p + 72, 0x0040_0000 | 0x0004_0000 | 0x0008_0000 | 0x0010_0000);
    let _ = c.host.write_u32(p + 168, 32);
    let _ = c.host.write_u32(p + 172, 1920);
    let _ = c.host.write_u32(p + 176, 1080);
    let _ = c.host.write_u32(p + 184, 60);
}

fn write_devmode_a(c: &mut Ctx, p: u32) {
    let _ = c.host.fill(p, 156, 0);
    let _ = c.host.write_cstr(p, b"gmrecomp display");
    let _ = c.host.write_u16(p + 32, 0x0401);
    let _ = c.host.write_u16(p + 34, 0x0401);
    let _ = c.host.write_u16(p + 36, 156);
    let _ = c.host.write_u32(p + 40, 0x0040_0000 | 0x0004_0000 | 0x0008_0000 | 0x0010_0000);
    let _ = c.host.write_u32(p + 104, 32);
    let _ = c.host.write_u32(p + 108, 1920);
    let _ = c.host.write_u32(p + 112, 1080);
    let _ = c.host.write_u32(p + 120, 60);
}

fn draw_text(c: &mut Ctx, wide: bool, ex: bool) -> ApiResult {
    let (hdc, s, len, rect, flags) = (c.arg(0), c.arg(1), c.arg(2) as i32, c.arg(3), c.arg(4));
    let n = if len < 0 {
        if wide { wstr(c, s).encode_utf16().count() } else { c.host.read_cstr(s).map(|v| v.len()).unwrap_or(0) }
    } else {
        len as usize
    };
    let h = {
        let dc = match c.p.handles.get(&hdc) {
            Some(Handle::Gdi(crate::process::GdiObject::Dc { font, .. })) => *font,
            _ => 0,
        };
        match c.p.handles.get(&dc) {
            Some(Handle::Gdi(crate::process::GdiObject::Font { height, .. })) if *height != 0 => height.abs(),
            _ => 16,
        }
    };
    let width = (n as i32) * (h / 2).max(1);
    if flags & 0x400 != 0 && rect != 0 {
        let (l, t, _, _) = read_rect(c, rect);
        write_rect(c, rect, l, t, l + width, t + h);
    }
    ok(if ex { 6 } else { 5 }, h as u32)
}

/// A child window created on behalf of an HLE component (no window procedure).
pub fn create_simple_child(c: &mut Ctx, class: &str, parent: u32, x: i32, y: i32, w: i32, h: i32) -> u32 {
    create_window(c, 0, class, "", WS_CHILD | 0x1000_0000, x, y, w, h, parent, 0, 0, 0).unwrap_or(0)
}

pub fn register_more(r: &mut Registry) {
    let u = "user32.dll";
    // ---- characters ----------------------------------------------------------------
    r.add(u, "CharUpperW", |c| {
        let v = case_map_w(c, c.arg(0), None, true);
        ok(1, v)
    });
    r.add(u, "CharLowerW", |c| {
        let v = case_map_w(c, c.arg(0), None, false);
        ok(1, v)
    });
    r.add(u, "CharUpperBuffW", |c| {
        let v = case_map_w(c, c.arg(0), Some(c.arg(1)), true);
        ok(2, v)
    });
    r.add(u, "CharLowerBuffW", |c| {
        let v = case_map_w(c, c.arg(0), Some(c.arg(1)), false);
        ok(2, v)
    });
    r.add(u, "CharNextW", |c| {
        let p = c.arg(0);
        let v = if c.host.read_u16(p).unwrap_or(0) == 0 { p } else { p + 2 };
        ok(1, v)
    });
    r.add(u, "CharPrevW", |c| {
        let (start, p) = (c.arg(0), c.arg(1));
        ok(2, if p > start { p - 2 } else { p })
    });
    r.add(u, "IsCharAlphaW", |c| ok(1, char::from_u32(c.arg(0)).map(|ch| ch.is_alphabetic()).unwrap_or(false) as u32));
    r.add(u, "IsCharAlphaNumericW", |c| ok(1, char::from_u32(c.arg(0)).map(|ch| ch.is_alphanumeric()).unwrap_or(false) as u32));
    r.add(u, "IsCharUpperW", |c| ok(1, char::from_u32(c.arg(0)).map(|ch| ch.is_uppercase()).unwrap_or(false) as u32));
    r.add(u, "IsCharLowerW", |c| ok(1, char::from_u32(c.arg(0)).map(|ch| ch.is_lowercase()).unwrap_or(false) as u32));
    r.add(u, "IsCharAlphaA", |c| ok(1, (c.arg(0) as u8).is_ascii_alphabetic() as u32));
    r.add(u, "IsCharAlphaNumericA", |c| ok(1, (c.arg(0) as u8).is_ascii_alphanumeric() as u32));
    r.add(u, "IsCharUpperA", |c| ok(1, (c.arg(0) as u8).is_ascii_uppercase() as u32));
    r.add(u, "IsCharLowerA", |c| ok(1, (c.arg(0) as u8).is_ascii_lowercase() as u32));

    // ---- classes, props, text -----------------------------------------------------------
    r.add(u, "GetClassInfoW", |c| {
        let name = class_name_arg(c, 1, true);
        let out = c.arg(2);
        match c.p.classes.get(&name).cloned() {
            Some(cls) => {
                write_class(c, out, &cls, true);
                ok(3, 1)
            }
            None => ok(3, 0),
        }
    });
    r.add(u, "GetClassInfoExW", |c| {
        let name = class_name_arg(c, 1, true);
        let out = c.arg(2);
        match c.p.classes.get(&name).cloned() {
            Some(cls) => {
                let _ = c.host.write_u32(out, 48);
                write_class(c, out + 4, &cls, true);
                ok(3, 1)
            }
            None => ok(3, 0),
        }
    });
    r.alias(u, "GetClassLongW", u, "GetClassLongA");
    r.alias(u, "SetClassLongW", u, "SetClassLongA");
    r.add(u, "UnregisterClassW", |c| {
        let name = class_name_arg(c, 0, true);
        c.p.classes.remove(&name);
        ok(2, 1)
    });
    r.add(u, "UnregisterClassA", |c| {
        let name = class_name_arg(c, 0, false);
        c.p.classes.remove(&name);
        ok(2, 1)
    });
    r.add(u, "SetPropW", |c| {
        let (hwnd, key, v) = (c.arg(0), wprop_key(c, 1), c.arg(2));
        match c.p.windows.get_mut(&hwnd) {
            Some(w) => {
                w.props.insert(key, v);
                ok(3, 1)
            }
            None => ok(3, 0),
        }
    });
    r.add(u, "GetPropW", |c| {
        let (hwnd, key) = (c.arg(0), wprop_key(c, 1));
        ok(2, c.p.windows.get(&hwnd).and_then(|w| w.props.get(&key)).copied().unwrap_or(0))
    });
    r.add(u, "RemovePropW", |c| {
        let (hwnd, key) = (c.arg(0), wprop_key(c, 1));
        let v = c.p.windows.get_mut(&hwnd).and_then(|w| w.props.remove(&key)).unwrap_or(0);
        ok(2, v)
    });
    r.add(u, "GetWindowTextW", |c| {
        let hwnd = c.arg(0);
        let t = c.p.windows.get(&hwnd).map(|w| w.title.clone()).unwrap_or_default();
        let n = super::kernel32::copy_wstr_out(c, c.arg(1), c.arg(2), &t);
        ok(3, n)
    });
    r.add(u, "GetWindowTextLengthW", |c| {
        let hwnd = c.arg(0);
        ok(1, c.p.windows.get(&hwnd).map(|w| w.title.encode_utf16().count() as u32).unwrap_or(0))
    });
    r.add(u, "SetWindowTextW", |c| {
        let hwnd = c.arg(0);
        let t = wstr(c, c.arg(1));
        if let Some(w) = c.p.windows.get_mut(&hwnd) {
            w.title = t;
        }
        ok(2, 1)
    });
    r.add(u, "DrawTextA", |c| draw_text(c, false, false));
    r.add(u, "DrawTextW", |c| draw_text(c, true, false));
    r.add(u, "DrawTextExA", |c| draw_text(c, false, true));
    r.add(u, "DrawTextExW", |c| draw_text(c, true, true));
    r.add(u, "LoadStringW", |c| {
        let (hinst, id, buf, cch) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        match super::kernel32::load_string(c, hinst, id) {
            Some((addr, units)) => {
                if cch == 0 {
                    let _ = c.host.write_u32(buf, addr);
                    return ok(4, units.len() as u32);
                }
                let n = units.len().min(cch as usize - 1);
                for (i, u) in units[..n].iter().enumerate() {
                    let _ = c.host.write_u16(buf + 2 * i as u32, *u);
                }
                let _ = c.host.write_u16(buf + 2 * n as u32, 0);
                ok(4, n as u32)
            }
            None => {
                if cch != 0 {
                    let _ = c.host.write_u16(buf, 0);
                }
                ok(4, 0)
            }
        }
    });
    r.add(u, "LoadStringA", |c| {
        let (hinst, id, buf, cch) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        match super::kernel32::load_string(c, hinst, id) {
            Some((_, units)) => {
                let s = String::from_utf16_lossy(&units);
                let n = super::kernel32_copy_str(c, buf, cch, s.as_bytes());
                ok(4, n)
            }
            None => {
                if cch != 0 {
                    let _ = c.host.write_u8(buf, 0);
                }
                ok(4, 0)
            }
        }
    });
    r.add(u, "LoadBitmapW", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Bitmap { w: 32, h: 32, bpp: 32, bits: 0, pitch: 0 });
        ok(2, h)
    });
    r.alias(u, "LoadBitmapA", u, "LoadBitmapW");
    r.add(u, "CreateIcon", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(7, h)
    });
    r.add(u, "CreateIconIndirect", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(1, h)
    });
    r.add(u, "CreateIconFromResourceEx", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(7, h)
    });
    r.add(u, "CreateIconFromResource", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Icon);
        ok(4, h)
    });
    r.add(u, "CreateCursor", |c| {
        let h = c.p.new_gdi(crate::process::GdiObject::Cursor);
        ok(7, h)
    });
    r.add(u, "LookupIconIdFromDirectory", |_c| ok(2, 0));
    r.add(u, "LookupIconIdFromDirectoryEx", |_c| ok(5, 0));
    r.add(u, "GetIconInfo", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u32(out, 1);
        let _ = c.host.write_u32(out + 4, 0);
        let _ = c.host.write_u32(out + 8, 0);
        let m = c.p.new_gdi(crate::process::GdiObject::Bitmap { w: 32, h: 32, bpp: 1, bits: 0, pitch: 4 });
        let col = c.p.new_gdi(crate::process::GdiObject::Bitmap { w: 32, h: 32, bpp: 32, bits: 0, pitch: 128 });
        let _ = c.host.write_u32(out + 12, m);
        let _ = c.host.write_u32(out + 16, col);
        ok(2, 1)
    });
    r.add(u, "DrawIconEx", |_c| ok(9, 1));
    r.add(u, "DrawIcon", |_c| ok(4, 1));

    // ---- menus (no UI) -------------------------------------------------------------------
    for (name, n) in [("GetMenuItemInfoW", 4), ("GetMenuItemInfoA", 4), ("SetMenuItemInfoW", 4), ("SetMenuItemInfoA", 4), ("InsertMenuItemW", 4), ("InsertMenuItemA", 4), ("InsertMenuW", 5), ("InsertMenuA", 5), ("AppendMenuW", 4), ("AppendMenuA", 4), ("ModifyMenuW", 5), ("ModifyMenuA", 5), ("DeleteMenu", 3), ("RemoveMenu", 3), ("IsMenu", 1), ("TrackPopupMenu", 7), ("TrackPopupMenuEx", 6), ("HiliteMenuItem", 4), ("SetMenuDefaultItem", 3), ("SetMenuInfo", 2), ("GetMenuInfo", 2), ("SetMenuItemBitmaps", 5), ("GetMenuItemRect", 4), ("MenuItemFromPoint", 3), ("EndMenu", 0), ("TranslateMDISysAccel", 2), ("IsDialogMessageW", 2), ("IsDialogMessageA", 2), ("IsDialogMessage", 2)] {
        match n {
            0 => r.add(u, name, |_c| ok(0, 0)),
            1 => r.add(u, name, |_c| ok(1, 0)),
            2 => r.add(u, name, |_c| ok(2, 0)),
            3 => r.add(u, name, |_c| ok(3, 0)),
            4 => r.add(u, name, |_c| ok(4, 0)),
            5 => r.add(u, name, |_c| ok(5, 0)),
            6 => r.add(u, name, |_c| ok(6, 0)),
            _ => r.add(u, name, |_c| ok(7, 0)),
        };
    }
    r.add(u, "GetMenuStringW", |_c| ok(5, 0));
    r.add(u, "GetMenuStringA", |_c| ok(5, 0));
    r.add(u, "GetMenuItemCount", |_c| ok(1, 0));
    r.add(u, "GetMenuItemID", |_c| ok(2, 0xFFFF_FFFF));
    r.add(u, "GetMenuState", |_c| ok(3, 0xFFFF_FFFF));
    r.add(u, "CheckMenuItem", |_c| ok(3, 0xFFFF_FFFF));
    r.add(u, "EnableMenuItem", |_c| ok(3, 0xFFFF_FFFF));
    r.add(u, "CheckMenuRadioItem", |_c| ok(5, 1));
    r.add(u, "GetSubMenu", |_c| ok(2, 0));
    r.add(u, "GetMenu", |_c| ok(1, 0));
    r.add(u, "GetSystemMenu", |_c| ok(2, 0));
    r.add(u, "SetMenu", |_c| ok(2, 1));
    r.add(u, "DrawMenuBar", |_c| ok(1, 1));
    r.add(u, "CreateMenu", |c| {
        let h = c.p.new_handle(Handle::Dummy("menu"));
        ok(0, h)
    });
    r.add(u, "CreatePopupMenu", |c| {
        let h = c.p.new_handle(Handle::Dummy("menu"));
        ok(0, h)
    });
    r.add(u, "DestroyMenu", |c| {
        let h = c.arg(0);
        c.p.handles.remove(&h);
        ok(1, 1)
    });
    r.add(u, "LoadMenuW", |_c| ok(2, 0));
    r.add(u, "LoadMenuA", |_c| ok(2, 0));
    r.add(u, "CreateAcceleratorTableW", |c| {
        let h = c.p.new_handle(Handle::Dummy("accel"));
        ok(2, h)
    });
    r.alias(u, "CreateAcceleratorTableA", u, "CreateAcceleratorTableW");
    r.add(u, "DestroyAcceleratorTable", |_c| ok(1, 1));
    r.add(u, "TranslateAcceleratorW", |_c| ok(3, 0));
    r.alias(u, "TranslateAcceleratorA", u, "TranslateAcceleratorW");
    r.alias(u, "TranslateAccelerator", u, "TranslateAcceleratorW");
    r.add(u, "LoadAcceleratorsW", |_c| ok(2, 0));
    r.alias(u, "LoadAcceleratorsA", u, "LoadAcceleratorsW");

    // ---- default procs -------------------------------------------------------------------
    r.add(u, "DefFrameProcW", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(2), c.arg(3), c.arg(4));
        let v = def_window_proc(c, hwnd, msg, wp, lp);
        ok(5, v)
    });
    r.alias(u, "DefFrameProcA", u, "DefFrameProcW");
    r.add(u, "DefMDIChildProcW", |c| {
        let (hwnd, msg, wp, lp) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let v = def_window_proc(c, hwnd, msg, wp, lp);
        ok(4, v)
    });
    r.alias(u, "DefMDIChildProcA", u, "DefMDIChildProcW");
    r.alias(u, "SetWindowsHookExW", u, "SetWindowsHookExA");
    r.add(u, "ShowOwnedPopups", |_c| ok(2, 1));
    r.add(u, "FindWindowW", |_c| ok(2, 0));
    r.add(u, "FindWindowA", |_c| ok(2, 0));
    r.add(u, "FindWindowExW", |_c| ok(4, 0));
    r.add(u, "FindWindowExA", |_c| ok(4, 0));

    // ---- caret, keyboard, clipboard, display -----------------------------------------
    r.add(u, "GetCaretPos", |c| {
        let _ = c.host.write_u64(c.arg(0), 0);
        ok(1, 1)
    });
    r.add(u, "SetCaretPos", |_c| ok(2, 1));
    r.add(u, "CreateCaret", |_c| ok(4, 1));
    r.add(u, "DestroyCaret", |_c| ok(0, 1));
    r.add(u, "ShowCaret", |_c| ok(1, 1));
    r.add(u, "HideCaret", |_c| ok(1, 1));
    r.add(u, "GetCaretBlinkTime", |_c| ok(0, 530));
    r.add(u, "GetKeyNameTextW", |c| {
        let n = super::kernel32::copy_wstr_out(c, c.arg(1), c.arg(2), "Key");
        ok(3, n)
    });
    r.add(u, "GetKeyboardLayoutNameW", |c| {
        let _ = c.host.write_wstr(c.arg(0), "00000409");
        ok(1, 1)
    });
    r.add(u, "LoadKeyboardLayoutW", |_c| ok(2, 0x0409_0409));
    r.add(u, "LoadKeyboardLayoutA", |_c| ok(2, 0x0409_0409));
    r.add(u, "GetKeyboardLayout", |_c| ok(1, 0x0409_0409));
    r.add(u, "ActivateKeyboardLayout", |_c| ok(2, 0x0409_0409));
    r.add(u, "GetKeyboardLayoutList", |c| {
        let (n, out) = (c.arg(0), c.arg(1));
        if n >= 1 && out != 0 {
            let _ = c.host.write_u32(out, 0x0409_0409);
        }
        ok(2, 1)
    });
    r.add(u, "UnloadKeyboardLayout", |_c| ok(1, 1));
    r.add(u, "RegisterClipboardFormatW", |c| {
        let name = wstr(c, c.arg(0));
        let a = atom(c, &name);
        ok(1, a as u32)
    });
    r.add(u, "RegisterClipboardFormatA", |c| {
        let name = c.str_arg(0);
        let a = atom(c, &name);
        ok(1, a as u32)
    });
    r.add(u, "EnumDisplaySettingsW", |c| {
        let (mode, out) = (c.arg(1), c.arg(2));
        if mode == 0 || mode == 0xFFFF_FFFF || mode == 0xFFFF_FFFE {
            write_devmode_w(c, out);
            ok(3, 1)
        } else {
            ok(3, 0)
        }
    });
    r.add(u, "EnumDisplaySettingsA", |c| {
        let (mode, out) = (c.arg(1), c.arg(2));
        if mode == 0 || mode == 0xFFFF_FFFF || mode == 0xFFFF_FFFE {
            write_devmode_a(c, out);
            ok(3, 1)
        } else {
            ok(3, 0)
        }
    });
    r.alias(u, "EnumDisplaySettingsExW", u, "EnumDisplaySettingsW");
    r.alias(u, "EnumDisplaySettingsExA", u, "EnumDisplaySettingsA");
    r.add(u, "ChangeDisplaySettingsW", |c| {
        c.trace(format!("(devmode {:#x}, flags {:#x})", c.arg(0), c.arg(1)));
        ok(2, 0)
    });
    r.alias(u, "ChangeDisplaySettingsA", u, "ChangeDisplaySettingsW");
    r.add(u, "ChangeDisplaySettingsExW", |_c| ok(5, 0));
    r.alias(u, "ChangeDisplaySettingsExA", u, "ChangeDisplaySettingsExW");
    r.add(u, "EnumDisplayDevicesW", |_c| ok(4, 0));
    r.add(u, "EnumDisplayDevicesA", |_c| ok(4, 0));
    r.add(u, "WINNLSEnableIME", |_c| ok(2, 1));
    r.add(u, "WINNLSGetEnableStatus", |_c| ok(1, 1));
    r.add(u, "WINNLSGetIMEHotkey", |_c| ok(1, 0));
    r.add(u, "PhysicalToLogicalPoint", |_c| ok(2, 1));
    r.add(u, "LogicalToPhysicalPoint", |_c| ok(2, 1));
    r.add(u, "GetGestureInfo", |_c| ok(2, 0));
    r.add(u, "CloseGestureInfoHandle", |_c| ok(1, 0));
    r.add(u, "SetGestureConfig", |_c| ok(5, 0));
    r.add(u, "GetGestureConfig", |_c| ok(6, 0));
    r.add(u, "MsgWaitForMultipleObjectsEx", |c| {
        let (count, ms) = (c.arg(0), c.arg(2));
        if !c.p.messages.is_empty() {
            return ok(5, count);
        }
        if ms != 0xFFFF_FFFF {
            c.advance_clock(ms as u64 * 1000);
            pump_timers(c);
            return ok(5, if c.p.messages.is_empty() { 258 } else { count });
        }
        c.advance_clock(10_000);
        pump_timers(c);
        ok(5, count)
    });
    r.add(u, "MsgWaitForMultipleObjects", |c| {
        let (count, ms) = (c.arg(0), c.arg(3));
        if !c.p.messages.is_empty() {
            return ok(5, count);
        }
        if ms != 0xFFFF_FFFF {
            c.advance_clock(ms as u64 * 1000);
            pump_timers(c);
            return ok(5, if c.p.messages.is_empty() { 258 } else { count });
        }
        c.advance_clock(10_000);
        pump_timers(c);
        ok(5, count)
    });
}
