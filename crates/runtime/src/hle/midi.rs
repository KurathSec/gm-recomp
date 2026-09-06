//! MIDI music (DirectMusic MIDI segments, the MCI sequencer) through a SoundFont
//! synthesizer. The SoundFont is large, so the host supplies it only once a game
//! plays MIDI (the browser fetches it then); until it arrives the music is silent.
//! Sound output never feeds back into the guest, so when it arrives does not affect
//! determinism.

use super::Ctx;
use super::audio::Mixer;
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};
use std::sync::Arc;

/// A MIDI file being played.
pub struct MidiPlayer {
    seq: MidiFileSequencer,
}

/// A Standard MIDI File (also inside a RIFF RMID wrapper).
pub fn parse(bytes: &[u8]) -> Option<Arc<MidiFile>> {
    let smf = if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"RMID") { bytes.get(20..)? } else { bytes };
    MidiFile::new(&mut &smf[..]).ok().map(Arc::new)
}

/// The SoundFont once the host has it (asking for it the first time).
pub fn soundfont(c: &mut Ctx) -> Option<Arc<SoundFont>> {
    if let Some(sf) = &c.p.soundfont {
        return Some(sf.clone());
    }
    if c.p.soundfont_failed {
        return None;
    }
    let bytes = c.host.soundfont()?;
    match SoundFont::new(&mut &bytes[..]) {
        Ok(sf) => {
            let sf = Arc::new(sf);
            c.p.soundfont = Some(sf.clone());
            Some(sf)
        }
        Err(e) => {
            c.p.log(format!("MIDI: the SoundFont cannot be read ({e:?}); MIDI stays silent"));
            c.p.soundfont_failed = true;
            None
        }
    }
}

pub fn player(sf: &Arc<SoundFont>, midi: &Arc<MidiFile>, looping: bool, rate: u32) -> Option<MidiPlayer> {
    let settings = SynthesizerSettings::new(rate as i32);
    let synth = Synthesizer::new(sf, &settings).ok()?;
    let mut seq = MidiFileSequencer::new(synth);
    seq.play(midi, looping);
    Some(MidiPlayer { seq })
}

/// Renders what the player plays between `t0` and `t1` (µs) into the mix, at
/// `volume` (hundredths of a decibel).
pub fn render(mixer: &mut Mixer, player: &mut MidiPlayer, t0: u64, t1: u64, volume: i32) {
    let (k0, k1) = (mixer.frame_at(t0).max(mixer.base), mixer.frame_at(t1));
    if k1 <= k0 {
        return;
    }
    let n = (k1 - k0) as usize;
    let need = 2 * (k1 - mixer.base) as usize;
    if mixer.buf.len() < need {
        mixer.buf.resize(need, 0.0);
    }
    let mut l = vec![0f32; n];
    let mut r = vec![0f32; n];
    player.seq.render(&mut l, &mut r);
    let g = if volume <= -10000 { 0.0 } else { 10f32.powf(volume.min(0) as f32 / 2000.0) };
    let o = 2 * (k0 - mixer.base) as usize;
    for i in 0..n {
        mixer.buf[o + 2 * i] += l[i] * g;
        mixer.buf[o + 2 * i + 1] += r[i] * g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One note (middle C, piano) for half a second, as a format-0 MIDI file.
    fn one_note() -> Vec<u8> {
        let track: Vec<u8> = vec![0x00, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0x00, 0xFF, 0x2F, 0x00];
        let mut v = b"MThd".to_vec();
        v.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xE0]);
        v.extend_from_slice(b"MTrk");
        v.extend_from_slice(&(track.len() as u32).to_be_bytes());
        v.extend_from_slice(&track);
        v
    }

    #[test]
    fn renders_a_note() {
        let Ok(bytes) = std::fs::read(std::env::var("GMR_SOUNDFONT").unwrap_or_else(|_| "/usr/share/soundfonts/default.sf2".into())) else {
            eprintln!("no SoundFont installed; skipping");
            return;
        };
        let sf = Arc::new(SoundFont::new(&mut &bytes[..]).unwrap());
        let midi = parse(&one_note()).unwrap();
        let mut p = player(&sf, &midi, false, 48_000).unwrap();
        let mut mixer = Mixer::default();
        render(&mut mixer, &mut p, 0, 500_000, 0);
        let out = mixer.drain(500_000);
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!(rms > 0.01, "rms {rms}");
    }
}
