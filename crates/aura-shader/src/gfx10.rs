//! Instruction decoding for the console's GPU (RDNA2, "GFX10.3").
//!
//! This is the first stage of translating a program of yeni çıkmış bir oyun
//! (the newly released game this layer runs): split the code into
//! instructions, name the encoding and opcode, and give typed access to the
//! fields the later stages need. It decides lengths exactly, because every
//! later step walks the code by them and one wrong length misreads
//! everything after it. Opcode names are kept for the instructions the
//! geometry-to-mesh translation reasons about (control flow, messages, LDS);
//! the rest keep their number until a translation step needs a name.
//!
//! The encoding rules come from the public RDNA2 instruction set reference.

/// An instruction format. The format fixes where the opcode and operands sit
/// and how long the instruction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Sop2,
    Sopk,
    Sop1,
    Sopc,
    Sopp,
    Smem,
    Vop2,
    Vop1,
    Vopc,
    Vop3,
    Vop3p,
    Vintrp,
    Ds,
    Mubuf,
    Mtbuf,
    Mimg,
    Exp,
    Flat,
    Global,
    Scratch,
}

/// Why code could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The instruction at `offset` continues past the end of the code.
    Truncated { offset: usize },
    /// No encoding starts with this word.
    UnknownEncoding { offset: usize, word: u32 },
    /// A FLAT-family instruction names the reserved segment.
    ReservedSegment { offset: usize },
}

/// One decoded instruction: its words as they appear in the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inst {
    /// Byte offset in the program.
    pub offset: usize,
    pub encoding: Encoding,
    pub opcode: u16,
    /// 1-3 dwords (a MIMG with extra address dwords can be longer).
    pub words: Vec<u32>,
}

/// Operand codes that mean "a 32-bit literal follows the instruction".
pub const LITERAL: u32 = 0xff;
/// VOP1/VOP2/VOPC source-0 codes that add a second dword: SDWA, DPP16,
/// DPP8, and DPP8 with fetch-inactive.
const EXTRA_DWORD_SRC0: [u32; 4] = [0xf9, 0xfa, 0xe9, 0xea];

/// Decodes a whole program (little-endian bytes, a multiple of 4).
pub fn decode(code: &[u8]) -> Result<Vec<Inst>, DecodeError> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset + 4 <= code.len() {
        let inst = decode_one(code, offset)?;
        offset += 4 * inst.words.len();
        out.push(inst);
    }
    Ok(out)
}

/// Decodes the instruction at byte `offset`.
pub fn decode_one(code: &[u8], offset: usize) -> Result<Inst, DecodeError> {
    let word = |i: usize| -> Result<u32, DecodeError> {
        let at = offset + 4 * i;
        code.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or(DecodeError::Truncated { offset })
    };
    let w = word(0)?;
    let literal_if = |srcs: &[u32]| usize::from(srcs.contains(&LITERAL));
    let (encoding, opcode, len): (Encoding, u32, usize) = if w >> 31 == 0 {
        let src0 = w & 0x1ff;
        let extra = usize::from(src0 == LITERAL || EXTRA_DWORD_SRC0.contains(&src0));
        match w >> 25 {
            0x3f => (Encoding::Vop1, (w >> 9) & 0xff, 1 + extra),
            0x3e => (Encoding::Vopc, (w >> 17) & 0xff, 1 + extra),
            op => {
                // The multiply-add forms with a constant operand always carry
                // it as a literal, whatever their sources say.
                let with_constant = matches!(op, 0x20 | 0x21 | 0x2c | 0x2d | 0x37 | 0x38);
                (Encoding::Vop2, op, 1 + extra.max(usize::from(with_constant)))
            }
        }
    } else if w >> 30 == 0b10 {
        match w >> 23 {
            0x17d => (Encoding::Sop1, (w >> 8) & 0xff, 1 + literal_if(&[w & 0xff])),
            0x17e => (Encoding::Sopc, (w >> 16) & 0x7f, 1 + literal_if(&[w & 0xff, (w >> 8) & 0xff])),
            0x17f => (Encoding::Sopp, (w >> 16) & 0x7f, 1),
            _ if w >> 28 == 0b1011 => {
                let op = (w >> 23) & 0x1f;
                // s_setreg_imm32_b32 takes its value as a literal.
                (Encoding::Sopk, op, 1 + usize::from(op == 0x15))
            }
            _ => (Encoding::Sop2, (w >> 23) & 0x7f, 1 + literal_if(&[w & 0xff, (w >> 8) & 0xff])),
        }
    } else {
        let vop3_len = |w1: u32| 2 + literal_if(&[w1 & 0x1ff, (w1 >> 9) & 0x1ff, (w1 >> 18) & 0x1ff]);
        match w >> 26 {
            0x32 => (Encoding::Vintrp, (w >> 16) & 0x3, 1),
            0x33 => (Encoding::Vop3p, (w >> 16) & 0x7f, vop3_len(word(1)?)),
            0x35 => (Encoding::Vop3, (w >> 16) & 0x3ff, vop3_len(word(1)?)),
            0x36 => (Encoding::Ds, (w >> 18) & 0xff, 2),
            0x37 => {
                let encoding = match (w >> 14) & 3 {
                    0 => Encoding::Flat,
                    1 => Encoding::Scratch,
                    2 => Encoding::Global,
                    _ => return Err(DecodeError::ReservedSegment { offset }),
                };
                (encoding, (w >> 18) & 0x7f, 2)
            }
            0x38 => (Encoding::Mubuf, ((w >> 18) & 0x7f) | ((w >> 25) & 1) << 7, 2),
            0x3a => (Encoding::Mtbuf, ((w >> 16) & 0x7) | ((word(1)? >> 21) & 1) << 3, 2),
            // Bits 2:1 count the extra dwords of non-sequential addresses.
            0x3c => (Encoding::Mimg, ((w >> 18) & 0x7f) | (w & 1) << 7, 2 + ((w >> 1) & 3) as usize),
            0x3d => (Encoding::Smem, (w >> 18) & 0xff, 2),
            0x3e => (Encoding::Exp, 0, 2),
            _ => return Err(DecodeError::UnknownEncoding { offset, word: w }),
        }
    };
    Ok(Inst {
        offset,
        encoding,
        opcode: opcode as u16,
        words: (0..len).map(word).collect::<Result<_, _>>()?,
    })
}

