//! Launch stage timing: how long each step takes from the Play click to the game running.
//!
//! A `Timeline` records elapsed time (a monotonic `Instant`, one small `Vec` push per mark)
//! for the sign-in stages and for the milestones every launch path shares. It only ever sees
//! stage names and fixed milestone labels, never account fields or session bytes.
//!
//! Output has three shapes:
//! - one compact line per mark for the launch's own log (`unwritten_lines`),
//! - a one-line summary for `launch-history.log` (`summary`): each phase's own duration,
//!   ending with the total from the click, e.g. `sign-in 2.8s, tool 0.4s, hello 1.9s,
//!   game started 9.4s`,
//! - a short line for the account row (`row_summary`), both figures from the click:
//!   `Signed in 2.8s · game started 9.4s`.

use crate::i18n::tf;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The same machine's performance counter, carried only for diagnostic timing.
/// Unlike a serialized elapsed duration, it includes child startup and pipe transit.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MonotonicStamp { ticks: u64, frequency: u64 }

impl MonotonicStamp {
    pub(crate) fn capture(clicked: Instant) -> Option<Self> {
        let now = Instant::now();
        let (ticks, frequency) = performance_counter()?;
        let elapsed = now.checked_duration_since(clicked)?.as_nanos();
        let passed = u64::try_from(elapsed.checked_mul(frequency as u128)? / 1_000_000_000).ok()?;
        Some(Self { ticks: ticks.checked_sub(passed)?, frequency })
    }

    pub(crate) fn restore(stamp: Option<Self>, fallback_elapsed_ms: u64) -> Instant {
        let now = Instant::now();
        let elapsed = stamp.and_then(|stamp| {
            let (ticks, frequency) = performance_counter()?;
            if stamp.frequency != frequency { return None; }
            let passed = ticks.checked_sub(stamp.ticks)?;
            Some(Duration::new(passed / frequency,
                ((passed % frequency) as u128 * 1_000_000_000 / frequency as u128) as u32))
        }).unwrap_or_else(|| Duration::from_millis(fallback_elapsed_ms));
        now.checked_sub(elapsed).unwrap_or(now)
    }
}

#[cfg(windows)]
fn performance_counter() -> Option<(u64, u64)> {
    // These kernel32 calls share one counter/frequency across local processes.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(value: *mut i64) -> i32;
        fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    }
    let (mut ticks, mut frequency) = (0i64, 0i64);
    // SAFETY: both calls write to live, correctly sized local integers.
    if unsafe { QueryPerformanceCounter(&mut ticks) } == 0
        || unsafe { QueryPerformanceFrequency(&mut frequency) } == 0
        || ticks < 0 || frequency <= 0 { return None; }
    Some((ticks as u64, frequency as u64))
}

#[cfg(not(windows))]
fn performance_counter() -> Option<(u64, u64)> { None }

/// Content-free cross-process timestamp for temporary startup diagnostics.
pub(crate) fn diagnostic_host_micros() -> Option<u64> {
    let (ticks, frequency) = performance_counter()?;
    u64::try_from(u128::from(ticks) * 1_000_000 / u128::from(frequency)).ok()
}

/// The points every launch path reports (each path reports the ones that exist for it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    SignInStart,
    SignInDone,
    /// The Ashita or Windower process was started.
    ToolStarted,
    /// crystal-loader connected and its image was verified.
    Hello,
    /// The session line reached the loader.
    SessionSent,
    /// The loader reported `started` (GameStart called).
    GameStarted,
    /// A native client process was spawned.
    ClientSpawned,
    /// The native client's login marker appeared.
    LoginAccepted,
}

impl Milestone {
    /// The words used in the log lines.
    fn log_label(self) -> &'static str {
        match self {
            Self::SignInStart => "sign-in start",
            Self::SignInDone => "sign-in done",
            Self::ToolStarted => "tool process started",
            Self::Hello => "loader hello",
            Self::SessionSent => "session delivered",
            Self::GameStarted => "game started",
            Self::ClientSpawned => "client process spawned",
            Self::LoginAccepted => "login accepted",
        }
    }
    /// The account row's text for a terminal milestone reached `at` after the
    /// click: capitalised when it starts the line, lower-case after
    /// "Signed in ...". Shown only, never stored in a file, so it is translated
    /// (the history summary above keeps its English words).
    fn row_text(self, at: Duration, first: bool) -> String {
        let time = secs(at, 1);
        let args: &[(&str, &dyn std::fmt::Display)] = &[("time", &time)];
        match (self, first) {
            (Self::GameStarted, true) => tf("Game started {time}", args),
            (Self::GameStarted, false) => tf("game started {time}", args),
            (Self::ClientSpawned, true) => tf("Client started {time}", args),
            (Self::ClientSpawned, false) => tf("client started {time}", args),
            (Self::LoginAccepted, true) => tf("Login accepted {time}", args),
            (Self::LoginAccepted, false) => tf("login accepted {time}", args),
            // Only `TERMINALS` reach the row; any other milestone would show
            // its time alone rather than an untranslated (history) label.
            _ => time,
        }
    }
    /// The words used in the history summary.
    fn summary_label(self) -> &'static str {
        match self {
            Self::SignInStart => "start",
            Self::SignInDone => "sign-in",
            Self::ToolStarted => "tool",
            Self::Hello => "hello",
            Self::SessionSent => "session",
            Self::GameStarted => "game started",
            Self::ClientSpawned => "client started",
            Self::LoginAccepted => "login accepted",
        }
    }
}

