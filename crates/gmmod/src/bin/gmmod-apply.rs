//! Applies mods to an imported game natively, for testing (the page does it in the browser):
//!   gmmod-apply <game dir (web/public/games/<slug>)> '<mods json: [{"id": ..., "config": {...}}]>' <out dir> [mods asset dir]
//! The out dir gets a copy of the game (manifest and files) with the mods applied, which
//! `harness run` plays.
use std::fs;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if e.path().is_dir() {
            walk(&e.path(), &r, out);
        } else {
            out.push((r, e.path()));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: gmmod-apply <game dir> <mods json> <out dir> [mods asset dir]");
        std::process::exit(2);
    }
    let (game_dir, mods, out_dir) = (Path::new(&args[1]), &args[2], Path::new(&args[3]));
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(game_dir.join("manifest.json")).expect("manifest.json")).expect("manifest");
    let mods: Vec<serde_json::Value> = serde_json::from_str(mods).expect("mods json");
    let mut game = gmmod::Game::new(gmmod::Engine::from_json(&manifest["engine"]), manifest["entry"].as_str().unwrap_or(""));
    let mut files = Vec::new();
    walk(&game_dir.join("files"), "", &mut files);
    for (rel, path) in &files {
        game.add(rel, fs::read(path).expect("read"));
    }
    let mut assets = gmmod::Assets::default();
    if let Some(dir) = args.get(4) {
        for m in &mods {
            let id = m["id"].as_str().unwrap_or("");
            let mut list = Vec::new();
            walk(&Path::new(dir).join(id), "", &mut list);
            for (rel, path) in list {
                assets.files.push((id.to_owned(), rel, fs::read(path).expect("read asset")));
            }
        }
    }
    match gmmod::apply(&mut game, &mods, &assets) {
        Ok(log) => log.iter().for_each(|l| println!("{l}")),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    fs::create_dir_all(out_dir.join("files")).expect("out dir");
    fs::write(out_dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    for ((rel, data), changed) in game.files.iter().zip(&game.changed) {
        let p = out_dir.join("files").join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, data).unwrap();
        if *changed {
            println!("changed {rel}");
        }
    }
}
