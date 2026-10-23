//! Identifies which GameMaker runner an entry exe belongs to. The runtime does
//! not depend on this (it executes whatever x86 it is given); it only feeds the
//! reports and lets the corpus be grouped by runner version.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Engine {
    Gm80,
    Gm81,
    Gms {
        bytecode: u8,
        version: [u32; 4],
        /// No CODE chunk means the game logic was compiled into the exe by YYC.
        yyc: bool,
        data_file: String,
        /// Shader resources (compiled at run time by d3dcompiler).
        shaders: u32,
        /// The runner is a 64-bit exe (GameMaker Studio 2's later ones).
        x64: bool,
    },
    Unknown,
}

const GM8_MAGIC: u32 = 1_234_321;
/// Start of the GM8.1 runner routine that scans for the scrambled game data header.
const GM81_SIGNATURE: (usize, [u8; 8]) = (0x226CF3, [0xE8, 0x80, 0xF2, 0xDD, 0xFF, 0xC7, 0x45, 0xF0]);

fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    d.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// A GM8.0 game's data header where its runner looks for it: at 1,980,000 or every
/// 10,000 bytes from 2,000,000 on (the runner builds differ in size).
pub fn gm80_header(exe: &[u8]) -> Option<usize> {
    std::iter::once(1_980_000).chain((2_000_000..exe.len()).step_by(10_000)).find(|&p| u32_at(exe, p) == Some(GM8_MAGIC) && u32_at(exe, p + 4) == Some(800))
}

pub fn detect(exe: &[u8], game_dir: &Path) -> Engine {
    if gm80_header(exe).is_some() {
        return Engine::Gm80;
    }
    let (off, sig) = GM81_SIGNATURE;
    if exe.get(off..off + sig.len()) == Some(&sig[..]) {
        return Engine::Gm81;
    }
    for name in ["data.win", "game.win"] {
        if let Ok(d) = std::fs::read(game_dir.join(name)) {
            if let Some(e) = parse_gms_form(&d, name, pe64(exe)) {
                return e;
            }
        }
    }
    // YYC builds can embed the FORM blob in the exe's .data section instead of shipping data.win.
    let mut from = 0;
    while let Some(pos) = find(&exe[from..], b"FORM").map(|p| p + from) {
        if exe.get(pos + 8..pos + 12) == Some(b"GEN8") {
            if let Some(e) = parse_gms_form(&exe[pos..], &format!("embedded@{pos}"), pe64(exe)) {
                return e;
            }
        }
        from = pos + 4;
    }
    Engine::Unknown
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A PE32+ (64-bit) image: optional header magic 0x20b.
pub fn pe64(exe: &[u8]) -> bool {
    let Some(pe) = u32_at(exe, 0x3c) else { return false };
    exe.get(pe as usize..pe as usize + 4) == Some(b"PE\0\0") && exe.get(pe as usize + 24..pe as usize + 26) == Some(&[0x0b, 0x02][..])
}

fn parse_gms_form(d: &[u8], name: &str, x64: bool) -> Option<Engine> {
    if d.get(0..4) != Some(b"FORM") {
        return None;
    }
    let end = (8 + u32_at(d, 4)? as usize).min(d.len());
    let mut off = 8;
    let (mut gen8, mut code_size, mut shaders) = (None, None, 0);
    while off + 8 <= end {
        let size = u32_at(d, off + 4)? as usize;
        match &d[off..off + 4] {
            b"GEN8" => gen8 = Some(off + 8),
            b"CODE" => code_size = Some(size),
            b"SHDR" if size >= 4 => shaders = u32_at(d, off + 8)?,
            _ => {}
        }
        off += 8 + size;
    }
    let g = gen8?;
    Some(Engine::Gms {
        bytecode: *d.get(g + 1)?,
        version: [u32_at(d, g + 44)?, u32_at(d, g + 48)?, u32_at(d, g + 52)?, u32_at(d, g + 56)?],
        yyc: code_size.unwrap_or(0) == 0,
        data_file: name.to_owned(),
        shaders,
        x64,
    })
}
