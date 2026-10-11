//! dgVoodoo 2 for the game: Settings → Graphics turns it on, and Crystal downloads, verifies
//! and sets it up on its own, like the official launchers do with their extras.
//!
//! FFXiMain imports `d3d8.dll` by name, so a `D3D8.dll` in the folder of the exe that hosts the
//! game wins over the system one (DLL search order). The game runs inside Crystal's own loader,
//! `crystal\pol.exe` (or a versioned `crystal\<hash8>\pol.exe`, or the Settings override), so the
//! wrapper goes next to whichever loader a launch actually runs.
//!
//! * The release zip is pinned by URL and SHA-256. Nothing is extracted from a zip that doesn't
//!   match, and every extracted file must match its own pinned hash too, so no unverified DLL is
//!   ever written where an elevated game could load it.
//! * Only the files FFXI needs are kept, in `<data>\crystal\dgvoodoo\<version>\` with the
//!   settings app (`dgVoodooCpl.exe`) and the user's `dgVoodoo.conf`, which the settings app edits
//!   in its own folder.
//! * Each launch copies the files next to the loader when they are missing or differ. A file
//!   Crystal didn't put there is renamed aside first, never deleted. With the option off, only
//!   files that match what Crystal placed are removed.

use crate::i18n::{t, tf};
use crate::icons::{icon, Icon};
use crate::ui::{colors, primary, quiet, scale, Colors, BODY, SEMI};
use crate::{App, Message};
use iced::widget::{column, container, progress_bar, row, text, toggler, Space};
use iced::{Alignment, Border, Color, Element, Length, Subscription, Task};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// The pinned dgVoodoo 2 release. v2.86.5 (what Ashita's bootloader ships) has no files on its
/// GitHub release and is gone from the author's site, so this is the newest release that is
/// published on GitHub with a recorded digest.
pub(crate) const VERSION: &str = "2.87.5";
const URL: &str = "https://github.com/dege-diosg/dgVoodoo2/releases/download/v2.87.5/dgVoodoo2_87_5.zip";

/// One file taken from the zip: where it is in the zip, what it is called on disk, its SHA-256.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pinned {
    pub entry: &'static str,
    pub name: &'static str,
    pub sha256: &'static str,
}

/// A release: the zip's own hash and size cap, the DLLs, the settings app and the stock conf.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Release {
    pub version: &'static str,
    pub zip_sha256: &'static str,
    pub max_zip: u64,
    pub dlls: &'static [Pinned],
    pub cpl: Pinned,
    pub conf: Pinned,
}

pub(crate) const CONF: &str = "dgVoodoo.conf";
/// Next to the loader: what Crystal placed there and each file's hash, so only those are touched.
const MANIFEST: &str = "crystal-dgvoodoo.txt";

pub(crate) static PINNED: Release = Release {
    version: VERSION,
    zip_sha256: "5ffde6927f7355ca3fdd5d785b581256a8e6539fa13e395a891ade6ba1040850",
    max_zip: 32 * 1024 * 1024,
    dlls: &[
        Pinned { entry: "MS/x86/D3D8.dll", name: "D3D8.dll", sha256: "d03e2562178db1fcf3493fc0a0b23a465e35c55d73adb3ec095be866cd662704" },
        Pinned { entry: "MS/x86/D3DImm.dll", name: "D3DImm.dll", sha256: "93c534f2d17419ea78f15551f7e0aac78b3c503733a840914fa063708a5afe8e" },
        Pinned { entry: "MS/x86/DDraw.dll", name: "DDraw.dll", sha256: "612a24408a090a3c6f3886557fa18034ee742e94ad0a40ebdf854d2816176c2e" },
    ],
    cpl: Pinned { entry: "dgVoodooCpl.exe", name: "dgVoodooCpl.exe", sha256: "87fb878166dcaef3e75ff170de030527c73f5c9b164c5943da13f8a97b0b75c6" },
    conf: Pinned { entry: "dgVoodoo.conf", name: CONF, sha256: "45cc0eb8e6ed4f067aca1b2e4ac1268dfbd164bbbd06d264c53e8a60642c4dbf" },
};

/// Settings-side installs and launch-time installs never write the cache at the same time.
static WRITING: Mutex<()> = Mutex::new(());

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn file_hash(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| sha256_hex(&b))
}

fn has_hash(path: &Path, hash: &str) -> bool {
    file_hash(path).is_some_and(|h| h == hash)
}

/// Crystal's per-user data folder (the one `crystal\pol.exe` lives under).
pub(crate) fn data_root() -> PathBuf {
    crate::pol_less::tools::embedded::data_dir()
}

/// `<root>\crystal\dgvoodoo\<version>`.
pub(crate) fn cache_dir(root: &Path, release: &Release) -> PathBuf {
    root.join("crystal").join("dgvoodoo").join(release.version)
}

/// Every DLL (and the settings app) is in the cache and matches its pinned hash.
pub(crate) fn installed(root: &Path, release: &Release) -> bool {
    let dir = cache_dir(root, release);
    release.dlls.iter().all(|p| has_hash(&dir.join(p.name), p.sha256))
        && has_hash(&dir.join(release.cpl.name), release.cpl.sha256)
        && dir.join(CONF).is_file()
}

/// Writes through a temp file and a rename, so a reader never sees half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let result = fs::write(&temp, bytes).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The wanted entries' bytes, by their on-disk name. Entry names compare without case and with
/// either slash; anything missing is an error.
pub(crate) fn pick_files(zip: &[u8], wanted: &[Pinned]) -> Result<Vec<(Pinned, Vec<u8>)>, String> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(io::Cursor::new(zip))
        .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
    let norm = |name: &str| name.replace('\\', "/").to_ascii_lowercase();
    let mut found = Vec::with_capacity(wanted.len());
    for pin in wanted {
        let want = norm(pin.entry);
        let index = (0..archive.len())
            .find(|&i| archive.name_for_index(i).is_some_and(|n| norm(n) == want))
            .ok_or_else(|| tf("The dgVoodoo download has no {file}.", &[("file", &pin.entry)]))?;
        let mut entry = archive
            .by_index(index)
            .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
        if entry.size() > 16 * 1024 * 1024 {
            return Err(tf("The dgVoodoo download has no {file}.", &[("file", &pin.entry)]));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
        found.push((*pin, bytes));
    }
    Ok(found)
}

