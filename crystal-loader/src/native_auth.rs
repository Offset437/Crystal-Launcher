//! Retail login and services owned by the installed, verified POL core, driven
//! the way the stock PlayOnline Viewer (app.dll on pol.exe's main thread)
//! drives it, so the servers see the Viewer's calls in the Viewer's order.
//! No cipher, endpoint, successful-result or credential-getter replacements.
//! Evidence: pol-spec/01-bootstrap-signin.md, 02-member-settings.md and
//! 03-lifecycle.md (app.dll/polcore addresses are cited inline).

use crate::{
    b64, events, log,
    native_jobs::{self, Policy, Request, FRAME},
    native_profile,
    proto::{self, NativeCredentials},
};
use std::{
    ffi::{c_void, CStr},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        mpsc, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

// Verified against polcore 73b1864b / timestamp 4e4e5917, preferred base
// 10000000 (pol-spec/evidence/verify-handoff/slotmap.txt).
const ENTRY_RVAS: &[(usize, usize)] = &[
    (125, 0x48a40),
    (126, 0x48be0),
    (367, 0x49d40),
    (807, 0x46410),
    (809, 0x44720),
    (814, 0x448a0),
    (815, 0x44a50),
    (816, 0x452f0),
    (817, 0x45350),
    (818, 0x45480),
    (819, 0x45930),
    (823, 0x459f0),
    (824, 0x45a20),
    (825, 0x45a50),
    (826, 0x45ad0),
    (828, 0x45b90),
    (835, 0x46190),
    (838, 0x46010),
    (845, 0x467c0),
    (846, 0x46a70),
    (849, 0x46cb0),
    (851, 0x46d90),
    (852, 0x46df0),
    (853, 0x46e50),
    (854, 0x46e70),
    (856, 0x46eb0),
    (858, 0x47340),
    (868, 0x47390),
    (870, 0x473c0),
    (871, 0x473e0),
    (872, 0x47550),
    (884, 0x475e0),
    (886, 0x46e30),
    (891, 0x47720),
    (892, 0x47390),
    (914, 0x30d70),
    (915, 0x30d90),
    (920, 0x2e910),
    (965, 0x181b0),
    (969, 0x4a440),
    (1032, 0x1b4f0),
    (1103, 0x45920),
    (1140, 0x464f0),
    (1149, 0x46720),
    (1175, 0x4a510),
    (1502, 0x1e4b0),
    (1309, 0x19880),
    (1527, 0x4db30),
    (1528, 0x181d0),
    (1072, 0x1ada0),
    (1074, 0x19560),
    (1075, 0x19550),
    (1302, 0x1b100),
    (163, 0x23440),
    (164, 0x255c0),
    (167, 0x23da0),
    (169, 0x23f90),
    (199, 0x25280),
    (195, 0x1db60),
    (196, 0x1df10),
    (206, 0x1cb20),
    (207, 0x1cb50),
    (171, 0x1e470),
    (172, 0x1e490),
    (175, 0x23440),
    (176, 0x23460),
    (334, 0x1b6f0),
    (374, 0x1d400),
    (375, 0x1d490),
    (376, 0x1db60),
    (377, 0x1db90),
    (383, 0x28330),
    (384, 0x28380),
    (385, 0x28040),
    (401, 0x2af70),
    (404, 0x29ff0),
    (405, 0x2a0e0),
    (434, 0x2a760),
    (463, 0x48020),
    (288, 0x1b500),
    (921, 0x2ec30),
    (922, 0x2e920),
    (923, 0x2eeb0),
    (928, 0x2ee60),
    (929, 0x2edc0),
    (932, 0x2e850),
    (933, 0x2e8b0),
];
// Commands are accepted only while the COM owner is idle. A click during
// GameStart must never become a queued second game or sign out beneath FFXI.
const GAME: u8 = 0;
const SOCIAL: u8 = 1;
const START_GAME: u8 = 2;
const SIGN_OUT: u8 = 3;
const VISIBLE: u8 = 4;
const AWAY: u8 = 5;
const INVISIBLE: u8 = 6;
const SOCIAL_REQUEST: u8 = 7;
/// Status command flag: the dialog's "apply to groups" (app 10031b2b).
const GROUPS: u8 = 0x10;
static CONTROL: AtomicU8 = AtomicU8::new(GAME);

/// FINAL FANTASY XI's content id (InstallFolder\0001), the app id the Viewer
/// stores with 1175 before handing off (app 101a0611, [esi+0x27c]).
const FFXI_APP: usize = 1;
/// The Viewer application id it publishes on return (app 101cf603).
const VIEWER_APP: i32 = 1000;
/// app 1001b13f: the auto-away check runs when the gated counter exceeds 3600.
const AUTO_AWAY_FRAMES: u32 = 3600;
/// 101983fb polls 817 at least 100 ms apart (696 clock) and never pumps.
const CLOSE_POLL: Duration = Duration::from_millis(100);
/// The Viewer polls without a limit; Crystal logs and stops after this.
const CLOSE_LIMIT: Duration = Duration::from_secs(10);
/// Sign-in has no Viewer timeout (the user cancels). Crystal's cancel uses the
/// Viewer's own abort (807) after this, or when the launcher goes away.
const SIGN_IN_LIMIT: Duration = Duration::from_secs(120);
const ABORT_LIMIT: Duration = Duration::from_secs(30);
const SLOTS_FULL: i32 = -256;
/// The four sign-in/return lists in the Viewer's start order (app 10040650,
/// 10040822): friends, handles, characters, groups.
const LISTS: [(&str, usize, usize); 4] =
    [("friends", 163, 164), ("handles", 171, 172), ("characters", 175, 176), ("groups", 383, 384)];

pub fn request_start_game() { request_control(START_GAME, "start_game"); }
static START_GATE: std::sync::Mutex<Option<crate::start_gate::Participant>> = std::sync::Mutex::new(None);
pub fn request_batch_game(token: &str) {
    let mut gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let code = match crate::start_gate::Participant::open(token) {
        Ok(participant) if gate.is_none() => {
            // Publish the gate before the owner can observe START_GAME.
            *gate = Some(participant);
            let code = accept_control(&CONTROL, START_GAME);
            if code != 0 { *gate = None; }
            code
        }
        _ => -256,
    };
    events::send(format!("{{\"type\":\"native_control_result\",\"action\":\"start_game\",\"code\":{code}}}\n"));
}
pub fn wait_batch_game() -> Result<(), String> {
    let gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(gate) = gate { gate.wait()?; }
    Ok(())
}
pub fn request_sign_out() { request_control(SIGN_OUT, "sign_out"); }
pub fn request_status(mode: i32, groups: bool) {
    let flag = if groups { GROUPS } else { 0 };
    match mode {
        0 => request_control(VISIBLE | flag, "visible"),
        1 => request_control(AWAY | flag, "away"),
        4 => request_control(INVISIBLE | flag, "invisible"),
        _ => {},
    }
}
fn request_control(command: u8, name: &str) {
    let code = accept_control(&CONTROL, command);
    events::send(format!("{{\"type\":\"native_control_result\",\"action\":\"{name}\",\"code\":{code}}}\n"));
}
fn accept_control(control: &AtomicU8, command: u8) -> i32 {
    if control.compare_exchange(SOCIAL, command, Ordering::AcqRel, Ordering::Acquire).is_ok() { 0 } else { -256 }
}

/// The Viewer has no manual friend refresh: Get Friend List (163) runs only at
/// sign-in and on return from a game (03 §6, 04 D14). The launcher's request is
/// answered and nothing is sent.
pub fn refuse_friends_refresh() {
    log!("native POL friend refresh refused (the Viewer has none)");
    events::send("{\"type\":\"native_friends_refresh_result\",\"code\":-256}\n".into());
}
/// Launcher-window input counts as Viewer input (owner decision 2): it resets
/// the idle clock the auto-away check reads (app [obj+0x2c0], 10060039).
pub fn note_activity() {
    LAST_INPUT_MS.store(clock_ms(), Ordering::Release);
}
static CLOCK: OnceLock<Instant> = OnceLock::new();
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
fn clock_ms() -> u64 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64
}
fn idle_time() -> Duration {
    Duration::from_millis(clock_ms().saturating_sub(LAST_INPUT_MS.load(Ordering::Acquire)))
}

