//! Where the game's settings live, and reading, writing and backing them up.
//!
//! - Retail installs (and Windower, Ashita, the official client) read the
//!   registry: `HKLM\SOFTWARE\WOW6432Node\<vendor>\SquareEnix\FinalFantasyXI`,
//!   `<vendor>` being PlayOnlineUS, PlayOnlineEU or PlayOnline (Japan).
//! - The native client (host64) has no registry of its own: it builds one in
//!   memory from `.reg` files, later ones winning - the repository's
//!   `ffxi.reg`, then each account's `runtime/saved.reg` (where the game saves
//!   what it changes), then `--reg-final` files (`runtime/display.reg`, then the
//!   launcher's `game-settings.reg`). The page writes that last file.
use super::model::{self, Data, Values};
use crate::i18n::{t, tf};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    Us,
    Eu,
    Jp,
}

impl Region {
    pub(crate) const ALL: [Region; 3] = [Region::Us, Region::Eu, Region::Jp];
    pub(crate) fn vendor(self) -> &'static str {
        match self {
            Region::Us => "PlayOnlineUS",
            Region::Eu => "PlayOnlineEU",
            Region::Jp => "PlayOnline",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Region::Us => t("US"),
            Region::Eu => t("Europe"),
            Region::Jp => t("Japan"),
        }
    }
}

/// What the page edits: the native client's settings file or one region's
/// registry key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Native,
    Retail(Region),
}

impl Target {
    pub(crate) fn slug(self) -> String {
        match self {
            Target::Native => "native".into(),
            Target::Retail(r) => format!("retail-{}", r.vendor().to_ascii_lowercase()),
        }
    }
    pub(crate) fn label(self) -> String {
        match self {
            Target::Native => t("Native client").into(),
            Target::Retail(r) => tf("Windows registry · {region}", &[("region", &r.label())]),
        }
    }
}

/// The files the native client reads, from the launcher's configuration.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub base_reg: PathBuf,
    pub accounts: PathBuf,
    pub launcher: PathBuf,
}

impl Paths {
    pub(crate) fn from_config(config: &crate::backend::Config) -> Self {
        Self {
            base_reg: config.repository.join("ffxi.reg"),
            accounts: config.experiment.join("test-launcher/data"),
            launcher: config.experiment.join("iced-launcher"),
        }
    }
    /// The launcher's own `--reg-final` file.
    pub(crate) fn native_file(&self) -> PathBuf {
        self.launcher.join("game-settings.reg")
    }
    pub(crate) fn backups(&self) -> PathBuf {
        self.launcher.join("game-config-backups")
    }
    fn menu_file(&self, region: Region) -> PathBuf {
        self.launcher.join(format!("menu-{}.json", region.vendor().to_ascii_lowercase()))
    }
    pub(crate) fn location(&self, target: Target) -> String {
        match target {
            Target::Native => self.native_file().display().to_string(),
            Target::Retail(r) => format!(
                "HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\{}\\SquareEnix\\FinalFantasyXI",
                r.vendor()
            ),
        }
    }
}

pub(crate) const NATIVE_VENDOR: &str = "PlayOnlineUS";

/// A menu size explicitly applied in Crystal survives an injector rewriting the
/// shared registry. An absent preference leaves each launch profile in control.
pub(crate) fn menu_override(paths: &Paths, region: Region) -> Result<Option<(u32, u32)>, String> {
    use std::io::Read;
    let path = paths.menu_file(region);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(tf("Could not read {path}.", &[("path", &path.display())])),
    };
    let mut bytes = Vec::new();
    file.take(1025).read_to_end(&mut bytes)
        .map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?;
    let size: (u32, u32) = serde_json::from_slice(&bytes)
        .map_err(|_| tf("The saved menu resolution in {path} is invalid.", &[("path", &path.display())]))?;
    if bytes.len() > 1024 || size.0 < model::MIN_MENU.0 || size.1 < model::MIN_MENU.1 {
        return Err(tf("The saved menu resolution in {path} is invalid.", &[("path", &path.display())]));
    }
    Ok(Some(size))
}

