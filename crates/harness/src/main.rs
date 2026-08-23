mod compare;
mod casegen;
mod native;
mod proto;
mod wasm;

use anyhow::Result;
use clap::{Parser, Subcommand};
use iced_x86::{Code, FastFormatter, Mnemonic};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "CPU core differential tests and native debugging tools")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate random instruction sequences, run them natively (Wine) and in wasm, compare.
    CpuDiff {
        #[arg(long, default_value_t = 2000)]
        count: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Instructions per case (upper bound).
        #[arg(long, default_value_t = 3)]
        max_instrs: usize,
        /// Only these mnemonics (comma separated, iced names, e.g. "Add,Sub").
        #[arg(long)]
        mnemonics: Option<String>,
        /// Only codes whose iced `Code` name contains this text.
        #[arg(long)]
        code_filter: Option<String>,
        #[arg(long, default_value = "target/refrun")]
        work: PathBuf,
        /// Show this many failing cases in detail.
        #[arg(long, default_value_t = 8)]
        show: usize,
        /// Print the list of codes under test and exit.
        #[arg(long)]
        list: bool,
    },
    /// Print a .wasm file as text (for inspecting failed modules).
    Wat { path: PathBuf },
    /// Translate raw x86 bytes (hex) as a test program and print the wasm text.
    Translate { hex: String },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Wat { path } => {
            let bytes = std::fs::read(&path)?;
            println!("{}", wasmprinter::print_bytes(&bytes).map_err(|e| anyhow::anyhow!("{e}"))?);
            Ok(())
        }
        Cmd::Translate { hex } => {
            let mut code: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<_, _>>()?;
            code.push(0xF4);
            let case = proto::CaseIn::new(code);
            let runner = wasm::Runner::new()?;
            let out = runner.translate(&case, 0x2000_2000)?;
            for (ip, text) in &out.unimplemented {
                println!(";; unimplemented at {ip:#x}: {text}");
            }
            println!("{}", wasmprinter::print_bytes(&out.wasm).map_err(|e| anyhow::anyhow!("{e}"))?);
            Ok(())
        }
        Cmd::CpuDiff { count, seed, max_instrs, mnemonics, code_filter, work, show, list } => {
            let mn: Vec<Mnemonic> = match mnemonics {
                Some(s) => {
                    let all = casegen::integer_mnemonics();
                    s.split(',')
                        .map(|name| {
                            all.iter().copied().find(|m| format!("{m:?}").eq_ignore_ascii_case(name.trim()))
                                .unwrap_or_else(|| panic!("unknown or unsupported mnemonic {name}"))
                        })
                        .collect()
                }
                None => casegen::integer_mnemonics(),
            };
            let mut codes = casegen::supported_codes(&mn);
            if let Some(f) = &code_filter {
                codes.retain(|c| format!("{c:?}").contains(f.as_str()));
            }
            if list {
                for c in &codes {
                    println!("{c:?}");
                }
                println!("{} codes", codes.len());
                return Ok(());
            }
            anyhow::ensure!(!codes.is_empty(), "no codes selected");
            let repo = std::env::current_dir()?;
            let nat = native::Native::prepare(&repo, &work)?;
            let runner = wasm::Runner::new()?;
            let mut g = casegen::Gen::new(seed);
            let mut cases = Vec::new();
            while cases.len() < count {
                if let Some(c) = g.case(&codes, max_instrs) {
                    cases.push(c);
                }
            }
            let inputs: Vec<proto::CaseIn> = cases.iter().map(|c| c.input.clone()).collect();
            let t0 = std::time::Instant::now();
            let native_out = nat.run(&inputs)?;
            let t_native = t0.elapsed();
            let stack_base = native_out[0].stack_base;

            let mut per_code: BTreeMap<Code, (u32, u32)> = BTreeMap::new();
            let mut shown = 0;
            let mut failures = 0;
            let mut unimpl: BTreeMap<String, u32> = BTreeMap::new();
            let t1 = std::time::Instant::now();
            let mut fmt = FastFormatter::new();
            for (k, case) in cases.iter().enumerate() {
                let res = match runner.run(&case.input, stack_base) {
                    Ok(r) => r,
                    Err(e) => {
                        failures += 1;
                        for &c in &case.codes {
                            per_code.entry(c).or_insert((0, 0)).1 += 1;
                        }
                        if shown < show {
                            shown += 1;
                            println!("--- case {k} (seed {seed}) TRANSLATION/RUN ERROR: {e:#}");
                            for i in &case.instrs {
                                let mut s = String::new();
                                fmt.format(i, &mut s);
                                println!("    {:08x}  {s}", i.ip());
                            }
                        }
                        continue;
                    }
                };
                let diffs = compare::compare(&native_out[k], &res.out, &case.instrs, false);
                for &c in &case.codes {
                    let e = per_code.entry(c).or_insert((0, 0));
                    if diffs.is_empty() { e.0 += 1 } else { e.1 += 1 }
                }
                if res.out.status == 0xFFFF_0001 {
                    for (_, text) in &res.unimplemented {
                        *unimpl.entry(text.clone()).or_default() += 1;
                    }
                }
                if !diffs.is_empty() {
                    failures += 1;
                    if shown < show {
                        shown += 1;
                        println!("--- case {k} (seed {seed}) FAILED");
                        for i in &case.instrs {
                            let mut s = String::new();
                            fmt.format(i, &mut s);
                            println!("    {:08x}  {s}", i.ip());
                        }
                        let r = &case.input.regs;
                        println!("    in: eax={:08x} ecx={:08x} edx={:08x} ebx={:08x} esp=+{:x} ebp={:08x} esi={:08x} edi={:08x} eflags={:#x}",
                            r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], case.input.eflags);
                        for d in &diffs {
                            println!("    {:<12} native {:<22} wasm {}", d.field, d.native, d.wasm);
                        }
                        if let Some(e) = &res.wasm_error {
                            println!("    wasm error: {e}");
                        }
                    }
                }
            }
            let t_wasm = t1.elapsed();
            println!("\n{} cases: {} passed, {} failed  (native {:.1}s, wasm {:.1}s)", count, count - failures, failures, t_native.as_secs_f64(), t_wasm.as_secs_f64());
            let mut failing: Vec<(&Code, &(u32, u32))> = per_code.iter().filter(|(_, v)| v.1 > 0).collect();
            failing.sort_by_key(|(_, v)| std::cmp::Reverse(v.1));
            if !failing.is_empty() {
                println!("codes with failures (pass/fail):");
                for (c, (p, f)) in failing.iter().take(40) {
                    println!("  {c:?}: {p}/{f}");
                }
            }
            if !unimpl.is_empty() {
                println!("unimplemented instructions hit:");
                for (t, n) in unimpl.iter().take(40) {
                    println!("  {n:>5}  {t}");
                }
            }
            println!("{} codes tested", per_code.len());
            Ok(())
        }
    }
}
