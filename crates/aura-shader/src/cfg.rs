//! Control flow of a console program: basic blocks, edges, dominators.
//!
//! The translation to DXIL needs structured control flow and SSA values,
//! and both start here: which instructions run in sequence (blocks), where
//! control can go next (edges), and which block every path to another must
//! pass through (dominators, and their frontiers, where SSA joins go).
//!
//! Only code reachable from the entry is kept. The programs of yeni çıkmış
//! bir oyun (the newly released game this layer runs) end in `s_endpgm`, and
//! what follows it (padding, `s_code_end`, constant data) is not code; a walk
//! from the entry never reads it as such.
//!
//! Branches here are wave-uniform: a `s_cbranch_execz` skips code no lane
//! runs. The per-lane picture (EXEC masks) is the next stage's work; this
//! module only says where the program counter can go.

use crate::gfx10::{Encoding, Inst};

/// Why a control-flow graph could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgError {
    /// A branch at `from` goes to `to`, which is not the start of an
    /// instruction.
    BadTarget { from: usize, to: usize },
    /// Control reaches the end of the code without ending the program.
    FallsOffEnd { at: usize },
    /// A jump through a register, a call or a subvector loop at `at`: not
    /// handled yet.
    Unsupported { at: usize, what: &'static str },
}

/// How a block ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Continues into the next block.
    Fallthrough,
    /// `s_branch`: always goes to its target.
    Jump,
    /// `s_cbranch_*`: target or next.
    Branch,
    /// The program ends here.
    End,
}

/// A run of instructions entered only at its first and left only after its
/// last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Indices into [`Cfg::insts`], `start..end`.
    pub start: usize,
    pub end: usize,
    pub exit: Exit,
    /// Successor blocks; for a branch, the taken target comes first.
    pub succs: Vec<usize>,
    pub preds: Vec<usize>,
}

/// The program's reachable code in blocks. Block 0 is the entry.
#[derive(Debug, Clone)]
pub struct Cfg {
    pub insts: Vec<Inst>,
    pub blocks: Vec<Block>,
}

/// What an instruction does to the program counter.
enum Flow {
    Next,
    Jump(usize),
    Branch(usize),
    End,
}

fn flow(inst: &Inst) -> Result<Flow, CfgError> {
    let at = inst.offset;
    Ok(match (inst.encoding, inst.name()) {
        (Encoding::Sopp, Some("s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" | "s_code_end")) => {
            Flow::End
        }
        (Encoding::Sopp, Some("s_branch")) => Flow::Jump(inst.branch_target()),
        (Encoding::Sopp, Some(name)) if name.starts_with("s_cbranch_") => {
            if name.starts_with("s_cbranch_cdbg") {
                // Debugger branches: never taken outside a debugger.
                Flow::Next
            } else {
                Flow::Branch(inst.branch_target())
            }
        }
        (Encoding::Sop1, Some(what @ ("s_setpc_b64" | "s_swappc_b64" | "s_rfe_b64"))) => {
            return Err(CfgError::Unsupported { at, what });
        }
        (Encoding::Sopk, Some(what @ ("s_call_b64" | "s_subvector_loop_begin" | "s_subvector_loop_end"))) => {
            return Err(CfgError::Unsupported { at, what });
        }
        _ => Flow::Next,
    })
}

