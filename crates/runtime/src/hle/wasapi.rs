//! Windows audio sessions (WASAPI), as GameMaker Studio 2's runner plays its sound: the
//! default render endpoint and shared-mode streams that the guest fills through
//! IAudioRenderClient. A stream's buffer is a ring in guest memory, mixed into the
//! process's output (`audio::mix_source`) as the virtual clock passes; an event-driven
//! stream's event is set every device period, as the audio engine does.
//!
//! The interfaces are registered the first time the enumerator is made, so no other
//! game's stub addresses move.

use super::com::{self, Interface, Method, field, format_iid, out, out_ptr, parse_iid, set_field, E_NOINTERFACE, S_OK};
use super::{ApiFn, ApiResult, Ctx};
use crate::fasthash::HashMap;
use crate::mem::Guest;
use crate::process::Handle;

pub const CLSID_MMDEVICE_ENUMERATOR: &str = "{BCDE0395-E52F-467C-8E3D-C4579291692E}";
const E_NOTFOUND: u32 = 0x8007_0490;
const E_POINTER: u32 = 0x8000_4003;
const AUDCLNT_E_NOT_INITIALIZED: u32 = 0x8889_0001;
const AUDCLNT_E_ALREADY_INITIALIZED: u32 = 0x8889_0002;
const AUDCLNT_E_UNSUPPORTED_FORMAT: u32 = 0x8889_0008;
const AUDCLNT_E_BUFFER_TOO_LARGE: u32 = 0x8889_0006;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 2;
/// The engine's period (100 ns units): 10 ms.
const PERIOD_HNS: u64 = 100_000;
const MIX_RATE: u32 = 48_000;
const KSDATAFORMAT_SUBTYPE_PCM: &str = "{00000001-0000-0010-8000-00AA00389B71}";
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: &str = "{00000003-0000-0010-8000-00AA00389B71}";

/// A shared-mode stream (one IAudioClient).
pub struct Stream {
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub float: bool,
    pub align: u32,
    /// The ring: `frames` frames, then as many again (GetBuffer hands out contiguous
    /// memory: what goes past the end is copied to the start at ReleaseBuffer).
    pub ring: u32,
    pub frames: u32,
    /// Frames written and played so far (played is fractional: resampled).
    pub written: u64,
    pub played: f64,
    /// Frames GetBuffer handed out.
    pub pending: u32,
    pub started: bool,
    pub last_us: u64,
    pub event: u32,
    /// The period count at which the event was last set.
    pub signaled_period: u64,
    pub volume: f32,
    pub mute: bool,
}