/// Phases reported as their own duration. Native POL signs in after the tool
/// starts; older routes sign in first, so their actual timestamps set the order.
const PHASES: [Milestone; 5] = [
    Milestone::SignInDone,
    Milestone::ToolStarted,
    Milestone::Hello,
    Milestone::SessionSent,
    Milestone::ClientSpawned,
];
/// How far a launch got, best first; the best one reached is reported as a total from the click.
const TERMINALS: [Milestone; 3] = [Milestone::GameStarted, Milestone::LoginAccepted, Milestone::ClientSpawned];

#[derive(Clone, Debug)]
struct Mark {
    at: Duration,
    milestone: Option<Milestone>,
    label: String,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    start: Instant,
    marks: Vec<Mark>,
    /// How many marks `unwritten_lines` has already handed out.
    written: usize,
}

impl Timeline {
    /// Start counting now.
    #[cfg(test)]
    pub fn begin() -> Self {
        Self::begin_at(Instant::now())
    }
    /// Start counting from an earlier instant, such as the Play click.
    pub fn begin_at(start: Instant) -> Self {
        Self { start, marks: Vec::new(), written: 0 }
    }

    /// Record a milestone now.
    pub fn mark(&mut self, milestone: Milestone) {
        self.push(self.start.elapsed(), Some(milestone), milestone.log_label().to_owned());
    }
    /// Record a sign-in stage now. The name comes from the sign-in library's progress text;
    /// it is flattened and capped so nothing odd can reach a log line.
    pub fn stage(&mut self, name: &str) {
        let flat: String = name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(80)
            .collect();
        self.push(self.start.elapsed(), None, format!("sign-in stage: {}", flat.trim()));
    }
    fn push(&mut self, at: Duration, milestone: Option<Milestone>, label: String) {
        self.marks.push(Mark { at, milestone, label });
    }

    /// When the milestone was first reached, counted from the start.
    pub fn elapsed_at(&self, milestone: Milestone) -> Option<Duration> {
        self.marks.iter().find(|m| m.milestone == Some(milestone)).map(|m| m.at)
    }

    /// The log lines for marks not yet handed out, one per mark:
    /// `timing +3.20s (+0.40s) tool process started`.
    pub fn unwritten_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for index in self.written..self.marks.len() {
            let previous = index.checked_sub(1).map_or(Duration::ZERO, |i| self.marks[i].at);
            let mark = &self.marks[index];
            lines.push(format!(
                "timing +{} (+{}) {}",
                secs(mark.at, 2),
                secs(mark.at.saturating_sub(previous), 2),
                mark.label
            ));
        }
        self.written = self.marks.len();
        lines
    }

    /// The history-line summary; empty when no milestone was reached.
    pub fn summary(&self) -> String {
        let terminal = self.terminal();
        let mut parts = Vec::new();
        let mut previous = Duration::ZERO;
        let mut phases: Vec<_> = PHASES.into_iter()
            .filter_map(|phase| self.elapsed_at(phase).map(|at| (phase, at)))
            .collect();
        phases.sort_by_key(|(_, at)| *at);
        for (phase, at) in phases {
            if Some(phase) == terminal {
                continue;
            }
            parts.push(format!("{} {}", phase.summary_label(), secs(at.saturating_sub(previous), 1)));
            previous = at;
        }
        if let Some(milestone) = terminal {
            if let Some(at) = self.elapsed_at(milestone) {
                parts.push(format!("{} {}", milestone.summary_label(), secs(at, 1)));
            }
        }
        parts.join(", ")
    }

    /// The account row's line, both figures from the click; empty when there is nothing to show.
    pub fn row_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(at) = self.elapsed_at(Milestone::SignInDone) {
            parts.push(tf("Signed in {time}", &[("time", &secs(at, 1))]));
        }
        if let Some(milestone) = self.terminal() {
            if let Some(at) = self.elapsed_at(milestone) {
                parts.push(milestone.row_text(at, parts.is_empty()));
            }
        } else if let Some(at) = self.elapsed_at(Milestone::SessionSent) {
            parts.push(tf("session delivered {time}", &[("time", &secs(at, 1))]));
        }
        parts.join(" · ")
    }

    fn terminal(&self) -> Option<Milestone> {
        TERMINALS.into_iter().find(|m| self.elapsed_at(*m).is_some())
    }

    /// Append every unwritten mark to `path` (best effort: timing never fails a launch).
    pub fn append_to(&mut self, path: &Path) {
        let lines = self.unwritten_lines();
        if lines.is_empty() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let time = chrono::Local::now().format("%H:%M:%S");
            for line in lines {
                let _ = writeln!(file, "[{time}] [launcher] {line}");
            }
        }
    }
}

