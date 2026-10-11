//! Read-only detection of the Windows components FINAL FANTASY XI needs.
//!
//! Every check works without administrator rights: file presence and version
//! resources in SysWOW64, the Visual C++ runtime's installer record, and the
//! component-based-servicing package state Windows keeps for DirectPlay
//! (readable by every user). The decisions are pure functions over those
//! readings so they can be unit-tested without touching the machine.

use crate::i18n::{t, tf};

/// One component the System tab checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Component {
    DirectPlay,
    DirectX9,
    VcRedist,
}

impl Component {
    pub(crate) const ALL: [Component; 3] = [Component::DirectPlay, Component::DirectX9, Component::VcRedist];

    pub(crate) fn index(self) -> usize {
        match self {
            Component::DirectPlay => 0,
            Component::DirectX9 => 1,
            Component::VcRedist => 2,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Component::DirectPlay => "DirectPlay",
            Component::DirectX9 => t("DirectX 9.0c runtime"),
            Component::VcRedist => t("Visual C++ runtime (x86)"),
        }
    }
    /// Microsoft's own product names (it localizes them too).
    pub(crate) fn full_name(self) -> &'static str {
        match self {
            Component::DirectPlay => t("DirectPlay (Windows optional feature)"),
            Component::DirectX9 => t("DirectX End-User Runtime, June 2010"),
            Component::VcRedist => t("Microsoft Visual C++ 2015–2022 Redistributable (x86)"),
        }
    }
    /// Why FFXI needs it, in one line.
    pub(crate) fn why(self) -> &'static str {
        match self {
            Component::DirectPlay => t("FINAL FANTASY XI checks for DirectPlay's network helper at startup and quits without it."),
            Component::DirectX9 => t("Supplies XInput 1.3 for FFXI's controller support and the D3DX 9 libraries older tools load."),
            Component::VcRedist => t("Needed by the FFXI Config tool, Windower and Ashita (32-bit Visual C++ programs)."),
        }
    }
    /// What breaks without it, for the Home/Downloads banner.
    pub(crate) fn consequence(self) -> &'static str {
        match self {
            Component::DirectPlay => t("FINAL FANTASY XI can't start without it."),
            Component::DirectX9 => t("controllers won't work in FINAL FANTASY XI."),
            Component::VcRedist => t("the FFXI Config tool and add-on loaders won't run."),
        }
    }
    /// The fix button's verb.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Component::DirectPlay => t("Enable"),
            _ => t("Install"),
        }
    }
    /// Blocks the game outright (vs. degrading parts of it).
    pub(crate) fn critical(self) -> bool {
        self == Component::DirectPlay
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Present. `version` is a short display string when one is known.
    Installed { version: Option<String> },
    /// Not present (or incomplete); `detail` says what is missing.
    Missing { detail: String },
    /// Windows reports the change but needs a restart to finish it.
    RestartPending,
}

impl Status {
    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Status::Missing { .. })
    }
}

/// The result for one component plus how it was checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) status: Status,
    pub(crate) evidence: Evidence,
}

/// What a check read, kept as data so its line is worded when shown (and so
/// follows a language change without checking again).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// DirectPlay: the Windows feature state and which SysWOW64 files are missing.
    Feature { state: FeatureState, missing: Vec<String> },
    /// These files are present in SysWOW64.
    FilesPresent(Vec<String>),
    /// These files are missing from SysWOW64.
    FilesMissing(Vec<String>),
    /// The VC++ installer record and the runtime files.
    VcRecordAndFiles,
    /// The VC++ runtime files without an installer record.
    VcFilesOnly,
    /// Neither the VC++ installer record nor its files.
    VcNothing,
    #[cfg_attr(windows, allow(dead_code))] // Built only off Windows.
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeatureState {
    /// Windows' servicing state could not be read.
    Unavailable,
    Pending,
    On,
    Off,
}

