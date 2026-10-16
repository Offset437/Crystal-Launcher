//! The launcher (server) side of the crystal-loader session pipe: a randomly
//! named, user-only, local-only, single-instance
//! byte pipe carrying UTF-8 JSON lines. The loader says hello, the launcher checks the
//! client's PID and image, sends the one session line, then keeps reading status events
//! until the game ends.
//!
//! Synchronous overlapped I/O on a handle we own outright (no IOCP, no async runtime), so
//! the session bytes pass through no buffer this module doesn't zeroize.

use crate::i18n::{t, tf};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[cfg(test)]
use zeroize::Zeroizing;

/// The contract's maximum line length.
pub(crate) const MAX_LINE: usize = 1 << 20;
/// The one protocol major version this launcher speaks.
pub(crate) const PROTOCOL: u32 = 1;

// ---- protocol -------------------------------------------------------------------------------

/// A line from the loader. Unknown types and fields are tolerated.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoaderEvent {
    /// `viewer_check`: the loader asks for the sign-in's W2U at Connect (07c §1).
    /// `park`: it accepts a credential-less `park` line first (pol-spec 07d §1).
    Hello { v: u32, pid: u32, native_auth: bool, native_social: bool, native_batch: bool, viewer_check: bool, startup_sign_out: bool, park: bool },
    /// `parked`: polcore is created and the host waits for `credentials` (07d §1.3).
    Parked { polcore_ms: Option<u64> },
    Started { pid: Option<u32> },
    Presence(Vec<u8>),
    NativeAuthStage(i32),
    Authenticated,
    NativeFriends { blob: Vec<u8>, locations: Option<Vec<u8>> },
    NativeFriendsRefresh(i32),
    NativeStatus(i32),
    NativeSessionState(bool),
    NativeOwnMode(i32),
    NativeGameEnded { code: i32, game_returned: bool },
    NativeSignOutComplete(i32),
    NativeControlResult { action: String, code: i32 },
    ControllerResult { id: u64, ok: bool, message: String },
    NativeSocialReady,
    /// `native_viewer_check`: the loader reached Open(0) and waits for this sign-in's
    /// W2U (:54000) verdict before Open(1) (pol-spec 07c §1).
    NativeViewerCheck,
    /// `native_main_menu`: the Viewer presence publish completed (07c §2); W20 may follow.
    NativeMainMenu { code: i32, returned: bool },
    NativeMailChanged,
    NativeSocialResult { id: String, action: String, code: i32, payload: Option<Vec<u8>> },
    Exit(i32),
    Error(String),
    /// A type this launcher doesn't know; ignored.
    Unknown,
    /// The pipe closed.
    Closed,
}

#[derive(serde::Deserialize)]
struct Incoming {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    v: Option<u64>,
    #[serde(default)]
    pid: Option<u64>,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    locations: Option<String>,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    game_returned: bool,
    #[serde(default)]
    returned: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    native_auth: bool,
    #[serde(default)]
    native_social: bool,
    #[serde(default)]
    native_batch: bool,
    #[serde(default)]
    viewer_check: bool,
    #[serde(default)]
    startup_sign_out: bool,
    #[serde(default)]
    park: bool,
    #[serde(default)]
    polcore_ms: Option<u64>,
    #[serde(default)]
    game: Option<bool>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    mode: Option<i64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    request_id: Option<u64>,
    #[serde(default)]
    ok: Option<bool>,
}

