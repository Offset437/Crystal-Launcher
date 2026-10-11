//! PlayOnline component readiness for the shared native updater.
//! FFXI's updater must not apply product-0001 files or records to product 1000.
use crate::{i18n::{t, tf}, patch::{self, PatchReport, PatchState}, updater};
use std::{fs, io::{Read, Seek, SeekFrom}, path::{Path, PathBuf}, sync::Mutex, time::SystemTime};

// Matches the embedded loader's verified layout in crystal-loader/src/game.rs.
// Its timestamp, image bounds and native entry checks remain authoritative.
const SUPPORTED_CORE_TIMESTAMP: u32 = 0x4E4E_5917;

#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub folder: Option<PathBuf>,
    pub version: PatchReport,
    pub core_error: Option<String>,
}
impl Status {
    pub fn requires_update(&self) -> bool {
        self.folder.is_none() || self.version.state == PatchState::Required || self.core_error.is_some()
    }
    pub fn retranslate(&mut self, from: crate::i18n::Lang) {
        let keys = ["Unavailable", "Not checked", "Not initialized",
            "PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.",
            "PlayOnline Viewer matches the official version.",
            "A PlayOnline Viewer update is required before a new sign-in. Choose Update.",
            "PlayOnline has not completed its initial update. Open Downloads and choose Update.",
            "PlayOnline version has not been verified.",
            "Installed PlayOnline components are too old for Crystal. Open Downloads and choose Update.",
            "This PlayOnline core build is unsupported by Crystal. A compatible Crystal build is required before a new sign-in."];
        for text in [&mut self.version.local_version, &mut self.version.remote_version, &mut self.version.detail]
            .into_iter().chain(self.core_error.iter_mut()) {
            if let Some(shown) = crate::retranslated(text, from, &keys) { *text = shown.into(); }
        }
    }
}

type FileKey = Option<(u64, SystemTime)>;
#[derive(PartialEq, Eq)]
struct Key { folder: Option<PathBuf>, record: FileKey, core: FileKey }
static CACHE: Mutex<Option<(Key, crate::i18n::Lang, Status)>> = Mutex::new(None);

fn file_key(path: &Path) -> FileKey {
    fs::metadata(path).ok().and_then(|m| m.modified().ok().map(|time| (m.len(), time)))
}

/// One shared query per unchanged installed Viewer. Explicit checks and the
/// end of a shared update refresh it; launcher redraws never query POL.
pub(crate) fn status(force: bool) -> Status {
    // Unit/process fixtures stay offline. The opt-in installed probe uses force.
    if cfg!(test) && !force {
        return Status { folder: Some(std::env::temp_dir()), version: unknown("Offline test fixture".into()), core_error: None };
    }
    let folder = patch::registry_install_folder("1000");
    let key = Key {
        record: folder.as_ref().and_then(|p| file_key(&p.join("patch.ver"))),
        core: folder.as_ref().and_then(|p| file_key(&p.join("viewer/com/polcore.dll"))),
        folder: folder.clone(),
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if !force {
        if let Some((saved, lang, report)) = &*cache {
            if saved == &key {
                let mut report = report.clone();
                report.retranslate(*lang);
                return report;
            }
        }
    }
    let report = match folder {
        Some(folder) => {
            let safe = updater::ensure_plain(&folder);
            let version = match &safe {
                Ok(()) => patch::viewer_check(&folder),
                Err(error) => unknown(error.clone()),
            };
            let core_error = safe.and_then(|()| core_compatibility(&folder)).err();
            Status { folder: Some(folder), version, core_error }
        }
        None => Status { folder: None, version: unknown(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into()), core_error: None },
    };
    *cache = Some((key, crate::i18n::current(), report.clone()));
    report
}

fn unknown(detail: String) -> PatchReport {
    PatchReport { state: PatchState::Unknown, local_version: t("Unavailable").into(),
        remote_version: t("Not checked").into(), detail, files_verified: None, source_url: None }
}

pub(crate) fn unavailable(error: String) -> Status {
    Status { folder: patch::registry_install_folder("1000"), version: unknown(error.clone()), core_error: Some(error) }
}

/// Read local readiness against the version already checked by the updater.
/// This is deliberately independent of the network/cache lock on cancellation.
pub(crate) fn after_check(folder: PathBuf, remote: Option<&str>) -> Status {
    let report = Status { version: patch::viewer_report_with_remote(&folder, remote),
        core_error: updater::ensure_plain(&folder).and_then(|()| core_compatibility(&folder)).err(),
        folder: Some(folder.clone()) };
    if remote.is_some() {
        if let Ok(mut cache) = CACHE.try_lock() {
            let key = Key { folder: Some(folder.clone()), record: file_key(&folder.join("patch.ver")),
                core: file_key(&folder.join("viewer/com/polcore.dll")) };
            *cache = Some((key, crate::i18n::current(), report.clone()));
        }
    }
    report
}

/// Only new sign-ins are gated. Existing game and social sessions retain their
/// own loaded core; updates require them to exit through the ordinary controls.
pub(crate) fn gate(report: &Status) -> Result<(), String> {
    let Some(folder) = &report.folder else { return Err(report.version.detail.clone()); };
    // A cached ready report can predate an update or interrupted transaction.
    let _update_lock = patch::check_update_state(folder)?;
    if report.version.state == PatchState::Required {
        return Err(tf("PlayOnline update required: installed {local}, official {remote}. Open Downloads and choose Update.",
            &[("local", &report.version.local_version), ("remote", &report.version.remote_version)]));
    }
    if let Some(error) = &report.core_error { return Err(error.clone()); }
    // A failed network check stays unverified; it cannot claim an update or
    // override the native loader's compatibility checks.
    Ok(())
}

fn core_compatibility(folder: &Path) -> Result<(), String> {
    let timestamp = core_timestamp(&folder.join("viewer/com/polcore.dll"))?;
    match timestamp.cmp(&SUPPORTED_CORE_TIMESTAMP) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(t("Installed PlayOnline components are too old for Crystal. Open Downloads and choose Update.").into()),
        std::cmp::Ordering::Greater => Err(t("This PlayOnline core build is unsupported by Crystal. A compatible Crystal build is required before a new sign-in.").into()),
    }
}

