//! Temporary pilot diagnostics. Forward the five verified native backup APIs,
//! unchanged, and log only operation, byte count and native result. Never read
//! the caller's macro data, file name, metadata or account identity.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use crate::log;
use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

macro_rules! backup_log {
    ($($argument:tt)*) => {{
        // File logging must not replace the original function's Win32 error.
        let last_error = unsafe { GetLastError() };
        log!($($argument)*);
        unsafe { SetLastError(last_error) };
    }};
}

const ENTRIES: [(usize, usize); 5] = [
    (463, 0x48020), (464, 0x48240), (465, 0x482a0),
    (1535, 0x48060), (1536, 0x481d0),
];
static ORIGINAL: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
static PROGRESS: AtomicI32 = AtomicI32::new(-1);
static STATE: AtomicUsize = AtomicUsize::new(0);
static MISSING_FILE_POLICY: AtomicUsize = AtomicUsize::new(0);
static PHASE: AtomicI32 = AtomicI32::new(-1);

pub struct BackupTrace {
    table: *mut usize,
    originals: [usize; 5],
}
impl BackupTrace {
    /// Install before GameStart; keep the verified COM core alive until Drop,
    /// and finish/release the game's callers before restoring the table.
    pub unsafe fn install(table: &mut [usize], base: usize) -> Result<Self, String> {
        for (slot, rva) in ENTRIES {
            if table.get(slot).copied() != base.checked_add(rva) {
                return Err(format!("Native POL backup entry {slot} is unsupported."));
            }
        }
        if table.get(1074).copied() != base.checked_add(0x19560) {
            return Err("Native POL backup policy getter is unsupported.".into());
        }
        let trace = Self::forward(table)?;
        // This exact core has one backup context, with its state word at +0.
        // Only read that scalar; the retained module owns it through Drop.
        STATE.store(base + 0x9aab8, Ordering::Release);
        MISSING_FILE_POLICY.store(table[1074], Ordering::Release);
        Ok(trace)
    }
    unsafe fn forward(table: &mut [usize]) -> Result<Self, String> {
        let originals = ENTRIES.map(|(slot, _)| table[slot]);
        if originals.contains(&0)
            || ORIGINAL[0].compare_exchange(0, originals[0], Ordering::AcqRel, Ordering::Acquire).is_err()
        {
            return Err("Native POL backup trace is already active or missing an entry.".into());
        }
        for (index, &original) in originals.iter().enumerate().skip(1) {
            ORIGINAL[index].store(original, Ordering::Release);
        }
        PROGRESS.store(-1, Ordering::Relaxed);
        PHASE.store(-1, Ordering::Relaxed);
        for ((slot, _), wrapper) in ENTRIES.into_iter().zip([
            cancel as *const () as usize, query as *const () as usize,
            poll as *const () as usize, upload as *const () as usize,
            download as *const () as usize,
        ]) {
            table[slot] = wrapper;
        }
        backup_log!("native POL backup: five content-free forwarding traces enabled");
        Ok(Self { table: table.as_mut_ptr(), originals })
    }
}
impl Drop for BackupTrace {
    fn drop(&mut self) {
        for ((slot, _), original) in ENTRIES.into_iter().zip(self.originals) {
            // SAFETY: the install contract retains the core/table through Drop;
            // all game callers have finished before restoration.
            unsafe { *self.table.add(slot) = original };
        }
        for original in &ORIGINAL { original.store(0, Ordering::Release); }
        STATE.store(0, Ordering::Release);
        MISSING_FILE_POLICY.store(0, Ordering::Release);
    }
}

unsafe fn original<F: Copy>(index: usize) -> F {
    std::mem::transmute_copy(&ORIGINAL[index].load(Ordering::Acquire))
}
unsafe fn state() -> i32 {
    let pointer = STATE.load(Ordering::Acquire);
    if pointer == 0 { -1 } else { std::ptr::read_volatile(pointer as *const i32) }
}
unsafe extern "C" fn cancel() {
    original::<unsafe extern "C" fn()>(0)();
    PROGRESS.store(-1, Ordering::Relaxed);
    PHASE.store(-1, Ordering::Relaxed);
    backup_log!("native POL backup: reset completed");
}
unsafe extern "C" fn query(out: *mut u32) -> i32 {
    let result = original::<unsafe extern "C" fn(*mut u32) -> i32>(1)(out);
    PROGRESS.store(-1, Ordering::Relaxed);
    backup_log!("native POL backup: query start result {result}");
    result
}
unsafe extern "C" fn poll(ticket: i32) -> i32 {
    let before = state();
    let result = original::<unsafe extern "C" fn(i32) -> i32>(2)(ticket);
    let after = state();
    // Log transitions, not the native UI's tight progress-poll loop.
    let bucket = if result < 0 || result >= 100 { result } else { result / 25 };
    let progress_changed = PROGRESS.swap(bucket, Ordering::Relaxed) != bucket;
    let phase_changed = PHASE.swap(after, Ordering::Relaxed) != after;
    if progress_changed || phase_changed {
        backup_log!("native POL backup: phase {before}->{after} progress/result {result}");
    }
    result
}
unsafe extern "C" fn upload(data: *const u8, size: u32, kind: u32, name: *const u8) -> i32 {
    let result = original::<unsafe extern "C" fn(*const u8, u32, u32, *const u8) -> i32>(3)(data, size, kind, name);
    PROGRESS.store(-1, Ordering::Relaxed);
    let policy = MISSING_FILE_POLICY.load(Ordering::Acquire);
    // The verified getter is a single scalar load and does not touch transport.
    let policy = if policy == 0 { -1 } else {
        std::mem::transmute::<usize, unsafe extern "C" fn() -> i32>(policy)()
    };
    backup_log!("native POL backup: upload bytes={size} start result {result} missing-file policy {policy}");
    result
}
unsafe extern "C" fn download(data: *mut u8, size: u32, metadata: *mut u32) -> i32 {
    let result = original::<unsafe extern "C" fn(*mut u8, u32, *mut u32) -> i32>(4)(data, size, metadata);
    PROGRESS.store(-1, Ordering::Relaxed);
    backup_log!("native POL backup: download bytes={size} start result {result}");
    result
}

