//! Maintains an already registered chat connection; never opens a connection.
//!
//! Installed polcore 10018030/10017200 replies to PING with literal `PONG a`.
//! 10015c30 sends `PING a` after 19 elapsed seconds when no ping is outstanding,
//! and fails an unanswered ping after 400 seconds. Both stream directions reset
//! at every complete frame (10015eb7 and 1001389d), allowing independent ciphers.
//! Authenticated friend NOTICE updates are retained in a bounded feed, including
//! updates buffered during registration. Other application messages are ignored.
//!
//! Sign-out follows polcore's graceful chat close, traced statically from the
//! installed build 73b1864b (unpacked image, preferred VAs):
//! - API slots 135/136 (`10014650`/`10014700`, table `1006fbe8`) and the
//!   wrappers at slots 1246/1247 (`1000d350`/`1000d3c0`) move the chat context
//!   to state 0x14 instead of closing the socket at once.
//! - State 0x14 sends command 5 with no arguments (`1001479d`). Command 5 is
//!   `QUIT` in the name table at `10073f38`, and its formatter `10016d30`
//!   writes the bare word `QUIT`. It goes through the normal encrypted line
//!   framing, like `PONG a`.
//! - The context then waits. The `ERROR` handler `10018180` (incoming command
//!   0x1f) moves a quitting context to state 0x16, which closes the socket
//!   (`10010b10` -> `closesocket`, ws2_32 ordinal 3). polcore sets no
//!   SO_LINGER (its only `setsockopt` calls set SO_SNDBUF/SO_RCVBUF), so the
//!   close is an orderly FIN, not an abort.
//!
//! FFXiMain never calls these slots; POL itself does when it signs out. No
//! profile-server "offline" status transaction was found on that path. The
//! sequence is statically derived and not yet confirmed against the live
//! service; whatever the server does, the connection is closed with a FIN
//! within the caller's bound.

use crate::{
    chat_transport::{HandshakeError, Registered},
    crypto::{PolBlowfish, StreamReset},
    identity,
};
use std::{
    io::{self, Write},
    net::{Shutdown, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_DEADLINE: Duration = Duration::from_secs(400);
// Local bound on a stalled write, not a claimed service protocol requirement.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The default bound for [`Keepalive::sign_out_within`]'s callers.
pub const SIGN_OUT_TIMEOUT: Duration = Duration::from_secs(3);
/// The bound used when a live keepalive is dropped without an explicit sign-out.
pub const DROP_SIGN_OUT_TIMEOUT: Duration = Duration::from_secs(2);
/// After the server's ERROR, how long to wait for its FIN before closing ours.
const ACK_GRACE: Duration = Duration::from_millis(500);
/// Extra time, past the sign-out bound, for workers to exit once unblocked.
const JOIN_GRACE: Duration = Duration::from_millis(500);
const CANCEL_RETRY: Duration = Duration::from_millis(20);
/// polcore's QUIT formatter output (`10016d30`).
const QUIT: &[u8] = b"QUIT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepaliveError {
    Io(io::ErrorKind),
    InvalidFrame,
    ServerError,
    HeartbeatExpired,
    WorkerFailed,
}
impl std::fmt::Display for KeepaliveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "chat connection error: {kind:?}"),
            Self::InvalidFrame => f.write_str("invalid registered chat frame"),
            Self::ServerError => f.write_str("chat server ended the connection"),
            Self::HeartbeatExpired => f.write_str("chat heartbeat was not acknowledged"),
            Self::WorkerFailed => f.write_str("chat maintenance worker stopped unexpectedly"),
        }
    }
}
impl std::error::Error for KeepaliveError {}
impl From<io::Error> for KeepaliveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// What a sign-out achieved. It never fails: every field is best effort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignOutReport {
    /// The chat connection had already failed or been closed; nothing was sent.
    pub already_closed: bool,
    /// The encrypted `QUIT` line was written.
    pub quit_sent: bool,
    /// The server answered `QUIT` with `ERROR`, polcore's cue to close.
    pub acknowledged: bool,
    /// The server closed its side before ours was torn down: an orderly
    /// two-way close with nothing left unread.
    pub peer_closed: bool,
    /// Both maintenance threads exited within the bound (none were left to
    /// finish on their own).
    pub joined: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Ping,
    Pong,
    Quit,
    Stop,
}

