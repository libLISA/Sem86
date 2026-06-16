use liblisa::encoding::bitpattern::ImmBitOrder;
use liblisa::encoding::dataflows::{
    AccessKind, AddrTerm, AddrTermSize, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation,
};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use liblisa::utils::bitmask_u64;
use sem86_core::arch::intel386::{GpReg, HANDLER_CS_UPDATED, Intel386};
use sem86_core::il::{BinOp, Cmd, Jump, Op, Val};

use super::{FLAG_CF, FLAG_OF, FLAG_PF, FLAG_SF};
use crate::builder::*;
use crate::context::{BuildFromContext, Context, Mode};
use crate::dsl::*;
use crate::instrs::mmu::load_and_check_segment;
use crate::instrs::{DWORD, FLAG_ZF, WORD, invoke_gp};
use crate::{Config, encoding, encoding_group, ops};

fn jump_with_condition(
    ctx: &mut Context, prelude: Vec<Cmd<Intel386>>, jump_offset: Val<Intel386>, cond: Val<Intel386>, negate: bool,
) -> SemSpec<Intel386> {
    let cond_val = ctx.fresh_temp_var();
    let prelude = ops! {
        #[context(ctx)]
        ..prelude;

        #[if negate] {
            cond_val := is_zero(cond);
        } else {
            cond_val := cond;
        }
    };

    checked_jump(
        ctx,
        prelude,
        JumpTarget::RelativeConditional {
            offset: jump_offset,
            condition: cond_val,
        },
        |_, _, _| Vec::new(),
    )
}

/// Stores return address in temp0, computed next value of IP in temp1.
fn checked_jump(
    ctx: &mut Context, prelude: Vec<Cmd<Intel386>>, target: JumpTarget,
    ops: impl FnOnce(&mut Context, Val<Intel386>, Val<Intel386>) -> Vec<Cmd<Intel386>>,
) -> SemSpec<Intel386> {
    let size = match ctx.mode() {
        Mode::RealOrProtected16 => 2,
        Mode::Protected32 => 4,
    };

    let condition = match target {
        JumpTarget::RelativeConditional {
            condition, ..
        } => condition,
        JumpTarget::Relative(_) => Val::const_val(1),
        JumpTarget::Absolute(_) => Val::const_val(1),
    };

    SemSpec {
        commands: ops! {
            #[context(ctx)]
            ..prelude;

            if !is_zero(condition) {
                let ip_after = add((GpReg::Ip, DWORD), (UnsizedParLoc::InstrLen, Size::from_bytes(size)));
                let jump_target;
                ..(target.store_in(jump_target, ip_after));

                let masked_target = and(jump_target, bitmask_u64(size as u32 * 8));
                let outside_limits = cmp_gt(masked_target, (GpReg::CsLimit, DWORD));

                if is_zero(outside_limits) {
                    ..ops(ctx, ip_after, masked_target);
                } else {
                    ..invoke_gp();
                }
            }
        },
        jump: match target {
            JumpTarget::RelativeConditional {
                offset,
                condition,
            } => Jump::NearRelativeOffset {
                condition,
                offset: vec![offset],
            },
            JumpTarget::Relative(val) => Jump::NearRelativeOffset {
                condition: Val::const_val(1),
                offset: vec![val],
            },
            JumpTarget::Absolute(val) => Jump::NearAbsolute(val),
        },
        ..Default::default()
    }
}

#[derive(Copy, Clone, Debug)]
enum JumpTarget {
    RelativeConditional {
        offset: Val<Intel386>,
        condition: Val<Intel386>,
    },
    Relative(Val<Intel386>),
    Absolute(Val<Intel386>),
}

impl JumpTarget {
    fn store_in(self, v: Val<Intel386>, base: Val<Intel386>) -> Cmd<Intel386> {
        match self {
            JumpTarget::Relative(rel)
            | JumpTarget::RelativeConditional {
                offset: rel, ..
            } => Cmd::store(
                v,
                Op::BinOp {
                    args: [base, rel],
                    op: BinOp::Add,
                },
            ),
            JumpTarget::Absolute(abs) => Cmd::mov(v, abs),
        }
    }
}

