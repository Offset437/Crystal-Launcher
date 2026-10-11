//! FINAL FANTASY XI's gamepad settings and where each client keeps them.
//!
//! The game reads four string values from
//! `HKLM\SOFTWARE\[WOW6432Node\]PlayOnline{US,EU,}\SquareEnix\FinalFantasyXI`:
//!
//! * `padmode000`: six 0/1 flags: gamepad on, force feedback, sliders, hat
//!   switches, keep working while the window is inactive, XInput.
//! * `padsin000`: one button/axis id per game function (27 slots, see
//!   [`SLOT_COUNT`]); `-1` is unbound. Ids 0.. are buttons, 32.. are axes and a
//!   minus sign reverses an axis. What an id means depends on the input API:
//!   with XInput on the game numbers the XInput buttons its own way; with it
//!   off ids are DirectInput button numbers and DIJOYSTATE axis offsets.
//! * `padguid000`: the DirectInput instance the game binds to (all zero: any).
//! * `padexsin000` exists on newer installs; it is preserved, never edited.
//!
//! Retail FFXI (Windower, Ashita) reads the Windows registry. The original
//! values are backed up as a `.reg` file before the first change.
use super::detect::Btn;
use crate::i18n::{t, tf};
use std::path::{Path, PathBuf};

pub(crate) const SLOT_COUNT: usize = 27;
pub(crate) const MODE_COUNT: usize = 6;
/// The XInput defaults the native client ships in `ffxi.reg` (also what the
/// FFXI config tool writes for an Xbox controller).
pub(crate) const DEFAULT_XINPUT_SIN: [i32; SLOT_COUNT] = [
    8, 9, 13, 12, 10, 0, 1, 3, 2, 15, -1, -1, 14, -33, -33, 32, 32, -36, -36, 35, 35, 6, 7, 5, 4,
    11, -1,
];
pub(crate) const DEFAULT_MODE: [i32; MODE_COUNT] = [1, 1, 0, 0, 0, 1];

/// `padmode000` flag positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flag {
    Enabled = 0,
    ForceFeedback = 1,
    Background = 4,
    XInput = 5,
}

/// How a `padsin000` id maps to a physical control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// XInput on: FFXI's own XInput numbering.
    XInput,
    /// DirectInput, Xbox-style device (the native client's DirectInput view, or
    /// an Xbox pad through Windows' HID driver).
    DInputXbox,
    /// DirectInput, Sony HID numbering (DualShock 4 / DualSense).
    DInputSony,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stick {
    Left,
    Right,
    Dpad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AxisDir {
    X,
    Y,
}

/// One decoded `padsin000` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    None,
    Button(Btn),
    Axis(Stick, AxisDir, bool),
    /// An id this layout does not name; shown as a number, kept as is.
    Raw(i32),
}

const XINPUT_BUTTONS: [Btn; 16] = [
    Btn::East,
    Btn::West,
    Btn::North,
    Btn::South,
    Btn::DRight,
    Btn::DLeft,
    Btn::DUp,
    Btn::DDown,
    Btn::LB,
    Btn::LT,
    Btn::LS,
    Btn::RB,
    Btn::RT,
    Btn::RS,
    Btn::Start,
    Btn::Back,
];
const DI_XBOX_BUTTONS: [Btn; 11] = [
    Btn::South,
    Btn::East,
    Btn::West,
    Btn::North,
    Btn::LB,
    Btn::RB,
    Btn::Back,
    Btn::Start,
    Btn::LS,
    Btn::RS,
    Btn::Guide,
];
// Sony's USB HID descriptor exposes touchpad click as button 14 and the
// DualSense mic button as button 15 (zero-based ids 13/14 in padsin000).
// Bluetooth omits the mic button; saving id 14 cannot make it appear there.
const DI_SONY_BUTTONS: [Btn; 15] = [
    Btn::West,
    Btn::South,
    Btn::East,
    Btn::North,
    Btn::LB,
    Btn::RB,
    Btn::LT,
    Btn::RT,
    Btn::Back,
    Btn::Start,
    Btn::LS,
    Btn::RS,
    Btn::Guide,
    Btn::Touchpad,
    Btn::Mute,
];

