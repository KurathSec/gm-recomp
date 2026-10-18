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
