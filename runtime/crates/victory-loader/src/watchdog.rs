//! The title's stall watchdog, off (aura; `VICTORY_WATCHDOG=1` keeps it).
//!
//! What the title does (build 1.0.2.1, image offsets): a thread whose
//! function starts at +0x2215a60 sleeps a second at a time and compares the
//! main loop's last progress time with the one it saw before; after 30
//! passes with no progress it ends the process on purpose (a null write at
//! +0x2215c35, reached through +0x2215c27). On a 16 GB PC loads stall for
//! that long while memory is paged (a run here had no frame for 34 s and
//! ended so; fxceless/victory-fork issue #416 is the same). The thread's
//! function becomes `xor eax, eax ; ret`: the thread ends at once and
//! nothing watches. Its first 16 bytes are compared before anything is
//! written; other bytes (another build) are left alone and said so.

use crate::resolution::{Patched, Site};

/// The watchdog thread's function, its first 16 bytes.
pub const THREAD: Site = Site {
    at: 0x2215a60,
    what: "the title's stall watchdog thread",
    bytes: &[0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x56, 0x57, 0x55, 0x53, 0x48, 0x83, 0xec, 0x68],
};

/// `xor eax, eax ; ret`.
pub const RETURN_AT_ONCE: [u8; 3] = [0x31, 0xc0, 0xc3];

/// Whether the watchdog is turned off (unless `VICTORY_WATCHDOG=1`).
pub fn off() -> bool {
    std::env::var_os("VICTORY_WATCHDOG").is_none_or(|v| v != "1")
}

/// Turns the watchdog off in the mapped image.
///
/// # Safety
/// The image of `size` bytes at `base` stays mapped and no thread runs its
/// code yet.
#[cfg(windows)]
pub unsafe fn apply(base: usize, size: usize) -> Patched {
    if !off() {
        return Patched::Differs;
    }
    // SAFETY: the caller's promise.
    unsafe { crate::resolution::write(base, size, &THREAD, &RETURN_AT_ONCE) }
}
