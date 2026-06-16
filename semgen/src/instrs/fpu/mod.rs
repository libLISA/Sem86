use liblisa::encoding::UnsizedParLoc;
use liblisa::state::Size;
use liblisa::utils::bitmask_u128;
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::{
    FLAG_X87_CC1, FLAG_X87_DENORMALIZED_OPERAND, FLAG_X87_INVALID_OPERATION, FLAG_X87_MASKED_INVALID_OPERATION,
    FLAG_X87_OVERFLOW, FLAG_X87_PRECISION, FLAG_X87_STACK_FAULT, FLAG_X87_UNDERFLOW, GpReg, Intel386, Reg, X87Reg,
};
use sem86_core::il::{Cmd, Commands, FpBinOp, FpUnOp, Op, UnOp, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Context};
use crate::dsl::*;
use crate::instrs::fpu::dsl::{
    F80, FloatingPoint, exponent, fpresult_is_inexact, fpresult_is_overflow, fpresult_is_rounded_up, fpresult_is_underflow,
    is_denormal, is_infinity, is_nan, sign,
};
use crate::instrs::{EffectiveAddress, WORD};
use crate::{Config, encoding, encoding_group, ops};

mod cmp;
mod dsl;
mod env;
mod mov;

const FP80_SIZE: Size = Size::new(0, 9);

fn check_available(ctx: &mut Context, f: impl FnOnce(&mut Context) -> Vec<Cmd<Intel386>>) -> SemSpec<Intel386> {
    let mut commands = Commands::Ops(f(ctx));
    commands.wrap_memory_accesses(0..ctx.num_accesses());

    SemSpec {
        commands: vec![
            Cmd::store(
                Val::Temp(0),
                Op::UnOp {
                    arg: Val::from((GpReg::Cr0, WORD)),
                    op: UnOp::SelectBit(3),
                },
            ),
            Cmd::If {
                val: Val::Temp(0),
                if_zero: commands,
                if_nonzero: Commands::Ops(vec![Cmd::Exception {
                    exception: Exception::DeviceNotAvailable,
                    code: Val::const_val(0),
                }]),
            },
        ],
        manual_memory_accesses: true,
        ..Default::default()
    }
}

pub struct CheckExceptionFlags(Val<Intel386>);

impl AppendToOpVec<Intel386> for CheckExceptionFlags {
    fn append_to_op_vec(self, ctx: &mut Context, vec: &mut Vec<Cmd<Intel386>>) {
        vec.extend(ops! {
            #[context(ctx)]
            let is_inexact = fpresult_is_inexact(self.0);
            if !is_zero(is_inexact) {
                FLAG_X87_PRECISION := 1;
            }

            let is_overflow = fpresult_is_overflow(self.0);
            if !is_zero(is_overflow) {
                FLAG_X87_OVERFLOW := 1;
            }

            let is_underflow = fpresult_is_underflow(self.0);
            if !is_zero(is_underflow) {
                FLAG_X87_UNDERFLOW := 1;
            }

            let is_rounded_up = fpresult_is_rounded_up(self.0);
            if !is_zero(is_rounded_up) {
                (X87Reg::ConditionCodes, Size::one_byte(1)) := 1;
            }
        })
    }
}

pub struct ApplyPrecisionControl(Val<Intel386>);

impl LoadIntoVal<Intel386> for ApplyPrecisionControl {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let pc = X87Reg::PrecisionControl;
            if is_zero(pc) {
                target := Op::FpUnOp {
                    arg: self.0,
                    rc: X87Reg::RoundingControl.into(),
                    op: FpUnOp::RoundF80ToF32,
                };
            } else {
                let double = cmp_eq(pc, 2);
                if !is_zero(double) {
                    target := Op::FpUnOp {
                        arg: self.0,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::RoundF80ToF64,
                    };
                } else {
                    target := self.0;
                }
            }
        })
    }
}

// TODO: Eagerly read/write mm registers so we don't generate a huge spaghetti of loads and moves
pub struct UncheckedDynMmx<T>(T);

impl<T: Into<Val<Intel386>>> LoadIntoVal<Intel386> for UncheckedDynMmx<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let index = self.0.into();
            let cropped_index = and(index, 7);
            ..Cmd::LoadDynamicReg {
                regs: vec![
                    Some(Reg::X87(X87Reg::Mm(0))),
                    Some(Reg::X87(X87Reg::Mm(1))),
                    Some(Reg::X87(X87Reg::Mm(2))),
                    Some(Reg::X87(X87Reg::Mm(3))),
                    Some(Reg::X87(X87Reg::Mm(4))),
                    Some(Reg::X87(X87Reg::Mm(5))),
                    Some(Reg::X87(X87Reg::Mm(6))),
                    Some(Reg::X87(X87Reg::Mm(7))),
                ],
                index: cropped_index,
                into: target,
                size: FP80_SIZE,
            };
        })
    }
}

