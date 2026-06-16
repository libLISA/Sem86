use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt::Display;

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use itertools::Itertools;
use liblisa::Instruction;
use liblisa::arch::{Arch, CpuState, Register};
use liblisa::encoding::bitpattern::{FlowValueLocation, Part, PartMapping};
use liblisa::encoding::dataflows::{AccessKind, AddrTermSize, MemoryAccesses, ParameterizedComputation};
use liblisa::encoding::{Encoding, EncodingRef, IgnoredMetadata, ParLoc, Semantics, UnsizedParLoc};
use liblisa::state::{Addr, Size};
use liblisa::utils::{EitherIter, bitmask_u64, bitmask_u128};
use liblisa::value::{AsValue, MutValue};
use log::{info, trace};
use mem_dbg::{CopyType, False, MemSize};
use num_traits::FromPrimitive;
use sem86_arch::exceptions::Exception;
use sem86_arch::mem::{Mem32, Mmio};
use serde::{Deserialize, Serialize};
use softfloat::{
    Float32, Float64, Float80, RoundedFrom, RoundingControl, SOFTFLOAT_FLAG_ROUNDED_UP, clear_exception_flags,
    get_exception_flags,
};

use crate::arch::intel386::{GpReg, HandlerId, Intel386, Reg, X87Reg};
use crate::emulator::exec::ExecutionContext;
use crate::hw::HwMmio;
use crate::il::part_values::{PackingStructure, PartValues};

pub mod absint;
mod jump;
pub mod part_values;

pub use jump::{Jump, NextIp};

pub const MAX_TEMP_VARS: usize = 256;

struct Ctx<'a, 'mem, 'tag, A> {
    tmp: [u128; MAX_TEMP_VARS],
    print: bool,
    execution_context: &'a mut ExecutionContext<'mem, 'tag, A>,
    mem_areas: &'a [MemArea],
}

#[repr(C)]
pub struct EfficientSystemState<'a, A: Arch> {
    pub instr: Instruction,
    pub cpu: &'a mut A::CpuState,
    pub mem: ArrayVec<u128, 16>,
    pub part_values: PartValues,
    pub part_packing: &'a PackingStructure,
    pub parts: &'a [Part<A>],
}

impl<A: Arch> std::fmt::Display for EfficientSystemState<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.cpu, f)?;

        for (index, mem) in self.mem.iter().enumerate() {
            if index == 0 {
                writeln!(f, "instr = {:X}", self.instr)?;
            } else {
                writeln!(f, "m{index} = 0x{mem:X}")?;
            }
        }

        Ok(())
    }
}

impl<A: Arch> std::fmt::Debug for EfficientSystemState<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.cpu, f)?;

        writeln!(f, "instr = {:X}", self.instr)?;
        for (index, mem) in self.mem.iter().enumerate() {
            writeln!(f, "m{index} = 0x{mem:X}")?;
        }

        Ok(())
    }
}

impl<A: Arch> EfficientSystemState<'_, A> {
    pub fn get_dest(&self, dest: ParLoc<A>, apply_bitorder: bool) -> u128 {
        let mask = bitmask_u128(dest.size.num_bytes() as u32 * 8);
        let val = match dest.loc {
            UnsizedParLoc::InstrLen => self.instr.byte_len() as u128,
            UnsizedParLoc::Const(value) => value as u128,
            UnsizedParLoc::Part(part_index) => match &self.parts[part_index].mapping {
                PartMapping::Register {
                    mapping,
                } => self
                    .cpu
                    .reg(mapping[self.part_values.get(self.part_packing, part_index) as usize].unwrap())
                    .unwrap_num() as u128,
                PartMapping::Imm {
                    bits,
                    mapping,
                } => {
                    assert!(bits.is_none());
                    if apply_bitorder && let Some(mapping) = mapping.as_ref() {
                        mapping.compute(self.part_values.get(self.part_packing, part_index)).unwrap() as u128
                    } else {
                        self.part_values.get(self.part_packing, part_index) as u128
                    }
                },
                PartMapping::MemoryComputation {
                    ..
                } => unreachable!(),
            },
            UnsizedParLoc::Reg(reg) => self.cpu.reg(reg).unwrap_num() as u128,
            UnsizedParLoc::Mem(index) => self.mem[index],
        };

        (val >> (dest.size.start_byte() * 8)) & mask
    }

    pub fn cpu(&self) -> &A::CpuState {
        self.cpu
    }

    /// Allows the value of `dest` to be modified through a [`MutValue`].
    pub fn modify_dest(&mut self, dest: &ParLoc<A>, modify: impl FnOnce(MutValue<'_>)) {
        match self.resolve_loc(*dest).loc {
            UnsizedParLoc::Reg(reg) => self.cpu.modify_reg(reg, |value| match value {
                MutValue::Num(current) => {
                    let w = dest.size.num_bytes() * 8;
                    let unshifted_mask = bitmask_u64(w.min(64) as u32);
                    let mask = unshifted_mask << (dest.size.start_byte() * 8);
                    let mut val = (*current >> (dest.size.start_byte() * 8)) & unshifted_mask;
                    modify(MutValue::Num(&mut val));
                    *current = (*current & !mask) | ((val & unshifted_mask) << (dest.size.start_byte() * 8));
                },
                MutValue::Bytes(bytes) => modify(MutValue::Bytes(&mut bytes[dest.size.start_byte()..=dest.size.end_byte()])),
            }),
            UnsizedParLoc::Mem(index) => modify(MutValue::Bytes(
                &mut bytemuck::cast_mut::<_, [u8; 16]>(&mut self.mem[index])[dest.size.start_byte()..=dest.size.end_byte()],
            )),
            UnsizedParLoc::Const(_) | UnsizedParLoc::InstrLen | UnsizedParLoc::Part(_) => panic!("unable to modify: {dest:?}"),
        }
    }

    pub fn resolve_loc(&self, input: ParLoc<A>) -> ParLoc<A> {
        match input.loc {
            UnsizedParLoc::Part(part_index) => match &self.parts[part_index].mapping {
                PartMapping::Register {
                    mapping,
                } => ParLoc {
                    loc: UnsizedParLoc::Reg(mapping[self.part_values.get(self.part_packing, part_index) as usize].unwrap()),
                    size: input.size,
                },
                // TODO: Apply MappingOrBitorder
                PartMapping::Imm {
                    ..
                } => ParLoc {
                    loc: UnsizedParLoc::Const(self.part_values.get(self.part_packing, part_index)),
                    size: input.size,
                },
                PartMapping::MemoryComputation {
                    ..
                } => unreachable!(),
            },
            _ => input,
        }
    }
}

type _ValSize = Val<Intel386>;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, mem_dbg::MemSize)]
pub enum Val<A: Arch> {
    Temp(usize),
    Loc(ParLoc<A>),
    Conv {
        loc: ParLoc<A>,
        source_bits: u8,
        target_bits: u8,
        sign_extend: bool,
        swap_endianness: bool,
    },
}

impl<A: Arch> From<ParLoc<A>> for Val<A> {
    fn from(loc: ParLoc<A>) -> Self {
        Self::Loc(loc)
    }
}

impl<A: Arch> From<(UnsizedParLoc<A>, Size)> for Val<A> {
    fn from((loc, size): (UnsizedParLoc<A>, Size)) -> Self {
        Self::Loc(ParLoc {
            loc,
            size,
        })
    }
}

impl From<Reg> for Val<Intel386> {
    fn from(reg: Reg) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(reg),
            size: Size::new(0, reg.byte_size() - 1),
        })
    }
}

impl From<GpReg> for Val<Intel386> {
    fn from(reg: GpReg) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
            size: Size::new(0, reg.byte_size() - 1),
        })
    }
}

impl From<X87Reg> for Val<Intel386> {
    fn from(reg: X87Reg) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(Reg::X87(reg)),
            size: Size::new(0, reg.byte_size() - 1),
        })
    }
}

impl From<(GpReg, Size)> for Val<Intel386> {
    fn from((reg, size): (GpReg, Size)) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(Reg::Gp(reg)),
            size,
        })
    }
}

impl From<(X87Reg, Size)> for Val<Intel386> {
    fn from((reg, size): (X87Reg, Size)) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(Reg::X87(reg)),
            size,
        })
    }
}

impl From<(Reg, Size)> for Val<Intel386> {
    fn from((reg, size): (Reg, Size)) -> Self {
        Self::Loc(ParLoc {
            loc: UnsizedParLoc::Reg(reg),
            size,
        })
    }
}

