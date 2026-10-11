//! Fresh credential login, separate from the earlier POL-session handoff.
//! No connection is made until `sign_in` is called. Mock tests do not establish
//! retail interoperability. US defaults are traced from the installed viewer.

pub use crate::keepalive::{SignOutReport, DROP_SIGN_OUT_TIMEOUT, SIGN_OUT_TIMEOUT};
pub use crate::loader::{
    LoaderSession, LoaderSessionError, PresenceArgs, PresenceError, PresenceOutcome, Region,
    CONTRACT_VERSION, PRESENCE_ARGS_LEN,
};
pub use crate::pol_records::PresenceState;
use crate::{
    catalog, chat, chat_transport, credential, entropy, identity, keepalive, loader,
    friends, lobby_selection, pol_records, profile, rsa, session, transport,
};
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub struct Credentials<'a> {
    pub pol_id: &'a str,
    pub pol_password: &'a str,
    pub se_id: &'a str,
    pub se_password: &'a str,
    pub otp: Option<&'a str>,
}

pub struct Options {
    pub chat_host: String,
    pub chat_port: u16,
    pub nick_region: u8,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}
impl Options {
    /// Installed US env.dat POL_UCS_AREA_KBN=01, primary chat flags0x11,
    /// independent USER mode8. Not the launcher's US/EU/JP preference enum.
    ///
    /// `io_timeout` bounds each wait for a server reply. It was 30 s; the
    /// slowest stage of a live 20.8 s sign-in (2026-10-03 05:35) took 5.2 s
    /// including its connect, so 20 s keeps about four times that margin while
    /// a stalled server is reported in 20 s instead of 30 s.
    pub fn installed_us() -> Self {
        Self {
            chat_host: "ci000.pol.com".into(),
            chat_port: 51240,
            nick_region: 1,
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Validate,
    ChatConnect,
    ChatReconnect,
    ChatRegister,
    MemberLogin,
    Handles,
    SelectionMap,
    Selection,
    Community,
    Ready,
}
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Validate => "account validation",
            Self::ChatConnect => "retail connection",
            Self::ChatReconnect => "server redirect connection",
            Self::ChatRegister => "PlayOnline registration",
            Self::MemberLogin => "Square Enix sign-in",
            Self::Handles => "account handles",
            Self::SelectionMap => "account selection map",
            Self::Selection => "account selection",
            Self::Community => "FFXI lobby session",
            Self::Ready => "server-issued session ready",
        })
    }
}

/// How long one sign-in stage took. `progress(stage)` is still called as each
/// stage starts; these are the measured durations, kept on [`Authenticated`]
/// and [`Error`] for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageTiming {
    pub stage: Stage,
    /// When the stage started, from the start of the sign-in.
    pub started: Duration,
    pub elapsed: Duration,
    /// The part of `elapsed` spent resolving and opening TCP connections. The
    /// rest is almost entirely waiting for the server's replies.
    pub connect: Duration,
    /// The stage ran at the same time as other `concurrent` stages.
    pub concurrent: bool,
}
impl std::fmt::Display for StageTiming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {:.2}s (connect {:.2}s{})",
            self.stage,
            self.elapsed.as_secs_f64(),
            self.connect.as_secs_f64(),
            if self.concurrent { ", concurrent" } else { "" }
        )
    }
}

#[derive(Debug)]
pub struct Error {
    pub stage: Stage,
    reason: String,
    /// The network error behind this failure, if any.
    kind: Option<io::ErrorKind>,
    /// The failure happened while opening a connection, not inside an exchange.
    connecting: bool,
    hint: Option<String>,
    elapsed: Option<Duration>,
    timings: Vec<StageTiming>,
}
impl Error {
    fn at(stage: Stage, error: impl std::fmt::Display) -> Self {
        Self {
            stage,
            reason: error.to_string(),
            kind: None,
            connecting: false,
            hint: None,
            elapsed: None,
            timings: Vec::new(),
        }
    }
    fn network(stage: Stage, error: impl std::fmt::Display, kind: Option<io::ErrorKind>) -> Self {
        Self {
            kind,
            ..Self::at(stage, error)
        }
    }
    fn connect(stage: Stage, error: &io::Error) -> Self {
        Self {
            connecting: true,
            ..Self::network(stage, error.kind(), Some(error.kind()))
        }
    }
    fn exchange(stage: Stage, error: transport::ExchangeError) -> Self {
        let kind = match &error {
            transport::ExchangeError::Io(e) => Some(e.kind()),
            transport::ExchangeError::Catalog(catalog::CatalogError::Io(e)) => Some(e.kind()),
            _ => None,
        };
        Self::network(stage, error, kind)
    }
    fn handshake(stage: Stage, error: chat_transport::HandshakeError) -> Self {
        let kind = match error {
            chat_transport::HandshakeError::Io(kind) => Some(kind),
            _ => None,
        };
        Self::network(stage, error, kind)
    }
    fn keepalive(stage: Stage, error: keepalive::KeepaliveError) -> Self {
        let kind = match error {
            keepalive::KeepaliveError::Io(kind) => Some(kind),
            _ => None,
        };
        Self::network(stage, error, kind)
    }

    /// A suggested next step, when the failure matches a known pattern (a
    /// server that accepted the connection but stalled, shortly after this
    /// PlayOnline ID's previous session). Also part of `Display`.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
    /// How long the failing stage ran before it failed.
    pub fn elapsed(&self) -> Option<Duration> {
        self.elapsed
    }
    /// Every stage that ran, including the failing one.
    pub fn stage_timings(&self) -> &[StageTiming] {
        &self.timings
    }

