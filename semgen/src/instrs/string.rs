use liblisa::encoding::dataflows::{AccessKind, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_core::arch::intel386::{GpReg, Intel386};
use sem86_core::il::{BinOp, Cmd, Jump, Op, Val};

use super::FLAG_RF;
use crate::builder::*;
use crate::context::{BuildFromContext, Context};
use crate::dsl::*;
use crate::instrs::arith::{compute_cf, compute_pf, compute_sf, compute_sub_of, compute_zf};
use crate::instrs::{DWORD, FLAG_AF, FLAG_DF, FLAG_ZF};
use crate::{Config, encoding, encoding_group, ops};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Cmps,
    Lods,
    Movs,
    Scas,
    Stos,
}

pub fn handle_rep(rep: bool, x: Vec<Cmd<Intel386>>, ctx: &mut Context) -> SemSpec<Intel386> {
    let cx_size = Size::from_bytes(ctx.addr_size());
    SemSpec {
        commands: ops! {
            #[context(ctx)]
            #[if !rep] {
                ..x;
            } else {
                let should_repeat = (GpReg::Cx, cx_size);
                if !is_zero(should_repeat) {
                    (GpReg::Cx, cx_size) -= 1;
                    FLAG_RF := ite((GpReg::Cx, cx_size), FLAG_RF, 1);

                    ..x;
                }
            }
        },
        manual_memory_accesses: true,
        jump: if rep {
            // Repeat as long as CX is non-zero
            Jump::Repeat {
                condition: (GpReg::Cx, cx_size).into(),
            }
        } else {
            Jump::Sequential
        },
        ..Default::default()
    }
}

