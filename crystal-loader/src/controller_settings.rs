//! Live controller changes run in FFXI's verified keyboard frame. The stock
//! config-tool sequence is suspend background input, reload, then resume it.
//! The following stock pad frame releases/recreates the device when required.
//! A result is sent only after a later frame observes the new values and the
//! recreation flag cleared. No external memory writes or synthetic keys.

use std::{ffi::c_void, sync::{mpsc, Mutex}, thread, time::{Duration, Instant}};
use windows_sys::Win32::{
    Foundation::HWND,
    Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
    System::{Diagnostics::Debug::ReadProcessMemory, SystemInformation::GetTickCount64,
        Threading::{GetCurrentProcess, GetCurrentThreadId}},
    UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId, IsWindow},
};
use crate::{ffxi_settings::{FfxiSettings, SettingValue}, game_layout::Resolution, log};

const LIMIT: Duration = Duration::from_secs(4);
const UNAVAILABLE: &str = "Live controller settings are unavailable for this game session; saved changes apply after restarting it.";
const EXPIRED: &str = "The controller update could not be confirmed before the request expired; saved changes apply after restarting the game.";
const EXITED: &str = "The game closed before the controller update could be confirmed.";
const RELOAD: usize = 0x2840;
const SUSPEND: usize = 0x5600;
const CONFIG: usize = 0x3D3CB8;
const KEYBOARD: usize = 0x455C20;
const FLAGS: usize = 0x455C34;
const BINDINGS: usize = 0x455B20;
const GUID: usize = 0x455B10;
const CONFIG_TOOL: usize = 0x662DBC;

// Exact bodies and parser tables in the two independently dumped clients. Relocation operands
// are restored to the preferred base before hashing, so ASLR is supported.
// Reload 2840 takes (this, initial: u32) and returns with ret 4. Suspend 5600
// takes (keyboard, suspended: u32), also ret 4. The native WndProc uses reload
// with false at 16D32; config launch/finish use suspend at 1BCE13/1BCE8E.
// Keep the entire parsers and pad recreation branch in the check, not just
// their prologues: their global addresses and field offsets are used below.
const CODE: &[(usize, usize, &[usize])] = &[
    (0x2840, 0x2D5, &[0x28, 0x46, 0x66, 0x7A, 0x89, 0xAC, 0xC5, 0xD3, 0xE2, 0xF8, 0x106, 0x131, 0x142, 0x150, 0x167, 0x175, 0x183, 0x191, 0x196, 0x19C, 0x1AD, 0x1B2, 0x1CF, 0x24C, 0x27B, 0x2B3, 0x2BD]),
    (0x5130, 0x355, &[0x27, 0x33, 0x47, 0x4C, 0x62, 0x81, 0x86, 0x8C, 0x92, 0x98, 0xA4, 0xA9, 0xAF, 0xB6, 0xE9, 0xF0, 0x105, 0x10C, 0x114, 0x11C, 0x14C, 0x154, 0x170, 0x18A, 0x190, 0x1B8, 0x1CB, 0x1D5, 0x1E0, 0x219, 0x21E, 0x224, 0x22E, 0x241, 0x246, 0x25A, 0x25F, 0x276, 0x2BD, 0x2C4, 0x2D9, 0x2E0, 0x318, 0x32C, 0x332, 0x33B, 0x346]),
    (0x5490, 0x1C4, &[0x4, 0xA, 0x10, 0x19, 0x2D, 0x35, 0x3D, 0x45, 0x4D, 0x77, 0xA0, 0xC6, 0xD3, 0xE1, 0xEB, 0xF2, 0xF8, 0xFF, 0x11C, 0x135, 0x13F, 0x149, 0x151, 0x159, 0x179, 0x184, 0x18A, 0x18F, 0x195, 0x1A1, 0x1A7, 0x1AC, 0x1BD]),
    (0x60EB, 0x78, &[0x1, 0x1A, 0x29, 0x35, 0x5D, 0x6E]),
    (0x83D0, 0x3A0, &[0x1, 0x30, 0x3C, 0x58, 0x7A, 0x12F, 0x14F, 0x16F, 0x2A6, 0x2B9, 0x2BE, 0x2D2, 0x2E2, 0x31E, 0x37A]),
    (0x8D90, 0x104, &[0x1, 0x19, 0x36, 0x3F, 0x47, 0x51, 0x5B, 0x64, 0x71, 0x81, 0x92, 0x99, 0xA2, 0xA7, 0xAF, 0xB4, 0xB9, 0xC2, 0xC7, 0xCD, 0xD4, 0xE1, 0xEA, 0xF5, 0xFB]),
    (0x18FC0, 0x40, &[0x25]),
    (0x19060, 0x7A, &[0x35, 0x60]),
    (0x1286A, 0x16, &[0x2, 0xD]),
    (0x1BCC30, 0x65, &[0x2, 0xC, 0x3B, 0x5D]),
    (0x1BCE07, 0x11, &[0x2]),
    (0x1BCE30, 0x78, &[0x14, 0x2C, 0x47, 0x4F, 0x59]),
    (0x34F930, 0x84, &[]), // source-slot map, including its terminating -1
    (0x34F9EC, 0x3, &[]), // strtok delimiters
    (0x34F47C, 0xB, &[]), // padmode000
    (0x34F424, 0xA, &[]), // padsin000
    (0x34F3D0, 0xC, &[]), // padexsin000
    (0x34F374, 0xB, &[]), // padguid000
];
const OLD_HASH: &str = "cb153e02eb44290e529b4e50a6f4ff7e492391ceaadd688517539d4fd2b86abb";
const CURRENT_HASH: &str = "7141598351ddfb55a56c88d7b7fd5355a73db76b122ce9b5eb5fe0c990101ab2";