    fn timed_out(&self) -> bool {
        matches!(
            self.kind,
            Some(io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        )
    }
    /// The server accepted the connection, then stopped answering or dropped
    /// it: the pattern seen on relogins right after a game exit.
    fn server_stalled(&self) -> bool {
        !self.connecting
            && !matches!(self.stage, Stage::Validate)
            && matches!(
                self.kind,
                Some(
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                )
            )
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.reason)?;
        if let Some(elapsed) = self.elapsed {
            write!(f, " (after {:.1}s)", elapsed.as_secs_f64())?;
        }
        if let Some(hint) = &self.hint {
            write!(f, ". {hint}")?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}

/// Drop closes the managed chat connection; callers must retain this object
/// during the game child's lifetime. Dropping it signs out of the chat session
/// (bounded by [`DROP_SIGN_OUT_TIMEOUT`]); call [`Authenticated::sign_out`]
/// when the game exits to do so explicitly and get a report. Has no Debug dump.
///
/// It retains, zeroed on drop, what polcore itself keeps after a sign-in: the
/// POL ID and POL password (every profile request's header digest needs them),
/// the chat key/IV, POL address, and the handle/character tables. That is what
/// `send_presence` and `loader_session` need; the Square Enix credentials are
/// not retained.
pub struct Authenticated {
    pub session: session::LobbySession,
    pub keepalive: keepalive::Keepalive,
    loader: LoaderState,
    timings: Vec<StageTiming>,
    friends: Mutex<Option<friends::Friends>>,
}

impl Authenticated {
    /// Fetch the two POL friend lists using this authenticated chat session.
    /// Never signs in again or changes the selected game/character.
    pub fn fetch_friends(&self) -> Result<friends::Friends, String> {
        self.keepalive.check().map_err(|e| e.to_string())?;
        let mut friends = self.loader.fetch_friends(|host, port| {
            let socket = connect_with(host, port,
                self.loader.connect_timeout.min(Duration::from_secs(5)),
                self.loader.io_timeout.min(Duration::from_secs(5)))?;
            Ok(DeadlineChannel { socket, deadline: Instant::now() + Duration::from_secs(10) })
        }).map_err(|e| e.to_string())?;
        self.keepalive.apply_friend_presence(&mut friends, identity::pol_id_mask(&self.loader.member_id));
        *self.friends.lock().unwrap_or_else(|e| e.into_inner()) = Some(friends.clone());
        Ok(friends)
    }

    /// Cached friends with new server pushes applied; no network round trip.
    pub fn friend_updates(&self, since: u64) -> Option<(u64, friends::Friends)> {
        self.keepalive.check().ok()?;
        if self.keepalive.friend_revision() == since { return None; }
        let mut cache = self.friends.lock().unwrap_or_else(|e| e.into_inner());
        let friends = cache.as_mut()?;
        let revision = self.keepalive.apply_friend_presence(friends, identity::pol_id_mask(&self.loader.member_id));
        Some((revision, friends.clone()))
    }

    /// The contract `session` values for a POL-less loader. `install_dir` is
    /// left `None` for the launcher to fill. Fails only for a sign-in area
    /// code with no verified region (anything but the installed US value).
    pub fn loader_session(&self) -> Result<LoaderSession, LoaderSessionError> {
        self.loader.loader_session(&self.session)
    }

    /// polcore's current ChangeMyStatus state and whether the community state
    /// is valid (true after a successful sign-in).
    pub fn presence_state(&self) -> (PresenceState, bool) {
        self.loader.presence_state()
    }

    /// Forward a POL status update that FFXiMain asked polcore to send
    /// (slot 195/376, ChangeMyStatus): `payload` is the loader's 24-byte
    /// [`PresenceArgs`]. Blocks for one profile round trip (bounded by the
    /// sign-in's connect/IO timeouts); call it off the UI thread.
    ///
    /// Wire format, as polcore `1001d7e0`/`1001db90`: one community (4,5)
    /// transaction on a fresh TCP connection to the profile server
    /// (`ppNNN.pol.com:51220`, or its cached redirect), enciphered with the
    /// live chat session's Blowfish key and IV and authenticated with the POL
    /// ID/password digest. The chat (51240) connection itself carries only
    /// PING/PONG, so it must be alive (checked first) but is not written to.
    /// An unchanged request completes without network I/O, as in polcore.
    ///
    /// Statically derived; not yet confirmed against the live service.
    pub fn send_presence(&self, payload: &[u8]) -> Result<PresenceOutcome, PresenceError> {
        let args = PresenceArgs::parse(payload)?;
        self.keepalive.check().map_err(PresenceError::ChatSession)?;
        let (connect_timeout, io_timeout) = (self.loader.connect_timeout, self.loader.io_timeout);
        self.loader.presence(&args, |host, port| {
            connect_with(host, port, connect_timeout, io_timeout)
        })
    }

    /// Sign out of PlayOnline as POL does when it closes: send `QUIT` on the
    /// chat session, wait for the server's `ERROR`, and close the connection
    /// with a FIN. Call it when the game exits. Blocks for at most
    /// [`SIGN_OUT_TIMEOUT`] plus a short grace; never fails. The session
    /// material is zeroed afterwards, as on drop.
    ///
    /// Sequence traced from polcore (see `keepalive`); not yet confirmed
    /// against the live service. No profile-server "offline" status call is
    /// made: none was found on POL's own sign-out path.
    pub fn sign_out(self) -> SignOutReport {
        self.sign_out_within(SIGN_OUT_TIMEOUT)
    }

    /// [`Authenticated::sign_out`] with a caller-chosen bound.
    pub fn sign_out_within(self, timeout: Duration) -> SignOutReport {
        // The rest of `self` drops after the sign-out, so the session record
        // sees its outcome.
        self.keepalive.sign_out_within(timeout)
    }

    /// How long each sign-in stage took.
    pub fn stage_timings(&self) -> &[StageTiming] {
        &self.timings
    }
}

/// A catalog is many reads; a slow trickle must not restart its timeout for
/// each record and hold the session owner indefinitely during sign-out.
struct DeadlineChannel { socket: TcpStream, deadline: Instant }
impl DeadlineChannel {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }
}
impl Read for DeadlineChannel {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes)
    }
}
impl Write for DeadlineChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> { self.socket.flush() }
}

/// Session material retained after sign-in for the loader and presence.
struct LoaderState {
    area: u8,
    account_id: Zeroizing<u64>,
    member_id: Zeroizing<[u8; 8]>,
    member_secret: Zeroizing<Vec<u8>>,
    address: Zeroizing<[u8; 20]>,
    chat_key: Zeroizing<[u8; 8]>,
    iv: (u32, u32),
    greeting_clock: u32,
    host: u8,
    connect_timeout: Duration,
    io_timeout: Duration,
    records: pol_records::PolRecords,
    map: catalog::SelectionMap,
    presence: Mutex<PresenceSlot>,
    /// Records when this chat session ended, for the relogin hint.
    _session: Option<SessionMark>,
}

struct PresenceSlot {
    state: PresenceState,
    valid: bool,
    peer: ProfilePeer,
    lobby: Option<session::LobbySession>,
}

impl LoaderState {
    fn loader_session(
        &self,
        lobby: &session::LobbySession,
    ) -> Result<LoaderSession, LoaderSessionError> {
        let region = Region::from_area_code(self.area)
            .ok_or(LoaderSessionError::UnverifiedRegion(self.area))?;
        let slot = self.presence.lock().unwrap_or_else(|e| e.into_inner());
        let lobby = slot.lobby.as_ref().unwrap_or(lobby);
        let blob = self.records.to_blob(&slot.state);
        let mut pol_records = Vec::with_capacity(blob.len());
        pol_records.extend_from_slice(&blob);
        Ok(LoaderSession {
            region,
            lang: region,
            install_dir: None,
            v_key: *lobby.value(),
            auth_code: *lobby.authcode(),
            pol_records: Some(pol_records),
            content_ids: self.records.content_ids(),
            account_id: self.native_account_id(),
        })
    }

    fn presence_state(&self) -> (PresenceState, bool) {
        let slot = self.presence.lock().unwrap_or_else(|e| e.into_inner());
        (slot.state, slot.valid)
    }

    fn native_account_id(&self) -> u64 {
        *self.account_id
    }

    fn fetch_friends<C: Read + Write>(&self, mut connect: impl FnMut(&str, u16) -> io::Result<C>)
        -> Result<friends::Friends, transport::ExchangeError> {
        // Only copy the redirect under the presence lock; friends I/O must not
        // hold up ChangeMyStatus (world entry / character switch).
        let hostname = self.presence.lock().unwrap_or_else(|e| e.into_inner()).peer.hostname.clone();
        let parameters = transport::Parameters {
            member_id: &self.member_id, member_secret: &self.member_secret,
            pol_address: &self.address, session_key: &self.chat_key, iv: self.iv,
        };
        let mut channel = connect(&hostname, 51220)?;
        let reply = transport::exchange_content_ids(&mut channel, &parameters, 300)?;
        drop(channel);
        self.presence.lock().unwrap_or_else(|e| e.into_inner()).peer.apply(&reply.header);
        Ok(friends::Friends::from_catalog(&reply.catalog, identity::pol_id_mask(&self.member_id)))
    }

    /// One ChangeMyStatus round trip. The lock serializes updates, as polcore
    /// keeps a single retained state.
    fn presence<C: Read + Write>(
        &self,
        args: &PresenceArgs,
        mut connect: impl FnMut(&str, u16) -> io::Result<C>,
    ) -> Result<PresenceOutcome, PresenceError> {
        let mut slot = self.presence.lock().unwrap_or_else(|e| e.into_inner());
        let plan = loader::plan_presence(args, &slot.state, slot.valid, &self.map)?;
        if !plan.changed {
            return Ok(PresenceOutcome::Unchanged);
        }
        let parameters = transport::Parameters {
            member_id: &self.member_id,
            member_secret: &self.member_secret,
            pol_address: &self.address,
            session_key: &self.chat_key,
            iv: self.iv,
        };
        let mut channel =
            connect(&slot.peer.hostname, 51220).map_err(|e| PresenceError::Io(e.kind()))?;
        let reply = transport::exchange(
            &mut channel,
            &parameters,
            profile::ENTER_COMMUNITY,
            &plan.body,
        )?;
        drop(channel);
        slot.peer.apply(&reply.header);
        let mut body = Zeroizing::new([0; 28]);
        body.copy_from_slice(&reply.payload);
        let state = session::SessionState::new(
            *self.address,
            self.greeting_clock,
            *self.chat_key,
            self.host,
        );
        slot.state = plan.next;
        slot.valid = true;
        let session = session::LobbySession::from_community(&body, &state);
        slot.lobby = Some(session.clone());
        Ok(PresenceOutcome::Sent { session })
    }
}