/// The release's stock conf with the on-screen dgVoodoo watermark off; everything else as shipped.
pub(crate) fn watermark_off(conf: &str) -> String {
    let mut out = String::with_capacity(conf.len());
    for line in conf.split_inclusive('\n') {
        let key = line.split('=').next().unwrap_or("").trim();
        if key.eq_ignore_ascii_case("dgVoodooWatermark") && line.contains('=') {
            let (head, value) = line.split_once('=').unwrap();
            let ending = if value.ends_with("\r\n") { "\r\n" } else if value.ends_with('\n') { "\n" } else { "" };
            let pad = &value[..value.len() - value.trim_start().len()];
            out.push_str(&format!("{head}={pad}false{ending}"));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Where the conf Crystal starts from comes from: one the user already has in Crystal's folder
/// (this version's, else an older version's), then PlayOnline's, then the release default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfSource {
    Kept,
    Crystal(PathBuf),
    PlayOnline(PathBuf),
    Default,
}

/// Puts a `dgVoodoo.conf` in `dir` unless one is there already (never overwrites the user's).
pub(crate) fn seed_conf(dir: &Path, pol_conf: Option<&Path>, release_conf: &[u8]) -> io::Result<ConfSource> {
    let target = dir.join(CONF);
    if target.is_file() {
        return Ok(ConfSource::Kept);
    }
    let readable = |p: &Path| fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= 1024 * 1024);
    // An older version's conf in Crystal's own dgvoodoo folder, newest first.
    let mut older: Vec<(std::time::SystemTime, PathBuf)> = dir
        .parent()
        .and_then(|p| fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(CONF))
        .filter(|p| p.parent() != Some(dir) && readable(p))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    older.sort();
    let (bytes, source) = if let Some((_, path)) = older.pop() {
        (fs::read(&path)?, ConfSource::Crystal(path))
    } else if let Some(path) = pol_conf.filter(|p| readable(p)) {
        (fs::read(path)?, ConfSource::PlayOnline(path.to_owned()))
    } else {
        (watermark_off(&String::from_utf8_lossy(release_conf)).into_bytes(), ConfSource::Default)
    };
    write_atomic(&target, &bytes)?;
    Ok(source)
}

/// PlayOnline's own `dgVoodoo.conf`, from the Viewer's install folder in the registry.
pub(crate) fn playonline_conf() -> Option<PathBuf> {
    crate::patch::registry_install_folder("1000").map(|dir| dir.join(CONF)).filter(|p| p.is_file())
}

/// Checks the zip against the pinned hash, then writes only the needed files to the cache and
/// seeds the conf. Nothing is written for a zip that doesn't match.
pub(crate) fn install_from_zip(root: &Path, zip: &[u8], release: &Release, pol_conf: Option<&Path>) -> Result<ConfSource, String> {
    if sha256_hex(zip) != release.zip_sha256 {
        return Err(t("The dgVoodoo download doesn't match its pinned SHA-256, so nothing was installed.").into());
    }
    let mut wanted: Vec<Pinned> = release.dlls.to_vec();
    wanted.push(release.cpl);
    wanted.push(release.conf);
    let files = pick_files(zip, &wanted)?;
    for (pin, bytes) in &files {
        if sha256_hex(bytes) != pin.sha256 {
            return Err(tf("{file} in the dgVoodoo download doesn't match its pinned SHA-256, so nothing was installed.", &[("file", &pin.name)]));
        }
    }
    let dir = cache_dir(root, release);
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let save_error = |e: io::Error| tf("Could not save dgVoodoo in {folder}: {error}", &[("folder", &dir.display()), ("error", &e)]);
    let mut release_conf: &[u8] = &[];
    for (pin, bytes) in &files {
        if pin.entry == release.conf.entry {
            release_conf = bytes;
        } else if !has_hash(&dir.join(pin.name), pin.sha256) {
            write_atomic(&dir.join(pin.name), bytes).map_err(save_error)?;
        }
    }
    seed_conf(&dir, pol_conf, release_conf).map_err(save_error)
}

// ---- next to the loader ----

fn read_manifest(dir: &Path) -> Vec<(String, String)> {
    fs::read_to_string(dir.join(MANIFEST))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(h, n)| (n.trim().to_owned(), h.trim().to_ascii_lowercase())))
        .filter(|(n, h)| !n.contains(['/', '\\']) && h.len() == 64)
        .collect()
}

fn write_manifest(dir: &Path, entries: &[(String, String)]) -> io::Result<()> {
    let body: String = entries.iter().map(|(n, h)| format!("{h} {n}\n")).collect();
    write_atomic(&dir.join(MANIFEST), body.as_bytes())
}

/// What a sync did next to the loader.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Placed {
    pub copied: Vec<String>,
    /// Files that were there, weren't Crystal's, and were renamed aside.
    pub set_aside: Vec<String>,
}

