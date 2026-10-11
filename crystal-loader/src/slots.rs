//! Legacy offline fixtures for polcore's common function table.
//!
//! Native retail sessions retain the installed core's original service entries.
//! The table has 1,560 cdecl function pointers. The legacy fixture path replaces
//! session, presence, friend-list, mail, FAQ and server-settings entries while
//! retaining the core's file, path, text and timer functions.
//!
//! Every slot is cdecl (`extern "C"` on i686): the caller pops the arguments,
//! so a function may declare fewer parameters than the caller pushes.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use zeroize::Zeroize;

use crate::proto::{self, PolRecords, PresenceResult, PresenceState, Secrets, CHAR_RECORD_LEN, HANDLE_RECORD_LEN};

pub const TABLE_LEN: usize = 1560;

// --- patching ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    BadTableLen(usize),
    SlotOutOfRange(usize),
    DuplicateSlot(usize),
    NullFunction(usize),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchError::BadTableLen(n) => write!(f, "function table has {n} slots, expected {TABLE_LEN}"),
            PatchError::SlotOutOfRange(s) => write!(f, "override for slot {s} is outside the table"),
            PatchError::DuplicateSlot(s) => write!(f, "slot {s} is overridden twice"),
            PatchError::NullFunction(s) => write!(f, "override for slot {s} is null"),
        }
    }
}

/// The original entries replaced by `patch`, for `restore` and for calling
/// polcore's own implementation afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patched {
    originals: Vec<(usize, usize)>,
}

impl Patched {
    pub fn len(&self) -> usize {
        self.originals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.originals.is_empty()
    }

    pub fn original(&self, slot: usize) -> Option<usize> {
        self.originals.iter().find(|(s, _)| *s == slot).map(|(_, f)| *f)
    }

    /// Slots whose original entry was null (unexpected in a real polcore).
    pub fn null_originals(&self) -> Vec<usize> {
        self.originals.iter().filter(|(_, f)| *f == 0).map(|(s, _)| *s).collect()
    }

    /// Puts the original entries back.
    pub fn restore(&self, table: &mut [usize]) {
        for &(slot, f) in &self.originals {
            table[slot] = f;
        }
    }
}

/// Validates every override first, then writes them all (all or nothing).
pub fn patch(table: &mut [usize], overrides: &[(usize, usize)]) -> Result<Patched, PatchError> {
    if table.len() != TABLE_LEN {
        return Err(PatchError::BadTableLen(table.len()));
    }
    let mut seen = vec![false; TABLE_LEN];
    for &(slot, f) in overrides {
        if slot >= TABLE_LEN {
            return Err(PatchError::SlotOutOfRange(slot));
        }
        if seen[slot] {
            return Err(PatchError::DuplicateSlot(slot));
        }
        if f == 0 {
            return Err(PatchError::NullFunction(slot));
        }
        seen[slot] = true;
    }
    let mut originals = Vec::with_capacity(overrides.len());
    for &(slot, f) in overrides {
        originals.push((slot, table[slot]));
        table[slot] = f;
    }
    Ok(Patched { originals })
}

fn fp(f: *const ()) -> usize {
    f as usize
}

/// The full override set. `selftest` adds 221/222 so the lobby host name
/// resolves to 127.0.0.1 and nothing can reach Square Enix.
pub fn override_set(selftest: bool) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = vec![
        // session / lobby bring-up
        (818, fp(s818_session_pump as *const ())),
        (195, fp(s195_status_begin as *const ())),
        (376, fp(s195_status_begin as *const ())), // same function as 195 on retail
        (196, fp(s196_status_poll as *const ())),
        (936, fp(s936_auth_code as *const ())),
        (1003, fp(s1003_session_value as *const ())),
        (838, fp(s838_signin_state as *const ())),
        (836, fp(s836_service_client as *const ())),
        // presence store read back by FFXiMain
        (189, fp(s189_presence_sub as *const ())),
        (190, fp(s190_presence_chan as *const ())),
        (204, fp(s204_presence_flag as *const ())),
        (206, fp(s206_presence_mode as *const ())),
        (207, fp(s207_presence_word as *const ())),
        // records
        (167, fp(s167_friend_a as *const ())),
        (169, fp(s169_friend_b as *const ())),
        (173, fp(s173_handle as *const ())),
        (177, fp(s177_character as *const ())),
        (283, fp(s283_account_id as *const ())),
        // friend/user lookups
        (208, fp(s208_lookup_begin as *const ())),
        (209, fp(s209_lookup_poll as *const ())),
        // mail
        (278, fp(s278_fetch_headers as *const ())),
        (282, fp(s282_unread as *const ())),
        (930, fp(s930_list_poll as *const ())),
        (923, fp(s923_close as *const ())),
        // server-side settings backup
        (463, fp(s_zero as *const ())),
        (464, fp(s464_backup_query as *const ())),
        (465, fp(s465_backup_poll as *const ())),
        (1535, fp(s_settings_fail as *const ())),
        (1536, fp(s_settings_fail as *const ())),
    ];
    // async request starts -> handle 0; polls -> done
    for s in [165, 175, 179, 181, 273, 275, 280, 301, 931] {
        v.push((s, fp(s_start as *const ())));
    }
    for s in [166, 176, 180, 182, 274, 276, 279, 281, 302] {
        v.push((s, fp(s_poll_done as *const ())));
    }
    // no-ops: dead or service-only paths, FAQ search off (667 -> 0)
    for s in [174, 183, 184, 185, 186, 187, 188, 197, 199, 277, 422].into_iter().chain(638..=669) {
        v.push((s, fp(s_zero as *const ())));
    }
    // mail file channels fail on purpose
    for s in [921, 922, 924, 925, 926, 927, 928, 929] {
        v.push((s, fp(s_file_fail as *const ())));
    }
    if selftest {
        v.push((221, fp(s221_selftest_dns_begin as *const ())));
        v.push((222, fp(s222_selftest_dns_poll as *const ())));
    }
    v
}

