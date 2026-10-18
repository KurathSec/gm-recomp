//! gm8x_fix 0.5.9 (DbgHelper's GameMaker 8 part) as DbgHelper runs it: silently, every
//! patch the runner can take except the kinds its options leave out. A patch applies when
//! all its bytes are as before (done when all are as after); one that matches neither is
//! for another runner build.

use super::gm8x_fix_data::PATCHES;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Upx,
    Joystick,
    Memory,
    DirectPlay,
    Scheduler,
    InputLag,
    Reset,
    Bugfix,
}

/// The patch kinds DbgHelper can leave out (its config; the display reset patch always
/// applies).
pub struct Options {
    pub input_lag: bool,
    pub joystick: bool,
    pub scheduler: bool,
    pub memory: bool,
    pub direct_play: bool,
}

#[derive(PartialEq)]
enum State {
    Unfound,
    Able,
    Done,
}

fn state(exe: &[u8], bytes: &[(u32, u8, u8)]) -> State {
    let before = bytes.iter().all(|&(p, b, _)| exe.get(p as usize) == Some(&b));
    let after = bytes.iter().all(|&(p, _, a)| exe.get(p as usize) == Some(&a));
    if after {
        State::Done
    } else if before {
        State::Able
    } else {
        State::Unfound
    }
}

/// A runner packed with UPX (gm8x_fix needs upx.exe to unpack it first).
fn upx_packed(exe: &[u8]) -> bool {
    let pe = exe.get(0x3C..0x40).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let opt = exe.get(pe + 0x14..pe + 0x16).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
    exe.get(pe + 0x18 + opt..pe + 0x18 + opt + 8) == Some(b"UPX0\0\0\0\0")
}

pub fn apply(exe: &mut [u8], o: &Options) -> Result<Vec<String>, String> {
    if exe.get(0..2) != Some(b"MZ") {
        return Err("not an executable".into());
    }
    if upx_packed(exe) {
        return Err("the runner is packed with UPX; gm8x_fix needs it unpacked first".into());
    }
    let off = |k: Kind| match k {
        Kind::InputLag => !o.input_lag,
        Kind::Joystick => !o.joystick,
        Kind::Scheduler => !o.scheduler,
        Kind::Memory => !o.memory,
        Kind::DirectPlay => !o.direct_play,
        _ => false,
    };
    let states: Vec<State> = PATCHES
        .iter()
        .map(|&(bytes, _, kind)| match state(exe, bytes) {
            State::Able if off(kind) => State::Unfound,
            s => s,
        })
        .collect();
    let others = || PATCHES.iter().zip(&states).filter(|((_, _, k), _)| *k != Kind::Memory);
    let can_apply = others().any(|(_, s)| *s == State::Able);
    let applied = others().any(|(_, s)| *s == State::Done);
    if !can_apply && !applied {
        return Err("gm8x_fix does not know this runner (not GameMaker 7, 8.0 or 8.1)".into());
    }
    if !can_apply {
        return Ok(vec!["already patched".into()]);
    }
    let mut log = Vec::new();
    for ((bytes, name, _), s) in PATCHES.iter().zip(&states) {
        if *s == State::Able {
            for &(p, _, a) in bytes.iter() {
                exe[p as usize] = a;
            }
            log.push(name.to_string());
        }
    }
    Ok(log)
}
