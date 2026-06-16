use std::ops::{Add, Div, Mul, Rem, Sub};

use num_derive::FromPrimitive;

#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static mut softfloat_roundingMode: u8;
    static mut softfloat_exceptionFlags: u8;
}

const SOFTFLOAT_ROUND_NEAR_EVEN: u8 = 0;
const SOFTFLOAT_ROUND_MINMAG: u8 = 1;
const SOFTFLOAT_ROUND_MIN: u8 = 2;
const SOFTFLOAT_ROUND_MAX: u8 = 3;
const _SOFTFLOAT_ROUND_NEAR_MAXMAG: u8 = 4;
const _SOFTFLOAT_ROUND_ODD: u8 = 6;

pub const SOFTFLOAT_FLAG_INEXACT: u8 = 1;
pub const SOFTFLOAT_FLAG_UNDERFLOW: u8 = 2;
pub const SOFTFLOAT_FLAG_OVERFLOW: u8 = 4;
pub const SOFTFLOAT_FLAG_INFINITE: u8 = 8;
pub const SOFTFLOAT_FLAG_INVALID: u8 = 16;
pub const SOFTFLOAT_FLAG_ROUNDED_UP: u8 = 16;

pub trait RoundedFrom<T> {
    fn rounded_from(val: T, rc: RoundingControl) -> Self;
}

pub fn clear_exception_flags() {
    unsafe {
        softfloat_exceptionFlags = 0;
    }
}

pub fn get_exception_flags() -> u8 {
    unsafe { softfloat_exceptionFlags }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, FromPrimitive)]
pub enum RoundingControl {
    ToNearest = 0,
    Down,
    Up,
    TowardsZero,
}

impl RoundingControl {
    pub fn to_softfloat_rounding_mode(&self) -> u8 {
        match self {
            RoundingControl::ToNearest => SOFTFLOAT_ROUND_NEAR_EVEN,
            RoundingControl::Down => SOFTFLOAT_ROUND_MIN,
            RoundingControl::Up => SOFTFLOAT_ROUND_MAX,
            RoundingControl::TowardsZero => SOFTFLOAT_ROUND_MINMAG,
        }
    }
}

#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Float64(u64);

impl Float64 {
    pub const NAN: Float64 = Float64(f64::NAN.to_bits());

    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub fn to_bits(&self) -> u64 {
        self.0
    }
}

impl From<Float64> for f64 {
    fn from(value: Float64) -> Self {
        f64::from_bits(value.to_bits())
    }
}

impl From<f64> for Float64 {
    fn from(value: f64) -> Self {
        Float64::from_bits(value.to_bits())
    }
}

impl Add for Float64 {
    type Output = Float64;

    fn add(self, rhs: Self) -> Self::Output {
        unsafe { f64_add(self, rhs) }
    }
}

impl RoundedFrom<i64> for Float64 {
    fn rounded_from(value: i64, rc: RoundingControl) -> Self {
        set_rounding(rc);
        unsafe { i64_to_f64(value) }
    }
}

impl RoundedFrom<Float80> for Float64 {
    fn rounded_from(value: Float80, rc: RoundingControl) -> Self {
        if value.is_invalid() {
            return Float64::NAN
        }

        set_rounding(rc);
        unsafe { extF80M_to_f64(&value.internal()) }
    }
}

#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Float32(u32);

impl Float32 {
    pub const NAN: Float32 = Float32(0xFFC0_0000);

    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub fn to_bits(&self) -> u32 {
        self.0
    }
}

impl RoundedFrom<i64> for Float32 {
    fn rounded_from(value: i64, rc: RoundingControl) -> Self {
        unsafe {
            set_rounding(rc);
            i64_to_f32(value)
        }
    }
}