static ENUMERATOR: Interface = Interface {
    name: "IMMDeviceEnumerator",
    iids: &["{A95664D2-9614-4F35-A746-DE8DB63617E6}"],
    methods: &[
        ("EnumAudioEndpoints", 4, Some(enum_endpoints as ApiFn)),
        ("GetDefaultAudioEndpoint", 4, Some(default_endpoint as ApiFn)),
        ("GetDevice", 3, Some(get_device as ApiFn)),
        ("RegisterEndpointNotificationCallback", 2, Some(com::okn as ApiFn)),
        ("UnregisterEndpointNotificationCallback", 2, Some(com::okn as ApiFn)),
    ],
};
static COLLECTION: Interface = Interface {
    name: "IMMDeviceCollection",
    iids: &["{0BD7A1BE-7A1A-44DB-8397-CC5392387B5E}"],
    methods: &[("GetCount", 2, Some(collection_count as ApiFn)), ("Item", 3, Some(collection_item as ApiFn))],
};
static DEVICE: Interface = Interface {
    name: "IMMDevice",
    iids: &["{D666063F-1587-4E43-81F1-B948E807363F}"],
    methods: &[
        ("Activate", 5, Some(device_activate as ApiFn)),
        ("OpenPropertyStore", 3, Some(device_properties as ApiFn)),
        ("GetId", 2, Some(device_id as ApiFn)),
        ("GetState", 2, Some(device_state as ApiFn)),
    ],
};
static ENDPOINT: Interface = Interface {
    name: "IMMEndpoint",
    iids: &["{1BE09788-6894-4089-8586-9A2A6C265AC5}"],
    methods: &[("GetDataFlow", 2, Some(com::out_zero as ApiFn))],
};
static PROPERTIES: Interface = Interface {
    name: "IPropertyStore",
    iids: &["{886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99}"],
    methods: &[
        ("GetCount", 2, Some(com::out_zero as ApiFn)),
        ("GetAt", 3, Some(com::notimpl as ApiFn)),
        ("GetValue", 3, Some(property_value as ApiFn)),
        ("SetValue", 3, Some(com::notimpl as ApiFn)),
        ("Commit", 1, Some(com::okn as ApiFn)),
    ],
};
const CLIENT_METHODS: &[Method] = &[
    // (stdcall slots: the two REFERENCE_TIMEs take two each)
    ("Initialize", 9, Some(client_initialize as ApiFn)),
    ("GetBufferSize", 2, Some(client_buffer_size as ApiFn)),
    ("GetStreamLatency", 2, Some(client_latency as ApiFn)),
    ("GetCurrentPadding", 2, Some(client_padding as ApiFn)),
    ("IsFormatSupported", 4, Some(client_format_supported as ApiFn)),
    ("GetMixFormat", 2, Some(client_mix_format as ApiFn)),
    ("GetDevicePeriod", 3, Some(client_device_period as ApiFn)),
    ("Start", 1, Some(client_start as ApiFn)),
    ("Stop", 1, Some(client_stop as ApiFn)),
    ("Reset", 1, Some(client_reset as ApiFn)),
    ("SetEventHandle", 2, Some(client_set_event as ApiFn)),
    ("GetService", 3, Some(client_service as ApiFn)),
    // IAudioClient2
    ("IsOffloadCapable", 3, Some(client_not_offload as ApiFn)),
    ("SetClientProperties", 2, Some(com::okn as ApiFn)),
    ("GetBufferSizeLimits", 5, Some(com::notimpl as ApiFn)),
];
static CLIENT: Interface = Interface {
    name: "IAudioClient",
    iids: &["{1CB9AD4C-DBFA-4C32-B178-C2F568A703B2}", "{726778CD-F60A-4EDA-82DE-E47610CD78AA}"],
    methods: CLIENT_METHODS,
};
static RENDER: Interface = Interface {
    name: "IAudioRenderClient",
    iids: &["{F294ACFC-3146-4483-A7BF-ADDCA7C260E2}"],
    methods: &[("GetBuffer", 3, Some(render_get_buffer as ApiFn)), ("ReleaseBuffer", 3, Some(render_release_buffer as ApiFn))],
};
static CLOCK: Interface = Interface {
    name: "IAudioClock",
    iids: &["{CD63314F-3FBA-4A1B-812C-EF96358728E7}"],
    methods: &[
        ("GetFrequency", 2, Some(clock_frequency as ApiFn)),
        ("GetPosition", 3, Some(clock_position as ApiFn)),
        ("GetCharacteristics", 2, Some(com::out_zero as ApiFn)),
    ],
};
static VOLUME: Interface = Interface {
    name: "ISimpleAudioVolume",
    iids: &["{87CE5498-68D6-44E5-9215-6DA47EF883D8}"],
    methods: &[
        ("SetMasterVolume", 3, Some(volume_set as ApiFn)),
        ("GetMasterVolume", 2, Some(volume_get as ApiFn)),
        ("SetMute", 3, Some(mute_set as ApiFn)),
        ("GetMute", 2, Some(mute_get as ApiFn)),
    ],
};

static ENUMERATOR_CLASS: [&Interface; 1] = [&ENUMERATOR];
static COLLECTION_CLASS: [&Interface; 1] = [&COLLECTION];
static DEVICE_CLASS: [&Interface; 2] = [&DEVICE, &ENDPOINT];
static PROPERTIES_CLASS: [&Interface; 1] = [&PROPERTIES];
static CLIENT_CLASS: [&Interface; 1] = [&CLIENT];
static RENDER_CLASS: [&Interface; 1] = [&RENDER];
static CLOCK_CLASS: [&Interface; 1] = [&CLOCK];
static VOLUME_CLASS: [&Interface; 1] = [&VOLUME];

