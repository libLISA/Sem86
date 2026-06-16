use liblisa::arch::Arch;
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use liblisa::utils::bitmask_u64;
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::{GpReg, Intel386, Reg};
use sem86_core::il::{Cmd, Op, UnOp, Val};

use super::{FLAG_AF, FLAG_CF, FLAG_OF, FLAG_PF, FLAG_SF, FLAG_ZF};
use crate::builder::*;
use crate::context::{BuildFromContext, Context};
use crate::dsl::*;
use crate::instrs::{DWORD, HIGH_BYTE, LOW_BYTE, WORD};
use crate::{Config, encoding, encoding_group, ops};

// TODO: Can we just use effective_rhs here to make the OF the same for all additions and subtractions?

// Compute add OF via ((lhs ^ result) & (rhs ^ result)) & 0x80 != 0
pub fn compute_add_of(
    ctx: &mut Context, num_bits: usize, result: Val<Intel386>, lhs: Val<Intel386>, rhs: Val<Intel386>,
) -> Vec<Cmd<Intel386>> {
    assert!(num_bits.is_multiple_of(8));
    assert!(num_bits <= 32);

    ops! {
        #[context(ctx)]

        let a = xor(result, lhs);
        let b = xor(result, rhs);
        let c = and(a, b);
        FLAG_OF := select_bit(c, num_bits as u8 - 1);
    }
}

// Compute sub OF via ((lhs ^ rhs) & (lhs ^ result)) & 0x80 != 0
pub fn compute_sub_of(
    ctx: &mut Context, num_bits: usize, result: Val<Intel386>, lhs: Val<Intel386>, rhs: Val<Intel386>,
) -> Vec<Cmd<Intel386>> {
    assert!(num_bits.is_multiple_of(8));
    assert!(num_bits <= 32);

    ops! {
        #[context(ctx)]

        let a = xor(lhs, rhs);
        let b = xor(result, lhs);
        let c = and(a, b);
        FLAG_OF := select_bit(c, num_bits as u8 - 1);
    }
}

pub fn compute_pf(result: Val<Intel386>) -> Cmd<Intel386> {
    Cmd::store(
        Val::Loc(FLAG_PF),
        Op::UnOp {
            arg: result,
            op: UnOp::Parity,
        },
    )
}

pub fn compute_zf(ctx: &mut Context, num_bits: usize, result: Val<Intel386>) -> Vec<Cmd<Intel386>> {
    ops! {
        #[context(ctx)]

        let masked = and(result, bitmask_u64(num_bits as u32));
        FLAG_ZF := is_zero(masked);
    }
}

pub fn compute_cf(
    ctx: &mut Context, num_bits: usize, result: Val<Intel386>, _lhs: Val<Intel386>, _rhs: Val<Intel386>,
) -> Vec<Cmd<Intel386>> {
    ops! {
        #[context(ctx)]
        FLAG_CF := select_bit(result, num_bits.try_into().unwrap());
    }
}

pub fn compute_sf(num_bits: usize, result: Val<Intel386>) -> Cmd<Intel386> {
    Cmd::store(
        Val::Loc(FLAG_SF),
        Op::UnOp {
            arg: result,
            op: UnOp::SelectBit((num_bits - 1).try_into().unwrap()),
        },
    )
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Sub,
    Or,
    Xor,
    And,
    Test,
    Add,
    Adc,
    Sbb,
    Dec,
    Inc,
    Cmp,
}

impl Kind {
    fn as_fn<A: Arch, L: Into<Val<A>>, R: Into<Val<A>>>(&self) -> impl Fn(L, R) -> Op<A> {
        match self {
            Kind::Sub | Kind::Sbb | Kind::Dec | Kind::Cmp => sub,
            Kind::Or => or,
            Kind::Xor => xor,
            Kind::And => and,
            Kind::Test => and,
            Kind::Add | Kind::Adc | Kind::Inc => add,
        }
    }
}

impl From<u64> for Kind {
    fn from(value: u64) -> Kind {
        match value {
            0 => Kind::Add,
            1 => Kind::Or,
            2 => Kind::Adc,
            3 => Kind::Sbb,
            4 => Kind::And,
            5 => Kind::Sub,
            6 => Kind::Xor,
            7 => Kind::Cmp,
            _ => unreachable!(),
        }
    }
}

