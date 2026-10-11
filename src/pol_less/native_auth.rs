//! Credentials travel once over the existing private, PID-verified pipe. The
//! game host's installed POL core performs and owns the actual authentication.
use super::{pipe, SignedIn};
use crate::i18n::{t, tf};
use crate::{
    backend::{self, Account},
    friends::{self, Snapshot},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc, Mutex,
    },
};
use zeroize::Zeroizing;

struct Input {
    pol_id: Zeroizing<String>,
    pol_password: Zeroizing<String>,
    se_id: Zeroizing<String>,
    se_secret: Zeroizing<[u8; 20]>,
    otp: Zeroizing<String>,
    key: String,
}
pub(super) struct NativeAuth {
    input: Mutex<Option<Input>>,
    directory: PathBuf,
    snapshot: Mutex<Snapshot>,
    cache_dirty: AtomicBool,
    status: AtomicI32,
    social_only: bool,
    /// A Play whose game start waits for the sign-in and the version check (see `SignedIn`).
    start_pending: AtomicBool,
    invisible_login: bool,
    auto_away_minutes: u8,
    auto_login: super::AutoLogin,
    game_running: Arc<AtomicBool>,
}
impl NativeAuth {
    pub fn new(
        account: &Account,
        otp: Zeroizing<String>,
        directory: PathBuf,
    ) -> Result<Self, String> {
        let pol_id = Zeroizing::new(account.pol_id.to_ascii_uppercase());
        if pol_id.len() != 8
            || !pol_id.as_bytes()[..4].iter().all(u8::is_ascii_uppercase)
            || !pol_id.as_bytes()[4..].iter().all(u8::is_ascii_digit)
        {
            return Err(t("Enter a valid eight-character PlayOnline ID.").into());
        }
        if account.pol_password.is_empty()
            || account.pol_password.len() > 15
            || !account
                .pol_password
                .bytes()
                .all(|b| (32..=126).contains(&b))
        {
            return Err(
                t("PlayOnline passwords must contain 1–15 printable ASCII characters.").into(),
            );
        }
        if account.se_id.is_empty()
            || account.se_id.len() > 16
            || !account
                .se_id
                .bytes()
                .all(|b| b.is_ascii() && !b.is_ascii_control())
        {
            return Err(t("Square Enix IDs must contain 1–16 ASCII characters.").into());
        }
        if account.se_password.is_empty()
            || account.se_password.len() > 512
            || account.se_password.contains('\0')
        {
            return Err(t("Enter a valid Square Enix password.").into());
        }
        if !otp.is_empty() && (otp.len() != 6 || !otp.bytes().all(|b| b.is_ascii_digit())) {
            return Err(t("Enter a six-digit one-time code, or leave it empty.").into());
        }
        let utf16 = Zeroizing::new(account.se_password.encode_utf16().collect::<Vec<_>>());
        let se_secret =
            crystal_pol::credential::EditorSecret::from_utf16(&utf16).into_native_secret();
        Ok(Self {
            input: Mutex::new(Some(Input {
                pol_id,
                pol_password: Zeroizing::new(account.pol_password.clone()),
                se_id: Zeroizing::new(account.se_id.clone()),
                se_secret,
                otp,
                key: backend::account_key(account),
            })),
            directory,
            snapshot: Mutex::new(Snapshot::default()),
            cache_dirty: AtomicBool::new(false),
            status: AtomicI32::new(0),
            social_only: false,
            start_pending: AtomicBool::new(false),
            invisible_login: false,
            auto_away_minutes: super::default_auto_away(),
            auto_login: super::AutoLogin::default(),
            game_running: Arc::new(AtomicBool::new(true)),
        })
    }
    pub fn set_social_only(&mut self, social_only: bool) {
        self.social_only = social_only;
        self.game_running.store(!social_only, Ordering::Release);
    }
    /// The session line says `start_game: false` and the worker starts FFXI after its own
    /// version check. `game_running` stays true meanwhile: the start is under way.
    pub fn set_deferred_start(&mut self, deferred: bool) {
        self.start_pending.store(deferred, Ordering::Release);
    }
    pub fn set_invisible_login(&mut self, invisible: bool) { self.invisible_login = invisible; }
    /// 0 = off, else one of the Viewer's 10..60-minute choices.
    pub fn set_auto_away(&mut self, minutes: u8) {
        self.auto_away_minutes = if super::AUTO_AWAY_CHOICES.contains(&minutes) { minutes } else { super::default_auto_away() };
    }
    pub fn set_auto_login(&mut self, choice: super::AutoLogin) {
        self.auto_login = choice.normalized();
    }
    fn publish(&self, snapshot: Snapshot) -> Result<usize, String> {
        let count = snapshot.entries.len();
        // Native events are authoritative even when an observer temporarily prevents cache
        // replacement. Keep one-shot idle/ready/results in memory so a later push can heal it.
        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        *current = snapshot;
        self.cache_dirty.store(true, Ordering::Release);
        crate::session_worker::publish_friends(&self.directory, &current)?;
        self.cache_dirty.store(false, Ordering::Release);
        Ok(count)
    }
}
impl SignedIn for NativeAuth {
    fn native_core(&self) -> bool {
        true
    }
    fn social_only(&self) -> bool { self.social_only }
    fn start_pending(&self) -> bool { self.start_pending.load(Ordering::Acquire) }
    fn idle_first(&self) -> bool { self.social_only || self.start_pending() }
    fn deferred_start_done(&self, blocked: Option<&str>) {
        self.start_pending.store(false, Ordering::Release);
        if let Some(message) = blocked {
            // Same as the idle report the pending start had held back, plus the reason.
            self.game_running.store(false, Ordering::Release);
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            snapshot.game_running = Some(false);
            snapshot.session_error = Some(message.to_owned());
            snapshot.social.ready = false;
            let _ = self.publish(snapshot);
        }
    }
    fn game_running(&self) -> Option<Arc<AtomicBool>> { Some(self.game_running.clone()) }
    fn native_session_state(&self, game: bool) -> Result<(), String> {
        // Until a Play's start is decided, the idle report must not look like a closed game.
        if !game && self.start_pending() { return Ok(()); }
        self.game_running.store(game, Ordering::Release);
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.game_running = Some(game);
        snapshot.session_error = None;
        snapshot.social.ready = false;
        self.publish(snapshot).map(|_| ())
    }
    fn native_own_mode(&self, mode: i32) -> Result<(), String> {
        if self.status.load(Ordering::Acquire) < 0 { return Ok(()); }
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.own_mode = Some(mode);
        self.publish(snapshot).map(|_| ())
    }
    fn native_control_result(&self, action: &str, code: i32) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.session_error = (code < 0).then(|| {
            // The action is a protocol name; each one gets its own sentence.
            let args: [(&str, &dyn std::fmt::Display); 1] = [("code", &code)];
            match action {
                "start_game" => tf("PlayOnline could not start the game ({code}).", &args),
                "sign_out" => tf("PlayOnline could not sign out ({code}).", &args),
                "visible" | "away" | "invisible" => tf("PlayOnline could not change your online status ({code}).", &args),
                _ => tf("PlayOnline could not accept {action} ({code}).", &[("action", &action), ("code", &code)]),
            }
        });
        self.publish(snapshot).map(|_| ())
    }
    fn take_native_control(&self) -> Option<(crate::session_worker::NativeControl, bool)> {
        crate::session_worker::take_native_control(&self.directory)
    }
    fn take_native_batch(&self) -> Option<Vec<u8>> {
        crate::session_worker::take_native_batch(&self.directory)
    }
    fn native_command_listener(&self) -> Option<crate::session_worker::CommandWake> {
        crate::session_worker::listen_for_commands()
    }
    fn take_native_activity(&self) -> bool {
        // Consume the note either way; only an idle Viewer session uses it.
        let noted = crate::session_worker::take_native_activity(&self.directory);
        noted && self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).game_running == Some(false)
    }
    fn take_native_social(&self) -> Option<Zeroizing<Vec<u8>>> {
        {
            let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            if snapshot.social.busy || !snapshot.social.ready || snapshot.game_running != Some(false) { return None; }
        }
        let (request, _lock) = crate::session_worker::take_social(&self.directory)?;
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let previous_social = snapshot.social.clone();
        snapshot.social.begin(request.id.clone(), &request.request);
        let line = request.line();
        if let Err(error) = &line {
            snapshot.social.busy = false;
            snapshot.social.error = Some(error.clone());
        }
        if let Err(error) = self.publish(snapshot) {
            // This is an outgoing intent, not a native completion: nothing was delivered.
            // Undo busy/pending state while retaining the actual session's readiness.
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            snapshot.social = previous_social;
            snapshot.social.error = Some(error);
            let _ = self.publish(snapshot);
            return None;
        }
        line.ok()
    }
    fn native_social_ready(&self) -> Result<(), String> {
        if self.status.load(Ordering::Acquire) < 0 { return Ok(()); }
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.social = crate::social::State { ready: true, ..Default::default() };
        self.publish(snapshot).map(|_| ())
    }
    fn retry_native_publication(&self) -> Result<bool, String> {
        if !self.cache_dirty.load(Ordering::Acquire) { return Ok(false); }
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // A newer native event may already have published while this caller waited.
        if !self.cache_dirty.load(Ordering::Acquire) { return Ok(false); }
        crate::session_worker::publish_friends(&self.directory, &snapshot)?;
        self.cache_dirty.store(false, Ordering::Release);
        Ok(true)
    }
    fn native_social_result(&self, id: &str, action: &str, code: i32, payload: Option<&[u8]>) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Err(error) = snapshot.social.apply(id, action, code, payload) {
            snapshot.social.busy = false;
            snapshot.social.error = Some(error);
        }
        self.publish(snapshot).map(|_| ())
    }
    fn native_mail_changed(&self) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // A previous idle period must not mark a game-owned or ended session.
        if !snapshot.social.ready || snapshot.game_running != Some(false) { return Ok(()); }
        snapshot.social.inbox_changed = true;
        self.publish(snapshot).map(|_| ())
    }
    fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
        let input = self
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(t("Native POL credentials were already handed to the game host."))?;
        let install = install_dir
            .to_str()
            .ok_or(t("The game folder's path is not valid Unicode."))?;
        // An idle Friends login can still start FFXI. Once that game returns,
        // its owner must run native sign-out and exit rather than retain pol.exe.
        let pol = crate::discovery::find(crate::discovery::Product::PlayOnline, None);
        let auto_login = serde_json::to_string(&self.auto_login.normalized())
            .map_err(|_| t("Could not encode character login settings."))?;
        let pol = serde_json::to_string(&pol.as_ref().map(|p| p.to_string_lossy().into_owned()))
            .map_err(|_| t("Could not encode the PlayOnline folder."))?;
        let head=format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":{},\"keep_session\":false,\"invisible_login\":{},\"auto_away_minutes\":{},\"auto_login\":{},\"pol_install_dir\":{},\"install_dir\":{},\"native_auth\":{{", !self.idle_first(), self.invisible_login, self.auto_away_minutes, auto_login, pol, serde_json::to_string(install).map_err(|_| t("Could not encode the game folder."))?);
        // Preallocate all secret fields, preventing discarded allocation copies.
        let fields: [(&str, &[u8]); 5] = [
            ("pol_id", input.pol_id.as_bytes()),
            ("pol_password", input.pol_password.as_bytes()),
            ("se_id", input.se_id.as_bytes()),
            ("se_secret", &*input.se_secret),
            ("otp", input.otp.as_bytes()),
        ];
        let capacity = head.len()
            + fields
                .iter()
                .map(|(k, v)| k.len() + 6 + v.len().div_ceil(3) * 4)
                .sum::<usize>()
            + 80;
        let mut line = Zeroizing::new(Vec::with_capacity(capacity));
        line.extend_from_slice(head.as_bytes());
        for (index, (name, value)) in fields.iter().enumerate() {
            if index != 0 {
                line.push(b',');
            }
            line.push(b'"');
            line.extend_from_slice(name.as_bytes());
            line.extend_from_slice(b"\":");
            if *name == "otp" && value.is_empty() {
                line.extend_from_slice(b"null");
            } else {
                line.push(b'"');
                pipe::base64_into(&mut line, value);
                line.push(b'"');
            }
        }
        line.extend_from_slice(b",\"profile_key\":\"");
        line.extend_from_slice(input.key.as_bytes());
        line.extend_from_slice(b"\"}}\n");
        debug_assert!(line.len() <= capacity);
        Ok(line)
    }
    fn forward_presence(&self, _: &[u8]) -> Result<bool, String> {
        Err("This session's presence is owned by the native PlayOnline core.".into())
    }
    fn receive_native_friends(&self, blob: &[u8], locations: Option<&[u8]>) -> Result<usize, String> {
        let mut snapshot = friends::native_snapshot(blob)?;
        if let Some(locations) = locations { friends::apply_native_locations(&mut snapshot, locations)?; }
        let previous = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // The core's cache is the only friend source (the Viewer has no
        // manual refresh); a connection error stays until the connection returns.
        snapshot.error.clone_from(&previous.error);
        snapshot.game_running = previous.game_running;
        snapshot.session_error.clone_from(&previous.session_error);
        snapshot.own_mode = previous.own_mode;
        snapshot.social.clone_from(&previous.social);
        drop(previous);
        // The native observer and COM owner publish on different threads. A
        // snapshot already queued at connection loss can still arrive here;
        // only a healthy status event may make its live presence valid again.
        if self.status.load(Ordering::Acquire) < 0 {
            for friend in &mut snapshot.entries {
                friend.online = None;
                friend.location = None;
            }
        }
        self.publish(snapshot)
    }
    fn native_status(&self, code: i32) {
        let previous = self.status.swap(code, Ordering::AcqRel);
        if code < 0 || previous < 0 {
            let mut snapshot = self
                .snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if code < 0 {
                snapshot.own_mode = None;
                for friend in &mut snapshot.entries {
                    friend.online = None;
                    friend.location = None;
                }
                snapshot.social.ready = false;
                snapshot.social.busy = false;
                snapshot.social.id = None;
                snapshot.social.action = None;
                snapshot.social.pending_header = None;
            }
            snapshot.error = (code < 0).then(|| t("The native PlayOnline connection ended.").into());
            let _ = self.publish(snapshot);
        }
    }
    fn session_status(&self) -> i32 {
        self.status.load(Ordering::Acquire)
    }
    fn sign_out(self) -> String {
        self.native_status(-1025);
        "The native PlayOnline game-host session ended.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifetime_survives_friend_updates_and_reports_control_failures() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_own_mode(4).unwrap();
        assert!(!fixture.auth.game_running.load(Ordering::Acquire));
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().game_running, Some(false));
        assert_eq!(fixture.cached().own_mode, Some(4));
        fixture.auth.native_control_result("start_game", -256).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().session_error.as_deref(), Some("PlayOnline could not start the game (-256)."));
        fixture.auth.native_session_state(true).unwrap();
        assert!(fixture.auth.game_running.load(Ordering::Acquire));
        assert!(fixture.cached().session_error.is_none());
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().game_running, Some(true));
        assert_eq!(fixture.cached().own_mode, Some(4));
    }

    #[test]
    fn friend_pushes_preserve_social_results_and_new_viewer_sessions_clear_them() {
        let fixture = Fixture::new(); fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        assert!(fixture.cached().social.ready);
        let id = "0123456789abcdef0123456789abcdef";
        let mut snapshot = fixture.cached(); snapshot.social.begin(id.into(), &crate::social::Request::Inbox { folder:0, page:0 });
        fixture.auth.publish(snapshot).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap(); assert!(fixture.cached().social.busy);
        fixture.auth.native_social_result(id, "inbox", 1, Some(&[0; 12 + 32 * 72])).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap(); assert!(!fixture.cached().social.busy);
        fixture.auth.native_session_state(true).unwrap(); assert!(!fixture.cached().social.ready);
        fixture.auth.native_session_state(false).unwrap(); fixture.auth.native_social_ready().unwrap();
        assert!(fixture.cached().social.id.is_none());
        fixture.auth.native_status(-1025); assert!(!fixture.cached().social.ready);
    }

    #[cfg(windows)]
    #[test]
    fn failed_cache_replacement_cannot_discard_native_idle_or_one_shot_readiness() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(true).unwrap();
        let observer = fixture.deny_cache_replacement();
        assert!(fixture.auth.native_session_state(false).is_err());
        assert!(fixture.auth.native_social_ready().is_err());
        let native = fixture.auth.snapshot.lock().unwrap().clone();
        assert_eq!(native.game_running, Some(false));
        assert!(native.social.ready && !native.social.busy);
        // The observer still sees its old file; it must not become our source of truth.
        assert_eq!(fixture.cached().game_running, Some(true));
        assert!(!fixture.cached().social.ready);
        drop(observer);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        let healed = fixture.cached();
        assert_eq!(healed.game_running, Some(false));
        assert!(healed.social.ready && !healed.social.busy);
        assert_eq!(healed.entries.len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn failed_readiness_publication_recovers_without_another_native_event() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        let observer = fixture.deny_cache_replacement();
        // Keep the real Windows reader open through the entire bounded replacement retry.
        assert!(fixture.auth.native_social_ready().is_err());
        assert!(!fixture.cached().social.ready);
        assert!(fixture.auth.retry_native_publication().is_err());
        drop(observer);
        assert!(fixture.auth.retry_native_publication().unwrap());
        let recovered = fixture.cached();
        assert_eq!(recovered.game_running, Some(false));
        assert!(recovered.social.ready && !recovered.social.busy);
        let _observer = fixture.deny_cache_replacement();
        assert!(!fixture.auth.retry_native_publication().unwrap(), "a successful retry clears pending work and performs no second write");
    }

    #[cfg(windows)]
    #[test]
    fn failed_outgoing_social_publication_preserves_ready_without_sticking_busy() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        let cache: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture.auth.directory.join("friends.json")).unwrap(),
        ).unwrap();
        let request_path = fixture.auth.directory.join("native-social.json");
        let submit = |id: &str| {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "worker": cache["worker"], "id": id, "request": crate::social::Request::Check {},
            })).unwrap();
            std::fs::write(&request_path, bytes).unwrap();
        };
        submit("0123456789abcdef0123456789abcdef");
        let observer = fixture.deny_cache_replacement();
        assert!(fixture.auth.take_native_social().is_none(), "failed publication must not send the request");
        assert!(!request_path.exists());
        let native = fixture.auth.snapshot.lock().unwrap().clone();
        assert!(native.social.ready && !native.social.busy);
        assert!(native.social.id.is_none() && native.social.error.is_some());
        drop(observer);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert!(fixture.cached().social.ready && !fixture.cached().social.busy);
        assert!(fixture.cached().social.error.is_some());
        submit("1123456789abcdef0123456789abcdef");
        let line = fixture.auth.take_native_social().expect("the next request remains usable");
        let sent: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(sent["type"], "native_social");
        assert_eq!(sent["id"], "1123456789abcdef0123456789abcdef");
        assert!(fixture.cached().social.busy);
    }

    #[test]
    fn incoming_mail_marks_only_the_live_idle_session_and_survives_friend_pushes() {
        let fixture = Fixture::new();
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.auth.snapshot.lock().unwrap().social.inbox_changed);
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        fixture.auth.native_mail_changed().unwrap();
        assert!(fixture.cached().social.inbox_changed);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert!(fixture.cached().social.inbox_changed);
        let id = "0123456789abcdef0123456789abcdef";
        let mut snapshot = fixture.cached();
        snapshot.social.begin(id.into(), &crate::social::Request::Inbox { folder:0, page:0 });
        fixture.auth.publish(snapshot).unwrap();
        fixture.auth.native_social_result(id, "inbox", -515, None).unwrap();
        assert!(fixture.cached().social.inbox_changed);
        fixture.auth.native_social_result(id, "inbox", 1, Some(&[0; 12 + 32 * 72])).unwrap();
        assert!(!fixture.cached().social.inbox_changed);
        fixture.auth.native_session_state(true).unwrap();
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.cached().social.inbox_changed);
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        fixture.auth.native_status(-1025);
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.cached().social.inbox_changed);
    }

    struct Fixture { auth: NativeAuth }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "crystal-native-refresh-{}", backend::unique_id().unwrap()
            ));
            Self { auth: NativeAuth {
                input: Mutex::new(None), directory,
                snapshot: Mutex::new(Snapshot::default()),
                cache_dirty: AtomicBool::new(false),
                status: AtomicI32::new(0), social_only: false, start_pending: AtomicBool::new(false), invisible_login: false, auto_away_minutes: 10, auto_login: super::super::AutoLogin::default(), game_running: Arc::new(AtomicBool::new(true)),
            } }
        }
        fn cached(&self) -> Snapshot {
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(self.auth.directory.join("friends.json")).unwrap()
            ).unwrap();
            serde_json::from_value(value["snapshot"].clone()).unwrap()
        }
        #[cfg(windows)]
        fn deny_cache_replacement(&self) -> std::fs::File {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(self.auth.directory.join("friends.json")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // This fixture owns only its unique absolute temporary directory.
            let _ = std::fs::remove_dir_all(&self.auth.directory);
        }
    }
    fn friend_blob() -> Vec<u8> {
        use crystal_pol::friends::{BLOB_LEN, MAGIC};
        let mut blob = vec![0; BLOB_LEN];
        blob[..8].copy_from_slice(MAGIC);
        blob[8 + 0x98] = 1;
        blob[8 + 0xa0..8 + 0xa5].copy_from_slice(b"Alice");
        blob[8 + 0x10..8 + 0x18].copy_from_slice(&123u64.to_le_bytes());
        blob[8 + 8..8 + 16].copy_from_slice(&((1u64 << 11) | (1u64 << 33)).to_le_bytes());
        blob
    }

    #[test]
    fn disconnection_clears_presence_without_losing_the_list() {
        let fixture = Fixture::new();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        fixture.auth.native_status(-1025);
        let disconnected = fixture.cached();
        assert_eq!(disconnected.entries[0].name, "Alice");
        assert_eq!(disconnected.entries[0].online, None);
        assert_eq!(disconnected.entries[0].location, None);
        assert!(disconnected.error.is_some());
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        fixture.auth.native_own_mode(4).unwrap();
        fixture.auth.native_social_ready().unwrap();
        let queued_after_loss = fixture.cached();
        assert_eq!(queued_after_loss.entries[0].online, None);
        assert_eq!(queued_after_loss.entries[0].location, None);
        assert_eq!(queued_after_loss.own_mode, None);
        assert!(!queued_after_loss.social.ready);
        assert!(queued_after_loss.error.is_some());
        fixture.auth.native_status(0);
        assert!(fixture.cached().error.is_none());
        assert_eq!(fixture.cached().entries[0].online, None);
        // Recovery needs a new event even when the native bytes are unchanged.
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().entries[0].online, Some(true));
    }

    #[test]
    fn game_locations_publish_with_presence_and_clear_when_no_longer_available() {
        let fixture = Fixture::new();
        let mut locations = vec![0; 8 + 300 * 96];
        locations[..8].copy_from_slice(b"XIPOLG\x01\0");
        locations[8..13].copy_from_slice(b"Alice");
        locations[24..38].copy_from_slice(b"Bastok Markets");
        fixture.auth.receive_native_friends(&friend_blob(), Some(&locations)).unwrap();
        assert_eq!(fixture.cached().entries[0].location.as_deref(), Some("Alice · Bastok Markets"));
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().entries[0].location.as_deref(), Some("FINAL FANTASY XI"));
        fixture.auth.receive_native_friends(&friend_blob(), Some(&locations)).unwrap();
        fixture.auth.native_status(-1025);
        assert_eq!(fixture.cached().entries[0].location, None);
    }

    #[test]
    fn credentials_are_one_use_and_never_become_session_keys() {
        let account = Account {
            label: "test".into(),
            mode: backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "abcd1234".into(),
            pol_password: "fake pol pass".into(),
            se_id: "fakeuser".into(),
            se_password: "fake SE pass".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
        };
        let mut auth =
            NativeAuth::new(&account, Zeroizing::new("123456".into()), PathBuf::new()).unwrap();
        assert_eq!(auth.auto_away_minutes, 10, "the Viewer's default");
        auth.set_auto_away(25);
        assert_eq!(auth.auto_away_minutes, 10, "only the Viewer's choices");
        auth.set_auto_away(0);
        auth.set_social_only(true);
        auth.set_invisible_login(true);
        auth.set_auto_login(super::super::AutoLogin { enabled: true, character_slot: 3 });
        let line = auth.session_line(Path::new(r"C:\Games\FFXI")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert!(value.get("auth_code").is_none());
        assert!(value.get("v_key").is_none());
        assert_eq!(value["start_game"], false);
        assert_eq!(value["keep_session"], false);
        assert_eq!(value["invisible_login"], true);
        assert_eq!(value["auto_away_minutes"], 0);
        assert_eq!(value["auto_login"]["enabled"], true);
        assert_eq!(value["auto_login"]["character_slot"], 3);
        let credentials = &value["native_auth"];
        assert_eq!(
            pipe::base64_decode(credentials["pol_id"].as_str().unwrap()).unwrap(),
            b"ABCD1234"
        );
        assert_eq!(
            pipe::base64_decode(credentials["se_secret"].as_str().unwrap())
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            pipe::base64_decode(credentials["otp"].as_str().unwrap()).unwrap(),
            b"123456"
        );
        assert!(!String::from_utf8_lossy(&line).contains("fake SE pass"));
        assert!(auth.session_line(Path::new(r"C:\Games\FFXI")).is_err());
    }

    fn play_account() -> Account {
        Account {
            label: "test".into(),
            mode: backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "abcd1234".into(),
            pol_password: "fake pol pass".into(),
            se_id: "fakeuser".into(),
            se_password: "fake SE pass".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
        }
    }

    #[test]
    fn a_play_signs_in_idle_and_holds_the_idle_report_until_its_start_is_decided() {
        let directory = std::env::temp_dir().join(format!("crystal-deferred-{}", backend::unique_id().unwrap()));
        let start_game = |deferred: bool| {
            let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
            auth.set_deferred_start(deferred);
            let line = auth.session_line(Path::new(r"C:\Games\FFXI")).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(value["keep_session"], false, "every game signs out its own POL host when it closes");
            value["start_game"].clone()
        };
        assert_eq!(start_game(false), true, "a plain launch still tells the loader to start the game");
        assert_eq!(start_game(true), false, "a Play signs in idle; the worker starts the game after its check");

        let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
        auth.set_deferred_start(true);
        assert!(auth.idle_first() && auth.start_pending() && !auth.social_only());
        // The loader's idle report does not read as a closed game while the start is pending.
        auth.native_session_state(false).unwrap();
        assert!(auth.game_running.load(Ordering::Acquire));
        // The check refused the start: the session is idle, with the reason as its error.
        auth.deferred_start_done(Some("Update required: installed 1, official server 2."));
        assert!(!auth.game_running.load(Ordering::Acquire) && !auth.start_pending());
        let snapshot = auth.snapshot.lock().unwrap().clone();
        assert_eq!(snapshot.game_running, Some(false));
        assert!(snapshot.session_error.as_deref().unwrap().starts_with("Update required"));
        // Afterwards the game's own return to the Viewer is an ordinary idle report.
        auth.native_session_state(true).unwrap();
        auth.native_session_state(false).unwrap();
        assert!(!auth.game_running.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(&directory);
    }}