fn save_menu(paths: &Paths, region: Region, size: (u32, u32)) -> Result<(), String> {
    let path = paths.menu_file(region);
    if size == (0, 0) {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(tf("Could not save {path}.", &[("path", &path.display())])),
        };
    }
    std::fs::create_dir_all(&paths.launcher)
        .map_err(|_| tf("Could not create {path}.", &[("path", &paths.launcher.display())]))?;
    let temporary = path.with_extension(format!("{}.tmp", crate::backend::unique_id()?));
    let result = std::fs::write(&temporary, serde_json::to_vec(&size).expect("integer pair"))
        .and_then(|_| std::fs::rename(&temporary, &path));
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result.map_err(|_| tf("Could not save {path}.", &[("path", &path.display())]))
}

fn retain_menu(values: &mut Values, size: Option<(u32, u32)>) {
    if let Some((w, h)) = size {
        values.insert(model::MENU_W.into(), Data::Dword(w));
        values.insert(model::MENU_H.into(), Data::Dword(h));
    }
}

fn layer(values: &mut Values, path: &Path, vendor: &str) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            values.extend(model::parse_reg(&model::decode_text(&bytes), vendor));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(tf("Could not read {path}.", &[("path", &path.display())])),
    }
}

/// The account runtime folder the native client used most recently.
fn newest_runtime(accounts: &Path) -> Option<PathBuf> {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    std::fs::read_dir(accounts)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("runtime"))
        .filter_map(|rt| {
            let newest = [rt.join("saved.reg"), rt.join("display.reg")]
                .iter()
                .filter_map(|p| modified(p))
                .max()?;
            Some((newest, rt))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, rt)| rt)
}

/// The values the native client starts with, layered as host64 loads them.
pub(crate) fn read_native(paths: &Paths) -> Result<Values, String> {
    let mut values = Values::new();
    layer(&mut values, &paths.base_reg, NATIVE_VENDOR)?;
    if let Some(runtime) = newest_runtime(&paths.accounts) {
        layer(&mut values, &runtime.join("saved.reg"), NATIVE_VENDOR)?;
        layer(&mut values, &runtime.join("display.reg"), NATIVE_VENDOR)?;
    }
    layer(&mut values, &paths.native_file(), NATIVE_VENDOR)?;
    Ok(values)
}

/// The native settings file's next contents: what it holds now, with every
/// managed value from `edited`, so all accounts start from the same settings.
/// Values the native client cannot start with are replaced, and the font type
/// is always written, so this last file also overrides an unsupported one in
/// `saved.reg` or `ffxi.reg`.
pub(crate) fn native_contents(existing: &Values, edited: &Values) -> Values {
    let mut out = existing.clone();
    for key in model::MANAGED {
        if let Some(data) = edited.get(key) {
            out.insert(key.into(), data.clone());
        }
    }
    model::guard_native(&mut out);
    out.entry(model::FONT_TYPE.into())
        .or_insert(Data::Dword(model::NATIVE_FONT_TYPE));
    out
}

pub(crate) fn write_native(paths: &Paths, edited: &Values) -> Result<(), String> {
    let file = paths.native_file();
    let mut existing = Values::new();
    layer(&mut existing, &file, NATIVE_VENDOR)?;
    let text = model::encode_reg(&native_contents(&existing, edited), NATIVE_VENDOR);
    std::fs::create_dir_all(&paths.launcher)
        .map_err(|_| tf("Could not create {path}.", &[("path", &paths.launcher.display())]))?;
    let tmp = file.with_extension("reg.tmp");
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| tf("Could not save {path}: {error}", &[("path", &file.display()), ("error", &e)]))?;
    std::fs::rename(&tmp, &file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        tf("Could not save {path}: {error}", &[("path", &file.display()), ("error", &e)])
    })
}