impl<T: Into<Val<Intel386>>> StoreInto<Intel386> for UncheckedDynMmx<T> {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let val = val;
            let index = self.0.into();
            let cropped_index = and(index, 7);

            ..Cmd::StoreDynamicReg {
                regs: vec![
                    Some(Reg::X87(X87Reg::Mm(0))),
                    Some(Reg::X87(X87Reg::Mm(1))),
                    Some(Reg::X87(X87Reg::Mm(2))),
                    Some(Reg::X87(X87Reg::Mm(3))),
                    Some(Reg::X87(X87Reg::Mm(4))),
                    Some(Reg::X87(X87Reg::Mm(5))),
                    Some(Reg::X87(X87Reg::Mm(6))),
                    Some(Reg::X87(X87Reg::Mm(7))),
                ],
                index: cropped_index,
                value: val,
                size: FP80_SIZE,
            };
        })
    }
}

pub struct DynMmx(Val<Intel386>);

impl LoadIntoVal<Intel386> for DynMmx {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let index = self.0;
            let cropped_index = and(index, 7);

            let n = mul(cropped_index, 8);
            let tag = shr(X87Reg::MmIsValid, n);
            let tag = and(tag, 1);

            if is_zero(tag) {
                FLAG_X87_CC1 := 0;
                FLAG_X87_INVALID_OPERATION := 1;
                FLAG_X87_STACK_FAULT := 1;

                if is_zero(FLAG_X87_MASKED_INVALID_OPERATION) {
                    ..Cmd::Exception {
                        exception: Exception::FloatingPointException,
                        code: 0.into(),
                    };
                } else {
                    target := F80_NAN;
                }
            } else {
                target := UncheckedDynMmx(self.0);
            }
        })
    }
}

impl StoreInto<Intel386> for DynMmx {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            UncheckedDynMmx(self.0) := val;
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct St(Val<Intel386>);

impl LoadIntoVal<Intel386> for St {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let index = add(X87Reg::Top, self.0);
            target := DynMmx(index);
        });
    }
}

impl StoreInto<Intel386> for St {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let index = add(X87Reg::Top, self.0);
            let cropped_index = and(index, 7);
            DynMmx(cropped_index) := val;

            let n = mul(cropped_index, 8);
            let bit = shl(1, n);
            X87Reg::MmIsValid := or(X87Reg::MmIsValid, bit);
        });
    }
}

/// St or Val.
#[derive(Copy, Clone, Debug)]
pub enum Sov {
    St(St),
    Val(Val<Intel386>),
}

pub const ST0: Sov = Sov::st(0);
pub const ST1: Sov = Sov::st(1);
pub const F80_NAN: U128 = U128(0xFFFFC000000000000000);
pub const F80_ONE: U128 = U128(0x3FFF8000000000000000);

impl Sov {
    const fn st(n: usize) -> Self {
        Sov::St(St(Val::const_val(n as u64)))
    }
}

impl StoreInto<Intel386> for Sov {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        match self {
            Sov::St(st) => st.store_into(ctx, val, output),
            Sov::Val(val) => val.store_into(ctx, val, output),
        }
    }
}

impl LoadIntoVal<Intel386> for Sov {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        match self {
            Sov::St(st) => st.load_into(ctx, target, output),
            Sov::Val(val) => val.load_into(ctx, target, output),
        }
    }
}

/// Reads from/writes to the FP stack, automatically incrementing or decrementing TOP.
struct FpStack;

impl StoreInto<Intel386> for FpStack {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let index = sub(X87Reg::Top, 1);
            let cropped_index = and(index, 7);

            let n = mul(cropped_index, 8);
            let tag = shr(X87Reg::MmIsValid, n);
            let tag = and(tag, 1);

            if is_zero(tag) {
                FLAG_X87_CC1 := 0;
                let val = val;

                DynMmx(cropped_index) := val;
                X87Reg::Top := cropped_index;

                let bit = shl(1, n);
                X87Reg::MmIsValid := or(X87Reg::MmIsValid, bit);
            } else {
                FLAG_X87_CC1 := 1;
                FLAG_X87_INVALID_OPERATION := 1;
                FLAG_X87_STACK_FAULT := 1;
                if is_zero(FLAG_X87_MASKED_INVALID_OPERATION) {
                    ..Cmd::Exception {
                        exception: Exception::FloatingPointException,
                        code: 0.into(),
                    };
                } else {
                    let val = F80_NAN;
                    DynMmx(cropped_index) := val;
                    X87Reg::Top := cropped_index;

                    let bit = shl(1, n);
                    X87Reg::MmIsValid := or(X87Reg::MmIsValid, bit);
                }
            }
        });
    }
}

