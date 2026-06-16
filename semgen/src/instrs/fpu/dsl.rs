#![allow(unused)]

use std::marker::PhantomData;

use liblisa::utils::{bitmask_u64, bitmask_u128};
use sem86_core::arch::intel386::Intel386;
use sem86_core::il::{Cmd, Val};

use crate::context::Context;
use crate::dsl::*;
use crate::ops;

pub struct F32(u32);
pub struct F64(u64);
pub struct F80(u128);

pub trait FloatingPoint {
    type Bits: Copy;

    const FRACTION_SIZE: usize;
    const EXPONENT_SIZE: usize;
    const LEADING_BIT: bool;
    const SIGNIFICAND_SIZE: usize = Self::FRACTION_SIZE + Self::LEADING_BIT as usize;
    const EXPONENT_BIAS: u64 = bitmask_u64(Self::EXPONENT_SIZE as u32 - 1);

    fn make(sign: bool, exponent: i64, significand: u64) -> Self;

    fn bits(self) -> Self::Bits;
}

impl FloatingPoint for F32 {
    type Bits = u32;

    const FRACTION_SIZE: usize = 23;
    const EXPONENT_SIZE: usize = 8;
    const LEADING_BIT: bool = false;

    fn make(_sign: bool, _exponent: i64, _significand: u64) -> Self {
        todo!()
    }

    fn bits(self) -> Self::Bits {
        self.0
    }
}

impl FloatingPoint for F64 {
    type Bits = u64;

    const FRACTION_SIZE: usize = 52;
    const EXPONENT_SIZE: usize = 11;
    const LEADING_BIT: bool = false;

    fn make(_sign: bool, _exponent: i64, _significand: u64) -> Self {
        todo!()
    }

    fn bits(self) -> Self::Bits {
        self.0
    }
}

impl FloatingPoint for F80 {
    type Bits = u128;

    const FRACTION_SIZE: usize = 63;
    const EXPONENT_SIZE: usize = 15;
    const LEADING_BIT: bool = true;

    fn make(sign: bool, exponent: i64, significand: u64) -> Self {
        let biased_exponent = exponent + Self::EXPONENT_BIAS as i64;
        debug_assert!(biased_exponent >= 0);
        F80(((sign as u128) << (Self::SIGNIFICAND_SIZE + Self::FRACTION_SIZE))
            | ((biased_exponent as u128) << Self::SIGNIFICAND_SIZE)
            | significand as u128)
    }

    fn bits(self) -> Self::Bits {
        self.0
    }
}

pub fn exponent<T: FloatingPoint>(val: Val<Intel386>) -> Exponent<T> {
    Exponent(val, false, PhantomData)
}

pub fn raw_exponent<T: FloatingPoint>(val: Val<Intel386>) -> Exponent<T> {
    Exponent(val, true, PhantomData)
}

pub fn significand<T: FloatingPoint>(val: Val<Intel386>) -> Significand<T> {
    Significand(val, PhantomData)
}

pub fn fraction<T: FloatingPoint>(val: Val<Intel386>) -> Fraction<T> {
    Fraction(val, PhantomData)
}

pub fn sign<T: FloatingPoint>(val: Val<Intel386>) -> Sign<T> {
    Sign(val, PhantomData)
}

pub fn abs<T: FloatingPoint>(val: Val<Intel386>) -> Abs<T> {
    Abs(val, PhantomData)
}

pub fn integer_bit<T: FloatingPoint>(val: Val<Intel386>) -> IntegerBit<T> {
    IntegerBit(val, PhantomData)
}

pub fn is_denormal<T: FloatingPoint>(val: Val<Intel386>) -> IsDenormal<T> {
    IsDenormal(val, PhantomData)
}

pub fn is_finite<T: FloatingPoint>(val: Val<Intel386>) -> IsFinite<T> {
    IsFinite(val, PhantomData)
}

pub fn is_infinity<T: FloatingPoint>(val: Val<Intel386>) -> IsInfinity<T> {
    IsInfinity(val, PhantomData)
}

pub fn is_invalid<T: FloatingPoint>(val: Val<Intel386>) -> IsInvalid<T> {
    IsInvalid(val, PhantomData)
}

pub fn is_nan_or_invalid<T: FloatingPoint>(val: Val<Intel386>) -> IsNanOrInvalid<T> {
    IsNanOrInvalid(val, PhantomData)
}

pub fn is_nan<T: FloatingPoint>(val: Val<Intel386>) -> IsNan<T> {
    IsNan(val, PhantomData)
}

pub fn is_quiet_nan<T: FloatingPoint>(val: Val<Intel386>) -> IsQuietNan<T> {
    IsQuietNan(val, PhantomData)
}

pub fn is_signaling_nan<T: FloatingPoint>(val: Val<Intel386>) -> IsSignalingNan<T> {
    IsSignalingNan(val, PhantomData)
}

