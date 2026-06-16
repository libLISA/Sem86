use liblisa::encoding::dataflows::{AccessKind, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_core::arch::intel386::{GpReg, Intel386};
use sem86_core::il::{BinOp, Cmd, Op, Val};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::{DWORD, FLAG_DF, WORD};
use crate::{Config, encoding, encoding_group, ops};

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Name { "IN - Fixed Port" },
                Prefixes,
                1, 1, 1, 0, 0, 1, 0, W,
                Imm { 8 } = port,
                Acc = dst,
            ] = (Some(port), dst),
            [
                Name { "IN - Variable Port" },
                Prefixes,
                1, 1, 1, 0, 1, 1, 0, W,
                Acc = dst,
            ] = (None, dst),
            map |(port, dst): (Option<Val<Intel386>>, _)| BuildFromContext::new(move |ctx| vec![
                Cmd::In {
                    len: ctx.op_size(),
                    port: port.unwrap_or(Val::from((GpReg::Dx, WORD))),
                    data: dst,
                },
            ])
        },
        encoding_group! {
            [
                Name { "OUT - Fixed Port" },
                Prefixes,
                1, 1, 1, 0, 0, 1, 1, W,
                Acc = val,
                Imm { 8 } = port,
            ] = (Some(port), val),
            [
                Name { "OUT - Variable Port" },
                Prefixes,
                1, 1, 1, 0, 1, 1, 1, W,
                Acc = val,
            ] = (None, val),
            map |(port, val): (Option<Val<Intel386>>, _)| BuildFromContext::new(move |ctx| vec![
                Cmd::Out {
                    len: ctx.op_size(),
                    port: port.unwrap_or(Val::from((GpReg::Dx, WORD))),
                    data: val,
                },
            ])
        },
        encoding_group! {
            [
                Name { "REP INS" },
                LegacyPrefixesWithRep = rep,
                0, 1, 1, 0, 1, 1, 0, W,
            ] = rep.is_some(),
            map |rep| BuildFromContext::new(move |ctx| {
                let dst = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(ctx.op_size() as u64, ctx.op_size() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.segment_calculation().clone()),
                    alignment: 1,
                    inputs: Inputs::unsorted(vec![
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::EsBase.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Di.into()), size: Size::from_bytes(ctx.addr_size()) },
                    ]),
                });

                let size = Size::from_bytes(ctx.addr_size());
                super::string::handle_rep(rep, ops![
                    #[context(ctx)]
                    ..Cmd::In {
                        len: ctx.op_size(),
                        port: Val::from((GpReg::Dx, WORD)),
                        data: Val::Loc(dst),
                    };
                    ..PerformMemoryWrites([ dst ]);

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Di, size) += step;
                ], ctx)
            })
        },
        encoding_group! {
            [
                LegacyPrefixesWithRep = rep,
                Name { "OUTS" },
                0, 1, 1, 0, 1, 1, 1, W,
            ] = rep.is_some(),
            map |rep| BuildFromContext::new(move |ctx| {
                let inputs = Inputs::unsorted(vec![
                    ctx.segment_override().unwrap_or(ParLoc { loc: UnsizedParLoc::Reg(GpReg::DsBase.into()), size: DWORD }),
                    ParLoc { loc: UnsizedParLoc::Reg(GpReg::Si.into()), size: Size::from_bytes(ctx.addr_size()) },
                ]);
                let src = ctx.add_access(MemoryAccess {
                    kind: AccessKind::InputOutput,
                    size: MemorySizeRange::new(ctx.op_size() as u64, ctx.op_size() as u64),
                    calculation: ParameterizedComputation::Calculation(ctx.segment_calculation().clone()),
                    alignment: 1,
                    inputs,
                });

                let size = Size::from_bytes(ctx.addr_size());
                super::string::handle_rep(rep, ops![
                    #[context(ctx)]
                    ..PerformMemoryReads([ src ]);
                    ..Cmd::Out {
                        len: ctx.op_size(),
                        port: Val::from((GpReg::Dx, WORD)),
                        data: Val::Loc(src),
                    };

                    let step = ite(FLAG_DF, ctx.op_size() as u64, (ctx.op_size() as u64).wrapping_neg());
                    (GpReg::Si, size) += step;
                ], ctx)
            })
        },
    ]
}
