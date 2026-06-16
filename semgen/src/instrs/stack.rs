use liblisa::encoding::dataflows::{AccessKind, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_core::arch::intel386::{GpReg, Intel386, Reg};
use sem86_core::il::{BinOp, Cmd, Op, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Mode};
use crate::instrs::mmu::load_and_check_segment;
use crate::instrs::{DWORD, WORD};
use crate::{Config, encoding, encoding_group, ops};

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Wide { true },
                Name { "PUSH_rm" },
                Prefixes, #0xFF,
                Mod = md, 1, 1, 0, Rm { md } = rm,
            ] = (rm, false),
            [
                Wide { true },
                Name { "PUSH_reg" },
                Prefixes,
                0, 1, 0, 1, 0, RegBits = reg,
            ] = (reg, false),
            [
                Wide { true },
                Name { "PUSH - Segment Register (ES, CS, SS or DS)" },
                Prefixes,
                0, 0, 0, Sreg2 { true } = sreg, 1, 1, 0,
            ] = (sreg, true),
            [
                Wide { true },
                Name { "PUSH - Segment Register (FS or GS)" },
                Prefixes, #0x0F,
                1, 0, Sreg3 { false } = sreg, 0, 0, 0,
            ] = (sreg, true),
            [
                Wide { true },
                Name { "PUSH_imm" },
                Prefixes,
                0, 1, 1, 0, 1, 0, S, 0,
                FullImm = imm,
            ] = (imm, false),
            map |(val, is_seg)| BuildFromContext::new(move |ctx| {
                let sp_size = ctx.sp_size();
                let sp_increment = ctx.size().num_bytes() as u64;
                let mem_size = if is_seg {
                    WORD
                } else {
                    ctx.size()
                };
                let mem = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(mem_size.num_bytes() as u64, mem_size.num_bytes() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(-(sp_increment as i64))),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                    ]),
                });

                ops! {
                    #[context(ctx)]

                    // The way in which we do these operations is used to detect the CPU version.
                    // The 8086 will decrement-copy.
                    // The 286+ will copy-decrement.
                    mem := val;
                    (GpReg::Sp, sp_size) -= sp_increment;

                }
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "POP_rm" },
                Prefixes,
                AjustForPop,
                #0x8F,
                Mod = md, 0, 0, 0, Rm { md } = rm,
            ] = rm,
            [
                Wide { true },
                Name { "POP_reg" },
                Prefixes,
                AjustForPop,
                0, 1, 0, 1, 1, RegBits = reg,
            ] = reg,
            map |val| BuildFromContext::new(move |ctx| {
                let sp_size = ctx.sp_size();

                let size = ctx.size();
                let mem = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation()),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                    ]),
                });

                ops! {
                    #[context(ctx)]

                    let popped_val = mem;
                    (GpReg::Sp, sp_size) += size.num_bytes();
                    val := popped_val;

                }
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "POP - Segment Register (ES, SS or DS)" },
                Prefixes,
                AjustForPop,
                0, 0, 0, ExpandedSreg2 { false } = (sreg, index), 1, 1, 1,
            ] = (sreg, index),
            [
                Wide { true },
                Name { "POP - Segment Register (FS or GS)" },
                Prefixes,
                AjustForPop,
                #0x0F,
                1, 0, ExpandedSreg3 { false } = (sreg, index), 0, 0, 1,
            ] = (sreg, index),
            map |(sreg, _)| BuildFromContext::new(move |ctx| {
                let sp_size = ctx.sp_size();
                let size = ctx.size();
                let mem = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(2, 2),
                    calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation()),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                    ]),
                });

                load_and_check_segment(sreg, Val::Loc(mem), ops! {
                    #[context(ctx)]

                    (GpReg::Sp, sp_size) += size.num_bytes();

                }, true, ctx)
            })
        },
        encoding! {
            Wide { true },
            Name { "PUSHA" },
            Prefixes, #0x60,
            {
                BuildFromContext::new(move |ctx| {
                    let sp_size = ctx.sp_size();

                    let mut ops = Vec::new();
                    let size = ctx.op_size() as u64;

                    ops.push(Cmd::mov(Val::Temp(0), Val::from((GpReg::Sp, sp_size))));

                    for (n, &reg) in [ GpReg::Ax, GpReg::Cx, GpReg::Dx, GpReg::Bx, GpReg::Sp, GpReg::Bp, GpReg::Si, GpReg::Di ].iter().rev().enumerate() {
                        ops.extend([
                            Cmd::mov(Val::Loc(ctx.add_access(MemoryAccess {
                                kind: AccessKind::InputOutput,
                                size: MemorySizeRange::new(size, size),
                                calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().clone().with_offset(size as i64 * -8 + size as i64 * n as i64)),
                                alignment: 1,
                                inputs: Inputs::unsorted(vec![
                                    ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                                    ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: sp_size },
                                ]),
                            })), Val::Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
                                size: Size::from_bytes(size as usize),
                            })),
                        ])
                    }

                    ops.extend([
                        Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                            args: [
                                Val::from((GpReg::Sp, sp_size)),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(size * 8), size: sp_size }),
                            ],
                            op: BinOp::Sub,
                        }),
                    ]);

                    ops
                })
            }
        },
        encoding! {
            Wide { true },
            Name { "POPA" },
            Prefixes, #0x61,
            {
                BuildFromContext::new(move |ctx| {
                    let sp_size = Size::from_bytes(match ctx.mode() {
                        Mode::RealOrProtected16 => 2,
                        Mode::Protected32 => 4,
                    });

                    let mut ops = Vec::new();
                    let size = ctx.op_size() as u64;

                    for (n, &reg) in [ GpReg::Ax, GpReg::Cx, GpReg::Dx, GpReg::Bx, GpReg::Sp, GpReg::Bp, GpReg::Si, GpReg::Di ].iter().rev().enumerate() {
                        if reg == GpReg::Sp {
                            continue
                        }

                        ops.extend([
                            Cmd::mov(Val::Loc(ParLoc {
                                loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
                                size: Size::from_bytes(size as usize),
                            }), Val::Loc(ctx.add_access(MemoryAccess {
                                kind: AccessKind::InputOutput,
                                size: MemorySizeRange::new(size, size),
                                calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().clone().with_offset(size as i64 * n as i64)),
                                alignment: 1,
                                inputs: Inputs::unsorted(vec![
                                    ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                                    ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                                ]),
                            }))),
                        ])
                    }

                    ops.extend([
                        Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                            args: [
                                Val::from((GpReg::Sp, sp_size)),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(size * 8), size: sp_size }),
                            ],
                            op: BinOp::Add,
                        }),
                    ]);

                    ops
                })
            }
        },
    ]
}