// --- state the slots read ---------------------------------------------------

static SECRETS: Mutex<Option<Box<Secrets>>> = Mutex::new(None);
static RECORDS: OnceLock<Option<PolRecords>> = OnceLock::new();
static LOOKUP: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static SEEN: [AtomicU32; TABLE_LEN.div_ceil(32)] = [const { AtomicU32::new(0) }; TABLE_LEN.div_ceil(32)];
static SELFTEST_LOGGED: AtomicBool = AtomicBool::new(false);
static SESSION_STATUS: AtomicI32 = AtomicI32::new(0);
static FRIENDS: Mutex<Option<proto::FriendRecords>> = Mutex::new(None);

pub fn session_status(code: i32) {
    if code < 0 { let _ = SESSION_STATUS.compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire); }
}

pub fn friends_result(records: Option<proto::FriendRecords>) {
    if let Some(records) = records {
        *FRIENDS.lock().unwrap_or_else(|e| e.into_inner()) = Some(records);
    }
}

/// Gives the slots their session. A mutex protects copies and refreshes; the
/// stable allocation stays locked in memory until `wipe_session`.
pub fn install_session(mut secrets: Box<Secrets>, records: Option<PolRecords>) {
    let p: *mut Secrets = &mut *secrets;
    // Keep it out of the page file where possible; failure is harmless.
    // SAFETY: p points to a live allocation of this size.
    unsafe { windows_sys::Win32::System::Memory::VirtualLock(p.cast(), std::mem::size_of::<Secrets>()) };
    let old = SECRETS.lock().unwrap_or_else(|e| e.into_inner()).replace(secrets);
    if let Some(mut old) = old {
        old.zeroize();
        // SAFETY: the replaced allocation is no longer accessible to slots.
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock((&mut *old as *mut Secrets).cast(), std::mem::size_of::<Secrets>()) };
    }
    if let Some(r) = &records {
        presence().state = r.presence;
    }
    let _ = RECORDS.set(records);
}

/// The ordered reply reader applies this before completing ChangeMyStatus,
/// so a subsequent lobby login reads the server's latest V/authCode.
pub fn update_credentials(credentials: proto::Credentials) {
    if let Some(secrets) = SECRETS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        secrets.v_key.copy_from_slice(&credentials.v_key);
        secrets.auth_code.copy_from_slice(&credentials.auth_code);
    }
}

/// Zeroizes and frees the session values (after the game has exited).
pub fn wipe_session() {
    *FRIENDS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let old = SECRETS.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut b) = old {
        b.zeroize();
        // SAFETY: same allocation as VirtualLock'd above.
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock((&mut *b as *mut Secrets).cast(), std::mem::size_of::<Secrets>()) };
        drop(b);
    }
}

fn records() -> Option<&'static PolRecords> {
    RECORDS.get().and_then(Option::as_ref)
}

/// Logs the first call of each overridden slot (slot number only).
fn first_call(slot: usize) {
    let bit = 1u32 << (slot % 32);
    if SEEN[slot / 32].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        log!("slot {slot} first called");
    }
}

// --- presence: slot 195 starts a request, the launcher sends it, 196 polls ---

pub const PRESENCE_TIMEOUT: Duration = Duration::from_secs(30);
const ERR_BAD_HANDLE: i32 = -7190; // 0xffffe3ea
const ERR_BAD_PRIMARY: i32 = -7171; // 0xffffe3fd
const ERR_BAD_VALUE: i32 = -7185; // 0xffffe3ef
const ERR_TYPE3_WITHOUT_HANDLE: i32 = -263;
const ERR_NO_CONTEXT: i32 = -256; // all four request contexts busy
const ERR_HANDLE_NOT_IN_USE: i32 = -5136;
const ERR_NOT_SIGNED_IN: i32 = -515;
const ERR_TIMEOUT: i32 = -260;

