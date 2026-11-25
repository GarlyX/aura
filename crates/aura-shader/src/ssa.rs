//! Static single assignment over register units.
//!
//! Every value a console program computes gets one name, defined once: a
//! register written twice becomes two values, and where control flow joins
//! two definitions of a register, a phi chooses between them. The DXIL the
//! translation emits is SSA too, so this is the shape everything after
//! this stage works on.
//!
//! Variables are register units (32-bit registers, see `operands`), so a
//! 64-bit pair written whole and read in halves needs no splitting later.
//! Phis go only where a unit is live across blocks (semi-pruned SSA:
//! a unit read in a block before that block writes it), at the iterated
//! dominance frontier of its definitions; renaming walks the dominator tree.
//! A unit read before anything writes it is a program input; `abi` says
//! where its value comes from.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::cfg::Cfg;
use crate::operands::{operands, Unit, UseKind};

/// Index into [`Ssa::values`].
pub type ValueId = u32;

/// Where a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueDef {
    /// The unit's value at program entry.
    Input(Unit),
    /// Written by instruction `inst` (an index into [`Cfg::insts`]).
    Inst { inst: usize, unit: Unit },
    /// Chosen at the start of `block` among its predecessors' values.
    Phi { block: usize, unit: Unit },
}

/// A join of one unit's values at a block's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phi {
    pub unit: Unit,
    pub value: ValueId,
    /// One value per predecessor, in the order of the block's `preds`.
    pub args: Vec<ValueId>,
}

/// The program in SSA form, alongside its [`Cfg`].
#[derive(Debug, Clone)]
pub struct Ssa {
    pub values: Vec<ValueDef>,
    /// Per block.
    pub phis: Vec<Vec<Phi>>,
    /// Per instruction: each use with the value it reads.
    pub uses: Vec<Vec<(Unit, UseKind, ValueId)>>,
    /// Per instruction: each definition with its new value.
    pub defs: Vec<Vec<(Unit, ValueId)>>,
    /// Program inputs by unit.
    pub inputs: BTreeMap<Unit, ValueId>,
}

impl Ssa {
    /// Builds SSA for the whole graph.
    pub fn build(cfg: &Cfg) -> Ssa {
        let nb = cfg.blocks.len();
        let ops: Vec<_> = cfg.insts.iter().map(operands).collect();
        let mut ssa = Ssa {
            values: Vec::new(),
            phis: vec![Vec::new(); nb],
            uses: vec![Vec::new(); cfg.insts.len()],
            defs: vec![Vec::new(); cfg.insts.len()],
            inputs: BTreeMap::new(),
        };
        if nb == 0 {
            return ssa;
        }
        let idom = cfg.idoms();
        let frontiers = cfg.frontiers(&idom);

        // Units live across blocks, and the blocks defining each unit.
        let mut global = BTreeSet::new();
        let mut def_blocks: HashMap<Unit, BTreeSet<usize>> = HashMap::new();
        for (b, block) in cfg.blocks.iter().enumerate() {
            let mut written = BTreeSet::new();
            for o in &ops[block.start..block.end] {
                for &(u, _) in &o.uses {
                    if !written.contains(&u) {
                        global.insert(u);
                    }
                }
                for &u in &o.defs {
                    written.insert(u);
                    def_blocks.entry(u).or_default().insert(b);
                }
            }
        }

        // Phis at the iterated dominance frontier.
        for &u in &global {
            let Some(defs) = def_blocks.get(&u) else { continue };
            let mut has_phi = BTreeSet::new();
            let mut work: Vec<usize> = defs.iter().copied().collect();
            while let Some(b) = work.pop() {
                for &f in &frontiers[b] {
                    if has_phi.insert(f) {
                        let value = ssa.new_value(ValueDef::Phi { block: f, unit: u });
                        ssa.phis[f].push(Phi { unit: u, value, args: Vec::new() });
                        if !defs.contains(&f) {
                            work.push(f);
                        }
                    }
                }
            }
        }

        // Rename along the dominator tree.
        let mut children = vec![Vec::new(); nb];
        for b in 1..nb {
            if idom[b] != usize::MAX && idom[b] != b {
                children[idom[b]].push(b);
            }
        }
        let mut current: HashMap<Unit, Vec<ValueId>> = HashMap::new();
        // (block, entered?) — on exit, pop what the block pushed.
        let mut stack = vec![(0usize, false)];
        let mut pushed: Vec<Vec<Unit>> = vec![Vec::new(); nb];
        while let Some((b, entered)) = stack.pop() {
            if entered {
                for u in pushed[b].drain(..) {
                    current.get_mut(&u).expect("pushed").pop();
                }
                continue;
            }
            stack.push((b, true));
            for phi in &ssa.phis[b] {
                current.entry(phi.unit).or_default().push(phi.value);
                pushed[b].push(phi.unit);
            }
            for i in cfg.blocks[b].start..cfg.blocks[b].end {
                for &(u, kind) in &ops[i].uses {
                    let v = ssa.lookup(&mut current, u);
                    ssa.uses[i].push((u, kind, v));
                }
                for &u in &ops[i].defs {
                    let v = ssa.new_value(ValueDef::Inst { inst: i, unit: u });
                    current.entry(u).or_default().push(v);
                    pushed[b].push(u);
                    ssa.defs[i].push((u, v));
                }
            }
            for &s in &cfg.blocks[b].succs {
                let slot = cfg.blocks[s].preds.iter().position(|&p| p == b).expect("edge");
                for k in 0..ssa.phis[s].len() {
                    let u = ssa.phis[s][k].unit;
                    let v = ssa.lookup(&mut current, u);
                    let args = &mut ssa.phis[s][k].args;
                    if args.len() < cfg.blocks[s].preds.len() {
                        args.resize(cfg.blocks[s].preds.len(), ValueId::MAX);
                    }
                    args[slot] = v;
                }
            }
            for &c in children[b].iter().rev() {
                stack.push((c, false));
            }
        }
        ssa
    }

