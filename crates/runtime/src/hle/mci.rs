//! MCI (winmm) media playback: the command and string interfaces over one table of
//! devices. Wave, MP3 (what GameMaker's "multimedia" sounds open through MPEGVideo)
//! and MIDI elements play through the mixer on the virtual clock; positions, lengths
//! and MM_MCINOTIFY follow that clock. MIDI is silent until a synthesizer exists.

use super::audio::{self, Pcm, Source};
use super::{ApiResult, Ctx, Ret};
use std::collections::BTreeMap;
use std::rc::Rc;

const MCI_OPEN: u32 = 0x803;
const MCI_CLOSE: u32 = 0x804;
const MCI_PLAY: u32 = 0x806;
const MCI_SEEK: u32 = 0x807;
const MCI_STOP: u32 = 0x808;
const MCI_PAUSE: u32 = 0x809;
const MCI_GETDEVCAPS: u32 = 0x80B;
const MCI_SET: u32 = 0x80D;
const MCI_STATUS: u32 = 0x814;
const MCI_RESUME: u32 = 0x855;
const MCI_SETAUDIO: u32 = 0x873;

const MCI_NOTIFY: u32 = 0x1;
const MCI_FROM: u32 = 0x4;
const MCI_TO: u32 = 0x8;
const MCI_SEEK_TO_START: u32 = 0x100;
const MCI_SEEK_TO_END: u32 = 0x200;
const MCI_SET_TIME_FORMAT: u32 = 0x400;
const MCI_DGV_PLAY_REPEAT: u32 = 0x10000;
const MCI_DGV_SETAUDIO_ITEM: u32 = 0x0080_0000;
const MCI_DGV_SETAUDIO_VALUE: u32 = 0x0100_0000;
const MCI_DGV_SETAUDIO_VOLUME: u32 = 0x4002;
const MCI_ALL_DEVICE_ID: u32 = 0xFFFF;

const MCI_MODE_STOP: u32 = 525;
const MCI_MODE_PLAY: u32 = 526;
const MCI_MODE_PAUSE: u32 = 529;
const MCI_FORMAT_MILLISECONDS: u32 = 0;
const MCI_NOTIFY_SUCCESSFUL: u32 = 1;
const MCI_NOTIFY_SUPERSEDED: u32 = 2;
const MCI_NOTIFY_ABORTED: u32 = 4;
const MM_MCINOTIFY: u32 = 0x3B9;
const MCIERR_INVALID_DEVICE_ID: u32 = 257;
const MCIERR_UNRECOGNIZED_COMMAND: u32 = 261;
const MCIERR_FILE_NOT_FOUND: u32 = 275;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

/// An open device.
pub struct McDevice {
    pub element: String,
    pub alias: String,
    pub media: Rc<Media>,
    pub mode: u32,
    /// Position in source frames (fractional) and when it was last advanced.
    pub pos: f64,
    pub last_us: u64,
    /// Play stops here (frames); `repeat` starts over at the start instead.
    pub to: f64,
    pub repeat: bool,
    /// Window that gets MM_MCINOTIFY when the play ends (0: no notification).
    pub notify: u32,
    /// 0..=1000
    pub volume: u32,
    pub time_format: u32,
    /// MIDI elements: the synthesizer playing them.
    pub midi: Option<Box<super::midi::MidiPlayer>>,
}

/// Decoded media: a sound, or a MIDI file's length.
pub struct Media {
    pub pcm: Pcm,
    pub midi_us: Option<u64>,
    pub midi: Option<std::sync::Arc<rustysynth::MidiFile>>,
}

impl Media {
    /// Frame rate positions count in (MIDI: microseconds as "frames").
    fn rate(&self) -> f64 {
        if self.midi_us.is_some() { 1e6 } else { self.pcm.rate.max(1) as f64 }
    }
    fn frames(&self) -> f64 {
        match self.midi_us {
            Some(us) => us as f64,
            None => self.pcm.frames() as f64,
        }
    }
    fn ms(&self, frames: f64) -> u32 {
        (frames * 1000.0 / self.rate()).round() as u32
    }
}

