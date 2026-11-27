//! Renderer port #2 (after MKmajster's 8 GB fork; `VICTORY_PORT_DESCRIPTORS=1`):
//! hooks the title's direct
//! descriptor-table record stores so they mark the written slots in a shared
//! map instead of forcing a full write-watch scan of descriptor heap pages.
//!
//! Each patched site is verified byte-for-byte before patching; if any site
//! differs from the expected instruction sequence, it is left untouched.
//!
//! This module implements its own near-code allocation and rel32 jump encoding
//! without depending on `console_insn`.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE,
    VirtualAlloc, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Size of the shared `Marks` descriptor table read by the generated stubs:
/// `{ start: u64, len: u64, slots: *mut u8, chunks: *mut u8 }`.
pub const MARKS_LEN: usize = 32;

/// Size of the executable stub slab allocated near the title image.
const STUB_SLAB_LEN: usize = 64 * 1024;

/// Allocation granularity for Windows virtual memory searches (64 KiB).
const ALLOC_GRANULARITY: usize = 64 * 1024;

/// Maximum signed displacement reachable by an x86-64 `jmp rel32` instruction
/// with a safety margin for stub offsets inside the slab.
const MAX_REL32_DISTANCE: usize = 0x7E00_0000;

/// A store site inside the title image to verify and hook.
pub struct Site {
    pub window: usize,
    pub store: usize,
    pub len: usize,
    /// The window's length and its FNV-1a 64-bit hash: the title's own bytes
    /// are checked against it before anything is written, and are not kept here.
    pub window_len: usize,
    pub fnv: u64,
}

/// The six 32-byte `vmovups` descriptor-record stores in the title's graphics
/// and compute table-filling loops (image RVAs and verification windows).
pub const SITES: [Site; 6] = [
    // Graphics table filler, loop 1: [rcx+rsi]
    Site {
        window: 0x4d9f04f,
        store: 10,
        len: 5,
        window_len: 21,
        fnv: 0x465a7842dfd99f87,
    },
    // Graphics table filler, loop 2: [rcx+rsi]
    Site {
        window: 0x4d9f10d,
        store: 10,
        len: 5,
        window_len: 22,
        fnv: 0x123f3bc33f381f16,
    },
    // Graphics table filler, loop 3: [r9+rsi]
    Site {
        window: 0x4d9f19c,
        store: 12,
        len: 6,
        window_len: 26,
        fnv: 0x046d3a9ff6c88f13,
    },
    // Compute table filler, loop 1: [r9+r8]
    Site {
        window: 0x4eec95d,
        store: 11,
        len: 6,
        window_len: 23,
        fnv: 0xc20e38b1feec98a1,
    },
    // Compute table filler, loop 2: [rdi+r8]
    Site {
        window: 0x4eeca08,
        store: 10,
        len: 6,
        window_len: 22,
        fnv: 0xc462b5858adee087,
    },
    // Compute table filler, loop 3: [r10+r8]
    Site {
        window: 0x4eeca82,
        store: 11,
        len: 6,
        window_len: 24,
        fnv: 0x76aef3fa82a21b65,
    },
];

/// FNV-1a, 64 bits: what a site's window is checked against.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Encodes a 5-byte x86-64 `jmp rel32` from `from` to `to`, or `None` if `to`
/// does not fit in a signed 32-bit displacement from `from + 5`.
pub(crate) fn encode_rel32_jump(from: usize, to: usize) -> Option<[u8; 5]> {
    let next_rip = (from as i128).checked_add(5)?;
    let delta = (to as i128).checked_sub(next_rip)?;
    let rel = i32::try_from(delta).ok()?;
    let b = rel.to_le_bytes();
    Some([0xe9, b[0], b[1], b[2], b[3]])
}

