//! Native-vs-wasm comparison with per-instruction undefined-flag masking.

use crate::proto::CaseOut;
use iced_x86::{Instruction, RflagsBits};

/// EFLAGS bits that are compared: CF PF AF ZF SF DF OF.
const FLAG_BITS: u32 = 0x0cd5;

/// Flags left undefined by the sequence (last writer wins).
pub fn undefined_flags(instrs: &[Instruction]) -> u32 {
    let mut undef = 0u32;
    for i in instrs {
        let modified = i.rflags_modified();
        undef = (undef & !modified) | i.rflags_undefined();
    }
    let mut bits = 0;
    if undef & RflagsBits::CF != 0 { bits |= 1 << 0; }
    if undef & RflagsBits::PF != 0 { bits |= 1 << 2; }
    if undef & RflagsBits::AF != 0 { bits |= 1 << 4; }
    if undef & RflagsBits::ZF != 0 { bits |= 1 << 6; }
    if undef & RflagsBits::SF != 0 { bits |= 1 << 7; }
    if undef & RflagsBits::OF != 0 { bits |= 1 << 11; }
    bits
}

pub struct Diff {
    pub field: String,
    pub native: String,
    pub wasm: String,
}

pub fn compare(native: &CaseOut, wasm: &CaseOut, instrs: &[Instruction], compare_xmm: bool) -> Vec<Diff> {
    let mut diffs = Vec::new();
    fn push_into(diffs: &mut Vec<Diff>, field: &str, a: String, b: String) {
        if a != b {
            diffs.push(Diff { field: field.to_string(), native: a, wasm: b });
        }
    }
    macro_rules! push {
        ($f:expr, $a:expr, $b:expr) => { push_into(&mut diffs, $f, $a, $b) };
    }
    push!("status", format!("{:#x}", native.status), format!("{:#x}", wasm.status));
    if native.status == wasm.status && wasm.fault_off != u32::MAX {
        push!("fault_off", native.fault_off.to_string(), wasm.fault_off.to_string());
    }
    if native.status != 0 && native.status == wasm.status {
        // After a fault only the fault location is meaningful.
        return diffs;
    }
    let names = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"];
    for r in 0..8 {
        push!(names[r], format!("{:#010x}", native.regs[r]), format!("{:#010x}", wasm.regs[r]));
    }
    let mask = FLAG_BITS & !undefined_flags(instrs);
    push!("eflags", format!("{:#06x}", native.eflags & mask), format!("{:#06x}", wasm.eflags & mask));
    for (i, (a, b)) in native.data.iter().zip(&wasm.data).enumerate() {
        if a != b {
            push!(&format!("data[{i:#x}]"), format!("{a:#04x}"), format!("{b:#04x}"));
            if diffs.len() > 12 { break; }
        }
    }
    for (i, (a, b)) in native.stack.iter().zip(&wasm.stack).enumerate() {
        if a != b {
            push!(&format!("stack[{i:#x}]"), format!("{a:#04x}"), format!("{b:#04x}"));
            if diffs.len() > 12 { break; }
        }
    }
    if compare_xmm {
        push!("mxcsr", format!("{:#x}", u32::from_le_bytes(native.fx[24..28].try_into().unwrap())), format!("{:#x}", u32::from_le_bytes(wasm.fx[24..28].try_into().unwrap())));
        for x in 0..8 {
            let a = &native.fx[160 + x * 16..176 + x * 16];
            let b = &wasm.fx[160 + x * 16..176 + x * 16];
            push!(&format!("xmm{x}"), hex(a), hex(b));
        }
    }
    diffs
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