impl LoadIntoVal<Intel386> for FpStack {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            target := DynMmx(X87Reg::Top.into());

            let n = mul(X87Reg::Top, 8);
            let bit = shl(1, n);
            let mask = xor(bit, u64::MAX);
            X87Reg::MmIsValid := and(X87Reg::MmIsValid, mask);

            let index = add(X87Reg::Top, 1);
            X87Reg::Top := and(index, 7);
        });
    }
}

impl AppendToOpVec<Intel386> for FpStack {
    fn append_to_op_vec(self, ctx: &mut Context, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]

            let n = mul(X87Reg::Top, 8);
            let bit = shl(1, n);
            let mask = xor(bit, u64::MAX);
            X87Reg::MmIsValid := and(X87Reg::MmIsValid, mask);

            let top = add(X87Reg::Top, 1);
            X87Reg::Top := and(top, 7);
        });
    }
}

pub fn fpstack_pop() -> impl LoadIntoVal<Intel386> + AppendToOpVec<Intel386> {
    FpStack
}
pub fn fpstack_push() -> impl StoreInto<Intel386> {
    FpStack
}

pub struct UpdateFpPointers;

impl AppendToOpVec<Intel386> for UpdateFpPointers {
    fn append_to_op_vec(self, ctx: &mut Context, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            X87Reg::InstructionPointer := GpReg::Ip;
            X87Reg::InstructionSelector := GpReg::Cs;
        });

        // TODO: Write last opcode register with 11 bits from: unprefixed_instr_byte0[0:2] ++ unprefixed_instr_byte1[0:8]

        if ctx.num_accesses() > 0 {
            assert_eq!(ctx.num_accesses(), 1);
            let a = ctx.access_mut(0).clone();
            let seg = a.inputs[0];
            if let UnsizedParLoc::Reg(r) = seg.loc {
                let Reg::Gp(sreg) = r else { panic!() };
                let sreg = match sreg {
                    GpReg::EsBase => GpReg::Es,
                    GpReg::CsBase => GpReg::Cs,
                    GpReg::DsBase => GpReg::Ds,
                    GpReg::SsBase => GpReg::Ss,
                    GpReg::FsBase => GpReg::Fs,
                    GpReg::GsBase => GpReg::Gs,
                    _ => unreachable!(),
                };

                output.extend(ops! {
                    #[context(ctx)]
                    X87Reg::DataSelector := sreg;
                })
            } else {
                // TODO: Handle segment overrides
                log::warn!("TODO: Segment override as part");
            }

            let ea = EffectiveAddress(&a);
            ea.load_into(ctx, X87Reg::DataPointer.into(), output);
        }
    }
}

pub struct F80IsZero(Val<Intel386>);

impl LoadIntoVal<Intel386> for F80IsZero {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let sign_bit = shl(1, 79);
            let mask = sub(sign_bit, 1);
            let masked_val = and(self.0, mask);
            target := is_zero(masked_val);
        })
    }
}

pub struct F80IsInfinity(Val<Intel386>);

impl LoadIntoVal<Intel386> for F80IsInfinity {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let sign_bit = shl(1, 79);
            let mask = sub(sign_bit, 1);
            let masked_val = and(self.0, mask);
            let infinity_marker = U128(0x7FFF8000000000000000);
            target := cmp_eq(masked_val, infinity_marker);
        })
    }
}

pub struct F80IsNan(Val<Intel386>);

impl LoadIntoVal<Intel386> for F80IsNan {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let exp = shr(self.0, 64);
            let exp = and(exp, 0x7fff);
            // TODO: Use cmp_eq instead
            let exp_diff = xor(exp, 0x7fff);

            let mantissa = and(self.0, 0x7fff_ffff_ffff_ffffu64);
            // If mantissa is zero, we might have infinity but not NaN.
            // If mantissa is non-zero, we need to check if exponent matches 0x7fff.
            let is_not_nan = ite(mantissa, 1, exp_diff);