pub fn builder(config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Lockable,
                Name { "ALU_reg_rm" },
                Prefixes,
                0, 0, BitsInto::<Kind> { 3 } = kind, 0, ExpandedBit = d, W,
                ModRm = (reg, rm),
                SrcDst { d, reg, rm } = (src, dst),
            ] = (kind, src, dst),
            [
                Lockable,
                Name { "ALU_imm_eax" },
                Prefixes,
                0, 0, BitsInto::<Kind> { 3 } = kind, 1, 0, W,
                FullImm = imm,
                Acc = acc,
            ] = (kind, imm, acc),
            [
                Lockable,
                Name { "ALU_imm_rm" },
                Prefixes,
                1, 0, 0, 0, 0, 0, S, W,
                Mod = md, BitsInto::<Kind> { 3 } = kind, Rm { md } = rm,
                FullImm = imm,
            ] = (kind, imm, rm),
            [
                Name { "TEST_rm" },
                Prefixes,
                1, 0, 0, 0, 0, 1, 0, W,
                ModRm = (reg, rm),
            ] = (Kind::Test, reg, rm),
            [
                Name { "TEST_imm" },
                Prefixes,
                1, 1, 1, 1, 0, 1, 1, W,
                Mod = md, 0, 0, 0, Rm { md } = rm,
                FullImm = imm,
            ] = (Kind::Test, imm, rm),
            [
                Name { "TEST_imm_eax" },
                Prefixes,
                1, 0, 1, 0, 1, 0, 0, W,
                FullImm = imm,
                Acc = acc,
            ] = (Kind::Test, imm, acc),
            [
                Lockable,
                Wide { true },
                Name { "INC/DEC_reg" },
                Prefixes,
                0, 1, 0, 0, ExpandedBit = decrement, RegBits = reg,
            ] = (if decrement { Kind::Dec } else { Kind::Inc }, Val::const_val(1), reg),
            [
                Lockable,
                Name { "INC_rm" },
                Prefixes,
                1, 1, 1, 1, 1, 1, 1, W,
                Mod = md, 0, 0, ExpandedBit = decrement, Rm { md } = rm,
            ] = (if decrement { Kind::Dec } else { Kind::Inc }, Val::const_val(1), rm),
            map |(kind, src, dst): (Kind, _, _)| BuildFromContext::new(move |ctx| ops! {
                #[context(ctx)]
                const mask = !(u64::MAX << (ctx.op_size() * 8));

                let lhs = and(dst, mask);
                let rhs = and(src, mask);
                let result;

                let effective_rhs = rhs;
                #[if kind == Kind::Adc || kind == Kind::Sbb] {
                    effective_rhs := add(effective_rhs, FLAG_CF);
                }

                result := (kind.as_fn())(lhs, effective_rhs);

                #[match kind] {
                    Kind::Xor | Kind::Or | Kind::And | Kind::Test => {
                        // AF is undefined. This mimics Bochs' behavior.
                        FLAG_AF := 0;

                        // OF should be set to zero, which we need to do explicitly,
                        // since the computation for add/sub could give non-zero results for these instructions.
                        FLAG_OF := 0;
                    }
                    Kind::Add | Kind::Inc | Kind::Adc | Kind::Sub | Kind::Dec | Kind::Cmp | Kind::Sbb => {
                        #[match kind] {
                            Kind::Add | Kind::Inc | Kind::Adc => {
                                ..compute_add_of(ctx, ctx.op_size() * 8, result, lhs, rhs);
                            }
                            Kind::Sub | Kind::Dec | Kind::Cmp | Kind::Sbb => {
                                ..compute_sub_of(ctx, ctx.op_size() * 8, result, lhs, rhs);
                            }
                            _ => {},
                        }

                        // Update the AF based on bit[4]((a & 0xf) +/- ((b & 0xf)))
                        let lhs_nibble = and(lhs, 0xf);
                        let rhs_nibble = and(rhs, 0xf);
                        // TODO: This should either include SBB, or ideally we would just use effective_rhs here...
                        #[if kind == Kind::Adc] {
                            rhs_nibble := add(rhs_nibble, FLAG_CF);
                        }
                        let nibble_sum = (kind.as_fn())(lhs_nibble, rhs_nibble);
                        FLAG_AF := select_bit(nibble_sum, 4);
                    }
                }

                // This sets CF=0 for Xor, Or, And and Test.
                #[if kind != Kind::Inc && kind != Kind::Dec] {
                    ..compute_cf(ctx, ctx.op_size() * 8, result, lhs, rhs);
                }

                ..compute_zf(ctx, ctx.op_size() * 8, result);
                ..compute_pf(result);
                ..compute_sf(ctx.op_size() * 8, result);

                // If we are not doing a compare, we need to save the result.
                #[if kind != Kind::Cmp && kind != Kind::Test] {
                    dst := result;
                }


            })
        },
        encoding! {
            Lockable,
            Name { "NEG" },
            Prefixes,
            1, 1, 1, 1, 0, 1, 1, W,
            Mod = md, 0, 1, 1, Rm { md } = rm,
            ops! {
                #[context(ctx)]
                let rhs = rm;
                FLAG_CF := ite(rhs, 0, 1);
                let result = xor(rhs, u64::MAX);
                let result = add(result, 1);
                rm := result;

                // Update the AF based on bit[4](0 - ((b & 0xf)))
                let rhs_nibble = and(rhs, 0xF);
                let nibble_sum = sub(0, rhs_nibble);
                FLAG_AF := select_bit(nibble_sum, 4);

                ..compute_sub_of(ctx, ctx.op_size() * 8, result, Val::const_val(0), rhs);
                ..compute_zf(ctx, ctx.op_size() * 8, result);
                ..compute_sf(ctx.op_size() * 8, result);
                ..compute_pf(result);

            }
        },
        encoding! {
            Name { "AAA" },
            Prefixes, #0x37,
            ops! {
                #[context(ctx)]

                let x = and((GpReg::Ax, LOW_BYTE), 0xf);
                let is_gt = cmp_gt(x, 9);
                FLAG_AF := or(is_gt, FLAG_AF);
                FLAG_CF := FLAG_AF;

                let tmp3 = and((GpReg::Ax, WORD), 0xff0f);
                let tmp1 = add(tmp3, 0x106);
                let tmp2 = ite(FLAG_AF, (GpReg::Ax, WORD), tmp1);
                (GpReg::Ax, WORD) := and(tmp2, 0xff0f);

            }
        },
        encoding! {
            Name { "AAS" },
            Prefixes, #0x3F,
            ops! {
                #[context(ctx)]
                let x = and((GpReg::Ax, LOW_BYTE), 0xf);
                let is_gt = cmp_gt(x, 9);
                let cond = or(is_gt, FLAG_AF);
                FLAG_CF := cond;
                FLAG_AF := cond;
                if !is_zero(cond) {
                    (GpReg::Ax, WORD) := sub((GpReg::Ax, WORD), 6);
                    (GpReg::Ax, HIGH_BYTE) := sub((GpReg::Ax, HIGH_BYTE), 1);
                }

                (GpReg::Ax, LOW_BYTE) := and((GpReg::Ax, LOW_BYTE), 0xf);

            }
        },
        encoding! {
            Name { "DAA" },
            Prefixes, #0x27,
            ops! {
                #[context(ctx)]
                let tmp0 = (GpReg::Ax, LOW_BYTE);
                let tmp1 = FLAG_CF;
                let tmp2 = and((GpReg::Ax, LOW_BYTE), 0x0f);
                let tmp3 = cmp_gt(tmp2, 9);
                FLAG_AF := or(tmp3, FLAG_AF);
                if !is_zero(FLAG_AF) {
                    (GpReg::Ax, LOW_BYTE) := add((GpReg::Ax, LOW_BYTE), 6);
                }

                let tmp6 = cmp_gt(tmp0, 0x99);
                FLAG_CF := or(tmp6, tmp1);

                if !is_zero(FLAG_CF) {
                    (GpReg::Ax, LOW_BYTE) := add((GpReg::Ax, LOW_BYTE), 0x60);
                    tmp0 := (GpReg::Ax, LOW_BYTE);
                }

                ..compute_zf(ctx, 8, tmp0);
                ..compute_sf(8, tmp0);
                ..compute_pf(tmp0);

            }
        },
        encoding! {
            Name { "DAS" },
            Prefixes, #0x2F,
            ops! {
                #[context(ctx)]

                let tmp0 = (GpReg::Ax, LOW_BYTE);
                let tmp1 = FLAG_CF;
                FLAG_CF := 0;
                let tmp2 = and((GpReg::Ax, LOW_BYTE), 0x0f);
                let tmp3 = cmp_gt(tmp2, 9);
                FLAG_AF := or(tmp3, FLAG_AF);
                if !is_zero(FLAG_AF) {
                    let tmp5 = cmp_lt((GpReg::Ax, LOW_BYTE), 6);
                    FLAG_CF := or(tmp1, tmp5);
                    (GpReg::Ax, LOW_BYTE) := sub((GpReg::Ax, LOW_BYTE), 6);
                }

                let tmp6 = cmp_gt(tmp0, 0x99);
                let tmp7 = or(tmp6, tmp1);
                if !is_zero(tmp7) {
                    FLAG_CF := 1;
                    (GpReg::Ax, LOW_BYTE) := sub((GpReg::Ax, LOW_BYTE), 0x60);
                }

                let result = (GpReg::Ax, LOW_BYTE);
                ..compute_zf(ctx, 8, result);
                ..compute_sf(8, result);
                ..compute_pf(result);

            }
        },
        encoding! {
            Name { "MUL" },
            Prefixes,
            1, 1, 1, 1, 0, 1, 1, W,
            Mod = md, 1, 0, 0, Rm { md } = rm,
            ops! {
                #[context(ctx)]

                let result = mul((GpReg::Ax, ctx.size()), rm);
                ..compute_pf(result);
                (GpReg::Ax, if ctx.op_size() < 2 {
                    Size::from_bytes(2)
                } else {
                    ctx.size()
                }) := result;

                let upper_half = and(result, match ctx.op_size() {
                    4 => 0xffffffff00000000u64,
                    2 => 0xffff0000,
                    1 => 0x0000ff00,
                    _ => unreachable!(),
                });
                if is_zero(upper_half) {
                    FLAG_CF := 0;
                    FLAG_OF := 0;
                } else {
                    FLAG_CF := 1;
                    FLAG_OF := 1;
                }

                // All these flags are undefined. This mimics Bochs' behavior.
                FLAG_AF := 0;
                FLAG_SF := select_bit(result, ctx.op_size() as u8 * 8 - 1);
                ..compute_pf(result);
                ..compute_zf(ctx, ctx.op_size() * 8, result);

                #[if ctx.op_size() >= 2] {
                    let upper_half = shr(result, ctx.op_size() as u64 * 8);
                    (GpReg::Dx, ctx.size()) := upper_half;
                }
            }
        },
        encoding_group! {
            [
                Name { "IMUL - Accumulator with Register/Memory" },
                Prefixes,
                1, 1, 1, 1, 0, 1, 1, W,
                Mod = md, 1, 0, 1, Rm { md } = rm,
                Acc = acc,
            ] = (acc, rm, None, true),
            [
                Wide { true },
                Name { "IMUL - Register with Register/Memory" },
                Prefixes, #0x0F, #0xAF,
                ModRm = (reg, rm),
            ] = (reg, rm, Some(reg), false),
            [
                Wide { true },
                Name { "IMUL - Register/Memory with Immediate to Register" },
                Prefixes,
                0, 1, 1, 0, 1, 0, S, 1,
                ModRm = (reg, rm),
                FullImm = imm,
            ] = (imm, rm, Some(reg), false),
            map |(a, b, target, overflow_dx): (Val<_>, Val<_>, Option<Val<_>>, bool)| BuildFromContext::new(move |ctx| {
                let real_target = target.unwrap_or(Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Ax)),
                    size: if ctx.op_size() >= 2 {
                        ctx.size()
                    } else {
                        WORD
                    },
                }));

                ops! {
                    #[context(ctx)]

                    let result = mul(a.sign_extend((ctx.op_size() * 8).try_into().unwrap(), 64), b.sign_extend((ctx.op_size() * 8).try_into().unwrap(), 64));
                    real_target := result;

                    // Set CF/OF if upper half + sign bit isn't all zeros or all ones
                    const upper_half_mask = match ctx.op_size() {
                        4 => 0xffffffff80000000u64,
                        2 => 0xffff8000,
                        1 => 0x0000ff80,
                        _ => unreachable!(),
                    };
                    let upper_half = and(result, upper_half_mask);
                    let flipped_upper_half = xor(upper_half, upper_half_mask);

                    let zero_half = ite(upper_half, upper_half, flipped_upper_half);
                    FLAG_CF := ite(zero_half, 0, 1);
                    FLAG_OF := FLAG_CF;

                    // All these flags are undefined. This mimics Bochs' behavior.
                    FLAG_AF := 0;
                    FLAG_SF := select_bit(result, if ctx.op_size() >= 2 {
                        ctx.op_size() as u8 * 8 - 1
                    } else {
                        15
                    });
                    ..compute_pf(result);

                    #[if !config.no_imul_zf] {
                        ..compute_zf(ctx, if ctx.op_size() >= 2 {
                            ctx.op_size() * 8
                        } else {
                            16
                        }, result);
                    }

                    #[if target.is_none() && ctx.op_size() >= 2 && overflow_dx] {
                        let upper_half = shr(result, ctx.op_size() as u64 * 8);
                        (GpReg::Dx, ctx.size()) := upper_half;
                    }
                }
            })
        },
        encoding! {
            Name { "DIV - Accumulator by Register/Memory" },
            Prefixes,
            1, 1, 1, 1, 0, 1, 1, W = wide,
            Mod = md, 1, 1, 0, Rm { md } = rm,
            ops! {
                #[context(ctx)]
                const ax_size = Size::from_bytes(if !wide {
                    2
                } else {
                    ctx.op_size_ext(ctx.mode(), true, ctx.has_wide_operand_size_override())
                });

                if is_zero(rm) {
                    ..(Exception::DivisionError, 0);
                } else {
                    let lhs;
                    #[if ctx.op_size() >= 2] {
                        lhs := shl((GpReg::Dx, ctx.size()), ctx.op_size() as u64 * 8);
                        lhs := or(lhs, (GpReg::Ax, ax_size));
                    } else {
                        lhs := (GpReg::Ax, ax_size);
                    }

                    let result = div(lhs, rm);
                    let remainder = modulo(lhs, rm);

                    let result_too_big = cmp_gt(result, match ctx.op_size() {
                        1 => 0xff,
                        2 => 0xffff,
                        4 => 0xffffffffu64,
                        _ => unreachable!(),
                    });
                    if is_zero(result_too_big) {
                        #[if ctx.op_size() >= 2] {
                            (GpReg::Ax, ctx.size()) := result;
                            (GpReg::Dx, ctx.size()) := remainder;
                        } else {
                            (GpReg::Ax, LOW_BYTE) := result;
                            (GpReg::Ax, HIGH_BYTE) := remainder;
                        }


                    } else {
                        ..(Exception::DivisionError, 0);
                    }
                }
            }
        },
        encoding! {
            Name { "IDIV - Accumulator By Register/Memory" },
            Prefixes,
            1, 1, 1, 1, 0, 1, 1, W,
            Mod = md, 1, 1, 1, Rm { md } = rm,
            ops! {
                #[context(ctx)]

                let lhs;
                #[if ctx.op_size() >= 2] {
                    lhs := shl((GpReg::Dx, ctx.size()), ctx.op_size() as u64 * 8);
                    lhs := or(lhs, (GpReg::Ax, ctx.size()));
                } else {
                    lhs := Val::Conv {
                        loc: ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: WORD },
                        source_bits: 16,
                        target_bits: 32,
                        sign_extend: true,
                        swap_endianness: false,
                    };
                }

                if is_zero(rm) {
                    ..(Exception::DivisionError, 0);
                } else {
                    const rm = match rm {
                        Val::Loc(loc) => loc,
                        _ => unreachable!(),
                    };

                    // TODO: I think we need to do signed 64-bit division here to account for overflow issues...
                    let result = signeddiv64(lhs, Val::Conv {
                        loc: rm,
                        source_bits: (ctx.op_size() * 8).try_into().unwrap(),
                        target_bits: 64,
                        sign_extend: true,
                        swap_endianness: false,
                    });
                    let remainder = signedmod64(lhs, Val::Conv {
                        loc: rm,
                        source_bits: (ctx.op_size() * 8).try_into().unwrap(),
                        target_bits: 64,
                        sign_extend: true,
                        swap_endianness: false,
                    });

                    let result_too_big = cmp_gt(result, match ctx.op_size() {
                        1 => 0x7f,
                        2 => 0x7fff,
                        4 => 0x7fffffff,
                        _ => unreachable!(),
                    });
                    let result_too_small = cmp_lt(result, match ctx.op_size() {
                        1 => 0xffff_ff80u64,
                        2 => 0xffff_8000,
                        4 => 0x8000_0000,
                        _ => unreachable!(),
                    });

                    let overflow = and(result_too_big, result_too_small);
                    if is_zero(overflow) {
                        #[if ctx.op_size() >= 2] {
                            (GpReg::Ax, ctx.size()) := result;
                            (GpReg::Dx, ctx.size()) := remainder;
                        } else {
                            (GpReg::Ax, LOW_BYTE) := result;
                            (GpReg::Ax, HIGH_BYTE) := remainder;
                        }


                    } else {
                        ..(Exception::DivisionError, 0);
                    }
                }
            }
        },
        encoding! {
            Name { "AAD" },
            Prefixes, #0xD5,
            Imm { 8 } = imm,
            ops! {
                #[context(ctx)]

                let low = (GpReg::Ax, LOW_BYTE);
                let high = (GpReg::Ax, HIGH_BYTE);
                let result = mul(high, imm);
                let result = add(result, low);
                let result = and(result, 0xff);
                (GpReg::Ax, LOW_BYTE) := result;
                (GpReg::Ax, HIGH_BYTE) := 0;
                ..compute_pf(result);
                ..compute_sf(8, result);
                ..compute_zf(ctx, 8, result);

            }
        },
        encoding! {
            Name { "AAM" },
            Prefixes, #0xD4,
            Imm { 8 } = imm,
            ops! {
                #[context(ctx)]
                if is_zero(imm) {
                    ..(Exception::DivisionError, 0);
                } else {
                    let tmp = (GpReg::Ax, LOW_BYTE);
                    (GpReg::Ax, HIGH_BYTE) := div(tmp, imm);
                    let low_result = modulo(tmp, imm);
                    (GpReg::Ax, LOW_BYTE) := low_result;
                    ..compute_pf(low_result);
                    ..compute_sf(8, low_result);
                    ..compute_zf(ctx, 8, low_result);

                }
            }
        },
        encoding! {
            Name { "NOT" },
            Lockable,
            Prefixes,
            1, 1, 1, 1, 0, 1, 1, W,
            Mod = md, 0, 1, 0, Rm { md } = rm,
            ops! {
                #[context(ctx)]
                rm := xor(rm, u64::MAX);

            }
        },
        encoding! {
            Name { "XADD" },
            Lockable,
            Prefixes, #0x0F,
            1, 1, 0, 0, 0, 0, 0, W,
            ModRm = (reg, rm),
            ops! {
                #[context(ctx)]
                const mask = !(u64::MAX << (ctx.op_size() * 8));

                let lhs = and(rm, mask);
                let rhs = and(reg, mask);
                let result = add(lhs, rhs);

                // Update the AF based on bit[4]((a & 0xf) + ((b & 0xf)))
                let lhs_nibble = and(lhs, 0xf);
                let rhs_nibble = and(rhs, 0xf);
                let nibble_sum = add(lhs_nibble, rhs_nibble);
                FLAG_AF := select_bit(nibble_sum, 4);

                ..compute_add_of(ctx, ctx.op_size() * 8, result, lhs, rhs);
                ..compute_cf(ctx, ctx.op_size() * 8, result, lhs, rhs);
                ..compute_sf(ctx.op_size() * 8, result);
                ..compute_pf(result);
                ..compute_zf(ctx, ctx.op_size() * 8, result);

                // Store RM in reg, store result in RM
                reg := lhs;
                rm := result;


            }
        },
        encoding! {
            Wide { true },
            Name { "BSWAP" },
            Prefixes,
            #0x0F,
            1, 1, 0, 0, 1, RegBits = reg,
            ops! {
                #[context(ctx)]

                reg := (reg, match ctx.op_size() {
                    2 => UnOp::ByteSwap16,
                    4 => UnOp::ByteSwap32,
                    _ => unreachable!(),
                });

            }
        },
        encoding! {
            Name { "CMPXCHG" },
            Lockable,
            Prefixes, #0x0F,
            1, 0, 1, 1, 0, 0, 0, W,
            ModRm = (reg, rm),
            Acc = a,
            ops! {
                #[context(ctx)]
                let lhs = a;
                let rhs = rm;
                let result = sub(lhs, rhs);

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

                a := rm;
                rm := ite(FLAG_ZF, rm, reg);

            }
        },
        encoding! {
            Name { "CMPXCHG8B" },
            OverrideMemorySize { 8 },
            Lockable,
            Prefixes, #0x0F, #0xC7,
            ModNRm { 1 } = rm,
            ops! {
                #[context(ctx)]
                let lhs = rm;
                let rhs_lower = (GpReg::Ax, DWORD);
                let rhs_upper = shl((GpReg::Dx, DWORD), 32);
                let rhs = or(rhs_lower, rhs_upper);
                let result = sub(lhs, rhs);
                (GpReg::Ax, DWORD) := rm;
                (GpReg::Dx, DWORD) := shr(rm, 32);
                FLAG_ZF := is_zero(result);

                if is_zero(result) {
                    let dest_lower = (GpReg::Bx, DWORD);
                    let dest_upper = shl((GpReg::Cx, DWORD), 32);
                    rm := or(dest_lower, dest_upper);
                }


            }
        },
    ]
}
