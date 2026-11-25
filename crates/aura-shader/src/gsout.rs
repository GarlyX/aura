//! What a geometry program reads and emits, for the mesh-shader translation.
//!
//! On the console a geometry wave runs with the stage before it on the same
//! compute unit: the earlier stage leaves its vertices in LDS, the geometry
//! program reads its primitive's vertices from there, writes each output
//! vertex into a per-lane LDS region, and tells the hardware with
//! `s_sendmsg MSG_GS EMIT` that a vertex is complete (`CUT` ends a strip,
//! `GS_DONE` ends the wave). A mesh shader has no such protocol: it writes
//! vertices and primitives to output arrays directly. This module recovers
//! the protocol's content from a program in SSA form:
//!
//! - **outputs**: for each EMIT, which value lands in which dword of the
//!   vertex (the vertex layout is the same for every vertex: one stride in
//!   the lane's region), and how CUTs group vertices into strips;
//! - **inputs**: each distinct LDS address the program reads its input
//!   vertices through, and the dwords it reads at each.
//!
//! The geometry programs of yeni çıkmış bir oyun (the newly released game
//! this layer runs) that fail to translate today are straight-line code,
//! one input triangle per lane and one output triangle per lane; this is
//! the shape handled here, and anything else is reported rather than
//! guessed.

use std::collections::BTreeMap;

use crate::cfg::Cfg;
use crate::gfx10::{GsOp, Message};
use crate::operands::Unit;
use crate::ssa::{Ssa, ValueId};

/// Why a geometry program's output could not be recovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GsError {
    /// Branches: not the straight-line shape handled yet.
    Branches,
    /// LDS writes through more than one base address register value.
    SeveralOutputBases { at: usize },
    /// The vertex stride cannot be told from the writes.
    NoStride,
    /// A write of vertex `vertex` falls outside its stride slot.
    OutsideVertex { at: usize, vertex: usize },
    /// Nothing emitted.
    NoVertices,
}

/// One vertex the program emits: (dword within the vertex, value).
pub type Vertex = BTreeMap<u32, ValueId>;

/// An input vertex: the LDS address value it is read through, and the
/// dwords read (dword offset from that address → the value read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputVertex {
    pub address: ValueId,
    pub dwords: BTreeMap<u32, ValueId>,
}

/// The recovered output and input protocol of a geometry program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsProgram {
    /// The register value every output write is addressed through (the
    /// lane's region in the output ring).
    pub output_base: ValueId,
    /// Dwords between consecutive vertices of one lane.
    pub vertex_stride: u32,
    pub vertices: Vec<Vertex>,
    /// Vertex indices of each strip (a CUT ends one; GS_DONE the last).
    pub strips: Vec<Vec<usize>>,
    pub inputs: Vec<InputVertex>,
}

