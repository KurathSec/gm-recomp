//! Staging a game into `<out>/<slug>/files`: unpacking zips (with cp932-aware
//! names), copying folders, and unwrapping IExpress self-extractors.

use anyhow::{Context, Result, bail};
use pe::{Pe, RT_RCDATA, ResId};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Decodes a zip entry name. Zips made on Japanese Windows store Shift-JIS
/// without the UTF-8 flag, so try UTF-8, then cp932, then cp437.
pub fn decode_zip_name(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_owned();
    }
    let (s, _, had_errors) = encoding_rs::SHIFT_JIS.decode(raw);
    if !had_errors {
        return s.into_owned();
    }
    raw.iter().map(|&b| cp437_char(b)).collect()
}

fn cp437_char(b: u8) -> char {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    if b < 0x80 { b as char } else { HIGH.chars().nth((b - 0x80) as usize).unwrap_or('?') }
}

/// Rejects absolute paths and `..` so archive entries cannot escape the staging dir.
fn safe_relative(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(name.replace('\\', "/"));
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to).with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

pub struct Staged {
    pub title: String,
    /// Exe the user pointed at, when the input was an exe rather than a folder or zip.
    pub entry_hint: Option<String>,
}

/// Copies or unpacks `input` into `files_dir` (which must not exist yet).
pub fn stage(input: &Path, files_dir: &Path) -> Result<Staged> {
    let meta = fs::metadata(input).with_context(|| format!("reading {}", input.display()))?;
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if meta.is_dir() {
        copy_dir(input, files_dir)?;
        return Ok(Staged { title: input.file_name().unwrap().to_string_lossy().into_owned(), entry_hint: None });
    }
    let ext = input.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut magic = [0u8; 6];
    let _ = fs::File::open(input).and_then(|mut f| f.read_exact(&mut magic));
    // Archives are identified by content: renamed .zip/.rar files are common.
    if magic.starts_with(b"PK\x03\x04") {
        let title = extract_zip(input, files_dir)?.unwrap_or(stem);
        return Ok(Staged { title, entry_hint: None });
    }
    if magic.starts_with(b"7z\xbc\xaf\x27\x1c") || magic.starts_with(b"Rar!\x1a\x07") {
        extract_with_tool(input, files_dir)?;
        let title = hoist_single_top_dir(files_dir)?.unwrap_or(stem);
        return Ok(Staged { title, entry_hint: None });
    }
    match ext.as_str() {
        "exe" => {
            let dir = input.parent().context("exe has no parent folder")?;
            copy_dir(dir, files_dir)?;
            Ok(Staged {
                title: dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(stem),
                entry_hint: Some(input.file_name().unwrap().to_string_lossy().into_owned()),
            })
        }
        _ => bail!("unsupported input {} (expected a folder, .zip or .exe)", input.display()),
    }
}

/// Extracts 7z/RAR archives with whichever external tool can read them.
/// Fedora's 7z lacks the RAR codec, so bsdtar (libarchive) and unar are fallbacks.
fn extract_with_tool(input: &Path, files_dir: &Path) -> Result<()> {
    fs::create_dir_all(files_dir)?;
    let attempts: [(&str, Vec<String>); 3] = [
        ("7z", vec!["x".into(), "-y".into(), format!("-o{}", files_dir.display()), input.display().to_string()]),
        ("bsdtar", vec!["-xf".into(), input.display().to_string(), "-C".into(), files_dir.display().to_string()]),
        ("unar", vec!["-q".into(), "-f".into(), "-D".into(), "-o".into(), files_dir.display().to_string(), input.display().to_string()]),
    ];
    let mut tried = Vec::new();
    for (tool, args) in attempts {
        match Command::new(tool).args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => tried.push(format!("{tool} exited with {s}")),
            Err(_) => tried.push(format!("{tool} not installed")),
        }
        // A failed attempt may leave partial output behind.
        fs::remove_dir_all(files_dir)?;
        fs::create_dir_all(files_dir)?;
    }
    bail!("could not extract {} ({})", input.display(), tried.join("; "))
}

/// If `dir` holds exactly one folder and nothing else, moves its contents up. Returns the folder name.
fn hoist_single_top_dir(dir: &Path) -> Result<Option<String>> {
    let entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    let [only] = entries.as_slice() else { return Ok(None) };
    if !only.file_type()?.is_dir() {
        return Ok(None);
    }
    let name = only.file_name();
    let tmp = dir.with_extension("hoist");
    fs::rename(only.path(), &tmp)?;
    for e in fs::read_dir(&tmp)? {
        let e = e?;
        fs::rename(e.path(), dir.join(e.file_name()))?;
    }
    fs::remove_dir(&tmp)?;
    Ok(Some(name.to_string_lossy().into_owned()))
}

