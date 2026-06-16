use liblisa::encoding::dataflows::{AccessKind, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use sem86_core::arch::intel386::{GpReg, HANDLER_IF_UPDATED, Intel386};
use sem86_core::il::{BinOp, Cmd, Op, Val};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::other::gp_in_vm_ioplnot3_or;
use crate::instrs::{
    DWORD, FLAG_AC, FLAG_AF, FLAG_CF, FLAG_DF, FLAG_ID, FLAG_IF, FLAG_NT, FLAG_OF, FLAG_PF, FLAG_SF, FLAG_TF, FLAG_VM, FLAG_ZF,
    HIGH_BYTE, LOW_BYTE, WORD, invoke_gp,
};
use crate::{Config, encoding, encoding_group, ops};

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Name { "CLC" },
                Prefixes, #0xF8,
            ] = (FLAG_CF, 0),
            [
                Name { "CLD" },
                Prefixes, #0xFC,
            ] = (FLAG_DF, 0),
            [
                Name { "CLI" },
                Prefixes, #0xFA,
            ] = (FLAG_IF, 0),
            [
                Name { "STC" },
                Prefixes, #0xF9,
            ] = (FLAG_CF, 1),
            [
                Name { "STD" },
                Prefixes, #0xFD,
            ] = (FLAG_DF, 1),
            [
                Name { "STI" },
                Prefixes, #0xFB,
            ] = (FLAG_IF, 1),
            map |(target, val)| BuildFromContext::new(move |ctx| ops! {
                #[context(ctx)]

                // TODO: Follow table here: https://www.felixcloutier.com/x86/sti
                #[if target == FLAG_IF] {
                    let cpl_is_not_3 = xor((GpReg::Iopl, LOW_BYTE), 3);
                    let iopl_less_than_cpl = cmp_lt((GpReg::Iopl, LOW_BYTE), (GpReg::Cpl, LOW_BYTE));
                    let vm = FLAG_VM;
                    let cannot_update_if = ite(vm, iopl_less_than_cpl, cpl_is_not_3);

                    if is_zero(cannot_update_if) {
                        Val::Loc(target) := Val::const_val(val);

                        ..Cmd::Handler {
                            id: HANDLER_IF_UPDATED,
                            args: Vec::new(),
                        };
                    } else {
                        ..invoke_gp();
                    }
                } else {
                    Val::Loc(target) := Val::const_val(val);
                }
            })
        },
        encoding! {
            Name { "CLTS" },
            Prefixes, #0x0F, #0x06,
            {
                let cr0 = Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(GpReg::Cr0.into()),
                    size: DWORD,
                });
                BuildFromContext::new(move |_| vec![
                    Cmd::store(cr0, Op::BinOp {
                        args: [
                            cr0,
                            Val::const_val(0xfffffff7),
                        ],
                        op: BinOp::And,
                    }),
                ])
            }
        },
        encoding! {
            Name { "CMC" },
            Prefixes, #0xF5,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::store(Val::Loc(FLAG_CF), Op::BinOp {
                        args: [
                            Val::Loc(FLAG_CF),
                            Val::Loc(ParLoc { loc: UnsizedParLoc::Const(1), size: WORD })
                        ],
                        op: BinOp::Xor,
                    }),
                ])
            }
        },
        encoding! {
            Name { "LAHF" },
            Prefixes, #0x9F,
            {
                BuildFromContext::new(move |ctx| ops! {
                    #[context(ctx)]
                    let result = 0;
                    let val;
                    val := shl(FLAG_CF, 0);
                    result := or(result, val);

                    val := shl(FLAG_PF, 2);
                    result := or(result, val);

                    val := shl(FLAG_AF, 4);
                    result := or(result, val);

                    val := shl(FLAG_ZF, 6);
                    result := or(result, val);

                    val := shl(FLAG_SF, 7);
                    result := or(result, val);

                    // Set reserved bits
                    // TODO: For 8088 this is 0xf002
                    result := or(result, 0x0002);

                    (GpReg::Ax, HIGH_BYTE) := result;

                })
            }
        },
        encoding! {
            Name { "SAHF" },
            Prefixes, #0x9E,
            {
                BuildFromContext::new(move |ctx| ops! {
                    #[context(ctx)]
                    let tmp = (UnsizedParLoc::Reg(GpReg::Ax.into()), HIGH_BYTE);
                    FLAG_CF := select_bit(tmp, 0);
                    FLAG_PF := select_bit(tmp, 2);
                    FLAG_AF := select_bit(tmp, 4);
                    FLAG_ZF := select_bit(tmp, 6);
                    FLAG_SF := select_bit(tmp, 7);

                })
            }
        },
        encoding! {
            Wide { true },
            Name { "POPF" },
            Prefixes, AjustForPop, #0x9D,
            {
                BuildFromContext::new(move |ctx| {
                    let sp_size = ctx.sp_size();
                    gp_in_vm_ioplnot3_or(ops! {
                        #[context(ctx)]
                        let tmp = ctx.add_access(MemoryAccess {
                            kind: AccessKind::InputOutput,
                            size: MemorySizeRange::single(ctx.op_size() as u64),
                            calculation: ParameterizedComputation::Calculation(ctx.stack_segment_calculation()),
                            alignment: 1,
                            inputs: Inputs::unsorted(vec![
                                ParLoc { loc: UnsizedParLoc::Reg(GpReg::SsBase.into()), size: DWORD },
                                ParLoc { loc: UnsizedParLoc::Reg(GpReg::Sp.into()), size: DWORD },
                            ]),
                        });
                        FLAG_CF := select_bit(tmp, 0);
                        FLAG_PF := select_bit(tmp, 2);
                        FLAG_AF := select_bit(tmp, 4);
                        FLAG_ZF := select_bit(tmp, 6);
                        FLAG_SF := select_bit(tmp, 7);
                        FLAG_TF := select_bit(tmp, 8);
                        FLAG_IF := select_bit(tmp, 9);
                        FLAG_DF := select_bit(tmp, 10);
                        FLAG_OF := select_bit(tmp, 11);
                        FLAG_NT := select_bit(tmp, 14);

                        let iopl = shr(tmp, 12);
                        (GpReg::Iopl, LOW_BYTE) := and(iopl, 3);

                        // TODO: RF, VM?, VIF, VIP
                        #[if ctx.op_size() >= 4] {
                            FLAG_AC := select_bit(tmp, 18);
                            FLAG_ID := select_bit(tmp, 21);
                        }

                        (GpReg::Sp, sp_size) += ctx.op_size();

                    }, ctx)
                })
            }
        },
        encoding! {
            Wide { true },
            Name { "PUSHF" },
            Prefixes, #0x9C,
            {
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

                    gp_in_vm_ioplnot3_or(ops! {
                        #[context(ctx)]
                        let result = 0;
                        let val;
                        val := shl(FLAG_CF, 0);
                        result := or(result, val);

                        val := shl(FLAG_PF, 2);
                        result := or(result, val);

                        val := shl(FLAG_AF, 4);
                        result := or(result, val);

                        val := shl(FLAG_ZF, 6);
                        result := or(result, val);

                        val := shl(FLAG_SF, 7);
                        result := or(result, val);

                        val := shl(FLAG_IF, 9);
                        result := or(result, val);

                        val := shl(FLAG_DF, 10);
                        result := or(result, val);

                        val := shl(FLAG_OF, 11);
                        result := or(result, val);

                        val := shl((GpReg::Iopl, LOW_BYTE), 12);
                        result := or(result, val);

                        val := shl(FLAG_NT, 14);
                        result := or(result, val);

                        val := shl(FLAG_VM, 17);
                        result := or(result, val);

                        val := shl(FLAG_AC, 18);
                        result := or(result, val);

                        val := shl(FLAG_ID, 21);
                        result := or(result, val);

                        // Set reserved bits
                        // TODO: For 8088 this is 0xf002
                        result := or(result, 0x0002);

                        mem := result;
                        (GpReg::Sp, sp_size) -= size.num_bytes();

                    }, ctx)
                })
            }
        },
    ]
}
