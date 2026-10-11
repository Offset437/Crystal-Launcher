//! A detached coordinator for a multi-account Play or a Play during sign-in. Workers
//! still own authentication, keepalive, and independent game/session exits.
use super::*;
use std::{collections::HashSet, sync::mpsc};

const BATCH_PREFIX: &str = "CRYSTAL-BATCH ";
const BATCH_FRAME: usize = 512 * 1024;
const PREPARE_LIMIT: Duration = Duration::from_secs(180);

#[derive(Serialize, Deserialize)]
pub(crate) struct BatchAccount {
    pub key: String, pub config: Config, pub account: Account, pub otp: String, pub via: Plan,
    #[serde(default)]
    existing_worker: Option<Identity>,
}
impl BatchAccount {
    pub(crate) fn new(key: String, config: Config, account: Account, otp: String, via: Plan) -> Self {
        Self { key, config, account, otp, via, existing_worker: None }
    }

    pub(crate) fn existing(key: String, config: Config, account: Account, otp: String, via: Plan, session: &BackgroundSession) -> Self {
        let mut job = Self::new(key, config, account, otp, via);
        job.existing_worker = Some(session.worker);
        job
    }

    fn existing_session(&self) -> Result<Option<BackgroundSession>, String> {
        match self.existing_worker {
            Some(worker) => {
                let session = find(&self.config, &self.account).filter(|session| session.worker == worker)
                    .ok_or_else(|| "The batch account's original worker ended or changed.".to_owned())?;
                // A running game is monitored, never started again by this batch.
                // A preference edited during play must not block other accounts.
                if session.game_running() != Some(true) { session.ensure_launch_tool(self.via.tool)?; }
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }
}
impl Drop for BatchAccount { fn drop(&mut self) { self.otp.zeroize(); } }
#[derive(Serialize, Deserialize)]
struct BatchRequest {
    jobs: Vec<BatchAccount>, elapsed_ms: u64,
    #[serde(default)]
    clicked_clock: Option<crate::launch_timing::MonotonicStamp>,
}
#[derive(Serialize, Deserialize)]
enum BatchEvent {
    Progress(String, String),
    Ready(String, Identity, Started),
    Failed(String, String),
    Done,
}
fn emit_batch(event: &BatchEvent) {
    if let Ok(line) = serde_json::to_string(event) { let _ = writeln!(io::stdout().lock(), "{BATCH_PREFIX}{line}"); }
}

/// Credentials cross an anonymous pipe once. This observer may disappear:
/// the coordinator owns the gate and continues independently of the window.
pub(crate) fn launch_together(
    jobs: Vec<BatchAccount>, clicked: Instant,
    mut progress: impl FnMut(&str, &str),
    mut complete: impl FnMut(&str, Result<LaunchResult, String>),
) -> Result<(), String> {
    let mut command = Command::new(std::env::current_exe().map_err(|_| "Could not locate Crystal's batch worker.")?);
    command.arg("--batch-worker");
    launch_batch_command(command, jobs, clicked, &mut progress, &mut complete)
}

fn launch_batch_command(
    mut command: Command, mut jobs: Vec<BatchAccount>, clicked: Instant,
    progress: &mut dyn FnMut(&str, &str), complete: &mut dyn FnMut(&str, Result<LaunchResult, String>),
) -> Result<(), String> {
    prepare_dispatch(&mut jobs, || crate::viewer::gate(&crate::viewer::status(false)))?;
    let paths: std::collections::HashMap<_, _> = jobs.iter().map(|j| (j.key.clone(), record_path(&j.config, &j.account))).collect();
    let request = BatchRequest { jobs, elapsed_ms: clicked.elapsed().as_millis().min(u64::MAX as u128) as u64,
        clicked_clock: crate::launch_timing::MonotonicStamp::capture(clicked) };
    let bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| "Could not encode the batch request.")?);
    drop(request);
    if bytes.len() > BATCH_FRAME { return Err("The batch request is too large.".into()); }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW); }
    let mut child = command.spawn().map_err(|_| "Could not start Crystal's batch worker.")?;
    let sent = (|| -> io::Result<()> {
        let mut input = child.stdin.take().ok_or_else(|| io::Error::other("Missing batch input"))?;
        input.write_all(&(bytes.len() as u32).to_le_bytes())?; input.write_all(&bytes)
    })();
    drop(bytes);
    if sent.is_err() { let _ = child.kill(); let _ = child.wait(); return Err("Could not hand the batch to its worker.".into()); }
    let mut output = BufReader::new(child.stdout.take().ok_or("Missing batch output.")?);
    loop {
        let mut line = String::new();
        let size = output.by_ref().take((MAX_FRAME + 1) as u64).read_line(&mut line).map_err(|_| "Could not read batch progress.")?;
        if size == 0 { return Err("The batch worker ended before completing the game starts.".into()); }
        if size > MAX_FRAME { return Err("Batch progress is too large.".into()); }
        let Some(json) = line.trim_end().strip_prefix(BATCH_PREFIX) else { continue };
        match serde_json::from_str(json).map_err(|_| "Invalid batch progress.")? {
            BatchEvent::Progress(key, line) => progress(&key, &line),
            BatchEvent::Ready(key, worker, started) => {
                let path = paths.get(&key).ok_or("Unexpected batch account.")?;
                complete(&key, track(path.clone(), worker, started));
            }
            BatchEvent::Failed(key, error) => complete(&key, Err(error)),
            BatchEvent::Done => return Ok(()),
        }
    }
}
fn prepare_dispatch(jobs: &mut [BatchAccount], viewer_gate: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    validate_jobs(jobs)?;
    // Preserve any lifetime required by the caller and capture the other live
    // workers now. Losing either must never turn this into a cold sign-in.
    for job in jobs.iter_mut() {
        if job.existing_worker.is_some() { job.existing_session()?; }
        else { job.existing_worker = find(&job.config, &job.account).map(|session| session.worker); }
    }
    if jobs.iter().any(|job| job.existing_worker.is_none()) { viewer_gate()?; }
    Ok(())
}
fn validate_jobs(jobs: &[BatchAccount]) -> Result<(), String> {
    let mut keys = HashSet::new(); let mut accounts = HashSet::new();
    if !(1..=32).contains(&jobs.len()) || jobs.iter().any(|j| j.key.is_empty() || j.key.len() > 128
        || !keys.insert(&j.key) || !accounts.insert(record_path(&j.config, &j.account))) {
        Err("Choose one to thirty-two different accounts for a batch.".into())
    } else { Ok(()) }
}

