use liblisa::encoding::dataflows::{MemoryAccess, MemorySizeRange};
use liblisa::state::Size;
use sem86_core::arch::intel386::{
    FLAG_X87_CC0, FLAG_X87_CC1, FLAG_X87_CC2, FLAG_X87_CC3, FLAG_X87_DENORMALIZED_OPERAND, FLAG_X87_INVALID_OPERATION,
    FLAG_X87_OVERFLOW, FLAG_X87_PRECISION, FLAG_X87_STACK_FAULT, FLAG_X87_UNDERFLOW, FLAG_X87_ZERO_DIVIDE, GpReg, Intel386,
    X87Reg,
};
use sem86_core::il::{Cmd, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Context};
use crate::dsl::*;
use crate::instrs::fpu::{UncheckedDynMmx, UpdateFpPointers, check_available};
use crate::instrs::{DWORD, LOW_BYTE, QWORD, WORD};
use crate::{Config, encoding, encoding_group, ops};

struct PackedControlWord;

impl LoadIntoVal<Intel386> for PackedControlWord {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let result = 0xffff_0040u64;
            let val;
            val := shl((X87Reg::ExceptionMasks, Size::one_byte(0)), 0);
            result := or(result, val);

            val := shl((X87Reg::ExceptionMasks, Size::one_byte(1)), 1);
            result := or(result, val);

            val := shl((X87Reg::ExceptionMasks, Size::one_byte(2)), 2);
            result := or(result, val);

            val := shl((X87Reg::ExceptionMasks, Size::one_byte(3)), 3);
            result := or(result, val);

            val := shl((X87Reg::ExceptionMasks, Size::one_byte(4)), 4);
            result := or(result, val);

            val := shl((X87Reg::ExceptionMasks, Size::one_byte(5)), 5);
            result := or(result, val);

            // There are only 6 valid masks (bit 0-5) in the control word.
            // For legacy reasons, bit 6 is also expected to be modifiable.
            val := shl((X87Reg::ExceptionMasks, Size::one_byte(6)), 6);
            result := or(result, val);

            val := shl(X87Reg::PrecisionControl, 8);
            result := or(result, val);

            val := shl(X87Reg::RoundingControl, 10);
            result := or(result, val);

            val := shl(X87Reg::InfinityControl, 12);
            result := or(result, val);

            target := result;
        });
    }
}

impl StoreInto<Intel386> for PackedControlWord {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let val = val;
            (X87Reg::ExceptionMasks, Size::one_byte(0)) := select_bit(val, 0);
            (X87Reg::ExceptionMasks, Size::one_byte(1)) := select_bit(val, 1);
            (X87Reg::ExceptionMasks, Size::one_byte(2)) := select_bit(val, 2);
            (X87Reg::ExceptionMasks, Size::one_byte(3)) := select_bit(val, 3);
            (X87Reg::ExceptionMasks, Size::one_byte(4)) := select_bit(val, 4);
            (X87Reg::ExceptionMasks, Size::one_byte(5)) := select_bit(val, 5);
            (X87Reg::ExceptionMasks, Size::one_byte(6)) := select_bit(val, 6);

            let bits = shr(val, 8);
            X87Reg::PrecisionControl := and(bits, 3);

            let bits = shr(val, 10);
            X87Reg::RoundingControl := and(bits, 3);

            X87Reg::InfinityControl := select_bit(val, 12);
        })
    }
}

struct PackedStatusWord;

impl LoadIntoVal<Intel386> for PackedStatusWord {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let result = 0xffff_0000u64;
            let val;
            val := shl(FLAG_X87_INVALID_OPERATION, 0);
            result := or(result, val);

            val := shl(FLAG_X87_DENORMALIZED_OPERAND, 1);
            result := or(result, val);

            val := shl(FLAG_X87_ZERO_DIVIDE, 2);
            result := or(result, val);

            val := shl(FLAG_X87_OVERFLOW, 3);
            result := or(result, val);

            val := shl(FLAG_X87_UNDERFLOW, 4);
            result := or(result, val);

            val := shl(FLAG_X87_PRECISION, 5);
            result := or(result, val);

            val := shl(FLAG_X87_STACK_FAULT, 6);
            result := or(result, val);

            let mask = xor(X87Reg::ExceptionMasks, 0x010101_01010101u64);
            let summary = and(X87Reg::ExceptionFlags, mask);
            let summary = ite(summary, 0, 1);

            val :=  shl(summary, 7);
            result := or(result, val);

            val := shl(FLAG_X87_CC0, 8);
            result := or(result, val);

            val := shl(FLAG_X87_CC1, 9);
            result := or(result, val);

            val := shl(FLAG_X87_CC2, 10);
            result := or(result, val);

            val := shl(X87Reg::Top, 11);
            result := or(result, val);