fn read<T: Copy>(address: usize) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut count = 0;
    let size = std::mem::size_of::<T>();
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const c_void,
        value.as_mut_ptr().cast(), size, &mut count) };
    (ok != 0 && count == size).then(|| unsafe { value.assume_init() })
}

fn read_bytes(address: usize, bytes: &mut [u8]) -> bool {
    let mut count = 0;
    // Windows rejects stale/unmapped objects; Rust never dereferences them.
    unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const c_void,
        bytes.as_mut_ptr().cast(), bytes.len(), &mut count) != 0 && count == bytes.len() }
}

fn pointer(address: usize) -> Option<usize> {
    read::<u32>(address).map(|value| value as usize)
        .filter(|value| *value >= 0x10000 && *value <= usize::MAX - 0x10000 && value % 4 == 0)
}

fn code_hash(base: usize, mut read: impl FnMut(usize, &mut [u8]) -> bool) -> Option<String> {
    let mut joined = Vec::new();
    for &(rva, length, relocations) in CODE {
        let mut bytes = vec![0u8; length];
        if !read(rva, &mut bytes) { return None; }
        for &offset in relocations {
            let actual = u32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
            let target = actual.checked_sub(base).filter(|rva| *rva < 0xC15000)?;
            bytes[offset..offset + 4].copy_from_slice(&(0x10000000u32 + target as u32).to_le_bytes());
        }
        joined.extend_from_slice(&bytes);
    }
    let mut digest = [0u8; 32];
    let result = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0,
        joined.as_ptr(), joined.len() as u32, digest.as_mut_ptr(), digest.len() as u32) };
    (result == 0).then(|| digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Clone, Copy)]