#[derive(Default)]
struct Progress {
    quit_sent: bool,
    writer_done: bool,
    acknowledged: bool,
    peer_closed: bool,
    reader_done: bool,
    /// Set once the owner will no longer cancel the reader's I/O, so the
    /// reader may close its socket handle.
    released: bool,
}

struct Shared {
    stopping: AtomicBool,
    quitting: AtomicBool,
    failure: Mutex<Option<KeepaliveError>>,
    progress: Mutex<Progress>,
    changed: Condvar,
    friends: Mutex<crate::friend_presence::Feed>,
}
impl Shared {
    fn new() -> Self {
        Self {
            stopping: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            failure: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            changed: Condvar::new(),
            friends: Mutex::new(Default::default()),
        }
    }
    fn fail(&self, error: KeepaliveError) {
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_none() {
            *failure = Some(error);
        }
    }
    fn result(&self) -> Result<(), KeepaliveError> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map_or(Ok(()), Err)
    }
    fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn update(&self, change: impl FnOnce(&mut Progress)) {
        change(&mut self.progress());
        self.changed.notify_all();
    }
    /// Wait until `done` holds or `deadline` passes; returns whether it held.
    fn wait_until(&self, deadline: Instant, done: impl Fn(&Progress) -> bool) -> bool {
        let mut progress = self.progress();
        loop {
            if done(&progress) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            progress = self
                .changed
                .wait_timeout(progress, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Cancels the reader's blocking receive. On Windows, `shutdown` does not end
/// a `recv` blocked in another thread (verified on this machine); CancelIoEx on
/// the reader's own handle does. Elsewhere `shutdown` suffices.
struct ReaderCancel {
    #[cfg(windows)]
    raw: std::os::windows::io::RawSocket,
}
impl ReaderCancel {
    fn of(socket: &TcpStream) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            Self {
                raw: socket.as_raw_socket(),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = socket;
            Self {}
        }
    }
    /// Callers hold the progress lock with `reader_done` false, so the reader
    /// still owns (has not closed) this handle.
    fn cancel(&self) {
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            extern "system" {
                fn CancelIoEx(
                    file: *mut core::ffi::c_void,
                    overlapped: *mut core::ffi::c_void,
                ) -> i32;
            }
            // SAFETY: the handle is open (see above); a null OVERLAPPED
            // cancels every pending request on it, which only makes the
            // reader's blocking recv return an error.
            unsafe {
                CancelIoEx(self.raw as usize as *mut _, std::ptr::null_mut());
            }
        }
    }
}

/// Own this guard through profile transactions and the game child's lifetime.
/// Dropping it signs out like [`Keepalive::sign_out`], bounded by
/// [`DROP_SIGN_OUT_TIMEOUT`]. Errors contain no received text, account
/// fields, or session material.
pub struct Keepalive {
    shutdown_socket: TcpStream,
    events: SyncSender<Event>,
    shared: Arc<Shared>,
    cancel: ReaderCancel,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    outcome: Arc<Mutex<Option<SignOutReport>>>,
}
impl Keepalive {
    pub fn start(mut registered: Registered<TcpStream>) -> Result<Self, KeepaliveError> {
        // The reader never times out between bytes. Existing read_message keeps
        // partial lines locally, so retrying it after a timeout would lose bytes.
        registered.channel_ref().set_read_timeout(None)?;
        registered
            .channel_ref()
            .set_write_timeout(Some(WRITE_TIMEOUT))?;
        let shutdown_socket = registered.channel_ref().try_clone()?;
        let mut writer_socket = registered.channel_ref().try_clone()?;
        let reader_shutdown = registered.channel_ref().try_clone()?;
        let writer_shutdown = registered.channel_ref().try_clone()?;
        let cancel = ReaderCancel::of(registered.channel_ref());
        let mut cipher = PolBlowfish::new(&registered.session_key[..], registered.iv);
        let shared = Arc::new(Shared::new());
        *shared.friends.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut registered.friend_presence);
        let (events, receiver) = mpsc::sync_channel(32);
        let write_shared = Arc::clone(&shared);
        let writer = thread::Builder::new()
            .name("pol-chat-heartbeat".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    write_loop(&mut writer_socket, &mut cipher, receiver, &write_shared)
                }))
                .unwrap_or(Err(KeepaliveError::WorkerFailed));
                finish_worker(&write_shared, result, &writer_shutdown);
                write_shared.update(|p| p.writer_done = true);
            })?;
        let read_shared = Arc::clone(&shared);
        let read_events = events.clone();
        let reader = match thread::Builder::new()
            .name("pol-chat-reader".into())
            .spawn(move || {
                let mut registered = registered;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    read_loop(
                        &mut registered,
                        &read_events,
                        &read_shared,
                        &reader_shutdown,
                    )
                }))
                .unwrap_or(Err(KeepaliveError::WorkerFailed));
                finish_worker(&read_shared, result, &reader_shutdown);
                let _ = read_events.try_send(Event::Stop);
                // Keep the socket handle open until the owner can no longer
                // cancel I/O on it, so a cancellation never hits a reused handle.
                read_shared.update(|p| p.reader_done = true);
                let mut progress = read_shared.progress();
                while !progress.released {
                    progress = read_shared
                        .changed
                        .wait(progress)
                        .unwrap_or_else(|e| e.into_inner());
                }
                drop(progress);
                drop(registered);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                shared.stopping.store(true, Ordering::Release);
                let _ = shutdown_socket.shutdown(Shutdown::Both);
                let _ = events.try_send(Event::Stop);
                let _ = writer.join();
                return Err(error.into());
            }
        };
        Ok(Self {
            shutdown_socket,
            events,
            shared,
            cancel,
            reader: Some(reader),
            writer: Some(writer),
            outcome: Arc::new(Mutex::new(None)),
        })
    }

    /// Nonblocking status check; success means no failure has been observed yet.
    pub fn friend_revision(&self) -> u64 {
        self.shared.friends.lock().unwrap_or_else(|e| e.into_inner()).revision()
    }

    pub fn apply_friend_presence(&self, friends: &mut crate::friends::Friends, mask: u64) -> u64 {
        self.shared.friends.lock().unwrap_or_else(|e| e.into_inner()).apply(friends, mask)
    }

    pub fn check(&self) -> Result<(), KeepaliveError> {
        self.shared.result()
    }

    /// End the connection and wait for its workers. Signs out first, like
    /// [`Keepalive::sign_out`]; the result reports a failure observed before.
    pub fn shutdown(mut self) -> Result<(), KeepaliveError> {
        self.sign_out_inner(SIGN_OUT_TIMEOUT);
        self.shared.result()
    }

    /// Sign out of the chat session as POL does, bounded by
    /// [`SIGN_OUT_TIMEOUT`]. See [`Keepalive::sign_out_within`].
    pub fn sign_out(self) -> SignOutReport {
        self.sign_out_within(SIGN_OUT_TIMEOUT)
    }

    /// Send `QUIT`, wait up to `timeout` for the server's `ERROR` and close,
    /// then close our side with a FIN. Returns within `timeout` plus a short
    /// fixed grace for the workers to exit; a worker that still has not exited
    /// is left to finish on its own rather than block the caller.
    pub fn sign_out_within(mut self, timeout: Duration) -> SignOutReport {
        self.sign_out_inner(timeout)
    }

    /// The sign-out report, once this keepalive has signed out or been dropped.
    pub(crate) fn outcome_handle(&self) -> Arc<Mutex<Option<SignOutReport>>> {
        Arc::clone(&self.outcome)
    }

    fn sign_out_inner(&mut self, timeout: Duration) -> SignOutReport {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut report = SignOutReport::default();
        if self.reader.is_none() && self.writer.is_none() {
            // Already signed out; report the first outcome.
            return self
                .outcome
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or(SignOutReport {
                    already_closed: true,
                    ..report
                });
        }
        if self.shared.stopping.load(Ordering::Acquire) || self.shared.result().is_err() {
            report.already_closed = true;
        } else {
            // Set before QUIT is queued so the reader treats the server's
            // ERROR and close as the expected answer, not a failure.
            self.shared.quitting.store(true, Ordering::Release);
            if self.queue(Event::Quit, deadline) {
                self.shared.wait_until(deadline, |p| p.writer_done);
                report.quit_sent = self.shared.progress().quit_sent;
            }
            if report.quit_sent {
                self.shared
                    .wait_until(deadline, |p| p.reader_done || p.acknowledged);
                if self.shared.progress().acknowledged {
                    let grace = deadline.min(Instant::now() + ACK_GRACE);
                    self.shared.wait_until(grace, |p| p.reader_done);
                }
            }
        }
        report.joined = self.stop_and_join(deadline.max(Instant::now()) + JOIN_GRACE);
        {
            let progress = self.shared.progress();
            report.acknowledged = progress.acknowledged;
            report.peer_closed = progress.peer_closed;
        }
        report.elapsed = start.elapsed();
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        report
    }

    fn queue(&self, event: Event, deadline: Instant) -> bool {
        loop {
            match self.events.try_send(event) {
                Ok(()) => return true,
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
                Err(mpsc::TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Close the socket (FIN), unblock and join both workers by `deadline`.
    /// Returns whether both exited; one that did not is detached.
    fn stop_and_join(&mut self, deadline: Instant) -> bool {
        self.shared.stopping.store(true, Ordering::Release);
        let _ = self.shutdown_socket.shutdown(Shutdown::Both);
        let _ = self.events.try_send(Event::Stop);
        // Stop/atomic flag makes the writer drop its bounded receiver, which
        // releases a reader blocked trying to enqueue a heartbeat response.
        if self.reader.is_some() {
            loop {
                {
                    let progress = self.shared.progress();
                    if progress.reader_done {
                        break;
                    }
                    self.cancel.cancel();
                }
                if Instant::now() >= deadline {
                    break;
                }
                let retry = deadline.min(Instant::now() + CANCEL_RETRY);
                self.shared.wait_until(retry, |p| p.reader_done);
            }
        }
        self.shared.update(|p| p.released = true);
        self.shared
            .wait_until(deadline, |p| p.reader_done && p.writer_done);
        let (reader_done, writer_done) = {
            let progress = self.shared.progress();
            (progress.reader_done, progress.writer_done)
        };
        let mut joined = true;
        for (handle, done) in [
            (self.writer.take(), writer_done),
            (self.reader.take(), reader_done),
        ] {
            let Some(handle) = handle else { continue };
            if !done {
                // Detach: it ends by itself once its blocked call returns.
                joined = false;
            } else if handle.join().is_err() {
                self.shared.fail(KeepaliveError::WorkerFailed);
            }
        }
        joined
    }
}
impl Drop for Keepalive {
    fn drop(&mut self) {
        if self.reader.is_some() || self.writer.is_some() {
            self.sign_out_inner(DROP_SIGN_OUT_TIMEOUT);
        }
    }
}

/// A worker that ends with an error shuts the socket down so the other one
/// stops too. A clean end (stop or sign-out) leaves the close to its owner:
/// shutting down receives while the server's reply is in flight would make
/// Windows answer that reply with a reset.
fn finish_worker(shared: &Shared, result: Result<(), KeepaliveError>, socket: &TcpStream) {
    if let Err(error) = result {
        if !shared.stopping.swap(true, Ordering::AcqRel) {
            shared.fail(error);
        }
        let _ = socket.shutdown(Shutdown::Both);
    }
}

fn read_loop(
    registered: &mut Registered<TcpStream>,
    events: &SyncSender<Event>,
    shared: &Shared,
    socket: &TcpStream,
) -> Result<(), KeepaliveError> {
    loop {
        let quitting = shared.quitting.load(Ordering::Acquire);
        if shared.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let line = match registered.read_message() {
            Ok(line) => line,
            Err(error) => {
                // During sign-out or stop, any end of input is the close we asked for.
                if shared.quitting.load(Ordering::Acquire) {
                    if matches!(error, HandshakeError::Io(io::ErrorKind::UnexpectedEof)) {
                        shared.update(|p| p.peer_closed = true);
                    }
                    return Ok(());
                }
                if shared.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                return Err(match error {
                    HandshakeError::Io(kind) => KeepaliveError::Io(kind),
                    _ => KeepaliveError::InvalidFrame,
                });
            }
        };
        let event = event_from_line(&line);
        if quitting || shared.quitting.load(Ordering::Acquire) {
            if event == Err(KeepaliveError::ServerError) {
                // polcore closes on ERROR (10018180 -> state 0x16). Send our
                // FIN now and keep reading to the server's, so nothing is left
                // unread when the handle closes.
                shared.update(|p| p.acknowledged = true);
                let _ = socket.shutdown(Shutdown::Write);
            }
            continue;
        }
        if let Some(notice) = crate::friend_presence::Notice::parse(&line) {
            shared.friends.lock().unwrap_or_else(|e| e.into_inner()).push(notice);
        }
        if let Some(event) = event? {
            if events.send(event).is_err() && !shared.quitting.load(Ordering::Acquire) {
                return Err(KeepaliveError::WorkerFailed);
            }
        }
    }
}

fn event_from_line(line: &[u8]) -> Result<Option<Event>, KeepaliveError> {
    let body = identity::verified_line_body(line).map_err(|_| KeepaliveError::InvalidFrame)?;
    let mut tokens = body.split(|b| *b == b' ').filter(|token| !token.is_empty());
    let first = tokens.next().ok_or(KeepaliveError::InvalidFrame)?;
    let command = if first.starts_with(b":") {
        tokens.next().ok_or(KeepaliveError::InvalidFrame)?
    } else {
        first
    };
    if command.eq_ignore_ascii_case(b"PING") {
        Ok(Some(Event::Ping))
    } else if command.eq_ignore_ascii_case(b"PONG") {
        Ok(Some(Event::Pong))
    } else if command.eq_ignore_ascii_case(b"ERROR") {
        Err(KeepaliveError::ServerError)
    } else {
        Ok(None)
    }
}

fn write_loop(
    socket: &mut impl Write,
    cipher: &mut PolBlowfish,
    receiver: Receiver<Event>,
    shared: &Shared,
) -> Result<(), KeepaliveError> {
    let mut last_ping = Instant::now();
    let mut pending_ping = None::<Instant>;
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let wait = if let Some(sent) = pending_ping {
            PONG_DEADLINE.saturating_sub(sent.elapsed())
        } else {
            PING_INTERVAL.saturating_sub(last_ping.elapsed())
        };
        match receiver.recv_timeout(wait) {
            Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Event::Ping) => send_frame(socket, cipher, b"PONG a")?,
            Ok(Event::Pong) => pending_ping = None,
            Ok(Event::Quit) => {
                // Nothing follows QUIT on this connection.
                send_frame(socket, cipher, QUIT)?;
                shared.update(|p| p.quit_sent = true);
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        if shared.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(sent) = pending_ping {
            if sent.elapsed() >= PONG_DEADLINE {
                return Err(KeepaliveError::HeartbeatExpired);
            }
        } else if last_ping.elapsed() >= PING_INTERVAL {
            send_frame(socket, cipher, b"PING a")?;
            last_ping = Instant::now();
            pending_ping = Some(last_ping);
        }
    }
}
fn send_frame(
    socket: &mut impl Write,
    cipher: &mut PolBlowfish,
    body: &[u8],
) -> Result<(), KeepaliveError> {
    let frame = identity::frame_line(body).map_err(|_| KeepaliveError::InvalidFrame)?;
    let encrypted = Zeroizing::new(cipher.stream(&frame, StreamReset::Boundary));
    socket.write_all(&encrypted)?;
    socket.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read},
        net::TcpListener,
    };

    fn shared() -> Shared {
        Shared::new()
    }

    #[test]
    fn parses_verified_prefixed_heartbeat_and_terminal_error() {
        for (body, expected) in [
            (b"PING :server-token".as_slice(), Some(Event::Ping)),
            (b":server PONG a", Some(Event::Pong)),
            (b":server NOTICE user :opaque", None),
        ] {
            assert_eq!(
                event_from_line(&identity::frame_line(body).unwrap()),
                Ok(expected)
            );
        }
        for body in [b"ERROR :synthetic".as_slice(), b":server error :synthetic"] {
            assert_eq!(
                event_from_line(&identity::frame_line(body).unwrap()),
                Err(KeepaliveError::ServerError)
            );
        }
        let mut damaged = identity::frame_line(b"PING a").unwrap();
        // Low-byte body changes can collide: this checksum drops the low 8
        // sum bits. Change a declared high checksum digit within its alphabet.
        let checksum = damaged.len() - 6;
        damaged[checksum] = if damaged[checksum] == 63 { 64 } else { 63 };
        assert_eq!(event_from_line(&damaged), Err(KeepaliveError::InvalidFrame));
    }

    #[test]
    fn pong_body_is_fixed_and_cipher_restarts_for_each_frame() {
        let mut cipher = PolBlowfish::new(b"fixture!", (0x12345678, 0xabcdef01));
        let mut wire = Vec::new();
        send_frame(&mut wire, &mut cipher, b"PONG a").unwrap();
        let length = wire.len();
        send_frame(&mut wire, &mut cipher, b"PONG a").unwrap();
        assert_eq!(wire[..length], wire[length..]);
        let mut decoder = PolBlowfish::new(b"fixture!", (0x12345678, 0xabcdef01));
        let clear = decoder.stream(&wire[..length], StreamReset::Boundary);
        assert_eq!(identity::verified_line_body(&clear).unwrap(), b"PONG a");
    }

    #[test]
    fn queued_ping_is_answered_without_waiting_for_periodic_ping() {
        let (sender, receiver) = mpsc::sync_channel(2);
        sender.send(Event::Ping).unwrap();
        sender.send(Event::Stop).unwrap();
        let shared = shared();
        let mut cipher = PolBlowfish::new(b"fixture!", (1, 2));
        let mut wire = Vec::new();
        write_loop(&mut wire, &mut cipher, receiver, &shared).unwrap();
        let mut decoder = PolBlowfish::new(b"fixture!", (1, 2));
        let clear = decoder.stream(&wire, StreamReset::Boundary);
        assert_eq!(identity::verified_line_body(&clear).unwrap(), b"PONG a");
    }

    #[test]
    fn quit_is_the_last_frame_and_follows_queued_replies() {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender.send(Event::Ping).unwrap();
        sender.send(Event::Quit).unwrap();
        sender.send(Event::Ping).unwrap();
        let shared = shared();
        let mut cipher = PolBlowfish::new(b"fixture!", (1, 2));
        let mut wire = Vec::new();
        write_loop(&mut wire, &mut cipher, receiver, &shared).unwrap();
        assert!(shared.progress().quit_sent);
        let lines: Vec<_> = wire
            .split_inclusive(|b| *b == b'\n')
            .map(|frame| {
                let clear =
                    PolBlowfish::new(b"fixture!", (1, 2)).stream(frame, StreamReset::Boundary);
                identity::verified_line_body(&clear).unwrap().to_vec()
            })
            .collect();
        assert_eq!(lines, [b"PONG a".to_vec(), b"QUIT".to_vec()]);
    }

    #[test]
    fn shutdown_with_full_event_queue_does_not_wait_or_write() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Event::Ping).unwrap();
        assert!(matches!(
            sender.try_send(Event::Stop),
            Err(mpsc::TrySendError::Full(_))
        ));
        let shared = shared();
        shared.stopping.store(true, Ordering::Release);
        let mut cipher = PolBlowfish::new(b"fixture!", (1, 2));
        let mut wire = Vec::new();
        write_loop(&mut wire, &mut cipher, receiver, &shared).unwrap();
        assert!(wire.is_empty());
        // Exiting the writer releases a reader blocked on this bounded queue.
        assert!(matches!(sender.send(Event::Ping), Err(mpsc::SendError(_))));
    }

    #[test]
    fn heartbeat_write_failure_is_reported_without_frame_contents() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Event::Ping).unwrap();
        let shared = shared();
        let mut cipher = PolBlowfish::new(b"fixture!", (1, 2));
        assert_eq!(
            write_loop(&mut Broken, &mut cipher, receiver, &shared),
            Err(KeepaliveError::Io(io::ErrorKind::BrokenPipe))
        );
    }

    // Loopback mock chat servers below: offline, synthetic key, no credentials.
    const KEY: [u8; 8] = *b"fixture!";
    const IV: (u32, u32) = (0x0102_0304, 0x0506_0708);

    /// How the mock server answers once it has read QUIT.
    #[derive(Clone, Copy)]
    enum Reply {
        /// `ERROR :Closing Link`, then close: what an IRC-style server does.
        ErrorThenClose,
        /// Close without a reply.
        Close,
        /// Never reply and never close.
        Silent,
    }

    struct Server {
        lines: Arc<Mutex<Vec<Vec<u8>>>>,
        /// Whether the server's read loop saw our FIN (EOF) rather than a reset.
        saw_fin: Arc<Mutex<Option<bool>>>,
        hold: Arc<(Mutex<bool>, Condvar)>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            *self.hold.0.lock().unwrap() = true;
            self.hold.1.notify_all();
        }
    }

    fn keepalive_with(reply: Reply) -> (Keepalive, Server) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let saw_fin = Arc::new(Mutex::new(None));
        let hold = Arc::new((Mutex::new(false), Condvar::new()));
        let (server_lines, server_fin, server_hold) =
            (Arc::clone(&lines), Arc::clone(&saw_fin), Arc::clone(&hold));
        thread::spawn(move || {
            let mut writer = server.try_clone().unwrap();
            let mut reader = BufReader::new(server);
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) => {
                        *server_fin.lock().unwrap() = Some(true);
                        break;
                    }
                    Err(_) => {
                        *server_fin.lock().unwrap() = Some(false);
                        break;
                    }
                    Ok(_) => (),
                }
                let clear = PolBlowfish::new(&KEY, IV).stream(&raw, StreamReset::Boundary);
                let body = identity::verified_line_body(&clear).unwrap().to_vec();
                let quit = body == QUIT;
                server_lines.lock().unwrap().push(body);
                if !quit {
                    continue;
                }
                match reply {
                    Reply::ErrorThenClose => {
                        let line = identity::frame_line(b"ERROR :Closing Link: (Quit)").unwrap();
                        let wire = PolBlowfish::new(&KEY, IV).stream(&line, StreamReset::Boundary);
                        writer.write_all(&wire).unwrap();
                        let _ = writer.shutdown(Shutdown::Write);
                    }
                    Reply::Close => {
                        let _ = writer.shutdown(Shutdown::Write);
                    }
                    Reply::Silent => {
                        // Hold the connection open until the test ends.
                        let (lock, cvar) = &*server_hold;
                        let mut done = lock.lock().unwrap();
                        while !*done {
                            done = cvar.wait(done).unwrap();
                        }
                        return;
                    }
                }
            }
            // Drain anything else so the client close is observed as a FIN.
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
        });
        let keepalive = Keepalive::start(Registered::for_test(client, KEY, IV)).unwrap();
        (
            keepalive,
            Server {
                lines,
                saw_fin,
                hold,
            },
        )
    }

    fn wait_for<T>(slot: &Arc<Mutex<Option<T>>>) -> Option<T>
    where
        T: Copy,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(value) = *slot.lock().unwrap() {
                return Some(value);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn sign_out_sends_one_encrypted_quit_and_closes_after_the_servers_error() {
        let (keepalive, server) = keepalive_with(Reply::ErrorThenClose);
        let report = keepalive.sign_out_within(Duration::from_secs(3));
        assert!(report.quit_sent && report.acknowledged && report.peer_closed);
        assert!(report.joined && !report.already_closed);
        assert!(report.elapsed < Duration::from_secs(2), "{report:?}");
        assert_eq!(*server.lines.lock().unwrap(), [b"QUIT".to_vec()]);
        // Our side closed with a FIN, not a reset.
        assert_eq!(wait_for(&server.saw_fin), Some(true));
    }

    #[test]
    fn encrypted_friend_push_does_not_interrupt_ping_pong_or_sign_out() {
        use std::net::TcpListener;
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let client=TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server,_)=listener.accept().unwrap();
        let pong=Arc::new(AtomicBool::new(false));
        let server_pong=pong.clone();
        let worker=thread::spawn(move || {
            let mut reader=std::io::BufReader::new(server);
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let notice=crate::friend_presence::tests::frame(199,1,123,100,[2,1,0,0,1,0,0,0],&[],true);
            for frame in [&notice[..],&identity::frame_line(b"PING a").unwrap()[..]] {
                reader.get_mut().write_all(&PolBlowfish::new(&KEY,IV).stream(frame,StreamReset::Boundary)).unwrap();
            }
            use std::io::BufRead;
            let mut replies=Vec::new();
            loop {
                let mut line=Vec::new(); reader.read_until(b'\n',&mut line).unwrap();
                let clear=PolBlowfish::new(&KEY,IV).stream(&line,StreamReset::Boundary);
                let body=identity::verified_line_body(&clear).unwrap().to_vec();
                if body==b"PONG a" { server_pong.store(true,Ordering::Release); }
                let quit=body==b"QUIT"; replies.push(body);
                if quit {
                    let error=identity::frame_line(b"ERROR :Closing Link: (Quit)").unwrap();
                    reader.get_mut().write_all(&PolBlowfish::new(&KEY,IV).stream(&error,StreamReset::Boundary)).unwrap();
                    reader.get_mut().shutdown(Shutdown::Write).unwrap(); break;
                }
            }
            replies
        });
        let keepalive=Keepalive::start(Registered::for_test(client,KEY,IV)).unwrap();
        let deadline=Instant::now()+Duration::from_secs(3);
        while (keepalive.friend_revision()==0 || !pong.load(Ordering::Acquire)) && Instant::now()<deadline { thread::sleep(Duration::from_millis(5)); }
        assert!(pong.load(Ordering::Acquire));
        let mut friends=crate::friend_presence::tests::friends();
        assert_eq!(keepalive.apply_friend_presence(&mut friends,55),1);
        assert_eq!(friends.presence(false,199).0,Some(true));
        keepalive.check().unwrap();
        let report=keepalive.sign_out_within(Duration::from_secs(3));
        assert!(report.quit_sent && report.acknowledged && report.joined);
        assert_eq!(worker.join().unwrap(),[b"PONG a".to_vec(),b"QUIT".to_vec()]);
    }

    #[test]
    fn sign_out_accepts_a_close_without_error() {
        let (keepalive, server) = keepalive_with(Reply::Close);
        let report = keepalive.sign_out();
        assert!(report.quit_sent && !report.acknowledged && report.peer_closed);
        assert!(report.joined);
        assert!(report.elapsed < Duration::from_secs(2), "{report:?}");
        assert_eq!(wait_for(&server.saw_fin), Some(true));
    }

    #[test]
    fn sign_out_is_bounded_when_the_server_never_answers() {
        let (keepalive, server) = keepalive_with(Reply::Silent);
        let bound = Duration::from_millis(300);
        let started = Instant::now();
        let report = keepalive.sign_out_within(bound);
        let took = started.elapsed();
        assert!(report.quit_sent && !report.acknowledged && !report.peer_closed);
        // The blocked reader was unblocked (CancelIoEx on Windows) and joined.
        assert!(report.joined, "{report:?}");
        assert!(
            took < bound + JOIN_GRACE + Duration::from_millis(500),
            "{took:?}"
        );
        assert_eq!(*server.lines.lock().unwrap(), [b"QUIT".to_vec()]);
    }

    #[test]
    fn drop_signs_out_within_its_bound() {
        let (keepalive, server) = keepalive_with(Reply::Silent);
        let outcome = keepalive.outcome_handle();
        let started = Instant::now();
        drop(keepalive);
        let took = started.elapsed();
        assert!(
            took < DROP_SIGN_OUT_TIMEOUT + JOIN_GRACE + Duration::from_millis(500),
            "{took:?}"
        );
        let report = outcome.lock().unwrap().unwrap();
        assert!(report.quit_sent && report.joined);
        assert_eq!(*server.lines.lock().unwrap(), [b"QUIT".to_vec()]);

        let (keepalive, server) = keepalive_with(Reply::ErrorThenClose);
        drop(keepalive);
        assert_eq!(*server.lines.lock().unwrap(), [b"QUIT".to_vec()]);
        assert_eq!(wait_for(&server.saw_fin), Some(true));
    }

    #[test]
    fn failed_session_is_closed_without_sending_quit() {
        let (keepalive, server) = keepalive_with(Reply::ErrorThenClose);
        keepalive.shared.fail(KeepaliveError::HeartbeatExpired);
        keepalive.shared.stopping.store(true, Ordering::Release);
        let report = keepalive.sign_out_within(Duration::from_millis(300));
        assert!(report.already_closed && !report.quit_sent && report.joined);
        assert!(server.lines.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_reports_an_earlier_failure_and_signs_out_otherwise() {
        let (keepalive, server) = keepalive_with(Reply::ErrorThenClose);
        assert_eq!(keepalive.shutdown(), Ok(()));
        assert_eq!(*server.lines.lock().unwrap(), [b"QUIT".to_vec()]);
    }
}