impl Layout {
    fn buttons(self) -> &'static [Btn] {
        match self {
            Layout::XInput => &XINPUT_BUTTONS,
            Layout::DInputXbox => &DI_XBOX_BUTTONS,
            Layout::DInputSony => &DI_SONY_BUTTONS,
        }
    }
    /// Axis ids: 32 + the DIJOYSTATE axis index (X, Y, Z, Rx, Ry, Rz), the
    /// POV hat as 40 (x) / 41 (y).
    fn axis_id(self, stick: Stick, dir: AxisDir) -> Option<i32> {
        Some(match (self, stick, dir) {
            (_, Stick::Left, AxisDir::X) => 32,
            (_, Stick::Left, AxisDir::Y) => 33,
            (Layout::DInputSony, Stick::Right, AxisDir::X) => 34,
            (Layout::DInputSony, Stick::Right, AxisDir::Y) => 37,
            (_, Stick::Right, AxisDir::X) => 35,
            (_, Stick::Right, AxisDir::Y) => 36,
            (Layout::XInput, Stick::Dpad, _) => return None,
            (_, Stick::Dpad, AxisDir::X) => 40,
            (_, Stick::Dpad, AxisDir::Y) => 41,
        })
    }
    pub(crate) fn decode(self, id: i32) -> Control {
        if id == -1 || (self == Layout::XInput && id == 16) {
            return Control::None;
        }
        if (0..32).contains(&id) {
            return self
                .buttons()
                .get(id as usize)
                .map_or(Control::Raw(id), |b| Control::Button(*b));
        }
        let (neg, abs) = (id < 0, id.unsigned_abs() as i32);
        for stick in [Stick::Left, Stick::Right, Stick::Dpad] {
            for dir in [AxisDir::X, AxisDir::Y] {
                if self.axis_id(stick, dir) == Some(abs) {
                    return Control::Axis(stick, dir, neg);
                }
            }
        }
        Control::Raw(id)
    }
    pub(crate) fn encode_button(self, b: Btn) -> Option<i32> {
        self.buttons().iter().position(|x| *x == b).map(|i| i as i32)
    }
    fn encode(self, c: Control) -> i32 {
        match c {
            Control::None => -1,
            Control::Button(b) => self.encode_button(b).unwrap_or(-1),
            Control::Axis(stick, dir, neg) => self
                .axis_id(stick, dir)
                .map_or(-1, |id| if neg { -id } else { id }),
            Control::Raw(id) => id,
        }
    }
    /// The four `padsin000` ids (up, down, left, right) for a direction group.
    pub(crate) fn group(self, stick: Stick) -> [i32; 4] {
        match (self, stick) {
            (Layout::XInput, Stick::Dpad) => [6, 7, 5, 4],
            _ => {
                let y = self.axis_id(stick, AxisDir::Y).unwrap_or(-1);
                let x = self.axis_id(stick, AxisDir::X).unwrap_or(-1);
                // Y reversed, as the shipped defaults have it.
                [-y, -y, x, x]
            }
        }
    }
    /// Which stick a direction group's four ids describe, if they match one.
    pub(crate) fn group_stick(self, ids: &[i32]) -> Option<Stick> {
        [Stick::Left, Stick::Right, Stick::Dpad].into_iter().find(|s| {
            let want = self.group(*s);
            ids.len() == 4
                && (0..4).all(|i| want[i].unsigned_abs() == ids[i].unsigned_abs())
        })
    }
}

/// The rows the bindings list shows, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Single(usize),
    /// Four slots from here: up, down, left, right.
    Group(usize),
}

pub(crate) const ROWS: [Row; 18] = [
    Row::Single(7),
    Row::Single(5),
    Row::Single(6),
    Row::Single(8),
    Row::Group(13),
    Row::Group(17),
    Row::Group(21),
    Row::Single(4),
    Row::Single(0),
    Row::Single(1),
    Row::Single(3),
    Row::Single(2),
    Row::Single(9),
    Row::Single(10),
    Row::Single(11),
    Row::Single(26),
    Row::Single(25),
    Row::Single(12),
];