pub fn fpresult_is_inexact(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<120>(val)
}

pub fn fpresult_is_underflow(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<121>(val)
}

pub fn fpresult_is_overflow(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<122>(val)
}

pub fn fpresult_is_infinite(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<123>(val)
}

pub fn fpresult_is_invalid(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<124>(val)
}

pub fn fpresult_is_rounded_up(val: Val<Intel386>) -> impl LoadIntoVal<Intel386> {
    SelectBit::<125>(val)
}

pub struct Exponent<T>(Val<Intel386>, bool, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for Exponent<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let exp = shr(self.0, T::SIGNIFICAND_SIZE);
            let exp = and(exp, bitmask_u64(T::EXPONENT_SIZE as u32));
            #[if self.1] {
                target := exp;
            } else {
                let biased_exponent = sub(exp, T::EXPONENT_BIAS);
                target := biased_exponent;
            }
        })
    }
}

pub struct Significand<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for Significand<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            target := and(self.0, bitmask_u64(T::SIGNIFICAND_SIZE as u32));
        })
    }
}

pub struct Fraction<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for Fraction<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            target := and(self.0, bitmask_u64(T::FRACTION_SIZE as u32));
        })
    }
}

pub struct IntegerBit<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IntegerBit<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            #[if T::LEADING_BIT] {
                target := select_bit(self.0, T::FRACTION_SIZE as u8);
            } else {
                target := 1;
            }
        })
    }
}

pub struct Sign<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for Sign<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            target := select_bit(self.0, (T::SIGNIFICAND_SIZE + T::EXPONENT_SIZE) as u8);
        })
    }
}

pub struct IsDenormal<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsDenormal<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let exp = raw_exponent::<T>(self.0);
            let significand = significand::<T>(self.0);

            target := ite(exp, significand, 0);
        })
    }
}

pub struct IsFinite<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsFinite<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            // If exponent is not all ones it is finite (or an invalid floating point number).

            #[context(ctx)]
            let exp = raw_exponent::<T>(self.0);
            target := xor(exp, bitmask_u64(T::EXPONENT_SIZE as u32));
        })
    }
}

pub struct IsNanOrInvalid<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsNanOrInvalid<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let is_nan = is_nan::<T>(self.0);
            let is_invalid = is_invalid::<T>(self.0);

            target := or(is_nan, is_invalid);
        })
    }
}

pub struct IsInvalid<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsInvalid<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            // Any value with integer bit 1 is valid (can be normal, inf, or NaN)
            // If integer bit is zero, exponent must also be zero.

            #[context(ctx)]
            let integer_bit = integer_bit::<T>(self.0);
            let exp = raw_exponent::<T>(self.0);
            target := ite(integer_bit, exp, 0);
        })
    }
}

struct SelectBit<const N: usize>(Val<Intel386>);

impl<const N: usize> LoadIntoVal<Intel386> for SelectBit<N> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            target := select_bit(self.0, N as u8);
        })
    }
}

pub struct IsNan<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsNan<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let exp = raw_exponent::<T>(self.0);
            let exp_diff = xor(exp, 0x7fff);

            let fraction = fraction::<T>(self.0);
            // If fraction is zero, we might have infinity but not NaN.
            // If fraction is non-zero, we need to check if exponent matches 0x7fff.
            let is_not_nan = ite(fraction, 1, exp_diff);

            target := is_zero(is_not_nan);
        })
    }
}

pub struct IsQuietNan<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsQuietNan<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let is_nan = IsNan::<T>(self.0, PhantomData);
            let signaling = select_bit(self.0, T::FRACTION_SIZE as u8 - 1);

            // signaling iff the top fraction bit is 0.
            target := ite(signaling, 0, is_nan);
        })
    }
}

pub struct IsSignalingNan<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsSignalingNan<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let is_nan = IsNan::<T>(self.0, PhantomData);
            let signaling = select_bit(self.0, T::FRACTION_SIZE as u8 - 1);

            // signaling iff the top fraction bit is 0.
            target := ite(signaling, is_nan, 0);
        })
    }
}

pub struct Abs<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for Abs<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let sign_bit = shl(1, T::SIGNIFICAND_SIZE + T::EXPONENT_SIZE);
            let mask = sub(sign_bit, 1);
            let masked_val = and(self.0, mask);
            target := is_zero(masked_val);
        })
    }
}

pub struct IsInfinity<T>(Val<Intel386>, PhantomData<T>);

impl<T: FloatingPoint> LoadIntoVal<Intel386> for IsInfinity<T> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let abs = abs::<T>(self.0);
            let infinity_marker = U128(bitmask_u128(T::EXPONENT_SIZE as u32 + T::LEADING_BIT as u32) << T::FRACTION_SIZE);
            let diff = xor(abs, infinity_marker);
            target := is_zero(diff);
        })
    }
}
