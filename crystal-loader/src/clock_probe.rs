//! Offline experiment only. Run this one ignored test in a fresh x86 process.
//! No bootstrap, member credentials, Open, pump, game component, or OS clock write.
#![cfg(all(test, target_arch = "x86", target_os = "windows"))]

use super::*;
use std::time::Duration;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS};
use windows_sys::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount};

const PREFERRED_BASE: u32 = 0x1000_0000;
const CLOCK_RVA: usize = 0x3e4c30;
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
type State = [u32; 5]; // Unix seconds, initialized, synchronized, mode, tick anchor.

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) { unsafe { CoUninitialize() }; }
}

struct RestoreClock { pointer: *mut u32, state: State }
impl Drop for RestoreClock {
    fn drop(&mut self) {
        // The DLL stays resident. This runs before the stock COM object is released,
        // including on a failed assertion. No other core consumer was initialized.
        unsafe { write_state(self.pointer, self.state); }
    }
}

unsafe fn read_state(pointer: *const u32) -> State {
    std::array::from_fn(|index| pointer.add(index).read_volatile())
}

unsafe fn write_state(pointer: *mut u32, state: State) {
    for (index, value) in state.into_iter().enumerate() { pointer.add(index).write_volatile(value); }
}

// These bytes came from the statically unpacked text of the installed file:
// SHA256 73b1864bf522d3aa6ec28e9f4bb226fcee956f7f4848a238c9c2d756dc703b06.
// The whole-file FNV checksum below detects fixture drift; it is not a security
// authenticity check. Each invoked routine is also compared byte for byte after
// rebasing its absolute operands. Relative calls do not need relocation.
unsafe fn check_code(base: usize, rva: usize, hex: &str, absolute_offsets: &[usize]) {
    let mut expected: Vec<u8> = hex.as_bytes().chunks_exact(2).map(|pair| {
        u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
    }).collect();
    let delta = (base as u32).wrapping_sub(PREFERRED_BASE);
    for &offset in absolute_offsets {
        let original = u32::from_le_bytes(expected[offset..offset + 4].try_into().unwrap());
        expected[offset..offset + 4].copy_from_slice(&original.wrapping_add(delta).to_le_bytes());
    }
    assert_eq!(std::slice::from_raw_parts((base + rva) as *const u8, expected.len()), expected,
        "Unrecognized native clock routine at RVA {rva:#x}");
}

unsafe fn verify_clock(module: HMODULE, dll: &Path) -> usize {
    assert!(same_dll_path(&PathBuf::from(module_path(module)), dll));
    let (base, size, timestamp) = module_image(module);
    assert_eq!((size, timestamp), (0x451000, POLCORE_TIMESTAMP));
    check_code(base, 0x4cc00, "a1344c3e1085c0755d8b44240453568d4c240c5751a33c4c3e10e84e0701008b7c24148b1db051061083c4048d54241052e8370701008b74241483c404ffd33bfea3404c3e1074e447c705384c3e1000000000893d304c3e10c705344c3e10010000005f5e5bc3", &[1,22,37,66,75,85,91]);
    check_code(base, 0x4cc70, "a13c4c3e10c3", &[1]);
    check_code(base, 0x4cc80, "a1344c3e1085c0740ac705344c3e1000000000c3", &[1,11]);
    check_code(base, 0x4cca0, "ff15b0510610a3404c3e108b442404a3304c3e10b801000000a3384c3e10c3", &[2,7,16,26]);
    check_code(base, 0x4ccf0, "e81b0000008b0d304c3e1003c18b4c24048901a1384c3e10c3", &[7,20]);
    check_code(base, 0x4cd10, "568b35b0510610ffd6ffd6ffd68b15404c3e108bc82bcab8d34d6210f7e18bc25ec1e806c3", &[3,15]);
    check_code(base, 0x5d36d, "558bec51518d45f850ff15545106108b45f88b4dfc6a00050080c12a688096980081d1214e62fe5150e8e569ffff8b4d0885c974028901c9c3", &[11]);
    let kernel = GetModuleHandleW(wide("kernel32.dll").as_ptr());
    assert!(!kernel.is_null());
    for (rva, name) in [(0x651b0, b"GetTickCount\0".as_slice()), (0x65154, b"GetSystemTimeAsFileTime\0".as_slice())] {
        let expected = GetProcAddress(kernel, name.as_ptr()).expect("unmodified system clock import") as usize;
        assert_eq!(((base + rva) as *const usize).read(), expected, "Clock import was changed");
    }
    let pointer = (base + CLOCK_RVA) as *mut u32;
    let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    assert_ne!(VirtualQuery(pointer.cast(), &mut info, std::mem::size_of_val(&info)), 0);
    assert_eq!(info.AllocationBase as usize, base);
    assert_eq!(info.State, MEM_COMMIT);
    assert_eq!(info.Protect & (PAGE_GUARD | PAGE_NOACCESS), 0);
    assert!(matches!(info.Protect & 0xff, PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY));
    assert!(pointer as usize + std::mem::size_of::<State>() <= info.BaseAddress as usize + info.RegionSize);
    assert_eq!(read_state(pointer), [0; 5], "Clock already initialized; use a fresh process");
    base
}

