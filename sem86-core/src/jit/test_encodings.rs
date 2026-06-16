use AccessKind::*;
use BinOp::*;
use Commands::Ops;
use UnOp::*;
use Val::*;
use itertools::Itertools;
use liblisa::encoding::bitpattern::{Bit, Part, PartMapping};
use liblisa::encoding::dataflows::{
    AccessKind, AddrSize, AddrTerm, AddressComputation, Inputs, MemoryAccess, MemoryAccesses, MemorySizeRange,
    ParameterizedComputation,
};
use liblisa::encoding::prefixes::{EquivalentPrefixes, PrefixSequence, SubstitutionSequence};
use liblisa::encoding::{Encoding, IgnoredMetadata, ParLoc, UnsizedParLoc};
use liblisa::state::Size;

use crate::arch::intel386::{FLAG_DF, FLAG_RF, GpReg, Intel386, Reg};
use crate::il::part_values::PackingStructure;
use crate::il::{BinOp, Cmd, Commands, Jump, MiniSem, Op, UnOp, Val};

pub fn empty() -> Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata> {
    Encoding {
        bits: vec![
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(0),
        ]
        .into_iter()
        .map_into()
        .collect(),
        equivalent_prefixes: EquivalentPrefixes::new_matching_empty_sequence(0),
        parts: Vec::new(),
        semantics: MiniSem {
            name: String::from("empty_nopart"),
            addresses: MemoryAccesses {
                memory: Vec::new(),
                use_trap_flag: false,
            },
            commands: Commands::Ops(Vec::new()),
            part_packing: PackingStructure::from_part_sizes([]),
            jump: Jump::Sequential,
            is_rep: false,
        },
        metadata: None,
    }
}

