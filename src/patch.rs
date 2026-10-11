//! Retail version checks, shaped like the stock PlayOnline Viewer's.
//! Packet framing is implemented by updater::polp.
//! These query Square Enix directly; they never apply a patch or use credentials.
//!
//! Stock sends exactly two version requests (one POLP frame each, to
//! `pc001.pol.com:53000 + product id`, reply read, connection closed):
//! * the Viewer's own check (`W2U`, product `1000`, port 54000, the Viewer's installed
//!   version) once when the Viewer starts, before any sign-in traffic;
//! * the game's check (`W20`, product `0001`, port 53001, FFXI's installed version) when
//!   Play is pressed, after authentication and before the game starts. Crystal overlaps
//!   this stateless request with native startup mail/presence; it still waits for full
//!   native idle and consumes the fresh verdict before releasing the game.
//! FFXI is not queried at launcher start, on refresh, on a timer, or after a
//! settings save. The Viewer verdict is shared by startup and sign-in checks;
//! explicit Viewer checks and official updates refresh it. Empty versions are
//! sent only for a fresh install in these pre-launch checks.

use crate::i18n::{t, tf};
use crate::updater::polp;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

const SOURCE: &str = "tcp://pc001.pol.com:53001";
const HOST: &str = "pc001.pol.com";
const HEADER: usize = 16;
const MAX_VERSION_FRAME: usize = 4096;
/// How long a launch waits for the patch server before starting anyway (unreachable
/// server = unverified, exactly as before; it is never a reason to refuse a launch).
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Leads the message that stops a start because the installed version is not the server's.
pub(crate) const UPDATE_REQUIRED: &str = "Update required:";
/// `gate`'s update-required sentence (its `tf` key; a test keeps the two equal).
const UPDATE_REQUIRED_TEMPLATE: &str = "Update required: installed {local}, official server {remote}. Open Downloads, select the game folder, then choose Update (or Repair) before starting FINAL FANTASY XI.";

/// One product the patch server knows: platform tag and product id (the port is
/// `53000 + product id`).
#[derive(Clone, Copy)]
struct Product {
    platform: [u8; 4],
    title: [u8; 4],
}
const FFXI: Product = Product { platform: *b"W20\0", title: *b"0001" };
const VIEWER: Product = Product { platform: *b"W2U\0", title: *b"1000" };

impl Product {
    fn title(&self) -> &str {
        std::str::from_utf8(&self.title).unwrap_or("0000")
    }
    fn server(&self) -> String {
        format!("{HOST}:{}", 53000 + self.title().parse::<u16>().unwrap_or(0))
    }
    /// The stock 88-byte version frame carrying `installed` (empty only for a fresh install).
    fn request(&self, installed: &str) -> Vec<u8> {
        polp::version(&polp::Tags { platform: self.platform, title: self.title }, installed.as_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchState {
    Current,
    Required,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PatchReport {
    pub state: PatchState,
    pub local_version: String,
    pub remote_version: String,
    pub detail: String,
    /// None for version-only checks; an actual file operation reports its
    /// observed result without changing the installed-version launch gate.
    pub files_verified: Option<bool>,
    pub source_url: Option<String>,
}

/// What is installed, as the stock client reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Local {
    Installed(String),
    /// No patch.ver at all: a fresh install, the one case stock sends an empty version for.
    Fresh,
    Unreadable(String),
    /// Staged/mixed files cannot be vouched for by the previous version record.
    Updating(String),
}

fn read_local(install: &Path, title: &str) -> Local {
    if let Err(error) = check_recovery_state(install) { return Local::Updating(error); }
    match std::fs::metadata(install.join("patch.ver")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Local::Fresh,
        _ => match local_version_for(install, title) {
            Ok(version) => Local::Installed(version),
            Err(error) => Local::Unreadable(error),
        },
    }
}

fn check_recovery_state(install: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(install.join(".xi-updater/transaction.json")) {
        Ok(_) => Err(t("An interrupted update must be recovered before starting. Open Downloads and choose Update or Repair.").into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(tf("Cannot check updater recovery state: {error}", &[("error", &error)])),
    }
}

/// Keep an existing updater lock read-shared while querying the version. This
/// refuses another launcher's active update before reading an older patch.ver.
/// The updater's own version request deliberately uses read_local directly.
pub(crate) fn check_update_state(install: &Path) -> Result<Option<std::fs::File>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    }
    let lock = match options.open(install.join(".xi-updater/lock")) {
        Ok(lock) => Some(lock),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(t("The installation is being updated or its updater lock is unavailable. Wait for the update to finish before starting.").into()),
    };
    check_recovery_state(install)?;
    Ok(lock)
}

fn read_local_for_launch(install: &Path, title: &str) -> (Local, Option<std::fs::File>) {
    match check_update_state(install) {
        Ok(lock) => (read_local(install, title), lock),
        Err(error) => (Local::Updating(error), None),
    }
}

/// The version text the updater puts in its own version request: the installed version,
/// empty for a fresh install. A patch.ver that cannot be decoded (the case Repair exists
/// for) also sends empty, since there is no installed version to report.
pub(crate) fn installed_for_request(install: &Path) -> String {
    installed_for_request_for(install, "0001")
}

pub(crate) fn installed_for_request_for(install: &Path, title: &str) -> String {
    match read_local(install, title) {
        Local::Installed(version) => version,
        Local::Fresh | Local::Unreadable(_) | Local::Updating(_) => String::new(),
    }
}

/// Reads the applied-version record only. No network: the server is asked when Play
/// is pressed (`check_before_launch`) and by Check files / Update / Repair.
pub fn local_report(install: &Path) -> PatchReport {
    let local = read_local(install, FFXI.title());
    let installed = matches!(local, Local::Installed(_));
    let mut result = game_report(local, Err(t("Not checked").into()));
    result.remote_version = t("Not checked").into();
    if installed {
        result.detail = t("The official patch server is asked when you press Play, before FINAL FANTASY XI starts.").into();
    }
    result
}

/// The installed record against a version the official server just reported (the updater's).
pub fn report_with_remote(install: &Path, remote: &str) -> PatchReport {
    game_report(read_local(install, FFXI.title()), Ok(remote.to_owned()))
}

fn game_report(local: Local, remote: Result<String, String>) -> PatchReport {
    match local {
        Local::Installed(version) => report(Ok(version), remote),
        Local::Fresh => PatchReport {
            state: PatchState::Required,
            local_version: t("Not initialized").into(),
            remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()),
            detail: t("FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.").into(),
            files_verified: None,
            source_url: Some(SOURCE.into()),
        },
        Local::Unreadable(error) => PatchReport {
            state: PatchState::Required,
            local_version: t("Unavailable").into(),
            remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()),
            detail: tf("The installed-version record cannot be read: {error}. Choose Repair modified files to verify the installation and rebuild it with backups.", &[("error", &error)]),
            files_verified: None,
            source_url: Some(SOURCE.into()),
        },
        Local::Updating(error) => PatchReport {
            state: PatchState::Required,
            local_version: t("Unavailable").into(),
            remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()),
            detail: error,
            files_verified: None,
            source_url: Some(SOURCE.into()),
        },
    }
}