struct Layout { base: usize, window: usize }
impl Layout {
    fn resolve(resolution: &Resolution) -> Result<Self, &'static str> {
        if resolution.size != 0xC15000
            || resolution.input.as_ref().map(|keys| keys.keyboard).ok() != Some(KEYBOARD) {
            return Err(UNAVAILABLE);
        }
        let window = resolution.menus.as_ref().map_err(|_| UNAVAILABLE)?.window;
        let layout = Self { base: resolution.base, window };
        layout.verify().then_some(layout).ok_or(UNAVAILABLE)
    }

    fn verify(self) -> bool {
        matches!(code_hash(self.base, |rva, out| read_bytes(self.base + rva, out)).as_deref(),
            Some(OLD_HASH | CURRENT_HASH))
    }

    fn context(self, data: *mut u8) -> Option<Context> {
        let keyboard = pointer(self.base + KEYBOARD)?;
        if keyboard.checked_add(crate::auto_login::KEYBOARD_BUFFER) != Some(data as usize) { return None; }
        let window = read::<u32>(self.base + self.window)? as usize as HWND;
        let mut pid = 0;
        let mut class = [0u16; 32];
        let owned = unsafe { !window.is_null() && IsWindow(window) != 0
            && GetWindowThreadProcessId(window, &mut pid) == GetCurrentThreadId()
            && pid == std::process::id()
            && GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) == 9
            && class[..9] == [70, 70, 88, 105, 67, 108, 97, 115, 115] };
        owned.then_some(Context { keyboard, window: window as usize })
    }

    fn flags(self) -> Option<[u8; 15]> { read(self.base + FLAGS) }

    fn applied(self, expected: &Expected) -> bool {
        let Some(flags) = self.flags() else { return false; };
        let Some(bindings) = read::<[i8; 32]>(self.base + BINDINGS) else { return false; };
        let Some(guid) = read::<[u8; 16]>(self.base + GUID) else { return false; };
        expected.matches(&flags, &bindings, &guid)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context { keyboard: usize, window: usize }

#[derive(Clone, Debug, PartialEq, Eq)]
struct Expected { mode: [u8; 6], bindings: Vec<i8>, guid: Option<[u8; 16]> }
impl Expected {
    fn parse(settings: &FfxiSettings) -> Result<Self, &'static str> {
        const INVALID: &str = "The live request contains invalid controller settings.";
        if settings.names().any(|name| !["padmode000", "padsin000", "padguid000"].iter().any(|key| name.eq_ignore_ascii_case(key))) {
            return Err(INVALID);
        }
        let text = |name| match settings.get(name) { Some(SettingValue::Text(value)) if value.is_ascii() && value.len() < 256 => Ok(value.as_str()), _ => Err(INVALID) };
        let list = |value: &str| -> Result<Vec<i32>, &'static str> {
            value.split(',').map(|part| part.trim().parse::<i32>().map_err(|_| INVALID)).collect()
        };
        let mode = list(text("padmode000")?)?;
        let bindings = list(text("padsin000")?)?;
        if !(6..=32).contains(&mode.len()) || mode.iter().any(|flag| !(0..=1).contains(flag))
            || !(27..=32).contains(&bindings.len()) || bindings.iter().any(|id| !(-41..=41).contains(id)) {
            return Err(INVALID);
        }
        let guid = if settings.get("padguid000").is_some() {
            Some(parse_guid(text("padguid000")?).ok_or(INVALID)?)
        } else { None };
        Ok(Self { mode: std::array::from_fn(|index| mode[index] as u8),
            bindings: bindings.into_iter().map(|id| id as i8).collect(), guid })
    }

    fn matches(&self, flags: &[u8; 15], bindings: &[i8; 32], guid: &[u8; 16]) -> bool {
        // Mode order: enabled, rumble, sliders, hat, background, XInput.
        [0, 5, 2, 1, 3, 4].into_iter().enumerate().all(|(i, offset)| flags[offset] == self.mode[i])
            && flags[12] == 0 && flags[14] == 0
            // Stock skips binding parsing while the pad is disabled. The
            // retained process-local string is read when it is enabled again.
            && (self.mode[0] == 0 || bindings[..self.bindings.len()] == self.bindings)
            && self.guid.as_ref().is_none_or(|expected| expected == guid)
    }
}

fn parse_guid(text: &str) -> Option<[u8; 16]> {
    // The stock 19060 reader calls CLSIDFromString, which requires braces.
    let text = text.strip_prefix('{')?.strip_suffix('}')?;
    if text.len() != 36 || [8, 13, 18, 23].iter().any(|&i| text.as_bytes()[i] != b'-') { return None; }
    let hex: String = text.chars().filter(|&ch| ch != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) { return None; }
    let mut out = [0u8; 16];
    for (i, value) in out.iter_mut().enumerate() { *value = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?; }
    out[..4].reverse(); out[4..6].reverse(); out[6..8].reverse();
    Some(out)
}