pub(crate) fn read(paths: &Paths, target: Target) -> Result<Values, String> {
    match target {
        Target::Native => read_native(paths),
        Target::Retail(r) => {
            let mut values = registry::read(r.vendor())?
                .ok_or_else(|| tf("FINAL FANTASY XI ({region}) has no settings in the registry.", &[("region", &r.label())]))?;
            retain_menu(&mut values, menu_override(paths, r)?);
            Ok(values)
        }
    }
}

/// Writes the changed managed values (registry) or the whole managed set
/// (native file).
pub(crate) fn write(paths: &Paths, target: Target, original: &Values, edited: &Values, menu_selected: bool) -> Result<(), String> {
    match target {
        Target::Native => write_native(paths, edited),
        Target::Retail(r) => {
            let changes: Vec<(&str, &Data)> = model::changed_keys(original, edited)
                .into_iter()
                .filter_map(|k| edited.get(k).map(|d| (k, d)))
                .collect();
            registry::write(r.vendor(), &changes)?;
            if menu_selected || changes.iter().any(|(key, _)| matches!(*key, model::MENU_W | model::MENU_H)) {
                let size = (model::dword(edited, model::MENU_W).unwrap_or(0), model::dword(edited, model::MENU_H).unwrap_or(0));
                save_menu(paths, r, size)?;
            }
            Ok(())
        }
    }
}

/// Regions whose FinalFantasyXI key holds game settings (not just an
/// install path).
pub(crate) fn retail_regions() -> Vec<Region> {
    Region::ALL
        .into_iter()
        .filter(|r| {
            registry::read(r.vendor())
                .ok()
                .flatten()
                .is_some_and(|v| model::dword(&v, model::WINDOW_W).is_some())
        })
        .collect()
}

pub(crate) fn writable(target: Target) -> bool {
    match target {
        Target::Native => true,
        Target::Retail(r) => registry::writable(r.vendor()),
    }
}

// ---- backups ----

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub(crate) struct Backup {
    pub target: String,
    pub location: String,
    pub taken: String,
    pub values: Values,
}

#[derive(Clone, Debug)]
pub(crate) struct BackupEntry {
    pub path: PathBuf,
    pub label: String,
}

impl PartialEq for BackupEntry {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl std::fmt::Display for BackupEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Saves `values` as `<dir>/<slug>-<YYYYMMDD-HHMMSS>.json`, never replacing a
/// file.
pub(crate) fn save_backup(dir: &Path, target: Target, location: &str, values: &Values, now: chrono::NaiveDateTime) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|_| tf("Could not create the backup folder {path}.", &[("path", &dir.display())]))?;
    let backup = Backup {
        target: target.slug(),
        location: location.into(),
        taken: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        values: values.clone(),
    };
    let json = serde_json::to_vec_pretty(&backup).map_err(|_| t("Could not encode the backup.").to_string())?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    for n in 0..100 {
        let name = if n == 0 {
            format!("{}-{stamp}.json", target.slug())
        } else {
            format!("{}-{stamp}-{n}.json", target.slug())
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                return file
                    .write_all(&json)
                    .and_then(|_| file.sync_all())
                    .map(|_| path.clone())
                    .map_err(|_| tf("Could not write the backup {path}.", &[("path", &path.display())]));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(tf("Could not write a backup in {path}.", &[("path", &dir.display())])),
        }
    }
    Err(t("Too many backups were made this second.").into())
}

/// This target's backups, newest first.
pub(crate) fn list_backups(dir: &Path, target: Target) -> Vec<BackupEntry> {
    let prefix = format!("{}-", target.slug());
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".json")?.to_owned();
            // "retail-playonline-" must not claim "retail-playonlineus-..." files.
            stamp.as_bytes().first()?.is_ascii_digit().then_some((stamp, e.path()))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .map(|(stamp, path)| BackupEntry { label: backup_label(&stamp), path })
        .collect()
}

fn backup_label(stamp: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(stamp.get(..15).unwrap_or(stamp), "%Y%m%d-%H%M%S")
        .map(|t| t.format("%b %-d, %Y  %H:%M").to_string())
        .unwrap_or_else(|_| stamp.to_owned())
}