/// Decodes an MP3 (or anything else the decoder recognizes).
pub fn decode_compressed(bytes: Vec<u8>) -> Result<Pcm, String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("no audio parameters")?;
    let mut rate = params.sample_rate.unwrap_or(0);
    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default()).map_err(|e| e.to_string())?;
    let id = track.id;
    let mut samples = Vec::new();
    let mut buf: Vec<f32> = Vec::new();
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let channels = decoded.spec().channels().count().max(1);
        rate = decoded.spec().rate();
        buf.resize(decoded.samples_interleaved(), 0.0);
        decoded.copy_to_slice_interleaved(&mut buf);
        for f in buf.chunks_exact(channels) {
            let l = f[0];
            let r = if channels >= 2 { f[1] } else { l };
            samples.push(l);
            samples.push(r);
        }
    }
    if rate == 0 {
        return Err("no sample rate".into());
    }
    Ok(Pcm { samples, rate })
}

/// Opens (and decodes, once per file) an element.
fn load_media(c: &mut Ctx, element: &str) -> Option<Rc<Media>> {
    let key = c.p.vpath(element);
    if let Some(m) = c.p.mci_media.get(&key) {
        return Some(m.clone());
    }
    let bytes = c.p.fs.read_all(&key)?;
    let media = if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        match audio::parse_wav(&bytes) {
            Ok(pcm) => Media { pcm, midi_us: None, midi: None },
            Err(e) => {
                c.p.log(format!("MCI: {element}: {e}"));
                Media { pcm: Pcm::default(), midi_us: Some(0), midi: None }
            }
        }
    } else if bytes.starts_with(b"MThd") || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"RMID") {
        let smf = if bytes.starts_with(b"RIFF") { bytes.get(20..).unwrap_or(&[]).to_vec() } else { bytes };
        Media { pcm: Pcm::default(), midi_us: Some(super::dmusic::midi_length_us(&smf).unwrap_or(0)), midi: super::midi::parse(&smf) }
    } else {
        match decode_compressed(bytes) {
            Ok(pcm) => Media { pcm, midi_us: None, midi: None },
            Err(e) => {
                c.p.log(format!("MCI: {element}: cannot decode ({e})"));
                Media { pcm: Pcm::default(), midi_us: Some(0), midi: None }
            }
        }
    };
    let m = Rc::new(media);
    c.p.mci_media.insert(key, m.clone());
    Some(m)
}

fn open(c: &mut Ctx, element: &str, alias: &str) -> Result<u32, u32> {
    let media = if element.is_empty() { Rc::new(Media { pcm: Pcm::default(), midi_us: Some(0), midi: None }) } else { load_media(c, element).ok_or(MCIERR_FILE_NOT_FOUND)? };
    c.p.next_mci += 1;
    let id = c.p.next_mci;
    let to = media.frames();
    let now = c.p.clock_us;
    c.p.mci.insert(id, McDevice { element: element.to_string(), alias: alias.to_ascii_lowercase(), media, mode: MCI_MODE_STOP, pos: 0.0, last_us: now, to, repeat: false, notify: 0, volume: 1000, time_format: MCI_FORMAT_MILLISECONDS, midi: None });
    c.p.log(format!("MCI open {element:?} = device {id}"));
    Ok(id)
}

/// Sends MM_MCINOTIFY for a device whose play was asked to notify.
fn notify(c: &mut Ctx, id: u32, code: u32) {
    let Some(d) = c.p.mci.get_mut(&id) else { return };
    let hwnd = std::mem::take(&mut d.notify);
    if hwnd != 0 {
        super::user32::post(c, hwnd, MM_MCINOTIFY, code, id);
    }
}

fn play(c: &mut Ctx, id: u32, from_ms: Option<u32>, to_ms: Option<u32>, repeat: bool, notify_hwnd: Option<u32>) {
    tick(c);
    if notify_hwnd.is_some() {
        // a new notifying play supersedes the old one
        notify(c, id, MCI_NOTIFY_SUPERSEDED);
    }
    let now = c.p.clock_us;
    let Some(d) = c.p.mci.get_mut(&id) else { return };
    let rate = d.media.rate();
    if let Some(f) = from_ms {
        d.pos = f as f64 * rate / 1000.0;
    }
    d.to = to_ms.map(|t| t as f64 * rate / 1000.0).unwrap_or(d.media.frames()).min(d.media.frames());
    if d.pos >= d.to && !repeat {
        d.pos = d.to;
    }
    d.repeat = repeat;
    d.mode = MCI_MODE_PLAY;
    // the synthesizer cannot seek: a MIDI play starts it over
    d.midi = None;
    d.last_us = now;
    if let Some(h) = notify_hwnd {
        d.notify = h;
    }
}

fn stop(c: &mut Ctx, id: u32, mode: u32) {
    tick(c);
    if let Some(d) = c.p.mci.get_mut(&id) {
        d.mode = mode;
    }
    if mode == MCI_MODE_STOP {
        notify(c, id, MCI_NOTIFY_ABORTED);
    }
}