    fn new_value(&mut self, def: ValueDef) -> ValueId {
        self.values.push(def);
        (self.values.len() - 1) as ValueId
    }

    /// The unit's current value; a program input if nothing wrote it yet.
    fn lookup(&mut self, current: &mut HashMap<Unit, Vec<ValueId>>, u: Unit) -> ValueId {
        if let Some(&v) = current.get(&u).and_then(|s| s.last()) {
            return v;
        }
        if let Some(&v) = self.inputs.get(&u) {
            return v;
        }
        let v = self.new_value(ValueDef::Input(u));
        self.inputs.insert(u, v);
        v
    }

    /// Program inputs that some instruction computes with (not only
    /// keeps in disabled lanes).
    pub fn read_inputs(&self) -> BTreeSet<Unit> {
        let mut out = BTreeSet::new();
        let mut reaches_read = vec![false; self.values.len()];
        // An input counts if a Read use sees it directly or through phis.
        let mut changed = true;
        for uses in &self.uses {
            for &(_, kind, v) in uses {
                if kind == UseKind::Read {
                    reaches_read[v as usize] = true;
                }
            }
        }
        while changed {
            changed = false;
            for phis in &self.phis {
                for phi in phis {
                    if reaches_read[phi.value as usize] {
                        for &a in &phi.args {
                            if a != ValueId::MAX && !reaches_read[a as usize] {
                                reaches_read[a as usize] = true;
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        for (&u, &v) in &self.inputs {
            if reaches_read[v as usize] {
                out.insert(u);
            }
        }
        out
    }
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
    /// s_mov_b32 s<dst>, s<src>
    const fn smov(dst: u32, src: u32) -> u32 {
        0xbe80_0300 | dst << 16 | src
    }
    /// s_mov_b32 s<dst>, <inline integer>
    const fn smovi(dst: u32, k: u32) -> u32 {
        0xbe80_0300 | dst << 16 | (128 + k)
    }

    #[test]
    fn straight_line_renames_each_write() {
        // s0 = 1 ; s0 = 2 ; s1 = s0 ; s2 = s7 (input)
        let (_, ssa) = build(&[smovi(0, 1), smovi(0, 2), smov(1, 0), smov(2, 7), END]);
        let first = ssa.defs[0][0].1;
        let second = ssa.defs[1][0].1;
        assert_ne!(first, second);
        assert_eq!(ssa.uses[2][0], (Unit::S(0), UseKind::Read, second));
        assert_eq!(ssa.inputs.keys().copied().collect::<Vec<_>>(), [Unit::S(7)]);
        assert!(ssa.phis.iter().all(|p| p.is_empty()));
    }

    #[test]
    fn a_diamond_joins_with_a_phi() {
        // 0: cbranch_scc0 +2 -> 3 ; 1: s0 = 1 ; 2: branch +1 -> 4 ; 3: s0 = 2 ; 4: s1 = s0 ; 5: end
        let (cfg, ssa) = build(&[sopp(0x04, 2), smovi(0, 1), sopp(0x02, 1), smovi(0, 2), smov(1, 0), END]);
        let join = cfg.blocks.iter().position(|b| b.start == 4).unwrap();
        let phi = ssa.phis[join].iter().find(|p| p.unit == Unit::S(0)).expect("phi for s0");
        assert_eq!(phi.args.len(), 2);
        let mut args = phi.args.clone();
        args.sort();
        let mut want = vec![ssa.defs[1][0].1, ssa.defs[3][0].1];
        want.sort();
        assert_eq!(args, want);
        assert_eq!(ssa.uses[4][0].2, phi.value);
        // The branch reads SCC before anything writes it.
        assert!(ssa.read_inputs().contains(&Unit::Scc));
    }

    #[test]
    fn a_loop_carried_register_gets_a_phi_at_the_header() {
        // 0: s0 = 0 ; 1: s1 = s0 (header) ; 2: s0 = s1 ; 3: cbranch_scc0 -3 -> 1 ; 4: end
        let (cfg, ssa) = build(&[smovi(0, 0), smov(1, 0), smov(0, 1), sopp(0x04, -3), END]);
        let header = cfg.blocks.iter().position(|b| b.start == 1).unwrap();
        let phi = ssa.phis[header].iter().find(|p| p.unit == Unit::S(0)).expect("phi");
        let entry_def = ssa.defs[0][0].1;
        let body_def = ssa.defs[2][0].1;
        assert!(phi.args.contains(&entry_def) && phi.args.contains(&body_def));
        assert_eq!(ssa.uses[1][0].2, phi.value);
        assert!(!ssa.inputs.contains_key(&Unit::S(0)));
    }
}
