//! Named-pipe transport: the loader's client end, plus a server end used by the
//! offline harness and the tests to stand in for the launcher.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use zeroize::{Zeroize, Zeroizing};

use crate::proto::MAX_LINE;

#[derive(Debug)]
pub enum PipeError {
    Io(&'static str, io::Error),
    Timeout(&'static str),
    Closed,
    LineTooLong,
    WrongServer { expected: u32, actual: u32 },
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::Io(what, e) => write!(f, "{what}: {e}"),
            PipeError::Timeout(what) => write!(f, "timed out {what}"),
            PipeError::Closed => write!(f, "the launcher closed the pipe"),
            PipeError::LineTooLong => write!(f, "line longer than {MAX_LINE} bytes"),
            PipeError::WrongServer { expected, actual } => {
                write!(f, "pipe server is process {actual}, not the launcher {expected}")
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(what: &'static str) -> PipeError {
    PipeError::Io(what, io::Error::last_os_error())
}

pub struct Pipe {
    h: HANDLE,
}

// SAFETY: a pipe HANDLE may be used from any thread.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.h) };
    }
}

impl Pipe {
    /// Connects to `path` (`\\.\pipe\...`), retrying while the server is not up
    /// yet or busy, and checks that the server is `expected_server_pid`.
    pub fn connect(path: &str, expected_server_pid: u32, timeout: Duration) -> Result<Pipe, PipeError> {
        let name = wide(path);
        let deadline = Instant::now() + timeout;
        let pipe = loop {
            // SECURITY_IDENTIFICATION: the server may identify us, never impersonate us.
            // SAFETY: name is NUL-terminated; other pointers are null/valid.
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                break Pipe { h };
            }
            // SAFETY: plain FFI.
            let err = unsafe { GetLastError() };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PipeError::Timeout("connecting to the launcher pipe"));
            }
            match err {
                // SAFETY: name is NUL-terminated.
                ERROR_PIPE_BUSY => unsafe {
                    WaitNamedPipeW(name.as_ptr(), left.as_millis().min(5000) as u32);
                },
                ERROR_FILE_NOT_FOUND => std::thread::sleep(Duration::from_millis(50)),
                _ => return Err(PipeError::Io("opening the launcher pipe", io::Error::from_raw_os_error(err as i32))),
            }
        };
        let actual = pipe.server_pid()?;
        if actual != expected_server_pid {
            return Err(PipeError::WrongServer { expected: expected_server_pid, actual });
        }
        Ok(pipe)
    }

    /// Creates a single-instance, local-only server end (harness and tests).
    pub fn create_server(path: &str) -> Result<Pipe, PipeError> {
        let name = wide(path);
        // SAFETY: name is NUL-terminated; default security (the test runs as one user).
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error("creating the pipe"));
        }
        Ok(Pipe { h })
    }

    /// Server end: blocks until a client connects.
    pub fn accept(&self) -> Result<(), PipeError> {
        // SAFETY: synchronous handle, no OVERLAPPED.
        let ok = unsafe { ConnectNamedPipe(self.h, std::ptr::null_mut()) };
        // SAFETY: plain FFI.
        if ok == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            return Err(last_error("waiting for the loader"));
        }
        Ok(())
    }

    pub fn server_pid(&self) -> Result<u32, PipeError> {
        let mut pid = 0;
        // SAFETY: pid is a valid out pointer.
        if unsafe { GetNamedPipeServerProcessId(self.h, &mut pid) } == 0 {
            return Err(last_error("reading the pipe server's process id"));
        }
        Ok(pid)
    }

    pub fn client_pid(&self) -> Result<u32, PipeError> {
        let mut pid = 0;
        // SAFETY: pid is a valid out pointer.
        if unsafe { GetNamedPipeClientProcessId(self.h, &mut pid) } == 0 {
            return Err(last_error("reading the pipe client's process id"));
        }
        Ok(pid)
    }

    pub fn write_all(&self, mut data: &[u8]) -> Result<(), PipeError> {
        while !data.is_empty() {
            let mut n = 0;
            let chunk = data.len().min(64 * 1024) as u32;
            // SAFETY: data is valid for `chunk` bytes.
            if unsafe { WriteFile(self.h, data.as_ptr(), chunk, &mut n, std::ptr::null_mut()) } == 0 {
                // SAFETY: plain FFI.
                return Err(if unsafe { GetLastError() } == ERROR_BROKEN_PIPE { PipeError::Closed } else { last_error("writing to the pipe") });
            }
            data = &data[n as usize..];
        }
        Ok(())
    }

    /// Bytes waiting to be read, without blocking. `Err(Closed)` once the other end is gone.
    fn available(&self) -> Result<u32, PipeError> {
        let mut avail = 0;
        // SAFETY: only the "bytes available" out pointer is used.
        let ok = unsafe { PeekNamedPipe(self.h, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) };
        if ok == 0 {
            // SAFETY: plain FFI.
            return Err(if unsafe { GetLastError() } == ERROR_BROKEN_PIPE { PipeError::Closed } else { last_error("polling the pipe") });
        }
        Ok(avail)
    }

    fn read_some(&self, out: &mut [u8]) -> Result<usize, PipeError> {
        let mut n = 0;
        // SAFETY: out is valid for out.len() bytes.
        if unsafe { ReadFile(self.h, out.as_mut_ptr(), out.len() as u32, &mut n, std::ptr::null_mut()) } == 0 {
            // SAFETY: plain FFI.
            return Err(if unsafe { GetLastError() } == ERROR_BROKEN_PIPE { PipeError::Closed } else { last_error("reading from the pipe") });
        }
        if n == 0 {
            return Err(PipeError::Closed);
        }
        Ok(n as usize)
    }
}