impl Row {
    /// What the game function bound to this row is called in the list.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Row::Single(7) => t("Confirm"),
            Row::Single(5) => t("Cancel"),
            Row::Single(6) => t("Main menu"),
            Row::Single(8) => t("Active window"),
            Row::Group(13) => t("Move character"),
            Row::Group(17) => t("Camera"),
            Row::Group(21) => t("Menu cursor / target"),
            Row::Single(4) => t("Heal / lock target"),
            Row::Single(0) => t("Auto-run"),
            Row::Single(1) => t("Macro palette (Ctrl)"),
            Row::Single(3) => t("Macro palette (Alt)"),
            Row::Single(2) => t("First / third person"),
            Row::Single(9) => t("Hide windows"),
            Row::Single(10) => t("Hold: menus with stick"),
            Row::Single(11) => t("Hold: camera with stick"),
            Row::Single(26) => t("Swap stick roles"),
            Row::Single(25) => t("Screenshot"),
            Row::Single(12) => t("Log out"),
            _ => "",
        }
    }
}

/// The values the game reads, as stored (lists keep their length).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PadConfig {
    pub mode: Vec<i32>,
    pub sin: Vec<i32>,
    pub guid: String,
}

impl Default for PadConfig {
    fn default() -> Self {
        Self {
            mode: DEFAULT_MODE.to_vec(),
            sin: DEFAULT_XINPUT_SIN.to_vec(),
            guid: String::new(),
        }
    }
}

pub(crate) fn parse_list(value: &str) -> Option<Vec<i32>> {
    value
        .split(',')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.parse::<i32>().ok())
        .collect()
}

pub(crate) fn format_list(values: &[i32]) -> String {
    values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
}

