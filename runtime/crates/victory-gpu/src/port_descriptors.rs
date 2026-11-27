//! Renderer port #2 (after MKmajster's 8 GB fork; `VICTORY_PORT_DESCRIPTORS=1`,
//! off by default, see [`on`]): tracks
//! written descriptor slots via patched title stores and intercepted
//! `memcpy`/`memmove` calls, replacing per-frame full write-watch scans in
//! [`crate::descriptor::sync_heaps`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock, RwLock};

/// Slots per chunk byte in the two-level dirty bitmap (128 slots = 4 KiB).
const CHUNK: usize = 128;

/// Shared layout read directly by the patched descriptor-store stubs
/// (`victory_loader::descriptor_sites::MARKS_LEN` = 32 bytes).
#[repr(C)]
struct Marks {
    start: AtomicU64,
    len: AtomicU64,
    slots: AtomicPtr<AtomicU8>,
    chunks: AtomicPtr<AtomicU8>,
}

static MARKS: Marks = Marks {
    start: AtomicU64::new(0),
    len: AtomicU64::new(0),
    slots: AtomicPtr::new(std::ptr::null_mut()),
    chunks: AtomicPtr::new(std::ptr::null_mut()),
};

/// Title image `[base, size, patched_store_count]`.
static IMAGE: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

/// Whether renderer port #2 is enabled (`VICTORY_PORT_DESCRIPTORS=1`; never
/// in tests, which have no patched title).
///
/// aura, run port-run8 (720p, the world settled): the patched stores and
/// the copies told about 60,000 slots a frame and the view-heap pass went
/// from 4.3 to 1.2 ms before making slots, but the audit found ~160 records
/// a frame no hook marked (writers inside the title and in a host DLL), so
/// they stay stale until an audit reaches their page. Off by default.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !cfg!(test) && std::env::var_os("VICTORY_PORT_DESCRIPTORS").is_some_and(|v| v == "1"))
}

/// Address of the static [`Marks`] block for the loader's patched stubs.
pub fn marks_address() -> usize {
    std::ptr::from_ref(&MARKS) as usize
}

/// Records the loaded image bounds and how many descriptor stores were patched.
pub fn hooked(base: usize, size: usize, patched: usize) {
    IMAGE[0].store(base, Relaxed);
    IMAGE[1].store(size, Relaxed);
    IMAGE[2].store(patched, Relaxed);
}

/// Whether port #2 is active (switch on and at least one store patched).
fn working() -> bool {
    on() && IMAGE[2].load(Relaxed) > 0
}

struct Adopted {
    start: u64,
    count: usize,
    slots: &'static [AtomicU8],
    chunks: &'static [AtomicU8],
    writers: &'static [AtomicUsize],
}

static ADOPTED: RwLock<Option<Adopted>> = RwLock::new(None);

/// Adopts the first placed shader-visible view heap for slot-level marking.
pub(crate) fn adopt(start: u64, count: u32) -> bool {
    working() && adopt_now(start, count)
}

fn adopt_now(start: u64, count: u32) -> bool {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.is_some() {
        tracing::info!(
            start = format_args!("{start:#x}"),
            count,
            "renderer port 2: additional placed view heap kept on write-watch scan"
        );
        return false;
    }
    let count = count as usize;
    let leak_u8 = |n: usize| -> &'static [AtomicU8] {
        Box::leak((0..n).map(|_| AtomicU8::new(0)).collect())
    };
    let slots = leak_u8(count);
    let chunks = leak_u8(count.div_ceil(CHUNK));
    let writers: &'static [AtomicUsize] = Box::leak(
        (0..(count * 32).div_ceil(1 << 16))
            .map(|_| AtomicUsize::new(0))
            .collect(),
    );
    MARKS.slots.store(slots.as_ptr().cast_mut(), Release);
    MARKS.chunks.store(chunks.as_ptr().cast_mut(), Release);
    MARKS.start.store(start, Release);
    MARKS.len.store(count as u64 * 32, Release);
    *adopted = Some(Adopted {
        start,
        count,
        slots,
        chunks,
        writers,
    });
    #[cfg(windows)]
    start_trace(start as usize, count * 32);
    tracing::info!(
        start = format_args!("{start:#x}"),
        count,
        stores = IMAGE[2].load(Relaxed),
        "renderer port 2: adopted placed view heap for direct slot marks (VICTORY_PORT_DESCRIPTORS)"
    );
    true
}

