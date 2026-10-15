//! Serialize FFXIDB's complete settings transactions between Crystal hosts.
//!
//! FFXIDB 1.2.3.14 ignores an unsuccessful XML read and then saves its fresh
//! document. Its CRT opens writers exclusively, so simultaneous startup or a
//! later minimap save can erase another character's settings. A file-open lock
//! would be too short: it must include read, modify, and write.
//!
//! This adapter is deliberately specific to the verified x86 builds below.
//! Only Hook's GetProcAddress import and the new FFXIDB object's vptr change.
//! No instructions, vendor files, other plugins, or process-wide APIs change.
//! The original factory, object, callbacks, calling conventions, and return
//! values remain in use. All references and tables live until process exit.

use std::{
    ffi::{c_void, CStr, OsString},
    marker::PhantomData,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};
use windows_sys::{
    core::PCSTR,
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, FARPROC, HMODULE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
        System::{
            Diagnostics::Debug::ReadProcessMemory,
            LibraryLoader::{
                GetModuleFileNameW, GetModuleHandleExW, GetProcAddress,
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            },
            Memory::{VirtualProtect, PAGE_READWRITE},
            Threading::{
                CreateMutexW, GetCurrentProcess, ReleaseMutex, WaitForSingleObject, INFINITE,
            },
        },
    },
};

const HOOK_SHA256: &str = "c32826605356777a8fb9507c1b238b5ede4da6cb3ef6f491424891db864a8cb5";
const FFXIDB_SHA256: &str = "a843f6eca81fdf6b3e60984e0dd0bd93f6b292b2ccfe8bf79cca0fe51dbd8a58";
const HOOK_ID: (u32, u32) = (0x6AC4_F0DD, 0x23_0000);
const FFXIDB_ID: (u32, u32) = (0x6A51_21AC, 0xD_5000);
const GET_PROC_IAT: usize = 0x14_111C;
const FACTORY_RVA: usize = 0x6_9DC0;
const VTABLE_RVA: usize = 0xB_54C4;

// The 17 public stdcall callbacks are followed by a deleting destructor and
// 16 internal thiscall callbacks. Preserve every entry and the RTTI locator
// at vtable[-1], since the stock callbacks dispatch to the internal entries.
const VTABLE_RVAS: [usize; 34] = [
    0x78A50, 0x78A40, 0x78940, 0x78920, 0x78910, 0x78900, 0x788F0, 0x788D0, 0x788B0, 0x78890,
    0x78870, 0x78670, 0x78640, 0x78610, 0x785F0, 0x785D0, 0x785B0, 0x149D0, 0x6A670, 0x6A470,
    0x77810, 0x6A180, 0x6A170, 0x6A1A0, 0x77800, 0x777F0, 0x777E0, 0x6A4A0, 0x777D0, 0x6A120,
    0x777C0, 0x777B0, 0x777A0, 0x6A190,
];

type Resolve = unsafe extern "system" fn(HMODULE, PCSTR) -> FARPROC;
type Factory = unsafe extern "C" fn() -> *mut c_void;
type Initialize = unsafe extern "system" fn(*mut c_void, *mut c_void);
#[cfg(test)]
type Notify = unsafe extern "system" fn(*mut c_void);
type Command = unsafe extern "system" fn(*mut c_void, *const u8);
// The last argument and result are byte-sized flags, passed in four-byte
// stack slots / AL. Use u8 rather than Rust bool: preserve every bit.
type Incoming = unsafe extern "system" fn(*mut c_void, *const u8, *const u8, u32, u8) -> u8;
type Mouse = unsafe extern "system" fn(*mut c_void, u32, i32, i32, i32, u8) -> u8;

struct Module(usize);
impl Module {
    fn retain(address: usize) -> Option<Self> {
        let mut module = std::ptr::null_mut();
        let ok = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                address as *const u16,
                &mut module,
            )
        };
        (ok != 0 && module as usize == address).then_some(Self(module as usize))
    }
}
impl Drop for Module {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                FreeLibrary(self.0 as HMODULE);
            }
        }
    }
}