/// Makes the loader's folder hold the cached DLLs and conf. Copies a file only when it is missing
/// or differs; a differing file Crystal didn't place is renamed to `<name>.crystal-backup[-n]`.
pub(crate) fn place(root: &Path, release: &Release, loader_dir: &Path) -> Result<Placed, String> {
    let cache = cache_dir(root, release);
    let mut sources: Vec<(String, PathBuf, String)> = Vec::new();
    for pin in release.dlls {
        let path = cache.join(pin.name);
        if !has_hash(&path, pin.sha256) {
            return Err(t("dgVoodoo is not installed yet.").into());
        }
        sources.push((pin.name.to_owned(), path, pin.sha256.to_owned()));
    }
    let conf = cache.join(CONF);
    let conf_hash = file_hash(&conf).ok_or_else(|| t("dgVoodoo is not installed yet.").to_owned())?;
    sources.push((CONF.to_owned(), conf, conf_hash));
    let ours = read_manifest(loader_dir);
    let mut placed = Placed::default();
    let mut manifest = Vec::new();
    let fail = |e: io::Error| tf("Could not set up dgVoodoo in {folder}: {error}", &[("folder", &loader_dir.display()), ("error", &e)]);
    for (name, source, hash) in sources {
        let target = loader_dir.join(&name);
        match file_hash(&target) {
            Some(current) if current == hash => {}
            current => {
                let mine = current.as_ref().is_some_and(|h| ours.iter().any(|(n, o)| n.eq_ignore_ascii_case(&name) && o == h))
                    || current.as_ref().is_some_and(|h| release.dlls.iter().any(|p| p.sha256 == h));
                if current.is_some() && !mine {
                    let aside = (0..)
                        .map(|n| loader_dir.join(if n == 0 { format!("{name}.crystal-backup") } else { format!("{name}.crystal-backup-{n}") }))
                        .find(|p| !p.exists())
                        .expect("an unused name");
                    fs::rename(&target, &aside).map_err(fail)?;
                    placed.set_aside.push(name.clone());
                }
                let bytes = fs::read(&source).map_err(fail)?;
                if sha256_hex(&bytes) != hash {
                    return Err(t("dgVoodoo is not installed yet.").into());
                }
                write_atomic(&target, &bytes).map_err(fail)?;
                placed.copied.push(name.clone());
            }
        }
        manifest.push((name, hash));
    }
    write_manifest(loader_dir, &manifest).map_err(fail)?;
    Ok(placed)
}

/// Removes, from a loader's folder, only files that match the pinned DLLs or what Crystal's
/// manifest says it placed (the conf). Anything else stays. A failed removal keeps the
/// manifest so a later attempt can still recognize the conf and older release's DLLs.
pub(crate) fn remove_from(release: &Release, loader_dir: &Path) -> Result<Vec<String>, String> {
    let ours = read_manifest(loader_dir);
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    let failure = |path: &Path, error: io::Error| {
        tf("Could not remove {file}: {error}", &[("file", &path.display()), ("error", &error)])
    };
    let mut names: Vec<String> = release.dlls.iter().map(|p| p.name.to_owned()).collect();
    names.push(CONF.to_owned());
    for name in names {
        let target = loader_dir.join(&name);
        let current = match fs::read(&target) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                errors.push(failure(&target, error));
                continue;
            }
        };
        let pinned = release.dlls.iter().any(|p| p.name.eq_ignore_ascii_case(&name) && p.sha256 == current);
        let recorded = ours.iter().any(|(n, h)| n.eq_ignore_ascii_case(&name) && *h == current);
        if pinned || recorded {
            match fs::remove_file(&target) {
                Ok(()) => removed.push(name),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => errors.push(failure(&target, error)),
            }
        }
    }
    if errors.is_empty() {
        let manifest = loader_dir.join(MANIFEST);
        match fs::remove_file(&manifest) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => errors.push(failure(&manifest, error)),
        }
    }
    if errors.is_empty() { Ok(removed) } else { Err(errors.join("\n")) }
}

/// The folders a loader can run from: `crystal\`, each versioned `crystal\<hash8>\`, and the
/// Settings override's folder.
pub(crate) fn loader_dirs(root: &Path, override_path: &str) -> Vec<PathBuf> {
    let crystal = root.join("crystal");
    let mut dirs = vec![crystal.clone()];
    for entry in fs::read_dir(&crystal).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit()) && entry.path().join("pol.exe").is_file() {
            dirs.push(entry.path());
        }
    }
    let custom = override_path.trim();
    if !custom.is_empty() {
        if let Some(parent) = Path::new(custom).parent() {
            if !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_owned());
            }
        }
    }
    dirs
}

/// Removes the placed files everywhere a loader can run from.
fn remove_everywhere(root: &Path, override_path: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for dir in loader_dirs(root, override_path) {
        if let Err(error) = remove_from(&PINNED, &dir) {
            errors.push(error);
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

/// Deletes the cached DLLs and settings app. The user's `dgVoodoo.conf` stays for next time.
fn uninstall(root: &Path, override_path: &str) -> Result<(), String> {
    remove_everywhere(root, override_path)?;
    let dir = cache_dir(root, &PINNED);
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    for pin in PINNED.dlls.iter().chain([&PINNED.cpl]) {
        let path = dir.join(pin.name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| tf("Could not remove {file}: {error}", &[("file", &path.display()), ("error", &e)]))?;
        }
    }
    Ok(())
}

/// Run in the launch (the session worker), with the exact loader it starts: puts dgVoodoo next to
/// it, or takes Crystal's copy away when the option is off. Never fails a launch; a problem comes
/// back as a line for the launch status.
pub(crate) fn sync_for_launch(loader: &Path, enabled: bool) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let dir = loader.parent()?;
    if !enabled {
        return remove_from(&PINNED, dir).err();
    }
    let root = data_root();
    if !installed(&root, &PINNED) {
        return Some(t("dgVoodoo is not installed yet, so this launch uses the game's own DirectX 8.").into());
    }
    match place(&root, &PINNED, dir) {
        Ok(_) => Some(tf("dgVoodoo {version} is set up for this launch.", &[("version", &VERSION)])),
        Err(error) => Some(tf("{error} The game starts without dgVoodoo.", &[("error", &error)])),
    }
}

// ---- download ----

fn trusted_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com" || host.ends_with(".githubusercontent.com")
}

