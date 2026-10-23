//! GDI+ (gdiplus.dll's flat API) as far as GameMaker 8 extensions use it: FoxWriting, the
//! text extension of many Chinese fangames, makes fonts, measures and draws strings
//! through it. Its objects are handles that remember what layout asks of them (a font's
//! size, the strings added to a path); nothing is drawn yet, so such text stays blank
//! instead of the game stopping at the first call.

use super::{ApiResult, Ctx, Registry, Ret};
use std::collections::HashMap;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

const DLL: &str = "gdiplus.dll";
const STATUS_OK: u32 = 0;
const INVALID_PARAMETER: u32 = 2;
const NOT_IMPLEMENTED: u32 = 6;

#[derive(Default)]
pub struct State {
    next: u32,
    /// font -> em size in pixels
    fonts: HashMap<u32, f32>,
    /// path -> bounds (left, top, right, bottom) of the strings added to it
    paths: HashMap<u32, (f32, f32, f32, f32)>,
}

/// A new object, its handle written to `out` (GDI+ objects are opaque pointers to callers).
fn object(c: &mut Ctx, out: u32) -> u32 {
    let s = &mut c.p.gdiplus;
    s.next += 1;
    let h = 0x6D00_0000 | (s.next & 0xF_FFFF) << 4;
    c.write_ptr(out, h);
    h
}

fn read_f32(c: &mut Ctx, p: u32) -> f32 {
    f32::from_bits(c.host.read_u32(p).unwrap_or(0))
}

fn write_f32(c: &mut Ctx, p: u32, v: f32) {
    let _ = c.host.write_u32(p, v.to_bits());
}

/// A string argument: `len` UTF-16 units, or up to the NUL when it is -1.
fn wstr(c: &mut Ctx, p: u32, len: u32) -> Vec<u16> {
    let mut out = Vec::new();
    let max = if len == u32::MAX { 1 << 16 } else { len.min(1 << 16) };
    for i in 0..max {
        let u = c.host.read_u16(p + 2 * i).unwrap_or(0);
        if len == u32::MAX && u == 0 {
            break;
        }
        out.push(u);
    }
    out
}

/// The extent of `text` in a font of `size` pixels: wide (CJK) characters an em, others
/// a little over half, lines 1.2 em apart.
fn extent(text: &[u16], size: f32) -> (f32, f32) {
    let (mut w, mut widest, mut lines) = (0.0f32, 0.0f32, 1.0f32);
    for &u in text {
        if u == b'\n' as u16 {
            widest = widest.max(w);
            w = 0.0;
            lines += 1.0;
        } else if u != b'\r' as u16 {
            w += if u >= 0x2E80 { size } else { size * 0.55 };
        }
    }
    (widest.max(w), lines * size * 1.2)
}

fn font_size(c: &Ctx, font: u32) -> f32 {
    c.p.gdiplus.fonts.get(&font).copied().unwrap_or(12.0)
}

/// An em size in pixels from GDI+'s unit (2 pixel, 3 point, 4 inch, 5 document, 6 mm).
fn to_pixels(size: f32, unit: u32) -> f32 {
    match unit {
        3 => size * 96.0 / 72.0,
        4 => size * 96.0,
        5 => size * 96.0 / 300.0,
        6 => size * 96.0 / 25.4,
        _ => size,
    }
}