/// Allocates `len` bytes of executable RWX memory within `±2 GiB` of `anchor`
/// so 5-byte relative jumps can reach both directions.
fn alloc_near(anchor: usize, len: usize) -> Option<usize> {
    let min_addr = anchor.saturating_sub(MAX_REL32_DISTANCE).max(ALLOC_GRANULARITY);
    let max_addr = anchor.saturating_add(MAX_REL32_DISTANCE);

    // Try allocating above `anchor` first, then below `anchor`, stepping over
    // queried regions using `VirtualQuery`.
    let try_range = |mut cursor: usize, limit: usize| -> Option<usize> {
        while cursor < limit {
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid pointer and size for VirtualQuery.
            let queried = unsafe { VirtualQuery(cursor as *const _, &mut mbi, size_of_val(&mbi)) };
            if queried == 0 || mbi.RegionSize == 0 {
                break;
            }
            let region_base = mbi.BaseAddress as usize;
            let region_end = region_base.saturating_add(mbi.RegionSize);
            if mbi.State == MEM_FREE {
                let aligned = cursor.next_multiple_of(ALLOC_GRANULARITY);
                if aligned >= region_base && aligned.saturating_add(len) <= region_end.min(limit) {
                    // SAFETY: reserving and committing fresh anonymous memory for code stubs.
                    let ptr = unsafe {
                        VirtualAlloc(
                            aligned as *const _,
                            len,
                            MEM_RESERVE | MEM_COMMIT,
                            PAGE_EXECUTE_READWRITE,
                        )
                    };
                    if !ptr.is_null() {
                        return Some(ptr as usize);
                    }
                }
            }
            if region_end <= cursor {
                break;
            }
            cursor = region_end.next_multiple_of(ALLOC_GRANULARITY);
        }
        None
    };

    try_range(anchor.next_multiple_of(ALLOC_GRANULARITY), max_addr)
        .or_else(|| try_range(min_addr.next_multiple_of(ALLOC_GRANULARITY), anchor))
}

/// Builds `lea rax, [base+index]` matching a VEX `vmovups [base+index], ymm0`
/// store (ModRM `0x04` + SIB, no displacement, non-RSP base).
fn lea_of(store: &[u8]) -> Option<[u8; 4]> {
    let (x, b, op_at) = match *store.first()? {
        0xc5 => (false, false, 2),
        0xc4 => {
            let p = *store.get(1)?;
            if p & 0x1f != 1 {
                return None;
            }
            (p & 0x40 == 0, p & 0x20 == 0, 3)
        }
        _ => return None,
    };
    let (op, modrm, sib) = (*store.get(op_at)?, *store.get(op_at + 1)?, *store.get(op_at + 2)?);
    if op != 0x11 || modrm != 0x04 || store.len() != op_at + 3 {
        return None;
    }
    if sib & 7 == 5 || (sib & 7 == 4 && !b) || ((sib >> 3) & 7 == 4 && !x) {
        return None;
    }
    Some([0x48 | (u8::from(x) << 1) | u8::from(b), 0x8d, 0x04, sib])
}

/// Generates a trampoline stub placed at `to` for the `store` instruction at
/// `at`, marking the written descriptor slot and its 4 KiB chunk in `marks`,
/// then jumping back to `at + store.len()`.
fn build_stub(store: &[u8], at: usize, to: usize, marks: usize) -> Option<Vec<u8>> {
    let lea = lea_of(store)?;
    let mut out = Vec::with_capacity(store.len() + 64);
    out.extend_from_slice(store);
    out.push(0x50); // push rax
    out.extend_from_slice(&lea); // lea rax, [base+index]
    out.extend_from_slice(&[0x52, 0x51]); // push rdx; push rcx
    out.extend_from_slice(&[0x48, 0xba]); // mov rdx, marks
    out.extend_from_slice(&(marks as u64).to_le_bytes());
    out.extend_from_slice(&[0x48, 0x2b, 0x02]); // sub rax, [rdx] (start)
    out.extend_from_slice(&[0x48, 0x3b, 0x42, 0x08]); // cmp rax, [rdx+8] (len)
    out.extend_from_slice(&[0x73, 0x18]); // jae skip_marks
    out.extend_from_slice(&[0x48, 0xc1, 0xe8, 0x05]); // shr rax, 5 (slot index)
    out.extend_from_slice(&[0x48, 0x8b, 0x4a, 0x10]); // mov rcx, [rdx+16] (slots ptr)
    out.extend_from_slice(&[0xc6, 0x04, 0x01, 0x01]); // mov byte ptr [rcx+rax], 1
    out.extend_from_slice(&[0x48, 0xc1, 0xe8, 0x07]); // shr rax, 7 (chunk index)
    out.extend_from_slice(&[0x48, 0x8b, 0x4a, 0x18]); // mov rcx, [rdx+24] (chunks ptr)
    out.extend_from_slice(&[0xc6, 0x04, 0x01, 0x01]); // mov byte ptr [rcx+rax], 1
    out.extend_from_slice(&[0x59, 0x5a, 0x58]); // pop rcx; pop rdx; pop rax
    let back = encode_rel32_jump(to + out.len(), at + store.len())?;
    out.extend_from_slice(&back);
    Some(out)
}