impl Inst {
    /// The opcode's name, for the opcodes this crate names so far.
    pub fn name(&self) -> Option<&'static str> {
        let table: &[(u16, &str)] = match self.encoding {
            Encoding::Sopp => SOPP,
            Encoding::Sop1 => SOP1,
            Encoding::Sop2 => SOP2,
            Encoding::Sopk => SOPK,
            Encoding::Smem => SMEM,
            Encoding::Ds => DS,
            Encoding::Exp => return Some("exp"),
            _ => return None,
        };
        table.iter().find(|(op, _)| *op == self.opcode).map(|(_, n)| *n)
    }

    /// The 16-bit immediate of a SOPP or SOPK instruction.
    pub fn simm16(&self) -> u16 {
        self.words[0] as u16
    }

    /// The branch target of a SOPP branch: the next instruction plus the
    /// signed dword offset.
    pub fn branch_target(&self) -> usize {
        let next = self.offset as i64 + 4;
        (next + 4 * i64::from(self.simm16() as i16)) as usize
    }

    /// The literal dword, if the instruction carries one.
    pub fn literal(&self) -> Option<u32> {
        let base = match self.encoding {
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk => 1,
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc => 1,
            Encoding::Vop3 | Encoding::Vop3p => 2,
            _ => return None,
        };
        let sdwa_or_dpp = matches!(self.encoding, Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc)
            && EXTRA_DWORD_SRC0.contains(&(self.words[0] & 0x1ff));
        if sdwa_or_dpp {
            return None;
        }
        self.words.get(base).copied()
    }

    /// The message of an `s_sendmsg`/`s_sendmsghalt`.
    pub fn message(&self) -> Option<Message> {
        if self.encoding != Encoding::Sopp || !matches!(self.opcode, 0x10 | 0x11) {
            return None;
        }
        let imm = self.simm16();
        Some(match imm & 0xf {
            2 | 3 => {
                let op = match (imm >> 4) & 3 {
                    0 => GsOp::Nop,
                    1 => GsOp::Cut,
                    2 => GsOp::Emit,
                    _ => GsOp::EmitCut,
                };
                let stream = ((imm >> 8) & 3) as u8;
                if imm & 0xf == 2 {
                    Message::Gs { op, stream }
                } else {
                    Message::GsDone { op, stream }
                }
            }
            9 => Message::GsAllocReq,
            id => Message::Other(id as u8),
        })
    }

    /// The fields of a DS (LDS) instruction.
    pub fn ds(&self) -> Option<Ds> {
        if self.encoding != Encoding::Ds {
            return None;
        }
        let (w0, w1) = (self.words[0], self.words[1]);
        Some(Ds {
            offset0: (w0 & 0xff) as u8,
            offset1: ((w0 >> 8) & 0xff) as u8,
            gds: (w0 >> 17) & 1 == 1,
            addr: (w1 & 0xff) as u8,
            data0: ((w1 >> 8) & 0xff) as u8,
            data1: ((w1 >> 16) & 0xff) as u8,
            vdst: ((w1 >> 24) & 0xff) as u8,
        })
    }

    /// The fields of an SMEM (scalar memory) instruction.
    pub fn smem(&self) -> Option<Smem> {
        if self.encoding != Encoding::Smem {
            return None;
        }
        let (w0, w1) = (self.words[0], self.words[1]);
        // The offset is a signed 21-bit byte offset.
        let raw = w1 & 0x1f_ffff;
        let offset = ((raw << 11) as i32) >> 11;
        Some(Smem {
            sbase: ((w0 & 0x3f) << 1) as u8,
            sdata: ((w0 >> 6) & 0x7f) as u8,
            offset,
            soffset: (w1 >> 25) as u8,
        })
    }
}

