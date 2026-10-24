//! The Audio Compression Manager (msacm32, emulated when a game has no copy): streams
//! that decode Microsoft ADPCM and IMA ADPCM to 16-bit PCM, as Windows' own codecs do
//! (FMOD Ex hands such WAV files to ACM: without them it reported "Unsupported file or
//! audio format"), and copy PCM. Other formats are not possible.

use serde::{Deserialize, Serialize};
use super::{ApiResult, Ctx, Registry, Ret};
use std::collections::HashMap;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

const ACMERR_NOTPOSSIBLE: u32 = 512;
const MMSYSERR_INVALHANDLE: u32 = 5;
const MMSYSERR_NOTSUPPORTED: u32 = 8;
const PCM: u16 = 1;
const MS_ADPCM: u16 = 2;
const IMA_ADPCM: u16 = 0x11;

/// A WAVEFORMATEX, with what the ADPCM formats add after it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Format {
    tag: u16,
    channels: u16,
    rate: u32,
    block_align: u16,
    bits: u16,
    samples_per_block: u16,
    /// MS ADPCM's predictor coefficient pairs.
    coefs: Vec<(i32, i32)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Stream {
    src: Format,
    dst: Format,
}

/// Open streams by handle.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Streams {
    list: HashMap<u32, Stream>,
    next: u32,
}

fn read_format(c: &Ctx, p: u32) -> Option<Format> {
    if p == 0 {
        return None;
    }
    let u16_at = |o: u32| c.host.read_u16(p + o).unwrap_or(0);
    let tag = u16_at(0);
    let mut f = Format { tag, channels: u16_at(2), rate: c.host.read_u32(p + 4).unwrap_or(0), block_align: u16_at(12), bits: u16_at(14), ..Default::default() };
    let extra = if tag == PCM { 0 } else { u16_at(16) };
    match tag {
        MS_ADPCM if extra >= 4 => {
            f.samples_per_block = u16_at(18);
            let n = u16_at(20).min(256) as u32;
            f.coefs = (0..n).map(|k| (u16_at(22 + 4 * k) as i16 as i32, u16_at(24 + 4 * k) as i16 as i32)).collect();
        }
        IMA_ADPCM if extra >= 2 => f.samples_per_block = u16_at(18),
        _ => {}
    }
    Some(f)
}

/// Whether `src` decodes (or copies) to `dst`, and the PCM it gives.
fn convertible(src: &Format, dst: &Format) -> bool {
    let pcm16 = dst.tag == PCM && dst.bits == 16 && dst.channels == src.channels && dst.rate == src.rate;
    match src.tag {
        MS_ADPCM => pcm16 && src.samples_per_block >= 2 && !src.coefs.is_empty() && src.channels >= 1 && src.channels <= 2,
        IMA_ADPCM => pcm16 && src.samples_per_block >= 1 && src.channels >= 1 && src.channels <= 2,
        PCM => dst.tag == PCM && dst.bits == src.bits && dst.channels == src.channels && dst.rate == src.rate,
        _ => false,
    }
}

fn pcm_for(src: &Format) -> Format {
    let bits = if src.tag == PCM { src.bits } else { 16 };
    Format { tag: PCM, channels: src.channels, rate: src.rate, block_align: src.channels * bits / 8, bits, ..Default::default() }
}

fn write_pcm_format(c: &mut Ctx, p: u32, f: &Format, size: u32) {
    let _ = c.host.fill(p, size.min(18), 0);
    let _ = c.host.write_u16(p, PCM);
    let _ = c.host.write_u16(p + 2, f.channels);
    let _ = c.host.write_u32(p + 4, f.rate);
    let _ = c.host.write_u32(p + 8, f.rate * f.block_align as u32);
    let _ = c.host.write_u16(p + 12, f.block_align);
    let _ = c.host.write_u16(p + 14, f.bits);
}

/// Output bytes for `n` input bytes (whole blocks only), and the reverse.
fn out_size(s: &Stream, n: u32) -> u32 {
    if s.src.tag == PCM {
        return n;
    }
    let blocks = n / s.src.block_align.max(1) as u32;
    blocks * s.src.samples_per_block as u32 * s.src.channels as u32 * 2
}

fn in_size(s: &Stream, n: u32) -> u32 {
    if s.src.tag == PCM {
        return n;
    }
    let per_block = s.src.samples_per_block as u32 * s.src.channels as u32 * 2;
    n.div_ceil(per_block.max(1)) * s.src.block_align as u32
}

const ADAPT: [i32; 16] = [230, 230, 230, 230, 307, 409, 512, 614, 768, 614, 512, 409, 307, 230, 230, 230];

