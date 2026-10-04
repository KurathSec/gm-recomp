//! Checks the browser importer natively: `cargo run -p webimport --example import_zip
//! -- <zip>` prints the manifest it makes and the system DLLs it would ask for.

fn main() {
    let path = std::env::args().nth(1).expect("usage: import_zip <zip>");
    let bytes = std::fs::read(&path).expect("reading the zip");
    match webimport::import(&bytes, &path) {
        Ok(g) => {
            let m = g.manifest();
            let shaders = m["engine"]["shaders"].as_u64().unwrap_or(0) > 0;
            println!("{}", serde_json::to_string_pretty(&m).unwrap());
            println!("needs: {:?}", g.needs(shaders));
            // IMPORT_DUMP=<dir>: also write the files there (to compare with an import)
            if let Ok(dir) = std::env::var("IMPORT_DUMP") {
                for (p, d) in &g.files {
                    let to = std::path::Path::new(&dir).join(p);
                    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                    std::fs::write(to, d).unwrap();
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
