//! Prints discovery diagnostics for one PE file: `cargo run -p x86 --example diag -- file.exe [filter]`

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: diag <pe file> [substring filter]");
    let filter = args.next();
    let bytes = std::fs::read(&path).expect("read");
    let pe = pe::Pe::parse(&bytes).expect("parse");
    let d = x86::discover(&pe);
    println!(
        "exec {} KB, strong {:.1}%, total {:.1}%, functions {}, jump tables {}, unresolved {}, invalid {}, gaps {} ({} KB)",
        d.exec_bytes / 1024,
        100.0 * d.strong_bytes as f64 / d.exec_bytes.max(1) as f64,
        100.0 * d.total_bytes as f64 / d.exec_bytes.max(1) as f64,
        d.functions,
        d.jump_tables.len(),
        d.unresolved_indirect_jumps,
        d.invalid_decodes,
        d.gap_count,
        d.gap_bytes / 1024
    );
    for (k, n) in &d.functions_by_kind {
        println!("  {k:?}: {n}");
    }
    for line in d.diagnostics.iter().filter(|l| filter.as_deref().is_none_or(|f| l.contains(f))) {
        println!("{line}");
    }
}
