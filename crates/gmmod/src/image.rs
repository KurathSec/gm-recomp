//! Mods' images: PNG files decoded to RGBA, and strips cut into frames.

use crate::gm8::Image;

/// A PNG (any colour type and depth) as 8-bit RGBA.
pub fn decode_png(bytes: &[u8]) -> Result<Image, String> {
    let mut dec = png::Decoder::new(bytes);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("png: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("png: {e}"))?;
    let px = (info.width * info.height) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3].chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..px * 2].chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("png: palette not expanded".into()),
    };
    Ok(Image { w: info.width, h: info.height, rgba })
}

/// A horizontal strip of `n` frames, each `w / n` wide (as Fangame.ModLoader cuts it).
pub fn frames(strip: &Image, n: u32) -> Vec<Image> {
    let n = n.max(1);
    let fw = strip.w / n;
    (0..n)
        .map(|i| {
            let mut rgba = Vec::with_capacity((fw * strip.h * 4) as usize);
            for y in 0..strip.h {
                let row = ((y * strip.w + i * fw) * 4) as usize;
                rgba.extend_from_slice(&strip.rgba[row..row + (fw * 4) as usize]);
            }
            Image { w: fw, h: strip.h, rgba }
        })
        .collect()
}