struct Bridge {
    _hook: Module,
    resolve: Resolve,
}
struct Adapter {
    _plugin: Module,
    factory: Factory,
    original: [usize; 34],
    original_vtable: usize,
    // Box is stable when Adapter moves into OnceLock.
    shadow: Box<[usize; 35]>,
    transaction: TransactionMutex,
}
static BRIDGE: OnceLock<Bridge> = OnceLock::new();
static ADAPTER: OnceLock<Adapter> = OnceLock::new();
static INSTALL: Mutex<()> = Mutex::new(());

fn read<T: Copy>(address: usize) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut count = 0;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const c_void,
            value.as_mut_ptr().cast(),
            std::mem::size_of::<T>(),
            &mut count,
        )
    };
    (ok != 0 && count == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

fn pe_identity(base: usize) -> Option<(u32, u32)> {
    let offset = read::<u32>(base.checked_add(0x3C)?)?;
    if offset >= 0x10000 {
        return None;
    }
    let nt = base.checked_add(offset as usize)?;
    (read::<u16>(base) == Some(0x5A4D)
        && read::<u32>(nt) == Some(0x4550)
        && read::<u16>(nt + 4) == Some(0x14C)
        && read::<u16>(nt + 24) == Some(0x10B))
    .then_some((read::<u32>(nt + 8)?, read::<u32>(nt + 80)?))
}

fn module_path(module: usize) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let n =
        unsafe { GetModuleFileNameW(module as HMODULE, buffer.as_mut_ptr(), buffer.len() as u32) }
            as usize;
    (n != 0 && n < buffer.len()).then(|| PathBuf::from(OsString::from_wide(&buffer[..n])))
}

fn sha256(bytes: &[u8]) -> Option<String> {
    let mut digest = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            bytes.as_ptr(),
            u32::try_from(bytes.len()).ok()?,
            digest.as_mut_ptr(),
            32,
        )
    };
    (status == 0).then(|| digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn file_matches(path: &Path, expected: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| sha256(&bytes))
        .as_deref()
        == Some(expected)
}

fn hook_layout(base: usize) -> bool {
    // 6F605 pushes "CreateInstance", calls this IAT entry, then calls the
    // cdecl factory. 6F9D0 passes (this, API) to public stdcall vtable+8.
    read::<u8>(base + 0x6F605) == Some(0x68)
        && read::<u32>(base + 0x6F606) == Some((base + 0x185BE0) as u32)
        && read::<[u8; 3]>(base + 0x6F60A) == Some([0x57, 0xFF, 0x15])
        && read::<u32>(base + 0x6F60D) == Some((base + GET_PROC_IAT) as u32)
        && read::<[u8; 15]>(base + 0x185BE0) == Some(*b"CreateInstance\0")
        && read::<[u8; 2]>(base + 0x6F61C) == Some([0xFF, 0xD0])
        && read::<[u8; 7]>(base + 0x6F9D0) == Some([0x52, 0x51, 0x8B, 0x01, 0xFF, 0x50, 0x08])
}

fn plugin_layout(base: usize) -> bool {
    read::<u32>(base + VTABLE_RVA - 4) == Some((base + 0xBCEE0) as u32)
        && VTABLE_RVAS.iter().enumerate().all(|(index, rva)| {
            read::<u32>(base + VTABLE_RVA + index * 4) == Some((base + rva) as u32)
        })
        && read::<[u8; 5]>(base + FACTORY_RVA) == Some([0x55, 0x8B, 0xEC, 0x6A, 0xFF])
        && read::<[u8; 5]>(base + 0x78940) == Some([0x55, 0x8B, 0xEC, 0x6A, 0xFF])
        && read::<[u8; 3]>(base + 0x78A35) == Some([0xC2, 0x08, 0x00])
        && read::<[u8; 3]>(base + 0x788EB) == Some([0xC2, 0x08, 0x00])
        && read::<[u8; 3]>(base + 0x78866) == Some([0xC2, 0x14, 0x00])
        && read::<[u8; 3]>(base + 0x78633) == Some([0xC2, 0x18, 0x00])
}