/// Keep a valid version record separate from the outcome of an actual file
/// check. A matching patch.ver alone cannot make missing/outdated files ready.
pub(crate) fn report_after_operation(install: &Path, files: &crate::updater::Report, mode: crate::updater::Mode) -> PatchReport {
    let mut result = report_with_remote(install, &files.version);
    result.files_verified = Some(files.verified);
    if mode == crate::updater::Mode::Check && files.outdated + files.missing > 0 {
        result.state = PatchState::Required;
        result.detail = tf("{outdated} outdated and {missing} missing FFXI files. Choose Update.",
            &[("outdated", &files.outdated), ("missing", &files.missing)]);
    } else if !files.verified && files.modified > 0 {
        if result.state == PatchState::Current { result.state = PatchState::Unknown; }
        result.detail = tf("{modified} unrecognized FFXI files were preserved. Repair modified files can restore official files with backups; the installed version is advanced only after all files verify.",
            &[("modified", &files.modified)]);
    }
    result
}

/// What the stock FFXI check decides before the game starts.
pub enum LaunchCheck {
    /// Installed version equals the server's.
    Start(PatchReport),
    /// The server's version differs from the installed one: do not start; update first.
    Update(PatchReport),
    /// Could not compare (server unreachable, reply unusable, installed version unreadable).
    /// The launch proceeds, as it did when the patch server was unreachable before.
    Unverified(PatchReport),
}

/// Sends the stock FFXI check (`W20`/`0001`, FFXI's installed version, port 53001) and
/// reads the reply. Blocking, at most `CHECK_TIMEOUT`.
pub fn check_before_launch(install: &Path) -> LaunchCheck {
    check_with(install, |request| exchange(&FFXI.server(), request, CHECK_TIMEOUT))
}

