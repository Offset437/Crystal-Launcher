//! The game's settings as values: the numbered entries under
//! `...\SquareEnix\FinalFantasyXI`, their meanings, defaults, and the `.reg`
//! text codec the native client reads.
//!
//! Meanings come from the community's documentation of the key (Ashita v4's
//! boot-config reference, FFXIclopedia's registry notes) and from what the
//! native client's own sources do with them (host64 `modern.c`, `signin.c`,
//! `user32.h`). Values the page does not manage are read, kept and backed up
//! but never changed.
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One stored value, as the registry types it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Data {
    Dword(u32),
    Text(String),
    Binary(Vec<u8>),
}

/// Value name ("0001", "padmode000", ...) to its data.
pub(crate) type Values = BTreeMap<String, Data>;

// ---- the managed entries ----
pub(crate) const MIP_MAPPING: &str = "0000";
pub(crate) const WINDOW_W: &str = "0001";
pub(crate) const WINDOW_H: &str = "0002";
pub(crate) const BACKGROUND_W: &str = "0003";
pub(crate) const BACKGROUND_H: &str = "0004";
pub(crate) const SOUND: &str = "0007";
pub(crate) const ENV_ANIMATION: &str = "0011";
pub(crate) const BUMP_MAPPING: &str = "0017";
pub(crate) const TEXTURE_COMPRESSION: &str = "0018";
pub(crate) const MAP_COMPRESSION: &str = "0019";
pub(crate) const HARDWARE_MOUSE: &str = "0021";
pub(crate) const OPENING_MOVIE: &str = "0022";
pub(crate) const SIMPLE_CREATION: &str = "0023";
pub(crate) const GAMMA: &str = "0028";
pub(crate) const MAX_SOUNDS: &str = "0029";
pub(crate) const WINDOW_MODE: &str = "0034";
pub(crate) const SOUND_UNFOCUSED: &str = "0035";
pub(crate) const FONT_TYPE: &str = "0036";
pub(crate) const MENU_W: &str = "0037";
pub(crate) const MENU_H: &str = "0038";
pub(crate) const STABILIZATION: &str = "0040";
pub(crate) const SCREENSHOT_DIR: &str = "0042";
pub(crate) const SCREENSHOT_SCREEN_RES: &str = "0043";
pub(crate) const KEEP_ASPECT: &str = "0044";

/// Every entry the page edits. Anything else under the key is left alone.
pub(crate) const MANAGED: [&str; 24] = [
    MIP_MAPPING,
    WINDOW_W,
    WINDOW_H,
    BACKGROUND_W,
    BACKGROUND_H,
    SOUND,
    ENV_ANIMATION,
    BUMP_MAPPING,
    TEXTURE_COMPRESSION,
    MAP_COMPRESSION,
    HARDWARE_MOUSE,
    OPENING_MOVIE,
    SIMPLE_CREATION,
    GAMMA,
    MAX_SOUNDS,
    WINDOW_MODE,
    SOUND_UNFOCUSED,
    FONT_TYPE,
    MENU_W,
    MENU_H,
    STABILIZATION,
    SCREENSHOT_DIR,
    SCREENSHOT_SCREEN_RES,
    KEEP_ASPECT,
];