impl PadConfig {
    /// Builds a config from the stored strings; missing or malformed values
    /// fall back to the defaults (and lists are padded to their known length).
    pub(crate) fn from_values(mode: Option<&str>, sin: Option<&str>, guid: Option<&str>) -> Self {
        let mut mode = mode.and_then(parse_list).unwrap_or_else(|| DEFAULT_MODE.to_vec());
        let mut sin = sin.and_then(parse_list).unwrap_or_else(|| DEFAULT_XINPUT_SIN.to_vec());
        mode.resize(mode.len().max(MODE_COUNT), 0);
        sin.resize(sin.len().max(SLOT_COUNT), -1);
        Self {
            mode,
            sin,
            guid: guid.unwrap_or_default().to_string(),
        }
    }
    pub(crate) fn flag(&self, f: Flag) -> bool {
        self.mode.get(f as usize).is_some_and(|v| *v != 0)
    }
    /// Sets a flag. Turning XInput on or off renumbers the bindings so each
    /// function keeps its physical control.
    pub(crate) fn set_flag(&mut self, f: Flag, on: bool, di: Layout) {
        if f == Flag::XInput && self.flag(f) != on {
            let (from, to) = if on { (di, Layout::XInput) } else { (Layout::XInput, di) };
            self.translate(from, to);
        }
        self.mode[f as usize] = on as i32;
    }
    pub(crate) fn translate(&mut self, from: Layout, to: Layout) {
        for row in ROWS {
            match row {
                Row::Single(i) => self.sin[i] = to.encode(from.decode(self.sin[i])),
                Row::Group(i) => {
                    if let Some(stick) = from.group_stick(&self.sin[i..i + 4]) {
                        self.sin[i..i + 4].copy_from_slice(&to.group(stick));
                    } else {
                        for j in i..i + 4 {
                            self.sin[j] = to.encode(from.decode(self.sin[j]));
                        }
                    }
                }
            }
        }
    }
    /// The shipped defaults, in the current input mode.
    pub(crate) fn reset(&mut self, di: Layout) {
        let mut fresh = PadConfig::default();
        fresh.mode[Flag::XInput as usize] = self.flag(Flag::XInput) as i32;
        fresh.mode[Flag::Enabled as usize] = 1;
        if !fresh.flag(Flag::XInput) {
            fresh.translate(Layout::XInput, di);
        }
        self.mode[..MODE_COUNT].copy_from_slice(&fresh.mode);
        self.sin[..SLOT_COUNT].copy_from_slice(&fresh.sin);
    }
    /// Binds a button to a single-slot function; a function that already had
    /// that button takes over this slot's previous one (a swap), so no
    /// button ends up doing two things by accident.
    pub(crate) fn bind_button(&mut self, slot: usize, id: i32) {
        let old = self.sin[slot];
        for row in ROWS {
            if let Row::Single(other) = row {
                if other != slot && self.sin[other] == id {
                    self.sin[other] = old;
                }
            }
        }
        self.sin[slot] = id;
    }
    pub(crate) fn bind_group(&mut self, first: usize, ids: [i32; 4]) {
        let old: [i32; 4] = self.sin[first..first + 4].try_into().unwrap_or([-1; 4]);
        for row in ROWS {
            if let Row::Group(other) = row {
                if other != first && self.sin[other..other + 4] == ids {
                    self.sin[other..other + 4].copy_from_slice(&old);
                }
            }
        }
        self.sin[first..first + 4].copy_from_slice(&ids);
    }
    /// How many visible settings differ (flags plus binding rows).
    pub(crate) fn changes(&self, other: &PadConfig) -> usize {
        let flags = [Flag::Enabled, Flag::ForceFeedback, Flag::Background, Flag::XInput]
            .into_iter()
            .filter(|f| self.flag(*f) != other.flag(*f))
            .count();
        let rows = ROWS
            .iter()
            .filter(|row| match **row {
                Row::Single(i) => self.sin[i] != other.sin[i],
                Row::Group(i) => self.sin[i..i + 4] != other.sin[i..i + 4],
            })
            .count();
        let hidden = (self.mode != other.mode || self.sin != other.sin) as usize;
        (flags + rows).max(hidden)
    }
    pub(crate) fn values(&self) -> [(&'static str, String); 2] {
        [
            ("padmode000", format_list(&self.mode)),
            ("padsin000", format_list(&self.sin)),
        ]
    }
}

// ---- .reg files (the native client) ------------------------------------------

#[cfg(test)]
fn is_game_key(header: &str) -> bool {
    let h = header.to_ascii_lowercase().replace("\\wow6432node", "");
    h.starts_with("hkey_local_machine\\software\\playonline")
        && h.ends_with("\\squareenix\\finalfantasyxi")
}

#[cfg(test)]
fn unquote(s: &str) -> Option<String> {
    let s = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// Reads a `.reg` file's text (UTF-16 with BOM, UTF-8, or ANSI).
#[cfg(test)]
pub(crate) fn decode_reg_bytes(bytes: &[u8]) -> (String, bool) {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        (String::from_utf16_lossy(&units), true)
    } else {
        let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
        (String::from_utf8_lossy(b).into_owned(), false)
    }
}

pub(crate) fn encode_reg_text(text: &str, utf16: bool) -> Vec<u8> {
    if utf16 {
        let mut out = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    } else {
        text.as_bytes().to_vec()
    }
}

/// A string value from the game's key in `.reg` text (the last one wins, as
/// the client loads them).
#[cfg(test)]
pub(crate) fn reg_value(text: &str, name: &str) -> Option<String> {
    let mut in_key = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            in_key = line.trim_end().ends_with(']') && is_game_key(&line[1..line.trim_end().len() - 1]);
            continue;
        }
        if !in_key {
            continue;
        }
        if let Some(rest) = line.strip_prefix('"') {
            if let Some((n, v)) = rest.split_once("\"=") {
                if n.eq_ignore_ascii_case(name) {
                    found = unquote(v);
                }
            }
        }
    }
    found
}

