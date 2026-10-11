//! Read-only, per-process lobby diagnostics for the verified installed game.
//! Only state codes are sampled. No packets, tickets, names or account IDs.

use std::{ffi::c_void, sync::mpsc, thread, time::{Duration, Instant}};
use windows_sys::Win32::{
    System::{Diagnostics::Debug::ReadProcessMemory, LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcess},
};
use crate::log;

pub const TIMESTAMP: u32 = 0x6A99_5428;
pub const IMAGE_SIZE: u32 = 0x00C1_5000;
pub const LOBBY_PTR_RVA: usize = 0x004D_F770;
const LOBBY_GET_ERROR_RVA: usize = 0x000E_F920;
const MAIN_PTR_RVA: usize = 0x0036_01BC;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

fn read<T: Copy>(address: usize) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes = 0;
    // SAFETY: reads our own address space. Windows rejects inaccessible or
    // partially mapped ranges; never dereference game-owned pointers in Rust.
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const c_void,
        value.as_mut_ptr().cast(), std::mem::size_of::<T>(), &mut bytes) };
    (ok != 0 && bytes == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

fn valid_layout(base: usize, timestamp: u32, size: u32, getter: [u8; 9]) -> bool {
    timestamp == TIMESTAMP && size == IMAGE_SIZE
        // Installed 100ef920: mov eax,[104df770]; test eax,eax; je ...
        && getter[0] == 0xA1 && getter[5..9] == [0x85, 0xC0, 0x74, 0x1E]
        && base.checked_add(LOBBY_PTR_RVA).and_then(|v| u32::try_from(v).ok())
            == Some(u32::from_le_bytes(getter[1..5].try_into().unwrap()))
}

/// Timestamp, size and relocated instruction operand must all match. The
/// earlier fallback used 104df7b0 from another build despite this timestamp.
pub(crate) fn verified_image() -> Result<Option<usize>, &'static str> {
    let name: Vec<u16> = "FFXiMain.dll\0".encode_utf16().collect();
    let base = unsafe { GetModuleHandleW(name.as_ptr()) } as usize;
    if base == 0 { return Ok(None); }
    let nt = base.checked_add(read::<u32>(base + 0x3C).ok_or("unreadable game header")? as usize)
        .ok_or("game header overflows")?;
    let timestamp = read::<u32>(nt + 8).ok_or("unreadable game timestamp")?;
    let size = read::<u32>(nt + 24 + 56).ok_or("unreadable game size")?;
    if timestamp != TIMESTAMP || size != IMAGE_SIZE { return Err("unsupported game layout"); }
    let getter = read::<[u8; 9]>(base + LOBBY_GET_ERROR_RVA).ok_or("unreadable lobby getter")?;
    if !valid_layout(base, timestamp, size, getter) { return Err("lobby getter signature differs"); }
    Ok(Some(base))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    operation: u32,
    socket_state: i32,
    socket_error: i32,
    reply_state: i32,
    lobby_error: i32,
    protocol_error: i32,
}

fn snapshot(base: usize) -> Option<Snapshot> {
    let lobby = read::<u32>(base + LOBBY_PTR_RVA)? as usize;
    if lobby == 0 { return None; }
    let tcp = read::<u32>(lobby + 0x13690)? as usize;
    if tcp == 0 { return None; }
    let operation = read::<u32>(tcp + 0x4F1A8)?;
    let context = read::<u32>(tcp + 0x4F1AC)? as usize;
    if operation > 21 || context == 0 { return None; }
    Some(Snapshot {
        operation,
        socket_state: read(tcp + 0x2F0F8)?,
        socket_error: read(tcp + 0x2F0FC)?,
        reply_state: read(lobby + 0x13698)?,
        lobby_error: read(lobby + 0x136AC)?,
        protocol_error: read(context + 0x13C)?,
    })
}

