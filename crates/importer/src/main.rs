mod engine;
mod extract;
mod inventory;
mod report;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_WINE_DLLS: &str = "/usr/lib64/wine-wow64/wine/i386-windows";

#[derive(Parser)]
#[command(about = "Imports Windows GameMaker games for the wasm recompiler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Stage games (folder, .zip or .exe) into <out>/<slug>/ and inventory their modules.
    Import {
        inputs: Vec<PathBuf>,
        #[arg(long)]
        out: PathBuf,
        /// Folder of i386 system DLLs whose export names identify dynamically resolved APIs.
        #[arg(long, default_value = DEFAULT_WINE_DLLS)]
        wine_dlls: PathBuf,
        /// Replace an already imported game with the same slug.
        #[arg(long)]
        force: bool,
    },
    /// Write <out>/index.json, the game list the launcher page shows.
    Index { out: PathBuf },
    /// Summarize every manifest under <out> into a Markdown API/module inventory.
    Report {
        out: PathBuf,
        #[arg(long)]
        md: PathBuf,
    },
    /// Run static code discovery on every PE module of every imported game.
    Discover {
        out: PathBuf,
        /// Only games whose slug contains this text.
        #[arg(long)]
        only: Option<String>,
    },
}

/// DLLs the game's modules need that it does not ship and the runtime does not emulate
/// are copied from the system DLL folder (Wine's PE builds, debug sections stripped)
/// to files/C/Windows/system32, where the runtime looks after the game directory,
/// with the DLLs they need in turn. "Need" is a static import, or for redistributable
/// runtimes (Visual C++, the shader compiler) a name in the module's strings, as
/// D3DX loads d3dcompiler; the shader compiler only when the game has shaders.
fn supply_system_dlls(files_dir: &Path, system: &Path, shaders: bool) -> Result<Vec<String>> {
    let emulated = |stem: &str| runtime::hle::kernel32::CORE_DLLS.contains(&stem) || runtime::hle::kernel32::HLE_DLLS.contains(&stem);
    let redistributable = |name: &str| {
        ["msvcr", "msvcp", "vcruntime", "concrt", "ucrtbase"].iter().any(|p| name.starts_with(p)) || (shaders && name.starts_with("d3dcompiler_"))
    };
    let mut all = Vec::new();
    walk(files_dir, files_dir, &mut all)?;
    let shipped: std::collections::HashSet<String> =
        all.iter().filter(|f| !f.path.contains('/')).map(|f| f.path.to_lowercase()).collect();
    let mut queue: Vec<PathBuf> = all
        .iter()
        .filter(|f| { let l = f.path.to_lowercase(); l.ends_with(".exe") || l.ends_with(".dll") })
        .map(|f| files_dir.join(&f.path))
        .collect();
    let sysdir = files_dir.join("C").join("Windows").join("system32");
    let mut supplied = Vec::new();
    while let Some(path) = queue.pop() {
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(pe) = pe::Pe::parse(&bytes) else { continue };
        let mut wanted: Vec<String> = pe.imports().unwrap_or_default().iter().map(|d| d.dll.to_lowercase()).collect();
        wanted.extend(dll_names_in(&bytes).into_iter().filter(|n| redistributable(n)));
        for name in wanted {
            let stem = name.trim_end_matches(".dll").to_string();
            if emulated(&stem) || shipped.contains(&name) || supplied.contains(&name) {
                continue;
            }
            let src = system.join(&name);
            if !src.is_file() {
                continue;
            }
            fs::create_dir_all(&sysdir)?;
            let dst = sysdir.join(&name);
            fs::copy(&src, &dst)?;
            // Wine's builds carry DWARF; the runtime needs none of it.
            let _ = std::process::Command::new("objcopy").arg("--strip-debug").arg(&dst).status();
            supplied.push(name);
            queue.push(dst);
        }
    }
    Ok(supplied)
}

/// "*.dll" names in a binary's ASCII and UTF-16 strings (lowercase).
fn dll_names_in(bytes: &[u8]) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    let ok = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.';
    for step in [1usize, 2] {
        let mut i = 0;
        while i + 4 * step <= bytes.len() {
            let at = |k: usize| bytes[i + k * step];
            let is_dll = (0..4).all(|k| step == 1 || bytes.get(i + k * step + 1) == Some(&0))
                && at(0) == b'.' && at(1).eq_ignore_ascii_case(&b'd') && at(2).eq_ignore_ascii_case(&b'l') && at(3).eq_ignore_ascii_case(&b'l');
            if is_dll {
                let mut start = i;
                while start >= step && ok(bytes[start - step]) && (step == 1 || bytes[start - step + 1] == 0) {
                    start -= step;
                }
                let name: String = (start..i + 4 * step).step_by(step).map(|k| bytes[k] as char).collect();
                if name.len() > 4 && name.len() < 64 {
                    out.insert(name.to_lowercase());
                }
            }
            i += step;
        }
    }
    out.into_iter().collect()
}

