//! Where each scalar register a console shader starts with gets its value.
//!
//! A shader for the console GPU (RDNA2) begins with some SGPRs already
//! filled: system values the hardware writes, then "user data" dwords the
//! driver writes from the draw's root arguments. The program's container
//! declares a root layout (which dwords hold which root parameter), but the
//! layout does not tell the whole story, and a translator that trusts it
//! alone gets programs of yeni çıkmış bir oyun (the newly released game this
//! layer runs) wrong:
//!
//! - **Merged stages** (geometry, hull) run as one wave with the stage before
//!   them, so the hardware puts 8 system SGPRs first and user data starts at
//!   s8. Pixel and compute programs have no such block.
//! - **The engine prefix**: in every graphics container we have read, the
//!   root layout starts at user dword 8. Dwords 0..7 are declared by no root,
//!   yet the engine writes them on every draw and programs read them (a
//!   geometry program follows dword 0:1 as a 40-bit pointer to find its LDS
//!   ring). Refusing them as "undeclared" drops the whole program.
//! - **Above the block**: a program may read the SGPR just past its user
//!   data, which the console leaves 0 (a 64-bit pair whose high half is the
//!   next register, for instance). Reading 0 there is correct, not a guess.
//! - **POPS** (primitive-ordered pixel shading): pixel programs that support it
//!   read a control dword after the user block. With POPS off (bit 31 clear)
//!   they take their unordered path, which is what a host GPU can run.
//!
//! The geometry stage's system SGPRs carry what makes the program run at all:
//! s3 bits 15:8 are the number of primitives this wave handles, which the
//! program turns into its EXEC mask. Supplying 0 there (the usual "unknown
//! register reads 0" escape) silently produces a program that does nothing.
//! [`GsWaveInfo`] encodes those fields so the mesh-shader translation can
//! supply them from the draw instead.

/// The hardware stage a program runs as on the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Pixel,
    Compute,
    /// The vertex stage when it runs alone (no geometry or tessellation).
    Vertex,
    /// Geometry, merged with the export stage before it into one wave.
    Geometry,
    /// Hull, merged with the local stage before it into one wave.
    Hull,
}

/// System SGPRs the hardware fills before user data in merged stages.
pub const MERGED_SYSTEM_SGPRS: u32 = 8;

/// User dwords the engine writes on every graphics draw without declaring
/// them in the root layout.
pub const ENGINE_PREFIX_DWORDS: u32 = 8;

impl Stage {
    /// The SGPR holding user dword 0.
    pub fn user_base(self) -> u32 {
        match self {
            Stage::Geometry | Stage::Hull => MERGED_SYSTEM_SGPRS,
            Stage::Pixel | Stage::Compute | Stage::Vertex => 0,
        }
    }

    /// Whether the engine prefix applies (graphics stages only; compute
    /// programs declare everything they read).
    pub fn has_engine_prefix(self) -> bool {
        self != Stage::Compute
    }
}

/// What kind of value a root parameter places in user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// 32-bit address of a descriptor table.
    Table,
    /// A 64-bit buffer address.
    Descriptor,
    /// Inline constants.
    Constants,
    /// Base of the resource descriptor heap.
    ResourceHeap,
    /// Base of the sampler descriptor heap.
    SamplerHeap,
}

/// One root parameter as the container declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Root {
    pub kind: RootKind,
    /// First user dword it occupies.
    pub user_data: u32,
    /// How many dwords (SGPRs) it occupies.
    pub sgprs: u32,
}

/// Where an input SGPR's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A system SGPR of a merged stage (index 0..8).
    System(u32),
    /// A dword of the engine's undeclared prefix (0..8).
    EnginePrefix(u32),
    /// Dword `dword` of root parameter `root` (index into the layout).
    Root { root: usize, dword: u32 },
    /// A user dword no root declares and outside the engine prefix: the
    /// draw leaves it unwritten.
    Undeclared(u32),
    /// The POPS control dword, one SGPR past the user block (the SGPR in
    /// between reads 0); 0 = POPS off.
    PopsControl,
    /// Above everything the hardware fills: reads 0 on the console.
    Zero,
}

/// The input SGPRs of one program: its stage and declared root layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub stage: Stage,
    pub roots: Vec<Root>,
    /// Whether the program is a pixel program built with POPS support (its
    /// container says so; it then reads the control dword).
    pub pops: bool,
}

