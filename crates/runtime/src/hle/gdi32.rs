//! gdi32: device contexts, fonts, brushes, pens, bitmaps and regions kept as
//! metadata. Drawing calls succeed without rasterising; the browser runtime (R3)
//! renders VCL surfaces to a canvas, the harness only needs consistent handles
//! and text metrics so the Delphi forms layer initialises.

use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::PAGE_READWRITE;
use crate::process::{GdiObject, Handle};

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

pub const OBJ_PEN: u32 = 1;
pub const OBJ_BRUSH: u32 = 2;
pub const OBJ_DC: u32 = 3;
pub const OBJ_PAL: u32 = 5;
pub const OBJ_FONT: u32 = 6;
pub const OBJ_BITMAP: u32 = 7;
pub const OBJ_REGION: u32 = 8;
pub const OBJ_MEMDC: u32 = 10;

pub const NULLREGION: u32 = 1;
pub const SIMPLEREGION: u32 = 2;
pub const GDI_ERROR: u32 = 0xFFFF_FFFF;

pub const SCREEN_W: i32 = 1920;
pub const SCREEN_H: i32 = 1080;

fn gdi(c: &Ctx, h: u32) -> Option<GdiObject> {
    match c.p.handles.get(&h) {
        Some(Handle::Gdi(o)) => Some(o.clone()),
        _ => None,
    }
}

fn gdi_mut<'a>(c: &'a mut Ctx, h: u32) -> Option<&'a mut GdiObject> {
    match c.p.handles.get_mut(&h) {
        Some(Handle::Gdi(o)) => Some(o),
        _ => None,
    }
}

fn is_stock(c: &Ctx, h: u32) -> bool {
    c.p.stock_objects.values().any(|&v| v == h)
}

/// `GetStockObject` handles are created once and never deleted.
pub fn stock(c: &mut Ctx, i: u32) -> u32 {
    if let Some(&h) = c.p.stock_objects.get(&i) {
        return h;
    }
    let font = |height: i32, face: &str| GdiObject::Font { height, weight: 400, face: face.to_string() };
    let o = match i {
        0 => GdiObject::Brush { color: 0xFF_FFFF },
        1 => GdiObject::Brush { color: 0xC0_C0C0 },
        2 => GdiObject::Brush { color: 0x80_8080 },
        3 => GdiObject::Brush { color: 0x40_4040 },
        4 | 5 => GdiObject::Brush { color: 0 },
        6 => GdiObject::Pen { color: 0xFF_FFFF },
        7 | 8 => GdiObject::Pen { color: 0 },
        10 | 11 | 16 => font(12, "Courier"),
        12 => font(12, "MS Sans Serif"),
        13 | 14 => font(16, "System"),
        17 => font(-11, "Tahoma"),
        15 => GdiObject::Palette,
        18 => GdiObject::Brush { color: 0xFF_FFFF },
        19 => GdiObject::Pen { color: 0 },
        // DEFAULT_BITMAP: what a fresh memory DC has selected.
        21 => GdiObject::Bitmap { w: 1, h: 1, bpp: 1, bits: 0, pitch: 4 },
        _ => return 0,
    };
    let h = c.p.new_gdi(o);
    c.p.stock_objects.insert(i, h);
    h
}

/// A display or memory DC with the default selection.
pub fn create_dc(c: &mut Ctx, hwnd: u32) -> u32 {
    let font = stock(c, 13);
    let brush = stock(c, 0);
    let pen = stock(c, 7);
    let bitmap = stock(c, 21);
    c.p.new_gdi(GdiObject::Dc { hwnd, font, brush, pen, bitmap, text_color: 0, bk_color: 0xFF_FFFF, bk_mode: 2 })
}

fn valloc(c: &mut Ctx, size: u32) -> u32 {
    let size = size.max(1).div_ceil(0x1000) * 0x1000;
    let Some(a) = c.p.vm.alloc(0, size, true, PAGE_READWRITE, false, "gdi bits") else { return 0 };
    if c.host.ensure_mapped(a, size).is_err() {
        return 0;
    }
    let _ = c.host.fill(a, size, 0);
    a
}

fn font_height(c: &Ctx, hdc: u32) -> i32 {
    let f = match gdi(c, hdc) {
        Some(GdiObject::Dc { font, .. }) => font,
        _ => 0,
    };
    match gdi(c, f) {
        Some(GdiObject::Font { height, .. }) if height != 0 => height.abs(),
        _ => 16,
    }
}

fn logfont_bytes(height: i32, weight: i32, face: &str, wide: bool) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [height, 0, 0, 0, weight] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]);
    if wide {
        let mut units: Vec<u16> = face.encode_utf16().take(31).collect();
        units.resize(32, 0);
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
    } else {
        let mut b: Vec<u8> = face.bytes().take(31).collect();
        b.resize(32, 0);
        v.extend_from_slice(&b);
    }
    v
}

fn read_logfont(c: &Ctx, p: u32, wide: bool) -> GdiObject {
    let height = c.host.read_u32(p).unwrap_or(0) as i32;
    let weight = c.host.read_u32(p + 16).unwrap_or(400) as i32;
    let face = if wide {
        let mut s = String::new();
        for i in 0..32 {
            let u = c.host.read_u16(p + 28 + 2 * i).unwrap_or(0);
            if u == 0 {
                break;
            }
            s.push(char::from_u32(u as u32).unwrap_or('?'));
        }
        s
    } else {
        let mut b = Vec::new();
        for i in 0..32 {
            let x = c.host.read_u8(p + 28 + i).unwrap_or(0);
            if x == 0 {
                break;
            }
            b.push(x);
        }
        String::from_utf8_lossy(&b).into_owned()
    };
    GdiObject::Font { height, weight, face }
}

