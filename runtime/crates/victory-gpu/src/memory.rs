//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! Numbers are recycled: the structure builder alone names thousands of
//! buffers over a run, so when the [`COPIES`] numbers run out, the copy
//! unused for the longest gives its number up (its directory entries go
//! back to empty; a page still wanted faults back in, a frame late).
//! Bringing copies up to date is bounded the same way: a dispatch's list
//! refreshes the copies the dispatch names (plus the ones that faulted
//! recently and the ones numbered since its last submission), not all of
//! them, and declares writes only for the named ones — the builder
//! dispatches hundreds of times a frame, and refreshing every copy in
//! every list exhausted the host before recycling was in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows; the
/// ray-tracing structure builder names over a thousand 4 MiB windows while
/// the world streams in, and every numbered window is pinned by the heap
/// slots that show it — 1023 of them meant 5.5 GB held at once, which with
/// the game's own growth ran the PC out of commit (kimi-run11). 511 caps
/// the pin at about 2 GB; what falls out comes back through its fault.
const COPIES: u32 = 511;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// The frame copy `n` was last named in (parallel to `copies`): a
    /// dispatch that names it needs it now, so the number is only recycled
    /// for a copy no dispatch named this frame and whose last write-back
    /// the card has run (`flushed`, `recycle`).
    used: Vec<u64>,
    /// The frame copy `n` last gained a page through a fault (parallel to
    /// `copies`, 0: never). Faults say a shader missed the copy; it is
    /// refreshed for a few frames after, when the miss is likely repeated.
    faulted: Vec<u64>,
    /// Per host view heap (by address): its run of slots.
    heaps: HashMap<usize, HeapRun>,
    /// The frame the faults were last read in.
    frame: u64,
    /// The submission count (`host::started`) when copy `n` was last
    /// declared written back to title memory (parallel to `copies`, 0:
    /// never). A number can only be recycled once the card has run that
    /// submission: earlier the old copy's freshest bytes are still on their
    /// way, the new copy's refresh would read stale ones, and the title
    /// reads the mess back over its own data (kimi-run13's guest crash).
    flushed: Vec<u64>,
    /// Numbers [`Memory::recycle`] freed at the start of this frame, for
    /// copies named during it.
    free: Vec<usize>,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
}

