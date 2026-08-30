//! DirectSound: devices and buffers as HLE COM objects. Buffer memory lives in guest
//! memory (Lock hands out pointers into it); play cursors advance with the virtual
//! clock, so streaming code that polls GetCurrentPosition or waits on notification
//! events sees the timing of a real device. Mixing to an output is done by the host
//! (the browser's audio worklet) from `Process::sound_buffers`.

use super::com::{self, Interface, Method, okn, out, out_zero, ret, S_OK};
use super::{ApiFn, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{Handle, SoundBuffer};
use std::collections::HashMap;

pub const DSERR_INVALIDPARAM: u32 = 0x8007_0057;
pub const DSERR_INVALIDCALL: u32 = 0x8878_0032;
pub const DSERR_BUFFERLOST: u32 = 0x8878_0096;
pub const DSERR_CONTROLUNAVAIL: u32 = 0x8878_001E;

const DSBCAPS_PRIMARYBUFFER: u32 = 0x1;
const DSBPLAY_LOOPING: u32 = 0x1;
const DSBSTATUS_PLAYING: u32 = 0x1;
const DSBSTATUS_LOOPING: u32 = 0x4;

fn this(c: &Ctx) -> u32 {
    c.arg(0)
}

fn buffer_id(c: &Ctx) -> Option<u32> {
    let base = com::object_of(c, this(c))?.0;
    c.p.sound_buffers.contains_key(&base).then_some(base)
}

fn read_format(c: &Ctx, p: u32) -> (u16, u16, u32, u16, u16) {
    let tag = c.host.read_u16(p).unwrap_or(1);
    let ch = c.host.read_u16(p + 2).unwrap_or(2);
    let rate = c.host.read_u32(p + 4).unwrap_or(44100);
    let align = c.host.read_u16(p + 12).unwrap_or(4);
    let bits = c.host.read_u16(p + 14).unwrap_or(16);
    (tag, ch, rate, align, bits)
}

/// Advances a buffer's play cursor to `now_us`, mixing the samples it passes into
/// `mixer`, and records the notification events it crossed.
pub fn update_buffer(b: &mut SoundBuffer, now_us: u64, mem: &dyn Guest, mixer: &mut super::audio::Mixer) {
    let t0 = b.last_us;
    b.last_us = now_us;
    if !b.playing || b.primary || b.size == 0 || now_us <= t0 {
        return;
    }
    let align = b.block_align.max(1) as u64;
    let frames = b.size as u64 / align;
    if frames == 0 {
        return;
    }
    let start = b.cursor;
    super::audio::mix(mixer, mem, b, start, t0, now_us);
    let end = start + (now_us - t0) as f64 * b.freq as f64 / 1e6;
    let (old, np) = (start.floor() as u64 * align, end.floor() as u64 * align);
    // Notification positions crossed in (old, np].
    for &(pos, ev) in &b.notify {
        let crossed = if pos == 0xFFFF_FFFF {
            !b.looping && np >= b.size as u64
        } else {
            let p = pos as u64;
            (old < p && p <= np) || (old < p + b.size as u64 && p + b.size as u64 <= np)
        };
        if crossed {
            b.fired.push(ev);
        }
    }
    if b.looping {
        b.cursor = end % frames as f64;
    } else if end >= frames as f64 {
        b.cursor = 0.0;
        b.playing = false;
    } else {
        b.cursor = end;
    }
    b.play = (b.cursor.floor() as u64 * align) as u32;
}

/// Brings every buffer to the current time (mixing what they played) and signals the
/// notification events they crossed.
pub fn tick(c: &mut Ctx) {
    let now = c.p.clock_us;
    let mut fired = Vec::new();
    let p = &mut *c.p;
    for b in p.sound_buffers.values_mut() {
        update_buffer(b, now, &*c.host, &mut p.mixer);
        fired.append(&mut b.fired);
    }
    for ev in fired {
        if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&ev) {
            *signaled = true;
        }
    }
}

/// Hands the finished part of the mix to the host (called at each Present).
pub fn flush_audio(c: &mut Ctx) {
    tick(c);
    let now = c.p.clock_us;
    let out = c.p.mixer.drain(now);
    if !out.is_empty() {
        let rate = c.p.mixer.rate;
        c.p.audio.push(&out, rate);
    }
}

// ---- IDirectSound8 ----------------------------------------------------------------------