impl Evidence {
    /// The line in the current language.
    pub(crate) fn text(&self) -> String {
        match self {
            Evidence::Feature { state, missing } => {
                let files = if missing.is_empty() {
                    t("dpnhpast, dplayx and dpnet present in SysWOW64").to_owned()
                } else {
                    tf("missing from SysWOW64: {files}", &[("files", &missing.join(", "))])
                };
                let args: [(&str, &dyn std::fmt::Display); 1] = [("files", &files)];
                match state {
                    FeatureState::Unavailable => tf("Windows feature state unavailable · {files}", &args),
                    FeatureState::Pending => tf("Windows feature change pending a restart · {files}", &args),
                    FeatureState::On => tf("Windows feature: on · {files}", &args),
                    FeatureState::Off => tf("Windows feature: off · {files}", &args),
                }
            }
            Evidence::FilesPresent(files) => tf("{files} present in SysWOW64", &[("files", &files.join(", "))]),
            Evidence::FilesMissing(files) => tf("missing from SysWOW64: {files}", &[("files", &files.join(", "))]),
            Evidence::VcRecordAndFiles => {
                t("Installer record (Runtimes\\x86 Installed=1) · runtime files present").into()
            }
            Evidence::VcFilesOnly => t("Runtime files present in SysWOW64 (no installer record)").into(),
            Evidence::VcNothing => t("No x86 runtime installer record; runtime files not in SysWOW64").into(),
            Evidence::NotApplicable => t("Not applicable on this OS").into(),
        }
    }
}

/// An installed version as shown: the DirectX runtime's d3dx9_43 build
/// 9.29.952.x is the June 2010 release.
pub(crate) fn shown_version(component: Component, version: &str) -> String {
    if component == Component::DirectX9 && version.starts_with("9.29.952") {
        tf("June 2010 · {version}", &[("version", &version)])
    } else {
        version.to_owned()
    }
}

/// One detection pass over all components, indexed by `Component::index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) findings: [Finding; 3],
}

impl Report {
    pub(crate) fn get(&self, component: Component) -> &Finding {
        &self.findings[component.index()]
    }
}

// ---- pure decisions ---------------------------------------------------------

/// CBS package `CurrentState` values (see DISM / CBS documentation).
const CBS_INSTALL_PENDING: u32 = 0x60;
const CBS_PARTIALLY_INSTALLED: u32 = 0x65;
const CBS_INSTALLED: u32 = 0x70;
const CBS_PERMANENT: u32 = 0x80;
const CBS_UNINSTALL_PENDING: u32 = 0x05;

/// The SysWOW64 files the 32-bit game and PlayOnline load from DirectPlay.
pub(crate) const DIRECTPLAY_FILES: [&str; 3] = ["dpnhpast.dll", "dplayx.dll", "dpnet.dll"];
/// The June 2010 runtime files FFXI's install actually loads (xinputdll.dll
/// imports XINPUT1_3; d3dx9_43 is the June 2010 marker).
pub(crate) const DIRECTX_FILES: [&str; 2] = ["d3dx9_43.dll", "xinput1_3.dll"];
pub(crate) const VC_FILES: [&str; 2] = ["vcruntime140.dll", "msvcp140.dll"];

/// Whether a CBS package name belongs to the DirectPlay optional feature.
fn is_directplay_package(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("microsoft-windows-directplay-")
}