/// Releases the adopted view heap when dropped.
pub(crate) fn let_go(start: u64) {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.as_ref().is_some_and(|a| a.start == start) {
        MARKS.len.store(0, Release);
        *adopted = None;
        tracing::info!(
            start = format_args!("{start:#x}"),
            "renderer port 2: released marked view heap"
        );
    }
}

/// Whether `start` is the adopted marked heap.
pub(crate) fn adopted(start: u64) -> bool {
    ADOPTED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|a| a.start == start)
}

/// Marks slot `slot` in the leaked dirty maps (caller verified bounds).
#[inline]
fn mark_slot(slot: usize) {
    // SAFETY: `slots` and `chunks` are leaked static slices sized to `len / 32`.
    unsafe {
        (*MARKS.slots.load(Relaxed).add(slot)).store(1, Release);
        (*MARKS.chunks.load(Relaxed).add(slot / CHUNK)).store(1, Release);
    }
}

/// Marks the slot at `handle` when written by our own descriptor API calls.
#[inline]
pub(crate) fn mark(handle: u64) {
    let len = MARKS.len.load(Acquire);
    if len == 0 {
        return;
    }
    let at = handle.wrapping_sub(MARKS.start.load(Relaxed));
    if at >= len {
        return;
    }
    mark_slot((at / 32) as usize);
    MARKED_BY_US.fetch_add(1, Relaxed);
}

fn marked(a: &Adopted, index: u32) -> bool {
    a.slots
        .get(index as usize)
        .is_some_and(|s| s.load(Acquire) != 0)
}

/// Drains all marked slots of the adopted heap as contiguous `(first, count)` runs.
pub(crate) fn take(start: u64) -> Option<Vec<(u32, u32)>> {
    let began = std::time::Instant::now();
    let adopted = ADOPTED.read().unwrap_or_else(|e| e.into_inner());
    let a = adopted.as_ref().filter(|a| a.start == start)?;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut n = 0u64;
    for (c, chunk) in a.chunks.iter().enumerate() {
        if chunk.load(Relaxed) == 0 {
            continue;
        }
        chunk.swap(0, AcqRel);
        let first = c * CHUNK;
        for (i, slot) in a.slots[first..(first + CHUNK).min(a.count)].iter().enumerate() {
            if slot.load(Relaxed) == 0 || slot.swap(0, AcqRel) == 0 {
                continue;
            }
            let index = (first + i) as u32;
            n += 1;
            match runs.last_mut() {
                Some((at, len)) if *at + *len == index => *len += 1,
                _ => runs.push((index, 1)),
            }
        }
    }
    TAKEN[0].fetch_add(n, Relaxed);
    TAKEN[1].fetch_add(1, Relaxed);
    TAKEN[2].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    Some(runs)
}

/// Records the faulting instruction RIP for a 64 KiB block of the adopted heap.
pub(crate) fn faulted(at: usize, rip: usize) {
    if MARKS.len.load(Relaxed) == 0 {
        return;
    }
    let Ok(adopted) = ADOPTED.try_read() else {
        return;
    };
    if let Some(a) = adopted.as_ref() {
        let off = (at as u64).wrapping_sub(a.start);
        if let Some(w) = a.writers.get((off >> 16) as usize) {
            w.store(rip, Relaxed);
        }
    }
}

/// Fallback write-watch polling interval in frames (`VICTORY_PORT_DESCRIPTORS_EVERY`, default 8).
fn check_every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_PORT_DESCRIPTORS_EVERY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(8u64)
            .max(1)
    })
}

/// Fallback pages scanned per frame (`VICTORY_PORT_DESCRIPTORS_CHECK`, default 16; 0 disables).
fn check_budget() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_PORT_DESCRIPTORS_CHECK")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(16usize)
    })
}

struct Fallback {
    pages: BTreeSet<usize>,
    asked: u64,
    looked: u64,
    next: usize,
}

static FALLBACK: Mutex<Fallback> = Mutex::new(Fallback {
    pages: BTreeSet::new(),
    asked: 0,
    looked: 0,
    next: 0,
});

