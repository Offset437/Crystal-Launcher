//! Optional navigation of the real FFXI login UI. Game memory is read only.
//! Input is limited to this GameStart's foreground FFXiClass window. An unknown
//! build, menu, missing character, changed selection or timeout leaves control
//! with the user. No game functions, packets or menu-memory writes are used.

use std::{ffi::c_void, sync::mpsc, thread, time::{Duration, Instant}};
use windows_sys::Win32::{
    Foundation::HWND,
    System::{Diagnostics::Debug::ReadProcessMemory, Threading::GetCurrentProcess},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, IsWindowEnabled, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD,
            KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE},
        WindowsAndMessaging::{GetClassNameW, GetForegroundWindow,
            GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible},
    },
};
use crate::{log, proto::AutoLogin};

const POLL: Duration = Duration::from_millis(100);
const SETTLE: Duration = Duration::from_millis(250);
const LIMIT: Duration = Duration::from_secs(180);
const FOCUS_RVA: usize = 0x0062_188C;
const LICENSE_RVA: usize = 0x0066_9000;
const TITLE_RVA: usize = 0x0066_9050;
const CHARACTERS_RVA: usize = 0x0066_9080;
const HELP_RVA: usize = 0x0066_9078;
const CONFIRM_RVA: usize = 0x0066_9004;
const SELECTED_RVA: usize = 0x0066_908C;
const WINDOW_RVA: usize = 0x0045_69A8;

fn read<T: Copy>(address: usize) -> Option<T> {
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes = 0;
    // Windows rejects inaccessible ranges; no Rust game-memory dereference.
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const c_void,
        out.as_mut_ptr().cast(), std::mem::size_of::<T>(), &mut bytes) };
    (ok != 0 && bytes == std::mem::size_of::<T>()).then(|| unsafe { out.assume_init() })
}

fn pointer(address: usize) -> Option<usize> {
    read::<u32>(address).map(|value| value as usize)
        .filter(|value| *value >= 0x10000 && *value <= usize::MAX - 0x20000 && value % 4 == 0)
}

fn operand_matches(base: usize, code: &[u8], prefix: &[u8], rva: usize, suffix: &[u8]) -> bool {
    let Some(address) = base.checked_add(rva).and_then(|n| u32::try_from(n).ok()) else { return false; };
    code.len() == prefix.len() + 4 + suffix.len()
        && code.starts_with(prefix) && code.ends_with(suffix)
        && code[prefix.len()..prefix.len() + 4] == address.to_le_bytes()
}