/// Parse one loader line (without its `\n`).
pub(crate) fn parse_event(line: &[u8]) -> Result<LoaderEvent, String> {
    if line.len() > MAX_LINE {
        return Err("crystal-loader sent a message that exceeds the session protocol limit.".into());
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let value: Incoming = serde_json::from_slice(line)
        .map_err(|_| "crystal-loader sent a line that is not a valid message.".to_owned())?;
    let pid = |p: Option<u64>| p.and_then(|p| u32::try_from(p).ok());
    Ok(match value.kind.as_str() {
        "hello" => LoaderEvent::Hello {
            v: value.v.and_then(|v| u32::try_from(v).ok()).ok_or("crystal-loader's hello has no protocol version.")?,
            pid: pid(value.pid).ok_or("crystal-loader's hello has no process ID.")?,
            native_auth: value.native_auth,
            native_social: value.native_social,
            native_batch: value.native_batch,
            viewer_check: value.viewer_check,
            startup_sign_out: value.startup_sign_out,
            park: value.park,
        },
        "parked" => LoaderEvent::Parked { polcore_ms: value.polcore_ms },
        "started" => LoaderEvent::Started { pid: pid(value.pid) },
        "presence" => LoaderEvent::Presence(
            base64_decode(value.payload.as_deref().unwrap_or(""))
                .ok_or("crystal-loader sent a presence update that is not base64.")?,
        ),
        "native_auth_stage" => LoaderEvent::NativeAuthStage(value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL authentication state.")?),
        "authenticated" => LoaderEvent::Authenticated,
        "native_friends" => {
            let blob=base64_decode(value.payload.as_deref().unwrap_or(""))
                .filter(|b| b.len()==crystal_pol::friends::BLOB_LEN && &b[..8]==crystal_pol::friends::MAGIC)
                .ok_or("Invalid native POL friend snapshot.")?;
            let locations = value.locations.as_deref().map(|encoded| {
                base64_decode(encoded).filter(|b| crate::friends::valid_locations(b))
                    .ok_or("Invalid game friend locations.")
            }).transpose()?;
            LoaderEvent::NativeFriends { blob, locations }
        },
        "native_pol_status" => {
            let code=value.code.and_then(|c| i32::try_from(c).ok()).filter(|c| matches!(c,0|-1025|-1038))
                .ok_or("Invalid native POL connection status.")?;
            LoaderEvent::NativeStatus(code)
        },
        "native_friends_refresh_result" => LoaderEvent::NativeFriendsRefresh(
            value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL friend refresh result.")?
        ),
        "native_session_state" => LoaderEvent::NativeSessionState(value.game.ok_or("Invalid native POL session state.")?),
        "native_own_presence" => LoaderEvent::NativeOwnMode(value.mode.and_then(|m| i32::try_from(m).ok()).filter(|m| (-1..=4).contains(m)).ok_or("Invalid native POL presence mode.")?),
        "native_game_ended" => LoaderEvent::NativeGameEnded {
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid game exit result.")?,
            game_returned: value.game_returned,
        },
        "native_sign_out_complete" => LoaderEvent::NativeSignOutComplete(
            value.code.and_then(|c| i32::try_from(c).ok()).filter(|c| *c <= 0)
                .ok_or("Invalid native POL sign-out result.")?
        ),
        "native_control_result" => LoaderEvent::NativeControlResult {
            action: value.action.filter(|a| matches!(a.as_str(), "start_game" | "sign_out" | "visible" | "away" | "invisible" | "groups")).ok_or("Invalid native POL control action.")?,
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL control result.")?,
        },
        "controller_result" => LoaderEvent::ControllerResult {
            id: value.request_id.ok_or("Invalid controller request identity.")?,
            ok: value.ok.ok_or("Invalid controller result.")?,
            message: value.message.filter(|text| text.len() <= 1024 && !text.contains('\0'))
                .ok_or("Invalid controller result message.")?,
        },
        "native_social_ready" => LoaderEvent::NativeSocialReady,
        "native_viewer_check" => LoaderEvent::NativeViewerCheck,
        "native_main_menu" => LoaderEvent::NativeMainMenu {
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL main-menu result.")?,
            returned: value.returned,
        },
        "native_mail_changed" => LoaderEvent::NativeMailChanged,
        "native_social_result" => {
            let id = value.id.filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).ok_or("Invalid native social request identity.")?;
            let action = value.action.filter(|s| matches!(s.as_str(), "inbox" | "check" | "read" | "send" | "remove")).ok_or("Invalid native social operation.")?;
            let payload = value.payload.as_deref().map(|s| base64_decode(s).filter(|b| b.len() <= 8192).ok_or("Invalid native social result.")).transpose()?;
            LoaderEvent::NativeSocialResult { id, action, code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native social result code.")?, payload }
        },
        "exit" => LoaderEvent::Exit(value.code.map_or(0, |c| c.clamp(i32::MIN as i64, i32::MAX as i64) as i32)),
        "error" => LoaderEvent::Error(clean_message(value.message.as_deref().unwrap_or("unspecified error"))),
        _ => LoaderEvent::Unknown,
    })
}

/// One line of loader text, safe for the status bar and history file.
fn clean_message(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(240)
        .collect();
    flat.trim().to_owned()
}

/// The values the session line carries. Secret fields are zeroized on drop. The real line
/// comes from crystal-pol's `LoaderSession::to_contract_json`; this independent
/// encoder backs the fake sign-in in tests and checks the two agree on the contract.
#[cfg(test)]
pub(crate) struct SessionSecrets {
    pub v_key: Zeroizing<[u8; 16]>,
    pub auth_code: Zeroizing<[u8; 52]>,
    pub pol_records: Option<Zeroizing<Vec<u8>>>,
    pub content_ids: Vec<u32>,
    pub account_id: Zeroizing<u64>,
}

/// The launcher's one `session` line, `\n`-terminated. Built in a buffer sized up front so
/// no reallocation leaves a secret copy behind; the result zeroizes itself on drop.
#[cfg(test)]
pub(crate) fn session_line(region: &str, install_dir: &Path, secrets: &SessionSecrets) -> Result<Zeroizing<Vec<u8>>, String> {
    if !matches!(region, "US" | "EU" | "JP") {
        return Err("Unsupported region.".into());
    }
    let install = install_dir
        .to_str()
        .ok_or(t("The game folder's path is not valid Unicode."))?;
    // Non-secret parts go through serde_json for escaping.
    let head = format!(
        r#"{{"type":"session","v":{PROTOCOL},"region":"{region}","lang":"{region}","install_dir":{},"v_key":""#,
        serde_json::to_string(install).map_err(|_| "Could not encode the game folder.")?
    );
    let ids = secrets
        .content_ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let tail = format!(r#","content_ids":[{ids}],"account_id":"{:016x}"}}"#, *secrets.account_id);
    let records = secrets.pol_records.as_ref().map(|r| r.as_slice());
    let capacity = head.len()
        + base64_len(16)
        + r#"","auth_code":""#.len()
        + base64_len(52)
        + r#"","pol_records":"#.len()
        + records.map_or(4, |r| base64_len(r.len()) + 2)
        + tail.len()
        + 1;
    let mut line = Zeroizing::new(Vec::with_capacity(capacity));
    line.extend_from_slice(head.as_bytes());
    base64_into(&mut line, &secrets.v_key[..]);
    line.extend_from_slice(br#"","auth_code":""#);
    base64_into(&mut line, &secrets.auth_code[..]);
    line.extend_from_slice(br#"","pol_records":"#);
    match records {
        Some(r) => {
            line.push(b'"');
            base64_into(&mut line, r);
            line.push(b'"');
        }
        None => line.extend_from_slice(b"null"),
    }
    line.extend_from_slice(tail.as_bytes());
    line.push(b'\n');
    debug_assert_eq!(line.len(), capacity);
    if line.len() > MAX_LINE {
        return Err(t("The session is larger than the loader accepts.").into());
    }
    Ok(line)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg(test)]
fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Standard base64 with padding, appended in place (the caller reserves the space).
pub(crate) fn base64_into(out: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63]);
        out.push(B64[(n >> 12) as usize & 63]);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] } else { b'=' });
    }
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let value = |c: u8| B64.iter().position(|&b| b == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = n << 6 | value(c)?;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---- pipe -------------------------------------------------------------------------------

/// Why waiting for the loader to connect stopped.
#[derive(Debug)]
pub(crate) enum AcceptError {
    TimedOut,
    /// The caller's check (the injector process) reported a failure.
    Check(String),
    Io(io::Error),
}

/// The server end of one session pipe. Dropping it closes the pipe.
pub(crate) struct LoaderPipe {
    name: String,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(windows)]
    event: std::os::windows::io::OwnedHandle,
    buffer: Vec<u8>,
    native_auth: bool,
    native_social: bool,
    native_batch: bool,
    viewer_check: bool,
    startup_sign_out: bool,
    park: bool,
    command_wake: Option<crate::session_worker::CommandWake>,
    command_wake_pending: bool,
}