/// Whether a periodic fallback write-watch query is due on `frame`.
pub(crate) fn ask_due(frame: u64) -> bool {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    if check_budget() == 0 || (f.asked != 0 && frame < f.asked + check_every()) {
        return false;
    }
    f.asked = frame.max(1);
    true
}

/// Queues byte ranges reported by the fallback write-watch for incremental page verification.
pub(crate) fn asked(ranges: &[(usize, usize)]) {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    for &(from, to) in ranges {
        let mut page = from & !(crate::watch::PAGE - 1);
        while page < to {
            f.pages.insert(page);
            page += crate::watch::PAGE;
        }
    }
}

/// Returns up to `check_budget()` pages to audit on `frame` (at most once per frame).
pub(crate) fn to_check(frame: u64) -> Vec<usize> {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    if f.looked == frame {
        return Vec::new();
    }
    f.looked = frame;
    let mut out = Vec::new();
    while out.len() < check_budget() {
        let from = f.next;
        let Some(&page) = f.pages.range(from..).next().or_else(|| f.pages.iter().next()) else {
            break;
        };
        f.pages.remove(&page);
        f.next = page + crate::watch::PAGE;
        out.push(page);
    }
    CHECKED[0].fetch_add(out.len() as u64, Relaxed);
    out
}

fn should_warn_missed(total: u64) -> bool {
    total < 8
}

/// Accounts for a changed slot discovered by the fallback scan.
pub(crate) fn missed(index: u32, handle: u64, view: bool) {
    if !view {
        CHECKED[2].fetch_add(1, Relaxed);
        return;
    }
    let adopted = ADOPTED.read().unwrap_or_else(|e| e.into_inner());
    let Some(a) = adopted.as_ref() else { return };
    if marked(a, index) {
        CHECKED[3].fetch_add(1, Relaxed);
        return;
    }
    CHECKED[1].fetch_add(1, Relaxed);
    let total = MISSED_ALL.fetch_add(1, Relaxed);
    let rip = a
        .writers
        .get(((handle - a.start) >> 16) as usize)
        .map_or(0, |w| w.load(Relaxed));
    let (base, size) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
    let writer = if rip >= base && rip < base + size {
        format!("title+{:#x}", rip - base)
    } else if rip != 0 {
        format!("{rip:#x}")
    } else {
        "unknown".into()
    };
    {
        let mut by = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        *by.entry(writer.clone()).or_default() += 1;
    }
    if should_warn_missed(total) {
        tracing::warn!(
            slot = index,
            writer,
            "renderer port 2: fallback found unmarked view record"
        );
    }
}

/// Real host `memcpy` and `memmove` addresses saved during import binding.
static REAL: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static COPIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Intercepts the title's `memcpy`/`memmove` imports when port #2 is enabled so
/// descriptor records bulk-copied into the adopted heap also mark their slots.
pub fn import(name: &str, address: usize) -> usize {
    let (which, ours) = match name {
        "memcpy" => (0, copy as *const () as usize),
        "memmove" => (1, shift as *const () as usize),
        _ => return address,
    };
    if !on() {
        return address;
    }
    REAL[which].store(address, Relaxed);
    ours
}

type CopyFn = unsafe extern "system-unwind" fn(*mut u8, *const u8, usize) -> *mut u8;

unsafe extern "system-unwind" fn copy(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: forwards to host memcpy captured during import binding.
    unsafe { copied(to, from, n, REAL[0].load(Relaxed)) }
}

unsafe extern "system-unwind" fn shift(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: forwards to host memmove captured during import binding.
    unsafe { copied(to, from, n, REAL[1].load(Relaxed)) }
}

