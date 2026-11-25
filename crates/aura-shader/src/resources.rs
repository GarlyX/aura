//! Tracking descriptors from the loads that read them to their uses.
//!
//! A console program reads its resource descriptors out of tables the driver
//! points it at: a scalar load (`s_load_dwordx4 s[36:39], s[6:7], s36`)
//! fetches the 4 or 8 dwords of a view descriptor, and the program either
//! hands those dwords to an image or buffer instruction, or reads them as
//! numbers. Translating the first is binding; translating the second is
//! where shipped programs get interesting, and what this module models:
//!
//! - **Size fields** (rule A). A descriptor's dwords carry the view's base
//!   address, dimensions and format. Programs read them: a compute pass
//!   skips an empty buffer by testing its byte count, a pixel program
//!   divides by a texture's width. The host knows these values, so the
//!   translation materializes the console-format dwords from the bound
//!   view's metadata instead of refusing the program.
//! - **Address walks** (rule A, ray tracing). A ray-tracing pass loads a
//!   descriptor only to extract the 40-bit address inside and read the
//!   structure it points at. The host answers with the bound view's GPU
//!   address. When the table index is computed per draw (a *dynamic*
//!   offset), the answer cannot be a compile-time constant: the host keeps
//!   a parallel table of view addresses the translated program indexes at
//!   run time.
//! - **Constant samplers at a join** (rule B). A phi can choose between a
//!   sampler loaded from the heap and one the compiler baked into the code
//!   as four constant dwords. DXIL cannot merge a static sampler with a
//!   heap descriptor in one register, so the constant goes to a reserved
//!   slot at the end of the sampler heap and the phi selects that slot's
//!   index. [`SpilledSamplers`] keeps the mapping; the host fills the
//!   reserved slots when it creates the pipeline.
//!
//! The classification walk is deliberately shallow: values are followed
//! through moves and arithmetic to the instruction that consumes them, and
//! that sink decides the class. A value that feeds nothing we model is
//! reported, not guessed.

use std::collections::{BTreeSet, HashMap};

use crate::cfg::Cfg;
use crate::gfx10::{Encoding, Inst};
use crate::operands::{Unit, UseKind};
use crate::ssa::{Ssa, ValueDef, ValueId};

/// First sampler-heap slot reserved for spilled constant samplers. The host
/// pads its sampler heap to 2048; a title's own tables sit far below this
/// (the one measured used 106), so the top eight slots are safe.
pub const SPILL_BASE: u32 = 2040;
/// How many distinct constant samplers one program may spill.
pub const SPILL_CAPACITY: usize = 8;

/// A scalar load that reads descriptor dwords from a table or the heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorLoad {
    /// Index of the load in `Cfg::insts`.
    pub inst: usize,
    /// First SGPR of the address pair (the table pointer).
    pub sbase: u8,
    /// First SGPR the dwords land in.
    pub sdata: u8,
    /// Dwords loaded (4 for a buffer, 8 for an image descriptor).
    pub dwords: u8,
    /// Static byte offset into the table.
    pub offset: i32,
    /// Dynamic byte offset, when the index is computed per draw: the SSA
    /// value of the offset register.
    pub dynamic: Option<ValueId>,
}

/// Every scalar load of descriptor width in the program, in order.
///
/// `s_load_*` reads through a 64-bit table pointer, `s_buffer_load_*`
/// through a 4-dword buffer resource; both appear as descriptor sources in
/// shipped programs.
pub fn descriptor_loads(cfg: &Cfg, ssa: &Ssa) -> Vec<DescriptorLoad> {
    let mut out = Vec::new();
    for (i, inst) in cfg.insts.iter().enumerate() {
        if inst.encoding != Encoding::Smem {
            continue;
        }
        let Some(name) = inst.name() else { continue };
        if !name.starts_with("s_load") && !name.starts_with("s_buffer_load") {
            continue;
        }
        let Some(s) = inst.smem() else { continue };
        let dynamic = (s.soffset != 0x7d).then(|| {
            ssa.uses[i]
                .iter()
                .find(|(u, k, _)| *k == UseKind::Read && *u == Unit::S(s.soffset))
                .map(|(_, _, v)| *v)
        }).flatten();
        out.push(DescriptorLoad {
            inst: i,
            sbase: s.sbase,
            sdata: s.sdata,
            dwords: load_dwords(name),
            offset: s.offset,
            dynamic,
        });
    }
    out
}