// Relogin hint. On 2026-10-03 a game exited at 05:06:25 with its chat session
// dropped (no QUIT); relogins then stalled at different stages at +1.4, +2.5
// and +22 minutes, and the next one at +29 minutes worked. Whether the server
// still held the old session is inferred, not confirmed, so this is only used
// to word a hint, never to delay or refuse a sign-in.
const SESSION_LINGER: Duration = Duration::from_secs(30 * 60);

/// A one-way digest of the POL ID, so the in-process record holds no ID.
type AccountKey = [u8; 20];

struct SessionEnd {
    account: AccountKey,
    at: Instant,
    clean: bool,
}
static SESSION_ENDS: Mutex<Vec<SessionEnd>> = Mutex::new(Vec::new());

fn account_key(pol_id: &[u8; 8]) -> AccountKey {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(b"crystal-pol session end\0");
    hash.update(pol_id);
    hash.finalize().into()
}

fn note_session_end(account: AccountKey, at: Instant, clean: bool) {
    let mut ends = SESSION_ENDS.lock().unwrap_or_else(|e| e.into_inner());
    ends.retain(|end| end.account != account && end.at.elapsed() < SESSION_LINGER);
    ends.push(SessionEnd { account, at, clean });
}

/// How long ago this account's last chat session ended in this process, and
/// whether it was signed out cleanly (the server answered or closed).
fn previous_session(account: &AccountKey) -> Option<(Duration, bool)> {
    let ends = SESSION_ENDS.lock().unwrap_or_else(|e| e.into_inner());
    ends.iter()
        .rev()
        .find(|end| &end.account == account)
        .map(|end| (end.at.elapsed(), end.clean))
        .filter(|(ago, _)| *ago < SESSION_LINGER)
}

/// Notes the end of a registered chat session when dropped.
struct SessionMark {
    account: AccountKey,
    outcome: Arc<Mutex<Option<SignOutReport>>>,
}
impl Drop for SessionMark {
    fn drop(&mut self) {
        let clean = self
            .outcome
            .lock()
            .ok()
            .and_then(|report| *report)
            .is_some_and(|report| report.acknowledged || report.peer_closed);
        note_session_end(self.account, Instant::now(), clean);
    }
}

/// A registered chat session during sign-in. Fields drop in order, so the
/// keepalive signs out before the mark records how that went.
struct LiveSession {
    keepalive: keepalive::Keepalive,
    mark: SessionMark,
}

fn annotate(mut error: Error, account: Option<&AccountKey>, options: &Options) -> Error {
    if error.timed_out() && !error.connecting {
        error.reason = format!(
            "{} (no reply within {}s)",
            error.reason,
            options.io_timeout.as_secs()
        );
    }
    if !error.server_stalled() {
        return error;
    }
    error.hint = match account.and_then(previous_session) {
        Some((ago, clean)) => {
            let left = SESSION_LINGER
                .saturating_sub(ago)
                .as_secs()
                .div_ceil(60)
                .max(1);
            Some(format!(
                "The previous session for this PlayOnline ID ended {} min ago{} and may still \
                 be active on the server; wait about {left} min, then try again",
                ago.as_secs() / 60,
                if clean {
                    ""
                } else {
                    " without a clean sign-out"
                },
            ))
        }
        None if error.timed_out() => Some(
            "If this PlayOnline ID was signed in during the last 30 minutes (for example by \
             a game that just closed), the server may still be holding that session; wait a \
             few minutes, then try again"
                .into(),
        ),
        None => None,
    };
    error
}

/// Measures stages as `progress` enters them.
struct StageClock {
    start: Instant,
    current: Option<(Stage, Instant)>,
    connect: Duration,
    timings: Vec<StageTiming>,
}
impl StageClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            current: None,
            connect: Duration::ZERO,
            timings: Vec::new(),
        }
    }
    fn enter(&mut self, stage: Stage, progress: &mut dyn FnMut(Stage)) {
        self.finish();
        self.current = Some((stage, Instant::now()));
        progress(stage);
    }
    fn finish(&mut self) {
        if let Some((stage, at)) = self.current.take() {
            self.timings.push(StageTiming {
                stage,
                started: at.saturating_duration_since(self.start),
                elapsed: at.elapsed(),
                connect: std::mem::take(&mut self.connect),
                concurrent: false,
            });
        }
    }
    fn connected(&mut self, took: Duration) {
        self.connect += took;
    }
    fn into_timings(mut self) -> Vec<StageTiming> {
        self.finish();
        self.timings
    }
}

fn prepare(
    input: &Credentials<'_>,
) -> Result<(Zeroizing<[u8; 8]>, u64, credential::ProfileCredential), Error> {
    let stage = Stage::Validate;
    if input.pol_id.len() != 8 || !input.pol_id.is_ascii() {
        return Err(Error::at(stage, "enter the eight-character PlayOnline ID"));
    }
    let mut pol_id = Zeroizing::new([0; 8]);
    for (out, byte) in pol_id.iter_mut().zip(input.pol_id.bytes()) {
        *out = byte.to_ascii_uppercase();
    }
    let account =
        identity::id_decode(&pol_id[..]).map_err(|_| Error::at(stage, "invalid PlayOnline ID"))?;
    if input.pol_password.is_empty()
        || input.pol_password.len() > 15
        || !input.pol_password.bytes().all(|b| (32..=126).contains(&b))
    {
        return Err(Error::at(
            stage,
            "PlayOnline password must contain1 to15 ASCII characters",
        ));
    }
    if input.se_password.is_empty()
        || input.se_password.len() > 512
        || input.se_password.contains('\0')
    {
        return Err(Error::at(stage, "invalid Square Enix password length"));
    }
    let password = Zeroizing::new(input.se_password.encode_utf16().collect::<Vec<_>>());
    let secret = credential::EditorSecret::from_utf16(&password);
    let profile = credential::ProfileCredential::new(input.se_id, secret, input.otp)
        .map_err(|e| Error::at(stage, e))?;
    Ok((pol_id, account, profile))
}

/// Perform one login attempt; never retries rejected credentials. All account
/// exchanges remain off the caller's UI thread. Endpoints contain no secrets.
///
/// `progress` is called as each stage starts. After member login, the handle
/// list, selection map and selection requests run at the same time (each on
/// its own connection, with the same bytes as before), so those three stages
/// are reported together; [`Authenticated::stage_timings`] and
/// [`Error::stage_timings`] have each stage's own duration.
pub fn sign_in(
    input: Credentials<'_>,
    options: &Options,
    mut progress: impl FnMut(Stage),
) -> Result<Authenticated, Error> {
    let mut clock = StageClock::new();
    let mut account = None;
    let result = sign_in_stages(&input, options, &mut clock, &mut progress, &mut account);
    match result {
        Ok(mut authenticated) => {
            authenticated.timings = clock.into_timings();
            Ok(authenticated)
        }
        Err(error) => {
            let timings = clock.into_timings();
            let mut error = annotate(error, account.as_ref(), options);
            error.elapsed = timings
                .iter()
                .rev()
                .find(|t| t.stage == error.stage)
                .map(|t| t.elapsed);
            error.timings = timings;
            Err(error)
        }
    }
}

