mod casegen;
mod compare;
mod flowgen;
mod ssegen;
mod x87gen;
mod native;
mod proto;
mod run;
mod wasm;

use anyhow::Result;
use clap::{Parser, Subcommand};
use iced_x86::{Code, FastFormatter, Mnemonic};
use rand::RngExt;
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
        /// Case shape: "instr" (random sequences), "flow" (branches/calls/loops/strings) or "mixed".
        #[arg(long, default_value = "mixed")]
        mode: String,
        /// Run and show only this case index (after generation).
        #[arg(long)]
        only: Option<usize>,
        /// With --only: also print this data-block range, as off:len (hex).
        #[arg(long)]
        dump_data: Option<String>,
    },
    /// Run one x87 program natively and in wasm with chosen ST registers, and print both states.
    Probe {
        /// Code bytes in hex (hlt is appended).
        hex: String,
        /// Initial ST(0), ST(1), ... as 20 hex digits (little-endian 80-bit images).
        #[arg(long)]
        st: Vec<String>,
        #[arg(long, default_value = "037f")]
        cw: String,
        /// Data block contents as off=hexbytes (offset in hex, relative to the data block).
        #[arg(long)]
        data: Vec<String>,
        /// Also print this data range after the run, as off:len (hex).
        #[arg(long)]
        dump: Option<String>,
        #[arg(long, default_value = "target/refrun")]
        work: PathBuf,
    },
    /// Run an imported game (web/public/games/<slug>) natively with the HLE runtime.
    Run {
        game_dir: PathBuf,
        /// Entry executable (defaults to the manifest's entry).
        #[arg(long)]
        exe: Option<String>,
        /// Print every API call.
        #[arg(long)]
        trace: bool,
        /// Stop after this many Present calls.
        #[arg(long, default_value_t = 1)]
        frames: u64,
        /// Translate every discovered function up front instead of on demand.
        #[arg(long)]
        aot: bool,
        /// Stop after this many API calls (runaway protection).
        #[arg(long, default_value_t = 50_000_000)]
        max_api_calls: u64,
        /// Do not print the log tail at the end.
        #[arg(long)]
        quiet: bool,
        /// Wall-clock limit in seconds.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Show the last N dispatcher targets at the end.
        #[arg(long, default_value_t = 0)]
        recent: usize,
        /// Hex-dump guest memory at the end, as addr:len (hex), repeatable.
        #[arg(long)]
        dump: Vec<String>,
        /// Keep files the game wrote in earlier runs.
        #[arg(long)]
        keep_writes: bool,
    },
    /// Print a .wasm file as text (for inspecting failed modules).
    Wat { path: PathBuf },
    /// Translate raw x86 bytes (hex) as a test program and print the wasm text.
    Translate { hex: String },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Probe { hex, st, cw, data, dump, work } => {
            let mut code: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<_, _>>()?;
            code.push(0xF4);
            let mut case = proto::CaseIn::new(code);
            case.regs[3] = proto::DATA_BASE + 0x200;
            case.regs[4] = 0x200;
            case.fx[0..2].copy_from_slice(&u16::from_str_radix(&cw, 16)?.to_le_bytes());
            let mut tags = 0u8;
            for (i, h) in st.iter().enumerate() {
                let bytes: Vec<u8> = (0..h.len()).step_by(2).map(|k| u8::from_str_radix(&h[k..k + 2], 16)).collect::<Result<_, _>>()?;
                case.fx[32 + 16 * i..32 + 16 * i + bytes.len()].copy_from_slice(&bytes);
                tags |= 1 << i;
            }
            case.fx[4] = tags;
            for d in &data {
                let (o, h) = d.split_once('=').expect("--data off=hex");
                let o = usize::from_str_radix(o.trim_start_matches("0x"), 16)?;
                let bytes: Vec<u8> = (0..h.len()).step_by(2).map(|k| u8::from_str_radix(&h[k..k + 2], 16)).collect::<Result<_, _>>()?;
                case.data[o..o + bytes.len()].copy_from_slice(&bytes);
            }
            let repo = std::env::current_dir()?;
            let nat = native::Native::prepare(&repo, &work)?;
            let runner = wasm::Runner::new()?;
            let n = nat.run(std::slice::from_ref(&case))?.remove(0);
            let w = runner.run(&case, n.stack_base, &[])?.out;
            for (label, o) in [("native", &n), ("wasm  ", &w)] {
                println!("{label}: status={:#x} fcw={:#06x} fsw={:#06x} ftw={:#04x} st: {}", o.status, u16::from_le_bytes([o.fx[0], o.fx[1]]), u16::from_le_bytes([o.fx[2], o.fx[3]]), o.fx[4], (0..8).map(|r| compare::hex(&o.fx[32 + 16 * r..42 + 16 * r])).collect::<Vec<_>>().join(" "));
                if let Some(d) = &dump {
                    let (o2, l) = d.split_once(':').unwrap_or((d, "16"));
                    let (o2, l) = (usize::from_str_radix(o2.trim_start_matches("0x"), 16)?, usize::from_str_radix(l.trim_start_matches("0x"), 16)?);
                    println!("        data[{o2:#x}..] {}", compare::hex(&o.data[o2..o2 + l]));
                }
            }
            Ok(())
        }
        Cmd::Run { game_dir, exe, trace, frames, aot, max_api_calls, quiet, seconds, recent, dump, keep_writes } => {
            let dump = dump
                .iter()
                .map(|d| {
                    let (a, l) = d.split_once(':').ok_or_else(|| anyhow::anyhow!("--dump wants addr:len"))?;
                    Ok((u32::from_str_radix(a.trim_start_matches("0x"), 16)?, u32::from_str_radix(l.trim_start_matches("0x"), 16)?))
                })
                .collect::<Result<Vec<_>>>()?;
            run::run(run::RunOpts { game_dir, exe, trace, frames, aot, max_api_calls, quiet, seconds, recent, dump, keep_writes })
        }
        Cmd::Wat { path } => {
            let bytes = std::fs::read(&path)?;
            let mut cfg = wasmprinter::Config::new();
            cfg.print_offsets(true);
            let mut text = String::new();
            cfg.print(&bytes, &mut wasmprinter::PrintFmtWrite(&mut text)).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{text}");
            let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
            match v.validate_all(&bytes) {
                Ok(_) => println!(";; valid"),
                Err(e) => println!(";; INVALID: {e} (offset {:#x})", e.offset()),
            }
            Ok(())
        }
        Cmd::Translate { hex } => {
            let mut code: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<_, _>>()?;
            code.push(0xF4);
            let case = proto::CaseIn::new(code);
            let runner = wasm::Runner::new()?;
            let out = runner.translate(&case, 0x2000_2000, &[])?;
            for (ip, text) in &out.unimplemented {
                println!(";; unimplemented at {ip:#x}: {text}");
            }
            println!("{}", wasmprinter::print_bytes(&out.wasm).map_err(|e| anyhow::anyhow!("{e}"))?);
            Ok(())
        }
        Cmd::CpuDiff { count, seed, max_instrs, mnemonics, code_filter, work, show, list, mode, only, dump_data } => {
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
            let mut codes = if mode == "sse" {
                ssegen::sse_codes(&ssegen::sse_mnemonics())
            } else if mode == "x87" {
                x87gen::x87_codes(&x87gen::x87_mnemonics())
            } else if mode == "x87trans" {
                x87gen::x87_codes(&x87gen::transcendental_mnemonics())
            } else {
                casegen::supported_codes(&mn)
            };
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
                let c = match mode.as_str() {
                    "sse" => ssegen::sse_case(&mut g, &codes, max_instrs),
                    "x87" => x87gen::x87_case(&mut g, &codes, max_instrs),
                    "x87trans" => x87gen::x87_case_mode(&mut g, &codes, max_instrs, true),
                    "flow" => flowgen::flow_case(&mut g, &codes),
                    "instr" => g.case(&codes, max_instrs),
                    _ => if g.rng.random_bool(0.4) { flowgen::flow_case(&mut g, &codes) } else { g.case(&codes, max_instrs) },
                };
                if let Some(c) = c {
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
            let mut indeterminate = 0;
            let mut unimpl: BTreeMap<String, u32> = BTreeMap::new();
            let t1 = std::time::Instant::now();
            let mut fmt = FastFormatter::new();
            for (k, case) in cases.iter().enumerate() {
                if only.is_some_and(|o| o != k) {
                    continue;
                }
                let res = match runner.run(&case.input, stack_base, &case.entries) {
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
                let fxmode = match mode.as_str() {
                    "sse" => compare::FxCompare::Xmm,
                    "x87" => compare::FxCompare::X87,
                    "x87trans" => compare::FxCompare::X87Approx,
                    _ => compare::FxCompare::None,
                };
                let mut diffs = compare::compare(&native_out[k], &res.out, &case.instrs, fxmode);
                if !diffs.is_empty() && compare::reads_undefined_flag(&case.instrs) {
                    indeterminate += 1;
                    diffs.clear();
                }
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
                        if only.is_some() {
                            let (n, w) = (&native_out[k], &res.out);
                            println!("    native out: regs={:08x?} eflags={:#x} status={:#x} off={}", n.regs, n.eflags, n.status, n.fault_off);
                            println!("    wasm   out: regs={:08x?} eflags={:#x} status={:#x} off={}", w.regs, w.eflags, w.status, w.fault_off);
                            let i = &case.input;
                            println!("    data[0x2f8..0x310] in  {}", compare::hex(&i.data[0x2f8..0x310]));
                            println!("    data[0x3f8..0x410] in  {}", compare::hex(&i.data[0x3f8..0x410]));
                            println!("    data[0x3f8..0x410] nat {}", compare::hex(&n.data[0x3f8..0x410]));
                            println!("    data[0x3f8..0x410] was {}", compare::hex(&w.data[0x3f8..0x410]));
                            if let Some(d) = &dump_data {
                                let (o, l) = d.split_once(':').unwrap_or((d, "16"));
                                let (o, l) = (usize::from_str_radix(o.trim_start_matches("0x"), 16).unwrap(), usize::from_str_radix(l.trim_start_matches("0x"), 16).unwrap());
                                println!("    data[{o:#x}..] in  {}", compare::hex(&i.data[o..o + l]));
                                println!("    data[{o:#x}..] nat {}", compare::hex(&n.data[o..o + l]));
                                println!("    data[{o:#x}..] was {}", compare::hex(&w.data[o..o + l]));
                            }
                            println!("    fx in : {} st: {}", compare::hex(&i.fx[..8]), (0..8).map(|r| compare::hex(&i.fx[32 + 16 * r..42 + 16 * r])).collect::<Vec<_>>().join(" "));
                            println!("    fx nat: {} st: {}", compare::hex(&n.fx[..8]), (0..8).map(|r| compare::hex(&n.fx[32 + 16 * r..42 + 16 * r])).collect::<Vec<_>>().join(" "));
                            println!("    fx was: {} st: {}", compare::hex(&w.fx[..8]), (0..8).map(|r| compare::hex(&w.fx[32 + 16 * r..42 + 16 * r])).collect::<Vec<_>>().join(" "));
                        }
                        if let Some(e) = &res.wasm_error {
                            println!("    wasm error: {e}");
                        }
                    }
                }
            }
            let t_wasm = t1.elapsed();
            println!("\n{} cases: {} passed, {} failed, {} indeterminate (read an undefined flag)  (native {:.1}s, wasm {:.1}s)", count, count - failures, failures, indeterminate, t_native.as_secs_f64(), t_wasm.as_secs_f64());
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