/// Change one aligned data pointer; never write executable code. A failed
/// page restore is reported even when the replacement is already active.
fn replace_pointer(address: usize, expected: usize, replacement: usize) -> Result<(), String> {
    if address % std::mem::align_of::<usize>() != 0 || read::<usize>(address) != Some(expected) {
        return Err("FFXIDB adapter pointer changed before installation".into());
    }
    let size = std::mem::size_of::<usize>();
    let mut previous = 0;
    if unsafe {
        VirtualProtect(
            address as *const c_void,
            size,
            PAGE_READWRITE,
            &mut previous,
        )
    } == 0
    {
        return Err(format!(
            "FFXIDB adapter could not make its pointer writable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let changed = unsafe { AtomicUsize::from_ptr(address as *mut usize) }
        .compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    let mut ignored = 0;
    let restored =
        unsafe { VirtualProtect(address as *const c_void, size, previous, &mut ignored) } != 0;
    if !restored {
        return Err("FFXIDB adapter could not restore pointer-page protection".into());
    }
    if !changed {
        return Err("FFXIDB adapter pointer was changed concurrently".into());
    }
    Ok(())
}

/// Call after Hook has loaded and before GameStart. Ok(true) means Hook's
/// factory interception is installed; the log confirms FFXIDB when it loads.
/// An unsupported Hook build is explicitly logged and left stock.
pub(crate) fn install() -> Result<bool, String> {
    let _serial = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    let name: Vec<_> = "Hook.dll\0".encode_utf16().collect();
    let mut module = std::ptr::null_mut();
    if unsafe { GetModuleHandleExW(0, name.as_ptr(), &mut module) } == 0 {
        return Ok(false);
    }
    let hook = Module(module as usize);
    let base = hook.0;
    if pe_identity(base) != Some(HOOK_ID)
        || !module_path(base).is_some_and(|path| file_matches(&path, HOOK_SHA256))
        || !hook_layout(base)
    {
        log!("note: FFXIDB shared-settings protection unavailable: unsupported Windower Hook.dll (requires verified 4.7.9.6)");
        return Ok(false);
    }
    let replacement = resolve as *const () as usize;
    let current =
        read::<usize>(base + GET_PROC_IAT).ok_or("Cannot read Windower GetProcAddress import")?;
    if current == replacement && BRIDGE.get().is_some() {
        return Ok(true);
    }
    // Do not bypass another tool's resolver or overwrite an unknown IAT hook.
    if current != GetProcAddress as *const () as usize {
        return Err("Windower's plugin resolver was already changed; cannot safely install FFXIDB settings protection".into());
    }
    let bridge = Bridge {
        _hook: hook,
        resolve: unsafe { std::mem::transmute::<usize, Resolve>(current) },
    };
    if let Err(other) = BRIDGE.set(bridge) {
        if BRIDGE.get().map(|b| b._hook.0) != Some(other._hook.0) {
            return Err("Windower module changed after preparing FFXIDB protection".into());
        }
    }
    replace_pointer(base + GET_PROC_IAT, current, replacement)?;
    log!("stage: Windower plugin resolver ready for FFXIDB shared-settings protection");
    Ok(true)
}

/// A kernel mutex is reentrant on the owning OS thread and shared across
/// processes. A Rust Mutex would neither allow callback reentry nor cover
/// another game. The guard is intentionally !Send and !Sync.
struct TransactionMutex(usize);
struct Transaction<'a> {
    mutex: &'a TransactionMutex,
    _same_thread: PhantomData<Rc<()>>,
}
impl TransactionMutex {
    fn open(name: &str) -> Result<Self, String> {
        let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "Cannot create FFXIDB settings mutex: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(handle as usize))
    }
    fn acquire(&self, timeout: u32) -> Result<Transaction<'_>, String> {
        let status = unsafe { WaitForSingleObject(self.0 as *mut c_void, timeout) };
        match status {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                if status == WAIT_ABANDONED {
                    log!("note: FFXIDB settings mutex recovered after another host exited during a callback");
                }
                Ok(Transaction {
                    mutex: self,
                    _same_thread: PhantomData,
                })
            }
            _ => Err(format!(
                "Cannot acquire FFXIDB settings mutex (wait status {status:#x}): {}",
                std::io::Error::last_os_error()
            )),
        }
    }
}
impl Drop for TransactionMutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as *mut c_void);
        }
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if unsafe { ReleaseMutex(self.mutex.0 as *mut c_void) } == 0 {
            log!("error: FFXIDB settings mutex release failed; terminating this host to prevent unlocked settings writes");
            std::process::abort();
        }
    }
}

