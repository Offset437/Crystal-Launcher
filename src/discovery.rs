//! Read-only discovery of this PC's game and launch tools. Paths are hints until
//! product markers validate them; no registry entries or user files are changed.

use std::{
    collections::VecDeque,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Product {
    Ffxi,
    PlayOnline,
    Windower,
    Ashita,
}

impl Product {
    const ALL: [Self; 4] = [Self::Ffxi, Self::PlayOnline, Self::Windower, Self::Ashita];
    fn index(self) -> usize {
        self as usize
    }
    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Ffxi => &["FINAL FANTASY XI", "FinalFantasyXI", "FFXI"],
            Self::PlayOnline => &["PlayOnlineViewer", "PlayOnline Viewer", "PlayOnline"],
            Self::Windower => &["Windower 4", "Windower4", "Windower"],
            Self::Ashita => &["AshitaV4", "Ashita v4", "Ashita4", "Ashita"],
        }
    }
    fn executable(self, name: &str) -> bool {
        let names: &[&str] = match self {
            Self::Ffxi => &["FFXiMain.dll", "FFXi.dll", "FFXiMain.exe"],
            Self::PlayOnline => &["pol.exe"],
            Self::Windower => &["Windower.exe"],
            Self::Ashita => &["Ashita-cli.exe", "Ashita.exe"],
        };
        names.iter().any(|n| name.eq_ignore_ascii_case(n))
    }
    fn display_name(self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        match self {
            Self::Ffxi => name.contains("final fantasy xi") || name.contains("finalfantasyxi") || name.contains("ffxi"),
            Self::PlayOnline => name.contains("playonline"),
            Self::Windower => name.contains("windower"),
            Self::Ashita => name.contains("ashita"),
        }
    }
}

/// A damaged official installation can still be located for Repair. The
/// embedded Crystal host named pol.exe is never mistaken for installed POL.
pub(crate) fn is_install(product: Product, folder: &Path) -> bool {
    match product {
        Product::Ffxi => ["FFXi.dll", "FFXiMain.dll", "ROM/0/0.DAT"].iter().any(|p| folder.join(p).is_file()),
        Product::PlayOnline => {
            ["viewer/com/polcore.dll", "viewer/com/app.dll"].iter().any(|p| folder.join(p).is_file())
                || (folder.join("pol.exe").is_file() && folder.join("viewer").is_dir())
        }
        Product::Windower => folder.join("Windower.exe").is_file(),
        Product::Ashita => folder.join("Ashita-cli.exe").is_file(),
    }
}

pub(crate) fn validated_folder(product: Product, path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return None;
    }
    let folder = if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| product.executable(n)) && path.is_file() {
        path.parent()?
    } else {
        path
    };
    if !is_install(product, folder) {
        return None;
    }
    let folder = crate::backend::plain_windows_path(&fs::canonicalize(folder).ok()?);
    folder.is_dir().then_some(folder)
}

/// Keep a valid selected copy first; stale paths from another PC fall back to
/// validated local evidence. Discovery does not save or replace that selection.
pub(crate) fn find(product: Product, preferred: Option<&Path>) -> Option<PathBuf> {
    if let Some(folder) = preferred.and_then(|p| validated_folder(product, p)) {
        return Some(folder);
    }
    #[cfg(windows)]
    if let Some(folder) = windows::registered_folder(product) {
        return Some(folder);
    }
    candidates(product).iter().find_map(|p| validated_folder(product, p))
}

type Inventory = [Vec<PathBuf>; 4];
static CACHE: Mutex<Option<(u64, Instant, Inventory)>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn invalidate() {
    // The UI's Find action never waits for a scan currently holding CACHE.
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn candidates(product: Product) -> Vec<PathBuf> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let generation = GENERATION.load(Ordering::Acquire);
    if cache.as_ref().is_none_or(|(saved, at, _)| *saved != generation || at.elapsed() > Duration::from_secs(30)) {
        *cache = Some((generation, Instant::now(), discover()));
    }
    cache.as_ref().unwrap().2[product.index()].clone()
}