            target := is_zero(is_not_nan);
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Format {
    Float32,
    Float64,
    Float80,
    Int16,
    Int32,
    Int64,
}

impl Format {
    pub fn byte_size(&self) -> usize {
        match self {
            Format::Float32 => 4,
            Format::Float64 => 8,
            Format::Float80 => 10,
            Format::Int16 => 2,
            Format::Int32 => 4,
            Format::Int64 => 8,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct FormatBits;

impl Default for FormatBits {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatBits {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for FormatBits {
    type Output = Format;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBits::new(3).build(ctx, &mut |mut ctx, val| {
            let format = match val {
                0b100 => Format::Float32,
                0b110 => Format::Float64,
                0b101 => Format::Int32,
                0b111 => Format::Int16,
                _ => return,
            };

            ctx.override_mem_size(format.byte_size());
            next(ctx, format)
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct StBits;

impl Default for StBits {
    fn default() -> Self {
        Self::new()
    }
}

impl StBits {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for StBits {
    type Output = Sov;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Imm::new(3).build(ctx, &mut |ctx, val| next(ctx, Sov::St(St(val))))
    }
}

pub struct CastFloat {
    pub from: Format,
    pub to: Format,
    pub val: Val<Intel386>,
}

impl LoadIntoVal<Intel386> for CastFloat {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]

            #[match (self.from, self.to)] {
                (Format::Float32, Format::Float32)
                    | (Format::Float64, Format::Float64)
                    | (Format::Float80, Format::Float80)
                    | (Format::Int16, Format::Int16)
                    | (Format::Int32, Format::Int32) => {
                    target := self.val;
                }
                (Format::Float32, Format::Float80) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F32ToF80
                    };
                }
                (Format::Float64, Format::Float80) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F64ToF80
                    };
                }
                (Format::Float80, Format::Float32) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F80ToF32
                    };
                }
                (Format::Float80, Format::Float64) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F80ToF64
                    };
                }
                (Format::Int16, Format::Float80) => {
                    let val = sign_extend(self.val, 16);
                    target := Op::FpUnOp {
                        arg: val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::I64ToF80
                    };
                }
                (Format::Int32, Format::Float80) => {
                    let val = sign_extend(self.val, 32);
                    target := Op::FpUnOp {
                        arg: val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::I64ToF80
                    };
                }
                (Format::Int64, Format::Float80) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::I64ToF80
                    };
                }
                (Format::Float80, Format::Int16) => {
                    // TODO
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F80ToI64
                    };
                }
                (Format::Float80, Format::Int32) => {
                    // TODO
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F80ToI64
                    };
                }
                (Format::Float80, Format::Int64) => {
                    target := Op::FpUnOp {
                        arg: self.val,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::F80ToI64
                    };
                }

                // TODO: Other conversions

                (from, to) => {
                    ..Cmd::Log { message: format!("TODO: convert from {from:?} to {to:?}") };
                    target := 0;
                }
            }
        });
    }
}

