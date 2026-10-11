//! Fixing a missing component: download (with progress and cancel), verify
//! the Microsoft signature, then run the installer and map its exit code.
//! Only ever started by an explicit click on the System tab.
use super::detect::Component;
use crate::i18n::{t, tf};
use iced::futures::{SinkExt, Stream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DIRECTX_URL: &str =
    "https://download.microsoft.com/download/8/4/A/84A35BF1-DAFE-4AE8-82AF-AD2AE20B6B14/directx_Jun2010_redist.exe";
const VC_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x86.exe";
/// Refuse anything implausibly large for these two installers.
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Downloading { done: u64, total: Option<u64> },
    Verifying,
    Extracting,
    Installing,
}

impl Phase {
    pub(crate) fn cancellable(&self) -> bool {
        matches!(self, Phase::Downloading { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    RestartRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Phase(Phase),
    Finished(Result<Outcome, String>),
}

/// What a fix does, shown before the user confirms it.
pub(crate) struct Plan {
    pub(crate) steps: Vec<&'static str>,
    pub(crate) command: &'static str,
    pub(crate) download: Option<(&'static str, &'static str)>,
}

pub(crate) fn plan(component: Component) -> Plan {
    match component {
        Component::DirectPlay => Plan {
            steps: vec![
                t("Turns on the DirectPlay feature with Windows' own servicing tool (DISM). Nothing is downloaded by the launcher; Windows may fetch the files from Windows Update."),
            ],
            command: "dism.exe /online /enable-feature /featurename:DirectPlay /all /norestart",
            download: None,
        },
        Component::DirectX9 => Plan {
            steps: vec![
                t("Downloads Microsoft's DirectX End-User Runtime (June 2010) from download.microsoft.com."),
                t("Checks the file is validly signed by Microsoft, unpacks it to a temporary folder and runs its setup silently."),
            ],
            command: "directx_Jun2010_redist.exe /Q /T:<temp>  then  DXSETUP.exe /silent",
            download: Some(("directx_Jun2010_redist.exe", t("about 100 MB"))),
        },
        Component::VcRedist => Plan {
            steps: vec![
                t("Downloads Microsoft's Visual C++ 2015–2022 Redistributable (x86) from aka.ms/vs/17/release."),
                t("Checks the file is validly signed by Microsoft, then runs it silently."),
            ],
            command: "vc_redist.x86.exe /install /quiet /norestart",
            download: Some(("vc_redist.x86.exe", t("about 14 MB"))),
        },
    }
}

// ---- exit codes (pure) --------------------------------------------------------

const ERROR_CANCELLED: u32 = 1223;
const ERROR_SUCCESS_REBOOT_INITIATED: u32 = 1641;
const ERROR_SUCCESS_REBOOT_REQUIRED: u32 = 3010;
const ERROR_ELEVATION_REQUIRED: u32 = 740;

fn declined() -> String {
    t("Windows' permission prompt was declined, so nothing was changed.").into()
}

pub(crate) fn dism_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED | ERROR_SUCCESS_REBOOT_INITIATED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED => Err(declined()),
        ERROR_ELEVATION_REQUIRED => Err(t("Enabling DirectPlay needs administrator rights.").into()),
        0x800F081F | 0x800F0950 | 0x800F0906 | 0x800F0907 => Err(tf(
            "Windows couldn't find the DirectPlay files ({code}). Connect to the internet so Windows Update can supply them, or enable it manually.",
            &[("code", &format!("0x{code:08X}"))],
        )),
        other => Err(tf(
            "DISM couldn't enable DirectPlay (exit code {code}).",
            &[("code", &format!("0x{other:08X}"))],
        )),
    }
}

pub(crate) fn vc_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        // A same-or-newer version is already installed.
        1638 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED | ERROR_SUCCESS_REBOOT_INITIATED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED | 1602 => Err(declined()),
        1618 => Err(t("Another installation is in progress. Wait for it to finish and try again.").into()),
        other => Err(tf("The Visual C++ installer failed (exit code {code}).", &[("code", &(other as i32))])),
    }
}

