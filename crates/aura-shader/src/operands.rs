//! Which registers each instruction reads and writes.
//!
//! SSA construction needs, for every instruction, the register units it
//! defines and the ones it uses. A unit is one 32-bit register: a 64-bit
//! operand is two units (`s[4:5]` is s4 and s5), so values that are later
//! read in halves, or written in halves, need no special case.
//!
//! Widths come from the opcode name: the console's mnemonics carry their
//! operand types (`_b64`, `_f32`, `_dwordx4`), and conversions name the
//! destination type before the source type (`v_cvt_f64_i32`). The handful
//! of instructions whose operands do not follow that rule (shifts with a
//! 32-bit shift count, 64-bit multiply-adds, lane reads) are listed below.
//!
//! Vector instructions only write the lanes EXEC enables; the others keep
//! what the register held. That earlier value is a real input of the write
//! (it flows through to later reads), recorded as a [`UseKind::Keep`] use so
//! that callers asking "what does this program read" can tell it apart from
//! the operands it computes with.
//!
//! The programs of yeni çıkmış bir oyun (the newly released game this layer
//! runs) are written for 64-lane waves: lane masks (VCC, EXEC, compare
//! results) are two units.

use crate::gfx10::{Encoding, Inst};

/// One 32-bit register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unit {
    /// A scalar register by its operand code: s0..s105, VCC (106/107),
    /// trap temporaries (108..123), M0 (124), EXEC (126/127). (GFX10 moved
    /// M0 to 124 and gave 125 to NULL; older generations had them swapped.)
    S(u8),
    /// The scalar condition bit.
    Scc,
    /// A vector register.
    V(u8),
}

pub const VCC: u8 = 106;
pub const M0: u8 = 124;
pub const EXEC: u8 = 126;
const NULL: u32 = 125;

/// Why a register is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseKind {
    /// An operand the instruction computes with.
    Read,
    /// The register's earlier value, kept in the lanes EXEC disables.
    Keep,
}

/// The register units an instruction defines and uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operands {
    pub defs: Vec<Unit>,
    pub uses: Vec<(Unit, UseKind)>,
    /// The model of this instruction is an approximation (image address
    /// counts, relative addressing): fine for structure, to be refined
    /// before the translation relies on it.
    pub approximate: bool,
}

impl Operands {
    fn def(&mut self, u: Unit, n: u32) {
        for k in 0..n {
            self.defs.push(step(u, k));
        }
    }
    fn read(&mut self, u: Unit, n: u32) {
        for k in 0..n {
            self.uses.push((step(u, k), UseKind::Read));
        }
    }
    fn keep(&mut self, u: Unit, n: u32) {
        for k in 0..n {
            self.uses.push((step(u, k), UseKind::Keep));
        }
    }
    /// A scalar operand code (8 or 9 bits) read with width `n`.
    fn read_src(&mut self, code: u32, n: u32) {
        match code {
            0..=124 | 126..=127 => self.read(Unit::S(code as u8), n),
            251 => self.read(Unit::S(VCC), 2), // VCCZ
            252 => self.read(Unit::S(EXEC), 2), // EXECZ
            253 => self.read(Unit::Scc, 1),
            256..=511 => self.read(Unit::V((code - 256) as u8), n),
            _ => {} // inline constants, literal, NULL
        }
    }
    /// A scalar destination code written with width `n`.
    fn def_sdst(&mut self, code: u32, n: u32) {
        if code != NULL && code <= 127 {
            self.def(Unit::S(code as u8), n);
        }
    }
    /// A vector destination: written in enabled lanes, kept in the rest.
    fn def_vdst(&mut self, v: u32, n: u32) {
        self.def(Unit::V(v as u8), n);
        self.keep(Unit::V(v as u8), n);
    }
}

fn step(u: Unit, k: u32) -> Unit {
    match u {
        Unit::S(s) => Unit::S(s.wrapping_add(k as u8)),
        Unit::V(v) => Unit::V(v.wrapping_add(k as u8)),
        Unit::Scc => Unit::Scc,
    }
}