struct Request {
    handle: i32,
    next: PresenceState,
    sent: Instant,
    /// Launcher's answer: Ok(changed) or the error text.
    outcome: Option<Result<bool, String>>,
    /// 196 gave up on it (timeout); drop its answer when it arrives.
    abandoned: bool,
}

struct PresenceCtl {
    /// Committed state, read back by 189/190/204/206/207.
    state: PresenceState,
    /// In the order the `presence` lines were sent; answers arrive in the same order.
    requests: VecDeque<Request>,
    next_handle: i32,
}

static PRESENCE: Mutex<PresenceCtl> = Mutex::new(PresenceCtl { state: PresenceState::DEFAULT, requests: VecDeque::new(), next_handle: 0 });

fn presence() -> std::sync::MutexGuard<'static, PresenceCtl> {
    PRESENCE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Apply polcore `1001d7e0`'s argument checks and "-2 = keep" rules, returning
/// the state to commit if the request succeeds, or retail's negative code.
pub fn plan_presence(a: [i32; 6], current: &PresenceState) -> Result<PresenceState, i32> {
    let [handle, primary, context, selection_type, flag, class] = a;
    if handle >= 64 {
        return Err(ERR_BAD_HANDLE);
    }
    if primary >= 64 {
        return Err(ERR_BAD_PRIMARY);
    }
    let mut next = *current;
    for (value, slot) in [(primary, &mut next.primary), (handle, &mut next.handle)] {
        match value {
            -2 => {}
            v if v < -1 => return Err(ERR_BAD_HANDLE),
            v => *slot = v,
        }
    }
    match context {
        -2 => {}
        0 | 1 => next.context = context,
        _ => return Err(ERR_BAD_VALUE),
    }
    match selection_type {
        -2 => {}
        0..=4 => next.selection_type = selection_type,
        _ => return Err(ERR_BAD_VALUE),
    }
    if (0..0x400).contains(&class) {
        next.content_class = class;
    }
    if flag >= 0 {
        next.flag = flag;
    }
    if next.selection_type == 3 && next.handle < 0 {
        return Err(ERR_TYPE3_WITHOUT_HANDLE);
    }
    Ok(next)
}

/// Called by the reply reader for each `presence_result`, in order.
pub fn presence_result(result: PresenceResult) {
    let mut ctl = presence();
    let Some(pos) = ctl.requests.iter().position(|r| r.outcome.is_none()) else {
        log!("ignoring a presence_result with no request outstanding");
        return;
    };
    let outcome = match result {
        PresenceResult::Ok { changed } => Ok(changed),
        PresenceResult::Failed(e) => Err(e),
    };
    if ctl.requests[pos].abandoned {
        ctl.requests.remove(pos);
    } else {
        ctl.requests[pos].outcome = Some(outcome);
    }
}

fn start_presence(a: [i32; 6]) -> i32 {
    let mut ctl = presence();
    // Plan against the newest state, including requests still in flight.
    let base = ctl.requests.iter().rev().find(|r| !r.abandoned && !matches!(r.outcome, Some(Err(_)))).map(|r| r.next).unwrap_or(ctl.state);
    let next = match plan_presence(a, &base) {
        Ok(n) => n,
        Err(code) => return code,
    };
    if ctl.requests.iter().filter(|r| !r.abandoned).count() >= 4 {
        return ERR_NO_CONTEXT;
    }
    let handle = ctl.next_handle;
    log!("presence request {handle} begin (handle={}, primary={}, context={}, type={}, flag={}, class={})", a[0], a[1], a[2], a[3], a[4], a[5]);
    ctl.next_handle = (ctl.next_handle + 1) & 3;
    ctl.requests.push_back(Request { handle, next, sent: Instant::now(), outcome: None, abandoned: false });
    // Queued under the lock so lines go out in request order.
    crate::events::send(proto::presence_line(&proto::presence_payload(a)));
    handle
}

fn poll_presence(handle: i32, closed: bool) -> i32 {
    let mut ctl = presence();
    let Some(pos) = ctl.requests.iter().position(|r| r.handle == handle && !r.abandoned) else {
        return ERR_HANDLE_NOT_IN_USE;
    };
    match ctl.requests[pos].outcome.take() {
        Some(Ok(changed)) => {
            let r = ctl.requests.remove(pos).expect("index from position");
            ctl.state = r.next;
            log!("presence request {handle} done (changed={changed})");
            1
        }
        Some(Err(e)) => {
            ctl.requests.remove(pos);
            log!("presence request {handle} failed in the launcher: {e}");
            ERR_NOT_SIGNED_IN
        }
        None if closed => {
            ctl.requests.remove(pos);
            ERR_NOT_SIGNED_IN
        }
        None if ctl.requests[pos].sent.elapsed() > PRESENCE_TIMEOUT => {
            ctl.requests[pos].abandoned = true;
            log!("presence request {handle}: no answer from the launcher in {}s", PRESENCE_TIMEOUT.as_secs());
            ERR_TIMEOUT
        }
        None => 0,
    }
}

