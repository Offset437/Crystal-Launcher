//! The game session belongs to a headless copy of Crystal, not the iced window.
//! Credentials travel once through anonymous stdin; disk records contain only status.
//! Closing/reopening the window neither signs out nor starts a second session.

use crate::{
    backend::{self, Account, Config, LaunchResult},
    launch_log::ExitReport,
    pol_less::{Plan, Tool},
};
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

const MAX_FRAME: usize = 64 * 1024;
const PREFIX: &str = "CRYSTAL-WORKER ";
#[path = "../crystal-loader/src/start_gate.rs"]
mod start_gate;
#[path = "session_batch.rs"]
mod batch;
pub(crate) use batch::{BatchAccount, batch_pending, launch_together, run_batch};
#[path = "session_wake.rs"]
mod wake;
pub(crate) use wake::CommandWake;

pub(crate) fn listen_for_commands() -> Option<CommandWake> {
    CommandWake::listen(identity(std::process::id())?).ok()
}

#[derive(Serialize, Deserialize)]
struct BatchControl { worker: Identity, token: String }

pub(crate) fn take_native_batch(directory: &Path) -> Option<Vec<u8>> {
    let path = directory.join("native-batch.json");
    let bytes = read_bounded(&path, 1024)?;
    fs::remove_file(&path).ok()?;
    let request: BatchControl = serde_json::from_slice(&bytes).ok()?;
    if Some(request.worker) != identity(std::process::id()) { return None; }
    // Also refuses stale commands whose coordinator canceled or died.
    start_gate::Participant::open(&request.token).ok()?;
    Some(format!("{{\"type\":\"native_batch_game\",\"token\":{}}}\n", serde_json::to_string(&request.token).ok()?).into_bytes())
}

#[derive(Serialize, Deserialize)]
struct Request {
    config: Config,
    account: Account,
    otp: String,
    via: Option<Plan>,
    elapsed_ms: u64,
    #[serde(default)]
    clicked_clock: Option<crate::launch_timing::MonotonicStamp>,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.otp.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    pid: u32,
    created: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Started {
    pid: u32,
    detail: String,
    log: PathBuf,
    timing: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Event {
    Progress(String),
    Started(Started),
    Failed(String),
    Finished(Option<ExitReport>),
}

#[derive(Serialize, Deserialize)]
struct Record {
    worker: Identity,
    state: Event,
    /// Nonsecret intent and milestone; older workers default to no early readiness.
    #[serde(default)]
    social_only: bool,
    #[serde(default)]
    authenticated: bool,
    #[serde(default)]
    native_batch: bool,
    /// A signed-in host keeps its injected tool until it signs out.
    #[serde(default)]
    launch_tool: Option<Tool>,
}

// The login thread and late loader supervisor can both update this process's record.
static RECORD_WRITE: Mutex<()> = Mutex::new(());
static STATUS_WRITE_ID: AtomicU64 = AtomicU64::new(0);

fn record_path(config: &Config, account: &Account) -> PathBuf {
    config.account_directory(account).join("session.json")
}

/// An open handle with deny-sharing is also released if a worker crashes.
fn exclusive(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(path)
}

/// Windower uses fixed named IPC objects: keep spawn-to-hello serialized across workers.
pub(crate) fn windower_turn(progress: &mut dyn FnMut(&str)) -> Result<File, String> {
    let root = std::env::var_os("CRYSTAL_DATA_ROOT")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .ok_or(t("Could not locate the Windower launch lock directory."))?;
    let path = PathBuf::from(root).join("Crystal Launcher/locks/windower.lock");
    let mut announced = false;
    loop {
        match exclusive(&path) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.raw_os_error() == Some(32) => {
                if !announced {
                    progress(t("Waiting for the previous Windower start to reach the game..."));
                    announced = true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Err(t("Could not acquire the Windower launch lock.").into()),
        }
    }
}

#[cfg(windows)]
fn identity(pid: u32) -> Option<Identity> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{
            GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        },
    };
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return None;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } != 258 {
        return None;
    }
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut exited, mut kernel, mut user) = (created, created, created);
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    Some(Identity {
        pid,
        created: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    })
}

#[cfg(not(windows))]
fn identity(_pid: u32) -> Option<Identity> {
    None
}

fn read_record(path: &Path) -> Option<Record> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_FRAME {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn save_record(path: &Path, record: &Record) -> Result<(), String> {
    let _guard = RECORD_WRITE.lock().map_err(|_| "Could not lock the session status.")?;
    save_json(path, record)?;
    Ok(())
}

fn update_record(path: &Path, worker: Identity, update: impl FnOnce(&mut Record)) -> Result<(), String> {
    let _guard = RECORD_WRITE.lock().map_err(|_| "Could not lock the session status.")?;
    let mut record = read_record(path).filter(|r| r.worker == worker)
        .ok_or("The session status belongs to a different worker.")?;
    update(&mut record);
    save_json(path, &record)?;
    Ok(())
}

/// Called only for the native loader's typed authenticated event, never a progress label.
pub(crate) fn publish_authenticated(directory: &Path) -> Result<(), String> {
    let worker = identity(std::process::id()).ok_or("Could not identify the authenticated worker.")?;
    update_record(&directory.join("session.json"), worker, |record| {
        if matches!(record.state, Event::Progress(_) | Event::Started(_)) { record.authenticated = true; }
    })
}

/// Published only after the verified loader hello reports this capability.
pub(crate) fn publish_native_batch(directory: &Path, supported: bool) -> Result<(), String> {
    let worker = identity(std::process::id()).ok_or("Could not identify the session worker.")?;
    update_record(&directory.join("session.json"), worker, |record| {
        if matches!(record.state, Event::Progress(_) | Event::Started(_)) { record.native_batch = supported; }
    })
}

fn save_json(path: &Path, record: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(|_| t("Could not encode the session status."))?;
    // A shared .tmp name lets concurrent writers replace or rename each other's data.
    // Each publication owns its staging file, including cleanup after a failed replace.
    let (temporary, mut file) = loop {
        let mut name = path.file_name().ok_or_else(|| t("Could not save the session status.").to_owned())?.to_os_string();
        name.push(format!(".{}.{}.tmp", std::process::id(), STATUS_WRITE_ID.fetch_add(1, Ordering::Relaxed)));
        let temporary = path.with_file_name(name);
        match OpenOptions::new().write(true).create_new(true).open(&temporary) {
            Ok(file) => break (PendingStatusFile(temporary), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(status_write_error(t("Could not save the session status."), path, error)),
        }
    };
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| status_write_error(t("Could not save the session status."), path, error))?;
    drop(file);
    replace_status_file(&temporary.0, path)
        .map_err(|error| status_write_error(t("Could not replace the session status."), path, error))
}

struct PendingStatusFile(PathBuf);
impl Drop for PendingStatusFile {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

fn status_write_error(message: &str, path: &Path, error: io::Error) -> String {
    // These files contain only status; naming the file and OS error makes a failed
    // publication actionable without logging any account or session payload.
    format!("{message} {}: {error}", path.display())
}

fn replace_status_file(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let wide_path = |path: &Path| -> io::Result<Vec<u16>> {
            let filename = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing status filename"))?;
            let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
            // Windows canonicalization supplies the extended drive/UNC prefix.
            // Resolve only the parent: the target need not exist, and replacing
            // a link must replace that directory entry rather than its referent.
            let absolute = fs::canonicalize(parent)?.join(filename);
            Ok(absolute.as_os_str().encode_wide().chain(Some(0)).collect())
        };
        let source = wide_path(source)?;
        let target = wide_path(target)?;
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut pause = Duration::from_millis(5);
        loop {
            if unsafe { MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ) } != 0 { return Ok(()); }
            let error = io::Error::last_os_error();
            // A scanner or reader without delete sharing may briefly hold the target.
            // Permanent permission failures still return promptly and keep the old file.
            if !matches!(error.raw_os_error(), Some(5 | 32 | 33)) || Instant::now() >= deadline {
                return Err(error);
            }
            std::thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }
    #[cfg(not(windows))]
    fs::rename(source, target)
}

#[derive(Serialize, Deserialize)]
struct FriendCache { worker: Identity, snapshot: crate::friends::Snapshot }

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeControl { StartGame, SignOut, Visible, Away, Invisible }
impl NativeControl {
    pub(crate) fn name(self) -> &'static str {
        match self { Self::StartGame => "start_game", Self::SignOut => "sign_out", Self::Visible => "visible", Self::Away => "away", Self::Invisible => "invisible" }
    }
}
/// `groups`: a status change also goes to my groups (Viewer 10040f39, 404
/// when 385() > 0). The Viewer's default for its checkbox is not verified.
#[derive(Serialize, Deserialize)]
struct ControlRequest { worker: Identity, action: NativeControl, #[serde(default = "yes")] groups: bool }
fn yes() -> bool { true }

#[derive(Serialize, Deserialize)]
pub(crate) struct SocialRequest {
    worker: Identity,
    pub id: String,
    pub request: crate::social::Request,
}
impl SocialRequest {
    pub fn line(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(64 * 1024));
        bytes.extend_from_slice(b"{\"type\":\"native_social\",\"id\":\"");
        bytes.extend_from_slice(self.id.as_bytes());
        bytes.extend_from_slice(b"\",");
        // English: it lands in the social state, which `social::localize` shows.
        let request = Zeroizing::new(serde_json::to_vec(&self.request).map_err(|_| "Could not encode the PlayOnline request.")?);
        bytes.extend_from_slice(&request[1..]);
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Publish one account-bound request atomically. A second window cannot replace
/// a pending send, and an old process's request cannot reach a new session.
pub(crate) fn request_social(config: &Config, account: &Account, request: crate::social::Request) -> Result<String, String> {
    let _batch = exclusive(&config.account_directory(account).join("batch.lock"))
        .map_err(|_| "This account is waiting for its batch to start.")?;
    if !request.valid() { return Err(t("Invalid PlayOnline request.").into()); }
    let session = find(config, account).ok_or(t("This account has no active PlayOnline session."))?;
    let directory = config.account_directory(account);
    let _lock = exclusive(&directory.join("native-social.lock")).map_err(|_| t("Another PlayOnline request is being submitted."))?;
    let snapshot = read_friend_cache(&directory, session.worker).ok_or(t("Launch this account with the current build to use PlayOnline messages."))?;
    if snapshot.game_running != Some(false) || !snapshot.social.ready {
        return Err(t("Return from FFXI to the signed-in PlayOnline session first.").into());
    }
    if snapshot.social.busy || directory.join("native-social.json").exists()
        || directory.join("native-control.json").exists() {
        return Err(t("Wait for the current PlayOnline request to finish.").into());
    }
    let id = crate::pol_less::pipe::random_hex().map_err(|_| t("Could not create the PlayOnline request identity."))?;
    let request = SocialRequest { worker: session.worker, id: id.clone(), request };
    let bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| t("Could not encode the PlayOnline request."))?);
    let temporary = PendingStatusFile(directory.join("native-social.json.tmp"));
    let mut file = File::create(&temporary.0).map_err(|_| t("Could not save the PlayOnline request."))?;
    file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| t("Could not save the PlayOnline request."))?;
    drop(file);
    fs::rename(&temporary.0, directory.join("native-social.json")).map_err(|_| t("Could not submit the PlayOnline request."))?;
    drop(_lock);
    wake::signal(session.worker);
    Ok(id)
}