fn transition(flags: &[u8; 15], expected: &Expected, stock_config: u8) -> Result<(), &'static str> {
    if [0, 1, 2, 3, 4, 5, 12, 13, 14].iter().any(|&offset| flags[offset] > 1) { return Err(UNAVAILABLE); }
    if stock_config > 1 { return Err(UNAVAILABLE); }
    if stock_config != 0 || flags[14] != 0 { return Err("Close the game's controller configuration window before applying live changes."); }
    if flags[4] != 0 && expected.mode[5] == 0 {
        // 83D0 sets +C9A when XInput is enabled, but never clears it when
        // disabled. Reusing that path would keep detecting the pad as XInput.
        return Err("Turning XInput off requires restarting the game; the new setting has been saved.");
    }
    Ok(())
}

struct Request { id: u64, settings: FfxiSettings, expected: Expected, expires: Instant, deadline_ms: u64 }
impl Request {
    fn new(id: u64, settings: FfxiSettings, deadline_ms: u64, now: Instant, ticks: u64) -> Result<Self, &'static str> {
        if deadline_ms <= ticks { return Err(EXPIRED); }
        let expected = Expected::parse(&settings)?;
        let duration = LIMIT.min(Duration::from_millis(deadline_ms - ticks));
        Ok(Self { id, settings, expected, expires: now + duration, deadline_ms })
    }
    fn expired(&self, now: Instant, ticks: u64) -> bool { self.expires <= now || self.deadline_ms <= ticks }
}
struct Waiting { request: Request, context: Context }
#[derive(Default)]
struct State { layout: Option<Layout>, unsupported: bool, pending: Option<Request>, waiting: Option<Waiting> }
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn result(id: u64, value: Result<(), &'static str>) {
    match value {
        Ok(()) => crate::events::send(crate::proto::controller_result(id, true, "")),
        Err(error) => crate::events::send(crate::proto::controller_result(id, false, error)),
    }
}

impl State {
    fn cancel(&mut self, error: &'static str) {
        if let Some(request) = self.pending.take() { result(request.id, Err(error)); }
        if let Some(waiting) = self.waiting.take() { result(waiting.request.id, Err(error)); }
    }
    fn expire(&mut self, now: Instant, ticks: u64) {
        if self.pending.as_ref().is_some_and(|request| request.expired(now, ticks)) {
            result(self.pending.take().unwrap().id, Err(EXPIRED));
        }
        if self.waiting.as_ref().is_some_and(|waiting| waiting.request.expired(now, ticks)) {
            result(self.waiting.take().unwrap().request.id, Err(EXPIRED));
        }
    }
}

/// Called by the pipe reader. One pending update is retained, so dragging or
/// repeated edits cannot build an unbounded queue of stale game mutations.
pub(crate) fn submit(id: u64, settings: FfxiSettings, deadline_ms: u64) {
    let request = match Request::new(id, settings, deadline_ms, Instant::now(), unsafe { GetTickCount64() }) {
        Ok(value) => value, Err(error) => { result(id, Err(error)); return; }
    };
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut().filter(|state| !state.unsupported) else { result(id, Err(UNAVAILABLE)); return; };
    if let Some(old) = state.pending.replace(request) {
        result(old.id, Err("A newer controller change replaced this request."));
    }
}

/// The shared GetKeyboardState wrapper calls this after the real API, before
/// the stock pad frame. Wrong buffers/threads never consume a request.
pub(crate) fn on_input(data: *mut u8) {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else { return; };
    if state.pending.is_none() && state.waiting.is_none() { return; }
    if crate::events::pipe_closed() { state.cancel(EXITED); return; }
    let Some(layout) = state.layout else { return; };
    let Some(context) = layout.context(data) else { return; };
    state.expire(Instant::now(), unsafe { GetTickCount64() });
    if let Some(waiting) = state.waiting.as_ref() {
        if waiting.context != context {
            result(state.waiting.take().unwrap().request.id, Err(EXITED));
        } else if layout.applied(&waiting.request.expected) {
            result(state.waiting.take().unwrap().request.id, Ok(()));
        } else {
            return;
        }
    }
    let Some(request) = state.pending.take() else { return; };
    match apply(layout, context, &request) {
        Ok(()) => state.waiting = Some(Waiting { request, context }),
        Err(error) => result(request.id, Err(error)),
    }
}