fn ensure_registered(c: &mut Ctx) {
    if c.p.registry.lookup(com::DLL, "IMMDeviceEnumerator::QueryInterface").is_some() {
        return;
    }
    for i in [&ENUMERATOR, &COLLECTION, &DEVICE, &ENDPOINT, &PROPERTIES, &CLIENT, &RENDER, &CLOCK, &VOLUME] {
        com::register_interface(&mut c.p.registry, i);
    }
}

/// CoCreateInstance of the MMDevice enumerator; None for other classes.
pub fn create_instance(c: &mut Ctx, clsid: &str, iid_p: u32, ppv: u32) -> Option<u32> {
    if !clsid.eq_ignore_ascii_case(CLSID_MMDEVICE_ENUMERATOR) {
        return None;
    }
    ensure_registered(c);
    let want = read_iid(c, iid_p);
    if want != "{00000000-0000-0000-C000-000000000046}" && !ENUMERATOR.iids.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        out_ptr(c, ppv, 0);
        return Some(E_NOINTERFACE);
    }
    let e = com::create(c, "MMDeviceEnumerator", &ENUMERATOR_CLASS, HashMap::default());
    out_ptr(c, ppv, e);
    Some(S_OK)
}

fn read_iid(c: &Ctx, p: u32) -> String {
    let mut g = [0u8; 16];
    let _ = c.host.read(p, &mut g);
    format_iid(&g)
}

fn ret(c: &Ctx, v: u32) -> ApiResult {
    com::ret(c, v)
}

/// The one render endpoint (made at first use).
fn device(c: &mut Ctx) -> u32 {
    if let Some((&d, _)) = c.p.d3d_objects.iter().find(|(_, o)| o.kind == "MMDevice") {
        return d;
    }
    com::create(c, "MMDevice", &DEVICE_CLASS, HashMap::default())
}

// ---- IMMDeviceEnumerator / IMMDeviceCollection / IMMDevice ---------------------------------

/// EDataFlow 0 is eRender, 2 eAll; DEVICE_STATE_ACTIVE is 1.
fn enum_endpoints(c: &mut Ctx) -> ApiResult {
    let (flow, mask, pp) = (c.arg(1), c.arg(2), c.arg(3));
    let n = (flow != 1 && mask & 1 != 0) as u32;
    let coll = com::create(c, "MMDeviceCollection", &COLLECTION_CLASS, HashMap::from_iter([("count", n)]));
    out_ptr(c, pp, coll);
    ret(c, S_OK)
}

fn default_endpoint(c: &mut Ctx) -> ApiResult {
    let (flow, pp) = (c.arg(1), c.arg(3));
    if pp == 0 {
        return ret(c, E_POINTER);
    }
    // no capture devices
    if flow == 1 {
        out_ptr(c, pp, 0);
        return ret(c, E_NOTFOUND);
    }
    let d = device(c);
    out_ptr(c, pp, d);
    ret(c, S_OK)
}

fn get_device(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(2);
    let d = device(c);
    out_ptr(c, pp, d);
    ret(c, S_OK)
}

fn collection_count(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    let n = field(c, t, "count");
    out(c, p, n);
    ret(c, S_OK)
}

fn collection_item(c: &mut Ctx) -> ApiResult {
    let (t, i, pp) = (c.arg(0), c.arg(1), c.arg(2));
    if i >= field(c, t, "count") {
        out_ptr(c, pp, 0);
        return ret(c, E_INVALIDARG);
    }
    let d = device(c);
    out_ptr(c, pp, d);
    ret(c, S_OK)
}

const E_INVALIDARG: u32 = 0x8007_0057;

/// Activate(iid, clsctx, params, ppv): an audio client (the volume and clock interfaces
/// come from its GetService).
fn device_activate(c: &mut Ctx) -> ApiResult {
    let (iid, pp) = (c.arg(1), c.arg(4));
    let want = read_iid(c, iid);
    if !CLIENT.iids.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        c.p.log(format!("wasapi: IMMDevice::Activate({want}) not supported"));
        out_ptr(c, pp, 0);
        return ret(c, E_NOINTERFACE);
    }
    let cl = com::create(c, "AudioClient", &CLIENT_CLASS, HashMap::default());
    out_ptr(c, pp, cl);
    ret(c, S_OK)
}

fn device_properties(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(2);
    let s = com::create(c, "PropertyStore", &PROPERTIES_CLASS, HashMap::default());
    out_ptr(c, pp, s);
    ret(c, S_OK)
}