fn push(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.as_os_str().is_empty() && !list.iter().any(|p| p.as_os_str().eq_ignore_ascii_case(path.as_os_str())) {
        list.push(path);
    }
}

fn remember(inventory: &mut Inventory, product: Product, hint: &Path) {
    if !local_candidate(hint) {
        return;
    }
    for candidate in std::iter::once(hint.to_path_buf()).chain(product.names().iter().map(|name| hint.join(name))) {
        if let Some(folder) = validated_folder(product, &candidate) {
            push(&mut inventory[product.index()], folder);
        }
    }
}

fn discover() -> Inventory {
    let mut inventory: Inventory = Default::default();
    let mut roots = Vec::new();
    let mut shortcuts = Vec::new();
    system_hints(&mut inventory, &mut roots, &mut shortcuts);
    // Installed POL and FFXI usually share a vendor folder. A selected product
    // may be registered even when the sibling registration is missing.
    for product in [Product::Ffxi, Product::PlayOnline] {
        for folder in inventory[product.index()].clone() {
            if let Some(parent) = folder.parent() {
                push(&mut roots, parent.to_path_buf());
            }
        }
    }
    for variable in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(path) = std::env::var_os(variable) {
            push(&mut roots, path.into());
        }
    }
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        push(&mut roots, PathBuf::from(path).join("Programs"));
    }
    if let Some(path) = std::env::var_os("USERPROFILE") {
        let path = PathBuf::from(path);
        for name in ["Games", "Documents", "Desktop", "Downloads"] {
            push(&mut roots, path.join(name));
        }
        push(&mut shortcuts, path.join("Desktop"));
    }
    if let Some(path) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        push(&mut roots, path);
    }
    for root in &roots {
        for vendor in
            ["PlayOnline", "PlayOnlineUS", "SquareEnix", "Square Enix", "Steam/steamapps/common", "steamapps/common"]
        {
            let vendor = root.join(vendor);
            for product in Product::ALL {
                remember(&mut inventory, product, &vendor);
            }
        }
        for product in Product::ALL {
            remember(&mut inventory, product, root);
        }
    }
    scan_shortcuts(&mut inventory, &shortcuts);
    scan_roots(&mut inventory, &roots);
    inventory
}

fn plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        if !m.is_dir() || m.file_type().is_symlink() {
            return false;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if m.file_attributes() & 0x400 != 0 {
                return false;
            }
        }
        true
    })
}

fn scan_roots(inventory: &mut Inventory, roots: &[PathBuf]) {
    // Broad enough for unpacked tools in a nested Downloads/Games folder, but
    // never recursively walk whole disks, junctions, source trees or caches.
    let mut remaining = 1600usize;
    let mut entries_left = 24_000usize;
    for root in roots.iter().take(64) {
        if !local_candidate(root) || !plain_directory(root) {
            continue;
        }
        let mut queue = VecDeque::from([(root.clone(), 0u8)]);
        let mut per_root = 0usize;
        while let Some((folder, depth)) = queue.pop_front() {
            if remaining == 0 || per_root >= 160 || entries_left == 0 {
                break;
            }
            remaining -= 1;
            per_root += 1;
            for product in Product::ALL {
                remember(inventory, product, &folder);
            }
            if depth >= 3 {
                continue;
            }
            let Ok(entries) = fs::read_dir(&folder) else {
                continue;
            };
            let entries: Vec<_> = entries.take(entries_left.min(512)).flatten().collect();
            entries_left -= entries.len();
            let mut children: Vec<_> = entries
                .into_iter()
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with('.')
                        && !["target", "node_modules", "cache", "Windows", "WindowsApps", "$Recycle.Bin"]
                            .iter()
                            .any(|skip| name.eq_ignore_ascii_case(skip))
                        && plain_directory(&entry.path())
                })
                .map(|entry| entry.path())
                .collect();
            children.sort_by_key(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
                let related = Product::ALL.iter().any(|product| product.display_name(&name))
                    || ["squareenix", "square enix", "steam", "steamapps", "common", "games"].contains(&name.as_str());
                (!related, name)
            });
            queue.extend(children.into_iter().map(|p| (p, depth + 1)));
        }
    }
}