/// Result of attempting to patch one descriptor-store site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hooked {
    Done,
    /// Bytes at the site did not match the expected sequence; left unchanged.
    Differs,
    /// Outside the image, out of reach for rel32, or OS refused protection change.
    Failed(&'static str),
}

/// Verifies and patches `sites` in the loaded title image `[base, base+size)`.
///
/// # Safety
/// The image must stay mapped and no title thread may be executing its code yet.
/// `marks` must point to a valid `MARKS_LEN`-byte static table for the process lifetime.
pub unsafe fn apply(base: usize, size: usize, marks: usize, sites: &[Site]) -> Vec<Hooked> {
    let mut slab: Option<(usize, usize)> = None;
    let mut out = Vec::with_capacity(sites.len());
    for site in sites {
        let done = (|| {
            if site.window.checked_add(site.window_len).is_none_or(|end| end > size)
                || site.store + site.len > site.window_len
                || site.len < 5
            {
                return Hooked::Failed("outside the image");
            }
            let window = base + site.window;
            // SAFETY: verified within `[base, base + size)`.
            let current = unsafe { std::slice::from_raw_parts(window as *const u8, site.window_len) };
            if fnv1a(current) != site.fnv {
                return Hooked::Differs;
            }
            let at = window + site.store;
            if slab.is_none() {
                slab = alloc_near(at, STUB_SLAB_LEN).map(|addr| (addr, 0));
            }
            let Some((slab_base, used)) = slab.as_mut() else {
                return Hooked::Failed("no executable room near the image");
            };
            let to = *slab_base + *used;
            let store = &current[site.store..site.store + site.len];
            let (Some(body), Some(jump)) =
                (build_stub(store, at, to, marks), encode_rel32_jump(at, to))
            else {
                return Hooked::Failed("unsupported store encoding or rel32 out of range");
            };
            if *used + body.len() > STUB_SLAB_LEN {
                return Hooked::Failed("stub slab exhausted");
            }
            let mut old_protect = 0u32;
            // SAFETY: writing to our fresh RWX slab, then patching the verified site
            // before title threads start.
            unsafe {
                std::ptr::copy_nonoverlapping(body.as_ptr(), to as *mut u8, body.len());
                if VirtualProtect(at as *const _, site.len, PAGE_EXECUTE_READWRITE, &mut old_protect)
                    == 0
                {
                    return Hooked::Failed("VirtualProtect failed");
                }
                std::ptr::write_bytes(at as *mut u8, 0x90, site.len);
                std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
                VirtualProtect(at as *const _, site.len, old_protect, &mut old_protect);
                FlushInstructionCache(GetCurrentProcess(), at as *const _, site.len);
                FlushInstructionCache(GetCurrentProcess(), to as *const _, body.len());
            }
            *used += body.len().next_multiple_of(16);
            Hooked::Done
        })();
        let offset = format_args!("{:#x}", site.window + site.store);
        match done {
            Hooked::Done => tracing::info!(
                site = %offset,
                "renderer port 2: patched descriptor store at title+site"
            ),
            Hooked::Differs => tracing::warn!(
                site = %offset,
                "renderer port 2: skipped descriptor store, bytes differ"
            ),
            Hooked::Failed(why) => tracing::warn!(
                site = %offset,
                why,
                "renderer port 2: failed to patch descriptor store"
            ),
        }
        out.push(done);
    }
    out
}

/// A run of stores that writes `slots` 32-byte records from `rbx` on: the
/// graphics table filler's fast path for a table of exactly four records
/// (title+0x4d9f25f: `vmovups [rbx+0x60]`, `[rbx+0x40]`, `[rbx+0x20]`,
/// `[rbx]`), which the six [`SITES`] do not cover. Found by
/// `VICTORY_PORT_DESCRIPTORS_TRACE` (run port-run20): the one writer of view
/// records into the heap besides the hooked stores and `memcpy`.
pub struct RunSite {
    pub at: usize,
    pub len: usize,
    pub fnv: u64,
    pub slots: u8,
}