/// Splits a pipe's byte stream into lines. The buffer is allocated once at its
/// full size (so it never reallocates and leaves copies behind) and is zeroized
/// as lines are consumed and on drop.
pub struct LineReader {
    buf: Zeroizing<Vec<u8>>,
    len: usize,
    consumed: usize,
}

impl Default for LineReader {
    fn default() -> Self {
        LineReader { buf: Zeroizing::new(vec![0u8; MAX_LINE + 1]), len: 0, consumed: 0 }
    }
}

impl LineReader {
    fn discard_consumed(&mut self) {
        if self.consumed > 0 {
            self.buf.copy_within(self.consumed..self.len, 0);
            let new_len = self.len - self.consumed;
            self.buf[new_len..self.len].zeroize();
            self.len = new_len;
            self.consumed = 0;
        }
    }

    /// Parses one owned message, then immediately wipes its source bytes while
    /// retaining any following lines already read from the byte-stream pipe.
    pub fn read_parsed<T>(&mut self, pipe: &Pipe, timeout: Option<Duration>, parse: impl FnOnce(&[u8]) -> T) -> Result<T, PipeError> {
        let parsed = parse(self.read_line(pipe, timeout)?);
        self.discard_consumed();
        Ok(parsed)
    }

    /// Returns the next line without its `\n`. `timeout: None` waits forever.
    pub fn read_line(&mut self, pipe: &Pipe, timeout: Option<Duration>) -> Result<&[u8], PipeError> {
        self.discard_consumed();
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(pos) = self.buf[..self.len].iter().position(|&b| b == b'\n') {
                self.consumed = pos + 1;
                return Ok(&self.buf[..pos]);
            }
            if self.len >= self.buf.len() {
                return Err(PipeError::LineTooLong);
            }
            let avail = pipe.available()? as usize;
            if avail == 0 {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(PipeError::Timeout("waiting for a line from the pipe"));
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let room = self.buf.len() - self.len;
            let n = pipe.read_some(&mut self.buf[self.len..self.len + avail.min(room)])?;
            self.len += n;
        }
    }

    /// Zeroizes everything read so far.
    pub fn wipe(&mut self) {
        self.buf.zeroize();
        self.buf.resize(MAX_LINE + 1, 0);
        self.len = 0;
        self.consumed = 0;
    }
}

