//! The save-state schema (`state::SCHEMA`): a hash of the definition of every type a
//! state serializes (items deriving `Serialize`, comments left out) and of the serde
//! helpers, so a state is only read by a runtime that lays it out the same way.

use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Code without comments and blank space.
fn strip(line: &str) -> String {
    let code = match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    };
    code.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The items deriving `Serialize`: from the derive to the end of the definition.
fn serialized_items(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(strip).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !(lines[i].starts_with("#[derive(") && lines[i].contains("Serialize")) {
            i += 1;
            continue;
        }
        let mut item = String::new();
        let mut depth = 0i32;
        let mut opened = false;
        while i < lines.len() {
            let l = &lines[i];
            item.push_str(l);
            item.push('\n');
            i += 1;
            for ch in l.chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if (opened && depth <= 0) || (!opened && l.ends_with(';') && (l.contains("struct ") || l.contains("enum "))) {
                break;
            }
        }
        out.push(item);
    }
    out
}

fn main() {
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    files.sort();
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    let mut mix = |s: &str| {
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01B3);
        }
    };
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let text = std::fs::read_to_string(f).unwrap();
        if f.ends_with("state.rs") {
            // the helpers (their part before the state code)
            let helpers = text.split("// ---- making and loading states").next().unwrap_or("");
            mix(&helpers.lines().map(strip).collect::<Vec<_>>().join("\n"));
        }
        for item in serialized_items(&text) {
            mix(&item);
        }
    }
    // the format's own crates
    if let Ok(lock) = std::fs::read_to_string("../../Cargo.lock") {
        println!("cargo:rerun-if-changed=../../Cargo.lock");
        for pkg in lock.split("[[package]]") {
            if pkg.contains("name = \"postcard\"") || pkg.contains("name = \"serde\"") {
                mix(pkg.lines().find(|l| l.starts_with("version")).unwrap_or(""));
            }
        }
    }
    println!("cargo:rustc-env=GMR_STATE_SCHEMA={h:016x}");
}
