//! Launch diagnostics: why a game client ended, read from the tail of its stderr log,
//! plus the one-line-per-launch history file. Nothing here sees passwords or session
//! bytes; it only reads the client's own log text and exit status.

use crate::i18n::{t, tf};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// A client that ends sooner than this after it was spawned counts as a startup failure.
const STARTUP_WINDOW: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExitKind {
    /// A clean game exit or an acknowledged native sign-out.
    Normal,
    /// A `[recomp] FATAL at <address>: <reason>` line.
    Crash { address: String, reason: String },
    /// `GameStart returned ...; exit code N`.
    Code { code: i32 },
    /// Ended soon after spawn with nothing recognisable in the log.
    Startup,
    /// Anything else.
    Other,
    /// The POL-less loader reported an error (its message, one line).
    LoaderError { message: String },
    /// The background session worker process went away without a report.
    WorkerEnded,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExitReport {
    pub kind: ExitKind,
    /// The process exit status, when Windows gave one.
    pub exit_status: Option<i32>,
    /// Whether the client ended inside the startup window.
    pub during_startup: bool,
    /// One English line for the history file and logs; the Accounts page shows
    /// `message()`, the same line in the current language.
    pub summary: String,
}

impl ExitReport {
    /// Everything except a clean quit is worth the user's attention.
    pub fn is_problem(&self) -> bool {
        self.kind != ExitKind::Normal
    }

    /// `summary` in the current language, worded from the report's fields.
    pub fn message(&self) -> String {
        let startup = self.during_startup;
        let process = self.exit_status.filter(|code| *code != 0);
        match &self.kind {
            ExitKind::Normal => t("Game closed normally").into(),
            ExitKind::Crash { address, reason } => {
                let reason = crash_reason_shown(reason);
                let args: [(&str, &dyn std::fmt::Display); 2] = [("reason", &reason), ("address", address)];
                if startup {
                    tf("Game crashed during startup ({reason} at {address})", &args)
                } else {
                    tf("Game crashed ({reason} at {address})", &args)
                }
            }
            ExitKind::Code { code } if startup => tf("Game exited during startup with code {code}", &[("code", code)]),
            ExitKind::Code { code } => tf("Game exited with code {code}", &[("code", code)]),
            ExitKind::Startup => match process {
                Some(code) => tf("Game exited during startup (process exit {code})", &[("code", &code)]),
                None => t("Game exited during startup").into(),
            },
            ExitKind::Other => match process {
                Some(code) => tf("Game closed unexpectedly (process exit {code})", &[("code", &code)]),
                None => t("Game closed unexpectedly").into(),
            },
            ExitKind::LoaderError { message } if startup => {
                tf("Game loader error during startup: {message}", &[("message", message)])
            }
            ExitKind::LoaderError { message } => tf("Game loader error: {message}", &[("message", message)]),
            ExitKind::WorkerEnded => {
                t("The background session worker ended unexpectedly. Check the game's connection.").into()
            }
        }
    }

    /// The report for a session worker that ended without writing one.
    pub fn worker_ended() -> Self {
        ExitReport {
            kind: ExitKind::WorkerEnded,
            exit_status: None,
            during_startup: false,
            summary: "The background session worker ended unexpectedly. Check the game's connection.".into(),
        }
    }
}

/// Classify how a client ended. `tail` is the end of its stderr log, `exit_status` the process
/// exit code and `ran` how long it ran (None when unknown, which is never treated as startup).
pub fn classify_exit(tail: &str, exit_status: Option<i32>, ran: Option<Duration>) -> ExitReport {
    let during_startup = ran.is_some_and(|d| d < STARTUP_WINDOW);
    let lines: Vec<&str> = tail.lines().map(str::trim_end).collect();
    let last = lines.iter().rev().find(|l| !l.trim().is_empty()).copied();
    let phase = if during_startup { " during startup" } else { "" };

    let fatal = lines.iter().rev().find_map(|l| fatal_line(l));
    let kind = if let Some((address, reason)) = fatal {
        ExitKind::Crash { address, reason }
    } else if last == Some("[recomp] quit") {
        ExitKind::Normal
    } else if let Some(code) = lines.iter().rev().find_map(|l| game_start_code(l)) {
        ExitKind::Code { code }
    } else if lines.iter().any(|l| *l == "[recomp] quit") {
        // A quit line with trailing chatter after it is still a quit.
        ExitKind::Normal
    } else if during_startup {
        ExitKind::Startup
    } else {
        ExitKind::Other
    };
    let status_note = match exit_status {
        Some(code) if code != 0 => format!(" (process exit {code})"),
        _ => String::new(),
    };
    let summary = match &kind {
        ExitKind::Normal => "Game closed normally".to_owned(),
        ExitKind::Crash { address, reason } => {
            format!("Game crashed{phase} ({} at {address})", crash_reason(reason))
        }
        ExitKind::Code { code } => format!("Game exited{phase} with code {code}"),
        ExitKind::Startup => format!("Game exited during startup{status_note}"),
        ExitKind::Other => format!("Game closed unexpectedly{status_note}"),
        // Never read from a client log.
        ExitKind::LoaderError { .. } | ExitKind::WorkerEnded => String::new(),
    };
    ExitReport { kind, exit_status, during_startup, summary }
}

/// How a POL-less session (crystal-loader under Ashita or Windower) ended, as its session
/// pipe reported it. The loader writes no `[recomp]` log, so this replaces the log tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoaderEnd {
    /// `{"type":"exit","code":N}`.
    Exit(i32),
    /// Exit 0 after the native idle session accepted the user's sign-out.
    SignedOut,
    /// `{"type":"error","message":...}` (already flattened to one line).
    Error(String),
    /// The pipe closed (or failed) without an exit line.
    Closed,
}

