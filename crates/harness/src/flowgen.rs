//! Small random programs with real control flow: branches, loops, calls and
//! returns, stack traffic and string operations.

use crate::casegen::{Case, Gen};
use crate::proto::{CODE_BASE, DATA_BASE};
use iced_x86::code_asm::{CodeAssembler, CodeLabel};
use iced_x86::{BlockEncoderOptions, Code, Decoder, DecoderOptions, Instruction, Register};
use rand::RngExt;

fn jcc(a: &mut CodeAssembler, k: u32, l: CodeLabel) -> Result<(), iced_x86::IcedError> {
    match k % 16 {
        0 => a.jo(l),
        1 => a.jno(l),
        2 => a.jb(l),
        3 => a.jae(l),
        4 => a.je(l),
        5 => a.jne(l),
        6 => a.jbe(l),
        7 => a.ja(l),
        8 => a.js(l),
        9 => a.jns(l),
        10 => a.jp(l),
        11 => a.jnp(l),
        12 => a.jl(l),
        13 => a.jge(l),
        14 => a.jle(l),
        _ => a.jg(l),
    }
}

pub fn flow_case(g: &mut Gen, codes: &[Code]) -> Option<Case> {
    let mut a = CodeAssembler::new(32).ok()?;
    let mut written: Vec<Register> = Vec::new();
    let mut used = Vec::new();
    let mut entries = Vec::new();
    let fill_opt = |a: &mut CodeAssembler, g: &mut Gen, written: &mut Vec<Register>, used: &mut Vec<Code>, stack: bool| -> Option<()> {
        let (i, c) = g.filler(codes, written, stack)?;
        a.add_instruction(i).ok()?;
        used.push(c);
        Some(())
    };
    let fill = |a: &mut CodeAssembler, g: &mut Gen, written: &mut Vec<Register>, used: &mut Vec<Code>| fill_opt(a, g, written, used, true);
    let template = g.rng.random_range(0..7);
    let mut call_targets: Vec<CodeLabel> = Vec::new();
    match template {
        0 => {
            // flags; jcc L1; A; jmp L2; L1: B; L2: C
            let mut l1 = a.create_label();
            let mut l2 = a.create_label();
            fill(&mut a, g, &mut written, &mut used)?;
            jcc(&mut a, g.rng.random(), l1).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
            a.jmp(l2).ok()?;
            a.set_label(&mut l1).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
            a.set_label(&mut l2).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
        }
        1 => {
            // counted loop with dec/jnz
            use iced_x86::code_asm::registers::*;
            let mut top = a.create_label();
            a.mov(ecx, g.rng.random_range(1u32..6)).ok()?;
            written.push(Register::ECX);
            a.set_label(&mut top).ok()?;
            a.add(eax, ecx).ok()?;
            a.xor(edx, eax).ok()?;
            written.push(Register::EAX);
            written.push(Register::EDX);
            a.dec(ecx).ok()?;
            a.jne(top).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
        }
        2 => {
            // loop / loope / loopne / jecxz
            use iced_x86::code_asm::registers::*;
            let mut top = a.create_label();
            let mut out = a.create_label();
            a.mov(ecx, g.rng.random_range(0u32..5)).ok()?;
            written.push(Register::ECX);
            a.jecxz(out).ok()?;
            a.set_label(&mut top).ok()?;
            a.add(eax, 3u32).ok()?;
            written.push(Register::EAX);
            a.cmp(eax, 0x5555_5560u32).ok()?;
            match g.rng.random_range(0..3) {
                0 => a.loop_(top).ok()?,
                1 => a.loope(top).ok()?,
                _ => a.loopne(top).ok()?,
            }
            a.set_label(&mut out).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
        }
        3 => {
            // call F; A; hlt; F: B; ret
            let mut f = a.create_label();
            a.call(f).ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
            a.hlt().ok()?;
            a.set_label(&mut f).ok()?;
            call_targets.push(f);
            fill(&mut a, g, &mut written, &mut used)?;
            if g.rng.random_bool(0.3) {
                use iced_x86::code_asm::registers::*;
                a.push(ebp).ok()?;
                a.mov(ebp, esp).ok()?;
                a.sub(esp, 8u32).ok()?;
                a.mov(iced_x86::code_asm::dword_ptr(ebp - 4), eax).ok()?;
                a.leave().ok()?;
            }
            a.ret().ok()?;
        }
        4 => {
            // call through a register, callee returns with ret imm16 after pushes
            use iced_x86::code_asm::registers::*;
            let mut f = a.create_label();
            a.push(1u32).ok()?;
            a.push(2u32).ok()?;
            a.mov(eax, 0u32).ok()?; // patched below with the absolute address of F
            a.call(eax).ok()?;
            written.push(Register::EAX);
            fill(&mut a, g, &mut written, &mut used)?;
            a.hlt().ok()?;
            a.set_label(&mut f).ok()?;
            call_targets.push(f);
            a.mov(edx, iced_x86::code_asm::dword_ptr(esp + 4)).ok()?;
            a.add(edx, iced_x86::code_asm::dword_ptr(esp + 8)).ok()?;
            written.push(Register::EDX);
            a.ret_1(8).ok()?;
        }
        5 => {
            // stack traffic
            use iced_x86::code_asm::registers::*;
            match g.rng.random_range(0..3) {
                0 => {
                    a.pushad().ok()?;
                    fill_opt(&mut a, g, &mut written, &mut used, false)?;
                    a.popad().ok()?;
                }
                1 => {
                    a.pushfd().ok()?;
                    fill_opt(&mut a, g, &mut written, &mut used, false)?;
                    // keep only the arithmetic flags: TF/AC would single-step or fault natively
                    a.and(iced_x86::code_asm::dword_ptr(esp), 0x8d5u32).ok()?;
                    a.popfd().ok()?;
                }
                _ => {
                    a.push(eax).ok()?;
                    a.push(iced_x86::code_asm::dword_ptr(ebx + 0x10)).ok()?;
                    a.pop(edx).ok()?;
                    a.pop(iced_x86::code_asm::dword_ptr(ebx + 0x14)).ok()?;
                    written.push(Register::EDX);
                }
            }
            fill(&mut a, g, &mut written, &mut used)?;
        }
        _ => {
            // string operations on the data block
            use iced_x86::code_asm::registers::*;
            a.mov(ecx, g.rng.random_range(0u32..12)).ok()?;
            written.push(Register::ECX);
            if g.rng.random_bool(0.3) { a.std().ok()?; } else { a.cld().ok()?; }
            let k = g.rng.random_range(0..9);
            let i = match k {
                0 => Instruction::with_rep_movsb(32),
                1 => Instruction::with_rep_movsd(32),
                2 => Instruction::with_rep_stosb(32),
                3 => Instruction::with_rep_stosd(32),
                4 => Instruction::with_repe_cmpsb(32),
                5 => Instruction::with_repne_scasb(32),
                6 => Instruction::with_rep_lodsd(32),
                7 => Instruction::with_movsd(32, Register::None, iced_x86::RepPrefixKind::None),
                _ => Instruction::with_repe_cmpsd(32),
            }
            .ok()?;
            a.add_instruction(i).ok()?;
            written.push(Register::ESI);
            written.push(Register::EDI);
            a.cld().ok()?;
            fill(&mut a, g, &mut written, &mut used)?;
        }
    }
    a.hlt().ok()?;
    let res = a.assemble_options(CODE_BASE as u64, BlockEncoderOptions::NONE).ok()?;
    let mut code = res.inner.code_buffer.clone();
    for l in &call_targets {
        entries.push(res.label_ip(l).ok()? as u32);
    }
    if template == 4 {
        // patch `mov eax, 0` (B8 00 00 00 00) after the two pushes with F's address
        let f = entries[0];
        let pos = code.windows(5).position(|w| w == [0xB8, 0, 0, 0, 0])?;
        code[pos + 1..pos + 5].copy_from_slice(&f.to_le_bytes());
    }
    let mut input = g.fresh_input(code.clone());
    // string ops: keep ESI/EDI inside the block even when DF=1
    input.regs[6] = DATA_BASE + 0x300;
    input.regs[7] = DATA_BASE + 0x400;
    let mut dec = Decoder::with_ip(32, &code, CODE_BASE as u64, DecoderOptions::NONE);
    let instrs: Vec<Instruction> = dec.iter().collect();
    Some(Case { input, instrs, codes: used, entries })
}