impl LoaderPipe {
    pub(crate) fn listen_for_commands(&mut self, wake: Option<crate::session_worker::CommandWake>) {
        self.command_wake = wake;
    }
    pub(crate) fn supports_native_auth(&self)->bool { self.native_auth }
    pub(crate) fn supports_native_social(&self)->bool { self.native_social }
    pub(crate) fn supports_native_batch(&self)->bool { self.native_batch }
    pub(crate) fn supports_startup_sign_out(&self)->bool { self.startup_sign_out }
    /// The launcher sends no W2U of its own; a loader without this never
    /// sends one before :51240 either.
    pub(crate) fn supports_viewer_check(&self)->bool { self.viewer_check }
    /// The loader accepts a credential-less `park` line (pol-spec 07d §1); a loader without
    /// it is never parked and only gets today's `session` line.
    pub(crate) fn supports_park(&self)->bool { self.park }
    /// The full `\\.\pipe\...` path the loader opens.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Next line, or `None` once the pipe closes. `deadline` bounds the whole wait.
    pub(crate) fn read_line(&mut self, deadline: Option<Instant>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                if end > MAX_LINE {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if self.buffer.len() > MAX_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            // A wake may race a successful partial read. Keep the bytes, but let the
            // supervisor consume commands before issuing another blocking read.
            if std::mem::take(&mut self.command_wake_pending) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut chunk = [0u8; 4096];
            match self.read_some(&mut chunk, deadline)? {
                0 => return Ok(None),
                n => self.buffer.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// The next loader event; `Closed` when the pipe ends. A malformed line is an error.
    pub(crate) fn next_event(&mut self, deadline: Option<Instant>) -> io::Result<Result<LoaderEvent, String>> {
        Ok(match self.read_line(deadline)? {
            None => Ok(LoaderEvent::Closed),
            Some(line) => parse_event(&line),
        })
    }

    /// Read the hello, then check that the connected process is the one it claims to be
    /// and runs `expected_image`. Returns the loader's PID.
    pub(crate) fn handshake(&mut self, expected_image: &Path, timeout: Duration) -> Result<u32, String> {
        let client = self
            .client_pid()
            .map_err(|_| t("Could not identify the process on the session pipe; the session was not sent.").to_owned())?;
        let line = match self.read_line(Some(Instant::now() + timeout)) {
            Ok(Some(line)) => line,
            Ok(None) => return Err(t("crystal-loader closed the session pipe before saying hello.").into()),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(tf("crystal-loader connected but did not say hello within {seconds} s.", &[("seconds", &timeout.as_secs())]))
            }
            Err(_) => return Err(t("The session pipe failed before crystal-loader said hello.").into()),
        };
        match parse_event(&line)? {
            LoaderEvent::Hello { v, .. } if v != PROTOCOL => Err(tf(
                "crystal-loader speaks session protocol v{loader}; this launcher speaks v{launcher}. Update the launcher and the loader together.",
                &[("loader", &v), ("launcher", &PROTOCOL)],
            )),
            LoaderEvent::Hello { pid, .. } if pid != client => Err(tf(
                "The process on the session pipe (PID {client}) is not the one that said hello (PID {pid}); the session was not sent.",
                &[("client", &client), ("pid", &pid)],
            )),
            LoaderEvent::Hello { pid, native_auth, native_social, native_batch, viewer_check, startup_sign_out, park, .. } => {
                verify_image(pid, expected_image)?;
                self.native_auth=native_auth;
                self.native_social=native_social;
                self.native_batch=native_batch;
                self.startup_sign_out=startup_sign_out;
                self.viewer_check=viewer_check;
                self.park=park;
                Ok(pid)
            }
            _ => Err(t("crystal-loader did not start with a hello; the session was not sent.").into()),
        }
    }
}

/// The process's image must be the loader we pointed the injector at.
fn verify_image(pid: u32, expected: &Path) -> Result<(), String> {
    let actual = process_image(pid)
        .map_err(|_| tf("Could not read the image path of PID {pid}; the session was not sent.", &[("pid", &pid)]))?;
    if same_file(&actual, expected) {
        Ok(())
    } else {
        Err(tf(
            "The process on the session pipe runs {actual}, not {expected}; the session was not sent.",
            &[("actual", &actual.display()), ("expected", &expected.display())],
        ))
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        crate::backend::plain_windows_path(&p).to_string_lossy().to_lowercase()
    };
    norm(a) == norm(b)
}

/// 32 random hex digits from the system RNG.
pub(crate) fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
        let status = unsafe {
            BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status < 0 {
            return Err(io::Error::other("system RNG failed"));
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
            WAIT_OBJECT_0, WAIT_FAILED,
        },
        Security::{
            AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
            SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, SECURITY_ATTRIBUTES,
            SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
                WaitForMultipleObjects, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
            },
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        },
    };