/// How the dwords a [`DescriptorLoad`] fetched are used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Uses {
    /// They reach an image or buffer instruction's resource operand: the
    /// load binds a view.
    pub resource: bool,
    /// Only the address bits are extracted, and the memory behind the view
    /// is read (a ray-tracing structure walk).
    pub address_walk: bool,
    /// Size or format dwords are read as numbers.
    pub data_read: bool,
}

/// What the host must supply for a descriptor load (rule A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAnswer {
    /// Bind the matching host view.
    Bind,
    /// The bound view's GPU address, two dwords; the program walks the
    /// memory behind it. With a dynamic offset this comes from the host's
    /// parallel address table at run time, not a constant.
    ViewAddress,
    /// Console-format metadata the program reads as numbers: a buffer's
    /// byte count, an image's dimensions and level count.
    ViewMetadata,
    /// A binding, plus materialized dwords for the fields read as data.
    BindAndMaterialize,
    /// An address, plus the metadata read through or beside it.
    AddressAndMetadata,
    /// Nothing we model uses the dwords.
    Unknown,
}

/// Maps a load's [`Uses`] to the host's answer.
pub fn host_answer(u: Uses) -> HostAnswer {
    match (u.resource, u.address_walk, u.data_read) {
        (true, _, true) => HostAnswer::BindAndMaterialize,
        (true, _, false) => HostAnswer::Bind,
        (false, true, true) => HostAnswer::AddressAndMetadata,
        (false, true, false) => HostAnswer::ViewAddress,
        (false, false, true) => HostAnswer::ViewMetadata,
        _ => HostAnswer::Unknown,
    }
}

/// Bound on the forward walk, so a pathological program cannot spin it.
const WALK_LIMIT: usize = 4096;

/// Classifies the uses of every dword `load` fetches, following values
/// through moves and arithmetic to the instructions that consume them.
pub fn classify(cfg: &Cfg, ssa: &Ssa, load: &DescriptorLoad) -> Uses {
    // Value -> the instructions reading it.
    let mut users: HashMap<ValueId, Vec<usize>> = HashMap::new();
    for (i, uses) in ssa.uses.iter().enumerate() {
        for (_, kind, v) in uses {
            if *kind == UseKind::Read {
                users.entry(*v).or_default().push(i);
            }
        }
    }
    let mut out = Uses::default();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<ValueId> = ssa.defs[load.inst].iter().map(|d| d.1).collect();
    while let Some(v) = queue.pop() {
        if !seen.insert(v) || seen.len() > WALK_LIMIT {
            continue;
        }
        let Some(list) = users.get(&v) else { continue };
        for &u in list.clone().iter() {
            let inst = &cfg.insts[u];
            match inst.encoding {
                Encoding::Mimg | Encoding::Mubuf | Encoding::Mtbuf => out.resource = true,
                // A memory instruction's address comes from the dwords: the
                // program walks the memory behind the descriptor.
                Encoding::Global | Encoding::Flat | Encoding::Scratch | Encoding::Smem
                | Encoding::Ds => out.address_walk = true,
                // A compare or a branch on the dwords is a data read.
                Encoding::Sopc => out.data_read = true,
                // Moves, lane reads, scalar and vector arithmetic: the value
                // passes through; where it lands decides.
                _ => queue.extend(ssa.defs[u].iter().map(|d| d.1)),
            }
        }
    }
    out
}

/// A phi over the four dwords of a sampler descriptor where at least one
/// arm loads from the heap and one arm is a compile-time constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplerPhi {
    /// The block the phi sits at the top of.
    pub block: usize,
    /// First SGPR of the four.
    pub base: u8,
    /// One arm per predecessor of `block`.
    pub arms: Vec<SamplerArm>,
}

/// One arm of a [`SamplerPhi`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SamplerArm {
    /// The descriptor is loaded from the heap by this instruction.
    Load(usize),
    /// The compiler baked the descriptor into the code as four dwords.
    Constant([u32; 4]),
}