fn ds_create_buffer(c: &mut Ctx) -> ApiResult {
    let (desc, pp) = (c.arg(1), c.arg(2));
    let size = c.host.read_u32(desc).unwrap_or(0);
    let flags = c.host.read_u32(desc + 4).unwrap_or(0);
    let bytes = c.host.read_u32(desc + 8).unwrap_or(0);
    let fmt = c.host.read_u32(desc + 16).unwrap_or(0);
    if size < 20 || pp == 0 {
        return ret(c, DSERR_INVALIDPARAM);
    }
    let primary = flags & DSBCAPS_PRIMARYBUFFER != 0;
    let (tag, ch, rate, align, bits) = if fmt != 0 { read_format(c, fmt) } else { (1, 2, 22050, 4, 16) };
    if !primary && (fmt == 0 || bytes == 0) {
        return ret(c, DSERR_INVALIDPARAM);
    }
    let bytes = if primary { 0x8000 } else { bytes };
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let data = c.p.heap_alloc(g, heap, bytes, true);
    let obj = com::create(c, "DirectSoundBuffer", &BUFFER_CLASS, HashMap::new());
    c.p.sound_buffers.insert(
        obj,
        SoundBuffer {
            primary,
            flags,
            data,
            size: bytes,
            format_tag: tag,
            channels: ch,
            rate,
            freq: rate,
            block_align: align,
            bits,
            volume: 0,
            pan: 0,
            play: 0,
            write: 0,
            playing: false,
            looping: false,
            last_us: c.p.clock_us,
            cursor: 0.0,
            notify: Vec::new(),
            fired: Vec::new(),
        },
    );
    c.trace(format!("({bytes} bytes, {ch}ch {rate}Hz {bits}bit, flags {flags:#x}) = {obj:#x}"));
    out(c, pp, obj);
    ret(c, S_OK)
}

fn ds_get_caps(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let size = c.host.read_u32(p).unwrap_or(96).clamp(4, 96);
    let _ = c.host.fill(p + 4, size - 4, 0);
    // DSCAPS_PRIMARYMONO|STEREO|8BIT|16BIT|CONTINUOUSRATE|SECONDARYMONO|STEREO|8BIT|16BIT
    let _ = c.host.write_u32(p + 4, 0x0000_0F1F);
    let _ = c.host.write_u32(p + 8, 100);
    let _ = c.host.write_u32(p + 12, 200_000);
    ret(c, S_OK)
}

fn ds_duplicate(c: &mut Ctx) -> ApiResult {
    let (orig, pp) = (c.arg(1), c.arg(2));
    let Some(base) = com::object_of(c, orig).map(|o| o.0) else { return ret(c, DSERR_INVALIDPARAM) };
    let Some(mut b) = c.p.sound_buffers.get(&base).cloned() else { return ret(c, DSERR_INVALIDPARAM) };
    b.playing = false;
    b.play = 0;
    b.cursor = 0.0;
    b.notify.clear();
    let obj = com::create(c, "DirectSoundBuffer", &BUFFER_CLASS, HashMap::new());
    c.p.sound_buffers.insert(obj, b);
    out(c, pp, obj);
    ret(c, S_OK)
}

// ---- IDirectSoundBuffer8 -------------------------------------------------------------------

fn with_buffer<T>(c: &mut Ctx, f: impl FnOnce(&mut SoundBuffer) -> T) -> Option<T> {
    let id = buffer_id(c)?;
    let now = c.p.clock_us;
    let p = &mut *c.p;
    let b = p.sound_buffers.get_mut(&id)?;
    update_buffer(b, now, &*c.host, &mut p.mixer);
    Some(f(b))
}

fn buf_get_caps(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let Some((flags, size)) = with_buffer(c, |b| (b.flags, b.size)) else { return ret(c, DSERR_INVALIDCALL) };
    let _ = c.host.write_u32(p + 4, flags | 0x8 /* LOCSOFTWARE */);
    let _ = c.host.write_u32(p + 8, size);
    let _ = c.host.write_u32(p + 12, 0);
    let _ = c.host.write_u32(p + 16, 0);
    ret(c, S_OK)
}

