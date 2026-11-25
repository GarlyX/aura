//! Title memory: one physical allocation seen at two addresses.
//!
//! The console yeni çıkmış bir oyun (the newly released game this layer
//! runs, called "the title" below) was built for has unified memory: GPU
//! resources are ordinary allocations in the title's address space. On the host we must
//! not copy that memory per frame (the reference layer's multi-gigabyte
//! shadow is what paged the test machine to death). So aura keeps *one*
//! physical allocation — a pagefile-backed section — and maps it twice:
//!
//! - the **title view**, at the address the title's code uses, and
//! - the **layer view**, the same pages at a second address, for our GPU
//!   upload path to read.
//!
//! Change detection follows the architecture: OS write-watch on the title
//! view is the only change signal; hashing is the fallback for cases the
//! watch cannot see. Two of those cases are real:
//!
//! - the OS may refuse (or silently drop) `MEM_WRITE_WATCH` on a section
//!   view — detected by a probe at construction, reported by
//!   [`TitleMem::is_watched`], and
//! - writes through the *layer* view are invisible to a watch on the title
//!   view. That blind spot is deliberate: our own writes (DirectStorage
//!   landings, resource fix-ups) must not masquerade as title writes, or we
//!   would re-upload our own work every frame.
//!
//! The hash fallback ([`IncrementalHash`]) is incremental by design: a
//! rotating slice per call, with slices that keep changing (video playback
//! targets) escalating to every-call scans until they settle.

use std::ops::Range;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_RESERVE, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, MapViewOfFileEx, PAGE_READWRITE, SEC_RESERVE, UnmapViewOfFile, VirtualAlloc,
};
use windows_sys::Win32::System::SystemServices::MEM_WRITE_WATCH;

use crate::watch;

/// One physical allocation, two addresses.
///
/// Dropping unmaps both views and closes the section. Both pointers stay
/// valid for the whole lifetime.
pub struct TitleMem {
    section: HANDLE,
    title: *mut u8,
    layer: *mut u8,
    size: usize,
    /// True when the probe at construction proved the OS records writes to
    /// the title view. False routes the caller to [`IncrementalHash`].
    watched: bool,
}

// Plain memory plus an OS handle; the owner may move between threads.
unsafe impl Send for TitleMem {}
unsafe impl Sync for TitleMem {}

impl TitleMem {
    /// Reserves `size` bytes of title memory. Nothing is committed; pages
    /// are committed in chunks with [`TitleMem::commit`] as the title
    /// allocates, so an 8 GiB console heap does not charge the host's
    /// commit budget up front.
    pub fn new(size: usize) -> Option<Self> {
        if size == 0 {
            return None;
        }
        // A section, not a bare VirtualAlloc reservation: a section is what
        // can appear at two addresses at once.
        // SAFETY: INVALID_HANDLE_VALUE selects the page file; SEC_RESERVE
        // defers the commit charge until pages are committed inside a view.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                (size >> 32) as u32,
                (size & 0xffff_ffff) as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            tracing::warn!(error = unsafe { GetLastError() }, "CreateFileMappingW failed");
            return None;
        }

        // Title view, asking for write tracking. The OS may refuse the flag
        // (ERROR_INVALID_PARAMETER) or take it and track nothing; both end
        // at `watched: false` via the probe below.
        let title = map_view(section, size, MEM_RESERVE | MEM_WRITE_WATCH)
            .or_else(|| map_view(section, size, MEM_RESERVE));
        let Some(title) = title else {
            // SAFETY: the section handle is ours.
            unsafe { CloseHandle(section) };
            return None;
        };
        // Layer view: same pages, second address, no tracking wanted.
        let layer = map_view(section, size, MEM_RESERVE);
        let Some(layer) = layer else {
            // SAFETY: the view and the handle are ours.
            unsafe {
                UnmapViewOfFile(title);
                CloseHandle(section);
            }
            return None;
        };

