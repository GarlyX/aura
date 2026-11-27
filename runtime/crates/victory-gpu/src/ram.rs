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