/// Dwords of a type token (`f64` 2, `f32`/`u16`/`b32` 1).
fn type_width(t: &str) -> Option<u32> {
    let bits: u32 = t.get(1..)?.parse().ok()?;
    matches!(t.as_bytes()[0], b'f' | b'u' | b'i' | b'b').then_some(if bits > 32 { 2 } else { 1 })
}

/// (destination, source) widths of a typed mnemonic: the last type token
/// is the source type; the one before it, if any, the destination type.
fn widths(name: &str) -> (u32, u32) {
    let types: Vec<u32> = name.split('_').filter_map(type_width).collect();
    match types.as_slice() {
        [] => (1, 1),
        [w] => (*w, *w),
        [.., d, s] => (*d, *s),
    }
}

/// Dwords of a memory access by its mnemonic suffix.
fn data_dwords(name: &str) -> u32 {
    for (suffix, n) in [("x16", 16), ("x8", 8), ("x4", 4), ("x3", 3), ("x2", 2)] {
        if name.ends_with(suffix) || name.contains(&format!("{suffix}_")) {
            return n;
        }
    }
    if name.contains("b128") {
        4
    } else if name.contains("b96") {
        3
    } else if name.contains("b64") || name.contains("u64") || name.contains("i64") || name.contains("f64") {
        2
    } else {
        1
    }
}

/// Scalar ops that leave SCC alone (everything else in SOP1/SOP2 writes it).
fn scalar_keeps_scc(name: &str) -> bool {
    const KEEP: [&str; 16] = [
        "s_mov", "s_cmov", "s_cselect", "s_brev", "s_ff0", "s_ff1", "s_flbit", "s_sext",
        "s_bitset", "s_getpc", "s_setpc", "s_swappc", "s_movrel", "s_bfm", "s_pack", "s_mul_",
    ];
    KEEP.iter().any(|k| name.starts_with(k)) || name == "s_mulk_i32" || name.starts_with("s_getreg")
}

