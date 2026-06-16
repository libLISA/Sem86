use liblisa::state::Size;
use sem86_core::arch::intel386::{
    FLAG_CF, FLAG_PF, FLAG_X87_CC0, FLAG_X87_CC2, FLAG_X87_CC3, FLAG_X87_DENORMALIZED_OPERAND, FLAG_X87_INVALID_OPERATION,
    FLAG_X87_STACK_FAULT, FLAG_ZF, Intel386, X87Reg,
};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::fpu::dsl::{F80, is_invalid, is_nan_or_invalid, is_signaling_nan};
use crate::instrs::fpu::{
    CastFloat, F80IsZero, Format, FormatBits, ST0, Sov, St, StBits, UpdateFpPointers, check_available, dsl, fpstack_pop,
};
use crate::{Config, encoding, encoding_group, ops};

#[derive(Copy, Clone, Debug)]
enum QuietNaNBehavior {
    Exception,
    NoException,
}

#[derive(Copy, Clone, Debug)]
enum FlagTarget {
    ConditionCodes,
    EFlags,
}

#[derive(Copy, Clone, Debug)]
enum Pop {
    Zero,
    Once,
    Twice,
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    use FlagTarget::*;
    use QuietNaNBehavior::*;

    [
        encoding! {
            Name { "FTST" },
            Prefixes, #0xD9, #0xE4,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let val = St(0.into());

                    let is_denormal = dsl::is_denormal::<F80>(val);
                    if !is_zero(is_denormal) {
                        FLAG_X87_DENORMALIZED_OPERAND := 1;
                    }

                    let is_nan_or_invalid = is_nan_or_invalid::<F80>(val);
                    if !is_zero(is_nan_or_invalid) {
                        (X87Reg::ConditionCodes, Size::one_byte(0)) := 1;
                        (X87Reg::ConditionCodes, Size::one_byte(2)) := 1;
                        (X87Reg::ConditionCodes, Size::one_byte(3)) := 1;
                        FLAG_X87_INVALID_OPERATION := 1;
                        FLAG_X87_STACK_FAULT := 0;
                    } else {
                        (X87Reg::ConditionCodes, Size::one_byte(0)) := f80_cmp_lt(val, 0, X87Reg::RoundingControl);
                        (X87Reg::ConditionCodes, Size::one_byte(1)) := 0;
                        (X87Reg::ConditionCodes, Size::one_byte(2)) := 0;
                        (X87Reg::ConditionCodes, Size::one_byte(3)) := F80IsZero(val);
                    }


                }))
            }
        },
        encoding_group! {
            [
                Name { "FCOM" },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 2 } = rm,
            ] = (ST0, Sov::Val(rm), format, Pop::Zero, Exception, ConditionCodes),
            [
                Name { "FCOMP" },
                OverrideMemorySize { 4 },
                Prefixes, 1, 1, 0, 1, FormatBits = format, 0,
                ModNMemRm { 3 } = rm,
            ] = (ST0, Sov::Val(rm), format, Pop::Once, Exception, ConditionCodes),
            [
                Name { "FCOMPP" },
                Prefixes, #0xDE, #0xD9,
            ] = (ST0, Sov::st(1), Format::Float80, Pop::Twice, Exception, ConditionCodes),

            [
                Name { "FCOM_sti" },
                Prefixes, #0xD8,
                1, 1, 0, 1, 0, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Zero, Exception, ConditionCodes),
            [
                Name { "FCOMP_sti" },
                Prefixes, #0xD8,
                1, 1, 0, 1, 1, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Once, Exception, ConditionCodes),
            [
                Name { "FUCOM_sti" },
                Prefixes, #0xDD,
                1, 1, 1, 0, 0, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Zero, NoException, ConditionCodes),
            [
                Name { "FUCOMP_sti" },
                Prefixes, #0xDD,
                1, 1, 1, 0, 1, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Once, NoException, ConditionCodes),
            [
                Name { "FUCOMPP" },
                Prefixes, #0xDA, #0xE9,
            ]= (ST0, Sov::st(1), Format::Float80, Pop::Twice, NoException, ConditionCodes),

            [
                Name { "FCOMI_st0_sti" },
                Prefixes, #0xDB,
                1, 1, 1, 1, 0, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Zero, Exception, EFlags),
            [
                Name { "FCOMIP_st0_sti" },
                Prefixes, #0xDF,
                1, 1, 1, 1, 0, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Once, Exception, EFlags),
            [
                Name { "FUCOMI_st0_sti" },
                Prefixes, #0xDB,
                1, 1, 1, 0, 1, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Zero, NoException, EFlags),
            [
                Name { "FUCOMIP_st0_sti" },
                Prefixes, #0xDF,
                1, 1, 1, 0, 1, StBits = reg,
            ] = (ST0, reg, Format::Float80, Pop::Once, NoException, EFlags),
            map |(lhs, rhs, format, pop, quiet_nan_behavior, flag_target)|{
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let lhs = lhs;
                    let rhs = rhs;
                    let rhs = CastFloat {
                        from: format,
                        to: Format::Float80,
                        val: rhs,
                    };

                    let lhs_is_invalid = is_nan_or_invalid::<F80>(lhs);
                    let rhs_is_invalid = is_nan_or_invalid::<F80>(rhs);
                    let either_invalid = or(lhs_is_invalid, rhs_is_invalid);

                    if !is_zero(either_invalid) {
                        #[match quiet_nan_behavior] {
                            QuietNaNBehavior::NoException => {
                                let lhs_is_invalid = is_invalid::<F80>(lhs);
                                let rhs_is_invalid = is_invalid::<F80>(rhs);
                                let either_invalid = or(lhs_is_invalid, rhs_is_invalid);

                                if is_zero(either_invalid) {
                                    let lhs_signaling = is_signaling_nan::<F80>(lhs);
                                    let rhs_signaling = is_signaling_nan::<F80>(rhs);
                                    let either_signaling = or(lhs_signaling, rhs_signaling);

                                    // Only raise exception if either NaN is signaling.
                                    if !is_zero(either_signaling) {
                                        FLAG_X87_INVALID_OPERATION := 1;
                                    }
                                } else {
                                    // Always raise exception if values are invalid ("unsupported")
                                    FLAG_X87_INVALID_OPERATION := 1;
                                }
                            }
                            QuietNaNBehavior::Exception => {
                                FLAG_X87_INVALID_OPERATION := 1;
                            }
                        }

                        #[match flag_target] {
                            FlagTarget::ConditionCodes => {
                                FLAG_X87_CC0 := 1;
                                FLAG_X87_CC2 := 1;
                                FLAG_X87_CC3 := 1;
                            }
                            FlagTarget::EFlags => {
                                FLAG_CF := 1;
                                FLAG_PF := 1;
                                FLAG_ZF := 1;
                            }
                        }
                    } else {
                        let lt = f80_cmp_lt(lhs, rhs, X87Reg::RoundingControl);
                        let eq = f80_cmp_eq(lhs, rhs, X87Reg::RoundingControl);

                        #[match flag_target] {
                            FlagTarget::ConditionCodes => {
                                FLAG_X87_CC0 := lt;
                                FLAG_X87_CC2 := 0;
                                FLAG_X87_CC3 := eq;
                            }
                            FlagTarget::EFlags => {
                                FLAG_CF := lt;
                                FLAG_PF := 0;
                                FLAG_ZF := eq;
                            }
                        }
                    }

                    #[match pop] {
                        Pop::Zero => {}
                        Pop::Once => {
                            ..fpstack_pop();
                        }
                        Pop::Twice => {
                            ..fpstack_pop();
                            ..fpstack_pop();
                        }
                    }


                }))
            }
        },
    ]
}
