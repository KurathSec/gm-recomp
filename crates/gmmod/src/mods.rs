//! Fangame.ModLoader's mods, reimplemented: what each one does to a game's data. The
//! names each mod looks for are those of the builds in use (their DLLs).

use crate::Assets;
use crate::gm8::{Entry, Gm8, Image, Object, Script, Sprite, event};
use crate::image;
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;

fn png(assets: &Assets, module: &str, name: &str, n: u32) -> Result<Vec<Image>> {
    let bytes = assets.get(module, name).ok_or_else(|| format!("{name} is missing from the mod's files"))?;
    Ok(image::frames(&image::decode_png(bytes)?, n))
}

/// Each script called one of `names`, to change.
fn gm8_scripts(g: &mut Gm8, names: &[&str], mut f: impl FnMut(&mut Script)) -> Result<usize> {
    let mut n = 0;
    for e in g.scripts.entries.iter_mut() {
        let Some(mut s) = Script::parse(&e.content()?)? else { continue };
        if names.iter().any(|&m| s.name == m.as_bytes()) {
            f(&mut s);
            *e = Entry::Content(s.content());
            n += 1;
        }
    }
    Ok(n)
}

/// Each sprite called one of `names` gets `frames` (and `origin`, if given).
fn gm8_sprites(g: &mut Gm8, names: &[&str], frames: &[Image], origin: Option<(i32, i32)>) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for e in g.sprites.entries.iter_mut() {
        let Some(mut s) = Sprite::parse(&e.content()?)? else { continue };
        if names.iter().any(|&m| s.name == m.as_bytes()) {
            s.set_frames(frames);
            if let Some(o) = origin {
                s.origin = o;
            }
            done.push(String::from_utf8_lossy(&s.name).into_owned());
            *e = Entry::Content(s.content());
        }
    }
    Ok(done)
}

/// The index of the last object called one of `names`.
fn gm8_last_object(g: &Gm8, names: &[&str]) -> Result<Option<usize>> {
    let all = g.objects.names()?;
    Ok(all.iter().rposition(|n| n.as_deref().is_some_and(|n| names.iter().any(|&m| n == m.as_bytes()))))
}

fn gm8_object(g: &Gm8, i: usize) -> Result<Object> {
    Object::parse(&g.objects.entries[i].content()?)?.ok_or_else(|| "empty object slot".into())
}

/// GodMode: the scripts that kill the player return at once.
pub fn godmode_gm8(g: &mut Gm8) -> Result<Vec<String>> {
    let names = ["scrKillPlayer", "killPlayer", "player_kill", "kill_player"];
    let n = gm8_scripts(g, &names, |s| {
        let mut src = b"exit;\n".to_vec();
        src.extend_from_slice(&s.source);
        s.source = src;
    })?;
    if n == 0 {
        return Err("no script that kills the player (scrKillPlayer, killPlayer, player_kill, kill_player)".into());
    }
    Ok(vec![format!("{n} kill script(s) made to return at once")])
}

/// AllowFullscreen: F4 switches to full screen (a GameMaker 8 setting).
pub fn allow_fullscreen_gm8(g: &mut Gm8) -> Result<Vec<String>> {
    let s = g.settings_mut()?;
    let f4 = s.get_mut(60..64).ok_or("settings too short")?;
    f4.copy_from_slice(&1u32.to_le_bytes());
    Ok(vec!["F4 toggles full screen".into()])
}

