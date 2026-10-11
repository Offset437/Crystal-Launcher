//! Diagnostic slot tracing (`--trace-slots`, implied by `--selftest-offline`):
//! every function-table entry is wrapped in a small x86 thunk that logs the
//! first call of that slot (slot number and calling module only; never
//! arguments) and then jumps to the real entry. Registers and flags are
//! preserved, so cdecl, stdcall or register-passing entries all work.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE};

use crate::slots::TABLE_LEN;

const STRIDE: usize = 32;
static SEEN: [AtomicU32; TABLE_LEN.div_ceil(32)] = [const { AtomicU32::new(0) }; TABLE_LEN.div_ceil(32)];

extern "C" fn on_call(slot: u32, ret: usize) {
    let slot = slot as usize;
    if slot >= TABLE_LEN {
        return;
    }
    let bit = 1u32 << (slot % 32);
    if SEEN[slot / 32].fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    let mut m: HMODULE = std::ptr::null_mut();
    // SAFETY: address lookup only; no reference count change.
    let found = unsafe {
        GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, ret as *const u16, &mut m)
    } != 0;
    let caller = if found {
        let mut buf = [0u16; 260];
        // SAFETY: buf is valid for its length.
        let n = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), 260) } as usize;
        let path = String::from_utf16_lossy(&buf[..n]);
        let name = path.rsplit('\\').next().unwrap_or("?").to_string();
        format!("{name}+{:#x}", ret - m as usize)
    } else {
        format!("{ret:#x}")
    };
    log!("trace: slot {slot} first called from {caller}");
}

/// Wraps every non-null entry of `table` in a logging thunk. Returns the
/// original entries (restore them before polcore is released).
///
/// # Safety
/// `table` must be polcore's live function table and no other thread may be
/// calling through it yet.
pub unsafe fn install(table: &mut [usize]) -> Result<Vec<usize>, String> {
    if table.len() != TABLE_LEN {
        return Err("bad table length".into());
    }
    let originals: &'static mut [usize] = Box::leak(table.to_vec().into_boxed_slice());
    let code = VirtualAlloc(std::ptr::null(), TABLE_LEN * STRIDE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as *mut u8;
    if code.is_null() {
        return Err(format!("VirtualAlloc failed ({})", std::io::Error::last_os_error()));
    }
    let hook = on_call as extern "C" fn(u32, usize) as usize;
    for (slot, entry) in table.iter_mut().enumerate() {
        if *entry == 0 {
            continue;
        }
        let at = code.add(slot * STRIDE);
        let mut s: Vec<u8> = Vec::with_capacity(STRIDE);
        s.push(0x9C); // pushfd
        s.push(0x60); // pushad
        s.extend_from_slice(&[0xFF, 0x74, 0x24, 0x24]); // push dword [esp+0x24]  (caller's return address)
        s.push(0x68); // push imm32 (slot)
        s.extend_from_slice(&(slot as u32).to_le_bytes());
        s.push(0xE8); // call rel32 on_call
        let next = at as usize + s.len() + 4;
        s.extend_from_slice(&(hook.wrapping_sub(next) as u32).to_le_bytes());
        s.extend_from_slice(&[0x83, 0xC4, 0x08]); // add esp, 8
        s.push(0x61); // popad
        s.push(0x9D); // popfd
        s.extend_from_slice(&[0xFF, 0x25]); // jmp [abs32]
        s.extend_from_slice(&((originals.as_ptr().add(slot)) as u32).to_le_bytes());
        debug_assert!(s.len() <= STRIDE);
        std::ptr::copy_nonoverlapping(s.as_ptr(), at, s.len());
        *entry = at as usize;
    }
    let mut old = 0;
    VirtualProtect(code as *const c_void, TABLE_LEN * STRIDE, PAGE_EXECUTE_READ, &mut old);
    log!("trace: {} slots wrapped", table.iter().filter(|&&e| e != 0).count());
    Ok(originals.to_vec())
}