pub fn movs_repe() -> Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata> {
    Encoding {
        bits: vec![
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(1),
            Bit::Fixed(1),
        ]
        .into_iter()
        .map_into()
        .collect(),
        equivalent_prefixes: EquivalentPrefixes::from_edges(
            3,
            [
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::EquivalentTo(PrefixSequence::new([0xF3, 0x66, 0x67])),
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
                SubstitutionSequence::NotEquivalent,
            ],
            [
                (0, 0x2E, 1),
                (0, 0x3E, 1),
                (0, 0x26, 1),
                (0, 0x36, 1),
                (0, 0x64, 1),
                (0, 0x65, 1),
                (0, 0x66, 2),
                (0, 0x67, 6),
                (0, 0xF0, 0),
                (0, 0xF2, 0),
                (0, 0xF3, 8),
                (1, 0x2E, 1),
                (1, 0x3E, 1),
                (1, 0x26, 1),
                (1, 0x36, 1),
                (1, 0x64, 1),
                (1, 0x65, 1),
                (1, 0x66, 1),
                (1, 0x67, 1),
                (1, 0xF0, 1),
                (1, 0xF2, 1),
                (1, 0xF3, 1),
                (2, 0x2E, 1),
                (2, 0x3E, 1),
                (2, 0x26, 1),
                (2, 0x36, 1),
                (2, 0x64, 1),
                (2, 0x65, 1),
                (2, 0x66, 2),
                (2, 0x67, 3),
                (2, 0xF0, 2),
                (2, 0xF2, 2),
                (2, 0xF3, 5),
                (3, 0x2E, 1),
                (3, 0x3E, 1),
                (3, 0x26, 1),
                (3, 0x36, 1),
                (3, 0x64, 1),
                (3, 0x65, 1),
                (3, 0x66, 3),
                (3, 0x67, 3),
                (3, 0xF0, 3),
                (3, 0xF2, 3),
                (3, 0xF3, 4),
                (4, 0x2E, 1),
                (4, 0x3E, 1),
                (4, 0x26, 1),
                (4, 0x36, 1),
                (4, 0x64, 1),
                (4, 0x65, 1),
                (4, 0x66, 4),
                (4, 0x67, 4),
                (4, 0xF0, 4),
                (4, 0xF2, 3),
                (4, 0xF3, 4),
                (5, 0x2E, 1),
                (5, 0x3E, 1),
                (5, 0x26, 1),
                (5, 0x36, 1),
                (5, 0x64, 1),
                (5, 0x65, 1),
                (5, 0x66, 5),
                (5, 0x67, 4),
                (5, 0xF0, 5),
                (5, 0xF2, 2),
                (5, 0xF3, 5),
                (6, 0x2E, 1),
                (6, 0x3E, 1),
                (6, 0x26, 1),
                (6, 0x36, 1),
                (6, 0x64, 1),
                (6, 0x65, 1),
                (6, 0x66, 3),
                (6, 0x67, 6),
                (6, 0xF0, 6),
                (6, 0xF2, 6),
                (6, 0xF3, 7),
                (7, 0x2E, 1),
                (7, 0x3E, 1),
                (7, 0x26, 1),
                (7, 0x36, 1),
                (7, 0x64, 1),
                (7, 0x65, 1),
                (7, 0x66, 4),
                (7, 0x67, 7),
                (7, 0xF0, 7),
                (7, 0xF2, 6),
                (7, 0xF3, 7),
                (8, 0x2E, 1),
                (8, 0x3E, 1),
                (8, 0x26, 1),
                (8, 0x36, 1),
                (8, 0x64, 1),
                (8, 0x65, 1),
                (8, 0x66, 5),
                (8, 0x67, 7),
                (8, 0xF0, 8),
                (8, 0xF2, 0),
                (8, 0xF3, 8),
            ],
        ),
        parts: vec![],
        semantics: MiniSem {
            name: String::from("movs_repe"),
            addresses: MemoryAccesses {
                memory: vec![
                    MemoryAccess {
                        kind: InputOutput,
                        inputs: Inputs::unsorted(vec![
                            ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::DsBase)),
                                size: Size::new(0, 3),
                            },
                            ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Si)),
                                size: Size::new(0, 3),
                            },
                        ]),
                        size: MemorySizeRange::new(4, 4),
                        calculation: ParameterizedComputation::Calculation(
                            AddressComputation::from_iter(
                                [AddrTerm::identity(AddrSize::Addr32), AddrTerm::identity(AddrSize::Addr16)].into_iter(),
                                0,
                            )
                            .with_addr_size(AddrSize::Addr32),
                        ),
                        alignment: 1,
                    },
                    MemoryAccess {
                        kind: InputOutput,
                        inputs: Inputs::unsorted(vec![
                            ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::EsBase)),
                                size: Size::new(0, 3),
                            },
                            ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Di)),
                                size: Size::new(0, 3),
                            },
                        ]),
                        size: MemorySizeRange::new(4, 4),
                        calculation: ParameterizedComputation::Calculation(
                            AddressComputation::from_iter(
                                [AddrTerm::identity(AddrSize::Addr32), AddrTerm::identity(AddrSize::Addr16)].into_iter(),
                                0,
                            )
                            .with_addr_size(AddrSize::Addr32),
                        ),
                        alignment: 1,
                    },
                ],
                use_trap_flag: true,
            },
            commands: Ops(vec![Cmd::If {
                val: Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Cx)),
                    size: Size::new(0, 3),
                }),
                if_zero: Ops(vec![Cmd::Store {
                    to: Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                        size: Size::new(0, 3),
                    }),
                    op: Op::BinOp {
                        args: [
                            Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                                size: Size::new(0, 3),
                            }),
                            Loc(ParLoc {
                                loc: UnsizedParLoc::InstrLen,
                                size: Size::new(0, 3),
                            }),
                        ],
                        op: Add,
                    },
                }]),
                if_nonzero: Ops(vec![
                    Cmd::Store {
                        to: Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Cx)),
                            size: Size::new(0, 3),
                        }),
                        op: Op::BinOp {
                            args: [
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Cx)),
                                    size: Size::new(0, 3),
                                }),
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::Const(1),
                                    size: Size::new(0, 3),
                                }),
                            ],
                            op: Sub,
                        },
                    },
                    Cmd::Store {
                        to: Temp(0),
                        op: Op::BinOp {
                            args: [
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                                    size: Size::new(0, 3),
                                }),
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::InstrLen,
                                    size: Size::new(0, 1),
                                }),
                            ],
                            op: Add,
                        },
                    },
                    Cmd::Store {
                        to: Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                            size: Size::new(0, 3),
                        }),
                        op: Op::Ite {
                            cond: Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Cx)),
                                size: Size::new(0, 3),
                            }),
                            if_nonzero: Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                                size: Size::new(0, 3),
                            }),
                            if_zero: Temp(0),
                        },
                    },
                    Cmd::Store {
                        to: Loc(FLAG_RF),
                        op: Op::Ite {
                            cond: Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Cx)),
                                size: Size::new(0, 3),
                            }),
                            if_nonzero: Loc(ParLoc {
                                loc: UnsizedParLoc::Const(1),
                                size: Size::new(0, 7),
                            }),
                            if_zero: Loc(FLAG_RF),
                        },
                    },
                    Cmd::Store {
                        to: Loc(ParLoc {
                            loc: UnsizedParLoc::Mem(1),
                            size: Size::new(0, 3),
                        }),
                        op: Op::UnOp {
                            arg: Loc(ParLoc {
                                loc: UnsizedParLoc::Mem(0),
                                size: Size::new(0, 3),
                            }),
                            op: Id,
                        },
                    },
                    Cmd::Store {
                        to: Temp(0),
                        op: Op::Ite {
                            cond: Loc(FLAG_DF),
                            if_nonzero: Loc(ParLoc {
                                loc: UnsizedParLoc::Const(0xFFFFFFFFFFFFFFFC),
                                size: Size::new(0, 7),
                            }),
                            if_zero: Loc(ParLoc {
                                loc: UnsizedParLoc::Const(4),
                                size: Size::new(0, 3),
                            }),
                        },
                    },
                    Cmd::Store {
                        to: Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Di)),
                            size: Size::new(0, 3),
                        }),
                        op: Op::BinOp {
                            args: [
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Di)),
                                    size: Size::new(0, 3),
                                }),
                                Temp(0),
                            ],
                            op: Add,
                        },
                    },
                    Cmd::Store {
                        to: Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Si)),
                            size: Size::new(0, 3),
                        }),
                        op: Op::BinOp {
                            args: [
                                Loc(ParLoc {
                                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Si)),
                                    size: Size::new(0, 3),
                                }),
                                Temp(0),
                            ],
                            op: Add,
                        },
                    },
                ]),
            }]),
            part_packing: PackingStructure::from_part_sizes([]),
            jump: Jump::Sequential,
            is_rep: false,
        },
        metadata: None,
    }
}