fn ms_adpcm_block(f: &Format, b: &[u8], out: &mut Vec<i16>) {
    let ch = f.channels as usize;
    if b.len() < 7 * ch {
        return;
    }
    let i16_at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]) as i32;
    let mut coef = [(0i32, 0i32); 2];
    let (mut delta, mut s1, mut s2) = ([0i32; 2], [0i32; 2], [0i32; 2]);
    for c in 0..ch {
        coef[c] = f.coefs[(b[c] as usize).min(f.coefs.len() - 1)];
        delta[c] = i16_at(ch + 2 * c);
        s1[c] = i16_at(3 * ch + 2 * c);
        s2[c] = i16_at(5 * ch + 2 * c);
    }
    for c in 0..ch {
        out.push(s2[c] as i16);
    }
    for c in 0..ch {
        out.push(s1[c] as i16);
    }
    let mut decode = |c: usize, nib: u8, out: &mut Vec<i16>| {
        let signed = if nib & 8 != 0 { nib as i32 - 16 } else { nib as i32 };
        let pred = (s1[c] * coef[c].0 + s2[c] * coef[c].1) >> 8;
        let v = (pred + signed * delta[c]).clamp(-32768, 32767);
        s2[c] = s1[c];
        s1[c] = v;
        delta[c] = ((ADAPT[nib as usize] * delta[c]) >> 8).max(16);
        out.push(v as i16);
    };
    let total = f.samples_per_block as usize * ch;
    let mut produced = 2 * ch;
    for &byte in &b[7 * ch..] {
        for (k, nib) in [byte >> 4, byte & 15].into_iter().enumerate() {
            if produced < total {
                decode(if ch == 2 { k } else { 0 }, nib, out);
                produced += 1;
            }
        }
    }
}

const IMA_INDEX: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];
const IMA_STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209,
    230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749,
    3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623,
    27086, 29794, 32767,
];

fn ima_adpcm_block(f: &Format, b: &[u8], out: &mut Vec<i16>) {
    let ch = f.channels as usize;
    if b.len() < 4 * ch {
        return;
    }
    let (mut pred, mut index) = ([0i32; 2], [0i32; 2]);
    for c in 0..ch {
        pred[c] = i16::from_le_bytes([b[4 * c], b[4 * c + 1]]) as i32;
        index[c] = (b[4 * c + 2] as i32).clamp(0, 88);
    }
    let n = f.samples_per_block as usize;
    // per channel, the header sample then 8 samples from each 4-byte group
    let mut chans: Vec<Vec<i16>> = (0..ch).map(|c| vec![pred[c] as i16]).collect();
    let data = &b[4 * ch..];
    for (g, group) in data.chunks(4).enumerate() {
        let c = g % ch;
        for &byte in group {
            for nib in [byte & 15, byte >> 4] {
                if chans[c].len() >= n {
                    continue;
                }
                let step = IMA_STEP[index[c] as usize];
                let mut diff = step >> 3;
                if nib & 1 != 0 {
                    diff += step >> 2;
                }
                if nib & 2 != 0 {
                    diff += step >> 1;
                }
                if nib & 4 != 0 {
                    diff += step;
                }
                pred[c] = if nib & 8 != 0 { pred[c] - diff } else { pred[c] + diff }.clamp(-32768, 32767);
                index[c] = (index[c] + IMA_INDEX[nib as usize]).clamp(0, 88);
                chans[c].push(pred[c] as i16);
            }
        }
    }
    let len = chans.iter().map(|v| v.len()).min().unwrap_or(0);
    for i in 0..len {
        for chan in &chans {
            out.push(chan[i]);
        }
    }
}

