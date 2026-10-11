//! Crystal's unified PlayOnline and FFXI verification, update, and repair.
//! The packet, manifest, and patch codecs are implemented in this crate.
//! Downloaded manifests describe desired files; only a verified transaction can
//! advance patch.ver. Check never creates updater state or changes the install.
mod lz;
mod manifest;
mod installer_baseline;
pub(crate) mod polp;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod download_bench;
#[cfg(test)]
mod product_tests;
#[cfg(test)]
mod viewer_live_test;
#[cfg(test)]
mod fresh_install_bench;

use crate::i18n::{t, tf};
use crate::operation::{Operation, UpdateWork};
use manifest::{FileHistory, FileSig, Manifest};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVER: &str = "pc001.pol.com:53001";
const VIEWER_SERVER: &str = "pc001.pol.com:54000";
const MAX_MANIFEST: usize = 64 * 1024 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const STATE_DIR: &str = ".xi-updater";
const SIGNATURE_BUFFER_LEN: usize = 1 << 20;
const MAX_CHECK_WORKERS: usize = 8;
const ACTIVE_GAME_CHECK_WORKERS: usize = 4;
const MAX_DOWNLOAD_WORKERS: usize = 8;
const DOWNLOAD_BUDGET_MIB: usize = 1024;
#[cfg(test)]
const MAX_BENCHMARK_CHECK_WORKERS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Product { Ffxi, Viewer }
impl Product {
    fn label(self) -> &'static str {
        match self { Self::Ffxi => t("FINAL FANTASY XI"), Self::Viewer => t("PlayOnline") }
    }
    fn server(self) -> &'static str {
        match self { Self::Ffxi => SERVER, Self::Viewer => VIEWER_SERVER }
    }
    fn title(self) -> &'static str {
        match self { Self::Ffxi => "0001", Self::Viewer => "1000" }
    }
    fn tags(self) -> polp::Tags {
        polp::Tags { platform: match self { Self::Ffxi => *b"W20\0", Self::Viewer => *b"W2U\0" },
            title: match self { Self::Ffxi => *b"0001", Self::Viewer => *b"1000" } }
    }
    fn valid_stamp(self, version: &str) -> bool {
        match self { Self::Ffxi => valid_stamp(version), Self::Viewer => crate::patch::valid_viewer_version(version) }
    }
    fn latest_stamp<'a>(self, manifest: &'a Manifest) -> Option<&'a str> {
        match self {
            Self::Ffxi => manifest.latest_stamp(),
            // Viewer revisions are hexadecimal digits in the live manifest.
            // The fixed-width date sorts chronologically before its revision.
            Self::Viewer => manifest.files.iter().flat_map(|h| h.versions.iter()).map(|v| v.stamp.as_str())
                .max_by_key(|s| s.split_once('_').map(|(date, revision)|
                    (date.parse::<u64>().unwrap_or(0), u64::from_str_radix(revision, 16).unwrap_or(0)))),
        }
    }
    fn parse_manifest(self, text: &str) -> Result<Manifest, String> {
        match self { Self::Ffxi => manifest::parse(text), Self::Viewer => manifest::parse_viewer(text) }
    }
    fn deployment(self, root: &Path, manifest: Manifest) -> Result<Manifest, String> {
        if self == Self::Ffxi { return Ok(manifest); }
        let mut files = Vec::new();
        for mut history in manifest.files {
            let lower = history.path.to_ascii_lowercase();
            // The same region=1 filter used by app.dll for PlayOnlineUS.
            if lower.starts_with("eu/") || matches!(lower.as_str(),
                "patchfiles/playonlineviewer/viewer/com/appeu.dll" |
                "patchfiles/playonlineviewer/viewer/com/polcoreeu.dll") { continue; }
            if lower.starts_with("patchfiles/") {
                let target = match lower.as_str() {
                    "patchfiles/playonlineviewer/viewer/com/app.dll" => "viewer/com/app.dll",
                    "patchfiles/playonlineviewer/viewer/com/polcore.dll" => "viewer/com/polcore.dll",
                    _ => return Err(tf("Unsafe patch path {path}", &[("path", &history.path)])),
                };
                // A stock updater may have left a pending staged DLL. Keep it
                // in sync so the next official Viewer cannot copy an old DLL
                // over Crystal's verified deployment. Its absence is normal.
                if confined(root, &history.path)?.is_file() { files.push(history.clone()); }
                history.path = target.into();
                history.optional = false;
            }
            if history.optional && !confined(root, &history.path)?.is_file() { continue; }
            files.push(history);
        }
        let manifest = Manifest { files };
        validate_manifest_for(self, &manifest)?;
        Ok(manifest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Check,
    Update,
    Repair,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub summary: String,
    pub version: String,
    pub total: usize,
    pub current: usize,
    pub outdated: usize,
    pub missing: usize,
    pub modified: usize,
    pub repaired: usize,
    pub bytes: u64,
    pub verified: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Direct,
    Deltas(usize),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    index: usize,
    action: Action,
    original: Option<FileSig>,
}

#[derive(Clone)]
struct StageGroup { source: Planned, targets: Vec<Planned> }

/// The Viewer deploys one payload to both its live and retained staged DLL.
/// Their old signatures may differ, so choose the cheapest valid source plan
/// while retaining each target's own transaction precondition.
fn stage_groups(product: Product, manifest: &Manifest, planned: &[Planned]) -> Vec<StageGroup> {
    let mut groups: Vec<StageGroup> = Vec::new();
    let mut payloads = std::collections::HashMap::new();
    for item in planned {
        let current = manifest.files[item.index].current();
        let key = (current.direct.clone(), current.sig);
        let existing = if product == Product::Viewer { payloads.get(&key).copied() } else { None };
        if let Some(index) = existing {
            let group: &mut StageGroup = &mut groups[index];
            if download_size(&manifest.files[item.index], &item.action)
                < download_size(&manifest.files[group.source.index], &group.source.action) {
                group.source = item.clone();
            }
            group.targets.push(item.clone());
        } else {
            payloads.insert(key, groups.len());
            groups.push(StageGroup { source: item.clone(), targets: vec![item.clone()] });
        }
    }
    groups
}

/// Exercise live POL transports and both codecs entirely in memory. This does
/// not create updater state, stage downloads, or change an applied record.
pub async fn probe(install: PathBuf, operation: Operation) -> Result<String, String> {
    operation.check_cancelled()?;
    ensure_plain(&install)?;
    let root = fs::canonicalize(&install).map_err(|e| format!("Install directory: {e}"))?;
    ensure_plain(&root)?;
    if !root.is_dir() {
        return Err("Install must be a directory".into());
    }
    operation.progress("Probe", 0, 5, "Fetching canonical manifest into memory");
    let mut session = Session::connect(&operation).await?;
    // The stock shape: FFXI's installed version (empty only for a fresh install).
    let installed = crate::patch::installed_for_request(&root);
    let reply = session
        .exchange(&polp::version(&tags(), installed.as_bytes()), 8, &operation)
        .await?;
    let version = polp::parse_version_reply(&reply[16..])?;
    if version.label == "unknown" || !valid_stamp(&version.version) {
        return Err("Invalid official version reply".into());
    }
    let reply = session
        .exchange(&polp::hello(&tags()), 2, &operation)
        .await?;
    let text = decode_payload(&reply[16..], MAX_MANIFEST)?;
    let m = manifest::parse(std::str::from_utf8(&text).map_err(|_| "Manifest is not UTF-8")?)?;
    validate_manifest(&m)?;
    if m.latest_stamp() != Some(version.version.as_str()) {
        return Err("Official manifest/version changed during probe".into());
    }
    let mut lines = vec![format!(
        "Official manifest: version {}, {} files, {} decoded bytes, MD5 {:x}.",
        version.version,
        m.files.len(),
        text.len(),
        md5::compute(&text)
    )];
    for (i, path) in ["patch.sin", "patch.txt", "ROM/285/40.DAT"]
        .iter()
        .enumerate()
    {
        operation.check_cancelled()?;
        let history = m
            .files
            .iter()
            .find(|h| h.path == *path)
            .ok_or_else(|| format!("Canonical manifest lacks {path}"))?;
        operation.progress(
            "Probe",
            i as u64 + 1,
            5,
            format!("Downloading {path} into memory"),
        );
        let raw = session
            .fetch(
                &history.current().direct,
                history.current().direct_len,
                &operation,
            )
            .await?;
        let decoded = decode_payload(&raw, history.current().sig.len as usize)?;
        let remote = FileSig::of(&decoded);
        if remote != history.current().sig {
            return Err(format!(
                "{path}: live direct download does not match canonical signature"
            ));
        }
        let local_path = confined(&root, path)?;
        let local = fs::read(&local_path)
            .map_err(|e| format!("{path}: cannot read local comparison: {e}"))?;
        operation.check_cancelled()?;
        if FileSig::of(&local) != remote || local != decoded {
            return Err(format!(
                "{path}: live direct download verified, but local bytes differ"
            ));
        }
        lines.push(format!("{path}: direct payload mode {}, {} downloaded bytes -> {} decoded bytes; signature ({}, {}, {}) and local bytes match.",raw[0],raw.len(),decoded.len(),remote.len,remote.byte_sum,remote.md5_word));
    }
    // Exercise an adjacent historical pair; never assume an arbitrary local
    // file is the correct delta base. Both base and delta come from the server.
    let candidate = m
        .files
        .iter()
        .flat_map(|h| h.versions.windows(2).map(move |pair| (h, pair)))
        .filter(|(_, pair)| {
            pair[1]
                .indirect
                .as_ref()
                .is_some_and(|d| d.len <= 1024 * 1024)
                && pair[0].direct_len <= 1024 * 1024
                && pair[0].sig.len <= 1024 * 1024
                && pair[1].sig.len <= 1024 * 1024
        })
        .min_by_key(|(_, pair)| pair[0].direct_len + pair[1].indirect.as_ref().unwrap().len);
    if let Some((history, pair)) = candidate {
        operation.progress(
            "Probe",
            4,
            5,
            format!("Testing a historical delta for {} in memory", history.path),
        );
        let raw = session
            .fetch(&pair[0].direct, pair[0].direct_len, &operation)
            .await?;
        let base = decode_payload(&raw, pair[0].sig.len as usize)?;
        if FileSig::of(&base) != pair[0].sig {
            return Err("Downloaded historical delta base failed signature verification".into());
        }
        let delta = pair[1].indirect.as_ref().unwrap();
        let raw = session.fetch(&delta.path, delta.len, &operation).await?;
        operation.check_cancelled()?;
        let target = lz::apply_indirect_bounded(&raw, &base, pair[1].sig.len as usize)?;
        if FileSig::of(&target) != pair[1].sig {
            return Err("Live historical delta result failed signature verification".into());
        }
        let direct = session
            .fetch(&pair[1].direct, pair[1].direct_len, &operation)
            .await?;
        let direct = decode_payload(&direct, pair[1].sig.len as usize)?;
        if direct != target {
            return Err(
                "Live historical delta differs from the same version's direct download".into(),
            );
        }
        lines.push(format!("{}: delta {} -> {}, {} delta bytes; old base, delta result and independent target direct download all verify.",history.path,pair[0].stamp,pair[1].stamp,raw.len()));
    } else {
        lines.push("No small historical delta pair is available in the current manifest.".into());
    }
    operation.check_cancelled()?;
    operation.progress(
        "Probe",
        5,
        5,
        "Read-only direct/delta verification complete",
    );
    lines.push("All downloads remained in memory. No installation files or applied-version metadata changed.".into());
    Ok(lines.join("\n"))
}

/// Read-only full verification, or a recoverable update/explicit repair.
pub async fn run(install: PathBuf, mode: Mode, operation: Operation) -> Result<Report, String> {
    run_for(Product::Ffxi, install, mode, operation).await
}

pub(crate) async fn run_for(product: Product, install: PathBuf, mode: Mode, operation: Operation) -> Result<Report, String> {
    let prepared = prepare(product, install, mode, &operation).await?;
    let mut reports = execute(vec![prepared], &operation).await?;
    Ok(reports.remove(0))
}

pub(crate) struct CombinedReport { pub viewer: Report, pub game: Report }
impl CombinedReport {
    pub fn summary(&self) -> String {
        tf("PlayOnline: {pol}\nFINAL FANTASY XI: {ffxi}", &[("pol", &self.viewer.summary), ("ffxi", &self.game.summary)])
    }
}

pub(crate) struct CombinedFailure { pub message: String, pub viewer_version: Option<String> }
impl From<String> for CombinedFailure {
    fn from(message: String) -> Self { Self { message, viewer_version: None } }
}

/// One engine and one operation. Plan both components before downloading so
/// the total and ETA cover both; stage everything, then commit POL before FFXI.
pub(crate) async fn run_combined(viewer: PathBuf, game: PathBuf, mode: Mode, operation: Operation) -> Result<CombinedReport, CombinedFailure> {
    ensure_plain(&viewer)?; ensure_plain(&game)?;
    let viewer = fs::canonicalize(viewer).map_err(|e| e.to_string())?;
    let game = fs::canonicalize(game).map_err(|e| e.to_string())?;
    if viewer == game || viewer.starts_with(&game) || game.starts_with(&viewer) {
        return Err(t("Choose separate PlayOnline and FINAL FANTASY XI installation folders.").to_owned().into());
    }
    operation.check_cancelled()?;
    if mode != Mode::Check {
        operation.begin_update();
        ensure_idle(&viewer)?; ensure_idle(&game)?;
    }
    let pol = prepare(Product::Viewer, viewer, mode, &operation.component(t("PlayOnline"))).await?;
    let viewer_version = pol.report.version.clone();
    let failure = |message| CombinedFailure { message, viewer_version: Some(viewer_version.clone()) };
    let ffxi = prepare(Product::Ffxi, game, mode, &operation.component(t("FINAL FANTASY XI"))).await.map_err(failure)?;
    let mut reports = execute(vec![pol, ffxi], &operation).await.map_err(failure)?;
    let game = reports.pop().unwrap();
    Ok(CombinedReport { viewer: reports.pop().unwrap(), game })
}

struct PreparedUpdate {
    product: Product,
    root: PathBuf,
    _lock: Option<fs::File>,
    mode: Mode,
    report: Report,
    manifest: Manifest,
    text: Vec<u8>,
    planned: Vec<Planned>,
    seed: Option<u64>,
}

async fn prepare(product: Product, install: PathBuf, mode: Mode, operation: &Operation) -> Result<PreparedUpdate, String> {
    operation.check_cancelled()?;
    if mode != Mode::Check { operation.begin_update(); }
    ensure_plain(&install)?;
    let root = fs::canonicalize(&install).map_err(|e| tf("Install directory: {error}", &[("error", &e)]))?;
    ensure_plain(&root)?;
    if !root.is_dir() { return Err(t("Install must be a directory").into()); }
    validate_installation(product, &root)?;
    let lock = if mode != Mode::Check {
        ensure_idle(&root)?;
        let state = confined(&root, STATE_DIR)?;
        fs::create_dir_all(&state).map_err(|e| e.to_string())?;
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(windows)] {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        Some(options.open(confined(&root, &format!("{STATE_DIR}/lock"))?)
            .map_err(|e| tf("Another updater may be using this install: {error}", &[("error", &e)]))?)
    } else { None };
    if mode != Mode::Check { ensure_idle(&root)?; recover(&root)?; }
    operation.progress("Manifest", 0, 1, t("Downloading the official file manifest"));
    let mut session = Session::connect_for(product, operation).await?;
    let installed = crate::patch::installed_for_request_for(&root, product.title());
    let version_frame = session.exchange(&polp::version(&product.tags(), installed.as_bytes()), 8, operation).await?;
    let version = polp::parse_version_reply(&version_frame[16..])?;
    if version.label == "unknown" || !product.valid_stamp(&version.version) {
        return Err(t("Official version response is invalid").into());
    }
    let frame = session.exchange(&polp::hello(&product.tags()), 2, operation).await?;
    let text = decode_payload(&frame[16..], MAX_MANIFEST)?;
    let utf8 = std::str::from_utf8(&text).map_err(|_| t("Official manifest is not UTF-8"))?;
    let manifest = product.parse_manifest(utf8)?;
    validate_manifest_for(product, &manifest)?;
    if product.latest_stamp(&manifest) != Some(version.version.as_str()) {
        return Err(t("Official version changed while fetching the manifest; retry").into());
    }
    drop(session);
    let manifest = product.deployment(&root, manifest)?;
    operation.progress("Manifest", 1, 1, tf("{count} files, version {version}",
        &[("count", &manifest.files.len()), ("version", &version.version)]));
    let scan_root = root.clone(); let scan_manifest = manifest.clone(); let scan_operation = operation.clone();
    // POL's official update restores canonical components. Fresh installer
    // DLLs are not necessarily in the retained patch history. Preserve FFXI
    // modifications as before; POL originals still receive transaction backups.
    let scan_mode = if product == Product::Viewer && mode == Mode::Update { Mode::Repair } else { mode };
    let (mut report, planned) = tokio::task::spawn_blocking(move || scan_for(product, &scan_root, &scan_manifest, scan_mode, &scan_operation))
        .await.map_err(|e| e.to_string())??;
    report.version = version.version;
    let seed = if mode == Mode::Check { None } else {
        confined(&root, "patch.ver")?;
        Some(if mode == Mode::Repair { crate::patch::repair_seed_for(&root, product.title())? }
            else { crate::patch::applied_seed_for(&root, product.title())? })
    };
    if mode == Mode::Check {
        report.verified = report.current == report.total;
        let counts: [(&str, &dyn std::fmt::Display); 5] = [("total", &report.total), ("current", &report.current),
            ("outdated", &report.outdated), ("missing", &report.missing), ("modified", &report.modified)];
        report.summary = if report.verified {
            tf("Checked {total} files: {current} current, {outdated} outdated, {missing} missing, {modified} modified. All official files match.", &counts)
        } else {
            tf("Checked {total} files: {current} current, {outdated} outdated, {missing} missing, {modified} modified. No files changed.", &counts)
        };
    }
    Ok(PreparedUpdate { product, root, _lock: lock, mode, report, manifest, text, planned, seed })
}

// A directory selection is not evidence of an installed product. Allow repair
// when individual binaries are missing, but never populate an unrelated folder.
fn validate_installation(product: Product, root: &Path) -> Result<(), String> {
    let markers: &[&str] = match product {
        Product::Ffxi => &["FFXi.dll", "FFXiMain.dll", "ROM/0/0.DAT"],
        Product::Viewer => &["pol.exe", "viewer/com/polcore.dll", "viewer/com/app.dll"],
    };
    for marker in markers {
        let path = confined(root, marker)?;
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(()),
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err(tf("The selected folder does not contain an installed {product}. Run official setup or select its existing installation folder.",
        &[("product", &product.label())]))
}

async fn execute(updates: Vec<PreparedUpdate>, operation: &Operation) -> Result<Vec<Report>, String> {
    if updates.iter().all(|update| update.mode == Mode::Check) {
        return Ok(updates.into_iter().map(|u| u.report).collect());
    }
    let sizes: Vec<u64> = updates.iter().flat_map(|u| stage_groups(u.product, &u.manifest, &u.planned).into_iter()
        .map(|g| download_size(&u.manifest.files[g.source.index], &g.source.action))).collect();
    operation.plan_update(sizes.iter().sum(),
        updates.iter().flat_map(|u| u.planned.iter().map(|p| staging_work(&u.manifest.files[p.index], p))).sum(),
        updates.iter().flat_map(|u| u.planned.iter().map(|p| applying_work(u.manifest.files[p.index].current().sig, p.original))).sum(),
        updates.iter().flat_map(|u| u.manifest.files.iter().map(|h| io_work(h.current().sig))).sum());
    let downloads = Downloads::new(operation.clone(), sizes);
    let mut staged_updates = Vec::new(); let mut first = 0;
    for mut update in updates {
        operation.check_cancelled()?;
        let op = operation.component(update.product.label());
        let staged = confined(&update.root, &format!("{STATE_DIR}/staged"))?;
        fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
        let (ready, bytes) = stage_plan_shared(update.product, &update.root, &staged, &update.manifest,
            &update.planned, &op, downloads.clone(), first).await?;
        first += stage_groups(update.product, &update.manifest, &update.planned).len(); update.report.bytes = bytes;
        staged_updates.push((update, ready));
    }
    let mut reports = Vec::new();
    for (update, ready) in staged_updates {
        let op = operation.component(update.product.label());
        reports.push(tokio::task::spawn_blocking(move || commit(update, ready, &op)).await.map_err(|e| e.to_string())??);
    }
    operation.progress("Finish", 0, 0, t("Finalizing update…"));
    operation.finish_update();
    Ok(reports)
}

fn commit(mut update: PreparedUpdate, ready: Vec<Ready>, operation: &Operation) -> Result<Report, String> {
    operation.check_cancelled()?; ensure_idle(&update.root)?;
    let preserved = update.report.modified > 0 && update.product == Product::Ffxi && update.mode == Mode::Update;
    let planned: HashSet<_> = update.planned.iter().map(|item| item.index).collect();
    let baseline = if preserved { Some(installer_baseline::load()?) } else { None };
    let committed = apply(&update.root, &ready, operation, || {
        let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
        let mut retained_modifications = false;
        for (i, history) in update.manifest.files.iter().enumerate() {
            operation.check_cancelled()?;
            operation.progress("Verify", i as u64, update.manifest.files.len() as u64, history.path.clone());
            let path = confined(&update.root, &history.path)?;
            let local = if path.is_file() { Some(local_file_with_buffer(&path, operation, &mut buffer)?) } else { None };
            if local.map(|file| file.sig) != Some(history.current().sig) {
                // A preserved modification does not excuse a failed download,
                // an outdated stock file, or a missing file later in the list.
                if preserved && !planned.contains(&i) && local.is_some_and(|file|
                    !history.versions.iter().any(|version| version.sig == file.sig)
                    && !baseline.is_some_and(|b| b.matches(&history.path, file.sig.len, file.md5))) {
                    retained_modifications = true;
                } else {
                    return Err(tf("{path}: final file verification failed", &[("path", &history.path)]));
                }
            }
            operation.complete_update_work(UpdateWork::Verify, io_work(history.current().sig), None);
        }
        operation.progress("Verify", update.manifest.files.len() as u64, update.manifest.files.len() as u64, t("Verifying downloaded files"));
        operation.check_cancelled()?;
        if retained_modifications { return Ok(None); }
        Ok(Some((update.text.clone(), crate::patch::encode_applied_record_for(&update.report.version, update.seed.unwrap(), update.product.title())?)))
    })?;
    update.report.repaired = update.planned.len(); update.report.verified = committed;
    update.report.summary = if committed {
        tf("{total} files verified; {repaired} files updated. Applied version {version}. Backups retained in .xi-updater/backups.",
            &[("total", &update.report.total), ("repaired", &update.report.repaired), ("version", &update.report.version)])
    } else {
        tf("{repaired} known files updated; {modified} modified files preserved. Applied version was not advanced. Repair can replace modified files with backups.",
            &[("repaired", &update.report.repaired), ("modified", &update.report.modified)])
    };
    Ok(update.report)
}

fn tags() -> polp::Tags {
    polp::Tags {
        platform: *b"W20\0",
        title: *b"0001",
    }
}
fn valid_stamp(s: &str) -> bool {
    s.split_once('_').is_some_and(|(d, r)| {
        d.len() == 8
            && d.starts_with('3')
            && d.bytes().all(|x| x.is_ascii_digit())
            && !r.is_empty()
            && r.bytes().all(|x| x.is_ascii_digit())
    })
}
fn hex_md5(b: &[u8]) -> String {
    format!("{:x}", md5::compute(b))
}

type Ready = (String, PathBuf, FileSig, Option<FileSig>);

fn io_work(sig: FileSig) -> u64 { sig.len.max(1) }
fn applying_work(sig: FileSig, original: Option<FileSig>) -> u64 {
    // Four original-file signature reads plus the backup read/write; two
    // target signature reads plus the replacement copy's read/write.
    original.map(io_work).unwrap_or(0) * 6 + io_work(sig) * 4
}
fn staging_work(history: &FileHistory, item: &Planned) -> u64 {
    let base = if matches!(item.action, Action::Deltas(_)) {
        item.original.map(io_work).unwrap_or(0)
    } else { 0 };
    // Staged output write and its independent signature read, plus a delta base.
    io_work(history.current().sig) * 2 + base
}

/// One stable byte total shared by all download workers. File chunks must not
/// replace aggregate progress with the current file's unrelated denominator.
#[derive(Clone)]
struct Downloads {
    operation: Operation,
    state: Arc<Mutex<DownloadState>>,
}
struct DownloadState {
    files: Vec<(u64, u64)>, // (received, expected), including planned deltas
    done: u64,
    total: u64,
}
struct FileProgress {
    downloads: Downloads,
    index: usize,
    operation: Operation,
}
impl Downloads {
    fn new(operation: Operation, sizes: Vec<u64>) -> Self {
        let total = sizes.iter().sum();
        let downloads = Self {
            operation,
            state: Arc::new(Mutex::new(DownloadState {
                files: sizes.into_iter().map(|size| (0, size)).collect(), done: 0, total,
            })),
        };
        downloads.operation.progress_bytes("Download", 0, total, t("Downloading official files"));
        downloads
    }
    #[cfg(test)]
    fn file(&self, index: usize) -> FileProgress { self.file_scoped(index, self.operation.clone()) }
    fn file_scoped(&self, index: usize, operation: Operation) -> FileProgress {
        FileProgress { downloads: self.clone(), index, operation }
    }
}
impl FileProgress {
    fn advance(&self, bytes: u64, detail: String, reused: bool) {
        let mut state = self.downloads.state.lock().unwrap_or_else(|e| e.into_inner());
        let (received, expected) = state.files[self.index];
        let next = if reused { expected } else { received.saturating_add(bytes).min(expected) };
        state.files[self.index].0 = next;
        state.done += next - received;
        // Publish under this lock so out-of-order workers cannot move the
        // progress backwards after another worker has already advanced it.
        if reused {
            self.operation.progress_bytes_rebased("Download", state.done, state.total, detail);
        } else {
            self.operation.progress_bytes("Download", state.done, state.total, detail);
        }
    }
    fn fallback(&self, full_size: u64, detail: String) {
        let mut state = self.downloads.state.lock().unwrap_or_else(|e| e.into_inner());
        let (received, expected) = state.files[self.index];
        state.files[self.index].1 = received + full_size;
        // Keep transferred bytes, replace only the untransferred delta work.
        state.total = state.total - (expected - received) + full_size;
        self.operation.progress_bytes_rebased("Download", state.done, state.total, detail);
    }
}
fn download_size(history: &FileHistory, action: &Action) -> u64 {
    match action {
        Action::Direct => history.current().direct_len,
        Action::Deltas(from) => history.versions[*from..].iter()
            .filter_map(|version| version.indirect.as_ref()).map(|delta| delta.len).sum(),
    }
}

/// Reserve estimated encoded/decoded buffers, including Vec growth and the
/// direct fallback for a failed delta. Oversized files own the entire budget
/// and run alone; this is a scheduling bound, not a process memory limit.
fn download_memory_mib(history: &FileHistory, action: &Action) -> u32 {
    let output = |sig: FileSig| sig.len.saturating_add(1).saturating_mul(2);
    let current = history.current();
    let mut peak = current.direct_len.saturating_add(output(current.sig));
    if let Action::Deltas(from) = action {
        let mut base = history.versions[from - 1].sig;
        for version in &history.versions[*from..] {
            let raw = version.indirect.as_ref().map_or(0, |delta| delta.len);
            peak = peak.max(output(base).saturating_add(raw).saturating_add(output(version.sig)));
            base = version.sig;
        }
    }
    peak.saturating_add(2 * SIGNATURE_BUFFER_LEN as u64)
        .div_ceil(1 << 20).clamp(1, DOWNLOAD_BUDGET_MIB as u64) as u32
}

async fn reserve_download_memory(
    budget: Arc<tokio::sync::Semaphore>,
    amount: u32,
    op: &Operation,
) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    op.check_cancelled()?;
    let acquire = budget.acquire_many_owned(amount);
    tokio::pin!(acquire);
    loop {
        tokio::select! {
            result = &mut acquire => {
                let permit = result.map_err(|e| e.to_string())?;
                op.check_cancelled()?;
                return Ok(permit);
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => op.check_cancelled()?,
        }
    }
}

#[cfg(test)]
async fn stage_plan(
    root: &Path,
    staged: &Path,
    m: &Manifest,
    planned: &[Planned],
    op: &Operation,
) -> Result<(Vec<Ready>, u64), String> {
    let product = Product::Ffxi;
    let downloads = Downloads::new(op.clone(), stage_groups(product, m, planned).iter()
        .map(|g| download_size(&m.files[g.source.index], &g.source.action)).collect());
    stage_plan_shared(product, root, staged, m, planned, op, downloads, 0).await
}

async fn stage_plan_shared(
    product: Product,
    root: &Path,
    staged: &Path,
    m: &Manifest,
    planned: &[Planned],
    op: &Operation,
    downloads: Downloads,
    first: usize,
) -> Result<(Vec<Ready>, u64), String> {
    if planned.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let m = Arc::new(m.clone());
    let planned = Arc::new(stage_groups(product, &m, planned));
    let next = Arc::new(AtomicUsize::new(0));
    let memory = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_BUDGET_MIB));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..planned.len().min(MAX_DOWNLOAD_WORKERS) {
        let root = root.to_path_buf();
        let staged = staged.to_path_buf();
        let op = op.clone();
        let m = m.clone();
        let planned = planned.clone();
        let next = next.clone();
        let downloads = downloads.clone();
        let memory = memory.clone();
        tasks.spawn(async move {
            let mut session = None;
            let mut mine = Vec::new();
            let mut bytes = 0u64;
            loop {
                op.check_cancelled()?;
                let n = next.fetch_add(1, Ordering::Relaxed);
                let Some(group) = planned.get(n) else {
                    break;
                };
                let item = &group.source;
                let history = &m.files[item.index];
                let current = history.current();
                let transfer = downloads.file_scoped(first + n, op.clone());
                let cache = staged.join(format!(
                    "{}-{}.bin",
                    hex_md5(if product == Product::Viewer { current.direct.as_bytes() } else { history.path.as_bytes() }),
                    hex_md5(format!("{:?}", current.sig).as_bytes())
                ));
                ensure_plain(&cache)?;
                if cache.is_file() && signature(&cache, &op)? == current.sig {
                    transfer.advance(
                        0,
                        tf("Reusing verified {path}", &[("path", &history.path)]),
                        true,
                    );
                } else {
                    let _reservation = reserve_download_memory(memory.clone(), download_memory_mib(history, &item.action), &op).await?;
                    if session.is_none() { session = Some(Session::connect_for(product, &op).await?); }
                    let session = session.as_mut().unwrap();
                    let produced = produce(session, &root, history, &item.action, &op, Some(&transfer)).await;
                    let data = match produced {
                        Ok(data) => data,
                        Err(e) if matches!(item.action, Action::Deltas(_)) && !op.cancelled() => {
                            transfer.fallback(
                                current.direct_len,
                                tf(
                                    "Delta failed for {path}; using full file ({error})",
                                    &[("path", &history.path), ("error", &e)],
                                ),
                            );
                            produce(session, &root, history, &Action::Direct, &op, Some(&transfer)).await?
                        }
                        Err(e) => return Err(e),
                    };
                    if FileSig::of(&data) != current.sig {
                        return Err(tf("{path}: downloaded signature mismatch", &[("path", &history.path)]));
                    }
                    op.check_cancelled()?;
                    save(&cache, &data)?;
                    if signature(&cache, &op)? != current.sig {
                        return Err(tf("{path}: staged file did not verify", &[("path", &history.path)]));
                    }
                    bytes = bytes.saturating_add(data.len() as u64);
                }
                for target in &group.targets {
                    let history = &m.files[target.index];
                    op.complete_update_work(UpdateWork::Staging, staging_work(history, target), None);
                    mine.push((target.index, (history.path.clone(), cache.clone(), current.sig, target.original)));
                }
            }
            Ok::<_, String>((mine, bytes))
        });
    }
    let mut ready = Vec::new();
    let mut bytes = 0u64;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok((mut mine, n))) => {
                ready.append(&mut mine);
                bytes = bytes.saturating_add(n);
            }
            failure => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(match failure {
                    Ok(Err(e)) => e,
                    Err(e) => e.to_string(),
                    _ => unreachable!(),
                });
            }
        }
    }
    ready.sort_by_key(|(n, _)| *n);
    Ok((ready.into_iter().map(|(_, item)| item).collect(), bytes))
}