fn verified_layout(base: usize) -> bool {
    // Installed instructions independently inspected at 1015fdf0 (focused UI),
    // 10241842 (one-based UI row -> selected index), 10243699 (login help mode)
    // and 10243bba (existing character -> login confirmation 13). Relocation is
    // checked too. Deletion reuses dbnamese but sets help mode 0x12, not 0x11.
    let Some(focus) = read::<[u8; 8]>(base + 0x15FDF0) else { return false; };
    let Some(row) = read::<[u8; 10]>(base + 0x241842) else { return false; };
    let Some(help) = read::<[u8; 13]>(base + 0x243699) else { return false; };
    let Some(window) = read::<[u8; 6]>(base + 0x015A57) else { return false; };
    operand_matches(base, &focus, &[0x3B, 0x05], FOCUS_RVA, &[0x75, 0x09])
        && operand_matches(base, &row, &[0x0F, 0xBF, 0x40, 0x4C, 0x48, 0xA3], SELECTED_RVA, &[])
        && operand_matches(base, &help, &[0x8B, 0x0D], HELP_RVA, &[0xC7, 0x41, 0x14, 2, 0, 0, 0])
        && operand_matches(base, &window, &[0x8B, 0x0D], WINDOW_RVA, &[])
        && read::<[u8; 8]>(base + 0x243BBA) == Some([0x83, 0xC0, 0x44, 0x53, 0x50, 0x53, 0x6A, 0x0D])
        && read::<[u8; 7]>(base + 0x243C18) == Some([0xC7, 0x42, 0x2C, 0x11, 0, 0, 0])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen { Warning, Title, Characters, Confirm, Other }

fn screen(name: &[u8; 16]) -> Screen {
    match name {
        b"menu    ptc8lice" => Screen::Warning,
        b"menu    loby2win" => Screen::Title,
        b"menu    dbnamese" => Screen::Characters,
        b"menu    ptc6yesn" => Screen::Confirm,
        _ => Screen::Other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    screen: Screen,
    focus: usize,
    cursor: i32,
    character_count: i32,
    target_available: bool,
    login_context: bool,
    selected: i32,
    login_confirmation: bool,
}

fn snapshot(base: usize, target: u8) -> Option<Snapshot> {
    let focus = pointer(base + FOCUS_RVA)?;
    let resource = pointer(focus + 4)?;
    let name = read::<[u8; 16]>(resource + 0x46)?;
    let screen = screen(&name);
    let controller_rva = match screen {
        Screen::Warning => LICENSE_RVA,
        Screen::Title => TITLE_RVA,
        Screen::Characters => CHARACTERS_RVA,
        Screen::Confirm => CONFIRM_RVA,
        Screen::Other => 0,
    };
    let mut state = Snapshot { screen, focus, cursor: -1, character_count: 0,
        target_available: false, login_context: false, selected: -1, login_confirmation: false };
    if controller_rva != 0 {
        let controller = pointer(base + controller_rva)?;
        // The focused menu must be this controller's live UI, not a background
        // character list behind a modal or a reused, stale controller.
        if pointer(controller + 8)? != focus || read::<u8>(focus + 0x5D)? != 0 { return None; }
        state.cursor = i32::from(read::<i16>(focus + 0x4C)?) - 1;
        if matches!(screen, Screen::Characters | Screen::Confirm) {
            let help = pointer(base + HELP_RVA)?;
            let characters = pointer(base + CHARACTERS_RVA)?;
            let lobby = pointer(base + crate::lobby::LOBBY_PTR_RVA)?;
            state.login_context = read::<u32>(help + 0x2C)? == 0x11;
            // 100efca0 reads 0x8c-byte lobby character entries at +0x13824;
            // their count immediately precedes them. Never read an absent row.
            state.character_count = read::<i32>(lobby + 0x13820)?;
            if (1..=16).contains(&state.character_count) && i32::from(target) < state.character_count {
                let row = characters + usize::from(target) * 0xB0;
                state.target_available = read::<u32>(row + 0x68)? != 0
                    && read::<u32>(row + 0x98)? == 0;
            }
            state.selected = read::<i32>(base + SELECTED_RVA)?;
            if screen == Screen::Confirm {
                // The generic yes/no menu also serves delete/restore/rename.
                // Only the normal Play confirmation (13, ordinary variant) is safe.
                state.login_confirmation = read::<u32>(controller + 0x18)? == 13
                    && read::<u8>(controller + 0xA5E0)? == 0;
                state.cursor = read::<i32>(controller + 0x14)?;
            }
        }
    }
    // Reject a menu switch during this multi-field read.
    (pointer(base + FOCUS_RVA)? == focus && read::<[u8; 16]>(resource + 0x46)? == name).then_some(state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key { Enter, Up, Down }
impl Key {
    fn scan(self) -> u16 { match self { Self::Enter => 0x1C, Self::Up => 0x48, Self::Down => 0x50 } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action { Wait, Tap(Key), Finished, Stop(&'static str) }

#[derive(Clone, Copy, Debug)]
struct Pending { before: Snapshot, key: Key, sent: Duration }

struct Navigator {
    target: u8,
    pending: Option<Pending>,
    selected: bool,
    confirmed: bool,
    taps: u8,
}

impl Navigator {
    fn new(slot: u8) -> Self { Self { target: slot - 1, pending: None, selected: false, confirmed: false, taps: 0 } }

    fn action(&mut self, state: Snapshot, now: Duration) -> Action {
        if self.taps >= 24 { return Action::Stop("input limit reached; finish login manually"); }
        if let Some(pending) = self.pending {
            if state.screen == pending.before.screen && state.focus == pending.before.focus {
                if matches!(pending.key, Key::Up | Key::Down) {
                    let expected = pending.before.cursor + if pending.key == Key::Up { -1 } else { 1 };
                    if state.cursor == expected { self.pending = None; }
                    else if state.cursor != pending.before.cursor { return Action::Stop("selection changed unexpectedly"); }
                    else if now.saturating_sub(pending.sent) > Duration::from_secs(3) { return Action::Stop("game did not accept navigation input"); }
                    else { return Action::Wait; }
                } else if now.saturating_sub(pending.sent) > Duration::from_secs(60) {
                    return Action::Stop("game did not advance from the login menu");
                } else { return Action::Wait; }
            } else {
                self.pending = None;
                match (pending.before.screen, state.screen) {
                    (Screen::Warning, Screen::Title | Screen::Characters)
                    | (Screen::Title, Screen::Characters)
                    | (Screen::Characters, Screen::Confirm) => {}
                    (Screen::Confirm, _) if self.confirmed => return Action::Finished,
                    _ => return Action::Stop("unexpected login screen; finish login manually"),
                }
            }
        }
        match state.screen {
            Screen::Warning | Screen::Title if !self.selected => {
                if state.cursor == 0 { Action::Tap(Key::Enter) }
                else { Action::Stop("login menu selection was changed; finish login manually") }
            }
            Screen::Characters if !self.selected => {
                if !state.login_context { return Action::Stop("character menu is not the Play screen"); }
                if !(1..=16).contains(&state.character_count) || i32::from(self.target) >= state.character_count || !state.target_available {
                    return Action::Stop("chosen character slot is empty or unavailable");
                }
                if state.cursor < 0 || state.cursor >= state.character_count { return Action::Stop("character cursor is outside the list"); }
                match state.cursor.cmp(&i32::from(self.target)) {
                    std::cmp::Ordering::Less => Action::Tap(Key::Down),
                    std::cmp::Ordering::Greater => Action::Tap(Key::Up),
                    std::cmp::Ordering::Equal => Action::Tap(Key::Enter),
                }
            }
            Screen::Confirm if self.selected && !self.confirmed => {
                if !state.login_context || !state.login_confirmation || state.selected != i32::from(self.target) || !state.target_available {
                    return Action::Stop("confirmation does not match the chosen character login");
                }
                match state.cursor { 0 => Action::Tap(Key::Enter), 1 => Action::Tap(Key::Up),
                    _ => Action::Stop("unexpected confirmation selection") }
            }
            _ => Action::Stop("unexpected login screen; finish login manually"),
        }
    }

    fn sent(&mut self, state: Snapshot, key: Key, now: Duration) {
        self.taps += 1;
        if key == Key::Enter {
            if state.screen == Screen::Characters { self.selected = true; }
            if state.screen == Screen::Confirm { self.confirmed = true; }
        }
        self.pending = Some(Pending { before: state, key, sent: now });
    }
}

fn game_window(base: usize) -> Option<(HWND, u32)> {
    // GameMain may create its UI on a different thread from GameStart. Read its
    // actual HWND (also passed to DestroyWindow at 10015a57), then validate it.
    let window = read::<u32>(base + WINDOW_RVA)? as usize as HWND;
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid == std::process::id() && owned_window(window, thread)).then_some((window, thread))
}

fn owned_window(window: HWND, game_thread: u32) -> bool {
    if window.is_null() { return false; }
    let mut pid = 0;
    let mut class = [0u16; 32];
    // No title/name matching: game thread, host process and the game class.
    unsafe {
        IsWindow(window) != 0 && GetWindowThreadProcessId(window, &mut pid) == game_thread
            && pid == std::process::id()
            && GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) == 9
            && class[..9] == [70, 70, 88, 105, 67, 108, 97, 115, 115]
    }
}

fn active(window: HWND, game_thread: u32) -> bool {
    owned_window(window, game_thread) && unsafe {
        GetForegroundWindow() == window && IsWindowVisible(window) != 0
            && IsWindowEnabled(window) != 0 && IsIconic(window) == 0
    }
}

fn keys_down() -> bool {
    // Never combine an automated key with held modifiers, manual navigation,
    // confirmation, Escape or mouse buttons. It can resume once they are up.
    [1, 2, 0x0D, 0x10, 0x11, 0x12, 0x1B, 0x25, 0x26, 0x27, 0x28, 0x5B, 0x5C]
        .iter().any(|key| unsafe { GetAsyncKeyState(*key) } < 0)
}

fn input(key: Key, release: bool) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
        wVk: 0, wScan: key.scan(), dwFlags: KEYEVENTF_SCANCODE
            | if key != Key::Enter { KEYEVENTF_EXTENDEDKEY } else { 0 }
            | if release { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0,
    } } }
}

fn send(key: Key, release: bool) -> bool {
    let input = input(key, release);
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1 }
}

struct Pressed(Key);
impl Drop for Pressed { fn drop(&mut self) { let _ = send(self.0, true); } }

#[derive(Debug, PartialEq, Eq)]
enum Tap { Sent, Skipped, Cancelled, Failed }

fn hold_until_release(stop: &mpsc::Receiver<()>, mut unchanged: impl FnMut() -> bool) -> bool {
    for _ in 0..7 {
        if stop.recv_timeout(Duration::from_millis(10)) != Err(mpsc::RecvTimeoutError::Timeout) {
            return false;
        }
        if !unchanged() { break; }
    }
    true
}

fn tap(window: HWND, game_thread: u32, base: usize, target: u8, expected: Snapshot, key: Key, stop: &mpsc::Receiver<()>) -> Tap {
    // The final menu/focus check is immediately adjacent to key-down. No
    // SetForegroundWindow, AttachThreadInput, process-wide hooks or queued
    // background input. Key-up is always paired, even on focus loss/cancel.
    if stop.try_recv() != Err(mpsc::TryRecvError::Empty) { return Tap::Cancelled; }
    if keys_down() || snapshot(base, target) != Some(expected)
        || game_window(base) != Some((window, game_thread)) || !active(window, game_thread) { return Tap::Skipped; }
    if !send(key, false) { return Tap::Failed; }
    let pressed = Pressed(key);
    let completed = hold_until_release(stop, || active(window, game_thread) && snapshot(base, target) == Some(expected));
    drop(pressed);
    if completed { Tap::Sent } else { Tap::Cancelled }
}

pub(crate) struct Worker { stop: mpsc::Sender<()>, worker: Option<thread::JoinHandle<()>> }
impl Worker {
    pub(crate) fn start(config: AutoLogin) -> Option<Self> {
        if !config.enabled || !(1..=16).contains(&config.character_slot) { return None; }
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new().name("crystal-auto-login".into()).spawn(move || {
            let started = Instant::now();
            let mut base = None;
            let mut stable = None;
            let mut changed = Instant::now();
            let mut navigator = Navigator::new(config.character_slot);
            log!("auto-login: waiting for this game's foreground login screen (slot {})", config.character_slot);
            while started.elapsed() < LIMIT {
                if receiver.recv_timeout(POLL) != Err(mpsc::RecvTimeoutError::Timeout) { return; }
                if crate::events::pipe_closed() { return; }
                if base.is_none() {
                    match crate::lobby::verified_image() {
                        Ok(Some(image)) if verified_layout(image) => base = Some(image),
                        Ok(None) => continue,
                        _ => { log!("auto-login: unsupported game layout; finish login manually"); return; }
                    }
                }
                let base = base.unwrap();
                let Some((window, game_thread)) = game_window(base) else { stable = None; continue; };
                if !active(window, game_thread) || keys_down() { stable = None; continue; }
                let current = snapshot(base, navigator.target);
                if current != stable { stable = current; changed = Instant::now(); continue; }
                if changed.elapsed() < SETTLE { continue; }
                let Some(current) = current else { continue; };
                match navigator.action(current, started.elapsed()) {
                    Action::Wait => {},
                    Action::Finished => { log!("auto-login: character login confirmed; input finished"); return; }
                    Action::Stop(reason) => { log!("auto-login: {reason}"); return; }
                    Action::Tap(key) => {
                        match tap(window, game_thread, base, navigator.target, current, key, &receiver) {
                            Tap::Sent => {
                                navigator.sent(current, key, started.elapsed());
                                if navigator.confirmed {
                                    log!("auto-login: login confirmation submitted; input finished");
                                    return;
                                }
                            },
                            Tap::Skipped => {},
                            Tap::Cancelled => return,
                            Tap::Failed => { log!("auto-login: Windows rejected input; finish login manually"); return; }
                        }
                        stable = None;
                    }
                }
            }
            log!("auto-login: timed out; finish login manually");
        }).ok()?;
        Some(Self { stop, worker: Some(worker) })
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only owned synthetic bytes are inspected; these tests never load FFXI or
    // call SendInput. Distinct menu/controller pointers exercise the real reader.
    struct Fixture(Vec<u8>);
    impl Fixture {
        fn new() -> Self {
            let mut fixture = Self(vec![0; 0x66_9100]);
            fixture.ptr(FOCUS_RVA, 0x1000);
            fixture.ptr(0x1004, 0x2000);
            for (global, local) in [(LICENSE_RVA, 0x3000), (TITLE_RVA, 0x4000),
                (CHARACTERS_RVA, 0x5000), (HELP_RVA, 0x7000), (CONFIRM_RVA, 0x8000),
                (crate::lobby::LOBBY_PTR_RVA, 0x20000)] {
                fixture.ptr(global, local);
                fixture.ptr(local + 8, 0x1000);
            }
            fixture.u32(0x33820, 3);
            fixture.u32(0x7000 + 0x2C, 0x11);
            fixture.u32(0x5000 + 0xB0 + 0x68, 1);
            fixture.u32(SELECTED_RVA, 1);
            fixture.u32(0x8000 + 0x18, 13);
            fixture.menu(b"menu    dbnamese", 2);
            fixture
        }
        fn base(&self) -> usize { self.0.as_ptr() as usize }
        fn u32(&mut self, offset: usize, value: u32) { self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes()); }
        fn ptr(&mut self, offset: usize, local: usize) { self.u32(offset, (self.base() + local).try_into().unwrap()); }
        fn menu(&mut self, name: &[u8; 16], cursor: i16) {
            self.0[0x2046..0x2056].copy_from_slice(name);
            self.0[0x104C..0x104E].copy_from_slice(&cursor.to_le_bytes());
        }
        fn snapshot(&self) -> Option<Snapshot> { snapshot(self.base(), 1) }
    }

    fn state(screen: Screen, cursor: i32) -> Snapshot {
        Snapshot { screen, focus: 123, cursor, character_count: 3, target_available: true,
            login_context: true, selected: 1, login_confirmation: true }
    }
    #[test]
    fn selects_requested_existing_row_and_only_its_login_confirmation() {
        let mut nav = Navigator::new(2);
        let first = state(Screen::Characters, 0);
        assert_eq!(nav.action(first, Duration::ZERO), Action::Tap(Key::Down));
        nav.sent(first, Key::Down, Duration::ZERO);
        assert_eq!(nav.action(first, Duration::from_secs(1)), Action::Wait);
        let target = state(Screen::Characters, 1);
        assert_eq!(nav.action(target, Duration::from_secs(1)), Action::Tap(Key::Enter));
        nav.sent(target, Key::Enter, Duration::from_secs(1));
        let confirm = Snapshot { focus: 456, ..state(Screen::Confirm, 1) };
        assert_eq!(nav.action(confirm, Duration::from_secs(2)), Action::Tap(Key::Up));
        nav.sent(confirm, Key::Up, Duration::from_secs(2));
        let yes = Snapshot { cursor: 0, ..confirm };
        assert_eq!(nav.action(yes, Duration::from_secs(3)), Action::Tap(Key::Enter));
        nav.sent(yes, Key::Enter, Duration::from_secs(3));
        assert_eq!(nav.action(state(Screen::Other, 0), Duration::from_secs(4)), Action::Finished);
    }
    #[test]
    fn refuses_delete_menu_generic_confirmation_empty_slot_and_changed_cursor() {
        for invalid in [Snapshot { login_context: false, ..state(Screen::Characters, 0) },
            Snapshot { target_available: false, ..state(Screen::Characters, 0) },
            Snapshot { character_count: 1, ..state(Screen::Characters, 0) },
            state(Screen::Confirm, 0), state(Screen::Title, 1), state(Screen::Other, 0)] {
            assert!(matches!(Navigator::new(2).action(invalid, Duration::ZERO), Action::Stop(_)));
        }
        let mut nav = Navigator::new(2);
        nav.sent(state(Screen::Characters, 1), Key::Enter, Duration::ZERO);
        let wrong = Snapshot { focus: 456, login_confirmation: false, ..state(Screen::Confirm, 0) };
        assert!(matches!(nav.action(wrong, Duration::from_secs(1)), Action::Stop(_)));
        let mut nav = Navigator::new(2);
        nav.sent(state(Screen::Characters, 0), Key::Down, Duration::ZERO);
        assert!(matches!(nav.action(state(Screen::Characters, 2), Duration::from_secs(1)), Action::Stop(_)));
    }
    #[test]
    fn never_repeats_unacknowledged_input_and_stops_on_timeout() {
        let mut nav = Navigator::new(1);
        let title = state(Screen::Title, 0);
        nav.sent(title, Key::Enter, Duration::ZERO);
        assert_eq!(nav.action(title, Duration::from_secs(10)), Action::Wait);
        assert!(matches!(nav.action(title, Duration::from_secs(61)), Action::Stop(_)));
        let mut nav = Navigator::new(2);
        let row = state(Screen::Characters, 0);
        nav.sent(row, Key::Down, Duration::ZERO);
        assert!(matches!(nav.action(row, Duration::from_secs(4)), Action::Stop(_)));
    }
    #[test]
    fn signatures_include_relocated_operands_and_input_is_scancode_paired() {
        for base in [0x1000_0000usize, 0x1800_0000] {
            let mut bytes = vec![0x3B, 0x05];
            bytes.extend_from_slice(&((base + FOCUS_RVA) as u32).to_le_bytes());
            bytes.extend_from_slice(&[0x75, 0x09]);
            assert!(operand_matches(base, &bytes, &[0x3B, 0x05], FOCUS_RVA, &[0x75, 0x09]));
            assert!(!operand_matches(base + 4, &bytes, &[0x3B, 0x05], FOCUS_RVA, &[0x75, 0x09]));
        }
        for key in [Key::Enter, Key::Up, Key::Down] {
            let down = unsafe { input(key, false).Anonymous.ki };
            let up = unsafe { input(key, true).Anonymous.ki };
            assert_eq!(down.wScan, up.wScan);
            assert_eq!(down.wVk, 0);
            assert_eq!(up.dwFlags, down.dwFlags | KEYEVENTF_KEYUP);
        }
        assert!(!owned_window(std::ptr::null_mut(), 0));
        assert_eq!(read::<u32>(0), None);
    }

    #[test]
    fn cancellation_during_a_held_key_is_not_consumed_and_lost() {
        let (send, receive) = mpsc::channel();
        send.send(()).unwrap();
        assert!(!hold_until_release(&receive, || panic!("cancel must win")));
        drop(send);
        assert!(!hold_until_release(&receive, || panic!("disconnect must win")));
        let (_send, receive) = mpsc::channel();
        let mut checks = 0;
        assert!(hold_until_release(&receive, || { checks += 1; false }));
        assert_eq!(checks, 1);
    }

    #[test]
    fn real_reader_requires_focused_controller_login_context_and_occupied_slot() {
        let mut fixture = Fixture::new();
        let ready = fixture.snapshot().unwrap();
        assert_eq!(ready.cursor, 1);
        assert_eq!(ready.character_count, 3);
        assert_eq!(Navigator::new(2).action(ready, Duration::ZERO), Action::Tap(Key::Enter));
        fixture.u32(0x7000 + 0x2C, 0x12);
        assert!(matches!(Navigator::new(2).action(fixture.snapshot().unwrap(), Duration::ZERO), Action::Stop(_)));
        fixture.u32(0x7000 + 0x2C, 0x11);
        fixture.u32(0x5000 + 0xB0 + 0x68, 0);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.u32(0x5000 + 0xB0 + 0x68, 1);
        fixture.u32(0x5000 + 0xB0 + 0x98, 1);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.u32(0x33820, 1);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.ptr(0x5008, 0x1100);
        assert_eq!(fixture.snapshot(), None, "background list is never navigated");
        fixture.ptr(0x5008, 0x1000);
        fixture.0[0x105D] = 1;
        assert_eq!(fixture.snapshot(), None, "disabled menus are never navigated");
    }

    #[test]
    fn real_reader_identifies_only_the_ordinary_login_confirmation() {
        let mut fixture = Fixture::new();
        fixture.menu(b"menu    ptc6yesn", 1);
        fixture.u32(0x8000 + 0x14, 1);
        let confirm = fixture.snapshot().unwrap();
        assert_eq!(confirm.screen, Screen::Confirm);
        assert_eq!(confirm.cursor, 1);
        assert!(confirm.login_confirmation && confirm.target_available);
        fixture.u32(0x8000 + 0x18, 14);
        assert!(!fixture.snapshot().unwrap().login_confirmation);
        fixture.u32(0x8000 + 0x18, 13);
        fixture.0[0x8000 + 0xA5E0] = 1;
        assert!(!fixture.snapshot().unwrap().login_confirmation);
        fixture.u32(FOCUS_RVA, u32::MAX);
        assert_eq!(fixture.snapshot(), None, "invalid pointers fail closed without overflow");
    }
}