/// slot 177's packed character id, as FFXiMain 0x100efe00 compares it.
pub fn pack_char_id(char_id: u32, world: u32) -> u32 {
    (char_id & 0xFFFF) | ((world & 0xFFFF) << 16) | (((char_id >> 16) & 0xFF) << 24)
}

/// A slot-177 record for one character, registered to handle 0.
pub fn build_char_record(content_id: u32, char_id: u32, world: u32) -> [u8; CHAR_RECORD_LEN] {
    let mut r = [0u8; CHAR_RECORD_LEN];
    r[0] = 1; // valid
    r[2..4].copy_from_slice(&1u16.to_le_bytes()); // FFXI character
    r[4..8].copy_from_slice(&pack_char_id(char_id, world).to_le_bytes());
    r[8..12].copy_from_slice(&content_id.to_le_bytes());
    r[0x10..0x18].copy_from_slice(&0x8000u64.to_le_bytes()); // registered, handle index 0
    r
}

/// The default handle record 0 when the launcher sent no catalog.
pub fn default_handle_record(idx: u32) -> [u8; HANDLE_RECORD_LEN] {
    let mut r = [0u8; HANDLE_RECORD_LEN];
    if idx == 0 {
        r[0] = 1;
        r[8..12].copy_from_slice(b"FFXI");
    }
    r
}

// FFXiMain's character table used by the legacy slot-177 fixture.

/// The FFXiMain build the offsets below were read from (PE timestamp, SizeOfImage).
pub const FFXIMAIN_TIMESTAMP: u32 = 0x6A99_5428;
pub const FFXIMAIN_SIZE_OF_IMAGE: u32 = 0x00C1_5000;
const LOBBYCTL_PTR_RVA: usize = crate::lobby::LOBBY_PTR_RVA; // verified [0x104df770]
const CHAR_TABLE_OFFSET: usize = 0x13820;
const CHAR_ENTRY_SIZE: usize = 0x8C;
const MAX_LOBBY_CHARS: u32 = 16;

static LOBBYCTL_PTR: OnceLock<Option<usize>> = OnceLock::new();

fn read<T: Copy>(addr: usize) -> Option<T> {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut v = std::mem::MaybeUninit::<T>::uninit();
    let mut n = 0usize;
    // SAFETY: ReadProcessMemory on our own process fails cleanly on unmapped memory.
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), addr as *const c_void, v.as_mut_ptr().cast(), std::mem::size_of::<T>(), &mut n) };
    // SAFETY: fully written when ok and n == size_of::<T>().
    (ok != 0 && n == std::mem::size_of::<T>()).then(|| unsafe { v.assume_init() })
}

/// Address of FFXiMain's lobby-context pointer, only for the verified build.
fn lobbyctl_ptr() -> Option<usize> {
    *LOBBYCTL_PTR.get_or_init(|| {
        match crate::lobby::verified_image() {
            Ok(Some(base)) => Some(base + LOBBYCTL_PTR_RVA),
            Ok(None) => { log!("slot 177: FFXiMain.dll is not loaded"); None }
            Err(reason) => { log!("slot 177: fallback unavailable: {reason}"); None }
        }
    })
}

fn ffximain_char_record(idx: u32) -> Option<[u8; CHAR_RECORD_LEN]> {
    if idx >= MAX_LOBBY_CHARS {
        return None;
    }
    let ctl = read::<u32>(lobbyctl_ptr()?)? as usize;
    if ctl == 0 {
        return None;
    }
    let e = ctl + CHAR_TABLE_OFFSET + idx as usize * CHAR_ENTRY_SIZE;
    let entry = read::<[u8; 0x10]>(e)?;
    let content = u32::from_le_bytes(entry[4..8].try_into().ok()?);
    let char_id = u32::from(u16::from_le_bytes([entry[8], entry[9]])) | (u32::from(entry[0xF]) << 16);
    let world = u32::from(u16::from_le_bytes([entry[0xA], entry[0xB]]));
    (content != 0).then(|| build_char_record(content, char_id, world))
}

// --- the slot functions -----------------------------------------------------

/// Actual chat health, with POL's latched disconnect / heartbeat errors.
extern "C" fn s818_session_pump() -> u32 {
    first_call(818);
    if crate::events::pipe_closed() { session_status(-1025); }
    SESSION_STATUS.load(Ordering::Acquire) as u32
}