/// <out>/index.json: [{slug, title, engine}] of every imported game, by title.
fn write_index(out: &Path) -> Result<()> {
    let mut games = Vec::new();
    for e in fs::read_dir(out)?.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("manifest.json")) else { continue };
        let m: serde_json::Value = serde_json::from_str(&text)?;
        games.push(serde_json::json!({ "slug": m["slug"], "title": m["title"], "engine": m["engine"] }));
    }
    games.sort_by_key(|g| g["title"].as_str().unwrap_or_default().to_lowercase());
    fs::write(out.join("index.json"), serde_json::to_string_pretty(&games)?)?;
    Ok(())
}

fn discover_all(out: &Path, only: Option<&str>) -> Result<()> {
    let mut dirs: Vec<_> = fs::read_dir(out)?.flatten().map(|e| e.path()).filter(|p| p.join("manifest.json").is_file()).collect();
    dirs.sort();
    println!("{:<34} {:<28} {:>9} {:>7} {:>7} {:>6} {:>6} {:>6}", "game", "module", "exec KB", "strong", "total", "funcs", "jtabs", "unres");
    for dir in dirs {
        let slug = dir.file_name().unwrap().to_string_lossy().into_owned();
        if only.is_some_and(|o| !slug.contains(o)) {
            continue;
        }
        let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("manifest.json"))?)?;
        let mut results = serde_json::Map::new();
        for m in manifest["modules"].as_array().into_iter().flatten() {
            let rel = m["path"].as_str().unwrap_or_default();
            let bytes = fs::read(dir.join("files").join(rel))?;
            let pe = pe::Pe::parse(&bytes)?;
            let d = x86::discover(&pe);
            let pct = |b: u64| if d.exec_bytes == 0 { 0.0 } else { 100.0 * b as f64 / d.exec_bytes as f64 };
            println!(
                "{:<34} {:<28} {:>9} {:>6.1}% {:>6.1}% {:>6} {:>6} {:>6}",
                slug,
                rel.chars().take(28).collect::<String>(),
                d.exec_bytes / 1024,
                pct(d.strong_bytes),
                pct(d.total_bytes),
                d.functions,
                d.jump_tables.len(),
                d.unresolved_indirect_jumps
            );
            results.insert(rel.to_owned(), serde_json::to_value(&d)?);
        }
        fs::write(dir.join("discovery.json"), serde_json::to_string_pretty(&results)?)?;
    }
    Ok(())
}

#[derive(Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

#[derive(Serialize)]
pub struct Manifest {
    pub slug: String,
    pub title: String,
    pub source: String,
    pub engine: engine::Engine,
    pub entry: String,
    pub entry_candidates: Vec<String>,
    pub iexpress: Option<extract::IExpress>,
    pub files: Vec<FileEntry>,
    pub modules: Vec<inventory::Module>,
    /// Files with an MZ header that are not PE32/i386 (16-bit, 64-bit, .NET-only, ...).
    pub unsupported_binaries: Vec<(String, String)>,
}

fn slugify(title: &str) -> String {
    let mut s = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    s.trim_end_matches('-').to_owned()
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if e.file_type()?.is_dir() {
            walk(&p, base, out)?;
        } else {
            let rel = p.strip_prefix(base)?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>();
            out.push(FileEntry { path: rel.join("/"), size: e.metadata()?.len() });
        }
    }
    Ok(())
}

fn choose_entry(files: &[FileEntry], iexpress: &Option<extract::IExpress>, hint: &Option<String>) -> (String, Vec<String>) {
    let top_exes: Vec<&FileEntry> = files
        .iter()
        .filter(|f| !f.path.contains('/') && f.path.to_lowercase().ends_with(".exe"))
        .filter(|f| !f.path.to_lowercase().starts_with("unins"))
        .collect();
    let candidates: Vec<String> = top_exes.iter().map(|f| f.path.clone()).collect();
    let find = |name: &str| files.iter().find(|f| f.path.eq_ignore_ascii_case(name)).map(|f| f.path.clone());
    if let Some(ix) = iexpress {
        if let Some(p) = find(&ix.run_program) {
            return (p, candidates);
        }
    }
    if let Some(p) = hint.as_deref().and_then(find) {
        return (p, candidates);
    }
    let largest = top_exes.iter().max_by_key(|f| f.size).map(|f| f.path.clone()).unwrap_or_default();
    (largest, candidates)
}