fn validate_manifest(m: &Manifest) -> Result<(), String> {
    validate_manifest_for(Product::Ffxi, m)
}

fn validate_manifest_for(product: Product, m: &Manifest) -> Result<(), String> {
    if m.files.is_empty() {
        return Err(t("Official manifest contains no files").into());
    }
    let mut names = HashSet::new();
    for h in &m.files {
        validate_relative(&h.path)?;
        if h.path.eq_ignore_ascii_case("patch.ver")
            || h.path.to_ascii_lowercase().starts_with(STATE_DIR)
        {
            return Err(t("Manifest conflicts with updater state").into());
        }
        if !names.insert(h.path.to_ascii_lowercase()) {
            return Err(tf("Duplicate manifest path {path}", &[("path", &h.path)]));
        }
        for v in &h.versions {
            if !product.valid_stamp(&v.stamp)
                || v.sig.len > MAX_FILE
                || v.direct_len == 0
                || v.direct_len > MAX_FILE
            {
                return Err(tf("{path}: unsupported version or size", &[("path", &h.path)]));
            }
            validate_relative(&v.direct)?;
            if let Some(d) = &v.indirect {
                validate_relative(&d.path)?;
                if d.len == 0 || d.len > MAX_FILE {
                    return Err(t("Unsupported delta length").into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn scan(
    root: &Path,
    m: &Manifest,
    mode: Mode,
    op: &Operation,
) -> Result<(Report, Vec<Planned>), String> {
    scan_for(Product::Ffxi, root, m, mode, op)
}

fn scan_for(product: Product, root: &Path, m: &Manifest, mode: Mode, op: &Operation) -> Result<(Report, Vec<Planned>), String> {
    op.check_cancelled()?;
    let available = std::thread::available_parallelism().map_or(1, |count| count.get());
    // This snapshot is only a scheduling hint. Check remains read-only and is
    // allowed while playing; mutation guards still run independently of it.
    let game_may_be_running = mode == Mode::Check && ensure_idle(root).is_err();
    scan_with_workers_for(product, root, m, mode, op, file_check_workers(available, game_may_be_running))
}

fn file_check_workers(available: usize, game_may_be_running: bool) -> usize {
    available.clamp(1, if game_may_be_running { ACTIVE_GAME_CHECK_WORKERS } else { MAX_CHECK_WORKERS })
}

#[cfg(test)]
fn scan_with_workers(
    root: &Path,
    m: &Manifest,
    mode: Mode,
    op: &Operation,
    workers: usize,
) -> Result<(Report, Vec<Planned>), String> {
    scan_with_workers_for(Product::Ffxi, root, m, mode, op, workers)
}

fn scan_with_workers_for(product: Product, root: &Path, m: &Manifest, mode: Mode, op: &Operation, workers: usize) -> Result<(Report, Vec<Planned>), String> {
    let signatures = scan_signatures(root, m, op, workers)?;
    plan_from_signatures(product, m, mode, op, signatures)
}

fn plan_from_signatures(product: Product, m: &Manifest, mode: Mode, op: &Operation, signatures: Vec<Option<LocalFile>>) -> Result<(Report, Vec<Planned>), String> {
    let baseline = if product == Product::Ffxi { Some(installer_baseline::load()?) } else { None };
    let mut report = Report {
        total: m.files.len(),
        ..Default::default()
    };
    let mut planned = Vec::new();
    for (index, (h, local)) in m.files.iter().zip(signatures).enumerate() {
        op.check_cancelled()?;
        let matched = local.and_then(|file| h.versions.iter().rposition(|v| v.sig == file.sig));
        if matched == Some(h.versions.len() - 1) {
            report.current += 1;
            continue;
        }
        // Retained patch history omits some bytes from the official full
        // installer. Recognize those only by the pinned per-path full MD5 and
        // length, never by missing patch.ver, timestamps, or a local file list.
        let stock = matched.is_none() && local.is_some_and(|file| baseline.is_some_and(|b|
            b.matches(&h.path, file.sig.len, file.md5)));
        let outdated = matched.is_some() || stock;
        if local.is_none() {
            report.missing += 1;
        } else if outdated {
            report.outdated += 1;
        } else {
            report.modified += 1;
        }
        if report.issues.len() < 200 {
            report.issues.push(format!(
                "{}: {}",
                h.path,
                if local.is_none() {
                    "missing"
                } else if outdated {
                    "outdated"
                } else {
                    "modified"
                }
            ));
        }
        if mode == Mode::Check || (mode == Mode::Update && local.is_some() && !outdated) {
            continue;
        }
        let mut action = Action::Direct;
        if let Some(i) = matched {
            let later = &h.versions[i + 1..];
            let delta_bytes = later.iter().try_fold(0u64, |sum, v| {
                v.indirect.as_ref().and_then(|d| sum.checked_add(d.len))
            });
            if delta_bytes.is_some_and(|n| n < h.current().direct_len) {
                action = Action::Deltas(i + 1);
            }
        }
        planned.push(Planned {
            index,
            action,
            original: local.map(|file| file.sig),
        });
    }
    op.progress(
        "Check",
        m.files.len() as u64,
        m.files.len() as u64,
        t("File verification complete"),
    );
    Ok((report, planned))
}

/// All bytes are still read and checked. Each bounded worker reuses its buffer;
/// the coordinator publishes completed-file progress and preserves manifest order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalFile { sig: FileSig, md5: [u8; 16] }

fn scan_signatures(
    root: &Path,
    m: &Manifest,
    op: &Operation,
    workers: usize,
) -> Result<Vec<Option<LocalFile>>, String> {
    op.check_cancelled()?;
    op.progress("Check", 0, m.files.len() as u64, t("Checking game files"));
    if m.files.is_empty() { return Ok(Vec::new()); }
    // Opt-in timing tests can compare larger pools without increasing the
    // production limit selected by scan.
    #[cfg(test)]
    let limit = MAX_BENCHMARK_CHECK_WORKERS;
    #[cfg(not(test))]
    let limit = MAX_CHECK_WORKERS;
    let workers = workers.clamp(1, limit).min(m.files.len());
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(workers * 2);
        let mut handles = Vec::with_capacity(workers);
        let mut error = None;
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            let stopped = &stopped;
            let spawned = std::thread::Builder::new().name("crystal-file-check".into())
                .spawn_scoped(scope, move || {
                    let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
                    while !stopped.load(Ordering::Acquire) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(history) = m.files.get(index) else { break; };
                        let result = (|| {
                            op.check_cancelled()?;
                            let path = confined(root, &history.path)?;
                            match fs::metadata(&path) {
                                Ok(metadata) if metadata.is_file() =>
                                    local_file_with_buffer(&path, op, &mut buffer).map(Some),
                                Ok(_) => Err(tf("{path} is not a regular file", &[("path", &path.display())])),
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                                Err(e) => Err(format!("{}: {e}", path.display())),
                            }
                        })();
                        if result.is_err() { stopped.store(true, Ordering::Release); }
                        if sender.send((index, result)).is_err() { break; }
                    }
                });
            match spawned {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    stopped.store(true, Ordering::Release);
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        drop(sender);
        let mut signatures = vec![None; m.files.len()];
        let mut done = 0;
        let mut last_progress = Instant::now();
        // Drain even after a failure so a worker in send cannot block a scoped join.
        for (index, result) in receiver {
            match result {
                Ok(signature) => {
                    signatures[index] = Some(signature);
                    done += 1;
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        op.progress("Check", done, m.files.len() as u64, m.files[index].path.clone());
                        last_progress = Instant::now();
                    }
                }
                Err(e) => { if error.is_none() { error = Some(e); } }
            }
        }
        for handle in handles {
            if handle.join().is_err() && error.is_none() {
                error = Some(t("File verification worker failed.").into());
            }
        }
        if let Some(error) = error { return Err(error); }
        op.check_cancelled()?;
        signatures.into_iter().map(|signature| signature
            .ok_or_else(|| t("File verification did not complete.").into())).collect()
    })
}

fn signature(path: &Path, op: &Operation) -> Result<FileSig, String> {
    let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
    signature_with_buffer(path, op, &mut buffer)
}

fn signature_with_buffer(path: &Path, op: &Operation, buffer: &mut [u8]) -> Result<FileSig, String> {
    local_file_with_buffer(path, op, buffer).map(|file| file.sig)
}

fn local_file_with_buffer(path: &Path, op: &Operation, buffer: &mut [u8]) -> Result<LocalFile, String> {
    let started = Instant::now();
    ensure_plain(path)?;
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let signature = read_local_file(file, op, buffer)?;
    // Keep per-file durations: later Apply and Verify still execute serially.
    op.record_disk_io(signature.sig.len.max(1), started.elapsed());
    Ok(signature)
}

#[cfg(test)]
fn read_signature(file: impl Read, op: &Operation, buffer: &mut [u8]) -> Result<FileSig, String> {
    read_local_file(file, op, buffer).map(|file| file.sig)
}

fn read_local_file(mut file: impl Read, op: &Operation, buffer: &mut [u8]) -> Result<LocalFile, String> {
    debug_assert!(!buffer.is_empty());
    let mut hasher = md5::Context::new();
    let mut sum = 0i32;
    let mut len = 0u64;
    loop {
        op.check_cancelled()?;
        let n = file.read(buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        for b in &buffer[..n] {
            sum = sum.wrapping_add(*b as i8 as i32);
        }
        hasher.consume(&buffer[..n]);
        len += n as u64;
    }
    let d = hasher.compute().0;
    Ok(LocalFile { sig: FileSig {
        len,
        byte_sum: sum,
        md5_word: i32::from_le_bytes(d[..4].try_into().unwrap()),
    }, md5: d })
}

struct Session {
    stream: tokio::net::TcpStream,
    product: Product,
}
impl Session {
    async fn connect(op: &Operation) -> Result<Self, String> {
        Self::connect_for(Product::Ffxi, op).await
    }
    async fn connect_for(product: Product, op: &Operation) -> Result<Self, String> {
        let stream = cancel_io(tokio::net::TcpStream::connect(product.server()), op).await?;
        Ok(Self { stream, product })
    }
    async fn exchange(
        &mut self,
        request: &[u8],
        expected: u32,
        op: &Operation,
    ) -> Result<Vec<u8>, String> {
        cancel_io(self.stream.write_all(request), op).await?;
        let mut head = [0u8; 4];
        cancel_io(self.stream.read_exact(&mut head), op).await?;
        let len = polp::declared_len(&head);
        let cap = if expected == 2 {
            16 * 1024 * 1024
        } else {
            128 * 1024
        };
        if !(16..=cap).contains(&len) {
            return Err(tf("Invalid patch frame length {length}", &[("length", &len)]));
        }
        let mut data = vec![0u8; len];
        data[..4].copy_from_slice(&head);
        if expected == polp::kind::MANIFEST {
            read_manifest_body(&mut self.stream, &mut data, op, Duration::from_secs(30)).await?;
        } else {
            cancel_io(self.stream.read_exact(&mut data[4..]), op).await?;
        }
        let p = polp::parse(&data)?;
        if p.kind != expected {
            return Err(tf(
                "Patch server reply {kind}, expected {expected}",
                &[("kind", &p.kind), ("expected", &expected)],
            ));
        }
        Ok(data)
    }
    async fn fetch(&mut self, path: &str, len: u64, op: &Operation) -> Result<Vec<u8>, String> {
        self.fetch_tracked(path, len, op, None).await
    }
    async fn fetch_tracked(&mut self, path: &str, len: u64, op: &Operation, progress: Option<&FileProgress>) -> Result<Vec<u8>, String> {
        if len > MAX_FILE {
            return Err(t("Download exceeds file limit").into());
        }
        let mut data = Vec::with_capacity(len as usize);
        while (data.len() as u64) < len {
            op.check_cancelled()?;
            let offset = data.len() as u32;
            let want = (len - data.len() as u64).min(polp::CHUNK_LEN as u64) as u32;
            let request = polp::file_request(&self.product.tags(), path, offset, want);
            let mut last = String::new();
            let mut received = None;
            for attempt in 0..3 {
                match self.exchange(&request, 4, op).await {
                    Ok(frame) => {
                        received = Some(frame);
                        break;
                    }
                    Err(e) => {
                        last = e;
                        op.check_cancelled()?;
                        if attempt < 2 {
                            *self = Self::connect_for(self.product, op).await?;
                        }
                    }
                }
            }
            let frame = received.ok_or_else(|| tf("{path}: {error}", &[("path", &path), ("error", &last)]))?;
            let body = &frame[16..];
            let (got, chunk) = polp::file_chunk(body)?;
            let name_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
            let echoed = body
                .get(12..12 + name_len)
                .ok_or(t("Invalid echoed patch path"))?;
            if got != offset
                || chunk.len() != want as usize
                || echoed != [path.as_bytes(), &[0]].concat()
                || body.len() != 12 + name_len + chunk.len()
            {
                return Err(t("Patch chunk path, offset, or size mismatch").into());
            }
            data.extend_from_slice(chunk);
            if let Some(progress) = progress {
                progress.advance(chunk.len() as u64, path.to_owned(), false);
            } else {
                op.progress_bytes("Download", data.len() as u64, len, path.to_owned());
            }
        }
        Ok(data)
    }
}
// A manifest can take longer than 30 seconds while bytes keep arriving. Limit
// inactivity between reads, not the duration of the entire bounded frame.
async fn read_manifest_body<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    data: &mut [u8],
    op: &Operation,
    idle_limit: Duration,
) -> Result<(), String> {
    let mut received = 4;
    let detail = t("Downloading the official file manifest");
    op.progress_bytes("Manifest", received as u64, data.len() as u64, detail);
    while received < data.len() {
        op.check_cancelled()?;
        let end = (received + 64 * 1024).min(data.len());
        let count = cancel_io_with_timeout(stream.read(&mut data[received..end]), op, idle_limit).await?;
        if count == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string());
        }
        received += count;
        op.progress_bytes("Manifest", received as u64, data.len() as u64, detail);
    }
    Ok(())
}

async fn cancel_io<F, T>(future: F, op: &Operation) -> Result<T, String>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    cancel_io_with_timeout(future, op, Duration::from_secs(30)).await
}