#[cfg(test)]
pub unsafe fn smoke_entrypoints(table: &mut [usize], base: usize) {
    let before = table.to_vec();
    let rejected = |table: &[usize]| {
        let query: unsafe extern "C" fn(*mut u32) -> i32 = std::mem::transmute(table[464]);
        let poll: unsafe extern "C" fn(i32) -> i32 = std::mem::transmute(table[465]);
        let upload: unsafe extern "C" fn(*const u8, u32, u32, *const u8) -> i32 = std::mem::transmute(table[1535]);
        let download: unsafe extern "C" fn(*mut u8, u32, *mut u32) -> i32 = std::mem::transmute(table[1536]);
        // All of these exit at native parameter validation, before allocating
        // any request. No connect step, pump, account or macro file is involved.
        [query(std::ptr::null_mut()), poll(1),
            upload(std::ptr::null(), 0x200001, 0, std::ptr::null()),
            upload(std::ptr::null(), 1, 0, std::ptr::null()),
            download(std::ptr::null_mut(), 1, std::ptr::null_mut())]
    };
    let expected = rejected(table);
    assert_eq!(expected, [-5120, -5136, -7199, -5120, -5120]);
    let trace = BackupTrace::install(table, base).unwrap();
    assert_eq!(state(), 0, "parameter failures must not allocate a backup context");
    assert_eq!(rejected(table), expected);
    assert_eq!(state(), 0);
    drop(trace);
    assert_eq!(STATE.load(Ordering::Acquire), 0);
    assert_eq!(MISSING_FILE_POLICY.load(Ordering::Acquire), 0);
    assert_eq!(table, before);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    static CALLS: Mutex<Vec<(usize, Vec<usize>)>> = Mutex::new(Vec::new());
    unsafe extern "C" fn mock_cancel() { CALLS.lock().unwrap().push((463, vec![])); }
    unsafe extern "C" fn mock_query(out: *mut u32) -> i32 {
        CALLS.lock().unwrap().push((464, vec![out as usize]));
        *out = 17;
        SetLastError(0x123456);
        0
    }
    unsafe extern "C" fn mock_poll(ticket: i32) -> i32 {
        CALLS.lock().unwrap().push((465, vec![ticket as usize]));
        -7198
    }
    unsafe extern "C" fn mock_upload(data: *const u8, size: u32, kind: u32, name: *const u8) -> i32 {
        CALLS.lock().unwrap().push((1535, vec![data as usize, size as usize, kind as usize, name as usize]));
        -256
    }
    unsafe extern "C" fn mock_download(data: *mut u8, size: u32, meta: *mut u32) -> i32 {
        CALLS.lock().unwrap().push((1536, vec![data as usize, size as usize, meta as usize]));
        *meta = 23;
        0
    }
    #[test]
    fn traces_forward_arguments_results_and_outputs_and_restore_only_their_entries() {
        let mut table = vec![123; 1560];
        let pointers = [mock_cancel as *const () as usize, mock_query as *const () as usize,
            mock_poll as *const () as usize, mock_upload as *const () as usize,
            mock_download as *const () as usize];
        for ((slot, _), pointer) in ENTRIES.into_iter().zip(pointers) { table[slot] = pointer; }
        let before = table.clone();
        // The production installer must refuse these mock addresses.
        assert!(unsafe { BackupTrace::install(&mut table, 0x10000000) }.is_err());
        assert_eq!(table, before);
        let trace = unsafe { BackupTrace::forward(&mut table) }.unwrap();
        let mut output = 0;
        let mut bytes = [1u8, 2, 3];
        let name = b"synthetic\0";
        unsafe {
            assert_eq!(query(&mut output), 0);
            assert_eq!(output, 17);
            assert_eq!(GetLastError(), 0x123456);
            assert_eq!(upload(bytes.as_ptr(), 3, 42, name.as_ptr()), -256);
            assert_eq!(poll(0), -7198);
            assert_eq!(download(bytes.as_mut_ptr(), 3, &mut output), 0);
            assert_eq!(output, 23);
            cancel();
        }
        assert_eq!(bytes, [1, 2, 3]);
        assert_eq!(*CALLS.lock().unwrap(), vec![
            (464, vec![&output as *const u32 as usize]),
            (1535, vec![bytes.as_ptr() as usize, 3, 42, name.as_ptr() as usize]),
            (465, vec![0]),
            (1536, vec![bytes.as_ptr() as usize, 3, &output as *const u32 as usize]),
            (463, vec![]),
        ]);
        drop(trace);
        assert_eq!(table, before);
    }
}
