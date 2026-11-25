//! Tracking which pages of a region the title (or we) wrote.
//!
//! GPU resources live in title memory; to upload what changed we must know
//! what was written. The OS can track writes for us
//! (`MEM_WRITE_WATCH` + `GetWriteWatch`): every store through the region
//! faults once per page per interval and is recorded, without any hashing
//! or shadow copy. This module is that primitive.
//!
//! The known blind spot, carried over from the reference designs this layer
//! studies: the OS tracks the pages of *this* allocation. A write to the
//! same physical pages through a second mapping made elsewhere is not seen;
//! regions that can be aliased that way need an incremental-hash fallback
//! (planned, not implemented).

use std::ops::Range;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Memory::{
    GetWriteWatch, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, ResetWriteWatch,
    VirtualAlloc, VirtualFree,
};
use windows_sys::Win32::System::SystemServices::MEM_WRITE_WATCH;

/// A committed region with OS write tracking.
///
/// Dropping it frees the region. The pointer handed out by [`Watch::ptr`]
/// is valid for the region's whole lifetime.
pub struct Watch {
    base: *mut u8,
    size: usize,
}

// The region is plain memory; the owning handle may move between threads.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

impl Watch {
    /// Commits `size` bytes with write tracking on. `None` if the commit
    /// fails (out of memory).
    pub fn new(size: usize) -> Option<Self> {
        if size == 0 {
            return None;
        }
        // SAFETY: no address preference; the flags ask for write tracking.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                size,
                MEM_COMMIT | MEM_RESERVE | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        (!base.is_null()).then_some(Self {
            base: base.cast(),
            size,
        })
    }

    /// Base of the region; writable through any thread.
    pub fn ptr(&self) -> *mut u8 {
        self.base
    }

    /// Region size in bytes (as requested).
    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Ranges (offsets into the region) written since the last reset,
    /// merged where written pages touch.
    ///
    /// The scratch array has one slot per page of the region, so a
    /// concurrent writer cannot overflow it: there is a slot for every page
    /// it could possibly dirty.
    pub fn writes(&self) -> Vec<Range<usize>> {
        let page = page_size();
        let mut pages = vec![std::ptr::null_mut::<u8>(); self.size.div_ceil(page)];
        let mut count = pages.len();
        let mut gran = 0u32;
        // SAFETY: `pages` has room for `count` addresses and the region is
        // live for the call.
        let rc = unsafe {
            GetWriteWatch(
                0,
                self.base.cast(),
                self.size,
                pages.as_mut_ptr().cast(),
                &mut count,
                &mut gran,
            )
        };
        if rc != 0 {
            tracing::warn!(error = unsafe { GetLastError() }, "GetWriteWatch failed");
            return Vec::new();
        }
        pages.truncate(count);
        pages.sort_unstable();
        pages.dedup();
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for p in pages {
            let start = p as usize - self.base as usize;
            let end = (start + page).min(self.size);
            match ranges.last_mut() {
                Some(last) if start <= last.end => last.end = last.end.max(end),
                _ => ranges.push(start..end),
            }
        }
        ranges
    }

    /// Forgets every write seen so far; tracking continues from now.
    pub fn reset(&self) {
        // SAFETY: the region is live; resets the tracking bits only.
        let rc = unsafe { ResetWriteWatch(self.base.cast(), self.size) };
        if rc != 0 {
            tracing::warn!(error = unsafe { GetLastError() }, "ResetWriteWatch failed");
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the region is ours and freed once.
        unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
    }
}

fn page_size() -> usize {
    use std::sync::OnceLock;
    static PAGE: OnceLock<usize> = OnceLock::new();
    *PAGE.get_or_init(|| {
        let mut info = windows_sys::Win32::System::SystemInformation::SYSTEM_INFO::default();
        // SAFETY: the out pointer is valid.
        unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info) };
        info.dwPageSize as usize
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_written_nothing_reported() {
        let w = Watch::new(1 << 20).unwrap();
        assert!(w.writes().is_empty());
    }

    #[test]
    fn a_write_is_reported_and_reset_clears_it() {
        let w = Watch::new(1 << 20).unwrap();
        // SAFETY: 64 KiB in, the region is 1 MiB.
        unsafe { *w.ptr().add(64 << 10) = 42 };
        let ranges = w.writes();
        assert_eq!(ranges.len(), 1);
        let r = &ranges[0];
        assert!(r.start <= 64 << 10 && r.end > 64 << 10, "{r:?}");
        w.reset();
        assert!(w.writes().is_empty());
    }

    #[test]
    fn touching_pages_merge_into_one_range() {
        let w = Watch::new(1 << 20).unwrap();
        let page = page_size();
        // SAFETY: the three pages lie inside the region.
        unsafe {
            *w.ptr().add(page) = 1;
            *w.ptr().add(2 * page) = 1;
            *w.ptr().add(3 * page) = 1;
        }
        let ranges = w.writes();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], page..4 * page);
    }

    #[test]
    fn distant_writes_stay_separate_ranges() {
        let w = Watch::new(1 << 20).unwrap();
        let page = page_size();
        // SAFETY: both offsets lie inside the region.
        unsafe {
            *w.ptr() = 1;
            *w.ptr().add(100 * page) = 1;
        }
        let ranges = w.writes();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], 0..page);
        assert_eq!(ranges[1], 100 * page..101 * page);
    }

    #[test]
    fn zero_size_is_refused() {
        assert!(Watch::new(0).is_none());
    }
}