impl Inputs {
    /// User dwords the hardware loads: through the last declared root, and
    /// at least the engine prefix in graphics stages.
    pub fn user_dwords(&self) -> u32 {
        let declared = self.roots.iter().map(|r| r.user_data + r.sgprs).max().unwrap_or(0);
        if self.stage.has_engine_prefix() {
            declared.max(ENGINE_PREFIX_DWORDS)
        } else {
            declared
        }
    }

    /// SGPRs initialised at entry (system + user block). Everything at or
    /// above this reads as described by [`Inputs::source`].
    pub fn initialized_sgprs(&self) -> u32 {
        self.stage.user_base() + self.user_dwords()
    }

    /// Where SGPR `n` gets its value at entry.
    pub fn source(&self, n: u32) -> Input {
        let base = self.stage.user_base();
        if n < base {
            return Input::System(n);
        }
        let end = self.initialized_sgprs();
        if n >= end {
            // The SGPR right after the block reads 0; the one after that is
            // where the measured POPS programs find their control dword.
            return if self.pops && self.stage == Stage::Pixel && n == end + 1 {
                Input::PopsControl
            } else {
                Input::Zero
            };
        }
        let dword = n - base;
        if let Some((root, r)) = self
            .roots
            .iter()
            .enumerate()
            .find(|(_, r)| (r.user_data..r.user_data + r.sgprs).contains(&dword))
        {
            return Input::Root {
                root,
                dword: dword - r.user_data,
            };
        }
        if self.stage.has_engine_prefix() && dword < ENGINE_PREFIX_DWORDS {
            Input::EnginePrefix(dword)
        } else {
            Input::Undeclared(dword)
        }
    }
}

/// The geometry wave's information SGPR (s3 of a merged geometry stage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GsWaveInfo {
    /// Primitives this wave handles (bits 15:8). The program builds its
    /// EXEC mask from it; 0 means the wave does nothing.
    pub primitives: u8,
    /// Bits 23:16, which the program copies into M0 (the LDS window it
    /// writes its output through).
    pub m0_field: u8,
}

/// The SGPR of a merged geometry stage that holds [`GsWaveInfo`].
pub const GS_WAVE_INFO_SGPR: u32 = 3;
/// The SGPR of a merged geometry stage that holds the wave's LDS offset
/// component, added to every LDS address the program forms.
pub const GS_LDS_OFFSET_SGPR: u32 = 2;

impl GsWaveInfo {
    pub fn encode(self) -> u32 {
        (u32::from(self.primitives) << 8) | (u32::from(self.m0_field) << 16)
    }

    pub fn decode(value: u32) -> GsWaveInfo {
        GsWaveInfo {
            primitives: (value >> 8) as u8,
            m0_field: (value >> 16) as u8,
        }
    }

    /// The EXEC mask the program derives from the primitive count
    /// (`s_bfm_b64`: the low `primitives` bits set), for a 64-lane wave.
    pub fn exec(self) -> u64 {
        match self.primitives {
            0 => 0,
            n if n >= 64 => u64::MAX,
            n => (1u64 << n) - 1,
        }
    }
}