pub fn push_reg() -> Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata> {
    Encoding {
        bits: vec![
            Bit::Part(0),
            Bit::Part(0),
            Bit::Part(0),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(0),
            Bit::Fixed(1),
            Bit::Fixed(0),
        ]
        .into_iter()
        .map_into()
        .collect(),
        equivalent_prefixes: EquivalentPrefixes::from_edges(
            0,
            [
                SubstitutionSequence::EquivalentTo(PrefixSequence::empty()),
                SubstitutionSequence::NotEquivalent,
            ],
            [
                (0, 0x2E, 0),
                (0, 0x3E, 0),
                (0, 0x26, 0),
                (0, 0x36, 0),
                (0, 0x64, 0),
                (0, 0x65, 0),
                (0, 0x66, 1),
                (0, 0x67, 1),
                (0, 0xF0, 0),
                (0, 0xF2, 0),
                (0, 0xF3, 0),
                (1, 0x2E, 1),
                (1, 0x3E, 1),
                (1, 0x26, 1),
                (1, 0x36, 1),
                (1, 0x64, 1),
                (1, 0x65, 1),
                (1, 0x66, 1),
                (1, 0x67, 1),
                (1, 0xF0, 1),
                (1, 0xF2, 1),
                (1, 0xF3, 1),
            ],
        ),
        parts: vec![Part {
            size: 3,
            value: 0,
            mapping: PartMapping::Register {
                mapping: vec![
                    Some(Reg::Gp(GpReg::Ax)),
                    Some(Reg::Gp(GpReg::Cx)),
                    Some(Reg::Gp(GpReg::Dx)),
                    Some(Reg::Gp(GpReg::Bx)),
                    Some(Reg::Gp(GpReg::Sp)),
                    Some(Reg::Gp(GpReg::Bp)),
                    Some(Reg::Gp(GpReg::Si)),
                    Some(Reg::Gp(GpReg::Di)),
                ],
            },
        }],
        semantics: MiniSem {
            name: String::from("PUSH_reg"),
            addresses: MemoryAccesses {
                memory: vec![MemoryAccess {
                    kind: InputOutput,
                    inputs: Inputs::unsorted(vec![
                        ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::SsBase)),
                            size: Size::new(0, 3),
                        },
                        ParLoc {
                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Sp)),
                            size: Size::new(0, 3),
                        },
                    ]),
                    size: MemorySizeRange::new(4, 4),
                    calculation: ParameterizedComputation::Calculation(
                        AddressComputation::from_iter(
                            [AddrTerm::identity(AddrSize::Addr32), AddrTerm::identity(AddrSize::Addr32)].into_iter(),
                            -4,
                        )
                        .with_addr_size(AddrSize::Addr32),
                    ),
                    alignment: 1,
                }],
                use_trap_flag: true,
            },
            commands: Ops(vec![
                Cmd::Store {
                    to: Loc(ParLoc {
                        loc: UnsizedParLoc::Mem(0),
                        size: Size::new(0, 3),
                    }),
                    op: Op::UnOp {
                        arg: Loc(ParLoc {
                            loc: UnsizedParLoc::Part(0),
                            size: Size::new(0, 3),
                        }),
                        op: Id,
                    },
                },
                Cmd::Store {
                    to: Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Sp)),
                        size: Size::new(0, 3),
                    }),
                    op: Op::BinOp {
                        args: [
                            Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Sp)),
                                size: Size::new(0, 3),
                            }),
                            Loc(ParLoc {
                                loc: UnsizedParLoc::Const(4),
                                size: Size::new(0, 3),
                            }),
                        ],
                        op: Sub,
                    },
                },
                Cmd::Store {
                    to: Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                        size: Size::new(0, 3),
                    }),
                    op: Op::BinOp {
                        args: [
                            Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ip)),
                                size: Size::new(0, 3),
                            }),
                            Loc(ParLoc {
                                loc: UnsizedParLoc::InstrLen,
                                size: Size::new(0, 3),
                            }),
                        ],
                        op: Add,
                    },
                },
            ]),
            part_packing: PackingStructure::from_part_sizes([3]),
            jump: Jump::Sequential,
            is_rep: false,
        },
        metadata: None,
    }
}