/// Finds every [`SamplerPhi`] in the program.
///
/// Four aligned SGPR units phi'd together at one block, where each arm is
/// either one scalar load or four constant moves, is a sampler chosen by
/// control flow. The console compiler emits these when a sampler is shared
/// between draws and inlined for some of them.
pub fn sampler_phis(cfg: &Cfg, ssa: &Ssa) -> Vec<SamplerPhi> {
    let mut out = Vec::new();
    for (b, phis) in ssa.phis.iter().enumerate() {
        for phi in phis {
            let Unit::S(base) = phi.unit else { continue };
            if base % 4 != 0 {
                continue;
            }
            let group: Option<Vec<_>> = (0..4)
                .map(|k| phis.iter().find(|p| p.unit == Unit::S(base + k)))
                .collect();
            let Some(group) = group else { continue };
            let arity = phi.args.len();
            if !group.iter().all(|p| p.args.len() == arity) {
                continue;
            }
            let mut arms = Vec::with_capacity(arity);
            let mut ok = true;
            for a in 0..arity {
                let vals: Vec<ValueId> = group.iter().map(|p| p.args[a]).collect();
                match arm_of(cfg, ssa, &vals) {
                    Some(arm) => arms.push(arm),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            let mixed = arms.iter().any(|a| matches!(a, SamplerArm::Load(_)))
                && arms.iter().any(|a| matches!(a, SamplerArm::Constant(_)));
            if ok && mixed {
                out.push(SamplerPhi { block: b, base, arms });
            }
        }
    }
    out
}

/// What defines one arm's four dwords: one load, or four constant moves.
fn arm_of(cfg: &Cfg, ssa: &Ssa, vals: &[ValueId]) -> Option<SamplerArm> {
    let defs: BTreeSet<Option<usize>> = vals
        .iter()
        .map(|&v| match ssa.values[v as usize] {
            ValueDef::Inst { inst, .. } => Some(inst),
            _ => None,
        })
        .collect();
    if defs.len() == 1 {
        let i = defs.into_iter().next().flatten()?;
        if cfg.insts[i].encoding == Encoding::Smem {
            return Some(SamplerArm::Load(i));
        }
    }
    let mut words = [0u32; 4];
    for (k, &v) in vals.iter().enumerate() {
        let ValueDef::Inst { inst, .. } = ssa.values[v as usize] else { return None };
        words[k] = const_mov(&cfg.insts[inst])?;
    }
    Some(SamplerArm::Constant(words))
}

/// The constant an `s_mov_b32` writes, when it is a literal or an inline
/// integer (128..=192 is 0..=64, 193..=208 is -1..=-16).
fn const_mov(inst: &Inst) -> Option<u32> {
    if inst.encoding != Encoding::Sop1 || inst.name()? != "s_mov_b32" {
        return None;
    }
    match inst.words[0] & 0xff {
        0xff => Some(inst.words[1]),
        k @ 128..=192 => Some(k - 128),
        k @ 193..=208 => Some((k as i32 - 192).wrapping_neg() as u32),
        _ => None,
    }
}

/// A constant sampler registered for a reserved heap slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpilledSampler {
    /// The four descriptor dwords as the code carries them.
    pub words: [u32; 4],
    /// Whether the sampler is a comparison one (the site decides).
    pub comparison: bool,
}

/// The constant samplers of one translation, in first-seen order.
///
/// The slot index a phi selects for a constant arm is
/// [`SPILL_BASE`] + the entry's position here, so the DXIL constant and the
/// heap slot the host fills agree by construction. The order is per run:
/// entries follow the order the translator meets them, which is why a
/// program that spills must not be written to the disk shader cache.
#[derive(Debug, Default)]
pub struct SpilledSamplers {
    entries: Vec<SpilledSampler>,
}

impl SpilledSamplers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reserved slot for `words`, registering the sampler on first use.
    /// `None` once [`SPILL_CAPACITY`] distinct samplers are registered.
    pub fn slot(&mut self, words: [u32; 4], comparison: bool) -> Option<u32> {
        if let Some(k) = self
            .entries
            .iter()
            .position(|e| e.words == words && e.comparison == comparison)
        {
            return Some(SPILL_BASE + k as u32);
        }
        if self.entries.len() >= SPILL_CAPACITY {
            return None;
        }
        self.entries.push(SpilledSampler { words, comparison });
        Some(SPILL_BASE + self.entries.len() as u32 - 1)
    }

    /// The registered samplers, in slot order.
    pub fn entries(&self) -> &[SpilledSampler] {
        &self.entries
    }
}

