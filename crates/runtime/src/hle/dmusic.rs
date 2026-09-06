//! DirectMusic as GameMaker 8 uses it for its sounds: a performance with one
//! standard audio path per sound, segments loaded from the sound's temp file through
//! the loader, PlaySegmentEx / StopEx / IsPlaying, path volume and the path's buffer
//! for pan. Wave segments play through the mixer on the virtual clock, like
//! DirectSound buffers; MIDI segments go through the SoundFont synthesizer (`midi`).

use super::audio::{self, Pcm, Source};
use super::com::{self, E_FAIL, Interface, S_FALSE, S_OK, create, field, okn, out, out_zero, ret, set_field};
use super::{ApiResult, Ctx, Registry};
use std::collections::HashMap;

/// A loaded sound (wave data, or a MIDI file's length).
pub struct DmSound {
    pub name: String,
    pub pcm: Pcm,
    /// MIDI files: their length in µs, and the file for the synthesizer.
    pub midi_us: Option<u64>,
    pub midi: Option<std::sync::Arc<rustysynth::MidiFile>>,
}

/// A segment playing on the performance.
pub struct DmVoice {
    pub segment: u32,
    pub state: u32,
    pub path: u32,
    pub sound: usize,
    pub primary: bool,
    /// Frame position (fractional) and the virtual time it was reached.
    pub cursor: f64,
    pub last_us: u64,
    pub started_us: u64,
    /// Plays left after the current one (u32::MAX: forever).
    pub repeats_left: u32,
    /// MIDI segments: the synthesizer playing them.
    pub midi: Option<Box<super::midi::MidiPlayer>>,
}

const DMUS_OBJ_CLASS: u32 = 0x2;
const DMUS_OBJ_FILENAME: u32 = 0x10;
const DMUS_OBJ_FULLPATH: u32 = 0x20;
const DMUS_OBJ_MEMORY: u32 = 0x400;
const DMUS_SEGF_SECONDARY: u32 = 0x80;
const DMUS_SEG_REPEAT_INFINITE: u32 = 0xFFFF_FFFF;
const DMUS_PATH_BUFFER: u32 = 0x6000;
const DMUS_E_LOADER_FAILEDOPEN: u32 = 0x8878_11D2;
const DMUS_E_UNSUPPORTED_STREAM: u32 = 0x8878_11D0;
const DMUS_E_NOT_FOUND: u32 = 0x8878_1194;
const DMUS_E_GET_UNSUPPORTED: u32 = 0x8878_1197;

const CLSID_SEGMENT: &str = "{D2AC2882-B39B-11D1-8704-00600893B1BD}";
const GUID_PERF_MASTER_VOLUME: &str = "{D2AC28B1-B39B-11D1-8704-00600893B1BD}";
const IID_SOUND_BUFFER8: &str = "{6825A449-7524-4D82-920F-50E36AB3AB1E}";
const IID_SOUND_BUFFER: &str = "{279AFA85-4981-11CE-A521-0020AF0BE560}";

fn guid_at(c: &Ctx, p: u32) -> String {
    let mut g = [0u8; 16];
    let _ = c.host.read(p, &mut g);
    com::format_iid(&g)
}

