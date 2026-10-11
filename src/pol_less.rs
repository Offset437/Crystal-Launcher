//! Retail launches directly, through Ashita v4, or through Windower 4.
//!
//! The launcher starts the game host directly or through Ashita-cli/Windower and hands
//! credentials to the verified loader over a private named pipe.
//! The installed native PlayOnline core authenticates and owns the retail session;
//! the headless launcher worker supervises it independently of the iced window.
//!
//! Secrets never appear on a command line, in the environment, in Ashita's boot ini or
//! config.json, or in the per-launch log; they reach the loader only through its pipe.

mod ffxi_settings;
mod native_auth;
pub(crate) mod pipe;
pub(crate) mod profile_access;
pub(crate) mod tools;
mod turns;

use crate::backend::{plain_windows_path, unique_id, with_log, Account, Config, LaunchResult};
use crate::i18n::{t, tf};
use crate::launch_log::{classify_loader_end, LoaderEnd, Session};
use crate::launch_timing::{noted, Milestone, Timeline};
use pipe::{AcceptError, LoaderEvent, LoaderPipe};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
pub(crate) use tools::Tool;
use turns::Turns;
use zeroize::Zeroizing;

/// The launcher-wide gate on injector starts (see `turns` for what is serialized and why).
static TURNS: Turns = Turns::new();

/// The Launch tools settings, as the Settings page stores them. Empty means "detect".
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolSettings {
    pub ashita: String,
    pub windower: String,
    /// Base boot ini file name inside Ashita's `config\boot` (default `default.ini`).
    pub boot_ini: String,
    pub windower_profile: String,
    /// Dev override for `crystal-loader.exe`.
    pub loader: String,
    /// put dgVoodoo next to the loader (else take Crystal's copy away).
    #[serde(default)]
    pub dgvoodoo: bool,
    #[serde(default)]
    pub reshade: crate::reshade::Preferences,
}

/// One retail launch request: which launch method, with which settings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Plan {
    pub tool: Tool,
    pub settings: ToolSettings,
    #[serde(default)]
    pub social_only: bool,
    #[serde(default)]
    pub invisible_login: bool,
    /// Auto-Away after this many idle minutes (0 = off; Viewer setting 4/7).
    #[serde(default = "default_auto_away")]
    pub auto_away_minutes: u8,
    #[serde(default)]
    pub auto_login: AutoLogin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct AutoLogin {
    pub enabled: bool,
    /// One-based row in the character selection screen.
    pub character_slot: u8,
}
impl Default for AutoLogin {
    fn default() -> Self { Self { enabled: false, character_slot: 1 } }
}
impl AutoLogin {
    pub(crate) fn normalized(self) -> Self {
        if (1..=16).contains(&self.character_slot) { self } else { Self::default() }
    }
}

/// The Viewer's static default for "Auto-away time" (app 10490ef0 entry 7,
/// index 0 = 10 minutes).
pub(crate) const fn default_auto_away() -> u8 { 10 }
/// The Viewer's choices: 10..60 minutes in steps of 10, or off (-1 there).
pub(crate) const AUTO_AWAY_CHOICES: [u8; 7] = [0, 10, 20, 30, 40, 50, 60];

#[derive(Clone, Copy)]
struct Timeouts {
    /// Injector start to loader connect. Covers Ashita's UAC prompt and Windower's update check.
    connect: Duration,
    /// Connect to hello.
    hello: Duration,
    /// Writing the session line.
    send: Duration,
    /// Session sent to `started`; past this the launch returns as "start unconfirmed".
    start: Duration,
    /// Windower exits once the game window exists; the launch log notes when, if within this
    /// long after the hello. Nothing waits for it.
    injector_exit: Duration,
}

const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(60),
    hello: Duration::from_secs(10),
    send: Duration::from_secs(10),
    start: Duration::from_secs(120),
    injector_exit: Duration::from_secs(30),
};

/// What the launcher needs from a finished retail sign-in: the contract's `session` line and
/// a way to forward the loader's presence updates. A trait so tests can stand in a fake.
pub(crate) trait SignedIn: Send + Sync + 'static {
    fn native_core(&self) -> bool { false }
    fn social_only(&self) -> bool { false }
    /// A Play: the session signs in idle (`start_game: false`) and the worker starts FFXI
    /// itself, after the stock version check, once the loader reports the idle session.
    fn start_pending(&self) -> bool { false }
    /// The session is signed in with no game: a sign-in-only launch, or a Play until its start.
    fn idle_first(&self) -> bool { self.social_only() }
    /// The deferred start was sent (`None`) or stopped by the version check (`Some(message)`:
    /// the session stays signed in and idle, with the message as its error).
    fn deferred_start_done(&self, _blocked: Option<&str>) {}
    fn game_running(&self) -> Option<Arc<AtomicBool>> { None }
    fn native_session_state(&self, _game: bool) -> Result<(), String> { Ok(()) }
    fn native_own_mode(&self, _mode: i32) -> Result<(), String> { Ok(()) }
    fn native_control_result(&self, _action: &str, _code: i32) -> Result<(), String> { Ok(()) }
    /// A pending control and its "also for my groups" choice.
    fn take_native_control(&self) -> Option<(crate::session_worker::NativeControl, bool)> { None }
    fn take_native_batch(&self) -> Option<Vec<u8>> { None }
    fn native_command_listener(&self) -> Option<crate::session_worker::CommandWake> { None }
    /// The window saw input for this idle session since the last call.
    fn take_native_activity(&self) -> bool { false }
    fn take_native_social(&self) -> Option<Zeroizing<Vec<u8>>> { None }
    fn native_social_ready(&self) -> Result<(), String> { Ok(()) }
    /// Retry only a failed local status-cache publication; no native POL operation.
    fn retry_native_publication(&self) -> Result<bool, String> { Ok(false) }
    fn native_mail_changed(&self) -> Result<(), String> { Ok(()) }
    fn native_social_result(&self, _id: &str, _action: &str, _code: i32, _payload: Option<&[u8]>) -> Result<(), String> { Ok(()) }
    fn receive_native_friends(&self, _blob: &[u8], _locations: Option<&[u8]>) -> Result<usize,String> { Err("No native POL session is active.".into()) }
    fn native_status(&self, _code:i32) {}
    /// The one `session` line, `\n`-terminated, in a buffer zeroed on drop.
    fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String>;
    /// Forward a 24-byte `PresenceArgs` payload; `Ok(changed)`.
    fn forward_presence(&self, payload: &[u8]) -> Result<bool, String>;
    /// Refreshed lobby credentials, sent before the presence completion.
    fn live_credentials_line(&self) -> Result<Option<Zeroizing<String>>, String> { Ok(None) }
    fn fetch_friends(&self) -> Result<crate::friends::Fetched, String> { Err("Friends unavailable in this test session.".into()) }
    fn friend_updates(&self, _since: u64) -> Option<(u64,crate::friends::Fetched)> { None }
    fn session_status(&self) -> i32 { 0 }
    fn sign_out(self) -> String where Self: Sized { "Session closed.".into() }
}

impl SignedIn for crystal_pol::retail::Authenticated {
    fn friend_updates(&self, since: u64) -> Option<(u64,crate::friends::Fetched)> {
        self.friend_updates(since).map(|(revision,records)| (revision,crate::friends::fetched(&records)))
    }
    fn fetch_friends(&self) -> Result<crate::friends::Fetched, String> {
        self.fetch_friends().map(|records| crate::friends::fetched(&records))
    }
    fn session_status(&self) -> i32 {
        use crystal_pol::keepalive::KeepaliveError;
        match self.keepalive.check() {
            Ok(()) => 0,
            Err(KeepaliveError::HeartbeatExpired) => -1038,
            Err(_) => -1025,
        }
    }
    fn sign_out(self) -> String {
        let report = self.sign_out();
        format!("POL sign-out: QUIT sent={}, acknowledged={}, peer closed={}.", report.quit_sent, report.acknowledged, report.peer_closed)
    }
    fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut session = self
            .loader_session()
            .map_err(|e| tf("The signed-in session can't be handed to crystal-loader: {error}.", &[("error", &e)]))?;
        session.install_dir = Some(
            install_dir
                .to_str()
                .ok_or(t("The game folder's path is not valid Unicode."))?
                .to_owned(),
        );
        let json = session.to_contract_json();
        drop(session);
        let mut line = Zeroizing::new(Vec::with_capacity(json.len() + 1));
        line.extend_from_slice(json.as_bytes());
        line.push(b'\n');
        Ok(line)
    }
    fn forward_presence(&self, payload: &[u8]) -> Result<bool, String> {
        use crystal_pol::retail::PresenceOutcome;
        match self.send_presence(payload) {
            Ok(PresenceOutcome::Unchanged) => Ok(false),
            Ok(PresenceOutcome::Sent { .. }) => Ok(true),
            Err(e) => Err(e.to_string()),
        }
    }
    fn live_credentials_line(&self) -> Result<Option<Zeroizing<String>>, String> {
        let session = self.loader_session().map_err(|e| e.to_string())?;
        Ok(Some(credentials_line(&session.v_key, &session.auth_code)))
    }
}

/// Status controls carry `groups`: the Viewer follows a status change with
/// ChgAllGrpStatus 404 when I am in groups (app 10040f39) unless told not to.
fn native_control_line(action: crate::session_worker::NativeControl, groups: bool) -> String {
    use crate::session_worker::NativeControl::{Away, Invisible, Visible};
    match action {
        Visible | Away | Invisible => format!("{{\"type\":\"native_{}\",\"groups\":{groups}}}\n", action.name()),
        _ => format!("{{\"type\":\"native_{}\"}}\n", action.name()),
    }
}

