//! The installed Windower renderer clears its command-worker stop flag, then
//! joins without waking the worker's auto-reset event. Both captured hangs had
//! that exact thread/event pair. Its base destructor then joins that already
//! joined thread again and throws from cleanup. Wake before the first join and
//! skip only an empty thread at these two calls. POL services stay unchanged.

use std::{ffi::c_void, sync::OnceLock};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE, WAIT_TIMEOUT},
    System::{
        Diagnostics::Debug::{FlushInstructionCache, ReadProcessMemory},
        LibraryLoader::GetModuleHandleExW,
        Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE},
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetProcessIdOfThread, GetThreadId, SetEvent,
            WaitForSingleObject,
        },
    },
};

const TIMESTAMP: u32 = 0x6AC1_2264;
const IMAGE_SIZE: u32 = 0x0023_1000;
const JOIN_RVA: usize = 0x35B00;
const CALL_RVA: usize = 0x60466;
const BASE_CALL_RVA: usize = 0x3E3D8;
const THREAD_OFFSET: usize = 0x2E34;
const EVENT_OFFSET: usize = 0x2E24;
const RUNNING_OFFSET: usize = 0x4280;
const SHUTDOWN: [u8; 18] = [
    0x8D, 0x8F, 0x34, 0x2E, 0, 0, 0xC6, 0x87, 0x80, 0x42, 0, 0, 0, 0xE8, 0x95, 0x56, 0xFD, 0xFF,
];
const BASE_SHUTDOWN: [u8; 18] = [
    0x8D, 0x8E, 0xFC, 0x2D, 0, 0, 0xC6, 0x86, 0x48, 0x42, 0, 0, 0, 0xE8, 0x23, 0x77, 0xFF, 0xFF,
];
const WAIT_ARGUMENT: [u8; 12] = [
    0x8B, 0x03, 0x6A, 0xFF, 0xFF, 0xB0, 0xEC, 0x2D, 0, 0, 0xFF, 0x15,
];
const WAIT_STOP: [u8; 11] = [0x8B, 0x03, 0x80, 0xB8, 0x48, 0x42, 0, 0, 0, 0x74, 0x08];
const JOIN_ENTRY: [u8; 11] = [
    0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF8, 0x56, 0x57, 0x8B, 0xF9, 0x83,
];
type Join = unsafe extern "thiscall" fn(*mut c_void);
static ORIGINAL_JOIN: OnceLock<Join> = OnceLock::new();