unsafe fn unix_100ns() -> u64 {
    let mut time = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    GetSystemTimeAsFileTime(&mut time);
    (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64).checked_sub(FILETIME_UNIX_EPOCH).unwrap()
}

// This is the proposed formula under test, not a replacement initializer. The
// two tick reads reject samples straddling a GetTickCount quantum. FILETIME may
// still have a different phase/quantization from the stock boundary spin.
unsafe fn candidate_anchor(target_phase_ms: u32) -> (State, u32, u64) {
    let started = Instant::now();
    loop {
        let capture_started = Instant::now();
        let before = GetTickCount();
        let wall = unix_100ns();
        let after = GetTickCount();
        let capture_ns = capture_started.elapsed().as_nanos() as u64;
        let phase_ms = ((wall / 10_000) % 1_000) as u32;
        if before == after && phase_ms >= target_phase_ms && phase_ms < target_phase_ms + 32 {
            return ([(wall / 10_000_000) as u32, 1, 0, 2, after.wrapping_sub(phase_ms)], (wall % 10_000_000) as u32, capture_ns);
        }
        assert!(started.elapsed() < Duration::from_secs(2), "Could not sample requested clock phase");
        std::thread::sleep(Duration::from_millis(1));
    }
}

type Reader = unsafe extern "C" fn(*mut u32) -> u32;
type ServerRebase = unsafe extern "C" fn(u32);
type Reset = unsafe extern "C" fn();

unsafe fn native_read(read: Reader) -> (u32, u32) {
    let mut second = 0;
    let synchronized = read(&mut second);
    (second, synchronized)
}

unsafe fn check_arithmetic(read: Reader, state: State) {
    for _ in 0..64 {
        let before = GetTickCount();
        let actual = native_read(read);
        let after = GetTickCount();
        if before == after {
            assert_eq!(actual, (state[0].wrapping_add(before.wrapping_sub(state[4]) / 1000), state[2]));
            return;
        }
    }
    panic!("Could not bracket a native read within one tick quantum");
}