/// CrimsonKid: the player's and the bow's sprites from the mod's images; the bow drawn
/// behind the player and animated at `BowImageSpeed`.
pub fn crimson_kid_gm8(g: &mut Gm8, config: &Value, assets: &Assets) -> Result<Vec<String>> {
    let speed = config["BowImageSpeed"].as_f64().unwrap_or(0.2);
    let mut log = Vec::new();
    for (name, file, n) in [
        ("sprPlayerIdle", "sprPlayerIdle.png", 4),
        ("sprPlayerRunning", "sprPlayerRunning.png", 4),
        ("sprPlayerJump", "sprPlayerJump.png", 2),
        ("sprPlayerFall", "sprPlayerFall.png", 2),
        ("sprPlayerSliding", "sprPlayerSliding.png", 2),
        ("sprBow", "sprBow.png", 4),
    ] {
        let frames = png(assets, "CrimsonKid", file, n)?;
        let origin = (name == "sprBow").then_some((22, 23));
        for s in gm8_sprites(g, &[name], &frames, origin)? {
            log.push(format!("sprite {s} replaced"));
        }
    }
    let player = gm8_last_object(g, &["player", "objPlayer"])?;
    if let Some(b) = gm8_last_object(g, &["bow", "objBow"])? {
        let mut bow = gm8_object(g, b)?;
        if let Some(p) = player {
            bow.depth = gm8_object(g, p)?.depth + 1;
        }
        bow.add_code(event::CREATE, 0, format!("\nimage_speed = {speed};").as_bytes());
        g.objects.entries[b] = Entry::Content(bow.content());
        log.push(format!("bow: behind the player, image_speed {speed}"));
    }
    Ok(log)
}

/// DotKid: the player drawn as its collision mask (a dot), its own sprites blank, with
/// an outline circle if `ShowOutline`.
pub fn dot_kid_gm8(g: &mut Gm8, config: &Value, assets: &Assets) -> Result<Vec<String>> {
    let outline = config["ShowOutline"].as_bool().unwrap_or(true);
    let mut log = Vec::new();
    let mask = png(assets, "DotKid", "sprDotkidMask.png", 1)?;
    for s in gm8_sprites(g, &["maskPlayer", "maskPlayer2", "sprPlayerMask", "sprPlayerMaskFlip"], &mask, Some((16, 16)))? {
        log.push(format!("mask {s} is the dot"));
    }
    let empty = png(assets, "DotKid", "sprEmpty32x32.png", 1)?;
    for s in gm8_sprites(g, &["sprPlayerIdle", "sprPlayerRunning", "sprPlayerJump", "sprPlayerFall", "sprBow", "sprPlayerSliding"], &empty, Some((16, 16)))? {
        log.push(format!("sprite {s} blank"));
    }
    let mut code = b"draw_sprite_ext(mask_index, image_index, x, y, image_xscale, image_yscale, image_angle, image_blend, image_alpha);".to_vec();
    if outline {
        code.extend_from_slice(b"draw_sprite_ext(sprDotkidModCircle, image_index, x, y, image_xscale, image_yscale, image_angle, image_blend, image_alpha);");
        let mut circle = Sprite::new("sprDotkidModCircle");
        circle.set_frames(&png(assets, "DotKid", "sprDotkidCircle.png", 1)?);
        circle.origin = (65, 64);
        g.sprites.entries.push(Entry::Content(circle.content()));
        log.push("sprite sprDotkidModCircle added".into());
    }
    let names = g.objects.names()?;
    for (i, n) in names.iter().enumerate() {
        if n.as_deref().is_some_and(|n| [&b"player"[..], b"objPlayer", b"Player"].contains(&n)) {
            let mut o = gm8_object(g, i)?;
            o.add_code(event::DRAW, 0, &code);
            g.objects.entries[i] = Entry::Content(o.content());
            log.push(format!("object {} draws the dot", String::from_utf8_lossy(n.as_deref().unwrap())));
        }
    }
    Ok(log)
}

/// PlayOnline's settings (its Config.json fields).
pub struct OnlineConfig {
    pub server: String,
    pub tcp_port: i64,
    pub udp_port: i64,
    pub name: String,
    pub password: String,
    pub race: bool,
}

impl OnlineConfig {
    pub fn from_json(c: &Value) -> OnlineConfig {
        OnlineConfig {
            server: c["ServerIp"].as_str().unwrap_or("iwannastartaparty.com").to_owned(),
            tcp_port: c["TcpPort"].as_i64().unwrap_or(8002),
            udp_port: c["UdpPort"].as_i64().unwrap_or(8003),
            name: c["PlayerName"].as_str().unwrap_or("Player").to_owned(),
            password: c["Password"].as_str().unwrap_or("").to_owned(),
            race: c["RaceMode"].as_bool().unwrap_or(false),
        }
    }
}