/// Recovers the protocol from a straight-line geometry program.
pub fn analyse(cfg: &Cfg, ssa: &Ssa) -> Result<GsProgram, GsError> {
    if cfg.blocks.len() != 1 {
        return Err(GsError::Branches);
    }
    // Writes per emitted vertex: (instruction, dword offset, value).
    let mut pending: Vec<(usize, u32, ValueId)> = Vec::new();
    let mut emitted: Vec<Vec<(usize, u32, ValueId)>> = Vec::new();
    let mut strips = vec![Vec::new()];
    let mut base: Option<ValueId> = None;
    let mut inputs: Vec<InputVertex> = Vec::new();
    let value_of = |i: usize, u: Unit| ssa.uses[i].iter().find(|(x, _, _)| *x == u).map(|e| e.2);
    for (i, inst) in cfg.insts.iter().enumerate() {
        if let Some(msg) = inst.message() {
            match msg {
                Message::Gs { op: GsOp::Emit | GsOp::EmitCut, .. } => {
                    strips.last_mut().expect("one").push(emitted.len());
                    emitted.push(std::mem::take(&mut pending));
                    if matches!(msg, Message::Gs { op: GsOp::EmitCut, .. }) {
                        strips.push(Vec::new());
                    }
                }
                Message::Gs { op: GsOp::Cut, .. } => strips.push(Vec::new()),
                _ => {}
            }
            continue;
        }
        let Some(ds) = inst.ds() else { continue };
        let name = inst.name().unwrap_or("");
        let addr = value_of(i, Unit::V(ds.addr)).expect("ds reads its address");
        if name.starts_with("ds_write") {
            if *base.get_or_insert(addr) != addr {
                return Err(GsError::SeveralOutputBases { at: inst.offset });
            }
            let two = name.contains("write2");
            let (o0, o1) = if two {
                (u32::from(ds.offset0), u32::from(ds.offset1))
            } else {
                (u32::from(ds.offset16()) / 4, 0)
            };
            let st64 = if name.contains("st64") { 64 } else { 1 };
            pending.push((i, o0 * st64, value_of(i, Unit::V(ds.data0)).expect("data0")));
            if two {
                pending.push((i, o1 * st64, value_of(i, Unit::V(ds.data1)).expect("data1")));
            }
        } else if name.starts_with("ds_read") {
            let two = name.contains("read2");
            let offsets: Vec<u32> = if two {
                vec![u32::from(ds.offset0), u32::from(ds.offset1)]
            } else {
                vec![u32::from(ds.offset16()) / 4]
            };
            let defs = &ssa.defs[i];
            let slot = match inputs.iter().position(|v| v.address == addr) {
                Some(k) => k,
                None => {
                    inputs.push(InputVertex { address: addr, dwords: BTreeMap::new() });
                    inputs.len() - 1
                }
            };
            for (k, &o) in offsets.iter().enumerate() {
                if let Some(&(_, v)) = defs.get(k) {
                    inputs[slot].dwords.insert(o, v);
                }
            }
        }
    }
    if strips.last().is_some_and(|s| s.is_empty()) {
        strips.pop();
    }
    if emitted.is_empty() {
        return Err(GsError::NoVertices);
    }
    // The stride: vertex k's lowest dword is k strides in.
    let mut stride = None;
    for (k, writes) in emitted.iter().enumerate().skip(1) {
        let Some(low) = writes.iter().map(|w| w.1).min() else { continue };
        let s = low / k as u32;
        if stride.is_some_and(|t| t != s) || s == 0 {
            return Err(GsError::NoStride);
        }
        stride = Some(s);
    }
    let stride = match (stride, emitted.len()) {
        (Some(s), _) => s,
        // One vertex: its own extent is the stride.
        (None, 1) => emitted[0].iter().map(|w| w.1 + 1).max().ok_or(GsError::NoStride)?,
        (None, _) => return Err(GsError::NoStride),
    };
    let mut vertices = Vec::new();
    for (k, writes) in emitted.iter().enumerate() {
        let mut v = Vertex::new();
        let lo = k as u32 * stride;
        for &(i, o, val) in writes {
            if !(lo..lo + stride).contains(&o) {
                return Err(GsError::OutsideVertex { at: cfg.insts[i].offset, vertex: k });
            }
            v.insert(o - lo, val);
        }
        vertices.push(v);
    }
    Ok(GsProgram {
        output_base: base.ok_or(GsError::NoVertices)?,
        vertex_stride: stride,
        vertices,
        strips,
        inputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode;

    fn program(words: &[u32]) -> Result<GsProgram, GsError> {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let cfg = Cfg::build(decode(&bytes).unwrap()).unwrap();
        let ssa = Ssa::build(&cfg);
        analyse(&cfg, &ssa)
    }

    /// ds_write2_b32 v<addr>, v<a>, v<b> offset0:<o0> offset1:<o1>
    fn write2(addr: u32, a: u32, b: u32, o0: u32, o1: u32) -> [u32; 2] {
        [0xd800_0000 | 0x0e << 18 | o1 << 8 | o0, b << 16 | a << 8 | addr]
    }
    /// ds_read2_b32 v[dst:dst+1], v<addr> offset0:<o0> offset1:<o1>
    fn read2(dst: u32, addr: u32, o0: u32, o1: u32) -> [u32; 2] {
        [0xd800_0000 | 0x37 << 18 | o1 << 8 | o0, dst << 24 | addr]
    }
    const EMIT: u32 = 0xbf90_0022;
    const CUT: u32 = 0xbf90_0012;
    const DONE: u32 = 0xbf90_0003;
    const END: u32 = 0xbf81_0000;

    #[test]
    fn a_triangle_of_three_vertices() {
        let mut w = Vec::new();
        w.extend(read2(20, 6, 0, 1)); // input vertex through v6: dwords 0, 1
        w.extend(read2(22, 8, 0, 1)); // input vertex through v8
        for k in 0..3u32 {
            w.extend(write2(12, 20, 21, 4 * k, 4 * k + 1)); // dwords 0, 1 of vertex k
            w.push(EMIT);
        }
        w.extend([CUT, DONE, END]);
        let p = program(&w).unwrap();
        assert_eq!(p.vertex_stride, 4);
        assert_eq!(p.vertices.len(), 3);
        assert!(p.vertices.iter().all(|v| v.keys().copied().collect::<Vec<_>>() == [0, 1]));
        assert_eq!(p.strips, [vec![0, 1, 2]]);
        assert_eq!(p.inputs.len(), 2);
        assert_eq!(p.inputs[0].dwords.keys().copied().collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn writes_through_two_bases_are_refused() {
        let mut w = Vec::new();
        w.extend(write2(12, 1, 2, 0, 1));
        w.extend(write2(13, 1, 2, 0, 1));
        w.extend([EMIT, DONE, END]);
        assert!(matches!(program(&w), Err(GsError::SeveralOutputBases { .. })));
    }
}