pub(crate) fn load_backup(path: &Path) -> Result<Backup, String> {
    let bytes = std::fs::read(path).map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?;
    serde_json::from_slice(&bytes).map_err(|_| tf("{path} is not a readable backup.", &[("path", &path.display())]))
}

// ---- the registry ----

#[cfg(windows)]
mod registry {
    use super::super::model::{Data, Values};
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
        KEY_READ, KEY_SET_VALUE, KEY_WOW64_32KEY, REG_DWORD, REG_EXPAND_SZ, REG_SZ,
    };

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: an open key handle from RegOpenKeyExW.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The 32-bit view, where the 32-bit game and its tools keep the key
    /// (WOW6432Node to a 64-bit process).
    fn open(vendor: &str, access: u32) -> Result<Key, u32> {
        let path = wide(&format!("SOFTWARE\\{vendor}\\SquareEnix\\FinalFantasyXI"));
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated path and out pointer.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, access | KEY_WOW64_32KEY, &mut key) };
        if status == ERROR_SUCCESS {
            Ok(Key(key))
        } else {
            Err(status)
        }
    }

    pub(super) fn read(vendor: &str) -> Result<Option<Values>, String> {
        let key = match open(vendor, KEY_READ) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND) => return Ok(None),
            Err(code) => return Err(crate::i18n::tf("Windows could not read the FINAL FANTASY XI settings (error {code}).", &[("code", &code)])),
        };
        let mut values = Values::new();
        let mut name = vec![0u16; 512];
        let mut data = vec![0u8; 8192];
        for index in 0..4096u32 {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            // SAFETY: buffers and their lengths are valid for the call.
            let status = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                continue; // an oversized value: not one of the game's
            }
            let value_name = String::from_utf16_lossy(&name[..name_len as usize]);
            let bytes = &data[..data_len as usize];
            let value = match kind {
                REG_DWORD if bytes.len() == 4 => Data::Dword(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                REG_SZ | REG_EXPAND_SZ => {
                    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
                    Data::Text(String::from_utf16_lossy(&units[..end]))
                }
                _ => Data::Binary(bytes.to_vec()),
            };
            values.insert(value_name, value);
        }
        Ok(Some(values))
    }

    /// Whether this process may set values (opening for write changes nothing).
    pub(super) fn writable(vendor: &str) -> bool {
        open(vendor, KEY_SET_VALUE | KEY_QUERY_VALUE).is_ok()
    }

    pub(super) fn write(vendor: &str, changes: &[(&str, &Data)]) -> Result<(), String> {
        let key = open(vendor, KEY_SET_VALUE | KEY_QUERY_VALUE).map_err(|code| match code {
            ERROR_ACCESS_DENIED => crate::i18n::t("Windows refused the change: saving FINAL FANTASY XI's settings needs Crystal Launcher to run as administrator. Nothing was changed.").to_string(),
            ERROR_FILE_NOT_FOUND => crate::i18n::t("FINAL FANTASY XI's settings key is missing; run the game or its Config tool once first. Nothing was changed.").to_string(),
            code => crate::i18n::tf("Windows could not open FINAL FANTASY XI's settings for writing (error {code}). Nothing was changed.", &[("code", &code)]),
        })?;
        for (done, (name, data)) in changes.iter().enumerate() {
            let wname = wide(name);
            let (kind, bytes): (u32, Vec<u8>) = match data {
                Data::Dword(v) => (REG_DWORD, v.to_le_bytes().to_vec()),
                Data::Text(s) => (REG_SZ, wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()),
                Data::Binary(b) => (windows_sys::Win32::System::Registry::REG_BINARY, b.clone()),
            };
            // SAFETY: valid name, data pointer and length.
            let status = unsafe { RegSetValueExW(key.0, wname.as_ptr(), 0, kind, bytes.as_ptr(), bytes.len() as u32) };
            if status != ERROR_SUCCESS {
                return Err(crate::i18n::tf(
                    "Windows refused to save setting {name} (error {status}); {done} of {total} changes were saved. Restore the backup if the game misbehaves.",
                    &[("name", name), ("status", &status), ("done", &done), ("total", &changes.len())],
                ));
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod registry {
    use super::super::model::{Data, Values};
    pub(super) fn read(_: &str) -> Result<Option<Values>, String> {
        Ok(None)
    }
    pub(super) fn writable(_: &str) -> bool {
        false
    }
    pub(super) fn write(_: &str, _: &[(&str, &Data)]) -> Result<(), String> {
        Err(crate::i18n::t("The registry exists only on Windows.").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_config::model::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("game-config-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_paths(root: &Path) -> Paths {
        Paths {
            base_reg: root.join("repo/ffxi.reg"),
            accounts: root.join("test-launcher/data"),
            launcher: root.join("iced-launcher"),
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const KEY: &str = "[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]";

    #[test]
    fn game_config_native_layers_match_host64_load_order() {
        let root = scratch("layers");
        let p = fake_paths(&root);
        write(&p.base_reg, &format!("Windows Registry Editor Version 5.00\n{KEY}\n\"0000\"=dword:00000006\n\"0001\"=dword:00000d70\n\"0031\"=dword:3bc49ba6\n"));
        let rt = p.accounts.join("abc/runtime");
        write(&rt.join("saved.reg"), "REGEDIT4\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\n\"0000\"=dword:00000003\n\"0034\"=dword:00000000\n");
        write(&rt.join("display.reg"), &format!("{KEY}\n\"0001\"=dword:00000500\n\"0034\"=dword:00000001\n"));
        let v = read_native(&p).unwrap();
        assert_eq!(dword(&v, "0000"), Some(3)); // saved.reg over ffxi.reg
        assert_eq!(dword(&v, "0001"), Some(1280)); // display.reg over both
        assert_eq!(dword(&v, "0034"), Some(1));
        assert_eq!(dword(&v, "0031"), Some(0x3bc49ba6));
        // The launcher's file is loaded last and wins.
        write(&p.native_file(), &model::encode_reg(&[("0001".to_string(), Data::Dword(2560))].into(), NATIVE_VENDOR));
        assert_eq!(dword(&read_native(&p).unwrap(), "0001"), Some(2560));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_config_native_write_keeps_unmanaged_and_writes_all_managed() {
        let root = scratch("write");
        let p = fake_paths(&root);
        write(&p.native_file(), "REGEDIT4\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\"custom\"=dword:00000007\r\n\"0001\"=dword:00000500\r\n");
        let mut edited = defaults((1920, 1080));
        edited.insert("0031".into(), Data::Dword(99)); // unmanaged: not written
        write_native(&p, &edited).unwrap();
        let saved = model::parse_reg(&std::fs::read_to_string(p.native_file()).unwrap(), NATIVE_VENDOR);
        assert_eq!(dword(&saved, "custom"), Some(7));
        assert_eq!(dword(&saved, WINDOW_W), Some(1920));
        assert_eq!(saved.get("0031"), None);
        for key in MANAGED {
            assert!(saved.contains_key(key), "{key} written");
        }
        assert!(!p.launcher.join("game-settings.reg.tmp").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_config_native_write_never_saves_a_crashing_font_type() {
        let root = scratch("font");
        let p = fake_paths(&root);
        // An earlier file holding High quality fonts is corrected on the next write.
        write(&p.native_file(), "REGEDIT4\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\"0036\"=dword:00000002\r\n");
        let mut edited = defaults((3840, 2160));
        edited.insert(FONT_TYPE.into(), Data::Dword(2));
        edited.insert(WINDOW_MODE.into(), Data::Dword(3));
        write_native(&p, &edited).unwrap();
        let saved = model::parse_reg(&std::fs::read_to_string(p.native_file()).unwrap(), NATIVE_VENDOR);
        assert_eq!(dword(&saved, FONT_TYPE), Some(NATIVE_FONT_TYPE));
        assert_eq!(dword(&saved, WINDOW_MODE), Some(3));
        assert_eq!(dword(&saved, WINDOW_W), Some(3840));
        // Without a font type in the edits, one is still written so this last
        // file overrides an unsupported value from saved.reg.
        let mut edited = defaults((1920, 1080));
        edited.remove(FONT_TYPE);
        let out = native_contents(&Values::new(), &edited);
        assert_eq!(dword(&out, FONT_TYPE), Some(NATIVE_FONT_TYPE));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_config_backups_round_trip_and_never_overwrite() {
        let root = scratch("backup");
        let dir = root.join("backups");
        let values = defaults((2560, 1440));
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 3).unwrap().and_hms_opt(14, 5, 9).unwrap();
        let us = Target::Retail(Region::Us);
        let a = save_backup(&dir, us, "HKLM\\...", &values, now).unwrap();
        let b = save_backup(&dir, us, "HKLM\\...", &values, now).unwrap();
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("retail-playonlineus-20261003-140509"));
        save_backup(&dir, Target::Native, "x", &values, now).unwrap();
        // The Japanese key's slug is a prefix of the US one's; listings stay apart.
        assert!(list_backups(&dir, Target::Retail(Region::Jp)).is_empty());
        let listed = list_backups(&dir, us);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].label, "Oct 3, 2026  14:05");
        let loaded = load_backup(&listed[1].path).unwrap();
        assert_eq!(loaded.values, values);
        assert_eq!(loaded.target, "retail-playonlineus");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Read-only check against this machine: `cargo test game_config_reads_this_machine -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn game_config_reads_this_machine() {
        let paths = Paths::from_config(&crate::backend::Config::default());
        for region in retail_regions() {
            let v = read(&paths, Target::Retail(region)).unwrap();
            println!("{}: {} values, window {:?}x{:?}, writable {}", region.label(), v.len(),
                dword(&v, WINDOW_W), dword(&v, WINDOW_H), writable(Target::Retail(region)));
        }
        let v = read_native(&paths).unwrap();
        println!("native: {} values, window {:?}x{:?}, mode {:?}", v.len(), dword(&v, WINDOW_W), dword(&v, WINDOW_H), dword(&v, WINDOW_MODE));
        println!("modes: {:?}", crate::game_config::display::modes());
    }

    #[test]
    fn game_config_retail_write_sends_only_changed_values() {
        // The change list the registry writer receives, without touching the registry.
        let original = defaults((1920, 1080));
        let mut edited = original.clone();
        edited.insert(MIP_MAPPING.into(), Data::Dword(2));
        edited.insert("0031".into(), Data::Dword(5));
        let changes: Vec<&str> = changed_keys(&original, &edited);
        assert_eq!(changes, vec![MIP_MAPPING]);
    }

    #[test]
    fn saved_menu_survives_injector_registry_resets_and_can_be_released() {
        let root = scratch("retained-menu");
        let paths = fake_paths(&root);
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), None);
        save_menu(&paths, Region::Us, (1600, 900)).unwrap();
        // A fresh reader sees the persisted size after Windower's 1x profile
        // resets the registry, without changing that registry or other values.
        let mut registry = defaults((3840, 2160));
        registry.insert(MENU_W.into(), Data::Dword(0));
        registry.insert(MENU_H.into(), Data::Dword(0));
        retain_menu(&mut registry, menu_override(&fake_paths(&root), Region::Us).unwrap());
        assert_eq!((dword(&registry, MENU_W), dword(&registry, MENU_H)), (Some(1600), Some(900)));
        assert_eq!(dword(&registry, WINDOW_W), Some(3840));
        assert_eq!(menu_override(&paths, Region::Eu).unwrap(), None);
        save_menu(&paths, Region::Us, (1280, 720)).unwrap();
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), Some((1280, 720)));
        save_menu(&paths, Region::Us, (0, 0)).unwrap();
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