fn close(c: &mut Ctx, id: u32) {
    tick(c);
    notify(c, id, MCI_NOTIFY_ABORTED);
    c.p.mci.remove(&id);
}

fn devices(c: &Ctx, id: u32) -> Vec<u32> {
    if id == MCI_ALL_DEVICE_ID { c.p.mci.keys().copied().collect() } else { vec![id] }
}

/// Plays every device up to the current virtual time and ends the plays that are done.
pub fn tick(c: &mut Ctx) {
    let now = c.p.clock_us;
    let mut ended = Vec::new();
    // MIDI devices that play get a synthesizer once the SoundFont is there.
    let need_synth = c.p.mci.values().any(|d| d.mode == MCI_MODE_PLAY && d.media.midi.is_some() && d.midi.is_none());
    let sf = if need_synth { super::midi::soundfont(c) } else { None };
    let p = &mut *c.p;
    for (&id, d) in p.mci.iter_mut() {
        let t0 = d.last_us;
        d.last_us = now;
        if d.mode != MCI_MODE_PLAY || now <= t0 {
            continue;
        }
        let rate = d.media.rate();
        let total = d.media.frames();
        let volume = if d.volume >= 1000 { 0 } else if d.volume == 0 { -10000 } else { (2000.0 * (d.volume as f64 / 1000.0).log10()) as i32 };
        if let (Some(file), None, Some(sf)) = (&d.media.midi, &d.midi, &sf) {
            d.midi = super::midi::player(sf, file, d.repeat, p.mixer.rate).map(Box::new);
        }
        if let Some(player) = d.midi.as_mut() {
            super::midi::render(&mut p.mixer, player, t0, now, volume);
        }
        if d.media.midi_us.is_none() && total > 0.0 {
            let pcm = &d.media.pcm;
            let read = |i: u64| pcm.frame(i);
            let src = Source { frame: &read, frames: d.to.ceil() as u64, freq: pcm.rate, looping: d.repeat, volume, pan: 0 };
            audio::mix_source(&mut p.mixer, &src, d.pos, t0, now);
        }
        let mut end = d.pos + (now - t0) as f64 * rate / 1e6;
        if d.repeat {
            if d.to > 0.0 {
                while end >= d.to {
                    end -= d.to;
                }
            }
            d.pos = end;
        } else if end >= d.to {
            d.pos = d.to;
            d.mode = MCI_MODE_STOP;
            ended.push(id);
        } else {
            d.pos = end;
        }
    }
    for id in ended {
        notify(c, id, MCI_NOTIFY_SUCCESSFUL);
    }
}

fn status_value(c: &Ctx, id: u32, item: u32) -> u32 {
    let Some(d) = c.p.mci.get(&id) else { return 0 };
    match item {
        1 => d.media.ms(d.media.frames()), // MCI_STATUS_LENGTH
        2 => d.media.ms(d.pos),            // MCI_STATUS_POSITION
        3 => 1,                            // MCI_STATUS_NUMBER_OF_TRACKS
        4 => d.mode,                       // MCI_STATUS_MODE
        5 => 1,                            // MCI_STATUS_READY
        6 => d.time_format,                // MCI_STATUS_TIME_FORMAT
        7 => 1,                            // MCI_STATUS_MEDIA_PRESENT
        8 => 1,                            // MCI_STATUS_CURRENT_TRACK
        0x4019 => d.volume,                // MCI_DGV_STATUS_VOLUME
        _ => 0,
    }
}