fn reg_line(name: &str, value: &str) -> String {
    format!("\"{name}\"=\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Sets string values in the game's key of `.reg` text, replacing existing
/// lines in place and adding missing ones (and the key) as needed. Everything
/// else is kept byte for byte.
#[cfg(test)]
pub(crate) fn patch_reg_text(text: &str, values: &[(&str, String)]) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
    if text.is_empty() {
        lines = vec!["REGEDIT4".into(), String::new()];
    }
    let mut done = vec![false; values.len()];
    let mut header = None;
    let mut in_key = false;
    for (i, line) in lines.iter_mut().enumerate() {
        if line.starts_with('[') {
            let t = line.trim_end();
            in_key = t.ends_with(']') && is_game_key(&t[1..t.len() - 1]);
            if in_key {
                header = Some(i);
            }
            continue;
        }
        if !in_key {
            continue;
        }
        if let Some(rest) = line.strip_prefix('"') {
            if let Some((n, _)) = rest.split_once("\"=") {
                if let Some(k) = values.iter().position(|(name, _)| n.eq_ignore_ascii_case(name)) {
                    *line = reg_line(values[k].0, &values[k].1);
                    done[k] = true;
                }
            }
        }
    }
    let missing: Vec<String> = values
        .iter()
        .zip(&done)
        .filter(|(_, d)| !**d)
        .map(|((n, v), _)| reg_line(n, v))
        .collect();
    if !missing.is_empty() {
        match header {
            Some(h) => {
                for (k, l) in missing.into_iter().enumerate() {
                    lines.insert(h + 1 + k, l);
                }
            }
            None => {
                while lines.last().is_some_and(|l| l.is_empty()) {
                    lines.pop();
                }
                lines.push(String::new());
                lines.push("[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]".into());
                lines.extend(missing);
                lines.push(String::new());
            }
        }
    }
    lines.join(newline)
}

// ---- where the game keeps the values --------------------------------------------

pub(crate) struct Loaded {
    pub config: PadConfig,
    /// Where the values were read from, for the UI.
    pub source: String,
    pub found: bool,
}

/// The account overlays the native client loads after `ffxi.reg`.
#[cfg(test)]
pub(crate) fn native_overlays(config: &crate::backend::Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(config.experiment.join("test-launcher/data")) {
        for d in dirs.flatten() {
            let runtime = d.path().join("runtime");
            if runtime.is_dir() {
                out.push(runtime.join("saved.reg"));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
fn read_reg(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| decode_reg_bytes(&b).0)
}

#[cfg(test)]
pub(crate) fn load_native(config: &crate::backend::Config) -> Loaded {
    let base_path = config.repository.join("ffxi.reg");
    let base = read_reg(&base_path).unwrap_or_default();
    // The most recently saved overlay is the one the player last played with.
    let overlay = native_overlays(config)
        .into_iter()
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p);
    let over = overlay.as_deref().and_then(read_reg).unwrap_or_default();
    let final_path = config.experiment.join("iced-launcher/game-settings.reg");
    let final_values = read_reg(&final_path).unwrap_or_default();
    let get = |name: &str| reg_value(&final_values, name).or_else(|| reg_value(&over, name)).or_else(|| reg_value(&base, name));
    let (mode, sin, guid) = (get("padmode000"), get("padsin000"), get("padguid000"));
    let found = mode.is_some() || sin.is_some();
    Loaded {
        config: PadConfig::from_values(mode.as_deref(), sin.as_deref(), guid.as_deref()),
        source: match &overlay {
            _ if reg_value(&final_values, "padsin000").is_some() => final_path.display().to_string(),
            Some(p) if reg_value(&over, "padsin000").is_some() => p.display().to_string(),
            _ => base_path.display().to_string(),
        },
        found,
    }
}

fn backup_dir(config: &crate::backend::Config) -> PathBuf {
    config.experiment.join("iced-launcher/gamepad-backup")
}

/// Writes `bytes` to `path` only if it does not exist yet: the first backup is
/// the original and is never replaced.
fn backup_once(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| tf("Could not create the backup folder: {error}", &[("error", &e)]))?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => f.write_all(bytes).map_err(|e| tf("Could not write the backup: {error}", &[("error", &e)])),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(tf("Could not write the backup: {error}", &[("error", &e)])),
    }
}

/// Use the existing final override, including before an account's first launch.
#[cfg(test)]
pub(crate) fn save_native(config: &crate::backend::Config, pad: &PadConfig) -> Result<String, String> {
    let path = config.experiment.join("iced-launcher/game-settings.reg");
    let original = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(tf("Could not read {path}: {error}. Nothing was changed.", &[("path", &path.display()), ("error", &error)])),
    };
    let values = pad.values();
    if path.is_file() { backup_once(&backup_dir(config).join("game-settings.reg"), &original)?; }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| tf("Could not create the controller settings folder: {error}", &[("error", &e)]))?;
    let (text, utf16) = decode_reg_bytes(&original);
    let patched = patch_reg_text(&text, &values);
    let tmp = path.with_extension("reg.tmp");
    std::fs::write(&tmp, encode_reg_text(&patched, utf16))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| tf("Could not save {path}: {error}", &[("path", &path.display()), ("error", &e)]))?;
    Ok(tf("Saved native controller settings. They apply to every native account on its next launch. Backups are in {path}.", &[("path", &backup_dir(config).display())]))
}