fn sign_in_stages(
    input: &Credentials<'_>,
    options: &Options,
    clock: &mut StageClock,
    progress: &mut dyn FnMut(Stage),
    account_out: &mut Option<AccountKey>,
) -> Result<Authenticated, Error> {
    clock.enter(Stage::Validate, progress);
    let (pol_id, account, credential) = prepare(input)?;
    *account_out = Some(account_key(&pol_id));
    let suffix = identity::NickSuffix::windows_baseline(options.nick_region, &pol_id[..])
        .map_err(|e| Error::at(Stage::Validate, e))?;
    // Generate before opening a socket so primality work does not consume the
    // server's greeting timeout. Randomness uses BCryptGenRandom on Windows.
    let key = rsa::KeyPair::generate(&mut entropy::SystemRandom)
        .map_err(|_| Error::at(Stage::Validate, "could not generate the session key"))?;
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u32;
    clock.enter(Stage::ChatConnect, progress);
    let began = Instant::now();
    let socket = connect(&options.chat_host, options.chat_port, options)
        .map_err(|e| Error::connect(Stage::ChatConnect, &e))?;
    clock.connected(began.elapsed());
    clock.enter(Stage::ChatRegister, progress);
    let mut reconnect_failed = false;
    let registered = chat_transport::register_reconnecting(
        socket,
        key,
        chat_transport::Inputs {
            account_id: account,
            pol_secret: input.pol_password.as_bytes(),
            suffix: &suffix,
            user_mode: 8,
            address_tail: [0; 12],
            initial_metadata: chat::RegistrationMetadata { converted: [0; 24] },
            maximum_line_bytes: 4096,
        },
        |target| {
            clock.enter(Stage::ChatReconnect, progress);
            let began = Instant::now();
            let result = connect(
                &Ipv4Addr::from(target.ipv4).to_string(),
                target.port,
                options,
            );
            clock.connected(began.elapsed());
            reconnect_failed = result.is_err();
            result.map_err(chat_transport::HandshakeError::from)
        },
        now,
    )
    .map_err(|e| {
        // The stage the clock is in: registration, or the redirect after it.
        let stage = clock
            .current
            .map_or(Stage::ChatRegister, |(stage, _)| stage);
        let mut error = Error::handshake(stage, e);
        error.connecting = reconnect_failed;
        error
    })?;
    let address = registered.address;
    let chat_key = Zeroizing::new(*registered.session_key);
    let iv = registered.iv;
    let server_clock = registered.clock;
    let greeting_clock = registered.greeting.server_clock;
    let host = registered.metadata.host_index();
    // 10044e3f -> 1001e9b0 -> 10019dc0 uses the actual converted 300
    // metadata word and byte 2, preserving the decoded account's low 41 bits.
    let class = u16::from_le_bytes(registered.metadata.converted[..2].try_into().unwrap());
    let kind = registered.metadata.converted[2] & 0x7f;
    let native_account_id = (account & ((1 << 41) - 1))
        | ((u64::from(class) << 7 | u64::from(kind)) << 41);
    let native_account_id = Zeroizing::new(native_account_id ^ identity::pol_id_mask(&pol_id));
    // The native separate metadata BSS starts at zero; if no300 arrives, retain
    // that state instead of inventing an entitlement/metadata readiness gate.
    let keepalive = keepalive::Keepalive::start(registered)
        .map_err(|e| Error::keepalive(Stage::ChatRegister, e))?;
    let live = LiveSession {
        mark: SessionMark {
            account: account_key(&pol_id),
            outcome: keepalive.outcome_handle(),
        },
        keepalive,
    };
    let parameters = transport::Parameters {
        member_id: &pol_id,
        member_secret: input.pol_password.as_bytes(),
        pol_address: &address,
        session_key: &chat_key,
        iv,
    };
    let connect_profile = |hostname: &str, port: u16| connect(hostname, port, options);
    let login = profile_login(
        &connect_profile,
        host,
        &parameters,
        &credential,
        || server_clock.seconds_at(now()),
        clock,
        progress,
    )?;
    let state = session::SessionState::new(address, greeting_clock, *chat_key, host);
    let session = session::LobbySession::from_community(&login.reply, &state);
    live.keepalive
        .check()
        .map_err(|e| Error::keepalive(Stage::Ready, e))?;
    let LiveSession { keepalive, mark } = live;
    let loader = LoaderState {
        area: options.nick_region,
        account_id: native_account_id,
        member_id: pol_id,
        member_secret: Zeroizing::new(input.pol_password.as_bytes().to_vec()),
        address: Zeroizing::new(address),
        chat_key,
        iv,
        greeting_clock,
        host,
        connect_timeout: options.connect_timeout,
        io_timeout: options.io_timeout,
        records: login.records,
        map: login.map,
        presence: Mutex::new(PresenceSlot {
            state: login.presence,
            valid: true,
            peer: login.peer,
            lobby: None,
        }),
        _session: Some(mark),
    };
    clock.enter(Stage::Ready, progress);
    Ok(Authenticated {
        session,
        keepalive,
        loader,
        timings: Vec::new(),
        friends: Mutex::new(None),
    })
}

fn connect(host: &str, port: u16, options: &Options) -> io::Result<TcpStream> {
    connect_with(host, port, options.connect_timeout, options.io_timeout)
}