/// The units `inst` defines and uses.
pub fn operands(inst: &Inst) -> Operands {
    let mut o = Operands::default();
    let name = inst.name().unwrap_or("");
    let w0 = inst.words[0];
    let w1 = inst.words.get(1).copied().unwrap_or(0);
    match inst.encoding {
        Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk => scalar(&mut o, inst, name),
        Encoding::Sopp => {
            if name.starts_with("s_cbranch_scc") {
                o.read(Unit::Scc, 1);
            } else if name.starts_with("s_cbranch_vcc") {
                o.read(Unit::S(VCC), 2);
            } else if name.starts_with("s_cbranch_exec") {
                o.read(Unit::S(EXEC), 2);
            } else if name.starts_with("s_sendmsg") {
                o.read(Unit::S(M0), 1);
            }
        }
        Encoding::Smem => {
            let s = inst.smem().expect("smem");
            let buffer = name.starts_with("s_buffer_");
            o.read(Unit::S(s.sbase), if buffer { 4 } else { 2 });
            if s.soffset != 0x7d {
                o.read_src(u32::from(s.soffset), 1);
            }
            if name.contains("load") {
                o.def_sdst(u32::from(s.sdata), data_dwords(name));
            } else if name.starts_with("s_memtime") || name.starts_with("s_memrealtime") {
                o.def_sdst(u32::from(s.sdata), 2);
            } else if name.contains("store") || name.contains("atomic") {
                o.read_src(u32::from(s.sdata), data_dwords(name));
            }
        }
        Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 | Encoding::Vop3p => {
            vector(&mut o, inst, name)
        }
        Encoding::Vintrp => {
            o.read(Unit::S(M0), 1);
            o.read(Unit::V(w0 as u8), 1);
            o.def_vdst((w0 >> 18) & 0xff, 1);
            o.read(Unit::S(EXEC), 2);
        }
        Encoding::Ds => {
            let ds = inst.ds().expect("ds");
            let w = data_dwords(name);
            let two = name.contains("2_") || name.contains("2st64");
            let no_addr = matches!(name, "ds_append" | "ds_consume" | "ds_gws_init" | "ds_gws_barrier")
                || name.starts_with("ds_gws_sema");
            if !no_addr {
                o.read(Unit::V(ds.addr), 1);
            }
            if no_addr || ds.gds || name.starts_with("ds_ordered") {
                o.read(Unit::S(M0), 1);
            }
            let returns = name.starts_with("ds_read") || name.contains("_rtn_") || matches!(
                name,
                "ds_append" | "ds_consume" | "ds_swizzle_b32" | "ds_permute_b32" | "ds_bpermute_b32"
            ) || name.starts_with("ds_ordered");
            let writes_data = !name.starts_with("ds_read") && !matches!(name, "ds_append" | "ds_consume" | "ds_swizzle_b32");
            if writes_data {
                o.read(Unit::V(ds.data0), w);
                if two || name.contains("cmpst") || name.contains("cmpswap") || name.contains("mskor") {
                    o.read(Unit::V(ds.data1), w);
                }
            }
            if returns {
                let n = if two && name.starts_with("ds_read") { 2 * w } else { w };
                o.def_vdst(u32::from(ds.vdst), n);
            }
            o.read(Unit::S(EXEC), 2);
        }
        Encoding::Exp => {
            let enable = w0 & 0xf;
            let compressed = (w0 >> 10) & 1 == 1;
            for (k, shift) in [0, 8, 16, 24].into_iter().enumerate() {
                let on = if compressed { enable & (0b11 << (2 * (k / 2))) != 0 && k % 2 == 0 } else { enable & (1 << k) != 0 };
                if on {
                    o.read(Unit::V((w1 >> shift) as u8), 1);
                }
            }
            o.read(Unit::S(EXEC), 2);
        }
        Encoding::Mubuf | Encoding::Mtbuf => {
            // Same bits in both formats: an offset and/or an index VGPR.
            let (offen, idxen) = ((w0 >> 12) & 1, (w0 >> 13) & 1);
            o.read(Unit::V(w1 as u8), offen + idxen);
            o.read(Unit::S((((w1 >> 16) & 0x1f) * 4) as u8), 4);
            o.read_src(w1 >> 24, 1);
            let vdata = (w1 >> 8) & 0xff;
            let n = if inst.encoding == Encoding::Mtbuf { (inst.opcode as u32 & 3) + 1 } else { data_dwords(name) };
            if name.contains("load") {
                o.def_vdst(vdata, n);
            } else if name.contains("store") || name.contains("atomic") {
                o.read(Unit::V(vdata as u8), n);
                if name.contains("atomic") && (w0 >> 14) & 1 == 1 {
                    o.def_vdst(vdata, n);
                }
            }
            o.read(Unit::S(EXEC), 2);
        }
        Encoding::Mimg => {
            let dmask = (w0 >> 8) & 0xf;
            let n = dmask.count_ones().max(1);
            let (addresses, exact) = image_addresses(name, (w0 >> 3) & 7, (w1 >> 30) & 1 == 1);
            o.approximate = !exact || (w0 >> 1) & 3 != 0; // non-sequential addresses
            o.read(Unit::V(w1 as u8), addresses);
            o.read(Unit::S((((w1 >> 16) & 0x1f) * 4) as u8), 8);
            if name.contains("sample") || name.contains("gather") {
                o.read(Unit::S((((w1 >> 21) & 0x1f) * 4) as u8), 4);
            }
            let vdata = (w1 >> 8) & 0xff;
            if name.contains("store") {
                o.read(Unit::V(vdata as u8), n);
            } else {
                o.def_vdst(vdata, n);
            }
            o.read(Unit::S(EXEC), 2);
        }
        Encoding::Flat | Encoding::Global | Encoding::Scratch => {
            let saddr = (w1 >> 16) & 0x7f;
            let with_saddr = inst.encoding != Encoding::Flat && saddr != 0x7d && saddr != NULL;
            o.read(Unit::V(w1 as u8), if with_saddr || inst.encoding == Encoding::Scratch { 1 } else { 2 });
            if with_saddr {
                o.read(Unit::S(saddr as u8), if inst.encoding == Encoding::Scratch { 1 } else { 2 });
            }
            let n = data_dwords(name);
            if name.contains("load") {
                o.def_vdst(w1 >> 24, n);
            } else {
                o.read(Unit::V((w1 >> 8) as u8), n);
                if name.contains("atomic") && (w0 >> 16) & 1 == 1 {
                    o.def_vdst(w1 >> 24, n);
                }
            }
            o.read(Unit::S(EXEC), 2);
        }
    }
    o
}