impl From<u64> for Condition {
    fn from(value: u64) -> Self {
        match value {
            0 => Self::Overflow,
            1 => Self::Below,
            2 => Self::Equal,
            3 => Self::BelowOrEqual,
            4 => Self::Sign,
            5 => Self::Parity,
            6 => Self::Less,
            7 => Self::LessOrEqual,
            _ => unreachable!(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// JO / JNO
    Overflow,

    /// JB / JNB
    Below,

    /// JE / JNE
    Equal,

    /// JBE / JNBE
    BelowOrEqual,

    /// JS / JNSe
    Sign,

    /// JP / JNP
    Parity,

    /// JL / JGE
    Less,

    /// JLE / JG
    LessOrEqual,
}

impl Condition {
    pub fn get_condition(&self, ops: &mut Vec<Cmd<Intel386>>, scratch: Val<Intel386>) -> Val<Intel386> {
        match self {
            Condition::Overflow => Val::Loc(FLAG_OF),
            Condition::Below => Val::Loc(FLAG_CF),
            Condition::Equal => Val::Loc(FLAG_ZF),
            Condition::BelowOrEqual => {
                ops.push(Cmd::store(
                    scratch,
                    Op::BinOp {
                        args: [Val::Loc(FLAG_CF), Val::Loc(FLAG_ZF)],
                        op: BinOp::Or,
                    },
                ));

                scratch
            },
            Condition::Sign => Val::Loc(FLAG_SF),
            Condition::Parity => Val::Loc(FLAG_PF),
            Condition::Less => {
                ops.push(Cmd::store(
                    scratch,
                    Op::BinOp {
                        args: [Val::Loc(FLAG_SF), Val::Loc(FLAG_OF)],
                        op: BinOp::Xor,
                    },
                ));

                scratch
            },
            Condition::LessOrEqual => {
                ops.extend([
                    Cmd::store(
                        scratch,
                        Op::BinOp {
                            args: [Val::Loc(FLAG_SF), Val::Loc(FLAG_OF)],
                            op: BinOp::Xor,
                        },
                    ),
                    Cmd::store(
                        scratch,
                        Op::BinOp {
                            args: [Val::Loc(FLAG_ZF), scratch],
                            op: BinOp::Or,
                        },
                    ),
                ]);

                scratch
            },
        }
    }
}

impl LoadIntoVal<Intel386> for Condition {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        let scratch = ctx.fresh_temp_var();
        let condition = self.get_condition(output, scratch);
        output.extend(ops! {
            #[context(ctx)]
            target := condition;
        })
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        // Calls and jumps within the same segment
        encoding! {
            Wide { true },
            SetSignExtend { true },
            Name { "JCXZ" },
            Prefixes, #0xE3,
            Imm { 8 } = disp,
            {
                BuildFromContext::new(move |ctx| {
                    let cond = Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size: Size::from_bytes(ctx.addr_size()) });
                    jump_with_condition(ctx, Vec::new(), disp, cond, true)
                })
            }
        },
        encoding! {
            Wide { true },
            SetSignExtend { true },
            Name { "LOOP" },
            Prefixes, #0xE2,
            Imm { 8 } = disp,
            {
                BuildFromContext::new(move |ctx| {
                    let size = Size::from_bytes(ctx.addr_size());
                    let prelude = vec![
                        Cmd::store(Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size }), Op::BinOp {
                            args: [
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size }),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(1), size }),
                            ],
                            op: BinOp::Sub,
                        }),
                    ];

                    let cond = Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size });
                    jump_with_condition(ctx, prelude, disp, cond, false)
                })
            }
        },
        encoding! {
            Wide { true },
            SetSignExtend { true },
            Name { "LOOPcc" },
            Prefixes,
            1, 1, 1, 0, 0, 0, 0, ExpandedBit = negate,
            Imm { 8 } = disp,
            {
                BuildFromContext::new(move |ctx| {
                    let size = Size::from_bytes(ctx.addr_size());
                    let prelude = vec![
                        Cmd::store(Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size }), Op::BinOp {
                            args: [
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size }),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(1), size }),
                            ],
                            op: BinOp::Sub,
                        }),
                        Cmd::store(Val::Temp(6), Op::Ite {
                            cond: Val::Loc(FLAG_ZF),
                            if_nonzero: Val::const_val(negate as u64),
                            if_zero: Val::const_val(1 ^ negate as u64),
                        }),
                        Cmd::store(Val::Temp(7), Op::Ite {
                            cond: Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Cx.into()), size: Size::from_bytes(ctx.addr_size()) }),
                            if_nonzero: Val::Temp(6),
                            if_zero: Val::const_val(0),
                        }),
                    ];

                    jump_with_condition(ctx, prelude, disp, Val::Temp(7), false)
                })
            }
        },
        // SETcc
        encoding_group! {
            [
                Name { "SETcc" },
                Prefixes, #0x0F,
                1, 0, 0, 1, BitsInto::<Condition> { 3 } = condition, ExpandedBit = negate,
                Mod = md, 0, 0, 0, Rm { md } = rm,
            ] = (condition, negate, rm),
            map |(condition, negate, rm): (Condition, _, _)| BuildFromContext::new(move |_| {
                let mut ops = Vec::new();
                let cond = condition.get_condition(&mut ops, Val::Temp(2));
                let (cond_true, cond_false) = if negate {
                    (Val::const_val(0), Val::const_val(1))
                } else {
                    (Val::const_val(1), Val::const_val(0))
                };

                ops.extend([
                    Cmd::store(rm, Op::Ite {
                        cond,
                        if_nonzero: cond_true,
                        if_zero: cond_false,
                    }),
                ]);

                ops
            })
        },
        // Jcc
        encoding_group! {
            [
                Wide { true },
                SetSignExtend { true },
                Name { "Jcc_rel8" },
                Prefixes,
                0, 1, 1, 1, BitsInto::<Condition> { 3 } = condition, ExpandedBit = negate,
                Imm { 8 } = disp,
            ] = (condition, negate, disp),
            [
                Wide { true },
                Name { "Jcc_rel_full" },
                Prefixes, #0x0F,
                1, 0, 0, 0, BitsInto::<Condition> { 3 } = condition, ExpandedBit = negate,
                FullImm = disp,
            ] = (condition, negate, disp),
            map |(condition, negate, disp): (Condition, _, _)| BuildFromContext::new(move |ctx| {
                let mut prelude = Vec::new();
                let cond = condition.get_condition(&mut prelude, Val::Temp(2));
                jump_with_condition(ctx, prelude, disp, cond, negate)
            })
        },
        encoding_group! {
            [
                Wide { true },
                SetSignExtend { true },
                Name { "JMP_within_segment_rel8" },
                Prefixes, #0xEB,
                Imm { 8 } = disp,
            ] = JumpTarget::Relative(disp),
            [
                Wide { true },
                Name { "JMP_within_segment_rel_full" },
                Prefixes, #0xE9,
                FullImm = disp,
            ] = JumpTarget::Relative(disp),
            [
                Wide { true },
                Name { "JMP_within_segment_indirect" },
                Prefixes, #0xFF,
                Mod = md, 1, 0, 0, Rm { md } = rm,
            ] = JumpTarget::Absolute(rm),
            map |target: JumpTarget| BuildFromContext::new(move |ctx| {
                checked_jump(ctx, Vec::new(), target, |_, _, _| Vec::new())
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "CALL_within_segment_rel_full" },
                Prefixes, #0xE8,
                FullImm = disp,
            ] = JumpTarget::Relative(disp),
            [
                Wide { true },
                Name { "CALL_within_segment" },
                Prefixes, #0xFF,
                Mod = md, 0, 1, 0, Rm { md } = rm,
            ] = JumpTarget::Absolute(rm),
            map |target: JumpTarget| BuildFromContext::new(move |ctx| {
                let sp_size = ctx.sp_size();
                let size = ctx.size();
                let mem = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(-(size.num_bytes() as i64))),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                    ]),
                });

                checked_jump(ctx, Vec::new(), target, |_, ip_after, _| vec![
                    Cmd::mov(Val::Loc(mem), ip_after),
                    Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                        args: [
                            Val::from((GpReg::Sp, sp_size)),
                            Val::const_val(size.num_bytes() as u64),
                        ],
                        op: BinOp::Sub,
                    }),
                ])
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "RET_within_segment" },
                Prefixes, #0xC3,
            ] = Val::const_val(0),
            [
                Name { "RET_within_segment_imm16" },
                Wide { true },
                Prefixes, #0xC2,
                Imm { 16 } = disp,
            ] = disp,
            map |disp| BuildFromContext::new(move |ctx| {
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

                // TODO: Check if we stay inside SS limit.
                checked_jump(ctx, Vec::new(), JumpTarget::Absolute(Val::Loc(mem)), |_, _, _| vec![
                    Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                        args: [
                            Val::from((GpReg::Sp, sp_size)),
                            Val::const_val(size.num_bytes() as u64),
                        ],
                        op: BinOp::Add,
                    }),

                    // Pop extra bytes
                    Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                        args: [
                            Val::from((GpReg::Sp, sp_size)),
                            disp,
                        ],
                        op: BinOp::Add,
                    }),
                ])
            })
        },
        encoding! {
            Name { "ENTER" },
            Wide { true },
            Prefixes, #0xC8,
            ImmWithBitOrder { 16, (8..16).chain(0..8).map(ImmBitOrder::Negative).collect() } = disp,
            DontCare, DontCare, DontCare,
            Imm { 5 } = _level,
            {
                // TODO: Handle nesting levels
                // TODO: Generate access to RSP - disp + level * sp_size; We need a pagefault there if it isn't mapped.

                BuildFromContext::new(move |ctx| {
                    let sp_size = ctx.sp_size();
                    let size = ctx.size();
                    let mem = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(-(size.num_bytes() as i64))),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                        ]),
                    });

                    let write_check = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(1, 1),
                        calculation: ParameterizedComputation::Calculation({
                            let mut c = ctx.stack_segment_calculation();
                            c.add_term(AddrTerm::identity(match ctx.sp_size().num_bytes() {
                                2 => AddrTermSize::U16,
                                4 => AddrTermSize::U32,
                                _ => unreachable!(),
                            }));
                            c.with_offset(-(size.num_bytes() as i64))
                        }),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                            *disp.loc().unwrap(),
                        ]),
                    });

                    SemSpec {
                        commands:  ops! {
                            #[context(ctx)]

                            ..PerformMemoryReads([ write_check ]);
                            write_check := write_check;
                            ..PerformMemoryWrites([ write_check ]);

                            mem := (GpReg::Bp, size);
                            let new_bp = sub((GpReg::Sp, sp_size), size.num_bytes());
                            let new_sp = sub(new_bp, disp);
                            (GpReg::Sp, sp_size) := new_sp;
                            (GpReg::Bp, sp_size) := new_bp;

                            ..PerformMemoryWrites([ mem ]);

                        },
                        manual_memory_accesses: true,
                        ..Default::default()
                    }
                })
            }
        },
        encoding! {
            Name { "LEAVE" },
            Prefixes, #0xC9,
            {
                BuildFromContext::new(move |ctx| {
                    let size = match ctx.mode() {
                        Mode::RealOrProtected16 => 2,
                        Mode::Protected32 => 4,
                    };

                    let mut ops = Vec::new();
                    let mem = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(size, size),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation()),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Bp.into()), size: ctx.sp_size() },
                        ]),
                    });

                    ops.extend([
                        Cmd::store(Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: ctx.sp_size() }), Op::BinOp {
                            args: [
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Bp.into()), size: ctx.sp_size() }),
                                Val::const_val(size),
                            ],
                            op: BinOp::Add,
                        }),
                        Cmd::mov(Val::Loc(ParLoc { loc: UnsizedParLoc::Reg(GpReg::Bp.into()), size: ctx.sp_size() }), Val::Loc(mem)),
                    ]);
                    ops
                })
            }
        },
        // Intersegment calls and jumps
        encoding_group! {
            [
                Wide { true },
                Name { "CALL_direct_intersegment" },
                Prefixes, #0x9A,
                FullImm = disp, Imm { 16 } = selector,
            ] = (disp, selector),
            [
                Wide { true },
                Name { "CALL_indirect_intersegment" },
                Prefixes, #0xFF,
                Mod = md, 0, 1, 1, FarPointerRm { md } = (ip, cs),
            ] = (ip, cs),
            map |(disp, selector)| BuildFromContext::new(move |ctx| SemSpec {
                commands: {
                    let sp_size = ctx.sp_size();
                    let size = ctx.size();
                    let mem_cs = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(-(size.num_bytes() as i64))),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                        ]),
                    });

                    let mem_ip = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(-(2 * size.num_bytes() as i64))),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                        ]),
                    });

                    let mut ops = vec![
                        // TODO: Check SS limits, check new CS limits

                        // Push CS
                        Cmd::mov(Val::Loc(mem_cs), Val::from((GpReg::Cs, WORD))),

                        // Push IP
                        Cmd::store(Val::Loc(mem_ip), Op::BinOp {
                            args: [
                                Val::from((GpReg::Ip, DWORD)),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::InstrLen, size: Size::from_bytes(ctx.addr_size()) }),
                            ],
                            op: BinOp::Add,
                        }),

                        // Decrement SP
                        Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                            args: [
                                Val::from((GpReg::Sp, sp_size)),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(size.num_bytes() as u64 * 2), size: sp_size }),
                            ],
                            op: BinOp::Sub,
                        }),
                    ];

                    // TODO: Invoke far call handler, because we need to potentially push much more than this.

                    ops.extend(load_and_check_segment(GpReg::Cs, selector, vec![
                        Cmd::mov(
                            Val::from((GpReg::Ip, DWORD)),
                            disp,
                        ),
                        Cmd::Handler {
                            id: HANDLER_CS_UPDATED,
                            args: Vec::new(),
                        }
                    ], true, ctx));
                    ops
                },
                jump: Jump::Far,
                ..Default::default()
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "JMP_direct_intersegment" },
                Prefixes, #0xEA,
                FullImm = disp, Imm { 16 } = selector,
            ] = (disp, selector),
            [
                Wide { true },
                Name { "JMP_indirect_intersegment" },
                Prefixes, #0xFF,
                Mod = md, 1, 0, 1, FarPointerRm { md } = (ip, cs),
            ] = (ip, cs),
            map |(disp, selector)| BuildFromContext::new(move |ctx| SemSpec {
                commands: load_and_check_segment(GpReg::Cs, selector, vec![
                    Cmd::mov(
                        Val::from((GpReg::Ip, DWORD)),
                        disp,
                    ),
                    Cmd::Handler {
                        id: HANDLER_CS_UPDATED,
                        args: Vec::new(),
                    },
                ], true, ctx),
                jump: Jump::Far,
                ..Default::default()
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "RET_intersegment" },
                Prefixes, #0xCB,
            ] = Val::const_val(0),
            [
                Wide { true },
                Name { "RET_intersegment_imm16" },
                Prefixes, #0xCA,
                Imm { 16 } = disp,
            ] = disp,
            map |sp_offset| BuildFromContext::new(move |ctx| SemSpec {
                commands: {
                    let sp_size = ctx.sp_size();
                    let size = ctx.size();
                    let mem_ip = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(size.num_bytes() as u64, size.num_bytes() as u64),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation()),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                        ]),
                    });

                    let mem_cs = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(2, 2),
                        calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation().with_offset(size.num_bytes() as i64)),
                        alignment: 1,
                        inputs: Inputs::unsorted(vec![
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                            ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                        ]),
                    });

                    // TODO: Check if we stay inside SS limit, check if return target is within current CS. (copied from call, does this need to happen???)
                    load_and_check_segment(GpReg::Cs, Val::Loc(mem_cs), vec![
                        Cmd::mov(Val::from((GpReg::Ip, DWORD)), Val::Loc(mem_ip)),
                        Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                            args: [
                                Val::from((GpReg::Sp, sp_size)),
                                Val::Loc(ParLoc { loc: UnsizedParLoc::Const(size.num_bytes() as u64 * 2), size: sp_size }),
                            ],
                            op: BinOp::Add,
                        }),

                        // Pop extra bytes
                        Cmd::store(Val::from((GpReg::Sp, sp_size)), Op::BinOp {
                            args: [
                                Val::from((GpReg::Sp, sp_size)),
                                sp_offset,
                            ],
                            op: BinOp::Add,
                        }),

                        Cmd::Handler {
                            id: HANDLER_CS_UPDATED,
                            args: Vec::new(),
                        },
                    ], true, ctx)
                },
                jump: Jump::Far,
                ..Default::default()
            })
        },
    ]
}