/// 195 (and its alias 376): ChangeMyStatus / enter community (4,5). Validates
/// like polcore, then asks the launcher (which holds the chat session) to send
/// it; returns the request handle 0..3. Never touches the network itself.
extern "C" fn s195_status_begin(handle: i32, primary: i32, context: i32, selection_type: i32, flag: i32, class: i32) -> i32 {
    first_call(195);
    start_presence([handle, primary, context, selection_type, flag, class])
}

/// 196: poll 195's request: 0 while the launcher works on it, 1 when it
/// answered ok (the new presence state is committed), <0 on failure.
extern "C" fn s196_status_poll(handle: i32) -> i32 {
    first_call(196);
    poll_presence(handle, crate::events::pipe_closed())
}

/// 936: copy the 0x34-byte authCode.
extern "C" fn s936_auth_code(out: *mut u8) -> u32 {
    first_call(936);
    let secrets = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(secrets) = secrets.as_ref().filter(|_| !out.is_null()) else {
        return 0;
    };
    // SAFETY: FFXiMain passes a 0x40-byte buffer; the guard keeps secrets live.
    unsafe { std::ptr::copy_nonoverlapping(secrets.auth_code.as_ptr(), out, proto::AUTH_CODE_LEN) };
    0
}

/// 1003: copy the 16-byte session value V; returns 16.
extern "C" fn s1003_session_value(out: *mut u8) -> u32 {
    first_call(1003);
    let secrets = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(secrets) = secrets.as_ref().filter(|_| !out.is_null()) else {
        return 16;
    };
    // SAFETY: FFXiMain passes a 16-byte buffer; the guard keeps secrets live.
    unsafe { std::ptr::copy_nonoverlapping(secrets.v_key.as_ptr(), out, proto::V_KEY_LEN) };
    16
}

/// 838: sign-in state; 3 = signed in (0 raises error 0x7d1 at shutdown).
extern "C" fn s838_signin_state() -> u32 {
    first_call(838);
    // The authenticated game context lives until GameStart returns. Network
    // health is separate (818); losing chat must not manufacture an unsigned
    // state while the game is unwinding.
    if SECRETS.lock().unwrap_or_else(|e| e.into_inner()).is_some() { 3 } else { 0 }
}

/// This flag starts FFXI's entire mail/social job client, not just its friend
/// getter. Keep it gated until those remaining service callbacks are real.
/// The launcher Friends page and cached 167/169 getters work independently.
extern "C" fn s836_service_client() -> u32 {
    first_call(836);
    0
}

extern "C" fn s189_presence_sub() -> i32 {
    presence().state.primary
}
extern "C" fn s190_presence_chan() -> i32 {
    presence().state.handle
}
extern "C" fn s204_presence_flag() -> i32 {
    presence().state.context
}
extern "C" fn s206_presence_mode() -> i32 {
    presence().state.selection_type
}
extern "C" fn s207_presence_word() -> i32 {
    presence().state.content_class
}

/// 167: genuine friend list A, including POL's getter normalization.
extern "C" fn s167_friend_a(idx: u32, out: *mut u8) -> u32 {
    if idx >= 200 {
        return 0xFFFF_E3F9;
    }
    let record = FRIENDS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .and_then(|r| r.record(false, idx as usize));
    if let Some(record) = record {
        if !out.is_null() { unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), out, proto::FRIEND_RECORD_LEN); } }
        return 0;
    }
    if !out.is_null() {
        // SAFETY: FFXiMain passes a >= 0xb0-byte record buffer.
        unsafe {
            std::ptr::write_bytes(out, 0, 0xB0);
            let q = (u64::from(idx) & 0xFF) << 20;
            std::ptr::copy_nonoverlapping(q.to_le_bytes().as_ptr(), out.add(8), 8);
        }
    }
    0
}

/// 169: genuine friend list B (100 entries).
extern "C" fn s169_friend_b(idx: u32, out: *mut u8) -> u32 {
    if idx >= 100 {
        return 0xFFFF_E3F9;
    }
    let record = FRIENDS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .and_then(|r| r.record(true, idx as usize));
    if let Some(record) = record {
        if !out.is_null() { unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), out, proto::FRIEND_RECORD_LEN); } }
        return 0;
    }
    if !out.is_null() {
        // SAFETY: as 167.
        unsafe { std::ptr::write_bytes(out, 0, 0xB0) };
    }
    0
}

/// 173: handle record (0x28 bytes).
extern "C" fn s173_handle(idx: u32, out: *mut u8) -> u32 {
    first_call(173);
    if idx >= 64 {
        return 0xFFFF_E3EA;
    }
    let rec = match records() {
        Some(r) => r.handles.get(idx as usize).copied().unwrap_or([0; HANDLE_RECORD_LEN]),
        None => default_handle_record(idx),
    };
    if !out.is_null() {
        // SAFETY: FFXiMain passes a 0x28-byte buffer.
        unsafe { std::ptr::copy_nonoverlapping(rec.as_ptr(), out, HANDLE_RECORD_LEN) };
    }
    0
}