/// DirectPlay from the CBS package states, the optional-feature selection
/// flag and the SysWOW64 files. CBS is authoritative when readable; the files
/// decide only when Windows' servicing state is unavailable.
pub(crate) fn decide_directplay(
    packages: &[(String, u32)],
    selection: Option<u32>,
    files: &[(&str, bool)],
) -> Finding {
    let missing_files: Vec<&str> = files.iter().filter(|(_, ok)| !ok).map(|(n, _)| *n).collect();
    let files_ok = missing_files.is_empty();
    let states: Vec<u32> = packages
        .iter()
        .filter(|(name, _)| is_directplay_package(name))
        .map(|(_, state)| *state)
        .collect();
    let evidence = |state| Evidence::Feature { state, missing: missing_files.iter().map(|f| f.to_string()).collect() };
    if states.is_empty() {
        // Servicing state unreadable: fall back to the feature flag and files.
        let evidence = evidence(FeatureState::Unavailable);
        return if files_ok && selection != Some(0) {
            Finding { status: Status::Installed { version: None }, evidence }
        } else {
            Finding { status: Status::Missing { detail: "Turned off in Windows Features".into() }, evidence }
        };
    }
    let installed = states.iter().any(|s| matches!(*s, CBS_INSTALLED | CBS_PERMANENT));
    let pending = states
        .iter()
        .any(|s| matches!(*s, CBS_INSTALL_PENDING | CBS_PARTIALLY_INSTALLED | CBS_UNINSTALL_PENDING));
    if pending {
        return Finding { status: Status::RestartPending, evidence: evidence(FeatureState::Pending) };
    }
    if installed && files_ok {
        Finding { status: Status::Installed { version: None }, evidence: evidence(FeatureState::On) }
    } else if installed {
        Finding {
            status: Status::Missing { detail: format!("Feature is on but {} is missing", missing_files.join(", ")) },
            evidence: evidence(FeatureState::On),
        }
    } else {
        Finding {
            status: Status::Missing { detail: "Turned off in Windows Features".into() },
            evidence: evidence(FeatureState::Off),
        }
    }
}

/// DirectX June 2010 from the SysWOW64 files (name, file version if present).
pub(crate) fn decide_directx(files: &[(&str, Option<String>)]) -> Finding {
    let missing: Vec<&str> = files.iter().filter(|(_, v)| v.is_none()).map(|(n, _)| *n).collect();
    let d3dx = files.iter().find(|(n, _)| *n == "d3dx9_43.dll").and_then(|(_, v)| v.clone());
    if missing.is_empty() {
        // The raw file version; `shown_version` names the June 2010 release.
        Finding {
            status: Status::Installed { version: d3dx },
            evidence: Evidence::FilesPresent(files.iter().map(|(n, _)| n.to_string()).collect()),
        }
    } else {
        Finding {
            status: Status::Missing { detail: format!("{} not found", missing.join(", ")) },
            evidence: Evidence::FilesMissing(missing.iter().map(|n| n.to_string()).collect()),
        }
    }
}

/// `v14.51.36247.00` → `14.51.36247`.
pub(crate) fn parse_vc_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() < 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let numbers: Vec<u32> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    Some(format!("{}.{}.{}", numbers[0], numbers[1], numbers[2]))
}

/// Visual C++ x86 from the installer record (`Installed`, `Version`) and the
/// runtime files. A record without files, or files without a record, both
/// count: the first is what the redistributable writes, the second covers a
/// runtime another installer placed.
pub(crate) fn decide_vc(installed: Option<u32>, version: Option<&str>, files: &[(&str, Option<String>)]) -> Finding {
    let missing: Vec<&str> = files.iter().filter(|(_, v)| v.is_none()).map(|(n, _)| *n).collect();
    let file_version = files
        .iter()
        .find(|(n, _)| *n == "vcruntime140.dll")
        .and_then(|(_, v)| v.as_deref())
        .and_then(parse_vc_version);
    if installed == Some(1) && missing.is_empty() {
        return Finding {
            status: Status::Installed { version: version.and_then(parse_vc_version).or(file_version) },
            evidence: Evidence::VcRecordAndFiles,
        };
    }
    if installed == Some(1) {
        return Finding {
            status: Status::Missing { detail: format!("Installed record found but {} is missing", missing.join(", ")) },
            evidence: Evidence::FilesMissing(missing.iter().map(|n| n.to_string()).collect()),
        };
    }
    if missing.is_empty() {
        return Finding { status: Status::Installed { version: file_version }, evidence: Evidence::VcFilesOnly };
    }
    Finding { status: Status::Missing { detail: "Not installed".into() }, evidence: Evidence::VcNothing }
}

// ---- machine reads (Windows only) ------------------------------------------