#[cfg(windows)]
mod registry {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_32KEY, REG_EXPAND_SZ, REG_SZ,
    };

    const REGIONS: [&str; 3] = ["PlayOnlineUS", "PlayOnline", "PlayOnlineEU"];

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub(super) fn key_path(region: &str) -> String {
        format!("SOFTWARE\\{region}\\SquareEnix\\FinalFantasyXI")
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(region: &str, access: u32) -> Option<Key> {
        let path = wide(&key_path(region));
        let mut key: HKEY = std::ptr::null_mut();
        // KEY_WOW64_32KEY: the 32-bit game's view (WOW6432Node on 64-bit Windows).
        let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, access | KEY_WOW64_32KEY, &mut key) };
        (r == ERROR_SUCCESS).then_some(Key(key))
    }

    /// The first install region whose key exists.
    pub(super) fn region() -> Option<&'static str> {
        REGIONS.into_iter().find(|r| open(r, KEY_QUERY_VALUE).is_some())
    }

    pub(super) fn read(region: &str, name: &str) -> Option<String> {
        let key = open(region, KEY_QUERY_VALUE)?;
        let name = wide(name);
        let (mut kind, mut size) = (0u32, 0u32);
        let r = unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut size) };
        if r != ERROR_SUCCESS || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut bytes = (buf.len() * 2) as u32;
        let r = unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut bytes) };
        if r != ERROR_SUCCESS {
            return None;
        }
        buf.truncate(bytes as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf16_lossy(&buf))
    }

    pub(super) fn write(region: &str, name: &str, value: &str) -> Result<(), String> {
        let key = open(region, KEY_SET_VALUE).ok_or_else(|| {
            crate::i18n::t("Windows refused write access to FFXI's registry key. Run the launcher as administrator.").to_string()
        })?;
        let name_w = wide(name);
        let data = wide(value);
        let r = unsafe {
            RegSetValueExW(key.0, name_w.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
        };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(crate::i18n::tf("Could not write {name} to the registry (error {code}).", &[("name", &name), ("code", &r)]))
        }
    }
}

pub(crate) fn load_retail() -> Loaded {
    #[cfg(windows)]
    if let Some(region) = registry::region() {
        let get = |n: &str| registry::read(region, n);
        let (mode, sin, guid) = (get("padmode000"), get("padsin000"), get("padguid000"));
        return Loaded {
            found: mode.is_some() || sin.is_some(),
            config: PadConfig::from_values(mode.as_deref(), sin.as_deref(), guid.as_deref()),
            source: format!("HKLM\\SOFTWARE\\WOW6432Node\\{region}\\SquareEnix\\FinalFantasyXI"),
        };
    }
    Loaded {
        config: PadConfig::default(),
        source: t("FINAL FANTASY XI is not installed (no registry key)").into(),
        found: false,
    }
}

