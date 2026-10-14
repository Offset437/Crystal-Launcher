//! crystal-loader: a 32-bit host that authenticates through the installed
//! PlayOnline core and starts retail FINAL FANTASY XI. Crystal runs it directly
//! for Vanilla or through Ashita v4 and Windower 4.
//!
//! Order matters for the injectors: nothing game-related is loaded until `run`
//! has the session inputs; polcore, FFXi.dll and FFXiMain are then loaded at run time
//! through COM / LoadLibraryExW, in this process, on the game thread.

pub mod args;
mod auto_login;
mod controller_settings;
pub mod b64;
pub mod events;
pub mod ffxi_settings;
mod friend_locations;
pub mod game;
mod game_layout;
pub mod json;
pub mod lobby;
pub mod native_auth;
mod native_jobs;
mod native_profile;
mod native_social;
mod park;
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
mod sigscan;
pub mod slots;
pub mod trace;
#[cfg(target_arch = "x86")]
mod windower_exit;

use std::time::Duration;

use pipe::{LineReader, Pipe};

/// Fixed-name startup markers only. QPC uses the same host clock as the
/// launcher's `launch_timing::MonotonicStamp`; it never changes POL's clock.
pub(crate) mod startup_timing {
    use std::time::Instant;
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError, FILETIME},
        System::{SystemInformation::GetSystemTimePreciseAsFileTime,
            Threading::{GetCurrentProcess, GetProcessTimes}},
    };

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

    fn filetime_ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    fn process_age_micros(created: u64, sampled: u64) -> Option<u64> {
        if created == 0 { return None; }
        sampled.checked_sub(created).map(|ticks| ticks / 10)
    }

    pub(crate) struct Entry {
        stamp: Stamp,
        process_age_us: Option<u64>,
        sample_span_us: u128,
    }
    impl Entry {
        pub(crate) fn capture() -> Self {
            let _error = LastError::save();
            let stamp = Stamp::capture();
            let mut sampled = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            // Sample wall time before querying process metadata. The short span
            // bounds its offset from the first-entry monotonic stamp; process
            // age includes time spent suspended before Windower resumes us.
            // SAFETY: the API writes one live FILETIME output.
            unsafe { GetSystemTimePreciseAsFileTime(&mut sampled); }
            let sample_span_us = stamp.instant.elapsed().as_micros().saturating_add(1);
            let (mut created, mut exited, mut kernel, mut user) = (sampled, sampled, sampled, sampled);
            // SAFETY: the current-process pseudo-handle and four live FILETIME
            // outputs require no handle ownership or process enumeration.
            let valid = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created,
                &mut exited, &mut kernel, &mut user) } != 0;
            let process_age_us = valid.then(|| process_age_micros(
                filetime_ticks(created), filetime_ticks(sampled))).flatten();
            Self { stamp, process_age_us, sample_span_us }
        }

        pub(crate) fn report(&self) {
            let _error = LastError::save();
            crate::log!("native startup timing: stage=loader-entry start_host_us={} end_host_us={} host_clock_valid={} process_age_us={} process_age_valid={} process_age_sample_span_us={}",
                self.stamp.host_us.unwrap_or(0), self.stamp.host_us.unwrap_or(0), self.stamp.host_us.is_some(),
                self.process_age_us.unwrap_or(0), self.process_age_us.is_some(), self.sample_span_us);
        }
    }

    pub(crate) fn report(stage: &'static str, start: Stamp, end: Stamp) {
        let _error = LastError::save();
        crate::log!("native startup timing: stage={stage} start_host_us={} end_host_us={} host_clock_valid={} elapsed_us={}",
            start.host_us.unwrap_or(0), end.host_us.unwrap_or(0), start.host_us.is_some() && end.host_us.is_some(),
            end.instant.saturating_duration_since(start.instant).as_micros());
    }

    #[cfg(feature = "native-login-diagnostics")]
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

        #[test]
        fn process_age_uses_filetime_units_and_rejects_missing_or_future_creation() {
            let created = filetime_ticks(FILETIME { dwLowDateTime: 0xffff_fff8, dwHighDateTime: 2 });
            let sampled = filetime_ticks(FILETIME { dwLowDateTime: 0x18, dwHighDateTime: 3 });
            assert_eq!(process_age_micros(created, sampled), Some(3));
            assert_eq!(process_age_micros(created, created), Some(0));
            assert_eq!(process_age_micros(0, sampled), None);
            assert_eq!(process_age_micros(sampled, created), None);
        }

        #[test]
        fn current_process_entry_has_valid_clocks_and_preserves_last_error() {
            let _error = LastError::save();
            unsafe { SetLastError(23456) };
            let entry = Entry::capture();
            assert_eq!(unsafe { GetLastError() }, 23456);
            assert!(entry.stamp.host_us.is_some());
            assert!(entry.process_age_us.is_some());
            entry.report();
            assert_eq!(unsafe { GetLastError() }, 23456);
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
    let entry = startup_timing::Entry::capture();
    let log_started = startup_timing::Stamp::capture();
    let log_path = log::init();
    let log_ready = startup_timing::Stamp::capture();
    entry.report();
    startup_timing::report("log-init", log_started, log_ready);
    log!("crystal-loader {} starting{}", env!("CARGO_PKG_VERSION"), log_path.map(|p| format!(" (log {})", p.display())).unwrap_or_default());

    let args = match args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            log!("error: arguments: {e}");
            return EXIT_ARGS;
        }
    };
    log!("stage: args ok (pipe {}, launcher pid {}{})", args.pipe, args.launcher_pid, if args.selftest_offline { ", SELFTEST OFFLINE" } else { "" });

    let connect_started = startup_timing::Stamp::capture();
    let connected = Pipe::connect(&args.pipe, args.launcher_pid, CONNECT_TIMEOUT);
    let connect_finished = startup_timing::Stamp::capture();
    let pipe = match connected {
        Ok(p) => p,
        Err(e) => {
            startup_timing::report("pipe-connect", connect_started, connect_finished);
            log!("error: pipe: {e}");
            return EXIT_PIPE;
        }
    };
    log!("stage: connected; pipe server is the launcher (pid {})", args.launcher_pid);

    let hello_started = startup_timing::Stamp::capture();
    let hello = pipe.write_all(proto::hello_line(std::process::id()).as_bytes());
    let hello_finished = startup_timing::Stamp::capture();
    // Flush these markers after the hello so diagnostics do not hold Windower's
    // global injection turn longer than the actual handshake requires.
    startup_timing::report("pipe-connect", connect_started, connect_finished);
    startup_timing::report("hello-send", hello_started, hello_finished);
    if let Err(e) = hello {
        log!("error: sending hello: {e}");
        return EXIT_PIPE;
    }
    let first = {
        let mut reader = LineReader::default();
        let parsed = match reader.read_line(&pipe, Some(SESSION_TIMEOUT)) {
            Ok(line) => proto::parse_first_line(line),
            Err(e) => return fail(&pipe, EXIT_PIPE, format!("reading the session: {e}")),
        };
        reader.wipe();
        match parsed {
            Ok(first) => first,
            Err(e) => return fail(&pipe, EXIT_PROTOCOL, format!("session rejected: {e}")),
        }
    };
    match first {
        proto::FirstLine::Session(session) => run_session(pipe, session, &args),
        proto::FirstLine::Park(park) => run_parked(pipe, park, &args),
    }
}