fn buf_get_position(c: &mut Ctx) -> ApiResult {
    let (pp, pw) = (c.arg(1), c.arg(2));
    let Some((play, size, playing, align)) = with_buffer(c, |b| (b.play, b.size, b.playing, b.block_align as u32)) else { return ret(c, DSERR_INVALIDCALL) };
    // The write cursor runs ~15 ms ahead of the play cursor while playing.
    let lead = if playing { (c.p.sound_buffers.get(&buffer_id(c).unwrap()).map(|b| b.freq).unwrap_or(44100) * align * 15 / 1000) / align.max(1) * align.max(1) } else { 0 };
    out(c, pp, play);
    out(c, pw, if size == 0 { 0 } else { (play + lead) % size });
    ret(c, S_OK)
}

fn buf_get_format(c: &mut Ctx) -> ApiResult {
    let (p, size, pw) = (c.arg(1), c.arg(2), c.arg(3));
    let Some((tag, ch, rate, align, bits)) = with_buffer(c, |b| (b.format_tag, b.channels, b.rate, b.block_align, b.bits)) else { return ret(c, DSERR_INVALIDCALL) };
    if p != 0 && size >= 16 {
        let _ = c.host.write_u16(p, tag);
        let _ = c.host.write_u16(p + 2, ch);
        let _ = c.host.write_u32(p + 4, rate);
        let _ = c.host.write_u32(p + 8, rate * align as u32);
        let _ = c.host.write_u16(p + 12, align);
        let _ = c.host.write_u16(p + 14, bits);
        if size >= 18 {
            let _ = c.host.write_u16(p + 16, 0);
        }
    }
    out(c, pw, 18);
    ret(c, S_OK)
}

fn buf_get_status(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let Some(st) = with_buffer(c, |b| if b.playing { DSBSTATUS_PLAYING | if b.looping { DSBSTATUS_LOOPING } else { 0 } } else { 0 }) else { return ret(c, DSERR_INVALIDCALL) };
    out(c, p, st);
    ret(c, S_OK)
}

fn buf_lock(c: &mut Ctx) -> ApiResult {
    let (off, bytes, pp1, pb1, pp2, pb2, flags) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5), c.arg(6), c.arg(7));
    let Some((data, size, write)) = with_buffer(c, |b| (b.data, b.size, b.write)) else { return ret(c, DSERR_INVALIDCALL) };
    // DSBLOCK_FROMWRITECURSOR 1, DSBLOCK_ENTIREBUFFER 2
    let off = if flags & 1 != 0 { write } else { off };
    let bytes = if flags & 2 != 0 { size } else { bytes };
    if size == 0 || off >= size || bytes > size {
        return ret(c, DSERR_INVALIDPARAM);
    }
    let first = bytes.min(size - off);
    out(c, pp1, data + off);
    out(c, pb1, first);
    if first < bytes {
        out(c, pp2, data);
        out(c, pb2, bytes - first);
    } else {
        out(c, pp2, 0);
        out(c, pb2, 0);
    }
    ret(c, S_OK)
}

fn buf_play(c: &mut Ctx) -> ApiResult {
    let flags = c.arg(3);
    let now = c.p.clock_us;
    let Some(()) = with_buffer(c, |b| {
        b.playing = true;
        b.looping = flags & DSBPLAY_LOOPING != 0;
        b.last_us = now;
    }) else {
        return ret(c, DSERR_INVALIDCALL);
    };
    ret(c, S_OK)
}

fn buf_stop(c: &mut Ctx) -> ApiResult {
    let Some(()) = with_buffer(c, |b| b.playing = false) else { return ret(c, DSERR_INVALIDCALL) };
    ret(c, S_OK)
}

fn buf_set_position(c: &mut Ctx) -> ApiResult {
    let pos = c.arg(1);
    let Some(()) = with_buffer(c, |b| {
        b.play = if b.size == 0 { 0 } else { pos.min(b.size - 1) };
        b.cursor = (b.play / b.block_align.max(1) as u32) as f64;
    }) else {
        return ret(c, DSERR_INVALIDCALL);
    };
    ret(c, S_OK)
}

fn buf_set_format(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let (tag, ch, rate, align, bits) = read_format(c, p);
    let Some(()) = with_buffer(c, |b| {
        b.format_tag = tag;
        b.channels = ch;
        b.rate = rate;
        b.freq = rate;
        b.block_align = align;
        b.bits = bits;
    }) else {
        return ret(c, DSERR_INVALIDCALL);
    };
    ret(c, S_OK)
}