pub fn builder(config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        Box::new(mov::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(env::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(cmp::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        encoding_group! {
            [
                Name { "FCHS" },
                Prefixes, #0xD9, #0xE0,
            ] = "fchs",
            [
                Name { "FABS" },
                Prefixes, #0xD9, #0xE1,
            ] = "fabs",
            [
                Name { "F2XM1" },
                Prefixes, #0xD9, #0xF0,
            ] = "f2xm1",
            [
                Name { "FSQRT" },
                Prefixes, #0xD9, #0xFA,
            ] = "fsqrt",
            [
                Name { "FSIN" },
                Prefixes, #0xD9, #0xFE,
            ] = "fsin",
            [
                Name { "FCOS" },
                Prefixes, #0xD9, #0xFF,
            ] = "fcos",
            [
                Name { "FRNDINT" },
                Prefixes, #0xD9, #0xFC,
            ] = "frndint",
            map |op| {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops!{
                    #[context(ctx)]
                    ..UpdateFpPointers;
                    #[match op] {
                        "fchs" => {
                            let sign_bit = shl(1, 79);
                            let current = ST0;
                            ST0 := xor(current, sign_bit);
                        }
                        "fabs" => {
                            let sign_bit = shl(1, 79);
                            let exp_significand_mask = sub(sign_bit, 1);
                            let current = ST0;
                            ST0 := and(current, exp_significand_mask);
                        }
                        "frndint" => {
                            let current = ST0;
                            let result = Op::FpUnOp {
                                arg: current,
                                rc: X87Reg::RoundingControl.into(),
                                op: FpUnOp::RoundToIntF80,
                            };
                            ..CheckExceptionFlags(result);
                            ST0 := result;
                        }
                        "fsqrt" => {
                            let current = ST0;
                            let result = Op::FpUnOp {
                                arg: current,
                                rc: X87Reg::RoundingControl.into(),
                                op: FpUnOp::SqrtF80,
                            };
                            let result = ApplyPrecisionControl(result);
                            ..CheckExceptionFlags(result);

                            ST0 := result;
                        }
                        "fsin" => {
                            let current = ST0;
                            let result = Op::FpUnOp {
                                arg: current,
                                rc: X87Reg::RoundingControl.into(),
                                op: FpUnOp::SinF80,
                            };
                            ..CheckExceptionFlags(result);

                            ST0 := result;
                        }
                        "fcos" => {
                            let current = ST0;
                            let result = Op::FpUnOp {
                                arg: current,
                                rc: X87Reg::RoundingControl.into(),
                                op: FpUnOp::CosF80,
                            };
                            ..CheckExceptionFlags(result);

                            ST0 := result;
                        }
                        "f2xm1" => {
                            let current = ST0;
                            let result = Op::FpUnOp {
                                arg: current,
                                rc: X87Reg::RoundingControl.into(),
                                op: FpUnOp::F2Xm1F80,
                            };
                            ..CheckExceptionFlags(result);

                            ST0 := result;
                        }
                        _ => {
                            #[match unreachable!()] { }
                        }
                    }


                }))
            }
        },
        encoding_group! {
            [
                Name { "FADD" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 0 } = rm,
            ] = (ST0, Sov::Val(rm), false, format),
            [
                Name { "FADD_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 0, 0, 0, StBits = src,
            ] = (ST0, src, false, Format::Float80),
            [
                Name { "FADD_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 0, 0, 0, StBits = dst,
            ] = (dst, ST0, false, Format::Float80),
            [
                Name { "FADDP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 0, 0, 0, StBits = dst,
            ] = (dst, ST0, true, Format::Float80),
            map |(dst, src, do_pop, format): (Sov, Sov, bool, Format)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let lhs = dst;
                let rhs = src;
                let rhs = CastFloat {
                    from: format,
                    to: Format::Float80,
                    val: rhs,
                };

                let lhs_denormal = dsl::is_denormal::<F80>(lhs);
                let rhs_denormal = dsl::is_denormal::<F80>(rhs);
                let any_denormal = or(lhs_denormal, rhs_denormal);
                if !is_zero(any_denormal) {
                    FLAG_X87_DENORMALIZED_OPERAND := 1;
                }

                let lhs_is_infinity = F80IsInfinity(lhs);
                let rhs_is_infinity = F80IsInfinity(rhs);
                let both_infinity = and(lhs_is_infinity, rhs_is_infinity);

                let lhs_sign = dsl::sign::<F80>(lhs);
                let rhs_sign = dsl::sign::<F80>(rhs);
                // TODO: Use cmp_eq instead
                let sign_cmp = xor(lhs_sign, rhs_sign);

                let infinities_with_different_signs = ite(both_infinity, 0, sign_cmp);
                // When trying to add infinities with different signs, an exception should be generated.
                let result;
                if !is_zero(infinities_with_different_signs) {
                    FLAG_X87_INVALID_OPERATION := 1;
                    FLAG_X87_STACK_FAULT := 0;
                    result := F80_NAN;
                } else {
                    result := f80_add(lhs, rhs, X87Reg::RoundingControl);
                }

                let result = ApplyPrecisionControl(result);

                ..CheckExceptionFlags(result);

                dst := result;

                #[if do_pop] {
                    ..fpstack_pop();
                }


            }))
        },
        encoding_group! {
            [
                Name { "FSUB" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 4 } = rm,
            ] = (ST0, ST0, Sov::Val(rm), Format::Float80, format, false),
            [
                Name { "FSUBR" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 5 } = rm,
            ] = (ST0, Sov::Val(rm), ST0, format, Format::Float80, false),
            [
                Name { "FSUB_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 1, 0, 0, StBits = src,
            ] = (ST0, ST0, src, Format::Float80, Format::Float80, false),
            [
                Name { "FSUBR_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 1, 0, 1, StBits = dst,
            ] = (ST0, dst, ST0, Format::Float80, Format::Float80, false),
            [
                Name { "FSUB_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 1, 0, 1, StBits = dst,
            ] = (dst, dst, ST0, Format::Float80, Format::Float80, false),
            [
                Name { "FSUBR_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 1, 0, 0, StBits = src,
            ] = (src, ST0, src, Format::Float80, Format::Float80, false),
            [
                Name { "FSUBP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 1, 0, 1, StBits = dst,
            ] = (dst, dst, ST0, Format::Float80, Format::Float80, true),
            [
                Name { "FSUBRP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 1, 0, 0, StBits = src,
            ] = (src, ST0, src, Format::Float80, Format::Float80, true),
            map |(dst, lhs, rhs, lhs_format, rhs_format, do_pop)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let lhs = lhs;
                let rhs = rhs;
                let lhs = CastFloat {
                    from: lhs_format,
                    to: Format::Float80,
                    val: lhs,
                };
                let rhs = CastFloat {
                    from: rhs_format,
                    to: Format::Float80,
                    val: rhs,
                };

                let lhs_denormal = dsl::is_denormal::<F80>(lhs);
                let rhs_denormal = dsl::is_denormal::<F80>(rhs);
                let any_denormal = or(lhs_denormal, rhs_denormal);
                if !is_zero(any_denormal) {
                    FLAG_X87_DENORMALIZED_OPERAND := 1;
                }

                let lhs_is_infinity = F80IsInfinity(lhs);
                let rhs_is_infinity = F80IsInfinity(rhs);
                let both_infinity = and(lhs_is_infinity, rhs_is_infinity);

                let lhs_sign = dsl::sign::<F80>(lhs);
                let rhs_sign = dsl::sign::<F80>(rhs);
                let rhs_sign = xor(1, rhs_sign); // Negate sign because we are subtracting
                // TODO: Use cmp_eq instead
                let sign_cmp = xor(lhs_sign, rhs_sign);

                let infinities_with_different_signs = ite(both_infinity, 0, sign_cmp);
                // When trying to add infinities with different signs, an exception should be generated.
                let result;
                if !is_zero(infinities_with_different_signs) {
                    FLAG_X87_INVALID_OPERATION := 1;
                    FLAG_X87_STACK_FAULT := 0;
                    result := F80_NAN;
                } else {
                    result := f80_sub(lhs, rhs, X87Reg::RoundingControl);
                }

                let result = ApplyPrecisionControl(result);

                ..CheckExceptionFlags(result);
                dst := result;

                #[if do_pop] {
                    ..fpstack_pop();
                }


            }))
        },
        encoding_group! {
            [
                // st0 = st0 / rm
                Name { "FDIV" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 6 } = rm,
            ] = (ST0, ST0, Sov::Val(rm), Format::Float80, format, false),
            [
                // st0 = rm / st0
                Name { "FDIVR" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 7 } = rm,
            ] = (ST0, Sov::Val(rm), ST0, format, Format::Float80, false),
            [
                Name { "FDIV_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 1, 1, 0, StBits = src,
            ] = (ST0, ST0, src, Format::Float80, Format::Float80, false),
            [
                Name { "FDIVR_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 1, 1, 1, StBits = src,
            ] = (ST0, src, ST0, Format::Float80, Format::Float80, false),
            [
                Name { "FDIV_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 1, 1, 1, StBits = dst,
            ] = (dst, dst, ST0, Format::Float80, Format::Float80, false),
            [
                Name { "FDIVR_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 1, 1, 0, StBits = dst,
            ] = (dst, ST0, dst, Format::Float80, Format::Float80, false),
            [
                Name { "FDIVP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 1, 1, 1, StBits = dst,
            ] = (dst, dst, ST0, Format::Float80, Format::Float80, true),
            [
                Name { "FDIVRP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 1, 1, 0, StBits = dst,
            ] = (dst, ST0, dst, Format::Float80, Format::Float80, true),
            map |(dst, lhs, rhs, lhs_format, rhs_format, do_pop)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let lhs = lhs;
                let rhs = rhs;
                let lhs = CastFloat {
                    from: lhs_format,
                    to: Format::Float80,
                    val: lhs,
                };
                let rhs = CastFloat {
                    from: rhs_format,
                    to: Format::Float80,
                    val: rhs,
                };

                let result = f80_div(lhs, rhs, X87Reg::RoundingControl);
                let result = ApplyPrecisionControl(result);

                ..CheckExceptionFlags(result);

                dst := result;

                #[if do_pop] {
                    ..fpstack_pop();
                }


            }))
        },
        encoding_group! {
            [
                Name { "FMUL" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 1 } = rm,
            ] = (ST0, Sov::Val(rm), format, false),
            [
                Name { "FMUL_st0_sti" },
                Prefixes, #0xD8,
                1, 1, 0, 0, 1, StBits = src,
            ] = (ST0, src, Format::Float80, false),
            [
                Name { "FMUL_sti_st0" },
                Prefixes, #0xDC,
                1, 1, 0, 0, 1, StBits = dst,
            ] = (dst, ST0, Format::Float80, false),
            [
                Name { "FMULP_sti_st0" },
                Prefixes, #0xDE,
                1, 1, 0, 0, 1, StBits = dst,
            ] = (dst, ST0, Format::Float80, true),
            map |(dst, src, format, do_pop)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let lhs = dst;
                let rhs = src;
                let rhs = CastFloat {
                    from: format,
                    to: Format::Float80,
                    val: rhs,
                };

                let result = f80_mul(lhs, rhs, X87Reg::RoundingControl);
                let result = ApplyPrecisionControl(result);

                ..CheckExceptionFlags(result);

                dst := result;

                #[if do_pop] {
                    ..fpstack_pop();
                }


            }))
        },
        encoding! {
            Name { "FPREM" },
            Prefixes, #0xD9, #0xF8,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let lhs = ST0;
                    let rhs = ST1;

                    // Compute Q := Int(TruncateTowardsZero(ST(0) / ST(1)));
                    let float_quotient = f80_div(lhs, rhs, X87Reg::RoundingControl);
                    let rounded_float_quotient = Op::FpUnOp {
                        arg: float_quotient,
                        rc: 0b11.into(), // Round to zero
                        op: FpUnOp::RoundToIntF80,
                    };
                    let int_quotient = Op::FpUnOp {
                        arg: rounded_float_quotient,
                        rc: 0b11.into(), // Round to zero
                        op: FpUnOp::F80ToI64,
                    };

                    // Compute result via ST0 - ST1 * Q
                    let num_to_subtract = f80_mul(rhs, rounded_float_quotient, X87Reg::RoundingControl);
                    let result = f80_sub(lhs, num_to_subtract, X87Reg::RoundingControl);

                    ..CheckExceptionFlags(result);

                    // TODO: This instruction can only do 63 subtractions per iteration. If result > rhs, C2 should be 1 to indicate that another FPREM is needed.
                    // Assign lowest 3 bits of quotient to condition codes: C0, C3, C1 := Q[2], Q[1], Q[0].
                    (X87Reg::ConditionCodes, Size::one_byte(1)) := select_bit(int_quotient, 0);
                    (X87Reg::ConditionCodes, Size::one_byte(3)) := select_bit(int_quotient, 1);
                    (X87Reg::ConditionCodes, Size::one_byte(0)) := select_bit(int_quotient, 2);
                    (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;

                    ST0 := result;

                }))
            }
        },
        encoding! {
            Name { "FPREM1" },
            Prefixes, #0xD9, #0xF5,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let lhs = ST0;
                    let rhs = ST1;
                    let result = f80_rem(lhs, rhs, X87Reg::RoundingControl);

                    ..CheckExceptionFlags(result);

                    // TODO: This instruction can only do 63 subtractions per iteration. If result > rhs, C2 should be 1 to indicate that another FPREM is needed.
                    (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;

                    ST0 := result;

                }))
            }
        },
        encoding_group! {
            [
                Name { "FNOP" },
                Prefixes, #0xD9, #0xD0,
            ] = (),
            [
                Name { "FDISI8087_NOP" },
                Prefixes, #0xDB, #0xE1,
            ] = (),
            [
                Name { "FENI8087_NOP" },
                Prefixes, #0xDB, #0xE0,
            ] = (),
            [
                Name { "FSETPM287_NOP" },
                Prefixes, #0xDB, #0xE4,
            ] = (),
            map |()| BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                #[context(ctx)]

                ..UpdateFpPointers;

            }))
        },
        encoding! {
            Name { "FPTAN" },
            Prefixes, #0xD9, #0xF2,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let current = ST0;
                    let result = Op::FpUnOp {
                        arg: current,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::TanF80,
                    };
                    let result = ApplyPrecisionControl(result);
                    ..CheckExceptionFlags(result);

                    ST0 := result;
                    (fpstack_push()) := F80_ONE;

                    // TODO: Check valid range, set to 1 if outside range (and don't write st0/push 1.0)
                    (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;

                }))
            }
        },
        encoding! {
            Name { "FSINCOS" },
            Prefixes, #0xD9, #0xFB,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let current = ST0;
                    let sin = Op::FpUnOp {
                        arg: current,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::SinF80,
                    };
                    let cos = Op::FpUnOp {
                        arg: current,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::CosF80,
                    };
                    ..CheckExceptionFlags(sin);
                    ..CheckExceptionFlags(cos);

                    ST0 := sin;
                    (fpstack_push()) := cos;


                }))
            }
        },
        encoding! {
            Name { "FSCALE" },
            Prefixes, #0xD9, #0xFD,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let dst = ST0;
                    let src = ST1;
                    let result = Op::FpBinOp {
                        args: [ dst, src ],
                        rc: X87Reg::RoundingControl.into(),
                        op: FpBinOp::F80Scale,
                    };

                    ..CheckExceptionFlags(result);

                    ST0 := result;

                }))
            }
        },
        encoding! {
            Name { "FXTRACT" },
            Prefixes, #0xD9, #0xF4,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let val = ST0;

                    let exponent = exponent::<F80>(val);
                    let exponent_f80 = CastFloat {
                        from: Format::Int64,
                        to: Format::Float80,
                        val: exponent,
                    };

                    // Zero out exponent, then set to 0x3fff (biased value for exponent of 0)
                    let mask = U128(!(bitmask_u128(F80::EXPONENT_SIZE as u32) << F80::SIGNIFICAND_SIZE));
                    let masked_val = and(val, mask);
                    let new_exponent = U128(0x3fff << F80::SIGNIFICAND_SIZE);
                    let significand = or(masked_val, new_exponent);

                    ST0 := exponent_f80;
                    (fpstack_push()) := significand;


                }))
            }
        },
        encoding! {
            Name { "FPATAN" },
            Prefixes, #0xD9, #0xF3,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let rhs = fpstack_pop();
                    let lhs = ST0;
                    let ratio = f80_div(lhs, rhs, X87Reg::RoundingControl);
                    let result = Op::FpUnOp {
                        arg: ratio,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::ArcTanF80,
                    };
                    ..CheckExceptionFlags(result);

                    ST0 := result;


                }))
            }
        },
        encoding! {
            Name { "FXAM" },
            Prefixes, #0xD9, #0xE5,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let n = mul(X87Reg::Top, 8);
                    let bit = shl(1, n);
                    let is_valid = and(X87Reg::MmIsValid, bit);

                    let val = ST0;
                    (X87Reg::ConditionCodes, Size::one_byte(1)) := sign::<F80>(val);

                    if is_zero(is_valid) {
                        (X87Reg::ConditionCodes, Size::one_byte(3)) := 1;
                        (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;
                        (X87Reg::ConditionCodes, Size::one_byte(0)) := 1;
                    } else {
                        let nan = is_nan::<F80>(val);
                        if !is_zero(nan) {
                            (X87Reg::ConditionCodes, Size::one_byte(3)) := 0;
                            (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;
                            (X87Reg::ConditionCodes, Size::one_byte(0)) := 1;
                        } else {
                            let infinity = is_infinity::<F80>(val);
                            if !is_zero(infinity) {
                                (X87Reg::ConditionCodes, Size::one_byte(3)) := 0;
                                (X87Reg::ConditionCodes, Size::one_byte(2)) := 1;
                                (X87Reg::ConditionCodes, Size::one_byte(0)) := 1;
                            } else {
                                let sign_bit = shl(1, 79);
                                let exp_significand_mask = sub(sign_bit, 1);
                                let abs = and(val, exp_significand_mask);
                                if is_zero(abs) {
                                    (X87Reg::ConditionCodes, Size::one_byte(3)) := 1;
                                    (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;
                                    (X87Reg::ConditionCodes, Size::one_byte(0)) := 0;
                                } else {
                                    let denormal = is_denormal::<F80>(val);
                                    // TODO: This is somehow true for 0x3FFFA331B30803831000
                                    if !is_zero(denormal) {
                                        (X87Reg::ConditionCodes, Size::one_byte(3)) := 1;
                                        (X87Reg::ConditionCodes, Size::one_byte(2)) := 1;
                                        (X87Reg::ConditionCodes, Size::one_byte(0)) := 0;
                                    } else {
                                        (X87Reg::ConditionCodes, Size::one_byte(3)) := 0;
                                        (X87Reg::ConditionCodes, Size::one_byte(2)) := 1;
                                        (X87Reg::ConditionCodes, Size::one_byte(0)) := 0;
                                    }
                                }
                            }
                        }
                    }


                }))
            }
        },
        encoding! {
            Name { "FXCH" },
            Prefixes, #0xD9,
            1, 1, 0, 0, 1, StBits = reg,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let tmp = St(0.into());
                    ST0 := reg;
                    reg := tmp;
                    (X87Reg::ConditionCodes, Size::one_byte(1)) := 0;


                }))
            }
        },
        encoding! {
            Name { "FYL2X" },
            Prefixes, #0xD9, #0xF1,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let x = fpstack_pop();
                    let y = ST0;

                    let log2x = Op::FpUnOp {
                        arg: x,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::Log2F80,
                    };

                    let result = f80_mul(y, log2x, X87Reg::RoundingControl);
                    ST0 := result;


                }))
            }
        },
        encoding! {
            Name { "FYL2XP1" },
            Prefixes, #0xD9, #0xF9,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let x = fpstack_pop();
                    let y = ST0;

                    let log2x = Op::FpUnOp {
                        arg: x,
                        rc: X87Reg::RoundingControl.into(),
                        op: FpUnOp::Log2F80,
                    };

                    let result = f80_mul(y, log2x, X87Reg::RoundingControl);
                    let one = F80_ONE;
                    let result = f80_add(result, one, X87Reg::RoundingControl);
                    ST0 := result;


                }))
            }
        },
    ]
}