fn settings_mutex_name(plugin: &Path) -> Result<String, String> {
    let canonical =
        std::fs::canonicalize(plugin).map_err(|e| format!("Cannot resolve FFXIDB path: {e}"))?;
    let directory = canonical.parent().ok_or("FFXIDB has no parent directory")?;
    let settings = directory.join("settings").join("FFXIDB.xml");
    // Canonicalize the existing DLL (the XML need not exist yet), resolving
    // junction/short-path aliases. Use a constant settings suffix and fold
    // case before hashing so spelling variations share the same mutex.
    let normalized = settings
        .as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    let bytes: Vec<_> = std::ffi::OsStr::new(&normalized)
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    let digest = sha256(&bytes).ok_or("Cannot hash FFXIDB settings path")?;
    Ok(format!(
        "Local\\CrystalLauncher.FFXIDB.settings.v1.{digest}"
    ))
}

fn shadow_table(original: &[usize; 34], rtti: usize) -> Box<[usize; 35]> {
    let mut shadow = Box::new([0; 35]);
    shadow[0] = rtti;
    shadow[1..].copy_from_slice(original);
    // XML entry points in this exact image:
    // - Initialize registers fields (173D0/17790/17B60/17F20) and loads the
    //   current character (3B550).
    // - Command -> 701F0 -> 238D0 (save all), 26170, or 6B3E0/6BD00/
    //   6C110/6C2D0 -> 25E70/25D60/25C40 -> 23DD0 (save one field).
    // - Mouse -> 72540 -> 6C110/6C2D0 -> 25C40 -> 23DD0.
    // - Incoming -> 78670 -> 3B550 loads character settings on 0x00A/0x00B.
    // Update/render/shutdown do not reach these XML helpers. Leave their
    // graphics/resource calls outside the mutex to avoid adding a lock order
    // between the settings lock and unrelated Hook/Direct3D callbacks.
    for (slot, wrapper) in [
        (2, initialize as *const () as usize),
        (7, command as *const () as usize),
        (11, incoming as *const () as usize),
        (13, mouse as *const () as usize),
    ] {
        shadow[slot + 1] = wrapper;
    }
    shadow
}

fn prepare(module: usize, factory: usize, path: &Path) -> Result<Option<Adapter>, String> {
    let plugin =
        Module::retain(module).ok_or("FFXIDB module disappeared during adapter preparation")?;
    if pe_identity(module) != Some(FFXIDB_ID)
        || !file_matches(path, FFXIDB_SHA256)
        || factory != module + FACTORY_RVA
        || !plugin_layout(module)
    {
        return Ok(None);
    }
    let transaction = TransactionMutex::open(&settings_mutex_name(path)?)?;
    {
        let _probe = transaction.acquire(30_000)?;
    }
    let original = VTABLE_RVAS.map(|rva| module + rva);
    let shadow = shadow_table(&original, module + 0xBCEE0);
    Ok(Some(Adapter {
        _plugin: plugin,
        factory: unsafe { std::mem::transmute::<usize, Factory>(factory) },
        original,
        original_vtable: module + VTABLE_RVA,
        shadow,
        transaction,
    }))
}

unsafe extern "system" fn resolve(module: HMODULE, name: PCSTR) -> FARPROC {
    let bridge = BRIDGE.get().unwrap_or_else(|| std::process::abort());
    let original = (bridge.resolve)(module, name);
    if original.is_none()
        || name as usize <= 0xFFFF
        || CStr::from_ptr(name.cast()).to_bytes() != b"CreateInstance"
    {
        return original;
    }
    if let Some(adapter) = ADAPTER.get() {
        if adapter._plugin.0 == module as usize {
            return Some(std::mem::transmute::<
                Factory,
                unsafe extern "system" fn() -> isize,
            >(create_instance));
        }
    }
    let Some(path) = module_path(module as usize) else {
        return original;
    };
    if !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("FFXIDB.dll"))
    {
        return original;
    }
    match prepare(
        module as usize,
        original.unwrap() as *const () as usize,
        &path,
    ) {
        Ok(Some(adapter)) => {
            let _ = ADAPTER.set(adapter);
            if ADAPTER.get().map(|a| a._plugin.0) != Some(module as usize) {
                log!("error: refusing a second FFXIDB image with a different settings location");
                return Some(std::mem::transmute::<
                    Factory,
                    unsafe extern "system" fn() -> isize,
                >(disabled_factory));
            }
            log!("stage: verified FFXIDB 1.2.3.14 shared-settings transaction protection prepared");
            Some(std::mem::transmute::<
                Factory,
                unsafe extern "system" fn() -> isize,
            >(create_instance))
        }
        Ok(None) => {
            log!("note: FFXIDB shared-settings protection unavailable: unsupported plugin build (requires verified 1.2.3.14); leaving stock callbacks");
            original
        }
        Err(error) => {
            log!("error: FFXIDB disabled before construction because settings protection failed: {error}");
            Some(std::mem::transmute::<
                Factory,
                unsafe extern "system" fn() -> isize,
            >(disabled_factory))
        }
    }
}