/// The engine prefix's dword 0:1 as the geometry program uses it: a 40-bit
/// address (dword 1 contributes only its low 8 bits).
pub fn prefix_pointer(dword0: u32, dword1: u32) -> u64 {
    u64::from(dword0) | (u64::from(dword1 & 0xff) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixel layout measured from the console containers: tables at
    /// dwords 8, 9, 10, a descriptor at 11:12, constants at 13:14, the two
    /// heaps at 15 and 16. Nothing declares dwords 0..7.
    fn pixel() -> Inputs {
        Inputs {
            stage: Stage::Pixel,
            roots: vec![
                Root { kind: RootKind::Table, user_data: 8, sgprs: 1 },
                Root { kind: RootKind::Table, user_data: 9, sgprs: 1 },
                Root { kind: RootKind::Table, user_data: 10, sgprs: 1 },
                Root { kind: RootKind::Descriptor, user_data: 11, sgprs: 2 },
                Root { kind: RootKind::Constants, user_data: 13, sgprs: 2 },
                Root { kind: RootKind::ResourceHeap, user_data: 15, sgprs: 1 },
                Root { kind: RootKind::SamplerHeap, user_data: 16, sgprs: 1 },
            ],
            pops: true,
        }
    }

    /// The geometry layout: a table at dword 8, constants at 11:12, heaps at
    /// 13 and 14; user dword N sits in s(N+8).
    fn geometry() -> Inputs {
        Inputs {
            stage: Stage::Geometry,
            roots: vec![
                Root { kind: RootKind::Table, user_data: 8, sgprs: 1 },
                Root { kind: RootKind::Constants, user_data: 11, sgprs: 2 },
                Root { kind: RootKind::ResourceHeap, user_data: 13, sgprs: 1 },
                Root { kind: RootKind::SamplerHeap, user_data: 14, sgprs: 1 },
            ],
            pops: false,
        }
    }

    #[test]
    fn pixel_inputs_match_the_console() {
        let p = pixel();
        assert_eq!(p.initialized_sgprs(), 17);
        assert_eq!(p.source(0), Input::EnginePrefix(0));
        assert_eq!(p.source(7), Input::EnginePrefix(7));
        assert_eq!(p.source(8), Input::Root { root: 0, dword: 0 });
        assert_eq!(p.source(12), Input::Root { root: 3, dword: 1 });
        assert_eq!(p.source(16), Input::Root { root: 6, dword: 0 });
        // s16:s17 is read as a pair: s17 must be 0, then the POPS dword.
        assert_eq!(p.source(17), Input::Zero);
        assert_eq!(p.source(18), Input::PopsControl);
        assert_eq!(p.source(19), Input::Zero);
        let no_pops = Inputs { pops: false, ..pixel() };
        assert_eq!(no_pops.source(18), Input::Zero);
    }

    #[test]
    fn geometry_inputs_match_the_console() {
        let g = geometry();
        assert_eq!(g.stage.user_base(), 8);
        assert_eq!(g.initialized_sgprs(), 8 + 15);
        assert_eq!(g.source(GS_LDS_OFFSET_SGPR), Input::System(2));
        assert_eq!(g.source(GS_WAVE_INFO_SGPR), Input::System(3));
        // The pointer the program follows: user dwords 0:1 in s8:s9.
        assert_eq!(g.source(8), Input::EnginePrefix(0));
        assert_eq!(g.source(9), Input::EnginePrefix(1));
        assert_eq!(g.source(16), Input::Root { root: 0, dword: 0 });
        // Dwords 9 and 10 lie between declared roots, past the prefix.
        assert_eq!(g.source(17), Input::Undeclared(9));
        assert_eq!(g.source(19), Input::Root { root: 1, dword: 0 });
        assert_eq!(g.source(23), Input::Zero);
    }

    #[test]
    fn compute_has_no_prefix_or_system_block() {
        let c = Inputs {
            stage: Stage::Compute,
            roots: vec![Root { kind: RootKind::Constants, user_data: 0, sgprs: 4 }],
            pops: false,
        };
        assert_eq!(c.initialized_sgprs(), 4);
        assert_eq!(c.source(0), Input::Root { root: 0, dword: 0 });
        assert_eq!(c.source(4), Input::Zero);
    }

    #[test]
    fn a_graphics_program_without_roots_still_gets_the_prefix() {
        let v = Inputs { stage: Stage::Vertex, roots: vec![], pops: false };
        assert_eq!(v.initialized_sgprs(), 8);
        assert_eq!(v.source(3), Input::EnginePrefix(3));
        assert_eq!(v.source(8), Input::Zero);
    }

    #[test]
    fn gs_wave_info_round_trips_and_builds_exec() {
        let info = GsWaveInfo { primitives: 5, m0_field: 0x2a };
        assert_eq!(info.encode(), 0x2a_05_00);
        assert_eq!(GsWaveInfo::decode(info.encode()), info);
        assert_eq!(info.exec(), 0b11111);
        assert_eq!(GsWaveInfo::default().exec(), 0, "0 primitives: the wave does nothing");
        assert_eq!(GsWaveInfo { primitives: 64, m0_field: 0 }.exec(), u64::MAX);
    }

    #[test]
    fn prefix_pointer_is_forty_bits() {
        assert_eq!(prefix_pointer(0x8000_1000, 0xffff_ff12), 0x12_8000_1000);
    }
}