async fn cancel_io_with_timeout<F, T>(future: F, op: &Operation, timeout: Duration) -> Result<T, String>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    tokio::pin!(future);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {result=&mut future=>return result.map_err(|e|e.to_string()),_=&mut deadline=>return Err(t("Patch server timed out").into()),_=tokio::time::sleep(Duration::from_millis(100))=>op.check_cancelled()?,}
    }
}

fn decode_payload(data: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let (&flag, body) = data.split_first().ok_or(t("Empty compressed payload"))?;
    let out = match flag {
        1 => {
            if body.len() > max {
                return Err(t("Stored payload exceeds expected size").into());
            }
            body.to_vec()
        }
        3 => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(body)
                .take(max as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            out
        }
        0 | 2 => lz::decode_direct_bounded(body, max)?,
        _ => return Err(tf("Unsupported patch compression flag {flag}", &[("flag", &flag)])),
    };
    if out.len() > max {
        return Err(t("Decoded payload exceeds expected size").into());
    }
    Ok(out)
}
async fn produce(
    session: &mut Session,
    root: &Path,
    h: &FileHistory,
    action: &Action,
    op: &Operation,
    progress: Option<&FileProgress>,
) -> Result<Vec<u8>, String> {
    match action {
        Action::Direct => {
            let raw = session
                .fetch_tracked(&h.current().direct, h.current().direct_len, op, progress)
                .await?;
            decode_payload(&raw, h.current().sig.len as usize)
        }
        Action::Deltas(from) => {
            let mut data = read_delta_base(&confined(root, &h.path)?, h.versions[from - 1].sig, op)?;
            for v in &h.versions[*from..] {
                op.check_cancelled()?;
                let d = v.indirect.as_ref().ok_or(t("Missing delta"))?;
                let raw = session.fetch_tracked(&d.path, d.len, op, progress).await?;
                data = lz::apply_indirect_bounded(&raw, &data, v.sig.len as usize)?;
                if FileSig::of(&data) != v.sig {
                    return Err(tf("{path}: delta signature mismatch", &[("path", &h.path)]));
                }
            }
            Ok(data)
        }
    }
}