/// `2.8s`; `digits` decimal places.
pub fn secs(duration: Duration, digits: usize) -> String {
    format!("{:.*}s", digits, duration.as_secs_f64())
}

/// The history outcome with the launch's timings after it; unchanged when there are none.
pub fn noted(outcome: &str, summary: &str) -> String {
    if summary.is_empty() {
        outcome.to_owned()
    } else {
        format!("{outcome} (launch: {summary})")
    }
}

/// Where a native client's timing lines go: beside its stderr log, never inside it (the
/// client owns that file and the exit classifier reads its tail).
pub fn sibling_path(stderr_log: &Path) -> PathBuf {
    let name = stderr_log.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".stderr.log").unwrap_or_else(|| name.strip_suffix(".log").unwrap_or(&name));
    stderr_log.with_file_name(format!("{stem}.timing.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_monotonic_stamp_keeps_legacy_timing() {
        assert!(MonotonicStamp::restore(None, 250).elapsed() >= Duration::from_millis(250));
        assert!(MonotonicStamp::restore(Some(MonotonicStamp { ticks: u64::MAX, frequency: 0 }), 250)
            .elapsed() >= Duration::from_millis(250));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "offline timing subprocess only"]
    fn clock_fixture() {
        use std::io::Read;
        // The message was already serialized before this delay. A duration-only
        // handoff would lose it when the child reconstructed the click.
        std::thread::sleep(Duration::from_millis(50));
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        let stamp: MonotonicStamp = serde_json::from_slice(&bytes).unwrap();
        println!("TIMING-ELAPSED {}", MonotonicStamp::restore(Some(stamp), 0).elapsed().as_millis());
    }

    #[cfg(windows)]
    #[test]
    fn monotonic_click_includes_subprocess_startup_and_request_transit() {
        use std::process::{Command, Stdio};
        let clicked = Instant::now();
        let bytes = serde_json::to_vec(&MonotonicStamp::capture(clicked).unwrap()).unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::clock_fixture"), "--ignored", "--nocapture"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let elapsed: u128 = stdout.lines().find_map(|line| line.strip_prefix("TIMING-ELAPSED "))
            .expect("child timing result").parse().unwrap();
        assert!(elapsed >= 50, "child startup/request delay was lost: {elapsed}ms");
        assert!(elapsed <= clicked.elapsed().as_millis(), "cross-process timing exceeds the observed duration");
    }

    fn timeline(marks: &[(u64, Milestone)]) -> Timeline {
        let mut t = Timeline::begin();
        for (ms, m) in marks {
            t.push(Duration::from_millis(*ms), Some(*m), m.log_label().to_owned());
        }
        t
    }

    fn pol_less() -> Timeline {
        timeline(&[
            (10, Milestone::SignInStart),
            (2800, Milestone::SignInDone),
            (3200, Milestone::ToolStarted),
            (5100, Milestone::Hello),
            (5200, Milestone::SessionSent),
            (9400, Milestone::GameStarted),
        ])
    }

    #[test]
    fn seconds_format_with_requested_digits() {
        assert_eq!(secs(Duration::from_millis(2840), 1), "2.8s");
        assert_eq!(secs(Duration::from_millis(2846), 2), "2.85s");
        assert_eq!(secs(Duration::ZERO, 1), "0.0s");
        assert_eq!(secs(Duration::from_secs(125), 1), "125.0s");
    }

    #[test]
    fn summary_gives_each_phase_then_the_total() {
        assert_eq!(
            pol_less().summary(),
            "sign-in 2.8s, tool 0.4s, hello 1.9s, session 0.1s, game started 9.4s"
        );
    }

    #[test]
    fn native_pol_summary_measures_authentication_after_the_tool_handshake() {
        let native = timeline(&[
            (220, Milestone::ToolStarted),
            (2800, Milestone::Hello),
            (2810, Milestone::SessionSent),
            (10350, Milestone::SignInDone),
            (13000, Milestone::GameStarted),
        ]);
        assert_eq!(native.summary(), "tool 0.2s, hello 2.6s, session 0.0s, sign-in 7.5s, game started 13.0s");
    }

    #[test]
    fn summary_stops_where_the_launch_stopped() {
        let unconfirmed = timeline(&[(2800, Milestone::SignInDone), (3200, Milestone::ToolStarted), (5100, Milestone::Hello)]);
        assert_eq!(unconfirmed.summary(), "sign-in 2.8s, tool 0.4s, hello 1.9s");
        assert_eq!(Timeline::begin().summary(), "");
        assert_eq!(Timeline::begin().row_summary(), "");
    }

    #[test]
    fn native_launches_end_at_the_spawn_or_the_login_marker() {
        let retail = timeline(&[(2800, Milestone::SignInDone), (3100, Milestone::ClientSpawned)]);
        assert_eq!(retail.summary(), "sign-in 2.8s, client started 3.1s");
        assert_eq!(retail.row_summary(), "Signed in 2.8s · client started 3.1s");
        let private = timeline(&[(100, Milestone::ClientSpawned), (4200, Milestone::LoginAccepted)]);
        assert_eq!(private.summary(), "client started 0.1s, login accepted 4.2s");
        assert_eq!(private.row_summary(), "Login accepted 4.2s");
        let unconfirmed = timeline(&[(100, Milestone::ClientSpawned)]);
        assert_eq!(unconfirmed.summary(), "client started 0.1s");
        assert_eq!(unconfirmed.row_summary(), "Client started 0.1s");
    }

    #[test]
    fn row_summary_uses_totals_from_the_click() {
        assert_eq!(pol_less().row_summary(), "Signed in 2.8s · game started 9.4s");
        let unconfirmed = timeline(&[(2800, Milestone::SignInDone), (8000, Milestone::SessionSent)]);
        assert_eq!(unconfirmed.row_summary(), "Signed in 2.8s · session delivered 8.0s");
    }

    #[test]
    fn log_lines_show_total_and_step_and_are_handed_out_once() {
        let mut t = timeline(&[(2800, Milestone::SignInDone), (3200, Milestone::ToolStarted)]);
        t.push(Duration::from_millis(3300), None, "sign-in stage: Logging in".into());
        assert_eq!(
            t.unwritten_lines(),
            vec![
                "timing +2.80s (+2.80s) sign-in done",
                "timing +3.20s (+0.40s) tool process started",
                "timing +3.30s (+0.10s) sign-in stage: Logging in",
            ]
        );
        assert!(t.unwritten_lines().is_empty());
        t.push(Duration::from_millis(5100), Some(Milestone::Hello), "loader hello".into());
        assert_eq!(t.unwritten_lines(), vec!["timing +5.10s (+1.80s) loader hello"]);
    }

    #[test]
    fn stage_names_are_flattened_and_capped() {
        let mut t = Timeline::begin();
        t.stage("Line one\r\nline two");
        t.stage(&"x".repeat(500));
        let lines = t.unwritten_lines();
        assert!(lines[0].ends_with("sign-in stage: Line one  line two"));
        assert!(!lines[0].contains('\n') && !lines[0].contains('\r'));
        assert!(lines[1].len() < 130);
        assert_eq!(t.summary(), "");
    }

    #[test]
    fn marks_from_a_real_clock_never_go_backwards() {
        let mut t = Timeline::begin();
        t.mark(Milestone::SignInStart);
        t.mark(Milestone::SignInDone);
        assert!(t.elapsed_at(Milestone::SignInDone) >= t.elapsed_at(Milestone::SignInStart));
        assert_eq!(t.unwritten_lines().len(), 2);
    }

    #[test]
    fn history_note_is_appended_only_when_there_is_one() {
        assert_eq!(noted("Game closed normally", ""), "Game closed normally");
        assert_eq!(
            noted("Game closed normally", "sign-in 2.8s, game started 9.4s"),
            "Game closed normally (launch: sign-in 2.8s, game started 9.4s)"
        );
    }

    #[test]
    fn native_timing_file_sits_beside_the_stderr_log() {
        assert_eq!(
            sibling_path(Path::new(r"C:\d\logs\abc.stderr.log")),
            PathBuf::from(r"C:\d\logs\abc.timing.log")
        );
        assert_eq!(sibling_path(Path::new(r"C:\d\logs\abc.log")), PathBuf::from(r"C:\d\logs\abc.timing.log"));
    }

    #[test]
    fn append_writes_each_mark_once() {
        let dir = std::env::temp_dir().join(format!("xi-launch-timing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("a.timing.log");
        let mut t = timeline(&[(2800, Milestone::SignInDone)]);
        t.append_to(&path);
        t.append_to(&path);
        t.push(Duration::from_millis(3100), Some(Milestone::ClientSpawned), "client process spawned".into());
        t.append_to(&path);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().contains("[launcher] timing +2.80s (+2.80s) sign-in done"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
