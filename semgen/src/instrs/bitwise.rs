use sem86_core::arch::intel386::{GpReg, Intel386};
use sem86_core::il::{BinOp, Cmd, Commands, Op, UnOp, Val};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::arith::{compute_pf, compute_sf, compute_zf};
use crate::instrs::{FLAG_AF, FLAG_CF, FLAG_OF};
use crate::{Config, encoding, encoding_group, ops};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Rol,
    Ror,
    Rcl,
    Rcr,
    Shl,
    Shr,
    Unused,
    Sar,
}

impl From<u64> for Kind {
    fn from(value: u64) -> Self {
        match value {
            0 => Kind::Rol,
            1 => Kind::Ror,
            2 => Kind::Rcl,
            3 => Kind::Rcr,
            4 => Kind::Shl,
            5 => Kind::Shr,
            6 => Kind::Unused,
            7 => Kind::Sar,
            _ => unreachable!(),
        }
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Name { "SHIFT_1" },
                Prefixes,
                1, 1, 0, 1, 0, 0, 0, W,
                Mod = md, BitsInto::<Kind> { 3 } = ttt, Rm { md } = rm,
            ] = (rm, ttt, Val::const_val(1)),
            [
                Name { "SHIFT_cl" },
                Prefixes,
                1, 1, 0, 1, 0, 0, 1, W,
                Mod = md, BitsInto::<Kind> { 3 } = ttt, Rm { md } = rm,
                FixedReg { GpReg::Cx } = count,
            ] = (rm, ttt, count),
            [
                Name { "SHIFT_imm" },
                Prefixes,
                1, 1, 0, 0, 0, 0, 0, W,
                Mod = md, BitsInto::<Kind> { 3 } = ttt, Rm { md } = rm,
                Imm { 8 } = count,
            ] = (rm, ttt, count),

            map |(rm, ttt, count)| BuildFromContext::new(move |ctx| {
                ops! {
                    #[context(ctx)]

                    let val = rm;
                    let masked_count = and(count, 0x1f);

                    if !is_zero(masked_count) {
                        #[match ttt] {
                            Kind::Rcl | Kind::Rcr => {
                                // Add CF into operand
                                let cf = shl(FLAG_CF, ctx.op_size() as u64 * 8);
                                val := or(cf, val);
                            }
                            _ => {}
                        }

                        let result = Op::BinOp {
                            args: [
                                val,
                                masked_count,
                            ],
                            op: match ttt {
                                Kind::Rol => BinOp::Rol(ctx.op_size() as u8 * 8),
                                Kind::Ror => BinOp::Ror(ctx.op_size() as u8 * 8),
                                Kind::Rcl => BinOp::Rol(ctx.op_size() as u8 * 8 + 1), // RCL
                                Kind::Rcr => BinOp::Ror(ctx.op_size() as u8 * 8 + 1), // RCR
                                Kind::Shl | Kind::Unused => BinOp::Shl,
                                Kind::Shr => BinOp::Shr,
                                Kind::Sar => BinOp::Sar(ctx.op_size() as u8),
                            },
                        };

                        #[match ttt] {
                            Kind::Shl | Kind::Shr | Kind::Sar => {
                                // Undefined. This mimics Bochs' behavior.
                                FLAG_AF := 0;
                            }
                            _ => {}
                        }

                        // Compute CF
                        #[match ttt] {
                            Kind::Rol | Kind::Unused => {
                                FLAG_CF := select_bit(result, 0);
                            },
                            Kind::Rcl | Kind::Rcr | Kind::Shl => {
                                FLAG_CF := select_bit(result, ctx.op_size() as u8 * 8);
                            },
                            Kind::Ror => {
                                FLAG_CF := select_bit(result, ctx.op_size() as u8 * 8 - 1);
                            },
                            Kind::Shr | Kind::Sar => {
                                let masked_count_minus_1 = sub(masked_count, 1);
                                let shifted = shr(val, masked_count_minus_1);
                                FLAG_CF := select_bit(shifted, 0);
                            }
                        }

                        // Compute OF
                        #[match ttt] {
                            Kind::Rol | Kind::Shl | Kind::Rcl => {
                                let tmp = select_bit(result, ctx.op_size() as u8 * 8 - 1);
                                FLAG_OF := xor(FLAG_CF, tmp);
                            },
                            Kind::Ror | Kind::Rcr => {
                                let top1 = select_bit(result, ctx.op_size() as u8 * 8 - 1);
                                let top2 = select_bit(result, ctx.op_size() as u8 * 8 - 2);
                                FLAG_OF := xor(top1, top2);
                            },
                            Kind::Shr => {
                                let masked_count_minus_1 = sub(masked_count, 1);
                                let original_top = select_bit(rm, ctx.op_size() as u8 * 8 - 1);
                                // Set to zero if masked count >= 2 to mimic Bochs' behavior.
                                FLAG_OF := ite(masked_count_minus_1, original_top, 0);
                            }
                            Kind::Sar | Kind::Unused => {
                                FLAG_OF := 0;
                            },
                        }