impl<A: Arch> From<(Val<A>, Size)> for Val<A> {
    fn from((val, size): (Val<A>, Size)) -> Self {
        match val {
            Val::Loc(par_loc) => Self::Loc(ParLoc {
                loc: par_loc.loc,
                size,
            }),
            Val::Temp(_) => todo!(),
            Val::Conv {
                ..
            } => todo!(),
        }
    }
}

impl<A: Arch> From<u64> for Val<A> {
    fn from(value: u64) -> Self {
        Self::const_val(value)
    }
}

impl<A: Arch> From<usize> for Val<A> {
    fn from(value: usize) -> Self {
        Self::const_val(u64::try_from(value).unwrap())
    }
}

impl<A: Arch> From<i32> for Val<A> {
    fn from(value: i32) -> Self {
        Self::const_val(value as i64 as u64)
    }
}

impl<A: Arch> Val<A> {
    pub const fn const_val(val: u64) -> Val<A> {
        Val::Loc(ParLoc {
            loc: UnsizedParLoc::Const(val),
            size: Size::qword(),
        })
    }

    pub fn sign_extend(self, source_bits: u8, target_bits: u8) -> Val<A> {
        match self {
            Val::Loc(loc) => Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend: true,
                swap_endianness: false,
            },
            Val::Conv {
                loc,
                swap_endianness: false,
                source_bits: x,
                ..
            } if x >= source_bits => Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend: true,
                swap_endianness: false,
            },
            Val::Conv {
                loc,
                source_bits: x,
                swap_endianness,
                ..
            } if x == source_bits => Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend: true,
                swap_endianness,
            },
            Val::Conv {
                loc,
                source_bits: x,
                swap_endianness,
                sign_extend: true,
                ..
            } if source_bits >= x => Val::Conv {
                loc,
                source_bits: x,
                target_bits,
                sign_extend: true,
                swap_endianness,
            },
            _ => panic!("Cannot sign-extend ({source_bits} => {target_bits}) {self:?}"),
        }
    }

    fn map_locs(&self, mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>) -> Val<A> {
        match self {
            Val::Temp(n) => Val::Temp(*n),
            Val::Loc(par_loc) => Val::Loc(map_flows(false, par_loc).unwrap()),
            Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend,
                swap_endianness,
            } => Val::Conv {
                loc: map_flows(false, loc).unwrap(),
                source_bits: *source_bits,
                target_bits: *target_bits,
                sign_extend: *sign_extend,
                swap_endianness: *swap_endianness,
            },
        }
    }

    fn eval(&self, tmp: &[u128], s: &mut EfficientSystemState<A>) -> u128 {
        match self {
            Val::Temp(n) => tmp[*n],
            Val::Loc(dest)
            | Val::Conv {
                loc: dest, ..
            } => {
                let v = s.get_dest(*dest, false);

                match *self {
                    Val::Conv {
                        source_bits,
                        sign_extend,
                        swap_endianness,
                        target_bits,
                        ..
                    } => Self::apply_conversion(v, source_bits, sign_extend, swap_endianness, target_bits),
                    _ => v,
                }
            },
        }
    }

    pub fn accesses_mem(&self, index: usize) -> bool {
        match self {
            Val::Temp(_) => false,
            Val::Loc(loc)
            | Val::Conv {
                loc, ..
            } => loc.loc == UnsizedParLoc::Mem(index),
        }
    }

    fn write(&self, s: &mut EfficientSystemState<A>, ctx: &mut Ctx<A>, result: u128) {
        match self {
            Val::Temp(n) => {
                if ctx.print {
                    trace!("  ! eval: temp{n} = 0x{result:X}");
                }
                ctx.tmp[*n] = result
            },
            Val::Loc(dest) => s.modify_dest(dest, |val| match val {
                liblisa::value::MutValue::Num(n) => {
                    if ctx.print {
                        trace!("  ! eval: {dest} = 0x{result:X}");
                    }
                    *n = result as u64
                },
                liblisa::value::MutValue::Bytes(b) => {
                    for (n, b) in b.iter_mut().enumerate() {
                        *b = (result >> (n * 8)) as u8;
                    }
                },
            }),
            Val::Conv {
                ..
            } => unimplemented!(),
        }
    }

    pub fn apply_conversion(v: u128, source_bits: u8, sign_extend: bool, swap_endianness: bool, target_bits: u8) -> u128 {
        let mut v = v;
        if swap_endianness {
            match source_bits {
                8 => (),
                16 => v = (v as u16).swap_bytes() as u128,
                32 => v = (v as u32).swap_bytes() as u128,
                n if n <= 64 => v = v.swap_bytes() >> (((64 - n) / 8) * 8),
                n => todo!("swap bytes for {n} bits?"),
            }
        }

        if sign_extend {
            let s = 64 - source_bits;
            v = (((v as i64) << s) >> s) as u128;
        }

        v & !(u128::MAX.checked_shl(target_bits as u32).unwrap_or(0))
    }

    pub fn loc(&self) -> Option<&ParLoc<A>> {
        match self {
            Val::Temp(_) => None,
            Val::Loc(loc)
            | Val::Conv {
                loc, ..
            } => Some(loc),
        }
    }

    fn as_const(&self) -> Option<u64> {
        self.loc().and_then(|parloc| {
            if let UnsizedParLoc::Const(c) = parloc.loc {
                Some(c)
            } else {
                None
            }
        })
    }
}

impl PartialEq<GpReg> for Val<Intel386> {
    fn eq(&self, other: &GpReg) -> bool {
        match self {
            Val::Loc(loc)
            | Val::Conv {
                loc, ..
            } => loc.loc == UnsizedParLoc::Reg(Reg::Gp(*other)),
            _ => false,
        }
    }
}

impl<A: Arch> Display for Val<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Val::Temp(n) => write!(f, "tmp{n}"),
            Val::Loc(par_loc) => write!(f, "{par_loc}"),
            Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend,
                swap_endianness,
            } => write!(
                f,
                "{loc}[bits={source_bits} => {target_bits}, sign_extend={sign_extend}, swap_endianness={swap_endianness}]"
            ),
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, mem_dbg::MemSize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Xor,
    Or,
    And,
    Shl,
    Shr,
    /// Count is cropped to 64 bits, then `count % bitsize` is computed for the actual rotation.
    Rol(u8),

    /// Count is cropped to 64 bits, then `count % bitsize` is computed for the actual rotation.
    Ror(u8),
    Sar(u8),
    Div,
    Mod,
    SignedMod64,
    CmpGt,
    CmpLt,
    CmpEq,
    SignedDiv64,
}

impl BinOp {
    pub fn is_commutative(&self) -> bool {
        matches!(self, BinOp::Add | BinOp::Mul | BinOp::Xor | BinOp::Or | BinOp::And)
    }

    #[inline(always)]
    pub fn execute(&self, x: u128, y: u128) -> u128 {
        match self {
            BinOp::Add => x.wrapping_add(y),
            BinOp::Sub => x.wrapping_sub(y),
            BinOp::Mul => x.wrapping_mul(y),
            BinOp::Xor => x ^ y,
            BinOp::Or => x | y,
            BinOp::And => x & y,
            BinOp::Shl => x.wrapping_shl(y as u32),
            BinOp::Shr => x.wrapping_shr(y as u32),
            BinOp::Rol(num_bits) => {
                x.wrapping_shl(y as u32 % *num_bits as u32)
                    | x.wrapping_shr((*num_bits as u32).wrapping_sub(y as u32 % *num_bits as u32))
            },
            BinOp::Ror(num_bits) => {
                x.wrapping_shr(y as u32 % *num_bits as u32)
                    | x.wrapping_shl((*num_bits as u32).wrapping_sub(y as u32 % *num_bits as u32))
            },
            BinOp::Sar(num_bytes) => match *num_bytes {
                1 => (x as i8 as i64).wrapping_shr(y as u32) as u128,
                2 => (x as i16 as i64).wrapping_shr(y as u32) as u128,
                4 => (x as i32 as i64).wrapping_shr(y as u32) as u128,
                _ => unreachable!(),
            },
            BinOp::Div => {
                if y == 0 {
                    0
                } else {
                    x.wrapping_div(y)
                }
            },
            BinOp::Mod => (x).checked_rem(y).unwrap_or(0),
            BinOp::SignedMod64 => (x as i64).checked_rem(y as i64).unwrap_or(0) as u64 as u128,
            BinOp::CmpGt => (x > y) as u128,
            BinOp::CmpLt => (x < y) as u128,
            BinOp::CmpEq => (x == y) as u128,
            BinOp::SignedDiv64 => (x as i64).checked_div(y as i64).unwrap_or(0) as u64 as u128,
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, mem_dbg::MemSize)]
pub enum FpBinOp {
    F80Add,
    F80Sub,
    F80Mul,
    F80Div,
    F80Rem,
    F80CmpLt,
    F80CmpEq,
    F80Scale,
}

impl FpBinOp {
    pub fn is_commutative(&self) -> bool {
        false
    }