/// A PROPVARIANT of VT_EMPTY for every key.
fn property_value(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    if p != 0 {
        let _ = c.host.fill(p, if c.x64() { 24 } else { 16 }, 0);
    }
    ret(c, S_OK)
}

fn device_id(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let id = "{0.0.0.00000000}.{6d4b5a10-0000-4000-8000-676d72656370}";
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let s = c.p.heap_alloc(g, heap, 2 * id.len() as u32 + 2, true);
    let _ = c.host.write_wstr(s, id);
    out_ptr(c, pp, s);
    ret(c, S_OK)
}

fn device_state(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    out(c, p, 1); // DEVICE_STATE_ACTIVE
    ret(c, S_OK)
}

// ---- IAudioClient -----------------------------------------------------------------------------

/// Channels, rate, bits and float of a WAVEFORMATEX(TENSIBLE) we can mix: PCM of 8, 16,
/// 24 or 32 bits or 32-bit float, one or two channels or more (the first two play).
fn read_format(c: &Ctx, p: u32) -> Option<(u16, u32, u16, bool)> {
    let tag = c.host.read_u16(p).ok()?;
    let channels = c.host.read_u16(p + 2).ok()?;
    let rate = c.host.read_u32(p + 4).ok()?;
    let bits = c.host.read_u16(p + 14).ok()?;
    let float = match tag {
        1 => false,
        3 => true,
        0xFFFE => {
            let mut g = [0u8; 16];
            let _ = c.host.read(p + 24, &mut g);
            let sub = format_iid(&g);
            if sub.eq_ignore_ascii_case(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
                true
            } else if sub.eq_ignore_ascii_case(KSDATAFORMAT_SUBTYPE_PCM) {
                false
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let ok = channels >= 1 && channels <= 8 && (8_000..=192_000).contains(&rate) && if float { bits == 32 } else { matches!(bits, 8 | 16 | 24 | 32) };
    ok.then_some((channels, rate, bits, float))
}

/// A REFERENCE_TIME argument (by value: one slot in Win64, two in 32-bit stdcall,
/// which `index32` counts in).
fn arg_i64(c: &Ctx, index64: u32, index32: u32) -> i64 {
    if c.x64() { c.arg64(index64) as i64 } else { (c.arg(index32) as u64 | (c.arg(index32 + 1) as u64) << 32) as i64 }
}

/// Initialize(shareMode, streamFlags, hnsBufferDuration, hnsPeriodicity, pFormat,
/// AudioSessionGuid).
fn client_initialize(c: &mut Ctx) -> ApiResult {
    let t = c.arg(0);
    let (mode, flags) = (c.arg(1), c.arg(2));
    let duration = arg_i64(c, 3, 3);
    let fmt = if c.x64() { c.arg(5) } else { c.arg(7) };
    if c.p.wasapi.contains_key(&t) {
        return ret(c, AUDCLNT_E_ALREADY_INITIALIZED);
    }
    let Some((channels, rate, bits, float)) = read_format(c, fmt) else { return ret(c, AUDCLNT_E_UNSUPPORTED_FORMAT) };
    if mode != 0 {
        c.p.log("wasapi: exclusive mode taken as shared".into());
    }
    let align = channels as u32 * bits as u32 / 8;
    // at least two periods (shared mode rounds the buffer up), at most two seconds
    let hns = (duration.max(0) as u64).max(2 * PERIOD_HNS);
    let frames = (hns * rate as u64).div_ceil(10_000_000) as u32;
    if frames > 2 * rate {
        return ret(c, AUDCLNT_E_BUFFER_TOO_LARGE);
    }
    let size = (2 * frames * align).div_ceil(0x1000) * 0x1000;
    let Some(ring) = c.p.vm.alloc(0, size, true, crate::mem::PAGE_READWRITE, false, "wasapi buffer") else { return ret(c, 0x8007_000E) };
    let _ = c.host.ensure_mapped(ring, size);
    let _ = c.host.fill(ring, size, 0);
    let s = Stream {
        channels,
        rate,
        bits,
        float,
        align,
        ring,
        frames,
        written: 0,
        played: 0.0,
        pending: 0,
        started: false,
        last_us: c.p.clock_us,
        event: 0,
        signaled_period: 0,
        volume: 1.0,
        mute: false,
    };
    c.p.wasapi.insert(t, s);
    set_field(c, t, "event_mode", (flags & AUDCLNT_STREAMFLAGS_EVENTCALLBACK != 0) as u32);
    c.trace(format!("({channels}ch {rate}Hz {bits}bit{}, {frames} frames, flags {flags:#x})", if float { " float" } else { "" }));
    ret(c, S_OK)
}

fn with_stream<R>(c: &mut Ctx, t: u32, f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
    let now = c.p.clock_us;
    let p = &mut *c.p;
    let s = p.wasapi.get_mut(&t)?;
    advance(s, now, &*c.host, &mut p.mixer);
    Some(f(s))
}

fn client_buffer_size(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    match c.p.wasapi.get(&t).map(|s| s.frames) {
        Some(n) => {
            out(c, p, n);
            ret(c, S_OK)
        }
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

fn write_hns(c: &mut Ctx, p: u32, v: u64) {
    if p != 0 {
        let _ = c.host.write_u64(p, v);
    }
}

fn client_latency(c: &mut Ctx) -> ApiResult {
    let p = c.arg(1);
    write_hns(c, p, PERIOD_HNS);
    ret(c, S_OK)
}

fn client_padding(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    match with_stream(c, t, |s| padding(s)) {
        Some(n) => {
            out(c, p, n);
            ret(c, S_OK)
        }
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

fn padding(s: &Stream) -> u32 {
    (s.written as f64 - s.played).max(0.0).ceil() as u32
}

/// IsFormatSupported(mode, format, ppClosest): what `read_format` takes.
fn client_format_supported(c: &mut Ctx) -> ApiResult {
    let (fmt, pp) = (c.arg(2), c.arg(3));
    out_ptr(c, pp, 0);
    ret(c, if read_format(c, fmt).is_some() { S_OK } else { AUDCLNT_E_UNSUPPORTED_FORMAT })
}

/// GetMixFormat: 48 kHz stereo 32-bit float, a WAVEFORMATEXTENSIBLE from CoTaskMemAlloc.
fn client_mix_format(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let f = c.p.heap_alloc(g, heap, 40, true);
    let _ = c.host.write_u16(f, 0xFFFE);
    let _ = c.host.write_u16(f + 2, 2);
    let _ = c.host.write_u32(f + 4, MIX_RATE);
    let _ = c.host.write_u32(f + 8, MIX_RATE * 8);
    let _ = c.host.write_u16(f + 12, 8);
    let _ = c.host.write_u16(f + 14, 32);
    let _ = c.host.write_u16(f + 16, 22);
    let _ = c.host.write_u16(f + 18, 32); // wValidBitsPerSample
    let _ = c.host.write_u32(f + 20, 3); // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
    let _ = c.host.write(f + 24, &parse_iid(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT));
    out_ptr(c, pp, f);
    ret(c, S_OK)
}

fn client_device_period(c: &mut Ctx) -> ApiResult {
    let (pd, pm) = (c.arg(1), c.arg(2));
    write_hns(c, pd, PERIOD_HNS);
    write_hns(c, pm, PERIOD_HNS / 3);
    ret(c, S_OK)
}

fn client_start(c: &mut Ctx) -> ApiResult {
    let t = c.arg(0);
    let now = c.p.clock_us;
    let Some(ev) = with_stream(c, t, |s| {
        s.started = true;
        s.last_us = now;
        s.event
    }) else {
        return ret(c, AUDCLNT_E_NOT_INITIALIZED);
    };
    // room in the buffer from the start
    signal(c, ev);
    ret(c, S_OK)
}

fn client_stop(c: &mut Ctx) -> ApiResult {
    let t = c.arg(0);
    match with_stream(c, t, |s| s.started = false) {
        Some(()) => ret(c, S_OK),
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

fn client_reset(c: &mut Ctx) -> ApiResult {
    let t = c.arg(0);
    match with_stream(c, t, |s| {
        s.written = 0;
        s.played = 0.0;
        s.signaled_period = 0;
    }) {
        Some(()) => ret(c, S_OK),
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

fn client_set_event(c: &mut Ctx) -> ApiResult {
    let (t, h) = (c.arg(0), c.arg(1));
    match c.p.wasapi.get_mut(&t) {
        Some(s) => {
            s.event = h;
            ret(c, S_OK)
        }
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

/// GetService(riid, ppv): the render client, clock or volume of this stream.
fn client_service(c: &mut Ctx) -> ApiResult {
    let (t, iid, pp) = (c.arg(0), c.arg(1), c.arg(2));
    let want = read_iid(c, iid);
    if !c.p.wasapi.contains_key(&t) {
        out_ptr(c, pp, 0);
        return ret(c, AUDCLNT_E_NOT_INITIALIZED);
    }
    let class: &'static [&'static Interface] = if RENDER.iids.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        &RENDER_CLASS
    } else if CLOCK.iids.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        &CLOCK_CLASS
    } else if VOLUME.iids.iter().any(|i| i.eq_ignore_ascii_case(&want)) {
        &VOLUME_CLASS
    } else {
        c.p.log(format!("wasapi: IAudioClient::GetService({want}) not supported"));
        out_ptr(c, pp, 0);
        return ret(c, E_NOINTERFACE);
    };
    let o = com::create(c, class[0].name, class, HashMap::from_iter([("client", t)]));
    out_ptr(c, pp, o);
    ret(c, S_OK)
}

fn client_not_offload(c: &mut Ctx) -> ApiResult {
    let p = c.arg(2);
    out(c, p, 0);
    ret(c, S_OK)
}

// ---- IAudioRenderClient, IAudioClock, ISimpleAudioVolume -----------------------------------

fn client_of(c: &Ctx, t: u32) -> u32 {
    field(c, t, "client")
}

/// GetBuffer(frames, ppData): room after what is queued, contiguous.
fn render_get_buffer(c: &mut Ctx) -> ApiResult {
    let (t, n, pp) = (c.arg(0), c.arg(1), c.arg(2));
    let cl = client_of(c, t);
    let r = with_stream(c, cl, |s| {
        if n > s.frames - padding(s).min(s.frames) {
            return Err(AUDCLNT_E_BUFFER_TOO_LARGE);
        }
        s.pending = n;
        Ok(s.ring + (s.written % s.frames as u64) as u32 * s.align)
    });
    match r {
        Some(Ok(at)) => {
            out_ptr(c, pp, at);
            ret(c, S_OK)
        }
        Some(Err(e)) => {
            out_ptr(c, pp, 0);
            ret(c, e)
        }
        None => ret(c, AUDCLNT_E_NOT_INITIALIZED),
    }
}

/// ReleaseBuffer(frames, flags): the frames written join the queue.
fn render_release_buffer(c: &mut Ctx) -> ApiResult {
    let (t, n, flags) = (c.arg(0), c.arg(1), c.arg(2));
    let cl = client_of(c, t);
    let Some((ring, frames, align, at)) = c.p.wasapi.get(&cl).map(|s| (s.ring, s.frames, s.align, (s.written % s.frames as u64) as u32)) else {
        return ret(c, AUDCLNT_E_NOT_INITIALIZED);
    };
    let n = n.min(frames);
    if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
        let _ = c.host.fill(ring + at * align, n * align, 0);
    }
    // what went past the end belongs at the start
    if at + n > frames {
        let over = at + n - frames;
        let _ = c.host.copy_within(ring, ring + frames * align, over * align);
    }
    with_stream(c, cl, |s| {
        s.written += n as u64;
        s.pending = 0;
    });
    ret(c, S_OK)
}

/// The clock counts bytes (frequency: bytes per second).
fn clock_frequency(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    let cl = client_of(c, t);
    let f = c.p.wasapi.get(&cl).map(|s| s.rate as u64 * s.align as u64).unwrap_or(MIX_RATE as u64 * 8);
    if p != 0 {
        let _ = c.host.write_u64(p, f);
    }
    ret(c, S_OK)
}

/// GetPosition(pu64Position, pu64QPCPosition).
fn clock_position(c: &mut Ctx) -> ApiResult {
    let (t, p, pq) = (c.arg(0), c.arg(1), c.arg(2));
    let cl = client_of(c, t);
    let pos = with_stream(c, cl, |s| s.played as u64 * s.align as u64).unwrap_or(0);
    if p != 0 {
        let _ = c.host.write_u64(p, pos);
    }
    if pq != 0 {
        // QueryPerformanceCounter's 10 MHz
        let q = c.p.clock_us * 10;
        let _ = c.host.write_u64(pq, q);
    }
    ret(c, S_OK)
}

fn volume_set(c: &mut Ctx) -> ApiResult {
    let t = c.arg(0);
    let v = c.arg_f32(1).clamp(0.0, 1.0);
    let cl = client_of(c, t);
    with_stream(c, cl, |s| s.volume = v);
    ret(c, S_OK)
}

fn volume_get(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    let cl = client_of(c, t);
    let v = c.p.wasapi.get(&cl).map(|s| s.volume).unwrap_or(1.0);
    out(c, p, v.to_bits());
    ret(c, S_OK)
}

fn mute_set(c: &mut Ctx) -> ApiResult {
    let (t, m) = (c.arg(0), c.arg(1) != 0);
    let cl = client_of(c, t);
    with_stream(c, cl, |s| s.mute = m);
    ret(c, S_OK)
}

fn mute_get(c: &mut Ctx) -> ApiResult {
    let (t, p) = (c.arg(0), c.arg(1));
    let cl = client_of(c, t);
    let m = c.p.wasapi.get(&cl).map(|s| s.mute).unwrap_or(false);
    out(c, p, m as u32);
    ret(c, S_OK)
}

// ---- playing ----------------------------------------------------------------------------------

/// Reads frame `i` of the ring as stereo.
fn ring_frame(mem: &dyn Guest, s: &Stream, i: u64) -> (f32, f32) {
    let at = s.ring + (i % s.frames as u64) as u32 * s.align;
    let bytes = (s.bits / 8) as u32;
    let sample = |k: u32| -> f32 {
        let mut b = [0u8; 4];
        if mem.read(at + k * bytes, &mut b[..bytes as usize]).is_err() {
            return 0.0;
        }
        match (s.float, bytes) {
            (true, _) => f32::from_le_bytes(b),
            (false, 1) => (b[0] as f32 - 128.0) / 128.0,
            (false, 2) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (false, 3) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            _ => i32::from_le_bytes(b) as f32 / 2_147_483_648.0,
        }
    };
    let l = sample(0);
    let r = if s.channels >= 2 { sample(1) } else { l };
    (l, r)
}

/// Plays a started stream up to `now`: what is queued, at its rate; an underrun leaves
/// silence and the stream waits for more.
fn advance(s: &mut Stream, now: u64, mem: &dyn Guest, mixer: &mut super::audio::Mixer) {
    if !s.started || now <= s.last_us {
        s.last_us = s.last_us.max(now);
        return;
    }
    let queued = s.written as f64 - s.played;
    let elapsed = (now - s.last_us) as f64 * s.rate as f64 / 1e6;
    let n = queued.min(elapsed).max(0.0);
    if n > 0.0 && !s.mute && s.volume > 0.0 {
        let base = s.played;
        let first = base.floor() as u64;
        let read = |i: u64| {
            let (l, r) = ring_frame(mem, s, first + i);
            (l * s.volume, r * s.volume)
        };
        let frames = (base + n).ceil() as u64 - first;
        let src = super::audio::Source { frame: &read, frames, freq: s.rate, looping: false, volume: 0, pan: 0 };
        let t1 = s.last_us + (n * 1e6 / s.rate as f64) as u64;
        super::audio::mix_source(mixer, &src, base - first as f64, s.last_us, t1);
    }
    s.played += n;
    s.last_us = now;
}

fn signal(c: &mut Ctx, ev: u32) {
    if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&ev) {
        *signaled = true;
    }
}

/// Plays every stream to the clock and sets the events of those whose engine period
/// passed (called with DirectSound's tick).
pub fn tick(c: &mut Ctx) {
    if c.p.wasapi.is_empty() {
        return;
    }
    let now = c.p.clock_us;
    let mut fire = Vec::new();
    let p = &mut *c.p;
    for s in p.wasapi.values_mut() {
        advance(s, now, &*c.host, &mut p.mixer);
        let period = now / (PERIOD_HNS / 10);
        if s.started && s.event != 0 && period != s.signaled_period {
            s.signaled_period = period;
            fire.push(s.event);
        }
    }
    for ev in fire {
        signal(c, ev);
    }
}
