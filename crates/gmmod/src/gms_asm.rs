//! GameMaker: Studio bytecode (versions 14 to 16), for assembling new code. Encodings after
//! UndertaleModLib (UndertaleCode.cs at 92c968f): a 32-bit word (opcode in bits 31-24, types
//! in 23-20 and 19-16, an instance or immediate in 15-0), then an operand for some.

/// Data types.
pub mod ty {
    pub const DOUBLE: u32 = 0;
    pub const INT32: u32 = 2;
    pub const VAR: u32 = 5;
    pub const INT16: u32 = 0xF;
}

/// The instance field of `self` (-1).
pub const SELF: u32 = 0xFFFF;

/// Opcodes, by their name in bytecode 15 and later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Conv,
    Pop,
    /// push of an int16 in the word (`pushi`; `push.e` before bytecode 15)
    PushI,
    PopZ,
    B,
    Push,
    Call,
}

impl Op {
    /// The opcode byte for `bytecode`.
    pub fn code(self, bytecode: u32) -> u8 {
        use Op::*;
        let new = bytecode >= 15;
        match self {
            Conv => if new { 0x07 } else { 0x03 },
            Pop => if new { 0x45 } else { 0x41 },
            PushI => if new { 0x84 } else { 0xC0 },
            PopZ => if new { 0x9E } else { 0x9F },
            B => if new { 0xB6 } else { 0xB7 },
            Push => 0xC0,
            Call => if new { 0xD9 } else { 0xDA },
        }
    }
}

/// An instruction's first word: `op` with types `t1` (bits 19-16) and `t2` (23-20) and the
/// low 16 bits.
pub fn word(bytecode: u32, op: Op, t1: u32, t2: u32, low: u32) -> u32 {
    (op.code(bytecode) as u32) << 24 | (t2 & 0xF) << 20 | (t1 & 0xF) << 16 | (low & 0xFFFF)
}

/// A branch to `words` words ahead.
pub fn branch(bytecode: u32, words: u32) -> u32 {
    let mask = if bytecode >= 15 { 0x007F_FFFF } else { 0x00FF_FFFF };
    (Op::B.code(bytecode) as u32) << 24 | (words & mask)
}

/// The size of the instruction whose first word is `w`, and whether its operand is a
/// variable or function reference.
pub fn decode(bytecode: u32, w: u32) -> (usize, bool) {
    let op = (w >> 24) as u8;
    let t1 = (w >> 16) & 0xF;
    let new = bytecode >= 15;
    let push = if new { (0xC0..=0xC3).contains(&op) } else { op == 0xC0 };
    if push {
        return match t1 {
            ty::DOUBLE | 3 => (12, false),
            ty::VAR => (8, true),
            ty::INT16 => (4, false),
            _ => (8, false),
        };
    }
    if op == Op::Pop.code(bytecode) {
        return if t1 == ty::INT16 { (4, false) } else { (8, true) };
    }
    if op == Op::Call.code(bytecode) {
        return (8, true);
    }
    if op == 0xFF && t1 == ty::INT32 {
        return (8, false);
    }
    (4, false)
}

/// A reference from code.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Ref {
    /// VARI entry, with the reference type in the operand's top byte (0xA0: a plain
    /// variable)
    Var(usize, u8),
    /// FUNC entry
    Func(usize),
}

/// An instruction.
#[derive(Clone, Debug)]
pub enum Ins {
    /// words as they are
    Raw(Vec<u32>),
    /// a word followed by a reference operand
    Ref(u32, Ref),
}

/// `code` as bytes, and each reference's operand offset in them. A reference's operand is
/// its type's top byte for now: the chain links go in when the file is written.
pub fn assemble(code: &[Ins]) -> (Vec<u8>, Vec<(usize, Ref)>) {
    let mut bytes = Vec::new();
    let mut refs = Vec::new();
    for i in code {
        match i {
            Ins::Raw(w) => w.iter().for_each(|x| bytes.extend_from_slice(&x.to_le_bytes())),
            Ins::Ref(w, r) => {
                bytes.extend_from_slice(&w.to_le_bytes());
                refs.push((bytes.len(), *r));
                let top = match r {
                    Ref::Var(_, t) => (*t as u32) << 24,
                    Ref::Func(_) => 0,
                };
                bytes.extend_from_slice(&top.to_le_bytes());
            }
        }
    }
    (bytes, refs)
}