impl RoundedFrom<Float80> for Float32 {
    fn rounded_from(value: Float80, rc: RoundingControl) -> Self {
        if value.is_invalid() {
            return Float32::NAN
        }

        unsafe {
            set_rounding(rc);
            extF80M_to_f32(&value.internal())
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Float80(u64, u16);

impl RoundedFrom<Float32> for Float80 {
    fn rounded_from(value: Float32, rc: RoundingControl) -> Self {
        unsafe {
            set_rounding(rc);
            let mut result = Internal80::default();
            f32_to_extF80M(value, &mut result);
            Float80::from_internal(result)
        }
    }
}

impl RoundedFrom<Float64> for Float80 {
    fn rounded_from(value: Float64, rc: RoundingControl) -> Self {
        set_rounding(rc);
        unsafe {
            let mut result = Internal80::default();
            f64_to_extF80M(value, &mut result);
            Float80::from_internal(result)
        }
    }
}

impl RoundedFrom<i16> for Float80 {
    fn rounded_from(value: i16, rc: RoundingControl) -> Self {
        <Self as RoundedFrom<_>>::rounded_from(value as i64, rc)
    }
}

impl RoundedFrom<i32> for Float80 {
    fn rounded_from(value: i32, rc: RoundingControl) -> Self {
        <Self as RoundedFrom<_>>::rounded_from(value as i64, rc)
    }
}

impl RoundedFrom<i64> for Float80 {
    fn rounded_from(value: i64, rc: RoundingControl) -> Self {
        unsafe {
            set_rounding(rc);
            let mut result = Internal80::default();
            i64_to_extF80M(value, &mut result);
            Float80::from_internal(result)
        }
    }
}

impl Float80 {
    pub fn from_bits(bits: u128) -> Self {
        Self(bits as u64, (bits >> 64) as u16)
    }

    pub fn to_bits(&self) -> u128 {
        self.0 as u128 | ((self.1 as u128) << 64)
    }

    fn internal(&self) -> Internal80 {
        Internal80 {
            signif: self.0,
            sign_exp: self.1,
        }
    }

    fn from_internal(val: Internal80) -> Self {
        Self(val.signif, val.sign_exp)
    }

    pub fn is_less_than(&self, other: &Float80) -> bool {
        unsafe { extF80M_lt(&self.internal(), &other.internal()) }
    }

    pub fn is_equal_to(&self, other: &Float80) -> bool {
        unsafe { extF80M_eq(&self.internal(), &other.internal()) }
    }

    pub fn round_to_int(&self, rc: RoundingControl) -> Float80 {
        unsafe {
            let mut result = Internal80::default();
            extF80M_roundToInt(&self.internal(), rc.to_softfloat_rounding_mode(), true, &mut result);
            Self::from_internal(result)
        }
    }

    pub fn cast_to_f32_is_precise(&self) -> bool {
        unsafe {
            softfloat_exceptionFlags = 0;
            extF80M_to_f32(&self.internal());
            softfloat_exceptionFlags & SOFTFLOAT_FLAG_INEXACT == 0
        }
    }

    pub fn cast_to_f64_is_precise(&self) -> bool {
        unsafe {
            softfloat_exceptionFlags = 0;
            extF80M_to_f64(&self.internal());
            softfloat_exceptionFlags & SOFTFLOAT_FLAG_INEXACT == 0
        }
    }

    pub fn sqrt(&self, rc: RoundingControl) -> Float80 {
        set_rounding(rc);
        unsafe {
            let mut result = Internal80::default();
            extF80M_sqrt(&self.internal(), &mut result);
            Self::from_internal(result)
        }
    }

    pub fn to_i64(&self, rc: RoundingControl) -> i64 {
        set_rounding(rc);
        unsafe { extF80M_to_i64(&self.internal()) }
    }

    pub fn add(self, other: Float80, rc: RoundingControl) -> Self {
        set_rounding(rc);
        self + other
    }

    pub fn sub(self, other: Float80, rc: RoundingControl) -> Self {
        set_rounding(rc);
        self - other
    }

    pub fn mul(self, other: Float80, rc: RoundingControl) -> Self {
        set_rounding(rc);
        self * other
    }

    pub fn div(self, other: Float80, rc: RoundingControl) -> Self {
        set_rounding(rc);
        self / other
    }

    pub fn rem(self, other: Float80, rc: RoundingControl) -> Self {
        set_rounding(rc);
        self % other
    }

    pub fn scale(mut self, scale: Float80, rc: RoundingControl) -> Self {
        // TODO: Proper implementation
        let n = scale.to_i64(rc);
        self.1 = self.1.wrapping_add(n as i16 as u16);
        self
    }

    pub fn is_invalid(&self) -> bool {
        (self.0 >> 63) != 1 && (0x0001..=0x7ffe).contains(&self.1)
    }
}

fn set_rounding(rc: RoundingControl) {
    unsafe {
        softfloat_roundingMode = rc.to_softfloat_rounding_mode();
    }
}

impl Sub for Float80 {
    type Output = Float80;

    fn sub(self, rhs: Self) -> Self::Output {
        unsafe {
            let mut result = Internal80::default();
            extF80M_sub(&self.internal(), &rhs.internal(), &mut result);
            Self::from_internal(result)
        }
    }
}

impl Add for Float80 {
    type Output = Float80;

    fn add(self, rhs: Self) -> Self::Output {
        unsafe {
            let mut result = Internal80::default();
            extF80M_add(&self.internal(), &rhs.internal(), &mut result);
            Self::from_internal(result)
        }
    }
}

impl Mul for Float80 {
    type Output = Float80;

    fn mul(self, rhs: Self) -> Self::Output {
        unsafe {
            let mut result = Internal80::default();
            extF80M_mul(&self.internal(), &rhs.internal(), &mut result);
            Self::from_internal(result)
        }
    }
}

impl Div for Float80 {
    type Output = Float80;

    fn div(self, rhs: Self) -> Self::Output {
        unsafe {
            let mut result = Internal80::default();
            extF80M_div(&self.internal(), &rhs.internal(), &mut result);
            Self::from_internal(result)
        }
    }
}

impl Rem for Float80 {
    type Output = Float80;

    fn rem(self, rhs: Self) -> Self::Output {
        unsafe {
            let mut result = Internal80::default();
            extF80M_rem(&self.internal(), &rhs.internal(), &mut result);
            Self::from_internal(result)
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct Internal80 {
    pub signif: u64,
    pub sign_exp: u16,
}

unsafe extern "C" {
    fn extF80M_add(_: &Internal80, _: &Internal80, _: &mut Internal80) -> Internal80;
    fn extF80M_sub(_: &Internal80, _: &Internal80, _: &mut Internal80) -> Internal80;
    fn f64_add(_: Float64, _: Float64) -> Float64;
    fn extF80M_mul(_: &Internal80, _: &Internal80, _: &mut Internal80);
    fn extF80M_div(_: &Internal80, _: &Internal80, _: &mut Internal80);
    fn extF80M_rem(_: &Internal80, _: &Internal80, _: &mut Internal80);
    fn f32_to_extF80M(_: Float32, _: &mut Internal80);
    fn f64_to_extF80M(_: Float64, _: &mut Internal80);
    fn extF80M_to_f32(_: &Internal80) -> Float32;
    fn extF80M_to_f64(_: &Internal80) -> Float64;
    fn extF80M_to_i64(_: &Internal80) -> i64;
    fn extF80M_lt(_: &Internal80, _: &Internal80) -> bool;
    fn extF80M_eq(_: &Internal80, _: &Internal80) -> bool;
    fn i64_to_f32(_: i64) -> Float32;
    fn i64_to_f64(_: i64) -> Float64;
    fn i64_to_extF80M(_: i64, _: &mut Internal80);
    fn extF80M_roundToInt(_: &Internal80, _: u8, _: bool, _: &mut Internal80);
    fn extF80M_sqrt(_: &Internal80, _: &mut Internal80);
}

#[cfg(test)]
mod tests {
    use crate::{Float64, Float80};

    #[test]
    pub fn f80_addition_works() {
        let a = Float80::from_bits(0x3FFF8000000000000000); // 1.0
        let b = Float80::from_bits(0x4000C90FDAA22168C235); // pi
        let result = a + b;

        println!("{result:X?}");
        assert_eq!(result, Float80::from_bits(0x40018487ED5110B4611A)); // pi + 1.0
    }

    #[test]
    pub fn f64_addition_works() {
        let a = Float64::from_bits(0xddccbbaa);
        let b = Float64::from_bits(0x44332211);
        let result = a + b;

        println!("{result:X?}");
        assert_eq!(result, Float64::from_bits(0x121ffddbb));
    }
}