pub const RUNS: [RunSite; 1] = [RunSite { at: 0x4d9f25f, len: 19, fnv: 0x87c9dad5ad1cf103, slots: 4 }];

/// The stub for a [`RunSite`]: the window's stores as they are, then each
/// of the `slots` records from `rbx` on marked as [`build_stub`] marks one,
/// then back to `back`. The flags are not kept (the title sets them again
/// before it reads them, as after the single stores).
fn build_run_stub(window: &[u8], back: usize, to: usize, marks: usize, slots: u8) -> Option<Vec<u8>> {
    let mut out = window.to_vec();
    out.extend_from_slice(&[0x50, 0x52, 0x51]); // push rax; push rdx; push rcx
    out.extend_from_slice(&[0x48, 0xba]); // mov rdx, marks
    out.extend_from_slice(&(marks as u64).to_le_bytes());
    for k in 0..slots {
        out.extend_from_slice(&[0x48, 0x8d, 0x43, k.checked_mul(32)?]); // lea rax, [rbx + k*32]
        out.extend_from_slice(&[0x48, 0x2b, 0x02]); // sub rax, [rdx] (start)
        out.extend_from_slice(&[0x48, 0x3b, 0x42, 0x08]); // cmp rax, [rdx+8] (len)
        out.extend_from_slice(&[0x73, 0x18]); // jae past the marks
        out.extend_from_slice(&[0x48, 0xc1, 0xe8, 0x05]); // shr rax, 5
        out.extend_from_slice(&[0x48, 0x8b, 0x4a, 0x10]); // mov rcx, [rdx+16] (slots)
        out.extend_from_slice(&[0xc6, 0x04, 0x01, 0x01]); // mov byte [rcx+rax], 1
        out.extend_from_slice(&[0x48, 0xc1, 0xe8, 0x07]); // shr rax, 7
        out.extend_from_slice(&[0x48, 0x8b, 0x4a, 0x18]); // mov rcx, [rdx+24] (chunks)
        out.extend_from_slice(&[0xc6, 0x04, 0x01, 0x01]); // mov byte [rcx+rax], 1
    }
    out.extend_from_slice(&[0x59, 0x5a, 0x58]); // pop rcx; pop rdx; pop rax
    let jump = encode_rel32_jump(to + out.len(), back)?;
    out.extend_from_slice(&jump);
    Some(out)
}