fn core_timestamp(path: &Path) -> Result<u32, String> {
    updater::ensure_plain(path)?;
    let read = || -> std::io::Result<u32> {
        let mut file = fs::File::open(path)?;
        let mut dos = [0u8; 64];
        file.read_exact(&mut dos)?;
        let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
        if &dos[..2] != b"MZ" || !(64..=1024 * 1024).contains(&offset) {
            return Err(std::io::Error::other("Invalid DOS/PE header"));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut pe = [0u8; 24];
        file.read_exact(&mut pe)?;
        if &pe[..4] != b"PE\0\0" || u16::from_le_bytes(pe[4..6].try_into().unwrap()) != 0x14c {
            return Err(std::io::Error::other("Expected a 32-bit PlayOnline core"));
        }
        Ok(u32::from_le_bytes(pe[8..12].try_into().unwrap()))
    };
    read().map_err(|e| tf("Cannot check PlayOnline components: {error}. Use official setup or Update.", &[("error", &e)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-viewer-test-{}-{}", std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&root).unwrap();
        root
    }
    fn core_bytes(timestamp: u32) -> Vec<u8> {
        let mut bytes = vec![0; 88];
        bytes[..2].copy_from_slice(b"MZ"); bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0"); bytes[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        bytes[72..76].copy_from_slice(&timestamp.to_le_bytes()); bytes
    }
    #[test]
    fn incompatible_missing_and_malformed_cores_cannot_start_a_new_session() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        let core = root.join("viewer/com/polcore.dll");
        let mut report = Status { folder: Some(root.clone()), version: unknown("offline".into()), core_error: None };
        for timestamp in [0x45c83316, SUPPORTED_CORE_TIMESTAMP + 1] {
            fs::write(&core, core_bytes(timestamp)).unwrap();
            report.core_error = core_compatibility(&root).err();
            assert!(report.requires_update()); assert!(gate(&report).is_err());
        }
        fs::write(&core, core_bytes(SUPPORTED_CORE_TIMESTAMP)).unwrap();
        report.core_error = core_compatibility(&root).err();
        assert!(!report.requires_update()); assert!(gate(&report).is_ok());
        let mut wrong_machine = core_bytes(SUPPORTED_CORE_TIMESTAMP); wrong_machine[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        for bytes in [wrong_machine, b"invalid core".to_vec()] {
            fs::write(&core, bytes).unwrap(); assert!(core_compatibility(&root).is_err());
        }
        fs::remove_file(&core).unwrap(); assert!(core_compatibility(&root).is_err());
        report.core_error = None; report.version.state = PatchState::Required;
        assert!(gate(&report).unwrap_err().contains("PlayOnline update required"));
        report.folder = None; assert!(gate(&report).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn update_readiness_reuses_remote_without_waiting_for_the_network_cache() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(SUPPORTED_CORE_TIMESTAMP)).unwrap();
        let seed = patch::applied_seed_for(&root, "1000").unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, "1000").unwrap()).unwrap();
        let held_network_cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let report = after_check(root.clone(), Some("20110829_E"));
        assert_eq!(report.version.state, PatchState::Current); assert!(gate(&report).is_ok());
        let later = after_check(root.clone(), Some("20110830_0"));
        assert_eq!(later.version.state, PatchState::Required); assert!(gate(&later).is_err());
        let unknown = after_check(root.clone(), None);
        assert_eq!(unknown.version.state, PatchState::Unknown);
        drop(held_network_cache);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cached_ready_viewer_is_blocked_by_a_new_interrupted_update() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(SUPPORTED_CORE_TIMESTAMP)).unwrap();
        let seed = patch::applied_seed_for(&root, "1000").unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, "1000").unwrap()).unwrap();
        let report = after_check(root.clone(), Some("20110829_E"));
        assert_eq!(report.version.state, PatchState::Current);
        assert!(gate(&report).is_ok());

        fs::create_dir(root.join(".xi-updater")).unwrap();
        let journal = root.join(".xi-updater/transaction.json");
        fs::write(&journal, b"interrupted journal fixture").unwrap();
        assert!(gate(&report).unwrap_err().contains("interrupted update"));
        fs::remove_file(journal).unwrap();
        assert!(gate(&report).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "read-only official Viewer version check; requires an installed PlayOnline Viewer and network"]
    fn installed_viewer_version_and_core() {
        let report = status(true);
        eprintln!("Viewer: folder={:?}; state={:?}; local={}; official={}; detail={}; core={:?}",
            report.folder, report.version.state, report.version.local_version, report.version.remote_version,
            report.version.detail, report.core_error);
        assert!(report.folder.is_some()); assert_ne!(report.version.state, PatchState::Unknown);
    }
}