/// Classify a loader session's end. `ran` is measured from when the session was handed over.
pub fn classify_loader_end(end: &LoaderEnd, ran: Duration) -> ExitReport {
    let during_startup = ran < STARTUP_WINDOW && !matches!(end, LoaderEnd::SignedOut);
    let phase = if during_startup { " during startup" } else { "" };
    let (kind, exit_status, summary) = match end {
        LoaderEnd::SignedOut => (ExitKind::Normal, Some(0), "Game closed normally".to_owned()),
        LoaderEnd::Exit(0) if !during_startup => (ExitKind::Normal, Some(0), "Game closed normally".to_owned()),
        LoaderEnd::Exit(code) => (
            ExitKind::Code { code: *code },
            Some(*code),
            format!("Game exited{phase} with code {code}"),
        ),
        LoaderEnd::Error(message) => {
            let short: String = message.chars().take(160).collect();
            let summary = format!("Game loader error{phase}: {short}");
            (ExitKind::LoaderError { message: short }, None, summary)
        }
        LoaderEnd::Closed if during_startup => (ExitKind::Startup, None, "Game exited during startup".to_owned()),
        LoaderEnd::Closed => (ExitKind::Other, None, "Game closed unexpectedly".to_owned()),
    };
    ExitReport { kind, exit_status, during_startup, summary }
}

/// `[recomp] FATAL at 102dbbc4: indirect call/jump ...` -> (address, reason).
fn fatal_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("[recomp] FATAL at ")?;
    let (address, reason) = rest.split_once(':').unwrap_or((rest, ""));
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    Some((address.to_owned(), reason.trim().to_owned()))
}

/// `[recomp] GameStart returned 00000000; exit code -14` -> -14.
fn game_start_code(line: &str) -> Option<i32> {
    let rest = line.trim().strip_prefix("[recomp] GameStart returned ")?;
    let (_, code) = rest.split_once("exit code ")?;
    code.trim().parse().ok()
}

/// Short wording for the crash reasons the client is known to report.
fn crash_reason(reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("no translation") {
        "no translation".to_owned()
    } else if reason.is_empty() {
        "fatal error".to_owned()
    } else {
        let short: String = reason.chars().take(60).collect();
        short
    }
}

/// `crash_reason` in the current language; a reason the client wrote itself
/// is shown as written.
fn crash_reason_shown(reason: &str) -> String {
    if reason.to_ascii_lowercase().contains("no translation") {
        t("no translation").into()
    } else if reason.is_empty() {
        t("fatal error").into()
    } else {
        crash_reason(reason)
    }
}

/// What a supervisor needs to describe and record one launch when it ends.
#[derive(Clone)]
pub struct Session {
    pub history: PathBuf,
    pub label: String,
    pub log: PathBuf,
}

impl Session {
    pub fn record(&self, outcome: &str) {
        append_history(&self.history, &self.label, outcome, Some(&self.log));
    }
}