pub(crate) fn run_batch() -> i32 {
    run_batch_with(prepare)
}
fn run_batch_with(prepare: fn(BatchAccount, Instant) -> Result<Prepared, String>) -> i32 {
    let result = (|| -> Result<(), String> {
        let mut input = io::stdin().lock(); let mut length = [0; 4];
        input.read_exact(&mut length).map_err(|_| "Missing batch request.")?;
        let length = u32::from_le_bytes(length) as usize;
        if length > BATCH_FRAME { return Err("The batch request is too large.".into()); }
        let mut bytes = Zeroizing::new(vec![0; length]);
        input.read_exact(&mut bytes).map_err(|_| "Incomplete batch request.")?;
        let request: BatchRequest = serde_json::from_slice(&bytes).map_err(|_| "Invalid batch request.")?;
        drop(bytes); drop(input);
        validate_jobs(&request.jobs)?;
        let clicked = crate::launch_timing::MonotonicStamp::restore(request.clicked_clock, request.elapsed_ms);
        coordinate(request.jobs, clicked, prepare, emit_batch);
        Ok(())
    })();
    if result.is_ok() { 0 } else { 1 }
}

struct Prepared { job: BatchAccount, session: BackgroundSession, launch: LaunchResult, check: Result<(), String>, start_game: bool }
pub(crate) fn batch_pending(config: &Config, account: &Account) -> bool {
    read_bounded(&config.account_directory(account).join("batch-owner.json"), 1024)
        .and_then(|b| serde_json::from_slice::<Identity>(&b).ok())
        .is_some_and(|owner| identity(owner.pid) == Some(owner))
}
struct Ownership { files: Vec<File>, paths: Vec<PathBuf> }
impl Ownership {
    fn acquire(jobs: &[BatchAccount]) -> Result<Self, String> {
        let owner = identity(std::process::id()).ok_or("Could not identify the batch worker.")?;
        let mut ownership = Self { files: Vec::new(), paths: Vec::new() };
        for job in jobs {
            if job.existing_session()?.is_some_and(|session| session.game_running() == Some(true)) { continue; }
            let directory = job.config.account_directory(&job.account);
            ownership.files.push(exclusive(&directory.join("batch.lock")).map_err(|_| "An account already belongs to a pending batch or game start.")?);
            // A terminated worker can leave a command behind. Its identity
            // makes that command inert; it must not prevent a fresh batch.
            if find(&job.config, &job.account).is_some()
                && (directory.join("native-control.json").exists() || directory.join("native-social.json").exists()) {
                return Err("Wait for the account's pending PlayOnline action to finish.".into());
            }
            let path = directory.join("batch-owner.json");
            save_json(&path, &owner)?;
            ownership.paths.push(path);
        }
        Ok(ownership)
    }
}
impl Drop for Ownership { fn drop(&mut self) { for path in &self.paths { let _ = fs::remove_file(path); } } }
fn prepare(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
    prepare_with(job, clicked, check_version)
}
fn check_version(config: &Config) -> Result<String, String> {
    check_version_with(config, crate::patch::check_before_launch)
}
fn check_version_with(config: &Config, check: impl FnOnce(&Path) -> crate::patch::LaunchCheck) -> Result<String, String> {
    let install = crate::pol_less::install_directory(config)?;
    // A batch uses the same fresh-check policy as one Play: known updates block,
    // and an incomparable version remains explicitly unverified in the returned note.
    crate::patch::gate(check(&install))
}
fn report_version(key: &str, log: &Path, result: &Result<String, String>, mut emit: impl FnMut(&BatchEvent)) {
    let note = match result { Ok(note) | Err(note) => note };
    // The account's worker and Windower observer also append to this existing log.
    // Keep each flattened note in one append, just like the session logger.
    if let Ok(mut file) = OpenOptions::new().append(true).open(log) {
        let time = chrono::Local::now().format("%H:%M:%S");
        let flat: String = note.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let _ = file.write_all(format!("[{time}] [launcher] {flat}\n").as_bytes());
    }
    let progress = if result.is_ok() {
        format!("{note} Ready; waiting for the other selected accounts…")
    } else { note.clone() };
    emit(&BatchEvent::Progress(key.into(), progress));
}
fn prepare_with(mut job: BatchAccount, clicked: Instant, check_version: fn(&Config) -> Result<String, String>) -> Result<Prepared, String> {
    let deadline = Instant::now() + PREPARE_LIMIT;
    let mut plan = job.via.clone(); plan.social_only = true;
    let mut otp = Zeroizing::new(std::mem::take(&mut job.otp));
    let existing = job.existing_session()?;
    if let Some(session) = existing.as_ref().filter(|session| session.game_running() == Some(true)) {
        let launch = session.clone().reconnect(|line| emit_batch(&BatchEvent::Progress(job.key.clone(), line.into())))?;
        return Ok(Prepared { session: session.clone(), job, launch, check: Ok(()), start_game: false });
    }
    let canceled = AtomicBool::new(false);
    let (session, launch, check) = std::thread::scope(|scope| {
        // launch/reconnect waits for full native idle. Its exact worker publishes authentication
        // earlier, so this one fresh request can overlap mail and presence initialization.
        let version = std::thread::Builder::new().name("batch-version-check".into()).spawn_scoped(scope, || {
            let session = loop {
                if canceled.load(Ordering::Acquire) { return Err("The batch sign-in was canceled.".to_owned()); }
                if let Some(session) = find(&job.config, &job.account) {
                    if job.existing_worker.is_some_and(|worker| worker != session.worker) {
                        return Err("The batch account's original worker changed.".into());
                    }
                    break session;
                }
                if job.existing_worker.is_some() { return Err("The batch account's original worker ended.".into()); }
                if Instant::now() >= deadline { return Err("Timed out waiting for the account to sign in.".into()); }
                std::thread::sleep(Duration::from_millis(20));
            };

            loop {
                if canceled.load(Ordering::Acquire) { return Err("The batch sign-in was canceled.".to_owned()); }
                if identity(session.worker.pid) != Some(session.worker)
                    || !read_record(&session.path).is_some_and(|record| record.worker == session.worker
                        && matches!(record.state, Event::Progress(_) | Event::Started(_))) {
                    return Err("The batch account's worker ended.".into());
                }
                if session.authenticated() { break; }
                if Instant::now() >= deadline { return Err("Timed out waiting for the account to sign in.".into()); }
                std::thread::sleep(Duration::from_millis(20));
            }
            emit_batch(&BatchEvent::Progress(job.key.clone(), "Checking the game version...".into()));
            Ok((session.worker, check_version(&job.config)))
        }).map_err(|_| "Could not start the batch game version check.")?;
        let progress = |line: &str| emit_batch(&BatchEvent::Progress(job.key.clone(), line.into()));
        let launched = match existing {
            Some(session) => session.reconnect(progress),
            None => launch_preflighted(&job.config, job.account.clone(), std::mem::take(&mut *otp), Some(plan), clicked, progress),
        };
        let launch = match launched {
            Ok(launch) => launch,
            Err(error) => { canceled.store(true, Ordering::Release); return Err(error); }
        };
        let session = match find(&job.config, &job.account) {
            Some(session) => session,
            None => { canceled.store(true, Ordering::Release); return Err("The prepared session ended.".into()); }
        };
        let idle = (|| {

            loop {
                if !launch.alive.load(Ordering::Acquire) || identity(session.worker.pid) != Some(session.worker) { return Err("The batch account's worker ended.".into()); }
                if let Some(snapshot) = read_friend_cache(&job.config.account_directory(&job.account), session.worker) {
                    if snapshot.game_running == Some(true) { return Err("This account already has a game running.".into()); }
                    if let Some(error) = snapshot.session_error { return Err(error); }
                    if snapshot.game_running == Some(false) && snapshot.social.ready && !snapshot.social.busy { break; }
                }
                if Instant::now() >= deadline { return Err("Timed out waiting for the account to finish signing in.".into()); }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        })();
        if idle.is_err() { canceled.store(true, Ordering::Release); }
        let checked = version.join().unwrap_or_else(|_| Err("The batch game version check failed.".into()));
        let check = idle.and_then(|()| {
            let (worker, result) = checked?;
            if worker != session.worker { return Err("The batch account's worker changed during its version check.".into()); }
            result
        });
        report_version(&job.key, &launch.log, &check, emit_batch);
        Ok::<_, String>((session, launch, check.map(|_| ())))
    })?;
    Ok(Prepared { job, session, launch, check, start_game: true })
}

fn coordinate(
    jobs: Vec<BatchAccount>, clicked: Instant,
    prepare: fn(BatchAccount, Instant) -> Result<Prepared, String>,
    mut emit: impl FnMut(&BatchEvent),
) {
    let keys: Vec<_> = jobs.iter().map(|j| j.key.clone()).collect();
    let ownership = match Ownership::acquire(&jobs) {
        Ok(ownership) => ownership,
        Err(error) => { for key in keys { emit(&BatchEvent::Failed(key, error.clone())); } emit(&BatchEvent::Done); return; }
    };
    let (sender, receiver) = mpsc::channel();
    for job in jobs {
        let sender = sender.clone(); let key = job.key.clone();
        let _ = std::thread::Builder::new().name("batch-sign-in".into()).spawn(move || {
            let result = prepare(job, clicked);
            let _ = sender.send((key, result));
        });
    }
    drop(sender);
    let deadline = Instant::now() + PREPARE_LIMIT;
    let mut prepared = Vec::new(); let mut error = None;
    for _ in &keys {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((_, Ok(item))) => { if let Err(reason) = &item.check { error.get_or_insert(reason.clone()); } prepared.push(item); }
            Ok((_, Err(reason))) => { error.get_or_insert(reason); }
            Err(_) => { error.get_or_insert("A batch account did not become ready in time.".into()); break; }
        }
    }
    if error.is_none() {
        let starts: Vec<_> = prepared.iter().filter(|item| item.start_game).collect();
        if !starts.is_empty() {
            if let Err(reason) = release_prepared(&starts) { error = Some(reason); }
        }
    }
    drop(ownership);
    // Keep monitors for every live prepared session, also on a failed check.
    for item in &prepared {
        emit(&BatchEvent::Ready(item.job.key.clone(), item.session.worker, Started {
            pid: item.launch.pid,
            detail: if !item.start_game { "FFXI is already running for this account.".into() }
                else if error.is_none() { "FFXI started with the selected accounts.".into() } else { item.launch.detail.clone() },
            log: item.launch.log.clone(), timing: item.launch.timing.clone(),
        }));
    }
    if let Some(error) = error {
        for key in keys {
            if !prepared.iter().any(|item| item.job.key == key && !item.start_game) {
                emit(&BatchEvent::Failed(key, error.clone()));
            }
        }
    }
    emit(&BatchEvent::Done);
}