fn scalar(o: &mut Operands, inst: &Inst, name: &str) {
    let w0 = inst.words[0];
    let (src0, src1, sdst) = (w0 & 0xff, (w0 >> 8) & 0xff, (w0 >> 16) & 0x7f);
    let (dw, sw) = widths(name);
    let writes_scc = match inst.encoding {
        Encoding::Sopc => true,
        Encoding::Sopk => name.starts_with("s_cmpk") || name == "s_addk_i32",
        _ => !scalar_keeps_scc(name),
    };
    if name.starts_with("s_cmov") || name.starts_with("s_cselect") || name.starts_with("s_addc") || name.starts_with("s_subb") {
        o.read(Unit::Scc, 1);
    }
    if name.starts_with("s_movrel") {
        o.approximate = true;
        o.read(Unit::S(M0), 1);
    }
    match inst.encoding {
        Encoding::Sop1 => {
            if !matches!(name, "s_getpc_b64") {
                o.read_src(src0, sw);
            }
            if name.contains("saveexec") || name.contains("wrexec") {
                o.read(Unit::S(EXEC), 2);
                o.def(Unit::S(EXEC), 2);
            }
            if !matches!(name, "s_setpc_b64") {
                if name.starts_with("s_bitset") {
                    o.read(Unit::S(sdst as u8), 2.min(dw)); // sets one bit, keeps the rest
                }
                o.def_sdst(sdst, dw);
            }
        }
        Encoding::Sop2 => {
            // Shift counts, field descriptors and bit-mask builders are 32-bit
            // whatever the result width.
            let src1_w = if name.starts_with("s_lsh") || name.starts_with("s_ashr") || name.starts_with("s_bfe") {
                1
            } else {
                sw
            };
            let src0_w = if name == "s_bfm_b64" { 1 } else { sw };
            let src1_w = if name == "s_bfm_b64" { 1 } else { src1_w };
            o.read_src(src0, src0_w);
            o.read_src(src1, src1_w);
            o.def_sdst(sdst, dw);
        }
        Encoding::Sopc => {
            let w = if name.ends_with("_u64") || name.ends_with("_b64") || name.ends_with("_i64") { 2 } else { 1 };
            let w1 = if name.starts_with("s_bitcmp") { 1 } else { w };
            o.read_src(src0, w);
            o.read_src(src1, w1);
        }
        Encoding::Sopk => {
            let sdst = (w0 >> 16) & 0x7f;
            match name {
                "s_movk_i32" | "s_getreg_b32" => o.def_sdst(sdst, 1),
                "s_cmovk_i32" => {
                    o.keep_s(sdst);
                    o.def_sdst(sdst, 1);
                }
                "s_addk_i32" | "s_mulk_i32" => {
                    o.read_src(sdst, 1);
                    o.def_sdst(sdst, 1);
                }
                n if n.starts_with("s_cmpk") => o.read_src(sdst, 1),
                "s_setreg_b32" => o.read_src(sdst, 1),
                _ => {}
            }
        }
        _ => unreachable!(),
    }
    if writes_scc {
        o.def(Unit::Scc, 1);
    }
}

impl Operands {
    fn keep_s(&mut self, code: u32) {
        if code <= 127 && code != NULL {
            self.keep(Unit::S(code as u8), 1);
        }
    }
}

/// Vector source and destination widths, with the exceptions to the
/// "types in the name" rule.
fn vector_widths(name: &str) -> (u32, [u32; 3]) {
    let (d, s) = widths(name);
    match name {
        "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => (2, [1, 2, 1]),
        "v_ldexp_f64" | "v_trig_preop_f64" => (2, [2, 1, 1]),
        "v_mad_u64_u32" | "v_mad_i64_i32" => (2, [1, 1, 2]),
        "v_div_fixup_f64" | "v_div_fmas_f64" | "v_fma_f64" => (2, [2, 2, 2]),
        "v_readlane_b32" | "v_readfirstlane_b32" | "v_writelane_b32" => (1, [1, 1, 1]),
        _ => (d, [s, s, s]),
    }
}