fn fail(pipe: &Pipe, code: i32, msg: String) -> i32 {
    log!("error: {msg}");
    let _ = pipe.write_all(proto::error_line(&msg).as_bytes());
    code
}

fn log_session(session: &proto::Session) {
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
}

fn log_ffxi_settings(settings: Option<&ffxi_settings::FfxiSettings>) {
    if let Some(fx) = settings {
        log!("stage: ffxi_settings: {} values served in-process ({})", fx.len(), fx.names().collect::<Vec<_>>().join(" "));
    }
}

/// The cold path: a direct `session` line.
fn run_session(pipe: Pipe, session: proto::Session, args: &args::Args) -> i32 {
    log_session(&session);
    log_ffxi_settings(session.ffxi_settings.as_ref());
    if args.selftest_offline && session.native_auth.is_some() {
        return fail(&pipe, EXIT_PROTOCOL, "Offline self-test cannot request a native account login.".into());
    }
    ffxi_settings::install(session.ffxi_settings);
    if session.native_auth.is_none() { slots::install_session(session.secrets, session.pol_records); }
    if session.invisible_login {
        log!("note: invisible_login ignored; the Viewer sends no status at sign-in");
    }
    let cfg = game::GameConfig { region: session.region, install_dir: session.install_dir, pol_install_dir: session.pol_install_dir, native_auth: session.native_auth, start_game: session.start_game, keep_session: session.keep_session, viewer_gaps: session.viewer_gaps, auto_away_minutes: session.auto_away_minutes, auto_login: session.auto_login, selftest: args.selftest_offline, trace_slots: args.trace_slots };
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

    let game = spawn_game(move || game::run(cfg));
    finish(game.and_then(join_game))
}