fn wstr_n(c: &Ctx, p: u32, max: u32) -> String {
    let mut units = Vec::new();
    for i in 0..max {
        let mut b = [0u8; 2];
        if c.host.read(p + 2 * i, &mut b).is_err() {
            break;
        }
        let u = u16::from_le_bytes(b);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    String::from_utf16_lossy(&units)
}

/// Music time (120 bpm, 768 ticks per quarter note) of a duration.
fn music_time(us: u64) -> u32 {
    (us * 768 * 2 / 1_000_000) as u32
}

/// MIDI file length: walks every track with the tempo map of track 0 (format 1) or
/// its own tempo changes (format 0).
pub fn midi_length_us(b: &[u8]) -> Option<u64> {
    if b.len() < 14 || &b[0..4] != b"MThd" {
        return None;
    }
    let division = u16::from_be_bytes([b[12], b[13]]);
    if division & 0x8000 != 0 || division == 0 {
        return None; // SMPTE timing: not used by GameMaker's MIDI files
    }
    let ppq = division as u64;
    let mut tempo_events: Vec<(u64, u64)> = Vec::new(); // (tick, µs per quarter)
    let mut last_tick = 0u64;
    let mut p = 8 + u32::from_be_bytes(b[4..8].try_into().ok()?) as usize;
    while p + 8 <= b.len() {
        let len = u32::from_be_bytes(b[p + 4..p + 8].try_into().ok()?) as usize;
        let track = &b[p + 8..(p + 8 + len).min(b.len())];
        p += 8 + len;
        let (mut q, mut tick, mut status) = (0usize, 0u64, 0u8);
        let var = |q: &mut usize| -> u64 {
            let mut v = 0u64;
            while *q < track.len() {
                let x = track[*q];
                *q += 1;
                v = (v << 7) | (x & 0x7f) as u64;
                if x & 0x80 == 0 {
                    break;
                }
            }
            v
        };
        while q < track.len() {
            tick += var(&mut q);
            let Some(&x) = track.get(q) else { break };
            if x & 0x80 != 0 {
                status = x;
                q += 1;
            }
            match status {
                0xFF => {
                    let ty = *track.get(q)?;
                    q += 1;
                    let n = var(&mut q) as usize;
                    if ty == 0x51 && n == 3 && q + 3 <= track.len() {
                        tempo_events.push((tick, u32::from_be_bytes([0, track[q], track[q + 1], track[q + 2]]) as u64));
                    }
                    q += n;
                    if ty == 0x2F {
                        break;
                    }
                }
                0xF0 | 0xF7 => {
                    let n = var(&mut q) as usize;
                    q += n;
                }
                s if s & 0xF0 == 0xC0 || s & 0xF0 == 0xD0 => q += 1,
                s if s >= 0x80 => q += 2,
                _ => break,
            }
        }
        last_tick = last_tick.max(tick);
    }
    tempo_events.sort();
    let (mut us, mut at, mut tempo) = (0u64, 0u64, 500_000u64);
    for (t, v) in tempo_events {
        let t = t.min(last_tick);
        us += (t - at) * tempo / ppq;
        at = t;
        tempo = v;
    }
    Some(us + (last_tick - at) * tempo / ppq)
}

/// Loads a sound for a new segment; returns the segment, or an HRESULT.
fn load_segment(c: &mut Ctx, name: String, bytes: Vec<u8>) -> Result<u32, u32> {
    let (pcm, midi_us) = if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        match audio::parse_wav(&bytes) {
            Ok(p) => (p, None),
            Err(e) => {
                c.p.log(format!("DirectMusic: {name}: {e}"));
                return Err(DMUS_E_UNSUPPORTED_STREAM);
            }
        }
    } else if let Some(us) = midi_length_us(&bytes) {
        (Pcm::default(), Some(us))
    } else {
        c.p.log(format!("DirectMusic: {name}: unsupported content"));
        return Err(DMUS_E_UNSUPPORTED_STREAM);
    };
    let us = midi_us.unwrap_or_else(|| if pcm.rate == 0 { 0 } else { pcm.frames() * 1_000_000 / pcm.rate as u64 });
    let midi = midi_us.and_then(|_| super::midi::parse(&bytes));
    c.p.dm_sounds.push(DmSound { name, pcm, midi_us, midi });
    let sound = c.p.dm_sounds.len() as u32; // 1-based in the object's fields
    Ok(create(c, "DirectMusicSegment", &SEGMENT_CLASS, HashMap::from([("sound", sound), ("repeats", 0), ("length", music_time(us))])))
}

/// Hands out `obj` through `ppv` as the interface `iid` asks for.
fn give(c: &mut Ctx, obj: u32, iid_p: u32, ppv: u32) -> u32 {
    let iid = guid_at(c, iid_p);
    let ifaces = c.p.com_classes.get(&obj).copied().unwrap_or(&[]);
    match ifaces.iter().position(|i| i.iids.iter().any(|s| s.eq_ignore_ascii_case(&iid))) {
        Some(k) => {
            out(c, ppv, obj + 4 * k as u32);
            S_OK
        }
        None => {
            c.p.log(format!("DirectMusic: interface {iid} not supported"));
            out(c, ppv, 0);
            com::E_NOINTERFACE
        }
    }
}

fn read_file(c: &mut Ctx, path: &str) -> Option<Vec<u8>> {
    let key = c.p.vpath(path);
    c.p.fs.read_all(&key)
}

// ---- IDirectMusicLoader8 -----------------------------------------------------------------