/// Ops whose accumulator is their destination register.
fn accumulates(name: &str) -> bool {
    matches!(
        name,
        "v_fmac_f32" | "v_mac_f32" | "v_fmac_f16" | "v_mac_f16" | "v_mac_legacy_f32" | "v_fmac_legacy_f32"
            | "v_dot2c_f32_f16" | "v_dot4c_i32_i8"
    )
}

/// Source operands a VOP3/VOP3P instruction really has. Unused source
/// fields hold 0, which is also the code of s0, so the count must come from
/// the opcode, not from the fields.
fn vop3_sources(inst: &Inst, name: &str) -> usize {
    if inst.encoding == Encoding::Vop3p {
        return if name.contains("fma") || name.contains("mad") || name.starts_with("v_dot") { 3 } else { 2 };
    }
    match inst.opcode {
        0x000..0x100 => 2, // compares
        0x100..0x140 => {
            if name.starts_with("v_cndmask") || name.contains("_co_ci_") || name.starts_with("v_fmaak") || name.starts_with("v_fmamk") {
                3
            } else {
                2
            }
        }
        0x180..0x200 => 1,
        _ => {
            const TWO: [&str; 18] = [
                "v_mbcnt_", "v_bfm_b32", "v_lshlrev_b", "v_lshrrev_b", "v_ashrrev_i", "v_ldexp_", "v_readlane",
                "v_writelane", "v_mul_lo_", "v_mul_hi_", "v_bcnt_", "v_cvt_pk", "v_trig_preop", "v_pack_",
                "v_add_nc_", "v_sub_nc_", "v_subrev_nc_", "v_interp_p1ll",
            ];
            const TWO_EXACT: [&str; 14] = [
                "v_add_f64", "v_mul_f64", "v_min_f64", "v_max_f64", "v_add_co_u32", "v_sub_co_u32",
                "v_subrev_co_u32", "v_max_u16", "v_max_i16", "v_min_u16", "v_min_i16", "v_mul_legacy_f32",
                "v_cvt_pknorm_i16_f32", "v_cvt_pknorm_u16_f32",
            ];
            if TWO.iter().any(|p| name.starts_with(p)) || TWO_EXACT.contains(&name) {
                2
            } else {
                3
            }
        }
    }
}

/// Address VGPRs of an image instruction: coordinates for the dimension
/// (`dim`: 1D, 2D, 3D, cube, 1D array, 2D array, 2D MSAA, 2D MSAA array),
/// plus offset, bias/LOD, compare value, gradients and mip level as the
/// mnemonic asks. With 16-bit addresses (`a16`) values pack two to a
/// register; the packing of the extra operands is approximated.
fn image_addresses(name: &str, dim: u32, a16: bool) -> (u32, bool) {
    let coords: u32 = [1, 2, 3, 3, 2, 3, 3, 4][dim as usize];
    let dims = [1, 2, 3, 3, 1, 2, 2, 2][dim as usize];
    let mut extra: u32 = 0;
    let mut extra_names = 0;
    let has = |t: &str| name.split('_').any(|p| p == t);
    for t in ["o", "c", "b", "cl"] {
        if has(t) {
            extra += 1;
            extra_names += 1;
        }
    }
    if has("l") || has("mip") {
        extra += 1;
        extra_names += 1;
    }
    if has("d") || has("cd") {
        extra += 2 * dims;
        extra_names += 1;
    }
    if name.starts_with("image_get_resinfo") {
        return (1, true);
    }
    if a16 {
        ((coords + extra).div_ceil(2), extra_names == 0)
    } else {
        (coords + extra, true)
    }
}