    const GENERIC_ALL: u32 = 0x1000_0000;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    /// How often a pending operation wakes to check its deadline and the caller's check.
    const SLICE_MS: u32 = 100;

    fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
        }
    }

    /// The current user's SID, as a TOKEN_USER buffer (u64-aligned).
    fn token_user() -> io::Result<Vec<u64>> {
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = owned(token)?;
            let mut needed = 0u32;
            GetTokenInformation(token.as_raw_handle() as _, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            if GetTokenInformation(token.as_raw_handle() as _, TokenUser, buffer.as_mut_ptr() as _, (buffer.len() * 8) as u32, &mut needed) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(buffer)
        }
    }

    impl LoaderPipe {
        /// `\\.\pipe\crystal-loader-<32 hex>`: duplex byte pipe, first and only instance,
        /// remote clients rejected, DACL granting only the current user.
        pub(crate) fn create() -> io::Result<Self> {
            let name = format!(r"\\.\pipe\crystal-loader-{}", random_hex()?);
            let user = token_user()?;
            unsafe {
                let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
                let acl_len = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                    - std::mem::size_of::<u32>()
                    + GetLengthSid(sid) as usize;
                let mut acl = vec![0u64; acl_len.div_ceil(8)];
                let acl_ptr = acl.as_mut_ptr() as *mut ACL;
                if InitializeAcl(acl_ptr, (acl.len() * 8) as u32, ACL_REVISION) == 0
                    || AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
                let descriptor_ptr = &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut std::ffi::c_void;
                if InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) == 0
                    || SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor_ptr,
                    bInheritHandle: 0,
                };
                let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                let handle = owned(CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    64 * 1024,
                    64 * 1024,
                    0,
                    &attributes,
                ))?;
                let event = owned(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()))?;
                Ok(Self { name, handle, event, buffer: Vec::new(), native_auth:false, native_social:false, native_batch:false, viewer_check:false, startup_sign_out:false, park:false,
                    command_wake:None, command_wake_pending:false })
            }
        }

        fn raw(&self) -> HANDLE {
            self.handle.as_raw_handle() as HANDLE
        }

        /// Wait for an overlapped operation; `Ok(bytes)` on completion. Always leaves the
        /// operation finished (cancelled and drained if it did not complete), so the
        /// OVERLAPPED and buffer may be freed afterwards.
        fn finish(
            &mut self,
            overlapped: &mut OVERLAPPED,
            deadline: Option<Instant>,
            check: &mut dyn FnMut() -> Result<(), String>,
            idle_forever: bool,
        ) -> Result<u32, AcceptError> {
            loop {
                let slice = match deadline {
                    None if idle_forever => INFINITE,
                    None => SLICE_MS,
                    Some(end) => end
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(SLICE_MS as u128) as u32,
                };
                let wake = idle_forever.then_some(self.command_wake.as_ref()).flatten();
                let waited = if let Some(wake) = wake {
                    let handles = [self.event.as_raw_handle() as HANDLE, wake.raw()];
                    unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, slice) }
                } else {
                    unsafe { WaitForSingleObject(self.event.as_raw_handle() as HANDLE, slice) }
                };
                if waited == WAIT_OBJECT_0 {
                    let mut bytes = 0u32;
                    return if unsafe { GetOverlappedResult(self.raw(), overlapped, &mut bytes, 0) } == 0 {
                        Err(AcceptError::Io(io::Error::last_os_error()))
                    } else {
                        Ok(bytes)
                    };
                }
                let notified = wake.is_some() && waited == WAIT_OBJECT_0 + 1;
                if notified { self.command_wake_pending = true; }
                let stop = match check() {
                    _ if notified => Some(AcceptError::Check("Native command ready.".into())),
                    _ if waited == WAIT_FAILED => Some(AcceptError::Io(io::Error::last_os_error())),
                    Err(message) => Some(AcceptError::Check(message)),
                    Ok(()) if deadline.is_some_and(|end| Instant::now() >= end) => Some(AcceptError::TimedOut),
                    Ok(()) => None,
                };
                if let Some(stop) = stop {
                    let mut bytes = 0u32;
                    unsafe {
                        CancelIoEx(self.raw(), overlapped);
                        // Completed after all (raced the cancel): take the result.
                        if GetOverlappedResult(self.raw(), overlapped, &mut bytes, 1) != 0 {
                            return Ok(bytes);
                        }
                    }
                    return Err(stop);
                }
            }
        }

        /// Wait for the loader to connect. `check` runs about every 100 ms and can abort.
        pub(crate) fn accept(
            &mut self,
            timeout: Duration,
            mut check: impl FnMut() -> Result<(), String>,
        ) -> Result<(), AcceptError> {
            let deadline = Instant::now() + timeout;
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
            if unsafe { ConnectNamedPipe(self.raw(), &mut overlapped) } != 0 {
                return Ok(());
            }
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => Ok(()),
                ERROR_IO_PENDING => self.finish(&mut overlapped, Some(deadline), &mut check, false).map(|_| ()),
                error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
            }
        }

        pub(crate) fn client_pid(&self) -> io::Result<u32> {
            let mut pid = 0u32;
            if unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pid)
        }

        /// Read what is available (at least one byte), 0 at end of pipe.
        pub(super) fn read_some(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
            let mut read = 0u32;
            let issued = unsafe {
                ReadFile(self.raw(), buffer.as_mut_ptr(), buffer.len() as u32, &mut read, &mut overlapped)
            };
            let result = if issued != 0 {
                Ok(read)
            } else {
                match unsafe { GetLastError() } {
                    ERROR_IO_PENDING => self.finish(&mut overlapped, deadline, &mut || Ok(()), true),
                    error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
                }
            };
            match result {
                Ok(n) => Ok(n as usize),
                Err(AcceptError::Io(e)) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
                Err(AcceptError::Io(e)) => Err(e),
                Err(AcceptError::TimedOut) => Err(io::ErrorKind::TimedOut.into()),
                Err(AcceptError::Check(_)) => {
                    self.command_wake_pending = false;
                    Err(io::ErrorKind::Interrupted.into())
                }
            }
        }

        /// Write all of `bytes` before `deadline`.
        pub(crate) fn send(&mut self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
            while !bytes.is_empty() {
                let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
                overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
                let mut written = 0u32;
                let issued = unsafe {
                    WriteFile(self.raw(), bytes.as_ptr(), bytes.len().min(u32::MAX as usize) as u32, &mut written, &mut overlapped)
                };
                let result = if issued != 0 {
                    Ok(written)
                } else {
                    match unsafe { GetLastError() } {
                        ERROR_IO_PENDING => self.finish(&mut overlapped, Some(deadline), &mut || Ok(()), false),
                        error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
                    }
                };
                match result {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => bytes = &bytes[n as usize..],
                    Err(AcceptError::Io(e)) => return Err(e),
                    Err(_) => return Err(io::ErrorKind::TimedOut.into()),
                }
            }
            Ok(())
        }
    }

    /// The full Win32 path of a process's image.
    pub(super) fn process_image(pid: u32) -> io::Result<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        unsafe {
            let process = owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))?;
            let mut buffer = vec![0u16; 32 * 1024];
            let mut size = buffer.len() as u32;
            if QueryFullProcessImageNameW(process.as_raw_handle() as HANDLE, 0, buffer.as_mut_ptr(), &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..size as usize])))
        }
    }
}