fn loader_get_object(c: &mut Ctx) -> ApiResult {
    let (this, desc, iid, ppv) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
    let valid = c.host.read_u32(desc + 4).unwrap_or(0);
    if valid & DMUS_OBJ_CLASS != 0 && !guid_at(c, desc + 24).eq_ignore_ascii_case(CLSID_SEGMENT) {
        c.p.log(format!("DirectMusic: loader asked for class {}", guid_at(c, desc + 24)));
        return ret(c, E_FAIL);
    }
    let (name, bytes) = if valid & DMUS_OBJ_MEMORY != 0 {
        let len = c.host.read_u32(desc + 832).unwrap_or(0);
        let ptr = c.host.read_u32(desc + 840).unwrap_or(0);
        let mut b = vec![0u8; len as usize];
        let _ = c.host.read(ptr, &mut b);
        ("<memory>".to_string(), Some(b))
    } else if valid & DMUS_OBJ_FILENAME != 0 {
        let mut name = wstr_n(c, desc + 312, 260);
        if valid & DMUS_OBJ_FULLPATH == 0 {
            let dir = c.p.dm_search_dirs.get(&this).cloned().unwrap_or_default();
            if !dir.is_empty() {
                name = format!("{}\\{}", dir.trim_end_matches('\\'), name);
            }
        }
        let b = read_file(c, &name);
        (name, b)
    } else {
        c.p.log("DirectMusic: loader descriptor without a file or memory".to_string());
        return ret(c, E_FAIL);
    };
    let Some(bytes) = bytes else {
        return ret(c, DMUS_E_LOADER_FAILEDOPEN);
    };
    c.trace(format!("({name}, {} bytes)", bytes.len()));
    match load_segment(c, name, bytes) {
        Ok(seg) => {
            let hr = give(c, seg, iid, ppv);
            ret(c, hr)
        }
        Err(hr) => {
            out(c, ppv, 0);
            ret(c, hr)
        }
    }
}

fn loader_load_from_file(c: &mut Ctx) -> ApiResult {
    let (class, iid, path, ppv) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
    if !guid_at(c, class).eq_ignore_ascii_case(CLSID_SEGMENT) {
        c.p.log(format!("DirectMusic: LoadObjectFromFile of class {}", guid_at(c, class)));
        return ret(c, E_FAIL);
    }
    let name = wstr_n(c, path, 260);
    let Some(bytes) = read_file(c, &name) else {
        return ret(c, DMUS_E_LOADER_FAILEDOPEN);
    };
    match load_segment(c, name, bytes) {
        Ok(seg) => {
            let hr = give(c, seg, iid, ppv);
            ret(c, hr)
        }
        Err(hr) => ret(c, hr),
    }
}

fn loader_set_search_directory(c: &mut Ctx) -> ApiResult {
    let (this, dir) = (c.arg(0), c.arg(2));
    let d = wstr_n(c, dir, 260);
    c.p.dm_search_dirs.insert(this, d);
    ret(c, S_OK)
}

pub static LOADER8: Interface = Interface {
    name: "IDirectMusicLoader8",
    iids: &["{19E7C08C-0A44-4E6A-A116-595A7CD5DE8C}", "{2FFAACA2-5DCA-11D2-AFA6-00AA0024D8B6}"],
    methods: &[
        ("GetObject", 4, Some(loader_get_object)),
        ("SetObject", 2, Some(|c| ret(c, E_FAIL))),
        ("SetSearchDirectory", 4, Some(loader_set_search_directory)),
        ("ScanDirectory", 4, Some(okn)),
        ("CacheObject", 2, Some(okn)),
        ("ReleaseObject", 2, Some(okn)),
        ("ClearCache", 2, Some(okn)),
        ("EnableCache", 3, Some(okn)),
        ("EnumObject", 4, Some(|c| ret(c, S_FALSE))),
        ("CollectGarbage", 1, Some(okn)),
        ("ReleaseObjectByUnknown", 2, Some(okn)),
        ("LoadObjectFromFile", 5, Some(loader_load_from_file)),
    ],
};

// ---- IDirectMusicSegment8 -----------------------------------------------------------------

fn seg_get_field(key: &'static str) -> impl Fn(&mut Ctx) -> ApiResult {
    move |c| {
        let (this, p) = (c.arg(0), c.arg(1));
        let v = field(c, this, key);
        out(c, p, v);
        ret(c, S_OK)
    }
}

fn seg_set_repeats(c: &mut Ctx) -> ApiResult {
    let (this, n) = (c.arg(0), c.arg(1));
    set_field(c, this, "repeats", n);
    ret(c, S_OK)
}

fn seg_set_length(c: &mut Ctx) -> ApiResult {
    let (this, n) = (c.arg(0), c.arg(1));
    set_field(c, this, "length", n);
    ret(c, S_OK)
}