fn apply(layout: Layout, context: Context, request: &Request) -> Result<(), &'static str> {
    if request.expired(Instant::now(), unsafe { GetTickCount64() }) { return Err(EXPIRED); }
    if !layout.verify() { return Err(UNAVAILABLE); }
    let flags = layout.flags().ok_or(UNAVAILABLE)?;
    // The config UI sets +4 before launching its asynchronous worker, which
    // suspends input later. Checking only the suspended flag would race it.
    let stock_config = pointer(layout.base + CONFIG_TOOL).and_then(|object| read::<u8>(object + 4)).ok_or(UNAVAILABLE)?;
    transition(&flags, &request.expected, stock_config)?;
    let config = pointer(layout.base + CONFIG).ok_or(UNAVAILABLE)?;
    // Exact constructor/registry ownership and object extents. The keyboard
    // pointer is also the currently executing frame's +298 buffer owner.
    if read::<u32>(config) != Some((layout.base + 0x3295FC) as u32)
        || read::<u32>(config + 0x8C) != Some((layout.base + 0x32A064) as u32)
        || read::<u32>(config + 0x94) != Some(1)
        || read::<[u8; 0xB4]>(config).is_none()
        || read::<[u8; 0xDC4]>(context.keyboard).is_none()
        || pointer(layout.base + KEYBOARD) != Some(context.keyboard) {
        return Err(UNAVAILABLE);
    }
    let key = read::<u32>(config + 0x90).filter(|key| *key != 0).ok_or(UNAVAILABLE)? as usize;
    #[cfg(target_arch = "x86")]
    {
        // The sender's deadline uses the same Windows boot clock in both
        // processes. Recheck after validation, immediately before mutation.
        if request.expired(Instant::now(), unsafe { GetTickCount64() }) { return Err(EXPIRED); }
        crate::ffxi_settings::update_controller_values(&request.settings, key)?;
        type Method = unsafe extern "thiscall" fn(usize, u32);
        // Exact bodies/ret 4 ABIs are checked immediately above. These are
        // the game's own official config calls on the current input thread.
        unsafe {
            let suspend: Method = std::mem::transmute(layout.base + SUSPEND);
            let reload: Method = std::mem::transmute(layout.base + RELOAD);
            suspend(context.keyboard, 1);
            reload(config, 0);
            suspend(context.keyboard, 0);
        }
        Ok(())
    }
    #[cfg(not(target_arch = "x86"))]
    { let _ = (key, request); Err(UNAVAILABLE) }
}