                        // Compute PF, SF, ZF
                        #[match ttt] {
                            // ROL/ROR/RCL/RCR only update the ZF
                            // TODO: Other flags?
                            Kind::Shl | Kind::Shr | Kind::Sar => {
                                ..compute_pf(result);
                                ..compute_zf(ctx, ctx.op_size() * 8, result);
                                ..compute_sf(ctx.op_size() * 8, result);
                            },
                            _ => {},
                        }

                        rm := result;
                    }
                }
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "SHLD - Register/Memory by Immediate" },
                Prefixes, #0x0F, #0xA4,
                ModRm = (reg, rm),
                Imm { 8 } = count,
            ] = (reg, rm, count),
            [
                Wide { true },
                Name { "SHLD_cl" },
                Prefixes, #0x0F, #0xA5,
                ModRm = (reg, rm),
                FixedReg { GpReg::Cx } = cl,
            ] = (reg, rm, cl),
            map |(reg, rm, count)| BuildFromContext::new(move |ctx| {
                let mut v = vec![
                    // Perform the shift and store it in the upper half of temp0
                    Cmd::store(Val::Temp(0), Op::BinOp {
                        args: [
                            Val::Temp(1),
                            Val::Temp(2),
                        ],
                        op: BinOp::Shl,
                    }),

                    // CF: last bit shifted out of destination operand (RM)
                    // temp8 = size - count
                    Cmd::store(Val::Temp(8), Op::BinOp {
                        args: [
                            Val::const_val(ctx.op_size() as u64 * 8),
                            count,
                        ],
                        op: BinOp::Sub,
                    }),
                    // temp9 = rm[size - count]
                    Cmd::store(Val::Temp(9), Op::BinOp {
                        args: [
                            rm,
                            Val::Temp(8),
                        ],
                        op: BinOp::Shr,
                    }),
                    // CF = temp9 & 1
                    Cmd::store(Val::Loc(FLAG_CF), Op::BinOp {
                        args: [
                            Val::Temp(9),
                            Val::const_val(1),
                        ],
                        op: BinOp::And,
                    }),

                    // Move result to lower half of temp0
                    Cmd::store(Val::Temp(0), Op::BinOp {
                        args: [
                            Val::Temp(0),
                            Val::const_val(ctx.op_size() as u64 * 8),
                        ],
                        op: BinOp::Shr,
                    }),

                    // OF: CF XOR SF
                    Cmd::store(Val::Temp(10), Op::UnOp {
                        arg: Val::Temp(0),
                        op: UnOp::SelectBit(ctx.op_size() as u8 * 8 - 1),
                    }),
                    Cmd::store(Val::Loc(FLAG_OF), Op::BinOp {
                        args: [
                            Val::Temp(10),
                            Val::Loc(FLAG_CF),
                        ],
                        op: BinOp::Xor,
                    }),

                    // Undefined, but this mimics bochs' behavior
                    Cmd::mov(Val::Loc(FLAG_AF), Val::const_val(0)),

                    // Store result in destination register
                    Cmd::mov(rm, Val::Temp(0)),
                ];

                v.extend(compute_zf(ctx, ctx.op_size() * 8, Val::Temp(0)));
                v.push(compute_sf(ctx.op_size() * 8, Val::Temp(0)));
                v.extend([
                    compute_pf(Val::Temp(0)),
                ]);

                vec![
                    // Store the double-sized operand in temp1
                    Cmd::store(Val::Temp(1), Op::BinOp {
                        args: [
                            rm,
                            Val::const_val(ctx.op_size() as u64 * 8),
                        ],
                        op: BinOp::Shl,
                    }),
                    Cmd::store(Val::Temp(1), Op::BinOp {
                        args: [
                            reg,
                            Val::Temp(1),
                        ],
                        op: BinOp::Or,
                    }),

                    // temp2 = masked count
                    Cmd::store(Val::Temp(2), Op::BinOp {
                        args: [
                            count,
                            Val::const_val(0x1f),
                        ],
                        op: BinOp::And,
                    }),

                    Cmd::If {
                        val: Val::Temp(2),
                        if_zero: Commands::Ops(Vec::new()),
                        if_nonzero: Commands::Ops(v),
                    },
                ]
            })
        },
        encoding_group! {
            [
                Wide { true },
                Name { "SHRD - Register/Memory by Immediate" },
                Prefixes, #0x0F, #0xAC,
                ModRm = (reg, rm),
                Imm { 8 } = count,
            ] = (reg, rm, count),
            [
                Wide { true },
                Name { "SHRD_cl" },
                Prefixes, #0x0F, #0xAD,
                ModRm = (reg, rm),
                FixedReg { GpReg::Cx } = cl,
            ] = (reg, rm, cl),
            map |(reg, rm, count)| BuildFromContext::new(move |ctx| {
                let mut v = vec![
                    // Perform the shift and store it in temp0
                    Cmd::store(Val::Temp(0), Op::BinOp {
                        args: [
                            Val::Temp(1),
                            Val::Temp(2),
                        ],
                        op: BinOp::Shr,
                    }),

                    // Store result in destination register
                    Cmd::mov(rm, Val::Temp(0)),

                    // CF: last bit shifted out of destination operand (RM)
                    Cmd::store(Val::Temp(4), Op::BinOp {
                        args: [
                            Val::Temp(2),
                            Val::const_val(1),
                        ],
                        op: BinOp::Sub,
                    }),
                    Cmd::store(Val::Temp(5), Op::BinOp {
                        args: [
                            Val::Temp(1),
                            Val::Temp(4),
                        ],
                        op: BinOp::Shr,
                    }),
                    Cmd::store(Val::Loc(FLAG_CF), Op::UnOp {
                        arg: Val::Temp(5),
                        op: UnOp::SelectBit(0),
                    }),


                    // OF: XOR of the top 2 bits.
                    Cmd::store(Val::Temp(10), Op::UnOp {
                        arg: Val::Temp(0),
                        op: UnOp::SelectBit(ctx.op_size() as u8 * 8 - 1),
                    }),
                    Cmd::store(Val::Temp(11), Op::UnOp {
                        arg: Val::Temp(0),
                        op: UnOp::SelectBit(ctx.op_size() as u8 * 8 - 2),
                    }),
                    Cmd::store(Val::Loc(FLAG_OF), Op::BinOp {
                        args: [
                            Val::Temp(10),
                            Val::Temp(11),
                        ],
                        op: BinOp::Xor,
                    }),

                    // Undefined, but this mimics bochs' behavior
                    Cmd::mov(Val::Loc(FLAG_AF), Val::const_val(0)),
                ];

                v.extend(compute_zf(ctx, ctx.op_size() * 8, Val::Temp(0)));
                v.push(compute_sf(ctx.op_size() * 8, Val::Temp(0)));
                v.extend([
                    compute_pf(Val::Temp(0)),
                ]);

                vec![
                    // Store the double-sized operand in temp1
                    Cmd::store(Val::Temp(1), Op::BinOp {
                        args: [
                            reg,
                            Val::const_val(ctx.op_size() as u64 * 8),
                        ],
                        op: BinOp::Shl,
                    }),
                    Cmd::store(Val::Temp(1), Op::BinOp {
                        args: [
                            rm,
                            Val::Temp(1),
                        ],
                        op: BinOp::Or,
                    }),

                    // temp2 = masked count
                    Cmd::store(Val::Temp(2), Op::BinOp {
                        args: [
                            count,
                            Val::const_val(0x1f),
                        ],
                        op: BinOp::And,
                    }),
                    Cmd::If {
                        val: Val::Temp(2),
                        if_zero: Commands::Ops(Vec::new()),
                        if_nonzero: Commands::Ops(v),
                    },
                ]
            })
        },
    ]
}