fn read<T: Copy>(address: usize) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes = 0;
    // Windows validates accessibility; no Rust dereference of Hook-owned data.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const c_void,
            value.as_mut_ptr().cast(),
            std::mem::size_of::<T>(),
            &mut bytes,
        )
    };
    (ok != 0 && bytes == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

fn verified_image(base: usize) -> bool {
    let Some(nt) = read::<u32>(base + 0x3C)
        .filter(|offset| *offset < 0x1000)
        .and_then(|offset| base.checked_add(offset as usize))
    else {
        return false;
    };
    read::<u16>(base) == Some(0x5A4D)
        && read::<u32>(nt) == Some(0x4550)
        && read::<u16>(nt + 4) == Some(0x014C)
        && read::<u32>(nt + 8) == Some(TIMESTAMP)
        && read::<u32>(nt + 24 + 56) == Some(IMAGE_SIZE)
        && read::<[u8; 18]>(base + 0x60459) == Some(SHUTDOWN)
        && read::<[u8; 18]>(base + 0x3E3CB) == Some(BASE_SHUTDOWN)
        && read::<[u8; 12]>(base + 0x50C48) == Some(WAIT_ARGUMENT)
        && read::<[u8; 11]>(base + 0x50C58) == Some(WAIT_STOP)
        && read::<[u8; 11]>(base + JOIN_RVA) == Some(JOIN_ENTRY)
}

fn relative_call(site: usize, destination: usize) -> [u8; 5] {
    // x86 rel32 uses wrapping 32-bit address arithmetic, including >2GiB jumps.
    let displacement = (destination as u32).wrapping_sub((site as u32).wrapping_add(5));
    let mut bytes = [0xE8, 0, 0, 0, 0];
    bytes[1..].copy_from_slice(&displacement.to_le_bytes());
    bytes
}

/// Called only at the verified renderer shutdown join, after Hook clears its
/// own flag. The base-class worker fields are at derived-object +0x38.
fn wake_stopped_worker(thread: *mut c_void) -> bool {
    let Some(owner) = (thread as usize).checked_sub(THREAD_OFFSET) else {
        return false;
    };
    let Some(running) = owner.checked_add(RUNNING_OFFSET) else {
        return false;
    };
    let Some(event_address) = owner.checked_add(EVENT_OFFSET) else {
        return false;
    };
    if read::<u8>(running) != Some(0) {
        return false;
    }
    let Some([handle, tid]) = read::<[u32; 2]>(thread as usize) else {
        return false;
    };
    let Some(event) = read::<u32>(event_address).filter(|event| *event != 0) else {
        return false;
    };
    if handle == 0 || tid == 0 {
        return false;
    }
    let handle = handle as usize as *mut c_void;
    // Verify a live worker in this host, rather than acting on a stale handle.
    unsafe {
        if GetThreadId(handle) != tid
            || GetProcessIdOfThread(handle) != GetCurrentProcessId()
            || WaitForSingleObject(handle, 0) != WAIT_TIMEOUT
        {
            return false;
        }
        SetEvent(event as usize as *mut c_void) != 0
    }
}

unsafe fn wake_then_join(thread: *mut c_void, join: Join) {
    if read::<[u32; 2]>(thread as usize) == Some([0, 0]) {
        log!("stage: Windower command worker already joined; continuing base cleanup");
        return;
    }
    if wake_stopped_worker(thread) {
        log!("stage: Windower stopped command worker woken before renderer teardown join");
    }
    // Perform the real join for every nonempty thread, even if the wake failed.
    join(thread);
}

unsafe extern "thiscall" fn renderer_join(thread: *mut c_void) {
    let Some(join) = ORIGINAL_JOIN.get() else {
        std::process::abort();
    };
    wake_then_join(thread, *join);
}

struct Module(HMODULE);
impl Drop for Module {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}

struct WriteError {
    message: &'static str,
    applied: bool,
}

fn write_call(site: usize, expected: [u8; 5], replacement: [u8; 5]) -> Result<(), WriteError> {
    if read::<[u8; 5]>(site) != Some(expected) {
        return Err(WriteError {
            message: "renderer join call changed",
            applied: false,
        });
    }
    let mut protection = 0;
    unsafe {
        if VirtualProtect(
            site as *const c_void,
            5,
            PAGE_EXECUTE_READWRITE,
            &mut protection,
        ) == 0
        {
            return Err(WriteError {
                message: "renderer join page is not writable",
                applied: false,
            });
        }
        std::ptr::copy_nonoverlapping(replacement.as_ptr(), site as *mut u8, 5);
        let flushed = FlushInstructionCache(GetCurrentProcess(), site as *const c_void, 5) != 0;
        let mut ignored = 0;
        let restored = VirtualProtect(site as *const c_void, 5, protection, &mut ignored) != 0;
        if !flushed || !restored {
            return Err(WriteError {
                message: "renderer join cache/page-protection restore failed",
                applied: true,
            });
        }
    }
    Ok(())
}

/// Owns a module reference through FFXI destruction. Only this host's loaded
/// image is changed; the Windower DLL on disk and other games are untouched.
pub(crate) struct Wakeup {
    module: Option<Module>,
    calls: [Call; 2],
}

struct Call {
    installed: bool,
    site: usize,
    original: [u8; 5],
    replacement: [u8; 5],
}

impl Call {
    fn new(base: usize, rva: usize) -> Self {
        let site = base + rva;
        Self {
            installed: false,
            site,
            original: relative_call(site, base + JOIN_RVA),
            replacement: relative_call(site, renderer_join as *const () as usize),
        }
    }
}

impl Wakeup {
    /// Run before GameStart can construct a renderer or execute this call site.
    pub(crate) fn install() -> Option<Self> {
        let name: Vec<u16> = "Hook.dll\0".encode_utf16().collect();
        let mut module = std::ptr::null_mut();
        if unsafe { GetModuleHandleExW(0, name.as_ptr(), &mut module) } == 0 {
            return None;
        }
        let module = Module(module);
        let base = module.0 as usize;
        if !verified_image(base) {
            log!("note: Windower renderer wake-up skipped: unsupported Hook.dll layout");
            return None;
        }
        let join: Join = unsafe { std::mem::transmute(base + JOIN_RVA) };
        if ORIGINAL_JOIN.set(join).is_err() {
            log!("note: Windower renderer wake-up was already prepared");
            return None;
        }
        let mut patch = Self {
            module: Some(module),
            // Prepare the base-destructor guard first, so a partial install
            // cannot wake the first join and expose an unguarded second join.
            calls: [Call::new(base, BASE_CALL_RVA), Call::new(base, CALL_RVA)],
        };
        for call in &mut patch.calls {
            if let Err(error) = write_call(call.site, call.original, call.replacement) {
                log!("error: Windower renderer wake-up: {}", error.message);
                if !error.applied {
                    return None;
                }
            }
            call.installed = true;
        }
        log!("stage: verified Windower renderer shutdown wake-up installed");
        Some(patch)
    }
}

impl Drop for Wakeup {
    fn drop(&mut self) {
        // Remove the first-join wake before removing the second-join guard.
        for call in self.calls.iter().rev().filter(|call| call.installed) {
            if let Err(error) = write_call(call.site, call.replacement, call.original) {
                log!(
                    "error: Windower renderer wake-up restore: {}",
                    error.message
                );
                // Keep the module and any later guards resident on failure.
                // No running callbacks are abandoned or unloaded.
                if let Some(module) = self.module.take() {
                    std::mem::forget(module);
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::windows::io::AsRawHandle,
        sync::{
            atomic::{AtomicU32, Ordering},
            Mutex,
        },
        thread,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::{
            Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE},
            Threading::CreateEventW,
        },
    };

    static JOIN_RESULT: AtomicU32 = AtomicU32::new(u32::MAX);
    static JOIN_TEST: Mutex<()> = Mutex::new(());
    unsafe extern "thiscall" fn test_join(thread: *mut c_void) {
        let handle = read::<u32>(thread as usize).unwrap() as usize as *mut c_void;
        let result = WaitForSingleObject(handle, 2_000);
        JOIN_RESULT.store(result, Ordering::Release);
        if result == WAIT_OBJECT_0 {
            // The real 35b00 join empties both fields. The Rust fixture owns
            // its OS handle, so it closes that separately through JoinHandle.
            thread.cast::<u32>().write(0);
            thread.cast::<u32>().add(1).write(0);
        }
    }

    struct Worker {
        event: *mut c_void,
        thread: Option<thread::JoinHandle<u32>>,
        object: Vec<u32>,
    }
    impl Worker {
        fn new(running: bool) -> Self {
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            assert!(!event.is_null());
            let event_value = event as usize;
            let thread = thread::spawn(move || unsafe {
                WaitForSingleObject(event_value as *mut c_void, 3_000)
            });
            let mut object = vec![0; (RUNNING_OFFSET + 4) / 4];
            object[EVENT_OFFSET / 4] = event as usize as u32;
            object[THREAD_OFFSET / 4] = thread.as_raw_handle() as usize as u32;
            object[(THREAD_OFFSET + 4) / 4] = unsafe { GetThreadId(thread.as_raw_handle()) };
            object[RUNNING_OFFSET / 4] = u32::from(running);
            Self {
                event,
                thread: Some(thread),
                object,
            }
        }
        fn join_pointer(&mut self) -> *mut c_void {
            unsafe { self.object.as_mut_ptr().add(THREAD_OFFSET / 4).cast() }
        }
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            unsafe { SetEvent(self.event) };
            self.thread.take().unwrap().join().unwrap();
            unsafe { CloseHandle(self.event) };
        }
    }

    #[test]
    fn stopped_worker_is_woken_before_original_thiscall_join() {
        let _serial = JOIN_TEST.lock().unwrap();
        let mut worker = Worker::new(false);
        let thread = worker.thread.as_ref().unwrap().as_raw_handle();
        assert_eq!(unsafe { WaitForSingleObject(thread, 0) }, WAIT_TIMEOUT);
        JOIN_RESULT.store(u32::MAX, Ordering::Release);
        unsafe { wake_then_join(worker.join_pointer(), test_join) };
        assert_eq!(JOIN_RESULT.load(Ordering::Acquire), WAIT_OBJECT_0);
    }

    #[test]
    fn running_worker_and_stale_thread_identity_are_not_signaled() {
        let mut worker = Worker::new(true);
        assert!(!wake_stopped_worker(worker.join_pointer()));
        assert_eq!(
            unsafe { WaitForSingleObject(worker.event, 0) },
            WAIT_TIMEOUT
        );
        worker.object[RUNNING_OFFSET / 4] = 0;
        worker.object[(THREAD_OFFSET + 4) / 4] ^= 1;
        assert!(!wake_stopped_worker(worker.join_pointer()));
        assert_eq!(
            unsafe { WaitForSingleObject(worker.event, 0) },
            WAIT_TIMEOUT
        );
        assert!(!wake_stopped_worker(std::ptr::null_mut()));
        assert!(!wake_stopped_worker(usize::MAX as *mut c_void));
    }

    #[test]
    fn real_x86_call_is_replaced_and_original_call_is_restored() {
        let _serial = JOIN_TEST.lock().unwrap();
        let _ = ORIGINAL_JOIN.set(test_join);
        struct Code(*mut c_void);
        impl Drop for Code {
            fn drop(&mut self) {
                unsafe { VirtualFree(self.0, 0, MEM_RELEASE) };
            }
        }
        let code = Code(unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        });
        assert!(!code.0.is_null());
        let site = code.0 as usize;
        let original = relative_call(site, test_join as *const () as usize);
        let replacement = relative_call(site, renderer_join as *const () as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(original.as_ptr(), code.0.cast(), 5);
            code.0.cast::<u8>().add(5).write(0xC3); // call; ret, preserving thiscall ECX
        }
        assert!(write_call(site, original, replacement).is_ok());
        let entry: Join = unsafe { std::mem::transmute(code.0) };
        let mut worker = Worker::new(false);
        JOIN_RESULT.store(u32::MAX, Ordering::Release);
        unsafe { entry(worker.join_pointer()) };
        assert_eq!(JOIN_RESULT.load(Ordering::Acquire), WAIT_OBJECT_0);
        JOIN_RESULT.store(u32::MAX, Ordering::Release);
        unsafe { entry(worker.join_pointer()) };
        assert_eq!(
            JOIN_RESULT.load(Ordering::Acquire),
            u32::MAX,
            "an empty worker must not be joined again"
        );
        assert!(write_call(site, replacement, original).is_ok());
        assert_eq!(read::<[u8; 5]>(site), Some(original));
        let mut original_worker = Worker::new(false);
        unsafe { SetEvent(original_worker.event) };
        JOIN_RESULT.store(u32::MAX, Ordering::Release);
        unsafe { entry(original_worker.join_pointer()) };
        assert_eq!(JOIN_RESULT.load(Ordering::Acquire), WAIT_OBJECT_0);
        // A changed call site must be left alone, rather than overwriting it.
        assert!(write_call(site, replacement, original).is_err());
        assert_eq!(read::<[u8; 5]>(site), Some(original));
    }

    #[test]
    fn relative_calls_preserve_x86_wrapping_addresses() {
        for (site, target) in [(0x7000_0000, 0x0040_1000), (0x0040_0000, 0xE000_1000)] {
            let call = relative_call(site, target);
            assert_eq!(call[0], 0xE8);
            assert_eq!(
                (site as u32)
                    .wrapping_add(5)
                    .wrapping_add(u32::from_le_bytes(call[1..].try_into().unwrap())),
                target as u32
            );
        }
    }
}
