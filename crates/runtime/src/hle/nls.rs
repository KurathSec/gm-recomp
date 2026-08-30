//! Case mapping as the guest sees it. Rust's tables follow the newest Unicode version
//! of whichever compiler built the host, so mappings are limited to the scripts whose
//! case pairs predate Windows 7 (Unicode 5.1); characters outside them map to
//! themselves. This keeps results identical across hosts and close to Windows.

const RANGES: &[(u32, u32)] = &[
    (0x0041, 0x024F), // Basic Latin .. Latin Extended-B
    (0x0250, 0x02AF), // IPA extensions (results must stay in range)
    (0x0370, 0x058F), // Greek, Cyrillic, Cyrillic Supplement, Armenian
    (0x10A0, 0x10FF), // Georgian
    (0x1E00, 0x1FFF), // Latin Extended Additional, Greek Extended
    (0x2160, 0x218F), // Roman numerals
    (0x24B6, 0x24E9), // circled letters
    (0x2C00, 0x2C5F), // Glagolitic
    (0x2C60, 0x2C7F), // Latin Extended-C
    (0x2C80, 0x2CFF), // Coptic
    (0x2D00, 0x2D2F), // Georgian Supplement
    (0xFF21, 0xFF5A), // fullwidth Latin
];

fn mapped(ch: char) -> bool {
    let c = ch as u32;
    RANGES.iter().any(|&(a, b)| (a..=b).contains(&c))
}

fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
    let a = it.next()?;
    if it.next().is_some() { None } else { Some(a) }
}

pub fn upper(ch: char) -> char {
    if !mapped(ch) {
        return ch;
    }
    match single(ch.to_uppercase()) {
        Some(u) if mapped(u) => u,
        _ => ch,
    }
}

pub fn lower(ch: char) -> char {
    if !mapped(ch) {
        return ch;
    }
    match single(ch.to_lowercase()) {
        Some(l) if mapped(l) => l,
        _ => ch,
    }
}

pub fn upper_str(s: &str) -> String {
    s.chars().map(upper).collect()
}

pub fn lower_str(s: &str) -> String {
    s.chars().map(lower).collect()
}