pub(crate) struct Worker { stop: mpsc::Sender<()>, thread: Option<thread::JoinHandle<()>> }
impl Worker {
    pub(crate) fn start() -> Option<Self> {
        if !cfg!(target_arch = "x86") || !crate::ffxi_settings::active() { result(0, Err(UNAVAILABLE)); return None; }
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() { return None; }
        *guard = Some(State::default());
        let (stop, receive) = mpsc::channel();
        let spawned = thread::Builder::new().name("crystal-controller-settings".into()).spawn(move || {
            let mut hook = None;
            loop {
                if receive.recv_timeout(Duration::from_millis(100)) != Err(mpsc::RecvTimeoutError::Timeout) { break; }
                let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state) = guard.as_mut() else { break; };
                if crate::events::pipe_closed() { state.cancel(EXITED); break; }
                state.expire(Instant::now(), unsafe { GetTickCount64() });
                if state.layout.is_some() || state.unsupported { continue; }
                drop(guard);
                let Some(resolution) = crate::game_layout::ffximain() else { continue; };
                if !crate::ffxi_settings::controller_ready() { continue; }
                let prepared = Layout::resolve(resolution).and_then(|layout| {
                    crate::auto_login::input_hook(resolution).map(|hook| (layout, hook)).map_err(|_| UNAVAILABLE)
                });
                let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state) = guard.as_mut() else { break; };
                match prepared {
                    Ok((layout, lease)) => { state.layout = Some(layout); hook = Some(lease); result(0, Ok(())); }
                    Err(error) => { state.unsupported = true; state.cancel(error); result(0, Err(error)); log!("controller settings: {error}"); }
                }
            }
            drop(hook);
        });
        match spawned {
            Ok(thread) => Some(Self { stop, thread: Some(thread) }),
            Err(_) => { *guard = None; result(0, Err(UNAVAILABLE)); None }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Cancel synchronously before waiting for the worker. A callback that
        // already holds the lock completes first; no callback can use retired
        // settings after this point, including on another GameStart cycle.
        if let Some(mut state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).take() { state.cancel(EXITED); }
        result(0, Err(EXITED));
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str, bindings: &str) -> FfxiSettings {
        FfxiSettings::new(vec![("padmode000".into(), SettingValue::Text(mode.into())),
            ("padsin000".into(), SettingValue::Text(bindings.into()))]).unwrap()
    }
    fn bindings(count: usize, id: i32) -> String { vec![id.to_string(); count].join(",") }

    #[test]
    fn native_parser_preserves_extended_slots_and_rejects_unsafe_raw_values() {
        let extra = bindings(28, -41);
        assert_eq!(Expected::parse(&settings("1,1,0,0,1,0", &extra)).unwrap().bindings, vec![-41; 28]);
        assert!(Expected::parse(&settings("1,1,0,0,1,0,1,0", &bindings(32, 41))).is_ok());
        assert!(Expected::parse(&settings("1,1,0,0,1,0,2", &extra)).is_err(), "unknown mode tail is not silently dropped");
        assert!(Expected::parse(&settings("1,1,0,0,1,0", &format!("{},42", bindings(27, 1)))).is_err(),
            "unsafe binding tail is preserved by the launcher but never passed to the native parser");
        for value in [bindings(26, 0), bindings(33, 0), bindings(27, 128), bindings(27, i32::MIN), format!("{},", bindings(27, 1))] {
            assert!(Expected::parse(&settings("1,1,0,0,1,0", &value)).is_err());
        }
        assert!(Expected::parse(&settings("1,1,0,0,2,0", &extra)).is_err());
        let bad = FfxiSettings::new(vec![("0001".into(), SettingValue::Dword(1920))]).unwrap();
        assert!(Expected::parse(&bad).is_err());
        assert_eq!(parse_guid("{00112233-4455-6677-8899-AABBCCDDEEFF}"),
            Some([0x33,0x22,0x11,0,0x55,0x44,0x77,0x66,0x88,0x99,0xAA,0xBB,0xCC,0xDD,0xEE,0xFF]));
        assert!(parse_guid("00112233-4455-6677-8899-AABBCCDDEEFF").is_none());
        assert!(parse_guid("bad").is_none());
    }

    #[test]
    fn acknowledgement_requires_background_restored_and_recreation_consumed() {
        let expected = Expected::parse(&settings("1,1,0,0,1,1", &bindings(27, 3))).unwrap();
        let mut flags = [0u8; 15];
        for offset in [0, 3, 4, 5, 13] { flags[offset] = 1; }
        assert!(expected.matches(&flags, &[3;32], &[0;16]));
        flags[12] = 1;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]), "setter return is not a completed recreation");
        flags[12] = 0; flags[3] = 0;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]), "reload alone leaves background disabled");
        flags[3] = 1; flags[14] = 1;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]));
        flags[14] = 0;
        assert!(!expected.matches(&flags, &[4;32], &[0;16]));
    }

    #[test]
    fn unsafe_xinput_disable_and_an_open_stock_config_window_are_refused() {
        let direct = Expected::parse(&settings("1,1,0,0,1,0", &bindings(27, 3))).unwrap();
        let mut flags = [0u8;15]; flags[4] = 1;
        assert!(transition(&flags, &direct, 0).unwrap_err().contains("restarting"));
        flags[4] = 0; flags[14] = 1;
        assert!(transition(&flags, &direct, 0).unwrap_err().contains("configuration window"));
        flags[14] = 0;
        assert!(transition(&flags, &direct, 1).unwrap_err().contains("configuration window"),
            "the stock worker may still be starting before it suspends input");
        assert!(transition(&flags, &direct, 0).is_ok());
    }

    #[test]
    fn expired_or_retired_requests_cannot_apply_on_a_later_frame() {
        let config = settings("1,1,0,0,0,1", &bindings(27, 3));
        let request = |id| Request { id, settings: config.clone(), expected: Expected::parse(&config).unwrap(), expires: Instant::now(), deadline_ms: u64::MAX };
        let mut state = State { pending: Some(request(1)), waiting: Some(Waiting { request: request(2), context: Context { keyboard: 1, window: 2 } }), ..State::default() };
        let events = crate::events::capture(|| state.expire(Instant::now() + LIMIT, 0));
        assert!(state.pending.is_none() && state.waiting.is_none());
        assert_eq!(events.len(), 2);
        state.pending = Some(request(3));
        let events = crate::events::capture(|| state.cancel(EXITED));
        assert!(state.pending.is_none());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn sender_deadline_is_not_extended_when_a_queued_request_arrives_late() {
        let config = settings("1,1,0,0,0,1", &bindings(27, 3));
        let now = Instant::now();
        assert!(matches!(Request::new(1, config.clone(), 1000, now, 1000), Err(EXPIRED)));
        let request = Request::new(2, config, 1500, now, 1000).unwrap();
        assert!(request.expired(now, 1500), "elapsed IPC time is not a new four-second lease");
        assert_eq!(request.expires, now + Duration::from_millis(500));
        let layout = Layout { base: 0, window: 0 };
        let request = Request { deadline_ms: 0, ..request };
        assert_eq!(apply(layout, Context { keyboard: 0, window: 0 }, &request), Err(EXPIRED),
            "an expired request returns before any game read, registry mutation or native call");
    }

    #[test]
    #[ignore = "reads the two local unpacked FFXiMain images; no game launch"]
    fn controller_bodies_match_both_clients_rebases_and_reject_changed_code() {
        let Some(root) = std::env::var_os("CRYSTAL_FFXIMAIN_DUMPS").map(std::path::PathBuf::from) else { return; };
        for (name, expected) in [("6A995428", OLD_HASH), ("6AC37846", CURRENT_HASH)] {
            let bytes = std::fs::read(root.join(format!("FFXiMain-{name}.img"))).unwrap();
            for base in [0x10000000usize, 0x18000000] {
                let mut moved = bytes.clone();
                for &(rva, _, relocations) in CODE {
                    for &offset in relocations {
                        let at = rva + offset;
                        let value = u32::from_le_bytes(moved[at..at+4].try_into().unwrap()).wrapping_add((base - 0x10000000) as u32);
                        moved[at..at+4].copy_from_slice(&value.to_le_bytes());
                    }
                }
                let hash = |image: &[u8]| code_hash(base, |rva, out| {
                    let Some(source) = image.get(rva..rva+out.len()) else { return false; };
                    out.copy_from_slice(source); true
                });
                assert_eq!(hash(&moved).as_deref(), Some(expected), "{name} at {base:x}");
                moved[SUSPEND + 0x51] ^= 1;
                assert_ne!(hash(&moved).as_deref(), Some(expected), "changed native return ABI is refused");
            }
        }
    }

    #[test]
    #[ignore = "requires CRYSTAL_AUTO_LOGIN_DLL; loads DLL only, never starts FFXI"]
    fn installed_game_controller_layout_is_verified_without_starting_game() {
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
        let path = std::env::var("CRYSTAL_AUTO_LOGIN_DLL").unwrap();
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let base = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) } as usize;
        assert_ne!(base, 0);
        let resolution = crate::game_layout::resolve_module(base).unwrap();
        assert!(Layout::resolve(&resolution).is_ok());
        let first = crate::auto_login::input_hook(&resolution).unwrap();
        let second = crate::auto_login::input_hook(&resolution).unwrap();
        drop(first); drop(second);
    }
}