pub(crate) fn take_social(directory: &Path) -> Option<(SocialRequest, File)> {
    // Hold the publication lock until the worker publishes busy=true. Removing
    // the one-shot file first must not expose an idle cache to a second window.
    let lock = exclusive(&directory.join("native-social.lock")).ok()?;
    let path = directory.join("native-social.json");
    let bytes = Zeroizing::new(read_bounded(&path, 64 * 1024)?);
    // A reader can deny deletion on Windows. Never deliver the message while
    // its one-shot file survives: the next idle tick would send it again.
    fs::remove_file(&path).ok()?;
    let request: SocialRequest = serde_json::from_slice(&bytes).ok()?;
    (Some(request.worker) == identity(std::process::id()) && request.request.valid()
        && request.id.len() == 32 && request.id.bytes().all(|b| b.is_ascii_hexdigit())).then_some((request, lock))
}

pub(crate) fn request_native_control(config: &Config, account: &Account, action: NativeControl) -> Result<(), String> {
    request_native_status(config, account, action, true)
}

pub(crate) fn request_native_status(config: &Config, account: &Account, action: NativeControl, groups: bool) -> Result<(), String> {
    let session = find(config, account).ok_or(t("This account has no active PlayOnline session."))?;
    request_session_control(&session, action, groups)
}

fn request_session_control(session: &BackgroundSession, action: NativeControl, groups: bool) -> Result<(), String> {
    let directory = session.path.parent().ok_or(t("This account has no active PlayOnline session."))?;
    let _batch = exclusive(&directory.join("batch.lock"))
        .map_err(|_| "This account is waiting for its batch to start.")?;
    // Shares the social publication lock so a control cannot race a message
    // submission or the worker's read/remove of the previous control.
    let _command = exclusive(&directory.join("native-social.lock"))
        .map_err(|_| t("Another PlayOnline request is being submitted."))?;
    if !session.active() { return Err(t("This account has no active PlayOnline session.").into()); }
    let snapshot = read_friend_cache(directory, session.worker)
        .ok_or(t("This session does not support native PlayOnline controls. Launch it using the current build."))?;
    if snapshot.game_running != Some(false) {
        return Err(t("Close FFXI before starting another game or signing out of PlayOnline.").into());
    }
    if snapshot.social.busy || directory.join("native-social.json").exists()
        || directory.join("native-control.json").exists() {
        return Err(t("Wait for the current PlayOnline request to finish.").into());
    }
    save_json(&directory.join("native-control.json"), &ControlRequest { worker: session.worker, action, groups })?;
    wake::signal(session.worker);
    Ok(())
}

/// The action and its "also for my groups" choice.
pub(crate) fn take_native_control(directory: &Path) -> Option<(NativeControl, bool)> {
    let _lock = exclusive(&directory.join("native-social.lock")).ok()?;
    let path = directory.join("native-control.json");
    let bytes = read_bounded(&path, 1024)?;
    fs::remove_file(&path).ok()?;
    let request: ControlRequest = serde_json::from_slice(&bytes).ok()?;
    (Some(request.worker) == identity(std::process::id())).then_some((request.action, request.groups))
}

/// The window saw input while this account's session is signed in and idle.
/// The worker forwards it as `native_activity` (the Viewer's last-input
/// stamp for auto-Away, app 10060039). Callers throttle; a pending note is
/// simply replaced.
pub(crate) fn note_native_activity(config: &Config, account: &Account) -> Result<(), String> {
    let session = find(config, account).ok_or(t("This account has no active PlayOnline session."))?;
    let directory = config.account_directory(account);
    let idle = read_friend_cache(&directory, session.worker).is_some_and(|s| s.game_running == Some(false));
    if !idle { return Ok(()); }
    save_json(&directory.join("native-activity.json"), &session.worker)?;
    wake::signal(session.worker);
    Ok(())
}

pub(crate) fn take_native_activity(directory: &Path) -> bool {
    let path = directory.join("native-activity.json");
    let Some(bytes) = read_bounded(&path, 1024) else { return false };
    if fs::remove_file(&path).is_err() { return false; }
    serde_json::from_slice::<Identity>(&bytes).ok() == identity(std::process::id())
}

fn read_friend_cache(directory: &Path, worker: Identity) -> Option<crate::friends::Snapshot> {
    let bytes = read_bounded(&directory.join("friends.json"), 512 * 1024)?;
    let cache: FriendCache = serde_json::from_slice(&bytes).ok()?;
    (cache.worker == worker && cache.snapshot.entries.len() <= 300
        && cache.snapshot.entries.iter().all(|f| f.name.len() <= 256 && f.location.as_ref().is_none_or(|s| s.len() <= 256))
        && cache.snapshot.error.as_ref().is_none_or(|e| e.len() <= 4096)
        && cache.snapshot.session_error.as_ref().is_none_or(|e| e.len() <= 4096)
        && cache.snapshot.social.valid()).then_some(cache.snapshot)
}