fn buf_set_volume(c: &mut Ctx) -> ApiResult {
    let v = c.arg_i32(1);
    let Some(()) = with_buffer(c, |b| b.volume = v.clamp(-10000, 0)) else { return ret(c, DSERR_INVALIDCALL) };
    ret(c, S_OK)
}

fn buf_set_pan(c: &mut Ctx) -> ApiResult {
    let v = c.arg_i32(1);
    let Some(()) = with_buffer(c, |b| b.pan = v.clamp(-10000, 10000)) else { return ret(c, DSERR_INVALIDCALL) };
    ret(c, S_OK)
}

fn buf_set_frequency(c: &mut Ctx) -> ApiResult {
    let v = c.arg(1);
    let Some(()) = with_buffer(c, |b| b.freq = if v == 0 { b.rate } else { v }) else { return ret(c, DSERR_INVALIDCALL) };
    ret(c, S_OK)
}

fn buf_get_volume(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let Some(v) = with_buffer(c, |b| b.volume) else { return ret(c, DSERR_INVALIDCALL) };
    out(c, p, v as u32);
    ret(c, S_OK)
}

fn buf_get_pan(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let Some(v) = with_buffer(c, |b| b.pan) else { return ret(c, DSERR_INVALIDCALL) };
    out(c, p, v as u32);
    ret(c, S_OK)
}

fn buf_get_frequency(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    let Some(v) = with_buffer(c, |b| b.freq) else { return ret(c, DSERR_INVALIDCALL) };
    out(c, p, v);
    ret(c, S_OK)
}

fn buf_unlock(c: &mut Ctx) -> ApiResult {
    let (p1, b1, p2, b2) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
    let _ = (p2, b2);
    // Remember the end of the last write for DSBLOCK_FROMWRITECURSOR.
    let _ = with_buffer(c, |b| {
        if p1 >= b.data && b1 > 0 {
            b.write = (p1 - b.data + b1) % b.size.max(1);
        }
    });
    ret(c, S_OK)
}

fn notify_set_positions(c: &mut Ctx) -> ApiResult {
    let (n, p) = (c.arg(1), c.arg(2));
    let mut list = Vec::new();
    for i in 0..n.min(1024) {
        let pos = c.host.read_u32(p + 8 * i).unwrap_or(0);
        let ev = c.host.read_u32(p + 8 * i + 4).unwrap_or(0);
        list.push((pos, ev));
    }
    c.trace(format!("({list:x?})"));
    let Some(()) = with_buffer(c, |b| b.notify = list) else { return ret(c, DSERR_INVALIDCALL) };
    ret(c, S_OK)
}

static DIRECTSOUND8: Interface = Interface {
    name: "IDirectSound8",
    iids: &["{C50A7E93-F395-4834-9EF6-7FA99DE50966}", "{279AFA83-4981-11CE-A521-0020AF0BE560}"],
    methods: &[
        ("CreateSoundBuffer", 4, Some(ds_create_buffer)),
        ("GetCaps", 2, Some(ds_get_caps)),
        ("DuplicateSoundBuffer", 3, Some(ds_duplicate)),
        ("SetCooperativeLevel", 3, Some(okn)),
        ("Compact", 1, Some(okn)),
        ("GetSpeakerConfig", 2, Some(|c: &mut Ctx| {
            let p = c.arg(1);
            out(c, p, 4); // DSSPEAKER_STEREO
            ret(c, S_OK)
        })),
        ("SetSpeakerConfig", 2, Some(okn)),
        ("Initialize", 2, Some(okn)),
        ("VerifyCertification", 2, Some(out_zero)),
    ],
};

static BUFFER_METHODS: &[Method] = &[
    ("GetCaps", 2, Some(buf_get_caps)),
    ("GetCurrentPosition", 3, Some(buf_get_position)),
    ("GetFormat", 4, Some(buf_get_format)),
    ("GetVolume", 2, Some(buf_get_volume)),
    ("GetPan", 2, Some(buf_get_pan)),
    ("GetFrequency", 2, Some(buf_get_frequency)),
    ("GetStatus", 2, Some(buf_get_status)),
    ("Initialize", 3, Some(okn)),
    ("Lock", 8, Some(buf_lock)),
    ("Play", 4, Some(buf_play)),
    ("SetCurrentPosition", 2, Some(buf_set_position)),
    ("SetFormat", 2, Some(buf_set_format)),
    ("SetVolume", 2, Some(buf_set_volume)),
    ("SetPan", 2, Some(buf_set_pan)),
    ("SetFrequency", 2, Some(buf_set_frequency)),
    ("Stop", 1, Some(buf_stop)),
    ("Unlock", 5, Some(buf_unlock)),
    ("Restore", 1, Some(okn)),
    ("SetFX", 4, Some(|c: &mut Ctx| ret(c, DSERR_CONTROLUNAVAIL))),
    ("AcquireResources", 4, Some(okn)),
    ("GetObjectInPath", 5, Some(|c: &mut Ctx| ret(c, 0x8878_1194))),
];