fn release_prepared(prepared: &[&Prepared]) -> Result<(), String> {
    let id = crate::pol_less::pipe::random_hex().map_err(|_| "Could not create the batch gate.")?;
    let (gate, tokens) = start_gate::Coordinator::new(&id, prepared.len())?;
    // File controls are bound to exact worker lifetimes. The native gate itself
    // belongs to this detached coordinator, not the UI or any one game.
    for (item, token) in prepared.iter().zip(tokens) {
        if !healthy(item) { return Err("A batch session ended before preparation.".into()); }
        save_json(&item.job.config.account_directory(&item.job.account).join("native-batch.json"),
            &BatchControl { worker: item.session.worker, token })?;
        super::wake::signal(item.session.worker);
    }
    let until = Instant::now() + start_gate::LIMIT;
    while !gate.ready() {
        if gate.canceled() { return Err("A prepared game canceled the batch.".into()); }
        if prepared.iter().any(|item| !healthy(item)) { return Err("A batch session ended while preparing its game.".into()); }
        if Instant::now() >= until { return Err("Timed out preparing all games; the batch was canceled.".into()); }
        std::thread::sleep(Duration::from_millis(5));
    }
    if prepared.iter().any(|item| !healthy(item)) { return Err("A batch session ended before release.".into()); }
    gate.release()?;
    while !gate.started() {
        if prepared.iter().any(|item| !healthy(item)) || Instant::now() >= until { return Err("A prepared game ended during batch release.".into()); }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn healthy(item: &Prepared) -> bool {
    item.launch.alive.load(Ordering::Acquire) && identity(item.session.worker.pid) == Some(item.session.worker)
        && identity(item.launch.pid).is_some()
        && read_friend_cache(&item.job.config.account_directory(&item.job.account), item.session.worker)
            .is_none_or(|s| s.session_error.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    static FIXTURE_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("crystal-batch-{}", crate::pol_less::pipe::random_hex().unwrap()));
        fs::create_dir_all(&path).unwrap(); path
    }
    fn job(root: &Path, key: &str) -> BatchAccount {
        BatchAccount::new(key.into(),
            Config { experiment: root.into(), repository: root.into(), game_install: None, dat_overlays: Vec::new() },
            Account { label: key.into(), mode: backend::AccountMode::Retail, server: String::new(), pol_id: key.into(), pol_password: "fixture-secret".into(), se_id: String::new(), se_password: String::new(), region: 0, launch_with: None, ashita_boot_ini: None, windower_profile: None },
            "123456".into(), Plan { tool: crate::pol_less::Tool::Windower, settings: Default::default(), social_only: false, invisible_login: false, auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default() })
    }
    fn watch_fixture_start(job: &BatchAccount) {
        let directory = job.config.account_directory(&job.account);
        fs::create_dir_all(&directory).unwrap();
        let stop = job.config.experiment.join("stop-fixture");
        let started = job.config.experiment.join(format!("{}-started", job.key));
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && !stop.exists() {
                if let Some(bytes) = read_bounded(&directory.join("native-batch.json"), 1024) {
                    let request: BatchControl = serde_json::from_slice(&bytes).unwrap();
                    // The actual native owners signal this only after all setup.
                    if start_gate::Participant::open(&request.token).and_then(|gate| gate.wait()).is_ok() {
                        fs::write(started, []).unwrap();
                        FIXTURE_STARTED.fetch_add(1, Ordering::SeqCst);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    }
    fn fixture_prepare(job: BatchAccount, _: Instant) -> Result<Prepared, String> {
        if job.key == "cold" { std::thread::sleep(Duration::from_millis(40)); }
        let worker = identity(std::process::id()).unwrap();
        watch_fixture_start(&job);
        let alive = Arc::new(AtomicBool::new(job.key != "dead"));
        let check = if job.key == "fail" { Err("Missing game version.".into()) } else { Ok(()) };
        Ok(Prepared { session: BackgroundSession { path: record_path(&job.config, &job.account), worker },
            launch: LaunchResult { pid: worker.pid, detail: "Fixture idle session".into(), alive, game_running: Arc::new(AtomicBool::new(false)), outcome: Arc::new(Mutex::new(None)), log: root_log(&job), timing: String::new() }, job, check, start_game: true })
    }
    fn root_log(job: &BatchAccount) -> PathBuf { job.config.experiment.join("fixture.log") }
    fn until(mut test: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !test() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5)); }
    }
    #[test]
    fn mixed_warm_cold_batch_releases_only_after_both_prepare() {
        let root = root(); let mut events = Vec::new();
        coordinate(vec![job(&root, "warm"), job(&root, "cold")], Instant::now(), fixture_prepare, |e| events.push(serde_json::to_string(e).unwrap()));
        until(|| root.join("warm-started").exists() && root.join("cold-started").exists());
        assert_eq!(events.iter().filter(|e| e.contains("Ready")).count(), 2);
        assert!(!events.iter().any(|e| e.contains("Failed")));
        assert!(!batch_pending(&job(&root,"warm").config, &job(&root,"warm").account));
    }

    #[test]
    fn one_running_account_does_not_enter_or_block_the_other_five_start_gates() {
        static CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn prepare_six(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            let prepared = prepare_with(job, clicked, |_| {
                CHECKS.fetch_add(1, Ordering::SeqCst);
                Ok("Fixture version is current.".into())
            })?;
            if prepared.start_game { watch_fixture_start(&prepared.job); }
            Ok(prepared)
        }
        CHECKS.store(0, Ordering::SeqCst);
        let root = root();
        let worker = identity(std::process::id()).unwrap();
        let keys = ["running", "second", "third", "fourth", "fifth", "sixth"];
        let mut jobs = Vec::new();
        let mut paths = Vec::new();
        for key in keys {
            let request = job(&root, key);
            let path = record_path(&request.config, &request.account);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let log = root.join(format!("{key}.log")); fs::write(&log, []).unwrap();
            save_record(&path, &Record { worker, state: Event::Started(Started {
                pid: worker.pid, detail: "Existing session".into(), log, timing: String::new(),
            }), social_only: true, authenticated: true, native_batch: true,
                launch_tool: Some(crate::pol_less::Tool::Windower) }).unwrap();
            let mut snapshot = crate::friends::Snapshot { game_running: Some(key == "running"), ..Default::default() };
            snapshot.social.ready = true;
            publish_friends(path.parent().unwrap(), &snapshot).unwrap();
            paths.push(path); jobs.push(request);
        }
        // A changed next-launch preference and a queued social action on the
        // running account must not become conditions for starting the other five.
        jobs[0].via.tool = crate::pol_less::Tool::Vanilla;
        let running_directory = paths[0].parent().unwrap();
        let queued = b"existing running account action";
        fs::write(running_directory.join("native-control.json"), queued).unwrap();
        prepare_dispatch(&mut jobs, || panic!("all six workers already exist")).unwrap();
        let mut events = Vec::new();
        coordinate(jobs, Instant::now(), prepare_six, |event| events.push(serde_json::to_string(event).unwrap()));
        until(|| keys[1..].iter().all(|key| root.join(format!("{key}-started")).exists()));
        assert_eq!(CHECKS.load(Ordering::SeqCst), 5);
        assert_eq!(events.iter().filter(|event| event.contains("Ready")).count(), 6);
        assert!(!events.iter().any(|event| event.contains("Failed")), "{events:?}");
        assert!(!root.join("running-started").exists());
        assert!(!running_directory.join("native-batch.json").exists());
        assert_eq!(fs::read(running_directory.join("native-control.json")).unwrap(), queued);
        fs::write(root.join("stop-fixture"), []).unwrap();
        for path in paths { update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap(); }
    }
    fn signing_in_record(job: &BatchAccount) -> Identity {
        let worker = identity(std::process::id()).unwrap();
        let path = record_path(&job.config, &job.account);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_record(&path, &Record { worker, state: Event::Progress("Fixture signing in".into()),
            social_only: true, authenticated: false, native_batch: true, launch_tool: None }).unwrap();
        worker
    }

    #[test]
    fn batch_reuse_refuses_an_injected_host_for_vanilla_before_any_version_check() {
        let root = root(); let mut request = job(&root, "ABCD1234");
        let worker = signing_in_record(&request);
        request.existing_worker = Some(worker);
        let path = record_path(&request.config, &request.account);
        update_record(&path, worker, |record| record.launch_tool = Some(crate::pol_less::Tool::Windower)).unwrap();
        request.via.tool = crate::pol_less::Tool::Vanilla;
        let error = prepare_with(request, Instant::now(), |_| panic!("must stop before checking the version")).err().unwrap();
        assert!(error.contains("before switching launch methods"), "{error}");
        assert!(!path.parent().unwrap().join("native-batch.json").exists());
        update_record(&path, worker, |record| record.launch_tool = Some(crate::pol_less::Tool::Vanilla)).unwrap();
        let mut request = job(&root, "ABCD1234");
        request.existing_worker = Some(worker);
        request.via.tool = crate::pol_less::Tool::Vanilla;
        assert!(request.existing_session().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
    struct ReplacementWorker { child: std::process::Child, worker: Identity }
    impl ReplacementWorker {
        fn new() -> Self {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "session_worker::batch::tests::identity_fixture", "--ignored"])
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let worker = identity(child.id()).unwrap();
            Self { child, worker }
        }
    }
    impl Drop for ReplacementWorker { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }
    #[test]
    #[ignore = "offline identity subprocess only"]
    fn identity_fixture() { let _ = io::stdin().read(&mut [0]); }
    #[test]
    fn dispatch_skips_viewer_for_warm_jobs_but_gates_a_mixed_batch_once() {
        let root = root();
        let mut jobs = vec![job(&root, "first"), job(&root, "second")];
        let worker = signing_in_record(&jobs[0]);
        signing_in_record(&jobs[1]);
        prepare_dispatch(&mut jobs, || panic!("existing sessions must not query the Viewer")).unwrap();
        let serialized = Zeroizing::new(serde_json::to_vec(&jobs).unwrap());
        let captured: Vec<BatchAccount> = serde_json::from_slice(&serialized).unwrap();
        assert!(captured.iter().all(|job| job.existing_worker == Some(worker)));
        fs::remove_file(record_path(&jobs[1].config, &jobs[1].account)).unwrap();
        let mut jobs = vec![job(&root, "first"), job(&root, "second")];
        let mut checks = 0;
        let result = prepare_dispatch(&mut jobs, || { checks += 1; Err("Fixture Viewer update required".into()) });
        assert_eq!(checks, 1);
        assert_eq!(result, Err("Fixture Viewer update required".into()));
        assert_eq!(jobs[0].existing_worker, Some(worker));
        assert_eq!(jobs[1].existing_worker, None);
    }
    #[test]
    fn a_worker_required_by_the_caller_cannot_be_reclassified_before_dispatch() {
        for replaced in [false, true] {
            let root = root();
            let mut selected = job(&root, "existing");
            let worker = signing_in_record(&selected);
            let session = find(&selected.config, &selected.account).unwrap();
            let mut jobs = vec![BatchAccount::existing(selected.key.clone(), selected.config.clone(),
                selected.account.clone(), std::mem::take(&mut selected.otp), selected.via.clone(), &session)];
            let path = record_path(&jobs[0].config, &jobs[0].account);
            let replacement = replaced.then(ReplacementWorker::new);
            if replaced {
                save_record(&path, &Record { worker: replacement.as_ref().unwrap().worker,
                    state: Event::Progress("Replacement worker".into()), social_only: true,
                    authenticated: false, native_batch: true, launch_tool: None }).unwrap();
                assert_ne!(find(&jobs[0].config, &jobs[0].account).unwrap().worker, worker);
            } else { fs::remove_file(&path).unwrap(); }
            let error = prepare_dispatch(&mut jobs, || panic!("a selected worker must not become cold"))
                .expect_err("the caller's original worker is required");
            assert!(error.contains("original worker ended or changed"), "{error}");
            assert_eq!(jobs[0].existing_worker, Some(worker));
            assert!(!path.with_extension("lock").exists());
        }
    }
    #[test]
    fn an_existing_worker_that_disappears_or_changes_never_becomes_a_cold_launch() {
        for replaced in [false, true] {
            let root = root();
            let mut jobs = vec![job(&root, "existing")];
            let worker = signing_in_record(&jobs[0]);
            prepare_dispatch(&mut jobs, || panic!("the existing worker needs no Viewer query")).unwrap();
            let path = record_path(&jobs[0].config, &jobs[0].account);
            let replacement = replaced.then(ReplacementWorker::new);
            if replaced {
                save_record(&path, &Record { worker: replacement.as_ref().unwrap().worker,
                    state: Event::Progress("Replacement worker".into()), social_only: true,
                    authenticated: false, native_batch: true, launch_tool: None }).unwrap();
                assert_ne!(find(&jobs[0].config, &jobs[0].account).unwrap().worker, worker);
            } else { fs::remove_file(&path).unwrap(); }
            let error = prepare_with(jobs.remove(0), Instant::now(), |_| panic!("must stop before the version check"))
                .err().expect("the dispatched worker must still exist");
            assert!(error.contains("original worker ended or changed"), "{error}");
            assert!(!path.with_extension("lock").exists(), "no replacement session worker was started");
            assert!(!path.parent().unwrap().join("native-batch.json").exists());
        }
    }
    #[test]
    fn batch_allows_explicitly_unverified_versions_but_known_updates_block_every_game() {
        fn unverified_check(config: &Config) -> Result<String, String> {
            check_version_with(config, |install| {
                let mut report = crate::patch::local_report(install);
                assert_eq!(report.local_version, "30260904_1");
                report.detail = "Fixture official server is unreachable.".into();
                crate::patch::LaunchCheck::Unverified(report)
            })
        }
        fn update_check(config: &Config) -> Result<String, String> {
            check_version_with(config, |_| crate::patch::LaunchCheck::Update(crate::patch::PatchReport {
                state: crate::patch::PatchState::Required,
                local_version: "30260904_1".into(), remote_version: "30260905_0".into(),
                detail: "Official fixture reply requires an update.".into(), source_url: None, files_verified: None,
            }))
        }
        fn prepare_policy(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            let update = job.key == "update";
            // The actual native gate protocol is still exercised by fixture participants;
            // prepare_with owns reconnect, full idle, the version check and its reporting.
            let fixture = fixture_prepare(job, clicked)?;
            prepare_with(fixture.job, clicked, if update { update_check } else { unverified_check })
        }
        for update in [false, true] {
            let root = root();
            fs::write(root.join("FFXi.dll"), []).unwrap();
            fs::write(root.join("FFXiMain.dll"), []).unwrap();
            // Unreachable servers allow a readable installed version, while
            // missing/corrupt records now require initialization/repair.
            let record = crate::patch::encode_applied_record("30260904_1", crate::patch::applied_seed(&root).unwrap()).unwrap();
            fs::write(root.join("patch.ver"), record).unwrap();
            let mut config = job(&root, "warm").config.clone();
            config.game_install = Some(root.clone());
            let single_note = unverified_check(&config).unwrap();
            assert!(single_note.contains("could not be compared") && !single_note.contains("matches"));
            let keys = ["warm", if update { "update" } else { "unknown" }];
            let worker = identity(std::process::id()).unwrap();
            let mut jobs = Vec::new(); let mut records = Vec::new();
            for key in keys {
                let mut job = job(&root, key); job.config.game_install = Some(root.clone());
                let path = record_path(&job.config, &job.account);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let log = root.join(format!("{key}.crystal.log")); fs::write(&log, []).unwrap();
                save_record(&path, &Record { worker, state: Event::Started(Started {
                    pid: worker.pid, detail: "Existing native idle fixture".into(), log, timing: String::new(),
                }), social_only: true, authenticated: true, native_batch: true, launch_tool: None }).unwrap();
                let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
                snapshot.social.ready = true;
                publish_friends(path.parent().unwrap(), &snapshot).unwrap();
                records.push(path); jobs.push(job);
            }
            let mut errors = Vec::new();
            coordinate(jobs, Instant::now(), prepare_policy, |event| {
                if let BatchEvent::Failed(_, error) = event { errors.push(error.clone()); }
            });
            if update {
                assert_eq!(errors.len(), 2);
                assert!(errors.iter().all(|error| error.starts_with(crate::patch::UPDATE_REQUIRED)));
                assert!(!root.join("warm-started").exists() && !root.join("update-started").exists());
                assert!(fs::read_to_string(root.join("update.crystal.log")).unwrap().contains(crate::patch::UPDATE_REQUIRED));
            } else {
                assert!(errors.is_empty(), "{errors:?}");
                until(|| root.join("warm-started").exists() && root.join("unknown-started").exists());
                assert!(fs::read_to_string(root.join("unknown.crystal.log")).unwrap().contains(&single_note));
            }
            assert!(fs::read_to_string(root.join("warm.crystal.log")).unwrap().contains(&single_note));
            fs::write(root.join("stop-fixture"), []).unwrap();
            for path in records { update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap(); }

            // The same reporting path carries the exact unverified note to GUI progress
            // and retains it in the account log instead of labeling it current.
            let log = root.join("progress.crystal.log"); fs::write(&log, []).unwrap();
            let mut progress = Vec::new();
            report_version("unknown", &log, &Ok(single_note.clone()), |event| {
                if let BatchEvent::Progress(key, note) = event { progress.push((key.clone(), note.clone())); }
            });
            assert_eq!(progress.len(), 1);
            assert_eq!(progress[0].0, "unknown");
            assert!(progress[0].1.starts_with(&single_note));
            assert!(fs::read_to_string(log).unwrap().contains(&single_note));
        }
    }
    #[test]
    fn refused_version_or_dead_worker_cancels_every_start_and_retains_monitors() {
        for failed in ["fail", "dead"] {
            let root = root(); let mut ready = 0; let mut errors = 0;
            coordinate(vec![job(&root, "warm"), job(&root, failed)], Instant::now(), fixture_prepare, |e| match e {
                BatchEvent::Ready(..) => ready += 1, BatchEvent::Failed(..) => errors += 1, _ => {},
            });
            fs::write(root.join("stop-fixture"), []).unwrap();
            assert_eq!((ready, errors), (2, 2));
            assert!(!root.join("warm-started").exists()); assert!(!root.join(format!("{failed}-started")).exists());
        }
    }
    #[test]
    fn ownership_blocks_overlap_and_controls_but_stale_owner_does_not() {
        let root = root(); let jobs = vec![job(&root, "warm"), job(&root, "cold")];
        let ownership = Ownership::acquire(&jobs).unwrap();
        assert!(batch_pending(&jobs[0].config, &jobs[0].account)); assert!(Ownership::acquire(&jobs).is_err());
        assert!(request_native_control(&jobs[0].config, &jobs[0].account, NativeControl::StartGame).is_err());
        drop(ownership); assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
        save_json(&jobs[0].config.account_directory(&jobs[0].account).join("batch-owner.json"), &Identity { pid: std::process::id(), created: 0 }).unwrap();
        assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
    }

    #[test]
    fn orphaned_commands_do_not_block_a_new_batch_but_live_commands_do() {
        let root = root();
        let jobs = vec![job(&root, "orphaned")];
        let directory = jobs[0].config.account_directory(&jobs[0].account);
        fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        for name in ["native-control.json", "native-social.json"] {
            fs::write(directory.join(name), b"orphaned command").unwrap();
        }
        let path = record_path(&jobs[0].config, &jobs[0].account);
        save_record(&path, &Record { worker: Identity { created: 0, ..worker }, state: Event::Progress("old worker".into()),
            social_only: true, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        drop(Ownership::acquire(&jobs).expect("a dead worker's files cannot execute in the replacement session"));
        save_record(&path, &Record { worker, state: Event::Progress("live worker".into()),
            social_only: true, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
        assert!(Ownership::acquire(&jobs).is_err(), "the live worker still owns its queued action");
        assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
    }
    #[test]
    fn preparation_reuses_warm_and_still_signing_in_workers() {
        for signing_in in [false, true] {
            let root = root(); let job = job(&root, "warm");
            let worker = identity(std::process::id()).unwrap();
            let path = record_path(&job.config, &job.account);
            let started = Event::Started(Started { pid: worker.pid, detail: "Existing worker".into(), log: root.join("fixture.log"), timing: String::new() });
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            save_record(&path, &Record { worker, state: if signing_in { Event::Progress("Still signing in".into()) } else { started.clone() }, social_only: true, authenticated: false, native_batch: false, launch_tool: None }).unwrap();
            let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
            snapshot.social.ready = true;
            publish_friends(path.parent().unwrap(), &snapshot).unwrap();
            if signing_in {
                let path = path.clone();
                std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(40)); update_record(&path, worker, |record| record.state = started).unwrap(); });
            }
            // A second process would reject the test executable's --session-worker;
            // reaching this injected version check proves exact-worker reconnect.
            let prepared = prepare_with(job, Instant::now(), |_| Ok("Fixture version matches.".into())).unwrap();
            assert_eq!(prepared.launch.pid, worker.pid); assert!(prepared.check.is_ok());
            assert!(!prepared.job.config.account_directory(&prepared.job.account).join("native-control.json").exists());
            update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap();
        }
    }
    #[test]
    fn one_account_play_checks_on_typed_authentication_but_waits_for_all_native_services() {
        type Signals = (mpsc::Sender<()>, mpsc::Receiver<Result<String, String>>, mpsc::Sender<()>);
        static SIGNALS: Mutex<Option<Signals>> = Mutex::new(None);
        static CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        for refused in [false, true] {
            CHECKS.store(0, Ordering::SeqCst);
            let root = root(); let job = job(&root, "pending-services");
            let worker = identity(std::process::id()).unwrap();
            let path = record_path(&job.config, &job.account);
            let directory = path.parent().unwrap().to_path_buf();
            fs::create_dir_all(&directory).unwrap();
            save_record(&path, &Record { worker, state: Event::Progress("fixture locale-independent sign-in".into()), social_only: true, authenticated: false, native_batch: true, launch_tool: None }).unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (verdict_tx, verdict_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            *SIGNALS.lock().unwrap() = Some((entered_tx, verdict_rx, finished_tx));
            let mut jobs = vec![job];
            prepare_dispatch(&mut jobs, || panic!("reuse the existing sign-in")).unwrap();
            let preparing = std::thread::spawn(move || {
                let mut events = Vec::new();
                coordinate(jobs, Instant::now(), |job, clicked| {
                    let prepared = prepare_with(job, clicked, |_| {
                        CHECKS.fetch_add(1, Ordering::SeqCst);
                        let (entered, verdict, finished) = SIGNALS.lock().unwrap().take().expect("one version request");
                        entered.send(()).unwrap();
                        let result = verdict.recv_timeout(Duration::from_secs(10)).unwrap();
                        finished.send(()).unwrap();
                        result
                    })?;
                    if prepared.check.is_ok() { watch_fixture_start(&prepared.job); }
                    Ok(prepared)
                }, |event| events.push(serde_json::to_string(event).unwrap()));
                events
            });
            assert!(matches!(entered_rx.recv_timeout(Duration::from_millis(30)), Err(mpsc::RecvTimeoutError::Timeout)),
                "the version check must wait for typed authentication");
            publish_authenticated(&directory).unwrap();
            entered_rx.recv_timeout(Duration::from_secs(10)).expect("check starts while native services remain pending");
            assert!(matches!(read_record(&path).unwrap().state, Event::Progress(_)));
            assert!(!directory.join("friends.json").exists());
            verdict_tx.send(if refused { Err("fixture version refusal".into()) } else { Ok("Fixture version matches.".into()) }).unwrap();
            finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(!preparing.is_finished(), "the version result cannot replace full native idle");
            assert!(!directory.join("native-batch.json").exists());
            let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
            publish_friends(&directory, &snapshot).unwrap();
            update_record(&path, worker, |record| record.state = Event::Started(Started {
                pid: worker.pid, detail: "Native startup services complete".into(), log: root.join("fixture.log"), timing: String::new(),
            })).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            assert!(!preparing.is_finished(), "idle alone does not publish social readiness");
            snapshot.social.ready = true; snapshot.social.busy = true;
            publish_friends(&directory, &snapshot).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            assert!(!directory.join("native-batch.json").exists(), "a busy native owner cannot start");
            snapshot.social.busy = false;
            publish_friends(&directory, &snapshot).unwrap();
            let events = preparing.join().unwrap();
            assert_eq!(events.iter().any(|event| event.contains("Failed")), refused);
            assert_eq!(events.iter().filter(|event| event.contains("Ready")).count(), 1);
            assert_eq!(CHECKS.load(Ordering::SeqCst), 1);
            if !refused { until(|| root.join("pending-services-started").exists()); }
            assert_eq!(root.join("pending-services-started").exists(), !refused);
            assert!(!directory.join("native-control.json").exists());
            assert!(!path.with_extension("lock").exists(), "the original worker was reused");
            update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap();
        }
    }
    #[test]
    #[ignore = "offline batch subprocess only"]
    fn batch_fixture() {
        let code = run_batch_with(fixture_prepare);
        if code == 0 { until(|| FIXTURE_STARTED.load(Ordering::SeqCst) == 2); }
        // libtest prints its result to stdout; the real headless worker exits
        // directly and deliberately tolerates a closed observer pipe.
        std::process::exit(code);
    }
    #[test]
    fn batch_continues_after_its_window_observer_closes() {
        let root = root();
        let request = BatchRequest { jobs: vec![job(&root,"warm"), job(&root,"cold")], elapsed_ms: 0, clicked_clock: None };
        let bytes = Zeroizing::new(serde_json::to_vec(&request).unwrap());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session_worker::batch::tests::batch_fixture", "--ignored", "--nocapture"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap(); input.write_all(&bytes).unwrap(); drop(input);
        // Let libtest finish its own initial stdout banner before detaching;
        // the actual worker uses fallible writes throughout.
        until(|| batch_pending(&request.jobs[0].config, &request.jobs[0].account));
        drop(child.stdout.take()); // Same broken observer pipe as closing iced.
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(root.join("warm-started").exists()); assert!(root.join("cold-started").exists());
    }
}