pub(crate) fn dxsetup_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED => Err(declined()),
        other => Err(tf("DirectX setup failed (exit code {code}).", &[("code", &(other as i32))])),
    }
}

pub(crate) fn extract_outcome(code: u32) -> Result<(), String> {
    match code {
        0 => Ok(()),
        ERROR_CANCELLED => Err(declined()),
        other => Err(tf("Couldn't unpack the DirectX runtime (exit code {code}).", &[("code", &(other as i32))])),
    }
}

#[cfg(windows)]
pub(crate) fn system32() -> PathBuf {
    super::detect::sys::system32()
}
#[cfg(not(windows))]
pub(crate) fn system32() -> PathBuf {
    PathBuf::from("/")
}

/// Whether a download's final (post-redirect) host is Microsoft's.
pub(crate) fn trusted_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "microsoft.com" || host.ends_with(".microsoft.com")
}

fn trusted_download_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| host == "aka.ms" || trusted_host(host))
}

// ---- the job ----------------------------------------------------------------

/// A temporary folder removed when the job ends (or is cancelled).
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("CrystalLauncher-components-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).map_err(|e| tf("Couldn't create a temporary folder: {error}", &[("error", &e)]))?;
        Ok(Self(dir))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Out = iced::futures::channel::mpsc::Sender<Event>;

/// The event stream for one fix. Dropping it (Cancel) stops a download and
/// removes the partial file; an installer that has started is left to finish.
pub(crate) fn job(component: Component) -> impl Stream<Item = Event> {
    iced::stream::channel(8, async move |mut out: Out| {
        let result = run(component, &mut out).await;
        let _ = out.send(Event::Finished(result)).await;
    })
}

async fn run(component: Component, out: &mut Out) -> Result<Outcome, String> {
    match component {
        Component::DirectPlay => {
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(|| {
                let dism = system32().join("dism.exe");
                run_hidden(&dism, "/online /enable-feature /featurename:DirectPlay /all /norestart", None)
                    .and_then(dism_outcome)
            })
            .await
        }
        Component::VcRedist => {
            let scratch = Scratch::new()?;
            let exe = scratch.0.join("vc_redist.x86.exe");
            download(VC_URL, &exe, out).await?;
            let _ = out.send(Event::Phase(Phase::Verifying)).await;
            let (scratch, lock) = blocking(move || {
                let lock = verified(&exe)?;
                Ok((scratch, (exe, lock)))
            })
            .await?;
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(move || {
                let _scratch = scratch;
                let (exe, _lock) = lock;
                run_hidden(&exe, "/install /quiet /norestart", None).and_then(vc_outcome)
            })
            .await
        }
        Component::DirectX9 => {
            let scratch = Scratch::new()?;
            let exe = scratch.0.join("directx_Jun2010_redist.exe");
            download(DIRECTX_URL, &exe, out).await?;
            let _ = out.send(Event::Phase(Phase::Verifying)).await;
            let (scratch, lock) = blocking(move || {
                let lock = verified(&exe)?;
                Ok((scratch, (exe, lock)))
            })
            .await?;
            let _ = out.send(Event::Phase(Phase::Extracting)).await;
            let (scratch, setup) = blocking(move || {
                let (exe, _lock) = lock;
                let target = scratch.0.join("dx");
                std::fs::create_dir(&target).map_err(|e| tf("Couldn't create a temporary folder: {error}", &[("error", &e)]))?;
                run_hidden(&exe, &format!("/Q /T:\"{}\"", target.display()), None).and_then(extract_outcome)?;
                let setup = target.join("DXSETUP.exe");
                let setup_lock = verified(&setup)?;
                Ok((scratch, (setup, setup_lock, target)))
            })
            .await?;
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(move || {
                let _scratch = scratch;
                let (setup, _lock, target) = setup;
                run_hidden(&setup, "/silent", Some(&target)).and_then(dxsetup_outcome)
            })
            .await
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Err(t("The installer worker stopped unexpectedly.").into()))
}

/// Opens `path` denying writes and deletes, then checks its signature. The
/// returned handle must stay open until the file has run.
fn verified(path: &Path) -> Result<std::fs::File, String> {
    let file = open_locked(path)?;
    super::trust::verify_microsoft(path, &file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_locked(path: &Path) -> Result<std::fs::File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(path)
        .map_err(|e| tf("Couldn't open {path}: {error}", &[("path", &path.display()), ("error", &e)]))
}
#[cfg(not(windows))]
fn open_locked(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::open(path).map_err(|e| tf("Couldn't open {path}: {error}", &[("path", &path.display()), ("error", &e)]))
}

async fn download(url: &str, dest: &Path, out: &mut Out) -> Result<(), String> {
    use std::io::Write;
    let _ = out.send(Event::Phase(Phase::Downloading { done: 0, total: None })).await;
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        // Includes the response headers: connect_timeout alone lets a server
        // accept the connection and then leave the job waiting indefinitely.
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error(t("Too many prerequisite download redirects."))
            } else if trusted_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error(t("The download left Microsoft's HTTPS servers, so it was stopped."))
            }
        }))
        .user_agent("CrystalLauncher/0.1")
        .build()
        .map_err(|e| tf("Couldn't start the download: {error}", &[("error", &e)]))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| t("Couldn't reach Microsoft's download server. Check your internet connection.").to_owned())?;
    if !response.status().is_success() {
        return Err(tf("Microsoft's download server answered {status}.", &[("status", &response.status())]));
    }
    if !trusted_download_url(response.url()) || !response.url().host_str().is_some_and(trusted_host) {
        return Err(t("The download was redirected away from microsoft.com, so it was stopped.").into());
    }
    let total = response.content_length();
    if total.is_some_and(|size| size > MAX_DOWNLOAD) {
        return Err(t("The download is larger than expected, so it was stopped.").into());
    }
    let mut file = std::fs::File::create(dest).map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| t("The download stalled. Check your internet connection and try again.").to_owned())?
            .map_err(|_| t("The download was interrupted. Check your internet connection and try again.").to_owned())?;
        let Some(chunk) = chunk else { break };
        done += chunk.len() as u64;
        if done > MAX_DOWNLOAD {
            return Err(t("The download is larger than expected, so it was stopped.").into());
        }
        file.write_all(&chunk).map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
        if last.elapsed() >= Duration::from_millis(120) {
            last = Instant::now();
            let _ = out.send(Event::Phase(Phase::Downloading { done, total })).await;
        }
    }
    if total.is_some_and(|size| size != done) {
        return Err(t("The download ended early. Try again.").into());
    }
    file.sync_all().map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
    let _ = out.send(Event::Phase(Phase::Downloading { done, total: Some(done) })).await;
    Ok(())
}

