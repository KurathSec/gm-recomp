//! Native-vs-wasm comparison with per-instruction undefined-flag masking.

use crate::proto::CaseOut;
use iced_x86::{Instruction, RflagsBits};

/// EFLAGS bits that are compared: CF PF AF ZF SF DF OF.
const FLAG_BITS: u32 = 0x0cd5;

/// Undefined-flag analysis over the program's control-flow graph (branches, calls,
/// loops): returns the flags that may be undefined when `hlt` is reached, and whether
/// any instruction may read a flag that is undefined on some path into it.
pub fn undefined_analysis(instrs: &[Instruction]) -> (u32, bool) {
    use iced_x86::{FlowControl, Mnemonic, OpKind, Register};
    use std::collections::HashMap;
    let index: HashMap<u64, usize> = instrs.iter().enumerate().map(|(k, i)| (i.ip(), k)).collect();
    let n = instrs.len();
    let mut undef_in = vec![0u32; n];
    let mut reads_undef = false;
    let mut out_at_hlt = 0u32;
    // Returns-to targets: every instruction after a call.
    let after_calls: Vec<usize> = instrs.iter().enumerate().filter(|(_, i)| i.flow_control() == FlowControl::Call).map(|(k, _)| k + 1).filter(|&k| k < n).collect();
    for _ in 0..n + 2 {
        let mut changed = false;
        for k in 0..n {
            let i = &instrs[k];
            let inn = undef_in[k];
            if i.rflags_read() & inn != 0 {
                reads_undef = true;
            }
            let cl_count = matches!(i.mnemonic(), Mnemonic::Shl | Mnemonic::Sal | Mnemonic::Shr | Mnemonic::Sar | Mnemonic::Rol | Mnemonic::Ror | Mnemonic::Rcl | Mnemonic::Rcr | Mnemonic::Shld | Mnemonic::Shrd)
                && (0..i.op_count()).any(|m| i.op_kind(m) == OpKind::Register && i.op_register(m) == Register::CL);
            if cl_count && inn != 0 {
                reads_undef = true;
            }
            let out = (inn & !i.rflags_modified()) | i.rflags_undefined();
            // Straight-line cases omit the trailing hlt: the last instruction is terminal.
            if i.mnemonic() == Mnemonic::Hlt || k + 1 == n {
                out_at_hlt |= out;
                continue;
            }
            let mut succ: Vec<usize> = Vec::new();
            let target = || index.get(&i.near_branch_target()).copied();
            match i.flow_control() {
                FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                    if let Some(t) = target() { succ.push(t); }
                    succ.push(k + 1);
                }
                FlowControl::ConditionalBranch => {
                    if let Some(t) = target() { succ.push(t); }
                    succ.push(k + 1);
                }
                FlowControl::UnconditionalBranch => {
                    if let Some(t) = target() { succ.push(t); }
                }
                FlowControl::Return => succ.extend(after_calls.iter().copied()),
                _ => {}
            }
            for t in succ {
                if t < n && undef_in[t] | out != undef_in[t] {
                    undef_in[t] |= out;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    (out_at_hlt, reads_undef)
}

/// Flags left undefined when the program reaches `hlt`, as EFLAGS bits.
pub fn undefined_flags(instrs: &[Instruction]) -> u32 {
    let (undef, _) = undefined_analysis(instrs);
    let mut bits = 0;
    if undef & RflagsBits::CF != 0 { bits |= 1 << 0; }
    if undef & RflagsBits::PF != 0 { bits |= 1 << 2; }
    if undef & RflagsBits::AF != 0 { bits |= 1 << 4; }
    if undef & RflagsBits::ZF != 0 { bits |= 1 << 6; }
    if undef & RflagsBits::SF != 0 { bits |= 1 << 7; }
    if undef & RflagsBits::OF != 0 { bits |= 1 << 11; }
    bits
}

/// True when some instruction may read a flag left undefined by an earlier one: the
/// outcome is not specified, so native and wasm may legitimately differ.
pub fn reads_undefined_flag(instrs: &[Instruction]) -> bool {
    undefined_analysis(instrs).1
}

pub struct Diff {
    pub field: String,
    pub native: String,
    pub wasm: String,
}

/// How much of the FXSAVE image to compare.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FxCompare {
    None,
    Xmm,
    /// x87 registers bit-exact.
    X87,
    /// x87 registers within ~53 bits (transcendental results), status C1/PE ignored.
    X87Approx,
    /// xmm registers and the x87 register images MMX writes (tags not compared: an
    /// MMX instruction marks all eight valid, the translation only those it writes).
    Mmx,
}

fn ext_to_f64(b: &[u8]) -> f64 {
    let mant = u64::from_le_bytes(b[..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (se & 0x7fff) as i32;
    if exp == 0x7fff {
        return if mant << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
    }
    sign * (mant as f64) * 2f64.powi(exp - 16383 - 63)
}

pub fn compare(native: &CaseOut, wasm: &CaseOut, instrs: &[Instruction], fx: FxCompare) -> Vec<Diff> {
    let compare_xmm = matches!(fx, FxCompare::Xmm | FxCompare::Mmx);
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
            if compare_xmm && nan_chunk(&native.data, &wasm.data, i) {
                continue; // a stored NaN whose payload wasm does not reproduce
            }
            push!(&format!("data[{i:#x}]"), format!("{a:#04x}"), format!("{b:#04x}"));
            if diffs.len() > 64 { break; }
        }
    }
    for (i, (a, b)) in native.stack.iter().zip(&wasm.stack).enumerate() {
        if a != b {
            push!(&format!("stack[{i:#x}]"), format!("{a:#04x}"), format!("{b:#04x}"));
            if diffs.len() > 64 { break; }
        }
    }
    if matches!(fx, FxCompare::X87 | FxCompare::X87Approx) {
        let (n, w) = (&native.fx, &wasm.fx);
        push!("fcw", format!("{:#06x}", u16::from_le_bytes([n[0], n[1]])), format!("{:#06x}", u16::from_le_bytes([w[0], w[1]])));
        let sw_mask: u16 = if fx == FxCompare::X87Approx { !(0x0200 | 0x0020) } else { 0xffff };
        push!("fsw", format!("{:#06x}", u16::from_le_bytes([n[2], n[3]]) & sw_mask), format!("{:#06x}", u16::from_le_bytes([w[2], w[3]]) & sw_mask));
        push!("ftw", format!("{:#04x}", n[4]), format!("{:#04x}", w[4]));
        let top = ((u16::from_le_bytes([n[2], n[3]]) >> 11) & 7) as usize;
        for i in 0..8 {
            // FXSAVE stores ST(i) raw, empty or not: popped values stay comparable.
            let phys = (top + i) & 7;
            if fx == FxCompare::X87Approx && n[4] & (1 << phys) == 0 && w[4] & (1 << phys) == 0 {
                continue;
            }
            let a = &n[32 + 16 * i..42 + 16 * i];
            let b = &w[32 + 16 * i..42 + 16 * i];
            if fx == FxCompare::X87Approx {
                let (fa, fb) = (ext_to_f64(a), ext_to_f64(b));
                let ok = (fa.is_nan() && fb.is_nan()) || fa == fb || ((fa - fb).abs() <= fa.abs().max(fb.abs()) * 2f64.powi(-44));
                if !ok {
                    push!(&format!("st{i}~"), hex(a), hex(b));
                }
            } else {
                push!(&format!("st{i}"), hex(a), hex(b));
            }
        }
    }
    if fx == FxCompare::Mmx {
        for i in 0..8 {
            let a = &native.fx[32 + 16 * i..42 + 16 * i];
            let b = &wasm.fx[32 + 16 * i..42 + 16 * i];
            if a != b {
                push!(&format!("mm{i}/st"), hex(a), hex(b));
            }
        }
    }
    if compare_xmm {
        // MXCSR exception flags (bits 0-5) are not modelled; compare the control bits only.
        push!("mxcsr", format!("{:#x}", u32::from_le_bytes(native.fx[24..28].try_into().unwrap()) & !0x3f), format!("{:#x}", u32::from_le_bytes(wasm.fx[24..28].try_into().unwrap()) & !0x3f));
        for x in 0..8 {
            let a = &native.fx[160 + x * 16..176 + x * 16];
            let b = &wasm.fx[160 + x * 16..176 + x * 16];
            if !xmm_equal_modulo_nan(a, b) {
                push!(&format!("xmm{x}"), hex(a), hex(b));
            }
        }
    }
    diffs
}

/// Byte `i` lies in a 4- or 8-byte aligned chunk that holds a NaN on both sides.
fn nan_chunk(a: &[u8], b: &[u8], i: usize) -> bool {
    let c8 = i & !7;
    if c8 + 8 <= a.len() {
        let (x, y) = (u64::from_le_bytes(a[c8..c8 + 8].try_into().unwrap()), u64::from_le_bytes(b[c8..c8 + 8].try_into().unwrap()));
        if f64::from_bits(x).is_nan() && f64::from_bits(y).is_nan() {
            return true;
        }
    }
    let c4 = i & !3;
    if c4 + 4 <= a.len() {
        let (x, y) = (u32::from_le_bytes(a[c4..c4 + 4].try_into().unwrap()), u32::from_le_bytes(b[c4..c4 + 4].try_into().unwrap()));
        if f32::from_bits(x).is_nan() && f32::from_bits(y).is_nan() {
            return true;
        }
    }
    false
}

/// Lanes that are NaN on both sides compare equal whatever their payloads, at 32- and
/// 64-bit granularity (the harness cannot know the lane type).
fn xmm_equal_modulo_nan(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    let f64_nan = |x: &[u8]| f64::from_bits(u64::from_le_bytes(x.try_into().unwrap())).is_nan();
    let f32_nan = |x: &[u8]| f32::from_bits(u32::from_le_bytes(x.try_into().unwrap())).is_nan();
    let by64 = (0..2).all(|l| a[8 * l..8 * l + 8] == b[8 * l..8 * l + 8] || (f64_nan(&a[8 * l..8 * l + 8]) && f64_nan(&b[8 * l..8 * l + 8])));
    let by32 = (0..4).all(|l| a[4 * l..4 * l + 4] == b[4 * l..4 * l + 4] || (f32_nan(&a[4 * l..4 * l + 4]) && f32_nan(&b[4 * l..4 * l + 4])));
    by64 || by32
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
