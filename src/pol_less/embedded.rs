//! The 32-bit loader (`pol.exe`) travels inside the launcher exe (`build.rs` embeds it) and is
//! written to `<data dir>\crystal\pol.exe` before a POL-less launch. The loader writes its own
//! log beside itself, so the folder stays user-writable.
//!
//! The file is rewritten only when its SHA-256 differs from the embedded one, through a temp file
//! and a rename. A running game keeps the old `pol.exe` mapped, so a rename onto it fails; the new
//! build then goes to `crystal\<hash8>\pol.exe` and the old one stays until nothing runs it.

use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

static BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded-pol.exe"));
static SHA256: &str = include_str!(concat!(env!("OUT_DIR"), "/embedded-pol.sha256"));

/// The embedded loader and its SHA-256 (lower-case hex), or `None` for a build without one.
pub(crate) fn embedded() -> Option<(&'static [u8], &'static str)> {
    (!BYTES.is_empty() && !SHA256.is_empty()).then_some((BYTES, SHA256.trim()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The launcher's per-user data folder, where `crystal\` lives.
pub(crate) fn data_dir() -> PathBuf {
    crate::backend::Config::default().experiment.join("iced-launcher")
}

/// Extracts the embedded loader under the launcher's data folder; `None` when this build embeds
/// none or the folder can't be written.
pub(crate) fn extracted() -> Option<PathBuf> {
    // Unit tests never write into the real data folder.
    if cfg!(test) {
        return None;
    }
    let (bytes, hash) = embedded()?;
    extract_into(&data_dir(), bytes, hash).ok()
}

fn has_hash(path: &Path, hash: &str) -> bool {
    fs::read(path).map(|b| sha256_hex(&b) == hash).unwrap_or(false)
}

/// Writes `bytes` to `path` through a temp file and a rename, so a reader never sees half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!("pol.exe.{}.tmp", std::process::id()));
    let result = fs::write(&temp, bytes).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Puts the loader in `<root>\crystal` and returns the path to run. The standard
/// `crystal\pol.exe` when it already matches or can be replaced, else `crystal\<hash8>\pol.exe`.
pub(crate) fn extract_into(root: &Path, bytes: &[u8], hash: &str) -> io::Result<PathBuf> {
    let standard = root.join("crystal").join("pol.exe");
    if has_hash(&standard, hash) {
        return Ok(standard);
    }
    if write_atomic(&standard, bytes).is_ok() && has_hash(&standard, hash) {
        return Ok(standard);
    }
    // In use by a running game (or otherwise unreplaceable): a versioned copy beside it.
    let versioned = root.join("crystal").join(&hash[..8.min(hash.len())]).join("pol.exe");
    if has_hash(&versioned, hash) {
        return Ok(versioned);
    }
    write_atomic(&versioned, bytes)?;
    Ok(versioned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crystal-embedded-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_extraction_writes_the_standard_path() {
        let root = temp("fresh");
        let bytes = b"loader one";
        let path = extract_into(&root, bytes, &sha256_hex(bytes)).unwrap();
        assert_eq!(path, root.join("crystal").join("pol.exe"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(root.join("crystal")).unwrap().count(), 1, "no temp file is left");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unchanged_hash_is_not_rewritten() {
        let root = temp("same");
        let bytes = b"loader one";
        let hash = sha256_hex(bytes);
        let path = extract_into(&root, bytes, &hash).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(extract_into(&root, bytes, &hash).unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn changed_hash_replaces_the_file() {
        let root = temp("changed");
        let old = b"loader one";
        let new = b"loader two, newer";
        let path = extract_into(&root, old, &sha256_hex(old)).unwrap();
        assert_eq!(extract_into(&root, new, &sha256_hex(new)).unwrap(), path);
        assert_eq!(fs::read(&path).unwrap(), new);
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_loader_gets_a_versioned_copy_and_stays_untouched() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = temp("locked");
        let old = b"loader one";
        let new = b"loader two, newer";
        let standard = extract_into(&root, old, &sha256_hex(old)).unwrap();
        // A running image can be read but not replaced or deleted (share mode: read only).
        let _held = fs::OpenOptions::new().read(true).share_mode(1).open(&standard).unwrap();
        let hash = sha256_hex(new);
        let path = extract_into(&root, new, &hash).unwrap();
        assert_eq!(path, root.join("crystal").join(&hash[..8]).join("pol.exe"));
        assert_eq!(fs::read(&path).unwrap(), new);
        assert_eq!(fs::read(&standard).unwrap(), old, "the running file is left alone");
        // A later launch reuses the versioned copy.
        assert_eq!(extract_into(&root, new, &hash).unwrap(), path);
        drop(_held);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_embedded_loader_matches_its_recorded_hash() {
        if let Some((bytes, hash)) = embedded() {
            assert_eq!(sha256_hex(bytes), hash);
        }
    }
}
