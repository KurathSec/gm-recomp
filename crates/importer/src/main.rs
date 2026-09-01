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
