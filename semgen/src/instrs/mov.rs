use liblisa::encoding::bitpattern::PartValue;
use liblisa::encoding::dataflows::{
    AccessKind, AddrTerm, AddrTermSize, AddressComputation, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation,
};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_core::arch::intel386::{GpReg, HANDLER_CS_UPDATED, HANDLER_SS_UPDATED, HANDLER_WRITE_CR, Intel386, Reg};
use sem86_core::il::{BinOp, Cmd, Jump, Op, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Context, Mode};
use crate::dsl::*;
use crate::instrs::DWORD;
use crate::instrs::flow::Condition;
use crate::instrs::mmu::load_and_check_segment;
use crate::{Config, encoding, encoding_group, ops};

fn create_move(source: Val<Intel386>, target: Val<Intel386>) -> Vec<Cmd<Intel386>> {
    vec![Cmd::mov(target, source)]
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Name { "MOV_from_reg" },
                Prefixes,
                1, 0, 0, 0, 1, 0, ExpandedBit = d, W,
                ModRm = (reg, rm),
                SrcDst { d, reg, rm } = (src, dst),
            ] = (src, dst),
            [
                Name { "MOV_imm_rm" },
                Prefixes,
                1, 1, 0, 0, 0, 1, 1, W,
                Mod = md, 0, 0, 0, Rm { md } = rm,
                FullImm = imm,
            ] = (imm, rm),
            [
                Name { "MOV - Immediate to Register (short form)" },
                Prefixes,
                1, 0, 1, 1, W, RegBits = reg,
                FullImm = imm,
            ] = (imm, reg),
            map |(src, dst)| BuildFromContext::new(move |_| create_move(src, dst))
        },
        encoding! {
            Name { "MOVSX/ZX" },
            AllowHighRegByte { false },
            OverrideRmSizingMode { Mode::RealOrProtected16 },
            Prefixes, #0x0F,
            1, 0, 1, 1, S = sign_extend, 1, 1, W,
            ModRm = (reg, src),
            {
                BuildFromContext::new(move |ctx| {
                    let dst_size = Size::from_bytes(ctx.op_size_ext(ctx.mode(), true, ctx.has_wide_operand_size_override()));
                    let dst = match reg {
                        Val::Loc(dst) => Val::Loc(ParLoc {
                            loc: dst.loc,
                            size: dst_size,
                        }),
                        _ => unreachable!(),
                    };

                    let src = if sign_extend {
                        let src_bits = match src {
                            Val::Loc(src) => src.size.num_bytes() * 8,
                            _ => todo!(),
                        };

                        src.sign_extend(src_bits.try_into().unwrap(), (dst_size.num_bytes() * 8).try_into().unwrap())
                    } else {
                        src
                    };

                    create_move(src, dst)
                })
            }
        },
        encoding_group! {
            [
                Name { "MOV_mem_to_eax" },
                Prefixes,
                1, 0, 1, 0, 0, 0, 0, W,
                FullDisp = disp,
            ] = (disp, false),
            [
                Name { "MOV_eax_to_mem" },
                Prefixes,
                1, 0, 1, 0, 0, 0, 1, W,
                FullDisp = disp,
            ] = (disp, true),
            map |(disp, to_mem)| BuildFromContext::new(move |ctx| {
                let inputs = Inputs::unsorted(vec![
                    ctx.segment_override().unwrap_or(ParLoc { loc: UnsizedParLoc::Reg(GpReg::DsBase.into()), size: DWORD }),
                    disp,
                ]);
                let mem = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(ctx.op_size() as u64, ctx.op_size() as u64),
                    calculation: ParameterizedComputation::Calculation(AddressComputation::from_iter([
                        AddrTerm::single(AddrTermSize::U32, 0, 1),
                        AddrTerm::single(ctx.memory_reg_and_addr_size().0, 0, 1),
                    ].into_iter(), 0).with_addr_size(ctx.memory_reg_and_addr_size().1)),
                    alignment: 1,
                    inputs,
                });

                ops! {
                    #[context(ctx)]
                    #[if to_mem] {
                        mem := (GpReg::Ax, ctx.size());
                    } else {
                        (GpReg::Ax, ctx.size()) := mem;
                    }

                }
            })
        },
        encoding! {
            Wide { true },
            Name { "MOV_to_sreg" },
            OverrideMemorySize { 2 },
            Prefixes, #0x8E,
            Mod = md, ExpandedSreg3 { true } = (sreg, _index), Rm { md } = rm,
            {
                BuildFromContext::new(move |ctx| {
                    let ops = ops! {
                        #[context(ctx)]
                        #[if sreg == GpReg::Ss] {
                            ..HANDLER_SS_UPDATED;
                        }
                        #[if sreg == GpReg::Cs] {
                            ..HANDLER_CS_UPDATED;
                        }
                    };
                    SemSpec {
                        commands: load_and_check_segment(sreg, rm, ops, true, ctx),
                        jump: if sreg == GpReg::Cs {
                            Jump::Far
                        } else {
                            Jump::Sequential
                        },
                        ..Default::default()
                    }
                })
            }
        },
        encoding! {
            Wide { true },
            Name { "MOV_from_sreg" },
            OverrideMemorySize { 2 },
            Prefixes, #0x8C,
            Mod = md, Sreg3 { true } = sreg, Rm { md } = rm,
            ops! {
                #[context(ctx)]
                rm := sreg;

            }
        },
        encoding! {
            Wide { true },
            Name { "CBW / CWDE" },
            Prefixes, #0x98,
            ops! {
                #[context(ctx)]
                (GpReg::Ax, Size::from_bytes(ctx.op_size())) := Val::Conv {
                    loc: ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: Size::from_bytes(ctx.op_size() / 2) },
                    source_bits: ((ctx.op_size() / 2) * 8).try_into().unwrap(),
                    target_bits: 32,
                    sign_extend: true,
                    swap_endianness: false,
                };

            }
        },
        encoding! {
            Wide { true },
            Name { "CWD / CQD" },
            Prefixes, #0x99,
            {
                BuildFromContext::new(move |ctx| vec![
                    Cmd::mov(
                        Val::Temp(0),
                        Val::Conv {
                            loc: ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: ctx.size() },
                            source_bits: (ctx.size().num_bytes() * 8).try_into().unwrap(),
                            target_bits: 64,
                            sign_extend: true,
                            swap_endianness: false,
                        }
                    ),
                    Cmd::store(Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Dx.into()), size: ctx.size() }), Op::BinOp {
                        args: [ Val::Temp(0), Val::const_val(ctx.size().num_bytes() as u64 * 8) ],
                        op: BinOp::Shr,
                    }),
                ])
            }
        },
        encoding! {
            MaximumOperandSize,
            Name { "MOV_to_cr" },
            Prefixes, #0x0F, #0x22,
            1, 1, ImmWithMapping { 3, vec![ PartValue::Valid, PartValue::Invalid, PartValue::Valid, PartValue::Valid, PartValue::Valid, PartValue::Valid, PartValue::Valid, PartValue::Valid ] } = eee, RegBits = reg,
            {
                BuildFromContext::new(move |_ctx| vec![
                    Cmd::Handler {
                        id: HANDLER_WRITE_CR,
                        args: vec![
                            eee,
                            reg,
                        ]
                    }
                ])
            }
        },
        encoding! {
            MaximumOperandSize,
            Name { "MOV_from_cr" },
            Prefixes, #0x0F, #0x20,
            1, 1, ExpandedBits { 3 } = eee, RegBits = reg,
            Filter::<Box<dyn Fn(&Context) -> bool>> { Box::new(move |_| eee != 1) },
            {
                BuildFromContext::new(move |ctx| vec![
                    Cmd::mov(reg, Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(Reg::Gp(match eee {
                        0 => GpReg::Cr0,
                        2 => GpReg::Cr2,
                        3 => GpReg::Cr3,
                        4 => GpReg::Cr4,
                        _ => GpReg::Riz,
                    })), size: ctx.size() })),
                ])
            }
        },
        encoding! {
            Wide { true },  // TODO: Always 2 bytes
            Name { "LMSW - From Register/Memory" },
            Prefixes, #0x0F, #0x01,
            Mod = md, 1, 1, 0, Rm { md } = rm,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::Handler {
                        id: HANDLER_WRITE_CR,
                        args: vec![
                            Val::const_val(0),
                            rm,
                        ]
                    }
                ])
            }
        },
        encoding! {
            Wide { true }, // TODO: Always 2 bytes
            Name { "SMSW - Status Word" },
            Prefixes, #0x0F, #0x01,
            Mod = md, 1, 0, 0, Rm { md } = rm,
            {
                BuildFromContext::new(move |ctx| vec![
                    Cmd::mov(rm, Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cr0.into()), size: ctx.size() })),
                ])
            }
        },
        encoding! {
            Wide { true },
            Name { "MOV - DRn From Register" },
            Prefixes, #0x0F, #0x23,
            1, 1, ExpandedBits { 3 } = eee, RegBits = reg,
            {
                BuildFromContext::new(move |ctx| vec![
                    Cmd::mov(Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Reg(Reg::Gp(match eee {
                            0 => GpReg::Dr0,
                            1 => GpReg::Dr1,
                            2 => GpReg::Dr2,
                            3 => GpReg::Dr3,
                            // TODO: Check CR4.DE. If DE=1, accesses to DR4 and DR5 should cause InvalidOpcode exceptions.
                            4 | 6 => GpReg::Dr6,
                            5 | 7 => GpReg::Dr7,
                            _ => unreachable!(),
                        })),
                        size: ctx.size()
                    }), reg),
                ])
            }
        },
        encoding! {
            Wide { true },
            Name { "MOV - Register from DRn" },
            Prefixes, #0x0F, #0x21,
            1, 1, ExpandedBits { 3 } = eee, RegBits = reg,
            {
                BuildFromContext::new(move |ctx| vec![
                    Cmd::store(reg, Op::BinOp {
                        op: BinOp::Or,
                        args: [
                            Val::Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(match eee {
                                    0 => GpReg::Dr0,
                                    1 => GpReg::Dr1,
                                    2 => GpReg::Dr2,
                                    3 => GpReg::Dr3,
                                    // TODO: Check CR4.DE. If DE=1, accesses to DR4 and DR5 should cause InvalidOpcode exceptions.
                                    4 | 6 => GpReg::Dr6,
                                    5 | 7 => GpReg::Dr7,
                                    _ => unreachable!(),
                                })),
                                size: ctx.size()
                            }),
                            Val::const_val(match eee {
                                4 | 6 => 0xffff0ff0,
                                5 | 7 => 0x00000400,
                                _ => 0,
                            })
                        ]
                    }),
                ])
            }
        },
        // Test registers -- we probably never need them.
        // encoding! {
        //     Wide { true },
        //     Name { "MOV - TR from Register" },
        //     Prefixes, #0x0F, #0x26,
        //     1, 1, ExpandedBits { 3 } = _eee, RegBits = _reg,
        //     {
        //         BuildFromContext::new(move |_| Vec::new())
        //     }
        // },
        // encoding! {
        //     Wide { true },
        //     Name { "MOV - Register from TR" },
        //     Prefixes, #0x0F, #0x24,
        //     1, 1, ExpandedBits { 3 } = _eee, RegBits = _reg,
        //     {
        //         BuildFromContext::new(move |_| Vec::new())
        //     }
        // },
        encoding_group! {
            [
                Name { "XCHG - Register/Memory With Register" },
                Lockable,
                Prefixes,
                1, 0, 0, 0, 0, 1, 1, W,
                ModRm = (reg, rm),
            ] = (reg, rm),
            [
                Wide { true },
                Name { "XCHG - Register With Accumulator (short form)" },
                Prefixes,
                1, 0, 0, 1, 0, RegBits = reg,
                Acc = acc,
            ] = (reg, acc),
            map |(a, b)| BuildFromContext::new(move |ctx| ops! {
                #[context(ctx)]

                let tmp = a;
                a := b;
                b := tmp;

            })
        },
        encoding! {
            Name { "CMOVcc" },
            Wide { true },
            Prefixes,
            #0x0F,
            0, 1, 0, 0,
            BitsInto::<Condition> { 3 } = condition, ExpandedBit = negate,
            ModRm = (reg, rm),
            {
                BuildFromContext::new(move |ctx| ops! {
                    #[context(ctx)]

                    let cond = condition;
                    let cond = xor(cond, negate as u64);
                    reg := ite(cond, reg, rm);


                })
            }
        },
    ]
}
