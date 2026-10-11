//! crystal-loader: a 32-bit host that authenticates through the installed
//! PlayOnline core and starts retail FINAL FANTASY XI. Crystal runs it directly
//! for Vanilla or through Ashita v4 and Windower 4.
//!
//! Order matters for the injectors: nothing game-related is loaded until `run`
//! has the session inputs; polcore, FFXi.dll and FFXiMain are then loaded at run time
//! through COM / LoadLibraryExW, in this process, on the game thread.

pub mod args;
mod auto_login;
pub mod b64;
pub mod events;
pub mod ffxi_settings;
mod friend_locations;
pub mod game;
pub mod json;
pub mod lobby;
pub mod native_auth;
mod native_jobs;
mod native_profile;
mod native_social;
mod start_gate;
#[cfg(any(test, feature = "native-backup-diagnostics"))]
mod native_backup;
#[cfg(feature = "native-login-diagnostics")]
mod native_socket_trace;
#[macro_use]
pub mod log;
pub mod pipe;
pub mod proto;
pub mod registry;
pub mod slots;
pub mod trace;
#[cfg(target_arch = "x86")]
mod windower_exit;

use std::time::Duration;

use pipe::{LineReader, Pipe};

/// Fixed-name startup markers only. QPC uses the same host clock as the
/// launcher's `launch_timing::MonotonicStamp`; it never changes POL's clock.
#[cfg(feature = "native-login-diagnostics")]
pub(crate) mod startup_timing {
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(value: *mut i64) -> i32;
        fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    }

    struct LastError(u32);
    impl LastError {
        fn save() -> Self { Self(unsafe { GetLastError() }) }
    }
    impl Drop for LastError {
        fn drop(&mut self) { unsafe { SetLastError(self.0) }; }
    }

    fn host_micros(ticks: i64, frequency: i64) -> Option<u64> {
        if ticks < 0 || frequency <= 0 { return None; }
        u64::try_from(ticks as u128 * 1_000_000 / frequency as u128).ok()
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Stamp { instant: Instant, host_us: Option<u64> }
    impl Stamp {
        pub(crate) fn capture() -> Self {
            let _error = LastError::save();
            let instant = Instant::now();
            let (mut ticks, mut frequency) = (0i64, 0i64);
            // SAFETY: both APIs write to live, correctly sized local integers.
            let valid = unsafe { QueryPerformanceCounter(&mut ticks) } != 0
                && unsafe { QueryPerformanceFrequency(&mut frequency) } != 0;
            Self { instant, host_us: valid.then(|| host_micros(ticks, frequency)).flatten() }
        }
    }

    pub(crate) fn report(stage: &'static str, start: Stamp, end: Stamp) {
        let _error = LastError::save();
        crate::log!("native startup timing: stage={stage} start_host_us={} end_host_us={} host_clock_valid={} elapsed_us={}",
            start.host_us.unwrap_or(0), end.host_us.unwrap_or(0), start.host_us.is_some() && end.host_us.is_some(),
            end.instant.saturating_duration_since(start.instant).as_micros());
    }

    pub(crate) fn mark(stage: &'static str) {
        let at = Stamp::capture();
        report(stage, at, at);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn host_clock_conversion_uses_microseconds_and_rejects_invalid_values() {
            assert_eq!(host_micros(15, 10), Some(1_500_000));
            assert_eq!(host_micros(1, 3), Some(333_333));
            assert_eq!(host_micros(0, 10), Some(0));
            for (ticks, frequency) in [(-1, 10), (1, 0), (1, -1), (i64::MAX, 1)] {
                assert_eq!(host_micros(ticks, frequency), None);
            }
        }

        #[test]
        fn stamps_are_monotonic_and_diagnostics_preserve_last_error() {
            let _error = LastError::save();
            unsafe { SetLastError(12345) };
            let start = Stamp::capture();
            assert_eq!(unsafe { GetLastError() }, 12345);
            let end = Stamp::capture();
            assert!(end.instant >= start.instant);
            assert!(end.host_us.unwrap() >= start.host_us.unwrap());
            report("offline-clock-test", start, end);
            assert_eq!(unsafe { GetLastError() }, 12345);
        }
    }
}

pub const EXIT_ARGS: i32 = 2;
pub const EXIT_PIPE: i32 = 3;
pub const EXIT_PROTOCOL: i32 = 4;
pub const EXIT_GAME_START: i32 = 5;
pub const EXIT_SELFTEST_WATCHDOG: i32 = 9;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
const SELFTEST_WATCHDOG: Duration = Duration::from_secs(120);
const GAME_THREAD_STACK: usize = 4 << 20;