fn seg_clone(c: &mut Ctx) -> ApiResult {
    let (this, pp) = (c.arg(0), c.arg(3));
    let (sound, repeats, length) = (field(c, this, "sound"), field(c, this, "repeats"), field(c, this, "length"));
    let seg = create(c, "DirectMusicSegment", &SEGMENT_CLASS, HashMap::from([("sound", sound), ("repeats", repeats), ("length", length)]));
    out(c, pp, seg);
    ret(c, S_OK)
}

pub static SEGMENT8: Interface = Interface {
    name: "IDirectMusicSegment8",
    iids: &["{C6784488-41A3-418F-AA15-B35093BA42D4}", "{D38894D1-C052-11D2-872F-00600893B1BD}", "{F96029A2-4282-11D2-8717-00600893B1BD}"],
    methods: &[
        ("GetLength", 2, Some(|c| seg_get_field("length")(c))),
        ("SetLength", 2, Some(seg_set_length)),
        ("GetRepeats", 2, Some(|c| seg_get_field("repeats")(c))),
        ("SetRepeats", 2, Some(seg_set_repeats)),
        ("GetDefaultResolution", 2, Some(out_zero)),
        ("SetDefaultResolution", 2, Some(okn)),
        ("GetTrack", 5, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
        ("GetTrackGroup", 3, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
        ("InsertTrack", 3, Some(okn)),
        ("RemoveTrack", 2, Some(okn)),
        ("InitPlay", 4, Some(|c| ret(c, E_FAIL))),
        ("GetGraph", 2, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
        ("SetGraph", 2, Some(okn)),
        ("AddNotificationType", 2, Some(okn)),
        ("RemoveNotificationType", 2, Some(okn)),
        ("GetParam", 7, Some(|c| ret(c, DMUS_E_GET_UNSUPPORTED))),
        ("SetParam", 6, Some(okn)),
        ("Clone", 4, Some(seg_clone)),
        ("SetStartPoint", 2, Some(okn)),
        ("GetStartPoint", 2, Some(out_zero)),
        ("SetLoopPoints", 3, Some(okn)),
        ("GetLoopPoints", 3, Some(|c| {
            let (a, b) = (c.arg(1), c.arg(2));
            out(c, a, 0);
            out(c, b, 0);
            ret(c, S_OK)
        })),
        ("SetPChannelsUsed", 3, Some(okn)),
        ("SetTrackConfig", 6, Some(okn)),
        ("GetAudioPathConfig", 2, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
        ("Compose", 5, Some(|c| ret(c, E_FAIL))),
        ("Download", 2, Some(okn)),
        ("Unload", 2, Some(okn)),
    ],
};

pub static SEGMENT_CLASS: [&Interface; 1] = [&SEGMENT8];

pub static SEGMENT_STATE8: Interface = Interface {
    name: "IDirectMusicSegmentState8",
    iids: &["{A50E4730-0AE4-48A7-9839-BC04BFE07772}", "{A3AFDCC7-D3EE-11D1-BC8D-00A0C922E6EB}"],
    methods: &[
        ("GetRepeats", 2, Some(|c| seg_get_field("repeats")(c))),
        ("GetSegment", 2, Some(|c| {
            let (this, p) = (c.arg(0), c.arg(1));
            let seg = field(c, this, "segment");
            if let Some(o) = com::object_of(c, seg).and_then(|(b, _)| c.p.d3d_objects.get_mut(&b)) {
                o.refcount += 1;
            }
            out(c, p, seg);
            ret(c, S_OK)
        })),
        ("GetStartTime", 2, Some(|c| seg_get_field("start")(c))),
        ("GetSeek", 2, Some(out_zero)),
        ("GetStartPoint", 2, Some(out_zero)),
        ("SetTrackConfig", 6, Some(okn)),
        ("GetObjectInPath", 8, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
    ],
};

static SEGMENT_STATE_CLASS: [&Interface; 1] = [&SEGMENT_STATE8];

// ---- IDirectMusicAudioPath8 and its buffer ---------------------------------------------------

fn path_set_volume(c: &mut Ctx) -> ApiResult {
    let (this, v) = (c.arg(0), c.arg(1));
    set_field(c, this, "volume", (v as i32).clamp(-9600, 0) as u32);
    ret(c, S_OK)
}

fn path_get_object(c: &mut Ctx) -> ApiResult {
    let (this, stage, iid, pp) = (c.arg(0), c.arg(2), c.arg(6), c.arg(7));
    let want = guid_at(c, iid);
    if stage == DMUS_PATH_BUFFER && (want.eq_ignore_ascii_case(IID_SOUND_BUFFER8) || want.eq_ignore_ascii_case(IID_SOUND_BUFFER)) {
        let mut buf = field(c, this, "buffer");
        if buf == 0 {
            buf = create(c, "DirectMusicPathBuffer", &PATH_BUFFER_CLASS, HashMap::from([("path", this)]));
            set_field(c, this, "buffer", buf);
        } else if let Some(o) = c.p.d3d_objects.get_mut(&buf) {
            o.refcount += 1;
        }
        out(c, pp, buf);
        return ret(c, S_OK);
    }
    c.p.log(format!("DirectMusic: audio path object stage {stage:#x} {want} not available"));
    out(c, pp, 0);
    ret(c, DMUS_E_NOT_FOUND)
}

pub static AUDIOPATH8: Interface = Interface {
    name: "IDirectMusicAudioPath8",
    iids: &["{C87631F5-23BE-4986-8836-05832FCC48F9}"],
    methods: &[
        ("GetObjectInPath", 8, Some(path_get_object)),
        ("Activate", 2, Some(|c| {
            let (this, on) = (c.arg(0), c.arg(1));
            set_field(c, this, "inactive", (on == 0) as u32);
            ret(c, S_OK)
        })),
        ("SetVolume", 3, Some(path_set_volume)),
        ("ConvertPChannel", 3, Some(|c| {
            let (ch, p) = (c.arg(1), c.arg(2));
            out(c, p, ch);
            ret(c, S_OK)
        })),
    ],
};

pub static AUDIOPATH_CLASS: [&Interface; 1] = [&AUDIOPATH8];

/// The audio path's secondary buffer: volume, pan and frequency of what the path plays.
fn pb_set(key: &'static str) -> impl Fn(&mut Ctx) -> ApiResult {
    move |c| {
        let (this, v) = (c.arg(0), c.arg(1));
        let path = field(c, this, "path");
        set_field(c, path, key, v);
        ret(c, S_OK)
    }
}

fn pb_get(key: &'static str) -> impl Fn(&mut Ctx) -> ApiResult {
    move |c| {
        let (this, p) = (c.arg(0), c.arg(1));
        let path = field(c, this, "path");
        let v = field(c, path, key);
        out(c, p, v);
        ret(c, S_OK)
    }
}

static PATH_BUFFER8: Interface = Interface {
    name: "IDirectSoundBuffer8(DirectMusic)",
    iids: &["{6825A449-7524-4D82-920F-50E36AB3AB1E}", "{279AFA85-4981-11CE-A521-0020AF0BE560}"],
    methods: &[
        ("GetCaps", 2, Some(|c| ret(c, E_FAIL))),
        ("GetCurrentPosition", 3, Some(|c| ret(c, E_FAIL))),
        ("GetFormat", 4, Some(|c| ret(c, E_FAIL))),
        ("GetVolume", 2, Some(|c| pb_get("buffer_volume")(c))),
        ("GetPan", 2, Some(|c| pb_get("pan")(c))),
        ("GetFrequency", 2, Some(|c| pb_get("frequency")(c))),
        ("GetStatus", 2, Some(out_zero)),
        ("Initialize", 3, Some(okn)),
        ("Lock", 8, Some(|c| ret(c, E_FAIL))),
        ("Play", 4, Some(okn)),
        ("SetCurrentPosition", 2, Some(okn)),
        ("SetFormat", 2, Some(okn)),
        ("SetVolume", 2, Some(|c| pb_set("buffer_volume")(c))),
        ("SetPan", 2, Some(|c| pb_set("pan")(c))),
        ("SetFrequency", 2, Some(|c| pb_set("frequency")(c))),
        ("Stop", 1, Some(okn)),
        ("Unlock", 5, Some(okn)),
        ("Restore", 1, Some(okn)),
        ("SetFX", 4, Some(|c| ret(c, E_FAIL))),
        ("AcquireResources", 4, Some(okn)),
        ("GetObjectInPath", 5, Some(|c| ret(c, DMUS_E_NOT_FOUND))),
    ],
};

static PATH_BUFFER_CLASS: [&Interface; 1] = [&PATH_BUFFER8];

// ---- IDirectMusicPerformance8 -------------------------------------------------------------

fn perf_init_audio(c: &mut Ctx) -> ApiResult {
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

fn perf_get_time(c: &mut Ctx) -> ApiResult {
    let (prt, pmt) = (c.arg(1), c.arg(2));
    let us = c.p.clock_us;
    if prt != 0 {
        let _ = c.host.write_u64(prt, us * 10);
    }
    if pmt != 0 {
        let _ = c.host.write_u32(pmt, music_time(us));
    }
    ret(c, S_OK)
}

fn perf_create_standard_audio_path(c: &mut Ctx) -> ApiResult {
    let (kind, pchannels, activate, pp) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
    let path = create(c, "DirectMusicAudioPath", &AUDIOPATH_CLASS, HashMap::from([("kind", kind), ("pchannels", pchannels), ("inactive", (activate == 0) as u32)]));
    out(c, pp, path);
    ret(c, S_OK)
}

fn default_path(c: &mut Ctx) -> u32 {
    if c.p.dm_default_path == 0 {
        c.p.dm_default_path = create(c, "DirectMusicAudioPath", &AUDIOPATH_CLASS, HashMap::from([("kind", 8)]));
    }
    c.p.dm_default_path
}

/// Stops the voices `obj` (a segment, segment state or audio path; 0 = all) names.
fn stop_voices(c: &mut Ctx, obj: u32) {
    c.p.dm_voices.retain(|v| obj != 0 && v.segment != obj && v.state != obj && v.path != obj);
}

fn perf_play_segment_ex(c: &mut Ctx) -> ApiResult {
    let (seg, flags, pp_state, from, path) = (c.arg(1), c.arg(4), c.arg(7), c.arg(8), c.arg(9));
    let sound = field(c, seg, "sound") as usize;
    if sound == 0 {
        c.p.log(format!("DirectMusic: PlaySegmentEx of {seg:#x}, which is not a loaded segment"));
        return ret(c, E_FAIL);
    }
    let path = if path != 0 { path } else { default_path(c) };
    let primary = flags & DMUS_SEGF_SECONDARY == 0;
    if from != 0 {
        stop_voices(c, from);
    }
    if primary {
        c.p.dm_voices.retain(|v| !v.primary);
    }
    let now = c.p.clock_us;
    let repeats = field(c, seg, "repeats");
    let state = create(c, "DirectMusicSegmentState", &SEGMENT_STATE_CLASS, HashMap::from([("segment", seg), ("repeats", repeats), ("start", music_time(now))]));
    c.trace(format!("({}, flags {flags:#x}, repeats {repeats}, path {path:#x}) = state {state:#x}", c.p.dm_sounds[sound - 1].name));
    c.p.dm_voices.push(DmVoice { segment: seg, state, path, sound: sound - 1, primary, cursor: 0.0, last_us: now, started_us: now, repeats_left: repeats, midi: None });
    if pp_state != 0 {
        out(c, pp_state, state);
    } else if let Some(o) = c.p.d3d_objects.get_mut(&state) {
        // Nobody holds the state; the voice keeps it alive until it ends.
        o.refcount = 1;
    }
    ret(c, S_OK)
}

fn perf_play_segment(c: &mut Ctx) -> ApiResult {
    // PlaySegment(seg, flags, i64 start, pp_state): no audio path argument.
    let (seg, flags, pp_state) = (c.arg(1), c.arg(2), c.arg(5));
    let sound = field(c, seg, "sound") as usize;
    if sound == 0 {
        return ret(c, E_FAIL);
    }
    let path = default_path(c);
    let primary = flags & DMUS_SEGF_SECONDARY == 0;
    if primary {
        c.p.dm_voices.retain(|v| !v.primary);
    }
    let now = c.p.clock_us;
    let repeats = field(c, seg, "repeats");
    let state = create(c, "DirectMusicSegmentState", &SEGMENT_STATE_CLASS, HashMap::from([("segment", seg), ("repeats", repeats), ("start", music_time(now))]));
    c.p.dm_voices.push(DmVoice { segment: seg, state, path, sound: sound - 1, primary, cursor: 0.0, last_us: now, started_us: now, repeats_left: repeats, midi: None });
    out(c, pp_state, state);
    ret(c, S_OK)
}

fn perf_stop_ex(c: &mut Ctx) -> ApiResult {
    let obj = c.arg(1);
    super::dsound::tick(c);
    stop_voices(c, obj);
    ret(c, S_OK)
}

fn perf_stop(c: &mut Ctx) -> ApiResult {
    let (seg, state) = (c.arg(1), c.arg(2));
    super::dsound::tick(c);
    if seg == 0 && state == 0 {
        stop_voices(c, 0);
    } else {
        for o in [seg, state] {
            if o != 0 {
                stop_voices(c, o);
            }
        }
    }
    ret(c, S_OK)
}

fn perf_is_playing(c: &mut Ctx) -> ApiResult {
    let (seg, state) = (c.arg(1), c.arg(2));
    super::dsound::tick(c);
    let playing = c.p.dm_voices.iter().any(|v| (seg != 0 && v.segment == seg) || (state != 0 && v.state == state));
    ret(c, if playing { S_OK } else { S_FALSE })
}

fn perf_set_global_param(c: &mut Ctx) -> ApiResult {
    let (guid, data, size) = (c.arg(1), c.arg(2), c.arg(3));
    if guid_at(c, guid).eq_ignore_ascii_case(GUID_PERF_MASTER_VOLUME) && size >= 4 {
        let v = c.host.read_u32(data).unwrap_or(0) as i32;
        c.p.dm_master_volume = v.clamp(-20000, 2000);
    }
    ret(c, S_OK)
}

fn perf_get_global_param(c: &mut Ctx) -> ApiResult {
    let (guid, data, size) = (c.arg(1), c.arg(2), c.arg(3));
    if guid_at(c, guid).eq_ignore_ascii_case(GUID_PERF_MASTER_VOLUME) && size >= 4 {
        let v = c.p.dm_master_volume as u32;
        out(c, data, v);
        return ret(c, S_OK);
    }
    ret(c, E_FAIL)
}

fn perf_get_default_audio_path(c: &mut Ctx) -> ApiResult {
    let pp = c.arg(1);
    let path = default_path(c);
    if let Some(o) = c.p.d3d_objects.get_mut(&path) {
        o.refcount += 1;
    }
    out(c, pp, path);
    ret(c, S_OK)
}

fn unsupported(c: &mut Ctx) -> ApiResult {
    let n = c.name();
    c.p.log(format!("{n}: not implemented, reporting failure"));
    ret(c, E_FAIL)
}

pub static PERFORMANCE8: Interface = Interface {
    name: "IDirectMusicPerformance8",
    iids: &[
        "{679C4137-C62E-4147-B2B4-9D569ACB254C}",
        "{07D43D03-6523-11D2-871D-00600893B1BD}",
        "{6FC2CAE0-BC78-11D2-AFA6-00AA0024D8B6}",
    ],
    methods: &[
        ("Init", 4, Some(perf_init_audio)),
        ("PlaySegment", 6, Some(perf_play_segment)),
        ("Stop", 5, Some(perf_stop)),
        ("GetSegmentState", 3, Some(unsupported)),
        ("SetPrepareTime", 2, Some(okn)),
        ("GetPrepareTime", 2, Some(out_zero)),
        ("SetBumperLength", 2, Some(okn)),
        ("GetBumperLength", 2, Some(out_zero)),
        ("SendPMsg", 2, Some(unsupported)),
        ("MusicToReferenceTime", 3, Some(unsupported)),
        ("ReferenceToMusicTime", 4, Some(unsupported)),
        ("IsPlaying", 3, Some(perf_is_playing)),
        ("GetTime", 3, Some(perf_get_time)),
        ("AllocPMsg", 3, Some(unsupported)),
        ("FreePMsg", 2, Some(okn)),
        ("GetGraph", 2, Some(unsupported)),
        ("SetGraph", 2, Some(okn)),
        ("SetNotificationHandle", 4, Some(okn)),
        ("GetNotificationPMsg", 2, Some(|c| ret(c, S_FALSE))),
        ("AddNotificationType", 2, Some(okn)),
        ("RemoveNotificationType", 2, Some(okn)),
        ("AddPort", 2, Some(okn)),
        ("RemovePort", 2, Some(okn)),
        ("AssignPChannelBlock", 4, Some(okn)),
        ("AssignPChannel", 5, Some(okn)),
        ("PChannelInfo", 5, Some(unsupported)),
        ("DownloadInstrument", 9, Some(unsupported)),
        ("Invalidate", 3, Some(okn)),
        ("GetParam", 7, Some(unsupported)),
        ("SetParam", 6, Some(okn)),
        ("GetGlobalParam", 4, Some(perf_get_global_param)),
        ("SetGlobalParam", 4, Some(perf_set_global_param)),
        ("GetLatencyTime", 2, Some(perf_get_time)),
        ("GetQueueTime", 2, Some(perf_get_time)),
        ("AdjustTime", 3, Some(okn)),
        ("CloseDown", 1, Some(|c| {
            stop_voices(c, 0);
            ret(c, S_OK)
        })),
        ("GetResolvedTime", 5, Some(unsupported)),
        ("MIDIToMusic", 6, Some(unsupported)),
        ("MusicToMIDI", 6, Some(unsupported)),
        ("TimeToRhythm", 7, Some(unsupported)),
        ("RhythmToTime", 7, Some(unsupported)),
        ("InitAudio", 8, Some(perf_init_audio)),
        ("PlaySegmentEx", 10, Some(perf_play_segment_ex)),
        ("StopEx", 5, Some(perf_stop_ex)),
        ("ClonePMsg", 3, Some(unsupported)),
        ("CreateAudioPath", 4, Some(unsupported)),
        ("CreateStandardAudioPath", 5, Some(perf_create_standard_audio_path)),
        ("SetDefaultAudioPath", 2, Some(|c| {
            let p = c.arg(1);
            c.p.dm_default_path = p;
            ret(c, S_OK)
        })),
        ("GetDefaultAudioPath", 2, Some(perf_get_default_audio_path)),
        ("GetParamEx", 8, Some(unsupported)),
        ("SetParamEx", 7, Some(okn)),
    ],
};

pub static PERFORMANCE_CLASS: [&Interface; 1] = [&PERFORMANCE8];
pub static LOADER_CLASS: [&Interface; 1] = [&LOADER8];

pub fn register(r: &mut Registry) {
    for i in [&SEGMENT8, &SEGMENT_STATE8, &AUDIOPATH8, &PATH_BUFFER8] {
        com::register_interface(r, i);
    }
}

// ---- playback ----------------------------------------------------------------------------

/// Brings every playing segment to the current virtual time, mixing its sound.
pub fn tick(c: &mut Ctx) {
    let now = c.p.clock_us;
    let master = c.p.dm_master_volume;
    let mut voices = std::mem::take(&mut c.p.dm_voices);
    voices.retain_mut(|v| {
        let t0 = v.last_us;
        v.last_us = now;
        if now <= t0 {
            return true;
        }
        let sound = &c.p.dm_sounds[v.sound];
        if let Some(len) = sound.midi_us {
            // the synthesizer plays it (once the SoundFont is there); the length rules
            let file = sound.midi.clone();
            if v.midi.is_none() {
                if let (Some(file), Some(sf)) = (file, super::midi::soundfont(c)) {
                    let rate = c.p.mixer.rate;
                    v.midi = super::midi::player(&sf, &file, v.repeats_left != 0, rate).map(Box::new);
                }
            }
            if let Some(player) = v.midi.as_mut() {
                let volume = (field(c, v.path, "volume") as i32 + c.p.dm_master_volume).clamp(-10000, 0);
                if field(c, v.path, "inactive") == 0 {
                    super::midi::render(&mut c.p.mixer, player, t0, now, volume);
                }
            }
            let mut end = v.started_us + len;
            while now >= end && v.repeats_left > 0 && len > 0 {
                if v.repeats_left != DMUS_SEG_REPEAT_INFINITE {
                    v.repeats_left -= 1;
                }
                v.started_us = end;
                end += len;
            }
            return now < end;
        }
        let frames = sound.pcm.frames();
        let rate = sound.pcm.rate;
        if frames == 0 || rate == 0 {
            return false;
        }
        let path_volume = field(c, v.path, "volume") as i32;
        let buffer_volume = field(c, v.path, "buffer_volume") as i32;
        let pan = field(c, v.path, "pan") as i32;
        let freq = match field(c, v.path, "frequency") {
            0 => rate,
            f => f,
        };
        let inactive = field(c, v.path, "inactive") != 0;
        let volume = (path_volume + buffer_volume + master).clamp(-10000, 0);
        let looping = v.repeats_left != 0;
        let pcm = &c.p.dm_sounds[v.sound].pcm;
        let read = |i: u64| pcm.frame(i);
        if !inactive {
            let src = Source { frame: &read, frames, freq, looping, volume, pan };
            audio::mix_source(&mut c.p.mixer, &src, v.cursor, t0, now);
        }
        let mut end = v.cursor + (now - t0) as f64 * freq as f64 / 1e6;
        while end >= frames as f64 && v.repeats_left > 0 {
            if v.repeats_left != DMUS_SEG_REPEAT_INFINITE {
                v.repeats_left -= 1;
            }
            end -= frames as f64;
        }
        v.cursor = end;
        end < frames as f64
    });
    // Voices started while mixing (none: tick does not call the guest) keep their order.
    voices.append(&mut c.p.dm_voices);
    c.p.dm_voices = voices;
}