/// The lines of a text file, as .NET's File.ReadAllLines splits them.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'\r' | b'\n' => {
                out.push(&text[start..i]);
                if text[i] == b'\r' && text.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// The scripts of a "#define name" list (PlayOnline's ImportScriptList: each line followed
/// by CRLF).
fn script_list(text: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut name: Vec<u8> = Vec::new();
    let mut body = Vec::new();
    for line in lines(text) {
        if line.starts_with(b"#define") {
            if !name.is_empty() {
                out.push((std::mem::take(&mut name), std::mem::take(&mut body)));
            }
            name = line.get(8..).unwrap_or(&[]).to_vec();
        } else {
            body.extend_from_slice(line);
            body.extend_from_slice(b"\r\n");
        }
    }
    out.push((name, body));
    out
}

/// PlayOnline's GML for every engine, then for this one (`GML/GM8` or `GML/GMS`): each
/// file directly in the folder, by name, as a script.
fn online_scripts<'a>(assets: &'a Assets, folder: &str) -> Vec<(String, &'a [u8])> {
    let mut v: Vec<(String, &[u8])> = assets
        .files
        .iter()
        .filter(|(m, _, _)| m == "PlayOnline")
        .filter_map(|(_, n, d)| {
            let n = n.replace('\\', "/");
            let rest = n.strip_prefix(folder)?.strip_prefix('/')?;
            let stem = rest.strip_suffix(".gml")?;
            (!stem.contains('/')).then(|| (stem.to_owned(), d.as_slice()))
        })
        .collect();
    v.sort_by_key(|(n, _)| n.to_lowercase());
    v
}