fn scan_shortcuts(inventory: &mut Inventory, roots: &[PathBuf]) {
    let mut remaining = 1024usize;
    for root in roots {
        if !local_candidate(root) || !plain_directory(root) {
            continue;
        }
        let mut queue = VecDeque::from([(root.clone(), 0u8)]);
        while let Some((folder, depth)) = queue.pop_front() {
            if remaining == 0 {
                return;
            }
            let Ok(entries) = fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.take(256).flatten() {
                remaining -= 1;
                let path = entry.path();
                if depth < 3 && plain_directory(&path) {
                    queue.push_back((path, depth + 1));
                } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
                    && path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| Product::ALL.iter().any(|p| p.display_name(n)))
                {
                    if let Some(bytes) = read_shortcut(&path) {
                        for hint in shortcut_paths(&bytes) {
                            for product in Product::ALL {
                                remember(inventory, product, &hint);
                            }
                        }
                    }
                }
                if remaining == 0 {
                    return;
                }
            }
        }
    }
}

fn read_shortcut(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return None;
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= 1024 * 1024).then_some(bytes)
}

/// Read local Shell Link target/working-directory fields only. No COM
/// resolution, shortcut execution, network target, arguments or icon loading.
fn shortcut_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let u16_at = |offset| bytes.get(offset..offset + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()));
    let u32_at = |offset| bytes.get(offset..offset + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let mut paths = Vec::new();
    if u32_at(0) != Some(76) || bytes.get(4..20) != Some(&[1, 0x14, 2, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46][..])
    {
        return paths;
    }
    let Some(flags) = u32_at(20) else {
        return paths;
    };
    let mut cursor = 76usize;
    if flags & 1 != 0 {
        let Some(size) = u16_at(cursor) else {
            return paths;
        };
        cursor += 2 + size as usize;
    }
    if flags & 2 != 0 {
        let Some(size) = u32_at(cursor) else {
            return paths;
        };
        let Some(info) = cursor.checked_add(size).and_then(|end| bytes.get(cursor..end)) else {
            return paths;
        };
        if size < 28 {
            return paths;
        }
        let field = |at| info.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        let text = |offset: usize, unicode: bool| -> Option<String> {
            if offset < 28 || offset >= info.len() {
                return None;
            }
            let data = &info[offset..];
            if unicode {
                let words: Vec<_> =
                    data.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).take_while(|n| *n != 0).collect();
                String::from_utf16(&words).ok()
            } else {
                let end = data.iter().position(|b| *b == 0)?;
                Some(ansi(&data[..end]))
            }
        };
        let unicode = field(4).is_some_and(|header| header >= 36);
        if let Some(base) = (if unicode { field(28).and_then(|at| text(at, true)) } else { None })
            .or_else(|| field(16).and_then(|at| text(at, false)))
        {
            if !base.is_empty() {
                push(&mut paths, PathBuf::from(base));
            }
        }
        cursor += size;
    }
    for flag in [4, 8, 16] {
        if flags & flag == 0 {
            continue;
        }
        let Some(length) = u16_at(cursor) else {
            return paths;
        };
        cursor += 2;
        let unicode = flags & 128 != 0;
        let length = length as usize * if unicode { 2 } else { 1 };
        let Some(data) = cursor.checked_add(length).and_then(|end| bytes.get(cursor..end)) else {
            return paths;
        };
        if flag == 16 {
            let value = if unicode {
                String::from_utf16(&data.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>())
                    .ok()
            } else {
                Some(ansi(data))
            };
            if let Some(value) = value {
                push(&mut paths, PathBuf::from(value));
            }
        }
        cursor += length;
    }
    paths
}