fn operation_name(operation: u32) -> &'static str {
    match operation {
        0 => "idle",
        2 => "connect",
        3 => "lobby login (0x26)",
        4 => "character list (0x1f)",
        5 => "character selection (0x07)",
        8 => "close connection",
        _ => "other",
    }
}

fn network_step(base: usize) -> Option<(u32, u32)> {
    let main = read::<u32>(base + MAIN_PTR_RVA)? as usize;
    if main == 0 { return None; }
    // Installed 10016580 passes this pointer directly to 100daf30,
    // which calls 100da750 with the same this pointer (state at +0x88).
    Some((read(main + 0x88)?, read(main + 0x0C)?))
}

/// Starts before GameStart and joins before game interfaces are released.
/// Sampling never changes or calls into the game. Four small samples/second;
/// logs only changes and a single 10s stall notice for an outstanding request.
pub(crate) struct Observer {
    stop: mpsc::Sender<()>,
    worker: thread::JoinHandle<()>,
}

impl Observer {
    pub(crate) fn start() -> Option<Self> {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new().name("crystal-lobby-observer".into()).spawn(move || {
            let mut base = None;
            let mut last = None;
            let mut changed = Instant::now();
            let mut stalled = false;
            loop {
                if receiver.recv_timeout(SAMPLE_INTERVAL) != Err(mpsc::RecvTimeoutError::Timeout) { break; }
                if base.is_none() {
                    match verified_image() {
                        Ok(Some(image)) => { base = Some(image); log!("lobby: verified per-process state diagnostics enabled"); }
                        Ok(None) => continue,
                        Err(reason) => { log!("lobby: state diagnostics unavailable: {reason}"); break; }
                    }
                }
                let image = base.unwrap();
                let Some(current) = snapshot(image) else { continue; };
                if last != Some(current) {
                    log!("lobby: {} op={} socket_state={} socket_error={} reply_state={} lobby_error={} protocol_error={} network_step={:?}",
                        operation_name(current.operation), current.operation, current.socket_state, current.socket_error,
                        current.reply_state, current.lobby_error, current.protocol_error, network_step(image));
                    last = Some(current); changed = Instant::now(); stalled = false;
                } else if matches!(current.operation, 2..=5) && !stalled && changed.elapsed() >= Duration::from_secs(10) {
                    log!("lobby: still waiting for {} after {:.1}s; network_step={:?}", operation_name(current.operation), changed.elapsed().as_secs_f32(), network_step(image));
                    stalled = true;
                }
            }
        }).ok()?;
        Some(Self { stop, worker })
    }

    pub(crate) fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_rejects_the_old_pointer_and_supports_relocation() {
        let getter = |base: u32, offset: u32| {
            let mut bytes = [0xA1, 0, 0, 0, 0, 0x85, 0xC0, 0x74, 0x1E];
            bytes[1..5].copy_from_slice(&(base + offset).to_le_bytes()); bytes
        };
        for base in [0x1000_0000, 0x1800_0000] {
            assert!(valid_layout(base as usize, TIMESTAMP, IMAGE_SIZE, getter(base, LOBBY_PTR_RVA as u32)));
            assert!(!valid_layout(base as usize, TIMESTAMP, IMAGE_SIZE, getter(base, 0x004D_F7B0)));
            assert!(!valid_layout(base as usize, TIMESTAMP+1, IMAGE_SIZE, getter(base, LOBBY_PTR_RVA as u32)));
        }
        assert!(!valid_layout(0x1000_0000, TIMESTAMP, IMAGE_SIZE, [0;9]));
    }
    #[test]
    fn unmapped_state_reads_fail_cleanly() { assert_eq!(read::<u32>(0), None); }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn network_state_uses_the_direct_manager_pointer() {
        let mut manager = [0u32; 0x90 / 4];
        manager[0x88 / 4] = 9;
        manager[0x0C / 4] = 4;
        let root = manager.as_ptr() as u32;
        let base = (&root as *const u32 as usize).checked_sub(MAIN_PTR_RVA).unwrap();
        assert_eq!(network_step(base), Some((9, 4)));
    }
}
