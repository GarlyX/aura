//! Which lanes each vector instruction runs on.
//!
//! A console wave runs 64 lanes in lockstep; the EXEC mask says which of
//! them a vector instruction affects, and scalar code reshapes EXEC to
//! express per-lane control flow (if/else becomes "and EXEC with a compare
//! result, run, restore"). DXIL has no EXEC: each thread is one lane, and
//! "this lane is off" must become a condition. This module reads EXEC
//! symbolically: every EXEC value the program creates becomes a [`Mask`]
//! built from the SSA values that define it, so the translation can turn
//! each guarded write into "if my lane is in the mask".
//!
//! The geometry programs of yeni çıkmış bir oyun (the newly released game
//! this layer runs) open with the hardware's idiom for "the first n lanes":
//! `s_bfm_b64 exec, n, 0`, then `s_cmp_eq_u32 n, 64; s_cselect_b64 exec, -1,
//! exec` for the one count a 64-bit shift cannot express. That pair is
//! recognised as [`Mask::LowLanes`]: in a mesh shader it is simply
//! "lane index < n".

use crate::cfg::Cfg;
use crate::gfx10::{Encoding, Inst};
use crate::operands::{Unit, UseKind, EXEC};
use crate::ssa::{Ssa, ValueDef, ValueId};

/// A 64-lane mask, in terms of SSA values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mask {
    /// EXEC as the hardware set it at entry.
    Entry,
    /// Every lane.
    All,
    /// No lane.
    None,
    /// Lanes `0..n`, `n` a 32-bit scalar value (0..=64).
    LowLanes(ValueId),
    /// A per-lane compare result (the mask's low unit value).
    Compare(ValueId),
    And(Box<Mask>, Box<Mask>),
    /// Lanes in either (an if/else merge restoring the lanes it set aside).
    Or(Box<Mask>, Box<Mask>),
    /// Lanes in exactly one (the else side: saved lanes minus the then side).
    Xor(Box<Mask>, Box<Mask>),
    /// The first mask without the lanes of the second.
    AndNot(Box<Mask>, Box<Mask>),
    /// The first mask if the scalar condition (an SCC value) holds, else the
    /// second.
    Select(ValueId, Box<Mask>, Box<Mask>),
    /// Whole quads of lanes where any lane of the quad is set.
    Wqm(Box<Mask>),
    /// A mask that joins at a block start (loop or if/else merge).
    Phi(ValueId),
    /// Anything else (the mask's low unit value).
    Opaque(ValueId),
}

/// A scalar source operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    Reg(Unit),
    Const(i64),
}

fn scalar_src(inst: &Inst, code: u32) -> Src {
    match code {
        128..=192 => Src::Const(i64::from(code - 128)),
        193..=208 => Src::Const(-i64::from(code - 192)),
        255 => Src::Const(i64::from(inst.literal().unwrap_or(0) as i32)),
        0..=127 => Src::Reg(Unit::S(code as u8)),
        253 => Src::Reg(Unit::Scc),
        _ => Src::Const(i64::MIN), // floats and specials: not a mask constant
    }
}

/// Lane masks of a program in SSA form.
pub struct Lanes<'a> {
    cfg: &'a Cfg,
    ssa: &'a Ssa,
}