/// PlayOnline on a GameMaker 8 game: the networking DLL's scripts and the mod's scripts,
/// its three objects, hooks in the world object and the save and load scripts, and its
/// files next to the exe. `original` is the game's exe as shipped (its MD5 is the game's
/// id on the server); `exe_name` its file name.
pub fn play_online_gm8(g: &mut Gm8, config: &Value, assets: &Assets, original: &[u8], exe_name: &str) -> Result<(Vec<String>, Vec<(String, Vec<u8>)>)> {
    let c = OnlineConfig::from_json(config);
    let names = g.objects.names()?;
    let has = |n: &str| names.iter().any(|x| x.as_deref() == Some(n.as_bytes()));
    if has("__ONLINE_onlinePlayer") || has("po_onlineplayer") {
        return Err("the game is already an online version".into());
    }
    let world = gm8_last_object(g, &["world", "World", "objWorld"])?.ok_or("the game has no \"world\" object")?;
    let get = |file: &str| assets.get("PlayOnline", file).ok_or_else(|| format!("{file} is missing from the mod's files"));
    let mut log = Vec::new();

    // the networking DLL's functions
    let list = script_list(get("po_http_dll_2_3.gml")?);
    log.push(format!("{} scripts for po_http_dll_2_3.dll", list.len()));
    for (name, source) in list {
        g.scripts.entries.push(Entry::Content(Script { name, source }.content()));
    }
    // the settings
    let digest = md5::compute(original);
    let stem = exe_name.rsplit(['/', '\\']).next().unwrap_or(exe_name);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    let mut cfg = String::from_utf8_lossy(get("GML/Config/po_config.gml")?).into_owned();
    for (k, v) in [
        ("{SERVER}", c.server.clone()),
        ("{TCP_PORT}", c.tcp_port.to_string()),
        ("{UDP_PORT}", c.udp_port.to_string()),
        ("{RACE}", (if c.race { "true" } else { "false" }).to_owned()),
        ("{PASSWORD}", c.password.clone()),
        ("{GAME_ID}", format!("{digest:x}")),
        ("{GAME_NAME}", stem.to_owned()),
        ("{NAME}", c.name.clone()),
        ("{VERSION}", "1.1.9".to_owned()),
    ] {
        cfg = cfg.replace(k, &v);
    }
    g.scripts.entries.push(Entry::Content(Script { name: b"po_config".to_vec(), source: cfg.into_bytes() }.content()));

    // the world object runs the networking
    let mut w = gm8_object(g, world)?;
    w.add_code(event::CREATE, 0, b"\npo_world_create();");
    w.add_code(event::STEP, event::END_STEP, b"\npo_world_endstep();");
    w.add_code(event::OTHER, event::GAME_END, b"\npo_world_gameend();");
    g.objects.entries[world] = Entry::Content(w.content());
    log.push(format!("world object: {}", String::from_utf8_lossy(names[world].as_deref().unwrap_or(b""))));

    // saving tells the others; loading picks up what they saved
    let scripts = g.scripts.names()?;
    let script = |n: &str| scripts.iter().position(|x| x.as_deref() == Some(n.as_bytes()));
    let append = |g: &mut Gm8, i: usize, code: &[u8]| -> Result<()> {
        let mut s = Script::parse(&g.scripts.entries[i].content()?)?.ok_or("empty script slot")?;
        s.source.extend_from_slice(b"\n");
        s.source.extend_from_slice(code);
        g.scripts.entries[i] = Entry::Content(s.content());
        Ok(())
    };
    if let Some(i) = script("saveGame") {
        append(g, i, b"po_someonesave_send();")?;
    } else if let Some(i) = script("scrSaveGame") {
        append(g, i, b"if (argument0) { po_someonesave_send(); }")?;
    } else if let Some(i) = script("savedata_save") {
        append(g, i, b"po_someonesave_send();")?;
    }
    if let (Some(t), Some(l)) = (script("tempExe"), script("loadGame")) {
        append(g, l, b"po_temp_socketstate_save();")?;
        append(g, t, b"po_someonesave_load();")?;
    } else if let Some(i) = script("saveExe").or(script("scrLoadGame")).or(script("savedata_load")) {
        append(g, i, b"po_someonesave_load();")?;
    }

    // the other players, their chat and their saves
    let mut player = Object::new("po_onlineplayer");
    player.persistent = true;
    player.depth = -10;
    player.add_code(event::CREATE, 0, b"po_onlineplayer_create();");
    player.add_code(event::STEP, event::END_STEP, b"po_onlineplayer_endstep();");
    player.add_code(event::DRAW, 0, b"po_onlineplayer_draw();");
    let mut chat = Object::new("po_chatbox");
    chat.visible = true;
    chat.depth = -11;
    chat.persistent = true;
    chat.add_code(event::CREATE, 0, b"po_chatbox_create();");
    chat.add_code(event::STEP, event::END_STEP, b"po_chatbox_endstep();");
    chat.add_code(event::DRAW, 0, b"po_chatbox_draw();");
    let mut saved = Object::new("po_playersaved");
    saved.visible = true;
    saved.depth = -10;
    saved.add_code(event::STEP, event::END_STEP, b"po_playersaved_endstep();");
    saved.add_code(event::DRAW, 0, b"po_playersaved_draw();");
    for o in [player, chat, saved] {
        g.objects.entries.push(Entry::Content(o.content()));
    }

    let mut n = 0;
    for folder in ["GML", "GML/GM8"] {
        for (name, source) in online_scripts(assets, folder) {
            g.scripts.entries.push(Entry::Content(Script { name: name.into_bytes(), source: source.to_vec() }.content()));
            n += 1;
        }
    }
    log.push(format!("{n} scripts and 3 objects added"));
    let mut files = Vec::new();
    for f in ["po_http_dll_2_3.dll", "po_snd_chatbox.wav", "po_snd_saved.wav"] {
        files.push((f.to_owned(), get(f)?.to_vec()));
    }
    log.push(format!("server {}:{} (UDP {}), name {}", c.server, c.tcp_port, c.udp_port, c.name));
    Ok((log, files))
}