unsafe extern "C" fn disabled_factory() -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn create_instance() -> *mut c_void {
    let adapter = adapter();
    let _guard = match adapter.transaction.acquire(30_000) {
        Ok(guard) => guard,
        Err(error) => {
            log!("error: FFXIDB not constructed: {error}");
            return std::ptr::null_mut();
        }
    };
    let instance = (adapter.factory)();
    if instance.is_null() {
        return instance;
    }
    if let Err(error) = replace_pointer(
        instance as usize,
        adapter.original_vtable,
        adapter.shadow[1..].as_ptr() as usize,
    ) {
        // Do not guess a destructor for an object whose vptr is unexpected.
        // Nothing has been initialized or published to Hook yet. Its normal
        // null-factory path rejects the plugin; the small allocation remains.
        log!("error: FFXIDB not initialized: {error}");
        return std::ptr::null_mut();
    }
    log!("stage: FFXIDB settings callbacks protected across Crystal game hosts");
    instance
}

fn adapter() -> &'static Adapter {
    ADAPTER.get().unwrap_or_else(|| std::process::abort())
}
fn callback_transaction(adapter: &Adapter) -> Transaction<'_> {
    adapter
        .transaction
        .acquire(INFINITE)
        .unwrap_or_else(|error| {
            // A permanent valid handle cannot time out. Never call stock code
            // unlocked, or return from void Initialize pretending it succeeded.
            log!(
                "error: {error}; terminating this host to prevent unlocked FFXIDB settings writes"
            );
            std::process::abort()
        })
}