/// 177: POL character record (0x68 bytes); required for world entry.
extern "C" fn s177_character(idx: u32, out: *mut u8) -> u32 {
    first_call(177);
    if idx >= 64 {
        return 0xFFFF_E3FD;
    }
    let rec = match records() {
        Some(r) => r.chars.get(idx as usize).copied().unwrap_or([0; CHAR_RECORD_LEN]),
        None => ffximain_char_record(idx).unwrap_or([0; CHAR_RECORD_LEN]),
    };
    if !out.is_null() {
        // SAFETY: FFXiMain passes a 0x68-byte buffer.
        unsafe { std::ptr::copy_nonoverlapping(rec.as_ptr(), out, CHAR_RECORD_LEN) };
    }
    0
}

/// 283: the signed-in native credential ID (u64 in edx:eax).
extern "C" fn s283_account_id() -> u64 {
    SECRETS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |s| s.account_id)
}

extern "C" fn s208_lookup_begin(_word: u32, lo: u32, hi: u32) -> u32 {
    LOOKUP[0].store(lo, Ordering::Relaxed);
    LOOKUP[1].store(hi, Ordering::Relaxed);
    0
}

extern "C" fn s209_lookup_poll(_h: u32, out_id: *mut u32, out2: *mut u32) -> u32 {
    // SAFETY: FFXiMain's out pointers, checked for null.
    unsafe {
        if !out_id.is_null() {
            *out_id = LOOKUP[0].load(Ordering::Relaxed);
            *out_id.add(1) = LOOKUP[1].load(Ordering::Relaxed);
        }
        if !out2.is_null() {
            *out2 = 0;
        }
    }
    1
}

extern "C" fn s278_fetch_headers(out: *mut u8, count: u32) -> u32 {
    if !out.is_null() {
        // SAFETY: FFXiMain's header array of `count` 0x48-byte entries (bounded defensively).
        unsafe { std::ptr::write_bytes(out, 0, count.min(256) as usize * 0x48) };
    }
    0
}

extern "C" fn s282_unread(hdr: *const u8) -> u32 {
    if hdr.is_null() {
        return 0;
    }
    // SAFETY: a 0x48-byte mail header.
    let w = unsafe { std::ptr::read_unaligned(hdr.add(0x3E).cast::<u16>()) };
    u32::from(w >> 15)
}

extern "C" fn s930_list_poll(_ch: u32, count: *mut i32) -> u32 {
    if !count.is_null() {
        // SAFETY: FFXiMain's out pointer.
        unsafe { *count = 0 };
    }
    1
}

extern "C" fn s923_close() -> u32 {
    1
}

extern "C" fn s464_backup_query(out: *mut u32) -> u32 {
    if out.is_null() {
        return 0xFFFF_EC00;
    }
    // SAFETY: FFXiMain's out pointer.
    unsafe { *out = 0 };
    0
}

extern "C" fn s465_backup_poll() -> u32 {
    100
}

extern "C" fn s_settings_fail() -> u32 {
    0xFFFF_FF00
}

extern "C" fn s_file_fail() -> u32 {
    0xFFFF_D800
}

extern "C" fn s_start() -> u32 {
    0
}

extern "C" fn s_poll_done() -> u32 {
    1
}

extern "C" fn s_zero() -> u32 {
    0
}

/// Selftest only, 221: the lobby bring-up asks to resolve its host name.
/// Logged as evidence; nothing is looked up.
extern "C" fn s221_selftest_dns_begin(name: *const c_char) -> u32 {
    let host = if name.is_null() {
        String::from("(null)")
    } else {
        // SAFETY: FFXiMain passes a NUL-terminated host name.
        unsafe { CStr::from_ptr(name) }.to_string_lossy().chars().take(64).collect()
    };
    log!("selftest: lobby bring-up reached slot 221, resolve \"{host}\" -> answering 127.0.0.1 (no DNS, no traffic to Square Enix)");
    0
}