pub(crate) fn publish_friends(directory: &Path, snapshot: &crate::friends::Snapshot) -> Result<(), String> {
    let worker = identity(std::process::id()).ok_or(t("Could not identify the friends session."))?;
    fs::create_dir_all(directory).map_err(|_| t("Could not save the friend list."))?;
    save_json(&directory.join("friends.json"), &FriendCache { worker, snapshot: snapshot.clone() })?;

    // Full idle is also the authenticated() fallback for a loader without typed milestones.

    Ok(())
}

/// Returns only a snapshot owned by this exact, still-running account worker.
pub(crate) fn friends_snapshot(config: &Config, account: &Account) -> (bool, Option<crate::friends::Snapshot>) {
    let Some(session) = find(config, account) else { return (false, None) };
    (true, read_friend_cache(&config.account_directory(account), session.worker))
}

fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn emit(event: &Event) {
    if let Ok(line) = serde_json::to_string(event) {
        // A closed UI pipe must never stop the keepalive owner.
        let _ = writeln!(io::stdout().lock(), "{PREFIX}{line}");
    }
}

pub(crate) fn run() -> i32 {
    run_with(|config, account, otp, via, clicked, progress| {
        backend::login_with_progress(config, account, otp, via, clicked, progress)
    })
}

fn run_with(
    login: impl FnOnce(
        &Config,
        Account,
        String,
        Option<Plan>,
        Instant,
        &mut dyn FnMut(&str),
    ) -> Result<LaunchResult, String>,
) -> i32 {
    let result = (|| -> Result<(), String> {
        let mut stdin = io::stdin().lock();
        let mut length = [0u8; 4];
        stdin
            .read_exact(&mut length)
            .map_err(|_| t("Missing session request."))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_FRAME {
            return Err(t("Session request is too large.").into());
        }
        let mut bytes = Zeroizing::new(vec![0u8; length]);
        stdin
            .read_exact(&mut bytes)
            .map_err(|_| t("Incomplete session request."))?;
        let mut request: Request =
            serde_json::from_slice(&bytes).map_err(|_| t("Invalid session request."))?;
        drop(bytes);
        drop(stdin);
        let path = record_path(&request.config, &request.account);
        let _account_lock = exclusive(&path.with_extension("lock")).map_err(|_| {
            t("This account already has a background session. Reopen Crystal to reconnect.")
        })?;
        let worker =
            identity(std::process::id()).ok_or(t("Could not identify the session worker."))?;
        save_record(
            &path,
            &Record {
                worker,
                state: Event::Progress(t("Starting sign-in…").into()),
                social_only: request.via.as_ref().is_some_and(|plan| plan.social_only),
                authenticated: false,
                native_batch: false,
                launch_tool: request.via.as_ref().map(|plan| plan.tool),
            },
        )?;
        let clicked = crate::launch_timing::MonotonicStamp::restore(request.clicked_clock, request.elapsed_ms);
        let mut progress = |message: &str| {
            emit(&Event::Progress(message.to_owned()));
        };
        let launch = login(
            &request.config,
            request.account.clone(),
            std::mem::take(&mut request.otp),
            request.via.clone(),
            clicked,
            &mut progress,
        );
        drop(request); // Do not retain credentials for the lifetime of the game.
        let launch = match launch {
            Ok(launch) => launch,
            Err(error) => {
                let _ = update_record(&path, worker, |record| record.state = Event::Failed(error.clone()));
                return Err(error);
            }
        };
        let started = Event::Started(Started {
            pid: launch.pid,
            detail: launch.detail.clone(),
            log: launch.log.clone(),
            timing: launch.timing.clone(),
        });
        let _ = update_record(&path, worker, |record| record.state = started.clone());
        emit(&started);
        while launch.alive.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let ended = Event::Finished(launch.outcome.lock().ok().and_then(|slot| slot.clone()));
        let _ = update_record(&path, worker, |record| record.state = ended.clone());
        emit(&ended);
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            emit(&Event::Failed(error));
            1
        }
    }
}