    #[inline(always)]
    pub fn execute(&self, x: u128, y: u128, rc: u8) -> u128 {
        clear_exception_flags();

        let rc = RoundingControl::from_u8(rc).unwrap();
        let result = match self {
            FpBinOp::F80Add => (Float80::from_bits(x).add(Float80::from_bits(y), rc)).to_bits(),
            FpBinOp::F80Sub => (Float80::from_bits(x).sub(Float80::from_bits(y), rc)).to_bits(),
            FpBinOp::F80Mul => (Float80::from_bits(x).mul(Float80::from_bits(y), rc)).to_bits(),
            FpBinOp::F80Div => (Float80::from_bits(x).div(Float80::from_bits(y), rc)).to_bits(),
            FpBinOp::F80Rem => (Float80::from_bits(x).rem(Float80::from_bits(y), rc)).to_bits(),
            FpBinOp::F80CmpLt => Float80::from_bits(x).is_less_than(&Float80::from_bits(y)) as u128,
            FpBinOp::F80CmpEq => Float80::from_bits(x).is_equal_to(&Float80::from_bits(y)) as u128,
            FpBinOp::F80Scale => Float80::from_bits(x).scale(Float80::from_bits(y), rc).to_bits(),
        };

        let ef = get_exception_flags();
        result | ((ef as u128) << 120)
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, mem_dbg::MemSize)]
pub enum FpUnOp {
    F32ToF80,
    F64ToF80,
    F80ToF64,
    F80ToF32,
    F80ToF64IsPrecise,
    F80ToF32IsPrecise,
    I64ToF32,
    I64ToF64,
    I64ToF80,
    F80ToI64,
    RoundToIntF80,
    SinF80,
    CosF80,
    TanF80,
    SqrtF80,
    F2Xm1F80,
    RoundF80ToF32,
    RoundF80ToF64,
    Log2F80,
    ArcTanF80,
}

fn reduce_f80_precision<const BITS: u32>(mut val: u128, rc: RoundingControl) -> u128 {
    let round_mask = bitmask_u128(BITS);
    let round_max = 1u128 << BITS;
    let round_bits = val & round_mask;
    let is_negative = (val >> 79) & 1 != 0;
    let lsb_kept = (val >> BITS) & 1 != 0;

    let round_up = match rc {
        // Round to nearest value; equivalent for positive and negative numbers
        RoundingControl::ToNearest => match round_bits.cmp(&(round_max / 2)) {
            Ordering::Greater => true,
            Ordering::Less => false,
            // When equally close, round to even value
            Ordering::Equal => lsb_kept,
        },
        // Round towards -inf; rounding up when negative
        RoundingControl::Down => is_negative,
        // Round towards +inf; rounding up when positive
        RoundingControl::Up => !is_negative,
        // Truncate; always rounding down
        RoundingControl::TowardsZero => false,
    };

    if round_up {
        val += round_mask;
        val &= !round_mask;
        // If the significand overflowed into the exponent, we need to make sure to keep the integer bit 1.
        val |= 0x8000_0000_0000_0000;
        val |= (SOFTFLOAT_FLAG_ROUNDED_UP as u128) << 120;
    } else {
        val &= !round_mask;
    }

    val
}

impl FpUnOp {
    #[inline(always)]
    pub fn execute(&self, s: u128, rc: u8) -> u128 {
        clear_exception_flags();
        let rc = RoundingControl::from_u8(rc).unwrap();
        let result = match self {
            FpUnOp::F32ToF80 => Float80::rounded_from(Float32::from_bits(s as u32), rc).to_bits(),
            FpUnOp::F64ToF80 => Float80::rounded_from(Float64::from_bits(s as u64), rc).to_bits(),
            FpUnOp::F80ToF64 => Float64::rounded_from(Float80::from_bits(s), rc).to_bits() as u128,
            FpUnOp::F80ToF32 => Float32::rounded_from(Float80::from_bits(s), rc).to_bits() as u128,
            FpUnOp::F80ToF64IsPrecise => Float80::from_bits(s).cast_to_f64_is_precise() as u128,
            FpUnOp::F80ToF32IsPrecise => Float80::from_bits(s).cast_to_f32_is_precise() as u128,
            FpUnOp::I64ToF32 => Float32::rounded_from(s as i128 as i64, rc).to_bits() as u128,
            FpUnOp::I64ToF64 => Float64::rounded_from(s as i128 as i64, rc).to_bits() as u128,
            FpUnOp::I64ToF80 => Float80::rounded_from(s as u64 as i64, rc).to_bits(),
            FpUnOp::F80ToI64 => Float80::from_bits(s).to_i64(rc) as u64 as u128,
            FpUnOp::SinF80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = val.sin();
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::CosF80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = val.cos();
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::TanF80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = val.tan();
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::F2Xm1F80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = 2f64.powf(val) - 1.;
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::Log2F80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = val.log2();
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::ArcTanF80 => {
                // TODO: Approximation using full 80 bits
                let val = f64::from(Float64::rounded_from(Float80::from_bits(s), rc));
                let result = val.atan();
                Float80::rounded_from(Float64::from(result), rc).to_bits()
            },
            FpUnOp::RoundToIntF80 => Float80::from_bits(s).round_to_int(rc).to_bits(),
            FpUnOp::SqrtF80 => Float80::from_bits(s).sqrt(rc).to_bits(),
            FpUnOp::RoundF80ToF32 => reduce_f80_precision::<40>(s, rc),
            FpUnOp::RoundF80ToF64 => reduce_f80_precision::<11>(s, rc),
        };

        let ef = get_exception_flags();
        result | ((ef as u128) << 120)
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, mem_dbg::MemSize)]
pub enum UnOp {
    Id,
    ByteSwap16,
    ByteSwap32,
    ByteSwap64,
    IsZero,
    SelectBit(u8),
    SignExtend(u8),
    Parity,
    TrailingZeros,
    HighestBitSet,
}

impl UnOp {
    pub fn execute(&self, s: u128) -> u128 {
        match self {
            UnOp::Id => s,
            UnOp::ByteSwap16 => (s as u16).swap_bytes() as u128,
            UnOp::ByteSwap32 => (s as u32).swap_bytes() as u128,
            UnOp::ByteSwap64 => (s as u64).swap_bytes() as u128,
            UnOp::IsZero => (s == 0) as u128,
            UnOp::SelectBit(n) => (s >> n) & 1,
            UnOp::SignExtend(n) => (((s << (128 - n)) as i128) >> (128 - n)) as u128,
            UnOp::Parity => ((s & 0xff).count_ones() + 1) as u128 & 1,
            UnOp::TrailingZeros => s.trailing_zeros() as u128,
            UnOp::HighestBitSet => 127 - s.leading_zeros() as u128,
        }
    }
}

type _OpSize = Op<Intel386>;

// TODO: Long-term we should be moving towards only allowing temporaries as args.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, mem_dbg::MemSize)]
pub enum Op<A: Arch> {
    FpBinOp {
        args: [Val<A>; 2],
        rc: Val<A>,
        op: FpBinOp,
    },
    FpUnOp {
        arg: Val<A>,
        rc: Val<A>,
        op: FpUnOp,
    },
    BinOp {
        args: [Val<A>; 2],
        op: BinOp,
    },
    UnOp {
        arg: Val<A>,
        op: UnOp,
    },
    Ite {
        cond: Val<A>,
        if_nonzero: Val<A>,
        if_zero: Val<A>,
    },
}

impl<A: Arch, T: Into<Val<A>>> From<T> for Op<A> {
    fn from(arg: T) -> Self {
        Self::UnOp {
            arg: arg.into(),
            op: UnOp::Id,
        }
    }
}

impl<A: Arch> From<(Val<A>, UnOp)> for Op<A> {
    fn from((val, op): (Val<A>, UnOp)) -> Self {
        Op::UnOp {
            arg: val,
            op,
        }
    }
}

impl<A: Arch> Op<A> {
    fn map_locs(&self, mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>) -> Op<A> {
        match self {
            Op::BinOp {
                args,
                op,
            } => Op::BinOp {
                args: (*args).map(|arg| arg.map_locs(&mut map_flows)),
                op: *op,
            },
            Op::FpBinOp {
                args,
                rc,
                op,
            } => Op::FpBinOp {
                args: (*args).map(|arg| arg.map_locs(&mut map_flows)),
                rc: rc.map_locs(&mut map_flows),
                op: *op,
            },
            Op::FpUnOp {
                arg,
                rc,
                op,
            } => Op::FpUnOp {
                arg: arg.map_locs(&mut map_flows),
                rc: rc.map_locs(&mut map_flows),
                op: *op,
            },
            Op::UnOp {
                arg,
                op,
            } => Op::UnOp {
                arg: arg.map_locs(&mut map_flows),
                op: *op,
            },
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => Op::Ite {
                cond: cond.map_locs(&mut map_flows),
                if_nonzero: if_nonzero.map_locs(&mut map_flows),
                if_zero: if_zero.map_locs(&mut map_flows),
            },
        }
    }