        let mut this = Self {
            section,
            title: title.Value.cast(),
            layer: layer.Value.cast(),
            size,
            watched: false,
        };
        this.probe_watch();
        Some(this)
    }

    /// Commits `[offset, offset + len)` in both views. Idempotent; bounds
    /// are checked against the region size. False on bad range or OS
    /// failure.
    pub fn commit(&self, offset: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        if end > self.size {
            return false;
        }
        // Each view of a SEC_RESERVE section commits its own pages.
        // SAFETY: both ranges lie inside live mapped views.
        let a = unsafe {
            VirtualAlloc(
                self.title.add(offset).cast(),
                len,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if a.is_null() {
            return false;
        }
        // SAFETY: same range in the second view.
        let b = unsafe {
            VirtualAlloc(
                self.layer.add(offset).cast(),
                len,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        !b.is_null()
    }

    /// The address the title's code uses.
    pub fn title_ptr(&self) -> *mut u8 {
        self.title
    }

    /// The same pages at the address our upload path reads through.
    pub fn layer_ptr(&self) -> *mut u8 {
        self.layer
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Whether the OS proved at construction that it tracks title-view
    /// writes. When false, use [`IncrementalHash`] on the layer view.
    pub fn is_watched(&self) -> bool {
        self.watched
    }

    /// Title writes since the last [`TitleMem::reset`], as offsets. `None`
    /// when this region is not watched — never an empty guess.
    pub fn writes(&self) -> Option<Vec<Range<usize>>> {
        self.watched
            .then(|| watch::collect_writes(self.title, self.size))
            .flatten()
    }

    /// Forgets every write seen so far; tracking continues from now.
    /// Harmless when the region is not watched.
    pub fn reset(&self) {
        if !self.watched {
            return;
        }
        // SAFETY: the view is live; resets tracking bits only.
        let rc = unsafe { windows_sys::Win32::System::Memory::ResetWriteWatch(self.title.cast(), self.size) };
        if rc != 0 {
            tracing::warn!(error = unsafe { GetLastError() }, "ResetWriteWatch failed");
        }
    }

    /// Decides once, honestly, whether the watch works: commit one page,
    /// write through the title view, ask. A silent no-op watch is worse
    /// than none, so the answer comes from the kernel, not from the flag
    /// the mapping call accepted. The probe dirties and then clears page 0;
    /// the tracking state is reset so the caller's first `writes()` is
    /// clean.
    fn probe_watch(&mut self) {
        if !self.commit(0, watch::page_size()) {
            return;
        }
        // SAFETY: page 0 is committed and inside the region. Flip a byte
        // and put it back: the value is unchanged, the page is dirtied
        // either way, which is all the probe needs.
        unsafe {
            let old = *self.title;
            *self.title = old ^ 0xFF;
            *self.title = old;
        }
        let seen = watch::collect_writes(self.title, watch::page_size())
            .map(|w| !w.is_empty())
            .unwrap_or(false);
        if seen {
            self.watched = true;
            self.reset();
        }
    }
}

impl Drop for TitleMem {
    fn drop(&mut self) {
        // SAFETY: both views and the section handle are ours, released once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.title.cast(),
            });
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.layer.cast(),
            });
            CloseHandle(self.section);
        }
    }
}