impl Cfg {
    /// Builds the graph of the code reachable from the first instruction.
    pub fn build(insts: Vec<Inst>) -> Result<Cfg, CfgError> {
        let index_of = |offset: usize| insts.binary_search_by_key(&offset, |i| i.offset).ok();
        let n = insts.len();
        if n == 0 {
            return Ok(Cfg { insts, blocks: Vec::new() });
        }
        // Walk from the entry: which instructions run, and which start a
        // block (the entry, branch targets, what follows a branch).
        let mut reached = vec![false; n];
        let mut leader = vec![false; n];
        leader[0] = true;
        let mut work = vec![0usize];
        while let Some(start) = work.pop() {
            let mut i = start;
            loop {
                if reached[i] {
                    break;
                }
                reached[i] = true;
                let inst = &insts[i];
                let target = |to: usize| {
                    index_of(to).ok_or(CfgError::BadTarget { from: inst.offset, to })
                };
                let next = |i: usize| {
                    (i + 1 < n).then_some(i + 1).ok_or(CfgError::FallsOffEnd { at: inst.offset })
                };
                match flow(inst)? {
                    Flow::Next => i = next(i)?,
                    Flow::End => break,
                    Flow::Jump(to) => {
                        let t = target(to)?;
                        leader[t] = true;
                        work.push(t);
                        break;
                    }
                    Flow::Branch(to) => {
                        let t = target(to)?;
                        leader[t] = true;
                        work.push(t);
                        let f = next(i)?;
                        leader[f] = true;
                        work.push(f);
                        break;
                    }
                }
            }
        }
        // Cut the reached instructions into blocks.
        let mut blocks: Vec<Block> = Vec::new();
        let mut block_of = vec![usize::MAX; n];
        let mut i = 0;
        while i < n {
            if !reached[i] {
                i += 1;
                continue;
            }
            let start = i;
            let exit = loop {
                block_of[i] = blocks.len();
                let exit = match flow(&insts[i])? {
                    Flow::End => Some(Exit::End),
                    Flow::Jump(_) => Some(Exit::Jump),
                    Flow::Branch(_) => Some(Exit::Branch),
                    Flow::Next => None,
                };
                i += 1;
                if let Some(exit) = exit {
                    break exit;
                }
                if i >= n || leader[i] || !reached[i] {
                    break Exit::Fallthrough;
                }
            };
            blocks.push(Block { start, end: i, exit, succs: Vec::new(), preds: Vec::new() });
        }
        // Edges.
        for b in 0..blocks.len() {
            let last = &insts[blocks[b].end - 1];
            let next = || block_of[blocks[b].end];
            let target = || block_of[index_of(last.branch_target()).expect("checked in the walk")];
            let succs = match blocks[b].exit {
                Exit::End => vec![],
                Exit::Fallthrough => vec![next()],
                Exit::Jump => vec![target()],
                Exit::Branch => {
                    let (t, f) = (target(), next());
                    if t == f { vec![t] } else { vec![t, f] }
                }
            };
            for &s in &succs {
                blocks[s].preds.push(b);
            }
            blocks[b].succs = succs;
        }
        Ok(Cfg { insts, blocks })
    }