    fn execute(&self, tmp: &[u128], s: &mut EfficientSystemState<A>) -> u128 {
        match self {
            Op::BinOp {
                args,
                op,
            } => op.execute(args[0].eval(tmp, s), args[1].eval(tmp, s)),
            Op::FpBinOp {
                args,
                rc,
                op,
            } => op.execute(args[0].eval(tmp, s), args[1].eval(tmp, s), rc.eval(tmp, s) as u8),
            Op::FpUnOp {
                arg,
                rc,
                op,
            } => op.execute(arg.eval(tmp, s), rc.eval(tmp, s) as u8),
            Op::UnOp {
                arg,
                op,
            } => op.execute(arg.eval(tmp, s)),
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => {
                if cond.eval(tmp, s) != 0 {
                    if_nonzero.eval(tmp, s)
                } else {
                    if_zero.eval(tmp, s)
                }
            },
        }
    }

    fn accesses_mem(&self, index: usize) -> bool {
        match self {
            Op::FpBinOp {
                args,
                rc,
                ..
            } => args.iter().any(|a| a.accesses_mem(index)) || rc.accesses_mem(index),
            Op::FpUnOp {
                arg,
                rc,
                ..
            } => arg.accesses_mem(index) || rc.accesses_mem(index),
            Op::BinOp {
                args, ..
            } => args.iter().any(|a| a.accesses_mem(index)),
            Op::UnOp {
                arg, ..
            } => arg.accesses_mem(index),
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => cond.accesses_mem(index) || if_nonzero.accesses_mem(index) || if_zero.accesses_mem(index),
        }
    }
}

impl<A: Arch> Display for Op<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::FpBinOp {
                args,
                rc,
                op,
            } => write!(f, "{op:?}({}, {})", args.iter().format(", "), rc),
            Op::FpUnOp {
                arg,
                rc,
                op,
            } => write!(f, "{op:?}({arg}, {rc:?})"),
            Op::BinOp {
                args,
                op,
            } => write!(f, "{op:?}({})", args.iter().format(", ")),
            Op::UnOp {
                arg,
                op,
            } => write!(f, "{op:?}({arg})"),
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => write!(f, "if {cond} then {if_nonzero} else {if_zero} fi"),
        }
    }
}

type _CmdSize = Cmd<Intel386>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Cmd<A: Arch> {
    Store {
        to: Val<A>,
        op: Op<A>,
    },
    StoreDynamicReg {
        regs: Vec<Option<A::Reg>>,
        index: Val<A>,
        value: Val<A>,
        size: Size,
    },
    LoadDynamicReg {
        regs: Vec<Option<A::Reg>>,
        index: Val<A>,
        into: Val<A>,
        size: Size,
    },
    Log {
        message: String,
    },
    Handler {
        id: HandlerId,
        // TODO: Limit argument count to 3
        args: Vec<Val<A>>,
    },
    If {
        val: Val<A>,
        if_zero: Commands<A>,
        if_nonzero: Commands<A>,
    },
    Exception {
        exception: Exception,
        code: Val<A>,
    },

    /// Reads a segment descriptor from the LDT or GDT.
    /// For a NULL selector, ReadDescriptor will return access_rights that indicate the segment is present, but will set ok=0.
    ReadDescriptor {
        /// Always read from a descriptor table, even in real / virtual 8086 mode.
        force: bool,

        /// The selector to load.
        selector: Val<A>,

        /// 1 is written to this value if the segment passed all access checks, 0 if it did not.
        ok: Val<A>,

        /// The segment base from the descriptor.
        /// This value will not be valid if the descriptor is a gate.
        base: Val<A>,

        /// The segment limit from the descriptor.
        /// This value will not be valid if the descriptor is a gate.
        limit: Val<A>,

        /// The access rights for the descriptor.
        /// Uses the format required by LAR.
        access_rights: Val<A>,

        /// Whether to mark the segment as accessed upon successful load.
        mark_accessed: bool,
    },
    Out {
        len: usize,
        port: Val<A>,
        data: Val<A>,
    },
    In {
        len: usize,
        port: Val<A>,
        data: Val<A>,
    },
    ReadMemory {
        index: usize,
    },
    WriteMemory {
        index: usize,
    },
}

impl<A: Arch> MemSize for Cmd<A>
where
    A::Reg: MemSize + CopyType,
{
    fn mem_size(&self, flags: mem_dbg::SizeFlags) -> usize {
        size_of::<Self>()
            + match self {
                Cmd::Log {
                    message,
                } => message.mem_size(flags),
                Cmd::Handler {
                    args, ..
                } => (*args).mem_size(flags),
                Cmd::If {
                    if_zero,
                    if_nonzero,
                    ..
                } => if_zero.mem_size(flags) + if_nonzero.mem_size(flags),
                _ => 0,
            }
    }
}

impl<A: Arch> CopyType for Cmd<A> {
    type Copy = False;
}

impl<A: Arch> Display for Cmd<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_with_indent(f, Indent::default())
    }
}

impl<A: Arch> Cmd<A> {
    pub const fn store(to: Val<A>, op: Op<A>) -> Self {
        Self::Store {
            to,
            op,
        }
    }

    pub fn mov(to: Val<A>, from: Val<A>) -> Self {
        Self::Store {
            to,
            op: Op::UnOp {
                arg: from,
                op: UnOp::Id,
            },
        }
    }

    fn map_locs(&self, mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>) -> Cmd<A> {
        match self {
            Cmd::Store {
                to,
                op,
            } => Self::Store {
                to: to.map_locs(&mut map_flows),
                op: op.map_locs(&mut map_flows),
            },
            Cmd::StoreDynamicReg {
                regs,
                index,
                value,
                size,
            } => Self::StoreDynamicReg {
                regs: regs.clone(),
                index: index.map_locs(&mut map_flows),
                value: value.map_locs(&mut map_flows),
                size: *size,
            },
            Cmd::LoadDynamicReg {
                regs,
                index,
                into,
                size,
            } => Self::LoadDynamicReg {
                regs: regs.clone(),
                index: index.map_locs(&mut map_flows),
                into: into.map_locs(&mut map_flows),
                size: *size,
            },
            Cmd::Handler {
                id,
                args,
            } => Cmd::Handler {
                id: *id,
                args: args.iter().map(|arg| arg.map_locs(&mut map_flows)).collect(),
            },
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => {
                let mut b = Box::new(map_flows);
                let mut b = &mut b as &mut dyn FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>;
                Self::If {
                    val: val.map_locs(&mut b),
                    if_zero: if_zero.map(&mut b),
                    if_nonzero: if_nonzero.map(&mut b),
                }
            },
            Cmd::Exception {
                exception,
                code,
            } => Cmd::Exception {
                exception: *exception,
                code: code.map_locs(&mut map_flows),
            },
            Cmd::ReadDescriptor {
                force,
                selector,
                ok,
                base,
                limit,
                access_rights,
                mark_accessed,
            } => Cmd::ReadDescriptor {
                force: *force,
                selector: selector.map_locs(&mut map_flows),
                ok: ok.map_locs(&mut map_flows),
                base: base.map_locs(&mut map_flows),
                limit: limit.map_locs(&mut map_flows),
                access_rights: access_rights.map_locs(&mut map_flows),
                mark_accessed: *mark_accessed,
            },
            Cmd::Out {
                len,
                port,
                data,
            } => Cmd::Out {
                len: *len,
                port: port.map_locs(&mut map_flows),
                data: data.map_locs(&mut map_flows),
            },
            Cmd::In {
                len,
                port,
                data,
            } => Cmd::In {
                len: *len,
                port: port.map_locs(&mut map_flows),
                data: data.map_locs(&mut map_flows),
            },
            Cmd::Log {
                message,
            } => Cmd::Log {
                message: message.clone(),
            },
            Cmd::ReadMemory {
                index,
            } => Cmd::ReadMemory {
                index: *index,
            },
            Cmd::WriteMemory {
                index,
            } => Cmd::WriteMemory {
                index: *index,
            },
        }
    }