/// mciSendCommand.
pub fn command(c: &mut Ctx, wide: bool) -> ApiResult {
    let (dev, msg, flags, parms) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
    let str_at = |c: &Ctx, p: u32| if p == 0 { String::new() } else if p < 0x10000 { format!("#{p}") } else if wide { c.host.read_wstr(p).unwrap_or_default() } else { c.host.read_cstr_lossy(p) };
    let rd = |c: &Ctx, off: u32| c.host.read_u32(parms + off).unwrap_or(0);
    let callback = if flags & MCI_NOTIFY != 0 && parms != 0 { Some(rd(c, 0) & 0xFFFF) } else { None };
    let r = match msg {
        MCI_OPEN => {
            // MCI_OPEN_PARMS: callback, device id (out), type, element, alias
            let element = if flags & 0x200 != 0 { str_at(c, rd(c, 12)) } else { String::new() };
            let alias = if flags & 0x400 != 0 { str_at(c, rd(c, 16)) } else { String::new() };
            match open(c, &element, &alias) {
                Ok(id) => {
                    let _ = c.host.write_u32(parms + 4, id);
                    0
                }
                Err(e) => e,
            }
        }
        _ if !c.p.mci.contains_key(&dev) && dev != MCI_ALL_DEVICE_ID => MCIERR_INVALID_DEVICE_ID,
        MCI_CLOSE => {
            for id in devices(c, dev) {
                close(c, id);
            }
            0
        }
        MCI_PLAY => {
            // MCI_PLAY_PARMS: callback, from, to
            let from = (flags & MCI_FROM != 0).then(|| rd(c, 4));
            let to = (flags & MCI_TO != 0).then(|| rd(c, 8));
            play(c, dev, from, to, flags & MCI_DGV_PLAY_REPEAT != 0, callback);
            0
        }
        MCI_RESUME => {
            tick(c);
            if let Some(d) = c.p.mci.get_mut(&dev) {
                if d.mode == MCI_MODE_PAUSE {
                    d.mode = MCI_MODE_PLAY;
                    d.last_us = c.p.clock_us;
                }
            }
            0
        }
        MCI_STOP => {
            for id in devices(c, dev) {
                stop(c, id, MCI_MODE_STOP);
            }
            0
        }
        MCI_PAUSE => {
            for id in devices(c, dev) {
                stop(c, id, MCI_MODE_PAUSE);
            }
            0
        }
        MCI_SEEK => {
            stop(c, dev, MCI_MODE_STOP);
            let to_ms = rd(c, 4);
            if let Some(d) = c.p.mci.get_mut(&dev) {
                d.pos = if flags & MCI_SEEK_TO_END != 0 {
                    d.media.frames()
                } else if flags & MCI_SEEK_TO_START != 0 {
                    0.0
                } else {
                    to_ms as f64 * d.media.rate() / 1000.0
                };
            }
            0
        }
        MCI_STATUS => {
            tick(c);
            // MCI_STATUS_PARMS: callback, return (out), item, track
            let v = status_value(c, dev, rd(c, 8));
            let _ = c.host.write_u32(parms + 4, v);
            0
        }
        MCI_SET => {
            if flags & MCI_SET_TIME_FORMAT != 0 {
                let f = rd(c, 4);
                if let Some(d) = c.p.mci.get_mut(&dev) {
                    d.time_format = f;
                }
            }
            0
        }
        MCI_SETAUDIO => {
            // MCI_DGV_SETAUDIO_PARMS: callback, item, value, over, algorithm, quality
            if flags & MCI_DGV_SETAUDIO_ITEM != 0 && flags & MCI_DGV_SETAUDIO_VALUE != 0 && rd(c, 4) == MCI_DGV_SETAUDIO_VOLUME {
                tick(c);
                let v = rd(c, 8).min(1000);
                if let Some(d) = c.p.mci.get_mut(&dev) {
                    d.volume = v;
                }
            }
            0
        }
        MCI_GETDEVCAPS => {
            let _ = c.host.write_u32(parms + 4, 1);
            0
        }
        _ => 0,
    };
    if r == 0 && msg != MCI_PLAY && msg != MCI_OPEN {
        if let Some(h) = callback {
            super::user32::post(c, h, MM_MCINOTIFY, MCI_NOTIFY_SUCCESSFUL, dev);
        }
    }
    c.trace(format!("(device {dev}, msg {msg:#x}, flags {flags:#x}) = {r}"));
    ok(4, r)
}

fn device_named(c: &Ctx, name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    c.p.mci.iter().find(|(_, d)| d.alias == n || d.alias.is_empty() && d.element.eq_ignore_ascii_case(name)).map(|(&id, _)| id)
}