#[cfg(windows)]
use sys::process_image;
#[cfg(not(windows))]
fn process_image(_: u32) -> io::Result<PathBuf> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(windows))]
impl LoaderPipe {
    pub(crate) fn create() -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(crate) fn accept(&mut self, _: Duration, _: impl FnMut() -> Result<(), String>) -> Result<(), AcceptError> {
        Err(AcceptError::Io(io::ErrorKind::Unsupported.into()))
    }
    pub(crate) fn client_pid(&self) -> io::Result<u32> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn read_some(&mut self, _: &mut [u8], _: Option<Instant>) -> io::Result<usize> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(crate) fn send(&mut self, _: &[u8], _: Instant) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn controller_results_keep_large_ids_and_do_not_use_social_ids() {
        assert!(matches!(parse_event(br#"{"type":"controller_result","request_id":18446744073709551615,"ok":true,"message":""}"#),
            Ok(LoaderEvent::ControllerResult { id: u64::MAX, ok: true, message }) if message.is_empty()));
        assert!(matches!(parse_event(br#"{"type":"controller_result","request_id":0,"ok":false,"message":"Unavailable"}"#),
            Ok(LoaderEvent::ControllerResult { id: 0, ok: false, .. })));
        for invalid in [
            r#"{"type":"controller_result","id":"1","ok":true,"message":""}"#,
            r#"{"type":"controller_result","request_id":1,"message":""}"#,
            r#"{"type":"controller_result","request_id":-1,"ok":true,"message":""}"#,
            r#"{"type":"controller_result","request_id":1,"ok":true,"message":"bad\u0000value"}"#,
        ] { assert!(parse_event(invalid.as_bytes()).is_err()); }
    }

    fn secrets() -> SessionSecrets {
        SessionSecrets {
            v_key: Zeroizing::new([0xA5; 16]),
            auth_code: Zeroizing::new([0x3C; 52]),
            pol_records: None,
            content_ids: vec![1234567, 7654321],
            account_id: Zeroizing::new(0x9182_7364_5546_3728),
        }
    }

    fn open_client(name: &str) -> std::fs::File {
        std::fs::OpenOptions::new().read(true).write(true).open(name).unwrap()
    }

    #[test]
    fn base64_round_trips_every_tail_length() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 5) as u8).collect();
            let mut text = Vec::new();
            base64_into(&mut text, &data);
            assert_eq!(text.len(), base64_len(n));
            assert_eq!(base64_decode(std::str::from_utf8(&text).unwrap()).unwrap(), data);
        }
        let mut text = Vec::new();
        base64_into(&mut text, b"Man");
        assert_eq!(text, b"TWFu");
        assert!(base64_decode("TW=u").is_none());
        assert!(base64_decode("TWF").is_none());
    }

    #[test]
    fn session_line_matches_the_contract() {
        let line = session_line("US", Path::new(r"C:\Games\SquareEnix\FINAL FANTASY XI"), &secrets()).unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["type"], "session");
        assert_eq!(value["v"], 1);
        assert_eq!(value["region"], "US");
        assert_eq!(value["lang"], "US");
        assert_eq!(value["install_dir"], r"C:\Games\SquareEnix\FINAL FANTASY XI");
        assert_eq!(base64_decode(value["v_key"].as_str().unwrap()).unwrap(), [0xA5; 16]);
        assert_eq!(base64_decode(value["auth_code"].as_str().unwrap()).unwrap(), [0x3C; 52]);
        assert!(value["pol_records"].is_null());
        assert_eq!(value["content_ids"], serde_json::json!([1234567, 7654321]));
        let mut with_records = secrets();
        with_records.pol_records = Some(Zeroizing::new(vec![1, 2, 3, 4]));
        with_records.content_ids.clear();
        let line = session_line("US", Path::new("C:\\a \"quoted\" dir"), &with_records).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(base64_decode(value["pol_records"].as_str().unwrap()).unwrap(), [1, 2, 3, 4]);
        assert_eq!(value["install_dir"], "C:\\a \"quoted\" dir");
        assert_eq!(value["content_ids"], serde_json::json!([]));
    }

    #[test]
    fn loader_lines_parse_and_tolerate_unknown_fields() {
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true}"#).unwrap(), LoaderEvent::Hello { v:1,pid:1234,native_auth:true,native_social:true,native_batch:false,viewer_check:false, startup_sign_out:false, park:false });
        // A pre-round-1 loader (16e26f3): no viewer_check, so it never asks for the W2U.
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true}"#).unwrap(), LoaderEvent::Hello { v:1,pid:1234,native_auth:true,native_social:true,native_batch:true,viewer_check:false, startup_sign_out:false, park:false });
        // crystal-loader proto::hello_line.
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true,"viewer_check":true}"#).unwrap(), LoaderEvent::Hello { v:1,pid:1234,native_auth:true,native_social:true,native_batch:true,viewer_check:true, startup_sign_out:false, park:false });
        assert_eq!(parse_event(br#"{"type":"native_session_state","game":false}"#).unwrap(), LoaderEvent::NativeSessionState(false));
        assert!(parse_event(br#"{"type":"native_session_state","game":1}"#).is_err());
        assert!(parse_event(br#"{"type":"native_session_state"}"#).is_err());
        assert!(parse_event(br#"{"type":"native_control_result","action":"delete_profile","code":0}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"native_control_result","action":"sign_out","code":-256}"#).unwrap(), LoaderEvent::NativeControlResult { action: "sign_out".into(), code: -256 });
        assert_eq!(parse_event(br#"{"type":"native_control_result","action":"groups","code":1}"#).unwrap(), LoaderEvent::NativeControlResult { action: "groups".into(), code: 1 });
        assert_eq!(parse_event(br#"{"type":"native_sign_out_complete","code":0}"#).unwrap(), LoaderEvent::NativeSignOutComplete(0));
        assert_eq!(parse_event(br#"{"type":"native_sign_out_complete","code":-2046}"#).unwrap(), LoaderEvent::NativeSignOutComplete(-2046));
        assert!(parse_event(br#"{"type":"native_sign_out_complete"}"#).is_err());
        assert!(parse_event(br#"{"type":"native_sign_out_complete","code":7}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"extra":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, native_auth:false, native_social:false, native_batch:false, viewer_check:false, startup_sign_out:false, park:false });
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true}"#).unwrap(), LoaderEvent::Hello { v:1,pid:1234,native_auth:true,native_social:false,native_batch:false,viewer_check:false, startup_sign_out:false, park:false });
        assert_eq!(parse_event(br#"{"type":"authenticated"}"#).unwrap(),LoaderEvent::Authenticated);
        assert_eq!(parse_event(br#"{"type":"native_pol_status","code":-1025}"#).unwrap(),LoaderEvent::NativeStatus(-1025));
        assert!(parse_event(br#"{"type":"native_pol_status","code":1}"#).is_err());
        for code in [0, 1, -10240, i32::MIN, i32::MAX] {
            let line = format!("{{\"type\":\"native_friends_refresh_result\",\"code\":{code}}}");
            assert_eq!(parse_event(line.as_bytes()).unwrap(), LoaderEvent::NativeFriendsRefresh(code));
        }
        assert!(parse_event(br#"{"type":"native_friends_refresh_result","code":2147483648}"#).is_err());
        assert!(parse_event(br#"{"type":"native_friends_refresh_result"}"#).is_err());
        assert!(parse_event(br#"{"type":"native_friends","payload":"AQID"}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"started","pid":1234}"#).unwrap(), LoaderEvent::Started { pid: Some(1234) });
        assert_eq!(parse_event(br#"{"type":"presence","payload":"AQID"}"#).unwrap(), LoaderEvent::Presence(vec![1, 2, 3]));
        assert_eq!(parse_event(br#"{"type":"exit","code":-14}"#).unwrap(), LoaderEvent::Exit(-14));
        assert_eq!(parse_event(br#"{"type":"native_game_ended","code":-14}"#).unwrap(),
            LoaderEvent::NativeGameEnded { code: -14, game_returned: false });
        assert_eq!(parse_event(br#"{"type":"native_game_ended","code":-14,"game_returned":true}"#).unwrap(),
            LoaderEvent::NativeGameEnded { code: -14, game_returned: true });
        assert!(parse_event(br#"{"type":"native_game_ended","code":-14,"game_returned":"true"}"#).is_err());
        assert_eq!(
            parse_event(b"{\"type\":\"error\",\"message\":\"FFXiMain.dll\\nmissing\"}\r").unwrap(),
            LoaderEvent::Error("FFXiMain.dll missing".into())
        );
        assert_eq!(parse_event(br#"{"type":"future-thing"}"#).unwrap(), LoaderEvent::Unknown);
        assert!(parse_event(b"not json").is_err());
        assert!(parse_event(br#"{"type":"hello","pid":1}"#).is_err());
    }

    #[test]
    fn native_friend_locations_are_optional_and_bounded() {
        use crystal_pol::friends::{BLOB_LEN,MAGIC};
        let mut blob=vec![0;BLOB_LEN]; blob[..8].copy_from_slice(MAGIC);
        let encoded = |b: &[u8]| { let mut out=Vec::new(); base64_into(&mut out,b); String::from_utf8(out).unwrap() };
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob)});
        assert_eq!(parse_event(line.to_string().as_bytes()).unwrap(),LoaderEvent::NativeFriends { blob:blob.clone(),locations:None });
        let mut locations=vec![0;8+300*96]; locations[..8].copy_from_slice(b"XIPOLG\x01\0");
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob),"locations":encoded(&locations)});
        assert_eq!(parse_event(line.to_string().as_bytes()).unwrap(),LoaderEvent::NativeFriends { blob:blob.clone(),locations:Some(locations.clone()) });
        locations.pop();
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob),"locations":encoded(&locations)});
        assert!(parse_event(line.to_string().as_bytes()).is_err());
    }

    /// The loader's lines from pol-spec 07c (round-1 contract), as crystal-loader proto.rs writes them.
    #[test]
    fn round1_viewer_check_and_main_menu_events_parse() {
        assert_eq!(parse_event(br#"{"type":"native_viewer_check"}"#).unwrap(), LoaderEvent::NativeViewerCheck);
        assert_eq!(parse_event(br#"{"type":"native_main_menu","code":1,"returned":false}"#).unwrap(), LoaderEvent::NativeMainMenu { code: 1, returned: false });
        assert_eq!(parse_event(br#"{"type":"native_main_menu","code":-2046,"returned":true}"#).unwrap(), LoaderEvent::NativeMainMenu { code: -2046, returned: true });
        assert!(parse_event(br#"{"type":"native_main_menu","returned":false}"#).is_err());
    }

    /// pol-spec 07d §1: the hello's `park` and the `parked` line, as crystal-loader proto.rs writes them.
    #[test]
    fn round2_park_hello_and_parked_events_parse() {
        let hello = br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true,"viewer_check":true,"park":true}"#;
        assert_eq!(parse_event(hello).unwrap(), LoaderEvent::Hello { v:1,pid:1234,native_auth:true,native_social:true,native_batch:true,viewer_check:true, startup_sign_out:false, park:true });
        assert_eq!(parse_event(br#"{"type":"parked","pid":1234,"polcore_ms":431}"#).unwrap(), LoaderEvent::Parked { polcore_ms: Some(431) });
        assert_eq!(parse_event(br#"{"type":"parked","pid":1234}"#).unwrap(), LoaderEvent::Parked { polcore_ms: None });
    }

    #[test]
    fn social_results_validate_correlation_actions_and_payload_bounds() {
        assert_eq!(parse_event(br#"{"type":"native_social_ready"}"#).unwrap(), LoaderEvent::NativeSocialReady);
        assert_eq!(parse_event(br#"{"type":"native_mail_changed"}"#).unwrap(), LoaderEvent::NativeMailChanged);
        let result = br#"{"type":"native_social_result","id":"0123456789abcdef0123456789abcdef","action":"send","code":-7189}"#;
        assert!(matches!(parse_event(result), Ok(LoaderEvent::NativeSocialResult { code:-7189, payload:None, .. })));
        let s = std::str::from_utf8(result).unwrap();
        assert!(parse_event(s.replace("send", "unknown").as_bytes()).is_err());
        assert!(parse_event(s.replace("0123456789abcdef0123456789abcdef", "bad-id").as_bytes()).is_err());
        assert!(parse_event(s.replace("-7189", "2147483648").as_bytes()).is_err());
    }

    #[test]
    fn pipe_name_is_random_and_local() {
        let a = LoaderPipe::create().unwrap();
        let b = LoaderPipe::create().unwrap();
        assert_ne!(a.name(), b.name());
        let suffix = a.name().strip_prefix(r"\\.\pipe\crystal-loader-").unwrap();
        assert_eq!(suffix.len(), 32);
        assert!(suffix.bytes().all(|c| c.is_ascii_hexdigit()));
        // Single instance: a second client cannot open the same pipe while one is connected.
        let name = a.name().to_owned();
        let _first = open_client(&name);
        assert!(std::fs::OpenOptions::new().read(true).write(true).open(&name).is_err());
    }

    #[test]
    fn a_terminated_oversized_line_cannot_bypass_the_transport_limit() {
        let mut pipe = LoaderPipe::create().unwrap();
        pipe.buffer = vec![b' '; MAX_LINE + 1];
        pipe.buffer.push(b'\n');
        assert_eq!(pipe.read_line(None).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(parse_event(&pipe.buffer).is_err());
        pipe.buffer = vec![b' '; MAX_LINE];
        pipe.buffer.push(b'\n');
        assert_eq!(pipe.read_line(None).unwrap().unwrap().len(), MAX_LINE);
    }

    #[test]
    fn native_command_wakes_a_silent_pipe_and_retains_timeout_fallback() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (finish, finished) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let _file = open_client(&name);
            finished.recv().unwrap();
        });
        pipe.accept(Duration::from_secs(3), || Ok(())).unwrap();
        let (listener, signal) = crate::session_worker::CommandWake::test_listener();
        pipe.listen_for_commands(Some(listener));
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signal();
        });
        let error = pipe.next_event(Some(Instant::now() + Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        notifier.join().unwrap();
        let error = pipe.next_event(Some(Instant::now() + Duration::from_millis(30))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "missing notifications retain polling fallback");
        finish.send(()).unwrap();
        client.join().unwrap();
    }

    #[test]
    fn native_command_wake_preserves_partial_lines_and_completion_races() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (finish, finished) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            file.write_all(br#"{"type":"ex"#).unwrap();
            finished.recv().unwrap();
            file.write_all(b"it\",\"code\":0}\n").unwrap();
        });
        pipe.accept(Duration::from_secs(3), || Ok(())).unwrap();
        let mut chunk = [0; 4096];
        let count = pipe.read_some(&mut chunk, Some(Instant::now() + Duration::from_secs(2))).unwrap();
        pipe.buffer.extend_from_slice(&chunk[..count]);
        let partial = pipe.buffer.clone();
        let (listener, signal) = crate::session_worker::CommandWake::test_listener();
        pipe.listen_for_commands(Some(listener));
        signal();
        assert_eq!(pipe.read_line(Some(Instant::now() + Duration::from_secs(2))).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(pipe.buffer, partial);
        // CancelIoEx may race a successful partial read; finish retains that notification.
        pipe.command_wake_pending = true;
        assert_eq!(pipe.read_line(Some(Instant::now() + Duration::from_secs(2))).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(pipe.buffer, partial);
        finish.send(()).unwrap();
        assert_eq!(pipe.next_event(Some(Instant::now() + Duration::from_secs(2))).unwrap().unwrap(), LoaderEvent::Exit(0));
        client.join().unwrap();
        assert_eq!(pipe.next_event(None).unwrap().unwrap(), LoaderEvent::Closed);
    }

    /// An in-process fake loader: hello, read the session, then report events.
    #[test]
    fn fake_loader_exchanges_lines_end_to_end() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let file = open_client(&name);
            let mut writer = file.try_clone().unwrap();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
            let mut reader = BufReader::new(file);
            let mut session = String::new();
            reader.read_line(&mut session).unwrap();
            writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
            writeln!(writer, r#"{{"type":"presence","payload":"AAEC"}}"#).unwrap();
            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            session
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let me = std::env::current_exe().unwrap();
        assert_eq!(pipe.handshake(&me, Duration::from_secs(10)).unwrap(), std::process::id());
        let line = session_line("US", Path::new(r"C:\FFXI"), &secrets()).unwrap();
        pipe.send(&line, Instant::now() + Duration::from_secs(10)).unwrap();
        drop(line);
        let deadline = Some(Instant::now() + Duration::from_secs(10));
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Started { pid: Some(std::process::id()) });
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Presence(vec![0, 1, 2]));
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Exit(0));
        let session = client.join().unwrap();
        let value: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(value["type"], "session");
        assert_eq!(base64_decode(value["v_key"].as_str().unwrap()).unwrap(), [0xA5; 16]);
        // The client closed: the pipe reports the end.
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Closed);
    }

    #[test]
    fn hello_with_another_pid_is_rejected_before_the_session() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id() ^ 0x40).unwrap();
            let mut rest = Vec::new();
            let _ = std::io::Read::read_to_end(&mut file, &mut rest);
            rest
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_secs(10)).unwrap_err();
        assert!(error.contains("not the one that said hello"), "{error}");
        drop(pipe);
        assert!(client.join().unwrap().is_empty(), "nothing may be sent to an impostor");
    }

    #[test]
    fn hello_from_another_image_is_rejected() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
            file
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe
            .handshake(Path::new(r"C:\nowhere\crystal-loader.exe"), Duration::from_secs(10))
            .unwrap_err();
        assert!(error.contains("not C:\\nowhere\\crystal-loader.exe"), "{error}");
        drop(client.join().unwrap());
    }

    #[test]
    fn newer_protocol_is_rejected() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":2,"pid":{}}}"#, std::process::id()).unwrap();
            file
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_secs(10)).unwrap_err();
        assert!(error.contains("protocol v2"), "{error}");
        drop(client.join().unwrap());
    }

    #[test]
    fn accept_times_out_and_check_can_abort() {
        let mut pipe = LoaderPipe::create().unwrap();
        let started = Instant::now();
        assert!(matches!(pipe.accept(Duration::from_millis(300), || Ok(())), Err(AcceptError::TimedOut)));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut calls = 0;
        let result = pipe.accept(Duration::from_secs(30), || {
            calls += 1;
            if calls >= 2 { Err("injector exited".into()) } else { Ok(()) }
        });
        assert!(matches!(result, Err(AcceptError::Check(m)) if m == "injector exited"));
        // The pipe still works after a cancelled connect.
        let name = pipe.name().to_owned();
        let _client = open_client(&name);
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
    }

    #[test]
    fn silent_client_times_out_waiting_for_hello() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let _client = open_client(&name);
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("did not say hello"), "{error}");
    }

    #[test]
    fn overlong_line_is_refused() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            let _ = file.write_all(&vec![b'x'; MAX_LINE + 8192]);
        });
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
        let error = pipe.read_line(Some(Instant::now() + Duration::from_secs(10))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(pipe);
        let _ = client.join();
    }
}