/// Writes the values to the registry, backing up the originals first.
pub(crate) fn save_retail(config: &crate::backend::Config, pad: &PadConfig) -> Result<String, String> {
    #[cfg(windows)]
    {
        let region = registry::region().ok_or(t("FINAL FANTASY XI's registry key was not found."))?;
        let mut backup = format!(
            "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\{region}\\SquareEnix\\FinalFantasyXI]\r\n"
        );
        for name in ["padmode000", "padsin000", "padguid000", "padexsin000"] {
            if let Some(v) = registry::read(region, name) {
                backup.push_str(&reg_line(name, &v));
                backup.push_str("\r\n");
            }
        }
        backup_once(
            &backup_dir(config).join(format!("registry-{region}.reg")),
            &encode_reg_text(&backup, true),
        )?;
        for (name, value) in pad.values() {
            registry::write(region, name, &value)?;
        }
        Ok(tf(
            "Saved to the registry. The original values are backed up in {path}.",
            &[("path", &backup_dir(config).display())],
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = (config, pad);
        Err(t("The registry exists on Windows only.").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_controller_settings_work_before_first_launch_and_leave_account_files_intact() {
        let root = std::env::temp_dir().join(format!("crystal-pad-save-{}", crate::backend::unique_id().unwrap()));
        let config = crate::backend::Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let mut pad = PadConfig::default();
        pad.mode[1] = 0;
        save_native(&config, &pad).unwrap();
        assert_eq!(load_native(&config).config, pad);
        let final_path = root.join("iced-launcher/game-settings.reg");
        let original = std::fs::read_to_string(&final_path).unwrap();
        std::fs::write(&final_path, format!("{original}\n\"0001\"=dword:00000780\n\"padexsin000\"=\"preserved\"\n")).unwrap();
        let account_file = root.join("test-launcher/data/account/runtime/saved.reg");
        std::fs::create_dir_all(account_file.parent().unwrap()).unwrap();
        std::fs::write(&account_file, b"account-owned configuration").unwrap();
        pad.mode[1] = 1;
        save_native(&config, &pad).unwrap();
        assert_eq!(std::fs::read(&account_file).unwrap(), b"account-owned configuration");
        let final_text = std::fs::read_to_string(&final_path).unwrap();
        assert!(final_text.contains("00000780"));
        assert_eq!(reg_value(&final_text, "padexsin000").as_deref(), Some("preserved"));
        assert_eq!(load_native(&config).config.mode, pad.mode);
        assert!(backup_dir(&config).join("game-settings.reg").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn an_unreadable_native_controller_settings_file_is_kept_unchanged() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("crystal-pad-locked-{}", crate::backend::unique_id().unwrap()));
        let config = crate::backend::Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let file = root.join("iced-launcher/game-settings.reg");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"preserve these settings").unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        assert!(save_native(&config, &PadConfig::default()).is_err());
        drop(held);
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve these settings");
    }

    #[test]
    fn gamepad_unknown_axis_ids_remain_unbound_to_a_direction_group() {
        for layout in [Layout::XInput, Layout::DInputXbox, Layout::DInputSony] {
            assert_eq!(layout.group_stick(&[i32::MIN; 4]), None);
            assert_eq!(layout.group_stick(&[-1; 4]), None);
            let reversed: Vec<_> = layout.group(Stick::Left).into_iter().map(|axis| -axis).collect();
            assert_eq!(layout.group_stick(&reversed), Some(Stick::Left));
        }
    }

    #[test]
    fn gamepad_defaults_decode_to_the_xbox_layout() {
        let l = Layout::XInput;
        let d = DEFAULT_XINPUT_SIN;
        assert_eq!(l.decode(d[7]), Control::Button(Btn::South)); // confirm: A
        assert_eq!(l.decode(d[5]), Control::Button(Btn::East)); // cancel: B
        assert_eq!(l.decode(d[6]), Control::Button(Btn::West)); // main menu: X
        assert_eq!(l.decode(d[8]), Control::Button(Btn::North)); // active window: Y
        assert_eq!(l.group_stick(&d[13..17]), Some(Stick::Left));
        assert_eq!(l.group_stick(&d[17..21]), Some(Stick::Right));
        assert_eq!(l.group_stick(&d[21..25]), Some(Stick::Dpad));
        assert_eq!(l.decode(-33), Control::Axis(Stick::Left, AxisDir::Y, true));
        assert_eq!(l.decode(16), Control::None);
    }

    #[test]
    fn gamepad_ps5_extra_bindings_round_trip_in_saved_directinput_values() {
        let mut config = PadConfig::default();
        config.set_flag(Flag::XInput, false, Layout::DInputSony);
        config.bind_button(6, Layout::DInputSony.encode_button(Btn::Touchpad).unwrap());
        config.bind_button(8, Layout::DInputSony.encode_button(Btn::Mute).unwrap());
        let values = config.values();
        let loaded = PadConfig::from_values(Some(&values[0].1), Some(&values[1].1), None);
        assert!(!loaded.flag(Flag::XInput));
        assert_eq!(loaded.sin[6], 13);
        assert_eq!(loaded.sin[8], 14);
        assert_eq!(Layout::DInputSony.decode(loaded.sin[6]), Control::Button(Btn::Touchpad));
        assert_eq!(Layout::DInputSony.decode(loaded.sin[8]), Control::Button(Btn::Mute));
        assert_eq!(Layout::XInput.encode_button(Btn::Touchpad), None);
        assert_eq!(Layout::XInput.encode_button(Btn::Mute), None);
    }

    #[test]
    fn gamepad_lists_round_trip_and_keep_their_length() {
        let real = "-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1";
        let c = PadConfig::from_values(Some("0,0,0,0,0,0"), Some(real), Some("{00000000-0000-0000-0000-000000000000}"));
        assert_eq!(c.sin.len(), 32);
        assert_eq!(format_list(&c.sin), real);
        assert_eq!(c.values()[0].1, "0,0,0,0,0,0");
        let short = PadConfig::from_values(Some("1,1"), Some("8,9"), None);
        assert_eq!(short.mode.len(), MODE_COUNT);
        assert_eq!(short.sin.len(), SLOT_COUNT);
        assert_eq!(PadConfig::from_values(Some("x"), None, None).mode, DEFAULT_MODE.to_vec());
    }

    #[test]
    fn gamepad_xinput_toggle_keeps_each_function_on_its_button() {
        let mut c = PadConfig::default();
        c.set_flag(Flag::XInput, false, Layout::DInputSony);
        let l = Layout::DInputSony;
        assert_eq!(l.decode(c.sin[7]), Control::Button(Btn::South));
        assert_eq!(c.sin[7], 1); // DirectInput cross
        assert_eq!(l.group_stick(&c.sin[17..21]), Some(Stick::Right));
        assert_eq!(&c.sin[17..21], &[-37, -37, 34, 34]);
        assert_eq!(&c.sin[21..25], &[-41, -41, 40, 40]);
        c.set_flag(Flag::XInput, true, Layout::DInputSony);
        // Triggers have no DirectInput button on Xbox pads, but Sony has L2/R2.
        assert_eq!(c.sin[..SLOT_COUNT], DEFAULT_XINPUT_SIN);
    }

    #[test]
    fn gamepad_binding_a_used_button_swaps() {
        let mut c = PadConfig::default();
        c.bind_button(7, 0); // confirm := B (was cancel's)
        assert_eq!(c.sin[7], 0);
        assert_eq!(c.sin[5], 3); // cancel took A
        c.bind_group(13, Layout::XInput.group(Stick::Right));
        assert_eq!(Layout::XInput.group_stick(&c.sin[17..21]), Some(Stick::Left));
        let d = PadConfig::default();
        assert_eq!(c.changes(&d), 4);
        c.reset(Layout::DInputXbox);
        assert_eq!(c.changes(&d), 0);
    }

    #[test]
    fn gamepad_reg_text_patch_replaces_and_inserts() {
        let text = "REGEDIT4\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\n\"0042\"=\"\"\n\"padmode000\"=\"1,1,0,0,0,1\"\n\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\Interface]\n\"x\"=\"y\"\n";
        let out = patch_reg_text(text, &[("padmode000", "0,1,0,0,0,1".into()), ("padsin000", "1,2".into())]);
        assert_eq!(reg_value(&out, "padmode000").as_deref(), Some("0,1,0,0,0,1"));
        assert_eq!(reg_value(&out, "padsin000").as_deref(), Some("1,2"));
        assert!(out.contains("\"0042\"=\"\"\n"));
        assert!(out.ends_with("\"x\"=\"y\"\n"));
        assert_eq!(out.matches("FinalFantasyXI]").count(), 1);
        // A file without the key gets one; an empty file gets a header too.
        let fresh = patch_reg_text("", &[("padsin000", "3".into())]);
        assert!(fresh.starts_with("REGEDIT4"));
        assert_eq!(reg_value(&fresh, "padsin000").as_deref(), Some("3"));
    }

    #[test]
    fn gamepad_reg_reader_folds_wow6432node_and_utf16() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\"padsin000\"=\"8,9\"\r\n";
        let bytes = encode_reg_text(text, true);
        let (decoded, utf16) = decode_reg_bytes(&bytes);
        assert!(utf16);
        assert_eq!(reg_value(&decoded, "padsin000").as_deref(), Some("8,9"));
        let patched = patch_reg_text(&decoded, &[("padsin000", "1".into())]);
        assert!(patched.contains("\"padsin000\"=\"1\"\r\n"));
    }

    #[test]
    fn gamepad_reads_the_shipped_native_defaults() {
        // Read-only: the experimental client's base registry, if present here.
        let config = crate::backend::Config::default();
        if let Ok(bytes) = std::fs::read(config.repository.join("ffxi.reg")) {
            let (text, _) = decode_reg_bytes(&bytes);
            assert_eq!(
                reg_value(&text, "padsin000").and_then(|v| parse_list(&v)),
                Some(DEFAULT_XINPUT_SIN.to_vec())
            );
        }
    }
}
