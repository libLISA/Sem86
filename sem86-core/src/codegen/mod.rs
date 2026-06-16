use std::fmt::Display;

use liblisa::arch::Arch;
use serde::{Deserialize, Serialize};

use crate::arch::intel386::{Intel386, State};

pub mod backends;
pub mod components;
pub mod functions;
pub mod graph_traits;
pub mod lir;
pub mod mir;
pub mod mm;
pub mod page;
pub mod see;
pub mod singlepass;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Ptr {
    CpuState,
    K,
}

impl Ptr {
    pub fn ptr_hint(&self, offset: u16) -> impl Display {
        match self {
            Ptr::CpuState => Intel386::iter_regs()
                .find(|&reg| State::byte_offset_of(reg) == offset as usize)
                .map(|reg| format!("{reg}"))
                .unwrap_or_default(),
            Ptr::K => String::from("K"),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DataSize {
    /// A single byte.
    Byte,

    /// Two bytes, naturally aligned.
    Word,

    /// Four bytes, naturally aligned.
    Dword,

    /// Eight bytes, naturally aligned.
    Qword,

    /// The lower 10 bytes of a 16-byte value, aligned to at least 8 bytes.
    F80,

    /// A 16-byte value, aligned to at least 8 bytes.
    Oword,
}

impl DataSize {
    pub fn try_from_bits(bits: usize) -> Option<Self> {
        Some(match bits {
            8 => DataSize::Byte,
            16 => DataSize::Word,
            32 => DataSize::Dword,
            64 => DataSize::Qword,
            80 => DataSize::F80,
            128 => DataSize::Oword,
            _ => return None,
        })
    }

    fn num_bits(&self) -> usize {
        self.num_bytes() * 8
    }

    fn num_bytes(&self) -> usize {
        match self {
            DataSize::Byte => 1,
            DataSize::Word => 2,
            DataSize::Dword => 4,
            DataSize::Qword => 8,
            DataSize::F80 => 10,
            DataSize::Oword => 16,
        }
    }

    fn try_from_bytes(size: usize) -> Option<DataSize> {
        Self::try_from_bits(size * 8)
    }
}

impl TryFrom<usize> for DataSize {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Byte,
            2 => Self::Word,
            4 => Self::Dword,
            8 => Self::Qword,
            10 => Self::F80,
            16 => Self::Oword,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use liblisa::Instruction;
    use liblisa::encoding::dataflows::MemoryAccesses;
    use liblisa::encoding::prefixes::EquivalentPrefixes;
    use liblisa::encoding::{Encoding, IgnoredMetadata, ParLoc, UnsizedParLoc};
    use liblisa::state::Size;
    use test_log::test;

    use crate::arch::intel386::{GpReg, Intel386, Reg};
    use crate::codegen::lir::MirToLir;
    use crate::codegen::mir::{EncodingEntry, MirBuilder};
    use crate::il::part_values::{PackingStructure, PartValues};
    use crate::il::{BinOp, BorrowEncoding, Cmd, Jump, MiniSem, Op, Val};

    #[test]
    fn rip_increments_should_fold() {
        let e = Encoding::<Intel386, MiniSem<Intel386>, IgnoredMetadata> {
            bits: Vec::new(),
            equivalent_prefixes: EquivalentPrefixes::new_matching_empty_sequence(0),
            parts: Vec::new(),
            semantics: MiniSem {
                name: String::from("test"),
                addresses: MemoryAccesses {
                    memory: Vec::new(),
                    use_trap_flag: false,
                },
                commands: crate::il::Commands::Ops(vec![Cmd::Store {
                    to: Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                        size: Size::new(0, 1),
                    }),
                    op: Op::BinOp {
                        op: BinOp::Add,
                        args: [
                            Val::Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                                size: Size::new(0, 1),
                            }),
                            Val::const_val(7),
                        ],
                    },
                }]),
                part_packing: PackingStructure::from_part_sizes([]),
                jump: Jump::Sequential,
                is_rep: false,
            },
            metadata: None,
        };

        const N: usize = 4;

        let mut items = Vec::new();
        for _ in 0..N as u32 {
            items.push(EncodingEntry {
                instr: Some(Instruction::new(&[0x00])),
                instr_len: 1,
                encoding: e.borrow_encoding(),
                part_values: PartValues::ALL_ZERO,
                metadata: None,
                is_cs32: false,
            });
        }

        let mir = MirBuilder::build_from_sequence(true, &items);
        let lir = MirToLir::new(&mir).build();

        println!("LIR: {lir:#X?}");

        assert_eq!(lir.blocks.len(), 1);
        assert!(lir.consts.contains(&(N as u128 * 7)));
        assert!(lir.blocks[0].operations().len() <= 6);
    }
}