#[cfg(windows)]
fn ansi(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP};
    let mut wide = vec![0u16; bytes.len()];
    // SAFETY: bounded slices with matching input/output lengths.
    let size = unsafe {
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), wide.len() as i32)
    };
    String::from_utf16_lossy(&wide[..size.max(0) as usize])
}
#[cfg(not(windows))]
fn ansi(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn local_candidate(path: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let path = crate::backend::plain_windows_path(path);
    let Some(text) = path.to_str() else {
        return false;
    };
    let bytes = text.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || !matches!(bytes[2], b'\\' | b'/') {
        return false;
    }
    let root: Vec<_> = format!("{}:\\", char::from(bytes[0])).encode_utf16().chain(Some(0)).collect();
    // DRIVE_FIXED: automatic discovery never probes disconnected network or USB drives.
    unsafe { GetDriveTypeW(root.as_ptr()) == 3 }
}
#[cfg(not(windows))]
fn local_candidate(path: &Path) -> bool {
    path.is_absolute()
}

#[cfg(not(windows))]
fn system_hints(_: &mut Inventory, _: &mut Vec<PathBuf>, _: &mut Vec<PathBuf>) {}

#[cfg(windows)]
fn system_hints(inventory: &mut Inventory, roots: &mut Vec<PathBuf>, shortcuts: &mut Vec<PathBuf>) {
    windows::registry_hints(inventory, roots, shortcuts);
    windows::running_hints(inventory);
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    for index in 0..26 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let drive = PathBuf::from(format!("{}:\\", char::from(b'A' + index)));
        if !local_candidate(&drive) {
            continue;
        }
        for name in [
            "Games",
            "PlayOnline",
            "SquareEnix",
            "Square Enix",
            "SteamLibrary/steamapps/common",
            "Program Files",
            "Program Files (x86)",
        ] {
            push(roots, drive.join(name));
        }
        // Named folders directly under any fixed drive are also common.
        for product in Product::ALL {
            remember(inventory, product, &drive);
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Registry::*;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn value(hive: HKEY, key: &str, name: &str, view: u32) -> Option<String> {
        let key = wide(key);
        let name = wide(name);
        let mut buffer = vec![0u16; 16384];
        let mut size = (buffer.len() * 2) as u32;
        // RegGetValue expands REG_EXPAND_SZ itself; bound data and never read a
        // command line, credential, or arbitrary registry tree.
        let status = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | view,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS || size > buffer.len() as u32 * 2 {
            return None;
        }
        let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16(&buffer[..length]).ok().filter(|s| !s.is_empty())
    }

    fn subkeys(hive: HKEY, path: &str, view: u32) -> Vec<String> {
        let mut key = std::ptr::null_mut();
        let path = wide(path);
        if unsafe { RegOpenKeyExW(hive, path.as_ptr(), 0, KEY_ENUMERATE_SUB_KEYS | view, &mut key) } != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut keys = Vec::new();
        for index in 0..2048 {
            let mut name = [0u16; 256];
            let mut size = name.len() as u32;
            if unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &mut size,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } != ERROR_SUCCESS
            {
                break;
            }
            keys.push(String::from_utf16_lossy(&name[..size as usize]));
        }
        unsafe { RegCloseKey(key) };
        keys
    }

    fn location(raw: &str) -> Option<PathBuf> {
        let raw = raw.trim();
        let path = if let Some(quoted) = raw.strip_prefix('"') {
            quoted.split_once('"')?.0
        } else {
            raw.rsplit_once(',').filter(|(_, tail)| tail.trim().parse::<i32>().is_ok()).map_or(raw, |(path, _)| path)
        };
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    /// Common launches need a handful of registry reads, not a full inventory.
    /// A missing/stale entry still reaches the broader cached discovery below.
    pub(super) fn registered_folder(product: Product) -> Option<PathBuf> {
        for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            for view in [RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY] {
                let paths: Vec<_> = match product {
                    Product::Ffxi | Product::PlayOnline => {
                        let title = if product == Product::Ffxi { "0001" } else { "1000" };
                        value(hive, r"SOFTWARE\PlayOnlineUS\InstallFolder", title, view).into_iter().collect()
                    }
                    Product::Windower | Product::Ashita => {
                        let exes: &[&str] = if product == Product::Windower {
                            &["Windower.exe"]
                        } else {
                            &["Ashita-cli.exe", "Ashita.exe"]
                        };
                        exes.iter()
                            .filter_map(|exe| {
                                value(
                                    hive,
                                    &format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}"),
                                    "",
                                    view,
                                )
                            })
                            .collect()
                    }
                };
                for path in paths.into_iter().filter_map(|s| location(&s)) {
                    if !local_candidate(&path) {
                        continue;
                    }
                    if let Some(folder) = validated_folder(product, &path) {
                        return Some(folder);
                    }
                }
            }
        }
        None
    }

    pub(super) fn registry_hints(inventory: &mut Inventory, roots: &mut Vec<PathBuf>, shortcuts: &mut Vec<PathBuf>) {
        for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            for (read_view, open_view) in
                [(RRF_SUBKEY_WOW6432KEY, KEY_WOW64_32KEY), (RRF_SUBKEY_WOW6464KEY, KEY_WOW64_64KEY)]
            {
                // US is the current runtime region; finding paths adds no new
                // claim of native authentication support for another region.
                for (product, title) in [(Product::Ffxi, "0001"), (Product::PlayOnline, "1000")] {
                    if let Some(path) =
                        value(hive, r"SOFTWARE\PlayOnlineUS\InstallFolder", title, read_view).and_then(|s| location(&s))
                    {
                        remember(inventory, product, &path);
                        if let Some(parent) = path.parent() {
                            push(roots, parent.to_path_buf());
                        }
                    }
                }
                for (product, exes) in [
                    (Product::Windower, &["Windower.exe"][..]),
                    (Product::Ashita, &["Ashita.exe", "Ashita-cli.exe"][..]),
                    (Product::PlayOnline, &["pol.exe"][..]),
                ] {
                    for exe in exes {
                        let key = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
                        if let Some(path) = value(hive, &key, "", read_view).and_then(|s| location(&s)) {
                            remember(inventory, product, &path);
                        }
                    }
                }
                let uninstall = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
                for child in subkeys(hive, uninstall, open_view) {
                    let key = format!("{uninstall}\\{child}");
                    let Some(name) = value(hive, &key, "DisplayName", read_view) else {
                        continue;
                    };
                    let products: Vec<_> = Product::ALL.into_iter().filter(|p| p.display_name(&name)).collect();
                    if products.is_empty() {
                        continue;
                    }
                    for field in ["InstallLocation", "DisplayIcon"] {
                        if let Some(path) = value(hive, &key, field, read_view).and_then(|s| location(&s)) {
                            for product in &products {
                                remember(inventory, *product, &path);
                            }
                            let folder = if path.is_file() { path.parent().unwrap_or(&path) } else { &path };
                            push(roots, folder.to_path_buf());
                        }
                    }
                }
            }
        }
        let shell = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders";
        for name in ["Personal", "Desktop", "{374DE290-123F-4565-9164-39C4925E467B}"] {
            if let Some(path) = value(HKEY_CURRENT_USER, shell, name, 0).and_then(|s| location(&s)) {
                if name == "Desktop" {
                    push(shortcuts, path.clone());
                }
                push(roots, path);
            }
        }
        for (hive, names) in [
            (HKEY_CURRENT_USER, &["Programs", "Start Menu"][..]),
            (HKEY_LOCAL_MACHINE, &["Common Programs", "Common Start Menu", "Common Desktop"][..]),
        ] {
            for name in names {
                if let Some(path) = value(hive, shell, name, 0).and_then(|s| location(&s)) {
                    push(shortcuts, path);
                }
            }
        }
        for key in [r"SOFTWARE\Valve\Steam", r"SOFTWARE\WOW6432Node\Valve\Steam"] {
            for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
                if let Some(path) = value(hive, key, "SteamPath", 0)
                    .or_else(|| value(hive, key, "InstallPath", 0))
                    .and_then(|s| location(&s))
                {
                    push(roots, path.join("steamapps/common"));
                }
            }
        }
    }

    pub(super) fn running_hints(inventory: &mut Inventory) {
        use windows_sys::Win32::System::{
            Diagnostics::ToolHelp::*,
            Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        for _ in 0..4096 {
            if !has_entry {
                break;
            }
            let length = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
            if Product::ALL.iter().any(|p| p.executable(&name)) {
                let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID) };
                if !process.is_null() {
                    let mut path = vec![0u16; 32768];
                    let mut length = path.len() as u32;
                    if unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) } != 0 {
                        let path = PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
                        for product in Product::ALL {
                            if product.executable(&name) {
                                remember(inventory, product, &path);
                            }
                        }
                    }
                    unsafe { CloseHandle(process) };
                }
            }
            has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "crystal-discovery-{}-{}",
            std::process::id(),
            crate::backend::unique_id().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn nested_tools_and_sibling_game_are_found_without_pc_specific_paths() {
        let root = fixture();
        let ashita = root.join("Games/Custom tools/Release 4");
        let windower = root.join("Downloads/An extracted archive/Launcher");
        let game = root.join("Games/PlayOnline/SquareEnix/FINAL FANTASY XI");
        for (folder, marker) in [(&ashita, "Ashita-cli.exe"), (&windower, "Windower.exe"), (&game, "FFXi.dll")] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join(marker), b"fixture").unwrap();
        }
        let mut inventory = Default::default();
        scan_roots(&mut inventory, &[root.join("Games"), root.join("Downloads")]);
        for (product, path) in [(Product::Ashita, &ashita), (Product::Windower, &windower), (Product::Ffxi, &game)] {
            assert_eq!(inventory[product.index()], [validated_folder(product, path).unwrap()]);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_selected_copy_wins_and_crystals_host_is_not_installed_pol() {
        let root = fixture();
        let selected = root.join("selected");
        let other = root.join("registered");
        for path in [&selected, &other] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("Windower.exe"), b"fixture").unwrap();
        }
        assert_eq!(find(Product::Windower, Some(&selected)), validated_folder(Product::Windower, &selected));
        fs::create_dir(root.join("crystal")).unwrap();
        fs::write(root.join("crystal/pol.exe"), b"Crystal host fixture").unwrap();
        assert!(!is_install(Product::PlayOnline, &root.join("crystal")));
        assert!(validated_folder(Product::Windower, &root.join("../selected")).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn steam_style_forward_slashes_are_valid_local_search_roots() {
        let root = fixture();
        let tool = root.join("Windower");
        fs::create_dir(&tool).unwrap();
        fs::write(tool.join("Windower.exe"), b"fixture").unwrap();
        let steam_style = PathBuf::from(root.to_string_lossy().replace('\\', "/"));
        let mut inventory = Default::default();
        scan_roots(&mut inventory, &[steam_style]);
        assert_eq!(inventory[Product::Windower.index()], [validated_folder(Product::Windower, &tool).unwrap()]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_unicode_shortcut_reads_target_and_tolerates_truncated_data() {
        let target = r"D:\Games\Joueur été\Windower.exe";
        let mut bytes = vec![0u8; 76];
        bytes[..4].copy_from_slice(&76u32.to_le_bytes());
        bytes[4..20].copy_from_slice(&[1, 0x14, 2, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
        bytes[20..24].copy_from_slice(&2u32.to_le_bytes());
        let words: Vec<_> = target.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        let mut info = vec![0u8; 36];
        info[..4].copy_from_slice(&(36u32 + words.len() as u32).to_le_bytes());
        info[4..8].copy_from_slice(&36u32.to_le_bytes());
        info[28..32].copy_from_slice(&36u32.to_le_bytes());
        info.extend(words);
        bytes.extend(info);
        assert_eq!(shortcut_paths(&bytes), [PathBuf::from(target)]);
        for end in 0..bytes.len() {
            assert!(shortcut_paths(&bytes[..end]).is_empty());
        }
        let mut malformed = bytes;
        malformed[76..80].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(shortcut_paths(&malformed).is_empty());
    }
}