    fn always_terminates(&self) -> bool {
        match self {
            Cmd::ReadDescriptor {
                ..
            }
            | Cmd::Out {
                ..
            }
            | Cmd::In {
                ..
            }
            | Cmd::Log {
                ..
            }
            | Cmd::Store {
                ..
            }
            | Cmd::ReadMemory {
                ..
            }
            | Cmd::WriteMemory {
                ..
            }
            | Cmd::StoreDynamicReg {
                ..
            }
            | Cmd::LoadDynamicReg {
                ..
            } => false,
            Cmd::Exception {
                ..
            }
            | Cmd::Handler {
                ..
            } => true,
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => if_zero.all_paths_terminate() && if_nonzero.all_paths_terminate(),
        }
    }

    fn insert_memory_writes(&mut self, accesses: &[usize]) {
        if let Cmd::If {
            if_zero,
            if_nonzero,
            ..
        } = self
        {
            if_zero.insert_memory_writes(accesses, false);
            if_nonzero.insert_memory_writes(accesses, false);
        }
    }

    pub fn reads_from_mem(&self, mem_index: usize) -> bool {
        match self {
            Cmd::Store {
                op, ..
            } => op.accesses_mem(mem_index),
            Cmd::StoreDynamicReg {
                index,
                value,
                ..
            } => index.accesses_mem(mem_index) || value.accesses_mem(mem_index),
            Cmd::LoadDynamicReg {
                index, ..
            } => index.accesses_mem(mem_index),
            Cmd::Handler {
                args, ..
            } => args.iter().any(|a| a.accesses_mem(mem_index)),
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => val.accesses_mem(mem_index) || if_zero.reads_from_mem(mem_index) || if_nonzero.reads_from_mem(mem_index),
            Cmd::Exception {
                code, ..
            } => code.accesses_mem(mem_index),
            Cmd::ReadDescriptor {
                selector, ..
            } => selector.accesses_mem(mem_index),
            Cmd::Out {
                port,
                data,
                ..
            } => port.accesses_mem(mem_index) || data.accesses_mem(mem_index),
            Cmd::In {
                port, ..
            } => port.accesses_mem(mem_index),
            Cmd::Log {
                ..
            }
            | Cmd::ReadMemory {
                ..
            }
            | Cmd::WriteMemory {
                ..
            } => false,
        }
    }

    pub fn writes_to_mem(&self, mem_index: usize) -> bool {
        match self {
            Cmd::Store {
                to, ..
            } => to.accesses_mem(mem_index),
            Cmd::LoadDynamicReg {
                into, ..
            } => into.accesses_mem(mem_index),
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => if_zero.writes_to_mem(mem_index) || if_nonzero.writes_to_mem(mem_index),
            Cmd::In {
                data, ..
            } => data.accesses_mem(mem_index),
            Cmd::ReadDescriptor {
                access_rights,
                ok,
                base,
                limit,
                ..
            } => {
                access_rights.accesses_mem(mem_index)
                    || ok.accesses_mem(mem_index)
                    || base.accesses_mem(mem_index)
                    || limit.accesses_mem(mem_index)
            },
            _ => false,
        }
    }

    pub fn accesses_mem(&self, mem_index: usize) -> bool {
        match self {
            Cmd::Store {
                to,
                op,
                ..
            } => to.accesses_mem(mem_index) || op.accesses_mem(mem_index),
            Cmd::StoreDynamicReg {
                index,
                value,
                ..
            } => index.accesses_mem(mem_index) || value.accesses_mem(mem_index),
            Cmd::LoadDynamicReg {
                index,
                into,
                ..
            } => index.accesses_mem(mem_index) || into.accesses_mem(mem_index),
            Cmd::Handler {
                args, ..
            } => args.iter().any(|a| a.accesses_mem(mem_index)),
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => val.accesses_mem(mem_index) || if_zero.accesses_mem(mem_index) || if_nonzero.accesses_mem(mem_index),
            Cmd::Exception {
                code, ..
            } => code.accesses_mem(mem_index),
            Cmd::ReadDescriptor {
                selector,
                access_rights,
                ok,
                base,
                limit,
                ..
            } => {
                selector.accesses_mem(mem_index)
                    || access_rights.accesses_mem(mem_index)
                    || ok.accesses_mem(mem_index)
                    || base.accesses_mem(mem_index)
                    || limit.accesses_mem(mem_index)
            },
            Cmd::Out {
                port,
                data,
                ..
            } => port.accesses_mem(mem_index) || data.accesses_mem(mem_index),
            Cmd::In {
                port, ..
            } => port.accesses_mem(mem_index),
            Cmd::Log {
                ..
            }
            | Cmd::ReadMemory {
                ..
            }
            | Cmd::WriteMemory {
                ..
            } => false,
        }
    }

    pub fn collect_write_targets(&self, output: &mut HashSet<Val<A>>) {
        match self {
            Cmd::Store {
                to, ..
            } => {
                output.insert(*to);
            },
            Cmd::StoreDynamicReg {
                regs,
                size,
                ..
            } => output.extend(regs.iter().flatten().map(|&reg| {
                Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(reg),
                    size: *size,
                })
            })),
            Cmd::LoadDynamicReg {
                into, ..
            } => {
                output.insert(*into);
            },
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => {
                if_zero.collect_write_targets(output);
                if_nonzero.collect_write_targets(output);
            },
            Cmd::ReadDescriptor {
                ok,
                access_rights,
                base,
                limit,
                ..
            } => {
                output.insert(*ok);
                output.insert(*access_rights);
                output.insert(*base);
                output.insert(*limit);
            },
            Cmd::In {
                data, ..
            } => {
                output.insert(*data);
            },
            Cmd::Exception {
                ..
            }
            | Cmd::Handler {
                ..
            }
            | Cmd::Out {
                ..
            }
            | Cmd::Log {
                ..
            } => (),
            Cmd::ReadMemory {
                index,
            } => {
                output.insert(Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Mem(*index),
                    size: Size::new(0, 15),
                }));
            },
            Cmd::WriteMemory {
                ..
            } => (),
        }
    }

    fn reads_descriptors(&self) -> bool {
        match self {
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => if_zero.reads_descriptors() || if_nonzero.reads_descriptors(),
            Cmd::ReadDescriptor {
                ..
            } => true,
            _ => false,
        }
    }

    fn performs_port_io(&self) -> bool {
        match self {
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => if_zero.reads_descriptors() || if_nonzero.reads_descriptors(),
            Cmd::In {
                ..
            }
            | Cmd::Out {
                ..
            } => true,
            _ => false,
        }
    }

    fn inspect_all_cmds(&self, inspect: &mut impl FnMut(&Cmd<A>)) {
        inspect(self);

        if let Cmd::If {
            if_zero,
            if_nonzero,
            ..
        } = self
        {
            if_zero.inspect_all_cmds(inspect);
            if_nonzero.inspect_all_cmds(inspect);
        }
    }

    fn write_with_indent(&self, f: &mut std::fmt::Formatter<'_>, indent: Indent) -> Result<(), std::fmt::Error> {
        write!(f, "{indent}")?;
        match self {
            Cmd::Store {
                to,
                op,
            } => write!(f, "{to} := {op}"),
            Cmd::LoadDynamicReg {
                into,
                regs,
                index,
                size,
            } => write!(f, "{into} := {regs:?}[{index}] ({size:?})"),
            Cmd::StoreDynamicReg {
                value,
                regs,
                index,
                size,
            } => write!(f, " {regs:?}[{index}] ({size:?}) := {value}"),
            Cmd::Handler {
                id,
                args,
            } => write!(f, "{id:?}({})", args.iter().format(", ")),
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => {
                writeln!(f, "IF {val} THEN")?;
                let inner = indent.next();
                if_nonzero.write_with_indent(f, inner)?;

                if !if_zero.is_empty() {
                    writeln!(f, "{indent}ELSE")?;
                    if_zero.write_with_indent(f, inner)?;
                }

                write!(f, "{indent}FI")?;

                Ok(())
            },
            Cmd::Exception {
                exception,
                code,
            } => write!(f, "exception({exception:X?}, {code})"),
            Cmd::ReadDescriptor {
                force,
                selector,
                ok,
                base,
                limit,
                access_rights,
                mark_accessed,
            } => write!(
                f,
                "descriptor({selector}, {force}) into ok={ok}, base={base}, limit={limit}, access_rights={access_rights}, mark_accessed={mark_accessed}"
            ),
            Cmd::Out {
                len,
                port,
                data,
            } => write!(f, "OUT(len={len}, port={port}, data={data})"),
            Cmd::In {
                len,
                port,
                data,
            } => write!(f, "{data} := IN(len={len}, port={port})"),
            Cmd::Log {
                message,
            } => write!(f, "log({message:?})"),
            Cmd::ReadMemory {
                index,
            } => write!(f, "read mem{index}"),
            Cmd::WriteMemory {
                index,
            } => write!(f, "write mem{index}"),
        }
    }
}