fn check_with(install: &Path, send: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>) -> LaunchCheck {
    let (local, _update_lock) = read_local_for_launch(install, FFXI.title());
    let remote = match &local {
        Local::Installed(version) => send(&FFXI.request(version)).and_then(|frame| parse_reply(&frame)),
        Local::Fresh => send(&FFXI.request("")).and_then(|frame| parse_reply(&frame)),
        // Never fall back to an anonymous request for a record that cannot be read.
        Local::Unreadable(error) | Local::Updating(error) => Err(tf("No version request sent: {error}", &[("error", error)])),
    };
    let verdict = match (&local, &remote) {
        (Local::Installed(l), Ok(r)) if l == r => Verdict::Start,
        (Local::Installed(_), Ok(_)) => Verdict::Update,
        (Local::Fresh | Local::Unreadable(_) | Local::Updating(_), _) => Verdict::Update,
        _ => Verdict::Unverified,
    };
    let report = game_report(local, remote);
    match verdict {
        Verdict::Start => LaunchCheck::Start(report),
        Verdict::Update => LaunchCheck::Update(report),
        Verdict::Unverified => LaunchCheck::Unverified(report),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Start,
    Update,
    Unverified,
}

/// Turns a check into the gate every game start passes: `Ok(note)` to start (the note is
/// for the status line and log), `Err(message)` to stop and send the user to Update.
pub fn gate(check: LaunchCheck) -> Result<String, String> {
    match check {
        LaunchCheck::Start(report) => Ok(tf(
            "Game version {version} matches the official server.",
            &[("version", &report.local_version)],
        )),
        // Starts with `UPDATE_REQUIRED`; `parse_update_required` reads it back.
        LaunchCheck::Update(report) => Err(tf(
            "Update required: installed {local}, official server {remote}. Open Downloads, select the game folder, then choose Update (or Repair) before starting FINAL FANTASY XI.",
            &[("local", &report.local_version), ("remote", &report.remote_version)],
        )),
        LaunchCheck::Unverified(report) => Ok(tf(
            "Game version could not be compared with the official server ({reason}). Starting anyway.",
            &[("reason", &report.detail.trim_end_matches('.'))],
        )),
    }
}

/// The check and the gate in one, for a start that has no use for the report.
pub fn gate_before_start(install: &Path) -> Result<String, String> {
    gate(check_before_launch(install))
}

/// The report the interface shows for `gate`'s outcome (`Err` is an update-required message).
pub fn parse_update_required(message: &str) -> Option<PatchReport> {
    // `gate` words the message in the language of the process that ran the
    // check (the window or a session worker started before a language change),
    // so every language's wording is tried, not only the one showing now.
    let (local, remote) = parse_english_update_required(message).or_else(|| {
        crate::i18n::Lang::ALL
            .into_iter()
            .find_map(|lang| parse_update_required_with(message, crate::i18n::t_in(lang, UPDATE_REQUIRED_TEMPLATE)))
    })?;
    Some(PatchReport {
        state: PatchState::Required,
        local_version: local,
        remote_version: remote,
        detail: t("A newer official version is available.").into(),
        files_verified: None,
        source_url: Some(SOURCE.into()),
    })
}

fn parse_english_update_required(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix(UPDATE_REQUIRED)?.trim_start().strip_prefix("installed ")?;
    let (local, rest) = rest.split_once(", official server ")?;
    let (remote, _) = rest.split_once('.')?;
    // `remote` ends at the first '.', and versions contain none.
    Some((local.into(), remote.into()))
}

/// Reads the two versions out of a message built from `template` (its `{local}`
/// and `{remote}` placeholders), so a translated message parses as well. The
/// remote version ends at the first character the template puts after it.
/// The placeholders may come in either order.
fn parse_update_required_with(message: &str, template: &str) -> Option<(String, String)> {
    let (local_at, remote_at) = (template.find("{local}")?, template.find("{remote}")?);
    let (first, second) = if local_at < remote_at { ("{local}", "{remote}") } else { ("{remote}", "{local}") };
    let (head, rest) = template.split_once(first)?;
    let (middle, tail) = rest.split_once(second)?;
    let (one, after) = message.strip_prefix(head)?.split_once(middle)?;
    let two = match tail.chars().next() {
        Some(end) => after.split_once(end)?.0,
        None => after,
    };
    let (local, remote) = if local_at < remote_at { (one, two) } else { (two, one) };
    (!local.is_empty() && !remote.is_empty()).then(|| (local.to_owned(), remote.to_owned()))
}

/// The Viewer's separate product/version check. The caller caches its verdict
/// for the launcher session and explicitly refreshes it after official updates.
pub(crate) fn viewer_check(dir: &Path) -> PatchReport {
    viewer_check_with(dir, |request| exchange(&VIEWER.server(), request, CHECK_TIMEOUT))
}

/// `W2U`/`1000` with the Viewer's own installed version and record key.
fn viewer_check_with(
    dir: &Path,
    send: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> PatchReport {
    let (local, _update_lock) = read_local_for_launch(dir, VIEWER.title());
    let remote = match &local {
        Local::Installed(version) => send(&VIEWER.request(version)).and_then(|f| parse_reply_for(&f, true)),
        Local::Fresh => send(&VIEWER.request("")).and_then(|f| parse_reply_for(&f, true)),
        Local::Unreadable(error) | Local::Updating(error) => Err(error.clone()),
    };
    viewer_report(local, remote)
}

/// Reuse the shared updater's validated version reply, including after a
/// partial/cancelled transaction, without starting a second network request.
pub(crate) fn viewer_report_with_remote(dir: &Path, remote: Option<&str>) -> PatchReport {
    viewer_report(read_local(dir, VIEWER.title()), remote
        .filter(|version| valid_viewer_version(version)).map(|version| Ok(version.to_owned()))
        .unwrap_or_else(|| Err(t("Not checked").into())))
}

fn viewer_report(local: Local, remote: Result<String, String>) -> PatchReport {
    let state = match (&local, &remote) {
        (Local::Updating(_), _) => PatchState::Required,
        (Local::Installed(l), Ok(r)) if l == r => PatchState::Current,
        (Local::Installed(_) | Local::Fresh, Ok(_)) => PatchState::Required,
        _ => PatchState::Unknown,
    };
    let detail = match (&local, &remote, state) {
        (Local::Updating(error), _, _) => error.clone(),
        (_, _, PatchState::Current) => t("PlayOnline Viewer matches the official version.").into(),
        (Local::Fresh, _, PatchState::Required) => t("PlayOnline has not completed its initial update. Open Downloads and choose Update.").into(),
        (_, _, PatchState::Required) => t("A PlayOnline Viewer update is required before a new sign-in. Choose Update.").into(),
        (_, Err(error), _) => tf("Could not verify PlayOnline: {error}", &[("error", error)]),
        _ => t("PlayOnline version has not been verified.").into(),
    };
    PatchReport {
        state,
        local_version: match local {
            Local::Installed(v) => v,
            Local::Fresh => t("Not initialized").into(),
            Local::Unreadable(_) | Local::Updating(_) => t("Unavailable").into(),
        },
        remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()),
        detail,
        files_verified: None,
        source_url: Some(format!("tcp://{}", VIEWER.server())),
    }
}

/// One request, one reply, then close. The deadline covers connect, send and receive.
fn exchange(server: &str, request: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + limit;
    let left = || deadline.saturating_duration_since(Instant::now());
    let mut last = String::from(t("no address"));
    let mut stream = None;
    for address in server
        .to_socket_addrs()
        .map_err(|e| tf("Cannot find the official patch server: {error}", &[("error", &e)]))?
    {
        if left().is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, left()) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let mut stream = stream.ok_or_else(|| tf("Cannot connect to official patch server: {error}", &[("error", &last)]))?;
    stream
        .set_write_timeout(Some(left().max(Duration::from_millis(1))))
        .and_then(|_| stream.write_all(request))
        .map_err(|e| tf("Cannot send patch version request: {error}", &[("error", &e)]))?;
    let mut header = [0u8; HEADER];
    read_exact_by(&mut stream, &mut header, deadline)
        .map_err(|e| tf("Cannot read patch version reply: {error}", &[("error", &e)]))?;
    let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if !(HEADER..=MAX_VERSION_FRAME).contains(&len) {
        return Err(t("Official server returned an invalid version frame length").into());
    }
    let mut frame = vec![0; len];
    frame[..HEADER].copy_from_slice(&header);
    read_exact_by(&mut stream, &mut frame[HEADER..], deadline)
        .map_err(|e| tf("Incomplete patch version reply: {error}", &[("error", &e)]))?;
    Ok(frame)
}

fn read_exact_by(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(buffer) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buffer = &mut buffer[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(std::io::ErrorKind::TimedOut.into())
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Locate the installed product on this PC. Historical callers use this name;
/// registry entries take priority, with validated local evidence as fallback.
pub(crate) fn registry_install_folder(title: &str) -> Option<std::path::PathBuf> {
    let product = match title {
        "0001" => crate::discovery::Product::Ffxi,
        "1000" => crate::discovery::Product::PlayOnline,
        _ => return None,
    };
    crate::discovery::find(product, None)
}

/// Keep a valid selected installation, otherwise discover it on this PC.
/// Markers identify the game folder even when its first update has not run or
/// a binary is missing and needs repair. The version record is checked separately.
pub(crate) fn find_game_installation(preferred: Option<&Path>) -> Option<std::path::PathBuf> {
    if preferred.is_none() { crate::discovery::invalidate(); }
    crate::discovery::find(crate::discovery::Product::Ffxi, preferred)
}

#[cfg(test)]
fn find_game_installation_from(preferred: Option<&Path>, registered: Option<&Path>) -> Option<std::path::PathBuf> {
    [preferred, registered].into_iter().flatten()
        .find_map(|path| crate::discovery::validated_folder(crate::discovery::Product::Ffxi, path))
}

fn report(local: Result<String, String>, remote: Result<String, String>) -> PatchReport {
    let mut result = PatchReport {
        state: PatchState::Unknown,
        local_version: local
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| t("Unavailable").into()),
        remote_version: remote
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| t("Unavailable").into()),
        detail: String::new(),
        files_verified: None,
        source_url: Some(SOURCE.into()),
    };
    result.detail = match (local, remote) {
        (Ok(local), Ok(remote)) if local == remote => {
            result.state = PatchState::Current;
            t("FFXI's installed-version record matches the official patch server. Check files verifies file integrity.").into()
        }
        (Ok(local), Ok(remote)) => {
            match (stamp_key(&local), stamp_key(&remote)) {
                (Some(a), Some(b)) if a < b => {
                    result.state = PatchState::Required;
                    t("A newer official version is available. Open Downloads, select the game folder, then choose Update.").into()
                }
                _ => t("Local metadata is newer than the official server response; update status cannot be established.").into(),
            }
        }
        (Err(local), Ok(_)) => local,
        (Ok(_), Err(remote)) => remote,
        (Err(local), Err(remote)) => tf("{local}. {remote}", &[("local", &local), ("remote", &remote)]),
    };
    result
}

fn stamp_key(stamp: &str) -> Option<(u32, u32)> {
    let (date, revision) = stamp.split_once('_')?;
    if date.len() != 8
        || !date.starts_with('3')
        || !date.bytes().all(|b| b.is_ascii_digit())
        || revision.is_empty()
        || !revision.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((date.parse().ok()?, revision.parse().ok()?))
}


/// The applied version in `install\patch.ver`, decoded with the key for product `title`
/// (`0001` FFXI, `1000` the PlayOnline Viewer; the registry holds one key per product).
fn local_version_for(install: &Path, title: &str) -> Result<String, String> {
    let path = install.join("patch.ver");
    let file = std::fs::File::open(&path)
        .map_err(|e| tf("Cannot read applied-version record {path}: {error}", &[("path", &path.display()), ("error", &e)]))?;
    let mut bytes = Vec::new();
    file.take(289)
        .read_to_end(&mut bytes)
        .map_err(|e| tf("Cannot read patch.ver: {error}", &[("error", &e)]))?;
    if bytes.len() != 288 {
        return Err(t("Applied-version record patch.ver has an unsupported length").into());
    }
    // The title-specific Interface value is a patch-record codec seed, not an
    // account credential. The native decoder also tries the no-registry seed.
    for seed in patch_seeds_for(title) {
        let decoded = if title == FFXI.title() {
            decode_patch_record(&bytes, seed)
        } else {
            decode_record_for(&bytes, seed, title)
        };
        if let Ok(version) = decoded {
            return Ok(version);
        }
    }
    Err(t("Cannot validate/decode applied-version record patch.ver with the installed patch keys. Downloaded patch.cfg does not prove an update was applied.").into())
}

/// Preserve the key that validates the installed applied-version record.
/// A missing record uses the installed title key (or POL's seed-1 fallback).
#[cfg(test)]
pub(crate) fn applied_seed(install: &Path) -> Result<u64, String> {
    applied_seed_for(install, "0001")
}

pub(crate) fn applied_seed_for(install: &Path, title: &str) -> Result<u64, String> {
    let path = install.join("patch.ver");
    match std::fs::read(&path) {
        Ok(bytes) => patch_seeds_for(title).into_iter()
            .find(|seed| decode_record_for(&bytes, *seed, title).is_ok())
            .ok_or_else(|| t("Existing patch.ver cannot be validated; refusing to replace its applied-version record").into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(patch_seeds_for(title)[0]),
        Err(e) => Err(tf("Cannot read {path}: {error}", &[("path", &path.display()), ("error", &e)])),
    }
}

/// Explicit Repair may replace an invalid applied record after every official
/// file verifies. Prefer the installed title key, retaining the old record as
/// a transaction backup rather than treating it as a successfully applied version.
#[cfg(test)]
pub(crate) fn repair_seed(install: &Path) -> Result<u64, String> {
    repair_seed_for(install, "0001")
}

pub(crate) fn repair_seed_for(install: &Path, title: &str) -> Result<u64, String> {
    match applied_seed_for(install, title) {
        Ok(seed) => Ok(seed),
        Err(_) => {
            // Still report read/access errors; only codec-invalid regular files
            // are eligible for explicit metadata repair.
            let bytes = std::fs::read(install.join("patch.ver")).map_err(|e| e.to_string())?;
            if patch_seeds_for(title)
                .iter()
                .any(|s| decode_record_for(&bytes, *s, title).is_ok())
            {
                return Err(t("Applied record unexpectedly changed during repair preparation").into());
            }
            Ok(patch_seeds_for(title)[0])
        }
    }
}

/// Inverse of the validated retail codec below; used only after all files verify.
#[cfg(test)]
pub(crate) fn encode_applied_record(version: &str, seed: u64) -> Result<Vec<u8>, String> {
    encode_applied_record_for(version, seed, "0001")
}

pub(crate) fn encode_applied_record_for(version: &str, seed: u64, title: &str) -> Result<Vec<u8>, String> {
    if !valid_product_version(version, title) { return Err(t("Unsupported applied version").into()); }
    encode_record(version, seed)
}

fn decode_record_for(bytes: &[u8], seed: u64, title: &str) -> Result<String, String> {
    let version = decode_patch_text(bytes, seed)?;
    if !valid_product_version(&version, title) { return Err(t("Unsupported applied version").into()); }
    Ok(version)
}

fn valid_product_version(version: &str, title: &str) -> bool {
    match title {
        "0001" => stamp_key(version).is_some(),
        "1000" => valid_viewer_version(version),
        _ => false,
    }
}

fn encode_record(version: &str, seed: u64) -> Result<Vec<u8>, String> {
    if version.len() >= 256 {
        return Err(t("Applied version is too long").into());
    }
    let key = seed.to_le_bytes();
    let mut first = [0u8; 8];
    first[..4].copy_from_slice(
        &u32::from_le_bytes(key[4..].try_into().unwrap())
            .rotate_left(16)
            .to_le_bytes(),
    );
    first[4..].copy_from_slice(
        &u32::from_le_bytes(key[..4].try_into().unwrap())
            .rotate_left(8)
            .to_le_bytes(),
    );
    first[0] = first[0].wrapping_add(0x45);
    for i in 1..8 {
        first[i] = first[i].wrapping_add(first[i - 1]).wrapping_sub(0x2c)
            ^ first[i - 1].wrapping_shl(2)
            ^ 0x45;
    }
    let mut schedule = [0u64; 32];
    schedule[0] = u64::from_le_bytes(first);
    for i in 1..32 {
        schedule[i] = schedule[i - 1].wrapping_mul(5);
    }
    let mut plain = [0u8; 288];
    plain[24..24 + version.len()].copy_from_slice(version.as_bytes());
    plain[280..284].copy_from_slice(&280u32.to_le_bytes());
    let checksum = (0..35).fold(0u32, |s, i| {
        s.wrapping_add(u32::from(plain[i * 8]))
            .wrapping_add(u32::from(plain[i * 8 + 4]))
    });
    plain[284..].copy_from_slice(&checksum.to_le_bytes());
    let mut cipher = vec![0u8; 288];
    for i in 0..36 {
        let word = schedule[i % 32];
        let key = word.to_le_bytes();
        let counter = (i * 8) as u64;
        let mut expanded = counter;
        for _ in 0..3 {
            expanded = (expanded << 10) | counter;
        }
        let value =
            u64::from_le_bytes(plain[i * 8..i * 8 + 8].try_into().unwrap()).rotate_right(32);
        let block =
            ((value ^ word ^ expanded.wrapping_add(0xa1652347)).wrapping_add(word)).to_le_bytes();
        let mut chain = (i as u8) ^ 0x45;
        for j in 0..8 {
            let mut v = block[j];
            for k in key {
                v = v.wrapping_sub(0x78).wrapping_add(k);
            }
            cipher[i * 8 + j] = v ^ chain;
            chain = cipher[i * 8 + j];
        }
    }
    if decode_patch_text(&cipher, seed)? != version {
        return Err(t("Applied-version encoder validation failed").into());
    }
    Ok(cipher)
}


/// Candidate keys for the record of product `title`, as polcore builds them (slot 1171:
/// 1004a170 / 1004a090 / 10049f60): the registry string `Interface\<title>` folded into
/// eight bytes by `key[i % 8] += c`, plus the product number (1 for FFXI, 1000 for the
/// Viewer); with no usable string, the product number alone.
#[cfg(windows)]
fn patch_seeds_for(title: &str) -> Vec<u64> {
    let product = title.parse::<u64>().unwrap_or(0);
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut seeds = Vec::new();
    for region in ["PlayOnlineUS", "PlayOnlineEU", "PlayOnline"] {
        let path: Vec<u16> = format!(r"SOFTWARE\WOW6432Node\{region}\Interface")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let name: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let mut value = [0u16; 128];
        let mut len = std::mem::size_of_val(&value) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                path.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if status != 0 {
            continue;
        }
        let value = &value[..(len as usize / 2).min(value.len())];
        let end = value.iter().position(|c| *c == 0).unwrap_or(value.len());
        // The installed viewer reads an ANSI key; published patch keys are ASCII.
        // Unsupported encodings remain Unknown instead of inventing a seed.
        if value[..end].iter().any(|c| *c > 127) {
            continue;
        }
        let mut key = [0u8; 8];
        for (i, c) in value[..end].iter().enumerate() {
            key[i % 8] = key[i % 8].wrapping_add(*c as u8);
        }
        seeds.push(u64::from_le_bytes(key).wrapping_add(product));
    }
    seeds.push(product);
    seeds
}
#[cfg(not(windows))]
fn patch_seeds_for(title: &str) -> Vec<u64> {
    vec![title.parse::<u64>().unwrap_or(0)]
}

/// Applied-version codec traced statically from installed polcore.dll
/// SHA256 73b1864bf522d3aa6ec28e9f4bb226fcee956f7f4848a238c9c2d756dc703b06:
/// table slot +0x124c -> 1004a0e0, 1004a170, 1004a220, 100081e0;
/// key schedule 10007bc0; inverse byte/mix stages 100080c0/10008000;
/// trailer validation 10008130. app.dll 1029da51 supplies title index 1;
/// its completion path 102a4cbb writes this record after patching.
/// No original DLL is loaded, and no account/profile/session data is read.
fn decode_patch_record(cipher: &[u8], seed: u64) -> Result<String, String> {
    let version = decode_patch_text(cipher, seed)?;
    stamp_key(&version).ok_or("patch.ver has an unsupported applied version")?;
    Ok(version)
}

/// The codec without FFXI's version-stamp rule: the Viewer's own record holds a different
/// style of version (`20110829_E`).
fn decode_patch_text(cipher: &[u8], seed: u64) -> Result<String, String> {
    if cipher.len() != 288 {
        return Err("unsupported patch.ver length".into());
    }
    let key = seed.to_le_bytes();
    let mut first = [0u8; 8];
    first[..4].copy_from_slice(
        &u32::from_le_bytes(key[4..].try_into().unwrap())
            .rotate_left(16)
            .to_le_bytes(),
    );
    first[4..].copy_from_slice(
        &u32::from_le_bytes(key[..4].try_into().unwrap())
            .rotate_left(8)
            .to_le_bytes(),
    );
    first[0] = first[0].wrapping_add(0x45);
    for i in 1..8 {
        first[i] = first[i].wrapping_add(first[i - 1]).wrapping_sub(0x2c)
            ^ first[i - 1].wrapping_shl(2)
            ^ 0x45;
    }
    let mut schedule = [0u64; 32];
    schedule[0] = u64::from_le_bytes(first);
    for i in 1..32 {
        schedule[i] = schedule[i - 1].wrapping_mul(5);
    }
    let mut plain = [0u8; 288];
    let mut checksum = 0u32;
    for (i, chunk) in cipher.chunks_exact(8).enumerate() {
        let word = schedule[i % 32];
        let key = word.to_le_bytes();
        let mut chain = (i as u8) ^ 0x45;
        let mut block = [0u8; 8];
        for (j, c) in chunk.iter().enumerate() {
            let mut value = *c ^ chain;
            chain = *c;
            for k in key {
                value = value.wrapping_add(0x78).wrapping_sub(k);
            }
            block[j] = value;
        }
        let counter = (i * 8) as u64;
        let mut expanded = counter;
        for _ in 0..3 {
            expanded = (expanded << 10) | counter;
        }
        let mixed =
            u64::from_le_bytes(block).wrapping_sub(word) ^ expanded.wrapping_add(0xa1652347) ^ word;
        block = mixed.rotate_left(32).to_le_bytes();
        plain[i * 8..i * 8 + 8].copy_from_slice(&block);
        if i < 35 {
            checksum = checksum
                .wrapping_add(u32::from(block[0]))
                .wrapping_add(u32::from(block[4]));
        }
    }
    if u32::from_le_bytes(plain[280..284].try_into().unwrap()) != 280
        || u32::from_le_bytes(plain[284..288].try_into().unwrap()) != checksum
        || plain[..24].iter().any(|b| *b != 0)
    {
        return Err("patch.ver checksum/header validation failed".into());
    }
    let text = &plain[24..280];
    let end = text
        .iter()
        .position(|b| *b == 0)
        .ok_or("patch.ver version is not terminated")?;
    if text[end..].iter().any(|b| *b != 0) {
        return Err("patch.ver has unexpected version padding".into());
    }
    let version =
        std::str::from_utf8(&text[..end]).map_err(|_| "patch.ver version is not ASCII")?;
    Ok(version.to_owned())
}

fn parse_reply(frame: &[u8]) -> Result<String, String> {
    parse_reply_for(frame, false)
}

fn parse_reply_for(frame: &[u8], viewer: bool) -> Result<String, String> {
    if frame.len() < HEADER
        || frame.len() > MAX_VERSION_FRAME
        || u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize != frame.len()
        || &frame[8..12] != b"POLP"
    {
        return Err(t("Official server returned an invalid POLP frame").into());
    }
    if md5::compute(&frame[8..]).0[..4] != frame[4..8] {
        return Err(t("Official patch reply checksum failed").into());
    }
    match u32::from_le_bytes(frame[12..16].try_into().unwrap()) {
        8 => {}
        5 => return Err((if viewer { t("Official patch server rejected the PlayOnline request") } else { t("Official patch server rejected the Windows FFXI request") }).into()),
        _ => return Err(t("Official patch server returned an unexpected reply type").into()),
    }
    let body = &frame[HEADER..];
    if body.len() < 76 {
        return Err(t("Official version reply is truncated").into());
    }
    let label = body[8..72].split(|b| *b == 0).next().unwrap_or_default();
    if label == b"unknown" {
        return Err(t("Official patch server did not recognize the version request").into());
    }
    let size = u32::from_le_bytes(body[72..76].try_into().unwrap()) as usize;
    if size == 0 || size > 64 || body.len() != 76 + size {
        return Err(t("Official version string has an invalid length").into());
    }
    let bytes = &body[76..];
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let version = std::str::from_utf8(bytes)
        .map_err(|_| t("Official version string is not ASCII").to_string())?;
    let valid = if viewer { valid_viewer_version(version) } else { stamp_key(version).is_some() };
    if !valid { return Err(t("Official patch server returned an unsupported version format").into()); }
    Ok(version.into())
}

pub(crate) fn valid_viewer_version(version: &str) -> bool {
    version.split_once('_').is_some_and(|(date, suffix)|
        date.len() == 8 && date.starts_with('2') && date.bytes().all(|b| b.is_ascii_digit())
        && !suffix.is_empty()
        && suffix.len() <= 16 && suffix.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_detection_accepts_fresh_installs_and_keeps_a_valid_selected_copy() {
        let root = install_with(None, false);
        let selected = root.join("selected");
        let registered = root.join("registered");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&registered).unwrap();
        std::fs::write(selected.join("FFXiMain.dll"), b"game marker").unwrap();
        std::fs::write(registered.join("FFXi.dll"), b"game marker").unwrap();
        let plain = |path: &Path| crate::backend::plain_windows_path(&std::fs::canonicalize(path).unwrap());
        assert_eq!(find_game_installation_from(Some(&selected), Some(&registered)), Some(plain(&selected)));
        std::fs::remove_file(selected.join("FFXiMain.dll")).unwrap();
        assert_eq!(find_game_installation_from(Some(&selected), Some(&registered)), Some(plain(&registered)));
        std::fs::remove_file(registered.join("FFXi.dll")).unwrap();
        std::fs::create_dir_all(registered.join("ROM/0")).unwrap();
        std::fs::write(registered.join("ROM/0/0.DAT"), b"repairable game marker").unwrap();
        assert_eq!(find_game_installation_from(None, Some(&registered)), Some(plain(&registered)));
        assert!(!registered.join("patch.ver").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn game_detection_rejects_stale_registrations_and_non_game_folders() {
        let root = install_with(None, false);
        let missing = root.join("removed");
        let viewer = root.join("viewer");
        std::fs::create_dir(&viewer).unwrap();
        std::fs::write(viewer.join("pol.exe"), b"viewer marker").unwrap();
        assert!(find_game_installation_from(Some(&missing), Some(&viewer)).is_none());
        assert!(find_game_installation_from(Some(Path::new("relative/client")), None).is_none());
        assert!(find_game_installation_from(None, Some(&viewer.join("pol.exe"))).is_none());
        assert!(find_game_installation_from(Some(&viewer.join("../viewer")), None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Read-only check of this PC's registered FFXI installation"]
    fn game_detection_reads_the_installed_windows_registration() {
        let started = std::time::Instant::now();
        let found = find_game_installation(None).expect("No installed US FFXI folder was detected");
        let registered = registry_install_folder("0001").expect("No US FFXI registration");
        assert_eq!(found, crate::backend::plain_windows_path(&std::fs::canonicalize(registered).unwrap()));
        println!("Detected {} in {:?}", found.display(), started.elapsed());
    }

    #[test]
    fn applied_encoder_roundtrips_installed_and_fallback_seeds() {
        for seed in [1, 0x123456789abcdef0, 0xffffffffffffffff] {
            let bytes = encode_applied_record("30260904_1", seed).unwrap();
            assert_eq!(decode_patch_record(&bytes, seed).unwrap(), "30260904_1");
            assert!(decode_patch_record(&bytes, seed.wrapping_add(1)).is_err());
        }
    }

    #[test]
    fn product_records_keep_viewer_revisions_and_title_keys_separate() {
        let root = install_with(None, false);
        for version in ["20061212_3", "20110829_E"] {
            let seed = applied_seed_for(&root, "1000").unwrap();
            let bytes = encode_applied_record_for(version, seed, "1000").unwrap();
            std::fs::write(root.join("patch.ver"), &bytes).unwrap();
            assert_eq!(local_version_for(&root, "1000").unwrap(), version);
            assert_eq!(applied_seed_for(&root, "1000").unwrap(), seed);
            assert!(decode_record_for(&bytes, seed, "0001").is_err());
            assert!(encode_applied_record_for(version, seed, "0001").is_err());
        }
        assert!(encode_applied_record_for("30260904_1", 1, "1000").is_err());
        std::fs::write(root.join("patch.ver"), b"corrupt record").unwrap();
        assert!(applied_seed_for(&root, "1000").is_err());
        assert!(repair_seed_for(&root, "1000").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn reply() -> Vec<u8> {
        let mut frame = vec![0; HEADER + 76];
        frame[8..12].copy_from_slice(b"POLP");
        frame[12..16].copy_from_slice(&8u32.to_le_bytes());
        frame[16..20].copy_from_slice(&1788518731u32.to_le_bytes());
        frame[20..24].copy_from_slice(&1u32.to_le_bytes());
        frame[24..29].copy_from_slice(b"empty");
        frame[88..92].copy_from_slice(&11u32.to_le_bytes());
        frame.extend_from_slice(b"30260904_1\0");
        let len = frame.len() as u32;
        frame[..4].copy_from_slice(&len.to_le_bytes());
        let digest = md5::compute(&frame[8..]);
        frame[4..8].copy_from_slice(&digest.0[..4]);
        frame
    }

    #[test]
    fn decodes_applied_version_record_and_rejects_wrong_keys_or_corruption() {
        // Independent encoder fixture derived from native routines, seed 1;
        // not a real registry key or a captured account/session record.
        let hex = "93d1f53dc7021e030368992fdb33a909a01cc9c400909ab19d1466c3c1cfdeca519d825677206ae569b460062824a77c74b7fd85a5e9772fbeee98d934bf99500fb9ad69fb38f00a2e90fcb0f52edb34bd6bb94b6e38ef566c3ab3ff942916bd9446bb66932f6c0de63c291e823c5ca507fe653157947991a2befea820c175e9f471000c2155c1da69f89d654d1dee78efe2fabedb448e725555b916ac82837fd6f205c9f80d1fe72d5cc9732af399718f566bc9e8a7851e663e1f9afe67dfcbbc62e403ddd2780f2034a8f34c167717224f0d2bb067996082a18a87743134d8da2febe096f1d57fbbe2df3bcdc67a24010994481b52ed0a79c4b180a2d1109bb3f0e8300acfd3ce234da00632da40e08039f00d891913384f2d7bf25984bb5d";
        let mut fixture: Vec<u8> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(decode_patch_record(&fixture, 1).unwrap(), "30260904_1");
        assert_eq!(encode_applied_record("30260904_1", 1).unwrap(), fixture);
        assert!(decode_patch_record(&fixture, 2).is_err());
        fixture[24] ^= 1;
        assert!(decode_patch_record(&fixture, 1).is_err());
        assert!(decode_patch_record(&fixture[..287], 1).is_err());
    }

    #[test]
    fn compares_versions_without_claiming_unknown_metadata_is_current() {
        assert_eq!(
            report(Ok("30260904_1".into()), Ok("30260904_1".into())).state,
            PatchState::Current
        );
        assert_eq!(
            report(Ok("30260805_0".into()), Ok("30260904_1".into())).state,
            PatchState::Required
        );
        assert_eq!(
            report(Err("missing".into()), Ok("30260904_1".into())).state,
            PatchState::Unknown
        );
        assert_eq!(
            report(Ok("30260904_1".into()), Err("offline".into())).state,
            PatchState::Unknown
        );
        assert_eq!(
            report(Ok("30260904_10".into()), Ok("30260904_9".into())).state,
            PatchState::Unknown
        );
    }

    #[test]
    fn validates_official_reply_layout_and_detects_corruption() {
        let frame = reply();
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        let mut corrupted = frame.clone();
        corrupted[95] ^= 1;
        assert!(parse_reply(&corrupted).unwrap_err().contains("checksum"));
        assert!(parse_reply(&frame[..frame.len() - 1]).is_err());
        assert!(parse_reply(&[]).is_err());
    }

    #[test]
    fn a_translated_update_required_message_still_parses() {
        // The template a translation would use: placeholders moved, other punctuation.
        let template = "更新が必要です: インストール済み {local}、公式サーバー {remote}。ダウンロードを開いてください。";
        let message = "更新が必要です: インストール済み 30260904_1、公式サーバー 30260905_0。ダウンロードを開いてください。";
        assert_eq!(
            parse_update_required_with(message, template),
            Some(("30260904_1".to_owned(), "30260905_0".to_owned()))
        );
        assert_eq!(parse_update_required_with("something else", template), None);
        // A translation may name the server's version first.
        assert_eq!(
            parse_update_required_with("公式 30260905_0（現在 30260904_1）。更新してください。", "公式 {remote}（現在 {local}）。更新してください。"),
            Some(("30260904_1".to_owned(), "30260905_0".to_owned()))
        );
        // The parser's template is gate's key, and a message gate built in
        // Japanese still parses after the window has switched to English (and
        // the other way round).
        let report = PatchReport {
            state: PatchState::Required,
            local_version: "30260904_1".into(),
            remote_version: "30260905_0".into(),
            detail: String::new(),
            files_verified: None,
            source_url: None,
        };
        crate::i18n::set(crate::i18n::Lang::En);
        assert!(gate(LaunchCheck::Update(report.clone())).unwrap_err().starts_with(UPDATE_REQUIRED_TEMPLATE.split_once('{').unwrap().0));
        for (built, shown) in [(crate::i18n::Lang::Ja, crate::i18n::Lang::En), (crate::i18n::Lang::En, crate::i18n::Lang::Ja)] {
            crate::i18n::set(built);
            let message = gate(LaunchCheck::Update(report.clone())).unwrap_err();
            crate::i18n::set(shown);
            let parsed = parse_update_required(&message).unwrap_or_else(|| panic!("unparsed: {message}"));
            assert_eq!((parsed.local_version.as_str(), parsed.remote_version.as_str()), ("30260904_1", "30260905_0"));
        }
        crate::i18n::set(crate::i18n::Lang::En);
        // A gate key edit must update the parser's copy too.
        // (Its definition, and gate's tf call.)
        assert_eq!(include_str!("patch.rs").matches(&format!("\"{UPDATE_REQUIRED_TEMPLATE}\"")).count(), 2);
        // English keeps working through both routes.
        let english = gate(LaunchCheck::Update(PatchReport {
            state: PatchState::Required,
            local_version: "30260904_1".into(),
            remote_version: "30260905_0".into(),
            detail: String::new(),
            files_verified: None,
            source_url: None,
        }))
        .unwrap_err();
        assert!(english.starts_with(UPDATE_REQUIRED));
        assert!(parse_update_required(&english).is_some());
    }

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks_exact(2)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect()
    }

    /// Golden bytes: the 88-byte frames the stock Viewer sent in the live capture
    /// (`W2U`/`1000` to :54000 and `W20`/`0001` to :53001), checksum included.
    const STOCK_VIEWER: &str = "58000000a6d67388504f4c5007000000573255003130303032303131303832395f45000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const STOCK_FFXI: &str = "5800000061d437a3504f4c5007000000573230003030303133303236303930345f31000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    /// What Crystal used to send at launcher start, on refresh and every 600 s.
    const OLD_EMPTY: &str = "580000007c6e8b19504f4c5007000000573230003030303100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn stock_frames_are_reproduced_byte_for_byte() {
        assert_eq!(VIEWER.request("20110829_E"), hex(STOCK_VIEWER));
        assert_eq!(FFXI.request("30260904_1"), hex(STOCK_FFXI));
        assert_eq!(VIEWER.server(), "pc001.pol.com:54000");
        assert_eq!(FFXI.server(), "pc001.pol.com:53001");
        // Empty is the fresh-install shape only; it is never produced for an installed version.
        assert_eq!(FFXI.request(""), hex(OLD_EMPTY));
        assert_ne!(FFXI.request("30260904_1"), FFXI.request(""));
    }

    fn install_with(version: Option<&str>, stamped: bool) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crystal-patch-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(version) = version {
            let bytes = if stamped {
                encode_applied_record(version, 1).unwrap()
            } else {
                // The Viewer's key is its registry string plus 1000; 1000 alone is the fallback.
                encode_record(version, 1000).unwrap()
            };
            std::fs::write(dir.join("patch.ver"), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn same_version_starts_and_a_different_one_goes_to_update() {
        let install = install_with(Some("30260904_1"), true);
        let mut sent = Vec::new();
        let outcome = check_with(&install, |request| {
            sent = request.to_vec();
            Ok(reply())
        });
        // The request carried the installed version in the stock shape.
        assert_eq!(sent, hex(STOCK_FFXI));
        assert!(matches!(outcome, LaunchCheck::Start(_)));
        assert!(gate(outcome).unwrap().contains("30260904_1"));

        let mut newer = reply();
        newer[92..102].copy_from_slice(b"30260905_0");
        let len = newer.len();
        let digest = md5::compute(&newer[8..len]);
        newer[4..8].copy_from_slice(&digest.0[..4]);
        let outcome = check_with(&install, |_| Ok(newer.clone()));
        let LaunchCheck::Update(report) = &outcome else { panic!("expected an update") };
        assert_eq!((report.local_version.as_str(), report.remote_version.as_str()), ("30260904_1", "30260905_0"));
        let message = gate(outcome).unwrap_err();
        assert!(message.starts_with(UPDATE_REQUIRED) && message.contains("Update"));
        let shown = parse_update_required(&message).unwrap();
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!((shown.local_version.as_str(), shown.remote_version.as_str()), ("30260904_1", "30260905_0"));
    }

    #[test]
    fn an_interrupted_transaction_blocks_game_and_viewer_even_with_current_records() {
        for (version, game) in [("30260904_1", true), ("20110829_E", false)] {
            let install = install_with(Some(version), game);
            let original = std::fs::read(install.join("patch.ver")).unwrap();
            std::fs::create_dir(install.join(".xi-updater")).unwrap();
            let journal = install.join(".xi-updater/transaction.json");
            // Existence, including a damaged journal, requires recovery first.
            std::fs::write(&journal, b"interrupted journal fixture").unwrap();
            assert!(check_update_state(&install).unwrap_err().contains("interrupted update"));
            if game {
                let outcome = check_with(&install, |_| panic!("unfinished update cannot query or start"));
                let LaunchCheck::Update(report) = &outcome else { panic!("expected recovery refusal") };
                assert!(report.detail.contains("recovered"));
                assert!(gate(outcome).is_err());
                assert_eq!(report_with_remote(&install, version).state, PatchState::Required);
            } else {
                assert_eq!(viewer_check_with(&install, |_| panic!("unfinished Viewer update cannot query"))
                    .state, PatchState::Required);
                assert_eq!(viewer_report_with_remote(&install, Some(version)).state, PatchState::Required);
            }
            assert_eq!(std::fs::read(install.join("patch.ver")).unwrap(), original);
            assert!(journal.is_file());
            std::fs::remove_dir_all(install).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn another_launcher_update_blocks_version_checks_without_changing_its_record() {
        use std::os::windows::fs::OpenOptionsExt;
        let install = install_with(Some("30260904_1"), true);
        std::fs::create_dir(install.join(".xi-updater")).unwrap();
        let path = install.join(".xi-updater/lock");
        let updating = || std::fs::OpenOptions::new().read(true).write(true)
            .create(true).truncate(false).share_mode(0).open(&path);
        let exclusive = updating().unwrap();
        assert!(matches!(check_with(&install, |_| panic!("active updater cannot launch")), LaunchCheck::Update(_)));
        // The updater itself still uses the installed version under its lock.
        assert_eq!(installed_for_request(&install), "30260904_1");
        drop(exclusive);
        let held = check_update_state(&install).unwrap().unwrap();
        assert!(updating().is_err(), "a version check keeps the existing lock read-shared");
        drop(held);
        assert!(matches!(check_with(&install, |_| Ok(reply())), LaunchCheck::Start(_)));
        std::fs::remove_dir_all(install).unwrap();
    }

    #[test]
    fn an_unreachable_server_starts_unverified_never_with_an_empty_request() {
        let install = install_with(Some("30260904_1"), true);
        let mut requests = Vec::new();
        let outcome = check_with(&install, |request| {
            requests.push(request.to_vec());
            Err("Cannot connect to official patch server: refused".into())
        });
        assert_eq!(requests, vec![hex(STOCK_FFXI)]);
        let LaunchCheck::Unverified(report) = &outcome else { panic!("expected unverified") };
        assert_eq!(report.state, PatchState::Unknown);
        let note = gate(outcome).unwrap();
        assert!(note.contains("Starting anyway") && note.contains("refused"));
    }

    #[test]
    fn invalid_and_fresh_records_require_initialization_without_changing_stock_request_shapes() {
        let broken = install_with(None, true);
        std::fs::write(broken.join("patch.ver"), [7u8; 288]).unwrap();
        let outcome = check_with(&broken, |_| panic!("no request may be sent for an unreadable record"));
        assert!(matches!(outcome, LaunchCheck::Update(_)));
        assert_eq!(installed_for_request(&broken), "");

        let fresh = install_with(None, true);
        let mut sent = Vec::new();
        let outcome = check_with(&fresh, |request| {
            sent = request.to_vec();
            Ok(reply())
        });
        assert_eq!(sent, hex(OLD_EMPTY));
        assert!(matches!(outcome, LaunchCheck::Update(_)));
        assert_eq!(installed_for_request(&fresh), "");
        assert_eq!(installed_for_request(&install_with(Some("30260904_1"), true)), "30260904_1");
    }

    #[test]
    fn missing_record_is_not_ready_even_before_a_network_check() {
        let root = install_with(None, true);
        let local = local_report(&root);
        assert_eq!(local.state, PatchState::Required);
        assert_eq!(local.local_version, t("Not initialized"));
        assert_eq!(local.remote_version, t("Not checked"));
        let shown = report_with_remote(&root, "30260904_1");
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!(shown.remote_version, "30260904_1");
        assert!(gate(check_with(&root, |_| Err("offline".into()))).is_err());
        assert!(!root.join("patch.ver").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_file_outcomes_override_a_matching_version_record() {
        let root = install_with(Some("30260904_1"), true);
        let mut files = crate::updater::Report { version: "30260904_1".into(), total: 2,
            current: 1, missing: 1, ..Default::default() };
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Check);
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!(shown.files_verified, Some(false));
        assert!(shown.detail.contains("1 missing"));
        files.missing = 0; files.modified = 1;
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Update);
        assert_eq!(shown.state, PatchState::Unknown, "preserved modifications are not claimed verified");
        assert_eq!(shown.files_verified, Some(false));
        assert!(shown.detail.contains("preserved") && shown.detail.contains("backups"));
        files.modified = 0; files.verified = true;
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Update);
        assert_eq!(shown.state, PatchState::Current);
        assert_eq!(shown.files_verified, Some(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_clean_file_check_does_not_hide_a_missing_applied_version_record() {
        let root = install_with(None, true);
        let files = crate::updater::Report { version: "30260904_1".into(), total: 2,
            current: 2, verified: true, ..Default::default() };
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Check);
        assert_eq!(shown.files_verified, Some(true));
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!(local_report(&root).files_verified, None);
        assert_eq!(report_with_remote(&root, &files.version).files_verified, None);
        assert!(!root.join("patch.ver").exists(), "Check files never initializes an applied record");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_viewer_check_sends_the_viewers_own_installed_version() {
        let viewer = install_with(Some("20110829_E"), false);
        assert_eq!(read_local(&viewer, VIEWER.title()), Local::Installed("20110829_E".into()));
        let mut sent = Vec::new();
        let report = viewer_check_with(&viewer, |request| {
            sent = request.to_vec();
            let mut frame = reply();
            frame[92..102].copy_from_slice(b"20110829_E");
            let digest = md5::compute(&frame[8..]);
            frame[4..8].copy_from_slice(&digest.0[..4]);
            Ok(frame)
        });
        assert_eq!(sent, hex(STOCK_VIEWER));
        assert_eq!(report.state, PatchState::Current);
        assert_eq!(report.remote_version, "20110829_E");
        assert_eq!(viewer_check_with(&viewer, |_| Err("down".into())).state, PatchState::Unknown);
        let unreadable = install_with(None, true);
        std::fs::write(unreadable.join("patch.ver"), [1u8; 288]).unwrap();
        assert_eq!(viewer_check_with(&unreadable, |_| panic!("nothing may be sent")).state, PatchState::Unknown);
    }

    fn viewer_reply(version: &str) -> Vec<u8> {
        let mut frame = reply();
        assert_eq!(version.len(), 10);
        frame[92..102].copy_from_slice(version.as_bytes());
        let digest = md5::compute(&frame[8..]);
        frame[4..8].copy_from_slice(&digest.0[..4]);
        frame
    }

    #[test]
    fn viewer_initialization_and_a_version_mismatch_require_its_own_update() {
        let fresh = install_with(None, false);
        let report = viewer_check_with(&fresh, |request| {
            assert_eq!(request, VIEWER.request(""));
            Ok(viewer_reply("20110829_E"))
        });
        assert_eq!(report.state, PatchState::Required);
        assert_eq!(report.local_version, "Not initialized");
        assert!(report.detail.contains("initial update"));
        let old = install_with(Some("20070101_E"), false);
        let report = viewer_check_with(&old, |request| {
            assert_eq!(request, VIEWER.request("20070101_E"));
            Ok(viewer_reply("20110829_E"))
        });
        assert_eq!(report.state, PatchState::Required);
        assert_eq!((report.local_version.as_str(), report.remote_version.as_str()), ("20070101_E", "20110829_E"));
        std::fs::remove_dir_all(fresh).unwrap(); std::fs::remove_dir_all(old).unwrap();
    }

    #[test]
    fn viewer_rejection_bad_checksum_and_ffxi_version_cannot_claim_it_is_current() {
        let viewer = install_with(Some("20110829_E"), false);
        let mut reject = viewer_reply("20110829_E"); reject[12..16].copy_from_slice(&5u32.to_le_bytes());
        let digest = md5::compute(&reject[8..]); reject[4..8].copy_from_slice(&digest.0[..4]);
        let mut bad = viewer_reply("20110829_E"); bad[4] ^= 1;
        for frame in [reject, bad, reply()] {
            assert_eq!(viewer_check_with(&viewer, |_| Ok(frame)).state, PatchState::Unknown);
        }
        assert!(parse_reply(&viewer_reply("20110829_E")).is_err());
        assert_eq!(parse_reply_for(&viewer_reply("20110829_E"), true).unwrap(), "20110829_E");
        std::fs::remove_dir_all(viewer).unwrap();
    }

    /// On a machine with the stock Viewer installed, its patch.ver must decode with the
    /// product-1000 registry key (not the FFXI one).
    #[test]
    fn an_installed_viewers_own_record_decodes_with_its_own_key() {
        let Some(dir) = registry_install_folder(VIEWER.title()) else { return };
        if !dir.join("patch.ver").is_file() {
            return;
        }
        match read_local(&dir, VIEWER.title()) {
            Local::Installed(version) => eprintln!("Viewer version {version}"),
            other => panic!("the Viewer's patch.ver did not decode: {other:?}"),
        }
    }
    fn serve(reply: Vec<u8>, chunk: usize, linger: Duration) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 88];
            stream.read_exact(&mut request).unwrap();
            for part in reply.chunks(chunk.max(1)) {
                stream.write_all(part).unwrap();
                stream.flush().unwrap();
            }
            std::thread::sleep(linger);
            request
        });
        (address, server)
    }

    #[test]
    fn handles_a_fragmented_tcp_version_reply() {
        let (address, server) = serve(reply(), 3, Duration::ZERO);
        let frame = exchange(&address, &FFXI.request("30260904_1"), Duration::from_secs(5)).unwrap();
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        assert_eq!(server.join().unwrap(), hex(STOCK_FFXI));
    }

    #[test]
    fn a_silent_server_times_out_instead_of_hanging_a_launch() {
        let (address, server) = serve(Vec::new(), 1, Duration::from_millis(800));
        let started = Instant::now();
        let error = exchange(&address, &FFXI.request("30260904_1"), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("reply"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(790));
        server.join().unwrap();
    }

}