fn get_object(c: &mut Ctx, wide: bool) -> ApiResult {
    let h = c.arg(0);
    let cb = c.arg(1);
    let buf = c.arg(2);
    let Some(o) = gdi(c, h) else { return ok(3, 0) };
    let bytes: Vec<u8> = match o {
        GdiObject::Font { height, weight, face } => logfont_bytes(height, weight, &face, wide),
        GdiObject::Bitmap { w, h, bpp, bits, pitch } => {
            let mut v = Vec::new();
            v.extend_from_slice(&0i32.to_le_bytes());
            v.extend_from_slice(&w.to_le_bytes());
            v.extend_from_slice(&h.to_le_bytes());
            v.extend_from_slice(&pitch.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&(bpp as u16).to_le_bytes());
            v.extend_from_slice(&bits.to_le_bytes());
            v
        }
        GdiObject::Brush { color } => {
            let mut v = Vec::new();
            v.extend_from_slice(&0u32.to_le_bytes());
            v.extend_from_slice(&color.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
            v
        }
        GdiObject::Pen { color } => {
            let mut v = Vec::new();
            for x in [0u32, 1, 0, color] {
                v.extend_from_slice(&x.to_le_bytes());
            }
            v
        }
        GdiObject::Palette => 256u16.to_le_bytes().to_vec(),
        _ => return ok(3, 0),
    };
    if buf == 0 {
        return ok(3, bytes.len() as u32);
    }
    let n = bytes.len().min(cb as usize);
    let _ = c.host.write(buf, &bytes[..n]);
    ok(3, n as u32)
}

fn text_metrics(c: &mut Ctx, wide: bool) -> ApiResult {
    let hdc = c.arg(0);
    let out = c.arg(1);
    let h = font_height(c, hdc);
    let ascent = h * 4 / 5;
    let descent = h - ascent;
    let fields: [i32; 11] = [h, ascent, descent, h / 8, 0, h / 2, h, 400, 0, 96, 96];
    let mut v = Vec::new();
    for f in fields {
        v.extend_from_slice(&f.to_le_bytes());
    }
    if wide {
        for ch in [0x20u16, 0xFF, 0x3F, 0x20] {
            v.extend_from_slice(&ch.to_le_bytes());
        }
    } else {
        v.extend_from_slice(&[0x20, 0xFF, 0x3F, 0x20]);
    }
    // italic, underlined, struckout, pitch&family (variable|swiss), charset
    v.extend_from_slice(&[0, 0, 0, 0x22, 0]);
    let _ = c.host.write(out, &v);
    ok(2, 1)
}

fn text_extent(c: &mut Ctx, wide: bool) -> ApiResult {
    let hdc = c.arg(0);
    let len = c.arg(2) as i32;
    let out = c.arg(3);
    let _ = wide;
    let h = font_height(c, hdc);
    let cx = (len.max(0) * (h / 2).max(1)) as u32;
    let _ = c.host.write_u32(out, cx);
    let _ = c.host.write_u32(out + 4, h as u32);
    ok(4, 1)
}

fn select_object(c: &mut Ctx) -> ApiResult {
    let hdc = c.arg(0);
    let h = c.arg(1);
    let Some(obj) = gdi(c, h) else { return ok(2, 0) };
    let Some(GdiObject::Dc { font, brush, pen, bitmap, .. }) = gdi_mut(c, hdc) else { return ok(2, 0) };
    let prev = match obj {
        GdiObject::Font { .. } => std::mem::replace(font, h),
        GdiObject::Brush { .. } => std::mem::replace(brush, h),
        GdiObject::Pen { .. } => std::mem::replace(pen, h),
        GdiObject::Bitmap { .. } => std::mem::replace(bitmap, h),
        GdiObject::Region => SIMPLEREGION,
        _ => 0,
    };
    ok(2, prev)
}

fn device_caps(index: u32) -> u32 {
    match index {
        0 => 0x4000,                       // DRIVERVERSION
        2 => 1,                            // TECHNOLOGY: DT_RASDISPLAY
        4 => 508,                          // HORZSIZE (mm)
        6 => 285,                          // VERTSIZE
        8 => SCREEN_W as u32,              // HORZRES
        10 => SCREEN_H as u32,             // VERTRES
        12 => 32,                          // BITSPIXEL
        14 => 1,                           // PLANES
        16 | 18 => 0xFFFF_FFFF,            // NUMBRUSHES, NUMPENS
        20 => 0,                           // NUMMARKERS
        22 => 0,                           // NUMFONTS
        24 => 0xFFFF_FFFF,                 // NUMCOLORS
        26 => 0,                           // PDEVICESIZE
        28 => 0x1FF,                       // CURVECAPS
        30 => 0x3FFF,                      // LINECAPS
        32 => 0xFFF,                       // POLYGONALCAPS
        34 => 0xFFFF,                      // TEXTCAPS
        36 => 0,                           // CLIPCAPS (CP_NONE)… VCL only checks nonzero rarely
        38 => 0x2A81,                      // RASTERCAPS: BITBLT|DI_BITMAP|DIBTODEV|STRETCHBLT|STRETCHDIB
        40 | 42 => 36,                     // ASPECTX/Y
        44 => 51,                          // ASPECTXY
        88 | 90 => 96,                     // LOGPIXELSX/Y
        104 => 0,                          // SIZEPALETTE
        106 => 20,                         // NUMRESERVED
        108 => 24,                         // COLORRES
        116 => 60,                         // VREFRESH
        _ => 0,
    }
}

fn charset_codepage(charset: u32) -> Option<u32> {
    Some(match charset {
        0 => 1252,
        128 => 932,
        129 => 949,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        255 => 437,
        _ => return None,
    })
}

fn translate_charset_info(c: &mut Ctx) -> ApiResult {
    let src = c.arg(0);
    let out = c.arg(1);
    let flags = c.arg(2);
    let (charset, cp) = match flags {
        1 => match charset_codepage(src) {
            Some(cp) => (src, cp),
            None => return ok(3, 0),
        },
        2 => {
            let found = (0u32..256).find(|&cs| charset_codepage(cs) == Some(src));
            match found {
                Some(cs) => (cs, src),
                None => return ok(3, 0),
            }
        }
        4 => {
            let csb0 = c.host.read_u32(src + 16).unwrap_or(0);
            let bit = csb0.trailing_zeros();
            let cs = match bit {
                0 => 0,
                1 => 238,
                2 => 204,
                3 => 161,
                4 => 162,
                5 => 177,
                6 => 178,
                7 => 186,
                16 => 222,
                17 => 128,
                18 => 134,
                19 => 129,
                20 => 136,
                _ => return ok(3, 0),
            };
            (cs, charset_codepage(cs).unwrap_or(1252))
        }
        _ => return ok(3, 0),
    };
    let _ = c.host.write_u32(out, charset);
    let _ = c.host.write_u32(out + 4, cp);
    let _ = c.host.fill(out + 8, 24, 0);
    let _ = c.host.write_u32(out + 24, 1 << (if charset == 128 { 17 } else { 0 }));
    ok(3, 1)
}

fn read_bmi(c: &Ctx, bmi: u32) -> (i32, i32, u32) {
    let w = c.host.read_u32(bmi + 4).unwrap_or(0) as i32;
    let h = c.host.read_u32(bmi + 8).unwrap_or(0) as i32;
    let bpp = c.host.read_u16(bmi + 14).unwrap_or(32) as u32;
    (w, h, bpp)
}

fn new_bitmap(c: &mut Ctx, w: i32, h: i32, bpp: u32, with_bits: bool) -> u32 {
    let pitch = ((w.unsigned_abs() * bpp + 31) / 32) * 4;
    let size = pitch * h.unsigned_abs();
    let bits = if with_bits && size != 0 { valloc(c, size) } else { 0 };
    c.p.new_gdi(GdiObject::Bitmap { w, h, bpp, bits, pitch })
}

fn write_bitmapinfoheader(c: &mut Ctx, p: u32, w: i32, h: i32, bpp: u32, pitch: u32) {
    let _ = c.host.write_u32(p, 40);
    let _ = c.host.write_u32(p + 4, w as u32);
    let _ = c.host.write_u32(p + 8, h as u32);
    let _ = c.host.write_u16(p + 12, 1);
    let _ = c.host.write_u16(p + 14, bpp as u16);
    let _ = c.host.write_u32(p + 16, 0);
    let _ = c.host.write_u32(p + 20, pitch * h.unsigned_abs());
    let _ = c.host.write_u32(p + 24, 3780);
    let _ = c.host.write_u32(p + 28, 3780);
    let _ = c.host.write_u32(p + 32, 0);
    let _ = c.host.write_u32(p + 36, 0);
}

/// Copies whole scanlines between a bitmap's bits and a DIB buffer (same layout assumed).
fn copy_rows(c: &mut Ctx, dst: u32, src: u32, pitch: u32, start: u32, lines: u32, h: u32) -> u32 {
    if dst == 0 || src == 0 || pitch == 0 {
        return 0;
    }
    let lines = lines.min(h.saturating_sub(start));
    let n = pitch * lines;
    let off = pitch * start;
    let mut buf = vec![0u8; n as usize];
    if c.host.read(src + off, &mut buf).is_ok() {
        let _ = c.host.write(dst + off, &buf);
    }
    lines
}

pub fn register(r: &mut Registry) {
    let g = "gdi32.dll";
    // ---- DCs -----------------------------------------------------------------
    r.add(g, "CreateCompatibleDC", |c| {
        let dc = create_dc(c, 0);
        ok(1, dc)
    });
    r.add(g, "CreateDCA", |c| {
        let dc = create_dc(c, 0);
        ok(4, dc)
    });
    r.alias(g, "CreateDCW", g, "CreateDCA");
    r.alias(g, "CreateICA", g, "CreateDCA");
    r.alias(g, "CreateICW", g, "CreateDCA");
    r.add(g, "DeleteDC", |c| {
        let h = c.arg(0);
        let r = matches!(c.p.handles.remove(&h), Some(Handle::Gdi(GdiObject::Dc { .. })));
        ok(1, r as u32)
    });
    r.add(g, "SaveDC", |_c| ok(1, 1));
    r.add(g, "RestoreDC", |_c| ok(2, 1));
    r.add(g, "GetDeviceCaps", |c| {
        let i = c.arg(1);
        ok(2, device_caps(i))
    });
    r.add(g, "GdiFlush", |_c| ok(0, 1));
    r.add(g, "GetDCOrgEx", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u64(out, 0);
        ok(2, 1)
    });
    r.add(g, "GetObjectType", |c| {
        let h = c.arg(0);
        let t = match gdi(c, h) {
            Some(GdiObject::Dc { hwnd, .. }) => if hwnd == 0 { OBJ_MEMDC } else { OBJ_DC },
            Some(GdiObject::Font { .. }) => OBJ_FONT,
            Some(GdiObject::Brush { .. }) => OBJ_BRUSH,
            Some(GdiObject::Pen { .. }) => OBJ_PEN,
            Some(GdiObject::Bitmap { .. }) => OBJ_BITMAP,
            Some(GdiObject::Region) => OBJ_REGION,
            Some(GdiObject::Palette) => OBJ_PAL,
            _ => 0,
        };
        ok(1, t)
    });
    r.add(g, "GetCurrentObject", |c| {
        let hdc = c.arg(0);
        let ty = c.arg(1);
        let v = match gdi(c, hdc) {
            Some(GdiObject::Dc { font, brush, pen, bitmap, .. }) => match ty {
                OBJ_PEN => pen,
                OBJ_BRUSH => brush,
                OBJ_FONT => font,
                OBJ_BITMAP => bitmap,
                OBJ_PAL => 0,
                _ => 0,
            },
            _ => 0,
        };
        ok(2, v)
    });
    r.add(g, "SelectObject", select_object);
    r.add(g, "DeleteObject", |c| {
        let h = c.arg(0);
        if h == 0 || is_stock(c, h) {
            return ok(1, (h != 0) as u32);
        }
        let removed = match c.p.handles.get(&h) {
            Some(Handle::Gdi(GdiObject::Dc { .. })) => false,
            Some(Handle::Gdi(_)) => {
                c.p.handles.remove(&h);
                true
            }
            _ => false,
        };
        ok(1, removed as u32)
    });
    r.add(g, "GetStockObject", |c| {
        let i = c.arg(0);
        let h = stock(c, i);
        ok(1, h)
    });
    r.add(g, "GetObjectA", |c| get_object(c, false));
    r.add(g, "GetObjectW", |c| get_object(c, true));
    r.add(g, "UnrealizeObject", |_c| ok(1, 1));

    // ---- DC attributes -------------------------------------------------------
    r.add(g, "SetBkColor", |c| {
        let hdc = c.arg(0);
        let v = c.arg(1);
        let prev = match gdi_mut(c, hdc) {
            Some(GdiObject::Dc { bk_color, .. }) => std::mem::replace(bk_color, v),
            _ => 0xFFFF_FFFF,
        };
        ok(2, prev)
    });
    r.add(g, "SetTextColor", |c| {
        let hdc = c.arg(0);
        let v = c.arg(1);
        let prev = match gdi_mut(c, hdc) {
            Some(GdiObject::Dc { text_color, .. }) => std::mem::replace(text_color, v),
            _ => 0xFFFF_FFFF,
        };
        ok(2, prev)
    });
    r.add(g, "SetBkMode", |c| {
        let hdc = c.arg(0);
        let v = c.arg(1);
        let prev = match gdi_mut(c, hdc) {
            Some(GdiObject::Dc { bk_mode, .. }) => std::mem::replace(bk_mode, v),
            _ => 0,
        };
        ok(2, prev)
    });
    r.add(g, "GetBkColor", |c| {
        let hdc = c.arg(0);
        let v = match gdi(c, hdc) {
            Some(GdiObject::Dc { bk_color, .. }) => bk_color,
            _ => 0xFFFF_FFFF,
        };
        ok(1, v)
    });
    r.add(g, "GetTextColor", |c| {
        let hdc = c.arg(0);
        let v = match gdi(c, hdc) {
            Some(GdiObject::Dc { text_color, .. }) => text_color,
            _ => 0xFFFF_FFFF,
        };
        ok(1, v)
    });
    r.add(g, "SetROP2", |_c| ok(2, 13));
    r.add(g, "SetStretchBltMode", |_c| ok(2, 1));
    r.add(g, "SetTextAlign", |_c| ok(2, 0));
    r.add(g, "GetTextAlign", |_c| ok(1, 0));
    r.add(g, "SetMapMode", |_c| ok(2, 1));
    r.add(g, "GetMapMode", |_c| ok(1, 1));
    r.add(g, "SetPolyFillMode", |_c| ok(2, 1));
    r.add(g, "SetTextCharacterExtra", |_c| ok(2, 0));
    for name in ["SetViewportOrgEx", "SetWindowOrgEx", "SetViewportExtEx", "SetWindowExtEx", "SetBrushOrgEx"] {
        r.add(g, name, |c| {
            let out = c.arg(3);
            if out != 0 {
                let _ = c.host.write_u64(out, 0);
            }
            ok(4, 1)
        });
    }
    for name in ["GetWindowOrgEx", "GetViewportOrgEx", "GetWindowExtEx", "GetViewportExtEx", "GetBrushOrgEx", "GetCurrentPositionEx"] {
        r.add(g, name, |c| {
            let out = c.arg(1);
            if out != 0 {
                let _ = c.host.write_u64(out, 0);
            }
            ok(2, 1)
        });
    }
    r.add(g, "LPtoDP", |_c| ok(3, 1));
    r.add(g, "DPtoLP", |_c| ok(3, 1));
    r.add(g, "SetAbortProc", |_c| ok(2, 1));
    r.add(g, "StartDocA", |_c| ok(2, 1));
    r.alias(g, "StartDocW", g, "StartDocA");
    r.add(g, "EndDoc", |_c| ok(1, 1));
    r.add(g, "AbortDoc", |_c| ok(1, 1));
    r.add(g, "StartPage", |_c| ok(1, 1));
    r.add(g, "EndPage", |_c| ok(1, 1));

    // ---- fonts and text ------------------------------------------------------
    r.add(g, "CreateFontA", |c| {
        let height = c.arg_i32(0);
        let weight = c.arg_i32(4);
        let face = c.str_arg(13);
        let h = c.p.new_gdi(GdiObject::Font { height, weight, face });
        ok(14, h)
    });
    r.add(g, "CreateFontW", |c| {
        let height = c.arg_i32(0);
        let weight = c.arg_i32(4);
        let face = c.wstr_arg(13);
        let h = c.p.new_gdi(GdiObject::Font { height, weight, face });
        ok(14, h)
    });
    r.add(g, "CreateFontIndirectA", |c| {
        let p = c.arg(0);
        let f = read_logfont(c, p, false);
        let h = c.p.new_gdi(f);
        ok(1, h)
    });
    r.add(g, "CreateFontIndirectW", |c| {
        let p = c.arg(0);
        let f = read_logfont(c, p, true);
        let h = c.p.new_gdi(f);
        ok(1, h)
    });
    r.add(g, "GetTextMetricsA", |c| text_metrics(c, false));
    r.add(g, "GetTextMetricsW", |c| text_metrics(c, true));
    r.add(g, "GetTextExtentPoint32A", |c| text_extent(c, false));
    r.add(g, "GetTextExtentPoint32W", |c| text_extent(c, true));
    r.alias(g, "GetTextExtentPointA", g, "GetTextExtentPoint32A");
    r.alias(g, "GetTextExtentPointW", g, "GetTextExtentPoint32W");
    r.add(g, "GetTextExtentExPointA", |c| {
        let hdc = c.arg(0);
        let len = c.arg(2) as i32;
        let fit = c.arg(4);
        let dx = c.arg(5);
        let out = c.arg(6);
        let h = font_height(c, hdc);
        let cw = (h / 2).max(1);
        if fit != 0 {
            let _ = c.host.write_u32(fit, len.max(0) as u32);
        }
        if dx != 0 {
            for i in 0..len.max(0) {
                let _ = c.host.write_u32(dx + 4 * i as u32, ((i + 1) * cw) as u32);
            }
        }
        if out != 0 {
            let _ = c.host.write_u32(out, (len.max(0) * cw) as u32);
            let _ = c.host.write_u32(out + 4, h as u32);
        }
        ok(7, 1)
    });
    r.alias(g, "GetTextExtentExPointW", g, "GetTextExtentExPointA");
    r.add(g, "ExtTextOutA", |_c| ok(8, 1));
    r.alias(g, "ExtTextOutW", g, "ExtTextOutA");
    r.add(g, "TextOutA", |_c| ok(5, 1));
    r.alias(g, "TextOutW", g, "TextOutA");
    r.add(g, "GetCharABCWidthsA", |c| {
        let hdc = c.arg(0);
        let first = c.arg(1);
        let last = c.arg(2);
        let out = c.arg(3);
        let w = (font_height(c, hdc) / 2).max(1) as u32;
        for i in 0..=last.saturating_sub(first) {
            let _ = c.host.write_u32(out + 12 * i, 0);
            let _ = c.host.write_u32(out + 12 * i + 4, w);
            let _ = c.host.write_u32(out + 12 * i + 8, 0);
        }
        ok(4, 1)
    });
    r.alias(g, "GetCharABCWidthsW", g, "GetCharABCWidthsA");
    r.add(g, "GetCharWidth32A", |c| {
        let hdc = c.arg(0);
        let first = c.arg(1);
        let last = c.arg(2);
        let out = c.arg(3);
        let w = (font_height(c, hdc) / 2).max(1) as u32;
        for i in 0..=last.saturating_sub(first) {
            let _ = c.host.write_u32(out + 4 * i, w);
        }
        ok(4, 1)
    });
    r.alias(g, "GetCharWidth32W", g, "GetCharWidth32A");
    r.alias(g, "GetCharWidthA", g, "GetCharWidth32A");
    r.alias(g, "GetCharWidthW", g, "GetCharWidth32A");
    r.add(g, "GetCharacterPlacementA", |_c| ok(6, 0));
    r.alias(g, "GetCharacterPlacementW", g, "GetCharacterPlacementA");
    r.add(g, "GetGlyphOutlineA", |_c| ok(7, GDI_ERROR));
    r.alias(g, "GetGlyphOutlineW", g, "GetGlyphOutlineA");
    r.add(g, "GetOutlineTextMetricsA", |_c| ok(3, 0));
    r.alias(g, "GetOutlineTextMetricsW", g, "GetOutlineTextMetricsA");
    r.add(g, "GetFontLanguageInfo", |_c| ok(1, 0));
    r.add(g, "EnumFontFamiliesExW", |_c| ok(5, 0));
    r.alias(g, "EnumFontFamiliesExA", g, "EnumFontFamiliesExW");
    r.add(g, "EnumFontsW", |_c| ok(4, 0));
    r.alias(g, "EnumFontsA", g, "EnumFontsW");
    r.alias(g, "EnumFontFamiliesA", g, "EnumFontsW");
    r.alias(g, "EnumFontFamiliesW", g, "EnumFontsW");
    r.add(g, "TranslateCharsetInfo", translate_charset_info);
    r.add(g, "GetTextCharsetInfo", |_c| ok(3, 0));
    r.add(g, "GetTextCharset", |_c| ok(1, 0));
    r.add(g, "GetTextFaceA", |c| {
        let hdc = c.arg(0);
        let cap = c.arg(1);
        let buf = c.arg(2);
        let face = match gdi(c, hdc).and_then(|d| if let GdiObject::Dc { font, .. } = d { gdi(c, font) } else { None }) {
            Some(GdiObject::Font { face, .. }) => face,
            _ => "System".into(),
        };
        let n = super::kernel32_copy_str(c, buf, cap, face.as_bytes());
        ok(3, n)
    });

    // ---- brushes, pens, palettes ---------------------------------------------
    r.add(g, "CreateSolidBrush", |c| {
        let color = c.arg(0);
        let h = c.p.new_gdi(GdiObject::Brush { color });
        ok(1, h)
    });
    r.add(g, "CreateBrushIndirect", |c| {
        let p = c.arg(0);
        let color = c.host.read_u32(p + 4).unwrap_or(0);
        let h = c.p.new_gdi(GdiObject::Brush { color });
        ok(1, h)
    });
    r.add(g, "CreateHatchBrush", |c| {
        let color = c.arg(1);
        let h = c.p.new_gdi(GdiObject::Brush { color });
        ok(2, h)
    });
    r.add(g, "CreatePatternBrush", |c| {
        let h = c.p.new_gdi(GdiObject::Brush { color: 0 });
        ok(1, h)
    });
    r.add(g, "CreateDIBPatternBrushPt", |c| {
        let h = c.p.new_gdi(GdiObject::Brush { color: 0 });
        ok(2, h)
    });
    r.add(g, "CreatePen", |c| {
        let color = c.arg(2);
        let h = c.p.new_gdi(GdiObject::Pen { color });
        ok(3, h)
    });
    r.add(g, "CreatePenIndirect", |c| {
        let p = c.arg(0);
        let color = c.host.read_u32(p + 12).unwrap_or(0);
        let h = c.p.new_gdi(GdiObject::Pen { color });
        ok(1, h)
    });
    r.add(g, "ExtCreatePen", |c| {
        let lb = c.arg(2);
        let color = c.host.read_u32(lb + 4).unwrap_or(0);
        let h = c.p.new_gdi(GdiObject::Pen { color });
        ok(5, h)
    });
    r.add(g, "CreatePalette", |c| {
        let h = c.p.new_gdi(GdiObject::Palette);
        ok(1, h)
    });
    r.add(g, "CreateHalftonePalette", |c| {
        let h = c.p.new_gdi(GdiObject::Palette);
        ok(1, h)
    });
    r.add(g, "SelectPalette", |c| {
        let h = stock(c, 15);
        ok(3, h)
    });
    r.add(g, "RealizePalette", |_c| ok(1, 0));
    r.add(g, "ResizePalette", |_c| ok(2, 1));
    r.add(g, "GetPaletteEntries", |_c| ok(4, 0));
    r.add(g, "SetPaletteEntries", |_c| ok(4, 0));
    r.add(g, "GetSystemPaletteEntries", |_c| ok(4, 0));
    r.add(g, "GetNearestPaletteIndex", |_c| ok(2, 0));
    r.add(g, "GetNearestColor", |c| ok(2, c.arg(1)));
    r.add(g, "AnimatePalette", |_c| ok(4, 1));
    r.add(g, "SetSystemPaletteUse", |_c| ok(2, 1));
    r.add(g, "GetSystemPaletteUse", |_c| ok(1, 1));

    // ---- bitmaps --------------------------------------------------------------
    r.add(g, "CreateBitmap", |c| {
        let w = c.arg_i32(0);
        let h = c.arg_i32(1);
        let bpp = c.arg(2) * c.arg(3);
        let bits = c.arg(4);
        let hb = new_bitmap(c, w, h, bpp.max(1), true);
        if bits != 0 {
            if let Some(GdiObject::Bitmap { bits: dst, pitch, .. }) = gdi(c, hb) {
                copy_rows(c, dst, bits, pitch, 0, h.unsigned_abs(), h.unsigned_abs());
            }
        }
        ok(5, hb)
    });
    r.add(g, "CreateBitmapIndirect", |c| {
        let p = c.arg(0);
        let w = c.host.read_u32(p + 4).unwrap_or(0) as i32;
        let h = c.host.read_u32(p + 8).unwrap_or(0) as i32;
        let bpp = c.host.read_u16(p + 18).unwrap_or(32) as u32;
        let hb = new_bitmap(c, w, h, bpp, true);
        ok(1, hb)
    });
    r.add(g, "CreateCompatibleBitmap", |c| {
        let w = c.arg_i32(1);
        let h = c.arg_i32(2);
        let hb = new_bitmap(c, w, h, 32, true);
        ok(3, hb)
    });
    r.add(g, "CreateDIBitmap", |c| {
        let bmih = c.arg(1);
        let (w, h, bpp) = read_bmi(c, bmih);
        let hb = new_bitmap(c, w, h, bpp.max(1), true);
        let init = c.arg(2);
        let bits = c.arg(3);
        if init & 4 != 0 && bits != 0 {
            if let Some(GdiObject::Bitmap { bits: dst, pitch, .. }) = gdi(c, hb) {
                copy_rows(c, dst, bits, pitch, 0, h.unsigned_abs(), h.unsigned_abs());
            }
        }
        ok(6, hb)
    });
    r.add(g, "CreateDIBSection", |c| {
        let bmi = c.arg(1);
        let ppv = c.arg(3);
        let (w, h, bpp) = read_bmi(c, bmi);
        let hb = new_bitmap(c, w, h, bpp.max(1), true);
        let bits = match gdi(c, hb) {
            Some(GdiObject::Bitmap { bits, .. }) => bits,
            _ => 0,
        };
        if ppv != 0 {
            let _ = c.host.write_u32(ppv, bits);
        }
        c.trace(format!("({w}x{h}x{bpp}) = {hb:#x} bits={bits:#x}"));
        ok(6, hb)
    });
    r.add(g, "GetBitmapBits", |c| {
        let hb = c.arg(0);
        let cb = c.arg(1);
        let out = c.arg(2);
        let n = match gdi(c, hb) {
            Some(GdiObject::Bitmap { h, bits, pitch, .. }) if bits != 0 => {
                let total = pitch * h.unsigned_abs();
                let n = cb.min(total);
                let mut buf = vec![0u8; n as usize];
                let _ = c.host.read(bits, &mut buf);
                let _ = c.host.write(out, &buf);
                n
            }
            _ => 0,
        };
        ok(3, n)
    });
    r.add(g, "SetBitmapBits", |c| {
        let hb = c.arg(0);
        let cb = c.arg(1);
        let src = c.arg(2);
        let n = match gdi(c, hb) {
            Some(GdiObject::Bitmap { h, bits, pitch, .. }) if bits != 0 => {
                let total = pitch * h.unsigned_abs();
                let n = cb.min(total);
                let mut buf = vec![0u8; n as usize];
                let _ = c.host.read(src, &mut buf);
                let _ = c.host.write(bits, &buf);
                n
            }
            _ => 0,
        };
        ok(3, n)
    });
    r.add(g, "GetDIBits", |c| {
        let hb = c.arg(1);
        let start = c.arg(2);
        let lines = c.arg(3);
        let out = c.arg(4);
        let bmi = c.arg(5);
        let Some(GdiObject::Bitmap { w, h, bpp, bits, pitch }) = gdi(c, hb) else { return ok(7, 0) };
        if out == 0 {
            write_bitmapinfoheader(c, bmi, w, h, bpp, pitch);
            return ok(7, h.unsigned_abs());
        }
        // Honour the caller's requested depth only when it matches ours.
        let (_, _, req_bpp) = read_bmi(c, bmi);
        if req_bpp != bpp || bits == 0 {
            c.trace(format!(": depth conversion {bpp}->{req_bpp} not implemented"));
            return ok(7, 0);
        }
        let n = copy_rows(c, out, bits, pitch, start, lines, h.unsigned_abs());
        ok(7, n)
    });
    r.add(g, "SetDIBits", |c| {
        let hb = c.arg(1);
        let start = c.arg(2);
        let lines = c.arg(3);
        let src = c.arg(4);
        let bmi = c.arg(5);
        let Some(GdiObject::Bitmap { h, bpp, bits, pitch, .. }) = gdi(c, hb) else { return ok(7, 0) };
        let (_, _, req_bpp) = read_bmi(c, bmi);
        if req_bpp != bpp || bits == 0 {
            c.trace(format!(": depth conversion {req_bpp}->{bpp} not implemented"));
            return ok(7, lines);
        }
        let n = copy_rows(c, bits, src, pitch, start, lines, h.unsigned_abs());
        ok(7, n)
    });
    r.add(g, "SetDIBitsToDevice", |c| ok(12, c.arg(5)));
    r.add(g, "StretchDIBits", |c| ok(13, c.arg(8)));
    r.add(g, "GetDIBColorTable", |_c| ok(4, 0));
    r.add(g, "SetDIBColorTable", |_c| ok(4, 0));

    // ---- drawing (no rasterisation in the harness) ---------------------------
    r.add(g, "BitBlt", |_c| ok(9, 1));
    r.add(g, "StretchBlt", |_c| ok(11, 1));
    r.add(g, "PatBlt", |_c| ok(6, 1));
    r.add(g, "MaskBlt", |_c| ok(12, 1));
    r.add(g, "PlgBlt", |_c| ok(10, 1));
    r.add(g, "Rectangle", |_c| ok(5, 1));
    r.add(g, "Ellipse", |_c| ok(5, 1));
    r.add(g, "RoundRect", |_c| ok(7, 1));
    r.add(g, "Arc", |_c| ok(9, 1));
    r.add(g, "Chord", |_c| ok(9, 1));
    r.add(g, "Pie", |_c| ok(9, 1));
    r.add(g, "LineTo", |_c| ok(3, 1));
    r.add(g, "MoveToEx", |c| {
        let out = c.arg(3);
        if out != 0 {
            let _ = c.host.write_u64(out, 0);
        }
        ok(4, 1)
    });
    r.add(g, "Polygon", |_c| ok(3, 1));
    r.add(g, "Polyline", |_c| ok(3, 1));
    r.add(g, "PolylineTo", |_c| ok(3, 1));
    r.add(g, "PolyBezier", |_c| ok(3, 1));
    r.add(g, "PolyBezierTo", |_c| ok(3, 1));
    r.add(g, "PolyPolyline", |_c| ok(4, 1));
    r.add(g, "PolyPolygon", |_c| ok(4, 1));
    r.add(g, "ExtFloodFill", |_c| ok(5, 1));
    r.add(g, "FloodFill", |_c| ok(4, 1));
    r.add(g, "SetPixel", |c| ok(4, c.arg(3)));
    r.add(g, "SetPixelV", |_c| ok(4, 1));
    r.add(g, "GetPixel", |_c| ok(3, 0));
    r.add(g, "FrameRgn", |_c| ok(5, 1));
    r.add(g, "FillRgn", |_c| ok(3, 1));
    r.add(g, "PaintRgn", |_c| ok(2, 1));
    r.add(g, "InvertRgn", |_c| ok(2, 1));

    // ---- regions and clipping -------------------------------------------------
    r.add(g, "CreateRectRgn", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(4, h)
    });
    r.add(g, "CreateRectRgnIndirect", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(1, h)
    });
    r.add(g, "CreateEllipticRgn", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(4, h)
    });
    r.add(g, "CreatePolygonRgn", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(3, h)
    });
    r.add(g, "CreateRoundRectRgn", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(6, h)
    });
    r.add(g, "CombineRgn", |_c| ok(4, SIMPLEREGION));
    r.add(g, "SetRectRgn", |_c| ok(5, 1));
    r.add(g, "OffsetRgn", |_c| ok(3, SIMPLEREGION));
    r.add(g, "PtInRegion", |_c| ok(3, 1));
    r.add(g, "RectInRegion", |_c| ok(2, 1));
    r.add(g, "EqualRgn", |_c| ok(2, 0));
    r.add(g, "GetRegionData", |_c| ok(3, 0));
    r.add(g, "ExtCreateRegion", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(3, h)
    });
    r.add(g, "SelectClipRgn", |_c| ok(2, SIMPLEREGION));
    r.add(g, "ExtSelectClipRgn", |_c| ok(3, SIMPLEREGION));
    r.add(g, "GetClipRgn", |_c| ok(2, 0));
    r.add(g, "ExcludeClipRect", |_c| ok(5, SIMPLEREGION));
    r.add(g, "IntersectClipRect", |_c| ok(5, SIMPLEREGION));
    r.add(g, "OffsetClipRgn", |_c| ok(3, SIMPLEREGION));
    r.add(g, "GetClipBox", |c| {
        let hdc = c.arg(0);
        let out = c.arg(1);
        let (w, h) = match gdi(c, hdc) {
            Some(GdiObject::Dc { hwnd, bitmap, .. }) => {
                if let Some(GdiObject::Bitmap { w, h, .. }) = gdi(c, bitmap) {
                    (w, h.abs())
                } else if let Some(win) = c.p.windows.get(&hwnd) {
                    (win.w, win.h)
                } else {
                    (SCREEN_W, SCREEN_H)
                }
            }
            _ => (SCREEN_W, SCREEN_H),
        };
        super::user32::write_rect(c, out, 0, 0, w, h);
        ok(2, SIMPLEREGION)
    });
    r.add(g, "GetRgnBox", |c| {
        let out = c.arg(1);
        super::user32::write_rect(c, out, 0, 0, SCREEN_W, SCREEN_H);
        ok(2, SIMPLEREGION)
    });
    r.add(g, "RectVisible", |_c| ok(2, 1));
    r.add(g, "PtVisible", |_c| ok(3, 1));

    // ---- metafiles (unsupported) ---------------------------------------------
    r.add(g, "CreateEnhMetaFileA", |_c| ok(4, 0));
    r.alias(g, "CreateEnhMetaFileW", g, "CreateEnhMetaFileA");
    r.add(g, "CloseEnhMetaFile", |_c| ok(1, 0));
    r.add(g, "CopyEnhMetaFileA", |_c| ok(2, 0));
    r.alias(g, "CopyEnhMetaFileW", g, "CopyEnhMetaFileA");
    r.add(g, "DeleteEnhMetaFile", |_c| ok(1, 1));
    r.add(g, "GetEnhMetaFileA", |_c| ok(1, 0));
    r.alias(g, "GetEnhMetaFileW", g, "GetEnhMetaFileA");
    r.add(g, "GetEnhMetaFileBits", |_c| ok(3, 0));
    r.add(g, "GetEnhMetaFileHeader", |_c| ok(3, 0));
    r.add(g, "GetEnhMetaFileDescriptionA", |_c| ok(3, 0));
    r.alias(g, "GetEnhMetaFileDescriptionW", g, "GetEnhMetaFileDescriptionA");
    r.add(g, "GetEnhMetaFilePaletteEntries", |_c| ok(3, 0));
    r.add(g, "GetWinMetaFileBits", |_c| ok(5, 0));
    r.add(g, "PlayEnhMetaFile", |_c| ok(3, 0));
    r.add(g, "SetEnhMetaFileBits", |_c| ok(2, 0));
    r.add(g, "SetWinMetaFileBits", |_c| ok(4, 0));
    r.add(g, "GetMetaFileA", |_c| ok(1, 0));
    r.add(g, "PlayMetaFile", |_c| ok(2, 0));
    r.add(g, "DeleteMetaFile", |_c| ok(1, 1));
    r.add(g, "SetMetaFileBitsEx", |_c| ok(2, 0));
    r.add(g, "GetMetaFileBitsEx", |_c| ok(3, 0));
    r.add(g, "CreateMetaFileA", |_c| ok(1, 0));
    r.add(g, "CloseMetaFile", |_c| ok(1, 0));
    r.add(g, "CopyMetaFileA", |_c| ok(2, 0));

    // ---- misc -------------------------------------------------------------------
    r.add(g, "GdiSetBatchLimit", |_c| ok(1, 20));
    r.add(g, "GdiGetBatchLimit", |_c| ok(0, 20));
    r.add(g, "GetColorAdjustment", |_c| ok(2, 1));
    r.add(g, "SetColorAdjustment", |_c| ok(2, 1));
    r.add(g, "SetLayout", |_c| ok(2, 0));
    r.add(g, "GetLayout", |_c| ok(1, 0));
    r.add(g, "SetGraphicsMode", |_c| ok(2, 1));
    r.add(g, "GetGraphicsMode", |_c| ok(1, 1));
    r.add(g, "SetWorldTransform", |_c| ok(2, 1));
    r.add(g, "GetWorldTransform", |_c| ok(2, 1));
    r.add(g, "ModifyWorldTransform", |_c| ok(3, 1));
    r.add(g, "GetBoundsRect", |_c| ok(3, 0));
    r.add(g, "SetBoundsRect", |_c| ok(3, 0));
    r.add(g, "GetKerningPairsA", |_c| ok(3, 0));
    r.alias(g, "GetKerningPairsW", g, "GetKerningPairsA");
    r.add(g, "AddFontResourceA", |_c| ok(1, 0));
    r.alias(g, "AddFontResourceW", g, "AddFontResourceA");
    r.add(g, "AddFontResourceExA", |_c| ok(3, 0));
    r.alias(g, "AddFontResourceExW", g, "AddFontResourceExA");
    r.add(g, "RemoveFontResourceA", |_c| ok(1, 0));
    r.alias(g, "RemoveFontResourceW", g, "RemoveFontResourceA");
    r.add(g, "AddFontMemResourceEx", |_c| ok(4, 0));
    r.add(g, "RemoveFontMemResourceEx", |_c| ok(1, 0));
    r.add(g, "GetFontData", |_c| ok(5, GDI_ERROR));
    r.add(g, "GetFontUnicodeRanges", |_c| ok(2, 0));
    r.add(g, "GetTextCharacterExtra", |_c| ok(1, 0));
    r.add(g, "SetMiterLimit", |_c| ok(3, 1));
    r.add(g, "BeginPath", |_c| ok(1, 1));
    r.add(g, "EndPath", |_c| ok(1, 1));
    r.add(g, "CloseFigure", |_c| ok(1, 1));
    r.add(g, "FillPath", |_c| ok(1, 1));
    r.add(g, "StrokePath", |_c| ok(1, 1));
    r.add(g, "StrokeAndFillPath", |_c| ok(1, 1));
    r.add(g, "PathToRegion", |c| {
        let h = c.p.new_gdi(GdiObject::Region);
        ok(1, h)
    });
    r.add(g, "SelectClipPath", |_c| ok(2, 1));
    r.add(g, "AbortPath", |_c| ok(1, 1));
    r.add(g, "GetPath", |_c| ok(4, 0));
    r.add(g, "WidenPath", |_c| ok(1, 1));
    r.add(g, "FlattenPath", |_c| ok(1, 1));
    r.add(g, "CreateScalableFontResourceA", |_c| ok(4, 0));
    r.add(g, "GetArcDirection", |_c| ok(1, 1));
    r.add(g, "SetArcDirection", |_c| ok(2, 1));
    r.add(g, "SwapBuffers", |_c| ok(1, 1));
    r.add(g, "ChoosePixelFormat", |_c| ok(2, 0));
    r.add(g, "SetPixelFormat", |_c| ok(3, 0));
    r.add(g, "DescribePixelFormat", |_c| ok(4, 0));
    r.add(g, "GetPixelFormat", |_c| ok(1, 0));
    r.add(g, "GetCharWidthFloatA", |_c| ok(4, 0));
    r.add(g, "GetAspectRatioFilterEx", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u64(out, 0);
        ok(2, 1)
    });
    r.add(g, "SetDeviceGammaRamp", |_c| ok(2, 0));
    r.add(g, "GetDeviceGammaRamp", |_c| ok(2, 0));
}
