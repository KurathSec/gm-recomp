//! Prints a GameMaker 8 game's scripts, objects (events with their actions' arguments)
//! and rooms (creation code, views, instances; GameMaker 8.2 rooms too), for debugging:
//!   cargo run --release -p gmmod --example gm8dump -- <game.exe> [scripts|objects|rooms]...
use gmmod::gm8::{Gm8, Reader, Script};

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Each action: (library, action id, argument strings).
fn actions(mut d: &[u8], n: u32) -> Vec<(u32, u32, Vec<String>)> {
    let mut out = Vec::new();
    for _ in 0..n {
        let mut r = Reader::new(d);
        let parse = |r: &mut Reader| -> Result<(u32, u32, Vec<String>), Box<dyn std::error::Error>> {
            r.u32()?;
            let lib = r.u32()?;
            let id = r.u32()?;
            r.skip(4 * 5)?;
            r.string()?;
            r.string()?;
            r.u32()?;
            let types = r.u32()? as usize;
            r.skip(4 * types)?;
            r.skip(4 * 2)?;
            let mut args = Vec::new();
            for _ in 0..r.u32()? {
                args.push(s(r.string()?));
            }
            r.u32()?;
            Ok((lib, id, args))
        };
        match parse(&mut r) {
            Ok(a) => out.push(a),
            Err(_) => break,
        }
        d = &d[r.pos..];
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let g = Gm8::parse(&std::fs::read(&args[1])?)?;
    let want = |k: &str| args.len() < 3 || args[2..].iter().any(|a| a == k);
    if want("scripts") {
        for (i, e) in g.scripts.entries.iter().enumerate() {
            if let Some(sc) = Script::parse(&e.content()?)? {
                println!("==== script {i} {}\n{}", s(&sc.name), s(&sc.source));
            }
        }
    }
    if want("objects") {
        for (i, e) in g.objects.entries.iter().enumerate() {
            let c = e.content()?;
            let Some(o) = gmmod::gm8::Object::parse(&c)? else { continue };
            println!("==== object {i} {} sprite {} visible {} depth {} parent {} persistent {}", s(&o.name), o.sprite, o.visible, o.depth, o.parent, o.persistent);
            for (ty, list) in o.events.iter().enumerate() {
                for (sub, n, data) in list {
                    println!("-- event {ty}:{sub}");
                    for (lib, id, a) in actions(data, *n) {
                        let a: Vec<&String> = a.iter().filter(|x| !x.is_empty()).collect();
                        println!("   action {lib}/{id}: {}", a.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(" | "));
                    }
                }
            }
        }
    }
    if want("rooms") {
        for (i, e) in g.rooms.entries.iter().enumerate() {
            let c = e.content()?;
            let mut r = Reader::new(&c);
            if r.u32()? == 0 {
                continue;
            }
            let name = s(r.string()?);
            let ver = r.u32()?;
            let caption = s(r.string()?);
            let (w, h) = (r.u32()?, r.u32()?);
            if ver < 811 {
                r.skip(4 * 3)?; // snap x, y, isometric
            }
            let speed = r.u32()?;
            let persistent = r.u32()?;
            let colour = r.u32()?;
            let draw_colour = r.u32()?;
            let code = s(r.string()?);
            println!("==== room {i} {name} (v{ver}) {w}x{h} speed {speed} caption {caption:?} persistent {persistent} colour {colour:#x} draw {draw_colour}\n{code}");
            let nb = r.u32()?;
            for b in 0..nb {
                let v: Vec<i32> = (0..10).map(|_| r.i32().unwrap_or(0)).collect();
                if v[0] != 0 {
                    println!("  background {b}: {v:?}");
                }
            }
            let views = r.u32()?;
            let nv = r.u32()?;
            for v in 0..nv {
                let x: Vec<i32> = (0..14).map(|_| r.i32().unwrap_or(0)).collect();
                if views != 0 && x[0] != 0 {
                    println!("  view {v}: {x:?}");
                }
            }
            let ni = r.u32()?;
            for _ in 0..ni {
                let (x, y, obj, id) = (r.i32()?, r.i32()?, r.i32()?, r.u32()?);
                let code = s(r.string()?);
                if ver >= 811 {
                    r.skip(8 + 8 + 4 + 8)?; // x/y scale, blend, angle
                } else {
                    r.u32()?;
                }
                println!("  instance {id} obj {obj} at {x},{y}{}", if code.is_empty() { String::new() } else { format!(" code: {code}") });
            }
        }
    }
    Ok(())
}
