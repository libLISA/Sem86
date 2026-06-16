use sem86_core::arch::intel386::{FLAG_X87_DENORMALIZED_OPERAND, FLAG_X87_INVALID_OPERATION, Intel386};
use sem86_core::il::{Cmd, Val};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::fpu::dsl::{F32, F64, is_denormal, is_signaling_nan};
use crate::instrs::fpu::{
    CastFloat, CheckExceptionFlags, Format, ST0, St, UpdateFpPointers, check_available, fpstack_pop, fpstack_push,
};
use crate::{Config, encoding, encoding_group, ops};

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding_group! {
            [
                Name { "FLD_m32fp" },
                OverrideMemorySize { 4 },
                Prefixes, #0xD9,
                ModNMemRm { 0 } = rm,
            ] = (rm, Format::Float32),
            [
                Name { "FLD_m64fp" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDD,
                ModNMemRm { 0 } = rm,
            ] = (rm, Format::Float64),
            [
                Name { "FLD_m80fp" },
                OverrideMemorySize { 10 },
                Prefixes, #0xDB,
                ModNMemRm { 5 } = rm,
            ] = (rm, Format::Float80),

            [
                Name { "FILD_m16int" },
                OverrideMemorySize { 2 },
                Prefixes, #0xDF,
                ModNMemRm { 0 } = rm,
            ] = (rm, Format::Int16),
            [
                Name { "FILD_m32int" },
                OverrideMemorySize { 4 },
                Prefixes, #0xDB,
                ModNMemRm { 0 } = rm,
            ] = (rm, Format::Int32),
            [
                Name { "FILD_m64int" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDF,
                ModNMemRm { 5 } = rm,
            ] = (rm, Format::Int64),
            map |(rm, format)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let val = rm;
                let denormal;
                let snan;
                #[match format] {
                    Format::Float32 => {
                        denormal := is_denormal::<F32>(val);
                        snan := is_signaling_nan::<F32>(val);
                    }
                    Format::Float64 => {
                        denormal := is_denormal::<F64>(val);
                        snan := is_signaling_nan::<F64>(val);
                    }
                    _ => {
                        denormal := 0;
                        snan := 0;
                    },
                }

                if !is_zero(denormal) {
                    FLAG_X87_DENORMALIZED_OPERAND := 1;
                }

                if !is_zero(snan) {
                    FLAG_X87_INVALID_OPERATION := 1;
                }

                let val = CastFloat {
                    val,
                    from: format,
                    to: Format::Float80
                };

                (fpstack_push()) := val;

            }))
        },
        encoding! {
            Name { "FLD_sti" },
            Prefixes, #0xD9,
            1, 1, 0, 0, 0, Imm { 3 } = reg,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let val = St(reg);
                    (fpstack_push()) := val;

                }))
            }
        },
        encoding! {
            Name { "FLD_const" },
            Prefixes, #0xD9,
            1, 1, 1, 0, 1, ExpandedBits { 3 } = const_id, // +1.0, log2(10), log2(e), pi, log10(2), loge(2), +0.0, invalid
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops!{
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    const consts = &[
                        0x3FFF8000000000000000u128, // +1.0
                        0x4000D49A784BCD1B8AFE, // log2(10)
                        0x3FFFB8AA3B295C17F0BC, // log2(e)
                        0x4000C90FDAA22168C235, // pi
                        0x3FFD9A209A84FBCFF799, // log10(2)
                        0x3FFEB17217F7D1CF79AC, // loge(2)
                        0x00000000000000000000, // +0.0
                        0, // TODO: should not be considered a valid instruction
                    ];

                    let val = shl((consts[const_id as usize] >> 64) as u64, 64);
                    let val = or(val, consts[const_id as usize] as u64);

                    fpstack_push() := val;

                }))
            }
        },
        encoding_group! {
            [
                Name { "FST_m32fp" },
                OverrideMemorySize { 4 },
                Prefixes, #0xD9,
                ModNMemRm { 2 } = rm,
            ] = (rm, Format::Float32, false),
            [
                Name { "FST_m64fp" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDD,
                ModNMemRm { 2 } = rm,
            ] = (rm, Format::Float64, false),
            [
                Name { "FSTP_m32fp" },
                OverrideMemorySize { 4 },
                Prefixes, #0xD9,
                ModNMemRm { 3 } = rm,
            ] = (rm, Format::Float32, true),
            [
                Name { "FSTP_m64fp" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDD,
                ModNMemRm { 3 } = rm,
            ] = (rm, Format::Float64, true),
            [
                Name { "FSTP_m80fp" },
                OverrideMemorySize { 10 },
                Prefixes, #0xDB,
                ModNMemRm { 7 } = rm,
            ] = (rm, Format::Float80, true),

            [
                Name { "FIST_m16int" },
                OverrideMemorySize { 2 },
                Prefixes, #0xDF,
                ModNMemRm { 2 } = rm,
            ] = (rm, Format::Int16, false),
            [
                Name { "FIST_m32int" },
                OverrideMemorySize { 4 },
                Prefixes, #0xDB,
                ModNMemRm { 2 } = rm,
            ] = (rm, Format::Int32, false),
            [
                Name { "FISTP_m16int" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDF,
                ModNMemRm { 3 } = rm,
            ] = (rm, Format::Int64, true),
            [
                Name { "FISTP_m32int" },
                OverrideMemorySize { 4 },
                Prefixes, #0xDB,
                ModNMemRm { 3 } = rm,
            ] = (rm, Format::Int32, true),
            [
                Name { "FISTP_m64int" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDF,
                ModNMemRm { 7 } = rm,
            ] = (rm, Format::Int64, true),
            map |(rm, format, do_pop)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                let val = ST0;

                let result = CastFloat {
                    val,
                    from: Format::Float80,
                    to: format,
                };

                ..CheckExceptionFlags(result);

                // TODO: Properly detect which values are invalid and properly set exceptions
                #[match format] {
                    Format::Int16 => {
                        // TODO
                    }
                    Format::Int32 => {
                        // TODO
                    }
                    Format::Int64 => {
                        let sentinel = cmp_eq(result, 0x8000_0000_0000_0000u64);
                        if !is_zero(sentinel) {
                            FLAG_X87_INVALID_OPERATION := 1;
                        }
                    }
                    _ => {}
                }

                rm := result;

                #[if do_pop] {
                    ..fpstack_pop();
                }


            }))
        },
        encoding! {
            Name { "FST(P)_sti" },
            Prefixes, #0xDD,
            1, 1, 0, 1, ExpandedBit = do_pop, Imm { 3 } = reg,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let val = ST0;
                    St(reg) := val;

                    #[if do_pop] {
                        ..fpstack_pop();
                    }


                }))
            }
        },
        encoding_group! {
            [
                Name { "FISTTP_m16int" },
                OverrideMemorySize { 2 },
                Prefixes, #0xDF,
                ModNMemRm { 1 } = rm,
            ] = (rm, Format::Int16),
            [
                Name { "FISTTP_m32int" },
                OverrideMemorySize { 4 },
                Prefixes, #0xDB,
                ModNMemRm { 1 } = rm,
            ] = (rm, Format::Int32),
            [
                Name { "FISTTP_m64int" },
                OverrideMemorySize { 8 },
                Prefixes, #0xDD,
                ModNMemRm { 1 } = rm,
            ] = (rm, Format::Int64),
            map |(rm, format)| BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                #[context(ctx)]
                ..UpdateFpPointers;

                // TODO: FISTTP
                ..Cmd::Log { message: format!("TODO: FISTTP {format:?}") };
                ..Cmd::mov(Val::Temp(0), rm);

            }))
        },
        encoding! {
            Name { "FBLD" },
            Prefixes, #0xDF,
            ModNMemRm { 4 } = rm,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    // TODO: FBLD
                    ..Cmd::Log { message: String::from("TODO: FBLD") };
                    ..Cmd::mov(Val::Temp(0), rm);

                }))
            }
        },
        encoding! {
            Name { "FBSTP" },
            Prefixes, #0xDF,
            ModNMemRm { 6 } = rm,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    // TODO: FBSTP
                    ..Cmd::Log { message: String::from("TODO: FBSTP") };
                    ..Cmd::mov(Val::Temp(0), rm);

                }))
            }
        },
        encoding_group! {
            [
                Name { "FCMOVB_st0_sti" },
                Prefixes,
                1, 1, 0, 1, 1, 0, 1, ExpandedBit = _negate,
                1, 1, 0, ExpandedBits { 2 } = cond, Imm { 3 } = src,
            ] = (0, src, cond),
            map |(_src, _dst, _cond)| {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    // TODO: FCMOVB_st0_sti
                    ..Cmd::Log { message: String::from("TODO: FCMOVB_st0_sti") };

                }))
            }
        },
    ]
}