fn connect_with(
    host: &str,
    port: u16,
    connect_timeout: Duration,
    io_timeout: Duration,
) -> io::Result<TcpStream> {
    // Match the original IPv4 transport. Do not choose a guessed fallback host.
    let addresses = (host, port).to_socket_addrs()?;
    let mut last = io::Error::from(io::ErrorKind::AddrNotAvailable);
    for address in addresses.filter(|a| a.is_ipv4()) {
        match TcpStream::connect_timeout(&address, connect_timeout) {
            Ok(socket) => {
                socket.set_read_timeout(Some(io_timeout))?;
                socket.set_write_timeout(Some(io_timeout))?;
                socket.set_nodelay(true)?;
                return Ok(socket);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

struct ProfilePeer {
    hostname: String,
    cached: bool,
}
impl ProfilePeer {
    fn new(host: u8) -> Self {
        Self {
            hostname: format!("pp{host:03}.pol.com"),
            cached: false,
        }
    }
    fn apply(&mut self, header: &profile::ReplyHeader) {
        // Native caches the positive reply's internal host-orderIPv4 for the
        // next request. It neither repeats this request nor retries a login.
        if !self.cached && header.redirect != 0 {
            self.hostname = Ipv4Addr::from(header.redirect.to_be_bytes()).to_string();
            self.cached = true;
        }
    }
}

/// Everything the profile phase of a sign-in yields.
struct ProfileLogin {
    reply: Zeroizing<[u8; 28]>,
    records: pol_records::PolRecords,
    map: catalog::SelectionMap,
    /// Retained ChangeMyStatus state after the lobby (4,5) succeeded.
    presence: PresenceState,
    peer: ProfilePeer,
}

type Connector<'a, C> = &'a (dyn Fn(&str, u16) -> io::Result<C> + Sync);

/// Open a fresh profile connection and run one transaction on it; the channel
/// closes on return, as native does after every transaction.
fn transaction<C, T>(
    connect: Connector<'_, C>,
    hostname: &str,
    stage: Stage,
    run: impl FnOnce(&mut C) -> Result<T, transport::ExchangeError>,
) -> (Result<T, Error>, Duration) {
    let began = Instant::now();
    let mut channel = match connect(hostname, 51220) {
        Ok(channel) => channel,
        Err(e) => return (Err(Error::connect(stage, &e)), began.elapsed()),
    };
    let connected = began.elapsed();
    (
        run(&mut channel).map_err(|e| Error::exchange(stage, e)),
        connected,
    )
}

type Fetched<T> = (Result<T, Error>, StageTiming);

/// Run a transaction on this thread or a scoped one, measuring it.
fn timed<C, T>(
    origin: Instant,
    stage: Stage,
    connect: Connector<'_, C>,
    hostname: &str,
    run: impl FnOnce(&mut C) -> Result<T, transport::ExchangeError>,
) -> Fetched<T> {
    let began = Instant::now();
    let (result, connect) = transaction(connect, hostname, stage, run);
    let timing = StageTiming {
        stage,
        started: began.saturating_duration_since(origin),
        elapsed: began.elapsed(),
        connect,
        concurrent: true,
    };
    (result, timing)
}

fn profile_login<C: Read + Write>(
    connect: Connector<'_, C>,
    host: u8,
    params: &transport::Parameters<'_>,
    credential: &credential::ProfileCredential,
    mut seconds: impl FnMut() -> u32,
    clock: &mut StageClock,
    progress: &mut dyn FnMut(Stage),
) -> Result<ProfileLogin, Error> {
    let mut peer = ProfilePeer::new(host);
    clock.enter(Stage::MemberLogin, progress);
    let payload = credential.login_payload(seconds());
    let (member, took) = transaction(connect, &peer.hostname, Stage::MemberLogin, |channel| {
        transport::exchange(channel, params, profile::MEMBER_LOGIN, &payload[..])
    });
    clock.connected(took);
    let member = member?;
    peer.apply(&member.header);
    drop(payload);

    let (handles, map_reply, selection) = if peer.cached {
        // The profile server is now fixed for every later request, so the
        // three independent requests can run at once without changing where
        // or what is sent. Native polcore likewise runs its app tasks as
        // concurrent per-connection state machines (sign-in findings,
        // "App10040650 queues tasks ... no waits between calls").
        clock.finish();
        progress(Stage::Handles);
        progress(Stage::SelectionMap);
        progress(Stage::Selection);
        let origin = clock.start;
        let hostname = peer.hostname.as_str();
        let (handles, map_reply, selection) = thread::scope(|scope| {
            let handles = scope.spawn(|| {
                timed(origin, Stage::Handles, connect, hostname, |channel| {
                    transport::exchange_services(channel, params)
                })
            });
            let map = scope.spawn(|| {
                timed(origin, Stage::SelectionMap, connect, hostname, |channel| {
                    transport::exchange_selection_map(channel, params)
                })
            });
            let selection = timed(origin, Stage::Selection, connect, hostname, |channel| {
                transport::exchange(channel, params, profile::SELECT_SERVICE, &[0; 20])
            });
            (
                joined(handles.join(), Stage::Handles),
                joined(map.join(), Stage::SelectionMap),
                selection,
            )
        });
        clock.timings.extend([handles.1, map_reply.1, selection.1]);
        // Report the first failure in the original stage order.
        (handles.0?, map_reply.0?, selection.0?)
    } else {
        clock.enter(Stage::Handles, progress);
        let (handles, took) = transaction(connect, &peer.hostname, Stage::Handles, |channel| {
            transport::exchange_services(channel, params)
        });
        clock.connected(took);
        let handles = handles?;
        peer.apply(&handles.header);

        clock.enter(Stage::SelectionMap, progress);
        let (map_reply, took) =
            transaction(connect, &peer.hostname, Stage::SelectionMap, |channel| {
                transport::exchange_selection_map(channel, params)
            });
        clock.connected(took);
        let map_reply = map_reply?;
        peer.apply(&map_reply.header);

        clock.enter(Stage::Selection, progress);
        let (selection, took) = transaction(connect, &peer.hostname, Stage::Selection, |channel| {
            transport::exchange(channel, params, profile::SELECT_SERVICE, &[0; 20])
        });
        clock.connected(took);
        (handles, map_reply, selection?)
    };
    peer.apply(&handles.header);
    peer.apply(&map_reply.header);
    peer.apply(&selection.header);
    let mut map = crate::catalog::SelectionMap::default();
    map.refresh(&map_reply.catalog)
        .map_err(|e| Error::at(Stage::SelectionMap, e))?;
    let selected = lobby_selection::SelectionState::parse(&selection.payload, &map)
        .map_err(|e| Error::at(Stage::Selection, e))?;
    let request = selected
        .lobby_request(&handles.catalog)
        .map_err(|e| Error::at(Stage::Selection, e))?;
    // The same call through polcore's 1001d7e0 model yields the state the
    // server holds after this request; the wire bodies are identical.
    let presence = loader::plan_presence(
        &loader::PresenceArgs::LOBBY,
        &selection_presence(&selected),
        false,
        &map,
    )
    .map_err(|e| Error::at(Stage::Selection, e))?
    .next;

    clock.enter(Stage::Community, progress);
    let (community, took) = transaction(connect, &peer.hostname, Stage::Community, |channel| {
        transport::exchange(channel, params, profile::ENTER_COMMUNITY, &request)
    });
    clock.connected(took);
    let community = community?;
    peer.apply(&community.header);
    let mut reply = Zeroizing::new([0; 28]);
    reply.copy_from_slice(&community.payload);
    if reply[..16].iter().all(|v| *v == 0) {
        return Err(Error::at(
            Stage::Community,
            "server returned an empty session",
        ));
    }
    let records = pol_records::PolRecords::build(&handles.catalog, &map_reply.catalog, &map);
    Ok(ProfileLogin {
        reply,
        records,
        map,
        presence,
        peer,
    })
}

fn joined<T>(result: thread::Result<Fetched<T>>, stage: Stage) -> Fetched<T> {
    result.unwrap_or_else(|_| {
        (
            Err(Error::at(stage, "profile worker stopped unexpectedly")),
            StageTiming {
                stage,
                started: Duration::ZERO,
                elapsed: Duration::ZERO,
                connect: Duration::ZERO,
                concurrent: true,
            },
        )
    })
}

/// polcore's retained globals after a successful selection (4,6), before the
/// lobby community call (`1001d658..1001d769`).
fn selection_presence(selected: &lobby_selection::SelectionState) -> PresenceState {
    PresenceState {
        handle: selected.handle().map_or(-1, i32::from),
        primary: selected.primary().map_or(-1, i32::from),
        context: i32::from(selected.context()),
        selection_type: i32::from(selected.selection_type()),
        flag: i32::from(selected.flag()),
        content_class: i32::from(selected.content_class()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{self, PolBlowfish, StreamReset};
    use std::{
        collections::{HashMap, VecDeque},
        io::Cursor,
        sync::{Arc, Condvar, Mutex},
    };
    const KEY: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const IV: (u32, u32) = (0x12345678, 0x87654321);
    struct Channel {
        input: Cursor<Vec<u8>>,
        output: Arc<Mutex<Vec<u8>>>,
    }
    impl Read for Channel {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(3);
            self.input.read(&mut out[..n])
        }
    }
    impl Write for Channel {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn reply(status: u8, redirect: u32, payload: &[u8], sealed: bool) -> Channel {
        let mut input = vec![0; 24];
        input[20..24].copy_from_slice(&[9, 8, 7, 6]);
        let mut header = [0; 24];
        header[1] = status;
        let mut body = payload.to_vec();
        if sealed {
            body.extend(crypto::checksum(payload, 0).to_le_bytes());
        }
        header[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        header[8..12].copy_from_slice(&redirect.to_le_bytes());
        let mut cipher = PolBlowfish::new(&KEY, IV);
        input.extend(cipher.stream(&header, StreamReset::Boundary));
        input.extend(cipher.stream(&body, StreamReset::Continue));
        Channel {
            input: Cursor::new(input),
            output: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn input() -> Credentials<'static> {
        Credentials {
            pol_id: "TEST0001",
            pol_password: "synthetic",
            se_id: "Synthetic",
            se_password: "synthetic secret",
            otp: Some("123456"),
        }
    }
    fn params() -> transport::Parameters<'static> {
        transport::Parameters {
            member_id: b"TEST0001",
            member_secret: b"synthetic",
            pol_address: &[0; 20],
            session_key: &KEY,
            iv: IV,
        }
    }

    /// Hands out pre-built channels in connection order.
    struct Ordered {
        channels: Mutex<VecDeque<Channel>>,
        targets: Mutex<Vec<(String, u16)>>,
    }
    impl Ordered {
        fn new(channels: Vec<Channel>) -> Self {
            Self {
                channels: Mutex::new(channels.into()),
                targets: Mutex::new(Vec::new()),
            }
        }
        fn connect(&self, host: &str, port: u16) -> io::Result<Channel> {
            self.targets.lock().unwrap().push((host.to_owned(), port));
            Ok(self.channels.lock().unwrap().pop_front().unwrap())
        }
    }

    const CONCURRENT: [(u8, u8); 3] = [(0, 9), (1, 3), (4, 6)];

    /// A mock profile server: each connection answers the transaction its
    /// request names, so concurrent connections get the right replies.
    struct Routed {
        replies: Mutex<HashMap<(u8, u8), Vec<u8>>>,
        sent: Mutex<HashMap<(u8, u8), Vec<u8>>>,
        order: Mutex<Vec<(u8, u8)>>,
        targets: Mutex<Vec<String>>,
        /// When set, the three post-login requests must all be in flight
        /// before any of them is answered (proves they run at once).
        rendezvous: Option<(Mutex<usize>, Condvar)>,
    }
    impl Routed {
        fn new(replies: Vec<((u8, u8), Channel)>, rendezvous: bool) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(tx, c)| (tx, c.input.into_inner()))
                        .collect(),
                ),
                sent: Mutex::new(HashMap::new()),
                order: Mutex::new(Vec::new()),
                targets: Mutex::new(Vec::new()),
                rendezvous: rendezvous.then(|| (Mutex::new(0), Condvar::new())),
            })
        }
        fn connect(self: &Arc<Self>, host: &str, _: u16) -> io::Result<RoutedChannel> {
            self.targets.lock().unwrap().push(host.to_owned());
            Ok(RoutedChannel {
                server: Arc::clone(self),
                output: Vec::new(),
                input: None,
                hello: 0,
            })
        }
        fn outputs(&self, tx: profile::Transaction) -> Vec<u8> {
            self.sent.lock().unwrap()[&(tx.0, tx.1)].clone()
        }
    }
    struct RoutedChannel {
        server: Arc<Routed>,
        output: Vec<u8>,
        input: Option<Cursor<Vec<u8>>>,
        hello: usize,
    }
    impl Read for RoutedChannel {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(3);
            if self.input.is_none() {
                let mut hello = [0u8; 24];
                hello[20..24].copy_from_slice(&[9, 8, 7, 6]);
                if self.hello < 24 {
                    let n = n.min(24 - self.hello);
                    out[..n].copy_from_slice(&hello[self.hello..self.hello + n]);
                    self.hello += n;
                    return Ok(n);
                }
                let plain =
                    PolBlowfish::new(&KEY, IV).stream(&self.output[40..80], StreamReset::Boundary);
                let tx = (plain[1], plain[2]);
                self.server.order.lock().unwrap().push(tx);
                self.server
                    .sent
                    .lock()
                    .unwrap()
                    .insert(tx, self.output.clone());
                if let (Some((count, ready)), true) =
                    (&self.server.rendezvous, CONCURRENT.contains(&tx))
                {
                    let mut arrived = count.lock().unwrap();
                    *arrived += 1;
                    ready.notify_all();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while *arrived < CONCURRENT.len() {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        arrived = ready.wait_timeout(arrived, deadline - now).unwrap().0;
                    }
                }
                let bytes = self.server.replies.lock().unwrap().remove(&tx).unwrap();
                self.input = Some(Cursor::new(bytes[24..].to_vec()));
            }
            self.input.as_mut().unwrap().read(&mut out[..n])
        }
    }
    impl Write for RoutedChannel {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn run_login<C: Read + Write>(
        connect: &(dyn Fn(&str, u16) -> io::Result<C> + Sync),
        host: u8,
        stages: &mut Vec<Stage>,
    ) -> (Result<ProfileLogin, Error>, Vec<StageTiming>) {
        let (_, _, credential) = prepare(&input()).unwrap();
        let mut clock = StageClock::new();
        let result = profile_login(
            connect,
            host,
            &params(),
            &credential,
            || 123456,
            &mut clock,
            &mut |stage| stages.push(stage),
        );
        (result, clock.into_timings())
    }

    #[test]
    fn sequence_uses_fresh_channels_server_time_and_native_lobby_class() {
        let (_, _, credential) = prepare(&input()).unwrap();
        let mut selected = [0; 124];
        selected[0] = 5;
        selected[6] = 2;
        selected[7] = 1;
        let community: Vec<u8> = (1..=28).collect();
        let server = Routed::new(
            vec![
                ((4, 7), reply(0, 0x01020304, &[], false)),
                ((0, 9), reply(0, 0x09090909, &[0; 8], false)),
                ((1, 3), reply(0, 0, &[0; 8], false)),
                ((4, 6), reply(0, 0, &selected, true)),
                ((4, 5), reply(0, 0, &community, true)),
            ],
            true,
        );
        let mut stages = Vec::new();
        let (result, timings) = run_login(&|h, p| server.connect(h, p), 2, &mut stages);
        let result = result.unwrap();
        assert_eq!(&result.reply[..], &community);
        // Selection handle 5, type 1, flag 1; the lobby call clears primary,
        // context and sets class 1.
        assert_eq!(
            result.presence,
            PresenceState {
                handle: 5,
                primary: -1,
                context: 0,
                selection_type: 1,
                flag: 1,
                content_class: 1,
            }
        );
        let targets = server.targets.lock().unwrap();
        assert_eq!(targets[0], "pp002.pol.com");
        assert!(targets[1..].iter().all(|t| t == "1.2.3.4"));
        assert_eq!(targets.len(), 5);
        assert_eq!(
            stages,
            [
                Stage::MemberLogin,
                Stage::Handles,
                Stage::SelectionMap,
                Stage::Selection,
                Stage::Community
            ]
        );
        for tx in [
            profile::MEMBER_LOGIN,
            crate::catalog::SERVICES,
            crate::catalog::SELECTION_MAP,
            profile::SELECT_SERVICE,
            profile::ENTER_COMMUNITY,
        ] {
            // Byte-identical requests: plaintext hello, then the request.
            let out = server.outputs(tx);
            assert_eq!(&out[..40], &transport::connect_hello(&[0; 20]));
            let plain = Zeroizing::new(
                PolBlowfish::new(&KEY, IV).stream(&out[40..], StreamReset::Boundary),
            );
            let payload: Vec<u8> = match tx {
                profile::MEMBER_LOGIN => credential.login_payload(123456).to_vec(),
                profile::SELECT_SERVICE => vec![0; 20],
                profile::ENTER_COMMUNITY => plain[40..76].to_vec(),
                _ => Vec::new(),
            };
            let expected =
                profile::request(tx, &payload, b"TEST0001", b"synthetic", [9, 8, 7, 6]).unwrap();
            assert_eq!(plain, expected);
            if tx == profile::ENTER_COMMUNITY {
                assert_eq!(&plain[56..66], &[5, 0, 0, 1, 1, 0, 2, 1, 0, 1]);
            }
        }
        // Member login strictly first, community strictly last.
        let order = server.order.lock().unwrap();
        assert_eq!(order[0], (4, 7));
        assert_eq!(order[4], (4, 5));
        let concurrent: Vec<_> = timings
            .iter()
            .filter(|t| t.concurrent)
            .map(|t| t.stage)
            .collect();
        assert_eq!(
            concurrent,
            [Stage::Handles, Stage::SelectionMap, Stage::Selection]
        );
        assert_eq!(
            timings.iter().map(|t| t.stage).collect::<Vec<_>>(),
            [
                Stage::MemberLogin,
                Stage::Handles,
                Stage::SelectionMap,
                Stage::Selection,
                Stage::Community
            ]
        );
    }

    #[test]
    fn without_a_fixed_profile_server_requests_stay_sequential_and_follow_redirects() {
        let mut selected = [0; 124];
        selected[0] = 5;
        selected[6] = 2;
        selected[7] = 1;
        let community: Vec<u8> = (1..=28).collect();
        // No redirect from member login; the handle list's reply sets one,
        // which every later request must use.
        let server = Routed::new(
            vec![
                ((4, 7), reply(0, 0, &[], false)),
                ((0, 9), reply(0, 0x05060708, &[0; 8], false)),
                ((1, 3), reply(0, 0, &[0; 8], false)),
                ((4, 6), reply(0, 0, &selected, true)),
                ((4, 5), reply(0, 0, &community, true)),
            ],
            false,
        );
        let mut stages = Vec::new();
        let (result, timings) = run_login(&|h, p| server.connect(h, p), 2, &mut stages);
        result.unwrap();
        assert_eq!(
            *server.order.lock().unwrap(),
            [(4, 7), (0, 9), (1, 3), (4, 6), (4, 5)]
        );
        assert_eq!(
            *server.targets.lock().unwrap(),
            [
                "pp002.pol.com",
                "pp002.pol.com",
                "5.6.7.8",
                "5.6.7.8",
                "5.6.7.8"
            ]
        );
        assert!(timings.iter().all(|t| !t.concurrent));
    }

    #[test]
    fn concurrent_failure_reports_the_first_stage_in_order() {
        let server = Routed::new(
            vec![
                ((4, 7), reply(0, 0x01020304, &[], false)),
                ((0, 9), reply(0, 0, &[0; 8], false)),
                ((1, 3), reply(180, 0, &[], false)),
                ((4, 6), reply(180, 0, &[], false)),
            ],
            false,
        );
        let (result, timings) = run_login(&|h, p| server.connect(h, p), 2, &mut Vec::new());
        let error = result.err().unwrap();
        assert_eq!(error.stage, Stage::SelectionMap);
        assert!(error.to_string().contains("180"));
        assert!(!server.order.lock().unwrap().contains(&(4, 5)));
        assert_eq!(timings.len(), 4);
    }

    #[test]
    fn account_refusal_stops_before_catalogs_or_game_session() {
        let ordered = Ordered::new((0..5).map(|_| reply(180, 0, &[], false)).collect());
        let (result, _) = run_login(&|h, p| ordered.connect(h, p), 2, &mut Vec::new());
        let error = result.err().unwrap();
        assert_eq!(ordered.targets.lock().unwrap().len(), 1);
        assert_eq!(error.stage, Stage::MemberLogin);
        assert!(error.to_string().contains("180"));
        assert!(!error.to_string().contains("synthetic"));
    }
    #[test]
    fn validation_normalizes_pol_case_and_rejects_otp_before_network() {
        let mut credentials = input();
        credentials.pol_id = "test0001";
        assert_eq!(&prepare(&credentials).unwrap().0[..], b"TEST0001");
        credentials.otp = Some("invalid");
        assert_eq!(prepare(&credentials).err().unwrap().stage, Stage::Validate);
        credentials = input();
        credentials.pol_password = "sixteencharacters";
        assert!(prepare(&credentials).is_err());
        let defaults = Options::installed_us();
        assert_eq!((defaults.chat_port, defaults.nick_region), (51240, 1));
        assert_eq!(defaults.io_timeout, Duration::from_secs(20));
    }

    #[test]
    fn empty_session_check_uses_the_native_first_sixteen_bytes() {
        for valid in [false, true] {
            let mut payload = [0; 28];
            if valid {
                payload[0] = 1;
            } else {
                payload[27] = 1;
            }
            let ordered = Ordered::new(vec![
                reply(0, 0, &[], false),
                reply(0, 0, &[0; 8], false),
                reply(0, 0, &[0; 8], false),
                reply(0, 0, &[0; 124], true),
                reply(0, 0, &payload, true),
            ]);
            let (result, _) = run_login(&|h, p| ordered.connect(h, p), 0, &mut Vec::new());
            assert_eq!(result.is_ok(), valid);
        }
    }

    #[test]
    fn stalled_exchanges_fail_with_the_stage_the_wait_and_a_relogin_hint() {
        let options = Options::installed_us();
        let stalled = |stage| {
            Error::exchange(
                stage,
                transport::ExchangeError::Io(io::ErrorKind::TimedOut.into()),
            )
        };
        // No earlier session in this process: a softer, conditional hint.
        let unknown = account_key(b"HINT0001");
        let error = annotate(stalled(Stage::Selection), Some(&unknown), &options);
        let text = error.to_string();
        assert!(text.starts_with("account selection: "), "{text}");
        assert!(text.contains("no reply within 20s"), "{text}");
        assert!(error.hint().unwrap().contains("wait a few minutes"));

        // A session for this ID ended five minutes ago without a clean sign-out.
        let recent = account_key(b"HINT0002");
        note_session_end(recent, Instant::now() - Duration::from_secs(5 * 60), false);
        let error = annotate(stalled(Stage::ChatRegister), Some(&recent), &options);
        let hint = error.hint().unwrap();
        assert!(
            hint.contains("ended 5 min ago without a clean sign-out"),
            "{hint}"
        );
        assert!(hint.contains("wait about 25 min"), "{hint}");
        // A dropped connection fits the pattern too.
        let closed = Error::exchange(
            Stage::Handles,
            transport::ExchangeError::Catalog(catalog::CatalogError::Io(
                io::ErrorKind::UnexpectedEof.into(),
            )),
        );
        let error = annotate(closed, Some(&recent), &options);
        assert!(error.to_string().contains("catalog channel read failed"));
        assert!(error.hint().is_some());

        // Not the pattern: refused credentials, a connection that never
        // opened, and an end older than the window.
        let refused = Error::exchange(
            Stage::MemberLogin,
            transport::ExchangeError::Service {
                status: 180,
                details: Zeroizing::new(Vec::new()),
            },
        );
        assert!(annotate(refused, Some(&recent), &options).hint().is_none());
        let unreachable = Error::connect(Stage::Selection, &io::ErrorKind::TimedOut.into());
        let error = annotate(unreachable, Some(&recent), &options);
        assert!(error.hint().is_none());
        assert!(!error.to_string().contains("no reply within"));
        let old = account_key(b"HINT0003");
        note_session_end(old, Instant::now() - SESSION_LINGER, true);
        assert!(previous_session(&old).is_none());
    }

    #[test]
    fn session_mark_records_whether_the_sign_out_was_clean() {
        let account = account_key(b"MARK0001");
        let outcome = Arc::new(Mutex::new(Some(SignOutReport {
            quit_sent: true,
            acknowledged: true,
            ..SignOutReport::default()
        })));
        drop(SessionMark { account, outcome });
        let (ago, clean) = previous_session(&account).unwrap();
        assert!(clean && ago < Duration::from_secs(5));
        drop(SessionMark {
            account,
            outcome: Arc::new(Mutex::new(None)),
        });
        assert!(!previous_session(&account).unwrap().1);
    }

    #[test]
    fn stage_clock_measures_entered_stages_and_connect_time() {
        let mut clock = StageClock::new();
        let mut seen = Vec::new();
        clock.enter(Stage::ChatConnect, &mut |s| seen.push(s));
        clock.connected(Duration::from_millis(7));
        thread::sleep(Duration::from_millis(20));
        clock.enter(Stage::ChatRegister, &mut |s| seen.push(s));
        let timings = clock.into_timings();
        assert_eq!(seen, [Stage::ChatConnect, Stage::ChatRegister]);
        assert_eq!(timings[0].stage, Stage::ChatConnect);
        assert_eq!(timings[0].connect, Duration::from_millis(7));
        assert!(timings[0].elapsed >= Duration::from_millis(20));
        assert_eq!(timings[1].connect, Duration::ZERO);
        assert!(timings[1].started >= timings[0].elapsed);
        assert!(timings[0].to_string().starts_with("retail connection 0.0"));
    }

    fn catalog_body(records: &[&[u8]]) -> Vec<u8> {
        let mut body = vec![records.len() as u8, 0, 0, 0, 0, 0, 0, 0];
        for record in records {
            body.extend_from_slice(record);
        }
        body
    }

    /// A sign-in whose catalogs carry one handle and one linked FFXI
    /// character (index 2 in handle 0, subslot 1), selected by (4,6).
    fn fixture_login() -> (ProfileLogin, Vec<Arc<Mutex<Vec<u8>>>>) {
        use crate::pol_records::tests::{character_wire, handle_wire};
        let mut selected = [0; 124];
        selected[1] = 1; // subslot present
        selected[2] = 1; // subslot 1 -> map[0][1] -> character 2
        selected[3] = 1; // context
        selected[4..6].copy_from_slice(&1000u16.to_le_bytes());
        selected[6] = 1; // type 0, handle present
        selected[7] = 1;
        let community: Vec<u8> = (1..=28).collect();
        let handle = handle_wire(0, b"Handle");
        let character = character_wire(2, 0, 1, 0x0001_e240, 0x1c03_0201);
        let channels = vec![
            reply(0, 0, &[], false),
            reply(0, 0, &catalog_body(&[&handle]), false),
            reply(0, 0, &catalog_body(&[&character]), false),
            reply(0, 0, &selected, true),
            reply(0, 0, &community, true),
        ];
        let outputs = channels.iter().map(|c| c.output.clone()).collect();
        let ordered = Ordered::new(channels);
        let (login, _) = run_login(&|h, p| ordered.connect(h, p), 2, &mut Vec::new());
        (login.unwrap(), outputs)
    }

    fn loader_state(login: ProfileLogin) -> LoaderState {
        LoaderState {
            area: 1,
            account_id: Zeroizing::new(0x9182_7364_5546_3728),
            member_id: Zeroizing::new(*b"TEST0001"),
            member_secret: Zeroizing::new(b"synthetic".to_vec()),
            address: Zeroizing::new([0; 20]),
            chat_key: Zeroizing::new(KEY),
            iv: IV,
            greeting_clock: 123456,
            host: 2,
            connect_timeout: Duration::from_secs(1),
            io_timeout: Duration::from_secs(1),
            records: login.records,
            map: login.map,
            presence: Mutex::new(PresenceSlot {
                state: login.presence,
                valid: true,
                peer: login.peer,
                lobby: None,
            }),
            _session: None,
        }
    }

    fn decrypt_request(output: &Arc<Mutex<Vec<u8>>>) -> Zeroizing<Vec<u8>> {
        let out = output.lock().unwrap();
        let mut cipher = PolBlowfish::new(&KEY, IV);
        Zeroizing::new(cipher.stream(&out[40..], StreamReset::Boundary))
    }

    #[test]
    fn sign_in_keeps_catalog_records_and_builds_the_loader_session() {
        let (login, outputs) = fixture_login();
        assert_eq!(
            login.presence,
            PresenceState {
                handle: 0,
                primary: -1,
                context: 0,
                selection_type: 0,
                flag: 1,
                content_class: 1,
            }
        );
        // The retained-state model reproduces the lobby body that was sent.
        let sent = decrypt_request(&outputs[4]);
        let modelled = loader::plan_presence(
            &PresenceArgs::LOBBY,
            &PresenceState {
                handle: 0,
                primary: 2,
                context: 1,
                selection_type: 0,
                flag: 1,
                content_class: 1000,
            },
            false,
            &login.map,
        )
        .unwrap();
        assert_eq!(&sent[40..76], &modelled.body);

        let state = loader_state(login);
        let lobby = session::LobbySession::from_community(
            &(1..=28).collect::<Vec<u8>>().try_into().unwrap(),
            &session::SessionState::new([0; 20], 123456, KEY, 2),
        );
        let loader = state.loader_session(&lobby).unwrap();
        assert_eq!((loader.region, loader.lang), (Region::Us, Region::Us));
        assert_eq!(&loader.v_key, lobby.value());
        assert_eq!(&loader.auth_code, lobby.authcode());
        assert_eq!(loader.content_ids, [0x0001_e240]);
        let blob = loader.pol_records.as_deref().unwrap();
        assert_eq!(blob.len(), pol_records::BLOB_LEN);
        assert_eq!(&blob[..8], &pol_records::MAGIC);
        let character = &blob[pol_records::CHARACTERS_OFFSET + 2 * 0x68..][..0x68];
        assert_eq!(character, state.records.character(2).unwrap());
        assert_eq!(&character[8..12], &0x0001_e240u32.to_le_bytes());
        let handle = &blob[pol_records::HANDLES_OFFSET..][..0x28];
        assert_eq!(&handle[8..15], b"Handle\0");
        assert_eq!(handle[0x21], (2 << 1) | 1);

        let json = loader.to_contract_json();
        assert_eq!(
            crate::loader::tests::decode_base64(crate::loader::tests::field(&json, "pol_records")),
            blob
        );
        assert_eq!(
            crate::loader::tests::field(&json, "content_ids"),
            "[123456]"
        );

        let mut other = loader_state(fixture_login().0);
        other.area = 2;
        assert_eq!(
            other.loader_session(&lobby).err(),
            Some(LoaderSessionError::UnverifiedRegion(2))
        );
    }

    #[test]
    fn presence_forwards_world_entry_change_my_status_once() {
        let state = loader_state(fixture_login().0);
        let world = PresenceArgs {
            handle: 0,
            primary: 2,
            context: 0,
            selection_type: -2,
            flag: -1,
            content_class: 1,
        };
        let new_value: Vec<u8> = (101..=128).collect();
        let original = session::LobbySession::from_community(&[1; 28],
            &session::SessionState::new([0; 20], 123456, KEY, 2));
        let channel = reply(0, 0x05060708, &new_value, true);
        let output = channel.output.clone();
        let mut channel = Some(channel);
        let mut targets = Vec::new();
        let outcome = state
            .presence(&world, |host, port| {
                targets.push((host.to_owned(), port));
                Ok(channel.take().unwrap())
            })
            .unwrap();
        assert_eq!(targets, [("pp002.pol.com".to_owned(), 51220)]);
        let plain = decrypt_request(&output);
        let mut body = [0u8; 36];
        body[16..26].copy_from_slice(&[0, 1, 1, 1, 1, 0, 1, 1, 0, 1]);
        let expected = profile::request(
            profile::ENTER_COMMUNITY,
            &body,
            b"TEST0001",
            b"synthetic",
            [9, 8, 7, 6],
        )
        .unwrap();
        assert_eq!(plain, expected);
        let refreshed = state.loader_session(&original).unwrap();
        match outcome {
            PresenceOutcome::Sent { session } => {
                assert_eq!(&session.value()[..], &new_value[..16]);
                assert_eq!(&refreshed.v_key, session.value());
                assert_eq!(&refreshed.auth_code, session.authcode());
            }
            PresenceOutcome::Unchanged => panic!("expected a request"),
        }
        let (now, valid) = state.presence_state();
        assert!(valid);
        assert_eq!((now.handle, now.primary, now.context), (0, 2, 0));
        // Unchanged: completes without a connection, as polcore 1001dbee.
        let again = state
            .presence(&world, |_, _| -> io::Result<Channel> {
                panic!("no I/O expected")
            })
            .unwrap();
        assert!(matches!(again, PresenceOutcome::Unchanged));
        // The reply's redirect is cached for the next request.
        let moved = PresenceArgs {
            primary: -1,
            ..world
        };
        let mut seen = String::new();
        let error = state
            .presence(&moved, |host, _| {
                seen = host.to_owned();
                Ok(reply(180, 0, &[], false))
            })
            .unwrap_err();
        assert_eq!(seen, "5.6.7.8");
        assert_eq!(error, PresenceError::Service(180));
        // A refused request leaves the retained state as it was.
        assert_eq!(state.presence_state().0.primary, 2);
        let retained = state.loader_session(&original).unwrap();
        assert_eq!(retained.v_key, refreshed.v_key);
        assert_eq!(retained.auth_code, refreshed.auth_code);
    }

    #[test]
    fn friends_refresh_uses_current_session_and_does_not_change_selection() {
        let state = loader_state(fixture_login().0);
        let before = state.presence_state();
        let mut payload = vec![0u8; 8 + catalog::CONTENT_RECORD_LEN];
        payload[..4].copy_from_slice(&1u32.to_le_bytes());
        // A genuine-shaped catalog record in the final A-list slot.
        payload[8] = 0x20;
        payload[8 + 8] = 199;
        payload[8 + 0x18..8 + 0x1d].copy_from_slice(b"Alice");
        let channel = reply(0, 0x05060708, &payload, false);
        let output = channel.output.clone();
        let mut channel = Some(channel);
        let friends = state.fetch_friends(|host, port| {
            assert_eq!((host, port), ("pp002.pol.com", 51220));
            Ok(channel.take().unwrap())
        }).unwrap();
        let plain = decrypt_request(&output);
        let expected = profile::request(profile::CONTENT_IDS, &[], b"TEST0001", b"synthetic", [9, 8, 7, 6]).unwrap();
        assert_eq!(plain, expected);
        assert_eq!(&friends.record(false, 199).unwrap()[0xa0..0xa5], b"Alice");
        assert_eq!(friends.entries().count(), 1);
        assert_eq!(state.presence_state(), before);
        let mut redirected = false;
        assert!(state.fetch_friends(|host, port| {
            redirected = host == "5.6.7.8" && port == 51220;
            Ok(reply(180, 0, &[], false))
        }).is_err());
        assert!(redirected);
        assert_eq!(state.presence_state(), before);
    }

    #[test]
    fn presence_arguments_are_validated_before_any_connection() {
        let state = loader_state(fixture_login().0);
        let bad = PresenceArgs {
            context: 7,
            ..PresenceArgs::LOBBY
        };
        let error = state
            .presence(&bad, |_, _| -> io::Result<Channel> {
                panic!("no I/O expected")
            })
            .unwrap_err();
        assert_eq!(error, PresenceError::Argument(-7185));
    }
}