pub fn request_social(request: proto::NativeSocialRequest) {
    let mut pending = crate::native_social::REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_some() || accept_control(&CONTROL, SOCIAL_REQUEST) != 0 {
        events::send(format!("{{\"type\":\"native_social_result\",\"id\":\"{}\",\"action\":\"{}\",\"code\":-256}}\n", request.id(), request.action()));
    } else { *pending = Some(request); }
}

pub fn validate_entries(table: &[usize], base: usize) -> Result<(), String> {
    for &(slot, rva) in ENTRY_RVAS.iter().chain(crate::native_social::ENTRY_RVAS).chain(crate::native_jobs::ENTRY_RVAS) {
        if table.get(slot).copied() != base.checked_add(rva) {
            return Err(format!(
                "PlayOnline service entry {slot} differs from the verified native core."
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Api(Box<[usize]>);
impl Api {
    #[cfg(test)]
    pub(crate) fn test_table(table: &[usize]) -> Self { Self(table.into()) }
    // All signatures below are verified cdecl wrappers from this exact image.
    pub(crate) unsafe fn f<F: Copy>(&self, slot: usize) -> F {
        std::mem::transmute_copy(&self.0[slot])
    }
    pub(crate) fn zero(&self, slot: usize) -> i32 {
        unsafe { self.f::<unsafe extern "C" fn() -> i32>(slot)() }
    }
    pub(crate) fn one(&self, slot: usize, argument: usize) -> i32 {
        unsafe { self.f::<unsafe extern "C" fn(usize) -> i32>(slot)(argument) }
    }
    pub(crate) fn two(&self, slot: usize, a: usize, b: usize) {
        // Credential/string setters have no return value.
        unsafe { self.f::<unsafe extern "C" fn(usize, usize)>(slot)(a, b) }
    }
    /// cdecl call with up to six word arguments (signed values as two's
    /// complement). The caller pops, so void entries just return garbage eax.
    pub(crate) fn call(&self, slot: usize, a: &[usize]) -> i32 {
        self.word(slot, a) as i32
    }
    /// Same, for entries returning a pointer.
    pub(crate) fn word(&self, slot: usize, a: &[usize]) -> usize {
        type W = usize;
        unsafe {
            match *a {
                [] => self.f::<unsafe extern "C" fn() -> W>(slot)(),
                [p] => self.f::<unsafe extern "C" fn(W) -> W>(slot)(p),
                [p, q] => self.f::<unsafe extern "C" fn(W, W) -> W>(slot)(p, q),
                [p, q, r] => self.f::<unsafe extern "C" fn(W, W, W) -> W>(slot)(p, q, r),
                [p, q, r, s] => self.f::<unsafe extern "C" fn(W, W, W, W) -> W>(slot)(p, q, r, s),
                [p, q, r, s, t] => self.f::<unsafe extern "C" fn(W, W, W, W, W) -> W>(slot)(p, q, r, s, t),
                [p, q, r, s, t, u] => self.f::<unsafe extern "C" fn(W, W, W, W, W, W) -> W>(slot)(p, q, r, s, t, u),
                _ => unreachable!("no common-table entry takes more than six arguments here"),
            }
        }
    }
    fn se(&self, credentials: Option<&NativeCredentials>) {
        let (mode, name, secret, otp) = credentials.map_or((0, 0, 0, 0), |c| {
            (
                if c.otp.is_some() { 2 } else { 1 },
                c.se_id.as_ptr() as usize,
                c.se_secret.as_ptr() as usize,
                c.otp.as_ref().map_or(0, |o| o.as_ptr() as usize),
            )
        });
        unsafe {
            self.f::<unsafe extern "C" fn(i32, usize, usize, usize)>(1502)(mode, name, secret, otp)
        }
    }
    /// ChangeMyStatus (376 = the same code as 195) with the Viewer's tuple
    /// (world/handle, char, flag, mode, field5, app); -2 keeps a field.
    fn begin_status(&self, tuple: [i32; 6]) -> i32 {
        self.call(376, &tuple.map(|v| v as usize))
    }
    fn restore_viewer_policy(&self) -> Result<(), String> {
        // Stock POL keeps this scalar at zero through game backup. Windower's
        // supported Hook.dll calls common entry 1075(1) after loading the core.
        // Besides viewer limits, that makes native delete reject a missing old
        // backup (-5326), preventing the first upload. Use the original setter
        // to restore stock behavior; native transfer and errors remain intact.
        let before = self.zero(1074);
        if before != 0 {
            unsafe { self.f::<unsafe extern "C" fn(i32)>(1075)(0) };
        }
        if self.zero(1074) != 0 {
            return Err("The native PlayOnline viewer policy could not be restored.".into());
        }
        log!("native POL viewer policy {before}->0");
        Ok(())
    }
}

/// Runs a status-job request (376/377) under the Viewer's Normal policy.
fn status_request(api: &Api, tuple: [i32; 6], frame: &mut dyn FnMut() -> bool) -> i32 {
    native_jobs::run(api, Request { start: &mut || api.begin_status(tuple), poll: 377, policy: Policy::Normal }, frame)
}

// --- Viewer host callbacks (pol-spec 01 §2.1 #13/#15/#17) -------------------

extern "C" {
    // The CRT heap: app.dll registers its CRT malloc/free/realloc (1019b8fe).
    // The core frees profile memory through these while it lives.
    fn malloc(size: usize) -> *mut c_void;
    fn free(block: *mut c_void);
    fn realloc(block: *mut c_void, size: usize) -> *mut c_void;
}
/// 871 entries 0 and 2 (app 100a60cd, 100a606c): queue member-session text
/// into the Viewer's chat window ring ([105050a4]); without a chat window they
/// do nothing. The core ignores their result (10017ef0).
unsafe extern "C" fn no_chat_window(_: usize, _: usize, _: usize, _: usize, _: usize) {}
/// 871 entry 5, the 288 receiver: app 10008ec7 (`ret`) before sign-in and
/// 1005657d (local UI redraws only) after it.
unsafe extern "C" fn no_service_view(_: usize, _: usize) {}
/// 1149 (app 1019b63c, 1019acf0): inactivity warning/expiry dialogs. The
/// inactivity timer is never armed here (the Viewer's 1001cfb7(0) default;
/// Crystal never calls 1140(1)), so neither can fire.
unsafe extern "C" fn no_inactivity_warning() {}
unsafe extern "C" fn no_inactivity_expiry() {}

/// The Viewer's auto-Away (app 1001b0f3 / 1001b16c), counted per idle owner
/// frame while signed in, between games and Visible.
pub(crate) struct AutoAway {
    minutes: u32,
    frames: u32,
}
impl AutoAway {
    pub(crate) fn new(minutes: u32) -> Self { Self { minutes, frames: 0 } }
    /// One gated frame; true when the Viewer would set Away now. The counter
    /// advances only on gated frames and resets after every check.
    pub(crate) fn frame(&mut self, visible: bool, idle: Duration) -> bool {
        if self.minutes == 0 || !visible {
            return false;
        }
        self.frames += 1;
        if self.frames <= AUTO_AWAY_FRAMES {
            return false;
        }
        self.frames = 0;
        idle.as_millis() > u128::from(self.minutes) * 60_000
    }
}

/// Lives inside the same scope as the COM core: the Viewer's host state and
/// its signed-in member session. Drop before releasing the core; it runs the
/// Viewer's sign-out and close teardown (03 §5).
pub struct NativeSession {
    api: Api,
    viewer: PathBuf,
    area: i32,
    /// The game language 421 falls back to (app 102787d6 → 1528).
    language: u32,
    auto_away: AutoAway,
    social: crate::native_social::Service,
    /// Profile list header returned by 825.
    profiles: usize,
    /// 932/914 started the file worker and option.bin mutex.
    files: bool,
    /// A member home is selected (834() >= 0).
    home: bool,
    /// 819/824/profile list/1149 are registered (app 1019b874).
    registered: bool,
    /// Member session reached 30 with 838 == 3 and has not been lost.
    signed_in: bool,
    /// GameStart returned; the Viewer's restart is still due.
    returned: bool,
}

impl NativeSession {
    fn new(api: Api, viewer: &Path, area: i32, auto_away_minutes: u32) -> Self {
        Self {
            api,
            viewer: viewer.to_path_buf(),
            area,
            language: u32::from(area != 0),
            auto_away: AutoAway::new(auto_away_minutes),
            social: Default::default(),
            profiles: 0,
            files: false,
            home: false,
            registered: false,
            signed_in: false,
            returned: false,
        }
    }

    /// pol.exe → app.dll startup on a fresh core, before any 815/814 (A1/A2/A8).
    /// The caller keeps the verified core alive through this object's lifetime;
    /// `viewer_c` and `regkey` must outlive it (the core keeps the pointers).
    pub unsafe fn bootstrap(
        table: &[usize],
        base: usize,
        viewer: &Path,
        viewer_c: &'static CStr,
        regkey: &'static CStr,
        area: i32,
        language: u32,
        auto_away_minutes: u32,
    ) -> Result<Self, String> {
        validate_entries(table, base)?;
        let mut session = Self::new(Api(table.into()), viewer, area, auto_away_minutes);
        session.language = language;
        session.api.restore_viewer_policy()?;
        session.bootstrap_core(viewer_c, regkey)?;
        // 10050fa2 → 10057896: notification titles and texts.
        install_message_texts(&session.api, viewer)?;
        log!("native POL host registered like the Viewer (824/871/1149, profile list loaded)");
        Ok(session)
    }

    fn bootstrap_core(&mut self, viewer_c: &CStr, regkey: &CStr) -> Result<(), String> {
        let api = self.api.clone();
        self.host_paths(viewer_c, regkey);
        api.call(870, &[usize::MAX]); // 10199783: no member
        api.zero(932); // 100192d6 (919 = 932): file worker
        api.zero(914); // 100192e1: option.bin manager mutex
        self.files = true;
        self.region_settings();
        self.register_callbacks(); // 1019b8b9, fresh Viewer only
        self.register_host() // 1019b8e2..1019b929
    }

    /// The game language first, then the Viewer root and registry key.
    /// app 102787d6: 1528(102afa80(region)) — JP 0, US 1, EU the registry
    /// language. FFXi.dll/FFXiMain read it through 421, which caches its first
    /// answer; 125's path code (core 10048e1f/10048edc/100491be) already asks
    /// 421, so 1528 must come before 125 or FFXI starts in Japanese.
    fn host_paths(&self, viewer_c: &CStr, regkey: &CStr) {
        self.api.call(1528, &[self.language as usize]);
        self.api.call(125, &[viewer_c.as_ptr() as usize]); // 1001a6ea (1456 → 125)
        self.api.call(367, &[regkey.as_ptr() as usize]); // 100192bb
    }

    /// app 1001a175: 965(region != 0), 1527(region == 0), 1309(POL_UCS_AREA_KBN
    /// "01" from data\utf\env.dat, US). 420 is never called by the US Viewer.
    fn region_settings(&self) {
        self.api.call(965, &[usize::from(self.area != 0)]);
        self.api.call(1527, &[usize::from(self.area == 0)]);
        self.api.call(1309, &[b"01\0".as_ptr() as usize]);
    }

    /// 871 as app 1019b8b9 / 10050fd9: entry 1 is the core's own dispatcher
    /// (table[334]) — the only route for server pushes on the member session —
    /// and entry 3 is the core's 401, which the Viewer's 100a608e calls first.
    /// Entry 4 is the Viewer's mail receiver (app 10056b05); it accepts pushes
    /// from here until the hand-off to a game, since 1032 gates its calls.
    fn register_callbacks(&self) {
        let set = [
            no_chat_window as *const () as usize,
            self.api.0[334],
            no_chat_window as *const () as usize,
            self.api.0[401],
            crate::native_social::mail_arrived as *const () as usize,
            no_service_view as *const () as usize,
        ];
        self.api.call(871, &[set.as_ptr() as usize]);
        crate::native_social::set_listening(true);
    }

    /// app 1019b874 (after 871): 819(0), 824(CRT heap), the profile list from
    /// usr\all\inet_w.bin (101a0f48), 1149(callbacks). Redone on return.
    fn register_host(&mut self) -> Result<(), String> {
        self.api.call(819, &[0]);
        self.api.call(824, &[malloc as *const () as usize, free as *const () as usize, realloc as *const () as usize]);
        self.registered = true;
        let file = native_profile::read_file(&self.viewer);
        self.profiles = native_profile::load(&self.api, file.as_deref())?.0;
        self.api.call(1149, &[no_inactivity_warning as *const () as usize, no_inactivity_expiry as *const () as usize]);
        Ok(())
    }

    /// app 1019869a, preceded by 828 (101a09db): free the profile list, then
    /// unregister the heap, error callback, connection callbacks and 1149.
    fn unregister_host(&mut self) {
        if !self.registered {
            return;
        }
        self.api.zero(828);
        self.api.call(824, &[0, 0, 0]);
        self.api.call(819, &[0]);
        self.api.zero(872); // 871 set → core defaults (331/288 ← 0)
        self.api.call(1149, &[0, 0]);
        self.registered = false;
        self.profiles = 0;
    }

    /// Sign in the member like a Viewer user: select the member, choose the
    /// LAN connection, open the network, then the member session (A3/A4/A5).
    pub fn sign_in(&mut self, credentials: NativeCredentials) -> Result<(), String> {
        // Local storage must work before spending a login/OTP.
        let home = claim_home(&self.viewer, &credentials.profile_key)?;
        note_activity();
        let result = self.sign_in_core(home, &credentials);
        drop(credentials);
        result
    }

    fn sign_in_core(&mut self, home: u32, c: &NativeCredentials) -> Result<(), String> {
        self.select_member(home, c);
        let name = native_profile::selected_name(&self.api, self.profiles)?;
        native_profile::connect(&self.api, &name)?; // connect screen 10199d02
        // The cold stock probe (2026-10-06, [+0x99414]) shows Open(0) to the
        // network level (838 == 2), then Open(1) for member login. Mode 1 skips
        // the core's own state-40 status request that Open(3) would add; the
        // live captures contain none. 1502 is set before member login.
        self.open(0, 2)?;
        self.api.se(Some(c));
        self.open(1, 3)?;
        self.signed_in = true;
        log!("native POL member session signed in");
        // The live stock probe shows delivery switched on right after sign-in,
        // before the status load; then 101aa269 → 10050fd9: 871 again, the
        // status load alone, and its success handler (10040650) queues the
        // four lists. No status request of our own at sign-in (A6).
        self.api.call(1032, &[1]);
        self.register_callbacks();
        let api = self.api.clone();
        let status_started = Instant::now();
        let status = {
            let mut cadence = StartupCadence::new("status");
            native_jobs::run(&api, Request { start: &mut || api.zero(374), poll: 375, policy: Policy::Login }, &mut || cadence.frame(&api))
        };
        log!("native POL status load result {status} in {:.2}s", status_started.elapsed().as_secs_f64());
        if !(status > 0 || status == -261) {
            return Err(format!("Native PlayOnline status loading failed ({status})."));
        }
        let results = {
            let mut cadence = StartupCadence::new("lists");
            cadence.frame(&api); // the queued lists start on the next pump tick
            run_concurrent(&api, &LISTS, &mut || cadence.frame(&api))
        };
        if let Some((name, result)) = LISTS.iter().zip(&results).map(|((n, ..), r)| (n, *r)).find(|(_, r)| !(*r > 0 || *r == -261)) {
            return Err(format!("Native PlayOnline {name} loading failed ({result})."));
        }
        events::send("{\"type\":\"authenticated\"}\n".into());
        log!("native POL authenticated; all game service callbacks are original");
        // Main menu entry: the mail list, then the Viewer presence about a
        // second later (live capture: 496-byte list, then the 120-byte status).
        let mail_started = Instant::now();
        // The list's timeouts use wall time, so it can advance at startup
        // cadence. Downloads and acknowledgements still count Viewer frames.
        self.social.social_entry_with_list_frame(&api,
            &mut || owner_frame_at(&api, STARTUP_FRAME),
            &mut || owner_frame(&api));
        log!("native POL startup mail completed in {:.2}s", mail_started.elapsed().as_secs_f64());
        let presence_started = Instant::now();
        {
            let mut cadence = StartupCadence::new("presence");
            self.publish_viewer(&mut || cadence.frame(&api));
        }
        log!("native POL startup presence completed in {:.2}s", presence_started.elapsed().as_secs_f64());
        Ok(())
    }

    /// Member select (app 101a425f): 870 → 851 → 852 → 886(encoded "") → 853 → 891.
    /// That 891 fails like stock (-11267): Viewer-made members have no profile name.
    fn select_member(&mut self, home: u32, c: &NativeCredentials) {
        let api = &self.api;
        api.call(870, &[home as usize]);
        self.home = true;
        let mut id = Zeroizing::new([0u8; 8]);
        api.two(854, id.as_mut_ptr() as usize, c.pol_id.as_ptr() as usize);
        api.one(851, id.as_ptr() as usize);
        let mut password = Zeroizing::new([0u8; 17]);
        api.two(856, password.as_mut_ptr() as usize, c.pol_password.as_ptr() as usize);
        api.two(852, password.as_ptr() as usize, 0);
        let mut none = [0u8; native_profile::NAME_LEN];
        api.call(858, &[none.as_mut_ptr() as usize, b"\0".as_ptr() as usize]);
        api.call(886, &[none.as_ptr() as usize]);
        let mut host = [0u8; 0x42];
        api.two(868, host.as_mut_ptr() as usize, b"ci000.pol.com\0".as_ptr() as usize);
        api.one(853, host.as_ptr() as usize);
        let code = api.zero(891);
        if code != native_profile::PROFILE_NOT_FOUND {
            log!("note: member-select profile load returned {code} (stock: -11267)");
        }
    }

    /// Sign-in object (app 1019cb52): Open(mode, 835()), then one 818 and one
    /// 815 per frame (102797f0, 1019ce64) until state 30 with 838 == `linked`
    /// (2 network, 3 member). 31..38 are transitional; failure is a negative
    /// state, reported with 823 (D18).
    fn open(&mut self, mode: usize, linked: i32) -> Result<(), String> {
        let mut cadence = StartupCadence::new(if mode == 0 { "open-network" } else { "open-member" });
        let api = &self.api;
        let connection = api.word(835, &[]);
        #[cfg(feature = "native-login-diagnostics")]
        let open_started = crate::startup_timing::Stamp::capture();
        let code = api.call(814, &[mode, connection]);
        #[cfg(feature = "native-login-diagnostics")]
        let open_finished = crate::startup_timing::Stamp::capture();
        if code < 0 {
            #[cfg(feature = "native-login-diagnostics")]
            crate::startup_timing::report(if mode == 0 { "open-network-call-failed" } else { "open-member-call-failed" }, open_started, open_finished);
            return Err(format!("Native PlayOnline login could not start (mode {mode}, POL result {code})."));
        }
        let started = Instant::now();
        let mut aborted: Option<Instant> = None;
        let mut previous = i32::MIN;
        #[cfg(feature = "native-login-diagnostics")]
        let (mut first_poll, mut state17_seen) = (true, false);
        loop {
            cadence.tick();
            pump_idle_messages();
            api.zero(818);
            let state = api.zero(815);
            #[cfg(feature = "native-login-diagnostics")]
            let polled = (first_poll || state == 17 && !state17_seen).then(crate::startup_timing::Stamp::capture);
            if state != previous {
                log!("native POL sign-in state {state} (mode {mode}, +{:.2}s)", started.elapsed().as_secs_f64());
                events::send(format!("{{\"type\":\"native_auth_stage\",\"code\":{state}}}\n"));
                previous = state;
            }
            #[cfg(feature = "native-login-diagnostics")]
            if let Some(polled) = polled {
                // Capture before logging; no extra writes between Open and its first poll.
                if first_poll {
                    crate::startup_timing::report(if mode == 0 { "open-network-call" } else { "open-member-call" }, open_started, open_finished);
                    crate::startup_timing::report(if mode == 0 { "open-network-first-poll" } else { "open-member-first-poll" }, open_finished, polled);
                    first_poll = false;
                }
                if state == 17 && !state17_seen {
                    crate::startup_timing::report(if mode == 0 { "open-network-first-state17" } else { "open-member-first-state17" }, open_started, polled);
                    state17_seen = true;
                }
            }
            match connection_result(state, || api.zero(838), linked) {
                Some(Ok(())) => return Ok(()),
                Some(Err(())) => {
                    let error = api.zero(823); // non-consuming native error getter
                    return Err(format!("Native PlayOnline login failed (state {state}, POL result {error})."));
                }
                None => {}
            }
            match aborted {
                // The Viewer's cancel button: 807 makes the core fail the login.
                None if events::pipe_closed() || started.elapsed() >= SIGN_IN_LIMIT => {
                    log!("native POL sign-in cancelled at state {state}");
                    api.zero(807);
                    aborted = Some(Instant::now());
                }
                Some(at) if at.elapsed() >= ABORT_LIMIT => {
                    return Err(format!("Native PlayOnline login timed out at state {state}."));
                }
                _ => {}
            }
            pause_owner(STARTUP_FRAME);
        }
    }

    fn social_entry(&mut self) {
        let api = self.api.clone();
        self.social.social_entry(&self.api, &mut || owner_frame(&api));
    }

    /// Before GameStart, mirror the Viewer's launch and exit (B3, 03 §3.1).
    /// Requests run synchronously on this thread, so none is pending here.
    pub fn attach_game(&mut self) {
        CONTROL.store(GAME, Ordering::Release);
        let api = self.api.clone();
        // Answer messages pushed during the last idle frames before the game
        // owns the core; later pushes go to FFXI's 288 receiver.
        crate::native_social::set_listening(false);
        if self.signed_in {
            self.social.idle_frame(&api, &mut || owner_frame(&api));
        }
        // Play (app 1019fe84 / 1019fec7): clear the arrival flag, hold delivery
        // off only around the version check, then back on. The live probe
        // shows 1032 at 1 throughout the game, so FFXI's 288 receiver gets
        // pushes during play; the launcher runs that check before this call.
        api.call(1305, &[0]);
        api.call(1032, &[1]);
        // Launch state 8 (101a05ea): disarm inactivity, launched app, exit code.
        api.call(1140, &[0]);
        api.call(1175, &[FFXI_APP]);
        api.call(969, &[0]);
        // Viewer exit: 101aa2ac (463), then 1019869a (828/824/819/872/1149).
        api.zero(463);
        self.unregister_host();
        events::send("{\"type\":\"native_session_state\",\"game\":true}\n".into());
    }

    /// GameStart has returned and all game callbacks/observers have joined.
    /// FFXI's 288 receiver lives in FFXiMain; clear it before anything pumps
    /// (LOCAL; the restarted Viewer's 871 replaces it anyway).
    pub fn detach_game(&mut self) {
        self.api.call(288, &[0]);
        self.returned = true;
    }

    /// pol.exe's next Viewer instance reusing the signed-in core (B4, 03 §3.4):
    /// startup registrations, 1032(1), post-login 871, the four lists without
    /// 374, the app-1000 publish, then the social entry.
    fn return_from_game(&mut self) -> Result<(), String> {
        note_activity(); // 101abaf8 / 1019fde5 refresh the last-input stamp
        self.region_settings(); // 1001a175
        if self.api.zero(838) != 3 {
            self.signed_in = false;
            events::send("{\"type\":\"native_pol_status\",\"code\":-1025}\n".into());
            return Err("PlayOnline is no longer signed in after the game. Sign in again to reconnect.".into());
        }
        self.register_host()?; // 1019b874 (871 is skipped at startup)
        self.api.call(1032, &[1]); // 101aaebe
        self.register_callbacks(); // 101aa269 → 10050fd9
        let api = self.api.clone();
        let mut frame = || owner_frame(&api);
        let results = run_concurrent(&api, &LISTS, &mut frame);
        for ((name, ..), result) in LISTS.iter().zip(&results) {
            log!("native POL {name} reload result {result}");
        }
        // As at sign-in: the mail list first, then the Viewer presence.
        self.social_entry();
        self.publish_viewer(&mut frame);
        Ok(())
    }

    /// app 101cf603 → 10041342(1000) with no Viewer-own handle (10019224):
    /// a selected character is cleared with the publish; otherwise publish
    /// only if the application is not already the Viewer.
    fn publish_viewer(&self, frame: &mut dyn FnMut() -> bool) {
        let api = &self.api;
        let tuple = if api.zero(190) >= 0 {
            [-2, -1, 1, -2, 0, VIEWER_APP]
        } else if api.zero(207) as u16 as i32 != VIEWER_APP {
            [-2, -2, 1, -2, 0, VIEWER_APP]
        } else {
            log!("native POL Viewer presence already published");
            return;
        };
        let code = status_request(api, tuple, frame);
        log!("native POL Viewer presence result {code}");
    }

    /// The idle Viewer between games (03 §1): commands, auto-Away, the social
    /// idle work and the per-frame 1103 loss check, one 818 per frame.
    pub fn wait_for_game(&mut self) -> Result<bool, String> {
        // The launcher is gone: sign out without the restarted Viewer's requests.
        if events::pipe_closed() {
            return Ok(false);
        }
        if std::mem::take(&mut self.returned) {
            self.return_from_game()?;
        }
        CONTROL.store(SOCIAL, Ordering::Release);
        events::send("{\"type\":\"native_session_state\",\"game\":false}\n".into());
        events::send("{\"type\":\"native_social_ready\"}\n".into());
        let observer = self.observe_friends()?;
        let api = self.api.clone();
        let mut frame = || owner_frame(&api);
        let result = loop {
            if crate::native_social::take_mail_changed() {
                events::send("{\"type\":\"native_mail_changed\"}\n".into());
            }
            let control = CONTROL.load(Ordering::Acquire);
            match control & !GROUPS {
                SOCIAL_REQUEST => {
                    let request = crate::native_social::REQUEST.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(request) = request { self.social.execute_with(&self.api, request, &mut frame); }
                    CONTROL.store(SOCIAL, Ordering::Release);
                }
                START_GAME => break Ok(true),
                SIGN_OUT => break Ok(false),
                action @ (VISIBLE | AWAY | INVISIBLE) => {
                    let mode = match action { VISIBLE => 0, AWAY => 1, _ => 4 };
                    self.user_status(mode, control & GROUPS != 0, &mut frame);
                    CONTROL.store(SOCIAL, Ordering::Release);
                }
                _ => {},
            }
            if events::pipe_closed() { break Ok(false); }
            let error = self.api.zero(1103);
            if error < 0 {
                break Err(self.connection_lost(error));
            }
            self.auto_away_tick(idle_time(), &mut frame);
            self.social.idle_frame(&self.api, &mut frame);
            frame();
        };
        CONTROL.store(GAME, Ordering::Release);
        drop(observer);
        result
    }

    /// One idle frame of the Viewer's OnUpdate auto-Away (app 1001b0f3): gated
    /// on Visible (206() == 0); when due and idle long enough, 10040d4f(1)
    /// sends (-2,-2,1,1,0,-2). No group push and no automatic return (03 §1.3).
    fn auto_away_tick(&mut self, idle: Duration, frame: &mut dyn FnMut() -> bool) -> Option<i32> {
        if !self.auto_away.frame(self.api.zero(206) == 0, idle) {
            return None;
        }
        let code = status_request(&self.api, [-2, -2, 1, 1, 0, -2], frame);
        log!("native POL auto-away after {}s idle: result {code}", idle.as_secs());
        Some(code)
    }

    /// Status dialog apply (app 10031b2b → 10040d4f, 10040f39): ChangeMyStatus,
    /// then, when asked to apply to groups and a group is active, ChgAllGrpStatus
    /// (404 maps 0/1/4 to 1/2/5 itself, core 10029ff0).
    fn user_status(&mut self, mode: i32, groups: bool, frame: &mut dyn FnMut() -> bool) {
        let name = match mode { 0 => "visible", 1 => "away", _ => "invisible" };
        let api = &self.api;
        let code = status_request(api, [-2, -2, 1, mode, 0, -2], frame);
        events::send(format!("{{\"type\":\"native_control_result\",\"action\":\"{name}\",\"code\":{code}}}\n"));
        // 10040f39 starts ChgAllGrpStatus alongside ChangeMyStatus, whatever its result.
        if groups && api.zero(385) > 0 {
            let pushed = native_jobs::run(api, Request { start: &mut || api.call(404, &[mode as usize]), poll: 405, policy: Policy::Normal }, frame);
            log!("native POL group status mode={mode} result={pushed}");
            events::send(format!("{{\"type\":\"native_control_result\",\"action\":\"groups\",\"code\":{pushed}}}\n"));
        }
    }

    /// app 101984da: the latched pump error. Local sign-out only: 101aa28e,
    /// cleared flags, 101aa2ac (463), 434; no 816/817/836, no automatic
    /// re-login (B8).
    fn connection_lost(&mut self, error: i32) -> String {
        log!("native POL connection lost (POL result {error})");
        events::send("{\"type\":\"native_pol_status\",\"code\":-1025}\n".into());
        self.signed_in = false;
        crate::native_social::set_listening(false);
        // Keep what already arrived; the lost session can't take receipts.
        self.social.flush_pushed(&self.api, false);
        crate::native_social::discard_pushed();
        self.api.zero(463);
        self.api.zero(434);
        format!("PlayOnline session ended (POL result {error}). Sign in again to reconnect.")
    }

    /// Observe native snapshots through locked getters only (167/169/206/207/
    /// 838 and friend_locations). Never requests, pumps or consumes
    /// notifications, so it may run during play.
    pub fn observe_friends(&self) -> Result<FriendObserver, String> {
        let api = self.api.clone();
        let (stop, stopping) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("native-pol-friends".into())
            .spawn(move || run_friend_observer(api, stopping, events::send))
            .map_err(|_| "Could not observe native POL friends.")?;
        Ok(FriendObserver {
            stop,
            thread: Some(thread),
        })
    }
}

/// One Viewer frame for request drivers: window messages, one 818, one frame
/// of sleep. False when the owner must stop: launcher gone or a latched pump
/// error (1103, peeked like the Viewer's frame tail 101984da).
fn owner_frame(api: &Api) -> bool {
    owner_frame_at(api, FRAME)
}

fn owner_frame_at(api: &Api, interval: Duration) -> bool {
    pump_idle_messages();
    api.zero(818);
    pause_owner(interval);
    !events::pipe_closed() && api.zero(1103) >= 0
}

// Initial open/status/list/presence polls, including the wall-clock mail list,
// use this cadence. FRAME remains 20 ms for mail downloads/acknowledgements,
// their frame-counted backoffs, idle and game return.
const STARTUP_FRAME: Duration = if cfg!(feature = "native-fast-login-poll") {
    Duration::from_millis(1)
} else {
    FRAME
};

fn pause_owner(interval: Duration) {
    #[cfg(test)]
    if tests::capture_pause(interval) { return; }
    std::thread::sleep(interval);
}

/// One summary per startup stage, including failed stages. Gaps are between
/// pump entries, so they include scheduler/native work as well as our sleep.
struct StartupCadence {
    stage: &'static str,
    started: Instant,
    previous: Option<Instant>,
    pumps: u64,
    max_gap: Duration,
}
impl StartupCadence {
    fn new(stage: &'static str) -> Self {
        Self { stage, started: Instant::now(), previous: None, pumps: 0, max_gap: Duration::ZERO }
    }
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.previous.replace(now) {
            self.max_gap = self.max_gap.max(now.duration_since(previous));
        }
        self.pumps += 1;
    }
    fn frame(&mut self, api: &Api) -> bool {
        self.tick();
        owner_frame_at(api, STARTUP_FRAME)
    }
}
impl Drop for StartupCadence {
    fn drop(&mut self) {
        log!("native POL startup cadence: {} interval={}ms pumps={} max_gap={:.2}ms elapsed={:.2}s",
            self.stage, STARTUP_FRAME.as_millis(), self.pumps,
            self.max_gap.as_secs_f64() * 1000.0, self.started.elapsed().as_secs_f64());
    }
}

/// Concurrent requests under the Login policy, as the Viewer's job runner
/// starts several queued jobs in one tick (1003feb5): all start in list order,
/// then each frame polls them in order. -256 restarts on the next frame; no
/// timeout or retry; only a stopping owner cancels its own pending tickets.
fn run_concurrent(api: &Api, jobs: &[(&str, usize, usize)], frame: &mut dyn FnMut() -> bool) -> Vec<i32> {
    #[derive(Clone, Copy)]
    enum Job { Start, Poll(i32), Done(i32) }
    let mut state = vec![Job::Start; jobs.len()];
    let started = Instant::now();
    let mut reissues = vec![0u32; jobs.len()];
    let mut first_start = vec![true; jobs.len()];
    let mut max_frame = Duration::ZERO;
    loop {
        for (((job, retries), first), &(name, start, poll)) in state.iter_mut().zip(&mut reissues).zip(&mut first_start).zip(jobs) {
            let was_done = matches!(job, Job::Done(_));
            *job = match *job {
                Job::Start if api.zero(838) != 3 => Job::Done(native_jobs::ABORTED),
                Job::Start => match api.zero(start) {
                    ticket if ticket >= 0 => {
                        if std::mem::take(first) {
                            log!("native POL {name} load started at +{:.2}s", started.elapsed().as_secs_f64());
                        }
                        Job::Poll(ticket)
                    },
                    SLOTS_FULL => { *retries += 1; Job::Start },
                    error => Job::Done(error),
                },
                Job::Poll(ticket) => match api.one(poll, ticket as usize) {
                    0 => Job::Poll(ticket),
                    SLOTS_FULL if api.one(1107, SLOTS_FULL as usize) != 0 => { *retries += 1; Job::Start },
                    result => Job::Done(result),
                },
                done => done,
            };
            if !was_done {
                if let Job::Done(result) = *job {
                    log!("native POL {name} load result {result} in {:.2}s (full-table reissues {retries})", started.elapsed().as_secs_f64());
                }
            }
        }
        if state.iter().all(|j| matches!(j, Job::Done(_))) {
            break;
        }
        let frame_started = Instant::now();
        let running = frame();
        max_frame = max_frame.max(frame_started.elapsed());
        if !running {
            for job in &mut state {
                match *job {
                    Job::Poll(ticket) => { api.one(native_jobs::CANCEL, ticket as usize); *job = Job::Done(native_jobs::ABORTED); }
                    Job::Start => *job = Job::Done(native_jobs::ABORTED),
                    Job::Done(_) => {}
                }
            }
            break;
        }
    }
    log!("native POL concurrent loads completed in {:.2}s (longest owner frame {:.0}ms)", started.elapsed().as_secs_f64(), max_frame.as_secs_f64() * 1000.0);
    state.into_iter().map(|j| if let Job::Done(code) = j { code } else { native_jobs::ABORTED }).collect()
}

/// Keep this STA responsive to COM and injected-tool window messages between
/// games. GameStart owns message dispatch during play. Its WM_QUIT ends that
/// game, while explicit POL sign-out is handled by the session owner above.
pub(crate) fn pump_idle_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT};
    let mut message: MSG = unsafe { std::mem::zeroed() };
    for _ in 0..32 {
        if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 { break; }
        if message.message != WM_QUIT {
            unsafe { TranslateMessage(&message); DispatchMessageW(&message); }
        }
    }
}

/// Read the native cache again when its connection returns, even if its bytes
/// match the last connected snapshot. The launcher invalidates presence on loss.
/// Read-only: no request, cancel, pump or notification slot is ever called.
fn run_friend_observer(api: Api, stopping: mpsc::Receiver<()>, send: impl Fn(String)) {
    let mut before = Zeroizing::new(Vec::new());
    let mut locations_before = None;
    let mut game_friends = crate::friend_locations::GameFriends::default();
    let mut location_counts = None;
    let mut own_presence = None;
    let mut health = None;
    loop {
        let catalog = crate::native_social::CATALOG_ACCESS.lock().unwrap_or_else(|e| e.into_inner());
        let connected = api.zero(838) == 3;
        let connection_changed = health.replace(connected) != Some(connected);
        if connection_changed {
            send(format!(
                "{{\"type\":\"native_pol_status\",\"code\":{}}}\n",
                if connected { 0 } else { -1025 }
            ));
        }
        if connected {
            // Original locked getters; the word-returning class getter
            // leaves its upper return bits unspecified on x86.
            let presence = (api.zero(206), api.zero(207) as u16);
            if own_presence != Some(presence) || connection_changed {
                log!("native POL own presence: mode={} application={}", presence.0, presence.1);
                own_presence = Some(presence);
                send(format!("{{\"type\":\"native_own_presence\",\"mode\":{}}}\n", presence.0));
            }
            let mut blob = Zeroizing::new(vec![0u8; proto::FRIENDS_LEN]);
            blob[..8].copy_from_slice(b"XIPOLF\x01\0");
            for index in 0..proto::FRIEND_SLOTS {
                let (getter, index_in_table) = if index < 200 {
                    (167, index)
                } else {
                    (169, index - 200)
                };
                let out = &mut blob[8 + index * proto::FRIEND_RECORD_LEN..]
                    [..proto::FRIEND_RECORD_LEN];
                api.two(getter, index_in_table, out.as_mut_ptr() as usize);
            }
            let locations = game_friends.snapshot(&blob);
            if *blob != *before || locations != locations_before || connection_changed {
                let active = blob[8..].chunks_exact(proto::FRIEND_RECORD_LEN).filter(|r| {
                    let flags = u64::from_le_bytes(r[8..16].try_into().unwrap());
                    r[0x98] & 1 != 0 && flags & (1 << 11) != 0 && (flags >> 33) & 0x3ff == 1
                }).count();
                let mut counts = [active, 0, 0, 0];
                if let Some(extra) = &locations {
                    for r in extra[8..].chunks_exact(crate::friend_locations::RECORD_LEN) {
                        counts[1] += usize::from(r[0] != 0);
                        counts[2] += usize::from(r[16] != 0);
                        counts[3] += usize::from(r[80] != 0);
                    }
                }
                if location_counts != Some(counts) {
                    log!("native friend locations: active_ffxi={} characters={} areas={} other_worlds={}", counts[0], counts[1], counts[2], counts[3]);
                    location_counts = Some(counts);
                }
                let extra = locations.as_ref().map_or(String::new(), |b| format!(",\"locations\":\"{}\"", b64::encode(b)));
                send(format!(
                    "{{\"type\":\"native_friends\",\"payload\":\"{}\"{extra}}}\n",
                    b64::encode(&blob),
                ));
                before = blob;
                locations_before = locations;
            }
        }
        drop(catalog);
        match stopping.recv_timeout(Duration::from_millis(500)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Drop for NativeSession {
    /// Viewer close (03 §5): synchronous sign-out 101983fb when signed in, then
    /// 101aa2d4 (1502 clear), 1019869a (host unregistration) and 10019464
    /// (920 file worker stop, 915 option.bin mutex). No 852 (B9).
    fn drop(&mut self) {
        crate::native_social::set_listening(false);
        if self.home {
            // Store (and while still signed in, answer) the last pushes first.
            let receipts = self.signed_in && self.api.zero(838) == 3;
            self.social.flush_pushed(&self.api, receipts);
        }
        crate::native_social::discard_pushed();
        if self.signed_in {
            self.signed_in = false;
            self.api.call(816, &[1]);
            let started = Instant::now();
            loop {
                let state = self.api.zero(817);
                if state == 7 {
                    log!("native POL sign-out completed (state 7)");
                    break;
                }
                if state < 0 {
                    log!("error: native POL sign-out failed (state {state})");
                    break;
                }
                if started.elapsed() >= CLOSE_LIMIT {
                    log!("error: native POL sign-out still at state {state} after {}s; closing anyway", CLOSE_LIMIT.as_secs());
                    break;
                }
                std::thread::sleep(CLOSE_POLL);
            }
            self.api.zero(463);
            self.api.zero(434);
        }
        if self.home {
            self.api.se(None);
        }
        self.unregister_host();
        if self.files {
            self.api.zero(920);
            self.api.zero(915);
        }
    }
}

pub struct FriendObserver {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for FriendObserver {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The sign-in tick (app 1019ce44): 30 finishes the Open when 838 shows the
/// expected link; negative states fail; 31..38 (and 39) keep polling.
fn connection_result(state: i32, linked: impl FnOnce() -> i32, expected: i32) -> Option<Result<(), ()>> {
    if state == 30 {
        Some(if linked() == expected { Ok(()) } else { Err(()) })
    } else if state < 0 {
        Some(Err(()))
    } else {
        None
    }
}

/// Reserve disjoint POL message stores, without adopting official or foreign
/// folders. The ownership marker is created atomically; no hash-modulo aliasing.
fn claim_home(viewer: &Path, key: &str) -> Result<u32, String> {
    if !viewer.is_absolute() {
        return Err("PlayOnline's message root must be an absolute path.".into());
    }
    if key.len() != 40
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid native POL profile key.".into());
    }
    let public = viewer.join("pub");
    fs::create_dir_all(&public).map_err(|_| "Could not create PlayOnline message storage.")?;
    let expected = format!("Crystal POL profile v1\n{key}\n");
    let owned = |path: &Path| {
        fs::metadata(path)
            .ok()
            .is_some_and(|m| m.is_file() && m.len() == expected.len() as u64)
            && fs::read_to_string(path).ok().as_deref() == Some(&expected)
    };
    // Prefer an existing owned store even when a lower-numbered store is free.
    let existing = (50..100).find(|slot| {
        let marker = public
            .join(format!("home{slot:02}"))
            .join(".crystal-profile");
        let private = viewer.join("usr").join(format!("home{slot:02}"));
        owned(&marker) && (!private.exists() || owned(&private.join(".crystal-profile")))
    });
    let slot = if let Some(slot) = existing {
        slot
    } else {
        let mut claimed = None;
        for slot in 50..100 {
            let home = public.join(format!("home{slot:02}"));
            if viewer.join("usr").join(format!("home{slot:02}")).exists() {
                continue;
            }
            if fs::create_dir(&home).is_err() {
                continue;
            }
            let marker = home.join(".crystal-profile");
            let created = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
                .and_then(|mut file| {
                    file.write_all(expected.as_bytes())?;
                    file.sync_all()
                });
            if created.is_ok() {
                claimed = Some(slot);
                break;
            }
            // Preserve any uncertain contents; never adopt or recursively delete.
        }
        claimed.ok_or("No unused PlayOnline message store is available in home50–home99.")?
    };
    let home = public.join(format!("home{slot:02}"));
    let private = viewer.join("usr").join(format!("home{slot:02}"));
    if !private.exists() {
        fs::create_dir_all(&private)
            .map_err(|_| "Could not prepare the account's PlayOnline profile folder.")?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(private.join(".crystal-profile"))
            .and_then(|mut file| {
                file.write_all(expected.as_bytes())?;
                file.sync_all()
            })
            .map_err(|_| "Could not reserve the account's PlayOnline profile folder.")?;
    } else if !owned(&private.join(".crystal-profile")) {
        return Err("The PlayOnline profile folder belongs to another client.".into());
    }
    for relative in [
        "msg/s/a",
        "msg/s/b",
        "msg/r/a",
        "msg/r/b",
        "mail/s/a",
        "mail/s/b",
        "mail/r/a",
        "mail/r/b",
        "mail/uidl",
        "mail/adr",
        "hidden",
        "log",
        "open/GreetingCards",
        "open/SaveFiles",
        "open/ScreenShots/FinalFantasyXI",
        "open/ContentsData/FinalFantasyXI",
        "open/etc",
    ] {
        fs::create_dir_all(home.join(relative))
            .map_err(|_| "Could not prepare the account's PlayOnline message folders.")?;
    }
    Ok(slot)
}

// The viewer's resource IDs, app.dll 10057911..10057a11. Read the installed
// localization instead of substituting approximate message text.
const TITLE_IDS: [u16; 10] = [
    0x46c9, 0x46d0, 0x46d1, 0x46d2, 0x46d3, 0x46d4, 0x46d6, 0x46d8, 0x46dc, 0x46da,
];
const TEXT_IDS: [u16; 10] = [
    0x46e6, 0x46ea, 0x46ec, 0x4ab8, 0x3bad, 0x46d7, 0x46d9, 0x46dd, 0x46db, 0x46d5,
];

/// A notification string as the Viewer hands it to the core: 1302 texts lose
/// every CR first (app 10057a46); 1072 titles are passed unchanged.
fn viewer_text(setter: usize, mut text: Vec<u8>) -> Vec<u8> {
    if setter == 1302 {
        text.retain(|&b| b != b'\r');
    }
    text
}

fn install_message_texts(api: &Api, viewer: &Path) -> Result<(), String> {
    let path = viewer.join("viewer/data/common/StringTable.bin");
    let size = fs::metadata(&path)
        .map_err(|_| "PlayOnline's message localization is missing.")?
        .len();
    if size > 2_000_000 {
        return Err("PlayOnline's message localization is too large.".into());
    }
    let raw = fs::read(path).map_err(|_| "Could not read PlayOnline's message localization.")?;
    let decoded = decode_pex(&raw).ok_or("PlayOnline's message localization is invalid.")?;
    for (setter, ids, max) in [(1072, &TITLE_IDS, 127), (1302, &TEXT_IDS, 255)] {
        for (index, &id) in ids.iter().enumerate() {
            let text = resource_text(&decoded, id)
                .ok_or("PlayOnline's message localization is incomplete or unsupported.")?;
            let mut terminated = viewer_text(setter, text);
            if terminated.len() > max {
                return Err(
                    "PlayOnline's message localization exceeds its native field size.".into(),
                );
            }
            terminated.push(0);
            api.two(setter, index, terminated.as_ptr() as usize);
        }
    }
    Ok(())
}

/// Installed-core ABI check: host registration, the real profile list (read
/// only), member select and connection choice with synthetic credentials.
/// Never calls 814/815/816/818 or any request; nothing reaches the network.
#[cfg(test)]
pub unsafe fn smoke_prepare(table: &[usize], base: usize, viewer: &Path) {
    validate_entries(table, base).unwrap();
    let mut session = NativeSession::new(Api(table.into()), viewer, 1, 10);
    // Reproduce the injector's setting in this test's own core, then restore
    // the stock value through the production function. No remote operation.
    let before = table.to_vec();
    session.api.f::<unsafe extern "C" fn(i32)>(1075)(1);
    assert_eq!(session.api.zero(1074), 1);
    session.api.restore_viewer_policy().unwrap();
    assert_eq!(session.api.zero(1074), 0);
    session.api.restore_viewer_policy().unwrap();
    assert_eq!(session.api.zero(1074), 0);
    assert_eq!(table, before, "restoring native policy must preserve all callbacks");
    install_message_texts(&session.api, viewer).unwrap();
    // FFXi.dll/FFXiMain read their language from 421, which falls back to the
    // 1528 value while 420 is unset and caches its first answer. Run the
    // production host calls (125's path code reads 421 itself), then check
    // FFXI would get English for the US Viewer.
    // The core keeps these pointers, as production's leaked strings.
    let root: &'static CStr = Box::leak(std::ffi::CString::new(viewer.to_str().unwrap()).unwrap().into_boxed_c_str());
    let key: &'static CStr = Box::leak(std::ffi::CString::new("SOFTWARE\\PlayOnlineUS").unwrap().into_boxed_c_str());
    session.host_paths(root, key);
    assert_eq!(session.api.zero(421), 1, "FFXI must start in English for the US Viewer");

    // Host registration as app 1019b874, the installed inet_w.bin through
    // 825/826 and its 884 validation, then the built-in default for comparison.
    session.register_callbacks();
    assert_eq!(*((base + 0x99424) as *const usize), table[334], "871 routes pushes to the core dispatcher");
    session.api.call(819, &[0]);
    session.api.call(824, &[malloc as *const () as usize, free as *const () as usize, realloc as *const () as usize]);
    session.registered = true;
    let file = native_profile::read_file(viewer).expect("installed usr\\all\\inet_w.bin");
    let (header, from_file) = native_profile::load(&session.api, Some(&file)).unwrap();
    assert!(from_file, "the installed profile file must pass the Viewer's validation");
    assert_eq!(*((base + 0x9926c) as *const usize), header);
    let snapshot = |api: &Api| {
        let record = api.word(849, &[0]);
        let mut bytes = std::slice::from_raw_parts(header_of(base) as *const u8, native_profile::HEADER_LEN).to_vec();
        bytes.extend_from_slice(std::slice::from_raw_parts(record as *const u8, native_profile::RECORD_LEN));
        bytes
    };
    let installed = snapshot(&session.api);
    session.api.zero(828);
    let (_, from_file) = native_profile::load(&session.api, None).unwrap();
    assert!(!from_file);
    let built = snapshot(&session.api);
    assert_eq!(installed[..native_profile::HEADER_LEN], built[..native_profile::HEADER_LEN], "default header");
    assert_eq!(installed[native_profile::HEADER_LEN..], built[native_profile::HEADER_LEN..], "default WindowsNetWork record");
    session.profiles = *((base + 0x9926c) as *const usize);
    session.api.call(1149, &[no_inactivity_warning as *const () as usize, no_inactivity_expiry as *const () as usize]);

    // Member select (stock failure), then the connect screen's profile load.
    let credentials = NativeCredentials {
        pol_id: *b"ABCD1234\0",
        pol_password: *b"fake password\0\0\0",
        se_id: *b"synthetic\0\0\0\0\0\0\0\0",
        se_secret: [0x51; 20],
        otp: Some(*b"123456"),
        profile_key: "a".repeat(40),
    };
    session.select_member(50, &credentials);
    assert_eq!(*((base + 0x992e0) as *const u32), 50);
    assert_eq!(session.api.zero(891), native_profile::PROFILE_NOT_FOUND, "Viewer members name no profile");
    let name = native_profile::selected_name(&session.api, session.profiles).unwrap();
    native_profile::connect(&session.api, &name).unwrap();
    assert_eq!(*((base + 0x99c80) as *const i32), 1, "connection kind LAN");
    assert_eq!(*(session.api.word(835, &[]) as *const i32), 1, "Open argument kind LAN");
    session.api.se(Some(&credentials));
    assert_eq!(*((base + 0xaaa98) as *const u32), 2, "native SE + OTP mode");
    assert_eq!(
        std::slice::from_raw_parts((base + 0xaaaad) as *const u8, 20),
        &[0xae; 20],
        "native transformed secret retained"
    );
    assert_ne!(session.api.zero(838), 3);
    // Close teardown without a member session: 1502 clear, 828/824/819/872/1149.
    session.home = true;
    session.files = false; // smoke_message_files owns this test's file worker
    drop(session);
    assert_eq!(*((base + 0xaaa98) as *const u32), 0, "native credentials cleared");
    assert_eq!(*((base + 0x9926c) as *const usize), 0, "profile list freed");
    assert_eq!(*((base + 0x9925c) as *const usize), 0, "host heap unregistered");
    assert_eq!(*((base + 0x99424) as *const usize), table[334], "872 restores the core's defaults");
}
#[cfg(test)]
unsafe fn header_of(base: usize) -> usize { *((base + 0x9926c) as *const usize) }

#[cfg(test)]
pub unsafe fn smoke_message_files(table: &[usize], base: usize) {
    validate_entries(table, base).unwrap();
    let api = Api(table.into());
    let root = std::env::temp_dir().join(format!(
        "crystal-native-mail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let slot = claim_home(&root, &"a".repeat(40)).unwrap();
    let root_c = std::ffi::CString::new(root.to_str().unwrap()).unwrap();
    api.one(125, root_c.as_ptr() as usize);
    api.one(870, slot as usize);
    api.zero(932);
    let mut directory = [0u8; 512];
    let get_path = api.f::<unsafe extern "C" fn(i32, *mut u8, i32, i32) -> i32>(126);
    let length = get_path(6, directory.as_mut_ptr(), 512, 0);
    assert!(length > 0 && length < 512);
    let directory =
        std::path::PathBuf::from(std::str::from_utf8(&directory[..length as usize]).unwrap());
    assert_eq!(directory, root.join(format!("pub/home{slot:02}/msg/s/a")));
    let path = directory.join("offline-test.bin");
    let file = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    let open = api.f::<unsafe extern "C" fn(*const u8, u32) -> i32>(921);
    let ticket = open(file.as_ptr().cast(), 0x0e); // FFXI's failing friend-request mode
    assert!(ticket >= 0, "native message-file open failed: {ticket}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = api.one(922, ticket as usize);
    while ready == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        ready = api.one(922, ticket as usize);
    }
    assert_eq!(ready, 1, "native message-file open completion");
    let bytes = b"Crystal native message storage check";
    let write = api.f::<unsafe extern "C" fn(i32, *const u8, u32) -> i32>(928);
    assert!(write(ticket, bytes.as_ptr(), bytes.len() as u32) >= 0);
    let poll = api.f::<unsafe extern "C" fn(i32, *mut u32) -> i32>(929);
    let mut written = 0;
    let mut ready = poll(ticket, &mut written);
    while ready == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        ready = poll(ticket, &mut written);
    }
    assert_eq!(ready, 1, "native message-file write completion");
    assert_eq!(written, bytes.len() as u32);
    api.one(923, ticket as usize);
    api.zero(920); // the Viewer's close stop (= 933); 923 only enqueues its close
    crate::native_social::smoke_read_file(table, file.as_bytes_with_nul(), bytes);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    // Test-owned absolute temporary root only; contains no account data.
    fs::remove_dir_all(root).unwrap();
}

fn decode_pex(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 16 || &raw[..4] != b"PEX\0" {
        return None;
    }
    let n = u32::from_le_bytes(raw[4..8].try_into().ok()?) as usize;
    if !(16..=2_000_000).contains(&n) {
        return None;
    }
    let mut bit = 128usize;
    let mut bits = |count: usize| -> Option<usize> {
        let mut value = 0;
        for shift in 0..count {
            value |= usize::from((raw.get(bit / 8)? >> (bit % 8)) & 1) << shift;
            bit += 1;
        }
        Some(value)
    };
    let mut out = vec![0; 4096];
    out.reserve(n);
    while out.len() < 4096 + n {
        if bits(1)? == 0 {
            out.push(bits(8)? as u8);
        } else {
            let offset = bits(12)?;
            let length = bits(4)? + 2;
            let start = out.len() - 4096 + offset;
            if start >= out.len() || out.len() + length > 4096 + n {
                return None;
            }
            for index in 0..length {
                out.push(*out.get(start + index)?);
            }
        }
    }
    Some(out.split_off(4096))
}

fn resource_text(decoded: &[u8], id: u16) -> Option<Vec<u8>> {
    let mut at = 16;
    while at + 4 <= decoded.len() {
        let size = u16::from_le_bytes(decoded[at..at + 2].try_into().ok()?) as usize;
        let key = u16::from_le_bytes(decoded[at + 2..at + 4].try_into().ok()?);
        if size < 4 || size % 2 != 0 || at.checked_add(size)? > decoded.len() {
            return None;
        }
        if key == id {
            let units: Vec<u16> = decoded[at + 4..at + size]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
            // Verified US templates are ASCII. Refuse unsupported encodings rather
            // than corrupting a native sprintf template or replacing its text.
            if units[..end].iter().any(|u| *u > 127) {
                return None;
            }
            return Some(units[..end].iter().map(|u| *u as u8).collect());
        }
        at += size;
    }
    None
}

#[cfg(test)]
mod tests;