/// A heap's run of [`COPIES` + 1] slots: slot 0 the null view, slot
/// `base + 1 + n` copy `n`'s raw view.
struct HeapRun {
    base: u32,
    /// Which copy's view slot `n` holds, by the copy's id (0: none yet).
    /// Numbers are recycled, so a slot is rewritten when its number's copy
    /// changes, not only when a number is first used.
    slots: Vec<u64>,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            used: Vec::new(),
            faulted: Vec::new(),
            flushed: Vec::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            free: Vec::new(),
            refused: Default::default(),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's, and notes the copy as used in `frame`.
    fn map(&mut self, address: u64, frame: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        let existing = unsafe { entry.read() };
        if existing != 0 {
            self.used[existing as usize - 1] = frame;
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => {
                // A freed number whose copy is asked for again: taken back.
                if let Some(i) = self.free.iter().position(|&f| f == n) {
                    self.free.swap_remove(i);
                }
                n
            }
            None if self.copies.len() < COPIES as usize => {
                self.copies.push(shadow.clone());
                self.used.push(0);
                self.faulted.push(0);
                self.flushed.push(0);
                self.copies.len() - 1
            }
            None => match self.free.pop() {
                Some(n) => {
                    // Retired, not dropped: the settle in `recycle` covers
                    // the lists that could read the old copy, and `retire`
                    // is the belt for a submission that slipped in after.
                    let old = std::mem::replace(&mut self.copies[n], shadow.clone());
                    crate::host::retire(old);
                    self.faulted[n] = 0;
                    self.flushed[n] = 0;
                    n
                }
                None => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!(COPIES, "title memory by address: no number free this frame; a new copy's pages read zeros until the next frame's recycling")
                    });
                    return;
                }
            },
        };
        self.used[n] = frame;
        // Every page the copy holds whole, not only the one asked
        // for: a structure is more than 64 KiB.
        // shortcut: a page of a title buffer inside this one that
        // has a copy of its own is given this copy too.
        let (first, len) = shadow.span();
        let mut at = first.next_multiple_of(PAGE);
        while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
            // SAFETY: inside the directory's mapping.
            let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
            // SAFETY: as above; the offset first, as below.
            unsafe {
                if other.read() == 0 {
                    other.add(1).write_volatile((at - first) as u32);
                    other.write_volatile(n as u32 + 1);
                }
            }
            at += PAGE;
        }
        let offset = shadow.offset_in(start).unwrap_or(0);
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset as u32);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// At the start of a frame, when the numbers have run out: frees the
    /// copies unused for the longest (a quarter of the numbers), for the
    /// copies the frame names. Their directory entries go back to empty, so
    /// their pages read the null view again; a page a shader still wants
    /// comes back through its fault, a frame late.
    ///
    /// The directory and the heap slots are read by lists when the card
    /// runs them, not when they are recorded. Freeing a number during a
    /// frame let lists recorded before it, still to be submitted, reach the
    /// number's next copy: the builder writes through pointers it reads out
    /// of memory (no copy named for them), and its bounding boxes (-inf
    /// floats, 0xff800000) landed in another title buffer, which the
    /// write-back then gave the title (kimi-run13/14's guest crash on
    /// pointers 0xff800008). So numbers are only freed here, between frames,
    /// once the card has run every list sent so far; a copy asked for during
    /// a frame with no number free reads zeros until the next one.
    /// shortcut: a list recorded in the frame before and submitted after its
    /// end is not waited for.
    fn recycle(&mut self, frame: u64) {
        let want = (COPIES / 4) as usize;
        if self.copies.len() < COPIES as usize || self.free.len() >= want {
            return;
        }
        if crate::host::started() > crate::host::finished() {
            crate::host::settle_for_recycle();
        }
        let done = crate::host::finished();
        let mut order: Vec<usize> = (0..self.copies.len())
            .filter(|n| !self.free.contains(n) && self.used[*n] < frame && self.flushed[*n] <= done)
            .collect();
        order.sort_by_key(|&n| self.used[n]);
        let take = want - self.free.len();
        for n in order.into_iter().take(take) {
            let (first, len) = self.copies[n].span();
            // Every page of the span, not only the aligned ones `map` filled:
            // the page first asked for may sit at the span's edge.
            let last = first + len.saturating_sub(1);
            let mut at = first / PAGE;
            while at <= last / PAGE && at < u64::from(MEMORY_PAGES) - 1 {
                // SAFETY: inside the directory's mapping.
                let entry = unsafe { self.directory.1.add(at as usize * 2) };
                // SAFETY: as above. Only this copy's pages: a page of a buffer
                // inside the span may carry another copy's number.
                unsafe {
                    if entry.read() == n as u32 + 1 {
                        entry.write_volatile(0);
                        entry.add(1).write_volatile(0);
                    }
                }
                at += 1;
            }
            self.faulted[n] = 0;
            self.free.push(n);
            // How many numbers were recycled: the first sign of thrash when a
            // game's live structures outgrow the directory.
            static RECYCLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let recycled = RECYCLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if recycled.is_power_of_two() {
                tracing::info!(recycled, "title memory by address: copy numbers recycled so far");
            }
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self, frame: u64) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE, frame);
            // The copy the page landed in is refreshed for a few frames:
            // the shader that missed it is likely to run again.
            // SAFETY: inside the directory's mapping.
            let entry = unsafe { self.directory.1.add(page as usize * 2) };
            // SAFETY: as above.
            let n = unsafe { entry.read() };
            if n != 0 {
                self.faulted[n as usize - 1] = frame;
            }
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64, frame: u64) {
        self.map(address, frame);
        self.map((address & !(CELL - 1)) + CELL, frame);
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run, and the copies this dispatch names (and
/// the ones a shader missed lately) are brought up to date when `list` is
/// submitted (and, with `writes`, the named ones copied back after). None
/// if the host cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.recycle(frame);
        m.take_faults(frame);
    }
    let numbered = m.copies.len();
    for &address in addresses {
        m.name(address, frame);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32, frame);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8, frame);
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let run = match m.heaps.get_mut(&key) {
        Some(run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            heap.null_raw_uav_at(base);
            m.heaps.entry(key).or_insert(HeapRun { base, slots: Vec::new() })
        }
    };
    for (n, shadow) in m.copies.iter().enumerate() {
        // Rewritten when the slot is new or its number was recycled to
        // another copy; the null view at slot 0 never changes.
        if run.slots.get(n).copied() != Some(shadow.id()) {
            let (_, len) = shadow.span();
            heap.raw_uav_at(run.base + 1 + n as u32, &shadow.buffer, 0, len);
            if run.slots.len() <= n {
                run.slots.resize(n + 1, 0);
            }
            run.slots[n] = shadow.id();
        }
    }
    let base = run.base;
    // A list refreshes the copies this dispatch names (its reads and
    // writes reach them), the ones a shader missed in the last few frames
    // (it is likely to run again) and the ones numbered since its last
    // submission (never refreshed here before); writes are declared for
    // the named ones. The builder dispatches hundreds of times a frame:
    // refreshing every copy in every list exhausted the host.
    // shortcut: a copy a shader writes through a pointer it read out of
    // memory, without naming the copy, is not declared written; its bytes
    // catch up at the next whole-copy flush of the shadow copies.
    /// How many frames a fault keeps a copy refreshed.
    const FAULT_RECENT: u64 = 3;
    /// `first_use` keys for the per-list write declarations, one per copy
    /// number: a run of its own, below every other fixed key (`WRITE - n`
    /// from `usize::MAX - 0x6e` down reached the key `usize::MAX - 0x77`).
    const WRITE: usize = usize::MAX - 0x1_0000;
    for (n, shadow) in m.copies.iter().enumerate() {
        let (start, len) = shadow.span();
        let named = m.used[n] == frame;
        let missed = m.faulted[n] != 0 && frame - m.faulted[n] <= FAULT_RECENT;
        if named || missed || n >= numbered {
            shadow.refresh(list, start, len);
        }
        if writes && (named || n >= numbered) && list.first_use(WRITE - n) {
            shadow.gpu_writes(list, start, len);
            m.flushed[n] = crate::host::started();
        }
    }
    if list.first_use(usize::MAX - 0x6d) {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}