unsafe extern "system" fn initialize(this: *mut c_void, api: *mut c_void) {
    let a = adapter();
    let _guard = callback_transaction(a);
    let original: Initialize = std::mem::transmute(a.original[2]);
    original(this, api);
}
unsafe extern "system" fn command(this: *mut c_void, text: *const u8) {
    let a = adapter();
    let _guard = callback_transaction(a);
    let original: Command = std::mem::transmute(a.original[7]);
    original(this, text);
}
unsafe extern "system" fn incoming(
    this: *mut c_void,
    previous: *const u8,
    data: *const u8,
    size: u32,
    blocked: u8,
) -> u8 {
    let a = adapter();
    let _guard = callback_transaction(a);
    let original: Incoming = std::mem::transmute(a.original[11]);
    original(this, previous, data, size, blocked)
}
unsafe extern "system" fn mouse(
    this: *mut c_void,
    kind: u32,
    x: i32,
    y: i32,
    wheel: i32,
    blocked: u8,
) -> u8 {
    let a = adapter();
    let _guard = callback_transaction(a);
    let original: Mouse = std::mem::transmute(a.original[13]);
    original(this, kind, x, y, wheel, blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command as Process, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;

    fn unique() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
    fn test_mutex() -> TransactionMutex {
        TransactionMutex::open(&format!("Local\\CrystalLauncher.FFXIDB.test.{}", unique())).unwrap()
    }

    #[repr(C)]
    struct Fixture {
        vptr: usize,
        calls: u32,
        api: usize,
        command: usize,
    }
    static FIXTURE_VTABLE: OnceLock<[usize; 34]> = OnceLock::new();
    unsafe extern "C" fn fixture_factory() -> *mut c_void {
        Box::into_raw(Box::new(Fixture {
            vptr: FIXTURE_VTABLE.get().unwrap().as_ptr() as usize,
            calls: 0,
            api: 0,
            command: 0,
        }))
        .cast()
    }
    unsafe extern "system" fn fixture_initialize(this: *mut c_void, api: *mut c_void) {
        let object = &mut *this.cast::<Fixture>();
        object.api = api as usize;
        object.calls |= 1;
        // Reenter a wrapped callback on the same thread while Initialize owns
        // the mutex; this would deadlock with a nonreentrant Rust mutex.
        let f: Command = std::mem::transmute(*(object.vptr as *const usize).add(7));
        f(this, c"nested".as_ptr().cast());
    }
    unsafe extern "system" fn fixture_shutdown(this: *mut c_void) {
        drop(Box::from_raw(this.cast::<Fixture>()));
    }
    unsafe extern "system" fn fixture_update(this: *mut c_void) {
        (*this.cast::<Fixture>()).calls |= 2;
    }
    unsafe extern "system" fn fixture_render(this: *mut c_void) {
        (*this.cast::<Fixture>()).calls |= 4;
    }
    unsafe extern "system" fn fixture_command(this: *mut c_void, text: *const u8) {
        (*this.cast::<Fixture>()).command = text as usize;
    }
    unsafe extern "system" fn fixture_incoming(
        this: *mut c_void,
        previous: *const u8,
        data: *const u8,
        size: u32,
        blocked: u8,
    ) -> u8 {
        assert!(!this.is_null());
        assert_eq!(
            (previous as usize, data as usize, size, blocked),
            (0xAABBCCDD, 0x12345678, 0x98765432, 0xA5)
        );
        0xC3
    }
    unsafe extern "system" fn fixture_mouse(
        this: *mut c_void,
        kind: u32,
        x: i32,
        y: i32,
        wheel: i32,
        blocked: u8,
    ) -> u8 {
        assert!(!this.is_null());
        assert_eq!(
            (kind, x, y, wheel, blocked),
            (0xFFFFFFFF, -500, 700, -120, 0xA5)
        );
        0xC7
    }

    #[test]
    fn factory_and_stdcall_callbacks_preserve_arguments_results_reentry_and_reload() {
        let mut original = [0x1234usize; 34];
        for (slot, f) in [
            (2, fixture_initialize as *const () as usize),
            (3, fixture_shutdown as *const () as usize),
            (5, fixture_update as *const () as usize),
            (6, fixture_render as *const () as usize),
            (7, fixture_command as *const () as usize),
            (11, fixture_incoming as *const () as usize),
            (13, fixture_mouse as *const () as usize),
        ] {
            original[slot] = f;
        }
        assert!(FIXTURE_VTABLE.set(original).is_ok());
        let shadow = shadow_table(&original, 0x5678);
        let stable_table = shadow[1..].as_ptr() as usize;
        for slot in 0..34 {
            if ![2, 7, 11, 13].contains(&slot) {
                assert_eq!(shadow[slot + 1], original[slot]);
            }
        }
        assert!(ADAPTER
            .set(Adapter {
                _plugin: Module(0),
                factory: fixture_factory,
                original,
                original_vtable: FIXTURE_VTABLE.get().unwrap().as_ptr() as usize,
                shadow,
                transaction: test_mutex()
            })
            .is_ok());
        for _ in 0..64 {
            unsafe {
                let object = create_instance();
                assert!(!object.is_null());
                let table = (*object.cast::<Fixture>()).vptr as *const usize;
                assert_eq!(table as usize, stable_table);
                assert_eq!(*table.sub(1), 0x5678);
                let init: Initialize = std::mem::transmute(*table.add(2));
                init(object, 0xF1234567usize as *mut c_void);
                assert_eq!(
                    CStr::from_ptr((*object.cast::<Fixture>()).command as *const i8).to_bytes(),
                    b"nested"
                );
                let tick: Notify = std::mem::transmute(*table.add(5));
                tick(object);
                let draw: Notify = std::mem::transmute(*table.add(6));
                draw(object);
                let cmd: Command = std::mem::transmute(*table.add(7));
                let save = c"save".as_ptr().cast();
                cmd(object, save);
                let packet: Incoming = std::mem::transmute(*table.add(11));
                assert_eq!(
                    packet(
                        object,
                        0xAABBCCDDusize as *const u8,
                        0x12345678usize as *const u8,
                        0x98765432,
                        0xA5
                    ),
                    0xC3
                );
                let input: Mouse = std::mem::transmute(*table.add(13));
                assert_eq!(input(object, 0xFFFFFFFF, -500, 700, -120, 0xA5), 0xC7);
                assert_eq!(
                    (
                        (*object.cast::<Fixture>()).calls,
                        (*object.cast::<Fixture>()).api
                    ),
                    (7, 0xF1234567)
                );
                assert_eq!((*object.cast::<Fixture>()).command, save as usize);
                let release: Notify = std::mem::transmute(*table.add(3));
                release(object);
            }
        }
    }

    #[test]
    fn another_thread_cannot_enter_until_all_recursive_guards_release() {
        let mutex = std::sync::Arc::new(test_mutex());
        let first = mutex.acquire(0).unwrap();
        let second = mutex.acquire(0).unwrap();
        for guard in [second, first] {
            let other = mutex.clone();
            assert_eq!(
                thread::spawn(move || unsafe { WaitForSingleObject(other.0 as *mut c_void, 0) })
                    .join()
                    .unwrap(),
                WAIT_TIMEOUT
            );
            drop(guard);
        }
        let other = mutex.clone();
        assert!(thread::spawn(move || {
            let _guard = other.acquire(1000).unwrap();
            true
        })
        .join()
        .unwrap());
    }

    // Separate test-binary processes exercise the actual named kernel mutex,
    // without loading Windower/FFXIDB or executing any vendor code.
    #[test]
    fn settings_child_process() {
        let Some(root) = std::env::var_os("CRYSTAL_FFXIDB_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let role = std::env::var("CRYSTAL_FFXIDB_TEST_ROLE").unwrap();
        let mutex =
            TransactionMutex::open(&std::env::var("CRYSTAL_FFXIDB_TEST_MUTEX").unwrap()).unwrap();
        std::fs::write(root.join(format!("{role}.ready")), b"ready").unwrap();
        for iteration in 0..12 {
            let _guard = mutex.acquire(5000).unwrap();
            let _nested = mutex.acquire(0).unwrap();
            std::fs::write(root.join(format!("{role}.entered")), b"entered").unwrap();
            let path = root.join("FFXIDB.xml");
            // The real failure is a failed read becoming a fresh document,
            // followed by a non-atomic truncating save. Keep that behavior in
            // this fixture so only the transaction mutex prevents the loss.
            let mut xml = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| "<settings><global/></settings>".into());
            thread::sleep(Duration::from_millis(3));
            let field = format!(
                "<{role}{iteration}><X>{}</X><Y>55</Y></{role}{iteration}>",
                700 + iteration
            );
            xml = xml.replace("</settings>", &(field + "</settings>"));
            use std::{io::Write, os::windows::fs::OpenOptionsExt};
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .share_mode(0)
                .open(path)
                .unwrap();
            thread::sleep(Duration::from_millis(3));
            output.write_all(xml.as_bytes()).unwrap();
        }
    }

    #[test]
    fn simultaneous_process_transactions_preserve_global_and_character_coordinates() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let initial = "<settings><global><X>120</X><Y>240</Y></global><Existing><X>360</X><Y>480</Y></Existing></settings>";
        std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
        let name = format!("Local\\CrystalLauncher.FFXIDB.test.{}", unique());
        let mutex = TransactionMutex::open(&name).unwrap();
        let guard = mutex.acquire(0).unwrap();
        let mut children: Vec<_> = ["Alpha", "Beta"]
            .into_iter()
            .map(|role| {
                Process::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windower_plugin_settings::tests::settings_child_process",
                        "--nocapture",
                    ])
                    .env("CRYSTAL_FFXIDB_TEST_ROOT", &root)
                    .env("CRYSTAL_FFXIDB_TEST_ROLE", role)
                    .env("CRYSTAL_FFXIDB_TEST_MUTEX", &name)
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let start = Instant::now();
        while !["Alpha", "Beta"]
            .iter()
            .all(|role| root.join(format!("{role}.ready")).exists())
        {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "both child processes must reach the held mutex"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("Alpha.entered").exists());
        assert!(!root.join("Beta.entered").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap(),
            initial
        );
        drop(guard);
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
        assert!(xml.contains("<global><X>120</X><Y>240</Y></global>"));
        assert!(xml.contains("<Existing><X>360</X><Y>480</Y></Existing>"));
        for role in ["Alpha", "Beta"] {
            for i in 0..12 {
                assert!(xml.contains(&format!(
                    "<{role}{i}><X>{}</X><Y>55</Y></{role}{i}>",
                    700 + i
                )));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_aliases_share_one_mutex_but_other_installations_do_not() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-path-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let plugin = root.join("FFXIDB.dll");
        std::fs::write(&plugin, b"fixture").unwrap();
        let a = settings_mutex_name(&plugin).unwrap();
        let b = settings_mutex_name(&root.join(".").join("ffxidb.DLL")).unwrap();
        assert_eq!(a, b);
        let different = root.join("different");
        std::fs::create_dir(&different).unwrap();
        std::fs::write(different.join("FFXIDB.dll"), b"fixture").unwrap();
        assert_ne!(
            a,
            settings_mutex_name(&different.join("FFXIDB.dll")).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn map_pe(file: &[u8]) -> Vec<u8> {
        let u16_at = |at| u16::from_le_bytes(file[at..at + 2].try_into().unwrap()) as usize;
        let u32_at = |at| u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        let nt = u32_at(0x3C);
        let mut image = vec![0; u32_at(nt + 80)];
        let headers = u32_at(nt + 84);
        image[..headers].copy_from_slice(&file[..headers]);
        for index in 0..u16_at(nt + 6) {
            let section = nt + 24 + u16_at(nt + 20) + index * 40;
            let (rva, size, raw) = (
                u32_at(section + 12),
                u32_at(section + 16),
                u32_at(section + 20),
            );
            image[rva..rva + size].copy_from_slice(&file[raw..raw + size]);
        }
        let delta = (image.as_ptr() as u32).wrapping_sub(u32_at(nt + 52) as u32);
        let mut at = u32_at(nt + 24 + 96 + 5 * 8);
        let end = at + u32_at(nt + 24 + 100 + 5 * 8);
        while at < end {
            let page = u32::from_le_bytes(image[at..at + 4].try_into().unwrap()) as usize;
            let size = u32::from_le_bytes(image[at + 4..at + 8].try_into().unwrap()) as usize;
            assert!(size >= 8);
            for offset in (at + 8..at + size).step_by(2) {
                let entry = u16::from_le_bytes(image[offset..offset + 2].try_into().unwrap());
                if entry >> 12 == 3 {
                    let target = page + (entry & 0xFFF) as usize;
                    let value = u32::from_le_bytes(image[target..target + 4].try_into().unwrap())
                        .wrapping_add(delta);
                    image[target..target + 4].copy_from_slice(&value.to_le_bytes());
                } else {
                    assert_eq!(entry >> 12, 0);
                }
            }
            at += size;
        }
        image
    }

    #[test]
    #[ignore = "Read-only installed PE verification; neither vendor DLL is loaded or executed"]
    fn installed_builds_match_adapter_and_callback_abi() {
        let root = std::env::var_os("CRYSTAL_WINDOWER_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Games\Final Fantasy XI Programs\Windower 4"));
        for (path, hash, id, layout) in [
            (
                root.join("Hook.dll"),
                HOOK_SHA256,
                HOOK_ID,
                hook_layout as fn(usize) -> bool,
            ),
            (
                root.join("plugins").join("FFXIDB.dll"),
                FFXIDB_SHA256,
                FFXIDB_ID,
                plugin_layout as fn(usize) -> bool,
            ),
        ] {
            let file = std::fs::read(path).unwrap();
            assert_eq!(sha256(&file).as_deref(), Some(hash));
            let mut image = map_pe(&file);
            let base = image.as_ptr() as usize;
            assert_eq!(pe_identity(base), Some(id));
            assert!(layout(base));
            if id == FFXIDB_ID {
                image[VTABLE_RVA + 2 * 4] ^= 1;
            } else {
                image[0x6F60B] ^= 1;
            }
            assert!(
                !layout(base),
                "changed callback/loader layout must be refused"
            );
        }
    }
}
