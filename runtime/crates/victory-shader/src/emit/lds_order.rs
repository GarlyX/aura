//! Which LDS accesses of a console wave run as two host waves need the
//! console's order between the halves ([`super::lower`]'s `lds_meetings`):
//! single-thread conditions, the bytes an access reaches, and the clash test.

use std::collections::{HashMap, HashSet};

use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Program, Reg, Value};
use crate::structure::Structured;

/// Whether every lane where `b` holds has `atom` true.
pub(super) fn implies(bools: &crate::structure::Bools, b: crate::structure::Bool, atom: crate::structure::Atom) -> bool {
    use crate::structure::Bool;
    fn go(
        bools: &crate::structure::Bools,
        b: Bool,
        atom: crate::structure::Atom,
        memo: &mut HashMap<Bool, bool>,
    ) -> bool {
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let r = match bools.node(b) {
            None => b == Bool::FALSE,
            // ordered: `atom` is not tested again below its own node
            Some((a, lo, _)) if a == atom => lo == Bool::FALSE,
            Some((_, lo, hi)) => go(bools, lo, atom, memo) && go(bools, hi, atom, memo),
        };
        memo.insert(b, r);
        r
    }
    go(bools, b, atom, &mut HashMap::new())
}

/// Lane conditions true in at most one thread of the group: an equality of a uniform value
/// (scalar, constant, [`lane_uniform`] VGPR) with the thread number moved by uniform amounts
/// (`v0` of a one-dimensional group, through adds, subtracts, xors and `v_lshl_add` onto it,
/// each a bijection). The console compiler guards a group's single-thread setup this way
/// (`if (tid == 0)`).
pub(super) fn lone_lanes(p: &Program, st: &Structured, same: &HashSet<Value>, one_d: bool) -> Vec<crate::structure::Atom> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let mut ids: HashSet<Value> = HashSet::new();
    if !one_d {
        return Vec::new(); // (v0 is then one coordinate of the thread)
    }
    if let Some(entry) = p.blocks.first() {
        // the thread number as the program finds it in v0
        for inst in &entry.insts {
            if let Some(v) = inst.read(Reg::V(0)) {
                ids.insert(v);
                break;
            }
            if inst.writes.iter().any(|w| w.0 == Reg::V(0)) {
                break;
            }
        }
    }
    let mut lone = Vec::new();
    for b in crate::ir::reverse_postorder(&p.blocks) {
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            let Fields::Valu(v) = &inst.fields else { continue };
            if v.dpp.is_some() || v.sdwa.is_some() || st.guards[b][i].is_some() {
                continue;
            }
            let val = |op: &Operand| match *op {
                Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
                _ => None,
            };
            let id = |op: &Operand| val(op).is_some_and(|x| ids.contains(&x));
            let alike = |op: &Operand| match *op {
                Operand::Vector { .. } => val(op).is_some_and(|x| same.contains(&x)),
                Operand::Scalar { code, .. } => {
                    code != NULL && inst.read(Reg::S(code)).is_none_or(|x| !is_mask(x))
                }
                Operand::Const(_) | Operand::Literal(_) => true,
                _ => false,
            };
            let name = inst.name();
            let src = &v.src;
            let one_of = |n: usize| {
                src.len() == n
                    && src.iter().filter(|o| id(o)).count() == 1
                    && src.iter().all(|o| id(o) || alike(o))
            };
            let moved = match name {
                "v_mov_b32" => src.len() == 1 && id(&src[0]),
                "v_add_nc_u32" | "v_add_u32" | "v_sub_nc_u32" | "v_subrev_nc_u32" | "v_xor_b32" => one_of(2),
                "v_add3_u32" => one_of(3),
                // (src0 << src1) + src2
                "v_lshl_add_u32" => src.len() == 3 && id(&src[2]) && alike(&src[0]) && alike(&src[1]),
                _ => false,
            };
            if moved {
                ids.extend(inst.writes.iter().filter(|w| matches!(w.0, Reg::V(_))).map(|w| w.1));
            } else if matches!(name, "v_cmp_eq_u32" | "v_cmp_eq_i32") && one_of(2) {
                lone.extend(inst.writes.iter().map(|w| crate::structure::Atom::Lane(w.1)));
            }
        }
    }
    lone
}

/// The bytes (start, length) an LDS instruction reaches past its address register; `None`
/// when not known here.
pub(super) fn lds_span(name: &str, offset0: u8, offset1: u8) -> Option<Vec<(u32, u32)>> {
    let size = if name.contains("b128") {
        16
    } else if name.contains("b96") {
        12
    } else if name.ends_with("64") && !name.contains("st64") || name.contains("_b64") || name.contains("_u64") {
        8
    } else if name.contains("16") {
        2
    } else if name.contains("8") {
        1
    } else {
        4
    };
    if name.contains("read2") || name.contains("write2") {
        let scale = if name.contains("st64") { size * 64 } else { size };
        return Some(vec![(u32::from(offset0) * scale, size), (u32::from(offset1) * scale, size)]);
    }
    if name.contains("addtid") || name.contains("swizzle") || name.contains("permute") {
        return None;
    }
    Some(vec![(u32::from(offset1) << 8 | u32::from(offset0), size)])
}

/// What one LDS access of a lane-branch region does, for [`lds_meetings`].
pub(super) struct LdsAccess {
    /// 0 read, 1 write, 2 atomic read-modify-write.
    pub(super) kind: u8,
    /// The address register's value, where every lane running the access holds the same.
    pub(super) base: Option<Value>,
    /// That value, where a move of a constant made it.
    pub(super) at: Option<u32>,
    pub(super) span: Option<Vec<(u32, u32)>>,
    /// A condition of [`lone_lanes`] the access's guard implies: one thread runs it.
    pub(super) lone: Option<crate::structure::Atom>,
}

/// Whether two accesses of one region may need the console's order between lanes of the two
/// host waves: a read and a write (or an atomic) that may reach the same dword from different
/// lanes. Atomics among themselves commute; two writes are left as [`lds_meetings`] leaves them.
pub(super) fn lds_clash(x: &LdsAccess, y: &LdsAccess) -> bool {
    if x.kind == y.kind && x.kind != 0 || x.kind == 0 && y.kind == 0 {
        return false;
    }
    if x.lone.is_some() && x.lone == y.lone {
        return false; // one thread runs both, in its program order
    }
    let meet = |s: &[(u32, u32)], t: &[(u32, u32)], d: i64| {
        s.iter().any(|&(p, n)| {
            t.iter().any(|&(q, m)| {
                let (p, q) = (i64::from(p), i64::from(q) + d);
                p < q + i64::from(m) && q < p + i64::from(n)
            })
        })
    };
    match (x.base, y.base, &x.span, &y.span) {
        (Some(a), Some(b), Some(s), Some(t)) if a == b => meet(s, t, 0),
        // two moves of constants: the bytes as numbers
        (Some(_), Some(_), Some(s), Some(t)) if x.at.is_some() && y.at.is_some() => {
            let d = i64::from(y.at.unwrap_or(0)) - i64::from(x.at.unwrap_or(0));
            meet(s, t, d)
        }
        _ => true,
    }
}