            val := shl(FLAG_X87_CC3, 14);
            result := or(result, val);

            target := result;
        });
    }
}

impl StoreInto<Intel386> for PackedStatusWord {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let val = val;

            FLAG_X87_INVALID_OPERATION := select_bit(val, 0);
            FLAG_X87_DENORMALIZED_OPERAND := select_bit(val, 1);
            FLAG_X87_ZERO_DIVIDE := select_bit(val, 2);
            FLAG_X87_OVERFLOW := select_bit(val, 3);
            FLAG_X87_UNDERFLOW := select_bit(val, 4);
            FLAG_X87_PRECISION := select_bit(val, 5);
            FLAG_X87_STACK_FAULT := select_bit(val, 6);
            FLAG_X87_CC0 := select_bit(val, 8);
            FLAG_X87_CC1 := select_bit(val, 9);
            FLAG_X87_CC2 := select_bit(val, 10);
            let bits = shr(val, 11);
            X87Reg::Top := and(bits, 7);
            FLAG_X87_CC3 := select_bit(val, 14);
        })
    }
}

/// Loads the legacy x87 2 bit tag words, relative to the current TOP.
/// Make sure to set PackedStatusWord (or X87Reg::Top) *before* storing into this, not after.
struct PackedTwoBitTagWord;

impl LoadIntoVal<Intel386> for PackedTwoBitTagWord {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let result = 0xffff_0000u64;
            let shift = mul(0, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11, 0);
            result := or(result, tmp);

            let shift = mul(1, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 2, 0);
            result := or(result, tmp);

            let shift = mul(2, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 4, 0);
            result := or(result, tmp);

            let shift = mul(3, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 6, 0);
            result := or(result, tmp);

            let shift = mul(4, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 8, 0);
            result := or(result, tmp);

            let shift = mul(5, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 10, 0);
            result := or(result, tmp);

            let shift = mul(6, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 12, 0);
            result := or(result, tmp);

            let shift = mul(7, 8);
            let is_valid = shr(X87Reg::MmIsValid, shift);
            let is_valid = and(is_valid, 1);
            let tmp = ite(is_valid, 0b11 << 14, 0);
            result := or(result, tmp);


            target := result;
        });
    }
}

impl StoreInto<Intel386> for PackedTwoBitTagWord {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]
            let val = val;

            let shift = mul(0, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(0)) := ite(tag, 0, 1);

            let shift = mul(1, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(1)) := ite(tag, 0, 1);

            let shift = mul(2, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(2)) := ite(tag, 0, 1);

            let shift = mul(3, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(3)) := ite(tag, 0, 1);

            let shift = mul(4, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(4)) := ite(tag, 0, 1);

            let shift = mul(5, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(5)) := ite(tag, 0, 1);

            let shift = mul(6, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(6)) := ite(tag, 0, 1);

            let shift = mul(7, 2);
            let tag = shr(val, shift);
            let tag = and(tag, 3);
            let tag = xor(tag, 3);
            (X87Reg::MmIsValid, Size::one_byte(7)) := ite(tag, 0, 1);
        })
    }
}