#[derive(Copy, Clone, Default)]
struct Indent(usize);

impl Indent {
    fn next(&self) -> Indent {
        Indent(self.0 + 2)
    }
}

impl Display for Indent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&"                                "[..self.0])
    }
}

impl Cmd<Intel386> {
    fn execute(&self, s: &mut EfficientSystemState<Intel386>, ctx: &mut Ctx<Intel386>) -> Result<ExecResult, Exception> {
        match self {
            Cmd::Store {
                to,
                op,
            } => {
                let result = op.execute(&ctx.tmp, s);
                to.write(s, ctx, result);

                Ok(ExecResult::Ok)
            },
            Cmd::StoreDynamicReg {
                regs,
                index,
                value,
                size,
            } => {
                let loc = Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(regs[index.eval(&ctx.tmp, s) as usize].unwrap()),
                    size: *size,
                });

                let result = value.eval(&ctx.tmp, s);
                loc.write(s, ctx, result);

                Ok(ExecResult::Ok)
            },
            Cmd::LoadDynamicReg {
                regs,
                index,
                into,
                size,
            } => {
                let loc = Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(regs[index.eval(&ctx.tmp, s) as usize].unwrap()),
                    size: *size,
                });

                let result = loc.eval(&ctx.tmp, s);
                into.write(s, ctx, result);

                Ok(ExecResult::Ok)
            },
            Cmd::Handler {
                id,
                args,
            } => {
                assert!(args.len() <= 2);

                let mut arg_values = [0; 2];
                for (result, arg) in arg_values.iter_mut().zip(args.iter()) {
                    *result = arg.eval(&ctx.tmp, s) as u32;
                }

                Ok(ExecResult::InvokeHandler {
                    id: *id,
                    args: arg_values,
                })
            },
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => {
                let val = val.eval(&ctx.tmp, s);
                if val != 0 {
                    if_nonzero.execute(s, ctx)
                } else {
                    if_zero.execute(s, ctx)
                }
            },
            Cmd::Exception {
                exception,
                code,
            } => Err(exception.with_code_from_u32(code.eval(&ctx.tmp, s) as u32)),
            Cmd::ReadDescriptor {
                force,
                selector,
                ok,
                base,
                limit,
                access_rights,
                mark_accessed,
            } => {
                let selector_val = selector.eval(&ctx.tmp, s);

                let result = ctx
                    .execution_context
                    .read_descriptor(s.cpu, *force, *mark_accessed, selector_val as u16)?;
                ok.write(s, ctx, result.ok as u128);
                base.write(s, ctx, result.base as u128);
                limit.write(s, ctx, result.limit as u128);
                access_rights.write(s, ctx, result.access_rights as u128);

                Ok(ExecResult::Ok)
            },
            Cmd::Out {
                len,
                port,
                data,
            } => {
                let port = port.eval(&ctx.tmp, s);
                let data = data.eval(&ctx.tmp, s) as u32;
                ctx.execution_context.port_out(s.cpu, port as u16, *len as u8, data)?;

                Ok(ExecResult::Ok)
            },
            Cmd::In {
                len,
                port,
                data,
            } => {
                let port = port.eval(&ctx.tmp, s);
                let val = ctx.execution_context.port_in(s.cpu, port as u16, *len as u8)?;
                data.write(s, ctx, val as u128);

                Ok(ExecResult::Ok)
            },
            Cmd::Log {
                message,
            } => {
                info!("{message}");
                Ok(ExecResult::Ok)
            },
            Cmd::ReadMemory {
                index,
            } => {
                s.mem[*index] = ctx.mem_areas[*index].read_from_mem_as_u128(
                    ctx.execution_context.memory,
                    s.cpu.is_userspace(),
                    &mut ctx.execution_context.mmio_ctx,
                )?;
                Ok(ExecResult::Ok)
            },
            Cmd::WriteMemory {
                index,
            } => {
                ctx.mem_areas[*index].write_u128_to_mem(
                    ctx.execution_context.memory,
                    s.cpu.is_userspace(),
                    &mut ctx.execution_context.mmio_ctx,
                    s.mem[*index],
                )?;
                Ok(ExecResult::Ok)
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, mem_dbg::MemSize)]
pub enum Commands<A: Arch> {
    Ops(Vec<Cmd<A>>),
}

impl Commands<Intel386> {
    fn execute(&self, s: &mut EfficientSystemState<Intel386>, ctx: &mut Ctx<Intel386>) -> Result<ExecResult, Exception> {
        match self {
            Commands::Ops(ops) => {
                for op in ops.iter() {
                    let r = op.execute(s, ctx)?;
                    if !matches!(r, ExecResult::Ok) {
                        return Ok(r)
                    }
                }

                Ok(ExecResult::Ok)
            },
        }
    }

    pub fn invokes_handler(&self) -> bool {
        let mut found = false;
        self.inspect_all_cmds(&mut |cmd| found |= matches!(cmd, Cmd::Handler { .. }));
        found
    }
}

impl<A: Arch> Commands<A> {
    pub fn wrap_memory_accesses(&mut self, accesses: impl IntoIterator<Item = usize> + Clone) {
        let targets_to_read = accesses
            .clone()
            .into_iter()
            .filter(|&index| self.reads_from_mem(index))
            .collect::<Vec<_>>();
        match self {
            Commands::Ops(cmds) => {
                for (pos, index) in targets_to_read.into_iter().enumerate() {
                    cmds.insert(
                        pos,
                        Cmd::ReadMemory {
                            index,
                        },
                    );
                }
            },
        }

        let targets_to_write = accesses
            .into_iter()
            .filter(|&index| self.writes_to_mem(index))
            .collect::<Vec<_>>();

        self.insert_memory_writes(&targets_to_write, true);
    }

    fn map(&self, mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>) -> Commands<A> {
        match self {
            Commands::Ops(ops) => Commands::Ops(ops.iter().cloned().map(|op| op.map_locs(&mut map_flows)).collect::<Vec<_>>()),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Commands::Ops(ops) => ops.is_empty(),
        }
    }

    fn inspect_all_cmds(&self, inspect: &mut impl FnMut(&Cmd<A>)) {
        match self {
            Commands::Ops(ops) => {
                for op in ops {
                    op.inspect_all_cmds(inspect)
                }
            },
        }
    }

    fn contains_fallible(&self) -> bool {
        let mut found = false;
        self.inspect_all_cmds(&mut |cmd| {
            found |= matches!(
                cmd,
                Cmd::Handler { .. }
                    | Cmd::Exception { .. }
                    | Cmd::ReadDescriptor { .. }
                    | Cmd::Out { .. }
                    | Cmd::In { .. }
                    | Cmd::ReadMemory { .. }
                    | Cmd::WriteMemory { .. }
            )
        });

        found
    }

    pub fn all_paths_terminate(&self) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.always_terminates()),
        }
    }

    pub fn reads_descriptors(&self) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.reads_descriptors()),
        }
    }

    pub fn performs_port_io(&self) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.performs_port_io()),
        }
    }

    pub fn reads_from_mem(&self, index: usize) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.reads_from_mem(index)),
        }
    }

    pub fn writes_to_mem(&self, index: usize) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.writes_to_mem(index)),
        }
    }

    pub fn insert_memory_writes(&mut self, accesses: &[usize], append_to_end_if_no_handler: bool) {
        match self {
            Commands::Ops(ops) => {
                let pos = ops
                    .iter()
                    .position(|cmd| matches!(cmd, Cmd::Handler { .. }))
                    .or(if append_to_end_if_no_handler { Some(ops.len()) } else { None });

                if let Some(pos) = pos {
                    for (n, &index) in accesses.iter().enumerate() {
                        ops.insert(
                            pos + n,
                            Cmd::WriteMemory {
                                index,
                            },
                        )
                    }
                }

                for cmd in ops.iter_mut() {
                    cmd.insert_memory_writes(accesses);
                }
            },
        }
    }

    pub fn accesses_mem(&self, index: usize) -> bool {
        match self {
            Commands::Ops(ops) => ops.iter().any(|op| op.accesses_mem(index)),
        }
    }

    pub fn collect_write_targets(&self, written: &mut HashSet<Val<A>>) {
        match self {
            Commands::Ops(ops) => {
                for op in ops.iter() {
                    op.collect_write_targets(written);
                }
            },
        }
    }

    fn write_with_indent(&self, f: &mut std::fmt::Formatter<'_>, indent: Indent) -> std::fmt::Result {
        match self {
            Commands::Ops(ops) => {
                for cmd in ops.iter() {
                    cmd.write_with_indent(f, indent)?;
                    writeln!(f)?;
                }
            },
        }

        Ok(())
    }
}