type GameResult = Result<game::GameOutcome, String>;

fn spawn_game(run: impl FnOnce() -> GameResult + Send + 'static) -> Result<std::thread::JoinHandle<GameResult>, String> {
    std::thread::Builder::new().name("crystal-game".into()).stack_size(GAME_THREAD_STACK).spawn(move || {
        #[cfg(feature = "native-login-diagnostics")]
        startup_timing::mark("game-worker-start");
        run()
    }).map_err(|e| format!("cannot start the game thread: {e}"))
}

fn join_game(game: std::thread::JoinHandle<GameResult>) -> GameResult {
    game.join().unwrap_or_else(|_| Err("the game thread panicked".into()))
}

/// A parked host (pol-spec 07d §1.3): polcore up to its validated table on
/// the game thread, `parked`, then the credentials with no timeout, then
/// exactly the cold path (`Park::into_session` rebuilds its session).
fn run_parked(pipe: Pipe, mut park: proto::Park, args: &args::Args) -> i32 {
    let park_started = std::time::Instant::now();
    log!("stage: park ok (v{}, region {}, lang {}, install_dir {}, ffxi_settings {}, auto_login {})",
        park.version, park.region.name(), park.lang, park.install_dir.as_deref().unwrap_or("(none)"),
        park.ffxi_settings.as_ref().map_or(0, |fx| fx.len()), if park.auto_login.enabled { "on" } else { "off" });
    if args.selftest_offline {
        return fail(&pipe, EXIT_PROTOCOL, "Offline self-test cannot park a native account host.".into());
    }
    log_ffxi_settings(park.ffxi_settings.as_ref());
    ffxi_settings::install(park.ffxi_settings.take());
    let cfg = game::GameConfig { region: park.region, install_dir: park.install_dir.clone(), pol_install_dir: park.pol_install_dir.clone(),
        native_auth: None, start_game: false, keep_session: false, viewer_gaps: false, auto_away_minutes: 0,
        auto_login: park.auto_login, selftest: false, trace_slots: args.trace_slots };
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (play, play_rx) = std::sync::mpsc::channel();
    let game = match spawn_game(move || game::run_parked(cfg, ready_tx, play_rx)) {
        Ok(game) => game,
        Err(e) => return game_failed(&pipe, e),
    };
    // Paths, polcore creation, the build pin: today's error line and exit 5.
    let Ok(polcore) = ready.recv() else {
        let error = match join_game(game) {
            Ok(outcome) if !outcome.message.is_empty() => outcome.message,
            Ok(outcome) => format!("the game host ended while parking (code {})", outcome.code),
            Err(e) => e,
        };
        return game_failed(&pipe, error);
    };
    let mut reader = LineReader::default();
    let credentials = park::await_credentials(std::process::id(), park_started, polcore,
        &mut |bytes| pipe.write_all(bytes),
        &mut || reader.read_line(&pipe, None).map(proto::parse_credentials));
    reader.wipe();
    let unpark = match credentials {
        Ok((unpark, _)) => unpark,
        Err(code) => {
            // The game thread wakes, releases polcore and ends; nothing was networked.
            drop(play);
            let deadline = std::time::Instant::now() + PARKED_RELEASE;
            while !game.is_finished() && std::time::Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
            return code;
        }
    };
    let session = park.into_session(unpark);
    log_session(&session);
    if session.invisible_login {
        log!("note: invisible_login ignored; the Viewer sends no status at sign-in");
    }
    let proto::Session { native_auth, start_game, keep_session, viewer_gaps, auto_away_minutes, .. } = session;
    let Some(native_auth) = native_auth else { return fail(&pipe, EXIT_PROTOCOL, "credentials rejected: no native credentials".into()) };
    events::start(pipe);
    // The game thread is parked waiting for exactly this; its sign-in follows.
    let sent = play.send(game::Play { native_auth, start_game, keep_session, viewer_gaps, auto_away_minutes });
    let result = join_game(game);
    finish(match (sent, result) {
        (Err(_), Ok(_)) => Err("the parked game host ended before the Play".into()),
        (_, result) => result,
    })
}

/// How long an exiting parked host lets its game thread release polcore.
const PARKED_RELEASE: Duration = Duration::from_secs(2);

/// A game-thread failure before the events pipe exists (parking).
fn game_failed(pipe: &Pipe, error: String) -> i32 {
    let code = fail(pipe, EXIT_GAME_START, error);
    let _ = pipe.write_all(proto::exit_line(code).as_bytes());
    code
}

/// The game thread's result as the launcher's `error`/`exit` lines.
fn finish(result: GameResult) -> i32 {
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