/// Downloads the pinned zip into memory, reporting (received, expected) as it goes.
async fn download(release: &Release, mut progress: impl FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
    let unreachable = || t("Couldn't reach GitHub to download dgVoodoo. Check your internet connection.").to_owned();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .user_agent("CrystalLauncher/0.1")
        .build()
        .map_err(|_| unreachable())?;
    let mut response = client.get(URL).send().await.map_err(|_| unreachable())?;
    if !response.status().is_success() {
        return Err(tf("GitHub answered {status} for the dgVoodoo download.", &[("status", &response.status())]));
    }
    if !response.url().host_str().is_some_and(trusted_host) {
        return Err(t("The dgVoodoo download was redirected away from GitHub, so it was stopped.").into());
    }
    let total = response.content_length();
    let too_large = || t("The dgVoodoo download is larger than expected, so it was stopped.").to_owned();
    if total.is_some_and(|size| size > release.max_zip) {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut last = Instant::now();
    progress(0, total);
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| t("The dgVoodoo download stalled. Check your internet connection and try again.").to_owned())?
            .map_err(|_| unreachable())?;
        let Some(chunk) = chunk else { break };
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > release.max_zip {
            return Err(too_large());
        }
        if last.elapsed() >= Duration::from_millis(120) {
            last = Instant::now();
            progress(bytes.len() as u64, total);
        }
    }
    progress(bytes.len() as u64, total);
    Ok(bytes)
}

/// Download, verify and install, off the UI thread.
async fn download_and_install(root: PathBuf, progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    let zip = download(&PINNED, progress).await?;
    tokio::task::spawn_blocking(move || install_from_zip(&root, &zip, &PINNED, playonline_conf().as_deref()).map(|_| ()))
        .await
        .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
}

/// For a launch with the option on: installs first when needed, reporting progress as launch
/// status lines. Runs on the launch's worker thread.
pub(crate) fn ensure_installed_blocking(mut status: impl FnMut(String)) -> Result<(), String> {
    let root = data_root();
    if installed(&root, &PINNED) {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| t("The dgVoodoo install stopped unexpectedly.").to_owned())?;
    status(t("Downloading dgVoodoo…").into());
    let mut shown = u64::MAX;
    runtime.block_on(download_and_install(root, move |done, total| {
        if let Some(total) = total.filter(|t| *t > 0) {
            let percent = done.saturating_mul(100) / total;
            if percent != shown {
                shown = percent;
                status(tf("Downloading dgVoodoo… {percent}%", &[("percent", &percent)]));
            }
        }
    }))
}

// ---- Settings → Graphics ----

#[derive(Clone, Debug, PartialEq)]
enum Job {
    Idle,
    Running { id: u64, done: u64, total: Option<u64> },
    Failed(String),
}

pub(crate) struct State {
    installed: bool,
    job: Job,
    next_id: u64,
    preview: bool,
}

impl State {
    pub(crate) fn busy(&self) -> bool { matches!(self.job, Job::Running { .. }) }
    /// Reads the cache; with the option on and nothing installed yet, the install starts now.
    pub(crate) fn new(enabled: bool) -> Self {
        let mut state = Self { installed: false, job: Job::Idle, next_id: 1, preview: false };
        if std::env::args().any(|a| a.starts_with("--preview")) {
            state.preview = true;
            match std::env::var("XI_PREVIEW_DGVOODOO").unwrap_or_default().as_str() {
                "installed" => state.installed = true,
                "downloading" => state.job = Job::Running { id: 0, done: 4_170_000, total: Some(9_268_078) },
                "failed" => {
                    state.job = Job::Failed(t("Couldn't reach GitHub to download dgVoodoo. Check your internet connection.").into())
                }
                _ => {}
            }
            return state;
        }
        // Unit tests never read or download into the real data folder.
        if cfg!(test) {
            return state;
        }
        state.installed = installed(&data_root(), &PINNED);
        if enabled && !state.installed {
            state.start();
        }
        state
    }
    fn start(&mut self) {
        if !matches!(self.job, Job::Running { .. }) {
            self.job = Job::Running { id: self.next_id, done: 0, total: None };
            self.next_id += 1;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Toggle(bool),
    Progress(u64, u64, Option<u64>),
    Finished(u64, Result<(), String>),
    OpenSettings,
    Remove,
    Removed(Result<(), String>),
}

fn dg(msg: Msg) -> Message {
    Message::Dgvoodoo(msg)
}

pub(crate) fn update(app: &mut App, msg: Msg) -> Task<Message> {
    if app.reshade.busy && matches!(msg, Msg::Toggle(_) | Msg::Remove) {
        app.status = t("Finish ReShade setup before changing dgVoodoo.").into();
        return Task::none();
    }
    if matches!(msg, Msg::Toggle(false) | Msg::Remove) && app.settings.reshade.enabled {
        if !app.preview && app.has_sessions() {
            app.status = t("Close games and sign out PlayOnline sessions before changing ReShade.").into();
            return Task::none();
        }
        app.settings.reshade.enabled = false;
    }
    let preview = app.dgvoodoo.preview;
    let save = |app: &mut App| {
        if !preview {
            if let Err(error) = crate::save_settings(&app.config, &app.settings) {
                app.status = error;
            }
        }
    };
    match msg {
        Msg::Toggle(on) => {
            app.settings.dgvoodoo = on;
            save(app);
            if preview {
                return Task::none();
            }
            if on {
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
                if !app.dgvoodoo.installed {
                    app.dgvoodoo.start();
                }
            } else {
                // Stops a download in progress (its subscription ends) and takes Crystal's
                // files away from the loader folders; the cache stays for turning it back on.
                app.dgvoodoo.job = Job::Idle;
                let loader = app.settings.loader_path.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || remove_everywhere(&data_root(), &loader))
                            .await
                            .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
                    },
                    |result| dg(Msg::Removed(result)),
                );
            }
        }
        Msg::Progress(id, done, total) => {
            if let Job::Running { id: running, .. } = app.dgvoodoo.job {
                if running == id {
                    app.dgvoodoo.job = Job::Running { id, done, total };
                }
            }
        }
        Msg::Finished(id, result) => {
            if matches!(app.dgvoodoo.job, Job::Running { id: running, .. } if running == id) {
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
                app.dgvoodoo.job = match result {
                    Ok(()) => Job::Idle,
                    Err(error) => Job::Failed(error),
                };
            }
        }
        Msg::OpenSettings => {
            if preview {
                return Task::none();
            }
            let dir = cache_dir(&data_root(), &PINNED);
            let cpl = dir.join(PINNED.cpl.name);
            // Only the pinned settings app runs.
            let started = if has_hash(&cpl, PINNED.cpl.sha256) {
                std::process::Command::new(&cpl).current_dir(&dir).spawn().map(|_| ()).map_err(|e| e.to_string())
            } else {
                Err(t("dgVoodoo is not installed yet.").into())
            };
            if let Err(error) = started {
                app.status = tf("Could not open dgVoodoo's settings: {error}", &[("error", &error)]);
            }
        }
        Msg::Remove => {
            app.settings.dgvoodoo = false;
            save(app);
            app.dgvoodoo.job = Job::Idle;
            if preview {
                app.dgvoodoo.installed = false;
                return Task::none();
            }
            let loader = app.settings.loader_path.clone();
            return Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || uninstall(&data_root(), &loader))
                        .await
                        .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
                },
                |result| dg(Msg::Removed(result)),
            );
        }
        Msg::Removed(result) => {
            if !preview {
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
            }
            if !app.settings.dgvoodoo {
                if let Err(error) = result {
                    app.dgvoodoo.job = Job::Failed(error);
                }
            }
        }
    }
    Task::none()
}

