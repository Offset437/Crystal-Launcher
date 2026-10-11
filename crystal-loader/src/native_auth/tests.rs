use super::*;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

// --- A recording fake core: every used slot records (slot, args) in order --

#[derive(Default)]
struct Fake {
    calls: Vec<(usize, Vec<usize>)>,
    scripted: HashMap<usize, VecDeque<usize>>,
    open0: Vec<i32>,
    open3: Vec<i32>,
    states: VecDeque<i32>,
    mode: usize,
    linked: i32,
    header: Vec<u8>,
    records: Vec<u8>,
    connection: [i32; 8],
    checksum: u32,
}
thread_local! { static FAKE: RefCell<Fake> = RefCell::new(Fake::default()); }
thread_local! { static PAUSES: RefCell<Option<Vec<(Duration, usize)>>> = const { RefCell::new(None) }; }

// Capture only this test thread's requested sleeps; other tests retain their
// real timing. The call index locates each pause in the actual native flow.
pub(super) fn capture_pause(interval: Duration) -> bool {
    PAUSES.with(|pauses| {
        let mut pauses = pauses.borrow_mut();
        let Some(pauses) = pauses.as_mut() else { return false; };
        pauses.push((interval, raw_calls().len()));
        true
    })
}

struct CapturePauses;
impl CapturePauses {
    fn start() -> Self { PAUSES.with(|p| *p.borrow_mut() = Some(Vec::new())); Self }
    fn take(&self) -> Vec<(Duration, usize)> { PAUSES.with(|p| std::mem::take(p.borrow_mut().as_mut().unwrap())) }
}
impl Drop for CapturePauses {
    fn drop(&mut self) { PAUSES.with(|p| *p.borrow_mut() = None); }
}

fn arity(slot: usize) -> usize {
    match slot {
        376 | 195 => 6,
        1502 => 4,
        824 => 3,
        825 | 826 | 854 | 856 | 852 | 858 | 868 | 892 | 1149 | 814 | 1072 | 1302 | 167 | 169 => 2,
        870 | 851 | 886 | 853 | 965 | 1527 | 1309 | 871 | 846 | 849 | 1032 | 1140 | 1175 | 969 | 288 | 331 | 375
        | 164 | 172 | 176 | 384 | 377 | 405 | 404 | 819 | 125 | 367 | 1075 | 1107 | 337 | 199 | 816 | 1305 | 330 | 1528 => 1,
        329 => 2,
        _ => 0,
    }
}

fn dispatch(slot: usize, a: [usize; 6]) -> usize {
    FAKE.with(|f| {
        let mut f = f.borrow_mut();
        f.calls.push((slot, a[..arity(slot)].to_vec()));
        if let Some(value) = f.scripted.get_mut(&slot).and_then(|q| q.pop_front()) {
            return value;
        }
        match slot {
            814 => {
                f.mode = a[0];
                f.states = if a[0] == 0 { f.open0.clone() } else { f.open3.clone() }.into();
                0
            }
            815 => {
                let state = if f.states.len() > 1 { f.states.pop_front().unwrap() } else { f.states.front().copied().unwrap_or(0) };
                if state == 30 {
                    f.linked = if f.mode == 0 { 2 } else { 3 };
                }
                state as usize
            }
            838 => f.linked as usize,
            825 => unsafe {
                *(a[0] as *mut usize) = f.header.as_mut_ptr() as usize;
                *(a[1] as *mut u32) = f.header.len() as u32;
                0
            },
            826 => unsafe {
                let n = f.records.len();
                *(a[0] as *mut usize) = if n > 0 { f.records.as_mut_ptr() as usize } else { 0 };
                *(a[1] as *mut u32) = n as u32;
                usize::from(n > 0)
            },
            884 => f.checksum as usize,
            809 => 1,
            849 => f.records.as_ptr() as usize,
            835 => f.connection.as_ptr() as usize,
            891 | 817 => if slot == 891 { 1 } else { 7 },
            190 => -1i32 as usize,
            207 => 1000,
            163 => 1,
            171 => 2,
            175 => 3,
            383 => 4,
            376 => 5,
            404 => 6,
            329 => 7, // the social entry's mail list: an empty inbox
            164 | 172 | 176 | 384 | 375 | 377 | 405 | 330 => 1,
            _ => 0,
        }
    })
}