#[inline]
unsafe fn copied(to: *mut u8, from: *const u8, n: usize, real: usize) -> *mut u8 {
    // SAFETY: host function pointer stored by `import`.
    let real: CopyFn = unsafe { std::mem::transmute(real) };
    // SAFETY: caller's memcpy/memmove arguments.
    let out = unsafe { real(to, from, n) };
    let len = MARKS.len.load(Acquire);
    if len != 0 && n >= 4 {
        let start = MARKS.start.load(Relaxed);
        let to_addr = to as u64;
        let lo = to_addr.max(start);
        let hi = to_addr.saturating_add(n as u64).min(start + len);
        if lo < hi {
            COPIES[0].fetch_add(1, Relaxed);
            let mut slot = (lo - start).div_ceil(32);
            let mut marked_count = 0u64;
            while start + slot * 32 + 4 <= hi {
                let at = start + slot * 32;
                // SAFETY: inside `[lo, hi)`, freshly written by `real`.
                if unsafe { (at as *const u32).read_unaligned() } == crate::descriptor::VIEW_TAG {
                    mark_slot(slot as usize);
                    marked_count += 1;
                }
                slot += 1;
            }
            COPIES[1].fetch_add(marked_count, Relaxed);
        }
    }
    out
}

static MARKED_BY_US: AtomicU64 = AtomicU64::new(0);
static TAKEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
pub(crate) static CHECKED: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static MISSED_ALL: AtomicU64 = AtomicU64::new(0);
static WRITERS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
static ALL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Periodic diagnostic summary logged alongside `descriptor::sync_line`.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let [taken, takings, ns] = [0, 1, 2].map(|i| TAKEN[i].swap(0, Relaxed));
    let ours = MARKED_BY_US.swap(0, Relaxed);
    let [pages, missed, other, pending, check_ns, tabled] =
        [0, 1, 2, 3, 4, 5].map(|i| CHECKED[i].swap(0, Relaxed));
    let copies = [0, 1].map(|i| COPIES[i].swap(0, Relaxed));
    ALL[0].fetch_add(taken, Relaxed);
    ALL[1].fetch_add(ours, Relaxed);
    ALL[2].fetch_add(pages, Relaxed);
    if !working() {
        tracing::info!(
            "renderer port 2 (VICTORY_PORT_DESCRIPTORS): inactive, no descriptor store patched"
        );
        return;
    }
    let writers = {
        let by = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<(&String, &u64)> = by.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.iter()
            .take(6)
            .map(|(w, n)| format!("{w} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    tracing::info!(
        "renderer port 2 (VICTORY_PORT_DESCRIPTORS) per frame: {:.0} slots told ({:.0} ours, rest by {} stores) in {:.1} takings ({:.3} ms); \
         memcpy/memmove {:.0} calls ({:.0} marked); fallback: {:.1} pages, {:.2} missed ({:.2} tabled), {:.0} non-view, {:.1} pending, {:.2} ms; \
         totals: {} slots, {} pages, {} missed; writers: {}",
        taken as f64 / frames,
        ours as f64 / frames,
        IMAGE[2].load(Relaxed),
        takings as f64 / frames,
        ns as f64 / 1e6 / frames,
        copies[0] as f64 / frames,
        copies[1] as f64 / frames,
        pages as f64 / frames,
        missed as f64 / frames,
        tabled as f64 / frames,
        other as f64 / frames,
        pending as f64 / frames,
        check_ns as f64 / 1e6 / frames,
        ALL[0].load(Relaxed),
        ALL[2].load(Relaxed),
        MISSED_ALL.load(Relaxed),
        if writers.is_empty() {
            "none".into()
        } else {
            writers
        }
    );
}

/// Writers of the adopted heap found by protection (diagnostics,
/// `VICTORY_PORT_DESCRIPTORS_TRACE=1` with the port on): once a second the
/// heap is made read-only; the first write to each page faults, its
/// instruction is counted ([`TRACED`]) and the page made writable again.
/// Tells exactly which code writes slots, where the guard-fault notes of
/// [`faulted`] only remember a block's last writer.
#[cfg(windows)]
fn trace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PORT_DESCRIPTORS_TRACE").is_some_and(|v| v == "1"))
}

/// Instruction address -> page faults it took in the traced heap.
#[cfg(windows)]
static TRACED: Mutex<BTreeMap<usize, u64>> = Mutex::new(BTreeMap::new());
/// The traced range `[start, end)` while tracing (0, 0 otherwise).
#[cfg(windows)]
static TRACE_RANGE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