fn credentials_line(v_key: &[u8; 16], auth_code: &[u8; 52]) -> Zeroizing<String> {
    let mut line = Vec::with_capacity(192);
    line.extend_from_slice(br#"{"type":"session_update","v_key":""#);
    pipe::base64_into(&mut line, v_key);
    line.extend_from_slice(br#"","auth_code":""#);
    pipe::base64_into(&mut line, auth_code);
    line.extend_from_slice(b"\"}\n");
    Zeroizing::new(String::from_utf8(line).expect("base64 is ASCII"))
}

/// Sign in and launch `account` with its selected retail method. Local checks run before
/// authentication, so a missing tool or loader never costs a sign-in.
pub(crate) fn launch(
    config: &Config,
    plan: &Plan,
    account: Account,
    otp: Zeroizing<String>,
    timing: Timeline,
    progress: &mut impl FnMut(&str),
) -> Result<LaunchResult, String> {
    if account.region != 0 {
        return Err(t("Retail login currently supports the verified US install only. EU and JP protocol region values are not verified yet.").into());
    }
    if !otp.is_empty() && (otp.len() != 6 || !otp.bytes().all(|b| b.is_ascii_digit())) {
        return Err(t("Enter a six-digit one-time code, or leave it empty.").into());
    }
    let prepared = prepare_with_progress(config, plan, &account, progress)?;
    // beside the exact loader this launch runs; never fails the launch.
    if let Some(line) = crate::dgvoodoo::sync_for_launch(&prepared.loader, plan.settings.dgvoodoo) {
        progress(&line);
    }
    let mut authenticated=native_auth::NativeAuth::new(&account,otp,config.account_directory(&account))?;
    authenticated.set_social_only(plan.social_only);
    // Every Play signs in idle first, like a sign-in-only launch. The worker runs the stock
    // FFXI version check after authentication, overlapping native startup mail/presence.
    // GameStart still requires both the full native idle report and the check's verdict.
    authenticated.set_deferred_start(!plan.social_only);
    authenticated.set_invisible_login(plan.invisible_login);
    authenticated.set_auto_away(plan.auto_away_minutes);
    authenticated.set_auto_login(plan.auto_login);
    drop(account);
    let sign_in = move |_timing: &mut Timeline, progress: &mut dyn FnMut(&str)| {
        progress(t("Preparing native PlayOnline authentication..."));
        Ok(authenticated)
    };
    sign_in_then_run(prepared, sign_in, timing, progress, &mut spawn_injector, &TURNS, TIMEOUTS)
}

/// Sign in, then start the tool. Each account runs this on its own worker thread, so the
/// sign-ins (the slow, independent network part) all run at once; only the tool start
/// takes a turn, and only for as long as the tool's own handshake needs (`turns`).
fn sign_in_then_run<A: SignedIn>(
    p: Prepared,
    sign_in: impl FnOnce(&mut Timeline, &mut dyn FnMut(&str)) -> Result<A, String>,
    mut timing: Timeline,
    progress: &mut dyn FnMut(&str),
    spawn: &mut Spawner<'_>,
    turns: &'static Turns,
    limits: Timeouts,
) -> Result<LaunchResult, String> {
    timing.mark(Milestone::SignInStart);
    let authenticated = sign_in(&mut timing, progress)?;
    if !authenticated.native_core() { timing.mark(Milestone::SignInDone); }
    run(p, authenticated, timing, progress, spawn, turns, limits)
}

/// Everything resolved before sign-in.
struct Prepared {
    tool: Tool,
    tool_dir: PathBuf,
    /// Canonical plain path; the hello's process must run exactly this image.
    loader: PathBuf,
    install_dir: PathBuf,
    region: &'static str,
    ini_name: String,
    base_ini: Option<Vec<u8>>,
    profile: String,
    /// Profile video/sound values and Crystal's retained menu size, served in-process
    /// (`ffxi_settings`), so the shared registry they are also written to doesn't matter.
    ffxi_settings: Option<ffxi_settings::WindowerSettings>,
    /// Protects the settings snapshot until Windower.exe finishes saving it.
    profile_access: Option<profile_access::Guard>,
    session: Session,
    friends_directory: Option<PathBuf>,
    /// The FFXI version check run before a deferred game start (replaced in tests).
    check: GameCheck,
}

/// The stock FFXI check for a game folder: `Ok(note)` to start, `Err(message)` to stop.
type GameCheck = fn(&Path) -> Result<String, String>;

enum CheckTask {
    Waiting,
    Running(std::thread::JoinHandle<Result<String, String>>),
    Failed(String),
    Consumed,
}

/// One fresh check per Play, moved intact from the startup wait to the supervisor.
/// Only the stateless public version request overlaps native startup services.
struct GameVersionCheck {
    install: PathBuf,
    directory: Option<PathBuf>,
    check: GameCheck,
    task: CheckTask,
}
impl GameVersionCheck {
    fn new(prepared: &Prepared) -> Self {
        Self { install: prepared.install_dir.clone(), directory: prepared.friends_directory.clone(), check: prepared.check, task: CheckTask::Waiting }
    }

    fn authenticated(&mut self, auth: &impl SignedIn, log: &mut SessionLog) {
        if let Some(directory) = &self.directory {
            if let Err(error) = crate::session_worker::publish_authenticated(directory) { log.line(&error); }
        }
        if auth.start_pending() { self.start(log); }
    }

    fn start(&mut self, log: &mut SessionLog) {
        if !matches!(self.task, CheckTask::Waiting) { return; }
        log.line("Signed in; checking the FFXI version while native startup services finish.");
        let (install, check) = (self.install.clone(), self.check);
        self.task = match std::thread::Builder::new().name("ffxi-version-check".into()).spawn(move || check(&install)) {
            Ok(task) => CheckTask::Running(task),
            Err(_) => CheckTask::Failed(t("Could not start the game version check.").into()),
        };
    }

    fn finish(&mut self, log: &mut SessionLog) -> Result<String, String> {
        // Older loaders may report only full idle. They still check exactly once.
        self.start(log);
        match std::mem::replace(&mut self.task, CheckTask::Consumed) {
            CheckTask::Running(task) => task.join().unwrap_or_else(|_| Err(t("The game version check failed.").into())),
            CheckTask::Failed(error) => Err(error),
            CheckTask::Waiting | CheckTask::Consumed => Err(t("The game version check was already consumed.").into()),
        }
    }
}

#[cfg(test)]
fn prepare(config: &Config, plan: &Plan, account: &Account) -> Result<Prepared, String> {
    prepare_with_progress(config, plan, account, &mut |_| {})
}

fn prepare_with_progress(config: &Config, plan: &Plan, account: &Account, progress: &mut dyn FnMut(&str)) -> Result<Prepared, String> {
    let tool = plan.tool;
    let settings = &plan.settings;
    let configured = match tool {
        Tool::Ashita => Some(&settings.ashita),
        Tool::Windower => Some(&settings.windower),
        Tool::Vanilla => None,
    };
    let tool_dir = if let Some(configured) = configured {
        Some(tools::detect(tool, configured).ok_or_else(|| {
            if configured.trim().is_empty() {
                tf(
                    "{tool} was not found in the usual folders. Set its folder in Settings → System → Launch tools, or choose another Launch with.",
                    &[("tool", &tool.name())],
                )
            } else {
                tf(
                    "{tool} was not found: {folder} has no {exe}. Fix the folder in Settings → System → Launch tools.",
                    &[("tool", &tool.name()), ("folder", &configured.trim()), ("exe", &tool.exe())],
                )
            }
        })?)
    } else { None };
    let loader = tools::loader_path(&settings.loader).ok_or_else(|| {
        if settings.loader.trim().is_empty() {
            tf(
                "{loader} is missing from the launcher's folder. Reinstall the launcher, or set the loader path in Settings → System → Launch tools.",
                &[("loader", &tools::LOADER_EXE)],
            )
        } else {
            tf("The crystal-loader path {path} does not exist.", &[("path", &settings.loader.trim())])
        }
    })?;
    let loader = crate::reshade::loader_for_launch(&loader, &settings.reshade, settings.dgvoodoo)?;
    let loader = plain_windows_path(&fs::canonicalize(&loader).unwrap_or(loader));
    let install_dir = install_directory(config)?;
    let tool_dir = tool_dir.unwrap_or_else(|| install_dir.clone());
    let region = "US";
    let mut base_ini = None;
    let mut profile = String::new();
    let mut profile_access = None;
    let ffxi_settings;
    // Windower gives Hook.dll its profile's UI dimensions during injection.
    // Overriding only FFXiMain's menu dimensions would leave game and plugins
    // using different coordinate systems. Crystal's menu preference belongs to
    // the other launch methods, including when that preference is unreadable.
    let menu = if tool == Tool::Windower { None } else { crate::game_config::retail_menu(config)? };
    match tool {
        Tool::Vanilla => ffxi_settings = menu.map(ffxi_settings::menu_settings),
        Tool::Ashita => {
            let explicit = !settings.boot_ini.trim().is_empty();
            let name = if explicit { settings.boot_ini.trim() } else { tools::DEFAULT_BOOT_INI };
            if !tools::valid_boot_ini_name(name) {
                return Err(t("The base boot config must be an .ini file name in Ashita's config\\boot folder.").into());
            }
            let path = tool_dir.join("config").join("boot").join(name);
            match fs::metadata(&path) {
                Ok(meta) if meta.len() > 1024 * 1024 => return Err(tf("{path} is too large to be a boot config.", &[("path", &path.display())])),
                Ok(_) => base_ini = Some(fs::read(&path).map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?),
                Err(_) if explicit => {
                    return Err(tf(
                        "The base boot config {name} was not found in {folder}. Choose another in the account's launch settings or in Settings → System → Launch tools.",
                        &[("name", &name), ("folder", &path.parent().unwrap_or(&path).display())],
                    ))
                }
                Err(_) => {}
            }
            // Fails early on a loader path Ashita can't read (non-ASCII).
            tools::ashita_boot_ini(base_ini.as_deref(), &loader, "check", 0, region, &[])?;
            ffxi_settings = menu.map(ffxi_settings::menu_settings);
        }
        Tool::Windower => {
            profile = settings.windower_profile.trim().to_owned();
            if !tools::valid_profile(&profile) {
                return Err(t("The Windower profile name can't contain quotes or control characters.").into());
            }
            profile_access = Some(profile_access::launching(&tool_dir, progress)?);
            ffxi_settings = Some(ffxi_settings::for_windower(&tool_dir, &profile)?);
        }
    }
    let logs = config.logs_directory(account);
    fs::create_dir_all(&logs).map_err(|_| t("Could not create the account's log folder."))?;
    Ok(Prepared {
        tool,
        tool_dir,
        loader,
        install_dir,
        region,
        ini_name: tools::boot_ini_name(&crate::backend::account_key(account)),
        base_ini,
        profile,
        ffxi_settings,
        profile_access,
        session: Session {
            history: config.history_path(),
            label: account.label.clone(),
            log: logs.join(format!("{}.crystal.log", unique_id()?)),
        },
        friends_directory: Some(config.account_directory(account)),
        check: crate::patch::gate_before_start,
    })
}

/// The retail game folder the loader runs: the one chosen on Downloads, else the installed
/// client's registry entry.
pub(crate) fn install_directory(config: &Config) -> Result<PathBuf, String> {
    let dir = match &config.game_install {
        Some(path) => plain_windows_path(path),
        None => registry_install_dir().ok_or(
            t("The FINAL FANTASY XI folder is not known. Select it on the Downloads page."),
        )?,
    };
    for name in ["FFXi.dll", "FFXiMain.dll"] {
        if !dir.join(name).is_file() {
            return Err(tf(
                "{folder} has no {name}. Select the FINAL FANTASY XI folder on the Downloads page.",
                &[("folder", &dir.display()), ("name", &name)],
            ));
        }
    }
    Ok(dir)
}

#[cfg(windows)]
fn registry_install_dir() -> Option<PathBuf> {
    crate::discovery::find(crate::discovery::Product::Ffxi, None)
}
#[cfg(not(windows))]
fn registry_install_dir() -> Option<PathBuf> {
    None
}

/// The per-launch log (`<id>.crystal.log` in the account's logs folder): what the launcher
/// did and what the loader reported. Never session bytes or credentials.
#[derive(Default)]
pub(crate) struct SessionLog {
    file: Option<File>,
    // Diagnostic context from verified pipe events; log writes may fail, so
    // the exit classification must not depend on rereading their text.
    native_idle: bool,
    native_sign_out: bool,
    native_game_started: bool,
    native_shutdown_returned: bool,
    native_failure: bool,
}
impl SessionLog {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| t("Could not create the launch log."))?;
        Ok(Self { file: Some(file), ..Self::default() })
    }
    fn line(&mut self, text: &str) {
        if let Some(file) = &mut self.file {
            let time = chrono::Local::now().format("%H:%M:%S");
            let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            // One append per line: the Windower watcher writes to the same log from its thread.
            let _ = file.write_all(format!("[{time}] [launcher] {flat}\n").as_bytes());
        }
    }
    /// Write any timing marks the log has not seen yet.
    fn timing(&mut self, timeline: &mut Timeline) {
        for line in timeline.unwritten_lines() {
            self.line(&line);
        }
    }
    /// Record a milestone and write it.
    fn reached(&mut self, timeline: &mut Timeline, milestone: Milestone) {
        timeline.mark(milestone);
        self.timing(timeline);
    }
    /// The log as a child's stdout/stderr, so the injector's own messages land in it too.
    fn stdio(&self) -> Stdio {
        self.file
            .as_ref()
            .and_then(|f| f.try_clone().ok())
            .map_or_else(Stdio::null, Stdio::from)
    }
}

/// The Ashita-cli or Windower process, watched until the loader connects.
pub(crate) enum Injector {
    Child(std::process::Child),
    /// Started through the UAC prompt; only waitable.
    #[cfg(windows)]
    Elevated(std::os::windows::io::OwnedHandle),
    /// No process to watch (tests).
    #[allow(dead_code)]
    Detached,
    /// A stand-in process that "exits" when its flag is set (tests).
    #[cfg(test)]
    Fake(Arc<AtomicBool>),
}
impl Injector {
    /// `Some(code)` once the process has exited.
    fn exited(&mut self) -> Option<Option<i32>> {
        match self {
            Injector::Child(child) => child.try_wait().ok().flatten().map(|s| s.code()),
            #[cfg(windows)]
            Injector::Elevated(handle) => {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::{
                    Foundation::WAIT_OBJECT_0,
                    System::Threading::{GetExitCodeProcess, WaitForSingleObject},
                };
                unsafe {
                    if WaitForSingleObject(handle.as_raw_handle() as _, 0) != WAIT_OBJECT_0 {
                        return None;
                    }
                    let mut code = 0u32;
                    Some((GetExitCodeProcess(handle.as_raw_handle() as _, &mut code) != 0).then_some(code as i32))
                }
            }
            Injector::Detached => None,
            #[cfg(test)]
            Injector::Fake(exited) => exited.load(Ordering::SeqCst).then_some(Some(0)),
        }
    }
    /// Stop our own child if it is still running (a stuck Ashita-cli or Windower).
    fn stop(&mut self) {
        if let Injector::Child(child) = self {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn wait(&mut self) -> std::io::Result<()> {
        match self {
            Injector::Child(child) => child.wait().map(|_| ()),
            #[cfg(windows)]
            Injector::Elevated(handle) => {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::{WaitForSingleObject, INFINITE}};
                if unsafe { WaitForSingleObject(handle.as_raw_handle() as _, INFINITE) } == WAIT_OBJECT_0 {
                    Ok(())
                } else { Err(std::io::Error::last_os_error()) }
            }
            Injector::Detached => Ok(()),
            #[cfg(test)]
            Injector::Fake(exited) => {
                while !exited.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(5)); }
                Ok(())
            }
        }
    }
}

/// On a failed Windower start, stop its launcher before releasing permission to
/// edit profiles. A successful start moves this owner into the exit observer.
struct LaunchedInjector {
    injector: Injector,
    profile_access: Option<profile_access::Guard>,
    stop_on_drop: bool,
}
impl Drop for LaunchedInjector {
    fn drop(&mut self) {
        if self.stop_on_drop { self.injector.stop(); }
    }
}

/// Start the injector for this launch.
fn spawn_injector(p: &Prepared, pipe: &str, log: &mut SessionLog) -> Result<Injector, String> {
    let pid = std::process::id();
    let exe = p.tool_dir.join(p.tool.exe());
    match p.tool {
        Tool::Vanilla => {
            let mut command = tools::vanilla_command(&p.loader, &p.install_dir, pipe, pid);
            command.stdout(log.stdio()).stderr(log.stdio());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
            }
            command.spawn().map(Injector::Child)
                .map_err(|e| tf("Could not start {exe}: {error}", &[("exe", &p.loader.display()), ("error", &e)]))
        }
        Tool::Ashita => {
            let ini = tools::ashita_boot_ini(p.base_ini.as_deref(), &p.loader, pipe, pid, p.region, &[])?;
            let boot = p.tool_dir.join("config").join("boot");
            let path = boot.join(&p.ini_name);
            let temporary = boot.join(format!("{}.tmp", p.ini_name));
            fs::create_dir_all(&boot)
                .and_then(|_| fs::write(&temporary, &ini))
                .and_then(|_| fs::rename(&temporary, &path))
                .map_err(|_| {
                    let _ = fs::remove_file(&temporary);
                    tf("Could not write Ashita's boot config {path}.", &[("path", &path.display())])
                })?;
            log.line(&format!("Wrote {}", path.display()));
            let mut command = tools::ashita_command(&p.tool_dir, &p.ini_name);
            command.stdout(log.stdio()).stderr(log.stdio());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
            }
            match command.spawn() {
                Ok(child) => Ok(Injector::Child(child)),
                // ERROR_ELEVATION_REQUIRED: Ashita-cli's manifest requires administrator.
                #[cfg(windows)]
                Err(e) if e.raw_os_error() == Some(740) => {
                    log.line("Ashita-cli needs administrator rights; asking Windows.");
                    run_elevated(&exe, &p.ini_name, &p.tool_dir).map(Injector::Elevated)
                }
                Err(e) => Err(tf("Could not start {exe}: {error}", &[("exe", &exe.display()), ("error", &e)])),
            }
        }
        Tool::Windower => {
            let mut command = tools::windower_command(&p.tool_dir, &p.profile, &p.loader, pipe, pid)?;
            command
                .spawn()
                .map(Injector::Child)
                .map_err(|e| tf("Could not start {exe}: {error}", &[("exe", &exe.display()), ("error", &e)]))
        }
    }
}

