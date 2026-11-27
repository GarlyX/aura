//! The PC's memory class (aura): below 24 GiB of physical memory the layer's
//! pools are sized for a 16 GB PC. Upstream's sizes are measured on 32 GB
//! and more; on 16 GB they ran the commit limit out (48.0 of 48.7 GB) five
//! minutes into the world and the run ended in the driver (rebase-run4):
//! 512 idle host lists kept their command allocators (4.7 GB more memory
//! shared with the card than with 128), and 512 MiB of spare scratch chunks.

use std::sync::OnceLock;

/// Below this much physical memory the PC counts as small.
const SMALL_BELOW: u64 = 24 << 30;

/// Whether the PC has less than 24 GiB of physical memory
/// (`VICTORY_RAM_CLASS=large` / `small` decides instead).
pub(crate) fn small() -> bool {
    static SMALL: OnceLock<bool> = OnceLock::new();
    *SMALL.get_or_init(|| {
        let total = total_physical();
        let small = match std::env::var("VICTORY_RAM_CLASS").as_deref() {
            Ok("large") => false,
            Ok("small") => true,
            _ => total.is_some_and(|t| t < SMALL_BELOW),
        };
        tracing::info!(
            total_mb = total.map(|t| t >> 20),
            small,
            "the PC's memory class: pools sized for a small PC (idle host lists 128, spare scratch 128 MiB, heap worker lists 64 MiB) when small"
        );
        small
    })
}

/// Total physical memory in bytes.
fn total_physical() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullTotalPhys)
}

/// The most host lists kept idle: `VICTORY_LIST_POOL_MOST`, else 128 on a
/// small PC and `large` otherwise.
pub(crate) fn idle_lists(large: usize) -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_LIST_POOL_MOST")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(if small() { 128 } else { large })
    })
}

/// Asks Windows to keep at least this much of the process in RAM (a hard
/// minimum working set), so that under pressure other processes and the
/// compression store are trimmed before the title's and the layer's pages.
///
/// Why (run port-run10-mem, 16 GB PC, the world settled): the process held
/// 5.6 GB in RAM, the compression store 3.1 GB, other programs about 2.5 GB,
/// 0.9 GB was free, and the title's frames took 100,000-165,000 page faults
/// each while it streamed (the submitting thread "at work" 1.9 s of a frame
/// and on a processor for 0.3 s of it). `VICTORY_WORKING_SET_MB` sets the
/// minimum; none by default: with 8,900 MB (port-run11-ws) the process held
/// 7.5-9 GB in RAM instead of 5.6, yet the streaming frames took more page
/// faults (115,000 against 94,000 a long frame) and were no faster. Most of
/// those faults are first touches of the 64 KiB views the title maps
/// thousands of times a frame, not pages Windows took away. Asked again at half the size,
/// down to 2 GiB, if Windows turns it down; the outcome is logged and
/// returned for the terminal.
pub fn hold_working_set() -> Option<u64> {
    use windows_sys::Win32::System::Memory::{
        QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_ENABLE, SetProcessWorkingSetSizeEx,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let total = total_physical()?;
    let asked = match std::env::var("VICTORY_WORKING_SET_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(mb) => mb << 20,
        None => 0,
    };
    let mut min = asked;
    while min >= 2 << 30 {
        // SAFETY: our own process; the maximum is a soft one (disabled as a limit).
        let done = unsafe {
            SetProcessWorkingSetSizeEx(
                GetCurrentProcess(),
                min as usize,
                total as usize,
                QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
            )
        } != 0;
        if done {
            tracing::info!(min_mb = min >> 20, asked_mb = asked >> 20, "the process's working set: a hard minimum set (VICTORY_WORKING_SET_MB)");
            return Some(min);
        }
        tracing::warn!(
            min_mb = min >> 20,
            error = %std::io::Error::last_os_error(),
            "the process's working set: the minimum was turned down; asking for half"
        );
        min /= 2;
    }
    None
}