pub(crate) fn subscription(state: &State) -> Subscription<Message> {
    let Job::Running { id, .. } = state.job else { return Subscription::none() };
    if state.preview {
        return Subscription::none();
    }
    Subscription::run_with(id, |id| {
        let id = *id;
        iced::stream::channel(16, async move |mut out: iced::futures::channel::mpsc::Sender<Message>| {
            use iced::futures::SinkExt;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let work = download_and_install(data_root(), move |done, total| {
                let _ = tx.send((done, total));
            });
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    result = &mut work => break result,
                    Some((done, total)) = rx.recv() => { let _ = out.send(dg(Msg::Progress(id, done, total))).await; }
                }
            };
            let _ = out.send(dg(Msg::Finished(id, result))).await;
        })
    })
}

fn rgb(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
/// The Graphics tab's success and error tones.
fn tone(light: bool, ok: bool) -> Color {
    match (ok, light) {
        (true, false) => rgb(0x7CC4A6),
        (true, true) => rgb(0x2F7A5F),
        (false, false) => rgb(0xE88A8A),
        (false, true) => rgb(0xB03E3E),
    }
}

fn chip<'a>(label: String, color: Color, s: f32) -> Element<'a, Message> {
    container(text(label).font(SEMI).size(11.5 * s).color(color))
        .padding([2.0 * s, 9.0 * s])
        .style(move |_| container::Style {
            background: Some(Color { a: 0.13, ..color }.into()),
            border: Border { color: Color { a: 0.32, ..color }, width: 1.0, radius: (10.0 * s).into() },
            ..Default::default()
        })
        .into()
}

fn mb(value: u64) -> String {
    format!("{:.1} MB", value as f64 / 1_000_000.0)
}

/// The dgVoodoo card at the top of Settings → Graphics.
pub(crate) fn view(app: &App) -> Element<'_, Message> {
    let light = app.settings.light_theme;
    let c = colors(light);
    let s = scale(app);
    let state = &app.dgvoodoo;
    let on = app.settings.dgvoodoo;

    let status = match &state.job {
        Job::Running { done, total: Some(total), .. } if *total > 0 => {
            chip(tf("Downloading {percent}%", &[("percent", &(done.saturating_mul(100) / total))]), c.accent, s)
        }
        Job::Running { .. } => chip(t("Downloading…").into(), c.accent, s),
        _ if state.installed => chip(tf("Installed v{version}", &[("version", &VERSION)]), tone(light, true), s),
        _ => chip(t("Not installed").into(), c.sub, s),
    };
    let switch = crate::controller_nav::toggler(on)
        .on_toggle(|v| dg(Msg::Toggle(v)))
        .size(21.0 * s)
        .style(move |_, status| {
            let c = colors(light);
            let (toggled, hovered) = match status {
                toggler::Status::Active { is_toggled } => (is_toggled, false),
                toggler::Status::Hovered { is_toggled } => (is_toggled, true),
                toggler::Status::Disabled { is_toggled } => (is_toggled, false),
            };
            toggler::Style {
                background: if toggled { c.play.into() } else { c.field.into() },
                background_border_width: 1.0,
                background_border_color: if toggled { c.play } else if hovered { c.sub } else { c.line },
                foreground: if toggled { c.play_text.into() } else { c.sub.into() },
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: None,
                border_radius: None,
                padding_ratio: 0.2,
            }
        });
    let head = row![
        column![
            row![text(t("Use dgVoodoo (DirectX 8 → 11 wrapper)")).font(SEMI).size(16.0 * s).color(c.ink), status]
                .spacing(10.0 * s)
                .align_y(Alignment::Center),
            text(t("Draws the game through Direct3D 11, which can fix black screens, stutter and alt-tab problems on newer graphics cards. Crystal downloads it from the official GitHub release, checks it, and sets it up at each launch."))
                .font(BODY)
                .size(12.5 * s)
                .color(c.sub),
        ]
        .spacing(5.0 * s)
        .width(Length::Fill),
        switch,
    ]
    .spacing(20.0 * s)
    .align_y(Alignment::Center);

    let mut body = column![head].spacing(14.0 * s);
    match &state.job {
        Job::Running { done, total, .. } => {
            let fraction = total.filter(|t| *t > 0).map(|t| (*done as f64 / t as f64).clamp(0.0, 1.0) as f32);
            let detail = match total {
                Some(total) => tf("{done} of {total}", &[("done", &mb(*done)), ("total", &mb(*total))]),
                None => tf("{done} received", &[("done", &mb(*done))]),
            };
            body = body.push(
                column![
                    row![
                        text(tf("Downloading dgVoodoo {version}", &[("version", &VERSION)])).font(BODY).size(13.0 * s).color(c.ink).width(Length::Fill),
                        text(detail).font(BODY).size(12.0 * s).color(c.sub)
                    ]
                    .align_y(Alignment::Center),
                    progress_bar(0.0..=1.0, fraction.unwrap_or(0.0)).girth(6.0 * s).style(move |_| progress_bar::Style {
                        background: c.select.into(),
                        bar: c.accent.into(),
                        border: Border { radius: 3.0.into(), ..Default::default() },
                    }),
                ]
                .spacing(8.0 * s),
            );
        }
        Job::Failed(error) => {
            let color = tone(light, false);
            body = body.push(
                row![
                    icon(Icon::AlertCircle, 15.0 * s, color),
                    container(crate::ui::private_text(app, error, 12.5 * s, BODY, c.ink)).width(Length::Fill),
                ]
                .spacing(8.0 * s)
                .align_y(Alignment::Center),
            );
        }
        Job::Idle => {}
    }
    let busy = matches!(state.job, Job::Running { .. });
    let failed_and_on = on && matches!(state.job, Job::Failed(_)) && !state.installed;
    if state.installed || failed_and_on {
        let small = |which: Icon, label: &'static str, msg: Option<Msg>, tint: Color| {
            crate::controller_nav::button(row![icon(which, 14.0 * s, tint), text(label).font(BODY).size(13.0 * s)].spacing(7.0 * s).align_y(Alignment::Center))
                .padding([7.0 * s, 12.0 * s])
                .style(quiet)
                .on_press_maybe(msg.map(dg))
        };
        let mut actions = row![].spacing(6.0 * s).align_y(Alignment::Center);
        if state.installed {
            actions = actions
                .push(small(Icon::Settings, t("Open dgVoodoo settings"), Some(Msg::OpenSettings), c.accent))
                .push(small(Icon::Trash, t("Remove"), (!busy).then_some(Msg::Remove), c.sub));
        } else {
            actions = actions.push(
                crate::controller_nav::button(text(t("Try again")).font(SEMI).size(13.0 * s))
                    .padding([7.0 * s, 16.0 * s])
                    .style(primary)
                    .on_press(dg(Msg::Toggle(true))),
            );
        }
        let note = text(tf("dgVoodoo {version} · settings are kept in Crystal's folder", &[("version", &VERSION)]))
            .font(BODY)
            .size(11.5 * s)
            .color(c.sub);
        body = body.push(divider(c)).push(row![actions, Space::new().width(Length::Fill), note].align_y(Alignment::Center));
    }

    container(
        container(body)
            .padding([18.0 * s, 22.0 * s])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(c.panel.into()),
                text_color: Some(c.ink),
                border: Border { color: c.line, width: 1.0, radius: (9.0 * s).into() },
                ..Default::default()
            }),
    )
    .padding(iced::Padding::ZERO.top(4.0 * s).right(16.0 * s))
    .width(Length::Fill)
    .into()
}