/// Labels for the enumerated entries, in value order.
/// The text shown is translated, the stored value (the number) never is.
pub(crate) fn window_modes() -> [(u32, &'static str); 4] {
    [
        (0, t("Full screen")),
        (1, t("Windowed")),
        (2, t("Borderless window")),
        (3, t("Borderless full screen")),
    ]
}
pub(crate) fn mip_levels() -> [(u32, &'static str); 7] {
    [
        (0, t("Off")),
        (1, t("Level 1")),
        (2, t("Level 2")),
        (3, t("Level 3")),
        (4, t("Level 4")),
        (5, t("Level 5")),
        (6, t("Level 6 (best)")),
    ]
}
pub(crate) fn texture_levels() -> [(u32, &'static str); 3] {
    [
        (0, t("High compression")),
        (1, t("Low compression")),
        (2, t("Uncompressed (best)")),
    ]
}
pub(crate) fn map_levels() -> [(u32, &'static str); 2] {
    [(0, t("Compressed")), (1, t("Uncompressed (sharper)"))]
}
pub(crate) fn animation_levels() -> [(u32, &'static str); 3] {
    [(0, t("Off")), (1, t("Normal")), (2, t("Smooth"))]
}
pub(crate) fn font_types() -> [(u32, &'static str); 3] {
    [(0, t("Compressed")), (1, t("Uncompressed")), (2, t("High quality"))]
}

/// The only font type the native client starts with. Uncompressed (1) and
/// High quality (2) stop host64 during startup, right after the D3D8 device
/// is made, with "FATAL at 102dbbc4: indirect call/jump to an address with no
/// translation": FFXiMain's image loader longjmps back into itself, a path the
/// recompiled client cannot follow. Every other managed value starts fine.
pub(crate) const NATIVE_FONT_TYPE: u32 = 0;

/// Whether the native client starts with `value` for `key`.
pub(crate) fn native_supports(key: &str, value: u32) -> bool {
    match key {
        FONT_TYPE => value == NATIVE_FONT_TYPE,
        _ => true,
    }
}

/// Replaces stored values the native client cannot start with by ones it
/// can; returns the keys it changed. Missing entries stay missing.
pub(crate) fn guard_native(values: &mut Values) -> Vec<&'static str> {
    if matches!(values.get(FONT_TYPE), None | Some(Data::Dword(NATIVE_FONT_TYPE))) {
        return Vec::new();
    }
    values.insert(FONT_TYPE.into(), Data::Dword(NATIVE_FONT_TYPE));
    vec![FONT_TYPE]
}

pub(crate) const MAX_SOUNDS_RANGE: (u32, u32) = (12, 20);
pub(crate) const MIN_WINDOW: (u32, u32) = (640, 480);
/// The smallest interface the native client's sign-in screen accepts.
pub(crate) const MIN_MENU: (u32, u32) = (512, 384);
/// Interface magnifications offered for the menu resolution.
pub(crate) const MENU_SCALES: [f32; 7] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/// A sensible starting point for `desktop` (w, h): the native client's own
/// first-run defaults (`signin.c`), sized to the screen.
pub(crate) fn defaults(desktop: (u32, u32)) -> Values {
    let (w, h) = (desktop.0.max(MIN_WINDOW.0), desktop.1.max(MIN_WINDOW.1));
    let (mw, mh) = menu_for(w, h, default_menu_scale(h));
    let mut v = Values::new();
    let mut d = |k: &str, x: u32| {
        v.insert(k.into(), Data::Dword(x));
    };
    d(MIP_MAPPING, 6);
    d(WINDOW_W, w);
    d(WINDOW_H, h);
    d(BACKGROUND_W, 4096);
    d(BACKGROUND_H, 4096);
    d(SOUND, 1);
    d(ENV_ANIMATION, 1);
    d(BUMP_MAPPING, 1);
    d(TEXTURE_COMPRESSION, 1);
    d(MAP_COMPRESSION, 1);
    d(HARDWARE_MOUSE, 1);
    d(OPENING_MOVIE, 1);
    d(SIMPLE_CREATION, 0);
    d(GAMMA, 0f32.to_bits());
    d(MAX_SOUNDS, 12);
    d(WINDOW_MODE, 1);
    d(SOUND_UNFOCUSED, 1);
    d(FONT_TYPE, 0);
    d(MENU_W, mw);
    d(MENU_H, mh);
    d(STABILIZATION, 0);
    d(SCREENSHOT_SCREEN_RES, 0);
    d(KEEP_ASPECT, 1);
    v.insert(SCREENSHOT_DIR.into(), Data::Text(String::new()));
    v
}

/// The interface magnification that keeps the menus readable: about 1080
/// lines of interface at most, in the scale list's steps.
pub(crate) fn default_menu_scale(window_h: u32) -> f32 {
    let wanted = window_h as f32 / 1080.0;
    MENU_SCALES
        .iter()
        .copied()
        .filter(|s| *s <= wanted.max(1.0) + 0.01)
        .last()
        .unwrap_or(1.0)
}

/// The menu size for a window at an interface scale, rounded to even sizes as
/// the native client's Config > Display does.
pub(crate) fn menu_for(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let even = |x: f32| ((x.round() as u32) + 1) & !1;
    (even(w as f32 / scale), even(h as f32 / scale))
}

/// The interface scale a menu size gives a window (1.0 when unknown).
pub(crate) fn scale_of(window_h: u32, menu_h: u32) -> f32 {
    if menu_h == 0 {
        1.0
    } else {
        window_h as f32 / menu_h as f32
    }
}

pub(crate) fn dword(values: &Values, key: &str) -> Option<u32> {
    match values.get(key) {
        Some(Data::Dword(v)) => Some(*v),
        _ => None,
    }
}

pub(crate) fn text_value<'a>(values: &'a Values, key: &str) -> Option<&'a str> {
    match values.get(key) {
        Some(Data::Text(v)) => Some(v.as_str()),
        _ => None,
    }
}

/// Gamma is a float stored as its bits; -1.0 (darker) to 1.0 (brighter).
pub(crate) fn gamma_of(bits: u32) -> f32 {
    let g = f32::from_bits(bits);
    if g.is_finite() {
        g.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Gamma as stored: two decimals, so a slider's float noise never shows up as
/// a pending change.
pub(crate) fn gamma_bits(g: f32) -> u32 {
    let g = (g.clamp(-1.0, 1.0) * 100.0).round() / 100.0;
    // -0.0 and 0.0 are the same setting.
    if g == 0.0 {
        0
    } else {
        g.to_bits()
    }
}

/// The managed entries whose value differs between `original` and `edited`.
pub(crate) fn changed_keys(original: &Values, edited: &Values) -> Vec<&'static str> {
    MANAGED
        .iter()
        .copied()
        .filter(|k| original.get(*k) != edited.get(*k))
        .collect()
}

/// Checks a set of values before it is written.
pub(crate) fn validate(values: &Values, ascii_paths_only: bool) -> Result<(), String> {
    let get = |k| dword(values, k).unwrap_or(0);
    if get(WINDOW_W) < MIN_WINDOW.0 || get(WINDOW_H) < MIN_WINDOW.1 {
        return Err(t("The window resolution must be at least 640 × 480.").into());
    }
    // Windower uses a zero pair for automatic (1x) menus. It is a valid
    // retail setting, not a zero-sized interface; partial pairs are invalid.
    let automatic_menu = !ascii_paths_only && get(MENU_W) == 0 && get(MENU_H) == 0;
    if !automatic_menu && (get(MENU_W) < MIN_MENU.0 || get(MENU_H) < MIN_MENU.1) {
        return Err(t("The menu resolution must be at least 512 × 384.").into());
    }
    if get(BACKGROUND_W) < 256 || get(BACKGROUND_H) < 256 {
        return Err(t("The background resolution must be at least 256 × 256.").into());
    }
    if let Some(n) = dword(values, MAX_SOUNDS) {
        if !(MAX_SOUNDS_RANGE.0..=MAX_SOUNDS_RANGE.1).contains(&n) {
            return Err(t("Maximum sounds must be between 12 and 20.").into());
        }
    }
    if let Some(dir) = text_value(values, SCREENSHOT_DIR) {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = std::path::Path::new(dir);
            if !path.is_absolute() {
                return Err(t("Enter the screenshot folder as a full path, such as D:\\Pictures\\FFXI.").into());
            }
            if ascii_paths_only && !dir.is_ascii() {
                return Err(t("The native client reads the screenshot folder as plain ASCII; choose a folder without accented or non-Latin characters.").into());
            }
            if dir.len() > 200 {
                return Err(t("The screenshot folder path is too long for the game.").into());
            }
            if !path.is_dir() {
                return Err(t("The screenshot folder does not exist. Create it first, or leave the field empty to use the game's default.").into());
            }
        }
    }
    Ok(())
}

// ---- .reg text ----

/// Decodes a `.reg` file's bytes: UTF-16LE with a BOM (what `reg export`
/// writes), UTF-8 with or without a BOM, else Latin-1.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// The section header for a vendor key ("PlayOnlineUS"), as the native
/// client's registry names it once WOW6432Node is folded away.
pub(crate) fn section_path(vendor: &str) -> String {
    format!("HKEY_LOCAL_MACHINE\\SOFTWARE\\{vendor}\\SquareEnix\\FinalFantasyXI")
}

fn fold_header(header: &str) -> String {
    let lower = header.trim().to_ascii_lowercase();
    lower.replacen("\\software\\wow6432node\\", "\\software\\", 1)
}

/// The FinalFantasyXI values of `vendor` in a `.reg` text, later lines
/// winning. Other sections are ignored.
pub(crate) fn parse_reg(text: &str, vendor: &str) -> Values {
    let wanted = fold_header(&section_path(vendor));
    let mut values = Values::new();
    let mut inside = false;
    let mut pending = String::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        // A trailing backslash continues a hex value on the next line.
        if !pending.is_empty() {
            pending.push_str(raw.trim_start());
        } else {
            pending.push_str(raw);
        }
        if pending.ends_with('\\') && !pending.trim_start().starts_with('[') && hex_continues(&pending) {
            pending.pop();
            continue;
        }
        let line = std::mem::take(&mut pending);
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            let header = rest.split(']').next().unwrap_or("");
            inside = !header.starts_with('-') && fold_header(header) == wanted;
            continue;
        }
        if !inside || !(line.starts_with('"') || line.starts_with('@')) {
            continue;
        }
        if let Some((name, data)) = parse_value(line) {
            match data {
                Some(data) => {
                    values.insert(name, data);
                }
                None => {
                    values.remove(&name);
                }
            }
        }
    }
    values
}

/// Whether a line ending in a backslash is a hex value's continuation rather
/// than a string ending in an escaped backslash.
fn hex_continues(line: &str) -> bool {
    line.split_once('=')
        .map(|(_, v)| v.trim_start().starts_with("hex"))
        .unwrap_or(false)
}

/// `"name"=...` into its name and data; `None` data for a deletion (`=-`).
fn parse_value(line: &str) -> Option<(String, Option<Data>)> {
    let (name, rest) = if let Some(rest) = line.strip_prefix('@') {
        (String::new(), rest)
    } else {
        let (name, rest) = unquote(&line[1..])?;
        (name, rest)
    };
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    if rest == "-" {
        return Some((name, None));
    }
    if let Some(s) = rest.strip_prefix('"') {
        let (s, _) = unquote(s)?;
        return Some((name, Some(Data::Text(s))));
    }
    if let Some(hex) = rest.strip_prefix("dword:") {
        let v = u32::from_str_radix(hex.trim(), 16).ok()?;
        return Some((name, Some(Data::Dword(v))));
    }
    if let Some(rest) = rest.strip_prefix("hex") {
        let (kind, bytes) = match rest.strip_prefix('(') {
            Some(r) => {
                let (k, b) = r.split_once(')')?;
                (u32::from_str_radix(k.trim(), 16).ok()?, b)
            }
            None => (3, rest),
        };
        let bytes = bytes.trim_start().strip_prefix(':')?;
        let raw: Vec<u8> = bytes
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|b| u8::from_str_radix(b, 16))
            .collect::<Result<_, _>>()
            .ok()?;
        // hex(1) is a string, as the native client writes a path holding a backslash.
        if kind == 1 {
            let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
            let s: String = raw[..end].iter().map(|&b| b as char).collect();
            return Some((name, Some(Data::Text(s))));
        }
        if kind == 4 && raw.len() == 4 {
            return Some((name, Some(Data::Dword(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])))));
        }
        return Some((name, Some(Data::Binary(raw))));
    }
    None
}