/// Extracts a zip, stripping a single shared top-level folder. Returns that folder's name.
fn extract_zip(zip_path: &Path, files_dir: &Path) -> Result<Option<String>> {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let f = archive.by_index(i)?;
        let name = decode_zip_name(f.name_raw());
        let rel = safe_relative(&name).with_context(|| format!("unsafe zip entry name {name:?}"))?;
        entries.push((i, rel, f.is_dir()));
    }
    let first = |p: &PathBuf| p.components().next().map(|c| c.as_os_str().to_owned());
    let top = entries.first().and_then(|(_, p, _)| first(p));
    let shared_top = top.filter(|t| {
        entries.iter().all(|(_, p, is_dir)| first(p).as_ref() == Some(t) && (p.components().count() > 1 || *is_dir))
    });
    fs::create_dir_all(files_dir)?;
    for (i, rel, is_dir) in entries {
        let rel = match &shared_top {
            Some(_) => rel.components().skip(1).collect::<PathBuf>(),
            None => rel,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = files_dir.join(&rel);
        if is_dir {
            fs::create_dir_all(&dst)?;
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = archive.by_index(i)?;
        let mut buf = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut buf)?;
        fs::write(&dst, buf)?;
    }
    Ok(shared_top.map(|t| t.to_string_lossy().into_owned()))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IExpress {
    pub stub: String,
    pub run_program: String,
    pub title: String,
}

fn rcdata_string(pe: &Pe, name: &str) -> Option<String> {
    let res = pe.resources().ok()?;
    let r = res.iter().find(|r| r.kind == ResId::Id(RT_RCDATA) && r.name == ResId::Name(name.into()))?;
    let bytes = pe.resource_data(r).ok()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// If a top-level exe is an IExpress package, extracts its CABINET next to it with 7z,
/// deletes the stub and returns what it would have run.
pub fn unwrap_iexpress(files_dir: &Path, scratch: &Path) -> Result<Option<IExpress>> {
    for entry in fs::read_dir(files_dir)? {
        let path = entry?.path();
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
            continue;
        }
        let bytes = fs::read(&path)?;
        let Ok(pe) = Pe::parse(&bytes) else { continue };
        let Ok(res) = pe.resources() else { continue };
        let Some(cab) = res.iter().find(|r| r.kind == ResId::Id(RT_RCDATA) && r.name == ResId::Name("CABINET".into()))
        else {
            continue;
        };
        let cab_bytes = pe.resource_data(cab)?;
        if !cab_bytes.starts_with(b"MSCF") {
            continue;
        }
        // Extract beside, not into, files_dir: the payload often reuses the stub's file name.
        let unpacked = scratch.join("unpacked");
        fs::create_dir_all(&unpacked)?;
        let cab_path = scratch.join("iexpress.cab");
        fs::write(&cab_path, cab_bytes)?;
        let status = Command::new("7z")
            .arg("x")
            .arg("-y")
            .arg("-tcab")
            .arg(format!("-o{}", unpacked.display()))
            .arg(&cab_path)
            .stdout(std::process::Stdio::null())
            .status()
            .context("running 7z (is p7zip installed?)")?;
        fs::remove_file(&cab_path)?;
        if !status.success() {
            bail!("7z failed to extract the IExpress cabinet in {}", path.display());
        }
        let run = rcdata_string(&pe, "RUNPROGRAM").unwrap_or_default();
        let info = IExpress {
            stub: path.file_name().unwrap().to_string_lossy().into_owned(),
            run_program: run.trim().trim_matches('"').to_owned(),
            title: rcdata_string(&pe, "TITLE").unwrap_or_default(),
        };
        drop(bytes);
        fs::remove_file(&path)?;
        for e in fs::read_dir(&unpacked)? {
            let e = e?;
            fs::rename(e.path(), files_dir.join(e.file_name()))?;
        }
        fs::remove_dir_all(&unpacked)?;
        return Ok(Some(info));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_names_decode_shift_jis() {
        // "テスト.txt" in Shift-JIS
        let raw = [0x83, 0x65, 0x83, 0x58, 0x83, 0x67, b'.', b't', b'x', b't'];
        assert_eq!(decode_zip_name(&raw), "テスト.txt");
        assert_eq!(decode_zip_name(b"plain/name.exe"), "plain/name.exe");
    }

    #[test]
    fn rejects_escaping_paths() {
        assert!(safe_relative("../evil").is_none());
        assert!(safe_relative("/abs").is_none());
        assert_eq!(safe_relative("a\\b.txt"), Some(PathBuf::from("a/b.txt")));
    }
}