impl<A: Arch> Display for Commands<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_with_indent(f, Indent::default())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum ExecResult {
    Ok,
    InvokeHandler { id: HandlerId, args: [u32; 2] },
}

/// Result type that is accessible by JITed code.
/// `discr` is guaranteed to convert to an exception with the same vector if less than or equal to 0x80.
/// If greater, it converts to the handler with that ID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PackedExecResult {
    pub(crate) discr: u8,
    pub(crate) parameters: [u32; 2],
}

impl Default for PackedExecResult {
    fn default() -> Self {
        Self {
            discr: 0xff,
            parameters: [0xdeadbeef, 0xdeadbeef],
        }
    }
}

impl From<ExecResult> for PackedExecResult {
    fn from(value: ExecResult) -> Self {
        match value {
            ExecResult::Ok => unreachable!(),
            ExecResult::InvokeHandler {
                id,
                args,
            } => Self::handler(id, &args),
        }
    }
}

impl From<Exception> for PackedExecResult {
    fn from(value: Exception) -> Self {
        Self::exception(value.as_u8(), value.code_as_u32().unwrap_or(0), value.address().unwrap_or(0))
    }
}

impl From<Result<ExecResult, Exception>> for PackedExecResult {
    fn from(value: Result<ExecResult, Exception>) -> Self {
        match value {
            Ok(x) => x.into(),
            Err(x) => x.into(),
        }
    }
}

impl PackedExecResult {
    pub fn handler(id: HandlerId, args: &[u32; 2]) -> Self {
        Self {
            discr: (id as u8).checked_add(0x80).unwrap(),
            parameters: *args,
        }
    }

    pub fn exception(vector: u8, code: u32, address: u32) -> Self {
        assert!(vector <= 0x80);
        Self {
            discr: vector,
            parameters: [code, address],
        }
    }

    pub fn unpack(&self) -> Result<ExecResult, Exception> {
        if let Some(id) = self.discr.checked_sub(0x80) {
            Ok(ExecResult::InvokeHandler {
                id: HandlerId::from_u8(id).unwrap(),
                args: self.parameters,
            })
        } else {
            Err(Exception::from_vector_and_params(
                self.discr,
                self.parameters[0],
                self.parameters[1],
            ))
        }
    }

    pub fn is_exception(&self) -> bool {
        self.unpack().is_err()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MiniSem<A: Arch> {
    #[serde(default)]
    pub name: String,
    pub addresses: MemoryAccesses<A>,
    pub commands: Commands<A>,
    pub part_packing: PackingStructure,
    pub jump: Jump<A>,
    pub is_rep: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MiniSemRef<'a, A: Arch> {
    pub name: &'a str,
    pub addresses: &'a MemoryAccesses<A>,
    pub commands: &'a Commands<A>,
    pub part_packing: &'a PackingStructure,
    pub jump: &'a Jump<A>,
    pub is_rep: bool,
}

impl<A: Arch> MiniSemRef<'_, A> {
    pub fn to_owned(&self) -> MiniSem<A> {
        MiniSem {
            name: self.name.to_string(),
            addresses: self.addresses.clone(),
            commands: self.commands.clone(),
            part_packing: self.part_packing.clone(),
            jump: self.jump.clone(),
            is_rep: self.is_rep,
        }
    }

    pub fn performs_memory_accesses(&self) -> bool {
        !self.addresses.is_empty()
    }

    pub fn performs_port_io(&self) -> bool {
        self.commands.performs_port_io()
    }

    pub fn can_fail(&self) -> bool {
        self.commands.contains_fallible()
    }
}

pub trait MakeEncoding {
    type Made;

    fn make_encoding(&self) -> Self::Made;
}

pub trait BorrowEncoding {
    type Borrowed<'a>
    where
        Self: 'a;

    fn borrow_encoding(&self) -> Self::Borrowed<'_>;
}

impl<A: Arch> BorrowEncoding for Encoding<A, MiniSem<A>, IgnoredMetadata> {
    type Borrowed<'a> = EncodingRef<'a, A, MiniSemRef<'a, A>, IgnoredMetadata>;

    fn borrow_encoding(&self) -> Self::Borrowed<'_> {
        EncodingRef {
            bits: &self.bits,
            equivalent_prefixes: &self.equivalent_prefixes,
            parts: &self.parts,
            semantics: self.semantics.borrow(),
            metadata: &self.metadata,
        }
    }
}

impl<'a, A: Arch> MakeEncoding for EncodingRef<'a, A, MiniSemRef<'a, A>, IgnoredMetadata> {
    type Made = Encoding<A, MiniSem<A>, IgnoredMetadata>;

    fn make_encoding(&self) -> Self::Made {
        Encoding {
            bits: self.bits.to_vec(),
            equivalent_prefixes: self.equivalent_prefixes.clone(),
            parts: self.parts.to_vec(),
            semantics: self.semantics.to_owned(),
            metadata: self.metadata.clone(),
        }
    }
}

