//! Which EFLAGS each instruction reads and writes, for liveness, and the lazy
//! flag model: arithmetic results are kept as (op, operands, result) until a
//! reader needs an actual bit.

use iced_x86::{ConditionCode, Instruction, Mnemonic, RflagsBits};

pub const CF: u8 = 1 << 0;
pub const PF: u8 = 1 << 1;
pub const AF: u8 = 1 << 2;
pub const ZF: u8 = 1 << 3;
pub const SF: u8 = 1 << 4;
pub const OF: u8 = 1 << 5;
pub const ALL: u8 = CF | PF | AF | ZF | SF | OF;
pub const FLAG_NAMES: [&str; 6] = ["cf", "pf", "af", "zf", "sf", "of"];

fn from_rflags(bits: u32) -> u8 {
    let mut m = 0;
    if bits & RflagsBits::CF != 0 { m |= CF; }
    if bits & RflagsBits::PF != 0 { m |= PF; }
    if bits & RflagsBits::AF != 0 { m |= AF; }
    if bits & RflagsBits::ZF != 0 { m |= ZF; }
    if bits & RflagsBits::SF != 0 { m |= SF; }
    if bits & RflagsBits::OF != 0 { m |= OF; }
    m
}

/// (defined, read) flag masks. Undefined results count as defined: their old
/// value is dead afterwards.
pub fn effects(i: &Instruction) -> (u8, u8) {
    let def = from_rflags(i.rflags_written() | i.rflags_cleared() | i.rflags_set() | i.rflags_undefined());
    let used = from_rflags(i.rflags_read());
    match i.mnemonic() {
        // iced marks these as reading everything; translated code only needs what pushfd/lahf copy.
        Mnemonic::Pushfd | Mnemonic::Pushf | Mnemonic::Lahf => (def, ALL),
        _ => (def, used),
    }
}

/// How the lazily tracked flags are derived from the saved operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagOp {
    /// res = a + b
    Add,
    /// res = a + b + c
    Adc,
    /// res = a - b
    Sub,
    /// res = a - b - c
    Sbb,
    /// and/or/xor/test: CF = OF = 0, AF = 0.
    Logic,
    /// res = a + 1; CF untouched (already materialized).
    Inc,
    /// res = a - 1; CF untouched.
    Dec,
    /// res = a << n (n >= 1); b holds n.
    Shl,
    /// res = a >> n logical; b holds n.
    Shr,
    /// res = a >> n arithmetic; b holds n.
    Sar,
}

/// Flags a condition code needs.
pub fn cond_uses(cc: ConditionCode) -> u8 {
    match cc {
        ConditionCode::None => 0,
        ConditionCode::o | ConditionCode::no => OF,
        ConditionCode::b | ConditionCode::ae => CF,
        ConditionCode::e | ConditionCode::ne => ZF,
        ConditionCode::be | ConditionCode::a => CF | ZF,
        ConditionCode::s | ConditionCode::ns => SF,
        ConditionCode::p | ConditionCode::np => PF,
        ConditionCode::l | ConditionCode::ge => SF | OF,
        ConditionCode::le | ConditionCode::g => SF | OF | ZF,
    }
}