#[test]
#[ignore = "Private fresh-process installed US core clock experiment; no sign-in or game"]
fn installed_polcore_clock_phase_wrap_rebase_offline() {
    unsafe {
        assert!(GetModuleHandleW(wide("polcore.dll").as_ptr()).is_null(), "Run only this test in a fresh process");
        assert!(CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0);
        let _apartment = Apartment;
        let viewer = registry::install_folder(Region::Us, "1000").expect("installed US viewer");
        let dll = PathBuf::from(ansi_to_string(&viewer)).join("viewer/com/polcore.dll");
        let file = std::fs::read(&dll).unwrap();
        assert_eq!(file.len(), 548352);
        let checksum = file.iter().fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3));
        assert_eq!(checksum, 0x54df4d8fb2fac492, "Installed core differs from the investigated file");
        let module = LoadLibraryExW(wide(&dll.to_string_lossy()).as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
        assert!(!module.is_null());
        let base = verify_clock(module, &dll); // Before constructing or invoking clock functions.
        let activation_started = Instant::now();
        let core = create_from_dll(&CLSID_POLCORE[1], &IID_POLCORE[1], &dll, "offline clock probe").unwrap();
        let stock_activation_ms = activation_started.elapsed().as_secs_f64() * 1000.0;
        let pointer = (base + CLOCK_RVA) as *mut u32;
        let stock = read_state(pointer);
        let restore = RestoreClock { pointer, state: stock };
        assert_eq!(&stock[1..4], &[1, 0, 2]);
        assert_eq!(core.method::<usize>(VT_GET_COMMON_FUNCTION_TABLE), base + 0x690e);
        let read: Reader = std::mem::transmute(base + 0x4ccf0);
        let rebase: ServerRebase = std::mem::transmute(base + 0x4cca0);
        let reset: Reset = std::mem::transmute(base + 0x4cc80);
        check_arithmetic(read, stock);

        // Irregular spacing avoids repeatedly sampling the same coarse timer phase.
        let phases = [73, 281, 542, 887];
        let captures = phases.map(|phase| candidate_anchor(phase));
        let candidates = captures.map(|capture| capture.0);
        let alignment_ms = candidates.map(|candidate| {
            // Compare the tick anchors at the same Unix second, modulo 2^32.
            candidate[4].wrapping_add(stock[0].wrapping_sub(candidate[0]).wrapping_mul(1000)).wrapping_sub(stock[4]) as i32
        });
        let mut samples = 0;
        let mut mismatches = [0usize; 4];
        let mut max_difference_seconds = [0u32; 4];
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(2200) {
            write_state(pointer, stock);
            let tick_before = GetTickCount();
            let expected = native_read(read);
            let actual = candidates.map(|state| { write_state(pointer, state); native_read(read) });
            write_state(pointer, stock);
            let tick_after = GetTickCount();
            if tick_before == tick_after {
                samples += 1;
                for (index, value) in actual.into_iter().enumerate() {
                    assert_eq!(value.1, expected.1);
                    if value.0 != expected.0 {
                        mismatches[index] += 1;
                        max_difference_seconds[index] = max_difference_seconds[index].max((value.0.wrapping_sub(expected.0) as i32).unsigned_abs());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(samples > 50);
        eprintln!("CLOCK_INIT stock_activation_ms={stock_activation_ms:.3}");
        for index in 0..4 {
            eprintln!("CLOCK_PHASE requested_phase_ms={} captured_phase_ms={:.4} capture_us={:.3} alignment_ms={} stable_tick_samples={samples} mismatches={} max_difference_seconds={}", phases[index], captures[index].1 as f64 / 10_000.0, captures[index].2 as f64 / 1000.0, alignment_ms[index], mismatches[index], max_difference_seconds[index]);
        }

        // Force elapsed values on either side of a second and unsigned-u32 wrap
        // by moving only this private DLL's base tick. The OS clock is untouched.
        let elapsed_cases = [0, 1, 999, 1000, 1001, 65_535, 4_294_966_999, u32::MAX - 1000, u32::MAX - 1, u32::MAX];
        for elapsed in elapsed_cases {
            let mut observed = false;
            for _ in 0..64 {
                let tick = GetTickCount();
                let state = [0xffff_fffe, 1, 0, 2, tick.wrapping_sub(elapsed)];
                write_state(pointer, state);
                let actual = native_read(read);
                if GetTickCount() == tick {
                    assert_eq!(actual, (state[0].wrapping_add(elapsed / 1000), 0), "elapsed={elapsed}");
                    observed = true;
                    break;
                }
            }
            assert!(observed, "Could not observe elapsed={elapsed} at a stable tick");
        }

        for state in std::iter::once(stock).chain(candidates) {
            write_state(pointer, state);
            let before = GetTickCount();
            rebase(1_700_000_123); // Synthetic server second, direct clock setter only.
            let after = GetTickCount();
            let rebased = read_state(pointer);
            assert_eq!(&rebased[..4], &[1_700_000_123, state[1], 1, state[3]]);
            assert!(rebased[4].wrapping_sub(before) <= after.wrapping_sub(before));
            check_arithmetic(read, rebased);
            reset();
            let mut reset_expected = rebased;
            reset_expected[1] = 0;
            assert_eq!(read_state(pointer), reset_expected);
            check_arithmetic(read, reset_expected); // Reset alone does not reset/rebase the reader.
            reset();
            assert_eq!(read_state(pointer), reset_expected);
        }
        eprintln!("CLOCK_SEMANTICS unsigned_wrap_cases={} rebase_reset_states=5 passed=true observed_equivalent={} (sampling is not a general equivalence proof)", elapsed_cases.len(), mismatches == [0; 4]);
        drop(restore);
        assert_eq!(read_state(pointer), stock);
        drop(core);
    }
}