fn import_one(input: &Path, out: &Path, sys: &inventory::SystemExports, force: bool) -> Result<String> {
    let staging = out.join(".staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    let files_dir = staging.join("files");
    let staged = extract::stage(input, &files_dir)?;
    let iexpress = extract::unwrap_iexpress(&files_dir, &staging.join("scratch"))?;
    if let Some(installer) = extract::unwrap_nsis(&files_dir, &staging.join("scratch"))? {
        println!("installed the NSIS package {installer}");
    }
    let _ = fs::remove_dir_all(staging.join("scratch"));

    let mut slug = slugify(&staged.title);
    if slug.is_empty() {
        slug = slugify(&input.file_stem().unwrap_or_default().to_string_lossy());
    }
    let dest = out.join(&slug);
    if dest.exists() {
        if !force {
            fs::remove_dir_all(&staging)?;
            bail!("{} already exists (use --force to replace it)", dest.display());
        }
        fs::remove_dir_all(&dest)?;
    }

    let mut files = Vec::new();
    walk(&files_dir, &files_dir, &mut files)?;
    let (entry, entry_candidates) = choose_entry(&files, &iexpress, &staged.entry_hint);
    let mut modules = Vec::new();
    let mut unsupported_binaries = Vec::new();
    for f in &files {
        let path = files_dir.join(&f.path);
        let mut head = [0u8; 2];
        if f.size < 64 || fs::File::open(&path).and_then(|mut h| std::io::Read::read_exact(&mut h, &mut head)).is_err() {
            continue;
        }
        if &head != b"MZ" {
            continue;
        }
        let bytes = fs::read(&path)?;
        match inventory::analyze(&f.path, &bytes, sys) {
            Ok(m) => modules.push(m),
            Err(e) => unsupported_binaries.push((f.path.clone(), e.to_string())),
        }
    }
    let engine = if entry.is_empty() {
        engine::Engine::Unknown
    } else {
        let exe = fs::read(files_dir.join(&entry))?;
        let exe_dir = files_dir.join(&entry);
        engine::detect(&exe, exe_dir.parent().unwrap())
    };
    let shaders = matches!(engine, engine::Engine::Gms { shaders, .. } if shaders > 0);
    let supplied = supply_system_dlls(&files_dir, &sys.dir, shaders)?;
    for dll in &supplied {
        println!("  supplying {dll} (needed, neither shipped nor emulated)");
    }
    if !supplied.is_empty() {
        files.clear();
        walk(&files_dir, &files_dir, &mut files)?;
        for f in files.iter().filter(|f| f.path.starts_with("C/")) {
            let bytes = fs::read(files_dir.join(&f.path))?;
            match inventory::analyze(&f.path, &bytes, sys) {
                Ok(m) => modules.push(m),
                Err(e) => unsupported_binaries.push((f.path.clone(), e.to_string())),
            }
        }
    }
    let manifest = Manifest {
        slug: slug.clone(),
        title: staged.title,
        source: input.display().to_string(),
        engine,
        entry,
        entry_candidates,
        iexpress,
        files,
        modules,
        unsupported_binaries,
    };
    fs::write(staging.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
    fs::rename(&staging, &dest).with_context(|| format!("moving staged game to {}", dest.display()))?;
    Ok(slug)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Import { inputs, out, wine_dlls, force } => {
            fs::create_dir_all(&out)?;
            let sys = inventory::SystemExports::load(&wine_dlls);
            if sys.is_empty() {
                eprintln!("warning: no system DLLs found in {}; dynamic API detection disabled", wine_dlls.display());
            }
            let mut failed = 0;
            for input in &inputs {
                match import_one(input, &out, &sys, force) {
                    Ok(slug) => println!("imported {} -> {slug}", input.display()),
                    Err(e) => {
                        failed += 1;
                        eprintln!("error importing {}: {e:#}", input.display());
                    }
                }
            }
            write_index(&out)?;
            if failed > 0 {
                bail!("{failed} of {} imports failed", inputs.len());
            }
            Ok(())
        }
        Cmd::Index { out } => write_index(&out),
        Cmd::Report { out, md } => report::write(&out, &md),
        Cmd::Discover { out, only } => discover_all(&out, only.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugs() {
        assert_eq!(slugify("I wanna run the Marathon v1.2"), "i-wanna-run-the-marathon-v1-2");
        assert_eq!(slugify("Crimson Needle 3"), "crimson-needle-3");
        assert_eq!(slugify("にほんご"), "");
    }
}