/// A quoted string's contents (after the opening quote) and what follows the
/// closing quote. `\\` and `\"` are unescaped.
fn unquote(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(n);
                }
            }
            '"' => return Some((out, &s[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A REGEDIT4 file (CRLF) holding `values` under the vendor's FinalFantasyXI
/// key, in the line format the native client's loader and its in-game
/// Config > Display rewriter both accept.
pub(crate) fn encode_reg(values: &Values, vendor: &str) -> String {
    let mut out = format!("REGEDIT4\r\n\r\n[{}]\r\n", section_path(vendor));
    for (name, data) in values {
        let name = if name.is_empty() {
            "@".to_owned()
        } else {
            format!("\"{}\"", quote(name))
        };
        match data {
            Data::Dword(v) => out.push_str(&format!("{name}=dword:{v:08x}\r\n")),
            Data::Text(s) => out.push_str(&format!("{name}=\"{}\"\r\n", quote(s))),
            Data::Binary(b) => {
                let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
                out.push_str(&format!("{name}=hex:{}\r\n", hex.join(",")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\PlayOnlineUS\\InstallFolder]\r\n\
\"0001\"=\"C:\\\\Games\\\\FINAL FANTASY XI\\\\\"\r\n\r\n\
[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\
\"padmode000\"=\"1,1,0,0,0,1\"\r\n\
\"bFirst\"=hex:00\r\n\
\"0000\"=dword:00000006\r\n\
\"0001\"=dword:00000d70\r\n\
\"0002\"=dword:000005a0\r\n\
\"0031\"=dword:3bc49ba6\r\n\
\"0042\"=\"D:\\\\Shots \\\"XI\\\"\"\r\n";

    #[test]
    fn game_config_parses_registry_export_and_folds_wow6432node() {
        let v = parse_reg(SAMPLE, "PlayOnlineUS");
        assert_eq!(dword(&v, "0000"), Some(6));
        assert_eq!(dword(&v, "0001"), Some(3440));
        assert_eq!(dword(&v, "0002"), Some(1440));
        assert_eq!(dword(&v, "0031"), Some(0x3bc49ba6));
        assert_eq!(v.get("bFirst"), Some(&Data::Binary(vec![0])));
        assert_eq!(text_value(&v, "padmode000"), Some("1,1,0,0,0,1"));
        assert_eq!(text_value(&v, "0042"), Some("D:\\Shots \"XI\""));
        // InstallFolder's 0001 belongs to another key.
        assert!(!v.values().any(|d| matches!(d, Data::Text(t) if t.contains("Games"))));
        // Another region's section is not this one.
        assert!(parse_reg(SAMPLE, "PlayOnlineEU").is_empty());
    }

    #[test]
    fn game_config_decodes_utf16_exports() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in SAMPLE.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let text = decode_text(&bytes);
        assert_eq!(parse_reg(&text, "PlayOnlineUS"), parse_reg(SAMPLE, "PlayOnlineUS"));
        assert_eq!(decode_text(b"\xEF\xBB\xBFREGEDIT4"), "REGEDIT4");
    }

    #[test]
    fn game_config_later_lines_and_files_win_and_deletions_apply() {
        let text = "REGEDIT4\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\n\
\"0001\"=dword:00000500\n\"0034\"=dword:00000001\n\"0001\"=dword:00000780\n\"0034\"=-\n";
        let v = parse_reg(text, "PlayOnlineUS");
        assert_eq!(dword(&v, "0001"), Some(1920));
        assert_eq!(v.get("0034"), None);
    }

    #[test]
    fn game_config_reads_native_client_overlay_forms() {
        // saved.reg as the native client writes it: hex(1) strings, multi-line hex.
        let text = "REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\n\
\"0042\"=hex(1):43,3a,5c,58,00\n\"blob\"=hex:01,02,\\\n  03,04\n\"0044\"=dword:00000001\n";
        let v = parse_reg(text, "PlayOnlineUS");
        assert_eq!(text_value(&v, "0042"), Some("C:\\X"));
        assert_eq!(v.get("blob"), Some(&Data::Binary(vec![1, 2, 3, 4])));
        assert_eq!(dword(&v, "0044"), Some(1));
    }

    #[test]
    fn game_config_encode_round_trips_every_type() {
        let mut v = defaults((2560, 1440));
        v.insert(SCREENSHOT_DIR.into(), Data::Text("D:\\Pictures\\\"FFXI\"".into()));
        v.insert("bFirst".into(), Data::Binary(vec![0, 0xAB]));
        v.insert("padmode000".into(), Data::Text("1,1,0,0,0,1".into()));
        let text = encode_reg(&v, "PlayOnlineUS");
        assert!(text.starts_with("REGEDIT4\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n"));
        // The native client's Config > Display rewrites lines of exactly this shape.
        assert!(text.contains("\"0001\"=dword:00000a00\r\n"));
        assert_eq!(parse_reg(&text, "PlayOnlineUS"), v);
    }

    #[test]
    fn game_config_gamma_is_float_bits_with_two_decimals() {
        assert_eq!(gamma_bits(0.0), 0);
        assert_eq!(gamma_bits(-0.0), 0);
        assert_eq!(gamma_bits(1.0), 0x3f80_0000);
        assert_eq!(gamma_bits(-1.0), 0xbf80_0000);
        assert_eq!(gamma_bits(5.0), 0x3f80_0000);
        assert_eq!(gamma_of(gamma_bits(-0.4012)), -0.4);
        assert_eq!(gamma_of(0x7fc0_0000), 0.0); // NaN reads as neutral
    }

    #[test]
    fn game_config_menu_resolution_follows_interface_scale() {
        assert_eq!(menu_for(1920, 1080, 1.0), (1920, 1080));
        assert_eq!(menu_for(1920, 1080, 1.5), (1280, 720));
        assert_eq!(menu_for(3840, 2160, 2.0), (1920, 1080));
        // Odd results round up to even, as the native client stores them.
        assert_eq!(menu_for(1366, 768, 1.25), (1094, 614));
        assert_eq!(default_menu_scale(1080), 1.0);
        assert_eq!(default_menu_scale(1440), 1.25);
        assert_eq!(default_menu_scale(2160), 2.0);
        assert_eq!(default_menu_scale(720), 1.0);
        assert!((scale_of(2160, 1080) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn game_config_defaults_are_complete_and_valid() {
        let d = defaults((1920, 1080));
        for key in MANAGED {
            assert!(d.contains_key(key), "default for {key}");
        }
        assert_eq!(dword(&d, WINDOW_W), Some(1920));
        assert_eq!(dword(&d, MENU_H), Some(1080));
        validate(&d, true).unwrap();
        // A tiny desktop still yields a usable window.
        assert_eq!(dword(&defaults((320, 200)), WINDOW_W), Some(640));
    }

    #[test]
    fn game_config_changed_keys_only_reports_managed_differences() {
        let original = parse_reg(SAMPLE, "PlayOnlineUS");
        let mut edited = original.clone();
        edited.insert("0031".into(), Data::Dword(1)); // unmanaged
        assert!(changed_keys(&original, &edited).is_empty());
        edited.insert(MIP_MAPPING.into(), Data::Dword(3));
        edited.insert(SOUND.into(), Data::Dword(1)); // absent before
        assert_eq!(changed_keys(&original, &edited), vec![MIP_MAPPING, SOUND]);
    }

    #[test]
    fn game_config_native_guard_keeps_font_type_compressed() {
        // 0036 = 1 or 2 alone stops host64 at startup (FATAL at 102dbbc4); 0 starts.
        assert!(native_supports(FONT_TYPE, 0));
        assert!(!native_supports(FONT_TYPE, 1));
        assert!(!native_supports(FONT_TYPE, 2));
        assert!(native_supports(WINDOW_MODE, 3));
        // The settings file that crashed the client: only the font type changes.
        let text = "REGEDIT4\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\
\"0000\"=dword:00000006\r\n\"0001\"=dword:00000f00\r\n\"0002\"=dword:00000870\r\n\"0003\"=dword:00001000\r\n\
\"0004\"=dword:00001000\r\n\"0018\"=dword:00000002\r\n\"0021\"=dword:00000001\r\n\"0022\"=dword:00000000\r\n\
\"0034\"=dword:00000003\r\n\"0036\"=dword:00000002\r\n\"0037\"=dword:00000a00\r\n\"0038\"=dword:000005a0\r\n";
        let crashed = parse_reg(text, "PlayOnlineUS");
        let mut v = crashed.clone();
        assert_eq!(guard_native(&mut v), vec![FONT_TYPE]);
        assert_eq!(dword(&v, FONT_TYPE), Some(NATIVE_FONT_TYPE));
        v.insert(FONT_TYPE.into(), Data::Dword(2));
        assert_eq!(v, crashed);
        // Already safe, or absent: nothing changes.
        let mut d = defaults((3840, 2160));
        assert!(guard_native(&mut d).is_empty());
        d.remove(FONT_TYPE);
        assert!(guard_native(&mut d).is_empty());
        assert!(!d.contains_key(FONT_TYPE));
        // A value of the wrong type is replaced too.
        d.insert(FONT_TYPE.into(), Data::Text("2".into()));
        assert_eq!(guard_native(&mut d), vec![FONT_TYPE]);
        assert_eq!(dword(&d, FONT_TYPE), Some(0));
    }

    #[test]
    fn game_config_validation_rejects_unusable_values() {
        let mut v = defaults((1920, 1080));
        v.insert(WINDOW_W.into(), Data::Dword(320));
        assert!(validate(&v, false).is_err());
        let mut v = defaults((1920, 1080));
        v.insert(MAX_SOUNDS.into(), Data::Dword(40));
        assert!(validate(&v, false).is_err());
        let mut v = defaults((1920, 1080));
        v.insert(SCREENSHOT_DIR.into(), Data::Text("relative\\shots".into()));
        assert!(validate(&v, false).is_err());
        let dir = std::env::temp_dir();
        v.insert(SCREENSHOT_DIR.into(), Data::Text(dir.display().to_string()));
        validate(&v, false).unwrap();
        v.insert(SCREENSHOT_DIR.into(), Data::Text("C:\\Bilder\\Größe".into()));
        let err = validate(&v, true).unwrap_err();
        assert!(err.contains("ASCII"));
    }
}
