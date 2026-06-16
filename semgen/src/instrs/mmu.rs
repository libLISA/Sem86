use liblisa::encoding::bitpattern::PartMapping;
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::{GpReg, HANDLER_INVALIDATE_PAGE, HANDLER_SS_UPDATED, Intel386, Reg};
use sem86_core::il::{BinOp, Cmd, Commands, Op, UnOp, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Context, Mode};
use crate::dsl::*;
use crate::instrs::{DWORD, FLAG_ZF, LOW_BYTE, QWORD, WORD};
use crate::{Config, encoding, encoding_group, ops};

pub trait SegmentIdentifier {
    fn into_gpreg(self) -> GpReg;
}

impl SegmentIdentifier for Val<Intel386> {
    fn into_gpreg(self) -> GpReg {
        match self {
            Val::Loc(ParLoc {
                loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
                ..
            }) => reg,
            _ => unimplemented!(),
        }
    }
}

impl SegmentIdentifier for GpReg {
    fn into_gpreg(self) -> GpReg {
        self
    }
}

pub fn load_and_check_segment(
    sreg: impl SegmentIdentifier, source: Val<Intel386>, after_loading: Vec<Cmd<Intel386>>, mark_accessed: bool,
    ctx: &mut Context,
) -> Vec<Cmd<Intel386>> {
    let reg = sreg.into_gpreg();
    let sreg = Val::Loc(ParLoc {
        loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
        size: WORD,
    });
    let (base_reg, limit_reg, ar_reg) = match reg {
        GpReg::Cs => (GpReg::CsBase, GpReg::CsLimit, Some(GpReg::CsAr)),
        GpReg::Ds => (GpReg::DsBase, GpReg::DsLimit, Some(GpReg::DsAr)),
        GpReg::Es => (GpReg::EsBase, GpReg::EsLimit, Some(GpReg::EsAr)),
        GpReg::Ss => (GpReg::SsBase, GpReg::SsLimit, Some(GpReg::SsAr)),
        GpReg::Fs => (GpReg::FsBase, GpReg::FsLimit, Some(GpReg::FsAr)),
        GpReg::Gs => (GpReg::GsBase, GpReg::GsLimit, Some(GpReg::GsAr)),
        GpReg::Tr => (GpReg::TrBase, GpReg::TrLimit, None),
        GpReg::Ldt => (GpReg::LdtBase, GpReg::LdtLimit, None),
        _ => unreachable!(),
    };

    ops! {
        #[context(ctx)]

        let ok;
        let base;
        let limit;
        let access_rights;

        ..Cmd::ReadDescriptor {
            force: false,
            selector: source,
            ok,
            base,
            limit,
            access_rights,
            mark_accessed,
        };

        let present = select_bit(access_rights, 15);
        if is_zero(present) {
            let code = and(source, 0xfffc);
            ..(Exception::SegmentNotPresent(0), code);
        }

        // TODO: Do we need to check any access rights here?

        if is_zero(ok) {
            let code = and(source, 0xfffc);
            ..(Exception::GeneralProtectionFault(0), code);
        } else {
            (base_reg, DWORD) := base;
            sreg := source;

            // We should only update the limits and access rights if we are running in protected mode.
            let in_protected_mode = and((GpReg::Cr0, LOW_BYTE), 1);
            if is_zero(in_protected_mode) {
                // We use the upper half of the access rights to cache the effective segment start.
                // This needs to be updated even in real mode.
                #[if ar_reg.is_some()] {
                    let upper_half = shr(access_rights, 32);
                    (ar_reg.unwrap(), Size::new(4, 7)) := upper_half;
                }
            } else {
                (limit_reg, DWORD) := limit;
                #[if ar_reg.is_some()] {
                    (ar_reg.unwrap(), QWORD) := access_rights;
                }
            }

            ..after_loading;
        }
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            // TODO: Use FarPointerRm instead?
            [
                Wide { true },
                Name { "LDS" },
                Prefixes, #0xC5,
                Mod = md, RegBits = reg, FarPointerRm { md } = (offset, selector),
                FixedReg { GpReg::Ds } = sreg,
            ] = (reg, offset, selector, sreg),
            [
                Wide { true },
                Name { "LES" },
                Prefixes, #0xC4,
                Mod = md, RegBits = reg, FarPointerRm { md } = (offset, selector),
                FixedReg { GpReg::Es } = sreg,
            ] = (reg, offset, selector, sreg),
            [
                Wide { true },
                Name { "LFS" },
                Prefixes, #0x0F, #0xB4,
                Mod = md, RegBits = reg, FarPointerRm { md } = (offset, selector),
                FixedReg { GpReg::Fs } = sreg,
            ] = (reg, offset, selector, sreg),
            [
                Wide { true },
                Name { "LGS" },
                Prefixes, #0x0F, #0xB5,
                Mod = md, RegBits = reg, FarPointerRm { md } = (offset, selector),
                FixedReg { GpReg::Gs } = sreg,
            ] = (reg, offset, selector, sreg),
            map |(reg, offset, selector, sreg)| BuildFromContext::new(move |ctx| load_and_check_segment(sreg, selector, vec![
                Cmd::mov(reg, offset),
            ], true, ctx))
        },
        encoding! {
            Wide { true },
            Name { "LSS" },
            Prefixes, #0x0F, #0xB2,
            Mod = md, RegBits = reg, FarPointerRm { md } = (offset, selector),
            FixedReg { GpReg::Ss } = sreg,
            {
                BuildFromContext::new(move |ctx| load_and_check_segment(sreg, selector, vec![
                    Cmd::mov(reg, offset),
                    Cmd::Handler {
                        id: HANDLER_SS_UPDATED,
                        args: Vec::new(),
                    }
                ], true, ctx))
            }
        },
        encoding! {
            Name { "ARPL - From Register/Memory" },
            Prefixes,
            OverrideMemorySize { 2 },
            #0x63,
            ModRm = (src, dest),
            {
                BuildFromContext::new(move |ctx| match ctx.mode() {
                    // This instruction does not exist in real mode and virtual 8086 mode.
                    // Windows uses it to exit real mode (by intentionally triggering an exception)
                    Mode::RealOrProtected16 => vec![
                        Cmd::Exception {
                            exception: Exception::InvalidOpcode,
                            code: Val::const_val(0),
                        }
                    ],
                    Mode::Protected32 => vec![
                        Cmd::store(Val::Temp(0), Op::BinOp {
                            args: [
                                dest,
                                Val::const_val(3),
                            ],
                            op: BinOp::And,
                        }),
                        Cmd::store(Val::Temp(1), Op::BinOp {
                            args: [
                                src,
                                Val::const_val(3),
                            ],
                            op: BinOp::And,
                        }),
                        Cmd::store(Val::Temp(2), Op::BinOp {
                            args: [
                                src,
                                Val::const_val(!3),
                            ],
                            op: BinOp::And,
                        }),
                        Cmd::store(Val::Temp(3), Op::BinOp {
                            args: [
                                Val::Temp(2),
                                Val::Temp(0),
                            ],
                            op: BinOp::Or,
                        }),
                        Cmd::store(Val::Temp(4), Op::BinOp {
                            args: [
                                Val::Temp(0),
                                Val::Temp(1),
                            ],
                            op: BinOp::CmpLt,
                        }),
                        Cmd::mov(Val::Loc(FLAG_ZF), Val::Temp(4)),
                        Cmd::store(dest, Op::Ite {
                            cond: Val::Temp(4),
                            if_zero: dest,
                            if_nonzero: Val::Temp(3),
                        }),
                    ],
                })
            }
        },
        encoding_group! {
            [
                Wide { true },
                Name { "LGDT - Table Register" },
                OverrideMemorySize { 6 },
                Prefixes, #0x0F, #0x01,
                Mod = md, 0, 1, 0, Rm { md } = rm,
            ] = (rm, GpReg::GdtBase.into(), GpReg::GdtLimit.into()),
            [
                Wide { true },
                Name { "LIDT - Table Register" },
                OverrideMemorySize { 6 },
                Prefixes, #0x0F, #0x01,
                Mod = md, 0, 1, 1, Rm { md } = rm,
            ] = (rm, GpReg::IdtBase.into(), GpReg::IdtLimit.into()),
            map |(src, base, limit)| BuildFromContext::new(move |ctx| vec![
                Cmd::store(Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(limit),
                    size: Size::new(0, 1),
                }), Op::BinOp {
                    args: [
                        src,
                        Val::const_val(0xffff),
                    ],
                    op: BinOp::And,
                }),
                Cmd::store(Val::Temp(0), Op::BinOp {
                    args: [
                        src,
                        Val::const_val(16),
                    ],
                    op: BinOp::Shr,
                }),
                Cmd::store(Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(base),
                    size: Size::new(0, 3),
                }), Op::BinOp {
                    args: [
                        Val::Temp(0),
                        Val::const_val(match ctx.op_size() {
                            2 => 0x00ff_ffff,
                            4 => 0xffff_ffff,
                            _ => unreachable!(),
                        }),
                    ],
                    op: BinOp::And,
                }),
            ])
        },
        encoding! {
            Name { "LLDT" },
            OverrideMemorySize { 2 },
            Prefixes, #0x0F, #0x00,
            Mod = md, 0, 1, 0, Rm { md } = rm,
            {
                BuildFromContext::new(move |ctx| load_and_check_segment(GpReg::Ldt, rm, vec![
                ], false, ctx))
            }
        },
        encoding_group! {
            [
                Name { "SGDT" },
                Prefixes,
                OverrideMemorySize { 6 },
                #0x0F, #0x01,
                Mod = md, 0, 0, 0, Rm { md } = rm,
            ] = (rm, GpReg::GdtBase.into(), GpReg::GdtLimit.into()),
            [
                Name { "SIDT" },
                Prefixes,
                OverrideMemorySize { 6 },
                #0x0F, #0x01,
                Mod = md, 0, 0, 1, Rm { md } = rm,
            ] = (rm, GpReg::IdtBase.into(), GpReg::IdtLimit.into()),
            map |(dst, base, limit)| BuildFromContext::new(move |_| vec![
                Cmd::store(Val::Temp(0), Op::BinOp {
                    args: [
                        Val::Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(base),
                            size: Size::new(0, 3),
                        }),
                        Val::const_val(16),
                    ],
                    op: BinOp::Shl,
                }),
                Cmd::store(Val::Temp(0), Op::BinOp {
                    args: [
                        Val::Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(limit),
                            size: Size::new(0, 1),
                        }),
                        Val::Temp(0),
                    ],
                    op: BinOp::Or,
                }),
                Cmd::mov(dst, Val::Temp(0)),
            ])
        },
        encoding! {
            Wide { true },
            Name { "SLDT" },
            Prefixes,
            OverrideMemorySize { 2 },
            #0x0F, #0x00,
            Mod = md, 0, 0, 0, Rm { md } = rm,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::mov(rm, Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(GpReg::Ldt.into()),
                        size: WORD,
                    })),
                ])
            }
        },
        encoding! {
            Wide { true },
            OverrideMemorySize { 2 },
            Name { "LTR" },
            Prefixes, #0x0F, #0x00,
            Mod = md, 0, 1, 1, Rm { md } = rm,
            {
                // TODO: We should mark this segment as accessed, right?
                BuildFromContext::new(move |ctx| load_and_check_segment(GpReg::Tr, rm, vec![
                ], true, ctx))
            }
        },
        encoding! {
            Wide { true },
            OverrideMemorySize { 2 },
            Name { "STR" },
            Prefixes, #0x0F, #0x00,
            Mod = md, 0, 0, 1, Rm { md } = rm,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::mov(
                        rm,
                        Val::from((GpReg::Tr, WORD)),
                    ),
                ])
            }
        },
        encoding! {
            Wide { true },
            Name { "LAR - From Register/Memory" },
            Prefixes,
            OverrideMemorySize { 2 },
            #0x0F, #0x02,
            ModRm = (reg, rm),
            {
                // NOTE: load_and_check_segment considers NULL selector OK.
                BuildFromContext::new(move |_| vec![
                    // Check for NULL selector
                    Cmd::store(Val::Temp(5), Op::BinOp {
                        args: [
                            rm,
                            Val::const_val(!3),
                        ],
                        op: BinOp::And,
                    }),
                    Cmd::If {
                        val: Val::Temp(5),
                        if_zero: Commands::Ops(vec![
                            Cmd::mov(Val::Loc(FLAG_ZF), Val::const_val(0)),
                        ]),
                        if_nonzero: Commands::Ops(vec![
                            Cmd::ReadDescriptor {
                                force: true,
                                selector: rm,
                                ok: Val::Loc(FLAG_ZF),
                                base: Val::Temp(2),
                                limit: Val::Temp(3),
                                access_rights: reg,
                                mark_accessed: false,
                            },
                        ])
                    },
                ])
            }
        },
        encoding! {
            Wide { true },
            Name { "LSL - From Register/Memory" },
            Prefixes,
            OverrideMemorySize { 2 },
            #0x0F, #0x03,
            ModRm = (reg, rm),
            {
                BuildFromContext::new(move |_| vec![
                    // TODO: Set ZF=0 is descriptor type is unsupported

                    // Check for NULL selector
                    Cmd::store(Val::Temp(5), Op::BinOp {
                        args: [
                            rm,
                            Val::const_val(!3),
                        ],
                        op: BinOp::And,
                    }),
                    Cmd::If {
                        val: Val::Temp(5),
                        if_zero: Commands::Ops(vec![
                            Cmd::mov(Val::Loc(FLAG_ZF), Val::const_val(0)),
                        ]),
                        if_nonzero: Commands::Ops(vec![
                            Cmd::ReadDescriptor {
                                force: true,
                                selector: rm,
                                ok: Val::Loc(FLAG_ZF),
                                base: Val::Temp(2),
                                limit: reg,
                                access_rights: Val::Temp(4),
                                mark_accessed: false,
                            },
                        ])
                    },
                ])
            }
        },
        encoding_group! {
            [
                Wide { true },
                Name { "VERR_rm" },
                Prefixes, #0x0F, #0x00,
                Mod = md, 1, 0, 0, Rm { md } = rm,
            ] = (rm, false),
            [
                Wide { true },
                Name { "VERW" },
                Prefixes, #0x0F, #0x00,
                Mod = md, 1, 0, 1, Rm { md } = rm,
            ] = (rm, true),
            map |(selector, check_write)| BuildFromContext::new(move |_| {
                let mut ops = vec![
                    Cmd::ReadDescriptor {
                        force: true,
                        selector,
                        ok: Val::Loc(FLAG_ZF),
                        base: Val::Temp(2),
                        limit: Val::Temp(3),
                        access_rights: Val::Temp(0),
                        mark_accessed: false,
                    },
                ];

                if check_write {
                    ops.extend([
                        Cmd::store(Val::Temp(1), Op::UnOp {
                            arg: Val::Temp(0),
                            op: UnOp::SelectBit(9),
                        }),
                        Cmd::store(Val::Loc(FLAG_ZF), Op::BinOp {
                            args: [
                                Val::Loc(FLAG_ZF),
                                Val::Temp(1),
                            ],
                            op: BinOp::And,
                        })
                    ]);
                }

                vec![
                    // TODO: Set ZF=0 is descriptor type is unsupported
                    // Check for NULL selector
                    Cmd::store(Val::Temp(5), Op::BinOp {
                        args: [
                            rm,
                            Val::const_val(!3),
                        ],
                        op: BinOp::And,
                    }),
                    Cmd::If {
                        val: Val::Temp(5),
                        if_zero: Commands::Ops(vec![
                            Cmd::mov(Val::Loc(FLAG_ZF), Val::const_val(0)),
                        ]),
                        if_nonzero: Commands::Ops(ops),
                    },
                ]
            })
        },
        encoding! {
            Wide { true },
            Name { "INVLPG" },
            Prefixes, #0x0F, #0x01,
            Mod = md, 1, 1, 1, Rm { md } = _rm,
            Filter::<Box<dyn Fn(&Context) -> bool>> { Box::new(move |_| md != ModVal::Mod11) },
            {
                BuildFromContext::new(move |ctx| {
                    let mut ops = Vec::new();
                    let addr = ctx.pop_access();
                    ops.push(Cmd::mov(Val::Temp(0), Val::const_val(0)));
                    for (input, term) in addr.inputs.iter().zip(addr.calculation.unwrap_calculation().terms.iter()) {
                        ops.extend([
                            Cmd::store(Val::Temp(1), Op::BinOp {
                                args: [
                                    if let UnsizedParLoc::Part(part) = input.loc {
                                        let part = &ctx.parts()[part];
                                        if matches!(part.mapping, PartMapping::Imm { .. }) {
                                            Val::Conv {
                                                loc: *input,
                                                source_bits: part.size.try_into().unwrap(),
                                                target_bits: (ctx.addr_size() * 8).try_into().unwrap(),
                                                sign_extend: true,
                                                swap_endianness: true,
                                            }
                                        } else {
                                            Val::Loc(*input)
                                        }
                                    } else {
                                        Val::Loc(*input)
                                    },
                                    Val::const_val(term.primary.shift.mult() as u64),
                                ],
                                op: BinOp::Mul,
                            }),
                            Cmd::store(Val::Temp(0), Op::BinOp {
                                args: [
                                    Val::Temp(0),
                                    Val::Temp(1),
                                ],
                                op: BinOp::Add,
                            }),
                        ])
                    }

                    ops.extend([
                        Cmd::Handler {
                            id: HANDLER_INVALIDATE_PAGE,
                            args: vec![
                                Val::Temp(0),
                            ]
                        }
                    ]);

                    ops
                })
            }
        },
    ]
}