/// Runs every check. Blocking; call from a worker thread.
#[cfg(windows)]
pub(crate) fn detect() -> Report {
    let wow = sys::syswow64();
    let directplay_files: Vec<(&str, bool)> =
        DIRECTPLAY_FILES.iter().map(|n| (*n, wow.join(n).is_file())).collect();
    let packages = sys::cbs_packages("Microsoft-Windows-DirectPlay-");
    let selection = sys::reg_dword(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\Notifications\OptionalFeatures\DirectPlay",
        "Selection",
    );
    let directplay = decide_directplay(&packages, selection, &directplay_files);

    let dx_files: Vec<(&str, Option<String>)> =
        DIRECTX_FILES.iter().map(|n| (*n, sys::file_version(&wow.join(n)))).collect();
    let directx = decide_directx(&dx_files);

    let key = r"SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x86";
    let vc_files: Vec<(&str, Option<String>)> =
        VC_FILES.iter().map(|n| (*n, sys::file_version(&wow.join(n)))).collect();
    let vc = decide_vc(sys::reg_dword(key, "Installed"), sys::reg_string(key, "Version").as_deref(), &vc_files);

    Report { findings: [directplay, directx, vc] }
}

#[cfg(not(windows))]
pub(crate) fn detect() -> Report {
    let na = || Finding { status: Status::Installed { version: None }, evidence: Evidence::NotApplicable };
    Report { findings: [na(), na(), na()] }
}