impl<A: Arch> BorrowEncoding for EncodingRef<'_, A, MiniSem<A>, IgnoredMetadata> {
    type Borrowed<'a>
        = EncodingRef<'a, A, MiniSemRef<'a, A>, IgnoredMetadata>
    where
        Self: 'a;

    fn borrow_encoding(&self) -> Self::Borrowed<'_> {
        EncodingRef {
            bits: self.bits,
            equivalent_prefixes: self.equivalent_prefixes,
            parts: self.parts,
            semantics: self.semantics.borrow(),
            metadata: self.metadata,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum MemArea {
    Real { segment_offset: u32, addr: u16, len: u8 },
    Protected { addr: u32, len: u8 },
}

impl MemArea {
    pub fn read_from_mem(&self, mem: &Mem32, userspace: bool, mmio: &mut impl Mmio) -> Result<Vec<u8>, Exception> {
        let mut data = Vec::with_capacity(match self {
            MemArea::Protected {
                len, ..
            }
            | MemArea::Real {
                len, ..
            } => *len as usize,
        });

        for addr in self.iter_addrs() {
            data.push(mem.read::<u8>(addr, userspace, mmio)?)
        }

        Ok(data)
    }

    #[inline]
    pub fn read_from_mem_as_u64(&self, mem: &Mem32, userspace: bool, mmio: &mut impl Mmio) -> Result<u64, Exception> {
        if self.wraps() {
            let mut result = 0;
            for (index, addr) in self.iter_addrs().enumerate() {
                result |= (mem.read::<u8>(addr, userspace, mmio)? as u64) << (index * 8);
            }

            Ok(result)
        } else {
            Ok(match self.len() {
                1 => mem.read::<u8>(self.start_addr().as_u64() as u32, userspace, mmio)? as u64,
                2 => mem.read::<u16>(self.start_addr().as_u64() as u32, userspace, mmio)? as u64,
                4 => mem.read::<u32>(self.start_addr().as_u64() as u32, userspace, mmio)? as u64,
                6 => {
                    mem.read::<u32>(self.start_addr().as_u64() as u32, userspace, mmio)? as u64
                        | ((mem.read::<u16>(self.start_addr().as_u64() as u32 + 4, userspace, mmio)? as u64) << 32)
                },
                8 => mem.read::<u64>(self.start_addr().as_u64() as u32, userspace, mmio)?,
                n => panic!("Cannot read {n} bytes into u64"),
            })
        }
    }

    #[inline]
    pub fn read_from_mem_as_u128(&self, mem: &Mem32, userspace: bool, mmio: &mut impl Mmio) -> Result<u128, Exception> {
        if self.wraps() {
            let mut result = 0;
            for (index, addr) in self.iter_addrs().enumerate() {
                result |= (mem.read::<u8>(addr, userspace, mmio)? as u128) << (index * 8);
            }

            Ok(result)
        } else {
            Ok(match self.len() {
                1 => mem.read::<u8>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128,
                2 => mem.read::<u16>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128,
                4 => mem.read::<u32>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128,
                6 => {
                    mem.read::<u32>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128
                        | ((mem.read::<u16>(self.start_addr().as_u64() as u32 + 4, userspace, mmio)? as u128) << 32)
                },
                8 => mem.read::<u64>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128,
                10 => {
                    mem.read::<u64>(self.start_addr().as_u64() as u32, userspace, mmio)? as u128
                        | ((mem.read::<u16>(self.start_addr().as_u64() as u32 + 8, userspace, mmio)? as u128) << 64)
                },
                n => panic!("Cannot read {n} bytes into u128"),
            })
        }
    }

    pub fn start_addr(&self) -> Addr {
        match *self {
            MemArea::Real {
                segment_offset,
                addr,
                ..
            } => Addr::new((addr as u64).wrapping_add(segment_offset as u64)),
            MemArea::Protected {
                addr, ..
            } => Addr::new(addr as u64),
        }
    }

    pub fn iter_addrs(&self) -> impl Iterator<Item = u32> {
        match *self {
            MemArea::Real {
                segment_offset,
                addr,
                len,
            } => EitherIter::Left(
                (0..len).map(move |n| (addr.wrapping_add(n as u16) as u64).wrapping_add(segment_offset as u64) as u32),
            ),
            MemArea::Protected {
                addr,
                len,
            } => EitherIter::Right((0..len).map(move |n| addr.wrapping_add(n as u32))),
        }
    }

    pub fn write_u64_to_mem(&self, mem: &Mem32, userspace: bool, mmio: &mut HwMmio, val: u64) -> Result<(), Exception> {
        if self.wraps() {
            for (index, addr) in self.iter_addrs().enumerate() {
                mem.write::<u8>(addr, userspace, (val >> (index * 8)) as u8, mmio)?
            }
        } else {
            match self.len() {
                1 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u8, mmio)?,
                2 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u16, mmio)?,
                4 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u32, mmio)?,
                6 => {
                    mem.write(self.start_addr().as_u64() as u32, userspace, val as u32, mmio)?;
                    mem.write(self.start_addr().as_u64() as u32 + 4, userspace, (val >> 32) as u16, mmio)?;
                },
                8 => mem.write(self.start_addr().as_u64() as u32, userspace, val, mmio)?,
                n => panic!("Cannot read {n} bytes into u64"),
            }
        }

        Ok(())
    }

    pub fn write_u128_to_mem(&self, mem: &Mem32, userspace: bool, mmio: &mut impl Mmio, val: u128) -> Result<(), Exception> {
        if self.wraps() {
            for (index, addr) in self.iter_addrs().enumerate() {
                mem.write::<u8>(addr, userspace, (val >> (index * 8)) as u8, mmio)?
            }
        } else {
            match self.len() {
                1 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u8, mmio)?,
                2 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u16, mmio)?,
                4 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u32, mmio)?,
                6 => {
                    mem.write(self.start_addr().as_u64() as u32, userspace, val as u32, mmio)?;
                    mem.write(self.start_addr().as_u64() as u32 + 4, userspace, (val >> 32) as u16, mmio)?;
                },
                8 => mem.write(self.start_addr().as_u64() as u32, userspace, val as u64, mmio)?,
                10 => {
                    mem.write(self.start_addr().as_u64() as u32, userspace, val as u64, mmio)?;
                    mem.write(self.start_addr().as_u64() as u32 + 8, userspace, (val >> 64) as u16, mmio)?;
                },
                n => panic!("Cannot read {n} bytes into u64"),
            }
        }

        Ok(())
    }

    pub fn len(&self) -> usize {
        match *self {
            MemArea::Real {
                len, ..
            }
            | MemArea::Protected {
                len, ..
            } => len as usize,
        }
    }

    #[inline]
    pub fn wraps(&self) -> bool {
        match *self {
            MemArea::Real {
                addr,
                len,
                ..
            } => addr.checked_add((len - 1) as u16).is_none(),
            MemArea::Protected {
                ..
            } => false,
        }
    }
}

impl<A: Arch> MiniSem<A> {
    pub fn borrow(&self) -> MiniSemRef<'_, A> {
        MiniSemRef {
            name: &self.name,
            addresses: &self.addresses,
            commands: &self.commands,
            part_packing: &self.part_packing,
            jump: &self.jump,
            is_rep: self.is_rep,
        }
    }

    pub fn reads_descriptors(&self) -> bool {
        self.commands.reads_descriptors()
    }

    pub fn performs_port_io(&self) -> bool {
        self.commands.performs_port_io()
    }
}

impl MiniSemRef<'_, Intel386> {
    pub fn execute<'a>(
        &self, s: &mut EfficientSystemState<Intel386>, execution_context: &mut ExecutionContext<'_, '_, Intel386>,
        mem_areas: &[MemArea], print: bool,
    ) -> Result<ExecResult, Exception> {
        self.commands.execute(
            s,
            &mut Ctx {
                tmp: [0; MAX_TEMP_VARS],
                print,
                execution_context,
                mem_areas,
            },
        )
    }

    pub fn reads_from_mem(&self, index: usize) -> bool {
        self.commands.reads_from_mem(index)
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl<A: Arch> MiniSemRef<'_, A> {
    pub fn extract_memory_areas<'a>(
        &'a self, protected_mode: bool, state: &'a EfficientSystemState<A>,
    ) -> impl Iterator<Item = MemArea> + 'a {
        self.addresses.iter().map(move |access| {
            let c = access.calculation.unwrap_calculation();
            if protected_mode {
                MemArea::Protected {
                    addr: c.evaluate_from_iter::<A>(access.inputs.iter().map(|&input| state.get_dest(input, true) as u64)) as u32,
                    len: access.size.end as u8,
                }
            } else {
                let mut sum = 0u16;
                let mut segment_offset = 0u64;
                for (input, shift) in access.inputs.iter().zip(c.terms.iter()) {
                    let v = state.get_dest(*input, true) as u64;
                    if shift.primary.size == AddrTermSize::U32 {
                        segment_offset = segment_offset.wrapping_add(shift.apply(v));
                    } else {
                        sum = sum.wrapping_add(shift.apply(v) as u16);
                    }
                }

                let addr = sum.wrapping_add(c.offset as u16);

                MemArea::Real {
                    addr,
                    segment_offset: segment_offset as u32,
                    len: access.size.end as u8,
                }
            }
        })
    }
}

impl<A: Arch> Semantics<A> for MiniSem<A> {
    fn is_part_used_in_computation(&self, _part_index: usize) -> bool {
        todo!()
    }

    fn foreach_loc(&mut self, _f: impl FnMut(&mut ParLoc<A>)) {
        todo!()
    }

    fn map(
        &self, _instr: liblisa::Instruction, _part_values: &[Option<u64>],
        mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>,
        map_address_computations: impl FnMut(
            usize,
            &liblisa::encoding::dataflows::ParameterizedComputation,
        ) -> Option<liblisa::encoding::dataflows::ParameterizedComputation>,
    ) -> Self {
        let addresses = self.addresses.map(
            |loc: FlowValueLocation, val| map_flows(loc.is_address(), val),
            map_address_computations,
        );
        Self {
            name: self.name.clone(),
            addresses,
            commands: self.commands.map(&mut map_flows),
            part_packing: self.part_packing.clone(),
            jump: self.jump.map(&mut map_flows),
            is_rep: self.is_rep,
        }
    }
}

impl<A: Arch> Display for MiniSemRef<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "[[ {} ]]", self.name)?;
        for (index, access) in self.addresses.iter().enumerate() {
            write!(
                f,
                "{:10} = ",
                format!(
                    "Addr{}(m{}; {} bytes)",
                    match access.kind {
                        AccessKind::Executable => "X ",
                        AccessKind::Input => "R ",
                        AccessKind::InputOutput => "RW",
                    },
                    index,
                    access.size.end,
                )
            )?;

            let names = access.inputs.iter().map(|input| format!("{}", input)).collect::<Vec<_>>();

            match &access.calculation {
                ParameterizedComputation::FromPart(index) => {
                    writeln!(f, "Part[{index}] with inputs {}", names.iter().format(", "))?
                },
                ParameterizedComputation::Calculation(c) => writeln!(f, "{}", c.display(&names))?,
            }
        }

        writeln!(f, "{}", self.commands)?;
        writeln!(f, "JUMP {}", self.jump)?;
        writeln!(f, "Is REP: {}", self.is_rep)?;

        Ok(())
    }
}

impl<A: Arch> Display for MiniSem<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.borrow().fmt(f)
    }
}