/// Runs `exe args` without a console window and returns its exit code. An
/// elevated launcher starts it directly; otherwise Windows asks for
/// permission (the "runas" verb) first.
#[cfg(windows)]
pub(crate) fn run_hidden(exe: &Path, args: &str, dir: Option<&Path>) -> Result<u32, String> {
    if super::detect::sys::is_elevated() {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(exe);
        command.raw_arg(args).creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let status = command
            .status()
            .map_err(|e| tf("Couldn't start {path}: {error}", &[("path", &exe.display()), ("error", &e)]))?;
        return Ok(status.code().unwrap_or(-1) as u32);
    }
    use super::detect::sys::{wide, wide_os};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide("runas");
    let file = wide_os(exe.as_os_str());
    let params = wide(args);
    let directory = dir.map(|d| wide_os(d.as_os_str()));
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.lpDirectory = directory.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let error = GetLastError();
            return if error == ERROR_CANCELLED {
                Ok(ERROR_CANCELLED)
            } else {
                Err(tf(
                    "Couldn't start {path} (error {error}).",
                    &[("path", &exe.display()), ("error", &error)],
                ))
            };
        }
        if info.hProcess.is_null() {
            return Err(tf("Couldn't track {path}.", &[("path", &exe.display())]));
        }
        let waited = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if waited != WAIT_OBJECT_0 || ok == 0 {
            return Err(tf("Lost track of {path}.", &[("path", &exe.display())]));
        }
        Ok(code)
    }
}
#[cfg(not(windows))]
pub(crate) fn run_hidden(_exe: &Path, _args: &str, _dir: Option<&Path>) -> Result<u32, String> {
    Err(t("Installing Windows components needs Windows.").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(dism_outcome(0), Ok(Outcome::Done));
        assert_eq!(dism_outcome(3010), Ok(Outcome::RestartRequired));
        assert!(dism_outcome(1223).unwrap_err().contains("declined"));
        assert!(dism_outcome(0x800F081F).unwrap_err().contains("0x800F081F"));
        assert!(dism_outcome(87).is_err());

        assert_eq!(vc_outcome(0), Ok(Outcome::Done));
        assert_eq!(vc_outcome(1638), Ok(Outcome::Done));
        assert_eq!(vc_outcome(3010), Ok(Outcome::RestartRequired));
        assert_eq!(vc_outcome(1641), Ok(Outcome::RestartRequired));
        assert!(vc_outcome(1618).unwrap_err().contains("in progress"));
        assert!(vc_outcome(1602).unwrap_err().contains("declined"));

        assert_eq!(dxsetup_outcome(0), Ok(Outcome::Done));
        assert!(dxsetup_outcome((-9i32) as u32).unwrap_err().contains("-9"));
        assert_eq!(extract_outcome(0), Ok(()));
        assert!(extract_outcome(5).is_err());
    }

    #[test]
    fn download_hosts() {
        assert!(trusted_host("download.microsoft.com"));
        assert!(trusted_host("download.visualstudio.microsoft.com"));
        assert!(trusted_host("Microsoft.com"));
        assert!(!trusted_host("microsoft.com.evil.example"));
        assert!(!trusted_host("notmicrosoft.com"));
        assert!(!trusted_host("aka.ms"));
        for url in [DIRECTX_URL, VC_URL, "https://download.visualstudio.microsoft.com/file"] {
            assert!(trusted_download_url(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
        for url in ["http://download.microsoft.com/file", "https://microsoft.com.evil.example/file",
            "https://user@download.microsoft.com/file", "https://download.microsoft.com:444/file",
            "https://aka.ms.evil.example/file"] {
            assert!(!trusted_download_url(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn plans_describe_commands() {
        assert!(plan(Component::DirectPlay).command.contains("/featurename:DirectPlay"));
        assert!(plan(Component::DirectPlay).download.is_none());
        assert!(plan(Component::VcRedist).command.contains("/quiet"));
        assert!(plan(Component::DirectX9).command.contains("/silent"));
    }

    #[test]
    fn scratch_is_removed() {
        let scratch = Scratch::new().unwrap();
        let path = scratch.0.clone();
        std::fs::write(path.join("x"), b"1").unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    /// Downloads both real installers to a temp folder and verifies their
    /// signatures. Never runs them. `cargo test prereqs -- --ignored`.
    #[test]
    #[ignore]
    fn live_download_and_verify() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            for (url, name) in [(VC_URL, "vc_redist.x86.exe"), (DIRECTX_URL, "directx_Jun2010_redist.exe")] {
                let scratch = Scratch::new().unwrap();
                let exe = scratch.0.join(name);
                let (mut tx, _rx) = iced::futures::channel::mpsc::channel(1024);
                download(url, &exe, &mut tx).await.unwrap();
                let size = std::fs::metadata(&exe).unwrap().len();
                let lock = verified(&exe);
                println!("{name}: {size} bytes, {:?}", lock.as_ref().map(|_| "signed by Microsoft"));
                assert!(lock.is_ok(), "{lock:?}");
            }
        });
    }
}