#[cfg(windows)]
fn start_trace(start: usize, len: usize) {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    if !trace_on() || TRACE_RANGE[1].load(Relaxed) != 0 {
        return;
    }
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != 1
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        if at < TRACE_RANGE[0].load(Relaxed) || at >= TRACE_RANGE[1].load(Relaxed) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let page = at & !0xfff;
        let mut old = 0u32;
        // SAFETY: a page of the traced heap, read-only by our hand.
        unsafe { VirtualProtect(page as *const _, 0x1000, PAGE_READWRITE, &mut old) };
        // Outside the title (a copy routine, ours or the system's): its
        // caller too, the return address at the top of its stack (the
        // copy routines keep no frame of their own).
        let rip = r.ExceptionAddress as usize;
        let (base, size) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
        // SAFETY: the faulting thread's context, given by the system.
        let rsp = unsafe { (*(*info).ContextRecord).Rsp } as usize;
        let caller = if rip >= base && rip < base + size {
            0
        } else {
            // SAFETY: the top of the faulting thread's own stack.
            unsafe { (rsp as *const usize).read() }
        };
        if let Ok(mut t) = TRACED.try_lock() {
            *t.entry(rip).or_default() += 1;
            if caller != 0 {
                *t.entry(caller | 1 << 63).or_default() += 1;
            }
        }
        EXCEPTION_CONTINUE_EXECUTION
    }
    TRACE_RANGE[0].store(start, Relaxed);
    TRACE_RANGE[1].store(start + len, Relaxed);
    // SAFETY: the handler only touches the traced heap's protection and a map.
    unsafe { AddVectoredExceptionHandler(1, Some(on_write)) };
    let _ = std::thread::Builder::new().name("victory-port2-trace".into()).spawn(move || {
        let mut n = 0u64;
        loop {
            let mut old = 0u32;
            // SAFETY: the adopted heap, kept mapped for the run.
            unsafe { VirtualProtect(start as *const _, len, PAGE_READONLY, &mut old) };
            std::thread::sleep(std::time::Duration::from_secs(1));
            n += 1;
            if n % 10 == 0 {
                let (base, size) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
                let mut all: Vec<(usize, u64)> = TRACED.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(a, n)| (*a, *n)).collect();
                all.sort_by(|a, b| b.1.cmp(&a.1));
                let top: Vec<String> = all
                    .iter()
                    .take(24)
                    .map(|&(rip, n)| {
                        let (called, rip) = (rip >> 63 == 1, rip & !(1 << 63));
                        let at = if rip >= base && rip < base + size { format!("title+{:#x}", rip - base) } else { format!("{rip:#x}") };
                        if called { format!("(called from {at}) {n}") } else { format!("{at} {n}") }
                    })
                    .collect();
                tracing::info!("renderer port 2 trace (VICTORY_PORT_DESCRIPTORS_TRACE): writers of the view heap's pages since the start: {}", top.join(", "));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_layout_matches_stub_offsets() {
        assert_eq!(size_of::<Marks>(), 32);
        assert_eq!(std::mem::offset_of!(Marks, start), 0);
        assert_eq!(std::mem::offset_of!(Marks, len), 8);
        assert_eq!(std::mem::offset_of!(Marks, slots), 16);
        assert_eq!(std::mem::offset_of!(Marks, chunks), 24);
    }

    #[test]
    fn marked_slots_drain_once_as_merged_runs() {
        let start = 0x1000_0000u64;
        assert!(adopt_now(start, 1000));
        assert!(adopted(start) && !adopted(start + 32));
        {
            let a = ADOPTED.read().unwrap();
            let a = a.as_ref().unwrap();
            a.slots[5].store(1, Release);
            a.chunks[5 / CHUNK].store(1, Release);
            a.slots[999].store(1, Release);
            a.chunks[999 / CHUNK].store(1, Release);
        }
        mark(start + 6 * 32 + 7);
        mark(start + 300 * 32);
        mark(start - 32);
        mark(start + 1000 * 32);
        assert_eq!(take(start).unwrap(), [(5, 2), (300, 1), (999, 1)]);
        assert_eq!(take(start).unwrap(), []);
        assert!(take(start + 32).is_none());
        let_go(start);
        mark(start + 5 * 32);
        assert!(take(start).is_none());
        assert_eq!(MARKS.len.load(Relaxed), 0);
    }

    #[test]
    fn fallback_warn_rate_limits_spam() {
        assert!(should_warn_missed(0));
        assert!(should_warn_missed(7));
        assert!(!should_warn_missed(8));
    }
}