/// Maps the whole section with the given allocation flags, address chosen
/// by the OS. Fixed title addresses come later, when the loader owns the
/// title's memory map.
fn map_view(section: HANDLE, size: usize, flags: u32) -> Option<MEMORY_MAPPED_VIEW_ADDRESS> {
    // SAFETY: the section is live; null base lets the OS place the view.
    let view = unsafe {
        MapViewOfFile3(
            section,
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
            size,
            flags,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if !view.Value.is_null() {
        return Some(view);
    }
    // Older builds may lack MapViewOfFile3 in the API set; the classic call
    // cannot take MEM_WRITE_WATCH, so the probe will simply answer false.
    if flags & MEM_WRITE_WATCH != 0 {
        return None;
    }
    // SAFETY: the section is live; null base lets the OS place the view.
    let view = unsafe { MapViewOfFileEx(section, FILE_MAP_ALL_ACCESS, 0, 0, size, std::ptr::null()) };
    (!view.Value.is_null()).then_some(view)
}

/// Fallback change signal for unwatched title memory: rehash a rotating
/// slice per call, and re-scan every slice that was dirty at its last scan
/// until it comes back clean.
///
/// Everything starts hot, so the first call reports the whole region —
/// that is the first upload. A slice that keeps changing (a video target)
/// stays on the every-call list; one clean scan cools it back to the
/// rotation. Escalation is therefore immediate once the rotation has seen
/// a slice change once, never a lap late.
pub struct IncrementalHash {
    base: *const u8,
    len: usize,
    slice: usize,
    hashes: Vec<Option<u64>>,
    hot: Vec<bool>,
    cursor: usize,
}

// Reads only; the owning handle may move between threads.
unsafe impl Send for IncrementalHash {}
unsafe impl Sync for IncrementalHash {}

impl IncrementalHash {
    /// `slice` is the scan granularity in bytes; one page minimum.
    pub fn new(base: *const u8, len: usize, slice: usize) -> Self {
        let slice = slice.max(watch::page_size());
        let count = len.div_ceil(slice);
        Self {
            base,
            len,
            slice,
            hashes: vec![None; count],
            hot: vec![true; count],
            cursor: 0,
        }
    }

    /// Scans the hot slices plus up to `budget` further slices in rotation;
    /// returns the changed ranges as offsets, merged where slices touch.
    pub fn step(&mut self, budget: usize) -> Vec<Range<usize>> {
        let count = self.hashes.len();
        let mut out: Vec<Range<usize>> = Vec::new();
        for i in 0..count {
            if self.hot[i] {
                self.scan(i, &mut out);
            }
        }
        let mut spent = 0;
        // `walked` caps the lap: when everything is hot the rotation finds
        // nothing to scan and must not spin forever.
        let mut walked = 0;
        while spent < budget && walked < count {
            let i = self.cursor;
            self.cursor = (self.cursor + 1) % count;
            walked += 1;
            if self.hot[i] {
                continue; // already scanned above
            }
            self.scan(i, &mut out);
            spent += 1;
        }
        merge(&mut out);
        out
    }

    /// Marks every slice for a scan on the next call. For bulk landings we
    /// did not track (a DirectStorage batch we wrote through the layer view
    /// ourselves is *not* such an event — those pages the title never
    /// wrote).
    pub fn mark_all_dirty(&mut self) {
        self.hot.fill(true);
    }

    fn scan(&mut self, i: usize, out: &mut Vec<Range<usize>>) {
        let start = i * self.slice;
        let end = (start + self.slice).min(self.len);
        // SAFETY: the caller keeps the region alive and mapped for reads.
        let bytes = unsafe { std::slice::from_raw_parts(self.base.add(start), end - start) };
        let h = hash(bytes);
        if self.hashes[i] == Some(h) {
            self.hot[i] = false;
        } else {
            self.hashes[i] = Some(h);
            self.hot[i] = true;
            out.push(start..end);
        }
    }
}

/// Dependency-free, process-local change hash. SipHash (std's default) is
/// not the fastest choice per byte, but this path runs on a slice per call,
/// not on gigabytes — that is the whole point of the rotation.
fn hash(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

fn merge(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_unstable_by_key(|r| r.start);
    ranges.dedup();
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    std::mem::swap(ranges, &mut merged);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    #[test]
    fn aliases_see_the_same_physical_pages() {
        let m = TitleMem::new(MIB).unwrap();
        assert!(m.commit(0, 2 * watch::page_size()));
        // SAFETY: the pages are committed in both views.
        unsafe {
            *m.title_ptr().add(123) = 0xAB;
            assert_eq!(*m.layer_ptr().add(123), 0xAB);
            *m.layer_ptr().add(watch::page_size()) = 0xCD;
            assert_eq!(*m.title_ptr().add(watch::page_size()), 0xCD);
        }
    }

    #[test]
    fn watch_flag_matches_observed_behavior() {
        let m = TitleMem::new(MIB).unwrap();
        // Whatever the probe decided, a fresh write must agree with it:
        // watched regions report, unwatched ones say None (never a guess).
        // SAFETY: page 0 was committed by the probe.
        unsafe { *m.title_ptr() = 7 };
        match (m.is_watched(), m.writes()) {
            (true, Some(w)) => assert!(!w.is_empty()),
            (false, None) => {}
            other => panic!("watch flag disagrees with behavior: {other:?}"),
        }
    }

    #[test]
    fn our_own_layer_writes_are_not_title_writes() {
        let m = TitleMem::new(MIB).unwrap();
        if !m.is_watched() {
            return; // unwatched path: nothing to prove here
        }
        m.reset();
        // SAFETY: page 0 is committed in both views.
        unsafe { *m.layer_ptr() = 9 };
        let w = m.writes().unwrap();
        assert!(w.is_empty(), "layer-view writes must stay invisible: {w:?}");
    }

    #[test]
    fn commit_bounds_are_checked() {
        let m = TitleMem::new(MIB).unwrap();
        assert!(!m.commit(MIB, 1));
        assert!(!m.commit(usize::MAX - 1, 4));
        assert!(m.commit(MIB - watch::page_size(), watch::page_size()));
    }

    #[test]
    fn zero_size_is_refused() {
        assert!(TitleMem::new(0).is_none());
    }

    #[test]
    fn first_rotation_reports_everything() {
        let m = TitleMem::new(MIB).unwrap();
        assert!(m.commit(0, MIB));
        let mut h = IncrementalHash::new(m.layer_ptr(), MIB, MIB / 4);
        let out = h.step(4);
        assert_eq!(out, vec![0..MIB]);
    }

    #[test]
    fn rotation_finds_a_write_within_one_lap() {
        let m = TitleMem::new(MIB).unwrap();
        assert!(m.commit(0, MIB));
        let mut h = IncrementalHash::new(m.layer_ptr(), MIB, MIB / 4);
        let _ = h.step(4); // baseline
        // SAFETY: committed memory.
        unsafe { *m.title_ptr().add(MIB / 2) = 1 };
        let mut found = Vec::new();
        for _ in 0..4 {
            found.extend(h.step(1));
        }
        merge(&mut found);
        assert_eq!(found, vec![MIB / 2..3 * MIB / 4]);
    }

    #[test]
    fn repeatedly_changed_slice_goes_hot_then_cools() {
        let m = TitleMem::new(MIB).unwrap();
        assert!(m.commit(0, MIB));
        let mut h = IncrementalHash::new(m.layer_ptr(), MIB, MIB / 4);
        let _ = h.step(4); // first lap records baselines, reports all
        let _ = h.step(4); // second lap finds nothing changed: all cool
        assert!(h.hot.iter().all(|x| !x));
        // Change slice 1: the rotation finds it within one lap, and it
        // turns hot.
        // SAFETY: committed memory.
        unsafe { *m.title_ptr().add(MIB / 4) = 10 };
        let out = h.step(2); // cursor walks slices 0 and 1
        assert!(out.contains(&(MIB / 4..MIB / 2)), "{out:?}");
        assert!(h.hot[1]);
        // From now on the every-call pass reports it even with budget 0.
        // SAFETY: committed memory.
        unsafe { *m.title_ptr().add(MIB / 4) = 11 };
        let out = h.step(0);
        assert_eq!(out, vec![MIB / 4..MIB / 2]);
        // Settle: one clean scan cools it.
        let out = h.step(0);
        assert!(out.is_empty(), "{out:?}");
        assert!(!h.hot[1]);
    }
}