/// Selftest only, 222: resolve done, 127.0.0.1 in host byte order.
extern "C" fn s222_selftest_dns_poll(_h: u32, out: *mut u8) -> u32 {
    if !SELFTEST_LOGGED.swap(true, Ordering::Relaxed) {
        log!("selftest: slot 222 answered 127.0.0.1; FFXiMain now connects to the lobby on loopback");
    }
    if out.is_null() {
        return 0xFFFF_FFF6;
    }
    // SAFETY: FFXiMain passes a 0x14-byte buffer.
    unsafe {
        std::ptr::write_bytes(out, 0, 0x14);
        std::ptr::copy_nonoverlapping(1u16.to_le_bytes().as_ptr(), out, 2);
        std::ptr::copy_nonoverlapping(0x7F00_0001u32.to_le_bytes().as_ptr(), out.add(4), 4);
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_table() -> Vec<usize> {
        (0..TABLE_LEN).map(|i| 0x1000_0000 + i * 16).collect()
    }

    #[test]
    fn patch_replaces_only_listed_slots_and_restores() {
        let mut t = fake_table();
        let before = t.clone();
        let p = patch(&mut t, &[(195, 0xAAAA), (1003, 0xBBBB)]).unwrap();
        assert_eq!(t[195], 0xAAAA);
        assert_eq!(t[1003], 0xBBBB);
        assert_eq!(p.original(195), Some(before[195]));
        assert_eq!(p.len(), 2);
        let changed: Vec<usize> = (0..TABLE_LEN).filter(|&i| t[i] != before[i]).collect();
        assert_eq!(changed, vec![195, 1003]);
        p.restore(&mut t);
        assert_eq!(t, before);
    }

    #[test]
    fn patch_is_all_or_nothing() {
        let mut t = fake_table();
        let before = t.clone();
        assert_eq!(patch(&mut t, &[(1, 5), (TABLE_LEN, 6)]), Err(PatchError::SlotOutOfRange(TABLE_LEN)));
        assert_eq!(patch(&mut t, &[(1, 5), (1, 6)]), Err(PatchError::DuplicateSlot(1)));
        assert_eq!(patch(&mut t, &[(1, 5), (2, 0)]), Err(PatchError::NullFunction(2)));
        assert_eq!(t, before);
        let mut short = vec![1usize; 100];
        assert_eq!(patch(&mut short, &[(1, 5)]), Err(PatchError::BadTableLen(100)));
    }

    #[test]
    fn override_set_covers_the_documented_slots() {
        let set = override_set(false);
        let slots: Vec<usize> = set.iter().map(|(s, _)| *s).collect();
        for must in [195, 376, 196, 936, 1003, 838, 836, 818, 173, 177, 189, 190, 204, 206, 207, 667, 1535, 1536, 921, 930, 931] {
            assert!(slots.contains(&must), "slot {must} missing");
        }
        // real polcore keeps its file, path, DNS, clock, port and exit-code slots
        for keep in [125, 126, 127, 221, 222, 421, 689, 705, 969, 970, 1026, 1027, 1080, 1171] {
            assert!(!slots.contains(&keep), "slot {keep} must stay real");
        }
        // a real table patches cleanly with it
        let mut t = fake_table();
        let p = patch(&mut t, &set).unwrap();
        assert_eq!(p.len(), set.len());
        assert!(p.null_originals().is_empty());
        assert_eq!(t[195], t[376]);

        let st: Vec<usize> = override_set(true).iter().map(|(s, _)| *s).collect();
        assert!(st.contains(&221) && st.contains(&222));
        let mut t = fake_table();
        patch(&mut t, &override_set(true)).unwrap();
    }

    const AFTER_SIGNIN: PresenceState = PresenceState { handle: 0, primary: -1, context: 0, selection_type: 0, flag: 1, content_class: 1 };

    #[test]
    fn presence_args_follow_polcore_checks() {
        // lobby call and world entry
        assert_eq!(plan_presence([-2, -1, 0, -2, -1, 1], &AFTER_SIGNIN), Ok(AFTER_SIGNIN));
        assert_eq!(plan_presence([0, 2, 0, -2, -1, 1], &AFTER_SIGNIN).unwrap().primary, 2);
        for (a, code) in [
            ([64, -1, 0, -2, -1, 1], -7190),
            ([-3, -1, 0, -2, -1, 1], -7190),
            ([-2, 64, 0, -2, -1, 1], -7171),
            ([-2, -3, 0, -2, -1, 1], -7190),
            ([-2, -1, 2, -2, -1, 1], -7185),
            ([-2, -1, 0, 5, -1, 1], -7185),
            ([-2, -1, 0, -1, -1, 1], -7185),
            ([-1, -1, 0, 3, -1, 1], -263),
        ] {
            assert_eq!(plan_presence(a, &AFTER_SIGNIN), Err(code), "{a:?}");
        }
        // out-of-range class and negative flag keep instead of failing
        let n = plan_presence([-2, -2, -2, -2, -7, 0x400], &AFTER_SIGNIN).unwrap();
        assert_eq!(n, AFTER_SIGNIN);
        let n = plan_presence([-2, -2, 1, 3, 0, 5], &AFTER_SIGNIN).unwrap();
        assert_eq!((n.context, n.selection_type, n.flag, n.content_class), (1, 3, 0, 5));
    }

    #[test]
    fn presence_request_waits_for_the_launcher() {
        presence().state = AFTER_SIGNIN;
        // 195 starts a request; 196 waits until the launcher answers
        let h = start_presence([0, 2, 0, -2, -1, 1]);
        assert!((0..4).contains(&h));
        assert_eq!(poll_presence(h, false), 0);
        assert_eq!(s189_presence_sub(), -1, "not committed before the answer");
        presence_result(PresenceResult::Ok { changed: true });
        assert_eq!(poll_presence(h, false), 1);
        assert_eq!(s189_presence_sub(), 2);
        assert_eq!(s190_presence_chan(), 0);
        assert_eq!(poll_presence(h, false), -5136, "handle freed");

        // a failure leaves the state alone
        let h = start_presence([-2, 5, -2, -2, -1, -1]);
        presence_result(PresenceResult::Failed("chat session ended".into()));
        assert_eq!(poll_presence(h, false), -515);
        assert_eq!(s189_presence_sub(), 2);

        // bad arguments never reach the launcher
        assert_eq!(start_presence([70, 0, 0, 0, 0, 0]), -7190);

        // a closed pipe fails a pending request at once
        let h = start_presence([-2, 3, -2, -2, -1, -1]);
        assert_eq!(poll_presence(h, true), -515);

        // a timed-out request is abandoned and its late answer is discarded
        let h = start_presence([-2, 4, -2, -2, -1, -1]);
        presence().requests.back_mut().unwrap().sent -= PRESENCE_TIMEOUT + Duration::from_secs(1);
        assert_eq!(poll_presence(h, false), -260);
        let h2 = start_presence([-2, 6, -2, -2, -1, -1]);
        presence_result(PresenceResult::Ok { changed: true }); // the late answer for h
        assert_eq!(poll_presence(h2, false), 0);
        presence_result(PresenceResult::Ok { changed: true });
        assert_eq!(poll_presence(h2, false), 1);
        assert_eq!(s189_presence_sub(), 6);
        assert!(presence().requests.is_empty());
    }

    #[test]
    fn character_record_matches_ffximain_lookup() {
        let r = build_char_record(1234567, 0x0012_3456, 0x5A);
        assert_eq!(r[0] & 1, 1);
        assert_eq!(u16::from_le_bytes([r[2], r[3]]), 1);
        assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 0x3456 | (0x5A << 16) | (0x12 << 24));
        assert_eq!(u32::from_le_bytes(r[8..12].try_into().unwrap()), 1234567);
        assert_eq!(u32::from_le_bytes(r[12..16].try_into().unwrap()), 0);
        let q = u64::from_le_bytes(r[0x10..0x18].try_into().unwrap());
        assert!(q & 0x8000 != 0);
        assert_eq!((q >> 16) & 0x3F, 0);
        assert_eq!(default_handle_record(0)[0], 1);
        assert_eq!(default_handle_record(1), [0; HANDLE_RECORD_LEN]);
    }

    #[test]
    fn selftest_dns_answer_is_loopback() {
        let mut out = [0xEEu8; 0x14];
        assert_eq!(s222_selftest_dns_poll(0, out.as_mut_ptr()), 1);
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 1);
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 0x7F00_0001);
        // FFXiMain builds the address as buf[4]<<24|buf[5]<<16|buf[6]<<8|buf[7] then inet_ntoa
        assert_eq!(&out[4..8], &[1, 0, 0, 0x7F]);
    }

    #[test]
    fn session_slots_copy_installed_values() {
        install_session(Box::new(Secrets { v_key: [7; 16], auth_code: [9; 52], account_id: 0x9182_7364_5546_3728 }), None);
        let mut v = [0u8; 16];
        let mut a = [0u8; 0x40];
        assert_eq!(s1003_session_value(v.as_mut_ptr()), 16);
        assert_eq!(s936_auth_code(a.as_mut_ptr()), 0);
        assert_eq!(v, [7; 16]);
        assert_eq!(&a[..52], &[9; 52]);
        assert_eq!(&a[52..], &[0; 12]);
        assert_eq!(s283_account_id(), 0x9182_7364_5546_3728);
        assert_eq!(s838_signin_state(), 3);
        assert_eq!(s836_service_client(), 0);
        // A server update must be visible on the next lobby read, without
        // replacing the signed-in identity or changing buffer boundaries.
        update_credentials(proto::Credentials { v_key: [11; 16], auth_code: [13; 52] });
        s1003_session_value(v.as_mut_ptr());
        s936_auth_code(a.as_mut_ptr());
        assert_eq!(v, [11; 16]);
        assert_eq!(&a[..52], &[13; 52]);
        assert_eq!(&a[52..], &[0; 12]);
        assert_eq!(s283_account_id(), 0x9182_7364_5546_3728);
        wipe_session();
        assert_eq!(s838_signin_state(), 0);
        let mut v2 = [1u8; 16];
        s1003_session_value(v2.as_mut_ptr());
        assert_eq!(v2, [1; 16], "nothing is served after the wipe");
    }
}