fn read_delta_base(path: &Path, expected: FileSig, op: &Operation) -> Result<Vec<u8>, String> {
    op.check_cancelled()?;
    ensure_plain(path)?;
    let input = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::with_capacity(expected.len as usize);
    // Read one extra byte to reject growth without reading an arbitrarily
    // enlarged file after the scan selected its old canonical signature.
    input.take(expected.len.saturating_add(1)).read_to_end(&mut data).map_err(|e| e.to_string())?;
    op.check_cancelled()?;
    if FileSig::of(&data) != expected {
        return Err(t("Delta base changed after verification").into());
    }
    Ok(data)
}

fn validate_relative(path: &str) -> Result<(), String> {
    let unsafe_path = || tf("Unsafe patch path {path}", &[("path", &format!("{path:?}"))]);
    if path.is_empty()
        || path.contains('\\')
        || path.bytes().any(|b| b < 32)
        || path.contains([':', '<', '>', '"', '|', '?', '*'])
    {
        return Err(unsafe_path());
    }
    for p in path.split('/') {
        let stem = p.split('.').next().unwrap_or("").to_ascii_uppercase();
        if p.is_empty()
            || p == "."
            || p == ".."
            || p.ends_with(['.', ' '])
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(unsafe_path());
        }
    }
    if Path::new(path)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(unsafe_path());
    }
    Ok(())
}
pub(crate) fn ensure_plain(path: &Path) -> Result<(), String> {
    let mut at = PathBuf::new();
    for part in path.components() {
        at.push(part);
        // A Windows verbatim drive prefix (`\\?\C:`) is not a directory.
        // Keep it in the path, then inspect only after RootDir has completed it.
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&at) {
            Ok(m) => {
                if m.file_type().is_symlink() {
                    return Err(tf("Symlink path {path}", &[("path", &at.display())]));
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if m.file_attributes() & 0x400 != 0 {
                        return Err(tf("Reparse path {path}", &[("path", &at.display())]));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", at.display())),
        }
    }
    Ok(())
}
fn confined(root: &Path, rel: &str) -> Result<PathBuf, String> {
    validate_relative(rel)?;
    let p = root.join(rel);
    ensure_plain(&p)?;
    Ok(p)
}
fn save(path: &Path, data: &[u8]) -> Result<(), String> {
    ensure_plain(path)?;
    let tmp = path.with_extension("xi-part");
    ensure_plain(&tmp)?;
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    ensure_plain(path)?;
    publish(&tmp, path)
}

/// Publish flushed bytes without an unflushed Windows rename between a
/// recovered file and deletion of its sole transaction journal.
fn publish(from: &Path, to: &Path) -> Result<(), String> {
    ensure_plain(from)?;
    ensure_plain(to)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: both NUL-terminated path buffers remain live for this call.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(tf(
                "Publish {path}: {error}",
                &[("path", &to.display()), ("error", &std::io::Error::last_os_error())],
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to).map_err(|e| e.to_string())?;
        if let Some(parent) = to.parent() {
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Journal {
    id: String,
    committed: bool,
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    backup: Option<String>,
    // Older journals did not record the backup identity. Preserve recovery
    // compatibility, while checking every backup written by this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original: Option<Sig>,
    expected: Sig,
}
#[derive(Serialize, Deserialize)]
struct Sig {
    len: u64,
    sum: i32,
    word: i32,
}
impl From<FileSig> for Sig {
    fn from(s: FileSig) -> Self {
        Self {
            len: s.len,
            sum: s.byte_sum,
            word: s.md5_word,
        }
    }
}
impl Sig {
    fn matches(&self, s: FileSig) -> bool {
        self.len == s.len && self.sum == s.byte_sum && self.word == s.md5_word
    }
}
fn write_journal(root: &Path, j: &Journal) -> Result<(), String> {
    save(
        &confined(root, &format!("{STATE_DIR}/transaction.json"))?,
        &serde_json::to_vec(j).map_err(|e| e.to_string())?,
    )
}
fn apply<F>(
    root: &Path,
    ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
    op: &Operation,
    verify: F,
) -> Result<bool, String>
where
    F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
{
    apply_with_guard(root, ready, op, verify, ensure_idle)
}

/// Transaction mechanics share an internal liveness seam with isolated fixture
/// tests. Every production call goes through `apply` and its real process guard.
fn apply_with_guard<F, G>(
    root: &Path,
    ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
    op: &Operation,
    verify: F,
    guard: G,
) -> Result<bool, String>
where
    F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
    G: Fn(&Path) -> Result<(), String>,
{
    let id = format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos(),
        std::process::id()
    );
    let backup_dir = confined(root, &format!("{STATE_DIR}/backups/{id}"))?;
    fs::create_dir_all(&backup_dir).map_err(|e| e.to_string())?;
    let mut j = Journal {
        id,
        committed: false,
        entries: Vec::new(),
    };
    write_journal(root, &j)?;
    let result = (|| {
        for (i, (path, cache, sig, original)) in ready.iter().enumerate() {
            let file_started = Instant::now();
            op.progress("Apply", i as u64, ready.len() as u64, path.clone());
            op.check_cancelled()?;
            guard(root)?;
            let dest = confined(root, path)?;
            let now = if dest.is_file() {
                Some(signature(&dest, op)?)
            } else {
                None
            };
            if now != *original {
                return Err(tf("{path} changed since the check; retry", &[("path", path)]));
            }
            replace(root, &mut j, path, cache, *sig, op)?;
            op.complete_update_work(UpdateWork::Apply, applying_work(*sig, *original), Some(file_started.elapsed()));
        }
        op.progress("Apply", ready.len() as u64, ready.len() as u64, t("Applying files"));
        let metadata = verify()?;
        if let Some((cfg, ver)) = &metadata {
            for (name, data) in [("patch.cfg", cfg), ("patch.ver", ver)] {
                op.check_cancelled()?;
                guard(root)?;
                let tmp = confined(root, &format!("{STATE_DIR}/staged/{name}"))?;
                save(&tmp, data)?;
                replace(root, &mut j, name, &tmp, FileSig::of(data), op)?;
            }
        }
        op.check_cancelled()?;
        j.committed = true;
        save(
            &backup_dir.join("records.json"),
            &serde_json::to_vec(&j).map_err(|e| e.to_string())?,
        )?;
        write_journal(root, &j)?;
        fs::remove_file(confined(root, &format!("{STATE_DIR}/transaction.json"))?)
            .map_err(|e| e.to_string())?;
        Ok(metadata.is_some())
    })();
    if let Err(e) = result {
        // Rollback ignores user cancellation but preserves the recorded backups.
        return match recover_with_guard(root, &guard) {
            Ok(()) => Err(tf(
                "{error}. Transaction recovered; verified downloads retained.",
                &[("error", &e)],
            )),
            Err(r) => Err(tf(
                "{error}. Recovery required: {recovery}. Backups and transaction retained.",
                &[("error", &e), ("recovery", &r)],
            )),
        };
    }
    result
}
fn replace(
    root: &Path,
    j: &mut Journal,
    path: &str,
    cache: &Path,
    sig: FileSig,
    op: &Operation,
) -> Result<(), String> {
    let dest = confined(root, path)?;
    ensure_plain(cache)?;
    if signature(cache, op)? != sig {
        return Err(tf("{path}: staged content changed", &[("path", &path)]));
    }
    let (backup, original) = if dest.exists() {
        let rel = format!("{STATE_DIR}/backups/{}/{}.bin", j.id, j.entries.len());
        let p = confined(root, &rel)?;
        let original = signature(&dest, op)?;
        fs::copy(&dest, &p).map_err(|e| e.to_string())?;
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        if signature(&p, op)? != original || signature(&dest, op)? != original {
            return Err(tf("{path}: original changed while backing up; retry", &[("path", &path)]));
        }
        (Some(rel), Some(original.into()))
    } else {
        (None, None)
    };
    j.entries.push(Entry {
        path: path.to_string(),
        backup,
        original,
        expected: sig.into(),
    });
    write_journal(root, j)?;
    if let Some(parent) = dest.parent() {
        ensure_plain(parent)?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = dest.with_extension(format!("xi-{}", j.id));
    ensure_plain(&tmp)?;
    if tmp.exists() {
        return Err(t("Replacement temporary path already exists").into());
    }
    fs::copy(cache, &tmp).map_err(|e| e.to_string())?;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    op.check_cancelled()?;
    ensure_plain(&dest)?;
    publish(&tmp, &dest)?;
    if signature(&dest, op)? != sig {
        return Err(tf("{path}: replacement did not verify", &[("path", &path)]));
    }
    Ok(())
}
fn recover(root: &Path) -> Result<(), String> {
    recover_with_guard(root, ensure_idle)
}

fn recover_with_guard(root: &Path, guard: impl Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let path = confined(root, &format!("{STATE_DIR}/transaction.json"))?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(t("Updater transaction is unexpectedly large").into());
    }
    let j: Journal = serde_json::from_slice(&bytes)
        .map_err(|e| tf("Invalid updater transaction: {error}", &[("error", &e)]))?;
    if j.id.is_empty() || !j.id.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Err(t("Invalid updater transaction identity").into());
    }
    if !j.committed {
        for entry in j.entries.iter().rev() {
            guard(root)?;
            let dest = confined(root, &entry.path)?;
            if let Some(backup) = &entry.backup {
                if !backup.starts_with(&format!("{STATE_DIR}/backups/{}/", j.id)) {
                    return Err(t("Invalid backup path in transaction").into());
                }
                let source = confined(root, backup)?;
                if !source.is_file() {
                    return Err(tf("Missing recovery backup {backup}", &[("backup", backup)]));
                }
                let restore_sig = FileSig::of_path(&source)?;
                if entry.original.as_ref().is_some_and(|original| !original.matches(restore_sig)) {
                    return Err(tf("{path}: recovery backup changed; backups and transaction were retained",
                        &[("path", &entry.path)]));
                }
                if dest.is_file() {
                    let s = FileSig::of_path(&dest)?;
                    if !entry.expected.matches(s) && s != restore_sig {
                        return Err(tf(
                            "{path} changed after interruption; preserve it before recovery",
                            &[("path", &entry.path)],
                        ));
                    }
                }
                let tmp = dest.with_extension("xi-recover");
                ensure_plain(&tmp)?;
                fs::copy(&source, &tmp).map_err(|e| e.to_string())?;
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(&tmp)
                    .map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
                drop(file);
                if FileSig::of_path(&tmp)? != restore_sig {
                    return Err(tf("{path}: recovery copy did not verify", &[("path", &entry.path)]));
                }
                guard(root)?;
                publish(&tmp, &dest)?;
                if FileSig::of_path(&dest)? != restore_sig {
                    return Err(tf("{path}: restored file did not verify", &[("path", &entry.path)]));
                }
            } else if dest.exists() {
                if !entry.expected.matches(FileSig::of_path(&dest)?) {
                    return Err(tf("{path} changed after interruption", &[("path", &entry.path)]));
                }
                guard(root)?;
                fs::remove_file(&dest).map_err(|e| e.to_string())?;
            }
            let leftover = dest.with_extension(format!("xi-{}", j.id));
            ensure_plain(&leftover)?;
            if leftover.exists() {
                fs::remove_file(leftover).map_err(|e| e.to_string())?;
            }
        }
    }
    fs::remove_file(path).map_err(|e| e.to_string())
}

#[cfg(windows)]
pub(crate) fn ensure_idle(root: &Path) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
        MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(t("Cannot verify that the game is stopped").into());
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry);
        if more == 0 {
            CloseHandle(snapshot);
            return Err(t("Cannot verify that the game is stopped").into());
        }
        let mut busy = false;
        let mut incomplete = false;
        while more != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
            if name.starts_with("host64-") || name.starts_with("host32-") {
                busy = true;
            }
            if name == "pol.exe"
                || name == "startpol.exe"
                || name == "crystal-loader.exe"
                || name.contains("ffxi")
                || name.starts_with("xi_recomp")
                || name.starts_with("xi-client")
            {
                let process =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                if process.is_null() {
                    busy = true;
                } else {
                    let mut buf = [0u16; 32768];
                    let mut n = buf.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut n) == 0 {
                        busy = true;
                    } else {
                        let exe = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
                        busy |= under_root(&exe, root);
                    }
                    CloseHandle(process);
                }
                let modules = CreateToolhelp32Snapshot(
                    TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32,
                    entry.th32ProcessID,
                );
                if modules == INVALID_HANDLE_VALUE {
                    busy = true;
                } else {
                    let mut module: MODULEENTRY32W = std::mem::zeroed();
                    module.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
                    let mut found = Module32FirstW(modules, &mut module);
                    if found == 0 { incomplete = true; }
                    while found != 0 {
                        let end = module
                            .szExePath
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(module.szExePath.len());
                        let path =
                            PathBuf::from(String::from_utf16_lossy(&module.szExePath[..end]));
                        if under_root(&path, root) {
                            busy = true;
                            break;
                        }
                        found = Module32NextW(modules, &mut module);
                        if found == 0 && GetLastError() != ERROR_NO_MORE_FILES { incomplete = true; }
                    }
                    CloseHandle(modules);
                }
            }
            if busy || incomplete {
                break;
            }
            more = Process32NextW(snapshot, &mut entry);
            if more == 0 && GetLastError() != ERROR_NO_MORE_FILES { incomplete = true; }
        }
        CloseHandle(snapshot);
        if incomplete {
            return Err(t("Cannot verify that the game is stopped").into());
        }
        if busy {
            return Err(t("Close the game using this install before updating files").into());
        }
    }
    Ok(())
}
#[cfg(windows)]
fn under_root(path: &Path, root: &Path) -> bool {
    let normalized = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_ascii_lowercase()
    };
    let root = normalized(root);
    let path = normalized(path);
    path == root || path.starts_with(&(root + "\\"))
}
#[cfg(not(windows))]
pub(crate) fn ensure_idle(_root: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_budget_covers_delta_peaks_and_full_file_fallback() {
        let mib = 1 << 20;
        let version = |decoded: u64, encoded: u64, delta: Option<u64>| manifest::Version {
            stamp: "30260904_1".into(),
            sig: FileSig { len: decoded, byte_sum: 0, md5_word: 0 },
            direct: "30260904_1/Direct/test.DAT.slc".into(), direct_len: encoded,
            indirect: delta.map(|len| manifest::Delta { path: "30260904_1/Indirect/test.DAT.olc".into(), len }),
        };
        let mut history = FileHistory { path: "test.DAT".into(), optional: false, versions: vec![version(32 * mib, mib, None)] };
        assert_eq!(download_memory_mib(&history, &Action::Direct), 68);
        history.versions = vec![version(128 * mib, mib, None), version(8 * mib, 5 * mib, Some(mib))];
        assert_eq!(download_memory_mib(&history, &Action::Direct), 24);
        assert_eq!(download_memory_mib(&history, &Action::Deltas(1)), 276);
        // A tiny delta can need a larger direct fallback under the same permit.
        history.versions = vec![version(mib, mib, None), version(2 * mib, 80 * mib, Some(100))];
        assert_eq!(download_memory_mib(&history, &Action::Deltas(1)), 87);
        // The largest intermediate version must survive later smaller outputs.
        history.versions = vec![version(8 * mib, mib, None), version(64 * mib, mib, Some(2 * mib)), version(2 * mib, mib, Some(mib))];
        assert_eq!(download_memory_mib(&history, &Action::Deltas(1)), 149);
        history.versions = vec![version(MAX_FILE, MAX_FILE, None)];
        assert_eq!(download_memory_mib(&history, &Action::Direct), DOWNLOAD_BUDGET_MIB as u32);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_download_memory_reservation() {
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let held = budget.clone().acquire_owned().await.unwrap();
        let op = Operation::new();
        let waiting_op = op.clone();
        let waiting_budget = budget.clone();
        let waiting = tokio::spawn(async move { reserve_download_memory(waiting_budget, 1, &waiting_op).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        op.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap().unwrap_err();
        assert!(error.contains("Cancelled"));
        assert_eq!(budget.available_permits(), 0);
        drop(held);
        assert_eq!(budget.available_permits(), 1);
    }

    #[test]
    fn delta_base_reads_require_the_exact_previous_signature() {
        let root = fixture("bounded-delta-base");
        let path = root.join("base.DAT");
        let expected = FileSig::of(b"old");
        fs::write(&path, b"old").unwrap();
        assert_eq!(read_delta_base(&path, expected, &Operation::new()).unwrap(), b"old");
        let mut enlarged = vec![0; 2 * SIGNATURE_BUFFER_LEN];
        enlarged[..3].copy_from_slice(b"old");
        for data in [b"bad".as_slice(), b"ol".as_slice(), enlarged.as_slice()] {
            fs::write(&path, data).unwrap();
            assert!(read_delta_base(&path, expected, &Operation::new()).unwrap_err().contains("Delta base changed"));
        }
        let cancelled = Operation::new();
        cancelled.cancel();
        assert!(read_delta_base(&path, expected, &cancelled).unwrap_err().contains("Cancelled"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cache_only_staging_reuses_verified_files_without_a_server_connection() {
        let root = fixture("offline-staging-cache");
        let staged = root.join("staged");
        fs::create_dir(&staged).unwrap();
        let mut manifest = Manifest::default();
        let mut planned = Vec::new();
        for index in 0..24 {
            let path = format!("{index}.DAT");
            let data = format!("official cached bytes {index}").into_bytes();
            let sig = FileSig::of(&data);
            let history = FileHistory { path: path.clone(), optional: false, versions: vec![manifest::Version {
                stamp: "30260904_1".into(), sig,
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: data.len() as u64 + 1, indirect: None,
            }] };
            let cache = staged.join(format!("{}-{}.bin", hex_md5(path.as_bytes()), hex_md5(format!("{sig:?}").as_bytes())));
            fs::write(&cache, &data).unwrap();
            manifest.files.push(history);
            planned.push(Planned { index, action: Action::Direct, original: None });
        }
        let expected_total = planned.iter().map(|p| download_size(&manifest.files[p.index], &p.action)).sum();
        let op = Operation::new();
        let (ready, bytes) = stage_plan(&root, &staged, &manifest, &planned, &op).await.unwrap();
        assert_eq!(bytes, 0);
        assert_eq!(ready.len(), planned.len());
        for (index, (path, cache, sig, original)) in ready.iter().enumerate() {
            assert_eq!(path, &manifest.files[index].path);
            assert_eq!(*sig, FileSig::of(&fs::read(cache).unwrap()));
            assert_eq!(*original, None);
        }
        assert_eq!((op.snapshot().done, op.snapshot().total), (expected_total, expected_total));
        assert!(!root.join("patch.ver").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interleaved_file_chunks_keep_the_whole_download_denominator() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![100, 300]);
        let first = downloads.file(0);
        let second = downloads.file(1);
        first.advance(20, "first".into(), false);
        second.advance(80, "second".into(), false);
        let progress = op.snapshot();
        assert_eq!((progress.done, progress.total), (100, 400));
        assert_eq!(progress.remaining_percent(), Some(75));
        assert!(progress.bytes);
        first.advance(80, "first".into(), false);
        assert_eq!((op.snapshot().done, op.snapshot().total), (180, 400));
        second.advance(220, "second".into(), false);
        assert_eq!(op.snapshot().remaining_percent(), Some(0));
    }

    #[test]
    fn six_download_workers_publish_all_of_their_progress() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![1000; 6]);
        std::thread::scope(|scope| {
            for index in 0..6 {
                let file = downloads.file(index);
                let op = op.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        file.advance(10, format!("file {index}"), false);
                        let progress = op.snapshot();
                        assert_eq!(progress.total, 6000);
                        assert!(progress.done <= progress.total);
                    }
                });
            }
        });
        assert_eq!((op.snapshot().done, op.snapshot().total), (6000, 6000));
    }

    #[test]
    fn full_file_fallback_and_verified_cache_keep_correct_remaining_bytes() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![100, 200]);
        let delta = downloads.file(0);
        delta.advance(30, "delta".into(), false);
        delta.fallback(90, "full file".into());
        assert_eq!((op.snapshot().done, op.snapshot().total), (30, 320));
        delta.advance(90, "full file".into(), false);
        assert_eq!((op.snapshot().done, op.snapshot().total), (120, 320));
        downloads.file(1).advance(0, "verified cache".into(), true);
        assert_eq!((op.snapshot().done, op.snapshot().total), (320, 320));
        assert_eq!(op.snapshot().remaining, Some(Duration::ZERO));
    }

    #[tokio::test]
    async fn manifest_body_accepts_progress_beyond_one_io_timeout() {
        let expected = polp::seal(polp::kind::MANIFEST, &[1; 252]);
        let mut data = vec![0; expected.len()];
        data[..4].copy_from_slice(&expected[..4]);
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let op = Operation::new();
        let idle_limit = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let send = async {
            let mut sent = 4;
            for chunk in expected[4..].chunks(32) {
                tokio::time::sleep(Duration::from_millis(60)).await;
                writer.write_all(chunk).await.unwrap();
                sent += chunk.len();
                // Observe progress before supplying the next chunk, as the UI does.
                while op.snapshot().done < sent as u64 {
                    tokio::task::yield_now().await;
                }
                let progress = op.snapshot();
                assert_eq!(progress.stage, "Manifest");
                assert_eq!(progress.total, expected.len() as u64);
            }
        };
        let received = read_manifest_body(&mut reader, &mut data, &op, idle_limit);
        let ((), result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(send, received)
        }).await.expect("the progressing manifest must complete");
        result.unwrap();
        assert!(started.elapsed() > idle_limit);
        assert_eq!(data, expected);
        assert_eq!(polp::parse(&data).unwrap().kind, polp::kind::MANIFEST);
    }

    #[tokio::test]
    async fn manifest_body_still_stops_on_idle_eof_and_cancellation() {
        let (mut reader, writer) = tokio::io::duplex(64);
        let mut data = [0; 32];
        let op = Operation::new();
        let error = read_manifest_body(&mut reader, &mut data, &op, Duration::from_millis(40)).await.unwrap_err();
        assert_eq!(error, t("Patch server timed out"));
        assert_eq!(op.snapshot().done, 4);
        drop(writer);
        let error = read_manifest_body(&mut reader, &mut data, &op, Duration::from_secs(30)).await.unwrap_err();
        assert_eq!(error, std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string());
        let (mut reader, _writer) = tokio::io::duplex(64);
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                op.cancel();
            };
            let (_, result) = tokio::join!(cancel,
                read_manifest_body(&mut reader, &mut data, &op, Duration::from_secs(30)));
            result
        }).await;
        assert!(result.expect("cancellation must interrupt the pending read").is_err());
    }

    fn fixture_apply<F>(
        root: &Path,
        ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
        op: &Operation,
        verify: F,
    ) -> Result<bool, String>
    where
        F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
    {
        // These fixtures have no game process. Other suite tests may start a
        // host64 self-test process that deliberately trips production liveness.
        assert!(root.starts_with(std::env::temp_dir()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("xi-updater-"));
        apply_with_guard(root, ready, op, verify, |_| Ok(()))
    }
    #[test]
    fn unsafe_paths_are_rejected() {
        for path in [
            "../x", "/x", "C:/x", "a/../x", "a:b", "NUL.dat", "a\\b", "a/", "a.", "CON",
        ] {
            assert!(validate_relative(path).is_err(), "{path}");
        }
        assert!(validate_relative("Tools/FINAL FANTASY XI Config.chm").is_ok());
    }
    #[test]
    fn payload_limits_and_modes() {
        assert_eq!(decode_payload(&[1, 2, 3], 2).unwrap(), [2, 3]);
        assert!(decode_payload(&[1, 2, 3], 1).is_err());
        assert!(decode_payload(&[4, 0], 10).is_err());
    }
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "xi-updater-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".xi-updater/staged")).unwrap();
        root
    }
    #[test]
    fn canonicalized_temp_root_can_be_confined_and_checked() {
        let root = fixture("canonical-root");
        let canonical = fs::canonicalize(&root).unwrap();
        ensure_plain(&canonical).unwrap();
        let path = confined(&canonical, "ROM/1/test.DAT").unwrap();
        assert!(path.starts_with(&canonical));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"fixture bytes").unwrap();
        ensure_plain(&path).unwrap();
        assert_eq!(
            signature(&path, &Operation::new()).unwrap(),
            FileSig::of(b"fixture bytes")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_validation_rejects_unrelated_and_other_product_folders() {
        let root = fixture("product-selection");
        assert!(validate_installation(Product::Ffxi, &root).is_err());
        assert!(validate_installation(Product::Viewer, &root).is_err());
        fs::write(root.join("pol.exe"), b"damaged viewer fixture").unwrap();
        assert!(validate_installation(Product::Viewer, &root).is_ok());
        assert!(validate_installation(Product::Ffxi, &root).is_err());
        fs::remove_file(root.join("pol.exe")).unwrap();
        fs::create_dir_all(root.join("ROM/0")).unwrap();
        fs::write(root.join("ROM/0/0.DAT"), b"surviving game data").unwrap();
        assert!(validate_installation(Product::Ffxi, &root).is_ok());
        assert!(validate_installation(Product::Viewer, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_waits_for_a_game_started_after_replacement() {
        let root = fixture("busy-recovery");
        let old = b"original";
        let new = b"replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![("game.dat".into(), cache.clone(), FileSig::of(new), Some(FileSig::of(old)))];
        let running = std::cell::Cell::new(false);
        let guard = |_: &Path| if running.get() { Err("fixture game started".into()) } else { Ok(()) };
        let error = apply_with_guard(&root, &ready, &Operation::new(), || {
            running.set(true);
            Err("fixture verification interrupted".into())
        }, guard).unwrap_err();
        assert!(error.contains("Recovery required") && error.contains("fixture game started"), "{error}");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        assert_eq!(fs::read(&cache).unwrap(), new);
        assert!(root.join(".xi-updater/transaction.json").is_file());
        assert!(!root.join("patch.ver").exists());
        running.set(false);
        recover_with_guard(&root, guard).unwrap();
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_a_changed_backup_without_replacing_the_new_file() {
        let root = fixture("backup-integrity");
        let old = b"original";
        let new = b"replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![("game.dat".into(), cache.clone(), FileSig::of(new), Some(FileSig::of(old)))];
        let journal = root.join(".xi-updater/transaction.json");
        let error = fixture_apply(&root, &ready, &Operation::new(), || {
            let recorded: Journal = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            let entry = &recorded.entries[0];
            assert!(entry.original.as_ref().unwrap().matches(FileSig::of(old)));
            fs::write(root.join(entry.backup.as_ref().unwrap()), b"corrupt!").unwrap();
            Err("fixture verification interrupted".into())
        }).unwrap_err();
        assert!(error.contains("recovery backup changed"), "{error}");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        assert!(journal.is_file());
        // An older on-disk journal remains recoverable after a launcher update.
        let mut recorded: serde_json::Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        let backup = recorded["entries"][0]["backup"].as_str().unwrap();
        fs::write(root.join(backup), old).unwrap();
        recorded["entries"][0].as_object_mut().unwrap().remove("original");
        fs::write(&journal, serde_json::to_vec(&recorded).unwrap()).unwrap();
        recover_with_guard(&root, |_| Ok(())).unwrap();
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!journal.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_after_replacement_restores_original_and_keeps_download() {
        let root = fixture("cancel");
        let op = Operation::new();
        op.begin_update();
        let old = b"old original";
        let new = b"verified replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache.clone(),
            FileSig::of(new),
            Some(FileSig::of(old)),
        )];
        let result = fixture_apply(&root, &ready, &op, || {
            assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
            op.cancel();
            Ok(None)
        });
        assert!(result.is_err());
        assert!(!op.snapshot().overall.unwrap().complete, "a rolled-back update must not report completion");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!root.join("patch.ver").exists());
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_liveness_refusal_preserves_files_and_applied_metadata() {
        let root = fixture("liveness");
        let op = Operation::new();
        let original = b"original";
        let update = b"update";
        fs::write(root.join("game.dat"), original).unwrap();
        let cache = root.join(".xi-updater/staged/update.bin");
        fs::write(&cache, update).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache,
            FileSig::of(update),
            Some(FileSig::of(original)),
        )];
        let result = apply_with_guard(
            &root,
            &ready,
            &op,
            || panic!("Refused transaction must not reach verification"),
            |_| Err("fixture liveness refusal".into()),
        );
        assert!(result.unwrap_err().contains("fixture liveness refusal"));
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), original);
        assert!(!root.join("patch.ver").exists());
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn successful_transaction_retains_original_backup_and_commits_metadata_last() {
        let root = fixture("commit");
        let op = Operation::new();
        op.begin_update();
        let old = b"old";
        let new = b"new";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache,
            FileSig::of(new),
            Some(FileSig::of(old)),
        )];
        let encoded = crate::patch::encode_applied_record("30260904_1", 1).unwrap();
        assert!(fixture_apply(&root, &ready, &op, || {
            assert!(!root.join("patch.ver").exists());
            assert!(!op.snapshot().overall.unwrap().complete);
            Ok(Some((b"end\n".to_vec(), encoded.clone())))
        })
        .unwrap());
        assert_eq!(fs::read(root.join("patch.ver")).unwrap(), encoded);
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        let backup = fs::read_dir(root.join(".xi-updater/backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("0.bin");
        assert_eq!(fs::read(backup).unwrap(), old);
        assert!(!op.snapshot().overall.unwrap().complete, "the full run declares completion after transaction return");
        op.finish_update();
        assert!(op.snapshot().overall.unwrap().complete);
        assert_eq!(op.snapshot().overall.unwrap().remaining_percent(), Some(0));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_update_preserves_unknown_modification_but_repair_plans_it() {
        let root = fixture("modified");
        fs::write(root.join("f.dat"), b"custom").unwrap();
        let m = Manifest {
            files: vec![FileHistory {
                optional: false,
                path: "f.dat".into(),
                versions: vec![manifest::Version {
                    stamp: "30260904_1".into(),
                    sig: FileSig::of(b"official"),
                    direct: "30260904_1/Direct/f.dat.slc".into(),
                    direct_len: 9,
                    indirect: None,
                }],
            }],
        };
        let (report, plan) = scan(&root, &m, Mode::Update, &Operation::new()).unwrap();
        assert_eq!(report.modified, 1);
        assert!(plan.is_empty());
        assert_eq!(
            scan(&root, &m, Mode::Repair, &Operation::new())
                .unwrap()
                .1
                .len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "read-only timing run; requires CRYSTAL_FILE_CHECK_BENCH_INSTALL with a saved patch.cfg"]
    fn read_only_file_check_benchmark() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_FILE_CHECK_BENCH_INSTALL")
            .expect("Set CRYSTAL_FILE_CHECK_BENCH_INSTALL to the installation to read"));
        assert!(root.is_absolute());
        ensure_plain(&root).unwrap();
        let m = manifest::parse(&fs::read_to_string(root.join("patch.cfg")).unwrap()).unwrap();
        validate_manifest(&m).unwrap();
        assert!(!m.files.is_empty());
        let bytes: u64 = m.files.iter().map(|h| h.current().sig.len).sum();
        let mut reference = None;
        let workers: Vec<usize> = std::env::var("CRYSTAL_FILE_CHECK_BENCH_WORKERS")
            .unwrap_or_else(|_| format!("{MAX_CHECK_WORKERS},{MAX_CHECK_WORKERS}")).split(',')
            .map(|count| count.parse::<usize>().expect("worker counts must be integers"))
            .collect();
        assert!(!workers.is_empty() && workers.iter().all(|count| (1..=MAX_BENCHMARK_CHECK_WORKERS).contains(count)));
        for (repeat, workers) in workers.into_iter().enumerate() {
            let started = Instant::now();
            let (report, planned) = scan_with_workers(&root, &m, Mode::Check, &Operation::new(), workers).unwrap();
            assert!(planned.is_empty());
            if let Some(reference) = &reference { assert_eq!(&report, reference); }
            else { reference = Some(report.clone()); }
            eprintln!("file-check benchmark repeat={} workers={workers} elapsed={:.3}s files={} bytes={bytes} current={} outdated={} missing={} modified={}", repeat + 1,
                started.elapsed().as_secs_f64(), report.total, report.current, report.outdated,
                report.missing, report.modified);
        }
        // Sample evenly across the manifest to compare concurrency without five
        // additional full-install reads. Finish with a warm serial control.
        let sample = Manifest { files: m.files.iter().step_by(16).cloned().collect() };
        let mut reference = None;
        for workers in [1, 2, 4, 1] {
            let started = Instant::now();
            let result = scan_with_workers(&root, &sample, Mode::Check, &Operation::new(), workers).unwrap();
            if let Some(reference) = &reference { assert_eq!(&result, reference); }
            else { reference = Some(result.clone()); }
            eprintln!("file-check sample workers={workers} elapsed={:.3}s files={}", started.elapsed().as_secs_f64(), sample.files.len());
        }
    }

    #[test]
    #[ignore = "read-only component/concurrency timing; requires CRYSTAL_FILE_CHECK_BENCH_INSTALL"]
    fn read_only_file_check_profile() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_FILE_CHECK_BENCH_INSTALL")
            .expect("Set CRYSTAL_FILE_CHECK_BENCH_INSTALL to the installation to read"));
        assert!(root.is_absolute());
        ensure_plain(&root).unwrap();
        let m = manifest::parse(&fs::read_to_string(root.join("patch.cfg")).unwrap()).unwrap();
        validate_manifest(&m).unwrap();
        let mut indices: Vec<usize> = (0..m.files.len()).step_by(32).collect();
        let mut largest: Vec<usize> = (0..m.files.len()).collect();
        largest.sort_unstable_by_key(|&index| std::cmp::Reverse(m.files[index].current().sig.len));
        indices.extend(largest.into_iter().take(8));
        indices.sort_unstable();
        indices.dedup();
        let sample = Manifest { files: indices.into_iter().map(|index| m.files[index].clone()).collect() };
        let bytes: u64 = sample.files.iter().map(|h| h.current().sig.len).sum();
        let mut guards = Duration::ZERO;
        let mut opens = Duration::ZERO;
        let mut reads = Duration::ZERO;
        let mut sums = Duration::ZERO;
        let mut hashes = Duration::ZERO;
        let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
        for history in &sample.files {
            let started = Instant::now();
            let path = confined(&root, &history.path).unwrap();
            assert!(fs::metadata(&path).unwrap().is_file());
            ensure_plain(&path).unwrap();
            guards += started.elapsed();
            let started = Instant::now();
            let mut file = fs::File::open(&path).unwrap();
            opens += started.elapsed();
            let mut hasher = md5::Context::new();
            let mut sum = 0i32;
            let mut len = 0;
            loop {
                let started = Instant::now();
                let n = file.read(&mut buffer).unwrap();
                reads += started.elapsed();
                if n == 0 { break; }
                let started = Instant::now();
                for byte in &buffer[..n] { sum = sum.wrapping_add(*byte as i8 as i32); }
                sums += started.elapsed();
                let started = Instant::now();
                hasher.consume(&buffer[..n]);
                hashes += started.elapsed();
                len += n as u64;
            }
            let started = Instant::now();
            let digest = hasher.compute().0;
            hashes += started.elapsed();
            assert_eq!(FileSig { len, byte_sum: sum, md5_word: i32::from_le_bytes(digest[..4].try_into().unwrap()) }, history.current().sig);
        }
        eprintln!("file-check profile files={} bytes={bytes} guards={:.3}s open={:.3}s read={:.3}s sum={:.3}s md5={:.3}s", sample.files.len(),
            guards.as_secs_f64(), opens.as_secs_f64(), reads.as_secs_f64(), sums.as_secs_f64(), hashes.as_secs_f64());
        let mut reference = None;
        for workers in [4, 6, 8, 12, 4] {
            let started = Instant::now();
            let result = scan_with_workers(&root, &sample, Mode::Check, &Operation::new(), workers).unwrap();
            if let Some(reference) = &reference { assert_eq!(&result, reference); }
            else { reference = Some(result.clone()); }
            eprintln!("file-check concurrency workers={workers} elapsed={:.3}s files={} bytes={bytes}", started.elapsed().as_secs_f64(), sample.files.len());
        }
    }

    #[test]
    fn file_check_worker_count_scales_with_cpu_and_keeps_the_playing_limit() {
        for (available, idle, playing) in [(0, 1, 1), (1, 1, 1), (2, 2, 2), (4, 4, 4),
            (6, 6, 4), (8, 8, 4), (12, 8, 4), (24, 8, 4)] {
            assert_eq!(file_check_workers(available, false), idle);
            assert_eq!(file_check_workers(available, true), playing);
        }
    }

    #[test]
    fn parallel_checks_preserve_manifest_order_classification_and_delta_plans() {
        let root = fixture("parallel-check");
        let mut m = Manifest::default();
        for index in 0..272 {
            let path = format!("{index}.DAT");
            let old = manifest::Version {
                stamp: "30260903_0".into(), sig: FileSig::of(b"old"),
                direct: format!("30260903_0/Direct/{path}.slc"), direct_len: 20, indirect: None,
            };
            let current = manifest::Version {
                stamp: "30260904_1".into(), sig: FileSig::of(b"official"),
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: 20,
                indirect: Some(manifest::Delta { path: format!("30260904_1/Indirect/{path}.olc"), len: 2 }),
            };
            match index % 4 {
                0 => fs::write(root.join(&path), b"official").unwrap(),
                1 => fs::write(root.join(&path), b"old").unwrap(),
                2 => {},
                _ => fs::write(root.join(&path), b"modified").unwrap(),
            }
            m.files.push(FileHistory { path, optional: false, versions: vec![old, current] });
        }
        for mode in [Mode::Check, Mode::Update, Mode::Repair] {
            let expected = scan_with_workers(&root, &m, mode, &Operation::new(), 1).unwrap();
            assert_eq!((expected.0.current, expected.0.outdated, expected.0.missing, expected.0.modified), (68, 68, 68, 68));
            assert_eq!(expected.0.issues.len(), 200);
            match mode {
                Mode::Check => assert!(expected.1.is_empty()),
                Mode::Update => {
                    assert_eq!(expected.1.len(), 136);
                    assert!(expected.1.iter().all(|p| p.index % 4 != 3));
                    assert!(expected.1.iter().filter(|p| p.index % 4 == 1).all(|p| p.action == Action::Deltas(1)));
                }
                Mode::Repair => assert_eq!(expected.1.len(), 204),
            }
            for workers in [2, 4, 6, 8, 12] {
                let op = Operation::new();
                assert_eq!(scan_with_workers(&root, &m, mode, &op, workers).unwrap(), expected);
                assert_eq!((op.snapshot().done, op.snapshot().total), (272, 272));
            }
        }
        assert_eq!(fs::read(root.join("3.DAT")).unwrap(), b"modified");
        assert!(!root.join("patch.ver").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reusable_checksum_buffer_checks_large_tiny_empty_and_same_size_changes() {
        let root = fixture("checksum-buffer");
        let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
        let large: Vec<u8> = (0..SIGNATURE_BUFFER_LEN * 2 + 73).map(|index| index as u8).collect();
        let mut changed = large.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        let mut results = Vec::new();
        for data in [&large[..], b"tiny", b"", &changed[..]] {
            let path = root.join("file.DAT");
            fs::write(&path, data).unwrap();
            let signature = signature_with_buffer(&path, &Operation::new(), &mut buffer).unwrap();
            assert_eq!(signature, FileSig::of(data));
            assert_eq!(local_file_with_buffer(&path, &Operation::new(), &mut buffer).unwrap().md5, md5::compute(data).0);
            results.push(signature);
        }
        assert_ne!(results[0], results[3]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stock_installer_files_omitted_from_patch_history_update_directly_but_mods_are_preserved() {
        // Independent installed MSI/CAB identity; the server has retained only
        // a later target and offers no recognized base for this DLL.
        let stock = LocalFile { sig: FileSig { len: 2_869_840, byte_sum: 29_617_305, md5_word: -361_925_474 },
            md5: [0x9e, 0x74, 0x6d, 0xea, 0x4c, 0xf3, 0xe3, 0x28, 0x6d, 0xc0, 0x28, 0x95, 0x14, 0x69, 0x2b, 0xb1] };
        let manifest = Manifest { files: vec![FileHistory { path: "FFXiMain.dll".into(), optional: false,
            versions: vec![manifest::Version { stamp: "30260904_1".into(), sig: FileSig::of(b"new official DLL"),
                direct: "30260904_1/Direct/FFXiMain.dll.slc".into(), direct_len: 100,
                indirect: Some(manifest::Delta { path: "30260904_1/Indirect/FFXiMain.dll.olc".into(), len: 1 }) }] }] };
        for mode in [Mode::Check, Mode::Update, Mode::Repair] {
            let (report, planned) = plan_from_signatures(Product::Ffxi, &manifest, mode, &Operation::new(), vec![Some(stock)]).unwrap();
            assert_eq!((report.outdated, report.modified), (1, 0));
            if mode == Mode::Check { assert!(planned.is_empty()); }
            else { assert_eq!(planned, vec![Planned { index: 0, action: Action::Direct, original: Some(stock.sig) }]); }
        }
        let mut modified = stock;
        modified.md5[15] ^= 1; // Same length and stock protocol MD5 prefix is insufficient.
        let (report, planned) = plan_from_signatures(Product::Ffxi, &manifest, Mode::Update, &Operation::new(), vec![Some(modified)]).unwrap();
        assert_eq!((report.outdated, report.modified), (0, 1));
        assert!(planned.is_empty());
        let (report, _) = plan_from_signatures(Product::Viewer, &manifest, Mode::Check, &Operation::new(), vec![Some(stock)]).unwrap();
        assert_eq!(report.modified, 1, "FFXI's installer identities never classify PlayOnline files");
    }

    #[test]
    fn preserving_a_modification_still_checks_later_files_and_does_not_mask_a_missing_file() {
        let root = fixture("preserved-final-verification");
        fs::write(root.join("a.DAT"), b"user texture").unwrap();
        fs::write(root.join("b.DAT"), b"official").unwrap();
        let manifest = Manifest { files: ["a.DAT", "b.DAT"].into_iter().map(|path|
            FileHistory { path: path.into(), optional: false, versions: vec![manifest::Version {
                stamp: "30260904_1".into(), sig: FileSig::of(b"official"),
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: 20, indirect: None }] }).collect() };
        let (mut report, planned) = scan(&root, &manifest, Mode::Update, &Operation::new()).unwrap();
        report.version = "30260904_1".into();
        assert_eq!((report.current, report.modified), (1, 1));
        assert!(planned.is_empty());
        fs::remove_file(root.join("b.DAT")).unwrap(); // Removed after the check.
        let prepared = PreparedUpdate { product: Product::Ffxi, root: root.clone(), _lock: None,
            mode: Mode::Update, report, manifest, text: vec![], planned, seed: Some(1) };
        let error = commit(prepared, vec![], &Operation::new()).unwrap_err();
        assert!(error.contains("b.DAT: final file verification failed"), "{error}");
        assert!(!root.join("patch.ver").exists());
        assert_eq!(fs::read(root.join("a.DAT")).unwrap(), b"user texture");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checksum_cancellation_is_observed_between_reads() {
        struct CancellingReader { op: Operation, reads: usize }
        impl Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                assert_eq!(self.reads, 1, "a cancelled checksum must not start another read");
                buffer.fill(42);
                self.op.cancel();
                Ok(buffer.len())
            }
        }
        let op = Operation::new();
        let error = read_signature(CancellingReader { op: op.clone(), reads: 0 }, &op, &mut [0; 16]).unwrap_err();
        assert!(op.cancelled());
        assert!(error.contains("Cancelled"));
    }

    #[test]
    fn parallel_check_error_stops_workers_without_masking_error_or_reporting_success() {
        let root = fixture("parallel-error");
        fs::create_dir(root.join("bad.DAT")).unwrap();
        let mut m = Manifest::default();
        for index in 0..64 {
            m.files.push(FileHistory { optional: false, path: if index == 5 { "bad.DAT".into() } else { format!("missing{index}.DAT") },
                versions: vec![manifest::Version { stamp: "30260904_1".into(), sig: FileSig::of(b"official"),
                    direct: "30260904_1/Direct/f.DAT.slc".into(), direct_len: 20, indirect: None }] });
        }
        for workers in [4, 12] {
            let op = Operation::new();
            let error = scan_with_workers(&root, &m, Mode::Check, &op, workers).unwrap_err();
            assert!(error.contains("bad.DAT") && error.contains("not a regular file"), "{error}");
            assert!(!op.cancelled(), "an IO failure must not become a user cancellation");
            assert!(op.snapshot().done < op.snapshot().total);
            op.cancel();
            assert!(scan_with_workers(&root, &m, Mode::Check, &op, workers).unwrap_err().contains("Cancelled"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_repair_replaces_corrupt_applied_record_only_after_file_verification() {
        let root = fixture("corrupt-applied");
        let op = Operation::new();
        let corrupt = b"damaged applied record";
        fs::write(root.join("patch.ver"), corrupt).unwrap();
        fs::write(root.join("official.dat"), b"official bytes").unwrap();
        assert!(crate::patch::applied_seed(&root).is_err());
        let seed = crate::patch::repair_seed(&root).unwrap();
        let verified = FileSig::of(b"official bytes");
        let encoded = crate::patch::encode_applied_record("30260904_1", seed).unwrap();
        assert!(fixture_apply(&root, &[], &op, || {
            assert_eq!(
                signature(&root.join("official.dat"), &op).unwrap(),
                verified
            );
            assert_eq!(fs::read(root.join("patch.ver")).unwrap(), corrupt);
            Ok(Some((b"end\n".to_vec(), encoded.clone())))
        })
        .unwrap());
        assert_eq!(crate::patch::applied_seed(&root).unwrap(), seed);
        assert_eq!(fs::read(root.join("patch.ver")).unwrap(), encoded);
        let backup_dir = fs::read_dir(root.join(".xi-updater/backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let record: Journal =
            serde_json::from_slice(&fs::read(backup_dir.join("records.json")).unwrap()).unwrap();
        let backup = record
            .entries
            .iter()
            .find(|e| e.path == "patch.ver")
            .unwrap()
            .backup
            .as_ref()
            .unwrap();
        assert_eq!(fs::read(root.join(backup)).unwrap(), corrupt);
        fs::remove_dir_all(root).unwrap();
    }
}