/// What `s_sendmsg` tells the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// A geometry-stage output step: emit a vertex and/or end a strip.
    Gs { op: GsOp, stream: u8 },
    /// The wave has finished its geometry output.
    GsDone { op: GsOp, stream: u8 },
    /// Reserve output space for the primitives of a merged geometry wave.
    GsAllocReq,
    Other(u8),
}

/// The geometry operation of a GS message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GsOp {
    Nop,
    Cut,
    Emit,
    EmitCut,
}

/// DS fields: LDS address and data VGPRs, two 8-bit offsets (one 16-bit
/// offset for single-address forms: `offset1 << 8 | offset0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ds {
    pub offset0: u8,
    pub offset1: u8,
    pub gds: bool,
    pub addr: u8,
    pub data0: u8,
    pub data1: u8,
    pub vdst: u8,
}

impl Ds {
    pub fn offset16(&self) -> u16 {
        u16::from(self.offset1) << 8 | u16::from(self.offset0)
    }
}

/// SMEM fields: base SGPR pair, first data SGPR, byte offset, offset SGPR
/// (`0x7d` = none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Smem {
    pub sbase: u8,
    pub sdata: u8,
    pub offset: i32,
    pub soffset: u8,
}

const SOPP: &[(u16, &str)] = &[
    (0x00, "s_nop"),
    (0x01, "s_endpgm"),
    (0x02, "s_branch"),
    (0x03, "s_wakeup"),
    (0x04, "s_cbranch_scc0"),
    (0x05, "s_cbranch_scc1"),
    (0x06, "s_cbranch_vccz"),
    (0x07, "s_cbranch_vccnz"),
    (0x08, "s_cbranch_execz"),
    (0x09, "s_cbranch_execnz"),
    (0x0a, "s_barrier"),
    (0x0b, "s_setkill"),
    (0x0c, "s_waitcnt"),
    (0x0d, "s_sethalt"),
    (0x0e, "s_sleep"),
    (0x0f, "s_setprio"),
    (0x10, "s_sendmsg"),
    (0x11, "s_sendmsghalt"),
    (0x12, "s_trap"),
    (0x13, "s_icache_inv"),
    (0x14, "s_incperflevel"),
    (0x15, "s_decperflevel"),
    (0x16, "s_ttracedata"),
    (0x1b, "s_endpgm_saved"),
    (0x1e, "s_endpgm_ordered_ps_done"),
    (0x1f, "s_code_end"),
    (0x20, "s_inst_prefetch"),
    (0x21, "s_clause"),
    (0x22, "s_wait_idle"),
    (0x23, "s_waitcnt_depctr"),
    (0x24, "s_round_mode"),
    (0x25, "s_denorm_mode"),
];

const SOP1: &[(u16, &str)] = &[
    (0x03, "s_mov_b32"),
    (0x04, "s_mov_b64"),
    (0x1f, "s_getpc_b64"),
    (0x20, "s_setpc_b64"),
    (0x21, "s_swappc_b64"),
    (0x22, "s_rfe_b64"),
    (0x24, "s_and_saveexec_b64"),
];

const SOPK: &[(u16, &str)] = &[
    (0x15, "s_setreg_imm32_b32"),
    (0x16, "s_call_b64"),
    (0x1b, "s_subvector_loop_begin"),
    (0x1c, "s_subvector_loop_end"),
];