/// A fresh pipe path with 128 random bits, in the contract's naming scheme.
pub fn random_pipe_path() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hex = String::new();
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i ^ u64::from(std::process::id()));
        h.write_u128(Instant::now().elapsed().as_nanos());
        hex.push_str(&format!("{:016x}", h.finish()));
    }
    format!(r"\\.\pipe\crystal-loader-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coalesced_startup_handoff(parked: bool) {
        use crate::{b64, proto::{self, FirstLine, ServiceEvent}};
        use std::sync::mpsc;

        let credentials = format!("\"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":null,\"profile_key\":\"{}\"}}",
            b64::encode(b"ABCD1234"), b64::encode(b"fake password"), b64::encode(b"fakeuser"), b64::encode(&[0x51; 20]), "a".repeat(40));
        let startup = if parked {
            format!("{{\"type\":\"park\",\"v\":1,\"region\":\"US\",\"lang\":\"US\"}}\n{{\"type\":\"credentials\",\"start_game\":false,{credentials}}}\n")
        } else {
            format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":false,{credentials}}}\n")
        };
        let commands = concat!("{\"type\":\"native_sign_out\"}\n", "{\"type\":\"native_play_pending\"}\n",
            "{\"type\":\"controller_settings\",\"request_id\":42,\"deadline_ms\":123456,\"settings\":{\"padmode000\":\"1,1,0,0,1,1\",\"padsin000\":\"1,2,3\"}}\n");
        let payload = format!("{startup}{commands}");
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let (sent, written) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            server.accept().unwrap();
            server.write_all(payload.as_bytes()).unwrap(); // one byte-stream write
            sent.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
        written.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut reader = LineReader::default();
        let first = reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_first_line).unwrap().unwrap();
        let session = match first {
            FirstLine::Session(session) => { assert!(!parked); session }
            FirstLine::Park(park) => {
                assert!(parked);
                let (credentials, _) = crate::park::await_credentials(std::process::id(), Instant::now(), Duration::ZERO,
                    &mut |bytes| client.write_all(bytes),
                    &mut || reader.read_parsed(&client, None, proto::parse_credentials)).unwrap();
                park.into_session(credentials)
            }
        };
        assert!(session.native_auth.is_some() && !session.start_game);
        // The same kernel read must have included the commands, and no bytes
        // from the consumed credentials may remain in the allocated buffer.
        assert_eq!(&reader.buf[..reader.len], commands.as_bytes());
        assert!(reader.buf[reader.len..].iter().all(|&byte| byte == 0));
        assert_eq!(reader.consumed, 0);
        drop(session);

        // Continue the real parser on the reader thread with no further writes.
        std::thread::spawn(move || {
            let next = |reader: &mut LineReader| reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_service_event).unwrap().unwrap().unwrap();
            assert!(matches!(next(&mut reader), ServiceEvent::NativeSignOut));
            assert!(matches!(next(&mut reader), ServiceEvent::NativePlayPending));
            assert!(matches!(next(&mut reader), ServiceEvent::ControllerSettings { id: 42, deadline_ms: 123456, settings } if settings.len() == 2));
            assert_eq!((reader.len, reader.consumed), (0, 0));
            assert!(reader.buf.iter().all(|&byte| byte == 0));
        }).join().unwrap();
        release.send(()).unwrap();
        writer.join().unwrap();
    }

    #[test]
    fn cold_session_preserves_coalesced_commands_and_wipes_consumed_credentials() {
        coalesced_startup_handoff(false);
    }

    #[test]
    fn parked_credentials_preserve_coalesced_commands_and_wipe_consumed_credentials() {
        coalesced_startup_handoff(true);
    }

    #[test]
    fn lines_cross_reads_and_server_pid_is_checked() {
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let t = std::thread::spawn(move || {
            server.accept().unwrap();
            assert_eq!(server.client_pid().unwrap(), std::process::id());
            server.write_all(b"one\ntw").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            server.write_all(b"o\nthree\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
        let mut r = LineReader::default();
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"one");
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"two");
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"three");
        t.join().unwrap();
        assert!(matches!(r.read_line(&client, Some(Duration::from_secs(1))), Err(PipeError::Closed)));
    }

    #[test]
    fn wrong_server_pid_is_refused() {
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let t = std::thread::spawn(move || {
            let _ = server.accept();
            std::thread::sleep(Duration::from_millis(100));
        });
        let err = Pipe::connect(&path, std::process::id().wrapping_add(1), Duration::from_secs(5)).err().unwrap();
        assert!(matches!(err, PipeError::WrongServer { .. }));
        t.join().unwrap();
    }

    #[test]
    fn missing_pipe_times_out() {
        let err = Pipe::connect(&random_pipe_path(), 1, Duration::from_millis(200)).err().unwrap();
        assert!(matches!(err, PipeError::Timeout(_)));
    }
}