fn reset_x87_state(ctx: &mut Context) -> Vec<Cmd<Intel386>> {
    ops! {
        #[context(ctx)]

        // Default FCW: 0x37F
        // There are only 6 valid masks (bit 0-5) in the control word.
        // For legacy reasons, bit 6 is also expected to be modifiable.
        (X87Reg::ExceptionMasks, QWORD) := 0x010101010101u64;
        (X87Reg::PrecisionControl, LOW_BYTE) := 3;
        (X87Reg::RoundingControl, LOW_BYTE) := 0;
        (X87Reg::InfinityControl, LOW_BYTE) := 0;

        // Default FSW: 0
        (X87Reg::Top, LOW_BYTE) := 0;
        (X87Reg::ConditionCodes, DWORD) := 0;
        (X87Reg::ExceptionFlags, QWORD) := 0;

        // Other registers
        (X87Reg::MmIsValid, QWORD) := 0;
        (X87Reg::DataPointer, DWORD) := 0;
        (X87Reg::InstructionPointer, DWORD) := 0;
        (X87Reg::LastInstructionOpcode, DWORD) := 0;
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding! {
            Name { "WAIT" },
            Prefixes, #0x9B,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |_ctx|vec![
                    // TODO: Check for exceptions
                ]))
            }
        },
        encoding! {
            Name { "FNINIT" },
            Prefixes, #0xDB, #0xE3,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]

                    ..reset_x87_state(ctx);

                }))
            }
        },
        encoding! {
            Name { "FNSTCW" },
            OverrideMemorySize { 2 },
            Prefixes, #0xD9,
            ModNMemRm { 7 } = dest,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    (dest, WORD) := PackedControlWord;

                }))
            }
        },
        encoding_group! {
            [
                Name { "FNSTSW_ax" },
                Prefixes, #0xDF, #0xE0,
                FixedReg { GpReg::Ax } = dest,
            ] = dest,
            [
                Name { "FNSTSW_m2byte" },
                OverrideMemorySize { 2 },
                Prefixes, #0xDD,
                ModNMemRm { 7 } = rm,
            ] = rm,
            map |dest| BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                #[context(ctx)]
                (dest, WORD) := PackedStatusWord;

            }))
        },
        encoding! {
            Wide { true },
            Name { "FNSTENV" },
            Prefixes, #0xD9,
            ModNMemRm { 6 } = _,
            {
                BuildFromContext::new(move |ctx| {
                    let access = ctx.pop_access();
                    let calculation = access.calculation.unwrap_calculation();
                    let size = ctx.op_size();
                    let [
                        control_word,
                        status_word,
                        tag_word,
                        fip,
                        ip_selector,
                        data_pointer,
                        data_pointer_selector,
                    ] = std::array::from_fn(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * index) as i64).into(),
                        size: MemorySizeRange::single(size as u64),
                        ..access.clone()
                    }));

                    check_available(ctx, |ctx|ops! {
                        #[context(ctx)]

                        control_word := PackedControlWord;
                        status_word := PackedStatusWord;
                        tag_word := PackedTwoBitTagWord;
                        fip := X87Reg::InstructionPointer;
                        ip_selector := 0; // TODO
                        data_pointer := X87Reg::DataPointer;
                        data_pointer_selector := 0xffff_0000u64; // TODO: Store last-used segment selector value with x87 instruction in lower 16 bits

                    })
                })
            }
        },
        encoding! {
            Wide { true },
            Name { "FLDENV" },
            Prefixes, #0xD9,
            ModNMemRm { 4 } = _,
            {
                BuildFromContext::new(move |ctx| {
                    let access = ctx.pop_access();
                    let calculation = access.calculation.unwrap_calculation();
                    let size = ctx.op_size();
                    let [
                        control_word,
                        status_word,
                        tag_word,
                        fip,
                        _ip_selector,
                        data_pointer,
                        _data_pointer_selector,
                    ] = std::array::from_fn(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * index) as i64).into(),
                        size: MemorySizeRange::single(size as u64),
                        ..access.clone()
                    }));

                    check_available(ctx, |ctx|ops! {
                        #[context(ctx)]

                        PackedControlWord := control_word;
                        PackedStatusWord := status_word;
                        PackedTwoBitTagWord := tag_word;
                        X87Reg::InstructionPointer := fip;
                        // TODO: ip_selector
                        X87Reg::DataPointer := data_pointer;
                        // TODo: data_pointer_selector

                    })
                })
            }
        },
        encoding! {
            Name { "FLDCW" },
            OverrideMemorySize { 2 },
            Prefixes, #0xD9,
            ModNMemRm { 5 } = rm,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    PackedControlWord := rm;

                }))
            }
        },
        encoding! {
            Wide { true },
            Name { "FNSAVE" },
            Prefixes, #0xDD,
            ModNMemRm { 6 } = _,
            {
                BuildFromContext::new(move |ctx| {
                    let access = ctx.pop_access();
                    let calculation = access.calculation.unwrap_calculation();
                    let size = ctx.op_size();
                    let [
                        control_word,
                        status_word,
                        tag_word,
                        fip,
                        ip_selector,
                        data_pointer,
                        data_pointer_selector,
                    ] = std::array::from_fn(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * index) as i64).into(),
                        size: if [4, 6].contains(&index) {
                            MemorySizeRange::single(2)
                        } else {
                            MemorySizeRange::single(size as u64)
                        },
                        ..access.clone()
                    }));

                    let s = std::array::from_fn::<_, 8, _>(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * 7 + 10 * index) as i64).into(),
                        size: MemorySizeRange::single(10),
                        ..access.clone()
                    }));

                    check_available(ctx, |ctx|ops! {
                        #[context(ctx)]

                        control_word := PackedControlWord;
                        status_word := PackedStatusWord;
                        tag_word := PackedTwoBitTagWord;
                        fip := X87Reg::InstructionPointer;
                        ip_selector := X87Reg::InstructionSelector;
                        data_pointer := X87Reg::DataPointer;
                        data_pointer_selector := X87Reg::DataSelector;

                        let st0 = X87Reg::Top;
                        let st1 = add(st0, 1);
                        let st2 = add(st0, 2);
                        let st3 = add(st0, 3);
                        let st4 = add(st0, 4);
                        let st5 = add(st0, 5);
                        let st6 = add(st0, 6);
                        let st7 = add(st0, 7);

                        (s[0]) := UncheckedDynMmx(st0);
                        (s[1]) := UncheckedDynMmx(st1);
                        (s[2]) := UncheckedDynMmx(st2);
                        (s[3]) := UncheckedDynMmx(st3);
                        (s[4]) := UncheckedDynMmx(st4);
                        (s[5]) := UncheckedDynMmx(st5);
                        (s[6]) := UncheckedDynMmx(st6);
                        (s[7]) := UncheckedDynMmx(st7);

                        ..reset_x87_state(ctx);

                    })
                })
            }
        },
        encoding! {
            Wide { true },
            Name { "FRSTOR" },
            Prefixes, #0xDD,
            ModNMemRm { 4 } = _,
            {
                BuildFromContext::new(move |ctx| {
                    let access = ctx.pop_access();
                    let calculation = access.calculation.unwrap_calculation();
                    let size = ctx.op_size();
                    let [
                        control_word,
                        status_word,
                        tag_word,
                        fip,
                        ip_selector,
                        data_pointer,
                        data_pointer_selector,
                    ] = std::array::from_fn(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * index) as i64).into(),
                        size: MemorySizeRange::single(size as u64),
                        ..access.clone()
                    }));

                    let s = std::array::from_fn::<_, 8, _>(|index| ctx.add_access(MemoryAccess {
                        calculation: calculation.clone().with_added_offset((size * 7 + 10 * index) as i64).into(),
                        size: MemorySizeRange::single(10),
                        ..access.clone()
                    }));

                    check_available(ctx, |ctx|ops! {
                        #[context(ctx)]

                        PackedControlWord := control_word;
                        PackedStatusWord := status_word;
                        PackedTwoBitTagWord := tag_word;
                        X87Reg::InstructionPointer := fip;
                        X87Reg::InstructionSelector := ip_selector;
                        X87Reg::DataPointer := data_pointer;
                        X87Reg::DataSelector := data_pointer_selector;

                        let st0 = X87Reg::Top;
                        let st1 = add(st0, 1);
                        let st2 = add(st0, 2);
                        let st3 = add(st0, 3);
                        let st4 = add(st0, 4);
                        let st5 = add(st0, 5);
                        let st6 = add(st0, 6);
                        let st7 = add(st0, 7);

                        UncheckedDynMmx(st0) := (s[0]);
                        UncheckedDynMmx(st1) := (s[1]);
                        UncheckedDynMmx(st2) := (s[2]);
                        UncheckedDynMmx(st3) := (s[3]);
                        UncheckedDynMmx(st4) := (s[4]);
                        UncheckedDynMmx(st5) := (s[5]);
                        UncheckedDynMmx(st6) := (s[6]);
                        UncheckedDynMmx(st7) := (s[7]);


                    })
                })
            }
        },
        encoding! {
            Name { "FXRSTOR" },
            Prefixes, #0x0F, #0xAE,
            ModNMemRm { 1 } = rm,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |_ctx|vec![
                    // TODO: FXRSTOR
                    Cmd::Log { message: String::from("TODO: FXRSTOR") },
                    Cmd::mov(Val::Temp(0), rm),
                ]))
            }
        },
        encoding! {
            Name { "FXSAVE" },
            Prefixes, #0x0F, #0xAE,
            ModNMemRm { 0 } = rm,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |_ctx|vec![
                    // TODO: FXSAVE
                    Cmd::Log { message: String::from("TODO: FXSAVE") },
                    Cmd::mov(Val::Temp(0), rm),
                ]))
            }
        },
        encoding! {
            Name { "FFREE_sti" },
            Prefixes, #0xDD,
            1, 1, 0, 0, 0, Imm { 3 } = reg,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx| ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let n = mul(reg, 8);
                    let bit = shl(1, n);
                    let mask = xor(bit, u64::MAX);
                    X87Reg::MmIsValid := and(X87Reg::MmIsValid, mask);


                }))
            }
        },
        encoding! {
            Name { "FINCSTP" },
            Prefixes, #0xD9, #0xF7,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let result = add(X87Reg::Top, 1);
                    X87Reg::Top := and(result, 7);

                }))
            }
        },
        encoding! {
            Name { "FDECSTP" },
            Prefixes, #0xD9, #0xF6,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]
                    ..UpdateFpPointers;

                    let result = sub(X87Reg::Top, 1);
                    X87Reg::Top := and(result, 7);

                }))
            }
        },
        encoding! {
            Name { "FNCLEX" },
            Prefixes, #0xDB, #0xE2,
            {
                BuildFromContext::new(move |ctx| check_available(ctx, |ctx|ops! {
                    #[context(ctx)]

                    X87Reg::ExceptionFlags := 0;
                    // This instruction is also supposed to set FSW[15] := 0 according to the Intel Reference manual,
                    // but that bit is always 0 on modern CPUs.


                }))
            }
        },
    ]
}