pub fn register(r: &mut Registry) {
    // (re-registering a name keeps its API id: GdipCreateFromHDC and GdipDeleteGraphics
    // were registered as constants before)
    r.add(DLL, "GdipCreateFromHDC", |c| {
        let out = c.arg(1);
        object(c, out);
        ok(2, STATUS_OK)
    });
    r.add(DLL, "GdipDeleteGraphics", |_c| ok(1, STATUS_OK));
    let d = |r: &mut Registry, name: &str, f: super::ApiFn| r.add_on_demand(DLL, name, f);

    // ---- objects that are only made and deleted ----------------------------------------
    // (name, argument count, index of the out pointer)
    macro_rules! make {
        ($($name:literal => ($n:literal, $out:literal)),* $(,)?) => {
            $(d(r, $name, |c| {
                let out = c.arg($out);
                object(c, out);
                ok($n, STATUS_OK)
            });)*
        };
    }
    make! {
        "GdipCreateFromHWND" => (2, 1),
        "GdipGetImageGraphicsContext" => (2, 1),
        "GdipStringFormatGetGenericTypographic" => (1, 0),
        "GdipStringFormatGetGenericDefault" => (1, 0),
        "GdipCreateStringFormat" => (3, 2),
        "GdipCloneStringFormat" => (2, 1),
        "GdipCreateFontFamilyFromName" => (3, 2),
        "GdipGetGenericFontFamilySansSerif" => (1, 0),
        "GdipGetGenericFontFamilySerif" => (1, 0),
        "GdipGetGenericFontFamilyMonospace" => (1, 0),
        "GdipCloneFontFamily" => (2, 1),
        "GdipGetFamily" => (2, 1),
        "GdipNewPrivateFontCollection" => (1, 0),
        "GdipNewInstalledFontCollection" => (1, 0),
        "GdipCreateSolidFill" => (2, 1),
        "GdipCloneBrush" => (2, 1),
        "GdipCreatePen1" => (4, 3),
        "GdipCreateMatrix" => (1, 0),
        "GdipCloneImage" => (2, 1),
        "GdipCreateBitmapFromHBITMAP" => (3, 2),
        "GdipCreateBitmapFromScan0" => (6, 5),
        "GdipCreateBitmapFromGraphics" => (4, 3),
    }
    // ---- settings and drawing that change nothing yet ----------------------------------
    macro_rules! accept {
        ($($name:literal => $n:literal),* $(,)?) => {
            $(d(r, $name, |_c| ok($n, STATUS_OK));)*
        };
    }
    accept! {
        "GdipDeleteStringFormat" => 1, "GdipSetStringFormatAlign" => 2, "GdipSetStringFormatLineAlign" => 2,
        "GdipSetStringFormatFlags" => 2, "GdipSetStringFormatTrimming" => 2, "GdipSetStringFormatHotkeyPrefix" => 2,
        "GdipDeleteFontFamily" => 1, "GdipDeleteFont" => 1, "GdipDeletePrivateFontCollection" => 1,
        "GdipPrivateAddFontFile" => 2, "GdipPrivateAddMemoryFont" => 3,
        "GdipDeleteBrush" => 1, "GdipDeletePen" => 1, "GdipDeleteMatrix" => 1, "GdipDisposeImage" => 1,
        "GdipGraphicsClear" => 2, "GdipSetSmoothingMode" => 2, "GdipSetTextRenderingHint" => 2,
        "GdipSetPixelOffsetMode" => 2, "GdipSetInterpolationMode" => 2, "GdipSetCompositingMode" => 2,
        "GdipSetCompositingQuality" => 2, "GdipSetPageUnit" => 2, "GdipSetTextContrast" => 2,
        "GdipResetWorldTransform" => 1, "GdipTranslateWorldTransform" => 4, "GdipScaleWorldTransform" => 4,
        "GdipRotateWorldTransform" => 3, "GdipSetWorldTransform" => 2,
        "GdipDrawString" => 7, "GdipFillPath" => 3, "GdipDrawPath" => 3,
        "GdipFillRectangle" => 6, "GdipFillRectangleI" => 6, "GdipDrawImageRectI" => 6, "GdipDrawImageI" => 4,
        "GdipFlush" => 2, "GdipReleaseDC" => 2,
    }
    // pixel access needs real bitmaps
    d(r, "GdipBitmapLockBits", |_c| ok(5, NOT_IMPLEMENTED));
    d(r, "GdipBitmapUnlockBits", |_c| ok(2, STATUS_OK));
    d(r, "GdipGetDC", |c| {
        let out = c.arg(1);
        c.write_ptr(out, 0);
        ok(2, NOT_IMPLEMENTED)
    });

    // ---- memory --------------------------------------------------------------------------
    d(r, "GdipAlloc", |c| {
        let n = c.arg(0);
        let a = c.p.heap_alloc(c.host, 0, n.max(1), true);
        ok(1, a)
    });
    d(r, "GdipFree", |c| {
        let p = c.arg(0);
        c.p.heap_free(p);
        ok(1, 0)
    });

    // ---- fonts ---------------------------------------------------------------------------
    // GdipCreateFont(family, emSize, style, unit, font*)
    d(r, "GdipCreateFont", |c| {
        let (size, unit, out) = (c.arg_f32(1), c.arg(3), c.arg(4));
        let h = object(c, out);
        c.p.gdiplus.fonts.insert(h, to_pixels(size, unit));
        ok(5, STATUS_OK)
    });
    // GdipCreateFontFromLogfontW/A(hdc, logfont*, font*): the size is lfHeight's
    for name in ["GdipCreateFontFromLogfontW", "GdipCreateFontFromLogfontA"] {
        d(r, name, |c| {
            let (lf, out) = (c.arg(1), c.arg(2));
            let height = (c.host.read_u32(lf).unwrap_or(0) as i32).unsigned_abs().max(1);
            let h = object(c, out);
            c.p.gdiplus.fonts.insert(h, height as f32);
            ok(3, STATUS_OK)
        });
    }
    d(r, "GdipCreateFontFromDC", |c| {
        let out = c.arg(1);
        let h = object(c, out);
        c.p.gdiplus.fonts.insert(h, 12.0);
        ok(2, STATUS_OK)
    });
    d(r, "GdipCloneFont", |c| {
        let (font, out) = (c.arg(0), c.arg(1));
        let size = font_size(c, font);
        let h = object(c, out);
        c.p.gdiplus.fonts.insert(h, size);
        ok(2, STATUS_OK)
    });
    d(r, "GdipGetFontSize", |c| {
        let (font, out) = (c.arg(0), c.arg(1));
        let size = font_size(c, font);
        write_f32(c, out, size);
        ok(2, STATUS_OK)
    });
    d(r, "GdipGetFontHeight", |c| {
        let (font, out) = (c.arg(0), c.arg(2));
        let size = font_size(c, font);
        write_f32(c, out, size * 1.2);
        ok(3, STATUS_OK)
    });
    d(r, "GdipGetFontUnit", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u32(out, 2);
        ok(2, STATUS_OK)
    });
    d(r, "GdipGetFontStyle", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u32(out, 0);
        ok(2, STATUS_OK)
    });
    // GdipGetLogFontW(font, graphics, LOGFONTW*): lfHeight, a normal weight, the face name
    d(r, "GdipGetLogFontW", |c| {
        let (font, out) = (c.arg(0), c.arg(2));
        let size = font_size(c, font);
        let _ = c.host.fill(out, 92, 0);
        let _ = c.host.write_u32(out, (-(size.round() as i32)) as u32);
        let _ = c.host.write_u32(out + 16, 400);
        let _ = c.host.write_wstr(out + 28, "Arial");
        ok(3, STATUS_OK)
    });
    // GdipGetFamilyName(family, WCHAR name[32], language)
    d(r, "GdipGetFamilyName", |c| {
        let out = c.arg(1);
        if out != 0 {
            let _ = c.host.write_wstr(out, "Arial");
        }
        ok(3, STATUS_OK)
    });
    d(r, "GdipGetFontCollectionFamilyCount", |c| {
        let out = c.arg(1);
        let _ = c.host.write_u32(out, 1);
        ok(2, STATUS_OK)
    });
    // GdipGetFontCollectionFamilyList(collection, numSought, families[], numFound*)
    d(r, "GdipGetFontCollectionFamilyList", |c| {
        let (sought, list, found) = (c.arg(1), c.arg(2), c.arg(3));
        if sought == 0 || list == 0 {
            return ok(4, INVALID_PARAMETER);
        }
        object(c, list);
        let _ = c.host.write_u32(found, 1);
        ok(4, STATUS_OK)
    });

    // ---- text layout -----------------------------------------------------------------------
    // GdipMeasureString(graphics, string, length, font, layoutRect*, format, boundingBox*,
    // codepointsFitted*, linesFilled*)
    d(r, "GdipMeasureString", |c| {
        let (s, len, font, layout, bounds, fitted, lines) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(6), c.arg(7), c.arg(8));
        let text = wstr(c, s, len);
        let (w, h) = extent(&text, font_size(c, font));
        let (x, y) = if layout != 0 { (read_f32(c, layout), read_f32(c, layout + 4)) } else { (0.0, 0.0) };
        if bounds != 0 {
            for (i, v) in [x, y, w, h].into_iter().enumerate() {
                write_f32(c, bounds + 4 * i as u32, v);
            }
        }
        if fitted != 0 {
            let _ = c.host.write_u32(fitted, text.len() as u32);
        }
        if lines != 0 {
            let _ = c.host.write_u32(lines, (h / (font_size(c, font) * 1.2)).round().max(1.0) as u32);
        }
        ok(9, STATUS_OK)
    });
    d(r, "GdipCreatePath", |c| {
        let out = c.arg(1);
        let h = object(c, out);
        c.p.gdiplus.paths.remove(&h);
        ok(2, STATUS_OK)
    });
    d(r, "GdipDeletePath", |c| {
        let path = c.arg(0);
        c.p.gdiplus.paths.remove(&path);
        ok(1, STATUS_OK)
    });
    d(r, "GdipResetPath", |c| {
        let path = c.arg(0);
        c.p.gdiplus.paths.remove(&path);
        ok(1, STATUS_OK)
    });
    // GdipAddPathString(path, string, length, family, style, emSize, layoutRect*, format)
    d(r, "GdipAddPathString", |c| {
        let (path, s, len, size, layout) = (c.arg(0), c.arg(1), c.arg(2), c.arg_f32(5), c.arg(6));
        let text = wstr(c, s, len);
        let (w, h) = extent(&text, size);
        let (x, y) = if layout != 0 { (read_f32(c, layout), read_f32(c, layout + 4)) } else { (0.0, 0.0) };
        let b = c.p.gdiplus.paths.entry(path).or_insert((x, y, x, y));
        *b = (b.0.min(x), b.1.min(y), b.2.max(x + w), b.3.max(y + h));
        ok(8, STATUS_OK)
    });
    // GdipGetPathWorldBounds(path, RectF*, matrix, pen)
    d(r, "GdipGetPathWorldBounds", |c| {
        let (path, out) = (c.arg(0), c.arg(1));
        let (l, t, rt, b) = c.p.gdiplus.paths.get(&path).copied().unwrap_or_default();
        for (i, v) in [l, t, rt - l, b - t].into_iter().enumerate() {
            write_f32(c, out + 4 * i as u32, v);
        }
        ok(4, STATUS_OK)
    });
    // image sizes: unknown without real bitmaps
    for name in ["GdipGetImageWidth", "GdipGetImageHeight"] {
        d(r, name, |c| {
            let out = c.arg(1);
            let _ = c.host.write_u32(out, 0);
            ok(2, STATUS_OK)
        });
    }
}
