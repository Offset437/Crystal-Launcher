//! Windower saves settings.xml after the game window appears, well after the
//! loader hello. Hold its existing launch turn until that parent process exits
//! so a second save or fresh profile read cannot overlap it.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        LibraryLoader::{GetModuleFileNameW, GetModuleHandleW},
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
            QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        },
    },
};

fn created(process: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    let mut start = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = start;
    let mut kernel = start;
    let mut user = start;
    let ok = unsafe { GetProcessTimes(process, &mut start, &mut exit, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(start.dwHighDateTime) << 32) | u64::from(start.dwLowDateTime))
}

fn same_folder(first: &Path, second: &Path) -> bool {
    match (fs::canonicalize(first), fs::canonicalize(second)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

fn windower_parent() -> Result<Option<OwnedHandle>, String> {
    let name: Vec<u16> = "Hook.dll\0".encode_utf16().collect();
    let hook = unsafe { GetModuleHandleW(name.as_ptr()) };
    if hook.is_null() {
        return Ok(None);
    }
    let mut hook_name = vec![0u16; 32768];
    let length = unsafe { GetModuleFileNameW(hook, hook_name.as_mut_ptr(), hook_name.len() as u32) }
        as usize;
    if length == 0 || length == hook_name.len() {
        return Err("Could not locate Windower before saving profiles.".into());
    }
    let hook_path = PathBuf::from(String::from_utf16_lossy(&hook_name[..length]));
    let Some(folder) = hook_path.parent() else {
        return Err("Could not locate Windower's folder.".into());
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("Could not identify Windower's launcher process.".into());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut next = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    let own = unsafe { GetCurrentProcessId() };
    let mut parent = None;
    while next != 0 {
        if entry.th32ProcessID == own {
            parent = Some(entry.th32ParentProcessID);
            break;
        }
        next = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    let Some(pid) = parent else {
        return Err("Could not identify the game host's parent process.".into());
    };
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(None)
        } else {
            Err(format!(
                "Could not monitor Windower's profile save: {error}"
            ))
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut image = vec![0u16; 32768];
    let mut size = image.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle(), 0, image.as_mut_ptr(), &mut size)
    } == 0
    {
        if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
            return Ok(None);
        }
        return Err("Could not verify Windower's launcher process.".into());
    }
    let image = PathBuf::from(String::from_utf16_lossy(&image[..size as usize]));
    if !image
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Windower.exe"))
        || !image.parent().is_some_and(|path| same_folder(path, folder))
    {
        return Ok(None);
    }
    let (Some(parent_created), Some(host_created)) = (
        created(handle.as_raw_handle()),
        created(unsafe { GetCurrentProcess() }),
    ) else {
        return Err("Could not verify Windower's process identity.".into());
    };
    if parent_created > host_created {
        return Ok(None);
    } // reused parent PID
    Ok(Some(handle))
}

fn turn_path() -> Result<PathBuf, String> {
    // Same path as session_worker::windower_turn, including isolated test roots.
    let root = std::env::var_os("CRYSTAL_DATA_ROOT")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .ok_or("Could not locate the Windower launch lock directory.")?;
    Ok(PathBuf::from(root).join("Crystal Launcher/locks/windower.lock"))
}

fn open_turn(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

fn acquire(
    path: &Path,
    parent: &OwnedHandle,
    pump: &mut dyn FnMut() -> bool,
) -> Result<Option<File>, String> {
    fs::create_dir_all(
        path.parent()
            .ok_or("Could not locate the Windower launch lock directory.")?,
    )
    .map_err(|e| format!("Could not protect Windower's profile save: {e}"))?;
    let until = Instant::now() + Duration::from_secs(90);
    let mut announced = false;
    loop {
        match unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => return Ok(None),
            WAIT_TIMEOUT => {}
            _ => return Err("Could not monitor Windower's profile save.".into()),
        }
        match open_turn(path) {
            Ok(file) => return Ok(Some(file)),
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => {}
            Err(e) => return Err(format!("Could not protect Windower's profile save: {e}")),
        }
        if !announced {
            log!("stage: waiting for the previous Windower profile save");
            announced = true;
        }
        if Instant::now() >= until {
            return Err("Windower did not finish its previous profile save.".into());
        }
        if !pump() {
            return Err("The session ended while waiting for Windower's profile save.".into());
        }
    }
}

pub(crate) fn coordinate(pump: &mut dyn FnMut() -> bool) -> Result<(), String> {
    let Some(parent) = windower_parent()? else {
        return Ok(());
    };
    let Some(turn) = acquire(&turn_path()?, &parent, pump)? else {
        return Ok(());
    };
    retain_turn(parent, turn)
        .map_err(|_| "Could not monitor Windower's final profile save.".to_owned())?;
    Ok(())
}

fn retain_turn(parent: OwnedHandle, turn: File) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("windower-profile-save".into())
        .spawn(move || {
            let result = unsafe { WaitForSingleObject(parent.as_raw_handle(), u32::MAX) };
            if result == WAIT_OBJECT_0 {
                log!("stage: Windower profile save finished; launch turn released");
            } else {
                log!("error: Windower profile-save process wait failed");
            }
            drop(turn);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::windows::{io::IntoRawHandle, process::CommandExt},
        process::{Command, Stdio},
    };

    #[test]
    fn turn_excludes_a_second_launch_until_the_tool_process_exits() {
        let root = std::env::temp_dir().join(format!(
            "crystal-windower-save-{}-{}",
            std::process::id(),
            unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("windower.lock");
        let first = open_turn(&path).unwrap();
        assert_eq!(open_turn(&path).unwrap_err().raw_os_error(), Some(32));
        let mut child = Command::new("sort.exe")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let child = unsafe { OwnedHandle::from_raw_handle(child.into_raw_handle()) };
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        let save = retain_turn(child, first).unwrap();
        assert_eq!(open_turn(&path).unwrap_err().raw_os_error(), Some(32));
        drop(input); // Windower stand-in finishes its final save and exits.
        save.join().unwrap();
        assert!(open_turn(&path).is_ok());
    }

    #[test]
    fn blocked_start_keeps_pumping_and_honors_cancellation() {
        let root =
            std::env::temp_dir().join(format!("crystal-windower-cancel-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("windower.lock");
        let _first = open_turn(&path).unwrap();
        let live = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, GetCurrentProcessId()) };
        assert!(!live.is_null());
        let live = unsafe { OwnedHandle::from_raw_handle(live) };
        let mut calls = 0;
        let error = acquire(&path, &live, &mut || {
            calls += 1;
            false
        })
        .unwrap_err();
        assert!(error.contains("session ended"));
        assert_eq!(calls, 1);
    }
}