macro_rules! fake_table {
    ($($slot:literal)*) => {
        fn fake_table() -> Vec<usize> {
            let mut table = vec![0usize; 1560];
            $({
                unsafe extern "C" fn stub(a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> usize {
                    dispatch($slot, [a, b, c, d, e, f])
                }
                table[$slot] = stub as *const () as usize;
            })*
            table
        }
    };
}
fake_table!(125 126 163 164 165 166 167 169 171 172 175 176 190 195 196 199 206 207 288 322 323 326 327 328
    329 330 331 333 334 337 367 374 375 376 377 383 384 385 401 404 405 434 463 705 807 809 814 815 816
    817 818 819 823 824 825 826 828 835 836 838 845 846 849 851 852 853 854 856 858 868 870 871 872 884
    886 891 892 914 915 920 932 933 965 969 1032 1072 1074 1075 1103 1107 1140 1149 1175 1302 1304 1305 1309 1502 1527 1528);

fn script(slot: usize, values: &[i32]) {
    FAKE.with(|f| f.borrow_mut().scripted.entry(slot).or_default().extend(values.iter().map(|v| *v as usize)));
}
fn set_linked(linked: i32) { FAKE.with(|f| f.borrow_mut().linked = linked); }
fn raw_calls() -> Vec<(usize, Vec<usize>)> { FAKE.with(|f| f.borrow().calls.clone()) }
fn clear_calls() { FAKE.with(|f| f.borrow_mut().calls.clear()); }
fn show(v: usize) -> String {
    let i = v as i32;
    if (-0x10000..0x10000).contains(&i) { i.to_string() } else { "*".into() }
}
/// The recorded calls as `slot(args)`, pointers shown as `*`.
fn trace(skip: &[usize]) -> Vec<String> {
    raw_calls().iter().filter(|(s, _)| !skip.contains(s))
        .map(|(s, a)| format!("{s}({})", a.iter().map(|v| show(*v)).collect::<Vec<_>>().join(",")))
        .collect()
}
fn until(calls: &[String], last: &str) -> Vec<String> {
    let end = calls.iter().position(|c| c.starts_with(last)).unwrap_or_else(|| panic!("{last} missing from {calls:?}"));
    calls[..=end].to_vec()
}

/// A fake Viewer root with a stock-shaped usr\all\inet_w.bin, and a fresh fake core.
fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("crystal-pol-parity-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir_all(root.join("usr/all")).unwrap();
    let mut file = vec![0u8; native_profile::HEADER_LEN + native_profile::RECORD_LEN];
    file[..41].copy_from_slice(b"PlayOnline NetWork Setting File Ver 1.00\0");
    for (offset, value) in [(0x40, 0xe4i32), (0x44, 0x9ac), (0x48, 0x1234), (0x4c, !0x1234), (0x50, 1), (0x54, 0), (0x58, 0), (0x5c, -1), (0x60, 1), (0x64, 0)] {
        file[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    let record = native_profile::HEADER_LEN;
    file[record + 8..record + 8 + 14].copy_from_slice(b"encoded-name-1");
    file[record + 0x10c..record + 0x110].copy_from_slice(&1i32.to_le_bytes());
    fs::write(root.join("usr/all/inet_w.bin"), &file).unwrap();
    FAKE.with(|f| {
        *f.borrow_mut() = Fake {
            open0: vec![0, 17, 30],
            open3: vec![18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 40, 41, 29, 30],
            header: vec![0; native_profile::HEADER_LEN],
            records: vec![0; native_profile::RECORD_LEN],
            connection: [1, 0, 0, 0, 0, 0, 0, 0],
            checksum: 0x1234,
            ..Fake::default()
        }
    });
    root
}

fn session(viewer: &Path) -> NativeSession {
    NativeSession::new(Api::test_table(&fake_table()), viewer, 1, 10)
}

fn credentials() -> NativeCredentials {
    NativeCredentials {
        pol_id: *b"ABCD1234\0",
        pol_password: *b"fake password\0\0\0",
        se_id: *b"synthetic\0\0\0\0\0\0\0\0",
        se_secret: [0x51; 20],
        otp: Some(*b"123456"),
        profile_key: "a".repeat(40),
    }
}

/// Leave the session without a teardown so later assertions see only their calls.
fn forget(mut session: NativeSession) {
    session.signed_in = false;
    session.home = false;
    session.registered = false;
    session.files = false;
    drop(session);
}

/// Tests touching the global command state run one at a time.
static CONTROL_TEST: Mutex<()> = Mutex::new(());

// --- Sign-in --------------------------------------------------------------

#[test]
fn startup_poll_pilot_accelerates_mail_list_and_keeps_return_and_idle_on_viewer_cadence() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = CStr::from_bytes_with_nul(b"C:\\PlayOnlineViewer\\\0").unwrap();
    let key = CStr::from_bytes_with_nul(b"SOFTWARE\\PlayOnlineUS\0").unwrap();
    s.bootstrap_core(root, key).unwrap();
    script(891, &[native_profile::PROFILE_NOT_FOUND]);
    // Exercise a pending poll in every startup request stage, including mail.
    for poll in [375, 164, 330, 377] { script(poll, &[0, 1]); }
    script(190, &[0]); // actually publish presence
    let pauses = CapturePauses::start();
    s.sign_in_core(50, &credentials()).unwrap();
    let calls = raw_calls();
    let first = |slot| calls.iter().position(|(s, _)| *s == slot).unwrap();
    let stages = [first(814), first(374), first(163), first(329), first(376)];
    assert!(stages.windows(2).all(|p| p[0] < p[1]), "open, status, lists, mail, presence stay ordered");
    let startup = pauses.take();
    let expected = Duration::from_millis(if cfg!(feature = "native-fast-login-poll") { 1 } else { 20 });
    for (i, &start) in stages.iter().enumerate() {
        let end = stages.get(i + 1).copied().unwrap_or(calls.len() + 1);
        let sleeps: Vec<_> = startup.iter().filter(|(_, at)| *at > start && *at <= end).map(|(d, _)| *d).collect();
        assert!(!sleeps.is_empty(), "stage {i} must exercise a pending poll");
        assert!(sleeps.iter().all(|d| *d == expected), "stage {i}: {sleeps:?}");
    }
    // The same requests after a game, and the idle pump, stay at 20 ms even
    // in the fast build. Mail retry/frame policies still share the fixed FRAME.
    clear_calls();
    for poll in [164, 330, 377] { script(poll, &[0, 1]); }
    script(190, &[0]);
    s.return_from_game().unwrap();
    assert!(owner_frame(&s.api));
    let ordinary = pauses.take();
    assert!(ordinary.len() >= 4);
    assert!(ordinary.iter().all(|(d, _)| *d == Duration::from_millis(20)));
    assert_eq!(FRAME * 3600, Duration::from_secs(72));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_in_drives_the_core_in_the_stock_viewer_order() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = CStr::from_bytes_with_nul(b"C:\\PlayOnlineViewer\\\0").unwrap();
    let key = CStr::from_bytes_with_nul(b"SOFTWARE\\PlayOnlineUS\0").unwrap();
    s.bootstrap_core(root, key).unwrap();
    script(891, &[native_profile::PROFILE_NOT_FOUND]);
    s.sign_in_core(50, &credentials()).unwrap();
    let calls = trace(&[818, 815, 838, 1103]);
    assert_eq!(until(&calls, "384("), [
        // app.dll startup (01 §2.1): no 420, no lone 815.
        "1528(1)", "125(*)", "367(*)", "870(-1)", "932()", "914()", "965(1)", "1527(0)", "1309(*)",
        "871(*)", "819(0)", "824(*,*,*)", "825(*,*)", "826(*,*)", "884()", "1149(*,*)",
        // Member select 101a425f; its 891 fails like stock.
        "870(50)", "854(*,*)", "851(*)", "856(*,*)", "852(*,0)", "858(*,*)", "886(*)", "868(*,*)", "853(*)", "891()",
        // Connect screen 10199d02: the selected profile, 891 == 1, LAN kind.
        "809()", "849(0)", "886(*)", "891()", "835()",
        // Open(0) to the network level, SE 1502, then Open(1) member login (cold stock probe).
        "835()", "814(0,*)", "1502(2,*,*,*)", "835()", "814(1,*)",
        // Post sign-in: delivery on, 871 again, the status load alone, then four lists together.
        "1032(1)", "871(*)", "374()", "375(0)", "163()", "171()", "175()", "383()", "164(1)", "172(2)", "176(3)", "384(4)",
    ]);
    let raw = raw_calls();
    // Main menu entry after the lists: the mail list, then the Viewer presence
    // decision (already the Viewer here, so nothing is published).
    let tail = &calls[calls.iter().position(|c| c == "384(4)").unwrap() + 1..];
    assert_eq!(&tail[..5], ["1304()", "329(*,200)", "330(7)", "190()", "207()"]);
    // A6: no status request of our own and no 420 at sign-in.
    assert!(!raw.iter().any(|(s, _)| [195, 196, 376, 377, 420].contains(s)));
    // A4/A9: Open(0) then Open(1); each 815 follows that frame's 818.
    let steps: Vec<_> = raw.iter().enumerate().filter(|(_, (s, _))| *s == 815).map(|(i, _)| i).collect();
    assert_eq!(steps.len(), 18);
    assert!(steps.iter().all(|&i| raw[i - 1].0 == 818));
    let opens: Vec<_> = raw.iter().enumerate().filter(|(_, (s, _))| *s == 814).map(|(i, _)| i).collect();
    assert_eq!(opens.len(), 2);
    assert!(opens[0] < steps[0] && *steps.last().unwrap() < raw.iter().position(|(s, _)| *s == 374).unwrap());
    assert!(raw[..opens[0]].iter().all(|(s, _)| *s != 815), "no 815 before the Open");
    assert!(s.signed_in && s.home && s.registered);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_in_failure_is_a_negative_state_after_transitional_ones() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = CStr::from_bytes_with_nul(b"C:\\v\\\0").unwrap();
    s.bootstrap_core(root, root).unwrap();
    FAKE.with(|f| f.borrow_mut().open3 = vec![18, 31, 32, 33, 39, -1036]);
    script(823, &[-1036]);
    let error = s.sign_in_core(50, &credentials()).unwrap_err();
    assert!(error.contains("state -1036") && error.contains("POL result -1036"), "{error}");
    assert!(!s.signed_in);
    assert!(!raw_calls().iter().any(|(s, _)| *s == 374), "no loads after a failed login");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn invalid_or_missing_profile_files_get_the_viewer_default_in_memory_only() {
    let viewer = fixture();
    let s = session(&viewer);
    s.api.call(824, &[1, 2, 3]);
    let path = viewer.join("usr/all/inet_w.bin");
    let stock = fs::read(&path).unwrap();
    clear_calls();
    FAKE.with(|f| f.borrow_mut().checksum = 0x9999); // 884 mismatch
    let (_, from_file) = native_profile::load(&s.api, Some(&stock)).unwrap();
    assert!(!from_file);
    assert_eq!(trace(&[]), ["825(*,*)", "826(*,*)", "884()", "845()", "858(*,*)", "892(*,*)", "892(*,*)", "846(*)"]);
    clear_calls();
    let (_, from_file) = native_profile::load(&s.api, None).unwrap();
    assert!(!from_file);
    assert_eq!(trace(&[]), ["825(*,*)", "845()", "858(*,*)", "892(*,*)", "892(*,*)", "846(*)"]);
    clear_calls();
    let (_, from_file) = native_profile::load(&s.api, Some(&stock[..stock.len() - 1])).unwrap();
    assert!(!from_file, "a truncated file is never partly used");
    assert_eq!(fs::read(&path).unwrap(), stock, "the Viewer's file is never written");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- Game hand-off and return ---------------------------------------------

#[test]
fn game_start_mirrors_the_viewer_exit() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    s.registered = true;
    s.attach_game();
    assert_eq!(trace(&[]), ["1305(0)", "1032(1)", "1140(0)", "1175(1)", "969(0)", "463()", "828()", "824(0,0,0)", "819(0)", "872()", "1149(0,0)"]);
    assert_eq!(CONTROL.load(Ordering::Acquire), GAME);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn return_from_game_restarts_the_viewer_on_the_live_session() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    set_linked(3);
    s.detach_game();
    assert_eq!(trace(&[]), ["288(0)"]);
    clear_calls();
    script(190, &[0]); // a character is selected: publish clears it
    s.return_from_game().unwrap();
    let calls = trace(&[838, 818, 1103, 1107]);
    assert_eq!(until(&calls, "377("), [
        "965(1)", "1527(0)", "1309(*)",
        "819(0)", "824(*,*,*)", "825(*,*)", "826(*,*)", "884()", "1149(*,*)",
        "1032(1)", "871(*)",
        "163()", "171()", "175()", "383()", "164(1)", "172(2)", "176(3)", "384(4)",
        // The mail list on every entry, then the Viewer presence.
        "1304()", "329(*,200)", "330(7)",
        "190()", "376(-2,-1,1,-2,0,1000)", "377(5)",
    ]);
    assert!(!raw_calls().iter().any(|(s, _)| *s == 374), "no status load on return");

    // No character and already the Viewer: nothing is published.
    clear_calls();
    s.return_from_game().unwrap();
    let calls = trace(&[838, 818, 1103, 1107]);
    let tail = &calls[calls.iter().position(|c| c == "330(7)").unwrap() + 1..];
    assert_eq!(&tail[..2], ["190()", "207()"]);
    assert!(!calls.iter().any(|c| c.starts_with("376(")));

    // No character, another application: publish the Viewer only.
    clear_calls();
    script(207, &[1]);
    s.return_from_game().unwrap();
    assert!(trace(&[]).contains(&"376(-2,-2,1,-2,0,1000)".to_string()));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn return_without_a_member_session_is_a_sign_out_not_a_reload() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    set_linked(0);
    assert!(s.return_from_game().is_err());
    assert!(!s.signed_in);
    assert!(!raw_calls().iter().any(|(s, _)| [163, 824, 871, 376].contains(s)));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- Idle Viewer ----------------------------------------------------------

#[test]
fn status_changes_push_to_groups_only_when_asked_and_in_a_group() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    script(385, &[1]);
    s.user_status(0, true, &mut || true);
    assert_eq!(trace(&[838]), ["376(-2,-2,1,0,0,-2)", "377(5)", "385()", "404(0)", "405(6)"]);
    clear_calls();
    s.user_status(4, false, &mut || true);
    assert_eq!(trace(&[838]), ["376(-2,-2,1,4,0,-2)", "377(5)"]);
    clear_calls();
    s.user_status(1, true, &mut || true); // 385() == 0: no group active
    assert_eq!(trace(&[838]), ["376(-2,-2,1,1,0,-2)", "377(5)", "385()"]);
    clear_calls();
    // 10040f39 starts both jobs on apply: a failed status change is still pushed.
    script(377, &[-5000]);
    script(385, &[1]);
    s.user_status(0, true, &mut || true);
    assert_eq!(trace(&[838, 1107]), ["376(-2,-2,1,0,0,-2)", "377(5)", "385()", "404(0)", "405(6)"]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn auto_away_counts_gated_frames_and_checks_idle_every_3601() {
    let idle = Duration::from_secs(10 * 60 + 1);
    let mut away = AutoAway::new(10);
    for _ in 0..3600 { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "the 3601st Visible frame checks");
    for _ in 0..3600 { assert!(!away.frame(true, idle)); }
    assert!(!away.frame(true, Duration::from_secs(600)), "idle must exceed the setting");
    for _ in 0..3600 { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "the check resets the counter either way");
    let mut away = AutoAway::new(10);
    for _ in 0..10_000 { assert!(!away.frame(false, idle)); }
    for _ in 0..3600 { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "frames while not Visible do not count");
    let mut off = AutoAway::new(0);
    assert!((0..10_000).all(|_| !off.frame(true, Duration::from_secs(86_400))));
    let mut hour = AutoAway::new(60);
    for _ in 0..3600 { hour.frame(true, idle); }
    assert!(!hour.frame(true, Duration::from_secs(3599)));
}

#[test]
fn auto_away_sends_the_viewer_away_status_only_while_visible() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.auto_away.frames = AUTO_AWAY_FRAMES;
    script(206, &[1]); // Away already: not gated, counter unchanged
    assert_eq!(s.auto_away_tick(Duration::from_secs(3600), &mut || true), None);
    assert_eq!(s.auto_away.frames, AUTO_AWAY_FRAMES);
    assert_eq!(s.auto_away_tick(Duration::from_secs(3600), &mut || true), Some(1));
    assert_eq!(trace(&[838]), ["206()", "206()", "376(-2,-2,1,1,0,-2)", "377(5)"]);
    assert_eq!(s.auto_away.frames, 0);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn launcher_activity_resets_the_idle_clock() {
    note_activity();
    assert!(idle_time() < Duration::from_secs(5));
}

#[test]
fn connection_loss_is_a_local_sign_out_without_close_or_relogin() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.home = true;
    set_linked(3);
    // The owner frame peeks the latched pump error once per frame (101984da).
    script(1103, &[-1025]);
    assert!(!owner_frame(&s.api));
    assert_eq!(trace(&[]), ["818()", "1103()"]);
    clear_calls();
    let error = s.connection_lost(-1025);
    assert!(error.contains("-1025"), "{error}");
    assert_eq!(trace(&[]), ["463()", "434()"]);
    assert!(!s.signed_in);
    clear_calls();
    drop(s);
    let calls = trace(&[]);
    assert!(!calls.iter().any(|c| c.starts_with("816(") || c.starts_with("817(") || c.starts_with("815(")));
    assert_eq!(calls, ["1502(0,0,0,0)"]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_out_closes_like_the_viewer_without_pumping() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.home = true;
    s.registered = true;
    s.files = true;
    script(817, &[1, 1, 7]);
    let started = Instant::now();
    drop(s);
    assert!(started.elapsed() >= Duration::from_millis(200), "817 polls are >= 100 ms apart");
    // 838 only decides whether final pushes can still be answered.
    assert_eq!(trace(&[838]), [
        "816(1)", "817()", "817()", "817()", "463()", "434()", "1502(0,0,0,0)",
        "828()", "824(0,0,0)", "819(0)", "872()", "1149(0,0)", "920()", "915()",
    ]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn concurrent_loads_retry_a_full_table_and_cancel_only_their_own_tickets() {
    let viewer = fixture();
    let s = session(&viewer);
    set_linked(3);
    script(171, &[SLOTS_FULL]);
    script(164, &[0, 0]);
    let mut frames = 0;
    let results = run_concurrent(&s.api, &LISTS[..2], &mut || { frames += 1; true });
    assert_eq!(results, [1, 1]);
    assert_eq!(trace(&[838]), ["163()", "171()", "164(1)", "171()", "164(1)", "172(2)", "164(1)"]);
    assert_eq!(frames, 3);
    clear_calls();
    let results = run_concurrent(&s.api, &LISTS[..2], &mut || false);
    assert_eq!(results, [native_jobs::ABORTED, native_jobs::ABORTED]);
    assert_eq!(trace(&[838]), ["163()", "171()", "337(1)", "337(2)"]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- No requests during play; no manual refresh (B1, C6) --------------------

#[test]
fn friend_observer_only_reads_locked_getters() {
    let (stop, stopping) = mpsc::channel();
    let (done, calls) = mpsc::channel();
    let table = fake_table();
    let thread = std::thread::spawn(move || {
        set_linked(3);
        run_friend_observer(Api::test_table(&table), stopping, |_| {});
        let _ = done.send(raw_calls());
    });
    std::thread::sleep(Duration::from_millis(700));
    refuse_friends_refresh();
    stop.send(()).unwrap();
    thread.join().unwrap();
    let calls = calls.recv().unwrap();
    assert!(calls.iter().any(|(s, _)| *s == 167));
    assert!(calls.iter().all(|(s, _)| [838, 206, 207, 167, 169].contains(s)), "{:?}", calls.iter().map(|c| c.0).collect::<Vec<_>>());
}

#[test]
fn native_controls_cannot_sign_out_under_a_game_or_queue_duplicate_starts() {
    let control = AtomicU8::new(GAME);
    assert_eq!(accept_control(&control, SIGN_OUT), -256);
    assert_eq!(accept_control(&control, START_GAME), -256);
    assert_eq!(accept_control(&control, VISIBLE | GROUPS), -256, "no status request during play");
    assert_eq!(control.load(Ordering::Acquire), GAME);
    control.store(SOCIAL, Ordering::Release);
    assert_eq!(accept_control(&control, START_GAME), 0);
    assert_eq!(accept_control(&control, START_GAME), -256);
    assert_eq!(accept_control(&control, SIGN_OUT), -256);
    control.store(SOCIAL, Ordering::Release);
    assert_eq!(accept_control(&control, SIGN_OUT), 0);
    assert_eq!(accept_control(&control, START_GAME), -256);
}

static OBSERVER_CONNECTED: AtomicBool = AtomicBool::new(true);
unsafe extern "C" fn observer_connection() -> i32 {
    if OBSERVER_CONNECTED.load(Ordering::Acquire) { 3 } else { 0 }
}
unsafe extern "C" fn observer_friend(index: usize, output: usize) {
    if index == 0 {
        let record = std::slice::from_raw_parts_mut(output as *mut u8, proto::FRIEND_RECORD_LEN);
        record[0x98] = 1;
        record[8..16].copy_from_slice(&((1u64 << 11) | (1u64 << 33)).to_le_bytes());
        record[0x10..0x18].copy_from_slice(&123u64.to_le_bytes());
    }
}
unsafe extern "C" fn observer_empty(_: usize, _: usize) {}
unsafe extern "C" fn observer_presence() -> i32 { 1 }

#[test]
fn observer_republishes_unchanged_native_friends_after_connection_returns() {
    OBSERVER_CONNECTED.store(true, Ordering::Release);
    let mut table = vec![0; 1560];
    table[838] = observer_connection as *const () as usize;
    table[167] = observer_friend as *const () as usize;
    table[169] = observer_empty as *const () as usize;
    table[206] = observer_presence as *const () as usize;
    table[207] = observer_presence as *const () as usize;
    // The test leaves all request, pump and notification slots unavailable.
    let (stop, stopping) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let observer = FriendObserver {
        stop,
        thread: Some(std::thread::spawn(move || {
            run_friend_observer(Api(table.into()), stopping, |line| { let _ = tx.send(line); });
        })),
    };
    let receive = || rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":0}\n");
    assert_eq!(receive(), "{\"type\":\"native_own_presence\",\"mode\":1}\n");
    let initial = receive();
    assert!(initial.starts_with("{\"type\":\"native_friends\",\"payload\":\""));

    OBSERVER_CONNECTED.store(false, Ordering::Release);
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":-1025}\n");
    OBSERVER_CONNECTED.store(true, Ordering::Release);
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":0}\n");
    assert_eq!(receive(), "{\"type\":\"native_own_presence\",\"mode\":1}\n");
    // Without the fix this second snapshot is never emitted: the bytes match.
    assert_eq!(receive(), initial);
    drop(observer); // Same stop-and-join path as the live host.
}

// --- Unchanged local behaviour ---------------------------------------------

#[test]
fn authentic_success_requires_the_expected_link() {
    assert_eq!(connection_result(27, || 0, 3), None);
    assert_eq!(connection_result(31, || 0, 3), None, "31..38 are transitional");
    assert_eq!(connection_result(39, || 0, 3), None);
    assert_eq!(connection_result(30, || 3, 3), Some(Ok(())));
    assert_eq!(connection_result(30, || 2, 2), Some(Ok(())));
    assert_eq!(connection_result(30, || 2, 3), Some(Err(())));
    assert_eq!(connection_result(-1025, || 3, 3), Some(Err(())));
}

#[test]
fn notification_texts_lose_carriage_returns_like_the_viewer() {
    assert_eq!(viewer_text(1302, b"a\r\nb\r".to_vec()), b"a\nb");
    assert_eq!(viewer_text(1072, b"a\r\nb".to_vec()), b"a\r\nb");
}

#[test]
fn table_rejects_foreign_entries_before_any_function_call() {
    let mut table = vec![0; 1560];
    let base = 0x10000000;
    for &(slot, rva) in ENTRY_RVAS.iter().chain(crate::native_social::ENTRY_RVAS).chain(crate::native_jobs::ENTRY_RVAS) {
        table[slot] = base + rva;
    }
    assert!(validate_entries(&table, base).is_ok());
    for slot in [1502, 163, 164, 199, 1074, 1075, 824, 871, 891, 1103, 376, 404, 816] {
        table[slot] += 1;
        assert!(validate_entries(&table, base).is_err());
        table[slot] -= 1;
    }
    assert!(validate_entries(&table, usize::MAX).is_err());
}

#[test]
fn account_stores_never_alias_or_adopt_foreign_folders() {
    let root = std::env::temp_dir().join(format!(
        "crystal-pol-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("pub/home50/msg")).unwrap();
    fs::create_dir_all(root.join("usr/home51")).unwrap();
    fs::write(root.join("pub/home50/msg/official"), b"preserve").unwrap();
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    assert_eq!(claim_home(&root, &a).unwrap(), 52);
    assert_eq!(claim_home(&root, &b).unwrap(), 53);
    assert_eq!(claim_home(&root, &a).unwrap(), 52);
    assert!(!root.join("usr/home51/.crystal-profile").exists());
    assert_eq!(
        fs::read(root.join("pub/home50/msg/official")).unwrap(),
        b"preserve"
    );
    assert!(claim_home(&root, "../home50").is_err());
    // Test-owned absolute temporary root only.
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn localization_refuses_truncated_and_oversized_data() {
    assert!(decode_pex(b"PEX\0").is_none());
    let mut raw = vec![0; 16];
    raw[..4].copy_from_slice(b"PEX\0");
    raw[4..8].copy_from_slice(&2_000_001u32.to_le_bytes());
    assert!(decode_pex(&raw).is_none());
    let mut decoded = vec![0; 16];
    decoded.extend_from_slice(&10u16.to_le_bytes());
    decoded.extend_from_slice(&0x46c9u16.to_le_bytes());
    decoded.extend_from_slice(&[b'H', 0, b'i', 0, 0, 0]);
    assert_eq!(resource_text(&decoded, 0x46c9).unwrap(), b"Hi");
    assert!(resource_text(&decoded[..decoded.len() - 1], 0x46c9).is_none());
}