const SOP2: &[(u16, &str)] = &[
    (0x00, "s_add_u32"),
    (0x0e, "s_and_b32"),
    (0x20, "s_lshr_b32"),
    (0x25, "s_bfm_b64"),
    (0x27, "s_bfe_u32"),
];

const SMEM: &[(u16, &str)] = &[
    (0x00, "s_load_dword"),
    (0x01, "s_load_dwordx2"),
    (0x02, "s_load_dwordx4"),
    (0x08, "s_buffer_load_dword"),
];

const DS: &[(u16, &str)] = &[
    (0x00, "ds_add_u32"),
    (0x0d, "ds_write_b32"),
    (0x0e, "ds_write2_b32"),
    (0x0f, "ds_write2st64_b32"),
    (0x35, "ds_swizzle_b32"),
    (0x36, "ds_read_b32"),
    (0x37, "ds_read2_b32"),
    (0x38, "ds_read2st64_b32"),
    (0x3c, "ds_read_u16"),
    (0x3d, "ds_consume"),
    (0x3e, "ds_append"),
    (0x3f, "ds_ordered_count"),
    (0x4d, "ds_write_b64"),
    (0x4e, "ds_write2_b64"),
    (0x76, "ds_read_b64"),
    (0x77, "ds_read2_b64"),
    (0xb3, "ds_bpermute_b32"),
    (0xde, "ds_write_b96"),
    (0xdf, "ds_write_b128"),
    (0xfe, "ds_read_b96"),
    (0xff, "ds_read_b128"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    const fn sopp(op: u32, simm16: u16) -> u32 {
        0xbf80_0000 | op << 16 | simm16 as u32
    }

    #[test]
    fn scalar_lengths_and_literals() {
        // s_mov_b32 s0, 0x12345678 (literal) ; s_mov_b32 s1, s2 ; s_endpgm
        let code = bytes(&[0xbe80_03ff, 0x1234_5678, 0xbe81_0302, sopp(0x01, 0)]);
        let insts = decode(&code).unwrap();
        assert_eq!(insts.len(), 3);
        assert_eq!(insts[0].name(), Some("s_mov_b32"));
        assert_eq!(insts[0].literal(), Some(0x1234_5678));
        assert_eq!(insts[1].words.len(), 1);
        assert_eq!(insts[1].literal(), None);
        assert_eq!(insts[2].name(), Some("s_endpgm"));
        assert_eq!(insts[2].offset, 12);
    }

    #[test]
    fn sop2_bfm_and_branches() {
        // s_bfm_b64 s[0:1], s3, 0 ; s_cbranch_execz +2
        let bfm = 0x8000_0000 | 0x25 << 23 | 0x80 << 8 | 3;
        let code = bytes(&[bfm, sopp(0x08, 2)]);
        let insts = decode(&code).unwrap();
        assert_eq!(insts[0].encoding, Encoding::Sop2);
        assert_eq!(insts[0].name(), Some("s_bfm_b64"));
        assert_eq!(insts[1].name(), Some("s_cbranch_execz"));
        assert_eq!(insts[1].branch_target(), 4 + 4 + 8);
        // A backward branch.
        let back = decode_one(&bytes(&[sopp(0x02, (-3i16) as u16)]), 0).unwrap();
        assert_eq!(back.branch_target() as i64, 4 - 12);
    }

    #[test]
    fn sopk_setreg_imm32_carries_a_literal() {
        let setreg = 0xb000_0000 | 0x15 << 23 | 0x1234;
        let insts = decode(&bytes(&[setreg, 0xdead_beef, sopp(0, 0)])).unwrap();
        assert_eq!(insts[0].encoding, Encoding::Sopk);
        assert_eq!(insts[0].words.len(), 2);
        assert_eq!(insts[0].literal(), Some(0xdead_beef));
    }

    #[test]
    fn vector_extra_dwords() {
        // v_mov_b32 v0, v1 ; v_mov_b32 v0, literal ; v_mov_b32 with DPP16
        let vmov = |src0: u32| 0x7e00_0000 | 1 << 9 | src0;
        let code = bytes(&[vmov(0x101), vmov(0xff), 0x3f80_0000, vmov(0xfa), 0x0000_00e4]);
        let insts = decode(&code).unwrap();
        assert_eq!(insts.iter().map(|i| i.words.len()).collect::<Vec<_>>(), [1, 2, 2]);
        assert!(insts.iter().all(|i| i.encoding == Encoding::Vop1 && i.opcode == 1));
        assert_eq!(insts[1].literal(), Some(0x3f80_0000));
        assert_eq!(insts[2].literal(), None, "DPP control is not a literal");
        // VOP2 v_fmamk_f32 always has its constant.
        let fmamk = 0x2c << 25 | 1 << 9 | 0x100;
        assert_eq!(decode_one(&bytes(&[fmamk, 0x4000_0000]), 0).unwrap().words.len(), 2);
    }

    #[test]
    fn vop3_literal_in_any_source() {
        // VOP3 v_mbcnt_lo_u32_b32 v0, -1, 0 then with src1 = literal.
        let w0 = 0xd400_0000 | 0x365 << 16;
        let plain = decode(&bytes(&[w0, 0xc1 | 0x80 << 9])).unwrap();
        assert_eq!(plain[0].encoding, Encoding::Vop3);
        assert_eq!(plain[0].opcode, 0x365);
        assert_eq!(plain[0].words.len(), 2);
        let lit = decode(&bytes(&[w0, 0xc1 | 0xff << 9, 7])).unwrap();
        assert_eq!(lit[0].words.len(), 3);
        assert_eq!(lit[0].literal(), Some(7));
    }

    #[test]
    fn ds_fields() {
        // ds_read2_b32 v[4:5], v1 offset0:2 offset1:3
        let w0 = 0xd800_0000 | 0x37 << 18 | 3 << 8 | 2;
        let w1 = 4 << 24 | 1;
        let inst = decode_one(&bytes(&[w0, w1]), 0).unwrap();
        assert_eq!(inst.name(), Some("ds_read2_b32"));
        let ds = inst.ds().unwrap();
        assert_eq!((ds.addr, ds.vdst, ds.offset0, ds.offset1), (1, 4, 2, 3));
        assert_eq!(ds.offset16(), 0x0302);
        assert!(!ds.gds);
    }

    #[test]
    fn smem_fields_and_negative_offsets() {
        // s_load_dword s0, s[0:1], 0xb8 (the geometry program's ring base)
        let w0 = 0xf400_0000;
        let w1 = 0xfa00_0000 | 0xb8;
        let inst = decode_one(&bytes(&[w0, w1]), 0).unwrap();
        assert_eq!(inst.name(), Some("s_load_dword"));
        let s = inst.smem().unwrap();
        assert_eq!((s.sbase, s.sdata, s.offset, s.soffset), (0, 0, 0xb8, 0x7d));
        let neg = decode_one(&bytes(&[w0, 0xfa00_0000 | 0x1f_fffc]), 0).unwrap();
        assert_eq!(neg.smem().unwrap().offset, -4);
    }

    #[test]
    fn gs_messages() {
        let msg = |imm| decode_one(&bytes(&[sopp(0x10, imm)]), 0).unwrap().message();
        assert_eq!(msg(0x22), Some(Message::Gs { op: GsOp::Emit, stream: 0 }));
        assert_eq!(msg(0x12), Some(Message::Gs { op: GsOp::Cut, stream: 0 }));
        assert_eq!(msg(0x132), Some(Message::Gs { op: GsOp::EmitCut, stream: 1 }));
        assert_eq!(msg(0x03), Some(Message::GsDone { op: GsOp::Nop, stream: 0 }));
        assert_eq!(msg(0x09), Some(Message::GsAllocReq));
        assert_eq!(decode_one(&bytes(&[sopp(0, 0)]), 0).unwrap().message(), None);
    }

    #[test]
    fn memory_encodings_and_mimg_address_dwords() {
        // exp, global_load (segment 2), mimg with one extra address dword.
        let exp = 0xf800_0000;
        let global = 0xdc00_0000 | 2 << 14;
        let mimg = 0xf000_0000 | 1 << 1;
        let code = bytes(&[exp, 0, global, 0, mimg, 0, 0]);
        let insts = decode(&code).unwrap();
        assert_eq!(
            insts.iter().map(|i| (i.encoding, i.words.len())).collect::<Vec<_>>(),
            [(Encoding::Exp, 2), (Encoding::Global, 2), (Encoding::Mimg, 3)]
        );
        assert_eq!(insts[0].name(), Some("exp"));
    }

    #[test]
    fn bad_code_is_reported() {
        assert_eq!(
            decode(&bytes(&[0xbe80_03ff])),
            Err(DecodeError::Truncated { offset: 0 })
        );
        assert_eq!(
            decode(&bytes(&[0xdc00_c000, 0])),
            Err(DecodeError::ReservedSegment { offset: 0 })
        );
        assert!(matches!(
            decode(&bytes(&[0xfc00_0000])),
            Err(DecodeError::UnknownEncoding { offset: 0, .. })
        ));
    }
}