/// The whole loader. Returns the process exit code.
pub fn run() -> i32 {
    let log_path = log::init();
    log!("crystal-loader {} starting{}", env!("CARGO_PKG_VERSION"), log_path.map(|p| format!(" (log {})", p.display())).unwrap_or_default());

    let args = match args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            log!("error: arguments: {e}");
            return EXIT_ARGS;
        }
    };
    log!("stage: args ok (pipe {}, launcher pid {}{})", args.pipe, args.launcher_pid, if args.selftest_offline { ", SELFTEST OFFLINE" } else { "" });

    let pipe = match Pipe::connect(&args.pipe, args.launcher_pid, CONNECT_TIMEOUT) {
        Ok(p) => p,
        Err(e) => {
            log!("error: pipe: {e}");
            return EXIT_PIPE;
        }
    };
    log!("stage: connected; pipe server is the launcher (pid {})", args.launcher_pid);

    let fail = |pipe: &Pipe, code: i32, msg: String| -> i32 {
        log!("error: {msg}");
        let _ = pipe.write_all(proto::error_line(&msg).as_bytes());
        code
    };

    if let Err(e) = pipe.write_all(proto::hello_line(std::process::id()).as_bytes()) {
        log!("error: sending hello: {e}");
        return EXIT_PIPE;
    }
    let session = {
        let mut reader = LineReader::default();
        let parsed = match reader.read_line(&pipe, Some(SESSION_TIMEOUT)) {
            Ok(line) => proto::parse_session(line),
            Err(e) => return fail(&pipe, EXIT_PIPE, format!("reading the session: {e}")),
        };
        reader.wipe();
        match parsed {
            Ok(s) => s,
            Err(e) => return fail(&pipe, EXIT_PROTOCOL, format!("session rejected: {e}")),
        }
    };
    #[cfg(feature = "native-login-diagnostics")]
    startup_timing::mark("session-accepted");
    log!(
        "stage: session ok (v{}, region {}, lang {}, install_dir {}, pol_records {}, {} content ids)",
        session.version,
        session.region.name(),
        session.lang,
        session.install_dir.as_deref().unwrap_or("(none)"),
        session.pol_records.as_ref().map(|r| format!("{} valid handles/{} FFXI characters", r.valid_handles(), r.valid_chars())).unwrap_or_else(|| "none (records from FFXiMain's table)".into()),
        session.content_ids.len()
    );

    if let Some(fx) = &session.ffxi_settings {
        log!("stage: ffxi_settings: {} values served in-process ({})", fx.len(), fx.names().collect::<Vec<_>>().join(" "));
    }
    if args.selftest_offline && session.native_auth.is_some() {
        return fail(&pipe, EXIT_PROTOCOL, "Offline self-test cannot request a native account login.".into());
    }
    ffxi_settings::install(session.ffxi_settings);
    if session.native_auth.is_none() { slots::install_session(session.secrets, session.pol_records); }
    if session.invisible_login {
        log!("note: invisible_login ignored; the Viewer sends no status at sign-in");
    }
    let cfg = game::GameConfig { region: session.region, install_dir: session.install_dir, pol_install_dir: session.pol_install_dir, native_auth: session.native_auth, start_game: session.start_game, keep_session: session.keep_session, auto_away_minutes: session.auto_away_minutes, auto_login: session.auto_login, selftest: args.selftest_offline, trace_slots: args.trace_slots };
    events::start(pipe);

    if args.selftest_offline {
        std::thread::spawn(|| {
            std::thread::sleep(SELFTEST_WATCHDOG);
            log!("selftest: watchdog expired after {}s; ending the process", SELFTEST_WATCHDOG.as_secs());
            // GameStart may still be using the session allocation. End the process
            // without freeing it underneath an active game callback.
            std::process::exit(EXIT_SELFTEST_WATCHDOG);
        });
    }

    let game = std::thread::Builder::new().name("crystal-game".into()).stack_size(GAME_THREAD_STACK).spawn(move || {
        #[cfg(feature = "native-login-diagnostics")]
        startup_timing::mark("game-worker-start");
        game::run(cfg)
    });
    let result = match game {
        Ok(h) => h.join().unwrap_or_else(|_| Err("the game thread panicked".into())),
        Err(e) => Err(format!("cannot start the game thread: {e}")),
    };
    slots::wipe_session();

    let code = match result {
        Ok(outcome) => {
            log!("stage: game exited with code {}{}", outcome.code, if outcome.message.is_empty() { String::new() } else { format!(": {}", outcome.message) });
            if outcome.code != 0 && !outcome.message.is_empty() {
                events::send(proto::error_line(&outcome.message));
            }
            events::send(proto::exit_line(outcome.code));
            outcome.code
        }
        Err(msg) => {
            log!("error: {msg}");
            events::send(proto::error_line(&msg));
            events::send(proto::exit_line(EXIT_GAME_START));
            EXIT_GAME_START
        }
    };
    events::finish(Duration::from_secs(2));
    log!("stage: done (exit {code})");
    code
}
