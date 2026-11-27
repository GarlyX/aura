//! Notes of the view-heap slots a shader picks (`SLOT_HEAP_USED`): the host
//! keeps host copies only for buffer views shaders were seen to pick lately.

use super::*;

impl Lower<'_> {
    /// Whether a descriptor comes from a load out of the view heap itself.
    pub(super) fn from_heap(&self, src: &Source) -> bool {
        src.leaves().into_iter().any(|l| {
            matches!(*l, Source::Loaded { load, .. } | Source::Fetched { load, .. }
                if self.scalar.get(&load).and_then(|a| self.x.native.layout.get(a.param))
                    .is_some_and(|p| p.root == RootType::ResourceHeap))
        })
    }

    /// Notes that the shader picks view-heap slot `index` (`SLOT_HEAP_USED`).
    /// Not for a table's slots (and, but for plain heap loads, not in pixel
    /// shaders): a store there would cost the pixel shader its early depth test.
    ///
    /// Off unless `VICTORY_HEAP_PICKED=1` (host side: `draw.rs heap_buffers`):
    /// with it on, gameplay went from 3.0 to 1.4 fps (aura-run2: the stores
    /// go to a buffer in the PC's memory), and a form that stored once per
    /// wave behind a branch stopped every pipeline from being made
    /// (aura-run3). A card-memory buffer copied back once a frame is the next
    /// step.
    pub(super) fn mark_heap_use(&mut self, index: V) {
        if !heap_picked() {
            return;
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let last = self.s.m.i32(super::super::shader::SLOT_STRIDES_COUNT - 1);
        let slot = self.s.op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[index, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let two = self.s.m.i32(2);
        let byte = self.s.m.bin(c, Bin::Shl, slot, two);
        let h = self.s.root_uav_handle(super::super::shader::SLOT_HEAP_USED);
        let (iu, one, mask, align) = (self.s.m.undef(i32t), self.s.m.i32(1), self.s.m.i8(1), self.s.m.i32(4));
        let void = self.s.t.void;
        self.s.op(c, "rawBufferStore.i32", Attr::NoUnwind, void, 140, &[h, byte, iu, one, iu, iu, iu, mask, align]);
    }
}

/// `VICTORY_HEAP_PICKED=1`: shaders note the heap slots they pick.
fn heap_picked() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_PICKED").is_some_and(|v| v == "1"))
}
