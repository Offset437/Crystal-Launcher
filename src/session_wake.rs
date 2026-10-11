//! A notification only: command files still authenticate the exact worker and request.
//! Missing notifications retain the supervisor's existing timeout fallback.

use super::Identity;
use std::io;

pub(crate) struct CommandWake {
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn name(worker: Identity) -> Vec<u16> {
    format!("Local\\Crystal-command-{}-{:016x}", worker.pid, worker.created)
        .encode_utf16().chain(Some(0)).collect()
}

impl CommandWake {
    #[cfg(all(test, windows))]
    pub(crate) fn test_listener() -> (Self, impl Fn() + Send + 'static) {
        let worker = Identity { pid: std::process::id(), created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 };
        (Self::listen(worker).unwrap(), move || signal(worker))
    }

    pub(super) fn listen(worker: Identity) -> io::Result<Self> {
        #[cfg(windows)]
        { Self::listen_named(&name(worker)) }
        #[cfg(not(windows))]
        { let _ = worker; Err(io::ErrorKind::Unsupported.into()) }
    }

    #[cfg(windows)]
    fn listen_named(name: &[u16]) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::Threading::CreateEventW;
        // Auto-reset notifications coalesce; the durable files are read on every wake.
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if handle.is_null() { return Err(io::Error::last_os_error()); }
        Ok(Self { handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle as _) } })
    }

    #[cfg(windows)]
    pub(crate) fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.handle.as_raw_handle() as _
    }
}

pub(super) fn signal(worker: Identity) {
    #[cfg(windows)]
    signal_named(&name(worker));
    #[cfg(not(windows))]
    let _ = worker;
}

#[cfg(windows)]
fn signal_named(name: &[u16]) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !handle.is_null() {
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        unsafe { SetEvent(handle.as_raw_handle() as _); }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::{Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};

    #[test]
    fn notifications_coalesce_and_cannot_wake_another_worker_lifetime() {
        let worker = Identity { pid: std::process::id(), created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 };
        let listener = CommandWake::listen(worker).unwrap();
        signal(Identity { created: worker.created + 1, ..worker });
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_TIMEOUT);
        signal(worker);
        signal(worker);
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_OBJECT_0);
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_TIMEOUT);
        drop(listener);
        signal(worker); // Old builds and ended workers are harmless; the command file remains.
    }

}