    /// Blocks in reverse postorder from the entry (every block before its
    /// successors, except along back edges).
    pub fn reverse_postorder(&self) -> Vec<usize> {
        let mut seen = vec![false; self.blocks.len()];
        let mut order = Vec::with_capacity(self.blocks.len());
        if self.blocks.is_empty() {
            return order;
        }
        // Iterative depth-first search: (block, next successor to visit).
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            if let Some(&s) = self.blocks[b].succs.get(k) {
                top.1 += 1;
                if !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                order.push(b);
                stack.pop();
            }
        }
        order.reverse();
        order
    }

    /// The immediate dominator of each block (the entry's is itself), by
    /// the Cooper-Harvey-Kennedy iteration.
    pub fn idoms(&self) -> Vec<usize> {
        let rpo = self.reverse_postorder();
        let mut rank = vec![usize::MAX; self.blocks.len()];
        for (r, &b) in rpo.iter().enumerate() {
            rank[b] = r;
        }
        let mut idom = vec![usize::MAX; self.blocks.len()];
        if rpo.is_empty() {
            return idom;
        }
        idom[rpo[0]] = rpo[0];
        let mut changed = true;
        while changed {
            changed = false;
            for &b in &rpo[1..] {
                let mut new = usize::MAX;
                for &p in &self.blocks[b].preds {
                    if idom[p] == usize::MAX {
                        continue;
                    }
                    new = if new == usize::MAX {
                        p
                    } else {
                        let (mut x, mut y) = (p, new);
                        while x != y {
                            while rank[x] > rank[y] {
                                x = idom[x];
                            }
                            while rank[y] > rank[x] {
                                y = idom[y];
                            }
                        }
                        x
                    };
                }
                if idom[b] != new {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        idom
    }

    /// Whether block `a` dominates block `b`.
    pub fn dominates(idom: &[usize], a: usize, mut b: usize) -> bool {
        loop {
            if a == b {
                return true;
            }
            let up = idom[b];
            if up == b || up == usize::MAX {
                return false;
            }
            b = up;
        }
    }

    /// Each block's dominance frontier: the blocks where its definitions
    /// meet others and an SSA join is needed.
    pub fn frontiers(&self, idom: &[usize]) -> Vec<Vec<usize>> {
        let mut df = vec![Vec::new(); self.blocks.len()];
        for (b, block) in self.blocks.iter().enumerate() {
            if block.preds.len() < 2 {
                continue;
            }
            for &p in &block.preds {
                let mut runner = p;
                while runner != idom[b] && runner != usize::MAX {
                    if !df[runner].contains(&b) {
                        df[runner].push(b);
                    }
                    if idom[runner] == runner {
                        break;
                    }
                    runner = idom[runner];
                }
            }
        }
        df
    }

    /// Edges `from -> to` that close a loop (`to` dominates `from`).
    pub fn back_edges(&self, idom: &[usize]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (b, block) in self.blocks.iter().enumerate() {
            for &s in &block.succs {
                if Self::dominates(idom, s, b) {
                    out.push((b, s));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode;

    fn cfg(words: &[u32]) -> Result<Cfg, CfgError> {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        Cfg::build(decode(&bytes).unwrap())
    }

    const fn sopp(op: u32, simm16: i16) -> u32 {
        0xbf80_0000 | op << 16 | simm16 as u16 as u32
    }
    const NOP: u32 = sopp(0x00, 0);
    const END: u32 = sopp(0x01, 0);
    const fn branch(d: i16) -> u32 {
        sopp(0x02, d)
    }
    const fn cbranch_scc0(d: i16) -> u32 {
        sopp(0x04, d)
    }

    #[test]
    fn straight_line_is_one_block_and_ignores_what_follows_the_end() {
        let g = cfg(&[NOP, NOP, END, 0xdead_beef & 0x7fff_ffff, END]).unwrap();
        assert_eq!(g.blocks.len(), 1);
        assert_eq!((g.blocks[0].start, g.blocks[0].end), (0, 3));
        assert_eq!(g.blocks[0].exit, Exit::End);
    }

    #[test]
    fn if_then_diamond() {
        // 0: cbranch_scc0 +2 -> 3 ; 1: nop ; 2: branch +1 -> 4 ; 3: nop ; 4: end
        let g = cfg(&[cbranch_scc0(2), NOP, branch(1), NOP, END]).unwrap();
        let shape: Vec<_> = g.blocks.iter().map(|b| (b.start, b.end, b.succs.clone())).collect();
        assert_eq!(shape, [(0, 1, vec![2, 1]), (1, 3, vec![3]), (3, 4, vec![3]), (4, 5, vec![])]);
        let idom = g.idoms();
        assert_eq!(idom, [0, 0, 0, 0]);
        let df = g.frontiers(&idom);
        assert_eq!(df[1], [3]);
        assert_eq!(df[2], [3]);
        assert!(df[0].is_empty());
        assert!(g.back_edges(&idom).is_empty());
    }

    #[test]
    fn a_loop_has_a_back_edge_and_its_header_in_the_frontier() {
        // 0: nop ; 1: nop (header) ; 2: cbranch_scc0 -2 -> 1 ; 3: end
        let g = cfg(&[NOP, NOP, cbranch_scc0(-2), END]).unwrap();
        assert_eq!(g.blocks.len(), 3);
        let idom = g.idoms();
        assert_eq!(g.back_edges(&idom), [(1, 1)]);
        assert_eq!(g.frontiers(&idom)[1], [1]);
        assert_eq!(g.reverse_postorder(), [0, 1, 2]);
    }

    #[test]
    fn bad_flow_is_reported() {
        assert_eq!(cfg(&[NOP, NOP]).unwrap_err(), CfgError::FallsOffEnd { at: 4 });
        // A branch into the middle of a two-dword instruction.
        let movk = [0xbe80_03ff, 0x1234_5678];
        assert_eq!(
            cfg(&[branch(1), movk[0], movk[1], END]).unwrap_err(),
            CfgError::BadTarget { from: 0, to: 8 }
        );
        // s_setpc_b64 s[0:1]
        assert_eq!(
            cfg(&[0xbe80_2000, END]).unwrap_err(),
            CfgError::Unsupported { at: 0, what: "s_setpc_b64" }
        );
    }

    #[test]
    fn unreachable_code_between_blocks_is_dropped() {
        // 0: branch +1 -> 2 ; 1: nop (never runs) ; 2: end
        let g = cfg(&[branch(1), NOP, END]).unwrap();
        assert_eq!(g.blocks.len(), 2);
        assert_eq!((g.blocks[1].start, g.blocks[1].end), (2, 3));
        assert_eq!(g.blocks[0].succs, [1]);
    }
}