static SOUND_BUFFER8: Interface = Interface {
    name: "IDirectSoundBuffer8",
    iids: &["{6825A449-7524-4D82-920F-50E36AB3AB1E}", "{279AFA85-4981-11CE-A521-0020AF0BE560}"],
    methods: BUFFER_METHODS,
};

static SOUND_NOTIFY: Interface = Interface {
    name: "IDirectSoundNotify",
    iids: &["{B0210783-89CD-11D0-AF08-00A0C925CD16}"],
    methods: &[("SetNotificationPositions", 3, Some(notify_set_positions))],
};

/// IDirectSound3DBuffer: accepted and ignored (GM8 sets 3D parameters on request only).
static SOUND_3D_BUFFER: Interface = Interface {
    name: "IDirectSound3DBuffer",
    iids: &["{279AFA86-4981-11CE-A521-0020AF0BE560}"],
    methods: &[
        ("GetAllParameters", 2, Some(com::notimpl)),
        ("GetConeAngles", 3, Some(com::notimpl)),
        ("GetConeOrientation", 2, Some(com::notimpl)),
        ("GetConeOutsideVolume", 2, Some(com::notimpl)),
        ("GetMaxDistance", 2, Some(com::notimpl)),
        ("GetMinDistance", 2, Some(com::notimpl)),
        ("GetMode", 2, Some(out_zero)),
        ("GetPosition", 2, Some(com::notimpl)),
        ("GetVelocity", 2, Some(com::notimpl)),
        ("SetAllParameters", 3, Some(okn)),
        ("SetConeAngles", 4, Some(okn)),
        ("SetConeOrientation", 5, Some(okn)),
        ("SetConeOutsideVolume", 3, Some(okn)),
        ("SetMaxDistance", 3, Some(okn)),
        ("SetMinDistance", 3, Some(okn)),
        ("SetMode", 3, Some(okn)),
        ("SetPosition", 5, Some(okn)),
        ("SetVelocity", 5, Some(okn)),
    ],
};

static DEVICE_CLASS: [&Interface; 1] = [&DIRECTSOUND8];
static BUFFER_CLASS: [&Interface; 3] = [&SOUND_BUFFER8, &SOUND_NOTIFY, &SOUND_3D_BUFFER];

fn create_device(c: &mut Ctx, nargs: u32) -> ApiResult {
    let pp = c.arg(1);
    let obj = com::create(c, "DirectSound", &DEVICE_CLASS, HashMap::new());
    c.trace(format!(" = {obj:#x}"));
    out(c, pp, obj);
    Ok(Ret::stdcall(nargs, S_OK))
}

pub fn register(r: &mut Registry) {
    let d = "dsound.dll";
    r.add(d, "DirectSoundCreate", |c| create_device(c, 3));
    r.add(d, "DirectSoundCreate8", |c| create_device(c, 3));
    let enumerate: ApiFn = |c| {
        let (cb, ctx) = (c.arg(0), c.arg(1));
        // One device: the primary sound driver (NULL GUID).
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        let s = c.p.heap_alloc(g, heap, 64, true);
        let wide = c.name().ends_with('W');
        if wide {
            let _ = c.host.write_wstr(s, "Primary Sound Driver");
        } else {
            let _ = c.host.write_cstr(s, b"Primary Sound Driver");
        }
        let _ = c.call_guest(cb, &[0, s, s + 48, ctx])?;
        Ok(Ret::stdcall(2, S_OK))
    };
    r.add(d, "DirectSoundEnumerateA", enumerate);
    r.add(d, "DirectSoundEnumerateW", enumerate);
    for ifc in DEVICE_CLASS.iter().chain(BUFFER_CLASS.iter()) {
        com::register_interface(r, ifc);
    }
}