#[cfg(windows)]
pub(crate) mod sys {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
        KEY_WOW64_64KEY, REG_DWORD, REG_SZ,
    };

    pub(crate) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    pub(crate) fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub(crate) fn windows_dir() -> PathBuf {
        std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("windir"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
    }
    pub(crate) fn syswow64() -> PathBuf {
        windows_dir().join("SysWOW64")
    }
    pub(crate) fn system32() -> PathBuf {
        windows_dir().join("System32")
    }

    /// `a.b.c.d` from a file's fixed version resource; `None` when the file is
    /// absent. A present file without a version resource reports `"present"`.
    pub(crate) fn file_version(path: &Path) -> Option<String> {
        if !path.is_file() {
            return None;
        }
        let name = wide_os(path.as_os_str());
        unsafe {
            let mut handle = 0;
            let size = GetFileVersionInfoSizeW(name.as_ptr(), &mut handle);
            if size == 0 {
                return Some("present".into());
            }
            let mut data = vec![0u8; size as usize];
            if GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
                return Some("present".into());
            }
            let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            let root = wide("\\");
            if VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) == 0
                || info.is_null()
                || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
            {
                return Some("present".into());
            }
            let fixed = &*(info as *const VS_FIXEDFILEINFO);
            Some(format!(
                "{}.{}.{}.{}",
                fixed.dwFileVersionMS >> 16,
                fixed.dwFileVersionMS & 0xFFFF,
                fixed.dwFileVersionLS >> 16,
                fixed.dwFileVersionLS & 0xFFFF
            ))
        }
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn open(path: &str) -> Option<Key> {
        let path = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut key) };
        (status == ERROR_SUCCESS).then_some(Key(key))
    }
    fn query_dword(key: &Key, value: &str) -> Option<u32> {
        let value = wide(value);
        let mut kind = 0u32;
        let mut data = 0u32;
        let mut len = 4u32;
        let status = unsafe {
            RegQueryValueExW(key.0, value.as_ptr(), std::ptr::null(), &mut kind, (&mut data as *mut u32).cast(), &mut len)
        };
        (status == ERROR_SUCCESS && kind == REG_DWORD && len == 4).then_some(data)
    }
    pub(crate) fn reg_dword(path: &str, value: &str) -> Option<u32> {
        query_dword(&open(path)?, value)
    }
    pub(crate) fn reg_string(path: &str, value: &str) -> Option<String> {
        let key = open(path)?;
        let value = wide(value);
        let mut kind = 0u32;
        let mut buf = vec![0u16; 256];
        let mut len = (buf.len() * 2) as u32;
        let status = unsafe {
            RegQueryValueExW(key.0, value.as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut len)
        };
        if status != ERROR_SUCCESS || kind != REG_SZ {
            return None;
        }
        buf.truncate(len as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf16_lossy(&buf))
    }

    /// Every CBS package whose name starts with `prefix`, with its
    /// `CurrentState`. Readable without administrator rights.
    pub(crate) fn cbs_packages(prefix: &str) -> Vec<(String, u32)> {
        const ROOT: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\Packages";
        let Some(root) = open(ROOT) else {
            return Vec::new();
        };
        let prefix = prefix.to_ascii_lowercase();
        let mut found = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let status = unsafe {
                RegEnumKeyExW(
                    root.0,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            index += 1;
            if status != ERROR_SUCCESS {
                // ERROR_NO_MORE_ITEMS ends the walk; a too-long name is skipped.
                if status == windows_sys::Win32::Foundation::ERROR_MORE_DATA {
                    continue;
                }
                break;
            }
            let name = String::from_utf16_lossy(&name[..len as usize]);
            if !name.to_ascii_lowercase().starts_with(&prefix) {
                continue;
            }
            if let Some(state) = reg_dword(&format!(r"{ROOT}\{name}"), "CurrentState") {
                found.push((name, state));
            }
        }
        found
    }

    /// Whether this process runs with an elevated token.
    pub(crate) fn is_elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
            let mut returned = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkgs(list: &[(&str, u32)]) -> Vec<(String, u32)> {
        list.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }
    const ALL_FILES: [(&str, bool); 3] = [("dpnhpast.dll", true), ("dplayx.dll", true), ("dpnet.dll", true)];

    #[test]
    fn directplay_enabled_from_cbs() {
        // This machine's real layout: superseded 0x40 packages next to 0x70 ones.
        let p = pkgs(&[
            ("Microsoft-Windows-DirectPlay-OC-WOW64-Package~31bf3856ad364e35~amd64~~10.0.26100.1", 0x40),
            ("Microsoft-Windows-DirectPlay-OC-WOW64-Package~31bf3856ad364e35~amd64~~10.0.26100.4202", 0x70),
        ]);
        let f = decide_directplay(&p, Some(1), &ALL_FILES);
        assert_eq!(f.status, Status::Installed { version: None });
    }

    #[test]
    fn directplay_staged_only_is_missing() {
        let p = pkgs(&[("Microsoft-Windows-DirectPlay-OC-Package~31bf3856ad364e35~amd64~~10.0.26100.1", 0x40)]);
        // Files can exist as shims while the feature is off: CBS wins.
        let f = decide_directplay(&p, Some(0), &ALL_FILES);
        assert!(f.status.is_missing());
    }

    #[test]
    fn directplay_pending_restart() {
        let p = pkgs(&[("Microsoft-Windows-DirectPlay-OC-WOW64-Package~x~amd64~~10.0.1", 0x60)]);
        assert_eq!(decide_directplay(&p, Some(1), &ALL_FILES).status, Status::RestartPending);
    }

    #[test]
    fn directplay_on_but_file_missing() {
        let p = pkgs(&[("Microsoft-Windows-DirectPlay-OC-WOW64-Package~x~amd64~~10.0.1", 0x70)]);
        let files = [("dpnhpast.dll", false), ("dplayx.dll", true), ("dpnet.dll", true)];
        match decide_directplay(&p, Some(1), &files).status {
            Status::Missing { detail } => assert!(detail.contains("dpnhpast.dll")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn directplay_fallback_without_cbs() {
        assert_eq!(decide_directplay(&[], None, &ALL_FILES).status, Status::Installed { version: None });
        assert!(decide_directplay(&[], Some(0), &ALL_FILES).status.is_missing());
        let none = [("dpnhpast.dll", false), ("dplayx.dll", false), ("dpnet.dll", false)];
        assert!(decide_directplay(&[], None, &none).status.is_missing());
        // Unrelated packages are ignored.
        let other = pkgs(&[("Microsoft-Windows-Foo-Package~x~amd64~~1.0", 0x70)]);
        assert!(decide_directplay(&other, None, &none).status.is_missing());
    }

    #[test]
    fn directx_files() {
        let ok = [("d3dx9_43.dll", Some("9.29.952.3111".to_owned())), ("xinput1_3.dll", Some("9.18.944.0".to_owned()))];
        crate::i18n::set(crate::i18n::Lang::En);
        assert_eq!(decide_directx(&ok).status, Status::Installed { version: Some("9.29.952.3111".into()) });
        assert_eq!(shown_version(Component::DirectX9, "9.29.952.3111"), "June 2010 · 9.29.952.3111");
        assert_eq!(shown_version(Component::DirectX9, "9.26.1590.0"), "9.26.1590.0");
        assert_eq!(shown_version(Component::VcRedist, "9.29.952.1"), "9.29.952.1");
        let partial = [("d3dx9_43.dll", Some("9.29.952.3111".to_owned())), ("xinput1_3.dll", None)];
        match decide_directx(&partial).status {
            Status::Missing { detail } => assert_eq!(detail, "xinput1_3.dll not found"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vc_version_parsing() {
        assert_eq!(parse_vc_version("v14.51.36247.00").as_deref(), Some("14.51.36247"));
        assert_eq!(parse_vc_version("14.40.33810.0").as_deref(), Some("14.40.33810"));
        assert_eq!(parse_vc_version("v14"), None);
        assert_eq!(parse_vc_version("present"), None);
        assert_eq!(parse_vc_version(""), None);
    }

    #[test]
    fn vc_decisions() {
        let files = [("vcruntime140.dll", Some("14.51.36247.0".to_owned())), ("msvcp140.dll", Some("14.51.36247.0".to_owned()))];
        assert_eq!(
            decide_vc(Some(1), Some("v14.44.35211.00"), &files).status,
            Status::Installed { version: Some("14.44.35211".into()) }
        );
        assert_eq!(
            decide_vc(None, None, &files).status,
            Status::Installed { version: Some("14.51.36247".into()) }
        );
        let none = [("vcruntime140.dll", None), ("msvcp140.dll", None)];
        assert!(decide_vc(None, None, &none).status.is_missing());
        assert!(decide_vc(Some(1), Some("v14.1.1.0"), &none).status.is_missing());
        assert!(decide_vc(Some(0), None, &none).status.is_missing());
    }

    /// The "Checked:" line is worded when shown, from what the check read.
    #[test]
    fn evidence_is_worded_when_shown() {
        crate::i18n::set(crate::i18n::Lang::En);
        let on = decide_directplay(
            &pkgs(&[("Microsoft-Windows-DirectPlay-OC-WOW64-Package~x~amd64~~10.0.1", 0x70)]),
            Some(1),
            &[("dpnhpast.dll", false), ("dplayx.dll", true), ("dpnet.dll", false)],
        );
        assert_eq!(on.evidence.text(), "Windows feature: on · missing from SysWOW64: dpnhpast.dll, dpnet.dll");
        assert_eq!(
            decide_directplay(&[], None, &ALL_FILES).evidence.text(),
            "Windows feature state unavailable · dpnhpast, dplayx and dpnet present in SysWOW64"
        );
        let dx = [("d3dx9_43.dll", Some("9.29.952.3111".to_owned())), ("xinput1_3.dll", Some("9.18.944.0".to_owned()))];
        assert_eq!(decide_directx(&dx).evidence.text(), "d3dx9_43.dll, xinput1_3.dll present in SysWOW64");
        let none = [("vcruntime140.dll", None), ("msvcp140.dll", None)];
        assert_eq!(decide_vc(None, None, &none).evidence, Evidence::VcNothing);
        // The same finding, shown after a language change, uses that language's
        // words (the table's entry when it has one) and keeps the file names.
        crate::i18n::set(crate::i18n::Lang::Ja);
        let shown = on.evidence.text();
        assert!(shown.contains("dpnhpast.dll, dpnet.dll"), "{shown}");
        assert_eq!(
            shown,
            crate::i18n::tf(
                "Windows feature: on · {files}",
                &[("files", &crate::i18n::tf("missing from SysWOW64: {files}", &[("files", &"dpnhpast.dll, dpnet.dll")]))]
            )
        );
        crate::i18n::set(crate::i18n::Lang::En);
    }

    /// Live, read-only detection on this machine (`cargo test prereqs -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn live_detection() {
        let report = detect();
        for c in Component::ALL {
            println!("{}: {:?} — {}", c.name(), report.get(c).status, report.get(c).evidence.text());
        }
    }
}