fn divider<'a>(c: Colors) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fill)
        .height(1)
        .style(move |_| container::Style { background: Some(c.line.into()), ..Default::default() })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let base = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("crystal-dgvoodoo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CONF_TEXT: &str = "Version = 0x287\r\n[DirectX]\r\ndgVoodooWatermark                   = true\r\nResolution = unforced\r\n";

    /// A tiny release zip built here: three "DLLs", the settings app, the conf, plus noise.
    fn synthetic() -> (Vec<u8>, Release) {
        let files: [(&str, &[u8]); 7] = [
            ("3Dfx/x86/Glide.dll", b"glide"),
            ("MS/x86/D3D8.dll", b"d3d8 bytes"),
            ("MS/x86/D3DImm.dll", b"d3dimm bytes"),
            ("MS/x86/DDraw.dll", b"ddraw bytes"),
            ("MS/x64/D3D9.dll", b"wrong arch"),
            ("dgVoodooCpl.exe", b"cpl bytes"),
            ("dgVoodoo.conf", CONF_TEXT.as_bytes()),
        ];
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in files {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let pin = |entry: &'static str, name: &'static str, data: &[u8]| Pinned { entry, name, sha256: leak(sha256_hex(data)) };
        let dlls: &'static [Pinned] = Box::leak(Box::new([
            pin("MS/x86/D3D8.dll", "D3D8.dll", b"d3d8 bytes"),
            pin("MS/x86/D3DImm.dll", "D3DImm.dll", b"d3dimm bytes"),
            pin("MS/x86/DDraw.dll", "DDraw.dll", b"ddraw bytes"),
        ]));
        let release = Release {
            version: "9.9.9",
            zip_sha256: leak(sha256_hex(&bytes)),
            max_zip: 1024 * 1024,
            dlls,
            cpl: pin("dgVoodooCpl.exe", "dgVoodooCpl.exe", b"cpl bytes"),
            conf: pin("dgVoodoo.conf", CONF, CONF_TEXT.as_bytes()),
        };
        (bytes, release)
    }

    #[test]
    fn a_zip_that_does_not_match_its_hash_installs_nothing() {
        let root = temp("mismatch");
        let (mut bytes, release) = synthetic();
        *bytes.last_mut().unwrap() ^= 0xff;
        assert!(install_from_zip(&root, &bytes, &release, None).is_err());
        assert!(!root.join("crystal").exists(), "nothing is extracted from an unverified zip");
        // The real pin refuses anything but the real release.
        assert!(install_from_zip(&root, b"not the release", &PINNED, None).is_err());
        assert!(!root.join("crystal").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_that_does_not_match_its_own_pin_installs_nothing() {
        let root = temp("filepin");
        let (bytes, mut release) = synthetic();
        let mut dlls = release.dlls.to_vec();
        dlls[1].sha256 = "00".repeat(32).leak();
        release.dlls = Box::leak(dlls.into_boxed_slice());
        assert!(install_from_zip(&root, &bytes, &release, None).is_err());
        assert!(!root.join("crystal").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_the_needed_files_are_picked_from_the_zip() {
        let (bytes, release) = synthetic();
        let mut wanted = release.dlls.to_vec();
        wanted.push(release.cpl);
        let picked = pick_files(&bytes, &wanted).unwrap();
        let names: Vec<&str> = picked.iter().map(|(p, _)| p.name).collect();
        assert_eq!(names, ["D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodooCpl.exe"]);
        assert_eq!(picked[0].1, b"d3d8 bytes", "the x86 DLL, not another architecture's");
        // Entry names match without case or slash direction.
        let odd = Pinned { entry: r"ms\X86\ddraw.DLL", name: "DDraw.dll", sha256: "" };
        assert_eq!(pick_files(&bytes, &[odd]).unwrap()[0].1, b"ddraw bytes");
        let missing = Pinned { entry: "MS/x86/D3D9.dll", name: "D3D9.dll", sha256: "" };
        assert!(pick_files(&bytes, &[missing]).is_err());

        let root = temp("install");
        let source = install_from_zip(&root, &bytes, &release, None).unwrap();
        assert_eq!(source, ConfSource::Default);
        let dir = cache_dir(&root, &release);
        let mut files: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        assert_eq!(files, ["D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodoo.conf", "dgVoodooCpl.exe"]);
        assert!(installed(&root, &release));
        let conf = fs::read_to_string(dir.join(CONF)).unwrap();
        assert!(conf.contains("dgVoodooWatermark                   = false\r\n"), "{conf}");
        assert!(conf.contains("Resolution = unforced\r\n"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn conf_precedence_crystal_then_playonline_then_default() {
        let root = temp("conf");
        let dir = root.join("crystal").join("dgvoodoo").join("2.0");
        fs::create_dir_all(&dir).unwrap();
        let pol = root.join("PlayOnlineViewer").join(CONF);
        fs::create_dir_all(pol.parent().unwrap()).unwrap();
        fs::write(&pol, "tuned in PlayOnline").unwrap();
        // Nothing else: the release default, watermark off.
        assert_eq!(seed_conf(&dir, None, CONF_TEXT.as_bytes()).unwrap(), ConfSource::Default);
        fs::remove_file(dir.join(CONF)).unwrap();
        // PlayOnline's beats the default.
        assert_eq!(seed_conf(&dir, Some(&pol), CONF_TEXT.as_bytes()).unwrap(), ConfSource::PlayOnline(pol.clone()));
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "tuned in PlayOnline");
        // An existing Crystal conf is never overwritten.
        fs::write(dir.join(CONF), "edited in Crystal").unwrap();
        assert_eq!(seed_conf(&dir, Some(&pol), CONF_TEXT.as_bytes()).unwrap(), ConfSource::Kept);
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "edited in Crystal");
        // A new version starts from the older version's Crystal conf, over PlayOnline's.
        let newer = root.join("crystal").join("dgvoodoo").join("3.0");
        fs::create_dir_all(&newer).unwrap();
        assert_eq!(seed_conf(&newer, Some(&pol), CONF_TEXT.as_bytes()).unwrap(), ConfSource::Crystal(dir.join(CONF)));
        assert_eq!(fs::read_to_string(newer.join(CONF)).unwrap(), "edited in Crystal");
        fs::remove_dir_all(&root).unwrap();
    }

    fn installed_root(name: &str) -> (PathBuf, Release) {
        let root = temp(name);
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        (root, release)
    }

    #[test]
    fn missing_or_changed_settings_app_requires_repair_and_keeps_the_conf() {
        let root = temp("repair-settings");
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        let cache = cache_dir(&root, &release);
        let settings = cache.join(release.cpl.name);
        fs::write(cache.join(CONF), "user settings").unwrap();
        assert!(installed(&root, &release));
        fs::remove_file(&settings).unwrap();
        assert!(!installed(&root, &release));
        install_from_zip(&root, &bytes, &release, None).unwrap();
        assert!(installed(&root, &release));
        fs::write(&settings, b"damaged settings app").unwrap();
        assert!(!installed(&root, &release));
        install_from_zip(&root, &bytes, &release, None).unwrap();
        assert!(installed(&root, &release));
        assert_eq!(fs::read_to_string(cache.join(CONF)).unwrap(), "user settings");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn files_go_next_to_the_standard_and_the_versioned_loader() {
        let (root, release) = installed_root("place");
        let standard = root.join("crystal");
        let versioned = root.join("crystal").join("0123abcd");
        fs::create_dir_all(&versioned).unwrap();
        fs::write(standard.join("pol.exe"), b"loader").unwrap();
        fs::write(versioned.join("pol.exe"), b"loader 2").unwrap();
        for dir in [&standard, &versioned] {
            let placed = place(&root, &release, dir).unwrap();
            assert_eq!(placed.copied, ["D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodoo.conf"]);
            assert!(placed.set_aside.is_empty());
            for pin in release.dlls {
                assert!(has_hash(&dir.join(pin.name), pin.sha256));
            }
            // A second launch copies nothing.
            assert_eq!(place(&root, &release, dir).unwrap(), Placed::default());
        }
        assert_eq!(loader_dirs(&root, ""), vec![standard.clone(), versioned.clone()]);
        // An edited conf in the cache reaches the loader at the next launch.
        fs::write(cache_dir(&root, &release).join(CONF), "edited").unwrap();
        assert_eq!(place(&root, &release, &standard).unwrap().copied, ["dgVoodoo.conf"]);
        assert_eq!(fs::read_to_string(standard.join(CONF)).unwrap(), "edited");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_foreign_file_next_to_the_loader_is_set_aside_not_deleted() {
        let (root, release) = installed_root("foreign");
        let dir = root.join("custom");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("D3D8.dll"), b"someone else's d3d8").unwrap();
        let placed = place(&root, &release, &dir).unwrap();
        assert_eq!(placed.set_aside, ["D3D8.dll"]);
        assert_eq!(fs::read(dir.join("D3D8.dll.crystal-backup")).unwrap(), b"someone else's d3d8");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removal_leaves_files_that_are_not_crystals_alone() {
        let (root, release) = installed_root("remove");
        let dir = root.join("crystal");
        place(&root, &release, &dir).unwrap();
        // The user swaps in their own DDraw.dll and edits the conf by hand next to the loader.
        fs::write(dir.join("DDraw.dll"), b"user ddraw").unwrap();
        fs::write(dir.join(CONF), "hand edited").unwrap();
        fs::write(dir.join("notes.txt"), "mine").unwrap();
        let mut removed = remove_from(&release, &dir).unwrap();
        removed.sort();
        assert_eq!(removed, ["D3D8.dll", "D3DImm.dll"]);
        assert_eq!(fs::read(dir.join("DDraw.dll")).unwrap(), b"user ddraw");
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "hand edited");
        assert!(dir.join("notes.txt").is_file());
        assert!(!dir.join(MANIFEST).exists());
        // Crystal's untouched conf is removed with the DLLs.
        let other = root.join("other");
        fs::create_dir_all(&other).unwrap();
        place(&root, &release, &other).unwrap();
        let mut removed = remove_from(&release, &other).unwrap();
        removed.sort();
        assert_eq!(removed, ["D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodoo.conf"]);
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn locked_files_keep_the_ownership_needed_to_retry_removal() {
        use std::os::windows::fs::OpenOptionsExt;
        // Both a delete-only sharing violation and a file we cannot hash must
        // leave enough ownership information for a later release to retry.
        for sharing in [0, 3] {
            let (root, release) = installed_root(&format!("locked-removal-{sharing}"));
            let dir = root.join("crystal");
            place(&root, &release, &dir).unwrap();
            let manifest = fs::read(dir.join(MANIFEST)).unwrap();
            let locked_dll = fs::OpenOptions::new().read(true).share_mode(sharing)
                .open(dir.join("D3D8.dll")).unwrap();
            let locked_conf = fs::OpenOptions::new().read(true).share_mode(sharing)
                .open(dir.join(CONF)).unwrap();
            let error = remove_from(&release, &dir).unwrap_err();
            assert!(error.contains("D3D8.dll") && error.contains(CONF), "{error}");
            assert_eq!(fs::read(dir.join(MANIFEST)).unwrap(), manifest);
            drop(locked_dll);
            drop(locked_conf);

            // The current release no longer pins that old DLL. Its saved
            // manifest is the only proof that lets the retry remove it.
            let mut newer = release;
            let mut dlls = newer.dlls.to_vec();
            dlls[0].sha256 = Box::leak("00".repeat(32).into_boxed_str());
            newer.dlls = Box::leak(dlls.into_boxed_slice());
            let mut removed = remove_from(&newer, &dir).unwrap();
            removed.sort();
            assert_eq!(removed, ["D3D8.dll", "dgVoodoo.conf"]);
            assert!(!dir.join(MANIFEST).exists());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn watermark_line_keeps_its_spacing_and_line_ending() {
        assert_eq!(watermark_off("a = 1\ndgVoodooWatermark   = true\n3DfxWatermark = true"), "a = 1\ndgVoodooWatermark   = false\n3DfxWatermark = true");
    }

    #[test]
    fn settings_without_the_field_turn_dgvoodoo_on() {
        let old: crate::Settings = serde_json::from_str(r#"{"install_path":"C:\\x"}"#).unwrap();
        assert!(old.dgvoodoo, "on by default, also for older settings files");
        assert!(crate::Settings::default().dgvoodoo);
        let off: crate::Settings = serde_json::from_str(r#"{"install_path":"C:\\x","dgvoodoo":false}"#).unwrap();
        assert!(!off.dgvoodoo);
        let saved = serde_json::to_string(&off).unwrap();
        assert!(!serde_json::from_str::<crate::Settings>(&saved).unwrap().dgvoodoo);
        // A launch plan serialised by an older launcher (no field) still reads, as off.
        let tools: crate::pol_less::ToolSettings =
            serde_json::from_str(r#"{"ashita":"","windower":"","boot_ini":"","windower_profile":"","loader":""}"#).unwrap();
        assert!(!tools.dgvoodoo);
    }

    /// The real pinned release, offline: `DGVOODOO_ZIP=<path to dgVoodoo2_87_5.zip>`.
    #[test]
    #[ignore]
    fn the_real_release_installs_with_every_pin() {
        let Some(path) = std::env::var_os("DGVOODOO_ZIP") else { return };
        let root = temp("real");
        let source = install_from_zip(&root, &fs::read(path).unwrap(), &PINNED, None).unwrap();
        assert_eq!(source, ConfSource::Default);
        assert!(installed(&root, &PINNED));
        assert!(has_hash(&cache_dir(&root, &PINNED).join(PINNED.cpl.name), PINNED.cpl.sha256));
        let conf = fs::read_to_string(cache_dir(&root, &PINNED).join(CONF)).unwrap();
        assert!(conf.lines().any(|l| l.starts_with("dgVoodooWatermark") && l.trim_end().ends_with("= false")));
        let loader = root.join("crystal");
        assert_eq!(place(&root, &PINNED, &loader).unwrap().copied.len(), 4);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_pin_is_the_github_release() {
        assert!(URL.starts_with("https://github.com/dege-diosg/dgVoodoo2/releases/download/v2.87.5/"));
        assert_eq!(PINNED.zip_sha256.len(), 64);
        assert!(trusted_host("github.com") && trusted_host("release-assets.githubusercontent.com"));
        assert!(!trusted_host("github.com.example.net"));
    }
}
