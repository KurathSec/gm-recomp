//! Instruction-form histogram over discovered code, one or more PE files:
//! `cargo run -p x86 --example histo -- a.exe b.dll ... [--json out.json]`
//!
//! A "form" is mnemonic + operand kinds + operand size + prefixes, which is the
//! granularity the lifter and the differential tests are organized by.

use iced_x86::{Decoder, DecoderOptions, Instruction, OpKind, Register};
use std::collections::BTreeMap;

fn form(i: &Instruction) -> String {
    let mut s = format!("{:?}", i.mnemonic());
    for n in 0..i.op_count() {
        s.push(if n == 0 { ' ' } else { ',' });
        s.push_str(&match i.op_kind(n) {
            OpKind::Register => {
                let r = i.op_register(n);
                if r.is_gpr8() { "r8" } else if r.is_gpr16() { "r16" } else if r.is_gpr32() { "r32" }
                else if r.is_xmm() { "xmm" } else if r.is_mm() { "mm" } else if r.is_st() { "st" }
                else if r.is_segment_register() { "sreg" } else { "reg" }
                .to_string()
            }
            OpKind::Memory => {
                let sz = i.memory_size().size();
                let seg = match i.segment_prefix() { Register::FS => "fs:", Register::GS => "gs:", _ => "" };
                format!("{seg}m{}", sz * 8)
            }
            OpKind::Immediate8 | OpKind::Immediate8_2nd => "i8".into(),
            OpKind::Immediate16 => "i16".into(),
            OpKind::Immediate32 => "i32".into(),
            OpKind::Immediate8to16 | OpKind::Immediate8to32 => "i8s".into(),
            OpKind::NearBranch16 | OpKind::NearBranch32 => "rel".into(),
            OpKind::FarBranch16 | OpKind::FarBranch32 => "far".into(),
            OpKind::MemorySegSI | OpKind::MemorySegESI | OpKind::MemoryESDI | OpKind::MemoryESEDI => "str".into(),
            other => format!("{other:?}"),
        });
    }
    if i.has_rep_prefix() { s.push_str(" rep"); }
    if i.has_repne_prefix() { s.push_str(" repne"); }
    if i.has_lock_prefix() { s.push_str(" lock"); }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json_out = args.iter().position(|a| a == "--json").map(|p| args[p + 1].clone());
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--") && json_out.as_ref() != Some(*a)).collect();
    let mut forms: BTreeMap<String, u64> = BTreeMap::new();
    let mut mnemonics: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    for path in files {
        let bytes = std::fs::read(path).expect("read");
        let pe = pe::Pe::parse(&bytes).expect("parse");
        let d = x86::discover(&pe);
        let mut image = vec![0u8; pe.size_of_image as usize];
        for s in &pe.sections {
            let n = (s.raw_size as usize).min(s.mapped_size() as usize);
            if let Some(src) = bytes.get(s.raw_offset as usize..s.raw_offset as usize + n) {
                image[s.virtual_address as usize..s.virtual_address as usize + n].copy_from_slice(src);
            }
        }
        let mut decoder = Decoder::with_ip(32, &image, 0, DecoderOptions::NONE);
        let mut instr = Instruction::default();
        for &rva in &d.instruction_starts {
            decoder.set_position(rva as usize).unwrap();
            decoder.set_ip(rva as u64);
            decoder.decode_out(&mut instr);
            *forms.entry(form(&instr)).or_default() += 1;
            *mnemonics.entry(format!("{:?}", instr.mnemonic())).or_default() += 1;
            total += 1;
        }
        eprintln!("{path}: {} instructions", d.instruction_starts.len());
    }
    let mut by_count: Vec<(&String, &u64)> = forms.iter().collect();
    by_count.sort_by_key(|&(_, &c)| std::cmp::Reverse(c));
    println!("{total} instructions, {} mnemonics, {} forms", mnemonics.len(), forms.len());
    let mut cum = 0u64;
    for (i, &(f, &c)) in by_count.iter().enumerate() {
        cum += c;
        if i < 60 || c >= 1000 && i < 200 {
            println!("{c:>9} {:5.1}% cum {:5.1}%  {f}", 100.0 * c as f64 / total as f64, 100.0 * cum as f64 / total as f64);
        }
    }
    let mut by_mn: Vec<(&String, &u64)> = mnemonics.iter().collect();
    by_mn.sort_by_key(|&(_, &c)| std::cmp::Reverse(c));
    println!("\nmnemonics by count:");
    println!("{}", by_mn.iter().map(|(m, c)| format!("{m}:{c}")).collect::<Vec<_>>().join(" "));
    if let Some(p) = json_out {
        std::fs::write(&p, serde_json::to_string_pretty(&forms).unwrap()).unwrap();
        eprintln!("wrote {p}");
    }
}