pub fn register(r: &mut Registry) {
    let acm = "msacm32.dll";
    // (had, src, dst, dst size, flags): the PCM a format decodes to
    r.add_on_demand(acm, "acmFormatSuggest", |c| {
        let (src, dst, size) = (c.arg(1), c.arg(2), c.arg(3));
        let Some(f) = read_format(c, src) else { return ok(5, ACMERR_NOTPOSSIBLE) };
        let pcm = pcm_for(&f);
        if !matches!(f.tag, PCM | MS_ADPCM | IMA_ADPCM) || size < 16 {
            return ok(5, ACMERR_NOTPOSSIBLE);
        }
        write_pcm_format(c, dst, &pcm, size);
        ok(5, 0)
    });
    // (phas, had, src, dst, filter, callback, instance, flags: ACM_STREAMOPENF_QUERY 1)
    r.add_on_demand(acm, "acmStreamOpen", |c| {
        let (phas, src, dst, flags) = (c.arg(0), c.arg(2), c.arg(3), c.arg(7));
        let (Some(src), Some(dst)) = (read_format(c, src), read_format(c, dst)) else { return ok(8, ACMERR_NOTPOSSIBLE) };
        if !convertible(&src, &dst) {
            if phas != 0 {
                let _ = c.host.write_u32(phas, 0);
            }
            return ok(8, ACMERR_NOTPOSSIBLE);
        }
        if flags & 1 != 0 {
            return ok(8, 0);
        }
        let ss = &mut c.p.acm;
        ss.next += 1;
        let h = 0xACC0_0000 + ss.next;
        ss.list.insert(h, Stream { src, dst });
        let _ = c.host.write_u32(phas, h);
        ok(8, 0)
    });
    r.add_on_demand(acm, "acmStreamClose", |c| {
        let found = c.p.acm.list.remove(&c.arg(0)).is_some();
        ok(2, if found { 0 } else { MMSYSERR_INVALHANDLE })
    });
    // (has, bytes, out size, flags: ACM_STREAMSIZEF_DESTINATION 1)
    r.add_on_demand(acm, "acmStreamSize", |c| {
        let (h, n, out, flags) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let Some(s) = c.p.acm.list.get(&h) else { return ok(4, MMSYSERR_INVALHANDLE) };
        let v = if flags & 1 != 0 { in_size(s, n) } else { out_size(s, n) };
        let _ = c.host.write_u32(out, v);
        ok(4, 0)
    });
    // ACMSTREAMHEADER: cbStruct, fdwStatus, dwUser, pbSrc, cbSrcLength, cbSrcLengthUsed,
    // dwSrcUser, pbDst, cbDstLength, cbDstLengthUsed, ...
    const PREPARED: u32 = 0x0002_0000;
    const DONE: u32 = 0x0001_0000;
    r.add_on_demand(acm, "acmStreamPrepareHeader", |c| {
        let p = c.arg(1);
        let st = c.host.read_u32(p + 4).unwrap_or(0);
        let _ = c.host.write_u32(p + 4, st | PREPARED);
        ok(3, 0)
    });
    r.add_on_demand(acm, "acmStreamUnprepareHeader", |c| {
        let p = c.arg(1);
        let st = c.host.read_u32(p + 4).unwrap_or(0);
        let _ = c.host.write_u32(p + 4, st & !PREPARED);
        ok(3, 0)
    });
    r.add_on_demand(acm, "acmStreamConvert", |c| {
        let (h, p) = (c.arg(0), c.arg(1));
        let rd = |c: &Ctx, o: u32| c.host.read_u32(p + o).unwrap_or(0);
        let (src_at, src_len, dst_at, dst_len) = (rd(c, 12), rd(c, 16), rd(c, 28), rd(c, 32));
        let Some(s) = c.p.acm.list.get(&h) else { return ok(3, MMSYSERR_INVALHANDLE) };
        let (src, block) = (s.src.clone(), s.src.block_align.max(1) as u32);
        let (used, out): (u32, Vec<u8>) = if src.tag == PCM {
            let n = src_len.min(dst_len) / block * block;
            let mut v = vec![0u8; n as usize];
            let _ = c.host.read(src_at, &mut v);
            (n, v)
        } else {
            let per_block = src.samples_per_block as u32 * src.channels as u32 * 2;
            let blocks = (src_len / block).min(dst_len / per_block.max(1));
            let mut data = vec![0u8; (blocks * block) as usize];
            let _ = c.host.read(src_at, &mut data);
            let mut pcm: Vec<i16> = Vec::with_capacity((blocks * per_block / 2) as usize);
            for b in data.chunks(block as usize) {
                if src.tag == MS_ADPCM {
                    ms_adpcm_block(&src, b, &mut pcm);
                } else {
                    ima_adpcm_block(&src, b, &mut pcm);
                }
            }
            (blocks * block, pcm.iter().flat_map(|v| v.to_le_bytes()).collect())
        };
        let _ = c.host.write(dst_at, &out);
        let _ = c.host.write_u32(p + 20, used);
        let _ = c.host.write_u32(p + 36, out.len() as u32);
        let st = rd(c, 4);
        let _ = c.host.write_u32(p + 4, st | DONE);
        ok(3, 0)
    });
    // (hao, metric, out): ACM_METRIC_MAX_SIZE_FORMAT 50 is all that is asked
    r.add_on_demand(acm, "acmMetrics", |c| {
        if c.arg(1) == 50 {
            let _ = c.host.write_u32(c.arg(2), 50);
            return ok(3, 0);
        }
        ok(3, MMSYSERR_NOTSUPPORTED)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_adpcm_header_samples_come_first() {
        let f = Format { tag: MS_ADPCM, channels: 1, rate: 22050, block_align: 9, bits: 4, samples_per_block: 4, coefs: vec![(256, 0)] };
        // predictor 0, delta 16, sample1 100, sample2 50, then two nibbles of 0
        let b = [0u8, 16, 0, 100, 0, 50, 0, 0x00, 0];
        let mut out = Vec::new();
        ms_adpcm_block(&f, &b, &mut out);
        assert_eq!(out, vec![50, 100, 100, 100]);
    }

    #[test]
    fn ima_adpcm_block_length() {
        let f = Format { tag: IMA_ADPCM, channels: 1, rate: 22050, block_align: 8, bits: 4, samples_per_block: 9, coefs: vec![] };
        let b = [0x10u8, 0, 0, 0, 0, 0, 0, 0];
        let mut out = Vec::new();
        ima_adpcm_block(&f, &b, &mut out);
        assert_eq!(out.len(), 9);
        assert_eq!(out[0], 16);
    }
}