/// `<time> | <label> | <outcome> | <log path>`. Fields are flattened to one line so a label
/// or message can never forge or split a history entry.
pub fn history_line(time: &str, label: &str, outcome: &str, log: Option<&Path>) -> String {
    let flat = |s: &str| {
        s.chars()
            .map(|c| if c.is_control() || c == '|' { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let log = log.map_or_else(|| "-".to_owned(), |p| flat(&p.to_string_lossy()));
    format!("{time} | {} | {} | {log}", flat(label), flat(outcome))
}

/// Best-effort append; diagnostics must never fail a launch.
pub fn append_history(history: &Path, label: &str, outcome: &str, log: Option<&Path>) {
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = history_line(&time, label, outcome, log);
    if let Some(parent) = history.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(history) {
        let _ = writeln!(file, "{line}");
    }
}

/// Split a launch error such as `"Client exited (...). Log: C:\x\a.stderr.log"` into the
/// message and the log path it points at.
pub fn split_log(error: &str) -> (&str, Option<PathBuf>) {
    match error.rsplit_once("Log: ") {
        Some((message, path)) if !path.trim().is_empty() && !path.contains('\n') => {
            (message.trim_end(), Some(PathBuf::from(path.trim())))
        }
        _ => (error.trim_end(), None),
    }
}

/// Check that `target` is inside `logs_dir` (after resolving `..` and links) and, for a file,
/// is a `.log` file. Returns the resolved path to hand to the shell.
pub fn resolve_log_target(logs_dir: &Path, target: &Path) -> Result<PathBuf, String> {
    let dir = fs::canonicalize(logs_dir).map_err(|_| t("The logs folder does not exist yet."))?;
    let resolved = fs::canonicalize(target).map_err(|_| t("That log file no longer exists."))?;
    if !resolved.starts_with(&dir) {
        return Err(t("That file is not in this account's logs folder.").into());
    }
    if resolved.is_file() && resolved.extension().and_then(|e| e.to_str()) != Some("log") {
        return Err(t("Only .log files can be opened from here.").into());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    const LONG: Option<Duration> = Some(Duration::from_secs(600));
    const SHORT: Option<Duration> = Some(Duration::from_secs(5));

    const NORMAL: &str = "[recomp] gamecore: lobby ffxi00.pol.com -> 124.150.154.122\n[recomp] quit\n";
    const FATAL: &str = "[recomp] d3d8: 2560x1440 render target drawn at 3840x2160\n[recomp] xinput: pad 0 polled, nothing connected\n\n[recomp] FATAL at 102dbbc4: indirect call/jump to an address with no translation\n  eax=00000001 ecx=ffffffff\n  guest stack: 0077ef9c 0000003d\n";
    const CODE0: &str = "[recomp] GameStart\n[recomp] GameStart returned 00000000; exit code 0\n";
    const CODE14: &str = "[recomp] gamecore: lobby ffxi00.pol.com -> 124.150.154.122\n[recomp] GameStart returned 00000000; exit code -14\n";

    #[test]
    fn quit_line_is_a_normal_close() {
        let r = classify_exit(NORMAL, Some(0), LONG);
        assert_eq!(r.kind, ExitKind::Normal);
        assert_eq!(r.summary, "Game closed normally");
        assert!(!r.is_problem());
        // Even a short session that quit cleanly is normal.
        assert_eq!(classify_exit(NORMAL, Some(0), SHORT).kind, ExitKind::Normal);
        // Trailing blank lines don't hide the quit.
        assert_eq!(classify_exit("[recomp] quit\r\n\r\n", None, LONG).kind, ExitKind::Normal);
    }

    #[test]
    fn fatal_line_is_a_crash_with_its_address() {
        let r = classify_exit(FATAL, Some(1), SHORT);
        assert_eq!(
            r.kind,
            ExitKind::Crash {
                address: "102dbbc4".into(),
                reason: "indirect call/jump to an address with no translation".into()
            }
        );
        assert_eq!(r.summary, "Game crashed during startup (no translation at 102dbbc4)");
        assert!(r.is_problem());
        let late = classify_exit(FATAL, Some(1), LONG);
        assert_eq!(late.summary, "Game crashed (no translation at 102dbbc4)");
        assert!(!late.during_startup);
    }

    #[test]
    fn unknown_fatal_reason_is_shown_trimmed() {
        let r = classify_exit("[recomp] FATAL at 0f001234: access violation reading 0\n", None, LONG);
        assert_eq!(r.summary, "Game crashed (access violation reading 0 at 0f001234)");
        let bare = classify_exit("[recomp] FATAL at 0f001234\n", None, None);
        assert_eq!(bare.summary, "Game crashed (fatal error at 0f001234)");
    }

    #[test]
    fn game_start_return_reports_its_exit_code() {
        let r = classify_exit(CODE14, Some(242), LONG);
        assert_eq!(r.kind, ExitKind::Code { code: -14 });
        assert_eq!(r.summary, "Game exited with code -14");
        let zero = classify_exit(CODE0, Some(0), SHORT);
        assert_eq!(zero.kind, ExitKind::Code { code: 0 });
        assert_eq!(zero.summary, "Game exited during startup with code 0");
    }

    #[test]
    fn fatal_wins_over_an_earlier_game_start_line() {
        let text = format!("{CODE0}{FATAL}");
        assert!(matches!(classify_exit(&text, None, LONG).kind, ExitKind::Crash { .. }));
    }

    #[test]
    fn silent_early_exit_is_a_startup_failure() {
        let r = classify_exit("", Some(3), SHORT);
        assert_eq!(r.kind, ExitKind::Startup);
        assert_eq!(r.summary, "Game exited during startup (process exit 3)");
        assert_eq!(classify_exit("[recomp] GameStart\n", Some(0), SHORT).summary, "Game exited during startup");
    }

    #[test]
    fn silent_late_exit_is_other() {
        let r = classify_exit("[modern] display: 1280x720\n", Some(1), LONG);
        assert_eq!(r.kind, ExitKind::Other);
        assert_eq!(r.summary, "Game closed unexpectedly (process exit 1)");
        // Unknown runtime is never called startup.
        assert_eq!(classify_exit("", None, None).kind, ExitKind::Other);
    }

    #[test]
    fn loader_session_ends_are_classified() {
        let long = Duration::from_secs(600);
        let short = Duration::from_secs(5);
        let normal = classify_loader_end(&LoaderEnd::Exit(0), long);
        assert_eq!(normal.kind, ExitKind::Normal);
        assert_eq!(normal.summary, "Game closed normally");
        assert!(!normal.is_problem());
        assert_eq!(classify_loader_end(&LoaderEnd::Exit(0), short).summary, "Game exited during startup with code 0");
        let code = classify_loader_end(&LoaderEnd::Exit(-14), long);
        assert_eq!(code.kind, ExitKind::Code { code: -14 });
        assert_eq!(code.exit_status, Some(-14));
        let error = classify_loader_end(&LoaderEnd::Error("FFXiMain.dll not found".into()), short);
        assert_eq!(error.summary, "Game loader error during startup: FFXiMain.dll not found");
        assert!(error.is_problem());
        assert_eq!(classify_loader_end(&LoaderEnd::Closed, short).kind, ExitKind::Startup);
        assert_eq!(classify_loader_end(&LoaderEnd::Closed, long).summary, "Game closed unexpectedly");
    }

    #[test]
    fn acknowledged_native_sign_out_is_clean_even_inside_the_startup_window() {
        let short = Duration::from_secs(5);
        let signed_out = classify_loader_end(&LoaderEnd::SignedOut, short);
        assert_eq!(signed_out.kind, ExitKind::Normal);
        assert_eq!(signed_out.exit_status, Some(0));
        assert!(!signed_out.during_startup);
        assert!(!signed_out.is_problem());
        // Time and a zero exit alone never prove an intentional sign-out.
        for end in [LoaderEnd::Exit(0), LoaderEnd::Exit(-14), LoaderEnd::Closed,
            LoaderEnd::Error("sign-out failed".into())] {
            let report = classify_loader_end(&end, short);
            assert!(report.is_problem(), "{end:?}");
            assert!(report.during_startup, "{end:?}");
        }
    }

    /// The row's text is the history summary, worded from the report's fields
    /// in the current language, so a language change reaches it too.
    #[test]
    fn shown_message_matches_the_english_summary_for_every_kind() {
        crate::i18n::set(crate::i18n::Lang::En);
        let long = Duration::from_secs(600);
        let short = Duration::from_secs(5);
        let reports = [
            classify_exit(NORMAL, Some(0), LONG),
            classify_exit(FATAL, Some(1), SHORT),
            classify_exit(FATAL, Some(1), LONG),
            classify_exit("[recomp] FATAL at 0f001234: access violation reading 0\n", None, LONG),
            classify_exit("[recomp] FATAL at 0f001234\n", None, None),
            classify_exit(CODE14, Some(242), LONG),
            classify_exit(CODE0, Some(0), SHORT),
            classify_exit("", Some(3), SHORT),
            classify_exit("[recomp] GameStart\n", Some(0), SHORT),
            classify_exit("[modern] display: 1280x720\n", Some(1), LONG),
            classify_exit("", None, None),
            classify_loader_end(&LoaderEnd::Exit(0), long),
            classify_loader_end(&LoaderEnd::Exit(-14), short),
            classify_loader_end(&LoaderEnd::Error("FFXiMain.dll not found".into()), short),
            classify_loader_end(&LoaderEnd::Error("pipe broke".into()), long),
            classify_loader_end(&LoaderEnd::Closed, short),
            classify_loader_end(&LoaderEnd::Closed, long),
            ExitReport::worker_ended(),
            classify_loader_end(&LoaderEnd::SignedOut, short),
        ];
        for report in &reports {
            assert_eq!(report.message(), report.summary, "{report:?}");
        }
        // The worker's record format still reads back (new kinds included).
        let encoded = serde_json::to_string(&reports[13]).unwrap();
        assert_eq!(serde_json::from_str::<ExitReport>(&encoded).unwrap(), reports[13]);
        // Japanese words come from the table; the address and code stay as data.
        crate::i18n::set(crate::i18n::Lang::Ja);
        assert!(reports[1].message().contains("102dbbc4"));
        assert!(reports[5].message().contains("-14"));
        crate::i18n::set(crate::i18n::Lang::En);
    }

    #[test]
    fn history_lines_are_single_line_and_carry_all_fields() {
        let line = history_line(
            "2026-10-03 12:00:00",
            "Main | account\n",
            "Game crashed\r\n(no translation at 102dbbc4)",
            Some(Path::new(r"C:\data\logs\a.stderr.log")),
        );
        assert_eq!(
            line,
            r"2026-10-03 12:00:00 | Main   account | Game crashed  (no translation at 102dbbc4) | C:\data\logs\a.stderr.log"
        );
        assert!(!line.contains('\n') && !line.contains('\r'));
        assert!(history_line("t", "a", "b", None).ends_with("| b | -"));
    }

    #[test]
    fn launch_errors_split_into_message_and_log_path() {
        let (message, log) = split_log(r"Client exited during login/startup (exit code: 1). Log: C:\d\logs\a.stderr.log");
        assert_eq!(message, "Client exited during login/startup (exit code: 1).");
        assert_eq!(log, Some(PathBuf::from(r"C:\d\logs\a.stderr.log")));
        assert_eq!(split_log("Retail authentication failed: bad").1, None);
    }

    #[test]
    fn history_append_round_trip() {
        let dir = std::env::temp_dir().join(format!("xi-launch-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("logs")).unwrap();
        let log = dir.join("logs").join("a.stderr.log");
        let history = dir.join("iced-launcher").join("launch-history.log");
        let session = Session { history: history.clone(), label: "Main".into(), log: log.clone() };
        let report = classify_exit(FATAL, Some(1), Some(Duration::from_secs(3)));
        assert!(matches!(report.kind, ExitKind::Crash { .. }));
        session.record(&report.summary);
        session.record("second");
        let text = fs::read_to_string(&history).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("| Main | Game crashed during startup (no translation at 102dbbc4) |"));
        assert!(lines[0].ends_with("a.stderr.log"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn opener_stays_inside_the_logs_folder() {
        let dir = std::env::temp_dir().join(format!("xi-launch-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("a.stderr.log"), "x").unwrap();
        fs::write(logs.join("a.exe"), "x").unwrap();
        fs::write(dir.join("secret.log"), "x").unwrap();
        assert!(resolve_log_target(&logs, &logs.join("a.stderr.log")).is_ok());
        assert!(resolve_log_target(&logs, &logs).is_ok());
        assert!(resolve_log_target(&logs, &logs.join("a.exe")).is_err());
        assert!(resolve_log_target(&logs, &dir.join("secret.log")).is_err());
        assert!(resolve_log_target(&logs, &logs.join("..").join("secret.log")).is_err());
        assert!(resolve_log_target(&logs, &logs.join("missing.log")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