/// Verifies and patches the [`RunSite`]s in the loaded title image, as
/// [`apply`] does the single stores.
///
/// # Safety
/// As for [`apply`].
pub unsafe fn apply_runs(base: usize, size: usize, marks: usize, runs: &[RunSite]) -> Vec<Hooked> {
    let mut out = Vec::new();
    for run in runs {
        let done = (|| {
            if run.at.checked_add(run.len).is_none_or(|end| end > size) || run.len < 5 {
                return Hooked::Failed("outside the image");
            }
            let at = base + run.at;
            // SAFETY: inside the image.
            let window = unsafe { std::slice::from_raw_parts(at as *const u8, run.len) };
            if fnv1a(window) != run.fnv {
                return Hooked::Differs;
            }
            let Some(to) = alloc_near(at, STUB_SLAB_LEN) else {
                return Hooked::Failed("no executable room near the image");
            };
            let (Some(body), Some(jump)) =
                (build_run_stub(window, at + run.len, to, marks, run.slots), encode_rel32_jump(at, to))
            else {
                return Hooked::Failed("rel32 out of range");
            };
            let mut old = 0u32;
            // SAFETY: our fresh slab, then the verified window, before title threads start.
            unsafe {
                std::ptr::copy_nonoverlapping(body.as_ptr(), to as *mut u8, body.len());
                if VirtualProtect(at as *const _, run.len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                    return Hooked::Failed("VirtualProtect failed");
                }
                std::ptr::write_bytes(at as *mut u8, 0x90, run.len);
                std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
                VirtualProtect(at as *const _, run.len, old, &mut old);
                FlushInstructionCache(GetCurrentProcess(), at as *const _, run.len);
                FlushInstructionCache(GetCurrentProcess(), to as *const _, body.len());
            }
            Hooked::Done
        })();
        tracing::info!(site = format_args!("{:#x}", run.at), ?done, "renderer port 2: run of descriptor stores");
        out.push(done);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    #[test]
    fn rel32_jump_encodes_forward_backward_and_rejects_overflow() {
        assert_eq!(encode_rel32_jump(0x1000, 0x1005), Some([0xe9, 0, 0, 0, 0]));
        assert_eq!(encode_rel32_jump(0x1000, 0x1000), Some([0xe9, 0xfb, 0xff, 0xff, 0xff]));
        let fwd = encode_rel32_jump(0x1000_0000, 0x1000_1234).unwrap();
        assert_eq!(fwd[0], 0xe9);
        assert_eq!(i32::from_le_bytes(fwd[1..5].try_into().unwrap()), 0x1234 - 5);
        assert!(encode_rel32_jump(0x1_0000_0000, 0x3_0000_0000).is_none());
    }

    #[test]
    fn sites_decode_expected_lea_and_build_valid_stubs() {
        // The six stores' encodings (`vmovups [base+index], ymm0`, VEX), as an
        // assembler writes them; the sites keep only a hash of the title's bytes.
        let stores: [&[u8]; 6] = [&[0xc5, 0xfc, 0x11, 0x04, 0x31],&[0xc5, 0xfc, 0x11, 0x04, 0x31],&[0xc4, 0xc1, 0x7c, 0x11, 0x04, 0x31],&[0xc4, 0x81, 0x7c, 0x11, 0x04, 0x01],&[0xc4, 0xa1, 0x7c, 0x11, 0x04, 0x07],&[0xc4, 0x81, 0x7c, 0x11, 0x04, 0x02]];
        let leas: Vec<[u8; 4]> = stores.iter().map(|s| lea_of(s).unwrap()).collect();
        assert_eq!(leas[0], [0x48, 0x8d, 0x04, 0x31]); // [rcx+rsi]
        assert_eq!(leas[1], [0x48, 0x8d, 0x04, 0x31]);
        assert_eq!(leas[2], [0x49, 0x8d, 0x04, 0x31]); // [r9+rsi]
        assert_eq!(leas[3], [0x4b, 0x8d, 0x04, 0x01]); // [r9+r8]
        assert_eq!(leas[4], [0x4a, 0x8d, 0x04, 0x07]); // [rdi+r8]
        assert_eq!(leas[5], [0x4b, 0x8d, 0x04, 0x02]); // [r10+r8]

        for (s, store) in SITES.iter().zip(stores) {
            assert_eq!(s.len, store.len());
            assert!(s.store + s.len <= s.window_len);
            let stub = build_stub(store, 0x7ff7_0000_1000, 0x7ff7_0010_0000, 0x1234_5678).unwrap();
            assert!(stub.starts_with(store));
            assert_eq!(stub[stub.len() - 5], 0xe9);
        }
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);

        // Displaced, stack-based, or non-vmovups forms must be rejected.
        assert!(lea_of(&[0xc5, 0xfc, 0x11, 0x44, 0x31, 0x08]).is_none());
        assert!(lea_of(&[0xc5, 0xfc, 0x11, 0x04, 0x24]).is_none());
        assert!(lea_of(&[0x90, 0x90, 0x90, 0x90, 0x90]).is_none());
    }

    #[test]
    fn run_stub_keeps_the_stores_and_marks_each_record() {
        let window = [0xc5, 0xfc, 0x11, 0x5b, 0x60, 0xc5, 0xfc, 0x11, 0x53, 0x40, 0xc5, 0xfc, 0x11, 0x4b, 0x20, 0xc5, 0xfc, 0x11, 0x03];
        assert_eq!(fnv1a(&window), RUNS[0].fnv);
        let stub = build_run_stub(&window, 0x7ff7_0000_1013, 0x7ff7_0010_0000, 0x1234_5678, 4).unwrap();
        assert!(stub.starts_with(&window));
        assert_eq!(stub.iter().filter(|&&b| b == 0x73).count() >= 4, true);
        assert_eq!(stub[stub.len() - 5], 0xe9);
    }

    #[test]
    fn alloc_near_returns_rel32_reachable_slab() {
        let anchor = alloc_near as *const () as usize;
        let slab = alloc_near(anchor, STUB_SLAB_LEN).expect("near slab allocation");
        assert!(encode_rel32_jump(anchor, slab).is_some());
        assert!(encode_rel32_jump(slab + STUB_SLAB_LEN - 5, anchor).is_some());
        // SAFETY: freeing the slab allocated above.
        unsafe { VirtualFree(slab as *mut _, 0, MEM_RELEASE) };
    }
}
