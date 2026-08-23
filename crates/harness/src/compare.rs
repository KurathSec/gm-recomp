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
            if diffs.len() > 64 { break; }
        }
    }
    for (i, (a, b)) in native.stack.iter().zip(&wasm.stack).enumerate() {
        if a != b {
            push!(&format!("stack[{i:#x}]"), format!("{a:#04x}"), format!("{b:#04x}"));
            if diffs.len() > 64 { break; }
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