/// ShellExecuteEx "runas": the UAC prompt, for a launcher that isn't elevated.
#[cfg(windows)]
fn run_elevated(exe: &Path, argument: &str, directory: &Path) -> Result<std::os::windows::io::OwnedHandle, String> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_CANCELLED},
        UI::{
            Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
            WindowsAndMessaging::SW_HIDE,
        },
    };
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    let parameters = wide(argument.as_ref());
    let dir = wide(directory.as_os_str());
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = dir.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(if unsafe { GetLastError() } == ERROR_CANCELLED {
            t("The Windows administrator prompt for Ashita was declined. Ashita needs administrator rights: accept the prompt, or run the launcher as administrator.").into()
        } else {
            tf("Windows could not start {exe} as administrator.", &[("exe", &exe.display())])
        });
    }
    if info.hProcess.is_null() {
        return Err(tf("Windows started {exe} but returned no process to watch.", &[("exe", &exe.display())]));
    }
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(info.hProcess as _) })
}

/// After a Windower start's hello: wait (on its own thread) until Windower.exe has exited or
/// `until` passes, and note which in the launch log. If still running, retain the
/// profile guard in a blocking child wait until its final settings.xml save.
fn watch_windower_exit(mut child: LaunchedInjector, hello: Instant, until: Instant, log: PathBuf) -> impl FnOnce() + Send + 'static {
    move || {
        let exited = loop {
            if child.injector.exited().is_some() {
                break true;
            }
            if Instant::now() >= until {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let line = if exited {
            format!("Windower.exe exited {:.1} s after the loader's hello.", hello.elapsed().as_secs_f32())
        } else {
            format!("Windower.exe was still running {:.0} s after the loader's hello.", hello.elapsed().as_secs_f32())
        };
        if let Ok(mut log) = SessionLog::open(&log) {
            log.line(&line);
        }
        if !exited && child.profile_access.is_some() {
            let waited = child.injector.wait();
            if let Ok(mut log) = SessionLog::open(&log) {
                log.line(&match waited {
                    Ok(()) => format!("Windower.exe exited {:.1} s after the loader's hello; profile editing is available.", hello.elapsed().as_secs_f32()),
                    Err(error) => format!("Could not wait for Windower.exe to finish saving profiles: {error}"),
                });
            }
        }
    }
}

fn observe_windower_exit(child: LaunchedInjector, hello: Instant, until: Instant, log: PathBuf) -> Result<(), String> {
    let protected = child.profile_access.is_some();
    let watcher = watch_windower_exit(child, hello, until, log);
    let observer = std::thread::Builder::new().name("windower-exit".into()).spawn(watcher)
        .map_err(|_| t("Could not monitor Windower's profile save.").to_owned())?;
    if protected {
        profile_access::retain_observer(observer);
    }
    Ok(())
}

enum Step {
    Started,
    Continue,
    End(LoaderEnd),
}

/// The contract's reply to one `presence` line; the loader blocks on it.
fn presence_result(result: &Result<bool, String>) -> String {
    match result {
        Ok(changed) => format!(r#"{{"type":"presence_result","ok":true,"changed":{changed}}}"#),
        Err(error) => format!(
            r#"{{"type":"presence_result","ok":false,"error":{}}}"#,
            serde_json::to_string(error).unwrap_or_else(|_| "\"error\"".into())
        ),
    }
}

fn handle_event(event: LoaderEvent, pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog) -> Step {
    match event {
        LoaderEvent::Started { pid } => {
            log.native_idle = false;
            log.native_sign_out = false;
            log.native_game_started = auth.native_core();
            log.native_shutdown_returned = false;
            log.native_failure = false;
            log.line(&format!("Game started (GameStart called){}", pid.map_or(String::new(), |p| format!(", PID {p}"))));
            Step::Started
        }
        LoaderEvent::Presence(payload) => {
            let mut result = auth.forward_presence(&payload);
            if matches!(result, Ok(true)) {
                match auth.live_credentials_line() {
                    Ok(Some(line)) => {
                        if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
                            log.line("Could not deliver refreshed lobby credentials; the session pipe failed.");
                            return Step::End(LoaderEnd::Closed);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => result = Err(error),
                }
            }
            match &result {
                Ok(true) => log.line("Presence update sent."),
                Ok(false) => log.line("Presence update: nothing changed."),
                Err(why) => log.line(&format!("Presence update ({} bytes) not sent: {why}.", payload.len())),
            }
            let reply = presence_result(&result) + "\n";
            if pipe.send(reply.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
                log.line("Could not answer the presence update; the session pipe failed.");
                return Step::End(LoaderEnd::Closed);
            }
            Step::Continue
        }
        LoaderEvent::NativeSocialReady => {
            if auth.native_core() {
                if let Err(error) = auth.native_social_ready() { log.line(&error); }
            }
            Step::Continue
        }
        LoaderEvent::NativeSocialResult { id, action, code, payload } => {
            if auth.native_core() {
                log.line(&format!("Native POL {action} result {code}."));
                if let Err(error) = auth.native_social_result(&id, &action, code, payload.as_deref()) { log.line(&error); }
            }
            Step::Continue
        }
        LoaderEvent::NativeMailChanged => {
            if auth.native_core() {
                if let Err(error) = auth.native_mail_changed() { log.line(&error); }
            }
            Step::Continue
        }
        LoaderEvent::NativeFriends { blob, locations } => {
            if auth.native_core() {
                match auth.receive_native_friends(&blob, locations.as_deref()) {
                    Ok(count)=>log.line(&format!("Native POL published {count} friend records.")),
                    Err(error)=>log.line(&error),
                }
            }
            Step::Continue
        }
        LoaderEvent::NativeStatus(code) => {
            if auth.native_core() {
                if code < 0 {
                    log.native_idle = false;
                    log.native_sign_out = false;
                    log.native_failure = true;
                }
                auth.native_status(code);
                log.line(&format!("Native POL connection result {code}."));
            }
            Step::Continue
        }
        LoaderEvent::NativeFriendsRefresh(code) => {
            // The Viewer has no manual refresh; the launcher no longer asks.
            if auth.native_core() { log.line(&format!("Native POL friend refresh result {code}.")); }
            Step::Continue
        }
        LoaderEvent::NativeSessionState(game) => {
            if auth.native_core() {
                log.native_idle = !game;
                log.native_sign_out = false;
                if let Err(error) = auth.native_session_state(game) { log.line(&error); }
                log.line(if game { "Native POL session handed to FFXI." } else { "Native POL session is signed in to the Viewer; FFXI is closed." });
            }
            if !game && auth.idle_first() { Step::Started } else { Step::Continue }
        }
        LoaderEvent::NativeOwnMode(mode) => {
            if auth.native_core() {
                if let Err(error) = auth.native_own_mode(mode) { log.line(&error); }
            }
            Step::Continue
        }
        LoaderEvent::NativeControlResult { action, code } => {
            if auth.native_core() {
                if code < 0 { log.native_failure = true; }
                if action == "sign_out" {
                    log.native_sign_out = code == 0 && log.native_idle;
                } else if action == "start_game" && code == 0 {
                    log.native_idle = false;
                    log.native_sign_out = false;
                }
                if let Err(error) = auth.native_control_result(&action, code) { log.line(&error); }
                log.line(&format!("Native POL {action} request result {code}."));
            }
            Step::Continue
        }
        LoaderEvent::NativeGameEnded { code, game_returned } => {
            // Native FFXI uses -14 for /shutdown after GameStart returns S_OK.
            // Require this run's explicit successful return and sign-out acknowledgment.
            log.native_shutdown_returned = auth.native_core() && log.native_game_started && game_returned && code == -14;
            log.line(&format!("FFXI returned {code}."));
            Step::Continue
        }
        LoaderEvent::Exit(code) => {
            log.line(&format!("Loader reported exit code {code}."));
            Step::End(if log.native_sign_out && (code == 0 || code == -14 && log.native_shutdown_returned && !log.native_failure) {
                LoaderEnd::SignedOut
            } else { LoaderEnd::Exit(code) })
        }
        LoaderEvent::Error(message) => {
            log.line(&format!("Loader reported an error: {message}"));
            Step::End(LoaderEnd::Error(message))
        }
        LoaderEvent::Closed => {
            log.line("The session pipe closed.");
            Step::End(LoaderEnd::Closed)
        }
        LoaderEvent::Hello { .. } | LoaderEvent::Unknown | LoaderEvent::NativeAuthStage(_) | LoaderEvent::Authenticated => Step::Continue,
    }
}

/// Until `started` (`Ok(true)`), the deadline (`Ok(false)`, start unconfirmed), or the end.
/// `until_game`: the wait after a deferred `native_start_game`, which ends when the loader
/// reports the session handed to FFXI (an idle report is not a start).
fn wait_started(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, timeout: Duration, timing:&mut Timeline, progress:&mut dyn FnMut(&str), until_game: bool, game: &mut GameVersionCheck) -> Result<bool, LoaderEnd> {
    let deadline = Instant::now() + timeout;
    loop {
        match pipe.next_event(Some(deadline)) {
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                log.line(&format!("No start report within {} s; the game may still be loading.", timeout.as_secs()));
                return Ok(false);
            }
            Err(_) => {
                log.line("The session pipe failed.");
                return Err(LoaderEnd::Closed);
            }
            Ok(Err(message)) => log.line(&message),
            Ok(Ok(event)) => {
                if auth.native_core() {
                    match &event {
                        LoaderEvent::NativeAuthStage(state)=>{
                            log.line(&format!("Native POL authentication state {state}."));
                            progress(match state { 26..=28=>t("PlayOnline: authenticating Square Enix account..."),40..=41=>t("PlayOnline: selecting service..."),30=>t("PlayOnline: loading profile and friends..."),_=>t("PlayOnline: connecting...") });
                        }
                        LoaderEvent::Authenticated=>{
                            log.reached(timing,Milestone::SignInDone);
                            game.authenticated(auth, log);
                        }
                        _=>{},
                    }
                }
                let (handed_over, idle) = (matches!(&event, LoaderEvent::NativeSessionState(true)), matches!(&event, LoaderEvent::NativeSessionState(false)));
                match handle_event(event, pipe, auth, log) {
                Step::Started if until_game && idle => {}
                Step::Started => return Ok(true),
                Step::Continue if until_game && handed_over => return Ok(true),
                Step::Continue => {}
                Step::End(end) => return Err(end),
                }
            },
        }
    }
}

/// A Play whose session is now signed in and idle: consume its FFXI version check, then
/// `native_start_game`. `Ok(Ok(()))`: start sent. `Ok(Err(message))`: the check says update;
/// nothing is sent and the session stays signed in. `Err(())`: the pipe failed.
fn start_deferred(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, game: &mut GameVersionCheck, progress: &mut dyn FnMut(&str)) -> Result<Result<(), String>, ()> {
    progress(t("Checking the game version..."));
    log.line("Native startup services finished; awaiting the FFXI version check before starting the game.");
    match game.finish(log) {
        Err(message) => {
            log.line(&message);
            auth.deferred_start_done(Some(&message));
            Ok(Err(message))
        }
        Ok(note) => {
            log.line(&note);
            progress(t("Starting FFXI..."));
            let line = native_control_line(crate::session_worker::NativeControl::StartGame, false);
            if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
                log.line("Could not send the game start; the session pipe failed.");
                return Err(());
            }
            log.line("Game start sent to the signed-in session.");
            auth.deferred_start_done(None);
            Ok(Ok(()))
        }
    }
}

/// The rest of the session, on the supervisor thread.
fn watch(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, friends: Option<&FriendService>, game: &mut GameVersionCheck) -> LoaderEnd {
    pipe.listen_for_commands(auth.native_command_listener());
    // The loader begins healthy. Only changes need a pipe message.
    let mut reported_health = Some(0);
    let mut next_publication = Instant::now() + Duration::from_secs(1);
    loop {
        if auth.native_core() {
            if Instant::now() >= next_publication {
                if matches!(auth.retry_native_publication(), Ok(true)) {
                    log.line("Recovered the native PlayOnline session status cache.");
                }
                next_publication = Instant::now() + Duration::from_secs(1);
            }
            if let Some(line) = auth.take_native_batch() {
                if pipe.send(&line, Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
            }
            if let Some((action, groups)) = auth.take_native_control() {
                let line = native_control_line(action, groups);
                if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
            }
            if auth.take_native_activity()
                && pipe.send(b"{\"type\":\"native_activity\"}\n", Instant::now() + Duration::from_secs(10)).is_err() {
                return LoaderEnd::Closed;
            }
        }
        if let Some(line) = auth.take_native_social() {
            if pipe.send(&line, Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
        }
        let health = auth.session_status();
        if !auth.native_core() && reported_health != Some(health) {
            let line = format!("{{\"type\":\"session_status\",\"code\":{health}}}\n");
            if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
            reported_health = Some(health);
            if health < 0 { log.line("The authenticated POL chat session ended; the game was notified."); }
        }
        if let Some(friends) = friends {
            while let Ok(result) = friends.receiver.try_recv() {
                let line = match result {
                    Ok(fetched) => {
                        log.line(&format!("Retrieved {} POL friend records.", fetched.snapshot.entries.len()));
                        fetched.line
                    }
                    Err(_) => {
                        log.line("Could not refresh POL friends; the previous list was kept.");
                        Zeroizing::new("{\"type\":\"friends_result\",\"ok\":false}\n".into())
                    }
                };
                if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
            }
        }
        match pipe.next_event(Some(Instant::now() + Duration::from_millis(250))) {
            Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => continue,
            Err(_) => {
                log.line("The session pipe failed.");
                return LoaderEnd::Closed;
            }
            Ok(Err(message)) => log.line(&message),
            Ok(Ok(event)) => {
                if auth.native_core() && matches!(event, LoaderEvent::Authenticated) {
                    game.authenticated(auth, log);
                }
                // A Play whose sign-in outlasted the start wait: the idle session arrives here.
                if auth.start_pending() && matches!(event, LoaderEvent::NativeSessionState(false)) {
                    let _ = handle_event(event, pipe, auth, log);
                    if start_deferred(pipe, auth, log, game, &mut |_| {}).is_err() {
                        return LoaderEnd::Closed;
                    }
                    continue;
                }
                if let Step::End(end) = handle_event(event, pipe, auth, log) {
                    return end;
                }
            }
        }
    }
}

/// Network work stays off the game pipe pump and UI. The same authenticated
/// session supplies the launcher and native game; no second account login.
struct FriendService {
    receiver: std::sync::mpsc::Receiver<Result<crate::friends::Fetched, String>>,
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl FriendService {
    fn start<A: SignedIn>(auth: &Arc<A>, directory: PathBuf) -> Result<Self, String> {
        let auth = Arc::downgrade(auth);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (stop, stopping) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("pol-friends".into()).spawn(move || {
            let mut snapshot = crate::friends::Snapshot::default();
            let mut due = Instant::now();
            let mut revision=0;
            let mut was_connected=true;
            let mut pending=None;
            loop {
                if stopping.try_recv().is_ok() { return; }
                let Some(auth) = auth.upgrade() else { return };
                let connected=auth.session_status()==0;
                let mut changed=false;
                if Instant::now() >= due {
                    let result = auth.fetch_friends();
                    match &result {
                        Ok(fetched) => snapshot = fetched.snapshot.clone(),
                        Err(error) => snapshot.error = Some(error.clone()),
                    }
                    pending=Some(result);
                    changed=true;
                    due = Instant::now() + Duration::from_secs(30);
                }
                if connected {
                    if let Some((next,fetched))=auth.friend_updates(revision) {
                        revision=next; snapshot=fetched.snapshot.clone(); pending=Some(Ok(fetched)); changed=true;
                    }
                } else if was_connected {
                    for friend in &mut snapshot.entries { friend.online=None; friend.location=None; }
                    snapshot.error=Some(t("The PlayOnline connection ended; live friend status is unavailable.").into());
                    changed=true;
                }
                drop(auth);
                was_connected=connected;
                if changed { let _=crate::session_worker::publish_friends(&directory,&snapshot); }
                if let Some(result)=pending.take() {
                    match sender.try_send(result) {
                        Ok(())=>{},
                        Err(std::sync::mpsc::TrySendError::Full(result))=>pending=Some(result),
                        Err(std::sync::mpsc::TrySendError::Disconnected(_))=>return,
                    }
                }
                match stopping.recv_timeout(Duration::from_millis(250)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }).map_err(|_| t("Could not start the friends service."))?;
        Ok(Self { receiver, stop, thread: Some(thread) })
    }
}
impl Drop for FriendService {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

fn accept_failure(p: &Prepared, error: AcceptError, timeout: Duration) -> String {
    let hint = match p.tool {
        Tool::Vanilla => tf(
            "Check that Crystal's game host {loader} can run. Windows may have blocked or removed it.",
            &[("loader", &p.loader.display())],
        ),
        Tool::Ashita => tf(
            "Check that {exe} in {folder} started (accept its administrator prompt) and that it can run {loader}.",
            &[("exe", &Tool::Ashita.exe()), ("folder", &p.tool_dir.display()), ("loader", &p.loader.display())],
        ),
        Tool::Windower => tf(
            "Check that Windower in {folder} started (it may be updating itself) and that it can run {loader}.",
            &[("folder", &p.tool_dir.display()), ("loader", &p.loader.display())],
        ),
    };
    match error {
        AcceptError::TimedOut => tf("crystal-loader did not connect within {seconds} s. {hint}", &[("seconds", &timeout.as_secs()), ("hint", &hint)]),
        AcceptError::Check(message) => tf("{message} {hint}", &[("message", &message), ("hint", &hint)]),
        AcceptError::Io(e) => tf("The session pipe failed while waiting for crystal-loader ({error}).", &[("error", &e)]),
    }
}

type Spawner<'a> = dyn FnMut(&Prepared, &str, &mut SessionLog) -> Result<Injector, String> + 'a;

struct HandoffTiming {
    publication: Duration,
    transfer: Duration,
    to_session: Duration,
    total: Duration,
}

/// Publish capabilities before transferring the session to the verified loader.
fn deliver_handoff(
    log: &mut SessionLog,
    mut publish: impl FnMut(&mut SessionLog),
    send: impl FnOnce(&mut SessionLog) -> Result<Instant, String>,
) -> Result<HandoffTiming, String> {
    let began = Instant::now();
    publish(log);
    let publication = began.elapsed();
    let transfer_started = Instant::now();
    let sent_at = send(log)?;
    Ok(HandoffTiming {
        publication,
        transfer: sent_at.saturating_duration_since(transfer_started),
        to_session: sent_at.saturating_duration_since(began),
        total: began.elapsed(),
    })
}

/// After sign-in: start the injector, hand the loader its session, wait for the game to
/// start, then leave the rest of the session (and the keepalive) to a supervisor thread.
fn run<A: SignedIn>(
    mut p: Prepared,
    auth: A,
    mut timing: Timeline,
    progress: &mut dyn FnMut(&str),
    spawn: &mut Spawner<'_>,
    turns: &'static Turns,
    limits: Timeouts,
) -> Result<LaunchResult, String> {
    let auth = Arc::new(auth);
    let game_running = auth.game_running().unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
    let log_path = p.session.log.clone();
    let mut log = SessionLog::open(&log_path)?;
    log.line(&format!("Launcher image: {}; embedded loader SHA-256: {}",
        std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default(),
        tools::embedded::embedded().map_or("none", |(_, hash)| hash)));
    log.line(&format!("POL-less launch through {} in {}; loader {}", p.tool.name(), p.tool_dir.display(), p.loader.display()));
    // The sign-in marks were taken before this log existed; write them now.
    log.timing(&mut timing);
    let fail = |log: &mut SessionLog, message: String| {
        log.line(&message);
        with_log(message, &log_path)
    };

    // Start the supervisor before the game, so a thread failure can't leave a running game
    // without the keepalive's owner.
    let (sender, receiver) = std::sync::mpsc::sync_channel::<(LoaderPipe, Arc<A>, SessionLog, Instant, String, GameVersionCheck)>(1);
    let alive = Arc::new(AtomicBool::new(true));
    let outcome = Arc::new(Mutex::new(None));
    let (tracked, tracked_outcome, session) = (alive.clone(), outcome.clone(), p.session.clone());
    let friends_directory = p.friends_directory.clone();
    let mut game_check = GameVersionCheck::new(&p);
    std::thread::Builder::new()
        .name("retail-loader-session".into())
        .spawn(move || {
            if let Ok((mut pipe, auth, mut log, started, launch_timing, mut game_check)) = receiver.recv() {
                let friends = if auth.native_core() { None } else { friends_directory.and_then(|d| FriendService::start(&auth, d).map_err(|e| log.line(&e)).ok()) };
                let end = watch(&mut pipe, auth.as_ref(), &mut log, friends.as_ref(), &mut game_check);
                drop(pipe);
                drop(friends);
                let report = classify_loader_end(&end, started.elapsed());
                log.line(&format!("Session over: {}. Closing the sign-in session.", report.summary));
                session.record(&noted(&report.summary, &launch_timing));
                if let Ok(mut slot) = tracked_outcome.lock() {
                    *slot = Some(report);
                }
                if let Ok(auth) = Arc::try_unwrap(auth) { log.line(&auth.sign_out()); }
            }
            tracked.store(false, Ordering::Release);
        })
        .map_err(|_| t("Could not start the retail session supervisor."))?;

    let handed = (|| -> Result<(LoaderPipe, u32), String> {
        let turn = turns.enter(p.tool, progress);
        let process_turn = if p.tool == Tool::Windower && std::ptr::eq(turns, &TURNS) {
            Some(crate::session_worker::windower_turn(progress)?)
        } else {
            None
        };
        let mut pipe = LoaderPipe::create().map_err(|e| tf("Could not create the session pipe ({error}).", &[("error", &e)]))?;
        progress(&tf("Starting {tool}...", &[("tool", &p.tool.name())]));
        let injector = spawn(&p, pipe.name(), &mut log)?;
        let mut injector = LaunchedInjector { injector, profile_access: p.profile_access.take(), stop_on_drop: p.tool == Tool::Windower };
        log.line(&format!("Started {}; waiting for crystal-loader on its session pipe.", p.tool.exe()));
        log.reached(&mut timing, Milestone::ToolStarted);
        progress(t("Waiting for crystal-loader to connect..."));
        let tool = p.tool;
        let accepted = pipe.accept(limits.connect, || match injector.injector.exited() {
            Some(Some(code)) if code != 0 => Err(tf("{exe} exited with code {code} before crystal-loader connected.", &[("exe", &tool.exe()), ("code", &code)])),
            Some(None) => Err(tf("{exe} ended before crystal-loader connected.", &[("exe", &tool.exe())])),
            _ => Ok(()),
        });
        if let Err(error) = accepted {
            injector.injector.stop();
            return Err(accept_failure(&p, error, limits.connect));
        }
        let pid = pipe.handshake(&p.loader, limits.hello)?;
        if auth.native_core() && !pipe.supports_native_auth() {
            return Err(t("This crystal-loader cannot perform native POL authentication. Use the loader embedded in this launcher, or update the configured loader path.").into());
        }
        if auth.native_core() && !pipe.supports_native_social() {
            return Err(t("This loader cannot retain a native PlayOnline session. Use the loader embedded in this launcher or update the configured loader path.").into());
        }
        log.line(&format!("crystal-loader connected: PID {pid}, image verified."));
        log.reached(&mut timing, Milestone::Hello);
        // The game's main thread is running, so the tool's handshake is over: the next start
        // may go now, whatever its profile (see `turns`).
        drop(turn);
        drop(process_turn);
        let native = auth.native_core();
        let native_batch = pipe.supports_native_batch();
        let measured = deliver_handoff(&mut log, |log| {
            if native {
                if let Some(directory) = &p.friends_directory {
                    if let Err(error) = crate::session_worker::publish_native_batch(directory, native_batch) {
                        // A status-cache failure remains nonfatal to the native session.
                        log.line(&error);
                    }
                }
            }
        }, |log| {
            if p.tool == Tool::Windower {
                let hello = Instant::now();
                observe_windower_exit(injector, hello, hello + limits.injector_exit, log_path.clone())?;
            }
            progress(t("Handing the session to the game..."));
            let line = auth.session_line(&p.install_dir)?;
            let line = match &p.ffxi_settings {
                Some(settings) => {
                    let source = if p.tool == Tool::Windower { "Windower profile" } else { "Crystal" };
                    log.line(&format!("The game gets {} {source} settings in-process.", settings.count));
                    if let Some(note) = &settings.note {
                        log.line(note);
                    }
                    ffxi_settings::splice(line, &settings.json)?
                }
                None => line,
            };
            let sent = pipe.send(&line, Instant::now() + limits.send);
            drop(line);
            sent.map_err(|_| t("crystal-loader did not accept the session.").to_owned())?;
            let sent_at = Instant::now();
            log.line("Session sent; launcher copy cleared.");
            log.reached(&mut timing, Milestone::SessionSent);
            Ok(sent_at)
        })?;
        if cfg!(feature = "launcher-startup-diagnostics") {
            log.line(&format!("startup handoff: worker={} loader={pid} publication_us={} transfer_us={} to_session_us={} total_us={} end_host_us={}",
                std::process::id(), measured.publication.as_micros(),
                measured.transfer.as_micros(), measured.to_session.as_micros(), measured.total.as_micros(),
                crate::launch_timing::diagnostic_host_micros().unwrap_or(0)));
        }
        Ok((pipe, pid))
    })();
    let (mut pipe, pid) = handed.map_err(|message| fail(&mut log, message))?;

    let started = Instant::now();
    let deferred = auth.start_pending();
    progress(if deferred { t("Signing in to PlayOnline...") } else { t("Waiting for the game to start...") });
    let ended = |log: &mut SessionLog, end: LoaderEnd| {
        let report = classify_loader_end(&end, started.elapsed());
        fail(log, tf("{summary} ({tool} via crystal-loader).", &[("summary", &report.message()), ("tool", &p.tool.name())]))
    };
    let mut confirmed = match wait_started(&mut pipe, auth.as_ref(), &mut log, limits.start, &mut timing, progress, false, &mut game_check) {
        Ok(confirmed) => confirmed,
        Err(end) => return Err(ended(&mut log, end)),
    };
    // Full native idle is required even if the overlapping version check finished earlier.
    let mut blocked = None;
    if deferred && confirmed {
        match start_deferred(&mut pipe, auth.as_ref(), &mut log, &mut game_check, progress) {
            Ok(Ok(())) => {
                confirmed = match wait_started(&mut pipe, auth.as_ref(), &mut log, limits.start, &mut timing, progress, true, &mut game_check) {
                    Ok(confirmed) => confirmed,
                    Err(end) => return Err(ended(&mut log, end)),
                };
            }
            Ok(Err(message)) => blocked = Some(message),
            Err(()) => return Err(fail(&mut log, t("crystal-loader did not accept the game start.").into())),
        }
    }
    if confirmed && blocked.is_none() && (deferred || !auth.social_only()) {
        log.reached(&mut timing, Milestone::GameStarted);
    }
    let detail = if let Some(message) = &blocked {
        message.clone()
    } else if auth.social_only() && confirmed {
        with_log(tf("Signed in to PlayOnline. Friends are available; start FFXI from this session without another login. Host PID {pid}.", &[("pid", &pid)]), &log_path)
    } else if confirmed {
        with_log(
            tf("Retail authentication completed. Game PID {pid} started through {tool}.", &[("pid", &pid), ("tool", &p.tool.name())]),
            &log_path,
        )
    } else {
        with_log(
            tf("Game PID {pid} is loading through {tool}; start is not yet confirmed.", &[("pid", &pid), ("tool", &p.tool.name())]),
            &log_path,
        )
    };
    let (summary, row_timing) = (timing.summary(), timing.row_summary());
    if sender.send((pipe, auth, log, started, summary, game_check)).is_err() {
        return Err(t("The retail session supervisor could not accept the game session.").into());
    }
    Ok(LaunchResult { pid, detail, alive, game_running, log: log_path, outcome, timing: row_timing })
}

/// The Settings page's view of the tools: what was found where.
#[derive(Clone, Default, Debug)]
pub(crate) struct Detected {
    pub ashita: Option<PathBuf>,
    pub windower: Option<PathBuf>,
    pub loader: Option<PathBuf>,
    pub boot_inis: Vec<String>,
    pub profiles: Vec<String>,
}

pub(crate) fn detect_all(settings: &ToolSettings) -> Detected {
    let ashita = tools::detect(Tool::Ashita, &settings.ashita);
    let windower = tools::detect(Tool::Windower, &settings.windower);
    Detected {
        boot_inis: ashita.as_deref().map(tools::boot_inis).unwrap_or_default(),
        profiles: windower.as_deref().map(tools::windower_profiles).unwrap_or_default(),
        loader: tools::loader_path(&settings.loader),
        ashita,
        windower,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn status_controls_carry_the_groups_choice() {
        use crate::session_worker::NativeControl;
        assert_eq!(native_control_line(NativeControl::Visible, true), "{\"type\":\"native_visible\",\"groups\":true}\n");
        assert_eq!(native_control_line(NativeControl::Away, false), "{\"type\":\"native_away\",\"groups\":false}\n");
        assert_eq!(native_control_line(NativeControl::Invisible, true), "{\"type\":\"native_invisible\",\"groups\":true}\n");
        assert_eq!(native_control_line(NativeControl::SignOut, true), "{\"type\":\"native_sign_out\"}\n");
        assert_eq!(native_control_line(NativeControl::StartGame, false), "{\"type\":\"native_start_game\"}\n");
    }

    #[test]
    fn only_an_idle_native_sign_out_acknowledgment_makes_an_early_zero_exit_clean() {
        struct Auth(bool);
        impl SignedIn for Auth {
            fn native_core(&self) -> bool { self.0 }
            fn session_line(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
            fn forward_presence(&self, _: &[u8]) -> Result<bool, String> { unreachable!() }
        }
        let end = |native, events: Vec<LoaderEvent>| {
            let mut pipe = LoaderPipe::create().unwrap();
            // Diagnostics still know the acknowledged intent when log I/O is unavailable.
            let mut log = SessionLog::default();
            for event in events {
                if let Step::End(end) = handle_event(event, &mut pipe, &Auth(native), &mut log) {
                    return end;
                }
            }
            panic!("fixture did not report an end");
        };
        let idle = || LoaderEvent::NativeSessionState(false);
        let sign_out = |code| LoaderEvent::NativeControlResult { action: "sign_out".into(), code };
        assert_eq!(end(true, vec![idle(), sign_out(0), LoaderEvent::Exit(0)]), LoaderEnd::SignedOut);
        assert_eq!(end(true, vec![LoaderEvent::NativeSessionState(true), idle(), sign_out(0), LoaderEvent::Exit(0)]), LoaderEnd::SignedOut);
        assert_eq!(end(false, vec![idle(), sign_out(0), LoaderEvent::Exit(0)]), LoaderEnd::Exit(0));

        for events in [
            vec![LoaderEvent::Exit(0)],
            vec![idle(), LoaderEvent::Exit(0)],
            vec![sign_out(0), LoaderEvent::Exit(0)],
            vec![sign_out(0), idle(), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(-256), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), sign_out(-256), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeSessionState(true), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::Started { pid: None }, LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeControlResult { action: "start_game".into(), code: 0 }, LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeStatus(-1), LoaderEvent::Exit(0)],
        ] {
            assert_eq!(end(true, events), LoaderEnd::Exit(0));
        }
        assert_eq!(end(true, vec![idle(), sign_out(0), LoaderEvent::Exit(-14)]), LoaderEnd::Exit(-14));
        let started = || LoaderEvent::Started { pid: Some(std::process::id()) };
        let returned = |successful| LoaderEvent::NativeGameEnded { code: -14, game_returned: successful };
        assert_eq!(end(true, vec![started(), returned(true), idle(), sign_out(0), LoaderEvent::Exit(-14)]), LoaderEnd::SignedOut);
        for events in [
            vec![returned(true), idle(), sign_out(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(false), idle(), sign_out(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(true), idle(), sign_out(-256), LoaderEvent::Exit(-14)],
            vec![started(), LoaderEvent::NativeStatus(-1025), returned(true), idle(), sign_out(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(true), idle(), LoaderEvent::Exit(-14)],
        ] { assert_eq!(end(true, events), LoaderEnd::Exit(-14)); }
        assert_eq!(end(true, vec![idle(), sign_out(0), LoaderEvent::Closed]), LoaderEnd::Closed);
        assert_eq!(end(true, vec![idle(), sign_out(0), LoaderEvent::Error("sign-out failed".into())]), LoaderEnd::Error("sign-out failed".into()));
    }

    use std::io::{BufRead, BufReader, Read};
    use std::sync::atomic::AtomicUsize;

    struct FakeAuth {
        presence: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }
    impl SignedIn for FakeAuth {
        fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
            let secrets = pipe::SessionSecrets {
                v_key: Zeroizing::new([7; 16]),
                auth_code: Zeroizing::new([9; 52]),
                pol_records: None,
                content_ids: vec![],
                account_id: Zeroizing::new(0x9182_7364_5546_3728),
            };
            pipe::session_line("US", install_dir, &secrets)
        }
        /// The first update changes something; later ones fail, to exercise ok:false.
        fn forward_presence(&self, payload: &[u8]) -> Result<bool, String> {
            if self.presence.fetch_add(payload.len(), Ordering::SeqCst) == 0 {
                Ok(true)
            } else {
                Err("presence arguments refused (-7190)".into())
            }
        }
        fn live_credentials_line(&self) -> Result<Option<Zeroizing<String>>, String> {
            Ok(Some(credentials_line(&[11; 16], &[13; 52])))
        }
    }
    impl Drop for FakeAuth {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn pushed_friends_survive_backpressure_and_disconnect_clears_cached_presence() {
        struct FriendsAuth {
            generation: AtomicUsize,
            disconnected: AtomicBool,
        }
        impl FriendsAuth {
            fn fetched(&self) -> crate::friends::Fetched {
                let online=self.generation.load(Ordering::SeqCst)==0;
                crate::friends::Fetched {
                    snapshot: crate::friends::Snapshot {
                        entries: vec![crate::friends::Friend { name:"Alice".into(),alternate:false,slot:1,uid:None,
                            online:Some(online),location:online.then(||"Vana'diel".into()),character:None }],
                        fetched_at:crate::friends::now(), ..crate::friends::Snapshot::default()
                    },
                    line:Zeroizing::new("synthetic friend update".into()),
                }
            }
        }
        impl SignedIn for FriendsAuth {
            fn session_line(&self,_:&Path)->Result<Zeroizing<Vec<u8>>,String> { unreachable!() }
            fn forward_presence(&self,_:&[u8])->Result<bool,String> { unreachable!() }
            fn fetch_friends(&self)->Result<crate::friends::Fetched,String> { Ok(self.fetched()) }
            fn friend_updates(&self,since:u64)->Option<(u64,crate::friends::Fetched)> {
                let next=self.generation.load(Ordering::SeqCst) as u64;
                (next!=since).then(||(next,self.fetched()))
            }
            fn session_status(&self)->i32 { if self.disconnected.load(Ordering::SeqCst) { -1025 } else { 0 } }
        }
        let directory=std::env::temp_dir().join(format!("crystal-push-test-{}",crate::backend::unique_id().unwrap()));
        let auth=Arc::new(FriendsAuth { generation:AtomicUsize::new(0),disconnected:AtomicBool::new(false) });
        let service=FriendService::start(&auth,directory.clone()).unwrap();
        let cache=directory.join("friends.json");
        let read=||std::fs::read(&cache).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok());
        let until=|predicate:&dyn Fn()->bool| {
            let deadline=Instant::now()+Duration::from_secs(5);
            while !predicate() && Instant::now()<deadline { std::thread::sleep(Duration::from_millis(10)); }
            assert!(predicate());
        };
        until(&||read().is_some_and(|v|v["snapshot"]["entries"][0]["online"]==true));
        // Keep the initial result queued while the next push arrives.
        auth.generation.store(1,Ordering::SeqCst);
        until(&||read().is_some_and(|v|v["snapshot"]["entries"][0]["online"]==false));
        assert_eq!(service.receiver.recv_timeout(Duration::from_secs(2)).unwrap().unwrap().snapshot.entries[0].online,Some(true));
        assert_eq!(service.receiver.recv_timeout(Duration::from_secs(2)).unwrap().unwrap().snapshot.entries[0].online,Some(false));
        auth.disconnected.store(true,Ordering::SeqCst);
        until(&||read().is_some_and(|v|v["snapshot"]["entries"][0]["online"].is_null() && v["snapshot"]["error"].is_string()));
        assert!(read().unwrap()["snapshot"]["entries"][0]["location"].is_null());
        drop(service);
        std::fs::remove_file(cache).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    /// The shared session codec and launcher encoder agree on the session contract.
    #[test]
    fn session_json_matches_the_loader_contract() {
        use crystal_pol::retail::{LoaderSession, Region, CONTRACT_VERSION, PRESENCE_ARGS_LEN};
        assert_eq!(CONTRACT_VERSION, pipe::PROTOCOL);
        assert_eq!(PRESENCE_ARGS_LEN, 24);
        let session = LoaderSession {
            region: Region::Us,
            lang: Region::Us,
            install_dir: Some(r"C:\Games\FINAL FANTASY XI".into()),
            v_key: [7; 16],
            auth_code: [9; 52],
            pol_records: Some(vec![1, 2, 3, 4, 5]),
            content_ids: vec![1234567, 7654321],
            account_id: 0x9182_7364_5546_3728,
        };
        let theirs: serde_json::Value = serde_json::from_str(&session.to_contract_json()).unwrap();
        let ours = pipe::session_line(
            "US",
            Path::new(r"C:\Games\FINAL FANTASY XI"),
            &pipe::SessionSecrets {
                v_key: Zeroizing::new([7; 16]),
                auth_code: Zeroizing::new([9; 52]),
                pol_records: Some(Zeroizing::new(vec![1, 2, 3, 4, 5])),
                content_ids: vec![1234567, 7654321],
                account_id: Zeroizing::new(0x9182_7364_5546_3728),
            },
        )
        .unwrap();
        let ours: serde_json::Value = serde_json::from_slice(&ours).unwrap();
        assert_eq!(theirs, ours);
    }

    #[test]
    fn presence_results_follow_the_contract() {
        let parse = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(parse(presence_result(&Ok(false))), serde_json::json!({"type":"presence_result","ok":true,"changed":false}));
        assert_eq!(
            parse(presence_result(&Err("say \"hi\"\n".into()))),
            serde_json::json!({"type":"presence_result","ok":false,"error":"say \"hi\"\n"})
        );
        assert!(!presence_result(&Err("a\nb".into())).contains('\n'), "one line on the wire");
    }

    fn fixture(tool: Tool) -> (Prepared, PathBuf) {
        let dir = std::env::temp_dir().join(format!("xi-pol-less-{}", unique_id().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let prepared = Prepared {
            tool,
            tool_dir: dir.clone(),
            loader: std::env::current_exe().unwrap(),
            install_dir: PathBuf::from(r"C:\FFXI"),
            region: "US",
            ini_name: "crystal-test.ini".into(),
            base_ini: None,
            profile: String::new(),
            ffxi_settings: None,
            profile_access: None,
            session: Session { history: dir.join("history.log"), label: "Fake".into(), log: dir.join("a.crystal.log") },
            friends_directory: None,
            check: |_| Ok("Game version matches.".into()),
        };
        (prepared, dir)
    }

    const FAST: Timeouts = Timeouts {
        connect: Duration::from_millis(400),
        hello: Duration::from_secs(5),
        send: Duration::from_secs(5),
        start: Duration::from_secs(5),
        injector_exit: Duration::from_millis(10),
    };

    #[test]
    fn handoff_publishes_before_transferring_to_the_verified_loader() {
        use std::sync::mpsc;
        let (prepared, dir) = fixture(Tool::Ashita);
        let mut log = SessionLog::open(&prepared.session.log).unwrap();
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (received_tx, received_rx) = mpsc::channel();
        let loader = std::thread::spawn(move || {
            let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
            let mut writer = file.try_clone().unwrap();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true}}"#,
                std::process::id()).unwrap();
            let mut session = String::new();
            BufReader::new(file).read_line(&mut session).unwrap();
            received_tx.send(session).unwrap();
            // Early progress must remain readable after the durable publication.
            writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
        });
        pipe.accept(FAST.hello, || Ok(())).unwrap();
        assert_eq!(pipe.handshake(&prepared.loader, FAST.hello).unwrap(), std::process::id());
        assert!(pipe.supports_native_auth() && pipe.supports_native_social());
        let (publishing_tx, publishing_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let measured = std::thread::scope(|scope| {
            let (pipe_ref, log_ref) = (&mut pipe, &mut log);
            let handoff = scope.spawn(move || deliver_handoff(log_ref, |_| {
                publishing_tx.send(()).unwrap();
                release_rx.recv_timeout(FAST.hello).unwrap();
            }, |_| {
                let line = Zeroizing::new(b"private synthetic session\n".to_vec());
                let sent = pipe_ref.send(&line, Instant::now() + FAST.send);
                drop(line);
                sent.map_err(|error| error.to_string())?;
                Ok(Instant::now())
            }));
            publishing_rx.recv_timeout(FAST.hello).unwrap();
            let early = received_rx.recv_timeout(Duration::from_millis(30));
            // Always release the private publisher before assertions or joining.
            release_tx.send(()).unwrap();
            assert!(early.is_err(), "the session must wait for capability publication");
            let received = early.unwrap_or_else(|_| received_rx.recv_timeout(FAST.hello).unwrap());
            assert_eq!(received, "private synthetic session\n");
            handoff.join().unwrap().unwrap()
        });
        loader.join().unwrap();
        assert!(matches!(pipe.next_event(Some(Instant::now() + FAST.hello)).unwrap().unwrap(), LoaderEvent::Authenticated));
        assert!(measured.total >= measured.publication);
        assert!(!fs::read_to_string(&prepared.session.log).unwrap().contains("private synthetic session"));
        drop(log);
        drop(pipe);
        fs::remove_dir_all(dir).unwrap();
    }


    #[test]
    fn handoff_reports_failed_sends_and_keeps_publication_failure_nonfatal() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let mut log = SessionLog::open(&prepared.session.log).unwrap();
        let mut published = false;
        let error = deliver_handoff(&mut log, |_| published = true,
            |_| Err("private send failed".into())).err().unwrap();
        assert_eq!(error, "private send failed");
        assert!(published, "the capability is published before the transfer attempt");

        let directory = dir.join("unavailable-status");
        // There is no worker record: the actual publication API must fail, but a
        // successful handoff still owns its session and retains the error in the log.
        let result = deliver_handoff(&mut log, |log| {
            let error = crate::session_worker::publish_native_batch(&directory, true).unwrap_err();
            log.line(&error);
        }, |_| Ok(Instant::now()));
        assert!(result.is_ok());
        assert!(fs::read_to_string(&prepared.session.log).unwrap().contains("session status"));
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }

    fn wait_for(flag: &AtomicBool, value: bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while flag.load(Ordering::SeqCst) != value {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The file's text once it contains `what` (another thread writes it).
    fn path_wait(path: &Path, what: &str) -> String {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let text = fs::read_to_string(path).unwrap_or_default();
            if text.contains(what) {
                return text;
            }
            assert!(Instant::now() < end, "{what} never appeared: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A gate of its own, so parallel tests don't wait on each other's turns.
    fn test_turns() -> &'static Turns {
        Box::leak(Box::new(Turns::new()))
    }

    /// Counts how many of something are in progress at once, and the most ever seen.
    #[derive(Default)]
    struct Overlap {
        now: AtomicUsize,
        most: AtomicUsize,
    }
    impl Overlap {
        fn begin(&self) {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
        }
        fn end(&self) {
            self.now.fetch_sub(1, Ordering::SeqCst);
        }
        fn most(&self) -> usize {
            self.most.load(Ordering::SeqCst)
        }
    }

    /// Launch `count` accounts at once through `tool`, the way the app does (one worker thread
    /// per account), with a fake sign-in and a fake tool whose loader says hello after
    /// `handshake`. Returns (most sign-ins at once, most tool handshakes at once).
    fn launch_batch(tool: Tool, count: usize, sign_in_time: Duration, handshake: Duration) -> (usize, usize) {
        let turns = test_turns();
        let signing = Arc::new(Overlap::default());
        let shaking = Arc::new(Overlap::default());
        let done = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..count)
            .map(|_| {
                let (signing, shaking, done) = (signing.clone(), shaking.clone(), done.clone());
                std::thread::spawn(move || {
                    let (prepared, dir) = fixture(tool);
                    let sign_in = |_: &mut Timeline, _: &mut dyn FnMut(&str)| {
                        signing.begin();
                        std::thread::sleep(sign_in_time);
                        signing.end();
                        Ok(FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicBool::new(false)) })
                    };
                    let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                        shaking.begin();
                        let (name, shaking, done) = (pipe.to_owned(), shaking.clone(), done.clone());
                        std::thread::spawn(move || {
                            std::thread::sleep(handshake);
                            let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                            let mut writer = file.try_clone().unwrap();
                            // The handshake ends with the hello: count it over before sending it.
                            shaking.end();
                            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
                            let mut session = String::new();
                            BufReader::new(file).read_line(&mut session).unwrap();
                            writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                            while !done.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                        });
                        Ok(Injector::Detached)
                    };
                    let result = sign_in_then_run(prepared, sign_in, Timeline::begin(), &mut |_| {}, &mut spawn, turns, FAST);
                    (result, dir)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        done.store(true, Ordering::SeqCst);
        for (result, dir) in results {
            let result = result.unwrap();
            wait_for(&result.alive, false);
            let _ = fs::remove_dir_all(dir);
        }
        (signing.most(), shaking.most())
    }

    /// `launch_batch` gives every account the same (default) profile; profiles no longer change
    /// the order (see `another_windower_profile_starts_at_the_previous_hello`).
    #[test]
    fn launch_all_with_windower_signs_in_together_and_hands_over_one_at_a_time() {
        let started = Instant::now();
        let (sign_ins, handshakes) = launch_batch(Tool::Windower, 4, Duration::from_millis(400), Duration::from_millis(150));
        assert_eq!(sign_ins, 4, "every sign-in runs at once");
        assert_eq!(handshakes, 1, "Windower handshakes never overlap");
        // Sign-ins overlap and each turn ends at the hello: about 0.4 s + 4 x 0.15 s, where
        // signing in or holding the turn one account at a time would take 2.2 s or more.
        assert!(started.elapsed() < Duration::from_millis(2000), "{:?}", started.elapsed());
    }

    #[test]
    fn launch_all_with_ashita_starts_every_tool_at_once() {
        // The fake loader connects within FAST's 400 ms connect bound.
        for tool in [Tool::Ashita, Tool::Vanilla] {
            let (sign_ins, handshakes) = launch_batch(tool, 3, Duration::from_millis(200), Duration::from_millis(300));
            assert_eq!(sign_ins, 3);
            assert_eq!(handshakes, 3, "{} starts don't wait for each other", tool.name());
        }
    }

    /// A Windower start with another profile no longer waits for the previous Windower.exe to
    /// exit: each game gets its own profile's settings in-process, so it goes at the previous
    /// start's hello, and both games run side by side with their own `ffxi_settings`.
    #[test]
    fn another_windower_profile_starts_at_the_previous_hello() {
        let turns = test_turns();
        let windower_exited = Arc::new(AtomicBool::new(false));
        let first_done = Arc::new(AtomicBool::new(false));
        let slow_exit = Timeouts { injector_exit: Duration::from_secs(20), ..FAST };
        let (session_tx, session_rx) = std::sync::mpsc::channel::<(String, String)>();
        let start = |profile: &'static str, width: u32, exited: Arc<AtomicBool>, done: Arc<AtomicBool>| {
            let session_tx = session_tx.clone();
            std::thread::spawn(move || {
                let (mut prepared, dir) = fixture(Tool::Windower);
                prepared.profile = profile.into();
                let values = [("0001", ffxi_settings::Value::Dword(width))];
                prepared.ffxi_settings = Some(ffxi_settings::WindowerSettings { json: ffxi_settings::to_json(&values), count: 1, note: None });
                let auth = FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicBool::new(false)) };
                let spawned = Arc::new(Mutex::new(None::<Instant>));
                let when = spawned.clone();
                let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                    *when.lock().unwrap() = Some(Instant::now());
                    let (name, done, session_tx) = (pipe.to_owned(), done.clone(), session_tx.clone());
                    std::thread::spawn(move || {
                        let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                        let mut writer = file.try_clone().unwrap();
                        writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
                        let mut session = String::new();
                        BufReader::new(file).read_line(&mut session).unwrap();
                        session_tx.send((profile.to_owned(), session)).unwrap();
                        writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                        while !done.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                    });
                    Ok(Injector::Fake(exited.clone()))
                };
                let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, turns, slow_exit).unwrap();
                let spawned = spawned.lock().unwrap().unwrap();
                (result, spawned, dir)
            })
        };
        let began = Instant::now();
        let first = start("Main", 1920, windower_exited.clone(), first_done.clone());
        let (first_result, _, first_dir) = first.join().unwrap();
        let first_hello = Instant::now();
        let (second_done, second_exited) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let second = start("Mule", 1280, second_exited.clone(), second_done.clone());
        let (second_result, second_spawned, second_dir) = second.join().unwrap();
        // Main's Windower.exe never exited (it would hold a waiting start up to 20 s), yet the
        // Mule start spawned at once and both games are running.
        assert!(!windower_exited.load(Ordering::SeqCst));
        assert!(second_spawned.duration_since(first_hello) < Duration::from_secs(3), "{:?}", second_spawned.duration_since(first_hello));
        assert!(began.elapsed() < Duration::from_secs(10));
        assert!(first_result.alive.load(Ordering::SeqCst) && second_result.alive.load(Ordering::SeqCst));
        // Each game got its own profile's values.
        let mut sessions: Vec<(String, serde_json::Value)> =
            session_rx.try_iter().map(|(p, s)| (p, serde_json::from_str(&s).unwrap())).collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sessions.len(), 2);
        assert_eq!((sessions[0].0.as_str(), &sessions[0].1["ffxi_settings"]), ("Main", &serde_json::json!({"0001": 1920})));
        assert_eq!((sessions[1].0.as_str(), &sessions[1].1["ffxi_settings"]), ("Mule", &serde_json::json!({"0001": 1280})));
        assert_eq!(sessions[0].1["v_key"], sessions[1].1["v_key"], "the rest of the line is unchanged");
        windower_exited.store(true, Ordering::SeqCst);
        second_exited.store(true, Ordering::SeqCst);
        // The exit is still noted in the launch log, for timing evidence.
        let first_log = path_wait(&first_dir.join("a.crystal.log"), "Windower.exe exited ");
        assert!(first_log.contains(" s after the loader's hello."), "{first_log}");
        assert!(first_log.contains("1 Windower profile settings in-process"), "{first_log}");
        for (result, done, dir) in [(first_result, first_done, first_dir), (second_result, second_done, second_dir)] {
            done.store(true, Ordering::SeqCst);
            wait_for(&result.alive, false);
            let _ = fs::remove_dir_all(dir);
        }
    }

    /// A fake loader that connects, says hello, reads the session, reports `started` and a
    /// presence update, then exits when told.
    #[test]
    fn full_session_runs_to_exit_and_records_the_outcome() {
        for tool in [Tool::Windower, Tool::Vanilla] { check_full_session(tool); }
    }

    fn check_full_session(tool: Tool) {
        let (prepared, dir) = fixture(tool);
        let presence = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let auth = FakeAuth { presence: presence.clone(), dropped: dropped.clone() };
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let (session_tx, session_rx) = std::sync::mpsc::channel::<String>();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
        let mut exit_rx = Some(exit_rx);
        let mut spawned_args = String::new();
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            spawned_args = tools::loader_args(pipe, std::process::id());
            let name = pipe.to_owned();
            let exit_rx = exit_rx.take().unwrap();
            let session_tx = session_tx.clone();
            let reply_tx = reply_tx.clone();
            std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
                let mut session = String::new();
                reader.read_line(&mut session).unwrap();
                session_tx.send(session).unwrap();
                writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                // Each presence line is answered, in order, before the loader goes on.
                for n in 0..2 {
                    writeln!(writer, r#"{{"type":"presence","payload":"AAAAAAEAAAACAAAAAwAAAAQAAAAFAAAA"}}"#).unwrap();
                    if n == 0 {
                        let mut refreshed = String::new();
                        reader.read_line(&mut refreshed).unwrap();
                        reply_tx.send(refreshed).unwrap();
                    }
                    let mut reply = String::new();
                    reader.read_line(&mut reply).unwrap();
                    reply_tx.send(reply).unwrap();
                }
                exit_rx.recv().unwrap();
                writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            });
            Ok(Injector::Detached)
        };
        let mut stages = Vec::new();
        let result = run(prepared, auth, Timeline::begin(), &mut |s: &str| stages.push(s.to_owned()), &mut spawn, test_turns(), FAST).unwrap();
        assert_eq!(result.pid, std::process::id());
        assert!(result.detail.contains(&format!("started through {}", tool.name())), "{}", result.detail);
        assert!(result.alive.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst), "the keepalive lives while the game runs");
        assert!(stages.iter().any(|s| s.contains("Waiting for crystal-loader")));
        assert!(!spawned_args.contains("AAAA") && spawned_args.starts_with("--crystal-pipe "));
        let session: serde_json::Value = serde_json::from_str(&session_rx.recv().unwrap()).unwrap();
        assert_eq!(session["install_dir"], r"C:\FFXI");
        let refreshed: serde_json::Value = serde_json::from_str(&reply_rx.recv().unwrap()).unwrap();
        assert_eq!(refreshed["type"], "session_update");
        assert_eq!(pipe::base64_decode(refreshed["v_key"].as_str().unwrap()).unwrap(), [11; 16]);
        assert_eq!(pipe::base64_decode(refreshed["auth_code"].as_str().unwrap()).unwrap(), [13; 52]);
        let first: serde_json::Value = serde_json::from_str(&reply_rx.recv().unwrap()).unwrap();
        assert_eq!(first, serde_json::json!({"type":"presence_result","ok":true,"changed":true}));
        let second: serde_json::Value = serde_json::from_str(&reply_rx.recv().unwrap()).unwrap();
        assert_eq!(second, serde_json::json!({"type":"presence_result","ok":false,"error":"presence arguments refused (-7190)"}));
        exit_tx.send(()).unwrap();
        wait_for(&result.alive, false);
        assert!(dropped.load(Ordering::SeqCst), "the keepalive is dropped when the session ends");
        assert_eq!(presence.load(Ordering::SeqCst), 48);
        let report = result.outcome.lock().unwrap().clone().unwrap();
        assert_eq!(report.summary, "Game exited during startup with code 0");
        let history = fs::read_to_string(dir.join("history.log")).unwrap();
        assert!(history.contains("| Fake | Game exited during startup with code 0 (launch: tool "), "{history}");
        assert!(history.contains(", hello ") && history.contains(", game started "), "{history}");
        assert!(result.timing.starts_with("Game started "), "{}", result.timing);
        let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
        for what in ["tool process started", "loader hello", "session delivered", "game started"] {
            assert!(log.lines().any(|l| l.contains("[launcher] timing +") && l.ends_with(what)), "{what}: {log}");
        }
        assert!(log.contains("image verified") && log.contains("Presence update sent.") && log.contains("not sent: presence arguments refused"), "{log}");
        // The log never carries session material.
        assert!(!log.contains("BwcHBwcH") && !log.contains("CQkJCQkJ"), "{log}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loader_that_never_connects_times_out_with_a_hint() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let dropped = Arc::new(AtomicBool::new(false));
        let auth = FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: dropped.clone() };
        let mut spawn = |_: &Prepared, _: &str, _: &mut SessionLog| Ok(Injector::Detached);
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("crystal-loader did not connect within"), "{error}");
        assert!(error.contains("administrator prompt"), "{error}");
        assert!(error.contains("Log: "), "{error}");
        assert!(dropped.load(Ordering::SeqCst), "a failed launch releases the sign-in");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn injector_failure_stops_the_wait_early() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let auth = FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicBool::new(false)) };
        let mut spawn = |_: &Prepared, _: &str, _: &mut SessionLog| {
            std::process::Command::new("cmd")
                .args(["/c", "exit 3"])
                .spawn()
                .map(Injector::Child)
                .map_err(|e| e.to_string())
        };
        let started = Instant::now();
        let slow = Timeouts { connect: Duration::from_secs(30), ..FAST };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), slow).unwrap_err();
        assert!(error.contains("Ashita-cli.exe exited with code 3"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(20));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loader_error_before_start_fails_the_launch() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let auth = FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicBool::new(false)) };
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let name = pipe.to_owned();
            std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                let mut writer = file.try_clone().unwrap();
                writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
                let mut session = String::new();
                BufReader::new(file).read_line(&mut session).unwrap();
                writeln!(writer, r#"{{"type":"error","message":"FFXiMain.dll could not be loaded"}}"#).unwrap();
            });
            Ok(Injector::Detached)
        };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("Game loader error during startup: FFXiMain.dll could not be loaded"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn impostor_pid_gets_no_session() {
        let (prepared, dir) = fixture(Tool::Windower);
        let auth = FakeAuth { presence: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicBool::new(false)) };
        let (got_tx, got_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let name = pipe.to_owned();
            let got_tx = got_tx.clone();
            std::thread::spawn(move || {
                let mut file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id() + 4).unwrap();
                let mut rest = Vec::new();
                let _ = file.read_to_end(&mut rest);
                got_tx.send(rest).unwrap();
            });
            Ok(Injector::Detached)
        };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("not the one that said hello"), "{error}");
        assert!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_tools_fail_before_sign_in() {
        let dir = std::env::temp_dir().join(format!("xi-pol-less-prep-{}", unique_id().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let config = Config { experiment: dir.clone(), repository: dir.clone(), game_install: Some(dir.clone()), dat_overlays: Vec::new() };
        let account = Account {
            label: "Fake".into(),
            mode: crate::backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "ABCD1234".into(),
            pol_password: "synthetic".into(),
            se_id: "synthetic".into(),
            se_password: "synthetic".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
        };
        let plan = Plan {
            social_only: false,
            invisible_login: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(),
            tool: Tool::Ashita,
            settings: ToolSettings { ashita: dir.join("none").to_string_lossy().into_owned(), ..Default::default() },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("Ashita v4 was not found") || error.contains("pol.exe is missing"), "{error}");
        // A found tool but no loader.
        fs::write(dir.join("Ashita-cli.exe"), b"").unwrap();
        let plan = Plan {
            social_only: false,
            invisible_login: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(),
            tool: Tool::Ashita,
            settings: ToolSettings {
                ashita: dir.to_string_lossy().into_owned(),
                loader: dir.join("missing-loader.exe").to_string_lossy().into_owned(),
                ..Default::default()
            },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("crystal-loader path"), "{error}");
        // Missing either game DLL is caught before authenticating.
        fs::write(dir.join("crystal-loader.exe"), b"").unwrap();
        let plan = Plan {
            social_only: false,
            invisible_login: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(),
            tool: Tool::Ashita,
            settings: ToolSettings {
                ashita: dir.to_string_lossy().into_owned(),
                loader: dir.join("crystal-loader.exe").to_string_lossy().into_owned(),
                ..Default::default()
            },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("has no FFXi.dll"), "{error}");
        fs::write(dir.join("FFXi.dll"), b"").unwrap();
        let error = launch(&config, &plan, account, Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("has no FFXiMain.dll"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn vanilla_preparation_ignores_tool_folders_and_profiles_and_keeps_menu_settings() {
        let root = std::env::temp_dir().join(format!("crystal-vanilla-{}", unique_id().unwrap()));
        let game = root.join("FINAL FANTASY XI");
        fs::create_dir_all(&game).unwrap();
        let loader = root.join("pol.exe");
        fs::write(&loader, []).unwrap();
        let config = Config { experiment: root.clone(), repository: root.clone(),
            game_install: Some(game.clone()), dat_overlays: Vec::new() };
        let account = Account { label: "Fixture".into(), mode: crate::backend::AccountMode::Retail,
            server: String::new(), pol_id: "ABCD1234".into(), pol_password: "synthetic".into(),
            se_id: "synthetic".into(), se_password: "synthetic".into(), region: 0,
            launch_with: None, ashita_boot_ini: None, windower_profile: None };
        let plan = Plan { tool: Tool::Vanilla, social_only: false, invisible_login: false, auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(),
            settings: ToolSettings { loader: loader.to_string_lossy().into_owned(),
                ashita: root.join("missing-ashita").to_string_lossy().into_owned(),
                windower: root.join("missing-windower").to_string_lossy().into_owned(),
                boot_ini: "../invalid.ini".into(), windower_profile: "not installed".into(), ..Default::default() } };
        let error = prepare(&config, &plan, &account).err().unwrap();
        assert!(error.contains("has no FFXi.dll"), "{error}");
        fs::write(game.join("FFXi.dll"), []).unwrap();
        let error = prepare(&config, &plan, &account).err().unwrap();
        assert!(error.contains("has no FFXiMain.dll"), "{error}");
        fs::write(game.join("FFXiMain.dll"), []).unwrap();
        let prepared = prepare(&config, &plan, &account).unwrap();
        assert_eq!(prepared.tool, Tool::Vanilla);
        assert_eq!(prepared.install_dir, game);
        assert!(prepared.base_ini.is_none() && prepared.profile.is_empty() && prepared.ffxi_settings.is_none());
        let menu = root.join("iced-launcher/menu-playonlineus.json");
        fs::create_dir_all(menu.parent().unwrap()).unwrap();
        fs::write(&menu, "[1600,900]").unwrap();
        let prepared = prepare(&config, &plan, &account).unwrap();
        let values: serde_json::Value = serde_json::from_str(&prepared.ffxi_settings.unwrap().json).unwrap();
        assert_eq!(values, serde_json::json!({"0037":1600,"0038":900}));
        assert!(!root.join("missing-ashita").exists() && !root.join("missing-windower").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windower_preparation_preserves_profile_menu_and_settings_files() {
        let root = std::env::temp_dir().join(format!("crystal-windower-profile-{}", unique_id().unwrap()));
        let game = root.join("FINAL FANTASY XI");
        let windower = root.join("Windower");
        fs::create_dir_all(&game).unwrap();
        fs::create_dir_all(windower.join("plugins/settings")).unwrap();
        for dll in ["FFXi.dll", "FFXiMain.dll"] { fs::write(game.join(dll), []).unwrap(); }
        let loader = root.join("pol.exe");
        fs::write(&loader, []).unwrap();
        fs::write(windower.join("Windower.exe"), []).unwrap();
        let profile_file = windower.join("settings.xml");
        let profiles = br#"<settings><profile name=""><width>1920</width><height>1080</height><uiscale>1.5</uiscale><sound>false</sound></profile><profile name="Mule"><width>1280</width><height>720</height><uiscale>1</uiscale></profile></settings>"#;
        fs::write(&profile_file, profiles).unwrap();
        let plugin_file = windower.join("plugins/settings/ffxidb.xml");
        let plugin = br#"<settings><global><Map><X>700</X><Y>300</Y><Scale>1.2</Scale></Map></global></settings>"#;
        fs::write(&plugin_file, plugin).unwrap();
        let menu_file = root.join("iced-launcher/menu-playonlineus.json");
        fs::create_dir_all(menu_file.parent().unwrap()).unwrap();
        let config = Config { experiment: root.clone(), repository: root.clone(),
            game_install: Some(game), dat_overlays: Vec::new() };
        let account = Account { label: "Fixture".into(), mode: crate::backend::AccountMode::Retail,
            server: String::new(), pol_id: "ABCD1234".into(), pol_password: "synthetic".into(),
            se_id: "synthetic".into(), se_password: "synthetic".into(), region: 0,
            launch_with: None, ashita_boot_ini: None, windower_profile: None };
        let mut plan = Plan { tool: Tool::Windower, social_only: false, invisible_login: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(),
            settings: ToolSettings { loader: loader.to_string_lossy().into_owned(),
                windower: windower.to_string_lossy().into_owned(), ..Default::default() } };
        for retained_menu in ["[640,480]", "not valid JSON"] {
            fs::write(&menu_file, retained_menu).unwrap();
            for (profile, window, menu, sound) in [
                ("", (1920, 1080), (1280, 720), 0),
                ("Mule", (1280, 720), (0, 0), 1),
            ] {
                plan.settings.windower_profile = profile.into();
                let prepared = prepare(&config, &plan, &account).unwrap();
                assert_eq!(prepared.profile, profile);
                let values: serde_json::Value = serde_json::from_str(&prepared.ffxi_settings.unwrap().json).unwrap();
                assert_eq!((values["0001"].as_u64(), values["0002"].as_u64()), (Some(window.0), Some(window.1)));
                assert_eq!((values["0037"].as_u64(), values["0038"].as_u64()), (Some(menu.0), Some(menu.1)));
                assert_eq!(values["0007"], sound);
                assert_eq!(fs::read(&profile_file).unwrap(), profiles);
                assert_eq!(fs::read(&plugin_file).unwrap(), plugin);
                assert_eq!(fs::read_to_string(&menu_file).unwrap(), retained_menu);
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    // ---- Play: sign in idle, then the stock FFXI check, then the game start ----

    /// A signed-in native session that a Play will start itself (the parts of `NativeAuth`
    /// the flow depends on), recording what the worker told it.
    struct DeferAuth {
        pending: AtomicBool,
        idle_seen: AtomicBool,
        running: Arc<AtomicBool>,
        outcome: Arc<Mutex<Option<Option<String>>>>,
    }
    impl DeferAuth {
        fn new() -> (Self, Arc<Mutex<Option<Option<String>>>>) {
            let outcome = Arc::new(Mutex::new(None));
            (Self { pending: AtomicBool::new(true), idle_seen: AtomicBool::new(false), running: Arc::new(AtomicBool::new(true)), outcome: outcome.clone() }, outcome)
        }
    }
    impl SignedIn for DeferAuth {
        fn native_core(&self) -> bool { true }
        fn start_pending(&self) -> bool { self.pending.load(Ordering::SeqCst) }
        fn idle_first(&self) -> bool { self.start_pending() }
        fn deferred_start_done(&self, blocked: Option<&str>) {
            assert!(self.idle_seen.load(Ordering::SeqCst), "a check result cannot release Play before full native idle");
            self.pending.store(false, Ordering::SeqCst);
            if blocked.is_some() {
                self.running.store(false, Ordering::SeqCst);
            }
            *self.outcome.lock().unwrap() = Some(blocked.map(str::to_owned));
        }
        fn game_running(&self) -> Option<Arc<AtomicBool>> { Some(self.running.clone()) }
        fn native_session_state(&self, game: bool) -> Result<(), String> {
            if !game { self.idle_seen.store(true, Ordering::SeqCst); }
            if !game && self.start_pending() { return Ok(()); }
            self.running.store(game, Ordering::SeqCst);
            Ok(())
        }
        fn session_line(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
            Ok(Zeroizing::new(format!("{{\"type\":\"session\",\"start_game\":{}}}\n", !self.idle_first()).into_bytes()))
        }
        fn forward_presence(&self, _: &[u8]) -> Result<bool, String> { Err("native".into()) }
    }

    /// A loader for a native Play: hello, read the session, report the idle Viewer session
    /// after `idle_after`, then (`expect_start`) wait for `native_start_game` and report the
    /// game started; exit when `done`, then read whatever else the launcher sent until it closes.
    /// Everything the launcher wrote is pushed to `got`, in order.
    fn deferred_loader(name: String, idle_after: Duration, expect_start: bool, done: Arc<AtomicBool>, got: Arc<Mutex<Vec<String>>>, on_hello: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
        deferred_loader_events(name, expect_start, done, got, on_hello, move |_| std::thread::sleep(idle_after))
    }

    fn deferred_loader_events(name: String, expect_start: bool, done: Arc<AtomicBool>, got: Arc<Mutex<Vec<String>>>, on_hello: impl FnOnce() + Send + 'static, before_idle: impl FnOnce(&mut File) + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
            let mut writer = file.try_clone().unwrap();
            let mut reader = BufReader::new(file);
            on_hello();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true}}"#, std::process::id()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            got.lock().unwrap().push(std::mem::take(&mut line));
            before_idle(&mut writer);
            writeln!(writer, r#"{{"type":"native_session_state","game":false}}"#).unwrap();
            if expect_start {
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"native_session_state","game":true}}"#).unwrap();
            }
            while !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                got.lock().unwrap().push(std::mem::take(&mut line));
            }
        })
    }

    fn lines(got: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        got.lock().unwrap().clone()
    }

    #[test]
    fn authenticated_check_overlaps_native_services_and_survives_both_supervisor_handoffs() {
        use std::sync::mpsc::{self, Receiver, Sender};
        type Signals = (Sender<()>, Receiver<Result<String, String>>, Sender<()>);
        static SIGNALS: Mutex<Option<Signals>> = Mutex::new(None);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        // Hold idle until the check runs: a serialized implementation cannot complete this
        // exchange. Neither a fast success nor refusal may bypass native startup services.
        for (late_auth, late_idle, refused) in [(false, false, false), (false, false, true), (true, false, false), (false, true, false)] {
            CHECKS.store(0, Ordering::SeqCst);
            let (entered_tx, entered_rx) = mpsc::channel();
            let (verdict_tx, verdict_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            *SIGNALS.lock().unwrap() = Some((entered_tx, verdict_rx, finished_tx));
            let (mut prepared, dir) = fixture(Tool::Windower);
            prepared.check = |_| {
                CHECKS.fetch_add(1, Ordering::SeqCst);
                let (entered, verdict, finished) = SIGNALS.lock().unwrap().take().expect("one check only");
                entered.send(()).unwrap();
                let verdict = verdict.recv_timeout(Duration::from_secs(10)).unwrap();
                finished.send(()).unwrap();
                verdict
            };
            let (auth, outcome) = DeferAuth::new();
            let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
            let (resume_tx, resume_rx) = mpsc::channel();
            let (checking_tx, checking_rx) = mpsc::channel();
            let observed = outcome.clone();
            let mut before_idle = Some(move |writer: &mut File| {
                if late_auth { resume_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                // A duplicate milestone must not issue a second version request.
                writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                entered_rx.recv_timeout(Duration::from_secs(10)).expect("the version check must start before native idle");
                checking_tx.send(()).unwrap();
                if late_idle { resume_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                verdict_tx.send(if refused { Err("Update required: fixture version differs.".into()) } else { Ok("matches".into()) }).unwrap();
                finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(observed.lock().unwrap().is_none(), "native services are still pending");
            });
            let mut loader = None;
            let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                loader = Some(deferred_loader_events(pipe.to_owned(), !refused, done.clone(), got.clone(), || {}, before_idle.take().unwrap()));
                Ok(Injector::Detached)
            };
            let limits = if late_auth || late_idle { Timeouts { start: Duration::from_millis(300), ..FAST } } else { FAST };
            let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), limits).unwrap();
            if late_auth || late_idle {
                assert!(result.detail.contains("not yet confirmed"), "{}", result.detail);
                if late_auth { assert_eq!(CHECKS.load(Ordering::SeqCst), 0, "no request before authentication"); }
                if late_idle { checking_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                assert!(outcome.lock().unwrap().is_none());
                resume_tx.send(()).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while outcome.lock().unwrap().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(CHECKS.load(Ordering::SeqCst), 1);
            assert_eq!(outcome.lock().unwrap().as_ref().unwrap().is_some(), refused);
            done.store(true, Ordering::SeqCst);
            wait_for(&result.alive, false);
            loader.unwrap().join().unwrap();
            let sent = lines(&got);
            assert_eq!(sent.iter().filter(|line| line.contains("native_start_game")).count(), usize::from(!refused));
            let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
            assert!(log.find("checking the FFXI version").unwrap() < log.find("Native startup services finished").unwrap());
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn native_auth_or_startup_failure_never_releases_a_checked_game() {
        static CHECKED: AtomicBool = AtomicBool::new(false);
        for authenticated in [false, true] {
            CHECKED.store(false, Ordering::SeqCst);
            let (mut prepared, dir) = fixture(Tool::Windower);
            prepared.check = |_| { CHECKED.store(true, Ordering::SeqCst); Ok("matches".into()) };
            let (auth, outcome) = DeferAuth::new();
            let got = Arc::new(Mutex::new(Vec::new()));
            let mut loader = None;
            let mut spawn = |_: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
                let (name, got) = (name.to_owned(), got.clone());
                loader = Some(std::thread::spawn(move || {
                    let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                    let mut writer = file.try_clone().unwrap();
                    let mut reader = BufReader::new(file);
                    writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true}}"#, std::process::id()).unwrap();
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    got.lock().unwrap().push(std::mem::take(&mut line));
                    if authenticated {
                        writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                        wait_for(&CHECKED, true);
                    }
                    writeln!(writer, r#"{{"type":"error","message":"fixture native startup failed"}}"#).unwrap();
                    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                        got.lock().unwrap().push(std::mem::take(&mut line));
                    }
                }));
                Ok(Injector::Detached)
            };
            let error = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST).err().unwrap();
            assert!(error.contains("fixture native startup failed"), "{error}");
            loader.unwrap().join().unwrap();
            assert_eq!(CHECKED.load(Ordering::SeqCst), authenticated);
            assert!(outcome.lock().unwrap().is_none());
            assert_eq!(lines(&got).len(), 1, "no native_start_game before all native services succeed");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_play_signs_in_idle_then_checks_the_version_and_only_then_starts_the_game() {
        for tool in [Tool::Windower, Tool::Vanilla] { check_native_play(tool); }
    }

    fn check_native_play(tool: Tool) {
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        ORDER.lock().unwrap().clear();
        let (mut prepared, dir) = fixture(tool);
        prepared.check = |_| {
            ORDER.lock().unwrap().push("check");
            Ok("Game version 30260904_1 matches the official server.".into())
        };
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(300), true, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let mut stages = Vec::new();
        let result = run(prepared, auth, Timeline::begin(), &mut |s: &str| stages.push(s.to_owned()), &mut spawn, test_turns(), FAST).unwrap();
        // No check went out before the sign-in finished (the loader's idle report came 300 ms
        // after the session line, and the check ran after it, before the start line).
        let received = lines(&got);
        assert_eq!(received.len(), 2, "{received:?}");
        let session: serde_json::Value = serde_json::from_str(&received[0]).unwrap();
        assert_eq!(session["start_game"], false, "a Play signs in idle");
        assert_eq!(received[1], "{\"type\":\"native_start_game\"}\n");
        assert_eq!(*ORDER.lock().unwrap(), vec!["check"]);
        assert_eq!(*outcome.lock().unwrap(), Some(None), "the start was released");
        assert!(result.game_running.load(Ordering::SeqCst) && result.alive.load(Ordering::SeqCst));
        assert!(result.detail.contains(&format!("started through {}", tool.name())), "{}", result.detail);
        let position = |what: &str| stages.iter().position(|s| s.contains(what)).unwrap_or_else(|| panic!("{what}: {stages:?}"));
        assert!(position("Signing in to PlayOnline") < position("Checking the game version") && position("Checking the game version") < position("Starting FFXI"));
        let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
        let at = |what: &str| log.find(what).unwrap_or_else(|| panic!("{what}: {log}"));
        assert!(at("session delivered") < at("checking the FFXI version") && at("checking the FFXI version") < at("Game start sent") && at("Game start sent") < at("game started"), "{log}");
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_update_required_check_leaves_the_session_signed_in_without_a_start() {
        let (mut prepared, dir) = fixture(Tool::Windower);
        prepared.check = |_| Err(format!("{} installed 30260904_1, official server 30260905_0. Open Downloads.", crate::patch::UPDATE_REQUIRED));
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(50), false, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap();
        // Reported like a signed-in session with FFXI closed, carrying the update-required text
        // that the account row shows.
        assert!(result.detail.starts_with(crate::patch::UPDATE_REQUIRED), "{}", result.detail);
        let shown = crate::patch::parse_update_required(&result.detail).unwrap();
        assert_eq!((shown.local_version.as_str(), shown.remote_version.as_str()), ("30260904_1", "30260905_0"));
        assert!(!result.game_running.load(Ordering::SeqCst));
        assert!(result.alive.load(Ordering::SeqCst), "the session stays signed in");
        assert!(outcome.lock().unwrap().as_ref().unwrap().as_deref().unwrap().starts_with(crate::patch::UPDATE_REQUIRED));
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        let received = lines(&got);
        assert_eq!(received.len(), 1, "only the session line: {received:?}");
        assert!(!received.iter().any(|l| l.contains("native_start_game")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_play_whose_sign_in_outlasts_the_start_wait_still_checks_then_starts() {
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        let (mut prepared, dir) = fixture(Tool::Windower);
        prepared.check = |_| {
            ORDER.lock().unwrap().push("check");
            Ok("matches".into())
        };
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(900), true, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let slow = Timeouts { start: Duration::from_millis(300), ..FAST };
        let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), slow).unwrap();
        assert!(result.detail.contains("not yet confirmed"), "{}", result.detail);
        assert!(ORDER.lock().unwrap().is_empty(), "no check before the idle session is reported");
        let end = Instant::now() + Duration::from_secs(10);
        while lines(&got).len() < 2 {
            assert!(Instant::now() < end, "the supervisor never started the game: {:?}", lines(&got));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(lines(&got)[1], "{\"type\":\"native_start_game\"}\n");
        assert_eq!(*ORDER.lock().unwrap(), vec!["check"]);
        assert_eq!(*outcome.lock().unwrap(), Some(None));
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    /// Launch all: every account signs in idle and starts after its own check, while Windower
    /// still holds its turn only until the loader's hello.
    #[test]
    fn launch_all_plays_each_start_after_their_own_check_and_windower_keeps_one_turn_at_a_time() {
        let turns = test_turns();
        let shaking = Arc::new(Overlap::default());
        let done = Arc::new(AtomicBool::new(false));
        let began = Instant::now();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (shaking, done) = (shaking.clone(), done.clone());
                std::thread::spawn(move || {
                    let (prepared, dir) = fixture(Tool::Windower);
                    let (auth, _) = DeferAuth::new();
                    let got = Arc::new(Mutex::new(Vec::new()));
                    let mut loader = None;
                    let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                        shaking.begin();
                        let hello = shaking.clone();
                        loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(250), true, done.clone(), got.clone(), move || {
                            std::thread::sleep(Duration::from_millis(120));
                            hello.end();
                        }));
                        Ok(Injector::Detached)
                    };
                    let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, turns, FAST).unwrap();
                    (result, loader.unwrap(), got, dir)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(shaking.most(), 1, "the Windower turn ends at the hello, never later");
        // Four 120 ms hellos one at a time, with the 250 ms idle waits overlapping.
        assert!(began.elapsed() < Duration::from_millis(2500), "{:?}", began.elapsed());
        done.store(true, Ordering::SeqCst);
        for (result, loader, got, dir) in results {
            assert!(result.game_running.load(Ordering::SeqCst), "every account's game started");
            assert!(lines(&got)[1].contains("native_start_game"));
            wait_for(&result.alive, false);
            loader.join().unwrap();
            let _ = fs::remove_dir_all(dir);
        }
    }}
