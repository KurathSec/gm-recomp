//! Sound output: every playing DirectSound buffer is mixed into one stereo stream
//! on the virtual clock, at the moment its play cursor passes the samples. The host
//! drains finished samples (at each Present) into its [`AudioSink`]: the browser's
//! AudioWorklet ring, or a WAV file in the harness.

use crate::mem::Guest;
use crate::process::SoundBuffer;

pub const RATE: u32 = 48_000;

pub trait AudioSink {
    /// Interleaved stereo samples at `rate` Hz, in order, without gaps.
    fn push(&mut self, _samples: &[f32], _rate: u32) {}
}

pub struct NullSink;
impl AudioSink for NullSink {}

/// The mix in progress: output frames from `base` on, not yet drained.
pub struct Mixer {
    pub rate: u32,
    pub base: u64,
    pub buf: Vec<f32>,
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer { rate: RATE, base: 0, buf: Vec::new() }
    }
}

impl Mixer {
    pub fn frame_at(&self, us: u64) -> u64 {
        us * self.rate as u64 / 1_000_000
    }

    /// Output frames before `now` are complete once every buffer was advanced to `now`.
    pub fn drain(&mut self, now_us: u64) -> Vec<f32> {
        let end = self.frame_at(now_us).max(self.base);
        let n = (end - self.base) as usize;
        if self.buf.len() < 2 * n {
            self.buf.resize(2 * n, 0.0);
        }
        let mut out: Vec<f32> = self.buf.drain(..2 * n).collect();
        for s in &mut out {
            *s = s.clamp(-1.0, 1.0);
        }
        self.base = end;
        out
    }
}

/// DirectSound volume (hundredths of a decibel, -10000..0) and pan as left/right gains.
fn gains(volume: i32, pan: i32) -> (f32, f32) {
    let g = |db100: i32| if db100 <= -10000 { 0.0 } else { 10f32.powf(db100 as f32 / 2000.0) };
    let v = g(volume.min(0));
    let l = if pan > 0 { g(-pan) } else { 1.0 };
    let r = if pan < 0 { g(pan) } else { 1.0 };
    (v * l, v * r)
}

/// Reads frame `i` (wrapped) of a buffer as stereo.
fn frame(mem: &dyn Guest, b: &SoundBuffer, i: u64, frames: u64) -> (f32, f32) {
    let i = i % frames.max(1);
    let align = b.block_align.max(1) as u32;
    let addr = b.data + (i as u32) * align;
    let mut raw = [0u8; 4];
    let n = (align as usize).min(4);
    if mem.read(addr, &mut raw[..n]).is_err() {
        return (0.0, 0.0);
    }
    if b.bits == 8 {
        let s = |x: u8| (x as f32 - 128.0) / 128.0;
        if b.channels >= 2 { (s(raw[0]), s(raw[1])) } else { (s(raw[0]), s(raw[0])) }
    } else {
        let s = |k: usize| i16::from_le_bytes([raw[k], raw[k + 1]]) as f32 / 32768.0;
        if b.channels >= 2 { (s(0), s(2)) } else { (s(0), s(0)) }
    }
}

/// Mixes what `b` plays between `t0` and `t1` (µs), starting at source frame `start`.
/// Returns false when a non-looping buffer ran past its end.
pub fn mix(mixer: &mut Mixer, mem: &dyn Guest, b: &SoundBuffer, start: f64, t0: u64, t1: u64) {
    let frames = (b.size / b.block_align.max(1) as u32) as u64;
    if frames == 0 || b.format_tag != 1 || !(b.bits == 8 || b.bits == 16) {
        return;
    }
    let (k0, k1) = (mixer.frame_at(t0).max(mixer.base), mixer.frame_at(t1));
    if k1 <= k0 {
        return;
    }
    let need = 2 * (k1 - mixer.base) as usize;
    if mixer.buf.len() < need {
        mixer.buf.resize(need, 0.0);
    }
    let (gl, gr) = gains(b.volume, b.pan);
    if gl == 0.0 && gr == 0.0 {
        return;
    }
    let step = b.freq as f64 / mixer.rate as f64;
    // Source position of output frame k0.
    let mut pos = start + (k0 as f64 * 1e6 / mixer.rate as f64 - t0 as f64) * b.freq as f64 / 1e6;
    for k in k0..k1 {
        if pos < 0.0 {
            pos += step;
            continue;
        }
        if !b.looping && pos >= frames as f64 {
            break;
        }
        let i = pos.floor();
        let f = (pos - i) as f32;
        let (l0, r0) = frame(mem, b, i as u64, frames);
        let (l1, r1) = if b.looping || (i as u64) + 1 < frames { frame(mem, b, i as u64 + 1, frames) } else { (l0, r0) };
        let o = 2 * (k - mixer.base) as usize;
        mixer.buf[o] += (l0 + (l1 - l0) * f) * gl;
        mixer.buf[o + 1] += (r0 + (r1 - r0) * f) * gr;
        pos += step;
    }
}

/// A WAV file of everything pushed (the harness's `--audio-out`).
pub struct WavSink {
    pub path: std::path::PathBuf,
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl AudioSink for WavSink {
    fn push(&mut self, samples: &[f32], rate: u32) {
        self.rate = rate;
        self.samples.extend_from_slice(samples);
    }
}

impl WavSink {
    pub fn bytes(&self) -> Vec<u8> {
        let data: Vec<u8> = self.samples.iter().flat_map(|s| ((s * 32767.0) as i16).to_le_bytes()).collect();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        for x in [16u32] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&self.rate.to_le_bytes());
        v.extend_from_slice(&(self.rate * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, self.bytes());
    }
}