fn monitor(
    path: PathBuf,
    worker: Identity,
    alive: Arc<AtomicBool>,
    game_running: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<ExitReport>>>,
) {
    let mut previous_friends = None;
    loop {
        let modified = path.parent().and_then(|d| fs::metadata(d.join("friends.json")).ok()).and_then(|m| m.modified().ok());
        if modified != previous_friends {
            if let Some(snapshot) = path.parent().and_then(|d| read_friend_cache(d, worker)) {
                if let Some(game) = snapshot.game_running { game_running.store(game, Ordering::Release); }
                previous_friends = modified;
            }
        }
        if let Some(record) = read_record(&path).filter(|r| r.worker == worker) {
            match record.state {
                Event::Finished(report) => {
                    if let Ok(mut slot) = outcome.lock() {
                        *slot = report;
                    }
                    break;
                }
                Event::Failed(error) => {
                    if let Ok(mut slot) = outcome.lock() {
                        *slot = Some(crate::launch_log::classify_exit(&error, Some(1), None));
                    }
                    break;
                }
                _ => {}
            }
        }
        if identity(worker.pid) != Some(worker) {
            if let Ok(mut slot) = outcome.lock() {
                *slot = Some(ExitReport::worker_ended());
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    alive.store(false, Ordering::Release);
    game_running.store(false, Ordering::Release);
}

fn track(path: PathBuf, worker: Identity, started: Started) -> Result<LaunchResult, String> {
    let alive = Arc::new(AtomicBool::new(true));
    let game_running = Arc::new(AtomicBool::new(path.parent().and_then(|d| read_friend_cache(d, worker)).and_then(|s| s.game_running).unwrap_or(true)));
    let outcome = Arc::new(Mutex::new(None));
    let (tracked, ended, game) = (alive.clone(), outcome.clone(), game_running.clone());
    std::thread::Builder::new()
        .name("session-status".into())
        .spawn(move || monitor(path, worker, tracked, game, ended))
        .map_err(|_| t("Could not watch the background session."))?;
    Ok(LaunchResult {
        pid: started.pid,
        detail: started.detail,
        log: started.log,
        timing: started.timing,
        alive,
        game_running,
        outcome,
    })
}

#[derive(Clone)]
pub(crate) struct BackgroundSession {
    path: PathBuf,
    worker: Identity,
}

/// Find an exact living worker; a reused PID or a completed record cannot reattach.
pub(crate) fn find(config: &Config, account: &Account) -> Option<BackgroundSession> {
    let path = record_path(config, account);
    let record = read_record(&path)?;
    if identity(record.worker.pid) != Some(record.worker) {
        return None;
    }
    if !matches!(record.state, Event::Started(_) | Event::Progress(_)) {
        return None;
    }
    Some(BackgroundSession {
        path,
        worker: record.worker,
    })
}

impl BackgroundSession {
    pub(crate) fn game_running(&self) -> Option<bool> {
        if !self.active() { return None; }
        self.path.parent().and_then(|directory| read_friend_cache(directory, self.worker))
            .and_then(|snapshot| snapshot.game_running)
    }
    pub(crate) fn ensure_launch_tool(&self, requested: Tool) -> Result<(), String> {
        let record = read_record(&self.path).filter(|record|
            record.worker == self.worker && identity(self.worker.pid) == Some(self.worker)
            && matches!(record.state, Event::Progress(_) | Event::Started(_)))
            .ok_or_else(|| t("The background session ended before Play could start. Sign in again.").to_owned())?;
        // Old workers do not identify their tool. They can reconnect as before,
        // but cannot promise Vanilla because their host may already contain hooks.
        if record.launch_tool.is_some_and(|tool| tool != requested)
            || record.launch_tool.is_none() && requested == Tool::Vanilla {
            return Err(t("Sign out of PlayOnline before switching launch methods, then launch again.").into());
        }
        Ok(())
    }

    fn active(&self) -> bool {
        identity(self.worker.pid) == Some(self.worker)
            && read_record(&self.path).is_some_and(|record| record.worker == self.worker
                && matches!(record.state, Event::Progress(_) | Event::Started(_)))
    }

    pub(crate) fn supports_batch_start(&self) -> bool {
        identity(self.worker.pid) == Some(self.worker)
            && read_record(&self.path).is_some_and(|record| record.worker == self.worker
                && record.native_batch && matches!(record.state, Event::Progress(_) | Event::Started(_)))
    }

    /// A known idle session, or a sign-in explicitly started without a game.
    /// Unknown game state on an ordinary launch must never be treated as idle.
    pub(crate) fn can_prepare_update(&self) -> bool {
        if !self.active() { return false; }
        let snapshot = self.path.parent().and_then(|directory| read_friend_cache(directory, self.worker));
        match snapshot.and_then(|snapshot| snapshot.game_running) {
            Some(running) => !running,
            None => read_record(&self.path).is_some_and(|record| record.worker == self.worker && record.social_only),
        }
    }

    /// A reopened window can queue Play on a sign-in-only worker without starting another.
    pub(crate) fn preparing_social(&self) -> bool {
        identity(self.worker.pid) == Some(self.worker)
            && read_record(&self.path).is_some_and(|record| record.worker == self.worker
                && record.social_only && matches!(record.state, Event::Progress(_) | Event::Started(_)))
            && !self.path.parent().and_then(|directory| read_friend_cache(directory, self.worker))
                .is_some_and(|snapshot| snapshot.social.ready && snapshot.game_running.is_some())
    }

    fn authenticated(&self) -> bool {
        identity(self.worker.pid) == Some(self.worker)
            && read_record(&self.path).is_some_and(|record| record.worker == self.worker
                && matches!(record.state, Event::Progress(_) | Event::Started(_))
                && (record.authenticated
                    // An older worker has no typed milestone; full idle remains safe.
                    || self.path.parent().and_then(|directory| read_friend_cache(directory, self.worker))
                        .is_some_and(|snapshot| snapshot.game_running == Some(false) && snapshot.social.ready)))
    }

    /// Wait outside the UI thread if the previous window closed during sign-in.
    pub(crate) fn reconnect(self, mut progress: impl FnMut(&str)) -> Result<LaunchResult, String> {

        let mut previous = String::new();
        loop {
            if let Some(record) = read_record(&self.path) {
                if record.worker != self.worker { return Err(t("The background session worker changed.").into()); }
                match record.state {
                    Event::Started(started) => return track(self.path, self.worker, started),
                    Event::Failed(error) => return Err(error),
                    Event::Finished(report) => {
                        return Err(report
                            .map_or_else(|| t("The background session ended.").into(), |r| r.message()))
                    }
                    Event::Progress(message) if message != previous => {
                        progress(&message);
                        previous = message;
                    }
                    _ => {}
                }
            }
            if identity(self.worker.pid) != Some(self.worker) {
                return Err(t("The background session worker ended.").into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub(crate) fn launch(
    config: &Config,
    account: Account,
    otp: String,
    via: Option<Plan>,
    clicked: Instant,
    progress: impl FnMut(&str),
) -> Result<LaunchResult, String> {
    let mut otp = Zeroizing::new(otp);
    if account.mode == backend::AccountMode::Retail && find(config, &account).is_none() {
        crate::viewer::gate(&crate::viewer::status(false))?;
    }
    launch_preflighted(config, account, std::mem::take(&mut *otp), via, clicked, progress)
}

/// Finish idle POL sessions through their native sign-out path before file work.
/// Returning success means every captured owner ended; the updater independently
/// checks for clients again before changing either installation.
pub(crate) async fn sign_out_for_update(sessions: Vec<BackgroundSession>, operation: &crate::operation::Operation) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    operation.check_cancelled()?;
    if sessions.iter().any(|session| session.active() && !session.can_prepare_update()) {
        return Err(t("Close FFXI before starting another game or signing out of PlayOnline.").into());
    }
    let total = sessions.len() as u64;
    operation.progress("SignOut", 0, total, t("Signing out idle PlayOnline sessions before updating"));
    for session in &sessions {
        while session.active() {
            operation.check_cancelled()?;
            if !session.can_prepare_update() {
                return Err(t("Close FFXI before starting another game or signing out of PlayOnline.").into());
            }
            if Instant::now() >= deadline {
                return Err(t("Timed out waiting for PlayOnline to sign out. No update was started.").into());
            }
            let directory = session.path.parent().unwrap();
            let ready = read_friend_cache(directory, session.worker).is_some_and(|snapshot|
                snapshot.game_running == Some(false) && !snapshot.social.busy
                && !directory.join("native-social.json").exists());
            if ready {
                request_session_control(session, NativeControl::SignOut, true)?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    loop {
        operation.check_cancelled()?;
        let active: Vec<_> = sessions.iter().filter(|session| session.active()).collect();
        operation.progress("SignOut", total - active.len() as u64, total,
            t("Signing out idle PlayOnline sessions before updating"));
        if active.is_empty() { return Ok(()); }
        if active.iter().any(|session| !session.can_prepare_update()) {
            return Err(t("Close FFXI before starting another game or signing out of PlayOnline.").into());
        }
        if Instant::now() >= deadline {
            return Err(t("Timed out waiting for PlayOnline to sign out. No update was started.").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// The batch observer already checked the installed Viewer before handing off
// the anonymous request. Its detached coordinator must not repeat that query
// in every child process. Native component validation still runs in each host.
fn launch_preflighted(
    config: &Config,
    account: Account,
    otp: String,
    via: Option<Plan>,
    clicked: Instant,
    progress: impl FnMut(&str),
) -> Result<LaunchResult, String> {
    let mut otp = Zeroizing::new(otp);
    let mut command = Command::new(
        std::env::current_exe().map_err(|_| t("Could not locate Crystal's session worker."))?,
    );
    command.arg("--session-worker");
    launch_command(
        command,
        config,
        account,
        std::mem::take(&mut *otp),
        via,
        clicked,
        progress,
    )
}

fn launch_command(
    mut command: Command,
    config: &Config,
    account: Account,
    otp: String,
    via: Option<Plan>,
    clicked: Instant,
    mut progress: impl FnMut(&str),
) -> Result<LaunchResult, String> {
    let mut otp = Zeroizing::new(otp);
    if via.as_ref().is_some_and(|p| !p.social_only) && batch_pending(config, &account) {
        return Err("This account is waiting for its batch to start.".into());
    }
    if let Some(session) = find(config, &account) {
        if let Some(plan) = &via { session.ensure_launch_tool(plan.tool)?; }
        if via.as_ref().is_some_and(|p| !p.social_only)
            && read_friend_cache(&config.account_directory(&account), session.worker).is_some_and(|s| s.game_running == Some(false)) {
            // This session is already signed in (a worker record exists), so the stock FFXI
            // check below still follows the sign-in; a fresh Play checks inside the worker.
            let install = crate::pol_less::install_directory(config)?;
            crate::patch::gate_before_start(&install)?;
            request_native_control(config, &account, NativeControl::StartGame)?;
        }
        return session.reconnect(progress);
    }
    let _launch_claim = if via.as_ref().is_some_and(|p| !p.social_only) {
        Some(exclusive(&config.account_directory(&account).join("batch.lock"))
            .map_err(|_| "This account is waiting for its batch to start.")?)
    } else { None };
    let path = record_path(config, &account);
    let request = Request {
        config: config.clone(),
        account,
        otp: std::mem::take(&mut *otp),
        via,
        elapsed_ms: clicked.elapsed().as_millis().min(u64::MAX as u128) as u64,
        clicked_clock: crate::launch_timing::MonotonicStamp::capture(clicked),
    };
    let bytes = Zeroizing::new(
        serde_json::to_vec(&request).map_err(|_| t("Could not prepare the session request."))?,
    );
    drop(request);
    if bytes.len() > MAX_FRAME {
        return Err(t("Session request is too large.").into());
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|_| t("Could not start Crystal's background session worker."))?;
    let Some(worker) = identity(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(t("Could not identify Crystal's background session worker.").into());
    };
    let sent = (|| -> io::Result<()> {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Missing worker pipe"))?;
        input.write_all(&(bytes.len() as u32).to_le_bytes())?;
        input.write_all(&bytes)?;
        Ok(())
    })();
    drop(bytes);
    if sent.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(t("Could not hand credentials to the background session worker.").into());
    }
    let output = child
        .stdout
        .take()
        .ok_or(t("Missing session progress pipe."))?;
    let mut output = BufReader::new(output);
    loop {
        let mut line = String::new();
        let count = output
            .by_ref()
            .take((MAX_FRAME + 1) as u64)
            .read_line(&mut line)
            .map_err(|_| t("Could not read session progress."))?;
        if count == 0 {
            return Err(t("The background session worker ended before starting the game.").into());
        }
        if count > MAX_FRAME {
            return Err(t("Session progress is too large.").into());
        }
        let Some(json) = line.trim_end().strip_prefix(PREFIX) else {
            continue;
        };
        let event: Event = serde_json::from_str(json).map_err(|_| t("Invalid session progress."))?;
        match event {
            Event::Progress(message) => progress(&message),
            Event::Failed(error) => {
                let _ = child.wait();
                return Err(error);
            }
            Event::Started(started) => {
                // Dropping Child/stdout does not terminate the worker. Its session and
                // credentials never depend on this observer remaining open.
                drop(output);
                return track(path, worker, started);
            }
            Event::Finished(_) => {
                return Err(t("The background session ended before starting the game.").into())
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn status_publication_supports_long_unicode_paths_and_replaces_existing_data() {
        use std::os::windows::ffi::OsStrExt;
        let fixture = Fixture::new();
        let mut directory = fixture.0.clone();
        for index in 0..7 {
            directory.push(format!("crystal-status-試験-{index}-{}", "segment".repeat(5)));
        }
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("friends.json");
        assert!(path.as_os_str().encode_wide().count() > 300);
        for revision in 0..3 {
            save_json(&path, &serde_json::json!({"revision": revision})).unwrap();
            let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved["revision"], revision);
        }
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    }

    #[test]
    fn session_reuse_requires_the_original_launch_method_and_an_owned_live_worker() {
        let fixture = Fixture::new(); let config = config(&fixture.0); let account = account();
        let worker = identity(std::process::id()).unwrap();
        let path = record_path(&config, &account); fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old: Record = serde_json::from_value(serde_json::json!({
            "worker": worker, "state": Event::Progress("Fixture signing in".into()),
        })).unwrap();
        assert!(old.launch_tool.is_none(), "older records remain readable");
        for stored in [None, Some(Tool::Windower), Some(Tool::Ashita), Some(Tool::Vanilla)] {
            let record = Record { worker, state: old.state.clone(), social_only: false,
                authenticated: false, native_batch: false, launch_tool: stored };
            save_record(&path, &record).unwrap();
            let before = fs::read(&path).unwrap();
            let session = find(&config, &account).unwrap();
            for requested in [Tool::Windower, Tool::Ashita, Tool::Vanilla] {
                let compatible = stored.map_or(requested != Tool::Vanilla, |tool| tool == requested);
                assert_eq!(session.ensure_launch_tool(requested).is_ok(), compatible, "{stored:?} → {requested:?}");
            }
            assert_eq!(fs::read(&path).unwrap(), before, "checking must not sign out or change the session");
            update_record(&path, worker, |record| record.authenticated = true).unwrap();
            assert_eq!(read_record(&path).unwrap().launch_tool, stored, "milestones retain the launch method");
        }
        let session = find(&config, &account).unwrap();
        update_record(&path, worker, |record| record.worker.created += 1).unwrap();
        assert!(session.ensure_launch_tool(Tool::Vanilla).is_err(), "a replacement cannot inherit the old identity");
    }

    #[test]
    fn switching_to_vanilla_stops_before_reconnect_version_checks_or_game_control() {
        let fixture = Fixture::new(); let config = config(&fixture.0); let account = account();
        let path = record_path(&config, &account); fs::create_dir_all(path.parent().unwrap()).unwrap();
        let worker = identity(std::process::id()).unwrap();
        save_record(&path, &Record { worker, state: Event::Progress("Fixture Windower host".into()),
            social_only: true, authenticated: true, native_batch: true, launch_tool: Some(Tool::Windower) }).unwrap();
        let original = fs::read(&path).unwrap();
        let plan = Plan { tool: Tool::Vanilla, settings: Default::default(),
            social_only: false, invisible_login: false, auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default() };
        let error = launch_command(Command::new("must-not-spawn.exe"), &config, account.clone(), String::new(),
            Some(plan), Instant::now(), |_| panic!("must stop before reconnect")).err().unwrap();
        assert!(error.contains("before switching launch methods"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!config.account_directory(&account).join("native-control.json").exists());
    }

    #[test]
    fn reconnect_rejects_a_replaced_or_ended_worker() {
        for ended in [false, true] {
            let fixture = Fixture::new(); let config = config(&fixture.0); let account = account();
            let path = record_path(&config, &account); fs::create_dir_all(path.parent().unwrap()).unwrap();
            let worker = identity(std::process::id()).unwrap();
            save_record(&path, &Record { worker, state: Event::Progress("still authenticating".into()),
                social_only: true, authenticated: false, native_batch: true, launch_tool: None }).unwrap();
            let session = find(&config, &account).unwrap();
            let (progress_tx, progress_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let observer = std::thread::spawn(move || {
                let result = session.reconnect(|_| { let _ = progress_tx.send(()); });
                result_tx.send(result.map(|_| ())).unwrap();
            });
            progress_rx.recv_timeout(Duration::from_secs(5)).unwrap();

            assert!(matches!(result_rx.recv_timeout(Duration::from_millis(30)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
                "an unfinished record cannot complete reconnect");
            if ended {
                update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap();
            } else {
                save_record(&path, &Record { worker: Identity { created: worker.created + 1, ..worker },
                    state: Event::Started(Started { pid: worker.pid, detail: "replacement".into(), log: fixture.0.join("fixture.log"), timing: String::new() }),
                    social_only: true, authenticated: true, native_batch: true, launch_tool: None }).unwrap();

            }
            let error = result_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
            assert!(error.contains(if ended { "ended" } else { "changed" }), "{error}");
            observer.join().unwrap();
        }
    }

    #[test]
    fn status_replacement_preserves_old_data_and_recovers_from_windows_reader_locks() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let fixture = Fixture::new();
        let path = fixture.0.join("friends.json");
        save_json(&path, &serde_json::json!({"ready": false})).unwrap();
        let held_reader = OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(&path).unwrap();

        // Exercise a real Windows replacement failure instead of mocking a file error.
        let error = save_json(&path, &serde_json::json!({"ready": true})).unwrap_err();
        assert!(error.contains("friends.json") && error.contains("os error"), "{error}");
        let previous: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(previous["ready"], false);
        assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| entry.unwrap().path().extension().is_some_and(|e| e == "tmp")));

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            drop(held_reader);
        });
        save_json(&path, &serde_json::json!({"ready": true})).unwrap();
        release.join().unwrap();
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(current["ready"], true);
    }

    #[test]
    fn concurrent_status_publications_use_independent_staging_files() {
        let fixture = Fixture::new();
        let path = fixture.0.join("friends.json");
        let start = std::sync::Barrier::new(6);
        std::thread::scope(|scope| {
            for writer in 0..6 {
                let (path, start) = (&path, &start);
                scope.spawn(move || {
                    start.wait();
                    for revision in 0..10 {
                        save_json(path, &serde_json::json!({"writer": writer, "revision": revision, "data": "x".repeat(4096)})).unwrap();
                    }
                });
            }
        });
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["revision"], 9);
        assert_eq!(saved["data"].as_str().unwrap().len(), 4096);
        assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| entry.unwrap().path().extension().is_some_and(|e| e == "tmp")));
    }

    #[test]
    fn typed_startup_state_defaults_safely_and_preserves_late_started_records() {
        let fixture = Fixture::new(); let config = config(&fixture.0); let account = account();
        let directory = config.account_directory(&account); let path = record_path(&config, &account);
        fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        let old: Record = serde_json::from_value(serde_json::json!({
            "worker": worker, "state": Event::Progress("Authenticated: a label is not evidence".into()),
        })).unwrap();
        assert!(!old.authenticated && !old.social_only && !old.native_batch);
        save_record(&path, &old).unwrap();
        let session = find(&config, &account).unwrap();
        assert!(!session.preparing_social() && !session.authenticated() && !session.supports_batch_start());
        update_record(&path, worker, |record| record.social_only = true).unwrap();
        assert!(session.preparing_social());
        assert!(!session.authenticated());
        // Whichever writer arrives first, authentication only changes its milestone and
        // login completion only changes its event. Neither may replace the other's update.
        std::thread::scope(|scope| {
            scope.spawn(|| publish_authenticated(&directory).unwrap());
            scope.spawn(|| publish_native_batch(&directory, true).unwrap());
            scope.spawn(|| update_record(&path, worker, |record| record.state = Event::Started(Started {
                pid: worker.pid, detail: "Late startup still pending".into(), log: fixture.0.join("fixture.log"), timing: String::new(),
            })).unwrap());
        });
        let record = read_record(&path).unwrap();
        assert!(record.authenticated && record.social_only && record.native_batch && matches!(record.state, Event::Started(_)));
        assert!(session.authenticated() && session.preparing_social() && session.supports_batch_start());
        publish_native_batch(&directory, false).unwrap();
        assert!(!session.supports_batch_start(), "a loader without the capability keeps the old single-start path");
        let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
        snapshot.social.ready = true;
        publish_friends(&directory, &snapshot).unwrap();
        assert!(!session.preparing_social());
        update_record(&path, worker, |record| { record.state = Event::Finished(None); record.authenticated = false; }).unwrap();
        publish_authenticated(&directory).unwrap();
        publish_native_batch(&directory, true).unwrap();
        assert!(!read_record(&path).unwrap().authenticated, "an ended session stays ended");
        assert!(!session.authenticated() && !session.supports_batch_start());
        save_record(&path, &Record { worker: Identity { created: 0, ..worker }, state: Event::Progress("reused PID".into()), social_only: true, authenticated: true, native_batch: true, launch_tool: None }).unwrap();
        assert!(publish_authenticated(&directory).is_err());
        assert!(publish_native_batch(&directory, true).is_err());
        assert!(!session.preparing_social() && !session.authenticated() && !session.supports_batch_start());
    }

    #[test]
    fn control_requests_require_social_idle_and_the_exact_worker_identity() {
        let fixture = Fixture::new();
        let config = config(&fixture.0);
        let account = account();
        let directory = config.account_directory(&account);
        let path = record_path(&config, &account);
        let worker = identity(std::process::id()).unwrap();
        fs::create_dir_all(&directory).unwrap();
        save_record(&path, &Record { worker, state: Event::Progress("fixture".into()), social_only: false, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        let mut snapshot = crate::friends::Snapshot { game_running: Some(true), ..Default::default() };
        publish_friends(&directory, &snapshot).unwrap();
        assert!(request_native_control(&config, &account, NativeControl::SignOut).is_err());
        assert!(!directory.join("native-control.json").exists());
        snapshot.game_running = Some(false);
        snapshot.social.ready = true;
        publish_friends(&directory, &snapshot).unwrap();
        request_native_control(&config, &account, NativeControl::StartGame).unwrap();
        assert!(request_native_control(&config, &account, NativeControl::SignOut).is_err(), "a queued Play must not be replaced");
        assert!(request_social(&config, &account, crate::social::Request::Check {}).is_err(), "mail cannot race a queued control");
        let publication = exclusive(&directory.join("native-social.lock")).unwrap();
        assert_eq!(take_native_control(&directory), None, "the consumer uses the publisher's lock");
        drop(publication);
        assert_eq!(take_native_control(&directory), Some((NativeControl::StartGame, true)));
        assert_eq!(take_native_control(&directory), None);
        request_native_status(&config, &account, NativeControl::Away, false).unwrap();
        assert_eq!(take_native_control(&directory), Some((NativeControl::Away, false)));
        // Requests written before the groups choice existed apply to groups.
        fs::write(directory.join("native-control.json"), serde_json::json!({ "worker": worker, "action": "visible" }).to_string()).unwrap();
        assert_eq!(take_native_control(&directory), Some((NativeControl::Visible, true)));
        let stale = Identity { created: worker.created + 1, ..worker };
        save_json(&directory.join("native-control.json"), &ControlRequest { worker: stale, action: NativeControl::SignOut, groups: true }).unwrap();
        assert_eq!(take_native_control(&directory), None);
        save_json(&directory.join("friends.json"), &FriendCache { worker: stale, snapshot }).unwrap();
        assert!(request_native_control(&config, &account, NativeControl::StartGame).is_err());
    }

    #[test]
    fn one_shot_commands_are_not_delivered_until_windows_allows_removal() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let fixture = Fixture::new();
        let directory = &fixture.0;
        let worker = identity(std::process::id()).unwrap();
        let deny_removal = |path: &Path| OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(path).unwrap();

        let social = directory.join("native-social.json");
        save_json(&social, &SocialRequest { worker, id: "1".repeat(32), request: crate::social::Request::Send {
            alternate: false, slot: 0, uid: 1, subject: "fixture".into(), message: "Deliver once".into(),
        } }).unwrap();
        let observer = deny_removal(&social);
        assert!(take_social(directory).is_none(), "retaining the file must not send the message");
        assert!(social.exists());
        drop(observer);
        assert!(take_social(directory).is_some());
        assert!(take_social(directory).is_none(), "a consumed message is never replayed");

        let control = directory.join("native-control.json");
        save_json(&control, &ControlRequest { worker, action: NativeControl::StartGame, groups: false }).unwrap();
        let observer = deny_removal(&control);
        assert_eq!(take_native_control(directory), None);
        drop(observer);
        assert_eq!(take_native_control(directory), Some((NativeControl::StartGame, false)));
        assert_eq!(take_native_control(directory), None);

        let activity = directory.join("native-activity.json");
        save_json(&activity, &worker).unwrap();
        let observer = deny_removal(&activity);
        assert!(!take_native_activity(directory));
        drop(observer);
        assert!(take_native_activity(directory));
        assert!(!take_native_activity(directory));

        let batch = directory.join("native-batch.json");
        let (_gate, tokens) = start_gate::Coordinator::new(&crate::pol_less::pipe::random_hex().unwrap(), 1).unwrap();
        save_json(&batch, &BatchControl { worker, token: tokens[0].clone() }).unwrap();
        let observer = deny_removal(&batch);
        assert!(take_native_batch(directory).is_none());
        drop(observer);
        assert!(take_native_batch(directory).is_some());
        assert!(take_native_batch(directory).is_none());
    }

    #[test]
    fn activity_reaches_only_this_idle_worker() {
        let fixture = Fixture::new();
        let config = config(&fixture.0);
        let account = account();
        let directory = config.account_directory(&account);
        let worker = identity(std::process::id()).unwrap();
        fs::create_dir_all(&directory).unwrap();
        assert!(note_native_activity(&config, &account).is_err(), "no session");
        save_record(&record_path(&config, &account), &Record { worker, state: Event::Progress("fixture".into()), social_only: false, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        let mut snapshot = crate::friends::Snapshot { game_running: Some(true), ..Default::default() };
        publish_friends(&directory, &snapshot).unwrap();
        note_native_activity(&config, &account).unwrap();
        assert!(!take_native_activity(&directory), "FFXI owns the session");
        snapshot.game_running = Some(false);
        publish_friends(&directory, &snapshot).unwrap();
        note_native_activity(&config, &account).unwrap();
        assert!(take_native_activity(&directory));
        assert!(!take_native_activity(&directory));
        let stale = Identity { created: worker.created + 1, ..worker };
        save_json(&directory.join("native-activity.json"), &stale).unwrap();
        assert!(!take_native_activity(&directory));
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("CRYSTAL_DATA_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(format!("session-worker-{}", backend::unique_id().unwrap()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::write(self.0.join("stop"), []);
        }
    }
    fn config(root: &Path) -> Config {
        Config {
            experiment: root.to_path_buf(),
            repository: root.to_path_buf(),
            game_install: None,
            dat_overlays: Vec::new(),
        }
    }
    fn account() -> Account {
        Account {
            label: "Fixture".into(),
            mode: backend::AccountMode::Private,
            server: "localhost".into(),
            pol_id: "fixture".into(),
            pol_password: "fixture-password-never-on-disk".into(),
            se_id: String::new(),
            se_password: String::new(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
        }
    }
    fn child_command(test: &str, root: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env("CRYSTAL_SESSION_FIXTURE", root);
        command
    }
    fn until(mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !check() {
            assert!(Instant::now() < deadline, "fixture timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    #[ignore = "executed only in a fixture subprocess"]
    fn worker_fixture() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_SESSION_FIXTURE").unwrap());
        let code = run_with(|config, account, mut otp, _via, _clicked, progress| {
            otp.zeroize();
            progress("Fixture authenticated");
            let alive = Arc::new(AtomicBool::new(true));
            let social = root.join("social-fixture").exists();
            let game_running = Arc::new(AtomicBool::new(!social));
            let pol_directory = config.account_directory(&account);
            if social {
                publish_friends(&pol_directory, &crate::friends::Snapshot { game_running: Some(false), ..Default::default() }).unwrap();
            }
            let outcome = Arc::new(Mutex::new(None));
            let (tracked, ended, directory) = (alive.clone(), outcome.clone(), root.clone());
            let game = game_running.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut beat = 0;
                while !directory.join("stop").exists() && Instant::now() < deadline {
                    if social {
                        match take_native_control(&pol_directory).map(|(action, _)| action) {
                            Some(NativeControl::StartGame) => {
                                game.store(true, Ordering::Release);
                                publish_friends(&pol_directory, &crate::friends::Snapshot { game_running: Some(true), ..Default::default() }).unwrap();
                            }
                            Some(NativeControl::SignOut) => break,
                            Some(_) => {},
                            None => {},
                        }
                        if directory.join("game-exited").exists() {
                            fs::remove_file(directory.join("game-exited")).unwrap();
                            game.store(false, Ordering::Release);
                            publish_friends(&pol_directory, &crate::friends::Snapshot { game_running: Some(false), ..Default::default() }).unwrap();
                        }
                    }
                    beat += 1;
                    let _ = fs::write(directory.join("heartbeat"), beat.to_string());
                    std::thread::sleep(Duration::from_millis(50));
                }
                *ended.lock().unwrap() = Some(crate::launch_log::classify_exit(
                    "[recomp] quit",
                    Some(0),
                    None,
                ));
                tracked.store(false, Ordering::Release);
            });
            while root.join("hold-start").exists()
                && !root.join("release-start").exists()
                && alive.load(Ordering::Acquire)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(LaunchResult {
                pid: std::process::id(),
                detail: "Fixture started".into(),
                log: config.experiment.join("fixture.log"),
                timing: "Fixture timing".into(),
                alive,
                game_running,
                outcome,
            })
        });
        std::process::exit(code);
    }

    #[test]
    fn background_social_session_survives_ui_reconnect_and_game_exit() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("social-fixture"), []).unwrap();
        let config = config(&fixture.0);
        let account = account();
        let launch = launch_command(child_command("session_worker::tests::worker_fixture", &fixture.0), &config, account.clone(), "123456".into(), None, Instant::now(), |_| {}).unwrap();
        assert!(!launch.game_running.load(Ordering::Acquire));
        assert!(launch.alive.load(Ordering::Acquire));
        let session = find(&config, &account).unwrap();
        let reconnected = session.reconnect(|_| {}).unwrap();
        assert_eq!(reconnected.pid, launch.pid);
        assert!(!reconnected.game_running.load(Ordering::Acquire));
        request_native_control(&config, &account, NativeControl::StartGame).unwrap();
        until(|| launch.game_running.load(Ordering::Acquire) && reconnected.game_running.load(Ordering::Acquire));
        assert!(request_native_control(&config, &account, NativeControl::SignOut).is_err());
        fs::write(fixture.0.join("game-exited"), []).unwrap();
        until(|| !launch.game_running.load(Ordering::Acquire) && !reconnected.game_running.load(Ordering::Acquire));
        assert!(launch.alive.load(Ordering::Acquire));
        request_native_control(&config, &account, NativeControl::SignOut).unwrap();
        until(|| !launch.alive.load(Ordering::Acquire));
        assert!(find(&config, &account).is_none());
    }

    #[tokio::test]
    async fn main_update_prepares_an_automatic_social_session_and_waits_for_native_signout() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("social-fixture"), []).unwrap();
        let config = config(&fixture.0);
        let account = account();
        let launch = launch_command(child_command("session_worker::tests::worker_fixture", &fixture.0),
            &config, account.clone(), String::new(), None, Instant::now(), |_| {}).unwrap();
        let mut app = crate::tests::app();
        app.preview = false;
        app.config = config.clone();
        app.settings.install_selected = true;
        app.settings.account_startup_signin.insert(backend::account_key(&account));
        app.patch.as_mut().unwrap().state = crate::patch::PatchState::Required;
        app.accounts.truncate(1);
        let row = &mut app.accounts[0];
        row.key = backend::account_key(&account);
        row.account = account.clone();
        row.pid = Some(launch.pid);
        row.alive = Some(launch.alive.clone());
        row.game_running = Some(launch.game_running.clone());
        assert!(app.has_sessions());
        assert!(!app.sessions_block_update());

        // The real main-button dispatcher now accepts this previously blocked
        // automatic sign-in. Dropping its task avoids any real updater network
        // or disk work; exercise its native preparation below with the same op.
        assert_eq!(app.update(crate::Message::MainAction).units(), 1);
        assert!(app.page == crate::Page::Maintenance);
        assert!(app.maintenance == Some(crate::MaintenanceKind::Update));
        assert!(app.launch_blocked());
        let operation = app.active_operation.as_ref().unwrap();
        assert_eq!(operation.snapshot().stage, "SignOut");
        assert!(launch.alive.load(Ordering::Acquire));
        sign_out_for_update(app.update_sessions().unwrap(), operation).await.unwrap();
        until(|| !launch.alive.load(Ordering::Acquire));
        assert!(!launch.alive.load(Ordering::Acquire));
        assert!(find(&config, &account).is_none());
        assert_eq!(operation.snapshot().done, 1);
    }

    #[tokio::test]
    async fn update_signout_preserves_running_games_and_cancelled_idle_sessions() {
        let idle_fixture = Fixture::new();
        let game_fixture = Fixture::new();
        for fixture in [&idle_fixture, &game_fixture] {
            fs::write(fixture.0.join("social-fixture"), []).unwrap();
        }
        let idle_config = config(&idle_fixture.0);
        let game_config = config(&game_fixture.0);
        let account = account();
        let idle = launch_command(child_command("session_worker::tests::worker_fixture", &idle_fixture.0),
            &idle_config, account.clone(), String::new(), None, Instant::now(), |_| {}).unwrap();
        let game = launch_command(child_command("session_worker::tests::worker_fixture", &game_fixture.0),
            &game_config, account.clone(), String::new(), None, Instant::now(), |_| {}).unwrap();
        request_native_control(&game_config, &account, NativeControl::StartGame).unwrap();
        until(|| game.game_running.load(Ordering::Acquire));

        let operation = crate::operation::Operation::default();
        assert!(sign_out_for_update(vec![find(&idle_config, &account).unwrap(),
            find(&game_config, &account).unwrap()], &operation).await.is_err());
        assert!(idle.alive.load(Ordering::Acquire) && game.alive.load(Ordering::Acquire));
        assert!(!idle_config.account_directory(&account).join("native-control.json").exists());

        operation.cancel();
        assert!(sign_out_for_update(vec![find(&idle_config, &account).unwrap()], &operation).await.is_err());
        assert!(idle.alive.load(Ordering::Acquire) && game.game_running.load(Ordering::Acquire));
        sign_out_for_update(vec![find(&idle_config, &account).unwrap()],
            &crate::operation::Operation::default()).await.unwrap();
        until(|| !idle.alive.load(Ordering::Acquire));
        assert!(!idle.alive.load(Ordering::Acquire));
        assert!(game.alive.load(Ordering::Acquire) && game.game_running.load(Ordering::Acquire));
    }

    #[test]
    fn update_preparation_requires_known_idle_state_or_an_explicit_social_signin() {
        let fixture = Fixture::new();
        let config = config(&fixture.0);
        let account = account();
        let directory = config.account_directory(&account);
        fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        let path = record_path(&config, &account);
        let mut record = Record { worker, state: Event::Progress("Fixture signing in".into()),
            social_only: false, authenticated: false, native_batch: false, launch_tool: None };
        save_record(&path, &record).unwrap();
        let session = find(&config, &account).unwrap();
        assert!(!session.can_prepare_update());
        record.social_only = true;
        save_record(&path, &record).unwrap();
        assert!(session.can_prepare_update());
        let mut snapshot = crate::friends::Snapshot { game_running: Some(true), ..Default::default() };
        publish_friends(&directory, &snapshot).unwrap();
        assert!(!session.can_prepare_update());
        snapshot.game_running = Some(false);
        publish_friends(&directory, &snapshot).unwrap();
        assert!(session.can_prepare_update());
        record.worker.created += 1;
        save_record(&path, &record).unwrap();
        assert!(!session.can_prepare_update(), "a reused process identity cannot authorize sign-out");
    }

    #[test]
    #[ignore = "executed only in a fixture subprocess"]
    fn parent_fixture() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_SESSION_FIXTURE").unwrap());
        let launch = launch_command(
            child_command("session_worker::tests::worker_fixture", &root),
            &config(&root),
            account(),
            "123456".into(),
            None,
            Instant::now(),
            |_| {},
        )
        .unwrap();
        fs::write(root.join("parent-result"), launch.pid.to_string()).unwrap();
        // The process exits with a living worker and no game-session owner left here.
    }

    #[test]
    fn session_survives_parent_exit_reopens_and_records_clean_exit() {
        let fixture = Fixture::new();
        let status = child_command("session_worker::tests::parent_fixture", &fixture.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let config = config(&fixture.0);
        let path = record_path(&config, &account());
        until(|| {
            fs::read_to_string(fixture.0.join("heartbeat"))
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| n >= 5)
        });
        let record = read_record(&path).unwrap();
        assert_eq!(
            identity(record.worker.pid),
            Some(record.worker),
            "worker outlives its parent"
        );
        let reconnected = find(&config, &account())
            .unwrap()
            .reconnect(|_| {})
            .unwrap();
        assert_eq!(
            reconnected.pid.to_string(),
            fs::read_to_string(fixture.0.join("parent-result")).unwrap()
        );
        assert_eq!(reconnected.timing, "Fixture timing");
        // A duplicate Play uses the living worker; this nonexistent command must never spawn.
        let duplicate = launch_command(
            Command::new("nonexistent-crystal-fixture"),
            &config,
            account(),
            "123456".into(),
            None,
            Instant::now(),
            |_| {},
        )
        .unwrap();
        assert_eq!(duplicate.pid, reconnected.pid);
        let disk = fs::read_to_string(&path).unwrap();
        assert!(!disk.contains("fixture-password-never-on-disk"));
        assert!(!disk.contains("123456"));
        fs::write(fixture.0.join("stop"), []).unwrap();
        until(|| !reconnected.alive.load(Ordering::Acquire));
        assert!(!reconnected
            .outcome
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_problem());
        assert!(find(&config, &account()).is_none());
        until(|| identity(record.worker.pid).is_none());
    }

    #[test]
    fn closing_during_sign_in_reconnects_after_the_game_starts() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("hold-start"), []).unwrap();
        let mut parent = child_command("session_worker::tests::parent_fixture", &fixture.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        until(|| fixture.0.join("heartbeat").exists());
        parent.kill().unwrap();
        parent.wait().unwrap();
        let config = config(&fixture.0);
        let session = find(&config, &account()).unwrap();
        let resumed = std::thread::spawn(move || session.reconnect(|_| {}));
        fs::write(fixture.0.join("release-start"), []).unwrap();
        let launch = resumed.join().unwrap().unwrap();
        assert_eq!(launch.timing, "Fixture timing");
        assert!(launch.alive.load(Ordering::Acquire));
        fs::write(fixture.0.join("stop"), []).unwrap();
        until(|| !launch.alive.load(Ordering::Acquire));
    }

    #[test]
    fn stale_or_reused_process_identity_cannot_reattach() {
        let fixture = Fixture::new();
        let config = config(&fixture.0);
        let path = record_path(&config, &account());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut worker = identity(std::process::id()).unwrap();
        worker.created += 1;
        save_record(
            &path,
            &Record {
                worker,
                state: Event::Progress("old session".into()),
                social_only: false,
                authenticated: false,
                native_batch: false, launch_tool: None,
            },
        )
        .unwrap();
        assert!(find(&config, &account()).is_none());
    }

    #[test]
    fn friends_cache_is_account_and_session_scoped_and_refresh_is_one_shot() {
        let fixture = Fixture::new();
        let config = config(&fixture.0);
        let account = account();
        let directory = config.account_directory(&account);
        let path = record_path(&config, &account);
        fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        save_record(&path, &Record { worker, state: Event::Progress("signed in".into()), social_only: false, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        let snapshot = crate::friends::Snapshot {
            entries: vec![crate::friends::Friend { name: "Alice".into(), alternate: false, slot: 199, uid:None, online: Some(true), location: Some("Vana'diel".into()), character: None }],
            fetched_at: 1, ..crate::friends::Snapshot::default()
        };
        publish_friends(&directory, &snapshot).unwrap();
        let (connected, received) = friends_snapshot(&config, &account);
        assert!(connected);
        assert_eq!(received.unwrap().entries[0].name, "Alice");
        let mut other = account.clone();
        other.pol_id = "another-account".into();
        assert!(friends_snapshot(&config, &other).1.is_none());
        let mut stale = worker;
        stale.created += 1;
        save_json(&directory.join("friends.json"), &FriendCache { worker: stale, snapshot }).unwrap();
        assert!(friends_snapshot(&config, &account).1.is_none());
        fs::write(directory.join("friends.json"), vec![b' '; 512 * 1024 + 1]).unwrap();
        assert!(friends_snapshot(&config, &account).1.is_none());
        save_record(&path, &Record { worker, state: Event::Finished(None), social_only: false, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        assert!(!friends_snapshot(&config, &account).0);
    }

    #[test]
    fn social_requests_are_atomic_one_shot_and_bound_to_the_native_session() {
        let fixture = Fixture::new(); let config = config(&fixture.0); let account = account();
        let directory = config.account_directory(&account); fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        save_record(&record_path(&config, &account), &Record { worker, state: Event::Progress("signed in".into()), social_only: false, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err());
        snapshot.social.ready = true; publish_friends(&directory, &snapshot).unwrap();
        let id = request_social(&config, &account, crate::social::Request::Send { alternate:false, slot:1, uid:u64::MAX, subject:"あ\"".into(), message:"line\nline".into() }).unwrap();
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err());
        assert!(request_native_control(&config, &account, NativeControl::StartGame).is_err());
        let (request, lock) = take_social(&directory).unwrap(); assert_eq!(request.id, id); assert!(take_social(&directory).is_none());
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err(), "the consume/publication gap must remain locked");
        assert!(request_native_control(&config, &account, NativeControl::StartGame).is_err(), "a control also respects the consume/publication gap");
        snapshot.social.begin(id.clone(), &request.request); publish_friends(&directory, &snapshot).unwrap(); drop(lock);
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err(), "the published busy state must guard the request after unlocking");
        let line: serde_json::Value = serde_json::from_slice(&request.line().unwrap()).unwrap();
        assert_eq!(line["type"], "native_social"); assert_eq!(line["uid"].as_u64(), Some(u64::MAX)); assert_eq!(line["message"], "line\nline");
        let mut stale = worker; stale.created += 1;
        save_json(&directory.join("native-social.json"), &SocialRequest { worker:stale, id:id.clone(), request:crate::social::Request::Inbox { folder:0, page:0 } }).unwrap();
        assert!(take_social(&directory).is_none());
        snapshot.social.busy = true; publish_friends(&directory, &snapshot).unwrap();
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err());
        snapshot.social.busy = false; snapshot.game_running = Some(true); publish_friends(&directory, &snapshot).unwrap();
        assert!(request_social(&config, &account, crate::social::Request::Inbox { folder:0, page:0 }).is_err());
    }

    #[test]
    #[ignore = "executed only in a fixture subprocess"]
    fn lock_fixture() {
        let path =
            PathBuf::from(std::env::var_os("CRYSTAL_SESSION_FIXTURE").unwrap()).join("shared.lock");
        assert_eq!(exclusive(&path).unwrap_err().raw_os_error(), Some(32));
    }

    #[test]
    fn exclusive_lock_orders_workers_and_releases_after_owner_exit() {
        let fixture = Fixture::new();
        let path = fixture.0.join("shared.lock");
        let first = exclusive(&path).unwrap();
        assert!(
            child_command("session_worker::tests::lock_fixture", &fixture.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        drop(first);
        assert!(exclusive(&path).is_ok());
    }
}