/// mciSendString: "open <file> [type <t>] [alias <a>]", "play <a> [from x] [to y] [repeat]
/// [notify]", "status <a> mode|position|length", "setaudio <a> volume to <v>", ...
pub fn string(c: &mut Ctx, cmd: &str, ret_p: u32, cap: u32, hwnd: u32, wide: bool) -> u32 {
    // Words, with "quoted file names" kept whole.
    let mut words = Vec::new();
    let mut rest = cmd.trim();
    while !rest.is_empty() {
        if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"').unwrap_or(q.len());
            words.push(q[..end].to_string());
            rest = q.get(end + 1..).unwrap_or("").trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            words.push(rest[..end].to_string());
            rest = rest[end..].trim_start();
        }
    }
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let verb = lower.first().cloned().unwrap_or_default();
    let name = words.get(1).cloned().unwrap_or_default();
    let after = |k: &str| lower.iter().position(|w| w == k).and_then(|i| words.get(i + 1)).cloned();
    let has = |k: &str| lower.iter().any(|w| w == k);
    let callback = has("notify").then_some(hwnd & 0xFFFF);
    let mut out = String::new();
    let id = if name.eq_ignore_ascii_case("all") { Some(MCI_ALL_DEVICE_ID) } else { device_named(c, &name) };
    let r = match (verb.as_str(), id) {
        ("open", _) => {
            let alias = after("alias").unwrap_or_default();
            let element = if name.contains('.') || name.contains('\\') { name.clone() } else { String::new() };
            match open(c, &element, &alias) {
                Ok(id) => {
                    out = id.to_string();
                    0
                }
                Err(e) => e,
            }
        }
        (_, None) => MCIERR_INVALID_DEVICE_ID,
        ("close", Some(id)) => {
            for d in devices(c, id) {
                close(c, d);
            }
            0
        }
        ("play", Some(id)) => {
            let num = |s: Option<String>| s.and_then(|v| v.parse::<u32>().ok());
            play(c, id, num(after("from")), num(after("to")), has("repeat"), callback);
            0
        }
        ("resume", Some(id)) => {
            tick(c);
            if let Some(d) = c.p.mci.get_mut(&id) {
                if d.mode == MCI_MODE_PAUSE {
                    d.mode = MCI_MODE_PLAY;
                    d.last_us = c.p.clock_us;
                }
            }
            0
        }
        ("stop", Some(id)) => {
            for d in devices(c, id) {
                stop(c, d, MCI_MODE_STOP);
            }
            0
        }
        ("pause", Some(id)) => {
            for d in devices(c, id) {
                stop(c, d, MCI_MODE_PAUSE);
            }
            0
        }
        ("seek", Some(id)) => {
            stop(c, id, MCI_MODE_STOP);
            let target = after("to").unwrap_or_default().to_ascii_lowercase();
            if let Some(d) = c.p.mci.get_mut(&id) {
                d.pos = match target.as_str() {
                    "start" => 0.0,
                    "end" => d.media.frames(),
                    t => t.parse::<f64>().unwrap_or(0.0) * d.media.rate() / 1000.0,
                };
            }
            0
        }
        ("status", Some(id)) => {
            tick(c);
            let item = lower.get(2).cloned().unwrap_or_default();
            out = match item.as_str() {
                "mode" => match status_value(c, id, 4) {
                    MCI_MODE_PLAY => "playing".into(),
                    MCI_MODE_PAUSE => "paused".into(),
                    _ => "stopped".into(),
                },
                "position" => status_value(c, id, 2).to_string(),
                "length" => status_value(c, id, 1).to_string(),
                "volume" => status_value(c, id, 0x4019).to_string(),
                "ready" | "media" => "true".into(),
                _ => "0".into(),
            };
            0
        }
        ("setaudio", Some(id)) => {
            if has("volume") {
                tick(c);
                let v = after("to").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1000).min(1000);
                if let Some(d) = c.p.mci.get_mut(&id) {
                    d.volume = v;
                }
            }
            0
        }
        ("set" | "window" | "put" | "where" | "cue" | "realize", Some(_)) => 0,
        _ => MCIERR_UNRECOGNIZED_COMMAND,
    };
    if r == 0 && verb != "play" {
        if let (Some(h), Some(id)) = (callback, id) {
            super::user32::post(c, h, MM_MCINOTIFY, MCI_NOTIFY_SUCCESSFUL, id);
        }
    }
    if ret_p != 0 && cap > 0 {
        if wide {
            let units: Vec<u16> = out.encode_utf16().take(cap as usize - 1).chain([0]).collect();
            for (i, u) in units.iter().enumerate() {
                let _ = c.host.write(ret_p + 2 * i as u32, &u.to_le_bytes());
            }
        } else {
            let mut b: Vec<u8> = out.bytes().take(cap as usize - 1).collect();
            b.push(0);
            let _ = c.host.write(ret_p, &b);
        }
    }
    c.trace(format!("({cmd:?}) = {r} {out:?}"));
    r
}

/// Device ids by element/alias, for mciGetDeviceID.
pub fn device_id(c: &Ctx, name: &str) -> u32 {
    device_named(c, name).unwrap_or(0)
}

/// Ordered, so ticks and notifications run in a fixed order.
pub type Devices = BTreeMap<u32, McDevice>;