/// Dwords read by a scalar load, from its mnemonic suffix.
fn load_dwords(name: &str) -> u8 {
    for (suffix, n) in [("x16", 16), ("x8", 8), ("x4", 4), ("x2", 2)] {
        if name.ends_with(suffix) {
            return n;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode;

    fn build(words: &[u32]) -> (Cfg, Ssa) {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let cfg = Cfg::build(decode(&bytes).unwrap()).unwrap();
        let ssa = Ssa::build(&cfg);
        (cfg, ssa)
    }

    const fn sopp(op: u32, simm16: i16) -> u32 {
        0xbf80_0000 | op << 16 | simm16 as u16 as u32
    }
    const END: u32 = sopp(0x01, 0);
    /// s_mov_b32 s<dst>, <inline integer 0..=64>
    const fn smovi(dst: u32, k: u32) -> [u32; 1] {
        [0xbe80_0300 | dst << 16 | (128 + k)]
    }
    /// s_mov_b32 s<dst>, <literal>
    const fn smov_lit(dst: u32, k: u32) -> [u32; 2] {
        [0xbe80_03ff | dst << 16, k]
    }
    /// s_load_dwordx4 s<data>, s<base>, <soffset SGPR or 0x7d for none>
    const fn sload4(sdata: u32, sbase: u32, soffset: u32) -> [u32; 2] {
        [0xf408_0000 | (sbase / 2) | (sdata << 6), soffset << 25]
    }
    /// s_load_dwordx8 s<data>, s<base>, <static byte offset>
    const fn sload8(sdata: u32, sbase: u32, offset: u32) -> [u32; 2] {
        [0xf40c_0000 | (sbase / 2) | (sdata << 6), 0xfa00_0000 | offset]
    }
    /// s_and_b32 s<dst>, s<a>, <literal>
    const fn sand_lit(dst: u32, a: u32, k: u32) -> [u32; 2] {
        [0x8700_0000 | dst << 16 | 0xff << 8 | a, k]
    }
    /// v_mov_b32 v<dst>, s<src>
    const fn vmov_s(dst: u32, src: u32) -> u32 {
        0x7e00_0200 | dst << 17 | src
    }
    /// s_cmp_eq_u32 s<a>, <inline integer>
    const fn scmp_eq(a: u32, k: u32) -> u32 {
        0xbf06_0000 | (128 + k) << 8 | a
    }

    fn words<const N: usize>(parts: &[&[u32]; N]) -> Vec<u32> {
        parts.concat()
    }

    // image_load v3, [v0,v7], s[8:15] dim:2d — a shipped encoding, unchanged.
    const IMAGE_LOAD_S8: [u32; 3] = [0xf000_110a, 0x0002_0300, 0x0000_0007];
    // global_load_dword v48, v[30:31] — likewise.
    const GLOBAL_LOAD_V30: [u32; 2] = [0xdc30_8000, 0x307d_001e];

    #[test]
    fn finds_static_and_dynamic_loads() {
        // s_load_dwordx4 s[0:3], s[4:5] ; s_load_dwordx4 s[36:39], s[6:7], s36
        let w = words(&[
            &sload4(0, 4, 0x7d),
            &sload4(36, 6, 36),
            &[END],
        ]);
        let (cfg, ssa) = build(&w);
        let loads = descriptor_loads(&cfg, &ssa);
        assert_eq!(loads.len(), 2);
        assert_eq!(
            (loads[0].sbase, loads[0].sdata, loads[0].dwords, loads[0].offset, loads[0].dynamic),
            (4, 0, 4, 0, None)
        );
        assert_eq!(loads[1].sbase, 6);
        assert_eq!(loads[1].sdata, 36);
        assert!(loads[1].dynamic.is_some());
    }

    #[test]
    fn resource_use_asks_for_a_binding() {
        // s_load_dwordx8 s[8:15], s[4:5], 0x20 ; image_load v3, [v0,v7], s[8:15]
        let w = words(&[&sload8(8, 4, 0x20), &IMAGE_LOAD_S8, &[END]]);
        let (cfg, ssa) = build(&w);
        let loads = descriptor_loads(&cfg, &ssa);
        assert_eq!(loads.len(), 1);
        let uses = classify(&cfg, &ssa, &loads[0]);
        assert_eq!(uses, Uses { resource: true, address_walk: false, data_read: false });
        assert_eq!(host_answer(uses), HostAnswer::Bind);
    }

    #[test]
    fn address_walk_asks_for_the_view_address() {
        // load ; s16 = s0 & 0xffffff00 ; s17 = s1 & 0xff ; v30 = s16 ;
        // v31 = s17 ; global_load v48, v[30:31]
        let w = words(&[
            &sload4(0, 4, 0x7d),
            &sand_lit(16, 0, 0xffff_ff00),
            &sand_lit(17, 1, 0x0000_00ff),
            &[vmov_s(30, 16), vmov_s(31, 17)],
            &GLOBAL_LOAD_V30,
            &[END],
        ]);
        let (cfg, ssa) = build(&w);
        let loads = descriptor_loads(&cfg, &ssa);
        let uses = classify(&cfg, &ssa, &loads[0]);
        assert_eq!(uses, Uses { resource: false, address_walk: true, data_read: false });
        assert_eq!(host_answer(uses), HostAnswer::ViewAddress);
    }

    #[test]
    fn size_field_read_asks_for_metadata() {
        // load ; s_cmp_eq_u32 s2, 0 — the empty-buffer test.
        let w = words(&[&sload4(0, 4, 0x7d), &[scmp_eq(2, 0), END]]);
        let (cfg, ssa) = build(&w);
        let loads = descriptor_loads(&cfg, &ssa);
        let uses = classify(&cfg, &ssa, &loads[0]);
        assert_eq!(uses, Uses { resource: false, address_walk: false, data_read: true });
        assert_eq!(host_answer(uses), HostAnswer::ViewMetadata);
    }

    #[test]
    fn resource_and_data_read_materializes_both() {
        // s_load_dwordx8 s[8:15], s[4:5], 0x20 ; image_load … s[8:15] ;
        // s_cmp_eq_u32 s10, 0
        let w = words(&[
            &sload8(8, 4, 0x20),
            &IMAGE_LOAD_S8,
            &[scmp_eq(10, 0), END],
        ]);
        let (cfg, ssa) = build(&w);
        let loads = descriptor_loads(&cfg, &ssa);
        let uses = classify(&cfg, &ssa, &loads[0]);
        assert_eq!(uses, Uses { resource: true, address_walk: false, data_read: true });
        assert_eq!(host_answer(uses), HostAnswer::BindAndMaterialize);
    }

    #[test]
    fn a_diamond_sampler_phi_is_found_and_spilled() {
        // 0: cbranch_scc0 +3 -> 4 ; 1: load s[40:43], s[4:5] ; 3: branch +7
        // -> 11 ; 4: four constant moves into s[40:43] ; 11: sample with
        // sampler s[40:43] ; 14: end
        let sampler10 = 0x0142_4945u32; // image_sample_l …, s[8:15], s[40:43]
        let w = words(&[
            &[sopp(0x04, 3)],
            &sload4(40, 4, 0x7d),
            &[sopp(0x02, 7)],
            &smov_lit(40, 146),
            &smov_lit(41, 0x00c0_0000),
            &smov_lit(42, 0x1850_0000),
            &smovi(43, 0),
            &[0xf090_080a, sampler10, 0x0000_4447],
            &[END],
        ]);
        let (cfg, ssa) = build(&w);
        let found = sampler_phis(&cfg, &ssa);
        assert_eq!(found.len(), 1, "one phi group mixes a load and a constant");
        let sp = &found[0];
        assert_eq!(sp.base, 40);
        assert_eq!(sp.arms.len(), 2);
        let (load_arm, const_arm) = if matches!(sp.arms[0], SamplerArm::Load(_)) {
            (&sp.arms[0], &sp.arms[1])
        } else {
            (&sp.arms[1], &sp.arms[0])
        };
        assert_eq!(*load_arm, SamplerArm::Load(1));
        assert_eq!(
            *const_arm,
            SamplerArm::Constant([146, 0x00c0_0000, 0x1850_0000, 0])
        );
        // Rule B: the constant arm gets a reserved slot; the phi selects it.
        let mut spilled = SpilledSamplers::new();
        let SamplerArm::Constant(words) = const_arm else { unreachable!() };
        assert_eq!(spilled.slot(*words, false), Some(SPILL_BASE));
    }

    #[test]
    fn spill_slots_dedup_and_cap() {
        let mut s = SpilledSamplers::new();
        let a = [1, 2, 3, 4];
        let b = [5, 6, 7, 8];
        assert_eq!(s.slot(a, false), Some(SPILL_BASE));
        assert_eq!(s.slot(b, false), Some(SPILL_BASE + 1));
        // Same words again: same slot. The comparison flag distinguishes.
        assert_eq!(s.slot(a, false), Some(SPILL_BASE));
        assert_eq!(s.slot(a, true), Some(SPILL_BASE + 2));
        // Order is first-seen, so it is deterministic within a run.
        for k in 0..SPILL_CAPACITY - 3 {
            let w = [9 + k as u32; 4];
            assert_eq!(s.slot(w, false), Some(SPILL_BASE + 3 + k as u32));
        }
        // The ninth distinct sampler is refused: no slot, nothing added.
        assert_eq!(s.slot([0xdead_beef; 4], false), None);
        assert_eq!(s.entries().len(), SPILL_CAPACITY);
    }
}