impl<'a> Lanes<'a> {
    pub fn new(cfg: &'a Cfg, ssa: &'a Ssa) -> Lanes<'a> {
        Lanes { cfg, ssa }
    }

    /// The value instruction `i` reads for `unit`, if it reads it.
    fn use_value(&self, i: usize, unit: Unit) -> Option<ValueId> {
        self.ssa.uses[i].iter().find(|(u, _, _)| *u == unit).map(|&(_, _, v)| v)
    }

    /// The lanes instruction `i` runs on: the EXEC it reads, or `None` for
    /// instructions that do not depend on EXEC (scalar code).
    pub fn guard(&self, i: usize) -> Option<Mask> {
        let v = self.ssa.uses[i]
            .iter()
            .find(|(u, k, _)| *u == Unit::S(EXEC) && *k == UseKind::Read)?
            .2;
        Some(self.mask(v, 32))
    }

    /// The mask whose low unit holds SSA value `v`.
    pub fn mask(&self, v: ValueId, depth: u32) -> Mask {
        if depth == 0 {
            return Mask::Opaque(v);
        }
        let (i, unit) = match self.ssa.values[v as usize] {
            ValueDef::Input(Unit::S(EXEC)) => return Mask::Entry,
            ValueDef::Input(_) => return Mask::Opaque(v),
            ValueDef::Phi { .. } => return Mask::Phi(v),
            ValueDef::Inst { inst, unit } => (inst, unit),
        };
        let inst = &self.cfg.insts[i];
        let name = inst.name().unwrap_or("");
        let w0 = inst.words[0];
        let (s0, s1) = (w0 & 0xff, (w0 >> 8) & 0xff);
        // Only the low unit of a pair written as a whole names a mask.
        let Unit::S(dst) = unit else { return Mask::Opaque(v) };
        let src_mask = |code: u32| -> Mask {
            match scalar_src(inst, code) {
                Src::Const(-1) => Mask::All,
                Src::Const(0) => Mask::None,
                Src::Reg(u) => match self.use_value(i, u) {
                    Some(sv) => self.mask(sv, depth - 1),
                    None => Mask::Opaque(v),
                },
                Src::Const(_) => Mask::Opaque(v),
            }
        };
        match (inst.encoding, name) {
            (Encoding::Sop1, "s_mov_b64") => src_mask(s0),
            (Encoding::Sop1, "s_wqm_b64") => Mask::Wqm(Box::new(src_mask(s0))),
            (Encoding::Sop1, n) if n.ends_with("_saveexec_b64") => {
                let exec = self.use_value(i, Unit::S(EXEC)).map_or(Mask::Entry, |e| self.mask(e, depth - 1));
                if dst != EXEC {
                    // The saved copy: EXEC as it was before.
                    exec
                } else if n == "s_and_saveexec_b64" {
                    Mask::And(Box::new(src_mask(s0)), Box::new(exec))
                } else if n == "s_or_saveexec_b64" {
                    Mask::Or(Box::new(src_mask(s0)), Box::new(exec))
                } else if n == "s_andn2_saveexec_b64" {
                    Mask::AndNot(Box::new(src_mask(s0)), Box::new(exec))
                } else {
                    Mask::Opaque(v)
                }
            }
            (Encoding::Sop2, "s_and_b64") => Mask::And(Box::new(src_mask(s0)), Box::new(src_mask(s1))),
            (Encoding::Sop2, "s_or_b64") => Mask::Or(Box::new(src_mask(s0)), Box::new(src_mask(s1))),
            (Encoding::Sop2, "s_xor_b64") => Mask::Xor(Box::new(src_mask(s0)), Box::new(src_mask(s1))),
            (Encoding::Sop2, "s_andn2_b64") => Mask::AndNot(Box::new(src_mask(s0)), Box::new(src_mask(s1))),
            (Encoding::Sop2, "s_bfm_b64") => match (scalar_src(inst, s0), scalar_src(inst, s1)) {
                (Src::Reg(n), Src::Const(0)) => self.use_value(i, n).map_or(Mask::Opaque(v), Mask::LowLanes),
                (Src::Const(0), Src::Const(0)) => Mask::None,
                _ => Mask::Opaque(v),
            },
            (Encoding::Sop2, "s_cselect_b64") => self.cselect(i, inst, s0, s1, depth).unwrap_or_else(|| {
                match self.use_value(i, Unit::Scc) {
                    Some(scc) => Mask::Select(scc, Box::new(src_mask(s0)), Box::new(src_mask(s1))),
                    None => Mask::Opaque(v),
                }
            }),
            (Encoding::Vopc | Encoding::Vop3, n) if n.starts_with("v_cmpx") && dst == EXEC => {
                let exec = self.use_value(i, Unit::S(EXEC)).map_or(Mask::Entry, |e| self.mask(e, depth - 1));
                Mask::And(Box::new(Mask::Compare(v)), Box::new(exec))
            }
            (Encoding::Vopc | Encoding::Vop3, n) if n.starts_with("v_cmp") => Mask::Compare(v),
            _ => Mask::Opaque(v),
        }
    }

    /// `s_cselect_b64 dst, -1, m` where SCC is `n == 64` and `m` is
    /// `LowLanes(n)`: all 64 lanes is what "the first n lanes" means then.
    fn cselect(&self, i: usize, inst: &Inst, s0: u32, s1: u32, depth: u32) -> Option<Mask> {
        if scalar_src(inst, s0) != Src::Const(-1) {
            return None;
        }
        let Src::Reg(other) = scalar_src(inst, s1) else { return None };
        let other = self.mask(self.use_value(i, other)?, depth - 1);
        let Mask::LowLanes(n) = other else { return None };
        let scc = self.use_value(i, Unit::Scc)?;
        let ValueDef::Inst { inst: c, .. } = self.ssa.values[scc as usize] else { return None };
        let cmp = &self.cfg.insts[c];
        if cmp.encoding != Encoding::Sopc || cmp.name() != Some("s_cmp_eq_u32") {
            return None;
        }
        let w = cmp.words[0];
        let (a, b) = (scalar_src(cmp, w & 0xff), scalar_src(cmp, (w >> 8) & 0xff));
        let compared = match (a, b) {
            (Src::Reg(u), Src::Const(64)) | (Src::Const(64), Src::Reg(u)) => self.use_value(c, u)?,
            _ => return None,
        };
        (compared == n).then_some(Mask::LowLanes(n))
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

    const END: u32 = 0xbf81_0000;
    /// v_mov_b32 v1, v0
    const VMOV: u32 = 0x7e02_0300 | 0x100;

    #[test]
    fn the_first_n_lanes_idiom() {
        // The geometry program's opening, as on the console:
        // s_bfe_i32 s0, s3, 0x80008 ; s_bfm_b64 exec, s0, 0 ;
        // s_cmp_eq_u32 s0, 64 ; s_cselect_b64 exec, -1, exec ; v_mov_b32 v1, v0
        let (cfg, ssa) = build(&[0x9400_ff03, 0x0008_0008, 0x92fe_8000, 0xbf06_c000, 0x85fe_7ec1, VMOV, END]);
        let lanes = Lanes::new(&cfg, &ssa);
        let count = ssa.defs[0].iter().find(|(u, _)| *u == Unit::S(0)).unwrap().1;
        assert_eq!(lanes.guard(4), Some(Mask::LowLanes(count)));
        // Scalar code has no guard.
        assert_eq!(lanes.guard(0), None);
    }

    #[test]
    fn entry_all_and_compare_masks() {
        // v_mov_b32 (entry EXEC) ; s_mov_b64 exec, -1 ; v_mov_b32 ;
        // v_cmpx_gt_u32 exec, v0, v1 (VOPC 0xd4) ; v_mov_b32
        let cmpx = 0x7c00_0000 | 0xd4 << 17 | 1 << 9 | 0x100;
        let (cfg, ssa) = build(&[VMOV, 0xbefe_04c1, VMOV, cmpx, VMOV, END]);
        let lanes = Lanes::new(&cfg, &ssa);
        assert_eq!(lanes.guard(0), Some(Mask::Entry));
        assert_eq!(lanes.guard(2), Some(Mask::All));
        match lanes.guard(4) {
            Some(Mask::And(a, b)) => {
                assert!(matches!(*a, Mask::Compare(_)));
                assert_eq!(*b, Mask::All);
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn if_else_with_saveexec_xor_and_or() {
        // s_and_saveexec_b64 s[0:1], vcc   (then: exec = vcc & exec; s[0:1] = old)
        // v_mov ; s_xor_b64 exec, s[0:1], exec (else lanes) ; v_mov ;
        // s_or_b64 exec, exec, s[0:1] (restore) ; v_mov
        let saveexec = 0xbe80_246a;
        let sop2 = |op: u32, dst: u32, a: u32, b: u32| 0x8000_0000 | op << 23 | dst << 16 | b << 8 | a;
        let xor = sop2(0x13, 126, 0, 126);
        let or = sop2(0x11, 126, 126, 0);
        let (cfg, ssa) = build(&[saveexec, VMOV, xor, VMOV, or, VMOV, END]);
        let lanes = Lanes::new(&cfg, &ssa);
        assert!(matches!(lanes.guard(1), Some(Mask::And(..))));
        match lanes.guard(3) {
            Some(Mask::Xor(saved, _)) => assert_eq!(*saved, Mask::Entry),
            other => panic!("{other:?}"),
        }
        assert!(matches!(lanes.guard(5), Some(Mask::Or(..))));
    }
}