fn vector(o: &mut Operands, inst: &Inst, name: &str) {
    if matches!(name, "v_nop" | "v_pipeflush") {
        // No operands: the source fields are padding.
        return;
    }
    let w0 = inst.words[0];
    let w1 = inst.words.get(1).copied().unwrap_or(0);
    let (dw, sw) = vector_widths(name);
    let compare = name.starts_with("v_cmp");
    // Field positions per encoding: sources, vector destination and (VOP3
    // only) a scalar destination for carries and compare results.
    let (srcs, vdst, sdst): (Vec<u32>, Option<u32>, Option<u32>) = match inst.encoding {
        Encoding::Vop1 => (vec![w0 & 0x1ff], Some((w0 >> 17) & 0xff), None),
        Encoding::Vop2 => (vec![w0 & 0x1ff, 256 + ((w0 >> 9) & 0xff)], Some((w0 >> 17) & 0xff), None),
        Encoding::Vopc => (vec![w0 & 0x1ff, 256 + ((w0 >> 9) & 0xff)], None, None),
        _ => {
            let count = vop3_sources(inst, name);
            let srcs: Vec<u32> = [w1 & 0x1ff, (w1 >> 9) & 0x1ff, (w1 >> 18) & 0x1ff][..count].to_vec();
            let vop3b = name.contains("_co_") || name.starts_with("v_div_scale") || name.starts_with("v_mad_u64") || name.starts_with("v_mad_i64");
            let vdst = w0 & 0xff;
            if compare {
                (srcs, None, Some(vdst))
            } else if vop3b {
                (srcs, Some(vdst), Some((w0 >> 8) & 0x7f))
            } else {
                (srcs, Some(vdst), None)
            }
        }
    };
    // Lane reads/writes cross into scalar registers.
    match name {
        "v_readfirstlane_b32" => {
            o.read_src(srcs[0], 1);
            o.def_sdst(vdst.unwrap_or(0), 1);
            o.read(Unit::S(EXEC), 2);
            return;
        }
        "v_readlane_b32" => {
            o.read_src(srcs[0], 1);
            o.read_src(srcs[1], 1);
            o.def_sdst(vdst.unwrap_or(0), 1);
            return;
        }
        _ => {}
    }
    // Interpolation in VOP3 form: source 0 names the attribute and channel,
    // not a register; the attribute base comes from M0.
    let srcs: Vec<u32> = if name.starts_with("v_interp_") {
        o.read(Unit::S(M0), 1);
        let mut srcs = srcs;
        srcs[0] = 128; // an inline constant: reads nothing
        srcs
    } else {
        srcs
    };
    for (k, &s) in srcs.iter().enumerate() {
        // Carry-in / select operands are lane masks.
        let lane_mask = (name.starts_with("v_cndmask") && k == 2) || (name.contains("_co_ci_") && k == 2) || (name.starts_with("v_div_fmas") && k == 3);
        o.read_src(s, if lane_mask { 2 } else { sw[k.min(2)] });
    }
    if inst.encoding == Encoding::Vop2 && (name.starts_with("v_cndmask") || name.contains("_co_ci_")) {
        o.read(Unit::S(VCC), 2);
    }
    if accumulates(name) {
        // The accumulator is the destination register.
        let v = if inst.encoding == Encoding::Vop2 { (w0 >> 17) & 0xff } else { w0 & 0xff };
        o.read(Unit::V(v as u8), dw);
    }
    o.read(Unit::S(EXEC), 2);
    if compare {
        let mask_dst = match inst.encoding {
            Encoding::Vopc => VCC as u32,
            _ => sdst.unwrap_or(VCC as u32),
        };
        o.def_sdst(mask_dst, 2);
        if name.starts_with("v_cmpx") {
            o.def(Unit::S(EXEC), 2);
        }
        return;
    }
    if let Some(v) = vdst {
        o.def_vdst(v, dw);
    }
    if let Some(s) = sdst {
        o.def_sdst(s, 2);
    } else if inst.encoding == Encoding::Vop2 && name.contains("_co_") {
        o.def(Unit::S(VCC), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode_one;

    fn ops(words: &[u32]) -> Operands {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        operands(&decode_one(&bytes, 0).unwrap())
    }
    fn reads(o: &Operands) -> Vec<Unit> {
        o.uses.iter().filter(|u| u.1 == UseKind::Read).map(|u| u.0).collect()
    }

    #[test]
    fn widths_from_names() {
        assert_eq!(widths("s_mov_b64"), (2, 2));
        assert_eq!(widths("v_cvt_f64_i32"), (2, 1));
        assert_eq!(widths("v_cvt_i32_f64"), (1, 2));
        assert_eq!(widths("s_bcnt1_i32_b64"), (1, 2));
        assert_eq!(widths("v_add_f16"), (1, 1));
        assert_eq!(data_dwords("s_load_dwordx4"), 4);
        assert_eq!(data_dwords("ds_read2_b64"), 2);
        assert_eq!(data_dwords("global_load_dword"), 1);
    }

    #[test]
    fn bfm_builds_a_64_bit_mask_from_32_bit_operands() {
        // s_bfm_b64 exec, s0, 0  (the geometry program's EXEC setup)
        let o = ops(&[0x92fe_8000]);
        assert_eq!(o.defs, [Unit::S(EXEC), Unit::S(EXEC + 1)]);
        assert_eq!(reads(&o), [Unit::S(0)]);
    }

    #[test]
    fn scalar_logic_writes_scc_and_mov_does_not() {
        // s_and_b32 s1, s9, literal
        let and = ops(&[0x8701_ff09, 0xff]);
        assert_eq!(and.defs, [Unit::S(1), Unit::Scc]);
        assert_eq!(reads(&and), [Unit::S(9)]);
        // s_mov_b32 s0, s8
        let mov = ops(&[0xbe80_0308]);
        assert_eq!(mov.defs, [Unit::S(0)]);
    }

    #[test]
    fn smem_loads_define_their_data_registers() {
        // s_load_dword s0, s[0:1], 0xb8
        let o = ops(&[0xf400_0000, 0xfa00_00b8]);
        assert_eq!(reads(&o), [Unit::S(0), Unit::S(1)]);
        assert_eq!(o.defs, [Unit::S(0)]);
    }

    #[test]
    fn vector_writes_keep_disabled_lanes() {
        // v_lshlrev_b32 v3, 2, v0
        let o = ops(&[0x3406_0082]);
        assert_eq!(o.defs, [Unit::V(3)]);
        assert_eq!(reads(&o), [Unit::V(0), Unit::S(EXEC), Unit::S(EXEC + 1)]);
        assert!(o.uses.contains(&(Unit::V(3), UseKind::Keep)));
    }

    #[test]
    fn compares_write_a_lane_mask() {
        // v_cmp_gt_u32 vcc, v0, v1 (VOPC opcode 0xc4)
        let o = ops(&[0x7c00_0000 | 0xc4 << 17 | 1 << 9 | 0x100]);
        assert_eq!(o.defs, [Unit::S(VCC), Unit::S(VCC + 1)]);
        assert_eq!(&reads(&o)[..2], [Unit::V(0), Unit::V(1)]);
    }

    #[test]
    fn ds_read2_defines_two_registers() {
        // ds_read2_b32 v[4:5], v1 offset0:2 offset1:3
        let o = ops(&[0xd800_0000 | 0x37 << 18 | 3 << 8 | 2, 4 << 24 | 1]);
        assert_eq!(o.defs, [Unit::V(4), Unit::V(5)]);
        assert_eq!(reads(&o)[0], Unit::V(1));
    }
    #[test]
    fn m0_is_124_on_this_generation() {
        // s_mov_b32 m0, s3 (the geometry program's output window)
        let o = ops(&[0xbefc_0303]);
        assert_eq!(o.defs, [Unit::S(M0)]);
        assert_eq!(M0, 124);
        // s_sendmsg reads it.
        assert_eq!(reads(&ops(&[0xbf90_0022])), [Unit::S(M0)]);
    }

    #[test]
    fn v_nop_padding_reads_nothing() {
        // VOP3 v_nop with junk source fields and a literal.
        let o = ops(&[0xd580_0116, 0x03ff_b1cf, 0x18a7_6433]);
        assert!(o.defs.is_empty() && o.uses.is_empty());
    }

    #[test]
    fn interpolation_source_zero_is_an_attribute() {
        // v_interp_p1ll_f16 v12, attr, v0
        let o = ops(&[0xd742_000c, 0x0002_0041]);
        let r = reads(&o);
        assert!(r.contains(&Unit::S(M0)) && r.contains(&Unit::V(0)));
        assert!(!r.contains(&Unit::S(0x41)));
    }

    #[test]
    fn image_address_counts() {
        assert_eq!(image_addresses("image_sample", 1, false), (2, true));
        assert_eq!(image_addresses("image_sample", 1, true), (1, true));
        assert_eq!(image_addresses("image_sample_c_l", 2, false), (5, true));
        assert_eq!(image_addresses("image_sample_d", 1, false), (6, true));
    }
}