pub fn handle_repz(rep: bool, repeat_when_empty: bool, x: Vec<Cmd<Intel386>>, ctx: &mut Context) -> SemSpec<Intel386> {
    let cx_size = Size::from_bytes(ctx.addr_size());
    let should_repeat = ctx.fresh_temp_var();
    SemSpec {
        commands: ops! {
            #[context(ctx)]
            #[if !rep] {
                ..x.to_vec();

            } else {
                if !is_zero((GpReg::Cx, cx_size)) {
                    ..x.to_vec();

                    (GpReg::Cx, cx_size) -= 1;
                    let should_repeat_from_zf = ite(FLAG_ZF, repeat_when_empty as u64 ^ 1, repeat_when_empty as u64);
                    should_repeat := ite((GpReg::Cx, cx_size), 0, should_repeat_from_zf);
                    FLAG_RF := ite(should_repeat, FLAG_RF, 1);
                }
            }
        },
        manual_memory_accesses: true,
        jump: if rep {
            // Repeat as long as CX is non-zero
            Jump::Repeat {
                condition: should_repeat,
            }
        } else {
            Jump::Sequential
        },
        ..Default::default()
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [encoding_group! {
        [
            Name { "CMPS" },
            LegacyPrefixesWithRep = rep,
            1, 0, 1, 0, 0, 1, 1, W,
        ] = (rep, Kind::Cmps),
        [
            Name { "MOVS" },
            LegacyPrefixesWithRep = rep,
            1, 0, 1, 0, 0, 1, 0, W,
        ] = (rep, Kind::Movs),
        [
            Name { "LODS" },
            LegacyPrefixesWithRep = rep,
            1, 0, 1, 0, 1, 1, 0, W,
        ] = (rep, Kind::Lods),
        [
            Name { "SCAS" },
            LegacyPrefixesWithRep = rep,
            1, 0, 1, 0, 1, 1, 1, W,
        ] = (rep, Kind::Scas),
        [
            Name { "STOS" },
            LegacyPrefixesWithRep = rep,
            1, 0, 1, 0, 1, 0, 1, W,
        ] = (rep, Kind::Stos),
        map |(repeat_when_empty, kind): (Option<bool>, Kind)| BuildFromContext::new(move |ctx| {
            let inputs = Inputs::unsorted(vec![
                ctx.segment_override().unwrap_or(ParLoc { loc: UnsizedParLoc::Reg(GpReg::DsBase.into()), size: DWORD }),
                ParLoc { loc: UnsizedParLoc::Reg(GpReg::Si.into()), size: Size::from_bytes(ctx.addr_size()) },
            ]);
            let src = match kind {
                Kind::Scas | Kind::Stos => ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: ctx.size() },
                Kind::Movs | Kind::Cmps | Kind::Lods => ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(ctx.op_size() as u64, ctx.op_size() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.segment_calculation().clone()),
                    alignment: 1,
                    inputs,
                }),
            };

            let dst = match kind {
                Kind::Movs | Kind::Cmps | Kind::Scas | Kind::Stos => ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(ctx.op_size() as u64, ctx.op_size() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.segment_calculation().clone()),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::EsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Di.into()), size: Size::from_bytes(ctx.addr_size()) },
                    ]),
                }),
                Kind::Lods => ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: ctx.size() },
            };

            let rep = repeat_when_empty.is_some();
            let repeat_when_empty = repeat_when_empty.unwrap_or(false);

            let size = Size::from_bytes(ctx.addr_size());
            match kind {
                Kind::Movs => handle_rep(rep, ops! {
                    #[context(ctx)]
                    ..PerformMemoryReads([ src ]);
                    dst := src;
                    ..PerformMemoryWrites([ dst ]);

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Di, size) += step;
                    (GpReg::Si, size) += step;
                }, ctx),
                Kind::Cmps => handle_repz(rep, repeat_when_empty, ops! {
                    #[context(ctx)]

                    ..PerformMemoryReads([ src, dst ]);
                    let lhs = src;
                    let rhs = dst;
                    let result = sub(src, dst);

                    // Update the AF based on bit[4]((a & 0xf) - ((b & 0xf)))
                    let lhs_nibble = and(lhs, 0xf);
                    let rhs_nibble = and(rhs, 0xf);
                    let nibble_sum = sub(lhs_nibble, rhs_nibble);
                    FLAG_AF := select_bit(nibble_sum, 4);
                    ..compute_sub_of(ctx, ctx.op_size() * 8, result, lhs, rhs);
                    ..compute_cf(ctx, ctx.op_size() * 8, result, lhs, rhs);
                    ..compute_zf(ctx, ctx.op_size() * 8, result);
                    ..compute_sf(ctx.op_size() * 8, result);
                    ..compute_pf(result);

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Di, size) += step;
                    (GpReg::Si, size) += step;
                }, ctx),
                Kind::Scas => handle_repz(rep, repeat_when_empty, ops! {
                    #[context(ctx)]
                    ..PerformMemoryReads([ dst ]);
                    let lhs = src;
                    let rhs = dst;
                    let result = sub(src, dst);

                    // Update the AF based on bit[4]((a & 0xf) - ((b & 0xf)))
                    let lhs_nibble = and(lhs, 0xf);
                    let rhs_nibble = and(rhs, 0xf);
                    let nibble_sum = sub(lhs_nibble, rhs_nibble);
                    FLAG_AF := select_bit(nibble_sum, 4);
                    ..compute_sub_of(ctx, ctx.op_size() * 8, result, lhs, rhs);
                    ..compute_cf(ctx, ctx.op_size() * 8, result, lhs, rhs);
                    ..compute_zf(ctx, ctx.op_size() * 8, result);
                    ..compute_sf(ctx.op_size() * 8, result);
                    ..compute_pf(result);

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Di, size) += step;
                }, ctx),
                Kind::Lods => handle_rep(rep, ops![
                    #[context(ctx)]
                    ..PerformMemoryReads([ src ]);
                    dst := src;

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Si, size) += step;
                ], ctx),
                Kind::Stos => handle_rep(rep, ops![
                    #[context(ctx)]
                    dst := src;
                    ..PerformMemoryWrites([ dst ]);

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Di, size) += step;
                ], ctx)
            }
        })
    }]
}
